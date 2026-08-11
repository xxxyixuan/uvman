mod cli;
mod core;
mod ui;

use clap::Parser;
pub use eyre::Result;
pub use std::sync::LazyLock as Lazy;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = cli::Cli::parse();
    if cli.version {
        cli::version::Version { json: false }.run().await?;
        return Ok(());
    }
    if let Some(cmd) = cli.command {
        cmd.run().await?;
    }
    Ok(())
}
