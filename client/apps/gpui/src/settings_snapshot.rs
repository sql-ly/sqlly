//! The app's configuration settings, captured for feedback reports (Send
//! Feedback and Improve IntelliSense) so a reviewer can reproduce the user's
//! setup.
//!
//! Only the settings files named in [`SETTINGS_FILES`] are ever read. The
//! stores that hold connection details, sign-in accounts, history, sessions,
//! and caches are not on that list, and credentials and API keys live in the
//! OS keychain, which this module never touches. On top of that, every value
//! is scrubbed before it leaves: fields whose names mark them as secrets are
//! replaced with [`REDACTED`], URLs lose their user-info and query strings,
//! and strings shaped like API keys or bearer tokens are redacted wherever
//! they appear.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What both feedback dialogs tell the user about the settings they send.
pub const DISCLOSURE: &str = "Your app configuration settings are included in the report to help us \
     reproduce the problem. None of your connection credentials or API keys are included.";

/// Stand-in for every scrubbed value.
pub const REDACTED: &str = "[redacted]";

/// Largest single settings file carried as-is; anything bigger is listed in
/// `omitted` instead.
pub const MAX_FILE_BYTES: usize = 64 * 1024;

/// Every settings file a report may carry. Deliberately an allowlist: a new
/// store is excluded until it is added here, so nothing that holds
/// connections, accounts, history, or session state can leak in by default.
pub const SETTINGS_FILES: &[&str] = &[
    "agent-preferences.json",
    "ai-profiles.json",
    "ai-summary-preferences.json",
    "appearance-preferences.json",
    "cli-agent.json",
    "cloud-ai.json",
    "conditional-formatting.json",
    "data-model-preferences.json",
    "ddl-navigation.json",
    "decision-ai.json",
    "diagnostic-logging.json",
    "editor-keybindings.json",
    "editor-preferences.json",
    "explain.json",
    "export-options.json",
    "export.json",
    "external-editor.json",
    "fim.json",
    "gesture-preferences.json",
    "keybindings.json",
    "kubectl-preferences.json",
    "masking-rules.json",
    "mcp-servers.json",
    "native-tools.json",
    "os-notifications.json",
    "performance-preferences.json",
    "query-history-preferences.json",
    "result-cache-preferences.json",
    "result-formatting.json",
    "snippets-preferences.json",
    "sql-formatting.json",
    "sql-library-preferences.json",
    "tab-preferences.json",
    "tip-of-the-day.json",
    "transaction-preferences.json",
    "update-preferences.json",
];

/// The user's saved settings at capture time, scrubbed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsSnapshot {
    /// File name → the file's parsed, scrubbed JSON.
    pub files: BTreeMap<String, Value>,
    /// Settings files with no saved copy: those settings are at their
    /// built-in defaults.
    #[serde(default)]
    pub defaults: Vec<String>,
    /// Files that exist but were left out, with the reason (unreadable, not
    /// JSON, too large for the report).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub omitted: BTreeMap<String, String>,
}

impl SettingsSnapshot {
    /// Drop the largest files (recording each in `omitted`) until the
    /// snapshot serializes within `budget` bytes of JSON.
    pub fn fit_within(&mut self, budget: usize) {
        while serialized_len(self) > budget {
            let Some(largest) = self
                .files
                .iter()
                .max_by_key(|(_, value)| serialized_len(value))
                .map(|(name, value)| (name.clone(), serialized_len(value)))
            else {
                return;
            };
            self.files.remove(&largest.0);
            self.omitted.insert(
                largest.0,
                crate::localization::format(
                    "settings.advanced.feedback_snapshot.file_too_large",
                    &[("bytes", &largest.1.to_string())],
                ),
            );
        }
    }
}

fn serialized_len<T: Serialize>(value: &T) -> usize {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .unwrap_or(0)
}

/// Read and scrub every allowlisted settings file from the app's storage.
pub fn capture() -> SettingsSnapshot {
    use crate::host_storage::{StorageArea, load_text};
    // Most stores live under the data root; keybindings and a few others use
    // the config root (the same directory on macOS and Windows).
    capture_from(|name| {
        load_text(StorageArea::Data, name).or_else(|| load_text(StorageArea::Config, name))
    })
}

/// [`capture`] over an arbitrary loader (file name → contents). Pure for
/// testing.
pub fn capture_from(load: impl Fn(&str) -> Option<String>) -> SettingsSnapshot {
    let mut snapshot = SettingsSnapshot::default();
    for &name in SETTINGS_FILES {
        let Some(text) = load(name) else {
            snapshot.defaults.push(name.to_string());
            continue;
        };
        if text.len() > MAX_FILE_BYTES {
            snapshot.omitted.insert(
                name.to_string(),
                crate::localization::format(
                    "settings.advanced.feedback_snapshot.file_too_large",
                    &[("bytes", &text.len().to_string())],
                ),
            );
            continue;
        }
        match serde_json::from_str::<Value>(&text) {
            Ok(mut value) => {
                scrub(&mut value);
                snapshot.files.insert(name.to_string(), value);
            }
            Err(_) => {
                snapshot.omitted.insert(
                    name.to_string(),
                    crate::localization::text("settings.advanced.feedback_snapshot.invalid_json"),
                );
            }
        }
    }
    snapshot
}

/// Recursively scrub secrets out of a settings value in place.
pub fn scrub(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                // A flag such as `apiKeyConfigured: true` reveals nothing.
                if is_secret_key(key) && !matches!(child, Value::Bool(_) | Value::Null) {
                    *child = Value::String(REDACTED.to_string());
                } else {
                    scrub(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(scrub),
        Value::String(text) => {
            if let Some(clean) = scrub_string(text) {
                *text = clean;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Field names that hold (or may hold) a credential. Matched on the
/// lowercased name with separators removed, so `api_key`, `apiKey`, and
/// `API-KEY` all match.
fn is_secret_key(key: &str) -> bool {
    const CONTAINS: &[&str] = &[
        "password",
        "passwd",
        "passphrase",
        "secret",
        "apikey",
        "accesskey",
        "privatekey",
        "credential",
        "bearer",
        "cookie",
        "connectionstring",
        "authorization",
        "sessionkey",
    ];
    // Free-form fields that commonly carry secrets inline: process
    // environment, HTTP headers, command-line arguments.
    const EXACT: &[&str] = &["env", "environment", "headers", "auth", "args", "pwd"];
    let normalized: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    CONTAINS.iter().any(|word| normalized.contains(word))
        || normalized.ends_with("token")
        || EXACT.contains(&normalized.as_str())
}

/// A scrubbed replacement for `text`, or `None` when it is fine as-is.
fn scrub_string(text: &str) -> Option<String> {
    if looks_like_secret(text) {
        return Some(REDACTED.to_string());
    }
    scrub_url(text)
}

/// Strings that are credentials whatever field they sit in: well-known API
/// key and token shapes, bearer headers, JWTs, and connection strings with an
/// inline password.
fn looks_like_secret(text: &str) -> bool {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("bearer ") || lower.starts_with("basic ") {
        return true;
    }
    if ["password=", "pwd=", "passwd="]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return true;
    }
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_",
        "pk_live_",
        "rk_live_",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "akia",
        "asia",
        "aiza",
        "eyj",
    ];
    trimmed.len() >= 16
        && !trimmed.contains(char::is_whitespace)
        && PREFIXES.iter().any(|prefix| lower.starts_with(prefix))
}

/// `scheme://user:pass@host/path?query#frag` → `scheme://host/path` with
/// user-info dropped and any query or fragment replaced by [`REDACTED`].
/// `None` when `text` is not a URL or needs no change.
fn scrub_url(text: &str) -> Option<String> {
    let scheme_end = text.find("://")?;
    let scheme = &text[..scheme_end];
    if scheme.is_empty()
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    let rest = &text[scheme_end + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (path, query) = match tail.find(['?', '#']) {
        Some(at) => (&tail[..at], true),
        None => (tail, false),
    };
    let clean = format!(
        "{scheme}://{host}{path}{}",
        if query {
            format!("?{REDACTED}")
        } else {
            String::new()
        }
    );
    (clean != text).then_some(clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_allowlisted_files_are_read() {
        let asked = std::cell::RefCell::new(Vec::new());
        capture_from(|name| {
            asked.borrow_mut().push(name.to_string());
            None
        });
        let asked = asked.into_inner();
        assert_eq!(asked.len(), SETTINGS_FILES.len());
        for store in [
            "connections.json",
            "connection-store.json",
            "accounts.json",
            "entra-accounts.json",
            "workspace-session.json",
            "param-history.json",
            "tenant-scoping.json",
            "recent-files.json",
        ] {
            assert!(
                !asked.iter().any(|n| n == store),
                "{store} must never be read"
            );
        }
    }

    #[test]
    fn missing_files_are_defaults_and_bad_files_are_omitted() {
        let snapshot = capture_from(|name| match name {
            "editor-preferences.json" => Some(r#"{ "fontSize": 13 }"#.to_string()),
            "fim.json" => Some("{ not json".to_string()),
            "keybindings.json" => Some(format!("\"{}\"", "x".repeat(MAX_FILE_BYTES))),
            _ => None,
        });
        assert_eq!(
            snapshot.files["editor-preferences.json"],
            json!({ "fontSize": 13 })
        );
        assert_eq!(snapshot.omitted["fim.json"], "not valid JSON");
        assert!(snapshot.omitted["keybindings.json"].starts_with("too large"));
        assert!(
            snapshot
                .defaults
                .contains(&"appearance-preferences.json".to_string())
        );
        assert!(
            !snapshot
                .defaults
                .contains(&"editor-preferences.json".to_string())
        );
    }

    #[test]
    fn secret_fields_are_redacted_but_flags_and_settings_survive() {
        let mut value = json!({
            "apiKey": "abc",
            "api_key_ref": "ai-profile.1",
            "apiKeyConfigured": true,
            "clientSecret": "s3cret",
            "refreshToken": "r",
            "maxNewTokens": 128,
            "disableOnTokens": ["//", "#"],
            "env": "OPENAI_API_KEY=abc",
            "args": "--token abc",
            "password": null,
            "nested": [{ "Authorization": "x", "model": "gpt" }],
            "theme": "dark",
        });
        scrub(&mut value);
        assert_eq!(value["apiKey"], REDACTED);
        assert_eq!(value["api_key_ref"], REDACTED);
        assert_eq!(value["apiKeyConfigured"], true);
        assert_eq!(value["clientSecret"], REDACTED);
        assert_eq!(value["refreshToken"], REDACTED);
        assert_eq!(value["maxNewTokens"], 128);
        assert_eq!(value["disableOnTokens"], json!(["//", "#"]));
        assert_eq!(value["env"], REDACTED);
        assert_eq!(value["args"], REDACTED);
        assert!(value["password"].is_null());
        assert_eq!(value["nested"][0]["Authorization"], REDACTED);
        assert_eq!(value["nested"][0]["model"], "gpt");
        assert_eq!(value["theme"], "dark");
    }

    #[test]
    fn secret_shaped_strings_are_redacted_in_any_field() {
        let mut value = json!({
            "a": "sk-ant-api03-abcdefghijklmnop",
            "b": "Bearer abc.def",
            "c": "Server=db;User Id=sa;Password=hunter2",
            "d": "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig",
            "e": "sk-",
            "f": "Skip the first row",
        });
        scrub(&mut value);
        for key in ["a", "b", "c", "d"] {
            assert_eq!(value[key], REDACTED, "{key}");
        }
        assert_eq!(value["e"], "sk-", "too short to be a key");
        assert_eq!(value["f"], "Skip the first row");
    }

    #[test]
    fn urls_lose_user_info_and_query() {
        let mut value = json!({
            "proxy": "http://bob:pw@proxy.local:8080",
            "baseUrl": "https://api.example.com/v1?key=abc#frag",
            "plain": "https://api.openai.com/v1",
            "notUrl": "a :// b",
        });
        scrub(&mut value);
        assert_eq!(value["proxy"], "http://proxy.local:8080");
        assert_eq!(value["baseUrl"], "https://api.example.com/v1?[redacted]");
        assert_eq!(value["plain"], "https://api.openai.com/v1");
        assert_eq!(value["notUrl"], "a :// b");
    }

    #[test]
    fn fit_within_drops_the_largest_files_first() {
        let mut snapshot = capture_from(|name| match name {
            "editor-preferences.json" => Some(r#"{ "a": 1 }"#.to_string()),
            "masking-rules.json" => Some(format!(r#"{{ "rules": "{}" }}"#, "x".repeat(4000))),
            _ => None,
        });
        let budget = serialized_len(&snapshot) - 1000;
        snapshot.fit_within(budget);
        assert!(serialized_len(&snapshot) <= budget);
        assert!(snapshot.files.contains_key("editor-preferences.json"));
        assert!(snapshot.omitted["masking-rules.json"].starts_with("too large"));
    }
}
