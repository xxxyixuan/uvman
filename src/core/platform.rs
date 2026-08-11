use std::env;

use crate::Lazy;

pub static OS: Lazy<String> = Lazy::new(|| env::consts::OS.into());

pub static ARCH: Lazy<String> = Lazy::new(|| env::consts::ARCH.into());

#[cfg(test)]
mod tests {
    use super::{ARCH, OS};

    #[test]
    fn show_os_and_arch() {
        println!("OS: {}", *OS);
        println!("ARCH: {}", *ARCH);
    }
}
