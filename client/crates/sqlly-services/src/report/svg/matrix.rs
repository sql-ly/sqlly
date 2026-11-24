//! Grid charts: heatmap, cohort retention, correlation matrix and calendar
//! heatmap.

use chrono::{Datelike, NaiveDate};

use super::ChartOptions;
use super::data::{ChartError, Ctx, Matrix};
use super::parts::{Rect, TICK_SIZE, begin};
use super::scale::{fit_text, text_width};
use super::scene::{Anchor, Fill, Item, Scene, Stroke, TextItem};
use crate::report::format::{ValueFormat, format_number};
use crate::report::theme::{ReportTheme, Rgb};

struct Layout {
    cells: Rect,
    cw: f32,
    ch: f32,
}

fn grid_layout(m: &Matrix, area: Rect, legend_h: f32) -> (Layout, bool) {
    let row_w = m
        .rows
        .iter()
        .map(|r| text_width(r, TICK_SIZE))
        .fold(0.0, f32::max)
        .min(area.w * 0.3)
        + 8.0;
    let cols = m.columns.len().max(1) as f32;
    let cw_flat = (area.w - row_w) / cols;
    let widest = m
        .columns
        .iter()
        .map(|c| text_width(c, TICK_SIZE))
        .fold(0.0, f32::max);
    let rotate = widest + 4.0 > cw_flat;
    let head = if rotate {
        widest.min(80.0) * 0.72 + TICK_SIZE
    } else {
        TICK_SIZE + 6.0
    };
    let cells = Rect {
        x: area.x + row_w,
        y: area.y + head,
        w: area.w - row_w,
        h: (area.h - head - legend_h).max(10.0),
    };
    let rows = m.rows.len().max(1) as f32;
    let layout = Layout {
        cw: cells.w / cols,
        ch: (cells.h / rows).min(40.0),
        cells,
    };
    (layout, rotate)
}

fn draw_grid(
    scene: &mut Scene,
    m: &Matrix,
    layout: &Layout,
    rotate: bool,
    theme: &ReportTheme,
    color: impl Fn(f64) -> Rgb,
    text: impl Fn(f64) -> Option<String>,
) {
    let Layout { cells, cw, ch } = *layout;
    for (c, label) in m.columns.iter().enumerate() {
        let x = cells.x + cw * (c as f32 + 0.5);
        if rotate {
            scene.push_text(TextItem {
                x: x + TICK_SIZE * 0.35,
                y: cells.y - 4.0,
                text: fit_text(label, TICK_SIZE, 80.0),
                size: TICK_SIZE,
                color: theme.text_dim,
                anchor: Anchor::Start,
                bold: false,
                rotate: -45.0,
            });
        } else {
            scene.text(
                x,
                cells.y - 5.0,
                fit_text(label, TICK_SIZE, cw - 2.0),
                TICK_SIZE,
                theme.text_dim,
                Anchor::Middle,
            );
        }
    }
    for (r, label) in m.rows.iter().enumerate() {
        let y = cells.y + ch * r as f32;
        scene.text(
            cells.x - 6.0,
            y + ch / 2.0 + TICK_SIZE * 0.35,
            fit_text(label, TICK_SIZE, cells.x - 10.0),
            TICK_SIZE,
            theme.text_dim,
            Anchor::End,
        );
        for (c, v) in m.values[r].iter().enumerate() {
            let x = cells.x + cw * c as f32;
            let Some(v) = v else {
                scene.items.push(Item::Rect {
                    x: x + 0.5,
                    y: y + 0.5,
                    w: (cw - 1.0).max(0.5),
                    h: (ch - 1.0).max(0.5),
                    fill: None,
                    stroke: Some(Stroke::new(theme.grid, 1.0)),
                    radius: 0.0,
                });
                continue;
            };
            let fill = color(*v);
            scene.rect(
                x + 0.5,
                y + 0.5,
                (cw - 1.0).max(0.5),
                (ch - 1.0).max(0.5),
                Fill::solid(fill),
            );
            if let Some(t) = text(*v)
                && cw >= text_width(&t, TICK_SIZE) + 4.0
                && ch >= TICK_SIZE + 2.0
            {
                scene.text(
                    x + cw / 2.0,
                    y + ch / 2.0 + TICK_SIZE * 0.35,
                    t,
                    TICK_SIZE,
                    theme.ink_on(fill),
                    Anchor::Middle,
                );
            }
        }
    }
}

/// A five-step ramp key under the grid with the low and high values.
fn ramp_key(
    scene: &mut Scene,
    area: Rect,
    theme: &ReportTheme,
    lo: &str,
    hi: &str,
    color: impl Fn(f32) -> Rgb,
) {
    let y = area.bottom() - 10.0;
    let lw = text_width(lo, TICK_SIZE);
    let mut x = area.x + lw + 6.0;
    scene.text(
        area.x,
        y + 8.5,
        lo.to_string(),
        TICK_SIZE,
        theme.text_dim,
        Anchor::Start,
    );
    for i in 0..5 {
        scene.rect(x, y, 18.0, 10.0, Fill::solid(color(i as f32 / 4.0)));
        x += 18.0;
    }
    scene.text(
        x + 6.0,
        y + 8.5,
        hi.to_string(),
        TICK_SIZE,
        theme.text_dim,
        Anchor::Start,
    );
}

fn value_format(ctx: &Ctx) -> ValueFormat {
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

pub fn heatmap(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let theme = options.theme;
    let m = ctx.matrix();
    let Some((lo, hi)) = m.range() else {
        return Err(ChartError::NoData);
    };
    let (mut scene, area) = begin(ctx.spec, options);
    let (layout, rotate) = grid_layout(&m, area, 18.0);
    let span = if hi > lo { hi - lo } else { 1.0 };
    let format = value_format(ctx);
    let show = ctx.spec.show_values;
    draw_grid(
        &mut scene,
        &m,
        &layout,
        rotate,
        theme,
        |v| theme.ramp_color(((v - lo) / span) as f32),
        |v| show.then(|| format_number(v, &format)),
    );
    ramp_key(
        &mut scene,
        area,
        theme,
        &format_number(lo, &format),
        &format_number(hi, &format),
        |t| theme.ramp_color(t),
    );
    Ok(scene)
}

/// Each cohort row as a share of its period-0 size. Retention is relative
/// to period 0; a cohort without one (or with nobody in it) has no rate
/// rather than a made-up 100%.
fn retention(t: Matrix) -> Matrix {
    let zero = t
        .columns
        .iter()
        .position(|label| label.trim().parse::<f64>().is_ok_and(|p| p == 0.0));
    Matrix {
        values: t
            .values
            .iter()
            .map(|row| {
                let base = zero
                    .and_then(|z| row.get(z).copied().flatten())
                    .filter(|v| *v != 0.0);
                row.iter()
                    .map(|v| match (v, base) {
                        (Some(v), Some(b)) => Some(v / b),
                        _ => None,
                    })
                    .collect()
            })
            .collect(),
        ..t
    }
}

pub fn cohort(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let theme = options.theme;
    let m = ctx.matrix();
    if m.range().is_none() {
        return Err(ChartError::NoData);
    }
    let t = Matrix {
        columns: m.rows.clone(),
        rows: m.columns.clone(),
        values: (0..m.columns.len())
            .map(|c| m.values.iter().map(|row| row[c]).collect())
            .collect(),
    };
    let retention = retention(t);
    let (mut scene, area) = begin(ctx.spec, options);
    let (layout, rotate) = grid_layout(&retention, area, 18.0);
    let pct = ValueFormat::percent(0);
    draw_grid(
        &mut scene,
        &retention,
        &layout,
        rotate,
        theme,
        |v| theme.ramp_color(v.clamp(0.0, 1.0) as f32),
        |v| Some(format_number(v, &pct)),
    );
    ramp_key(&mut scene, area, theme, "0%", "100%", |t| {
        theme.ramp_color(t)
    });
    Ok(scene)
}

pub fn correlation(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let theme = options.theme;
    let m = ctx.correlation();
    if m.range().is_none() {
        return Err(ChartError::NoData);
    }
    let (mut scene, area) = begin(ctx.spec, options);
    let (mut layout, rotate) = grid_layout(&m, area, 18.0);
    let side = (layout.cells.w / m.columns.len().max(1) as f32)
        .min(layout.cells.h / m.rows.len().max(1) as f32)
        .min(60.0);
    layout.cw = side;
    layout.ch = side;
    let fmt = ValueFormat::decimals(2);
    draw_grid(
        &mut scene,
        &m,
        &layout,
        rotate,
        theme,
        |v| theme.diverging_color(v as f32),
        |v| Some(format_number(v, &fmt)),
    );
    ramp_key(&mut scene, area, theme, "-1", "+1", |t| {
        theme.diverging_color(t * 2.0 - 1.0)
    });
    Ok(scene)
}

pub fn calendar(ctx: &Ctx, options: ChartOptions) -> Result<Scene, ChartError> {
    let theme = options.theme;
    let mut days = ctx.daily();
    if days.is_empty() {
        return Err(ChartError::NoData);
    }
    let last = days[days.len() - 1].0;
    let earliest = last
        .checked_sub_days(chrono::Days::new(53 * 7 * 3))
        .unwrap_or(last);
    days.retain(|(d, _)| *d >= earliest);
    let first = days[0].0;
    let start = first
        .checked_sub_days(chrono::Days::new(u64::from(
            first.weekday().num_days_from_monday(),
        )))
        .unwrap_or(first);
    let weeks = ((last - start).num_days() / 7 + 1) as usize;
    let (mut scene, area) = begin(ctx.spec, options);
    let head = TICK_SIZE + 6.0;
    let cell = ((area.w - 4.0) / weeks as f32)
        .min((area.h - head - 22.0) / 7.0)
        .clamp(2.0, 18.0);
    let origin = (area.x, area.y + head);
    let lo = days.iter().map(|d| d.1).fold(f64::INFINITY, f64::min);
    let hi = days.iter().map(|d| d.1).fold(f64::NEG_INFINITY, f64::max);
    let span = if hi > lo { hi - lo } else { 1.0 };
    let lookup: std::collections::HashMap<NaiveDate, f64> = days.iter().copied().collect();
    let mut last_month_x = f32::NEG_INFINITY;
    let mut day = start;
    while day <= last {
        let w = ((day - start).num_days() / 7) as f32;
        let d = day.weekday().num_days_from_monday() as f32;
        let (x, y) = (origin.0 + w * cell, origin.1 + d * cell);
        match lookup.get(&day) {
            Some(v) => scene.rect(
                x + 0.5,
                y + 0.5,
                cell - 1.0,
                cell - 1.0,
                Fill::solid(theme.ramp_color(((v - lo) / span) as f32)),
            ),
            None if day >= first => scene.items.push(Item::Rect {
                x: x + 0.5,
                y: y + 0.5,
                w: cell - 1.0,
                h: cell - 1.0,
                fill: Some(Fill::solid(theme.surface)),
                stroke: None,
                radius: 0.0,
            }),
            None => {}
        }
        if day.day() == 1 || day == first {
            let label = day.format("%Y-%m").to_string();
            if x - last_month_x >= text_width(&label, TICK_SIZE) + 6.0 {
                scene.text(
                    x,
                    area.y + TICK_SIZE,
                    label,
                    TICK_SIZE,
                    theme.text_dim,
                    Anchor::Start,
                );
                last_month_x = x;
            }
        }
        day = match day.succ_opt() {
            Some(d) => d,
            None => break,
        };
    }
    let format = value_format(ctx);
    ramp_key(
        &mut scene,
        area,
        theme,
        &format_number(lo, &format),
        &format_number(hi, &format),
        |t| theme.ramp_color(t),
    );
    Ok(scene)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_is_relative_to_period_zero() {
        let t = Matrix {
            columns: vec!["-1".into(), "0".into(), "1".into(), "2".into()],
            rows: vec!["2026-01".into(), "2026-02".into()],
            values: vec![
                vec![Some(5.0), Some(100.0), Some(50.0), None],
                vec![None, None, Some(40.0), Some(20.0)],
            ],
        };
        let r = retention(t);
        assert_eq!(r.values[0], vec![Some(0.05), Some(1.0), Some(0.5), None]);
        assert_eq!(
            r.values[1],
            vec![None, None, None, None],
            "no period 0, no rate"
        );
    }
}
