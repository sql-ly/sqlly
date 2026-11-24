//! Chart cells. The seven kinds the grid charts live (bar, line, area,
//! scatter, histogram, pie, donut) draw through the datatable's chart state
//! and canvas, and a click on a bar, point or slice sets this chart's
//! cross-filter; every other kind, and any spec using a setting the live
//! chart lacks, draws through the report's SVG renderer, the same drawing
//! every export uses, and a click there is hit-tested on the drawn scene's
//! marks (bars, line and area categories, slices, funnel stages, treemap
//! tiles, waterfall steps). The live hit test still mirrors the datatable's
//! private chart layout; a public hit test there needs a crate release. The editor offers the top suggested charts for the
//! input and every encoding and option of the spec.

use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, AppContext as _, Bounds, Context, Entity, InteractiveElement as _, IntoElement,
    MouseButton, ParentElement as _, Pixels, Styled as _, StyledImage as _, Window, canvas, div,
    img, prelude::FluentBuilder as _, px,
};
use gpui_component::button::ButtonVariants as _;
use sqlly_core::{CellValue, DisplayMessage, ResultColumn};
use sqlly_datatable::{ChartCanvas, ChartState};
use sqlly_services::analytics::suggest::ChartSuggestion;
use sqlly_services::notebook::cells::chart::{
    self as spec_chart, AnomalyConfig, ChartCellSpec, SUGGESTIONS_SHOWN,
};
use sqlly_services::notebook::cells::spec::{self, CrossFilter};
use sqlly_services::report::chart::{
    Aggregate, Annotation, BarMode, ChartKind, ChartSpec, DateGrain, Encoding, NullTreatment,
    Orientation, ReferenceLine, Sort, SortBy, TopN,
};
use sqlly_services::report::format::{NumberStyle, ValueFormat};
use sqlly_services::report::labels::ReportLabels;
use sqlly_services::report::svg::{ChartOptions, Scene, render_chart, to_svg};

use super::chart_map::{LivePlan, PlotClick, StaticReason, hit_category, live_plan};
use super::image::{CachedImage, report_theme};
use super::{
    VisualCellView, VisualEvent, VisualInputs, choice_button, column_options, dim, editor_panel,
    editor_row, small_button, t, text_field,
};
use crate::theme::Theme;

/// Height of the live chart's legend strip above its plot.
const LEGEND_STRIP: f32 = 26.0;

#[derive(Default)]
pub struct ChartPart {
    state: Option<Entity<ChartState>>,
    canvas: Option<Entity<ChartCanvas>>,
    live_key: u64,
    /// The live plan last drawn, for click hit-testing.
    plan: Option<LivePlan>,
    image: CachedImage,
    /// The SVG-drawn chart's scene, for click hit-testing, with its key.
    pub(crate) scene: Option<(u64, Arc<Scene>)>,
    pub(crate) bounds: Rc<Cell<Bounds<Pixels>>>,
    suggestions: Option<(u64, Vec<ChartSuggestion>)>,
}

impl ChartPart {
    /// Whether the last draw used the live chart.
    #[cfg(test)]
    pub(crate) fn is_live(&self) -> bool {
        self.state.is_some() && self.plan.is_some()
    }
}

/// A point in a frame of `width` × `height` as a point of `scene`, drawn
/// into the frame scaled to fit and centered (`ObjectFit::Contain`).
pub(crate) fn scene_point(
    scene: &Scene,
    width: f32,
    height: f32,
    x: f32,
    y: f32,
) -> Option<(f32, f32)> {
    if scene.width <= 0.0 || scene.height <= 0.0 || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let scale = (width / scene.width).min(height / scene.height);
    let (dx, dy) = (
        (width - scene.width * scale) / 2.0,
        (height - scene.height * scale) / 2.0,
    );
    Some(((x - dx) / scale, (y - dy) / scale))
}

pub(crate) fn kind_label(kind: ChartKind) -> String {
    t(kind_key(kind))
}

fn kind_key(kind: ChartKind) -> &'static str {
    match kind {
        ChartKind::Bar => "notebook.visual.chart.kinds.bar",
        ChartKind::Line => "notebook.visual.chart.kinds.line",
        ChartKind::Area => "notebook.visual.chart.kinds.area",
        ChartKind::Scatter => "notebook.visual.chart.kinds.scatter",
        ChartKind::Bubble => "notebook.visual.chart.kinds.bubble",
        ChartKind::Histogram => "notebook.visual.chart.kinds.histogram",
        ChartKind::Pie => "notebook.visual.chart.kinds.pie",
        ChartKind::Donut => "notebook.visual.chart.kinds.donut",
        ChartKind::Heatmap => "notebook.visual.chart.kinds.heatmap",
        ChartKind::CalendarHeatmap => "notebook.visual.chart.kinds.calendar_heatmap",
        ChartKind::Waterfall => "notebook.visual.chart.kinds.waterfall",
        ChartKind::Funnel => "notebook.visual.chart.kinds.funnel",
        ChartKind::Treemap => "notebook.visual.chart.kinds.treemap",
        ChartKind::BoxPlot => "notebook.visual.chart.kinds.box",
        ChartKind::CohortRetention => "notebook.visual.chart.kinds.cohort_retention",
        ChartKind::CorrelationMatrix => "notebook.visual.chart.kinds.correlation_matrix",
        ChartKind::Sparkline => "notebook.visual.chart.kinds.sparkline",
        ChartKind::Kpi => "notebook.visual.chart.kinds.kpi",
    }
}

fn labels() -> ReportLabels {
    ReportLabels::resolve(sqlly_i18n::try_text)
}

/// A chart's output: the rows it draws (after the followed filter).
pub(crate) fn evaluate(
    inputs: &VisualInputs,
) -> Result<(Vec<ResultColumn>, Vec<Vec<CellValue>>), DisplayMessage> {
    let (output, rows) = inputs.input_rows().ok_or_else(|| missing_input(inputs))?;
    let cell: ChartCellSpec = spec::read(&inputs.spec);
    let (_, table) = spec_chart::prepare(
        &cell,
        &output.columns,
        rows.as_ref().clone(),
        output.completeness,
        output.row_count,
    );
    let columns = table
        .columns
        .iter()
        .map(|c| ResultColumn {
            name: c.name.clone(),
            data_type: c.data_type.clone(),
            nullable: true,
        })
        .collect();
    Ok((columns, table.rows))
}

/// Why a visual cell has nothing to draw, kept as a key.
pub(crate) fn missing_input(inputs: &VisualInputs) -> DisplayMessage {
    match &inputs.input {
        Some(i) => crate::localization::deferred(
            "notebook.visual.notice.input_not_run",
            &[("cell_name", i.name.as_str())],
        ),
        None => crate::localization::deferred_text("notebook.visual.errors.no_input"),
    }
}

fn hash_of(parts: impl Hash) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    h.finish()
}

fn number_text(n: f64) -> String {
    sqlly_services::report::format::float_literal(n)
}

impl VisualCellView {
    fn chart_spec(&self) -> ChartCellSpec {
        spec::read(&self.inputs.spec)
    }

    fn set_chart_spec(&mut self, cell: ChartCellSpec, cx: &mut Context<Self>) {
        self.emit_spec(spec::write(&cell), cx);
    }

    fn update_chart(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ChartSpec)) {
        let mut cell = self.chart_spec();
        change(&mut cell.chart);
        self.set_chart_spec(cell, cx);
    }

    pub(crate) fn apply_suggestion(
        &mut self,
        suggestion: &ChartSuggestion,
        cx: &mut Context<Self>,
    ) {
        let mut cell = self.chart_spec();
        let title = cell.chart.title.take();
        cell.chart = spec_chart::from_suggestion(suggestion);
        cell.chart.title = title;
        self.drop_fields("chart.");
        self.set_chart_spec(cell, cx);
    }

    pub(crate) fn chart_suggestions(&mut self) -> Vec<ChartSuggestion> {
        let Some((output, rows)) = self.inputs.input_rows() else {
            return Vec::new();
        };
        let key = hash_of((
            output.run_id.as_str(),
            rows.len(),
            self.inputs.filter.as_ref().map(|f| f.value.clone()),
        ));
        if let Some((k, list)) = &self.chart.suggestions
            && *k == key
        {
            return list.clone();
        }
        let list = spec_chart::suggestions(
            &output.columns,
            &rows,
            output.completeness,
            SUGGESTIONS_SHOWN,
        );
        self.chart.suggestions = Some((key, list.clone()));
        list
    }

    pub(crate) fn chart_actions(&mut self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let id = self.inputs.cell_id.clone();
        vec![
            small_button(
                format!("nb-chart-svg-{id}"),
                t("notebook.visual.chart.export_svg"),
            )
            .on_click(cx.listener(|this, _, _, cx| this.export_chart(false, cx)))
            .into_any_element(),
            small_button(
                format!("nb-chart-png-{id}"),
                t("notebook.visual.chart.export_png"),
            )
            .on_click(cx.listener(|this, _, _, cx| this.export_chart(true, cx)))
            .into_any_element(),
        ]
    }

    /// The chart as SVG markup at the export size, as every export draws it.
    pub(crate) fn chart_svg(&self, width: f32, height: f32) -> Option<String> {
        let (output, rows) = self.inputs.input_rows()?;
        let cell = self.chart_spec();
        let (chart, table) = spec_chart::prepare(
            &cell,
            &output.columns,
            rows.as_ref().clone(),
            output.completeness,
            output.row_count,
        );
        let labels = labels();
        let options = ChartOptions::new(report_theme(), &labels).size(width, height);
        Some(to_svg(
            &render_chart(&chart, &table, options).scene,
            &self.inputs.cell_id,
        ))
    }

    fn export_chart(&mut self, png: bool, cx: &mut Context<Self>) {
        let height = self.chart_spec().height();
        let Some(svg) = self.chart_svg(960.0, height.max(360.0)) else {
            cx.emit(VisualEvent::Status {
                message: t("notebook.visual.chart.export_nothing"),
                error: true,
            });
            return;
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let name = format!("{}.{}", self.inputs.name, if png { "png" } else { "svg" });
            let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some(&name));
            cx.spawn(async move |this, cx| {
                let Ok(Ok(Some(path))) = rx.await else {
                    return;
                };
                let bytes = if png {
                    sqlly_services::report::png::rasterize(&svg, 2.0)
                } else {
                    Ok(svg.into_bytes())
                };
                let result = bytes.and_then(|b| {
                    crate::fs_safe::write_atomic(&path, &b).map_err(|e| e.to_string())
                });
                this.update(cx, |_, cx| {
                    let (message, error) = match result {
                        Ok(()) => (
                            crate::localization::format(
                                "notebook.visual.chart.exported",
                                &[("file_path", path.display().to_string())],
                            ),
                            false,
                        ),
                        Err(reason) => (
                            crate::localization::format(
                                "notebook.visual.chart.export_failed",
                                &[("reason", reason)],
                            ),
                            true,
                        ),
                    };
                    cx.emit(VisualEvent::Status { message, error });
                })
                .ok();
            })
            .detach();
        }
        // The browser downloads the SVG; it has no rasterizer for PNG.
        #[cfg(target_arch = "wasm32")]
        {
            let (message, error) = if png {
                (
                    crate::notebook::browser::format_unavailable(
                        sqlly_services::report::ReportFormat::Png,
                    ),
                    true,
                )
            } else {
                let name = format!("{}.svg", self.inputs.name);
                match crate::browser_download::download_bytes(&name, svg.as_bytes()) {
                    Ok(()) => (
                        crate::localization::format(
                            "notebook.visual.chart.exported",
                            &[("file_path", name)],
                        ),
                        false,
                    ),
                    Err(reason) => (
                        crate::localization::format(
                            "notebook.visual.chart.export_failed",
                            &[("reason", reason)],
                        ),
                        true,
                    ),
                }
            };
            cx.emit(VisualEvent::Status { message, error });
        }
    }

    pub(crate) fn render_chart(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let cell = self.chart_spec();
        let mut root = div().flex().flex_col().w_full();
        if self.editing {
            root = root.child(self.render_chart_editor(&cell, window, cx));
        }
        if let Some(filter) = self.inputs.own_filter.clone() {
            let source = filter.source.clone();
            root = root.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .px(px(s(10.0)))
                    .py(px(s(2.0)))
                    .child(super::badge_chip(
                        crate::localization::format(
                            "notebook.visual.chart.selected",
                            &[
                                ("column_name", filter.column.as_str()),
                                ("value", filter.value.as_str()),
                            ],
                        ),
                        Theme::accent(),
                    ))
                    .child(
                        small_button(
                            format!("nb-chart-clear-{}", self.inputs.cell_id),
                            t("notebook.visual.filter.clear"),
                        )
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(VisualEvent::Filter {
                                source: source.clone(),
                                filter: None,
                            })
                        })),
                    ),
            );
        }
        let height = self
            .display
            .height
            .filter(|_| self.output_only)
            .unwrap_or_else(|| Theme::scaled(cell.height()));
        let Some((output, rows)) = self.inputs.input_rows() else {
            return root
                .child(
                    div()
                        .h(px(s(60.0)))
                        .px(px(s(12.0)))
                        .flex()
                        .items_center()
                        .child(dim(t("notebook.visual.chart.no_data"))),
                )
                .into_any_element();
        };
        if cell.chart.y.is_empty() && cell.chart.x.is_none() {
            root = root.child(self.render_suggestion_strip(cx));
        }
        let live = if cell.anomaly.is_some() {
            Err(StaticReason::Setting)
        } else {
            live_plan(&cell.chart, &output.columns, &rows)
        };
        let body = match live {
            Ok(plan) => self.render_live(plan, &output.run_id, &rows, height, window, cx),
            Err(_) => self.render_static(&cell, &output, &rows, height, window, cx),
        };
        root.child(body).into_any_element()
    }

    fn render_live(
        &mut self,
        plan: LivePlan,
        run_id: &str,
        rows: &Arc<Vec<Vec<CellValue>>>,
        height: f32,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = hash_of((
            run_id,
            rows.len(),
            format!("{:?}", plan.config),
            format!("{:?}", plan.columns),
            crate::notebook::visual::image::is_dark(),
        ));
        let columns: Vec<sqlly_datatable::Column> = plan
            .columns
            .iter()
            .map(|c| crate::grid_bridge::convert_column(None, c))
            .collect();
        if key != self.chart.live_key || self.chart.state.is_none() {
            let grid_rows: Vec<Vec<sqlly_datatable::CellValue>> = plan
                .rows
                .iter()
                .map(|row| row.iter().map(crate::grid_bridge::convert_cell).collect())
                .collect();
            let grid_rows = Arc::new(grid_rows);
            match &self.chart.state {
                Some(state) => state.update(cx, |s, cx| {
                    s.set_source(columns, grid_rows);
                    s.set_config(plan.config.clone());
                    cx.notify();
                }),
                None => {
                    let theme = crate::grid_bridge::build_grid_theme();
                    let focus = cx.focus_handle();
                    let font_size = Theme::font_size();
                    let config = plan.config.clone();
                    let state = cx.new(|_| {
                        ChartState::new(columns, grid_rows, config, theme, false, font_size, focus)
                    });
                    let canvas = cx.new(|_| ChartCanvas::new(state.clone()));
                    self.chart.state = Some(state);
                    self.chart.canvas = Some(canvas);
                }
            }
            self.chart.live_key = key;
        }
        self.chart.plan = Some(plan);
        let Some(canvas_view) = self.chart.canvas.clone() else {
            return div().into_any_element();
        };
        let bounds = self.chart.bounds.clone();
        div()
            .id(gpui::ElementId::Name(
                format!("nb-chart-live-{}", self.inputs.cell_id).into(),
            ))
            .relative()
            .h(px(height))
            .w_full()
            .child(canvas_view)
            .child(
                canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {})
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.chart_clicked(event.position, cx);
                }),
            )
            .into_any_element()
    }

    /// A click in the live chart: the category under it becomes this
    /// chart's cross-filter (a second click on it clears the filter).
    fn chart_clicked(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(plan), Some(state)) = (self.chart.plan.clone(), self.chart.state.clone()) else {
            return;
        };
        let Some(column) = plan.category_column.clone() else {
            return;
        };
        let bounds = self.chart.bounds.get();
        let x = f32::from(position.x - bounds.origin.x);
        let y = f32::from(position.y - bounds.origin.y) - LEGEND_STRIP;
        let width = f32::from(bounds.size.width);
        let height = f32::from(bounds.size.height) - LEGEND_STRIP;
        let picked = {
            let state = state.read(cx);
            let Some(data) = state.data() else {
                return;
            };
            let values: Vec<Option<f64>> = data
                .series
                .first()
                .map(|s| s.values.clone())
                .unwrap_or_default();
            let widest = values
                .iter()
                .flatten()
                .map(|v| number_text(v.abs().round()).len() + 1)
                .max()
                .unwrap_or(1) as f32;
            let gutter = Theme::font_size() * 0.85 * 0.6 * widest + 14.0;
            hit_category(
                plan.config.kind,
                data.categories.len(),
                &values,
                PlotClick {
                    width,
                    height,
                    x,
                    y,
                    gutter,
                },
            )
            .and_then(|i| data.categories.get(i).cloned())
        };
        let Some(value) = picked else {
            return;
        };
        let cell = self.chart_spec();
        let filter = CrossFilter {
            source: self.inputs.cell_id.clone(),
            column,
            value,
            grain: cell.chart.date_grain,
        };
        let filter = (self.inputs.own_filter.as_ref() != Some(&filter)).then_some(filter);
        cx.emit(VisualEvent::Filter {
            source: self.inputs.cell_id.clone(),
            filter,
        });
    }

    /// A click in the SVG-drawn chart: the category of the mark under it
    /// (hit-tested on the drawn scene) becomes this chart's cross-filter; a
    /// second click on it clears the filter.
    pub(crate) fn static_chart_clicked(
        &mut self,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some((_, scene)) = self.chart.scene.clone() else {
            return;
        };
        let bounds = self.chart.bounds.get();
        let Some((x, y)) = scene_point(
            &scene,
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            f32::from(position.x - bounds.origin.x),
            f32::from(position.y - bounds.origin.y),
        ) else {
            return;
        };
        let Some(value) = scene.hit_test(x, y).map(str::to_string) else {
            return;
        };
        let cell = self.chart_spec();
        let Some(column) = cell.chart.x.as_ref().map(|e| e.column.clone()) else {
            return;
        };
        let filter = CrossFilter {
            source: self.inputs.cell_id.clone(),
            column,
            value,
            grain: cell.chart.date_grain,
        };
        let filter = (self.inputs.own_filter.as_ref() != Some(&filter)).then_some(filter);
        cx.emit(VisualEvent::Filter {
            source: self.inputs.cell_id.clone(),
            filter,
        });
    }

    fn render_static(
        &mut self,
        cell: &ChartCellSpec,
        output: &Arc<sqlly_services::notebook::CellOutput>,
        rows: &Arc<Vec<Vec<CellValue>>>,
        height: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.chart.plan = None;
        let measured = f32::from(self.chart.bounds.get().size.width);
        let width = if measured > 100.0 { measured } else { 760.0 };
        let bucket = (width / 40.0).round() * 40.0;
        let scale = window.scale_factor();
        let spec_text = self.inputs.spec.to_string();
        let id = self.inputs.cell_id.clone();
        let key = (
            output.run_id.clone(),
            rows.len(),
            spec_text,
            bucket as u32,
            height as u32,
            format!("{:?}", self.inputs.filter),
        );
        let scene_key = hash_of(&key);
        if self.chart.scene.as_ref().map(|(k, _)| *k) != Some(scene_key) {
            let (chart, table) = spec_chart::prepare(
                cell,
                &output.columns,
                rows.as_ref().clone(),
                output.completeness,
                output.row_count,
            );
            let labels = labels();
            let options = ChartOptions::new(report_theme(), &labels).size(bucket, height);
            let drawn = render_chart(&chart, &table, options);
            self.chart.scene = Some((scene_key, Arc::new(drawn.scene)));
        }
        let scene = self
            .chart
            .scene
            .as_ref()
            .map(|(_, scene)| scene.clone())
            .expect("drawn above");
        let image = self
            .chart
            .image
            .get(key, scale, move || to_svg(&scene, &id));
        let bounds = self.chart.bounds.clone();
        let mut frame = div()
            .id(gpui::ElementId::Name(
                format!("nb-chart-svg-{}", self.inputs.cell_id).into(),
            ))
            .relative()
            .h(px(height))
            .w_full()
            .child(
                canvas(move |b, _, _| bounds.set(b), |_, _, _, _| {})
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _, cx| {
                    this.static_chart_clicked(event.position, cx);
                }),
            );
        frame = match image {
            Some(image) => frame.child(img(image).size_full().object_fit(gpui::ObjectFit::Contain)),
            None => frame.child(dim(self.chart.image.error.clone().unwrap_or_default())),
        };
        let invalid = {
            let (chart, table) = spec_chart::prepare(
                &self.chart_spec(),
                &output.columns,
                Vec::new(),
                output.completeness,
                output.row_count,
            );
            chart.validate(&table.columns).err()
        };
        let mut out = div().flex().flex_col().w_full();
        if let Some(errors) = invalid {
            for error in errors.iter().take(3) {
                out = out.child(
                    div()
                        .px(px(Theme::scaled(10.0)))
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::warning())
                        .child(crate::localization::message(error.message())),
                );
            }
        }
        out.child(frame).into_any_element()
    }

    fn render_suggestion_strip(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let suggestions = self.chart_suggestions();
        let mut row = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(6.0)))
            .px(px(s(10.0)))
            .py(px(s(4.0)))
            .child(dim(t("notebook.visual.chart.suggested")));
        for (i, suggestion) in suggestions.into_iter().enumerate() {
            let label = suggestion_label(&suggestion);
            row = row.child(
                small_button(
                    format!("nb-chart-suggest-{}-{i}", self.inputs.cell_id),
                    label,
                )
                .outline()
                .on_click(
                    cx.listener(move |this, _, _, cx| this.apply_suggestion(&suggestion, cx)),
                ),
            );
        }
        row.into_any_element()
    }

    fn render_chart_editor(
        &mut self,
        cell: &ChartCellSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = self.inputs.cell_id.clone();
        let this = cx.entity();
        let columns = self.inputs.input_columns();
        let chart = &cell.chart;
        let mut panel = editor_panel();

        panel = panel.child(self.render_suggestion_strip(cx));

        let kinds: Vec<(String, ChartKind, bool)> = ChartKind::ALL
            .iter()
            .filter(|k| **k != ChartKind::Kpi)
            .map(|k| (kind_label(*k), *k, *k == chart.kind))
            .collect();
        panel = panel.child(editor_row(
            t("notebook.visual.chart.editor.kind"),
            choice_button(
                format!("nb-chart-kind-{id}"),
                kind_label(chart.kind),
                kinds,
                this.clone(),
                |v, kind, cx| v.update_chart(cx, |c| c.kind = kind),
            ),
        ));

        let none = t("notebook.visual.chart.editor.none");
        let encoding = |label_key: &str,
                        current: Option<&str>,
                        tag: &'static str,
                        set: fn(&mut ChartSpec, Option<String>)| {
            editor_row(
                t(label_key),
                choice_button(
                    format!("nb-chart-{tag}-{id}"),
                    current.map(str::to_string).unwrap_or_else(|| none.clone()),
                    column_options(&columns, current, Some(none.clone())),
                    this.clone(),
                    move |v, column, cx| v.update_chart(cx, |c| set(c, column)),
                ),
            )
        };
        panel = panel
            .child(encoding(
                "notebook.visual.chart.editor.x",
                chart.x.as_ref().map(|e| e.column.as_str()),
                "x",
                |c, col| c.x = col.map(Encoding::new),
            ))
            .child(encoding(
                "notebook.visual.chart.editor.y",
                chart.y.first().map(|e| e.column.as_str()),
                "y",
                |c, col| {
                    let rest: Vec<Encoding> = c.y.iter().skip(1).cloned().collect();
                    c.y = col.map(Encoding::new).into_iter().chain(rest).collect();
                },
            ))
            .child(encoding(
                "notebook.visual.chart.editor.y2",
                chart.y.get(1).map(|e| e.column.as_str()),
                "y2",
                |c, col| {
                    c.y.truncate(1);
                    if let Some(col) = col {
                        c.y.push(Encoding::new(col));
                    }
                },
            ))
            .child(encoding(
                "notebook.visual.chart.editor.series",
                chart.series.as_ref().map(|e| e.column.as_str()),
                "series",
                |c, col| c.series = col.map(Encoding::new),
            ))
            .child(encoding(
                "notebook.visual.chart.editor.size",
                chart.size.as_ref().map(|e| e.column.as_str()),
                "size",
                |c, col| c.size = col.map(Encoding::new),
            ))
            .child(encoding(
                "notebook.visual.chart.editor.color",
                chart.color.as_ref().map(|e| e.column.as_str()),
                "color",
                |c, col| c.color = col.map(Encoding::new),
            ));

        let aggregates = [
            (Aggregate::None, "notebook.visual.chart.aggregates.none"),
            (Aggregate::Sum, "notebook.visual.chart.aggregates.sum"),
            (Aggregate::Avg, "notebook.visual.chart.aggregates.avg"),
            (Aggregate::Count, "notebook.visual.chart.aggregates.count"),
            (
                Aggregate::CountDistinct,
                "notebook.visual.chart.aggregates.count_distinct",
            ),
            (Aggregate::Min, "notebook.visual.chart.aggregates.min"),
            (Aggregate::Max, "notebook.visual.chart.aggregates.max"),
            (Aggregate::Median, "notebook.visual.chart.aggregates.median"),
        ];
        let agg_label = |a: Aggregate| {
            aggregates
                .iter()
                .find(|(x, _)| *x == a)
                .map(|(_, n)| t(n))
                .unwrap_or_default()
        };
        panel = panel.child(editor_row(
            t("notebook.visual.chart.editor.aggregate"),
            choice_button(
                format!("nb-chart-agg-{id}"),
                agg_label(chart.aggregate),
                aggregates
                    .iter()
                    .map(|(a, _)| (agg_label(*a), *a, *a == chart.aggregate))
                    .collect(),
                this.clone(),
                |v, a, cx| v.update_chart(cx, |c| c.aggregate = a),
            ),
        ));

        let sorts = [
            (
                SortBy::Natural,
                false,
                "notebook.visual.chart.sorts.natural",
            ),
            (
                SortBy::Category,
                false,
                "notebook.visual.chart.sorts.category_asc",
            ),
            (
                SortBy::Category,
                true,
                "notebook.visual.chart.sorts.category_desc",
            ),
            (
                SortBy::Value,
                true,
                "notebook.visual.chart.sorts.value_desc",
            ),
            (
                SortBy::Value,
                false,
                "notebook.visual.chart.sorts.value_asc",
            ),
        ];
        let sort_label = |by: SortBy, desc: bool| {
            sorts
                .iter()
                .find(|(b, d, _)| *b == by && (*d == desc || by == SortBy::Natural))
                .map(|(_, _, n)| t(n))
                .unwrap_or_default()
        };
        panel = panel.child(editor_row(
            t("notebook.visual.chart.editor.sort"),
            choice_button(
                format!("nb-chart-sort-{id}"),
                sort_label(chart.sort.by, chart.sort.descending),
                sorts
                    .iter()
                    .map(|(b, d, _)| {
                        (
                            sort_label(*b, *d),
                            (*b, *d),
                            *b == chart.sort.by && *d == chart.sort.descending,
                        )
                    })
                    .collect(),
                this.clone(),
                |v, (by, descending), cx| v.update_chart(cx, |c| c.sort = Sort { by, descending }),
            ),
        ));

        let grains = [
            (None, "notebook.visual.chart.grains.none"),
            (Some(DateGrain::Hour), "notebook.visual.chart.grains.hour"),
            (Some(DateGrain::Day), "notebook.visual.chart.grains.day"),
            (Some(DateGrain::Week), "notebook.visual.chart.grains.week"),
            (Some(DateGrain::Month), "notebook.visual.chart.grains.month"),
            (
                Some(DateGrain::Quarter),
                "notebook.visual.chart.grains.quarter",
            ),
            (Some(DateGrain::Year), "notebook.visual.chart.grains.year"),
        ];
        let grain_label = |g: Option<DateGrain>| {
            grains
                .iter()
                .find(|(x, _)| *x == g)
                .map(|(_, n)| t(n))
                .unwrap_or_default()
        };
        panel = panel.child(editor_row(
            t("notebook.visual.chart.editor.date_grain"),
            choice_button(
                format!("nb-chart-grain-{id}"),
                grain_label(chart.date_grain),
                grains
                    .iter()
                    .map(|(g, _)| (grain_label(*g), *g, *g == chart.date_grain))
                    .collect(),
                this.clone(),
                |v, g, cx| v.update_chart(cx, |c| c.date_grain = g),
            ),
        ));

        let nulls = [
            (NullTreatment::Gap, "notebook.visual.chart.nulls.gap"),
            (NullTreatment::Zero, "notebook.visual.chart.nulls.zero"),
            (NullTreatment::Skip, "notebook.visual.chart.nulls.skip"),
        ];
        let null_label = |n: NullTreatment| {
            nulls
                .iter()
                .find(|(x, _)| *x == n)
                .map(|(_, k)| t(k))
                .unwrap_or_default()
        };
        let modes = [
            (BarMode::Grouped, "notebook.visual.chart.bar_modes.grouped"),
            (BarMode::Stacked, "notebook.visual.chart.bar_modes.stacked"),
            (
                BarMode::Normalized,
                "notebook.visual.chart.bar_modes.normalized",
            ),
        ];
        let mode_label = |m: BarMode| {
            modes
                .iter()
                .find(|(x, _)| *x == m)
                .map(|(_, k)| t(k))
                .unwrap_or_default()
        };
        let horizontal = chart.orientation == Orientation::Horizontal;
        panel = panel.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(Theme::scaled(10.0)))
                .child(editor_row(
                    t("notebook.visual.chart.editor.nulls"),
                    choice_button(
                        format!("nb-chart-nulls-{id}"),
                        null_label(chart.nulls),
                        nulls
                            .iter()
                            .map(|(n, _)| (null_label(*n), *n, *n == chart.nulls))
                            .collect(),
                        this.clone(),
                        |v, n, cx| v.update_chart(cx, |c| c.nulls = n),
                    ),
                ))
                .child(editor_row(
                    t("notebook.visual.chart.editor.bar_mode"),
                    choice_button(
                        format!("nb-chart-mode-{id}"),
                        mode_label(chart.bar_mode),
                        modes
                            .iter()
                            .map(|(m, _)| (mode_label(*m), *m, *m == chart.bar_mode))
                            .collect(),
                        this.clone(),
                        |v, m, cx| v.update_chart(cx, |c| c.bar_mode = m),
                    ),
                ))
                .child(
                    small_button(
                        format!("nb-chart-horizontal-{id}"),
                        t("notebook.visual.chart.editor.horizontal"),
                    )
                    .when(horizontal, |b| b.primary())
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.update_chart(cx, |c| {
                            c.orientation = if horizontal {
                                Orientation::Vertical
                            } else {
                                Orientation::Horizontal
                            }
                        })
                    })),
                )
                .child({
                    let on = chart.show_values;
                    small_button(
                        format!("nb-chart-values-{id}"),
                        t("notebook.visual.chart.editor.show_values"),
                    )
                    .when(on, |b| b.primary())
                    .on_click(
                        cx.listener(move |v, _, _, cx| v.update_chart(cx, |c| c.show_values = !on)),
                    )
                })
                .child({
                    let on = cell.anomaly.is_some();
                    small_button(
                        format!("nb-chart-anomaly-{id}"),
                        t("notebook.visual.chart.editor.anomaly_band"),
                    )
                    .tooltip(t("notebook.visual.chart.editor.anomaly_band_tooltip"))
                    .when(on, |b| b.primary())
                    .on_click(cx.listener(move |v, _, _, cx| {
                        let mut cell = v.chart_spec();
                        cell.anomaly = if on {
                            None
                        } else {
                            Some(AnomalyConfig::default())
                        };
                        v.set_chart_spec(cell, cx);
                    }))
                }),
        );

        let title = self.field(
            &format!("chart.title.{id}"),
            chart.title.as_deref().unwrap_or_default(),
            "notebook.visual.chart.editor.title_placeholder",
            window,
            cx,
            |v, text, cx| {
                let text = text.trim().to_string();
                v.update_chart(cx, |c| c.title = (!text.is_empty()).then_some(text));
            },
        );
        let top_n = self.field(
            &format!("chart.top_n.{id}"),
            &chart.top_n.map(|t| t.n.to_string()).unwrap_or_default(),
            "notebook.visual.chart.editor.top_n_placeholder",
            window,
            cx,
            |v, text, cx| {
                let n = text.trim().parse::<usize>().ok().filter(|n| *n > 0);
                v.update_chart(cx, |c| {
                    let other = c.top_n.is_some_and(|t| t.other);
                    c.top_n = n.map(|n| TopN { n, other });
                });
            },
        );
        panel = panel.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(Theme::scaled(10.0)))
                .child(editor_row(
                    t("notebook.visual.chart.editor.title"),
                    text_field(&title, 220.0),
                ))
                .child(editor_row(
                    t("notebook.visual.chart.editor.top_n"),
                    text_field(&top_n, 70.0),
                ))
                .child({
                    let other = chart.top_n.is_some_and(|t| t.other);
                    small_button(
                        format!("nb-chart-other-{id}"),
                        t("notebook.visual.chart.editor.top_n_other"),
                    )
                    .when(other, |b| b.primary())
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.update_chart(cx, |c| {
                            if let Some(top) = c.top_n.as_mut() {
                                top.other = !other;
                            }
                        })
                    }))
                }),
        );

        let formats = [
            (NumberStyle::Plain, "notebook.visual.chart.formats.plain"),
            (
                NumberStyle::Percent,
                "notebook.visual.chart.formats.percent",
            ),
            (
                NumberStyle::Currency,
                "notebook.visual.chart.formats.currency",
            ),
            (
                NumberStyle::Compact,
                "notebook.visual.chart.formats.compact",
            ),
        ];
        let format_label = |n: NumberStyle| {
            formats
                .iter()
                .find(|(x, _)| *x == n)
                .map(|(_, k)| t(k))
                .unwrap_or_default()
        };
        let unit = self.field(
            &format!("chart.unit.{id}"),
            chart.y_axis.format.unit.as_deref().unwrap_or_default(),
            "notebook.visual.chart.editor.unit_placeholder",
            window,
            cx,
            |v, text, cx| {
                let text = text.trim().to_string();
                v.update_chart(cx, |c| {
                    c.y_axis.format.unit = (!text.is_empty()).then_some(text)
                });
            },
        );
        let currency = self.field(
            &format!("chart.currency.{id}"),
            chart.y_axis.format.currency.as_deref().unwrap_or_default(),
            "notebook.visual.chart.editor.currency_placeholder",
            window,
            cx,
            |v, text, cx| {
                let text = text.trim().to_uppercase();
                v.update_chart(cx, |c| {
                    c.y_axis.format.currency = (!text.is_empty()).then_some(text)
                });
            },
        );
        panel = panel.child(
            div()
                .flex()
                .flex_row()
                .flex_wrap()
                .gap(px(Theme::scaled(10.0)))
                .child(editor_row(
                    t("notebook.visual.chart.editor.value_format"),
                    choice_button(
                        format!("nb-chart-format-{id}"),
                        format_label(chart.y_axis.format.style),
                        formats
                            .iter()
                            .map(|(f, _)| (format_label(*f), *f, *f == chart.y_axis.format.style))
                            .collect(),
                        this.clone(),
                        |v, style, cx| {
                            v.update_chart(cx, |c| {
                                c.y_axis.format = ValueFormat {
                                    style,
                                    ..c.y_axis.format.clone()
                                }
                            })
                        },
                    ),
                ))
                .child(editor_row(
                    t("notebook.visual.chart.editor.unit"),
                    text_field(&unit, 70.0),
                ))
                .child(editor_row(
                    t("notebook.visual.chart.editor.currency"),
                    text_field(&currency, 70.0),
                )),
        );

        let lines = self.field(
            &format!("chart.lines.{id}"),
            &reference_lines_text(&chart.reference_lines),
            "notebook.visual.chart.editor.reference_lines_placeholder",
            window,
            cx,
            |v, text, cx| {
                let lines = parse_reference_lines(text);
                v.update_chart(cx, |c| c.reference_lines = lines);
            },
        );
        let notes = self.field(
            &format!("chart.notes.{id}"),
            &annotations_text(&chart.annotations),
            "notebook.visual.chart.editor.annotations_placeholder",
            window,
            cx,
            |v, text, cx| {
                let notes = parse_annotations(text);
                v.update_chart(cx, |c| c.annotations = notes);
            },
        );
        panel = panel
            .child(editor_row(
                t("notebook.visual.chart.editor.reference_lines"),
                text_field(&lines, 320.0),
            ))
            .child(editor_row(
                t("notebook.visual.chart.editor.annotations"),
                text_field(&notes, 320.0),
            ));
        panel.into_any_element()
    }
}

pub(crate) fn suggestion_label(suggestion: &ChartSuggestion) -> String {
    let kind = spec_chart::from_suggestion(suggestion).kind;
    let e = &suggestion.encodings;
    let measure = e
        .value
        .clone()
        .or_else(|| e.y.first().cloned())
        .unwrap_or_default();
    let by =
        e.x.clone()
            .or_else(|| e.category.clone())
            .unwrap_or_default();
    if by.is_empty() || measure.is_empty() {
        return kind_label(kind);
    }
    crate::localization::format(
        "notebook.visual.chart.suggestion",
        &[
            ("chart_kind", kind_label(kind).as_str()),
            ("measure", measure.as_str()),
            ("category", by.as_str()),
        ],
    )
}

/// Reference lines (and thresholds, when given a status) written as
/// `value label; value label!`, `!` marking a critical threshold and `?` a
/// warning.
pub(crate) fn parse_reference_lines(text: &str) -> Vec<ReferenceLine> {
    use sqlly_services::report::doc::Status;
    text.split(';')
        .filter_map(|part| {
            let part = part.trim();
            let (number, rest) = part.split_once(' ').unwrap_or((part, ""));
            let value = number
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite())?;
            let mut label = rest.trim().to_string();
            let status = if label.ends_with('!') {
                label.pop();
                Some(Status::Critical)
            } else if label.ends_with('?') {
                label.pop();
                Some(Status::Warning)
            } else {
                None
            };
            let label = label.trim().to_string();
            Some(ReferenceLine {
                value,
                label: (!label.is_empty()).then_some(label),
                status,
            })
        })
        .collect()
}

fn reference_lines_text(lines: &[ReferenceLine]) -> String {
    use sqlly_services::report::doc::Status;
    lines
        .iter()
        .map(|l| {
            let mark = match l.status {
                Some(Status::Critical) => "!",
                Some(Status::Warning) => "?",
                _ => "",
            };
            match &l.label {
                Some(label) => format!("{} {label}{mark}", number_text(l.value)),
                None => {
                    format!(
                        "{}{}",
                        number_text(l.value),
                        if mark.is_empty() { "" } else { " " }
                    ) + mark
                }
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Annotations written as `category: note; category: note`.
pub(crate) fn parse_annotations(text: &str) -> Vec<Annotation> {
    text.split(';')
        .filter_map(|part| {
            let (x, note) = part.split_once(':')?;
            let (x, note) = (x.trim(), note.trim());
            (!x.is_empty() && !note.is_empty()).then(|| Annotation {
                x: x.to_string(),
                text: note.to_string(),
            })
        })
        .collect()
}

fn annotations_text(notes: &[Annotation]) -> String {
    notes
        .iter()
        .map(|a| format!("{}: {}", a.x, a.text))
        .collect::<Vec<_>>()
        .join("; ")
}
