mod activate;
mod current;
mod doctor;
mod env;
mod install;
mod list;
mod plugin;
mod self_update;
mod shims;
mod uninstall;
mod use_;
pub(crate) mod version;
mod which;

use crate::Result;

#[derive(clap::Parser)]
#[clap(name = "uvman", about, disable_version_flag = true)]
pub struct Cli {
    /// print version information
    #[clap(short = 'V', long, hide = true)]
    pub version: bool,

    #[clap(subcommand)]
    pub command: Option<Commands>,

    /// Suppress non-error messages
    #[clap(short = 'q', long, global = true)]
    pub quiet: bool,

    /// Show extra output (full error report on failure)
    #[clap(short = 'v', long, global = true, default_value_t = 0, action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(clap::Subcommand)]
pub enum Commands {
    Version(version::Version),
    Plugin(plugin::Plugin),
    Install(install::Install),
    Uninstall(uninstall::Uninstall),
    List(list::List),
    Current(current::Current),
    Which(which::Which),
    Use(use_::Use),
    /// Internal evaluator driven by activate scripts; not user-facing
    #[clap(hide = true)]
    Env(env::Env),
    Activate(activate::Activate),
    Shims(shims::Shims),
    Doctor(doctor::Doctor),
    SelfUpdate(self_update::SelfUpdate),
}

impl Commands {
    pub async fn run(self) -> Result<()> {
        match self {
            Commands::Version(cmd) => cmd.run().await,
            Commands::Plugin(cmd) => cmd.run().await,
            Commands::Install(cmd) => cmd.run().await,
            Commands::Uninstall(cmd) => cmd.run().await,
            Commands::List(cmd) => cmd.run().await,
            Commands::Current(cmd) => cmd.run(),
            Commands::Which(cmd) => cmd.run(),
            Commands::Use(cmd) => cmd.run().await,
            Commands::Env(cmd) => cmd.run(),
            Commands::Activate(cmd) => cmd.run(),
            Commands::Shims(cmd) => cmd.run(),
            Commands::Doctor(cmd) => cmd.run(),
            Commands::SelfUpdate(cmd) => cmd.run().await,
        }
    }
}

/// Refresh the shim set after a state-changing command
/// (`install` / `uninstall` / `use`).
///
/// Best-effort and quiet by design: failures are warnings and must never block
/// the command that already succeeded. When the forwarding binary is absent
/// (template not shipped beside uvman) nothing is done — there is nothing to
/// copy — and a fresh home with no active tools yet skips creating an empty
/// `shims/` dir.
pub(crate) fn auto_rehash_after_change() {
    use crate::core::current;
    use crate::core::paths;
    use crate::core::shims;
    let home = paths::uvman_home();
    if !shims::shim_available(&home) {
        return;
    }
    if !home.join("shims").exists() && current::load().tools.is_empty() {
        return;
    }
    if let Err(e) = shims::rehash(&home) {
        crate::ui::report::print_warning(&format!("failed to refresh shims: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn bare_invocation_leaves_command_none() {
        let cli = Cli::try_parse_from(["uvman"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn help_lists_all_user_facing_commands() {
        let mut buf = Vec::new();
        Cli::command().write_help(&mut buf).unwrap();
        let help = String::from_utf8(buf).unwrap();
        for name in [
            "install",
            "uninstall",
            "list",
            "current",
            "which",
            "use",
            "activate",
            "doctor",
            "plugin",
            "self-update",
        ] {
            assert!(help.contains(name), "help output is missing `{name}`");
        }
        // The internal evaluator stays hidden from user-facing help
        let env_listed = help.lines().any(|l| l.starts_with("  env ") || l.trim_end() == "  env");
        assert!(!env_listed, "internal `env` leaked into help");
    }
}
