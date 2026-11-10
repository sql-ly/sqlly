//! Custom Protobuf schemas for the value viewer.
//!
//! Users register `.proto` files (compiled here by a pure-Rust compiler) or
//! compiled descriptor sets in Settings ▸ Results ▸ Protobuf Schemas, then
//! pick a message type for a Protobuf value. The value is decoded into named
//! fields (the JSON tree the viewer already has); a value that does not fit
//! the chosen type — or a choice whose schema is gone — falls back to the
//! schema-less field-number view with a note.
//!
//! The registered schemas and the message type picked per value live in
//! `protobuf-schemas.json`: a Redis key's pick is kept on disk (like its
//! display format in `value-formats.json`), any other cell's for the
//! session. The browser build has no file access, so it registers nothing
//! and Protobuf stays schema-less there.

use serde::{Deserialize, Serialize};

use crate::structured_data::{RenderedPayload, ValueFormat, render_payload};

/// What a registered schema file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaKind {
    /// A `.proto` source file; its folder is the import root.
    Proto,
    /// A compiled `FileDescriptorSet` (`protoc --descriptor_set_out`).
    DescriptorSet,
}

/// One registered schema file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaEntry {
    pub id: String,
    pub path: String,
    pub kind: SchemaKind,
}

impl SchemaEntry {
    /// The file name shown in pickers and the Settings list.
    pub fn display_name(&self) -> String {
        std::path::Path::new(&self.path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.clone())
    }
}

/// The message type picked for a value: which schema, which message.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageChoice {
    pub schema_id: String,
    /// Fully-qualified message name (`package.Message`).
    pub message: String,
}

/// Picks remembered on disk at most.
pub const MESSAGE_CHOICE_CAP: usize = 2_000;

/// The on-disk store: registered schemas plus per-value message picks,
/// least recently picked first.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SchemaStore {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub schemas: Vec<SchemaEntry>,
    #[serde(default)]
    pub message_types: Vec<(String, MessageChoice)>,
}

// The browser build never reads or writes the file.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
impl SchemaStore {
    pub const FILE: &'static str = "protobuf-schemas.json";
    pub const VERSION: u32 = 1;

    pub fn message_for(&self, identity: &str) -> Option<&MessageChoice> {
        self.message_types
            .iter()
            .find(|(known, _)| known == identity)
            .map(|(_, choice)| choice)
    }

    /// Record (or, with `None`, forget) the pick for `identity` as the most
    /// recent; past the cap the oldest picks go first.
    pub fn set_message(&mut self, identity: &str, choice: Option<MessageChoice>) {
        self.message_types.retain(|(known, _)| known != identity);
        if let Some(choice) = choice {
            self.message_types.push((identity.to_string(), choice));
            let excess = self.message_types.len().saturating_sub(MESSAGE_CHOICE_CAP);
            self.message_types.drain(..excess);
        }
    }

    /// Add a schema file; the same path twice keeps one entry.
    pub fn add(&mut self, path: String, kind: SchemaKind) -> SchemaEntry {
        if let Some(existing) = self.schemas.iter().find(|entry| entry.path == path) {
            return existing.clone();
        }
        let entry = SchemaEntry {
            id: uuid::Uuid::new_v4().to_string(),
            path,
            kind,
        };
        self.schemas.push(entry.clone());
        entry
    }

    /// Remove a schema and every pick that pointed at it.
    pub fn remove(&mut self, id: &str) {
        self.schemas.retain(|entry| entry.id != id);
        self.message_types
            .retain(|(_, choice)| choice.schema_id != id);
    }
}

/// A registered schema with what compiling it produced.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaSummary {
    pub entry: SchemaEntry,
    /// The message types it defines (fully qualified, sorted), or why it
    /// could not be read.
    pub messages: Result<Vec<String>, String>,
}

/// The label for a pick in menus: `Message (file.proto)`.
pub fn choice_label(choice: &MessageChoice, schemas: &[SchemaSummary]) -> String {
    let file = schemas
        .iter()
        .find(|summary| summary.entry.id == choice.schema_id)
        .map(|summary| summary.entry.display_name());
    match file {
        Some(file) => format!("{} ({file})", choice.message),
        None => choice.message.clone(),
    }
}

/// Render `payload` as the picked message type. Anything that keeps the
/// schema from applying — the schema was removed or no longer compiles, the
/// message is gone, or the bytes do not fit it — falls back to the
/// schema-less Protobuf view with a note saying why.
pub fn render_with_choice(payload: &[u8], choice: &MessageChoice) -> RenderedPayload {
    match native::decode_with_choice(payload, choice) {
        Ok((value, unknown)) => {
            let note = if unknown.is_empty() {
                crate::localization::format(
                    "results.structured_data.protobuf_schema_note",
                    &[("message", &choice.message)],
                )
            } else {
                crate::localization::format(
                    "results.structured_data.protobuf_schema_unknown_fields_note",
                    &[
                        ("message", &choice.message),
                        (
                            "field_numbers",
                            &unknown
                                .iter()
                                .map(u32::to_string)
                                .collect::<Vec<_>>()
                                .join(", "),
                        ),
                    ],
                )
            };
            let sorted = value;
            RenderedPayload {
                text: serde_json::to_string_pretty(&sorted).unwrap_or_else(|_| sorted.to_string()),
                as_json: true,
                as_xml: false,
                as_yaml: false,
                as_markdown: false,
                note: Some(note),
                decoded: Some(ValueFormat::Protobuf),
            }
        }
        Err(reason) => {
            let mut fallback = render_payload(payload, ValueFormat::Protobuf);
            let prefix = crate::localization::format(
                "results.structured_data.protobuf_schema_mismatch_note",
                &[("message", &choice.message), ("error", &reason)],
            );
            fallback.note = Some(match fallback.note.take() {
                Some(note) => format!("{prefix} · {note}"),
                None => prefix,
            });
            fallback
        }
    }
}

/// Every registered schema with its message types (compiled on first use,
/// re-read when the file changes). Empty in the browser build.
pub fn schemas() -> Vec<SchemaSummary> {
    native::schemas()
}

/// Whether any schema is registered (the viewer hides its message picker
/// otherwise).
pub fn any_schemas() -> bool {
    native::with_store(|store| !store.schemas.is_empty()).unwrap_or(false)
}

/// The message type last picked for `identity`, if any.
pub fn remembered_message(identity: &str) -> Option<MessageChoice> {
    if let Some(choice) = native::with_store(|store| store.message_for(identity).cloned()).flatten()
    {
        return Some(choice);
    }
    SESSION_CHOICES.lock().ok()?.get(identity).cloned()
}

/// Remember (or with `None` forget) the message type picked for `identity`:
/// on disk for a Redis key on a known connection, for the session otherwise.
pub fn remember_message(identity: &str, choice: Option<MessageChoice>) -> Result<(), String> {
    if let Ok(mut session) = SESSION_CHOICES.lock() {
        match &choice {
            Some(choice) => {
                if session.len() >= crate::structured_data::FORMAT_MEMORY_CAP
                    && !session.contains_key(identity)
                {
                    session.clear();
                }
                session.insert(identity.to_string(), choice.clone());
            }
            None => {
                session.remove(identity);
            }
        }
    }
    if !crate::structured_data::is_persistent_identity(identity) {
        return Ok(());
    }
    native::update_store(|store| store.set_message(identity, choice))
}

static SESSION_CHOICES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, MessageChoice>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Register a schema file: it must compile (or decode) and define at least
/// one message type, or it is refused with the reason.
#[cfg(not(target_arch = "wasm32"))]
pub fn add_schema(path: &std::path::Path, kind: SchemaKind) -> Result<SchemaSummary, String> {
    native::add_schema(path, kind)
}

/// Unregister a schema (its file is left alone).
#[cfg(not(target_arch = "wasm32"))]
pub fn remove_schema(id: &str) -> Result<(), String> {
    native::update_store(|store| store.remove(id))?;
    native::forget_compiled(id);
    Ok(())
}

/// Drop compiled schemas so the next use re-reads every file.
#[cfg(not(target_arch = "wasm32"))]
pub fn reload_schemas() {
    native::forget_all_compiled();
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, LazyLock, Mutex};
    use std::time::SystemTime;

    use prost_reflect::{DescriptorPool, DynamicMessage, SerializeOptions};

    use super::{MessageChoice, SchemaEntry, SchemaKind, SchemaStore, SchemaSummary};

    /// Largest schema file read (a descriptor set or a .proto source).
    const MAX_SCHEMA_BYTES: u64 = 16 * 1024 * 1024;

    #[derive(Default)]
    struct State {
        loaded: bool,
        store: SchemaStore,
        /// An unreadable store file blocks writes (the user's data is in it).
        blocked: Option<String>,
    }

    static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(State::default()));

    type Compiled = Result<Arc<DescriptorPool>, String>;

    /// Compiled pools by schema id, with the file's modification time when
    /// compiled (a changed file is recompiled).
    type CompiledCache = HashMap<String, (Option<SystemTime>, Compiled)>;

    static COMPILED: LazyLock<Mutex<CompiledCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));

    /// Where the store lives; `None` in unit tests, which must not touch the
    /// user's data directory.
    fn store_path() -> Option<PathBuf> {
        if cfg!(test) {
            return None;
        }
        crate::host_storage::artifact_path(
            crate::host_storage::StorageArea::Data,
            SchemaStore::FILE,
        )
    }

    fn ensure_loaded(state: &mut State) {
        if state.loaded {
            return;
        }
        state.loaded = true;
        let Some(path) = store_path() else {
            return;
        };
        match crate::fs_safe::load_json_store::<SchemaStore>(&path) {
            crate::fs_safe::StoreLoad::Missing => {}
            crate::fs_safe::StoreLoad::Loaded(store) => state.store = store,
            crate::fs_safe::StoreLoad::Corrupt { error } => {
                state.blocked = Some(crate::localization::message(error));
            }
        }
    }

    pub fn with_store<T>(read: impl FnOnce(&SchemaStore) -> T) -> Option<T> {
        let mut state = STATE.lock().ok()?;
        ensure_loaded(&mut state);
        Some(read(&state.store))
    }

    pub fn update_store(change: impl FnOnce(&mut SchemaStore)) -> Result<(), String> {
        let Ok(mut state) = STATE.lock() else {
            return Ok(());
        };
        ensure_loaded(&mut state);
        if let Some(error) = &state.blocked {
            return Err(crate::localization::format(
                "results.structured_data.protobuf_schemas_store_blocked",
                &[("error", error)],
            ));
        }
        change(&mut state.store);
        state.store.version = SchemaStore::VERSION;
        let Some(path) = store_path() else {
            return Ok(());
        };
        let data = serde_json::to_vec_pretty(&state.store).map_err(|error| error.to_string())?;
        crate::fs_safe::write_atomic(&path, &data).map_err(|error| {
            crate::localization::format(
                "results.structured_data.protobuf_schemas_save_failed",
                &[("error", &error.to_string())],
            )
        })
    }

    fn modified(path: &Path) -> Option<SystemTime> {
        std::fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
    }

    /// Compile (or decode) one schema file into a descriptor pool.
    pub fn compile(path: &Path, kind: SchemaKind) -> Result<DescriptorPool, String> {
        let size = std::fs::metadata(path)
            .map_err(|error| read_failed(path, &error.to_string()))?
            .len();
        if size > MAX_SCHEMA_BYTES {
            return Err(read_failed(path, &format!("{size} bytes")));
        }
        match kind {
            SchemaKind::DescriptorSet => {
                let bytes =
                    std::fs::read(path).map_err(|error| read_failed(path, &error.to_string()))?;
                DescriptorPool::decode(bytes.as_slice())
                    .map_err(|error| read_failed(path, &error.to_string()))
            }
            SchemaKind::Proto => {
                let root = path.parent().unwrap_or(Path::new("."));
                let mut compiler = protox::Compiler::new([root])
                    .map_err(|error| read_failed(path, &error.to_string()))?;
                compiler.include_imports(true);
                compiler
                    .open_file(path)
                    .map_err(|error| read_failed(path, &error.to_string()))?;
                Ok(compiler.descriptor_pool())
            }
        }
    }

    fn read_failed(path: &Path, error: &str) -> String {
        crate::localization::format(
            "results.structured_data.protobuf_schema_read_failed",
            &[
                ("path", &path.display().to_string()),
                ("error", &error.to_string()),
            ],
        )
    }

    /// The message types a pool defines, excluding the well-known
    /// `google.protobuf.*` ones pulled in by imports.
    pub fn message_names(pool: &DescriptorPool) -> Vec<String> {
        let mut names: Vec<String> = pool
            .all_messages()
            .map(|message| message.full_name().to_string())
            .filter(|name| !name.starts_with("google.protobuf."))
            .collect();
        names.sort();
        names
    }

    fn compiled(entry: &SchemaEntry) -> Compiled {
        let path = PathBuf::from(&entry.path);
        let stamp = modified(&path);
        if let Ok(cache) = COMPILED.lock()
            && let Some((cached_stamp, result)) = cache.get(&entry.id)
            && *cached_stamp == stamp
        {
            return result.clone();
        }
        let result = compile(&path, entry.kind).map(Arc::new);
        if let Ok(mut cache) = COMPILED.lock() {
            cache.insert(entry.id.clone(), (stamp, result.clone()));
        }
        result
    }

    pub fn forget_compiled(id: &str) {
        if let Ok(mut cache) = COMPILED.lock() {
            cache.remove(id);
        }
    }

    pub fn forget_all_compiled() {
        if let Ok(mut cache) = COMPILED.lock() {
            cache.clear();
        }
    }

    pub fn schemas() -> Vec<SchemaSummary> {
        let entries = with_store(|store| store.schemas.clone()).unwrap_or_default();
        entries
            .into_iter()
            .map(|entry| {
                let messages = compiled(&entry).map(|pool| message_names(&pool));
                SchemaSummary { entry, messages }
            })
            .collect()
    }

    pub fn add_schema(path: &Path, kind: SchemaKind) -> Result<SchemaSummary, String> {
        let pool = compile(path, kind)?;
        let messages = message_names(&pool);
        if messages.is_empty() {
            return Err(read_failed(
                path,
                &crate::localization::text("results.structured_data.protobuf_schema_no_messages"),
            ));
        }
        let mut added = None;
        update_store(|store| {
            added = Some(store.add(path.display().to_string(), kind));
        })?;
        let entry = added.ok_or_else(|| read_failed(path, "store"))?;
        if let Ok(mut cache) = COMPILED.lock() {
            cache.insert(entry.id.clone(), (modified(path), Ok(Arc::new(pool))));
        }
        Ok(SchemaSummary {
            entry,
            messages: Ok(messages),
        })
    }

    fn pool_for(choice: &MessageChoice) -> Result<Arc<DescriptorPool>, String> {
        let entry = with_store(|store| {
            store
                .schemas
                .iter()
                .find(|entry| entry.id == choice.schema_id)
                .cloned()
        })
        .flatten()
        .ok_or_else(|| {
            crate::localization::text("results.structured_data.protobuf_schema_missing")
        })?;
        compiled(&entry)
    }

    pub fn decode_with_choice(
        payload: &[u8],
        choice: &MessageChoice,
    ) -> Result<(serde_json::Value, Vec<u32>), String> {
        let pool = pool_for(choice)?;
        decode_message(&pool, &choice.message, payload)
    }

    /// Decode `payload` as `message` from `pool` into JSON with the field
    /// names from the schema, plus the numbers of fields the schema does not
    /// define. A payload none of whose fields the schema knows is refused —
    /// it is some other message.
    pub fn decode_message(
        pool: &DescriptorPool,
        message: &str,
        payload: &[u8],
    ) -> Result<(serde_json::Value, Vec<u32>), String> {
        let descriptor = pool.get_message_by_name(message).ok_or_else(|| {
            crate::localization::format(
                "results.structured_data.protobuf_message_missing",
                &[("message", message)],
            )
        })?;
        let decoded =
            DynamicMessage::decode(descriptor, payload).map_err(|error| error.to_string())?;
        let mut unknown: Vec<u32> = decoded
            .unknown_fields()
            .map(|field| field.number())
            .collect();
        unknown.sort_unstable();
        unknown.dedup();
        let known = decoded.fields().count();
        if !payload.is_empty() && known == 0 && !unknown.is_empty() {
            return Err(crate::localization::text(
                "results.structured_data.protobuf_schema_no_known_fields",
            ));
        }
        let options = SerializeOptions::new()
            .use_proto_field_name(true)
            .stringify_64_bit_integers(false)
            .skip_default_fields(false);
        let value = decoded
            .serialize_with_options(serde_json::value::Serializer, &options)
            .map_err(|error| error.to_string())?;
        Ok((value, unknown))
    }
}

/// The browser build reads no files: nothing is registered, and every pick
/// falls back to the schema-less view.
#[cfg(target_arch = "wasm32")]
mod native {
    use super::{MessageChoice, SchemaStore, SchemaSummary};

    pub fn with_store<T>(read: impl FnOnce(&SchemaStore) -> T) -> Option<T> {
        Some(read(&SchemaStore::default()))
    }

    pub fn update_store(_change: impl FnOnce(&mut SchemaStore)) -> Result<(), String> {
        Ok(())
    }

    pub fn schemas() -> Vec<SchemaSummary> {
        Vec::new()
    }

    pub fn decode_with_choice(
        _payload: &[u8],
        _choice: &MessageChoice,
    ) -> Result<(serde_json::Value, Vec<u32>), String> {
        Err(crate::localization::text(
            "results.structured_data.protobuf_schema_missing",
        ))
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    const PROTO: &str = r#"
syntax = "proto3";
package shop;

enum Role {
  ROLE_UNKNOWN = 0;
  ROLE_ADMIN = 1;
}

message Address {
  string city = 1;
}

message Person {
  string name = 1;
  int32 id = 2;
  repeated string tags = 3;
  Address address = 4;
  Role role = 5;
}

message Counter {
  int64 count = 1;
}
"#;

    /// `Person { name: "Ada", id: 42, tags: ["x", "y"], address: { city:
    /// "NY" }, role: ROLE_ADMIN }`, encoded by hand.
    fn person_bytes() -> Vec<u8> {
        let mut bytes = vec![0x0a, 0x03, b'A', b'd', b'a']; // 1: "Ada"
        bytes.extend([0x10, 0x2a]); // 2: 42
        bytes.extend([0x1a, 0x01, b'x', 0x1a, 0x01, b'y']); // 3: "x", "y"
        bytes.extend([0x22, 0x04, 0x0a, 0x02, b'N', b'Y']); // 4: { 1: "NY" }
        bytes.extend([0x28, 0x01]); // 5: 1
        bytes
    }

    fn proto_on_disk() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shop.proto");
        std::fs::write(&path, PROTO).unwrap();
        (dir, path)
    }

    #[test]
    fn a_proto_file_decodes_values_into_named_fields() {
        let (_dir, path) = proto_on_disk();
        let pool = native::compile(&path, SchemaKind::Proto).unwrap();
        assert_eq!(
            native::message_names(&pool),
            vec!["shop.Address", "shop.Counter", "shop.Person"]
        );
        let (value, unknown) =
            native::decode_message(&pool, "shop.Person", &person_bytes()).unwrap();
        assert!(unknown.is_empty());
        assert_eq!(value["name"], "Ada");
        assert_eq!(value["id"], 42);
        assert_eq!(value["tags"], serde_json::json!(["x", "y"]));
        assert_eq!(value["address"]["city"], "NY");
        assert_eq!(value["role"], "ROLE_ADMIN");
    }

    #[test]
    fn a_compiled_descriptor_set_works_too() {
        let (dir, path) = proto_on_disk();
        let mut compiler = protox::Compiler::new([dir.path()]).unwrap();
        compiler.open_file(&path).unwrap();
        let set_path = dir.path().join("shop.pb");
        std::fs::write(&set_path, compiler.encode_file_descriptor_set()).unwrap();
        let pool = native::compile(&set_path, SchemaKind::DescriptorSet).unwrap();
        let (value, _) = native::decode_message(&pool, "shop.Person", &person_bytes()).unwrap();
        assert_eq!(value["address"]["city"], "NY");
    }

    #[test]
    fn mismatched_messages_and_missing_schemas_fall_back_to_field_numbers() {
        let (_dir, path) = proto_on_disk();
        let pool = native::compile(&path, SchemaKind::Proto).unwrap();
        // Field 1 is a string in Person but a varint in Counter.
        assert!(native::decode_message(&pool, "shop.Counter", &person_bytes()).is_err());
        // An unknown message name.
        assert!(native::decode_message(&pool, "shop.Nope", &person_bytes()).is_err());
        // A value made only of fields the message does not define.
        assert!(native::decode_message(&pool, "shop.Address", &[0x50, 0x01]).is_err());

        // A pick whose schema is not registered renders schema-less, with
        // the reason in the note.
        let choice = MessageChoice {
            schema_id: "gone".to_string(),
            message: "shop.Person".to_string(),
        };
        let rendered = render_with_choice(&person_bytes(), &choice);
        assert_eq!(rendered.decoded, Some(ValueFormat::Protobuf));
        assert!(rendered.as_json);
        assert!(rendered.text.contains("\"1\""), "{}", rendered.text);
        assert!(rendered.note.unwrap_or_default().contains("shop.Person"));
    }

    #[test]
    fn unknown_fields_are_listed_not_hidden() {
        let (_dir, path) = proto_on_disk();
        let pool = native::compile(&path, SchemaKind::Proto).unwrap();
        let mut bytes = person_bytes();
        bytes.extend([0x50, 0x07]); // field 10 = 7, not in Person
        let (value, unknown) = native::decode_message(&pool, "shop.Person", &bytes).unwrap();
        assert_eq!(value["name"], "Ada");
        assert_eq!(unknown, vec![10]);
    }

    #[test]
    fn broken_proto_files_are_refused_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.proto");
        std::fs::write(&path, "syntax = \"proto3\"; message {").unwrap();
        assert!(native::compile(&path, SchemaKind::Proto).is_err());
        assert!(native::compile(&dir.path().join("missing.proto"), SchemaKind::Proto).is_err());
    }

    #[test]
    fn the_store_remembers_picks_per_value_and_forgets_removed_schemas() {
        let mut store = SchemaStore::default();
        let entry = store.add("/x/shop.proto".to_string(), SchemaKind::Proto);
        assert_eq!(
            store.add("/x/shop.proto".to_string(), SchemaKind::Proto),
            entry
        );
        let choice = MessageChoice {
            schema_id: entry.id.clone(),
            message: "shop.Person".to_string(),
        };
        store.set_message(
            "redis\u{1f}conn\u{1f}0\u{1f}6b\u{1f}value",
            Some(choice.clone()),
        );
        assert_eq!(
            store.message_for("redis\u{1f}conn\u{1f}0\u{1f}6b\u{1f}value"),
            Some(&choice)
        );
        let json = serde_json::to_string(&store).unwrap();
        let back: SchemaStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back, store);
        store.remove(&entry.id);
        assert!(store.schemas.is_empty());
        assert!(store.message_types.is_empty());
        // An empty or older file reads as defaults.
        let empty: SchemaStore = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, SchemaStore::default());
    }

    #[test]
    fn session_picks_are_remembered_for_non_redis_cells() {
        let identity = "select 1\u{1f}0\u{1f}0\u{1f}payload-protobuf-test";
        let choice = MessageChoice {
            schema_id: "s".to_string(),
            message: "shop.Person".to_string(),
        };
        remember_message(identity, Some(choice.clone())).unwrap();
        assert_eq!(remembered_message(identity), Some(choice));
        remember_message(identity, None).unwrap();
        assert_eq!(remembered_message(identity), None);
    }
}
