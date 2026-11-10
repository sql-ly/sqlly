//! Pure, hermetic pieces of the ClickHouse driver: the
//! `JSONCompactEachRowWithNamesAndTypes` response parser, the ClickHouse
//! type → [`CellValue`] mapping, the script splitter, the query/statement
//! classifier, and the `X-ClickHouse-Summary` header reader. Nothing here
//! touches the network, so every rule has a unit test below.

use serde_json::Value;
use sqlly_core::{CellValue, ResultColumn};

/// The output format every query is asked for: one JSON array per line, the
/// first line holding column names, the second column types, then rows.
pub const RESULT_FORMAT: &str = "JSONCompactEachRowWithNamesAndTypes";

/// How a column's JSON values map onto [`CellValue`], decided once per
/// result set from the ClickHouse type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    /// `Bool`.
    Bool,
    /// Every integer width. Values that fit `i64` become `I64`; wider ones
    /// (`UInt64` above `i64::MAX`, `Int128`, `UInt256`, …) stay exact text.
    Integer,
    /// `Float32` / `Float64` / `BFloat16`. Denormals arrive quoted
    /// (`"nan"`, `"inf"`) and stay text.
    Float,
    /// Everything ClickHouse renders as a JSON string: `String`,
    /// `FixedString`, `Decimal*` (quoted, exact), `Date*`, `DateTime*`,
    /// `UUID`, `Enum*`, `IPv4`/`IPv6`, `Interval*`, `Time*`.
    Text,
    /// `Array`, `Tuple`, `Map`, `Nested`, geo types, `JSON`/`Object`,
    /// `Variant`, `Dynamic`: the value is re-serialized as compact JSON text.
    Json,
    /// `Nothing` (the type of a bare `NULL`).
    Nothing,
}

/// Strip the wrapper types that do not change how a value renders:
/// `Nullable(T)` and `LowCardinality(T)`, in any nesting.
pub fn unwrap_type(type_name: &str) -> &str {
    let mut current = type_name.trim();
    loop {
        let inner = ["Nullable(", "LowCardinality("]
            .iter()
            .find_map(|prefix| current.strip_prefix(prefix))
            .and_then(|rest| rest.strip_suffix(')'));
        match inner {
            Some(inner) => current = inner.trim(),
            None => return current,
        }
    }
}

/// Whether a column type admits NULL (`Nullable(...)` at the top, possibly
/// under `LowCardinality`).
pub fn is_nullable(type_name: &str) -> bool {
    let mut current = type_name.trim();
    loop {
        if let Some(rest) = current.strip_prefix("Nullable(") {
            let _ = rest;
            return true;
        }
        match current
            .strip_prefix("LowCardinality(")
            .and_then(|rest| rest.strip_suffix(')'))
        {
            Some(inner) => current = inner.trim(),
            None => return false,
        }
    }
}

/// The base name of a type: `DateTime64(3, 'UTC')` → `DateTime64`,
/// `Enum8('a' = 1)` → `Enum8`, `Array(UInt8)` → `Array`.
fn base_name(type_name: &str) -> &str {
    let unwrapped = unwrap_type(type_name);
    unwrapped
        .split(['(', ' '])
        .next()
        .unwrap_or(unwrapped)
        .trim()
}

/// Decide how a column's values convert from the type ClickHouse reports.
pub fn column_kind(type_name: &str) -> ColumnKind {
    match base_name(type_name) {
        "Bool" => ColumnKind::Bool,
        "Int8" | "Int16" | "Int32" | "Int64" | "Int128" | "Int256" | "UInt8" | "UInt16"
        | "UInt32" | "UInt64" | "UInt128" | "UInt256" => ColumnKind::Integer,
        "Float32" | "Float64" | "BFloat16" => ColumnKind::Float,
        "Nothing" => ColumnKind::Nothing,
        "Array"
        | "Tuple"
        | "Map"
        | "Nested"
        | "JSON"
        | "Object"
        | "Variant"
        | "Dynamic"
        | "Point"
        | "Ring"
        | "LineString"
        | "MultiLineString"
        | "Polygon"
        | "MultiPolygon"
        | "AggregateFunction"
        | "SimpleAggregateFunction" => ColumnKind::Json,
        _ => ColumnKind::Text,
    }
}

/// Convert one JSON value from a row into the grid's cell shape.
pub fn cell_from_json(value: &Value, kind: ColumnKind) -> CellValue {
    match value {
        Value::Null => CellValue::Null,
        Value::Bool(b) => CellValue::Bool(*b),
        Value::Number(n) => match kind {
            ColumnKind::Integer => match n.as_i64() {
                Some(i) => CellValue::I64(i),
                None => CellValue::String(n.to_string()),
            },
            ColumnKind::Float => match n.as_f64() {
                Some(f) => CellValue::F64(f),
                None => CellValue::String(n.to_string()),
            },
            ColumnKind::Text | ColumnKind::Json => CellValue::String(n.to_string()),
            ColumnKind::Bool => match n.as_i64() {
                Some(i) => CellValue::Bool(i != 0),
                None => CellValue::String(n.to_string()),
            },
            ColumnKind::Nothing => CellValue::Null,
        },
        Value::String(s) => match kind {
            // Quoted 64-bit and wider integers (`output_format_json_quote_64bit_integers`).
            ColumnKind::Integer => match s.parse::<i64>() {
                Ok(i) => CellValue::I64(i),
                Err(_) => CellValue::String(s.clone()),
            },
            // Quoted denormals ("nan", "inf", "-inf") stay text; a quoted
            // finite float (never emitted by default) is parsed.
            ColumnKind::Float => match s.parse::<f64>() {
                Ok(f) if f.is_finite() => CellValue::F64(f),
                _ => CellValue::String(s.clone()),
            },
            ColumnKind::Bool => match s.as_str() {
                "true" | "1" => CellValue::Bool(true),
                "false" | "0" => CellValue::Bool(false),
                _ => CellValue::String(s.clone()),
            },
            ColumnKind::Text | ColumnKind::Json => CellValue::String(s.clone()),
            ColumnKind::Nothing => CellValue::Null,
        },
        Value::Array(_) | Value::Object(_) => CellValue::String(value.to_string()),
    }
}

/// One parsed line of a `JSONCompactEachRowWithNamesAndTypes` body.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// The header is complete: column descriptors are known.
    Header(Vec<ResultColumn>),
    /// A data row.
    Row(Vec<CellValue>),
    /// A ClickHouse exception written into the body (`Code: 62.
    /// DB::Exception: …`), which the server appends when a query fails
    /// after the response started.
    ServerError(String),
    /// Text that is neither JSON nor an exception: a statement that chose
    /// its own `FORMAT`, or `SHOW CREATE`-style plain output.
    Text(String),
}

/// Incremental parser for a streamed response body. Feed it bytes as they
/// arrive; it yields one [`Line`] per complete line and keeps a partial
/// trailing line for the next chunk.
#[derive(Debug, Default)]
pub struct ResponseParser {
    buffer: Vec<u8>,
    names: Option<Vec<String>>,
    columns: Option<Vec<ResultColumn>>,
    kinds: Vec<ColumnKind>,
}

impl ResponseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once the names-and-types header has been consumed.
    pub fn has_header(&self) -> bool {
        self.columns.is_some()
    }

    pub fn columns(&self) -> Option<&[ResultColumn]> {
        self.columns.as_deref()
    }

    /// Append a chunk and drain every complete line it completes.
    pub fn push_bytes(&mut self, chunk: &[u8]) -> Vec<Line> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]);
            if let Some(item) = self.push_line(text.trim_end_matches('\r')) {
                out.push(item);
            }
        }
        out
    }

    /// Flush a final line that had no trailing newline (the body ended).
    pub fn finish(&mut self) -> Option<Line> {
        if self.buffer.is_empty() {
            return None;
        }
        let rest = std::mem::take(&mut self.buffer);
        let text = String::from_utf8_lossy(&rest).to_string();
        self.push_line(text.trim_end_matches('\r'))
    }

    /// Interpret one complete line.
    pub fn push_line(&mut self, line: &str) -> Option<Line> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        if is_server_error(trimmed) {
            return Some(Line::ServerError(trimmed.to_string()));
        }
        let parsed: Result<Value, _> = serde_json::from_str(trimmed);
        let Ok(Value::Array(items)) = parsed else {
            return Some(Line::Text(line.to_string()));
        };
        if self.names.is_none() {
            let names = items
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            self.names = Some(names);
            return None;
        }
        if self.columns.is_none() {
            let names = self.names.clone().unwrap_or_default();
            let types: Vec<String> = items
                .iter()
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect();
            self.kinds = types.iter().map(|t| column_kind(t)).collect();
            let columns: Vec<ResultColumn> = names
                .into_iter()
                .zip(types)
                .map(|(name, data_type)| ResultColumn {
                    nullable: is_nullable(&data_type),
                    name,
                    data_type,
                })
                .collect();
            self.columns = Some(columns.clone());
            return Some(Line::Header(columns));
        }
        let cells = items
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let kind = self.kinds.get(i).copied().unwrap_or(ColumnKind::Text);
                cell_from_json(v, kind)
            })
            .collect();
        Some(Line::Row(cells))
    }
}

/// A line the server wrote as an exception rather than data.
pub fn is_server_error(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("Code: ") && trimmed.contains("DB::Exception")
}

/// The affected-row count for a statement, from the `X-ClickHouse-Summary`
/// header (`{"written_rows":"1234", …}`).
pub fn written_rows_from_summary(summary: &str) -> Option<u64> {
    let value: Value = serde_json::from_str(summary).ok()?;
    match value.get("written_rows")? {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// Skip leading whitespace and comments as ClickHouse reads them (nested
/// block comments, `# ` comments).
pub fn strip_leading_trivia(text: &str) -> &str {
    sqlly_lang::seclex::strip_leading_trivia(text, sqlly_lang::seclex::LexEngine::ClickHouse)
}

/// Split a script into the statements ClickHouse must receive one at a
/// time (its HTTP interface refuses multi-statement bodies). This is
/// `seclex::execution_units` — the same split the safety gates judge — so
/// `;` inside strings, quoted identifiers (with ClickHouse's backslash
/// escapes) and nested comments never splits; empty and comment-only pieces
/// are dropped.
pub fn split_script(sql: &str) -> Vec<String> {
    sqlly_lang::seclex::execution_units(sql, sqlly_core::DatabaseType::ClickHouse)
        .into_iter()
        .map(|unit| unit.text)
        .collect()
}

/// Statements that answer with a result set. Everything else (DDL, INSERT,
/// ALTER, KILL, SYSTEM, …) is run with `wait_end_of_query` so the summary
/// header carries the final written-row count.
const QUERY_KEYWORDS: [&str; 9] = [
    "SELECT", "WITH", "SHOW", "DESCRIBE", "DESC", "EXPLAIN", "EXISTS", "CHECK", "WATCH",
];

fn leading_keyword(text: &str) -> String {
    strip_leading_trivia(text)
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Whether a statement is expected to return rows.
pub fn is_query_statement(text: &str) -> bool {
    let keyword = leading_keyword(text);
    QUERY_KEYWORDS.contains(&keyword.as_str())
}

/// The message to surface for a non-200 response: ClickHouse's own
/// exception text from the body, or the HTTP status when the body is empty.
pub fn error_message(status: u16, body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        format!("ClickHouse returned HTTP {status}")
    } else {
        body.to_string()
    }
}

/// Whether an error text is an authentication failure (ClickHouse code 516
/// `AUTHENTICATION_FAILED`, or a proxy's 401/403).
pub fn is_authentication_error(status: u16, body: &str) -> bool {
    status == 401
        || status == 403
        || body.contains("AUTHENTICATION_FAILED")
        || body.contains("Code: 516")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrappers_are_unwrapped_and_nullability_detected() {
        assert_eq!(unwrap_type("Nullable(Int64)"), "Int64");
        assert_eq!(unwrap_type("LowCardinality(Nullable(String))"), "String");
        assert_eq!(
            unwrap_type("Array(Nullable(Int32))"),
            "Array(Nullable(Int32))"
        );
        assert!(is_nullable("Nullable(Int64)"));
        assert!(is_nullable("LowCardinality(Nullable(String))"));
        assert!(!is_nullable("Array(Nullable(Int32))"));
        assert!(!is_nullable("String"));
    }

    #[test]
    fn every_type_family_maps_to_a_kind() {
        assert_eq!(column_kind("Bool"), ColumnKind::Bool);
        for t in [
            "Int8",
            "Int16",
            "Int32",
            "Int64",
            "Int128",
            "Int256",
            "UInt8",
            "UInt16",
            "UInt32",
            "UInt64",
            "UInt128",
            "UInt256",
            "Nullable(Int64)",
            "LowCardinality(UInt8)",
        ] {
            assert_eq!(column_kind(t), ColumnKind::Integer, "{t}");
        }
        for t in ["Float32", "Float64", "BFloat16", "Nullable(Float64)"] {
            assert_eq!(column_kind(t), ColumnKind::Float, "{t}");
        }
        for t in [
            "String",
            "FixedString(4)",
            "Decimal(18, 2)",
            "Decimal128(9)",
            "Date",
            "Date32",
            "DateTime",
            "DateTime('UTC')",
            "DateTime64(6, 'Europe/Berlin')",
            "UUID",
            "Enum8('x' = 1, 'y' = 2)",
            "Enum16('a' = 1)",
            "IPv4",
            "IPv6",
            "IntervalDay",
            "Time",
            "Time64(3)",
            "LowCardinality(String)",
        ] {
            assert_eq!(column_kind(t), ColumnKind::Text, "{t}");
        }
        for t in [
            "Array(UInt8)",
            "Tuple(UInt8, String)",
            "Tuple(x UInt8, y String)",
            "Map(String, UInt8)",
            "Nested(a UInt8)",
            "JSON",
            "Object('json')",
            "Variant(String, UInt8)",
            "Dynamic",
            "Point",
            "Polygon",
        ] {
            assert_eq!(column_kind(t), ColumnKind::Json, "{t}");
        }
        assert_eq!(column_kind("Nothing"), ColumnKind::Nothing);
        assert_eq!(column_kind("Nullable(Nothing)"), ColumnKind::Nothing);
    }

    #[test]
    fn integers_fit_or_stay_exact_text() {
        let kind = ColumnKind::Integer;
        assert_eq!(
            cell_from_json(&serde_json::json!(5), kind),
            CellValue::I64(5)
        );
        assert_eq!(
            cell_from_json(&serde_json::json!("-9223372036854775808"), kind),
            CellValue::I64(i64::MIN)
        );
        assert_eq!(
            cell_from_json(&serde_json::json!("18446744073709551615"), kind),
            CellValue::String("18446744073709551615".into())
        );
        assert_eq!(
            cell_from_json(&serde_json::json!(18446744073709551615u64), kind),
            CellValue::String("18446744073709551615".into())
        );
        assert_eq!(
            cell_from_json(&serde_json::json!("-100000000000000000000"), kind),
            CellValue::String("-100000000000000000000".into())
        );
    }

    #[test]
    fn floats_decimals_bools_and_nulls_map_faithfully() {
        assert_eq!(
            cell_from_json(&serde_json::json!(1.5), ColumnKind::Float),
            CellValue::F64(1.5)
        );
        assert_eq!(
            cell_from_json(&serde_json::json!("nan"), ColumnKind::Float),
            CellValue::String("nan".into())
        );
        assert_eq!(
            cell_from_json(&serde_json::json!("-inf"), ColumnKind::Float),
            CellValue::String("-inf".into())
        );
        // Decimals arrive quoted and stay exact.
        assert_eq!(
            cell_from_json(&serde_json::json!("123456789.123456789"), ColumnKind::Text),
            CellValue::String("123456789.123456789".into())
        );
        // A decimal the server did not quote is still rendered as text.
        assert_eq!(
            cell_from_json(&serde_json::json!(1.5), ColumnKind::Text),
            CellValue::String("1.5".into())
        );
        assert_eq!(
            cell_from_json(&serde_json::json!(true), ColumnKind::Bool),
            CellValue::Bool(true)
        );
        assert_eq!(
            cell_from_json(&serde_json::json!(0), ColumnKind::Bool),
            CellValue::Bool(false)
        );
        assert_eq!(
            cell_from_json(&Value::Null, ColumnKind::Integer),
            CellValue::Null
        );
        assert_eq!(
            cell_from_json(&Value::Null, ColumnKind::Nothing),
            CellValue::Null
        );
    }

    #[test]
    fn containers_become_compact_json_text() {
        assert_eq!(
            cell_from_json(&serde_json::json!([1, 2]), ColumnKind::Json),
            CellValue::String("[1,2]".into())
        );
        assert_eq!(
            cell_from_json(&serde_json::json!({"a": 1}), ColumnKind::Json),
            CellValue::String("{\"a\":1}".into())
        );
        assert_eq!(
            cell_from_json(&serde_json::json!([1, "t"]), ColumnKind::Json),
            CellValue::String("[1,\"t\"]".into())
        );
        assert_eq!(
            cell_from_json(&serde_json::json!([1, null]), ColumnKind::Json),
            CellValue::String("[1,null]".into())
        );
    }

    #[test]
    fn parser_yields_header_then_rows_across_chunk_boundaries() {
        let mut parser = ResponseParser::new();
        let first = parser.push_bytes(b"[\"a\", \"b\", \"c\"]\n[\"UInt8\", \"Nullable(String)\", \"Array(UInt8)\"]\n[1, \"x\", [1,2]]\n[2, nu");
        assert_eq!(first.len(), 2);
        let Line::Header(columns) = &first[0] else {
            panic!("expected header, got {first:?}");
        };
        assert_eq!(columns.len(), 3);
        assert_eq!(columns[0].name, "a");
        assert_eq!(columns[0].data_type, "UInt8");
        assert!(!columns[0].nullable);
        assert_eq!(columns[1].data_type, "Nullable(String)");
        assert!(columns[1].nullable);
        assert_eq!(
            first[1],
            Line::Row(vec![
                CellValue::I64(1),
                CellValue::String("x".into()),
                CellValue::String("[1,2]".into())
            ])
        );
        let second = parser.push_bytes(b"ll, []]\r\n");
        assert_eq!(
            second,
            vec![Line::Row(vec![
                CellValue::I64(2),
                CellValue::Null,
                CellValue::String("[]".into())
            ])]
        );
        assert!(parser.finish().is_none());
    }

    #[test]
    fn parser_flushes_a_final_line_without_newline() {
        let mut parser = ResponseParser::new();
        assert!(parser.push_bytes(b"[\"n\"]\n[\"Int64\"]\n[\"5\"]").len() == 1);
        assert_eq!(parser.finish(), Some(Line::Row(vec![CellValue::I64(5)])));
    }

    #[test]
    fn parser_surfaces_a_mid_stream_exception_and_plain_text() {
        let mut parser = ResponseParser::new();
        let lines = parser.push_bytes(
            b"[\"number\"]\n[\"UInt64\"]\n[\"0\"]\nCode: 395. DB::Exception: boom: while executing (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO) (version 26.9.3.38 (official build))\n",
        );
        assert_eq!(lines.len(), 3);
        assert!(matches!(&lines[2], Line::ServerError(text) if text.contains("boom")));

        let mut plain = ResponseParser::new();
        let lines = plain.push_bytes(b"(Expression)\nExpressionTransform\n");
        assert_eq!(
            lines,
            vec![
                Line::Text("(Expression)".into()),
                Line::Text("ExpressionTransform".into())
            ]
        );
        assert!(!plain.has_header());
    }

    #[test]
    fn summary_header_yields_written_rows() {
        assert_eq!(
            written_rows_from_summary(
                r#"{"read_rows":"1234","read_bytes":"9872","written_rows":"1234","written_bytes":"4936","total_rows_to_read":"1234","result_rows":"1234","result_bytes":"4936","elapsed_ns":"2362583","memory_usage":"6672425"}"#
            ),
            Some(1234)
        );
        assert_eq!(written_rows_from_summary(r#"{"written_rows":7}"#), Some(7));
        assert_eq!(written_rows_from_summary("not json"), None);
        assert_eq!(written_rows_from_summary("{}"), None);
    }

    #[test]
    fn script_splits_on_semicolons_outside_strings_and_comments() {
        let script = "-- leading comment\nSELECT ';' AS a; /* x; y */ INSERT INTO t VALUES ('a;b'), (`c;d`)\n;\n\n  ;\nSELECT 2 -- trailing; comment\n";
        let parts = split_script(script);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0], "SELECT ';' AS a");
        assert_eq!(parts[1], "INSERT INTO t VALUES ('a;b'), (`c;d`)");
        assert_eq!(parts[2], "SELECT 2 -- trailing; comment");
        assert!(split_script("   \n-- nothing\n").is_empty());
        assert_eq!(split_script("SELECT 1"), vec!["SELECT 1".to_string()]);
    }

    #[test]
    fn statement_classification_follows_the_leading_keyword() {
        assert!(is_query_statement("SELECT 1"));
        assert!(is_query_statement(
            "  /* c */ with x as (select 1) select * from x"
        ));
        assert!(is_query_statement("SHOW TABLES"));
        assert!(is_query_statement("EXPLAIN json = 1 SELECT 1"));
        assert!(is_query_statement("DESCRIBE TABLE t"));
        assert!(is_query_statement("EXISTS TABLE t"));
        assert!(!is_query_statement("INSERT INTO t SELECT 1"));
        assert!(!is_query_statement(
            "CREATE TABLE t (x UInt8) ENGINE = Memory"
        ));
        assert!(!is_query_statement("ALTER TABLE t DELETE WHERE 1"));
        assert!(!is_query_statement("OPTIMIZE TABLE t FINAL"));
        assert!(!is_query_statement("KILL QUERY WHERE 1"));
    }

    #[test]
    fn error_texts_and_authentication_detection() {
        assert_eq!(
            error_message(400, "Code: 62. DB::Exception: Syntax error\n"),
            "Code: 62. DB::Exception: Syntax error"
        );
        assert_eq!(error_message(502, ""), "ClickHouse returned HTTP 502");
        assert!(is_authentication_error(
            403,
            "Code: 516. DB::Exception: default: Authentication failed: password is incorrect, or there is no user with such name. (AUTHENTICATION_FAILED)"
        ));
        assert!(is_authentication_error(401, ""));
        assert!(!is_authentication_error(
            400,
            "Code: 62. DB::Exception: Syntax error"
        ));
        assert!(is_server_error("Code: 62. DB::Exception: Syntax error"));
        assert!(!is_server_error("Code: 1 is not an exception"));
    }
}
