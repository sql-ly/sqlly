//! Which edition of SQLLY this binary is, and what that edition may do.
//!
//! The direct-download build (DMG / installers) and the Mac App Store build
//! are the same source; the App Store build is compiled with the `app-store`
//! cargo feature. Everything that differs between them asks this module
//! instead of testing the feature itself, so the list of differences lives in
//! one place:
//!
//! * purchases go through Apple's in-app purchase, never a link to the web
//!   store (App Store Review Guideline 3.1.1);
//! * the App Store updates the app, so the in-app update check is off;
//! * the free tier is limited to [`crate::license::FREE_SAVED_CONNECTIONS`]
//!   saved connections;
//! * the App Sandbox forbids launching programs the user installed
//!   (Terminal CLIs, local AI agent CLIs, stdio MCP servers, kubectl,
//!   external editors), so those features explain themselves instead.

/// Whether this binary is the Mac App Store edition. A standalone build is
/// never the App Store edition: if both features are on (only `cargo
/// clippy --all-features` does that), standalone wins.
pub const fn is_app_store() -> bool {
    cfg!(feature = "app-store") && !is_standalone()
}

/// Whether this binary is a standalone build (the `standalone` cargo
/// feature): it never contacts sqlly.app. Account sign-in, license checks and
/// nags, feedback upload, update checks, and Cloud Relay are all off, and each
/// says so when the user reaches for it. The README lists the full set.
pub const fn is_standalone() -> bool {
    cfg!(feature = "standalone")
}

/// Whether the app checks or nags about its license (supporter chip, pay
/// dialog, unlock). Off in standalone builds.
pub const fn license_checks_enabled() -> bool {
    !is_standalone()
}

/// The OS keychain service name for `base` in this edition, usable in a
/// `const`. The Mac App Store edition appends `.mac-app-store`: the two
/// editions are signed differently, so reading the other edition's item makes
/// macOS ask for the login password. Separate names mean neither ever finds
/// the other's items. `sqlly-core` and `sqlly-remote` do the same through
/// their `mac-app-store` feature.
macro_rules! keychain_service {
    ($base:literal) => {
        if $crate::distribution::is_app_store() {
            concat!($base, ".mac-app-store")
        } else {
            $base
        }
    };
}
pub(crate) use keychain_service;

/// Whether purchase surfaces may link to the web store (pricing page,
/// checkout). False in the App Store edition, where they open the in-app
/// purchase window instead.
pub const fn sells_through_web_store() -> bool {
    !is_app_store()
}

/// Whether this copy was installed from the Microsoft Store.
///
/// Unlike the Mac edition this is not a separate build: the Windows download
/// and the Store package carry the same signed binary, and only a Store
/// install runs with an MSIX package identity. The Store version can keep
/// selling through the web store (Microsoft Store Policy 10.8.7 lets non-game
/// apps use their own payment system), so updates are the only difference.
pub fn is_microsoft_store() -> bool {
    #[cfg(target_os = "windows")]
    {
        static PACKAGED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *PACKAGED.get_or_init(windows_package::has_package_identity)
    }
    #[cfg(not(target_os = "windows"))]
    {
        false
    }
}

/// Whether the app checks the SQLLY store for newer builds. Off in both
/// store editions, where the store installs updates, and in standalone
/// builds, which never contact sqlly.app.
pub fn checks_for_updates() -> bool {
    !is_standalone() && !is_app_store() && !is_microsoft_store()
}

/// The store that updates this copy, for "updates arrive through …" text.
/// `None` for the direct download, which updates itself.
pub fn updating_store_name() -> Option<&'static str> {
    if is_app_store() {
        Some("Mac App Store")
    } else if is_microsoft_store() {
        Some("Microsoft Store")
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
mod windows_package {
    /// `GetCurrentPackageFullName` reports `APPMODEL_ERROR_NO_PACKAGE` for an
    /// unpackaged process; with a zero-length buffer a packaged process gets
    /// `ERROR_INSUFFICIENT_BUFFER` instead (the name's length), and success
    /// is impossible. So any answer other than "no package" means packaged.
    const APPMODEL_ERROR_NO_PACKAGE: i32 = 15700;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentPackageFullName(length: *mut u32, name: *mut u16) -> i32;
    }

    pub(super) fn has_package_identity() -> bool {
        let mut length = 0u32;
        // SAFETY: a zero length with a null buffer is the documented way to
        // ask for the required size; nothing is written through the pointer.
        let status = unsafe { GetCurrentPackageFullName(&mut length, std::ptr::null_mut()) };
        status != APPMODEL_ERROR_NO_PACKAGE
    }
}

/// Whether the free tier's saved-connection limit is enforced.
pub const fn enforces_free_connection_limit() -> bool {
    is_app_store()
}

/// Whether the app may launch programs installed outside its bundle.
pub const fn can_launch_user_programs() -> bool {
    !is_app_store()
}

/// The error shown when a feature needs to launch a user-installed program
/// that the App Store edition's sandbox does not allow.
///
/// Deliberately does not point at another place to get SQLLY: App Review
/// treats steering users to a different distribution channel as a violation.
/// It points at Help ▸ Mac App Store Edition instead, which lists every
/// difference.
pub fn sandbox_blocked_message(feature: impl AsRef<str>) -> String {
    crate::localization::format(
        "distribution.sandbox_blocked",
        &[("feature", feature.as_ref())],
    )
}

/// The public docs page describing the macOS editions and the App Store
/// edition's restrictions.
pub const MACOS_DOCS_URL: &str = "https://sqlly.app/docs/features/platforms/macos";

/// Every way the Mac App Store edition differs from the direct download, as
/// (what, what to do instead). Shown in Help ▸ Mac App Store Edition and on
/// the docs page at [`MACOS_DOCS_URL`]; keep the two in sync.
pub const APP_STORE_LIMITATIONS: [(&str, &str); 11] = [
    (
        "Updates come from the Mac App Store",
        "SQLLY doesn't check for updates itself; the App Store installs them.",
    ),
    (
        "Open in Terminal is unavailable",
        "Run your command-line clients from Terminal yourself.",
    ),
    (
        "Local AI agent command-line tools can't be launched",
        "Cloud AI providers and a local Ollama server still work.",
    ),
    (
        "Local (stdio) MCP servers can't be launched",
        "MCP servers you reach over HTTP still work; SQLLY can't act as an MCP server for other apps.",
    ),
    (
        "Kubernetes discovery (kubectl) is unavailable",
        "Enter the forwarded host and port in the connection editor.",
    ),
    (
        "Command-line and terminal editors can't be opened",
        "Editors that are Mac apps still open.",
    ),
    (
        "Git integration is unavailable",
        "The SQL Library and query history still work without git.",
    ),
    (
        "DuckDB extensions aren't downloaded or installed",
        "DuckDB's built-in features still work.",
    ),
    (
        "SQLLY only reads files and folders you choose",
        "Pick SSH keys and certificates in the connection editor, and open a SQLite file's folder \
         (File ▸ Open Folder) if SQLLY needs to write next to it. ~/.ssh/config, ~/.pgpass and \
         ~/.kube aren't read automatically.",
    ),
    (
        "Settings, saved connections and passwords are kept separately",
        "Each edition has its own keychain entries, so neither asks for your Mac password to \
         read the other's. To move connections and their passwords, choose File ▸ Export \
         Credentials in one edition and File ▸ Import Connections in the other, then delete \
         the exported file.",
    ),
    (
        "iCloud sync is unavailable",
        "To copy connections to another Mac, choose File ▸ Export Connections, then File ▸ \
         Import Connections on the other Mac.",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keychain_service_names_differ_only_in_the_app_store_edition() {
        const SERVICE: &str = keychain_service!("SQLLY.sql-auth");
        if is_app_store() {
            assert_eq!(SERVICE, "SQLLY.sql-auth.mac-app-store");
        } else {
            assert_eq!(SERVICE, "SQLLY.sql-auth");
        }
    }

    #[test]
    fn edition_flags_agree_with_each_other() {
        assert_eq!(sells_through_web_store(), !is_app_store());
        assert_eq!(
            checks_for_updates(),
            !is_standalone() && !is_app_store() && !is_microsoft_store()
        );
        if !is_standalone() {
            assert_eq!(updating_store_name().is_none(), checks_for_updates());
        }
        assert_eq!(license_checks_enabled(), !is_standalone());
        assert_eq!(enforces_free_connection_limit(), is_app_store());
        assert_eq!(can_launch_user_programs(), !is_app_store());
    }

    /// `cargo test` never runs inside an MSIX package, so a test binary on
    /// any platform must read as the direct download.
    #[test]
    fn an_unpackaged_process_is_not_the_microsoft_store_edition() {
        assert!(!is_microsoft_store());
    }

    #[test]
    fn sandbox_message_names_the_feature_and_never_points_off_store() {
        let message = sandbox_blocked_message("Open in Terminal");
        assert!(message.starts_with("Open in Terminal isn't available"));
        assert!(message.contains("Help ▸ Mac App Store Edition"));
        assert!(!message.contains("sqlly.app"));
        assert!(!message.to_lowercase().contains("download"));
    }

    #[test]
    fn limitations_are_complete_sentences_without_off_store_pointers() {
        for (what, instead) in APP_STORE_LIMITATIONS {
            assert!(!what.is_empty() && !instead.is_empty());
            assert!(instead.ends_with('.'), "{instead}");
            for text in [what, instead] {
                assert!(!text.contains("sqlly.app"), "{text}");
                assert!(!text.to_lowercase().contains("direct-download"), "{text}");
            }
        }
    }
}
