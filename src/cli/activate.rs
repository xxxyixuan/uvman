use crate::Result;
use crate::core::error::UError;
use crate::core::paths::{absolute, tool_current_path, tools_dir};
use crate::core::shell::Shell;

/// Print an activation script that keeps the shell's uvman env in sync.
///
/// The script registers a prompt hook (mtime fast-path) that re-evaluates
/// `uvman env` whenever the state file changes, so `uvman use` applies on
/// the next prompt without manual refresh (mise-style activate).
///
/// Persist in your shell config (put it last, so a custom prompt function
/// is not overwritten):
///
///   bash/zsh:  eval "$(uvman activate)"
///   fish:      uvman activate | source
///   pwsh:      uvman activate | Out-String | Invoke-Expression
///              (Out-String first, or iex splits multi-line function defs)
///
/// cmd is not supported (no prompt hook): register `uvman env --shell cmd`
/// under the AutoRun registry value to apply at startup, or run it manually
/// after each switch.
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment)]
pub struct Activate {
    /// Target shell syntax; defaults to auto-detection
    #[clap(short = 's', long, value_enum)]
    pub shell: Option<Shell>,
}

impl Activate {
    pub fn run(&self) -> Result<()> {
        let script = build_script(self.shell.unwrap_or_else(Shell::detect))?;
        print!("{script}");
        Ok(())
    }
}

/// Build the activation script for the target shell. Paths are baked absolute:
/// the script may be evaluated from any CWD.
fn build_script(shell: Shell) -> Result<String> {
    if shell == Shell::Cmd {
        return Err(UError::SimpleError(
            "cmd has no prompt hook to auto-refresh; run `uvman env --shell cmd` \
             after each switch, or register it under the AutoRun registry value \
             to apply at startup"
                .into(),
        )
        .into());
    }

    let state = absolute(tool_current_path());
    let tools = absolute(tools_dir());
    let script = shell.activation_script(&state, &tools).map_err(UError::SimpleError)?;
    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cmd_rejected_with_actionable_alternatives() {
        let err = build_script(Shell::Cmd).unwrap_err().to_string();
        assert!(err.contains("AutoRun"), "names the startup path: {err}");
        assert!(err.contains("uvman env"), "names the manual refresh: {err}");
        // Shims were abandoned as a cross-platform design; don't promise them
        assert!(!err.to_lowercase().contains("shim"), "stale shim guidance: {err}");
    }

    #[test]
    fn test_bash_script_bakes_absolute_paths_and_env_call() {
        let script = build_script(Shell::Bash).unwrap();
        // The test home is the relative `test/` dir; baking must anchor it to CWD
        let cwd = std::env::current_dir().unwrap().to_string_lossy().replace('\\', "/");
        assert!(
            script.contains(&format!("{cwd}/test/config/tool_current.toml")),
            "absolute state path baked: {script}"
        );
        assert!(script.contains("uvman env --shell bash"));
        // Marker consumed by is_activated() in hook-driven code paths
        assert!(script.contains("UVMAN_SHELL=bash"));
    }
}
