pub mod config;
pub mod error;
pub mod file;
pub mod http;
pub mod paths;
pub mod platform;
pub mod plugin;
pub mod suggest;
mod types;

pub use types::*;
use versions::Versioning;

use crate::Lazy;

pub(crate) static VERSION: Lazy<Versioning> = Lazy::new(|| {
    let mut v = env!("CARGO_PKG_VERSION").to_string();
    if cfg!(debug_assertions) {
        v.push_str("-DEBUG");
    }
    Versioning::new(&v).unwrap()
});
