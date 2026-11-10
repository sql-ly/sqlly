//! Risk triage for Schema Compare differences.
//!
//! A deterministic first pass classifies every difference by what its status
//! alone implies ([`base_risk`]): target-only tables are data loss to deploy,
//! renames break callers, and so on. `Changed` definition pairs cannot be
//! judged from the status, so they start as [`Risk::Unknown`] until the
//! optional decision-model pass (plan 299 Step 2) classifies them. Deploy
//! logic never reads anything in this module: risk is a parallel view over
//! `ComparedDifference`, kept out of `schema_plan.rs` on purpose.

use std::collections::BTreeMap;

use sqlly_ai::catalog::{Answer, DecisionProvider, DecisionRequest, DecisionResponse, Question};
use sqlly_services::{ComparedDifference, DifferenceStatus};

/// How much a difference can hurt when deployed, riskiest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    /// Deploying drops a table or column that exists only in the target.
    DataLoss,
    /// Callers that depend on the current name or shape stop working.
    Breaking,
    /// A human must look: the compare itself flagged it, or the model was
    /// not confident.
    Review,
    /// Not yet classified: a changed definition awaiting the optional
    /// decision-model pass.
    Unknown,
    /// Additive; nothing existing is removed or narrowed.
    Safe,
    /// Whitespace, comments, or formatting only.
    Cosmetic,
}

impl Risk {
    /// Sort key: riskiest first. `Unknown` sits between `Review` and `Safe`
    /// so unassessed changes are never buried below known-safe rows.
    pub fn rank(self) -> u8 {
        match self {
            Risk::DataLoss => 0,
            Risk::Breaking => 1,
            Risk::Review => 2,
            Risk::Unknown => 3,
            Risk::Safe => 4,
            Risk::Cosmetic => 5,
        }
    }

    /// Grid cell text.
    pub fn label(self) -> String {
        crate::localization::text(match self {
            Risk::DataLoss => "explorer.schema_compare.risk_data_loss",
            Risk::Breaking => "explorer.schema_compare.risk_breaking",
            Risk::Review => "explorer.schema_compare.risk_review",
            Risk::Unknown => "explorer.schema_compare.risk_unknown",
            Risk::Safe => "explorer.schema_compare.risk_safe",
            Risk::Cosmetic => "explorer.schema_compare.risk_cosmetic",
        })
    }
}

/// The risk a difference carries from its status alone, before any model
/// pass. Deterministic and total: every difference gets a risk.
pub fn base_risk(difference: &ComparedDifference) -> Risk {
    match &difference.status {
        // Deploy (with drops enabled) would drop the object from the target.
        // Dropping a table loses rows; dropping anything else breaks whatever
        // referenced it but destroys no data.
        DifferenceStatus::ExtraInTarget => {
            let kind = difference.object.kind.to_ascii_uppercase();
            if kind.contains("TABLE") || kind.contains("COLUMN") {
                Risk::DataLoss
            } else {
                Risk::Breaking
            }
        }
        // Deploy creates the object in the target: purely additive.
        DifferenceStatus::MissingInTarget => Risk::Safe,
        DifferenceStatus::ManualReview | DifferenceStatus::Unsupported => Risk::Review,
        // The rename script changes a name callers may depend on.
        DifferenceStatus::Renamed { .. } => Risk::Breaking,
        // A definition change needs the definitions read to be judged.
        DifferenceStatus::Changed => Risk::Unknown,
    }
}

/// The order the grid shows differences in: riskiest first, then by object
/// identity so the order is stable. Returns indices into `differences`; the
/// slices themselves are never reordered, so deploy keeps reading the
/// original vector untouched.
pub fn display_order(risks: &[Risk], differences: &[ComparedDifference]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..differences.len()).collect();
    order.sort_by(|&a, &b| {
        let rank_a = risks.get(a).copied().unwrap_or(Risk::Unknown).rank();
        let rank_b = risks.get(b).copied().unwrap_or(Risk::Unknown).rank();
        rank_a.cmp(&rank_b).then_with(|| {
            let object_a = &differences[a].object;
            let object_b = &differences[b].object;
            (&object_a.schema, &object_a.name, &object_a.kind).cmp(&(
                &object_b.schema,
                &object_b.name,
                &object_b.kind,
            ))
        })
    });
    order
}

/// How many `Changed` items go into one decision request.
pub const ASSESS_BATCH_SIZE: usize = 20;

/// Definitions are truncated to this many characters before they are sent.
pub const DEFINITION_CHAR_CAP: usize = 4_000;

/// A choice below this confidence downgrades to [`Risk::Review`].
pub const MIN_CHOICE_CONFIDENCE: f32 = 0.6;

/// The question id for the difference at `index`.
fn question_id(index: usize) -> String {
    format!("risk_{index}")
}

/// `text` cut to at most `cap` characters on a char boundary.
fn truncated(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn risk_question(id: &str) -> Question {
    Question::Choice {
        instructions: format!(
            "For the state item with id '{id}', classify the risk of deploying the source \
             definition over the target definition."
        ),
        options: vec![
            (
                "data_loss".to_string(),
                Some("Narrows a type, drops a column, or removes data".to_string()),
            ),
            (
                "breaking".to_string(),
                Some(
                    "Changes a signature, name, or nullability that callers depend on".to_string(),
                ),
            ),
            (
                "safe".to_string(),
                Some("Adds something or widens a type".to_string()),
            ),
            (
                "cosmetic".to_string(),
                Some("Whitespace, comments, or formatting only".to_string()),
            ),
        ],
    }
}

/// One decision request per batch of up to [`ASSESS_BATCH_SIZE`] `Changed`
/// differences, each paired with the difference indices it asks about. The
/// state carries only what the questions need: object kind, object name, and
/// both definitions capped at [`DEFINITION_CHAR_CAP`] characters.
pub fn assessment_batches(
    differences: &[ComparedDifference],
) -> Vec<(Vec<usize>, DecisionRequest)> {
    let changed: Vec<usize> = differences
        .iter()
        .enumerate()
        .filter(|(_, difference)| difference.status == DifferenceStatus::Changed)
        .map(|(index, _)| index)
        .collect();
    changed
        .chunks(ASSESS_BATCH_SIZE)
        .map(|chunk| {
            let mut items = Vec::with_capacity(chunk.len());
            let mut questions = BTreeMap::new();
            for &index in chunk {
                let difference = &differences[index];
                let id = question_id(index);
                items.push(serde_json::json!({
                    "id": id,
                    "kind": difference.object.kind,
                    "object": format!("{}.{}", difference.object.schema, difference.object.name),
                    "source_definition":
                        truncated(&difference.object.definition, DEFINITION_CHAR_CAP),
                    "target_definition": truncated(
                        difference.target_definition.as_deref().unwrap_or(""),
                        DEFINITION_CHAR_CAP,
                    ),
                }));
                questions.insert(id.clone(), risk_question(&id));
            }
            (
                chunk.to_vec(),
                DecisionRequest {
                    state: serde_json::Value::Array(items),
                    questions,
                },
            )
        })
        .collect()
}

/// The risk a single answer maps to. Anything that is not a confident,
/// recognised choice — a low-confidence pick, an unknown option id, or a
/// wrongly-typed answer — lands on [`Risk::Review`] so a human looks at it.
pub fn risk_from_answer(answer: &Answer) -> Risk {
    match answer.confident_choice(MIN_CHOICE_CONFIDENCE) {
        Some("data_loss") => Risk::DataLoss,
        Some("breaking") => Risk::Breaking,
        Some("safe") => Risk::Safe,
        Some("cosmetic") => Risk::Cosmetic,
        _ => Risk::Review,
    }
}

/// Pair each answered difference index with its mapped risk. Indices whose
/// question the response did not answer are skipped: they keep their base
/// risk.
pub fn risks_from_response(indices: &[usize], response: &DecisionResponse) -> Vec<(usize, Risk)> {
    indices
        .iter()
        .filter_map(|&index| {
            response
                .answers
                .get(&question_id(index))
                .map(|answer| (index, risk_from_answer(answer)))
        })
        .collect()
}

/// Run the model pass over every `Changed` difference. Never fails the host
/// action: a batch that errors is skipped (its items keep their base risk)
/// and the first error text is returned for a status line.
pub async fn assess_changed(
    provider: &dyn DecisionProvider,
    differences: &[ComparedDifference],
) -> (Vec<(usize, Risk)>, Option<String>) {
    let mut results = Vec::new();
    let mut first_error = None;
    for (indices, request) in assessment_batches(differences) {
        match provider.decide(request).await {
            Ok(response) => results.extend(risks_from_response(&indices, &response)),
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error.to_string());
                }
            }
        }
    }
    (results, first_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::SchemaObject;

    fn difference(status: DifferenceStatus, kind: &str) -> ComparedDifference {
        ComparedDifference {
            status,
            object: SchemaObject {
                schema: "dbo".to_string(),
                name: "Orders".to_string(),
                kind: kind.to_string(),
                definition: "id int NOT NULL".to_string(),
            },
            target_definition: None,
            note: String::new(),
        }
    }

    #[test]
    fn missing_in_target_is_safe() {
        assert_eq!(
            base_risk(&difference(DifferenceStatus::MissingInTarget, "TABLE")),
            Risk::Safe
        );
    }

    #[test]
    fn extra_in_target_table_is_data_loss_and_other_kinds_break() {
        assert_eq!(
            base_risk(&difference(DifferenceStatus::ExtraInTarget, "TABLE")),
            Risk::DataLoss
        );
        assert_eq!(
            base_risk(&difference(DifferenceStatus::ExtraInTarget, "VIEW")),
            Risk::Breaking
        );
    }

    #[test]
    fn changed_is_unknown_until_assessed() {
        assert_eq!(
            base_risk(&difference(DifferenceStatus::Changed, "VIEW")),
            Risk::Unknown
        );
    }

    #[test]
    fn unsupported_is_review() {
        assert_eq!(
            base_risk(&difference(DifferenceStatus::Unsupported, "TRIGGER")),
            Risk::Review
        );
    }

    #[test]
    fn manual_review_is_review() {
        assert_eq!(
            base_risk(&difference(DifferenceStatus::ManualReview, "ROUTINE")),
            Risk::Review
        );
    }

    #[test]
    fn renamed_is_breaking() {
        assert_eq!(
            base_risk(&difference(
                DifferenceStatus::Renamed {
                    from: "Order".to_string()
                },
                "TABLE"
            )),
            Risk::Breaking
        );
    }

    fn choice(option: &str, confidence: f32) -> Answer {
        Answer::Choice {
            choice: option.to_string(),
            probabilities: BTreeMap::from([(option.to_string(), confidence)]),
            confidence,
        }
    }

    #[tokio::test]
    async fn scripted_answers_classify_only_changed_items() {
        use sqlly_ai::catalog::StubDecisionProvider;
        let differences = vec![
            difference(DifferenceStatus::MissingInTarget, "TABLE"),
            ComparedDifference {
                target_definition: Some("id int NULL".to_string()),
                ..difference(DifferenceStatus::Changed, "TABLE")
            },
            ComparedDifference {
                target_definition: Some("-- old body".to_string()),
                ..difference(DifferenceStatus::Changed, "VIEW")
            },
        ];
        let batches = assessment_batches(&differences);
        assert_eq!(batches.len(), 1, "3 items hold 2 Changed: one batch");
        assert_eq!(batches[0].0, vec![1, 2], "only Changed items are asked");
        assert!(
            batches[0].1.state.to_string().contains("dbo.Orders"),
            "state names the object"
        );
        let provider = StubDecisionProvider::with_answers(BTreeMap::from([
            ("risk_1".to_string(), choice("data_loss", 0.9)),
            ("risk_2".to_string(), choice("cosmetic", 0.95)),
        ]));
        let (results, error) = assess_changed(&provider, &differences).await;
        assert_eq!(error, None);
        assert_eq!(results, vec![(1, Risk::DataLoss), (2, Risk::Cosmetic)]);
    }

    #[tokio::test]
    async fn low_confidence_or_failed_batches_never_invent_a_risk() {
        use sqlly_ai::catalog::StubDecisionProvider;
        let differences = vec![ComparedDifference {
            target_definition: Some("id int NULL".to_string()),
            ..difference(DifferenceStatus::Changed, "TABLE")
        }];
        let provider = StubDecisionProvider::with_answers(BTreeMap::from([(
            "risk_0".to_string(),
            choice("safe", 0.4),
        )]));
        let (results, error) = assess_changed(&provider, &differences).await;
        assert_eq!(error, None);
        assert_eq!(
            results,
            vec![(0, Risk::Review)],
            "a low-confidence choice downgrades to Review"
        );
        // A provider error skips the batch and reports, leaving base risks
        // untouched.
        let empty = StubDecisionProvider::default();
        let (results, error) = assess_changed(&empty, &differences).await;
        assert_eq!(results, Vec::new());
        assert!(
            error.is_some(),
            "the batch error is surfaced for a status line"
        );
    }

    #[test]
    fn rank_orders_riskiest_first() {
        let ranks = [
            Risk::DataLoss,
            Risk::Breaking,
            Risk::Review,
            Risk::Unknown,
            Risk::Safe,
            Risk::Cosmetic,
        ]
        .map(Risk::rank);
        assert!(ranks.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
