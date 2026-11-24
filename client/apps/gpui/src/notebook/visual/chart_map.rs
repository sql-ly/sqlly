//! A report [`ChartSpec`] as the grid's live chart: the rows prepared the
//! way the spec groups them (date grain, null handling, a series column
//! split into one measure column per value) and the index-based
//! [`ChartConfig`] that draws them. Specs that use something the live chart
//! cannot draw (reference lines, annotations, bands, stacking, horizontal
//! bars, axis ranges, bubble sizes, …) are drawn by the SVG renderer
//! instead; [`live_plan`] says why.

use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::{ChartAggregate, ChartConfig, ChartKind as GridKind, ChartOrder, MAX_SERIES};
use sqlly_services::notebook::cells::spec::category_label;
use sqlly_services::report::chart::{
    Aggregate, BarMode, ChartKind, ChartSpec, NullTreatment, Orientation, PaletteKind, SortBy,
};

/// A category × series cell folded so far: sum, count, min, max.
type Fold = (f64, u64, Option<f64>, Option<f64>);

/// Why a spec is drawn as a static chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticReason {
    /// The kind has no live renderer.
    Kind,
    /// A setting the live chart cannot draw.
    Setting,
    /// The spec names a column the output lacks, or lacks a measure.
    Columns,
}

/// What the live chart draws.
#[derive(Debug, Clone, PartialEq)]
pub struct LivePlan {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
    pub config: ChartConfig,
    /// The column cross-filters compare, with the grain it was grouped by.
    pub category_column: Option<String>,
}

pub fn grid_kind(kind: ChartKind) -> Option<GridKind> {
    Some(match kind {
        ChartKind::Bar => GridKind::Bar,
        ChartKind::Line => GridKind::Line,
        ChartKind::Area => GridKind::Area,
        ChartKind::Scatter => GridKind::Scatter,
        ChartKind::Histogram => GridKind::Histogram,
        ChartKind::Pie => GridKind::Pie,
        ChartKind::Donut => GridKind::Donut,
        _ => return None,
    })
}

fn grid_aggregate(aggregate: Aggregate) -> Option<ChartAggregate> {
    Some(match aggregate {
        Aggregate::None => ChartAggregate::None,
        Aggregate::Sum => ChartAggregate::Sum,
        Aggregate::Avg => ChartAggregate::Average,
        Aggregate::Min => ChartAggregate::Minimum,
        Aggregate::Max => ChartAggregate::Maximum,
        Aggregate::Count => ChartAggregate::Count,
        Aggregate::CountDistinct | Aggregate::Median => return None,
    })
}

fn index_of(columns: &[ResultColumn], name: &str) -> Option<usize> {
    columns.iter().position(|c| c.name == name).or_else(|| {
        columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    })
}

fn number(cell: &CellValue) -> Option<f64> {
    match cell {
        CellValue::I64(n) => Some(*n as f64),
        CellValue::F64(f) if f.is_finite() => Some(*f),
        CellValue::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

fn needs_static(spec: &ChartSpec) -> bool {
    !spec.reference_lines.is_empty()
        || !spec.annotations.is_empty()
        || spec.anomaly_band.is_some()
        || spec.bar_mode != BarMode::Grouped
        || spec.orientation != Orientation::Vertical
        || spec.palette != PaletteKind::Categorical
        || spec.x_axis.min.is_some()
        || spec.x_axis.max.is_some()
        || spec.y_axis.min.is_some()
        || spec.y_axis.max.is_some()
        || spec.size.is_some()
        || spec.color.is_some()
        || spec.show_values
        || spec.top_n.is_some_and(|t| t.other)
        || (spec.sort.by == SortBy::Category && spec.sort.descending)
        || (spec.sort.by == SortBy::Value && !spec.sort.descending)
        || spec
            .y
            .iter()
            .any(|y| y.aggregate.is_some_and(|a| a != spec.aggregate))
}

/// The live chart for `spec` over `columns`/`rows`, or why there is none.
pub fn live_plan(
    spec: &ChartSpec,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
) -> Result<LivePlan, StaticReason> {
    let kind = grid_kind(spec.kind).ok_or(StaticReason::Kind)?;
    if needs_static(spec) {
        return Err(StaticReason::Setting);
    }
    let aggregate = grid_aggregate(spec.aggregate).ok_or(StaticReason::Setting)?;
    let measures: Vec<usize> = spec
        .y
        .iter()
        .map(|y| index_of(columns, &y.column).ok_or(StaticReason::Columns))
        .collect::<Result<_, _>>()?;
    if measures.is_empty() {
        return Err(StaticReason::Columns);
    }
    let order = match spec.sort.by {
        SortBy::Natural => ChartOrder::Source,
        SortBy::Category => ChartOrder::CategoryAscending,
        SortBy::Value => ChartOrder::ValueDescending,
    };
    let top_n = spec
        .top_n
        .map(|t| t.n.max(1))
        .unwrap_or_else(|| ChartConfig::default().top_n);
    let base = ChartConfig {
        kind,
        label_column: None,
        value_columns: Vec::new(),
        aggregate,
        order,
        top_n,
        navigate_on_click: false,
    };
    let keep_measure = |row: &Vec<CellValue>, m: usize| -> Option<CellValue> {
        let cell = row.get(m).cloned().unwrap_or(CellValue::Null);
        match (number(&cell), spec.nulls) {
            (Some(v), _) => Some(CellValue::F64(v)),
            (None, NullTreatment::Zero) => Some(CellValue::F64(0.0)),
            (None, NullTreatment::Skip) => None,
            (None, NullTreatment::Gap) => Some(CellValue::Null),
        }
    };
    if kind == GridKind::Histogram {
        let m = measures[0];
        let out_rows = rows
            .iter()
            .filter_map(|row| keep_measure(row, m).map(|v| vec![v]))
            .collect();
        return Ok(LivePlan {
            columns: vec![columns[m].clone()],
            rows: out_rows,
            config: ChartConfig {
                value_columns: vec![0],
                ..base
            },
            category_column: None,
        });
    }
    let x = spec
        .x
        .as_ref()
        .ok_or(StaticReason::Columns)
        .and_then(|e| index_of(columns, &e.column).ok_or(StaticReason::Columns))?;
    let label = |row: &Vec<CellValue>| {
        category_label(row.get(x).unwrap_or(&CellValue::Null), spec.date_grain)
    };
    let x_column = ResultColumn {
        name: columns[x].name.clone(),
        data_type: if kind == GridKind::Scatter && spec.date_grain.is_none() {
            columns[x].data_type.clone()
        } else {
            "varchar".into()
        },
        nullable: true,
    };
    let x_cell = |row: &Vec<CellValue>| {
        if kind == GridKind::Scatter && spec.date_grain.is_none() {
            row.get(x).cloned().unwrap_or(CellValue::Null)
        } else {
            CellValue::String(label(row))
        }
    };
    let temporal_x = spec.date_grain.is_some();
    let mut ordered: Vec<&Vec<CellValue>> = rows.iter().collect();
    if temporal_x && spec.sort.by == SortBy::Natural {
        ordered.sort_by_key(|row| label(row));
    }
    let Some(series) = spec.series.as_ref() else {
        let mut out_columns = vec![x_column];
        out_columns.extend(measures.iter().map(|&m| ResultColumn {
            name: columns[m].name.clone(),
            data_type: "double".into(),
            nullable: true,
        }));
        let out_rows = ordered
            .iter()
            .filter_map(|row| {
                let mut out = vec![x_cell(row)];
                for &m in &measures {
                    out.push(keep_measure(row, m)?);
                }
                Some(out)
            })
            .collect();
        return Ok(LivePlan {
            columns: out_columns,
            rows: out_rows,
            config: ChartConfig {
                label_column: Some(0),
                value_columns: (1..=measures.len()).collect(),
                ..base
            },
            category_column: Some(columns[x].name.clone()),
        });
    };
    // One measure split by the series column: one column per series value,
    // aggregated per category here (the live chart sums what is left).
    if measures.len() != 1 || matches!(kind, GridKind::Pie | GridKind::Donut | GridKind::Scatter) {
        return Err(StaticReason::Setting);
    }
    let s = index_of(columns, &series.column).ok_or(StaticReason::Columns)?;
    let m = measures[0];
    let mut names: Vec<String> = Vec::new();
    let mut categories: Vec<String> = Vec::new();
    let mut cells: std::collections::HashMap<(usize, usize), Fold> =
        std::collections::HashMap::new();
    for row in &ordered {
        let name = category_label(row.get(s).unwrap_or(&CellValue::Null), None);
        let si = match names.iter().position(|n| *n == name) {
            Some(i) => i,
            None if names.len() < MAX_SERIES => {
                names.push(name);
                names.len() - 1
            }
            None => return Err(StaticReason::Setting),
        };
        let category = label(row);
        let ci = match categories.iter().position(|c| *c == category) {
            Some(i) => i,
            None => {
                categories.push(category);
                categories.len() - 1
            }
        };
        let Some(value) = row.get(m).and_then(number) else {
            continue;
        };
        let entry = cells.entry((ci, si)).or_insert((0.0, 0, None, None));
        entry.0 += value;
        entry.1 += 1;
        entry.2 = Some(entry.2.map_or(value, |v: f64| v.min(value)));
        entry.3 = Some(entry.3.map_or(value, |v: f64| v.max(value)));
    }
    let folded = |cell: Option<&Fold>| -> CellValue {
        let Some(&(sum, count, min, max)) = cell else {
            return match spec.nulls {
                NullTreatment::Zero => CellValue::F64(0.0),
                _ => CellValue::Null,
            };
        };
        let v = match spec.aggregate {
            Aggregate::Avg => sum / count.max(1) as f64,
            Aggregate::Min => min.unwrap_or(0.0),
            Aggregate::Max => max.unwrap_or(0.0),
            Aggregate::Count => count as f64,
            _ => sum,
        };
        CellValue::F64(v)
    };
    let mut out_columns = vec![x_column];
    out_columns.extend(names.iter().map(|n| ResultColumn {
        name: n.clone(),
        data_type: "double".into(),
        nullable: true,
    }));
    let out_rows = categories
        .iter()
        .enumerate()
        .map(|(ci, category)| {
            let mut out = vec![CellValue::String(category.clone())];
            out.extend((0..names.len()).map(|si| folded(cells.get(&(ci, si)))));
            out
        })
        .collect();
    Ok(LivePlan {
        columns: out_columns,
        rows: out_rows,
        config: ChartConfig {
            label_column: Some(0),
            value_columns: (1..=names.len()).collect(),
            aggregate: ChartAggregate::None,
            ..base
        },
        category_column: Some(columns[x].name.clone()),
    })
}

/// The category under a click at `(x, y)` in a `width`×`height` plot, for
/// the live chart's layout: categories share the plot width evenly (after
/// the value-axis gutter) and pie slices sweep clockwise from twelve
/// o'clock. `values` are the first series' values, for slices.
/// Where a click landed in a plot of `width`×`height`, with the value
/// axis taking `gutter` on the left.
#[derive(Debug, Clone, Copy)]
pub struct PlotClick {
    pub width: f32,
    pub height: f32,
    pub x: f32,
    pub y: f32,
    pub gutter: f32,
}

pub fn hit_category(
    kind: GridKind,
    categories: usize,
    values: &[Option<f64>],
    click: PlotClick,
) -> Option<usize> {
    let PlotClick {
        width,
        height,
        x,
        y,
        gutter,
    } = click;
    if categories == 0 || width <= 0.0 || height <= 0.0 {
        return None;
    }
    if matches!(kind, GridKind::Pie | GridKind::Donut) {
        let total: f64 = values.iter().flatten().filter(|v| **v > 0.0).sum();
        if total <= 0.0 {
            return None;
        }
        let (cx, cy) = (width / 2.0, height / 2.0);
        let outer = (width.min(height) * 0.4).max(1.0);
        let inner = if kind == GridKind::Donut {
            outer * 0.56
        } else {
            0.0
        };
        let (dx, dy) = (x - cx, y - cy);
        let r = dx.hypot(dy);
        if r > outer || r < inner {
            return None;
        }
        let tau = std::f32::consts::TAU;
        let rel = ((dy.atan2(dx) + std::f32::consts::FRAC_PI_2) % tau + tau) % tau;
        let mut angle = 0f32;
        for (index, value) in values.iter().enumerate() {
            let value = value.unwrap_or(0.0).max(0.0);
            if value == 0.0 {
                continue;
            }
            angle += (value / total) as f32 * tau;
            if rel <= angle {
                return Some(index);
            }
        }
        return None;
    }
    let plot = width - gutter - 10.0;
    if plot < 20.0 || x < gutter || x > gutter + plot {
        return None;
    }
    let step = plot / categories as f32;
    Some((((x - gutter) / step).floor() as usize).min(categories - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::report::chart::{DateGrain, ReferenceLine, Sort, TopN};

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        }
    }

    fn sales() -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
        let columns = vec![
            col("day", "date"),
            col("region", "varchar"),
            col("revenue", "float"),
            col("orders", "int"),
        ];
        let rows = vec![
            vec![
                CellValue::String("2026-01-03".into()),
                CellValue::String("West".into()),
                CellValue::F64(10.0),
                CellValue::I64(1),
            ],
            vec![
                CellValue::String("2026-01-20".into()),
                CellValue::String("East".into()),
                CellValue::F64(5.0),
                CellValue::Null,
            ],
            vec![
                CellValue::String("2026-02-02".into()),
                CellValue::String("West".into()),
                CellValue::F64(7.0),
                CellValue::I64(3),
            ],
        ];
        (columns, rows)
    }

    #[test]
    fn the_seven_live_kinds_map_onto_grid_configs() {
        let (columns, rows) = sales();
        for (kind, grid) in [
            (ChartKind::Bar, GridKind::Bar),
            (ChartKind::Line, GridKind::Line),
            (ChartKind::Area, GridKind::Area),
            (ChartKind::Scatter, GridKind::Scatter),
            (ChartKind::Pie, GridKind::Pie),
            (ChartKind::Donut, GridKind::Donut),
        ] {
            let spec = ChartSpec::new(kind).x("region").y("revenue");
            let plan = live_plan(&spec, &columns, &rows).unwrap();
            assert_eq!(plan.config.kind, grid);
            assert_eq!(plan.config.label_column, Some(0));
            assert_eq!(plan.config.value_columns, vec![1]);
            assert!(!plan.config.navigate_on_click);
            assert_eq!(plan.rows.len(), 3);
            assert_eq!(plan.category_column.as_deref(), Some("region"));
        }
        let histogram = live_plan(
            &ChartSpec::new(ChartKind::Histogram).y("revenue"),
            &columns,
            &rows,
        )
        .unwrap();
        assert_eq!(histogram.config.kind, GridKind::Histogram);
        assert_eq!(histogram.config.label_column, None);
        assert_eq!(histogram.config.value_columns, vec![0]);
        assert_eq!(histogram.columns[0].name, "revenue");
    }

    #[test]
    fn aggregates_order_and_top_n_carry_over() {
        let (columns, rows) = sales();
        let mut spec = ChartSpec::new(ChartKind::Bar).x("region").y("revenue");
        spec.aggregate = Aggregate::Avg;
        spec.sort = Sort {
            by: SortBy::Value,
            descending: true,
        };
        spec.top_n = Some(TopN { n: 5, other: false });
        let plan = live_plan(&spec, &columns, &rows).unwrap();
        assert_eq!(plan.config.aggregate, ChartAggregate::Average);
        assert_eq!(plan.config.order, ChartOrder::ValueDescending);
        assert_eq!(plan.config.top_n, 5);
        spec.sort.by = SortBy::Category;
        spec.sort.descending = false;
        assert_eq!(
            live_plan(&spec, &columns, &rows).unwrap().config.order,
            ChartOrder::CategoryAscending
        );
        spec.aggregate = Aggregate::Median;
        assert_eq!(
            live_plan(&spec, &columns, &rows),
            Err(StaticReason::Setting)
        );
    }

    #[test]
    fn date_grain_nulls_and_series_are_prepared() {
        let (columns, rows) = sales();
        let mut spec = ChartSpec::new(ChartKind::Line).x("day").y("orders");
        spec.date_grain = Some(DateGrain::Month);
        spec.nulls = NullTreatment::Skip;
        let plan = live_plan(&spec, &columns, &rows).unwrap();
        assert_eq!(plan.rows.len(), 2, "the NULL measure row is skipped");
        let january = category_label(&rows[0][0], Some(DateGrain::Month));
        assert_eq!(plan.rows[0][0], CellValue::String(january));

        spec.nulls = NullTreatment::Zero;
        let plan = live_plan(&spec, &columns, &rows).unwrap();
        assert_eq!(plan.rows[1][1], CellValue::F64(0.0));

        let mut split = ChartSpec::new(ChartKind::Bar)
            .x("day")
            .y("revenue")
            .series("region");
        split.date_grain = Some(DateGrain::Month);
        split.aggregate = Aggregate::Sum;
        let plan = live_plan(&split, &columns, &rows).unwrap();
        let names: Vec<&str> = plan.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["day", "West", "East"]);
        assert_eq!(plan.rows.len(), 2);
        assert_eq!(plan.rows[0][1], CellValue::F64(10.0));
        assert_eq!(plan.rows[0][2], CellValue::F64(5.0));
        assert_eq!(plan.rows[1][2], CellValue::Null);
        assert_eq!(plan.config.aggregate, ChartAggregate::None);
    }

    #[test]
    fn static_only_settings_and_kinds_fall_back_to_svg() {
        let (columns, rows) = sales();
        let mut spec = ChartSpec::new(ChartKind::Bar).x("region").y("revenue");
        spec.reference_lines.push(ReferenceLine {
            value: 6.0,
            label: None,
            status: None,
        });
        assert_eq!(
            live_plan(&spec, &columns, &rows),
            Err(StaticReason::Setting)
        );
        let heat = ChartSpec::new(ChartKind::Heatmap).x("region").y("revenue");
        assert_eq!(live_plan(&heat, &columns, &rows), Err(StaticReason::Kind));
        let missing = ChartSpec::new(ChartKind::Bar).x("nope").y("revenue");
        assert_eq!(
            live_plan(&missing, &columns, &rows),
            Err(StaticReason::Columns)
        );
    }

    fn click(width: f32, height: f32, x: f32, y: f32, gutter: f32) -> PlotClick {
        PlotClick {
            width,
            height,
            x,
            y,
            gutter,
        }
    }

    #[test]
    fn clicks_resolve_to_categories_and_slices() {
        // Bars: four categories over a 400 px plot after a 40 px gutter.
        assert_eq!(
            hit_category(
                GridKind::Bar,
                4,
                &[],
                click(450.0, 300.0, 45.0, 100.0, 40.0)
            ),
            Some(0)
        );
        assert_eq!(
            hit_category(
                GridKind::Bar,
                4,
                &[],
                click(450.0, 300.0, 439.0, 100.0, 40.0)
            ),
            Some(3)
        );
        assert_eq!(
            hit_category(
                GridKind::Bar,
                4,
                &[],
                click(450.0, 300.0, 10.0, 100.0, 40.0)
            ),
            None
        );
        // Pie: two equal slices; right half is the first, left the second.
        let values = [Some(1.0), Some(1.0)];
        assert_eq!(
            hit_category(
                GridKind::Pie,
                2,
                &values,
                click(200.0, 200.0, 140.0, 100.0, 0.0)
            ),
            Some(0)
        );
        assert_eq!(
            hit_category(
                GridKind::Pie,
                2,
                &values,
                click(200.0, 200.0, 60.0, 100.0, 0.0)
            ),
            Some(1)
        );
        assert_eq!(
            hit_category(
                GridKind::Donut,
                2,
                &values,
                click(200.0, 200.0, 100.0, 100.0, 0.0)
            ),
            None
        );
    }
}
