//! Schema snapshots: point-in-time captures of one database's object
//! definitions, saved per (connection, database) so drift can be diffed
//! later — snapshot vs snapshot, or snapshot vs the live schema — without a
//! second server.
//!
//! The objects are exactly what Schema Compare collects (`SchemaObject`
//! rows from its per-engine metadata batch), so a snapshot feeds the same
//! diff, risk, and deploy pipeline as a live side. Storage is a small
//! versioned JSON index plus one versioned JSON payload per snapshot under
//! the app data directory; the store is rooted at an injectable path so
//! tests never touch the real one.

use sqlly_core::{ConnectionProfile, DatabaseType, DisplayMessage};
use sqlly_services::SchemaObject;

/// On-disk format version for both the index and the payload files.
pub const SCHEMA_SNAPSHOT_FORMAT_VERSION: u32 = 1;

const INDEX_FILE: &str = "schema-snapshots.json";
const PAYLOAD_EXT: &str = ".schema.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SchemaSnapshotMeta {
    pub id: String,
    pub taken_unix_ms: i64,
    #[serde(default)]
    pub label: Option<String>,
    pub engine: DatabaseType,
    pub connection_id: uuid::Uuid,
    pub connection_name: String,
    pub database: String,
    pub object_count: usize,
    /// File name (not path) of the payload inside the store directory.
    pub file: String,
}

impl SchemaSnapshotMeta {
    /// Whether this snapshot belongs to the given connection and database.
    pub fn is_for(&self, connection_id: uuid::Uuid, database: &str) -> bool {
        self.connection_id == connection_id && self.database == database
    }
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct SnapshotIndex {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    snapshots: Vec<SchemaSnapshotMeta>,
}

/// Serialized twin of `SchemaObject` (which carries no serde derives of its
/// own); kept private so the wire shape is owned here.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredObject {
    schema: String,
    name: String,
    kind: String,
    definition: String,
}

impl From<&SchemaObject> for StoredObject {
    fn from(object: &SchemaObject) -> Self {
        Self {
            schema: object.schema.clone(),
            name: object.name.clone(),
            kind: object.kind.clone(),
            definition: object.definition.clone(),
        }
    }
}

impl From<StoredObject> for SchemaObject {
    fn from(object: StoredObject) -> Self {
        Self {
            schema: object.schema,
            name: object.name,
            kind: object.kind,
            definition: object.definition,
        }
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SnapshotPayload {
    version: u32,
    objects: Vec<StoredObject>,
}

#[cfg(not(target_arch = "wasm32"))]
/// Filesystem-backed schema snapshot store rooted at a directory.
pub struct SchemaSnapshotStore {
    dir: std::path::PathBuf,
    mutation: std::sync::Mutex<()>,
}

#[cfg(not(target_arch = "wasm32"))]
impl SchemaSnapshotStore {
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self {
            dir,
            mutation: std::sync::Mutex::new(()),
        }
    }

    /// The app's default store under the data directory.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn default_store() -> Self {
        let mut dir = crate::preferences::preferences_path();
        dir.push("SchemaSnapshots");
        Self::new(dir)
    }

    fn index_path(&self) -> std::path::PathBuf {
        self.dir.join(INDEX_FILE)
    }

    /// Resolve a payload basename under `dir`, rejecting path escape.
    fn payload_path(&self, file: &str) -> Result<std::path::PathBuf, DisplayMessage> {
        let invalid = || {
            crate::localization::deferred_text("explorer.schema_history.errors.invalid_file_name")
        };
        if file.is_empty() || file.contains('/') || file.contains('\\') || file.contains("..") {
            return Err(invalid());
        }
        if std::path::Path::new(file).components().count() != 1 {
            return Err(invalid());
        }
        let Some(stem) = file.strip_suffix(PAYLOAD_EXT) else {
            return Err(invalid());
        };
        if uuid::Uuid::parse_str(stem).is_err() {
            return Err(invalid());
        }
        Ok(self.dir.join(file))
    }

    /// Load the index without repair. A missing file is an empty catalog; a
    /// corrupt or unreadable one is an error so mutations cannot clobber it.
    fn try_list(&self) -> Result<Vec<SchemaSnapshotMeta>, DisplayMessage> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&path).map_err(raw_error)?;
        let index = serde_json::from_str::<SnapshotIndex>(&text).map_err(raw_error)?;
        if index.version > SCHEMA_SNAPSHOT_FORMAT_VERSION {
            return Err(crate::localization::deferred(
                "explorer.schema_history.errors.index_version_too_new",
                &[
                    ("version", index.version),
                    ("supported_version", SCHEMA_SNAPSHOT_FORMAT_VERSION),
                ],
            ));
        }
        Ok(index.snapshots)
    }

    fn write_index(&self, snapshots: Vec<SchemaSnapshotMeta>) -> Result<(), DisplayMessage> {
        std::fs::create_dir_all(&self.dir).map_err(raw_error)?;
        let index = SnapshotIndex {
            version: SCHEMA_SNAPSHOT_FORMAT_VERSION,
            snapshots,
        };
        let text = serde_json::to_string_pretty(&index).map_err(raw_error)?;
        crate::fs_safe::write_atomic(&self.index_path(), text.as_bytes()).map_err(raw_error)
    }

    /// Every snapshot, newest first. Index entries whose payload is missing
    /// are dropped (and the pruned index persisted); a corrupt index yields
    /// an empty list without rewriting the file.
    pub fn list_all(&self) -> Vec<SchemaSnapshotMeta> {
        let Ok(_guard) = self.mutation.lock() else {
            return Vec::new();
        };
        let Ok(mut snapshots) = self.try_list() else {
            return Vec::new();
        };
        let before = snapshots.len();
        snapshots.retain(|meta| {
            self.payload_path(&meta.file)
                .ok()
                .is_some_and(|path| path.exists())
        });
        if snapshots.len() != before {
            let _ = self.write_index(snapshots.clone());
        }
        sort_newest_first(&mut snapshots);
        snapshots
    }

    /// Snapshots for one connection and database, newest first.
    pub fn list_for(&self, connection_id: uuid::Uuid, database: &str) -> Vec<SchemaSnapshotMeta> {
        self.list_all()
            .into_iter()
            .filter(|meta| meta.is_for(connection_id, database))
            .collect()
    }

    /// Persist a new snapshot: the objects to their own payload file, the
    /// metadata to the index.
    pub fn save_new(
        &self,
        profile: &ConnectionProfile,
        database: &str,
        label: Option<&str>,
        taken_unix_ms: i64,
        objects: &[SchemaObject],
    ) -> Result<SchemaSnapshotMeta, DisplayMessage> {
        let database = database.trim();
        if database.is_empty() {
            return Err(crate::localization::deferred_text(
                "explorer.schema_history.errors.database_required",
            ));
        }
        let guard = self.mutation.lock().map_err(|_| store_busy())?;
        // Refuse before writing anything so a corrupt index is never clobbered
        // and no payload is left unindexed.
        let mut snapshots = self.try_list()?;
        std::fs::create_dir_all(&self.dir).map_err(raw_error)?;
        let id = uuid::Uuid::new_v4().to_string();
        let file = format!("{id}{PAYLOAD_EXT}");
        let payload = SnapshotPayload {
            version: SCHEMA_SNAPSHOT_FORMAT_VERSION,
            objects: objects.iter().map(StoredObject::from).collect(),
        };
        let text = serde_json::to_string(&payload).map_err(raw_error)?;
        crate::fs_safe::write_atomic(&self.dir.join(&file), text.as_bytes()).map_err(raw_error)?;
        let meta = SchemaSnapshotMeta {
            id,
            taken_unix_ms,
            label: normalize_label(label),
            engine: profile.database_type,
            connection_id: profile.id.0,
            connection_name: profile.name.clone(),
            database: database.to_string(),
            object_count: objects.len(),
            file: file.clone(),
        };
        snapshots.insert(0, meta.clone());
        if let Err(error) = self.write_index(snapshots) {
            let _ = std::fs::remove_file(self.dir.join(&file));
            return Err(error);
        }
        drop(guard);
        Ok(meta)
    }

    /// Change a snapshot's label; a blank label clears it (labels are
    /// optional, the timestamp always identifies the snapshot).
    pub fn rename(&self, id: &str, label: Option<&str>) -> Result<(), DisplayMessage> {
        let _guard = self.mutation.lock().map_err(|_| store_busy())?;
        let mut snapshots = self.try_list()?;
        let Some(meta) = snapshots.iter_mut().find(|meta| meta.id == id) else {
            return Err(not_found());
        };
        meta.label = normalize_label(label);
        self.write_index(snapshots)
    }

    pub fn delete(&self, id: &str) -> Result<(), DisplayMessage> {
        let _guard = self.mutation.lock().map_err(|_| store_busy())?;
        let mut snapshots = self.try_list()?;
        let Some(position) = snapshots.iter().position(|meta| meta.id == id) else {
            return Err(not_found());
        };
        let meta = snapshots.remove(position);
        if let Ok(payload) = self.payload_path(&meta.file) {
            let _ = std::fs::remove_file(payload);
        }
        self.write_index(snapshots)
    }

    /// Read a snapshot's objects back in the shape Schema Compare consumes.
    pub fn read(&self, id: &str) -> Result<Vec<SchemaObject>, DisplayMessage> {
        let meta = self
            .try_list()?
            .into_iter()
            .find(|meta| meta.id == id)
            .ok_or_else(not_found)?;
        let payload = self.payload_path(&meta.file)?;
        let text = std::fs::read_to_string(payload).map_err(raw_error)?;
        let payload = serde_json::from_str::<SnapshotPayload>(&text).map_err(raw_error)?;
        if payload.version > SCHEMA_SNAPSHOT_FORMAT_VERSION {
            return Err(crate::localization::deferred(
                "explorer.schema_history.errors.snapshot_version_too_new",
                &[
                    ("version", payload.version),
                    ("supported_version", SCHEMA_SNAPSHOT_FORMAT_VERSION),
                ],
            ));
        }
        Ok(payload
            .objects
            .into_iter()
            .map(SchemaObject::from)
            .collect())
    }
}

/// File-system and JSON failures carry their own (untranslated) detail.
fn raw_error(error: impl std::fmt::Display) -> DisplayMessage {
    DisplayMessage::raw(error.to_string())
}

fn store_busy() -> DisplayMessage {
    crate::localization::deferred_text("explorer.schema_history.errors.store_busy")
}

fn not_found() -> DisplayMessage {
    crate::localization::deferred_text("explorer.schema_history.errors.snapshot_not_found")
}

fn normalize_label(label: Option<&str>) -> Option<String> {
    label
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .map(|label| label.chars().take(120).collect())
}

/// Newest first; ties keep their existing (insertion) order.
pub fn sort_newest_first(snapshots: &mut [SchemaSnapshotMeta]) {
    snapshots.sort_by_key(|meta| std::cmp::Reverse(meta.taken_unix_ms));
}

// ── Labels and time formatting ─────────────────────────────────────────────

/// "2026-09-26 14:03" in the given zone. `format_taken_at` uses local time;
/// this variant exists so formatting is testable with a fixed zone.
pub fn format_taken_at_in<Tz: chrono::TimeZone>(taken_unix_ms: i64, zone: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(taken_unix_ms)
        .map(|utc| utc.with_timezone(zone).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| crate::localization::text("explorer.schema_history.unknown_time"))
}

/// Wall-clock "taken at" in the user's local zone.
pub fn format_taken_at(taken_unix_ms: i64) -> String {
    format_taken_at_in(taken_unix_ms, &chrono::Local)
}

/// "<label> @ <time>" or "Snapshot @ <time>" — the identity shown wherever a
/// live side would show its connection name.
pub fn snapshot_title(meta: &SchemaSnapshotMeta) -> String {
    snapshot_title_with(meta, format_taken_at(meta.taken_unix_ms))
}

fn snapshot_title_with(meta: &SchemaSnapshotMeta, when: String) -> String {
    match meta.label.as_deref() {
        Some(label) => crate::localization::format(
            "explorer.schema_history.snapshot_title_labeled",
            &[("label", label), ("time", when.as_str())],
        ),
        None => crate::localization::format(
            "explorer.schema_history.snapshot_title_unlabeled",
            &[("time", when)],
        ),
    }
}

/// The Schema Compare picker entry for a snapshot.
pub fn picker_label(meta: &SchemaSnapshotMeta) -> String {
    crate::localization::format(
        "explorer.schema_compare.snapshot_picker_entry",
        &[
            ("title", snapshot_title(meta)),
            ("database", meta.database.clone()),
        ],
    )
}

// ── Comparison sources and timeline picks ──────────────────────────────────

/// One side of a schema comparison: a live database or a saved snapshot.
#[derive(Clone, Debug)]
pub enum CompareSource {
    Live {
        profile: Box<ConnectionProfile>,
        database: String,
    },
    Snapshot(SchemaSnapshotMeta),
}

impl CompareSource {
    pub fn database_type(&self) -> DatabaseType {
        match self {
            Self::Live { profile, .. } => profile.database_type,
            Self::Snapshot(meta) => meta.engine,
        }
    }

    /// What the compare header and scripts call this side.
    pub fn display_name(&self) -> String {
        match self {
            Self::Live { profile, .. } => profile.name.clone(),
            Self::Snapshot(meta) => snapshot_title(meta),
        }
    }
}

/// A point on the Schema History timeline: a snapshot (by index into the
/// chronological list) or the live database at the far end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimelinePoint {
    Snapshot(usize),
    Live,
}

#[cfg(not(target_arch = "wasm32"))]
/// Toggle a point in the pick set. At most two points stay picked: picking a
/// third replaces the oldest pick, so the pair always reflects the last two
/// clicks.
pub fn toggle_pick(picks: &mut Vec<TimelinePoint>, point: TimelinePoint) {
    if let Some(position) = picks.iter().position(|pick| *pick == point) {
        picks.remove(position);
        return;
    }
    if picks.len() >= 2 {
        picks.remove(0);
    }
    picks.push(point);
}

#[cfg(not(target_arch = "wasm32"))]
/// Resolve two picks into (source, target): the older point is the source
/// and the newer one (Live is newest) the target, so "missing in target"
/// reads as "removed since" and "extra in target" as "added since".
pub fn compare_pair(
    picks: &[TimelinePoint],
    chronological: &[SchemaSnapshotMeta],
    live: &CompareSource,
) -> Option<(CompareSource, CompareSource)> {
    if picks.len() != 2 {
        return None;
    }
    let rank = |point: &TimelinePoint| match point {
        TimelinePoint::Snapshot(index) => chronological
            .get(*index)
            .map(|meta| (0, meta.taken_unix_ms, *index)),
        TimelinePoint::Live => Some((1, i64::MAX, usize::MAX)),
    };
    let mut ordered = [picks[0], picks[1]];
    let (first, second) = (rank(&ordered[0])?, rank(&ordered[1])?);
    if second < first {
        ordered.swap(0, 1);
    }
    let resolve = |point: TimelinePoint| match point {
        TimelinePoint::Snapshot(index) => chronological
            .get(index)
            .cloned()
            .map(CompareSource::Snapshot),
        TimelinePoint::Live => Some(live.clone()),
    };
    Some((resolve(ordered[0])?, resolve(ordered[1])?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::ConnectionId;

    fn temp_store(tag: &str) -> SchemaSnapshotStore {
        let dir = std::env::temp_dir().join(format!(
            "sqlly-schema-snapshots-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        SchemaSnapshotStore::new(dir)
    }

    fn profile(name: &str) -> ConnectionProfile {
        use sqlly_core::{SecretProvider, SecretRef};
        let mut profile = ConnectionProfile::sql_login(
            name,
            "host",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        profile.id = ConnectionId(uuid::Uuid::new_v4());
        profile.database_type = DatabaseType::Postgres;
        profile
    }

    fn obj(schema: &str, name: &str, kind: &str, definition: &str) -> SchemaObject {
        SchemaObject {
            schema: schema.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            definition: definition.to_string(),
        }
    }

    fn meta(id: &str, taken: i64, label: Option<&str>) -> SchemaSnapshotMeta {
        SchemaSnapshotMeta {
            id: id.to_string(),
            taken_unix_ms: taken,
            label: label.map(str::to_string),
            engine: DatabaseType::Postgres,
            connection_id: uuid::Uuid::nil(),
            connection_name: "pg".into(),
            database: "app".into(),
            object_count: 0,
            file: String::new(),
        }
    }

    #[test]
    fn save_list_read_rename_delete_round_trip() {
        let store = temp_store("round-trip");
        let profile = profile("Local PG");
        let objects = vec![
            obj("public", "users", "TABLE", "id integer NOT NULL"),
            obj(
                "public",
                "v_users",
                "VIEW",
                "CREATE VIEW v_users AS SELECT 1",
            ),
        ];
        let meta = store
            .save_new(&profile, "app", Some("  before release  "), 1_000, &objects)
            .unwrap();
        assert_eq!(meta.label.as_deref(), Some("before release"));
        assert_eq!(meta.object_count, 2);
        assert_eq!(meta.engine, DatabaseType::Postgres);
        assert_eq!(meta.connection_id, profile.id.0);

        let listed = store.list_for(profile.id.0, "app");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], meta);
        assert!(store.list_for(profile.id.0, "other").is_empty());
        assert!(store.list_for(uuid::Uuid::new_v4(), "app").is_empty());

        let read_back = store.read(&meta.id).unwrap();
        assert_eq!(read_back, objects);

        store.rename(&meta.id, Some("after review")).unwrap();
        assert_eq!(store.list_all()[0].label.as_deref(), Some("after review"));
        store.rename(&meta.id, Some("   ")).unwrap();
        assert_eq!(store.list_all()[0].label, None, "blank label clears it");
        assert!(store.rename("missing", Some("x")).is_err());

        store.delete(&meta.id).unwrap();
        assert!(store.list_all().is_empty());
        assert!(store.read(&meta.id).is_err());
        assert!(
            !store.dir.join(&meta.file).exists(),
            "payload removed with the entry"
        );
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn index_and_payload_are_versioned_json() {
        let store = temp_store("versioned");
        let profile = profile("Local PG");
        let meta = store
            .save_new(&profile, "app", None, 5, &[obj("", "t", "TABLE", "id int")])
            .unwrap();
        let index: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(store.index_path()).unwrap()).unwrap();
        assert_eq!(index["version"], SCHEMA_SNAPSHOT_FORMAT_VERSION);
        assert_eq!(index["snapshots"][0]["id"], meta.id);
        assert_eq!(index["snapshots"][0]["database"], "app");
        let payload: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(store.dir.join(&meta.file)).unwrap())
                .unwrap();
        assert_eq!(payload["version"], SCHEMA_SNAPSHOT_FORMAT_VERSION);
        assert_eq!(payload["objects"][0]["name"], "t");
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn newer_format_versions_are_refused_not_misread() {
        let store = temp_store("newer");
        std::fs::create_dir_all(&store.dir).unwrap();
        std::fs::write(
            store.index_path(),
            format!(
                r#"{{"version":{},"snapshots":[]}}"#,
                SCHEMA_SNAPSHOT_FORMAT_VERSION + 1
            ),
        )
        .unwrap();
        assert!(store.try_list().is_err());
        assert!(store.list_all().is_empty());
        assert!(
            store.save_new(&profile("x"), "app", None, 1, &[]).is_err(),
            "a newer index is never overwritten"
        );
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn listing_is_newest_first_regardless_of_save_order() {
        let store = temp_store("ordering");
        let profile = profile("Local PG");
        store
            .save_new(&profile, "app", Some("middle"), 200, &[])
            .unwrap();
        store
            .save_new(&profile, "app", Some("oldest"), 100, &[])
            .unwrap();
        store
            .save_new(&profile, "app", Some("newest"), 300, &[])
            .unwrap();
        let labels: Vec<String> = store
            .list_all()
            .into_iter()
            .filter_map(|meta| meta.label)
            .collect();
        assert_eq!(labels, vec!["newest", "middle", "oldest"]);
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn corrupt_index_is_never_clobbered_and_missing_payloads_are_pruned() {
        let store = temp_store("corrupt");
        std::fs::create_dir_all(&store.dir).unwrap();
        let corrupt = r#"{"version":1,"snapshots":[{"id":"old""#;
        std::fs::write(store.index_path(), corrupt).unwrap();
        assert!(store.save_new(&profile("x"), "app", None, 1, &[]).is_err());
        assert_eq!(
            std::fs::read_to_string(store.index_path()).unwrap(),
            corrupt
        );

        let store = temp_store("pruned");
        let profile = profile("Local PG");
        let kept = store.save_new(&profile, "app", None, 1, &[]).unwrap();
        let broken = store.save_new(&profile, "app", None, 2, &[]).unwrap();
        std::fs::remove_file(store.dir.join(&broken.file)).unwrap();
        let listed = store.list_all();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, kept.id);
        assert_eq!(store.try_list().unwrap().len(), 1, "pruned index persisted");
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn payload_path_rejects_escape() {
        let store = temp_store("escape");
        assert!(store.payload_path("../../etc/passwd").is_err());
        assert!(store.payload_path("a/b.schema.json").is_err());
        assert!(store.payload_path("not-a-uuid.schema.json").is_err());
        let id = uuid::Uuid::new_v4();
        let name = format!("{id}{PAYLOAD_EXT}");
        assert_eq!(store.payload_path(&name).unwrap(), store.dir.join(&name));
    }

    #[test]
    fn two_snapshots_diff_through_the_existing_engine() {
        let store = temp_store("diff");
        let profile = profile("Local PG");
        let before = vec![
            obj("public", "users", "TABLE", "id integer NOT NULL"),
            obj("public", "legacy", "TABLE", "id integer NOT NULL"),
            obj("public", "v1", "VIEW", "CREATE VIEW v1 AS SELECT 1"),
        ];
        let after = vec![
            obj(
                "public",
                "users",
                "TABLE",
                "id integer NOT NULL, email text NULL",
            ),
            obj("public", "v1", "VIEW", "CREATE VIEW v1 AS SELECT 1"),
            obj("public", "orders", "TABLE", "id integer NOT NULL"),
        ];
        let a = store
            .save_new(&profile, "app", Some("before"), 1, &before)
            .unwrap();
        let b = store
            .save_new(&profile, "app", Some("after"), 2, &after)
            .unwrap();
        let source = store.read(&a.id).unwrap();
        let target = store.read(&b.id).unwrap();
        let diffs = sqlly_services::compare_objects(
            &source,
            &target,
            DatabaseType::Postgres,
            DatabaseType::Postgres,
        );
        let find = |name: &str| diffs.iter().find(|d| d.object.name == name).unwrap();
        assert_eq!(
            find("users").status,
            sqlly_services::DifferenceStatus::Changed
        );
        assert_eq!(
            find("legacy").status,
            sqlly_services::DifferenceStatus::MissingInTarget
        );
        assert_eq!(
            find("orders").status,
            sqlly_services::DifferenceStatus::ExtraInTarget
        );
        assert!(!diffs.iter().any(|d| d.object.name == "v1"));
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn titles_and_times_format_predictably() {
        // 2026-09-26 14:03:00 UTC.
        let ms = 1_790_431_380_000;
        assert_eq!(format_taken_at_in(ms, &chrono::Utc), "2026-09-26 14:03");
        let east = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(format_taken_at_in(ms, &east), "2026-09-26 16:03");
        let labeled = meta("a", ms, Some("before release"));
        assert_eq!(
            snapshot_title_with(&labeled, format_taken_at_in(ms, &chrono::Utc)),
            "before release @ 2026-09-26 14:03"
        );
        let unlabeled = meta("b", ms, None);
        assert_eq!(
            snapshot_title_with(&unlabeled, format_taken_at_in(ms, &chrono::Utc)),
            "Snapshot @ 2026-09-26 14:03"
        );
        assert!(picker_label(&labeled).starts_with("Snapshot: before release @ "));
        assert!(picker_label(&labeled).ends_with(" · app"));
        assert_eq!(format_taken_at_in(i64::MAX, &chrono::Utc), "unknown time");
    }

    #[test]
    fn picks_toggle_and_keep_the_last_two() {
        let mut picks = Vec::new();
        toggle_pick(&mut picks, TimelinePoint::Snapshot(0));
        toggle_pick(&mut picks, TimelinePoint::Snapshot(1));
        assert_eq!(
            picks,
            vec![TimelinePoint::Snapshot(0), TimelinePoint::Snapshot(1)]
        );
        toggle_pick(&mut picks, TimelinePoint::Live);
        assert_eq!(picks, vec![TimelinePoint::Snapshot(1), TimelinePoint::Live]);
        toggle_pick(&mut picks, TimelinePoint::Live);
        assert_eq!(picks, vec![TimelinePoint::Snapshot(1)]);
    }

    #[test]
    fn compare_pair_puts_the_older_point_first_and_live_last() {
        let chronological = vec![meta("old", 100, Some("old")), meta("new", 200, Some("new"))];
        let live = CompareSource::Live {
            profile: Box::new(profile("pg")),
            database: "app".into(),
        };
        let (source, target) = compare_pair(
            &[TimelinePoint::Snapshot(1), TimelinePoint::Snapshot(0)],
            &chronological,
            &live,
        )
        .unwrap();
        assert_eq!(source.display_name(), snapshot_title(&chronological[0]));
        assert_eq!(target.display_name(), snapshot_title(&chronological[1]));

        let (source, target) = compare_pair(
            &[TimelinePoint::Live, TimelinePoint::Snapshot(1)],
            &chronological,
            &live,
        )
        .unwrap();
        assert!(matches!(source, CompareSource::Snapshot(_)));
        assert!(matches!(&target, CompareSource::Live { database, .. } if database == "app"));
        assert_eq!(target.display_name(), "pg");
        assert_eq!(target.database_type(), DatabaseType::Postgres);

        assert!(compare_pair(&[TimelinePoint::Live], &chronological, &live).is_none());
        assert!(
            compare_pair(
                &[TimelinePoint::Snapshot(7), TimelinePoint::Live],
                &chronological,
                &live
            )
            .is_none(),
            "a stale index never resolves"
        );
    }
}
