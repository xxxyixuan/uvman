//! Shared shim logic: forward-target lookup and (re)generation.
//!
//! A shim is a command-name forwarder living in `<UVMAN_HOME>/shims/` — the
//! single stable PATH entry GUI/IDE processes see. `locate_forward_target`
//! resolves a shim invocation to the same executable `which` reports (both go
//! through the shared resolution entry), and `rehash` regenerates the shim set
//! from the active tools' deploy dirs. The `uvman-shim` binary only calls
//! `locate_forward_target`; the main program drives `rehash` and the PATH
//! wiring (plan 0.3.0).

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::current;
use crate::core::error::UError;
use crate::core::resolve;

/// Subdir of `shims/` holding uvman's private helper binary and the manifest;
/// never exposed on PATH itself
pub const HELPER_DIR: &str = ".uvman";

/// Manifest file recording which shims uvman generated (under
/// `shims/<HELPER_DIR>/`); rehash cleans only files it lists, so foreign files
/// a user placed by hand are never deleted
pub const MANIFEST_FILE: &str = "manifest.toml";

/// Windows extensions that count as a shimmable command entry (same set
/// `which` probes); Unix commands are bare names
fn shim_extensions() -> &'static [&'static str] {
    if cfg!(windows) { &[".exe", ".cmd", ".bat", ".ps1"] } else { &[""] }
}

/// Derive the command name from the shim file name: on Windows the command
/// is named by its *next* extension (`npm.cmd` → `npm`), while `node.exe`
/// keeps `node`; on Unix the file name is already the bare command.
fn command_of_shim(file_name: &str) -> &str {
    let bare = match cfg!(windows) {
        true => file_name,
        false => return file_name,
    };
    let name = bare.strip_suffix(".cmd").or_else(|| bare.strip_suffix(".bat"));
    match name {
        Some(stem) => stem,
        None => bare.trim_end_matches(".exe").trim_end_matches(".ps1"),
    }
}

fn is_executable_entry(file_name: &str) -> bool {
    shim_extensions()
        .iter()
        .any(|ext| if ext.is_empty() { !file_name.contains('.') } else { file_name.ends_with(ext) })
}

/// Locate a same-named executable inside a version dir: root first, then the
/// `bin/` subdir (mirrors the PATH precedence `env` bakes in). Falls back to
/// probing the platform candidates of the stripped command name (so a
/// `node.exe` shim still lands on a deploy whose binary is `node.cmd`),
/// matching the `which` rules.
fn find_named_executable(version_dir: &Path, file_name: &str) -> Option<PathBuf> {
    let bin_dir = version_dir.join("bin");
    for dir in [version_dir, bin_dir.as_path()] {
        let candidate = dir.join(file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // Exact name is absent: fall back to the platform candidate probe on the
    // bare command name (Windows extension order, Unix bare name)
    let command = command_of_shim(file_name);
    if command != file_name {
        return crate::core::executable::find_executable(version_dir, command);
    }
    None
}

/// Resolve the forward target for a shim invocation.
///
/// The active-tool table is matched against the shim file name: for each
/// active tool (name order) whose version dir is deployed, look for a
/// same-named executable (root first, then `bin/`). None when no active tool
/// provides the command — the shim then reports an actionable error instead
/// of silently doing nothing.
pub fn locate_forward_target(home: &Path, shim_file_name: &str) -> Option<PathBuf> {
    let table = current::load_from(&home.join("config").join("tool_current.toml"));
    let tools_root = home.join("tools");
    for (tool, entry) in &table.tools {
        let _ = entry; // the version comes from the shared resolution entry
        let (version, _scope) = resolve::from_table(&table, tool.as_str())?;
        let version_dir = tools_root.join(tool).join(version);
        if let Some(target) = find_named_executable(&version_dir, shim_file_name) {
            return Some(target);
        }
    }
    None
}

/// The file name under `shims/` this process is acting as: an explicit
/// `UVMAN_SHIM_NAME` (set by the `.cmd`/`.bat` wrappers, whose own name the
/// helper binary cannot infer) wins; otherwise the copy's own file name.
pub fn acting_shim_name() -> OsString {
    if let Some(name) = std::env::var_os("UVMAN_SHIM_NAME") {
        return name;
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(ToOwned::to_owned))
        .unwrap_or_default()
}

/// Manifest of generated shims (under `shims/.uvman/manifest.toml`)
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ShimManifest {
    /// Command names uvman generated shims for, e.g. `["node", "npm"]`
    #[serde(default)]
    pub generated: Vec<String>,
}

fn manifest_path(shims: &Path) -> PathBuf {
    shims.join(HELPER_DIR).join(MANIFEST_FILE)
}

fn load_manifest(shims: &Path) -> ShimManifest {
    fs::read_to_string(manifest_path(shims))
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_manifest(shims: &Path, manifest: &ShimManifest) -> Result<(), UError> {
    let path = manifest_path(shims);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|source| UError::FileError { path: parent.to_path_buf(), source })?;
    }
    let text = toml::to_string_pretty(manifest)
        .map_err(|source| UError::TomlSerializeError { path: path.clone(), source })?;
    fs::write(&path, text).map_err(|source| UError::FileError { path, source })
}

/// Command names the manifest records as generated shims (missing manifest →
/// empty)
pub fn manifest_names(shims: &Path) -> Vec<String> {
    load_manifest(shims).generated
}

/// Every command name the currently active, deployed tools provide (sorted,
/// deduped) — the desired shim set.
pub fn active_command_names(home: &Path) -> Vec<String> {
    let table = current::load_from(&home.join("config").join("tool_current.toml"));
    let mut names = Vec::new();
    for (tool, _entry) in &table.tools {
        let Some((version, _scope)) = resolve::from_table(&table, tool.as_str()) else {
            continue;
        };
        let version_dir = home.join("tools").join(tool).join(version);
        if !version_dir.is_dir() {
            continue; // active version deleted by hand: report, never repair
        }
        names.extend(executables_in(&version_dir));
    }
    names.sort();
    names.dedup();
    names
}

/// Outcome of one `rehash` run (consumed by the `shims rehash` command)
#[derive(Debug, Default)]
pub struct RehashReport {
    /// Shims (re)written in this run, sorted
    pub generated: Vec<String>,
    /// Stale shims removed because their tool/version left the active set
    pub removed: Vec<String>,
}

/// File name the `uvman-shim` binary has on this platform
pub fn shim_binary_name() -> &'static str {
    if cfg!(windows) { "uvman-shim.exe" } else { "uvman-shim" }
}

/// Locate the `uvman-shim` binary to copy as the forwarder: next to the
/// running `uvman` executable first (the shipped layout), then beside the home
/// dir (portable folder moves).
fn shim_template(home: &Path) -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(shim_binary_name())))
        .into_iter()
        .chain(std::iter::once(home.join(shim_binary_name())))
        .collect();
    candidates.into_iter().find(|p| p.is_file())
}

/// Whether shims can be generated right now (the forwarding binary is present
/// to copy); auto-rehash skips quietly when it is not.
pub fn shim_available(home: &Path) -> bool {
    shim_template(home).is_some()
}

/// Command-entry file names inside a version dir (root first, then `bin/`),
/// deduplicated — the set a deploy contributes to the shim namespace.
fn executables_in(version_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for dir in [version_dir.to_path_buf(), version_dir.join("bin")] {
        if let Ok(entries) = fs::read_dir(&dir) {
            for path in entries.flatten().map(|e| e.path()) {
                let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                    continue;
                };
                if path.is_file() && is_executable_entry(&name) {
                    names.push(name);
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Regenerate the shim set from the active tools' deploy dirs.
///
/// - Removes shims listed in the manifest that no active version provides
///   (never touches files a user placed by hand — they aren't in the
///   manifest);
/// - (re)writes a shim per active executable, plus the private helper copy
///   under `shims/<HELPER_DIR>/`;
/// - records the new set in the manifest. Idempotent: re-running overwrites
///   same-name shims only.
pub fn rehash(home: &Path) -> Result<RehashReport, UError> {
    let shims = home.join("shims");

    // Target set: every command name any active deployed version provides
    let target = active_command_names(home);

    // Stale cleanup is manifest-driven; the manifest dose not track the
    // helper dir, so it is safe to leave it untouched here.
    let previous = load_manifest(&shims);
    let removed: Vec<String> =
        previous.generated.iter().filter(|n| !target.contains(n)).cloned().collect();
    for name in &removed {
        let _ = fs::remove_file(shims.join(name));
    }

    let mut generated = Vec::new();
    if !target.is_empty() {
        let helper_dir = shims.join(HELPER_DIR);
        fs::create_dir_all(&helper_dir)
            .map_err(|source| UError::FileError { path: helper_dir.clone(), source })?;
        let template = shim_template(home).ok_or_else(|| {
            UError::SimpleError(
                "uvman-shim binary not found beside uvman; reinstall uvman to regenerate shims"
                    .into(),
            )
        })?;
        let helper = helper_dir.join(shim_binary_name());
        fs::copy(&template, &helper)
            .map_err(|source| UError::FileError { path: helper.clone(), source })?;
        for name in &target {
            write_shim(&shims, &helper, name)?;
            generated.push(name.clone());
        }
    }

    save_manifest(&shims, &ShimManifest { generated: target.clone() })?;
    Ok(RehashReport { generated, removed })
}

fn write_shim(shims: &Path, helper: &Path, name: &str) -> Result<(), UError> {
    #[cfg(windows)]
    {
        let dest = shims.join(name);
        if name.ends_with(".exe") {
            fs::copy(helper, &dest)
                .map_err(|source| UError::FileError { path: dest.clone(), source })?;
        } else {
            // cmd/bat/ps1 targets: a script wrapper that re-invokes the helper
            // with the intended shim name (the helper alone cannot infer it)
            let script = format!(
                "@echo off\r\nset \"UVMAN_SHIM_NAME={name}\"\r\n\"{}\" %*\r\n",
                helper.display()
            );
            fs::write(&dest, script).map_err(|source| UError::FileError { path: dest, source })?;
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dest = shims.join(name);
        fs::copy(helper, &dest)
            .map_err(|source| UError::FileError { path: dest.clone(), source })?;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))
            .map_err(|source| UError::FileError { path: dest, source })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_home_with(dir: &Path, tool: &str, version: &str, files: &[&str]) -> PathBuf {
        let home = dir.to_path_buf();
        let version_dir = home.join("tools").join(tool).join(version);
        for f in files {
            let p = version_dir.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b"binary").unwrap();
        }
        // Record the activation in the internal table file (as `use` would)
        current::set_current_at(&home.join("config").join("tool_current.toml"), tool, version)
            .unwrap();
        home
    }

    #[test]
    fn test_command_of_shim_names() {
        if cfg!(windows) {
            assert_eq!(command_of_shim("node.exe"), "node");
            assert_eq!(command_of_shim("npm.cmd"), "npm");
            assert_eq!(command_of_shim("npx.bat"), "npx");
            assert_eq!(command_of_shim("run.ps1"), "run");
        } else {
            // Unix shims are bare names; a dotted file name stays as-is
            assert_eq!(command_of_shim("node"), "node");
        }
    }

    #[test]
    fn test_is_executable_entry_classification() {
        if cfg!(windows) {
            assert!(is_executable_entry("node.exe"));
            assert!(is_executable_entry("npm.cmd"));
            assert!(is_executable_entry("run.bat"));
            // Libraries and docs are not shimmable
            assert!(!is_executable_entry("node.dll"));
            assert!(!is_executable_entry("README.md"));
            assert!(!is_executable_entry("node_modules"));
        } else {
            assert!(is_executable_entry("node"));
            assert!(!is_executable_entry("README.md"));
        }
    }

    #[test]
    fn test_locate_forward_target_hits_active_tool_bin_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "bin/npm.cmd"]);
        let tools = home.join("tools").join("node").join("22.19.0");

        if cfg!(windows) {
            assert_eq!(locate_forward_target(&home, "node.exe"), Some(tools.join("node.exe")));
            assert_eq!(
                locate_forward_target(&home, "npm.cmd"),
                Some(tools.join("bin").join("npm.cmd"))
            );
        } else {
            assert_eq!(locate_forward_target(&home, "node"), Some(tools.join("node")));
        }
    }

    #[test]
    fn test_locate_forward_target_skips_missing_and_inactive() {
        let dir = tempfile::tempdir().unwrap();
        // `node` is active but its version dir was deleted by hand; `go` is
        // active with a dir but no matching file. Both resolve to None, not
        // panic — the shim's actionable error path.
        let home = make_home_with(dir.path(), "go", "1.23.0", &["go"]);
        current::set_current_at(&home.join("config").join("tool_current.toml"), "node", "22.19.0")
            .unwrap();
        current::set_current_at(&home.join("config").join("tool_current.toml"), "go", "1.23.0")
            .unwrap();

        if cfg!(windows) {
            assert_eq!(locate_forward_target(&home, "node.exe"), None);
            assert_eq!(locate_forward_target(&home, "go.cmd"), None);
        } else {
            assert_eq!(locate_forward_target(&home, "node"), None);
            assert_eq!(locate_forward_target(&home, "go"), Some(home.join("tools/go/1.23.0/go")));
        }
    }

    #[test]
    fn test_locate_forward_target_empty_table() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "go", "1.23.0", &["go"]);
        // `go` was installed but never activated: no entry in the table, so
        // nothing resolves (tools on disk alone are not enough)
        current::remove_current_at(&home.join("config").join("tool_current.toml"), "go").unwrap();
        assert_eq!(locate_forward_target(&home, "go"), None);
        assert_eq!(locate_forward_target(&home, "python.exe"), None);
    }

    #[test]
    fn test_manifest_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let shims = dir.path().join("shims");
        let mut manifest = ShimManifest::default();
        manifest.generated.push("node".into());
        save_manifest(&shims, &manifest).unwrap();
        assert_eq!(load_manifest(&shims).generated, vec!["node".to_string()]);
        // Missing manifest reads back empty (list gets regenerated)
        assert!(load_manifest(&dir.path().join("absent")).generated.is_empty());
    }

    /// Same activation state → the shim forwarder lands on exactly the file
    /// `which` reports (plan 0.3.0 completion criterion: same source, no
    /// drift).
    #[test]
    fn test_forward_target_matches_which_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe"]);
        let shim_name = if cfg!(windows) { "node.exe" } else { "node" };
        let via_shim = locate_forward_target(&home, shim_name);
        let via_which =
            crate::core::executable::locate(&home.join("tools"), "node", "22.19.0").ok();
        assert_eq!(via_shim, via_which);
    }

    /// Place a fake `uvman-shim` template beside the home dir (the portable
    /// layout) so rehash can copy it
    fn seed_template(home: &Path) {
        fs::write(home.join(shim_binary_name()), b"template-binary").unwrap();
    }

    #[test]
    fn test_rehash_generates_shims_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "bin/npm.cmd"]);
        seed_template(&home);

        let report = rehash(&home).unwrap();
        assert!(!report.generated.is_empty());
        assert!(report.removed.is_empty());

        let shims = home.join("shims");
        // The helper binary landed in the private subdir
        assert!(shims.join(HELPER_DIR).join(shim_binary_name()).is_file());
        if cfg!(windows) {
            // .exe target → a binary copy; .cmd target → a wrapper script
            assert_eq!(
                fs::read(shims.join("node.exe")).unwrap(),
                fs::read(shims.join(HELPER_DIR).join(shim_binary_name())).unwrap()
            );
            let npm = fs::read_to_string(shims.join("npm.cmd")).unwrap();
            assert!(npm.contains("UVMAN_SHIM_NAME=npm.cmd"), "wrapper names the command");
        } else {
            assert!(shims.join("node").is_file());
            assert!(shims.join("npm").is_file());
        }
        // The manifest records exactly the generated set, sorted
        let mut expect: Vec<String> = if cfg!(windows) {
            vec!["npm.cmd".to_string(), "node.exe".to_string()]
        } else {
            vec!["npm".to_string(), "node".to_string()]
        };
        expect.sort();
        assert_eq!(load_manifest(&shims).generated, expect);
    }

    #[test]
    fn test_rehash_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe"]);
        seed_template(&home);

        let first = rehash(&home).unwrap();
        let second = rehash(&home).unwrap();
        // Same set both times: nothing new generated, nothing removed
        assert_eq!(first.generated, second.generated);
        assert!(second.removed.is_empty());
    }

    #[test]
    fn test_rehash_prunes_stale_shims_only_from_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe"]);
        seed_template(&home);
        rehash(&home).unwrap();

        let shims = home.join("shims");
        // A foreign file the user placed by hand is not in the manifest
        let foreign_needed = shims.join("keep_me.txt");
        fs::write(&foreign_needed, b"user file").unwrap();

        // Tool removed from the active set → its shim goes stale
        current::remove_current_at(&home.join("config").join("tool_current.toml"), "node").unwrap();
        let report = rehash(&home).unwrap();
        assert!(report.removed.contains(&"node.exe".to_string()));
        assert!(!shims.join("node.exe").exists(), "stale shim removed");
        assert!(foreign_needed.exists(), "user-placed file must survive");
        assert!(load_manifest(&shims).generated.is_empty(), "manifest emptied");
    }

    #[test]
    fn test_rehash_requires_template_binary() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe"]);
        // No template beside the home: an actionable error, no half-written state
        let err = rehash(&home).unwrap_err();
        assert!(err.to_string().contains("uvman-shim"), "err: {err}");
        assert!(!home.join("shims").join("node.exe").exists());
    }

    #[test]
    fn test_executables_in_scans_root_and_bin() {
        let dir = tempfile::tempdir().unwrap();
        let version_dir = dir.path().join("node").join("22.19.0");
        fs::create_dir_all(version_dir.join("bin")).unwrap();
        fs::write(version_dir.join("node.exe"), b"x").unwrap();
        fs::write(version_dir.join("bin/npm.cmd"), b"x").unwrap();
        // Non-executable entries are ignored
        fs::write(version_dir.join("README.md"), b"x").unwrap();
        fs::write(version_dir.join("node.dll"), b"x").unwrap();

        let mut names = executables_in(&version_dir);
        names.sort();
        if cfg!(windows) {
            assert_eq!(names, vec!["node.exe".to_string(), "npm.cmd".to_string()]);
        } else {
            // Unix: bare, dot-free names only
            assert!(names.is_empty() || names == vec!["npm".to_string(), "node".to_string()]);
        }
    }
}
