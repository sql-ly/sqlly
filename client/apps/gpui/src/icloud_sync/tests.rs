//! iCloud sync tests. A temporary directory stands in for iCloud Drive and
//! each "Mac" gets its own temporary settings folder, connection store and
//! SQL Library, so two-Mac scenarios run on one machine.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sqlly_core::{
    ConnectionProfile, ProxyHop, ProxyKind, SecretProvider, SecretRef, SshAuth, SshHop,
    SshTunnelConfig, TransportChain,
};
use uuid::Uuid;

use super::document::{
    SYNC_FORMAT, SyncDocument, SyncItem, device_file_name, parse_document, payload_hash,
    read_devices_folder,
};
use super::engine::{PassError, PassInput, STATE_FILE, run_pass};
use super::local::{LocalPlaces, connection_payload};
use super::location::{Availability, detect_in_home};
use super::merge::{
    LocalItem, LocalSnapshot, StateEntry, SyncState, local_entries, merge, plan_local_changes, wins,
};
use super::{ICloudSyncPreferences, secrets};
use crate::connection::ConnectionStore;

const PASSWORD: &str = "hunter2-do-not-sync";
const PASSPHRASE: &str = "ssh-passphrase-do-not-sync";
const PROXY_PASSWORD: &str = "proxy-password-do-not-sync";

/// One simulated Mac.
struct Mac {
    _dir: tempfile::TempDir,
    root: PathBuf,
    store: ConnectionStore,
}

impl Mac {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join("settings")).unwrap();
        std::fs::create_dir_all(root.join("library")).unwrap();
        let store = ConnectionStore::from_path(root.join("config").join("connections.json"));
        Self {
            _dir: dir,
            root,
            store,
        }
    }

    fn settings(&self) -> PathBuf {
        self.root.join("settings")
    }

    fn library(&self) -> PathBuf {
        self.root.join("library")
    }

    fn sync(&self, cloud: &Path, now: i64) -> super::engine::PassReport {
        self.try_sync(cloud, now).expect("pass")
    }

    fn try_sync(&self, cloud: &Path, now: i64) -> Result<super::engine::PassReport, PassError> {
        run_pass(&PassInput {
            places: LocalPlaces {
                store: &self.store,
                settings_dir: self.settings(),
                library_root: Some(self.library()),
            },
            devices_dir: cloud.to_path_buf(),
            state_path: self.settings().join(STATE_FILE),
            now,
        })
    }
}

fn keychain_ref() -> SecretRef {
    SecretRef {
        provider: SecretProvider::MacOsKeychain,
        key: Uuid::new_v4().to_string(),
    }
}

/// A connection carrying every kind of secret slot.
fn secret_laden_profile(name: &str, password: SecretRef) -> ConnectionProfile {
    let mut profile = ConnectionProfile::sql_login(name, "db.example.com", "app_user", password);
    profile.transport = TransportChain {
        proxy: Some(ProxyHop {
            kind: ProxyKind::Socks5,
            host: "proxy.example.com".into(),
            port: 1080,
            username: Some("proxy_user".into()),
            password: Some(SecretRef {
                provider: SecretProvider::InMemory,
                key: PROXY_PASSWORD.into(),
            }),
        }),
        ssh: Some(SshTunnelConfig {
            target: SshHop {
                host: "bastion.example.com".into(),
                port: 22,
                username: "ops".into(),
                auth: SshAuth::KeyFile {
                    path: "~/.ssh/id_ed25519".into(),
                    passphrase: Some(SecretRef {
                        provider: SecretProvider::InMemory,
                        key: PASSPHRASE.into(),
                    }),
                },
            },
            jump: None,
            keepalive: Default::default(),
        }),
        kube: None,
        ssm: None,
    };
    profile
}

fn read_cloud_text(cloud: &Path) -> String {
    let mut out = String::new();
    for entry in std::fs::read_dir(cloud).unwrap() {
        out.push_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    out
}

// ---------------------------------------------------------------------------
// The setting
// ---------------------------------------------------------------------------

#[test]
fn sync_is_off_by_default() {
    assert!(!ICloudSyncPreferences::default().enabled);
    let dir = tempfile::tempdir().unwrap();
    assert!(!ICloudSyncPreferences::load_from(dir.path()).enabled);
    // An unreadable settings file also means off, never on.
    std::fs::write(dir.path().join(super::PREFERENCES_FILE), b"{not json").unwrap();
    assert!(!ICloudSyncPreferences::load_from(dir.path()).enabled);
    ICloudSyncPreferences { enabled: true }
        .save_to(dir.path())
        .unwrap();
    assert!(ICloudSyncPreferences::load_from(dir.path()).enabled);
}

#[test]
fn the_switch_and_the_sync_record_never_sync() {
    for file in super::local::SYNCED_SETTINGS {
        assert_ne!(file.file_name, super::PREFERENCES_FILE);
        assert_ne!(file.file_name, STATE_FILE);
        assert!(!file.file_name.contains("connection"));
        for blocked in [
            "cloud-ai.json",
            "ai-profiles.json",
            "mcp-servers.json",
            "cli-agent.json",
            "external-editor.json",
            "native-tools.json",
        ] {
            assert_ne!(file.file_name, blocked);
        }
    }
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

#[test]
fn a_synced_connection_carries_no_secret_of_any_kind() {
    let keychain = keychain_ref();
    let profile = secret_laden_profile("prod", keychain.clone());
    let payload = connection_payload(&profile).unwrap();
    assert!(secrets::is_secret_free(&payload));
    let text = payload.to_string();
    for secret in [PASSPHRASE, PROXY_PASSWORD, keychain.key.as_str()] {
        assert!(!text.contains(secret), "{secret} leaked into {text}");
    }
    assert!(!text.contains("MacOsKeychain"));
    // The rest of the connection is intact.
    assert!(text.contains("db.example.com"));
    assert!(text.contains("bastion.example.com"));
    assert!(text.contains("app_user"));
    // Health notes are this Mac's own.
    assert!(payload.get("status").is_none());
}

#[test]
fn an_in_memory_password_never_reaches_icloud() {
    let cloud = tempfile::tempdir().unwrap();
    let mac = Mac::new();
    mac.store.add(secret_laden_profile(
        "inline",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: PASSWORD.into(),
        },
    ));
    mac.sync(cloud.path(), 1_000);
    let text = read_cloud_text(cloud.path());
    assert!(text.contains("inline"));
    for secret in [PASSWORD, PASSPHRASE, PROXY_PASSWORD] {
        assert!(!text.contains(secret), "{secret} reached iCloud");
    }
}

#[test]
fn restoring_keeps_this_macs_keychain_references_only_where_it_has_them() {
    let local = json!({
        "auth": { "provider": "MacOsKeychain", "key": "local-entry" },
        "list": [{ "provider": "MacOsKeychain", "key": "hop-entry" }],
    });
    let mut incoming = json!({
        "auth": secrets::placeholder(),
        "list": [secrets::placeholder(), secrets::placeholder()],
        "new": secrets::placeholder(),
    });
    secrets::restore_local(&mut incoming, &local);
    assert_eq!(incoming["auth"]["key"], "local-entry");
    assert_eq!(incoming["list"][0]["key"], "hop-entry");
    assert_eq!(incoming["list"][1], secrets::placeholder());
    assert_eq!(incoming["new"], secrets::placeholder());
}

// ---------------------------------------------------------------------------
// Merge rules
// ---------------------------------------------------------------------------

#[test]
fn the_newest_change_wins_and_ties_resolve_the_same_way_everywhere() {
    let older = SyncItem::live(10, json!({ "v": "old" }));
    let newer = SyncItem::live(20, json!({ "v": "new" }));
    assert!(wins(&newer, &older));
    assert!(!wins(&older, &newer));

    let delete = SyncItem::tombstone(20);
    assert!(
        wins(&delete, &newer),
        "a delete beats an edit made at the same moment"
    );

    let a = SyncItem::live(30, json!({ "v": "a" }));
    let b = SyncItem::live(30, json!({ "v": "b" }));
    assert_ne!(wins(&a, &b), wins(&b, &a));

    // Every Mac reaches the same result whatever order the files are read in.
    let doc = |device: &str, item: &SyncItem| {
        SyncDocument::new(device, 0, BTreeMap::from([("x".to_string(), item.clone())]))
    };
    let forward = merge(&BTreeMap::new(), &[doc("1", &a), doc("2", &b)]);
    let backward = merge(&BTreeMap::new(), &[doc("2", &b), doc("1", &a)]);
    assert_eq!(forward, backward);
}

#[test]
fn a_local_edit_is_stamped_now_and_an_untouched_item_adds_nothing() {
    let mut snapshot = LocalSnapshot::default();
    snapshot.covered.insert("snippet:".into());
    let same = json!({ "prefix": "sel", "body": "SELECT 1" });
    let edited = json!({ "prefix": "upd", "body": "UPDATE t SET x = 2" });
    snapshot
        .items
        .insert("snippet:sel".into(), LocalItem::Content(same.clone()));
    snapshot
        .items
        .insert("snippet:upd".into(), LocalItem::Content(edited));
    let entry = |hash: String| StateEntry {
        local_hash: hash.clone(),
        synced_hash: hash,
        modified_at: 5,
        deleted: false,
    };
    let state = SyncState {
        device_id: Uuid::new_v4().to_string(),
        items: BTreeMap::from([
            ("snippet:sel".to_string(), entry(payload_hash(&same))),
            (
                "snippet:upd".to_string(),
                entry(payload_hash(&json!({ "prefix": "upd", "body": "old" }))),
            ),
            (
                "snippet:gone".to_string(),
                entry(payload_hash(&json!({ "prefix": "gone" }))),
            ),
        ]),
    };
    let remote: BTreeSet<String> = ["snippet:sel", "snippet:upd", "snippet:gone"]
        .into_iter()
        .map(String::from)
        .collect();
    let local = local_entries(&snapshot, &state, &remote, 99);
    assert!(!local.contains_key("snippet:sel"));
    assert_eq!(local["snippet:upd"].modified_at, 99);
    assert!(local["snippet:gone"].deleted);
    assert_eq!(local["snippet:gone"].modified_at, 99);
}

#[test]
fn a_domain_that_could_not_be_read_is_neither_deleted_nor_changed() {
    let snapshot = LocalSnapshot::default(); // nothing covered
    let state = SyncState {
        device_id: Uuid::new_v4().to_string(),
        items: BTreeMap::from([(
            "connection:1".to_string(),
            StateEntry {
                local_hash: "h".into(),
                synced_hash: "h".into(),
                modified_at: 1,
                deleted: false,
            },
        )]),
    };
    assert!(local_entries(&snapshot, &state, &BTreeSet::new(), 50).is_empty());
    let merged = BTreeMap::from([("connection:1".to_string(), SyncItem::tombstone(60))]);
    assert!(plan_local_changes(&snapshot, &state, &merged).is_empty());
}

// ---------------------------------------------------------------------------
// Two Macs
// ---------------------------------------------------------------------------

#[test]
fn a_connection_reaches_the_other_mac_without_its_password_and_edits_flow_back() {
    let cloud = tempfile::tempdir().unwrap();
    let mac_a = Mac::new();
    let mac_b = Mac::new();
    let keychain = keychain_ref();
    let mut profile = secret_laden_profile("warehouse", keychain.clone());
    profile.notes = Some("Owned by the data team; read replica after 6pm.".into());
    let id = profile.id.0;
    mac_a.store.add(profile);

    mac_a.sync(cloud.path(), 1_000);
    let report = mac_b.sync(cloud.path(), 2_000);
    assert!(report.apply.connections_changed);

    let arrived = mac_b.store.get(id).expect("connection arrived on Mac B");
    assert_eq!(arrived.name, "warehouse");
    assert_eq!(
        arrived.notes.as_deref(),
        Some("Owned by the data team; read replica after 6pm.")
    );
    let secret = arrived.authentication.secret_ref().unwrap();
    // An empty in-memory secret: Mac B asks for the password on connect.
    assert_eq!(secret.provider, SecretProvider::InMemory);
    assert_eq!(secret.key, "");
    let ssh = arrived.transport.ssh.as_ref().unwrap();
    assert!(matches!(
        &ssh.target.auth,
        SshAuth::KeyFile { passphrase: Some(p), .. } if p.key.is_empty()
    ));

    // Mac B renames it; Mac A takes the rename but keeps its own keychain
    // reference, so its saved password still works.
    let mut renamed = arrived.clone();
    renamed.name = "warehouse (reporting)".into();
    mac_b.store.update(id, renamed);
    mac_b.sync(cloud.path(), 3_000);
    mac_a.sync(cloud.path(), 4_000);
    let back = mac_a.store.get(id).unwrap();
    assert_eq!(back.name, "warehouse (reporting)");
    assert_eq!(back.authentication.secret_ref().unwrap(), &keychain);
    let ssh = back.transport.ssh.as_ref().unwrap();
    assert!(matches!(
        &ssh.target.auth,
        SshAuth::KeyFile { passphrase: Some(p), .. } if p.key == PASSPHRASE
    ));

    // Settled: another round changes nothing on either Mac.
    assert_eq!(mac_a.sync(cloud.path(), 5_000).apply.applied, 0);
    assert_eq!(mac_b.sync(cloud.path(), 6_000).apply.applied, 0);
}

#[test]
fn a_delete_reaches_every_mac_and_a_stale_copy_never_brings_it_back() {
    let cloud = tempfile::tempdir().unwrap();
    let mac_a = Mac::new();
    let mac_b = Mac::new();
    let profile = ConnectionProfile::sql_login("old", "h", "u", keychain_ref());
    let id = profile.id.0;
    mac_a.store.add(profile);
    mac_a.sync(cloud.path(), 1_000);
    mac_b.sync(cloud.path(), 2_000);
    assert!(mac_b.store.get(id).is_some());

    // A third Mac goes offline holding the old copy in its file.
    let stale = std::fs::read_dir(cloud.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .next()
        .unwrap();
    let stale_doc: SyncDocument = serde_json::from_slice(&std::fs::read(&stale).unwrap()).unwrap();
    let offline_id = Uuid::new_v4().to_string();
    let offline = SyncDocument::new(&offline_id, 2_500, stale_doc.items.clone());
    super::document::write_device_file(cloud.path(), &offline).unwrap();

    mac_a.store.remove(id);
    mac_a.sync(cloud.path(), 3_000);
    mac_b.sync(cloud.path(), 4_000);
    assert!(mac_b.store.get(id).is_none(), "the delete reached Mac B");

    // Another round with the stale file still there: stays deleted.
    mac_a.sync(cloud.path(), 5_000);
    mac_b.sync(cloud.path(), 6_000);
    assert!(mac_a.store.get(id).is_none());
    assert!(mac_b.store.get(id).is_none());
    let doc: SyncDocument = serde_json::from_slice(
        &std::fs::read(cloud.path().join(device_file_name(&load_device_id(&mac_a)))).unwrap(),
    )
    .unwrap();
    assert!(doc.items[&format!("connection:{id}")].deleted);
}

fn load_device_id(mac: &Mac) -> String {
    super::engine::load_state(&mac.settings().join(STATE_FILE))
        .unwrap()
        .device_id
}

#[test]
fn labels_with_colors_and_snippets_sync() {
    let cloud = tempfile::tempdir().unwrap();
    let mac_a = Mac::new();
    let mac_b = Mac::new();
    let client_id = Uuid::new_v4();
    mac_a.store.upsert_client(crate::connection::ClientEntity {
        id: client_id,
        name: "Acme".into(),
        color_hex: Some("#FF8800".into()),
        image_path: None,
        safety: Default::default(),
    });
    let snippets = crate::snippets::SnippetPreferences {
        version: 1,
        snippets: vec![crate::snippets::UserSnippet {
            prefix: "top".into(),
            body: "SELECT TOP ${1:10} * FROM ${2:t}".into(),
            description: "Top rows".into(),
        }],
        quick_actions: Vec::new(),
    };
    std::fs::write(
        mac_a.settings().join(super::local::SNIPPETS_FILE),
        serde_json::to_vec(&snippets).unwrap(),
    )
    .unwrap();

    mac_a.sync(cloud.path(), 1_000);
    let report = mac_b.sync(cloud.path(), 2_000);
    assert!(report.apply.snippets_changed);
    let client = mac_b
        .store
        .clients()
        .into_iter()
        .find(|c| c.id == client_id)
        .unwrap();
    assert_eq!(client.color_hex.as_deref(), Some("#FF8800"));
    let synced: crate::snippets::SnippetPreferences = serde_json::from_slice(
        &std::fs::read(mac_b.settings().join(super::local::SNIPPETS_FILE)).unwrap(),
    )
    .unwrap();
    assert_eq!(synced.snippets, snippets.snippets);
}

#[test]
fn a_mac_joining_later_takes_the_shared_settings() {
    let cloud = tempfile::tempdir().unwrap();
    let mac_a = Mac::new();
    let mac_b = Mac::new();
    let file = "editor-preferences.json";
    std::fs::write(
        mac_a.settings().join(file),
        br#"{ "tabSize": 2, "configVersion": 3 }"#,
    )
    .unwrap();
    std::fs::write(
        mac_b.settings().join(file),
        br#"{ "tabSize": 8, "configVersion": 3 }"#,
    )
    .unwrap();
    mac_a.sync(cloud.path(), 1_000);
    // Mac B turns sync on later, with its own older settings.
    mac_b.sync(cloud.path(), 5_000);
    let b: Value =
        serde_json::from_slice(&std::fs::read(mac_b.settings().join(file)).unwrap()).unwrap();
    assert_eq!(b["tabSize"], 2);
}

#[test]
fn local_only_settings_stay_on_each_mac() {
    let cloud = tempfile::tempdir().unwrap();
    let mac_a = Mac::new();
    let mac_b = Mac::new();
    let file = "appearance-preferences.json";
    std::fs::write(
        mac_a.settings().join(file),
        br#"{ "darkTheme": "nordicDark", "customTranslationFile": "/Users/a/fr.json", "configVersion": 3 }"#,
    )
    .unwrap();
    std::fs::write(
        mac_b.settings().join(file),
        br#"{ "customTranslationFile": "/Users/b/de.json", "configVersion": 3 }"#,
    )
    .unwrap();
    mac_a.sync(cloud.path(), 1_000);
    assert!(!read_cloud_text(cloud.path()).contains("/Users/a/fr.json"));
    mac_b.sync(cloud.path(), 2_000);
    let b: Value =
        serde_json::from_slice(&std::fs::read(mac_b.settings().join(file)).unwrap()).unwrap();
    assert_eq!(b["darkTheme"], "nordicDark");
    assert_eq!(b["customTranslationFile"], "/Users/b/de.json");
}

#[test]
fn an_edited_query_that_loses_is_kept_as_a_conflicted_copy() {
    let cloud = tempfile::tempdir().unwrap();
    let mac_a = Mac::new();
    let mac_b = Mac::new();
    std::fs::write(mac_a.library().join("report.sql"), "SELECT 'from A';").unwrap();
    std::fs::write(mac_b.library().join("report.sql"), "SELECT 'from B';").unwrap();
    mac_a.sync(cloud.path(), 1_000);
    let report = mac_b.sync(cloud.path(), 2_000);
    assert_eq!(
        std::fs::read_to_string(mac_b.library().join("report.sql")).unwrap(),
        "SELECT 'from A';"
    );
    assert_eq!(report.apply.conflicted_copies.len(), 1);
    let copy = mac_b.library().join(&report.apply.conflicted_copies[0]);
    assert_eq!(std::fs::read_to_string(&copy).unwrap(), "SELECT 'from B';");
    // The copy goes out to the other Mac too.
    mac_a.sync(cloud.path(), 3_000);
    assert!(
        mac_a
            .library()
            .join(&report.apply.conflicted_copies[0])
            .exists()
    );
}

#[test]
fn a_query_too_large_to_sync_is_never_deleted_or_overwritten() {
    let cloud = tempfile::tempdir().unwrap();
    let mac = Mac::new();
    let big = "-- x\n".repeat((super::local::MAX_LIBRARY_FILE_BYTES as usize / 5) + 10);
    std::fs::write(mac.library().join("huge.sql"), &big).unwrap();
    mac.sync(cloud.path(), 1_000);
    assert!(!read_cloud_text(cloud.path()).contains("library:huge.sql"));
    let tombstone = SyncDocument::new(
        &Uuid::new_v4().to_string(),
        2_000,
        BTreeMap::from([("library:huge.sql".to_string(), SyncItem::tombstone(2_000))]),
    );
    super::document::write_device_file(cloud.path(), &tombstone).unwrap();
    mac.sync(cloud.path(), 3_000);
    assert_eq!(
        std::fs::read_to_string(mac.library().join("huge.sql")).unwrap(),
        big
    );
}

// ---------------------------------------------------------------------------
// Damaged, partial and newer files in iCloud
// ---------------------------------------------------------------------------

#[test]
fn a_damaged_or_partial_file_is_skipped_and_changes_nothing_here() {
    let cloud = tempfile::tempdir().unwrap();
    let mac = Mac::new();
    let profile = ConnectionProfile::sql_login("keep me", "h", "u", keychain_ref());
    let id = profile.id.0;
    mac.store.add(profile);
    mac.sync(cloud.path(), 1_000);
    let own = cloud.path().join(device_file_name(&load_device_id(&mac)));
    let full = std::fs::read(&own).unwrap();

    // Another Mac's file, cut off half way, plus one that is not a sync file
    // at all, plus a well-formed one claiming to delete everything but
    // whose item is malformed.
    let partial_name = device_file_name(&Uuid::new_v4().to_string());
    std::fs::write(cloud.path().join(&partial_name), &full[..full.len() / 2]).unwrap();
    let foreign_name = device_file_name(&Uuid::new_v4().to_string());
    std::fs::write(cloud.path().join(&foreign_name), br#"{"hello": "world"}"#).unwrap();
    let malformed_name = device_file_name(&Uuid::new_v4().to_string());
    std::fs::write(
        cloud.path().join(&malformed_name),
        json!({
            "format": SYNC_FORMAT,
            "version": 1,
            "deviceId": "x",
            "writtenAt": 9_999,
            "items": { format!("connection:{id}"): { "modifiedAt": 9_999, "deleted": true, "payload": {} } },
        })
        .to_string(),
    )
    .unwrap();

    let report = mac.sync(cloud.path(), 2_000);
    assert_eq!(report.skipped.len(), 3);
    assert_eq!(report.apply.applied, 0);
    assert!(mac.store.get(id).is_some(), "local data untouched");
    // The skipped files are left exactly as they were.
    assert_eq!(
        std::fs::read(cloud.path().join(&partial_name)).unwrap(),
        &full[..full.len() / 2]
    );
    assert_eq!(
        std::fs::read(cloud.path().join(&foreign_name)).unwrap(),
        br#"{"hello": "world"}"#
    );
}

#[test]
fn a_file_from_a_newer_version_is_skipped() {
    let text = json!({
        "format": SYNC_FORMAT,
        "version": super::document::SYNC_VERSION + 1,
        "deviceId": "x",
        "writtenAt": 0,
        "items": {},
        "somethingNew": true,
    })
    .to_string();
    assert!(matches!(
        parse_document("x.json", text.as_bytes()),
        Err(super::document::SkippedFile::Newer { .. })
    ));
}

#[test]
fn files_icloud_has_not_downloaded_are_waited_for_not_treated_as_missing() {
    let cloud = tempfile::tempdir().unwrap();
    let id = Uuid::new_v4();
    std::fs::write(cloud.path().join(format!(".{id}.json.icloud")), b"").unwrap();
    std::fs::write(cloud.path().join(format!("{id} 2.json")), b"conflict copy").unwrap();
    let remote = read_devices_folder(cloud.path()).unwrap();
    assert_eq!(remote.pending.len(), 1);
    assert!(remote.documents.is_empty());
    assert!(remote.skipped.is_empty());
}

#[test]
fn an_unreadable_sync_record_stops_the_pass_before_anything_changes() {
    let cloud = tempfile::tempdir().unwrap();
    let mac = Mac::new();
    std::fs::write(mac.settings().join(STATE_FILE), b"{ broken").unwrap();
    let other = SyncDocument::new(
        &Uuid::new_v4().to_string(),
        1,
        BTreeMap::from([(
            "preference:editor-preferences.json".to_string(),
            SyncItem::live(1, json!({ "tabSize": 3 })),
        )]),
    );
    super::document::write_device_file(cloud.path(), &other).unwrap();
    assert!(matches!(
        mac.try_sync(cloud.path(), 2),
        Err(PassError::StateUnreadable { .. })
    ));
    assert!(!mac.settings().join("editor-preferences.json").exists());
}

#[test]
fn an_unreadable_snippets_file_is_not_mistaken_for_deleted_snippets() {
    let cloud = tempfile::tempdir().unwrap();
    let mac_a = Mac::new();
    let mac_b = Mac::new();
    let path = mac_a.settings().join(super::local::SNIPPETS_FILE);
    std::fs::write(
        &path,
        br#"{ "snippets": [{ "prefix": "sel", "body": "SELECT 1" }] }"#,
    )
    .unwrap();
    mac_a.sync(cloud.path(), 1_000);
    std::fs::write(&path, b"{ half a fi").unwrap();
    mac_a.sync(cloud.path(), 2_000);
    mac_b.sync(cloud.path(), 3_000);
    let synced: crate::snippets::SnippetPreferences = serde_json::from_slice(
        &std::fs::read(mac_b.settings().join(super::local::SNIPPETS_FILE)).unwrap(),
    )
    .unwrap();
    assert_eq!(synced.snippets.len(), 1, "no tombstone was sent");
    assert_eq!(std::fs::read(&path).unwrap(), b"{ half a fi");
}

// ---------------------------------------------------------------------------
// iCloud availability
// ---------------------------------------------------------------------------

#[test]
fn icloud_signed_out_and_drive_off_are_told_apart() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(detect_in_home(home.path()), Availability::NotSignedIn);
    std::fs::create_dir_all(home.path().join(super::location::MOBILE_DOCUMENTS)).unwrap();
    assert_eq!(detect_in_home(home.path()), Availability::DriveOff);
    let drive = home
        .path()
        .join(super::location::MOBILE_DOCUMENTS)
        .join(super::location::ICLOUD_DRIVE_FOLDER);
    std::fs::create_dir_all(&drive).unwrap();
    assert_eq!(
        detect_in_home(home.path()),
        Availability::Available {
            devices: drive
                .join(super::document::SYNC_FOLDER)
                .join(super::document::DEVICES_FOLDER),
        }
    );
}

#[test]
fn sync_is_offered_only_on_macos_outside_the_app_store_edition() {
    assert_eq!(
        super::is_offered(),
        cfg!(target_os = "macos") && !crate::distribution::is_app_store()
    );
    if !cfg!(target_os = "macos") {
        assert_eq!(super::location::detect(), Availability::Unsupported);
    }
}

#[test]
fn status_messages_render_for_every_outcome() {
    let at = chrono::Local::now();
    let ok = super::status_for(&Ok(super::engine::PassReport::default()), at);
    assert!(!ok.is_problem);
    assert!(crate::localization::message(ok.summary.as_ref().unwrap()).starts_with("Last synced"));
    let err = super::status_for(
        &Err(PassError::FolderUnreadable {
            path: PathBuf::from("/x"),
            detail: "denied".into(),
        }),
        at,
    );
    assert!(err.is_problem);
    let text = crate::localization::message(err.summary.as_ref().unwrap());
    assert!(text.contains("/x") && text.contains("denied"), "{text}");
}
