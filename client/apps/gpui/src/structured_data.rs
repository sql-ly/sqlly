//! Pure, AppKit-free detection, pretty-printing, and syntax classification
//! of JSON and XML cell values, plus image and spatial sniffing for the
//! cell preview viewer.

use std::sync::OnceLock;

use regex::Regex;
use sqlly_core::CellValue;
use sqlly_core::spatial::{self, Geometry};

/// The kind of structured data detected inside a result-grid cell value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StructuredDataKind {
    Json,
    Xml,
    /// A binary/varbinary value shown as a canonical hex dump. Not detected
    /// from text — the viewer is opened explicitly for a `CellValue::Bytes`.
    Hex,
    Image,
    Spatial,
    /// YAML text: highlighted, with the JSON-style tree.
    Yaml,
    /// Markdown text: highlighted, with a rendered preview.
    Markdown,
}

/// The viewer's display format for a payload. A choice for a Redis key's
/// value is remembered on disk per connection, database and key (see
/// [`remember_format`]); any other cell's choice lasts for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueFormat {
    /// What the viewer always did: sniff JSON/XML/image/geometry, text
    /// otherwise.
    Auto,
    /// The payload as text (lossy when it is not valid UTF-8).
    Utf8,
    /// Parsed and pretty-printed JSON; invalid JSON keeps the raw text with
    /// an honest note.
    Json,
    /// YAML, highlighted with a tree when it parses; text that is not YAML
    /// keeps the raw text with a note.
    Yaml,
    /// Markdown, highlighted with a rendered preview.
    Markdown,
    /// The canonical hex dump.
    Hex,
    /// MessagePack decoded to JSON; an undecodable payload falls back to the
    /// hex dump with a note.
    MessagePack,
    /// Base64 (standard or URL-safe, padded or not) decoded, then shown as
    /// whatever the decoded bytes are (JSON, text, hex, …).
    Base64,
    /// A protocol-buffers message walked without a schema: field numbers
    /// and wire values as JSON.
    Protobuf,
    /// A gzip member decompressed, then shown as whatever it contains.
    Gzip,
    /// A zstd frame decompressed (native builds only), then shown as
    /// whatever it contains.
    Zstd,
    /// An LZ4 frame (or size-prepended block) decompressed, then shown as
    /// whatever it contains.
    Lz4,
    /// A zlib or raw deflate stream decompressed, then shown as whatever it
    /// contains.
    Deflate,
    /// MySQL `COMPRESS()` output (a 4-byte little-endian uncompressed
    /// length, then a zlib stream) decompressed, then shown as whatever it
    /// contains.
    MySqlCompress,
    /// PHP `serialize()` output decoded to JSON.
    PhpSerialized,
    /// Java `ObjectOutputStream` data: the stream header is recognised and
    /// the class names and strings in it are listed; field values are not
    /// decoded.
    JavaSerialized,
    /// Python pickle: the protocol header is recognised and the opcodes,
    /// globals and strings are summarised; the object is not rebuilt.
    Pickle,
}

/// Where a format sits in the picker menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormatGroup {
    /// The views that never decode: Auto, UTF-8, JSON, Hex.
    Plain,
    /// Encodings decoded to a readable form.
    Encoded,
    /// Compressed payloads, decompressed then re-detected.
    Compressed,
    /// Language serializers, decoded or summarised.
    Serialized,
}

impl FormatGroup {
    pub const ALL: [FormatGroup; 4] = [
        FormatGroup::Plain,
        FormatGroup::Encoded,
        FormatGroup::Compressed,
        FormatGroup::Serialized,
    ];
}

impl ValueFormat {
    pub const ALL: [ValueFormat; 17] = [
        ValueFormat::Auto,
        ValueFormat::Utf8,
        ValueFormat::Json,
        ValueFormat::Yaml,
        ValueFormat::Markdown,
        ValueFormat::Hex,
        ValueFormat::MessagePack,
        ValueFormat::Base64,
        ValueFormat::Protobuf,
        ValueFormat::Gzip,
        ValueFormat::Zstd,
        ValueFormat::Lz4,
        ValueFormat::Deflate,
        ValueFormat::MySqlCompress,
        ValueFormat::PhpSerialized,
        ValueFormat::JavaSerialized,
        ValueFormat::Pickle,
    ];

    /// Toolbar label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Utf8 => "UTF-8",
            Self::Json => "JSON",
            Self::Yaml => "YAML",
            Self::Markdown => "Markdown",
            Self::Hex => "Hex",
            Self::MessagePack => "MessagePack",
            Self::Base64 => "Base64",
            Self::Protobuf => "Protobuf",
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
            Self::Lz4 => "LZ4",
            Self::Deflate => "deflate",
            Self::MySqlCompress => "MySQL COMPRESS",
            Self::PhpSerialized => "PHP serialize",
            Self::JavaSerialized => "Java serialization",
            Self::Pickle => "Python pickle",
        }
    }

    pub fn localized_label(self) -> String {
        match self {
            Self::Auto => crate::localization::text("results.structured_data.format_auto"),
            Self::PhpSerialized => {
                crate::localization::text("results.structured_data.format_php_serialized")
            }
            Self::JavaSerialized => {
                crate::localization::text("results.structured_data.format_java_serialized")
            }
            _ => self.label().to_string(),
        }
    }

    /// The stable on-disk name (the serde name) of a format.
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Utf8 => "utf8",
            Self::Json => "json",
            Self::Yaml => "yaml",
            Self::Markdown => "markdown",
            Self::Hex => "hex",
            Self::MessagePack => "message_pack",
            Self::Base64 => "base64",
            Self::Protobuf => "protobuf",
            Self::Gzip => "gzip",
            Self::Zstd => "zstd",
            Self::Lz4 => "lz4",
            Self::Deflate => "deflate",
            Self::MySqlCompress => "my_sql_compress",
            Self::PhpSerialized => "php_serialized",
            Self::JavaSerialized => "java_serialized",
            Self::Pickle => "pickle",
        }
    }

    /// The format with a given on-disk name; `None` for a name this build
    /// does not know (a newer SQLly's format, or a corrupt entry).
    pub fn from_name(name: &str) -> Option<ValueFormat> {
        Self::ALL.into_iter().find(|format| format.name() == name)
    }

    pub fn group(self) -> FormatGroup {
        match self {
            Self::Auto | Self::Utf8 | Self::Json | Self::Yaml | Self::Markdown | Self::Hex => {
                FormatGroup::Plain
            }
            Self::MessagePack | Self::Base64 | Self::Protobuf => FormatGroup::Encoded,
            Self::Gzip | Self::Zstd | Self::Lz4 | Self::Deflate | Self::MySqlCompress => {
                FormatGroup::Compressed
            }
            Self::PhpSerialized | Self::JavaSerialized | Self::Pickle => FormatGroup::Serialized,
        }
    }

    /// Whether this build can decode the format at all. zstd links a C
    /// library and is native only; the browser build says so instead of
    /// pretending.
    pub fn available(self) -> bool {
        match self {
            Self::Zstd => cfg!(not(target_arch = "wasm32")),
            _ => true,
        }
    }
}

/// Session memory of explicit format choices, keyed by a caller-built value
/// identity (see [`value_format_identity`]: a Redis value by its key, any
/// other result cell by statement, result slot, row and column). Bounded
/// oldest-first; choosing Auto forgets the entry. Choices for Redis keys on a
/// known connection are also written to disk (see [`ValueFormatStore`]).
#[derive(Debug)]
pub struct FormatMemory {
    cap: usize,
    order: std::collections::VecDeque<String>,
    formats: std::collections::HashMap<String, ValueFormat>,
}

/// Value identities the session format memory keeps.
pub const FORMAT_MEMORY_CAP: usize = 512;

impl FormatMemory {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            order: std::collections::VecDeque::new(),
            formats: std::collections::HashMap::new(),
        }
    }

    pub fn get(&self, identity: &str) -> Option<ValueFormat> {
        self.formats.get(identity).copied()
    }

    /// Record a choice; `Auto` removes the identity (auto is the default).
    pub fn set(&mut self, identity: &str, format: ValueFormat) {
        self.order.retain(|known| known != identity);
        if format == ValueFormat::Auto {
            self.formats.remove(identity);
            return;
        }
        while self.order.len() >= self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.formats.remove(&oldest);
            }
        }
        self.order.push_back(identity.to_string());
        self.formats.insert(identity.to_string(), format);
    }

    #[cfg(test)]
    pub fn remembered_count(&self) -> usize {
        self.formats.len()
    }
}

static FORMAT_MEMORY: std::sync::LazyLock<std::sync::Mutex<FormatMemory>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(FormatMemory::new(FORMAT_MEMORY_CAP)));

/// Prefix of identities that persist: a Redis key on a known connection.
const PERSISTENT_IDENTITY_PREFIX: &str = "redis\u{1f}";

/// Whether choices for `identity` (a Redis key on a known connection) are
/// kept on disk rather than for the session.
pub fn is_persistent_identity(identity: &str) -> bool {
    identity.starts_with(PERSISTENT_IDENTITY_PREFIX)
}

/// Remembered Redis key formats kept on disk (`value-formats.json`).
pub const VALUE_FORMAT_STORE_CAP: usize = 2_000;

/// The on-disk layout version of `value-formats.json`. Version 1 (unversioned)
/// files hold the same entries; the number exists so a later layout change
/// can be told apart. Loading never refuses a version: entries are read by
/// name and an unknown format name falls back to Auto (the entry is dropped).
pub const VALUE_FORMAT_STORE_VERSION: u32 = 2;

/// The on-disk format memory for Redis keys: `(identity, format)` pairs,
/// least recently chosen first, capped at [`VALUE_FORMAT_STORE_CAP`].
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ValueFormatStore {
    pub version: u32,
    pub entries: Vec<(String, ValueFormat)>,
}

impl Default for ValueFormatStore {
    fn default() -> Self {
        Self {
            version: VALUE_FORMAT_STORE_VERSION,
            entries: Vec::new(),
        }
    }
}

/// The file as written by any version: format names stay strings here so a
/// name this build does not know (one a newer SQLly wrote) reads as Auto
/// instead of refusing the whole file.
#[derive(serde::Deserialize)]
struct RawValueFormatStore {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    entries: Vec<(String, String)>,
}

impl<'de> serde::Deserialize<'de> for ValueFormatStore {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawValueFormatStore::deserialize(deserializer)?;
        let mut store = Self {
            version: raw.version.max(1),
            entries: Vec::with_capacity(raw.entries.len().min(VALUE_FORMAT_STORE_CAP)),
        };
        for (identity, name) in raw.entries {
            let format = ValueFormat::from_name(&name).unwrap_or(ValueFormat::Auto);
            store.set(&identity, format);
        }
        store.version = VALUE_FORMAT_STORE_VERSION;
        Ok(store)
    }
}

impl ValueFormatStore {
    pub const FILE: &'static str = "value-formats.json";

    pub fn get(&self, identity: &str) -> Option<ValueFormat> {
        self.entries
            .iter()
            .find(|(known, _)| known == identity)
            .map(|(_, format)| *format)
    }

    /// Record a choice as the most recent; `Auto` forgets the identity.
    /// Past the cap the least recently chosen entries go first.
    pub fn set(&mut self, identity: &str, format: ValueFormat) {
        self.entries.retain(|(known, _)| known != identity);
        if format == ValueFormat::Auto {
            return;
        }
        self.entries.push((identity.to_string(), format));
        let excess = self.entries.len().saturating_sub(VALUE_FORMAT_STORE_CAP);
        self.entries.drain(..excess);
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn load_from(path: &std::path::Path) -> Result<Self, String> {
        match crate::fs_safe::load_json_store::<Self>(path) {
            crate::fs_safe::StoreLoad::Missing => Ok(Self::default()),
            crate::fs_safe::StoreLoad::Loaded(store) => Ok(store),
            crate::fs_safe::StoreLoad::Corrupt { error } => Err(crate::localization::format(
                "results.structured_data.remembered_formats_load_failed",
                &[("error", &crate::localization::message(error))],
            )),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn save_to(&self, path: &std::path::Path) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(self).map_err(|err| {
            crate::localization::format(
                "results.structured_data.remembered_formats_encode_failed",
                &[("error", &err.to_string())],
            )
        })?;
        crate::fs_safe::write_atomic(path, &data).map_err(|err| {
            crate::localization::format(
                "results.structured_data.remembered_formats_save_failed",
                &[("error", &err.to_string())],
            )
        })
    }
}

/// The process-wide persistent store: loaded on first use, written back off
/// the UI thread after every change. An unreadable file blocks writes (the
/// user's choices are in it) and the error is reported to the caller.
#[derive(Default)]
struct PersistentFormats {
    loaded: bool,
    store: ValueFormatStore,
    blocked: Option<String>,
}

static PERSISTENT_FORMATS: std::sync::LazyLock<std::sync::Mutex<PersistentFormats>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(PersistentFormats::default()));

/// Serializes background writes so an older snapshot never lands last.
#[cfg(not(target_arch = "wasm32"))]
static PERSISTENT_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where the store lives; `None` in unit tests, which must not touch the
/// user's data directory.
#[cfg(not(target_arch = "wasm32"))]
fn value_format_store_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::host_storage::artifact_path(
        crate::host_storage::StorageArea::Data,
        ValueFormatStore::FILE,
    )
}

impl PersistentFormats {
    fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = value_format_store_path() {
            match ValueFormatStore::load_from(&path) {
                Ok(store) => self.store = store,
                Err(error) => self.blocked = Some(error),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.store = crate::preferences::wasm_load_json(ValueFormatStore::FILE);
        }
    }
}

/// Write the current store in the background (native) or to browser storage.
fn persist_formats(store: ValueFormatStore) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Some(path) = value_format_store_path() else {
            return;
        };
        std::thread::spawn(move || {
            let _serial = PERSISTENT_WRITE
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            // Write the newest state, not necessarily the one this thread was
            // spawned with, so racing writes converge on the latest.
            let latest = PERSISTENT_FORMATS
                .lock()
                .map(|formats| formats.store.clone())
                .unwrap_or(store);
            if let Err(error) = latest.save_to(&path) {
                tracing::warn!(%error, "value format store write failed");
            }
        });
    }
    #[cfg(target_arch = "wasm32")]
    crate::preferences::wasm_save_json(ValueFormatStore::FILE, &store);
}

/// The format last picked for `identity`, if any: from disk for a Redis key
/// on a known connection, from this session's memory otherwise.
pub fn remembered_format(identity: &str) -> Option<ValueFormat> {
    if identity.starts_with(PERSISTENT_IDENTITY_PREFIX) {
        let mut formats = PERSISTENT_FORMATS.lock().ok()?;
        formats.ensure_loaded();
        if let Some(format) = formats.store.get(identity) {
            return Some(format);
        }
    }
    FORMAT_MEMORY.lock().ok()?.get(identity)
}

/// Remember (or, for Auto, forget) the format picked for `identity`. A Redis
/// key's choice is also written to disk; `Err` says why it could not be (the
/// store file is unreadable and is left alone) — the choice still holds for
/// this session.
pub fn remember_format(identity: &str, format: ValueFormat) -> Result<(), String> {
    if let Ok(mut memory) = FORMAT_MEMORY.lock() {
        memory.set(identity, format);
    }
    if !identity.starts_with(PERSISTENT_IDENTITY_PREFIX) {
        return Ok(());
    }
    let Ok(mut formats) = PERSISTENT_FORMATS.lock() else {
        return Ok(());
    };
    formats.ensure_loaded();
    if let Some(error) = &formats.blocked {
        return Err(crate::localization::format(
            "results.structured_data.remembered_formats_session_only",
            &[("error", error)],
        ));
    }
    formats.store.set(identity, format);
    let snapshot = formats.store.clone();
    drop(formats);
    persist_formats(snapshot);
    Ok(())
}

/// Where a result came from, for format identities that outlive the
/// session: the connection profile id and the database it ran in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatScope {
    pub connection_id: String,
    pub database: String,
}

/// The identity of one result cell for the format memory.
pub fn cell_format_identity(statement: &str, slot: usize, row: usize, column: &str) -> String {
    format!("{}\u{1f}{slot}\u{1f}{row}\u{1f}{column}", statement.trim())
}

/// Redis commands that read one key's value, with the key as their first
/// argument: the value a result grid shows came from that key.
const REDIS_KEY_READS: &[&str] = &[
    "GET",
    "GETRANGE",
    "HGET",
    "HGETALL",
    "HMGET",
    "HVALS",
    "LINDEX",
    "LRANGE",
    "SMEMBERS",
    "SRANDMEMBER",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZREVRANGE",
    "XRANGE",
    "XREVRANGE",
    "JSON.GET",
];

/// The key a statement reads, when it is exactly one Redis key-read
/// command (`GET k`, `HGETALL k`, …). No SQL dialect starts a statement
/// with these words, so SQL never matches.
pub fn redis_value_key(statement: &str) -> Option<Vec<u8>> {
    let commands = sqlly_intel::query::safety::redis::parse_script(statement).ok()?;
    let [command] = commands.as_slice() else {
        return None;
    };
    if !REDIS_KEY_READS.contains(&command.name().as_str()) {
        return None;
    }
    command.args.get(1).cloned()
}

/// The format-memory identity of a result cell: for a Redis key read, the
/// KEY and column (so a format picked for `user:42`'s value sticks to that
/// key whichever command, row or tab shows it next); otherwise
/// [`cell_format_identity`].
#[cfg(test)]
pub fn value_format_identity(statement: &str, slot: usize, row: usize, column: &str) -> String {
    scoped_value_format_identity(None, statement, slot, row, column)
}

/// [`value_format_identity`] for a result whose connection is known: a
/// Redis key's identity then names the connection and database too, and a
/// format picked for it is remembered on disk across sessions.
pub fn scoped_value_format_identity(
    scope: Option<&FormatScope>,
    statement: &str,
    slot: usize,
    row: usize,
    column: &str,
) -> String {
    match redis_value_key(statement) {
        Some(key) => {
            let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
            match scope {
                Some(scope) => format!(
                    "{PERSISTENT_IDENTITY_PREFIX}{}\u{1f}{}\u{1f}{hex}\u{1f}{column}",
                    scope.connection_id, scope.database
                ),
                None => format!("redis-key\u{1f}{hex}\u{1f}{column}"),
            }
        }
        None => cell_format_identity(statement, slot, row, column),
    }
}

/// The byte-exact payload the format picker re-renders a cell from:
/// binary cells as their bytes, text cells as their UTF-8; `None` for
/// images and geometry (their own viewers) and non-text scalars.
pub fn picker_payload(cell: &CellValue, preview: &ContentPreview) -> Option<Vec<u8>> {
    match (cell, preview) {
        (_, ContentPreview::Image(..)) | (_, ContentPreview::Spatial(_)) => None,
        (CellValue::Bytes(bytes), _) => Some(bytes.clone()),
        (CellValue::String(text), _) => Some(text.as_bytes().to_vec()),
        _ => None,
    }
}

/// Payloads larger than this get no format picker: the choice is per-view
/// and held in memory, and multi-megabyte values already truncate in every
/// rendering.
pub const FORMAT_PICKER_MAX_BYTES: usize = 1024 * 1024;

/// What a non-Auto format made of a payload.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedPayload {
    pub text: String,
    /// The text is JSON: pretty-print spans and the tree view apply.
    pub as_json: bool,
    /// The text is XML: the XML highlighter and tree apply.
    pub as_xml: bool,
    /// The text is YAML: the YAML highlighter and tree apply.
    pub as_yaml: bool,
    /// The text is Markdown: the Markdown highlighter and preview apply.
    pub as_markdown: bool,
    /// An honest caveat to show with the text ("not valid JSON — showing
    /// the raw text", "gzip → JSON, 1,234 → 56,789 bytes").
    pub note: Option<String>,
    /// The format the payload really was decoded as; `None` when the text
    /// is a plain UTF-8/hex rendering or a fallback after a failed decode.
    pub decoded: Option<ValueFormat>,
}

impl RenderedPayload {
    fn utf8(payload: &[u8]) -> Self {
        let (text, lossy) = match String::from_utf8(payload.to_vec()) {
            Ok(text) => (text, false),
            Err(error) => (String::from_utf8_lossy(error.as_bytes()).into_owned(), true),
        };
        Self {
            text,
            as_json: false,
            as_xml: false,
            as_yaml: false,
            as_markdown: false,
            note: lossy
                .then(|| crate::localization::text("results.structured_data.invalid_utf8_note")),
            decoded: None,
        }
    }

    fn hex(payload: &[u8], note: Option<String>) -> Self {
        Self {
            text: hex_dump(payload),
            as_json: false,
            as_xml: false,
            as_yaml: false,
            as_markdown: false,
            note,
            decoded: None,
        }
    }

    fn json_value(value: &serde_json::Value, decoded: ValueFormat, note: Option<String>) -> Self {
        let sorted = sort_json_value(value);
        Self {
            text: serde_json::to_string_pretty(&sorted).unwrap_or_else(|_| value.to_string()),
            as_json: true,
            as_xml: false,
            as_yaml: false,
            as_markdown: false,
            note,
            decoded: Some(decoded),
        }
    }

    /// The failed-decode fallback: the hex dump with the decoder's reason.
    fn failed(payload: &[u8], format: ValueFormat, error: &str) -> Self {
        Self::hex(
            payload,
            Some(crate::localization::format(
                "results.structured_data.decode_failed_fallback",
                &[
                    ("format_name", &format.localized_label()),
                    ("error", &error.to_string()),
                ],
            )),
        )
    }

    fn prefix_note(mut self, prefix: &str) -> Self {
        self.note = Some(match self.note.take() {
            Some(note) => format!("{prefix} · {note}"),
            None => prefix.to_string(),
        });
        self
    }
}

/// Decimal with thousands separators, for the decode notes.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Render a payload in an explicit format. `Auto` is the caller's own
/// detection path and is treated as UTF-8 here (see [`render_auto`] for
/// the sniffing Auto does on top of the viewer's JSON/XML detection).
pub fn render_payload(payload: &[u8], format: ValueFormat) -> RenderedPayload {
    match format {
        ValueFormat::Auto | ValueFormat::Utf8 => RenderedPayload::utf8(payload),
        ValueFormat::Json => {
            let text = String::from_utf8_lossy(payload).into_owned();
            let trimmed = text.trim();
            if is_json(trimmed) {
                RenderedPayload {
                    text: pretty_print_json(trimmed).unwrap_or(text),
                    as_json: true,
                    as_xml: false,
                    as_yaml: false,
                    as_markdown: false,
                    note: None,
                    decoded: Some(ValueFormat::Json),
                }
            } else {
                RenderedPayload {
                    text,
                    as_json: false,
                    as_xml: false,
                    as_yaml: false,
                    as_markdown: false,
                    note: Some(crate::localization::text(
                        "results.structured_data.invalid_json_note",
                    )),
                    decoded: None,
                }
            }
        }
        ValueFormat::Yaml | ValueFormat::Markdown => {
            let text = String::from_utf8_lossy(payload).into_owned();
            let yaml = format == ValueFormat::Yaml;
            let valid = if yaml {
                matches!(
                    crate::structured_text::parse_yaml(&text),
                    Some(serde_json::Value::Object(_) | serde_json::Value::Array(_))
                )
            } else {
                true
            };
            RenderedPayload {
                text,
                as_json: false,
                as_xml: false,
                as_yaml: yaml && valid,
                as_markdown: !yaml,
                note: (!valid).then(|| {
                    crate::localization::text("results.structured_data.invalid_yaml_note")
                }),
                decoded: valid.then_some(format),
            }
        }
        ValueFormat::Hex => RenderedPayload::hex(payload, None),
        ValueFormat::MessagePack => match crate::msgpack::decode(payload) {
            Ok(value) => RenderedPayload::json_value(&value, format, None),
            Err(error) => RenderedPayload::failed(payload, format, &error),
        },
        ValueFormat::Base64 => match codecs::base64_decode(payload) {
            Ok(inner) => render_inner(&inner, format, payload.len()),
            Err(error) => RenderedPayload::failed(payload, format, &error),
        },
        ValueFormat::Gzip
        | ValueFormat::Zstd
        | ValueFormat::Lz4
        | ValueFormat::Deflate
        | ValueFormat::MySqlCompress => {
            let result = match format {
                ValueFormat::Gzip => codecs::gunzip(payload),
                ValueFormat::Zstd => codecs::zstd_decode(payload),
                ValueFormat::Lz4 => codecs::lz4_decode(payload),
                ValueFormat::MySqlCompress => codecs::mysql_compress_decode(payload),
                _ => codecs::deflate_decode(payload),
            };
            match result {
                Ok(inner) => render_inner(&inner, format, payload.len()),
                Err(error) => RenderedPayload::failed(payload, format, &error),
            }
        }
        ValueFormat::Protobuf => match codecs::protobuf_decode(payload) {
            Ok(value) => RenderedPayload::json_value(
                &value,
                format,
                Some(crate::localization::text(
                    "results.structured_data.protobuf_summary_note",
                )),
            ),
            Err(error) => RenderedPayload::failed(payload, format, &error),
        },
        ValueFormat::PhpSerialized => match codecs::php_decode(payload) {
            Ok(value) => RenderedPayload::json_value(&value, format, None),
            Err(error) => RenderedPayload::failed(payload, format, &error),
        },
        ValueFormat::JavaSerialized => match codecs::java_summary(payload) {
            Ok(value) => RenderedPayload::json_value(
                &value,
                format,
                Some(crate::localization::text(
                    "results.structured_data.java_summary_note",
                )),
            ),
            Err(error) => RenderedPayload::failed(payload, format, &error),
        },
        ValueFormat::Pickle => match codecs::pickle_summary(payload) {
            Ok(value) => RenderedPayload::json_value(
                &value,
                format,
                Some(crate::localization::text(
                    "results.structured_data.pickle_summary_note",
                )),
            ),
            Err(error) => RenderedPayload::failed(payload, format, &error),
        },
    }
}

/// What the viewer shows for the bytes a decode produced: the one re-sniff
/// pass. JSON and XML get their trees, a recognised serialization its
/// decode, text its text, anything else the hex dump. The inner bytes are
/// never decompressed or Base64-decoded again — one layer per pick, so a
/// nested payload cannot chain allocations.
fn render_inner(inner: &[u8], outer: ValueFormat, outer_len: usize) -> RenderedPayload {
    let sizes = crate::localization::format(
        "results.structured_data.decoded_byte_sizes",
        &[
            ("outer_bytes", &thousands(outer_len)),
            ("inner_bytes", &thousands(inner.len())),
        ],
    );
    let (mut rendered, inside): (RenderedPayload, String) =
        if let Ok(text) = std::str::from_utf8(inner) {
            match detect(text) {
                Some(StructuredDataKind::Json) => (
                    RenderedPayload {
                        text: pretty_print_json(text.trim()).unwrap_or_else(|| text.to_string()),
                        as_json: true,
                        as_xml: false,
                        as_yaml: false,
                        as_markdown: false,
                        note: None,
                        decoded: Some(ValueFormat::Json),
                    },
                    "JSON".to_string(),
                ),
                Some(StructuredDataKind::Xml) => (
                    RenderedPayload {
                        text: pretty_print(text, StructuredDataKind::Xml),
                        as_json: false,
                        as_xml: true,
                        as_yaml: false,
                        as_markdown: false,
                        note: None,
                        decoded: None,
                    },
                    "XML".to_string(),
                ),
                Some(kind @ (StructuredDataKind::Yaml | StructuredDataKind::Markdown)) => {
                    let format = if kind == StructuredDataKind::Yaml {
                        ValueFormat::Yaml
                    } else {
                        ValueFormat::Markdown
                    };
                    let mut rendered = render_payload(inner, format);
                    rendered.decoded = None;
                    (rendered, format.label().to_string())
                }
                _ => match sniff_serialized(inner) {
                    Some(format) => (render_payload(inner, format), format.localized_label()),
                    None => (
                        RenderedPayload::utf8(inner),
                        crate::localization::text("results.structured_data.inside_text"),
                    ),
                },
            }
        } else {
            match sniff_serialized(inner) {
                Some(format) => (render_payload(inner, format), format.localized_label()),
                None => (
                    RenderedPayload::hex(inner, None),
                    crate::localization::text("results.structured_data.inside_binary"),
                ),
            }
        };
    let prefix = match sniff_compressed(inner) {
        Some(nested) => crate::localization::format(
            "results.structured_data.nested_decode_note",
            &[
                ("outer_format", &outer.localized_label()),
                ("inner_format", &inside),
                ("byte_sizes", &sizes),
                ("nested_format", &nested.localized_label()),
            ],
        ),
        None => crate::localization::format(
            "results.structured_data.decode_note",
            &[
                ("outer_format", &outer.localized_label()),
                ("inner_format", &inside),
                ("byte_sizes", &sizes),
            ],
        ),
    };
    rendered = rendered.prefix_note(&prefix);
    rendered.decoded = Some(outer);
    rendered
}

/// The serializers the inner re-sniff may hand a decoded payload to. They
/// are bounded walkers over the bytes they are given and never decode
/// another layer, so they are safe at the second level.
fn sniff_serialized(bytes: &[u8]) -> Option<ValueFormat> {
    if codecs::looks_like_java(bytes) {
        return Some(ValueFormat::JavaSerialized);
    }
    if codecs::looks_like_pickle(bytes) {
        return Some(ValueFormat::Pickle);
    }
    if codecs::looks_like_php(bytes) {
        return Some(ValueFormat::PhpSerialized);
    }
    if codecs::looks_like_protobuf(bytes) {
        return Some(ValueFormat::Protobuf);
    }
    None
}

/// The compressed container a payload's leading bytes announce.
fn sniff_compressed(bytes: &[u8]) -> Option<ValueFormat> {
    if codecs::looks_like_gzip(bytes) {
        return Some(ValueFormat::Gzip);
    }
    if codecs::looks_like_zstd(bytes) {
        return Some(ValueFormat::Zstd);
    }
    if codecs::looks_like_lz4(bytes) {
        return Some(ValueFormat::Lz4);
    }
    // MySQL COMPRESS() is a length prefix in front of a zlib stream. In the
    // rare case its prefix bytes also pass the zlib header check, the
    // length-validated decode decides which reading is real.
    if codecs::looks_like_mysql_compress(bytes)
        && (!codecs::looks_like_zlib(bytes) || codecs::mysql_compress_decode(bytes).is_ok())
    {
        return Some(ValueFormat::MySqlCompress);
    }
    if codecs::looks_like_zlib(bytes) {
        return Some(ValueFormat::Deflate);
    }
    None
}

/// Auto's sniff for the formats the viewer's own JSON/XML/image/geometry
/// detection does not cover: magic bytes first (gzip, zstd, LZ4, zlib,
/// Java, pickle), then structure (PHP, Base64, Protobuf). Conservative by
/// design — a plain string must keep showing as text — so the structural
/// sniffs require the whole payload to parse and, for Base64, the decoded
/// bytes to be something recognisable.
pub fn sniff_format(payload: &[u8]) -> Option<ValueFormat> {
    if payload.is_empty() {
        return None;
    }
    if let Some(format) = sniff_compressed(payload) {
        return Some(format);
    }
    if let Some(format) = sniff_serialized(payload) {
        return Some(format);
    }
    if codecs::looks_like_base64(payload) {
        return Some(ValueFormat::Base64);
    }
    None
}

/// Auto's second look at a payload the viewer found nothing structured in:
/// sniff the format and render it, `None` when nothing was recognised or
/// the recognised format did not decode cleanly (then the plain view
/// stands). Presentation only: the payload is read, never changed.
pub fn render_auto(payload: &[u8]) -> Option<(ValueFormat, RenderedPayload)> {
    let format = sniff_format(payload)?;
    if !format.available() {
        return None;
    }
    let rendered = render_payload(payload, format);
    (rendered.decoded == Some(format)).then_some((format, rendered))
}

impl StructuredDataKind {
    #[allow(dead_code)]
    /// File extension used when exporting a value of this kind.
    pub fn file_extension(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Xml => "xml",
            Self::Hex => "bin",
            Self::Image => "img",
            Self::Spatial => "geo",
            Self::Yaml => "yaml",
            Self::Markdown => "md",
        }
    }

    /// Short human-readable label, e.g. for window titles.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Json => "JSON",
            Self::Xml => "XML",
            Self::Hex => "Binary",
            Self::Image => "Image",
            Self::Spatial => "Geometry",
            Self::Yaml => "YAML",
            Self::Markdown => "Markdown",
        }
    }

    pub fn localized_display_name(self) -> String {
        match self {
            Self::Json | Self::Xml | Self::Yaml | Self::Markdown => self.display_name().to_string(),
            Self::Hex => crate::localization::text("results.structured_data.binary_title"),
            Self::Image => crate::localization::text("results.structured_data.image_title"),
            Self::Spatial => crate::localization::text("results.structured_data.geometry_title"),
        }
    }
}

/// Maximum bytes rendered in a hex dump; larger values are truncated with a
/// trailing note so a multi-megabyte BLOB cannot build an unbounded string.
pub const HEX_DUMP_MAX_BYTES: usize = 64 * 1024;

/// Canonical hex dump of `bytes`: 16 bytes per line, `OFFSET  HEX…  |ASCII|`,
/// with non-printable bytes shown as `.`. Truncated at [`HEX_DUMP_MAX_BYTES`].
pub fn hex_dump(bytes: &[u8]) -> String {
    let truncated = bytes.len() > HEX_DUMP_MAX_BYTES;
    let shown = &bytes[..bytes.len().min(HEX_DUMP_MAX_BYTES)];
    let mut out = String::with_capacity(shown.len() * 4);
    for (line, chunk) in shown.chunks(16).enumerate() {
        out.push_str(&format!("{:08x}  ", line * 16));
        for (i, byte) in chunk.iter().enumerate() {
            out.push_str(&format!("{byte:02x} "));
            if i == 7 {
                out.push(' ');
            }
        }
        // Pad a short final line so the ASCII gutter stays aligned.
        for i in chunk.len()..16 {
            out.push_str("   ");
            if i == 7 {
                out.push(' ');
            }
        }
        out.push_str(" |");
        for byte in chunk {
            let c = *byte;
            out.push(if (0x20..0x7f).contains(&c) {
                c as char
            } else {
                '.'
            });
        }
        out.push_str("|\n");
    }
    if truncated {
        out.push_str(&crate::localization::format(
            "results.structured_data.hex_truncated_note",
            &[
                (
                    "hidden_bytes",
                    &(bytes.len() - HEX_DUMP_MAX_BYTES).to_string(),
                ),
                ("total_bytes", &bytes.len().to_string()),
            ],
        ));
    }
    out
}

/// Detect whether `text` is JSON, XML, YAML, or Markdown (the last two by
/// cautious structural rules). Returns `None` for anything else.
pub fn detect(text: &str) -> Option<StructuredDataKind> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let first = trimmed.chars().next()?;
    if (first == '{' || first == '[') && is_json(trimmed) {
        return Some(StructuredDataKind::Json);
    }
    if first == '<' && is_xml(trimmed) {
        return Some(StructuredDataKind::Xml);
    }
    if crate::structured_text::is_yaml(trimmed) {
        return Some(StructuredDataKind::Yaml);
    }
    if crate::structured_text::is_markdown(trimmed) {
        return Some(StructuredDataKind::Markdown);
    }
    None
}

/// Raster formats the cell viewer can hand to gpui without a new crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
}

impl ImageFormat {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::Webp => "WebP",
            Self::Bmp => "BMP",
        }
    }
}

const MAX_STRING_SNIFF: usize = 16 * 1024 * 1024;
const MAX_BYTES_SNIFF: usize = 32 * 1024 * 1024;

/// Magic-byte sniff for PNG/JPEG/GIF/WebP/BMP. Conservative: short or
/// unknown buffers return `None` so the hex dump remains the fallback.
pub fn sniff_image(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.len() >= 8 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(ImageFormat::Png);
    }
    if bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Some(ImageFormat::Gif);
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some(ImageFormat::Webp);
    }
    if bytes.len() >= 26 && bytes.starts_with(b"BM") {
        return Some(ImageFormat::Bmp);
    }
    None
}

/// Decode `data:image/<png|jpeg|jpg|gif|webp|bmp>;base64,<payload>`.
pub fn decode_data_url(text: &str) -> Option<(ImageFormat, Vec<u8>)> {
    use base64::Engine as _;

    let t = text.trim();
    let rest = strip_prefix_ci(t, "data:")?.trim_start();
    let rest = strip_prefix_ci(rest, "image/")?.trim_start();
    let mime_end = rest.find(|c: char| c == ';' || c.is_whitespace())?;
    let mime = rest[..mime_end].to_ascii_lowercase();
    let format = match mime.as_str() {
        "png" => ImageFormat::Png,
        "jpeg" | "jpg" => ImageFormat::Jpeg,
        "gif" => ImageFormat::Gif,
        "webp" => ImageFormat::Webp,
        "bmp" => ImageFormat::Bmp,
        _ => return None,
    };
    let after_mime = rest[mime_end..].trim_start();
    let after_semi = after_mime.strip_prefix(';')?.trim_start();
    let after_b64 = strip_prefix_ci(after_semi, "base64")?.trim_start();
    let payload = after_b64.strip_prefix(',')?.trim();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.as_bytes())
        .ok()?;
    Some((format, bytes))
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    // `s.get(..prefix.len())` is boundary-safe: a byte length that lands inside
    // a multi-byte UTF-8 scalar yields `None` instead of panicking the way
    // `s[..prefix.len()]` would (e.g. a cell whose text is `abcd€`).
    let head = s.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

/// Classified cell payload for the structured-data / image / geometry viewer.
#[derive(Debug, Clone, PartialEq)]
pub enum ContentPreview {
    Json,
    Xml,
    Yaml,
    Markdown,
    Image(ImageFormat, Vec<u8>),
    Spatial(Geometry),
    Hex,
    Text,
}

/// Single sniffing entry point. Size-guarded so a huge LOB cannot stall the UI.
pub fn classify_cell(value: &CellValue) -> ContentPreview {
    match value {
        CellValue::Bytes(bytes) => {
            if bytes.len() > MAX_BYTES_SNIFF {
                return ContentPreview::Hex;
            }
            if let Some(fmt) = sniff_image(bytes) {
                return ContentPreview::Image(fmt, bytes.clone());
            }
            if let Ok(geom) = spatial::parse_wkb(bytes) {
                return ContentPreview::Spatial(geom);
            }
            ContentPreview::Hex
        }
        CellValue::String(text) => {
            if text.len() > MAX_STRING_SNIFF {
                return ContentPreview::Text;
            }
            match detect(text) {
                Some(StructuredDataKind::Json) => return ContentPreview::Json,
                Some(StructuredDataKind::Xml) => return ContentPreview::Xml,
                Some(StructuredDataKind::Yaml) => return ContentPreview::Yaml,
                Some(StructuredDataKind::Markdown) => return ContentPreview::Markdown,
                _ => {}
            }
            if let Some((fmt, bytes)) = decode_data_url(text) {
                return ContentPreview::Image(fmt, bytes);
            }
            if spatial::looks_like_wkt(text)
                && let Ok(geom) = spatial::parse_wkt(text)
            {
                return ContentPreview::Spatial(geom);
            }
            if spatial::looks_like_wkb_hex(text)
                && let Ok(geom) = spatial::parse_wkb_hex(text)
            {
                return ContentPreview::Spatial(geom);
            }
            ContentPreview::Text
        }
        _ => ContentPreview::Text,
    }
}

/// True when `trimmed` parses as a JSON object or array.
pub fn is_json(trimmed: &str) -> bool {
    let first = match trimmed.chars().next() {
        Some(c) if c == '{' || c == '[' => c,
        _ => return false,
    };
    // serde_json::from_str requires an object or array (not a bare scalar).
    let _: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return false,
    };
    // Confirm it's an object/array, not a bare string/number/bool/null that
    // happened to start with { or [ (shouldn't happen for valid JSON, but
    // keeps the contract tight).
    match first {
        '{' => trimmed.trim_start().starts_with('{'),
        '[' => trimmed.trim_start().starts_with('['),
        _ => false,
    }
}

/// True when `trimmed` parses as an XML document.
///
/// This is a lightweight well-formedness check: it verifies tag balance,
/// proper nesting, and that the document starts with `<`. It does NOT validate
/// against a DTD or schema. CDATA and processing instructions are tolerated.
pub fn is_xml(trimmed: &str) -> bool {
    if !trimmed.starts_with('<') {
        return false;
    }
    check_xml_well_formed(trimmed)
}

/// Pretty-print `text` as the given kind. Returns the original text if the
/// value cannot be reformatted.
pub fn pretty_print(text: &str, kind: StructuredDataKind) -> String {
    let trimmed = text.trim();
    match kind {
        StructuredDataKind::Json => pretty_print_json(trimmed).unwrap_or_else(|| text.to_string()),
        StructuredDataKind::Xml => pretty_print_xml(trimmed).unwrap_or_else(|| text.to_string()),
        StructuredDataKind::Hex
        | StructuredDataKind::Image
        | StructuredDataKind::Spatial
        | StructuredDataKind::Yaml
        | StructuredDataKind::Markdown => text.to_string(),
    }
}

fn pretty_print_json(trimmed: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    // Sort keys for deterministic output (mirrors Swift's .sortedKeys option).
    let sorted = sort_json_value(&value);
    serde_json::to_string_pretty(&sorted).ok()
}

fn sort_json_value(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => {
            let mut sorted = serde_json::Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                sorted.insert(k.clone(), sort_json_value(&map[k]));
            }
            serde_json::Value::Object(sorted)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(sort_json_value).collect())
        }
        other => other.clone(),
    }
}

fn pretty_print_xml(trimmed: &str) -> Option<String> {
    // Simple indentation-based pretty-printer. Walks tags and text,
    // indenting by nesting depth. Not perfect but sufficient for display.
    if !is_xml(trimmed) {
        return None;
    }
    let mut out = String::with_capacity(trimmed.len() * 2);
    let mut depth: usize = 0;
    let mut in_tag = false;
    let mut tag_buf = String::new();
    let mut after_tag_open = false;

    for ch in trimmed.chars() {
        match ch {
            '<' => {
                if !tag_buf.trim().is_empty() && !in_tag {
                    let trimmed_text = tag_buf.trim();
                    if !trimmed_text.is_empty() {
                        out.push_str(&"  ".repeat(depth));
                        out.push_str(trimmed_text);
                        out.push('\n');
                    }
                }
                tag_buf.clear();
                in_tag = true;
                tag_buf.push(ch);
                after_tag_open = false;
            }
            '>' => {
                tag_buf.push(ch);
                in_tag = false;
                let tag = tag_buf.as_str();
                if tag.starts_with("</") {
                    depth = depth.saturating_sub(1);
                    out.push_str(&"  ".repeat(depth));
                    out.push_str(tag);
                    out.push('\n');
                } else if tag.starts_with("<")
                    && !tag.starts_with("<?")
                    && !tag.starts_with("<!--")
                    && !tag.starts_with("<!")
                {
                    // Opening or self-closing tag
                    if !tag.ends_with("/>") {
                        // Check if it's a self-closing or empty element
                        let is_self_closing = tag.ends_with("/>");
                        out.push_str(&"  ".repeat(depth));
                        out.push_str(tag);
                        out.push('\n');
                        if !is_self_closing {
                            depth += 1;
                            after_tag_open = true;
                        }
                    } else {
                        out.push_str(&"  ".repeat(depth));
                        out.push_str(tag);
                        out.push('\n');
                    }
                } else {
                    out.push_str(&"  ".repeat(depth));
                    out.push_str(tag);
                    out.push('\n');
                }
                tag_buf.clear();
            }
            _ => {
                tag_buf.push(ch);
            }
        }
    }
    let _ = after_tag_open;
    let result = out.trim_end().to_string();
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Lightweight XML well-formedness check: verifies that tags are balanced
/// and properly nested. Tolerates CDATA, comments, and processing instructions.
fn check_xml_well_formed(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    let mut stack: Vec<String> = Vec::new();

    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }

        // Comment
        if text[i..].starts_with("<!--") {
            if let Some(end) = text[i + 4..].find("-->") {
                i += 4 + end + 3;
            } else {
                return false;
            }
            continue;
        }

        // CDATA
        if text[i..].starts_with("<![CDATA[") {
            if let Some(end) = text[i + 9..].find("]]>") {
                i += 9 + end + 3;
            } else {
                return false;
            }
            continue;
        }

        // Processing instruction or DOCTYPE
        if text[i..].starts_with("<?") {
            if let Some(end) = text[i + 2..].find("?>") {
                i += 2 + end + 2;
            } else {
                return false;
            }
            continue;
        }
        if text[i..].starts_with("<!") {
            if let Some(end) = text[i + 2..].find('>') {
                i += 2 + end + 1;
            } else {
                return false;
            }
            continue;
        }

        // Closing tag
        if text[i..].starts_with("</") {
            let after = i + 2;
            let close_end = match text[after..].find('>') {
                Some(p) => after + p,
                None => return false,
            };
            let name = text[after..close_end].trim();
            if name.is_empty() {
                return false;
            }
            match stack.pop() {
                Some(open) if open == name => {}
                _ => return false,
            }
            i = close_end + 1;
            continue;
        }

        // Opening tag
        if text[i..].starts_with('<') {
            let after = i + 1;
            // Find end of tag, respecting quoted attribute values
            let mut j = after;
            let mut in_quote = false;
            let mut quote_char = b'\0';
            while j < bytes.len() {
                let c = bytes[j];
                if in_quote {
                    if c == quote_char {
                        in_quote = false;
                    }
                } else if c == b'"' || c == b'\'' {
                    in_quote = true;
                    quote_char = c;
                } else if c == b'>' {
                    break;
                }
                j += 1;
            }
            if j >= bytes.len() {
                return false;
            }
            let tag_content = &text[after..j];
            // Self-closing?
            let self_closing = tag_content.ends_with('/');
            let name_end = tag_content
                .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .unwrap_or(tag_content.len());
            let name = tag_content[..name_end].trim();
            if name.is_empty() {
                return false;
            }
            if !self_closing {
                stack.push(name.to_string());
            }
            i = j + 1;
            continue;
        }

        i += 1;
    }

    stack.is_empty()
}

// ---------------------------------------------------------------------------
// Tree model
// ---------------------------------------------------------------------------

/// Maximum number of children materialized per node. Nodes with more children
/// get the first `MAX_TREE_CHILDREN` plus a trailing "… N more" marker node.
pub const MAX_TREE_CHILDREN: usize = 2000;

/// A node in the structured-data tree shown by the viewer's Tree mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredNode {
    /// Key / element name / `[index]` for array entries / `@name` for XML
    /// attributes. Truncation marker nodes use a "… N more" label.
    pub label: String,
    /// Scalar value, XML text content, or attribute summary. `None` for
    /// branch nodes.
    pub value_preview: Option<String>,
    /// Dot/bracket path for JSON (e.g. `$.items[2].name`); element path for
    /// XML (e.g. `/root/item[2]/name`, attributes as `/root/@id`).
    pub path: String,
    pub children: Vec<StructuredNode>,
}

impl StructuredNode {
    /// True for nodes that can expand/collapse.
    pub fn is_branch(&self) -> bool {
        !self.children.is_empty()
    }

    /// True for the synthetic "… N more" child-cap marker.
    pub fn is_truncation_marker(&self) -> bool {
        self.children.is_empty() && self.value_preview.is_none() && self.label.starts_with('…')
    }
}

/// Build the tree for `text` of the given kind. Returns `None` when the text
/// does not parse, in which case the viewer keeps the raw pretty-text path.
pub fn build_tree(text: &str, kind: StructuredDataKind) -> Option<StructuredNode> {
    let trimmed = text.trim();
    match kind {
        StructuredDataKind::Json => build_json_tree(trimmed),
        StructuredDataKind::Xml => build_xml_tree(trimmed),
        StructuredDataKind::Yaml => match crate::structured_text::parse_yaml(trimmed)? {
            value @ (serde_json::Value::Object(_) | serde_json::Value::Array(_)) => {
                Some(json_node("$".to_string(), "$".to_string(), &value))
            }
            _ => None,
        },
        StructuredDataKind::Hex
        | StructuredDataKind::Image
        | StructuredDataKind::Spatial
        | StructuredDataKind::Markdown => None,
    }
}

/// Truncate a preview string to a display-friendly length (char safe).
fn truncate_preview(s: &str) -> String {
    const MAX: usize = 120;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        let cut: String = s.chars().take(MAX).collect();
        format!("{cut}…")
    }
}

/// Apply the per-node child cap: keep the first `MAX_TREE_CHILDREN` produced
/// children and append a marker node when `total` exceeds the cap.
fn cap_children(
    parent_path: &str,
    total: usize,
    mut children: Vec<StructuredNode>,
) -> Vec<StructuredNode> {
    if total > MAX_TREE_CHILDREN {
        children.push(StructuredNode {
            label: crate::localization::format(
                "results.structured_data.tree_more_children",
                &[("hidden_count", &(total - MAX_TREE_CHILDREN).to_string())],
            ),
            value_preview: None,
            path: parent_path.to_string(),
            children: Vec::new(),
        });
    }
    children
}

// --- JSON tree ---

fn build_json_tree(trimmed: &str) -> Option<StructuredNode> {
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    Some(json_node("$".to_string(), "$".to_string(), &value))
}

/// True when `key` can be appended with dot notation (`$.key`); anything else
/// uses bracket-quoted form (`$["odd key"]`).
fn is_plain_json_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn json_node(label: String, path: String, v: &serde_json::Value) -> StructuredNode {
    match v {
        serde_json::Value::Object(map) => {
            let children: Vec<StructuredNode> = map
                .iter()
                .take(MAX_TREE_CHILDREN)
                .map(|(k, child)| {
                    let child_path = if is_plain_json_key(k) {
                        format!("{path}.{k}")
                    } else {
                        format!("{path}[{:?}]", k)
                    };
                    json_node(k.clone(), child_path, child)
                })
                .collect();
            StructuredNode {
                label,
                value_preview: None,
                path: path.clone(),
                children: cap_children(&path, map.len(), children),
            }
        }
        serde_json::Value::Array(arr) => {
            let children: Vec<StructuredNode> = arr
                .iter()
                .enumerate()
                .take(MAX_TREE_CHILDREN)
                .map(|(i, child)| json_node(format!("[{i}]"), format!("{path}[{i}]"), child))
                .collect();
            StructuredNode {
                label,
                value_preview: None,
                path: path.clone(),
                children: cap_children(&path, arr.len(), children),
            }
        }
        scalar => StructuredNode {
            label,
            value_preview: Some(truncate_preview(&scalar.to_string())),
            path,
            children: Vec::new(),
        },
    }
}

// --- XML tree ---

/// Intermediate element produced by the tolerant XML parser.
struct XmlElement {
    name: String,
    attributes: Vec<(String, String)>,
    text: String,
    children: Vec<XmlElement>,
}

fn build_xml_tree(trimmed: &str) -> Option<StructuredNode> {
    let roots = parse_xml_elements(trimmed)?;
    match roots.len() {
        0 => None,
        1 => {
            let root = &roots[0];
            Some(xml_node(root, &format!("/{}", root.name)))
        }
        _ => {
            // Multiple top-level elements (fragment): wrap in a synthetic
            // document node so every element keeps a stable path.
            let children: Vec<StructuredNode> = index_xml_children(&roots, "");
            let total = roots.len();
            Some(StructuredNode {
                label: "#document".to_string(),
                value_preview: None,
                path: "/".to_string(),
                children: cap_children("/", total, children),
            })
        }
    }
}

/// Build child element nodes with XPath-style sibling indices: `item[2]` when
/// several siblings share a name, plain `item` when the name is unique.
fn index_xml_children(children: &[XmlElement], parent_path: &str) -> Vec<StructuredNode> {
    use std::collections::HashMap;
    let mut name_totals: HashMap<&str, usize> = HashMap::new();
    for c in children {
        *name_totals.entry(c.name.as_str()).or_insert(0) += 1;
    }
    let mut seen: HashMap<&str, usize> = HashMap::new();
    children
        .iter()
        .take(MAX_TREE_CHILDREN)
        .map(|c| {
            let ord = seen.entry(c.name.as_str()).or_insert(0);
            *ord += 1;
            let path = if name_totals[c.name.as_str()] > 1 {
                format!("{parent_path}/{}[{}]", c.name, *ord)
            } else {
                format!("{parent_path}/{}", c.name)
            };
            xml_node(c, &path)
        })
        .collect()
}

fn xml_node(el: &XmlElement, path: &str) -> StructuredNode {
    let mut children: Vec<StructuredNode> = el
        .attributes
        .iter()
        .map(|(name, value)| StructuredNode {
            label: format!("@{name}"),
            value_preview: Some(truncate_preview(value)),
            path: format!("{path}/@{name}"),
            children: Vec::new(),
        })
        .collect();

    let text = el.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let element_children = index_xml_children(&el.children, path);
    let total = children.len() + el.children.len();
    children.extend(element_children);

    // Text-only leaves surface their text as the element's value preview;
    // mixed content keeps the text as an explicit `#text` child.
    let mut value_preview = None;
    if !text.is_empty() {
        if el.children.is_empty() {
            value_preview = Some(truncate_preview(&text));
        } else {
            children.push(StructuredNode {
                label: "#text".to_string(),
                value_preview: Some(truncate_preview(&text)),
                path: format!("{path}/#text"),
                children: Vec::new(),
            });
        }
    }
    // Elements with no text but with attributes summarize the attributes.
    if value_preview.is_none()
        && !el.attributes.is_empty()
        && el.children.is_empty()
        && text.is_empty()
    {
        let summary = el
            .attributes
            .iter()
            .map(|(k, v)| format!("{k}=\"{v}\""))
            .collect::<Vec<_>>()
            .join(" ");
        value_preview = Some(truncate_preview(&summary));
    }

    StructuredNode {
        label: el.name.clone(),
        value_preview,
        path: path.to_string(),
        children: cap_children(path, total, children),
    }
}

/// Deepest XML nesting the tree viewer accepts. The tree builder recurses
/// per nesting level, so deeper documents are refused here and the viewer
/// falls back to the (iterative) pretty-text path instead of overflowing
/// the stack.
const MAX_XML_TREE_DEPTH: usize = 256;

/// Small tolerant XML parser: elements, attributes, and text. Comments,
/// processing instructions, and DOCTYPE are skipped; CDATA becomes text.
/// Returns `None` on structural failure (mismatched/unclosed tags) and on
/// documents nested deeper than `MAX_XML_TREE_DEPTH`.
fn parse_xml_elements(text: &str) -> Option<Vec<XmlElement>> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut roots: Vec<XmlElement> = Vec::new();
    let mut stack: Vec<XmlElement> = Vec::new();

    fn attach(el: XmlElement, stack: &mut [XmlElement], roots: &mut Vec<XmlElement>) {
        if let Some(parent) = stack.last_mut() {
            parent.children.push(el);
        } else {
            roots.push(el);
        }
    }

    while i < bytes.len() {
        if bytes[i] != b'<' {
            // Text run up to the next tag.
            let end = text[i..].find('<').map(|p| i + p).unwrap_or(bytes.len());
            if let Some(current) = stack.last_mut() {
                current.text.push_str(&text[i..end]);
            }
            i = end;
            continue;
        }

        if text[i..].starts_with("<!--") {
            i += 4 + text[i + 4..].find("-->")? + 3;
        } else if text[i..].starts_with("<![CDATA[") {
            let end = text[i + 9..].find("]]>")?;
            if let Some(current) = stack.last_mut() {
                current.text.push_str(&text[i + 9..i + 9 + end]);
            }
            i += 9 + end + 3;
        } else if text[i..].starts_with("<?") {
            i += 2 + text[i + 2..].find("?>")? + 2;
        } else if text[i..].starts_with("<!") {
            i += 2 + text[i + 2..].find('>')? + 1;
        } else if text[i..].starts_with("</") {
            let after = i + 2;
            let close_end = after + text[after..].find('>')?;
            let name = text[after..close_end].trim();
            let open = stack.pop()?;
            if open.name != name {
                return None;
            }
            attach(open, &mut stack, &mut roots);
            i = close_end + 1;
        } else {
            // Opening or self-closing tag. Find its end, respecting quotes.
            let after = i + 1;
            let mut j = after;
            let mut in_quote = false;
            let mut quote_char = b'\0';
            while j < bytes.len() {
                let c = bytes[j];
                if in_quote {
                    if c == quote_char {
                        in_quote = false;
                    }
                } else if c == b'"' || c == b'\'' {
                    in_quote = true;
                    quote_char = c;
                } else if c == b'>' {
                    break;
                }
                j += 1;
            }
            if j >= bytes.len() {
                return None;
            }
            let tag_content = &text[after..j];
            let self_closing = tag_content.ends_with('/');
            let body = tag_content.trim_end_matches('/');
            let name_end = body.find(|c: char| c.is_whitespace()).unwrap_or(body.len());
            let name = body[..name_end].trim();
            if name.is_empty() {
                return None;
            }
            let attributes = parse_xml_attributes(&body[name_end..]);
            let el = XmlElement {
                name: name.to_string(),
                attributes,
                text: String::new(),
                children: Vec::new(),
            };
            if self_closing {
                attach(el, &mut stack, &mut roots);
            } else {
                if stack.len() >= MAX_XML_TREE_DEPTH {
                    return None;
                }
                stack.push(el);
            }
            i = j + 1;
        }
    }

    if !stack.is_empty() || roots.is_empty() {
        return None;
    }
    Some(roots)
}

/// Parse `name="value"` pairs from the attribute portion of a tag. Tolerant:
/// bare names get an empty value; an unterminated quote takes the rest.
fn parse_xml_attributes(s: &str) -> Vec<(String, String)> {
    let mut attrs = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            i += 1;
            continue;
        }
        // Attribute name.
        let name_start = i;
        while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '=' {
            i += 1;
        }
        let name: String = chars[name_start..i].iter().collect();
        if name.is_empty() {
            i += 1;
            continue;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && chars[i] == '=' {
            i += 1;
            while i < chars.len() && chars[i].is_whitespace() {
                i += 1;
            }
            if i < chars.len() && (chars[i] == '"' || chars[i] == '\'') {
                let quote = chars[i];
                i += 1;
                let val_start = i;
                while i < chars.len() && chars[i] != quote {
                    i += 1;
                }
                let value: String = chars[val_start..i].iter().collect();
                if i < chars.len() {
                    i += 1; // closing quote
                }
                attrs.push((name, value));
            } else {
                // Unquoted value: read to whitespace.
                let val_start = i;
                while i < chars.len() && !chars[i].is_whitespace() {
                    i += 1;
                }
                let value: String = chars[val_start..i].iter().collect();
                attrs.push((name, value));
            }
        } else {
            attrs.push((name, String::new()));
        }
    }
    attrs
}

// --- Flatten & search ---

/// A visible row produced by [`flatten_visible`] or [`search_rows`].
#[derive(Debug, Clone, Copy)]
pub struct FlatRow<'a> {
    pub depth: usize,
    pub node: &'a StructuredNode,
}

/// Depth-first visible rows given a set of collapsed node paths. Children of
/// a collapsed branch are not visited (and therefore no rows are built for
/// them).
pub fn flatten_visible<'a>(
    root: &'a StructuredNode,
    collapsed: &std::collections::HashSet<String>,
) -> Vec<FlatRow<'a>> {
    let mut out = Vec::new();
    flatten_rec(root, 0, collapsed, &mut out);
    out
}

fn flatten_rec<'a>(
    node: &'a StructuredNode,
    depth: usize,
    collapsed: &std::collections::HashSet<String>,
    out: &mut Vec<FlatRow<'a>>,
) {
    out.push(FlatRow { depth, node });
    if node.is_branch() && collapsed.contains(&node.path) {
        return;
    }
    for child in &node.children {
        flatten_rec(child, depth + 1, collapsed, out);
    }
}

/// Rows whose label, value preview, or path contains `query`
/// (case-insensitive), with all ancestors of a match included so matches stay
/// in context. An empty query returns the fully expanded tree.
pub fn search_rows<'a>(root: &'a StructuredNode, query: &str) -> Vec<FlatRow<'a>> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return flatten_visible(root, &std::collections::HashSet::new());
    }
    let mut out = Vec::new();
    search_rec(root, 0, &q, &mut out);
    out
}

/// Returns true when this subtree contains a match. Rows are pushed
/// optimistically and truncated when neither the node nor a descendant
/// matched.
fn search_rec<'a>(
    node: &'a StructuredNode,
    depth: usize,
    q: &str,
    out: &mut Vec<FlatRow<'a>>,
) -> bool {
    let self_match = node.label.to_lowercase().contains(q)
        || node.path.to_lowercase().contains(q)
        || node
            .value_preview
            .as_ref()
            .is_some_and(|v| v.to_lowercase().contains(q));
    let mark = out.len();
    out.push(FlatRow { depth, node });
    let mut child_match = false;
    for child in &node.children {
        child_match |= search_rec(child, depth + 1, q, out);
    }
    if self_match || child_match {
        true
    } else {
        out.truncate(mark);
        false
    }
}

// ---------------------------------------------------------------------------
// Syntax classification
// ---------------------------------------------------------------------------

/// A classified token kind. Color is assigned by the view layer. Ranges not
/// covered by any token render with the default text color.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyntaxToken {
    Key,
    String,
    Number,
    Boolean,
    Null,
    Punctuation,
    ElementName,
    AttributeName,
    AttributeValue,
    Comment,
    #[allow(dead_code)]
    Text,
}

/// A classified span: byte range + token kind. Ranges are in *application
/// order* (later entries win on overlap), mirroring the Swift implementation.
#[derive(Clone)]
pub struct SyntaxSpan {
    pub start: usize,
    pub end: usize,
    pub token: SyntaxToken,
}

/// Classify `text` of the given kind into colored spans in application order.
pub fn classify(text: &str, kind: StructuredDataKind) -> Vec<SyntaxSpan> {
    match kind {
        StructuredDataKind::Json => classify_json(text),
        StructuredDataKind::Xml => classify_xml(text),
        StructuredDataKind::Yaml => crate::structured_text::classify_yaml(text),
        StructuredDataKind::Markdown => crate::structured_text::classify_markdown(text),
        StructuredDataKind::Hex | StructuredDataKind::Image | StructuredDataKind::Spatial => {
            Vec::new()
        }
    }
}

// --- JSON classification ---

fn json_punctuation_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\{\}\[\],:]").unwrap())
}
fn json_number_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"-?\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?\b").unwrap())
}
fn json_boolean_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(?:true|false)\b").unwrap())
}
fn json_null_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bnull\b").unwrap())
}
fn json_string_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""(?:[^"\\]|\\.)*""#).unwrap())
}
fn json_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""(?:[^"\\]|\\.)*""#).unwrap())
}

fn classify_json(text: &str) -> Vec<SyntaxSpan> {
    let mut spans = Vec::new();
    scan(
        json_punctuation_re(),
        text,
        SyntaxToken::Punctuation,
        &mut spans,
    );
    scan(json_number_re(), text, SyntaxToken::Number, &mut spans);
    scan(json_boolean_re(), text, SyntaxToken::Boolean, &mut spans);
    scan(json_null_re(), text, SyntaxToken::Null, &mut spans);
    scan(json_string_re(), text, SyntaxToken::String, &mut spans);
    // Keys: strings immediately followed by optional whitespace then `:`.
    // Rust regex lacks look-ahead, so we re-scan strings and check the char
    // after each match.
    for m in json_key_re().find_iter(text) {
        let after = &text[m.end()..];
        if after.trim_start().starts_with(':') {
            spans.push(SyntaxSpan {
                start: m.start(),
                end: m.end(),
                token: SyntaxToken::Key,
            });
        }
    }
    spans
}

// --- XML classification ---

fn xml_punctuation_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[<>/=]").unwrap())
}
fn xml_attr_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([A-Za-z_][\w.:-]*)\s*=").unwrap())
}
fn xml_element_name_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"</?\s*([A-Za-z_][\w.:-]*)").unwrap())
}
fn xml_attr_value_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#""[^"]*"|'[^']*'"#).unwrap())
}
fn xml_comment_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<!--[\s\S]*?-->").unwrap())
}
fn xml_processing_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<\?[\s\S]*?\?>|<!DOCTYPE[^>]*>").unwrap())
}

fn classify_xml(text: &str) -> Vec<SyntaxSpan> {
    let mut spans = Vec::new();
    scan(
        xml_punctuation_re(),
        text,
        SyntaxToken::Punctuation,
        &mut spans,
    );
    scan_capture(
        xml_attr_name_re(),
        text,
        SyntaxToken::AttributeName,
        1,
        &mut spans,
    );
    scan_capture(
        xml_element_name_re(),
        text,
        SyntaxToken::ElementName,
        1,
        &mut spans,
    );
    scan(
        xml_attr_value_re(),
        text,
        SyntaxToken::AttributeValue,
        &mut spans,
    );
    scan(xml_comment_re(), text, SyntaxToken::Comment, &mut spans);
    scan(xml_processing_re(), text, SyntaxToken::Comment, &mut spans);
    spans
}

fn scan(re: &Regex, text: &str, token: SyntaxToken, spans: &mut Vec<SyntaxSpan>) {
    for m in re.find_iter(text) {
        spans.push(SyntaxSpan {
            start: m.start(),
            end: m.end(),
            token,
        });
    }
}

fn scan_capture(
    re: &Regex,
    text: &str,
    token: SyntaxToken,
    group: usize,
    spans: &mut Vec<SyntaxSpan>,
) {
    for caps in re.captures_iter(text) {
        if let Some(m) = caps.get(group) {
            spans.push(SyntaxSpan {
                start: m.start(),
                end: m.end(),
                token,
            });
        }
    }
}

/// Decoders behind the value viewer's formats. Every input is untrusted:
/// a length in the data is a claim, not a fact. Decompressors stop at
/// [`codecs::MAX_DECODED_BYTES`] (and refuse up front when a header
/// already claims more), structural walkers stop at
/// [`codecs::MAX_NESTING`], every slice read is bounded by the bytes
/// present, and a payload that does not fit its format is an `Err` the
/// viewer shows beside the hex dump — never a partial or guessed decode.
/// Nothing here writes to the payload: formats are presentation only.
pub mod codecs {
    use std::io::Read as _;

    use serde_json::{Value, json};

    /// Most bytes any decompressor or decoder may produce.
    pub const MAX_DECODED_BYTES: usize = 16 * 1024 * 1024;

    /// Deepest container nesting a structural walker follows.
    pub const MAX_NESTING: usize = 128;

    /// Base64 sniffing ignores anything shorter than this (after
    /// whitespace): short tokens are too often valid Base64 by accident.
    pub const BASE64_SNIFF_MIN_CHARS: usize = 16;

    /// Strings and class names a Java or pickle summary lists at most.
    const SUMMARY_SAMPLE: usize = 32;

    /// Characters kept of each sampled string.
    const SUMMARY_STRING_CHARS: usize = 80;

    fn error(key: &'static str, arguments: &[(&str, &str)]) -> String {
        crate::localization::format(key, arguments)
    }

    fn error_text(key: &'static str) -> String {
        crate::localization::text(key)
    }

    fn cap_text() -> String {
        error(
            "results.structured_data.decoded_size_cap",
            &[(
                "megabytes",
                &(MAX_DECODED_BYTES / (1024 * 1024)).to_string(),
            )],
        )
    }

    /// Drain a decoder into memory, refusing past the cap instead of
    /// growing with it.
    fn read_capped<R: std::io::Read>(reader: R, what: &str) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        let mut limited = reader.take(MAX_DECODED_BYTES as u64 + 1);
        limited.read_to_end(&mut out).map_err(|source| {
            error(
                "results.structured_data.decoder_read_failed",
                &[("decoder", what), ("error", &source.to_string())],
            )
        })?;
        if out.len() > MAX_DECODED_BYTES {
            return Err(error(
                "results.structured_data.decoder_expands_past_cap",
                &[("decoder", what), ("size_cap", &cap_text())],
            ));
        }
        Ok(out)
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Text a person could read: UTF-8 with no control characters beyond
    /// newlines and tabs.
    pub(super) fn printable_text(text: &str) -> bool {
        !text.is_empty()
            && text
                .chars()
                .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
    }

    // ----- gzip ---------------------------------------------------------

    /// `1f 8b` plus the deflate method byte and room for the trailer.
    pub fn looks_like_gzip(bytes: &[u8]) -> bool {
        bytes.len() >= 18 && bytes[0] == 0x1f && bytes[1] == 0x8b && bytes[2] == 8
    }

    /// Decompress one or more gzip members. The trailer's ISIZE (the
    /// uncompressed size modulo 2^32) is checked first: a "bomb" that
    /// announces more than the cap is refused before any inflating.
    pub fn gunzip(bytes: &[u8]) -> Result<Vec<u8>, String> {
        if bytes.len() < 18 || bytes[0] != 0x1f || bytes[1] != 0x8b {
            return Err(error_text("results.structured_data.gzip_missing_header"));
        }
        if bytes[2] != 8 {
            return Err(error(
                "results.structured_data.gzip_unknown_method",
                &[("method", &bytes[2].to_string())],
            ));
        }
        let claimed = u32::from_le_bytes([
            bytes[bytes.len() - 4],
            bytes[bytes.len() - 3],
            bytes[bytes.len() - 2],
            bytes[bytes.len() - 1],
        ]) as usize;
        if claimed > MAX_DECODED_BYTES {
            return Err(error(
                "results.structured_data.gzip_claimed_size_exceeds_cap",
                &[
                    ("claimed_bytes", &super::thousands(claimed)),
                    ("size_cap", &cap_text()),
                ],
            ));
        }
        read_capped(flate2::read::MultiGzDecoder::new(bytes), "gzip")
    }

    // ----- zstd ---------------------------------------------------------

    const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];

    pub fn looks_like_zstd(bytes: &[u8]) -> bool {
        bytes.len() >= 8 && bytes.starts_with(&ZSTD_MAGIC)
    }

    /// Decompress a zstd frame. A frame header that declares a content
    /// size past the cap is refused before decoding; a truncated frame is
    /// reported by the decoder.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn zstd_decode(bytes: &[u8]) -> Result<Vec<u8>, String> {
        if !bytes.starts_with(&ZSTD_MAGIC) {
            return Err(error_text("results.structured_data.zstd_missing_header"));
        }
        match zstd::zstd_safe::get_frame_content_size(bytes) {
            Ok(Some(size)) if size > MAX_DECODED_BYTES as u64 => {
                return Err(error(
                    "results.structured_data.zstd_claimed_size_exceeds_cap",
                    &[
                        (
                            "claimed_bytes",
                            &super::thousands(size.min(usize::MAX as u64) as usize),
                        ),
                        ("size_cap", &cap_text()),
                    ],
                ));
            }
            Ok(_) => {}
            Err(_) => {
                return Err(error_text("results.structured_data.zstd_invalid_header"));
            }
        }
        let decoder = zstd::stream::read::Decoder::new(bytes).map_err(|source| {
            error(
                "results.structured_data.zstd_open_failed",
                &[("error", &source.to_string())],
            )
        })?;
        read_capped(decoder, "zstd")
    }

    /// The browser build has no zstd: the C library behind it is not
    /// compiled for wasm.
    #[cfg(target_arch = "wasm32")]
    pub fn zstd_decode(_bytes: &[u8]) -> Result<Vec<u8>, String> {
        Err(error_text(
            "results.structured_data.zstd_browser_unavailable",
        ))
    }

    // ----- LZ4 ----------------------------------------------------------

    const LZ4_FRAME_MAGIC: [u8; 4] = [0x04, 0x22, 0x4d, 0x18];

    pub fn looks_like_lz4(bytes: &[u8]) -> bool {
        bytes.len() >= 7 && bytes.starts_with(&LZ4_FRAME_MAGIC)
    }

    /// Decompress an LZ4 frame, or — without the frame magic — a block with
    /// its original size prepended (the common raw layout). A block whose
    /// size prefix exceeds the cap is refused before allocating.
    pub fn lz4_decode(bytes: &[u8]) -> Result<Vec<u8>, String> {
        if bytes.starts_with(&LZ4_FRAME_MAGIC) {
            return read_capped(lz4_flex::frame::FrameDecoder::new(bytes), "LZ4");
        }
        if bytes.len() < 4 {
            return Err(error_text("results.structured_data.lz4_missing_header"));
        }
        let claimed = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if claimed > MAX_DECODED_BYTES {
            return Err(error(
                "results.structured_data.lz4_claimed_size_exceeds_cap",
                &[
                    ("claimed_bytes", &super::thousands(claimed)),
                    ("size_cap", &cap_text()),
                ],
            ));
        }
        lz4_flex::block::decompress_size_prepended(bytes).map_err(|source| {
            error(
                "results.structured_data.lz4_decode_failed",
                &[("error", &source.to_string())],
            )
        })
    }

    // ----- deflate ------------------------------------------------------

    /// A zlib (RFC 1950) header: deflate method, a window size of at most
    /// 32 KB, and the header check passes (`78 01`, `78 9c`, `78 da`, …).
    pub fn looks_like_zlib(bytes: &[u8]) -> bool {
        bytes.len() >= 6
            && bytes[0] & 0x0f == 8
            && bytes[0] >> 4 <= 7
            && ((u16::from(bytes[0]) << 8) | u16::from(bytes[1])).is_multiple_of(31)
    }

    /// Inflate a zlib stream when the header says so, a raw deflate stream
    /// otherwise.
    pub fn deflate_decode(bytes: &[u8]) -> Result<Vec<u8>, String> {
        if bytes.is_empty() {
            return Err(error_text("results.structured_data.deflate_empty"));
        }
        if looks_like_zlib(bytes) {
            return read_capped(flate2::read::ZlibDecoder::new(bytes), "zlib");
        }
        read_capped(flate2::read::DeflateDecoder::new(bytes), "raw deflate")
    }

    // ----- MySQL COMPRESS() ---------------------------------------------

    const MYSQL_COMPRESS: &str = "MySQL COMPRESS";

    /// The uncompressed length MySQL's `COMPRESS()` writes ahead of the zlib
    /// stream.
    fn mysql_compress_claimed(bytes: &[u8]) -> Option<usize> {
        let prefix: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
        Some(u32::from_le_bytes(prefix) as usize)
    }

    /// `COMPRESS()` output: a non-zero 4-byte little-endian length within the
    /// cap, then a zlib header.
    pub fn looks_like_mysql_compress(bytes: &[u8]) -> bool {
        mysql_compress_claimed(bytes).is_some_and(|claimed| {
            claimed > 0 && claimed <= MAX_DECODED_BYTES && looks_like_zlib(&bytes[4..])
        })
    }

    /// Undo MySQL's `COMPRESS()`: empty stays empty; otherwise the 4-byte
    /// little-endian length prefix is checked against the cap before
    /// inflating and against the inflated size after. MySQL appends a `.`
    /// when the stream ends in a space (so `CHAR` trimming cannot eat it);
    /// that byte is the only trailing data accepted.
    pub fn mysql_compress_decode(bytes: &[u8]) -> Result<Vec<u8>, String> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let fail = |reason: String| {
            error(
                "results.structured_data.decoder_read_failed",
                &[("decoder", MYSQL_COMPRESS), ("error", &reason)],
            )
        };
        let Some(claimed) = mysql_compress_claimed(bytes) else {
            return Err(fail("shorter than the 4-byte length prefix".to_string()));
        };
        if claimed > MAX_DECODED_BYTES {
            return Err(error(
                "results.structured_data.decoder_expands_past_cap",
                &[("decoder", MYSQL_COMPRESS), ("size_cap", &cap_text())],
            ));
        }
        let stream = &bytes[4..];
        if claimed == 0 || !looks_like_zlib(stream) {
            return Err(fail(
                "no zlib stream after a non-zero length prefix".to_string(),
            ));
        }
        let mut decoder = flate2::bufread::ZlibDecoder::new(stream);
        let out = read_capped(&mut decoder, MYSQL_COMPRESS)?;
        let rest = decoder.into_inner();
        if !(rest.is_empty() || rest == b".") {
            return Err(fail(format!(
                "{} unexpected bytes after the zlib stream",
                rest.len()
            )));
        }
        if out.len() != claimed {
            return Err(fail(format!(
                "the length prefix claims {} bytes but the stream holds {}",
                super::thousands(claimed),
                super::thousands(out.len())
            )));
        }
        Ok(out)
    }

    // ----- Base64 -------------------------------------------------------

    /// Decode standard or URL-safe Base64, padded or not, ignoring ASCII
    /// whitespace (wrapped values).
    pub fn base64_decode(bytes: &[u8]) -> Result<Vec<u8>, String> {
        use base64::Engine as _;
        use base64::engine::general_purpose::{
            STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
        };

        let text = std::str::from_utf8(bytes)
            .map_err(|_| error_text("results.structured_data.base64_not_ascii"))?;
        let compact: Vec<u8> = text
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        if compact.is_empty() {
            return Err(error_text("results.structured_data.base64_empty"));
        }
        for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
            if let Ok(decoded) = engine.decode(&compact) {
                return Ok(decoded);
            }
        }
        Err(error_text("results.structured_data.base64_invalid"))
    }

    /// Base64 by heuristic: at least [`BASE64_SNIFF_MIN_CHARS`] characters
    /// of one alphabet, padding only at the end, and either padding, a
    /// symbol character, or all three of digits/lower/upper present (so a
    /// plain word, a hex string, or `user:42` never qualifies) — and the
    /// decoded bytes must be something recognisable (text, JSON, XML, or a
    /// format this module sniffs).
    pub fn looks_like_base64(bytes: &[u8]) -> bool {
        let Ok(text) = std::str::from_utf8(bytes) else {
            return false;
        };
        let compact: Vec<u8> = text
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        if compact.len() < BASE64_SNIFF_MIN_CHARS {
            return false;
        }
        let pad = compact
            .iter()
            .rev()
            .take_while(|byte| **byte == b'=')
            .count();
        if pad > 2 || (pad > 0 && !compact.len().is_multiple_of(4)) {
            return false;
        }
        let body = &compact[..compact.len() - pad];
        if body.len() % 4 == 1 {
            return false;
        }
        let (mut digit, mut lower, mut upper, mut standard, mut url_safe) =
            (false, false, false, false, false);
        for byte in body {
            match byte {
                b'0'..=b'9' => digit = true,
                b'a'..=b'z' => lower = true,
                b'A'..=b'Z' => upper = true,
                b'+' | b'/' => standard = true,
                b'-' | b'_' => url_safe = true,
                _ => return false,
            }
        }
        if standard && url_safe {
            return false;
        }
        if !(pad > 0 || standard || url_safe || (digit && lower && upper)) {
            return false;
        }
        let Ok(decoded) = base64_decode(bytes) else {
            return false;
        };
        inner_recognisable(&decoded)
    }

    /// Whether decoded bytes are something the viewer can show as more
    /// than a hex dump — the Base64 sniff's guard against random text.
    fn inner_recognisable(decoded: &[u8]) -> bool {
        if decoded.is_empty() {
            return false;
        }
        if super::sniff_compressed(decoded).is_some() || super::sniff_serialized(decoded).is_some()
        {
            return true;
        }
        match std::str::from_utf8(decoded) {
            Ok(text) => super::detect(text).is_some() || printable_text(text),
            Err(_) => false,
        }
    }

    // ----- Protobuf -----------------------------------------------------

    /// A protobuf message by structure: not readable text, and the wire
    /// format walks cleanly to the last byte with at least one field.
    pub fn looks_like_protobuf(bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }
        if let Ok(text) = std::str::from_utf8(bytes)
            && printable_text(text)
        {
            return false;
        }
        walk_message(bytes, 0).is_ok_and(|fields| !fields.is_empty())
    }

    /// Dump a protobuf message without its schema: `{"1": 150, "2":
    /// "testing", "3": {…}}`. Repeated field numbers become arrays,
    /// length-delimited fields show as text when they read as text, as a
    /// nested message when they walk as one, and as `{"$bytes": "<hex>"}`
    /// otherwise; fixed-width fields show both integer and float readings.
    pub fn protobuf_decode(bytes: &[u8]) -> Result<Value, String> {
        if bytes.is_empty() {
            return Err(error_text("results.structured_data.protobuf_empty"));
        }
        let fields = walk_message(bytes, 0)?;
        if fields.is_empty() {
            return Err(error_text("results.structured_data.protobuf_no_fields"));
        }
        Ok(fields_to_json(fields))
    }

    fn read_varint(bytes: &[u8], at: usize) -> Result<(u64, usize), String> {
        let mut value = 0u64;
        for (i, byte) in bytes[at..].iter().take(10).enumerate() {
            value |= u64::from(byte & 0x7f) << (7 * i);
            if byte & 0x80 == 0 {
                if i == 9 && *byte > 1 {
                    return Err(error(
                        "results.structured_data.protobuf_varint_overflow",
                        &[("byte_offset", &at.to_string())],
                    ));
                }
                return Ok((value, i + 1));
            }
        }
        Err(error(
            "results.structured_data.protobuf_varint_truncated",
            &[("byte_offset", &at.to_string())],
        ))
    }

    fn walk_message(bytes: &[u8], depth: usize) -> Result<Vec<(u32, Value)>, String> {
        if depth > MAX_NESTING {
            return Err(error(
                "results.structured_data.protobuf_nesting_exceeded",
                &[("maximum_depth", &MAX_NESTING.to_string())],
            ));
        }
        let mut pos = 0;
        let mut fields = Vec::new();
        while pos < bytes.len() {
            let (key, used) = read_varint(bytes, pos)?;
            let tag_at = pos;
            pos += used;
            let field = key >> 3;
            let wire = key & 7;
            if field == 0 || field > 0x1fff_ffff {
                return Err(error(
                    "results.structured_data.protobuf_invalid_field",
                    &[
                        ("field_number", &field.to_string()),
                        ("byte_offset", &tag_at.to_string()),
                    ],
                ));
            }
            let value = match wire {
                0 => {
                    let (value, used) = read_varint(bytes, pos)?;
                    pos += used;
                    Value::from(value)
                }
                1 => {
                    let Some(slice) = bytes.get(pos..pos + 8) else {
                        return Err(error(
                            "results.structured_data.protobuf_fixed64_truncated",
                            &[("byte_offset", &pos.to_string())],
                        ));
                    };
                    pos += 8;
                    let raw = u64::from_le_bytes(slice.try_into().expect("8 bytes"));
                    let double = f64::from_bits(raw);
                    json!({
                        "$fixed64": raw,
                        "$double": if double.is_finite() { Value::from(double) } else { Value::from(double.to_string()) },
                    })
                }
                5 => {
                    let Some(slice) = bytes.get(pos..pos + 4) else {
                        return Err(error(
                            "results.structured_data.protobuf_fixed32_truncated",
                            &[("byte_offset", &pos.to_string())],
                        ));
                    };
                    pos += 4;
                    let raw = u32::from_le_bytes(slice.try_into().expect("4 bytes"));
                    let float = f32::from_bits(raw);
                    json!({
                        "$fixed32": raw,
                        "$float": if float.is_finite() { Value::from(float) } else { Value::from(float.to_string()) },
                    })
                }
                2 => {
                    let (len, used) = read_varint(bytes, pos)?;
                    pos += used;
                    let len = usize::try_from(len).unwrap_or(usize::MAX);
                    let Some(slice) = len.checked_add(pos).and_then(|end| bytes.get(pos..end))
                    else {
                        return Err(error(
                            "results.structured_data.protobuf_field_truncated",
                            &[
                                ("field_number", &field.to_string()),
                                ("claimed_bytes", &len.to_string()),
                                ("byte_offset", &pos.to_string()),
                            ],
                        ));
                    };
                    pos += len;
                    length_delimited(slice, depth)
                }
                3 | 4 => {
                    return Err(error(
                        "results.structured_data.protobuf_group_unsupported",
                        &[
                            ("field_number", &field.to_string()),
                            ("byte_offset", &tag_at.to_string()),
                        ],
                    ));
                }
                other => {
                    return Err(error(
                        "results.structured_data.protobuf_invalid_wire_type",
                        &[
                            ("wire_type", &other.to_string()),
                            ("byte_offset", &tag_at.to_string()),
                        ],
                    ));
                }
            };
            fields.push((field as u32, value));
        }
        Ok(fields)
    }

    fn length_delimited(slice: &[u8], depth: usize) -> Value {
        if let Ok(text) = std::str::from_utf8(slice)
            && (slice.is_empty() || printable_text(text))
        {
            return Value::from(text);
        }
        if depth < MAX_NESTING
            && let Ok(nested) = walk_message(slice, depth + 1)
            && !nested.is_empty()
        {
            return fields_to_json(nested);
        }
        json!({ "$bytes": hex(slice) })
    }

    fn fields_to_json(fields: Vec<(u32, Value)>) -> Value {
        let mut grouped: std::collections::BTreeMap<u32, Vec<Value>> = Default::default();
        for (field, value) in fields {
            grouped.entry(field).or_default().push(value);
        }
        let mut map = serde_json::Map::new();
        for (field, mut values) in grouped {
            let value = if values.len() == 1 {
                values.pop().expect("one value")
            } else {
                Value::Array(values)
            };
            map.insert(field.to_string(), value);
        }
        Value::Object(map)
    }

    // ----- PHP serialize ------------------------------------------------

    /// PHP `serialize()` output by structure: a type tag first and the
    /// whole payload parses.
    pub fn looks_like_php(bytes: &[u8]) -> bool {
        let tagged = matches!(
            bytes,
            [b'a' | b'O' | b's' | b'i' | b'd' | b'b' | b'C', b':', ..] | [b'N', b';', ..]
        );
        tagged && php_decode(bytes).is_ok()
    }

    /// Decode PHP `serialize()` output to JSON. Arrays with the keys
    /// `0..n` in order become JSON arrays, other arrays objects with string
    /// keys; objects become `{"$class": name, "$properties": {…}}` with
    /// protected/private property names spelled `protected:name` /
    /// `private:Class:name`; strings keep their exact bytes (`{"$bytes":
    /// "<hex>"}` when not UTF-8); references become `{"$ref": n}`.
    pub fn php_decode(bytes: &[u8]) -> Result<Value, String> {
        let mut parser = PhpParser { bytes, pos: 0 };
        let value = parser.value(0)?;
        if parser.pos != bytes.len() {
            return Err(error(
                "results.structured_data.php_trailing_bytes",
                &[("byte_count", &(bytes.len() - parser.pos).to_string())],
            ));
        }
        Ok(value)
    }

    struct PhpParser<'a> {
        bytes: &'a [u8],
        pos: usize,
    }

    impl PhpParser<'_> {
        fn next(&mut self) -> Result<u8, String> {
            let byte = *self.bytes.get(self.pos).ok_or_else(|| {
                error(
                    "results.structured_data.php_unexpected_end",
                    &[("byte_offset", &self.pos.to_string())],
                )
            })?;
            self.pos += 1;
            Ok(byte)
        }

        fn expect(&mut self, wanted: u8) -> Result<(), String> {
            let at = self.pos;
            let byte = self.next()?;
            if byte != wanted {
                return Err(error(
                    "results.structured_data.php_unexpected_byte",
                    &[
                        ("expected", &format!("{:?}", wanted as char)),
                        ("byte_offset", &at.to_string()),
                        ("actual", &format!("{:?}", byte as char)),
                    ],
                ));
            }
            Ok(())
        }

        /// The bytes up to (not including) `stop`; consumes `stop`.
        fn until(&mut self, stop: u8) -> Result<&[u8], String> {
            let start = self.pos;
            let end = self.bytes[start..]
                .iter()
                .position(|byte| *byte == stop)
                .map(|offset| start + offset)
                .ok_or_else(|| {
                    error(
                        "results.structured_data.php_delimiter_missing",
                        &[
                            ("delimiter", &format!("{:?}", stop as char)),
                            ("byte_offset", &start.to_string()),
                        ],
                    )
                })?;
            self.pos = end + 1;
            Ok(&self.bytes[start..end])
        }

        fn int(&mut self, stop: u8) -> Result<i64, String> {
            let at = self.pos;
            let digits = self.until(stop)?;
            std::str::from_utf8(digits)
                .ok()
                .and_then(|text| text.parse::<i64>().ok())
                .ok_or_else(|| {
                    error(
                        "results.structured_data.php_invalid_integer",
                        &[("byte_offset", &at.to_string())],
                    )
                })
        }

        fn length(&mut self, stop: u8) -> Result<usize, String> {
            let at = self.pos;
            let len = self.int(stop)?;
            let len = usize::try_from(len).map_err(|_| {
                error(
                    "results.structured_data.php_negative_length",
                    &[("byte_offset", &at.to_string())],
                )
            })?;
            if len > self.bytes.len() - self.pos {
                return Err(error(
                    "results.structured_data.php_length_exceeds_remaining",
                    &[
                        ("claimed_bytes", &len.to_string()),
                        ("byte_offset", &at.to_string()),
                        (
                            "remaining_bytes",
                            &(self.bytes.len() - self.pos).to_string(),
                        ),
                    ],
                ));
            }
            Ok(len)
        }

        fn take(&mut self, len: usize) -> Result<&[u8], String> {
            let start = self.pos;
            let slice = self.bytes.get(start..start + len).ok_or_else(|| {
                error(
                    "results.structured_data.php_bytes_truncated",
                    &[
                        ("byte_count", &len.to_string()),
                        ("byte_offset", &start.to_string()),
                    ],
                )
            })?;
            self.pos += len;
            Ok(slice)
        }

        fn quoted(&mut self, len: usize) -> Result<&[u8], String> {
            self.expect(b'"')?;
            let start = self.pos;
            self.take(len)?;
            self.expect(b'"')?;
            Ok(&self.bytes[start..start + len])
        }

        fn string_value(slice: &[u8]) -> Value {
            match std::str::from_utf8(slice) {
                Ok(text) => Value::from(text),
                Err(_) => json!({ "$bytes": hex(slice) }),
            }
        }

        fn value(&mut self, depth: usize) -> Result<Value, String> {
            if depth > MAX_NESTING {
                return Err(error(
                    "results.structured_data.php_nesting_exceeded",
                    &[("maximum_depth", &MAX_NESTING.to_string())],
                ));
            }
            let at = self.pos;
            let tag = self.next()?;
            match tag {
                b'N' => {
                    self.expect(b';')?;
                    Ok(Value::Null)
                }
                b'b' => {
                    self.expect(b':')?;
                    Ok(Value::from(self.int(b';')? != 0))
                }
                b'i' => {
                    self.expect(b':')?;
                    Ok(Value::from(self.int(b';')?))
                }
                b'd' => {
                    self.expect(b':')?;
                    let text = String::from_utf8_lossy(self.until(b';')?).into_owned();
                    Ok(match text.parse::<f64>() {
                        Ok(number) if number.is_finite() => Value::from(number),
                        _ => Value::from(text),
                    })
                }
                b's' => {
                    self.expect(b':')?;
                    let len = self.length(b':')?;
                    let slice = self.quoted(len)?;
                    let value = Self::string_value(slice);
                    self.expect(b';')?;
                    Ok(value)
                }
                b'a' => {
                    self.expect(b':')?;
                    let count = self.length(b':')?;
                    self.expect(b'{')?;
                    let entries = self.entries(count, depth)?;
                    self.expect(b'}')?;
                    Ok(Self::array_to_json(entries))
                }
                b'O' => {
                    self.expect(b':')?;
                    let len = self.length(b':')?;
                    let name = String::from_utf8_lossy(self.quoted(len)?).into_owned();
                    self.expect(b':')?;
                    let count = self.length(b':')?;
                    self.expect(b'{')?;
                    let entries = self.entries(count, depth)?;
                    self.expect(b'}')?;
                    let mut properties = serde_json::Map::new();
                    for (key, value) in entries {
                        properties.insert(Self::property_name(&key), value);
                    }
                    Ok(json!({ "$class": name, "$properties": properties }))
                }
                b'C' => {
                    self.expect(b':')?;
                    let len = self.length(b':')?;
                    let name = String::from_utf8_lossy(self.quoted(len)?).into_owned();
                    self.expect(b':')?;
                    let data_len = self.length(b':')?;
                    self.expect(b'{')?;
                    let data = Self::string_value(self.take(data_len)?);
                    self.expect(b'}')?;
                    Ok(json!({ "$class": name, "$custom": data }))
                }
                b'r' | b'R' => {
                    self.expect(b':')?;
                    Ok(json!({ "$ref": self.int(b';')? }))
                }
                other => Err(error(
                    "results.structured_data.php_unexpected_tag",
                    &[
                        ("tag_hex", &format!("{other:#04x}")),
                        ("tag_character", &format!("{:?}", other as char)),
                        ("byte_offset", &at.to_string()),
                    ],
                )),
            }
        }

        fn entries(&mut self, count: usize, depth: usize) -> Result<Vec<(String, Value)>, String> {
            // Each entry needs at least `i:0;N;` — refuse a count the bytes
            // cannot hold before allocating for it.
            if count > (self.bytes.len() - self.pos) / 4 {
                return Err(error(
                    "results.structured_data.php_entry_count_exceeds_remaining",
                    &[
                        ("entry_count", &count.to_string()),
                        ("byte_offset", &self.pos.to_string()),
                        (
                            "remaining_bytes",
                            &(self.bytes.len() - self.pos).to_string(),
                        ),
                    ],
                ));
            }
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                let key_at = self.pos;
                let key = match self.value(depth + 1)? {
                    Value::Number(number) => number.to_string(),
                    Value::String(text) => text,
                    other => {
                        let key = match other {
                            Value::Object(_) => "results.structured_data.php_object_key_invalid",
                            Value::Array(_) => "results.structured_data.php_array_key_invalid",
                            Value::Bool(_) => "results.structured_data.php_boolean_key_invalid",
                            _ => "results.structured_data.php_null_key_invalid",
                        };
                        return Err(error(key, &[("byte_offset", &key_at.to_string())]));
                    }
                };
                let value = self.value(depth + 1)?;
                entries.push((key, value));
            }
            Ok(entries)
        }

        fn array_to_json(entries: Vec<(String, Value)>) -> Value {
            let sequential = entries
                .iter()
                .enumerate()
                .all(|(index, (key, _))| key == &index.to_string());
            if sequential {
                return Value::Array(entries.into_iter().map(|(_, value)| value).collect());
            }
            let mut map = serde_json::Map::new();
            for (key, value) in entries {
                map.insert(key, value);
            }
            Value::Object(map)
        }

        /// `\0*\0name` is protected, `\0Class\0name` private.
        fn property_name(raw: &str) -> String {
            let mut parts = raw.split('\0');
            match (parts.next(), parts.next(), parts.next()) {
                (Some(""), Some("*"), Some(name)) => format!("protected:{name}"),
                (Some(""), Some(class), Some(name)) => format!("private:{class}:{name}"),
                _ => raw.replace('\0', "\\0"),
            }
        }
    }

    // ----- Java serialization -------------------------------------------

    const JAVA_MAGIC: [u8; 2] = [0xac, 0xed];
    const JAVA_STREAM_VERSION: u16 = 5;
    const TC_CLASSDESC: u8 = 0x72;
    const TC_STRING: u8 = 0x74;

    pub fn looks_like_java(bytes: &[u8]) -> bool {
        bytes.len() >= 5 && bytes.starts_with(&[0xac, 0xed, 0x00, 0x05])
    }

    /// Summarise a Java `ObjectOutputStream` payload: the stream version
    /// and the class descriptors (name and `serialVersionUID`) and strings
    /// found in it. Field values are not decoded — that needs the classes.
    pub fn java_summary(bytes: &[u8]) -> Result<Value, String> {
        if bytes.len() < 4 || !bytes.starts_with(&JAVA_MAGIC) {
            return Err(error_text("results.structured_data.java_missing_header"));
        }
        let version = u16::from_be_bytes([bytes[2], bytes[3]]);
        if version != JAVA_STREAM_VERSION {
            return Err(error(
                "results.structured_data.java_unknown_stream_version",
                &[
                    ("version", &version.to_string()),
                    ("supported_version", &JAVA_STREAM_VERSION.to_string()),
                ],
            ));
        }
        let mut classes: Vec<Value> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut strings = Vec::new();
        let mut i = 4;
        while i < bytes.len() {
            match bytes[i] {
                TC_CLASSDESC => {
                    if let Some((name, next)) = java_utf(bytes, i + 1, true)
                        && let Some(uid) = bytes.get(next..next + 8)
                    {
                        if seen.insert(name.clone()) {
                            classes.push(json!({
                                "name": name,
                                "serialVersionUID": format!("0x{}", hex(uid)),
                            }));
                        }
                        i = next + 8;
                        continue;
                    }
                }
                TC_STRING => {
                    if let Some((text, next)) = java_utf(bytes, i + 1, false) {
                        if strings.len() < SUMMARY_SAMPLE {
                            strings.push(Value::from(sample_string(&text)));
                        }
                        i = next;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        Ok(json!({
            "$java": {
                "streamVersion": version,
                "bytes": bytes.len(),
                "classes": classes,
                "strings": strings,
            }
        }))
    }

    /// A length-prefixed (u16 big-endian) UTF string at `at`; with
    /// `identifier` only a plausible class name (letters, digits, `_ $ . ;
    /// [`) qualifies.
    fn java_utf(bytes: &[u8], at: usize, identifier: bool) -> Option<(String, usize)> {
        let len = usize::from(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]));
        if len == 0 {
            return None;
        }
        let slice = bytes.get(at + 2..at + 2 + len)?;
        let text = std::str::from_utf8(slice).ok()?;
        if identifier {
            let plausible = text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.' | ';' | '['))
                && text
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '_' | '$' | '['));
            if !plausible {
                return None;
            }
        } else if !printable_text(text) {
            return None;
        }
        Some((text.to_string(), at + 2 + len))
    }

    fn sample_string(text: &str) -> String {
        if text.chars().count() <= SUMMARY_STRING_CHARS {
            return text.to_string();
        }
        let head: String = text.chars().take(SUMMARY_STRING_CHARS).collect();
        format!("{head}…")
    }

    // ----- Python pickle ------------------------------------------------

    const PICKLE_PROTO: u8 = 0x80;
    const PICKLE_STOP: u8 = b'.';
    const PICKLE_FRAME: u8 = 0x95;
    const PICKLE_GLOBAL: u8 = b'c';
    const PICKLE_INST: u8 = b'i';
    const PICKLE_STACK_GLOBAL: u8 = 0x93;

    /// A protocol 2+ pickle: the `PROTO` header, a `STOP` at the end, and
    /// every opcode between them known and complete.
    pub fn looks_like_pickle(bytes: &[u8]) -> bool {
        bytes.len() >= 3
            && bytes[0] == PICKLE_PROTO
            && (2..=5).contains(&bytes[1])
            && bytes[bytes.len() - 1] == PICKLE_STOP
            && pickle_summary(bytes).is_ok()
    }

    /// How an opcode's argument is laid out.
    enum PickleArg {
        /// A fixed number of bytes.
        Fixed(usize),
        /// A length of this many little-endian bytes, then that many bytes.
        Sized(usize),
        /// This many newline-terminated text lines.
        Lines(usize),
    }

    fn pickle_opcode(op: u8) -> Option<(&'static str, PickleArg)> {
        use PickleArg::{Fixed, Lines, Sized};
        Some(match op {
            0x80 => ("PROTO", Fixed(1)),
            0x95 => ("FRAME", Fixed(8)),
            b'.' => ("STOP", Fixed(0)),
            b'(' => ("MARK", Fixed(0)),
            b')' => ("EMPTY_TUPLE", Fixed(0)),
            b'0' => ("POP", Fixed(0)),
            b'1' => ("POP_MARK", Fixed(0)),
            b'2' => ("DUP", Fixed(0)),
            b'N' => ("NONE", Fixed(0)),
            b'Q' => ("BINPERSID", Fixed(0)),
            b'R' => ("REDUCE", Fixed(0)),
            b'a' => ("APPEND", Fixed(0)),
            b'b' => ("BUILD", Fixed(0)),
            b'd' => ("DICT", Fixed(0)),
            b'e' => ("APPENDS", Fixed(0)),
            b'l' => ("LIST", Fixed(0)),
            b'o' => ("OBJ", Fixed(0)),
            b's' => ("SETITEM", Fixed(0)),
            b't' => ("TUPLE", Fixed(0)),
            b'u' => ("SETITEMS", Fixed(0)),
            b'}' => ("EMPTY_DICT", Fixed(0)),
            b']' => ("EMPTY_LIST", Fixed(0)),
            0x81 => ("NEWOBJ", Fixed(0)),
            0x85 => ("TUPLE1", Fixed(0)),
            0x86 => ("TUPLE2", Fixed(0)),
            0x87 => ("TUPLE3", Fixed(0)),
            0x88 => ("NEWTRUE", Fixed(0)),
            0x89 => ("NEWFALSE", Fixed(0)),
            0x8f => ("EMPTY_SET", Fixed(0)),
            0x90 => ("ADDITEMS", Fixed(0)),
            0x91 => ("FROZENSET", Fixed(0)),
            0x92 => ("NEWOBJ_EX", Fixed(0)),
            0x93 => ("STACK_GLOBAL", Fixed(0)),
            0x94 => ("MEMOIZE", Fixed(0)),
            0x97 => ("NEXT_BUFFER", Fixed(0)),
            0x98 => ("READONLY_BUFFER", Fixed(0)),
            b'J' => ("BININT", Fixed(4)),
            b'K' => ("BININT1", Fixed(1)),
            b'M' => ("BININT2", Fixed(2)),
            b'G' => ("BINFLOAT", Fixed(8)),
            b'h' => ("BINGET", Fixed(1)),
            b'j' => ("LONG_BINGET", Fixed(4)),
            b'q' => ("BINPUT", Fixed(1)),
            b'r' => ("LONG_BINPUT", Fixed(4)),
            0x82 => ("EXT1", Fixed(1)),
            0x83 => ("EXT2", Fixed(2)),
            0x84 => ("EXT4", Fixed(4)),
            b'T' => ("BINSTRING", Sized(4)),
            b'U' => ("SHORT_BINSTRING", Sized(1)),
            b'X' => ("BINUNICODE", Sized(4)),
            0x8c => ("SHORT_BINUNICODE", Sized(1)),
            0x8d => ("BINUNICODE8", Sized(8)),
            b'B' => ("BINBYTES", Sized(4)),
            b'C' => ("SHORT_BINBYTES", Sized(1)),
            0x8e => ("BINBYTES8", Sized(8)),
            0x8a => ("LONG1", Sized(1)),
            0x8b => ("LONG4", Sized(4)),
            0x96 => ("BYTEARRAY8", Sized(8)),
            b'c' => ("GLOBAL", Lines(2)),
            b'i' => ("INST", Lines(2)),
            b'I' => ("INT", Lines(1)),
            b'L' => ("LONG", Lines(1)),
            b'F' => ("FLOAT", Lines(1)),
            b'S' => ("STRING", Lines(1)),
            b'V' => ("UNICODE", Lines(1)),
            b'P' => ("PERSID", Lines(1)),
            b'g' => ("GET", Lines(1)),
            b'p' => ("PUT", Lines(1)),
            _ => return None,
        })
    }

    /// Summarise a pickle: protocol, opcode counts, the globals
    /// (`module.name`) it references, a sample of its strings, and whether
    /// it ends with `STOP`. The object is not rebuilt (that would mean
    /// running the pickle's instructions).
    pub fn pickle_summary(bytes: &[u8]) -> Result<Value, String> {
        if bytes.len() < 2 || bytes[0] != PICKLE_PROTO {
            return Err(error_text("results.structured_data.pickle_missing_header"));
        }
        let protocol = bytes[1];
        if !(1..=5).contains(&protocol) {
            return Err(error(
                "results.structured_data.pickle_unknown_protocol",
                &[("protocol", &protocol.to_string())],
            ));
        }
        let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
        let mut globals: Vec<String> = Vec::new();
        let mut strings: Vec<String> = Vec::new();
        let mut recent: Vec<String> = Vec::new();
        let mut frames = 0usize;
        let mut total = 0usize;
        let mut stopped = false;
        let mut pos = 0;
        while pos < bytes.len() {
            let at = pos;
            let op = bytes[pos];
            pos += 1;
            let Some((name, arg)) = pickle_opcode(op) else {
                return Err(error(
                    "results.structured_data.pickle_unknown_opcode",
                    &[
                        ("opcode", &format!("{op:#04x}")),
                        ("byte_offset", &at.to_string()),
                    ],
                ));
            };
            total += 1;
            *counts.entry(name).or_default() += 1;
            match arg {
                PickleArg::Fixed(len) => {
                    if bytes.len() - pos < len {
                        return Err(error(
                            "results.structured_data.pickle_opcode_truncated",
                            &[("opcode", name), ("byte_offset", &at.to_string())],
                        ));
                    }
                    if op == PICKLE_FRAME {
                        frames += 1;
                    }
                    pos += len;
                }
                PickleArg::Sized(width) => {
                    let Some(prefix) = bytes.get(pos..pos + width) else {
                        return Err(error(
                            "results.structured_data.pickle_opcode_truncated",
                            &[("opcode", name), ("byte_offset", &at.to_string())],
                        ));
                    };
                    let mut len = 0u64;
                    for (i, byte) in prefix.iter().enumerate() {
                        len |= u64::from(*byte) << (8 * i);
                    }
                    pos += width;
                    let len = usize::try_from(len).unwrap_or(usize::MAX);
                    let Some(data) = len.checked_add(pos).and_then(|end| bytes.get(pos..end))
                    else {
                        return Err(error(
                            "results.structured_data.pickle_data_truncated",
                            &[
                                ("opcode", name),
                                ("byte_offset", &at.to_string()),
                                ("claimed_bytes", &len.to_string()),
                            ],
                        ));
                    };
                    pos += len;
                    if matches!(op, b'T' | b'U' | b'X' | 0x8c | 0x8d)
                        && let Ok(text) = std::str::from_utf8(data)
                    {
                        recent.push(text.to_string());
                        if recent.len() > 2 {
                            recent.remove(0);
                        }
                        if strings.len() < SUMMARY_SAMPLE {
                            strings.push(sample_string(text));
                        }
                    }
                }
                PickleArg::Lines(count) => {
                    let mut lines = Vec::with_capacity(count);
                    for _ in 0..count {
                        let Some(offset) = bytes[pos..].iter().position(|byte| *byte == b'\n')
                        else {
                            return Err(error(
                                "results.structured_data.pickle_line_unterminated",
                                &[("opcode", name), ("byte_offset", &at.to_string())],
                            ));
                        };
                        lines.push(String::from_utf8_lossy(&bytes[pos..pos + offset]).into_owned());
                        pos += offset + 1;
                    }
                    if matches!(op, PICKLE_GLOBAL | PICKLE_INST) && lines.len() == 2 {
                        globals.push(format!("{}.{}", lines[0], lines[1]));
                    }
                }
            }
            if op == PICKLE_STACK_GLOBAL && recent.len() == 2 {
                globals.push(format!("{}.{}", recent[0], recent[1]));
            }
            if op == PICKLE_STOP {
                stopped = true;
                if pos != bytes.len() {
                    return Err(error(
                        "results.structured_data.pickle_trailing_bytes",
                        &[("byte_count", &(bytes.len() - pos).to_string())],
                    ));
                }
                break;
            }
        }
        globals.dedup();
        Ok(json!({
            "$pickle": {
                "protocol": protocol,
                "opcodes": total,
                "opcodeCounts": counts,
                "globals": globals,
                "strings": strings,
                "frames": frames,
                "complete": stopped,
            }
        }))
    }

    #[cfg(test)]
    mod tests {
        use super::super::ValueFormat;
        use super::super::sniff_format;
        use super::super::tests::{gzip, lz4_frame, mysql_compress, raw_deflate, zlib};
        use super::*;

        #[test]
        fn capped_reader_refuses_past_the_cap() {
            let err = read_capped(std::io::repeat(0), "gzip").unwrap_err();
            assert_eq!(err, "gzip expands past the 16 MB cap");
            let ok = read_capped(&b"small"[..], "gzip").unwrap();
            assert_eq!(ok, b"small");
        }

        #[test]
        fn gzip_round_trips_and_is_sniffed() {
            let packed = gzip(br#"{"a":1}"#);
            assert!(looks_like_gzip(&packed));
            assert_eq!(gunzip(&packed).unwrap(), br#"{"a":1}"#);
            assert_eq!(sniff_format(&packed), Some(ValueFormat::Gzip));
            assert_eq!(
                gunzip(b"not gzip at all, just text").unwrap_err(),
                "no gzip header (1f 8b)"
            );
        }

        #[test]
        fn gzip_bomb_header_is_refused_before_inflating() {
            let mut packed = gzip(b"tiny");
            let n = packed.len();
            packed[n - 4..].copy_from_slice(&u32::MAX.to_le_bytes());
            let err = gunzip(&packed).unwrap_err();
            assert!(
                err.contains("claims 4,294,967,295 bytes") && err.contains("16 MB cap"),
                "{err}"
            );
        }

        #[test]
        fn truncated_gzip_is_reported() {
            let packed = gzip(b"hello hello hello hello hello hello");
            // Cut inside the deflate data (the trailer bytes left behind
            // are garbage, so either the trailer check or the inflater
            // refuses — both are an honest error, never partial output).
            let cut = &packed[..packed.len() - 6];
            let err = gunzip(cut).unwrap_err();
            assert!(
                err.starts_with("gzip:") || err.contains("trailer claims"),
                "{err}"
            );
            // Cut so the trailer still reads small: the inflater reports it.
            let mut short = packed[..packed.len() - 8].to_vec();
            short.extend_from_slice(&[0, 0, 0, 0]);
            let err = gunzip(&short).unwrap_err();
            assert!(err.starts_with("gzip:"), "{err}");
        }

        #[cfg(not(target_arch = "wasm32"))]
        #[test]
        fn zstd_round_trips_refuses_claimed_bombs_and_reports_truncation() {
            let data = br#"{"hello":"zstd"}"#;
            let packed = zstd::encode_all(&data[..], 3).unwrap();
            assert!(looks_like_zstd(&packed));
            assert_eq!(zstd_decode(&packed).unwrap(), data);
            assert_eq!(sniff_format(&packed), Some(ValueFormat::Zstd));

            let cut = &packed[..packed.len() - 3];
            let err = zstd_decode(cut).unwrap_err();
            assert!(err.starts_with("zstd:"), "truncated frame reported: {err}");

            // A frame header announcing 1 GB of content (single-segment,
            // 8-byte content size), refused before any decoding.
            let mut bomb = ZSTD_MAGIC.to_vec();
            bomb.push(0xe0);
            bomb.extend_from_slice(&(1u64 << 30).to_le_bytes());
            bomb.extend_from_slice(&[0; 8]);
            let err = zstd_decode(&bomb).unwrap_err();
            assert!(err.contains("claims 1,073,741,824 bytes"), "{err}");

            assert_eq!(
                zstd_decode(b"plain").unwrap_err(),
                "no zstd frame header (28 b5 2f fd)"
            );
        }

        #[test]
        fn lz4_frame_and_block_round_trip_and_refuse_claimed_bombs() {
            let data = br#"{"lz4":true}"#;
            let packed = lz4_frame(data);
            assert!(looks_like_lz4(&packed));
            assert_eq!(lz4_decode(&packed).unwrap(), data);
            assert_eq!(sniff_format(&packed), Some(ValueFormat::Lz4));

            let block = lz4_flex::compress_prepend_size(data);
            assert!(!looks_like_lz4(&block));
            assert_eq!(lz4_decode(&block).unwrap(), data);

            let mut bomb = block.clone();
            bomb[..4].copy_from_slice(&u32::MAX.to_le_bytes());
            let err = lz4_decode(&bomb).unwrap_err();
            assert!(err.contains("claims 4,294,967,295 bytes"), "{err}");

            let cut = &packed[..packed.len() - 5];
            assert!(lz4_decode(cut).is_err());
            assert!(lz4_decode(b"ab").is_err());
        }

        #[test]
        fn deflate_handles_zlib_and_raw_streams() {
            let data = b"<a><b>deflate</b></a>";
            let packed = zlib(data);
            assert!(looks_like_zlib(&packed), "{packed:02x?}");
            assert_eq!(deflate_decode(&packed).unwrap(), data);
            assert_eq!(sniff_format(&packed), Some(ValueFormat::Deflate));
            let raw = raw_deflate(data);
            assert_eq!(deflate_decode(&raw).unwrap(), data);
            assert!(deflate_decode(b"").is_err());
            assert!(deflate_decode(b"\x78\x9c\x00").is_err());
        }

        #[test]
        fn mysql_compress_decodes_length_prefixed_zlib() {
            let data = br#"{"booking":"BookingTimesheet","rows":[1,2,3]}"#;
            let packed = mysql_compress(data);
            assert_eq!(&packed[..4], &(data.len() as u32).to_le_bytes());
            assert!(looks_like_mysql_compress(&packed), "{packed:02x?}");
            assert_eq!(mysql_compress_decode(&packed).unwrap(), data);
            assert_eq!(sniff_format(&packed), Some(ValueFormat::MySqlCompress));
            // COMPRESS('') is the empty string, and UNCOMPRESS gives it back.
            assert_eq!(mysql_compress_decode(b"").unwrap(), b"");
            // A stream ending in a space gets a '.' appended by MySQL so
            // trailing-space trimming cannot damage it.
            let mut dotted = packed.clone();
            dotted.push(b'.');
            assert_eq!(mysql_compress_decode(&dotted).unwrap(), data);
            // Plain zlib is still plain deflate, not MySQL COMPRESS.
            assert!(!looks_like_mysql_compress(&zlib(data)));
            assert_eq!(sniff_format(&zlib(data)), Some(ValueFormat::Deflate));
        }

        #[test]
        fn mysql_compress_refuses_lying_or_damaged_payloads() {
            let data = b"hello hello hello hello";
            let packed = mysql_compress(data);
            // The prefix must match what the stream inflates to.
            let mut short = packed.clone();
            short[..4].copy_from_slice(&5u32.to_le_bytes());
            let err = mysql_compress_decode(&short).unwrap_err();
            assert!(err.contains("MySQL COMPRESS"), "{err}");
            let mut long = packed.clone();
            long[..4].copy_from_slice(&500u32.to_le_bytes());
            assert!(mysql_compress_decode(&long).is_err());
            // A prefix past the cap is refused before inflating.
            let mut bomb = packed.clone();
            bomb[..4].copy_from_slice(&u32::MAX.to_le_bytes());
            let err = mysql_compress_decode(&bomb).unwrap_err();
            assert!(err.contains("expands past"), "{err}");
            assert!(!looks_like_mysql_compress(&bomb));
            // Truncated stream, garbage after the stream, no stream at all.
            assert!(mysql_compress_decode(&packed[..packed.len() - 3]).is_err());
            let mut junk = packed.clone();
            junk.extend_from_slice(b"junk");
            assert!(mysql_compress_decode(&junk).is_err());
            assert!(mysql_compress_decode(b"\x05\x00\x00").is_err());
            assert!(mysql_compress_decode(b"\x05\x00\x00\x00hello").is_err());
            // A zero length prefix with a stream behind it is not COMPRESS output.
            let mut zero = packed.clone();
            zero[..4].copy_from_slice(&0u32.to_le_bytes());
            assert!(mysql_compress_decode(&zero).is_err());
        }

        #[test]
        fn base64_decodes_every_alphabet_and_sniffs_conservatively() {
            assert_eq!(base64_decode(b"aGVsbG8gd29ybGQ=").unwrap(), b"hello world");
            assert_eq!(base64_decode(b"aGVsbG8gd29ybGQ").unwrap(), b"hello world");
            assert_eq!(base64_decode(b"_-8=").unwrap(), [0xff, 0xef]);
            assert_eq!(
                base64_decode(b"aGVs\nbG8g\nd29y\nbGQ=").unwrap(),
                b"hello world"
            );
            assert!(base64_decode(b"not base64!").is_err());
            assert!(base64_decode(&[0xff, 0xfe]).is_err());

            // JSON inside, standard alphabet, 32 characters.
            let json = b"eyJ1c2VyIjo0MiwibmFtZSI6IkFubiJ9";
            assert!(looks_like_base64(json));
            assert_eq!(sniff_format(json), Some(ValueFormat::Base64));
            // Padded text qualifies.
            assert!(looks_like_base64(b"aGVsbG8gd29ybGQsIGhlbGxvIQ=="));
            // Too short, a plain word, hex, a key name, a mixed-case token
            // that decodes to noise: none of these are Base64 to Auto.
            for not in [
                &b"aGVsbG8="[..],
                b"abcdefghijklmnop",
                b"550e8400e29b41d4a716446655440000",
                b"DEADBEEFDEADBEEF",
                b"user:42:session:abcdef",
                b"Ab3dEf9hIj2kLm4nOp5q",
                b"eyJ1c2Vy+Ijo0Miwi-bmFtZSI6IkFubiJ9",
            ] {
                assert!(
                    !looks_like_base64(not),
                    "{:?}",
                    String::from_utf8_lossy(not)
                );
            }
        }

        fn varint(mut value: u64) -> Vec<u8> {
            let mut out = Vec::new();
            loop {
                let byte = (value & 0x7f) as u8;
                value >>= 7;
                if value == 0 {
                    out.push(byte);
                    return out;
                }
                out.push(byte | 0x80);
            }
        }

        fn field(number: u64, wire: u64) -> Vec<u8> {
            varint((number << 3) | wire)
        }

        fn length_delimited(number: u64, data: &[u8]) -> Vec<u8> {
            let mut out = field(number, 2);
            out.extend(varint(data.len() as u64));
            out.extend_from_slice(data);
            out
        }

        #[test]
        fn protobuf_walks_three_levels_and_rejects_text() {
            // {1: 150, 2: "testing", 3: {1: 1, 2: {1: 2, 3: "deep"}}}
            let mut inner2 = field(1, 0);
            inner2.extend(varint(2));
            inner2.extend(length_delimited(3, b"deep"));
            let mut inner1 = field(1, 0);
            inner1.extend(varint(1));
            inner1.extend(length_delimited(2, &inner2));
            let mut message = field(1, 0);
            message.extend(varint(150));
            message.extend(length_delimited(2, b"testing"));
            message.extend(length_delimited(3, &inner1));

            let value = protobuf_decode(&message).unwrap();
            assert_eq!(value["1"], 150);
            assert_eq!(value["2"], "testing");
            assert_eq!(value["3"]["1"], 1);
            assert_eq!(value["3"]["2"]["1"], 2);
            assert_eq!(value["3"]["2"]["3"], "deep");
            assert!(looks_like_protobuf(&message));
            assert_eq!(sniff_format(&message), Some(ValueFormat::Protobuf));

            assert!(!looks_like_protobuf(b"hello world"));
            assert!(!looks_like_protobuf(b""));
            assert!(protobuf_decode(&[0x08]).is_err());
            let err = protobuf_decode(&[0x12, 0x05, b'a']).unwrap_err();
            assert!(err.contains("past the end"), "{err}");
            let err = protobuf_decode(&[0x0b]).unwrap_err();
            assert!(err.contains("group"), "{err}");
            let err = protobuf_decode(&[0x0f]).unwrap_err();
            assert!(err.contains("invalid wire type 7"), "{err}");
            let err = protobuf_decode(&[0x00, 0x01]).unwrap_err();
            assert!(err.contains("invalid field number 0"), "{err}");

            let repeated = [0x08, 0x01, 0x08, 0x02];
            assert_eq!(protobuf_decode(&repeated).unwrap()["1"], json!([1, 2]));

            let mut fixed = field(1, 1);
            fixed.extend_from_slice(&1.5f64.to_le_bytes());
            fixed.extend(field(2, 5));
            fixed.extend_from_slice(&2.5f32.to_le_bytes());
            let value = protobuf_decode(&fixed).unwrap();
            assert_eq!(value["1"]["$double"], 1.5);
            assert_eq!(value["2"]["$float"], 2.5);

            let binary = length_delimited(1, &[0xff, 0x00, 0xfe]);
            assert_eq!(protobuf_decode(&binary).unwrap()["1"]["$bytes"], "ff00fe");
        }

        #[test]
        fn protobuf_nesting_is_bounded() {
            let mut message = vec![0x08, 0x01];
            for _ in 0..10 {
                message = length_delimited(1, &message);
            }
            let shallow = protobuf_decode(&message).unwrap().to_string();
            assert!(!shallow.contains("$bytes"), "{shallow}");
            for _ in 0..(MAX_NESTING + 10) {
                message = length_delimited(1, &message);
            }
            let deep = protobuf_decode(&message).unwrap().to_string();
            assert!(deep.contains("$bytes"), "past the cap the rest is bytes");
        }

        #[test]
        fn php_decodes_arrays_objects_and_rejects_bad_lengths() {
            let keyed = br#"a:2:{i:0;s:3:"foo";s:3:"bar";i:5;}"#;
            assert_eq!(php_decode(keyed).unwrap(), json!({"0": "foo", "bar": 5}),);
            assert!(looks_like_php(keyed));
            assert_eq!(sniff_format(keyed), Some(ValueFormat::PhpSerialized));

            let list = br#"a:2:{i:0;s:1:"a";i:1;s:1:"b";}"#;
            assert_eq!(php_decode(list).unwrap(), json!(["a", "b"]));

            let object =
                b"O:4:\"User\":2:{s:4:\"name\";s:3:\"Ann\";s:7:\"\0*\0role\";s:5:\"admin\";}";
            assert_eq!(
                php_decode(object).unwrap(),
                json!({"$class": "User", "$properties": {"name": "Ann", "protected:role": "admin"}}),
            );

            let nested = br#"a:1:{s:1:"a";a:1:{s:1:"b";a:1:{s:1:"c";i:3;}}}"#;
            assert_eq!(php_decode(nested).unwrap()["a"]["b"]["c"], 3);

            assert_eq!(php_decode(b"N;").unwrap(), Value::Null);
            assert_eq!(php_decode(b"b:1;").unwrap(), true);
            assert_eq!(php_decode(b"d:1.5;").unwrap(), 1.5);
            assert_eq!(php_decode(b"d:INF;").unwrap(), "INF");
            assert_eq!(php_decode(b"r:1;").unwrap(), json!({"$ref": 1}));
            assert_eq!(
                php_decode(b"s:2:\"\xff\xfe\";").unwrap(),
                json!({"$bytes": "fffe"})
            );
            assert_eq!(
                php_decode(b"C:3:\"Foo\":3:{bar}").unwrap(),
                json!({"$class": "Foo", "$custom": "bar"})
            );

            let err = php_decode(b"s:10:\"abc\";").unwrap_err();
            assert!(err.contains("length 10"), "{err}");
            let err = php_decode(b"a:999999:{}").unwrap_err();
            assert!(
                err.contains("999999") && err.contains("bytes left"),
                "{err}"
            );
            let err = php_decode(b"a:4:{i:0;N;}").unwrap_err();
            assert!(err.contains("4 entries claimed"), "{err}");
            let err = php_decode(b"i:1;x").unwrap_err();
            assert!(err.contains("trailing"), "{err}");
            let err = php_decode(b"a:1:{N;N;}").unwrap_err();
            assert!(err.contains("only integers and strings are keys"), "{err}");
            assert!(!looks_like_php(b"a:b"));
            assert!(!looks_like_php(b"session"));
        }

        #[test]
        fn php_nesting_is_capped() {
            let depth = MAX_NESTING + 5;
            let mut text = String::new();
            for _ in 0..depth {
                text.push_str("a:1:{i:0;");
            }
            text.push_str("N;");
            for _ in 0..depth {
                text.push('}');
            }
            let err = php_decode(text.as_bytes()).unwrap_err();
            assert!(err.contains("deeper than 128"), "{err}");
        }

        #[test]
        fn java_summary_lists_classes_and_strings() {
            let mut stream = vec![0xac, 0xed, 0x00, 0x05, 0x73, 0x72];
            let name = b"com.acme.User";
            stream.extend_from_slice(&(name.len() as u16).to_be_bytes());
            stream.extend_from_slice(name);
            stream.extend_from_slice(&[0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0]);
            stream.extend_from_slice(&[0x02, 0x00, 0x00, 0x78, 0x70]);
            stream.extend_from_slice(&[0x74, 0x00, 0x03]);
            stream.extend_from_slice(b"Ann");

            assert!(looks_like_java(&stream));
            assert_eq!(sniff_format(&stream), Some(ValueFormat::JavaSerialized));
            let summary = java_summary(&stream).unwrap();
            let java = &summary["$java"];
            assert_eq!(java["streamVersion"], 5);
            assert_eq!(java["classes"][0]["name"], "com.acme.User");
            assert_eq!(java["classes"][0]["serialVersionUID"], "0x123456789abcdef0");
            assert_eq!(java["strings"], json!(["Ann"]));

            let err = java_summary(&[0xac, 0xed, 0x00, 0x04, 0x73]).unwrap_err();
            assert!(err.contains("stream version 4"), "{err}");
            assert!(java_summary(b"plain").is_err());
            assert!(!looks_like_java(b"plain"));
        }

        #[test]
        fn pickle_summary_counts_opcodes_and_finds_globals() {
            let body = [b'}', 0x94, 0x8c, 0x01, b'a', 0x94, b'K', 0x01, b's', b'.'];
            let mut pickle = vec![0x80, 0x04, 0x95];
            pickle.extend_from_slice(&(body.len() as u64).to_le_bytes());
            pickle.extend_from_slice(&body);
            assert!(looks_like_pickle(&pickle));
            assert_eq!(sniff_format(&pickle), Some(ValueFormat::Pickle));
            let summary = pickle_summary(&pickle).unwrap();
            let p = &summary["$pickle"];
            assert_eq!(p["protocol"], 4);
            assert_eq!(p["complete"], true);
            assert_eq!(p["frames"], 1);
            assert_eq!(p["strings"], json!(["a"]));
            assert_eq!(p["opcodeCounts"]["MEMOIZE"], 2);
            assert_eq!(p["opcodes"], 9);

            let mut text_global = vec![0x80, 0x02, b'c'];
            text_global.extend_from_slice(b"datetime\ndatetime\n.");
            let summary = pickle_summary(&text_global).unwrap();
            assert_eq!(summary["$pickle"]["globals"], json!(["datetime.datetime"]));

            let mut stack_global = vec![0x80, 0x04, 0x8c, 0x08];
            stack_global.extend_from_slice(b"datetime");
            stack_global.extend_from_slice(&[0x8c, 0x08]);
            stack_global.extend_from_slice(b"datetime");
            stack_global.extend_from_slice(&[0x93, b'.']);
            let summary = pickle_summary(&stack_global).unwrap();
            assert_eq!(summary["$pickle"]["globals"], json!(["datetime.datetime"]));

            let err = pickle_summary(&[0x80, 0x04, 0xff, b'.']).unwrap_err();
            assert!(err.contains("unknown opcode 0xff"), "{err}");
            let err = pickle_summary(&[0x80, 0x04, 0x8c, 0x09, b'a', b'b']).unwrap_err();
            assert!(err.contains("claims 9 bytes"), "{err}");
            let err = pickle_summary(&[0x80, 0x04, b'.', b'x']).unwrap_err();
            assert!(err.contains("trailing"), "{err}");
            let open = pickle_summary(&[0x80, 0x04, b'N']).unwrap();
            assert_eq!(open["$pickle"]["complete"], false);
            assert!(!looks_like_pickle(&[0x80, 0x04, b'N']));
            assert!(pickle_summary(b"N.").is_err(), "protocol 0 has no header");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    pub(super) fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    pub(super) fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    pub(super) fn raw_deflate(data: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    /// What MySQL's `COMPRESS()` stores: the uncompressed length as four
    /// little-endian bytes, then a zlib stream.
    pub(super) fn mysql_compress(data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&zlib(data));
        out
    }

    pub(super) fn lz4_frame(data: &[u8]) -> Vec<u8> {
        let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn compressed_formats_re_detect_what_is_inside() {
        let json = render_payload(&gzip(br#"{"b":2,"a":1}"#), ValueFormat::Gzip);
        assert!(json.as_json, "{json:?}");
        assert_eq!(json.decoded, Some(ValueFormat::Gzip));
        assert!(json.text.contains("\"a\": 1"), "{}", json.text);
        let note = json.note.as_deref().unwrap();
        assert!(note.starts_with("gzip → JSON ("), "{note}");

        let text = render_payload(&gzip(b"plain text"), ValueFormat::Gzip);
        assert_eq!(text.text, "plain text");
        assert!(text.note.as_deref().unwrap().starts_with("gzip → text"));

        let xml = render_payload(&zlib(b"<a><b>1</b></a>"), ValueFormat::Deflate);
        assert!(xml.as_xml && !xml.as_json, "{xml:?}");
        assert!(xml.note.as_deref().unwrap().starts_with("deflate → XML"));

        let mysql = render_payload(&mysql_compress(br#"{"a":1}"#), ValueFormat::MySqlCompress);
        assert!(mysql.as_json, "{mysql:?}");
        assert_eq!(mysql.decoded, Some(ValueFormat::MySqlCompress));
        assert!(
            mysql
                .note
                .as_deref()
                .unwrap()
                .starts_with("MySQL COMPRESS → JSON"),
            "{mysql:?}"
        );
        let (auto_format, auto) = render_auto(&mysql_compress(br#"{"a":1}"#)).unwrap();
        assert_eq!(auto_format, ValueFormat::MySqlCompress);
        assert!(auto.as_json, "{auto:?}");

        let php = render_payload(
            &lz4_frame(br#"a:2:{i:0;s:3:"foo";s:3:"bar";i:5;}"#),
            ValueFormat::Lz4,
        );
        assert!(php.as_json, "{php:?}");
        assert!(php.text.contains("\"bar\": 5"), "{}", php.text);
        assert!(
            php.note
                .as_deref()
                .unwrap()
                .starts_with("LZ4 → PHP serialize"),
            "{php:?}"
        );

        // A second compressed layer is named, not decompressed again.
        let twice = render_payload(&gzip(&gzip(b"x")), ValueFormat::Gzip);
        assert!(twice.text.starts_with("00000000"), "{}", twice.text);
        assert!(
            twice
                .note
                .as_deref()
                .unwrap()
                .contains("nested gzip stream"),
            "{twice:?}"
        );

        let binary = render_payload(&gzip(&[0xff, 0x00, 0x01]), ValueFormat::Gzip);
        assert!(binary.note.as_deref().unwrap().starts_with("gzip → binary"));

        let failed = render_payload(b"nope", ValueFormat::Gzip);
        assert_eq!(failed.decoded, None);
        assert!(failed.text.starts_with("00000000"), "{}", failed.text);
        assert!(
            failed
                .note
                .as_deref()
                .unwrap()
                .contains("not decodable as gzip"),
            "{failed:?}"
        );
    }

    #[test]
    fn base64_and_serialized_formats_render_or_fall_back() {
        let b64 = render_payload(b"eyJ1c2VyIjo0MiwibmFtZSI6IkFubiJ9", ValueFormat::Base64);
        assert!(b64.as_json, "{b64:?}");
        assert_eq!(b64.decoded, Some(ValueFormat::Base64));
        assert!(b64.note.as_deref().unwrap().starts_with("Base64 → JSON"));
        let bad = render_payload(b"!!", ValueFormat::Base64);
        assert!(
            bad.note
                .as_deref()
                .unwrap()
                .contains("not decodable as Base64")
        );

        let proto = render_payload(&[0x08, 0x96, 0x01], ValueFormat::Protobuf);
        assert!(proto.as_json);
        assert!(proto.text.contains("\"1\": 150"), "{}", proto.text);
        assert!(proto.note.as_deref().unwrap().contains("no schema"));

        let php = render_payload(b"i:7;", ValueFormat::PhpSerialized);
        assert_eq!(php.text, "7");
        assert_eq!(php.decoded, Some(ValueFormat::PhpSerialized));

        let java = render_payload(&[0xac, 0xed, 0x00, 0x05, 0x70], ValueFormat::JavaSerialized);
        assert!(java.as_json);
        assert!(java.text.contains("$java"), "{}", java.text);
        assert!(java.note.as_deref().unwrap().contains("class names"));

        let pickle = render_payload(&[0x80, 0x04, b'N', b'.'], ValueFormat::Pickle);
        assert!(pickle.as_json);
        assert!(pickle.text.contains("\"protocol\": 4"), "{}", pickle.text);

        let zstd = render_payload(b"zz", ValueFormat::Zstd);
        assert_eq!(zstd.decoded, None);
        assert!(
            zstd.note
                .as_deref()
                .unwrap()
                .contains("not decodable as zstd")
        );
    }

    #[test]
    fn render_auto_sniffs_compressed_encoded_and_serialized_values() {
        let (format, rendered) = render_auto(&gzip(br#"{"k":"v"}"#)).unwrap();
        assert_eq!(format, ValueFormat::Gzip);
        assert!(rendered.as_json);
        assert_eq!(
            render_auto(b"eyJ1c2VyIjo0MiwibmFtZSI6IkFubiJ9").map(|(f, _)| f),
            Some(ValueFormat::Base64)
        );
        assert_eq!(
            render_auto(&[0x08, 0x96, 0x01, 0x12, 0x02, 0xff, 0xfe]).map(|(f, _)| f),
            Some(ValueFormat::Protobuf)
        );
        assert_eq!(
            render_auto(&[0xac, 0xed, 0x00, 0x05, 0x70]).map(|(f, _)| f),
            Some(ValueFormat::JavaSerialized)
        );
        assert_eq!(
            render_auto(br#"a:1:{i:0;i:1;}"#).map(|(f, _)| f),
            Some(ValueFormat::PhpSerialized)
        );
        // Plain values stay plain.
        for plain in [
            &b""[..],
            b"plain text",
            b"user:42",
            b"00014",
            br#"{"a":1}"#,
            b"<a/>",
            b"\xff\xfe",
        ] {
            assert_eq!(
                render_auto(plain),
                None,
                "{:?}",
                String::from_utf8_lossy(plain)
            );
        }
    }

    /// Redis Insight #6269: picking the JSON format on a string such as
    /// "00014" must not show it as the number 14. Leading zeros are not JSON,
    /// so the raw text stays, with a note saying why.
    #[test]
    fn json_format_keeps_leading_zero_strings_as_written() {
        let rendered = render_payload(b"00014", ValueFormat::Json);
        assert_eq!(rendered.text, "00014");
        assert!(!rendered.as_json);
        assert!(rendered.note.is_some());
        assert_eq!(render_payload(b"00014", ValueFormat::Utf8).text, "00014");
    }

    #[test]
    fn zstd_is_unavailable_only_in_the_browser_build() {
        for format in ValueFormat::ALL {
            assert_eq!(
                format.available(),
                format != ValueFormat::Zstd || cfg!(not(target_arch = "wasm32")),
                "{format:?}"
            );
        }
    }

    #[test]
    fn value_format_names_round_trip_and_unknown_names_are_none() {
        for format in ValueFormat::ALL {
            assert_eq!(ValueFormat::from_name(format.name()), Some(format));
            assert_eq!(
                serde_json::to_string(&format).unwrap(),
                format!("\"{}\"", format.name()),
                "serde name and stable name agree"
            );
        }
        assert_eq!(ValueFormat::from_name("brotli"), None);
        assert_eq!(ValueFormat::from_name(""), None);
        let groups: std::collections::HashSet<_> =
            ValueFormat::ALL.iter().map(|f| f.group()).collect();
        assert_eq!(
            groups.len(),
            FormatGroup::ALL.len(),
            "every group has members"
        );
    }

    #[test]
    fn value_format_store_reads_old_files_and_unknown_names_as_auto() {
        let old = r#"{"entries":[["k1","message_pack"],["k2","brotli"],["k3","gzip"]]}"#;
        let store: ValueFormatStore = serde_json::from_str(old).unwrap();
        assert_eq!(store.version, VALUE_FORMAT_STORE_VERSION);
        assert_eq!(store.get("k1"), Some(ValueFormat::MessagePack));
        assert_eq!(store.get("k2"), None, "unknown name is Auto, so dropped");
        assert_eq!(store.get("k3"), Some(ValueFormat::Gzip));
        let text = serde_json::to_string(&store).unwrap();
        assert!(text.contains("\"version\":2"), "{text}");
        assert!(text.contains("\"gzip\""), "{text}");

        let newer = r#"{"version":99,"entries":[["k1","pickle"],["k4","holographic"]]}"#;
        let store: ValueFormatStore = serde_json::from_str(newer).unwrap();
        assert_eq!(store.get("k1"), Some(ValueFormat::Pickle));
        assert_eq!(store.get("k4"), None);
        assert_eq!(store.entries.len(), 1);
    }

    proptest::proptest! {
        /// Formats are presentation only: rendering a payload in every
        /// format, and Auto's sniff, leave the bytes untouched, and the
        /// picker hands the viewer exactly the cell's bytes.
        #[test]
        fn every_format_leaves_the_raw_bytes_unchanged(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..512)
        ) {
            let before = bytes.clone();
            for format in ValueFormat::ALL {
                let _ = render_payload(&bytes, format);
                proptest::prop_assert_eq!(&bytes, &before);
            }
            let _ = render_auto(&bytes);
            proptest::prop_assert_eq!(&bytes, &before);
            let cell = CellValue::Bytes(bytes.clone());
            proptest::prop_assert_eq!(picker_payload(&cell, &ContentPreview::Hex), Some(before.clone()));
        }

        /// Each codec decodes what its encoder produced to the original
        /// bytes — the decode is lossless as well as side-effect free.
        #[test]
        fn codecs_round_trip_arbitrary_bytes(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 1..256)
        ) {
            use base64::Engine as _;
            proptest::prop_assert_eq!(codecs::gunzip(&gzip(&bytes)).unwrap(), bytes.clone());
            proptest::prop_assert_eq!(codecs::deflate_decode(&zlib(&bytes)).unwrap(), bytes.clone());
            proptest::prop_assert_eq!(codecs::deflate_decode(&raw_deflate(&bytes)).unwrap(), bytes.clone());
            proptest::prop_assert_eq!(codecs::lz4_decode(&lz4_frame(&bytes)).unwrap(), bytes.clone());
            proptest::prop_assert_eq!(
                codecs::lz4_decode(&lz4_flex::compress_prepend_size(&bytes)).unwrap(),
                bytes.clone()
            );
            let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
            proptest::prop_assert_eq!(codecs::base64_decode(encoded.as_bytes()).unwrap(), bytes.clone());
            #[cfg(not(target_arch = "wasm32"))]
            {
                let packed = zstd::encode_all(&bytes[..], 1).unwrap();
                proptest::prop_assert_eq!(codecs::zstd_decode(&packed).unwrap(), bytes.clone());
            }
        }
    }

    // --- detect ---

    #[test]
    fn detect_empty_is_none() {
        assert_eq!(detect(""), None);
        assert_eq!(detect("   "), None);
    }

    #[test]
    fn detect_json_object() {
        assert_eq!(detect(r#"{"a":1}"#), Some(StructuredDataKind::Json));
    }

    #[test]
    fn detect_json_array() {
        assert_eq!(detect(r#"[1,2,3]"#), Some(StructuredDataKind::Json));
    }

    #[test]
    fn detect_json_with_whitespace() {
        assert_eq!(detect(r#"  {"a":1}  "#), Some(StructuredDataKind::Json));
    }

    #[test]
    fn detect_xml() {
        assert_eq!(
            detect("<root><child/></root>"),
            Some(StructuredDataKind::Xml)
        );
    }

    #[test]
    fn detect_plain_text_is_none() {
        assert_eq!(detect("hello world"), None);
        assert_eq!(detect("SELECT 1"), None);
    }

    #[test]
    fn detect_html_fragment_is_none_or_xml() {
        // <br> without closing — not well-formed → None
        assert_eq!(detect("<br>"), None);
        // <br/> is self-closing → XML
        assert_eq!(detect("<br/>"), Some(StructuredDataKind::Xml));
    }

    #[test]
    fn detect_malformed_json_is_none() {
        assert_eq!(detect(r#"{a:1}"#), None);
        assert_eq!(detect(r#"{"""#), None);
    }

    // --- is_json ---

    #[test]
    fn is_json_valid_object() {
        assert!(is_json(r#"{"a":1}"#));
    }

    #[test]
    fn is_json_valid_array() {
        assert!(is_json(r#"[1,2,3]"#));
    }

    #[test]
    fn is_json_rejects_scalar() {
        assert!(!is_json("42"));
        assert!(!is_json("true"));
    }

    #[test]
    fn is_json_rejects_non_brace_start() {
        assert!(!is_json("hello"));
    }

    // --- is_xml ---

    #[test]
    fn is_xml_valid() {
        assert!(is_xml("<root><child>text</child></root>"));
        assert!(is_xml("<a/>"));
        assert!(is_xml("<a b=\"c\"/>"));
    }

    #[test]
    fn is_xml_rejects_unbalanced() {
        assert!(!is_xml("<root><child></root>"));
        assert!(!is_xml("<root>"));
    }

    #[test]
    fn is_xml_tolerates_comments_and_cdata() {
        assert!(is_xml("<!-- c --><root><![CDATA[x]]></root>"));
    }

    #[test]
    fn is_xml_tolerates_processing_instruction() {
        assert!(is_xml("<?xml version=\"1.0\"?><root/>"));
    }

    // --- pretty_print ---

    #[test]
    fn pretty_print_json_indents() {
        let input = r#"{"b":2,"a":1}"#;
        let pretty = pretty_print(input, StructuredDataKind::Json);
        assert!(pretty.contains("\n"));
        assert!(pretty.contains("\"a\""));
        assert!(pretty.contains("\"b\""));
    }

    #[test]
    fn pretty_print_json_sorts_keys() {
        let input = r#"{"z":1,"a":2}"#;
        let pretty = pretty_print(input, StructuredDataKind::Json);
        let a_pos = pretty.find("\"a\"").unwrap();
        let z_pos = pretty.find("\"z\"").unwrap();
        assert!(a_pos < z_pos);
    }

    #[test]
    fn pretty_print_json_invalid_returns_original() {
        let input = r#"{invalid}"#;
        let result = pretty_print(input, StructuredDataKind::Json);
        assert_eq!(result, input);
    }

    #[test]
    fn pretty_print_xml_indents() {
        let input = "<root><child>text</child></root>";
        let pretty = pretty_print(input, StructuredDataKind::Xml);
        assert!(pretty.contains("\n"));
        assert!(pretty.contains("<root>"));
        assert!(pretty.contains("<child>"));
    }

    // --- classify JSON ---

    #[test]
    fn classify_json_finds_keys_and_values() {
        let text = r#"{"name":"Alice","age":30}"#;
        let spans = classify(text, StructuredDataKind::Json);
        let has_key = spans.iter().any(|s| s.token == SyntaxToken::Key);
        let has_string = spans.iter().any(|s| s.token == SyntaxToken::String);
        let has_number = spans.iter().any(|s| s.token == SyntaxToken::Number);
        let has_punct = spans.iter().any(|s| s.token == SyntaxToken::Punctuation);
        assert!(has_key);
        assert!(has_string);
        assert!(has_number);
        assert!(has_punct);
    }

    #[test]
    fn classify_json_finds_boolean_and_null() {
        let text = r#"{"ok":true,"nothing":null}"#;
        let spans = classify(text, StructuredDataKind::Json);
        let has_bool = spans.iter().any(|s| s.token == SyntaxToken::Boolean);
        let has_null = spans.iter().any(|s| s.token == SyntaxToken::Null);
        assert!(has_bool);
        assert!(has_null);
    }

    // --- classify XML ---

    #[test]
    fn classify_xml_finds_element_and_attribute_names() {
        let text = r#"<root attr="value">text</root>"#;
        let spans = classify(text, StructuredDataKind::Xml);
        let has_element = spans.iter().any(|s| s.token == SyntaxToken::ElementName);
        let has_attr_name = spans.iter().any(|s| s.token == SyntaxToken::AttributeName);
        let has_attr_val = spans.iter().any(|s| s.token == SyntaxToken::AttributeValue);
        assert!(has_element);
        assert!(has_attr_name);
        assert!(has_attr_val);
    }

    #[test]
    fn classify_xml_finds_comments() {
        let text = "<!-- comment --><root/>";
        let spans = classify(text, StructuredDataKind::Xml);
        let has_comment = spans.iter().any(|s| s.token == SyntaxToken::Comment);
        assert!(has_comment);
    }

    // --- tree building: JSON ---

    fn find<'a>(node: &'a StructuredNode, label: &str) -> &'a StructuredNode {
        node.children
            .iter()
            .find(|c| c.label == label)
            .unwrap_or_else(|| panic!("missing child {label:?} under {:?}", node.label))
    }

    #[test]
    fn json_tree_builds_labels_and_paths() {
        let text = r#"{"items":[{"name":"a"},{"name":"b"},{"name":"c"}],"n":7}"#;
        let root = build_tree(text, StructuredDataKind::Json).unwrap();
        assert_eq!(root.path, "$");
        assert!(root.is_branch());

        let items = find(&root, "items");
        assert_eq!(items.path, "$.items");
        assert_eq!(items.children.len(), 3);

        let third = &items.children[2];
        assert_eq!(third.label, "[2]");
        assert_eq!(third.path, "$.items[2]");
        let name = find(third, "name");
        assert_eq!(name.path, "$.items[2].name");
        assert_eq!(name.value_preview.as_deref(), Some("\"c\""));

        let n = find(&root, "n");
        assert_eq!(n.path, "$.n");
        assert_eq!(n.value_preview.as_deref(), Some("7"));
        assert!(!n.is_branch());
    }

    #[test]
    fn json_tree_quotes_non_identifier_keys() {
        let text = r#"{"odd key":1}"#;
        let root = build_tree(text, StructuredDataKind::Json).unwrap();
        assert_eq!(root.children[0].path, r#"$["odd key"]"#);
    }

    #[test]
    fn json_tree_caps_children_with_marker() {
        let n = MAX_TREE_CHILDREN + 5;
        let mut text = String::from("[");
        for i in 0..n {
            if i > 0 {
                text.push(',');
            }
            text.push_str(&i.to_string());
        }
        text.push(']');
        let root = build_tree(&text, StructuredDataKind::Json).unwrap();
        assert_eq!(root.children.len(), MAX_TREE_CHILDREN + 1);
        let marker = root.children.last().unwrap();
        assert_eq!(marker.label, "… 5 more");
        assert!(marker.is_truncation_marker());
        // Real children are untouched.
        assert!(!root.children[0].is_truncation_marker());
    }

    #[test]
    fn json_tree_malformed_returns_none() {
        assert!(build_tree("{not json}", StructuredDataKind::Json).is_none());
    }

    // --- tree building: XML ---

    #[test]
    fn xml_tree_builds_elements_attributes_and_text() {
        let text = r#"<root><item id="1">alpha</item><item id="2"><name>beta</name></item></root>"#;
        let root = build_tree(text, StructuredDataKind::Xml).unwrap();
        assert_eq!(root.label, "root");
        assert_eq!(root.path, "/root");
        assert_eq!(root.children.len(), 2);

        let first = &root.children[0];
        assert_eq!(first.label, "item");
        assert_eq!(first.path, "/root/item[1]");
        // Text-only leaf: text becomes the value preview.
        assert_eq!(first.value_preview.as_deref(), Some("alpha"));
        let id = find(first, "@id");
        assert_eq!(id.value_preview.as_deref(), Some("1"));
        assert_eq!(id.path, "/root/item[1]/@id");

        let second = &root.children[1];
        assert_eq!(second.path, "/root/item[2]");
        let name = find(second, "name");
        assert_eq!(name.path, "/root/item[2]/name");
        assert_eq!(name.value_preview.as_deref(), Some("beta"));
    }

    #[test]
    fn xml_tree_unique_sibling_names_are_unindexed() {
        let text = "<root><only>x</only></root>";
        let root = build_tree(text, StructuredDataKind::Xml).unwrap();
        assert_eq!(root.children[0].path, "/root/only");
    }

    #[test]
    fn xml_tree_attr_only_element_gets_attr_summary() {
        let text = r#"<root><e a="1" b="2"/></root>"#;
        let root = build_tree(text, StructuredDataKind::Xml).unwrap();
        let e = &root.children[0];
        assert_eq!(e.value_preview.as_deref(), Some(r#"a="1" b="2""#));
        assert_eq!(e.children.len(), 2);
    }

    #[test]
    fn xml_tree_malformed_returns_none() {
        assert!(build_tree("<root><a></root>", StructuredDataKind::Xml).is_none());
        assert!(build_tree("<root>", StructuredDataKind::Xml).is_none());
        assert!(build_tree("<root", StructuredDataKind::Xml).is_none());
    }

    #[test]
    fn xml_tree_deeply_nested_returns_none_instead_of_overflowing() {
        // Far deeper than any real document: the parser must refuse the
        // document rather than let the recursive tree builder overflow the
        // stack (the DbGate "Maximum call stack size exceeded" bug class).
        let depth = 50_000;
        let mut text = String::with_capacity(depth * 8);
        for _ in 0..depth {
            text.push_str("<a>");
        }
        for _ in 0..depth {
            text.push_str("</a>");
        }
        assert!(build_tree(&text, StructuredDataKind::Xml).is_none());
    }

    #[test]
    fn json_tree_deeply_nested_returns_none_instead_of_overflowing() {
        // serde_json refuses documents past its own nesting limit, so the
        // recursive tree builder is never reached with hostile depth.
        let depth = 1_000;
        let mut text = String::with_capacity(depth * 6);
        for _ in 0..depth {
            text.push_str("{\"a\":");
        }
        text.push('1');
        for _ in 0..depth {
            text.push('}');
        }
        assert!(build_tree(&text, StructuredDataKind::Json).is_none());
    }

    #[test]
    fn xml_tree_tolerates_comments_pi_and_cdata() {
        let text = "<?xml version=\"1.0\"?><!-- c --><root><![CDATA[ raw ]]></root>";
        let root = build_tree(text, StructuredDataKind::Xml).unwrap();
        assert_eq!(root.label, "root");
        assert_eq!(root.value_preview.as_deref(), Some("raw"));
    }

    // --- flatten & search ---

    #[test]
    fn flatten_respects_collapse() {
        let text = r#"{"a":{"x":1,"y":2},"b":3}"#;
        let root = build_tree(text, StructuredDataKind::Json).unwrap();

        let expanded = flatten_visible(&root, &std::collections::HashSet::new());
        let labels: Vec<&str> = expanded.iter().map(|r| r.node.label.as_str()).collect();
        assert_eq!(labels, vec!["$", "a", "x", "y", "b"]);
        assert_eq!(expanded[1].depth, 1);
        assert_eq!(expanded[2].depth, 2);

        let mut collapsed = std::collections::HashSet::new();
        collapsed.insert("$.a".to_string());
        let visible = flatten_visible(&root, &collapsed);
        let labels: Vec<&str> = visible.iter().map(|r| r.node.label.as_str()).collect();
        // Collapsed branch row stays; its descendants produce no rows.
        assert_eq!(labels, vec!["$", "a", "b"]);
    }

    #[test]
    fn search_includes_ancestors() {
        let text = r#"{"outer":{"inner":{"target":"needle"}},"other":1}"#;
        let root = build_tree(text, StructuredDataKind::Json).unwrap();
        let rows = search_rows(&root, "NEEDLE");
        let labels: Vec<&str> = rows.iter().map(|r| r.node.label.as_str()).collect();
        // The matching leaf plus every ancestor, in tree order; non-matching
        // siblings are excluded.
        assert_eq!(labels, vec!["$", "outer", "inner", "target"]);
        assert_eq!(rows[3].depth, 3);
    }

    #[test]
    fn search_matches_labels_and_paths() {
        let text = r#"{"alpha":1,"beta":2}"#;
        let root = build_tree(text, StructuredDataKind::Json).unwrap();
        let by_label = search_rows(&root, "beta");
        assert!(by_label.iter().any(|r| r.node.label == "beta"));
        assert!(!by_label.iter().any(|r| r.node.label == "alpha"));
        // Path matching: "$.alpha" contains "alph".
        let by_path = search_rows(&root, "$.alph");
        assert!(by_path.iter().any(|r| r.node.label == "alpha"));
    }

    #[test]
    fn search_empty_query_returns_all_rows() {
        let text = r#"{"a":{"x":1}}"#;
        let root = build_tree(text, StructuredDataKind::Json).unwrap();
        assert_eq!(search_rows(&root, "  ").len(), 3);
    }

    // --- StructuredDataKind ---

    #[test]
    fn kind_file_extension() {
        assert_eq!(StructuredDataKind::Json.file_extension(), "json");
        assert_eq!(StructuredDataKind::Xml.file_extension(), "xml");
        assert_eq!(StructuredDataKind::Hex.file_extension(), "bin");
        assert_eq!(StructuredDataKind::Hex.display_name(), "Binary");
    }

    #[test]
    fn hex_dump_formats_offset_hex_and_ascii() {
        let dump = hex_dump(b"Hi!\x00\xff");
        // One line: offset, the five hex bytes, and an ASCII gutter where
        // non-printable bytes are dots.
        assert!(dump.starts_with("00000000  48 69 21 00 ff "), "{dump}");
        assert!(dump.trim_end().ends_with("|Hi!..|"), "{dump}");
        assert_eq!(dump.lines().count(), 1);
    }

    #[test]
    fn hex_dump_wraps_at_16_bytes_and_pads_ascii() {
        let bytes: Vec<u8> = (0u8..=16).collect();
        let dump = hex_dump(&bytes);
        let lines: Vec<&str> = dump.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("00000000  00 01 02 03 04 05 06 07  08"));
        assert!(lines[1].starts_with("00000010  10 "));
        // Second line has one byte; its ASCII gutter still renders.
        assert!(lines[1].ends_with("|.|"));
    }

    #[test]
    fn hex_dump_truncates_large_input_with_a_note() {
        let bytes = vec![0xABu8; HEX_DUMP_MAX_BYTES + 100];
        let dump = hex_dump(&bytes);
        assert!(dump.contains("100 more bytes not shown"));
    }

    #[test]
    fn kind_display_name() {
        assert_eq!(StructuredDataKind::Json.display_name(), "JSON");
        assert_eq!(StructuredDataKind::Xml.display_name(), "XML");
        assert_eq!(StructuredDataKind::Image.display_name(), "Image");
        assert_eq!(StructuredDataKind::Spatial.display_name(), "Geometry");
    }

    #[test]
    fn sniff_image_recognises_five_formats_and_rejects_text() {
        let png = b"\x89PNG\r\n\x1a\nrest";
        let jpeg = b"\xFF\xD8\xFF\xE0rest";
        let gif = b"GIF89a....";
        let mut webp = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        webp.extend_from_slice(b"xxxx");
        let mut bmp = b"BM".to_vec();
        bmp.resize(26, 0);
        assert_eq!(sniff_image(png), Some(ImageFormat::Png));
        assert_eq!(sniff_image(jpeg), Some(ImageFormat::Jpeg));
        assert_eq!(sniff_image(gif), Some(ImageFormat::Gif));
        assert_eq!(sniff_image(&webp), Some(ImageFormat::Webp));
        assert_eq!(sniff_image(&bmp), Some(ImageFormat::Bmp));
        assert_eq!(sniff_image(b"hello"), None);
        assert_eq!(sniff_image(b"BM"), None);
    }

    #[test]
    fn decode_data_url_handles_mime_variants_and_bad_base64() {
        use base64::Engine as _;
        let payload = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\n");
        let url = format!("data:image/png;base64,{payload}");
        let (fmt, bytes) = decode_data_url(&url).unwrap();
        assert_eq!(fmt, ImageFormat::Png);
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\n");

        let jpeg_url = format!("DATA:IMAGE/JPG;BASE64,{payload}");
        assert_eq!(decode_data_url(&jpeg_url).unwrap().0, ImageFormat::Jpeg);
        assert!(decode_data_url("data:image/png;base64,!!!").is_none());
        assert!(decode_data_url("data:image/svg+xml;base64,AAAA").is_none());
        assert!(decode_data_url("not a data url").is_none());
    }

    #[test]
    fn classify_cell_multibyte_prefix_does_not_panic() {
        // Regression: `strip_prefix_ci` used to byte-slice at `prefix.len()`,
        // panicking when that offset fell inside a multi-byte scalar. A plain
        // cell value like `abcd€` reaches this via the results-grid right-click
        // menu (menu build calls `classify_cell`).
        for text in ["abcd€", "dat€", "data€x", "abc😀def", "café"] {
            assert_eq!(
                classify_cell(&CellValue::String(text.into())),
                ContentPreview::Text
            );
            assert!(decode_data_url(text).is_none());
        }
    }

    #[test]
    fn classify_cell_prefers_json_then_image_then_spatial_then_text() {
        assert!(matches!(
            classify_cell(&CellValue::String(r#"{"a":1}"#.into())),
            ContentPreview::Json
        ));
        let png = b"\x89PNG\r\n\x1a\nrest".to_vec();
        assert!(matches!(
            classify_cell(&CellValue::Bytes(png)),
            ContentPreview::Image(ImageFormat::Png, _)
        ));
        assert!(matches!(
            classify_cell(&CellValue::String("SRID=4326;POINT(1 2)".into())),
            ContentPreview::Spatial(_)
        ));
        assert!(matches!(
            classify_cell(&CellValue::String(
                "0101000020E6100000000000000000F03F0000000000000040".into()
            )),
            ContentPreview::Spatial(_)
        ));
        assert!(matches!(
            classify_cell(&CellValue::String("hello".into())),
            ContentPreview::Text
        ));
        assert!(matches!(
            classify_cell(&CellValue::Bytes(vec![0, 1, 2, 3])),
            ContentPreview::Hex
        ));
    }

    #[test]
    fn classify_cell_skips_oversized_payloads() {
        let huge_text = "x".repeat(MAX_STRING_SNIFF + 1);
        assert!(matches!(
            classify_cell(&CellValue::String(huge_text)),
            ContentPreview::Text
        ));
        let mut huge_bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        huge_bytes.resize(MAX_BYTES_SNIFF + 1, 0);
        assert!(matches!(
            classify_cell(&CellValue::Bytes(huge_bytes)),
            ContentPreview::Hex
        ));
    }

    // --- value formats ---

    #[test]
    fn utf8_format_is_verbatim_and_marks_lossy() {
        let plain = render_payload(b"hello", ValueFormat::Utf8);
        assert_eq!(plain.text, "hello");
        assert!(!plain.as_json);
        assert!(plain.note.is_none());
        let lossy = render_payload(&[0xff, 0x61], ValueFormat::Utf8);
        assert!(
            lossy.note.as_deref().is_some_and(|n| n.contains("lossy")),
            "{lossy:?}"
        );
        assert!(lossy.text.contains('\u{fffd}'));
    }

    #[test]
    fn json_format_pretties_or_keeps_raw_with_a_note() {
        let pretty = render_payload(br#"{"b":2,"a":1}"#, ValueFormat::Json);
        assert!(pretty.as_json);
        assert!(pretty.text.contains('\n'));
        assert!(pretty.note.is_none());
        let raw = render_payload(b"not json", ValueFormat::Json);
        assert!(!raw.as_json);
        assert_eq!(raw.text, "not json");
        assert!(
            raw.note
                .as_deref()
                .is_some_and(|n| n.contains("not valid JSON"))
        );
    }

    #[test]
    fn hex_format_dumps_any_payload() {
        let rendered = render_payload(b"Hi!", ValueFormat::Hex);
        assert!(rendered.text.starts_with("00000000  48 69 21"));
        assert!(!rendered.as_json);
    }

    #[test]
    fn msgpack_format_decodes_or_falls_back_to_hex() {
        // {"a": 1, "b": [true, nil]}
        let bytes = [0x82, 0xa1, 0x61, 0x01, 0xa1, 0x62, 0x92, 0xc3, 0xc0];
        let rendered = render_payload(&bytes, ValueFormat::MessagePack);
        assert!(rendered.as_json, "{rendered:?}");
        assert!(rendered.text.contains("\"a\": 1"), "{}", rendered.text);
        assert!(
            rendered.text.contains("[true, null]") || rendered.text.contains("true"),
            "{}",
            rendered.text
        );
        // Undecodable payloads fall back to the hex dump with an honest note.
        let fallback = render_payload(b"\xc1nope", ValueFormat::MessagePack);
        assert!(!fallback.as_json);
        assert!(fallback.text.starts_with("00000000"), "{}", fallback.text);
        assert!(
            fallback
                .note
                .as_deref()
                .is_some_and(|n| n.contains("not decodable as MessagePack")),
            "{fallback:?}"
        );
        // A msgpack bin becomes a visible $binary object, never dropped.
        let bin = [0xc4, 0x02, 0xde, 0xad];
        let rendered = render_payload(&bin, ValueFormat::MessagePack);
        assert!(rendered.text.contains("$binary"), "{}", rendered.text);
        assert!(rendered.text.contains("dead"), "{}", rendered.text);
    }

    #[test]
    fn format_memory_remembers_forgets_on_auto_and_evicts_oldest() {
        let mut memory = FormatMemory::new(2);
        memory.set("GET a", ValueFormat::MessagePack);
        memory.set("GET b", ValueFormat::Hex);
        assert_eq!(memory.get("GET a"), Some(ValueFormat::MessagePack));
        // Re-picking refreshes recency, so `b` is now the oldest.
        memory.set("GET a", ValueFormat::Json);
        memory.set("GET c", ValueFormat::Utf8);
        assert_eq!(memory.get("GET b"), None, "oldest evicted");
        assert_eq!(memory.get("GET a"), Some(ValueFormat::Json));
        assert_eq!(memory.get("GET c"), Some(ValueFormat::Utf8));
        memory.set("GET c", ValueFormat::Auto);
        assert_eq!(memory.get("GET c"), None, "Auto forgets");
        assert_eq!(memory.remembered_count(), 1);
    }

    #[test]
    fn cell_identity_separates_statements_rows_and_columns() {
        let a = cell_format_identity("GET k1", 0, 0, "result");
        assert_eq!(a, cell_format_identity(" GET k1 \n", 0, 0, "result"));
        assert_ne!(a, cell_format_identity("GET k2", 0, 0, "result"));
        assert_ne!(a, cell_format_identity("GET k1", 0, 1, "result"));
        assert_ne!(a, cell_format_identity("GET k1", 0, 0, "value"));
        remember_format(&a, ValueFormat::Hex).unwrap();
        assert_eq!(remembered_format(&a), Some(ValueFormat::Hex));
        remember_format(&a, ValueFormat::Auto).unwrap();
        assert_eq!(remembered_format(&a), None);
    }

    fn scope(connection_id: &str, database: &str) -> FormatScope {
        FormatScope {
            connection_id: connection_id.into(),
            database: database.into(),
        }
    }

    #[test]
    fn redis_identity_with_a_scope_names_connection_database_and_key() {
        let conn_a = scope("11111111-aaaa", "0");
        let id = scoped_value_format_identity(Some(&conn_a), "GET user:42", 0, 0, "result");
        assert!(id.starts_with(PERSISTENT_IDENTITY_PREFIX), "{id:?}");
        assert_eq!(
            id,
            scoped_value_format_identity(Some(&conn_a), "get user:42", 2, 5, "result"),
            "same key, any row or tab"
        );
        assert_ne!(
            id,
            scoped_value_format_identity(
                Some(&scope("11111111-aaaa", "1")),
                "GET user:42",
                0,
                0,
                "result"
            ),
            "another database is another key"
        );
        assert_ne!(
            id,
            scoped_value_format_identity(
                Some(&scope("22222222-bbbb", "0")),
                "GET user:42",
                0,
                0,
                "result"
            ),
            "another connection is another key"
        );
        // SQL cells never persist, scope or not.
        let sql = scoped_value_format_identity(Some(&conn_a), "SELECT 1", 0, 0, "c");
        assert!(!sql.starts_with(PERSISTENT_IDENTITY_PREFIX));
    }

    #[test]
    fn persistent_identities_are_remembered_through_the_store() {
        let id = scoped_value_format_identity(
            Some(&scope("33333333-cccc", "0")),
            "GET persisted:key",
            0,
            0,
            "result",
        );
        remember_format(&id, ValueFormat::MessagePack).unwrap();
        let stored = PERSISTENT_FORMATS.lock().unwrap().store.get(&id);
        assert_eq!(
            stored,
            Some(ValueFormat::MessagePack),
            "written to the store"
        );
        assert_eq!(remembered_format(&id), Some(ValueFormat::MessagePack));
        remember_format(&id, ValueFormat::Auto).unwrap();
        assert_eq!(PERSISTENT_FORMATS.lock().unwrap().store.get(&id), None);
        assert_eq!(remembered_format(&id), None);
    }

    #[test]
    fn value_format_store_is_lru_capped_and_auto_forgets() {
        let mut store = ValueFormatStore::default();
        for i in 0..VALUE_FORMAT_STORE_CAP {
            store.set(&format!("k{i}"), ValueFormat::Hex);
        }
        // Re-choosing k0 makes it the most recent, so k1 is evicted next.
        store.set("k0", ValueFormat::Json);
        store.set("new", ValueFormat::Utf8);
        assert_eq!(store.entries.len(), VALUE_FORMAT_STORE_CAP);
        assert_eq!(store.get("k0"), Some(ValueFormat::Json));
        assert_eq!(store.get("k1"), None, "least recently chosen goes first");
        assert_eq!(store.get("new"), Some(ValueFormat::Utf8));
        store.set("new", ValueFormat::Auto);
        assert_eq!(store.get("new"), None);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn value_format_store_round_trips_and_refuses_a_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(ValueFormatStore::FILE);
        assert_eq!(
            ValueFormatStore::load_from(&path),
            Ok(ValueFormatStore::default())
        );
        let mut store = ValueFormatStore::default();
        store.set(
            "redis\u{1f}c\u{1f}0\u{1f}6b\u{1f}result",
            ValueFormat::MessagePack,
        );
        store.save_to(&path).unwrap();
        assert_eq!(ValueFormatStore::load_from(&path), Ok(store));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("message_pack"), "{text}");
        std::fs::write(&path, b"[not json").unwrap();
        let err = ValueFormatStore::load_from(&path).unwrap_err();
        assert!(err.contains("could not be loaded"), "{err}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"[not json",
            "left untouched"
        );
    }

    #[test]
    fn redis_values_are_remembered_by_key_not_by_statement() {
        // The same key read two ways, from two rows or tabs, is one value.
        let get = value_format_identity("GET user:42", 0, 0, "result");
        assert_eq!(
            get,
            value_format_identity("  get \"user:42\" ", 3, 9, "result")
        );
        assert_ne!(get, value_format_identity("GET user:43", 0, 0, "result"));
        // A different column of the same key is its own choice.
        let hash = value_format_identity("HGETALL h", 0, 0, "value");
        assert_eq!(hash, value_format_identity("HGETALL h", 0, 7, "value"));
        assert_ne!(hash, value_format_identity("HGETALL h", 0, 7, "field"));
        // Binary key names are exact, not lossy.
        assert_ne!(
            value_format_identity(r#"GET "\xff""#, 0, 0, "result"),
            value_format_identity(r#"GET "\xfe""#, 0, 0, "result")
        );
        // Not a single key read: the per-cell identity.
        for statement in ["SELECT * FROM t", "GET a\nGET b", "SCAN 0", "SET k v"] {
            assert_eq!(
                value_format_identity(statement, 1, 2, "c"),
                cell_format_identity(statement, 1, 2, "c"),
                "{statement}"
            );
        }
    }

    #[test]
    fn picker_payload_is_byte_exact_for_binary_and_text() {
        let bytes = CellValue::Bytes(vec![0x82, 0xa1, 0x61, 0x01]);
        assert_eq!(
            picker_payload(&bytes, &ContentPreview::Hex),
            Some(vec![0x82, 0xa1, 0x61, 0x01])
        );
        assert_eq!(
            picker_payload(&CellValue::String("{}".into()), &ContentPreview::Json),
            Some(b"{}".to_vec())
        );
        assert_eq!(
            picker_payload(&CellValue::I64(1), &ContentPreview::Text),
            None
        );
    }

    #[test]
    fn value_format_labels_cover_the_picker() {
        assert_eq!(ValueFormat::ALL.len(), 17);
        assert_eq!(ValueFormat::Yaml.label(), "YAML");
        assert_eq!(ValueFormat::Markdown.label(), "Markdown");
        assert_eq!(ValueFormat::Auto.label(), "Auto");
        assert_eq!(ValueFormat::MySqlCompress.label(), "MySQL COMPRESS");
        assert_eq!(ValueFormat::MessagePack.label(), "MessagePack");
        assert_eq!(ValueFormat::PhpSerialized.label(), "PHP serialize");
        let labels: std::collections::HashSet<_> =
            ValueFormat::ALL.iter().map(|f| f.label()).collect();
        assert_eq!(labels.len(), ValueFormat::ALL.len(), "labels are distinct");
    }
}
