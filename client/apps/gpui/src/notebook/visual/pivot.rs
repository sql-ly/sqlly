//! Pivot cells: the grid's pivot over the input rows, its layout kept in
//! the cell's spec by column name. Double-clicking a pivot value drills to
//! the rows behind it on the grid tab; the pivot sidebar's Save button (and
//! the cell's Save Layout) write the layout back to the spec.

use std::hash::{Hash, Hasher};

use gpui::{
    AnyElement, AppContext as _, Context, Entity, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Styled as _, Window, div, prelude::FluentBuilder as _, px,
};
use sqlly_core::{CellValue, DisplayMessage, ResultColumn};
use sqlly_datatable::pivot::PivotHitResult;
use sqlly_datatable::{
    AggregationFn, DEFAULT_PIVOT_SIDEBAR_WIDTH, GridConfig, GridTab, PivotConfig, PivotGrid,
    PivotSidebar, SqllyDataTable,
};
use sqlly_services::notebook::cells::pivot::{PivotAggregation, PivotCellSpec, PivotLayout};
use sqlly_services::notebook::cells::spec;

use super::{VisualCellView, VisualInputs, dim, small_button, t};
use crate::theme::Theme;

/// Default pivot height in the notebook, in logical pixels.
const PIVOT_HEIGHT: f32 = 360.0;

/// Bounds of an output-only pivot's height when it fits its content.
const FIT_MIN: f32 = 120.0;
const FIT_MAX: f32 = 480.0;

#[derive(Default)]
pub struct PivotPart {
    pub(crate) grid: Option<Entity<SqllyDataTable>>,
    built: u64,
    /// The pivot canvas and sidebar over the grid's own pivot state, for
    /// the output-only view (no tab bar).
    views: Option<(gpui::EntityId, Entity<PivotGrid>, Entity<PivotSidebar>)>,
    /// A drill-through from the output-only view: the grid shows the rows
    /// until the user goes back (`true` once its Grid tab is up).
    pub(crate) drill: Option<bool>,
}

pub(crate) fn evaluate(
    inputs: &VisualInputs,
) -> Result<(Vec<ResultColumn>, Vec<Vec<CellValue>>), DisplayMessage> {
    super::pass_through(inputs)
}

fn aggregation(a: PivotAggregation) -> AggregationFn {
    match a {
        PivotAggregation::Count => AggregationFn::Count,
        PivotAggregation::Sum => AggregationFn::Sum,
        PivotAggregation::Avg => AggregationFn::Avg,
        PivotAggregation::Min => AggregationFn::Min,
        PivotAggregation::Max => AggregationFn::Max,
        PivotAggregation::First => AggregationFn::First,
    }
}

fn layout_aggregation(a: AggregationFn) -> PivotAggregation {
    match a {
        AggregationFn::Count => PivotAggregation::Count,
        AggregationFn::Sum => PivotAggregation::Sum,
        AggregationFn::Avg => PivotAggregation::Avg,
        AggregationFn::Min => PivotAggregation::Min,
        AggregationFn::Max => PivotAggregation::Max,
        AggregationFn::First => PivotAggregation::First,
    }
}

fn index(columns: &[ResultColumn], name: &str) -> Option<usize> {
    columns.iter().position(|c| c.name == name).or_else(|| {
        columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    })
}

/// The grid's pivot configuration for a layout; names the output lacks are
/// left out.
pub fn pivot_config(layout: &PivotLayout, columns: &[ResultColumn]) -> PivotConfig {
    let indices = |names: &[String]| names.iter().filter_map(|n| index(columns, n)).collect();
    PivotConfig {
        row_fields: indices(&layout.rows),
        column_fields: indices(&layout.columns),
        value_field: layout.value.as_deref().and_then(|v| index(columns, v)),
        aggregation: aggregation(layout.aggregation),
        filter_fields: indices(&layout.filters),
        flat_rows: layout.flat_rows,
        show_row_subtotals: layout.show_row_subtotals,
        show_column_subtotals: layout.show_column_subtotals,
        show_row_grand_total: layout.show_row_grand_total,
        show_column_grand_total: layout.show_column_grand_total,
        blank_label: layout.blank_label.clone(),
        ..PivotConfig::default()
    }
}

/// The layout a grid pivot configuration describes, by column name.
pub fn layout_of(config: &PivotConfig, columns: &[ResultColumn]) -> PivotLayout {
    let names = |fields: &[usize]| {
        fields
            .iter()
            .filter_map(|i| columns.get(*i).map(|c| c.name.clone()))
            .collect()
    };
    PivotLayout {
        rows: names(&config.row_fields),
        columns: names(&config.column_fields),
        value: config
            .value_field
            .and_then(|i| columns.get(i).map(|c| c.name.clone())),
        aggregation: layout_aggregation(config.aggregation),
        filters: names(&config.filter_fields),
        flat_rows: config.flat_rows,
        show_row_subtotals: config.show_row_subtotals,
        show_column_subtotals: config.show_column_subtotals,
        show_row_grand_total: config.show_row_grand_total,
        show_column_grand_total: config.show_column_grand_total,
        blank_label: config.blank_label.clone(),
    }
}

impl VisualCellView {
    fn pivot_spec(&self) -> PivotCellSpec {
        spec::read(&self.inputs.spec)
    }

    fn save_pivot_layout(&mut self, config: &PivotConfig, cx: &mut Context<Self>) {
        let columns = self.inputs.input_columns();
        let mut cell = self.pivot_spec();
        cell.pivot = layout_of(config, &columns);
        self.emit_spec(spec::write(&cell), cx);
    }

    pub(crate) fn render_pivot(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let Ok((columns, rows)) = evaluate(&self.inputs) else {
            return div()
                .h(px(s(60.0)))
                .px(px(s(12.0)))
                .flex()
                .items_center()
                .child(dim(t("notebook.visual.pivot.no_data")))
                .into_any_element();
        };
        let cell = self.pivot_spec();
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.inputs
            .input
            .as_ref()
            .and_then(|i| i.output.as_ref())
            .map(|o| o.run_id.clone())
            .hash(&mut h);
        rows.len().hash(&mut h);
        format!("{:?}{:?}", cell.pivot, self.inputs.filter).hash(&mut h);
        crate::notebook::visual::image::is_dark().hash(&mut h);
        let key = h.finish();
        if key != self.pivot.built || self.pivot.grid.is_none() {
            self.pivot.built = key;
            let config = pivot_config(&cell.pivot, &columns);
            let weak = cx.weak_entity();
            match crate::grid_bridge::build_grid_data(None, &columns, &rows) {
                Ok(data) => {
                    let theme = crate::grid_bridge::build_grid_theme();
                    let table = SqllyDataTable::builder(data)
                        .config(GridConfig::default())
                        .theme(theme)
                        .pivot(config)
                        .pivot_save_config(move |config, app| {
                            let config = config.clone();
                            weak.update(app, |v, cx| v.save_pivot_layout(&config, cx))
                                .ok();
                        })
                        .build(cx);
                    let grid = cx.new(|_| table);
                    grid.update(cx, |g, cx| g.set_active_tab(GridTab::Pivot, cx));
                    self.pivot.grid = Some(grid);
                }
                Err(_) => self.pivot.grid = None,
            }
        }
        let Some(grid) = self.pivot.grid.clone() else {
            return dim(t("notebook.visual.pivot.no_data")).into_any_element();
        };
        if self.output_only {
            return self.render_pivot_output(grid, cx);
        }
        let id = self.inputs.cell_id.clone();
        let save_grid = grid.clone();
        let mut root = div().flex().flex_col().w_full();
        if self.editing {
            root = root.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .px(px(s(10.0)))
                    .py(px(s(4.0)))
                    .child(dim(t("notebook.visual.pivot.help")))
                    .child(
                        small_button(
                            format!("nb-pivot-save-{id}"),
                            t("notebook.visual.pivot.save_layout"),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let config = save_grid
                                .read(cx)
                                .pivot_state()
                                .map(|p| p.read(cx).config.clone());
                            if let Some(config) = config {
                                this.save_pivot_layout(&config, cx);
                            }
                        })),
                    ),
            );
        }
        root.child(
            div()
                .h(px(s(PIVOT_HEIGHT)))
                .w_full()
                .min_h_0()
                .child(crate::notebook::cells::frame_cached(grid)),
        )
        .into_any_element()
    }
}

impl VisualCellView {
    /// The pivot canvas alone (the grid's own pivot state, no tab bar), its
    /// sidebar beside it when the configuration is shown. A double-click on
    /// a value drills through as in the full grid: the grid shows the rows
    /// behind it, with a way back to the pivot.
    fn render_pivot_output(
        &mut self,
        grid: Entity<SqllyDataTable>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let id = self.inputs.cell_id.clone();
        let Some(state) = grid.read(cx).pivot_state().cloned() else {
            return div().child(grid).into_any_element();
        };
        if let Some(seen) = self.pivot.drill {
            let tab = grid.read(cx).active_tab();
            match (seen, tab) {
                (false, GridTab::Grid) => self.pivot.drill = Some(true),
                (true, GridTab::Pivot) => self.pivot.drill = None,
                _ => {}
            }
        }
        if self.pivot.drill.is_some() {
            let back = grid.clone();
            return div()
                .flex()
                .flex_col()
                .w_full()
                .child(
                    div().px(px(s(8.0))).py(px(s(2.0))).child(
                        small_button(
                            format!("nb-pivot-back-{id}"),
                            t("notebook.embedded.pivot.back"),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            back.update(cx, |g, cx| g.set_active_tab(GridTab::Pivot, cx));
                            this.pivot.drill = None;
                            cx.notify();
                        })),
                    ),
                )
                .child(
                    div()
                        .h(px(self.display.height.unwrap_or(s(PIVOT_HEIGHT))))
                        .w_full()
                        .min_h_0()
                        .child(crate::notebook::cells::frame_cached(grid)),
                )
                .into_any_element();
        }
        if self
            .pivot
            .views
            .as_ref()
            .is_none_or(|(of, _, _)| *of != state.entity_id())
        {
            let canvas = cx.new(|_| PivotGrid::new(state.clone()));
            let sidebar = cx.new(|_| PivotSidebar::new(state.clone()));
            self.pivot.views = Some((state.entity_id(), canvas, sidebar));
        }
        let Some((_, canvas, sidebar)) = self.pivot.views.clone() else {
            return div().into_any_element();
        };
        let config = self.display.config_shown;
        let height = self.display.height.unwrap_or_else(|| {
            let p = state.read(cx);
            let fit = (p.header_height() + p.content_size().1 + 16.0).clamp(FIT_MIN, FIT_MAX);
            if config {
                fit.max(s(PIVOT_HEIGHT))
            } else {
                fit
            }
        });
        let hit_state = state.clone();
        div()
            .id(gpui::ElementId::Name(
                format!("nb-pivot-output-{id}").into(),
            ))
            .debug_selector(move || format!("nb-pivot-output-{id}"))
            .flex()
            .flex_row()
            .h(px(height))
            .w_full()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            if event.click_count < 2 {
                                return;
                            }
                            let p = hit_state.read(cx);
                            let at = event.position - p.bounds.origin;
                            if matches!(p.hit_test(at), PivotHitResult::Cell { .. }) {
                                this.pivot.drill = Some(false);
                                cx.notify();
                            }
                        }),
                    )
                    .child(canvas),
            )
            .when(config, |d| {
                let selector = format!("nb-pivot-sidebar-{}", self.inputs.cell_id);
                d.child(
                    div()
                        .debug_selector(move || selector.clone())
                        .w(px(DEFAULT_PIVOT_SIDEBAR_WIDTH))
                        .flex_none()
                        .h_full()
                        .border_l_1()
                        .border_color(Theme::border())
                        .child(sidebar),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_and_grid_configs_map_by_name() {
        let col = |name: &str| ResultColumn {
            name: name.into(),
            data_type: "varchar".into(),
            nullable: true,
        };
        let columns = vec![col("country"), col("year"), col("revenue")];
        let layout = PivotLayout {
            rows: vec!["Country".into()],
            columns: vec!["year".into(), "missing".into()],
            value: Some("revenue".into()),
            aggregation: PivotAggregation::Avg,
            flat_rows: true,
            ..PivotLayout::default()
        };
        let config = pivot_config(&layout, &columns);
        assert_eq!(config.row_fields, vec![0]);
        assert_eq!(config.column_fields, vec![1]);
        assert_eq!(config.value_field, Some(2));
        assert_eq!(config.aggregation, AggregationFn::Avg);
        assert!(config.flat_rows);
        let back = layout_of(&config, &columns);
        assert_eq!(back.rows, vec!["country".to_string()]);
        assert_eq!(back.columns, vec!["year".to_string()]);
        assert_eq!(back.aggregation, PivotAggregation::Avg);
        assert_eq!(back.blank_label, layout.blank_label);
    }
}
