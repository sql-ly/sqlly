//! Decision-model attention triage for grid-backed admin panes (plan 300).
//!
//! Admin telemetry grids show a hundred rows so a DBA can find the two that
//! matter. This module asks the optional decision model — one `Choice` per
//! row, all rows in one request — whether each row needs intervention now
//! (`Act`), is unusual enough to check again soon (`Watch`), or is normal
//! activity (`Ignore`). It is generic over the `columns + rows` shape every
//! admin pane shares; the Activity Monitor is the first (and so far only)
//! consumer. Pure request/response logic lives here; the per-pane switch,
//! the spawn cadence, and the ordering live with the pane.
//!
//! Privacy: rows are sent as-is — SQL text (truncated), login, host, and
//! wait info included — so callers must gate on `allow_row_data` AND an
//! explicit per-pane opt-in. Durations are pre-formatted into readable
//! strings in code because the model is unreliable at arithmetic and dates.

use std::collections::BTreeMap;

use sqlly_ai::catalog::{Answer, DecisionProvider, DecisionRequest, Question};
use sqlly_core::{CellValue, ResultColumn};

/// Rows sent per triage request, at most. Callers pass their visible cap.
pub const TRIAGE_MAX_ROWS: usize = 100;

/// SQL-ish text values are truncated to this many characters before sending.
pub const SQL_TEXT_CHAR_CAP: usize = 300;

/// The model's call on one telemetry row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// Needs intervention now: blocking others, runaway, or failing.
    Act,
    /// Unusual; check again soon.
    Watch,
    /// Normal activity.
    Ignore,
}

impl Attention {
    pub fn label(self) -> String {
        crate::localization::text(match self {
            Self::Act => "explorer.admin_triage.attention_act",
            Self::Watch => "explorer.admin_triage.attention_watch",
            Self::Ignore => "explorer.admin_triage.attention_ignore",
        })
    }

    /// Sort rank: `Act` first, `Ignore` last.
    pub fn rank(self) -> u8 {
        match self {
            Self::Act => 0,
            Self::Watch => 1,
            Self::Ignore => 2,
        }
    }

    fn from_choice(choice: &str) -> Option<Self> {
        match choice {
            "act" => Some(Self::Act),
            "watch" => Some(Self::Watch),
            "ignore" => Some(Self::Ignore),
            _ => None,
        }
    }
}

fn row_question_id(index: usize) -> String {
    format!("r{index}")
}

/// Whether the column's values are SQL-ish free text worth truncating.
fn is_sql_text_column(column: &ResultColumn) -> bool {
    let name = column.name.to_ascii_lowercase();
    (name.contains("query") || name.contains("sql") || name.contains("text"))
        && !name.contains("handle")
}

/// Whether the column holds a duration the model must not be asked to do
/// arithmetic on.
fn is_duration_column(column: &ResultColumn) -> bool {
    let name = column.name.to_ascii_lowercase();
    name.contains("seconds") || name.contains("elapsed") || name.ends_with("_ms")
}

/// `95` → `"1 min 35 s"`, in units coarse enough to read at a glance.
pub fn humanize_seconds(seconds: i64) -> String {
    let (hours, minutes, secs) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    let (key, arguments) = if seconds < 0 {
        (
            "explorer.admin_triage.duration_seconds",
            vec![("seconds", seconds.to_string())],
        )
    } else if hours > 0 {
        (
            "explorer.admin_triage.duration_hours_minutes",
            vec![
                ("hours", hours.to_string()),
                ("minutes", minutes.to_string()),
            ],
        )
    } else if minutes > 0 && secs > 0 {
        (
            "explorer.admin_triage.duration_minutes_seconds",
            vec![
                ("minutes", minutes.to_string()),
                ("seconds", secs.to_string()),
            ],
        )
    } else if minutes > 0 {
        (
            "explorer.admin_triage.duration_minutes",
            vec![("minutes", minutes.to_string())],
        )
    } else {
        (
            "explorer.admin_triage.duration_seconds",
            vec![("seconds", secs.to_string())],
        )
    };
    let arguments = arguments
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect::<Vec<_>>();
    crate::localization::format(key, &arguments)
}

fn duration_seconds(column: &ResultColumn, value: &CellValue) -> Option<i64> {
    let raw = match value {
        CellValue::I64(v) => *v,
        CellValue::F64(v) if v.is_finite() => *v as i64,
        _ => return None,
    };
    if column.name.to_ascii_lowercase().ends_with("_ms")
        || column.name.to_ascii_lowercase().contains("elapsed_time")
    {
        Some(raw / 1_000)
    } else {
        Some(raw)
    }
}

fn cell_json(value: &CellValue, truncate_sql: bool) -> serde_json::Value {
    match value {
        CellValue::Null => serde_json::Value::Null,
        CellValue::Bool(v) => (*v).into(),
        CellValue::I64(v) => (*v).into(),
        CellValue::F64(v) => serde_json::Number::from_f64(*v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        CellValue::String(v) => {
            if truncate_sql && v.chars().count() > SQL_TEXT_CHAR_CAP {
                v.chars().take(SQL_TEXT_CHAR_CAP).collect::<String>().into()
            } else {
                v.clone().into()
            }
        }
        CellValue::Bytes(_) => serde_json::Value::Null,
    }
}

/// The single decision request behind one triage pass: the pane's context
/// sentence plus one object per row (SQL text truncated, durations doubled
/// into pre-formatted readable strings), and one act/watch/ignore `Choice`
/// per row. `None` when there is nothing to triage.
pub fn triage_request(
    context: &str,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    max_rows: usize,
) -> Option<DecisionRequest> {
    if rows.is_empty() || columns.is_empty() || max_rows == 0 {
        return None;
    }
    let mut state = serde_json::Map::new();
    state.insert("context".to_string(), context.into());
    let mut questions = BTreeMap::new();
    for (index, row) in rows.iter().take(max_rows).enumerate() {
        let mut entry = serde_json::Map::new();
        for (column, value) in columns.iter().zip(row) {
            entry.insert(
                column.name.clone(),
                cell_json(value, is_sql_text_column(column)),
            );
            // The model is unreliable at arithmetic and dates: give it the
            // duration as words next to the raw number.
            if is_duration_column(column)
                && let Some(seconds) = duration_seconds(column, value)
            {
                entry.insert("running_for".to_string(), humanize_seconds(seconds).into());
            }
        }
        state.insert(row_question_id(index), serde_json::Value::Object(entry));
        questions.insert(
            row_question_id(index),
            Question::Choice {
                instructions: format!(
                    "Does row {} of this server telemetry need a database administrator's attention?",
                    row_question_id(index)
                ),
                options: vec![
                    (
                        "act".to_string(),
                        Some("Needs intervention now: blocking others, runaway, or failing".to_string()),
                    ),
                    ("watch".to_string(), Some("Unusual; check again soon".to_string())),
                    ("ignore".to_string(), Some("Normal activity".to_string())),
                ],
            },
        );
    }
    Some(DecisionRequest {
        state: serde_json::Value::Object(state),
        questions,
    })
}

/// Triage up to `max_rows` rows through the decision model. Returns one
/// `(Attention, confidence)` per sent row, in row order; a missing or
/// malformed answer becomes `Watch` at zero confidence so a model gap can
/// never hide a row. `None` when there is nothing to send or the request
/// fails — the caller keeps its unranked view.
pub async fn triage_rows(
    decider: &dyn DecisionProvider,
    context: &str,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    max_rows: usize,
) -> Option<Vec<(Attention, f32)>> {
    let request = triage_request(context, columns, rows, max_rows)?;
    let sent = rows.len().min(max_rows);
    let response = decider.decide(request).await.ok()?;
    Some(
        (0..sent)
            .map(|index| {
                match response.answers.get(&row_question_id(index)) {
                    Some(Answer::Choice {
                        choice, confidence, ..
                    }) => Attention::from_choice(choice)
                        .map(|attention| (attention, *confidence))
                        // An id outside act/watch/ignore is a decode gap:
                        // keep the row visible.
                        .unwrap_or((Attention::Watch, 0.0)),
                    _ => (Attention::Watch, 0.0),
                }
            })
            .collect(),
    )
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

    fn state_object(request: &DecisionRequest) -> &serde_json::Map<String, serde_json::Value> {
        request.state.as_object().expect("object state")
    }

    #[test]
    fn caps_rows() {
        let columns = vec![col("session_id", "int")];
        let rows: Vec<Vec<CellValue>> = (0..150).map(|i| vec![CellValue::I64(i)]).collect();
        let request =
            triage_request("Sessions", &columns, &rows, TRIAGE_MAX_ROWS).expect("request");
        assert_eq!(request.questions.len(), TRIAGE_MAX_ROWS);
        // context + one entry per sent row
        assert_eq!(state_object(&request).len(), TRIAGE_MAX_ROWS + 1);
        assert!(!request.questions.contains_key("r100"));
    }

    #[test]
    fn truncates_sql() {
        let columns = vec![col("query_text", "nvarchar(max)")];
        let rows = vec![vec![CellValue::String("x".repeat(2_000))]];
        let request =
            triage_request("Sessions", &columns, &rows, TRIAGE_MAX_ROWS).expect("request");
        let sent = state_object(&request)["r0"]["query_text"]
            .as_str()
            .expect("string");
        assert_eq!(sent.chars().count(), SQL_TEXT_CHAR_CAP);
    }

    #[test]
    fn durations_are_preformatted_strings() {
        let columns = vec![col("session_id", "int"), col("seconds_running", "bigint")];
        let rows = vec![vec![CellValue::I64(51), CellValue::I64(14 * 60)]];
        let request =
            triage_request("Sessions", &columns, &rows, TRIAGE_MAX_ROWS).expect("request");
        assert_eq!(
            state_object(&request)["r0"]["running_for"],
            serde_json::Value::from("14 min")
        );
        assert_eq!(humanize_seconds(95), "1 min 35 s");
        assert_eq!(humanize_seconds(7_500), "2 h 5 min");
    }

    #[tokio::test]
    async fn maps_answers_and_defaults_gaps_to_watch() {
        let columns = vec![col("session_id", "int")];
        let rows: Vec<Vec<CellValue>> = (0..3).map(|i| vec![CellValue::I64(i)]).collect();
        let stub = StubDecisionProvider::with_answers(BTreeMap::from([
            (
                "r0".to_string(),
                Answer::Choice {
                    choice: "act".to_string(),
                    probabilities: BTreeMap::new(),
                    confidence: 0.9,
                },
            ),
            (
                "r1".to_string(),
                Answer::Choice {
                    choice: "ignore".to_string(),
                    probabilities: BTreeMap::new(),
                    confidence: 0.8,
                },
            ),
            (
                "r2".to_string(),
                Answer::Choice {
                    choice: "act".to_string(),
                    probabilities: BTreeMap::new(),
                    confidence: 0.7,
                },
            ),
        ]));
        let verdicts = triage_rows(&stub, "Sessions", &columns, &rows, 2)
            .await
            .expect("verdicts");
        // Only two rows were sent; r2's scripted answer is never requested.
        assert_eq!(
            verdicts,
            vec![(Attention::Act, 0.9), (Attention::Ignore, 0.8)]
        );
    }
}
