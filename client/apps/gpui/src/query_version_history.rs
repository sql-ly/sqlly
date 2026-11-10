//! Per-tab query version history: a bounded, restorable timeline of a query
//! tab's editor text, sealed on every execution and on idle-bounded edit
//! bursts, with optional gzipped result-set payloads.
//!
//! Storage lives under `<data-dir>/SQLLY/QueryHistory/<store-key>/`:
//! `versions.json` is the index (version id, seal timestamp, kind, SQL text,
//! optional payload metadata) and `<version-id>.bin.gz` holds the binary-encoded
//! then gzipped result snapshot for versions that stored results.
//!
//! Lifecycle: closing an untitled tab purges its whole store directory;
//! file-backed tabs keep their history across sessions and tab closures.
//! Eviction drops result payloads before SQL index entries when caps are
//! exceeded.
//!
//! This module is pure logic (no gpui) so every behavior is unit-testable.
//! The UI wires it up in `history_panel.rs` / `app.rs`.
//!
//! Native-only: the store is filesystem-backed and payloads use flate2.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlly_core::{CellValue, ResultColumn};

pub const DEFAULT_MAX_VERSIONS: usize = 100;
pub const DEFAULT_MAX_RESULTS_BYTES_PER_VERSION: u64 = 1024 * 1024;
pub const DEFAULT_MAX_RESULTS_BYTES_PER_TAB: u64 = 25 * 1024 * 1024;

const INDEX_FILE: &str = "versions.json";
const PAYLOAD_MAGIC: &[u8; 4] = b"SQVH";
const PAYLOAD_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum VersionKind {
    Run,
    Edit,
}

impl VersionKind {
    pub fn label(self) -> String {
        let key = match self {
            Self::Run => "query.version_history.kind_run",
            Self::Edit => "query.version_history.kind_edit",
        };
        crate::localization::text(key)
    }
}

/// Metadata for a version's stored result payload (the payload itself lives in
/// `<version-id>.bin.gz` next to the index).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredResultsMeta {
    pub file: String,
    pub result_sets: usize,
    pub rows: usize,
    pub bytes: u64,
    /// The engine the results ran on, so a restore reads their columns as
    /// that engine does. `None` for versions stored before it was recorded.
    #[serde(default)]
    pub engine: Option<sqlly_lang::sqlmodel::DatabaseType>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionEntry {
    pub id: String,
    pub sealed_at_unix_ms: i64,
    pub kind: VersionKind,
    pub sql: String,
    #[serde(default)]
    pub results: Option<StoredResultsMeta>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionIndex {
    #[serde(default)]
    pub versions: Vec<VersionEntry>,
}

/// A single result set as captured for storage. Mirrors the fields of
/// `results::ResultSet` that matter for a read-only historical render, kept
/// independent so this module stays gpui-free.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotSet {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
    pub rows_affected: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCaps {
    pub max_versions: usize,
    pub max_results_bytes_per_tab: u64,
    pub max_results_bytes_per_version: u64,
}

impl Default for HistoryCaps {
    fn default() -> Self {
        Self {
            max_versions: DEFAULT_MAX_VERSIONS,
            max_results_bytes_per_tab: DEFAULT_MAX_RESULTS_BYTES_PER_TAB,
            max_results_bytes_per_version: DEFAULT_MAX_RESULTS_BYTES_PER_VERSION,
        }
    }
}

/// What `seal` did, so the caller can distinguish a real new version from a
/// no-op dedupe (identical consecutive text never seals twice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealOutcome {
    Sealed(String),
    Duplicate,
}

/// What `attach_results` did, so the UI can annotate the version row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachOutcome {
    Stored(StoredResultsMeta),
    /// Encoded payload exceeded the per-version cap; nothing was written.
    TooLarge,
    VersionNotFound,
}

pub struct VersionStore {
    dir: PathBuf,
    caps: HistoryCaps,
    index: VersionIndex,
}

impl VersionStore {
    /// Root directory for every tab's history store.
    pub fn base_dir() -> PathBuf {
        if let Some(data) = dirs::data_dir() {
            return data.join("SQLLY").join("QueryHistory");
        }
        std::env::temp_dir().join("SQLLY").join("QueryHistory")
    }

    /// Stable store key for a tab: file-backed tabs key on their absolute file
    /// path (surviving sessions and tab closures); untitled tabs key on their
    /// tab id (session-scoped, purged on close and swept at startup).
    pub fn store_key(tab_id: &str, file_path: Option<&str>) -> String {
        match file_path {
            Some(p) if !p.is_empty() => {
                let mut h = DefaultHasher::new();
                p.hash(&mut h);
                format!("file-{:016x}", h.finish())
            }
            _ => format!("tab-{tab_id}"),
        }
    }

    /// Whether a store key belongs to an untitled (session-scoped) tab.
    pub fn is_untitled_key(key: &str) -> bool {
        key.starts_with("tab-")
    }

    /// Remove every untitled-tab store left behind by a crashed or force-quit
    /// session. Called once at startup; untitled history must never persist.
    pub fn purge_untitled_stores() {
        let base = Self::base_dir();
        let Ok(entries) = std::fs::read_dir(&base) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if Self::is_untitled_key(&name) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }

    /// Open (or create) the store for a directory, loading its index. A
    /// corrupt index is preserved as `versions.json.corrupt` (never silently
    /// truncated away) and the store starts empty.
    pub fn open(dir: PathBuf, caps: HistoryCaps) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let index_path = dir.join(INDEX_FILE);
        let index = match std::fs::read_to_string(&index_path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(index) => index,
                Err(e) => {
                    tracing::warn!(error = %e, path = %index_path.display(),
                        "query history index is corrupt; preserving as .corrupt and starting empty");
                    let _ = std::fs::rename(&index_path, index_path.with_extension("json.corrupt"));
                    VersionIndex::default()
                }
            },
            Err(_) => VersionIndex::default(),
        };
        Ok(Self { dir, caps, index })
    }

    #[allow(dead_code)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn versions(&self) -> &[VersionEntry] {
        &self.index.versions
    }

    pub fn get(&self, version_id: &str) -> Option<&VersionEntry> {
        self.index.versions.iter().find(|v| v.id == version_id)
    }

    /// Total on-disk bytes of stored result payloads.
    pub fn results_bytes(&self) -> u64 {
        self.index
            .versions
            .iter()
            .filter_map(|v| v.results.as_ref().map(|m| m.bytes))
            .sum()
    }

    /// Seal a new version. Consecutive SQL that differs only in whitespace
    /// (reformatting, re-indenting, blank lines) never seals twice; the token
    /// stream must change for a new version to be created.
    pub fn seal(
        &mut self,
        kind: VersionKind,
        sql: &str,
        sealed_at_unix_ms: i64,
    ) -> std::io::Result<SealOutcome> {
        if let Some(last) = self.index.versions.last()
            && last.sql.split_whitespace().eq(sql.split_whitespace())
        {
            return Ok(SealOutcome::Duplicate);
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.index.versions.push(VersionEntry {
            id: id.clone(),
            sealed_at_unix_ms,
            kind,
            sql: sql.to_string(),
            results: None,
        });
        self.evict()?;
        self.save_index()?;
        Ok(SealOutcome::Sealed(id))
    }

    /// Encode, gzip, and store result sets against an existing version,
    /// honouring the per-version size cap. `version_id` comes from `seal`.
    pub fn attach_results(
        &mut self,
        version_id: &str,
        sets: &[SnapshotSet],
        engine: Option<sqlly_lang::sqlmodel::DatabaseType>,
    ) -> std::io::Result<AttachOutcome> {
        let Some(pos) = self.index.versions.iter().position(|v| v.id == version_id) else {
            return Ok(AttachOutcome::VersionNotFound);
        };
        let encoded = encode_sets(sets);
        let gz = gzip(&encoded)?;
        if gz.len() as u64 > self.caps.max_results_bytes_per_version {
            return Ok(AttachOutcome::TooLarge);
        }
        let file = format!("{version_id}.bin.gz");
        let path = self.dir.join(&file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::fs_safe::write_atomic(&path, &gz)?;
        let meta = StoredResultsMeta {
            file,
            result_sets: sets.len(),
            rows: sets.iter().map(|s| s.rows.len()).sum(),
            bytes: gz.len() as u64,
            engine,
        };
        self.index.versions[pos].results = Some(meta.clone());
        self.evict()?;
        self.save_index()?;
        Ok(AttachOutcome::Stored(meta))
    }

    /// The engine a version's stored results ran on, when recorded.
    pub fn results_engine(&self, version_id: &str) -> Option<sqlly_lang::sqlmodel::DatabaseType> {
        self.get(version_id)?.results.as_ref()?.engine
    }

    /// Load and decode a version's stored result sets, if any.
    pub fn load_results(&self, version_id: &str) -> std::io::Result<Option<Vec<SnapshotSet>>> {
        let Some(entry) = self.get(version_id) else {
            return Ok(None);
        };
        let Some(meta) = &entry.results else {
            return Ok(None);
        };
        let path = self.dir.join(&meta.file);
        let gz = std::fs::read(&path)?;
        let raw = gunzip(&gz)?;
        Ok(Some(decode_sets(&raw).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e)
        })?))
    }

    /// Delete one version (and its payload file) from the index.
    pub fn delete_version(&mut self, version_id: &str) -> std::io::Result<bool> {
        let Some(pos) = self.index.versions.iter().position(|v| v.id == version_id) else {
            return Ok(false);
        };
        let entry = self.index.versions.remove(pos);
        if let Some(meta) = &entry.results {
            let _ = std::fs::remove_file(self.dir.join(&meta.file));
        }
        self.save_index()?;
        Ok(true)
    }

    /// Delete the entire store directory (index + payloads). Used when an
    /// untitled tab closes: its history must not survive the tab.
    pub fn purge(self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }

    /// Drop the store directory for a key without opening it first.
    #[allow(dead_code)]
    pub fn purge_key(key: &str) {
        let _ = std::fs::remove_dir_all(Self::base_dir().join(key));
    }

    /// Enforce caps: result payloads are evicted (oldest first) before SQL
    /// index entries. Payload eviction runs while the tab-level byte cap is
    /// exceeded; entry eviction runs while the version count cap is exceeded.
    fn evict(&mut self) -> std::io::Result<()> {
        while self.results_bytes() > self.caps.max_results_bytes_per_tab {
            let Some(pos) = self.index.versions.iter().position(|v| v.results.is_some()) else {
                break;
            };
            let meta = self.index.versions[pos]
                .results
                .take()
                .expect("checked is_some");
            let _ = std::fs::remove_file(self.dir.join(&meta.file));
        }
        while self.index.versions.len() > self.caps.max_versions {
            let entry = self.index.versions.remove(0);
            if let Some(meta) = &entry.results {
                let _ = std::fs::remove_file(self.dir.join(&meta.file));
            }
        }
        Ok(())
    }

    fn save_index(&self) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(&self.index)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::fs_safe::write_atomic(&self.dir.join(INDEX_FILE), text.as_bytes())
    }
}

// ── Binary payload encoding ────────────────────────────────────────────────
//
// Hand-rolled little-endian encoding (no external bincode dep), versioned via
// a magic + u32 header so future format changes can be detected and rejected
// cleanly instead of mis-decoding.

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn write_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

pub fn encode_sets(sets: &[SnapshotSet]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(PAYLOAD_MAGIC);
    out.extend_from_slice(&PAYLOAD_VERSION.to_le_bytes());
    out.extend_from_slice(&(sets.len() as u32).to_le_bytes());
    for set in sets {
        out.extend_from_slice(&(set.columns.len() as u32).to_le_bytes());
        for col in &set.columns {
            out.push(col.nullable as u8);
            write_str(&mut out, &col.name);
            write_str(&mut out, &col.data_type);
        }
        match set.rows_affected {
            Some(n) => {
                out.push(1);
                out.extend_from_slice(&n.to_le_bytes());
            }
            None => out.push(0),
        }
        out.extend_from_slice(&(set.rows.len() as u32).to_le_bytes());
        for row in &set.rows {
            for cell in row {
                match cell {
                    CellValue::Null => out.push(0),
                    CellValue::Bool(b) => {
                        out.push(1);
                        out.push(*b as u8);
                    }
                    CellValue::I64(v) => {
                        out.push(2);
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                    CellValue::F64(v) => {
                        out.push(3);
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                    CellValue::String(s) => {
                        out.push(4);
                        write_str(&mut out, s);
                    }
                    CellValue::Bytes(b) => {
                        out.push(5);
                        write_bytes(&mut out, b);
                    }
                }
            }
        }
    }
    out
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.pos + n > self.buf.len() {
            return Err(crate::localization::text(
                "query.version_history.payload_truncated",
            ));
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn i64(&mut self) -> Result<i64, String> {
        Ok(self.u64()? as i64)
    }

    fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_bits(self.u64()?))
    }

    fn str(&mut self) -> Result<String, String> {
        let n = self.u32()? as usize;
        let b = self.take(n)?;
        String::from_utf8(b.to_vec())
            .map_err(|_| crate::localization::text("query.version_history.invalid_utf8"))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
}

pub fn decode_sets(buf: &[u8]) -> Result<Vec<SnapshotSet>, String> {
    let mut r = Reader { buf, pos: 0 };
    if r.take(4)? != PAYLOAD_MAGIC {
        return Err(crate::localization::text("query.version_history.bad_magic"));
    }
    if r.u32()? != PAYLOAD_VERSION {
        return Err(crate::localization::text(
            "query.version_history.unsupported_payload",
        ));
    }
    let set_count = r.u32()? as usize;
    let mut sets = Vec::with_capacity(set_count.min(1024));
    for _ in 0..set_count {
        let col_count = r.u32()? as usize;
        let mut columns = Vec::with_capacity(col_count.min(4096));
        for _ in 0..col_count {
            let nullable = r.u8()? != 0;
            let name = r.str()?;
            let data_type = r.str()?;
            columns.push(ResultColumn {
                name,
                data_type,
                nullable,
            });
        }
        let rows_affected = if r.u8()? != 0 { Some(r.u64()?) } else { None };
        let row_count = r.u32()? as usize;
        let mut rows = Vec::with_capacity(row_count.min(1_000_000));
        for _ in 0..row_count {
            let mut row = Vec::with_capacity(col_count);
            for _ in 0..col_count {
                let cell = match r.u8()? {
                    0 => CellValue::Null,
                    1 => CellValue::Bool(r.u8()? != 0),
                    2 => CellValue::I64(r.i64()?),
                    3 => CellValue::F64(r.f64()?),
                    4 => CellValue::String(r.str()?),
                    5 => CellValue::Bytes(r.bytes()?),
                    other => {
                        return Err(crate::localization::format(
                            "query.version_history.unknown_cell_tag",
                            &[("tag", &other.to_string())],
                        ));
                    }
                };
                row.push(cell);
            }
            rows.push(row);
        }
        sets.push(SnapshotSet {
            columns,
            rows,
            rows_affected,
        });
    }
    Ok(sets)
}

pub(crate) fn gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(data)?;
    enc.finish()
}

pub(crate) fn gunzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut dec = flate2::read::GzDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)?;
    Ok(out)
}

// ── Git local history (read-only, file-backed tabs) ────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommitInfo {
    pub hash: String,
    pub subject: String,
    pub unix_ts: i64,
}

/// Recent commits touching `file_path`, newest first, by shelling out to the
/// git CLI (no git library dependency). Returns an empty list when the file
/// is not inside a git work tree or git is unavailable.
pub fn git_local_history(file_path: &Path, max: usize) -> Vec<GitCommitInfo> {
    let Some(dir) = file_path.parent() else {
        return Vec::new();
    };
    let Some(file_name) = file_path.file_name() else {
        return Vec::new();
    };
    if !crate::distribution::can_launch_user_programs() {
        return Vec::new();
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("log")
        .arg(format!("-n{max}"))
        .arg("--format=%H%x1f%ct%x1f%s")
        .arg("--")
        .arg(file_name)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\x1f');
            let hash = parts.next()?.to_string();
            let unix_ts = parts.next()?.parse().ok()?;
            let subject = parts.next().unwrap_or("").to_string();
            Some(GitCommitInfo {
                hash,
                subject,
                unix_ts,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: "int".to_string(),
            nullable: true,
        }
    }

    fn sample_sets() -> Vec<SnapshotSet> {
        vec![
            SnapshotSet {
                columns: vec![col("a"), col("b"), col("c"), col("d"), col("e"), col("f")],
                rows: vec![
                    vec![
                        CellValue::Null,
                        CellValue::Bool(true),
                        CellValue::I64(-42),
                        CellValue::F64(2.5),
                        CellValue::String("héllo".to_string()),
                        CellValue::Bytes(vec![0, 1, 255]),
                    ],
                    vec![
                        CellValue::Bool(false),
                        CellValue::Null,
                        CellValue::I64(i64::MAX),
                        CellValue::F64(-0.0),
                        CellValue::String(String::new()),
                        CellValue::Bytes(Vec::new()),
                    ],
                ],
                rows_affected: Some(2),
            },
            SnapshotSet {
                columns: vec![col("only")],
                rows: vec![],
                rows_affected: None,
            },
        ]
    }

    fn store_in(dir: &Path, caps: HistoryCaps) -> VersionStore {
        VersionStore::open(dir.to_path_buf(), caps).expect("open store")
    }

    #[test]
    fn encode_decode_round_trip() {
        let sets = sample_sets();
        let encoded = encode_sets(&sets);
        let decoded = decode_sets(&encoded).expect("decode");
        assert_eq!(decoded, sets);
    }

    #[test]
    fn decode_rejects_bad_magic_and_truncation() {
        assert!(decode_sets(b"nope").is_err());
        let encoded = encode_sets(&sample_sets());
        assert!(decode_sets(&encoded[..encoded.len() - 3]).is_err());
    }

    #[test]
    fn seal_dedupes_consecutive_identical_sql() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(tmp.path(), HistoryCaps::default());
        assert!(matches!(
            store.seal(VersionKind::Edit, "select 1", 1000).unwrap(),
            SealOutcome::Sealed(_)
        ));
        assert_eq!(
            store.seal(VersionKind::Edit, "select 1", 2000).unwrap(),
            SealOutcome::Duplicate
        );
        assert!(matches!(
            store.seal(VersionKind::Run, "select 2", 3000).unwrap(),
            SealOutcome::Sealed(_)
        ));
        assert_eq!(store.versions().len(), 2);
        assert_eq!(store.versions()[0].kind, VersionKind::Edit);
        assert_eq!(store.versions()[1].kind, VersionKind::Run);
    }

    #[test]
    fn seal_dedupes_whitespace_only_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(tmp.path(), HistoryCaps::default());
        assert!(matches!(
            store
                .seal(VersionKind::Edit, "select a,\n  b from t", 1000)
                .unwrap(),
            SealOutcome::Sealed(_)
        ));
        // Reformatting: indentation, newlines, trailing whitespace collapse.
        assert_eq!(
            store
                .seal(VersionKind::Edit, "select  a, b\nfrom   t\n", 2000)
                .unwrap(),
            SealOutcome::Duplicate
        );
        assert_eq!(
            store
                .seal(VersionKind::Run, "\tselect a, b from t ", 3000)
                .unwrap(),
            SealOutcome::Duplicate
        );
        // A token change still seals, even one that only removes a space.
        assert!(matches!(
            store
                .seal(VersionKind::Edit, "select a,b from t", 4000)
                .unwrap(),
            SealOutcome::Sealed(_)
        ));
        assert_eq!(store.versions().len(), 2);
    }

    #[test]
    fn seal_persists_index_across_open() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut store = store_in(tmp.path(), HistoryCaps::default());
            store.seal(VersionKind::Run, "select 1", 1000).unwrap();
        }
        let store = store_in(tmp.path(), HistoryCaps::default());
        assert_eq!(store.versions().len(), 1);
        assert_eq!(store.versions()[0].sql, "select 1");
    }

    #[test]
    fn attach_results_round_trips_through_gzip_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(tmp.path(), HistoryCaps::default());
        let SealOutcome::Sealed(id) = store.seal(VersionKind::Run, "select 1", 1000).unwrap()
        else {
            panic!("expected seal");
        };
        let outcome = store.attach_results(&id, &sample_sets(), None).unwrap();
        let AttachOutcome::Stored(meta) = outcome else {
            panic!("expected stored");
        };
        assert_eq!(meta.result_sets, 2);
        assert_eq!(meta.rows, 2);
        assert!(tmp.path().join(&meta.file).exists());
        let loaded = store.load_results(&id).unwrap().expect("payload");
        assert_eq!(loaded, sample_sets());
    }

    /// A version remembers the engine its results ran on, and a version
    /// stored before that was recorded reads back as engine unknown.
    #[test]
    fn stored_results_keep_the_engine_they_ran_on() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(tmp.path(), HistoryCaps::default());
        let SealOutcome::Sealed(id) = store.seal(VersionKind::Run, "select 1", 1000).unwrap()
        else {
            panic!("expected seal");
        };
        store
            .attach_results(
                &id,
                &sample_sets(),
                Some(sqlly_lang::sqlmodel::DatabaseType::Postgres),
            )
            .unwrap();
        assert_eq!(
            store.results_engine(&id),
            Some(sqlly_lang::sqlmodel::DatabaseType::Postgres)
        );

        let old: StoredResultsMeta =
            serde_json::from_str(r#"{"file":"a.bin.gz","resultSets":1,"rows":1,"bytes":10}"#)
                .unwrap();
        assert_eq!(old.engine, None);
    }

    #[test]
    fn attach_results_enforces_per_version_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let caps = HistoryCaps {
            max_results_bytes_per_version: 8,
            ..HistoryCaps::default()
        };
        let mut store = store_in(tmp.path(), caps);
        let SealOutcome::Sealed(id) = store.seal(VersionKind::Run, "select 1", 1000).unwrap()
        else {
            panic!("expected seal");
        };
        assert_eq!(
            store.attach_results(&id, &sample_sets(), None).unwrap(),
            AttachOutcome::TooLarge
        );
        assert!(store.get(&id).unwrap().results.is_none());
        assert_eq!(store.load_results(&id).unwrap(), None);
    }

    #[test]
    fn attach_results_unknown_version_is_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(tmp.path(), HistoryCaps::default());
        assert_eq!(
            store
                .attach_results("missing", &sample_sets(), None)
                .unwrap(),
            AttachOutcome::VersionNotFound
        );
    }

    #[test]
    fn eviction_drops_payloads_before_sql_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let caps = HistoryCaps {
            max_versions: 100,
            max_results_bytes_per_tab: 1,
            max_results_bytes_per_version: u64::MAX,
        };
        let mut store = store_in(tmp.path(), caps);
        let SealOutcome::Sealed(id) = store.seal(VersionKind::Run, "select 1", 1000).unwrap()
        else {
            panic!("expected seal");
        };
        store.attach_results(&id, &sample_sets(), None).unwrap();
        // Tab byte cap is 1: the payload must be evicted, the SQL entry kept.
        assert_eq!(store.versions().len(), 1);
        assert!(store.get(&id).unwrap().results.is_none());
        assert_eq!(store.load_results(&id).unwrap(), None);
    }

    #[test]
    fn eviction_drops_oldest_entries_beyond_version_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let caps = HistoryCaps {
            max_versions: 2,
            ..HistoryCaps::default()
        };
        let mut store = store_in(tmp.path(), caps);
        store.seal(VersionKind::Edit, "select 1", 1000).unwrap();
        store.seal(VersionKind::Edit, "select 2", 2000).unwrap();
        store.seal(VersionKind::Edit, "select 3", 3000).unwrap();
        let sqls: Vec<&str> = store.versions().iter().map(|v| v.sql.as_str()).collect();
        assert_eq!(sqls, vec!["select 2", "select 3"]);
    }

    #[test]
    fn delete_version_removes_entry_and_payload() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(tmp.path(), HistoryCaps::default());
        let SealOutcome::Sealed(id) = store.seal(VersionKind::Run, "select 1", 1000).unwrap()
        else {
            panic!("expected seal");
        };
        let AttachOutcome::Stored(meta) = store.attach_results(&id, &sample_sets(), None).unwrap()
        else {
            panic!("expected stored");
        };
        let payload_path = tmp.path().join(&meta.file);
        assert!(payload_path.exists());
        assert!(store.delete_version(&id).unwrap());
        assert!(!payload_path.exists());
        assert!(store.versions().is_empty());
        assert!(!store.delete_version(&id).unwrap());
    }

    #[test]
    fn purge_removes_whole_store_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("tab-1");
        let mut store = store_in(&dir, HistoryCaps::default());
        store.seal(VersionKind::Edit, "select 1", 1000).unwrap();
        assert!(dir.exists());
        store.purge();
        assert!(!dir.exists());
    }

    #[test]
    fn store_key_is_stable_for_file_path_and_scoped_for_tabs() {
        let a = VersionStore::store_key("tab-1", Some("/tmp/query.sql"));
        let b = VersionStore::store_key("tab-9", Some("/tmp/query.sql"));
        assert_eq!(a, b);
        assert!(a.starts_with("file-"));
        let t = VersionStore::store_key("tab-1", None);
        assert_eq!(t, "tab-tab-1");
        assert!(VersionStore::is_untitled_key(&t));
        assert!(!VersionStore::is_untitled_key(&a));
        let e = VersionStore::store_key("tab-1", Some(""));
        assert_eq!(e, "tab-tab-1");
    }

    #[test]
    fn writes_are_atomic_and_leave_no_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(tmp.path(), HistoryCaps::default());
        let SealOutcome::Sealed(id) = store.seal(VersionKind::Run, "select 1", 1000).unwrap()
        else {
            panic!("expected seal");
        };
        store.attach_results(&id, &sample_sets(), None).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away");
    }

    #[test]
    fn corrupt_index_is_preserved_and_store_starts_empty() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(INDEX_FILE), "{ not json").unwrap();
        let store = store_in(tmp.path(), HistoryCaps::default());
        assert!(store.versions().is_empty());
        let preserved = tmp.path().join("versions.json.corrupt");
        assert!(preserved.exists(), "corrupt index must be preserved");
        assert_eq!(std::fs::read_to_string(preserved).unwrap(), "{ not json");
        assert!(!tmp.path().join(INDEX_FILE).exists());
    }

    #[test]
    fn corrupt_payload_errors_instead_of_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = store_in(tmp.path(), HistoryCaps::default());
        let SealOutcome::Sealed(id) = store.seal(VersionKind::Run, "select 1", 1000).unwrap()
        else {
            panic!("expected seal");
        };
        let AttachOutcome::Stored(meta) = store.attach_results(&id, &sample_sets(), None).unwrap()
        else {
            panic!("expected stored");
        };
        std::fs::write(tmp.path().join(&meta.file), b"garbage").unwrap();
        assert!(store.load_results(&id).is_err());
    }

    #[test]
    #[cfg_attr(feature = "app-store", ignore = "the App Store edition never runs git")]
    fn git_local_history_lists_commits_touching_the_file() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        let file = tmp.path().join("q.sql");
        std::fs::write(&file, "select 1").unwrap();
        run(&["add", "q.sql"]);
        run(&["commit", "-q", "-m", "first"]);
        std::fs::write(&file, "select 2").unwrap();
        run(&["add", "q.sql"]);
        run(&["commit", "-q", "-m", "second"]);
        let history = git_local_history(&file, 10);
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].subject, "second");
        assert_eq!(history[1].subject, "first");
        assert!(history[0].unix_ts >= history[1].unix_ts);
        assert_eq!(history[0].hash.len(), 40);
        // A path outside any work tree yields an empty list, not an error.
        assert!(git_local_history(Path::new("/definitely/not/a/repo.sql"), 10).is_empty());
    }
}
