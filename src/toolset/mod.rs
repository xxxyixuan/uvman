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

use serde::{Deserialize, Serialize};

use crate::core::config::GLOBAL_CONFIG;
use crate::core::error::UError;
use crate::core::http::HTTP_CLIENT;
use crate::core::plugin::{InstallBin, ToolPlugin};
use crate::core::{paths, platform};

/// 缓存 TTL 默认值：24 小时
const DEFAULT_CACHE_TTL_HOURS: u64 = 24;

/// 工具档案缓存记录文件名（位于 cache/tools/<tool>/ 下）
const RECORDS_FILE: &str = "records.json";

/// 一次安装所需的全部原子信息（纯数据，供 execute 逐项消费）
pub struct InstallPlan {
    pub name: String,
    /// 已解析为具体版本号（x.y.z），非代号
    pub version: String,
    /// 完整下载 URL（占位符已替换）
    pub url: String,
    /// 平台对应的压缩包扩展名（如 zip / tar.gz）
    pub ext: String,
    /// 解压时剥离的顶层目录层数
    pub strip: u32,
    /// 可执行文件在解压根目录中的相对目录
    pub bin_dir: String,
    /// 可选的校验和配置
    pub checksum: Option<Checksum>,
    /// 下载档案的落盘路径（cache 下）
    pub archive_path: PathBuf,
    /// 最终安装目录 (tools/<name>/<version>)
    pub install_dir: PathBuf,
}

/// 校验和描述：算法 + 期望值
pub struct Checksum {
    pub algorithm: String,
    pub expected: String,
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
    vars.insert("registry", plugin.registry.default.as_str());
    vars.insert("version", version.as_str());
    vars.insert("os", os.as_str());
    vars.insert("arch", arch.as_str());
    vars.insert("ext", ext.as_str());
    let filename = format!("{name}-{version}-{os}-{arch}.{ext}");
    vars.insert("filename", filename.as_str());
    let url = plugin.render(&bin.download.path, &vars);

    let checksum = resolve_checksum(&plugin, bin, &vars).await?;

    let archive_path = paths::cache_tools_dir()
        .join(name)
        .join(format!("{name}-{version}-{os}-{arch}.{ext}"));
    let install_dir = paths::tools_dir().join(name).join(&version);

    Ok(InstallPlan {
        name: name.to_string(),
        version,
        url,
        ext,
        strip: bin.extract.strip,
        bin_dir: bin.deploy.bin_dir.clone(),
        checksum,
        archive_path,
        install_dir,
    })
}

/// 依次执行下载、校验、解压、部署
pub async fn execute(plan: &InstallPlan) -> Result<(), UError> {
    // 惰性 GC：迁移旧布局、清理过期缓存（best-effort，失败不阻断安装）
    let ttl = cache_ttl();
    gc_cache_at(&paths::cache_dir(), ttl);

    HTTP_CLIENT
        .download_to(
            &plan.url,
            &plan.archive_path,
            GLOBAL_CONFIG.network.retries.unwrap_or(0),
            GLOBAL_CONFIG.network.retry_delay.unwrap_or(0),
        )
        .await?;

    // 下载成功即写入缓存记录，档案后续（哪怕安装失败）也能被 GC 按期回收
    if ttl > 0 {
        record_archive(&plan.archive_path, ttl);
    }

    verify_checksum(plan)?;

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

    fs::create_dir_all(&plan.install_dir).map_err(|source| {
        UError::FileError { path: plan.install_dir.clone(), source }
    })?;
    copy_bin(extract_dir.path(), &plan.bin_dir, &plan.install_dir)?;

    // ttl = 0：不保留缓存，安装成功后立即删除压缩包
    if ttl == 0 {
        let _ = fs::remove_file(&plan.archive_path);
    }
    Ok(())
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

/// 记录（或刷新）一个档案的下载时间与 TTL
fn record_archive(archive: &Path, ttl_hours: u64) {
    let Some(tool_dir) = archive.parent() else { return };
    let Some(name) = archive.file_name().and_then(OsStr::to_str) else {
        return;
    };
    let mut records = load_records(tool_dir);
    records.archives.insert(
        name.to_string(),
        ArchiveRecord { downloaded_at: unix_now(), ttl_hours },
    );
    save_records(tool_dir, &records);
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
        if let Ok(entries) = fs::read_dir(cache) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && entry.file_name() != *"tools" {
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
            if !legacy.is_dir() || entry.file_name() == *"tools" {
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
                ArchiveRecord { downloaded_at, ttl_hours: default_ttl },
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

/// 将版本请求解析为具体版本号。请求可为：
/// - `None`：使用插件默认版本（`install.defaults.version`）
/// - 具体版本（`20.11.0`）或部分版本（`22`、`22.0`）
/// - 代号：`latest` / `lts` / `nightly`
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

    let versions = fetch_versions(plugin).await?;

    // 部分版本（如 22 / 22.0）→ 匹配该前缀的最新版
    if is_partial_version(request) {
        return resolve_partial(&versions, request).ok_or_else(|| {
            UError::VersionNotFound {
                tool: name.to_string(),
                version: request.to_string(),
            }
        });
    }

    let resolved = match request {
        "latest" => latest_of(&versions),
        // TODO(LTS 解析缺陷)：代号解析依赖版本字符串包含关键字，但上游 API 的
        // lts 元数据是独立字段（如 nodejs.org index.json 的 `"lts": "Iron" |
        // false`）， extract_versions_from_api
        // 已将其丢弃，导致这里匹配失败后回退 latest_of， 最终装到非 LTS
        // 的最新 Current 版（如 node@lts → 26.7.0）。 修复方向：release
        // 解析阶段保留 (version, 元数据) 对，代号匹配基于元数据字段。
        "lts" => {
            version_matching(&versions, "lts").or_else(|| latest_of(&versions))
        },
        "nightly" => version_matching(&versions, "nightly")
            .or_else(|| latest_of(&versions)),
        _ => None,
    };
    resolved.ok_or_else(|| UError::VersionNotFound {
        tool: name.to_string(),
        version: request.to_string(),
    })
}

/// 是否为形如 `22` / `22.0` 的部分版本（仅数字与点）
fn is_partial_version(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// 在版本列表中解析具体版本号（必为 x.y.z），失败返回 None
fn parse_version(s: &str) -> Option<semver::Version> {
    semver::Version::parse(s.trim_start_matches(['v', 'V'])).ok()
}

/// 取语义化版本最新；无合法版本时回退到列表中最后一个
fn latest_of(versions: &[String]) -> Option<String> {
    let mut with_ver: Vec<(&String, semver::Version)> = versions
        .iter()
        .filter_map(|v| parse_version(v).map(|p| (v, p)))
        .collect();
    with_ver.sort_by(|a, b| a.1.cmp(&b.1));
    with_ver
        .last()
        .map(|(v, _)| v.to_string())
        .or_else(|| versions.last().map(|v| v.to_string()))
}

/// 取包含指定关键字的版本中最新者；无则 None
fn version_matching(versions: &[String], keyword: &str) -> Option<String> {
    let matched: Vec<String> = versions
        .iter()
        .filter(|v| v.to_lowercase().contains(keyword))
        .cloned()
        .collect();
    if matched.is_empty() { None } else { latest_of(&matched) }
}

/// 匹配部分版本前缀（22 / 22.0）的最新完整版本
fn resolve_partial(versions: &[String], prefix: &str) -> Option<String> {
    let matched: Vec<String> = versions
        .iter()
        .filter(|v| {
            v.as_str() == prefix
                || v.starts_with(&format!("{prefix}."))
                || (prefix.starts_with(v.as_str()) && !v.contains('.'))
        })
        .cloned()
        .collect();
    if matched.is_empty() { None } else { latest_of(&matched) }
}

/// 从插件 release 源获取可用版本列表（已应用 version_pattern 清洗）
async fn fetch_versions(plugin: &ToolPlugin) -> Result<Vec<String>, UError> {
    let raw: Vec<String> = match plugin.release.source.as_str() {
        "static" => plugin.release.versions.clone().unwrap_or_default(),
        "api" => {
            let response = HTTP_CLIENT.get(&plugin.release.url).await?;
            let text = response.text().await.map_err(|source| {
                UError::NetworkError { url: plugin.release.url.clone(), source }
            })?;
            extract_versions_from_api(
                &text,
                plugin.release.version_path.as_deref(),
            )?
        },
        other => {
            return Err(UError::SimpleError(format!(
                "unsupported release source '{other}'"
            )));
        },
    };
    Ok(apply_version_pattern(raw, plugin.release.version_pattern.as_deref()))
}

/// 将 version_pattern 应用到每个版本字符串；无 pattern 或匹配失败时保留原值
fn apply_version_pattern(
    versions: Vec<String>, pattern: Option<&str>,
) -> Vec<String> {
    let Some(p) = pattern else {
        return versions;
    };
    let Ok(re) = regex::Regex::new(p) else {
        return versions;
    };
    versions
        .into_iter()
        .filter_map(|s| {
            let caps = re.captures(&s)?;
            let v = caps
                .name("version")
                .or_else(|| caps.get(1))
                .or_else(|| caps.get(0))?
                .as_str()
                .trim();
            if v.is_empty() { None } else { Some(v.to_string()) }
        })
        .collect()
}

/// 从 API 响应文本中提取版本字符串列表
fn extract_versions_from_api(
    text: &str, version_path: Option<&str>,
) -> Result<Vec<String>, UError> {
    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|source| {
            UError::SimpleError(format!("invalid release response: {source}"))
        })?;
    let target = match version_path {
        Some(path) => navigate_json(&root, path).unwrap_or(&root),
        None => &root,
    };

    let mut raw: Vec<String> = Vec::new();
    match target {
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    raw.push(s.to_string());
                } else if let Some(s) = object_version_field(item) {
                    raw.push(s);
                }
            }
        },
        serde_json::Value::String(s) => raw.push(s.clone()),
        _ => {
            if let Some(s) = object_version_field(target) {
                raw.push(s);
            }
        },
    }
    Ok(raw)
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

/// 解析校验和配置（hash.enabled 时才启用）
async fn resolve_checksum(
    plugin: &ToolPlugin, bin: &InstallBin, vars: &HashMap<&str, &str>,
) -> Result<Option<Checksum>, UError> {
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
    let rendered = plugin.render(hash_path, vars);
    let response = HTTP_CLIENT.get(&rendered).await?;
    let text = response.text().await.map_err(|source| {
        UError::NetworkError { url: rendered.clone(), source }
    })?;
    let pattern = hash.pattern.as_deref().map(|p| plugin.render(p, vars));
    let expected = extract_hash(&text, pattern.as_deref())?;
    Ok(Some(Checksum { algorithm, expected }))
}

/// 从校验和文件中提取期望值：优先 pattern，否则匹配对应长度的 hex
fn extract_hash(text: &str, pattern: Option<&str>) -> Result<String, UError> {
    if let Some(p) = pattern {
        let re = regex::Regex::new(p).map_err(|_| {
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
    use sha2::{Digest, Sha256, Sha512};
    let mut file = fs::File::open(path).map_err(|source| {
        UError::FileError { path: path.to_path_buf(), source }
    })?;
    let digest: String = match algorithm {
        "sha256" => {
            let mut h = Sha256::new();
            std::io::copy(&mut file, &mut h)?;
            hex_encode(&h.finalize())
        },
        "sha512" => {
            let mut h = Sha512::new();
            std::io::copy(&mut file, &mut h)?;
            hex_encode(&h.finalize())
        },
        _ => {
            return Err(UError::SimpleError(format!(
                "unsupported checksum algorithm '{algorithm}'"
            )));
        },
    };
    Ok(digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 校验下载档案（未启用校验和时直接通过）
fn verify_checksum(plan: &InstallPlan) -> Result<(), UError> {
    if let Some(cs) = &plan.checksum {
        let actual = compute_checksum(&plan.archive_path, &cs.algorithm)?;
        if actual != cs.expected {
            return Err(UError::ChecksumError {
                message: format!(
                    "expected {}, got {} for {}",
                    cs.expected,
                    actual,
                    plan.archive_path.display()
                ),
            });
        }
    }
    Ok(())
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

    fn versions(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
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
    fn test_version_matching_lts() {
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
    }

    #[test]
    fn test_extract_versions_from_api_array_of_objects() {
        let json = r#"[
            {"version": "v20.11.0", "lts": "Iron"},
            {"version": "v20.12.0", "lts": "Iron"},
            {"version": "v22.0.0", "lts": false}
        ]"#;
        let out = extract_versions_from_api(json, None).unwrap();
        assert_eq!(out, vec!["v20.11.0", "v20.12.0", "v22.0.0"]);
    }

    #[test]
    fn test_extract_versions_from_api_with_path() {
        let json = r#"{"data": {"releases": ["1.2.3", "1.2.4"]}}"#;
        let out =
            extract_versions_from_api(json, Some("data.releases")).unwrap();
        assert_eq!(out, vec!["1.2.3", "1.2.4"]);
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
        let out = extract_hash(text, Some(r"(?P<hash>[0-9a-f]{64})")).unwrap();
        assert_eq!(out.len(), 64);
        // 无 pattern 时也应能识别 64 位 hex
        let out2 = extract_hash(text, None).unwrap();
        assert_eq!(out2.len(), 64);
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
            ArchiveRecord { downloaded_at: unix_now(), ttl_hours: 24 },
        );
        records.archives.insert(
            "expired.zip".into(),
            ArchiveRecord {
                downloaded_at: unix_now() - 25 * 3600,
                ttl_hours: 24,
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

        record_archive(&archive, 24);
        let first = load_records(&tool_dir)
            .archives
            .get("node-22.0.0-win-x64.zip")
            .cloned()
            .unwrap();
        assert_eq!(first.ttl_hours, 24);

        // 重复下载同一档案会刷新下载时间
        std::thread::sleep(Duration::from_secs(1));
        record_archive(&archive, 48);
        let second = load_records(&tool_dir)
            .archives
            .get("node-22.0.0-win-x64.zip")
            .cloned()
            .unwrap();
        assert_eq!(second.ttl_hours, 48);
        assert!(second.downloaded_at > first.downloaded_at);
    }
}
