//! The files iCloud sync keeps in iCloud Drive.
//!
//! Every Mac writes only its own file, `devices/<device id>.json`, holding
//! its whole merged view of every synced item, and reads every other Mac's
//! file. One writer per file means iCloud never has to pick between two
//! Macs' edits of the same file; the per-item merge does that instead.
//!
//! A file that cannot be read, is not a sync file, or was written by a newer
//! version of the format is skipped whole: nothing in it is applied. Because
//! an item missing from a file never means "deleted" (deletes are explicit
//! tombstones), skipping a file can delay a change but never lose one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Value of every sync file's `format` field.
pub const SYNC_FORMAT: &str = "sqlly-icloud-sync";
/// Newest file version this build reads and the version it writes.
pub const SYNC_VERSION: u32 = 1;
/// Folder in iCloud Drive that holds the sync files.
pub const SYNC_FOLDER: &str = "SQLLY Sync";
/// Subfolder with one file per Mac.
pub const DEVICES_FOLDER: &str = "devices";

/// One Mac's file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SyncDocument {
    pub format: String,
    pub version: u32,
    /// The writing Mac's sync id (also the file's name).
    pub device_id: String,
    /// When it was written, in milliseconds since the Unix epoch.
    pub written_at: i64,
    pub items: BTreeMap<String, SyncItem>,
}

impl SyncDocument {
    pub fn new(device_id: &str, written_at: i64, items: BTreeMap<String, SyncItem>) -> Self {
        Self {
            format: SYNC_FORMAT.to_string(),
            version: SYNC_VERSION,
            device_id: device_id.to_string(),
            written_at,
            items,
        }
    }
}

/// One synced item: its latest content, or a tombstone saying it was
/// deleted, stamped with when that happened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncItem {
    /// Milliseconds since the Unix epoch. Zero means "this Mac's copy from
    /// before it first synced", which any synced copy beats.
    pub modified_at: i64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
}

impl SyncItem {
    pub fn live(modified_at: i64, payload: Value) -> Self {
        Self {
            modified_at,
            deleted: false,
            payload: Some(payload),
        }
    }

    pub fn tombstone(modified_at: i64) -> Self {
        Self {
            modified_at,
            deleted: true,
            payload: None,
        }
    }

    /// Content hash (empty for a tombstone).
    pub fn hash(&self) -> String {
        match (&self.payload, self.deleted) {
            (Some(payload), false) => payload_hash(payload),
            _ => String::new(),
        }
    }
}

/// `value` as JSON with every object's keys sorted, so equal content always
/// hashes the same whatever order it was written in.
pub fn canonical_json(value: &Value) -> String {
    fn write(value: &Value, out: &mut String) {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (index, key) in keys.into_iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    write(&map[key], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// SHA-256 of [`canonical_json`], hex.
pub fn payload_hash(value: &Value) -> String {
    let digest = Sha256::digest(canonical_json(value).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Why a file in the devices folder was not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkippedFile {
    /// Not JSON, not a sync file, or its items are malformed.
    Damaged { file_name: String, detail: String },
    /// Written by a newer sync format than this build reads.
    Newer { file_name: String, version: u32 },
}

/// Everything read from the devices folder.
#[derive(Debug, Default)]
pub struct RemoteFiles {
    pub documents: Vec<SyncDocument>,
    pub skipped: Vec<SkippedFile>,
    /// Files iCloud has not downloaded to this Mac yet (`.name.icloud`
    /// placeholders); read on a later pass.
    pub pending: Vec<PathBuf>,
}

/// Parse and check one sync file.
pub fn parse_document(file_name: &str, bytes: &[u8]) -> Result<SyncDocument, SkippedFile> {
    let damaged = |detail: String| SkippedFile::Damaged {
        file_name: file_name.to_string(),
        detail,
    };
    let value: Value = serde_json::from_slice(bytes).map_err(|e| damaged(e.to_string()))?;
    if value.get("format").and_then(Value::as_str) != Some(SYNC_FORMAT) {
        return Err(damaged("not a SQLLY sync file".to_string()));
    }
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);
    if version > SYNC_VERSION {
        return Err(SkippedFile::Newer {
            file_name: file_name.to_string(),
            version,
        });
    }
    if version == 0 {
        return Err(damaged("missing version".to_string()));
    }
    let document: SyncDocument =
        serde_json::from_value(value).map_err(|e| damaged(e.to_string()))?;
    for (id, item) in &document.items {
        if item.deleted == item.payload.is_some() {
            return Err(damaged(format!(
                "item {id} is neither content nor a tombstone"
            )));
        }
    }
    Ok(document)
}

/// The sync file name for a device id.
pub fn device_file_name(device_id: &str) -> String {
    format!("{device_id}.json")
}

/// Read every device file in `dir`. A missing folder is an empty result;
/// any other listing failure is an error (sync then changes nothing).
pub fn read_devices_folder(dir: &Path) -> std::io::Result<RemoteFiles> {
    let mut out = RemoteFiles::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(error),
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        // `.<uuid>.json.icloud`: iCloud's stand-in for a file it has not
        // downloaded yet.
        if let Some(inner) = name
            .strip_prefix('.')
            .and_then(|rest| rest.strip_suffix(".icloud"))
        {
            if is_device_file_name(inner) {
                out.pending.push(dir.join(&name));
            }
            continue;
        }
        // Anything else (temp files, iCloud's "name 2.json" conflict
        // copies) is not one Mac's current file.
        if !is_device_file_name(&name) {
            continue;
        }
        match std::fs::read(dir.join(&name)) {
            Ok(bytes) => match parse_document(&name, &bytes) {
                Ok(document) => out.documents.push(document),
                Err(skipped) => out.skipped.push(skipped),
            },
            Err(error) => out.skipped.push(SkippedFile::Damaged {
                file_name: name.clone(),
                detail: error.to_string(),
            }),
        }
    }
    Ok(out)
}

/// `<uuid>.json`.
fn is_device_file_name(name: &str) -> bool {
    name.strip_suffix(".json")
        .is_some_and(|stem| uuid::Uuid::parse_str(stem).is_ok())
}

/// Write this Mac's file atomically (a reader never sees half a file).
pub fn write_device_file(dir: &Path, document: &SyncDocument) -> std::io::Result<()> {
    let bytes = serde_json::to_vec_pretty(document)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    crate::fs_safe::write_atomic(&dir.join(device_file_name(&document.device_id)), &bytes)
}
