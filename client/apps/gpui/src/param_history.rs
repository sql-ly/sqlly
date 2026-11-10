//! Remembered values for `-- sqlly param:` names, shown as one-click
//! suggestions in the parameter strip. Keyed by parameter name
//! (case-insensitive, `@` included); most recent first; capped per name.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_VALUES_PER_NAME: usize = 8;

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParamHistory {
    /// Lower-cased parameter name (with `@`) → values, most recent first.
    #[serde(default)]
    pub values: BTreeMap<String, Vec<String>>,
}

impl ParamHistory {
    fn key(name: &str) -> String {
        name.trim().to_ascii_lowercase()
    }

    pub fn suggestions(&self, name: &str) -> &[String] {
        self.values
            .get(&Self::key(name))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Record `value` for `name`: trims, ignores empty and `NULL` (any case),
    /// dedupes case-insensitively (moving the match to the front with the new
    /// spelling), truncates to [`MAX_VALUES_PER_NAME`]. Returns true when the
    /// map changed.
    pub fn remember(&mut self, name: &str, value: &str) -> bool {
        let value = value.trim();
        if value.is_empty() || value.eq_ignore_ascii_case("null") {
            return false;
        }
        let key = Self::key(name);
        let entry = self.values.entry(key).or_default();
        if let Some(pos) = entry
            .iter()
            .position(|existing| existing.eq_ignore_ascii_case(value))
        {
            entry.remove(pos);
        }
        entry.insert(0, value.to_string());
        entry.truncate(MAX_VALUES_PER_NAME);
        true
    }

    #[allow(dead_code)] // Retained for future use.
    pub fn remove(&mut self, name: &str, value: &str) -> bool {
        let key = Self::key(name);
        let Some(entry) = self.values.get_mut(&key) else {
            return false;
        };
        let before = entry.len();
        entry.retain(|existing| !existing.eq_ignore_ascii_case(value.trim()));
        let changed = before != entry.len();
        if entry.is_empty() {
            self.values.remove(&key);
        }
        changed
    }

    pub fn load() -> Self {
        load_json()
    }

    pub fn save(&self) {
        if let Ok(text) = serde_json::to_string_pretty(self) {
            save_json(&text);
        }
    }
}

fn load_json() -> ParamHistory {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::fs::read_to_string(crate::preferences::param_history_path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::load_text(crate::host_storage::StorageArea::Data, "param-history.json")
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
}

fn save_json(text: &str) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = crate::preferences::param_history_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(&path) {
            use std::io::Write;
            let _ = file.write_all(text.as_bytes());
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            "param-history.json",
            text,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_dedupes_case_insensitively_and_moves_to_front() {
        let mut history = ParamHistory::default();
        assert!(history.remember("@id", "42"));
        assert!(history.remember("@id", "99"));
        assert!(history.remember("@id", "abc"));
        assert!(history.remember("@ID", "ABC"));
        let values = history.suggestions("@id");
        assert_eq!(values, &["ABC", "99", "42"]);
    }

    #[test]
    fn remember_caps_at_eight() {
        let mut history = ParamHistory::default();
        for i in 0..10 {
            history.remember("@n", &i.to_string());
        }
        assert_eq!(history.suggestions("@n").len(), MAX_VALUES_PER_NAME);
        assert_eq!(history.suggestions("@n")[0], "9");
        assert_eq!(history.suggestions("@n")[7], "2");
    }

    #[test]
    fn remember_ignores_empty_and_null() {
        let mut history = ParamHistory::default();
        assert!(!history.remember("@x", ""));
        assert!(!history.remember("@x", "   "));
        assert!(!history.remember("@x", "NULL"));
        assert!(!history.remember("@x", "null"));
        assert!(history.values.is_empty());
    }

    #[test]
    fn suggestions_are_case_insensitive_by_name() {
        let mut history = ParamHistory::default();
        history.remember("@CustomerId", "7");
        assert_eq!(history.suggestions("@customerid"), &["7"]);
        assert_eq!(history.suggestions("@CUSTOMERID"), &["7"]);
    }

    #[test]
    fn roundtrip_json() {
        let mut history = ParamHistory::default();
        history.remember("@a", "one");
        history.remember("@b", "two");
        let json = serde_json::to_string(&history).unwrap();
        let back: ParamHistory = serde_json::from_str(&json).unwrap();
        assert_eq!(back, history);
    }
}
