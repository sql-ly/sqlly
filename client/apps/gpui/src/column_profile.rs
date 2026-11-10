//! Semantic profiling of result columns via the decision seam (plan 294).
//!
//! Masking today is pattern-based on column names; a column named `contact`
//! or `c17` full of email addresses renders in clear. This module asks the
//! optional decision model one `Choice` question per column — all columns of
//! a result in a handful of parallel requests — and maps each answer to a
//! [`SemanticType`], so the results toolbar can suggest masking the columns
//! that look personal. Pure request/response logic lives here; the per-slot
//! spawn and the suggestion chip live in `results`.
//!
//! Privacy: names-only mode sends column names and database types. Row
//! values are sampled only when the caller passes `samples`, which the host
//! gates on the decision config's `allow_row_data` switch; columns already
//! masked by rules must be given no samples ([`collect_samples`] enforces
//! this for its `masked` set).

use std::collections::{BTreeMap, HashSet};

use sqlly_ai::catalog::{Answer, DecisionProvider, DecisionRequest, Question};
use sqlly_core::{CellValue, ResultColumn};

/// Distinct non-null values sampled per column, at most.
pub const MAX_SAMPLE_VALUES: usize = 8;

/// Each sampled value is truncated to this many characters.
pub const SAMPLE_VALUE_CHAR_CAP: usize = 64;

/// Only this many leading in-memory rows are scanned for samples. Rows may
/// be spill-backed or partial; the first resident rows are representative
/// enough for typing a column.
pub const SAMPLE_ROW_SCAN_CAP: usize = 200;

/// Columns per decision request; more are split into further requests.
pub const COLUMNS_PER_REQUEST: usize = 60;

/// A choice below this confidence becomes [`SemanticType::Other`].
pub const MIN_CONFIDENCE: f32 = 0.7;

/// What a result column appears to hold, judged from its name, database
/// type, and (when allowed) a few sampled values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticType {
    Email,
    Phone,
    PersonName,
    PostalAddress,
    GovernmentId,
    PaymentCard,
    IpAddress,
    Url,
    Currency,
    Percentage,
    Identifier,
    ForeignKeyLike,
    Timestamp,
    Category,
    FreeText,
    Json,
    Coordinates,
    Secret,
    Other,
}

impl SemanticType {
    /// Every variant, in the order the choice options are offered.
    pub fn all() -> &'static [SemanticType] {
        &[
            SemanticType::Email,
            SemanticType::Phone,
            SemanticType::PersonName,
            SemanticType::PostalAddress,
            SemanticType::GovernmentId,
            SemanticType::PaymentCard,
            SemanticType::IpAddress,
            SemanticType::Url,
            SemanticType::Currency,
            SemanticType::Percentage,
            SemanticType::Identifier,
            SemanticType::ForeignKeyLike,
            SemanticType::Timestamp,
            SemanticType::Category,
            SemanticType::FreeText,
            SemanticType::Json,
            SemanticType::Coordinates,
            SemanticType::Secret,
            SemanticType::Other,
        ]
    }

    /// The stable option id used on the wire.
    pub fn option_id(self) -> &'static str {
        match self {
            SemanticType::Email => "email",
            SemanticType::Phone => "phone",
            SemanticType::PersonName => "person_name",
            SemanticType::PostalAddress => "postal_address",
            SemanticType::GovernmentId => "government_id",
            SemanticType::PaymentCard => "payment_card",
            SemanticType::IpAddress => "ip_address",
            SemanticType::Url => "url",
            SemanticType::Currency => "currency",
            SemanticType::Percentage => "percentage",
            SemanticType::Identifier => "identifier",
            SemanticType::ForeignKeyLike => "foreign_key",
            SemanticType::Timestamp => "timestamp",
            SemanticType::Category => "category",
            SemanticType::FreeText => "free_text",
            SemanticType::Json => "json",
            SemanticType::Coordinates => "coordinates",
            SemanticType::Secret => "secret",
            SemanticType::Other => "other",
        }
    }

    /// The variant for a wire option id, if the id is known.
    pub fn from_option_id(id: &str) -> Option<SemanticType> {
        SemanticType::all()
            .iter()
            .copied()
            .find(|ty| ty.option_id() == id)
    }

    /// The option description shown to the model.
    fn description(self) -> String {
        crate::localization::text(match self {
            SemanticType::Email => "results.column_profile.email_description",
            SemanticType::Phone => "results.column_profile.phone_description",
            SemanticType::PersonName => "results.column_profile.person_name_description",
            SemanticType::PostalAddress => "results.column_profile.postal_address_description",
            SemanticType::GovernmentId => "results.column_profile.government_id_description",
            SemanticType::PaymentCard => "results.column_profile.payment_card_description",
            SemanticType::IpAddress => "results.column_profile.ip_address_description",
            SemanticType::Url => "results.column_profile.url_description",
            SemanticType::Currency => "results.column_profile.currency_description",
            SemanticType::Percentage => "results.column_profile.percentage_description",
            SemanticType::Identifier => "results.column_profile.identifier_description",
            SemanticType::ForeignKeyLike => "results.column_profile.foreign_key_description",
            SemanticType::Timestamp => "results.column_profile.timestamp_description",
            SemanticType::Category => "results.column_profile.category_description",
            SemanticType::FreeText => "results.column_profile.free_text_description",
            SemanticType::Json => "results.column_profile.json_description",
            SemanticType::Coordinates => "results.column_profile.coordinates_description",
            SemanticType::Secret => "results.column_profile.secret_description",
            SemanticType::Other => "results.column_profile.other_description",
        })
    }

    /// True for types whose values identify or expose a person — the ones
    /// the toolbar suggests masking.
    pub fn is_personal(&self) -> bool {
        matches!(
            self,
            SemanticType::Email
                | SemanticType::Phone
                | SemanticType::PersonName
                | SemanticType::PostalAddress
                | SemanticType::GovernmentId
                | SemanticType::PaymentCard
                | SemanticType::IpAddress
                | SemanticType::Secret
        )
    }
}

/// The question id for the column at `index`.
fn question_id(index: usize) -> String {
    format!("c{index}")
}

fn semantic_question(id: &str) -> Question {
    Question::Choice {
        instructions: format!(
            "For the state column with id '{id}', pick the semantic type that best describes \
             what the column holds."
        ),
        options: SemanticType::all()
            .iter()
            .map(|ty| {
                (
                    ty.option_id().to_string(),
                    Some(ty.description().to_string()),
                )
            })
            .collect(),
    }
}

/// `text` cut to at most `cap` characters on a char boundary.
fn truncated(text: &str, cap: usize) -> String {
    text.chars().take(cap).collect()
}

/// A cell rendered as sample text, or `None` for nulls and binary values
/// (bytes are never sampled: they are not useful as text and may be
/// sensitive).
fn sample_text(cell: &CellValue) -> Option<String> {
    match cell {
        CellValue::Null | CellValue::Bytes(_) => None,
        CellValue::Bool(value) => Some(value.to_string()),
        CellValue::I64(value) => Some(value.to_string()),
        CellValue::F64(value) => Some(value.to_string()),
        CellValue::String(value) => Some(truncated(value, SAMPLE_VALUE_CHAR_CAP)),
    }
}

/// Per-column sample values from the first resident rows: up to
/// [`MAX_SAMPLE_VALUES`] distinct non-null values per column, each truncated
/// to [`SAMPLE_VALUE_CHAR_CAP`] characters. Columns in `masked` get an empty
/// sample list — rule-masked columns are never sampled, only their names are
/// sent. Call this only when the user has allowed row data to be sent.
pub fn collect_samples(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    masked: &HashSet<usize>,
) -> Vec<Vec<String>> {
    let mut samples: Vec<Vec<String>> = vec![Vec::new(); columns.len()];
    let mut seen: Vec<HashSet<String>> = vec![HashSet::new(); columns.len()];
    for row in rows.iter().take(SAMPLE_ROW_SCAN_CAP) {
        for (index, cell) in row.iter().enumerate().take(columns.len()) {
            if masked.contains(&index) || samples[index].len() >= MAX_SAMPLE_VALUES {
                continue;
            }
            let Some(text) = sample_text(cell) else {
                continue;
            };
            if seen[index].insert(text.clone()) {
                samples[index].push(text);
            }
        }
    }
    samples
}

/// One decision request per chunk of up to [`COLUMNS_PER_REQUEST`] columns,
/// each paired with the column indices it asks about. The state carries one
/// entry per column in the chunk — `{"c0": {"name", "type"}}`, plus a
/// `"values"` array only when `samples` is present and non-empty for that
/// column (names-only mode sends no values at all).
pub fn profile_requests(
    columns: &[ResultColumn],
    samples: Option<&[Vec<String>]>,
) -> Vec<(Vec<usize>, DecisionRequest)> {
    let indices: Vec<usize> = (0..columns.len()).collect();
    indices
        .chunks(COLUMNS_PER_REQUEST)
        .map(|chunk| {
            let mut state = serde_json::Map::new();
            let mut questions = BTreeMap::new();
            for &index in chunk {
                let column = &columns[index];
                let id = question_id(index);
                let mut entry = serde_json::Map::new();
                entry.insert("name".to_string(), column.name.clone().into());
                entry.insert("type".to_string(), column.data_type.clone().into());
                if let Some(values) = samples.and_then(|s| s.get(index))
                    && !values.is_empty()
                {
                    entry.insert("values".to_string(), values.clone().into());
                }
                state.insert(id.clone(), serde_json::Value::Object(entry));
                questions.insert(id.clone(), semantic_question(&id));
            }
            (
                chunk.to_vec(),
                DecisionRequest {
                    state: serde_json::Value::Object(state),
                    questions,
                },
            )
        })
        .collect()
}

/// The semantic type one answer maps to. A low-confidence choice, an unknown
/// option id, or a wrongly-typed answer all land on [`SemanticType::Other`]
/// so the suggestion never fires on a guess.
pub fn semantic_from_answer(answer: &Answer) -> (SemanticType, f32) {
    match answer {
        Answer::Choice {
            choice, confidence, ..
        } => {
            if *confidence < MIN_CONFIDENCE {
                (SemanticType::Other, *confidence)
            } else {
                (
                    SemanticType::from_option_id(choice).unwrap_or(SemanticType::Other),
                    *confidence,
                )
            }
        }
        _ => (SemanticType::Other, 0.0),
    }
}

/// Profile every column in one pass: one `Choice` per column, chunked into
/// requests of at most [`COLUMNS_PER_REQUEST`] columns. Returns `None` when
/// there is nothing to profile or any request fails — the caller treats
/// `None` as "feature silently absent" and never blocks the host action.
pub async fn profile_columns(
    provider: &dyn DecisionProvider,
    columns: &[ResultColumn],
    samples: Option<&[Vec<String>]>,
) -> Option<Vec<(SemanticType, f32)>> {
    if columns.is_empty() {
        return None;
    }
    let mut out = vec![(SemanticType::Other, 0.0f32); columns.len()];
    for (indices, request) in profile_requests(columns, samples) {
        let response = provider.decide(request).await.ok()?;
        for index in indices {
            if let Some(answer) = response.answers.get(&question_id(index)) {
                out[index] = semantic_from_answer(answer);
            }
        }
    }
    Some(out)
}

/// The columns worth suggesting a mask for: personal-looking and not already
/// masked. `masked` holds the indices that already carry a mask override
/// (rules, connection overrides, or an accepted suggestion).
pub fn suggestion_targets(profile: &[(SemanticType, f32)], masked: &HashSet<usize>) -> Vec<usize> {
    profile
        .iter()
        .enumerate()
        .filter(|(index, (ty, _))| ty.is_personal() && !masked.contains(index))
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_ai::catalog::StubDecisionProvider;

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
        }
    }

    fn choice(option: &str, confidence: f32) -> Answer {
        Answer::Choice {
            choice: option.to_string(),
            probabilities: BTreeMap::from([(option.to_string(), confidence)]),
            confidence,
        }
    }

    #[test]
    fn names_only_state_has_no_values() {
        let columns = vec![col("contact", "varchar(255)"), col("id", "int")];
        let requests = profile_requests(&columns, None);
        assert_eq!(requests.len(), 1);
        let state = requests[0].1.state.to_string();
        assert!(
            !state.contains("values"),
            "names-only mode must send no values: {state}"
        );
        assert!(state.contains("contact") && state.contains("varchar(255)"));
    }

    #[test]
    fn masked_columns_are_never_sampled() {
        let columns = vec![col("ssn", "char(11)"), col("contact", "varchar(255)")];
        let rows = vec![
            vec![
                CellValue::String("123-45-6789".to_string()),
                CellValue::String("a@example.com".to_string()),
            ],
            vec![
                CellValue::String("987-65-4321".to_string()),
                CellValue::String("b@example.com".to_string()),
            ],
        ];
        let masked = HashSet::from([0]);
        let samples = collect_samples(&columns, &rows, &masked);
        assert!(
            samples[0].is_empty(),
            "a rule-masked column must yield no samples"
        );
        assert_eq!(samples[1], vec!["a@example.com", "b@example.com"]);
        // And the request built from these samples carries no masked value.
        let requests = profile_requests(&columns, Some(&samples));
        let state = requests[0].1.state.to_string();
        assert!(!state.contains("123-45-6789") && !state.contains("987-65-4321"));
        assert!(state.contains("a@example.com"));
    }

    #[test]
    fn samples_are_distinct_truncated_and_capped() {
        let columns = vec![col("note", "text")];
        let long = "x".repeat(200);
        let mut rows: Vec<Vec<CellValue>> = vec![
            vec![CellValue::String("dup".to_string())],
            vec![CellValue::String("dup".to_string())],
            vec![CellValue::Null],
            vec![CellValue::String(long.clone())],
        ];
        for i in 0..20 {
            rows.push(vec![CellValue::String(format!("v{i}"))]);
        }
        let samples = collect_samples(&columns, &rows, &HashSet::new());
        assert_eq!(samples[0].len(), MAX_SAMPLE_VALUES);
        assert_eq!(samples[0][0], "dup", "duplicates collapse to one entry");
        assert_eq!(
            samples[0][1].chars().count(),
            SAMPLE_VALUE_CHAR_CAP,
            "long values are truncated"
        );
    }

    #[tokio::test]
    async fn low_confidence_is_other() {
        let columns = vec![col("contact", "varchar(255)")];
        let provider = StubDecisionProvider::with_answers(BTreeMap::from([(
            "c0".to_string(),
            choice("email", 0.5),
        )]));
        let profile = profile_columns(&provider, &columns, None)
            .await
            .expect("profile");
        assert_eq!(profile, vec![(SemanticType::Other, 0.5)]);

        let confident = StubDecisionProvider::with_answers(BTreeMap::from([(
            "c0".to_string(),
            choice("email", 0.9),
        )]));
        let profile = profile_columns(&confident, &columns, None)
            .await
            .expect("profile");
        assert_eq!(profile, vec![(SemanticType::Email, 0.9)]);
    }

    #[test]
    fn chunks_over_60_columns() {
        let columns: Vec<ResultColumn> = (0..61).map(|i| col(&format!("col{i}"), "int")).collect();
        let requests = profile_requests(&columns, None);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0.len(), COLUMNS_PER_REQUEST);
        assert_eq!(requests[1].0, vec![60]);
        assert_eq!(requests[0].1.questions.len(), COLUMNS_PER_REQUEST);
        assert!(requests[0].1.questions.contains_key("c0"));
        assert!(requests[0].1.questions.contains_key("c59"));
        assert_eq!(requests[1].1.questions.len(), 1);
        assert!(requests[1].1.questions.contains_key("c60"));
        // Each request's state carries only its own chunk's columns.
        assert!(!requests[1].1.state.to_string().contains("col0\""));
        for (_, request) in &requests {
            request.validate().expect("requests must be valid");
        }
    }

    #[tokio::test]
    async fn a_failed_request_yields_none() {
        // The stub errors on unscripted questions; the whole profile is
        // dropped rather than half-applied.
        let columns = vec![col("a", "int"), col("b", "int")];
        let provider = StubDecisionProvider::with_answers(BTreeMap::from([(
            "c0".to_string(),
            choice("identifier", 0.9),
        )]));
        assert!(profile_columns(&provider, &columns, None).await.is_none());
    }

    #[test]
    fn suggestion_targets_skip_masked_and_impersonal() {
        let profile = vec![
            (SemanticType::Email, 0.9),
            (SemanticType::Identifier, 0.95),
            (SemanticType::Phone, 0.8),
            (SemanticType::Secret, 0.85),
        ];
        assert_eq!(suggestion_targets(&profile, &HashSet::new()), vec![0, 2, 3]);
        assert_eq!(
            suggestion_targets(&profile, &HashSet::from([2])),
            vec![0, 3]
        );
        assert!(suggestion_targets(&[], &HashSet::new()).is_empty());
    }
}
