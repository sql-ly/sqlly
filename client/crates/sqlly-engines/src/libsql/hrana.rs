//! Pure, hermetic pieces of the libSQL driver: the Hrana v2 HTTP pipeline
//! request shapes, the response decoder, the typed-value → [`CellValue`]
//! mapping, the script splitter, and the HTTP-level error reader. Nothing
//! here touches the network, so every rule has a unit test below.
//!
//! Hrana (the protocol `sqld` and Turso Cloud speak) is three JSON shapes:
//! a list of requests posted to `/v2/pipeline`, one result per request, and
//! typed values — `{"type":"integer","value":"123"}` (integers travel as
//! decimal strings so 64-bit values survive), `{"type":"float","value":1.5}`,
//! `{"type":"text","value":"…"}`, `{"type":"blob","base64":"…"}` and
//! `{"type":"null"}`. A statement that fails answers HTTP 200 with an
//! `error` result; only transport and authentication failures use HTTP
//! status codes.

use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlly_core::{CellValue, ResultColumn, SqlParam};

/// The pipeline endpoint, relative to the server root.
pub const PIPELINE_PATH: &str = "v2/pipeline";

/// One bound parameter as a Hrana typed value: integers travel as decimal
/// strings (so every `i64` survives), booleans as SQLite's `0`/`1`
/// integers, blobs as unpadded base64.
pub fn param_value(param: &SqlParam) -> Value {
    match param {
        SqlParam::Null => json!({ "type": "null" }),
        SqlParam::Bool(b) => json!({ "type": "integer", "value": if *b { "1" } else { "0" } }),
        SqlParam::I64(n) => json!({ "type": "integer", "value": n.to_string() }),
        SqlParam::F64(f) if f.is_finite() => json!({ "type": "float", "value": f }),
        // JSON has no NaN/Infinity; SQLite stores them as NULL anyway.
        SqlParam::F64(_) => json!({ "type": "null" }),
        SqlParam::String(s) => json!({ "type": "text", "value": s }),
        SqlParam::Bytes(bytes) => json!({
            "type": "blob",
            "base64": base64::engine::general_purpose::STANDARD_NO_PAD.encode(bytes),
        }),
    }
}

/// A `stmt` object: the SQL plus its positional arguments.
pub fn statement(sql: &str, params: &[SqlParam]) -> Value {
    let args: Vec<Value> = params.iter().map(param_value).collect();
    json!({ "sql": sql, "args": args })
}

/// One `execute` request.
pub fn execute_request(sql: &str, params: &[SqlParam]) -> Value {
    json!({ "type": "execute", "stmt": statement(sql, params) })
}

/// One `batch` request running `steps` in order on the stream, every step
/// conditioned on the previous step's success so the first failure stops
/// the rest exactly like the other drivers.
pub fn batch_request(steps: &[(String, Vec<SqlParam>)]) -> Value {
    let steps: Vec<Value> = steps
        .iter()
        .enumerate()
        .map(|(index, (sql, params))| {
            let mut step = json!({ "stmt": statement(sql, params) });
            if index > 0 {
                step["condition"] = json!({ "type": "ok", "step": index - 1 });
            }
            step
        })
        .collect();
    json!({ "type": "batch", "batch": { "steps": steps } })
}

/// The `close` request: the server releases the stream (rolling back any
/// open transaction) as soon as it has answered.
pub fn close_request() -> Value {
    json!({ "type": "close" })
}

/// A pipeline body: the requests, the stream's `baton` when continuing a
/// stream the server handed back, and a trailing `close` when the stream
/// is not needed afterwards.
pub fn pipeline_body(baton: Option<&str>, mut requests: Vec<Value>, close: bool) -> Value {
    if close {
        requests.push(close_request());
    }
    match baton {
        Some(baton) => json!({ "baton": baton, "requests": requests }),
        None => json!({ "requests": requests }),
    }
}

/// One `execute` request for a single statement, followed by `close` so the
/// server releases the stream as soon as it has answered.
pub fn execute_pipeline(sql: &str) -> Value {
    pipeline_body(None, vec![execute_request(sql, &[])], true)
}

/// One `batch` request running `statements` in order on a single stream —
/// so `BEGIN … COMMIT` and `USE`-like session state hold across them — with
/// every step conditioned on the previous step's success, so the first
/// failure stops the script exactly like the other drivers. `close` follows
/// so an open transaction rolls back on failure instead of lingering.
pub fn batch_pipeline(statements: &[String]) -> Value {
    let steps: Vec<(String, Vec<SqlParam>)> = statements
        .iter()
        .map(|sql| (sql.clone(), Vec::new()))
        .collect();
    pipeline_body(None, vec![batch_request(&steps)], true)
}

/// The pipeline response envelope.
#[derive(Debug, Deserialize)]
pub struct PipelineResponse {
    #[serde(default)]
    pub baton: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub results: Vec<StreamResult>,
}

/// One request's outcome.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StreamResult {
    Ok { response: StreamResponse },
    Error { error: HranaError },
}

/// The successful response to one request.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum StreamResponse {
    Execute {
        result: StmtResult,
    },
    Batch {
        result: BatchResult,
    },
    Close,
    #[serde(other)]
    Other,
}

/// A statement's result: columns (with SQLite's declared type), typed rows,
/// and the affected-row count for writes.
#[derive(Debug, Default, Deserialize)]
pub struct StmtResult {
    #[serde(default)]
    pub cols: Vec<Col>,
    #[serde(default)]
    pub rows: Vec<Vec<Value>>,
    #[serde(default)]
    pub affected_row_count: u64,
    #[serde(default)]
    pub last_insert_rowid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Col {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub decltype: Option<String>,
}

/// A batch's per-step outcomes: a step that did not run (its condition
/// failed) has neither a result nor an error.
#[derive(Debug, Default, Deserialize)]
pub struct BatchResult {
    #[serde(default)]
    pub step_results: Vec<Option<StmtResult>>,
    #[serde(default)]
    pub step_errors: Vec<Option<HranaError>>,
}

/// The server's own description of a failed statement.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HranaError {
    pub message: String,
    #[serde(default)]
    pub code: Option<String>,
}

/// One statement, decoded into the grid's shapes.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct StatementOutcome {
    /// Empty for statements that return no rows (DDL, INSERT, …).
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
    pub affected_rows: u64,
}

impl StatementOutcome {
    /// Whether the statement answered with a result set (possibly empty).
    pub fn has_result_set(&self) -> bool {
        !self.columns.is_empty()
    }
}

/// A batch's decoded outcomes in order, plus the error that stopped it (the
/// outcomes before it ran; everything after it did not).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct BatchOutcome {
    pub outcomes: Vec<StatementOutcome>,
    pub error: Option<HranaError>,
}

/// Decode a statement result. Column types are SQLite's declared type
/// (`INTEGER`, `TEXT`, …) lower-cased like the in-process driver reports
/// them, or blank for expressions; every column is nullable since SQLite
/// enforces nothing at the result level.
pub fn decode_statement(result: StmtResult) -> StatementOutcome {
    let columns = result
        .cols
        .into_iter()
        .map(|col| ResultColumn {
            name: col.name.unwrap_or_default(),
            data_type: col.decltype.unwrap_or_default().to_lowercase(),
            nullable: true,
        })
        .collect();
    let rows = result
        .rows
        .iter()
        .map(|row| row.iter().map(decode_cell).collect())
        .collect();
    StatementOutcome {
        columns,
        rows,
        affected_rows: result.affected_row_count,
    }
}

/// Decode a batch: outcomes for the steps that ran, and the first error.
pub fn decode_batch(result: BatchResult) -> BatchOutcome {
    let error = result.step_errors.into_iter().flatten().next();
    let outcomes = result
        .step_results
        .into_iter()
        .flatten()
        .map(decode_statement)
        .collect();
    BatchOutcome { outcomes, error }
}

/// Convert one Hrana typed value into a cell. Integers that fit `i64`
/// become `I64` (SQLite integers always do; wider digits stay exact text),
/// floats become `F64`, text stays text, blobs decode from base64 into
/// bytes, and a float the server could not encode (NaN, ±Infinity travel
/// as `null`) is shown as text rather than silently as NULL.
pub fn decode_cell(value: &Value) -> CellValue {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return match value {
            Value::Null => CellValue::Null,
            other => CellValue::String(other.to_string()),
        };
    };
    match kind {
        "null" => CellValue::Null,
        "integer" => match value.get("value") {
            Some(Value::String(digits)) => match digits.parse::<i64>() {
                Ok(n) => CellValue::I64(n),
                Err(_) => CellValue::String(digits.clone()),
            },
            Some(Value::Number(n)) => match n.as_i64() {
                Some(i) => CellValue::I64(i),
                None => CellValue::String(n.to_string()),
            },
            _ => CellValue::Null,
        },
        "float" => match value.get("value") {
            Some(Value::Number(n)) => match n.as_f64() {
                Some(f) => CellValue::F64(f),
                None => CellValue::String(n.to_string()),
            },
            Some(Value::String(s)) => match s.parse::<f64>() {
                Ok(f) if f.is_finite() => CellValue::F64(f),
                _ => CellValue::String(s.clone()),
            },
            _ => CellValue::String("non-finite".to_string()),
        },
        "text" => match value.get("value") {
            Some(Value::String(s)) => CellValue::String(s.clone()),
            Some(other) => CellValue::String(other.to_string()),
            None => CellValue::Null,
        },
        "blob" => match value.get("base64").and_then(Value::as_str) {
            Some(encoded) => match decode_base64(encoded) {
                Some(bytes) => CellValue::Bytes(bytes),
                None => CellValue::String(encoded.to_string()),
            },
            None => CellValue::Null,
        },
        _ => CellValue::String(value.to_string()),
    }
}

/// Hrana blobs are standard base64 without padding; accept padded input too.
fn decode_base64(encoded: &str) -> Option<Vec<u8>> {
    let trimmed = encoded.trim().trim_end_matches('=');
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(trimmed)
        .ok()
}

/// The message to surface for a non-200 response. `sqld` answers
/// authentication and protocol failures with `{"error":"…"}` or
/// `{"message":"…","code":"…"}`; anything else is shown as-is.
pub fn error_message(status: u16, body: &str) -> String {
    let body = body.trim();
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        for key in ["error", "message"] {
            if let Some(text) = value.get(key).and_then(Value::as_str)
                && !text.is_empty()
            {
                return text.to_string();
            }
        }
    }
    if body.is_empty() {
        format!("libSQL server returned HTTP {status}")
    } else {
        body.to_string()
    }
}

/// Whether a non-200 response is a sign-in failure: a missing or wrong
/// token (401/403) or a malformed one, which `sqld` reports as a 400 naming
/// the JWT.
pub fn is_authentication_error(status: u16, body: &str) -> bool {
    status == 401
        || status == 403
        || (status == 400 && (body.contains("JWT") || body.contains("Unauthorized")))
}

/// Split a script into the statements Hrana must receive one at a time (a
/// request holding two statements is refused with `SQL_MANY_STATEMENTS`).
/// This is `seclex::execution_units` — the same split the safety gates
/// judge: `;` inside strings, quoted identifiers and comments never splits; a
/// trigger body's inner statements stay with their `CREATE TRIGGER`; empty
/// and comment-only pieces are dropped.
pub fn split_script(sql: &str) -> Vec<String> {
    sqlly_lang::seclex::execution_units(sql, sqlly_core::DatabaseType::LibSql)
        .into_iter()
        .map(|unit| unit.text)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_and_batch_requests_take_the_documented_shape() {
        let single = execute_pipeline("SELECT 1");
        assert_eq!(single["requests"][0]["type"], "execute");
        assert_eq!(single["requests"][0]["stmt"]["sql"], "SELECT 1");
        assert_eq!(single["requests"][1]["type"], "close");

        let batch = batch_pipeline(&["BEGIN".into(), "SELECT 1".into(), "COMMIT".into()]);
        let steps = batch["requests"][0]["batch"]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 3);
        assert!(steps[0].get("condition").is_none());
        assert_eq!(steps[1]["condition"]["type"], "ok");
        assert_eq!(steps[1]["condition"]["step"], 0);
        assert_eq!(steps[2]["condition"]["step"], 1);
        assert_eq!(steps[2]["stmt"]["sql"], "COMMIT");
        assert_eq!(batch["requests"][1]["type"], "close");
    }

    #[test]
    fn parameters_encode_as_hrana_typed_values() {
        assert_eq!(param_value(&SqlParam::Null), json!({"type": "null"}));
        assert_eq!(
            param_value(&SqlParam::Bool(true)),
            json!({"type": "integer", "value": "1"})
        );
        assert_eq!(
            param_value(&SqlParam::I64(i64::MIN)),
            json!({"type": "integer", "value": "-9223372036854775808"})
        );
        assert_eq!(
            param_value(&SqlParam::F64(1.5)),
            json!({"type": "float", "value": 1.5})
        );
        assert_eq!(
            param_value(&SqlParam::F64(f64::NAN)),
            json!({"type": "null"})
        );
        assert_eq!(
            param_value(&SqlParam::String("é".into())),
            json!({"type": "text", "value": "é"})
        );
        assert_eq!(
            param_value(&SqlParam::Bytes(vec![0x01, 0xff])),
            json!({"type": "blob", "base64": "Af8"})
        );
        // Blobs round-trip through the decoder without padding.
        let encoded = param_value(&SqlParam::Bytes(vec![0, 0, 0]));
        assert_eq!(decode_cell(&encoded), CellValue::Bytes(vec![0, 0, 0]));
    }

    #[test]
    fn stream_bodies_carry_the_baton_args_and_optional_close() {
        let body = pipeline_body(
            Some("tok"),
            vec![execute_request(
                "UPDATE t SET a = ? WHERE id = ?",
                &[SqlParam::String("x".into()), SqlParam::I64(7)],
            )],
            false,
        );
        assert_eq!(body["baton"], "tok");
        let requests = body["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 1, "no close while the stream stays open");
        assert_eq!(requests[0]["stmt"]["args"][0]["value"], "x");
        assert_eq!(requests[0]["stmt"]["args"][1]["value"], "7");

        let steps = vec![
            ("BEGIN".to_string(), Vec::new()),
            (
                "DELETE FROM t WHERE id = ?".to_string(),
                vec![SqlParam::I64(1)],
            ),
        ];
        let body = pipeline_body(None, vec![batch_request(&steps)], true);
        assert!(body.get("baton").is_none());
        let requests = body["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["batch"]["steps"][1]["condition"]["step"], 0);
        assert_eq!(
            requests[0]["batch"]["steps"][1]["stmt"]["args"][0]["value"],
            "1"
        );
        assert_eq!(requests[1]["type"], "close");
    }

    #[test]
    fn typed_values_map_to_cells() {
        assert_eq!(decode_cell(&json!({"type": "null"})), CellValue::Null);
        assert_eq!(
            decode_cell(&json!({"type": "integer", "value": "9223372036854775807"})),
            CellValue::I64(i64::MAX)
        );
        assert_eq!(
            decode_cell(&json!({"type": "integer", "value": "-9223372036854775808"})),
            CellValue::I64(i64::MIN)
        );
        assert_eq!(
            decode_cell(&json!({"type": "integer", "value": "99999999999999999999"})),
            CellValue::String("99999999999999999999".into())
        );
        assert_eq!(
            decode_cell(&json!({"type": "integer", "value": 7})),
            CellValue::I64(7)
        );
        assert_eq!(
            decode_cell(&json!({"type": "float", "value": 1.5})),
            CellValue::F64(1.5)
        );
        assert_eq!(
            decode_cell(&json!({"type": "float", "value": null})),
            CellValue::String("non-finite".into())
        );
        assert_eq!(
            decode_cell(&json!({"type": "text", "value": "é"})),
            CellValue::String("é".into())
        );
        assert_eq!(
            decode_cell(&json!({"type": "blob", "base64": "Af8"})),
            CellValue::Bytes(vec![0x01, 0xff])
        );
        assert_eq!(
            decode_cell(&json!({"type": "blob", "base64": "Af8="})),
            CellValue::Bytes(vec![0x01, 0xff])
        );
        assert_eq!(
            decode_cell(&json!({"type": "blob", "base64": ""})),
            CellValue::Bytes(Vec::new())
        );
        assert_eq!(
            decode_cell(&json!({"type": "blob", "base64": "AAAA"})),
            CellValue::Bytes(vec![0, 0, 0])
        );
        assert_eq!(decode_cell(&Value::Null), CellValue::Null);
        assert_eq!(
            decode_cell(&json!({"type": "mystery", "value": 1})),
            CellValue::String("{\"type\":\"mystery\",\"value\":1}".into())
        );
    }

    #[test]
    fn statement_results_decode_columns_rows_and_counts() {
        let body = r#"{"cols":[{"name":"id","decltype":"INTEGER"},{"name":"n","decltype":null},{"name":"data","decltype":"BLOB"}],"rows":[[{"type":"integer","value":"1"},{"type":"null"},{"type":"blob","base64":"Af8"}]],"affected_row_count":0,"last_insert_rowid":null}"#;
        let result: StmtResult = serde_json::from_str(body).unwrap();
        let outcome = decode_statement(result);
        assert!(outcome.has_result_set());
        assert_eq!(outcome.columns.len(), 3);
        assert_eq!(outcome.columns[0].name, "id");
        assert_eq!(outcome.columns[0].data_type, "integer");
        assert!(outcome.columns[0].nullable);
        assert_eq!(outcome.columns[1].data_type, "");
        assert_eq!(
            outcome.rows,
            vec![vec![
                CellValue::I64(1),
                CellValue::Null,
                CellValue::Bytes(vec![1, 255])
            ]]
        );

        let write: StmtResult = serde_json::from_str(
            r#"{"cols":[],"rows":[],"affected_row_count":3,"last_insert_rowid":"9"}"#,
        )
        .unwrap();
        let outcome = decode_statement(write);
        assert!(!outcome.has_result_set());
        assert_eq!(outcome.affected_rows, 3);
    }

    #[test]
    fn pipeline_envelopes_decode_execute_batch_error_and_close() {
        let body = r#"{"baton":null,"base_url":null,"results":[
            {"type":"ok","response":{"type":"execute","result":{"cols":[{"name":"x","decltype":null}],"rows":[[{"type":"integer","value":"1"}]],"affected_row_count":0,"last_insert_rowid":null,"replication_index":"1","rows_read":1,"rows_written":0,"query_duration_ms":0.05}}},
            {"type":"error","error":{"message":"SQLite error: no such table: t2","code":"SQLITE_UNKNOWN"}},
            {"type":"ok","response":{"type":"batch","result":{"step_results":[{"cols":[],"rows":[],"affected_row_count":1,"last_insert_rowid":"2"},null,null],"step_errors":[null,{"message":"SQL input error: no such column: nope (at offset 7)","code":"SQL_INPUT_ERROR"},null],"replication_index":"2"}}},
            {"type":"ok","response":{"type":"close"}}
        ]}"#;
        let response: PipelineResponse = serde_json::from_str(body).unwrap();
        assert!(response.baton.is_none());
        assert_eq!(response.results.len(), 4);
        let StreamResult::Ok {
            response: StreamResponse::Execute { result },
        } = &response.results[0]
        else {
            panic!("{:?}", response.results[0]);
        };
        assert_eq!(result.rows.len(), 1);
        let StreamResult::Error { error } = &response.results[1] else {
            panic!("{:?}", response.results[1]);
        };
        assert_eq!(error.code.as_deref(), Some("SQLITE_UNKNOWN"));
        let StreamResult::Ok {
            response: StreamResponse::Batch { result },
        } = response.results.into_iter().nth(2).unwrap()
        else {
            panic!("expected a batch");
        };
        let batch = decode_batch(result);
        assert_eq!(batch.outcomes.len(), 1, "only the step that ran");
        assert_eq!(batch.outcomes[0].affected_rows, 1);
        assert_eq!(
            batch.error.unwrap().message,
            "SQL input error: no such column: nope (at offset 7)"
        );
    }

    #[test]
    fn http_errors_are_read_from_json_bodies_and_classified() {
        assert_eq!(
            error_message(
                401,
                r#"{"error":"Unauthorized: `unauthorized access attempt on database: empty JWT token`"}"#
            ),
            "Unauthorized: `unauthorized access attempt on database: empty JWT token`"
        );
        assert_eq!(
            error_message(
                400,
                r#"{"message":"The stream has expired due to inactivity","code":"STREAM_EXPIRED"}"#
            ),
            "The stream has expired due to inactivity"
        );
        assert_eq!(error_message(502, ""), "libSQL server returned HTTP 502");
        assert_eq!(error_message(500, "plain text"), "plain text");
        assert!(is_authentication_error(401, ""));
        assert!(is_authentication_error(403, "{}"));
        assert!(is_authentication_error(
            400,
            r#"{"error":"JWT error: InvalidToken"}"#
        ));
        assert!(!is_authentication_error(
            400,
            r#"{"message":"Cannot deserialize client message from JSON"}"#
        ));
    }

    #[test]
    fn script_splits_on_semicolons_but_keeps_trigger_bodies_whole() {
        let script = "-- leading\nSELECT ';' AS a; /* x; y */ INSERT INTO t VALUES ('a;b'), (\"c;d\")\n;\n\n  ;\nSELECT 2 -- trailing; comment\n";
        let parts = split_script(script);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0], "SELECT ';' AS a");
        assert_eq!(parts[1], "INSERT INTO t VALUES ('a;b'), (\"c;d\")");
        assert_eq!(parts[2], "SELECT 2 -- trailing; comment");
        assert!(split_script("   \n-- nothing\n").is_empty());
        assert_eq!(split_script("SELECT 1"), vec!["SELECT 1".to_string()]);

        let trigger = "CREATE TABLE t (a);\nCREATE TRIGGER trg AFTER INSERT ON t BEGIN\n  UPDATE t SET a = 1; DELETE FROM t WHERE a = 2;\nEND;\nSELECT 1";
        let parts = split_script(trigger);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert!(parts[1].starts_with("CREATE TRIGGER trg"));
        assert!(parts[1].contains("UPDATE t SET a = 1;"));
        assert!(parts[1].trim_end().ends_with("END"));
        assert_eq!(parts[2], "SELECT 1");

        let temp = "create temp trigger x before delete on t begin select 1; end";
        assert_eq!(split_script(temp).len(), 1);
    }
}
