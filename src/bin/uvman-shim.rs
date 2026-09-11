//! `uvman-shim`: forwarding shim binary (plan 0.3.0 task 1).
//!
//! A copy of this binary sits in `<UVMAN_HOME>/shims/` under the command name
//! (e.g. `node.exe`), so GUI/IDE processes that inherit the Explorer
//! environment resolve uvman-managed tools through the stable `shims/` PATH
//! entry. It shares only `core` with the main program — no clap, no network,
//! no UI — and forwards by locating the same executable `which` would report.

fn main() {}