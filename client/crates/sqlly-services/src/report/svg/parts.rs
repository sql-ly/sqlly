//! Pieces every chart shares: title block, legend, axes, gridlines,
//! reference lines and the placeholder for charts that cannot be drawn.

use super::ChartOptions;
use super::data::{ChartError, Ctx};
use super::scale::{Band, Linear, fit_text, nice_ticks, text_width};
use super::scene::{Anchor, Fill, Scene, Stroke, TextItem};
use crate::report::chart::{ChartSpec, PaletteKind};
use crate::report::doc::TableBlock;
use crate::report::format::{NumberStyle, ValueFormat, format_number};
use crate::report::labels::ReportLabels;
use crate::report::theme::{ReportTheme, Rgb};

pub const PAD: f32 = 12.0;
pub const TITLE_SIZE: f32 = 14.0;
pub const LABEL_SIZE: f32 = 10.5;
pub const TICK_SIZE: f32 = 10.0;

#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
}

/// Starts a chart scene: background, title and subtitle. Returns the area
/// left for the chart body.
pub fn begin(spec: &ChartSpec, options: ChartOptions) -> (Scene, Rect) {
    let theme = options.theme;
    let mut scene = Scene::new(options.width, options.height);
    scene.background = Some(theme.background);
    let mut top = PAD;
    if let Some(title) = spec.title.as_deref().filter(|t| !t.is_empty()) {
        top += TITLE_SIZE;
        scene.push_text(TextItem {
            x: PAD,
            y: top,
            text: fit_text(title, TITLE_SIZE, options.width - 2.0 * PAD),
            size: TITLE_SIZE,
            color: theme.text,
            anchor: Anchor::Start,
            bold: true,
            rotate: 0.0,
        });
        top += 6.0;
    }
    if let Some(subtitle) = spec.subtitle.as_deref().filter(|t| !t.is_empty()) {
        top += LABEL_SIZE;
        scene.text(
            PAD,
            top,
            fit_text(subtitle, LABEL_SIZE, options.width - 2.0 * PAD),
            LABEL_SIZE,
            theme.text_dim,
            Anchor::Start,
        );
        top += 6.0;
    }
    if let Some(badge) = options.badge.filter(|b| !b.is_empty()) {
        top += LABEL_SIZE;
        scene.push_text(TextItem {
            x: PAD,
            y: top,
            text: fit_text(badge, LABEL_SIZE, options.width - 2.0 * PAD),
            size: LABEL_SIZE,
            color: theme.warning,
            anchor: Anchor::Start,
            bold: true,
            rotate: 0.0,
        });
        top += 6.0;
    }
    let area = Rect {
        x: PAD,
        y: top + 4.0,
        w: options.width - 2.0 * PAD,
        h: (options.height - top - 4.0 - PAD).max(10.0),
    };
    (scene, area)
}

/// Series colors in legend order.
pub fn colors(spec: &ChartSpec, theme: &ReportTheme, count: usize) -> Vec<Rgb> {
    (0..count)
        .map(|i| match spec.palette {
            PaletteKind::Categorical => theme.series_color(i),
            PaletteKind::Sequential => theme.sequential_color(i, count),
        })
        .collect()
}

const SWATCH: f32 = 9.0;

fn legend_rows(items: &[(String, Rgb)], width: f32) -> Vec<Vec<(String, Rgb, f32)>> {
    let mut rows: Vec<Vec<(String, Rgb, f32)>> = vec![Vec::new()];
    let mut x = 0.0;
    for (name, color) in items {
        let label = fit_text(name, LABEL_SIZE, 180.0);
        let w = SWATCH + 5.0 + text_width(&label, LABEL_SIZE) + 14.0;
        if x + w > width && !rows.last().is_some_and(Vec::is_empty) {
            rows.push(Vec::new());
            x = 0.0;
        }
        rows.last_mut()
            .unwrap_or(&mut Vec::new())
            .push((label, *color, x));
        x += w;
        if rows.len() > 4 {
            break;
        }
    }
    rows.truncate(4);
    rows
}

/// Draws a legend along the bottom of `area` and returns the area above it.
pub fn legend(scene: &mut Scene, area: Rect, items: &[(String, Rgb)], theme: &ReportTheme) -> Rect {
    if items.is_empty() {
        return area;
    }
    let rows = legend_rows(items, area.w);
    let line = LABEL_SIZE + 6.0;
    let height = rows.len() as f32 * line;
    let top = area.bottom() - height;
    for (r, row) in rows.iter().enumerate() {
        let y = top + r as f32 * line;
        for (label, color, x) in row {
            scene.rect(area.x + x, y + 2.0, SWATCH, SWATCH, Fill::solid(*color));
            scene.text(
                area.x + x + SWATCH + 5.0,
                y + 2.0 + SWATCH - 0.5,
                label.clone(),
                LABEL_SIZE,
                theme.text_dim,
                Anchor::Start,
            );
        }
    }
    Rect {
        h: (area.h - height - 6.0).max(10.0),
        ..area
    }
}

/// The format axis labels use: the axis's own, else the measure column's,
/// else compact numbers.
pub fn axis_format(ctx: &Ctx, explicit: &ValueFormat, measure_column: Option<&str>) -> ValueFormat {
    if !explicit.is_default() {
        return explicit.clone();
    }
    let column = measure_column
        .and_then(|c| ctx.table.column_index(c))
        .map(|i| &ctx.table.columns[i]);
    match column {
        Some(c) if !c.format.is_default() => {
            let mut f = c.format.clone();
            f.decimals = None;
            if f.style == NumberStyle::Plain {
                f.style = NumberStyle::Compact;
            }
            f
        }
        _ => ValueFormat::compact(),
    }
}

pub fn tick_label(v: f64, format: &ValueFormat, step: f64) -> String {
    let mut f = format.clone();
    if f.decimals.is_none() && f.style != NumberStyle::Compact {
        let digits = if step >= 1.0 || step == 0.0 {
            0
        } else {
            (-step.log10().floor()) as u8
        };
        f.decimals = Some(if f.style == NumberStyle::Percent {
            digits.saturating_sub(2)
        } else {
            digits
        });
    }
    format_number(v, &f)
}

pub struct ValueAxis {
    pub scale: Linear,
    pub ticks: Vec<f64>,
    pub labels: Vec<String>,
    pub width: f32,
}

/// A value axis over `[min, max]` (widened to nice steps, `include_zero`
/// for bars and areas) mapped onto `range`.
pub fn value_axis(
    min: f64,
    max: f64,
    include_zero: bool,
    spec_min: Option<f64>,
    spec_max: Option<f64>,
    format: &ValueFormat,
    range: (f32, f32),
) -> ValueAxis {
    let (mut lo, mut hi) = (min, max);
    if include_zero {
        lo = lo.min(0.0);
        hi = hi.max(0.0);
    }
    if let Some(m) = spec_min {
        lo = m;
    }
    if let Some(m) = spec_max {
        hi = m;
    }
    let (d0, d1, mut ticks) = nice_ticks(lo, hi, 5);
    let (d0, d1) = (spec_min.unwrap_or(d0), spec_max.unwrap_or(d1));
    ticks.retain(|t| *t >= d0 - 1e-9 && *t <= d1 + 1e-9);
    let step = if ticks.len() > 1 {
        ticks[1] - ticks[0]
    } else {
        1.0
    };
    let labels: Vec<String> = ticks.iter().map(|t| tick_label(*t, format, step)).collect();
    let width = labels
        .iter()
        .map(|l| text_width(l, TICK_SIZE))
        .fold(0.0, f32::max);
    ValueAxis {
        scale: Linear::new((d0, d1), range),
        ticks,
        labels,
        width,
    }
}

/// Horizontal gridlines and left tick labels for a vertical value axis.
pub fn draw_value_axis_left(scene: &mut Scene, axis: &ValueAxis, plot: Rect, theme: &ReportTheme) {
    for (t, label) in axis.ticks.iter().zip(&axis.labels) {
        let y = axis.scale.map(*t);
        let stroke = if *t == 0.0 {
            Stroke::new(theme.border, 1.0)
        } else {
            Stroke::new(theme.grid, 1.0)
        };
        scene.line(plot.x, y, plot.right(), y, stroke);
        scene.text(
            plot.x - 6.0,
            y + TICK_SIZE * 0.35,
            label.clone(),
            TICK_SIZE,
            theme.text_dim,
            Anchor::End,
        );
    }
}

/// Vertical gridlines and bottom tick labels for a horizontal value axis.
pub fn draw_value_axis_bottom(
    scene: &mut Scene,
    axis: &ValueAxis,
    plot: Rect,
    theme: &ReportTheme,
) {
    for (t, label) in axis.ticks.iter().zip(&axis.labels) {
        let x = axis.scale.map(*t);
        let stroke = if *t == 0.0 {
            Stroke::new(theme.border, 1.0)
        } else {
            Stroke::new(theme.grid, 1.0)
        };
        scene.line(x, plot.y, x, plot.bottom(), stroke);
        scene.text(
            x,
            plot.bottom() + TICK_SIZE + 4.0,
            label.clone(),
            TICK_SIZE,
            theme.text_dim,
            Anchor::Middle,
        );
    }
}

/// How category labels fit under a band axis.
pub struct CategoryLabels {
    pub rotate: bool,
    pub every: usize,
    pub height: f32,
    pub max_width: f32,
}

pub fn plan_category_labels(labels: &[String], step: f32) -> CategoryLabels {
    let widest = labels
        .iter()
        .map(|l| text_width(l, TICK_SIZE))
        .fold(0.0, f32::max);
    if widest + 6.0 <= step {
        return CategoryLabels {
            rotate: false,
            every: 1,
            height: TICK_SIZE + 6.0,
            max_width: step,
        };
    }
    let max_width = widest.min(90.0);
    let every = ((TICK_SIZE * 1.3) / step.max(0.1)).ceil().max(1.0) as usize;
    CategoryLabels {
        rotate: true,
        every,
        height: max_width * 0.72 + TICK_SIZE + 6.0,
        max_width,
    }
}

pub fn draw_category_labels(
    scene: &mut Scene,
    labels: &[String],
    band: &Band,
    plan: &CategoryLabels,
    baseline: f32,
    theme: &ReportTheme,
) {
    for (i, label) in labels.iter().enumerate() {
        if i % plan.every != 0 {
            continue;
        }
        let x = band.center(i);
        let text = fit_text(label, TICK_SIZE, plan.max_width);
        if plan.rotate {
            scene.push_text(TextItem {
                x: x + TICK_SIZE * 0.35,
                y: baseline + 6.0,
                text,
                size: TICK_SIZE,
                color: theme.text_dim,
                anchor: Anchor::End,
                bold: false,
                rotate: -45.0,
            });
        } else {
            scene.text(
                x,
                baseline + TICK_SIZE + 4.0,
                text,
                TICK_SIZE,
                theme.text_dim,
                Anchor::Middle,
            );
        }
    }
}

/// Axis titles: x under the plot, y rotated along the left edge.
pub fn draw_axis_titles(
    scene: &mut Scene,
    spec: &ChartSpec,
    plot: Rect,
    area: Rect,
    theme: &ReportTheme,
) {
    if let Some(t) = spec.x_axis.title.as_deref().filter(|t| !t.is_empty()) {
        scene.text(
            plot.x + plot.w / 2.0,
            area.bottom(),
            fit_text(t, LABEL_SIZE, plot.w),
            LABEL_SIZE,
            theme.text_dim,
            Anchor::Middle,
        );
    }
    if let Some(t) = spec.y_axis.title.as_deref().filter(|t| !t.is_empty()) {
        scene.push_text(TextItem {
            x: area.x + LABEL_SIZE,
            y: plot.y + plot.h / 2.0,
            text: fit_text(t, LABEL_SIZE, plot.h),
            size: LABEL_SIZE,
            color: theme.text_dim,
            anchor: Anchor::Middle,
            bold: false,
            rotate: -90.0,
        });
    }
}

pub fn axis_title_space(spec: &ChartSpec) -> (f32, f32) {
    let has = |t: &Option<String>| t.as_deref().is_some_and(|t| !t.is_empty());
    (
        if has(&spec.y_axis.title) {
            LABEL_SIZE + 6.0
        } else {
            0.0
        },
        if has(&spec.x_axis.title) {
            LABEL_SIZE + 6.0
        } else {
            0.0
        },
    )
}

/// Dashed reference lines across a vertical value axis.
pub fn draw_reference_lines(
    scene: &mut Scene,
    spec: &ChartSpec,
    axis: &ValueAxis,
    plot: Rect,
    theme: &ReportTheme,
) {
    for line in &spec.reference_lines {
        if !line.value.is_finite() {
            continue;
        }
        let y = axis.scale.map(line.value);
        if y < plot.y - 0.5 || y > plot.bottom() + 0.5 {
            continue;
        }
        let color = line
            .status
            .map(|s| theme.status_color(s))
            .unwrap_or(theme.text_dim);
        scene.line(
            plot.x,
            y,
            plot.right(),
            y,
            Stroke::new(color, 1.25).dashed(5.0, 3.0),
        );
        if let Some(label) = line.label.as_deref().filter(|l| !l.is_empty()) {
            scene.text(
                plot.right() - 2.0,
                y - 3.0,
                fit_text(label, TICK_SIZE, plot.w / 2.0),
                TICK_SIZE,
                color,
                Anchor::End,
            );
        }
    }
}

pub fn reference_extent(spec: &ChartSpec) -> Option<(f64, f64)> {
    let vals = spec
        .reference_lines
        .iter()
        .map(|r| r.value)
        .filter(|v| v.is_finite());
    let min = vals.clone().reduce(f64::min)?;
    let max = vals.reduce(f64::max)?;
    Some((min, max))
}

pub fn accessible_title(spec: &ChartSpec) -> String {
    spec.title
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            let mut parts: Vec<&str> = spec.y.iter().map(|e| e.label()).collect();
            if let Some(x) = &spec.x {
                parts.push(x.label());
            }
            parts.join(", ")
        })
}

pub fn accessible_description(spec: &ChartSpec, table: &TableBlock) -> String {
    format!(
        "{} · {} · {}",
        spec.kind.name(),
        spec.columns().join(", "),
        table.rows.len()
    )
}

/// The box drawn instead of a chart that cannot be rendered.
pub fn placeholder(spec: &ChartSpec, options: ChartOptions, error: &ChartError) -> Scene {
    let (mut scene, area) = begin(spec, options);
    let theme = options.theme;
    scene.items.push(super::scene::Item::Rect {
        x: area.x,
        y: area.y,
        w: area.w,
        h: area.h,
        fill: Some(Fill::solid(theme.surface)),
        stroke: Some(Stroke::new(theme.border, 1.0).dashed(4.0, 3.0)),
        radius: 4.0,
    });
    let message = error_text(error, options.labels);
    scene.text(
        area.x + area.w / 2.0,
        area.y + area.h / 2.0 + LABEL_SIZE * 0.35,
        fit_text(&message, LABEL_SIZE, area.w - 16.0),
        LABEL_SIZE,
        theme.text_dim,
        Anchor::Middle,
    );
    scene.title = accessible_title(spec);
    scene.description = message;
    scene
}

pub fn error_text(error: &ChartError, labels: &ReportLabels) -> String {
    match error {
        ChartError::NoData => labels.chart_no_data.clone(),
        ChartError::Withheld => labels.data_withheld.clone(),
        ChartError::Invalid(errors) => ReportLabels::fill(
            &labels.chart_unavailable,
            &[(
                "reason",
                &errors
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join(" "),
            )],
        ),
    }
}
