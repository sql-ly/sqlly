//! Keep access to files and folders the user picked, across relaunches of the
//! sandboxed Mac App Store edition.
//!
//! The App Sandbox grants access to a file the user chose in an open panel (or
//! opened from Finder) only until the app quits. Saved connections point at
//! SQLite/DuckDB files and SSH keys, and projects point at folders, so those
//! picks are remembered as security-scoped bookmarks ([`remember`]) and every
//! bookmark is reopened once at launch ([`restore_all`]) before anything reads
//! those paths. In every other edition both are no-ops: there is no sandbox.

use std::collections::BTreeMap;
use std::path::Path;

/// Where the bookmarks live: a JSON object of path → base64 bookmark.
const BOOKMARKS_ARTIFACT: &str = "sandbox-bookmarks.json";

/// Upper bound on remembered picks, so the file can't grow without bound; the
/// oldest-sorted entries beyond it are dropped.
const MAX_BOOKMARKS: usize = 512;

fn load() -> BTreeMap<String, String> {
    crate::host_storage::load_text(crate::host_storage::StorageArea::Data, BOOKMARKS_ARTIFACT)
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save(bookmarks: &BTreeMap<String, String>) {
    if let Ok(text) = serde_json::to_string_pretty(bookmarks) {
        let _ = crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            BOOKMARKS_ARTIFACT,
            &text,
        );
    }
}

/// Insert `path → bookmark`, keeping at most [`MAX_BOOKMARKS`]. Pure.
fn insert_bounded(bookmarks: &mut BTreeMap<String, String>, path: String, bookmark: String) {
    bookmarks.insert(path.clone(), bookmark);
    while bookmarks.len() > MAX_BOOKMARKS {
        let Some(victim) = bookmarks.keys().find(|key| **key != path).cloned() else {
            break;
        };
        bookmarks.remove(&victim);
    }
}

/// Remember access to a file or folder the user just picked (or opened from
/// Finder) so it stays reachable after a relaunch. Call it right after the
/// pick, while the sandbox still grants access. A no-op outside the App Store
/// edition.
pub fn remember(path: &Path) {
    if !crate::distribution::is_app_store() {
        return;
    }
    let Some(key) = path.to_str().map(str::to_string) else {
        return;
    };
    let Some(bookmark) = ffi::create(&key) else {
        tracing::debug!(path = %key, "no sandbox access to bookmark");
        return;
    };
    let mut bookmarks = load();
    insert_bounded(&mut bookmarks, key, bookmark);
    save(&bookmarks);
}

/// Reopen every remembered bookmark for the life of the process. Bookmarks
/// that no longer resolve (file deleted, volume gone) are dropped; stale ones
/// are refreshed. Call once at launch, before connections or projects open.
pub fn restore_all() {
    if !crate::distribution::is_app_store() {
        return;
    }
    let bookmarks = load();
    if bookmarks.is_empty() {
        return;
    }
    let mut kept = BTreeMap::new();
    for (path, bookmark) in &bookmarks {
        match ffi::resolve(bookmark) {
            Some(Resolved::Ok {
                path: resolved,
                stale,
            }) => {
                let refreshed = if stale {
                    ffi::create(&resolved).unwrap_or_else(|| bookmark.clone())
                } else {
                    bookmark.clone()
                };
                kept.insert(resolved, refreshed);
            }
            Some(Resolved::Gone(error)) => {
                tracing::info!(%path, %error, "dropping a bookmark that no longer resolves");
            }
            None => {
                kept.insert(path.clone(), bookmark.clone());
            }
        }
    }
    if kept != bookmarks {
        save(&kept);
    }
}

#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(all(feature = "app-store", target_os = "macos")), allow(dead_code))]
enum Resolved {
    Ok { path: String, stale: bool },
    Gone(String),
}

/// Parse the Swift side's resolve JSON. Pure.
#[cfg_attr(not(all(feature = "app-store", target_os = "macos")), allow(dead_code))]
fn parse_resolved(json: &str) -> Resolved {
    #[derive(serde::Deserialize)]
    struct Reply {
        ok: bool,
        #[serde(default)]
        path: String,
        #[serde(default)]
        stale: bool,
        #[serde(default)]
        error: Option<String>,
    }
    match serde_json::from_str::<Reply>(json) {
        Ok(reply) if reply.ok => Resolved::Ok {
            path: reply.path,
            stale: reply.stale,
        },
        Ok(reply) => Resolved::Gone(reply.error.unwrap_or_default()),
        Err(error) => Resolved::Gone(error.to_string()),
    }
}

#[cfg(all(feature = "app-store", target_os = "macos"))]
mod ffi {
    use std::ffi::{CStr, CString, c_char};

    use super::{Resolved, parse_resolved};

    unsafe extern "C" {
        fn sqlly_free_string(pointer: *mut c_char);
        fn sqlly_bookmark_create(path: *const c_char) -> *mut c_char;
        fn sqlly_bookmark_resolve(base64: *const c_char) -> *mut c_char;
    }

    fn take(pointer: *mut c_char) -> Option<String> {
        if pointer.is_null() {
            return None;
        }
        // SAFETY: non-null results are NUL-terminated `strdup` strings we own.
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: freed exactly once, by the allocator that created it.
        unsafe { sqlly_free_string(pointer) };
        Some(text)
    }

    pub fn create(path: &str) -> Option<String> {
        let path = CString::new(path).ok()?;
        // SAFETY: `path` outlives the call; the result is taken exactly once.
        take(unsafe { sqlly_bookmark_create(path.as_ptr()) })
    }

    pub fn resolve(bookmark: &str) -> Option<Resolved> {
        let bookmark = CString::new(bookmark).ok()?;
        // SAFETY: as in `create`.
        take(unsafe { sqlly_bookmark_resolve(bookmark.as_ptr()) }).map(|json| parse_resolved(&json))
    }
}

#[cfg(not(all(feature = "app-store", target_os = "macos")))]
mod ffi {
    use super::Resolved;

    pub fn create(_path: &str) -> Option<String> {
        None
    }

    pub fn resolve(_bookmark: &str) -> Option<Resolved> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_reply_parses_path_and_staleness() {
        assert_eq!(
            parse_resolved(r#"{"ok":true,"path":"/Users/me/db.sqlite","stale":true}"#),
            Resolved::Ok {
                path: "/Users/me/db.sqlite".to_string(),
                stale: true
            }
        );
        assert_eq!(
            parse_resolved(r#"{"ok":false,"error":"missing"}"#),
            Resolved::Gone("missing".to_string())
        );
        assert!(matches!(parse_resolved("nope"), Resolved::Gone(_)));
    }

    #[test]
    fn bookmark_store_is_bounded_and_keeps_the_newest() {
        let mut bookmarks = BTreeMap::new();
        for i in 0..MAX_BOOKMARKS + 10 {
            insert_bounded(&mut bookmarks, format!("/p/{i:04}"), "b".to_string());
        }
        assert_eq!(bookmarks.len(), MAX_BOOKMARKS);
        let newest = format!("/p/{:04}", MAX_BOOKMARKS + 9);
        assert!(bookmarks.contains_key(&newest));
    }

    #[cfg(not(feature = "app-store"))]
    #[test]
    fn outside_the_app_store_edition_nothing_is_recorded() {
        // Would write to the data dir if it were active; it must not be.
        remember(Path::new("/tmp/never-recorded.sqlite"));
        restore_all();
    }
}
