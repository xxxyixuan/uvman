//! `uvman-shim`: command-name forwarding shim.
//!
//! A copy of this binary sits in `<UVMAN_HOME>/shims/` under a command name
//! (e.g. `node.exe`), so GUI/IDE processes that inherit the Explorer
//! environment resolve uvman-managed tools through the stable `shims/` PATH
//! entry. It forwards by locating the same executable `which` would report.
//!
//! # Binary entries only
//!
//! On Windows this forwarder serves *executable* command entries (`.exe`).
//! Script entries (`.cmd` / `.bat` / `.ps1`) never get a forwarder:
//! `core::shims::rehash` writes the tool's own script into `shims/` with its
//! `%~dp0` / `$PSScriptRoot` references rebased onto the deploy dir, so the
//! interpreter reads a real script that still finds its siblings.
//!
//! On Unix there is no script/executable split — a command is a bare name, and
//! a `#!/bin/sh` script is one of them. Unix therefore *does* forwarder script
//! targets too, and the OS handles the shebang itself.
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
/// (`shims/node.exe` acts as `node.exe`). Windows script shims are copies of
/// the tool's own script rather than this binary, so no override env var
/// exists; Unix *does* forward script targets, so a shim copied to a name that
/// no longer matches the resolved deploy entry can be pointed at another
/// command with `UVMAN_SHIM_AS` — a development aid, never used in a shipped
/// layout.
fn acting_shim_name() -> OsString {
    match std::env::var("UVMAN_SHIM_AS") {
        Ok(name) if !name.is_empty() => OsString::from(name),
        _ => own_file_name(),
    }
}

/// The running executable's own file name
fn own_file_name() -> OsString {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(ToOwned::to_owned))
        .unwrap_or_default()
}

/// Strip the binary extension to get the bare command name (`node.exe` ->
/// `node`); on Unix the file name is already the bare command.
///
/// `.exe` is the only shimmable binary extension on Windows, so stripping the
/// other Windows suffixes would change nothing: `.cmd`/`.bat`/`.ps1` names
/// classify as scripts and are copied-and-rebased by `rehash` instead of
/// being forwarded.
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
/// matching `which`; a Windows shim this binary was copied to never has a
/// script name (scripts are copied instead), so the ladder is only ever walked
/// for extensionless Windows commands.
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

/// The interpreter to run a non-native script target with (Unix only).
///
/// Unix has no script/executable split in the shim namespace: a `#!/bin/sh`
/// command reaches this forwarder like any other. A shebang script runs by
/// itself through the kernel, so the forwarder only needs to step in for the
/// two shapes the kernel cannot start: an extension-tagged script
/// (`.sh`/`.bash`/`.zsh`/`.fish`) and a file with no shebang at all.
/// Everything else — a real binary, or any shebang script — is executed
/// directly and lets the kernel do the work.
#[cfg(not(windows))]
fn interpreter_for(target: &Path) -> Option<&'static str> {
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".sh") || lower.ends_with(".bash") {
        return Some("sh");
    }
    if lower.ends_with(".zsh") {
        return Some("zsh");
    }
    if lower.ends_with(".fish") {
        return Some("fish");
    }
    // No extension: a binary (executed directly) or a shebang script (the
    // kernel resolves the interpreter) — both need no indirection here
    None
}

/// Build the command that runs `target` with the shim's own argv.
///
/// Windows never reaches the script cases here: `.cmd`/`.bat`/`.ps1` entries
/// are copied-and-rebased into `shims/` by `rehash` and read by their own
/// interpreter, while a forwarder copy always acts as a binary command. The
/// `cmd.exe` / PowerShell branches therefore exist for completeness and for a
/// hand-copied shim; they keep the argument with the script's own path so the
/// interpreter receives it as `%0` / `$MyInvocation`.
fn build_command(target: &Path) -> Command {
    #[cfg(windows)]
    {
        let ext = target.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "cmd" | "bat" => {
                let mut cmd = Command::new("cmd.exe");
                cmd.arg("/c").arg(target);
                cmd
            },
            "ps1" => {
                let mut cmd = Command::new(powershell());
                cmd.arg("-NoProfile")
                    .arg("-ExecutionPolicy")
                    .arg("Bypass")
                    .arg("-File")
                    .arg(target);
                cmd
            },
            _ => Command::new(target),
        }
    }
    #[cfg(not(windows))]
    {
        match interpreter_for(target) {
            Some(interpreter) => {
                let mut cmd = Command::new(interpreter);
                cmd.arg(target);
                cmd
            },
            None => Command::new(target),
        }
    }
}

/// PowerShell executable to run a `.ps1` target with: `pwsh` (7+) when it is
/// on PATH, the Windows PowerShell the OS ships otherwise.
#[cfg(windows)]
fn powershell() -> OsString {
    let has_pwsh = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|dir| dir.join("pwsh.exe").is_file()))
        .unwrap_or(false);
    if has_pwsh { OsString::from("pwsh") } else { OsString::from("powershell.exe") }
}

/// Spawn the resolved target and forward stdio + exit code.
///
/// A binary target is executed directly on both platforms. The command shape
/// for script targets lives in [`build_command`] — on Windows it never applies
/// to a shim this program was copied to, on Unix it is the shebang/no-shebang
/// indirection the kernel cannot perform itself.
fn forward(target: &Path) -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let status = match build_command(target).args(&args).status() {
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

    /// Unix only: the two shapes the kernel cannot start get an interpreter,
    /// a real binary or a shebang script gets none.
    #[cfg(not(windows))]
    #[test]
    fn test_interpreter_for_unix_shapes() {
        assert_eq!(interpreter_for(Path::new("/x/run.sh")), Some("sh"));
        assert_eq!(interpreter_for(Path::new("/x/run.BASH")), Some("sh"));
        assert_eq!(interpreter_for(Path::new("/x/run.ZSH")), Some("zsh"));
        assert_eq!(interpreter_for(Path::new("/x/run.fish")), Some("fish"));
        // Bare and shebang names are executed directly
        assert_eq!(interpreter_for(Path::new("/x/node")), None);
        assert_eq!(interpreter_for(Path::new("/x/activate")), None);
    }

    /// Unix only: an extension script is handed to its interpreter with the
    /// script path as the first argument, a binary is spawned as itself.
    #[cfg(not(windows))]
    #[test]
    fn test_build_command_unix_script_indirection() {
        let cmd = build_command(Path::new("/x/deploy.sh"));
        assert_eq!(cmd.get_program().to_string_lossy(), "sh");
        let argv: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(argv, vec!["/x/deploy.sh"]);

        let cmd = build_command(Path::new("/x/node"));
        assert_eq!(cmd.get_program().to_string_lossy(), "/x/node");
        assert_eq!(cmd.get_args().count(), 0);
    }

    /// A binary target is spawned as itself on every platform.
    #[test]
    fn test_build_command_direct_for_binary() {
        let target = if cfg!(windows) { Path::new(r"C:\x\node.exe") } else { Path::new("/x/node") };
        let cmd = build_command(target);
        assert_eq!(cmd.get_program().to_string_lossy(), target.to_string_lossy());
        assert_eq!(cmd.get_args().count(), 0, "argv is appended by the caller");
    }

    /// Windows: a script target is handed to its own interpreter, with the
    /// script path as the interpreter's target argument.
    #[cfg(windows)]
    #[test]
    fn test_build_command_windows_script_indirection() {
        let cmd = build_command(Path::new(r"C:\x\build.cmd"));
        assert_eq!(cmd.get_program().to_string_lossy().to_lowercase(), "cmd.exe");
        let argv: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(argv, vec!["/c", r"C:\x\build.cmd"]);

        let cmd = build_command(Path::new(r"C:\x\build.ps1"));
        let argv: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(argv.contains(&"-File".to_string()), "pwsh gets -File: {argv:?}");
        assert_eq!(argv.last().map(String::as_str), Some(r"C:\x\build.ps1"));
        // Bypass so a machine execution policy can't break tool installs
        assert!(argv.contains(&"Bypass".to_string()));
    }

    /// The `UVMAN_SHIM_AS` override wins over the copy's own file name (used by
    /// tests and manual debugging).
    #[test]
    fn test_acting_shim_name_env_override() {
        unsafe { std::env::set_var("UVMAN_SHIM_AS", "node.exe") };
        assert_eq!(acting_shim_name().to_string_lossy(), "node.exe");
        unsafe { std::env::remove_var("UVMAN_SHIM_AS") };
        // With no override the running test binary's own name is used, which
        // is never the empty string on a real process
        assert!(!acting_shim_name().is_empty());
    }

    /// Drift guard: the shim carries its own `std`-only copy of the lookup, so
    /// it must agree with the lib copy that `doctor` / `shims` use.
    #[test]
    fn shim_target_matches_core() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let version_dir = home.join("tools").join("node").join("22.19.0");
        std::fs::create_dir_all(version_dir.join("bin")).unwrap();
        // Binary entries only — Windows script entries are copied by rehash
        // and never forwarded, so they are not part of this contract
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
