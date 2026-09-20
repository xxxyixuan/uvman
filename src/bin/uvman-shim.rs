//! `uvman-shim`: command-name forwarding shim.
//!
//! A copy of this binary sits in `<UVMAN_HOME>/shims/` under a command name
//! (e.g. `node.exe`), so GUI/IDE processes that inherit the Explorer
//! environment resolve uvman-managed tools through the stable `shims/` PATH
//! entry. It forwards by locating the same executable `which` would report.
//!
//! # Every entry is a forwarder
//!
//! There are no script copies anymore: `.cmd` / `.bat` / `.ps1` and bare
//! shebang scripts all get a forwarder. When the tool ships several file forms
//! for one command (`npm`, `npm.cmd`, `npm.ps1`), the forwarder picks the form
//! the invoking terminal would pick natively (git-bash prefers the bare
//! script, PowerShell the `.ps1`, cmd/GUI the `.exe`/`.cmd`), then runs it via
//! the matching interpreter (`cmd.exe /c`, `pwsh -File`, a `sh`-family shell).
//! The script executes inside its deploy dir, so its own self-relative
//! references (`%~dp0`, `$PSScriptRoot`, `$basedir`, …) resolve natively —
//! nothing is rewritten.
//!
//! On Unix there is no script/executable split — a command is a bare name, and
//! a `#!/bin/sh` script is one of them. Unix therefore *does* forwarder script
//! targets too: shebang scripts run through the kernel, extension scripts
//! (`.sh`/`.zsh`/`.fish`) get their interpreter.
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
//! The shim stays behaviourally in step with `core::shims::locate_entry`
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
/// (`shims/npm.exe` acts as `npm`). Every shim is a copy of this binary
/// (renamed for its command), so the copy's own name suffices; `UVMAN_SHIM_AS`
/// overrides it for tests and manual debugging, never used in a shipped
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

/// Strip the shim's own `.exe` (Windows shims are generated `{command}.exe`)
/// to get the bare command name; on Unix the file name is already bare.
fn command_of_shim(file_name: &str) -> &str {
    if !cfg!(windows) {
        return file_name;
    }
    file_name.strip_suffix(".exe").unwrap_or(file_name)
}

/// Terminal context the shim is called from, mirroring
/// `core::shims::classify_terminal` (std-only twin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalType {
    Posix,
    PowerShell,
    Other,
}

fn classify_terminal(
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

fn detect_terminal() -> TerminalType {
    classify_terminal(
        std::env::var("OSTYPE").ok().as_deref(),
        std::env::var("MSYSTEM").ok().as_deref(),
        std::env::var("SHELL").ok().as_deref(),
        std::env::var("PSModulePath").ok().as_deref(),
    )
}

/// Whether a file starts with a `#!` shebang (std-only, two-byte peek).
fn has_shebang(path: &Path) -> bool {
    use std::io::Read;
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let mut head = [0u8; 2];
    file.read_exact(&mut head).is_ok() && head == [b'#', b'!']
}

/// Deployment file forms to probe for `command` in `terminal`'s native order
/// (twin of `core::shims::command_forms`; Windows only — Unix is a bare name).
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

/// A candidate is usable when it exists and, on Windows, a bare
/// (no-extension) form only counts as a shebang script — never a stray text
/// file (twin of `core::shims::form_usable`).
fn form_usable(candidate: &Path, form: &str) -> bool {
    if cfg!(windows) && !form.contains('.') {
        return has_shebang(candidate);
    }
    true
}

/// Resolve the deploy entry for `command` under a terminal's preferred form
/// order: each active tool's version dir (root first, then `bin/`), first
/// usable form wins. A tool whose active version is not deployed is skipped,
/// not fatal (twin of `core::shims::locate_entry`).
fn locate_entry(home: &Path, command: &str, terminal: TerminalType) -> Option<PathBuf> {
    let path = home.join("config").join("tool_current.toml");
    let text = std::fs::read_to_string(path).ok()?;
    let tools_root = home.join("tools");
    for (tool, version) in active_versions(&text) {
        let version_dir = tools_root.join(tool).join(version);
        if !version_dir.is_dir() {
            continue;
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
/// The shim's own file name (e.g. `npm.exe`) is reduced to the command name
/// and resolved to the deploy entry in the invoking terminal's preferred form
/// (see [`classify_terminal`]). None when no active tool provides the command
/// in any usable form — the shim then reports an actionable error instead of
/// silently doing nothing.
fn locate_forward_target(home: &Path, shim_file_name: &str) -> Option<PathBuf> {
    let command = command_of_shim(shim_file_name);
    locate_entry(home, command, detect_terminal())
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

/// The sh-family interpreter to run a bare shebang script with: `sh` when
/// present, else `bash` (git-bash/MSYS setups), else `sh` anyway — the spawn
/// failure surfaces as an actionable error from `forward`.
#[cfg(windows)]
fn shell_interpreter() -> &'static str {
    let program = |name: &str| {
        std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path)
                .any(|dir| dir.join(name).is_file() || dir.join(format!("{name}.exe")).is_file())
        })
    };
    if program("sh") {
        "sh"
    } else if program("bash") {
        "bash"
    } else {
        "sh"
    }
}

/// Build the command that runs `target` with the shim's own argv.
///
/// Windows routes scripts through their interpreters: `.cmd`/`.bat` via
/// `cmd.exe /c`, `.ps1` via `pwsh` (fallback `powershell.exe`) with
/// `-NoProfile -ExecutionPolicy Bypass -File`, and a bare file with a shebang
/// through a `sh`-family shell (git-bash/MSYS provide one). A binary or a bare
/// file without a shebang runs directly. Unix: extension scripts get their
/// interpreter, everything else runs as itself.
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
            "" if has_shebang(target) => {
                // Bare shebang script: the kernel won't interpret it, so run
                // it through the shell interpreter that is available
                let mut cmd = Command::new(shell_interpreter());
                cmd.arg(target);
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
/// for script targets lives in [`build_command`].
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
            assert_eq!(command_of_shim("npm.exe"), "npm");
            // Already-bare names pass through
            assert_eq!(command_of_shim("make"), "make");
        } else {
            assert_eq!(command_of_shim("node"), "node");
        }
    }

    /// Terminal classification mirrors the core rule (Posix beats PowerShell
    /// when both are hinted).
    #[test]
    fn test_classify_terminal_rule() {
        assert_eq!(
            classify_terminal(Some("msys"), None, None, Some("C:\\ps")),
            TerminalType::Posix
        );
        assert_eq!(classify_terminal(None, Some("MINGW64"), None, None), TerminalType::Posix);
        assert_eq!(classify_terminal(None, None, Some("/bin/sh"), None), TerminalType::Posix);
        assert_eq!(
            classify_terminal(None, None, None, Some("C:\\Windows\\Modules")),
            TerminalType::PowerShell
        );
        assert_eq!(classify_terminal(None, None, None, None), TerminalType::Other);
    }

    /// Windows-only: the command form selection for a multi-form command must
    /// respect the terminal (bare for posix, .ps1 for PowerShell, .cmd for
    /// cmd/GUI).
    #[test]
    fn test_locate_entry_selects_per_terminal() {
        if !cfg!(windows) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let version_dir = home.join("tools").join("node").join("22.19.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::fs::write(version_dir.join("npm"), "#!/usr/bin/env sh\necho bare\n").unwrap();
        std::fs::write(version_dir.join("npm.cmd"), "@echo off\r\necho cmd\r\n").unwrap();
        std::fs::write(version_dir.join("npm.ps1"), "Write-Output 'ps1'\r\n").unwrap();
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::write(
            home.join("config").join("tool_current.toml"),
            "[node]\nversion = \"22.19.0\"\n",
        )
        .unwrap();

        assert_eq!(locate_entry(home, "npm", TerminalType::Posix), Some(version_dir.join("npm")));
        assert_eq!(
            locate_entry(home, "npm", TerminalType::PowerShell),
            Some(version_dir.join("npm.ps1"))
        );
        assert_eq!(
            locate_entry(home, "npm", TerminalType::Other),
            Some(version_dir.join("npm.cmd"))
        );
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

    /// Windows: a bare file with a shebang is run through a sh-family shell, a
    /// bare file without one is executed directly.
    #[cfg(windows)]
    #[test]
    fn test_build_command_windows_bare_shebang() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("tool");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        let cmd = build_command(&script);
        let program = cmd.get_program().to_string_lossy().to_lowercase();
        assert!(
            program == "sh" || program == "bash",
            "bare shebang runs through sh-family shell, got {program}"
        );

        let plain = dir.path().join("plain");
        std::fs::write(&plain, "not a script").unwrap();
        let cmd = build_command(&plain);
        assert_eq!(
            cmd.get_program().to_string_lossy().to_lowercase(),
            plain.to_string_lossy().to_lowercase()
        );
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
    /// it must agree with the lib copy that `doctor` / `shims` use — for every
    /// terminal, a multi-form command must resolve to the same concrete file.
    #[test]
    fn shim_target_matches_core() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let version_dir = home.join("tools").join("node").join("22.19.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        if cfg!(windows) {
            std::fs::write(version_dir.join("node.exe"), b"binary").unwrap();
            std::fs::write(version_dir.join("npm"), "#!/usr/bin/env sh\n").unwrap();
            std::fs::write(version_dir.join("npm.cmd"), b"@echo off\r\n").unwrap();
            std::fs::write(version_dir.join("npm.ps1"), b"Write-Output hi\r\n").unwrap();
        } else {
            std::fs::write(version_dir.join("node"), b"binary").unwrap();
            std::fs::write(version_dir.join("npm"), "#!/usr/bin/env sh\n").unwrap();
        }
        std::fs::create_dir_all(home.join("config")).unwrap();
        std::fs::write(
            home.join("config").join("tool_current.toml"),
            "[node]\nversion = \"22.19.0\"\n",
        )
        .unwrap();

        for terminal in [TerminalType::Posix, TerminalType::PowerShell, TerminalType::Other] {
            let shim_pick = locate_entry(home, &command_of_shim("npm.exe"), terminal);
            let core_pick = uvman::core::shims::locate_entry(
                home,
                "npm",
                match terminal {
                    TerminalType::Posix => uvman::core::shims::TerminalType::Posix,
                    TerminalType::PowerShell => uvman::core::shims::TerminalType::PowerShell,
                    TerminalType::Other => uvman::core::shims::TerminalType::Other,
                },
            );
            assert_eq!(shim_pick, core_pick, "shim and core diverge for {terminal:?}");
        }
    }
}
