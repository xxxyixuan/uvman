//! Progress-bar style construction shared by download and install stages.
//!
//! Templates are static strings, so a build failure is impossible in practice;
//! it degrades to the indicatif default style instead of panicking.

use indicatif::{ProgressBar, ProgressStyle};

/// Spinner style from a static template; falls back to the default spinner.
pub fn spinner_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_spinner())
}

/// Bar style from a static template; falls back to the default bar.
pub fn bar_style(template: &str) -> ProgressStyle {
    ProgressStyle::with_template(template).unwrap_or_else(|_| ProgressStyle::default_bar())
}

/// Drop the spinner rotation and finalize the line colored by result mark.
pub fn finish_marked(pb: &ProgressBar, ok: bool, msg: &str) {
    pb.disable_steady_tick();
    pb.set_style(spinner_style("{msg}"));
    let mark = if ok { "✔" } else { "✖" };
    let line = format!("{mark} {msg}");
    let styled = if ok { crate::ui::style::ogreen(line).to_string() } else { crate::ui::style::ered(line).to_string() };
    pb.set_message(styled);
    pb.finish();
}
