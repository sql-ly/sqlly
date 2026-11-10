//! Decision-model-judged join candidates for tables without declared foreign
//! keys (plan 298).
//!
//! Many production schemas declare no foreign keys at all, so every join
//! helper that walks declared relations comes up empty. This module
//! pre-filters candidate column pairs cheaply in code (type-compatible,
//! name-similar or key-like) and then asks the optional [`DecisionProvider`]
//! one yes/no question per pair in a single parallel request. Callers must
//! treat the result as *suggestions*: an inferred join is never applied
//! without the user accepting it, and a provider error yields no suggestions
//! rather than blocking the host action.
//!
//! Privacy: the request state carries only the two tables' names, column
//! names, and column types. No row data is ever sent from here.

use std::collections::BTreeMap;

use super::decision::{Answer, DecisionProvider, DecisionRequest, Question};
use super::infer::{base_type, normalize, singularize};

/// At most this many candidate pairs are judged per table pair.
pub const MAX_CANDIDATE_PAIRS: usize = 30;

/// An inferred join must reach this probability to be suggested.
pub const MIN_JOIN_PROBABILITY: f32 = 0.7;

/// At most this many inferred joins are suggested per table pair.
pub const MAX_INFERRED_JOINS: usize = 3;

/// A table viewed as join-inference input: its name and its columns as
/// `(column name, optional type name)`. Types may be unknown (`None`); the
/// pre-filter is permissive then, matching the catalog inference rules.
#[derive(Debug, Clone, PartialEq)]
pub struct TableCols {
    pub name: String,
    pub columns: Vec<(String, Option<String>)>,
}

/// One suggested join between table `a` and table `b`, with the decision
/// model's probability that the two columns refer to the same entity.
#[derive(Debug, Clone, PartialEq)]
pub struct InferredJoin {
    pub a_col: String,
    pub b_col: String,
    pub probability: f32,
}

/// Whether two optional type names are join-compatible: permissive when
/// either side is unknown, otherwise the same base family (integer, uuid,
/// text, …).
fn types_compatible(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => base_type(x) == base_type(y),
        _ => true,
    }
}

/// Whether `column` looks like a key on a table named `table`: a bare `id`,
/// or the `{table}id` / `{table}_id` form (normalization collapses
/// separators, and the singular table name counts too).
fn is_key_like(column: &str, table: &str) -> bool {
    normalize(column) == "id" || references_table(column, table)
}

/// The bare table name: the last dot-separated segment, so a
/// schema-qualified `dbo.Customers` still matches a `CustomerId` column.
fn bare_table_name(table: &str) -> &str {
    table.rsplit('.').next().unwrap_or(table)
}

/// Whether `column` on one table names the *other* table's key — the
/// strongest naming signal (`Orders.CustomerId` against `Customer`).
fn references_table(column: &str, other_table: &str) -> bool {
    let column = normalize(column);
    let other = normalize(bare_table_name(other_table));
    column == format!("{other}id") || column == format!("{}id", singularize(&other))
}

/// Ranking score for a qualifying pair; higher sorts first.
fn pair_score(a: &TableCols, b: &TableCols, i: usize, j: usize) -> u32 {
    let (a_col, _) = &a.columns[i];
    let (b_col, _) = &b.columns[j];
    let mut score = 0;
    if normalize(a_col) == normalize(b_col) {
        score += 4;
    }
    if references_table(a_col, &b.name) || references_table(b_col, &a.name) {
        score += 3;
    }
    let a_key = is_key_like(a_col, &a.name);
    let b_key = is_key_like(b_col, &b.name);
    if a_key && b_key {
        score += 2;
    } else if a_key || b_key {
        score += 1;
    }
    // The specific `{table}id` form outranks a bare `id`.
    if references_table(a_col, &a.name) || references_table(b_col, &b.name) {
        score += 1;
    }
    score
}

/// Candidate column pairs between `a` and `b` worth judging: type-compatible,
/// and either one side is key-like or the names are equal after
/// normalization. Best name matches come first; at most
/// [`MAX_CANDIDATE_PAIRS`] pairs are returned.
pub fn candidate_pairs(a: &TableCols, b: &TableCols) -> Vec<(usize, usize)> {
    let mut scored: Vec<(u32, usize, usize)> = Vec::new();
    for (i, (a_col, a_type)) in a.columns.iter().enumerate() {
        for (j, (b_col, b_type)) in b.columns.iter().enumerate() {
            if !types_compatible(a_type.as_deref(), b_type.as_deref()) {
                continue;
            }
            let name_equal = normalize(a_col) == normalize(b_col);
            if !name_equal && !is_key_like(a_col, &a.name) && !is_key_like(b_col, &b.name) {
                continue;
            }
            scored.push((pair_score(a, b, i, j), i, j));
        }
    }
    scored.sort_by(|x, y| y.0.cmp(&x.0).then(x.1.cmp(&y.1)).then(x.2.cmp(&y.2)));
    scored
        .into_iter()
        .take(MAX_CANDIDATE_PAIRS)
        .map(|(_, i, j)| (i, j))
        .collect()
}

/// The JSON state entry for one table: its name plus each column's name and
/// type. Nothing else — irrelevant state degrades answers, and everything
/// here may leave the machine.
fn table_state(table: &TableCols) -> serde_json::Value {
    serde_json::json!({
        "table": table.name,
        "columns": table
            .columns
            .iter()
            .map(|(name, type_name)| {
                serde_json::json!({ "name": name, "type": type_name })
            })
            .collect::<Vec<_>>(),
    })
}

/// The question id for candidate pair `index`.
fn question_id(index: usize) -> String {
    format!("p{index}")
}

/// Judge the candidate pairs between `a` and `b` with one yes/no question
/// each, in a single request. Keeps answers at or above
/// [`MIN_JOIN_PROBABILITY`], sorted by probability descending, at most
/// [`MAX_INFERRED_JOINS`]. No candidates, a provider error, or a malformed
/// response all yield an empty list — the caller treats that as "no
/// suggestions" and never blocks on this.
pub async fn infer_joins(
    decider: &dyn DecisionProvider,
    a: &TableCols,
    b: &TableCols,
) -> Vec<InferredJoin> {
    let pairs = candidate_pairs(a, b);
    if pairs.is_empty() {
        return Vec::new();
    }
    let state = serde_json::json!({
        "table_a": table_state(a),
        "table_b": table_state(b),
    });
    let mut questions = BTreeMap::new();
    for (index, (i, j)) in pairs.iter().enumerate() {
        let instructions = format!(
            "Column {}.{} refers to the same entity as column {}.{}, so rows can be joined on \
             them.",
            a.name, a.columns[*i].0, b.name, b.columns[*j].0,
        );
        questions.insert(question_id(index), Question::Noul { instructions });
    }
    let Ok(response) = decider.decide(DecisionRequest { state, questions }).await else {
        return Vec::new();
    };
    let mut out: Vec<InferredJoin> = Vec::new();
    for (index, (i, j)) in pairs.iter().enumerate() {
        let Some(Answer::Noul { probability }) = response.answers.get(&question_id(index)) else {
            continue;
        };
        if *probability < MIN_JOIN_PROBABILITY {
            continue;
        }
        out.push(InferredJoin {
            a_col: a.columns[*i].0.clone(),
            b_col: b.columns[*j].0.clone(),
            probability: *probability,
        });
    }
    out.sort_by(|x, y| y.probability.total_cmp(&x.probability));
    out.truncate(MAX_INFERRED_JOINS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::decision::StubDecisionProvider;

    fn table(name: &str, columns: &[(&str, Option<&str>)]) -> TableCols {
        TableCols {
            name: name.to_string(),
            columns: columns
                .iter()
                .map(|(col, ty)| (col.to_string(), ty.map(|t| t.to_string())))
                .collect(),
        }
    }

    #[test]
    fn prefilter_rejects_type_mismatch() {
        let orders = table("Orders", &[("CustomerId", Some("uniqueidentifier"))]);
        let customers = table("Customers", &[("CustomerId", Some("int"))]);
        assert!(
            candidate_pairs(&orders, &customers).is_empty(),
            "uuid vs int must not pair"
        );
        // Same names with compatible types do pair.
        let orders_int = table("Orders", &[("CustomerId", Some("bigint"))]);
        assert_eq!(candidate_pairs(&orders_int, &customers), vec![(0, 0)]);
        // Unknown types are permissive.
        let orders_untyped = table("Orders", &[("CustomerId", None)]);
        assert_eq!(candidate_pairs(&orders_untyped, &customers), vec![(0, 0)]);
    }

    #[test]
    fn caps_at_30_pairs() {
        // A bare `Id` on one side is key-like, so it pairs with every
        // type-compatible column on the other side: 2 key-like columns × 31
        // other columns would exceed the cap without it.
        let a_cols: Vec<(String, Option<String>)> = (0..31)
            .map(|i| (format!("Value{i}"), Some("int".to_string())))
            .collect();
        let a = TableCols {
            name: "Widgets".to_string(),
            columns: a_cols,
        };
        let b = table("Gadgets", &[("Id", Some("int")), ("GadgetId", Some("int"))]);
        let pairs = candidate_pairs(&a, &b);
        assert_eq!(pairs.len(), MAX_CANDIDATE_PAIRS);
        // Best name matches first: the specific `{table}id` outranks bare `Id`.
        let (_, j) = pairs[0];
        assert_eq!(b.columns[j].0, "GadgetId");
    }

    #[test]
    fn schema_qualified_names_still_match() {
        // The query builder passes schema-qualified names; `OrderId` must
        // still count as key-like on `dbo.Orders`.
        let orders = table("dbo.Orders", &[("OrderId", Some("int"))]);
        let shipments = table("dbo.Shipments", &[("OrderRef", Some("int"))]);
        assert_eq!(candidate_pairs(&orders, &shipments), vec![(0, 0)]);
    }

    #[tokio::test]
    async fn keeps_only_high_probability() {
        let a = table(
            "Orders",
            &[
                ("CustomerId", Some("int")),
                ("RegionId", Some("int")),
                ("StatusId", Some("int")),
            ],
        );
        let b = table("Customers", &[("Id", Some("int"))]);
        let pairs = candidate_pairs(&a, &b);
        assert_eq!(pairs.len(), 3);
        // CustomerId references the Customers table, so it ranks first.
        assert_eq!(a.columns[pairs[0].0].0, "CustomerId");
        let scripted = BTreeMap::from([
            ("p0".to_string(), Answer::Noul { probability: 0.92 }),
            ("p1".to_string(), Answer::Noul { probability: 0.4 }),
            ("p2".to_string(), Answer::Noul { probability: 0.75 }),
        ]);
        let stub = StubDecisionProvider::with_answers(scripted);
        let joins = infer_joins(&stub, &a, &b).await;
        assert_eq!(joins.len(), 2, "0.4 must be dropped: {joins:?}");
        assert_eq!(joins[0].a_col, "CustomerId");
        assert!((joins[0].probability - 0.92).abs() < f32::EPSILON);
        assert_eq!(joins[0].b_col, "Id");
        assert!((joins[1].probability - 0.75).abs() < f32::EPSILON);

        // A provider error yields no suggestions rather than failing.
        let empty = StubDecisionProvider::default();
        assert!(infer_joins(&empty, &a, &b).await.is_empty());
    }
}
