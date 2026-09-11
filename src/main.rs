//! `uvman` binary: a thin wrapper around the library entry point.
//!
//! All command logic lives in the `uvman` library target; this binary only
//! bootstraps the error handler and runs the async CLI body on a tokio
//! runtime.

use uvman::core::error::UError;

fn main() -> std::process::ExitCode {
    let result = uvman::app::init().and_then(|()| {
        tokio::runtime::Runtime::new()
            .expect("failed to create the tokio runtime")
            .block_on(uvman::run())
    });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(report) => {
            let want_debug = std::env::var_os("RUST_BACKTRACE").is_some_and(|v| v != "0")
                || uvman::ui::report::verbose() > 0;
            // Print user-readable info by default; --verbose / RUST_BACKTRACE
            // emit the full debug report.
            let code = if want_debug {
                eprintln!("{report:?}");
                1
            } else if let Some(err) = report.downcast_ref::<UError>() {
                uvman::ui::report::print_error(err)
            } else {
                uvman::ui::report::print_error_message(&report.to_string())
            };
            std::process::ExitCode::from(code)
        },
    }
}
