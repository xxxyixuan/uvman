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
///   bash/zsh:  eval "$(uvman activate)"
///   fish:      uvman activate | source
///   pwsh:      uvman activate | Out-String | Invoke-Expression
///              (Out-String first, or iex splits multi-line function defs)
///
/// cmd is not supported (no prompt hook): use AutoRun startup injection
/// or the manual refresh command instead.
#[derive(Debug, clap::Args)]
pub struct Activate {
    /// Target shell syntax; defaults to auto-detection
    #[clap(short = 's', long, value_enum)]
    pub shell: Option<Shell>,
}

impl Activate {
    pub fn run(&self) -> Result<()> {
        let shell = self.shell.unwrap_or_else(Shell::detect);
        if shell == Shell::Cmd {
            return Err(UError::SimpleError(
                "cmd has no prompt hook to auto-refresh; use AutoRun startup \
                 injection or run the manual refresh command (shims will \
                 cover this in the future)"
                    .into(),
            )
            .into());
        }

        // Bake absolute paths: the script may be evaluated from any CWD
        let state = absolute(tool_current_path());
        let tools = absolute(tools_dir());

        let script = shell.activation_script(&state, &tools).map_err(UError::SimpleError)?;
        print!("{script}");
        Ok(())
    }
}
