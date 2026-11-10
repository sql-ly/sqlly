use std::io::Read;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use serde::Serialize;
use sqlly_core::{ConnectionProfile, StudioError};
use sqlly_lang::sqllex::{ScriptUnit, StreamSplitter, strip_leading_comments_in};
use sqlly_lang::sqlmodel::Dialect;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;

use crate::query::{QueryEvent, QueryExecutor};
use crate::script_runner::reader::ChunkReader;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    Stop,
    Continue,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionMode {
    None,
    Single,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ErrorDecision {
    Retry,
    Skip,
    SkipAll,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptRunOptions {
    pub dialect: Dialect,
    pub on_error: OnError,
    pub transaction: TransactionMode,
    pub coalesce_inserts: bool,
    pub session_key: String,
    pub coalesce_max_rows: usize,
    pub coalesce_max_bytes: usize,
    /// MySQL / MariaDB: remove or remap `DEFINER=` clauses before each
    /// statement is split and run. `None` runs the script as written.
    pub definer: Option<crate::mysql_definer::DefinerMode>,
}

impl ScriptRunOptions {
    pub fn coalesce_max_rows(&self) -> usize {
        self.coalesce_max_rows.max(1)
    }

    pub fn coalesce_max_bytes(&self) -> usize {
        self.coalesce_max_bytes.max(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptProgress {
    pub units_done: usize,
    pub units_failed: usize,
    pub units_skipped: usize,
    pub bytes_read: u64,
    pub total_bytes: u64,
    pub rows_affected: u64,
    pub current_line: u32,
    pub last_message: Option<String>,
    pub terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScriptUnitFailure {
    pub ordinal: usize,
    pub start_line: u32,
    pub message: String,
    pub decision: ErrorDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScriptOutcome {
    pub units_total: usize,
    pub units_done: usize,
    pub units_failed: usize,
    pub units_skipped: usize,
    pub rows_affected: u64,
    pub elapsed_ms: u128,
    pub failures: Vec<ScriptUnitFailure>,
    pub messages: Vec<String>,
    pub cancelled: bool,
    pub stopped_on_error: bool,
    pub database_changed_to: Option<String>,
}

const MESSAGE_CAP: usize = 2_000;
const MAX_RETRIES: u32 = 5;

/// Cooperative cancel handle shared between the runner and its caller.
#[derive(Clone, Default)]
pub struct ScriptCancel(Arc<AtomicBool>);

impl ScriptCancel {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

pub fn decide_after_error(
    policy: OnError,
    asked: Option<ErrorDecision>,
    skip_all: bool,
    consecutive_retries: u32,
) -> ErrorDecision {
    if skip_all {
        return ErrorDecision::Skip;
    }
    if consecutive_retries >= MAX_RETRIES {
        return ErrorDecision::Stop;
    }
    match policy {
        OnError::Stop => ErrorDecision::Stop,
        OnError::Continue => ErrorDecision::Skip,
        OnError::Ask => asked.unwrap_or(ErrorDecision::Stop),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressThrottle {
    last_sent: Option<Instant>,
    min_interval: Duration,
}

impl ProgressThrottle {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            last_sent: None,
            min_interval,
        }
    }

    pub fn should_send(&mut self, now: Instant, force: bool) -> bool {
        if force {
            self.last_sent = Some(now);
            return true;
        }
        match self.last_sent {
            None => {
                self.last_sent = Some(now);
                true
            }
            Some(last) if now.duration_since(last) >= self.min_interval => {
                self.last_sent = Some(now);
                true
            }
            _ => false,
        }
    }
}

pub fn begin_sql(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::SqlServer => "BEGIN TRANSACTION",
        _ => "BEGIN",
    }
}

pub fn commit_sql() -> &'static str {
    "COMMIT"
}

pub fn rollback_sql() -> &'static str {
    "ROLLBACK"
}

// Cohesive step signature; grouping args into a struct adds no clarity here.
#[allow(clippy::too_many_arguments)]
fn progress_snapshot(
    units_done: usize,
    units_failed: usize,
    units_skipped: usize,
    bytes_read: u64,
    total_bytes: u64,
    rows_affected: u64,
    current_line: u32,
    last_message: Option<String>,
    terminal: bool,
) -> ScriptProgress {
    ScriptProgress {
        units_done,
        units_failed,
        units_skipped,
        bytes_read,
        total_bytes,
        rows_affected,
        current_line,
        last_message,
        terminal,
    }
}

async fn send_progress(
    tx: &mpsc::Sender<ScriptProgress>,
    throttle: &mut ProgressThrottle,
    force: bool,
    snap: ScriptProgress,
) {
    if throttle.should_send(Instant::now(), force) {
        let _ = tx.send(snap).await;
    }
}

pub fn coalesce_inserts(
    dialect: Dialect,
    units: &[ScriptUnit],
    max_rows: usize,
    max_bytes: usize,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < units.len() {
        if let Some((sql, consumed)) = try_coalesce_group(dialect, &units[i..], max_rows, max_bytes)
        {
            out.push(sql);
            i += consumed;
        } else {
            out.push(units[i].text.clone());
            i += 1;
        }
    }
    out
}

fn try_coalesce_group(
    dialect: Dialect,
    units: &[ScriptUnit],
    max_rows: usize,
    max_bytes: usize,
) -> Option<(String, usize)> {
    // Comments above the first INSERT lead the merged batch; a later INSERT
    // with its own comment starts a new batch so its comment is kept.
    let first_text = &units[0].text;
    let first_body = strip_leading_comments_in(dialect, first_text);
    let comments = first_text[..first_text.len() - first_body.len()].to_string();
    let first = parse_insert_prefix(first_body)?;
    let mut tuples = vec![first.values];
    let prefix = first.prefix;
    let mut count = 1usize;
    for unit in units.iter().skip(1) {
        if strip_leading_comments_in(dialect, &unit.text).len() != unit.text.trim_start().len() {
            break;
        }
        let parsed = parse_insert_prefix(&unit.text)?;
        if parsed.prefix != prefix {
            break;
        }
        tuples.push(parsed.values);
        count += 1;
        if count > max_rows {
            tuples.pop();
            count -= 1;
            break;
        }
        let joined = format!("{prefix} {}", tuples.join(", "));
        if joined.len() > max_bytes {
            tuples.pop();
            count -= 1;
            break;
        }
    }
    if count <= 1 {
        return None;
    }
    Some((format!("{comments}{prefix} {}", tuples.join(", ")), count))
}

struct InsertParts {
    prefix: String,
    values: String,
}

fn parse_insert_prefix(sql: &str) -> Option<InsertParts> {
    let trimmed = sql.trim();
    let upper = trimmed.to_ascii_uppercase();
    if !upper.starts_with("INSERT INTO") {
        return None;
    }
    let values_idx = upper.rfind(" VALUES")?;
    if upper[values_idx..].contains("SELECT") {
        return None;
    }
    let prefix = normalize_ws(&trimmed[..values_idx + " VALUES".len()]);
    let values = trimmed[values_idx + " VALUES".len()..]
        .trim()
        .trim_end_matches(';')
        .trim();
    if !values.starts_with('(') {
        return None;
    }
    Some(InsertParts {
        prefix,
        values: values.to_string(),
    })
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn discard_spill_manifest(manifest: &crate::query::SpillManifestData) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        for rs in &manifest.result_sets {
            std::fs::remove_file(&rs.spill_file).ok();
            std::fs::remove_file(format!("{}.idx", rs.spill_file)).ok();
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = manifest;
    }
}

// Cohesive step signature; grouping args into a struct adds no clarity here.
#[allow(clippy::too_many_arguments)]
pub async fn run_script<R: Read + Send>(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    mut reader: ChunkReader<R>,
    total_bytes: u64,
    options: &ScriptRunOptions,
    progress: mpsc::Sender<ScriptProgress>,
    cancel: ScriptCancel,
    mut on_error: impl FnMut(
        &ScriptUnitFailure,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = ErrorDecision> + Send + '_>,
    > + Send,
) -> Result<ScriptOutcome, StudioError> {
    let started = Instant::now();
    let mut splitter = StreamSplitter::new(options.dialect);
    let mut pending_units: Vec<ScriptUnit> = Vec::new();
    let mut units_done = 0usize;
    let mut units_failed = 0usize;
    let mut units_skipped = 0usize;
    let mut rows_affected = 0u64;
    let mut failures = Vec::new();
    let mut messages = Vec::new();
    let mut database_changed_to = None::<String>;
    let mut cancelled = false;
    let mut stopped_on_error = false;
    let mut skip_all = false;
    let mut throttle = ProgressThrottle::new(Duration::from_millis(100));
    let mut in_transaction = false;
    let mut current_line = 1u32;

    send_progress(
        &progress,
        &mut throttle,
        false,
        progress_snapshot(
            units_done,
            units_failed,
            units_skipped,
            reader.bytes_read.min(total_bytes),
            total_bytes,
            rows_affected,
            current_line,
            messages.last().cloned(),
            false,
        ),
    )
    .await;

    if options.transaction == TransactionMode::Single {
        execute_unit(
            queries,
            profile,
            &options.session_key,
            begin_sql(options.dialect),
            &cancel,
            false,
            &mut rows_affected,
            &mut messages,
            &mut database_changed_to,
        )
        .await
        .map_err(StudioError::InvalidInput)?;
        in_transaction = true;
    }

    if cancel.is_cancelled() {
        cancelled = true;
    }

    let mut definer = options
        .definer
        .map(crate::mysql_definer::DefinerRewriter::new);
    while let Some(chunk) = reader.next_chunk()? {
        if cancel.is_cancelled() {
            cancelled = true;
            break;
        }
        let chunk = match definer.as_mut() {
            Some(rewriter) => {
                String::from_utf8_lossy(&rewriter.push(chunk.as_bytes())).into_owned()
            }
            None => chunk,
        };
        let mut fresh = Vec::new();
        splitter.push(&chunk, &mut fresh);
        pending_units.extend(fresh);
    }

    if !cancelled {
        if let Some(rewriter) = definer.as_mut() {
            let tail = String::from_utf8_lossy(&rewriter.finish()).into_owned();
            let mut fresh = Vec::new();
            splitter.push(&tail, &mut fresh);
            pending_units.extend(fresh);
        }
        splitter
            .finish(&mut pending_units)
            .map_err(StudioError::InvalidInput)?;
    }

    let units = std::mem::take(&mut pending_units);
    let units_total = units.len();

    let sql_units = if options.coalesce_inserts && options.dialect != Dialect::SqlServer {
        coalesce_inserts(
            options.dialect,
            &units,
            options.coalesce_max_rows(),
            options.coalesce_max_bytes(),
        )
    } else {
        units.iter().map(|u| u.text.clone()).collect()
    };

    'units: for (idx, sql) in sql_units.into_iter().enumerate() {
        if cancel.is_cancelled() {
            cancelled = true;
            break;
        }
        let unit_meta = units.get(idx).or_else(|| units.last());
        let ordinal = unit_meta.map(|u| u.ordinal).unwrap_or(idx);
        let start_line = unit_meta.map(|u| u.start_line).unwrap_or(1);
        current_line = unit_meta.map(|u| u.end_line).unwrap_or(current_line);

        let mut consecutive_retries = 0u32;
        loop {
            if cancel.is_cancelled() {
                cancelled = true;
                break 'units;
            }
            match execute_unit(
                queries,
                profile,
                &options.session_key,
                &sql,
                &cancel,
                true,
                &mut rows_affected,
                &mut messages,
                &mut database_changed_to,
            )
            .await
            {
                Ok(()) => {
                    units_done += 1;
                    send_progress(
                        &progress,
                        &mut throttle,
                        false,
                        progress_snapshot(
                            units_done,
                            units_failed,
                            units_skipped,
                            reader.bytes_read.min(total_bytes),
                            total_bytes,
                            rows_affected,
                            current_line,
                            messages.last().cloned(),
                            false,
                        ),
                    )
                    .await;
                    break;
                }
                Err(message) => {
                    units_failed += 1;
                    let failure = ScriptUnitFailure {
                        ordinal,
                        start_line,
                        message: message.clone(),
                        decision: ErrorDecision::Stop,
                    };
                    let asked = if skip_all {
                        Some(ErrorDecision::Skip)
                    } else if options.on_error == OnError::Ask {
                        Some(on_error(&failure).await)
                    } else {
                        None
                    };
                    let decision =
                        decide_after_error(options.on_error, asked, skip_all, consecutive_retries);
                    if matches!(decision, ErrorDecision::SkipAll) {
                        skip_all = true;
                    }
                    match decision {
                        ErrorDecision::Retry => {
                            consecutive_retries += 1;
                            units_failed -= 1;
                            continue;
                        }
                        ErrorDecision::Skip | ErrorDecision::SkipAll => {
                            units_skipped += 1;
                            failures.push(ScriptUnitFailure {
                                decision,
                                ..failure
                            });
                            send_progress(
                                &progress,
                                &mut throttle,
                                true,
                                progress_snapshot(
                                    units_done,
                                    units_failed,
                                    units_skipped,
                                    reader.bytes_read.min(total_bytes),
                                    total_bytes,
                                    rows_affected,
                                    current_line,
                                    messages.last().cloned(),
                                    false,
                                ),
                            )
                            .await;
                            break;
                        }
                        ErrorDecision::Stop => {
                            stopped_on_error = true;
                            failures.push(ScriptUnitFailure {
                                decision,
                                ..failure
                            });
                            send_progress(
                                &progress,
                                &mut throttle,
                                true,
                                progress_snapshot(
                                    units_done,
                                    units_failed,
                                    units_skipped,
                                    reader.bytes_read.min(total_bytes),
                                    total_bytes,
                                    rows_affected,
                                    current_line,
                                    messages.last().cloned(),
                                    false,
                                ),
                            )
                            .await;
                            break 'units;
                        }
                    }
                }
            }
        }
    }

    if in_transaction {
        let sql = if cancelled || stopped_on_error {
            rollback_sql()
        } else {
            commit_sql()
        };
        let _ = execute_unit(
            queries,
            profile,
            &options.session_key,
            sql,
            &cancel,
            false,
            &mut rows_affected,
            &mut messages,
            &mut database_changed_to,
        )
        .await;
    }

    if messages.len() > MESSAGE_CAP {
        let dropped = messages.len() - MESSAGE_CAP;
        messages = messages.split_off(messages.len() - MESSAGE_CAP);
        messages.insert(0, format!("({dropped} earlier messages omitted)"));
    }

    send_progress(
        &progress,
        &mut throttle,
        true,
        progress_snapshot(
            units_done,
            units_failed,
            units_skipped,
            reader.bytes_read.min(total_bytes),
            total_bytes,
            rows_affected,
            current_line,
            messages.last().cloned(),
            true,
        ),
    )
    .await;

    Ok(ScriptOutcome {
        units_total,
        units_done,
        units_failed,
        units_skipped,
        rows_affected,
        elapsed_ms: started.elapsed().as_millis(),
        failures,
        messages,
        cancelled,
        stopped_on_error,
        database_changed_to,
    })
}

// Cohesive step signature; grouping args into a struct adds no clarity here.
#[allow(clippy::too_many_arguments)]
async fn execute_unit(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    session_key: &str,
    sql: &str,
    cancel: &ScriptCancel,
    honor_cancel: bool,
    rows_affected: &mut u64,
    messages: &mut Vec<String>,
    database_changed_to: &mut Option<String>,
) -> Result<(), String> {
    let handle = queries
        .execute_in_session(profile, Some(session_key), sql, 16)
        .await
        .map_err(|e| e.to_string())?;
    let task_id = handle.task_id.clone();
    let mut events = handle.events;
    loop {
        if honor_cancel && cancel.is_cancelled() {
            let _ = queries.cancel(&task_id).await;
            return Err("cancelled".into());
        }
        let event = events.recv().await;
        match event {
            Some(QueryEvent::RowCount {
                rows_affected: n, ..
            }) => *rows_affected += n,
            Some(QueryEvent::Message { text }) => messages.push(text),
            Some(QueryEvent::DatabaseChanged { name }) => *database_changed_to = Some(name),
            Some(QueryEvent::SpillManifest { manifest }) => discard_spill_manifest(&manifest),
            Some(QueryEvent::Error { message }) => return Err(message),
            Some(QueryEvent::Completed { .. }) | Some(QueryEvent::Cancelled) => return Ok(()),
            Some(QueryEvent::Rows { .. })
            | Some(QueryEvent::TruncatedCells { .. })
            | Some(QueryEvent::ResultSet { .. })
            | Some(QueryEvent::RowsProgress { .. })
            | Some(QueryEvent::SpillWindow { .. })
            | Some(QueryEvent::Started) => {}
            None => return Ok(()),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn open_script_file(
    path: &std::path::Path,
) -> std::io::Result<(ChunkReader<std::fs::File>, u64)> {
    let file = std::fs::File::open(path)?;
    let total_bytes = file.metadata()?.len();
    Ok((ChunkReader::new(file), total_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_after_error_matrix() {
        assert_eq!(
            decide_after_error(OnError::Stop, None, false, 0),
            ErrorDecision::Stop
        );
        assert_eq!(
            decide_after_error(OnError::Continue, None, false, 0),
            ErrorDecision::Skip
        );
        assert_eq!(
            decide_after_error(OnError::Ask, Some(ErrorDecision::Retry), false, 0),
            ErrorDecision::Retry
        );
        assert_eq!(
            decide_after_error(OnError::Ask, Some(ErrorDecision::SkipAll), false, 0),
            ErrorDecision::SkipAll
        );
        assert_eq!(
            decide_after_error(OnError::Ask, Some(ErrorDecision::Retry), true, 0),
            ErrorDecision::Skip
        );
        assert_eq!(
            decide_after_error(OnError::Ask, Some(ErrorDecision::Retry), false, 5),
            ErrorDecision::Stop
        );
    }

    #[test]
    fn progress_throttle_cadence() {
        let mut t = ProgressThrottle::new(Duration::from_millis(100));
        let start = Instant::now();
        assert!(t.should_send(start, false));
        assert!(!t.should_send(start + Duration::from_millis(50), false));
        assert!(t.should_send(start + Duration::from_millis(100), false));
        assert!(t.should_send(start + Duration::from_millis(110), true));
    }

    #[test]
    fn coalesce_adjacent_inserts() {
        let units = vec![
            unit("INSERT INTO t (a,b) VALUES (1,2)"),
            unit("INSERT INTO t (a,b) VALUES (3,4)"),
            unit("INSERT INTO t (a,b) VALUES (5,6)"),
        ];
        let merged = coalesce_inserts(Dialect::Postgres, &units, 500, 1024 * 1024);
        assert_eq!(merged.len(), 1);
        assert!(merged[0].contains("(1,2)"));
        assert!(merged[0].contains("(3,4)"));
        assert!(merged[0].contains("(5,6)"));
    }

    #[test]
    fn coalesce_different_columns_not_merged() {
        let units = vec![
            unit("INSERT INTO t (a) VALUES (1)"),
            unit("INSERT INTO t (a,b) VALUES (2,3)"),
        ];
        assert_eq!(
            coalesce_inserts(Dialect::Postgres, &units, 500, 1024 * 1024).len(),
            2
        );
    }

    #[test]
    fn coalesce_non_insert_between_splits() {
        let units = vec![
            unit("INSERT INTO t (a) VALUES (1)"),
            unit("SELECT 1"),
            unit("INSERT INTO t (a) VALUES (2)"),
        ];
        assert_eq!(
            coalesce_inserts(Dialect::Postgres, &units, 500, 1024 * 1024).len(),
            3
        );
    }

    #[test]
    fn coalesce_insert_select_passes_through() {
        let units = vec![unit("INSERT INTO t SELECT 1")];
        assert_eq!(
            coalesce_inserts(Dialect::Postgres, &units, 500, 1024 * 1024).len(),
            1
        );
        assert_eq!(
            coalesce_inserts(Dialect::Postgres, &units, 500, 1024 * 1024)[0],
            units[0].text
        );
    }

    #[test]
    fn coalesce_keeps_a_leading_comment_and_stops_at_a_commented_insert() {
        // Statements keep the comments above them now; a dump's section
        // comment must not stop the inserts below it from batching, and a
        // comment further down must not be swallowed by a merged batch.
        let units = vec![
            unit("-- Data for t\nINSERT INTO t (a) VALUES (1)"),
            unit("INSERT INTO t (a) VALUES (2)"),
            unit("/* part two */\nINSERT INTO t (a) VALUES (3)"),
            unit("INSERT INTO t (a) VALUES (4)"),
        ];
        let merged = coalesce_inserts(Dialect::Postgres, &units, 500, 1024 * 1024);
        assert_eq!(
            merged,
            vec![
                "-- Data for t\nINSERT INTO t (a) VALUES (1), (2)".to_string(),
                "/* part two */\nINSERT INTO t (a) VALUES (3), (4)".to_string(),
            ]
        );
    }

    #[test]
    fn coalesce_treats_hash_comments_as_comments_on_mysql_only() {
        // A dump's `# Data for t` section header is a comment on MySQL, so
        // the INSERTs under it batch; on other engines `#` is code and the
        // unit passes through untouched.
        let units = vec![
            unit("# Data for t\nINSERT INTO t (a) VALUES (1)"),
            unit("INSERT INTO t (a) VALUES (2)"),
            unit("# part two\nINSERT INTO t (a) VALUES (3)"),
            unit("INSERT INTO t (a) VALUES (4)"),
        ];
        let merged = coalesce_inserts(Dialect::MySql, &units, 500, 1024 * 1024);
        assert_eq!(
            merged,
            vec![
                "# Data for t\nINSERT INTO t (a) VALUES (1), (2)".to_string(),
                "# part two\nINSERT INTO t (a) VALUES (3), (4)".to_string(),
            ]
        );
        let merged = coalesce_inserts(Dialect::Postgres, &units, 500, 1024 * 1024);
        assert_eq!(merged.len(), 4, "{merged:#?}");
        assert_eq!(merged[0], units[0].text);
    }

    fn unit(text: &str) -> ScriptUnit {
        ScriptUnit {
            text: text.to_string(),
            start_line: 1,
            end_line: 1,
            ordinal: 0,
        }
    }
}
