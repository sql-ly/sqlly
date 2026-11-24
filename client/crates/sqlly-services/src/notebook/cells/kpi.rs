//! KPI cells: one or more KPI tiles (a scorecard row) over another cell's
//! output, each evaluated by [`crate::analytics::kpi`]. The CLI runner
//! evaluates the same spec headlessly.
//!
//! ```json
//! {"input": "c_daily", "baseline": "c_last_year", "inputs": ["c_last_year"],
//!  "tiles": [{"label": "Revenue",
//!             "kpi": {"value": {"kind": "aggregate", "column": "revenue", "aggregate": "sum"},
//!                     "comparison": {"kind": "previous_period", "time_column": "day", "grain": "month"},
//!                     "direction": "higher_is_better",
//!                     "thresholds": {"basis": "delta_percent", "warn": -5, "critical": -20},
//!                     "sparkline": {"time_column": "day", "max_points": 60}}}]}
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlly_core::{CellValue, ResultColumn};

use super::spec::SpecExtra;
use crate::analytics::OutputView;
use crate::analytics::kpi::{
    Aggregate, Comparison, Direction, FormatHint, KpiError, KpiResult, KpiSpec, KpiStatus,
    NumberStyle, ValueSource, evaluate_kpi, format_value,
};
use crate::report::doc::{KpiBlock, KpiComparison, Status};
use crate::report::format::{NumberStyle as ReportStyle, ValueFormat};

/// Most tiles one KPI cell shows side by side.
pub const MAX_TILES: usize = 8;

/// A `baseline` that compares with the input's output in the previous
/// recorded run instead of another cell (command-line runs).
pub const PREVIOUS_RUN: &str = "previous_run";

/// Keys of the cell around an inline (single, untitled) KPI.
const CELL_KEYS: [&str; 4] = ["input", "inputs", "baseline", "follows"];

/// Why [`KpiCellSpec::parse`] refused a spec. `reason` is the JSON
/// reader's own wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KpiSpecProblem {
    NoInput,
    Invalid { reason: String },
    NoTiles,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct KpiCellSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// The cell whose output a `baseline` comparison reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
    /// Extra inputs for the dependency graph (the baseline cell).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    /// A chart cell whose clicks filter these KPIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follows: Option<String>,
    #[serde(default)]
    pub tiles: Vec<KpiTile>,
    #[serde(flatten)]
    pub extra: SpecExtra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KpiTile {
    pub label: String,
    pub kpi: KpiSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
}

impl KpiTile {
    /// A tile summing `column`, or counting rows when there is none.
    pub fn new(label: impl Into<String>, column: Option<&str>) -> Self {
        let value = match column {
            Some(column) => ValueSource::Aggregate {
                column: column.to_string(),
                aggregate: Aggregate::Sum,
            },
            None => ValueSource::RowCount,
        };
        Self {
            label: label.into(),
            kpi: KpiSpec {
                value,
                comparison: None,
                direction: Direction::HigherIsBetter,
                thresholds: None,
                sparkline: None,
                format: None,
                allow_incomplete: false,
            },
            caption: None,
        }
    }
}

impl KpiCellSpec {
    /// Read a spec strictly, for a headless runner. Besides the `tiles`
    /// list, a single KPI may be written inline next to `input` (the shape
    /// command-line notebooks use); it becomes one tile with no label.
    pub fn parse(spec: &Value) -> Result<Self, KpiSpecProblem> {
        let input = super::input_of(spec);
        if input.is_empty() {
            return Err(KpiSpecProblem::NoInput);
        }
        let invalid = |e: serde_json::Error| KpiSpecProblem::Invalid {
            reason: e.to_string(),
        };
        let mut parsed = if spec.get("tiles").is_some() {
            let mut copy = spec.clone();
            if let Value::Object(map) = &mut copy {
                map.remove("input");
            }
            serde_json::from_value::<KpiCellSpec>(copy).map_err(invalid)?
        } else {
            let inline = Value::Object(super::unknown_fields(spec, &CELL_KEYS));
            let kpi = serde_json::from_value::<KpiSpec>(inline).map_err(invalid)?;
            KpiCellSpec {
                baseline: spec
                    .get("baseline")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                tiles: vec![KpiTile {
                    label: String::new(),
                    kpi,
                    caption: None,
                }],
                ..KpiCellSpec::default()
            }
        };
        if parsed.tiles.is_empty() {
            return Err(KpiSpecProblem::NoTiles);
        }
        parsed.input = Some(input);
        Ok(parsed)
    }

    /// Whether `baseline` means the previous recorded run, not a cell.
    pub fn baseline_is_previous_run(&self) -> bool {
        self.baseline.as_deref() == Some(PREVIOUS_RUN)
    }

    /// Keep `inputs` in step with `baseline` so the graph sees it.
    pub fn sync_inputs(&mut self) {
        self.inputs = self.baseline.iter().cloned().collect();
    }

    /// Whether any tile compares with the baseline cell.
    pub fn uses_baseline(&self) -> bool {
        self.tiles
            .iter()
            .any(|t| matches!(t.kpi.comparison, Some(Comparison::Baseline)))
    }
}

/// One tile's evaluation.
#[derive(Debug, Clone, PartialEq)]
pub struct TileOutcome {
    pub label: String,
    pub result: Result<KpiResult, KpiError>,
}

/// Every tile over `view` (and `baseline` for baseline comparisons).
pub fn evaluate(
    spec: &KpiCellSpec,
    view: &OutputView<'_>,
    baseline: Option<&OutputView<'_>>,
) -> Vec<TileOutcome> {
    spec.tiles
        .iter()
        .take(MAX_TILES)
        .map(|tile| TileOutcome {
            label: tile.label.clone(),
            result: evaluate_kpi(view, &tile.kpi, baseline),
        })
        .collect()
}

/// The report's number format for an analytics format hint.
pub fn value_format(hint: &FormatHint) -> ValueFormat {
    let mut format = ValueFormat {
        decimals: Some(hint.decimals),
        thousands: true,
        ..ValueFormat::default()
    };
    match hint.style {
        NumberStyle::Integer | NumberStyle::Decimal => {}
        NumberStyle::Currency => {
            format.style = ReportStyle::Currency;
            format.currency = Some("USD".into());
        }
        NumberStyle::Percent {
            percent_of_one: true,
        } => format.style = ReportStyle::Percent,
        NumberStyle::Percent {
            percent_of_one: false,
        } => format.unit = Some("%".into()),
    }
    if hint.compact {
        format.style = ReportStyle::Compact;
    }
    format
}

pub fn report_status(status: KpiStatus) -> Option<Status> {
    match status {
        KpiStatus::Good => Some(Status::Good),
        KpiStatus::Warning => Some(Status::Warning),
        KpiStatus::Critical => Some(Status::Critical),
        KpiStatus::Neutral => Some(Status::Neutral),
        KpiStatus::Unknown => None,
    }
}

/// The report block for an evaluated tile; `comparison_label` names what
/// the value is compared with, in the caller's language.
pub fn kpi_block(
    tile: &KpiTile,
    result: &KpiResult,
    comparison_label: &str,
    cell_id: Option<&str>,
) -> KpiBlock {
    KpiBlock {
        cell_id: cell_id.map(str::to_string),
        label: tile.label.clone(),
        value: Some(result.value),
        format: value_format(&result.format),
        comparison: result.comparison_value.map(|value| KpiComparison {
            label: comparison_label.to_string(),
            value,
        }),
        status: report_status(result.status),
        thresholds: Vec::new(),
        sparkline: result.sparkline.iter().map(|p| p.value).collect(),
        higher_is_better: tile.kpi.direction != Direction::LowerIsBetter,
        caption: tile.caption.clone(),
    }
}

fn status_name(status: KpiStatus) -> &'static str {
    match status {
        KpiStatus::Good => "good",
        KpiStatus::Warning => "warning",
        KpiStatus::Critical => "critical",
        KpiStatus::Neutral => "neutral",
        KpiStatus::Unknown => "unknown",
    }
}

/// The cell's output: one row per tile with its value, comparison, change
/// and status (stable status names, not display text).
pub fn output_rows(outcomes: &[TileOutcome]) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let column = |name: &str, data_type: &str| ResultColumn {
        name: name.into(),
        data_type: data_type.into(),
        nullable: true,
    };
    let columns = vec![
        column("kpi", "varchar"),
        column("value", "double"),
        column("display", "varchar"),
        column("comparison", "double"),
        column("delta", "double"),
        column("delta_percent", "double"),
        column("status", "varchar"),
    ];
    let number = |v: Option<f64>| v.map_or(CellValue::Null, CellValue::F64);
    let rows = outcomes
        .iter()
        .map(|o| match &o.result {
            Ok(r) => vec![
                CellValue::String(o.label.clone()),
                CellValue::F64(r.value),
                CellValue::String(format_value(r.value, &r.format)),
                number(r.comparison_value),
                number(r.delta_absolute),
                number(r.delta_percent),
                CellValue::String(status_name(r.status).into()),
            ],
            Err(_) => vec![
                CellValue::String(o.label.clone()),
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                CellValue::String("error".into()),
            ],
        })
        .collect();
    (columns, rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::Completeness;
    use crate::analytics::kpi::{SparklineSpec, ThresholdBasis, Thresholds};
    use crate::analytics::time::TimeGrain;

    #[test]
    fn strict_parsing_reads_tiles_and_the_inline_command_line_shape() {
        use serde_json::json;
        let inline = KpiCellSpec::parse(&json!({"input": {"cell": "revenue"},
            "baseline": "previous_run",
            "value": {"kind": "aggregate", "column": "amount", "aggregate": "sum"},
            "comparison": {"kind": "target", "value": 1000.0},
            "thresholds": {"warn": 900.0, "critical": 500.0}}))
        .unwrap();
        assert_eq!(inline.input.as_deref(), Some("revenue"));
        assert!(inline.baseline_is_previous_run());
        assert_eq!(inline.tiles.len(), 1);
        assert!(inline.tiles[0].kpi.thresholds.is_some());
        let tiles = KpiCellSpec::parse(&json!({"input": "daily", "baseline": "c_last_year",
            "tiles": [{"label": "Revenue", "kpi": {"value": {"kind": "row_count"}}}]}))
        .unwrap();
        assert_eq!(tiles.tiles[0].label, "Revenue");
        assert!(!tiles.baseline_is_previous_run());
        assert_eq!(
            KpiCellSpec::parse(&json!({"value": {"kind": "row_count"}})),
            Err(KpiSpecProblem::NoInput)
        );
        assert_eq!(
            KpiCellSpec::parse(&json!({"input": "r", "tiles": []})),
            Err(KpiSpecProblem::NoTiles)
        );
        assert!(matches!(
            KpiCellSpec::parse(&json!({"input": "r"})),
            Err(KpiSpecProblem::Invalid { .. })
        ));
    }

    fn data() -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
        let columns = vec![
            ResultColumn {
                name: "day".into(),
                data_type: "date".into(),
                nullable: false,
            },
            ResultColumn {
                name: "revenue".into(),
                data_type: "decimal(12,2)".into(),
                nullable: true,
            },
        ];
        let rows = [
            ("2026-09-03", 100.0),
            ("2026-09-20", 50.0),
            ("2026-10-02", 120.0),
        ]
        .iter()
        .map(|(d, v)| vec![CellValue::String((*d).into()), CellValue::F64(*v)])
        .collect();
        (columns, rows)
    }

    fn scorecard() -> KpiCellSpec {
        let mut revenue = KpiTile::new("Revenue", Some("revenue"));
        revenue.kpi.comparison = Some(Comparison::PreviousPeriod {
            time_column: "day".into(),
            grain: Some(TimeGrain::Month),
        });
        revenue.kpi.thresholds = Some(Thresholds {
            basis: ThresholdBasis::DeltaPercent,
            warn: Some(-5.0),
            critical: Some(-30.0),
        });
        revenue.kpi.sparkline = Some(SparklineSpec {
            time_column: "day".into(),
            value_column: None,
            aggregate: Aggregate::Sum,
            grain: Some(TimeGrain::Month),
            max_points: 12,
        });
        let mut target = KpiTile::new("Orders", None);
        target.kpi.comparison = Some(Comparison::Target { value: 4.0 });
        KpiCellSpec {
            input: Some("c_daily".into()),
            tiles: vec![revenue, target],
            ..KpiCellSpec::default()
        }
    }

    #[test]
    fn scorecards_round_trip_and_evaluate_every_tile() {
        let mut spec = scorecard();
        spec.baseline = Some("c_base".into());
        spec.sync_inputs();
        let json = super::super::spec::write(&spec);
        assert_eq!(json["inputs"], serde_json::json!(["c_base"]));
        assert_eq!(super::super::spec::read::<KpiCellSpec>(&json), spec);

        let (columns, rows) = data();
        let view = OutputView::new(&columns, &rows, Completeness::Complete);
        let outcomes = evaluate(&spec, &view, None);
        assert_eq!(outcomes.len(), 2);
        let revenue = outcomes[0].result.as_ref().unwrap();
        assert_eq!(revenue.value, 120.0);
        assert_eq!(revenue.comparison_value, Some(150.0));
        assert_eq!(revenue.status, KpiStatus::Warning);
        assert_eq!(revenue.sparkline.len(), 2);
        let orders = outcomes[1].result.as_ref().unwrap();
        assert_eq!(orders.value, 3.0);

        let block = kpi_block(&spec.tiles[0], revenue, "previous month", Some("c_kpi"));
        assert_eq!(block.comparison.unwrap().value, 150.0);
        assert_eq!(block.status, Some(Status::Warning));
        let (out_columns, out_rows) = output_rows(&outcomes);
        assert_eq!(out_columns.len(), out_rows[0].len());
        assert_eq!(out_rows[0][6], CellValue::String("warning".into()));
    }

    #[test]
    fn truncated_inputs_withhold_status() {
        let spec = scorecard();
        let (columns, rows) = data();
        let view = OutputView::new(&columns, &rows, Completeness::Truncated { cap: 3 });
        let outcomes = evaluate(&spec, &view, None);
        let revenue = outcomes[0].result.as_ref().unwrap();
        assert!(revenue.inconclusive);
        assert_eq!(revenue.status, KpiStatus::Unknown);
        assert_eq!(report_status(revenue.status), None);
    }
}
