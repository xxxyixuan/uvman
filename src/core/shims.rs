//! Shared shim logic: forward-target lookup and (re)generation.
//!
//! A shim is a command-name entry living in `<UVMAN_HOME>/shims/` — the single
//! stable PATH entry GUI/IDE processes see. Every shim is a copy of the
//! `uvman-shim` forwarder, named after the command it serves (`node.exe`; on
//! Unix just `node`). At call time the forwarder resolves the deploy entry
//! `which` would report (both go through the same resolution rules) and
//! forwards argv, stdio and the exit code.
//!
//! # One entry per command — everything is a forwarder
//!
//! Earlier releases copied `.cmd` / `.bat` / `.ps1` scripts into `shims/` and
//! rewrote their `%~dp0` / `$PSScriptRoot` self-references. That proved
//! untenable: the npm ecosystem emits bin shims with many self-reference
//! idioms (`%~dp0`, `%dp0%`, `$PSScriptRoot`, `$basedir=...`), so the rewrite
//! rules kept missing one. Script entries are forwarders too now; a script
//! runs **in its own deploy dir**, so every relative reference resolves
//! natively and nothing needs rewriting.
//!
//! A deployed tool provides a *command* (e.g. `npm`) that may exist in several
//! file forms side by side (bare shebang script `npm`, `npm.cmd`, `npm.ps1`).
//! The forwarder picks the form the invoking terminal would pick natively —
//! see [`TerminalType`] — by probing the deploy dir in a terminal-specific
//! order. The health checks treat a command as present when *any* usable form
//! exists, so they don't depend on the caller's terminal.
//!
//! Note: the `uvman-shim` binary no longer calls into this module — it carries
//! its own `std`-only copy of the lookup so it stays dependency-free (see
//! `src/bin/uvman-shim.rs`). This copy is what `doctor` / `shims` use; the
//! `shim_target_matches_core` tests keep the two in step.

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

/// Terminal context a shim may be called from; selects which deploy file form
/// to run for commands that ship several (`npm` / `npm.cmd` / `npm.ps1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalType {
    /// git-bash / MSYS / Unix-like shells: run the bare shebang script
    Posix,
    /// PowerShell: prefers the `.ps1` entry for script semantics
    PowerShell,
    /// cmd.exe, GUI/IDE, anything else: native Windows resolution order
    Other,
}

/// Classify the invoking terminal from environment hints. A pure function so
/// both this crate and the `uvman-shim` binary share one rule and tests can
/// inject values without touching the process env.
///
/// POSIX wins over PowerShell when both are hinted (a git-bash session opened
/// from a PowerShell parent may still carry `PSModulePath`). `MSYSTEM` alone
/// is enough (git-bash sets `MINGW64`/`MINGW32`); `OSTYPE` covers MSYS/Unix;
/// `SHELL` names a POSIX shell.
pub fn classify_terminal(
    ostype: Option<&str>, msystem: Option<&str>, shell: Option<&str>, psmodulepath: Option<&str>,
) -> TerminalType {
    let posix = ostype.is_some_and(|v| {
        ["msys", "linux", "darwin", "cygwin", "gnu"].iter().any(|k| v.contains(k))
    }) || msystem.is_some_and(|v| !v.is_empty())
        || shell.is_some_and(|v| ["bash", "zsh", "sh", "fish"].iter().any(|k| v.contains(k)));
    if posix {
        return TerminalType::Posix;
    }
    if psmodulepath.is_some_and(|v| !v.is_empty()) {
        return TerminalType::PowerShell;
    }
    TerminalType::Other
}

/// Terminal type of the calling process, read from the environment hints
/// shells export; the pure rule lives in [`classify_terminal`].
pub fn detect_terminal() -> TerminalType {
    classify_terminal(
        std::env::var("OSTYPE").ok().as_deref(),
        std::env::var("MSYSTEM").ok().as_deref(),
        std::env::var("SHELL").ok().as_deref(),
        std::env::var("PSModulePath").ok().as_deref(),
    )
}

/// Whether a file starts with a `#!` shebang (std-only, two-byte peek).
pub fn has_shebang(path: &Path) -> bool {
    use std::io::Read;
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut head = [0u8; 2];
    file.read_exact(&mut head).is_ok() && head == [b'#', b'!']
}

/// Command name a shim serves: strip the shim's own `.exe` on Windows (the
/// only name shims are generated under); Unix shims are already bare names.
pub fn command_of_shim(shim_file_name: &str) -> &str {
    if !cfg!(windows) {
        return shim_file_name;
    }
    shim_file_name.strip_suffix(".exe").unwrap_or(shim_file_name)
}

/// Deployment file forms a command may exist under for `terminal`, in the
/// order that terminal would resolve them natively:
///
/// - `Posix`: bare shebang script, then `.cmd` / `.bat` scripts
/// - `PowerShell`: `.ps1` first for script semantics
/// - `Other` (cmd / GUI): `.exe` first, then scripts
///
/// Health checks use [`TerminalType::Other`]: that list spans *every* form, so
/// "any usable form exists" falls out of the same lookup the runtime uses.
fn command_forms(command: &str, terminal: TerminalType) -> Vec<String> {
    if !cfg!(windows) {
        return vec![command.to_string()];
    }
    let ext = |e: &str| format!("{command}{e}");
    match terminal {
        TerminalType::Posix => {
            vec![command.to_string(), ext(".cmd"), ext(".bat"), ext(".ps1"), ext(".exe")]
        },
        TerminalType::PowerShell => {
            vec![ext(".ps1"), ext(".cmd"), ext(".bat"), ext(".exe"), command.to_string()]
        },
        TerminalType::Other => {
            vec![ext(".exe"), ext(".cmd"), ext(".bat"), ext(".ps1"), command.to_string()]
        },
    }
}

/// A candidate is usable when it exists and, on Windows, a bare (no-extension)
/// form only counts as a script with a shebang — never a stray text file.
fn form_usable(candidate: &Path, form: &str) -> bool {
    if cfg!(windows) && !form.contains('.') {
        return has_shebang(candidate);
    }
    true
}

/// Locate the deploy entry for `command` under a terminal's preferred form
/// order: each active tool's version dir (root first, then `bin/`), first
/// usable form wins. A tool whose active version is not deployed on disk is
/// skipped, not fatal — one broken tool must not hide a later tool that does
/// provide the command.
pub fn locate_entry(home: &Path, command: &str, terminal: TerminalType) -> Option<PathBuf> {
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
            for form in command_forms(command, terminal) {
                let candidate = dir.join(&form);
                if candidate.is_file() && form_usable(&candidate, &form) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

/// Locate the forward target for a shim invocation, terminal-agnostically.
///
/// The shim's own file name (e.g. `npm.exe`) is reduced to the command name
/// and resolved to the deploy entry in *any* usable form; this is what health
/// checks need ("does the command exist at all") and what keeps this side in
/// step with `which`. The runtime forwarder (in `src/bin/uvman-shim.rs`) uses
/// [`locate_entry`] with its detected terminal to pick the exact form.
pub fn locate_forward_target(home: &Path, shim_file_name: &str) -> Option<PathBuf> {
    locate_entry(home, command_of_shim(shim_file_name), TerminalType::Other)
}

/// Manifest of generated shims (under `shims/.uvman/manifest.toml`)
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ShimManifest {
    /// Command names uvman generated shims for, e.g. `["node.exe", "npm.exe"]`
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
/// deduped) — the desired shim set, expressed as the shim file name the
/// command resolves to (`npm.exe` on Windows, `npm` on Unix).
pub fn active_command_names(home: &Path) -> Vec<String> {
    let table = current::load_from(&home.join("config").join("tool_current.toml"));
    let mut commands = Vec::new();
    for (tool, entry) in &table.tools {
        let version = entry.version.as_str();
        let version_dir = home.join("tools").join(tool).join(version);
        if !version_dir.is_dir() {
            continue; // active version deleted by hand: report, never repair
        }
        commands.extend(commands_in(&version_dir));
    }
    commands.sort();
    commands.dedup();
    commands.into_iter().map(|command| shim_name_of(&command)).collect()
}

/// Shim file name a command is generated under: `{command}.exe` on Windows
/// (so cmd/PowerShell/GUI all resolve the command through one PATHEXT-first
/// entry), the bare command on Unix.
fn shim_name_of(command: &str) -> String {
    if cfg!(windows) { format!("{command}.exe") } else { command.to_string() }
}

/// Whether an existing shim entry is broken: a forwarder whose command has no
/// usable deploy entry in any active tool (it would print "no actively managed
/// tool provides …" at call time). Present-but-foreign files are never
/// reported: uvman does not own them.
pub fn shim_is_broken(home: &Path, _shims: &Path, file_name: &str) -> bool {
    locate_forward_target(home, file_name).is_none()
}

/// Outcome of one `rehash` run (consumed by the `shims rehash` command)
#[derive(Debug, Default)]
pub struct RehashReport {
    /// Shims (re)written in this run, sorted
    pub generated: Vec<String>,
    /// Stale shims removed because their command left the active set
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

/// Command names a version dir contributes to the shim namespace (root first,
/// then `bin/`), deduplicated:
///
/// - Windows: the stem of every `.exe`/`.cmd`/`.bat`/`.ps1` file, plus bare
///   files that are shebang scripts (git-bash resolves those by name);
///   `.cmd`/`.ps1` siblings collapse onto one command (`npm.cmd`, `npm.ps1`
///   and a bare shebang `npm` all yield `npm`);
/// - Unix: bare, dot-free names (unchanged).
fn commands_in(version_dir: &Path) -> Vec<String> {
    let mut commands = Vec::new();
    for dir in [version_dir.to_path_buf(), version_dir.join("bin")] {
        if let Ok(entries) = fs::read_dir(&dir) {
            for path in entries.flatten().map(|e| e.path()) {
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                    continue;
                };
                if cfg!(windows) {
                    let lower = name.to_ascii_lowercase();
                    if let Some(stem) = [".exe", ".cmd", ".bat", ".ps1"].iter().find_map(|ext| {
                        let e = *ext;
                        lower.strip_suffix(e).map(|_| name[..name.len() - e.len()].to_string())
                    }) {
                        commands.push(stem);
                    } else if !name.contains('.') && has_shebang(&path) {
                        commands.push(name);
                    }
                } else if !name.contains('.') {
                    commands.push(name);
                }
            }
        }
    }
    commands.sort();
    commands.dedup();
    commands
}

/// Regenerate the shim set from the active tools' deploy dirs.
///
/// - Removes shims listed in the manifest that no active tool provides in any
///   form (never touches files a user placed by hand — they aren't in the
///   manifest);
/// - writes one forwarder per active command name (see [`commands_in`]);
/// - copies the private helper binary under `shims/<HELPER_DIR>/` — every
///   entry is a forwarder now, so the helper is always required;
/// - records the new set in the manifest. Idempotent: re-running overwrites
///   same-name shims only.
pub fn rehash(home: &Path) -> Result<RehashReport, UError> {
    let shims = home.join("shims");

    // Target set: every command name any active deployed version provides
    let target = active_command_names(home);

    // Stale cleanup is manifest-driven; the manifest does not track the
    // helper dir, so it is safe to leave it untouched here.
    let previous = load_manifest(&shims);
    let removed: Vec<String> =
        previous.generated.iter().filter(|n| !target.contains(n)).cloned().collect();
    for name in &removed {
        let _ = fs::remove_file(shims.join(name));
    }

    // Script entries are forwarders too, so the helper is never optional.
    let helper = install_helper(&shims, home)?;
    let mut generated = Vec::new();
    for name in &target {
        write_forward_shim(&shims, &helper, name)?;
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

/// Generate one forwarder: a copy of the helper that forwards by its own file
/// name (the helper is never exposed on PATH under its real name).
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
    fn test_classify_terminal_matrix() {
        // POSIX signals win even when PowerShell is also hinted
        assert_eq!(
            classify_terminal(Some("msys"), None, None, Some("C:\\ps")),
            TerminalType::Posix
        );
        assert_eq!(classify_terminal(Some("linux"), None, None, None), TerminalType::Posix);
        assert_eq!(classify_terminal(Some("darwin"), None, None, None), TerminalType::Posix);
        assert_eq!(classify_terminal(Some("cygwin"), None, None, None), TerminalType::Posix);
        assert_eq!(classify_terminal(None, Some("MINGW64"), None, None), TerminalType::Posix);
        assert_eq!(classify_terminal(None, None, Some("/usr/bin/bash"), None), TerminalType::Posix);
        assert_eq!(
            classify_terminal(None, None, Some("/opt/homebrew/bin/zsh"), None),
            TerminalType::Posix
        );

        // Only PowerShell hinted
        assert_eq!(
            classify_terminal(None, None, None, Some("C:\\Windows\\System32\\Modules")),
            TerminalType::PowerShell
        );

        // Nothing hinted → cmd / GUI
        assert_eq!(classify_terminal(None, None, None, None), TerminalType::Other);
        assert_eq!(classify_terminal(None, None, Some(""), None), TerminalType::Other);
    }

    #[test]
    fn test_command_forms_per_terminal() {
        if !cfg!(windows) {
            assert_eq!(command_forms("npm", TerminalType::Other), vec!["npm".to_string()]);
            return;
        }
        assert_eq!(
            command_forms("npm", TerminalType::Posix),
            vec!["npm", "npm.cmd", "npm.bat", "npm.ps1", "npm.exe"]
        );
        assert_eq!(
            command_forms("npm", TerminalType::PowerShell),
            vec!["npm.ps1", "npm.cmd", "npm.bat", "npm.exe", "npm"]
        );
        assert_eq!(
            command_forms("npm", TerminalType::Other),
            vec!["npm.exe", "npm.cmd", "npm.bat", "npm.ps1", "npm"]
        );
    }

    #[test]
    fn test_has_shebang() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n);
        fs::write(p("shebang"), "#!/usr/bin/env sh\n").unwrap();
        fs::write(p("solo"), "#").unwrap();
        fs::write(p("empty"), "").unwrap();
        fs::write(p("plain"), "echo hi\n").unwrap();
        assert!(has_shebang(&p("shebang")));
        assert!(!has_shebang(&p("solo")));
        assert!(!has_shebang(&p("empty")));
        assert!(!has_shebang(&p("plain")));
        assert!(!has_shebang(&p("missing")));
    }

    #[test]
    fn test_command_of_shim_strips_exe() {
        if cfg!(windows) {
            assert_eq!(command_of_shim("node.exe"), "node");
            assert_eq!(command_of_shim("npm.exe"), "npm");
            assert_eq!(command_of_shim("make"), "make");
        } else {
            assert_eq!(command_of_shim("node"), "node");
        }
    }

    #[test]
    fn test_commands_in_collapses_script_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let version_dir = dir.path().join("node").join("24.21.0");
        fs::create_dir_all(version_dir.join("bin")).unwrap();
        // Multiple forms of one command collapse onto a single command
        fs::write(version_dir.join("node.exe"), b"x").unwrap();
        fs::write(version_dir.join("npm"), "#!/usr/bin/env sh\n").unwrap();
        fs::write(version_dir.join("npm.cmd"), b"@echo off\r\n").unwrap();
        fs::write(version_dir.join("npm.ps1"), b"Write-Output hi\r\n").unwrap();
        fs::write(version_dir.join("bin/corepack.cmd"), b"@echo off\r\n").unwrap();

        let mut commands = commands_in(&version_dir);
        commands.sort();
        if cfg!(windows) {
            assert_eq!(commands, vec!["corepack", "node", "npm"]);
        } else {
            // Unix: bare dot-free names only; node.exe/npm.cmd/npm.ps1 have
            // dots and are excluded → only the bare `npm` counts
            assert_eq!(commands, vec!["npm"]);
        }
    }

    #[test]
    fn test_commands_in_ignores_plain_files() {
        if !cfg!(windows) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let version_dir = dir.path().join("tool").join("1.0.0");
        fs::create_dir_all(&version_dir).unwrap();
        // No shebang → bare files and dotted docs are not commands
        fs::write(version_dir.join("LICENSE"), b"MIT").unwrap();
        fs::write(version_dir.join("README.md"), b"docs").unwrap();
        fs::write(version_dir.join("node.exe"), b"binary").unwrap();
        assert_eq!(commands_in(&version_dir), vec!["node"]);
    }

    #[test]
    fn test_locate_entry_selects_per_terminal() {
        if !cfg!(windows) {
            // Unix has only the bare form; any terminal finds it
            let dir = tempfile::tempdir().unwrap();
            let home = make_home_with(dir.path(), "node", "22.19.0", &["npm"]);
            let tools = home.join("tools/node/22.19.0");
            assert_eq!(locate_entry(&home, "npm", TerminalType::Other), Some(tools.join("npm")));
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe"]);
        let tools = home.join("tools").join("node").join("22.19.0");
        fs::write(tools.join("npm"), "#!/usr/bin/env sh\necho npm-sh\n").unwrap();
        fs::write(tools.join("npm.cmd"), "@echo off\r\necho npm-cmd\r\n").unwrap();
        fs::write(tools.join("npm.ps1"), "Write-Output 'npm-ps1'\r\n").unwrap();

        // PowerShell prefers the .ps1 entry
        assert_eq!(
            locate_entry(&home, "npm", TerminalType::PowerShell),
            Some(tools.join("npm.ps1"))
        );
        // cmd/GUI resolution prefers the .exe, then scripts
        assert_eq!(locate_entry(&home, "npm", TerminalType::Other), Some(tools.join("npm.cmd")));
        // git-bash prefers the bare shebang script
        assert_eq!(locate_entry(&home, "npm", TerminalType::Posix), Some(tools.join("npm")));
        // A bare file without a shebang is skipped, so Posix falls back
        fs::write(tools.join("make"), b"plain").unwrap();
        fs::write(tools.join("make.cmd"), b"@echo off\r\n").unwrap();
        // make.cmd inserted: ensure make resolves to make.cmd, and that a
        // no-shebang bare file alone would not resolve
        let home2 = {
            let d = tempfile::tempdir().unwrap();
            let h = make_home_with(d.path(), "node", "22.19.0", &[]);
            let t = h.join("tools/node/22.19.0");
            fs::create_dir_all(&t).unwrap();
            fs::write(t.join("make"), b"plain").unwrap();
            h
        };
        assert_eq!(locate_entry(&home2, "make", TerminalType::Posix), None);
        assert_eq!(locate_entry(&home, "make", TerminalType::Posix), Some(tools.join("make.cmd")));
    }

    #[test]
    fn test_locate_forward_target_hits_any_form() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "bin/npm.cmd"]);
        let tools = home.join("tools").join("node").join("22.19.0");

        if cfg!(windows) {
            assert_eq!(locate_forward_target(&home, "node.exe"), Some(tools.join("node.exe")));
            // health check reduces the shim name to the command and matches any form
            assert_eq!(locate_forward_target(&home, "npm.exe"), Some(tools.join("bin/npm.cmd")));
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
            // `go` here is a bare file without a shebang; on Windows a bare
            // form only counts when it is a shebang script, so it does not
            // resolve (a real Windows deploy ships `go.exe` instead)
            assert_eq!(locate_forward_target(&home, "go.exe"), None);
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

    fn seed_template(home: &Path) {
        fs::write(home.join(shim_binary_name()), b"template-binary").unwrap();
    }

    #[test]
    fn test_rehash_generates_shims_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let home = make_home_with(dir.path(), "node", "22.19.0", &["node.exe"]);
        seed_template(&home);
        if cfg!(windows) {
            fs::write(home.join("tools/node/22.19.0/npm.cmd"), b"@echo off\r\n").unwrap();
        }

        let report = rehash(&home).unwrap();
        assert!(!report.generated.is_empty());
        assert!(report.removed.is_empty());

        let shims = home.join("shims");
        // The helper binary landed in the private subdir
        assert!(shims.join(HELPER_DIR).join(shim_binary_name()).is_file());
        let expect: Vec<String> = if cfg!(windows) {
            vec!["node.exe".to_string(), "npm.exe".to_string()]
        } else {
            vec!["node".to_string()]
        };
        for name in &expect {
            assert_eq!(
                fs::read(shims.join(name)).unwrap(),
                fs::read(shims.join(HELPER_DIR).join(shim_binary_name())).unwrap(),
                "{name} is a helper forwarder copy"
            );
        }
        // The manifest records exactly the generated set, sorted
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

    /// The old script-entry scheme generated `npm.cmd` / `npm.ps1` shims; a
    /// re-run under the full-forward scheme must clean them and replace them
    /// with the single `npm.exe` forwarder.
    #[test]
    fn test_rehash_migrates_legacy_script_shims() {
        if !cfg!(windows) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home =
            make_home_with(dir.path(), "node", "22.19.0", &["node.exe", "npm.cmd", "npm.ps1"]);
        seed_template(&home);

        // Simulate the old manifest + generated script copies
        let shims = home.join("shims");
        fs::create_dir_all(shims.join(HELPER_DIR)).unwrap();
        fs::write(shims.join("npm.cmd"), b"@echo off\r\n").unwrap();
        fs::write(shims.join("npm.ps1"), b"Write-Output hi\r\n").unwrap();
        save_manifest(
            &shims,
            &ShimManifest {
                generated: vec!["node.exe".into(), "npm.cmd".into(), "npm.ps1".into()],
            },
        )
        .unwrap();

        let report = rehash(&home).unwrap();
        assert!(report.removed.contains(&"npm.cmd".to_string()));
        assert!(report.removed.contains(&"npm.ps1".to_string()));
        assert!(!shims.join("npm.cmd").exists());
        assert!(!shims.join("npm.ps1").exists());
        assert!(shims.join("npm.exe").is_file(), "single npm.exe forwarder now serves npm");
        let mut want = vec!["npm.exe".to_string(), "node.exe".to_string()];
        want.sort();
        assert_eq!(load_manifest(&shims).generated, want);
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
}
