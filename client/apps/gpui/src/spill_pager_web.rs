//! Browser (wasm) variant of [`crate::spill_pager`]. The browser executor
//! keeps result sets in memory and never emits spill manifests (there is no
//! filesystem to spill to), so the pager here is a structural stand-in:
//! `open` always returns `None`, row access returns nothing, and cleanup is
//! a no-op. The manifest structs mirror the native ones field-for-field so
//! `results.rs` compiles unchanged.

use std::path::Path;

use sqlly_core::{CellValue, ResultColumn};

/// Describes one result set persisted to a spill file. Mirrors the native
/// struct; never populated in the browser build.
#[derive(Debug, Clone, PartialEq)]
pub struct SpillResultSet {
    pub result_set_index: usize,
    pub columns: Vec<ResultColumn>,
    pub row_count: usize,
    pub spill_file: String,
}

/// Spill manifest mirror. The browser executor never produces one.
#[derive(Debug, Clone, PartialEq)]
pub struct SpillManifest {
    pub result_sets: Vec<SpillResultSet>,
    pub messages: Vec<String>,
    pub error: Option<String>,
    pub elapsed_ms: Option<i64>,
}

impl SpillManifest {
    #[allow(dead_code)]
    /// Mirrors the native parser; the browser build has no CLI spill output,
    /// so nothing ever parses as a manifest.
    pub fn parse(_json_line: &str) -> Option<Self> {
        None
    }

    /// No spill files exist in the browser; nothing to clean up.
    pub fn cleanup(&self) {}
}

/// No spill files exist in the browser; nothing to remove.
pub fn remove_spill_files(_spill_file: &str) {}

/// Stand-in pager. `open` always returns `None` — the browser executor
/// never hands out spill-backed result sets.
pub struct SpillRowPager;

impl SpillRowPager {
    pub fn open<P: AsRef<Path>>(_path: P) -> Option<Self> {
        None
    }

    #[allow(dead_code)]
    pub fn row_count(&self) -> usize {
        0
    }

    pub fn row(&mut self, _index: usize) -> Option<Vec<CellValue>> {
        None
    }

    #[allow(dead_code)]
    pub fn page(&mut self, _offset: usize, _len: usize) -> Vec<Vec<CellValue>> {
        Vec::new()
    }

    #[allow(dead_code)]
    pub fn cell_value_at(
        &mut self,
        _row: usize,
        _column: usize,
        _descriptor: &ResultColumn,
    ) -> Option<CellValue> {
        None
    }
}
