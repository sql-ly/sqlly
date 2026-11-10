//! Import / Export wizard — CSV, JSON, Excel, or SQLLY Export into a table, script-first.
//!
//! Import: pick a CSV, JSON, Excel workbook (.xlsx/.xls), or `.sqllyexport` file,
//! choose delimiter and header handling (with automatic text-encoding detection for
//! delimited files), preview the parsed rows in a real grid, name the target table
//! (or create a new one from inferred types), and script either per-row or batched
//! `INSERT` statements (engine-agnostic, row-capped and saying so) or a server-side
//! `BULK INSERT` into a new editor tab. Nothing executes from the dialog. Export is
//! deliberately a pointer, not a duplicate pipeline: the app already exports any result
//! set via File > Save Results, so the dialog scripts the `SELECT` and says where to
//! save it.
//!
//! Desktop-only at the file boundary: the browser preview cannot read local
//! files, and the dialog says so instead of failing silently.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
#[cfg(not(target_arch = "wasm32"))]
use gpui_component::select::{Select, SelectEvent, SelectState};
use serde::Deserialize;
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::SqllyDataTable;
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::{AdminTableTarget, CsvImportPlan};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
#[cfg(not(target_arch = "wasm32"))]
use crate::import_profile::{ImportProfile, ImportProfiles, ImportScope};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogEvent, ToolQuerySet};

/// Rows shown in the preview grid.
const PREVIEW_ROWS: usize = 50;
/// Rows scripted as INSERT statements before the script truncates (visibly).
pub const INSERT_SCRIPT_ROW_CAP: usize = 1000;
/// Multi-row INSERT batch size for non-TDS engines.
const INSERT_BATCH_ROWS: usize = 500;
pub const IMPORT_FILE_BYTE_CAP: u64 = 64 * 1024 * 1024;
const SQLLY_IMPORT_COLUMN_CAP: usize = 256;
const SQLLY_IMPORT_ROW_CAP: usize = 100_000;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SqllyExportDocument {
    format: String,
    version: u32,
    source_engine: String,
    columns: Vec<SqllyExportColumn>,
    rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SqllyExportColumn {
    name: String,
    data_type: String,
    nullable: bool,
}

#[derive(Debug)]
pub struct ParsedSqllyExport {
    pub source_engine: DatabaseType,
    pub result_set: ToolQuerySet,
}

pub fn parse_sqlly_export_snapshot(text: &str) -> Result<ParsedSqllyExport, String> {
    let document: SqllyExportDocument = serde_json::from_str(text).map_err(|error| {
        crate::localization::format(
            "results.import.invalid_sqlly_export",
            &[("error", &error.to_string())],
        )
    })?;
    if document.format != "sqlly-export" || document.version != 1 {
        return Err(crate::localization::text(
            "results.import.unsupported_sqlly_export_version",
        ));
    }
    let source_engine = DatabaseType::ALL
        .into_iter()
        .find(|database_type| database_type.display_name() == document.source_engine)
        .ok_or_else(|| {
            crate::localization::format(
                "results.import.unsupported_source_engine",
                &[("engine_name", &document.source_engine)],
            )
        })?;
    if document.columns.is_empty() || document.columns.len() > SQLLY_IMPORT_COLUMN_CAP {
        return Err(crate::localization::format(
            "results.import.sqlly_export_column_cap",
            &[("column_cap", &SQLLY_IMPORT_COLUMN_CAP.to_string())],
        ));
    }
    if document.rows.len() > SQLLY_IMPORT_ROW_CAP {
        return Err(crate::localization::format(
            "results.import.sqlly_export_row_cap",
            &[("row_cap", &SQLLY_IMPORT_ROW_CAP.to_string())],
        ));
    }
    let width = document.columns.len();
    let rows = document
        .rows
        .into_iter()
        .enumerate()
        .map(|(row_index, row)| {
            if row.len() > width {
                return Err(crate::localization::format(
                    "results.import.row_cell_count_mismatch",
                    &[
                        ("row_number", &(row_index + 1).to_string()),
                        ("cell_count", &row.len().to_string()),
                        ("column_count", &width.to_string()),
                    ],
                ));
            }
            (0..width)
                .map(|column| {
                    row.get(column)
                        .map(json_cell_value)
                        .transpose()
                        .map(|value| value.unwrap_or(CellValue::Null))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ParsedSqllyExport {
        source_engine,
        result_set: ToolQuerySet {
            columns: document
                .columns
                .into_iter()
                .map(|column| ResultColumn {
                    name: column.name,
                    data_type: column.data_type,
                    nullable: column.nullable,
                })
                .collect(),
            rows,
        },
    })
}

pub fn parse_sqlly_export(text: &str) -> Result<ToolQuerySet, String> {
    parse_sqlly_export_snapshot(text).map(|snapshot| snapshot.result_set)
}

fn json_cell_value(value: &serde_json::Value) -> Result<CellValue, String> {
    match value {
        serde_json::Value::Null => Ok(CellValue::Null),
        serde_json::Value::Bool(value) => Ok(CellValue::Bool(*value)),
        serde_json::Value::Number(value) => value
            .as_i64()
            .map(CellValue::I64)
            .or_else(|| {
                value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .map(CellValue::F64)
            })
            .ok_or_else(|| crate::localization::text("results.import.number_out_of_range")),
        serde_json::Value::String(value) => Ok(CellValue::String(value.clone())),
        serde_json::Value::Object(value) if value.len() == 1 && value.contains_key("$bytes") => {
            let hex = value["$bytes"].as_str().ok_or_else(|| {
                crate::localization::text("results.import.bytes_hex_string_required")
            })?;
            if hex.len() % 2 != 0 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(crate::localization::text(
                    "results.import.bytes_even_hex_digits_required",
                ));
            }
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|index| {
                    u8::from_str_radix(&hex[index..index + 2], 16)
                        .map_err(|error| error.to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(CellValue::Bytes(bytes))
        }
        _ => Err(crate::localization::text(
            "results.import.invalid_sqlly_export_cell",
        )),
    }
}

/// Minimal quote-aware CSV parser: handles quoted fields, embedded
/// delimiters, doubled quotes, and CRLF. Deliberately not a full RFC engine —
/// preview/script generation for hand-shaped files, with BULK INSERT as the
/// heavy-duty path.
/// Convert a JSON document into the same rows-of-strings shape the CSV import
/// pipeline consumes (type inference, preview, and script generation all run on
/// `Vec<Vec<String>>`). Accepts a top-level array of objects, or an object
/// wrapping such an array (e.g. `{"data": [...]}`). The first row is the header
/// (the union of keys in first-seen order); each value is stringified, with
/// nested objects/arrays kept as compact JSON and null/missing rendered blank.
pub fn parse_json_rows(text: &str) -> Result<Vec<Vec<String>>, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|error| {
        crate::localization::format(
            "results.import.invalid_json",
            &[("error", &error.to_string())],
        )
    })?;
    let array = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(map) => map
            .into_iter()
            .find_map(|(_, v)| match v {
                serde_json::Value::Array(items) => Some(items),
                _ => None,
            })
            .ok_or_else(|| crate::localization::text("results.import.json_array_required"))?,
        _ => {
            return Err(crate::localization::text(
                "results.import.json_array_required",
            ));
        }
    };
    if array.is_empty() {
        return Ok(Vec::new());
    }

    let mut columns: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in &array {
        let serde_json::Value::Object(map) = item else {
            return Err(crate::localization::text(
                "results.import.json_elements_must_be_objects",
            ));
        };
        for key in map.keys() {
            if seen.insert(key.clone()) {
                columns.push(key.clone());
            }
        }
    }

    let mut rows = Vec::with_capacity(array.len() + 1);
    rows.push(columns.clone());
    for item in &array {
        let serde_json::Value::Object(map) = item else {
            continue;
        };
        rows.push(
            columns
                .iter()
                .map(|column| json_cell_to_string(map.get(column)))
                .collect(),
        );
    }
    Ok(rows)
}

/// Stringify a JSON value for a table cell: strings verbatim, numbers/bools via
/// their text form, null/missing as blank, and nested structures as compact
/// JSON so a structured column still round-trips into a text/JSON column.
fn json_cell_to_string(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(other) => other.to_string(),
    }
}

pub fn parse_csv(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && field.is_empty() {
            in_quotes = true;
        } else if c == delimiter {
            row.push(std::mem::take(&mut field));
        } else if c == '\n' {
            if field.ends_with('\r') {
                field.pop();
            }
            row.push(std::mem::take(&mut field));
            if !(row.len() == 1 && row[0].is_empty()) {
                rows.push(std::mem::take(&mut row));
            } else {
                row.clear();
            }
        } else {
            field.push(c);
        }
    }
    if !field.is_empty() || !row.is_empty() {
        if field.ends_with('\r') {
            field.pop();
        }
        row.push(field);
        if !(row.len() == 1 && row[0].is_empty()) {
            rows.push(row);
        }
    }
    rows
}

/// Column names for the parsed data: the header row when present, else
/// `Column1..N` sized to the widest row.
pub fn column_names(rows: &[Vec<String>], has_header: bool) -> Vec<String> {
    if has_header && let Some(header) = rows.first() {
        return header
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let t = h.trim();
                if t.is_empty() {
                    crate::localization::format(
                        "results.import.default_column_name",
                        &[("column_number", &(i + 1).to_string())],
                    )
                } else {
                    t.to_string()
                }
            })
            .collect();
    }
    let width = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    (1..=width)
        .map(|i| {
            crate::localization::format(
                "results.import.default_column_name",
                &[("column_number", &i.to_string())],
            )
        })
        .collect()
}

/// Positional CSV/export mapping. A blank specification keeps every source
/// column; otherwise comma-separated target names rename columns in source
/// order and `-` skips a source column.
pub fn column_mapping(
    source: &[String],
    specification: &str,
) -> Result<Vec<(usize, String)>, String> {
    if specification.trim().is_empty() {
        return Ok(source.iter().cloned().enumerate().collect());
    }
    let targets = specification.split(',').map(str::trim).collect::<Vec<_>>();
    if targets.len() != source.len() {
        return Err(crate::localization::format(
            "results.import.mapping_count_mismatch",
            &[
                ("entry_count", &targets.len().to_string()),
                ("column_count", &source.len().to_string()),
            ],
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut mapping = Vec::new();
    for (index, target) in targets.into_iter().enumerate() {
        if target == "-" {
            continue;
        }
        if target.is_empty() {
            return Err(crate::localization::format(
                "results.import.mapping_missing_target",
                &[("column_number", &(index + 1).to_string())],
            ));
        }
        if !seen.insert(target.to_ascii_lowercase()) {
            return Err(crate::localization::format(
                "results.import.mapping_duplicate_target",
                &[("target_name", &target)],
            ));
        }
        mapping.push((index, target.to_string()));
    }
    if mapping.is_empty() {
        return Err(crate::localization::text(
            "results.import.mapping_all_columns_skipped",
        ));
    }
    Ok(mapping)
}

/// Per-row INSERT script for the parsed data (after the header is skipped).
/// Values use the selected engine's string literal form; type coercion is the
/// server's job on this path. Output is row-capped and labels that cap.
#[cfg(test)]
pub fn insert_script(
    schema: &str,
    table: &str,
    columns: &[String],
    data_rows: &[Vec<String>],
) -> String {
    insert_script_for(
        DatabaseType::SqlServer,
        schema,
        table,
        columns,
        data_rows,
        "",
    )
}

/// Retained for tests as the untyped baseline; the live Import path now
/// scripts typed literals via [`typed_csv_rows`] + [`typed_insert_script_for`].
#[cfg(test)]
pub fn insert_script_for(
    database_type: DatabaseType,
    schema: &str,
    table: &str,
    columns: &[String],
    data_rows: &[Vec<String>],
    null_token: &str,
) -> String {
    let columns = columns
        .iter()
        .map(|name| ResultColumn {
            name: name.clone(),
            data_type: "text".to_string(),
            nullable: true,
        })
        .collect::<Vec<_>>();
    let rows = data_rows
        .iter()
        .take(INSERT_SCRIPT_ROW_CAP)
        .map(|row| {
            (0..columns.len())
                .map(|index| csv_cell_value(row.get(index), null_token))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let script = typed_insert_script_for(database_type, schema, table, &columns, &rows, "file");
    if data_rows.len() <= INSERT_SCRIPT_ROW_CAP {
        return script;
    }
    let mut notice = format!(
        "-- NOTE: file has {} data rows; this reviewed script contains the first {}.\n",
        data_rows.len(),
        INSERT_SCRIPT_ROW_CAP
    );
    if database_type.connection_kind() == sqlly_lang::sqlmodel::ConnectionKind::Tds {
        notice.push_str("-- For the full file use the BULK INSERT option instead.\n");
    }
    notice.push_str(&script);
    notice
}

fn csv_cell_value(value: Option<&String>, null_token: &str) -> CellValue {
    let Some(value) = value.map(|value| value.trim()) else {
        return CellValue::Null;
    };
    if value.is_empty() || (!null_token.is_empty() && value == null_token) {
        CellValue::Null
    } else {
        CellValue::String(value.to_string())
    }
}

/// The value types the CSV importer can infer for a column or be told to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsvType {
    Text,
    Integer,
    Decimal,
    Boolean,
    Date,
    DateTime,
    Uuid,
}

impl CsvType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Integer => "int",
            Self::Decimal => "decimal",
            Self::Boolean => "bool",
            Self::Date => "date",
            Self::DateTime => "datetime",
            Self::Uuid => "uuid",
        }
    }

    /// Parse one override token; `auto` (or blank) means "keep the inferred
    /// type" and returns `Ok(None)`.
    pub fn parse(token: &str) -> Result<Option<Self>, String> {
        Ok(Some(match token.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => return Ok(None),
            "text" | "string" | "str" => Self::Text,
            "int" | "integer" | "bigint" => Self::Integer,
            "decimal" | "number" | "numeric" | "float" | "double" => Self::Decimal,
            "bool" | "boolean" | "bit" => Self::Boolean,
            "date" => Self::Date,
            "datetime" | "timestamp" => Self::DateTime,
            "uuid" | "guid" => Self::Uuid,
            other => {
                return Err(crate::localization::format(
                    "results.import.unknown_column_type",
                    &[("type_name", other)],
                ));
            }
        }))
    }

    fn accepts(self, value: &str) -> bool {
        match self {
            Self::Text => true,
            Self::Integer => value.parse::<i64>().is_ok(),
            Self::Decimal => {
                value.parse::<f64>().is_ok_and(f64::is_finite)
                    && value.chars().any(|c| c.is_ascii_digit())
                    && value
                        .chars()
                        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | '.' | 'e' | 'E'))
            }
            Self::Boolean => matches!(
                value.to_ascii_lowercase().as_str(),
                "true" | "false" | "yes" | "no"
            ),
            Self::Date => is_iso_date(value),
            Self::DateTime => is_iso_datetime(value),
            Self::Uuid => is_uuid(value),
        }
    }
}

fn all_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_iso_date(value: &str) -> bool {
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return false;
    }
    if !parts.iter().all(|part| all_digits(part)) {
        return false;
    }
    let month: u32 = parts[1].parse().unwrap_or(0);
    let day: u32 = parts[2].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

/// A date with an optional `T`/space-separated `HH:MM[:SS[.fraction]]` time.
/// A bare date also qualifies, so a column mixing dates and timestamps
/// infers as datetime rather than falling all the way back to text.
fn is_iso_datetime(value: &str) -> bool {
    let Some(split) = value.find(['T', ' ']) else {
        return is_iso_date(value);
    };
    let (date, time) = value.split_at(split);
    if !is_iso_date(date) {
        return false;
    }
    let time = &time[1..];
    let time = time.strip_suffix('Z').unwrap_or(time);
    let (time, fraction) = match time.split_once('.') {
        Some((time, fraction)) => (time, Some(fraction)),
        None => (time, None),
    };
    if fraction.is_some_and(|fraction| !all_digits(fraction)) {
        return false;
    }
    let parts: Vec<&str> = time.split(':').collect();
    if !(parts.len() == 2 || parts.len() == 3) || !parts.iter().all(|part| part.len() == 2) {
        return false;
    }
    if !parts.iter().all(|part| all_digits(part)) {
        return false;
    }
    let hour: u32 = parts[0].parse().unwrap_or(99);
    let minute: u32 = parts[1].parse().unwrap_or(99);
    let second: u32 = parts.get(2).map(|s| s.parse().unwrap_or(99)).unwrap_or(0);
    hour < 24 && minute < 60 && second < 60
}

fn is_uuid(value: &str) -> bool {
    let segments: Vec<&str> = value.split('-').collect();
    segments.len() == 5
        && [8usize, 4, 4, 4, 12]
            .iter()
            .zip(&segments)
            .all(|(len, segment)| {
                segment.len() == *len && segment.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
}

/// How many non-null values inference examines per column.
const TYPE_INFERENCE_SAMPLE: usize = 1_000;

/// Infer one type per source column from a sample of the data: the most
/// specific type every sampled non-null value satisfies, falling back to
/// text. A column with no non-null samples stays text.
pub fn infer_csv_types(
    data_rows: &[Vec<String>],
    column_count: usize,
    null_token: &str,
) -> Vec<CsvType> {
    const CANDIDATES: [CsvType; 6] = [
        CsvType::Boolean,
        CsvType::Integer,
        CsvType::Decimal,
        CsvType::Date,
        CsvType::DateTime,
        CsvType::Uuid,
    ];
    (0..column_count)
        .map(|index| {
            let mut candidates: Vec<CsvType> = CANDIDATES.to_vec();
            let mut saw_value = false;
            for row in data_rows {
                if candidates.is_empty() {
                    break;
                }
                let cell = row.get(index).map(|value| value.trim()).unwrap_or("");
                if cell.is_empty() || (!null_token.is_empty() && cell == null_token) {
                    continue;
                }
                saw_value = true;
                candidates.retain(|candidate| candidate.accepts(cell));
                if data_rows.len() > TYPE_INFERENCE_SAMPLE {
                    // Bounded sample; enough to decide without a full scan.
                    break;
                }
            }
            if !saw_value {
                return CsvType::Text;
            }
            candidates.first().copied().unwrap_or(CsvType::Text)
        })
        .collect()
}

/// Parse the comma-separated per-source-column type override spec. Blank spec
/// = all inferred; otherwise one entry per source column, `auto` keeping the
/// inferred type.
pub fn parse_type_overrides(
    specification: &str,
    source_count: usize,
) -> Result<Vec<Option<CsvType>>, String> {
    if specification.trim().is_empty() {
        return Ok(vec![None; source_count]);
    }
    let tokens: Vec<&str> = specification.split(',').collect();
    if tokens.len() != source_count {
        return Err(crate::localization::format(
            "results.import.type_count_mismatch",
            &[
                ("entry_count", &tokens.len().to_string()),
                ("column_count", &source_count.to_string()),
            ],
        ));
    }
    tokens.into_iter().map(CsvType::parse).collect()
}

/// A data row excluded from the script because a cell failed its column type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RejectedRow {
    /// 1-based data-row number (after the header, when there is one).
    pub row_number: usize,
    pub column: String,
    pub value: String,
    pub reason: String,
}

/// Convert mapped CSV rows into typed cells, rejecting whole rows whose cells
/// fail their column type. `types` aligns with `mapping` entries (one per
/// mapped source column).
pub fn typed_csv_rows(
    mapping: &[(usize, String)],
    types: &[CsvType],
    data_rows: &[Vec<String>],
    null_token: &str,
) -> (Vec<Vec<CellValue>>, Vec<RejectedRow>) {
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    'rows: for (row_index, row) in data_rows.iter().enumerate() {
        let mut cells = Vec::with_capacity(mapping.len());
        for ((source_index, target_name), ty) in mapping.iter().zip(types) {
            let raw = row
                .get(*source_index)
                .map(|value| value.trim())
                .unwrap_or("");
            if raw.is_empty() || (!null_token.is_empty() && raw == null_token) {
                cells.push(CellValue::Null);
                continue;
            }
            if !ty.accepts(raw) {
                rejected.push(RejectedRow {
                    row_number: row_index + 1,
                    column: target_name.clone(),
                    value: raw.chars().take(48).collect(),
                    reason: crate::localization::format(
                        "results.import.invalid_cell_type",
                        &[("expected_type", ty.label())],
                    ),
                });
                continue 'rows;
            }
            cells.push(match ty {
                CsvType::Integer => CellValue::I64(raw.parse().unwrap_or_default()),
                CsvType::Decimal => CellValue::F64(raw.parse().unwrap_or_default()),
                CsvType::Boolean => {
                    CellValue::Bool(matches!(raw.to_ascii_lowercase().as_str(), "true" | "yes"))
                }
                CsvType::Text | CsvType::Date | CsvType::DateTime | CsvType::Uuid => {
                    CellValue::String(raw.to_string())
                }
            });
        }
        accepted.push(cells);
    }
    (accepted, rejected)
}

/// Whether the import targets an existing table or creates a new one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImportTarget {
    ExistingTable,
    CreateNewTable,
}

/// When inferred and override types disagree, pick the wider type for CREATE
/// TABLE DDL. `infer_csv_types` already yields the most specific type all
/// samples accept; `widen` is used when a per-column override is narrower than
/// the data supports — it never makes a column narrower.
pub fn widen(a: CsvType, b: CsvType) -> CsvType {
    use CsvType::*;
    if a == b {
        return a;
    }
    match (a, b) {
        (Integer, Decimal) | (Decimal, Integer) => Decimal,
        (Date, DateTime) | (DateTime, Date) => DateTime,
        (Boolean, Integer) | (Integer, Boolean) => Text,
        _ => Text,
    }
}

fn nvarchar_length(max_len: usize) -> usize {
    if max_len <= 50 {
        50
    } else if max_len <= 255 {
        255
    } else if max_len <= 1000 {
        1000
    } else {
        usize::MAX
    }
}

/// Engine column type for an inferred [`CsvType`].
pub fn ddl_type_for(database_type: DatabaseType, ty: CsvType, max_len: usize) -> String {
    use sqlly_lang::sqlmodel::ConnectionKind::*;
    match database_type.connection_kind() {
        Tds => match ty {
            CsvType::Integer => "BIGINT".to_string(),
            CsvType::Decimal => "DECIMAL(38, 10)".to_string(),
            CsvType::Boolean => "BIT".to_string(),
            CsvType::Date => "DATE".to_string(),
            CsvType::DateTime => "DATETIME2".to_string(),
            CsvType::Uuid => "UNIQUEIDENTIFIER".to_string(),
            CsvType::Text => {
                let n = nvarchar_length(max_len);
                if n == usize::MAX {
                    "NVARCHAR(MAX)".to_string()
                } else {
                    format!("NVARCHAR({n})")
                }
            }
        },
        PostgresWire => match ty {
            CsvType::Integer => "BIGINT".to_string(),
            CsvType::Decimal => "NUMERIC".to_string(),
            CsvType::Boolean => "BOOLEAN".to_string(),
            CsvType::Date => "DATE".to_string(),
            CsvType::DateTime => "TIMESTAMP".to_string(),
            CsvType::Uuid => "UUID".to_string(),
            CsvType::Text => "TEXT".to_string(),
        },
        MySqlWire => match ty {
            CsvType::Integer => "BIGINT".to_string(),
            CsvType::Decimal => "DECIMAL(38, 10)".to_string(),
            CsvType::Boolean => "TINYINT(1)".to_string(),
            CsvType::Date => "DATE".to_string(),
            CsvType::DateTime => "DATETIME".to_string(),
            CsvType::Uuid => "CHAR(36)".to_string(),
            CsvType::Text => {
                let n = nvarchar_length(max_len);
                if n == usize::MAX {
                    "TEXT".to_string()
                } else {
                    format!("VARCHAR({n})")
                }
            }
        },
        SqliteFile | HranaHttp => match ty {
            CsvType::Integer => "INTEGER".to_string(),
            CsvType::Decimal => "REAL".to_string(),
            CsvType::Boolean => "INTEGER".to_string(),
            CsvType::Date | CsvType::DateTime | CsvType::Uuid | CsvType::Text => "TEXT".to_string(),
        },
        _ => "TEXT".to_string(),
    }
}

fn dedupe_column_names(names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashMap::<String, usize>::new();
    names
        .into_iter()
        .map(|name| {
            let key = name.to_ascii_lowercase();
            let entry = seen.entry(key).or_insert(0);
            *entry += 1;
            if *entry == 1 {
                name
            } else {
                format!("{name}_{entry}")
            }
        })
        .collect()
}

pub fn create_table_script(
    database_type: DatabaseType,
    schema: &str,
    table: &str,
    columns: &[(String, CsvType, usize)],
) -> String {
    let names = dedupe_column_names(columns.iter().map(|(name, _, _)| name.clone()).collect());
    let target = qualified_target(database_type, schema, table);
    let body = columns
        .iter()
        .zip(&names)
        .map(|((_, ty, max_len), name)| {
            format!(
                "    {} {} NULL",
                database_type.quote_identifier(name),
                ddl_type_for(database_type, *ty, *max_len)
            )
        })
        .collect::<Vec<_>>()
        .join(",\n");
    format!("CREATE TABLE {target} (\n{body}\n);\n\n")
}

fn qualified_target(database_type: DatabaseType, schema: &str, table: &str) -> String {
    let table = database_type.quote_identifier(table);
    if schema.trim().is_empty() || database_type == DatabaseType::Sqlite {
        table
    } else {
        format!("{}.{}", database_type.quote_identifier(schema), table)
    }
}

/// One entry in the profile picker, keyed by the profile's id so picking a
/// label can never resolve to a same-named profile for another table.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
struct ProfileChoice {
    label: gpui::SharedString,
    id: uuid::Uuid,
}

#[cfg(not(target_arch = "wasm32"))]
impl gpui_component::searchable_list::SearchableListItem for ProfileChoice {
    type Value = uuid::Uuid;

    fn title(&self) -> gpui::SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.id
    }
}

#[cfg(test)]
pub fn typed_insert_script(
    schema: &str,
    table: &str,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
) -> String {
    typed_insert_script_for(
        DatabaseType::SqlServer,
        schema,
        table,
        columns,
        rows,
        "export",
    )
}

pub fn typed_insert_script_for(
    database_type: DatabaseType,
    schema: &str,
    table: &str,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    source_label: &str,
) -> String {
    let target = qualified_target(database_type, schema, table);
    let column_list = columns
        .iter()
        .map(|column| database_type.quote_identifier(&column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = String::new();
    if rows.len() > INSERT_SCRIPT_ROW_CAP {
        sql.push_str(&format!(
            "-- NOTE: {source_label} has {} data rows; this reviewed script contains the first {}.\n",
            rows.len(),
            INSERT_SCRIPT_ROW_CAP
        ));
        if source_label == "file"
            && database_type.connection_kind() == sqlly_lang::sqlmodel::ConnectionKind::Tds
        {
            sql.push_str("-- For the full file use the BULK INSERT option instead.\n");
        }
    }
    let capped: Vec<_> = rows.iter().take(INSERT_SCRIPT_ROW_CAP).collect();
    let batched = database_type.connection_kind() != sqlly_lang::sqlmodel::ConnectionKind::Tds;
    if batched {
        for chunk in capped.chunks(INSERT_BATCH_ROWS) {
            let values = chunk
                .iter()
                .map(|row| {
                    let literals = (0..columns.len())
                        .map(|index| {
                            crate::table_actions::sql_literal_for(
                                database_type,
                                row.get(index).unwrap_or(&CellValue::Null),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("({literals})")
                })
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(
                "INSERT INTO {target} ({column_list}) VALUES {values};\n"
            ));
        }
    } else {
        for row in capped {
            let values = (0..columns.len())
                .map(|index| {
                    crate::table_actions::sql_literal_for(
                        database_type,
                        row.get(index).unwrap_or(&CellValue::Null),
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(
                "INSERT INTO {target} ({column_list}) VALUES ({values});\n"
            ));
        }
    }
    sql
}

/// Which script the Import flow generates.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImportScriptKind {
    /// Row-by-row INSERTs built from the parsed file (engine-agnostic).
    InsertStatements,
    /// Server-side BULK INSERT referencing the file path (SQL Server reads
    /// the file — the path must be visible to the server).
    BulkInsert,
}

#[cfg(not(target_arch = "wasm32"))]
enum LoadedImportFile {
    Csv {
        text: Arc<str>,
        rows: Vec<Vec<String>>,
        encoding_label: Option<&'static str>,
        lossy: bool,
    },
    /// JSON import: rows already carry a header row, so the dialog forces
    /// "first row is header" on.
    Json {
        text: Arc<str>,
        rows: Vec<Vec<String>>,
    },
    Sheet {
        rows: Vec<Vec<String>>,
        sheet_names: Vec<String>,
        sheet: String,
        error_cells: usize,
    },
    Sqlly(ToolQuerySet),
}

#[cfg(not(target_arch = "wasm32"))]
fn read_import_file(
    path: &std::path::Path,
    delimiter: char,
    sheet: Option<&str>,
) -> Result<LoadedImportFile, String> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        crate::localization::format(
            "results.import.file_read_failed",
            &[("error", &error.to_string())],
        )
    })?;
    if metadata.len() > IMPORT_FILE_BYTE_CAP {
        return Err(crate::localization::text("results.import.file_size_limit"));
    }
    let bytes = std::fs::read(path).map_err(|error| {
        crate::localization::format(
            "results.import.file_read_failed",
            &[("error", &error.to_string())],
        )
    })?;
    let extension = path
        .extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "xlsx" | "xlsm" | "xlsb" | "xls" => {
            let info = crate::import_export_sheets::open_workbook_sheets(path)?;
            let sheet_name = sheet
                .map(str::to_string)
                .or_else(|| info.sheet_names.first().cloned())
                .ok_or_else(|| crate::localization::text("results.import.workbook_no_sheets"))?;
            let read = crate::import_export_sheets::read_sheet_rows(path, &sheet_name)?;
            Ok(LoadedImportFile::Sheet {
                rows: read.rows,
                sheet_names: info.sheet_names,
                sheet: sheet_name,
                error_cells: read.error_cells,
            })
        }
        "sqllyexport" => {
            let decoded = crate::import_encoding::decode_import_bytes(&bytes);
            parse_sqlly_export(&decoded.text).map(LoadedImportFile::Sqlly)
        }
        "json" => {
            let decoded = crate::import_encoding::decode_import_bytes(&bytes);
            let rows = parse_json_rows(&decoded.text)?;
            Ok(LoadedImportFile::Json {
                text: decoded.text.into(),
                rows,
            })
        }
        _ => {
            let decoded = crate::import_encoding::decode_import_bytes(&bytes);
            let encoding_label = if decoded.encoding_label == "UTF-8" && !decoded.had_bom {
                None
            } else {
                Some(decoded.encoding_label)
            };
            let text: Arc<str> = decoded.text.into();
            let rows = parse_csv(&text, delimiter);
            Ok(LoadedImportFile::Csv {
                text,
                rows,
                encoding_label,
                lossy: decoded.lossy,
            })
        }
    }
}

impl EventEmitter<ToolDialogEvent> for ImportExportDialog {}

pub struct ImportExportDialog {
    database_type: DatabaseType,
    file_path: Option<std::path::PathBuf>,
    source_label: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    raw_text: Option<Arc<str>>,
    raw_rows: Vec<Vec<String>>,
    typed_import: Option<ToolQuerySet>,
    parse_error: Option<String>,
    delimiter: char,
    /// True while the loaded file is JSON: rows came from `parse_json_rows`,
    /// so delimiter changes must not re-parse the text as CSV.
    loaded_json: bool,
    #[cfg(not(target_arch = "wasm32"))]
    sheet_names: Vec<String>,
    #[cfg(not(target_arch = "wasm32"))]
    selected_sheet: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    sheet_select: Entity<SelectState<Vec<String>>>,
    #[cfg(not(target_arch = "wasm32"))]
    sheet_error_cells: usize,
    #[cfg(not(target_arch = "wasm32"))]
    encoding_label: Option<&'static str>,
    #[cfg(not(target_arch = "wasm32"))]
    encoding_lossy: bool,
    has_header: bool,
    import_target: ImportTarget,
    script_kind: ImportScriptKind,
    schema_input: Entity<InputState>,
    table_input: Entity<InputState>,
    mapping_input: Entity<InputState>,
    null_token_input: Entity<InputState>,
    types_input: Entity<InputState>,
    schema: String,
    table: String,
    mapping_spec: String,
    null_token: String,
    types_spec: String,
    /// Rows the typed conversion would reject, from a bounded scan for the
    /// dialog display; the script-time conversion re-checks every row.
    rejected: Vec<RejectedRow>,
    preview_grid: Option<Entity<SqllyDataTable>>,
    /// Grid rebuild wanted (file/delimiter/header changed).
    preview_dirty: bool,
    /// Table-name prefill (file picked) applied at render, which has the
    /// window `set_value` needs.
    pending_table_prefill: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    pending_sheet_setup: Option<(Vec<String>, String)>,
    load_generation: u64,
    loading: bool,
    #[cfg(not(target_arch = "wasm32"))]
    scope_connection_id: uuid::Uuid,
    #[cfg(not(target_arch = "wasm32"))]
    scope_database: String,
    #[cfg(not(target_arch = "wasm32"))]
    profiles: Vec<ImportProfile>,
    #[cfg(not(target_arch = "wasm32"))]
    profile_select: Entity<SelectState<Vec<ProfileChoice>>>,
    #[cfg(not(target_arch = "wasm32"))]
    profile_name_input: Entity<InputState>,
    #[cfg(not(target_arch = "wasm32"))]
    selected_profile: Option<uuid::Uuid>,
    /// Profile waiting to be applied at render (which has the window that
    /// input `set_value` needs) — manual picks and default auto-applies.
    #[cfg(not(target_arch = "wasm32"))]
    pending_profile_apply: Option<ImportProfile>,
    /// Drift/limitation warning from the last apply, shown under the row.
    #[cfg(not(target_arch = "wasm32"))]
    profile_notice: Option<String>,
    /// The default profile auto-applied to the current source (profile id,
    /// `load_generation`), so re-checks on every schema/table edit apply a
    /// default once per loaded source instead of on each keystroke.
    #[cfg(not(target_arch = "wasm32"))]
    auto_applied: Option<(uuid::Uuid, u64)>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl ImportExportDialog {
    pub fn new_for_database(
        database_type: DatabaseType,
        connection: Option<&sqlly_core::ConnectionProfile>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let default_schema = match database_type {
            DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase => "dbo",
            DatabaseType::Postgres => "public",
            _ => "",
        };
        let schema_input = cx.new(|cx| InputState::new(window, cx).default_value(default_schema));
        let table_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.import.target_table_placeholder",
            ))
        });
        let mapping_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.import.mapping_placeholder",
            ))
        });
        let null_token_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.import.null_token_placeholder",
            ))
        });
        let types_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.import.types_placeholder",
            ))
        });
        #[cfg(not(target_arch = "wasm32"))]
        let sheet_select = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        #[cfg(not(target_arch = "wasm32"))]
        let profile_select =
            cx.new(|cx| SelectState::new(Vec::<ProfileChoice>::new(), None, window, cx));
        #[cfg(not(target_arch = "wasm32"))]
        let profile_name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.import.profile_name_placeholder",
            ))
        });
        #[cfg(not(target_arch = "wasm32"))]
        let scope_connection_id = connection.map(|p| p.id.0).unwrap_or_else(uuid::Uuid::nil);
        #[cfg(not(target_arch = "wasm32"))]
        let scope_database = connection
            .and_then(|p| p.database.clone())
            .unwrap_or_default();
        let mut subscriptions = vec![
            cx.subscribe_in(
                &schema_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.schema = state.read(cx).value().to_string();
                        // A new target may have its own default profile.
                        #[cfg(not(target_arch = "wasm32"))]
                        this.maybe_apply_default_profile(cx);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &table_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.table = state.read(cx).value().to_string();
                        // A new target may have its own default profile.
                        #[cfg(not(target_arch = "wasm32"))]
                        this.maybe_apply_default_profile(cx);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &mapping_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.mapping_spec = state.read(cx).value().to_string();
                        this.preview_dirty = true;
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &null_token_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.null_token = state.read(cx).value().trim().to_string();
                        this.preview_dirty = true;
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &types_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.types_spec = state.read(cx).value().to_string();
                        this.preview_dirty = true;
                        cx.notify();
                    }
                },
            ),
        ];
        #[cfg(not(target_arch = "wasm32"))]
        subscriptions.push(cx.subscribe_in(
            &sheet_select,
            window,
            |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                let Some(value) = value.clone() else {
                    return;
                };
                if this.selected_sheet.as_deref() != Some(value.as_str()) {
                    this.selected_sheet = Some(value);
                    this.reload_sheet(cx);
                }
            },
        ));
        #[cfg(not(target_arch = "wasm32"))]
        subscriptions.push(cx.subscribe_in(
            &profile_select,
            window,
            |this: &mut Self, _, event: &SelectEvent<Vec<ProfileChoice>>, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                let Some(id) = *value else {
                    return;
                };
                if let Some(profile) = this.profiles.iter().find(|p| p.id == id).cloned() {
                    this.selected_profile = Some(profile.id);
                    this.pending_profile_apply = Some(profile);
                    cx.notify();
                }
            },
        ));
        #[allow(unused_mut)]
        let mut this = Self {
            database_type,
            file_path: None,
            source_label: None,
            #[cfg(not(target_arch = "wasm32"))]
            raw_text: None,
            raw_rows: Vec::new(),
            typed_import: None,
            parse_error: None,
            delimiter: ',',
            loaded_json: false,
            #[cfg(not(target_arch = "wasm32"))]
            sheet_names: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            selected_sheet: None,
            #[cfg(not(target_arch = "wasm32"))]
            sheet_select,
            #[cfg(not(target_arch = "wasm32"))]
            sheet_error_cells: 0,
            #[cfg(not(target_arch = "wasm32"))]
            encoding_label: None,
            #[cfg(not(target_arch = "wasm32"))]
            encoding_lossy: false,
            has_header: true,
            import_target: ImportTarget::ExistingTable,
            script_kind: ImportScriptKind::InsertStatements,
            schema_input,
            table_input,
            mapping_input,
            null_token_input,
            types_input,
            schema: default_schema.to_string(),
            table: String::new(),
            mapping_spec: String::new(),
            null_token: String::new(),
            types_spec: String::new(),
            rejected: Vec::new(),
            preview_grid: None,
            preview_dirty: false,
            pending_table_prefill: None,
            #[cfg(not(target_arch = "wasm32"))]
            pending_sheet_setup: None,
            load_generation: 0,
            loading: false,
            #[cfg(not(target_arch = "wasm32"))]
            scope_connection_id,
            #[cfg(not(target_arch = "wasm32"))]
            scope_database,
            #[cfg(not(target_arch = "wasm32"))]
            profiles: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            profile_select,
            #[cfg(not(target_arch = "wasm32"))]
            profile_name_input,
            #[cfg(not(target_arch = "wasm32"))]
            selected_profile: None,
            #[cfg(not(target_arch = "wasm32"))]
            pending_profile_apply: None,
            #[cfg(not(target_arch = "wasm32"))]
            profile_notice: None,
            #[cfg(not(target_arch = "wasm32"))]
            auto_applied: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(820.0), Theme::scaled(560.0)),
            _subscriptions: subscriptions,
        };
        // List this database's saved profiles up front so one can be picked
        // before anything is saved in this session.
        #[cfg(not(target_arch = "wasm32"))]
        this.refresh_profiles(window, cx);
        this
    }

    /// Open the wizard with clipboard text already parsed. `schema` prefills
    /// the schema input when the caller knows it (explorer database node).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new_from_clipboard(
        database_type: DatabaseType,
        text: String,
        schema: Option<String>,
        connection: Option<&sqlly_core::ConnectionProfile>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let mut this = Self::new_for_database(database_type, connection, window, cx);
        if let Some(schema) = schema.filter(|s| !s.is_empty()) {
            this.schema = schema.clone();
            this.schema_input
                .update(cx, |state, cx| state.set_value(schema, window, cx));
        }
        this.apply_clipboard_text(text, cx);
        this
    }

    /// Install pasted text as a CSV-like source: detect delimiter, guess the
    /// header row, default to "Create new table" with a timestamped name.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_clipboard_text(&mut self, text: String, cx: &mut gpui::Context<Self>) {
        let text = crate::import_clipboard::normalise(&text);
        if text.trim().is_empty() {
            self.parse_error = Some(crate::localization::text("results.import.clipboard_empty"));
            cx.notify();
            return;
        }
        let delimiter = crate::import_clipboard::detect_delimiter(&text);
        let rows = parse_csv(&text, delimiter);
        self.load_generation = self.load_generation.wrapping_add(1);
        self.loading = false;
        self.file_path = None;
        self.source_label = None;
        self.typed_import = None;
        self.loaded_json = false;
        self.sheet_names.clear();
        self.selected_sheet = None;
        self.sheet_error_cells = 0;
        self.encoding_label = None;
        self.encoding_lossy = false;
        self.pending_sheet_setup = None;
        self.delimiter = delimiter;
        self.has_header = crate::import_clipboard::looks_like_header(&rows);
        self.raw_text = Some(Arc::from(text.as_str()));
        self.raw_rows = rows;
        self.parse_error = None;
        self.import_target = ImportTarget::CreateNewTable;
        self.script_kind = ImportScriptKind::InsertStatements;
        let name = crate::import_clipboard::default_table_name(chrono::Local::now().naive_local());
        self.table = name.clone();
        self.pending_table_prefill = Some(name);
        self.source_label = Some(crate::localization::format(
            "results.import.clipboard_source_label",
            &[("line_count", &self.raw_rows.len().to_string())],
        ));
        self.preview_dirty = true;
        self.maybe_apply_default_profile(cx);
        cx.notify();
    }

    fn choose_file(&mut self, cx: &mut gpui::Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Error,
                message: crate::browser_preview::unavailable(&crate::localization::text(
                    "results.import.browser_local_files_unavailable",
                ))
                .to_string(),
            });
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: None,
            });
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Ok(Ok(Some(paths))) = rx.await
                    && let Some(path) = paths.into_iter().next()
                {
                    let mut cx = async_cx.clone();
                    weak.update(&mut cx, |this: &mut Self, cx| {
                        this.begin_load_file(path, cx);
                    })
                    .ok();
                }
            })
            .detach();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn begin_load_file(&mut self, path: std::path::PathBuf, cx: &mut gpui::Context<Self>) {
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let delimiter = self.delimiter;
        self.loading = true;
        self.parse_error = None;
        self.preview_grid = None;
        self.file_path = Some(path.clone());
        cx.notify();

        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let worker_path = path.clone();
            let result = async_cx
                .background_executor()
                .spawn(async move { read_import_file(&worker_path, delimiter, None) })
                .await;
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this: &mut Self, cx| {
                this.apply_loaded_file(path, generation, result, cx);
            })
            .ok();
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn reload_sheet(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(path) = self.file_path.clone() else {
            return;
        };
        let Some(sheet) = self.selected_sheet.clone() else {
            return;
        };
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let delimiter = self.delimiter;
        self.loading = true;
        self.parse_error = None;
        self.preview_grid = None;
        cx.notify();

        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let worker_path = path.clone();
            let sheet_name = sheet.clone();
            let result = async_cx
                .background_executor()
                .spawn(async move { read_import_file(&worker_path, delimiter, Some(&sheet_name)) })
                .await;
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this: &mut Self, cx| {
                this.apply_loaded_file(path, generation, result, cx);
            })
            .ok();
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn apply_loaded_file(
        &mut self,
        path: std::path::PathBuf,
        generation: u64,
        result: Result<LoadedImportFile, String>,
        cx: &mut gpui::Context<Self>,
    ) {
        if generation != self.load_generation {
            return;
        }
        self.loading = false;
        self.raw_rows.clear();
        self.typed_import = None;
        self.raw_text = None;
        self.source_label = None;
        self.loaded_json = false;
        self.sheet_names.clear();
        self.selected_sheet = None;
        self.sheet_error_cells = 0;
        self.encoding_label = None;
        self.encoding_lossy = false;
        self.pending_sheet_setup = None;
        match result {
            Ok(LoadedImportFile::Csv {
                text,
                rows,
                encoding_label,
                lossy,
            }) => {
                self.raw_text = Some(text);
                self.raw_rows = rows;
                self.encoding_label = encoding_label;
                self.encoding_lossy = lossy;
                self.parse_error = None;
            }
            Ok(LoadedImportFile::Json { text, rows }) => {
                self.raw_text = Some(text);
                self.raw_rows = rows;
                self.loaded_json = true;
                // JSON always yields an explicit header row.
                self.has_header = true;
                self.script_kind = ImportScriptKind::InsertStatements;
                self.parse_error = None;
            }
            Ok(LoadedImportFile::Sheet {
                rows,
                sheet_names,
                sheet,
                error_cells,
            }) => {
                self.raw_rows = rows;
                self.sheet_names = sheet_names.clone();
                self.selected_sheet = Some(sheet.clone());
                self.sheet_error_cells = error_cells;
                self.pending_sheet_setup = Some((sheet_names, sheet));
                self.script_kind = ImportScriptKind::InsertStatements;
                self.parse_error = None;
            }
            Ok(LoadedImportFile::Sqlly(import)) => {
                self.typed_import = Some(import);
                self.script_kind = ImportScriptKind::InsertStatements;
                self.parse_error = None;
            }
            Err(error) => self.parse_error = Some(error),
        }
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        if self.table.is_empty() {
            self.table = stem.clone();
            self.pending_table_prefill = Some(stem);
        }
        self.file_path = Some(path);
        self.preview_dirty = true;
        self.maybe_apply_default_profile(cx);
        cx.notify();
    }

    /// Re-parse with the current delimiter (file already loaded).
    #[cfg(not(target_arch = "wasm32"))]
    fn reparse(&mut self, cx: &mut gpui::Context<Self>) {
        // JSON and Excel rows are delimiter-independent.
        if self.loaded_json || !self.sheet_names.is_empty() {
            if self.loaded_json {
                self.has_header = true;
            }
            self.preview_dirty = true;
            cx.notify();
            return;
        }
        let Some(text) = self.raw_text.clone() else {
            return;
        };
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let delimiter = self.delimiter;
        self.loading = true;
        self.parse_error = None;
        self.preview_grid = None;
        cx.notify();

        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let rows = async_cx
                .background_executor()
                .spawn(async move { parse_csv(&text, delimiter) })
                .await;
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this: &mut Self, cx| {
                if generation != this.load_generation {
                    return;
                }
                this.loading = false;
                this.raw_rows = rows;
                this.preview_dirty = true;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[cfg(target_arch = "wasm32")]
    fn reparse(&mut self, cx: &mut gpui::Context<Self>) {
        cx.notify();
    }

    fn data_rows(&self) -> &[Vec<String>] {
        if self.has_header && !self.raw_rows.is_empty() {
            &self.raw_rows[1..]
        } else {
            &self.raw_rows
        }
    }

    /// The per-mapped-column types for the loaded CSV: inferred from the
    /// data, then overridden by the Column types spec. `None` when the
    /// mapping or overrides are invalid.
    fn csv_column_types(&self, mapping: &[(usize, String)]) -> Option<Vec<CsvType>> {
        let source_count = column_names(&self.raw_rows, self.has_header).len();
        let inferred = infer_csv_types(self.data_rows(), source_count, &self.null_token);
        let overrides = parse_type_overrides(&self.types_spec, source_count).ok()?;
        Some(
            mapping
                .iter()
                .map(|(index, _)| {
                    overrides
                        .get(*index)
                        .copied()
                        .flatten()
                        .or_else(|| inferred.get(*index).copied())
                        .unwrap_or(CsvType::Text)
                })
                .collect(),
        )
    }

    fn create_table_ddl_columns(
        &self,
        mapping: &[(usize, String)],
    ) -> Option<Vec<(String, CsvType, usize)>> {
        let source_count = column_names(&self.raw_rows, self.has_header).len();
        let inferred = infer_csv_types(self.data_rows(), source_count, &self.null_token);
        let overrides = parse_type_overrides(&self.types_spec, source_count).ok()?;
        let mut max_lens = vec![0usize; source_count];
        for row in self.data_rows() {
            for (index, cell) in row.iter().enumerate().take(source_count) {
                max_lens[index] = max_lens[index].max(cell.trim().len());
            }
        }
        Some(
            mapping
                .iter()
                .map(|(index, name)| {
                    let inferred_ty = inferred.get(*index).copied().unwrap_or(CsvType::Text);
                    let ty = match overrides.get(*index).copied().flatten() {
                        Some(chosen) => widen(inferred_ty, chosen),
                        None => inferred_ty,
                    };
                    let max_len = max_lens.get(*index).copied().unwrap_or(0);
                    (name.clone(), ty, max_len)
                })
                .collect(),
        )
    }

    fn generate_script(&self) -> Option<String> {
        if self.table.trim().is_empty() || (self.raw_rows.is_empty() && self.typed_import.is_none())
        {
            return None;
        }
        if let Some(import) = &self.typed_import {
            let source_names = import
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect::<Vec<_>>();
            let mapping = column_mapping(&source_names, &self.mapping_spec).ok()?;
            let columns = mapping
                .iter()
                .map(|(index, name)| {
                    let mut column = import.columns[*index].clone();
                    column.name = name.clone();
                    column
                })
                .collect::<Vec<_>>();
            let rows = import
                .rows
                .iter()
                .map(|row| {
                    mapping
                        .iter()
                        .map(|(index, _)| row.get(*index).cloned().unwrap_or(CellValue::Null))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            return Some(typed_insert_script_for(
                self.database_type,
                &self.schema,
                &self.table,
                &columns,
                &rows,
                "export",
            ));
        }
        let source_columns = column_names(&self.raw_rows, self.has_header);
        let mapping = column_mapping(&source_columns, &self.mapping_spec).ok()?;
        let script_kind = if self.import_target == ImportTarget::CreateNewTable {
            ImportScriptKind::InsertStatements
        } else {
            self.script_kind
        };
        match script_kind {
            ImportScriptKind::InsertStatements => {
                let types = self.csv_column_types(&mapping)?;
                let (typed_rows, rejected) =
                    typed_csv_rows(&mapping, &types, self.data_rows(), &self.null_token);
                let columns = mapping
                    .iter()
                    .zip(&types)
                    .map(|((_, name), ty)| ResultColumn {
                        name: name.clone(),
                        data_type: ty.label().to_string(),
                        nullable: true,
                    })
                    .collect::<Vec<_>>();
                let mut script = if self.import_target == ImportTarget::CreateNewTable {
                    let ddl_columns = self.create_table_ddl_columns(&mapping)?;
                    create_table_script(self.database_type, &self.schema, &self.table, &ddl_columns)
                } else {
                    String::new()
                };
                script.push_str(&typed_insert_script_for(
                    self.database_type,
                    &self.schema,
                    &self.table,
                    &columns,
                    &typed_rows,
                    "file",
                ));
                if rejected.is_empty() {
                    return Some(script);
                }
                let mut notice = format!(
                    "-- NOTE: {} row(s) were rejected because a cell did not match its column type and are NOT in this script:\n",
                    rejected.len()
                );
                for reject in rejected.iter().take(10) {
                    notice.push_str(&format!(
                        "--   row {}: {} = '{}' ({})\n",
                        reject.row_number, reject.column, reject.value, reject.reason
                    ));
                }
                if rejected.len() > 10 {
                    notice.push_str(&format!("--   … and {} more\n", rejected.len() - 10));
                }
                notice.push_str(&script);
                Some(notice)
            }
            ImportScriptKind::BulkInsert => {
                if !self.mapping_spec.trim().is_empty()
                    || self.database_type.connection_kind()
                        != sqlly_lang::sqlmodel::ConnectionKind::Tds
                {
                    return None;
                }
                let path = self.file_path.as_ref()?;
                let plan = CsvImportPlan {
                    target: AdminTableTarget::new(self.schema.clone(), self.table.clone()),
                    file_path: path.to_string_lossy().to_string(),
                    has_header: self.has_header,
                    delimiter: self.delimiter,
                    batch_size: 5000,
                };
                Some(plan.bulk_insert_sql())
            }
        }
    }

    fn script_to_editor(&mut self, cx: &mut gpui::Context<Self>) {
        match self.generate_script() {
            Some(sql) => {
                cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                cx.emit(ToolDialogEvent::Close);
            }
            None => cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("results.import.file_and_target_required"),
            }),
        }
    }

    fn script_export_select(&mut self, cx: &mut gpui::Context<Self>) {
        if self.table.trim().is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("results.import.export_table_required"),
            });
            return;
        }
        let sql = format!(
            "SELECT *\nFROM {}.{}\n-- Run this, then File > Save Results… to export (CSV, JSON, \
             XLSX, …). To import back through Tools > Import / Export, save as CSV, JSON, Excel, \
             or SQLLY Export — Markdown and HTML are presentation-only.",
            crate::table_actions::escape_identifier(&self.schema),
            crate::table_actions::escape_identifier(&self.table),
        );
        cx.emit(ToolDialogEvent::ScriptToEditor { sql });
        cx.emit(ToolDialogEvent::Close);
    }

    fn rebuild_preview(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(import) = &self.typed_import {
            let source_names = import
                .columns
                .iter()
                .map(|column| column.name.clone())
                .collect::<Vec<_>>();
            let Ok(mapping) = column_mapping(&source_names, &self.mapping_spec) else {
                self.preview_grid = None;
                return;
            };
            let preview = ToolQuerySet {
                columns: mapping
                    .iter()
                    .map(|(index, name)| {
                        let mut column = import.columns[*index].clone();
                        column.name = name.clone();
                        column
                    })
                    .collect(),
                rows: import
                    .rows
                    .iter()
                    .take(PREVIEW_ROWS)
                    .map(|row| {
                        mapping
                            .iter()
                            .map(|(index, _)| row.get(*index).cloned().unwrap_or(CellValue::Null))
                            .collect()
                    })
                    .collect(),
            };
            self.preview_grid = crate::tool_dialog::build_result_grid(&preview, cx);
            return;
        }
        let source_columns = column_names(&self.raw_rows, self.has_header);
        let Ok(mapping) = column_mapping(&source_columns, &self.mapping_spec) else {
            self.preview_grid = None;
            return;
        };
        let columns = mapping
            .iter()
            .map(|(_, name)| name.clone())
            .collect::<Vec<_>>();
        if columns.is_empty() {
            self.preview_grid = None;
            return;
        }
        let set = ToolQuerySet {
            columns: columns
                .iter()
                .map(|name| ResultColumn {
                    name: name.clone(),
                    data_type: "nvarchar".to_string(),
                    nullable: true,
                })
                .collect(),
            rows: self
                .data_rows()
                .iter()
                .take(PREVIEW_ROWS)
                .map(|row| {
                    (0..columns.len())
                        .map(|i| csv_cell_value(row.get(mapping[i].0), &self.null_token))
                        .collect()
                })
                .collect(),
        };
        self.preview_grid = crate::tool_dialog::build_result_grid(&set, cx);
        // Refresh the rejected-row preview from a bounded scan, so a huge
        // file cannot make every input keystroke rescan everything; the
        // script-time conversion re-checks all rows.
        const REJECT_SCAN_ROWS: usize = 5_000;
        self.rejected = match self.csv_column_types(&mapping) {
            Some(types) => {
                let scan: Vec<Vec<String>> = self
                    .data_rows()
                    .iter()
                    .take(REJECT_SCAN_ROWS)
                    .cloned()
                    .collect();
                typed_csv_rows(&mapping, &types, &scan, &self.null_token).1
            }
            None => Vec::new(),
        };
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn current_scope(&self) -> ImportScope {
        ImportScope {
            engine: self.database_type.display_name().to_string(),
            connection_id: self.scope_connection_id,
            database: self.scope_database.clone(),
            schema: self.schema.trim().to_string(),
            table: self.table.trim().to_string(),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn current_source_columns(&self) -> Vec<String> {
        self.typed_import.as_ref().map_or_else(
            || column_names(&self.raw_rows, self.has_header),
            |import| {
                import
                    .columns
                    .iter()
                    .map(|column| column.name.clone())
                    .collect()
            },
        )
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn refresh_profiles(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let store = match ImportProfiles::load() {
            Ok(store) => store,
            Err(error) => {
                self.profile_notice = Some(crate::localization::format(
                    "results.import.profiles_unavailable",
                    &[("error", &error)],
                ));
                ImportProfiles::default()
            }
        };
        let scope = self.current_scope();
        self.profiles = store.for_connection(&scope).into_iter().cloned().collect();
        let items: Vec<ProfileChoice> = self
            .profiles
            .iter()
            .map(|p| ProfileChoice {
                label: p.label().into(),
                id: p.id,
            })
            .collect();
        let selected = self
            .selected_profile
            .and_then(|id| self.profiles.iter().position(|p| p.id == id))
            .map(gpui_component::IndexPath::new);
        self.profile_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            state.set_selected_index(selected, window, cx);
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn save_profile(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let name = self.profile_name_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("results.import.profile_name_required"),
            });
            return;
        }
        if self.table.trim().is_empty() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("results.import.profile_target_required"),
            });
            return;
        }
        if !self.current_scope().is_saved_connection() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("results.import.profile_connection_required"),
            });
            return;
        }
        let mut store = match ImportProfiles::load() {
            Ok(store) => store,
            Err(error) => {
                cx.emit(ToolDialogEvent::Status {
                    level: StatusLevel::Error,
                    message: error,
                });
                return;
            }
        };
        // Fresh id on purpose: Save upserts by (scope, name), so saving over
        // an existing name overwrites that profile (keeping its default flag)
        // and a new name always creates rather than silently renaming.
        let profile = ImportProfile {
            id: uuid::Uuid::new_v4(),
            name: name.clone(),
            scope: self.current_scope(),
            delimiter: self.delimiter.to_string(),
            has_header: self.has_header,
            sheet: self.selected_sheet.clone(),
            null_token: self.null_token.clone(),
            mapping_spec: self.mapping_spec.clone(),
            types_spec: self.types_spec.clone(),
            create_table: self.import_target == ImportTarget::CreateNewTable,
            bulk_insert: self.script_kind == ImportScriptKind::BulkInsert,
            source_columns: self.current_source_columns(),
            is_default: false,
            source_pattern: self
                .file_path
                .as_ref()
                .and_then(|path| path.file_name())
                .and_then(|name| crate::import_profile::source_pattern(&name.to_string_lossy())),
        };
        store.upsert(profile.clone());
        if let Err(error) = store.save() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Error,
                message: error,
            });
            return;
        }
        // Select the profile just saved (upsert may have kept an existing id).
        self.selected_profile = Some(
            store
                .profiles
                .iter()
                .find(|p| p.scope == profile.scope && p.name.eq_ignore_ascii_case(&name))
                .map_or(profile.id, |p| p.id),
        );
        self.profile_notice = None;
        self.refresh_profiles(window, cx);
        cx.emit(ToolDialogEvent::Status {
            level: StatusLevel::Success,
            message: crate::localization::format(
                "results.import.profile_saved",
                &[("profile_name", &name)],
            ),
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn rename_profile(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Some(id) = self.selected_profile else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("results.import.profile_selection_required"),
            });
            return;
        };
        let name = self.profile_name_input.read(cx).value().trim().to_string();
        let result = ImportProfiles::load().and_then(|mut store| {
            store.rename(id, &name)?;
            store.save()
        });
        match result {
            Ok(()) => {
                self.refresh_profiles(window, cx);
                cx.emit(ToolDialogEvent::Status {
                    level: StatusLevel::Success,
                    message: crate::localization::format(
                        "results.import.profile_renamed",
                        &[("profile_name", &name)],
                    ),
                });
            }
            Err(reason) => cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: reason,
            }),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn delete_profile(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Some(id) = self.selected_profile else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("results.import.profile_selection_required"),
            });
            return;
        };
        if let Err(error) = ImportProfiles::load().and_then(|mut store| {
            store.remove(id);
            store.save()
        }) {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Error,
                message: error,
            });
            return;
        }
        self.selected_profile = None;
        self.refresh_profiles(window, cx);
        cx.emit(ToolDialogEvent::Status {
            level: StatusLevel::Success,
            message: crate::localization::text("results.import.profile_deleted"),
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn toggle_profile_default(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Some(id) = self.selected_profile else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("results.import.profile_selection_required"),
            });
            return;
        };
        let currently_default = self
            .profiles
            .iter()
            .find(|p| p.id == id)
            .is_some_and(|p| p.is_default);
        if let Err(error) = ImportProfiles::load().and_then(|mut store| {
            store.set_default(id, !currently_default);
            store.save()
        }) {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Error,
                message: error,
            });
            return;
        }
        self.refresh_profiles(window, cx);
        let message = if currently_default {
            crate::localization::text("results.import.default_profile_cleared")
        } else {
            let scope = self.current_scope();
            crate::localization::format(
                "results.import.default_profile_set",
                &[("schema_name", &scope.schema), ("table_name", &scope.table)],
            )
        };
        cx.emit(ToolDialogEvent::Status {
            level: StatusLevel::Success,
            message,
        });
    }

    /// Auto-apply a default profile once a source exists (file chosen or
    /// clipboard pasted). Runs on load and again whenever the target schema
    /// or table changes: the exact target's default wins, otherwise a default
    /// on this connection whose saved file pattern or header columns match
    /// the source (see [`ImportProfiles::auto_apply_candidate`]). Each
    /// default applies at most once per loaded source, so typing in the
    /// table field never re-applies over the user's edits.
    #[cfg(not(target_arch = "wasm32"))]
    fn maybe_apply_default_profile(&mut self, cx: &mut gpui::Context<Self>) {
        if self.raw_rows.is_empty() && self.typed_import.is_none() {
            return;
        }
        // `profiles` already holds every profile on this connection and
        // database (refreshed on open and after each save), which is all
        // the candidates — no store read on every table-field keystroke.
        let store = ImportProfiles {
            profiles: self.profiles.clone(),
        };
        let columns = self.current_source_columns();
        let file_name = self
            .file_path
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().to_string());
        let source = crate::import_profile::LoadedSource {
            file_name: file_name.as_deref(),
            columns: &columns,
        };
        let Some(profile) = store
            .auto_apply_candidate(&self.current_scope(), &source)
            .cloned()
        else {
            return;
        };
        let key = (profile.id, self.load_generation);
        if self.auto_applied == Some(key) {
            return;
        }
        self.auto_applied = Some(key);
        self.selected_profile = Some(profile.id);
        self.pending_profile_apply = Some(profile);
        cx.notify();
    }

    /// Install a profile into the wizard's fields. Drift is honest: source
    /// columns the file no longer has keep their mapping entries dropped with
    /// a warning; unavailable options (BULK INSERT on another engine, a sheet
    /// the workbook lacks) degrade with a note instead of failing silently.
    #[cfg(not(target_arch = "wasm32"))]
    fn apply_profile_state(
        &mut self,
        profile: ImportProfile,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let mut notices: Vec<String> = Vec::new();
        // Show which profile is in effect (auto-applied defaults included).
        let id = profile.id;
        self.profile_select.update(cx, |state, cx| {
            state.set_selected_value(&id, window, cx);
        });
        if self.schema != profile.scope.schema || self.table != profile.scope.table {
            self.schema = profile.scope.schema.clone();
            self.table = profile.scope.table.clone();
            self.schema_input.update(cx, |state, cx| {
                state.set_value(self.schema.clone(), window, cx);
            });
            self.table_input.update(cx, |state, cx| {
                state.set_value(self.table.clone(), window, cx);
            });
        }
        if let Some(delimiter) = profile.delimiter.chars().next()
            && self.delimiter != delimiter
        {
            self.delimiter = delimiter;
            // Re-parse synchronously (apply is a deliberate click) so the
            // mapping rebase below sees the columns this delimiter produces.
            if !self.loaded_json
                && self.sheet_names.is_empty()
                && let Some(text) = self.raw_text.clone()
            {
                self.raw_rows = parse_csv(&text, delimiter);
            }
        }
        self.has_header = profile.has_header || self.loaded_json;
        self.null_token = profile.null_token.clone();
        self.null_token_input.update(cx, |state, cx| {
            state.set_value(self.null_token.clone(), window, cx);
        });
        self.import_target = if profile.create_table {
            ImportTarget::CreateNewTable
        } else {
            ImportTarget::ExistingTable
        };
        let bulk_supported = self.database_type.connection_kind()
            == sqlly_lang::sqlmodel::ConnectionKind::Tds
            && self.import_target != ImportTarget::CreateNewTable;
        self.script_kind = if profile.bulk_insert && bulk_supported {
            ImportScriptKind::BulkInsert
        } else {
            if profile.bulk_insert {
                notices.push(crate::localization::text(
                    "results.import.bulk_insert_fallback",
                ));
            }
            ImportScriptKind::InsertStatements
        };
        let source_columns = self.current_source_columns();
        let mapping = crate::import_profile::rebase_mapping_spec(
            &profile.source_columns,
            &profile.mapping_spec,
            &source_columns,
            profile.create_table,
        );
        if !mapping.dropped.is_empty() {
            notices.push(crate::localization::format(
                "results.import.mapping_dropped_columns",
                &[("column_names", &mapping.dropped.join(", "))],
            ));
        }
        if !mapping.skipped_new.is_empty() {
            notices.push(crate::localization::format(
                "results.import.mapping_new_columns_skipped",
                &[("column_names", &mapping.skipped_new.join(", "))],
            ));
        }
        self.mapping_spec = mapping.spec;
        self.mapping_input.update(cx, |state, cx| {
            state.set_value(self.mapping_spec.clone(), window, cx);
        });
        let types = crate::import_profile::rebase_types_spec(
            &profile.source_columns,
            &profile.types_spec,
            &source_columns,
        );
        self.types_spec = types.spec;
        self.types_input.update(cx, |state, cx| {
            state.set_value(self.types_spec.clone(), window, cx);
        });
        if let Some(sheet) = &profile.sheet {
            if self.sheet_names.iter().any(|name| name == sheet) {
                if self.selected_sheet.as_deref() != Some(sheet.as_str()) {
                    self.selected_sheet = Some(sheet.clone());
                    self.reload_sheet(cx);
                }
            } else if !self.sheet_names.is_empty() {
                notices.push(crate::localization::format(
                    "results.import.profile_sheet_missing",
                    &[("sheet_name", sheet)],
                ));
            }
        }
        self.profile_notice = if notices.is_empty() {
            None
        } else {
            Some(crate::localization::format(
                "results.import.profile_drift_notice",
                &[
                    ("profile_name", &profile.name),
                    ("notices", &notices.join("; ")),
                ],
            ))
        };
        self.preview_dirty = true;
        cx.notify();
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }

    fn chip(
        &self,
        label: String,
        active: bool,
        on_click: impl Fn(&mut Self, &mut gpui::Context<Self>) + 'static,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .px(px(Theme::scaled(10.0)))
            .py(px(Theme::scaled(3.0)))
            .rounded(px(Theme::scaled(4.0)))
            .cursor_pointer()
            .bg(if active {
                Theme::selection()
            } else {
                gpui::transparent_black()
            })
            .text_color(if active {
                Theme::text()
            } else {
                Theme::text_dim()
            })
            .child(label)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, cx| on_click(this, cx)),
            )
            .into_any_element()
    }
}

impl gpui::Focusable for ImportExportDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ImportExportDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        if let Some(prefill) = self.pending_table_prefill.take() {
            self.table_input
                .update(cx, |state, cx| state.set_value(prefill, window, cx));
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((names, selected)) = self.pending_sheet_setup.take() {
            self.sheet_select.update(cx, |state, cx| {
                state.set_items(names.clone(), window, cx);
                if let Some(index) = names.iter().position(|name| name == &selected) {
                    state.set_selected_index(
                        Some(gpui_component::IndexPath::new(index)),
                        window,
                        cx,
                    );
                }
            });
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(profile) = self.pending_profile_apply.take() {
            self.apply_profile_state(profile, window, cx);
        }
        if self.preview_dirty {
            self.preview_dirty = false;
            self.rebuild_preview(cx);
        }

        let file_label = {
            let source = self
                .file_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .or_else(|| self.source_label.clone())
                .unwrap_or_else(|| crate::localization::text("results.import.no_file_chosen"));
            #[cfg(not(target_arch = "wasm32"))]
            {
                let encoding = self.encoding_label.unwrap_or("").to_string();
                let errors = self.sheet_error_cells.to_string();
                let source = source.as_str();
                let encoding = encoding.as_str();
                let errors = errors.as_str();
                match (
                    self.encoding_label.is_some(),
                    self.encoding_lossy,
                    self.sheet_error_cells > 0,
                ) {
                    (false, false, false) => crate::localization::format(
                        "results.import.source_metadata_plain",
                        &[("source", source)],
                    ),
                    (true, false, false) => crate::localization::format(
                        "results.import.source_metadata_encoding",
                        &[("source", source), ("encoding", encoding)],
                    ),
                    (false, true, false) => crate::localization::format(
                        "results.import.source_metadata_lossy",
                        &[("source", source)],
                    ),
                    (true, true, false) => crate::localization::format(
                        "results.import.source_metadata_encoding_lossy",
                        &[("source", source), ("encoding", encoding)],
                    ),
                    (false, false, true) => crate::localization::format(
                        "results.import.source_metadata_sheet_errors",
                        &[("source", source), ("error_cell_count", errors)],
                    ),
                    (true, false, true) => crate::localization::format(
                        "results.import.source_metadata_encoding_sheet_errors",
                        &[
                            ("source", source),
                            ("encoding", encoding),
                            ("error_cell_count", errors),
                        ],
                    ),
                    (false, true, true) => crate::localization::format(
                        "results.import.source_metadata_lossy_sheet_errors",
                        &[("source", source), ("error_cell_count", errors)],
                    ),
                    (true, true, true) => crate::localization::format(
                        "results.import.source_metadata_all",
                        &[
                            ("source", source),
                            ("encoding", encoding),
                            ("error_cell_count", errors),
                        ],
                    ),
                }
            }
            #[cfg(target_arch = "wasm32")]
            {
                source
            }
        };
        let typed_import = self.typed_import.is_some();
        let data_row_count = self
            .typed_import
            .as_ref()
            .map_or_else(|| self.data_rows().len(), |import| import.rows.len());
        let source_columns = self.typed_import.as_ref().map_or_else(
            || column_names(&self.raw_rows, self.has_header),
            |import| {
                import
                    .columns
                    .iter()
                    .map(|column| column.name.clone())
                    .collect()
            },
        );
        let mapping_error = column_mapping(&source_columns, &self.mapping_spec).err();
        let types_error = if self.typed_import.is_some() {
            // Portable exports carry their own types; the spec only applies
            // to CSV.
            None
        } else {
            parse_type_overrides(&self.types_spec, source_columns.len()).err()
        };
        let mapping_supported =
            self.script_kind != ImportScriptKind::BulkInsert || self.mapping_spec.trim().is_empty();
        let can_script = !self.loading
            && data_row_count > 0
            && !self.table.trim().is_empty()
            && mapping_error.is_none()
            && types_error.is_none()
            && mapping_supported;

        let delimiter = self.delimiter;
        let delim_chip =
            |label: String, ch: char, this: &mut Self, cx: &mut gpui::Context<Self>| {
                this.chip(
                    label,
                    delimiter == ch,
                    move |this, cx| {
                        if this.delimiter != ch {
                            this.delimiter = ch;
                            this.reparse(cx);
                        }
                    },
                    cx,
                )
            };
        let delim_row = {
            let a = delim_chip(
                crate::localization::text("results.import.delimiter_comma"),
                ',',
                self,
                cx,
            );
            let b = delim_chip(
                crate::localization::text("results.import.delimiter_semicolon"),
                ';',
                self,
                cx,
            );
            let c = delim_chip(
                crate::localization::text("results.import.delimiter_tab"),
                '\t',
                self,
                cx,
            );
            let d = delim_chip(
                crate::localization::text("results.import.delimiter_pipe"),
                '|',
                self,
                cx,
            );
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(4.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("results.import.delimiter_label")),
                )
                .child(a)
                .child(b)
                .child(c)
                .child(d)
        };

        let kind = self.script_kind;
        let kind_row = {
            let a = self.chip(
                crate::localization::text("results.import.script_insert_statements"),
                kind == ImportScriptKind::InsertStatements,
                |this, cx| {
                    this.script_kind = ImportScriptKind::InsertStatements;
                    cx.notify();
                },
                cx,
            );
            let bulk_supported = self.database_type.connection_kind()
                == sqlly_lang::sqlmodel::ConnectionKind::Tds
                && self.import_target != ImportTarget::CreateNewTable;
            let b = bulk_supported.then(|| {
                self.chip(
                    crate::localization::text("results.import.script_bulk_insert"),
                    kind == ImportScriptKind::BulkInsert,
                    |this, cx| {
                        this.script_kind = ImportScriptKind::BulkInsert;
                        cx.notify();
                    },
                    cx,
                )
            });
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(4.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("results.import.script_as_label")),
                )
                .child(a)
                .children(if typed_import { None } else { b })
        };

        let create_table_checkbox = {
            let this = cx.entity().downgrade();
            Checkbox::new("ie-create-table")
                .checked(self.import_target == ImportTarget::CreateNewTable)
                .label(crate::localization::text("results.import.create_table"))
                .on_click(move |checked: &bool, _window, app| {
                    let checked = *checked;
                    if let Some(this) = this.upgrade() {
                        this.update(app, |this, cx| {
                            this.import_target = if checked {
                                ImportTarget::CreateNewTable
                            } else {
                                ImportTarget::ExistingTable
                            };
                            if checked {
                                this.script_kind = ImportScriptKind::InsertStatements;
                            }
                            cx.notify();
                        });
                    }
                })
        };

        let header_checkbox = {
            let this = cx.entity().downgrade();
            Checkbox::new("ie-header")
                .checked(self.has_header)
                .label(crate::localization::text("results.import.first_row_header"))
                .on_click(move |checked: &bool, _window, app| {
                    let checked = *checked;
                    if let Some(this) = this.upgrade() {
                        this.update(app, |this, cx| {
                            // JSON rows always start with a synthesized header
                            // row; treating it as data would corrupt the import.
                            this.has_header = checked || this.loaded_json;
                            this.preview_dirty = true;
                            cx.notify();
                        });
                    }
                })
        };

        #[cfg(not(target_arch = "wasm32"))]
        let sheet_controls = (!self.sheet_names.is_empty()).then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(4.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("results.import.sheet_label")),
                )
                .child(Select::new(&self.sheet_select))
                .into_any_element()
        });
        #[cfg(target_arch = "wasm32")]
        let sheet_controls: Option<gpui::AnyElement> = None;

        #[cfg(not(target_arch = "wasm32"))]
        let profiles_row = {
            let no_profile = self.selected_profile.is_none();
            Some(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(4.0)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(8.0)))
                            .child(
                                div()
                                    .w(px(s(120.0)))
                                    .text_size(px(Theme::font_size_ratio(0.87)))
                                    .text_color(Theme::text_dim())
                                    .child(crate::localization::text(
                                        "results.import.profiles_label",
                                    )),
                            )
                            .child(
                                div()
                                    .w(px(s(200.0)))
                                    .child(Select::new(&self.profile_select)),
                            )
                            .child(div().flex_1().child(Input::new(&self.profile_name_input)))
                            .child(
                                Button::new("ie-profile-save")
                                    .label(crate::localization::text(
                                        "results.import.profile_save_button",
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_profile(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("ie-profile-rename")
                                    .label(crate::localization::text(
                                        "results.import.profile_rename_button",
                                    ))
                                    .disabled(no_profile)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.rename_profile(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("ie-profile-default")
                                    .label(crate::localization::text(
                                        "results.import.profile_default_button",
                                    ))
                                    .disabled(no_profile)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.toggle_profile_default(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("ie-profile-delete")
                                    .label(crate::localization::text(
                                        "results.import.profile_delete_button",
                                    ))
                                    .disabled(no_profile)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.delete_profile(window, cx);
                                    })),
                            ),
                    )
                    .children(self.profile_notice.clone().map(|notice| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::warning())
                            .whitespace_normal()
                            .child(notice)
                    }))
                    .into_any_element(),
            )
        };
        #[cfg(target_arch = "wasm32")]
        let profiles_row: Option<gpui::AnyElement> = None;

        let preview: gpui::AnyElement = if self.loading {
            div()
                .p(px(s(10.0)))
                .text_color(Theme::text_dim())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .child(crate::localization::text("results.import.loading_preview"))
                .into_any_element()
        } else if let Some(err) = &self.parse_error {
            div()
                .p(px(s(10.0)))
                .text_color(Theme::error())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .child(err.clone())
                .into_any_element()
        } else if let Some(grid) = &self.preview_grid {
            div().size_full().child(grid.clone()).into_any_element()
        } else {
            div()
                .p(px(s(10.0)))
                .text_color(Theme::text_dim())
                .text_size(px(Theme::font_size_ratio(0.87)))
                .child(if cfg!(target_arch = "wasm32") {
                    crate::browser_preview::unavailable(&crate::localization::text(
                        "results.import.browser_local_files_unavailable",
                    ))
                } else {
                    crate::localization::text("results.import.empty_preview")
                })
                .into_any_element()
        };

        // Bare chrome body: the wizard keeps its own padding/gap and the
        // embedded preview grid manages its own space.
        let body = div()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .p(px(s(14.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        Button::new("ie-choose")
                            .label(crate::localization::text(
                                "results.import.choose_file_button",
                            ))
                            .disabled(self.loading)
                            .on_click(cx.listener(|this, _, _window, cx| this.choose_file(cx))),
                    )
                    .when(cfg!(not(target_arch = "wasm32")), |row| {
                        row.child(
                            Button::new("ie-paste")
                                .label(crate::localization::text(
                                    "results.import.paste_clipboard_button",
                                ))
                                .disabled(self.loading)
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    if let Some(text) =
                                        cx.read_from_clipboard().and_then(|item| item.text())
                                    {
                                        #[cfg(not(target_arch = "wasm32"))]
                                        this.apply_clipboard_text(text, cx);
                                        #[cfg(target_arch = "wasm32")]
                                        let _ = text;
                                    } else {
                                        this.parse_error = Some(crate::localization::text(
                                            "results.import.clipboard_empty",
                                        ));
                                        cx.notify();
                                    }
                                })),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .text_size(px(Theme::font_size_ratio(0.80)))
                            .text_color(Theme::text_dim())
                            .child(file_label),
                    ),
            )
            .when(!typed_import, |dialog| {
                dialog.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(14.0)))
                        .child(delim_row)
                        .child(header_checkbox)
                        .child(create_table_checkbox)
                        .children(sheet_controls),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.87)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text("results.import.target_label")),
                    )
                    .child(div().w(px(s(120.0))).child(Input::new(&self.schema_input)))
                    .child(div().text_color(Theme::text_dim()).child("."))
                    .child(div().w(px(s(220.0))).child(Input::new(&self.table_input))),
            )
            .children(profiles_row)
            .when(
                self.import_target == ImportTarget::CreateNewTable && !typed_import,
                |dialog| {
                    let summary = column_mapping(&source_columns, &self.mapping_spec)
                        .ok()
                        .and_then(|mapping| {
                            self.create_table_ddl_columns(&mapping).map(|columns| {
                                columns
                                    .iter()
                                    .map(|(name, ty, _)| format!("{}: {}", name, ty.label()))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                        })
                        .unwrap_or_else(|| "…".to_string());
                    dialog.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "results.import.proposed_columns",
                                &[("summary", &summary)],
                            )),
                    )
                },
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .w(px(s(120.0)))
                            .text_size(px(Theme::font_size_ratio(0.87)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "results.import.column_mapping_label",
                            )),
                    )
                    .child(div().flex_1().child(Input::new(&self.mapping_input))),
            )
            .when(!typed_import, |dialog| {
                dialog
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(8.0)))
                            .child(
                                div()
                                    .w(px(s(120.0)))
                                    .text_size(px(Theme::font_size_ratio(0.87)))
                                    .text_color(Theme::text_dim())
                                    .child(crate::localization::text(
                                        "results.import.null_token_label",
                                    )),
                            )
                            .child(div().flex_1().child(Input::new(&self.null_token_input))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(8.0)))
                            .child(
                                div()
                                    .w(px(s(120.0)))
                                    .text_size(px(Theme::font_size_ratio(0.87)))
                                    .text_color(Theme::text_dim())
                                    .child(crate::localization::text(
                                        "results.import.column_types_label",
                                    )),
                            )
                            .child(div().flex_1().child(Input::new(&self.types_input))),
                    )
                    .children(types_error.clone().map(|error| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::error())
                            .child(error)
                    }))
                    .when(!self.rejected.is_empty(), |dialog| {
                        let mut lines = self
                            .rejected
                            .iter()
                            .take(5)
                            .map(|reject| {
                                crate::localization::format(
                                    "results.import.rejected_row_detail",
                                    &[
                                        ("row_number", &reject.row_number.to_string()),
                                        ("column_name", &reject.column),
                                        ("value", &reject.value),
                                        ("reason", &reject.reason),
                                    ],
                                )
                            })
                            .collect::<Vec<_>>();
                        if self.rejected.len() > 5 {
                            lines.push(crate::localization::format(
                                "results.import.rejected_rows_omitted",
                                &[("rejected_count", &(self.rejected.len() - 5).to_string())],
                            ));
                        }
                        let rejected_count = self.rejected.len().to_string();
                        let details = lines.join(" · ");
                        let key = if self.rejected.len() == 1 {
                            "results.import.rejected_rows_summary_one"
                        } else {
                            "results.import.rejected_rows_summary"
                        };
                        dialog.child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.8)))
                                .text_color(Theme::warning())
                                .whitespace_normal()
                                .child(crate::localization::format(
                                    key,
                                    &[("rejected_count", &rejected_count), ("details", &details)],
                                )),
                        )
                    })
            })
            .children(mapping_error.map(|error| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::error())
                    .child(error)
            }))
            .when(!mapping_supported, |dialog| {
                dialog.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .text_color(Theme::warning())
                        .child(crate::localization::text(
                            "results.import.mapping_bulk_insert_warning",
                        )),
                )
            })
            .child(kind_row)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .child(preview),
            );

        let focus_handle = self.focus_handle.clone();
        Dialog::new("import-export-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("results.import.dialog_title"))
            .key_context("ImportExportDialog")
            .track_focus(&focus_handle)
            .bare_body()
            .dismiss_on_scrim_click(false)
            .on_close(|this, _window, cx| this.close(cx))
            .child(body)
            .leading_button(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(if data_row_count > PREVIEW_ROWS {
                        crate::localization::format(
                            "results.import.preview_footer_limited",
                            &[
                                ("preview_row_count", &PREVIEW_ROWS.to_string()),
                                ("data_row_count", &data_row_count.to_string()),
                            ],
                        )
                    } else {
                        crate::localization::format(
                            "results.import.preview_footer",
                            &[("data_row_count", &data_row_count.to_string())],
                        )
                    }),
            )
            .button(
                Button::new("ie-export")
                    .label(crate::localization::text(
                        "results.import.export_select_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| this.script_export_select(cx))),
            )
            .button(
                Button::new("ie-script")
                    .primary()
                    .label(crate::localization::text(
                        "results.import.script_import_button",
                    ))
                    .disabled(!can_script)
                    .on_click(cx.listener(|this, _, _window, cx| this.script_to_editor(cx))),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string_rows(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|row| row.iter().map(|cell| cell.to_string()).collect())
            .collect()
    }

    #[test]
    fn type_inference_finds_the_most_specific_type() {
        let rows = string_rows(&[
            &[
                "1",
                "1.5",
                "true",
                "2024-01-31",
                "2024-01-31 10:00:00",
                "0aeb1a2c-1111-2222-3333-444455556666",
                "hello",
            ],
            &[
                "-2",
                "2",
                "no",
                "2023-12-01",
                "2023-12-01",
                "0AEB1A2C-1111-2222-3333-444455556666",
                "3",
            ],
        ]);
        let types = infer_csv_types(&rows, 7, "");
        assert_eq!(
            types,
            vec![
                CsvType::Integer,
                CsvType::Decimal,
                CsvType::Boolean,
                CsvType::Date,
                CsvType::DateTime,
                CsvType::Uuid,
                CsvType::Text,
            ]
        );
    }

    #[test]
    fn type_inference_ignores_nulls_and_null_tokens() {
        let rows = string_rows(&[&["", "NULL"], &["7", "8"]]);
        let types = infer_csv_types(&rows, 2, "NULL");
        assert_eq!(types, vec![CsvType::Integer, CsvType::Integer]);
        // A column with no values stays text.
        let types = infer_csv_types(&string_rows(&[&[""], &[""]]), 1, "");
        assert_eq!(types, vec![CsvType::Text]);
    }

    #[test]
    fn type_overrides_parse_and_validate() {
        let overrides = parse_type_overrides("auto, int , text", 3).unwrap();
        assert_eq!(
            overrides,
            vec![None, Some(CsvType::Integer), Some(CsvType::Text)]
        );
        assert!(parse_type_overrides("int", 2).is_err());
        assert!(parse_type_overrides("widget", 1).is_err());
        assert_eq!(parse_type_overrides("", 2).unwrap(), vec![None, None]);
    }

    #[test]
    fn typed_rows_convert_and_reject_mismatches() {
        let mapping = vec![(0usize, "id".to_string()), (1, "name".to_string())];
        let types = vec![CsvType::Integer, CsvType::Text];
        let rows = string_rows(&[&["1", "Ada"], &["oops", "Grace"], &["", "Kay"]]);
        let (accepted, rejected) = typed_csv_rows(&mapping, &types, &rows, "");
        assert_eq!(accepted.len(), 2);
        assert_eq!(accepted[0][0], CellValue::I64(1));
        assert_eq!(accepted[1][0], CellValue::Null);
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].row_number, 2);
        assert_eq!(rejected[0].column, "id");
        assert!(rejected[0].reason.contains("int"));
    }

    #[test]
    fn typed_rows_render_typed_literals_in_the_script() {
        let mapping = vec![
            (0usize, "qty".to_string()),
            (1, "active".to_string()),
            (2, "label".to_string()),
        ];
        let types = vec![CsvType::Integer, CsvType::Boolean, CsvType::Text];
        let rows = string_rows(&[&["3", "true", "it's"]]);
        let (accepted, rejected) = typed_csv_rows(&mapping, &types, &rows, "");
        assert!(rejected.is_empty());
        let columns = mapping
            .iter()
            .map(|(_, name)| ResultColumn {
                name: name.clone(),
                data_type: "text".into(),
                nullable: true,
            })
            .collect::<Vec<_>>();
        let sql = typed_insert_script_for(
            DatabaseType::SqlServer,
            "dbo",
            "T",
            &columns,
            &accepted,
            "file",
        );
        assert!(sql.contains("VALUES (3, 1, N'it''s');"), "{sql}");
    }

    #[test]
    fn datetime_detection_accepts_iso_shapes_and_rejects_noise() {
        assert!(CsvType::DateTime.accepts("2024-05-01T08:30:00Z"));
        assert!(CsvType::DateTime.accepts("2024-05-01 08:30"));
        assert!(CsvType::DateTime.accepts("2024-05-01"));
        assert!(!CsvType::DateTime.accepts("yesterday"));
        assert!(!CsvType::Date.accepts("2024-13-01"));
        assert!(!CsvType::Decimal.accepts("1.2.3"));
        assert!(!CsvType::Decimal.accepts("inf"));
    }

    #[test]
    fn parse_csv_handles_quotes_and_embedded_delimiters() {
        let rows = parse_csv("a,\"b,c\",d\n1,\"he said \"\"hi\"\"\",3\n", ',');
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["a", "b,c", "d"]);
        assert_eq!(rows[1], vec!["1", "he said \"hi\"", "3"]);
    }

    #[test]
    fn clipboard_tab_text_parses_to_expected_shape() {
        let text = crate::import_clipboard::normalise("id\tname\n1\ta\n");
        let rows = parse_csv(&text, '\t');
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].len(), 2);
        assert_eq!(rows[1], vec!["1", "a"]);
    }

    #[test]
    fn create_table_script_for_clipboard_table_name() {
        let cols = vec![
            ("id".to_string(), CsvType::Integer, 0),
            ("name".to_string(), CsvType::Text, 10),
        ];
        let sql = create_table_script(
            DatabaseType::Postgres,
            "public",
            "pasted_data_20260830_1405",
            &cols,
        );
        assert!(sql.contains(r#"CREATE TABLE "public"."pasted_data_20260830_1405""#));
    }

    #[test]
    fn parse_csv_handles_crlf_and_missing_trailing_newline() {
        let rows = parse_csv("a,b\r\n1,2", ',');
        assert_eq!(rows, vec![vec!["a", "b"], vec!["1", "2"]]);
    }

    #[test]
    fn parse_json_array_of_objects_becomes_header_and_rows() {
        let rows = parse_json_rows(
            r#"[{"id":1,"name":"Alice","active":true},{"id":2,"name":"Bob","active":false}]"#,
        )
        .unwrap();
        assert_eq!(rows[0], vec!["id", "name", "active"]);
        assert_eq!(rows[1], vec!["1", "Alice", "true"]);
        assert_eq!(rows[2], vec!["2", "Bob", "false"]);
    }

    #[test]
    fn parse_json_unions_keys_and_blanks_missing_and_null() {
        let rows = parse_json_rows(r#"[{"a":1},{"b":2,"a":null},{"a":3,"c":"x"}]"#).unwrap();
        // Columns are the union in first-seen order.
        assert_eq!(rows[0], vec!["a", "b", "c"]);
        assert_eq!(rows[1], vec!["1", "", ""]); // b, c missing
        assert_eq!(rows[2], vec!["", "2", ""]); // a is null → blank
        assert_eq!(rows[3], vec!["3", "", "x"]);
    }

    #[test]
    fn parse_json_stringifies_nested_structures() {
        let rows = parse_json_rows(r#"[{"tags":["x","y"],"meta":{"k":1}}]"#).unwrap();
        assert_eq!(rows[0], vec!["tags", "meta"]);
        assert_eq!(rows[1][0], r#"["x","y"]"#);
        assert_eq!(rows[1][1], r#"{"k":1}"#);
    }

    #[test]
    fn parse_json_accepts_wrapped_array_and_rejects_non_objects() {
        let rows = parse_json_rows(r#"{"data":[{"a":1}]}"#).unwrap();
        assert_eq!(rows[0], vec!["a"]);
        assert!(parse_json_rows("not json").is_err());
        assert!(parse_json_rows("[1,2,3]").is_err());
        assert_eq!(parse_json_rows("[]").unwrap(), Vec::<Vec<String>>::new());
    }

    #[test]
    fn parse_csv_alternate_delimiters() {
        assert_eq!(parse_csv("a;b\n1;2\n", ';')[1], vec!["1", "2"]);
        assert_eq!(parse_csv("a\tb\n1\t2\n", '\t')[0], vec!["a", "b"]);
    }

    #[test]
    fn column_names_come_from_header_or_are_generated() {
        let rows = vec![
            vec!["id".to_string(), "name".to_string()],
            vec!["1".to_string(), "x".to_string()],
        ];
        assert_eq!(column_names(&rows, true), vec!["id", "name"]);
        assert_eq!(column_names(&rows, false), vec!["Column1", "Column2"]);
    }

    #[test]
    fn column_mapping_renames_skips_and_rejects_ambiguity() {
        let source = vec!["id".to_string(), "name".to_string(), "legacy".to_string()];
        assert_eq!(
            column_mapping(&source, "customer_id, display_name, -").unwrap(),
            vec![
                (0, "customer_id".to_string()),
                (1, "display_name".to_string())
            ]
        );
        assert!(
            column_mapping(&source, "id,name")
                .unwrap_err()
                .contains("3 columns")
        );
        assert!(
            column_mapping(&source, "id,id,-")
                .unwrap_err()
                .contains("more than once")
        );
        assert!(
            column_mapping(&source, "-,-,-")
                .unwrap_err()
                .contains("every")
        );
    }

    #[test]
    fn insert_script_escapes_and_caps() {
        let columns = vec!["id".to_string(), "name".to_string()];
        let rows = vec![vec!["1".to_string(), "O'Brien".to_string()]];
        let sql = insert_script("dbo", "People", &columns, &rows);
        assert!(
            sql.contains("INSERT INTO [dbo].[People] ([id], [name]) VALUES (N'1', N'O''Brien');")
        );

        let many: Vec<Vec<String>> = (0..INSERT_SCRIPT_ROW_CAP + 5)
            .map(|i| vec![i.to_string()])
            .collect();
        let sql = insert_script("dbo", "T", &["n".to_string()], &many);
        assert!(sql.contains("-- NOTE"), "cap must be announced");
        assert_eq!(sql.matches("INSERT INTO").count(), INSERT_SCRIPT_ROW_CAP);
    }

    #[test]
    fn empty_fields_become_null() {
        let columns = vec!["a".to_string(), "b".to_string()];
        let rows = vec![vec!["1".to_string(), "".to_string()]];
        let sql = insert_script("dbo", "T", &columns, &rows);
        assert!(sql.contains("VALUES (N'1', NULL)"));
    }

    #[test]
    fn insert_script_uses_engine_dialect_and_configured_null_token() {
        let columns = vec!["order".to_string(), "enabled".to_string()];
        let rows = vec![vec!["O'Brien".to_string(), "NULL".to_string()]];
        let postgres = insert_script_for(
            DatabaseType::Postgres,
            "public",
            "select",
            &columns,
            &rows,
            "NULL",
        );
        assert!(postgres.contains(
            "INSERT INTO \"public\".\"select\" (\"order\", \"enabled\") VALUES ('O''Brien', NULL);"
        ));

        let sqlite = insert_script_for(
            DatabaseType::Sqlite,
            "ignored",
            "select",
            &columns,
            &rows,
            "NULL",
        );
        assert!(sqlite.contains("INSERT INTO \"select\""));
        assert!(!sqlite.contains("ignored"));
    }

    #[test]
    fn sqlly_export_import_preserves_typed_cells() {
        let text = r#"{
            "format":"sqlly-export",
            "version":1,
            "sourceEngine":"PostgreSQL",
            "columns":[
                {"name":"id","dataType":"bigint","nullable":false},
                {"name":"active","dataType":"bool","nullable":false},
                {"name":"payload","dataType":"bytea","nullable":true}
            ],
            "rows":[[42,true,{"$bytes":"00A1"}],[null,false,null]]
        }"#;
        let import = parse_sqlly_export(text).unwrap();
        assert_eq!(import.columns.len(), 3);
        assert_eq!(import.rows[0][0], CellValue::I64(42));
        assert_eq!(import.rows[0][1], CellValue::Bool(true));
        assert_eq!(import.rows[0][2], CellValue::Bytes(vec![0, 0xA1]));
        assert_eq!(import.rows[1][0], CellValue::Null);
        let snapshot = parse_sqlly_export_snapshot(text).unwrap();
        assert_eq!(snapshot.source_engine, DatabaseType::Postgres);
    }

    #[test]
    fn sqlly_export_import_rejects_unknown_versions_and_bad_binary() {
        assert!(
            parse_sqlly_export(r#"{"format":"sqlly-export","version":2,"columns":[],"rows":[]}"#)
                .is_err()
        );
        let bad_bytes = r#"{"format":"sqlly-export","version":1,"sourceEngine":"PostgreSQL","columns":[{"name":"x","dataType":"binary","nullable":true}],"rows":[[{"$bytes":"ABC"}]]}"#;
        assert!(
            parse_sqlly_export(bad_bytes)
                .unwrap_err()
                .contains("even number")
        );
    }

    #[test]
    fn widen_matrix_is_symmetric_for_known_pairs() {
        use CsvType::*;
        let cases = [
            ((Integer, Decimal), Decimal),
            ((Decimal, Integer), Decimal),
            ((Date, DateTime), DateTime),
            ((DateTime, Date), DateTime),
            ((Boolean, Integer), Text),
            ((Integer, Boolean), Text),
            ((Integer, Text), Text),
            ((Uuid, Integer), Text),
        ];
        for ((left, right), expected) in cases {
            assert_eq!(widen(left, right), expected, "{left:?}+{right:?}");
            assert_eq!(widen(right, left), expected, "{right:?}+{left:?}");
        }
        assert_eq!(widen(CsvType::Integer, CsvType::Integer), CsvType::Integer);
    }

    #[test]
    fn ddl_type_for_matches_engine_dialects() {
        use sqlly_lang::sqlmodel::ConnectionKind;
        assert_eq!(
            ddl_type_for(DatabaseType::SqlServer, CsvType::Integer, 0),
            "BIGINT"
        );
        assert_eq!(
            ddl_type_for(DatabaseType::Postgres, CsvType::Uuid, 0),
            "UUID"
        );
        assert_eq!(
            ddl_type_for(DatabaseType::MySql, CsvType::Boolean, 0),
            "TINYINT(1)"
        );
        assert_eq!(
            ddl_type_for(DatabaseType::Sqlite, CsvType::Decimal, 0),
            "REAL"
        );
        assert_eq!(
            ddl_type_for(DatabaseType::SqlServer, CsvType::Text, 40),
            "NVARCHAR(50)"
        );
        assert_eq!(
            ddl_type_for(DatabaseType::SqlServer, CsvType::Text, 2_000),
            "NVARCHAR(MAX)"
        );
        assert_eq!(
            ddl_type_for(DatabaseType::MySql, CsvType::Text, 2_000),
            "TEXT"
        );
        assert_eq!(
            DatabaseType::Postgres.connection_kind(),
            ConnectionKind::PostgresWire
        );
    }

    #[test]
    fn create_table_script_quotes_and_dedupes_names() {
        let sql = create_table_script(
            DatabaseType::SqlServer,
            "dbo",
            "People",
            &[
                ("id".to_string(), CsvType::Integer, 0),
                ("ID".to_string(), CsvType::Text, 10),
            ],
        );
        assert!(sql.contains("CREATE TABLE [dbo].[People]"));
        assert!(sql.contains("[id] BIGINT NULL"));
        assert!(sql.contains("[ID_2] NVARCHAR(50) NULL"));

        let postgres = create_table_script(
            DatabaseType::Postgres,
            "public",
            "people",
            &[("name".to_string(), CsvType::Text, 5)],
        );
        assert!(postgres.contains("CREATE TABLE \"public\".\"people\""));
        assert!(postgres.contains("\"name\" TEXT NULL"));

        let mysql = create_table_script(
            DatabaseType::MySql,
            "app",
            "people",
            &[("name".to_string(), CsvType::Text, 5)],
        );
        assert!(mysql.contains("CREATE TABLE `app`.`people`"));
        assert!(mysql.contains("`name` VARCHAR(50) NULL"));

        let sqlite = create_table_script(
            DatabaseType::Sqlite,
            "ignored",
            "people",
            &[("name".to_string(), CsvType::Text, 5)],
        );
        assert!(sqlite.contains("CREATE TABLE \"people\""));
        assert!(!sqlite.contains("ignored"));
    }

    #[test]
    fn batched_insert_groups_rows_for_mysql_but_not_sql_server() {
        let columns = vec![ResultColumn {
            name: "n".into(),
            data_type: "int".into(),
            nullable: true,
        }];
        let rows: Vec<Vec<CellValue>> = (0..600).map(|value| vec![CellValue::I64(value)]).collect();
        let mysql =
            typed_insert_script_for(DatabaseType::MySql, "app", "nums", &columns, &rows, "file");
        assert_eq!(mysql.matches("INSERT INTO").count(), 2);
        assert!(mysql.contains("VALUES (0,") || mysql.contains("(0)"));
        let sql_server = typed_insert_script_for(
            DatabaseType::SqlServer,
            "dbo",
            "nums",
            &columns,
            &rows,
            "file",
        );
        assert_eq!(sql_server.matches("INSERT INTO").count(), 600);
    }

    #[test]
    fn create_table_script_is_prepended_for_new_table_imports() {
        let rows = string_rows(&[&["1", "Ada"], &["2", "Bob"]]);
        let inferred = infer_csv_types(&rows, 2, "");
        assert_eq!(inferred, vec![CsvType::Integer, CsvType::Text]);
        let ddl = create_table_script(
            DatabaseType::Sqlite,
            "",
            "people",
            &[
                ("id".to_string(), CsvType::Integer, 0),
                ("name".to_string(), CsvType::Text, 3),
            ],
        );
        assert!(ddl.starts_with("CREATE TABLE"));
    }

    #[test]
    fn typed_import_script_keeps_numbers_booleans_binary_and_escaping() {
        let columns = vec![
            ResultColumn {
                name: "n".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "ok".into(),
                data_type: "bit".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            },
            ResultColumn {
                name: "raw".into(),
                data_type: "varbinary".into(),
                nullable: true,
            },
        ];
        let rows = vec![vec![
            CellValue::I64(7),
            CellValue::Bool(true),
            CellValue::String("O'Brien".into()),
            CellValue::Bytes(vec![0xDE, 0xAD]),
        ]];
        let sql = typed_insert_script("dbo", "Imported", &columns, &rows);
        assert!(sql.contains("VALUES (7, 1, N'O''Brien', 0xDEAD)"), "{sql}");
    }
}
