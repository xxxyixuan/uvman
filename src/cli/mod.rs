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

    /// Show extra output
    #[clap(short = 'v', long, global = true, default_value_t = 0,action = clap::ArgAction::Count)]
    pub verbose: u8,
}

#[derive(clap::Subcommand)]
pub enum Commands {
    Version(version::Version),
}

impl Commands {
    pub async fn run(self) -> Result<()> {
        match self {
            Commands::Version(cmd) => cmd.run().await,
        }
    }
}
