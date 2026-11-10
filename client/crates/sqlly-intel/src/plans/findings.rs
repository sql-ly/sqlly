//! Human-first plan findings: translate [`PlanWarning`]s and operator stats
//! into plain-language, actionable findings ranked by how much they matter.
//!
//! The viewer's default layer renders only these summaries; the raw operator
//! numbers stay one expansion away via [`operator_rows`].

use crate::plans::warning::{MissingIndex, PlanWarning};
use crate::plans::{ExecutionPlan, PlanNode};
use sqlly_core::DisplayMessage;

fn message(key: &'static str, fallback: impl Into<String>) -> DisplayMessage {
    DisplayMessage::new(key, fallback)
}

/// How loudly a finding should be signaled. Maps onto the app's semantic
/// status palette (info / warning / error) — the only color the plan view
/// is allowed to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingSeverity {
    Info,
    Warning,
    Critical,
}

/// A single plain-language finding with its fix and the technical detail
/// shown on expansion.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanFinding {
    pub severity: FindingSeverity,
    /// One sentence, no jargon: what is wrong, in terms a tired developer
    /// can act on at 2am.
    pub summary: DisplayMessage,
    /// Why it matters (one or two sentences).
    pub why: DisplayMessage,
    /// The suggested next step. Concrete SQL where the plan provides enough
    /// information to write it.
    pub action: DisplayMessage,
    /// Runnable DDL for Copy SQL / Open in new tab, when the finding names
    /// enough identifiers to generate it.
    pub action_sql: Option<String>,
    /// Technical detail shown on expansion: operator, node, estimates, and
    /// the raw warning fields.
    pub detail: DisplayMessage,
    /// Operator the finding is attached to, when known.
    pub node_id: Option<i32>,
    /// Sort weight — higher ranks first. Combines severity with the
    /// operator's share of statement cost so the top finding is usually the
    /// most expensive fixable problem.
    pub rank: f64,
}

/// One row of the operators list: a compact human summary with the full
/// property set available on expansion.
#[derive(Debug, Clone, PartialEq)]
pub struct OperatorRow {
    pub node_id: i32,
    /// Physical operator name ("Index Seek").
    pub title: String,
    /// One-line human summary ("Index Seek — 4 rows estimated, 91% of cost").
    pub summary: DisplayMessage,
    /// Full technical detail for the expanded state.
    pub detail: DisplayMessage,
    pub cost_percent: f64,
    pub estimated_rows: f64,
    pub actual_rows: Option<f64>,
    pub has_warnings: bool,
}

/// Estimated-vs-actual divergence that qualifies as a mismatch finding.
const MISMATCH_RATIO: f64 = 10.0;

/// Build the ranked findings list for one statement's plan.
pub fn findings_for_plan(plan: &ExecutionPlan) -> Vec<PlanFinding> {
    let mut findings = Vec::new();
    let mut seen_missing_indexes: Vec<String> = Vec::new();

    for node in &plan.nodes {
        let cost_share = cost_percent(plan, node);
        for warning in &node.warnings {
            match warning {
                PlanWarning::MissingIndex(index) => {
                    // The same index suggestion repeats on every operator of
                    // the statement; report it once, at the highest impact.
                    let key = format!(
                        "{}.{}.{}|{:?}|{:?}",
                        index.database,
                        index.schema,
                        index.table,
                        index.equality_columns,
                        index.inequality_columns
                    );
                    if seen_missing_indexes.contains(&key) {
                        continue;
                    }
                    seen_missing_indexes.push(key);
                    findings.push(missing_index_finding(index, node, cost_share));
                }
                PlanWarning::TableScan { table, schema } => {
                    findings.push(table_scan_finding(table, schema, node, cost_share));
                }
                PlanWarning::NoJoinPredicate => {
                    findings.push(PlanFinding {
                        severity: FindingSeverity::Critical,
                        summary: message(
                            "services.plans.findings.no_join.summary",
                            "A join has no ON condition, so every row pairs with every row",
                        ),
                        why: message(
                            "services.plans.findings.no_join.why",
                            "Without a join predicate the result is a cross product: two 1,000-row tables produce 1,000,000 rows before any filter runs.",
                        ),
                        action: message(
                            "services.plans.findings.no_join.action",
                            "Add an ON clause that relates the two tables. If the full cross product is intended, write CROSS JOIN explicitly.",
                        ),
                        action_sql: None,
                        detail: message(
                            "services.plans.findings.no_join.detail",
                            format!(
                                "Operator: {} (node {})\nNoJoinPredicate=true\nCost share: {:.0}%",
                                display_op(node), node.id, cost_share,
                            ),
                        )
                        .with_arg("operator", display_op(node))
                        .with_arg("node_id", node.id.to_string())
                        .with_arg("cost_percent", format!("{cost_share:.0}")),
                        node_id: Some(node.id),
                        rank: 3000.0 + cost_share,
                    });
                }
                PlanWarning::ImplicitConversion { expression } => {
                    let summary = if expression.is_empty() {
                        message(
                            "services.plans.findings.implicit_conversion.summary_value",
                            "Converts a value on every row, which blocks index seeks",
                        )
                    } else {
                        message(
                            "services.plans.findings.implicit_conversion.summary_expression",
                            format!(
                                "Converts `{expression}` on every row, which blocks index seeks"
                            ),
                        )
                        .with_arg("expression", expression)
                    };
                    findings.push(PlanFinding {
                        severity: FindingSeverity::Warning,
                        summary,
                        why: message(
                            "services.plans.findings.implicit_conversion.why",
                            "A comparison that changes a column's type cannot seek an index; the index is scanned and every row is converted before it can be compared.",
                        ),
                        action: message(
                            "services.plans.findings.implicit_conversion.action",
                            "Match the parameter or compared column's data type to the column's type so the predicate stays seekable.",
                        ),
                        action_sql: None,
                        detail: message(
                            "services.plans.findings.implicit_conversion.detail",
                            format!(
                                "Operator: {} (node {})\nExpression: {}\nCost share: {:.0}%",
                                display_op(node),
                                node.id,
                                if expression.is_empty() { "—" } else { expression },
                                cost_share,
                            ),
                        )
                        .with_arg("operator", display_op(node))
                        .with_arg("node_id", node.id.to_string())
                        .with_arg("expression", if expression.is_empty() { "—" } else { expression })
                        .with_arg("cost_percent", format!("{cost_share:.0}")),
                        node_id: Some(node.id),
                        rank: 2000.0 + cost_share,
                    });
                }
                PlanWarning::KeyLookup { table, index } => {
                    let table_name = if table.is_empty() { "—" } else { table };
                    let action = if index.is_empty() {
                        message(
                            "services.plans.findings.key_lookup.action_unknown_index",
                            "Add the selected columns as INCLUDE columns on the index, or select fewer columns.",
                        )
                    } else {
                        message(
                            "services.plans.findings.key_lookup.action_named_index",
                            format!("Add the selected columns as INCLUDE columns on {index}, or select fewer columns."),
                        )
                        .with_arg("index_name", index)
                    };
                    findings.push(PlanFinding {
                        severity: FindingSeverity::Info,
                        summary: message(
                            "services.plans.findings.key_lookup.summary",
                            format!("Seeks the index, then re-reads each matching row from {table_name}"),
                        )
                        .with_arg("table_name", table_name),
                        why: message(
                            "services.plans.findings.key_lookup.why",
                            "The index locates the rows but does not contain every column the query selects, so each match costs an extra read against the base table.",
                        ),
                        action,
                        action_sql: if !index.is_empty() && !table.is_empty() {
                            Some(format!(
                                "-- Add the columns this query selects to the INCLUDE list, then run:\nCREATE NONCLUSTERED INDEX {} ON {} (/* key columns */) INCLUDE (/* selected columns */) WITH (DROP_EXISTING = ON);",
                                quote_ident(index),
                                quote_ident(table),
                            ))
                        } else {
                            None
                        },
                        detail: message(
                            "services.plans.findings.key_lookup.detail",
                            format!(
                                "Operator: {} (node {})\nTable: {}\nIndex: {}\nCost share: {:.0}%",
                                display_op(node), node.id, table_name,
                                if index.is_empty() { "—" } else { index }, cost_share,
                            ),
                        )
                        .with_arg("operator", display_op(node))
                        .with_arg("node_id", node.id.to_string())
                        .with_arg("table_name", table_name)
                        .with_arg("index_name", if index.is_empty() { "—" } else { index })
                        .with_arg("cost_percent", format!("{cost_share:.0}")),
                        node_id: Some(node.id),
                        rank: 1000.0 + cost_share,
                    });
                }
                PlanWarning::SpillToTempDb => {
                    findings.push(PlanFinding {
                        severity: FindingSeverity::Warning,
                        summary: message(
                            "services.plans.findings.spill.summary",
                            format!("{} spills to tempdb because its memory grant is too small", display_op(node)),
                        )
                        .with_arg("operator", display_op(node)),
                        why: message(
                            "services.plans.findings.spill.why",
                            "Writing intermediate results to disk is orders of magnitude slower than keeping them in memory.",
                        ),
                        action: message(
                            "services.plans.findings.spill.action",
                            "Update statistics so the grant estimate improves, reduce the rows reaching this operator, or simplify the sort/distinct.",
                        ),
                        action_sql: None,
                        detail: message(
                            "services.plans.findings.spill.detail",
                            format!(
                                "Operator: {} (node {})\nSpillToTempDb\nEstimated rows: {}\nCost share: {:.0}%",
                                display_op(node), node.id, human_count(node.estimated_rows), cost_share,
                            ),
                        )
                        .with_arg("operator", display_op(node))
                        .with_arg("node_id", node.id.to_string())
                        .with_arg("estimated_rows", human_count(node.estimated_rows))
                        .with_arg("cost_percent", format!("{cost_share:.0}")),
                        node_id: Some(node.id),
                        rank: 2000.0 + cost_share,
                    });
                }
                PlanWarning::CardinalityEstimation => {
                    findings.push(PlanFinding {
                        severity: FindingSeverity::Info,
                        summary: message(
                            "services.plans.findings.cardinality.summary",
                            "The optimizer has no statistics for at least one column here",
                        ),
                        why: message(
                            "services.plans.findings.cardinality.why",
                            "Without statistics the row estimates are guesses, and join and access-method choices built on them can be badly wrong.",
                        ),
                        action: message(
                            "services.plans.findings.cardinality.action",
                            "Create or update statistics on the filtered and joined columns.",
                        ),
                        action_sql: None,
                        detail: message(
                            "services.plans.findings.cardinality.detail",
                            format!(
                                "Operator: {} (node {})\nColumnsWithNoStatistics\nCost share: {:.0}%",
                                display_op(node), node.id, cost_share,
                            ),
                        )
                        .with_arg("operator", display_op(node))
                        .with_arg("node_id", node.id.to_string())
                        .with_arg("cost_percent", format!("{cost_share:.0}")),
                        node_id: Some(node.id),
                        rank: 1000.0 + cost_share,
                    });
                }
                PlanWarning::RowGoal => {
                    findings.push(PlanFinding {
                        severity: FindingSeverity::Info,
                        summary: message(
                            "services.plans.findings.row_goal.summary",
                            "Uses a row goal, which favors stopping early over total cost",
                        ),
                        why: message(
                            "services.plans.findings.row_goal.why",
                            "Row goals (TOP, EXISTS, FAST n) make the optimizer choose plans that return the first rows quickly, sometimes at a higher total cost.",
                        ),
                        action: message(
                            "services.plans.findings.row_goal.action",
                            "Usually fine. If the full result is slow, check the TOP or EXISTS clause driving it.",
                        ),
                        action_sql: None,
                        detail: message(
                            "services.plans.findings.row_goal.detail",
                            format!("Operator: {} (node {})\nCost share: {:.0}%", display_op(node), node.id, cost_share),
                        )
                        .with_arg("operator", display_op(node))
                        .with_arg("node_id", node.id.to_string())
                        .with_arg("cost_percent", format!("{cost_share:.0}")),
                        node_id: Some(node.id),
                        rank: 500.0 + cost_share,
                    });
                }
                PlanWarning::Spool => {
                    findings.push(PlanFinding {
                        severity: FindingSeverity::Info,
                        summary: message(
                            "services.plans.findings.spool.summary",
                            "Caches intermediate rows in a spool",
                        ),
                        why: message(
                            "services.plans.findings.spool.why",
                            "A spool re-reads rows cheaply, which is usually the optimizer helping a repeated lookup — but on the hot path it can hide a missing index.",
                        ),
                        action: message(
                            "services.plans.findings.spool.action",
                            "Usually harmless. Investigate only if this operator carries most of the cost.",
                        ),
                        action_sql: None,
                        detail: message(
                            "services.plans.findings.spool.detail",
                            format!("Operator: {} (node {})\nCost share: {:.0}%", display_op(node), node.id, cost_share),
                        )
                        .with_arg("operator", display_op(node))
                        .with_arg("node_id", node.id.to_string())
                        .with_arg("cost_percent", format!("{cost_share:.0}")),
                        node_id: Some(node.id),
                        rank: 500.0 + cost_share,
                    });
                }
                PlanWarning::Other(text) => {
                    findings.push(PlanFinding {
                        severity: FindingSeverity::Info,
                        summary: message(
                            "services.plans.findings.other.summary",
                            format!("Optimizer warning: {text}"),
                        )
                        .with_arg("warning", text),
                        why: message(
                            "services.plans.findings.other.why",
                            "The optimizer flagged something it could not classify.",
                        ),
                        action: message(
                            "services.plans.findings.other.action",
                            "Inspect the operator's properties for details.",
                        ),
                        action_sql: None,
                        detail: message(
                            "services.plans.findings.other.detail",
                            format!(
                                "Operator: {} (node {})\nWarning: {text}\nCost share: {:.0}%",
                                display_op(node),
                                node.id,
                                cost_share
                            ),
                        )
                        .with_arg("operator", display_op(node))
                        .with_arg("node_id", node.id.to_string())
                        .with_arg("warning", text)
                        .with_arg("cost_percent", format!("{cost_share:.0}")),
                        node_id: Some(node.id),
                        rank: 500.0 + cost_share,
                    });
                }
            }
        }

        // Actual plans only: estimates far from reality deserve their own
        // finding, since they point at stale statistics rather than a
        // structural problem.
        if let Some(actual) = node.actual_rows {
            let estimated = node.estimated_rows;
            if divergence_ratio(estimated, actual) >= MISMATCH_RATIO {
                findings.push(PlanFinding {
                    severity: FindingSeverity::Warning,
                    summary: message(
                        "services.plans.findings.estimate_mismatch.summary",
                        format!("Estimated {} rows but read {}", human_count(estimated), human_count(actual)),
                    )
                    .with_arg("estimated_rows", human_count(estimated))
                    .with_arg("actual_rows", human_count(actual)),
                    why: message(
                        "services.plans.findings.estimate_mismatch.why",
                        "Estimates this far off usually mean stale statistics, so the optimizer may be choosing the wrong join order or access method for the real data volume.",
                    ),
                    action: message(
                        "services.plans.findings.estimate_mismatch.action",
                        "Run UPDATE STATISTICS on the tables this operator reads, then re-check the plan.",
                    ),
                    action_sql: None,
                    detail: message(
                        "services.plans.findings.estimate_mismatch.detail",
                        format!(
                            "Operator: {} (node {})\nEstimated rows: {}\nActual rows: {}\nDivergence: {:.0}×\nCost share: {:.0}%",
                            display_op(node), node.id, human_count(estimated), human_count(actual),
                            divergence_ratio(estimated, actual), cost_share,
                        ),
                    )
                    .with_arg("operator", display_op(node))
                    .with_arg("node_id", node.id.to_string())
                    .with_arg("estimated_rows", human_count(estimated))
                    .with_arg("actual_rows", human_count(actual))
                    .with_arg("divergence", format!("{:.0}", divergence_ratio(estimated, actual)))
                    .with_arg("cost_percent", format!("{cost_share:.0}")),
                    node_id: Some(node.id),
                    rank: 2000.0 + cost_share,
                });
            }
        }
    }

    findings.sort_by(|a, b| {
        b.rank
            .total_cmp(&a.rank)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    findings
}

/// The operators list, most expensive first, with human one-liners.
pub fn operator_rows(plan: &ExecutionPlan) -> Vec<OperatorRow> {
    let mut rows: Vec<OperatorRow> = plan
        .nodes
        .iter()
        .map(|node| {
            let cost_share = cost_percent(plan, node);
            let title = display_op(node);
            let estimated_rows = human_count(node.estimated_rows);
            let cost = format!("{cost_share:.0}");
            let summary = if let Some(actual) = node.actual_rows {
                message(
                    "services.plans.operators.summary_actual",
                    format!(
                        "{} — {} rows estimated, {:.0}% of cost, read {}",
                        title,
                        estimated_rows,
                        cost_share,
                        human_count(actual),
                    ),
                )
                .with_arg("operator", title.clone())
                .with_arg("estimated_rows", estimated_rows)
                .with_arg("cost_percent", cost)
                .with_arg("actual_rows", human_count(actual))
            } else {
                message(
                    "services.plans.operators.summary_estimated",
                    format!(
                        "{} — {} rows estimated, {:.0}% of cost",
                        title, estimated_rows, cost_share,
                    ),
                )
                .with_arg("operator", title.clone())
                .with_arg("estimated_rows", estimated_rows)
                .with_arg("cost_percent", cost)
            };
            OperatorRow {
                node_id: node.id,
                title,
                summary,
                detail: operator_detail(plan, node),
                cost_percent: cost_share,
                estimated_rows: node.estimated_rows,
                actual_rows: node.actual_rows,
                has_warnings: !node.warnings.is_empty(),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.cost_percent
            .total_cmp(&a.cost_percent)
            .then_with(|| a.node_id.cmp(&b.node_id))
    });
    rows
}

/// How far apart an estimate and an actual row count are, expressed as a
/// ratio ≥ 1. A zero on either side is clamped to 1 so the ratio stays
/// finite (an estimate of 100 against 0 actual rows reads as "100× off",
/// not "inf× off").
fn divergence_ratio(estimated: f64, actual: f64) -> f64 {
    let larger = estimated.max(actual);
    let smaller = estimated.min(actual).max(1.0);
    larger / smaller
}

/// Human-readable row counts: exact with grouping below 10k ("8,210"),
/// compact three-significant-figure above ("48.2K", "1.2M", "3.4B").
/// Precise enough to compare, short enough to scan.
pub fn human_count(n: f64) -> String {
    if !n.is_finite() || n < 0.0 {
        return "0".to_string();
    }
    if n < 10_000.0 {
        return grouped(n.round() as u64);
    }
    let (scaled, suffix) = if n >= 1_000_000_000.0 {
        (n / 1_000_000_000.0, "B")
    } else if n >= 1_000_000.0 {
        (n / 1_000_000.0, "M")
    } else {
        (n / 1_000.0, "K")
    };
    if scaled >= 100.0 {
        format!("{scaled:.0}{suffix}")
    } else if scaled >= 10.0 {
        format!("{scaled:.1}{suffix}")
    } else {
        format!("{scaled:.2}{suffix}")
    }
}

fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn cost_percent(plan: &ExecutionPlan, node: &PlanNode) -> f64 {
    if plan.total_subtree_cost > 0.0 {
        (node.estimated_cost / plan.total_subtree_cost * 100.0).max(0.0)
    } else {
        0.0
    }
}

/// Physical op when present, else logical op.
fn display_op(node: &PlanNode) -> String {
    if node.physical_op.is_empty() {
        node.logical_op.clone()
    } else {
        node.physical_op.clone()
    }
}

fn qualified_table(schema: &str, table: &str) -> String {
    match (schema.is_empty(), table.is_empty()) {
        (false, false) => format!("{schema}.{table}"),
        (true, false) => table.to_string(),
        _ => "—".to_string(),
    }
}

/// Re-bracket an identifier for generated SQL: strips any existing brackets,
/// escapes inner `]`, and quotes each dotted part, so names with spaces or
/// punctuation still produce valid statements.
fn quote_ident(name: &str) -> String {
    crate::plans::parser::strip_brackets(name)
        .split('.')
        .map(|part| format!("[{}]", part.replace(']', "]]")))
        .collect::<Vec<_>>()
        .join(".")
}

fn missing_index_finding(index: &MissingIndex, node: &PlanNode, cost_share: f64) -> PlanFinding {
    let table = qualified_table(
        &crate::plans::parser::strip_brackets(&index.schema),
        &crate::plans::parser::strip_brackets(&index.table),
    );
    let mut key_columns = index.equality_columns.clone();
    key_columns.extend(index.inequality_columns.iter().cloned());

    let summary = if key_columns.is_empty() {
        message(
            "services.plans.findings.missing_index.summary_without_columns",
            format!("Add an index on {table}"),
        )
        .with_arg("table_name", table.clone())
    } else {
        message(
            "services.plans.findings.missing_index.summary_with_columns",
            format!("Add an index on {table} ({})", key_columns.join(", ")),
        )
        .with_arg("table_name", table.clone())
        .with_arg("columns", key_columns.join(", "))
    };
    let impact = index.impact.round() as u32;
    let why = if impact > 0 {
        message(
            "services.plans.findings.missing_index.why_with_impact",
            format!("The optimizer estimates this index would improve this statement by about {impact}%. It computed the index it wanted and could not find it."),
        )
        .with_arg("impact_percent", impact.to_string())
    } else {
        message(
            "services.plans.findings.missing_index.why_without_impact",
            "The optimizer computed an index it wanted for this statement and could not find it.",
        )
    };
    let (action, action_sql) = if key_columns.is_empty() {
        (
            message(
                "services.plans.findings.missing_index.action_without_columns",
                format!("Create a nonclustered index on {table} covering the filtered and joined columns."),
            )
            .with_arg("table_name", table.clone()),
            None,
        )
    } else {
        let stripped_table = crate::plans::parser::strip_brackets(&index.table);
        let stripped_schema = crate::plans::parser::strip_brackets(&index.schema);
        let stripped_cols: Vec<String> = key_columns
            .iter()
            .map(|c| crate::plans::parser::strip_brackets(c))
            .collect();
        let stripped_includes: Vec<String> = index
            .included_columns
            .iter()
            .map(|c| crate::plans::parser::strip_brackets(c))
            .collect();
        let index_name = format!("IX_{}_{}", stripped_table, stripped_cols.join("_"));
        let sql = crate::plans::IndexDdl {
            schema: stripped_schema,
            table: stripped_table,
            name: index_name,
            columns: stripped_cols,
            include_columns: stripped_includes,
            unique: false,
            nonclustered: true,
        }
        .render(crate::plans::IndexDdlDialect::SqlServer);
        (DisplayMessage::raw(sql.clone()), Some(sql))
    };
    let database = if index.database.is_empty() {
        "—"
    } else {
        index.database.as_str()
    };
    let detail = message(
        "services.plans.findings.missing_index.detail",
        format!(
            "Operator: {} (node {})\nDatabase: {}\nTable: {}\nEquality columns: {}\nInequality columns: {}\nIncluded columns: {}\nImpact: {:.1}%\nCost share: {:.0}%",
            display_op(node), node.id, database, table, or_none(&index.equality_columns),
            or_none(&index.inequality_columns), or_none(&index.included_columns), index.impact, cost_share,
        ),
    )
    .with_arg("operator", display_op(node))
    .with_arg("node_id", node.id.to_string())
    .with_arg("database_name", database)
    .with_arg("table_name", table)
    .with_arg("equality_columns", or_none(&index.equality_columns))
    .with_arg("inequality_columns", or_none(&index.inequality_columns))
    .with_arg("included_columns", or_none(&index.included_columns))
    .with_arg("impact_percent", format!("{:.1}", index.impact))
    .with_arg("cost_percent", format!("{cost_share:.0}"));
    PlanFinding {
        severity: if index.impact >= 90.0 {
            FindingSeverity::Critical
        } else {
            FindingSeverity::Warning
        },
        summary,
        why,
        action,
        action_sql,
        detail,
        node_id: Some(node.id),
        rank: if index.impact >= 90.0 {
            3000.0 + index.impact
        } else {
            2000.0 + index.impact
        },
    }
}

fn table_scan_finding(table: &str, schema: &str, node: &PlanNode, cost_share: f64) -> PlanFinding {
    let table_name = qualified_table(schema, table);
    let rows = human_count(node.estimated_rows);
    PlanFinding {
        severity: FindingSeverity::Warning,
        summary: message(
            "services.plans.findings.table_scan.summary",
            format!("Scans every row in {table_name} ({rows} rows) to find matches"),
        )
        .with_arg("table_name", table_name.clone())
        .with_arg("estimated_rows", rows.clone()),
        why: message(
            "services.plans.findings.table_scan.why",
            "A table scan has no index to seek into, so it reads the whole table every time. Cost grows with table size even when the query returns one row.",
        ),
        action: message(
            "services.plans.findings.table_scan.action",
            "If this filter runs often, add an index on the filtered or joined columns so the scan becomes a seek.",
        ),
        action_sql: None,
        detail: message(
            "services.plans.findings.table_scan.detail",
            format!(
                "Operator: {} (node {})\nTable: {}\nEstimated rows: {}\nCost share: {:.0}%",
                display_op(node),
                node.id,
                table_name,
                rows,
                cost_share,
            ),
        )
        .with_arg("operator", display_op(node))
        .with_arg("node_id", node.id.to_string())
        .with_arg("table_name", table_name)
        .with_arg("estimated_rows", rows)
        .with_arg("cost_percent", format!("{cost_share:.0}")),
        node_id: Some(node.id),
        rank: 2000.0 + cost_share,
    }
}

fn operator_detail(plan: &ExecutionPlan, node: &PlanNode) -> DisplayMessage {
    let cost_share = cost_percent(plan, node);
    let actual = node
        .actual_rows
        .map(human_count)
        .unwrap_or_else(|| "—".into());
    let warnings = if node.warnings.is_empty() {
        "—".into()
    } else {
        format!("{:?}", node.warnings)
    };
    let output = if node.output_list.is_empty() {
        "—".to_string()
    } else {
        node.output_list
            .iter()
            .map(|column| format!("- {column}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let fallback = format!(
        "Node {}\nPhysical op: {}\nLogical op: {}\nEstimated cost: {:.4} ({:.0}% of statement)\nEstimated rows: {}\nActual rows: {}\nEstimated CPU: {:.4}\nEstimated I/O: {:.4}\nWarnings: {}\nOutput\n{}",
        node.id,
        display_op(node),
        node.logical_op,
        node.estimated_cost,
        cost_share,
        human_count(node.estimated_rows),
        actual,
        node.estimated_cpu,
        node.estimated_io,
        warnings,
        output,
    );
    message("services.plans.operators.detail", fallback)
        .with_arg("node_id", node.id.to_string())
        .with_arg("physical_operator", display_op(node))
        .with_arg("logical_operator", node.logical_op.clone())
        .with_arg("estimated_cost", format!("{:.4}", node.estimated_cost))
        .with_arg("cost_percent", format!("{cost_share:.0}"))
        .with_arg("estimated_rows", human_count(node.estimated_rows))
        .with_arg("actual_rows", actual)
        .with_arg("estimated_cpu", format!("{:.4}", node.estimated_cpu))
        .with_arg("estimated_io", format!("{:.4}", node.estimated_io))
        .with_arg("warnings", warnings)
        .with_arg("output_list", output)
}

fn or_none(columns: &[String]) -> String {
    if columns.is_empty() {
        "—".to_string()
    } else {
        columns.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::parser::PlanParser;
    use crate::plans::warning::MissingIndex;

    fn plan_with(warning: PlanWarning, physical_op: &str) -> ExecutionPlan {
        ExecutionPlan {
            statement_text: "SELECT 1".to_string(),
            statement_type: "SELECT".to_string(),
            total_subtree_cost: 2.0,
            estimated_rows: 100.0,
            warnings: vec![warning.clone()],
            edges: vec![],
            nodes: vec![PlanNode {
                id: 1,
                logical_op: "Op".to_string(),
                physical_op: physical_op.to_string(),
                estimated_rows: 100.0,
                estimated_cost: 1.5,
                estimated_cpu: 0.1,
                estimated_io: 0.2,
                actual_rows: None,
                actual_rewinds: None,
                actual_rebinds: None,
                row_count: None,
                output_list: vec!["a".to_string()],
                warnings: vec![warning],
            }],
        }
    }

    #[test]
    fn missing_index_with_high_impact_is_critical_and_actionable() {
        let plan = plan_with(
            PlanWarning::MissingIndex(MissingIndex {
                database: "[db]".to_string(),
                schema: "[dbo]".to_string(),
                table: "[Orders]".to_string(),
                equality_columns: vec!["CustomerId".to_string()],
                inequality_columns: vec![],
                included_columns: vec!["Total".to_string()],
                impact: 92.9,
            }),
            "Index Scan",
        );
        let findings = findings_for_plan(&plan);
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert_eq!(finding.summary, "Add an index on dbo.Orders (CustomerId)");
        assert!(finding.why.contains("93%"));
        assert!(
            finding
                .action
                .contains("CREATE NONCLUSTERED INDEX [IX_Orders_CustomerId]")
        );
        assert!(finding.action.contains("ON [dbo].[Orders] ([CustomerId])"));
        assert!(finding.action.contains("INCLUDE ([Total])"));
        assert_eq!(
            finding.action_sql.as_deref(),
            Some(
                "CREATE NONCLUSTERED INDEX [IX_Orders_CustomerId] ON [dbo].[Orders] ([CustomerId]) INCLUDE ([Total]);"
            )
        );
        assert_eq!(finding.node_id, Some(1));
    }

    #[test]
    fn low_impact_missing_index_is_a_warning() {
        let plan = plan_with(
            PlanWarning::MissingIndex(MissingIndex {
                database: String::new(),
                schema: "dbo".to_string(),
                table: "T".to_string(),
                equality_columns: vec![],
                inequality_columns: vec![],
                included_columns: vec![],
                impact: 41.0,
            }),
            "Index Scan",
        );
        let findings = findings_for_plan(&plan);
        assert_eq!(findings[0].severity, FindingSeverity::Warning);
        assert_eq!(findings[0].summary, "Add an index on dbo.T");
        assert!(!findings[0].action.contains("CREATE NONCLUSTERED"));
    }

    #[test]
    fn key_lookup_has_template_sql() {
        let plan = plan_with(
            PlanWarning::KeyLookup {
                table: "dbo.Orders".to_string(),
                index: "IX_Orders_CustomerId".to_string(),
            },
            "Key Lookup",
        );
        let findings = findings_for_plan(&plan);
        let sql = findings[0].action_sql.as_deref().expect("template sql");
        assert!(sql.contains("CREATE NONCLUSTERED INDEX [IX_Orders_CustomerId] ON [dbo].[Orders]"));
        assert!(sql.contains("WITH (DROP_EXISTING = ON)"));
        assert!(sql.contains("/* key columns */"));
    }

    #[test]
    fn duplicate_missing_indexes_report_once() {
        let mut plan = plan_with(
            PlanWarning::MissingIndex(MissingIndex {
                database: String::new(),
                schema: "dbo".to_string(),
                table: "T".to_string(),
                equality_columns: vec!["a".to_string()],
                inequality_columns: vec![],
                included_columns: vec![],
                impact: 50.0,
            }),
            "Index Scan",
        );
        plan.nodes.push(plan.nodes[0].clone());
        let findings = findings_for_plan(&plan);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn table_scan_summary_names_table_and_rows() {
        let plan = plan_with(
            PlanWarning::TableScan {
                table: "Orders".to_string(),
                schema: "dbo".to_string(),
            },
            "Table Scan",
        );
        let findings = findings_for_plan(&plan);
        assert_eq!(findings[0].severity, FindingSeverity::Warning);
        assert_eq!(
            findings[0].summary,
            "Scans every row in dbo.Orders (100 rows) to find matches"
        );
    }

    #[test]
    fn no_join_predicate_is_critical() {
        let plan = plan_with(PlanWarning::NoJoinPredicate, "Nested Loops");
        let findings = findings_for_plan(&plan);
        assert_eq!(findings[0].severity, FindingSeverity::Critical);
        assert!(findings[0].action.contains("ON clause"));
    }

    #[test]
    fn actual_estimate_mismatch_becomes_a_finding() {
        let mut plan = plan_with(PlanWarning::RowGoal, "Index Seek");
        plan.nodes[0].actual_rows = Some(48_210.0);
        plan.nodes[0].estimated_rows = 10.0;
        plan.warnings.clear();
        plan.nodes[0].warnings.clear();
        let findings = findings_for_plan(&plan);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, FindingSeverity::Warning);
        assert_eq!(findings[0].summary, "Estimated 10 rows but read 48.2K");
        assert!(findings[0].action.contains("UPDATE STATISTICS"));
    }

    #[test]
    fn small_mismatch_is_not_a_finding() {
        let mut plan = plan_with(PlanWarning::RowGoal, "Index Seek");
        plan.nodes[0].actual_rows = Some(15.0);
        plan.nodes[0].estimated_rows = 10.0;
        plan.warnings.clear();
        plan.nodes[0].warnings.clear();
        assert!(findings_for_plan(&plan).is_empty());
    }

    #[test]
    fn findings_rank_critical_before_warning() {
        let mut plan = plan_with(
            PlanWarning::TableScan {
                table: "T".to_string(),
                schema: "dbo".to_string(),
            },
            "Table Scan",
        );
        plan.nodes[0].warnings.push(PlanWarning::NoJoinPredicate);
        let findings = findings_for_plan(&plan);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].severity, FindingSeverity::Critical);
        assert_eq!(findings[1].severity, FindingSeverity::Warning);
    }

    #[test]
    fn operator_rows_sort_by_cost_and_summarize() {
        let xml = r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="4"><RelOp NodeId="1" LogicalOp="Select" PhysicalOp="Nested Loops" EstimateRows="4" EstimatedTotalSubtreeCost="3"><RunTimeCountersPerThread ActualRows="7" /><OutputList><ColumnReference Column="OrderId" /></OutputList></RelOp><RelOp NodeId="2" LogicalOp="Scan" PhysicalOp="Table Scan" EstimateRows="4" EstimatedTotalSubtreeCost="1" /></StmtSimple></ShowPlanXML>"#;
        let plan = PlanParser::new().parse_xml(xml).unwrap();
        let rows = operator_rows(&plan);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "Nested Loops");
        assert!(rows[0].summary.contains("75% of cost"));
        assert!(rows[0].summary.contains("read 7"));
        assert!(rows[0].detail.contains("Physical op: Nested Loops"));
        assert_eq!(rows[1].title, "Table Scan");
        assert!(rows[1].has_warnings);
    }

    #[test]
    fn human_count_formats_ranges() {
        assert_eq!(human_count(0.0), "0");
        assert_eq!(human_count(1.0), "1");
        assert_eq!(human_count(999.0), "999");
        assert_eq!(human_count(8_210.0), "8,210");
        assert_eq!(human_count(48_210.0), "48.2K");
        assert_eq!(human_count(1_234_567.0), "1.23M");
        assert_eq!(human_count(12_345_678.0), "12.3M");
        assert_eq!(human_count(123_456_789.0), "123M");
        assert_eq!(human_count(1_500_000_000.0), "1.50B");
        assert_eq!(human_count(12_300_000_000.0), "12.3B");
        assert_eq!(human_count(f64::NAN), "0");
        assert_eq!(human_count(f64::INFINITY), "0");
    }

    #[test]
    fn divergence_ratio_stays_finite_when_a_side_is_zero() {
        assert_eq!(divergence_ratio(100.0, 0.0), 100.0);
        assert_eq!(divergence_ratio(0.0, 100.0), 100.0);
        assert_eq!(divergence_ratio(0.0, 0.0), 0.0);
        assert_eq!(divergence_ratio(1_000.0, 1_000.0), 1.0);
    }

    #[test]
    fn zero_actual_rows_still_flags_divergence_without_infinite_text() {
        let mut plan = plan_with(PlanWarning::RowGoal, "Index Seek");
        plan.nodes[0].estimated_rows = 100.0;
        plan.nodes[0].actual_rows = Some(0.0);
        let findings = findings_for_plan(&plan);
        let divergence = findings
            .iter()
            .find(|f| f.summary.starts_with("Estimated"))
            .expect("divergence finding");
        assert!(divergence.detail.contains("100×"));
        assert!(!divergence.detail.contains("inf"));
    }

    #[test]
    fn no_warnings_means_no_findings() {
        let mut plan = plan_with(PlanWarning::RowGoal, "Index Seek");
        plan.nodes[0].warnings.clear();
        plan.warnings.clear();
        assert!(findings_for_plan(&plan).is_empty());
    }
}
