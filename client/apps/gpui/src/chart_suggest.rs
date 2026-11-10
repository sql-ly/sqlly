//! Decision-model chart suggestion for the Chart tab (plan 296).
//!
//! Auto-chart today picks the first label-like column and every numeric
//! column, which often charts an ID column or a dozen unrelated series. This
//! module asks the optional decision model — in one request — which column
//! best labels the categories, which numeric columns are measures worth
//! plotting, and which chart kind fits, and builds a [`ChartConfig`] from the
//! answers. Pure request/response logic lives here; the per-slot spawn, the
//! precedence rule (a saved layout or directive always wins), and the
//! "Suggested chart · Revert" caption live in `results`.
//!
//! Privacy: sends column names and database types only. No values.

use std::collections::BTreeMap;

use sqlly_ai::catalog::{Answer, DecisionProvider, DecisionRequest, Question};
use sqlly_core::ResultColumn;
use sqlly_datatable::{ChartConfig, ChartKind};

use crate::result_cell_format::column_is_numeric;

/// A label choice below this confidence yields no suggestion at all.
pub const MIN_LABEL_CONFIDENCE: f32 = 0.6;

/// A numeric column is kept as a series when its measure probability is at
/// least this.
pub const MIN_SERIES_PROBABILITY: f32 = 0.5;

/// At most this many value columns are suggested, keeping the first chart
/// readable.
pub const MAX_SERIES: usize = 4;

/// The chart kinds offered to the model, with plain descriptions. Mirrors
/// the variants [`crate::result_chart::ResultChartSpec`] round-trips.
const KIND_OPTIONS: &[(ChartKind, &str, &str)] = &[
    (ChartKind::Bar, "bar", "Compare amounts across categories"),
    (
        ChartKind::Line,
        "line",
        "Show how values change over an ordered axis such as time",
    ),
    (
        ChartKind::Area,
        "area",
        "Show cumulative or stacked change over an ordered axis",
    ),
    (
        ChartKind::Scatter,
        "scatter",
        "Show the relationship between individual measurements",
    ),
    (
        ChartKind::Histogram,
        "histogram",
        "Show the distribution of one numeric column",
    ),
    (
        ChartKind::Pie,
        "pie",
        "Show each category's share of a single total",
    ),
    (
        ChartKind::Donut,
        "donut",
        "Show each category's share of a single total, with the center open",
    ),
];

fn kind_from_id(id: &str) -> Option<ChartKind> {
    KIND_OPTIONS
        .iter()
        .find(|(_, kind_id, _)| *kind_id == id)
        .map(|(kind, _, _)| *kind)
}

fn localized_kind_description(kind: ChartKind) -> String {
    let key = match kind {
        ChartKind::Bar => "results.chart_suggest.bar_description",
        ChartKind::Line => "results.chart_suggest.line_description",
        ChartKind::Area => "results.chart_suggest.area_description",
        ChartKind::Scatter => "results.chart_suggest.scatter_description",
        ChartKind::Histogram => "results.chart_suggest.histogram_description",
        ChartKind::Pie => "results.chart_suggest.pie_description",
        ChartKind::Donut => "results.chart_suggest.donut_description",
    };
    crate::localization::text(key)
}

/// Whether the declared database type is numeric — decided in code, never by
/// the model.
pub fn is_numeric_column(column: &ResultColumn) -> bool {
    column_is_numeric(Some(column))
}

fn series_question_id(index: usize) -> String {
    format!("series{index}")
}

/// The single decision request behind a chart suggestion: a `Choice` for the
/// label column, one `Noul` per numeric-typed column, and a `Choice` for the
/// chart kind. Returns `None` when the shape cannot chart (no columns, or no
/// numeric column to plot). The state carries column names and database
/// types only.
pub fn suggestion_request(columns: &[ResultColumn], row_count: usize) -> Option<DecisionRequest> {
    if columns.len() < 2 {
        return None;
    }
    let numeric: Vec<usize> = (0..columns.len())
        .filter(|&i| is_numeric_column(&columns[i]))
        .collect();
    if numeric.is_empty() {
        return None;
    }

    let state = serde_json::json!({
        "columns": columns
            .iter()
            .map(|c| serde_json::json!({ "name": c.name, "type": c.data_type }))
            .collect::<Vec<_>>(),
        "row_count": row_count,
    });

    let mut questions = BTreeMap::new();
    questions.insert(
        "label".to_string(),
        Question::Choice {
            instructions: crate::localization::text("results.chart_suggest.label_question"),
            options: columns
                .iter()
                .map(|column| {
                    let description = crate::localization::format(
                        "results.chart_suggest.column_type_description",
                        &[("data_type", &column.data_type)],
                    );
                    (column.name.clone(), Some(description))
                })
                .collect(),
        },
    );
    for &index in &numeric {
        questions.insert(
            series_question_id(index),
            Question::Noul {
                instructions: crate::localization::format(
                    "results.chart_suggest.measure_question",
                    &[("column_name", &columns[index].name)],
                ),
            },
        );
    }
    questions.insert(
        "kind".to_string(),
        Question::Choice {
            instructions: crate::localization::text("results.chart_suggest.kind_question"),
            options: KIND_OPTIONS
                .iter()
                .map(|(kind, id, _)| (id.to_string(), Some(localized_kind_description(*kind))))
                .collect(),
        },
    );
    Some(DecisionRequest { state, questions })
}

/// Ask the decision model for a first chart of this result. `None` when the
/// result has no chartable shape, the label answer is missing or under
/// [`MIN_LABEL_CONFIDENCE`], no series survives, or the request fails — the
/// caller falls back to today's behaviour in every one of those cases.
pub async fn suggest_chart(
    decider: &dyn DecisionProvider,
    columns: &[ResultColumn],
    row_count: usize,
) -> Option<ChartConfig> {
    let request = suggestion_request(columns, row_count)?;
    let response = decider.decide(request).await.ok()?;

    let label_name = response
        .answers
        .get("label")?
        .confident_choice(MIN_LABEL_CONFIDENCE)?
        .to_string();
    let label_column = columns.iter().position(|c| c.name == label_name)?;

    let mut value_columns: Vec<usize> = (0..columns.len())
        .filter(|&i| i != label_column && is_numeric_column(&columns[i]))
        .filter(|&i| {
            response
                .answers
                .get(&series_question_id(i))
                .is_some_and(|answer| answer.noul_at_least(MIN_SERIES_PROBABILITY))
        })
        .collect();
    value_columns.truncate(MAX_SERIES);
    if value_columns.is_empty() {
        return None;
    }

    let kind = response
        .answers
        .get("kind")
        .and_then(|answer| match answer {
            Answer::Choice { choice, .. } => kind_from_id(choice),
            _ => None,
        })
        .unwrap_or(ChartKind::Bar);

    Some(ChartConfig {
        kind,
        label_column: Some(label_column),
        value_columns,
        ..ChartConfig::default()
    })
}

#[cfg(test)]
mod tests {
    use sqlly_ai::catalog::StubDecisionProvider;

    use super::*;

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
        }
    }

    fn choice(id: &str, confidence: f32) -> Answer {
        Answer::Choice {
            choice: id.to_string(),
            probabilities: BTreeMap::from([(id.to_string(), 1.0)]),
            confidence,
        }
    }

    fn noul(probability: f32) -> Answer {
        Answer::Noul { probability }
    }

    fn columns() -> Vec<ResultColumn> {
        vec![
            col("Id", "int"),
            col("Region", "nvarchar(64)"),
            col("Total", "decimal(18,2)"),
        ]
    }

    #[tokio::test]
    async fn low_confidence_is_none() {
        let stub = StubDecisionProvider::with_answers(BTreeMap::from([
            ("label".to_string(), choice("Region", 0.4)),
            (series_question_id(0), noul(0.1)),
            (series_question_id(2), noul(0.9)),
            ("kind".to_string(), choice("bar", 0.9)),
        ]));
        assert_eq!(suggest_chart(&stub, &columns(), 10).await, None);
    }

    #[tokio::test]
    async fn id_like_numeric_excluded_when_noul_low() {
        let stub = StubDecisionProvider::with_answers(BTreeMap::from([
            ("label".to_string(), choice("Region", 0.9)),
            (series_question_id(0), noul(0.1)),
            (series_question_id(2), noul(0.9)),
            ("kind".to_string(), choice("bar", 0.9)),
        ]));
        let config = suggest_chart(&stub, &columns(), 10)
            .await
            .expect("suggestion");
        assert_eq!(config.label_column, Some(1));
        assert_eq!(config.value_columns, vec![2]);
        assert_eq!(config.kind, ChartKind::Bar);
    }

    #[tokio::test]
    async fn at_most_four_series() {
        let mut cols = vec![col("Month", "nvarchar(16)")];
        for i in 0..6 {
            cols.push(col(&format!("M{i}"), "float"));
        }
        let mut answers = BTreeMap::from([
            ("label".to_string(), choice("Month", 0.9)),
            ("kind".to_string(), choice("line", 0.9)),
        ]);
        for i in 1..=6 {
            answers.insert(series_question_id(i), noul(0.9));
        }
        let stub = StubDecisionProvider::with_answers(answers);
        let config = suggest_chart(&stub, &cols, 10).await.expect("suggestion");
        assert_eq!(config.value_columns.len(), MAX_SERIES);
        assert_eq!(config.value_columns, vec![1, 2, 3, 4]);
        assert_eq!(config.kind, ChartKind::Line);
    }

    #[tokio::test]
    async fn no_numeric_columns_is_none() {
        let cols = vec![col("A", "nvarchar(10)"), col("B", "nvarchar(10)")];
        let stub = StubDecisionProvider::with_answers(BTreeMap::new());
        assert_eq!(suggest_chart(&stub, &cols, 10).await, None);
    }
}
