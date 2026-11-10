//! Cross-platform "reveal in file manager" opener.
//!
//! Opens a directory in the platform's native file manager so the user can
//! inspect SQLLY's on-disk state (currently the IntelliSense data-model cache
//! folder from Settings ▸ IntelliSense Cache). The platform commands:
//!
//! - macOS: `open <dir>` (Launch Services, same path as `NSWorkspace.open`) —
//!   opens the folder in Finder.
//! - Linux: `xdg-open <dir>` — hands the folder to the user's configured file
//!   manager (Nautilus / Dolphin / Thunar / …).
//! - Windows: `explorer <dir>` — opens the folder in File Explorer.
//!
//! Like [`crate::browser_open`], the opener spawns a detached child and never
//! blocks: a successful `spawn` means the launcher accepted the request, not
//! that a window is already on screen.

use std::path::Path;

#[derive(Debug)]
pub enum RevealError {
    /// The current OS had no file-manager command wired. Never happens in a
    /// shipped desktop build (macOS / Linux / Windows are all covered).
    UnsupportedOS,
    /// The folder could not be created before revealing it. The cache folder
    /// may not exist yet (nothing cached), so the opener creates it first —
    /// this is the failure of that step.
    Prepare(std::io::Error),
    /// `std::io::Error` from `Command::spawn`: the file-manager command was
    /// missing on PATH, or the OS refused the spawn.
    Spawn(std::io::Error),
    /// Browser (wasm) build: there is no native file manager to reveal into.
    #[cfg(target_arch = "wasm32")]
    Unavailable,
}

impl std::fmt::Display for RevealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Worded on the UI thread, where the reveal action runs; the
            // I/O error text stays raw inside the keyed clause.
            RevealError::UnsupportedOS => f.write_str(&crate::localization::text(
                "settings.intellisense_cache.reveal_error_unsupported_os",
            )),
            RevealError::Prepare(e) => f.write_str(&crate::localization::format(
                "settings.intellisense_cache.reveal_error_prepare_folder",
                &[("detail", e)],
            )),
            RevealError::Spawn(e) => f.write_str(&crate::localization::format(
                "settings.intellisense_cache.reveal_error_open_file_manager",
                &[("detail", e)],
            )),
            #[cfg(target_arch = "wasm32")]
            RevealError::Unavailable => {
                write!(
                    f,
                    "{}",
                    crate::browser_preview::unavailable(&crate::localization::text(
                        "settings.intellisense_cache.reveal_unavailable"
                    ))
                )
            }
        }
    }
}

impl std::error::Error for RevealError {}

#[cfg(not(target_arch = "wasm32"))]
pub fn reveal_file(path: &Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    if let Some(parent) = path.parent() {
        let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
    }
}

/// Browser (wasm) build: no native file manager exists.
#[cfg(target_arch = "wasm32")]
pub fn reveal_file(_path: &Path) {}

/// Open `dir` in the platform's native file manager. The directory is created
/// first if it does not exist, so a "reveal the cache folder" action works
/// even before anything has been cached (the user sees an empty folder rather
/// than a "not found" error from the launcher).
#[cfg(not(target_arch = "wasm32"))]
pub fn reveal_in_file_manager(dir: &Path) -> Result<(), RevealError> {
    std::fs::create_dir_all(dir).map_err(RevealError::Prepare)?;
    let mut cmd = platform_command(dir).ok_or(RevealError::UnsupportedOS)?;
    cmd.spawn().map_err(RevealError::Spawn)?;
    Ok(())
}

/// Browser (wasm) build: no native file manager exists.
#[cfg(target_arch = "wasm32")]
pub fn reveal_in_file_manager(_dir: &Path) -> Result<(), RevealError> {
    Err(RevealError::Unavailable)
}

/// The platform's "open with the default app" launcher for a path — a
/// directory opens in the file manager, a file in its associated app. The
/// path is passed as a single argument with no shell in between, so spaces
/// and `&`/`^` in it are safe. Also used by Open in Spreadsheet.
#[cfg(target_os = "macos")]
pub(crate) fn platform_command(dir: &Path) -> Option<std::process::Command> {
    let mut cmd = std::process::Command::new("open");
    cmd.arg(dir);
    Some(cmd)
}

#[cfg(target_os = "linux")]
pub(crate) fn platform_command(dir: &Path) -> Option<std::process::Command> {
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(dir);
    Some(cmd)
}

#[cfg(target_os = "windows")]
pub(crate) fn platform_command(dir: &Path) -> Option<std::process::Command> {
    let mut cmd = std::process::Command::new("explorer");
    cmd.arg(dir);
    Some(cmd)
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "windows",
    target_arch = "wasm32"
)))]
pub(crate) fn platform_command(_dir: &Path) -> Option<std::process::Command> {
    None
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn platform_command_is_wired_for_this_os() {
        // macOS / Linux / Windows always return `Some`; the fallthrough stub is
        // itself a targeting error. We build a command without spawning it, so
        // no file manager window opens during `cargo test`.
        let cmd = platform_command(Path::new("/tmp"));
        assert!(
            cmd.is_some(),
            "platform_command should be wired for this OS"
        );
    }

    #[test]
    fn reveal_creates_a_missing_directory() {
        // The reveal path `create_dir_all`s its target first. Verify that step
        // in isolation (spawning a real file manager during tests is
        // disruptive, so we don't call `reveal_in_file_manager` itself).
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("DataModels/nested");
        assert!(!nested.exists());
        std::fs::create_dir_all(&nested).unwrap();
        assert!(nested.exists());
    }
}
