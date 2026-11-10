// GUI subsystem on Windows: without this every launch also opens a console
// window. Logs still reach the diagnostic log file; stdout/stderr are simply
// not attached to a console in release use.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    // reqwest is built with `rustls-no-provider`; install the workspace's one
    // compiled provider (ring) as the process default before any HTTPS client
    // (store, Azure, AI providers) is constructed.
    let _ = rustls::crypto::ring::default_provider().install_default();

    sqlly_gpui::run_native();
}

/// The browser build drives the library through the `apps/browser` wasm
/// entry point; this binary target does nothing on wasm.
#[cfg(target_arch = "wasm32")]
fn main() {}
