use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::cli::plugin::PluginCommand::*;
use crate::core::config::GLOBAL_CONFIG;
use crate::core::error::UError;
use crate::core::http::HTTP_CLIENT;
use crate::core::http::HttpClient;
use crate::core::paths::{plugin_path, plugins_dir};
use crate::core::plugin::ToolPlugin;

const DEFAULT_REPO_URL: &str = "https://github.com/xxxyixuan/uvman-plugin";

#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct Plugin {
    #[clap(subcommand)]
    pub command: PluginCommand,
}

#[derive(Debug, clap::Subcommand)]
pub enum PluginCommand {
    ///  Install plugin
    #[clap(visible_aliases = ["i", "add"])]
    Install(InstallArgs),
    /// Uninstall plugin
    #[clap(visible_aliases = ["rm", "remove"])]
    Uninstall(UninstallArgs),
    /// List installed plugins
    #[clap(visible_alias = "ls")]
    List(ListArgs),

    /// Upgrade a plugin
    #[clap(visible_alias = "up")]
    Upgrade(UpgradeArgs),

    /// Show plugin information
    #[clap(visible_alias = "show")]
    Info(InfoArgs),

    /// Sync remote plugin index into local cache
    #[clap(visible_alias = "refresh")]
    Sync(SyncArgs),

    /// Scaffold a new plugin from a template
    #[clap(visible_alias = "new")]
    Create(CreateArgs),
}

impl Plugin {
    pub async fn run(self) -> crate::Result<()> {
        // 网络错误的代理提示由 UError::hint() 统一提供
        match self.command {
            Install(args) => args.run().await,
            Uninstall(args) => args.run().await,
            List(args) => args.run().await,
            Upgrade(args) => args.run().await,
            Info(args) => args.run().await,
            Sync(args) => args.run().await,
            Create(args) => args.run().await,
        }?;
        Ok(())
    }
}

#[derive(Debug, clap::Args)]
pub struct InstallArgs {
    /// The name of the plugin to install
    ///
    /// e.g.: make, node
    #[clap(value_parser = validate_plugin_name_arg)]
    pub plugin: String,

    /// URL of the plugin source
    #[clap(long, conflicts_with = "path")]
    pub url: Option<String>,

    /// Local path to the plugin source
    #[clap(long, conflicts_with = "url")]
    pub path: Option<String>,

    /// Reinstall even if plugin exists
    #[clap(long, short = 'f')]
    pub force: bool,
}

impl InstallArgs {
    pub async fn run(&self) -> Result<(), UError> {
        let plugin_dir = plugins_dir();
        if !plugin_dir.exists() {
            fs::create_dir_all(&plugin_dir).map_err(|source| {
                UError::FileError { path: plugin_dir.clone(), source }
            })?;
        }

        let target_path = plugin_path(&self.plugin);
        if target_path.exists() && !self.force {
            return Err(UError::PluginAlreadyExists {
                name: self.plugin.clone(),
            });
        }

        if let Some(local_path) = &self.path {
            install_from_local_path(local_path, &target_path)?;
        } else if let Some(url) = &self.url {
            install_from_url(url, &target_path).await?;
        } else {
            install_from_remote(&self.plugin, &target_path).await?;
        }

        println!("Plugin '{}' installed successfully.", self.plugin);
        Ok(())
    }
}

fn install_from_local_path(
    local_path: &str, target_path: &Path,
) -> Result<(), UError> {
    let local_path = Path::new(local_path);
    if !local_path.exists() {
        return Err(UError::PathNotFound { path: local_path.to_path_buf() });
    }
    if !local_path.is_file() {
        return Err(UError::NotAFile { path: local_path.to_path_buf() });
    }
    let content = fs::read_to_string(local_path).map_err(|source| {
        UError::FileError { path: local_path.to_path_buf(), source }
    })?;
    // 安装前校验 TOML 合法性，避免覆盖好的插件写入损坏文件
    let _ = parse_plugin_toml(&content, &name_from_target(target_path))?;
    fs::write(target_path, &content).map_err(|source| UError::FileError {
        path: target_path.to_path_buf(),
        source,
    })?;
    Ok(())
}

async fn install_from_url(url: &str, target_path: &Path) -> Result<(), UError> {
    let content = HTTP_CLIENT
        .fetch_text(
            url,
            GLOBAL_CONFIG.network.retries.unwrap_or(0),
            GLOBAL_CONFIG.network.retry_delay.unwrap_or(0),
        )
        .await?;
    // 安装前校验 TOML 合法性，避免写入损坏文件
    let _ = parse_plugin_toml(&content, &name_from_target(target_path))?;
    fs::write(target_path, &content).map_err(|source| UError::FileError {
        path: target_path.to_path_buf(),
        source,
    })?;
    Ok(())
}

async fn install_from_remote(
    plugin_name: &str, target_path: &Path,
) -> Result<(), UError> {
    let repo = repo_url();
    let url = raw_plugin_url(&repo, plugin_name)?;
    install_from_url(&url, target_path).await
}

fn name_from_target(target: &Path) -> String {
    target.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string()
}

#[derive(Debug, clap::Args)]
pub struct UninstallArgs {
    /// The name of the plugin to uninstall
    #[clap(value_parser = validate_plugin_name_arg)]
    pub plugin: String,

    /// Skip confirmation prompt
    #[clap(long, short = 'y')]
    pub yes: bool,
}

impl UninstallArgs {
    pub async fn run(&self) -> Result<(), UError> {
        let plugin_path = crate::core::paths::plugin_path(&self.plugin);

        if !plugin_path.exists() {
            return Err(UError::PluginNotInstalled {
                name: self.plugin.clone(),
                similar: did_you_mean_installed(&self.plugin),
            });
        }

        if !self.yes
            && !confirm(&format!("Remove plugin '{}'? [y/N] ", self.plugin))?
        {
            println!("Aborted.");
            return Ok(());
        }
        fs::remove_file(&plugin_path).map_err(|source| UError::FileError {
            path: plugin_path.clone(),
            source,
        })?;

        println!("Plugin '{}' uninstalled successfully.", self.plugin);

        Ok(())
    }
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// List plugins available in the remote repository
    #[clap(long)]
    remote: bool,

    /// Filter results by name substring
    #[clap(long, short = 'f')]
    filter: Option<String>,

    /// Output in JSON format (array of plugin names)
    #[clap(long)]
    json: bool,
}

impl ListArgs {
    pub async fn run(&self) -> Result<(), UError> {
        if self.remote {
            self.list_remote_plugins().await
        } else {
            self.list_local_plugins()
        }
    }

    fn matches_filter(&self, name: &str) -> bool {
        match &self.filter {
            Some(f) => name.to_lowercase().contains(&f.to_lowercase()),
            None => true,
        }
    }

    async fn list_remote_plugins(&self) -> Result<(), UError> {
        let cache_path = crate::core::paths::plugin_index_path();
        let mut names: Vec<String> = if cache_path.exists() {
            match load_plugin_index() {
                Ok(index) => {
                    if !self.json {
                        crate::ui::report::print_warning(
                            "using cached plugin index; run `uvman plugin sync` to refresh",
                        );
                    }
                    index.plugins
                },
                Err(e) => {
                    // 缓存损坏时回退远端获取；警告走 stderr，不污染 --json 的
                    // stdout
                    crate::ui::report::print_warning(&format!(
                        "cached plugin index is corrupted ({e}), falling back to remote"
                    ));
                    fetch_remote_plugin_names().await?
                },
            }
        } else {
            fetch_remote_plugin_names().await?
        };

        names.retain(|n| self.matches_filter(n));
        print_plugin_names(names, "Available plugins", self.json)
    }

    fn list_local_plugins(&self) -> Result<(), UError> {
        let mut names = list_installed_plugin_names()?;
        names.retain(|n| self.matches_filter(n));
        print_plugin_names(names, "Local plugins", self.json)
    }
}

#[derive(Debug, clap::Args)]
pub struct UpgradeArgs {
    /// The name of the plugin to upgrade
    ///
    /// e.g.: make, node
    #[clap(value_parser = validate_plugin_name_arg)]
    pub name: Option<String>,

    /// URL of the plugin source to upgrade from
    #[clap(long, conflicts_with = "path", conflicts_with = "all")]
    pub url: Option<String>,

    /// Local path to the plugin source to upgrade from
    #[clap(long, conflicts_with = "url", conflicts_with = "all")]
    pub path: Option<String>,

    /// Upgrade all installed plugins
    #[clap(
        long,
        conflicts_with = "name",
        conflicts_with = "url",
        conflicts_with = "path"
    )]
    pub all: bool,

    /// Show what would be upgraded without performing it
    #[clap(long)]
    pub dry_run: bool,

    /// Skip confirmation prompt
    #[clap(long, short = 'y')]
    pub yes: bool,
}

enum UpgradeOutcome {
    Upgraded,
    UpToDate,
}

enum VersionComparison {
    /// 新版本更高（或版本缺失/非 semver 无法比较，按用户升级意图继续）
    Upgradable,
    /// 版本相同，已是最新
    UpToDate { current: String },
    /// 新版本更低，阻止降级
    Downgrade { current: String, remote: String },
}

fn compare_versions(
    current: Option<&str>, new: Option<&str>,
) -> VersionComparison {
    match (current, new) {
        (Some(cur), Some(new)) => {
            match (semver::Version::parse(cur), semver::Version::parse(new)) {
                (Ok(c), Ok(n)) if n < c => VersionComparison::Downgrade {
                    current: cur.to_string(),
                    remote: new.to_string(),
                },
                (Ok(c), Ok(n)) if n == c => {
                    VersionComparison::UpToDate { current: cur.to_string() }
                },
                _ => VersionComparison::Upgradable,
            }
        },
        _ => VersionComparison::Upgradable,
    }
}

impl UpgradeArgs {
    pub async fn run(&self) -> Result<(), UError> {
        match (&self.name, self.all) {
            (Some(name), _) => {
                self.upgrade_one(name, !self.yes).await.map(|_| ())
            },
            (None, true) => self.upgrade_all().await,
            (None, false) => Err(UError::MissingPluginTarget),
        }
    }

    async fn upgrade_all(&self) -> Result<(), UError> {
        let names = list_installed_plugin_names()?;

        if names.is_empty() {
            println!("No plugins installed.");
            return Ok(());
        }

        if self.dry_run {
            println!("Plugins that would be upgraded:");
        }
        let mut upgraded = 0usize;
        let mut up_to_date = 0usize;
        let mut failed = Vec::new();
        for name in &names {
            if !self.dry_run {
                println!("Upgrading {} ...", name);
            }
            match self.upgrade_one(name, false).await {
                Ok(UpgradeOutcome::Upgraded) => upgraded += 1,
                Ok(UpgradeOutcome::UpToDate) => up_to_date += 1,
                Err(e) => {
                    crate::ui::report::print_error_message(&format!(
                        "failed to upgrade {name}: {e}"
                    ));
                    failed.push(name.clone());
                },
            }
        }
        println!(
            "Upgraded {} plugin(s), {} already up to date, {} failed.",
            upgraded,
            up_to_date,
            failed.len()
        );
        for name in &failed {
            eprintln!("- {}", name);
        }
        Ok(())
    }

    /// 升级单个插件：先取新版本内容并比较版本，仅更高版本才覆盖。
    async fn upgrade_one(
        &self, name: &str, do_confirm: bool,
    ) -> Result<UpgradeOutcome, UError> {
        let plugin_path = crate::core::paths::plugin_path(name);

        if !plugin_path.exists() {
            return Err(UError::PluginNotInstalled {
                name: name.to_string(),
                similar: did_you_mean_installed(name),
            });
        }

        // 先取新内容（不落盘），保证版本比较失败时不破坏已安装插件
        let new_content = self.fetch_new_content(name).await?;
        let new_plugin = parse_plugin_toml(&new_content, name)?;
        let new_version = new_plugin.tool.version.clone();
        let current_version = ToolPlugin::load_from(&plugin_path)
            .ok()
            .and_then(|p| p.tool.version);

        // 版本比较：阻止降级，跳过已最新；版本缺失或非 semver
        // 时按用户升级意图继续
        match compare_versions(
            current_version.as_deref(),
            new_version.as_deref(),
        ) {
            VersionComparison::Downgrade { current, remote } => {
                return Err(UError::PluginDowngrade {
                    name: name.to_string(),
                    current,
                    remote,
                });
            },
            VersionComparison::UpToDate { current } => {
                println!("Plugin '{name}' is already up to date ({current}).");
                return Ok(UpgradeOutcome::UpToDate);
            },
            VersionComparison::Upgradable => {},
        }

        let current = current_version.as_deref().unwrap_or("unknown");
        let new = new_version.as_deref().unwrap_or("unknown");

        if self.dry_run {
            println!("- {name}: {current} -> {new}");
            return Ok(UpgradeOutcome::Upgraded);
        }

        if do_confirm
            && !confirm(&format!(
                "Upgrade plugin '{name}' ({current} -> {new})? [y/N] "
            ))?
        {
            println!("Aborted.");
            return Ok(UpgradeOutcome::UpToDate);
        }

        fs::write(&plugin_path, &new_content).map_err(|source| {
            UError::FileError { path: plugin_path.clone(), source }
        })?;
        println!("Plugin '{name}' upgraded to {new}.");
        Ok(UpgradeOutcome::Upgraded)
    }

    /// 从本地路径 / URL / 远端仓库读取新版本插件内容（不落盘）
    async fn fetch_new_content(&self, name: &str) -> Result<String, UError> {
        if let Some(local_path) = &self.path {
            let src_path = Path::new(local_path);
            if !src_path.exists() {
                return Err(UError::PathNotFound {
                    path: src_path.to_path_buf(),
                });
            }
            if !src_path.is_file() {
                return Err(UError::NotAFile { path: src_path.to_path_buf() });
            }
            return fs::read_to_string(src_path).map_err(|source| {
                UError::FileError { path: src_path.to_path_buf(), source }
            });
        }
        let url = match &self.url {
            Some(url) => url.clone(),
            None => {
                let repo = repo_url();
                raw_plugin_url(&repo, name)?
            },
        };
        HTTP_CLIENT
            .fetch_text(
                &url,
                GLOBAL_CONFIG.network.retries.unwrap_or(0),
                GLOBAL_CONFIG.network.retry_delay.unwrap_or(0),
            )
            .await
    }
}

#[derive(Debug, clap::Args)]
pub struct InfoArgs {
    /// Name of the plugin to inspect
    #[clap(value_parser = validate_plugin_name_arg)]
    pub name: String,

    /// Query the remote registry instead of the local plugin
    ///
    /// e.g.: https://github.com/xxxyixuan/uvman-plugin
    #[clap(long)]
    pub registry: Option<String>,

    /// Output in JSON format
    #[clap(long)]
    pub json: bool,
}

#[derive(Debug, serde::Serialize)]
struct InfoOutput {
    name: String,
    /// 仅本地查询时给出（远端查询不涉及安装状态）
    #[serde(skip_serializing_if = "Option::is_none")]
    installed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

impl InfoArgs {
    pub async fn run(&self) -> Result<(), UError> {
        if let Some(repo) = &self.registry {
            return self.show_remote(repo).await;
        }
        self.show_local()
    }

    async fn show_remote(&self, repo: &str) -> Result<(), UError> {
        let url = Url::parse(repo).map_err(|_| {
            UError::SimpleError(format!("Invalid registry URL: {}", repo))
        })?;
        let download_url = raw_plugin_url(&url, &self.name)?;
        let content = HTTP_CLIENT
            .fetch_text(
                &download_url,
                GLOBAL_CONFIG.network.retries.unwrap_or(0),
                GLOBAL_CONFIG.network.retry_delay.unwrap_or(0),
            )
            .await?;
        let plugin = parse_plugin_toml(&content, &self.name)?;
        print_plugin_info(&plugin, None, self.json)
    }

    fn show_local(&self) -> Result<(), UError> {
        let plugin_path = crate::core::paths::plugin_path(&self.name);
        if plugin_path.exists() {
            let plugin = ToolPlugin::load_from(&plugin_path)?;
            print_plugin_info(&plugin, Some(true), self.json)
        } else {
            if self.json {
                let out = InfoOutput {
                    name: self.name.clone(),
                    installed: Some(false),
                    description: None,
                    author: None,
                    version: None,
                };
                let json = serde_json::to_string_pretty(&out)
                    .map_err(|source| UError::JsonError { source })?;
                println!("{}", json);
            } else {
                println!("Plugin '{}' is not installed.", self.name);
                crate::ui::report::print_hint(
                    "to install it, run:",
                    &[format!("uvman plugin install {}", self.name)],
                );
            }
            Ok(())
        }
    }
}

fn print_plugin_info(
    plugin: &ToolPlugin, installed: Option<bool>, json: bool,
) -> Result<(), UError> {
    if json {
        let out = InfoOutput {
            name: plugin.tool.name.clone(),
            installed,
            description: plugin.tool.description.clone(),
            author: plugin.tool.author.clone(),
            version: plugin.tool.version.clone(),
        };
        let json = serde_json::to_string_pretty(&out)
            .map_err(|source| UError::JsonError { source })?;
        println!("{}", json);
    } else {
        println!("Plugin: {}", plugin.tool.name);
        if let Some(desc) = &plugin.tool.description {
            println!("Description: {}", desc);
        }
        if let Some(authors) = &plugin.tool.author {
            println!("Author(s): {}", authors.join(", "));
        }
        if let Some(version) = &plugin.tool.version {
            println!("Version: {}", version);
        }
    }
    Ok(())
}

#[derive(Debug, clap::Args)]
pub struct SyncArgs {
    /// Proxy URL to use when fetching the remote index
    ///
    /// e.g.: http://127.0.0.1:7890
    #[clap(long)]
    pub proxy: Option<String>,
}

impl SyncArgs {
    pub async fn run(&self) -> Result<(), UError> {
        let url = repo_url();
        let (owner, repo) = extract_github_owner_repo(&url)
            .ok_or_else(|| UError::InvalidGitHubUrl { url: url.to_string() })?;
        let items = fetch_repository_contents_with_proxy(
            &owner,
            &repo,
            self.proxy.as_deref(),
        )
        .await?;

        let mut names = extract_plugin_names_from_items(&items);
        names.sort();
        names.dedup();

        let index = PluginIndex {
            repo: url.to_string(),
            fetched_at: now_epoch_secs(),
            plugins: names.clone(),
        };
        save_plugin_index(&index)?;

        println!(
            "Synced {} plugin(s) from {} to {}",
            names.len(),
            url,
            crate::core::paths::plugin_index_path().display()
        );
        for name in &names {
            println!("- {}", name);
        }
        Ok(())
    }
}

#[derive(Debug, clap::Args)]
pub struct CreateArgs {
    /// The name of the plugin to create
    ///
    /// e.g.: make, node
    pub name: String,

    /// Directory to write the plugin file into (default: current directory)
    #[clap(long, short = 'o')]
    pub output: Option<PathBuf>,
}

impl CreateArgs {
    pub async fn run(&self) -> Result<(), UError> {
        validate_plugin_name(&self.name)?;

        let output_dir =
            self.output.clone().unwrap_or_else(|| PathBuf::from("."));
        fs::create_dir_all(&output_dir).map_err(|source| {
            UError::FileError { path: output_dir.clone(), source }
        })?;

        let target = output_dir.join(format!("{}.toml", self.name));
        if target.exists() {
            return Err(UError::FileExists { path: target.clone() });
        }

        let content = load_template(&self.name).await;

        fs::write(&target, content).map_err(|source| UError::FileError {
            path: target.clone(),
            source,
        })?;

        println!("Plugin template created at {}", target.display());
        println!("Edit the file, then install it with:");
        println!(
            "  uvman plugin install {} --path {}",
            self.name,
            target.display()
        );
        Ok(())
    }
}

async fn load_template(name: &str) -> String {
    let url = match template_raw_url() {
        Ok(url) => url,
        Err(_) => return embedded_template(name),
    };
    if let Ok(response) = HTTP_CLIENT.get(&url).await
        && let Ok(text) = response.text().await
        // 模板必须含 {name} 占位符才能正确生成插件名
        && text.contains("{name}")
    {
        return text.replace("{name}", name);
    }
    embedded_template(name)
}

fn template_raw_url() -> Result<String, UError> {
    let repo = repo_url();
    let (owner, repo_name) = extract_github_owner_repo(&repo)
        .ok_or_else(|| UError::InvalidGitHubUrl { url: repo.to_string() })?;
    Ok(format!(
        "https://raw.githubusercontent.com/{owner}/{repo_name}/HEAD/templates/template.toml"
    ))
}

fn embedded_template(name: &str) -> String {
    format!(
        r#"# uvman plugin: {name}
# 完整字段说明见插件仓库 templates/template.toml

[tool]
name = "{name}"
description = ""
version = "0.0.1"
license = ""
# author = ["your-name <you@example.com>"]

[registry]
default = "https://download-base-url"
# mirrors = ["https://mirror-1", "https://mirror-2"]

[release]
source = "api"   # api | static
url = "https://api.example.com/releases"
# version_path = "json.path.to.version"   # source = "api" 时版本在响应中的取值路径
# version_pattern = "v(?P<version>.*)"    # 从 tag/文件名提取版本的正则
# source = "static"
# versions = ["0.0.1"]

[platform]
os_map = {{ windows = "win", linux = "linux", macos = "darwin" }}
arch_map = {{ x86_64 = "x64", aarch64 = "arm64" }}

[install.defaults]
version = "latest"
mode = "bin"

# ---------- 二进制发行版安装（默认） ----------
[[install.bin]]
os = ["windows", "linux", "macos"]
arch = ["x86_64", "aarch64"]

[install.bin.download]
path = "{{registry}}/{name}-{{version}}-{{os}}-{{arch}}.{{ext}}"
[install.bin.download.ext]
windows = "zip"
linux = "tar.gz"
macos = "tar.gz"
[install.bin.download.hash]
enabled = false
# algorithm = "sha256"
# path = "..."
# pattern = "..."

[install.bin.extract]
strip = 1

[install.bin.deploy]
bin_dir = "bin"
# copy_extra = ["LICENSE", "README.md"]
# [install.bin.deploy.post_install]
# windows = ["..."]

# ---------- 源码编译安装（按需启用：mode 改为 "src"） ----------
# [[install.src]]
# os = ["windows", "linux", "macos"]
# arch = ["x86_64", "aarch64"]
#
# [install.src.dependencies.tools]
# node = ["20.0.0"]
# [install.src.dependencies.system_libs]
# linux = ["build-essential"]
#
# [install.src.download]
# path = "{{registry}}/src-{name}-{{version}}.{{ext}}"
# [install.src.download.ext]
# windows = "zip"
# linux = "tar.gz"
# macos = "tar.gz"
# [install.src.download.hash]
# enabled = false
#
# [install.src.extract]
# strip = 1
#
# [install.src.build.env]
# CMAKE_BUILD_TYPE = "Release"
# [install.src.build.command]
# windows = ["cmake --build ."]
# linux = ["make"]
# macos = ["make"]
#
# [install.src.deploy]
# bin_dir = "bin"
# copy_extra = ["LICENSE", "README.md"]
# [install.src.deploy.post_install]
# windows = ["..."]
"#
    )
}

// ---------- 共享工具函数（被 ≥2 个命令使用） ----------

// 输出辅助
fn print_plugin_names(
    mut names: Vec<String>, title: &str, json: bool,
) -> Result<(), UError> {
    names.sort();
    names.dedup();
    if json {
        print_json_array(&names)
    } else {
        println!("{title}:");
        if names.is_empty() {
            println!("  (none)");
        } else {
            for name in names {
                println!("- {}", name);
            }
        }
        Ok(())
    }
}

fn print_json_array(names: &[String]) -> Result<(), UError> {
    let json = serde_json::to_string_pretty(names)
        .map_err(|source| UError::JsonError { source })?;
    println!("{}", json);
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool, UError> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

// 名称校验与已安装插件扫描
fn validate_plugin_name(name: &str) -> Result<(), UError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(UError::InvalidPluginName { name: name.to_string() });
    }
    Ok(())
}

fn validate_plugin_name_arg(s: &str) -> Result<String, String> {
    validate_plugin_name(s).map(|_| s.to_string()).map_err(|e| e.to_string())
}

fn list_installed_plugin_names() -> Result<Vec<String>, UError> {
    let plugin_dir = crate::core::paths::plugins_dir();
    if !plugin_dir.exists() {
        return Ok(Vec::new());
    }
    let entries = fs::read_dir(&plugin_dir).map_err(|source| {
        UError::FileError { path: plugin_dir.clone(), source }
    })?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && path.extension().and_then(|e| e.to_str()) == Some("toml")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            names.push(stem.to_string());
        }
    }
    names.sort();
    Ok(names)
}

fn did_you_mean_installed(name: &str) -> Vec<String> {
    let installed = list_installed_plugin_names().unwrap_or_default();
    crate::core::suggest::did_you_mean(name, &installed)
}

// 仓库与 URL 解析
fn repo_url() -> Url {
    match Url::parse(GLOBAL_CONFIG.plugin.repo.as_str()) {
        Ok(url) => url,
        Err(e) => {
            warn_repo_url_fallback(&e);
            Url::parse(DEFAULT_REPO_URL).expect("default URL is valid")
        },
    }
}

fn warn_repo_url_fallback(err: &url::ParseError) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        crate::ui::report::print_warning(&format!(
            "invalid plugin repo URL '{}' in config ({err}); using default",
            GLOBAL_CONFIG.plugin.repo
        ));
    }
}

fn raw_plugin_url(repo_url: &Url, name: &str) -> Result<String, UError> {
    let (owner, repo) =
        extract_github_owner_repo(repo_url).ok_or_else(|| {
            UError::InvalidGitHubUrl { url: repo_url.to_string() }
        })?;
    Ok(format!(
        "https://raw.githubusercontent.com/{owner}/{repo}/HEAD/{name}.toml"
    ))
}

fn parse_plugin_toml(content: &str, name: &str) -> Result<ToolPlugin, UError> {
    toml::from_str::<ToolPlugin>(content).map_err(|source| UError::TomlError {
        path: PathBuf::from(format!("{name}.toml")),
        source,
    })
}

// GitHub API
async fn fetch_repository_contents_with_proxy(
    owner: &str, repo: &str, proxy: Option<&str>,
) -> Result<Vec<serde_json::Value>, UError> {
    let api_url =
        format!("https://api.github.com/repos/{}/{}/contents", owner, repo);

    let client = HttpClient::with_proxy(30, proxy)?;
    // get 内部已校验状态码，失败时返回 NetworkError /
    // HttpStatusError（保留错误链）
    let response = client.get(&api_url).await?;

    let items: Vec<serde_json::Value> =
        response.json().await.map_err(|source| UError::NetworkError {
            url: api_url.clone(),
            source,
        })?;
    Ok(items)
}

async fn fetch_repository_contents(
    owner: &str, repo: &str,
) -> Result<Vec<serde_json::Value>, UError> {
    fetch_repository_contents_with_proxy(owner, repo, None).await
}

async fn fetch_remote_plugin_names() -> Result<Vec<String>, UError> {
    let url = repo_url();
    let (owner, repo) = extract_github_owner_repo(&url)
        .ok_or_else(|| UError::InvalidGitHubUrl { url: url.to_string() })?;
    let items = fetch_repository_contents(&owner, &repo).await?;
    Ok(extract_plugin_names_from_items(&items))
}

fn extract_plugin_names_from_items(items: &[serde_json::Value]) -> Vec<String> {
    items
        .iter()
        .filter(|item| is_toml_file(item))
        .filter_map(|item| {
            item.get("name")
                .and_then(serde_json::Value::as_str)
                .map(|n| n.trim_end_matches(".toml").to_string())
        })
        .collect()
}

fn extract_github_owner_repo(url: &Url) -> Option<(String, String)> {
    let host = url.host_str()?;
    if !(host == "github.com" || host == "www.github.com") {
        return None;
    }

    let segments: Vec<&str> = url.path_segments()?.collect();
    if segments.len() < 2 {
        return None;
    }

    let owner = segments[0].to_string();
    let repo = segments[1].trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

fn is_toml_file(item: &serde_json::Value) -> bool {
    let is_file =
        item.get("type").and_then(serde_json::Value::as_str) == Some("file");

    let is_toml = item
        .get("name")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|name| name.ends_with(".toml"));

    is_file && is_toml
}

// 插件索引缓存
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginIndex {
    repo: String,
    fetched_at: u64,
    plugins: Vec<String>,
}

fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_plugin_index() -> Result<PluginIndex, UError> {
    let path = crate::core::paths::plugin_index_path();
    let content = fs::read_to_string(&path)
        .map_err(|source| UError::FileError { path: path.clone(), source })?;
    serde_json::from_str(&content)
        .map_err(|source| UError::JsonError { source })
}

fn save_plugin_index(index: &PluginIndex) -> Result<(), UError> {
    let path = crate::core::paths::plugin_index_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|source| UError::FileError {
            path: dir.to_path_buf(),
            source,
        })?;
    }
    let content = serde_json::to_string_pretty(index)
        .map_err(|source| UError::JsonError { source })?;
    fs::write(&path, content)
        .map_err(|source| UError::FileError { path: path.clone(), source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_plugin_name() {
        assert!(validate_plugin_name("node").is_ok());
        assert!(validate_plugin_name("node-js").is_ok());
        assert!(validate_plugin_name("node_js").is_ok());
        assert!(validate_plugin_name("").is_err());
        assert!(validate_plugin_name("a/b").is_err());
        assert!(validate_plugin_name("a b").is_err());
        assert!(validate_plugin_name("a.b").is_err());
    }

    #[test]
    fn test_raw_plugin_url() {
        let url =
            Url::parse("https://github.com/xxxyixuan/uvman-plugin").unwrap();
        assert_eq!(
            raw_plugin_url(&url, "node").unwrap(),
            "https://raw.githubusercontent.com/xxxyixuan/uvman-plugin/HEAD/node.toml"
        );

        let with_git = Url::parse("https://www.github.com/a/b.git").unwrap();
        assert_eq!(
            raw_plugin_url(&with_git, "make").unwrap(),
            "https://raw.githubusercontent.com/a/b/HEAD/make.toml"
        );

        let non_github = Url::parse("https://gitlab.com/a/b").unwrap();
        assert!(raw_plugin_url(&non_github, "node").is_err());
    }

    #[test]
    fn test_matches_filter() {
        let args = ListArgs {
            remote: false,
            filter: Some("no".to_string()),
            json: false,
        };
        assert!(args.matches_filter("node"));
        assert!(!args.matches_filter("make"));

        let all = ListArgs { remote: false, filter: None, json: false };
        assert!(all.matches_filter("anything"));
    }

    #[test]
    fn test_embedded_template_renders() {
        let tpl = embedded_template("node");
        assert!(tpl.contains("name = \"node\""));
        assert!(tpl.contains("mode = \"bin\""));
        // 模板占位符已正确转义为字面量 {registry}/{version}
        assert!(tpl.contains("{registry}/node-{version}-{os}-{arch}.{ext}"));
        // 内置模板必须可被解析为合法插件
        parse_plugin_toml(&tpl, "node").expect("template must be valid TOML");
    }

    #[test]
    fn test_template_raw_url() {
        // 测试环境 GLOBAL_CONFIG 的 plugin.repo 为默认仓库
        let url = template_raw_url().unwrap();
        assert_eq!(
            url,
            "https://raw.githubusercontent.com/xxxyixuan/uvman-plugin/HEAD/templates/template.toml"
        );
    }

    #[test]
    fn test_plugin_index_roundtrip() {
        let index = PluginIndex {
            repo: "https://github.com/x/y".to_string(),
            fetched_at: 123,
            plugins: vec!["make".to_string(), "node".to_string()],
        };
        save_plugin_index(&index).unwrap();
        let loaded = load_plugin_index().unwrap();
        assert_eq!(loaded.repo, index.repo);
        assert_eq!(loaded.fetched_at, index.fetched_at);
        assert_eq!(loaded.plugins, index.plugins);

        fs::remove_file(crate::core::paths::plugin_index_path()).unwrap();
    }

    #[test]
    fn test_validate_plugin_name_arg() {
        assert_eq!(validate_plugin_name_arg("node").unwrap(), "node");
        assert!(validate_plugin_name_arg("../evil").is_err());
        assert!(validate_plugin_name_arg("a b").is_err());
    }

    #[test]
    fn test_compare_versions() {
        // 新版本更高 → 可升级
        assert!(matches!(
            compare_versions(Some("1.0.0"), Some("1.1.0")),
            VersionComparison::Upgradable
        ));
        // 版本相同 → 已最新
        assert!(matches!(
            compare_versions(Some("1.0.0"), Some("1.0.0")),
            VersionComparison::UpToDate { .. }
        ));
        // 新版本更低 → 阻止降级
        assert!(matches!(
            compare_versions(Some("1.1.0"), Some("1.0.0")),
            VersionComparison::Downgrade { .. }
        ));
        // 任一版本缺失或非 semver → 按升级意图继续
        assert!(matches!(
            compare_versions(None, Some("1.0.0")),
            VersionComparison::Upgradable
        ));
        assert!(matches!(
            compare_versions(Some("1.0.0"), None),
            VersionComparison::Upgradable
        ));
        assert!(matches!(
            compare_versions(Some("not-a-version"), Some("1.0.0")),
            VersionComparison::Upgradable
        ));
    }

    #[test]
    fn test_extract_plugin_names_from_items() {
        let items = vec![
            serde_json::json!({"type": "file", "name": "node.toml"}),
            serde_json::json!({"type": "dir", "name": "src"}),
            serde_json::json!({"type": "file", "name": "README.md"}),
            serde_json::json!({"type": "file", "name": "make.toml"}),
        ];
        let names = extract_plugin_names_from_items(&items);
        assert_eq!(names, vec!["node".to_string(), "make".to_string()]);
    }

    #[test]
    fn test_list_installed_plugin_names_missing_dir() {
        // 目录不存在时应返回空列表而非报错（升级/列表场景容错）
        assert!(list_installed_plugin_names().is_ok());
    }
}
