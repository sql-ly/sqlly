//! Charts on x/y axes: bar, line, area, scatter, bubble, histogram, box
//! plot and waterfall.

use super::ChartOptions;
use super::data::{CategoryData, ChartError, Ctx, quantile};
use super::parts::{
    Rect, TICK_SIZE, axis_format, axis_title_space, begin, colors, draw_axis_titles,
    draw_category_labels, draw_reference_lines, draw_value_axis_bottom, draw_value_axis_left,
    legend, plan_category_labels, reference_extent, tick_label, value_axis,
};
use super::scale::{Band, Linear, fit_text, nice_ticks, text_width, time_ticks};
use super::scene::{Anchor, Fill, Scene, Stroke};
use crate::report::chart::{BarMode, ChartKind, Orientation};
use crate::report::format::{ValueFormat, format_number};

fn first_measure<'a>(ctx: &'a Ctx) -> Option<&'a str> {
    ctx.spec.y.first().map(|e| e.column.as_str())
}

fn value_format(ctx: &Ctx) -> ValueFormat {
    axis_format(ctx, &ctx.spec.y_axis.format, first_measure(ctx))
}

fn shown_value(ctx: &Ctx, v: f64) -> String {
    let column = first_measure(ctx).and_then(|c| ctx.table.column_index(c));
    let format = column
        .map(|i| ctx.table.columns[i].format.clone())
        .unwrap_or_default();
    if format.is_default() {
        format_number(v, &ValueFormat::compact())
    } else {
        format_number(v, &format)
    }
}

fn series_legend(
    ctx: &Ctx,
    data: &CategoryData,
    options: ChartOptions,
) -> Vec<(String, crate::report::theme::Rgb)> {
    if !ctx.spec.show_legend || data.series.len() < 2 {
        return Vec::new();
    }
    let palette = colors(ctx.spec, options.theme, data.series.len());
    data.series
        .iter()
        .zip(palette)
        .map(|(s, c)| (s.name.clone(), c))
        .collect()
}

/// Stacked (or 100%) extents per category: (negative base, positive top).
fn stacks(data: &CategoryData, normalized: bool) -> Vec<Vec<(f64, f64)>> {
    let n = data.categories.len();
    let mut out = vec![Vec::new(); data.series.len()];
    for c in 0..n {
        let total: f64 = data
            .series
            .iter()
            .filter_map(|s| s.values[c])
            .map(f64::abs)
            .sum();
        let (mut pos, mut neg) = (0.0, 0.0);
        for (s, series) in data.series.iter().enumerate() {
            let mut v = series.values[c].unwrap_or(0.0);
            if normalized && total > 0.0 {
                v /= total;
            }
            if v >= 0.0 {
                out[s].push((pos, pos + v));
                pos += v;
            } else {
                out[s].push((neg + v, neg));
                neg += v;
            }
        }
    }
    out
}

#[allow(clippy::needless_range_loop)]
pub fn bar(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let spec = ctx.spec;
    let theme = options.theme;
    let data = ctx.categories();
    if data.is_empty() {
        return Err(ChartError::NoData);
    }
    let (mut scene, area) = begin(spec, options);
    let items = series_legend(ctx, &data, options);
    let area = legend(&mut scene, area, &items, theme);
    let palette = colors(spec, theme, data.series.len());
    let stacked = spec.bar_mode != BarMode::Grouped && data.series.len() > 1;
    let normalized = spec.bar_mode == BarMode::Normalized && stacked;
    let extents = stacks(&data, normalized);
    let (mut lo, mut hi) = if stacked {
        extents
            .iter()
            .flatten()
            .fold((0.0f64, 0.0f64), |(lo, hi), (a, b)| {
                (lo.min(*a), hi.max(*b))
            })
    } else {
        data.series
            .iter()
            .flat_map(|s| s.values.iter().flatten())
            .fold((0.0f64, 0.0f64), |(lo, hi), v| (lo.min(*v), hi.max(*v)))
    };
    if let Some((a, b)) = reference_extent(spec) {
        lo = lo.min(a);
        hi = hi.max(b);
    }
    let format = if normalized {
        ValueFormat::percent(0)
    } else {
        value_format(ctx)
    };
    let (title_left, title_bottom) = axis_title_space(spec);
    let horizontal = spec.orientation == Orientation::Horizontal;
    let n = data.categories.len();

    if horizontal {
        let label_w = data
            .categories
            .iter()
            .map(|l| text_width(l, TICK_SIZE))
            .fold(0.0, f32::max)
            .min(area.w * 0.35)
            + 8.0;
        let plot = Rect {
            x: area.x + title_left + label_w,
            y: area.y,
            w: area.w - title_left - label_w - 8.0,
            h: area.h - TICK_SIZE - 8.0 - title_bottom,
        };
        let axis = value_axis(
            lo,
            hi,
            true,
            spec.y_axis.min,
            spec.y_axis.max,
            &format,
            (plot.x, plot.right()),
        );
        draw_value_axis_bottom(&mut scene, &axis, plot, theme);
        let band = Band::new(n, (plot.y, plot.bottom()), 0.25);
        for (i, label) in data.categories.iter().enumerate() {
            scene.text(
                plot.x - 6.0,
                band.center(i) + TICK_SIZE * 0.35,
                fit_text(label, TICK_SIZE, label_w - 8.0),
                TICK_SIZE,
                theme.text_dim,
                Anchor::End,
            );
        }
        let inner = if stacked { 1 } else { data.series.len().max(1) };
        let sub = band.width / inner as f32;
        for (s, series) in data.series.iter().enumerate() {
            for c in 0..n {
                let Some(v) = series.values[c] else { continue };
                let (a, b) = if stacked {
                    extents[s][c]
                } else {
                    (0.0f64.min(v), 0.0f64.max(v))
                };
                let (x0, x1) = (axis.scale.map(a), axis.scale.map(b));
                let y = band.left(c) + if stacked { 0.0 } else { sub * s as f32 };
                scene.rect(
                    x0.min(x1),
                    y,
                    (x1 - x0).abs().max(0.5),
                    sub.max(0.5),
                    Fill::solid(palette[s]),
                );
                scene.hit_rect(
                    x0.min(x1),
                    y,
                    (x1 - x0).abs().max(0.5),
                    sub.max(0.5),
                    data.keys.get(c).and_then(Option::as_ref),
                );
                if spec.show_values && !stacked {
                    scene.text(
                        x1 + 3.0,
                        y + sub / 2.0 + TICK_SIZE * 0.35,
                        shown_value(ctx, v),
                        TICK_SIZE,
                        theme.text,
                        Anchor::Start,
                    );
                }
            }
        }
        draw_axis_titles(&mut scene, spec, plot, area, theme);
        return Ok(scene);
    }

    let probe = value_axis(
        lo,
        hi,
        true,
        spec.y_axis.min,
        spec.y_axis.max,
        &format,
        (0.0, 1.0),
    );
    let left = area.x + title_left + probe.width + 8.0;
    let plot_w = area.right() - left;
    let band_probe = Band::new(n, (0.0, plot_w), 0.2);
    let plan = plan_category_labels(&data.categories, band_probe.step);
    let plot = Rect {
        x: left,
        y: area.y + 4.0,
        w: plot_w,
        h: (area.h - plan.height - title_bottom - 4.0).max(10.0),
    };
    let axis = value_axis(
        lo,
        hi,
        true,
        spec.y_axis.min,
        spec.y_axis.max,
        &format,
        (plot.bottom(), plot.y),
    );
    draw_value_axis_left(&mut scene, &axis, plot, theme);
    let band = Band::new(n, (plot.x, plot.right()), 0.2);
    draw_category_labels(
        &mut scene,
        &data.categories,
        &band,
        &plan,
        plot.bottom(),
        theme,
    );
    let inner = if stacked { 1 } else { data.series.len().max(1) };
    let sub = band.width / inner as f32;
    for (s, series) in data.series.iter().enumerate() {
        for c in 0..n {
            let Some(v) = series.values[c] else { continue };
            let (a, b) = if stacked {
                extents[s][c]
            } else {
                (0.0f64.min(v), 0.0f64.max(v))
            };
            let (y0, y1) = (axis.scale.map(b), axis.scale.map(a));
            let x = band.left(c) + if stacked { 0.0 } else { sub * s as f32 };
            scene.rect(
                x,
                y0.min(y1),
                sub.max(0.5),
                (y1 - y0).abs().max(0.5),
                Fill::solid(palette[s]),
            );
            scene.hit_rect(
                x,
                y0.min(y1),
                sub.max(0.5),
                (y1 - y0).abs().max(0.5),
                data.keys.get(c).and_then(Option::as_ref),
            );
            if spec.show_values && !stacked && sub >= 14.0 {
                let y = if v >= 0.0 {
                    y0 - 3.0
                } else {
                    y1 + TICK_SIZE + 2.0
                };
                scene.text(
                    x + sub / 2.0,
                    y,
                    shown_value(ctx, v),
                    TICK_SIZE,
                    theme.text,
                    Anchor::Middle,
                );
            }
        }
    }
    draw_reference_lines(&mut scene, spec, &axis, plot, theme);
    draw_annotations(&mut scene, ctx, &data, |i| band.center(i), plot, options);
    draw_axis_titles(&mut scene, spec, plot, area, theme);
    Ok(scene)
}

fn draw_annotations(
    scene: &mut Scene,
    ctx: &Ctx,
    data: &CategoryData,
    x_of: impl Fn(usize) -> f32,
    plot: Rect,
    options: ChartOptions,
) {
    for note in &ctx.spec.annotations {
        let Some(i) = data.categories.iter().position(|c| c == &note.x) else {
            continue;
        };
        let x = x_of(i);
        scene.line(
            x,
            plot.y,
            x,
            plot.bottom(),
            Stroke::new(options.theme.text_dim, 1.0).dashed(2.0, 3.0),
        );
        let anchor = if x > plot.x + plot.w * 0.7 {
            Anchor::End
        } else {
            Anchor::Start
        };
        let dx = if anchor == Anchor::End { -3.0 } else { 3.0 };
        scene.text(
            x + dx,
            plot.y + TICK_SIZE,
            fit_text(&note.text, TICK_SIZE, plot.w * 0.4),
            TICK_SIZE,
            options.theme.text,
            anchor,
        );
    }
}

#[allow(clippy::needless_range_loop)]
pub fn line(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let spec = ctx.spec;
    let theme = options.theme;
    let data = ctx.categories();
    if data.is_empty() {
        return Err(ChartError::NoData);
    }
    let area_kind = spec.kind == ChartKind::Area;
    let stacked = area_kind && spec.bar_mode != BarMode::Grouped && data.series.len() > 1;
    let normalized = stacked && spec.bar_mode == BarMode::Normalized;
    let (mut scene, area) = begin(spec, options);
    let items = series_legend(ctx, &data, options);
    let area = legend(&mut scene, area, &items, theme);
    let palette = colors(spec, theme, data.series.len());
    let extents = stacks(&data, normalized);
    let mut values: Vec<f64> = if stacked {
        extents
            .iter()
            .flatten()
            .flat_map(|(a, b)| [*a, *b])
            .collect()
    } else {
        data.series
            .iter()
            .flat_map(|s| s.values.iter().flatten().copied())
            .collect()
    };
    if let Some((lo, hi)) = &data.band {
        values.extend(lo.iter().flatten());
        values.extend(hi.iter().flatten());
    }
    if let Some((a, b)) = reference_extent(spec) {
        values.extend([a, b]);
    }
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let format = if normalized {
        ValueFormat::percent(0)
    } else {
        value_format(ctx)
    };
    let (title_left, title_bottom) = axis_title_space(spec);
    let probe = value_axis(
        lo,
        hi,
        area_kind,
        spec.y_axis.min,
        spec.y_axis.max,
        &format,
        (0.0, 1.0),
    );
    let left = area.x + title_left + probe.width + 8.0;
    let plot_w = area.right() - left - 4.0;
    let n = data.categories.len();
    let continuous = data
        .times
        .as_ref()
        .filter(|t| t.len() > 1 && t.iter().all(|v| v.is_finite()));
    let band_probe = Band::new(n, (0.0, plot_w), 0.0);
    let plan = plan_category_labels(&data.categories, band_probe.step);
    let label_h = if continuous.is_some() {
        TICK_SIZE + 6.0
    } else {
        plan.height
    };
    let plot = Rect {
        x: left,
        y: area.y + 4.0,
        w: plot_w,
        h: (area.h - label_h - title_bottom - 4.0).max(10.0),
    };
    let axis = value_axis(
        lo,
        hi,
        area_kind,
        spec.y_axis.min,
        spec.y_axis.max,
        &format,
        (plot.bottom(), plot.y),
    );
    draw_value_axis_left(&mut scene, &axis, plot, theme);
    let band = Band::new(n, (plot.x, plot.right()), 0.0);
    let x_of: Box<dyn Fn(usize) -> f32> = match continuous {
        Some(times) => {
            let (t0, t1) = (times[0].min(times[n - 1]), times[0].max(times[n - 1]));
            let scale = Linear::new((t0, t1), (plot.x, plot.right()));
            for (t, label) in time_ticks(t0, t1, (plot.w / 80.0) as usize) {
                let x = scale.map(t);
                scene.line(
                    x,
                    plot.bottom(),
                    x,
                    plot.bottom() + 3.0,
                    Stroke::new(theme.border, 1.0),
                );
                scene.text(
                    x,
                    plot.bottom() + TICK_SIZE + 4.0,
                    label,
                    TICK_SIZE,
                    theme.text_dim,
                    Anchor::Middle,
                );
            }
            let times = times.clone();
            Box::new(move |i| scale.map(times[i]))
        }
        None => {
            draw_category_labels(
                &mut scene,
                &data.categories,
                &band,
                &plan,
                plot.bottom(),
                theme,
            );
            Box::new(move |i| band.center(i))
        }
    };
    // Each category is clickable across its slice of the plot.
    let xs: Vec<f32> = (0..n).map(&x_of).collect();
    for c in 0..n {
        let from = if c == 0 {
            plot.x
        } else {
            (xs[c - 1] + xs[c]) / 2.0
        };
        let to = if c + 1 == n {
            plot.right()
        } else {
            (xs[c] + xs[c + 1]) / 2.0
        };
        scene.hit_rect(
            from.min(to),
            plot.y,
            (to - from).abs(),
            plot.h,
            data.keys.get(c).and_then(Option::as_ref),
        );
    }
    if let Some((lo, hi)) = &data.band {
        let mut run: Vec<(f32, f32, f32)> = Vec::new();
        let flush = |run: &mut Vec<(f32, f32, f32)>, scene: &mut Scene| {
            if run.len() > 1 {
                let mut pts: Vec<(f32, f32)> = run.iter().map(|(x, l, _)| (*x, *l)).collect();
                pts.extend(run.iter().rev().map(|(x, _, h)| (*x, *h)));
                scene.polygon(&pts, Fill::alpha(theme.accent, 0.14));
            }
            run.clear();
        };
        for i in 0..n {
            match (lo[i], hi[i]) {
                (Some(l), Some(h)) => run.push((x_of(i), axis.scale.map(l), axis.scale.map(h))),
                _ => flush(&mut run, &mut scene),
            }
        }
        flush(&mut run, &mut scene);
    }
    let baseline = axis.scale.map(0.0f64.max(axis.scale.d0));
    for (s, series) in data.series.iter().enumerate() {
        let color = palette[s];
        let mut segments: Vec<Vec<(f32, f32, usize)>> = vec![Vec::new()];
        for c in 0..n {
            let point = if stacked {
                series.values[c].map(|_| extents[s][c].1)
            } else {
                series.values[c]
            };
            match point {
                Some(v) => segments.last_mut().unwrap_or(&mut Vec::new()).push((
                    x_of(c),
                    axis.scale.map(v),
                    c,
                )),
                None if !stacked => segments.push(Vec::new()),
                None => {}
            }
        }
        for seg in segments.iter().filter(|s| !s.is_empty()) {
            let pts: Vec<(f32, f32)> = seg.iter().map(|(x, y, _)| (*x, *y)).collect();
            if area_kind {
                let mut poly = pts.clone();
                if stacked {
                    poly.extend(
                        seg.iter()
                            .rev()
                            .map(|(x, _, c)| (*x, axis.scale.map(extents[s][*c].0))),
                    );
                } else {
                    poly.push((pts[pts.len() - 1].0, baseline));
                    poly.push((pts[0].0, baseline));
                }
                scene.polygon(&poly, Fill::alpha(color, if stacked { 0.75 } else { 0.22 }));
            }
            scene.polyline(&pts, Stroke::new(color, 2.0));
            if pts.len() == 1 || (n <= 40 && !area_kind) {
                for (x, y) in &pts {
                    scene.circle(*x, *y, 2.6, Fill::solid(color));
                }
            }
        }
        if s == 0
            && let Some((lo, hi)) = &data.band
        {
            for c in 0..n {
                if let (Some(v), Some(l), Some(h)) = (series.values[c], lo[c], hi[c])
                    && (v < l || v > h)
                {
                    let (x, y) = (x_of(c), axis.scale.map(v));
                    scene.circle(x, y, 4.5, Fill::solid(theme.critical));
                    scene.circle(x, y, 2.0, Fill::solid(theme.background));
                }
            }
        }
        if spec.show_values && n <= 24 && !stacked {
            for c in 0..n {
                if let Some(v) = series.values[c] {
                    scene.text(
                        x_of(c),
                        axis.scale.map(v) - 6.0,
                        shown_value(ctx, v),
                        TICK_SIZE,
                        theme.text,
                        Anchor::Middle,
                    );
                }
            }
        }
    }
    draw_reference_lines(&mut scene, spec, &axis, plot, theme);
    draw_annotations(&mut scene, ctx, &data, &x_of, plot, options);
    draw_axis_titles(&mut scene, spec, plot, area, theme);
    Ok(scene)
}

pub fn scatter(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let spec = ctx.spec;
    let theme = options.theme;
    let data = ctx.points();
    if data.points.is_empty() {
        return Err(ChartError::NoData);
    }
    let (mut scene, area) = begin(spec, options);
    let palette = colors(spec, theme, data.groups.len());
    let items: Vec<_> = if spec.show_legend && data.groups.len() > 1 {
        data.groups
            .iter()
            .cloned()
            .zip(palette.iter().copied())
            .collect()
    } else {
        Vec::new()
    };
    let area = legend(&mut scene, area, &items, theme);
    let (title_left, title_bottom) = axis_title_space(spec);
    let ys = data.points.iter().map(|p| p.y);
    let (ylo, yhi) = (
        ys.clone().fold(f64::INFINITY, f64::min),
        ys.fold(f64::NEG_INFINITY, f64::max),
    );
    let format = value_format(ctx);
    let probe = value_axis(
        ylo,
        yhi,
        false,
        spec.y_axis.min,
        spec.y_axis.max,
        &format,
        (0.0, 1.0),
    );
    let plot = Rect {
        x: area.x + title_left + probe.width + 8.0,
        y: area.y + 4.0,
        w: area.w - title_left - probe.width - 16.0,
        h: area.h - TICK_SIZE - 10.0 - title_bottom,
    };
    let axis = value_axis(
        ylo,
        yhi,
        false,
        spec.y_axis.min,
        spec.y_axis.max,
        &format,
        (plot.bottom(), plot.y),
    );
    draw_value_axis_left(&mut scene, &axis, plot, theme);
    let xs = data.points.iter().map(|p| p.x);
    let (xlo, xhi) = (
        xs.clone().fold(f64::INFINITY, f64::min),
        xs.fold(f64::NEG_INFINITY, f64::max),
    );
    let x_scale = if data.x_temporal {
        let pad = ((xhi - xlo) * 0.02).max(1.0);
        let scale = Linear::new((xlo - pad, xhi + pad), (plot.x, plot.right()));
        for (t, label) in time_ticks(xlo - pad, xhi + pad, (plot.w / 90.0) as usize) {
            let x = scale.map(t);
            scene.line(x, plot.y, x, plot.bottom(), Stroke::new(theme.grid, 1.0));
            scene.text(
                x,
                plot.bottom() + TICK_SIZE + 4.0,
                label,
                TICK_SIZE,
                theme.text_dim,
                Anchor::Middle,
            );
        }
        scale
    } else {
        let x_format = axis_format(
            ctx,
            &spec.x_axis.format,
            spec.x.as_ref().map(|e| e.column.as_str()),
        );
        let xaxis = value_axis(
            xlo,
            xhi,
            false,
            spec.x_axis.min,
            spec.x_axis.max,
            &x_format,
            (plot.x, plot.right()),
        );
        draw_value_axis_bottom(&mut scene, &xaxis, plot, theme);
        xaxis.scale
    };
    let sizes = data
        .points
        .iter()
        .filter_map(|p| p.size)
        .filter(|s| *s > 0.0);
    let smax = sizes.fold(0.0f64, f64::max);
    for p in &data.points {
        let r = match (spec.kind, p.size) {
            (ChartKind::Bubble, Some(s)) if smax > 0.0 && s > 0.0 => {
                3.0 + 15.0 * (s / smax).sqrt() as f32
            }
            (ChartKind::Bubble, _) => 2.0,
            _ => 3.0,
        };
        scene.items.push(super::scene::Item::Circle {
            cx: x_scale.map(p.x),
            cy: axis.scale.map(p.y),
            r,
            fill: Some(Fill::alpha(palette[p.group.min(palette.len() - 1)], 0.7)),
            stroke: (spec.kind == ChartKind::Bubble).then(|| Stroke::new(theme.background, 0.75)),
        });
    }
    draw_reference_lines(&mut scene, spec, &axis, plot, theme);
    draw_axis_titles(&mut scene, spec, plot, area, theme);
    Ok(scene)
}

/// The bin of `value` among `edges` (bins are `[edges[i], edges[i + 1])`,
/// the last one closed). Compared against the rounded edges themselves, so
/// 0.3 lands in `[0.3, 0.4)` rather than where `0.3 / 0.1` rounds down to.
fn bin_index(edges: &[f64], value: f64) -> usize {
    let bins = edges.len().saturating_sub(1).max(1);
    if bins <= 1 {
        return 0;
    }
    edges[1..bins]
        .partition_point(|edge| *edge <= value)
        .min(bins - 1)
}

pub fn histogram(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let spec = ctx.spec;
    let theme = options.theme;
    let Some(measure) = spec.y.first() else {
        return Err(ChartError::NoData);
    };
    let values = ctx.values(measure);
    if values.is_empty() {
        return Err(ChartError::NoData);
    }
    let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let target = spec
        .bins
        .unwrap_or_else(|| ((values.len() as f64).log2().ceil() as usize + 1).clamp(5, 40));
    let (d0, d1, edges) = nice_ticks(lo, hi, target);
    let step = if edges.len() > 1 {
        edges[1] - edges[0]
    } else {
        1.0
    };
    let bins = edges.len().saturating_sub(1).max(1);
    let mut counts = vec![0usize; bins];
    for v in &values {
        counts[bin_index(&edges, *v)] += 1;
    }
    let (mut scene, area) = begin(spec, options);
    let (title_left, title_bottom) = axis_title_space(spec);
    let max = *counts.iter().max().unwrap_or(&1) as f64;
    let count_format = ValueFormat::default();
    let probe = value_axis(0.0, max, true, None, None, &count_format, (0.0, 1.0));
    let plot = Rect {
        x: area.x + title_left + probe.width + 8.0,
        y: area.y + 4.0,
        w: area.w - title_left - probe.width - 16.0,
        h: area.h - TICK_SIZE - 10.0 - title_bottom,
    };
    let axis = value_axis(
        0.0,
        max,
        true,
        None,
        None,
        &count_format,
        (plot.bottom(), plot.y),
    );
    draw_value_axis_left(&mut scene, &axis, plot, theme);
    let x = Linear::new((d0, d1), (plot.x, plot.right()));
    let label_format = axis_format(ctx, &spec.x_axis.format, Some(&measure.column));
    let label_every = ((edges.len() as f32 * 48.0) / plot.w).ceil().max(1.0) as usize;
    for (i, e) in edges.iter().enumerate() {
        if i % label_every == 0 {
            scene.text(
                x.map(*e),
                plot.bottom() + TICK_SIZE + 4.0,
                tick_label(*e, &label_format, step),
                TICK_SIZE,
                theme.text_dim,
                Anchor::Middle,
            );
        }
    }
    let color = colors(spec, theme, 1)[0];
    for (i, count) in counts.iter().enumerate() {
        if *count == 0 {
            continue;
        }
        let (x0, x1) = (
            x.map(d0 + step * i as f64),
            x.map(d0 + step * (i + 1) as f64),
        );
        let y = axis.scale.map(*count as f64);
        scene.rect(
            x0 + 0.5,
            y,
            (x1 - x0 - 1.0).max(0.5),
            plot.bottom() - y,
            Fill::solid(color),
        );
    }
    draw_reference_lines(&mut scene, spec, &axis, plot, theme);
    draw_axis_titles(&mut scene, spec, plot, area, theme);
    Ok(scene)
}

pub fn box_plot(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let spec = ctx.spec;
    let theme = options.theme;
    let Some(measure) = spec.y.first() else {
        return Err(ChartError::NoData);
    };
    let groups = ctx.groups(measure);
    if groups.iter().all(|(_, v)| v.is_empty()) {
        return Err(ChartError::NoData);
    }
    let (mut scene, area) = begin(spec, options);
    let all: Vec<f64> = groups.iter().flat_map(|(_, v)| v.iter().copied()).collect();
    let lo = all.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = all.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let format = value_format(ctx);
    let (title_left, title_bottom) = axis_title_space(spec);
    let probe = value_axis(
        lo,
        hi,
        false,
        spec.y_axis.min,
        spec.y_axis.max,
        &format,
        (0.0, 1.0),
    );
    let left = area.x + title_left + probe.width + 8.0;
    let names: Vec<String> = groups.iter().map(|(n, _)| n.clone()).collect();
    let band_probe = Band::new(groups.len(), (0.0, area.right() - left), 0.4);
    let plan = plan_category_labels(&names, band_probe.step);
    let plot = Rect {
        x: left,
        y: area.y + 4.0,
        w: area.right() - left,
        h: (area.h - plan.height - title_bottom - 4.0).max(10.0),
    };
    let axis = value_axis(
        lo,
        hi,
        false,
        spec.y_axis.min,
        spec.y_axis.max,
        &format,
        (plot.bottom(), plot.y),
    );
    draw_value_axis_left(&mut scene, &axis, plot, theme);
    let band = Band::new(groups.len(), (plot.x, plot.right()), 0.4);
    draw_category_labels(&mut scene, &names, &band, &plan, plot.bottom(), theme);
    let palette = colors(spec, theme, groups.len());
    for (i, (_, values)) in groups.iter().enumerate() {
        if values.is_empty() {
            continue;
        }
        let (q1, q2, q3) = (
            quantile(values, 0.25),
            quantile(values, 0.5),
            quantile(values, 0.75),
        );
        let iqr = q3 - q1;
        let (fence_lo, fence_hi) = (q1 - 1.5 * iqr, q3 + 1.5 * iqr);
        let inside: Vec<f64> = values
            .iter()
            .copied()
            .filter(|v| *v >= fence_lo && *v <= fence_hi)
            .collect();
        let wlo = inside.iter().copied().fold(q1, f64::min);
        let whi = inside.iter().copied().fold(q3, f64::max);
        let color = palette[i];
        let (x, w, cx) = (band.left(i), band.width, band.center(i));
        let stroke = Stroke::new(color, 1.5);
        scene.line(cx, axis.scale.map(whi), cx, axis.scale.map(q3), stroke);
        scene.line(cx, axis.scale.map(q1), cx, axis.scale.map(wlo), stroke);
        scene.line(
            cx - w / 4.0,
            axis.scale.map(whi),
            cx + w / 4.0,
            axis.scale.map(whi),
            stroke,
        );
        scene.line(
            cx - w / 4.0,
            axis.scale.map(wlo),
            cx + w / 4.0,
            axis.scale.map(wlo),
            stroke,
        );
        let (top, bottom) = (axis.scale.map(q3), axis.scale.map(q1));
        scene.items.push(super::scene::Item::Rect {
            x,
            y: top,
            w,
            h: (bottom - top).max(0.5),
            fill: Some(Fill::alpha(color, 0.22)),
            stroke: Some(stroke),
            radius: 0.0,
        });
        scene.line(
            x,
            axis.scale.map(q2),
            x + w,
            axis.scale.map(q2),
            Stroke::new(color, 2.5),
        );
        for v in values.iter().filter(|v| **v < fence_lo || **v > fence_hi) {
            scene.items.push(super::scene::Item::Circle {
                cx,
                cy: axis.scale.map(*v),
                r: 2.5,
                fill: None,
                stroke: Some(Stroke::new(color, 1.0)),
            });
        }
    }
    draw_reference_lines(&mut scene, spec, &axis, plot, theme);
    draw_axis_titles(&mut scene, spec, plot, area, theme);
    Ok(scene)
}

pub fn waterfall(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let spec = ctx.spec;
    let theme = options.theme;
    let data = ctx.categories();
    if data.is_empty() {
        return Err(ChartError::NoData);
    }
    let steps: Vec<f64> = data.series[0]
        .values
        .iter()
        .map(|v| v.unwrap_or(0.0))
        .collect();
    let mut labels = data.categories.clone();
    labels.push(options.labels.total.clone());
    let mut running = 0.0;
    let mut bars: Vec<(f64, f64)> = Vec::new();
    for v in &steps {
        bars.push((running, running + v));
        running += v;
    }
    bars.push((0.0, running));
    let lo = bars
        .iter()
        .flat_map(|(a, b)| [*a, *b])
        .fold(0.0f64, f64::min);
    let hi = bars
        .iter()
        .flat_map(|(a, b)| [*a, *b])
        .fold(0.0f64, f64::max);
    let (mut scene, area) = begin(spec, options);
    let format = value_format(ctx);
    let (title_left, title_bottom) = axis_title_space(spec);
    let probe = value_axis(
        lo,
        hi,
        true,
        spec.y_axis.min,
        spec.y_axis.max,
        &format,
        (0.0, 1.0),
    );
    let left = area.x + title_left + probe.width + 8.0;
    let band_probe = Band::new(labels.len(), (0.0, area.right() - left), 0.25);
    let plan = plan_category_labels(&labels, band_probe.step);
    let plot = Rect {
        x: left,
        y: area.y + 4.0,
        w: area.right() - left,
        h: (area.h - plan.height - title_bottom - 4.0).max(10.0),
    };
    let axis = value_axis(
        lo,
        hi,
        true,
        spec.y_axis.min,
        spec.y_axis.max,
        &format,
        (plot.bottom(), plot.y),
    );
    draw_value_axis_left(&mut scene, &axis, plot, theme);
    let band = Band::new(labels.len(), (plot.x, plot.right()), 0.25);
    draw_category_labels(&mut scene, &labels, &band, &plan, plot.bottom(), theme);
    let last = bars.len() - 1;
    for (i, (a, b)) in bars.iter().enumerate() {
        let color = if i == last {
            theme.accent
        } else if b >= a {
            theme.good
        } else {
            theme.critical
        };
        let (y0, y1) = (axis.scale.map(a.max(*b)), axis.scale.map(a.min(*b)));
        scene.rect(
            band.left(i),
            y0,
            band.width,
            (y1 - y0).max(0.5),
            Fill::solid(color),
        );
        if i < last {
            scene.hit_rect(
                band.left(i),
                y0,
                band.width,
                (y1 - y0).max(0.5),
                data.keys.get(i).and_then(Option::as_ref),
            );
        }
        if i < last {
            let y = axis.scale.map(*b);
            scene.line(
                band.left(i) + band.width,
                y,
                band.left(i + 1),
                y,
                Stroke::new(theme.text_dim, 1.0).dashed(2.0, 2.0),
            );
        }
        if spec.show_values && band.width >= 18.0 {
            let v = if i == last { *b } else { b - a };
            scene.text(
                band.center(i),
                y0 - 3.0,
                shown_value(ctx, v),
                TICK_SIZE,
                theme.text,
                Anchor::Middle,
            );
        }
    }
    draw_reference_lines(&mut scene, spec, &axis, plot, theme);
    draw_axis_titles(&mut scene, spec, plot, area, theme);
    Ok(scene)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_values_land_in_their_own_bins() {
        let (_, _, edges) = nice_ticks(0.0, 1.0, 10);
        assert_eq!(edges.len(), 11);
        assert_eq!(bin_index(&edges, 0.3), 3);
        assert_eq!(bin_index(&edges, 0.6), 6);
        assert_eq!(bin_index(&edges, 0.7), 7);
        assert_eq!(bin_index(&edges, 0.0), 0);
        assert_eq!(bin_index(&edges, 1.0), 9, "the last bin is closed");
        assert_eq!(bin_index(&edges, -5.0), 0);
        assert_eq!(bin_index(&edges, 0.29999), 2);
    }
}
