//! Gate the full IntelliSense verification suite behind an environment
//! variable so it compiles and runs only where CI asks for it.
//!
//! When `SQLLY_INTELLISENSE_SUITE` is set (to any non-empty value), we emit the
//! `intellisense_full_suite` cfg. Every regression test file under `tests/`
//! opens with `#![cfg(intellisense_full_suite)]`, so without the variable the
//! test binaries are empty and `cargo test --workspace` on a developer machine
//! costs nothing. CI sets the variable on exactly one platform leg (Linux x64).

fn main() {
    // Register the custom cfg so `-D warnings` clippy does not flag it as an
    // unexpected cfg on builds where the variable is unset.
    println!("cargo::rustc-check-cfg=cfg(intellisense_full_suite)");
    println!("cargo::rerun-if-env-changed=SQLLY_INTELLISENSE_SUITE");

    if std::env::var_os("SQLLY_INTELLISENSE_SUITE").is_some_and(|v| !v.is_empty()) {
        println!("cargo::rustc-cfg=intellisense_full_suite");
    }
}
