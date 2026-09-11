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

/// The reverse: every file name a given command's shim(s) may take on the
/// platform (Windows `node` → `node.cmd..`? no — `node` → `node.exe` etc.),
/// used to decide whether a deploy-dir file should become a shim.
fn shim_names_for_command(command: &str) -> Vec<String> {
    if cfg!(windows) {
        [".exe", ".cmd", ".bat", ".ps1"].iter().map(|ext| format!("{command}{ext}")).collect()
    } else {
        vec![command.to_string()]
    }
}

/// Whether a deploy-dir file name is a shimmable executable entry
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

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(
                locate_forward_target(&home, "node.exe"),
                Some(tools.join("node.exe"))
            );
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
}