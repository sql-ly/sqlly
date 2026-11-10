//! The Excel export of a result grid: a workbook whose first sheet is the
//! grid exactly as shown — column order, widths, hidden and frozen columns,
//! the query's formats and conditional formatting, and computed columns as
//! live Excel formulas — and whose second sheet is the pivot as it stands,
//! when the result has one configured.

use std::collections::HashMap;

use gpui::App;
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::pivot::state::{VisibleColKind, VisibleRowKind};
use sqlly_datatable::{ConditionalKind, TextCase};

use crate::computed_columns::Slot;
use crate::formula::{self, ExcelRefContext};
use crate::query_format::QueryColumnFormat;
use crate::xlsx_workbook::{
    self, Cell, ConditionalBlock, Sheet, SheetColumn, Workbook, excel_width, strftime_to_excel,
};

use super::{ResultsView, grid_marker_width};

/// How an exported column's cells are written.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Date,
    DateTime,
    Time,
    Number,
    Other,
}

fn kind_of(data_type: &str) -> Kind {
    let t = data_type.to_ascii_lowercase();
    if t.contains("datetime") || t.contains("timestamp") || t.contains("smalldatetime") {
        Kind::DateTime
    } else if t.contains("date") {
        Kind::Date
    } else if t.contains("time") {
        Kind::Time
    } else if crate::result_cell_format::column_is_numeric(Some(&ResultColumn {
        name: String::new(),
        data_type: data_type.to_string(),
        nullable: true,
    })) || t == "float"
    {
        Kind::Number
    } else {
        Kind::Other
    }
}

fn number_format(decimals: usize, thousands: bool) -> String {
    let mut f = if thousands {
        "#,##0".to_string()
    } else {
        "0".to_string()
    };
    if decimals > 0 {
        f.push('.');
        f.push_str(&"0".repeat(decimals));
    }
    f
}

impl ResultsView {
    /// Bring `slot`'s pivot up to date with its rows before an export reads
    /// it — the grid only computes the pivot when its tab is shown.
    pub(crate) fn prepare_excel_export(&self, slot: usize, cx: &mut gpui::Context<Self>) {
        if let Some(Some(grid)) = self.grid_views.get(slot) {
            grid.update(cx, |g, cx| g.sync_pivot_source(cx));
        }
    }

    /// What "Open in Spreadsheet" hands the OS for `rows` of `slot`: the
    /// grid workbook when the spreadsheet format is XLSX, otherwise the
    /// plain text export. Call [`Self::prepare_excel_export`] first.
    pub(crate) fn spreadsheet_payload(
        &self,
        slot: usize,
        prefs: &crate::preferences::ExportPreferences,
        columns: &[ResultColumn],
        rows: &[Vec<CellValue>],
        cx: &App,
    ) -> Option<(Vec<u8>, super::ExportFormat)> {
        if crate::open_in_spreadsheet::spreadsheet_format(prefs) == super::ExportFormat::Xlsx {
            return Some((
                self.excel_workbook(slot, columns, rows, cx),
                super::ExportFormat::Xlsx,
            ));
        }
        crate::open_in_spreadsheet::spreadsheet_bytes(prefs, self.source_db_type(), columns, rows)
    }

    /// The `.xlsx` bytes for exporting `rows` (result-column order, already
    /// masked and in display order) of result `slot`.
    pub(crate) fn excel_workbook(
        &self,
        slot: usize,
        columns: &[ResultColumn],
        rows: &[Vec<CellValue>],
        cx: &App,
    ) -> Vec<u8> {
        let mut workbook = Workbook::default();
        let name = self.result_set_label(slot);
        workbook
            .sheets
            .push(self.grid_sheet(slot, &name, columns, rows, &mut workbook.names, cx));
        if let Some(pivot) = self.pivot_sheet(slot, &name, cx) {
            workbook.sheets.push(pivot);
        }
        xlsx_workbook::write(&workbook)
    }

    fn grid_sheet(
        &self,
        slot: usize,
        name: &str,
        columns: &[ResultColumn],
        rows: &[Vec<CellValue>],
        names: &mut Vec<(String, String)>,
        cx: &App,
    ) -> Sheet {
        let grid = self
            .grid_views
            .get(slot)
            .and_then(|g| g.as_ref())
            .map(|g| g.read(cx).state.read(cx));
        // Display order (visible columns), then the hidden ones, hidden.
        let (order, hidden, frozen, widths, live_decimals) = match grid {
            Some(state) => {
                let marker = grid_marker_width(&state.data.columns, columns.len());
                let to_result = |g: &usize| g.checked_sub(marker).filter(|c| *c < columns.len());
                let order: Vec<usize> = state.column_order().iter().filter_map(to_result).collect();
                let hidden: Vec<usize> =
                    (0..columns.len()).filter(|c| !order.contains(c)).collect();
                let widths: Vec<f32> = (0..columns.len())
                    .map(|c| {
                        state
                            .data
                            .columns
                            .get(c + marker)
                            .map_or(120.0, |col| col.width)
                    })
                    .collect();
                let decimals: Vec<Option<(usize, bool)>> = (0..columns.len())
                    .map(|c| {
                        state
                            .config
                            .column_overrides
                            .get(c + marker)
                            .and_then(|o| o.number)
                            .map(|n| (n.decimals, n.thousands_separator))
                    })
                    .collect();
                (
                    order,
                    hidden,
                    state.frozen_columns().saturating_sub(marker),
                    Some(widths),
                    decimals,
                )
            }
            None => (
                (0..columns.len()).collect(),
                Vec::new(),
                0,
                None,
                vec![None; columns.len()],
            ),
        };
        let export_order: Vec<usize> = order.iter().chain(hidden.iter()).copied().collect();
        let letter_of: HashMap<String, String> = export_order
            .iter()
            .enumerate()
            .map(|(pos, &c)| {
                (
                    columns[c].name.to_ascii_lowercase(),
                    crate::result_export::excel_column_label(pos + 1),
                )
            })
            .collect();

        let hint = |c: usize| {
            self.format_hints
                .iter()
                .find(|h| h.column.eq_ignore_ascii_case(&columns[c].name))
        };
        let kinds: Vec<Kind> = columns.iter().map(|c| kind_of(&c.data_type)).collect();
        let sheet_columns: Vec<SheetColumn> = export_order
            .iter()
            .map(|&c| {
                let format = match hint(c).map(|h| &h.format) {
                    Some(QueryColumnFormat::Number {
                        decimals,
                        thousands,
                    }) => Some(number_format(*decimals, *thousands)),
                    Some(QueryColumnFormat::NumberStyle {
                        style: crate::preferences::NumberStyle::Percent,
                        decimals,
                        ..
                    }) => Some(format!("{}%", number_format(usize::from(*decimals), false))),
                    Some(QueryColumnFormat::Date { format }) => Some(strftime_to_excel(format)),
                    _ => match (live_decimals.get(c).copied().flatten(), kinds[c]) {
                        (Some((d, t)), Kind::Number) => Some(number_format(d, t)),
                        (_, Kind::Date) => Some("yyyy-mm-dd".into()),
                        (_, Kind::DateTime) => Some("yyyy-mm-dd hh:mm:ss".into()),
                        (_, Kind::Time) => Some("hh:mm:ss".into()),
                        _ => None,
                    },
                };
                SheetColumn {
                    width: widths.as_ref().map(|w| excel_width(w[c])),
                    hidden: hidden.contains(&c),
                    number_format: format,
                }
            })
            .collect();

        // Computed columns become formulas when their row references mean
        // the same thing in the sheet: same-row formulas always do; formulas
        // reading other rows only when the rows go out in the query's order.
        let plan = self.computed_plans.get(slot).and_then(|p| p.as_ref());
        let in_query_order = self
            .result_sets
            .get(slot)
            .is_some_and(|rs| rs.rows.len() == rows.len() && rs.rows.as_slice() == rows);
        let formulas: HashMap<usize, &crate::computed_columns::ComputedColumn> = plan
            .map(|plan| {
                plan.layout
                    .iter()
                    .enumerate()
                    .filter_map(|(pos, s)| match s {
                        Slot::Computed(k) => Some((pos, &plan.columns[*k])),
                        Slot::Source(_) => None,
                    })
                    .filter(|(_, spec)| in_query_order || !spec.formula.reads_other_rows())
                    .collect()
            })
            .unwrap_or_default();
        let mut used_vars: Vec<String> = Vec::new();

        let sheet_rows: Vec<Vec<Cell>> = rows
            .iter()
            .enumerate()
            .map(|(r, row)| {
                export_order
                    .iter()
                    .map(|&c| {
                        let value = row.get(c).unwrap_or(&CellValue::Null);
                        let cached = self.excel_cell(value, kinds[c], hint(c).map(|h| &h.format));
                        match formulas.get(&c) {
                            Some(spec) => {
                                let letter =
                                    |name: &str| letter_of.get(&name.to_ascii_lowercase()).cloned();
                                let var = |name: &str| xlsx_workbook::defined_name(name);
                                let ctx = ExcelRefContext {
                                    column_letter: &letter,
                                    first_data_row: 2,
                                    row_count: rows.len() as u32,
                                    row: r as i64,
                                    variable_name: &var,
                                };
                                match formula::to_excel_formula(&spec.formula.expr, &ctx) {
                                    Some(f) => Cell::Formula {
                                        formula: f,
                                        cached: Box::new(cached),
                                    },
                                    None => cached,
                                }
                            }
                            None => cached,
                        }
                    })
                    .collect()
            })
            .collect();
        for spec in formulas.values() {
            collect_vars(&spec.formula.expr, &mut used_vars);
        }
        for var in used_vars {
            let Some((_, value)) = self
                .formula_variables
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(&var))
            else {
                continue;
            };
            let constant = match value {
                formula::Value::Number(n) => format!("{n}"),
                formula::Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
                formula::Value::Text(t) => format!("\"{}\"", t.replace('"', "\"\"")),
                _ => "\"\"".into(),
            };
            let defined = xlsx_workbook::defined_name(&var);
            if !names.iter().any(|(n, _)| n.eq_ignore_ascii_case(&defined)) {
                names.push((defined, constant));
            }
        }

        // Conditional formatting: the query's rules, then the saved ones.
        let saved = crate::conditional_formatting::load_rules();
        let conditional: Vec<ConditionalBlock> = export_order
            .iter()
            .enumerate()
            .filter_map(|(pos, &c)| {
                let rules: Vec<ConditionalKind> = self
                    .conditional_directives
                    .iter()
                    .chain(saved.iter())
                    .filter(|r| r.column.eq_ignore_ascii_case(&columns[c].name))
                    .map(|r| r.kind.clone())
                    .collect();
                (!rules.is_empty()).then_some(ConditionalBlock { column: pos, rules })
            })
            .collect();

        Sheet {
            name: name.to_string(),
            columns: sheet_columns,
            header_rows: vec![
                export_order
                    .iter()
                    .map(|&c| columns[c].name.clone())
                    .collect(),
            ],
            rows: sheet_rows,
            freeze_columns: frozen.min(order.len()),
            table: true,
            conditional,
        }
    }

    /// One exported cell: dates as serial numbers (shown with the column's
    /// date format), numeric text as numbers, case directives applied.
    fn excel_cell(
        &self,
        value: &CellValue,
        kind: Kind,
        format: Option<&QueryColumnFormat>,
    ) -> Cell {
        match value {
            CellValue::Null => Cell::Empty,
            CellValue::Bool(b) => match format {
                Some(QueryColumnFormat::Boolean {
                    true_text,
                    false_text,
                }) => Cell::Text(if *b { true_text } else { false_text }.clone()),
                _ => Cell::Bool(*b),
            },
            CellValue::I64(n) => Cell::Number(*n as f64),
            CellValue::F64(n) => Cell::Number(*n),
            CellValue::Bytes(b) => Cell::Text(format!(
                "0x{}",
                b.iter().map(|x| format!("{x:02X}")).collect::<String>()
            )),
            CellValue::String(s) => {
                if s.starts_with('#')
                    && [
                        "#NULL!", "#DIV/0!", "#VALUE!", "#REF!", "#NAME?", "#NUM!", "#N/A",
                    ]
                    .contains(&s.as_str())
                {
                    return Cell::Error(s.clone());
                }
                match kind {
                    Kind::Date | Kind::DateTime | Kind::Time => formula::parse_datetime_text(s)
                        .map(Cell::Number)
                        .unwrap_or_else(|| Cell::Text(s.clone())),
                    Kind::Number => crate::result_cell_format::numeric_literal(s)
                        .and_then(|n| n.parse::<f64>().ok())
                        .map(Cell::Number)
                        .unwrap_or_else(|| Cell::Text(s.clone())),
                    Kind::Other => Cell::Text(match format {
                        Some(QueryColumnFormat::Text(TextCase::Upper)) => s.to_uppercase(),
                        Some(QueryColumnFormat::Text(TextCase::Lower)) => s.to_lowercase(),
                        Some(QueryColumnFormat::Text(TextCase::Title)) => title_case(s),
                        _ => s.clone(),
                    }),
                }
            }
        }
    }

    /// The pivot sheet: the pivot exactly as shown — expanded and collapsed
    /// groups, sort, filters, subtotals — when the result has one set up.
    fn pivot_sheet(&self, slot: usize, name: &str, cx: &App) -> Option<Sheet> {
        let grid = self.grid_views.get(slot)?.as_ref()?.read(cx);
        let pivot = grid.pivot_state()?.read(cx);
        pivot.config.value_field?;
        let result = &pivot.result;
        let cols = pivot.visible_cols().to_vec();
        let rows = pivot.visible_rows().to_vec();
        let depth = result.col_depth.max(1);
        let mut header_rows = vec![vec![String::new(); cols.len() + 1]; depth];
        header_rows[depth - 1][0] = if result.row_field_names.is_empty() {
            String::new()
        } else {
            result.row_field_names.join(" / ")
        };
        for (i, col) in cols.iter().enumerate() {
            let leaf_level = if matches!(col.kind, VisibleColKind::GrandTotal)
                || col.key == sqlly_datatable::pivot::TOTAL_KEY
            {
                depth - 1
            } else {
                col.depth.min(depth - 1)
            };
            header_rows[leaf_level][i + 1] = pivot.col_label(col);
            // Ancestor group labels above, as Excel prints a column header.
            let mut node = result.col_nodes.get(col.key).and_then(|n| n.parent);
            while let Some(id) = node {
                let Some(n) = result.col_nodes.get(id) else {
                    break;
                };
                if n.depth < depth && header_rows[n.depth][i + 1].is_empty() {
                    header_rows[n.depth][i + 1] = n.label.clone();
                }
                node = n.parent;
            }
        }
        let sheet_rows: Vec<Vec<Cell>> = rows
            .iter()
            .map(|row| {
                let bold = matches!(
                    row.kind,
                    VisibleRowKind::GrandTotal | VisibleRowKind::GroupHeader { .. }
                );
                let mut out = vec![Cell::Styled {
                    cell: Box::new(Cell::Text(pivot.row_label(row))),
                    indent: u8::try_from(row.depth).unwrap_or(u8::MAX),
                    bold,
                }];
                for col in &cols {
                    let value = match pivot.cell_value(row, col) {
                        sqlly_datatable::CellValue::Integer(n) => Cell::Number(*n as f64),
                        sqlly_datatable::CellValue::Decimal(n) => Cell::Number(*n),
                        // Same as the data sheet: a number, which Excel
                        // holds to 15 digits.
                        sqlly_datatable::CellValue::ExactDecimal(d) => Cell::Number(d.to_f64()),
                        sqlly_datatable::CellValue::Boolean(b) => Cell::Bool(*b),
                        sqlly_datatable::CellValue::Text(t) => Cell::Text(t.clone()),
                        sqlly_datatable::CellValue::Date(secs) => {
                            Cell::Number(*secs as f64 / 86_400.0 + 25_569.0)
                        }
                        _ => Cell::Empty,
                    };
                    out.push(if bold {
                        Cell::Styled {
                            cell: Box::new(value),
                            indent: 0,
                            bold: true,
                        }
                    } else {
                        value
                    });
                }
                out
            })
            .collect();
        let value_format = pivot
            .config
            .value_format
            .map(|f| number_format(f.decimals, f.thousands_separator));
        let mut columns = vec![SheetColumn {
            width: Some(excel_width(pivot.row_header_width.max(120.0))),
            ..Default::default()
        }];
        columns.extend(cols.iter().map(|_| SheetColumn {
            width: Some(excel_width(pivot.value_col_width)),
            hidden: false,
            number_format: value_format.clone(),
        }));
        Some(Sheet {
            name: crate::localization::format(
                "results.excel_export.pivot_sheet_name",
                &[("source_name", &name)],
            ),
            columns,
            header_rows,
            rows: sheet_rows,
            freeze_columns: 1,
            table: false,
            conditional: Vec::new(),
        })
    }
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut start = true;
    for c in s.chars() {
        if c.is_alphabetic() {
            if start {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            start = false;
        } else {
            out.push(c);
            start = true;
        }
    }
    out
}

fn collect_vars(expr: &formula::Expr, out: &mut Vec<String>) {
    use formula::Expr;
    match expr {
        Expr::Var(name) => {
            if !out.iter().any(|v| v.eq_ignore_ascii_case(name)) {
                out.push(name.clone());
            }
        }
        Expr::Neg(e) | Expr::Percent(e) => collect_vars(e, out),
        Expr::Binary(_, a, b) => {
            collect_vars(a, out);
            collect_vars(b, out);
        }
        Expr::Call(_, args) => args.iter().for_each(|a| collect_vars(a, out)),
        _ => {}
    }
}
