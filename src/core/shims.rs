//! Shared shim logic: forward-target lookup and (re)generation.
//!
//! A shim is a command-name entry living in `<UVMAN_HOME>/shims/` — the single
//! stable PATH entry GUI/IDE processes see. `locate_forward_target` resolves a
//! shim invocation to the same executable `which` reports (both go through the
//! shared resolution entry), and `rehash` regenerates the shim set from the
//! active tools' deploy dirs. The main program drives `rehash` and the PATH
//! wiring (plan 0.3.0).
//!
//! # Two shim kinds (`write_shim`)
//!
//! A deploy contributes two very different things under one command namespace,
//! and they must not be generated the same way:
//!
//! - **Binary entries** (`.exe`, Unix bare names) get a *forwarder*: a copy of
//!   `uvman-shim` that resolves the active version at call time and forwards
//!   argv + stdio + exit code. This is what makes `use` version switching
//!   PATH-free.
//! - **Script entries** (`.ps1` / `.cmd` / `.bat`) are *copied verbatim*. A
//!   forwarder cannot serve them: the interpreter must read the real file, and
//!   a bundled script resolves its own siblings through `%~dp0` / `$PSScriptRoot`
//!   — a wrapper in `shims/` breaks that layout (npm's `npm.cmd` is the classic
//!   case: it must stay next to `npm` and `node_modules/`). A byte-for-byte
//!   copy keeps every relative reference, argument and exit-code semantic
//!   exactly as the vendor shipped it.
//!
//! Note: the `uvman-shim` binary no longer calls into this module — it carries
//! its own `std`-only copy of the lookup so it stays dependency-free (see
//! `src/bin/uvman-shim.rs`). This copy is what `doctor` / `shims` use; the
//! `shim_target_matches_core` test in the shim keeps the two in step.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::current;
use crate::core::error::UError;

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

/// How a shim entry is materialised in `shims/`. The kind is derived from the
/// *file type* of the deploy entry, never from the command name, and it drives
/// both generation and the health checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShimKind {
    /// Executable program (`.exe` on Windows, bare name on Unix): a copy of
    /// the `uvman-shim` forwarder that resolves the active version at call time
    Forward,
    /// Script (`.ps1` / `.cmd` / `.bat`): the tool's own script, copied verbatim
    Script,
}

impl ShimKind {
    /// Classification of a shim file name.
    ///
    /// Windows suffix table — scripts first, everything else that carries a
    /// shimmable extension is a program:
    /// - `.ps1`, `.cmd`, `.bat` → [`ShimKind::Script`]
    /// - `.exe` and Unix bare names → [`ShimKind::Forward`]
    ///
    /// Case-insensitive on Windows: PATHEXT matching ignores case and an
    /// archive may ship `NPM.CMD`, which must still classify as a script.
    /// Scripts (unlike `.bin`/`.psm1`) are the only extensions Windows executes
    /// by name, which is what makes a verbatim copy a drop-in entry.
    pub fn of(file_name: &str) -> Self {
        if cfg!(windows) {
            let lower = file_name.to_ascii_lowercase();
            if [".ps1", ".cmd", ".bat"].iter().any(|ext| lower.ends_with(ext)) {
                return Self::Script;
            }
        }
        Self::Forward
    }

    /// Whether entries of this kind are copied from the deploy instead of
    /// being generated as forwarders
    pub fn is_script(self) -> bool {
        matches!(self, Self::Script)
    }
}

fn is_script_entry(file_name: &str) -> bool {
    ShimKind::of(file_name).is_script()
}

fn is_executable_entry(file_name: &str) -> bool {
    shim_extensions()
        .iter()
        .any(|ext| if ext.is_empty() { !file_name.contains('.') } else { file_name.ends_with(ext) })
}

/// Locate the *deploy source* of a shim: the same-named entry in each active
/// tool's version dir (root first, then `bin/`).
///
/// Binary shims use this only for the health check (the forwarder re-resolves
/// at every call); script shims use it as the copy source during `rehash`.
/// `predicate` decides which file type counts, so the two kinds never claim
/// each other's entry: a `node.exe` shim never resolves to a script sibling.
///
/// A tool whose active version is not deployed on disk is skipped, not fatal —
/// one broken tool must not hide a later tool that does provide the command.
pub fn locate_deploy_source(
    home: &Path, shim_file_name: &str, predicate: impl Fn(&str) -> bool,
) -> Option<PathBuf> {
    let table = current::load_from(&home.join("config").join("tool_current.toml"));
    let tools_root = home.join("tools");
    for (tool, entry) in &table.tools {
        let version = entry.version.as_str();
        let version_dir = tools_root.join(tool).join(version);
        if !version_dir.is_dir() {
            continue; // active version deleted by hand: report, never repair
        }
        let bin_dir = version_dir.join("bin");
        for dir in [version_dir.as_path(), bin_dir.as_path()] {
            let candidate = dir.join(shim_file_name);
            if candidate.is_file() && predicate(shim_file_name) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Locate the deploy source of a script shim (the verbatim copy source)
pub fn locate_script_source(home: &Path, shim_file_name: &str) -> Option<PathBuf> {
    locate_deploy_source(home, shim_file_name, is_script_entry)
}

/// Locate the forward target for a shim invocation.
///
/// The active-tool table is matched against the shim file name: for each
/// active tool (name order) whose version dir is deployed, look for a
/// same-named executable (root first, then `bin/`). None when no active tool
/// provides the command — the shim then reports an actionable error instead
/// of silently doing nothing.
pub fn locate_forward_target(home: &Path, shim_file_name: &str) -> Option<PathBuf> {
    locate_deploy_source(home, shim_file_name, |_| true)
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
    for (tool, entry) in &table.tools {
        let version = entry.version.as_str();
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

/// Whether an existing shim entry is broken.
///
/// The two kinds fail differently, because only one of them resolves at call
/// time:
/// - a **forwarder** is broken when no active tool provides its command (it
///   would print "no actively managed tool provides …" at call time);
/// - a **script copy** is broken when no active version provides that script
///   (stale copy, left behind by a version switch), and additionally when the
///   copy has drifted from its deploy source.
///
/// Present-but-foreign files (shims a user placed by hand) are never reported:
/// uvman does not own them.
pub fn shim_is_broken(home: &Path, shims: &Path, file_name: &str) -> bool {
    match ShimKind::of(file_name) {
        ShimKind::Forward => locate_forward_target(home, file_name).is_none(),
        ShimKind::Script => match locate_script_source(home, file_name) {
            None => true,
            Some(source) => !copies_identical(&source, &shims.join(file_name)),
        },
    }
}

/// Byte comparison of a copied shim against its deploy source. A missing or
/// unreadable copy counts as drifted (the caller reports it).
fn copies_identical(source: &Path, shim: &Path) -> bool {
    match (fs::read(source), fs::read(shim)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
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
/// - (re)writes one entry per active command name: a forwarder for binary
///   entries, a verbatim copy of the tool's own script for `.cmd` / `.bat` /
///   `.ps1` (see [`ShimKind`]);
/// - copies the private helper binary under `shims/<HELPER_DIR>/` before the
///   forwarders — script entries don't need it, so a tool set made only of
///   scripts still works without it;
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

    // Only forwarders need the uvman-shim binary; resolve it lazily so a
    // script-only tool set still rehashes on a release without the helper.
    let mut helper: Option<PathBuf> = None;
    let mut generated = Vec::new();
    for name in &target {
        if ShimKind::of(name).is_script() {
            let source = locate_script_source(home, name).ok_or_else(|| {
                UError::SimpleError(format!(
                    "no active version provides the script '{name}'; \
                     reinstall the tool or run `uvman shims rehash` after activating one"
                ))
            })?;
            copy_verbatim(&source, &shims.join(name))?;
        } else {
            let helper = match &helper {
                Some(path) => path.clone(),
                None => {
                    let path = install_helper(&shims, home)?;
                    helper = Some(path.clone());
                    path
                },
            };
            write_forward_shim(&shims, &helper, name)?;
        }
        generated.push(name.clone());
    }

    save_manifest(&shims, &ShimManifest { generated: target.clone() })?;
    Ok(RehashReport { generated, removed })
}

/// Copy the `uvman-shim` forwarder template into the private helper dir and
/// return the copy's path
fn install_helper(shims: &Path, home: &Path) -> Result<PathBuf, UError> {
    let helper_dir = shims.join(HELPER_DIR);
    fs::create_dir_all(&helper_dir)
        .map_err(|source| UError::FileError { path: helper_dir.clone(), source })?;
    let template = shim_template(home).ok_or_else(|| {
        UError::SimpleError(
            "uvman-shim binary not found beside uvman; reinstall uvman to regenerate shims".into(),
        )
    })?;
    let helper = helper_dir.join(shim_binary_name());
    fs::copy(&template, &helper)
        .map_err(|source| UError::FileError { path: helper.clone(), source })?;
    Ok(helper)
}

/// Generate one binary forwarder: a copy of the helper that forwards by its
/// own file name (the helper is never exposed on PATH under its real name).
///
/// Windows shares the copy between concurrent processes; Unix additionally
/// needs the execute bit, which `fs::copy` does not carry over predictably.
fn write_forward_shim(shims: &Path, helper: &Path, name: &str) -> Result<(), UError> {
    let dest = shims.join(name);
    fs::copy(helper, &dest).map_err(|source| UError::FileError { path: dest.clone(), source })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))
            .map_err(|source| UError::FileError { path: dest, source })?;
    }
    Ok(())
}

/// Materialise one script shim: the tool's own script, byte for byte.
///
/// A verbatim copy is the whole point of the script branch — a wrapper would
/// move the script out of its deploy layout and break `%~dp0` /
/// `$PSScriptRoot` resolution (see [`ShimKind`]). The copied bytes are left
/// untouched (no newline rewriting): the vendor's encoding and line endings
/// are part of the script's correctness.
fn copy_verbatim(source: &Path, dest: &Path) -> Result<(), UError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .map_err(|source| UError::FileError { path: parent.to_path_buf(), source })?;
    }
    fs::copy(source, dest)
        .map_err(|source| UError::FileError { path: dest.to_path_buf(), source })?;
    Ok(())
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

    /// Windows-only rule: Unix commands are bare names, so scripts never reach
    /// the script branch there.
    fn windows_only() -> bool {
        cfg!(windows)
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

    /// The generation rule hinges on this split: scripts are copied verbatim,
    /// programs get a forwarder. Only Windows ever sees scripts (Unix
    /// commands are bare names), and matching must ignore case.
    #[test]
    fn test_shim_kind_classification() {
        // Every file type that gets a verbatim copy, and the boundary cases
        // that must stay forwarders, per platform
        if cfg!(windows) {
            assert_eq!(ShimKind::of("run.ps1"), ShimKind::Script);
            assert_eq!(ShimKind::of("npm.cmd"), ShimKind::Script);
            assert_eq!(ShimKind::of("npx.bat"), ShimKind::Script);
            // Uppercase suffixes come from real archives and mean the same
            assert_eq!(ShimKind::of("NPM.CMD"), ShimKind::Script);
            assert_eq!(ShimKind::of("Build.Ps1"), ShimKind::Script);

            assert_eq!(ShimKind::of("node.exe"), ShimKind::Forward);
            // No-extension entries (Windows-only tools do ship them) forward too
            assert_eq!(ShimKind::of("make"), ShimKind::Forward);
        } else {
            assert_eq!(ShimKind::of("node"), ShimKind::Forward);
            // Unix scripts are bare names, so they keep the forwarder
            assert_eq!(ShimKind::of("run.sh"), ShimKind::Forward);
        }
    }

    #[test]
    fn test_locate_forward_target_hits_active_tool_bin_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "bin/npm.exe"]);
        let tools = home.join("tools").join("node").join("22.19.0");

        if cfg!(windows) {
            assert_eq!(locate_forward_target(&home, "node.exe"), Some(tools.join("node.exe")));
            assert_eq!(
                locate_forward_target(&home, "npm.exe"),
                Some(tools.join("bin").join("npm.exe"))
            );
        } else {
            assert_eq!(locate_forward_target(&home, "node"), Some(tools.join("node")));
        }
    }

    /// A script shim resolves to the very script `rehash` copies — and only
    /// through the script branch.
    #[test]
    fn test_locate_script_source_targets_the_script() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "bin/npm.cmd"]);
        let tools = home.join("tools").join("node").join("22.19.0");
        assert_eq!(locate_script_source(&home, "npm.cmd"), Some(tools.join("bin").join("npm.cmd")));
        // The `.exe` entry is not a script source
        assert_eq!(locate_script_source(&home, "node.exe"), None);
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

    /// One tool with a hand-deleted version dir must not hide a later tool that
    /// does provide the command (`node` sorts before `python`).
    #[test]
    fn test_locate_forward_target_keeps_scanning_past_undeployed_tool() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "python", "3.13.1", &["python.exe", "python"]);
        // `node` is active but its version dir is gone
        current::set_current_at(&home.join("config").join("tool_current.toml"), "node", "22.19.0")
            .unwrap();

        let expected = home.join("tools/python/3.13.1").join(if cfg!(windows) {
            "python.exe"
        } else {
            "python"
        });
        assert_eq!(
            locate_forward_target(&home, if cfg!(windows) { "python.exe" } else { "python" }),
            Some(expected)
        );
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
            // .exe target → a forwarder copy of the helper binary
            assert_eq!(
                fs::read(shims.join("node.exe")).unwrap(),
                fs::read(shims.join(HELPER_DIR).join(shim_binary_name())).unwrap()
            );
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

    /// The core of this fix: a `.cmd`/`.ps1` deploy entry is copied verbatim
    /// into `shims/` — same bytes, no wrapper, no interpreter forwarding.
    #[test]
    fn test_rehash_copies_script_entries_verbatim() {
        if !windows_only() {
            return; // Windows-only rule: Unix scripts are bare names
        }
        let dir = tempfile::tempdir().unwrap();
        let script = b"@ECHO off\r\nnode \"%~dp0\\node_modules\\npm\\bin\\npm-cli.js\" %*\r\n";
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "bin/npm.cmd"]);
        let tools = home.join("tools").join("node").join("22.19.0");
        // A vendor script whose exact bytes matter (CRLF + %~dp0), plus a
        // PowerShell entry to cover the second script extension
        fs::write(tools.join("bin").join("npm.cmd"), script).unwrap();
        fs::write(tools.join("bin").join("setup.ps1"), b"Write-Host 'hi'\r\n").unwrap();
        seed_template(&home);

        rehash(&home).unwrap();

        let shims = home.join("shims");
        assert_eq!(
            fs::read(shims.join("npm.cmd")).unwrap(),
            script,
            "script shim must be a byte-for-byte copy of the vendor script"
        );
        assert_eq!(fs::read(shims.join("setup.ps1")).unwrap(), b"Write-Host 'hi'\r\n");
        // The copied script must not be a forwarder wrapper
        let copied = fs::read_to_string(shims.join("npm.cmd")).unwrap();
        assert!(!copied.contains("uvman-shim"), "no wrapper: the vendor script is the entry");
    }

    /// Script entries are copied, so a rehash must be able to find them in the
    /// version dir root too (deploys differ: root vs `bin/`).
    #[test]
    fn test_locate_script_source_prefers_root_then_bin() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "corepack.cmd"]);
        let tools = home.join("tools").join("node").join("22.19.0");
        fs::create_dir_all(tools.join("bin")).unwrap();

        // Root hit
        assert_eq!(locate_script_source(&home, "corepack.cmd"), Some(tools.join("corepack.cmd")));
        // Root miss → `bin/` hit
        fs::write(tools.join("bin").join("npm.cmd"), b"@echo off\r\n").unwrap();
        assert_eq!(locate_script_source(&home, "npm.cmd"), Some(tools.join("bin").join("npm.cmd")));
        // A binary name never resolves through the script branch
        assert_eq!(locate_script_source(&home, "node.exe"), None);
    }

    /// A script shim must be usable even when no forwarding binary was shipped
    /// — scripts don't need the helper at all.
    #[test]
    fn test_rehash_scripts_do_not_require_template_binary() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["bin/npm.cmd"]);
        let script = b"@ECHO off\r\necho npm\r\n";
        fs::write(home.join("tools/node/22.19.0/bin/npm.cmd"), script).unwrap();
        // No template beside the home
        let report = rehash(&home).unwrap();
        assert_eq!(report.generated, vec!["npm.cmd".to_string()]);
        assert_eq!(fs::read(home.join("shims/npm.cmd")).unwrap(), script);
        // The helper dir is not populated for a script-only set
        assert!(!home.join("shims").join(HELPER_DIR).join(shim_binary_name()).exists());
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

    /// Re-running rehash refreshes a stale script copy (version switch).
    #[test]
    fn test_rehash_refreshes_stale_script_copy() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["bin/npm.cmd"]);
        let script = home.join("tools/node/22.19.0/bin/npm.cmd");
        fs::write(&script, b"@ECHO off\r\necho v1\r\n").unwrap();

        rehash(&home).unwrap();
        let shim = home.join("shims/npm.cmd");
        assert_eq!(fs::read(&shim).unwrap(), b"@ECHO off\r\necho v1\r\n");

        // The deploy moves on → the next rehash must replace the copy
        fs::write(&script, b"@ECHO off\r\necho v2\r\n").unwrap();
        rehash(&home).unwrap();
        assert_eq!(fs::read(&shim).unwrap(), b"@ECHO off\r\necho v2\r\n");
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

    /// Stale script copies are pruned through the manifest exactly like
    /// forwarders (the deletion path is shared).
    #[test]
    fn test_rehash_prunes_stale_script_shim() {
        if !windows_only() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["bin/npm.cmd"]);
        fs::write(home.join("tools/node/22.19.0/bin/npm.cmd"), b"@echo off\r\n").unwrap();
        rehash(&home).unwrap();
        assert!(home.join("shims/npm.cmd").exists());

        current::remove_current_at(&home.join("config").join("tool_current.toml"), "node").unwrap();
        let report = rehash(&home).unwrap();
        assert!(report.removed.contains(&"npm.cmd".to_string()));
        assert!(!home.join("shims/npm.cmd").exists());
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
