//! Deterministic chart rendering for every [`ChartKind`]: rows in, a
//! [`Scene`] out, written as SVG (no script, no `foreignObject`, every
//! string escaped) or painted into the PDF. The same input always yields
//! byte-identical markup.

pub mod data;
pub mod scale;
pub mod scene;

mod cartesian;
mod matrix;
mod parts;
mod radial;

use crate::report::chart::{ChartKind, ChartSpec};
use crate::report::doc::{KpiBlock, TableBlock};
use crate::report::labels::ReportLabels;
use crate::report::theme::ReportTheme;

pub use data::ChartError;
pub use radial::{delta_color as kpi_delta_color, delta_text as kpi_delta_text, kpi_value_text};
pub use scene::{Hit, HitShape, Scene, to_svg};

#[derive(Debug, Clone, Copy)]
pub struct ChartOptions<'a> {
    pub width: f32,
    pub height: f32,
    pub theme: &'a ReportTheme,
    pub labels: &'a ReportLabels,
    /// The incomplete-data line drawn under the title; [`render_chart`]
    /// sets it from the table, so every format carries it.
    pub badge: Option<&'a str>,
}

impl<'a> ChartOptions<'a> {
    pub fn new(theme: &'a ReportTheme, labels: &'a ReportLabels) -> Self {
        Self {
            width: 640.0,
            height: 360.0,
            theme,
            labels,
            badge: None,
        }
    }

    pub fn size(mut self, width: f32, height: f32) -> Self {
        self.width = width.clamp(80.0, 4000.0);
        self.height = height.clamp(40.0, 4000.0);
        self
    }
}

/// A drawn chart; `error` says why it fell back to a placeholder.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedChart {
    pub scene: Scene,
    pub error: Option<ChartError>,
    /// Categories (or groups, or heatmap rows and columns) folded into
    /// "Other" because there were too many to draw.
    pub capped_categories: usize,
}

/// The incomplete-data badge a chart over `table` carries, `None` when the
/// chart saw every row.
pub fn completeness_badge(table: &TableBlock, labels: &ReportLabels) -> Option<String> {
    use crate::report::doc::Completeness;
    match table.completeness {
        Completeness::Complete => None,
        Completeness::Spilled { total } if table.rows.len() as u64 >= total => None,
        Completeness::Spilled { total } => Some(labels.shown(table.rows.len(), total)),
        other => labels.completeness(other),
    }
}

pub fn render_chart(spec: &ChartSpec, table: &TableBlock, options: ChartOptions) -> RenderedChart {
    let ctx = data::Ctx::new(spec, table, options.labels);
    let badge = completeness_badge(table, options.labels);
    let options = ChartOptions {
        badge: badge.as_deref(),
        ..options
    };
    if let Err(error) = ctx.check() {
        return RenderedChart {
            scene: parts::placeholder(spec, options, &error),
            error: Some(error),
            capped_categories: 0,
        };
    }
    let result = match spec.kind {
        ChartKind::Bar => cartesian::bar(&ctx, options),
        ChartKind::Line | ChartKind::Area => cartesian::line(&ctx, options),
        ChartKind::Scatter | ChartKind::Bubble => cartesian::scatter(&ctx, options),
        ChartKind::Histogram => cartesian::histogram(&ctx, options),
        ChartKind::BoxPlot => cartesian::box_plot(&ctx, options),
        ChartKind::Waterfall => cartesian::waterfall(&ctx, options),
        ChartKind::Pie | ChartKind::Donut => radial::pie(&ctx, options),
        ChartKind::Funnel => radial::funnel(&ctx, options),
        ChartKind::Treemap => radial::treemap(&ctx, options),
        ChartKind::Heatmap => matrix::heatmap(&ctx, options),
        ChartKind::CohortRetention => matrix::cohort(&ctx, options),
        ChartKind::CorrelationMatrix => matrix::correlation(&ctx, options),
        ChartKind::CalendarHeatmap => matrix::calendar(&ctx, options),
        ChartKind::Sparkline => radial::sparkline_chart(&ctx, options),
        ChartKind::Kpi => radial::kpi_chart(&ctx, options),
    };
    match result {
        Ok(mut scene) => {
            scene.title = parts::accessible_title(spec);
            scene.description = parts::accessible_description(spec, table);
            if let Some(badge) = &badge {
                scene.description = format!("{} {badge}", scene.description).trim().to_string();
            }
            RenderedChart {
                scene,
                error: None,
                capped_categories: ctx.capped.get(),
            }
        }
        Err(error) => RenderedChart {
            scene: parts::placeholder(spec, options, &error),
            error: Some(error),
            capped_categories: 0,
        },
    }
}

/// [`render_chart`] written as an SVG document.
pub fn chart_svg(spec: &ChartSpec, table: &TableBlock, options: ChartOptions, id: &str) -> String {
    to_svg(&render_chart(spec, table, options).scene, id)
}

/// A small trend line with no axes, for KPI cards and table cells.
pub fn sparkline(values: &[f64], width: f32, height: f32, theme: &ReportTheme) -> Scene {
    radial::sparkline_scene(values, width, height, theme.accent)
}

/// [`sparkline`] over data that is not complete: it carries the small
/// incomplete-data mark instead of the badge's text.
pub fn partial_sparkline(values: &[f64], width: f32, height: f32, theme: &ReportTheme) -> Scene {
    let mut scene = sparkline(values, width, height, theme);
    radial::mark_partial(&mut scene, width, theme.warning);
    scene
}

/// A KPI card (label, value, change, status, sparkline) as a scene, for the
/// formats that draw rather than lay out text.
pub fn kpi_card(kpi: &KpiBlock, options: ChartOptions) -> Scene {
    radial::kpi_card(kpi, options)
}
