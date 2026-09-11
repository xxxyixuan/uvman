//! uvman library root.
//!
//! Exposing the crate as a library lets the second binary target
//! (`uvman-shim`) share the single version-resolution entry (`core::resolve`)
//! and the layout/path logic without recompiling the CLI surface or pulling in
//! clap / network / UI code paths (plan 0.3.0 task 1: shim and main program
//! share `core` only, no mutual dependency).

pub mod app;
pub mod cli;
pub mod core;
pub mod toolset;
pub mod ui;

pub use eyre::Result;
pub use std::sync::LazyLock as Lazy;

/// CLI entry point: parse args and run the command.
///
/// Kept in the library so the `uvman` binary only wraps it in a runtime; the
/// async body lives here next to the modules it drives.
pub async fn run() -> Result<()> {
    use clap::{CommandFactory, Parser};
    let cli = cli::Cli::parse();
    ui::report::set_verbose(cli.verbose);
    ui::report::set_quiet(cli.quiet);
    // Honor the NO_COLOR convention (https://no-color.org)
    if std::env::var_os("NO_COLOR").is_some() {
        ui::report::set_color(false);
    }
    if cli.version {
        cli::version::Version { json: false }.run().await?;
        return Ok(());
    }
    if let Some(cmd) = cli.command {
        cmd.run().await?;
    } else {
        // A bare `uvman` should be immediately useful: show the full help
        // (on stdout, exit 0) instead of printing nothing.
        cli::Cli::command().print_help()?;
    }
    Ok(())
}
