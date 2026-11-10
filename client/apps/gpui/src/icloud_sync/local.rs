//! Reading what this Mac holds into sync items, and applying merged items
//! back.
//!
//! Item ids and what they carry:
//!
//! - `connection:<id>`: a saved connection (notes included), as
//!   `connections.json` stores it, minus its health notes and with every secret reference emptied
//!   (see [`super::secrets`]).
//! - `client:<id>`, `project:<id>`, `environment:<id>`: a label with its
//!   name, color, safety switches and icon (embedded, since the icon file's
//!   path only exists on this Mac).
//! - `tunnel:<id>`: a tunnel profile, secrets emptied.
//! - `snippet:<prefix>` and `quick_action:<slot>`: snippets and quick actions.
//! - `preference:<file>`: one of [`SYNCED_SETTINGS`], minus its
//!   [`local-only`](SyncedSettingsFile::local_only) keys.
//! - `library:<relative path>`: a `.sql` file in the SQL Library, when the
//!   library is in its default folder.
//!
//! Shared logins are not synced: they exist to carry a password, and the
//! password never leaves this Mac, so a connection that used one arrives
//! as a plain sign-in that asks for its password.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};
use sqlly_core::credential_import::{decode_image, encode_image_data_uri};
use sqlly_core::{ConnectionProfile, TunnelProfile};
use uuid::Uuid;

use super::document::payload_hash;
use super::merge::{LocalChange, LocalItem, LocalSnapshot, SyncState};
use super::secrets;
use crate::connection::{
    ClientEntity, ConnectionStore, ConnectionStoreData, EnvironmentEntity, ProjectEntity,
    SafetyFeatures,
};
use crate::snippets::{QuickAction, SnippetPreferences, UserSnippet};

/// A settings file that syncs, and the keys in it that stay on this Mac.
#[derive(Debug, Clone, Copy)]
pub struct SyncedSettingsFile {
    pub file_name: &'static str,
    /// Keys that only make sense on this Mac (a file path, a purchase-
    /// dependent switch); neither sent nor overwritten.
    pub local_only: &'static [&'static str],
}

/// The settings that sync. Deliberately an allowlist of files that hold
/// look-and-feel choices only: AI providers and keys, MCP servers, external
/// editors, command-line tools, update and diagnostic settings, history,
/// caches, and anything else that names programs, files or accounts on
/// this Mac stay local.
pub const SYNCED_SETTINGS: &[SyncedSettingsFile] = &[
    SyncedSettingsFile {
        file_name: "appearance-preferences.json",
        local_only: &["customTranslationFile", "keepSupporterMessage"],
    },
    SyncedSettingsFile {
        file_name: "editor-preferences.json",
        local_only: &[],
    },
    SyncedSettingsFile {
        file_name: "sql-formatting.json",
        local_only: &[],
    },
    SyncedSettingsFile {
        file_name: "result-formatting.json",
        local_only: &[],
    },
    SyncedSettingsFile {
        file_name: "conditional-formatting.json",
        local_only: &[],
    },
    SyncedSettingsFile {
        file_name: "masking-rules.json",
        local_only: &[],
    },
    SyncedSettingsFile {
        file_name: "data-model-preferences.json",
        local_only: &[],
    },
    SyncedSettingsFile {
        file_name: "tab-preferences.json",
        local_only: &[],
    },
    SyncedSettingsFile {
        file_name: "transaction-preferences.json",
        local_only: &[],
    },
    SyncedSettingsFile {
        file_name: "keybindings.json",
        local_only: &[],
    },
];

/// The snippets file (in the settings folder), synced item by item.
pub const SNIPPETS_FILE: &str = "snippets-preferences.json";

/// Marks a SQL Library file sync kept beside an incoming version:
/// `<name> (conflicted copy <date> <time>).sql`.
pub const CONFLICT_MARKER: &str = "conflicted copy";

/// Largest SQL Library file that syncs; bigger ones stay on this Mac.
pub const MAX_LIBRARY_FILE_BYTES: u64 = 1024 * 1024;

/// Where this Mac keeps what syncs. Tests point every path at a temporary
/// directory.
pub struct LocalPlaces<'a> {
    pub store: &'a ConnectionStore,
    /// The settings folder (settings files and snippets).
    pub settings_dir: PathBuf,
    /// The SQL Library folder, or `None` when it is not synced (the user
    /// moved it to a folder of their own).
    pub library_root: Option<PathBuf>,
}

/// Domain keys, as [`super::merge::domain_of`] derives them.
const CONNECTION_DOMAINS: [&str; 5] = [
    "connection:",
    "client:",
    "project:",
    "environment:",
    "tunnel:",
];

// ---------------------------------------------------------------------------
// Reading this Mac
// ---------------------------------------------------------------------------

/// Read everything that syncs. A part that cannot be read is left out of
/// `covered`, so sync neither sends nor changes it this pass.
pub fn collect(places: &LocalPlaces<'_>) -> LocalSnapshot {
    let mut snapshot = LocalSnapshot::default();
    if let Some(data) = places.store.sync_snapshot() {
        collect_connections(&data, &mut snapshot);
    }
    collect_snippets(&places.settings_dir, &mut snapshot);
    collect_settings(&places.settings_dir, &mut snapshot);
    if let Some(root) = &places.library_root {
        collect_library(root, &mut snapshot);
    }
    snapshot
}

/// A saved connection as it syncs: no health notes, no secrets.
pub fn connection_payload(profile: &ConnectionProfile) -> Option<Value> {
    let mut value = serde_json::to_value(profile).ok()?;
    if let Value::Object(map) = &mut value {
        map.remove("status");
    }
    secrets::scrub(&mut value);
    // Belt and braces: a value that still holds a secret never leaves.
    secrets::is_secret_free(&value).then_some(value)
}

fn tunnel_payload(profile: &TunnelProfile) -> Option<Value> {
    let mut value = serde_json::to_value(profile).ok()?;
    secrets::scrub(&mut value);
    secrets::is_secret_free(&value).then_some(value)
}

fn entity_payload(
    name: &str,
    color_hex: &Option<String>,
    image_path: &Option<String>,
    safety: &SafetyFeatures,
) -> Value {
    let image = image_path
        .as_deref()
        .filter(|p| !p.trim().is_empty())
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| encode_image_data_uri(&bytes));
    serde_json::json!({
        "name": name,
        "colorHex": color_hex,
        "image": image,
        "safety": safety,
    })
}

fn collect_connections(data: &ConnectionStoreData, snapshot: &mut LocalSnapshot) {
    for profile in &data.profiles {
        if let Some(payload) = connection_payload(profile) {
            snapshot.items.insert(
                format!("connection:{}", profile.id.0),
                LocalItem::Content(payload),
            );
        }
    }
    for e in &data.clients {
        snapshot.items.insert(
            format!("client:{}", e.id),
            LocalItem::Content(entity_payload(
                &e.name,
                &e.color_hex,
                &e.image_path,
                &e.safety,
            )),
        );
    }
    for e in &data.projects {
        snapshot.items.insert(
            format!("project:{}", e.id),
            LocalItem::Content(entity_payload(
                &e.name,
                &e.color_hex,
                &e.image_path,
                &e.safety,
            )),
        );
    }
    for e in &data.environments {
        snapshot.items.insert(
            format!("environment:{}", e.id),
            LocalItem::Content(entity_payload(
                &e.name,
                &e.color_hex,
                &e.image_path,
                &e.safety,
            )),
        );
    }
    for tunnel in &data.tunnel_profiles {
        if let Some(payload) = tunnel_payload(tunnel) {
            snapshot
                .items
                .insert(format!("tunnel:{}", tunnel.id), LocalItem::Content(payload));
        }
    }
    snapshot
        .covered
        .extend(CONNECTION_DOMAINS.iter().map(|d| d.to_string()));
}

/// The snippets file, or `None` when it exists but cannot be read (then
/// snippets are not synced until it is fixed).
fn load_snippets(settings_dir: &Path) -> Option<SnippetPreferences> {
    match crate::fs_safe::load_json_store::<SnippetPreferences>(&settings_dir.join(SNIPPETS_FILE)) {
        crate::fs_safe::StoreLoad::Missing => Some(SnippetPreferences::default()),
        crate::fs_safe::StoreLoad::Loaded(prefs) => Some(prefs),
        crate::fs_safe::StoreLoad::Corrupt { .. } => None,
    }
}

fn snippet_id(prefix: &str) -> Option<String> {
    let prefix = prefix.trim();
    (!prefix.is_empty()).then(|| format!("snippet:{prefix}"))
}

fn collect_snippets(settings_dir: &Path, snapshot: &mut LocalSnapshot) {
    let Some(prefs) = load_snippets(settings_dir) else {
        return;
    };
    for snippet in &prefs.snippets {
        let Some(id) = snippet_id(&snippet.prefix) else {
            continue;
        };
        if snapshot.items.contains_key(&id) {
            continue;
        }
        if let Ok(payload) = serde_json::to_value(snippet) {
            snapshot.items.insert(id, LocalItem::Content(payload));
        }
    }
    for action in &prefs.quick_actions {
        let id = format!("quick_action:{}", action.slot);
        if let Ok(payload) = serde_json::to_value(action) {
            snapshot.items.insert(id, LocalItem::Content(payload));
        }
    }
    snapshot.covered.insert("snippet:".into());
    snapshot.covered.insert("quick_action:".into());
}

/// Read one settings file as it syncs: `Ok(None)` when it does not exist,
/// `Err` when it cannot be used (unreadable, not a JSON object, or holding
/// anything that looks like a secret).
fn read_setting(path: &Path, file: &SyncedSettingsFile) -> Result<Option<Value>, ()> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    let Value::Object(map) = &mut value else {
        return Err(());
    };
    for key in file.local_only {
        map.remove(*key);
    }
    let mut scrubbed = value.clone();
    crate::settings_snapshot::scrub(&mut scrubbed);
    if scrubbed != value {
        return Err(());
    }
    Ok(Some(value))
}

fn collect_settings(settings_dir: &Path, snapshot: &mut LocalSnapshot) {
    for file in SYNCED_SETTINGS {
        let id = format!("preference:{}", file.file_name);
        match read_setting(&settings_dir.join(file.file_name), file) {
            Ok(Some(value)) => {
                snapshot.items.insert(id.clone(), LocalItem::Content(value));
                snapshot.covered.insert(id);
            }
            Ok(None) => {
                snapshot.covered.insert(id);
            }
            Err(()) => {}
        }
    }
}

/// `relative` with `/` separators, if it is a plain path inside the
/// library (no `..`, no hidden or absolute parts) naming a `.sql` file.
pub fn library_id_path(relative: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        let part = part.to_str()?;
        if part.is_empty() || part.starts_with('.') || part.contains('/') || part.contains('\\') {
            return None;
        }
        parts.push(part);
    }
    let joined = parts.join("/");
    let is_sql = Path::new(&joined)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("sql"));
    (is_sql && joined.len() <= 512).then_some(joined)
}

fn collect_library(root: &Path, snapshot: &mut LocalSnapshot) {
    let Ok(tree) = crate::sql_library_model::scan_library(root) else {
        return;
    };
    for entry in tree.entries {
        let Some(relative) = library_id_path(&entry.relative) else {
            continue;
        };
        let id = format!("library:{relative}");
        let too_big = std::fs::metadata(&entry.path)
            .map(|m| m.len() > MAX_LIBRARY_FILE_BYTES)
            .unwrap_or(true);
        let item = match (too_big, std::fs::read_to_string(&entry.path)) {
            (false, Ok(text)) => LocalItem::Content(serde_json::json!({ "text": text })),
            _ => LocalItem::Opaque,
        };
        snapshot.items.insert(id, item);
    }
    snapshot.covered.insert("library:".into());
}

// ---------------------------------------------------------------------------
// Applying to this Mac
// ---------------------------------------------------------------------------

/// What applying changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    /// Changes applied.
    pub applied: usize,
    /// Ids that could not be applied (retried next pass).
    pub failed: BTreeSet<String>,
    /// Which kinds of thing changed, for refreshing the app.
    pub connections_changed: bool,
    pub snippets_changed: bool,
    pub settings_changed: bool,
    pub library_changed: bool,
    /// SQL Library files written beside an incoming version that replaced
    /// edits made on this Mac.
    pub conflicted_copies: Vec<String>,
}

/// Apply `changes`. `state` (the record from the last sync) tells which
/// local copies changed since, so an edited query that loses is kept as a
/// conflicted copy and an edited query is never deleted by sync.
pub fn apply(
    places: &LocalPlaces<'_>,
    snapshot: &LocalSnapshot,
    state: &SyncState,
    changes: &[LocalChange],
    now: i64,
) -> ApplyReport {
    let mut report = ApplyReport::default();
    let mut connection_changes = Vec::new();
    let mut snippet_changes = Vec::new();
    for change in changes {
        let id = change.id();
        let kind = id.split(':').next().unwrap_or_default();
        match kind {
            "connection" | "client" | "project" | "environment" | "tunnel" => {
                connection_changes.push(change)
            }
            "snippet" | "quick_action" => snippet_changes.push(change),
            "preference" => apply_setting(&places.settings_dir, change, &mut report),
            "library" => match &places.library_root {
                Some(root) => apply_library(root, snapshot, state, change, now, &mut report),
                None => {
                    report.failed.insert(id.to_string());
                }
            },
            // A kind a newer version syncs: kept in iCloud, ignored here.
            _ => {
                report.failed.insert(id.to_string());
            }
        }
    }
    if !connection_changes.is_empty() {
        apply_connections(places.store, &connection_changes, &mut report);
    }
    if !snippet_changes.is_empty() {
        apply_snippets(&places.settings_dir, &snippet_changes, &mut report);
    }
    report
}

fn id_suffix(id: &str) -> &str {
    id.split_once(':').map(|(_, rest)| rest).unwrap_or_default()
}

/// A label decoded from its synced form, icon already written to disk.
struct IncomingEntity {
    id: Uuid,
    name: String,
    color_hex: Option<String>,
    image_path: Option<String>,
    safety: SafetyFeatures,
}

fn decode_entity(id: &str, payload: &Value, images_dir: &Path) -> Option<IncomingEntity> {
    let id = Uuid::parse_str(id_suffix(id)).ok()?;
    let name = payload.get("name")?.as_str()?.to_string();
    let color_hex = payload
        .get("colorHex")
        .and_then(Value::as_str)
        .map(str::to_string);
    let safety: SafetyFeatures =
        serde_json::from_value(payload.get("safety").cloned().unwrap_or(Value::Null))
            .unwrap_or_default();
    let image_path = match payload.get("image").and_then(Value::as_str) {
        Some(raw) => {
            let image = decode_image(raw).ok()?;
            Some(crate::credential_import::write_image(images_dir, &image).ok()?)
        }
        None => None,
    };
    Some(IncomingEntity {
        id,
        name,
        color_hex,
        image_path,
        safety,
    })
}

/// A synced connection or tunnel profile as this Mac stores it: this Mac's
/// own secret references kept wherever it already had one.
fn decode_with_local_secrets<T: serde::Serialize + serde::de::DeserializeOwned>(
    payload: &Value,
    local: Option<&T>,
) -> Option<T> {
    let mut value = payload.clone();
    // Whatever the sender wrote, nothing secret is taken from it.
    secrets::scrub(&mut value);
    if let Some(local) = local.and_then(|l| serde_json::to_value(l).ok()) {
        secrets::restore_local(&mut value, &local);
    }
    serde_json::from_value(value).ok()
}

fn apply_connections(store: &ConnectionStore, changes: &[&LocalChange], report: &mut ApplyReport) {
    let images_dir = store.entity_images_dir();
    // Icons are written before taking the store's lock.
    let mut entities: BTreeMap<String, IncomingEntity> = BTreeMap::new();
    for change in changes {
        if let LocalChange::Upsert { id, payload } = change {
            let kind = id.split(':').next().unwrap_or_default();
            if matches!(kind, "client" | "project" | "environment") {
                match decode_entity(id, payload, &images_dir) {
                    Some(entity) => {
                        entities.insert(id.clone(), entity);
                    }
                    None => {
                        report.failed.insert(id.clone());
                    }
                }
            }
        }
    }
    let mut applied = 0usize;
    let mut failed = BTreeSet::new();
    let written = store.apply_sync(|data| {
        for change in changes {
            let id = change.id();
            let kind = id.split(':').next().unwrap_or_default();
            let ok = match (kind, change) {
                ("connection", LocalChange::Upsert { payload, .. }) => {
                    upsert_connection(data, id, payload)
                }
                ("connection", LocalChange::Delete { .. }) => {
                    let target = id_suffix(id);
                    data.profiles.retain(|p| p.id.0.to_string() != target);
                    true
                }
                ("tunnel", LocalChange::Upsert { payload, .. }) => upsert_tunnel(data, id, payload),
                ("tunnel", LocalChange::Delete { .. }) => {
                    let target = id_suffix(id).to_string();
                    data.tunnel_profiles.retain(|t| t.id.to_string() != target);
                    for profile in &mut data.profiles {
                        if profile
                            .tunnel_profile
                            .is_some_and(|t| t.to_string() == target)
                        {
                            profile.tunnel_profile = None;
                        }
                    }
                    true
                }
                ("client" | "project" | "environment", LocalChange::Upsert { .. }) => {
                    match entities.remove(id) {
                        Some(entity) => {
                            upsert_entity(data, kind, entity);
                            true
                        }
                        None => continue,
                    }
                }
                ("client" | "project" | "environment", LocalChange::Delete { .. }) => {
                    let Ok(target) = Uuid::parse_str(id_suffix(id)) else {
                        failed.insert(id.to_string());
                        continue;
                    };
                    match kind {
                        "client" => data.clients.retain(|e| e.id != target),
                        "project" => data.projects.retain(|e| e.id != target),
                        _ => data.environments.retain(|e| e.id != target),
                    }
                    true
                }
                _ => false,
            };
            if ok {
                applied += 1;
            } else {
                failed.insert(id.to_string());
            }
        }
    });
    if !written {
        for change in changes {
            report.failed.insert(change.id().to_string());
        }
        return;
    }
    report.applied += applied;
    report.connections_changed |= applied > 0;
    report.failed.extend(failed);
}

fn upsert_connection(data: &mut ConnectionStoreData, id: &str, payload: &Value) -> bool {
    let Ok(target) = Uuid::parse_str(id_suffix(id)) else {
        return false;
    };
    let index = data.profiles.iter().position(|p| p.id.0 == target);
    let local = index.map(|i| &data.profiles[i]);
    if local.is_some_and(crate::connection::is_provisioned) {
        // Managed by a provisioning file on this Mac.
        return false;
    }
    let Some(mut profile) = decode_with_local_secrets::<ConnectionProfile>(payload, local) else {
        return false;
    };
    if profile.id.0 != target {
        return false;
    }
    if crate::connection::is_provisioned(&profile) {
        return false;
    }
    match index {
        Some(i) => {
            profile.status = data.profiles[i].status.clone();
            data.profiles[i] = profile;
        }
        None => data.profiles.push(profile),
    }
    true
}

fn upsert_tunnel(data: &mut ConnectionStoreData, id: &str, payload: &Value) -> bool {
    let target = id_suffix(id).to_string();
    let index = data
        .tunnel_profiles
        .iter()
        .position(|t| t.id.to_string() == target);
    let local = index.map(|i| &data.tunnel_profiles[i]);
    let Some(profile) = decode_with_local_secrets::<TunnelProfile>(payload, local) else {
        return false;
    };
    if profile.id.to_string() != target {
        return false;
    }
    match index {
        Some(i) => data.tunnel_profiles[i] = profile,
        None => data.tunnel_profiles.push(profile),
    }
    true
}

fn upsert_entity(data: &mut ConnectionStoreData, kind: &str, e: IncomingEntity) {
    macro_rules! upsert {
        ($list:expr, $ty:ident) => {{
            let entity = $ty {
                id: e.id,
                name: e.name,
                color_hex: e.color_hex,
                image_path: e.image_path,
                safety: e.safety,
            };
            match $list.iter_mut().find(|x| x.id == entity.id) {
                Some(existing) => *existing = entity,
                None => $list.push(entity),
            }
        }};
    }
    match kind {
        "client" => upsert!(data.clients, ClientEntity),
        "project" => upsert!(data.projects, ProjectEntity),
        _ => upsert!(data.environments, EnvironmentEntity),
    }
}

fn apply_snippets(settings_dir: &Path, changes: &[&LocalChange], report: &mut ApplyReport) {
    let fail_all = |report: &mut ApplyReport| {
        for change in changes {
            report.failed.insert(change.id().to_string());
        }
    };
    let Some(mut prefs) = load_snippets(settings_dir) else {
        fail_all(report);
        return;
    };
    let mut applied = 0usize;
    for change in changes {
        let id = change.id();
        let ok = if let Some(prefix) = id.strip_prefix("snippet:") {
            match change {
                LocalChange::Upsert { payload, .. } => {
                    match serde_json::from_value::<UserSnippet>(payload.clone()) {
                        Ok(snippet) if snippet.prefix.trim() == prefix => {
                            match prefs
                                .snippets
                                .iter_mut()
                                .find(|s| s.prefix.trim() == prefix)
                            {
                                Some(existing) => *existing = snippet,
                                None => prefs.snippets.push(snippet),
                            }
                            true
                        }
                        _ => false,
                    }
                }
                LocalChange::Delete { .. } => {
                    prefs.snippets.retain(|s| s.prefix.trim() != prefix);
                    true
                }
            }
        } else if let Some(slot) = id
            .strip_prefix("quick_action:")
            .and_then(|s| s.parse::<u8>().ok())
        {
            match change {
                LocalChange::Upsert { payload, .. } => {
                    match serde_json::from_value::<QuickAction>(payload.clone()) {
                        Ok(action) if action.slot == slot => {
                            match prefs.quick_actions.iter_mut().find(|a| a.slot == slot) {
                                Some(existing) => *existing = action,
                                None => {
                                    prefs.quick_actions.push(action);
                                    prefs.quick_actions.sort_by_key(|a| a.slot);
                                }
                            }
                            true
                        }
                        _ => false,
                    }
                }
                LocalChange::Delete { .. } => {
                    prefs.quick_actions.retain(|a| a.slot != slot);
                    true
                }
            }
        } else {
            false
        };
        if ok {
            applied += 1;
        } else {
            report.failed.insert(id.to_string());
        }
    }
    let saved = crate::config_version::to_vec_pretty(&prefs)
        .ok()
        .is_some_and(|bytes| {
            crate::fs_safe::write_atomic(&settings_dir.join(SNIPPETS_FILE), &bytes).is_ok()
        });
    if saved {
        report.applied += applied;
        report.snippets_changed |= applied > 0;
    } else {
        fail_all(report);
    }
}

fn apply_setting(settings_dir: &Path, change: &LocalChange, report: &mut ApplyReport) {
    let id = change.id();
    let Some(file) = SYNCED_SETTINGS
        .iter()
        .find(|f| id_suffix(id) == f.file_name)
    else {
        report.failed.insert(id.to_string());
        return;
    };
    let path = settings_dir.join(file.file_name);
    let ok = match change {
        LocalChange::Delete { .. } => match std::fs::remove_file(&path) {
            Ok(()) => true,
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        },
        LocalChange::Upsert { payload, .. } => write_setting(&path, file, payload),
    };
    if ok {
        report.applied += 1;
        report.settings_changed = true;
    } else {
        report.failed.insert(id.to_string());
    }
}

fn write_setting(path: &Path, file: &SyncedSettingsFile, payload: &Value) -> bool {
    let Value::Object(incoming) = payload else {
        return false;
    };
    let stamped = file.file_name != "keybindings.json"
        && crate::config_version::SETTINGS_FILES.contains(&file.file_name);
    let mut value = Value::Object(incoming.clone());
    if stamped {
        // A newer version of SQLLY wrote it: this build might not read it
        // correctly, so it waits for this Mac to be updated.
        if crate::config_version::settings_version(&value) > crate::config_version::CONFIG_VERSION {
            return false;
        }
        crate::config_version::migrate_settings(&mut value);
    }
    // Keys that stay on this Mac keep this Mac's values.
    let local: Map<String, Value> = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|v| match v {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default();
    if let Value::Object(map) = &mut value {
        for key in file.local_only {
            map.remove(*key);
            if let Some(local_value) = local.get(*key) {
                map.insert((*key).to_string(), local_value.clone());
            }
        }
    }
    serde_json::to_vec_pretty(&value)
        .ok()
        .is_some_and(|bytes| crate::fs_safe::write_atomic(path, &bytes).is_ok())
}

/// `<stem> (conflicted copy 2026-11-09 1432).sql`, numbered if taken.
fn conflicted_copy_path(path: &Path, now: i64) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("query")
        .to_string();
    let stamp = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H%M")
                .to_string()
        })
        .unwrap_or_default();
    // Part of a file name, so a fixed marker rather than translated text
    // (translations are never persisted).
    let label = CONFLICT_MARKER;
    let mut n = 1;
    loop {
        let name = if n == 1 {
            format!("{stem} ({label} {stamp}).sql")
        } else {
            format!("{stem} ({label} {stamp} {n}).sql")
        };
        let candidate = path.with_file_name(name);
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

fn apply_library(
    root: &Path,
    snapshot: &LocalSnapshot,
    state: &SyncState,
    change: &LocalChange,
    now: i64,
    report: &mut ApplyReport,
) {
    let id = change.id();
    let Some(relative) = library_id_path(Path::new(id_suffix(id))) else {
        report.failed.insert(id.to_string());
        return;
    };
    let path = root.join(&relative);
    // Whether this Mac's copy changed since the last sync (or was never
    // synced): such a copy is never thrown away.
    let local_edited = snapshot.content(id).is_some_and(|local| {
        state
            .items
            .get(id)
            .is_none_or(|entry| entry.local_hash != payload_hash(local))
    });
    let ok = match change {
        LocalChange::Delete { .. } => {
            if local_edited {
                false
            } else {
                match std::fs::remove_file(&path) {
                    Ok(()) => true,
                    Err(error) => error.kind() == std::io::ErrorKind::NotFound,
                }
            }
        }
        LocalChange::Upsert { payload, .. } => {
            let Some(text) = payload.get("text").and_then(Value::as_str) else {
                report.failed.insert(id.to_string());
                return;
            };
            if local_edited
                && let Some(local_text) = snapshot
                    .content(id)
                    .and_then(|v| v.get("text"))
                    .and_then(Value::as_str)
            {
                let copy = conflicted_copy_path(&path, now);
                if crate::fs_safe::write_atomic(&copy, local_text.as_bytes()).is_err() {
                    report.failed.insert(id.to_string());
                    return;
                }
                if let Ok(rel) = copy.strip_prefix(root) {
                    report
                        .conflicted_copies
                        .push(rel.to_string_lossy().into_owned());
                }
            }
            crate::fs_safe::write_atomic(&path, text.as_bytes()).is_ok()
        }
    };
    if ok {
        report.applied += 1;
        report.library_changed = true;
    } else {
        report.failed.insert(id.to_string());
    }
}
