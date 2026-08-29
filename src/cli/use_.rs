use super::install::parse_spec;
use crate::Result;
use crate::core::current;
use crate::core::shell::{Shell, is_activated};
use crate::toolset::resolve_installed_version;
use crate::ui::report::print_hint;
use crate::ui::style::ogreen;

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
        let resolved = resolve_installed_version(&tool, version.as_deref()).await?;

        // `use` is the sole writer of the state table; env/list only read it
        let previous = current::current_version(&tool);
        current::set_current(&tool, &resolved)?;

        println!("{}", ogreen(switch_message(&tool, previous.as_deref(), &resolved)));
        print_apply_hint();
        Ok(())
    }
}

/// Success line: distinguishes an actual switch from a no-op re-select of the
/// already-active version
fn switch_message(tool: &str, previous: Option<&str>, resolved: &str) -> String {
    match previous {
        Some(prev) if prev != resolved => format!("Switched {tool} {prev} → {resolved}"),
        _ => format!("Using {tool}@{resolved}"),
    }
}

/// How the switch reaches the live shell. The write above only updates the
/// state file: an activated session refreshes itself on the next prompt, so
/// just say so; otherwise the env must be applied to the current shell by
/// hand — suggest the one-off eval for the detected shell.
fn print_apply_hint() {
    if is_activated() {
        print_hint("uvman is activated; changes will apply on your next prompt.", &[]);
    } else {
        print_hint(
            "to update your current shell environment, run:",
            &Shell::detect().inject_hint(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_switch_message() {
        assert_eq!(
            switch_message("node", Some("22.0.0"), "22.19.0"),
            "Switched node 22.0.0 → 22.19.0"
        );
        // Re-selecting the active version is not a switch
        assert_eq!(switch_message("node", Some("22.19.0"), "22.19.0"), "Using node@22.19.0");
        assert_eq!(switch_message("node", None, "22.19.0"), "Using node@22.19.0");
    }
}
