//! Engine-generic row-editing orchestration for the natively driven engines
//! (PostgreSQL wire, MySQL wire, SQLite).
//!
//! One implementation of the open/update/insert/delete/search flows runs
//! against a small per-engine connection trait ([`RowEditEngine`]), so every
//! engine gets the SAME guarantees the T-SQL path hand-codes:
//!
//! - provenance comes from the engine's own description of the prepared
//!   statement (never from column-name matching or client-side SQL parsing);
//! - rows are addressed only by real primary-key columns, all of which must
//!   be present in the projection with non-NULL values;
//! - writes are parameterized, wrapped in an explicit transaction, and
//!   guarded by an affected-row-count check: 1 commits, 0 rolls back and
//!   refetches to distinguish "row changed" from "row gone", anything else
//!   rolls back with a hard error;
//! - optimistic concurrency compares the original values of the changed
//!   columns (none of these engines has a rowversion column type).
//!
//! Engines override the SQL-construction hooks only where typing demands it
//! (PostgreSQL attaches parameter/type casts; MySQL refetches inserted rows
//! in place of `RETURNING`).

#![cfg(not(target_arch = "wasm32"))]

use async_trait::async_trait;
use sqlly_core::{
    CellValue, ConnectionProfile, DatabaseType, DisplayMessage, MaterializedResultSet, SqlParam,
    StudioResult,
};

use crate::row_editing::{
    DeleteRowOutcome, DeleteRowRequest, DescribeColumn, FkLabelRequest, FkLabelResult,
    FkSearchRequest, FkSearchResult, ForeignKeySpec, InsertRowOutcome, InsertRowRequest,
    LoadedTable, RelatedRowsRequest, RelatedRowsResult, RowEditColumn, RowEditOpenOutcome,
    RowEditOpenRequest, RowEditSession, TableTarget, UpdateRowOutcome, UpdateRowRequest,
    analyze_provenance, parse_fk_candidates, parse_fk_labels, policy_block_message, render_cell,
};
use crate::row_editing_sql as dialect;

/// One live per-operation connection to an engine, plus the engine-specific
/// pieces of the row-editing flows: statement description, catalog loading,
/// and (where typing requires it) SQL construction.
#[async_trait]
pub(crate) trait RowEditEngine: Send {
    fn database_type(&self) -> DatabaseType;

    /// Describe the statement's first result set from the engine's own
    /// prepared-statement metadata. `Ok(Err(reason))` means "not editable"
    /// (dynamic SQL, multi-statement batches, …) — a reason, not a failure.
    async fn describe(
        &mut self,
        sql: &str,
    ) -> StudioResult<Result<Vec<DescribeColumn>, DisplayMessage>>;

    /// Load authoritative catalog metadata for the target table.
    /// `Ok(Err(reason))` covers "not found" and "not a base table".
    async fn load_table(
        &mut self,
        target: &TableTarget,
    ) -> StudioResult<Result<LoadedTable, DisplayMessage>>;

    /// Parameterized, materialized read (first result set).
    async fn query(
        &mut self,
        sql: &str,
        params: &[SqlParam],
    ) -> StudioResult<MaterializedResultSet>;

    /// Parameterized non-query; returns affected rows.
    async fn execute(&mut self, sql: &str, params: &[SqlParam]) -> StudioResult<u64>;

    async fn begin(&mut self) -> StudioResult<()>;
    async fn commit(&mut self) -> StudioResult<()>;
    async fn rollback(&mut self) -> StudioResult<()>;

    // ── SQL construction hooks ──────────────────────────────────────────
    // Dialect-generic defaults; PostgreSQL overrides them to attach the
    // parameter and projection casts its strict typing needs.

    async fn build_select_row(
        &mut self,
        target: &TableTarget,
        key: &[(String, CellValue)],
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        Ok(dialect::build_select_row_sql_for(
            self.database_type(),
            target,
            key,
        ))
    }

    async fn build_update(
        &mut self,
        request: &UpdateRowRequest,
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        Ok(dialect::build_update_sql_for(self.database_type(), request))
    }

    async fn build_delete(
        &mut self,
        request: &DeleteRowRequest,
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        Ok(dialect::build_delete_sql_for(self.database_type(), request))
    }

    async fn build_related(
        &mut self,
        request: &RelatedRowsRequest,
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        Ok(dialect::build_related_rows_sql_for(
            self.database_type(),
            request,
        ))
    }

    async fn build_fk_display_lookup(
        &mut self,
        referenced: &TableTarget,
        display: &str,
        ref_column: &str,
        value: &CellValue,
    ) -> StudioResult<(String, Vec<SqlParam>)> {
        Ok(dialect::build_fk_display_sql_for(
            self.database_type(),
            referenced,
            display,
            ref_column,
            value,
        ))
    }

    /// Run the INSERT inside the already-open transaction and return the
    /// inserted row (identity/default values resolved). Default: `RETURNING *`
    /// with an exactly-one-row check; MySQL overrides with a key refetch.
    async fn insert_returning(
        &mut self,
        request: &InsertRowRequest,
    ) -> StudioResult<(Vec<String>, Vec<CellValue>)> {
        let (sql, params) = dialect::build_insert_returning_sql_for(self.database_type(), request);
        let set = self.query(&sql, &params).await?;
        expect_single_inserted_row(set)
    }

    /// Run a staged batch's statements inside the already-open transaction,
    /// stopping at the first execution error (`Err`) or the first statement
    /// whose affected-row count differs from what it expected. Default: one
    /// `execute` per statement; libSQL overrides to send the whole set as a
    /// single conditional batch.
    async fn run_batch_statements(
        &mut self,
        statements: &[crate::row_editing::staged::StagedStatement],
    ) -> StudioResult<BatchRun> {
        for (index, statement) in statements.iter().enumerate() {
            let affected = self.execute(&statement.sql, &statement.params).await?;
            if let Some(expected) = statement.expect_affected
                && affected != expected
            {
                return Ok(BatchRun::Mismatch {
                    statement_index: index,
                    affected,
                });
            }
        }
        Ok(BatchRun::Applied)
    }
}

/// How a staged batch's statements fared inside the transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BatchRun {
    /// Every statement ran and affected exactly the rows it expected.
    Applied,
    /// `statement_index` affected `affected` rows instead of the expected
    /// count (a concurrent change); the caller rolls back.
    Mismatch {
        statement_index: usize,
        affected: u64,
    },
}

/// Checks a run's affected-row counts against each statement's expectation
/// (the shared rule for engines that run the whole batch before checking).
pub(crate) fn check_batch_counts(
    statements: &[crate::row_editing::staged::StagedStatement],
    affected: &[u64],
) -> BatchRun {
    for (index, statement) in statements.iter().enumerate() {
        let Some(expected) = statement.expect_affected else {
            continue;
        };
        let actual = affected.get(index).copied().unwrap_or(0);
        if actual != expected {
            return BatchRun::Mismatch {
                statement_index: index,
                affected: actual,
            };
        }
    }
    BatchRun::Applied
}

/// Shared "exactly one inserted row" check for `RETURNING`-style inserts.
pub(crate) fn expect_single_inserted_row(
    set: MaterializedResultSet,
) -> StudioResult<(Vec<String>, Vec<CellValue>)> {
    if set.rows.len() != 1 {
        return Err(crate::row_editing::errors::insert_returned_rows(
            set.rows.len(),
        ));
    }
    let columns = set.columns.iter().map(|c| c.name.clone()).collect();
    let row = set.rows.into_iter().next().expect("length checked");
    Ok((columns, row))
}

async fn fetch_row_inner<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    target: &TableTarget,
    key: &[(String, CellValue)],
) -> StudioResult<(Vec<String>, Vec<Vec<CellValue>>)> {
    let (sql, params) = engine.build_select_row(target, key).await?;
    let set = engine.query(&sql, &params).await?;
    let names = set.columns.iter().map(|c| c.name.clone()).collect();
    Ok((names, set.rows))
}

/// Best-effort display label for each single-column FK's current value.
/// A failed lookup yields `None` rather than aborting the open.
async fn preload_fk_display<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    foreign_keys: &[ForeignKeySpec],
    columns: &[RowEditColumn],
    original_row: &[CellValue],
) -> Vec<Option<String>> {
    let mut labels = Vec::with_capacity(foreign_keys.len());
    for fk in foreign_keys {
        labels.push(
            resolve_one_fk_display(engine, fk, columns, original_row)
                .await
                .unwrap_or(None),
        );
    }
    labels
}

async fn resolve_one_fk_display<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    fk: &ForeignKeySpec,
    columns: &[RowEditColumn],
    original_row: &[CellValue],
) -> StudioResult<Option<String>> {
    let Some(display) = fk.display_column.clone() else {
        return Ok(None);
    };
    if fk.column_pairs.len() != 1 {
        return Ok(None); // composite FKs skip preload
    }
    let (local, referenced) = &fk.column_pairs[0];
    let value = columns
        .iter()
        .position(|c| &c.name == local)
        .and_then(|i| original_row.get(i))
        .cloned();
    let Some(value) = value else {
        return Ok(None);
    };
    if matches!(value, CellValue::Null) {
        return Ok(None);
    }
    let (sql, params) = engine
        .build_fk_display_lookup(&fk.referenced, &display, referenced, &value)
        .await?;
    let set = engine.query(&sql, &params).await?;
    let label = set
        .rows
        .first()
        .and_then(|row| row.first())
        .and_then(|cell| match cell {
            CellValue::Null => None,
            other => Some(render_cell(other)),
        });
    Ok(label)
}

/// Whether the session resolves an existing row (edit) or only the target
/// table's shape (insert/duplicate template).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenMode {
    Edit,
    Insert,
}

/// Resolves whether the clicked row is editable — the engine-generic mirror
/// of the T-SQL `open_session`/`open_insert_session` flows.
pub(crate) async fn open_session_on<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    profile: &ConnectionProfile,
    request: RowEditOpenRequest,
    mode: OpenMode,
) -> StudioResult<RowEditOpenOutcome> {
    let db = engine.database_type();
    let not_editable = |reason: DisplayMessage| Ok(RowEditOpenOutcome::not_editable(reason));

    if let Some(reason) = policy_block_message(profile) {
        return not_editable(reason);
    }
    if request.result_set_index != 0 {
        return not_editable(match mode {
            OpenMode::Edit => crate::row_editing::not_editable::later_result_set(),
            OpenMode::Insert => crate::row_editing::not_editable::later_result_set_template(),
        });
    }
    if request.columns.len() != request.row.len() {
        return Err(crate::row_editing::errors::value_count_mismatch());
    }

    // 1. Provenance from the engine's own statement description.
    let described = match engine.describe(&request.sql).await? {
        Ok(described) => described,
        Err(reason) => return not_editable(reason),
    };
    let provenance = match analyze_provenance(&described, &request.columns) {
        Ok(provenance) => provenance,
        Err(reason) => return not_editable(reason),
    };

    // 2. Authoritative table metadata + primary key.
    let LoadedTable {
        mut columns,
        key_columns,
        foreign_keys,
        check_constraints,
        incoming_foreign_keys,
    } = match engine.load_table(&provenance.target).await? {
        Ok(loaded) => loaded,
        Err(reason) => return not_editable(reason),
    };
    for column in &mut columns {
        column.result_ordinal = provenance
            .source_ordinals
            .iter()
            .find(|(name, _)| *name == column.name)
            .map(|(_, ordinal)| *ordinal);
    }

    if mode == OpenMode::Insert {
        // Align the template row into catalog order (NULL where not
        // projected); callers use it as the "duplicate" seed.
        let original_row: Vec<CellValue> = columns
            .iter()
            .map(|column| {
                column
                    .result_ordinal
                    .and_then(|ordinal| request.row.get(ordinal).cloned())
                    .unwrap_or(CellValue::Null)
            })
            .collect();
        let fk_display = vec![None; foreign_keys.len()];
        return Ok(RowEditOpenOutcome::Editable(Box::new(RowEditSession {
            target: provenance.target,
            key_columns,
            rowversion_column: None,
            columns,
            original_row,
            foreign_keys,
            fk_display,
            check_constraints,
            incoming_foreign_keys,
        })));
    }

    if key_columns.is_empty() {
        return not_editable(crate::row_editing::not_editable::no_primary_key(
            &dialect::quote_table(db, &provenance.target),
        ));
    }

    // 3. The full key must be present in the projection, with usable values.
    let mut key: Vec<(String, CellValue)> = Vec::with_capacity(key_columns.len());
    for key_column in &key_columns {
        let ordinal = columns
            .iter()
            .find(|column| &column.name == key_column)
            .and_then(|column| column.result_ordinal);
        let Some(ordinal) = ordinal else {
            return not_editable(crate::row_editing::not_editable::key_column_not_projected(
                &db.quote_identifier(key_column),
            ));
        };
        let value = request.row[ordinal].clone();
        if matches!(value, CellValue::Null) {
            return not_editable(crate::row_editing::not_editable::key_column_null(
                &db.quote_identifier(key_column),
            ));
        }
        key.push((key_column.clone(), value));
    }

    // 4. Fetch the authoritative current row by key.
    let (fetched_names, fetched_rows) = fetch_row_inner(engine, &provenance.target, &key).await?;
    let row = match fetched_rows.len() {
        0 => {
            return not_editable(crate::row_editing::not_editable::row_not_found());
        }
        1 => fetched_rows.into_iter().next().expect("length checked"),
        n => {
            return not_editable(crate::row_editing::not_editable::key_matched_many_rows(n));
        }
    };

    // Align the fetched row with catalog column order by name.
    let mut original_row: Vec<CellValue> = Vec::with_capacity(columns.len());
    for column in &columns {
        let position = fetched_names.iter().position(|name| name == &column.name);
        let Some(position) = position else {
            return Err(crate::row_editing::errors::fetched_row_missing_column(
                &db.quote_identifier(&column.name),
            ));
        };
        original_row.push(row[position].clone());
    }

    let fk_display = preload_fk_display(engine, &foreign_keys, &columns, &original_row).await;

    Ok(RowEditOpenOutcome::Editable(Box::new(RowEditSession {
        target: provenance.target,
        key_columns,
        rowversion_column: None,
        columns,
        original_row,
        foreign_keys,
        fk_display,
        check_constraints,
        incoming_foreign_keys,
    })))
}

/// Re-fetches the row by key. `None` when the row no longer exists.
pub(crate) async fn fetch_row_on<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    target: &TableTarget,
    key: &[(String, CellValue)],
) -> StudioResult<Option<(Vec<String>, Vec<CellValue>)>> {
    let (names, mut rows) = fetch_row_inner(engine, target, key).await?;
    match rows.len() {
        0 => Ok(None),
        1 => Ok(Some((names, rows.remove(0)))),
        n => Err(crate::row_editing::errors::key_matched_rows(n)),
    }
}

/// Guarded, transactional, parameterized UPDATE with the same conflict
/// semantics as the T-SQL path.
pub(crate) async fn update_row_on<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    profile: &ConnectionProfile,
    request: UpdateRowRequest,
) -> StudioResult<UpdateRowOutcome> {
    if let Some(error) = crate::row_editing::errors::policy_blocked(profile) {
        return Err(error);
    }
    if request.changes.is_empty() {
        return Ok(UpdateRowOutcome::NoChanges);
    }
    if request.key.is_empty() {
        return Err(crate::row_editing::errors::no_key_columns_for_update());
    }

    let (update_sql, params) = engine.build_update(&request).await?;
    engine.begin().await?;
    let affected = match engine.execute(&update_sql, &params).await {
        Ok(affected) => affected,
        Err(err) => {
            // Best-effort rollback; the caller drops the connection on error.
            let _ = engine.rollback().await;
            return Err(err);
        }
    };

    match affected {
        1 => {
            let (names, mut rows) = fetch_row_inner(engine, &request.target, &request.key).await?;
            engine.commit().await?;
            let row = if rows.len() == 1 {
                Some(rows.remove(0))
            } else {
                None
            };
            match row {
                Some(row) => Ok(UpdateRowOutcome::Updated {
                    columns: names,
                    row,
                }),
                None => Err(crate::row_editing::errors::update_not_refetched()),
            }
        }
        0 => {
            engine.rollback().await?;
            let (names, mut rows) = fetch_row_inner(engine, &request.target, &request.key).await?;
            let current_row = if rows.len() == 1 {
                Some(rows.remove(0))
            } else {
                None
            };
            Ok(UpdateRowOutcome::Conflict {
                current_columns: names,
                current_row,
            })
        }
        n => {
            engine.rollback().await?;
            Err(crate::row_editing::errors::update_affected_rows(n))
        }
    }
}

/// Guarded, transactional, parameterized INSERT returning the inserted row.
pub(crate) async fn insert_row_on<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    profile: &ConnectionProfile,
    request: InsertRowRequest,
) -> StudioResult<InsertRowOutcome> {
    if let Some(error) = crate::row_editing::errors::policy_blocked(profile) {
        return Err(error);
    }
    engine.begin().await?;
    let (columns, row) = match engine.insert_returning(&request).await {
        Ok(inserted) => inserted,
        Err(err) => {
            let _ = engine.rollback().await;
            return Err(err);
        }
    };
    engine.commit().await?;
    Ok(InsertRowOutcome::Inserted { columns, row })
}

/// Guarded, transactional, parameterized DELETE with the same conflict
/// semantics as the T-SQL path.
pub(crate) async fn delete_row_on<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    profile: &ConnectionProfile,
    request: DeleteRowRequest,
) -> StudioResult<DeleteRowOutcome> {
    if let Some(error) = crate::row_editing::errors::policy_blocked(profile) {
        return Err(error);
    }
    if request.key.is_empty() {
        return Err(crate::row_editing::errors::no_key_columns_for_delete());
    }

    let (delete_sql, params) = engine.build_delete(&request).await?;
    engine.begin().await?;
    let affected = match engine.execute(&delete_sql, &params).await {
        Ok(affected) => affected,
        Err(err) => {
            let _ = engine.rollback().await;
            return Err(err);
        }
    };
    match affected {
        1 => {
            engine.commit().await?;
            Ok(DeleteRowOutcome::Deleted)
        }
        0 => {
            engine.rollback().await?;
            let (names, mut rows) = fetch_row_inner(engine, &request.target, &request.key).await?;
            let current_row = if rows.len() == 1 {
                Some(rows.remove(0))
            } else {
                None
            };
            Ok(DeleteRowOutcome::Conflict {
                current_columns: names,
                current_row,
            })
        }
        n => {
            engine.rollback().await?;
            Err(crate::row_editing::errors::delete_affected_rows(n))
        }
    }
}

/// Applies a staged batch inside one transaction with per-statement affected-row
/// checks matching the single-row guarded flows.
pub(crate) async fn apply_batch_on<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    profile: &ConnectionProfile,
    request: crate::row_editing::BatchRequest,
) -> StudioResult<crate::row_editing::BatchOutcome> {
    use crate::row_editing::BatchOutcome;
    use crate::row_editing::staged::build_batch_statements;

    if let Some(error) = crate::row_editing::errors::policy_blocked(profile) {
        return Err(error);
    }
    let statements = build_batch_statements(engine.database_type(), &request.batch);
    if statements.is_empty() {
        return Ok(BatchOutcome::Committed { statements: 0 });
    }
    engine.begin().await?;
    match engine.run_batch_statements(&statements).await {
        Ok(BatchRun::Applied) => {}
        Ok(BatchRun::Mismatch {
            statement_index,
            affected,
        }) => {
            let _ = engine.rollback().await;
            return Ok(BatchOutcome::Conflict {
                statement_index,
                affected,
            });
        }
        Err(err) => {
            let _ = engine.rollback().await;
            return Err(err);
        }
    }
    engine.commit().await?;
    Ok(BatchOutcome::Committed {
        statements: statements.len(),
    })
}

/// Bounded, read-only FK autocomplete search.
pub(crate) async fn fk_search_on<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    request: FkSearchRequest,
) -> StudioResult<FkSearchResult> {
    if request.key_columns.is_empty() {
        return Err(crate::row_editing::errors::no_key_columns_for_fk_search());
    }
    let limit = request.limit.max(1);
    let has_display = request.display_column.is_some();
    let key_count = request.key_columns.len();
    let (sql, params) = dialect::build_fk_search_sql_for(engine.database_type(), &request);
    let set = engine.query(&sql, &params).await?;
    Ok(parse_fk_candidates(&set, key_count, has_display, limit))
}

/// Bounded, read-only batched FK label lookup.
pub(crate) async fn fk_labels_on<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    request: FkLabelRequest,
) -> StudioResult<FkLabelResult> {
    if request.keys.is_empty() {
        return Ok(FkLabelResult::default());
    }
    let (sql, params) = dialect::build_fk_labels_sql_for(engine.database_type(), &request)?;
    if sql.is_empty() {
        return Ok(FkLabelResult::default());
    }
    let set = engine.query(&sql, &params).await?;
    Ok(parse_fk_labels(&set))
}

/// Bounded, read-only related-rows fetch.
pub(crate) async fn related_rows_on<E: RowEditEngine + ?Sized>(
    engine: &mut E,
    request: RelatedRowsRequest,
) -> StudioResult<RelatedRowsResult> {
    let limit = request.limit.max(1);
    let open_sql = dialect::build_related_open_sql_for(engine.database_type(), &request);
    let (sql, params) = engine.build_related(&request).await?;
    let set = engine.query(&sql, &params).await?;
    let truncated = set.rows.len() > limit;
    let columns = set.columns.iter().map(|c| c.name.clone()).collect();
    let rows = set.rows.into_iter().take(limit).collect();
    Ok(RelatedRowsResult {
        columns,
        rows,
        truncated,
        open_sql,
    })
}

#[cfg(test)]
mod apply_batch_tests {
    use super::*;
    use crate::row_editing::staged::{StagedBatch, StagedOp};
    use crate::row_editing::{
        BatchRequest, DescribeColumn, LoadedTable, TableTarget, UpdateRowRequest,
    };
    use sqlly_core::StudioError;
    use sqlly_core::{CellValue, DatabaseType, MaterializedResultSet, SqlParam};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct CallLog {
        calls: Vec<String>,
    }

    struct FakeEngine {
        db: DatabaseType,
        log: Arc<Mutex<CallLog>>,
        fail_on_execute: bool,
        affected: u64,
    }

    impl FakeEngine {
        fn new(affected: u64) -> Self {
            Self {
                db: DatabaseType::Sqlite,
                log: Arc::new(Mutex::new(CallLog::default())),
                fail_on_execute: false,
                affected,
            }
        }

        fn push(&self, call: &str) {
            self.log.lock().unwrap().calls.push(call.to_string());
        }
    }

    #[async_trait::async_trait]
    impl RowEditEngine for FakeEngine {
        fn database_type(&self) -> DatabaseType {
            self.db
        }

        async fn describe(
            &mut self,
            _sql: &str,
        ) -> StudioResult<Result<Vec<DescribeColumn>, DisplayMessage>> {
            Err(StudioError::Other("unused".into()))
        }

        async fn load_table(
            &mut self,
            _target: &TableTarget,
        ) -> StudioResult<Result<LoadedTable, DisplayMessage>> {
            Err(StudioError::Other("unused".into()))
        }

        async fn query(
            &mut self,
            _sql: &str,
            _params: &[SqlParam],
        ) -> StudioResult<MaterializedResultSet> {
            Err(StudioError::Other("unused".into()))
        }

        async fn execute(&mut self, _sql: &str, _params: &[SqlParam]) -> StudioResult<u64> {
            self.push("execute");
            if self.fail_on_execute {
                return Err(StudioError::QueryExecution("boom".into()));
            }
            Ok(self.affected)
        }

        async fn begin(&mut self) -> StudioResult<()> {
            self.push("begin");
            Ok(())
        }

        async fn commit(&mut self) -> StudioResult<()> {
            self.push("commit");
            Ok(())
        }

        async fn rollback(&mut self) -> StudioResult<()> {
            self.push("rollback");
            Ok(())
        }
    }

    fn sample_batch() -> BatchRequest {
        BatchRequest {
            batch: StagedBatch {
                target: Some(TableTarget {
                    schema: String::new(),
                    table: "t".to_string(),
                }),
                key_columns: vec!["id".to_string()],
                ops: vec![StagedOp::Update(UpdateRowRequest {
                    target: TableTarget {
                        schema: String::new(),
                        table: "t".to_string(),
                    },
                    key: vec![("id".to_string(), CellValue::I64(1))],
                    rowversion: None,
                    changes: vec![("name".to_string(), CellValue::String("n".to_string()))],
                    original_values: vec![("name".to_string(), CellValue::String("o".to_string()))],
                    expressions: Vec::new(),
                })],
            },
        }
    }

    fn profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "t",
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".to_string(),
            },
        )
    }

    #[tokio::test]
    async fn apply_batch_commits_after_all_statements() {
        let mut engine = FakeEngine::new(1);
        let log = engine.log.clone();
        let outcome = apply_batch_on(&mut engine, &profile(), sample_batch())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            crate::row_editing::BatchOutcome::Committed { statements: 1 }
        );
        let calls = log.lock().unwrap().calls.clone();
        assert_eq!(calls, vec!["begin", "execute", "commit"]);
    }

    #[tokio::test]
    async fn apply_batch_rolls_back_on_execute_error_and_returns_err() {
        let mut engine = FakeEngine::new(1);
        engine.fail_on_execute = true;
        let log = engine.log.clone();
        let err = apply_batch_on(&mut engine, &profile(), sample_batch())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("boom"));
        let calls = log.lock().unwrap().calls.clone();
        assert_eq!(calls, vec!["begin", "execute", "rollback"]);
    }

    #[tokio::test]
    async fn apply_batch_rolls_back_on_affected_mismatch_and_returns_conflict() {
        let mut engine = FakeEngine::new(0);
        let log = engine.log.clone();
        let outcome = apply_batch_on(&mut engine, &profile(), sample_batch())
            .await
            .unwrap();
        assert_eq!(
            outcome,
            crate::row_editing::BatchOutcome::Conflict {
                statement_index: 0,
                affected: 0
            }
        );
        let calls = log.lock().unwrap().calls.clone();
        assert_eq!(calls, vec!["begin", "execute", "rollback"]);
    }

    #[test]
    fn batch_count_check_reports_the_first_mismatch_and_ignores_unchecked_statements() {
        use crate::row_editing::staged::StagedStatement;
        let statement = |expect: Option<u64>| StagedStatement {
            sql: String::new(),
            params: Vec::new(),
            expect_affected: expect,
        };
        let statements = vec![statement(Some(1)), statement(None), statement(Some(1))];
        assert_eq!(
            check_batch_counts(&statements, &[1, 7, 1]),
            BatchRun::Applied
        );
        assert_eq!(
            check_batch_counts(&statements, &[1, 0, 0]),
            BatchRun::Mismatch {
                statement_index: 2,
                affected: 0
            }
        );
        // A statement that never ran (the batch stopped early) counts as 0.
        assert_eq!(
            check_batch_counts(&statements, &[2]),
            BatchRun::Mismatch {
                statement_index: 0,
                affected: 2
            }
        );
    }

    #[tokio::test]
    async fn apply_batch_refuses_select_only_profile_without_begin() {
        let mut engine = FakeEngine::new(1);
        let mut profile = profile();
        profile.block_all_but_select = true;
        let log = engine.log.clone();
        let err = apply_batch_on(&mut engine, &profile, sample_batch())
            .await
            .unwrap_err();
        assert!(matches!(err, StudioError::PermissionDeniedMessage(_)));
        assert!(log.lock().unwrap().calls.is_empty());
    }
}
