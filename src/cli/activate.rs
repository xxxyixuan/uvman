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
///              （管道逐行喂 iex 会拆散多行函数定义，必须先 Out-String 合并）
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

        // 烘焙绝对路径：脚本可能在任意 CWD 下求值
        let state = absolute(tool_current_path());
        let tools = absolute(tools_dir());

        let script = shell
            .activation_script(&state, &tools)
            .map_err(UError::SimpleError)?;
        print!("{script}");
        Ok(())
    }
}
