//! Tool installation orchestration layer.
//!
//! Resolves "plugin TOML + version request" into an executable `InstallPlan`,
//! then consumes it in the order download → verify → extract → deploy. Pure
//! data lives in `InstallPlan` for easy per-stage testing and core reuse.

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
use crate::core::{paths, platform};

/// Default cache TTL: 24 hours
const DEFAULT_CACHE_TTL_HOURS: u64 = 24;

/// Tool archive cache record file name (under cache/tools/<tool>/)
const RECORDS_FILE: &str = "records.json";

/// All atomic information needed for one install (pure data, consumed stepwise
/// by execute).
///
/// Construction (`plan`) does no network IO: checksum fetching is deferred to
/// execution, so cache-hit / already-installed checks stay fully offline.
pub struct InstallPlan {
    pub name: String,
    /// Resolved concrete version (x.y.z), not an alias
    pub version: String,
    /// Candidate download URLs (placeholders resolved): global mirror → plugin
    /// mirrors → plugin default, tried in order until one succeeds
    pub urls: Vec<String>,
    /// Archive extension for the platform (e.g. zip / tar.gz)
    pub ext: String,
    /// Top-level directories to strip on extraction
    pub strip: u32,
    /// Bin directory relative to the extraction root
    pub bin_dir: String,
    /// Checksum fetch plan (URLs rendered; fetched over the network only after
    /// download at execution)
    pub hash: Option<HashPlan>,
    /// Where the downloaded archive is saved (under cache)
    pub archive_path: PathBuf,
    /// Final install directory (tools/<name>/<version>)
    pub install_dir: PathBuf,
}

/// Checksum fetch plan: candidate URLs + algorithm + parse rules (pure data)
pub struct HashPlan {
    /// Candidate checksum file URLs (same order as archive sources)
    pub urls: Vec<String>,
    /// Hash algorithm (defaults to sha256)
    pub algorithm: String,
    /// Rendered extraction pattern
    pub pattern: Option<String>,
    /// Official archive file name (used for line matching when no pattern)
    pub filename: String,
}

/// A validated hex digest (sha256: 64 / sha512: 128 lowercase hex chars).
///
/// The only public constructor is [`HexDigest::parse`], so a `HexDigest` in
/// scope is well-formed by type: comparison and persistence never re-validate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HexDigest(String);

impl HexDigest {
    /// Validate and normalize (lowercase) a raw digest string.
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let normalized = raw.trim().to_lowercase();
        (matches!(normalized.len(), 64 | 128)
            && normalized.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(Self(normalized))
    }

    /// Wrap a digest that is already well-formed.
    ///
    /// Valid by construction: callers pass either the output of `hex_encode`
    /// over a sha2 finalize (always 64/128 lowercase hex) or a token captured by
    /// a regex anchoring exactly `{64,128}` hex chars.
    fn from_lowercase_hex(raw: &str) -> Self {
        debug_assert!(Self::parse(raw).is_some(), "digest must be pre-validated: {raw}");
        Self(raw.to_lowercase())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HexDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Build an install plan from the plugin and version request (performs no
/// writes)
pub async fn plan(name: &str, version: Option<&str>) -> Result<InstallPlan, UError> {
    let plugin = ToolPlugin::load_from(&paths::plugin_path(name))
        .map_err(|_| UError::PluginNotInstalled { name: name.to_string(), similar: vec![] })?;

    let sys_os = platform::OS.as_str();
    let bin = select_bin(&plugin, sys_os)?;

    let version = resolve_version(&plugin, name, version).await?;
    let (os, arch) = plugin.resolve_platform()?;
    let ext = bin.download.ext.get(sys_os).cloned().unwrap_or_else(|| "zip".to_string());

    let mut vars: HashMap<&str, &str> = HashMap::new();
    vars.insert("version", version.as_str());
    vars.insert("os", os.as_str());
    vars.insert("arch", arch.as_str());
    vars.insert("ext", ext.as_str());

    // Candidate sources: global [registry] mirror (priority) → plugin mirrors →
    // plugin default; render a download URL per source
    let sources = merged_sources(name, &plugin);
    let urls: Vec<String> = sources
        .iter()
        .map(|source| {
            vars.insert("registry", source.as_str());
            plugin.render(&bin.download.path, &vars)
        })
        .collect();

    // {filename} comes from the URL basename (matches the official artifact),
    // not the local cache name — checksum files record lines by official name.
    let url_filename = urls.first().map(|u| url_basename(u)).unwrap_or_default();
    vars.insert("filename", url_filename.as_str());

    // Only build the fetch plan (render URLs); defer the network fetch to execution
    let hash = hash_plan(&plugin, bin, &mut vars)?;

    let archive_path =
        paths::cache_tools_dir().join(name).join(format!("{name}-{version}-{os}-{arch}.{ext}"));
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

/// Merge candidate download sources: global `[registry]` (by tool name, highest
/// priority) → plugin mirrors → plugin default; keep only the first occurrence
/// of duplicates
fn merged_sources(name: &str, plugin: &ToolPlugin) -> Vec<String> {
    let mut sources: Vec<String> = GLOBAL_CONFIG
        .registry
        .get(name)
        .map(|entry| entry.items().into_iter().cloned().collect())
        .unwrap_or_default();
    for s in plugin.registry.sources() {
        // Candidate lists are a handful of entries, so a linear scan beats hashing
        if !sources.contains(&s) {
            sources.push(s);
        }
    }
    sources
}

/// Run download (or cache reuse), verify, extract, and deploy in sequence.
///
/// Verification: after download, fetch the official checksum over the network
/// and write the algorithm/expected value into records.json; cache hits then
/// use local verification (offline-capable, and self-heal by re-downloading on
/// tamper/corruption).
pub async fn execute(plan: &InstallPlan) -> Result<(), UError> {
    // Lazy GC: migrate legacy layout and clean expired cache (best-effort; failures
    // don't block the install)
    let ttl = cache_ttl();
    gc_cache_at(&paths::cache_dir(), ttl);

    let from_cache = archive_cache_hit(plan);
    if from_cache {
        // Local verify failed (tampered/corrupted): drop the cached archive and
        // re-download to self-heal
        if verify_recorded_hash(plan).is_err() {
            let _ = fs::remove_file(&plan.archive_path);
            download_and_verify(plan, ttl).await?;
        } else {
            crate::ui::report::print_hint(
                &format!(
                    "using cached archive `{}`",
                    plan.archive_path.file_name().and_then(|n| n.to_str()).unwrap_or("archive")
                ),
                &[],
            );
        }
    } else {
        download_and_verify(plan, ttl).await?;
    }

    install_stages(plan)?;

    // ttl = 0: keep no cache; delete the archive right after a successful install
    if ttl == 0 {
        let _ = fs::remove_file(&plan.archive_path);
    }
    Ok(())
}

/// Download the archive and, when hash.enabled, verify against the remote
/// checksum; record the cache entry on success, including the expected value
/// for later local verification on cache hits.
async fn download_and_verify(plan: &InstallPlan, ttl: u64) -> Result<(), UError> {
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
            record_archive(&plan.archive_path, ttl, Some((&hp.algorithm, &expected)));
        }
    } else if ttl > 0 {
        record_archive(&plan.archive_path, ttl, None);
    }
    Ok(())
}

/// Download the archive trying candidate URLs in order (mirrors then default);
/// each URL still uses the configured network retries, erroring only when all
/// fail
async fn download_archive(plan: &InstallPlan) -> Result<(), UError> {
    let retries = GLOBAL_CONFIG.network.retries.unwrap_or(0);
    let retry_delay = GLOBAL_CONFIG.network.retry_delay.unwrap_or(0);

    let mut last_err = None;
    for url in &plan.urls {
        match HTTP_CLIENT.download_to(url, &plan.archive_path, retries, retry_delay).await {
            Ok(_) => return Ok(()),
            Err(e) => {
                crate::ui::report::print_warning(&format!(
                    "download source failed, trying next: {e}"
                ));
                last_err = Some(e);
            },
        }
    }
    Err(last_err.unwrap_or_else(|| {
        UError::SimpleError("no download source available for this tool".into())
    }))
}

/// Whether the cached archive is reusable: file exists, non-empty, and within
/// the recorded TTL
fn archive_cache_hit(plan: &InstallPlan) -> bool {
    if !plan.archive_path.is_file()
        || fs::metadata(&plan.archive_path).map(|m| m.len() == 0).unwrap_or(true)
    {
        return false;
    }
    let Some(tool_dir) = plan.archive_path.parent() else {
        return false;
    };
    let Some(name) = plan.archive_path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    match load_records(tool_dir).archives.get(name) {
        // No record: treat as reusable (orphans are handled by GC's mtime fallback)
        None => true,
        Some(record) => record.downloaded_at + record.ttl_hours * 3600 > unix_now(),
    }
}

/// Three install stages (verify → extract → deploy), each advanced by its own
/// spinner. Verification is local (value recorded in records.json); it makes no
/// network request.
fn install_stages(plan: &InstallPlan) -> Result<(), UError> {
    let archive_name = plan.archive_path.file_name().and_then(|n| n.to_str()).unwrap_or("archive");

    run_stage(format!("verifying {archive_name}"), || verify_recorded_hash(plan))?;

    let extract_dir = run_stage("extracting archive".to_string(), || {
        // Extract into a one-shot temp dir: TempDir auto-cleans on drop, covering both
        // success and failure paths, and concurrent installs don't interfere
        let extract_dir = tempfile::tempdir()
            .map_err(|source| UError::FileError { path: std::env::temp_dir(), source })?;
        extract_archive(&plan.archive_path, extract_dir.path(), &plan.ext, plan.strip)?;
        Ok(extract_dir)
    })?;

    run_stage(format!("deploying {}@{}", plan.name, plan.version), || {
        fs::create_dir_all(&plan.install_dir)
            .map_err(|source| UError::FileError { path: plan.install_dir.clone(), source })?;
        copy_bin(extract_dir.path(), &plan.bin_dir, &plan.install_dir)
    })?;

    Ok(())
}

/// Execute one install stage: a spinner shows an in-progress message; on
/// success the line is finalized green `✔ <msg>`, on failure red `✖ <msg>` and
/// the error bubbles up
fn run_stage<T>(msg: String, f: impl FnOnce() -> Result<T, UError>) -> Result<T, UError> {
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

/// Redraw the stage line on completion: drop the spinner and colorize by result
/// mark
fn finish_stage(pb: &ProgressBar, ok: bool, msg: &str) {
    crate::ui::progress::finish_marked(pb, ok, msg);
}

/// Single-stage spinner; hidden in quiet mode.
/// The steady tick keeps the spinner rotating during synchronous blocking.
fn stage_spinner() -> Option<ProgressBar> {
    if crate::ui::report::quiet() {
        return None;
    }
    let pb = ProgressBar::new_spinner();
    pb.set_style(crate::ui::progress::spinner_style("{spinner:.green} {msg}"));
    pb.enable_steady_tick(Duration::from_millis(120));
    Some(pb)
}

/// Read the configured cache TTL (hours); defaults to 24 when unset
fn cache_ttl() -> u64 {
    GLOBAL_CONFIG.cache.ttl.unwrap_or(DEFAULT_CACHE_TTL_HOURS)
}

/// Cache record for a single downloaded archive
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArchiveRecord {
    /// Download completion time (unix seconds)
    downloaded_at: u64,
    /// TTL in effect at download time (hours)
    ttl_hours: u64,
    /// Hash algorithm verified at download (e.g. sha256); absent when
    /// verification is disabled
    #[serde(default, skip_serializing_if = "Option::is_none")]
    algorithm: Option<String>,
    /// Expected hash verified at download; absent when verification is
    /// disabled. Used for local verification on cache hits to guard against
    /// tampering/corruption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
}

/// Contents of cache/tools/<tool>/records.json: archive file name → cache
/// record
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

/// Record (or refresh) an archive's download time, TTL, and checksum
fn record_archive(archive: &Path, ttl_hours: u64, hash: Option<(&str, &HexDigest)>) {
    let Some(tool_dir) = archive.parent() else { return };
    let Some(name) = archive.file_name().and_then(OsStr::to_str) else {
        return;
    };
    let (algorithm, expected) = match hash {
        Some((algorithm, expected)) => (Some(algorithm.to_string()), Some(expected.as_str().to_string())),
        None => (None, None),
    };
    let mut records = load_records(tool_dir);
    records.archives.insert(
        name.to_string(),
        ArchiveRecord { downloaded_at: unix_now(), ttl_hours, algorithm, hash: expected },
    );
    save_records(tool_dir, &records);
}

/// Verify an archive locally against its cache record (guards
/// tamper/corruption); skipped when there's no record (orphan) or no stored
/// checksum
fn verify_recorded_hash(plan: &InstallPlan) -> Result<(), UError> {
    let Some(tool_dir) = plan.archive_path.parent() else {
        return Ok(());
    };
    let Some(name) = plan.archive_path.file_name().and_then(OsStr::to_str) else {
        return Ok(());
    };
    let records = load_records(tool_dir);
    let Some(record) = records.archives.get(name) else {
        return Ok(());
    };
    let (Some(algorithm), Some(expected)) = (&record.algorithm, &record.hash) else {
        return Ok(());
    };
    // A stored hash that fails validation can never match; report a mismatch so
    // the caller drops the archive and re-downloads (self-heal)
    let Some(expected) = HexDigest::parse(expected) else {
        return Err(UError::ChecksumError {
            message: format!(
                "cached record holds an invalid digest '{expected}' for {}",
                plan.archive_path.display()
            ),
        });
    };
    let actual = compute_checksum(&plan.archive_path, algorithm)?;
    if actual != expected {
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
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Directories under cache/ that aren't "tool download cache" (new-layout
/// partitions / other caches) and so are excluded from legacy layout migration
const NON_TOOL_CACHE_DIRS: [&str; 3] = ["tools", "versions", "builds"];

/// Lazily clean the download cache (best-effort):
/// - `ttl = 0`: keep nothing; clear legacy dirs and the whole `cache/tools/`
/// - `ttl > 0`:
///   1. Migrate archives from legacy `cache/<tool>/` to `cache/tools/<tool>/`
///      (mtime backs the download-time record); drop leftover `extract/`
///      contents
///   2. Expire by records.json `downloaded_at + ttl_hours`; orphans without a
///      record fall back to mtime + current TTL
/// - Loose files at the cache root (e.g. plugins.json) aren't download cache;
///   untouched
fn gc_cache_at(cache: &Path, ttl: u64) {
    if ttl == 0 {
        // ttl=0 clears only the "tool archive cache": legacy tool dirs and
        // cache/tools/; other partitions like versions/builds stay untouched
        // (same protection as migration)
        if let Ok(entries) = fs::read_dir(cache) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() && entry.file_name() != *"tools" && !is_non_tool_cache_dir(&entry)
                {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        }
        let _ = fs::remove_dir_all(cache.join("tools"));
        return;
    }

    // Legacy layout migration
    if let Ok(entries) = fs::read_dir(cache) {
        for entry in entries.flatten() {
            let legacy = entry.path();
            if !legacy.is_dir() || is_non_tool_cache_dir(&entry) {
                continue;
            }
            migrate_legacy_tool_dir(cache, &legacy, ttl);
        }
    }

    // New layout: clean by records
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

/// Whether a cache-root dir is a non-tool cache dir (tools/versions/builds)
fn is_non_tool_cache_dir(entry: &fs::DirEntry) -> bool {
    NON_TOOL_CACHE_DIRS.contains(&entry.file_name().to_string_lossy().as_ref())
}

/// Migrate archives from legacy cache/<tool>/ to cache/tools/<tool>/, then
/// remove the legacy dir
fn migrate_legacy_tool_dir(cache: &Path, legacy: &Path, default_ttl: u64) {
    // Drop leftover extraction dirs from the old layout
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
        // Target exists (repeated migration): drop the old file, keep the newer record
        if to.exists() || fs::rename(&from, &to).is_ok() {
            let _ = fs::remove_file(&from);
            // Backfill a record: approximate download time with mtime, expire by current
            // TTL
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
                    // Legacy archives weren't checksummed; algorithm/hash left default
                    algorithm: None,
                    hash: None,
                },
            );
            save_records(&target, &records);
        }
    }
    let _ = fs::remove_dir(legacy);
}

/// Remove expired archives under cache/tools/<tool>/ by record and prune
/// records
fn gc_tool_cache(tool_dir: &Path, default_ttl: u64) {
    let mut records = load_records(tool_dir);
    let now = SystemTime::now();
    let mut changed = false;

    // Record-driven: drop entries that are expired or whose archive is gone
    records.archives.retain(|name, record| {
        let path = tool_dir.join(name);
        let expires =
            UNIX_EPOCH + Duration::from_secs(record.downloaded_at + record.ttl_hours * 3600);
        if !path.exists() || now >= expires {
            // Missing archives need no removal; the no-op error is ignored
            let _ = fs::remove_file(&path);
            changed = true;
            return false;
        }
        true
    });

    // Orphans (no record): fall back to mtime + current TTL
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

/// Select the bin install entry matching the current OS
fn select_bin<'a>(plugin: &'a ToolPlugin, sys_os: &str) -> Result<&'a InstallBin, UError> {
    let sys_os = sys_os.to_string();
    let sys_arch = platform::ARCH.as_str().to_string();
    plugin
        .install
        .bin
        .as_deref()
        .and_then(|bins| {
            bins.iter().find(|b| {
                b.os.iter().any(|o| o == &sys_os) && b.arch.iter().any(|a| a == &sys_arch)
            })
        })
        .ok_or_else(|| UError::PlatformNotSupported { os: sys_os, arch: sys_arch })
}

/// Remote version entry: a version plus release-line metadata.
///
/// latest/lts/stable/nightly are query semantics, not stored fields:
/// - `latest` → greatest semver in the whole set
/// - `lts` → greatest with `lts` metadata (e.g. node index.json `"lts":
///   "Iron"`)
/// - `stable` → semver without prerelease (decided by `parse_version`; not
///   stored)
/// - `nightly` → self-described by the prerelease segment (e.g.
///   `22.0.0-nightly20260101`)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteVersion {
    pub version: String,
    /// LTS codename; None means not an LTS (Current / prerelease)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lts: Option<String>,
}

/// Resolve a version request (the part after `@` in tool_spec) to a concrete
/// version:
/// - `20.11.0`: exact match against full semver
/// - `22` / `22.19`: partial version, newest x.y.z matching the prefix
/// - `latest`: latest stable (that version itself)
/// - `lts`: latest with LTS metadata
/// - `nightly`: latest whose version contains nightly
/// - default: use `install.defaults.version` then the above rules (node
///   defaults to latest)
async fn resolve_version(
    plugin: &ToolPlugin, name: &str, version: Option<&str>,
) -> Result<String, UError> {
    let request = version
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| plugin.install.defaults.version.clone());
    let request = request.trim();

    // Exact full version is used as-is
    if semver::Version::parse(request).is_ok() {
        return Ok(request.to_string());
    }

    let versions = remote_versions_of(plugin, name).await?;

    // Partial version (e.g. 22 / 22.19) → latest matching the prefix
    if is_partial_version(request) {
        return resolve_partial(versions.iter(), request).ok_or_else(|| UError::VersionNotFound {
            tool: name.to_string(),
            version: request.to_string(),
        });
    }

    // Aliases don't fall back: error explicitly on no match, avoiding silently
    // installing the wrong line
    let resolved = match request {
        "latest" => latest_of(versions.iter()),
        // Prefer metadata (api source); fall back to string naming convention (static source, e.g.
        // "22.1.0-lts")
        "lts" => lts_of(versions.iter()).or_else(|| version_matching(versions.iter(), "lts")),
        "nightly" => version_matching(versions.iter(), "nightly"),
        _ => None,
    };
    resolved.ok_or_else(|| UError::VersionNotFound {
        tool: name.to_string(),
        version: request.to_string(),
    })
}

/// Resolve a version request to a concrete "locally installed" version (the
/// `use` source).
///
/// Semantics align with [`resolve_version`] (remote set), but matching is
/// limited to version dirs actually present under `tools/<name>/`:
/// - `20.11.0`: exact version, must be installed
/// - `22` / `22.19`: partial, latest installed matching the prefix
/// - `latest` / default: newest installed
/// - `lts`: filter the installed set by remote metadata (api source via version
///   cache; static source falls back to string naming convention)
pub async fn resolve_installed_version(
    name: &str, request: Option<&str>,
) -> Result<String, UError> {
    let plugin = ToolPlugin::load_from(&paths::plugin_path(name)).map_err(|_| {
        UError::PluginNotInstalled { name: name.to_string(), similar: did_you_mean_installed(name) }
    })?;

    let installed = installed_versions(name);
    if installed.is_empty() {
        return Err(UError::SimpleError(format!("no local version of '{name}' is installed")));
    }
    let installed_rv: Vec<RemoteVersion> =
        installed.iter().map(|v| RemoteVersion { version: v.clone(), lts: None }).collect();

    let request = request.unwrap_or("latest");
    let not_found =
        || UError::VersionNotFound { tool: name.to_string(), version: request.to_string() };

    // Full semver: must be installed; error if missing (avoid silently switching to
    // a close version)
    if semver::Version::parse(request).is_ok() {
        return installed.iter().find(|v| v.as_str() == request).cloned().ok_or_else(not_found);
    }

    if is_partial_version(request) {
        return resolve_partial(installed_rv.iter(), request).ok_or_else(not_found);
    }

    let resolved = match request {
        "latest" => latest_of(installed_rv.iter()),
        // No local metadata for the installed set; filter via remote cache, then fall back to
        // string naming
        "lts" => {
            let remote = remote_versions_of(&plugin, name).await?;
            let lts_installed: Vec<RemoteVersion> = remote
                .into_iter()
                .filter(|v| v.lts.is_some() && installed.contains(&v.version))
                .collect();
            lts_of(lts_installed.iter()).or_else(|| version_matching(installed_rv.iter(), "lts"))
        },
        "nightly" => version_matching(installed_rv.iter(), "nightly"),
        _ => None,
    };
    resolved.ok_or_else(not_found)
}

/// Collect the locally installed versions of a tool (dir names, semver
/// ascending). Returns an empty list when the tool dir is missing (caller
/// decides how to error)
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
            && let Some(name) = e.file_name().to_str()
        {
            versions.push(name.to_string());
        }
    }
    // semver ascending (newest last); fall back to string order when unparseable
    versions.sort_by(|a, b| match (parse_version(a), parse_version(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => a.cmp(b),
    });
    versions
}

/// Whether this is a partial version like `22` / `22.0` (only digits and dots)
fn is_partial_version(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.')
}

/// Parse a concrete version (must be x.y.z); None on failure
fn parse_version(s: &str) -> Option<semver::Version> {
    semver::Version::parse(s.trim_start_matches(['v', 'V'])).ok()
}

/// Latest by semver; falls back to the last item when none parse.
///
/// Single lazy pass over the candidates: `max_by` instead of collect+sort, no
/// intermediate Vec. `max_by` keeps the last of equal maxima, matching the
/// previous stable-sort-then-take-last semantics.
fn latest_of<'a>(versions: impl Iterator<Item = &'a RemoteVersion> + Clone) -> Option<String> {
    versions
        .clone()
        .filter_map(|v| parse_version(&v.version).map(|p| (v, p)))
        .max_by(|a, b| a.1.cmp(&b.1))
        .map(|(v, _)| v.version.clone())
        .or_else(|| versions.last().map(|v| v.version.clone()))
}

/// Newest version with LTS metadata; None if none
fn lts_of<'a>(versions: impl Iterator<Item = &'a RemoteVersion> + Clone) -> Option<String> {
    latest_of(versions.filter(|v| v.lts.is_some()))
}

/// Newest entry whose version contains the keyword; None if none
fn version_matching<'a>(
    versions: impl Iterator<Item = &'a RemoteVersion> + Clone, keyword: &str,
) -> Option<String> {
    latest_of(versions.filter(|v| v.version.to_lowercase().contains(keyword)))
}

/// Newest full version matching a partial prefix (22 / 22.19).
/// The third condition only allows the request to continue past the version
/// (e.g. request 2.0 matches version 2), preventing request 22 from matching
/// version 2
fn resolve_partial<'a>(
    versions: impl Iterator<Item = &'a RemoteVersion> + Clone, prefix: &str,
) -> Option<String> {
    // Hoisted out of the per-item filter: one allocation, not n
    let dotted = format!("{prefix}.");
    latest_of(versions.filter(move |v| {
        v.version.as_str() == prefix
            || v.version.starts_with(&dotted)
            // `strip_prefix` expresses "the request continues past this version"
            // without a per-item format! allocation
            || prefix.strip_prefix(v.version.as_str()).is_some_and(|rest| rest.starts_with('.'))
    }))
}

/// Call the api source and parse version entries (version_path locate +
/// metadata preserve + pattern clean)
async fn fetch_api_versions(
    url: &str, version_path: Option<&str>, version_pattern: Option<&str>,
) -> Result<Vec<RemoteVersion>, UError> {
    let response = HTTP_CLIENT.get(url).await?;
    let text = response
        .text()
        .await
        .map_err(|source| UError::NetworkError { url: url.to_string(), source })?;
    let raw = extract_versions_from_api(&text, version_path)?;
    Ok(apply_version_pattern(raw, version_pattern))
}

/// Fetch a tool's published remote versions (single source for install
/// resolution and `uvman list <tool> --remote`).
///
/// - `static` source: return the fixed list defined by the plugin; no local
///   caching
/// - `api` source: read a non-expired cache in `cache/versions/` first; fetch
///   over the network and write back when missing or expired. The cache file
///   name embeds its expiry (unix seconds, hex):
///   `{tool}_remote_version_{expires_at}.json`
pub async fn remote_versions(name: &str) -> Result<Vec<RemoteVersion>, UError> {
    let plugin = ToolPlugin::load_from(&paths::plugin_path(name)).map_err(|_| {
        UError::PluginNotInstalled { name: name.to_string(), similar: did_you_mean_installed(name) }
    })?;
    remote_versions_of(&plugin, name).await
}

/// Fetch remote versions from an already-loaded plugin (reused by install to
/// avoid a second plugin read)
async fn remote_versions_of(plugin: &ToolPlugin, name: &str) -> Result<Vec<RemoteVersion>, UError> {
    match &plugin.release {
        Release::Static { versions } => {
            Ok(versions.iter().map(|v| RemoteVersion { version: v.clone(), lts: None }).collect())
        },
        Release::Api { url, version_path, version_pattern } => {
            let dir = paths::cache_versions_dir();
            if let Some(cached) = load_cached_versions(&dir, name) {
                return Ok(cached);
            }
            let versions =
                fetch_api_versions(url, version_path.as_deref(), version_pattern.as_deref())
                    .await?;
            save_cached_versions(&dir, name, &versions);
            Ok(versions)
        },
    }
}

/// Collect similarly-spelled names from installed plugins (did-you-mean
/// suggestions)
fn did_you_mean_installed(name: &str) -> Vec<String> {
    let installed: Vec<String> = std::fs::read_dir(paths::plugins_dir())
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(OsStr::to_str) == Some("toml"))
                .filter_map(|e| e.path().file_stem().and_then(OsStr::to_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    did_you_mean(name, &installed)
}

/// Remote-version cache file name: `{tool}_remote_version_{expiry-hex}.json`
fn version_cache_file(tool: &str, expires_at: u64) -> String {
    format!("{tool}_remote_version_{expires_at:x}.json")
}

/// Parse the expiry from a cache file name; None when not this tool's cache or
/// malformed
fn parse_cache_expiry(tool: &str, file_name: &str) -> Option<u64> {
    let hex = file_name.strip_suffix(".json")?.strip_prefix(&format!("{tool}_remote_version_"))?;
    u64::from_str_radix(hex, 16).ok()
}

/// Read a non-expired remote-version cache; opportunistically clean this tool's
/// expired cache files. Legacy plain-string-array caches fail to deserialize
/// and are treated as no cache, upgrading naturally on the next fetch.
fn load_cached_versions(dir: &Path, tool: &str) -> Option<Vec<RemoteVersion>> {
    // Single pass: drop expired files inline, keep the longest-lived unexpired
    // cache (ties resolve to the first entry, as the previous loop did)
    let (_, path) = fs::read_dir(dir).ok()?.flatten().filter_map(|entry| {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        match parse_cache_expiry(tool, &file_name) {
            Some(expires_at) if expires_at <= unix_now() => {
                let _ = fs::remove_file(entry.path());
                None
            },
            Some(expires_at) => Some((expires_at, entry.path())),
            None => None,
        }
    })
    .min_by_key(|(expires_at, _)| std::cmp::Reverse(*expires_at))?;
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Write the remote-version cache (best-effort; failures don't affect query
/// results).
///
/// Expiry = now + TTL (from `cache.ttl`, default 24h), encoded as hex in the
/// file name; `ttl = 0` means no caching.
fn save_cached_versions(dir: &Path, tool: &str, versions: &[RemoteVersion]) {
    let ttl = cache_ttl();
    if ttl == 0 {
        return;
    }
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    // Overwrite: clean this tool's old caches first to avoid accumulating files
    if let Ok(entries) = fs::read_dir(dir) {
        entries.flatten().for_each(|entry| {
            if parse_cache_expiry(tool, &entry.file_name().to_string_lossy()).is_some() {
                let _ = fs::remove_file(entry.path());
            }
        });
    }
    let expires_at = unix_now().saturating_add(ttl * 3600);
    let path = dir.join(version_cache_file(tool, expires_at));
    if let Ok(json) = serde_json::to_vec_pretty(versions) {
        let _ = fs::write(path, json);
    }
}

/// Apply version_pattern to each entry's version; keep the original when no
/// pattern or no match
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

/// Extract version entries (version + lts metadata) from an API response body
fn extract_versions_from_api(
    text: &str, version_path: Option<&str>,
) -> Result<Vec<RemoteVersion>, UError> {
    let root: serde_json::Value = serde_json::from_str(text)
        .map_err(|source| UError::SimpleError(format!("invalid release response: {source}")))?;
    let target = match version_path {
        Some(path) => navigate_json(&root, path).unwrap_or(&root),
        None => &root,
    };

    let raw: Vec<RemoteVersion> = match target {
        serde_json::Value::Array(items) => items.iter().filter_map(version_entry).collect(),
        _ => version_entry(target).into_iter().collect(),
    };
    Ok(raw)
}

/// One API entry → a version entry: a plain string carries no metadata; an
/// object takes its version field and preserves lts metadata; other shapes are
/// skipped
fn version_entry(value: &serde_json::Value) -> Option<RemoteVersion> {
    match value {
        serde_json::Value::String(s) => Some(RemoteVersion { version: s.clone(), lts: None }),
        serde_json::Value::Object(_) => {
            Some(RemoteVersion { version: object_version_field(value)?, lts: object_lts_field(value) })
        },
        _ => None,
    }
}

/// Take the LTS codename from an object: only a string value counts as an LTS
/// line (node index.json uses boolean `false` for non-LTS; `as_str` naturally
/// returns None)
fn object_lts_field(v: &serde_json::Value) -> Option<String> {
    v.as_object()?.get("lts")?.as_str().map(str::to_string)
}

/// Take the version field from an object (version / tag_name / name)
fn object_version_field(v: &serde_json::Value) -> Option<String> {
    let obj = v.as_object()?;
    for key in ["version", "tag_name", "name"] {
        if let Some(s) = obj.get(key).and_then(serde_json::Value::as_str) {
            return Some(s.to_string());
        }
    }
    None
}

/// Navigate JSON by a dot-separated path (e.g. `data.versions`)
fn navigate_json<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
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

/// Build a checksum fetch plan (render candidate URLs and pattern; no network).
/// Fetching is deferred to execution (after download) to keep the plan phase at
/// zero network IO
fn hash_plan(
    plugin: &ToolPlugin, bin: &InstallBin, vars: &mut HashMap<&str, &str>,
) -> Result<Option<HashPlan>, UError> {
    let hash = &bin.download.hash;
    if !hash.enabled {
        return Ok(None);
    }
    let algorithm = hash.algorithm.as_deref().unwrap_or("sha256").to_string();
    let hash_path = hash.path.as_ref().ok_or_else(|| {
        UError::SimpleError("hash is enabled but `download.hash.path` is missing".into())
    })?;

    // The checksum file shares the archive's source layout: render on each
    // candidate source. The per-iteration `source` reference isn't put into
    // vars (cloned into a local copy) to avoid leaking the borrow's lifetime.
    let urls: Vec<String> = merged_sources(&plugin.tool.name, plugin)
        .iter()
        .map(|source| {
            let mut scoped = vars.clone();
            scoped.insert("registry", source.as_str());
            plugin.render(hash_path, &scoped)
        })
        .collect();

    let pattern = hash.pattern.as_deref().map(|p| plugin.render(p, vars));
    let filename = vars.get("filename").map(|s| s.to_string()).unwrap_or_default();

    Ok(Some(HashPlan { urls, algorithm, pattern, filename }))
}

/// Fetch and parse the expected checksum trying candidate sources in order
/// (mirrors → default); errors only when all fail
async fn fetch_expected_hash(hp: &HashPlan) -> Result<HexDigest, UError> {
    let mut last_err = None;
    for url in &hp.urls {
        match HTTP_CLIENT.get(url).await {
            Ok(response) => match response.text().await {
                Ok(text) => {
                    return extract_hash(&text, hp.pattern.as_deref(), &hp.filename);
                },
                Err(source) => {
                    let e = UError::NetworkError { url: url.clone(), source };
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
    Err(last_err.unwrap_or_else(|| {
        UError::SimpleError("no download source available for checksum file".into())
    }))
}

/// Extract the expected checksum from the file: pattern first; without a
/// pattern, prefer the line of the archive's official name (official checksum
/// files list many files, so blindly taking the first hex could grab another
/// platform's hash), finally falling back to the first hex in the file
fn extract_hash(text: &str, pattern: Option<&str>, filename: &str) -> Result<HexDigest, UError> {
    if let Some(p) = pattern {
        // Checksum files are multi-line lists (one file per line), so ^$ must anchor by
        // line; enabled automatically when the user's pattern doesn't set (?m)
        // explicitly
        let re = regex::RegexBuilder::new(p)
            .multi_line(true)
            .build()
            .map_err(|_| UError::SimpleError(format!("invalid checksum pattern '{p}'")))?;
        if let Some(caps) = re.captures(text)
            && let Some(g) = caps.name("hash").or_else(|| caps.get(1)).or_else(|| caps.get(0))
        {
            // A pattern capture that is not a digest could never match the computed
            // hash; failing here reports the bad pattern instead of a confusing
            // guaranteed-later mismatch
            return HexDigest::parse(g.as_str()).ok_or_else(|| {
                UError::ChecksumError { message: format!("pattern matched invalid digest '{}'", g.as_str().trim()) }
            });
        }
        return Err(UError::ChecksumError { message: "hash not found in checksum file".into() });
    }

    // No pattern: match this archive's line as `<hash> [ *]<filename>`;
    // end of line isn't anchored to tolerate a trailing CRLF \r
    if !filename.is_empty() {
        for len in [64usize, 128] {
            // `regex::escape` guarantees the filename cannot break the pattern,
            // but build failures degrade to the next strategy instead of panicking
            let Ok(re) = regex::Regex::new(&format!(
                r"(?im)^[0-9a-f]{{{len}}}\s+\*?{}",
                regex::escape(filename)
            )) else {
                continue;
            };
            if let Some(m) = re.find(text) {
                // The pattern anchors `{len}` hex chars at line start, so the token
                // is a well-formed digest by construction
                let hash = m.as_str().split_whitespace().next().unwrap_or("");
                return Ok(HexDigest::from_lowercase_hex(hash));
            }
        }
    }

    // Final fallback: first 64/128-bit hex in the file
    for len in [64usize, 128] {
        let Ok(re) = regex::Regex::new(&format!(r"(?i)\b[0-9a-f]{{{len}}}\b")) else {
            continue;
        };
        if let Some(m) = re.find(text) {
            // The pattern matches exactly `{len}` hex chars, well-formed by
            // construction
            return Ok(HexDigest::from_lowercase_hex(m.as_str()));
        }
    }
    Err(UError::ChecksumError { message: "no hex checksum found in checksum file".into() })
}

/// Compute the archive checksum
pub(crate) fn compute_checksum(path: &Path, algorithm: &str) -> Result<HexDigest, UError> {
    use sha2::{Sha256, Sha512};
    let file = fs::File::open(path)
        .map_err(|source| UError::FileError { path: path.to_path_buf(), source })?;
    // Use chunked update instead of io::copy: hasher's io::Write impl relies on
    // sha2's std feature, which may be unavailable after dependency-tree pruning.
    match algorithm {
        "sha256" => hash_file::<Sha256>(file),
        "sha512" => hash_file::<Sha512>(file),
        _ => Err(UError::SimpleError(format!(
            "unsupported checksum algorithm '{algorithm}'"
        ))),
    }
}

/// Read the file in chunks updating the hash; return the hex digest
fn hash_file<D: sha2::Digest>(mut file: fs::File) -> Result<HexDigest, UError> {
    use std::io::Read;
    let mut hasher = D::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|source| UError::IoError { source })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(HexDigest::from_lowercase_hex(&hex_encode(&hasher.finalize())))
}

/// Lowercase hex encoding; a preallocated buffer avoids per-byte allocations
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    bytes.iter().for_each(|b| {
        // fmt::Write for String is infallible
        let _ = write!(out, "{b:02x}");
    });
    out
}

/// Last path segment of a URL (official artifact name); falls back to the whole
/// string if no separator
fn url_basename(url: &str) -> String {
    url.rsplit('/').find(|s| !s.is_empty()).unwrap_or(url).to_string()
}

/// Extract the archive to dest by extension, stripping the first `strip`
/// top-level dirs
pub(crate) fn extract_archive(archive: &Path, dest: &Path, ext: &str, strip: u32) -> Result<(), UError> {
    match ext {
        "zip" => extract_zip(archive, dest, strip),
        "tar.gz" | "tgz" => extract_tar_gz(archive, dest, strip),
        other => Err(UError::SimpleError(format!("unsupported archive extension '{other}'"))),
    }
}

fn extract_zip(archive: &Path, dest: &Path, strip: u32) -> Result<(), UError> {
    let file = fs::File::open(archive)
        .map_err(|source| UError::FileError { path: archive.to_path_buf(), source })?;
    let mut zip = zip::ZipArchive::new(file).map_err(|source| UError::ExtractError {
        path: archive.to_path_buf(),
        source: Box::new(source),
    })?;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|source| UError::ExtractError {
            path: archive.to_path_buf(),
            source: Box::new(source),
        })?;
        let name = entry.name().to_string();
        let out_path = stripped_path(dest, &name, strip)?;
        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .map_err(|source| UError::FileError { path: out_path.clone(), source })?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| UError::FileError { path: parent.to_path_buf(), source })?;
        }
        let mut out = fs::File::create(&out_path)
            .map_err(|source| UError::FileError { path: out_path.clone(), source })?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|source| UError::FileError { path: out_path.clone(), source })?;
    }
    Ok(())
}

fn extract_tar_gz(archive: &Path, dest: &Path, strip: u32) -> Result<(), UError> {
    let file = fs::File::open(archive)
        .map_err(|source| UError::FileError { path: archive.to_path_buf(), source })?;
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
            fs::create_dir_all(&out_path)
                .map_err(|source| UError::FileError { path: out_path.clone(), source })?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| UError::FileError { path: parent.to_path_buf(), source })?;
        }
        if entry_type.is_file() {
            let mut out = fs::File::create(&out_path)
                .map_err(|source| UError::FileError { path: out_path.clone(), source })?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|source| UError::FileError { path: out_path.clone(), source })?;
        }
    }
    Ok(())
}

/// Safely join an in-archive path under dest, stripping the first `strip`
/// top-level components and filtering any components that would escape
fn stripped_path(dest: &Path, name: &str, strip: u32) -> Result<PathBuf, UError> {
    let mut comps: Vec<String> = Path::new(name)
        .components()
        .filter_map(|c| match c {
            Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if comps.is_empty() {
        return Err(UError::SimpleError(format!("archive entry '{name}' has no valid path")));
    }
    let skip = (strip as usize).min(comps.len());
    comps.drain(0..skip);
    Ok(comps.iter().fold(dest.to_path_buf(), |acc, c| acc.join(c)))
}

/// Copy the bin subdirectory contents from the extraction root into the install
/// dir
fn copy_bin(extract_dir: &Path, bin_dir: &str, install_dir: &Path) -> Result<(), UError> {
    let src = extract_dir.join(bin_dir);
    if !src.exists() {
        return Err(UError::SimpleError(format!(
            "bin dir '{}' not found in extracted archive",
            src.display()
        )));
    }
    copy_dir_contents(&src, install_dir)
}

/// Recursively copy directory contents
fn copy_dir_contents(src: &Path, dest: &Path) -> Result<(), UError> {
    for entry in
        fs::read_dir(src).map_err(|source| UError::FileError { path: src.to_path_buf(), source })?
    {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            fs::create_dir_all(&to)
                .map_err(|source| UError::FileError { path: to.clone(), source })?;
            copy_dir_contents(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .map_err(|source| UError::FileError { path: from.clone(), source })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn versions(list: &[&str]) -> Vec<RemoteVersion> {
        list.iter().map(|s| RemoteVersion { version: s.to_string(), lts: None }).collect()
    }

    #[test]
    fn test_installed_versions_scans_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("node");
        fs::create_dir_all(tool.join("22.19.0")).unwrap();
        fs::create_dir_all(tool.join("22.9.1")).unwrap();
        fs::create_dir_all(tool.join("10.0.0")).unwrap();
        // Non-dir entries (stray cache files) don't enter the version list
        fs::write(tool.join("records.json"), "{}").unwrap();

        let v = installed_versions_in(&tool);
        assert_eq!(v, vec!["10.0.0", "22.9.1", "22.19.0"]);
        // Missing dir → empty list
        assert!(installed_versions_in(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn test_latest_of() {
        let v = versions(&["1.0.0", "2.10.0", "2.9.0", "0.5.0"]);
        assert_eq!(latest_of(v.iter()).as_deref(), Some("2.10.0"));
    }

    #[test]
    fn test_latest_of_with_prefix() {
        // v-prefixed versions parse and compare too
        let v = versions(&["20.11.0", "20.12.0", "v20.13.0"]);
        assert_eq!(latest_of(v.iter()).as_deref(), Some("v20.13.0"));
    }

    #[test]
    fn test_lts_of_uses_metadata() {
        // lts metadata wins: newest is Current, but the newest LTS is 22.12.0
        let v = [
            RemoteVersion { version: "26.7.0".into(), lts: None },
            RemoteVersion { version: "22.12.0".into(), lts: Some("Jod".into()) },
            RemoteVersion { version: "20.11.0".into(), lts: Some("Iron".into()) },
        ];
        assert_eq!(lts_of(v.iter()).as_deref(), Some("22.12.0"));
        // No LTS metadata at all → None (no fallback to latest)
        assert_eq!(lts_of(versions(&["1.0.0", "2.0.0"]).iter()), None);
    }

    #[test]
    fn test_version_matching_keyword() {
        // static-source naming-convention fallback: version string contains the keyword
        let v = versions(&["22.0.0", "22.1.0-lts", "22.2.0-lts.1"]);
        assert_eq!(version_matching(v.iter(), "lts").as_deref(), Some("22.2.0-lts.1"));
        assert_eq!(version_matching(v.iter(), "nightly"), None);
    }

    #[test]
    fn test_resolve_partial_major() {
        let v = versions(&["22.0.0", "22.11.0", "20.11.0"]);
        assert_eq!(resolve_partial(v.iter(), "22").as_deref(), Some("22.11.0"));
        assert_eq!(resolve_partial(v.iter(), "20").as_deref(), Some("20.11.0"));
        assert_eq!(resolve_partial(v.iter(), "18"), None);
    }

    #[test]
    fn test_resolve_partial_minor() {
        let v = versions(&["22.0.0", "22.0.1", "22.1.0"]);
        assert_eq!(resolve_partial(v.iter(), "22.0").as_deref(), Some("22.0.1"));
        // node@22.19 → newest in 22.19.z
        let v = versions(&["22.19.0", "22.19.3", "22.20.0"]);
        assert_eq!(resolve_partial(v.iter(), "22.19").as_deref(), Some("22.19.3"));
    }

    /// Request 22 must not match version 2 (a third-condition bug in the old
    /// implementation)
    #[test]
    fn test_resolve_partial_no_false_major_match() {
        let v = versions(&["2.0.0", "20.0.0"]);
        // "22" doesn't match "2" (request doesn't continue past "2.")
        assert_eq!(resolve_partial(v.iter(), "22"), None);
        // Request 2.0 may match dot-less version 2 (request continues past the version)
        let v2 = versions(&["2", "3.0.0"]);
        assert_eq!(resolve_partial(v2.iter(), "2.0").as_deref(), Some("2"));
    }

    /// Full round-trip of the recorded checksum with local verification:
    /// write → verify pass; tampered archive → verification fails
    #[test]
    fn test_recorded_hash_detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let tool_dir = dir.path().join("node");
        fs::create_dir_all(&tool_dir).unwrap();
        let archive = tool_dir.join("node-22.0.0-win-x64.zip");
        fs::write(&archive, b"hello").unwrap();

        // No recorded hash → verification skipped
        assert!(verify_recorded_hash(&cache_plan(dir.path(), "node-22.0.0-win-x64.zip")).is_ok());

        // Correct hash recorded → verification passes
        let digest = compute_checksum(&archive, "sha256").unwrap();
        record_archive(&archive, 24, Some(("sha256", &digest)));
        assert!(verify_recorded_hash(&cache_plan(dir.path(), "node-22.0.0-win-x64.zip")).is_ok());

        // Tampered archive → verification fails
        fs::write(&archive, b"tampered").unwrap();
        assert!(verify_recorded_hash(&cache_plan(dir.path(), "node-22.0.0-win-x64.zip")).is_err());
    }

    /// Global registry merge: plugin sources deduped, order preserved.
    /// (The test env has an empty GLOBAL_CONFIG.registry; the global branch is
    /// covered by config tests.)
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
                RemoteVersion { version: "v20.11.0".into(), lts: Some("Iron".into()) },
                RemoteVersion { version: "v20.12.0".into(), lts: Some("Iron".into()) },
                // lts: false (boolean) doesn't count as an LTS marker
                RemoteVersion { version: "v22.0.0".into(), lts: None },
            ]
        );
    }

    #[test]
    fn test_extract_versions_from_api_with_path() {
        let json = r#"{"data": {"releases": ["1.2.3", "1.2.4"]}}"#;
        let out = extract_versions_from_api(json, Some("data.releases")).unwrap();
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
        // When the pattern strips the v prefix, lts metadata must be preserved
        let raw = vec![
            RemoteVersion { version: "v20.11.0".into(), lts: Some("Iron".into()) },
            RemoteVersion { version: "v22.0.0".into(), lts: None },
        ];
        let out = apply_version_pattern(raw, Some("^v(?<version>.*)$"));
        assert_eq!(
            out,
            vec![
                RemoteVersion { version: "20.11.0".into(), lts: Some("Iron".into()) },
                RemoteVersion { version: "22.0.0".into(), lts: None },
            ]
        );
    }

    #[test]
    fn test_stripped_path_strips_and_sanitizes() {
        let dest = Path::new("out");
        let p = stripped_path(dest, "pkg-1.0.0/bin/app.exe", 1).unwrap();
        assert_eq!(p, dest.join("bin").join("app.exe"));

        // Escaping components (../) are filtered so nothing escapes dest
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
        // sha256 of "hello"
        assert_eq!(digest.as_str(), "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }

    #[test]
    fn test_extract_hash_pattern() {
        let text = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  node.exe";
        let out = extract_hash(text, Some(r"(?P<hash>[0-9a-f]{64})"), "node.exe").unwrap();
        assert_eq!(out.as_str().len(), 64);
        // Without a pattern, match the line by archive file name
        let out2 = extract_hash(text, None, "node.exe").unwrap();
        assert_eq!(out2.as_str().len(), 64);
    }

    /// SHASUMS256.txt real shape: multiple lines, target not first,
    // and the ^$ anchors of the pattern must apply per line.
    #[test]
    fn test_extract_hash_multiline_anchors() {
        let text = "\
1111111111111111111111111111111111111111111111111111111111111111  node-v24.19.0-aix-ppc64.tar.gz
2222222222222222222222222222222222222222222222222222222222222222  node-v24.19.0-linux-x64.tar.xz
2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  node-v24.19.0-win-x64.zip
";
        let pattern = r"^([a-f0-9]{64})\s+.*node-v24.19.0-win-x64.zip$";
        let out = extract_hash(text, Some(pattern), "node-v24.19.0-win-x64.zip").unwrap();
        assert_eq!(out.as_str(), "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }

    /// No pattern + multi-file lists: must take the line by archive file name,
    /// not the first hex in the file (which belongs to another platform's
    /// artifact)
    #[test]
    fn test_extract_hash_no_pattern_prefers_filename_line() {
        let text = "\
1111111111111111111111111111111111111111111111111111111111111111  node-v24.19.0-aix-ppc64.tar.gz
2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  node-v24.19.0-win-x64.zip
";
        let out = extract_hash(text, None, "node-v24.19.0-win-x64.zip").unwrap();
        assert_eq!(out.as_str(), "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        // With no matching line for the file name, fall back to the first hex in the
        // file
        let fallback = extract_hash(text, None, "no-such-file.zip").unwrap();
        assert_eq!(fallback.as_str().len(), 64);
        assert!(fallback.as_str().starts_with("1111"));
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

    /// Rewind a file's mtime by a duration, to build an "expired" archive
    fn age_file(path: &Path, ago: Duration) {
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - ago).unwrap();
    }

    #[test]
    fn test_gc_migrates_legacy_layout() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();

        // Legacy layout: cache/node/{extract leftovers, archive}
        let legacy = cache.join("node");
        fs::create_dir_all(legacy.join("extract")).unwrap();
        fs::write(legacy.join("extract/leftover.txt"), b"x").unwrap();
        fs::write(legacy.join("old.zip"), b"x").unwrap();

        // Loose files at the cache root (e.g. plugins.json) aren't download cache
        fs::write(cache.join("plugins.json"), b"{}").unwrap();

        gc_cache_at(cache, 24);

        assert!(!legacy.exists(), "旧布局目录应被移除");
        let new_tool = cache.join("tools").join("node");
        assert!(new_tool.join("old.zip").exists(), "档案应迁移到 cache/tools/<tool>/");
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
            ArchiveRecord { downloaded_at: unix_now(), ttl_hours: 24, algorithm: None, hash: None },
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
        assert!(!after.archives.contains_key("expired.zip"), "过期记录应被修剪");
        assert!(after.archives.contains_key("fresh.zip"));
    }

    #[test]
    fn test_gc_zero_ttl_purges_all() {
        // ttl = 0 means keep no cache: clear both legacy and new layouts
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
        let first =
            load_records(&tool_dir).archives.get("node-22.0.0-win-x64.zip").cloned().unwrap();
        assert_eq!(first.ttl_hours, 24);

        // Re-downloading the same archive refreshes its download time
        std::thread::sleep(Duration::from_secs(1));
        record_archive(
            &archive,
            48,
            Some((
                "sha256",
                &HexDigest::parse(
                    "abc123abc123abc123abc123abc123abc123abc123abc123abc123abc123abc1",
                )
                .unwrap(),
            )),
        );
        let second =
            load_records(&tool_dir).archives.get("node-22.0.0-win-x64.zip").cloned().unwrap();
        assert_eq!(second.ttl_hours, 48);
        assert!(second.downloaded_at > first.downloaded_at);
    }

    /// Build an install plan pointing to an archive under dir (only for
    /// cache-hit decisions)
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

        // Missing file → not a hit
        assert!(!archive_cache_hit(&cache_plan(dir, "a.zip")));

        // Exists, non-empty, no record (orphan) → hit
        fs::write(tool_dir.join("a.zip"), b"archive").unwrap();
        assert!(archive_cache_hit(&cache_plan(dir, "a.zip")));

        // Empty file (previous download interrupted) → not a hit
        fs::write(tool_dir.join("empty.zip"), b"").unwrap();
        assert!(!archive_cache_hit(&cache_plan(dir, "empty.zip")));

        // Record within TTL → hit; expired → not a hit
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
        // The file name carries a hex expiry
        assert_eq!(
            parse_cache_expiry("node", &version_cache_file("node", 0x19a4c0f00)),
            Some(0x19a4c0f00)
        );
        // Non-hex / other tool prefix / missing suffix → not parseable
        assert_eq!(parse_cache_expiry("node", "node_remote_version_xz.json"), None);
        assert_eq!(parse_cache_expiry("node", "go_remote_version_10.json"), None);
        assert_eq!(parse_cache_expiry("node", "node_remote_version_10"), None);
        // The prefix must be a full segment: nodej_remote_version_10 isn't node
        assert_eq!(parse_cache_expiry("node", "nodej_remote_version_10.json"), None);
    }

    #[test]
    fn test_version_cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let versions = vec![
            RemoteVersion { version: "20.11.0".into(), lts: Some("Iron".into()) },
            RemoteVersion { version: "22.11.0".into(), lts: None },
        ];
        save_cached_versions(dir.path(), "node", &versions);

        // The saved file name should hold a hex expiry and read back exactly (including
        // lts metadata)
        let saved: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(saved.len(), 1, "同工具仅保留一份缓存");
        assert!(saved[0].starts_with("node_remote_version_") && saved[0].ends_with(".json"));
        assert_eq!(load_cached_versions(dir.path(), "node").unwrap(), versions);
    }

    #[test]
    fn test_version_cache_expired_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        // Past timestamp → expired
        let stale = dir.path().join(version_cache_file("node", unix_now() - 10));
        fs::write(&stale, r#"[{"version":"1.0.0"}]"#).unwrap();

        assert!(load_cached_versions(dir.path(), "node").is_none());
        assert!(!stale.exists(), "过期缓存应被顺手清理");
    }

    #[test]
    fn test_save_replaces_old_cache() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join(version_cache_file("node", unix_now() + 3600));
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
        // Two caches for one tool (anomalous leftover): take the later-expiring one
        let soon = dir.path().join(version_cache_file("node", unix_now() + 60));
        let late = dir.path().join(version_cache_file("node", unix_now() + 3600));
        fs::write(&soon, r#"[{"version":"1.0.0"}]"#).unwrap();
        fs::write(&late, r#"[{"version":"2.0.0"}]"#).unwrap();

        assert_eq!(
            load_cached_versions(dir.path(), "node").unwrap(),
            vec![RemoteVersion { version: "2.0.0".into(), lts: None }]
        );
    }

    #[test]
    fn test_load_cache_rejects_legacy_format() {
        // Legacy plain-string-array cache → parse failure treated as no cache (upgrades
        // naturally on refetch)
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join(version_cache_file("node", unix_now() + 3600));
        fs::write(&legacy, r#"["1.0.0","2.0.0"]"#).unwrap();

        assert!(load_cached_versions(dir.path(), "node").is_none());
    }

    #[test]
    fn test_gc_skips_non_tool_cache_dirs() {
        // versions/builds are other cache partitions; must not be migrated as legacy
        // tool dirs
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        let legacy = cache.join("node");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("a.zip"), b"x").unwrap();

        let versions = cache.join("versions");
        fs::create_dir_all(&versions).unwrap();
        fs::write(versions.join(version_cache_file("node", unix_now() + 3600)), r#"["22.11.0"]"#)
            .unwrap();

        gc_cache_at(cache, 24);

        assert!(
            versions.join(version_cache_file("node", unix_now() + 3600)).exists(),
            "versions 缓存不应被迁移"
        );
        assert!(!legacy.exists(), "旧布局工具目录仍应正常迁移");
    }
}
