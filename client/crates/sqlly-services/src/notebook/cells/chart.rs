//! Chart cells: a [`ChartSpec`] over another cell's output, plus the cell
//! settings around it (input, linked cross-filter, anomaly band, height).
//!
//! ```json
//! {"input": "c_orders", "follows": "c_region_chart", "height": 320,
//!  "chart": {"kind": "line", "x": {"column": "day"}, "y": [{"column": "revenue"}],
//!            "date_grain": "week", "aggregate": "sum"},
//!  "anomaly": {"window": 7, "k": 3.0}}
//! ```

use serde::{Deserialize, Serialize};
use sqlly_core::{CellValue, ResultColumn};

use super::analytics_completeness;
use super::spec::SpecExtra;
use crate::analytics::OutputView;
use crate::analytics::kpi::Aggregate as SeriesAggregate;
use crate::analytics::profile::{ProfileOptions, profile_output};
use crate::analytics::suggest::{
    ChartKindName, ChartSuggestion, SortHint, SuggestOptions, suggest_charts,
};
use crate::analytics::time::TimeGrain;
use crate::analytics::timeseries::{AnomalyOptions, Seasonality, anomaly_bands, time_series};
use crate::notebook::output::Completeness;
use crate::report::chart::{
    Aggregate, AnomalyBand, ChartKind, ChartSpec, DateGrain, Encoding, Sort, SortBy, TopN,
};
use crate::report::doc::{TableBlock, TableColumn};

/// Suggestions shown in the chart editor.
pub const SUGGESTIONS_SHOWN: usize = 3;
/// Default chart height in the notebook, in logical pixels.
pub const DEFAULT_HEIGHT: f32 = 320.0;
/// Most points an anomaly band is computed over.
pub const MAX_BAND_POINTS: usize = 366;
/// Name of the computed band columns added to a chart's table.
pub const BAND_LOWER: &str = "expected_low";
pub const BAND_UPPER: &str = "expected_high";

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ChartCellSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// A chart cell whose clicks filter this chart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follows: Option<String>,
    #[serde(default)]
    pub chart: ChartSpec,
    /// Draw an expected range from the series' own history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anomaly: Option<AnomalyConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<f32>,
    #[serde(flatten)]
    pub extra: SpecExtra,
}

/// Rolling mean ± k·sd band settings (`analytics::timeseries`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AnomalyConfig {
    #[serde(default = "default_window")]
    pub window: usize,
    #[serde(default = "default_k")]
    pub k: f64,
    #[serde(default)]
    pub same_weekday: bool,
}

fn default_window() -> usize {
    7
}

fn default_k() -> f64 {
    3.0
}

impl Default for AnomalyConfig {
    fn default() -> Self {
        Self {
            window: default_window(),
            k: default_k(),
            same_weekday: false,
        }
    }
}

impl ChartCellSpec {
    pub fn height(&self) -> f32 {
        self.height.unwrap_or(DEFAULT_HEIGHT).clamp(160.0, 900.0)
    }
}

/// The report table for a cell output: its columns typed by the engine's
/// type names, its rows, and how complete they are.
pub fn table_block(
    columns: &[ResultColumn],
    rows: Vec<Vec<CellValue>>,
    completeness: Completeness,
    total_rows: u64,
) -> TableBlock {
    let mut table = TableBlock::new(
        columns
            .iter()
            .map(|c| TableColumn::new(c.name.clone(), c.data_type.clone()))
            .collect(),
        rows,
    );
    table.completeness = completeness.into();
    if total_rows > table.rows.len() as u64 {
        table.total_rows = Some(total_rows);
    }
    table
}

/// The best chart kinds for an output, best first.
pub fn suggestions(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    completeness: Completeness,
    count: usize,
) -> Vec<ChartSuggestion> {
    let view = OutputView::new(
        columns,
        rows,
        analytics_completeness(completeness, rows.len()),
    );
    let profile = profile_output(&view, &ProfileOptions::default());
    let options = SuggestOptions {
        max_suggestions: count,
        ..SuggestOptions::default()
    };
    let mut out = suggest_charts(&profile, &options);
    out.truncate(count);
    out
}

fn date_grain(grain: TimeGrain) -> DateGrain {
    match grain {
        TimeGrain::Second | TimeGrain::Minute | TimeGrain::Hour => DateGrain::Hour,
        TimeGrain::Day => DateGrain::Day,
        TimeGrain::Week => DateGrain::Week,
        TimeGrain::Month => DateGrain::Month,
        TimeGrain::Quarter => DateGrain::Quarter,
        TimeGrain::Year => DateGrain::Year,
    }
}

fn time_grain(grain: DateGrain) -> TimeGrain {
    match grain {
        DateGrain::Hour => TimeGrain::Hour,
        DateGrain::Day => TimeGrain::Day,
        DateGrain::Week => TimeGrain::Week,
        DateGrain::Month => TimeGrain::Month,
        DateGrain::Quarter => TimeGrain::Quarter,
        DateGrain::Year => TimeGrain::Year,
    }
}

/// The chart a suggestion describes, mapped onto the report's encodings.
pub fn from_suggestion(suggestion: &ChartSuggestion) -> ChartSpec {
    let e = &suggestion.encodings;
    let kind = match suggestion.kind {
        ChartKindName::Scatter if e.size.is_some() => ChartKind::Bubble,
        other => ChartKind::from_name(other.as_str()).unwrap_or_default(),
    };
    let mut spec = ChartSpec::new(kind);
    let measures: Vec<String> = match &e.value {
        Some(value) => vec![value.clone()],
        None => e.y.clone(),
    };
    match kind {
        ChartKind::Histogram => {
            let measure = e.x.clone().or_else(|| e.y.first().cloned());
            spec.y = measure.into_iter().map(Encoding::new).collect();
        }
        ChartKind::Heatmap => {
            spec.x = e.x.clone().map(Encoding::new);
            spec.series = e.y.first().cloned().map(Encoding::new);
            spec.y = e.value.clone().into_iter().map(Encoding::new).collect();
            spec.aggregate = Aggregate::Sum;
        }
        ChartKind::CohortRetention => {
            spec.x = e.y.first().cloned().map(Encoding::new);
            spec.series = e.x.clone().map(Encoding::new);
            spec.y = e.value.clone().into_iter().map(Encoding::new).collect();
        }
        ChartKind::CorrelationMatrix => {
            spec.y = e.y.iter().cloned().map(Encoding::new).collect();
        }
        _ => {
            spec.x =
                e.x.clone()
                    .or_else(|| e.category.clone())
                    .map(Encoding::new);
            spec.y = measures.into_iter().map(Encoding::new).collect();
            match kind {
                ChartKind::Scatter | ChartKind::Bubble => {
                    spec.color = e.series.clone().map(Encoding::new);
                    spec.size = e.size.clone().map(Encoding::new);
                }
                ChartKind::Pie | ChartKind::Donut | ChartKind::Funnel | ChartKind::Waterfall => {}
                _ => spec.series = e.series.clone().map(Encoding::new),
            }
        }
    }
    if let Some(grain) = suggestion.time_grain {
        spec.date_grain = Some(date_grain(grain));
        if spec.aggregate == Aggregate::None && kind != ChartKind::CalendarHeatmap {
            spec.aggregate = Aggregate::Sum;
        }
    }
    if let Some(n) = suggestion.top_n {
        spec.top_n = Some(TopN { n, other: true });
    }
    spec.sort = match suggestion.sort {
        Some(SortHint::ValueDescending) => Sort {
            by: SortBy::Value,
            descending: true,
        },
        Some(SortHint::XAscending) | None => Sort::default(),
    };
    spec
}

fn series_aggregate(aggregate: Aggregate) -> SeriesAggregate {
    match aggregate {
        Aggregate::None | Aggregate::Sum => SeriesAggregate::Sum,
        Aggregate::Avg => SeriesAggregate::Avg,
        Aggregate::Count => SeriesAggregate::Count,
        Aggregate::CountDistinct => SeriesAggregate::CountDistinct,
        Aggregate::Min => SeriesAggregate::Min,
        Aggregate::Max => SeriesAggregate::Max,
        Aggregate::Median => SeriesAggregate::Median,
    }
}

/// The chart and table to draw for `spec` over an output. With an anomaly
/// band configured on a line or area chart, the series is bucketed by its
/// date grain and two band columns are added from the analytics module's
/// rolling mean ± k·sd; otherwise the output's rows are drawn as they are.
pub fn prepare(
    spec: &ChartCellSpec,
    columns: &[ResultColumn],
    rows: Vec<Vec<CellValue>>,
    completeness: Completeness,
    total_rows: u64,
) -> (ChartSpec, TableBlock) {
    let chart = spec.chart.clone();
    let banded = spec.anomaly.filter(|_| {
        matches!(chart.kind, ChartKind::Line | ChartKind::Area)
            && chart.x.is_some()
            && chart.y.len() == 1
    });
    let Some(anomaly) = banded else {
        return (chart, table_block(columns, rows, completeness, total_rows));
    };
    let x = chart.x.clone().unwrap_or_default();
    let y = chart.y[0].clone();
    let view = OutputView::new(
        columns,
        &rows,
        analytics_completeness(completeness, rows.len()),
    );
    let series = time_series(
        &view,
        &x.column,
        Some(&y.column),
        series_aggregate(y.aggregate.unwrap_or(chart.aggregate)),
        chart.date_grain.map(time_grain),
        MAX_BAND_POINTS,
    );
    let Ok(series) = series else {
        return (chart, table_block(columns, rows, completeness, total_rows));
    };
    let options = AnomalyOptions {
        window: anomaly.window.max(1),
        k: anomaly.k,
        min_history: 3.min(anomaly.window.max(1)),
        seasonality: if anomaly.same_weekday {
            Seasonality::SameWeekday
        } else {
            Seasonality::None
        },
    };
    let bands = anomaly_bands(&series.points, &options);
    let number = |v: Option<f64>| v.map_or(CellValue::Null, CellValue::F64);
    let band_rows = bands
        .iter()
        .map(|p| {
            vec![
                CellValue::String(p.label.clone()),
                CellValue::F64(p.value),
                number(p.lower),
                number(p.upper),
            ]
        })
        .collect();
    let band_columns = [
        ResultColumn {
            name: x.column.clone(),
            data_type: "varchar".into(),
            nullable: false,
        },
        ResultColumn {
            name: y.column.clone(),
            data_type: "double".into(),
            nullable: true,
        },
        ResultColumn {
            name: BAND_LOWER.into(),
            data_type: "double".into(),
            nullable: true,
        },
        ResultColumn {
            name: BAND_UPPER.into(),
            data_type: "double".into(),
            nullable: true,
        },
    ];
    let mut banded = chart;
    banded.date_grain = None;
    banded.aggregate = Aggregate::None;
    banded.y = vec![Encoding {
        aggregate: None,
        ..y
    }];
    banded.series = None;
    banded.anomaly_band = Some(AnomalyBand {
        lower: BAND_LOWER.into(),
        upper: BAND_UPPER.into(),
    });
    let rows_in = bands.len() as u64;
    (
        banded,
        table_block(&band_columns, band_rows, completeness, rows_in),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::suggest::{Encodings, SuggestionReason};
    use crate::report::svg::{ChartOptions, render_chart};
    use crate::report::{labels::ReportLabels, theme};

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        }
    }

    fn daily() -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
        let columns = vec![col("day", "date"), col("revenue", "decimal(12,2)")];
        let rows = (1..=28)
            .map(|d| {
                let value = if d == 20 {
                    900.0
                } else {
                    100.0 + (d % 3) as f64
                };
                vec![
                    CellValue::String(format!("2026-02-{d:02}")),
                    CellValue::F64(value),
                ]
            })
            .collect();
        (columns, rows)
    }

    #[test]
    fn specs_round_trip_with_unknown_fields() {
        let json = serde_json::json!({
            "input": "c_orders",
            "follows": "c_other",
            "height": 280.0,
            "anomaly": {"window": 5, "k": 2.5, "same_weekday": false},
            "chart": {"kind": "waterfall", "x": {"column": "step"}, "y": [{"column": "delta"}], "show_values": true},
            "future_setting": [1, 2]
        });
        let spec: ChartCellSpec = super::super::spec::read(&json);
        assert_eq!(spec.input.as_deref(), Some("c_orders"));
        assert_eq!(spec.chart.kind, ChartKind::Waterfall);
        assert_eq!(spec.anomaly.unwrap().window, 5);
        let written = super::super::spec::write(&spec);
        assert_eq!(written["future_setting"], serde_json::json!([1, 2]));
        assert_eq!(super::super::spec::read::<ChartCellSpec>(&written), spec);
        assert!(spec.chart.show_values);
        assert_eq!(spec.height(), 280.0);
    }

    #[test]
    fn suggestions_map_onto_renderable_specs() {
        let (columns, rows) = daily();
        let found = suggestions(&columns, &rows, Completeness::Complete, SUGGESTIONS_SHOWN);
        assert!(!found.is_empty() && found.len() <= SUGGESTIONS_SHOWN);
        let first = from_suggestion(&found[0]);
        assert_eq!(first.x.as_ref().map(|e| e.column.as_str()), Some("day"));
        let table = table_block(&columns, rows.clone(), Completeness::Complete, 28);
        assert!(first.validate(&table.columns).is_ok(), "{first:?}");

        let heat = from_suggestion(&ChartSuggestion {
            kind: ChartKindName::Heatmap,
            score: 0.7,
            reason: SuggestionReason::Matrix,
            encodings: Encodings {
                x: Some("region".into()),
                y: vec!["product".into()],
                value: Some("units".into()),
                ..Encodings::default()
            },
            time_grain: None,
            top_n: Some(20),
            sort: Some(SortHint::ValueDescending),
            warnings: Vec::new(),
        });
        assert_eq!(heat.kind, ChartKind::Heatmap);
        assert_eq!(heat.series.unwrap().column, "product");
        assert_eq!(heat.y[0].column, "units");
        assert_eq!(heat.top_n, Some(TopN { n: 20, other: true }));
        assert_eq!(heat.sort.by, SortBy::Value);
    }

    #[test]
    fn anomaly_bands_add_expected_range_columns() {
        let (columns, rows) = daily();
        let spec = ChartCellSpec {
            chart: ChartSpec::new(ChartKind::Line).x("day").y("revenue"),
            anomaly: Some(AnomalyConfig::default()),
            ..ChartCellSpec::default()
        };
        let (chart, table) = prepare(&spec, &columns, rows, Completeness::Complete, 28);
        assert_eq!(table.columns.len(), 4);
        assert_eq!(table.columns[2].name, BAND_LOWER);
        assert!(chart.anomaly_band.is_some());
        assert!(chart.validate(&table.columns).is_ok());
        let labels = ReportLabels::default();
        let drawn = render_chart(&chart, &table, ChartOptions::new(&theme::LIGHT, &labels));
        assert!(drawn.error.is_none(), "{:?}", drawn.error);
    }

    #[test]
    fn truncated_outputs_keep_their_completeness() {
        let (columns, rows) = daily();
        let spec = ChartCellSpec {
            chart: ChartSpec::new(ChartKind::Bar).x("day").y("revenue"),
            ..ChartCellSpec::default()
        };
        let (_, table) = prepare(
            &spec,
            &columns,
            rows,
            Completeness::Truncated { cap: 28 },
            1_000,
        );
        assert_eq!(table.total_rows, Some(1_000));
        assert_eq!(
            table.completeness,
            crate::report::doc::Completeness::Truncated { cap: 28 }
        );
    }
}
