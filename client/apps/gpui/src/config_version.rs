//! Shared version stamp for the app's config files: `connections.json`,
//! `tenant-scoping.json`, and every settings file under the preferences
//! directory.
//!
//! Every file is written at [`CONFIG_VERSION`]. On startup each file is
//! checked: one written at an older version (or with no version at all)
//! gets the migrations it is missing applied, then is rewritten at the
//! current version, so each migration runs exactly once per file.
//!
//! Connections and tenant scoping keep their typed top-level `version`
//! field. Settings files are plain preference structs, so their stamp is an
//! extra top-level [`VERSION_KEY`] that the structs ignore on load and
//! [`to_vec_pretty`] adds back on every save.
//!
//! Migrations:
//! - **2** — tenant scoping becomes opt-in. Anything written before this
//!   version loses every tenant-scope setting, including explicit Warn and
//!   Required levels.
//! - **3** — JetBrains Mono becomes the default editor font. Anything
//!   written before this version has its editor font reset to it, whatever
//!   was picked before; later picks are kept.

use serde::Serialize;
use serde_json::Value;

/// The current config version written to every config file.
pub const CONFIG_VERSION: u32 = 3;

/// Files older than this lose all tenant-scope configuration on load.
pub const TENANT_SCOPE_RESET_VERSION: u32 = 2;

/// Files older than this have their editor font reset to
/// [`crate::DEFAULT_EDITOR_FONT`] on load.
pub const EDITOR_FONT_RESET_VERSION: u32 = 3;

/// The appearance-settings key holding the editor font family.
const EDITOR_FONT_KEY: &str = "editorFont";

/// The version key stamped onto settings files.
pub const VERSION_KEY: &str = "configVersion";

/// Settings files (relative to the preferences directory) that carry the
/// stamp and are migrated on startup. Every file here is a JSON object
/// whose loader tolerates unknown keys and falls back to defaults, and whose
/// save goes through [`to_vec_pretty`] / [`to_string_pretty`] (natively or
/// via `wasm_save_json`) so the stamp survives the next write.
///
/// Deliberately not listed:
/// - `connections.json` and `tenant-scoping.json`: typed top-level
///   `version` field, migrated by their own loaders.
/// - `keybindings.json`, `entra-accounts.json`, `query-ai-summary.json`,
///   `ai-templates.json`: live in the config directory (a different
///   directory from the data directory on Linux and in the browser), which
///   the startup pass does not read; `keybindings.json` also has its own
///   `version` field and migration.
/// - `mcp-servers.json`: written owner-only and atomically because it can
///   hold server command lines; the startup rewrite would widen that.
/// - `editor-keybindings.json` and `sandbox-bookmarks.json`: top-level maps
///   whose values are strings — a numeric version entry would fail the
///   whole file's parse and reset it.
/// - `azure-discoveries.json`, `explorer-favorites.json`, `explorer-pin-groups.json`,
///   `query-history-pins.json`, `recent-files.json`: top-level arrays,
///   which cannot carry a key. These, and the per-feature data stores
///   (saved filters, column layouts, dump schedules, import profiles, param
///   history, command-palette recents, workspace session, value formats,
///   user templates, AI profiles), are user data rather than settings.
pub const SETTINGS_FILES: &[&str] = &[
    "agent-preferences.json",
    "ai-summary-preferences.json",
    "appearance-preferences.json",
    "cli-agent.json",
    "cloud-ai.json",
    "conditional-formatting.json",
    "data-model-preferences.json",
    "ddl-navigation.json",
    "decision-ai.json",
    "detached-results.json",
    "diagnostic-logging.json",
    "editor-preferences.json",
    "explain.json",
    "export-options.json",
    "export.json",
    "external-editor.json",
    "fim.json",
    "gesture-preferences.json",
    "kubectl-preferences.json",
    "masking-rules.json",
    "native-tools.json",
    "os-notifications.json",
    "performance-preferences.json",
    "preferences-ui-state.json",
    "query-history-preferences.json",
    "query-resume.json",
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

/// Serialize a settings struct with the current version stamped on it.
pub fn to_vec_pretty<T: Serialize>(value: &T) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec_pretty(&stamped(value)?)
}

/// String form of [`to_vec_pretty`] (browser localStorage).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn to_string_pretty<T: Serialize>(value: &T) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&stamped(value)?)
}

fn stamped<T: Serialize>(value: &T) -> serde_json::Result<Value> {
    let mut json = serde_json::to_value(value)?;
    if let Value::Object(map) = &mut json {
        map.insert(VERSION_KEY.to_string(), Value::from(CONFIG_VERSION));
    }
    Ok(json)
}

/// The version a settings document was written at (0 when unstamped).
pub fn settings_version(json: &Value) -> u32 {
    json.get(VERSION_KEY)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0)
}

/// Bring one settings document up to [`CONFIG_VERSION`]. Returns whether it
/// changed and must be rewritten. Non-object documents are left alone.
pub fn migrate_settings(json: &mut Value) -> bool {
    if !json.is_object() {
        return false;
    }
    let version = settings_version(json);
    if version >= CONFIG_VERSION {
        return false;
    }
    if version < TENANT_SCOPE_RESET_VERSION {
        strip_tenant_scope_keys(json);
    }
    if let Value::Object(map) = json {
        if version < EDITOR_FONT_RESET_VERSION && map.contains_key(EDITOR_FONT_KEY) {
            map.insert(
                EDITOR_FONT_KEY.to_string(),
                Value::from(crate::DEFAULT_EDITOR_FONT),
            );
        }
        map.insert(VERSION_KEY.to_string(), Value::from(CONFIG_VERSION));
    }
    true
}

/// Whether a JSON key names tenant-scope configuration (requirement, level,
/// discriminator column, or filter). Entra/Azure `tenantId` account fields
/// are identity, not scoping, and are kept.
fn is_tenant_scope_key(key: &str) -> bool {
    let folded: String = key
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect();
    [
        "tenantscope",
        "tenantscoping",
        "requiretenant",
        "failclosedtenant",
        "tenantfilter",
        "tenantcolumn",
        "tenantmode",
    ]
    .iter()
    .any(|marker| folded.contains(marker))
}

fn strip_tenant_scope_keys(json: &mut Value) {
    match json {
        Value::Object(map) => {
            map.retain(|key, _| !is_tenant_scope_key(key));
            for value in map.values_mut() {
                strip_tenant_scope_keys(value);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_tenant_scope_keys),
        _ => {}
    }
}

/// Migrate every settings file once per process. Runs before any settings
/// are loaded; a file that is missing or not valid JSON is left untouched
/// (its loader already falls back to defaults).
pub fn migrate_settings_files() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        use crate::host_storage::{StorageArea, load_text, store_text};
        for name in SETTINGS_FILES {
            let Some(text) = load_text(StorageArea::Data, name) else {
                continue;
            };
            let Ok(mut json) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if !migrate_settings(&mut json) {
                continue;
            }
            match serde_json::to_string_pretty(&json) {
                Ok(text) => {
                    if let Err(error) = store_text(StorageArea::Data, name, &text) {
                        tracing::warn!(file = name, %error, "could not rewrite migrated settings");
                    }
                }
                Err(error) => {
                    tracing::warn!(file = name, %error, "could not serialize migrated settings");
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unstamped_settings_lose_tenant_scope_keys_and_gain_the_version() {
        let mut doc = json!({
            "fontSize": 13,
            "tenantScope": "required",
            "require_tenant_filter": true,
            "nested": { "tenantColumn": "TenantId", "keep": 1 },
            "accounts": [{ "tenantId": "t-1", "tenant_scope_mode": "warn" }]
        });
        assert!(migrate_settings(&mut doc));
        assert_eq!(
            doc,
            json!({
                "fontSize": 13,
                "nested": { "keep": 1 },
                "accounts": [{ "tenantId": "t-1" }],
                "configVersion": CONFIG_VERSION
            })
        );
    }

    #[test]
    fn current_settings_are_untouched() {
        let mut doc = json!({ "configVersion": CONFIG_VERSION, "tenantScope": "warn" });
        let before = doc.clone();
        assert!(!migrate_settings(&mut doc));
        assert_eq!(doc, before);
    }

    #[test]
    fn version_one_settings_are_migrated() {
        let mut doc = json!({ "configVersion": 1, "requireTenantFilter": true });
        assert!(migrate_settings(&mut doc));
        assert_eq!(doc, json!({ "configVersion": CONFIG_VERSION }));
    }

    #[test]
    fn older_settings_get_the_default_editor_font() {
        let mut doc = json!({ "configVersion": 2, "editorFont": "Menlo", "editorFontSize": 14 });
        assert!(migrate_settings(&mut doc));
        assert_eq!(
            doc,
            json!({
                "configVersion": CONFIG_VERSION,
                "editorFont": crate::DEFAULT_EDITOR_FONT,
                "editorFontSize": 14
            })
        );

        let mut unstamped = json!({ "editorFont": "Fira Code" });
        assert!(migrate_settings(&mut unstamped));
        assert_eq!(unstamped["editorFont"], crate::DEFAULT_EDITOR_FONT);
    }

    #[test]
    fn a_font_picked_after_the_reset_is_kept() {
        let mut doc = json!({ "configVersion": CONFIG_VERSION, "editorFont": "Menlo" });
        assert!(!migrate_settings(&mut doc));
        assert_eq!(doc["editorFont"], "Menlo");
    }

    #[test]
    fn settings_without_an_editor_font_do_not_gain_one() {
        let mut doc = json!({ "configVersion": 2, "fontSize": 13 });
        assert!(migrate_settings(&mut doc));
        assert_eq!(
            doc,
            json!({ "configVersion": CONFIG_VERSION, "fontSize": 13 })
        );
    }

    #[test]
    fn non_object_documents_are_left_alone() {
        let mut doc = json!(["tenantScope"]);
        assert!(!migrate_settings(&mut doc));
    }

    #[derive(Serialize, serde::Deserialize, Default, PartialEq, Debug)]
    #[serde(default)]
    struct Prefs {
        size: u32,
    }

    #[test]
    fn every_settings_file_is_listed_once_and_takes_the_stamp() {
        let mut seen = std::collections::BTreeSet::new();
        for name in SETTINGS_FILES {
            assert!(seen.insert(*name), "{name} listed twice");
            assert!(name.ends_with(".json"), "{name}");
            // An unstamped object (what every loader reads) gains the
            // version; a stamped one is left alone.
            let mut doc = json!({});
            assert!(migrate_settings(&mut doc), "{name}");
            assert_eq!(settings_version(&doc), CONFIG_VERSION, "{name}");
            assert!(!migrate_settings(&mut doc), "{name}");
        }
        let mut sorted: Vec<&str> = SETTINGS_FILES.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, SETTINGS_FILES, "keep SETTINGS_FILES sorted");
    }

    /// Each settings struct whose save newly routes through the stamp:
    /// reads back its own stamped bytes, and still reads a file written
    /// before the stamp existed (no key at all).
    fn round_trips<T>()
    where
        T: Serialize + serde::de::DeserializeOwned + Default + PartialEq + std::fmt::Debug,
    {
        let bytes = to_vec_pretty(&T::default()).unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(settings_version(&json), CONFIG_VERSION);
        let back: T = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, T::default());
        let unstamped: T = serde_json::from_str("{}").unwrap();
        assert_eq!(unstamped, T::default());
    }

    #[test]
    fn newly_stamped_settings_structs_load_stamped_and_unstamped_files() {
        round_trips::<crate::gesture_prefs::GesturePreferences>();
        round_trips::<crate::external_editor::ExternalEditorPreferences>();
        round_trips::<crate::fim_completion::FimPreferences>();
        round_trips::<crate::cloud_ai::CloudAIConfig>();
        round_trips::<crate::local_cli_agent_config::LocalCliAgentConfig>();
        round_trips::<crate::decision_ai::DecisionAiConfig>();
        round_trips::<crate::snippets::SnippetPreferences>();
        round_trips::<crate::tip_of_the_day::TipPreferences>();
    }

    #[test]
    fn stamped_settings_round_trip_through_their_struct() {
        let bytes = to_vec_pretty(&Prefs { size: 7 }).unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(settings_version(&json), CONFIG_VERSION);
        let back: Prefs = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, Prefs { size: 7 });
    }
}
