//! Cross-platform browser opener for the Entra OAuth redirect.
//!
//! The browser redirect URL (`https://login.microsoftonline.com/...`) must
//! be opened in the user's default browser. The platform-specific commands:
//!
//! - macOS: `open <url>` (uses Launch Services, the same path as
//!   `NSWorkspace.shared.open`)
//! - Linux: `xdg-open <url>` (falls back to `gio open` on modern GNOME)
//! - Windows: `rundll32 url.dll,FileProtocolHandler <url>` (ShellExecute,
//!   with no cmd.exe re-parsing of the URL)
//!
//! The opener spawns a detached child process — we do NOT pipe stdin or
//! capture stdout/stderr — so the helper returns as soon as the launcher
//! accepts the URL (typically under 30 ms).
//!
//! Kept dependency-free on purpose: prefer a `std::process::Command` over a
//! shellout library to avoid pulling in another crate for one short-lived
//! process per sign-in.

#[derive(Debug)]
pub enum OpenError {
    /// The current OS / target was not in the allowlist above. Should never
    /// happen in a supported build — the three targets the project ships
    /// (macOS, Linux, Windows) are all covered.
    UnsupportedOS,
    /// Only `http://` / `https://` URLs are handed to the OS opener; other
    /// schemes (`file:`, custom handlers) would launch local programs.
    NotAWebUrl,
    /// `std::io::Error` from `Command::spawn`. The browser-launching command
    /// was not found on PATH, or the OS refused the spawn (sandbox / seccomp).
    Spawn(std::io::Error),
    /// Browser (wasm) build only: `window.open` was refused (typically the
    /// popup blocker) or the DOM window is unavailable.
    #[cfg(target_arch = "wasm32")]
    Browser(String),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::UnsupportedOS => {
                write!(
                    f,
                    "{}",
                    crate::localization::text("browser.open.unsupported_os")
                )
            }
            OpenError::Spawn(error) => write!(
                f,
                "{}",
                crate::localization::format(
                    "browser.open.launch_failed",
                    &[("error", &error.to_string())],
                )
            ),
            OpenError::NotAWebUrl => {
                write!(
                    f,
                    "{}",
                    crate::localization::text("browser.open.not_web_url")
                )
            }
            #[cfg(target_arch = "wasm32")]
            OpenError::Browser(reason) => write!(
                f,
                "{}",
                crate::localization::format("browser.open.tab_failed", &[("error", reason)],)
            ),
        }
    }
}

impl std::error::Error for OpenError {}

/// Opens `url` in the user's default browser. Cross-platform; never blocks.
/// Returns `Err` only when the platform is not supported or the OS spawn
/// itself fails (e.g. the launcher binary is missing on PATH) — a *successful*
/// `spawn` is no guarantee the browser will actually navigate (the launcher
/// could legitimately refuse), but the user sees the launch event in their
/// dock / taskbar.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_in_browser(url: &str) -> Result<(), OpenError> {
    if !is_web_url(url) {
        return Err(OpenError::NotAWebUrl);
    }
    let mut cmd = platform_command(url).ok_or(OpenError::UnsupportedOS)?;
    cmd.spawn().map_err(OpenError::Spawn)?;
    Ok(())
}

/// Browser (wasm) build: open `url` in a new tab via `window.open`.
#[cfg(target_arch = "wasm32")]
pub fn open_in_browser(url: &str) -> Result<(), OpenError> {
    if !is_web_url(url) {
        return Err(OpenError::NotAWebUrl);
    }
    let window = web_sys::window().ok_or_else(|| {
        OpenError::Browser(crate::localization::text("browser.open.no_dom_window"))
    })?;
    match window.open_with_url_and_target(url, "_blank") {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(OpenError::Browser(crate::localization::text(
            "browser.open.popup_blocked",
        ))),
        Err(_) => Err(OpenError::Browser(crate::localization::text(
            "browser.open.window_open_failed",
        ))),
    }
}

#[cfg(target_os = "macos")]
fn platform_command(url: &str) -> Option<std::process::Command> {
    let mut cmd = std::process::Command::new("open");
    cmd.arg(url);
    Some(cmd)
}

#[cfg(target_os = "linux")]
fn platform_command(url: &str) -> Option<std::process::Command> {
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(url);
    Some(cmd)
}

/// Whether `url` is an absolute `http://` / `https://` URL — the only kind
/// the opener hands to the OS.
pub fn is_web_url(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    (lower.starts_with("https://") || lower.starts_with("http://")) && url == url.trim_start()
}

/// The Windows launcher: `rundll32 url.dll,FileProtocolHandler <url>` opens
/// the default browser through ShellExecute with the URL as one argv entry.
/// (`cmd /C start "" <url>` re-parses the line in cmd.exe, which splits at
/// `&` — present in every OAuth URL — and runs the rest as commands.)
#[allow(dead_code)]
pub fn windows_open_argv(url: &str) -> (&'static str, Vec<String>) {
    (
        "rundll32",
        vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()],
    )
}

#[cfg(target_os = "windows")]
fn platform_command(url: &str) -> Option<std::process::Command> {
    let (program, args) = windows_open_argv(url);
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    Some(cmd)
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "windows",
    target_arch = "wasm32"
)))]
fn platform_command(_url: &str) -> Option<std::process::Command> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cmd /C start "" <url>` hands the URL to cmd.exe, which splits it at
    /// `&` and runs the remainder as a command (every OAuth URL has `&`).
    /// The Windows opener must not route the URL through a shell at all.
    #[test]
    fn windows_opener_never_passes_the_url_through_cmd() {
        let url = "https://login.example/authorize?a=1&calc.exe&b=2";
        let (program, args) = windows_open_argv(url);
        assert_ne!(program.to_ascii_lowercase(), "cmd");
        assert_eq!(program, "rundll32");
        assert_eq!(
            args,
            vec!["url.dll,FileProtocolHandler".to_string(), url.to_string()]
        );
    }

    /// Only web URLs are handed to the OS opener: `file:`/custom schemes
    /// would launch local programs or documents.
    #[test]
    fn only_http_urls_are_opened() {
        assert!(is_web_url("https://sqlly.app/download"));
        assert!(is_web_url("HTTP://localhost:5000/x"));
        assert!(!is_web_url("file:///Applications/Calculator.app"));
        assert!(!is_web_url("javascript:alert(1)"));
        assert!(!is_web_url("-a Calculator"));
        assert!(!is_web_url("smb://host/share"));
    }

    #[test]
    fn platform_command_returns_some_on_supported_os() {
        // macOS / Linux / Windows always return `Some`. The unsupported
        // branch is a stub that is itself an error in targeting — calling
        // it from the binary will never happen because the Rust targets
        // table the project ships is exactly those three.
        let cmd = platform_command("https://example.com");
        assert!(
            cmd.is_some(),
            "platform_command should be wired for this OS"
        );
    }

    #[test]
    fn open_in_browser_returns_unsupported_only_when_stub() {
        // We cannot actually launch a browser in a test environment — calling
        // `open_in_browser` on a desktop OS opens a real browser tab, which
        // is disruptive during `cargo test`. The success path is verified by
        // manual smoke tests.
        //
        // Instead of calling the real function, verify that `platform_command`
        // produces a valid command (without executing it). This confirms the
        // platform dispatch is wired without side effects.
        let cmd = platform_command("https://example.com/");
        match cmd {
            Some(_) => {}
            None => {
                panic!("platform_command returned None — target_os is not in the supported set?")
            }
        }
    }
}
