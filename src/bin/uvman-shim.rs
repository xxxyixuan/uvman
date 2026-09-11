//! `uvman-shim`: forwarding shim binary (plan 0.3.0 task 1).
//!
//! A copy of this binary sits in `<UVMAN_HOME>/shims/` under the command name
//! (e.g. `node.exe`), so GUI/IDE processes that inherit the Explorer
//! environment resolve uvman-managed tools through the stable `shims/` PATH
//! entry. It shares only `core` with the main program — no clap, no network,
//! no UI — and forwards by locating the same executable `which` would report.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitCode};

use uvman::core::error::UError;
use uvman::core::paths;
use uvman::core::shims;

fn main() -> ExitCode {
    let shim_name = shims::acting_shim_name();
    let shim_name = shim_name.to_string_lossy().into_owned();
    let home = paths::shim_home();

    match shims::locate_forward_target(&home, &shim_name) {
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

/// Spawn the resolved target and forward stdio + exit code.
///
/// `.cmd`/`.bat` targets are launched through `cmd /c` and `.ps1` through
/// PowerShell (matching how Windows actually executes them); everything else
/// — including bare-name binaries on Unix — runs directly.
fn forward(target: &Path) -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut command = launch_command(target);
    let status = match command.args(&args).status() {
        Ok(status) => status,
        Err(source) => {
            let err = UError::FileError { path: target.to_path_buf(), source };
            eprintln!("uvman: failed to run {}: {err}", target.display());
            return ExitCode::FAILURE;
        },
    };
    // Exit-code passthrough: a signal-killed child falls back to 1
    match status.code() {
        Some(code) => ExitCode::from(code as u8),
        None => ExitCode::FAILURE,
    }
}

/// Build the command that actually executes the target file.
fn launch_command(target: &Path) -> Command {
    let ext = target.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    if cfg!(windows) {
        match ext.as_str() {
            // Scripts are executed by their interpreter; CreateProcess alone
            // cannot run them (no file association on the raw name)
            "cmd" | "bat" => {
                let mut cmd = Command::new("cmd");
                cmd.arg("/c").arg(target);
                cmd
            },
            "ps1" => {
                let mut cmd = Command::new("powershell");
                cmd.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]).arg(target);
                cmd
            },
            _ => Command::new(target),
        }
    } else {
        // Unix: the bare file may be a script with a shebang — exec directly
        Command::new(target)
    }
}
