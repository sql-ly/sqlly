//! Result pivot — collapses a flat result set into a pivot table per a
//! `ResultPivotSpec` parsed from `-- sqlly pivot:` SQL comment hints.
//!
//! Port of `SQLLYExports/ResultPivot.swift` + `ResultPivotBuilder.swift` +
//! `ResultViewHintParser.swift`. Pure, UI-free, no I/O — fully unit-testable.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::{AggregationFn, NumberFormat, PivotConfig, TextAlignment};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of distinct pivot columns before the table is truncated.
pub const MAX_PIVOT_COLUMNS: usize = 250;
#[allow(dead_code)]
/// Maximum source rows pivoted in memory; larger sets are treated as spill.
pub const MAX_SOURCE_ROWS: usize = 100_000;
/// Canonical key used to represent NULL row/column keys so they never collide
/// with a literal `"NULL"` string value.
pub const NULL_KEY_PLACEHOLDER: &str = "<<NULL>>";

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

/// The aggregate applied to the `value` field when collapsing many source rows
/// into a single pivot cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResultPivotAggregate {
    Count,
    Sum,
    Avg,
    First,
    Max,
    Min,
}

impl ResultPivotAggregate {
    #[allow(dead_code)]
    pub fn all() -> &'static [ResultPivotAggregate] {
        &[
            ResultPivotAggregate::Count,
            ResultPivotAggregate::Sum,
            ResultPivotAggregate::Avg,
            ResultPivotAggregate::First,
            ResultPivotAggregate::Max,
            ResultPivotAggregate::Min,
        ]
    }

    pub fn from_str(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "count" => Some(Self::Count),
            "sum" => Some(Self::Sum),
            "avg" => Some(Self::Avg),
            "first" => Some(Self::First),
            "max" => Some(Self::Max),
            "min" => Some(Self::Min),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Sum => "sum",
            Self::Avg => "avg",
            Self::First => "first",
            Self::Max => "max",
            Self::Min => "min",
        }
    }
}

// ---------------------------------------------------------------------------
// Spec
// ---------------------------------------------------------------------------

/// A fully-parsed pivot request. Fields are result-column names so directives
/// remain stable when the SELECT list is reordered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultPivotSpec {
    pub column_names: Vec<String>,
    pub row_names: Vec<String>,
    pub value_name: String,
    pub aggregate: ResultPivotAggregate,
    /// Which result set (0-based) the hint targets. Defaults to the first set.
    pub result_set_index: usize,
    /// Per-field display overrides (column name → format) for Rows / Columns /
    /// Filters fields, mirroring `PivotConfig::field_formats`. Keyed by name so
    /// directives remain stable when the SELECT list is reordered.
    pub field_formats: BTreeMap<String, NumberFormat>,
    /// Display override for value cells, mirroring `PivotConfig::value_format`.
    pub value_format: Option<NumberFormat>,
}

impl ResultPivotSpec {
    #[allow(dead_code)]
    pub fn new(
        column_name: impl Into<String>,
        row_name: impl Into<String>,
        value_name: impl Into<String>,
        aggregate: ResultPivotAggregate,
    ) -> Self {
        Self {
            column_names: vec![column_name.into()],
            row_names: vec![row_name.into()],
            value_name: value_name.into(),
            aggregate,
            result_set_index: 0,
            field_formats: BTreeMap::new(),
            value_format: None,
        }
    }

    fn resolved_indices(
        &self,
        columns: &[ResultColumn],
    ) -> Result<(Vec<usize>, Vec<usize>, usize), ResultPivotDiagnostic> {
        let resolve = |role: &str, name: &str| {
            columns
                .iter()
                .position(|column| column.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| {
                    localized_diagnostic(
                        "results.pivot.missing_column",
                        &[("field_role", role), ("column_name", name)],
                    )
                })
        };
        let column_indices = self
            .column_names
            .iter()
            .map(|name| resolve("column", name))
            .collect::<Result<Vec<_>, _>>()?;
        let row_indices = self
            .row_names
            .iter()
            .map(|name| resolve("row", name))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((
            column_indices,
            row_indices,
            resolve("value", &self.value_name)?,
        ))
    }

    /// Convert a SQL comment hint into the datatable's native pivot layout.
    /// Every aggregation — including `First` — is supported by the interactive
    /// pivot engine as of datatable 4.3.0.
    pub fn pivot_config(&self, columns: &[ResultColumn]) -> Option<PivotConfig> {
        let (column_fields, row_fields, value_index) = self.resolved_indices(columns).ok()?;
        let aggregation = match self.aggregate {
            ResultPivotAggregate::Count => AggregationFn::Count,
            ResultPivotAggregate::Sum => AggregationFn::Sum,
            ResultPivotAggregate::Avg => AggregationFn::Avg,
            ResultPivotAggregate::Max => AggregationFn::Max,
            ResultPivotAggregate::Min => AggregationFn::Min,
            ResultPivotAggregate::First => AggregationFn::First,
        };
        // Formats are cosmetic: entries naming a column that is not in the
        // result set are dropped instead of invalidating the whole hint.
        let field_formats: HashMap<usize, NumberFormat> = self
            .field_formats
            .iter()
            .filter_map(|(name, format)| {
                columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(name))
                    .map(|index| (index, *format))
            })
            .collect();
        Some(PivotConfig {
            row_fields,
            column_fields,
            value_field: Some(value_index),
            aggregation,
            value_format: self.value_format,
            field_formats,
            ..PivotConfig::default()
        })
    }

    /// Capture the native pivot's current index-based state as a stable,
    /// name-based query directive.
    pub fn from_config(
        config: &PivotConfig,
        columns: &[ResultColumn],
        result_set_index: usize,
    ) -> Option<Self> {
        let row_names = config
            .row_fields
            .iter()
            .map(|&field| columns.get(field).map(|column| column.name.clone()))
            .collect::<Option<Vec<_>>>()?;
        let column_names = config
            .column_fields
            .iter()
            .map(|&field| columns.get(field).map(|column| column.name.clone()))
            .collect::<Option<Vec<_>>>()?;
        // The interactive pivot is usable with a single populated axis
        // (rows-only n-tier grouping is the everyday shape); only a pivot
        // with no grouping at all has nothing worth persisting.
        if row_names.is_empty() && column_names.is_empty() {
            return None;
        }
        let value_name = columns.get(config.value_field?)?.name.clone();
        let aggregate = match config.aggregation {
            AggregationFn::Count => ResultPivotAggregate::Count,
            AggregationFn::Sum => ResultPivotAggregate::Sum,
            AggregationFn::Avg => ResultPivotAggregate::Avg,
            AggregationFn::Min => ResultPivotAggregate::Min,
            AggregationFn::Max => ResultPivotAggregate::Max,
            AggregationFn::First => ResultPivotAggregate::First,
        };
        let field_formats = config
            .field_formats
            .iter()
            .filter_map(|(&field, format)| {
                columns
                    .get(field)
                    .map(|column| (column.name.clone(), *format))
            })
            .collect();
        Some(Self {
            column_names,
            row_names,
            value_name,
            aggregate,
            result_set_index,
            field_formats,
            value_format: config.value_format,
        })
    }

    pub fn directive(&self) -> String {
        let mut directive = String::from("-- sqlly pivot:");
        if !self.row_names.is_empty() {
            directive.push_str(&format!(" row:{};", self.row_names.join(",")));
        }
        if !self.column_names.is_empty() {
            directive.push_str(&format!(" column:{};", self.column_names.join(",")));
        }
        directive.push_str(&format!(
            " value:{}:{}",
            self.value_name,
            self.aggregate.as_str()
        ));
        for (name, format) in &self.field_formats {
            directive.push_str(&format!(
                "; format:{}:{}",
                name,
                format_options_string(format)
            ));
        }
        if let Some(format) = &self.value_format {
            directive.push_str(&format!("; valueformat:{}", format_options_string(format)));
        }
        if self.result_set_index > 0 {
            directive.push_str(&format!("; result:{}", self.result_set_index + 1));
        }
        directive
    }
}

/// Serialize a `NumberFormat` as comma-separated `key=value` options,
/// emitting only values that differ from `NumberFormat::default()`. A format
/// equal to the default serializes as the keyword `default` so the override's
/// presence survives the round trip.
fn format_options_string(format: &NumberFormat) -> String {
    let default = NumberFormat::default();
    let mut options = Vec::new();
    if format.alignment != default.alignment {
        let align = match format.alignment {
            TextAlignment::Left => "left",
            TextAlignment::Center => "center",
            TextAlignment::Right => "right",
        };
        options.push(format!("align={align}"));
    }
    if format.decimals != default.decimals {
        options.push(format!("decimals={}", format.decimals));
    }
    if format.show_negative_red != default.show_negative_red {
        options.push(format!("red={}", format.show_negative_red));
    }
    if format.negative_parentheses != default.negative_parentheses {
        options.push(format!("parens={}", format.negative_parentheses));
    }
    if format.thousands_separator != default.thousands_separator {
        options.push(format!("thousands={}", format.thousands_separator));
    }
    if options.is_empty() {
        "default".to_string()
    } else {
        options.join(",")
    }
}

/// Parse comma-separated `key=value` format options (or the keyword
/// `default`) into a `NumberFormat`. Unspecified options keep their
/// `NumberFormat::default()` values.
fn parse_format_options(raw: &str) -> Result<NumberFormat, ResultPivotDiagnostic> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(localized_diagnostic(
            "results.pivot.format_options_missing",
            &[],
        ));
    }
    if raw.eq_ignore_ascii_case("default") {
        return Ok(NumberFormat::default());
    }
    let mut format = NumberFormat::default();
    for option in raw.split(',') {
        let option = option.trim();
        if option.is_empty() {
            continue;
        }
        let Some((key, value)) = option.split_once('=') else {
            return Err(localized_diagnostic(
                "results.pivot.malformed_format_option",
                &[("option", option)],
            ));
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "align" => {
                format.alignment = match value.to_ascii_lowercase().as_str() {
                    "left" => TextAlignment::Left,
                    "center" => TextAlignment::Center,
                    "right" => TextAlignment::Right,
                    _ => {
                        return Err(localized_diagnostic(
                            "results.pivot.invalid_alignment",
                            &[("value", value)],
                        ));
                    }
                };
            }
            "decimals" => {
                format.decimals = value.parse().map_err(|_| {
                    localized_diagnostic("results.pivot.invalid_decimals", &[("value", value)])
                })?;
            }
            "red" | "parens" | "thousands" => {
                let flag = match value.to_ascii_lowercase().as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(localized_diagnostic(
                            "results.pivot.invalid_boolean_option",
                            &[("option_name", &key), ("value", value)],
                        ));
                    }
                };
                match key.as_str() {
                    "red" => format.show_negative_red = flag,
                    "parens" => format.negative_parentheses = flag,
                    _ => format.thousands_separator = flag,
                }
            }
            _ => {
                return Err(localized_diagnostic(
                    "results.pivot.unknown_format_option",
                    &[("option_name", &key)],
                ));
            }
        }
    }
    Ok(format)
}

/// Replace the last pivot directive or insert one with the other leading
/// SQLLY directives. This is used when the native Pivot sidebar is persisted
/// back into the query editor.
pub fn update_or_insert_pivot_comment(sql: &str, spec: &ResultPivotSpec) -> String {
    let replacement = spec.directive();
    let mut lines: Vec<String> = sql.lines().map(str::to_string).collect();
    if let Some(index) = lines
        .iter()
        .rposition(|line| pivot_comment_body(line).is_some())
    {
        lines[index] = replacement;
    } else {
        let insert_at = lines
            .iter()
            .take_while(|line| {
                let line = line.trim();
                line.is_empty()
                    || (line.starts_with("--") && crate::sqlly_directive::names_namespace(line))
            })
            .count();
        lines.insert(insert_at, replacement);
    }
    let mut updated = lines.join("\n");
    if sql.ends_with('\n') {
        updated.push('\n');
    }
    updated
}

// ---------------------------------------------------------------------------
// Diagnostic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultPivotDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

impl ResultPivotDiagnostic {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            message: message.into(),
        }
    }
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            message: message.into(),
        }
    }
}

pub(crate) fn localized_diagnostic(
    key: &'static str,
    arguments: &[(&str, &str)],
) -> ResultPivotDiagnostic {
    ResultPivotDiagnostic::error(crate::localization::format(key, arguments))
}

fn localized_warning(key: &'static str, arguments: &[(&str, &str)]) -> ResultPivotDiagnostic {
    ResultPivotDiagnostic::warning(crate::localization::format(key, arguments))
}

// ---------------------------------------------------------------------------
// View hints
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultViewKind {
    Grid,
    Pivot,
    Chart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultViewHints {
    pub default_view: ResultViewKind,
    pub pivot: Option<ResultPivotSpec>,
    pub chart: Option<crate::result_chart::ResultChartSpec>,
    pub diagnostics: Vec<ResultPivotDiagnostic>,
}

impl Default for ResultViewHints {
    fn default() -> Self {
        Self {
            default_view: ResultViewKind::Grid,
            pivot: None,
            chart: None,
            diagnostics: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Output table
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ResultPivotTable {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ResultPivotBuildResult {
    pub table: ResultPivotTable,
    pub diagnostics: Vec<ResultPivotDiagnostic>,
}

// ---------------------------------------------------------------------------
// Accumulator
// ---------------------------------------------------------------------------

/// Accumulates the source `value` cells that fall into one pivot intersection.
#[derive(Debug, Clone, PartialEq)]
pub struct PivotAccumulator {
    values: Vec<CellValue>,
    numeric_values: Vec<f64>,
    non_null_count: usize,
    all_non_null_numeric: bool,
}

impl Default for PivotAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl PivotAccumulator {
    pub fn new() -> Self {
        Self {
            values: Vec::new(),
            numeric_values: Vec::new(),
            non_null_count: 0,
            all_non_null_numeric: true,
        }
    }

    pub fn append(&mut self, cell: CellValue) {
        self.values.push(cell.clone());
        if matches!(cell, CellValue::Null) {
            return;
        }
        self.non_null_count += 1;
        if let Some(n) = cell_numeric_value(&cell) {
            self.numeric_values.push(n);
        } else {
            self.all_non_null_numeric = false;
        }
    }

    pub fn result(&self, aggregate: ResultPivotAggregate) -> CellValue {
        match aggregate {
            ResultPivotAggregate::Count => CellValue::I64(self.non_null_count as i64),
            ResultPivotAggregate::Sum => {
                if self.numeric_values.is_empty() {
                    CellValue::Null
                } else {
                    CellValue::F64(self.numeric_values.iter().sum())
                }
            }
            ResultPivotAggregate::Avg => {
                if self.numeric_values.is_empty() {
                    CellValue::Null
                } else {
                    CellValue::F64(
                        self.numeric_values.iter().sum::<f64>() / self.numeric_values.len() as f64,
                    )
                }
            }
            ResultPivotAggregate::First => self
                .values
                .iter()
                .find(|c| !matches!(c, CellValue::Null))
                .cloned()
                .unwrap_or(CellValue::Null),
            ResultPivotAggregate::Max => self.extreme(true),
            ResultPivotAggregate::Min => self.extreme(false),
        }
    }

    fn extreme(&self, find_max: bool) -> CellValue {
        let non_null: Vec<&CellValue> = self
            .values
            .iter()
            .filter(|c| !matches!(c, CellValue::Null))
            .collect();
        if non_null.is_empty() {
            return CellValue::Null;
        }
        if self.all_non_null_numeric {
            let chosen = if find_max {
                non_null.iter().max_by(|a, b| {
                    cell_numeric_value(a)
                        .unwrap_or(0.0)
                        .partial_cmp(&cell_numeric_value(b).unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            } else {
                non_null.iter().min_by(|a, b| {
                    cell_numeric_value(a)
                        .unwrap_or(0.0)
                        .partial_cmp(&cell_numeric_value(b).unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            };
            chosen.map(|c| (*c).clone()).unwrap_or(CellValue::Null)
        } else {
            let chosen = if find_max {
                non_null
                    .iter()
                    .max_by(|a, b| cell_display_key(a).cmp(&cell_display_key(b)))
            } else {
                non_null
                    .iter()
                    .min_by(|a, b| cell_display_key(a).cmp(&cell_display_key(b)))
            };
            chosen.map(|c| (*c).clone()).unwrap_or(CellValue::Null)
        }
    }
}

// ---------------------------------------------------------------------------
// CellValue helpers
// ---------------------------------------------------------------------------

/// Extracts a numeric value from a `CellValue` for aggregation. Returns `None`
/// for Null, Bytes, and non-numeric Strings.
fn cell_numeric_value(cell: &CellValue) -> Option<f64> {
    match cell {
        CellValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        CellValue::I64(v) => Some(*v as f64),
        CellValue::F64(v) => Some(*v),
        CellValue::String(s) => s.parse::<f64>().ok(),
        CellValue::Null | CellValue::Bytes(_) => None,
    }
}

/// The display/key representation of a cell, used for lexicographic comparison
/// and pivot key extraction.
fn cell_display_key(cell: &CellValue) -> String {
    match cell {
        CellValue::Null => NULL_KEY_PLACEHOLDER.to_string(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(v) => v.to_string(),
        CellValue::F64(v) => v.to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(b) => format!("0x{}", hex_encode(b)),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02X}", b));
    }
    s
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Pure transformer that collapses a flat result set into a pivot table.
/// Returns `Err` with a blocking diagnostic, or `Ok` with the table plus any
/// non-blocking warnings.
pub fn build(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    spec: &ResultPivotSpec,
) -> Result<ResultPivotBuildResult, ResultPivotDiagnostic> {
    let (column_indices, row_indices, value_index) = spec.resolved_indices(columns)?;
    let column_index = *column_indices
        .first()
        .ok_or_else(|| missing_field("column"))?;
    let row_index = *row_indices.first().ok_or_else(|| missing_field("row"))?;

    // 2) Distinctness.
    if row_index == column_index || row_index == value_index || column_index == value_index {
        return Err(localized_diagnostic(
            "results.pivot.duplicate_axis_columns",
            &[],
        ));
    }

    let mut diagnostics = Vec::new();
    let mut saw_null_key = false;

    let mut row_keys: Vec<String> = Vec::new();
    let mut row_key_set: BTreeSet<String> = BTreeSet::new();
    let mut column_keys: Vec<String> = Vec::new();
    let mut column_key_set: BTreeSet<String> = BTreeSet::new();
    let mut cells: BTreeMap<String, BTreeMap<String, PivotAccumulator>> = BTreeMap::new();

    for row in rows {
        if row_index >= row.len() || column_index >= row.len() || value_index >= row.len() {
            continue;
        }
        let row_cell = &row[row_index];
        let column_cell = &row[column_index];
        let row_key = cell_display_key(row_cell);
        let column_key = cell_display_key(column_cell);
        if matches!(row_cell, CellValue::Null) || matches!(column_cell, CellValue::Null) {
            saw_null_key = true;
        }
        if row_key_set.insert(row_key.clone()) {
            row_keys.push(row_key.clone());
        }
        if column_key_set.insert(column_key.clone()) {
            column_keys.push(column_key.clone());
        }
        cells
            .entry(row_key)
            .or_default()
            .entry(column_key)
            .or_default()
            .append(row[value_index].clone());
    }

    if saw_null_key {
        diagnostics.push(localized_warning(
            "results.pivot.null_key_warning",
            &[("null_placeholder", NULL_KEY_PLACEHOLDER)],
        ));
    }

    // 3) Column cap.
    if column_keys.len() > MAX_PIVOT_COLUMNS {
        diagnostics.push(localized_warning(
            "results.pivot.column_cap_warning",
            &[("column_cap", &MAX_PIVOT_COLUMNS.to_string())],
        ));
        column_keys.truncate(MAX_PIVOT_COLUMNS);
    }

    // 4) Assemble output columns.
    let row_col_name = columns[row_index].name.clone();
    let mut output_columns = vec![ResultColumn {
        name: row_col_name,
        data_type: "nvarchar".to_string(),
        nullable: true,
    }];
    for ck in &column_keys {
        output_columns.push(ResultColumn {
            name: ck.clone(),
            data_type: "nvarchar".to_string(),
            nullable: true,
        });
    }

    // 5) Assemble output rows.
    let output_rows: Vec<Vec<CellValue>> = row_keys
        .iter()
        .map(|row_key| {
            let mut out_row = vec![CellValue::String(row_key.clone())];
            let row_cells = cells.get(row_key);
            for column_key in &column_keys {
                if let Some(accumulator) = row_cells.and_then(|m| m.get(column_key)) {
                    out_row.push(accumulator.result(spec.aggregate));
                } else {
                    out_row.push(CellValue::Null);
                }
            }
            out_row
        })
        .collect();

    let table = ResultPivotTable {
        columns: output_columns,
        rows: output_rows,
    };
    Ok(ResultPivotBuildResult { table, diagnostics })
}

// ---------------------------------------------------------------------------
// Hint parser (ResultViewHintParser)
// ---------------------------------------------------------------------------

/// Pure parser for result-view comment hints. Recognizes a leading
/// `-- sqlly view:pivot` directive and one or more inline
/// `-- sqlly pivot:` specs.
pub fn parse_view_hints(sql: &str) -> ResultViewHints {
    let sql = crate::sqlly_directive::unwrap_lock_blocks(sql);
    let lines: Vec<&str> = sql.split('\n').collect();

    let default_view = default_view_kind(&lines);

    let mut latest_pivot: Option<ResultPivotSpec> = None;
    let mut latest_chart: Option<crate::result_chart::ResultChartSpec> = None;
    let mut diagnostics = Vec::new();
    let mut valid_hint_count = 0;
    let mut valid_chart_count = 0;

    for line in &lines {
        if let Some(body) = pivot_comment_body(line) {
            match parse_pivot_body(body) {
                Ok(spec) => {
                    latest_pivot = Some(spec);
                    valid_hint_count += 1;
                }
                Err(diag) => diagnostics.push(diag),
            }
        }
        if let Some(body) = crate::result_chart::chart_comment_body(line) {
            match crate::result_chart::parse_chart_body(body) {
                Ok(spec) => {
                    latest_chart = Some(spec);
                    valid_chart_count += 1;
                }
                Err(diag) => diagnostics.push(diag),
            }
        }
    }

    if valid_hint_count > 1 {
        diagnostics.push(localized_warning("results.pivot.multiple_hints", &[]));
    }

    if valid_chart_count > 1 {
        diagnostics.push(localized_warning("results.chart.multiple_hints", &[]));
    }

    if default_view == ResultViewKind::Pivot && latest_pivot.is_none() {
        diagnostics.push(localized_warning("results.pivot.view_without_hint", &[]));
    }

    if default_view == ResultViewKind::Chart && latest_chart.is_none() {
        diagnostics.push(localized_warning("results.chart.view_without_hint", &[]));
    }

    ResultViewHints {
        default_view,
        pivot: latest_pivot,
        chart: latest_chart,
        diagnostics,
    }
}

fn default_view_kind(lines: &[&str]) -> ResultViewKind {
    let first_non_empty = lines.iter().find(|l| !l.trim().is_empty());
    let Some(first) = first_non_empty else {
        return ResultViewKind::Grid;
    };
    let Some(crate::sqlly_directive::Directive::View { body }) =
        crate::sqlly_directive::classify(first.trim())
    else {
        return ResultViewKind::Grid;
    };
    let value = body.trim();
    if value.eq_ignore_ascii_case("pivot") {
        ResultViewKind::Pivot
    } else if value.eq_ignore_ascii_case("chart") {
        ResultViewKind::Chart
    } else {
        ResultViewKind::Grid
    }
}

/// The `-- sqlly pivot:` body of a line that is entirely such a directive,
/// via the shared lexer. (`classify` requires the trimmed line to begin with
/// `--`, so a pivot comment trailing SQL code never matches.)
fn pivot_comment_body(line: &str) -> Option<&str> {
    match crate::sqlly_directive::classify(line.trim())? {
        crate::sqlly_directive::Directive::Pivot { body } => Some(body),
        _ => None,
    }
}

fn parse_pivot_body(body: &str) -> Result<ResultPivotSpec, ResultPivotDiagnostic> {
    let mut column_names: Option<Vec<String>> = None;
    let mut row_names: Option<Vec<String>> = None;
    let mut value_name: Option<String> = None;
    let mut aggregate: Option<ResultPivotAggregate> = None;
    let mut field_formats: BTreeMap<String, NumberFormat> = BTreeMap::new();
    let mut value_format: Option<NumberFormat> = None;
    let mut result_set_index = 0usize;

    for segment in body.split(';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some(colon_pos) = segment.find(':') else {
            return Err(localized_diagnostic(
                "results.pivot.malformed_field",
                &[("field", segment)],
            ));
        };
        let key = segment[..colon_pos].trim().to_ascii_lowercase();
        let rest = segment[colon_pos + 1..].trim();

        match key.as_str() {
            "column" => {
                column_names = Some(parse_column_names("column", rest)?);
            }
            "row" => {
                row_names = Some(parse_column_names("row", rest)?);
            }
            "value" => {
                let Some((name, aggregate_name)) = rest.rsplit_once(':') else {
                    return Err(localized_diagnostic(
                        "results.pivot.malformed_value_field",
                        &[],
                    ));
                };
                let name = parse_column_name("value", name)?;
                let Some(agg) = ResultPivotAggregate::from_str(aggregate_name.trim()) else {
                    return Err(localized_diagnostic(
                        "results.pivot.unsupported_aggregate",
                        &[("aggregate", aggregate_name.trim())],
                    ));
                };
                value_name = Some(name);
                aggregate = Some(agg);
            }
            "format" => {
                let Some((name, options)) = rest.split_once(':') else {
                    return Err(localized_diagnostic(
                        "results.pivot.malformed_format_field",
                        &[],
                    ));
                };
                let name = parse_column_name("format", name)?;
                field_formats.insert(name, parse_format_options(options)?);
            }
            "valueformat" => {
                value_format = Some(parse_format_options(rest)?);
            }
            "result" => {
                result_set_index = parse_result_number(rest).ok_or_else(|| {
                    localized_diagnostic("results.pivot.invalid_result_number", &[("value", rest)])
                })?;
            }
            _ => {
                return Err(localized_diagnostic(
                    "results.pivot.unknown_field",
                    &[("field_name", &key)],
                ));
            }
        }
    }

    // Either axis may be omitted (rows-only grouping is common), but a
    // pivot with neither has nothing to group by.
    let column_names = column_names.unwrap_or_default();
    let row_names = row_names.unwrap_or_default();
    if column_names.is_empty() && row_names.is_empty() {
        return Err(missing_field("row"));
    }
    let Some(value_name) = value_name else {
        return Err(missing_field("value"));
    };
    let Some(aggregate) = aggregate else {
        return Err(missing_field("value"));
    };

    Ok(ResultPivotSpec {
        column_names,
        row_names,
        value_name,
        aggregate,
        result_set_index,
        field_formats,
        value_format,
    })
}

/// `result:<n>` — a 1-based result set number, returned 0-based.
pub(crate) fn parse_result_number(raw: &str) -> Option<usize> {
    let raw = raw.trim();
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse::<usize>().ok()?.checked_sub(1)
}

fn parse_column_names(field: &str, raw: &str) -> Result<Vec<String>, ResultPivotDiagnostic> {
    raw.split(',')
        .map(|name| parse_column_name(field, name))
        .collect()
}

fn parse_column_name(field: &str, raw: &str) -> Result<String, ResultPivotDiagnostic> {
    let name = raw.trim().trim_matches(['[', ']', '`', '"']);
    if name.is_empty() || name.parse::<usize>().is_ok() {
        return Err(localized_diagnostic(
            "results.pivot.column_index_not_allowed",
            &[("field_role", field)],
        ));
    }
    Ok(name.to_string())
}

fn missing_field(field: &str) -> ResultPivotDiagnostic {
    localized_diagnostic(
        "results.pivot.required_field_missing",
        &[("field_role", field)],
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: "int".to_string(),
            nullable: false,
        }
    }

    fn row(vals: &[CellValue]) -> Vec<CellValue> {
        vals.to_vec()
    }

    #[test]
    fn aggregate_all_variants() {
        assert_eq!(ResultPivotAggregate::all().len(), 6);
    }

    #[test]
    fn aggregate_from_str_case_insensitive() {
        assert_eq!(
            ResultPivotAggregate::from_str("sum"),
            Some(ResultPivotAggregate::Sum)
        );
        assert_eq!(
            ResultPivotAggregate::from_str("SUM"),
            Some(ResultPivotAggregate::Sum)
        );
        assert_eq!(
            ResultPivotAggregate::from_str("count"),
            Some(ResultPivotAggregate::Count)
        );
        assert_eq!(
            ResultPivotAggregate::from_str("first"),
            Some(ResultPivotAggregate::First)
        );
        assert_eq!(
            ResultPivotAggregate::from_str("max"),
            Some(ResultPivotAggregate::Max)
        );
        assert_eq!(
            ResultPivotAggregate::from_str("min"),
            Some(ResultPivotAggregate::Min)
        );
        assert_eq!(
            ResultPivotAggregate::from_str("avg"),
            Some(ResultPivotAggregate::Avg)
        );
    }

    #[test]
    fn accumulator_count_ignores_null() {
        let mut acc = PivotAccumulator::new();
        acc.append(CellValue::I64(1));
        acc.append(CellValue::Null);
        acc.append(CellValue::I64(3));
        assert_eq!(acc.result(ResultPivotAggregate::Count), CellValue::I64(2));
    }

    #[test]
    fn accumulator_sum_numeric() {
        let mut acc = PivotAccumulator::new();
        acc.append(CellValue::I64(1));
        acc.append(CellValue::I64(2));
        acc.append(CellValue::I64(3));
        assert_eq!(acc.result(ResultPivotAggregate::Sum), CellValue::F64(6.0));
    }

    #[test]
    fn accumulator_avg_numeric() {
        let mut acc = PivotAccumulator::new();
        acc.append(CellValue::I64(2));
        acc.append(CellValue::I64(4));
        assert_eq!(acc.result(ResultPivotAggregate::Avg), CellValue::F64(3.0));
    }

    #[test]
    fn pivot_config_uses_datatable_layout() {
        let columns = vec![
            col("region"),
            col("country"),
            col("year"),
            col("quarter"),
            col("revenue"),
        ];
        let mut spec = ResultPivotSpec::new("year", "region", "revenue", ResultPivotAggregate::Avg);
        spec.row_names.push("country".into());
        spec.column_names.push("quarter".into());
        let config = spec.pivot_config(&columns).unwrap();
        assert_eq!(config.row_fields, vec![0, 1]);
        assert_eq!(config.column_fields, vec![2, 3]);
        assert_eq!(config.value_field, Some(4));
        assert_eq!(config.aggregation, AggregationFn::Avg);
    }

    #[test]
    fn first_aggregation_maps_to_native_pivot_engine() {
        // Datatable 4.3.0 added First; the interactive pivot now supports it
        // instead of falling back to the legacy flattened transformer.
        let columns = vec![col("region"), col("year"), col("name")];
        let spec = ResultPivotSpec::new("year", "region", "name", ResultPivotAggregate::First);
        let config = spec.pivot_config(&columns).expect("First is now supported");
        assert_eq!(config.aggregation, AggregationFn::First);
        // And the reverse mapping round-trips.
        let back = ResultPivotSpec::from_config(&config, &columns, 0).expect("round trips");
        assert_eq!(back.aggregate, ResultPivotAggregate::First);
    }

    #[test]
    fn accumulator_sum_empty_is_null() {
        let acc = PivotAccumulator::new();
        assert_eq!(acc.result(ResultPivotAggregate::Sum), CellValue::Null);
    }

    #[test]
    fn accumulator_first_skips_null() {
        let mut acc = PivotAccumulator::new();
        acc.append(CellValue::Null);
        acc.append(CellValue::String("a".into()));
        acc.append(CellValue::String("b".into()));
        assert_eq!(
            acc.result(ResultPivotAggregate::First),
            CellValue::String("a".into())
        );
    }

    #[test]
    fn accumulator_first_all_null_is_null() {
        let mut acc = PivotAccumulator::new();
        acc.append(CellValue::Null);
        acc.append(CellValue::Null);
        assert_eq!(acc.result(ResultPivotAggregate::First), CellValue::Null);
    }

    #[test]
    fn accumulator_max_numeric() {
        let mut acc = PivotAccumulator::new();
        acc.append(CellValue::I64(3));
        acc.append(CellValue::I64(1));
        acc.append(CellValue::I64(2));
        assert_eq!(acc.result(ResultPivotAggregate::Max), CellValue::I64(3));
    }

    #[test]
    fn accumulator_min_numeric() {
        let mut acc = PivotAccumulator::new();
        acc.append(CellValue::I64(3));
        acc.append(CellValue::I64(1));
        acc.append(CellValue::I64(2));
        assert_eq!(acc.result(ResultPivotAggregate::Min), CellValue::I64(1));
    }

    #[test]
    fn accumulator_max_lexicographic_for_strings() {
        let mut acc = PivotAccumulator::new();
        acc.append(CellValue::String("banana".into()));
        acc.append(CellValue::String("apple".into()));
        acc.append(CellValue::String("cherry".into()));
        assert_eq!(
            acc.result(ResultPivotAggregate::Max),
            CellValue::String("cherry".into())
        );
    }

    #[test]
    fn accumulator_min_lexicographic_for_strings() {
        let mut acc = PivotAccumulator::new();
        acc.append(CellValue::String("banana".into()));
        acc.append(CellValue::String("apple".into()));
        acc.append(CellValue::String("cherry".into()));
        assert_eq!(
            acc.result(ResultPivotAggregate::Min),
            CellValue::String("apple".into())
        );
    }

    #[test]
    fn accumulator_max_all_null_is_null() {
        let mut acc = PivotAccumulator::new();
        acc.append(CellValue::Null);
        assert_eq!(acc.result(ResultPivotAggregate::Max), CellValue::Null);
    }

    #[test]
    fn build_basic_pivot() {
        let columns = vec![col("region"), col("product"), col("sales")];
        let rows = vec![
            row(&[
                CellValue::String("West".into()),
                CellValue::String("A".into()),
                CellValue::I64(10),
            ]),
            row(&[
                CellValue::String("East".into()),
                CellValue::String("A".into()),
                CellValue::I64(20),
            ]),
            row(&[
                CellValue::String("West".into()),
                CellValue::String("B".into()),
                CellValue::I64(5),
            ]),
        ];
        let spec = ResultPivotSpec::new("product", "region", "sales", ResultPivotAggregate::Sum);
        let result = build(&columns, &rows, &spec).unwrap();
        assert_eq!(result.table.columns.len(), 3); // region + A + B
        assert_eq!(result.table.columns[0].name, "region");
        assert_eq!(result.table.columns[1].name, "A");
        assert_eq!(result.table.columns[2].name, "B");
        assert_eq!(result.table.rows.len(), 2); // West + East
        // West: A=10, B=5
        assert_eq!(result.table.rows[0][0], CellValue::String("West".into()));
        assert_eq!(result.table.rows[0][1], CellValue::F64(10.0));
        assert_eq!(result.table.rows[0][2], CellValue::F64(5.0));
        // East: A=20, B=null
        assert_eq!(result.table.rows[1][0], CellValue::String("East".into()));
        assert_eq!(result.table.rows[1][1], CellValue::F64(20.0));
        assert_eq!(result.table.rows[1][2], CellValue::Null);
    }

    #[test]
    fn build_count_aggregate() {
        let columns = vec![col("region"), col("product"), col("sales")];
        let rows = vec![
            row(&[
                CellValue::String("West".into()),
                CellValue::String("A".into()),
                CellValue::I64(10),
            ]),
            row(&[
                CellValue::String("West".into()),
                CellValue::String("A".into()),
                CellValue::I64(20),
            ]),
        ];
        let spec = ResultPivotSpec::new("product", "region", "sales", ResultPivotAggregate::Count);
        let result = build(&columns, &rows, &spec).unwrap();
        assert_eq!(result.table.rows[0][1], CellValue::I64(2));
    }

    #[test]
    fn build_missing_column_error() {
        let columns = vec![col("a"), col("b")];
        let rows: Vec<Vec<CellValue>> = vec![];
        let spec = ResultPivotSpec::new("missing", "a", "b", ResultPivotAggregate::Sum);
        let err = build(&columns, &rows, &spec).unwrap_err();
        assert_eq!(err.severity, DiagnosticSeverity::Error);
        assert!(err.message.contains("was not found"));
    }

    #[test]
    fn build_non_distinct_columns_error() {
        let columns = vec![col("a"), col("b"), col("c")];
        let rows: Vec<Vec<CellValue>> = vec![];
        let spec = ResultPivotSpec::new("a", "a", "b", ResultPivotAggregate::Sum);
        let err = build(&columns, &rows, &spec).unwrap_err();
        assert!(err.message.contains("three different columns"));
    }

    #[test]
    fn build_null_key_produces_warning() {
        let columns = vec![col("region"), col("product"), col("sales")];
        let rows = vec![row(&[
            CellValue::Null,
            CellValue::String("A".into()),
            CellValue::I64(10),
        ])];
        let spec = ResultPivotSpec::new("product", "region", "sales", ResultPivotAggregate::Sum);
        let result = build(&columns, &rows, &spec).unwrap();
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("null values"))
        );
    }

    #[test]
    fn build_column_cap_truncation() {
        let columns = vec![col("row"), col("col"), col("val")];
        let mut rows = Vec::new();
        for i in 0..300 {
            rows.push(row(&[
                CellValue::String("R".into()),
                CellValue::String(format!("C{}", i)),
                CellValue::I64(i as i64),
            ]));
        }
        let spec = ResultPivotSpec::new("col", "row", "val", ResultPivotAggregate::Sum);
        let result = build(&columns, &rows, &spec).unwrap();
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.message.contains("column cap"))
        );
        assert_eq!(result.table.columns.len(), MAX_PIVOT_COLUMNS + 1); // row + cap columns
    }

    #[test]
    fn build_skips_rows_with_missing_indices() {
        let columns = vec![col("a"), col("b"), col("c")];
        let rows = vec![row(&[CellValue::I64(1), CellValue::I64(2)])];
        let spec = ResultPivotSpec::new("b", "a", "c", ResultPivotAggregate::Sum);
        let result = build(&columns, &rows, &spec).unwrap();
        assert!(result.table.rows.is_empty());
    }

    #[test]
    fn parse_view_hints_empty_sql() {
        let hints = parse_view_hints("");
        assert_eq!(hints.default_view, ResultViewKind::Grid);
        assert!(hints.pivot.is_none());
    }

    #[test]
    fn parse_view_hints_ignores_unnamespaced_comments() {
        let hints = parse_view_hints(
            "--view:pivot\n-- pivot: row:Region, column:Year, value:Revenue:sum\nSELECT 1",
        );
        assert_eq!(hints.default_view, ResultViewKind::Grid);
        assert!(hints.pivot.is_none());
    }

    #[test]
    fn parse_view_hints_view_pivot_directive() {
        let sql = "-- sqlly view:pivot\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert_eq!(hints.default_view, ResultViewKind::Pivot);
    }

    #[test]
    fn parse_view_hints_view_grid_directive() {
        let sql = "-- sqlly view:grid\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert_eq!(hints.default_view, ResultViewKind::Grid);
    }

    #[test]
    fn parse_view_hints_pivot_spec() {
        let sql =
            "-- sqlly pivot: column:Year,Quarter; row:Region,Country; value:Revenue:sum\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert_eq!(hints.default_view, ResultViewKind::Grid);
        let spec = hints.pivot.unwrap();
        assert_eq!(spec.column_names, ["Year", "Quarter"]);
        assert_eq!(spec.row_names, ["Region", "Country"]);
        assert_eq!(spec.value_name, "Revenue");
        assert_eq!(spec.aggregate, ResultPivotAggregate::Sum);
    }

    #[test]
    fn parse_view_hints_pivot_spec_case_insensitive() {
        let sql = "-- SQLLY PIVOT: COLUMN:Year; ROW:Region; VALUE:Revenue:MAX\nSELECT 1";
        let hints = parse_view_hints(sql);
        let spec = hints.pivot.unwrap();
        assert_eq!(spec.column_names, ["Year"]);
        assert_eq!(spec.aggregate, ResultPivotAggregate::Max);
    }

    #[test]
    fn parse_view_hints_last_valid_wins() {
        let sql = "-- sqlly pivot: column:Year; row:Region; value:Revenue:sum\n-- sqlly pivot: column:Region; row:Year; value:Revenue:count\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert!(
            hints
                .diagnostics
                .iter()
                .any(|d| d.message.contains("multiple"))
        );
        let spec = hints.pivot.unwrap();
        assert_eq!(spec.column_names, ["Region"]);
        assert_eq!(spec.row_names, ["Year"]);
        assert_eq!(spec.aggregate, ResultPivotAggregate::Count);
    }

    #[test]
    fn parse_view_hints_malformed_field_error() {
        let sql = "-- sqlly pivot: column:Year; row:Region; value:Revenue\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert!(hints.pivot.is_none());
        assert!(
            hints
                .diagnostics
                .iter()
                .any(|d| d.message.contains("value field"))
        );
    }

    #[test]
    fn parse_view_hints_missing_field_error() {
        let sql = "-- sqlly pivot: column:Year; row:Region\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert!(hints.pivot.is_none());
        assert!(
            hints
                .diagnostics
                .iter()
                .any(|d| d.message.contains("missing"))
        );
    }

    #[test]
    fn parse_view_hints_unknown_aggregate_error() {
        let sql = "-- sqlly pivot: column:Year; row:Region; value:Revenue:median\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert!(hints.pivot.is_none());
        assert!(
            hints
                .diagnostics
                .iter()
                .any(|d| d.message.contains("not supported"))
        );
    }

    #[test]
    fn parse_view_hints_unknown_field_error() {
        let sql = "-- sqlly pivot: column:Year; row:Region; value:Revenue:sum; foo:Bar\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert!(
            hints
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unknown field"))
        );
    }

    #[test]
    fn parse_view_hints_rejects_column_indexes() {
        let sql = "-- sqlly pivot: column:Year; row:0; value:Revenue:sum\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert!(
            hints
                .diagnostics
                .iter()
                .any(|d| d.message.contains("not use a column index"))
        );
    }

    #[test]
    fn parse_view_hints_view_pivot_no_pivot_hint_warning() {
        let sql = "-- sqlly view:pivot\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert_eq!(hints.default_view, ResultViewKind::Pivot);
        assert!(
            hints
                .diagnostics
                .iter()
                .any(|d| d.message.contains("no -- sqlly pivot: hint"))
        );
    }

    #[test]
    fn parse_view_hints_non_comment_first_line_is_grid() {
        let sql = "SELECT 1";
        let hints = parse_view_hints(sql);
        assert_eq!(hints.default_view, ResultViewKind::Grid);
    }

    #[test]
    fn parse_view_hints_malformed_field_no_colon() {
        let sql = "-- sqlly pivot: column:Year; row:Region; value:Revenue:sum; abc\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert!(
            hints
                .diagnostics
                .iter()
                .any(|d| d.message.contains("malformed field"))
        );
    }

    #[test]
    fn updates_query_comment_from_current_native_pivot_config() {
        let columns = vec![
            col("Region"),
            col("Country"),
            col("Year"),
            col("Quarter"),
            col("Revenue"),
        ];
        let config = PivotConfig {
            row_fields: vec![0, 1],
            column_fields: vec![2, 3],
            value_field: Some(4),
            aggregation: AggregationFn::Sum,
            ..PivotConfig::default()
        };
        let spec = ResultPivotSpec::from_config(&config, &columns, 0).expect("ready pivot");
        let updated = update_or_insert_pivot_comment(
            "-- sqlly view:pivot\n-- sqlly pivot: row:Old; column:Old2; value:Old3:count\nSELECT 1",
            &spec,
        );
        assert!(updated.contains(
            "-- sqlly pivot: row:Region,Country; column:Year,Quarter; value:Revenue:sum"
        ));
        assert!(!updated.contains("row:Old"));
    }

    #[test]
    fn parse_view_hints_pivot_field_and_value_formats() {
        let sql = "-- sqlly pivot: row:Region; column:Year; value:Revenue:sum; \
                   format:Region:align=left,decimals=0,red=false; \
                   format:Year:default; \
                   valueformat:parens=true,thousands=false\nSELECT 1";
        let hints = parse_view_hints(sql);
        assert!(hints.diagnostics.is_empty(), "{:?}", hints.diagnostics);
        let spec = hints.pivot.unwrap();
        assert_eq!(
            spec.field_formats.get("Region"),
            Some(&NumberFormat {
                alignment: TextAlignment::Left,
                decimals: 0,
                show_negative_red: false,
                ..NumberFormat::default()
            })
        );
        assert_eq!(
            spec.field_formats.get("Year"),
            Some(&NumberFormat::default())
        );
        assert_eq!(
            spec.value_format,
            Some(NumberFormat {
                negative_parentheses: true,
                thousands_separator: false,
                ..NumberFormat::default()
            })
        );
    }

    #[test]
    fn directive_round_trips_formats() {
        let mut spec = ResultPivotSpec::new("Year", "Region", "Revenue", ResultPivotAggregate::Sum);
        spec.field_formats.insert(
            "Region".to_string(),
            NumberFormat {
                alignment: TextAlignment::Center,
                decimals: 3,
                ..NumberFormat::default()
            },
        );
        spec.field_formats
            .insert("Year".to_string(), NumberFormat::default());
        spec.value_format = Some(NumberFormat {
            show_negative_red: false,
            negative_parentheses: true,
            ..NumberFormat::default()
        });
        let directive = spec.directive();
        assert_eq!(
            directive,
            "-- sqlly pivot: row:Region; column:Year; value:Revenue:sum; \
             format:Region:align=center,decimals=3; format:Year:default; \
             valueformat:red=false,parens=true"
        );
        let reparsed = parse_view_hints(&format!("{directive}\nSELECT 1"))
            .pivot
            .unwrap();
        assert_eq!(reparsed, spec);
    }

    #[test]
    fn from_config_supports_rows_only_pivot() {
        // A rows-only n-tier pivot (no Columns fields) is the everyday
        // "group by several columns" layout; the sidebar's save button must
        // still produce a directive for it.
        let columns = vec![col("Region"), col("Product"), col("Revenue")];
        let config = PivotConfig {
            row_fields: vec![0, 1],
            column_fields: vec![],
            value_field: Some(2),
            aggregation: AggregationFn::Sum,
            ..PivotConfig::default()
        };
        let spec = ResultPivotSpec::from_config(&config, &columns, 0).unwrap();
        let directive = spec.directive();
        assert_eq!(
            directive,
            "-- sqlly pivot: row:Region,Product; value:Revenue:sum"
        );
        let reparsed = parse_view_hints(&format!("{directive}\nSELECT 1"))
            .pivot
            .unwrap();
        assert_eq!(reparsed, spec);
        // And it maps back onto the interactive pivot layout.
        let round = reparsed.pivot_config(&columns).unwrap();
        assert_eq!(round.row_fields, vec![0, 1]);
        assert!(round.column_fields.is_empty());
        assert_eq!(round.value_field, Some(2));
    }

    #[test]
    fn from_config_supports_columns_only_pivot() {
        let columns = vec![col("Year"), col("Revenue")];
        let config = PivotConfig {
            row_fields: vec![],
            column_fields: vec![0],
            value_field: Some(1),
            aggregation: AggregationFn::Avg,
            ..PivotConfig::default()
        };
        let spec = ResultPivotSpec::from_config(&config, &columns, 0).unwrap();
        let directive = spec.directive();
        assert_eq!(directive, "-- sqlly pivot: column:Year; value:Revenue:avg");
        let reparsed = parse_view_hints(&format!("{directive}\nSELECT 1"))
            .pivot
            .unwrap();
        assert_eq!(reparsed, spec);
    }

    #[test]
    fn from_config_rejects_pivot_with_no_grouping_axis() {
        let columns = vec![col("Revenue")];
        let config = PivotConfig {
            row_fields: vec![],
            column_fields: vec![],
            value_field: Some(0),
            ..PivotConfig::default()
        };
        assert!(ResultPivotSpec::from_config(&config, &columns, 0).is_none());
    }

    #[test]
    fn parse_rejects_directive_missing_both_axes() {
        let hints = parse_view_hints("-- sqlly pivot: value:V:sum\nSELECT 1");
        assert!(hints.pivot.is_none());
        assert!(
            hints
                .diagnostics
                .iter()
                .any(|d| d.message.contains("required field"))
        );
    }

    #[test]
    fn pivot_config_carries_formats_and_drops_unknown_format_columns() {
        let columns = vec![col("region"), col("year"), col("revenue")];
        let mut spec = ResultPivotSpec::new("year", "region", "revenue", ResultPivotAggregate::Sum);
        let region_format = NumberFormat {
            alignment: TextAlignment::Left,
            ..NumberFormat::default()
        };
        spec.field_formats
            .insert("Region".to_string(), region_format);
        spec.field_formats
            .insert("Missing".to_string(), NumberFormat::default());
        spec.value_format = Some(NumberFormat {
            decimals: 4,
            ..NumberFormat::default()
        });
        let config = spec.pivot_config(&columns).unwrap();
        assert_eq!(config.field_formats.len(), 1);
        assert_eq!(config.field_formats.get(&0), Some(&region_format));
        assert_eq!(config.value_format, spec.value_format);
    }

    #[test]
    fn from_config_maps_format_indices_to_names() {
        let columns = vec![col("Region"), col("Year"), col("Revenue")];
        let format = NumberFormat {
            decimals: 0,
            ..NumberFormat::default()
        };
        let config = PivotConfig {
            row_fields: vec![0],
            column_fields: vec![1],
            value_field: Some(2),
            aggregation: AggregationFn::Sum,
            value_format: Some(NumberFormat::default()),
            field_formats: [(1, format)].into_iter().collect(),
            ..PivotConfig::default()
        };
        let spec = ResultPivotSpec::from_config(&config, &columns, 0).unwrap();
        assert_eq!(spec.field_formats.get("Year"), Some(&format));
        assert_eq!(spec.value_format, Some(NumberFormat::default()));
        assert!(spec.directive().contains("format:Year:decimals=0"));
        assert!(spec.directive().contains("valueformat:default"));
    }

    #[test]
    fn parse_view_hints_rejects_malformed_format_options() {
        for (sql, needle) in [
            (
                "-- sqlly pivot: row:R; column:C; value:V:sum; format:R:sparkles=true",
                "unknown format option",
            ),
            (
                "-- sqlly pivot: row:R; column:C; value:V:sum; format:R:align=diagonal",
                "align must be",
            ),
            (
                "-- sqlly pivot: row:R; column:C; value:V:sum; format:R:decimals=lots",
                "decimals must be",
            ),
            (
                "-- sqlly pivot: row:R; column:C; value:V:sum; format:R:red=maybe",
                "red must be",
            ),
            (
                "-- sqlly pivot: row:R; column:C; value:V:sum; format:R:",
                "format options are missing",
            ),
            (
                "-- sqlly pivot: row:R; column:C; value:V:sum; format:R",
                "format field must be",
            ),
            (
                "-- sqlly pivot: row:R; column:C; value:V:sum; valueformat:bold",
                "malformed format option",
            ),
        ] {
            let hints = parse_view_hints(&format!("{sql}\nSELECT 1"));
            assert!(hints.pivot.is_none(), "hint should be rejected: {sql}");
            assert!(
                hints.diagnostics.iter().any(|d| d.message.contains(needle)),
                "expected {needle:?} in diagnostics for {sql}: {:?}",
                hints.diagnostics
            );
        }
    }

    #[test]
    #[allow(clippy::approx_constant)]
    fn cell_numeric_value_extracts_numbers() {
        assert_eq!(cell_numeric_value(&CellValue::Bool(true)), Some(1.0));
        assert_eq!(cell_numeric_value(&CellValue::Bool(false)), Some(0.0));
        assert_eq!(cell_numeric_value(&CellValue::I64(42)), Some(42.0));
        assert_eq!(cell_numeric_value(&CellValue::F64(3.14)), Some(3.14));
        assert_eq!(
            cell_numeric_value(&CellValue::String("42".into())),
            Some(42.0)
        );
        assert_eq!(cell_numeric_value(&CellValue::String("hello".into())), None);
        assert_eq!(cell_numeric_value(&CellValue::Null), None);
        assert_eq!(cell_numeric_value(&CellValue::Bytes(vec![1, 2])), None);
    }

    #[test]
    fn cell_display_key_null_uses_placeholder() {
        assert_eq!(cell_display_key(&CellValue::Null), NULL_KEY_PLACEHOLDER);
    }

    #[test]
    fn cell_display_key_string_uses_value() {
        assert_eq!(
            cell_display_key(&CellValue::String("hello".into())),
            "hello"
        );
    }
}
