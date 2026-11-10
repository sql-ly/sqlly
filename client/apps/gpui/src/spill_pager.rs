//! Random-access reader over a single result set's spill file, plus the
//! manifest that describes them.
//!
//! The on-disk format is the typed binary layout owned by
//! [`sqlly_core::spill`] (magic + version header, per-row u32 column count +
//! u32 cell lengths + tagged cell payloads, and a `.idx` sidecar giving row
//! `i`'s offset at byte `8 + i*8`). This module is a thin adapter over
//! [`sqlly_core::SpillReader`]: rows come back as typed [`CellValue`]s with
//! two seeks and no parsing, and neither the rows nor the offset index are
//! ever fully resident — peak memory is O(page).

use std::fs;
use std::path::Path;

use sqlly_core::{CellValue, ResultColumn, SpillReader};

/// Describes one result set persisted to a spill file by the CLI
/// `query --spill` command: its columns, total row count, and the on-disk
/// file backing it.
#[derive(Debug, Clone, PartialEq)]
pub struct SpillResultSet {
    pub result_set_index: usize,
    pub columns: Vec<ResultColumn>,
    pub row_count: usize,
    pub spill_file: String,
}

/// The manifest the CLI prints to stdout when a query is run with `--spill`.
/// Rows are NOT in the manifest — they live on disk in each result set's
/// spill file and are paged on demand, so the app never materializes the full
/// set.
#[derive(Debug, Clone, PartialEq)]
pub struct SpillManifest {
    pub result_sets: Vec<SpillResultSet>,
    pub messages: Vec<String>,
    pub error: Option<String>,
    pub elapsed_ms: Option<i64>,
}

impl SpillManifest {
    #[allow(dead_code)]
    /// Parses the `{"kind":"spill_manifest", ...}` JSON line emitted by the
    /// CLI. Returns `None` if the line is not a spill manifest (so callers can
    /// fall back to the legacy inline-event format).
    pub fn parse(json_line: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(json_line).ok()?;
        let obj = v.as_object()?;
        if obj.get("kind").and_then(|k| k.as_str()) != Some("spill_manifest") {
            return None;
        }

        let result_sets = obj
            .get("result_sets")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .map(|entry| {
                        let empty = serde_json::Map::new();
                        let e = entry.as_object().unwrap_or(&empty);
                        let columns = e
                            .get("columns")
                            .and_then(|v| v.as_array())
                            .map(|cols| {
                                cols.iter()
                                    .map(|col| {
                                        let empty_col = serde_json::Map::new();
                                        let c = col.as_object().unwrap_or(&empty_col);
                                        ResultColumn {
                                            name: c
                                                .get("name")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string(),
                                            data_type: c
                                                .get("data_type")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("nvarchar(max)")
                                                .to_string(),
                                            nullable: c
                                                .get("nullable")
                                                .and_then(|v| v.as_bool())
                                                .unwrap_or(true),
                                        }
                                    })
                                    .collect::<Vec<_>>()
                            })
                            .unwrap_or_default();
                        SpillResultSet {
                            result_set_index: e
                                .get("result_set_index")
                                .and_then(|v| v.as_u64())
                                .map(|n| n as usize)
                                .unwrap_or(0),
                            columns,
                            row_count: e
                                .get("row_count")
                                .and_then(|v| v.as_u64())
                                .map(|n| n as usize)
                                .unwrap_or(0),
                            spill_file: e
                                .get("spill_file")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        }
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Some(SpillManifest {
            result_sets,
            messages: obj
                .get("messages")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
            error: obj
                .get("error")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            elapsed_ms: obj.get("elapsed_ms").and_then(|v| v.as_i64()),
        })
    }

    /// Deletes every spill file referenced by this manifest. Best-effort;
    /// errors are ignored. Call when a query tab is closed or a new query
    /// replaces the results.
    pub fn cleanup(&self) {
        for set in &self.result_sets {
            remove_spill_files(&set.spill_file);
        }
    }
}

/// Deletes one result set's spill file and its `.idx` sidecar. Best-effort;
/// errors are ignored. Also used for manifests that arrive for a superseded
/// run — those files have no other owner.
pub fn remove_spill_files(spill_file: &str) {
    crate::computed_columns::forget_spill(spill_file);
    let _ = fs::remove_file(spill_file);
    let _ = fs::remove_file(format!("{spill_file}.idx"));
}

/// Random-access reader over a single result set's spill file — a thin
/// adapter over [`sqlly_core::SpillReader`] with `Option`-shaped accessors
/// for the view layer. Rows come back as typed [`CellValue`]s; neither the
/// rows nor the offset index are ever fully resident.
pub struct SpillRowPager {
    reader: SpillReader,
    /// The result's `-- sqlly column:` formulas, when it has any: every
    /// row read comes back with the computed columns in place.
    computed: Option<std::sync::Arc<crate::computed_columns::ComputedWindow>>,
}

impl SpillRowPager {
    /// Opens `path` (validating the format header, rebuilding a missing
    /// `.idx` sidecar if needed). Returns `None` if the file cannot be
    /// opened or is corrupt.
    pub fn open<P: AsRef<Path>>(path: P) -> Option<Self> {
        let computed = crate::computed_columns::spill_window(&path.as_ref().to_string_lossy());
        SpillReader::open(path)
            .ok()
            .map(|reader| Self { reader, computed })
    }

    #[allow(dead_code)]
    /// Number of rows in the spill file.
    pub fn row_count(&self) -> usize {
        self.reader.row_count()
    }

    /// Reads one row of typed cells by absolute index, seeking directly.
    /// Returns `None` on any I/O or decode error.
    pub fn row(&mut self, index: usize) -> Option<Vec<CellValue>> {
        if self.computed.is_some() {
            return self.page(index, 1).pop();
        }
        self.reader.row(index).ok()
    }

    #[allow(dead_code)]
    /// Reads a contiguous page `[offset, offset+len)` of typed rows, clamped
    /// to the available rows. This is what the grid calls per page.
    pub fn page(&mut self, offset: usize, len: usize) -> Vec<Vec<CellValue>> {
        match self.computed.clone() {
            Some(window) => window.page(offset, len, |start, n| {
                self.reader.page(start, n).unwrap_or_default()
            }),
            None => self.reader.page(offset, len).unwrap_or_default(),
        }
    }

    #[allow(dead_code)]
    /// Typed access to a single cell. The spill format stores typed cells, so
    /// no display-text coercion is involved; the column descriptor is no
    /// longer needed but kept for call-site compatibility.
    pub fn cell_value_at(
        &mut self,
        row: usize,
        column: usize,
        _descriptor: &ResultColumn,
    ) -> Option<CellValue> {
        self.row(row)?.into_iter().nth(column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(data_type: &str) -> ResultColumn {
        ResultColumn {
            name: "c".into(),
            data_type: data_type.into(),
            nullable: true,
        }
    }

    #[test]
    fn manifest_parse_rejects_non_manifest() {
        assert!(SpillManifest::parse(r#"{"kind":"rows_batch"}"#).is_none());
        assert!(SpillManifest::parse("not json").is_none());
    }

    #[test]
    fn manifest_parse_basic() {
        let json = r#"{"kind":"spill_manifest","result_sets":[{"result_set_index":0,"columns":[{"name":"id","data_type":"int","nullable":false}],"row_count":3,"spill_file":"/tmp/x.spill"}],"messages":["ok"],"elapsed_ms":42}"#;
        let m = SpillManifest::parse(json).unwrap();
        assert_eq!(m.result_sets.len(), 1);
        assert_eq!(m.result_sets[0].result_set_index, 0);
        assert_eq!(m.result_sets[0].row_count, 3);
        assert_eq!(m.result_sets[0].spill_file, "/tmp/x.spill");
        assert_eq!(m.result_sets[0].columns.len(), 1);
        assert_eq!(m.result_sets[0].columns[0].name, "id");
        assert_eq!(m.result_sets[0].columns[0].data_type, "int");
        assert!(!m.result_sets[0].columns[0].nullable);
        assert_eq!(m.messages, vec!["ok".to_string()]);
        assert_eq!(m.elapsed_ms, Some(42));
        assert_eq!(m.error, None);
    }

    #[test]
    fn manifest_parse_with_error() {
        let json = r#"{"kind":"spill_manifest","result_sets":[],"error":"boom"}"#;
        let m = SpillManifest::parse(json).unwrap();
        assert!(m.result_sets.is_empty());
        assert_eq!(m.error.as_deref(), Some("boom"));
    }

    #[test]
    fn manifest_parse_defaults_missing_fields() {
        let json = r#"{"kind":"spill_manifest","result_sets":[]}"#;
        let m = SpillManifest::parse(json).unwrap();
        assert!(m.messages.is_empty());
        assert_eq!(m.elapsed_ms, None);
    }

    /// Round-trip through the canonical typed binary format: what the
    /// services executor writes, the pager reads back as typed cells.
    #[test]
    fn pager_round_trips_typed_rows() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "gpui_spill_pager_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut w = sqlly_core::SpillWriter::create(&path).unwrap();
        for i in 0..100i64 {
            w.append(&[
                CellValue::I64(i),
                CellValue::String(format!("row{i}")),
                CellValue::Null,
                CellValue::Bool(i % 2 == 0),
            ])
            .unwrap();
        }
        w.finish().unwrap();

        let mut pager = SpillRowPager::open(&path).expect("pager opens");
        assert_eq!(pager.row_count(), 100);
        assert_eq!(
            pager.row(42).unwrap(),
            vec![
                CellValue::I64(42),
                CellValue::String("row42".into()),
                CellValue::Null,
                CellValue::Bool(true),
            ]
        );
        let page = pager.page(90, 20);
        assert_eq!(page.len(), 10, "page clamps to available rows");
        assert_eq!(
            pager.cell_value_at(7, 1, &col("nvarchar")),
            Some(CellValue::String("row7".into()))
        );
        assert_eq!(pager.row(100), None, "out of range is None, not a panic");
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(format!("{}.idx", path.display())).ok();
    }

    #[test]
    fn pager_open_rejects_garbage_file() {
        let mut path = std::env::temp_dir();
        path.push(format!("gpui_spill_garbage_{}.bin", std::process::id()));
        std::fs::write(&path, b"this is not a spill file").unwrap();
        assert!(SpillRowPager::open(&path).is_none());
        std::fs::remove_file(&path).ok();
    }
}
