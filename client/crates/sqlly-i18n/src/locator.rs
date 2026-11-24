//! Translator locator side table: while the translation manager is
//! highlighting or picking, every resolved string is recorded against the key
//! that produced it so the preview can map painted text back to keys. The
//! strings themselves are never changed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Distinct strings remembered at most; formatted values with live numbers
/// would otherwise grow the table for as long as tracking stays on.
const MAX_TEXTS: usize = 20_000;

/// The keys that produced one wording, most recently resolved first.
pub type Keys = Arc<[Box<str>]>;
type Table = Option<HashMap<Box<str>, Keys>>;

static ENABLED: AtomicBool = AtomicBool::new(false);
static TEXT_KEYS: Mutex<Table> = Mutex::new(None);

fn table() -> MutexGuard<'static, Table> {
    TEXT_KEYS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Turns recording on or off. Turning it off forgets every recorded string.
pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Release);
    if !enabled {
        *table() = None;
    }
}

/// Forgets every recorded string while recording stays on. The catalog
/// changed (language or overrides), so a wording recorded before may no
/// longer be what its key shows; the refresh that follows re-records the
/// strings on screen.
pub(crate) fn forget_texts() {
    *table() = None;
}

pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Ignores the decoration call sites add around a translation, such as
/// surrounding whitespace or a trailing ellipsis or colon.
fn normalize(text: &str) -> &str {
    text.trim()
        .trim_end_matches(['…', ':', '：'])
        .trim_end_matches("...")
        .trim_end()
}

/// Remembers that `key` resolved to `text`. The most recently resolved key
/// for a wording comes first.
pub(crate) fn record(key: &str, text: &str) {
    if !enabled() {
        return;
    }
    let text = normalize(text);
    if text.is_empty() {
        return;
    }
    let mut table = table();
    let table = table.get_or_insert_with(HashMap::new);
    if let Some(keys) = table.get_mut(text) {
        if keys.first().is_some_and(|first| &**first == key) {
            return;
        }
        let mut updated = Vec::with_capacity(keys.len() + 1);
        updated.push(Box::from(key));
        updated.extend(keys.iter().filter(|existing| &***existing != key).cloned());
        *keys = updated.into();
    } else if table.len() < MAX_TEXTS {
        table.insert(Box::from(text), Arc::from([Box::from(key)]));
    }
}

/// The keys recorded for `text`, most recently resolved first.
pub fn keys_for(text: &str) -> Option<Keys> {
    if !enabled() {
        return None;
    }
    let text = normalize(text);
    table().as_ref()?.get(text).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    // One test owns the process-wide table so parallel tests cannot race it.
    #[test]
    fn records_keys_by_wording_and_forgets_them_when_disabled() {
        set_enabled(false);
        record("locator_test.file", "Locator File");
        assert!(keys_for("Locator File").is_none());

        set_enabled(true);
        record("locator_test.file", "Locator File");
        record("locator_test.dialog_file", "Locator File");
        record("locator_test.connect", "Locator Connect…");
        record("locator_test.file", "Locator File");
        let keys = keys_for(" Locator File ").unwrap();
        assert_eq!(
            keys.iter().map(|key| &**key).collect::<Vec<_>>(),
            ["locator_test.file", "locator_test.dialog_file"]
        );
        assert_eq!(
            &*keys_for("Locator Connect").unwrap()[0],
            "locator_test.connect"
        );
        assert!(keys_for("Locator Elsewhere").is_none());

        set_enabled(false);
        assert!(keys_for("Locator File").is_none());
        set_enabled(true);
        assert!(keys_for("Locator File").is_none());

        // An override change drops wordings the old catalog produced, so a
        // key's former wording no longer outlines text that still shows it.
        record("locator_test.file", "Locator Old");
        crate::clear_overrides();
        assert!(enabled());
        assert!(keys_for("Locator Old").is_none());
        record("locator_test.file", "Locator New");
        assert_eq!(&*keys_for("Locator New").unwrap()[0], "locator_test.file");
        set_enabled(false);
    }
}
