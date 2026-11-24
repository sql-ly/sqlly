//! The Operators view: every operator of the statement in one sortable grid,
//! beside the details panel. Sorting, column sizing, copy and export are the
//! results grid's own.

use gpui::{AnyElement, Context, Entity, IntoElement, Styled, Subscription, div, prelude::*};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::{Selection, SqllyDataTable};
use sqlly_services::plans::{OperatorTableColumn, OperatorTableRow, operator_table_columns};

use super::{ExecutionPlanView, is_wide};
use crate::tool_dialog::{ToolQuerySet, selected_source_row};

#[derive(Default)]
pub(super) struct TableState {
    grid: Option<Entity<SqllyDataTable>>,
    /// Operator id of each source row of the grid.
    node_ids: Vec<i32>,
    /// Interface-language revision the headers were written in.
    chrome_revision: u64,
    /// The selection the grid was last told about, or told us about.
    synced_node: Option<i32>,
    subscription: Option<Subscription>,
}

impl TableState {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    pub(super) fn grid(&self) -> Option<&Entity<SqllyDataTable>> {
        self.grid.as_ref()
    }
}

/// A column header: the core's label, with the unit where the cells are
/// bare numbers so they sort as numbers.
fn header(column: OperatorTableColumn) -> String {
    use OperatorTableColumn as C;
    let label = crate::localization::message(column.label());
    let key = match column {
        C::SelfTime | C::TotalTime => "query.execution_plan.table_header_ms",
        C::Memory => "query.execution_plan.table_header_kb",
        C::EstimateFactor => "query.execution_plan.table_header_factor",
        _ => return label,
    };
    crate::localization::format(key, &[("label", &label)])
}

fn count(value: Option<f64>) -> CellValue {
    match value {
        Some(value) if value.is_finite() && value.fract() == 0.0 && value.abs() < 9.0e15 => {
            CellValue::I64(value as i64)
        }
        Some(value) if value.is_finite() => CellValue::F64(value),
        _ => CellValue::Null,
    }
}

fn number(value: Option<f64>, decimals: i32) -> CellValue {
    match value {
        Some(value) if value.is_finite() => {
            let scale = 10f64.powi(decimals);
            CellValue::F64((value * scale).round() / scale)
        }
        _ => CellValue::Null,
    }
}

fn text(value: Option<&str>) -> CellValue {
    value.map_or(CellValue::Null, |value| {
        CellValue::String(value.to_string())
    })
}

/// One grid cell. Numbers stay numbers so the grid sorts and sums them.
pub(super) fn cell(row: &OperatorTableRow, column: OperatorTableColumn) -> CellValue {
    use OperatorTableColumn as C;
    match column {
        C::Step => CellValue::I64(row.step as i64),
        C::Operator => CellValue::String(row.operator.clone()),
        C::Object => match (&row.object, &row.index) {
            (Some(object), Some(index)) => CellValue::String(format!("{object} ({index})")),
            (Some(object), None) => CellValue::String(object.clone()),
            (None, index) => text(index.as_deref()),
        },
        C::SelfTime => number(row.self_time_ms, 3),
        C::TotalTime => number(row.inclusive_time_ms, 3),
        C::TimeShare => number(row.time_share.map(|share| share * 100.0), 1),
        C::SelfCost => number(row.self_cost, 4),
        C::TotalCost => number(row.inclusive_cost, 4),
        C::CostShare => number(row.cost_share.map(|share| share * 100.0), 1),
        C::Rows => count(row.rows),
        C::EstimatedRows => count(row.estimated_rows),
        C::EstimateFactor => number(row.estimate_factor, 1),
        C::Loops => count(row.loops),
        C::RowsRemoved => count(row.rows_removed),
        C::Io => count(row.io_pages),
        C::Memory => count(row.memory_kb.map(f64::round)),
    }
}

/// Decimal places a fractional column shows. `None` is a text or whole-number
/// column.
fn decimals(column: OperatorTableColumn) -> Option<i32> {
    use OperatorTableColumn as C;
    match column {
        C::SelfTime | C::TotalTime => Some(3),
        C::SelfCost | C::TotalCost => Some(2),
        C::TimeShare | C::CostShare | C::EstimateFactor => Some(1),
        _ => None,
    }
}

/// The grid over the operator rows. It follows the user's Results number
/// settings (separators, negative style) but fixes the decimal places per
/// column, since a time in milliseconds and a share in percent need
/// different precision, and leaves a missing measurement blank: an operator
/// with no table is not a NULL the engine returned.
fn build_grid(
    set: &ToolQuerySet,
    columns: &[OperatorTableColumn],
    cx: &mut gpui::App,
) -> Option<Entity<SqllyDataTable>> {
    if set.columns.is_empty() {
        return None;
    }
    let mut prefs = crate::preferences::ResultFormattingPreferences::load();
    prefs.column_overrides.clear();
    for (column, header) in columns.iter().zip(&set.columns) {
        if let Some(places) = decimals(*column) {
            let mut format = crate::preferences::ColumnFormat::default_for_kind(
                crate::preferences::ResultKind::Decimal,
            );
            format.decimal_format = prefs.defaults.decimal_format.clone();
            format.decimal_format.decimal_places = places;
            format.decimal_format.reduce_large_to_metric = false;
            prefs.column_overrides.insert(header.name.clone(), format);
        }
    }
    let mut config = crate::grid_bridge::build_grid_config(None, &prefs, &set.columns);
    config.conditional_rules.clear();
    config.default_null.text = String::new();
    let data = crate::grid_bridge::build_grid_data(None, &set.columns, &set.rows).ok()?;
    let view = SqllyDataTable::builder(data)
        .config(config)
        .context_menu_visibility(crate::grid_bridge::grid_menu_tooltip_policy)
        .theme(crate::grid_bridge::build_grid_theme())
        .build(cx);
    Some(cx.new(|_cx| view))
}

pub(super) fn table_set(rows: &[OperatorTableRow]) -> ToolQuerySet {
    let columns = operator_table_columns(rows);
    ToolQuerySet {
        columns: columns
            .iter()
            .map(|column| ResultColumn {
                name: header(*column),
                data_type: match column {
                    OperatorTableColumn::Operator | OperatorTableColumn::Object => "nvarchar",
                    column if decimals(*column).is_some() => "float",
                    _ => "bigint",
                }
                .to_string(),
                nullable: true,
            })
            .collect(),
        rows: rows
            .iter()
            .map(|row| columns.iter().map(|column| cell(row, *column)).collect())
            .collect(),
    }
}

impl ExecutionPlanView {
    fn ensure_table(&mut self, cx: &mut Context<Self>) {
        let chrome = crate::localization::chrome_revision();
        if self.table.grid.is_some() && self.table.chrome_revision == chrome {
            return;
        }
        let Some(active) = self.statements.get(self.active_statement) else {
            return;
        };
        let set = table_set(&active.table);
        let columns = operator_table_columns(&active.table);
        let node_ids = active.table.iter().map(|row| row.node_id).collect();
        let grid = build_grid(&set, &columns, cx);
        self.table.subscription = grid.as_ref().map(|grid| {
            let state = grid.read(cx).state.clone();
            let grid = grid.clone();
            cx.observe(&state, move |this, _, cx| {
                let node = selected_source_row(&grid, cx)
                    .and_then(|row| this.table.node_ids.get(row).copied());
                if let Some(node) = node
                    && this.table.synced_node != Some(node)
                {
                    this.table.synced_node = Some(node);
                    this.select_node(Some(node), cx);
                }
            })
        });
        self.table.grid = grid;
        self.table.node_ids = node_ids;
        self.table.chrome_revision = chrome;
        self.table.synced_node = None;
    }

    /// Put the grid's selection on the operator the other views selected.
    fn sync_table_selection(&mut self, cx: &mut Context<Self>) {
        if self.table.synced_node == self.selected_node {
            return;
        }
        self.table.synced_node = self.selected_node;
        let (Some(grid), Some(node_id)) = (self.table.grid.clone(), self.selected_node) else {
            return;
        };
        let Some(source) = self.table.node_ids.iter().position(|id| *id == node_id) else {
            return;
        };
        let rows = self.table.node_ids.len();
        let state = grid.read(cx).state.clone();
        state.update(cx, |state, cx| {
            if let Some(display) =
                (0..rows).find(|display| state.resident_row_for_display(*display) == Some(source))
            {
                state.selection = Selection::Row(display);
                cx.notify();
            }
        });
    }

    pub(super) fn render_table(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.ensure_table(cx);
        self.sync_table_selection(cx);
        let wide = self
            .body_bounds
            .get()
            .is_none_or(|bounds| is_wide(bounds.size.width));
        let Some(active) = self.statements.get(self.active_statement) else {
            return div().into_any_element();
        };
        div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .children(self.table.grid.clone()),
            )
            .when(wide, |row| row.child(self.render_details(active, cx)))
            .into_any_element()
    }
}
