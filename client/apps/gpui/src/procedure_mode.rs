//! Procedure mode: per-tab state for the deploy + invoke loop, the pure
//! run-plan builder, and the async step orchestrator.
//!
//! The GPUI surface (toolbar toggle + parameter strip) lives in `app.rs`;
//! everything here is render-free so the validation, verb-flip, and
//! batch-composition rules are unit tested without a window.
//!
//! Two execution shapes, both forced through one transaction scope decision:
//!
//! - Auto-rollback OFF: probe, deploy, and invoke run as three sequential
//!   `execute` calls. Deploy commits normally.
//! - Auto-rollback ON: probe, then ONE composite batch
//!   (`BEGIN TRAN` + deploy via `sp_executesql` + invoke + `ROLLBACK`). The
//!   app's executor opens a fresh connection per `execute`, so a transaction
//!   cannot span requests; the deploy rides inside `sp_executesql` because
//!   `CREATE`/`ALTER PROCEDURE` must be the only statement in its batch.
//!
//! Every step runs with a cloned profile whose `auto_wrap_rollback` is forced
//! off: the feature manages its own transaction scope, and the driver-level
//! wrap (`apply_connection_policy`) would nest and break sequencing. The
//! profile's `block_all_but_select` is left untouched and still wins.

// Browser build: swap tokio's channels/spawn for the reactor-free shim so
// the `tokio::` paths below compile unchanged.
#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;
use std::sync::Arc;

use gpui::Entity;
use gpui_component::input::InputState;
use sqlly_core::{CellValue, ConnectionProfile, ResultColumn};
use sqlly_intel::query::pg_procedure::{self, PgProcHeader};
use sqlly_intel::query::procedure::{
    self, ParamArg, ProcHeader, ProcVerb, TypeFamily, build_invoke_batch, build_rollback_batch,
    object_probe_sql, rewrite_deploy,
};
use sqlly_services::{QueryEvent, QueryExecutor};
use tokio::sync::mpsc;

/// Which engine family's procedure grammar and deploy/invoke pipeline the
/// tab's script goes through. Derived from the tab's connection at refresh
/// time; T-SQL is the default (and the pre-port behavior).
pub use sqlly_services::ProcToolDialect as ProcDialect;

/// Per-tab procedure-mode state. Rows are materialized lazily at render time
/// (`synced_revision` tracks the parse they were built from) because
/// `InputState` entities need a `&mut Window`, which event handlers do not
/// have.
#[derive(Default)]
pub struct ProcedureModeState {
    pub enabled: bool,
    /// Head scan matched the current buffer (drives the toggle glow).
    pub detected: bool,
    /// Parse of the current buffer; `Some(Err)` when the header no longer
    /// parses, `None` when the buffer is not a procedure script at all.
    /// For PostgreSQL the header is a converted view of `pg_parsed` so every
    /// strip/row surface reads one shape.
    pub parsed: Option<Result<ProcHeader, String>>,
    /// The PostgreSQL parse `parsed` was converted from (None on T-SQL).
    pub pg_parsed: Option<PgProcHeader>,
    /// Grammar/pipeline the current parse used.
    pub dialect: ProcDialect,
    /// Bumped every time `parsed` is recomputed.
    pub parse_revision: u64,
    /// The revision `rows` was reconciled against.
    pub synced_revision: u64,
    pub rollback: bool,
    pub rows: Vec<ProcParamRow>,
    /// Return code from the last completed run.
    pub return_code: Option<String>,
    /// Whether the strip currently reflects a run in flight (inputs disabled).
    pub running: bool,
}

/// UI-shape view of a PostgreSQL routine header: same `ProcParam` list (the
/// PG parser reuses it), verb normalized to `CreateOrAlter` because the PG
/// deploy always rewrites to `CREATE OR REPLACE` (no probe/flip dance).
fn pg_header_as_proc_header(pg: &PgProcHeader) -> ProcHeader {
    ProcHeader {
        schema: pg.schema.clone(),
        name: pg.name.clone(),
        verb: ProcVerb::CreateOrAlter,
        verb_start: pg.verb_start,
        verb_end: pg.verb_end,
        params: pg.params.iter().map(|p| p.param.clone()).collect(),
    }
}

impl ProcedureModeState {
    /// Re-run detection (and the full parse, when the mode is on) against the
    /// current buffer. Cheap: the head scan never lexes past a prefix cap.
    pub fn refresh_from_script_for(&mut self, script: &str, dialect: ProcDialect) {
        self.dialect = dialect;
        match dialect {
            ProcDialect::TSql => {
                self.detected = procedure::detect_proc_script(script).is_some();
                self.pg_parsed = None;
                if self.enabled {
                    if self.detected {
                        self.parsed =
                            Some(procedure::parse_proc_header(script).map_err(|e| e.message));
                    } else {
                        self.parsed = None;
                    }
                    self.parse_revision += 1;
                }
            }
            ProcDialect::Postgres => {
                self.detected = pg_procedure::detect_routine_script(script).is_some();
                if self.enabled {
                    if self.detected {
                        match pg_procedure::parse_routine_header(script) {
                            Ok(header) => {
                                self.parsed = Some(Ok(pg_header_as_proc_header(&header)));
                                self.pg_parsed = Some(header);
                            }
                            Err(error) => {
                                self.parsed = Some(Err(error.message));
                                self.pg_parsed = None;
                            }
                        }
                    } else {
                        self.parsed = None;
                        self.pg_parsed = None;
                    }
                    self.parse_revision += 1;
                }
            }
        }
    }

    /// T-SQL refresh — existing call shape, kept for tests and T-SQL-only
    /// paths.
    #[cfg(test)]
    pub fn refresh_from_script(&mut self, script: &str) {
        self.refresh_from_script_for(script, ProcDialect::TSql);
    }

    /// Copy every row's current input text into `last_value`. Subscriptions
    /// already do this on every keystroke; this is the belt-and-braces read at
    /// run dispatch so a value typed into a still-focused input cannot race
    /// the Run keypress.
    pub fn sync_values_from_inputs(&mut self, cx: &gpui::App) {
        for row in &mut self.rows {
            if let Some(input) = &row.input {
                row.last_value = input.read(cx).value().to_string();
            }
        }
    }
}

/// One parameter row in the strip. `last_value` is the canonical business
/// value; the `InputState` mirrors it (and is `None` for bit/unsupported
/// params, which render a checkbox or a note instead of a text field).
pub struct ProcParamRow {
    pub name: String,
    pub data_type: String,
    pub base_type: String,
    pub family: TypeFamily,
    pub is_output: bool,
    pub editable: bool,
    pub has_default: bool,
    pub null_checked: bool,
    pub bit_checked: bool,
    pub input: Option<Entity<InputState>>,
    pub last_value: String,
    /// Value captured by the sentinel SELECT after a run (OUTPUT params only).
    pub output_value: Option<String>,
    pub validation_error: Option<String>,
}

/// Live type check for the strip: validates a non-empty typed value against
/// the parameter's declared type so mistakes surface before Run. Blank stays
/// quiet (blank means "use the default" or "required", which Run reports).
#[cfg(test)]
pub fn live_validate(row: &ProcParamRow, value: &str) -> Option<String> {
    live_validate_for(row, value, ProcDialect::TSql)
}

/// Dialect-aware live type check — PostgreSQL literals differ from T-SQL
/// (no `N''` prefix, `true`/`false` booleans, different integer ranges).
pub fn live_validate_for(row: &ProcParamRow, value: &str, dialect: ProcDialect) -> Option<String> {
    if value.trim().is_empty() || !row.editable {
        return None;
    }
    let param = sqlly_intel::query::procedure::ProcParam {
        name: row.name.clone(),
        data_type: row.data_type.clone(),
        base_type: row.base_type.clone(),
        family: row.family,
        is_output: row.is_output,
        is_readonly: !row.editable,
        default_expr: None,
    };
    match dialect {
        ProcDialect::TSql => sqlly_intel::query::procedure::sql_literal(value, &param).err(),
        ProcDialect::Postgres => pg_procedure::sql_literal(value, &param).err(),
    }
}

/// Flip decision for the deploy verb given server-side existence.
pub fn flip_verb(verb: ProcVerb, exists: bool) -> Option<ProcVerb> {
    match (verb, exists) {
        (ProcVerb::Create, true) => Some(ProcVerb::Alter),
        (ProcVerb::Alter, false) => Some(ProcVerb::Create),
        _ => None,
    }
}

/// Collect the strip's values into EXEC arguments, validating presence:
/// a blank, non-NULL input on a parameter without a declared default blocks
/// the run. Blank on a parameter WITH a default omits it from the EXEC (the
/// server applies the default). Blank OUTPUT params still get captured.
pub fn collect_args(rows: &[ProcParamRow]) -> Result<Vec<ParamArg>, (usize, String)> {
    let mut args = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if !row.editable {
            continue;
        }
        if row.null_checked {
            args.push(ParamArg {
                name: row.name.clone(),
                value: None,
            });
            continue;
        }
        match row.family {
            TypeFamily::Bit => args.push(ParamArg {
                name: row.name.clone(),
                value: Some(if row.bit_checked { "1" } else { "0" }.to_string()),
            }),
            _ if row.last_value.trim().is_empty() => {
                if row.is_output {
                    // Captured with NULL in via the no-arg path in the batch builder.
                    continue;
                }
                if !row.has_default {
                    return Err((
                        index,
                        format!("@{} needs a value (or check NULL)", row.name),
                    ));
                }
            }
            _ => args.push(ParamArg {
                name: row.name.clone(),
                value: Some(row.last_value.clone()),
            }),
        }
    }
    Ok(args)
}

/// The sentinel result set has the return code first and one column per
/// OUTPUT parameter, in declaration order, aliased to the parameter names.
/// Matching the full shape (not just the first column) is the collision guard
/// against a procedure that returns its own lookalike set.
#[cfg(test)]
pub fn is_sentinel(columns: &[ResultColumn], output_names: &[String]) -> bool {
    is_sentinel_shaped(columns, output_names, true)
}

/// Shape match for the peeled output set. With `has_rc` the first column is
/// the T-SQL `__sqlly_rc` return code; without it (PostgreSQL `CALL`) the
/// columns are exactly the INOUT parameter names in declaration order.
pub fn is_sentinel_shaped(columns: &[ResultColumn], output_names: &[String], has_rc: bool) -> bool {
    if has_rc {
        columns.len() == output_names.len() + 1
            && columns[0]
                .name
                .eq_ignore_ascii_case(procedure::SENTINEL_COLUMN)
            && columns[1..]
                .iter()
                .zip(output_names)
                .all(|(column, name)| column.name.eq_ignore_ascii_case(name))
    } else {
        !output_names.is_empty()
            && columns.len() == output_names.len()
            && columns
                .iter()
                .zip(output_names)
                .all(|(column, name)| column.name.eq_ignore_ascii_case(name))
    }
}

/// Display form for a sentinel cell (return code / output values).
pub fn cell_display(cell: &CellValue) -> String {
    match cell {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(value) => if *value { "1" } else { "0" }.to_string(),
        CellValue::I64(value) => value.to_string(),
        CellValue::F64(value) => value.to_string(),
        CellValue::String(value) => value.clone(),
        CellValue::Bytes(bytes) => {
            let mut out = String::with_capacity(bytes.len() * 2 + 2);
            out.push_str("0x");
            for byte in bytes {
                out.push_str(&format!("{byte:02X}"));
            }
            out
        }
    }
}

/// Pre-fill text for a declared default, when the default is a simple literal
/// (`5`, `'abc'`, `N'abc'`). Expressions (function calls, arithmetic) return
/// `None` and the input stays blank, meaning "use the server default".
pub fn default_prefill(expr: &str) -> Option<String> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.parse::<f64>().is_ok() {
        return Some(trimmed.to_string());
    }
    for prefix in ["N'", "n'", "'"] {
        if let Some(rest) = trimmed.strip_prefix(prefix)
            && let Some(core) = rest.strip_suffix('\'')
        {
            return Some(core.replace("''", "'"));
        }
    }
    None
}

pub fn default_is_null(expr: &str) -> bool {
    expr.trim().eq_ignore_ascii_case("null")
}

/// Everything the orchestrator needs, computed at dispatch time on the UI
/// thread (header parse + validated argument literals).
#[derive(Debug, Clone)]
pub struct ProcRunPlan {
    /// The editor buffer verbatim: the deploy source of truth.
    pub script: String,
    pub header: ProcHeader,
    pub invoke_sql: String,
    pub rollback: bool,
    pub output_names: Vec<String>,
    pub qualified_name: String,
    pub dialect: ProcDialect,
    /// PostgreSQL: the deploy SQL, rewritten to `CREATE OR REPLACE` at plan
    /// time (no probe/verb-flip step). `None` on T-SQL, which probes first.
    pub pg_deploy_sql: Option<String>,
}

impl ProcRunPlan {
    /// Whether the peeled output result set carries a leading return-code
    /// column (`__sqlly_rc`). T-SQL only; PostgreSQL `CALL` returns INOUT
    /// values as a plain row named after the parameters.
    fn sentinel_has_rc(&self) -> bool {
        self.dialect == ProcDialect::TSql
    }
}

/// Updates the orchestrator sends back to the UI pump.
pub enum ProcRunUpdate {
    /// Forwarded to the results view (deploy/invoke messages, invoke result
    /// sets, the terminal Completed/Error/Cancelled of the visible stream).
    Event(QueryEvent),
    /// The current step's cancellation id, stored on the tab.
    TaskId(String),
    /// Peeled sentinel values: return code plus one (name, display) per
    /// OUTPUT parameter.
    Outputs {
        return_code: Option<String>,
        values: Vec<(String, String)>,
    },
    /// Terminal update for the whole sequence. Keyed: built on the runtime,
    /// worded on the UI thread.
    Finished {
        ok: bool,
        status: sqlly_core::DisplayMessage,
    },
}

enum StepEnd {
    Completed { elapsed_ms: u128 },
    Failed(String),
    Cancelled,
}

/// Run the full deploy + invoke sequence. Sends every UI-visible update over
/// `tx`; returns when the sequence finishes, fails, or is cancelled.
pub async fn run_proc_sequence(
    queries: Arc<dyn QueryExecutor>,
    profile: ConnectionProfile,
    plan: ProcRunPlan,
    tx: mpsc::Sender<ProcRunUpdate>,
) {
    // The feature implements its own transaction scope; the driver-level
    // rollback wrap would nest and break sequencing. SELECT-only enforcement
    // stays armed on the clone.
    let mut step_profile = profile;
    step_profile.auto_wrap_rollback = false;

    // 1. Deploy SQL. PostgreSQL rewrites to CREATE OR REPLACE at plan time —
    // no probe or verb-flip needed. T-SQL probes for existence first and
    // flips CREATE<->ALTER to match the server.
    let deploy_sql = if let Some(pg_deploy) = &plan.pg_deploy_sql {
        pg_deploy.clone()
    } else {
        let probe_sql = object_probe_sql(&plan.header);
        let mut probe_rows: Vec<Vec<CellValue>> = Vec::new();
        match run_collecting_step(
            &queries,
            &step_profile,
            &probe_sql,
            &tx,
            false,
            &mut probe_rows,
        )
        .await
        {
            StepEnd::Completed { .. } => {}
            StepEnd::Failed(message) => {
                send_error(&tx, &message).await;
                finish(
                    &tx,
                    false,
                    crate::localization::deferred(
                        "shell.procedure.deploy_failed",
                        &[("error", message)],
                    ),
                )
                .await;
                return;
            }
            StepEnd::Cancelled => {
                send_cancelled(&tx).await;
                finish(
                    &tx,
                    false,
                    crate::localization::deferred_text("shell.procedure.cancelled"),
                )
                .await;
                return;
            }
        }
        let exists = probe_rows
            .first()
            .and_then(|row| row.first())
            .is_some_and(|cell| matches!(cell, CellValue::I64(value) if *value == 1));
        match flip_verb(plan.header.verb, exists) {
            Some(target) => rewrite_deploy(&plan.script, &plan.header, target),
            None => plan.script.clone(),
        }
    };

    if plan.rollback {
        // 2R. One composite batch on one connection: nothing persists.
        let composite = match plan.dialect {
            ProcDialect::TSql => build_rollback_batch(&deploy_sql, &plan.invoke_sql),
            ProcDialect::Postgres => {
                pg_procedure::build_rollback_batch(&deploy_sql, &plan.invoke_sql)
            }
        };
        match run_visible_step(&queries, &step_profile, &composite, &plan, &tx).await {
            StepEnd::Completed { elapsed_ms } => {
                finish(
                    &tx,
                    true,
                    crate::localization::deferred(
                        "shell.procedure.finished_rolled_back",
                        &[
                            ("name", plan.qualified_name.clone()),
                            ("elapsed_ms", elapsed_ms.to_string()),
                        ],
                    ),
                )
                .await;
            }
            StepEnd::Failed(message) => {
                finish(
                    &tx,
                    false,
                    crate::localization::deferred(
                        "shell.procedure.run_failed",
                        &[("error", message)],
                    ),
                )
                .await;
            }
            StepEnd::Cancelled => {
                finish(
                    &tx,
                    false,
                    crate::localization::deferred_text("shell.procedure.cancelled"),
                )
                .await;
            }
        }
        return;
    }

    // 2. Deploy (commits). Server messages forward to the messages tab.
    match run_collecting_step(
        &queries,
        &step_profile,
        &deploy_sql,
        &tx,
        true,
        &mut Vec::new(),
    )
    .await
    {
        StepEnd::Completed { .. } => {}
        StepEnd::Failed(message) => {
            send_error(&tx, &message).await;
            finish(
                &tx,
                false,
                crate::localization::deferred(
                    "shell.procedure.deploy_failed",
                    &[("error", message)],
                ),
            )
            .await;
            return;
        }
        StepEnd::Cancelled => {
            send_cancelled(&tx).await;
            finish(
                &tx,
                false,
                crate::localization::deferred_text("shell.procedure.cancelled"),
            )
            .await;
            return;
        }
    }

    // 3. Invoke; result sets stream to the grid, the sentinel peels into the strip.
    match run_visible_step(&queries, &step_profile, &plan.invoke_sql, &plan, &tx).await {
        StepEnd::Completed { elapsed_ms } => {
            finish(
                &tx,
                true,
                crate::localization::deferred(
                    "shell.procedure.finished",
                    &[
                        ("name", plan.qualified_name.clone()),
                        ("elapsed_ms", elapsed_ms.to_string()),
                    ],
                ),
            )
            .await;
        }
        StepEnd::Failed(message) => {
            finish(
                &tx,
                false,
                crate::localization::deferred(
                    "shell.procedure.execution_failed",
                    &[("error", message)],
                ),
            )
            .await;
        }
        StepEnd::Cancelled => {
            finish(
                &tx,
                false,
                crate::localization::deferred_text("shell.procedure.cancelled"),
            )
            .await;
        }
    }
}

/// Execute one step, swallowing its events. Result-set rows are collected
/// into `rows` (the probe needs them); server messages are optionally
/// forwarded to the results view.
async fn run_collecting_step(
    queries: &Arc<dyn QueryExecutor>,
    profile: &ConnectionProfile,
    sql: &str,
    tx: &mpsc::Sender<ProcRunUpdate>,
    forward_messages: bool,
    rows: &mut Vec<Vec<CellValue>>,
) -> StepEnd {
    let mut handle = match queries.execute(profile, sql, 64).await {
        Ok(handle) => handle,
        Err(error) => return StepEnd::Failed(error.to_string()),
    };
    let _ = tx.send(ProcRunUpdate::TaskId(handle.task_id)).await;
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::Rows { rows: batch, .. } => rows.extend(batch),
            QueryEvent::Message { text } if forward_messages => {
                if tx
                    .send(ProcRunUpdate::Event(QueryEvent::Message { text }))
                    .await
                    .is_err()
                {
                    return StepEnd::Cancelled;
                }
            }
            QueryEvent::Error { message } => return StepEnd::Failed(message),
            QueryEvent::Completed { elapsed_ms } => return StepEnd::Completed { elapsed_ms },
            QueryEvent::Cancelled => return StepEnd::Cancelled,
            _ => {}
        }
    }
    // Channel closed without a terminal event: the producer was aborted.
    StepEnd::Cancelled
}

/// Execute the invoke (or rollback-composite) step, forwarding everything to
/// the results view except the sentinel result set, which is peeled and
/// reported as [`ProcRunUpdate::Outputs`] after completion.
async fn run_visible_step(
    queries: &Arc<dyn QueryExecutor>,
    profile: &ConnectionProfile,
    sql: &str,
    plan: &ProcRunPlan,
    tx: &mpsc::Sender<ProcRunUpdate>,
) -> StepEnd {
    let mut handle = match queries.execute(profile, sql, 256).await {
        Ok(handle) => handle,
        Err(error) => return StepEnd::Failed(error.to_string()),
    };
    let _ = tx.send(ProcRunUpdate::TaskId(handle.task_id)).await;
    let mut sentinel_index: Option<usize> = None;
    let mut sentinel_rows: Vec<Vec<CellValue>> = Vec::new();
    let end = loop {
        let Some(event) = handle.events.recv().await else {
            break StepEnd::Cancelled;
        };
        match event {
            QueryEvent::ResultSet { index, ref columns }
                if is_sentinel_shaped(columns, &plan.output_names, plan.sentinel_has_rc()) =>
            {
                sentinel_index = Some(index);
            }
            QueryEvent::Rows { index, rows } if sentinel_index == Some(index) => {
                sentinel_rows.extend(rows);
            }
            QueryEvent::RowCount { index, .. } | QueryEvent::RowsProgress { index, .. }
                if sentinel_index == Some(index) => {}
            QueryEvent::Error { message } => {
                let _ = tx
                    .send(ProcRunUpdate::Event(QueryEvent::Error {
                        message: message.clone(),
                    }))
                    .await;
                break StepEnd::Failed(message);
            }
            QueryEvent::Completed {
                elapsed_ms: elapsed,
            } => {
                let _ = tx
                    .send(ProcRunUpdate::Event(QueryEvent::Completed {
                        elapsed_ms: elapsed,
                    }))
                    .await;
                break StepEnd::Completed {
                    elapsed_ms: elapsed,
                };
            }
            QueryEvent::Cancelled => {
                let _ = tx.send(ProcRunUpdate::Event(QueryEvent::Cancelled)).await;
                break StepEnd::Cancelled;
            }
            other => {
                if tx.send(ProcRunUpdate::Event(other)).await.is_err() {
                    break StepEnd::Cancelled;
                }
            }
        }
    };
    if matches!(end, StepEnd::Completed { .. })
        && sentinel_index.is_some()
        && let Some(row) = sentinel_rows.first()
    {
        let (return_code, value_cells) = if plan.sentinel_has_rc() {
            (row.first().map(cell_display), &row[1..])
        } else {
            // PostgreSQL procedures have no return code; every column is an
            // INOUT value.
            (None, &row[..])
        };
        let values: Vec<(String, String)> = plan
            .output_names
            .iter()
            .cloned()
            .zip(value_cells.iter().map(cell_display))
            .collect();
        let _ = tx
            .send(ProcRunUpdate::Outputs {
                return_code,
                values,
            })
            .await;
    }
    end
}

async fn send_error(tx: &mpsc::Sender<ProcRunUpdate>, message: &str) {
    let _ = tx
        .send(ProcRunUpdate::Event(QueryEvent::Error {
            message: message.to_string(),
        }))
        .await;
}

async fn send_cancelled(tx: &mpsc::Sender<ProcRunUpdate>) {
    let _ = tx.send(ProcRunUpdate::Event(QueryEvent::Cancelled)).await;
}

async fn finish(tx: &mpsc::Sender<ProcRunUpdate>, ok: bool, status: sqlly_core::DisplayMessage) {
    let _ = tx.send(ProcRunUpdate::Finished { ok, status }).await;
}

/// Build the invoke batch from the strip's current values, mapping validation
/// failures back to their row for inline display.
pub fn build_plan(
    script: &str,
    header: &ProcHeader,
    rows: &[ProcParamRow],
    rollback: bool,
) -> Result<ProcRunPlan, (usize, String)> {
    let args = collect_args(rows)?;
    let invoke_sql = build_invoke_batch(header, &args).map_err(|error| {
        let index = rows
            .iter()
            .position(|row| row.name.eq_ignore_ascii_case(&error.param))
            .unwrap_or(0);
        (index, error.message)
    })?;
    let output_names = header
        .params
        .iter()
        .filter(|param| param.is_output)
        .map(|param| param.name.clone())
        .collect();
    Ok(ProcRunPlan {
        script: script.to_string(),
        header: header.clone(),
        invoke_sql,
        rollback,
        output_names,
        qualified_name: header.qualified_name(),
        dialect: ProcDialect::TSql,
        pg_deploy_sql: None,
    })
}

/// PostgreSQL plan builder: `CALL` / `SELECT * FROM` invocation from the PG
/// header, deploy rewritten to `CREATE OR REPLACE` up front (no probe step).
pub fn build_pg_plan(
    script: &str,
    pg_header: &PgProcHeader,
    rows: &[ProcParamRow],
    rollback: bool,
) -> Result<ProcRunPlan, (usize, String)> {
    let args = collect_args(rows)?;
    let invoke_sql = pg_procedure::build_invoke_sql(pg_header, &args).map_err(|error| {
        let index = rows
            .iter()
            .position(|row| row.name.eq_ignore_ascii_case(&error.param))
            .unwrap_or(0);
        (index, error.message)
    })?;
    Ok(ProcRunPlan {
        script: script.to_string(),
        header: pg_header_as_proc_header(pg_header),
        invoke_sql,
        rollback,
        output_names: pg_header.output_param_names(),
        qualified_name: pg_header.qualified_name(),
        dialect: ProcDialect::Postgres,
        pg_deploy_sql: Some(pg_procedure::rewrite_deploy(script, pg_header)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, family: TypeFamily) -> ProcParamRow {
        ProcParamRow {
            name: name.to_string(),
            data_type: "int".to_string(),
            base_type: "INT".to_string(),
            family,
            is_output: false,
            editable: true,
            has_default: false,
            null_checked: false,
            bit_checked: false,
            input: None,
            last_value: String::new(),
            output_value: None,
            validation_error: None,
        }
    }

    #[test]
    fn live_validate_flags_type_errors_but_stays_quiet_when_blank() {
        let r = row("n", TypeFamily::Integer);
        assert_eq!(live_validate(&r, ""), None);
        assert_eq!(live_validate(&r, "   "), None);
        assert_eq!(live_validate(&r, "42"), None);
        assert!(live_validate(&r, "abc").is_some());
        assert!(live_validate(&r, "999999999999").is_some());
        let mut ro = row("ro", TypeFamily::Integer);
        ro.editable = false;
        assert_eq!(live_validate(&ro, "abc"), None);
    }

    #[test]
    fn flip_verb_only_on_disagreement() {
        assert_eq!(flip_verb(ProcVerb::Create, true), Some(ProcVerb::Alter));
        assert_eq!(flip_verb(ProcVerb::Alter, false), Some(ProcVerb::Create));
        assert_eq!(flip_verb(ProcVerb::Create, false), None);
        assert_eq!(flip_verb(ProcVerb::Alter, true), None);
        assert_eq!(flip_verb(ProcVerb::CreateOrAlter, true), None);
        assert_eq!(flip_verb(ProcVerb::CreateOrAlter, false), None);
    }

    #[test]
    fn collect_args_requires_values_without_defaults() {
        let rows = vec![row("id", TypeFamily::Integer)];
        let (index, message) = collect_args(&rows).unwrap_err();
        assert_eq!(index, 0);
        assert!(message.contains("@id"));
    }

    #[test]
    fn collect_args_omits_blank_defaults_and_captures_blank_outputs() {
        let mut with_default = row("d", TypeFamily::Integer);
        with_default.has_default = true;
        let mut output = row("out", TypeFamily::String);
        output.is_output = true;
        let rows = vec![with_default, output];
        let args = collect_args(&rows).unwrap();
        assert!(args.is_empty());
    }

    #[test]
    fn collect_args_bit_and_null() {
        let mut bit = row("flag", TypeFamily::Bit);
        bit.bit_checked = true;
        let mut null_text = row("name", TypeFamily::String);
        null_text.null_checked = true;
        let args = collect_args(&[bit, null_text]).unwrap();
        assert_eq!(args[0].value.as_deref(), Some("1"));
        assert_eq!(args[1].value, None);
    }

    #[test]
    fn collect_args_skips_uneditable() {
        let mut tvp = row("ids", TypeFamily::Unsupported);
        tvp.editable = false;
        assert!(collect_args(&[tvp]).unwrap().is_empty());
    }

    #[test]
    fn sentinel_shape_is_exact() {
        let column = |name: &str| ResultColumn {
            name: name.to_string(),
            data_type: "int".to_string(),
            nullable: false,
        };
        let outputs = vec!["out".to_string()];
        assert!(is_sentinel(
            &[column("__sqlly_rc"), column("out")],
            &outputs
        ));
        // A procedure's own single-column result set named __sqlly_rc does
        // not match the expected two-column shape.
        assert!(!is_sentinel(&[column("__sqlly_rc")], &outputs));
        assert!(!is_sentinel(&[column("rc"), column("out")], &outputs));
        assert!(!is_sentinel(
            &[column("__sqlly_rc"), column("other")],
            &outputs
        ));
    }

    #[test]
    fn cell_display_formats() {
        assert_eq!(cell_display(&CellValue::Null), "NULL");
        assert_eq!(cell_display(&CellValue::I64(42)), "42");
        assert_eq!(cell_display(&CellValue::Bool(true)), "1");
        assert_eq!(cell_display(&CellValue::String("abc".into())), "abc");
        assert_eq!(cell_display(&CellValue::Bytes(vec![0xDE, 0xAD])), "0xDEAD");
    }

    #[test]
    fn default_prefill_simple_literals_only() {
        assert_eq!(default_prefill("5"), Some("5".to_string()));
        assert_eq!(default_prefill("-2.5"), Some("-2.5".to_string()));
        assert_eq!(default_prefill("N'it''s'"), Some("it's".to_string()));
        assert_eq!(
            default_prefill("'2024-01-01'"),
            Some("2024-01-01".to_string())
        );
        assert_eq!(default_prefill("getdate()"), None);
        assert_eq!(default_prefill("dbo.Compute(1, 2)"), None);
        assert!(default_is_null("NULL"));
        assert!(!default_is_null("'null'"));
    }

    #[test]
    fn build_plan_collects_output_names() {
        let header = procedure::parse_proc_header(
            "CREATE PROC dbo.Foo @in int, @out nvarchar(10) OUTPUT AS SELECT 1",
        )
        .unwrap();
        let mut input_row = row("in", TypeFamily::Integer);
        input_row.last_value = "7".to_string();
        let mut output_row = row("out", TypeFamily::String);
        output_row.is_output = true;
        output_row.data_type = "nvarchar(10)".to_string();
        let plan = build_plan(
            "CREATE PROC dbo.Foo @in int, @out nvarchar(10) OUTPUT AS SELECT 1",
            &header,
            &[input_row, output_row],
            false,
        )
        .unwrap();
        assert_eq!(plan.output_names, vec!["out".to_string()]);
        assert!(plan.invoke_sql.contains("[@out] = [@out] OUTPUT"));
    }

    #[test]
    fn build_plan_maps_literal_errors_to_rows() {
        let header =
            procedure::parse_proc_header("CREATE PROC dbo.Foo @in int AS SELECT 1").unwrap();
        let mut bad = row("in", TypeFamily::Integer);
        bad.last_value = "abc".to_string();
        let (index, message) = build_plan(
            "CREATE PROC dbo.Foo @in int AS SELECT 1",
            &header,
            &[bad],
            false,
        )
        .unwrap_err();
        assert_eq!(index, 0);
        assert_eq!(message, "expected an integer");
    }

    #[test]
    fn pg_refresh_parses_and_converts_header_for_the_strip() {
        let script = "CREATE OR REPLACE PROCEDURE app.upsert_user(IN uid integer, INOUT total integer) LANGUAGE plpgsql AS $$ BEGIN total := total + uid; END $$;";
        let mut mode = ProcedureModeState {
            enabled: true,
            ..Default::default()
        };
        mode.refresh_from_script_for(script, ProcDialect::Postgres);
        assert!(mode.detected);
        assert_eq!(mode.dialect, ProcDialect::Postgres);
        let header = mode
            .parsed
            .as_ref()
            .and_then(|p| p.as_ref().ok())
            .expect("parses");
        assert_eq!(header.params.len(), 2);
        assert!(header.params[1].is_output, "INOUT maps to is_output");
        assert!(mode.pg_parsed.is_some());
        // The same buffer refreshed as T-SQL parses differently or fails —
        // the dialect decides which grammar runs.
        mode.refresh_from_script_for("SELECT 1", ProcDialect::Postgres);
        assert!(!mode.detected);
    }

    #[test]
    fn build_pg_plan_uses_call_and_create_or_replace() {
        let script = "CREATE PROCEDURE app.tally(IN uid integer, INOUT total integer) LANGUAGE plpgsql AS $$ BEGIN total := total + uid; END $$;";
        let pg_header = pg_procedure::parse_routine_header(script).unwrap();
        let mut uid = row("uid", TypeFamily::Integer);
        uid.last_value = "7".to_string();
        let mut total = row("total", TypeFamily::Integer);
        total.is_output = true;
        total.last_value = "1".to_string();
        let plan = build_pg_plan(script, &pg_header, &[uid, total], true).unwrap();
        assert_eq!(plan.dialect, ProcDialect::Postgres);
        assert!(plan.invoke_sql.starts_with("CALL"), "{}", plan.invoke_sql);
        let deploy = plan.pg_deploy_sql.as_deref().unwrap();
        assert!(deploy.starts_with("CREATE OR REPLACE PROCEDURE"));
        assert_eq!(plan.output_names, vec!["total".to_string()]);
        assert!(!plan.sentinel_has_rc());
    }

    #[test]
    fn pg_sentinel_matches_inout_row_without_rc() {
        let column = |name: &str| ResultColumn {
            name: name.to_string(),
            data_type: "int".to_string(),
            nullable: false,
        };
        let outputs = vec!["total".to_string()];
        assert!(is_sentinel_shaped(&[column("total")], &outputs, false));
        assert!(!is_sentinel_shaped(&[column("other")], &outputs, false));
        // A procedure with no INOUT params never peels anything.
        assert!(!is_sentinel_shaped(&[column("total")], &[], false));
    }

    #[test]
    fn refresh_seeds_detection_for_programmatic_loads() {
        // Programmatic text loads (new tab from file, session restore,
        // history replay, AI panel) never emit editor Change, so the app
        // calls refresh_from_script directly after each one. These state
        // transitions are what those call sites rely on.
        let mut mode = ProcedureModeState::default();
        assert!(!mode.detected);
        assert!(mode.parsed.is_none());
        mode.refresh_from_script("CREATE PROC dbo.Foo @id int AS SELECT @id");
        assert!(mode.detected, "glow must appear without any keystroke");
        assert!(mode.parsed.is_none(), "parse stays lazy while mode is off");
        assert!(!mode.enabled);

        // Loading a non-procedure script over a procedure clears the glow.
        mode.refresh_from_script("SELECT 1");
        assert!(!mode.detected);
    }

    #[test]
    fn refresh_while_enabled_reparses_and_preserves_values() {
        let mut mode = ProcedureModeState {
            enabled: true,
            ..Default::default()
        };
        mode.refresh_from_script("CREATE PROC dbo.Foo @id int AS SELECT @id");
        let header = mode
            .parsed
            .as_ref()
            .and_then(|p| p.as_ref().ok())
            .expect("enabled mode parses eagerly");
        assert_eq!(header.qualified_name(), "[dbo].[Foo]");

        // A same-script refresh (tab switch, double load) re-parses but never
        // clobbers mode state or entered rows.
        mode.refresh_from_script("CREATE PROC dbo.Foo @id int AS SELECT @id");
        assert!(mode.enabled);
        assert!(mode.parsed.as_ref().is_some_and(|p| p.is_ok()));
    }
}
