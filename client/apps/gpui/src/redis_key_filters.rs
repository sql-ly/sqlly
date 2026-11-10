//! The Redis key browser's filter memory: the MATCH pattern each logical
//! database was last filtered with, and the last few patterns typed, per
//! connection profile. Kept beside the profile store in its own sidecar
//! file (`redis-key-filters.json`) rather than in the profile, so a filter
//! is a browsing preference — never synced, exported, or shared with a
//! profile — and a profile edit never touches it.
//!
//! The pure store is testable anywhere; the process-wide memory loads it on
//! first use and writes it back off the UI thread after every change, like
//! the value-format memory in `structured_data`.

use std::collections::BTreeMap;

/// Recently typed patterns kept per profile, most recent first.
pub const RECENT_PATTERNS_CAP: usize = 5;

/// One profile's memory: the pattern in force per database, and the
/// patterns typed most recently.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProfileFilters {
    #[serde(default)]
    pub current: BTreeMap<String, String>,
    #[serde(default)]
    pub recent: Vec<String>,
}

/// The on-disk store, keyed by profile id.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RedisFilterStore {
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileFilters>,
}

/// A pattern worth remembering: trimmed, non-empty, and not the match-all.
fn meaningful(pattern: &str) -> Option<&str> {
    let pattern = pattern.trim();
    (!pattern.is_empty() && pattern != "*").then_some(pattern)
}

impl RedisFilterStore {
    pub const FILE: &'static str = "redis-key-filters.json";

    /// The pattern `database` on `profile` was last filtered with.
    pub fn filter(&self, profile: &str, database: &str) -> Option<&str> {
        self.profiles
            .get(profile)?
            .current
            .get(database)
            .map(String::as_str)
    }

    /// The last patterns typed on `profile`, most recent first. The browser
    /// build has no Filter Keys… dialog to show them, so it never asks.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn recent(&self, profile: &str) -> &[String] {
        self.profiles
            .get(profile)
            .map(|filters| filters.recent.as_slice())
            .unwrap_or(&[])
    }

    /// Record the pattern `database` is now filtered with (`None` or an
    /// empty / `*` pattern clears it). A real pattern also moves to the
    /// front of the profile's recent list, deduplicated and capped.
    pub fn set_filter(&mut self, profile: &str, database: &str, pattern: Option<&str>) {
        let pattern = pattern.and_then(meaningful);
        let filters = self.profiles.entry(profile.to_string()).or_default();
        match pattern {
            Some(pattern) => {
                filters
                    .current
                    .insert(database.to_string(), pattern.to_string());
                filters.recent.retain(|known| known != pattern);
                filters.recent.insert(0, pattern.to_string());
                filters.recent.truncate(RECENT_PATTERNS_CAP);
            }
            None => {
                filters.current.remove(database);
            }
        }
        if filters.current.is_empty() && filters.recent.is_empty() {
            self.profiles.remove(profile);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn load_from(path: &std::path::Path) -> Result<Self, String> {
        match crate::fs_safe::load_json_store::<Self>(path) {
            crate::fs_safe::StoreLoad::Missing => Ok(Self::default()),
            crate::fs_safe::StoreLoad::Loaded(store) => Ok(store),
            crate::fs_safe::StoreLoad::Corrupt { error } => Err(crate::localization::format(
                "explorer.redis_key_filters.load_error",
                &[("error", &crate::localization::message(error))],
            )),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn save_to(&self, path: &std::path::Path) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(self).map_err(|error| {
            crate::localization::format(
                "explorer.redis_key_filters.encode_error",
                &[("error", &error.to_string())],
            )
        })?;
        crate::fs_safe::write_atomic(path, &data).map_err(|error| {
            crate::localization::format(
                "explorer.redis_key_filters.save_error",
                &[("error", &error.to_string())],
            )
        })
    }
}

/// The process-wide memory: loaded on first use, written back off the UI
/// thread after every change. An unreadable file blocks writes (the user's
/// filters are in it) so a corrupt store is never overwritten with less.
#[derive(Default)]
struct PersistentFilters {
    loaded: bool,
    store: RedisFilterStore,
    blocked: Option<String>,
}

static PERSISTENT_FILTERS: std::sync::LazyLock<std::sync::Mutex<PersistentFilters>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(PersistentFilters::default()));

/// Serializes background writes so an older snapshot never lands last.
#[cfg(not(target_arch = "wasm32"))]
static PERSISTENT_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where the store lives; `None` in unit tests, which must not touch the
/// user's data directory.
#[cfg(not(target_arch = "wasm32"))]
fn store_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::host_storage::artifact_path(
        crate::host_storage::StorageArea::Data,
        RedisFilterStore::FILE,
    )
}

impl PersistentFilters {
    fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = store_path() {
            match RedisFilterStore::load_from(&path) {
                Ok(store) => self.store = store,
                Err(error) => self.blocked = Some(error),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.store = crate::preferences::wasm_load_json(RedisFilterStore::FILE);
        }
    }
}

/// Write the current store in the background (native) or to browser storage.
fn persist(store: RedisFilterStore) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Some(path) = store_path() else {
            return;
        };
        std::thread::spawn(move || {
            let _serial = PERSISTENT_WRITE
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            // Write the newest state, not necessarily the one this thread was
            // spawned with, so racing writes converge on the latest.
            let latest = PERSISTENT_FILTERS
                .lock()
                .map(|filters| filters.store.clone())
                .unwrap_or(store);
            if let Err(error) = latest.save_to(&path) {
                tracing::warn!(%error, "redis key filter store write failed");
            }
        });
    }
    #[cfg(target_arch = "wasm32")]
    crate::preferences::wasm_save_json(RedisFilterStore::FILE, &store);
}

/// The pattern `database` on `profile` was last filtered with, if any.
pub fn remembered_filter(profile: uuid::Uuid, database: &str) -> Option<String> {
    let mut filters = PERSISTENT_FILTERS.lock().ok()?;
    filters.ensure_loaded();
    filters
        .store
        .filter(&profile.to_string(), database)
        .map(str::to_string)
}

/// The patterns last typed on `profile`, most recent first (the Filter
/// Keys… dialog's dropdown; native only, like the dialog).
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub fn recent_patterns(profile: uuid::Uuid) -> Vec<String> {
    let Ok(mut filters) = PERSISTENT_FILTERS.lock() else {
        return Vec::new();
    };
    filters.ensure_loaded();
    filters.store.recent(&profile.to_string()).to_vec()
}

/// Remember (or clear) the pattern `database` on `profile` is filtered
/// with and write the store back. A store that failed to load is left
/// alone and the reason is logged once per attempt.
pub fn remember_filter(profile: uuid::Uuid, database: &str, pattern: Option<&str>) {
    let Ok(mut filters) = PERSISTENT_FILTERS.lock() else {
        return;
    };
    filters.ensure_loaded();
    if let Some(reason) = &filters.blocked {
        tracing::warn!(%reason, "redis key filter not remembered");
        return;
    }
    filters
        .store
        .set_filter(&profile.to_string(), database, pattern);
    let snapshot = filters.store.clone();
    drop(filters);
    persist(snapshot);
}

// ---------------------------------------------------------------------------
// "That looks like a value query" — the Filter Keys hint
// ---------------------------------------------------------------------------

/// What a Filter Keys pattern looks like when it is really a search for
/// VALUES. Filter Keys is a glob over key NAMES, so either one silently
/// matches nothing; the dialog says so and offers FT.SEARCH instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueQueryKind {
    /// A JSON document or fragment (`{"name": "bob"}`, `"status": "open"`).
    Json,
    /// A search-module query (`@name:bob`, `@age:[30 40]`).
    Search,
}

/// Whether `pattern` reads as a value query rather than a key glob. Kept
/// strict so ordinary globs never trigger it: cluster hash tags
/// (`{user1}:*`), character classes (`user[12]`) and `@` inside a key name
/// (`mail:bob@corp:1`) are key patterns, not queries.
pub fn value_query_kind(pattern: &str) -> Option<ValueQueryKind> {
    let text = pattern.trim();
    if text.is_empty() {
        return None;
    }
    // `@field:` at the start or after a space, `(`, `-` or `|` — the
    // search syntax's field selector.
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'@' || !(i == 0 || matches!(bytes[i - 1], b' ' | b'(' | b'-' | b'|')) {
            continue;
        }
        let name_len = bytes[i + 1..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_' || **c == b'.' || **c == b'$')
            .count();
        if name_len > 0 && bytes.get(i + 1 + name_len) == Some(&b':') {
            return Some(ValueQueryKind::Search);
        }
    }
    // A JSON object or array of objects, or a quoted field followed by `:`.
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.starts_with("{\"")
        || compact.starts_with("[{")
        || compact.starts_with("[\"")
        || !json_pairs(text).is_empty()
    {
        return Some(ValueQueryKind::Json);
    }
    None
}

/// `"field": value` pairs in JSON-looking text, in order: string values
/// unquoted, numbers and `true`/`false`/`null` as written. Nested objects
/// and arrays are skipped (their own pairs still count).
fn json_pairs(text: &str) -> Vec<(String, String, bool)> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let read_string = |from: usize| -> Option<(String, usize)> {
        let mut value = String::new();
        let mut j = from + 1;
        while j < chars.len() {
            match chars[j] {
                '\\' if j + 1 < chars.len() => {
                    value.push(chars[j + 1]);
                    j += 2;
                }
                '"' => return Some((value, j + 1)),
                c => {
                    value.push(c);
                    j += 1;
                }
            }
        }
        None
    };
    while i < chars.len() {
        if chars[i] != '"' {
            i += 1;
            continue;
        }
        let Some((name, mut j)) = read_string(i) else {
            break;
        };
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        if chars.get(j) != Some(&':') || name.is_empty() {
            i = j.max(i + 1);
            continue;
        }
        j += 1;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        match chars.get(j) {
            Some('"') => {
                let Some((value, next)) = read_string(j) else {
                    break;
                };
                out.push((name, value, true));
                i = next;
            }
            Some(c) if c.is_ascii_digit() || *c == '-' || c.is_ascii_alphabetic() => {
                let value: String = chars[j..]
                    .iter()
                    .take_while(|c| !matches!(c, ',' | '}' | ']') && !c.is_whitespace())
                    .collect();
                i = j + value.chars().count();
                out.push((name, value, false));
            }
            _ => i = j,
        }
    }
    out
}

/// A search-module query for what the user typed into Filter Keys: a
/// search query as typed; JSON as one clause per `"field": value` pair
/// (`@name:"bob"`, numbers as a one-value range `@age:[30 30]`); `*` when
/// nothing in it maps to a field.
pub fn search_query_for(pattern: &str) -> String {
    let text = pattern.trim();
    match value_query_kind(text) {
        Some(ValueQueryKind::Search) => text.to_string(),
        _ => {
            let clauses: Vec<String> = json_pairs(text)
                .into_iter()
                .filter(|(_, value, _)| value != "null")
                .map(|(field, value, quoted)| {
                    let field: String = field
                        .chars()
                        .map(|c| {
                            if c.is_alphanumeric() || c == '_' {
                                c
                            } else {
                                '_'
                            }
                        })
                        .collect();
                    if !quoted && value.parse::<f64>().is_ok() {
                        format!("@{field}:[{value} {value}]")
                    } else {
                        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
                        format!("@{field}:\"{escaped}\"")
                    }
                })
                .collect();
            if clauses.is_empty() {
                "*".to_string()
            } else {
                clauses.join(" ")
            }
        }
    }
}

/// The query-tab script for "Search values with FT.SEARCH…": a comment
/// saying why, then a bounded search on the first index (the others named
/// so the user can switch).
pub fn value_search_script(pattern: &str, indexes: &[String]) -> Option<String> {
    let (first, others) = indexes.split_first()?;
    let quote = |s: &str| sqlly_intel::query::safety::redis::quote_argument(s.as_bytes());
    let mut script = crate::localization::format(
        "explorer.redis_key_dialog.value_search_script_comment",
        &[("index", first)],
    )
    .lines()
    .map(|line| format!("# {line}\n"))
    .collect::<String>();
    if !others.is_empty() {
        script.push_str(&format!(
            "# {}\n",
            crate::localization::format(
                "explorer.redis_key_dialog.value_search_other_indexes",
                &[("indexes", &others.join(", "))],
            )
        ));
    }
    script.push_str(&format!(
        "FT.SEARCH {} {} LIMIT 0 100",
        quote(first),
        quote(&search_query_for(pattern))
    ));
    Some(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_and_search_queries_are_detected_but_key_globs_are_not() {
        for json in [
            "{\"name\": \"bob\"}",
            "{ \"name\":\"bob\" }",
            "\"status\": \"open\"",
            "[{\"id\":1}]",
            "  {\"a\":{\"b\":2}}",
        ] {
            assert_eq!(value_query_kind(json), Some(ValueQueryKind::Json), "{json}");
        }
        for search in [
            "@name:bob",
            "@age:[30 40]",
            "hello @tags:{red}",
            "(@a:x | @b:y)",
            "-@done:true",
        ] {
            assert_eq!(
                value_query_kind(search),
                Some(ValueQueryKind::Search),
                "{search}"
            );
        }
        for glob in [
            "",
            "*",
            "user:*",
            "{user1}:*",
            "user:{123}:*",
            "user[12]:*",
            "mail:bob@corp:1",
            "session:*@*",
            "h?llo",
            "\"quoted\"",
            "@",
            "@:x",
        ] {
            assert_eq!(value_query_kind(glob), None, "{glob}");
        }
    }

    #[test]
    fn search_queries_translate_json_pairs_into_field_clauses() {
        assert_eq!(search_query_for("@name:bob"), "@name:bob");
        assert_eq!(search_query_for("{\"name\": \"bob\"}"), "@name:\"bob\"");
        assert_eq!(
            search_query_for("{\"name\": \"Bob Smith\", \"age\": 30, \"note\": null}"),
            "@name:\"Bob Smith\" @age:[30 30]"
        );
        assert_eq!(search_query_for("{\"a\": {\"b\": true}}"), "@b:\"true\"");
        assert_eq!(search_query_for("{\"say\": \"a\\\"b\"}"), "@say:\"a\\\"b\"");
        assert_eq!(search_query_for("{\"user-id\": \"7\"}"), "@user_id:\"7\"");
        assert_eq!(search_query_for("[{}]"), "*");
    }

    #[test]
    fn value_search_script_targets_the_first_index_and_names_the_rest() {
        assert_eq!(value_search_script("@a:b", &[]), None);
        let script = value_search_script(
            "{\"name\": \"bob\"}",
            &["idx:users".to_string(), "idx:orders".to_string()],
        )
        .unwrap();
        let commands = sqlly_intel::query::safety::redis::parse_script(&script).unwrap();
        assert_eq!(commands.len(), 1, "{script}");
        assert_eq!(commands[0].arg_text(0), "FT.SEARCH");
        assert_eq!(commands[0].arg_text(1), "idx:users");
        assert_eq!(commands[0].arg_text(2), "@name:\"bob\"");
        assert_eq!(commands[0].arg_text(3), "LIMIT");
        assert!(script.contains("idx:orders"), "{script}");
        assert!(
            script
                .lines()
                .take_while(|l| !l.starts_with("FT."))
                .all(|l| l.starts_with('#'))
        );
    }

    #[test]
    fn remembers_the_current_pattern_per_database_and_the_last_five_typed() {
        let mut store = RedisFilterStore::default();
        assert_eq!(store.filter("p1", "0"), None);
        assert!(store.recent("p1").is_empty());
        store.set_filter("p1", "0", Some("user:*"));
        store.set_filter("p1", "3", Some("session:*"));
        assert_eq!(store.filter("p1", "0"), Some("user:*"));
        assert_eq!(store.filter("p1", "3"), Some("session:*"));
        assert_eq!(store.recent("p1"), ["session:*", "user:*"]);
        // Another profile has its own memory.
        assert_eq!(store.filter("p2", "0"), None);
        // Re-typing a known pattern moves it to the front, no duplicate.
        store.set_filter("p1", "0", Some("user:*"));
        assert_eq!(store.recent("p1"), ["user:*", "session:*"]);
        for i in 0..6 {
            store.set_filter("p1", "0", Some(&format!("k{i}:*")));
        }
        assert_eq!(store.recent("p1").len(), RECENT_PATTERNS_CAP);
        assert_eq!(store.recent("p1")[0], "k5:*");
        assert!(!store.recent("p1").iter().any(|p| p == "user:*"));
        assert_eq!(store.filter("p1", "0"), Some("k5:*"));
        // Clearing drops the current pattern but keeps the recent list.
        store.set_filter("p1", "0", None);
        assert_eq!(store.filter("p1", "0"), None);
        assert_eq!(store.recent("p1").len(), RECENT_PATTERNS_CAP);
        // Empty and match-all patterns are clears, never remembered.
        store.set_filter("p1", "3", Some("  "));
        store.set_filter("p1", "4", Some("*"));
        assert_eq!(store.filter("p1", "3"), None);
        assert_eq!(store.filter("p1", "4"), None);
        assert!(!store.recent("p1").iter().any(|p| p == "*"));
        store.profiles.clear();
        // A clear on an unknown profile leaves no empty entry behind.
        store.set_filter("p3", "0", None);
        assert!(store.profiles.is_empty());
    }

    #[test]
    fn store_round_trips_through_json_and_tolerates_missing_fields() {
        let mut store = RedisFilterStore::default();
        store.set_filter("p1", "0", Some("user:*"));
        let json = serde_json::to_string(&store).unwrap();
        let back: RedisFilterStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back, store);
        let sparse: RedisFilterStore =
            serde_json::from_str(r#"{"profiles":{"p9":{"recent":["a:*"]}}}"#).unwrap();
        assert_eq!(sparse.recent("p9"), ["a:*"]);
        assert_eq!(sparse.filter("p9", "0"), None);
        let empty: RedisFilterStore = serde_json::from_str("{}").unwrap();
        assert!(empty.profiles.is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn store_loads_saves_and_refuses_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RedisFilterStore::FILE);
        assert_eq!(
            RedisFilterStore::load_from(&path).unwrap(),
            RedisFilterStore::default(),
            "a missing file is an empty store"
        );
        let mut store = RedisFilterStore::default();
        store.set_filter("p1", "2", Some("cache:*"));
        store.save_to(&path).unwrap();
        assert_eq!(RedisFilterStore::load_from(&path).unwrap(), store);
        std::fs::write(&path, b"{not json").unwrap();
        assert!(RedisFilterStore::load_from(&path).is_err());
    }

    #[test]
    fn the_process_memory_never_touches_disk_under_test() {
        let profile = uuid::Uuid::new_v4();
        assert_eq!(remembered_filter(profile, "0"), None);
        remember_filter(profile, "0", Some("orders:*"));
        assert_eq!(remembered_filter(profile, "0").as_deref(), Some("orders:*"));
        assert_eq!(recent_patterns(profile), ["orders:*"]);
        remember_filter(profile, "0", None);
        assert_eq!(remembered_filter(profile, "0"), None);
        assert_eq!(recent_patterns(profile), ["orders:*"]);
    }
}
