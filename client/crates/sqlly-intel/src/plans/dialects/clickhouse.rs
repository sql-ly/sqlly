//! ClickHouse: `EXPLAIN json = 1` (usually with `indexes = 1`, optionally
//! `actions = 1`).
//!
//! The capture is one row whose `explain` column holds a JSON document
//! shaped like PostgreSQL's: `[{"Plan": {"Node Type": .., "Node Id": ..,
//! "Description": .., "Plans": [..], "Indexes": [..]}}]`. ClickHouse plans
//! carry no cost model, and before the 25.x releases no cardinality
//! estimates either; the closest thing is the index analysis on
//! `ReadFromMergeTree` steps (how many parts and granules survived the
//! primary key, partition key and skip indexes), so that feeds the row
//! estimate (selected granules times the default granule size) and the
//! full-scan finding. `EXPLAIN` never runs the query, so every capture is
//! estimated.
//!
//! Clients that print tab-separated output (`clickhouse-client` writing to a
//! pipe, the HTTP interface) escape the document into a single line with
//! literal `\n` and `\'`; that form is accepted too.
//!
//! The step description shared with the text parser (`Step`, `IndexInfo`,
//! `step_node`) lives here so both formats fill the model the same way.

use serde_json::Value;

use crate::plans::dialects::PlanDialect;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode, PlanProperty};
use crate::plans::warning::PlanWarning;

/// The default `index_granularity` of a MergeTree table: rows per granule.
pub(super) const GRANULE_ROWS: f64 = 8192.0;

/// Steps nest one level per query-plan step; a document deeper than this is
/// not a real plan.
pub(super) const MAX_DEPTH: usize = 512;

/// Parse one ClickHouse `EXPLAIN json = 1` document into a plan.
pub fn parse_clickhouse_explain_json(doc: &str) -> PlanResult<ExecutionPlan> {
    let value = read_json(doc)?;
    let root = value
        .get(0)
        .and_then(|entry| entry.get("Plan"))
        .or_else(|| value.get("Plan"))
        .ok_or_else(|| {
            PlanError::InvalidStructure("ClickHouse EXPLAIN JSON: missing [0].Plan".into())
        })?;
    let mut builder = Builder::default();
    builder.visit(root, None, 0)?;
    let mut plan = ExecutionPlan {
        statement_type: "EXPLAIN".to_string(),
        nodes: builder.nodes,
        edges: builder.edges,
        source_format: Some("clickhouse-json".to_string()),
        ..Default::default()
    };
    finish_clickhouse_plan(&mut plan);
    Ok(plan)
}

/// Parse the document as JSON, undoing TSV escaping when that is what
/// stands in the way.
fn read_json(doc: &str) -> PlanResult<Value> {
    let trimmed = doc.trim();
    let first = serde_json::from_str::<Value>(trimmed);
    let error = match first {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    if trimmed.contains("\\n") || trimmed.contains("\\'") {
        // A header line (`explain`) may precede the row.
        let row = trimmed
            .lines()
            .find(|line| line.trim_start().starts_with(['[', '{']))
            .unwrap_or(trimmed);
        if let Ok(value) = serde_json::from_str::<Value>(unescape_tsv(row).trim()) {
            return Ok(value);
        }
    }
    Err(PlanError::InvalidStructure(format!(
        "ClickHouse EXPLAIN JSON: {error}"
    )))
}

/// Undo ClickHouse's TabSeparated escaping: `\n`, `\t`, `\\`, `\'` and the
/// rarer `\r`, `\0`, `\b`, `\f`. Unknown escapes are kept as written.
pub(super) fn unescape_tsv(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Plan-level fields every ClickHouse format shares.
pub(super) fn finish_clickhouse_plan(plan: &mut ExecutionPlan) {
    plan.dialect = Some(PlanDialect::ClickHouse);
    // The topmost step that has an estimate speaks for the plan.
    plan.estimated_rows = plan
        .nodes
        .iter()
        .map(|n| n.estimated_rows)
        .find(|rows| *rows > 0.0)
        .unwrap_or(0.0);
    super::common::finish_plan(plan);
}

/// One entry of a read step's index analysis.
#[derive(Debug, Default, Clone)]
pub(super) struct IndexInfo {
    /// `MinMax` (`Min-Max` from 25.x), `Partition`, `PrimaryKey`, `Skip`.
    pub kind: String,
    /// A skip index's name.
    pub name: Option<String>,
    /// A skip index's definition, such as `minmax GRANULARITY 4`.
    pub description: Option<String>,
    pub keys: Vec<String>,
    pub condition: Option<String>,
    /// Selected and initial part counts.
    pub parts: Option<(f64, f64)>,
    /// Selected and initial granule counts.
    pub granules: Option<(f64, f64)>,
    pub search_algorithm: Option<String>,
}

/// One query-plan step as either format describes it.
#[derive(Debug, Default)]
pub(super) struct Step {
    pub name: String,
    pub description: Option<String>,
    /// `None` when the document carries no index analysis for this step.
    pub indexes: Option<Vec<IndexInfo>>,
    /// Granules the step reads, when stated outside the index analysis.
    pub granules: Option<f64>,
    /// An explicit row estimate (`Output rows: estimated 36.79 thousand`).
    pub estimated_rows: Option<f64>,
    pub filter: Option<String>,
    pub group_keys: Vec<String>,
    pub sort_keys: Vec<String>,
    pub join_type: Option<String>,
    pub join_predicate: Option<String>,
    pub output_columns: Vec<String>,
    pub extra: Vec<String>,
    pub properties: Vec<PlanProperty>,
}

impl Step {
    pub(super) fn property(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        if !value.trim().is_empty() {
            self.properties.push(PlanProperty {
                name: name.to_string(),
                value,
            });
        }
    }

    pub(super) fn add_filter(&mut self, text: &str) {
        let text = text.trim();
        let text = text.strip_suffix("(removed)").unwrap_or(text).trim();
        if text.is_empty() {
            return;
        }
        self.filter = Some(match self.filter.take() {
            Some(existing) => format!("{existing} AND {text}"),
            None => text.to_string(),
        });
    }
}

/// Whether any index condition actually prunes, and the granule counts
/// before and after.
#[derive(Debug, Default, Clone, Copy)]
struct IndexAnalysis {
    selected_granules: Option<f64>,
    filtered: bool,
}

fn analyse_indexes(indexes: &[IndexInfo]) -> IndexAnalysis {
    let mut analysis = IndexAnalysis::default();
    for index in indexes {
        let condition = index.condition.as_deref().unwrap_or("");
        if !matches!(condition, "" | "true" | "unknown") {
            analysis.filtered = true;
        }
        if let Some((selected, initial)) = index.granules {
            analysis.selected_granules = Some(selected);
            if selected < initial {
                analysis.filtered = true;
            }
        }
    }
    analysis
}

/// A count as ClickHouse prints it: no fraction when it is whole.
fn count(value: f64) -> String {
    format!("{value}")
}

/// Build the model node for one step. `scan_without_analysis` says whether
/// a MergeTree read with no index analysis counts as a full scan: the JSON
/// parser has always said yes; the text parser cannot tell and says no.
pub(super) fn step_node(id: i32, step: Step, scan_without_analysis: bool) -> PlanNode {
    let Step {
        name,
        description,
        indexes,
        granules,
        estimated_rows,
        filter,
        group_keys,
        sort_keys,
        join_type,
        join_predicate,
        output_columns,
        extra,
        mut properties,
    } = step;
    let is_read = name.starts_with("ReadFrom");
    let physical_op = match &description {
        Some(description) => format!("{name} {description}"),
        None => name.clone(),
    };
    let analysis = indexes.as_deref().map(analyse_indexes);
    let estimated_rows = estimated_rows
        .or_else(|| {
            analysis
                .and_then(|a| a.selected_granules)
                .or(granules)
                .map(|granules| granules * GRANULE_ROWS)
        })
        .filter(|rows| rows.is_finite() && *rows >= 0.0)
        .unwrap_or(0.0);

    // `default.orders` names a table; `SystemNumbers` and the like do not.
    let table = description
        .as_deref()
        .filter(|d| is_read && !d.is_empty() && !d.contains([' ', '(']));
    let (schema, relation) = match table.map(|t| t.split_once('.')) {
        Some(Some((schema, table))) => (Some(schema.to_string()), Some(table.to_string())),
        Some(None) => (None, table.map(str::to_string)),
        None => (None, None),
    };

    let mut warnings = Vec::new();
    let unfiltered = match analysis {
        Some(analysis) => !analysis.filtered,
        None => scan_without_analysis,
    };
    if name == "ReadFromMergeTree"
        && unfiltered
        && let Some(description) = &description
    {
        let (schema, table) = match description.split_once('.') {
            Some((schema, table)) => (schema.to_string(), table.to_string()),
            None => (String::new(), description.clone()),
        };
        warnings.push(PlanWarning::TableScan { table, schema });
    }

    let mut output_list = Vec::new();
    let mut index_name = None;
    let mut index_cond = None;
    for index in indexes.iter().flatten() {
        let mut line = format!("{}:", index.kind);
        let condition = index.condition.as_deref().unwrap_or("");
        if !condition.is_empty() {
            line.push(' ');
            line.push_str(condition);
        }
        if let Some((selected, initial)) = index.parts {
            line.push_str(&format!(" parts {}/{}", count(selected), count(initial)));
        }
        if let Some((selected, initial)) = index.granules {
            line.push_str(&format!(" granules {}/{}", count(selected), count(initial)));
        }
        output_list.push(line);

        let label = match &index.name {
            Some(name) => format!("{} index {name}", index.kind),
            None => format!("{} index", index.kind),
        };
        let mut push = |what: &str, value: String| {
            if !value.is_empty() {
                properties.push(PlanProperty {
                    name: format!("{label} {what}"),
                    value,
                });
            }
        };
        push("keys", index.keys.join(", "));
        push("condition", condition.to_string());
        push("definition", index.description.clone().unwrap_or_default());
        if let Some((selected, initial)) = index.parts {
            push("parts", format!("{}/{}", count(selected), count(initial)));
        }
        if let Some((selected, initial)) = index.granules {
            push(
                "granules",
                format!("{}/{}", count(selected), count(initial)),
            );
        }
        push("search", index.search_algorithm.clone().unwrap_or_default());

        let prunes = !matches!(condition, "" | "true" | "unknown");
        if index.kind == "PrimaryKey" && prunes {
            index_cond = Some(condition.to_string());
        }
        if index.kind == "Skip" && index_name.is_none() {
            index_name = index.name.clone();
        }
    }

    PlanNode {
        id,
        logical_op: name,
        physical_op,
        estimated_rows,
        output_list,
        warnings,
        relation,
        schema,
        index_name,
        index_cond,
        filter,
        join_predicate,
        join_type,
        sort_keys,
        group_keys,
        output_columns,
        extra,
        properties,
        ..Default::default()
    }
}

/// `36.79 thousand`, `1.20 million`, `512`: the readable quantities the
/// 25.x planner prints for its estimates.
pub(super) fn parse_readable_quantity(text: &str) -> Option<f64> {
    let mut words = text.split_whitespace();
    let number: f64 = words.next()?.replace(',', "").parse().ok()?;
    let scale = match words.next() {
        None => 1.0,
        Some("thousand") => 1e3,
        Some("million") => 1e6,
        Some("billion") => 1e9,
        Some("trillion") => 1e12,
        Some("quadrillion") => 1e15,
        Some(_) => return None,
    };
    let value = number * scale;
    (value.is_finite() && value >= 0.0).then_some(value)
}

/// Split `a, f(b, c), d` at the commas outside brackets.
pub(super) fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth <= 0 => {
                let piece = current.trim();
                if !piece.is_empty() {
                    parts.push(piece.to_string());
                }
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    let piece = current.trim();
    if !piece.is_empty() {
        parts.push(piece.to_string());
    }
    parts
}

#[derive(Default)]
struct Builder {
    nodes: Vec<PlanNode>,
    edges: Vec<PlanEdge>,
}

/// Keys of a step object that are read into typed fields or are structure.
const HANDLED_KEYS: [&str; 18] = [
    "Node Type",
    "Description",
    "Plans",
    "Indexes",
    "Keys",
    "Aggregates",
    "Sort Description",
    "Type",
    "Clauses",
    "Join conditions",
    "Prewhere info",
    "Filter Column",
    "Header",
    "Granules",
    "Expression",
    "Actions",
    "Row level filter info",
    "Output rows",
];

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(items) => {
            let items: Vec<String> = items
                .iter()
                .filter(|item| !item.is_object() && !item.is_array())
                .filter_map(text_of)
                .collect();
            (!items.is_empty()).then(|| items.join(", "))
        }
        _ => None,
    }
    .filter(|text| !text.is_empty())
}

fn names_of(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| match item {
            Value::String(s) => Some(s.clone()),
            other => other
                .get("Name")
                .or_else(|| other.get("Column"))
                .and_then(Value::as_str)
                .map(str::to_string),
        })
        .collect()
}

fn read_indexes(value: &Value) -> Option<Vec<IndexInfo>> {
    let indexes = value.as_array()?;
    let pair = |index: &Value, selected: &str, initial: &str| match (
        index.get(selected).and_then(number),
        index.get(initial).and_then(number),
    ) {
        (Some(selected), Some(initial)) => Some((selected, initial)),
        _ => None,
    };
    let text =
        |index: &Value, key: &str| index.get(key).and_then(Value::as_str).map(str::to_string);
    Some(
        indexes
            .iter()
            .map(|index| IndexInfo {
                kind: text(index, "Type").unwrap_or_else(|| "Index".to_string()),
                name: text(index, "Name"),
                description: text(index, "Description"),
                keys: index.get("Keys").map(names_of).unwrap_or_default(),
                condition: text(index, "Condition"),
                parts: pair(index, "Selected Parts", "Initial Parts"),
                granules: pair(index, "Selected Granules", "Initial Granules"),
                search_algorithm: text(index, "Search Algorithm"),
            })
            .collect(),
    )
}

impl Builder {
    fn visit(&mut self, node: &Value, parent: Option<i32>, depth: usize) -> PlanResult<()> {
        if depth > MAX_DEPTH {
            return Err(PlanError::InvalidStructure(
                "ClickHouse EXPLAIN JSON: the plan nests too deeply".into(),
            ));
        }
        let object = node.as_object().ok_or_else(|| {
            PlanError::InvalidStructure("ClickHouse EXPLAIN JSON: step is not an object".into())
        })?;
        let mut step = Step {
            name: object
                .get("Node Type")
                .and_then(Value::as_str)
                .unwrap_or("Step")
                .to_string(),
            description: object
                .get("Description")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            indexes: object.get("Indexes").and_then(read_indexes),
            granules: object.get("Granules").and_then(number),
            ..Default::default()
        };
        let is_join = step.name.contains("Join");
        if let Some(keys) = object.get("Keys") {
            step.group_keys = names_of(keys);
        }
        if let Some(aggregates) = object.get("Aggregates") {
            step.property("Aggregates", names_of(aggregates).join(", "));
        }
        if let Some(sort) = object.get("Sort Description").and_then(Value::as_array) {
            for key in sort {
                let Some(column) = key.get("Column").and_then(Value::as_str) else {
                    continue;
                };
                let direction = match key.get("Ascending").and_then(Value::as_bool) {
                    Some(false) => " DESC",
                    _ => " ASC",
                };
                step.sort_keys.push(format!("{column}{direction}"));
            }
        }
        if let Some(kind) = object.get("Type").and_then(text_of) {
            if is_join {
                step.join_type = Some(kind);
            } else {
                step.property("Type", kind);
            }
        }
        if let Some(clauses) = object
            .get("Clauses")
            .or_else(|| object.get("Join conditions"))
            .and_then(text_of)
        {
            step.join_predicate = Some(clauses);
        }
        for key in ["Prewhere info", "Row level filter info"] {
            let Some(info) = object.get(key) else {
                continue;
            };
            for filter in ["Prewhere filter", "Row level filter"] {
                if let Some(column) = info
                    .get(filter)
                    .and_then(|f| f.get(format!("{filter} column")))
                    .and_then(Value::as_str)
                {
                    step.add_filter(column);
                }
            }
        }
        if let Some(column) = object.get("Filter Column").and_then(Value::as_str) {
            step.add_filter(column);
        }
        if let Some(header) = object.get("Header") {
            step.output_columns = names_of(header);
        }
        if let Some(rows) = object.get("Output rows").and_then(number) {
            step.estimated_rows = Some(rows);
        }
        if let Some(granules) = step.granules {
            step.property("Granules", count(granules));
        }
        for (key, value) in object {
            if HANDLED_KEYS.contains(&key.as_str()) {
                continue;
            }
            if let Some(text) = text_of(value) {
                step.property(key, text);
            }
        }

        let id = self.nodes.len() as i32 + 1;
        self.nodes.push(step_node(id, step, true));
        if let Some(parent) = parent {
            self.edges.push(PlanEdge {
                from_node: parent,
                to_node: id,
            });
        }
        if let Some(children) = object.get("Plans").and_then(Value::as_array) {
            for child in children {
                self.visit(child, Some(id), depth + 1)?;
            }
        }
        Ok(())
    }
}

fn number(value: &Value) -> Option<f64> {
    let parsed: Option<f64> = match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    parsed.filter(|n| n.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::findings_for_plan;

    const FILTERED: &str = r#"[{"Plan":{"Node Type":"Expression","Node Id":"Expression_9","Description":"Project names","Plans":[{"Node Type":"Limit","Node Id":"Limit_8","Description":"preliminary LIMIT","Plans":[{"Node Type":"Aggregating","Node Id":"Aggregating_4","Plans":[{"Node Type":"ReadFromMergeTree","Node Id":"ReadFromMergeTree_0","Description":"default.ex_a","Indexes":[{"Type":"PrimaryKey","Keys":["id"],"Condition":"(id in [11, +Inf))","Search Algorithm":"binary search","Initial Parts":1,"Selected Parts":1,"Initial Granules":4,"Selected Granules":2}]}]}]}]}}]"#;

    const FULL_SCAN: &str = r#"[{"Plan":{"Node Type":"Expression","Node Id":"Expression_2","Plans":[{"Node Type":"ReadFromMergeTree","Node Id":"ReadFromMergeTree_0","Description":"shop.orders","Indexes":[{"Type":"PrimaryKey","Condition":"true","Initial Parts":3,"Selected Parts":3,"Initial Granules":10,"Selected Granules":10}]}]}}]"#;

    #[test]
    fn filtered_read_becomes_nodes_edges_and_granule_estimates() {
        let plan = parse_clickhouse_explain_json(FILTERED).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert_eq!(plan.nodes.len(), 4);
        assert_eq!(plan.edges.len(), 3);
        assert_eq!(plan.nodes[0].physical_op, "Expression Project names");
        let read = &plan.nodes[3];
        assert_eq!(read.logical_op, "ReadFromMergeTree");
        assert_eq!(read.physical_op, "ReadFromMergeTree default.ex_a");
        assert_eq!(read.estimated_rows, 2.0 * GRANULE_ROWS);
        assert_eq!(plan.estimated_rows, 2.0 * GRANULE_ROWS);
        assert_eq!(
            read.output_list,
            vec!["PrimaryKey: (id in [11, +Inf)) parts 1/1 granules 2/4".to_string()]
        );
        assert!(read.warnings.is_empty(), "a pruned read is not a full scan");
        assert!(plan.nodes.iter().all(|n| n.actual_rows.is_none()));
    }

    #[test]
    fn unpruned_read_is_flagged_as_a_table_scan() {
        let plan = parse_clickhouse_explain_json(FULL_SCAN).unwrap();
        let read = &plan.nodes[1];
        assert!(matches!(
            read.warnings.as_slice(),
            [PlanWarning::TableScan { table, schema }] if table == "orders" && schema == "shop"
        ));
        // The plan is too small for a finding: the rules need measured rows
        // or a meaningful share of the cost before they call out a scan.
        let findings = findings_for_plan(&plan);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn reads_without_index_analysis_are_full_scans_and_a_bare_plan_object_parses() {
        let plan = parse_clickhouse_explain_json(
            r#"{"Plan":{"Node Type":"ReadFromMergeTree","Node Id":"ReadFromMergeTree_0","Description":"t"}}"#,
        )
        .unwrap();
        assert_eq!(plan.nodes.len(), 1);
        assert!(matches!(
            plan.nodes[0].warnings.as_slice(),
            [PlanWarning::TableScan { table, schema }] if table == "t" && schema.is_empty()
        ));
        assert_eq!(plan.estimated_rows, 0.0);
    }

    #[test]
    fn malformed_documents_yield_typed_errors_not_panics() {
        assert!(parse_clickhouse_explain_json("not json").is_err());
        assert!(parse_clickhouse_explain_json("[]").is_err());
        assert!(parse_clickhouse_explain_json("[{}]").is_err());
        assert!(parse_clickhouse_explain_json("[{\"Plan\": 1}]").is_err());
    }

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!("../../../tests/golden/plans/clickhouse/", $name))
        };
    }

    const JOIN_269: &str = fixture!("clickhouse269-join-indexes.json");
    const RANGE_ACTIONS_269: &str = fixture!("clickhouse269-range-indexes-actions.json");
    const RANGE_ESCAPED_269: &str = fixture!("clickhouse269-range-json-escaped.tsv");
    const JOIN_ACTIONS_248: &str = fixture!("clickhouse248-join-indexes-actions.json");
    const RANGE_ESCAPED_248: &str = fixture!("clickhouse248-range-json-escaped.tsv");

    fn ops(plan: &ExecutionPlan) -> Vec<&str> {
        plan.nodes.iter().map(|n| n.logical_op.as_str()).collect()
    }

    fn prop<'a>(node: &'a PlanNode, name: &str) -> Option<&'a str> {
        node.properties
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.value.as_str())
    }

    #[test]
    fn captured_join_fills_relations_indexes_and_format() {
        let plan = parse_clickhouse_explain_json(JOIN_269).unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::ClickHouse));
        assert_eq!(plan.source_format.as_deref(), Some("clickhouse-json"));
        assert!(!plan.has_actuals);
        let orders = plan
            .nodes
            .iter()
            .find(|n| n.relation.as_deref() == Some("orders"))
            .unwrap();
        assert_eq!(orders.schema.as_deref(), Some("default"));
        assert_eq!(
            orders.output_list[0],
            "Min-Max: (placed_at in [1685577600, +Inf)) parts 5/10 granules 179/392"
        );
        assert_eq!(
            orders.index_cond.as_deref(),
            Some("(placed_at in [1685577600, +Inf))")
        );
        assert_eq!(prop(orders, "Min-Max index keys"), Some("placed_at"));
        assert_eq!(orders.estimated_rows, 179.0 * GRANULE_ROWS);
        assert!(orders.warnings.is_empty());
        assert!(
            prop(orders, "Node Id")
                .unwrap()
                .starts_with("ReadFromMergeTree_")
        );
        let customers = plan
            .nodes
            .iter()
            .find(|n| n.relation.as_deref() == Some("customers"))
            .unwrap();
        assert_eq!(customers.warnings.len(), 1);
        assert!(ops(&plan).contains(&"BuildRuntimeFilter"));
    }

    #[test]
    fn actions_output_fills_keys_sorting_join_and_filter() {
        let plan = parse_clickhouse_explain_json(JOIN_ACTIONS_248).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "Expression",
                "Limit",
                "Sorting",
                "Expression",
                "Aggregating",
                "Expression",
                "Join",
                "Expression",
                "ReadFromMergeTree",
                "Expression",
                "ReadFromMergeTree",
            ]
        );
        assert_eq!(prop(&plan.nodes[1], "Limit"), Some("5"));
        assert_eq!(plan.nodes[2].sort_keys, ["sum(__table2.total) DESC"]);
        assert_eq!(plan.nodes[4].group_keys, ["__table1.country"]);
        assert_eq!(
            prop(&plan.nodes[4], "Aggregates"),
            Some("count(), sum(__table2.total)")
        );
        let join = &plan.nodes[6];
        assert_eq!(join.join_type.as_deref(), Some("INNER"));
        assert_eq!(
            join.join_predicate.as_deref(),
            Some("[(__table1.id) = (__table2.customer_id)]")
        );
        assert_eq!(prop(join, "Algorithm"), Some("HashJoin"));
        let orders = &plan.nodes[10];
        assert_eq!(
            orders.filter.as_deref(),
            Some(
                "and(greaterOrEquals(__table2.placed_at, '2023-06-01 00:00:00'_String), \
                 equals(__table2.status, 'paid'_String))"
            )
        );
        assert_eq!(prop(orders, "Read Type"), Some("Default"));
        assert_eq!(prop(orders, "Parts"), Some("5"));
        assert_eq!(orders.estimated_rows, 194.0 * GRANULE_ROWS);

        let plan = parse_clickhouse_explain_json(RANGE_ACTIONS_269).unwrap();
        let read = plan.nodes.last().unwrap();
        assert_eq!(read.index_name.as_deref(), Some("total_idx"));
        assert_eq!(read.estimated_rows, 10.0 * GRANULE_ROWS);
        assert!(read.filter.as_deref().unwrap().starts_with("and(greater("));
    }

    #[test]
    fn tsv_escaped_single_line_documents_parse() {
        // The 26.9 capture was taken without `indexes = 1`.
        for (doc, granules) in [
            (RANGE_ESCAPED_269, None),
            (RANGE_ESCAPED_248, Some("10/389")),
        ] {
            assert!(doc.contains("\\n"));
            let plan = parse_clickhouse_explain_json(doc).unwrap();
            let read = plan.nodes.last().unwrap();
            assert_eq!(read.logical_op, "ReadFromMergeTree");
            assert_eq!(read.relation.as_deref(), Some("orders"));
            assert_eq!(read.schema.as_deref(), Some("default"));
            assert_eq!(prop(read, "PrimaryKey index granules"), granules);
        }
        let with_header = format!("explain\n{RANGE_ESCAPED_248}");
        assert!(parse_clickhouse_explain_json(&with_header).is_ok());
    }

    /// `EXPLAIN json = 1, description = 0, header = 1 SELECT 1, 2 + dummy`,
    /// as the ClickHouse documentation prints it: `Header` is an array of
    /// `{"Name", "Type"}` objects per step.
    #[test]
    fn header_columns_become_the_output_columns_of_each_step() {
        let doc = r#"[
  {
    "Plan": {
      "Node Type": "Expression",
      "Node Id": "Expression_5",
      "Header": [
        {"Name": "1", "Type": "UInt8"},
        {"Name": "plus(2, dummy)", "Type": "UInt16"}
      ],
      "Plans": [
        {
          "Node Type": "ReadFromStorage",
          "Node Id": "ReadFromStorage_0",
          "Header": [
            {"Name": "dummy", "Type": "UInt8"}
          ]
        }
      ]
    }
  }
]"#;
        let plan = parse_clickhouse_explain_json(doc).unwrap();
        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.nodes[0].output_columns, ["1", "plus(2, dummy)"]);
        assert_eq!(plan.nodes[1].output_columns, ["dummy"]);
        // The header is not repeated as a raw property.
        assert!(prop(&plan.nodes[0], "Header").is_none());
    }

    #[test]
    fn clickhouse_json_survives_mangled_input() {
        for sample in [RANGE_ACTIONS_269, RANGE_ESCAPED_248] {
            super::super::common::assert_survives_mangling(sample, |doc| {
                let _ = parse_clickhouse_explain_json(doc);
            });
        }
    }
}
