//! The real-I/O row-editing service: warm-lane connection checkout, the SQL
//! Server (TDS) implementation of every operation, and the per-engine
//! dispatch. This whole module is compiled out on wasm (see `mod.rs`).

use async_trait::async_trait;
use sqlly_core::{
    CellValue, ConnectionProfile, DisplayMessage, MaterializedResultSet, SqlParam, StudioError,
    StudioResult,
};
use sqlly_engines::tds::{
    TdsClient, execute_batch_on_client, execute_params_on_client, open_client, query_rows_on_client,
};

use super::introspect::{
    FkColumnRow, IncomingFkColumnRow, LoadedTable, Provenance, analyze_provenance,
    declared_char_length, fill_enum_values_from_checks, group_foreign_keys,
    group_incoming_foreign_keys, parse_describe_result,
};
use super::sql::{
    build_delete_sql, build_fk_labels_sql, build_fk_search_sql, build_insert_sql,
    build_related_open_sql, build_related_rows_sql, build_select_row_sql, build_update_sql,
    parse_fk_candidates, parse_fk_labels, push_predicate, quote_ident, render_cell,
};
use super::staged::build_batch_statements;
use super::types::{
    BatchOutcome, BatchRequest, CheckConstraintSpec, DeleteRowOutcome, DeleteRowRequest,
    FkLabelRequest, FkLabelResult, FkSearchRequest, FkSearchResult, ForeignKeySpec,
    InsertRowOutcome, InsertRowRequest, RelatedRowsRequest, RelatedRowsResult, RowEditColumn,
    RowEditOpenOutcome, RowEditOpenRequest, RowEditSession, RowEditingService, TableTarget,
    UpdateRowOutcome, UpdateRowRequest, policy_block_message,
};

/// SQL that loads table columns (result set 1), primary-key columns (result
/// set 2), a base-table check (result set 3), outgoing foreign keys with a
/// display column per referenced table (result set 4), enabled `CHECK`
/// constraints (result set 5), and incoming foreign keys — child tables that
/// reference this one (result set 6) — for `@P1` = a two-part quoted table
/// name.
const TABLE_METADATA_SQL: &str = "\
SELECT c.name, t.name AS type_name, CONVERT(bigint, c.max_length) AS max_length, \
CONVERT(int, c.precision) AS precision, CONVERT(int, c.scale) AS scale, \
c.is_nullable, c.is_identity, c.is_computed, \
CONVERT(bit, CASE WHEN t.name IN ('timestamp','rowversion') THEN 1 ELSE 0 END) AS is_rowversion, \
dc.definition AS default_definition \
FROM sys.columns c \
JOIN sys.types t ON t.user_type_id = c.user_type_id \
LEFT JOIN sys.default_constraints dc \
ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
WHERE c.object_id = OBJECT_ID(@P1) \
ORDER BY c.column_id;\n\
SELECT col.name \
FROM sys.indexes i \
JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
JOIN sys.columns col ON col.object_id = ic.object_id AND col.column_id = ic.column_id \
WHERE i.object_id = OBJECT_ID(@P1) AND i.is_primary_key = 1 \
ORDER BY ic.key_ordinal;\n\
SELECT CONVERT(bit, CASE WHEN EXISTS (SELECT 1 FROM sys.tables WHERE object_id = OBJECT_ID(@P1)) \
THEN 1 ELSE 0 END) AS is_base_table;\n\
SELECT fk.name AS constraint_name, SCHEMA_NAME(ro.schema_id) AS ref_schema, \
ro.name AS ref_table, pc.name AS parent_column, rc.name AS ref_column, \
fkc.constraint_column_id AS ordinal, \
(SELECT TOP 1 dc.name FROM sys.columns dc \
JOIN sys.types dty ON dty.user_type_id = dc.user_type_id \
WHERE dc.object_id = fk.referenced_object_id \
AND dty.name IN ('varchar','nvarchar','char','nchar','text','ntext','sysname') \
AND LOWER(dc.name) IN ('name','displayname','fullname','title','label','description') \
ORDER BY CASE LOWER(dc.name) WHEN 'name' THEN 0 WHEN 'displayname' THEN 1 \
WHEN 'fullname' THEN 2 WHEN 'title' THEN 3 WHEN 'label' THEN 4 \
WHEN 'description' THEN 5 ELSE 6 END, dc.column_id) AS display_column \
FROM sys.foreign_keys fk \
JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
JOIN sys.objects ro ON ro.object_id = fk.referenced_object_id \
JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
WHERE fk.parent_object_id = OBJECT_ID(@P1) \
ORDER BY fk.name, fkc.constraint_column_id;\n\
SELECT cc.name AS constraint_name, cc.definition AS definition \
FROM sys.check_constraints cc \
WHERE cc.parent_object_id = OBJECT_ID(@P1) AND cc.is_disabled = 0 \
ORDER BY cc.name;\n\
SELECT fk.name AS constraint_name, SCHEMA_NAME(co.schema_id) AS child_schema, \
co.name AS child_table, cpc.name AS child_column, rpc.name AS this_column, \
fkc.constraint_column_id AS ordinal \
FROM sys.foreign_keys fk \
JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
JOIN sys.objects co ON co.object_id = fk.parent_object_id \
JOIN sys.columns cpc ON cpc.object_id = fkc.parent_object_id AND cpc.column_id = fkc.parent_column_id \
JOIN sys.columns rpc ON rpc.object_id = fkc.referenced_object_id AND rpc.column_id = fkc.referenced_column_id \
WHERE fk.referenced_object_id = OBJECT_ID(@P1) \
ORDER BY fk.name, fkc.constraint_column_id;";

/// Default row-editing service backed by `sqlly-tds`. Runs on a dedicated
/// warm-client lane (`tds_pool::ROW_EDIT_LANE`) so row operations skip the
/// fresh-connect handshake — the dominant cost for Entra profiles — without
/// ever sharing a client with general query lanes: row-editing sessions set
/// `XACT_ABORT ON` and run explicit transactions, session state a query lane
/// must never inherit.
///
/// Lane discipline: a checked-out client is returned to the lane ONLY after
/// it is provably clean — a successful `COMMIT`, or a successful `ROLLBACK`
/// followed by a successful read. Any error path drops the client (an
/// errored TDS stream or an ambiguous transaction state is never reused).
pub struct DefaultRowEditingService {
    pool: crate::tds_pool::TdsPool,
    /// Warm PostgreSQL clients on the row-editing lane.
    pg_pool: crate::tds_pool::PgPool,
    /// Warm MySQL clients on the row-editing lane. These are opened with
    /// `CLIENT_FOUND_ROWS` (guarded UPDATEs need matched-row counts), which
    /// is exactly why they live on their own lane and never mix with query
    /// pools.
    mysql_pool: crate::tds_pool::MySqlPool,
    /// Warm Oracle sessions on the row-editing lane: opened with autocommit
    /// off and the NLS formats pinned (see `row_editing_oracle`), session
    /// state a query lane must never inherit.
    oracle_pool: crate::tds_pool::OraclePool,
}

/// RAII guard for a row-editing lane client. Dropping without
/// [`mark_healthy`](LaneCheckout::mark_healthy) discards the client; marking
/// healthy returns it to the lane on drop. This makes every `?` early return
/// automatically discard, so check-in happens only at explicitly vetted
/// points.
struct LaneCheckout {
    pool: crate::tds_pool::TdsPool,
    key: String,
    client: Option<TdsClient>,
    healthy: bool,
}

impl LaneCheckout {
    fn client(&mut self) -> &mut TdsClient {
        self.client.as_mut().expect("lane client taken")
    }

    /// Certify the session as clean (transaction committed/rolled back, no
    /// stream error). The client returns to the lane when the guard drops.
    fn mark_healthy(&mut self) {
        self.healthy = true;
    }
}

impl Drop for LaneCheckout {
    fn drop(&mut self) {
        if self.healthy
            && let Some(client) = self.client.take()
        {
            self.pool.checkin(std::mem::take(&mut self.key), client);
        }
    }
}

impl Default for DefaultRowEditingService {
    fn default() -> Self {
        Self::new(crate::tds_pool::TdsPool::new())
    }
}

/// RAII guard for a warm-pool client on the row-editing lane — the
/// PostgreSQL/MySQL mirror of [`LaneCheckout`], generic over the client
/// type. Dropping without [`mark_healthy`](WarmCheckout::mark_healthy)
/// discards the client; marking healthy returns it to the lane on drop, so
/// every `?` early return automatically discards a possibly-dirty session.
struct WarmCheckout<C: Send + 'static> {
    pool: crate::tds_pool::WarmPool<C>,
    key: String,
    client: Option<C>,
    healthy: bool,
}

impl<C: Send + 'static> WarmCheckout<C> {
    fn client(&self) -> &C {
        self.client.as_ref().expect("lane client taken")
    }

    /// Certify the session as clean (transaction committed/rolled back, no
    /// stream error). The client returns to the lane when the guard drops.
    fn mark_healthy(&mut self) {
        self.healthy = true;
    }
}

impl<C: Send + 'static> Drop for WarmCheckout<C> {
    fn drop(&mut self) {
        if self.healthy
            && let Some(client) = self.client.take()
        {
            self.pool.checkin(std::mem::take(&mut self.key), client);
        }
    }
}

/// A checked-out per-operation connection for one of the natively driven
/// engines, ready to hand a [`RowEditEngine`](crate::row_editing_engines::RowEditEngine)
/// to the generic flows. SQLite and DuckDB are in-process and unpooled: a
/// client per operation, dropped (with an implicit rollback of anything
/// open) at the end. libSQL is one HTTP client per operation whose
/// transaction lives on a server-side stream the engine closes itself.
enum EngineLane {
    Pg(WarmCheckout<sqlly_engines::pg::PgClient>),
    MySql(WarmCheckout<sqlly_engines::mysql::MySqlClient>),
    Sqlite(sqlly_engines::sqlite::SqliteClient),
    LibSql(sqlly_engines::libsql::LibSqlClient),
    Duck(sqlly_engines::duckdb::DuckDbClient),
    Oracle(WarmCheckout<sqlly_engines::oracle::OracleClient>),
}

impl EngineLane {
    fn engine(
        &self,
        db: sqlly_core::DatabaseType,
    ) -> Box<dyn crate::row_editing_engines::RowEditEngine + '_> {
        match self {
            EngineLane::Pg(lane) => {
                Box::new(crate::row_editing_pg::PgEngine::new(lane.client(), db))
            }
            EngineLane::MySql(lane) => Box::new(crate::row_editing_mysql::MySqlEngine::new(
                lane.client(),
                db,
            )),
            EngineLane::Sqlite(client) => {
                Box::new(crate::row_editing_sqlite::SqliteEngine::new(client, db))
            }
            EngineLane::LibSql(client) => {
                Box::new(crate::row_editing_libsql::LibSqlEngine::new(client, db))
            }
            EngineLane::Duck(client) => {
                Box::new(crate::row_editing_duckdb::DuckDbEngine::new(client, db))
            }
            EngineLane::Oracle(lane) => Box::new(crate::row_editing_oracle::OracleEngine::new(
                lane.client(),
                db,
            )),
        }
    }

    /// Certify the pooled client (if any) as clean so it returns to its lane.
    fn mark_healthy(&mut self) {
        match self {
            EngineLane::Pg(lane) => lane.mark_healthy(),
            EngineLane::MySql(lane) => lane.mark_healthy(),
            EngineLane::Sqlite(_) | EngineLane::LibSql(_) | EngineLane::Duck(_) => {}
            EngineLane::Oracle(lane) => lane.mark_healthy(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure open-session steps (extracted from the TDS open flow)
// ---------------------------------------------------------------------------

/// Runs `sp_describe_first_result_set` for the request's statement and decides
/// whether the projection maps onto exactly one editable base table.
/// `Err(reason)` carries the user-facing "not editable" explanation — expected
/// for dynamic SQL, temp tables, procedures, joins, and shape mismatches —
/// never a transport error to bubble up.
async fn resolve_provenance(
    client: &mut TdsClient,
    request: &RowEditOpenRequest,
) -> Result<Provenance, DisplayMessage> {
    let describe = query_rows_on_client(
        client,
        "EXEC sp_describe_first_result_set @tsql = @P1, @params = NULL, \
         @browse_information_mode = 1",
        &[SqlParam::String(request.sql.clone())],
    )
    .await;
    let described = match describe {
        Ok(sets) => match sets.first() {
            Some(set) => parse_describe_result(set),
            None => Vec::new(),
        },
        Err(err) => {
            return Err(crate::row_editing::not_editable::provenance_failed(err));
        }
    };
    analyze_provenance(&described, &request.columns)
}

/// Stamps each catalog column with its ordinal in the projected result, when
/// the column was selected.
fn assign_result_ordinals(columns: &mut [RowEditColumn], provenance: &Provenance) {
    for column in columns.iter_mut() {
        column.result_ordinal = provenance
            .source_ordinals
            .iter()
            .find(|(name, _)| *name == column.name)
            .map(|(_, ordinal)| *ordinal);
    }
}

/// Extracts the full primary key from the clicked row. `Err(reason)` when a
/// key column is missing from the projection or NULL in the row (the row
/// cannot be identified safely).
fn key_from_projection(
    columns: &[RowEditColumn],
    key_columns: &[String],
    row: &[CellValue],
) -> Result<Vec<(String, CellValue)>, DisplayMessage> {
    let mut key: Vec<(String, CellValue)> = Vec::with_capacity(key_columns.len());
    for key_column in key_columns {
        let ordinal = columns
            .iter()
            .find(|column| &column.name == key_column)
            .and_then(|column| column.result_ordinal);
        let Some(ordinal) = ordinal else {
            return Err(crate::row_editing::not_editable::key_column_not_projected(
                &quote_ident(key_column),
            ));
        };
        let value = row[ordinal].clone();
        if matches!(value, CellValue::Null) {
            return Err(crate::row_editing::not_editable::key_column_null(
                &quote_ident(key_column),
            ));
        }
        key.push((key_column.clone(), value));
    }
    Ok(key)
}

/// Aligns a fetched row (`SELECT *` order) with catalog column order by name.
fn align_row_to_catalog(
    columns: &[RowEditColumn],
    fetched_names: &[String],
    row: &[CellValue],
) -> StudioResult<Vec<CellValue>> {
    let mut original_row: Vec<CellValue> = Vec::with_capacity(columns.len());
    for column in columns {
        let position = fetched_names.iter().position(|name| name == &column.name);
        let Some(position) = position else {
            return Err(crate::row_editing::errors::fetched_row_missing_column(
                &quote_ident(&column.name),
            ));
        };
        original_row.push(row[position].clone());
    }
    Ok(original_row)
}

/// Name of the table's rowversion/timestamp column, when present.
fn rowversion_column_of(columns: &[RowEditColumn]) -> Option<String> {
    columns
        .iter()
        .find(|column| column.is_rowversion)
        .map(|column| column.name.clone())
}

impl DefaultRowEditingService {
    pub fn new(pool: crate::tds_pool::TdsPool) -> Self {
        pool.spawn_reaper();
        let pg_pool = crate::tds_pool::PgPool::new();
        pg_pool.spawn_reaper();
        let mysql_pool = crate::tds_pool::MySqlPool::new();
        mysql_pool.spawn_reaper();
        let oracle_pool = crate::tds_pool::OraclePool::new();
        oracle_pool.spawn_reaper();
        Self {
            pool,
            pg_pool,
            mysql_pool,
            oracle_pool,
        }
    }

    async fn register_tds_lane_cancel(
        profile: &ConnectionProfile,
        lane: &mut LaneCheckout,
        cancel_slot: &crate::CancelSlot,
    ) {
        if let Ok(handle) = sqlly_engines::tds::cancel_handle(lane.client(), profile).await {
            crate::register_tds_cancel(cancel_slot, handle);
        }
    }

    async fn register_engine_lane_cancel(
        profile: &ConnectionProfile,
        lane: &mut EngineLane,
        cancel_slot: &crate::CancelSlot,
    ) {
        match lane {
            EngineLane::Pg(checkout) => crate::register_pg_cancel(cancel_slot, checkout.client()),
            EngineLane::MySql(checkout) => {
                crate::register_mysql_cancel(checkout.client(), profile, cancel_slot).await;
            }
            EngineLane::Sqlite(client) => crate::register_sqlite_cancel(cancel_slot, client),
            EngineLane::LibSql(client) => crate::query::register_libsql_cancel(cancel_slot, client),
            EngineLane::Duck(client) => crate::query::register_duckdb_cancel(cancel_slot, client),
            EngineLane::Oracle(checkout) => {
                crate::register_oracle_cancel(cancel_slot, checkout.client());
            }
        }
    }

    /// Engine subprocess path: same as [`RowEditingService::update_row`] but
    /// registers a server-side cancel hook before the guarded write runs.
    pub async fn update_row_cancellable(
        &self,
        profile: &ConnectionProfile,
        request: UpdateRowRequest,
        cancel_slot: &crate::CancelSlot,
    ) -> StudioResult<UpdateRowOutcome> {
        self.update_row_with_cancel(profile, request, Some(cancel_slot))
            .await
    }

    /// Engine subprocess path for INSERT.
    pub async fn insert_row_cancellable(
        &self,
        profile: &ConnectionProfile,
        request: InsertRowRequest,
        cancel_slot: &crate::CancelSlot,
    ) -> StudioResult<InsertRowOutcome> {
        self.insert_row_with_cancel(profile, request, Some(cancel_slot))
            .await
    }

    /// Engine subprocess path for DELETE.
    pub async fn delete_row_cancellable(
        &self,
        profile: &ConnectionProfile,
        request: DeleteRowRequest,
        cancel_slot: &crate::CancelSlot,
    ) -> StudioResult<DeleteRowOutcome> {
        self.delete_row_with_cancel(profile, request, Some(cancel_slot))
            .await
    }

    async fn update_row_with_cancel(
        &self,
        profile: &ConnectionProfile,
        request: UpdateRowRequest,
        cancel_slot: Option<&crate::CancelSlot>,
    ) -> StudioResult<UpdateRowOutcome> {
        if profile.database_type.connection_kind() == sqlly_core::ConnectionKind::Tds {
            return self.update_row_tds(profile, request, cancel_slot).await;
        }
        let mut lane = self.checkout_engine(profile).await?;
        if let Some(slot) = cancel_slot {
            Self::register_engine_lane_cancel(profile, &mut lane, slot).await;
        }
        let result = {
            let mut engine = lane.engine(profile.database_type);
            crate::row_editing_engines::update_row_on(engine.as_mut(), profile, request).await
        };
        if result.is_ok() {
            lane.mark_healthy();
        }
        result
    }

    async fn insert_row_with_cancel(
        &self,
        profile: &ConnectionProfile,
        request: InsertRowRequest,
        cancel_slot: Option<&crate::CancelSlot>,
    ) -> StudioResult<InsertRowOutcome> {
        if profile.database_type.connection_kind() == sqlly_core::ConnectionKind::Tds {
            return self.insert_row_tds(profile, request, cancel_slot).await;
        }
        let mut lane = self.checkout_engine(profile).await?;
        if let Some(slot) = cancel_slot {
            Self::register_engine_lane_cancel(profile, &mut lane, slot).await;
        }
        let result = {
            let mut engine = lane.engine(profile.database_type);
            crate::row_editing_engines::insert_row_on(engine.as_mut(), profile, request).await
        };
        if result.is_ok() {
            lane.mark_healthy();
        }
        result
    }

    /// Engine subprocess path for staged batch commit.
    pub async fn apply_batch_cancellable(
        &self,
        profile: &ConnectionProfile,
        request: BatchRequest,
        cancel_slot: &crate::CancelSlot,
    ) -> StudioResult<BatchOutcome> {
        self.apply_batch_with_cancel(profile, request, Some(cancel_slot))
            .await
    }

    async fn apply_batch_with_cancel(
        &self,
        profile: &ConnectionProfile,
        request: BatchRequest,
        cancel_slot: Option<&crate::CancelSlot>,
    ) -> StudioResult<BatchOutcome> {
        if profile.database_type.connection_kind() == sqlly_core::ConnectionKind::Tds {
            return self.apply_batch_tds(profile, request, cancel_slot).await;
        }
        let mut lane = self.checkout_engine(profile).await?;
        if let Some(slot) = cancel_slot {
            Self::register_engine_lane_cancel(profile, &mut lane, slot).await;
        }
        let result = {
            let mut engine = lane.engine(profile.database_type);
            crate::row_editing_engines::apply_batch_on(engine.as_mut(), profile, request).await
        };
        if result.is_ok() {
            lane.mark_healthy();
        }
        result
    }

    async fn apply_batch_tds(
        &self,
        profile: &ConnectionProfile,
        request: BatchRequest,
        cancel_slot: Option<&crate::CancelSlot>,
    ) -> StudioResult<BatchOutcome> {
        if let Some(error) = crate::row_editing::errors::policy_blocked(profile) {
            return Err(error);
        }
        let statements = build_batch_statements(profile.database_type, &request.batch);
        if statements.is_empty() {
            return Ok(BatchOutcome::Committed { statements: 0 });
        }

        let mut lane = self.checkout(profile).await?;
        if let Some(slot) = cancel_slot {
            Self::register_tds_lane_cancel(profile, &mut lane, slot).await;
        }
        let client = lane.client();

        execute_batch_on_client(client, "SET XACT_ABORT ON; BEGIN TRANSACTION;").await?;
        for (index, statement) in statements.iter().enumerate() {
            let affected = match execute_params_on_client(client, &statement.sql, &statement.params)
                .await
            {
                Ok(affected) => affected,
                Err(err) => {
                    let _ =
                        execute_batch_on_client(client, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;")
                            .await;
                    return Err(err);
                }
            };
            if let Some(expected) = statement.expect_affected
                && affected != expected
            {
                execute_batch_on_client(client, "ROLLBACK TRANSACTION;").await?;
                lane.mark_healthy();
                return Ok(BatchOutcome::Conflict {
                    statement_index: index,
                    affected,
                });
            }
        }
        execute_batch_on_client(client, "COMMIT TRANSACTION;").await?;
        lane.mark_healthy();
        Ok(BatchOutcome::Committed {
            statements: statements.len(),
        })
    }

    async fn delete_row_with_cancel(
        &self,
        profile: &ConnectionProfile,
        request: DeleteRowRequest,
        cancel_slot: Option<&crate::CancelSlot>,
    ) -> StudioResult<DeleteRowOutcome> {
        if profile.database_type.connection_kind() == sqlly_core::ConnectionKind::Tds {
            return self.delete_row_tds(profile, request, cancel_slot).await;
        }
        let mut lane = self.checkout_engine(profile).await?;
        if let Some(slot) = cancel_slot {
            Self::register_engine_lane_cancel(profile, &mut lane, slot).await;
        }
        let result = {
            let mut engine = lane.engine(profile.database_type);
            crate::row_editing_engines::delete_row_on(engine.as_mut(), profile, request).await
        };
        if result.is_ok() {
            lane.mark_healthy();
        }
        result
    }

    /// The clean capability error for engines without a row-editing path.
    fn unsupported(profile: &ConnectionProfile) -> StudioError {
        crate::row_editing::errors::unsupported_engine(profile)
    }

    /// Check out (or open) the per-engine connection for a non-TDS profile.
    async fn checkout_engine(&self, profile: &ConnectionProfile) -> StudioResult<EngineLane> {
        use sqlly_core::ConnectionKind;
        match profile.database_type.connection_kind() {
            ConnectionKind::PostgresWire => {
                let key = crate::tds_pool::PgPool::pool_key(
                    Some(crate::tds_pool::ROW_EDIT_LANE),
                    profile,
                );
                let client = match self.pg_pool.checkout(&key) {
                    Some(client) => client,
                    None => sqlly_engines::pg::open_client(profile).await?,
                };
                Ok(EngineLane::Pg(WarmCheckout {
                    pool: self.pg_pool.clone(),
                    key,
                    client: Some(client),
                    healthy: false,
                }))
            }
            ConnectionKind::MySqlWire => {
                let key = crate::tds_pool::MySqlPool::pool_key(
                    Some(crate::tds_pool::ROW_EDIT_LANE),
                    profile,
                );
                let client = match self.mysql_pool.checkout(&key) {
                    Some(client) => client,
                    None => sqlly_engines::mysql::open_client_found_rows(profile).await?,
                };
                Ok(EngineLane::MySql(WarmCheckout {
                    pool: self.mysql_pool.clone(),
                    key,
                    client: Some(client),
                    healthy: false,
                }))
            }
            ConnectionKind::SqliteFile => Ok(EngineLane::Sqlite(
                sqlly_engines::sqlite::open_client(profile).await?,
            )),
            ConnectionKind::HranaHttp => Ok(EngineLane::LibSql(
                sqlly_engines::libsql::open_client(profile).await?,
            )),
            ConnectionKind::DuckDbFile => Ok(EngineLane::Duck(
                sqlly_engines::duckdb::open_client(profile).await?,
            )),
            ConnectionKind::OracleNet => {
                let key = crate::tds_pool::OraclePool::pool_key(
                    Some(crate::tds_pool::ROW_EDIT_LANE),
                    profile,
                );
                let client = match self.oracle_pool.checkout(&key) {
                    Some(client) => client,
                    None => {
                        // Manual commit for the guarded transactions, and the
                        // driver's own NLS formats so typeless string binds
                        // (staged batches) convert to DATE / TIMESTAMP.
                        let client =
                            sqlly_engines::oracle::open_client_manual_commit(profile).await?;
                        sqlly_engines::oracle::execute_params_on_client(
                            &client,
                            &crate::row_editing_oracle::session_setup_sql(),
                            &[],
                        )
                        .await?;
                        client
                    }
                };
                Ok(EngineLane::Oracle(WarmCheckout {
                    pool: self.oracle_pool.clone(),
                    key,
                    client: Some(client),
                    healthy: false,
                }))
            }
            _ => Err(Self::unsupported(profile)),
        }
    }

    /// Check out the row-editing lane's warm client, opening a fresh
    /// connection on a miss. No stale-warm retry here (unlike the query
    /// lane): row operations are user-paced, and an implicit retry of a
    /// transactional write is only provably safe for the first statement —
    /// keep the failure visible and let the next attempt open fresh.
    async fn checkout(&self, profile: &ConnectionProfile) -> StudioResult<LaneCheckout> {
        let key = crate::tds_pool::TdsPool::pool_key(Some(crate::tds_pool::ROW_EDIT_LANE), profile);
        let client = match self.pool.checkout(&key) {
            Some(client) => client,
            None => open_client(profile).await?,
        };
        Ok(LaneCheckout {
            pool: self.pool.clone(),
            key,
            client: Some(client),
            healthy: false,
        })
    }

    async fn load_table(
        &self,
        client: &mut TdsClient,
        target: &TableTarget,
    ) -> StudioResult<Result<LoadedTable, DisplayMessage>> {
        let table_name = target.quoted();
        let sets = query_rows_on_client(
            client,
            TABLE_METADATA_SQL,
            &[SqlParam::String(table_name.clone())],
        )
        .await?;
        if sets.len() < 6 {
            return Err(crate::row_editing::errors::metadata_result_sets(sets.len()));
        }
        Ok(parse_table_metadata(&sets, &table_name))
    }

    /// Resolves the display label for each single-column FK's current value, so
    /// the form can show `Name` next to a raw key on open. Composite FKs and
    /// FKs without a display column resolve to `None`. Best-effort: a failed
    /// lookup yields `None` rather than aborting the open.
    async fn preload_fk_display(
        &self,
        client: &mut TdsClient,
        foreign_keys: &[ForeignKeySpec],
        columns: &[RowEditColumn],
        original_row: &[CellValue],
    ) -> Vec<Option<String>> {
        let mut labels = Vec::with_capacity(foreign_keys.len());
        for fk in foreign_keys {
            labels.push(
                self.resolve_one_fk_display(client, fk, columns, original_row)
                    .await
                    .unwrap_or(None),
            );
        }
        labels
    }

    async fn resolve_one_fk_display(
        &self,
        client: &mut TdsClient,
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
        let key = vec![(referenced.clone(), value)];
        let (sql, params) = {
            let mut params: Vec<SqlParam> = Vec::new();
            let mut predicates: Vec<String> = Vec::new();
            for (column, value) in &key {
                push_predicate(&mut predicates, &mut params, column, value);
            }
            (
                format!(
                    "SELECT TOP (1) {} FROM {} WHERE {}",
                    quote_ident(&display),
                    fk.referenced.quoted(),
                    predicates.join(" AND ")
                ),
                params,
            )
        };
        let sets = query_rows_on_client(client, &sql, &params).await?;
        let label = sets
            .first()
            .and_then(|set| set.rows.first())
            .and_then(|row| row.first())
            .and_then(|cell| match cell {
                CellValue::Null => None,
                other => Some(render_cell(other)),
            });
        Ok(label)
    }

    async fn fetch_row_on_client(
        &self,
        client: &mut TdsClient,
        target: &TableTarget,
        key: &[(String, CellValue)],
    ) -> StudioResult<(Vec<String>, Vec<Vec<CellValue>>)> {
        let (sql, params) = build_select_row_sql(target, key);
        let sets = query_rows_on_client(client, &sql, &params).await?;
        let set = sets.into_iter().next().unwrap_or_default();
        let names = set.columns.iter().map(|c| c.name.clone()).collect();
        Ok((names, set.rows))
    }
}

/// Parses the six result sets of [`TABLE_METADATA_SQL`] into a
/// [`LoadedTable`]. `Err(reason)` carries the user-facing "not editable"
/// explanation (view, unknown table). Callers guarantee `sets.len() >= 6`.
fn parse_table_metadata(
    sets: &[MaterializedResultSet],
    table_name: &str,
) -> Result<LoadedTable, DisplayMessage> {
    let is_base_table = matches!(
        sets[2].rows.first().and_then(|row| row.first()),
        Some(CellValue::Bool(true)) | Some(CellValue::I64(1))
    );
    if !is_base_table {
        return Err(crate::row_editing::not_editable::not_base_table(table_name));
    }

    let cell_str = |cell: Option<&CellValue>| match cell {
        Some(CellValue::String(s)) => Some(s.clone()),
        _ => None,
    };
    let cell_i64 = |cell: Option<&CellValue>| match cell {
        Some(CellValue::I64(n)) => Some(*n),
        _ => None,
    };
    let cell_bool = |cell: Option<&CellValue>| {
        matches!(cell, Some(CellValue::Bool(true)) | Some(CellValue::I64(1)))
    };

    let columns: Vec<RowEditColumn> = sets[0]
        .rows
        .iter()
        .map(|row| {
            let type_name = cell_str(row.get(1)).unwrap_or_default();
            let max_length = cell_i64(row.get(2)).unwrap_or(0);
            RowEditColumn {
                name: cell_str(row.first()).unwrap_or_default(),
                char_max_length: declared_char_length(&type_name, max_length),
                precision: cell_i64(row.get(3)).map(|p| p as u8),
                scale: cell_i64(row.get(4)).map(|s| s as u8),
                nullable: cell_bool(row.get(5)),
                is_primary_key: false, // filled in below
                is_identity: cell_bool(row.get(6)),
                is_computed: cell_bool(row.get(7)),
                is_rowversion: cell_bool(row.get(8)),
                default_definition: cell_str(row.get(9)),
                result_ordinal: None, // filled in by open_session
                data_type: type_name,
                enum_values: Vec::new(),
            }
        })
        .collect();
    if columns.is_empty() {
        return Err(crate::row_editing::not_editable::table_not_found(
            table_name,
        ));
    }

    let key_columns: Vec<String> = sets[1]
        .rows
        .iter()
        .filter_map(|row| cell_str(row.first()))
        .collect();

    let mut columns = columns;
    for column in &mut columns {
        column.is_primary_key = key_columns.contains(&column.name);
    }

    let fk_rows: Vec<FkColumnRow> = sets[3]
        .rows
        .iter()
        .filter_map(|row| {
            let constraint_name = cell_str(row.first())?;
            Some(FkColumnRow {
                constraint_name,
                ref_schema: cell_str(row.get(1)).unwrap_or_default(),
                ref_table: cell_str(row.get(2)).unwrap_or_default(),
                parent_column: cell_str(row.get(3)).unwrap_or_default(),
                referenced_column: cell_str(row.get(4)).unwrap_or_default(),
                display_column: cell_str(row.get(6)),
            })
        })
        .collect();
    let foreign_keys = group_foreign_keys(&fk_rows);

    let check_constraints: Vec<CheckConstraintSpec> = sets[4]
        .rows
        .iter()
        .filter_map(|row| {
            let name = cell_str(row.first())?;
            let definition = cell_str(row.get(1))?;
            Some(CheckConstraintSpec { name, definition })
        })
        .collect();

    fill_enum_values_from_checks(&mut columns, &check_constraints);

    let incoming_rows: Vec<IncomingFkColumnRow> = sets[5]
        .rows
        .iter()
        .filter_map(|row| {
            let constraint_name = cell_str(row.first())?;
            Some(IncomingFkColumnRow {
                constraint_name,
                child_schema: cell_str(row.get(1)).unwrap_or_default(),
                child_table: cell_str(row.get(2)).unwrap_or_default(),
                child_column: cell_str(row.get(3)).unwrap_or_default(),
                this_column: cell_str(row.get(4)).unwrap_or_default(),
            })
        })
        .collect();
    let incoming_foreign_keys = group_incoming_foreign_keys(&incoming_rows);

    Ok(LoadedTable {
        columns,
        key_columns,
        foreign_keys,
        check_constraints,
        incoming_foreign_keys,
    })
}

/// The SQL Server (TDS) implementations of every operation, byte-for-byte
/// the original single-engine service. The `RowEditingService` impl below
/// dispatches here for `ConnectionKind::Tds` profiles and to the
/// engine-generic flows for PostgreSQL/MySQL/SQLite.
impl DefaultRowEditingService {
    async fn open_session_tds(
        &self,
        profile: &ConnectionProfile,
        request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome> {
        let not_editable = |reason: DisplayMessage| Ok(RowEditOpenOutcome::not_editable(reason));

        if let Some(reason) = policy_block_message(profile) {
            return not_editable(reason);
        }
        if request.result_set_index != 0 {
            return not_editable(crate::row_editing::not_editable::later_result_set());
        }
        if request.columns.len() != request.row.len() {
            return Err(crate::row_editing::errors::value_count_mismatch());
        }

        let mut lane = self.checkout(profile).await?;
        let client = lane.client();

        // 1. Provenance. Failures here are expected for dynamic SQL, temp
        //    tables, procedures, etc. — they mean "not editable", not "error".
        let provenance = match resolve_provenance(client, &request).await {
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
        } = match self.load_table(client, &provenance.target).await? {
            Ok(loaded) => loaded,
            Err(reason) => return not_editable(reason),
        };
        if key_columns.is_empty() {
            return not_editable(crate::row_editing::not_editable::no_primary_key(
                &provenance.target.quoted(),
            ));
        }
        assign_result_ordinals(&mut columns, &provenance);

        // 3. The full key must be present in the projection, with usable values.
        let key = match key_from_projection(&columns, &key_columns, &request.row) {
            Ok(key) => key,
            Err(reason) => return not_editable(reason),
        };

        // 4. Fetch the authoritative current row by key.
        let (fetched_names, fetched_rows) = self
            .fetch_row_on_client(client, &provenance.target, &key)
            .await?;
        let row = match fetched_rows.len() {
            0 => {
                return not_editable(crate::row_editing::not_editable::row_not_found());
            }
            1 => fetched_rows.into_iter().next().expect("length checked"),
            n => {
                return not_editable(crate::row_editing::not_editable::key_matched_many_rows(n));
            }
        };

        // Align the fetched row (SELECT * order) with catalog column order by name.
        let original_row = align_row_to_catalog(&columns, &fetched_names, &row)?;
        let rowversion_column = rowversion_column_of(&columns);

        // Resolve FK display labels for the current row (single-column FKs).
        let fk_display = self
            .preload_fk_display(client, &foreign_keys, &columns, &original_row)
            .await;

        // All reads completed cleanly — the session is reusable.
        lane.mark_healthy();
        Ok(RowEditOpenOutcome::Editable(Box::new(RowEditSession {
            target: provenance.target,
            key_columns,
            rowversion_column,
            columns,
            original_row,
            foreign_keys,
            fk_display,
            check_constraints,
            incoming_foreign_keys,
        })))
    }

    async fn open_insert_session_tds(
        &self,
        profile: &ConnectionProfile,
        request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome> {
        let not_editable = |reason: DisplayMessage| Ok(RowEditOpenOutcome::not_editable(reason));

        if let Some(reason) = policy_block_message(profile) {
            return not_editable(reason);
        }
        if request.result_set_index != 0 {
            return not_editable(crate::row_editing::not_editable::later_result_set_template());
        }
        if request.columns.len() != request.row.len() {
            return Err(crate::row_editing::errors::value_count_mismatch());
        }

        let mut lane = self.checkout(profile).await?;
        let client = lane.client();

        let provenance = match resolve_provenance(client, &request).await {
            Ok(provenance) => provenance,
            Err(reason) => return not_editable(reason),
        };

        let LoadedTable {
            mut columns,
            key_columns,
            foreign_keys,
            check_constraints,
            incoming_foreign_keys,
        } = match self.load_table(client, &provenance.target).await? {
            Ok(loaded) => loaded,
            Err(reason) => return not_editable(reason),
        };
        assign_result_ordinals(&mut columns, &provenance);

        // Align the template row into catalog order (NULL where not projected);
        // callers use this as the "duplicate" seed and ignore it for a blank
        // insert.
        let original_row: Vec<CellValue> = columns
            .iter()
            .map(|column| {
                column
                    .result_ordinal
                    .and_then(|ordinal| request.row.get(ordinal).cloned())
                    .unwrap_or(CellValue::Null)
            })
            .collect();

        let rowversion_column = rowversion_column_of(&columns);
        let fk_display = vec![None; foreign_keys.len()];

        // Metadata reads completed cleanly — the session is reusable.
        lane.mark_healthy();
        Ok(RowEditOpenOutcome::Editable(Box::new(RowEditSession {
            target: provenance.target,
            key_columns,
            rowversion_column,
            columns,
            original_row,
            foreign_keys,
            fk_display,
            check_constraints,
            incoming_foreign_keys,
        })))
    }

    async fn fetch_row_tds(
        &self,
        profile: &ConnectionProfile,
        target: &TableTarget,
        key: &[(String, CellValue)],
    ) -> StudioResult<Option<(Vec<String>, Vec<CellValue>)>> {
        let mut lane = self.checkout(profile).await?;
        let client = lane.client();
        let (names, mut rows) = self.fetch_row_on_client(client, target, key).await?;
        // Read-only operation finished cleanly — the session is reusable.
        lane.mark_healthy();
        match rows.len() {
            0 => Ok(None),
            1 => Ok(Some((names, rows.remove(0)))),
            n => Err(crate::row_editing::errors::key_matched_rows(n)),
        }
    }

    async fn update_row_tds(
        &self,
        profile: &ConnectionProfile,
        request: UpdateRowRequest,
        cancel_slot: Option<&crate::CancelSlot>,
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

        let mut lane = self.checkout(profile).await?;
        if let Some(slot) = cancel_slot {
            Self::register_tds_lane_cancel(profile, &mut lane, slot).await;
        }
        let client = lane.client();

        // One session for the whole guarded write. XACT_ABORT guarantees any
        // execution error rolls the transaction back server-side, and an
        // abandoned session (client drop on early return) also rolls back.
        execute_batch_on_client(client, "SET XACT_ABORT ON; BEGIN TRANSACTION;").await?;

        let (update_sql, params) = build_update_sql(&request);
        let affected = match execute_params_on_client(client, &update_sql, &params).await {
            Ok(affected) => affected,
            Err(err) => {
                // Best-effort rollback; XACT_ABORT has usually already done it.
                let _ = execute_batch_on_client(client, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;")
                    .await;
                return Err(err);
            }
        };

        match affected {
            1 => {
                let (names, mut rows) = self
                    .fetch_row_on_client(client, &request.target, &request.key)
                    .await?;
                execute_batch_on_client(client, "COMMIT TRANSACTION;").await?;
                // COMMIT succeeded — the session is transactionally clean.
                lane.mark_healthy();
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
                execute_batch_on_client(client, "ROLLBACK TRANSACTION;").await?;
                let (names, mut rows) = self
                    .fetch_row_on_client(client, &request.target, &request.key)
                    .await?;
                // ROLLBACK + a successful read — the session is clean.
                lane.mark_healthy();
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
                execute_batch_on_client(client, "ROLLBACK TRANSACTION;").await?;
                Err(crate::row_editing::errors::update_affected_rows(n))
            }
        }
    }

    async fn fk_search_tds(
        &self,
        profile: &ConnectionProfile,
        request: FkSearchRequest,
    ) -> StudioResult<FkSearchResult> {
        if request.key_columns.is_empty() {
            return Err(crate::row_editing::errors::no_key_columns_for_fk_search());
        }
        let limit = request.limit.max(1);
        let has_display = request.display_column.is_some();
        let key_count = request.key_columns.len();
        let (sql, params) = build_fk_search_sql(&request);

        let mut lane = self.checkout(profile).await?;
        let client = lane.client();
        let sets = query_rows_on_client(client, &sql, &params).await?;
        // Read-only operation finished cleanly — the session is reusable.
        lane.mark_healthy();
        let set = sets.into_iter().next().unwrap_or_default();
        Ok(parse_fk_candidates(&set, key_count, has_display, limit))
    }

    async fn fk_labels_tds(
        &self,
        profile: &ConnectionProfile,
        request: FkLabelRequest,
    ) -> StudioResult<FkLabelResult> {
        if request.keys.is_empty() {
            return Ok(FkLabelResult::default());
        }
        let (sql, params) = build_fk_labels_sql(&request)?;
        if sql.is_empty() {
            return Ok(FkLabelResult::default());
        }

        let mut lane = self.checkout(profile).await?;
        let client = lane.client();
        let sets = query_rows_on_client(client, &sql, &params).await?;
        lane.mark_healthy();
        let set = sets.into_iter().next().unwrap_or_default();
        Ok(parse_fk_labels(&set))
    }

    async fn related_rows_tds(
        &self,
        profile: &ConnectionProfile,
        request: RelatedRowsRequest,
    ) -> StudioResult<RelatedRowsResult> {
        let limit = request.limit.max(1);
        let open_sql = build_related_open_sql(&request);
        let (sql, params) = build_related_rows_sql(&request);

        let mut lane = self.checkout(profile).await?;
        let client = lane.client();
        let sets = query_rows_on_client(client, &sql, &params).await?;
        // Read-only operation finished cleanly — the session is reusable.
        lane.mark_healthy();
        let set = sets.into_iter().next().unwrap_or_default();
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

    async fn insert_row_tds(
        &self,
        profile: &ConnectionProfile,
        request: InsertRowRequest,
        cancel_slot: Option<&crate::CancelSlot>,
    ) -> StudioResult<InsertRowOutcome> {
        if let Some(error) = crate::row_editing::errors::policy_blocked(profile) {
            return Err(error);
        }
        let mut lane = self.checkout(profile).await?;
        if let Some(slot) = cancel_slot {
            Self::register_tds_lane_cancel(profile, &mut lane, slot).await;
        }
        let client = lane.client();
        execute_batch_on_client(client, "SET XACT_ABORT ON; BEGIN TRANSACTION;").await?;

        let (sql, params) = build_insert_sql(&request);
        let sets = match query_rows_on_client(client, &sql, &params).await {
            Ok(sets) => sets,
            Err(err) => {
                let _ = execute_batch_on_client(client, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;")
                    .await;
                return Err(err);
            }
        };
        let set = sets.into_iter().next().unwrap_or_default();
        if set.rows.len() != 1 {
            let _ =
                execute_batch_on_client(client, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;").await;
            return Err(crate::row_editing::errors::insert_returned_rows(
                set.rows.len(),
            ));
        }
        execute_batch_on_client(client, "COMMIT TRANSACTION;").await?;
        // COMMIT succeeded — the session is transactionally clean.
        lane.mark_healthy();
        let columns = set.columns.iter().map(|c| c.name.clone()).collect();
        let row = set.rows.into_iter().next().expect("length checked");
        Ok(InsertRowOutcome::Inserted { columns, row })
    }

    async fn delete_row_tds(
        &self,
        profile: &ConnectionProfile,
        request: DeleteRowRequest,
        cancel_slot: Option<&crate::CancelSlot>,
    ) -> StudioResult<DeleteRowOutcome> {
        if let Some(error) = crate::row_editing::errors::policy_blocked(profile) {
            return Err(error);
        }
        if request.key.is_empty() {
            return Err(crate::row_editing::errors::no_key_columns_for_delete());
        }

        let mut lane = self.checkout(profile).await?;
        if let Some(slot) = cancel_slot {
            Self::register_tds_lane_cancel(profile, &mut lane, slot).await;
        }
        let client = lane.client();
        execute_batch_on_client(client, "SET XACT_ABORT ON; BEGIN TRANSACTION;").await?;

        let (delete_sql, params) = build_delete_sql(&request);
        let affected = match execute_params_on_client(client, &delete_sql, &params).await {
            Ok(affected) => affected,
            Err(err) => {
                let _ = execute_batch_on_client(client, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;")
                    .await;
                return Err(err);
            }
        };
        match affected {
            1 => {
                execute_batch_on_client(client, "COMMIT TRANSACTION;").await?;
                // COMMIT succeeded — the session is transactionally clean.
                lane.mark_healthy();
                Ok(DeleteRowOutcome::Deleted)
            }
            0 => {
                execute_batch_on_client(client, "ROLLBACK TRANSACTION;").await?;
                let (names, mut rows) = self
                    .fetch_row_on_client(client, &request.target, &request.key)
                    .await?;
                // ROLLBACK + a successful read — the session is clean.
                lane.mark_healthy();
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
                execute_batch_on_client(client, "ROLLBACK TRANSACTION;").await?;
                Err(crate::row_editing::errors::delete_affected_rows(n))
            }
        }
    }
}

/// Engine dispatch. TDS profiles run the original T-SQL path above,
/// byte-for-byte; PostgreSQL-wire, MySQL-wire, SQLite, libSQL, and DuckDB
/// profiles run the engine-generic flows in `row_editing_engines` over
/// their native drivers with the same guarantees. Every other connection
/// kind gets the clean capability error.
#[async_trait]
impl RowEditingService for DefaultRowEditingService {
    async fn open_session(
        &self,
        profile: &ConnectionProfile,
        request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome> {
        if profile.database_type.connection_kind() == sqlly_core::ConnectionKind::Tds {
            return self.open_session_tds(profile, request).await;
        }
        let mut lane = self.checkout_engine(profile).await?;
        let result = {
            let mut engine = lane.engine(profile.database_type);
            crate::row_editing_engines::open_session_on(
                engine.as_mut(),
                profile,
                request,
                crate::row_editing_engines::OpenMode::Edit,
            )
            .await
        };
        if result.is_ok() {
            lane.mark_healthy();
        }
        result
    }

    async fn open_insert_session(
        &self,
        profile: &ConnectionProfile,
        request: RowEditOpenRequest,
    ) -> StudioResult<RowEditOpenOutcome> {
        if profile.database_type.connection_kind() == sqlly_core::ConnectionKind::Tds {
            return self.open_insert_session_tds(profile, request).await;
        }
        let mut lane = self.checkout_engine(profile).await?;
        let result = {
            let mut engine = lane.engine(profile.database_type);
            crate::row_editing_engines::open_session_on(
                engine.as_mut(),
                profile,
                request,
                crate::row_editing_engines::OpenMode::Insert,
            )
            .await
        };
        if result.is_ok() {
            lane.mark_healthy();
        }
        result
    }

    async fn fetch_row(
        &self,
        profile: &ConnectionProfile,
        target: &TableTarget,
        key: &[(String, CellValue)],
    ) -> StudioResult<Option<(Vec<String>, Vec<CellValue>)>> {
        if profile.database_type.connection_kind() == sqlly_core::ConnectionKind::Tds {
            return self.fetch_row_tds(profile, target, key).await;
        }
        let mut lane = self.checkout_engine(profile).await?;
        let result = {
            let mut engine = lane.engine(profile.database_type);
            crate::row_editing_engines::fetch_row_on(engine.as_mut(), target, key).await
        };
        if result.is_ok() {
            lane.mark_healthy();
        }
        result
    }

    async fn update_row(
        &self,
        profile: &ConnectionProfile,
        request: UpdateRowRequest,
    ) -> StudioResult<UpdateRowOutcome> {
        self.update_row_with_cancel(profile, request, None).await
    }

    async fn fk_search(
        &self,
        profile: &ConnectionProfile,
        request: FkSearchRequest,
    ) -> StudioResult<FkSearchResult> {
        if profile.database_type.connection_kind() == sqlly_core::ConnectionKind::Tds {
            return self.fk_search_tds(profile, request).await;
        }
        let mut lane = self.checkout_engine(profile).await?;
        let result = {
            let mut engine = lane.engine(profile.database_type);
            crate::row_editing_engines::fk_search_on(engine.as_mut(), request).await
        };
        if result.is_ok() {
            lane.mark_healthy();
        }
        result
    }

    async fn fk_labels(
        &self,
        profile: &ConnectionProfile,
        request: FkLabelRequest,
    ) -> StudioResult<FkLabelResult> {
        if profile.database_type.connection_kind() == sqlly_core::ConnectionKind::Tds {
            return self.fk_labels_tds(profile, request).await;
        }
        let mut lane = self.checkout_engine(profile).await?;
        let result = {
            let mut engine = lane.engine(profile.database_type);
            crate::row_editing_engines::fk_labels_on(engine.as_mut(), request).await
        };
        if result.is_ok() {
            lane.mark_healthy();
        }
        result
    }

    async fn related_rows(
        &self,
        profile: &ConnectionProfile,
        request: RelatedRowsRequest,
    ) -> StudioResult<RelatedRowsResult> {
        if profile.database_type.connection_kind() == sqlly_core::ConnectionKind::Tds {
            return self.related_rows_tds(profile, request).await;
        }
        let mut lane = self.checkout_engine(profile).await?;
        let result = {
            let mut engine = lane.engine(profile.database_type);
            crate::row_editing_engines::related_rows_on(engine.as_mut(), request).await
        };
        if result.is_ok() {
            lane.mark_healthy();
        }
        result
    }

    async fn insert_row(
        &self,
        profile: &ConnectionProfile,
        request: InsertRowRequest,
    ) -> StudioResult<InsertRowOutcome> {
        self.insert_row_with_cancel(profile, request, None).await
    }

    async fn delete_row(
        &self,
        profile: &ConnectionProfile,
        request: DeleteRowRequest,
    ) -> StudioResult<DeleteRowOutcome> {
        self.delete_row_with_cancel(profile, request, None).await
    }

    async fn apply_batch(
        &self,
        profile: &ConnectionProfile,
        request: BatchRequest,
    ) -> StudioResult<BatchOutcome> {
        self.apply_batch_with_cancel(profile, request, None).await
    }
}
