//! `uvman-shim`: command-name forwarding shim.
//!
//! A copy of this binary sits in `<UVMAN_HOME>/shims/` under a command name
//! (e.g. `node.exe`), so GUI/IDE processes that inherit the Explorer
//! environment resolve uvman-managed tools through the stable `shims/` PATH
//! entry. It forwards by locating the same executable `which` would report.
//!
//! # Binary entries only
//!
//! This forwarder serves *executable* command entries (`.exe` on Windows, bare
//! names on Unix). Script entries (`.cmd` / `.bat` / `.ps1`) never get a
//! forwarder: `core::shims::rehash` copies the tool's own script into `shims/`
//! instead, because a bundled script resolves its siblings relative to its own
//! location (`%~dp0` / `$PSScriptRoot`) and a wrapper would break that. So the
//! lookup below never needs to find — or launch — a script.
//!
//! # Slimming constraint (keep this in mind when editing)
//!
//! This binary deliberately depends on **`std` only** — no `uvman` lib, no
//! clap, no serde/toml, no network/UI. Every dependency reachable from here is
//! copied into *every* shim (a tool set easily means ~30 copies), so a single
//! heavy edge multiplies across the whole shim dir. Before the slimming pass
//! the forwarder pulled `reqwest`/`hyper`/`tokio`/`toml`/`serde` in through
//! `core::error::UError` and `core::current`, costing ~1.9 MB per shim; the
//! `std`-only forwarder is a fraction of that.
//!
//! Consequences of that rule:
//! - the active-version table is scanned by a purpose-built reader instead of a
//!   TOML parser (`tool_current.toml` has a fixed, uvman-written shape);
//! - errors are reported as plain messages instead of going through `UError`.
//!
//! The shim stays behaviourally in step with `core::shims::locate_forward_target`
//! (which the main program and `doctor` use) — see `shim_target_matches_core`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Directory of the running executable.
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

fn user_home() -> PathBuf {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(key).map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

/// Home dir as seen from a shim copy.
///
/// Shims live at `<home>/shims/<name>`, so home is the grandparent of the
/// running executable. `UVMAN_HOME` wins on Windows (portable override); Unix
/// stays fixed at `~/.uvman` like the main program.
fn shim_home() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(p) = std::env::var("UVMAN_HOME") {
            return PathBuf::from(p);
        }
        if let Some(home) = exe_dir().and_then(|d| d.parent().map(|p| p.to_path_buf())) {
            return home;
        }
    }
    user_home().join(".uvman")
}

/// The command name this process is acting as: the copy's own file name
/// (`shims/node.exe` acts as `node.exe`). Script shims are verbatim copies of
/// the tool's script rather than this binary, so no override env var exists.
fn acting_shim_name() -> OsString {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(ToOwned::to_owned))
        .unwrap_or_default()
}

/// Strip the binary extension to get the bare command name (`node.exe` ->
/// `node`); on Unix the file name is already the bare command.
///
/// Only `.exe` is stripped: this binary is only ever copied to a forwarder
/// shim, and script entries (`.cmd`/`.bat`/`.ps1`) get a verbatim copy from
/// `rehash` instead of a forwarder, so they never rely on this fallback.
fn command_of_shim(file_name: &str) -> &str {
    if !cfg!(windows) {
        return file_name;
    }
    file_name.trim_end_matches(".exe")
}

/// First existing file among the platform candidates: version-dir root first,
/// then `bin/`.
///
/// Windows probes the deploy extension order because a deploy may ship a
/// command as an extensionless launcher; `core::executable` keeps the identical
/// list, and `shim_target_matches_core` guards the two against drift. Script
/// candidates (`.cmd`/`.bat`/`.ps1`) stay in the list only so the lookup keeps
/// matching `which`; they are never the forward target of a shim this binary
/// could have been copied to.
fn find_executable(version_dir: &Path, name: &str) -> Option<PathBuf> {
    let bin_dir = version_dir.join("bin");
    for dir in [version_dir, bin_dir.as_path()] {
        if cfg!(windows) {
            for ext in [".exe", ".cmd", ".bat", ".ps1"] {
                let candidate = dir.join(format!("{name}{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        } else {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Locate a same-named executable inside a version dir: root first, then
/// `bin/` (mirrors the PATH precedence `env` bakes in). Falls back to probing
/// the platform candidates of the stripped command name (so a `node.exe` shim
/// still lands on a deploy that ships the launcher extensionless), matching
/// the `which` rules.
fn find_named_executable(version_dir: &Path, file_name: &str) -> Option<PathBuf> {
    let bin_dir = version_dir.join("bin");
    for dir in [version_dir, bin_dir.as_path()] {
        let candidate = dir.join(file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let command = command_of_shim(file_name);
    if command != file_name {
        return find_executable(version_dir, command);
    }
    None
}

/// Read the active-version table into `(tool, version)` pairs, in file order.
///
/// `config/tool_current.toml` is written by uvman itself with the fixed shape
/// `[<tool>]\nversion = "<version>"`, so a line scan covers it without a TOML
/// parser. Anything unrecognised (including a corrupt file) is skipped — the
/// reader stays permissive, exactly like the main program's `current::load`.
fn active_versions(text: &str) -> impl Iterator<Item = (&str, &str)> {
    let mut tool = "";
    text.lines().filter_map(move |raw| {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        if let Some(rest) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            tool = rest.trim().trim_matches('"');
            return None;
        }
        let (key, value) = line.split_once('=')?;
        if key.trim() != "version" {
            return None;
        }
        Some((tool, value.trim().trim_matches('"')))
    })
}

/// Resolve the forward target for a shim invocation.
///
/// The active-tool table is matched against the shim file name: for each active
/// tool whose version dir is deployed, look for a same-named executable. None
/// when no active tool provides the command — the shim then reports an
/// actionable error instead of silently doing nothing.
///
/// A tool whose active version is not deployed is skipped, not fatal, so one
/// hand-deleted version dir cannot hide a later tool that does provide the
/// command (same rule as `core::shims::locate_deploy_source`).
fn locate_forward_target(home: &Path, shim_file_name: &str) -> Option<PathBuf> {
    let path = home.join("config").join("tool_current.toml");
    let text = std::fs::read_to_string(path).ok()?;
    let tools_root = home.join("tools");
    for (tool, version) in active_versions(&text) {
        let version_dir = tools_root.join(tool).join(version);
        if !version_dir.is_dir() {
            continue;
        }
        if let Some(target) = find_named_executable(&version_dir, shim_file_name) {
            return Some(target);
        }
    }
    None
}

/// Spawn the resolved target and forward stdio + exit code.
///
/// The target is a binary entry (`node.exe`, or a bare name on Unix), so it is
/// always executed directly: the OS runs it on both platforms, and on Unix a
/// shebang script needs no interpreter either. Script entries never reach this
/// point — `rehash` copies those into `shims/` instead of installing a
/// forwarder, so there is no interpreter indirection to perform here.
fn forward(target: &Path) -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let status = match Command::new(target).args(&args).status() {
        Ok(status) => status,
        Err(source) => {
            // Plain message: routing this through `UError` would link the whole
            // error tree (reqwest/toml/serde) into every shim copy.
            eprintln!("uvman: failed to run {}: {source}", target.display());
            return ExitCode::FAILURE;
        },
    };
    // Exit-code passthrough: a signal-killed child falls back to 1
    match status.code() {
        Some(code) => ExitCode::from(code as u8),
        None => ExitCode::FAILURE,
    }
}

fn main() -> ExitCode {
    let shim_name = acting_shim_name();
    let shim_name = shim_name.to_string_lossy().into_owned();
    let home = shim_home();

    match locate_forward_target(&home, &shim_name) {
        Some(target) => forward(&target),
        None => {
            eprintln!("uvman: no actively managed tool provides '{shim_name}'");
            eprintln!(
                "hint: activate an installed version with `uvman use <tool>@<version>`, \
                 or install one first with `uvman install <tool>@<version>`"
            );
            ExitCode::FAILURE
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader must cover the exact shape `current::set_current_at` writes.
    #[test]
    fn test_active_versions_reads_canonical_table() {
        let text = "[node]\nversion = \"22.19.0\"\n\n[go]\nversion = \"1.23.0\"\n";
        let pairs: Vec<_> = active_versions(text).collect();
        assert_eq!(pairs, vec![("node", "22.19.0"), ("go", "1.23.0")]);
    }

    /// Whitespace, comments and unrelated keys must not break the scan.
    #[test]
    fn test_active_versions_tolerates_comments_and_other_keys() {
        let text = "# managed by uvman\n[node]\nversion = \"22.19.0\"\nscope = \"global\"\n";
        let pairs: Vec<_> = active_versions(text).collect();
        assert_eq!(pairs, vec![("node", "22.19.0")]);
    }

    #[test]
    fn test_active_versions_empty_on_corrupt_input() {
        assert_eq!(active_versions("not = [valid").count(), 0);
        assert_eq!(active_versions("").count(), 0);
    }

    /// Windows deploy-extension stripping; Unix names are already bare.
    #[test]
    fn test_command_of_shim_names() {
        if cfg!(windows) {
            assert_eq!(command_of_shim("node.exe"), "node");
            // Script names never reach a forwarder, but stripping must still be
            // a no-op-ish pass so the candidate probe stays sane
            assert_eq!(command_of_shim("npm.cmd"), "npm.cmd");
        } else {
            assert_eq!(command_of_shim("node"), "node");
        }
    }

    /// End-to-end: a deployed active version resolves; an absent one does not.
    #[test]
    fn test_locate_forward_target_hits_deployed_version_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let version_dir = home.join("tools").join("node").join("22.19.0");
        std::fs::create_dir_all(version_dir.join("bin")).unwrap();
        let name = if cfg!(windows) { "node.exe" } else { "node" };
        std::fs::write(version_dir.join(name), b"binary").unwrap();
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::write(
            home.join("config").join("tool_current.toml"),
            "[node]\nversion = \"22.19.0\"\n",
        )
        .unwrap();

        assert_eq!(
            locate_forward_target(home, name),
            Some(version_dir.join(name)),
            "shim must land on the deployed binary"
        );

        // A command no active tool provides resolves to none (actionable error)
        let other = if cfg!(windows) { "python.exe" } else { "python" };
        assert_eq!(locate_forward_target(home, other), None);
    }

    /// No table file at all -> nothing to forward to, never a panic.
    #[test]
    fn test_locate_forward_target_without_table() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(locate_forward_target(dir.path(), "node"), None);
    }

    /// A `node.exe` shim must land on the deploy's actual launcher even when
    /// the file name differs, matching `which`'s probe order: the exact name
    /// misses, so the stripped command name is probed though the extension
    /// ladder.
    #[test]
    fn test_locate_forward_target_strips_extension_to_probe() {
        if !cfg!(windows) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let version_dir = home.join("tools").join("node").join("22.19.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        // No `node.exe` deployed; the command lives under another extension
        std::fs::write(version_dir.join("node.cmd"), b"@echo off\r\n").unwrap();
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::write(
            home.join("config").join("tool_current.toml"),
            "[node]\nversion = \"22.19.0\"\n",
        )
        .unwrap();

        assert_eq!(locate_forward_target(home, "node.exe"), Some(version_dir.join("node.cmd")));
    }

    /// A tool whose active version dir is missing must not hide a later tool
    /// (the bug the lib-side scan had): the search continues, it does not bail.
    #[test]
    fn test_locate_forward_target_continues_past_undeployed_tool() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let version_dir = home.join("tools").join("python").join("3.13.1");
        std::fs::create_dir_all(&version_dir).unwrap();
        let name = if cfg!(windows) { "python.exe" } else { "python" };
        std::fs::write(version_dir.join(name), b"binary").unwrap();
        std::fs::create_dir_all(home.join("config")).unwrap();
        // `node` sorts first and has no version dir on disk at all
        std::fs::write(
            home.join("config").join("tool_current.toml"),
            "[node]\nversion = \"22.19.0\"\n\n[python]\nversion = \"3.13.1\"\n",
        )
        .unwrap();

        assert_eq!(locate_forward_target(home, name), Some(version_dir.join(name)));
    }

    /// Drift guard: the shim carries its own `std`-only copy of the lookup, so
    /// it must agree with the lib copy that `doctor` / `shims` use.
    #[test]
    fn shim_target_matches_core() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let version_dir = home.join("tools").join("node").join("22.19.0");
        std::fs::create_dir_all(version_dir.join("bin")).unwrap();
        // Binary entries only — script entries are copied by rehash and never
        // forwarded, so they are not part of this contract
        let (node, npm) = if cfg!(windows) { ("node.exe", "npm.exe") } else { ("node", "npm") };
        std::fs::write(version_dir.join(node), b"binary").unwrap();
        std::fs::write(version_dir.join("bin").join(npm), b"binary").unwrap();
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::write(
            home.join("config").join("tool_current.toml"),
            "[node]\nversion = \"22.19.0\"\n",
        )
        .unwrap();

        for shim in [node, npm] {
            assert_eq!(
                locate_forward_target(home, shim),
                uvman::core::shims::locate_forward_target(home, shim),
                "shim and core must resolve {shim} identically"
            );
        }
    }
}
