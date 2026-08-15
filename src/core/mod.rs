pub mod config;
pub mod error;
pub mod file;
pub mod http;
pub mod paths;
pub mod platform;
pub mod plugin;
pub mod suggest;
mod types;

use crate::Lazy;
pub use types::*;
use versions::Versioning;

pub(crate) static VERSION: Lazy<Versioning> = Lazy::new(|| {
    let mut v = env!("CARGO_PKG_VERSION").to_string();
    if cfg!(debug_assertions) {
        v.push_str("-DEBUG");
    }
    Versioning::new(&v).unwrap()
});
