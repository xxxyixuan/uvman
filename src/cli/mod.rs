mod activate;
mod env;
mod install;
mod list;
mod plugin;
mod self_update;
mod use_;
pub(crate) mod version;

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
    List(list::List),
    Use(use_::Use),
    /// Internal evaluator driven by activate scripts; not user-facing
    #[clap(hide = true)]
    Env(env::Env),
    Activate(activate::Activate),
    SelfUpdate(self_update::SelfUpdate),
}

impl Commands {
    pub async fn run(self) -> Result<()> {
        match self {
            Commands::Version(cmd) => cmd.run().await,
            Commands::Plugin(cmd) => cmd.run().await,
            Commands::Install(cmd) => cmd.run().await,
            Commands::List(cmd) => cmd.run().await,
            Commands::Use(cmd) => cmd.run().await,
            Commands::Env(cmd) => cmd.run(),
            Commands::Activate(cmd) => cmd.run(),
            Commands::SelfUpdate(cmd) => cmd.run().await,
        }
    }
}
