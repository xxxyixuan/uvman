fn main() {
    // Inject the compile-time target triple so `uvman self-update` can pick the
    // release asset matching the platform it was built for.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown-target".into());
    println!("cargo:rustc-env=UVMAN_TARGET={target}");
    println!("cargo:rerun-if-changed=build.rs");
}
