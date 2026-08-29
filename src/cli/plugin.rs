use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::core::config::GLOBAL_CONFIG;
use crate::core::error::UError;
use crate::core::http::HTTP_CLIENT;
use crate::core::paths::{plugin_path, plugins_dir};
use crate::core::plugin::ToolPlugin;

const DEFAULT_REPO_URL: &str = "https://github.com/xxxyixuan/uvman-plugin";

#[derive(Debug, clap::Args)]
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

    /// Show plugin information
    #[clap(visible_alias = "show")]
    Info(InfoArgs),
}

impl Plugin {
    pub async fn run(self) -> crate::Result<()> {
        // The `?` converts UError into an eyre Report; network-error proxy hints
        // are unified in UError::hint()
        match self.command {
            PluginCommand::Install(args) => args.run().await?,
            PluginCommand::Uninstall(args) => args.run().await?,
            PluginCommand::List(args) => args.run().await?,
            PluginCommand::Info(args) => args.run().await?,
        }
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
        // create_dir_all is idempotent; no exists() check needed
        fs::create_dir_all(&plugin_dir)
            .map_err(|source| UError::FileError { path: plugin_dir.clone(), source })?;

        let target_path = plugin_path(&self.plugin);
        if target_path.exists() && !self.force {
            return Err(UError::PluginAlreadyExists { name: self.plugin.clone() });
        }

        let source = PluginSource::from_opts(self.url.as_deref(), self.path.as_deref());
        let content = fetch_plugin_content(source, &self.plugin).await?;
        // Validate the TOML before writing, so a working plugin is never
        // overwritten with a broken file
        let _ = parse_plugin_toml(&content, &self.plugin)?;
        fs::write(&target_path, &content)
            .map_err(|source| UError::FileError { path: target_path.clone(), source })?;

        println!("Plugin '{}' installed successfully.", self.plugin);
        Ok(())
    }
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

        if !self.yes && !confirm(&format!("Remove plugin '{}'? [y/N] ", self.plugin))? {
            println!("Aborted.");
            return Ok(());
        }
        fs::remove_file(&plugin_path)
            .map_err(|source| UError::FileError { path: plugin_path.clone(), source })?;

        println!("Plugin '{}' uninstalled successfully.", self.plugin);

        Ok(())
    }
}

#[derive(Debug, clap::Args)]
pub struct ListArgs {
    /// List plugins available in the remote repository
    ///
    /// Served through a local index cache (`cache/plugins.json`) that refreshes
    /// itself transparently: fresh cache is used silently, stale cache triggers
    /// a background refetch, and a failed refetch falls back to the stale
    /// cache.
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
        if self.remote { self.list_remote_plugins().await } else { self.list_local_plugins() }
    }

    fn matches_filter(&self, name: &str) -> bool {
        match &self.filter {
            Some(f) => name.to_lowercase().contains(&f.to_lowercase()),
            None => true,
        }
    }

    async fn list_remote_plugins(&self) -> Result<(), UError> {
        let repo = repo_url().to_string();
        let mut names = resolve_remote_plugin_names(&repo).await?;
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

#[derive(Debug, Default, serde::Serialize)]
struct InfoOutput {
    name: String,
    /// present in local queries only (remote queries don't cover install state)
    #[serde(skip_serializing_if = "Option::is_none")]
    installed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
}

impl From<&ToolPlugin> for InfoOutput {
    fn from(plugin: &ToolPlugin) -> Self {
        Self {
            name: plugin.tool.name.clone(),
            description: plugin.tool.description.clone(),
            author: plugin.tool.author.clone(),
            version: plugin.tool.version.clone(),
            installed: None,
        }
    }
}

impl InfoArgs {
    pub async fn run(&self) -> Result<(), UError> {
        if let Some(repo) = &self.registry {
            return self.show_remote(repo).await;
        }
        self.show_local()
    }

    async fn show_remote(&self, repo: &str) -> Result<(), UError> {
        let url = Url::parse(repo)
            .map_err(|_| UError::SimpleError(format!("Invalid registry URL: {}", repo)))?;
        let download_url = raw_plugin_url(&url, &self.name)?;
        let content = fetch_text_with_config(&download_url).await?;
        let plugin = parse_plugin_toml(&content, &self.name)?;
        print_plugin_info(&plugin, None, self.json)
    }

    fn show_local(&self) -> Result<(), UError> {
        let path = plugin_path(&self.name);
        if !path.exists() {
            return self.print_not_installed();
        }
        let plugin = ToolPlugin::load_from(&path)?;
        print_plugin_info(&plugin, Some(true), self.json)
    }

    fn print_not_installed(&self) -> Result<(), UError> {
        if self.json {
            print_info_json(&InfoOutput {
                name: self.name.clone(),
                installed: Some(false),
                ..InfoOutput::default()
            })
        } else {
            println!("Plugin '{}' is not installed.", self.name);
            crate::ui::report::print_hint(
                "to install it, run:",
                &[format!("uvman plugin install {}", self.name)],
            );
            Ok(())
        }
    }
}

/// Where a plugin's new content comes from (CLI `--url` / `--path` / default
/// remote)
#[derive(Debug)]
enum PluginSource<'a> {
    Local(&'a Path),
    Url(&'a str),
    /// The default remote plugin repo (from global config)
    Remote,
}

impl<'a> PluginSource<'a> {
    fn from_opts(url: Option<&'a str>, path: Option<&'a str>) -> Self {
        if let Some(path) = path {
            Self::Local(Path::new(path))
        } else if let Some(url) = url {
            Self::Url(url)
        } else {
            Self::Remote
        }
    }
}

/// Fetch the plugin TOML content from the given source without persisting it;
/// install validates it before writing and upgrade compares versions first
async fn fetch_plugin_content(source: PluginSource<'_>, name: &str) -> Result<String, UError> {
    match source {
        PluginSource::Local(path) => {
            if !path.exists() {
                return Err(UError::PathNotFound { path: path.to_path_buf() });
            }
            if !path.is_file() {
                return Err(UError::NotAFile { path: path.to_path_buf() });
            }
            fs::read_to_string(path)
                .map_err(|source| UError::FileError { path: path.to_path_buf(), source })
        },
        PluginSource::Url(url) => fetch_text_with_config(url).await,
        PluginSource::Remote => {
            let url = raw_plugin_url(&repo_url(), name)?;
            fetch_text_with_config(&url).await
        },
    }
}

/// fetch_text with the global `[network]` retry policy
async fn fetch_text_with_config(url: &str) -> Result<String, UError> {
    HTTP_CLIENT.fetch_text(url, network_retries(), network_retry_delay()).await
}

fn network_retries() -> u64 {
    GLOBAL_CONFIG.network.retries.unwrap_or(0)
}

fn network_retry_delay() -> u64 {
    GLOBAL_CONFIG.network.retry_delay.unwrap_or(0)
}

fn print_plugin_info(
    plugin: &ToolPlugin, installed: Option<bool>, json: bool,
) -> Result<(), UError> {
    if json {
        let mut out = InfoOutput::from(plugin);
        out.installed = installed;
        print_info_json(&out)
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
        Ok(())
    }
}

fn print_info_json(out: &InfoOutput) -> Result<(), UError> {
    let json = serde_json::to_string_pretty(out).map_err(|source| UError::JsonError { source })?;
    println!("{json}");
    Ok(())
}

fn print_plugin_names(mut names: Vec<String>, title: &str, json: bool) -> Result<(), UError> {
    names.sort();
    names.dedup();
    if json {
        print_names_json(&names)
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

fn print_names_json(names: &[String]) -> Result<(), UError> {
    let json =
        serde_json::to_string_pretty(names).map_err(|source| UError::JsonError { source })?;
    println!("{json}");
    Ok(())
}

fn confirm(prompt: &str) -> Result<bool, UError> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

fn validate_plugin_name(name: &str) -> Result<(), UError> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
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
    let entries = fs::read_dir(&plugin_dir)
        .map_err(|source| UError::FileError { path: plugin_dir.clone(), source })?;
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
    let (owner, repo) = extract_github_owner_repo(repo_url)
        .ok_or_else(|| UError::InvalidGitHubUrl { url: repo_url.to_string() })?;
    Ok(format!("https://raw.githubusercontent.com/{owner}/{repo}/HEAD/{name}.toml"))
}

fn parse_plugin_toml(content: &str, name: &str) -> Result<ToolPlugin, UError> {
    toml::from_str::<ToolPlugin>(content)
        .map_err(|source| UError::TomlError { path: PathBuf::from(format!("{name}.toml")), source })
}

const DEFAULT_INDEX_TTL_HOURS: u64 = 24;

/// Plugin index freshness window from `[cache] ttl` (hours); ttl = 0 disables
/// index caching entirely (every `list --remote` hits the network)
fn plugin_index_ttl_secs() -> Option<u64> {
    match GLOBAL_CONFIG.cache.ttl.unwrap_or(DEFAULT_INDEX_TTL_HOURS) {
        0 => None,
        hours => Some(hours * 3600),
    }
}

/// Resolve remote plugin names through the seamless index cache:
/// fresh cache → silent hit; stale/missing/repo-changed → refetch and persist;
/// refetch failure → fall back to the cached index (warned) when one exists.
async fn resolve_remote_plugin_names(repo: &str) -> Result<Vec<String>, UError> {
    if let Some(ttl) = plugin_index_ttl_secs()
        && let Ok(index) = load_plugin_index()
        && index.repo == repo
        && now_epoch_secs().saturating_sub(index.fetched_at) < ttl
    {
        return Ok(index.plugins);
    }

    match fetch_remote_plugin_names().await {
        Ok(names) => {
            // A failed cache write must not block listing
            if plugin_index_ttl_secs().is_some()
                && let Err(e) = save_plugin_index(&PluginIndex {
                    repo: repo.to_string(),
                    fetched_at: now_epoch_secs(),
                    plugins: names.clone(),
                })
            {
                crate::ui::report::print_warning(&format!(
                    "failed to save the plugin index cache: {e}"
                ));
            }
            Ok(names)
        },
        // Seamless degradation: a stale cache still beats a hard error
        Err(e) => match load_plugin_index() {
            Ok(index) if index.repo == repo => {
                crate::ui::report::print_warning(&format!(
                    "plugin index refresh failed ({e}); using the cached index"
                ));
                Ok(index.plugins)
            },
            _ => Err(e),
        },
    }
}

async fn fetch_remote_plugin_names() -> Result<Vec<String>, UError> {
    let url = repo_url();
    let (owner, repo) = extract_github_owner_repo(&url)
        .ok_or_else(|| UError::InvalidGitHubUrl { url: url.to_string() })?;
    let api_url = format!("https://api.github.com/repos/{owner}/{repo}/contents");
    // get() already validates the status code; HTTP_CLIENT carries the
    // [plugin].proxy / [network] timeout so the index honors global config
    let response = HTTP_CLIENT.get(&api_url).await?;
    let items: Vec<serde_json::Value> = response
        .json()
        .await
        .map_err(|source| UError::NetworkError { url: api_url.clone(), source })?;
    Ok(extract_plugin_names_from_items(&items))
}

fn extract_plugin_names_from_items(items: &[serde_json::Value]) -> Vec<String> {
    items
        .iter()
        .filter(|item| is_toml_file(item))
        .filter_map(|item| {
            item.get("name")
                .and_then(serde_json::Value::as_str)
                // strip exactly one ".toml" suffix ("a.toml.toml" stays distinct)
                .and_then(|n| n.strip_suffix(".toml"))
                .map(str::to_string)
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
    let is_file = item.get("type").and_then(serde_json::Value::as_str) == Some("file");

    let is_toml = item
        .get("name")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|name| name.ends_with(".toml"));

    is_file && is_toml
}

// Plugin index cache
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PluginIndex {
    repo: String,
    fetched_at: u64,
    plugins: Vec<String>,
}

fn now_epoch_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load_plugin_index() -> Result<PluginIndex, UError> {
    let path = crate::core::paths::plugin_index_path();
    let content = fs::read_to_string(&path)
        .map_err(|source| UError::FileError { path: path.clone(), source })?;
    serde_json::from_str(&content).map_err(|source| UError::JsonError { source })
}

fn save_plugin_index(index: &PluginIndex) -> Result<(), UError> {
    let path = crate::core::paths::plugin_index_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)
            .map_err(|source| UError::FileError { path: dir.to_path_buf(), source })?;
    }
    let content =
        serde_json::to_string_pretty(index).map_err(|source| UError::JsonError { source })?;
    fs::write(&path, content).map_err(|source| UError::FileError { path: path.clone(), source })
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

    #[tokio::test]
    async fn test_fetch_plugin_content_local() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("node.toml");
        std::fs::write(&file, "fake-content").unwrap();

        let content = fetch_plugin_content(PluginSource::Local(&file), "node").await.unwrap();
        assert_eq!(content, "fake-content");

        let missing = dir.path().join("missing.toml");
        let err = fetch_plugin_content(PluginSource::Local(&missing), "node").await.unwrap_err();
        assert!(matches!(err, UError::PathNotFound { .. }));
    }

    #[test]
    fn test_raw_plugin_url() {
        let url = Url::parse("https://github.com/xxxyixuan/uvman-plugin").unwrap();
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
        let args = ListArgs { remote: false, filter: Some("no".to_string()), json: false };
        assert!(args.matches_filter("node"));
        assert!(!args.matches_filter("make"));

        let all = ListArgs { remote: false, filter: None, json: false };
        assert!(all.matches_filter("anything"));
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
        // A missing dir should yield an empty list, not an error (tolerant paths
        // like upgrade/list)
        assert!(list_installed_plugin_names().is_ok());
    }
}
