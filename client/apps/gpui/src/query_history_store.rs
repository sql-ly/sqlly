//! Native query execution history: a local SQLite database at
//! `<data-dir>/SQLLY/query-history.db` with WAL journaling, deduplicated
//! consecutive runs, keyset paging, and a one-time import from the legacy
//! `query-history.txt` log (renamed to `.bak` after import).

// Only used by the cfg(unix) permission-tightening path below.
#[cfg(unix)]
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::intellisense_feedback::redact_sql;
use crate::query_history::{
    HistoryCursor, HistoryEntry, HistoryFilter, HistoryOutcome, HistoryPage, HistoryProvenance,
    HistoryRecord, TIMESTAMP_FORMAT, data_file, normalized_sql_hash, parse_history,
    retained_history_entries,
};

const DB_FILE_NAME: &str = "query-history.db";
const LEGACY_FILE_NAME: &str = "query-history.txt";

const SCHEMA: &str = "
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
CREATE TABLE IF NOT EXISTS history (
  id            INTEGER PRIMARY KEY,
  executed_at   TEXT    NOT NULL,
  server        TEXT    NOT NULL,
  database      TEXT,
  sql           TEXT    NOT NULL,
  sql_hash      TEXT    NOT NULL,
  outcome       TEXT    NOT NULL,
  duration_ms   INTEGER,
  affected_rows INTEGER,
  provenance    TEXT    NOT NULL DEFAULT 'manual',
  rollback_sql  TEXT,
  run_count     INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS history_executed_at_id ON history(executed_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS history_target_hash ON history(server, database, sql_hash);
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
";

thread_local! {
    static THREAD_STORE_PATH_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const {
        std::cell::RefCell::new(None)
    };
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn set_store_path_override(path: Option<PathBuf>) {
    // Thread-local ONLY — deliberately not a process-global. A shared global
    // leaks across parallel tests: a sibling test that merely reads the store
    // (e.g. constructing a `QueryHistoryBrowser`, whose `new` reads the log)
    // would resolve through another test's override to that test's private
    // database and race on the same SQLite file, silently dropping inserts.
    // Every test performs its own store reads on the same thread that set the
    // override, so the thread-local is sufficient and cannot bleed across tests.
    THREAD_STORE_PATH_OVERRIDE.with(|slot| *slot.borrow_mut() = path);
}

fn store_db_path() -> PathBuf {
    THREAD_STORE_PATH_OVERRIDE
        .with(|slot| slot.borrow().clone())
        .unwrap_or_else(|| data_file(DB_FILE_NAME))
}

/// Serializes tests that drive the store-path override. The override is now
/// thread-local (see [`set_store_path_override`]), so parallel tests no longer
/// read each other's database; this lock is retained as belt-and-suspenders for
/// tests that share other global store state. Poison is ignored: a panicking
/// test has already failed and must not cascade into the rest.
#[cfg(test)]
pub(crate) fn store_path_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    SERIAL.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub struct QueryHistory {
    path: PathBuf,
    conn: Mutex<Option<Connection>>,
}

impl QueryHistory {
    pub fn new() -> Self {
        Self {
            path: store_db_path(),
            conn: Mutex::new(None),
        }
    }

    #[allow(dead_code)]
    pub fn with_store_path(path: PathBuf) -> Self {
        Self {
            path,
            conn: Mutex::new(None),
        }
    }

    pub fn log(&self, record: &HistoryRecord<'_>) {
        let clean = redact_sql(record.sql.trim());
        let _ = self.with_conn(|conn| {
            insert_dedupe(
                conn,
                &self.path,
                InsertRow {
                    executed_at: chrono::Utc::now().format(TIMESTAMP_FORMAT).to_string(),
                    server: record.server.trim().to_string(),
                    database: record
                        .database
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    sql: clean,
                    outcome: record.outcome,
                    duration_ms: record.duration_ms,
                    affected_rows: record.affected_rows,
                    provenance: record.provenance,
                    rollback_sql: record.rollback_sql.map(str::to_string),
                },
            )
        });
    }

    #[cfg(test)]
    pub fn insert_at(&self, record: &HistoryRecord<'_>, executed_at: &str) {
        let clean = redact_sql(record.sql.trim());
        let _ = self.with_conn(|conn| {
            insert_dedupe(
                conn,
                &self.path,
                InsertRow {
                    executed_at: executed_at.to_string(),
                    server: record.server.trim().to_string(),
                    database: record
                        .database
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    sql: clean,
                    outcome: record.outcome,
                    duration_ms: record.duration_ms,
                    affected_rows: record.affected_rows,
                    provenance: record.provenance,
                    rollback_sql: record.rollback_sql.map(str::to_string),
                },
            )
        });
    }

    fn with_conn<R>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> Option<R> {
        let mut guard = self.conn.lock().ok()?;
        if guard.is_none() {
            *guard = open(&self.path).ok();
        }
        guard.as_ref().and_then(|conn| f(conn).ok())
    }
}

impl Default for QueryHistory {
    fn default() -> Self {
        Self::new()
    }
}

pub fn read_page(filter: &str, after: Option<&HistoryCursor>, limit: usize) -> HistoryPage {
    let path = store_db_path();
    let Ok(conn) = open(&path) else {
        return HistoryPage::default();
    };
    read_page_from(&conn, filter, after, limit)
}

#[allow(dead_code)]
pub fn read_entries(limit: usize) -> Vec<HistoryEntry> {
    read_page("", None, limit).entries
}

pub fn apply_retention() -> Result<usize, String> {
    let prefs = crate::preferences::QueryHistoryPreferences::load();
    let days = prefs.execution_retention_days();
    let max_entries = prefs.execution_max_entries();
    let path = store_db_path();
    let conn = open(&path).map_err(|error| error.to_string())?;
    apply_retention_to(&conn, chrono::Utc::now(), days, max_entries)
}

struct InsertRow {
    executed_at: String,
    server: String,
    database: Option<String>,
    sql: String,
    outcome: HistoryOutcome,
    duration_ms: Option<u64>,
    affected_rows: Option<u64>,
    provenance: HistoryProvenance,
    rollback_sql: Option<String>,
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    #[cfg(unix)]
    if !path.exists() {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path);
    }
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(SCHEMA)?;
    let version: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if version.is_none() {
        conn.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', '1')",
            [],
        )?;
    }
    import_legacy_text_log(&conn, path)?;
    #[cfg(unix)]
    tighten_db_permissions(path);
    Ok(conn)
}

#[cfg(unix)]
fn tighten_db_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for candidate in [
        path.to_path_buf(),
        path.with_extension("db-wal"),
        path.with_extension("db-shm"),
    ] {
        if let Ok(meta) = std::fs::metadata(&candidate) {
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                let _ = std::fs::set_permissions(
                    &candidate,
                    std::fs::Permissions::from_mode(mode & !0o077),
                );
            }
        }
    }
}

fn import_legacy_text_log(conn: &Connection, db_path: &Path) -> rusqlite::Result<()> {
    let txt_path = db_path.with_file_name(LEGACY_FILE_NAME);
    let done: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'legacy_import'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if done.is_some() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&txt_path).unwrap_or_default();
    let entries = parse_history(&text);
    let imported_count = entries.len();
    let tx = conn.unchecked_transaction()?;
    for entry in entries {
        let clean = redact_sql(entry.sql.trim());
        insert_dedupe_body(
            &tx,
            &InsertRow {
                executed_at: entry.timestamp,
                server: entry.server,
                database: entry.database,
                sql: clean,
                outcome: entry.outcome,
                duration_ms: entry.duration_ms,
                affected_rows: None,
                provenance: HistoryProvenance::Manual,
                rollback_sql: None,
            },
        )?;
    }
    tx.execute(
        "INSERT INTO meta(key, value) VALUES('legacy_import', ?1)",
        params![imported_count.to_string()],
    )?;
    tx.commit()?;
    if txt_path.exists() {
        let backup = txt_path.with_extension("txt.bak");
        let _ = std::fs::rename(txt_path, backup);
    }
    Ok(())
}

fn insert_dedupe_body(conn: &Connection, row: &InsertRow) -> rusqlite::Result<()> {
    let sql_hash = normalized_sql_hash(&row.sql);
    let newest: Option<(i64, String, String, Option<String>, String)> = conn
        .query_row(
            "SELECT id, sql_hash, server, database, provenance FROM history ORDER BY executed_at DESC, id DESC LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    if let Some((id, prev_hash, prev_server, prev_database, prev_provenance)) = newest
        && prev_hash == sql_hash
        && prev_server == row.server
        && databases_equal(&prev_database, &row.database)
        && prev_provenance == row.provenance.as_str()
    {
        conn.execute(
            "UPDATE history SET run_count = run_count + 1, executed_at = ?1, outcome = ?2, duration_ms = ?3, affected_rows = ?4, rollback_sql = COALESCE(?5, rollback_sql) WHERE id = ?6",
            params![
                row.executed_at,
                row.outcome.as_log_value(),
                row.duration_ms.map(i64::try_from).transpose().ok().flatten(),
                row.affected_rows.map(i64::try_from).transpose().ok().flatten(),
                row.rollback_sql,
                id,
            ],
        )?;
    } else {
        conn.execute(
            "INSERT INTO history(executed_at, server, database, sql, sql_hash, outcome, duration_ms, affected_rows, provenance, rollback_sql, run_count) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)",
            params![
                row.executed_at,
                row.server,
                row.database,
                row.sql,
                sql_hash,
                row.outcome.as_log_value(),
                row.duration_ms.map(i64::try_from).transpose().ok().flatten(),
                row.affected_rows.map(i64::try_from).transpose().ok().flatten(),
                row.provenance.as_str(),
                row.rollback_sql,
            ],
        )?;
    }
    Ok(())
}

// db_path feeds the cfg(unix) permission tightening; unused on other targets.
#[cfg_attr(not(unix), allow(unused_variables))]
fn insert_dedupe(conn: &Connection, db_path: &Path, row: InsertRow) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    insert_dedupe_body(&tx, &row)?;
    tx.commit()?;
    #[cfg(unix)]
    tighten_db_permissions(db_path);
    Ok(())
}

fn databases_equal(left: &Option<String>, right: &Option<String>) -> bool {
    match (left.as_deref(), right.as_deref()) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

fn read_page_from(
    conn: &Connection,
    filter: &str,
    after: Option<&HistoryCursor>,
    limit: usize,
) -> HistoryPage {
    let parsed = HistoryFilter::parse(filter);
    let (sql, bind) = build_read_sql(&parsed, after, limit);
    let mut stmt = match conn.prepare(&sql) {
        Ok(stmt) => stmt,
        Err(_) => return HistoryPage::default(),
    };
    let rows = stmt
        .query_map(rusqlite::params_from_iter(bind.iter()), row_to_entry)
        .ok();
    let mut entries: Vec<HistoryEntry> = rows
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| parsed.matches(entry))
        .collect();
    let mut next = None;
    if entries.len() > limit {
        let last_returned = entries[limit - 1].clone();
        entries.truncate(limit);
        next = Some(HistoryCursor {
            executed_at: last_returned.timestamp,
            id: last_returned.id,
        });
    }
    HistoryPage { entries, next }
}

fn build_read_sql(
    filter: &HistoryFilter,
    after: Option<&HistoryCursor>,
    limit: usize,
) -> (String, Vec<rusqlite::types::Value>) {
    let mut clauses = Vec::new();
    let mut bind = Vec::new();
    if let Some(server) = &filter.server {
        clauses.push("instr(lower(server), lower(?)) > 0".to_string());
        bind.push(rusqlite::types::Value::Text(server.clone()));
    }
    if let Some(database) = &filter.database {
        clauses.push("instr(lower(COALESCE(database, '')), lower(?)) > 0".to_string());
        bind.push(rusqlite::types::Value::Text(database.clone()));
    }
    if let Some(outcome) = filter.outcome {
        clauses.push("outcome = ?".to_string());
        bind.push(rusqlite::types::Value::Text(
            outcome.as_log_value().to_string(),
        ));
    }
    if let Some(after_date) = filter.after {
        clauses.push("substr(executed_at, 1, 10) >= ?".to_string());
        bind.push(rusqlite::types::Value::Text(after_date.to_string()));
    }
    if let Some(before_date) = filter.before {
        clauses.push("substr(executed_at, 1, 10) <= ?".to_string());
        bind.push(rusqlite::types::Value::Text(before_date.to_string()));
    }
    if let Some(min) = filter.min_duration_ms {
        clauses.push("duration_ms >= ?".to_string());
        bind.push(rusqlite::types::Value::Integer(
            i64::try_from(min).unwrap_or(i64::MAX),
        ));
    }
    if let Some(max) = filter.max_duration_ms {
        clauses.push("duration_ms <= ?".to_string());
        bind.push(rusqlite::types::Value::Integer(
            i64::try_from(max).unwrap_or(i64::MAX),
        ));
    }
    for token in &filter.text {
        clauses.push("instr(lower(sql), lower(?)) > 0".to_string());
        bind.push(rusqlite::types::Value::Text(token.clone()));
    }
    if filter.tag.is_some() {
        clauses.push("1 = 0".to_string());
    }
    if let Some(cursor) = after {
        clauses.push("(executed_at < ? OR (executed_at = ? AND id < ?))".to_string());
        bind.push(rusqlite::types::Value::Text(cursor.executed_at.clone()));
        bind.push(rusqlite::types::Value::Text(cursor.executed_at.clone()));
        bind.push(rusqlite::types::Value::Integer(cursor.id));
    }
    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    bind.push(rusqlite::types::Value::Integer(
        i64::try_from(limit + 1).unwrap_or(i64::MAX),
    ));
    let sql = format!(
        "SELECT id, executed_at, server, database, sql, outcome, duration_ms, affected_rows, provenance, rollback_sql, run_count FROM history{where_clause} ORDER BY executed_at DESC, id DESC LIMIT ?"
    );
    (sql, bind)
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEntry> {
    let database: Option<String> = row.get(3)?;
    Ok(HistoryEntry {
        id: row.get(0)?,
        sql: row.get(4)?,
        server: row.get(2)?,
        database: database.filter(|value| !value.is_empty()),
        timestamp: row.get(1)?,
        duration_ms: row
            .get::<_, Option<i64>>(6)?
            .map(|value| u64::try_from(value).unwrap_or(0)),
        outcome: HistoryOutcome::parse(&row.get::<_, String>(5)?),
        tags: Vec::new(),
        name: String::new(),
        description: String::new(),
        run_count: u32::try_from(row.get::<_, i64>(10)?).unwrap_or(1).max(1),
        provenance: HistoryProvenance::parse(&row.get::<_, String>(8)?),
        affected_rows: row
            .get::<_, Option<i64>>(7)?
            .map(|value| u64::try_from(value).unwrap_or(0)),
        rollback_sql: row.get(9)?,
    })
}

fn apply_retention_to(
    conn: &Connection,
    now: chrono::DateTime<chrono::Utc>,
    retention_days: u32,
    max_entries: usize,
) -> Result<usize, String> {
    let all: Vec<HistoryEntry> = conn
        .prepare(
            "SELECT id, executed_at, server, database, sql, outcome, duration_ms, affected_rows, provenance, rollback_sql, run_count FROM history ORDER BY executed_at ASC, id ASC",
        )
        .map_err(|error| error.to_string())?
        .query_map([], row_to_entry)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .collect();
    let before_len = all.len();
    let retained = retained_history_entries(all, now, retention_days, max_entries);
    let removed = before_len.saturating_sub(retained.len());
    if removed == 0 {
        return Ok(0);
    }
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    tx.execute("DELETE FROM history", [])
        .map_err(|error| error.to_string())?;
    for entry in retained {
        let sql_hash = normalized_sql_hash(&entry.sql);
        tx.execute(
            "INSERT INTO history(id, executed_at, server, database, sql, sql_hash, outcome, duration_ms, affected_rows, provenance, rollback_sql, run_count) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                entry.id,
                entry.timestamp,
                entry.server,
                entry.database,
                entry.sql,
                sql_hash,
                entry.outcome.as_log_value(),
                entry.duration_ms.map(|value| i64::try_from(value).unwrap_or(i64::MAX)),
                entry
                    .affected_rows
                    .map(|value| i64::try_from(value).unwrap_or(i64::MAX)),
                entry.provenance.as_str(),
                entry.rollback_sql,
                i64::from(entry.run_count),
            ],
        )
        .map_err(|error| error.to_string())?;
    }
    tx.commit().map_err(|error| error.to_string())?;
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    Ok(removed)
}

#[cfg(test)]
pub(crate) fn apply_retention_to_path(
    path: &Path,
    now: chrono::DateTime<chrono::Utc>,
    retention_days: u32,
    max_entries: usize,
) -> Result<usize, String> {
    let conn = open(path).map_err(|error| error.to_string())?;
    apply_retention_to(&conn, now, retention_days, max_entries)
}

#[cfg(test)]
pub(crate) fn read_entries_from_path(path: &Path, limit: usize) -> Vec<HistoryEntry> {
    let conn = open(path).unwrap_or_else(|_| Connection::open_in_memory().unwrap());
    read_page_from(&conn, "", None, limit).entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_history::{HistoryRecord, filter_entries};

    /// Shared with `query_history_browser`'s store-using test; see
    /// [`super::store_path_test_guard`].
    fn serial_lock() -> std::sync::MutexGuard<'static, ()> {
        super::store_path_test_guard()
    }

    fn record<'a>(sql: &'a str, server: &'a str, database: Option<&'a str>) -> HistoryRecord<'a> {
        HistoryRecord {
            sql,
            server,
            database,
            duration_ms: None,
            outcome: HistoryOutcome::Unknown,
            provenance: HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql: None,
        }
    }

    fn legacy_entry(
        timestamp: &str,
        server: &str,
        database: &str,
        outcome: HistoryOutcome,
        duration_ms: &str,
        sql: &str,
    ) -> String {
        format!(
            "[{timestamp}] {server}/{database}\t{}\t{duration_ms}\n{sql}\n\n",
            outcome.as_log_value()
        )
    }

    #[test]
    fn legacy_text_log_imports_once_and_is_renamed() {
        let _serial = serial_lock();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("h.db");
        let txt_path = dir.path().join("query-history.txt");
        let body = format!(
            "{}{}{}",
            legacy_entry(
                "2026-08-01T10:00:00Z",
                "alpha",
                "db1",
                HistoryOutcome::Succeeded,
                "100",
                "SELECT 1"
            ),
            legacy_entry(
                "2026-08-02T10:00:00Z",
                "beta",
                "db2",
                HistoryOutcome::Failed,
                "200",
                "SELECT 2"
            ),
            legacy_entry(
                "2026-08-03T10:00:00Z",
                "gamma",
                "db3",
                HistoryOutcome::Cancelled,
                "300",
                "SELECT 3"
            ),
        );
        std::fs::write(&txt_path, body).unwrap();
        set_store_path_override(Some(db_path.clone()));
        let _history = QueryHistory::with_store_path(db_path.clone());
        let entries = read_entries(10);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sql, "SELECT 3");
        assert_eq!(entries[2].sql, "SELECT 1");
        assert!(!txt_path.exists());
        assert!(dir.path().join("query-history.txt.bak").exists());
        let count_after_reopen = read_entries(10).len();
        assert_eq!(count_after_reopen, 3);
        set_store_path_override(None);
    }

    #[test]
    fn import_redacts_sql() {
        let _serial = serial_lock();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("h.db");
        let txt_path = dir.path().join("query-history.txt");
        std::fs::write(
            &txt_path,
            legacy_entry(
                "2026-08-01T10:00:00Z",
                "prod",
                "sales",
                HistoryOutcome::Succeeded,
                "",
                "-- password=hunter2",
            ),
        )
        .unwrap();
        set_store_path_override(Some(db_path));
        let _history = QueryHistory::new();
        let entries = read_entries(10);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].sql.contains("hunter2"));
        set_store_path_override(None);
    }

    #[test]
    fn consecutive_identical_runs_collapse_into_run_count() {
        let _serial = serial_lock();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("h.db");
        set_store_path_override(Some(db_path.clone()));
        let history = QueryHistory::with_store_path(db_path);
        history.log(&record("SELECT 1", "srv", Some("db")));
        history.log(&record("select   1 ;", "srv", Some("db")));
        history.log(&HistoryRecord {
            sql: "SELECT 1",
            server: "srv",
            database: Some("db"),
            duration_ms: Some(42),
            outcome: HistoryOutcome::Failed,
            provenance: HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql: None,
        });
        let entries = read_entries(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].run_count, 3);
        assert_eq!(entries[0].outcome, HistoryOutcome::Failed);
        assert_eq!(entries[0].duration_ms, Some(42));
        history.log(&record("SELECT 2", "srv", Some("db")));
        history.log(&record("SELECT 1", "srv", Some("db")));
        let entries = read_entries(10);
        assert_eq!(entries.len(), 3);
        set_store_path_override(None);
    }

    #[test]
    fn different_target_does_not_collapse() {
        let _serial = serial_lock();
        let dir = tempfile::tempdir().unwrap();
        set_store_path_override(Some(dir.path().join("h.db")));
        let history = QueryHistory::new();
        history.log(&record("SELECT 1", "srv", Some("db1")));
        history.log(&record("SELECT 1", "srv", Some("db2")));
        assert_eq!(read_entries(10).len(), 2);
        set_store_path_override(None);
    }

    #[test]
    fn keyset_paging_walks_without_gaps_or_repeats() {
        let _serial = serial_lock();
        let dir = tempfile::tempdir().unwrap();
        set_store_path_override(Some(dir.path().join("h.db")));
        let history = QueryHistory::new();
        for index in 0..25 {
            history.insert_at(
                &record(&format!("SELECT {index}"), "srv", Some("db")),
                &format!("2026-08-01T12:{index:02}:00Z"),
            );
        }
        assert_eq!(read_entries(100).len(), 25);
        let mut seen = Vec::new();
        let mut cursor = None;
        for _ in 0..3 {
            let page = read_page("", cursor.as_ref(), 10);
            seen.extend(page.entries.iter().map(|entry| entry.id));
            cursor = page.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(seen.len(), 25);
        let mut unique = seen.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 25);
        assert!(cursor.is_none());
        set_store_path_override(None);
    }

    #[test]
    fn retention_by_age_then_count_in_one_transaction() {
        let _serial = serial_lock();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("h.db");
        set_store_path_override(Some(db_path.clone()));
        let history = QueryHistory::new();
        for (day, sql) in [(1, "old"), (20, "middle"), (24, "newest")] {
            history.insert_at(
                &record(sql, "srv", Some("db")),
                &format!("2026-08-{day:02}T10:00:00Z"),
            );
        }
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-24T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(apply_retention_to_path(&db_path, now, 10, 1).unwrap(), 2);
        let retained = read_entries(10);
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].sql, "newest");
        set_store_path_override(None);
    }

    #[test]
    fn search_parity_with_in_memory_filter() {
        let _serial = serial_lock();
        let dir = tempfile::tempdir().unwrap();
        set_store_path_override(Some(dir.path().join("h.db")));
        let history = QueryHistory::new();
        let fixtures = [
            (
                "SELECT orders",
                "alpha",
                Some("sales"),
                "2026-01-10T10:00:00Z",
                HistoryOutcome::Succeeded,
                50u64,
            ),
            (
                "SELECT orders",
                "alpha",
                Some("sales"),
                "2026-01-16T10:00:00Z",
                HistoryOutcome::Succeeded,
                2500,
            ),
            (
                "SELECT users",
                "beta",
                Some("app"),
                "2026-01-12T10:00:00Z",
                HistoryOutcome::Failed,
                50,
            ),
            (
                "SELECT items",
                "alpha",
                Some("inventory"),
                "2026-01-14T10:00:00Z",
                HistoryOutcome::Succeeded,
                2500,
            ),
            (
                "SELECT audit",
                "beta",
                Some("sales"),
                "2026-01-11T10:00:00Z",
                HistoryOutcome::Cancelled,
                50,
            ),
            (
                "SELECT orders backup",
                "alpha",
                Some("sales"),
                "2026-01-13T10:00:00Z",
                HistoryOutcome::Failed,
                2500,
            ),
            (
                "SELECT metrics",
                "gamma",
                Some("ops"),
                "2026-01-15T10:00:00Z",
                HistoryOutcome::Succeeded,
                50,
            ),
            (
                "SELECT orders archive",
                "alpha",
                Some("sales"),
                "2026-01-15T10:00:00Z",
                HistoryOutcome::Succeeded,
                2500,
            ),
            (
                "SELECT queue",
                "beta",
                Some("app"),
                "2026-01-16T10:00:00Z",
                HistoryOutcome::Failed,
                50,
            ),
            (
                "SELECT orders delta",
                "alpha",
                Some("sales"),
                "2026-01-16T10:00:00Z",
                HistoryOutcome::Succeeded,
                50,
            ),
            (
                "SELECT cache",
                "gamma",
                Some("ops"),
                "2026-01-10T10:00:00Z",
                HistoryOutcome::Failed,
                2500,
            ),
            (
                "SELECT orders gamma",
                "alpha",
                Some("sales"),
                "2026-01-14T10:00:00Z",
                HistoryOutcome::Succeeded,
                50,
            ),
        ];
        for (sql, server, database, timestamp, outcome, duration) in fixtures {
            history.insert_at(
                &HistoryRecord {
                    sql,
                    server,
                    database,
                    duration_ms: Some(duration),
                    outcome,
                    provenance: HistoryProvenance::Manual,
                    affected_rows: None,
                    rollback_sql: None,
                },
                timestamp,
            );
        }
        let all = read_entries(100);
        let queries = [
            "",
            "select",
            "server:alpha",
            "db:sales",
            "outcome:failed",
            "after:2026-01-15",
            "before:2026-01-15",
            "min:1s",
            "max:100ms",
            "server:alpha outcome:succeeded orders",
        ];
        for query in queries {
            let store_rows: Vec<_> = read_page(query, None, 100)
                .entries
                .into_iter()
                .map(|entry| {
                    (
                        entry.sql.clone(),
                        entry.server.clone(),
                        entry.database.clone(),
                    )
                })
                .collect();
            let filtered: Vec<_> = filter_entries(&all, query)
                .into_iter()
                .map(|entry| {
                    (
                        entry.sql.clone(),
                        entry.server.clone(),
                        entry.database.clone(),
                    )
                })
                .collect();
            assert_eq!(store_rows, filtered, "query={query:?}");
        }
        set_store_path_override(None);
    }

    #[cfg(unix)]
    #[test]
    fn store_file_is_owner_only_on_unix() {
        let _serial = serial_lock();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("h.db");
        set_store_path_override(Some(db_path.clone()));
        let history = QueryHistory::new();
        history.log(&record("SELECT 1", "srv", Some("db")));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&db_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
        set_store_path_override(None);
    }

    #[test]
    fn provenance_and_affected_rows_round_trip() {
        let _serial = serial_lock();
        let dir = tempfile::tempdir().unwrap();
        set_store_path_override(Some(dir.path().join("h.db")));
        let history = QueryHistory::new();
        history.log(&HistoryRecord {
            sql: "UPDATE t SET x = 1",
            server: "srv",
            database: Some("db"),
            duration_ms: Some(9),
            outcome: HistoryOutcome::Succeeded,
            provenance: HistoryProvenance::Ai,
            affected_rows: Some(7),
            rollback_sql: Some("DELETE FROM t WHERE x = 1"),
        });
        let entry = &read_entries(1)[0];
        assert_eq!(entry.provenance, HistoryProvenance::Ai);
        assert_eq!(entry.affected_rows, Some(7));
        assert_eq!(
            entry.rollback_sql.as_deref(),
            Some("DELETE FROM t WHERE x = 1")
        );
        set_store_path_override(None);
    }
}
