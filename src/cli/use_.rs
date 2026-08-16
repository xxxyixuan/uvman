use crate::Result;
use crate::core::current;
use crate::ui::style;

use super::install::parse_spec;

/// Switch the currently used version of a tool
#[derive(Debug, clap::Args)]
#[clap(verbatim_doc_comment, visible_alias = "u")]
pub struct Use {
    /// Tool and version to use, in the form `tool@version`
    ///
    /// The version must be locally installed. Partial versions and
    /// aliases are resolved against installed versions:
    /// e.g.: `node@20.11.0`, `node@22`, `node@latest`, `node`
    pub tool_spec: String,
}

impl Use {
    pub async fn run(&self) -> Result<()> {
        let (tool, version) = parse_spec(&self.tool_spec)?;
        let resolved = crate::toolset::resolve_installed_version(
            &tool,
            version.as_deref(),
        )
        .await?;

        let previous = current::current_version(&tool);
        current::set_current(&tool, &resolved)?;

        match previous {
            Some(prev) if prev != resolved => println!(
                "{}",
                style::ogreen(format!(
                    "Switched {tool} {prev} → {resolved}"
                ))
            ),
            _ => println!(
                "{}",
                style::ogreen(format!("Using {tool}@{resolved}"))
            ),
        }

        // 切换只写状态文件；刷新方式取决于 shell 是否已激活钩子
        if std::env::var_os("UVMAN_SHELL").is_some() {
            crate::ui::report::print_hint(
                "uvman is activated; changes will apply on your next prompt.",
                &[],
            );
        } else {
            let shell = crate::core::shell::Shell::detect();
            crate::ui::report::print_hint(
                "to update your current shell environment, run:",
                &shell.inject_hint(),
            );
        }
        Ok(())
    }
}
