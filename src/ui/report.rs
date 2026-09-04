//! Unified diagnostic output (error / warning / hint).
//!
//! Design follows uv and mise:
//! - `error:` states the fact in one sentence
//! - `hint:` states the situation; runnable commands are printed underneath
//!   as `you can run "cmd"` (green-highlighted command, copy-paste ready)
//! - Low-level system errors (network/IO) get a dim footer pointing users to
//!   `--verbose` for self-service digging
//! - Usage errors exit with code 2, general errors with 1

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use console::StyledObject;

use crate::core::error::UError;
use crate::ui::style::{ecyan, egreen, ered, eyellow};

/// Global verbose level (set by CLI --verbose / -v)
static VERBOSE: AtomicU8 = AtomicU8::new(0);

/// Global quiet flag (set by CLI --quiet / -q); suppresses non-error output
static QUIET: AtomicBool = AtomicBool::new(false);

/// Global color toggle (disabled by NO_COLOR / in tests)
static COLOR: AtomicBool = AtomicBool::new(true);

pub fn set_verbose(level: u8) {
    VERBOSE.store(level, Ordering::Relaxed);
}

pub fn verbose() -> u8 {
    VERBOSE.load(Ordering::Relaxed)
}

pub fn set_quiet(enabled: bool) {
    QUIET.store(enabled, Ordering::Relaxed);
}

pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

pub fn set_color(enabled: bool) {
    COLOR.store(enabled, Ordering::Relaxed);
}

fn color_enabled() -> bool {
    COLOR.load(Ordering::Relaxed)
}

/// Print an error message, optional fix hints, and its exit code
pub fn print_error(err: &UError) -> u8 {
    print_prefixed(ered("error:"), &err.to_string());

    if let Some(hint) = err.hint() {
        print_prefixed(ecyan("hint:"), &hint.message);
        for cmd in &hint.commands {
            eprintln!("  {}", styled_code(cmd));
        }
    }

    // Only add the mise-style footer when there is an underlying cause and
    // verbose is off, so hint-resolvable semantic errors stay quiet.
    if err.has_source() && verbose() == 0 {
        eprintln!("{}", crate::ui::style::estyle("run with --verbose for more details").dim());
    }
    err.exit_code()
}

/// Print a bare error message (for non-UError cases)
pub fn print_error_message(msg: &str) -> u8 {
    print_prefixed(ered("error:"), msg);
    1
}

/// Print a warning
pub fn print_warning(msg: &str) {
    if quiet() {
        return;
    }
    print_prefixed(eyellow("warning:"), msg);
}

/// Print a hint (fix suggestions for non-error cases; commands line-copyable)
pub fn print_hint(message: &str, commands: &[String]) {
    if quiet() {
        return;
    }
    print_prefixed(ecyan("hint:"), message);
    for cmd in commands {
        eprintln!("you can run {}", styled_code(&format!("\"{cmd}\"")));
    }
}

fn print_prefixed(prefix: StyledObject<&str>, msg: &str) {
    eprint!("{} ", prefix.bold());
    render_backticks(msg);
}

/// Split on backticks; highlight backtick-enclosed fragments green
fn render_backticks(msg: &str) {
    if !color_enabled() {
        eprintln!("{msg}");
        return;
    }
    let mut in_code = false;
    for seg in msg.split('`') {
        if in_code {
            eprint!("{}", egreen(seg));
        } else {
            eprint!("{seg}");
        }
        in_code = !in_code;
    }
    eprintln!();
}

fn styled_code(cmd: &str) -> String {
    if color_enabled() { egreen(cmd).to_string() } else { cmd.to_string() }
}
