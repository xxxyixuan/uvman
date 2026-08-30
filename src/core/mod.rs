pub mod config;
pub mod current;
pub mod error;
pub mod file;
pub mod http;
pub mod install;
pub mod paths;
pub mod platform;
pub mod plugin;
pub mod shell;
pub mod suggest;
mod types;
pub mod upgrade;

pub use types::*;
use versions::{Mess, Versioning};

use crate::Lazy;

pub(crate) static VERSION: Lazy<Versioning> = Lazy::new(|| {
    let mut v = env!("CARGO_PKG_VERSION").to_string();
    if cfg!(debug_assertions) {
        v.push_str("-DEBUG");
    }
    // `env!("CARGO_PKG_VERSION")` is a compile-time constant guaranteed
    // well-formed by cargo, so the fallback arm is unreachable; `Mess::default()`
    // keeps it panic-free by construction instead of trusting that invariant
    // with an `unwrap`.
    Versioning::new(&v).unwrap_or_else(|| Versioning::Complex(Mess::default()))
});
