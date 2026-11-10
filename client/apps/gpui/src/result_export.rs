//! Result-set export — clipboard copy and file-save formats.
//!
//! Port of `SQLLYExports/ResultSetExporter.swift`. Produces TSV, CSV,
//! JSON, markdown, HTML, XML, SQL INSERT, temp-table SQL, and SQL VALUES
//! from a `ResultSet`. All functions are pure and take `&[ResultColumn]`
//! + `&[Vec<CellValue>]` so they can be unit-tested without any GPUI types.
//!
//! XLSX export lives in the sibling `result_export_xlsx` module.

use sqlly_core::{CellValue, ResultColumn};

use crate::result_cell_format::numeric_text_in_column;
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType, LogicalType};

use crate::export_options::{ExportOptions, apply_line_breaks};

/// Maximum rows materialized for spill-backed copy and file export.
///
/// Same bound as snapshots (`result_snapshots::SNAPSHOT_MAX_ROWS` aliases this
/// constant). Charts stay at `sqlly_datatable::MAX_SOURCE_ROWS` (10,000). Clipboard
/// and every export format share this cap; streaming a spill file into each
/// writer would require rewriting those formats.
pub const EXPORT_MAX_ROWS: usize = 100_000;

/// User-facing notice when a spill-backed copy or save stopped at
/// [`EXPORT_MAX_ROWS`].
pub fn spill_export_truncation_message(exported: usize, total: usize) -> String {
    crate::localization::format(
        "results.export.spill_truncation",
        &[
            ("exported_rows", &exported.to_string()),
            ("total_rows", &total.to_string()),
        ],
    )
}

/// What the Export menu reports after a text format lands on the clipboard.
/// Pure so it can be unit-tested (per AGENTS.md).
pub fn export_copied_message(format_label: &str, rows: usize) -> String {
    let key = if rows == 1 {
        "results.export.copied_one_row"
    } else {
        "results.export.copied_multiple_rows"
    };
    crate::localization::format(
        key,
        &[
            ("row_count", &crate::license::format_thousands(rows as u64)),
            ("format_name", &format_label.to_string()),
        ],
    )
}

/// Computes the 1-based Excel column label for a 1-based column index
/// (1 → "A", 26 → "Z", 27 → "AA"). Pure, no I/O.
pub fn excel_column_label(index: usize) -> String {
    let mut remaining = std::cmp::max(1, index);
    let mut label = String::new();
    while remaining > 0 {
        let digit = (remaining - 1) % 26;
        label.insert(0, char::from_u32(65 + digit as u32).unwrap());
        remaining = (remaining - 1) / 26;
    }
    label
}

/// Escapes XML special characters (`&` `<` `>` `"` `'`).
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("\u{26}amp;"),
            '<' => out.push_str("\u{26}lt;"),
            '>' => out.push_str("\u{26}gt;"),
            '"' => out.push_str("\u{26}quot;"),
            '\'' => out.push_str("\u{26}apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// CSV field escaping: wraps in double-quotes and doubles embedded quotes
/// if the value contains a comma, quote, newline, or carriage return.
#[allow(dead_code)] // public API wrapper; stream path uses `csv_field_with`
pub fn csv_field(value: &CellValue) -> String {
    csv_field_with(
        value,
        &ExportOptions::default_for(crate::results::ExportFormat::Csv),
    )
}

/// CSV field escaping with export options applied before quoting. When
/// `opts.formula_safe` is on, a text cell a spreadsheet would evaluate as a
/// formula is written quoted with a leading apostrophe (`"'=1+1"`); the
/// guard never touches numeric, boolean, binary, or NULL cells.
pub(crate) fn csv_field_with(value: &CellValue, opts: &ExportOptions) -> String {
    let text = cell_display_with(value, opts);
    let guarded = formula_guard_applies(value, opts, &text);
    if guarded
        || text.contains(',')
        || text.contains('"')
        || text.contains('\n')
        || text.contains('\r')
    {
        let escaped = text.replace('"', "\"\"");
        if guarded {
            format!(
                "\"{}{}\"",
                sqlly_core::formula_guard::FORMULA_GUARD_PREFIX,
                escaped
            )
        } else {
            format!("\"{}\"", escaped)
        }
    } else {
        text
    }
}

/// TSV field: the display text, with the formula guard prefixed to a text
/// cell a spreadsheet would evaluate. TSV has no quoting, so the apostrophe
/// is the whole guard.
pub(crate) fn tsv_field_with(value: &CellValue, opts: &ExportOptions) -> String {
    let text = cell_display_with(value, opts);
    if formula_guard_applies(value, opts, &text) {
        sqlly_core::formula_guard::guard_text(&text).into_owned()
    } else {
        text
    }
}

/// True when `value` is a *text* cell whose rendered `text` must be guarded
/// under `opts`. The rendered text (after NULL and line-break handling) is
/// judged rather than the raw string, so a leading tab replaced by a space
/// is no longer a trigger and a custom NULL token is never guarded.
fn formula_guard_applies(value: &CellValue, opts: &ExportOptions, text: &str) -> bool {
    opts.formula_safe
        && matches!(value, CellValue::String(_))
        && sqlly_core::formula_guard::needs_formula_guard(text)
}

/// Renders a `CellValue` as its display text for export.
pub fn cell_display(value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(v) => v.to_string(),
        CellValue::F64(v) => {
            if v.is_finite() {
                v.to_string()
            } else {
                "NULL".to_string()
            }
        }
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(b) => format!(
            "0x{}",
            b.iter().map(|x| format!("{:02X}", x)).collect::<String>()
        ),
    }
}

/// Renders a `CellValue` for export with NULL text and line-break handling.
pub fn cell_display_with(value: &CellValue, opts: &ExportOptions) -> String {
    match value {
        CellValue::Null => opts.null_text().to_string(),
        CellValue::String(s) => apply_line_breaks(s, opts.line_breaks).into_owned(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(v) => v.to_string(),
        CellValue::F64(v) => {
            if v.is_finite() {
                v.to_string()
            } else {
                opts.null_text().to_string()
            }
        }
        CellValue::Bytes(b) => format!(
            "0x{}",
            b.iter().map(|x| format!("{:02X}", x)).collect::<String>()
        ),
    }
}

/// Tab-separated values with the format's default options (formula guard
/// on). Production paths use `tsv_with` so the user's preference applies.
#[cfg_attr(not(test), allow(dead_code))]
pub fn tsv(columns: &[ResultColumn], rows: &[Vec<CellValue>], include_headers: bool) -> String {
    tsv_with(
        columns,
        rows,
        &ExportOptions {
            header_row: include_headers,
            ..ExportOptions::default_for(crate::results::ExportFormat::Tsv)
        },
    )
}

/// TSV with export options.
pub fn tsv_with(columns: &[ResultColumn], rows: &[Vec<CellValue>], opts: &ExportOptions) -> String {
    let mut lines = Vec::new();
    if opts.header_row {
        let header = columns
            .iter()
            .map(|c| tsv_field_with(&CellValue::String(c.name.clone()), opts))
            .collect::<Vec<_>>()
            .join("\t");
        lines.push(header);
    }
    for row in rows {
        let line = row
            .iter()
            .map(|c| tsv_field_with(c, opts))
            .collect::<Vec<_>>()
            .join("\t");
        lines.push(line);
    }
    lines.join("\n")
}

/// Comma-separated values with standard RFC 4180 quoting.
pub fn csv(columns: &[ResultColumn], rows: &[Vec<CellValue>], include_headers: bool) -> String {
    csv_with(
        columns,
        rows,
        &ExportOptions {
            header_row: include_headers,
            ..ExportOptions::default_for(crate::results::ExportFormat::Csv)
        },
    )
}

/// CSV with export options.
pub fn csv_with(columns: &[ResultColumn], rows: &[Vec<CellValue>], opts: &ExportOptions) -> String {
    let mut lines = Vec::new();
    if opts.header_row {
        let header = columns
            .iter()
            .map(|c| csv_field_with(&CellValue::String(c.name.clone()), opts))
            .collect::<Vec<_>>()
            .join(",");
        lines.push(header);
    }
    for row in rows {
        let line = row
            .iter()
            .map(|c| csv_field_with(c, opts))
            .collect::<Vec<_>>()
            .join(",");
        lines.push(line);
    }
    lines.join("\n")
}

#[allow(dead_code)]
/// Excel-compatible CSV: UTF-8 BOM + standard CSV.
pub fn excel_compatible_csv(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    format!("\u{FEFF}{}", csv(columns, rows, true))
}

/// Compact JSON object for one row, keyed by `names` from
/// [`unique_column_names`]: repeated column names get `_2`, `_3` suffixes (as
/// in the SQL exports), since a parser keeps only the last of duplicate keys.
pub(crate) fn json_object_named(
    columns: &[ResultColumn],
    names: &[String],
    row: &[CellValue],
) -> String {
    let mut pairs = Vec::with_capacity(columns.len());
    for (i, col) in columns.iter().enumerate() {
        let key = json_escape_string(names.get(i).map_or(col.name.as_str(), String::as_str));
        let val = match row.get(i) {
            Some(CellValue::Null) | None => "null".to_string(),
            Some(CellValue::Bool(b)) => b.to_string(),
            Some(CellValue::I64(v)) => v.to_string(),
            Some(CellValue::F64(v)) => {
                if v.is_finite() {
                    v.to_string()
                } else {
                    "null".to_string()
                }
            }
            // Decimal/money (and every value on the PostgreSQL and MySQL
            // text protocols) arrive as text: in a numeric column a plain
            // numeric literal is written as a JSON number, digits intact.
            Some(CellValue::String(s)) => match numeric_text_in_column(Some(col), s) {
                Some(number) => number.to_string(),
                None => format!("\"{}\"", json_escape_string(s)),
            },
            Some(CellValue::Bytes(b)) => {
                format!(
                    "\"{}\"",
                    json_escape_string(&format!(
                        "0x{}",
                        b.iter().map(|x| format!("{:02X}", x)).collect::<String>()
                    ))
                )
            }
        };
        pairs.push(format!("\"{}\":{}", key, val));
    }
    format!("{{{}}}", pairs.join(","))
}

/// JSON array of objects (one object per row, keyed by column name).
pub fn json(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let names = unique_column_names(columns);
    let objects: Vec<String> = rows
        .iter()
        .map(|row| json_object_named(columns, &names, row))
        .collect();
    format!("[{}]", objects.join(","))
}

/// JSON Lines: one compact JSON object per row, `\n`-separated, no trailing newline.
pub fn json_lines(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let names = unique_column_names(columns);
    rows.iter()
        .map(|row| json_object_named(columns, &names, row))
        .collect::<Vec<_>>()
        .join("\n")
}

fn json_escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Markdown table with a separator row.
#[allow(dead_code)] // public API wrapper; file export uses `markdown_with`
pub fn markdown(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    include_headers: bool,
) -> String {
    markdown_with(
        columns,
        rows,
        &ExportOptions {
            header_row: include_headers,
            ..ExportOptions::default_for(crate::results::ExportFormat::Markdown)
        },
    )
}

/// Markdown table with export options.
pub fn markdown_with(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    opts: &ExportOptions,
) -> String {
    if columns.is_empty() {
        return String::new();
    }
    let mut lines = Vec::new();
    if opts.header_row {
        let header = format!(
            "| {} |",
            columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(" | ")
        );
        let separator = format!(
            "| {} |",
            columns
                .iter()
                .map(|_| "---")
                .collect::<Vec<_>>()
                .join(" | ")
        );
        lines.push(header);
        lines.push(separator);
    }
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .map(|c| cell_display_with(c, opts))
            .map(|s| s.replace('|', "\\|"))
            .collect();
        lines.push(format!("| {} |", cells.join(" | ")));
    }
    lines.join("\n")
}

/// HTML `<table>` with header row.
#[allow(dead_code)] // public API wrapper; file export uses `html_with`
pub fn html(columns: &[ResultColumn], rows: &[Vec<CellValue>], include_headers: bool) -> String {
    html_with(
        columns,
        rows,
        &ExportOptions {
            header_row: include_headers,
            ..ExportOptions::default_for(crate::results::ExportFormat::Html)
        },
    )
}

/// HTML `<table>` with export options.
pub fn html_with(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    opts: &ExportOptions,
) -> String {
    let mut out = String::from("<table>\n");
    if opts.header_row {
        out.push_str("  <thead>\n    <tr>\n");
        for col in columns {
            out.push_str(&format!("      <th>{}</th>\n", xml_escape(&col.name)));
        }
        out.push_str("    </tr>\n  </thead>\n");
    }
    out.push_str("  <tbody>\n");
    for row in rows {
        out.push_str("    <tr>\n");
        for cell in row {
            let text = xml_escape(&cell_display_with(cell, opts));
            out.push_str(&format!("      <td>{}</td>\n", text));
        }
        out.push_str("    </tr>\n");
    }
    out.push_str("  </tbody>\n</table>");
    out
}

/// SQL `INSERT INTO [table] (...) VALUES (...)` statements, one per row.
#[cfg(test)]
pub fn insert_statements(columns: &[ResultColumn], rows: &[Vec<CellValue>], table: &str) -> String {
    insert_statements_for(DatabaseType::SqlServer, columns, rows, table)
}

/// SQL INSERT export with the engine's identifier quoting and literal forms;
/// SQL Server output is byte-identical to the legacy generator.
pub fn insert_statements_for(
    db: DatabaseType,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    table: &str,
) -> String {
    if columns.is_empty() || rows.is_empty() {
        return String::new();
    }
    let names = unique_column_names(columns);
    let col_list = names
        .iter()
        .map(|n| escape_identifier_for(db, n))
        .collect::<Vec<_>>()
        .join(", ");
    let table_escaped = escape_multipart_identifier_for(db, table);
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(format!("INSERT INTO {} ({})", table_escaped, col_list));
    lines.push("VALUES".to_string());
    for (i, row) in rows.iter().enumerate() {
        let values: Vec<String> = (0..names.len())
            .map(|ci| {
                sql_literal_in_column(db, columns.get(ci), row.get(ci).unwrap_or(&CellValue::Null))
            })
            .collect();
        let suffix = if i == rows.len() - 1 { ";" } else { "," };
        lines.push(format!("    ({}){}", values.join(", "), suffix));
    }
    lines.join("\n")
}

/// Why `update_statements_for` could not render a script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateRenderError {
    /// A declared key column is not in the projected result.
    MissingKey(String),
    /// Every projected column is a key, so there is nothing to SET.
    NoNonKeyColumns,
}

impl std::fmt::Display for UpdateRenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateRenderError::MissingKey(name) => f.write_str(&crate::localization::format(
                "results.export.update_missing_key",
                &[("column_name", name)],
            )),
            UpdateRenderError::NoNonKeyColumns => f.write_str(&crate::localization::text(
                "results.export.update_only_key_columns",
            )),
        }
    }
}

/// `IN (v1, v2, …)` for one column. Nulls are dropped; when `distinct`, duplicates
/// are removed preserving first occurrence. Empty input → `IN (NULL)` so the pasted
/// predicate matches nothing instead of failing to parse. Prefix is the quoted
/// column name so the result is a complete predicate: `[Id] IN (1, 2, 3)`.
pub fn sql_in_list_for(
    db: DatabaseType,
    column: &ResultColumn,
    values: &[CellValue],
    distinct: bool,
) -> String {
    let quoted = escape_identifier_for(db, &column.name);
    let mut literals = Vec::new();
    if distinct {
        let mut seen = std::collections::HashSet::new();
        for value in values {
            if matches!(value, CellValue::Null) {
                continue;
            }
            let lit = sql_literal_in_column(db, Some(column), value);
            if seen.insert(lit.clone()) {
                literals.push(lit);
            }
        }
    } else {
        for value in values {
            if matches!(value, CellValue::Null) {
                continue;
            }
            literals.push(sql_literal_in_column(db, Some(column), value));
        }
    }
    if literals.is_empty() {
        format!("{quoted} IN (NULL)")
    } else {
        format!("{quoted} IN ({})", literals.join(", "))
    }
}

/// One `UPDATE <table> SET <non-key cols> = <literal> WHERE <key cols> = <literal>;`
/// per row. Returns `Err(UpdateRenderError::MissingKey(name))` when a key column is
/// not present in `columns`, `Err(UpdateRenderError::NoNonKeyColumns)` when every
/// projected column is a key. Key predicates use `IS NULL` for null key values.
pub fn update_statements_for(
    db: DatabaseType,
    table: &str,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    key_columns: &[String],
) -> Result<String, UpdateRenderError> {
    let mut key_indexes = Vec::with_capacity(key_columns.len());
    for key in key_columns {
        let idx = columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(key))
            .ok_or_else(|| UpdateRenderError::MissingKey(key.clone()))?;
        key_indexes.push(idx);
    }
    let set_indexes: Vec<usize> = (0..columns.len())
        .filter(|i| !key_indexes.contains(i))
        .collect();
    if set_indexes.is_empty() {
        return Err(UpdateRenderError::NoNonKeyColumns);
    }
    let table_escaped = escape_multipart_identifier_for(db, table);
    let mut statements = Vec::with_capacity(rows.len());
    for row in rows {
        let set_clause = set_indexes
            .iter()
            .map(|&i| {
                let col = escape_identifier_for(db, &columns[i].name);
                let val = sql_literal_in_column(
                    db,
                    columns.get(i),
                    row.get(i).unwrap_or(&CellValue::Null),
                );
                format!("{col} = {val}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let where_clause = key_indexes
            .iter()
            .map(|&i| {
                let col = escape_identifier_for(db, &columns[i].name);
                let value = row.get(i).unwrap_or(&CellValue::Null);
                if matches!(value, CellValue::Null) {
                    format!("{col} IS NULL")
                } else {
                    format!(
                        "{col} = {}",
                        sql_literal_in_column(db, columns.get(i), value)
                    )
                }
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        statements.push(format!(
            "UPDATE {table_escaped} SET {set_clause} WHERE {where_clause};"
        ));
    }
    Ok(statements.join("\n"))
}

/// XML export: `<?xml ...?><results><row><column name="...">value</column></row></results>`.
/// Null cells render as `<column name="..." null="true" />`.
pub fn xml(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> String {
    let names = unique_column_names(columns);
    let mut lines = Vec::with_capacity(rows.len() * names.len() + 3);
    lines.push("<?xml version=\"1.0\" encoding=\"UTF-8\"?>".to_string());
    lines.push("<results>".to_string());
    for row in rows {
        lines.push("  <row>".to_string());
        for (ci, name) in names.iter().enumerate() {
            let escaped_name = xml_escape(name);
            let value = row.get(ci).unwrap_or(&CellValue::Null);
            if *value == CellValue::Null {
                lines.push(format!(
                    "    <column name=\"{}\" null=\"true\" />",
                    escaped_name
                ));
            } else {
                lines.push(format!(
                    "    <column name=\"{}\">{}</column>",
                    escaped_name,
                    xml_escape(&cell_display(value))
                ));
            }
        }
        lines.push("  </row>".to_string());
    }
    lines.push("</results>".to_string());
    lines.join("\n")
}

/// Temp-table SQL: `CREATE TABLE #T (...) INSERT INTO #T (...) VALUES ...`.
#[cfg(test)]
pub fn temp_table_sql(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    table_name: &str,
) -> String {
    temp_table_sql_for(DatabaseType::SqlServer, columns, rows, table_name)
}

/// Temp-table export in the engine's own temporary-table form: `#name` on
/// the SQL Server family, `CREATE TEMPORARY TABLE` on the MySQL lineage,
/// `CREATE TEMP TABLE` on the PostgreSQL/SQLite lineages. `table_name` is
/// the bare name; this function adds the engine's temp marker and quoting.
pub fn temp_table_sql_for(
    db: DatabaseType,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    table_name: &str,
) -> String {
    let bare = table_name.trim_start_matches('#');
    let (create_kw, table_ref) = match db.connection_kind() {
        // SQL Server temp tables are named into tempdb via the # prefix;
        // preserve a caller-supplied #name byte-for-byte.
        ConnectionKind::Tds => ("CREATE TABLE", format!("#{bare}")),
        ConnectionKind::MySqlWire => ("CREATE TEMPORARY TABLE", escape_identifier_for(db, bare)),
        _ => ("CREATE TEMP TABLE", escape_identifier_for(db, bare)),
    };
    let names = unique_column_names(columns);
    let mut lines = Vec::new();
    lines.push(format!("{create_kw} {table_ref} ("));
    for (i, name) in names.iter().enumerate() {
        let default_col = ResultColumn {
            name: name.clone(),
            data_type: String::new(),
            nullable: true,
        };
        let col = columns.get(i).unwrap_or(&default_col);
        let suffix = if i == names.len() - 1 { "" } else { "," };
        lines.push(format!(
            "    {} {} {}{}",
            escape_identifier_for(db, name),
            sql_type_for(db, col),
            if col.nullable { "NULL" } else { "NOT NULL" },
            suffix
        ));
    }
    lines.push(");".to_string());
    if rows.is_empty() {
        lines.push(String::new());
        lines.push("-- No rows returned.".to_string());
        return lines.join("\n");
    }
    lines.push(String::new());
    let col_list = names
        .iter()
        .map(|n| escape_identifier_for(db, n))
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!("INSERT INTO {} ({})", table_ref, col_list));
    lines.push("VALUES".to_string());
    for (i, row) in rows.iter().enumerate() {
        let values: Vec<String> = (0..names.len())
            .map(|ci| {
                sql_literal_in_column(db, columns.get(ci), row.get(ci).unwrap_or(&CellValue::Null))
            })
            .collect();
        let suffix = if i == rows.len() - 1 { ";" } else { "," };
        lines.push(format!("    ({}){}", values.join(", "), suffix));
    }
    lines.join("\n")
}

/// SQL VALUES: `SELECT col1, col2 FROM (VALUES ...) AS alias(col1, col2);`.
#[cfg(test)]
pub fn sql_values(columns: &[ResultColumn], rows: &[Vec<CellValue>], alias: &str) -> String {
    sql_values_for(DatabaseType::SqlServer, columns, rows, alias)
}

/// Inline result-set-as-SQL export. Engines whose FROM clause accepts a
/// column-aliased VALUES table (SQL Server, PostgreSQL lineage) get
/// `SELECT … FROM (VALUES …) AS alias(cols)`; the MySQL/SQLite lineages get
/// the portable `SELECT … UNION ALL SELECT …` form instead, since their
/// VALUES constructors cannot name columns that way.
pub fn sql_values_for(
    db: DatabaseType,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    alias: &str,
) -> String {
    let names = unique_column_names(columns);
    if names.is_empty() {
        return "SELECT 1 WHERE 1 = 0;".to_string();
    }
    let escaped_cols_list: Vec<String> =
        names.iter().map(|n| escape_identifier_for(db, n)).collect();
    let escaped_cols = escaped_cols_list.join(", ");
    if rows.is_empty() {
        let casts: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let default_col = ResultColumn {
                    name: n.clone(),
                    data_type: String::new(),
                    nullable: true,
                };
                let col = columns.get(i).unwrap_or(&default_col);
                format!(
                    "CAST(NULL AS {}) AS {}",
                    sql_type_for(db, col),
                    escape_identifier_for(db, n)
                )
            })
            .collect();
        return format!("SELECT {}\nWHERE 1 = 0;", casts.join(", "));
    }
    let values_alias_supported = matches!(
        db.connection_kind(),
        ConnectionKind::Tds | ConnectionKind::PostgresWire
    );
    if !values_alias_supported {
        // Portable UNION ALL form: first SELECT carries the column aliases.
        let mut lines = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            let values: Vec<String> = (0..names.len())
                .map(|ci| {
                    let lit = sql_literal_in_column(
                        db,
                        columns.get(ci),
                        row.get(ci).unwrap_or(&CellValue::Null),
                    );
                    if i == 0 {
                        format!("{} AS {}", lit, escaped_cols_list[ci])
                    } else {
                        lit
                    }
                })
                .collect();
            let prefix = if i == 0 { "SELECT" } else { "UNION ALL SELECT" };
            let suffix = if i == rows.len() - 1 { ";" } else { "" };
            lines.push(format!("{} {}{}", prefix, values.join(", "), suffix));
        }
        return lines.join("\n");
    }
    let mut lines = Vec::with_capacity(rows.len() + 3);
    lines.push(format!("SELECT {}", escaped_cols));
    lines.push("FROM (VALUES".to_string());
    for (i, row) in rows.iter().enumerate() {
        let values: Vec<String> = (0..names.len())
            .map(|ci| {
                sql_literal_in_column(db, columns.get(ci), row.get(ci).unwrap_or(&CellValue::Null))
            })
            .collect();
        let suffix = if i == rows.len() - 1 {
            format!(
                ") AS {}({});",
                escape_identifier_for(db, alias),
                escaped_cols
            )
        } else {
            ",".to_string()
        };
        lines.push(format!("    ({}){}", values.join(", "), suffix));
    }
    lines.join("\n")
}

/// Renders a `CellValue` as a SQL literal, matching Swift's `sqlLiteral`:
/// strings always get the `N'...'` prefix, bools render as `1`/`0`.
#[cfg(test)]
pub fn sql_literal(value: &CellValue) -> String {
    sql_literal_for(DatabaseType::SqlServer, value)
}

/// Renders a `CellValue` as a SQL literal for `column`: like
/// [`sql_literal_for`], except that a text cell holding a plain numeric
/// literal in a numeric column (decimal/money, or any number on a text
/// protocol) is written bare — `12.50`, not `N'12.50'` — so the script
/// inserts a number into a numeric column without an implicit conversion.
pub fn sql_literal_in_column(
    db: DatabaseType,
    column: Option<&ResultColumn>,
    value: &CellValue,
) -> String {
    if let CellValue::String(s) = value
        && let Some(number) = numeric_text_in_column(column, s)
    {
        return number.to_string();
    }
    sql_literal_for(db, value)
}

/// Renders a `CellValue` as a literal in the engine's own string, boolean,
/// and binary forms. SQL Server output is byte-identical to the legacy
/// `sql_literal`.
pub fn sql_literal_for(db: DatabaseType, value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => db.bool_literal(*b).to_string(),
        CellValue::I64(v) => v.to_string(),
        CellValue::F64(v) => {
            if v.is_finite() {
                v.to_string()
            } else {
                "NULL".to_string()
            }
        }
        CellValue::String(s) => db.string_literal(s),
        CellValue::Bytes(b) => db.bytes_literal(b),
    }
}

/// Deduplicates column names by appending `_2`, `_3`, etc. for duplicates,
/// skipping any suffix another column is already named. Empty names become
/// `"Column"`.
pub fn unique_column_names(columns: &[ResultColumn]) -> Vec<String> {
    let base_of = |c: &ResultColumn| {
        if c.name.trim().is_empty() {
            "Column".to_string()
        } else {
            c.name.clone()
        }
    };
    let reserved: std::collections::HashSet<String> = columns.iter().map(base_of).collect();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    columns
        .iter()
        .map(|c| {
            let base = base_of(c);
            if used.insert(base.clone()) {
                return base;
            }
            let mut n = 2;
            loop {
                let candidate = format!("{}_{}", base, n);
                if !reserved.contains(&candidate) && used.insert(candidate.clone()) {
                    return candidate;
                }
                n += 1;
            }
        })
        .collect()
}

/// Returns the SQL type for a column, defaulting to `nvarchar(max)` for
/// empty/unknown types (matching Swift's `sqlType`).
#[cfg(test)]
pub fn sql_type(column: &ResultColumn) -> String {
    sql_type_for(DatabaseType::SqlServer, column)
}

/// Returns the SQL type for a column, defaulting empty/unknown types to the
/// engine's own unbounded-text type.
pub fn sql_type_for(db: DatabaseType, column: &ResultColumn) -> String {
    let trimmed = column.data_type.trim();
    if trimmed.is_empty() || trimmed == "(unknown)" || trimmed == "(col)" {
        db.map_type(LogicalType::Text).native_name.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Escapes a multi-part identifier like `schema.table` by splitting on dots
/// outside brackets and escaping each part.
#[cfg(test)]
pub fn escape_multipart_identifier(identifier: &str) -> String {
    escape_multipart_identifier_for(DatabaseType::SqlServer, identifier)
}

/// Escapes a multi-part identifier like `schema.table` with the engine's
/// quoting, splitting on dots outside brackets.
pub fn escape_multipart_identifier_for(db: DatabaseType, identifier: &str) -> String {
    split_multipart_identifier(identifier)
        .iter()
        .map(|part| escape_identifier_for(db, part.trim()))
        .collect::<Vec<_>>()
        .join(".")
}

/// Splits an identifier on dots outside of `[...]` brackets.
fn split_multipart_identifier(identifier: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut bracket_depth: usize = 0;
    for ch in identifier.chars() {
        if ch == '[' {
            bracket_depth += 1;
        }
        if ch == ']' {
            bracket_depth = bracket_depth.saturating_sub(1);
        }
        if ch == '.' && bracket_depth == 0 {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(ch);
        }
    }
    parts.push(current);
    parts.into_iter().filter(|p| !p.trim().is_empty()).collect()
}

/// Quote one identifier with the engine's style. The bracket style delegates
/// to `table_actions::escape_identifier` (`]` doubling, `[Column]` fallback).
pub(crate) fn escape_identifier_for(db: DatabaseType, name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return match db.identifier_quoting() {
            sqlly_lang::sqlmodel::IdentifierQuoting::Brackets => "[Column]".to_string(),
            _ => db.quote_identifier("Column"),
        };
    }
    match db.identifier_quoting() {
        sqlly_lang::sqlmodel::IdentifierQuoting::Brackets => {
            crate::table_actions::escape_identifier(trimmed)
        }
        // The engine's own rule (ClickHouse/Spanner also escape `\`).
        _ => db.quote_identifier(trimmed),
    }
}

/// The portable `.sqllyexport` envelope: a self-describing JSON document
/// carrying the source engine, column schema (names, declared types,
/// nullability), and typed rows (null/bool/number/string; binary as a
/// `{"$bytes": "<hex>"}` marker; non-finite floats as strings). Version 1.
pub fn sqlly_export_envelope(
    db: DatabaseType,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
) -> String {
    let names = unique_column_names(columns);
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"format\": \"sqlly-export\",\n");
    out.push_str("  \"version\": 1,\n");
    out.push_str(&format!(
        "  \"sourceEngine\": \"{}\",\n",
        json_escape_string(db.display_name())
    ));
    out.push_str("  \"columns\": [\n");
    for (i, name) in names.iter().enumerate() {
        let default_col = ResultColumn {
            name: name.clone(),
            data_type: String::new(),
            nullable: true,
        };
        let col = columns.get(i).unwrap_or(&default_col);
        out.push_str(&format!(
            "    {{\"name\": \"{}\", \"dataType\": \"{}\", \"nullable\": {}}}{}\n",
            json_escape_string(name),
            json_escape_string(&col.data_type),
            col.nullable,
            if i == names.len() - 1 { "" } else { "," }
        ));
    }
    out.push_str("  ],\n");
    out.push_str("  \"rows\": [\n");
    for (ri, row) in rows.iter().enumerate() {
        let cells: Vec<String> = (0..names.len())
            .map(|ci| match row.get(ci).unwrap_or(&CellValue::Null) {
                CellValue::Null => "null".to_string(),
                CellValue::Bool(b) => b.to_string(),
                CellValue::I64(v) => v.to_string(),
                CellValue::F64(v) => {
                    if v.is_finite() {
                        v.to_string()
                    } else {
                        format!("\"{}\"", v)
                    }
                }
                CellValue::String(s) => format!("\"{}\"", json_escape_string(s)),
                CellValue::Bytes(b) => format!(
                    "{{\"$bytes\": \"{}\"}}",
                    b.iter().map(|x| format!("{x:02X}")).collect::<String>()
                ),
            })
            .collect();
        out.push_str(&format!(
            "    [{}]{}\n",
            cells.join(", "),
            if ri == rows.len() - 1 { "" } else { "," }
        ));
    }
    out.push_str("  ]\n}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clickhouse_export_identifiers_escape_backslashes() {
        assert_eq!(
            escape_identifier_for(DatabaseType::ClickHouse, r"c\"),
            r"`c\\`"
        );
    }
    use crate::table_actions::escape_identifier;

    fn cols(names: &[&str]) -> Vec<ResultColumn> {
        names
            .iter()
            .map(|n| ResultColumn {
                name: n.to_string(),
                data_type: "int".to_string(),
                nullable: true,
            })
            .collect()
    }

    fn row(values: &[CellValue]) -> Vec<CellValue> {
        values.to_vec()
    }

    #[test]
    fn excel_column_label_basic() {
        assert_eq!(excel_column_label(1), "A");
        assert_eq!(excel_column_label(26), "Z");
        assert_eq!(excel_column_label(27), "AA");
        assert_eq!(excel_column_label(52), "AZ");
        assert_eq!(excel_column_label(53), "BA");
        assert_eq!(excel_column_label(702), "ZZ");
        assert_eq!(excel_column_label(703), "AAA");
    }

    #[test]
    fn excel_column_label_clamps_zero() {
        assert_eq!(excel_column_label(0), "A");
    }

    #[test]
    fn xml_escape_special_chars() {
        let amp = "\u{26}amp;";
        let lt = "\u{26}lt;";
        let gt = "\u{26}gt;";
        let quot = "\u{26}quot;";
        let apos = "\u{26}apos;";
        let expected = format!("a{lt}b{gt}c{amp}d{quot}e{apos}f");
        assert_eq!(xml_escape("a<b>c&d\"e'f"), expected);
        assert_eq!(xml_escape("plain"), "plain");
    }

    #[test]
    fn csv_field_no_quoting() {
        assert_eq!(csv_field(&CellValue::I64(42)), "42");
        assert_eq!(csv_field(&CellValue::String("hello".into())), "hello");
    }

    #[test]
    fn csv_field_quoted_comma() {
        assert_eq!(csv_field(&CellValue::String("a,b".into())), "\"a,b\"");
    }

    #[test]
    fn csv_field_quoted_embedded_quote() {
        assert_eq!(
            csv_field(&CellValue::String("say \"hi\"".into())),
            "\"say \"\"hi\"\"\""
        );
    }

    #[test]
    fn csv_field_quoted_newline() {
        assert_eq!(
            csv_field(&CellValue::String("line1\nline2".into())),
            "\"line1\nline2\""
        );
    }

    #[test]
    fn tsv_includes_headers() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::String("Alice".into())]);
        let out = tsv(&c, &[r], true);
        assert_eq!(out, "id\tname\n1\tAlice");
    }

    #[test]
    fn tsv_no_headers() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::String("Alice".into())]);
        let out = tsv(&c, &[r], false);
        assert_eq!(out, "1\tAlice");
    }

    #[test]
    fn tsv_null_is_empty() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::Null]);
        let out = tsv(&c, &[r], false);
        assert_eq!(out, "1\t");
    }

    #[test]
    fn csv_basic() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::String("Alice".into())]);
        let out = csv(&c, &[r], true);
        assert_eq!(out, "id,name\n1,Alice");
    }

    #[test]
    fn csv_quoted_field() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::String("Smith, John".into())]);
        let out = csv(&c, &[r], true);
        assert_eq!(out, "id,name\n1,\"Smith, John\"");
    }

    #[test]
    fn excel_csv_has_bom() {
        let c = cols(&["id"]);
        let r = row(&[CellValue::I64(1)]);
        let out = excel_compatible_csv(&c, &[r]);
        assert!(out.starts_with('\u{FEFF}'));
        assert!(out.ends_with("id\n1"));
    }

    #[test]
    fn json_basic() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::String("Alice".into())]);
        let out = json(&c, &[r]);
        assert_eq!(out, r#"[{"id":1,"name":"Alice"}]"#);
    }

    #[test]
    fn json_null_and_types() {
        let mut c = cols(&["id", "flag", "name"]);
        c[1].data_type = "bit".to_string();
        let r = row(&[CellValue::I64(42), CellValue::Bool(true), CellValue::Null]);
        let out = json(&c, &[r]);
        assert_eq!(out, r#"[{"id":42,"flag":true,"name":null}]"#);
    }

    #[test]
    fn json_escapes_string() {
        let c = cols(&["msg"]);
        let r = row(&[CellValue::String("he said \"hi\"\nbye".into())]);
        let out = json(&c, &[r]);
        assert_eq!(out, r#"[{"msg":"he said \"hi\"\nbye"}]"#);
    }

    #[test]
    fn json_bytes() {
        let c = cols(&["data"]);
        let r = row(&[CellValue::Bytes(vec![0xAB, 0xCD])]);
        let out = json(&c, &[r]);
        assert_eq!(out, r#"[{"data":"0xABCD"}]"#);
    }

    #[test]
    fn markdown_basic() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::String("Alice".into())]);
        let out = markdown(&c, &[r], true);
        assert_eq!(out, "| id | name |\n| --- | --- |\n| 1 | Alice |");
    }

    #[test]
    fn markdown_escapes_pipe() {
        let c = cols(&["text"]);
        let r = row(&[CellValue::String("a|b".into())]);
        let out = markdown(&c, &[r], true);
        assert!(out.contains("a\\|b"));
    }

    #[test]
    fn html_basic() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::String("<b>".into())]);
        let out = html(&c, &[r], true);
        assert!(out.contains("<th>id</th>"));
        let lt = "\u{26}lt;";
        let gt = "\u{26}gt;";
        let expected = format!("<td>{lt}b{gt}</td>");
        assert!(out.contains(&expected), "output was: {}", out);
    }

    #[test]
    fn insert_statements_basic() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::String("Alice".into())]);
        let out = insert_statements(&c, &[r], "[Users]");
        assert_eq!(
            out,
            "INSERT INTO [Users] ([id], [name])\nVALUES\n    (1, N'Alice');"
        );
    }

    #[test]
    fn insert_statements_null() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::Null]);
        let out = insert_statements(&c, &[r], "[Users]");
        assert_eq!(
            out,
            "INSERT INTO [Users] ([id], [name])\nVALUES\n    (1, NULL);"
        );
    }

    #[test]
    fn insert_statements_nvarchar_prefix() {
        let mut c = cols(&["name"]);
        c[0].data_type = "nvarchar".to_string();
        let r = row(&[CellValue::String("héllo".into())]);
        let out = insert_statements(&c, &[r], "[T]");
        assert!(out.contains("N'héllo'"));
    }

    #[test]
    fn insert_statements_bytes() {
        let c = cols(&["data"]);
        let r = row(&[CellValue::Bytes(vec![0xFF, 0x00])]);
        let out = insert_statements(&c, &[r], "[T]");
        assert!(out.contains("0xFF00"));
    }

    #[test]
    fn insert_empty_rows() {
        let c = cols(&["id"]);
        let out = insert_statements(&c, &[], "[T]");
        assert_eq!(out, "");
    }

    #[test]
    fn xml_basic() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::String("Alice".into())]);
        let out = xml(&c, &[r]);
        assert_eq!(
            out,
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<results>\n  <row>\n    <column name=\"id\">1</column>\n    <column name=\"name\">Alice</column>\n  </row>\n</results>"
        );
    }

    #[test]
    fn xml_null_attribute() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::Null]);
        let out = xml(&c, &[r]);
        assert!(out.contains("null=\"true\""));
    }

    #[test]
    fn xml_escapes_special_chars() {
        let c = cols(&["msg"]);
        let r = row(&[CellValue::String("<b>&'\"</b>".into())]);
        let out = xml(&c, &[r]);
        let amp = "\u{26}amp;";
        let lt = "\u{26}lt;";
        let gt = "\u{26}gt;";
        let quot = "\u{26}quot;";
        let apos = "\u{26}apos;";
        assert!(out.contains(&format!("{lt}b{gt}{amp}{apos}{quot}")));
    }

    #[test]
    fn temp_table_sql_basic() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::String("Alice".into())]);
        let out = temp_table_sql(&c, &[r], "#ResultSetExport");
        assert!(out.contains("CREATE TABLE #ResultSetExport ("));
        assert!(out.contains("[id] int NULL"));
        assert!(out.contains("INSERT INTO #ResultSetExport"));
        assert!(out.contains("N'Alice'"));
    }

    #[test]
    fn temp_table_sql_empty_rows() {
        let c = cols(&["id"]);
        let out = temp_table_sql(&c, &[], "#T");
        assert!(out.contains("-- No rows returned."));
    }

    #[test]
    fn sql_values_basic() {
        let c = cols(&["id", "name"]);
        let r = row(&[CellValue::I64(1), CellValue::String("Alice".into())]);
        let out = sql_values(&c, &[r], "ResultValues");
        assert!(out.contains("SELECT [id], [name]"));
        assert!(out.contains("FROM (VALUES"));
        assert!(out.contains("N'Alice'"));
        assert!(out.contains("AS [ResultValues]([id], [name]);"));
    }

    #[test]
    fn sql_values_empty_rows() {
        let c = cols(&["id"]);
        let out = sql_values(&c, &[], "ResultValues");
        assert!(out.contains("CAST(NULL AS int)"));
        assert!(out.contains("WHERE 1 = 0;"));
    }

    #[test]
    fn sql_values_empty_columns() {
        let c: Vec<ResultColumn> = vec![];
        let out = sql_values(&c, &[], "ResultValues");
        assert_eq!(out, "SELECT 1 WHERE 1 = 0;");
    }

    #[test]
    fn sql_literal_always_uses_n_prefix() {
        assert_eq!(sql_literal(&CellValue::String("hello".into())), "N'hello'");
        assert_eq!(
            sql_literal(&CellValue::String("O'Brien".into())),
            "N'O''Brien'"
        );
        assert_eq!(sql_literal(&CellValue::I64(42)), "42");
        assert_eq!(sql_literal(&CellValue::Null), "NULL");
        assert_eq!(sql_literal(&CellValue::Bool(true)), "1");
        assert_eq!(sql_literal(&CellValue::Bool(false)), "0");
    }

    #[test]
    fn unique_column_names_dedup() {
        let c = vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: true,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "varchar".into(),
                nullable: true,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "varchar".into(),
                nullable: true,
            },
        ];
        let names = unique_column_names(&c);
        assert_eq!(names, vec!["id", "name", "name_2"]);
    }

    #[test]
    fn unique_column_names_empty_becomes_column() {
        let c = vec![ResultColumn {
            name: "".into(),
            data_type: "int".into(),
            nullable: true,
        }];
        let names = unique_column_names(&c);
        assert_eq!(names, vec!["Column"]);
    }

    #[test]
    fn unique_column_names_skip_suffixes_already_taken() {
        let c: Vec<ResultColumn> = ["num", "num", "num_2"]
            .iter()
            .map(|n| ResultColumn {
                name: (*n).into(),
                data_type: "int".into(),
                nullable: true,
            })
            .collect();
        assert_eq!(unique_column_names(&c), vec!["num", "num_3", "num_2"]);
    }

    #[test]
    fn sql_type_defaults_to_nvarchar() {
        let c = ResultColumn {
            name: "x".into(),
            data_type: "".into(),
            nullable: true,
        };
        assert_eq!(sql_type(&c), "nvarchar(max)");
        let c2 = ResultColumn {
            name: "x".into(),
            data_type: "(unknown)".into(),
            nullable: true,
        };
        assert_eq!(sql_type(&c2), "nvarchar(max)");
    }

    #[test]
    fn escape_multipart_identifier_basic() {
        assert_eq!(
            escape_multipart_identifier("schema.table"),
            "[schema].[table]"
        );
        assert_eq!(escape_multipart_identifier("table"), "[table]");
    }

    #[test]
    fn escape_identifier_empty() {
        assert_eq!(escape_identifier(""), "[Column]");
        assert_eq!(escape_identifier("   "), "[Column]");
    }

    #[test]
    fn escape_identifier_already_bracketed() {
        assert_eq!(escape_identifier("[my col]"), "[my col]");
    }

    #[test]
    fn escape_identifier_doubles_brackets() {
        assert_eq!(escape_identifier("my]col"), "[my]]col]");
    }

    #[test]
    fn cell_display_f64_nan_is_null() {
        assert_eq!(cell_display(&CellValue::F64(f64::NAN)), "NULL");
        assert_eq!(cell_display(&CellValue::F64(f64::INFINITY)), "NULL");
    }

    #[test]
    fn cell_display_preserves_full_i64_precision() {
        assert_eq!(
            cell_display(&CellValue::I64(9_223_372_036_854_775_807)),
            "9223372036854775807"
        );
        assert_eq!(
            cell_display(&CellValue::I64(-9_223_372_036_854_775_808)),
            "-9223372036854775808"
        );
    }

    #[test]
    fn cell_display_bytes_hex() {
        assert_eq!(
            cell_display(&CellValue::Bytes(vec![0xAB, 0xCD, 0xEF])),
            "0xABCDEF"
        );
    }

    // ── Per-engine SQL export coverage ──────────────────────────────────

    fn two_cols() -> Vec<ResultColumn> {
        vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: true,
            },
        ]
    }

    fn one_row() -> Vec<Vec<CellValue>> {
        vec![vec![CellValue::I64(1), CellValue::String("O'Brien".into())]]
    }

    #[test]
    fn insert_statements_for_postgres_uses_engine_forms() {
        let out = insert_statements_for(DatabaseType::Postgres, &two_cols(), &one_row(), "users");
        assert!(
            out.contains("INSERT INTO \"users\" (\"id\", \"name\")"),
            "{out}"
        );
        assert!(out.contains("(1, 'O''Brien');"), "{out}");
        assert!(!out.contains("N'"), "{out}");
    }

    #[test]
    fn insert_statements_for_mysql_uses_backticks() {
        let out = insert_statements_for(DatabaseType::MySql, &two_cols(), &one_row(), "users");
        assert!(out.contains("INSERT INTO `users` (`id`, `name`)"), "{out}");
    }

    #[test]
    fn temp_table_sql_for_per_engine_temp_form() {
        let mssql = temp_table_sql_for(DatabaseType::SqlServer, &two_cols(), &one_row(), "Results");
        assert!(mssql.starts_with("CREATE TABLE #Results ("), "{mssql}");
        let pg = temp_table_sql_for(DatabaseType::Postgres, &two_cols(), &one_row(), "Results");
        assert!(pg.starts_with("CREATE TEMP TABLE \"Results\" ("), "{pg}");
        assert!(pg.contains("INSERT INTO \"Results\""), "{pg}");
        let my = temp_table_sql_for(DatabaseType::MySql, &two_cols(), &one_row(), "Results");
        assert!(my.starts_with("CREATE TEMPORARY TABLE `Results` ("), "{my}");
        let lite = temp_table_sql_for(DatabaseType::Sqlite, &two_cols(), &one_row(), "Results");
        assert!(
            lite.starts_with("CREATE TEMP TABLE \"Results\" ("),
            "{lite}"
        );
    }

    #[test]
    fn temp_table_sql_for_unknown_type_falls_to_engine_text() {
        let cols = vec![ResultColumn {
            name: "x".into(),
            data_type: String::new(),
            nullable: true,
        }];
        let pg = temp_table_sql_for(DatabaseType::Postgres, &cols, &[], "T");
        assert!(pg.contains("\"x\" text NULL"), "{pg}");
        let mssql = temp_table_sql_for(DatabaseType::SqlServer, &cols, &[], "T");
        assert!(mssql.contains("[x] nvarchar(max) NULL"), "{mssql}");
    }

    #[test]
    fn sql_values_for_postgres_keeps_values_alias_form() {
        let out = sql_values_for(DatabaseType::Postgres, &two_cols(), &one_row(), "vals");
        assert!(out.contains("FROM (VALUES"), "{out}");
        assert!(out.contains(") AS \"vals\"(\"id\", \"name\");"), "{out}");
    }

    #[test]
    fn sql_values_for_mysql_and_sqlite_use_union_all() {
        let rows = vec![
            vec![CellValue::I64(1), CellValue::String("A".into())],
            vec![CellValue::I64(2), CellValue::String("B".into())],
        ];
        let my = sql_values_for(DatabaseType::MySql, &two_cols(), &rows, "vals");
        assert_eq!(
            my,
            "SELECT 1 AS `id`, 'A' AS `name`\nUNION ALL SELECT 2, 'B';"
        );
        let lite = sql_values_for(DatabaseType::Sqlite, &two_cols(), &rows, "vals");
        assert!(
            lite.starts_with("SELECT 1 AS \"id\", 'A' AS \"name\""),
            "{lite}"
        );
    }

    #[test]
    fn sqlly_export_envelope_is_self_describing() {
        let rows = vec![
            vec![CellValue::I64(1), CellValue::String("A\"B".into())],
            vec![CellValue::Null, CellValue::Bytes(vec![0xDE, 0xAD])],
        ];
        let out = sqlly_export_envelope(DatabaseType::Postgres, &two_cols(), &rows);
        assert!(out.contains("\"format\": \"sqlly-export\""), "{out}");
        assert!(out.contains("\"version\": 1"), "{out}");
        assert!(out.contains("\"sourceEngine\": \"PostgreSQL\""), "{out}");
        assert!(
            out.contains("{\"name\": \"id\", \"dataType\": \"int\", \"nullable\": false}"),
            "{out}"
        );
        assert!(out.contains("[1, \"A\\\"B\"],"), "{out}");
        assert!(out.contains("[null, {\"$bytes\": \"DEAD\"}]"), "{out}");
    }

    #[test]
    fn spill_export_cap_matches_snapshot_bound_and_names_the_truncation() {
        assert_eq!(EXPORT_MAX_ROWS, 100_000);
        let msg = spill_export_truncation_message(100_000, 1_000_000);
        assert!(msg.contains("100000"));
        assert!(msg.contains("1000000"));
        assert!(msg.contains("stay on disk"));
    }
}

#[cfg(test)]
mod copy_format_tests {
    use super::*;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: "int".to_string(),
            nullable: true,
        }
    }

    #[test]
    fn json_lines_table() {
        let columns = [col("id"), col("name")];
        let cases: Vec<(&str, Vec<Vec<CellValue>>, &str)> = vec![
            ("zero rows", vec![], ""),
            (
                "one row",
                vec![vec![CellValue::I64(1), CellValue::String("Alice".into())]],
                r#"{"id":1,"name":"Alice"}"#,
            ),
            (
                "three rows",
                vec![
                    vec![CellValue::I64(1), CellValue::String("Alice".into())],
                    vec![CellValue::I64(2), CellValue::Null],
                    vec![CellValue::I64(3), CellValue::String("say \"hi\"".into())],
                ],
                "{\"id\":1,\"name\":\"Alice\"}\n{\"id\":2,\"name\":null}\n{\"id\":3,\"name\":\"say \\\"hi\\\"\"}",
            ),
            (
                "bytes",
                vec![vec![CellValue::Bytes(vec![0xAB, 0xCD]), CellValue::Null]],
                r#"{"id":"0xABCD","name":null}"#,
            ),
        ];
        for (case, rows, expected) in cases {
            assert_eq!(json_lines(&columns, &rows), expected, "{case}");
        }
    }

    #[test]
    fn json_array_still_byte_identical_after_json_object_extract() {
        let columns = [col("id"), col("name")];
        let rows = [vec![CellValue::I64(1), CellValue::String("Alice".into())]];
        assert_eq!(json(&columns, &rows), r#"[{"id":1,"name":"Alice"}]"#);
    }

    #[test]
    #[allow(clippy::type_complexity)] // One-off signature; a type alias would not improve readability.
    fn sql_in_list_table() {
        let id = col("Id");
        let name = col("Name");
        let cases: Vec<(
            &str,
            DatabaseType,
            &ResultColumn,
            Vec<CellValue>,
            bool,
            &str,
        )> = vec![
            (
                "sqlserver integers",
                DatabaseType::SqlServer,
                &id,
                vec![CellValue::I64(1), CellValue::I64(2), CellValue::I64(3)],
                false,
                "[Id] IN (1, 2, 3)",
            ),
            (
                "postgres integers",
                DatabaseType::Postgres,
                &id,
                vec![CellValue::I64(1), CellValue::I64(2)],
                false,
                "\"Id\" IN (1, 2)",
            ),
            (
                "mysql integers",
                DatabaseType::MySql,
                &id,
                vec![CellValue::I64(1), CellValue::I64(2)],
                false,
                "`Id` IN (1, 2)",
            ),
            (
                "sqlite integers",
                DatabaseType::Sqlite,
                &id,
                vec![CellValue::I64(1), CellValue::I64(2)],
                false,
                "\"Id\" IN (1, 2)",
            ),
            (
                "sqlserver quote doubling",
                DatabaseType::SqlServer,
                &name,
                vec![CellValue::String("O'Brien".into())],
                false,
                "[Name] IN (N'O''Brien')",
            ),
            (
                "postgres quote doubling",
                DatabaseType::Postgres,
                &name,
                vec![CellValue::String("O'Brien".into())],
                false,
                "\"Name\" IN ('O''Brien')",
            ),
            (
                "mysql quote doubling",
                DatabaseType::MySql,
                &name,
                vec![CellValue::String("O'Brien".into())],
                false,
                "`Name` IN ('O''Brien')",
            ),
            (
                "sqlite quote doubling",
                DatabaseType::Sqlite,
                &name,
                vec![CellValue::String("O'Brien".into())],
                false,
                "\"Name\" IN ('O''Brien')",
            ),
            (
                "nulls dropped",
                DatabaseType::SqlServer,
                &id,
                vec![CellValue::I64(1), CellValue::Null, CellValue::I64(2)],
                false,
                "[Id] IN (1, 2)",
            ),
            (
                "distinct on",
                DatabaseType::SqlServer,
                &id,
                vec![CellValue::I64(1), CellValue::I64(1), CellValue::I64(2)],
                true,
                "[Id] IN (1, 2)",
            ),
            (
                "distinct off keeps duplicates",
                DatabaseType::SqlServer,
                &id,
                vec![CellValue::I64(1), CellValue::I64(1), CellValue::I64(2)],
                false,
                "[Id] IN (1, 1, 2)",
            ),
            (
                "empty becomes IN (NULL)",
                DatabaseType::SqlServer,
                &id,
                vec![],
                false,
                "[Id] IN (NULL)",
            ),
            (
                "all nulls becomes IN (NULL)",
                DatabaseType::Postgres,
                &id,
                vec![CellValue::Null, CellValue::Null],
                true,
                "\"Id\" IN (NULL)",
            ),
        ];
        for (case, db, column, values, distinct, expected) in cases {
            assert_eq!(
                sql_in_list_for(db, column, &values, distinct),
                expected,
                "{case}"
            );
        }
    }

    #[test]
    #[allow(clippy::type_complexity)] // One-off signature; a type alias would not improve readability.
    fn update_statements_table() {
        let cols = vec![col("Id"), col("Name"), col("Active")];
        let row = vec![
            CellValue::I64(1),
            CellValue::String("Ada".into()),
            CellValue::Bool(true),
        ];
        let ok_cases: Vec<(
            &str,
            DatabaseType,
            &str,
            &[ResultColumn],
            Vec<Vec<CellValue>>,
            Vec<String>,
            &str,
        )> = vec![
            (
                "single-column key sqlserver",
                DatabaseType::SqlServer,
                "dbo.Users",
                &cols,
                vec![row.clone()],
                vec!["Id".into()],
                "UPDATE [dbo].[Users] SET [Name] = N'Ada', [Active] = 1 WHERE [Id] = 1;",
            ),
            (
                "composite key",
                DatabaseType::SqlServer,
                "Users",
                &cols,
                vec![row.clone()],
                vec!["Id".into(), "Name".into()],
                "UPDATE [Users] SET [Active] = 1 WHERE [Id] = 1 AND [Name] = N'Ada';",
            ),
            (
                "null key uses IS NULL",
                DatabaseType::SqlServer,
                "Users",
                &cols,
                vec![vec![
                    CellValue::Null,
                    CellValue::String("Ada".into()),
                    CellValue::Bool(false),
                ]],
                vec!["Id".into()],
                "UPDATE [Users] SET [Name] = N'Ada', [Active] = 0 WHERE [Id] IS NULL;",
            ),
            (
                "postgres quoted identifiers and TRUE",
                DatabaseType::Postgres,
                "public.users",
                &cols,
                vec![row.clone()],
                vec!["id".into()],
                "UPDATE \"public\".\"users\" SET \"Name\" = 'Ada', \"Active\" = TRUE WHERE \"Id\" = 1;",
            ),
        ];
        for (case, db, table, columns, rows, keys, expected) in ok_cases {
            let actual = update_statements_for(db, table, columns, &rows, &keys)
                .unwrap_or_else(|e| panic!("{case}: {e}"));
            assert_eq!(actual, expected, "{case}");
        }

        let err_cases: Vec<(&str, &[ResultColumn], Vec<String>, UpdateRenderError)> = vec![
            (
                "missing key",
                &cols,
                vec!["Missing".into()],
                UpdateRenderError::MissingKey("Missing".into()),
            ),
            (
                "all-key projection",
                &cols[..1],
                vec!["Id".into()],
                UpdateRenderError::NoNonKeyColumns,
            ),
        ];
        for (case, columns, keys, expected) in err_cases {
            let err = update_statements_for(
                DatabaseType::SqlServer,
                "Users",
                columns,
                std::slice::from_ref(&row),
                &keys,
            )
            .expect_err(case);
            assert_eq!(err, expected, "{case}");
        }
    }

    #[test]
    fn update_render_error_display_mentions_primary_key() {
        assert!(
            UpdateRenderError::MissingKey("Id".into())
                .to_string()
                .contains("Primary key column Id")
        );
    }
}

#[cfg(test)]
mod options_tests {
    use super::*;
    use crate::export_options::{ExportOptions, LineBreaks, NullAs};
    use crate::results::ExportFormat;

    fn sample_columns() -> Vec<ResultColumn> {
        vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "name".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            },
        ]
    }

    #[test]
    fn csv_with_custom_null() {
        let cols = sample_columns();
        let rows = vec![vec![CellValue::I64(1), CellValue::Null]];
        let opts = ExportOptions {
            null_as: NullAs::Custom("\\N".into()),
            ..ExportOptions::default_for(ExportFormat::Csv)
        };
        let got = csv_with(&cols, &rows, &opts);
        assert!(got.contains("\\N"));
    }

    #[test]
    fn tsv_with_literal_null() {
        let cols = sample_columns();
        let rows = vec![vec![CellValue::I64(1), CellValue::Null]];
        let opts = ExportOptions {
            null_as: NullAs::Literal,
            ..ExportOptions::default_for(ExportFormat::Tsv)
        };
        let got = tsv_with(&cols, &rows, &opts);
        assert!(got.ends_with("1\tNULL"));
    }

    #[test]
    fn markdown_with_escape_line_break() {
        let cols = sample_columns();
        let rows = vec![vec![CellValue::I64(1), CellValue::String("\n".to_string())]];
        let opts = ExportOptions {
            line_breaks: LineBreaks::Escape,
            ..ExportOptions::default_for(ExportFormat::Markdown)
        };
        let got = markdown_with(&cols, &rows, &opts);
        assert!(got.contains("\\n"));
    }

    #[test]
    fn html_with_no_header_row() {
        let cols = sample_columns();
        let rows = vec![vec![CellValue::I64(1), CellValue::String("a".into())]];
        let opts = ExportOptions {
            header_row: false,
            ..ExportOptions::default_for(ExportFormat::Html)
        };
        let got = html_with(&cols, &rows, &opts);
        assert!(!got.contains("<th>"));
    }

    #[test]
    fn csv_default_equals_legacy() {
        let cols = sample_columns();
        let rows = vec![vec![CellValue::I64(1), CellValue::String("a,b\nc".into())]];
        let legacy = csv(&cols, &rows, true);
        let opts = ExportOptions::default_for(ExportFormat::Csv);
        assert_eq!(csv_with(&cols, &rows, &opts), legacy);
    }

    #[test]
    fn tsv_default_equals_legacy() {
        let cols = sample_columns();
        let rows = vec![vec![
            CellValue::I64(1),
            CellValue::String("line\nbreak".into()),
        ]];
        let legacy = tsv(&cols, &rows, true);
        let opts = ExportOptions::default_for(ExportFormat::Tsv);
        assert_eq!(tsv_with(&cols, &rows, &opts), legacy);
    }

    // ---- spreadsheet formula guard -------------------------------------

    fn csv_opts(formula_safe: bool) -> ExportOptions {
        ExportOptions {
            formula_safe,
            ..ExportOptions::default_for(ExportFormat::Csv)
        }
    }

    fn tsv_opts(formula_safe: bool) -> ExportOptions {
        ExportOptions {
            formula_safe,
            ..ExportOptions::default_for(ExportFormat::Tsv)
        }
    }

    #[test]
    fn csv_field_guards_every_trigger_character() {
        for trigger in sqlly_core::formula_guard::FORMULA_TRIGGERS {
            let value = CellValue::String(format!("{trigger}cmd"));
            assert_eq!(
                csv_field_with(&value, &csv_opts(true)),
                format!("\"'{trigger}cmd\""),
                "{trigger:?}"
            );
        }
    }

    #[test]
    fn tsv_field_guards_every_trigger_character() {
        for trigger in sqlly_core::formula_guard::FORMULA_TRIGGERS {
            let value = CellValue::String(format!("{trigger}cmd"));
            assert_eq!(
                tsv_field_with(&value, &tsv_opts(true)),
                format!("'{trigger}cmd"),
                "{trigger:?}"
            );
        }
    }

    #[test]
    fn csv_field_numeric_text_is_exempt() {
        for text in ["-12.5", "+3", "-1e10", "+.5", "-7."] {
            let value = CellValue::String(text.into());
            assert_eq!(csv_field_with(&value, &csv_opts(true)), text);
            assert_eq!(tsv_field_with(&value, &tsv_opts(true)), text);
        }
        // Arithmetic-looking text is not a plain number and stays guarded.
        assert_eq!(
            csv_field_with(&CellValue::String("-1+1".into()), &csv_opts(true)),
            "\"'-1+1\""
        );
    }

    #[test]
    fn csv_field_never_guards_non_text_cells() {
        let opts = csv_opts(true);
        assert_eq!(csv_field_with(&CellValue::I64(-5), &opts), "-5");
        assert_eq!(csv_field_with(&CellValue::F64(-0.25), &opts), "-0.25");
        assert_eq!(csv_field_with(&CellValue::Bool(true), &opts), "true");
        assert_eq!(csv_field_with(&CellValue::Bytes(vec![0x3d]), &opts), "0x3D");
        assert_eq!(csv_field_with(&CellValue::Null, &opts), "NULL");
        // A custom NULL token that looks like a formula is still a NULL, not text.
        let null_opts = ExportOptions {
            null_as: NullAs::Custom("-".into()),
            ..csv_opts(true)
        };
        assert_eq!(csv_field_with(&CellValue::Null, &null_opts), "-");
        assert_eq!(tsv_field_with(&CellValue::Null, &null_opts), "-");
    }

    #[test]
    fn csv_field_guard_keeps_quote_escaping() {
        let value = CellValue::String("=HYPERLINK(\"http://x\",\"a,b\")".into());
        assert_eq!(
            csv_field_with(&value, &csv_opts(true)),
            "\"'=HYPERLINK(\"\"http://x\"\",\"\"a,b\"\")\""
        );
        // Ordinary quoting is unchanged for cells that need no guard.
        assert_eq!(
            csv_field_with(&CellValue::String("a,b".into()), &csv_opts(true)),
            "\"a,b\""
        );
        assert_eq!(
            csv_field_with(&CellValue::String("plain".into()), &csv_opts(true)),
            "plain"
        );
    }

    #[test]
    fn csv_field_guard_judges_text_after_line_break_handling() {
        // A leading tab replaced by a space is no longer a trigger…
        let value = CellValue::String("\t=1".into());
        let space = ExportOptions {
            line_breaks: LineBreaks::Space,
            ..csv_opts(true)
        };
        assert_eq!(csv_field_with(&value, &space), " =1");
        // …while an escaped one starts with a backslash and is also safe.
        let escape = ExportOptions {
            line_breaks: LineBreaks::Escape,
            ..csv_opts(true)
        };
        assert_eq!(csv_field_with(&value, &escape), "\\t=1");
        // Kept verbatim, it is guarded and quoted (a tab needs no quoting, the guard does).
        assert_eq!(csv_field_with(&value, &csv_opts(true)), "\"'\t=1\"");
    }

    #[test]
    fn formula_guard_off_writes_text_verbatim() {
        let value = CellValue::String("=1+1".into());
        assert_eq!(csv_field_with(&value, &csv_opts(false)), "=1+1");
        assert_eq!(tsv_field_with(&value, &tsv_opts(false)), "=1+1");
        let cols = sample_columns();
        let rows = vec![vec![CellValue::I64(1), CellValue::String("@cmd".into())]];
        assert_eq!(csv_with(&cols, &rows, &csv_opts(false)), "id,name\n1,@cmd");
        assert_eq!(
            tsv_with(&cols, &rows, &tsv_opts(false)),
            "id\tname\n1\t@cmd"
        );
    }

    #[test]
    fn csv_and_tsv_documents_guard_cells_and_headers() {
        let mut cols = sample_columns();
        cols[1].name = "=total".into();
        let rows = vec![
            vec![CellValue::I64(1), CellValue::String("=1+1".into())],
            vec![CellValue::I64(2), CellValue::String("-12.5".into())],
            vec![CellValue::I64(-3), CellValue::Null],
        ];
        assert_eq!(
            csv_with(&cols, &rows, &csv_opts(true)),
            "id,\"'=total\"\n1,\"'=1+1\"\n2,-12.5\n-3,NULL"
        );
        assert_eq!(
            tsv_with(&cols, &rows, &tsv_opts(true)),
            "id\t'=total\n1\t'=1+1\n2\t-12.5\n-3\t"
        );
        // The legacy wrappers (clipboard defaults) are guarded too.
        assert_eq!(
            csv(&cols, &rows, true),
            csv_with(&cols, &rows, &csv_opts(true))
        );
        assert_eq!(tsv(&cols, &rows, false), "1\t'=1+1\n2\t-12.5\n-3\t");
    }

    #[test]
    fn excel_compatible_csv_is_guarded() {
        let cols = sample_columns();
        let rows = vec![vec![CellValue::I64(1), CellValue::String("+cmd".into())]];
        assert_eq!(
            excel_compatible_csv(&cols, &rows),
            "\u{FEFF}id,name\n1,\"'+cmd\""
        );
    }
}

#[cfg(test)]
mod export_notice_tests {
    use super::*;

    #[test]
    fn copied_message_names_the_format_and_row_count() {
        assert_eq!(
            export_copied_message("CSV", 1432),
            "Copied 1,432 rows as CSV"
        );
    }

    #[test]
    fn copied_message_is_singular_for_one_row() {
        assert_eq!(export_copied_message("JSON", 1), "Copied 1 row as JSON");
    }

    #[test]
    fn copied_message_handles_an_empty_but_present_result_set() {
        // A result set with zero rows still copied a header, so this is a
        // successful copy, not the "nothing to export" case.
        assert_eq!(export_copied_message("CSV", 0), "Copied 0 rows as CSV");
    }

    #[test]
    fn copied_message_uses_spanish_with_complete_named_arguments() {
        let _guard = crate::localization::test_language(crate::localization::Language::Spanish);
        let message = export_copied_message("CSV", 1432);
        assert_ne!(message, "Copied 1,432 rows as CSV");
        assert!(message.contains("1,432"), "{message}");
        assert!(message.contains("CSV"), "{message}");
        assert!(!message.contains("{row_count}"), "{message}");
        assert!(!message.contains("{format_name}"), "{message}");
    }

    #[test]
    fn export_message_keys_are_compiled_for_both_languages() {
        for key in [
            "results.export.copied_one_row",
            "results.export.copied_multiple_rows",
            "results.export.spill_truncation",
            "results.export.no_results",
        ] {
            assert!(sqlly_i18n::contains_key(key), "{key}");
        }
    }
}

#[cfg(test)]
mod numeric_column_tests {
    use super::*;

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
        }
    }

    fn row(values: &[CellValue]) -> Vec<CellValue> {
        values.to_vec()
    }

    #[test]
    fn json_writes_numeric_text_in_numeric_columns_as_numbers() {
        // decimal/money reach the exporter as text (full precision), and so
        // does every value on the PostgreSQL/MySQL text protocols.
        let c = vec![
            col("total", "money"),
            col("qty", "numeric(38,10)"),
            col("sku", "varchar"),
            col("note", "decimal(10,2)"),
        ];
        let r = row(&[
            CellValue::String("1234.56".into()),
            CellValue::String("-0.0000000001".into()),
            CellValue::String("00123".into()),
            CellValue::String("N/A".into()),
        ]);
        assert_eq!(
            json(&c, &[r]),
            r#"[{"total":1234.56,"qty":-0.0000000001,"sku":"00123","note":"N/A"}]"#
        );
    }

    #[test]
    fn json_keeps_every_column_when_names_repeat() {
        // `SELECT 1 num, 2 num, 3 num`: a JSON object with three "num" keys
        // parses back as one column holding the last value.
        let c = vec![col("num", "int"), col("num", "int"), col("num", "int")];
        let r = row(&[CellValue::I64(1), CellValue::I64(2), CellValue::I64(3)]);
        let text = json(&c, std::slice::from_ref(&r));
        assert_eq!(text, r#"[{"num":1,"num_2":2,"num_3":3}]"#);
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed[0].as_object().unwrap().len(), 3);
        assert_eq!(
            json_lines(&c, &[r.clone(), r]),
            "{\"num\":1,\"num_2\":2,\"num_3\":3}\n{\"num\":1,\"num_2\":2,\"num_3\":3}"
        );
    }

    #[test]
    fn sql_scripts_write_numeric_text_in_numeric_columns_bare() {
        let c = vec![
            col("id", "int"),
            col("price", "decimal(10,2)"),
            col("sku", "varchar"),
        ];
        let r = row(&[
            CellValue::String("7".into()),
            CellValue::String("12.50".into()),
            CellValue::String("00123".into()),
        ]);
        let insert =
            insert_statements_for(DatabaseType::SqlServer, &c, std::slice::from_ref(&r), "T");
        assert!(insert.contains("(7, 12.50, N'00123')"), "{insert}");
        let values = sql_values_for(DatabaseType::Postgres, &c, std::slice::from_ref(&r), "v");
        assert!(values.contains("(7, 12.50, '00123')"), "{values}");
        let union = sql_values_for(DatabaseType::MySql, &c, std::slice::from_ref(&r), "v");
        assert!(
            union.contains("SELECT 7 AS `id`, 12.50 AS `price`, '00123' AS `sku`"),
            "{union}"
        );
        let update = update_statements_for(
            DatabaseType::SqlServer,
            "T",
            &c,
            std::slice::from_ref(&r),
            &["id".into()],
        )
        .unwrap();
        assert!(
            update.contains("SET [price] = 12.50, [sku] = N'00123' WHERE [id] = 7;"),
            "{update}"
        );
        let in_list = sql_in_list_for(DatabaseType::SqlServer, &c[1], &r[1..2], true);
        assert_eq!(in_list, "[price] IN (12.50)");
        // Text that is not a plain numeric literal is still quoted.
        let r = row(&[
            CellValue::I64(1),
            CellValue::String("$1.00".into()),
            CellValue::Null,
        ]);
        let insert = insert_statements_for(DatabaseType::SqlServer, &c, &[r], "T");
        assert!(insert.contains("(1, N'$1.00', NULL)"), "{insert}");
    }

    /// Redis values arrive as text in `string`-typed columns (the driver
    /// never coerces a value), so the single classifier keeps `00014`, a
    /// 40-digit integer and `1e309` as text in every typed export — the
    /// JSON object quotes them, the CSV writes the digits verbatim, and
    /// the SQL script quotes them — exactly as the grid shows them.
    #[test]
    fn redis_text_values_export_as_text_not_numbers() {
        use crate::result_cell_format::{column_is_numeric, numeric_text_in_column};
        let c = vec![ResultColumn {
            name: "result".into(),
            data_type: "string".into(),
            nullable: true,
        }];
        assert!(!column_is_numeric(Some(&c[0])));
        for value in [
            "00014",
            "1234567890123456789012345678901234567890",
            "1e309",
            "1e5",
        ] {
            assert_eq!(numeric_text_in_column(Some(&c[0]), value), None);
            let r = row(&[CellValue::String(value.into())]);
            assert_eq!(
                json(&c, std::slice::from_ref(&r)),
                format!("[{{\"result\":\"{value}\"}}]")
            );
            let body = csv(&c, std::slice::from_ref(&r), false);
            assert_eq!(body.trim_end(), value, "{body:?}");
            assert_eq!(
                sql_literal_in_column(DatabaseType::SqlServer, Some(&c[0]), &r[0]),
                format!("N'{value}'")
            );
        }
        // A `Bytes` cell (invalid UTF-8) stays a hex string everywhere.
        let r = row(&[CellValue::Bytes(vec![0xff, 0xfe])]);
        assert_eq!(
            json(&c, std::slice::from_ref(&r)),
            "[{\"result\":\"0xFFFE\"}]"
        );
    }
}
