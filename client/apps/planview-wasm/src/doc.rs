//! The JSON documents the page renders, built from the shared plan module's
//! analysis, story, findings, diagram and projections. Numbers are sent both
//! raw (for sorting and scales) and worded (so the page never formats one
//! differently from the desktop app).

use std::collections::HashSet;

use serde_json::{Value, json};
use sqlly_intel::plans::learn::{
    LearnLink, engine_links_for_finding, engine_links_for_operator_in,
};
use sqlly_intel::plans::warning::PlanWarning;
use sqlly_intel::plans::{
    DiagramOptions, DiagramOrientation, EstimateDirection, ExecutionPlan, FindingSeverity,
    NodeAnalysis, NodeFlags, NodeHeat, OperatorTableColumn, OperatorTableRow, PlanAnalysis,
    PlanChangeReport, PlanDialect, PlanFinding, PlanMetric, PlanNode, PlanStory, ResolvedAlias,
    SchemaContext, human_cost, human_duration_ms, human_factor, human_number, human_share,
    human_size_kb, operator_table_columns, plan_breakdown, plan_diagram, plan_flame_layout,
    plan_operator_table,
};

/// JSON has no NaN or infinity; a malformed plan must not produce them.
pub(crate) fn finite(n: f64) -> f64 {
    if n.is_finite() { n } else { 0.0 }
}

fn opt(n: Option<f64>) -> Value {
    match n {
        Some(n) if n.is_finite() => json!(n),
        _ => Value::Null,
    }
}

pub(crate) fn severity_id(severity: FindingSeverity) -> &'static str {
    match severity {
        FindingSeverity::Critical => "critical",
        FindingSeverity::Warning => "warning",
        FindingSeverity::Info => "info",
    }
}

fn flags_json(flags: &NodeFlags) -> Value {
    json!({
        "bottleneck": flags.bottleneck,
        "neverExecuted": flags.never_executed,
        "spilled": flags.spilled,
        "parallel": flags.parallel,
        "criticalPath": flags.on_critical_path,
        "misestimate": flags.misestimate_origin,
    })
}

fn heat_json(heat: &NodeHeat) -> Value {
    json!({
        "time": heat.time,
        "cost": heat.cost,
        "rows": heat.rows,
        "estimate": heat.estimate,
        "io": heat.io,
    })
}

fn links_json(links: Vec<&'static LearnLink>) -> Value {
    Value::Array(
        links
            .into_iter()
            .map(|link| {
                json!({
                    "path": link.path,
                    "title": link.title,
                    "learn": link.learn,
                    "type": link.link_type.as_str(),
                })
            })
            .collect(),
    )
}

/// One line per operator warning for the details panel.
pub fn warning_label(warning: &PlanWarning) -> String {
    let qualified = |schema: &str, table: &str| {
        let schema = schema.trim_matches(['[', ']']);
        let table = table.trim_matches(['[', ']']);
        match (schema.is_empty(), table.is_empty()) {
            (_, true) => String::new(),
            (true, false) => table.to_string(),
            (false, false) => format!("{schema}.{table}"),
        }
    };
    match warning {
        PlanWarning::MissingIndex(index) => {
            let table = qualified(&index.schema, &index.table);
            format!(
                "Missing index on {table} (estimated impact {:.0}%)",
                index.impact
            )
        }
        PlanWarning::TableScan { table, schema } => match qualified(schema, table) {
            t if t.is_empty() => "Reads every row".to_string(),
            t => format!("Reads every row of {t}"),
        },
        PlanWarning::ImplicitConversion { expression } if expression.is_empty() => {
            "Implicit conversion prevents an index seek".to_string()
        }
        PlanWarning::ImplicitConversion { expression } => {
            format!("Implicit conversion prevents an index seek: {expression}")
        }
        PlanWarning::KeyLookup { table, .. } if table.is_empty() => "Lookup per row".to_string(),
        PlanWarning::KeyLookup { table, .. } => format!("Lookup per row into {table}"),
        PlanWarning::RowGoal => "Row goal (TOP, EXISTS, FAST n)".to_string(),
        PlanWarning::CardinalityEstimation => "Columns with no statistics".to_string(),
        PlanWarning::Spool => "Spool".to_string(),
        PlanWarning::SpillToTempDb => "Spilled to tempdb".to_string(),
        PlanWarning::NoJoinPredicate => "No join predicate".to_string(),
        PlanWarning::Other(text) => text.clone(),
    }
}

fn estimate_text(factor: Option<f64>, direction: Option<EstimateDirection>) -> Option<String> {
    let factor = factor?;
    Some(match direction {
        Some(EstimateDirection::Under) => {
            format!("{}x more rows than estimated", human_factor(factor))
        }
        Some(EstimateDirection::Over) => {
            format!("{}x fewer rows than estimated", human_factor(factor))
        }
        _ => "as estimated".to_string(),
    })
}

fn row(label: &str, value: impl Into<String>) -> Value {
    json!({ "label": label, "value": value.into() })
}

fn section(title: &str, rows: Vec<Value>) -> Option<Value> {
    (!rows.is_empty()).then(|| json!({ "title": title, "rows": rows }))
}

/// Everything the details panel shows for one operator, as titled groups of
/// label/value rows.
fn node_sections(node: Option<&PlanNode>, a: &NodeAnalysis) -> Vec<Value> {
    let m = &a.metrics;
    let mut sections = Vec::new();

    let mut reads = Vec::new();
    if let Some(object) = &a.object {
        reads.push(row("Table", object.label()));
    }
    if let Some(index) = &a.index {
        reads.push(row("Index", index.clone()));
    }
    if let Some(p) = &a.seek_predicate {
        reads.push(row("Index condition", p.text.clone()));
    }
    if let Some(p) = &a.filter {
        reads.push(row("Filter", p.text.clone()));
    }
    if let Some(p) = &a.join_predicate {
        reads.push(row("Join condition", p.text.clone()));
    }
    if let Some(kind) = &a.join_type {
        reads.push(row("Join type", kind.clone()));
    }
    if !a.sort_keys.is_empty() {
        reads.push(row("Sort keys", a.sort_keys.join(", ")));
    }
    if !a.group_keys.is_empty() {
        reads.push(row("Group keys", a.group_keys.join(", ")));
    }
    if let Some(name) = &a.subplan_name {
        reads.push(row("Subplan", name.clone()));
    }
    sections.extend(section("What it works on", reads));

    let mut rows = Vec::new();
    if let Some(n) = m.rows_out {
        rows.push(row("Rows out", human_number(n)));
    }
    if let Some(n) = m.loops.filter(|n| *n > 1.0) {
        rows.push(row("Loops", human_number(n)));
        if let Some(per) = m.rows_per_loop {
            rows.push(row("Rows per loop", human_number(per)));
        }
    }
    if let Some(n) = m.estimated_rows_total.or(m.estimated_rows) {
        rows.push(row("Estimated rows", human_number(n)));
    }
    if let Some(text) = estimate_text(m.estimate_factor, m.estimate_direction) {
        rows.push(row("Estimate", text));
    }
    if let Some(n) = m.rows_in {
        rows.push(row("Rows read", human_number(n)));
    }
    if let Some(n) = m.rows_removed.filter(|n| *n > 0.0) {
        let share = m
            .rows_removed_share
            .map(|s| format!(" ({} of what it read)", human_share(s)))
            .unwrap_or_default();
        rows.push(row("Rows discarded", format!("{}{share}", human_number(n))));
    }
    sections.extend(section("Rows", rows));

    let mut time = Vec::new();
    if let Some(ms) = m.self_time_ms {
        let share = m
            .time_share
            .map(|s| format!(" ({} of the statement)", human_share(s)))
            .unwrap_or_default();
        time.push(row("Own time", format!("{}{share}", human_duration_ms(ms))));
    }
    if let Some(ms) = m.inclusive_time_ms {
        time.push(row("Time with inputs", human_duration_ms(ms)));
    }
    if let Some(cost) = m.self_cost {
        let share = m
            .cost_share
            .map(|s| format!(" ({} of the plan)", human_share(s)))
            .unwrap_or_default();
        time.push(row("Own cost", format!("{}{share}", human_cost(cost))));
    }
    if let Some(cost) = m.inclusive_cost {
        time.push(row("Cost with inputs", human_cost(cost)));
    }
    if let Some(pages) = m.io_pages_self.filter(|n| *n > 0.0) {
        let share = m
            .io_share
            .map(|s| format!(" ({} of the plan)", human_share(s)))
            .unwrap_or_default();
        time.push(row("Pages read", format!("{}{share}", human_number(pages))));
    }
    if let Some(kb) = m.memory_kb.filter(|n| *n > 0.0) {
        time.push(row("Memory", human_size_kb(kb)));
    }
    if let Some(kb) = m.spilled_kb.filter(|n| *n > 0.0) {
        time.push(row("Spilled to disk", human_size_kb(kb)));
    }
    if let Some(n) = m.processes.filter(|n| *n > 1.0) {
        time.push(row("Processes", human_number(n)));
    }
    sections.extend(section("Time and cost", time));

    if let Some(node) = node {
        let mut raw: Vec<Value> = node
            .properties
            .iter()
            .filter(|p| !p.value.trim().is_empty())
            .map(|p| row(&p.name, p.value.clone()))
            .collect();
        for line in &node.extra {
            raw.push(row("Extra", line.clone()));
        }
        let output = if node.output_columns.is_empty() {
            &node.output_list
        } else {
            &node.output_columns
        };
        if !output.is_empty() {
            raw.push(row("Output", output.join(", ")));
        }
        sections.extend(section("As the plan printed it", raw));
    }
    sections
}

fn node_json(
    plan: &ExecutionPlan,
    analysis: &PlanAnalysis,
    a: &NodeAnalysis,
    step: usize,
) -> Value {
    let node = plan.nodes.iter().find(|n| n.id == a.node_id);
    let m = &a.metrics;
    json!({
        "id": a.node_id,
        "parentId": a.parent_id,
        "children": a.children,
        "depth": a.depth,
        "subtreeSize": a.subtree_size,
        "step": step,
        "title": a.title().fallback(),
        "operator": a.operator.name,
        "operatorKey": a.operator.key,
        "known": a.operator.known,
        "category": a.operator.category.key(),
        "categoryLabel": a.operator.category.label().fallback(),
        "description": a.operator.description.fallback(),
        "performance": a.operator.performance.fallback(),
        "object": a.object.as_ref().map(|o| o.label()),
        "index": a.index,
        "flags": flags_json(&a.flags),
        "heat": heat_json(&m.heat),
        "values": {
            "time": opt(m.self_time_ms),
            "cost": opt(m.self_cost),
            "rows": opt(m.rows_out.or(m.estimated_rows_total).or(m.estimated_rows)),
            "estimate": opt(m.estimate_factor),
            "io": opt(m.io_pages_self),
        },
        "warnings": node
            .map(|n| n.warnings.iter().map(warning_label).collect::<Vec<_>>())
            .unwrap_or_default(),
        "sections": node_sections(node, a),
        "learn": links_json(engine_links_for_operator_in(
            a.operator.key,
            a.operator.category,
            analysis.engine,
        )),
    })
}

fn story_json(story: &PlanStory) -> Value {
    json!({
        "headline": story.headline.fallback(),
        "summary": story.summary.iter().map(|s| s.fallback().to_string()).collect::<Vec<_>>(),
        "bottleneck": story.bottleneck.as_ref().map(|b| json!({
            "nodeId": b.node_id,
            "step": b.step,
            "share": finite(b.share),
            "message": b.message.fallback(),
            "detail": b.detail.as_ref().map(|d| d.fallback().to_string()),
        })),
        "steps": story.steps.iter().map(|s| json!({
            "number": s.number,
            "nodeId": s.node_id,
            "depth": s.depth,
            "title": s.title.fallback(),
            "sentence": s.sentence.fallback(),
            "notes": s.notes.iter().map(|n| json!({
                "kind": n.kind.key(),
                "message": n.message.fallback(),
            })).collect::<Vec<_>>(),
            "inputs": s.inputs,
            "share": s.share.map(finite),
            "shareText": s.share.map(human_share),
            "shareMetric": s.share_metric.key(),
            "bottleneck": s.is_bottleneck,
            "minor": s.minor,
            "neverExecuted": s.never_executed,
        })).collect::<Vec<_>>(),
        "text": story.to_text(),
    })
}

/// What tells two findings apart when one list was made with the schema and
/// the other without.
fn finding_identity(f: &PlanFinding) -> (&'static str, Option<i32>, String, Option<String>) {
    (
        f.kind.as_str(),
        f.node_id,
        f.summary.fallback().to_string(),
        f.action_sql.clone(),
    )
}

fn finding_json(
    f: &PlanFinding,
    analysis: &PlanAnalysis,
    story: &PlanStory,
    used_schema: bool,
) -> Value {
    let node_title = |id: i32| {
        analysis
            .node(id)
            .map(|n| n.title().fallback().to_string())
            .unwrap_or_default()
    };
    json!({
        "kind": f.kind.as_str(),
        "category": f.category.as_str(),
        "severity": severity_id(f.severity),
        "confidence": f.confidence.as_str(),
        "summary": f.summary.fallback(),
        "why": f.why.fallback(),
        "action": f.action.fallback(),
        "actionSql": f.action_sql,
        "alternatives": f.alternatives.iter().map(|a| json!({
            "label": a.label.fallback(),
            "sql": a.sql,
        })).collect::<Vec<_>>(),
        "detail": f.detail.fallback(),
        "evidence": f.evidence.iter().map(|e| e.fallback().to_string()).collect::<Vec<_>>(),
        "notes": f.notes.iter().map(|n| n.fallback().to_string()).collect::<Vec<_>>(),
        "nodeId": f.node_id,
        "nodeIds": f.node_ids,
        "nodeTitle": f.node_id.map(node_title),
        "step": f.node_id.and_then(|id| story.step_for_node(id)).map(|s| s.number),
        "impact": f.impact.map(|i| json!({
            "share": finite(i.share),
            "shareText": human_share(i.share),
            "basis": i.basis.as_str(),
            "timeMs": i.time_ms.map(finite),
            "optimizerEstimate": i.optimizer_estimate,
        })),
        "impactText": f.impact_message().map(|m| m.fallback().to_string()),
        "usedSchema": used_schema,
        "learn": links_json(engine_links_for_finding(f.kind.as_str(), analysis.engine)),
    })
}

/// The findings of one statement. `without_schema` is the list the same
/// plan produces with no table definitions; when given, each finding is
/// marked with whether the definitions changed it, and suggestions the
/// definitions ruled out are listed separately.
pub(crate) fn findings_json(
    findings: &[PlanFinding],
    without_schema: Option<&[PlanFinding]>,
    analysis: &PlanAnalysis,
    story: &PlanStory,
) -> (Vec<Value>, Vec<Value>) {
    let before: HashSet<_> = without_schema
        .map(|list| list.iter().map(finding_identity).collect())
        .unwrap_or_default();
    let after: HashSet<_> = findings.iter().map(finding_identity).collect();
    let list = findings
        .iter()
        .map(|f| {
            let used = without_schema.is_some()
                && (f.category.as_str() == "schema" || !before.contains(&finding_identity(f)));
            finding_json(f, analysis, story, used)
        })
        .collect();
    let ruled_out = without_schema
        .unwrap_or_default()
        .iter()
        .filter(|f| {
            // A suggestion the definitions reworded is "changed", not
            // "ruled out": only kinds that vanished for the node count.
            !after.contains(&finding_identity(f))
                && !findings
                    .iter()
                    .any(|g| g.kind == f.kind && g.node_id == f.node_id)
        })
        .map(|f| {
            json!({
                "kind": f.kind.as_str(),
                "summary": f.summary.fallback(),
                "actionSql": f.action_sql,
            })
        })
        .collect();
    (list, ruled_out)
}

fn table_cell(column: OperatorTableColumn, r: &OperatorTableRow) -> Value {
    use OperatorTableColumn as C;
    let number = |n: Option<f64>, text: fn(f64) -> String| match n {
        Some(n) if n.is_finite() => json!([n, text(n)]),
        _ => Value::Null,
    };
    match column {
        C::Step => json!([r.step, r.step.to_string()]),
        C::Operator => json!([r.operator, r.operator]),
        C::Object => {
            let text = match (&r.object, &r.index) {
                (Some(object), Some(index)) => format!("{object} ({index})"),
                (Some(object), None) => object.clone(),
                (None, Some(index)) => index.clone(),
                (None, None) => return Value::Null,
            };
            json!([text, text])
        }
        C::SelfTime => number(r.self_time_ms, human_duration_ms),
        C::TotalTime => number(r.inclusive_time_ms, human_duration_ms),
        C::TimeShare => number(r.time_share, human_share),
        C::SelfCost => number(r.self_cost, human_cost),
        C::TotalCost => number(r.inclusive_cost, human_cost),
        C::CostShare => number(r.cost_share, human_share),
        C::Rows => number(r.rows, human_number),
        C::EstimatedRows => number(r.estimated_rows, human_number),
        C::EstimateFactor => match r.estimate_factor {
            Some(factor) if factor.is_finite() => {
                let text = match r.estimate_direction {
                    Some(EstimateDirection::Under) => format!("{}x under", human_factor(factor)),
                    Some(EstimateDirection::Over) => format!("{}x over", human_factor(factor)),
                    _ => "exact".to_string(),
                };
                json!([factor, text])
            }
            _ => Value::Null,
        },
        C::Loops => number(r.loops, human_number),
        C::RowsRemoved => number(r.rows_removed, human_number),
        C::Io => number(r.io_pages, human_number),
        C::Memory => number(r.memory_kb, human_size_kb),
    }
}

fn table_json(analysis: &PlanAnalysis) -> Value {
    let rows = plan_operator_table(analysis);
    let columns = operator_table_columns(&rows);
    json!({
        "columns": columns.iter().map(|c| json!({
            "key": c.key(),
            "label": c.label().fallback(),
            "numeric": c.is_numeric(),
        })).collect::<Vec<_>>(),
        "rows": rows.iter().map(|r| {
            let mut cells = serde_json::Map::new();
            for column in &columns {
                cells.insert(column.key().to_string(), table_cell(*column, r));
            }
            json!({
                "nodeId": r.node_id,
                "depth": r.depth,
                "category": r.category.key(),
                "heat": heat_json(&r.heat),
                "flags": flags_json(&r.flags),
                "cells": cells,
            })
        }).collect::<Vec<_>>(),
    })
}

fn metric_value_text(metric: PlanMetric, value: f64) -> String {
    match metric {
        PlanMetric::Time => human_duration_ms(value),
        PlanMetric::Cost => human_cost(value),
        _ => human_number(value),
    }
}

fn flame_json(analysis: &PlanAnalysis, metric: PlanMetric) -> Value {
    let Some(layout) = plan_flame_layout(analysis, Some(metric)) else {
        return Value::Null;
    };
    json!({
        "metric": layout.metric.key(),
        "total": finite(layout.total),
        "totalText": metric_value_text(layout.metric, layout.total),
        "depth": layout.depth,
        "cells": layout.cells.iter().map(|c| json!({
            "nodeId": c.node_id,
            "depth": c.depth,
            "x": c.x,
            "width": c.width,
            "valueText": metric_value_text(layout.metric, c.value),
            "selfText": metric_value_text(layout.metric, c.self_value),
            "shareText": human_share(c.share),
            "label": c.label.fallback(),
            "category": c.category.key(),
            "heat": c.heat,
            "flags": flags_json(&c.flags),
        })).collect::<Vec<_>>(),
    })
}

fn breakdown_json(analysis: &PlanAnalysis) -> Value {
    let Some(breakdown) = plan_breakdown(analysis, 8) else {
        return Value::Null;
    };
    let entries = |list: &[sqlly_intel::plans::BreakdownEntry]| {
        list.iter()
            .map(|e| {
                json!({
                    "label": e.label.fallback(),
                    "nodeIds": e.node_ids,
                    "share": finite(e.share),
                    "shareText": human_share(e.share),
                    "valueText": metric_value_text(breakdown.metric, e.value),
                    "category": e.category.map(|c| c.key()),
                })
            })
            .collect::<Vec<_>>()
    };
    json!({
        "metric": breakdown.metric.key(),
        "totalText": metric_value_text(breakdown.metric, breakdown.total),
        "byOperator": entries(&breakdown.by_operator),
        "byCategory": entries(&breakdown.by_category),
        "byTable": entries(&breakdown.by_table),
    })
}

/// What the page may ask of a diagram layout.
#[derive(Debug, Clone, Default)]
pub struct DiagramRequest {
    pub orientation: DiagramOrientation,
    pub metric: Option<PlanMetric>,
    pub collapsed: HashSet<i32>,
}

impl DiagramRequest {
    /// Read `{ "orientation": "root_left", "metric": "cost", "collapsed": [3] }`.
    /// Unknown or missing values fall back to the defaults.
    pub fn from_json(options: &Value) -> Self {
        Self {
            orientation: options
                .get("orientation")
                .and_then(Value::as_str)
                .and_then(DiagramOrientation::from_key)
                .unwrap_or_default(),
            metric: options
                .get("metric")
                .and_then(Value::as_str)
                .and_then(PlanMetric::from_key),
            collapsed: options
                .get("collapsed")
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(Value::as_i64)
                        .filter_map(|id| i32::try_from(id).ok())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// The laid-out diagram: boxes, routed edges and group outlines in one
/// coordinate space.
pub(crate) fn diagram_json(
    plan: &ExecutionPlan,
    analysis: &PlanAnalysis,
    request: &DiagramRequest,
) -> Value {
    let options = DiagramOptions {
        orientation: request.orientation,
        collapsed: request.collapsed.clone(),
        metric: request.metric,
        ..DiagramOptions::default()
    };
    let diagram = plan_diagram(plan, analysis, &options);
    json!({
        "orientation": diagram.orientation.key(),
        "metric": diagram.metric.map(|m| m.key()),
        "width": diagram.width,
        "height": diagram.height,
        "nodes": diagram.nodes.iter().map(|n| json!({
            "id": n.node_id,
            "parentId": n.parent_id,
            "x": n.x,
            "y": n.y,
            "w": n.width,
            "h": n.height,
            "title": n.title,
            "subtitle": n.subtitle,
            "stats": n.stats.as_ref().map(|s| s.fallback().to_string()),
            "share": n.share.map(finite),
            "shareLabel": n.share_label,
            "category": n.category.key(),
            "heat": n.heat,
            "flags": flags_json(&n.flags),
            "warningCount": n.warning_count,
            "hasChildren": n.has_children,
            "collapsed": n.collapsed,
            "hiddenCount": n.hidden_count,
            "summary": n.summary.as_ref().map(|s| s.fallback().to_string()),
        })).collect::<Vec<_>>(),
        "edges": diagram.edges.iter().map(|e| json!({
            "from": e.from_node,
            "to": e.to_node,
            "rowsLabel": e.rows_label,
            "estimated": e.rows_estimated,
            "thickness": e.thickness,
            "points": e.points.iter().map(|(x, y)| json!([x, y])).collect::<Vec<_>>(),
            "labelX": e.label_x,
            "labelY": e.label_y,
            "criticalPath": e.on_critical_path,
            "neverExecuted": e.never_executed,
        })).collect::<Vec<_>>(),
        "groups": diagram.groups.iter().map(|g| json!({
            "kind": g.kind.key(),
            "label": g.label.fallback(),
            "x": g.x,
            "y": g.y,
            "w": g.width,
            "h": g.height,
        })).collect::<Vec<_>>(),
    })
}

/// The metrics this plan carries, in picker order, with the one the plan is
/// best read by first in `primary`.
fn metrics_json(analysis: &PlanAnalysis) -> Value {
    let available = |metric: PlanMetric| match metric {
        PlanMetric::Time => analysis.has_timing,
        PlanMetric::Cost => analysis.has_costs,
        PlanMetric::Rows => analysis.max_rows > 0.0,
        PlanMetric::EstimateError => analysis.has_actuals && analysis.has_estimates,
        PlanMetric::Io => analysis.has_io,
    };
    Value::Array(
        PlanMetric::ALL
            .into_iter()
            .filter(|m| available(*m) || *m == analysis.primary_metric)
            .map(|m| json!({ "key": m.key(), "label": m.label().fallback() }))
            .collect(),
    )
}

pub(crate) struct StatementInput<'a> {
    pub index: usize,
    pub plan: &'a ExecutionPlan,
    pub analysis: &'a PlanAnalysis,
    pub story: &'a PlanStory,
    pub share: Option<f64>,
    pub findings: &'a [PlanFinding],
    pub without_schema: Option<&'a [PlanFinding]>,
    pub aliases: &'a [ResolvedAlias],
    pub query_text: &'a str,
    pub diagram: &'a DiagramRequest,
}

pub(crate) fn statement_json(input: &StatementInput<'_>) -> Value {
    let StatementInput {
        plan,
        analysis,
        story,
        ..
    } = *input;
    let (findings, ruled_out) =
        findings_json(input.findings, input.without_schema, analysis, story);
    let count = |severity: FindingSeverity| {
        input
            .findings
            .iter()
            .filter(|f| f.severity == severity)
            .count()
    };
    let steps: std::collections::HashMap<i32, usize> = story
        .steps
        .iter()
        .map(|step| (step.node_id, step.number))
        .collect();
    let statement_text = if plan.statement_text.trim().is_empty() {
        input.query_text.trim()
    } else {
        plan.statement_text.trim()
    };
    json!({
        "index": input.index,
        "statementText": statement_text,
        "statementType": plan.statement_type,
        "kind": if analysis.has_actuals { "actual" } else { "estimated" },
        "share": input.share.map(finite),
        "shareText": input.share.map(human_share),
        "facts": {
            "hasActuals": analysis.has_actuals,
            "hasTiming": analysis.has_timing,
            "hasCosts": analysis.has_costs,
            "hasEstimates": analysis.has_estimates,
            "hasIo": analysis.has_io,
            "totalTimeMs": opt(analysis.total_time_ms),
            "totalTimeText": analysis.total_time_ms.map(human_duration_ms),
            "planningTimeText": analysis.planning_time_ms.map(human_duration_ms),
            "executionTimeText": analysis.execution_time_ms.map(human_duration_ms),
            "totalCostText": analysis.total_cost.map(human_cost),
            "totalPagesText": analysis.total_io_pages.map(human_number),
            "resultRowsText": analysis.result_rows.map(human_number),
            "operatorCount": analysis.nodes.len(),
            "primaryMetric": analysis.primary_metric.key(),
        },
        "metrics": metrics_json(analysis),
        "story": story_json(story),
        "findings": findings,
        "ruledOut": ruled_out,
        "findingCounts": {
            "critical": count(FindingSeverity::Critical),
            "warning": count(FindingSeverity::Warning),
            "info": count(FindingSeverity::Info),
        },
        "aliases": input.aliases.iter().map(|a| json!({
            "nodeId": a.node_id,
            "printed": a.printed,
            "table": a.table,
        })).collect::<Vec<_>>(),
        "roots": analysis.roots,
        "bottleneck": analysis.bottleneck,
        "criticalPath": analysis.critical_path,
        "nodes": analysis.nodes.iter().map(|a| {
            node_json(plan, analysis, a, steps.get(&a.node_id).copied().unwrap_or(0))
        }).collect::<Vec<_>>(),
        "diagram": diagram_json(plan, analysis, input.diagram),
        "table": table_json(analysis),
        "flame": {
            "time": flame_json(analysis, PlanMetric::Time),
            "cost": flame_json(analysis, PlanMetric::Cost),
        },
        "breakdown": breakdown_json(analysis),
    })
}

/// What the pasted table definitions held, for the summary line under the
/// schema box.
pub(crate) fn schema_json(schema: &SchemaContext, dialect: PlanDialect) -> Value {
    let indexes: usize = schema.tables.iter().map(|t| t.all_indexes().count()).sum();
    json!({
        "dialect": sqlly_intel::plans::PlanEngine::from_dialect(dialect).name(),
        "tables": schema.tables.len(),
        "indexes": indexes,
        "columns": schema.tables.iter().map(|t| t.columns.len()).sum::<usize>(),
        "rowCounts": schema.tables.iter().filter(|t| t.row_count.is_some()).count(),
        "tableNames": schema.tables.iter().map(|t| t.qualified_name()).collect::<Vec<_>>(),
        "skipped": schema.ignored_statements,
        "notes": schema.notes.iter().map(|n| n.message().fallback().to_string()).collect::<Vec<_>>(),
    })
}

/// The worded comparison of two captures of the same statements.
pub(crate) fn changes_json(report: &PlanChangeReport) -> Value {
    json!({
        "headline": report.headline.fallback(),
        "statements": report.statements.iter().map(|s| json!({
            "index": s.statement_index,
            "headline": s.headline.fallback(),
            "effect": s.headline_effect.key(),
            "sameShape": s.same_shape,
            "changes": s.changes.iter().map(|c| json!({
                "kind": c.kind.key(),
                "structural": c.kind.is_structural(),
                "effect": c.effect.key(),
                "beforeNodeId": c.baseline_node_id,
                "afterNodeId": c.comparison_node_id,
                "message": c.message.fallback(),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}
