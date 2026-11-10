//! Stream spill-backed and in-memory result exports row-by-row to disk.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use sqlly_core::{CellValue, DisplayMessage, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::query::QueryEvent;

use crate::background_tasks::TaskProgress;
use crate::export_options::{self, ExportOptions};
use crate::result_export::{
    csv_field_with, escape_identifier_for, escape_multipart_identifier_for, json_object_named,
    sql_literal_in_column, tsv_field_with, unique_column_names,
};
use crate::results::ExportFormat;
use crate::spill_pager::SpillRowPager;
#[cfg(not(target_arch = "wasm32"))]
use crate::table_export::TableExportTarget;

pub const CANCELLED: &str = "cancelled";

pub const PAGE_ROWS: usize = 2_000;

const INSERT_TABLE: &str = "Results";

/// Formats that can be written row by row without holding the set in memory.
pub fn is_streamable(format: ExportFormat) -> bool {
    matches!(
        format,
        ExportFormat::Csv | ExportFormat::Tsv | ExportFormat::JsonLines | ExportFormat::Insert
    )
}

pub trait RowWriter {
    fn begin(&mut self, out: &mut dyn Write, columns: &[ResultColumn]) -> std::io::Result<()>;
    fn row(
        &mut self,
        out: &mut dyn Write,
        columns: &[ResultColumn],
        row: &[CellValue],
    ) -> std::io::Result<()>;
    fn end(&mut self, out: &mut dyn Write) -> std::io::Result<()>;
}

pub fn writer_for(
    format: ExportFormat,
    db: DatabaseType,
    opts: ExportOptions,
) -> Option<Box<dyn RowWriter + Send>> {
    match format {
        ExportFormat::Csv => Some(Box::new(CsvWriter { opts })),
        ExportFormat::Tsv => Some(Box::new(TsvWriter { opts })),
        ExportFormat::JsonLines => Some(Box::new(JsonLinesWriter::new())),
        ExportFormat::Insert => Some(Box::new(InsertWriter {
            db,
            names: Vec::new(),
            pending: None,
            header_written: false,
        })),
        _ => None,
    }
}

pub fn gz_path(path: &Path, gzip: bool) -> PathBuf {
    export_options::gz_path(path, gzip)
}

struct TsvWriter {
    opts: ExportOptions,
}

impl RowWriter for TsvWriter {
    fn begin(&mut self, out: &mut dyn Write, columns: &[ResultColumn]) -> std::io::Result<()> {
        if !self.opts.header_row {
            return Ok(());
        }
        let header = columns
            .iter()
            .map(|c| tsv_field_with(&CellValue::String(c.name.clone()), &self.opts))
            .collect::<Vec<_>>()
            .join("\t");
        out.write_all(header.as_bytes())?;
        Ok(())
    }

    fn row(
        &mut self,
        out: &mut dyn Write,
        _columns: &[ResultColumn],
        row: &[CellValue],
    ) -> std::io::Result<()> {
        out.write_all(b"\n")?;
        let line = row
            .iter()
            .map(|c| tsv_field_with(c, &self.opts))
            .collect::<Vec<_>>()
            .join("\t");
        out.write_all(line.as_bytes())?;
        Ok(())
    }

    fn end(&mut self, _out: &mut dyn Write) -> std::io::Result<()> {
        Ok(())
    }
}

struct CsvWriter {
    opts: ExportOptions,
}

impl RowWriter for CsvWriter {
    fn begin(&mut self, out: &mut dyn Write, columns: &[ResultColumn]) -> std::io::Result<()> {
        if !self.opts.header_row {
            return Ok(());
        }
        let header = columns
            .iter()
            .map(|c| csv_field_with(&CellValue::String(c.name.clone()), &self.opts))
            .collect::<Vec<_>>()
            .join(",");
        out.write_all(header.as_bytes())?;
        Ok(())
    }

    fn row(
        &mut self,
        out: &mut dyn Write,
        _columns: &[ResultColumn],
        row: &[CellValue],
    ) -> std::io::Result<()> {
        out.write_all(b"\n")?;
        let line = row
            .iter()
            .map(|c| csv_field_with(c, &self.opts))
            .collect::<Vec<_>>()
            .join(",");
        out.write_all(line.as_bytes())?;
        Ok(())
    }

    fn end(&mut self, _out: &mut dyn Write) -> std::io::Result<()> {
        Ok(())
    }
}

struct JsonLinesWriter {
    first_row: bool,
    names: Vec<String>,
}

impl JsonLinesWriter {
    fn new() -> Self {
        Self {
            first_row: true,
            names: Vec::new(),
        }
    }
}

impl RowWriter for JsonLinesWriter {
    fn begin(&mut self, _out: &mut dyn Write, _columns: &[ResultColumn]) -> std::io::Result<()> {
        Ok(())
    }

    fn row(
        &mut self,
        out: &mut dyn Write,
        columns: &[ResultColumn],
        row: &[CellValue],
    ) -> std::io::Result<()> {
        if self.first_row {
            self.names = unique_column_names(columns);
        } else {
            out.write_all(b"\n")?;
        }
        self.first_row = false;
        out.write_all(json_object_named(columns, &self.names, row).as_bytes())?;
        Ok(())
    }

    fn end(&mut self, _out: &mut dyn Write) -> std::io::Result<()> {
        Ok(())
    }
}

struct InsertWriter {
    db: DatabaseType,
    names: Vec<String>,
    pending: Option<String>,
    header_written: bool,
}

impl RowWriter for InsertWriter {
    fn begin(&mut self, _out: &mut dyn Write, _columns: &[ResultColumn]) -> std::io::Result<()> {
        Ok(())
    }

    fn row(
        &mut self,
        out: &mut dyn Write,
        columns: &[ResultColumn],
        row: &[CellValue],
    ) -> std::io::Result<()> {
        if !self.header_written {
            self.names = unique_column_names(columns);
            let col_list = self
                .names
                .iter()
                .map(|n| escape_identifier_for(self.db, n))
                .collect::<Vec<_>>()
                .join(", ");
            let table_escaped = escape_multipart_identifier_for(self.db, INSERT_TABLE);
            writeln!(out, "INSERT INTO {} ({})", table_escaped, col_list)?;
            writeln!(out, "VALUES")?;
            self.header_written = true;
        }
        if let Some(prev) = self.pending.take() {
            writeln!(out, "{prev},")?;
        }
        let values: Vec<String> = (0..self.names.len())
            .map(|ci| {
                sql_literal_in_column(
                    self.db,
                    columns.get(ci),
                    row.get(ci).unwrap_or(&CellValue::Null),
                )
            })
            .collect();
        self.pending = Some(format!("    ({})", values.join(", ")));
        Ok(())
    }

    fn end(&mut self, out: &mut dyn Write) -> std::io::Result<()> {
        if let Some(last) = self.pending.take() {
            write!(out, "{last};")?;
        }
        Ok(())
    }
}

/// Where the rows come from — captured on the UI thread, consumed on a worker.
pub enum ExportSource {
    /// Already-resident rows (non-spill sets, filtered/sorted order).
    Rows(Vec<Vec<CellValue>>),
    /// Spill file: the worker opens its own `SpillRowPager` and pages `total` rows.
    Spill { path: String, total: usize },
}

struct CountingWriter<W: Write> {
    inner: W,
    progress: TaskProgress,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.progress.add_bytes(n as u64);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn remove_partial(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// I/O and writer failures carry library text, not catalog wording. Export
/// work runs off the UI thread, so every message here is a
/// [`DisplayMessage`] the UI resolves with `localization::message`.
fn raw_error(error: impl std::fmt::Display) -> DisplayMessage {
    DisplayMessage::raw(error.to_string())
}

fn open_export_sink(
    path: &Path,
    opts: &ExportOptions,
    progress: TaskProgress,
) -> Result<CountingWriter<Box<dyn Write + Send>>, DisplayMessage> {
    // Scheduled exports and table exports name a file inside a folder that
    // may not exist yet.
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| {
            crate::localization::deferred(
                "results.export_stream.create_directory_failed",
                &[
                    ("directory_path", &parent.display().to_string()),
                    ("error", &e.to_string()),
                ],
            )
        })?;
    }
    let file = std::fs::File::create(path).map_err(raw_error)?;
    let inner: Box<dyn Write + Send> = if opts.gzip {
        Box::new(BufWriter::new(flate2::write::GzEncoder::new(
            file,
            flate2::Compression::default(),
        )))
    } else {
        Box::new(BufWriter::new(file))
    };
    Ok(CountingWriter { inner, progress })
}

/// Blocking; run on a background thread. Writes through a `BufWriter`,
/// checks `progress.is_cancelled()` between pages, bumps rows/bytes, and
/// on cancel/error removes the partial file. Returns rows written.
/// Byte counts in the task chip are compressed size when `opts.gzip` is on.
pub fn run_export(
    path: &Path,
    format: ExportFormat,
    db: DatabaseType,
    columns: &[ResultColumn],
    source: ExportSource,
    progress: &TaskProgress,
    opts: &ExportOptions,
) -> Result<u64, DisplayMessage> {
    let final_path = gz_path(path, opts.gzip);
    let mut writer = writer_for(format, db, opts.clone()).expect("streamable format has a writer");

    let mut out = open_export_sink(&final_path, opts, progress.clone())?;

    let mut rows_written = 0u64;

    match source {
        ExportSource::Rows(rows) => {
            if format == ExportFormat::Insert && rows.is_empty() {
                return Ok(0);
            }
            writer.begin(&mut out, columns).map_err(raw_error)?;
            for row in &rows {
                if progress.is_cancelled() {
                    remove_partial(&final_path);
                    return Err(DisplayMessage::raw(CANCELLED));
                }
                writer.row(&mut out, columns, row).map_err(raw_error)?;
                rows_written += 1;
                progress.add_rows(1);
            }
            writer.end(&mut out).map_err(raw_error)?;
        }
        ExportSource::Spill {
            path: spill_path,
            total,
        } => {
            if format == ExportFormat::Insert && total == 0 {
                return Ok(0);
            }
            let mut pager = SpillRowPager::open(&spill_path).ok_or_else(|| {
                crate::localization::deferred_text("results.export_stream.spill_open_failed")
            })?;
            let mut offset = 0;
            let mut begun = false;
            while offset < total {
                if progress.is_cancelled() {
                    remove_partial(&final_path);
                    return Err(DisplayMessage::raw(CANCELLED));
                }
                let len = PAGE_ROWS.min(total - offset);
                let page = pager.page(offset, len);
                if offset == 0 && !begun {
                    writer.begin(&mut out, columns).map_err(raw_error)?;
                    begun = true;
                }
                for row in page {
                    writer.row(&mut out, columns, &row).map_err(raw_error)?;
                    rows_written += 1;
                    progress.add_rows(1);
                }
                offset += len;
            }
            if !begun {
                if format == ExportFormat::Insert {
                    return Ok(0);
                }
                writer.begin(&mut out, columns).map_err(raw_error)?;
                begun = true;
            }
            if begun {
                writer.end(&mut out).map_err(raw_error)?;
            }
        }
    }

    out.flush().map_err(raw_error)?;
    Ok(rows_written)
}

/// Streams each table's `SELECT *` through the row writer into its target
/// file. Errors are collected per table; cancel stops after the current table.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
pub async fn run_table_exports(
    services: sqlly_services::Services,
    mut profile: sqlly_core::ConnectionProfile,
    database: String,
    db: DatabaseType,
    targets: Vec<TableExportTarget>,
    format: ExportFormat,
    opts: ExportOptions,
    progress: TaskProgress,
    mut on_table: impl FnMut(usize, &str) + Send,
) -> Vec<(TableExportTarget, Result<u64, DisplayMessage>)> {
    profile.database = Some(database);
    let mut results = Vec::new();
    for (index, target) in targets.into_iter().enumerate() {
        if progress.is_cancelled() {
            results.push((target, Err(DisplayMessage::raw(CANCELLED))));
            break;
        }
        on_table(index, &target.table);
        let outcome =
            export_query_to_file(&services, &profile, &target, format, db, &opts, &progress).await;
        let stop = outcome.as_ref().err().is_some_and(|e| *e == CANCELLED);
        results.push((target, outcome));
        if stop {
            break;
        }
    }
    results
}

#[cfg(not(target_arch = "wasm32"))]
async fn export_query_to_file(
    services: &sqlly_services::Services,
    profile: &sqlly_core::ConnectionProfile,
    target: &TableExportTarget,
    format: ExportFormat,
    db: DatabaseType,
    opts: &ExportOptions,
    progress: &TaskProgress,
) -> Result<u64, DisplayMessage> {
    let path = &target.path;
    let mut writer = writer_for(format, db, opts.clone()).ok_or_else(|| {
        crate::localization::deferred(
            "results.export_stream.not_streamable_format",
            &[("format_name", &format.localized_label())],
        )
    })?;
    let mut out = open_export_sink(path, opts, progress.clone())?;
    // Full values: an export writes every cell whole, never the 8 KiB
    // preview the grid paints. Rows past the spill threshold still stream
    // through the spill file, so memory stays bounded.
    let handle = match services
        .queries
        .execute_full_values(profile, None, &target.sql, 256)
        .await
    {
        Ok(handle) => handle,
        Err(err) => {
            remove_partial(path);
            return Err(err.message());
        }
    };
    let mut events = handle.events;
    match stream_query_events(&mut events, writer.as_mut(), &mut out, progress).await {
        Ok(rows) => {
            out.flush().map_err(raw_error)?;
            Ok(rows)
        }
        Err(stop) => {
            let _ = services.queries.cancel(&handle.task_id).await;
            drop(out);
            remove_partial(path);
            Err(match stop {
                StreamStop::Cancelled => DisplayMessage::raw(CANCELLED),
                StreamStop::Failed(message) => message,
            })
        }
    }
}

/// Why [`stream_query_events`] stopped early.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, PartialEq, Eq)]
enum StreamStop {
    /// The task chip's cancel was pressed.
    Cancelled,
    Failed(DisplayMessage),
}

/// Write one query's single result set through `writer`, row by row.
///
/// Rows past the executor's spill threshold (or byte budget) are not
/// forwarded as `Rows`; they arrive only in the spill file named by the
/// final `SpillManifest`, so the tail is paged from that file — without it
/// an export silently stopped at the threshold. A second result set is
/// refused rather than appended under the first set's header, and a query
/// that ends without `Completed` is an error, not a short file.
#[cfg(not(target_arch = "wasm32"))]
async fn stream_query_events(
    events: &mut tokio::sync::mpsc::Receiver<QueryEvent>,
    writer: &mut (dyn RowWriter + Send),
    out: &mut (dyn Write + Send),
    progress: &TaskProgress,
) -> Result<u64, StreamStop> {
    let io = |e: std::io::Error| StreamStop::Failed(raw_error(e));
    let mut columns: Vec<ResultColumn> = Vec::new();
    // The result set being exported (the first one that has columns).
    let mut export_index: Option<usize> = None;
    // Rows of that set received as `Rows` events (forwarded prefix).
    let mut forwarded = 0usize;
    let mut rows_written = 0u64;
    let mut completed = false;
    let multiple = || {
        StreamStop::Failed(crate::localization::deferred_text(
            "results.export_stream.multiple_result_sets",
        ))
    };
    while let Some(event) = events.recv().await {
        if progress.is_cancelled() {
            return Err(StreamStop::Cancelled);
        }
        match event {
            QueryEvent::ResultSet {
                index,
                columns: cols,
            } => {
                if cols.is_empty() {
                    continue;
                }
                match export_index {
                    None => {
                        export_index = Some(index);
                        columns = cols;
                        writer.begin(out, &columns).map_err(io)?;
                    }
                    Some(current) if current == index => {}
                    Some(_) => return Err(multiple()),
                }
            }
            QueryEvent::Rows { index, rows } => {
                match export_index {
                    None => {
                        return Err(StreamStop::Failed(crate::localization::deferred_text(
                            "results.export_stream.rows_before_metadata",
                        )));
                    }
                    Some(current) if current != index => return Err(multiple()),
                    Some(_) => {}
                }
                for row in rows {
                    if progress.is_cancelled() {
                        return Err(StreamStop::Cancelled);
                    }
                    writer.row(out, &columns, &row).map_err(io)?;
                    forwarded += 1;
                    rows_written += 1;
                    progress.add_rows(1);
                }
            }
            QueryEvent::SpillManifest { manifest } => {
                let mut outcome = Ok(());
                for spilled in &manifest.result_sets {
                    if outcome.is_ok() && Some(spilled.result_set_index) == export_index {
                        outcome = write_spilled_tail(
                            spilled,
                            forwarded,
                            writer,
                            out,
                            &columns,
                            progress,
                            &mut rows_written,
                        );
                    }
                    // This consumer owns the files once the manifest arrives.
                    remove_spill_files(&spilled.spill_file);
                }
                outcome?;
            }
            QueryEvent::Error { message } => {
                return Err(StreamStop::Failed(DisplayMessage::raw(message)));
            }
            QueryEvent::Cancelled => {
                return Err(StreamStop::Failed(crate::localization::deferred_text(
                    "results.export_stream.cancelled_before_finish",
                )));
            }
            QueryEvent::Completed { .. } => {
                completed = true;
                break;
            }
            _ => {}
        }
    }
    if !completed {
        return Err(StreamStop::Failed(crate::localization::deferred_text(
            "results.export_stream.stopped_before_completion",
        )));
    }
    if export_index.is_some() {
        writer.end(out).map_err(io)?;
    }
    Ok(rows_written)
}

/// Page rows `forwarded..row_count` of a spilled set from its file.
#[cfg(not(target_arch = "wasm32"))]
fn write_spilled_tail(
    spilled: &sqlly_services::query::SpillResultSetData,
    forwarded: usize,
    writer: &mut (dyn RowWriter + Send),
    out: &mut (dyn Write + Send),
    columns: &[ResultColumn],
    progress: &TaskProgress,
    rows_written: &mut u64,
) -> Result<(), StreamStop> {
    if spilled.row_count <= forwarded {
        return Ok(());
    }
    let mut pager = SpillRowPager::open(&spilled.spill_file).ok_or_else(|| {
        StreamStop::Failed(crate::localization::deferred_text(
            "results.export_stream.tail_spill_open_failed",
        ))
    })?;
    let mut offset = forwarded;
    while offset < spilled.row_count {
        if progress.is_cancelled() {
            return Err(StreamStop::Cancelled);
        }
        let len = PAGE_ROWS.min(spilled.row_count - offset);
        let page = pager.page(offset, len);
        if page.len() != len {
            return Err(StreamStop::Failed(crate::localization::deferred(
                "results.export_stream.spill_ended_early",
                &[
                    ("offset", &offset.to_string()),
                    ("total_rows", &spilled.row_count.to_string()),
                ],
            )));
        }
        for row in page {
            writer
                .row(out, columns, &row)
                .map_err(|e| StreamStop::Failed(raw_error(e)))?;
            *rows_written += 1;
            progress.add_rows(1);
        }
        offset += len;
    }
    Ok(())
}

/// Delete a spill file and its `.idx` sidecar (readers must be dropped
/// first on Windows).
#[cfg(not(target_arch = "wasm32"))]
fn remove_spill_files(spill_file: &str) {
    let _ = std::fs::remove_file(format!("{spill_file}.idx"));
    let _ = std::fs::remove_file(spill_file);
}

/// Streams the first result set of `sql` to `path` as CSV or JSON Lines —
/// the scheduled query-export path. Runs untracked (no task chip): the
/// scheduler surfaces progress through the schedule's last-result line.
#[cfg(not(target_arch = "wasm32"))]
pub async fn export_sql_to_file(
    services: &sqlly_services::Services,
    profile: &sqlly_core::ConnectionProfile,
    sql: &str,
    path: &Path,
    format: ExportFormat,
    db: DatabaseType,
    opts: &ExportOptions,
) -> Result<u64, DisplayMessage> {
    let target = TableExportTarget {
        schema: String::new(),
        table: String::new(),
        sql: sql.to_string(),
        path: path.to_path_buf(),
    };
    export_query_to_file(
        services,
        profile,
        &target,
        format,
        db,
        opts,
        &TaskProgress::default(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::ResultColumn;

    fn sample_columns() -> Vec<ResultColumn> {
        vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            },
            ResultColumn {
                name: "active".into(),
                data_type: "bit".into(),
                nullable: false,
            },
        ]
    }

    fn sample_rows(n: usize) -> Vec<Vec<CellValue>> {
        (0..n)
            .map(|i| {
                vec![
                    CellValue::I64(i as i64),
                    CellValue::String(format!("row{i}")),
                    CellValue::Bool(i % 2 == 0),
                ]
            })
            .collect()
    }

    /// Unique per call: parallel tests sharing a tag can read the same clock
    /// value (macOS resolves to microseconds), so a counter disambiguates.
    fn temp_export_path(tag: &str) -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "sqlly_export_{}_{}_{}_{}.out",
            tag,
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        path
    }

    fn default_csv_opts() -> ExportOptions {
        ExportOptions::default_for(ExportFormat::Csv)
    }

    #[test]
    fn stream_csv_matches_whole_document_renderer() {
        let cols = sample_columns();
        let rows = sample_rows(10);
        let path = temp_export_path("csv");
        let progress = TaskProgress::default();
        let opts = default_csv_opts();
        run_export(
            &path,
            ExportFormat::Csv,
            DatabaseType::SqlServer,
            &cols,
            ExportSource::Rows(rows.clone()),
            &progress,
            &opts,
        )
        .unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        let want = crate::result_export::csv(&cols, &rows, true);
        assert_eq!(got, want);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn stream_tsv_matches() {
        let cols = sample_columns();
        let rows = sample_rows(10);
        let path = temp_export_path("tsv");
        let progress = TaskProgress::default();
        let opts = ExportOptions::default_for(ExportFormat::Tsv);
        run_export(
            &path,
            ExportFormat::Tsv,
            DatabaseType::SqlServer,
            &cols,
            ExportSource::Rows(rows.clone()),
            &progress,
            &opts,
        )
        .unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        let want = crate::result_export::tsv(&cols, &rows, true);
        assert_eq!(got, want);
        std::fs::remove_file(&path).ok();
    }

    fn formula_rows() -> Vec<Vec<CellValue>> {
        vec![
            vec![
                CellValue::I64(-1),
                CellValue::String("=1+1".into()),
                CellValue::Bool(true),
            ],
            vec![
                CellValue::I64(2),
                CellValue::String("-12.5".into()),
                CellValue::Bool(false),
            ],
        ]
    }

    #[test]
    fn stream_csv_guards_spreadsheet_formulas() {
        let cols = sample_columns();
        let path = temp_export_path("csv_formula");
        let progress = TaskProgress::default();
        run_export(
            &path,
            ExportFormat::Csv,
            DatabaseType::SqlServer,
            &cols,
            ExportSource::Rows(formula_rows()),
            &progress,
            &default_csv_opts(),
        )
        .unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(got, "id,name,active\n-1,\"'=1+1\",true\n2,-12.5,false");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn stream_tsv_guards_spreadsheet_formulas_unless_off() {
        let cols = sample_columns();
        for (formula_safe, want) in [
            (true, "id\tname\tactive\n-1\t'=1+1\ttrue\n2\t-12.5\tfalse"),
            (false, "id\tname\tactive\n-1\t=1+1\ttrue\n2\t-12.5\tfalse"),
        ] {
            let path = temp_export_path("tsv_formula");
            let progress = TaskProgress::default();
            let opts = ExportOptions {
                formula_safe,
                ..ExportOptions::default_for(ExportFormat::Tsv)
            };
            run_export(
                &path,
                ExportFormat::Tsv,
                DatabaseType::SqlServer,
                &cols,
                ExportSource::Rows(formula_rows()),
                &progress,
                &opts,
            )
            .unwrap();
            let got = std::fs::read_to_string(&path).unwrap();
            assert_eq!(got, want, "formula_safe = {formula_safe}");
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn stream_json_lines_matches() {
        let cols = sample_columns();
        let rows = sample_rows(10);
        let path = temp_export_path("jsonl");
        let progress = TaskProgress::default();
        let opts = ExportOptions::default_for(ExportFormat::JsonLines);
        run_export(
            &path,
            ExportFormat::JsonLines,
            DatabaseType::SqlServer,
            &cols,
            ExportSource::Rows(rows.clone()),
            &progress,
            &opts,
        )
        .unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        let want = crate::result_export::json_lines(&cols, &rows);
        assert_eq!(got, want);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn stream_json_lines_keeps_every_column_when_names_repeat() {
        let cols: Vec<ResultColumn> = (0..3)
            .map(|_| ResultColumn {
                name: "num".into(),
                data_type: "int".into(),
                nullable: false,
            })
            .collect();
        let rows = vec![vec![CellValue::I64(1), CellValue::I64(2), CellValue::I64(3)]; 2];
        let path = temp_export_path("jsonl");
        let progress = TaskProgress::default();
        let opts = ExportOptions::default_for(ExportFormat::JsonLines);
        run_export(
            &path,
            ExportFormat::JsonLines,
            DatabaseType::SqlServer,
            &cols,
            ExportSource::Rows(rows),
            &progress,
            &opts,
        )
        .unwrap();
        let got = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            got,
            "{\"num\":1,\"num_2\":2,\"num_3\":3}\n{\"num\":1,\"num_2\":2,\"num_3\":3}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn stream_insert_matches_for_sql_server_and_postgres() {
        let cols = sample_columns();
        let rows = sample_rows(3);
        for db in [DatabaseType::SqlServer, DatabaseType::Postgres] {
            let path = temp_export_path("insert");
            let progress = TaskProgress::default();
            let opts = ExportOptions::default_for(ExportFormat::Insert);
            run_export(
                &path,
                ExportFormat::Insert,
                db,
                &cols,
                ExportSource::Rows(rows.clone()),
                &progress,
                &opts,
            )
            .unwrap();
            let got = std::fs::read_to_string(&path).unwrap();
            let want = crate::result_export::insert_statements_for(db, &cols, &rows, INSERT_TABLE);
            assert_eq!(got, want, "insert export for {db:?}");
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn spill_source_pages_every_row() {
        let mut spill_path = std::env::temp_dir();
        spill_path.push(format!(
            "sqlly_spill_export_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut w = sqlly_core::SpillWriter::create(&spill_path).unwrap();
        for i in 0..5000i64 {
            w.append(&[
                CellValue::I64(i),
                CellValue::String(format!("row{i}")),
                CellValue::Bool(i % 2 == 0),
            ])
            .unwrap();
        }
        w.finish().unwrap();

        let cols = sample_columns();
        let out_path = temp_export_path("spill_csv");
        let progress = TaskProgress::default();
        let opts = default_csv_opts();
        run_export(
            &out_path,
            ExportFormat::Csv,
            DatabaseType::SqlServer,
            &cols,
            ExportSource::Spill {
                path: spill_path.display().to_string(),
                total: 5000,
            },
            &progress,
            &opts,
        )
        .unwrap();
        let text = std::fs::read_to_string(&out_path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 5001, "header + 5000 data rows");
        std::fs::remove_file(&out_path).ok();
        std::fs::remove_file(&spill_path).ok();
        std::fs::remove_file(format!("{}.idx", spill_path.display())).ok();
    }

    #[test]
    fn cancel_between_pages_removes_partial_file() {
        let cols = sample_columns();
        let rows = sample_rows(100);
        let path = temp_export_path("cancel");
        let progress = TaskProgress::default();
        progress.cancel();
        let opts = default_csv_opts();
        let err = run_export(
            &path,
            ExportFormat::Csv,
            DatabaseType::SqlServer,
            &cols,
            ExportSource::Rows(rows),
            &progress,
            &opts,
        )
        .unwrap_err();
        assert_eq!(err, CANCELLED);
        assert!(!path.exists());
    }

    #[test]
    fn gzip_export_starts_with_magic_and_decompresses() {
        use std::io::Read;

        let cols = sample_columns();
        let rows = sample_rows(3);
        let path = temp_export_path("gzip");
        let progress = TaskProgress::default();
        let mut opts = default_csv_opts();
        opts.gzip = true;
        run_export(
            &path,
            ExportFormat::Csv,
            DatabaseType::SqlServer,
            &cols,
            ExportSource::Rows(rows.clone()),
            &progress,
            &opts,
        )
        .unwrap();
        let gz_path = gz_path(&path, true);
        let bytes = std::fs::read(&gz_path).unwrap();
        assert_eq!(bytes[0], 0x1f);
        assert_eq!(bytes[1], 0x8b);
        let mut dec = flate2::read::GzDecoder::new(bytes.as_slice());
        let mut plain = String::new();
        dec.read_to_string(&mut plain).unwrap();
        let want = crate::result_export::csv(&cols, &rows, true);
        assert_eq!(plain, want);
        std::fs::remove_file(&gz_path).ok();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn csv_writer() -> Box<dyn RowWriter + Send> {
        writer_for(
            ExportFormat::Csv,
            DatabaseType::Postgres,
            ExportOptions::default_for(ExportFormat::Csv),
        )
        .expect("csv writer")
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn stream(events: Vec<QueryEvent>) -> (Result<u64, StreamStop>, String) {
        let (tx, mut rx) = tokio::sync::mpsc::channel(events.len().max(1));
        for event in events {
            tx.send(event).await.expect("send");
        }
        drop(tx);
        let mut writer = csv_writer();
        let mut out = Vec::new();
        let result =
            stream_query_events(&mut rx, writer.as_mut(), &mut out, &TaskProgress::default()).await;
        (result, String::from_utf8(out).expect("utf8"))
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn spilled_tail_is_exported_and_its_files_removed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let spill = dir.path().join("set0.spill");
        let rows = sample_rows(5);
        let mut spill_writer = sqlly_core::SpillWriter::create(&spill).expect("spill");
        for row in &rows {
            spill_writer.append(row).expect("append");
        }
        drop(spill_writer.finish().expect("finish"));
        let manifest = sqlly_services::query::SpillManifestData {
            result_sets: vec![sqlly_services::query::SpillResultSetData {
                result_set_index: 0,
                columns: sample_columns(),
                row_count: 5,
                spill_file: spill.display().to_string(),
            }],
            messages: Vec::new(),
            error: None,
            elapsed_ms: None,
        };
        // Only the first two rows were forwarded before suppression.
        let (result, text) = stream(vec![
            QueryEvent::ResultSet {
                index: 0,
                columns: sample_columns(),
            },
            QueryEvent::Rows {
                index: 0,
                rows: rows[..2].to_vec(),
            },
            QueryEvent::SpillManifest { manifest },
            QueryEvent::Completed { elapsed_ms: 1 },
        ])
        .await;
        assert_eq!(result, Ok(5), "all five rows, not just the forwarded two");
        let want = crate::result_export::csv(&sample_columns(), &rows, true);
        assert_eq!(text.replace("\r\n", "\n"), want.replace("\r\n", "\n"));
        assert!(!spill.exists(), "spill file removed");
        assert!(!dir.path().join("set0.spill.idx").exists(), "index removed");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn second_result_set_is_refused_not_appended() {
        let (result, _) = stream(vec![
            QueryEvent::ResultSet {
                index: 0,
                columns: sample_columns(),
            },
            QueryEvent::Rows {
                index: 0,
                rows: sample_rows(1),
            },
            QueryEvent::ResultSet {
                index: 1,
                columns: sample_columns()[..1].to_vec(),
            },
            QueryEvent::Completed { elapsed_ms: 1 },
        ])
        .await;
        match result {
            Err(StreamStop::Failed(message)) => {
                assert!(message.contains("more than one result set"), "{message}")
            }
            other => panic!("expected refusal, got {other:?}"),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn server_cancel_or_missing_completion_is_an_error() {
        let header = QueryEvent::ResultSet {
            index: 0,
            columns: sample_columns(),
        };
        let (result, _) = stream(vec![header.clone(), QueryEvent::Cancelled]).await;
        assert!(matches!(result, Err(StreamStop::Failed(_))));
        let (result, _) = stream(vec![header]).await;
        assert!(matches!(result, Err(StreamStop::Failed(_))));
    }

    /// Table and scheduled query exports run the query in full-values mode:
    /// a 20 KiB cell is written whole, not as the grid's 8 KiB preview.
    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn query_export_writes_a_20_kib_cell_whole() {
        let mut profile = sqlly_core::ConnectionProfile::sql_login(
            "sqlite-export-test",
            ":memory:",
            "",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = sqlly_core::DatabaseType::Sqlite;
        let services = sqlly_services::Services::new();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big.csv");
        let rows = export_sql_to_file(
            &services,
            &profile,
            "SELECT 1 AS id, replace(hex(zeroblob(10240)), '00', 'ab') AS body",
            &path,
            ExportFormat::Csv,
            DatabaseType::Sqlite,
            &default_csv_opts(),
        )
        .await
        .expect("export");
        assert_eq!(rows, 1);
        let text = std::fs::read_to_string(&path).expect("export file");
        let whole = "ab".repeat(10 * 1024);
        assert!(
            text.contains(&whole),
            "the 20 KiB cell must be exported whole ({} bytes written)",
            text.len()
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn export_sink_creates_the_missing_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nightly").join("out.csv");
        let sink = open_export_sink(&path, &default_csv_opts(), TaskProgress::default());
        assert!(sink.is_ok());
        assert!(path.exists());
    }

    #[test]
    fn gz_path_appends_extension() {
        assert_eq!(gz_path(Path::new("a.csv"), true), PathBuf::from("a.csv.gz"));
        assert_eq!(
            gz_path(Path::new("a.csv.gz"), true),
            PathBuf::from("a.csv.gz")
        );
    }
}
