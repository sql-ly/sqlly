//! PGlite bridge — a live Postgres running in the same browser tab.
//!
//! The host page boots [PGlite](https://pglite.dev) (Postgres compiled to
//! wasm), loads the Chinook sample database into it, and exposes a tiny
//! Promise-returning API at `globalThis.sqllyPglite` (see
//! `apps/browser/www/boot.js`, loaded by `apps/browser/www/index.html`).
//! This module is the Rust side of that seam:
//! it sends SQL over the bridge and parses the JSON response into the native
//! [`QueryEvent`] / [`CellValue`] stream.
//!
//! The service-port traits require `Send` futures, but every JavaScript value
//! is `!Send`. All JS interaction therefore happens inside a
//! `spawn_local` task on the browser's single thread; callers await a
//! `oneshot` receiver, which is `Send`, so the trait futures stay `Send`
//! without touching JS types themselves.

use std::future::Future;

use serde::Deserialize;
use sqlly_core::{
    CellValue, ConnectionKind, ConnectionProfile, ResultColumn, StudioError, StudioResult,
};
use tokio::sync::oneshot;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};

use crate::query::{CellLimits, QueryEvent, truncate_rows};

/// True when queries and metadata for `profile` should go to the in-tab
/// PGlite instance instead of the SQLite/demo backend: any engine that speaks
/// the Postgres wire protocol (PGlite is a real Postgres).
pub fn is_pglite_profile(profile: &ConnectionProfile) -> bool {
    profile.database_type.connection_kind() == ConnectionKind::PostgresWire
}

/// The JSON envelope `sqllyPglite.exec` resolves with.
#[derive(Deserialize)]
struct BridgeResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    results: Vec<BridgeResultSet>,
}

/// One statement's result: PGlite's `Results` with `rowMode: 'array'`.
#[derive(Deserialize)]
pub struct BridgeResultSet {
    #[serde(default)]
    pub fields: Vec<BridgeField>,
    #[serde(default)]
    pub rows: Vec<Vec<serde_json::Value>>,
    #[serde(default, rename = "affectedRows")]
    pub affected_rows: u64,
}

#[derive(Deserialize)]
pub struct BridgeField {
    pub name: String,
    #[serde(default, rename = "dataTypeID")]
    pub data_type_id: u32,
}

fn js_error(context: &str, value: JsValue) -> StudioError {
    let detail = value
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(&value, &JsValue::from_str("message"))
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| format!("{value:?}"));
    StudioError::QueryExecution(format!("{context}: {detail}"))
}

/// Call `globalThis.sqllyPglite.exec(sql)` and parse the response. Touches JS
/// values, so it is `!Send` — run it on the browser thread (directly from a
/// `spawn_local` task, or through the [`exec`] wrapper).
pub(super) async fn exec_js(sql: String) -> StudioResult<Vec<BridgeResultSet>> {
    let global = js_sys::global();
    let bridge = js_sys::Reflect::get(&global, &JsValue::from_str("sqllyPglite"))
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null())
        .ok_or_else(|| {
            StudioError::Connection(
                "the in-tab PGlite engine is not loaded (the Chinook sample may still be \
                 downloading, or it failed to start — see the browser console)"
                    .into(),
            )
        })?;
    let exec: js_sys::Function = js_sys::Reflect::get(&bridge, &JsValue::from_str("exec"))
        .ok()
        .and_then(|f| f.dyn_into().ok())
        .ok_or_else(|| {
            StudioError::Connection("the sqllyPglite bridge has no exec function".into())
        })?;
    let promise: js_sys::Promise = exec
        .call1(&bridge, &JsValue::from_str(&sql))
        .map_err(|e| js_error("PGlite call failed", e))?
        .dyn_into()
        .map_err(|_| {
            StudioError::QueryExecution("sqllyPglite.exec did not return a Promise".into())
        })?;
    let value = JsFuture::from(promise)
        .await
        .map_err(|e| js_error("PGlite execution failed", e))?;
    let json = value.as_string().ok_or_else(|| {
        StudioError::QueryExecution("sqllyPglite.exec did not resolve to a string".into())
    })?;
    let response: BridgeResponse = serde_json::from_str(&json)
        .map_err(|e| StudioError::QueryExecution(format!("malformed PGlite response: {e}")))?;
    if response.ok {
        Ok(response.results)
    } else {
        Err(StudioError::QueryExecution(
            response.error.unwrap_or_else(|| "PGlite error".into()),
        ))
    }
}

/// `Send` wrapper around [`exec_js`] for the port methods that need a value
/// back (connection test, database listing): the JS work runs in a
/// `spawn_local` task and the result crosses back over a oneshot channel.
pub fn exec(
    sql: impl Into<String>,
) -> impl Future<Output = StudioResult<Vec<BridgeResultSet>>> + Send {
    let sql = sql.into();
    let (tx, rx) = oneshot::channel();
    spawn_local(async move {
        let _ = tx.send(exec_js(sql).await);
    });
    async move {
        rx.await.unwrap_or_else(|_| {
            Err(StudioError::Connection(
                "the PGlite bridge task was dropped".into(),
            ))
        })
    }
}

/// Run `sql` and collect the first column of every row as strings.
pub fn query_scalar_strings(
    sql: impl Into<String>,
) -> impl Future<Output = StudioResult<Vec<String>>> + Send {
    let call = exec(sql);
    async move {
        Ok(call
            .await?
            .into_iter()
            .flat_map(|set| set.rows)
            .filter_map(|row| row.into_iter().next())
            .map(|value| match value {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            })
            .collect())
    }
}

/// Map bridge result sets onto the [`QueryEvent`] sequence the streaming
/// contract promises (everything between `Started` and `Completed`).
pub(super) fn events_from_results(results: Vec<BridgeResultSet>) -> Vec<QueryEvent> {
    let mut events = Vec::new();
    let mut index = 0usize;
    for set in results {
        if set.fields.is_empty() {
            // A statement with no result set (INSERT/UPDATE/DDL).
            events.push(QueryEvent::Message {
                text: format!("({} row(s) affected)", set.affected_rows),
            });
            continue;
        }
        let columns: Vec<ResultColumn> = set
            .fields
            .iter()
            .map(|f| ResultColumn {
                name: f.name.clone(),
                data_type: type_name_for_oid(f.data_type_id).to_string(),
                nullable: true,
            })
            .collect();
        events.push(QueryEvent::ResultSet { index, columns });
        let mut rows: Vec<Vec<CellValue>> = set
            .rows
            .iter()
            .map(|row| row.iter().map(json_to_cell).collect())
            .collect();
        let count = rows.len() as u64;
        let truncated = truncate_rows(&mut rows, 0, &CellLimits::default());
        events.push(QueryEvent::Rows { index, rows });
        if !truncated.is_empty() {
            events.push(QueryEvent::TruncatedCells {
                index,
                cells: truncated,
            });
        }
        events.push(QueryEvent::RowCount {
            index,
            rows_affected: count,
        });
        index += 1;
    }
    events
}

/// PGlite parses cells into JS values before they cross the bridge as JSON;
/// anything non-scalar (json/jsonb, arrays) arrives re-serialized as text.
fn json_to_cell(value: &serde_json::Value) -> CellValue {
    use serde_json::Value;
    match value {
        Value::Null => CellValue::Null,
        Value::Bool(b) => CellValue::Bool(*b),
        Value::Number(n) => n
            .as_i64()
            .map(CellValue::I64)
            .unwrap_or_else(|| CellValue::F64(n.as_f64().unwrap_or(f64::NAN))),
        Value::String(s) => CellValue::String(s.clone()),
        other => CellValue::String(other.to_string()),
    }
}

/// Display names for the Postgres type OIDs Chinook and the catalog queries
/// commonly produce; the grid only uses these as labels.
fn type_name_for_oid(oid: u32) -> &'static str {
    match oid {
        16 => "bool",
        17 => "bytea",
        18 => "char",
        19 => "name",
        20 => "int8",
        21 => "int2",
        23 => "int4",
        24 => "regproc",
        25 => "text",
        26 => "oid",
        114 => "json",
        142 => "xml",
        700 => "float4",
        701 => "float8",
        790 => "money",
        1007 => "int4[]",
        1009 => "text[]",
        1015 => "varchar[]",
        1042 => "bpchar",
        1043 => "varchar",
        1082 => "date",
        1083 => "time",
        1114 => "timestamp",
        1184 => "timestamptz",
        1186 => "interval",
        1266 => "timetz",
        1700 => "numeric",
        2950 => "uuid",
        3802 => "jsonb",
        _ => "unknown",
    }
}
