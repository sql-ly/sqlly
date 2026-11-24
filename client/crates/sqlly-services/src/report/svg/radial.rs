//! Part-of-whole and single-value charts: pie, donut, funnel, treemap,
//! sparkline and the KPI card.

use std::collections::HashMap;

use super::ChartOptions;
use super::data::{ChartError, Ctx};
use super::parts::{LABEL_SIZE, PAD, Rect, TICK_SIZE, begin, colors, legend};
use super::scale::{fit_text, text_width};
use super::scene::{Anchor, Cmd, Fill, Item, Scene, Stroke, TextItem, arc};
use crate::report::chart::ChartKind;
use crate::report::doc::KpiBlock;
use crate::report::format::{NumberStyle, ValueFormat, format_number};
use crate::report::labels::ReportLabels;
use crate::report::theme::{ReportTheme, Rgb};

fn measure_format(ctx: &Ctx) -> ValueFormat {
    let format = ctx
        .spec
        .y
        .first()
        .map(|e| ctx.format(e).clone())
        .unwrap_or_default();
    if format.is_default() {
        ValueFormat::compact()
    } else {
        format
    }
}

pub fn pie(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let spec = ctx.spec;
    let theme = options.theme;
    let data = ctx.categories();
    let keyed: Vec<(String, f64, Option<String>)> = data
        .categories
        .iter()
        .zip(&data.series[0].values)
        .zip(&data.keys)
        .filter_map(|((c, v), k)| v.filter(|v| *v > 0.0).map(|v| (c.clone(), v, k.clone())))
        .collect();
    let slices: Vec<(String, f64)> = keyed.iter().map(|(c, v, _)| (c.clone(), *v)).collect();
    let total: f64 = slices.iter().map(|(_, v)| v).sum();
    if slices.is_empty() || total <= 0.0 {
        return Err(ChartError::NoData);
    }
    let (mut scene, area) = begin(spec, options);
    let palette = colors(spec, theme, slices.len());
    let format = measure_format(ctx);
    let items: Vec<(String, Rgb)> = if spec.show_legend {
        slices
            .iter()
            .zip(&palette)
            .map(|((name, v), c)| (format!("{name} · {}", format_number(*v, &format)), *c))
            .collect()
    } else {
        Vec::new()
    };
    let area = legend(&mut scene, area, &items, theme);
    let r = (area.w.min(area.h) / 2.0 - 4.0).max(8.0);
    let (cx, cy) = (area.x + area.w / 2.0, area.y + area.h / 2.0);
    let inner = if spec.kind == ChartKind::Donut {
        r * 0.58
    } else {
        0.0
    };
    let mut a0 = 0.0f32;
    let tau = std::f32::consts::TAU;
    for (i, (_, v)) in slices.iter().enumerate() {
        let sweep = (*v / total) as f32 * tau;
        let a1 = if i == slices.len() - 1 {
            tau
        } else {
            a0 + sweep
        };
        let mut cmds;
        if inner > 0.0 {
            cmds = arc(cx, cy, r, a0, a1, Cmd::Move(0.0, 0.0));
            let mut back = arc(cx, cy, inner, a1, a0, Cmd::Line(0.0, 0.0));
            cmds.append(&mut back);
        } else if slices.len() == 1 {
            cmds = arc(cx, cy, r, 0.0, tau, Cmd::Move(0.0, 0.0));
        } else {
            cmds = vec![Cmd::Move(cx, cy)];
            cmds.extend(arc(cx, cy, r, a0, a1, Cmd::Line(0.0, 0.0)));
        }
        cmds.push(Cmd::Close);
        scene.hit_path(&cmds, keyed[i].2.as_ref());
        scene.items.push(Item::Path {
            cmds,
            fill: Some(Fill::solid(palette[i])),
            stroke: Some(Stroke::new(theme.background, 1.0)),
        });
        let share = *v / total;
        if share >= 0.05 {
            let mid = a0 + (a1 - a0) / 2.0;
            let lr = if inner > 0.0 {
                (r + inner) / 2.0
            } else {
                r * 0.64
            };
            let (lx, ly) = (cx + lr * mid.sin(), cy - lr * mid.cos());
            scene.text(
                lx,
                ly + TICK_SIZE * 0.35,
                format_number(share, &ValueFormat::percent(0)),
                TICK_SIZE,
                theme.ink_on(palette[i]),
                Anchor::Middle,
            );
        }
        a0 = a1;
    }
    if inner > 0.0 {
        scene.push_text(TextItem {
            x: cx,
            y: cy + 6.0,
            text: format_number(total, &format),
            size: 17.0,
            color: theme.text,
            anchor: Anchor::Middle,
            bold: true,
            rotate: 0.0,
        });
    }
    Ok(scene)
}

/// The funnel's stages in order. Top-N does not apply: folding the smaller
/// stages into an "Other" stage at the bottom would draw a stage that does
/// not exist.
#[cfg(test)]
pub(crate) fn funnel_stages(ctx: &Ctx) -> Vec<(String, f64)> {
    keyed_funnel_stages(ctx)
        .into_iter()
        .map(|(name, v, _)| (name, v))
        .collect()
}

/// [`funnel_stages`] with each stage's cross-filter label.
fn keyed_funnel_stages(ctx: &Ctx) -> Vec<(String, f64, Option<String>)> {
    let mut spec = ctx.spec.clone();
    spec.top_n = None;
    let data = Ctx::new(&spec, ctx.table, ctx.labels).categories();
    ctx.capped.set(ctx.capped.get().max(data.capped));
    data.categories
        .iter()
        .zip(&data.series[0].values)
        .zip(&data.keys)
        .map(|((c, v), k)| (c.clone(), v.unwrap_or(0.0).max(0.0), k.clone()))
        .collect()
}

pub fn funnel(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let spec = ctx.spec;
    let theme = options.theme;
    let keyed = keyed_funnel_stages(ctx);
    let stages: Vec<(String, f64)> = keyed.iter().map(|(n, v, _)| (n.clone(), *v)).collect();
    let max = stages.iter().map(|(_, v)| *v).fold(0.0, f64::max);
    if stages.is_empty() || max <= 0.0 {
        return Err(ChartError::NoData);
    }
    let (mut scene, area) = begin(spec, options);
    let format = measure_format(ctx);
    let first = stages[0].1;
    let label_w = (area.w * 0.32).min(200.0);
    let bars = Rect {
        x: area.x + label_w,
        y: area.y,
        w: area.w - label_w,
        h: area.h,
    };
    let row = (bars.h / stages.len() as f32).min(48.0);
    let color = colors(spec, theme, 1)[0];
    for (i, (name, v)) in stages.iter().enumerate() {
        let w = (bars.w * (*v / max) as f32).max(1.0);
        let y = bars.y + row * i as f32;
        let x = bars.x + (bars.w - w) / 2.0;
        let shade = color.mix(theme.background, 0.12 * i as f32);
        scene.items.push(Item::Rect {
            x,
            y: y + 2.0,
            w,
            h: row - 4.0,
            fill: Some(Fill::solid(shade)),
            stroke: None,
            radius: 3.0,
        });
        scene.hit_rect(bars.x, y, bars.w, row, keyed[i].2.as_ref());
        scene.text(
            area.x,
            y + row / 2.0 + LABEL_SIZE * 0.35,
            fit_text(name, LABEL_SIZE, label_w - 8.0),
            LABEL_SIZE,
            theme.text,
            Anchor::Start,
        );
        // A share of an empty first stage is undefined, so it is left off.
        let value = if first > 0.0 {
            format!(
                "{} · {}",
                format_number(*v, &format),
                format_number(*v / first, &ValueFormat::percent(1))
            )
        } else {
            format_number(*v, &format)
        };
        let text_w = text_width(&value, TICK_SIZE);
        let (tx, ink, anchor) = if text_w + 8.0 < w {
            (bars.x + bars.w / 2.0, theme.ink_on(shade), Anchor::Middle)
        } else {
            (x + w + 4.0, theme.text_dim, Anchor::Start)
        };
        scene.text(
            tx,
            y + row / 2.0 + TICK_SIZE * 0.35,
            value,
            TICK_SIZE,
            ink,
            anchor,
        );
    }
    Ok(scene)
}

/// Squarified treemap layout of positive `values` (sorted descending) in
/// `rect`.
pub fn squarify(values: &[f64], rect: Rect) -> Vec<Rect> {
    let total: f64 = values.iter().sum();
    let mut out = vec![
        Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0
        };
        values.len()
    ];
    if total <= 0.0 || rect.w <= 0.0 || rect.h <= 0.0 {
        return out;
    }
    let scale = f64::from(rect.w * rect.h) / total;
    let areas: Vec<f64> = values.iter().map(|v| v * scale).collect();
    let mut free = rect;
    let mut i = 0;
    while i < areas.len() {
        let short = f64::from(free.w.min(free.h));
        let mut end = i + 1;
        let worst = |row: &[f64]| {
            let s: f64 = row.iter().sum();
            let (mx, mn) = row
                .iter()
                .fold((0.0f64, f64::INFINITY), |(a, b), v| (a.max(*v), b.min(*v)));
            ((short * short * mx) / (s * s)).max((s * s) / (short * short * mn))
        };
        while end < areas.len() && worst(&areas[i..=end]) <= worst(&areas[i..end]) {
            end += 1;
        }
        let row_sum: f64 = areas[i..end].iter().sum();
        if free.w >= free.h {
            let w = (row_sum / f64::from(free.h)) as f32;
            let mut y = free.y;
            for (k, a) in areas[i..end].iter().enumerate() {
                let h = (*a / f64::from(w.max(1e-6))) as f32;
                out[i + k] = Rect { x: free.x, y, w, h };
                y += h;
            }
            free = Rect {
                x: free.x + w,
                w: free.w - w,
                ..free
            };
        } else {
            let h = (row_sum / f64::from(free.w)) as f32;
            let mut x = free.x;
            for (k, a) in areas[i..end].iter().enumerate() {
                let w = (*a / f64::from(h.max(1e-6))) as f32;
                out[i + k] = Rect { x, y: free.y, w, h };
                x += w;
            }
            free = Rect {
                y: free.y + h,
                h: free.h - h,
                ..free
            };
        }
        i = end;
    }
    out
}

pub fn treemap(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let spec = ctx.spec;
    let theme = options.theme;
    let data = ctx.categories();
    let mut tiles: Vec<(String, usize, f64)> = Vec::new();
    let mut tile_keys: HashMap<String, Option<String>> = HashMap::new();
    for (s, series) in data.series.iter().enumerate() {
        for (c, v) in series.values.iter().enumerate() {
            if let Some(v) = v.filter(|v| *v > 0.0) {
                tiles.push((data.categories[c].clone(), s, v));
                tile_keys.insert(data.categories[c].clone(), data.keys[c].clone());
            }
        }
    }
    if tiles.is_empty() {
        return Err(ChartError::NoData);
    }
    tiles.sort_by(|a, b| b.2.total_cmp(&a.2).then(a.0.cmp(&b.0)));
    tiles.truncate(300);
    let (mut scene, area) = begin(spec, options);
    let grouped = spec.series.is_some() && data.series.len() > 1;
    let palette = colors(
        spec,
        theme,
        if grouped {
            data.series.len()
        } else {
            tiles.len()
        },
    );
    let items: Vec<(String, Rgb)> = if grouped && spec.show_legend {
        data.series
            .iter()
            .zip(&palette)
            .map(|(s, c)| (s.name.clone(), *c))
            .collect()
    } else {
        Vec::new()
    };
    let area = legend(&mut scene, area, &items, theme);
    let values: Vec<f64> = tiles.iter().map(|t| t.2).collect();
    let rects = squarify(&values, area);
    let format = measure_format(ctx);
    for (i, ((name, s, v), r)) in tiles.iter().zip(&rects).enumerate() {
        let color = if grouped {
            palette[*s]
        } else {
            palette[i % palette.len()]
        };
        scene.items.push(Item::Rect {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
            fill: Some(Fill::solid(color)),
            stroke: Some(Stroke::new(theme.background, 1.5)),
            radius: 0.0,
        });
        scene.hit_rect(
            r.x,
            r.y,
            r.w,
            r.h,
            tile_keys.get(name).and_then(Option::as_ref),
        );
        if r.w > 36.0 && r.h > 26.0 {
            let ink = theme.ink_on(color);
            scene.text(
                r.x + 5.0,
                r.y + 4.0 + LABEL_SIZE,
                fit_text(name, LABEL_SIZE, r.w - 10.0),
                LABEL_SIZE,
                ink,
                Anchor::Start,
            );
            scene.text(
                r.x + 5.0,
                r.y + 8.0 + LABEL_SIZE + TICK_SIZE,
                fit_text(&format_number(*v, &format), TICK_SIZE, r.w - 10.0),
                TICK_SIZE,
                ink,
                Anchor::Start,
            );
        }
    }
    Ok(scene)
}

pub fn sparkline_scene(values: &[f64], width: f32, height: f32, color: Rgb) -> Scene {
    let mut scene = Scene::new(width, height);
    let finite: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.len() < 2 {
        return scene;
    }
    let lo = finite.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = finite.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = if hi > lo { hi - lo } else { 1.0 };
    let pad = 2.5;
    let step = (width - 2.0 * pad) / (finite.len() - 1) as f32;
    let pts: Vec<(f32, f32)> = finite
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let t = ((v - lo) / span) as f32;
            (
                pad + step * i as f32,
                height - pad - t * (height - 2.0 * pad),
            )
        })
        .collect();
    let mut area = pts.clone();
    area.push((pts[pts.len() - 1].0, height));
    area.push((pts[0].0, height));
    scene.polygon(&area, Fill::alpha(color, 0.15));
    scene.polyline(&pts, Stroke::new(color, 1.5));
    let (lx, ly) = pts[pts.len() - 1];
    scene.circle(lx, ly, 2.2, Fill::solid(color));
    scene
}

/// The incomplete-data mark of a sparkline, too small for the badge's
/// text: a warning-colored flag in the top-right corner (the scene's
/// description carries the badge's words).
pub fn mark_partial(scene: &mut Scene, width: f32, color: Rgb) {
    let size = 7.0_f32.min(width / 4.0);
    scene.polygon(
        &[(width - size, 0.0), (width, 0.0), (width, size)],
        Fill::solid(color),
    );
}

pub fn sparkline_chart(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let data = ctx.categories();
    let values: Vec<f64> = data
        .series
        .first()
        .map(|s| s.values.iter().flatten().copied().collect())
        .unwrap_or_default();
    if values.len() < 2 {
        return Err(ChartError::NoData);
    }
    let color = colors(ctx.spec, options.theme, 1)[0];
    let mut scene = sparkline_scene(&values, options.width, options.height, color);
    scene.background = Some(options.theme.background);
    if options.badge.is_some_and(|b| !b.is_empty()) {
        mark_partial(&mut scene, options.width, options.theme.warning);
    }
    Ok(scene)
}

/// The KPI chart kind: the last value of the first measure, compared with
/// the one before it.
pub fn kpi_chart(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let data = ctx.categories();
    let values: Vec<f64> = data
        .series
        .first()
        .map(|s| s.values.iter().flatten().copied().collect())
        .unwrap_or_default();
    let Some(&last) = values.last() else {
        return Err(ChartError::NoData);
    };
    let label = ctx
        .spec
        .title
        .clone()
        .or_else(|| ctx.spec.y.first().map(|e| e.label().to_string()))
        .unwrap_or_default();
    let comparison = (values.len() >= 2).then(|| crate::report::doc::KpiComparison {
        label: data
            .categories
            .get(data.categories.len().saturating_sub(2))
            .cloned()
            .unwrap_or_default(),
        value: values[values.len() - 2],
    });
    let kpi = KpiBlock {
        label,
        value: Some(last),
        format: ctx
            .spec
            .y
            .first()
            .map(|e| ctx.format(e).clone())
            .unwrap_or_default(),
        comparison,
        sparkline: values,
        higher_is_better: true,
        ..Default::default()
    };
    Ok(kpi_card(&kpi, options))
}

pub fn delta_text(kpi: &KpiBlock, labels: &ReportLabels) -> Option<String> {
    let comparison = kpi.comparison.as_ref()?;
    let delta = kpi.delta()?;
    let arrow = if delta > 0.0 {
        "▲"
    } else if delta < 0.0 {
        "▼"
    } else {
        "■"
    };
    let change = match kpi.delta_ratio() {
        Some(r) => format_number(
            r,
            &ValueFormat {
                style: NumberStyle::Percent,
                decimals: Some(1),
                ..Default::default()
            },
        ),
        None => format_number(delta, &kpi.format),
    };
    let sign = if delta > 0.0 { "+" } else { "" };
    let versus = ReportLabels::fill(&labels.compared_with, &[("label", &comparison.label)]);
    Some(format!("{arrow} {sign}{change} {versus}"))
}

/// Good news when the value moved the way the KPI wants.
pub fn delta_color(kpi: &KpiBlock, theme: &ReportTheme) -> Rgb {
    match kpi.delta() {
        Some(0.0) => theme.text_dim,
        Some(d) if (d > 0.0) == kpi.higher_is_better => theme.good,
        Some(_) => theme.critical,
        None => theme.text_dim,
    }
}

pub fn kpi_value_text(kpi: &KpiBlock) -> String {
    match kpi.value {
        Some(v) => format_number(v, &kpi.format),
        None => "—".into(),
    }
}

pub fn kpi_card(kpi: &KpiBlock, options: ChartOptions) -> Scene {
    let theme = options.theme;
    let (w, h) = (options.width, options.height);
    let mut scene = Scene::new(w, h);
    scene.background = Some(theme.background);
    scene.title = kpi.label.clone();
    scene.description = kpi_value_text(kpi);
    let status = kpi.resolved_status();
    scene.items.push(Item::Rect {
        x: 0.5,
        y: 0.5,
        w: w - 1.0,
        h: h - 1.0,
        fill: Some(Fill::solid(theme.surface)),
        stroke: Some(Stroke::new(theme.border, 1.0)),
        radius: 6.0,
    });
    if let Some(s) = status {
        scene.rect(0.5, 6.0, 3.5, h - 12.0, Fill::solid(theme.status_color(s)));
    }
    let x = PAD + 2.0;
    scene.text(
        x,
        PAD + LABEL_SIZE,
        fit_text(&kpi.label, LABEL_SIZE, w - 2.0 * PAD),
        LABEL_SIZE,
        theme.text_dim,
        Anchor::Start,
    );
    let value_size = (h * 0.24).clamp(14.0, 34.0);
    let value_y = PAD + LABEL_SIZE + 6.0 + value_size;
    scene.push_text(TextItem {
        x,
        y: value_y,
        text: fit_text(&kpi_value_text(kpi), value_size, w - 2.0 * PAD),
        size: value_size,
        color: theme.text,
        anchor: Anchor::Start,
        bold: true,
        rotate: 0.0,
    });
    let mut y = value_y + TICK_SIZE + 6.0;
    if let Some(delta) = delta_text(kpi, options.labels) {
        scene.text(
            x,
            y,
            fit_text(&delta, TICK_SIZE, w - 2.0 * PAD),
            TICK_SIZE,
            delta_color(kpi, theme),
            Anchor::Start,
        );
        y += TICK_SIZE + 4.0;
    }
    if let Some(s) = status {
        scene.text(
            x,
            y,
            options.labels.status(s).to_string(),
            TICK_SIZE,
            theme.status_color(s),
            Anchor::Start,
        );
        y += TICK_SIZE + 4.0;
    }
    if let Some(caption) = kpi.caption.as_deref().filter(|c| !c.is_empty()) {
        scene.text(
            x,
            y,
            fit_text(caption, TICK_SIZE, w - 2.0 * PAD),
            TICK_SIZE,
            theme.warning,
            Anchor::Start,
        );
        y += TICK_SIZE + 4.0;
    }
    let spark_h = h - y - PAD + 4.0;
    if kpi.sparkline.len() >= 2 && spark_h >= 12.0 {
        let spark = sparkline_scene(&kpi.sparkline, w - 2.0 * PAD, spark_h, theme.accent);
        offset_into(&mut scene, spark, PAD, y);
    }
    scene
}

/// Appends `other`'s items to `scene`, shifted by (dx, dy).
pub fn offset_into(scene: &mut Scene, other: Scene, dx: f32, dy: f32) {
    let shift_cmd = |c: Cmd| match c {
        Cmd::Move(x, y) => Cmd::Move(x + dx, y + dy),
        Cmd::Line(x, y) => Cmd::Line(x + dx, y + dy),
        Cmd::Cubic(a, b, c, d, x, y) => Cmd::Cubic(a + dx, b + dy, c + dx, d + dy, x + dx, y + dy),
        Cmd::Close => Cmd::Close,
    };
    for item in other.items {
        scene.items.push(match item {
            Item::Rect {
                x,
                y,
                w,
                h,
                fill,
                stroke,
                radius,
            } => Item::Rect {
                x: x + dx,
                y: y + dy,
                w,
                h,
                fill,
                stroke,
                radius,
            },
            Item::Line {
                x1,
                y1,
                x2,
                y2,
                stroke,
            } => Item::Line {
                x1: x1 + dx,
                y1: y1 + dy,
                x2: x2 + dx,
                y2: y2 + dy,
                stroke,
            },
            Item::Path { cmds, fill, stroke } => Item::Path {
                cmds: cmds.into_iter().map(shift_cmd).collect(),
                fill,
                stroke,
            },
            Item::Circle {
                cx,
                cy,
                r,
                fill,
                stroke,
            } => Item::Circle {
                cx: cx + dx,
                cy: cy + dy,
                r,
                fill,
                stroke,
            },
            Item::Text(mut t) => {
                t.x += dx;
                t.y += dy;
                Item::Text(t)
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squarify_fills_the_rectangle() {
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            w: 600.0,
            h: 400.0,
        };
        let values = [6.0, 6.0, 4.0, 3.0, 2.0, 2.0, 1.0];
        let rects = squarify(&values, rect);
        let area: f32 = rects.iter().map(|r| r.w * r.h).sum();
        assert!((area - 240_000.0).abs() < 1.0, "{area}");
        for r in &rects {
            assert!(r.x >= -0.01 && r.y >= -0.01 && r.right() <= 600.01 && r.bottom() <= 400.01);
        }
        let ratio = rects[0].w / rects[0].h;
        assert!(ratio > 0.5 && ratio < 2.0, "{ratio}");
    }

    #[test]
    fn funnels_keep_their_stages_and_skip_undefined_shares() {
        use crate::report::chart::{ChartSpec, TopN};
        use crate::report::doc::{TableBlock, TableColumn};
        use sqlly_core::CellValue;
        let table = |values: &[(&str, f64)]| {
            TableBlock::new(
                vec![
                    TableColumn::new("stage", "varchar"),
                    TableColumn::new("n", "int"),
                ],
                values
                    .iter()
                    .map(|(s, n)| vec![CellValue::String((*s).into()), CellValue::F64(*n)])
                    .collect(),
            )
        };
        let labels = ReportLabels::default();
        let mut spec = ChartSpec::new(ChartKind::Funnel).x("stage").y("n");
        spec.top_n = Some(TopN { n: 2, other: true });
        let t = table(&[
            ("Visit", 1000.0),
            ("Signup", 300.0),
            ("Trial", 100.0),
            ("Paid", 40.0),
        ]);
        let stages = funnel_stages(&Ctx::new(&spec, &t, &labels));
        let names: Vec<&str> = stages.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["Visit", "Signup", "Trial", "Paid"]);

        let t = table(&[("A", 0.0), ("B", 50.0), ("C", 20.0)]);
        let spec = ChartSpec::new(ChartKind::Funnel).x("stage").y("n");
        let scene = funnel(
            &Ctx::new(&spec, &t, &labels),
            super::super::ChartOptions::new(&crate::report::theme::LIGHT, &labels),
        )
        .unwrap();
        let texts: Vec<&str> = scene
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Text(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.contains(&"50"), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains('%')), "{texts:?}");
    }

    #[test]
    fn deltas_respect_direction() {
        let kpi = KpiBlock {
            label: "Errors".into(),
            value: Some(12.0),
            comparison: Some(crate::report::doc::KpiComparison {
                label: "last week".into(),
                value: 10.0,
            }),
            higher_is_better: false,
            ..Default::default()
        };
        assert_eq!(
            delta_text(&kpi, &ReportLabels::default()).unwrap(),
            "▲ +20.0% vs last week"
        );
        assert_eq!(
            delta_color(&kpi, &crate::report::theme::LIGHT),
            crate::report::theme::LIGHT.critical
        );
    }
}
