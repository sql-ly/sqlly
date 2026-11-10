//! Deterministic, bounded execution-plan comparison.
//!
//! Node ids are assigned by the engine and can change between equivalent
//! captures, so alignment uses operator identity plus occurrence order rather
//! than ids. Comparison is pure and never executes SQL.

use std::collections::{BTreeMap, HashMap};

use crate::plans::warning::PlanWarning;
use crate::plans::{ExecutionPlan, PlanNode};

/// Default per-side limit. A pathological plan should not make opening the
/// comparison pane allocate or sort without bound.
pub const DEFAULT_MAX_COMPARISON_NODES: usize = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OperatorDeltaKind {
    Regressed,
    Improved,
    Added,
    Removed,
}

impl OperatorDeltaKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Regressed => "Regressed",
            Self::Improved => "Improved",
            Self::Added => "Added",
            Self::Removed => "Removed",
        }
    }

    pub fn message(self) -> sqlly_core::DisplayMessage {
        let (key, fallback) = match self {
            Self::Regressed => ("services.plans.comparison.delta_regressed", "Regressed"),
            Self::Improved => ("services.plans.comparison.delta_improved", "Improved"),
            Self::Added => ("services.plans.comparison.delta_added", "Added"),
            Self::Removed => ("services.plans.comparison.delta_removed", "Removed"),
        };
        sqlly_core::DisplayMessage::new(key, fallback)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct OperatorDelta {
    pub statement_index: usize,
    pub kind: OperatorDeltaKind,
    pub operator: String,
    pub baseline_node_id: Option<i32>,
    pub comparison_node_id: Option<i32>,
    pub baseline_cost: Option<f64>,
    pub comparison_cost: Option<f64>,
    pub baseline_estimated_rows: Option<f64>,
    pub comparison_estimated_rows: Option<f64>,
    pub baseline_actual_rows: Option<f64>,
    pub comparison_actual_rows: Option<f64>,
    pub warning_delta: isize,
}

impl OperatorDelta {
    pub fn cost_delta(&self) -> Option<f64> {
        Some(self.comparison_cost? - self.baseline_cost?)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanComparison {
    pub statement_count: usize,
    pub baseline_total_cost: f64,
    pub comparison_total_cost: f64,
    pub baseline_node_count: usize,
    pub comparison_node_count: usize,
    pub baseline_warning_count: usize,
    pub comparison_warning_count: usize,
    pub operator_deltas: Vec<OperatorDelta>,
}

impl PlanComparison {
    pub fn total_cost_delta(&self) -> f64 {
        self.comparison_total_cost - self.baseline_total_cost
    }

    pub fn count(&self, kind: OperatorDeltaKind) -> usize {
        self.operator_deltas
            .iter()
            .filter(|delta| delta.kind == kind)
            .count()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanComparisonError {
    Empty,
    StatementCount {
        baseline: usize,
        comparison: usize,
    },
    StatementType {
        index: usize,
        baseline: String,
        comparison: String,
    },
    TooLarge {
        side: &'static str,
        nodes: usize,
        limit: usize,
    },
}

impl PlanComparisonError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::Empty => DisplayMessage::new(
                "services.plans.comparison.errors.empty",
                "Both captures must contain at least one statement",
            ),
            Self::StatementCount {
                baseline,
                comparison,
            } => DisplayMessage::new(
                "services.plans.comparison.errors.statement_count",
                format!("The captures contain different statement counts ({baseline} vs {comparison})"),
            )
            .with_arg("baseline_count", baseline.to_string())
            .with_arg("comparison_count", comparison.to_string()),
            Self::StatementType {
                index,
                baseline,
                comparison,
            } => DisplayMessage::new(
                "services.plans.comparison.errors.statement_type",
                format!("Statement {} changed type ({baseline} vs {comparison})", index + 1),
            )
            .with_arg("statement_number", (index + 1).to_string())
            .with_arg("baseline_type", baseline)
            .with_arg("comparison_type", comparison),
            Self::TooLarge {
                side: "baseline",
                nodes,
                limit,
            } => DisplayMessage::new(
                "services.plans.comparison.errors.baseline_too_large",
                format!("The baseline capture has {nodes} operators; comparison is limited to {limit} per capture"),
            )
            .with_arg("node_count", nodes.to_string())
            .with_arg("node_limit", limit.to_string()),
            Self::TooLarge { nodes, limit, .. } => DisplayMessage::new(
                "services.plans.comparison.errors.comparison_too_large",
                format!("The comparison capture has {nodes} operators; comparison is limited to {limit} per capture"),
            )
            .with_arg("node_count", nodes.to_string())
            .with_arg("node_limit", limit.to_string()),
        }
    }
}

impl std::fmt::Display for PlanComparisonError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(
                formatter,
                "Both captures must contain at least one statement"
            ),
            Self::StatementCount {
                baseline,
                comparison,
            } => write!(
                formatter,
                "The captures contain different statement counts ({baseline} vs {comparison})"
            ),
            Self::StatementType {
                index,
                baseline,
                comparison,
            } => write!(
                formatter,
                "Statement {} changed type ({baseline} vs {comparison})",
                index + 1
            ),
            Self::TooLarge { side, nodes, limit } => write!(
                formatter,
                "The {side} capture has {nodes} operators; comparison is limited to {limit} per capture"
            ),
        }
    }
}

/// Compare two compatible capture batches without re-executing their SQL.
pub fn compare_plans(
    baseline: &[ExecutionPlan],
    comparison: &[ExecutionPlan],
    node_limit: usize,
) -> Result<PlanComparison, PlanComparisonError> {
    if baseline.is_empty() || comparison.is_empty() {
        return Err(PlanComparisonError::Empty);
    }
    if baseline.len() != comparison.len() {
        return Err(PlanComparisonError::StatementCount {
            baseline: baseline.len(),
            comparison: comparison.len(),
        });
    }
    let baseline_node_count = baseline.iter().map(|plan| plan.nodes.len()).sum();
    let comparison_node_count = comparison.iter().map(|plan| plan.nodes.len()).sum();
    if baseline_node_count > node_limit {
        return Err(PlanComparisonError::TooLarge {
            side: "baseline",
            nodes: baseline_node_count,
            limit: node_limit,
        });
    }
    if comparison_node_count > node_limit {
        return Err(PlanComparisonError::TooLarge {
            side: "comparison",
            nodes: comparison_node_count,
            limit: node_limit,
        });
    }

    let mut operator_deltas = Vec::new();
    for (statement_index, (before, after)) in baseline.iter().zip(comparison).enumerate() {
        if !before.statement_type.is_empty()
            && !after.statement_type.is_empty()
            && !before
                .statement_type
                .eq_ignore_ascii_case(&after.statement_type)
        {
            return Err(PlanComparisonError::StatementType {
                index: statement_index,
                baseline: before.statement_type.clone(),
                comparison: after.statement_type.clone(),
            });
        }
        compare_statement(statement_index, before, after, &mut operator_deltas);
    }

    operator_deltas.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| {
                b.cost_delta()
                    .unwrap_or_default()
                    .abs()
                    .total_cmp(&a.cost_delta().unwrap_or_default().abs())
            })
            .then_with(|| a.statement_index.cmp(&b.statement_index))
            .then_with(|| a.operator.cmp(&b.operator))
    });

    Ok(PlanComparison {
        statement_count: baseline.len(),
        baseline_total_cost: baseline.iter().map(|plan| plan.total_subtree_cost).sum(),
        comparison_total_cost: comparison.iter().map(|plan| plan.total_subtree_cost).sum(),
        baseline_node_count,
        comparison_node_count,
        baseline_warning_count: warning_count(baseline),
        comparison_warning_count: warning_count(comparison),
        operator_deltas,
    })
}

fn compare_statement(
    statement_index: usize,
    baseline: &ExecutionPlan,
    comparison: &ExecutionPlan,
    deltas: &mut Vec<OperatorDelta>,
) {
    let baseline_nodes = keyed_nodes(baseline);
    let comparison_nodes = keyed_nodes(comparison);
    let keys: BTreeMap<_, _> = baseline_nodes
        .keys()
        .chain(comparison_nodes.keys())
        .map(|key| (key.clone(), ()))
        .collect();

    for key in keys.keys() {
        match (baseline_nodes.get(key), comparison_nodes.get(key)) {
            (None, Some(after)) => deltas.push(node_delta(
                statement_index,
                OperatorDeltaKind::Added,
                None,
                Some(after),
            )),
            (Some(before), None) => deltas.push(node_delta(
                statement_index,
                OperatorDeltaKind::Removed,
                Some(before),
                None,
            )),
            (Some(before), Some(after)) => {
                if let Some(kind) = changed_kind(before, after) {
                    deltas.push(node_delta(statement_index, kind, Some(before), Some(after)));
                }
            }
            (None, None) => unreachable!(),
        }
    }
}

/// Stable identity: engine node ids are deliberately absent. Repeated
/// operators are disambiguated by deterministic occurrence order.
fn keyed_nodes(plan: &ExecutionPlan) -> BTreeMap<String, &PlanNode> {
    let mut occurrences: HashMap<String, usize> = HashMap::new();
    plan.nodes
        .iter()
        .map(|node| {
            let identity = operator_identity(node);
            let occurrence = occurrences.entry(identity.clone()).or_default();
            let key = format!("{identity}#{occurrence}");
            *occurrence += 1;
            (key, node)
        })
        .collect()
}

fn operator_identity(node: &PlanNode) -> String {
    let object = node.warnings.iter().find_map(|warning| match warning {
        PlanWarning::TableScan { schema, table } => Some(format!("{schema}.{table}")),
        PlanWarning::KeyLookup { table, index } => Some(format!("{table}.{index}")),
        PlanWarning::MissingIndex(index) => Some(format!("{}.{}", index.schema, index.table)),
        _ => None,
    });
    format!(
        "{}|{}|{}",
        node.logical_op.trim().to_ascii_lowercase(),
        node.physical_op.trim().to_ascii_lowercase(),
        object.unwrap_or_default().to_ascii_lowercase()
    )
}

fn changed_kind(baseline: &PlanNode, comparison: &PlanNode) -> Option<OperatorDeltaKind> {
    let warning_delta = comparison.warnings.len() as isize - baseline.warnings.len() as isize;
    let cost_delta = comparison.estimated_cost - baseline.estimated_cost;
    let tolerance = baseline.estimated_cost.abs().max(1.0) * 0.01;
    let divergence_delta = estimate_divergence(comparison) - estimate_divergence(baseline);

    if warning_delta > 0 || cost_delta > tolerance || divergence_delta > 0.25 {
        Some(OperatorDeltaKind::Regressed)
    } else if warning_delta < 0 || cost_delta < -tolerance || divergence_delta < -0.25 {
        Some(OperatorDeltaKind::Improved)
    } else {
        None
    }
}

fn estimate_divergence(node: &PlanNode) -> f64 {
    let Some(actual) = node.actual_rows else {
        return 1.0;
    };
    let high = actual.max(node.estimated_rows).max(1.0);
    let low = actual.min(node.estimated_rows).max(1.0);
    high / low
}

fn node_delta(
    statement_index: usize,
    kind: OperatorDeltaKind,
    baseline: Option<&PlanNode>,
    comparison: Option<&PlanNode>,
) -> OperatorDelta {
    let representative = comparison.or(baseline).expect("delta always has one side");
    OperatorDelta {
        statement_index,
        kind,
        operator: if representative.physical_op.is_empty() {
            representative.logical_op.clone()
        } else {
            representative.physical_op.clone()
        },
        baseline_node_id: baseline.map(|node| node.id),
        comparison_node_id: comparison.map(|node| node.id),
        baseline_cost: baseline.map(|node| node.estimated_cost),
        comparison_cost: comparison.map(|node| node.estimated_cost),
        baseline_estimated_rows: baseline.map(|node| node.estimated_rows),
        comparison_estimated_rows: comparison.map(|node| node.estimated_rows),
        baseline_actual_rows: baseline.and_then(|node| node.actual_rows),
        comparison_actual_rows: comparison.and_then(|node| node.actual_rows),
        warning_delta: comparison.map_or(0, |node| node.warnings.len() as isize)
            - baseline.map_or(0, |node| node.warnings.len() as isize),
    }
}

fn warning_count(plans: &[ExecutionPlan]) -> usize {
    plans
        .iter()
        .flat_map(|plan| &plan.nodes)
        .map(|node| node.warnings.len())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::{PlanEdge, PlanNode};

    fn node(id: i32, operator: &str, cost: f64, warnings: Vec<PlanWarning>) -> PlanNode {
        PlanNode {
            id,
            logical_op: operator.to_string(),
            physical_op: operator.to_string(),
            estimated_rows: 10.0,
            estimated_cost: cost,
            estimated_cpu: 0.0,
            estimated_io: 0.0,
            actual_rows: Some(10.0),
            actual_rewinds: None,
            actual_rebinds: None,
            row_count: None,
            output_list: Vec::new(),
            warnings,
        }
    }

    fn plan(nodes: Vec<PlanNode>) -> ExecutionPlan {
        ExecutionPlan {
            statement_text: "SELECT * FROM orders".into(),
            statement_type: "SELECT".into(),
            total_subtree_cost: nodes.iter().map(|node| node.estimated_cost).sum(),
            estimated_rows: 10.0,
            edges: Vec::<PlanEdge>::new(),
            warnings: Vec::new(),
            nodes,
        }
    }

    #[test]
    fn aligns_equivalent_operators_when_node_ids_change() {
        let baseline = plan(vec![node(1, "Index Seek", 2.0, vec![])]);
        let comparison = plan(vec![node(99, "Index Seek", 4.0, vec![])]);
        let result = compare_plans(&[baseline], &[comparison], 100).unwrap();
        assert_eq!(result.operator_deltas.len(), 1);
        assert_eq!(result.operator_deltas[0].kind, OperatorDeltaKind::Regressed);
        assert_eq!(result.operator_deltas[0].baseline_node_id, Some(1));
        assert_eq!(result.operator_deltas[0].comparison_node_id, Some(99));
    }

    #[test]
    fn reports_added_removed_regressed_and_improved() {
        let baseline = plan(vec![
            node(1, "Index Seek", 5.0, vec![]),
            node(2, "Sort", 8.0, vec![]),
            node(3, "Key Lookup", 2.0, vec![]),
        ]);
        let comparison = plan(vec![
            node(10, "Index Seek", 9.0, vec![]),
            node(20, "Sort", 2.0, vec![]),
            node(40, "Hash Match", 3.0, vec![]),
        ]);
        let result = compare_plans(&[baseline], &[comparison], 100).unwrap();
        assert_eq!(result.count(OperatorDeltaKind::Regressed), 1);
        assert_eq!(result.count(OperatorDeltaKind::Improved), 1);
        assert_eq!(result.count(OperatorDeltaKind::Added), 1);
        assert_eq!(result.count(OperatorDeltaKind::Removed), 1);
    }

    #[test]
    fn rejects_incompatible_or_oversized_batches() {
        let select = plan(vec![node(1, "Scan", 1.0, vec![])]);
        let mut update = select.clone();
        update.statement_type = "UPDATE".into();
        assert!(matches!(
            compare_plans(std::slice::from_ref(&select), &[update], 100),
            Err(PlanComparisonError::StatementType { .. })
        ));
        assert!(matches!(
            compare_plans(
                std::slice::from_ref(&select),
                std::slice::from_ref(&select),
                0,
            ),
            Err(PlanComparisonError::TooLarge { .. })
        ));
    }
}
