mod app;
mod cli;
mod core;
mod toolset;
mod ui;

use core::error::UError;
pub use std::sync::LazyLock as Lazy;

use clap::Parser;
pub use eyre::Result;

fn main() -> std::process::ExitCode {
    let result = app::init().and_then(|()| _main());
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(report) => {
            let want_debug = std::env::var_os("RUST_BACKTRACE")
                .is_some_and(|v| v != "0")
                || ui::report::verbose() > 0;
            // 默认只输出用户可读信息；--verbose / RUST_BACKTRACE
            // 时输出详细调试报告
            let code = if want_debug {
                eprintln!("{report:?}");
                1
            } else if let Some(err) = report.downcast_ref::<UError>() {
                ui::report::print_error(err)
            } else {
                ui::report::print_error_message(&report.to_string())
            };
            std::process::ExitCode::from(code)
        },
    }
}

#[tokio::main]
async fn _main() -> Result<()> {
    let cli = cli::Cli::parse();
    ui::report::set_verbose(cli.verbose);
    ui::report::set_quiet(cli.quiet);
    // 遵循 NO_COLOR 惯例（https://no-color.org）
    if std::env::var_os("NO_COLOR").is_some() {
        ui::report::set_color(false);
    }
    if cli.version {
        cli::version::Version { json: false }.run().await?;
        return Ok(());
    }
    if let Some(cmd) = cli.command {
        cmd.run().await?;
    }
    Ok(())
}
