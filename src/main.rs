mod cli;
mod ui;
pub use eyre::Result;
pub use std::sync::LazyLock as Lazy;
use clap::Parser;

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
