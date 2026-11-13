//! Browser (wasm) variant of [`crate::spill_pager`]. Large results live in
//! the Origin Private File System, behind the spill worker that
//! [`sqlly_services::browser::spill`] talks to, and their paths start with
//! `opfs:`.
//!
//! The page thread cannot read OPFS synchronously, so the synchronous pager
//! here answers from the page cache: the rows the grid's windows loaded
//! (and the first rows of each spill, cached when it lands). That covers the
//! results view's lookups of rows on screen. Everything that walks a spill
//! (scroll paging, exports, column tools, drill-through) reads through the
//! async [`read_spill_rows`] / [`read_spill_window`] instead. The manifest
//! structs mirror the native ones field for field.

use std::path::Path;

use sqlly_core::{CellValue, ResultColumn};
use sqlly_services::browser::spill;

/// Describes one result set persisted to a spill. Mirrors the native struct.
#[derive(Debug, Clone, PartialEq)]
pub struct SpillResultSet {
    pub result_set_index: usize,
    pub columns: Vec<ResultColumn>,
    pub row_count: usize,
    pub spill_file: String,
}

/// Spill manifest mirror.
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

    /// Deletes every spill this manifest names from browser storage.
    pub fn cleanup(&self) {
        for set in &self.result_sets {
            remove_spill_files(&set.spill_file);
        }
    }
}

/// Deletes one result set's spill from browser storage.
pub fn remove_spill_files(spill_file: &str) {
    crate::computed_columns::forget_spill(spill_file);
    spill::remove(spill_file);
}

/// Raw rows `[offset, offset + len)` plus the rows the result's computed
/// columns look back over, read with `read`, extended with those columns.
async fn computed_page<F, Fut>(
    path: &str,
    offset: usize,
    len: usize,
    read: F,
) -> Option<Vec<Vec<CellValue>>>
where
    F: FnOnce(usize, usize) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<Vec<CellValue>>>>,
{
    match crate::computed_columns::spill_window(path) {
        Some(window) => {
            let start = offset.saturating_sub(window.plan.max_lookback());
            let raw = read(start, len + (offset - start)).await?;
            Some(window.page(offset, len, move |_, _| raw))
        }
        None => read(offset, len).await,
    }
}

/// Rows `[offset, offset + len)` of the spill at `path`, clamped to its
/// rows, read from browser storage without filling the page cache: for
/// background work that walks a whole spill. `None` when it cannot be read.
pub async fn read_spill_rows(
    path: String,
    offset: usize,
    len: usize,
) -> Option<Vec<Vec<CellValue>>> {
    let source = path.clone();
    computed_page(&path, offset, len, move |start, n| async move {
        spill::read_rows(&source, start, n)
            .await
            .map_err(|error| tracing::warn!(%error, "spill read failed"))
            .ok()
    })
    .await
}

/// [`read_spill_rows`] for the grid's windows: the rows stay in the page
/// cache, so the view's synchronous lookups of on-screen rows find them.
pub async fn read_spill_window(
    path: String,
    offset: usize,
    len: usize,
) -> Option<Vec<Vec<CellValue>>> {
    let source = path.clone();
    computed_page(&path, offset, len, move |start, n| async move {
        spill::read_rows_cached(&source, start, n)
            .await
            .map_err(|error| tracing::warn!(%error, "spill read failed"))
            .ok()
    })
    .await
}

/// Synchronous pager over a browser spill, answering from the page cache.
pub struct SpillRowPager {
    path: String,
    rows: usize,
    computed: Option<std::sync::Arc<crate::computed_columns::ComputedWindow>>,
}

impl SpillRowPager {
    /// `None` unless `path` names a finished browser spill.
    pub fn open<P: AsRef<Path>>(path: P) -> Option<Self> {
        let path = path.as_ref().to_string_lossy().to_string();
        let rows = spill::row_count(&path)?;
        let computed = crate::computed_columns::spill_window(&path);
        Some(Self {
            path,
            rows,
            computed,
        })
    }

    #[allow(dead_code)]
    pub fn row_count(&self) -> usize {
        self.rows
    }

    /// One cached row; `None` when it is not in the page cache.
    pub fn row(&mut self, index: usize) -> Option<Vec<CellValue>> {
        if index >= self.rows {
            return None;
        }
        self.page(index, 1).pop()
    }

    /// Rows `[offset, offset + len)`, clamped to the spill, when all of them
    /// are in the page cache; otherwise empty.
    pub fn page(&mut self, offset: usize, len: usize) -> Vec<Vec<CellValue>> {
        let path = &self.path;
        let cached =
            |start: usize, n: usize| spill::cached_rows(path, start, n).unwrap_or_default();
        match &self.computed {
            Some(window) => window.page(offset, len, cached),
            None => cached(offset, len),
        }
    }

    #[allow(dead_code)]
    pub fn cell_value_at(
        &mut self,
        row: usize,
        column: usize,
        _descriptor: &ResultColumn,
    ) -> Option<CellValue> {
        self.row(row)?.into_iter().nth(column)
    }
}

/// A browser spill being written, for drill-through's filtered results.
pub struct SpillFileWriter {
    sink: spill::SpillSink,
}

impl SpillFileWriter {
    /// A new spill; `_beside` is the spill it is filtered from (native
    /// writers put the file in the same folder).
    pub fn create(_beside: &str) -> Option<Self> {
        let mut sink = spill::SpillSink::new().ok()?;
        sink.open().ok()?;
        Some(Self { sink })
    }

    pub fn append(&mut self, row: &[CellValue]) -> Result<(), String> {
        self.sink.append(row)
    }

    /// Finishes the spill and returns its path.
    pub fn finish(self) -> Result<String, String> {
        self.sink.finish()
    }
}
