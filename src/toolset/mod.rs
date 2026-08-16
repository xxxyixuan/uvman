//! 工具安装编排层。
//!
//! 职责：把「插件 TOML + 版本请求」解析为一份可执行的 `InstallPlan`，
//! 再按 下载 → 校验 → 解压 → 部署 的顺序消费该计划。纯数据放在
//! `InstallPlan` 中，便于逐项测试与复用底层 core 基建。

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use indicatif::ProgressBar;
use serde::{Deserialize, Serialize};

use crate::core::config::GLOBAL_CONFIG;
use crate::core::error::UError;
use crate::core::http::HTTP_CLIENT;
use crate::core::plugin::{InstallBin, Release, ToolPlugin};
use crate::core::suggest::did_you_mean;
use crate::core::SingleOrArray;
use crate::ui::style::{ered, ogreen};
use crate::core::{paths, platform};

/// 缓存 TTL 默认值：24 小时
const DEFAULT_CACHE_TTL_HOURS: u64 = 24;

/// 工具档案缓存记录文件名（位于 cache/tools/<tool>/ 下）
const RECORDS_FILE: &str = "records.json";

/// 一次安装所需的全部原子信息（纯数据，供 execute 逐项消费）。
///
/// 构造阶段（`plan`）不做任何网络 IO：校验和的获取被推迟到执行期，
/// 保证缓存命中 / 已安装检测等场景可以完全离线判定。
pub struct InstallPlan {
    pub name: String,
    /// 已解析为具体版本号（x.y.z），非代号
    pub version: String,
    /// 候选下载 URL（占位符已替换）：全局镜像 → 插件镜像 → 插件 default，
    /// 逐个尝试直到成功
    pub urls: Vec<String>,
    /// 平台对应的压缩包扩展名（如 zip / tar.gz）
    pub ext: String,
    /// 解压时剥离的顶层目录层数
    pub strip: u32,
    /// 可执行文件在解压根目录中的相对目录
    pub bin_dir: String,
    /// 校验和获取计划（URL 已渲染；执行期下载完成后才联网拉取）
    pub hash: Option<HashPlan>,
    /// 下载档案的落盘路径（cache 下）
    pub archive_path: PathBuf,
    /// 最终安装目录 (tools/<name>/<version>)
    pub install_dir: PathBuf,
}

/// 校验和获取计划：候选 URL + 算法 + 解析规则（纯数据）
pub struct HashPlan {
    /// 校验和文件的候选 URL（与档案候选源同序）
    pub urls: Vec<String>,
    /// 哈希算法（插件缺省 sha256）
    pub algorithm: String,
    /// 已渲染的提取 pattern
    pub pattern: Option<String>,
    /// 档案官方文件名（无 pattern 时按行匹配校验和文件）
    pub filename: String,
}

/// 依据插件及版本请求生成安装计划（不执行任何写操作）
pub async fn plan(
    name: &str, version: Option<&str>,
) -> Result<InstallPlan, UError> {
    let plugin =
        ToolPlugin::load_from(&paths::plugin_path(name)).map_err(|_| {
            UError::PluginNotInstalled {
                name: name.to_string(),
                similar: vec![],
            }
        })?;

    let sys_os = platform::OS.as_str();
    let bin = select_bin(&plugin, sys_os)?;

    let version = resolve_version(&plugin, name, version).await?;
    let (os, arch) = plugin.resolve_platform()?;
    let ext = bin
        .download
        .ext
        .get(sys_os)
        .cloned()
        .unwrap_or_else(|| "zip".to_string());

    let mut vars: HashMap<&str, &str> = HashMap::new();
    vars.insert("version", version.as_str());
    vars.insert("os", os.as_str());
    vars.insert("arch", arch.as_str());
    vars.insert("ext", ext.as_str());

    // 候选源：全局 [registry] 镜像（优先）→ 插件镜像 → 插件 default；
    // 对每个源分别渲染下载 URL
    let sources = merged_sources(name, &plugin);
    let urls: Vec<String> = sources
        .iter()
        .map(|source| {
            vars.insert("registry", source.as_str());
            plugin.render(&bin.download.path, &vars)
        })
        .collect();

    // {filename} 取自下载 URL 的文件名（与官方发布物一致，如
    // node-v24.19.0-win-x64.zip），而非本地缓存名——校验和文件中的
    // 行按官方文件名记录
    let url_filename = urls
        .first()
        .map(|u| url_basename(u))
        .unwrap_or_default();
    vars.insert("filename", url_filename.as_str());

    // 校验和只构造获取计划（渲染 URL），联网拉取推迟到执行期
    let hash = hash_plan(&plugin, bin, &mut vars)?;

    let archive_path = paths::cache_tools_dir()
        .join(name)
        .join(format!("{name}-{version}-{os}-{arch}.{ext}"));
    let install_dir = paths::tools_dir().join(name).join(&version);

    Ok(InstallPlan {
        name: name.to_string(),
        version,
        urls,
        ext,
        strip: bin.extract.strip,
        bin_dir: bin.deploy.bin_dir.resolve(sys_os)?.clone(),
        hash,
        archive_path,
        install_dir,
    })
}

/// 合成候选下载源：全局 `[registry]`（按工具名，优先级最高）→
/// 插件 mirrors → 插件 default；重复源只保留先出现者
fn merged_sources(name: &str, plugin: &ToolPlugin) -> Vec<String> {
    let mut sources: Vec<String> = Vec::new();
    if let Some(global) = GLOBAL_CONFIG.registry.get(name) {
        match global {
            SingleOrArray::Single(s) => sources.push(s.clone()),
            SingleOrArray::Array(list) => {
                sources.extend(list.iter().cloned())
            },
        }
    }
    for s in plugin.registry.sources() {
        if !sources.contains(&s) {
            sources.push(s);
        }
    }
    sources
}

/// 依次执行下载（或复用缓存）、校验、解压、部署。
///
/// 校验策略：下载完成后联网拉取官方校验和校验，并将算法与期望值
/// 写入 records.json；此后缓存命中走本地校验（离线可用，且能在
/// 档案被篡改/损坏时自愈重下）。
pub async fn execute(plan: &InstallPlan) -> Result<(), UError> {
    // 惰性 GC：迁移旧布局、清理过期缓存（best-effort，失败不阻断安装）
    let ttl = cache_ttl();
    gc_cache_at(&paths::cache_dir(), ttl);

    let from_cache = archive_cache_hit(plan);
    if from_cache {
        // 本地校验失败（篡改/损坏）：删除缓存档案，转下载路径自愈
        if verify_recorded_hash(plan).is_err() {
            let _ = fs::remove_file(&plan.archive_path);
            download_and_verify(plan, ttl).await?;
        } else {
            crate::ui::report::print_hint(
                &format!(
                    "using cached archive `{}`",
                    plan.archive_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("archive")
                ),
                &[],
            );
        }
    } else {
        download_and_verify(plan, ttl).await?;
    }

    install_stages(plan)?;

    // ttl = 0：不保留缓存，安装成功后立即删除压缩包
    if ttl == 0 {
        let _ = fs::remove_file(&plan.archive_path);
    }
    Ok(())
}

/// 下载档案并完成远端校验（hash.enabled 时），通过后写入缓存记录。
/// 期望校验和一并记录，供后续缓存命中做本地校验
async fn download_and_verify(
    plan: &InstallPlan, ttl: u64,
) -> Result<(), UError> {
    download_archive(plan).await?;

    if let Some(hp) = &plan.hash {
        let expected = fetch_expected_hash(hp).await?;
        let actual = compute_checksum(&plan.archive_path, &hp.algorithm)?;
        if actual != expected {
            return Err(UError::ChecksumError {
                message: format!(
                    "expected {expected}, got {actual} for {}",
                    plan.archive_path.display()
                ),
            });
        }
        if ttl > 0 {
            record_archive(
                &plan.archive_path,
                ttl,
                Some((&hp.algorithm, &expected)),
            );
        }
    } else if ttl > 0 {
        record_archive(&plan.archive_path, ttl, None);
    }
    Ok(())
}

/// 按候选顺序下载档案（镜像按序 + default 兜底）；
/// 单个 URL 仍走配置的网络重试，全部候选失败才报错
async fn download_archive(plan: &InstallPlan) -> Result<(), UError> {
    let retries = GLOBAL_CONFIG.network.retries.unwrap_or(0);
    let retry_delay = GLOBAL_CONFIG.network.retry_delay.unwrap_or(0);

    let mut last_err = None;
    for url in &plan.urls {
        match HTTP_CLIENT
            .download_to(url, &plan.archive_path, retries, retry_delay)
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => {
                crate::ui::report::print_warning(&format!(
                    "download source failed, trying next: {e}"
                ));
                last_err = Some(e);
            },
        }
    }
    Err(last_err.unwrap_or_else(|| UError::SimpleError(
        "no download source available for this tool".into(),
    )))
}

/// 缓存档案是否可复用：文件存在、非空、且未超过记录的 TTL
fn archive_cache_hit(plan: &InstallPlan) -> bool {
    if !plan.archive_path.is_file()
        || fs::metadata(&plan.archive_path)
            .map(|m| m.len() == 0)
            .unwrap_or(true)
    {
        return false;
    }
    let Some(tool_dir) = plan.archive_path.parent() else {
        return false;
    };
    let Some(name) =
        plan.archive_path.file_name().and_then(OsStr::to_str)
    else {
        return false;
    };
    match load_records(tool_dir).archives.get(name) {
        // 无记录：视为可复用（孤儿档案由 GC 按 mtime 兜底）
        None => true,
        Some(record) => {
            record.downloaded_at + record.ttl_hours * 3600 > unix_now()
        },
    }
}

/// 安装三阶段（校验 → 解压 → 部署）；逐阶段 spinner 推进。
/// 校验阶段为本地校验（records.json 记录值），不产生网络请求
fn install_stages(plan: &InstallPlan) -> Result<(), UError> {
    let archive_name = plan
        .archive_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("archive");

    run_stage(format!("verifying {archive_name}"), || {
        verify_recorded_hash(plan)
    })?;

    let extract_dir = run_stage("extracting archive".to_string(), || {
        // 解压到一次性临时目录：TempDir 在 drop 时自动删除，
        // 成功与失败路径都会清理，且并发安装互不干扰
        let extract_dir = tempfile::tempdir().map_err(|source| {
            UError::FileError { path: std::env::temp_dir(), source }
        })?;
        extract_archive(
            &plan.archive_path,
            extract_dir.path(),
            &plan.ext,
            plan.strip,
        )?;
        Ok(extract_dir)
    })?;

    run_stage(format!("deploying {}@{}", plan.name, plan.version), || {
        fs::create_dir_all(&plan.install_dir).map_err(|source| {
            UError::FileError { path: plan.install_dir.clone(), source }
        })?;
        copy_bin(extract_dir.path(), &plan.bin_dir, &plan.install_dir)
    })?;

    Ok(())
}

/// 执行单个安装阶段：spinner 展示进行中消息，成功后该行固化为
/// 绿色 `✔ <msg>`，失败固化为红色 `✖ <msg>` 并向上返回错误
fn run_stage<T>(
    msg: String, f: impl FnOnce() -> Result<T, UError>,
) -> Result<T, UError> {
    let pb = stage_spinner();
    if let Some(pb) = &pb {
        pb.set_message(msg.clone());
    }
    match f() {
        Ok(v) => {
            if let Some(pb) = &pb {
                finish_stage(pb, true, &msg);
            }
            Ok(v)
        },
        Err(e) => {
            if let Some(pb) = &pb {
                finish_stage(pb, false, &msg);
            }
            Err(e)
        },
    }
}

/// 阶段完成时重绘该行：去掉 spinner、以结果符号着色固化
fn finish_stage(pb: &ProgressBar, ok: bool, msg: &str) {
    pb.disable_steady_tick();
    pb.set_style(
        indicatif::ProgressStyle::with_template("{msg}")
            .expect("valid progress template"),
    );
    let mark = if ok { "✔" } else { "✖" };
    let line = format!("{mark} {msg}");
    let styled = if ok {
        ogreen(line).to_string()
    } else {
        ered(line).to_string()
    };
    pb.set_message(styled);
    pb.finish();
}

/// 单阶段 spinner；quiet 模式下不显示。
/// steady tick 让 spinner 在同步阻塞期间持续转动
fn stage_spinner() -> Option<ProgressBar> {
    if crate::ui::report::quiet() {
        return None;
    }
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        indicatif::ProgressStyle::with_template("{spinner:.green} {msg}")
            .expect("valid progress template"),
    );
    pb.enable_steady_tick(Duration::from_millis(120));
    Some(pb)
}

/// 读取缓存 TTL 配置（小时）；未配置时默认 24 小时
fn cache_ttl() -> u64 {
    GLOBAL_CONFIG.cache.ttl.unwrap_or(DEFAULT_CACHE_TTL_HOURS)
}

/// 单个下载档案的缓存记录
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArchiveRecord {
    /// 下载完成时间（unix 秒）
    downloaded_at: u64,
    /// 下载时生效的 TTL（小时）
    ttl_hours: u64,
    /// 下载时校验通过的哈希算法（如 sha256）；未启用校验时无此字段
    #[serde(default, skip_serializing_if = "Option::is_none")]
    algorithm: Option<String>,
    /// 下载时校验通过的期望哈希值；未启用校验时无此字段。
    /// 缓存命中时据此本地校验，防止档案被篡改/损坏
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
}

/// cache/tools/<tool>/records.json 的内容：档案文件名 → 缓存记录
#[derive(Debug, Default, Serialize, Deserialize)]
struct ArchiveRecords {
    #[serde(default)]
    archives: HashMap<String, ArchiveRecord>,
}

fn records_path(tool_dir: &Path) -> PathBuf {
    tool_dir.join(RECORDS_FILE)
}

fn load_records(tool_dir: &Path) -> ArchiveRecords {
    fs::read(records_path(tool_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_records(tool_dir: &Path, records: &ArchiveRecords) {
    if let Ok(json) = serde_json::to_vec_pretty(records) {
        let _ = fs::write(records_path(tool_dir), json);
    }
}

/// 记录（或刷新）一个档案的下载时间、TTL 与校验和
fn record_archive(
    archive: &Path, ttl_hours: u64, hash: Option<(&str, &str)>,
) {
    let Some(tool_dir) = archive.parent() else { return };
    let Some(name) = archive.file_name().and_then(OsStr::to_str) else {
        return;
    };
    let (algorithm, expected) = match hash {
        Some((algorithm, expected)) => {
            (Some(algorithm.to_string()), Some(expected.to_string()))
        },
        None => (None, None),
    };
    let mut records = load_records(tool_dir);
    records.archives.insert(
        name.to_string(),
        ArchiveRecord {
            downloaded_at: unix_now(),
            ttl_hours,
            algorithm,
            hash: expected,
        },
    );
    save_records(tool_dir, &records);
}

/// 按缓存记录本地校验档案（防篡改/损坏）；
/// 无记录（孤儿档案）或记录未含校验和时跳过
fn verify_recorded_hash(plan: &InstallPlan) -> Result<(), UError> {
    let Some(tool_dir) = plan.archive_path.parent() else {
        return Ok(());
    };
    let Some(name) =
        plan.archive_path.file_name().and_then(OsStr::to_str)
    else {
        return Ok(());
    };
    let records = load_records(tool_dir);
    let Some(record) = records.archives.get(name) else {
        return Ok(());
    };
    let (Some(algorithm), Some(expected)) = (&record.algorithm, &record.hash)
    else {
        return Ok(());
    };
    let actual = compute_checksum(&plan.archive_path, algorithm)?;
    if actual != *expected {
        return Err(UError::ChecksumError {
            message: format!(
                "expected {expected}, got {actual} for {} \
                 (cached archive was modified or corrupted)",
                plan.archive_path.display()
            ),
        });
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// cache/ 下不属于「工具下载缓存」的目录（新布局分区/其他缓存），
/// 不参与旧布局迁移
const NON_TOOL_CACHE_DIRS: [&str; 3] = ["tools", "versions", "builds"];

/// 惰性清理下载缓存（best-effort）：
/// - `ttl = 0`：完全不保留缓存，旧布局目录与 `cache/tools/` 下全部清除
/// - `ttl > 0`：
///   1. 旧布局 `cache/<tool>/` 的档案迁移到 `cache/tools/<tool>/`（mtime
///      作为下载时间补记录）， `extract/` 解压残留直接清除
///   2. 按 records.json 的 `downloaded_at + ttl_hours`
///      判断过期；无记录的孤儿档案 按 mtime + 当前 TTL 兜底
/// - cache 根目录下的散落文件（如 plugins.json）不属于下载缓存，不动
fn gc_cache_at(cache: &Path, ttl: u64) {
    if ttl == 0 {
        // ttl=0 只清理「工具档案缓存」：旧布局工具目录与 cache/tools/；
        // versions/builds 等其他缓存分区不动（与迁移路径同一保护规则）
        if let Ok(entries) = fs::read_dir(cache) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir()
                    && entry.file_name() != *"tools"
                    && !is_non_tool_cache_dir(&entry)
                {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        }
        let _ = fs::remove_dir_all(cache.join("tools"));
        return;
    }

    // 旧布局迁移
    if let Ok(entries) = fs::read_dir(cache) {
        for entry in entries.flatten() {
            let legacy = entry.path();
            if !legacy.is_dir() || is_non_tool_cache_dir(&entry) {
                continue;
            }
            migrate_legacy_tool_dir(cache, &legacy, ttl);
        }
    }

    // 新布局按记录清理
    let tools_dir = cache.join("tools");
    if let Ok(entries) = fs::read_dir(&tools_dir) {
        for entry in entries.flatten() {
            let tool_dir = entry.path();
            if tool_dir.is_dir() {
                gc_tool_cache(&tool_dir, ttl);
            }
        }
    }
}

/// cache 根下的目录是否为非工具缓存目录（tools/versions/builds）
fn is_non_tool_cache_dir(entry: &fs::DirEntry) -> bool {
    NON_TOOL_CACHE_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
}

/// 将旧布局 cache/<tool>/ 的档案迁移到 cache/tools/<tool>/，完成后移除旧目录
fn migrate_legacy_tool_dir(cache: &Path, legacy: &Path, default_ttl: u64) {
    // 历史解压残留直接清除
    let extract = legacy.join("extract");
    if extract.is_dir() {
        let _ = fs::remove_dir_all(&extract);
    }

    let Some(tool_name) = legacy.file_name().and_then(OsStr::to_str) else {
        return;
    };
    let target = cache.join("tools").join(tool_name);

    let Ok(entries) = fs::read_dir(legacy) else {
        return;
    };
    for entry in entries.flatten() {
        let from = entry.path();
        if from.is_dir() {
            continue;
        }
        if fs::create_dir_all(&target).is_err() {
            return;
        }
        let to = target.join(entry.file_name());
        // 目标已存在（重复迁移）时丢弃旧文件，保留新记录
        if to.exists() || fs::rename(&from, &to).is_ok() {
            let _ = fs::remove_file(&from);
            // 补记录：以 mtime 近似下载时间，按当前 TTL 计算过期
            let downloaded_at = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut records = load_records(&target);
            let name = entry.file_name().to_string_lossy().into_owned();
            records.archives.insert(
                name,
                ArchiveRecord {
                    downloaded_at,
                    ttl_hours: default_ttl,
                    // 旧布局档案未经过校验和记录，algorithm/hash 缺省
                    algorithm: None,
                    hash: None,
                },
            );
            save_records(&target, &records);
        }
    }
    let _ = fs::remove_dir(legacy);
}

/// 按记录清理 cache/tools/<tool>/ 下过期的档案，并同步修剪记录
fn gc_tool_cache(tool_dir: &Path, default_ttl: u64) {
    let mut records = load_records(tool_dir);
    let now = SystemTime::now();

    // 记录驱动：过期或档案已不存在的条目一并移除
    let mut changed = false;
    let names: Vec<String> = records.archives.keys().cloned().collect();
    for name in names {
        let path = tool_dir.join(&name);
        if !path.exists() {
            records.archives.remove(&name);
            changed = true;
            continue;
        }
        let rec = records.archives[&name].clone();
        let expires = UNIX_EPOCH
            + Duration::from_secs(rec.downloaded_at + rec.ttl_hours * 3600);
        if now >= expires {
            let _ = fs::remove_file(&path);
            records.archives.remove(&name);
            changed = true;
        }
    }

    // 孤儿档案（无记录）：按 mtime + 当前 TTL 兜底
    if let Ok(entries) = fs::read_dir(tool_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() || entry.file_name() == *RECORDS_FILE {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if records.archives.contains_key(&name) {
                continue;
            }
            if let Ok(mtime) = entry.metadata().and_then(|m| m.modified())
                && mtime + Duration::from_secs(default_ttl * 3600) <= now
            {
                let _ = fs::remove_file(&path);
            }
        }
    }

    if changed {
        save_records(tool_dir, &records);
    }
}

/// 选择与当前系统 os 匹配的 bin 安装条目
fn select_bin<'a>(
    plugin: &'a ToolPlugin, sys_os: &str,
) -> Result<&'a InstallBin, UError> {
    let sys_os = sys_os.to_string();
    let sys_arch = platform::ARCH.as_str().to_string();
    plugin
        .install
        .bin
        .as_deref()
        .and_then(|bins| {
            bins.iter().find(|b| {
                b.os.iter().any(|o| o == &sys_os)
                    && b.arch.iter().any(|a| a == &sys_arch)
            })
        })
        .ok_or_else(|| UError::PlatformNotSupported {
            os: sys_os,
            arch: sys_arch,
        })
}

/// 远端版本条目：版本号 + 发布线元数据。
///
/// latest/lts/stable/nightly 是查询语义而非存储字段：
/// - `latest` → 全集中 semver 最大者
/// - `lts` → `lts` 元数据存在的最大者（如 node index.json 的 `"lts": "Iron"`）
/// - `stable` → semver 无 prerelease（`parse_version` 可判，无需存储）
/// - `nightly` → prerelease 段自描述（如 `22.0.0-nightly20260101`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteVersion {
    pub version: String,
    /// LTS 线代号；None 表示非 LTS（Current / 预发布）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lts: Option<String>,
}

/// 将版本请求解析为具体版本号（tool_spec 中 `@` 后的部分）：
/// - `20.11.0`：完整 semver 精准匹配
/// - `22` / `22.19`：部分版本，匹配 x.y.z / x.y.z' 最新
/// - `latest`：最新稳定版（等价于该版本本身）
/// - `lts`：LTS 元数据标记的最新版本
/// - `nightly`：版本号含 nightly 的最新版
/// - 缺省：取 `install.defaults.version` 再走上述规则（node 默认 latest）
async fn resolve_version(
    plugin: &ToolPlugin, name: &str, version: Option<&str>,
) -> Result<String, UError> {
    let request = version
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| plugin.install.defaults.version.clone());
    let request = request.trim();

    // 具体完整版本直接使用
    if semver::Version::parse(request).is_ok() {
        return Ok(request.to_string());
    }

    let versions = remote_versions_of(plugin, name).await?;

    // 部分版本（如 22 / 22.19）→ 匹配该前缀的最新版
    if is_partial_version(request) {
        return resolve_partial(&versions, request).ok_or_else(|| {
            UError::VersionNotFound {
                tool: name.to_string(),
                version: request.to_string(),
            }
        });
    }

    // 代号不回退：无法匹配时明确报错，避免静默装错版本线
    let resolved = match request {
        "latest" => latest_of(&versions),
        // 元数据优先（api 源），字符串命名约定回退（static 源如 "22.1.0-lts"）
        "lts" => {
            lts_of(&versions).or_else(|| version_matching(&versions, "lts"))
        },
        "nightly" => version_matching(&versions, "nightly"),
        _ => None,
    };
    resolved.ok_or_else(|| UError::VersionNotFound {
        tool: name.to_string(),
        version: request.to_string(),
    })
}

/// 将版本请求解析为「本地已安装」的具体版本号（`use` 的数据源）。
///
/// 与 [`resolve_version`]（远端全集）语义对齐，但匹配范围限定在
/// `tools/<name>/` 下实际存在的版本目录：
/// - `20.11.0`：精确版本，必须已安装
/// - `22` / `22.19`：部分版本，匹配已装集合中该前缀的最新
/// - `latest` / 缺省：已装最新版本
/// - `lts`：远端元数据筛选已装集合（api 源走版本缓存，static 源回退
///   字符串命名约定）
pub async fn resolve_installed_version(
    name: &str,
    request: Option<&str>,
) -> Result<String, UError> {
    let plugin =
        ToolPlugin::load_from(&paths::plugin_path(name)).map_err(|_| {
            UError::PluginNotInstalled {
                name: name.to_string(),
                similar: did_you_mean_installed(name),
            }
        })?;

    let installed = installed_versions(name);
    if installed.is_empty() {
        return Err(UError::SimpleError(format!(
            "no local version of '{name}' is installed"
        )));
    }
    let installed_rv: Vec<RemoteVersion> = installed
        .iter()
        .map(|v| RemoteVersion { version: v.clone(), lts: None })
        .collect();

    let request = request.unwrap_or("latest");
    let not_found = || UError::VersionNotFound {
        tool: name.to_string(),
        version: request.to_string(),
    };

    // 完整 semver：必须已安装，缺失即报错（避免静默切换到相近版本）
    if semver::Version::parse(request).is_ok() {
        return installed
            .iter()
            .find(|v| v.as_str() == request)
            .cloned()
            .ok_or_else(not_found);
    }

    if is_partial_version(request) {
        return resolve_partial(&installed_rv, request).ok_or_else(not_found);
    }

    let resolved = match request {
        "latest" => latest_of(&installed_rv),
        // 已装集合无本地元数据，借助远端缓存筛选后回退字符串命名
        "lts" => {
            let remote = remote_versions_of(&plugin, name).await?;
            let lts_installed: Vec<RemoteVersion> = remote
                .into_iter()
                .filter(|v| {
                    v.lts.is_some()
                        && installed.contains(&v.version)
                })
                .collect();
            lts_of(&lts_installed)
                .or_else(|| version_matching(&installed_rv, "lts"))
        },
        "nightly" => version_matching(&installed_rv, "nightly"),
        _ => None,
    };
    resolved.ok_or_else(not_found)
}

/// 收集某工具本地已安装的版本（目录名，semver 升序）。
/// 工具目录不存在时返回空表（由调用方决定如何报错）
pub fn installed_versions(name: &str) -> Vec<String> {
    installed_versions_in(&paths::tools_dir().join(name))
}

fn installed_versions_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut versions = Vec::new();
    for e in entries.flatten() {
        if e.file_type().is_ok_and(|t| t.is_dir())
            && let Some(name) = e.file_name().to_str() {
                versions.push(name.to_string());
            }
    }
    // semver 升序（最新在最后），无法解析时回退字符串序
    versions.sort_by(|a, b| match (parse_version(a), parse_version(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => a.cmp(b),
    });
    versions
}

/// 是否为形如 `22` / `22.0` 的部分版本（仅数字与点）
fn is_partial_version(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// 在版本条目中解析具体版本号（必为 x.y.z），失败返回 None
fn parse_version(s: &str) -> Option<semver::Version> {
    semver::Version::parse(s.trim_start_matches(['v', 'V'])).ok()
}

/// 取语义化版本最新；无合法版本时回退到列表中最后一个
fn latest_of(versions: &[RemoteVersion]) -> Option<String> {
    let mut with_ver: Vec<(&RemoteVersion, semver::Version)> = versions
        .iter()
        .filter_map(|v| parse_version(&v.version).map(|p| (v, p)))
        .collect();
    with_ver.sort_by(|a, b| a.1.cmp(&b.1));
    with_ver
        .last()
        .map(|(v, _)| v.version.clone())
        .or_else(|| versions.last().map(|v| v.version.clone()))
}

/// 取 LTS 元数据存在的最新版本；无则 None
fn lts_of(versions: &[RemoteVersion]) -> Option<String> {
    let matched: Vec<RemoteVersion> =
        versions.iter().filter(|v| v.lts.is_some()).cloned().collect();
    if matched.is_empty() { None } else { latest_of(&matched) }
}

/// 取版本号包含指定关键字的条目中最新者；无则 None
fn version_matching(
    versions: &[RemoteVersion], keyword: &str,
) -> Option<String> {
    let matched: Vec<RemoteVersion> = versions
        .iter()
        .filter(|v| v.version.to_lowercase().contains(keyword))
        .cloned()
        .collect();
    if matched.is_empty() { None } else { latest_of(&matched) }
}

/// 匹配部分版本前缀（22 / 22.19）的最新完整版本。
/// 第三条件仅允许「请求在版本之后继续下一段」（如请求 2.0 匹配
/// 版本 2），防止请求 22 误匹配版本 2
fn resolve_partial(versions: &[RemoteVersion], prefix: &str) -> Option<String> {
    let matched: Vec<RemoteVersion> = versions
        .iter()
        .filter(|v| {
            v.version.as_str() == prefix
                || v.version.starts_with(&format!("{prefix}."))
                || prefix.starts_with(&format!("{}.", v.version))
        })
        .cloned()
        .collect();
    if matched.is_empty() { None } else { latest_of(&matched) }
}

/// 请求 api 源并解析版本条目（version_path 定位 + 元数据保留 + pattern 清洗）
async fn fetch_api_versions(
    url: &str, version_path: Option<&str>, version_pattern: Option<&str>,
) -> Result<Vec<RemoteVersion>, UError> {
    let response = HTTP_CLIENT.get(url).await?;
    let text = response.text().await.map_err(|source| {
        UError::NetworkError { url: url.to_string(), source }
    })?;
    let raw = extract_versions_from_api(&text, version_path)?;
    Ok(apply_version_pattern(raw, version_pattern))
}

/// 获取工具的远端已发布版本列表（install 解析与 `uvman list <tool> --remote`
/// 的统一数据源）。
///
/// - `static` 源：直接返回插件定义的固定列表，本地数据无需缓存
/// - `api` 源：优先读 `cache/versions/` 中未过期的缓存；缺失或过期时
///   联网拉取并写回缓存。缓存文件名携带过期时间（unix 秒，16 进制）：
///   `{tool}_remote_version_{expires_at}.json`
pub async fn remote_versions(name: &str) -> Result<Vec<RemoteVersion>, UError> {
    let plugin =
        ToolPlugin::load_from(&paths::plugin_path(name)).map_err(|_| {
            UError::PluginNotInstalled {
                name: name.to_string(),
                similar: did_you_mean_installed(name),
            }
        })?;
    remote_versions_of(&plugin, name).await
}

/// 基于已加载插件获取远端版本（install 流程复用，避免二次读插件）
async fn remote_versions_of(
    plugin: &ToolPlugin, name: &str,
) -> Result<Vec<RemoteVersion>, UError> {
    match &plugin.release {
        Release::Static { versions } => Ok(versions
            .iter()
            .map(|v| RemoteVersion { version: v.clone(), lts: None })
            .collect()),
        Release::Api { url, version_path, version_pattern } => {
            let dir = paths::cache_versions_dir();
            if let Some(cached) = load_cached_versions(&dir, name) {
                return Ok(cached);
            }
            let versions = fetch_api_versions(
                url,
                version_path.as_deref(),
                version_pattern.as_deref(),
            )
            .await?;
            save_cached_versions(&dir, name, &versions);
            Ok(versions)
        },
    }
}

/// 从已安装插件中取拼写相近的名称（did-you-mean 建议）
fn did_you_mean_installed(name: &str) -> Vec<String> {
    let installed: Vec<String> = std::fs::read_dir(paths::plugins_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| {
                    let p = e.path();
                    (p.extension().and_then(|x| x.to_str()) == Some("toml"))
                        .then(|| {
                            p.file_stem()
                                .and_then(|s| s.to_str())
                                .map(str::to_string)
                        })
                        .flatten()
                })
                .collect()
        })
        .unwrap_or_default();
    did_you_mean(name, &installed)
}

/// 远端版本缓存文件名：`{tool}_remote_version_{过期时间 16 进制}.json`
fn version_cache_file(tool: &str, expires_at: u64) -> String {
    format!("{tool}_remote_version_{expires_at:x}.json")
}

/// 从缓存文件名解析过期时间；非本工具缓存或命名非法时返回 None
fn parse_cache_expiry(tool: &str, file_name: &str) -> Option<u64> {
    let hex = file_name
        .strip_suffix(".json")?
        .strip_prefix(&format!("{tool}_remote_version_"))?;
    u64::from_str_radix(hex, 16).ok()
}

/// 读取未过期的远端版本缓存；顺手清理该工具已过期的缓存文件。
/// 旧版纯字符串数组缓存反序列化失败 → 视为无缓存，重新拉取后自然升级
fn load_cached_versions(dir: &Path, tool: &str) -> Option<Vec<RemoteVersion>> {
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        let Some(expires_at) = parse_cache_expiry(tool, &file_name) else {
            continue;
        };
        if expires_at <= unix_now() {
            let _ = fs::remove_file(entry.path());
            continue;
        }
        if best.as_ref().is_none_or(|(e, _)| expires_at > *e) {
            best = Some((expires_at, entry.path()));
        }
    }
    let (_, path) = best?;
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// 写入远端版本缓存（best-effort，失败不影响查询结果）。
///
/// 过期时间 = 当前时间 + TTL（来自 `cache.ttl`，默认 24 小时），
/// 以 16 进制编入文件名；`ttl = 0` 表示不保留缓存
fn save_cached_versions(dir: &Path, tool: &str, versions: &[RemoteVersion]) {
    let ttl = cache_ttl();
    if ttl == 0 {
        return;
    }
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    // 覆盖式写入：先清理同工具旧缓存，避免多份文件累积
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if parse_cache_expiry(tool, &file_name).is_some() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    let expires_at = unix_now().saturating_add(ttl * 3600);
    let path = dir.join(version_cache_file(tool, expires_at));
    if let Ok(json) = serde_json::to_vec_pretty(versions) {
        let _ = fs::write(path, json);
    }
}

/// 将 version_pattern 应用到每个条目的版本号；无 pattern 或匹配失败时保留原值
fn apply_version_pattern(
    versions: Vec<RemoteVersion>, pattern: Option<&str>,
) -> Vec<RemoteVersion> {
    let Some(p) = pattern else {
        return versions;
    };
    let Ok(re) = regex::Regex::new(p) else {
        return versions;
    };
    versions
        .into_iter()
        .filter_map(|entry| {
            let caps = re.captures(&entry.version)?;
            let v = caps
                .name("version")
                .or_else(|| caps.get(1))
                .or_else(|| caps.get(0))?
                .as_str()
                .trim();
            if v.is_empty() {
                None
            } else {
                Some(RemoteVersion { version: v.to_string(), lts: entry.lts })
            }
        })
        .collect()
}

/// 从 API 响应文本中提取版本条目（版本号 + lts 元数据）
fn extract_versions_from_api(
    text: &str, version_path: Option<&str>,
) -> Result<Vec<RemoteVersion>, UError> {
    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|source| {
            UError::SimpleError(format!("invalid release response: {source}"))
        })?;
    let target = match version_path {
        Some(path) => navigate_json(&root, path).unwrap_or(&root),
        None => &root,
    };

    let mut raw: Vec<RemoteVersion> = Vec::new();
    match target {
        serde_json::Value::Array(items) => {
            for item in items {
                match item {
                    // 纯字符串数组：无元数据
                    serde_json::Value::String(s) => raw
                        .push(RemoteVersion { version: s.clone(), lts: None }),
                    // 对象：取版本字段，并保留 lts 元数据
                    obj @ serde_json::Value::Object(_) => {
                        if let Some(s) = object_version_field(obj) {
                            raw.push(RemoteVersion {
                                version: s,
                                lts: object_lts_field(obj),
                            });
                        }
                    },
                    _ => {},
                }
            }
        },
        serde_json::Value::String(s) => {
            raw.push(RemoteVersion { version: s.clone(), lts: None })
        },
        obj @ serde_json::Value::Object(_) => {
            if let Some(s) = object_version_field(obj) {
                raw.push(RemoteVersion {
                    version: s,
                    lts: object_lts_field(obj),
                });
            }
        },
        _ => {},
    }
    Ok(raw)
}

/// 从对象中取 LTS 代号：仅字符串值视为 LTS 线（node index.json 中
/// 非 LTS 为布尔 `false`，`as_str` 天然返回 None）
fn object_lts_field(v: &serde_json::Value) -> Option<String> {
    v.as_object()?.get("lts")?.as_str().map(str::to_string)
}

/// 从对象中取版本字段（version / tag_name / name）
fn object_version_field(v: &serde_json::Value) -> Option<String> {
    let obj = v.as_object()?;
    for key in ["version", "tag_name", "name"] {
        if let Some(s) = obj.get(key).and_then(serde_json::Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}

/// 按点分路径在 JSON 中导航（如 `data.versions`）
fn navigate_json<'a>(
    value: &'a serde_json::Value, path: &str,
) -> Option<&'a serde_json::Value> {
    let mut cur = value;
    for seg in path.split('.') {
        cur = match cur {
            serde_json::Value::Object(map) => map.get(seg)?,
            serde_json::Value::Array(_) if seg == "0" || seg.is_empty() => cur,
            _ => return None,
        };
    }
    Some(cur)
}

/// 构造校验和获取计划（渲染候选 URL 与 pattern，不联网）。
/// 联网拉取推迟到执行期（下载完成后），保证 plan 阶段零网络 IO
fn hash_plan(
    plugin: &ToolPlugin, bin: &InstallBin, vars: &mut HashMap<&str, &str>,
) -> Result<Option<HashPlan>, UError> {
    let hash = &bin.download.hash;
    if !hash.enabled {
        return Ok(None);
    }
    let algorithm = hash.algorithm.as_deref().unwrap_or("sha256").to_string();
    let hash_path = hash.path.as_ref().ok_or_else(|| {
        UError::SimpleError(
            "hash is enabled but `download.hash.path` is missing".into(),
        )
    })?;

    // 校验和文件与档案同源布局：对每个候选源分别渲染；
    // 循环局部的 source 引用不入 vars（clone 局部副本），避免生命周期外泄
    let urls: Vec<String> = merged_sources(&plugin.tool.name, plugin)
        .iter()
        .map(|source| {
            let mut scoped = vars.clone();
            scoped.insert("registry", source.as_str());
            plugin.render(hash_path, &scoped)
        })
        .collect();

    let pattern = hash
        .pattern
        .as_deref()
        .map(|p| plugin.render(p, vars));
    let filename = vars
        .get("filename")
        .map(|s| s.to_string())
        .unwrap_or_default();

    Ok(Some(HashPlan { urls, algorithm, pattern, filename }))
}

/// 按候选源顺序拉取并解析期望校验和（镜像 → default 逐个回退，
/// 全部失败才报错）
async fn fetch_expected_hash(hp: &HashPlan) -> Result<String, UError> {
    let mut last_err = None;
    for url in &hp.urls {
        match HTTP_CLIENT.get(url).await {
            Ok(response) => match response.text().await {
                Ok(text) => {
                    return extract_hash(
                        &text,
                        hp.pattern.as_deref(),
                        &hp.filename,
                    );
                },
                Err(source) => {
                    let e = UError::NetworkError {
                        url: url.clone(),
                        source,
                    };
                    crate::ui::report::print_warning(&format!(
                        "checksum source failed, trying next: {e}"
                    ));
                    last_err = Some(e);
                },
            },
            Err(e) => {
                crate::ui::report::print_warning(&format!(
                    "checksum source failed, trying next: {e}"
                ));
                last_err = Some(e);
            },
        }
    }
    Err(last_err.unwrap_or_else(|| UError::SimpleError(
        "no download source available for checksum file".into(),
    )))
}

/// 从校验和文件中提取期望值：优先 pattern；无 pattern 时优先取
/// 档案官方文件名所在行（官方校验和文件是多文件列表，盲目取首个
/// hex 会拿到其他平台产物的哈希），最后回退全文件首个 hex
fn extract_hash(
    text: &str, pattern: Option<&str>, filename: &str,
) -> Result<String, UError> {
    if let Some(p) = pattern {
        // 校验和文件为多行列表（每行一个文件），^$ 必须按行锚定；
        // 用户 pattern 未显式写 (?m) 时自动开启
        let re = regex::RegexBuilder::new(p)
            .multi_line(true)
            .build()
            .map_err(|_| {
                UError::SimpleError(format!("invalid checksum pattern '{p}'"))
            })?;
        if let Some(caps) = re.captures(text)
            && let Some(g) = caps
                .name("hash")
                .or_else(|| caps.get(1))
                .or_else(|| caps.get(0))
        {
            let v = g.as_str().trim().to_lowercase();
            if !v.is_empty() {
                return Ok(v);
            }
        }
        return Err(UError::ChecksumError {
            message: "hash not found in checksum file".into(),
        });
    }

    // 无 pattern：按行 `<hash> [ *]<filename>` 匹配本档案的条目；
    // 不锚定行尾（兼容 CRLF 行尾残留 \r）
    if !filename.is_empty() {
        for len in [64usize, 128] {
            let re = regex::Regex::new(&format!(
                r"(?im)^[0-9a-f]{{{len}}}\s+\*?{}",
                regex::escape(filename)
            ))
            .expect("valid checksum line regex");
            if let Some(m) = re.find(text) {
                let hash = m.as_str().split_whitespace().next().unwrap_or("");
                return Ok(hash.to_lowercase());
            }
        }
    }

    // 最终回退：全文件第一个 64/128 位 hex
    for len in [64usize, 128] {
        let re = regex::Regex::new(&format!(r"(?i)\b[0-9a-f]{{{len}}}\b"))
            .expect("valid hex regex");
        if let Some(m) = re.find(text) {
            return Ok(m.as_str().to_lowercase());
        }
    }
    Err(UError::ChecksumError {
        message: "no hex checksum found in checksum file".into(),
    })
}

/// 计算档案校验和
fn compute_checksum(path: &Path, algorithm: &str) -> Result<String, UError> {
    use sha2::{Sha256, Sha512};
    let file = fs::File::open(path).map_err(|source| {
        UError::FileError { path: path.to_path_buf(), source }
    })?;
    // 分块 update 而非 io::copy：hasher 的 io::Write 实现依赖 sha2
    // 的 std feature，依赖树裁剪后可能不可用
    let digest: String = match algorithm {
        "sha256" => hash_file::<Sha256>(file)?,
        "sha512" => hash_file::<Sha512>(file)?,
        _ => {
            return Err(UError::SimpleError(format!(
                "unsupported checksum algorithm '{algorithm}'"
            )));
        },
    };
    Ok(digest)
}

/// 分块读取文件并更新哈希，返回 hex 摘要
fn hash_file<D: sha2::Digest>(mut file: fs::File) -> Result<String, UError> {
    use std::io::Read;
    let mut hasher = D::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|source| UError::IoError {
            source,
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// URL 最后一个路径段（官方发布物文件名），无路径分隔时回退整串
fn url_basename(url: &str) -> String {
    url.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(url)
        .to_string()
}

/// 按扩展名解压档案到 dest，剥离前 strip 个顶层目录
fn extract_archive(
    archive: &Path, dest: &Path, ext: &str, strip: u32,
) -> Result<(), UError> {
    match ext {
        "zip" => extract_zip(archive, dest, strip),
        "tar.gz" | "tgz" => extract_tar_gz(archive, dest, strip),
        other => Err(UError::SimpleError(format!(
            "unsupported archive extension '{other}'"
        ))),
    }
}

fn extract_zip(archive: &Path, dest: &Path, strip: u32) -> Result<(), UError> {
    let file = fs::File::open(archive).map_err(|source| UError::FileError {
        path: archive.to_path_buf(),
        source,
    })?;
    let mut zip =
        zip::ZipArchive::new(file).map_err(|source| UError::ExtractError {
            path: archive.to_path_buf(),
            source: Box::new(source),
        })?;

    for i in 0..zip.len() {
        let mut entry =
            zip.by_index(i).map_err(|source| UError::ExtractError {
                path: archive.to_path_buf(),
                source: Box::new(source),
            })?;
        let name = entry.name().to_string();
        let out_path = stripped_path(dest, &name, strip)?;
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|source| {
                UError::FileError { path: out_path.clone(), source }
            })?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|source| UError::FileError {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut out = fs::File::create(&out_path).map_err(|source| {
            UError::FileError { path: out_path.clone(), source }
        })?;
        std::io::copy(&mut entry, &mut out).map_err(|source| {
            UError::FileError { path: out_path.clone(), source }
        })?;
    }
    Ok(())
}

fn extract_tar_gz(
    archive: &Path, dest: &Path, strip: u32,
) -> Result<(), UError> {
    let file = fs::File::open(archive).map_err(|source| UError::FileError {
        path: archive.to_path_buf(),
        source,
    })?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);

    for entry in tar.entries().map_err(|source| UError::ExtractError {
        path: archive.to_path_buf(),
        source: Box::new(source),
    })? {
        let mut entry = entry.map_err(|source| UError::ExtractError {
            path: archive.to_path_buf(),
            source: Box::new(source),
        })?;
        let name = entry
            .path()
            .map_err(|source| UError::ExtractError {
                path: archive.to_path_buf(),
                source: Box::new(source),
            })?
            .to_string_lossy()
            .into_owned();
        let out_path = stripped_path(dest, &name, strip)?;
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            fs::create_dir_all(&out_path).map_err(|source| {
                UError::FileError { path: out_path.clone(), source }
            })?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|source| UError::FileError {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        if entry_type.is_file() {
            let mut out = fs::File::create(&out_path).map_err(|source| {
                UError::FileError { path: out_path.clone(), source }
            })?;
            std::io::copy(&mut entry, &mut out).map_err(|source| {
                UError::FileError { path: out_path.clone(), source }
            })?;
        }
    }
    Ok(())
}

/// 将档案内路径安全拼接到 dest 下，剥离前 strip 个顶层组件，并过滤越界组件
fn stripped_path(
    dest: &Path, name: &str, strip: u32,
) -> Result<PathBuf, UError> {
    let mut comps: Vec<String> = Path::new(name)
        .components()
        .filter_map(|c| match c {
            Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if comps.is_empty() {
        return Err(UError::SimpleError(format!(
            "archive entry '{name}' has no valid path"
        )));
    }
    let skip = (strip as usize).min(comps.len());
    comps.drain(0..skip);
    Ok(comps.iter().fold(dest.to_path_buf(), |acc, c| acc.join(c)))
}

/// 将解压根目录下的 bin 子目录内容复制到安装目录
fn copy_bin(
    extract_dir: &Path, bin_dir: &str, install_dir: &Path,
) -> Result<(), UError> {
    let src = extract_dir.join(bin_dir);
    if !src.exists() {
        return Err(UError::SimpleError(format!(
            "bin dir '{}' not found in extracted archive",
            src.display()
        )));
    }
    copy_dir_contents(&src, install_dir)
}

/// 递归复制目录内容
fn copy_dir_contents(src: &Path, dest: &Path) -> Result<(), UError> {
    for entry in fs::read_dir(src).map_err(|source| UError::FileError {
        path: src.to_path_buf(),
        source,
    })? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            fs::create_dir_all(&to).map_err(|source| UError::FileError {
                path: to.clone(),
                source,
            })?;
            copy_dir_contents(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|source| UError::FileError {
                path: from.clone(),
                source,
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn versions(list: &[&str]) -> Vec<RemoteVersion> {
        list.iter()
            .map(|s| RemoteVersion { version: s.to_string(), lts: None })
            .collect()
    }

    #[test]
    fn test_installed_versions_scans_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("node");
        fs::create_dir_all(tool.join("22.19.0")).unwrap();
        fs::create_dir_all(tool.join("22.9.1")).unwrap();
        fs::create_dir_all(tool.join("10.0.0")).unwrap();
        // 非目录条目（缓存残留文件）不进入版本列表
        fs::write(tool.join("records.json"), "{}").unwrap();

        let v = installed_versions_in(&tool);
        assert_eq!(v, vec!["10.0.0", "22.9.1", "22.19.0"]);
        // 目录不存在 → 空表
        assert!(installed_versions_in(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn test_latest_of() {
        let v = versions(&["1.0.0", "2.10.0", "2.9.0", "0.5.0"]);
        assert_eq!(latest_of(&v).as_deref(), Some("2.10.0"));
    }

    #[test]
    fn test_latest_of_with_prefix() {
        // 带 v 前缀的版本也能被解析比较
        let v = versions(&["20.11.0", "20.12.0", "v20.13.0"]);
        assert_eq!(latest_of(&v).as_deref(), Some("v20.13.0"));
    }

    #[test]
    fn test_lts_of_uses_metadata() {
        // lts 元数据优先：最新版是 Current，但最新 LTS 是 22.12.0
        let v = vec![
            RemoteVersion { version: "26.7.0".into(), lts: None },
            RemoteVersion {
                version: "22.12.0".into(),
                lts: Some("Jod".into()),
            },
            RemoteVersion {
                version: "20.11.0".into(),
                lts: Some("Iron".into()),
            },
        ];
        assert_eq!(lts_of(&v).as_deref(), Some("22.12.0"));
        // 无任何 LTS 元数据 → None（不回退 latest）
        assert_eq!(lts_of(&versions(&["1.0.0", "2.0.0"])), None);
    }

    #[test]
    fn test_version_matching_keyword() {
        // static 源命名约定回退：版本字符串含关键字
        let v = versions(&["22.0.0", "22.1.0-lts", "22.2.0-lts.1"]);
        assert_eq!(
            version_matching(&v, "lts").as_deref(),
            Some("22.2.0-lts.1")
        );
        assert_eq!(version_matching(&v, "nightly"), None);
    }

    #[test]
    fn test_resolve_partial_major() {
        let v = versions(&["22.0.0", "22.11.0", "20.11.0"]);
        assert_eq!(resolve_partial(&v, "22").as_deref(), Some("22.11.0"));
        assert_eq!(resolve_partial(&v, "20").as_deref(), Some("20.11.0"));
        assert_eq!(resolve_partial(&v, "18"), None);
    }

    #[test]
    fn test_resolve_partial_minor() {
        let v = versions(&["22.0.0", "22.0.1", "22.1.0"]);
        assert_eq!(resolve_partial(&v, "22.0").as_deref(), Some("22.0.1"));
        // node@22.19 → 22.19.z 中最新
        let v = versions(&["22.19.0", "22.19.3", "22.20.0"]);
        assert_eq!(resolve_partial(&v, "22.19").as_deref(), Some("22.19.3"));
    }

    /// 请求 22 不得误匹配版本 2（旧实现的第三条件缺陷）
    #[test]
    fn test_resolve_partial_no_false_major_match() {
        let v = versions(&["2.0.0", "20.0.0"]);
        // "22" 不匹配 "2"（"22" 未在 "2." 之后继续下一段）
        assert_eq!(resolve_partial(&v, "22"), None);
        // 请求 2.0 允许匹配无点版本 2（请求在版本后继续下一段）
        let v2 = versions(&["2", "3.0.0"]);
        assert_eq!(resolve_partial(&v2, "2.0").as_deref(), Some("2"));
    }

    /// 记录的校验和与本地校验的完整流转：
    /// 写入 → 校验通过；档案被篡改 → 校验失败
    #[test]
    fn test_recorded_hash_detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let tool_dir = dir.path().join("node");
        fs::create_dir_all(&tool_dir).unwrap();
        let archive = tool_dir.join("node-22.0.0-win-x64.zip");
        fs::write(&archive, b"hello").unwrap();

        // 未记录 hash → 跳过校验
        assert!(verify_recorded_hash(&cache_plan(dir.path(), "node-22.0.0-win-x64.zip")).is_ok());

        // 记录正确 hash → 校验通过
        let digest = compute_checksum(&archive, "sha256").unwrap();
        record_archive(&archive, 24, Some(("sha256", &digest)));
        assert!(verify_recorded_hash(&cache_plan(dir.path(), "node-22.0.0-win-x64.zip")).is_ok());

        // 档案被篡改 → 校验失败
        fs::write(&archive, b"tampered").unwrap();
        assert!(verify_recorded_hash(&cache_plan(dir.path(), "node-22.0.0-win-x64.zip")).is_err());
    }

    /// 全局 registry 合并：插件源去重、顺序保持
    /// （测试环境 GLOBAL_CONFIG.registry 为空，全局分支经由配置测试覆盖）
    #[test]
    fn test_merged_sources_dedup_plugin_sources() {
        let plugin: ToolPlugin = toml::from_str(
            r#"
[tool]
name = "node"
[registry]
default = "https://nodejs.org/dist"
mirrors = ["https://npmmirror.com/mirrors/node"]
[release]
source = "static"
versions = []
[platform]
os_map = { windows = "win" }
arch_map = { x86_64 = "x64" }
[install.defaults]
version = "latest"
mode = "bin"
[[install.bin]]
os = ["windows"]
arch = ["x86_64"]
[install.bin.download]
path = "{registry}/node-{version}-{os}-{arch}.{ext}"
[install.bin.download.ext]
windows = "zip"
[install.bin.download.hash]
enabled = false
[install.bin.extract]
strip = 1
[install.bin.deploy]
bin_dir = "bin"
"#,
        )
        .unwrap();
        let sources = merged_sources("node", &plugin);
        assert_eq!(
            sources,
            vec![
                "https://npmmirror.com/mirrors/node".to_string(),
                "https://nodejs.org/dist".to_string(),
            ]
        );
    }

    #[test]
    fn test_extract_versions_from_api_array_of_objects() {
        let json = r#"[
            {"version": "v20.11.0", "lts": "Iron"},
            {"version": "v20.12.0", "lts": "Iron"},
            {"version": "v22.0.0", "lts": false}
        ]"#;
        let out = extract_versions_from_api(json, None).unwrap();
        assert_eq!(
            out,
            vec![
                RemoteVersion {
                    version: "v20.11.0".into(),
                    lts: Some("Iron".into())
                },
                RemoteVersion {
                    version: "v20.12.0".into(),
                    lts: Some("Iron".into())
                },
                // lts: false（布尔）不构成 LTS 标记
                RemoteVersion { version: "v22.0.0".into(), lts: None },
            ]
        );
    }

    #[test]
    fn test_extract_versions_from_api_with_path() {
        let json = r#"{"data": {"releases": ["1.2.3", "1.2.4"]}}"#;
        let out =
            extract_versions_from_api(json, Some("data.releases")).unwrap();
        assert_eq!(
            out,
            vec![
                RemoteVersion { version: "1.2.3".into(), lts: None },
                RemoteVersion { version: "1.2.4".into(), lts: None },
            ]
        );
    }

    #[test]
    fn test_apply_pattern_keeps_lts_metadata() {
        // pattern 清洗 v 前缀时必须保留 lts 元数据
        let raw = vec![
            RemoteVersion {
                version: "v20.11.0".into(),
                lts: Some("Iron".into()),
            },
            RemoteVersion { version: "v22.0.0".into(), lts: None },
        ];
        let out = apply_version_pattern(raw, Some("^v(?<version>.*)$"));
        assert_eq!(
            out,
            vec![
                RemoteVersion {
                    version: "20.11.0".into(),
                    lts: Some("Iron".into())
                },
                RemoteVersion { version: "22.0.0".into(), lts: None },
            ]
        );
    }

    #[test]
    fn test_stripped_path_strips_and_sanitizes() {
        let dest = Path::new("out");
        let p = stripped_path(dest, "pkg-1.0.0/bin/app.exe", 1).unwrap();
        assert_eq!(p, dest.join("bin").join("app.exe"));

        // 越界组件（../）被过滤，不会逃逸到 dest 之外
        let p = stripped_path(dest, "pkg/../../evil.txt", 1).unwrap();
        assert_eq!(p, dest.join("evil.txt"));
    }

    #[test]
    fn test_extract_zip_with_strip() {
        let dir = tempfile::tempdir().unwrap();
        let archive_path = dir.path().join("node.zip");
        let archive_file = fs::File::create(&archive_path).unwrap();
        let mut zip = zip::ZipWriter::new(archive_file);
        let opts: zip::write::SimpleFileOptions = Default::default();
        zip.add_directory("node-v20-bin/", opts).unwrap();
        zip.start_file("node-v20-bin/node.exe", opts).unwrap();
        zip.write_all(b"binary").unwrap();
        zip.finish().unwrap();

        let dest = dir.path().join("extracted");
        extract_zip(&archive_path, &dest, 1).unwrap();

        let bin = dest.join("node.exe");
        assert!(bin.exists(), "strip 后的 bin 文件应存在");
        assert_eq!(fs::read(&bin).unwrap(), b"binary");
    }

    #[test]
    fn test_compute_checksum_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("data.bin");
        fs::write(&file, b"hello").unwrap();

        let digest = compute_checksum(&file, "sha256").unwrap();
        // "hello" 的 sha256
        assert_eq!(
            digest,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_extract_hash_pattern() {
        let text = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  node.exe";
        let out = extract_hash(
            text,
            Some(r"(?P<hash>[0-9a-f]{64})"),
            "node.exe",
        )
        .unwrap();
        assert_eq!(out.len(), 64);
        // 无 pattern 时按档案文件名匹配所在行
        let out2 = extract_hash(text, None, "node.exe").unwrap();
        assert_eq!(out2.len(), 64);
    }

    /// SHASUMS256.txt 实际形态：多行、目标文件不在首行、
    //  官方文件名与 pattern 的 ^$ 锚定必须逐行生效
    #[test]
    fn test_extract_hash_multiline_anchors() {
        let text = "\
1111111111111111111111111111111111111111111111111111111111111111  node-v24.19.0-aix-ppc64.tar.gz
2222222222222222222222222222222222222222222222222222222222222222  node-v24.19.0-linux-x64.tar.xz
2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  node-v24.19.0-win-x64.zip
";
        let pattern = r"^([a-f0-9]{64})\s+.*node-v24.19.0-win-x64.zip$";
        let out = extract_hash(text, Some(pattern), "node-v24.19.0-win-x64.zip").unwrap();
        assert_eq!(
            out,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    /// 无 pattern + 多文件列表：必须按档案文件名取行，
    /// 而非全文件第一个 hex（那是其他平台产物的哈希）
    #[test]
    fn test_extract_hash_no_pattern_prefers_filename_line() {
        let text = "\
1111111111111111111111111111111111111111111111111111111111111111  node-v24.19.0-aix-ppc64.tar.gz
2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  node-v24.19.0-win-x64.zip
";
        let out =
            extract_hash(text, None, "node-v24.19.0-win-x64.zip").unwrap();
        assert_eq!(
            out,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        // 文件名无对应行时回退全文件首个 hex
        let fallback = extract_hash(text, None, "no-such-file.zip").unwrap();
        assert_eq!(fallback.len(), 64);
        assert!(fallback.starts_with("1111"));
    }

    #[test]
    fn test_url_basename() {
        assert_eq!(
            url_basename("https://nodejs.org/dist/v24.19.0/node-v24.19.0-win-x64.zip"),
            "node-v24.19.0-win-x64.zip"
        );
        assert_eq!(url_basename("https://x.y/z/"), "z");
        assert_eq!(url_basename("plain-name"), "plain-name");
    }

    /// 将文件 mtime 回拨指定时长，用于构造"过期"档案
    fn age_file(path: &Path, ago: Duration) {
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - ago).unwrap();
    }

    #[test]
    fn test_gc_migrates_legacy_layout() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();

        // 旧布局：cache/node/{extract/ 残留, 档案}
        let legacy = cache.join("node");
        fs::create_dir_all(legacy.join("extract")).unwrap();
        fs::write(legacy.join("extract/leftover.txt"), b"x").unwrap();
        fs::write(legacy.join("old.zip"), b"x").unwrap();

        // cache 根下的散落文件（如 plugins.json）不属于下载缓存
        fs::write(cache.join("plugins.json"), b"{}").unwrap();

        gc_cache_at(cache, 24);

        assert!(!legacy.exists(), "旧布局目录应被移除");
        let new_tool = cache.join("tools").join("node");
        assert!(
            new_tool.join("old.zip").exists(),
            "档案应迁移到 cache/tools/<tool>/"
        );
        assert!(!new_tool.join("extract").exists(), "解压残留不应被迁移");
        let records = load_records(&new_tool);
        assert!(records.archives.contains_key("old.zip"), "迁移后应补缓存记录");
        assert!(cache.join("plugins.json").exists(), "cache 根文件不应被清理");
    }

    #[test]
    fn test_gc_removes_expired_by_records_and_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        let tool_dir = cache.join("tools").join("node");
        fs::create_dir_all(&tool_dir).unwrap();

        let fresh = tool_dir.join("fresh.zip");
        let expired = tool_dir.join("expired.zip");
        let orphan = tool_dir.join("orphan.zip");
        fs::write(&fresh, b"x").unwrap();
        fs::write(&expired, b"x").unwrap();
        fs::write(&orphan, b"x").unwrap();
        age_file(&orphan, Duration::from_secs(3 * 24 * 3600));

        let mut records = ArchiveRecords::default();
        records.archives.insert(
            "fresh.zip".into(),
            ArchiveRecord {
                downloaded_at: unix_now(),
                ttl_hours: 24,
                algorithm: None,
                hash: None,
            },
        );
        records.archives.insert(
            "expired.zip".into(),
            ArchiveRecord {
                downloaded_at: unix_now() - 25 * 3600,
                ttl_hours: 24,
                algorithm: None,
                hash: None,
            },
        );
        save_records(&tool_dir, &records);

        gc_cache_at(cache, 24);

        assert!(fresh.exists(), "TTL 内的档案应保留");
        assert!(!expired.exists(), "超过记录 TTL 的档案应被删除");
        assert!(!orphan.exists(), "无记录的过期孤儿档案应被兜底删除");
        let after = load_records(&tool_dir);
        assert!(
            !after.archives.contains_key("expired.zip"),
            "过期记录应被修剪"
        );
        assert!(after.archives.contains_key("fresh.zip"));
    }

    #[test]
    fn test_gc_zero_ttl_purges_all() {
        // ttl = 0 表示完全不保留缓存：旧布局与新布局全部清除
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();

        let legacy = cache.join("node");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("a.zip"), b"x").unwrap();

        let tool_dir = cache.join("tools").join("node");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::write(tool_dir.join("b.zip"), b"x").unwrap();

        gc_cache_at(cache, 0);

        assert!(!legacy.exists(), "ttl=0 时旧布局应被清除");
        assert!(!tool_dir.join("b.zip").exists(), "ttl=0 时缓存档案应被清除");
    }

    #[test]
    fn test_record_archive_writes_and_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let tool_dir = dir.path().join("node");
        fs::create_dir_all(&tool_dir).unwrap();
        let archive = tool_dir.join("node-22.0.0-win-x64.zip");
        fs::write(&archive, b"x").unwrap();

        record_archive(&archive, 24, None);
        let first = load_records(&tool_dir)
            .archives
            .get("node-22.0.0-win-x64.zip")
            .cloned()
            .unwrap();
        assert_eq!(first.ttl_hours, 24);

        // 重复下载同一档案会刷新下载时间
        std::thread::sleep(Duration::from_secs(1));
        record_archive(&archive, 48, Some(("sha256", "abc123")));
        let second = load_records(&tool_dir)
            .archives
            .get("node-22.0.0-win-x64.zip")
            .cloned()
            .unwrap();
        assert_eq!(second.ttl_hours, 48);
        assert!(second.downloaded_at > first.downloaded_at);
    }

    /// 构造指向 dir 下工具档案的安装计划（仅用于缓存命中判定）
    fn cache_plan(dir: &Path, file: &str) -> InstallPlan {
        InstallPlan {
            name: "node".into(),
            version: "22.0.0".into(),
            urls: vec![String::new()],
            ext: "zip".into(),
            strip: 0,
            bin_dir: String::new(),
            hash: None,
            archive_path: dir.join("node").join(file),
            install_dir: dir.join("tools").join("node").join("22.0.0"),
        }
    }

    #[test]
    fn test_archive_cache_hit_rules() {
        let dir = tempfile::tempdir().unwrap();
        let dir = dir.path();
        let tool_dir = dir.join("node");
        fs::create_dir_all(&tool_dir).unwrap();

        // 文件不存在 → 未命中
        assert!(!archive_cache_hit(&cache_plan(dir, "a.zip")));

        // 存在且非空、无记录（孤儿档案）→ 命中
        fs::write(tool_dir.join("a.zip"), b"archive").unwrap();
        assert!(archive_cache_hit(&cache_plan(dir, "a.zip")));

        // 空文件（上次下载中断）→ 未命中
        fs::write(tool_dir.join("empty.zip"), b"").unwrap();
        assert!(!archive_cache_hit(&cache_plan(dir, "empty.zip")));

        // 记录未过期 → 命中；已过期 → 未命中
        let expired = tool_dir.join("old.zip");
        fs::write(&expired, b"archive").unwrap();
        record_archive(&expired, 24, None);
        assert!(archive_cache_hit(&cache_plan(dir, "old.zip")));

        let mut records = load_records(&tool_dir);
        records.archives.insert(
            "old.zip".into(),
            ArchiveRecord {
                downloaded_at: unix_now() - 25 * 3600,
                ttl_hours: 24,
                algorithm: None,
                hash: None,
            },
        );
        save_records(&tool_dir, &records);
        assert!(!archive_cache_hit(&cache_plan(dir, "old.zip")));
    }

    #[test]
    fn test_parse_cache_expiry() {
        // 文件名携带 16 进制过期时间
        assert_eq!(
            parse_cache_expiry(
                "node",
                &version_cache_file("node", 0x19a4c0f00)
            ),
            Some(0x19a4c0f00)
        );
        // 非 16 进制 / 其他工具前缀 / 缺后缀 → 无法解析
        assert_eq!(
            parse_cache_expiry("node", "node_remote_version_xz.json"),
            None
        );
        assert_eq!(
            parse_cache_expiry("node", "go_remote_version_10.json"),
            None
        );
        assert_eq!(parse_cache_expiry("node", "node_remote_version_10"), None);
        // 前缀必须是完整段：nodej_remote_version_10 不属于 node
        assert_eq!(
            parse_cache_expiry("node", "nodej_remote_version_10.json"),
            None
        );
    }

    #[test]
    fn test_version_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let versions = vec![
            RemoteVersion {
                version: "20.11.0".into(),
                lts: Some("Iron".into()),
            },
            RemoteVersion { version: "22.11.0".into(), lts: None },
        ];
        save_cached_versions(dir.path(), "node", &versions);

        // 保存后文件名应含 16 进制过期时间，且可原样读回（含 lts 元数据）
        let saved: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(saved.len(), 1, "同工具仅保留一份缓存");
        assert!(
            saved[0].starts_with("node_remote_version_")
                && saved[0].ends_with(".json")
        );
        assert_eq!(load_cached_versions(dir.path(), "node").unwrap(), versions);
    }

    #[test]
    fn test_version_cache_expired_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        // 过去的时间戳 → 已过期
        let stale =
            dir.path().join(version_cache_file("node", unix_now() - 10));
        fs::write(&stale, r#"[{"version":"1.0.0"}]"#).unwrap();

        assert!(load_cached_versions(dir.path(), "node").is_none());
        assert!(!stale.exists(), "过期缓存应被顺手清理");
    }

    #[test]
    fn test_save_replaces_old_cache() {
        let dir = tempfile::tempdir().unwrap();
        let old =
            dir.path().join(version_cache_file("node", unix_now() + 3600));
        fs::write(&old, r#"[{"version":"1.0.0"}]"#).unwrap();

        save_cached_versions(
            dir.path(),
            "node",
            &[RemoteVersion { version: "2.0.0".into(), lts: None }],
        );

        assert!(!old.exists(), "旧缓存应被替换而非累积");
        assert_eq!(
            load_cached_versions(dir.path(), "node").unwrap(),
            vec![RemoteVersion { version: "2.0.0".into(), lts: None }]
        );
    }

    #[test]
    fn test_load_cache_prefers_latest_expiry() {
        let dir = tempfile::tempdir().unwrap();
        // 同工具两份缓存（异常残留），应取过期时间更晚的一份
        let soon = dir.path().join(version_cache_file("node", unix_now() + 60));
        let late =
            dir.path().join(version_cache_file("node", unix_now() + 3600));
        fs::write(&soon, r#"[{"version":"1.0.0"}]"#).unwrap();
        fs::write(&late, r#"[{"version":"2.0.0"}]"#).unwrap();

        assert_eq!(
            load_cached_versions(dir.path(), "node").unwrap(),
            vec![RemoteVersion { version: "2.0.0".into(), lts: None }]
        );
    }

    #[test]
    fn test_load_cache_rejects_legacy_format() {
        // 旧版纯字符串数组缓存 → 解析失败视为无缓存（重新拉取后自然升级）
        let dir = tempfile::tempdir().unwrap();
        let legacy =
            dir.path().join(version_cache_file("node", unix_now() + 3600));
        fs::write(&legacy, r#"["1.0.0","2.0.0"]"#).unwrap();

        assert!(load_cached_versions(dir.path(), "node").is_none());
    }

    #[test]
    fn test_gc_skips_non_tool_cache_dirs() {
        // versions/builds 属其他缓存分区，不应被误当旧布局工具目录迁移
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        let legacy = cache.join("node");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("a.zip"), b"x").unwrap();

        let versions = cache.join("versions");
        fs::create_dir_all(&versions).unwrap();
        fs::write(
            versions.join(version_cache_file("node", unix_now() + 3600)),
            r#"["22.11.0"]"#,
        )
        .unwrap();

        gc_cache_at(cache, 24);

        assert!(
            versions
                .join(version_cache_file("node", unix_now() + 3600))
                .exists(),
            "versions 缓存不应被迁移"
        );
        assert!(!legacy.exists(), "旧布局工具目录仍应正常迁移");
    }
}
