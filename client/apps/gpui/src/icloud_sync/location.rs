//! Where iCloud Drive is on this Mac, and whether it is usable.
//!
//! The direct-download edition is not sandboxed and has no iCloud
//! entitlements, so it cannot have an app-specific iCloud container. It uses
//! a plain folder in the user's own iCloud Drive instead
//! (`~/Library/Mobile Documents/com~apple~CloudDocs/SQLLY Sync`), which macOS
//! uploads and downloads like any other file there. `~/Library/Mobile
//! Documents` exists only while the Mac is signed in to iCloud, and its
//! `com~apple~CloudDocs` folder only while iCloud Drive is on, which is how
//! the two "not available" cases are told apart.
//!
//! The Mac App Store edition is sandboxed: it cannot reach that folder, and
//! it is not built with iCloud entitlements, so sync is unavailable there.

use std::path::{Path, PathBuf};

use super::document::{DEVICES_FOLDER, SYNC_FOLDER};

/// Folder under the home directory that holds iCloud Drive and app
/// containers while the Mac is signed in to iCloud.
pub const MOBILE_DOCUMENTS: &str = "Library/Mobile Documents";
/// iCloud Drive's own folder inside [`MOBILE_DOCUMENTS`].
pub const ICLOUD_DRIVE_FOLDER: &str = "com~apple~CloudDocs";

/// Whether sync can run here, and where.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// iCloud Drive is on; `devices` is the folder holding each Mac's file.
    Available { devices: PathBuf },
    /// The Mac is not signed in to iCloud.
    NotSignedIn,
    /// Signed in, but iCloud Drive is turned off.
    DriveOff,
    /// The Mac App Store edition (sandboxed, no iCloud entitlement).
    AppStoreEdition,
    /// Not macOS.
    Unsupported,
}

/// Classify a home directory. Pure apart from checking that two folders
/// exist, so tests can point it at a temporary directory.
pub fn detect_in_home(home: &Path) -> Availability {
    let mobile = home.join(MOBILE_DOCUMENTS);
    if !mobile.is_dir() {
        return Availability::NotSignedIn;
    }
    let drive = mobile.join(ICLOUD_DRIVE_FOLDER);
    if !drive.is_dir() {
        return Availability::DriveOff;
    }
    Availability::Available {
        devices: drive.join(SYNC_FOLDER).join(DEVICES_FOLDER),
    }
}

/// This Mac, this edition.
pub fn detect() -> Availability {
    if !cfg!(target_os = "macos") {
        return Availability::Unsupported;
    }
    if crate::distribution::is_app_store() {
        return Availability::AppStoreEdition;
    }
    match dirs::home_dir() {
        Some(home) => detect_in_home(&home),
        None => Availability::NotSignedIn,
    }
}

/// Ask iCloud to download a file it is holding only in the cloud (the
/// `.name.icloud` placeholder). Best effort: the next pass reads it once it
/// has arrived.
pub fn request_download(placeholder: &Path) {
    #[cfg(target_os = "macos")]
    {
        let Some(name) = placeholder.file_name().and_then(|n| n.to_str()) else {
            return;
        };
        let Some(real) = name
            .strip_prefix('.')
            .and_then(|rest| rest.strip_suffix(".icloud"))
        else {
            return;
        };
        let target = placeholder.with_file_name(real);
        let _ = std::process::Command::new("/usr/bin/brctl")
            .arg("download")
            .arg(target)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    #[cfg(not(target_os = "macos"))]
    let _ = placeholder;
}
