use crate::plans::{
    ExecutionPlan, PlanEdge, PlanError, PlanNode, PlanResult,
    warning::{MissingIndex, PlanWarning},
};

pub struct PlanParser;

impl PlanParser {
    pub fn new() -> Self {
        Self
    }

    pub fn parse_xml(&self, xml: &str) -> PlanResult<ExecutionPlan> {
        self.parse_xml_multi(xml)?
            .into_iter()
            .next()
            .ok_or_else(|| PlanError::InvalidStructure("missing StmtSimple element".to_string()))
    }

    /// Parse every `<StmtSimple>` in the document into its own plan. A batch
    /// like `SELECT …; UPDATE …` produces one ShowPlan statement per SQL
    /// statement; keeping them separate lets the viewer group findings per
    /// statement instead of mixing every operator into one list.
    pub fn parse_xml_multi(&self, xml: &str) -> PlanResult<Vec<ExecutionPlan>> {
        if xml.trim().is_empty() {
            return Err(PlanError::EmptyPlan);
        }
        let statements = collect_elements(xml, "StmtSimple");
        if statements.is_empty() {
            return Err(PlanError::InvalidStructure(
                "missing StmtSimple element".to_string(),
            ));
        }
        Ok(statements
            .iter()
            .map(|statement| parse_statement(statement))
            .collect())
    }
}

fn collect_elements(xml: &str, name: &str) -> Vec<String> {
    let prefix = format!("<{name}");
    let suffix = format!("</{name}>");
    xml.match_indices(&prefix)
        .filter_map(|(start, _)| {
            let rest = &xml[start..];
            if let Some(end) = rest.find(&suffix) {
                return Some(rest[..end + suffix.len()].to_string());
            }
            let end = rest.find('>')?;
            Some(rest[..=end].to_string())
        })
        .collect()
}

/// Parse a single `<StmtSimple>` chunk into a plan. All RelOp/missing-index
/// extraction is scoped to this chunk so multi-statement batches stay grouped
/// per statement.
fn parse_statement(statement: &str) -> ExecutionPlan {
    let statement_text = attr(statement, "StatementText").unwrap_or_default();
    let statement_type = attr(statement, "StatementType").unwrap_or_default();
    let total_subtree_cost = attr(statement, "StatementSubTreeCost")
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or_default();

    let nodes = collect_elements(statement, "RelOp")
        .into_iter()
        .map(|element| PlanNode {
            id: attr(&element, "NodeId")
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
            logical_op: attr(&element, "LogicalOp").unwrap_or_default(),
            physical_op: attr(&element, "PhysicalOp").unwrap_or_default(),
            estimated_rows: attr(&element, "EstimateRows")
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
            estimated_cost: attr(&element, "EstimatedTotalSubtreeCost")
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
            estimated_cpu: attr(&element, "EstimateCPU")
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
            estimated_io: attr(&element, "EstimateIO")
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
            actual_rows: attr(&element, "ActualRows").and_then(|value| value.parse().ok()),
            actual_rewinds: None,
            actual_rebinds: None,
            row_count: attr(&element, "ActualRows").and_then(|value| value.parse().ok()),
            output_list: collect_column_references(&element),
            warnings: warning_flags(&element),
        })
        .collect::<Vec<_>>();

    let estimated_rows = nodes.iter().map(|node| node.estimated_rows).sum();
    let warnings = nodes
        .iter()
        .flat_map(|node| node.warnings.clone())
        .collect::<Vec<_>>();
    let mut edges = nested_relop_edges(statement);
    if edges.is_empty() && nodes.len() > 1 {
        edges = sequential_edges(&nodes);
    }

    ExecutionPlan {
        statement_text,
        statement_type,
        nodes,
        edges,
        total_subtree_cost,
        estimated_rows,
        warnings,
    }
}

/// Strip one layer of surrounding brackets: `[dbo]` → `dbo`. ShowPlan XML
/// quotes every identifier; findings read better without the noise.
pub(crate) fn strip_brackets(name: &str) -> String {
    name.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

fn attr(element: &str, name: &str) -> Option<String> {
    let prefix = format!("{name}=\"");
    let start = element.find(&prefix)? + prefix.len();
    let value = &element[start..];
    let end = value.find('"')?;
    Some(unescape_xml(&value[..end]))
}

/// Longest entity we will look ahead for, so a bare `&` in the SQL does not
/// swallow everything up to the next distant `;`. `&#x10FFFF;` is the longest
/// legal form at 10 characters.
const MAX_ENTITY_LEN: usize = 10;

/// Expand the XML entities SHOWPLAN uses inside attribute values.
///
/// Numeric character references matter as much as the five named ones: SQL
/// Server escapes the newlines and tabs inside `StatementText` as `&#xa;` and
/// `&#x9;`. Handling only the named entities left those on screen verbatim,
/// so a multi-line statement rendered as
/// `SELECT&#xa;&#x9;@Offset = …` in the plan's statement header.
fn unescape_xml(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        let Some(semi) = tail.find(';').filter(|end| *end <= MAX_ENTITY_LEN) else {
            // A `&` that starts no entity is just an ampersand.
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        match decode_entity(&tail[1..semi]) {
            Some(ch) => out.push(ch),
            // Unknown entity: pass it through rather than eat it.
            None => out.push_str(&tail[..=semi]),
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// Decode one entity body — the text between `&` and `;`.
fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "quot" => Some('"'),
        "apos" => Some('\''),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        _ => {
            let digits = entity.strip_prefix('#')?;
            let code = match digits.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => digits.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

fn warning_flags(element: &str) -> Vec<PlanWarning> {
    let mut warnings = Vec::new();
    if element.contains("NoJoinPredicate=\"1\"") || element.contains("NoJoinPredicate=\"true\"") {
        warnings.push(PlanWarning::NoJoinPredicate);
    }
    if element.contains("PhysicalOp=\"Table Scan\"") {
        warnings.push(PlanWarning::TableScan {
            table: object_attr(element, "Table").unwrap_or_default(),
            schema: object_attr(element, "Schema").unwrap_or_default(),
        });
    }
    if element.contains("PhysicalOp=\"Key Lookup\"") {
        warnings.push(PlanWarning::KeyLookup {
            table: object_attr(element, "Table").unwrap_or_default(),
            index: object_attr(element, "Index").unwrap_or_default(),
        });
    }
    if element.contains("<MissingIndex") {
        warnings.push(PlanWarning::MissingIndex(MissingIndex {
            database: attr(element, "Database").unwrap_or_default(),
            schema: attr(element, "Schema").unwrap_or_default(),
            table: attr(element, "Table").unwrap_or_default(),
            equality_columns: column_group(element, "EQUALITY"),
            inequality_columns: column_group(element, "INEQUALITY"),
            included_columns: column_group(element, "INCLUDE"),
            impact: attr(element, "Impact")
                .and_then(|value| value.parse().ok())
                .unwrap_or_default(),
        }));
    }
    if element.contains("ConvertIssue=") {
        warnings.push(PlanWarning::ImplicitConversion {
            expression: String::new(),
        });
    }
    if element.contains("<SpillToTempDb") {
        warnings.push(PlanWarning::SpillToTempDb);
    }
    if element.contains("ColumnsWithNoStatistics") {
        warnings.push(PlanWarning::CardinalityEstimation);
    }
    warnings
}

/// Read an attribute off the node's first `<Object …>` element (the table,
/// index, or view the operator touches). Values come bracket-quoted.
fn object_attr(element: &str, name: &str) -> Option<String> {
    let start = element.find("<Object")?;
    attr(&element[start..], name).map(|value| strip_brackets(&value))
}

/// Column names from every `<ColumnGroup Usage="{usage}">` block, in document
/// order, with quoting brackets stripped.
fn column_group(element: &str, usage: &str) -> Vec<String> {
    let marker = format!("<ColumnGroup Usage=\"{usage}\"");
    let mut columns = Vec::new();
    for (start, _) in element.match_indices(&marker) {
        let rest = &element[start..];
        let end = rest.find("</ColumnGroup>").unwrap_or(rest.len());
        let group = &rest[..end];
        for (col_start, _) in group.match_indices("<Column") {
            let col_rest = &group[col_start..];
            let Some(gt) = col_rest.find('>') else {
                continue;
            };
            if let Some(name) = attr(&col_rest[..gt], "Name") {
                columns.push(strip_brackets(&name));
            }
        }
    }
    columns
}

fn collect_column_references(element: &str) -> Vec<String> {
    element
        .match_indices("<ColumnReference")
        .filter_map(|(start, _)| {
            let rest = &element[start..];
            let tag = &rest[..rest.find('>')?];
            attr(tag, "Column")
        })
        .collect()
}

fn sequential_edges(nodes: &[PlanNode]) -> Vec<PlanEdge> {
    nodes
        .windows(2)
        .map(|window| PlanEdge {
            from_node: window[0].id,
            to_node: window[1].id,
        })
        .collect()
}

/// Parent → child edges for every `<RelOp>` in a statement chunk, derived
/// from XML nesting: the parent of a RelOp is the nearest enclosing RelOp.
/// Returns edges in document order; nodes with no enclosing RelOp are roots.
fn nested_relop_edges(statement: &str) -> Vec<PlanEdge> {
    #[derive(Clone, Copy)]
    enum Event {
        Open(usize),
        Close(usize),
    }

    let mut events: Vec<Event> = statement
        .match_indices("<RelOp")
        .map(|(start, _)| Event::Open(start))
        .chain(
            statement
                .match_indices("</RelOp>")
                .map(|(start, _)| Event::Close(start)),
        )
        .collect();
    events.sort_by_key(|event| match event {
        Event::Open(offset) | Event::Close(offset) => *offset,
    });

    let mut stack: Vec<i32> = Vec::new();
    let mut edges = Vec::new();
    for event in events {
        match event {
            Event::Open(start) => {
                let rest = &statement[start..];
                let Some(gt) = rest.find('>') else {
                    continue;
                };
                let tag_text = &rest[..=gt];
                let id = attr(tag_text, "NodeId")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_default();
                if let Some(parent) = stack.last() {
                    edges.push(PlanEdge {
                        from_node: *parent,
                        to_node: id,
                    });
                }
                if !tag_text.ends_with("/>") {
                    stack.push(id);
                }
            }
            Event::Close(_) => {
                stack.pop();
            }
        }
    }
    edges
}

impl Default for PlanParser {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_statement_and_relop_attributes() {
        let plan = PlanParser::new()
            .parse_xml(r#"<ShowPlanXML><StmtSimple StatementText="SELECT &quot;x&quot;" StatementType="SELECT" StatementSubTreeCost="0.42"><RelOp NodeId="1" LogicalOp="Clustered Index Scan" PhysicalOp="Table Scan" EstimateRows="3" EstimatedTotalSubtreeCost="0.4" EstimateCPU="0.1" EstimateIO="0.2" /></StmtSimple></ShowPlanXML>"#)
            .unwrap();

        assert_eq!(plan.statement_text, "SELECT \"x\"");
        assert_eq!(plan.statement_type, "SELECT");
        assert_eq!(plan.total_subtree_cost, 0.42);
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.nodes[0].physical_op, "Table Scan");
        assert_eq!(plan.estimated_rows, 3.0);
        assert_eq!(plan.warnings.len(), 1);
    }

    /// SQL Server escapes the newlines and tabs inside `StatementText` as
    /// numeric character references. They used to survive into the UI as the
    /// literal text `&#xa;` / `&#x9;` in the plan's statement header.
    #[test]
    fn decodes_numeric_character_references_in_statement_text() {
        let plan = PlanParser::new()
            .parse_xml(r#"<ShowPlanXML><StmtSimple StatementText="SELECT&#xa;&#x9;@Offset = t.Utc * 60&#10;FROM Shared.Tenant t" StatementType="SELECT" StatementSubTreeCost="1"><RelOp NodeId="1" LogicalOp="Select" PhysicalOp="Compute Scalar" EstimateRows="1" EstimatedTotalSubtreeCost="1" /></StmtSimple></ShowPlanXML>"#)
            .unwrap();

        assert_eq!(
            plan.statement_text,
            "SELECT\n\t@Offset = t.Utc * 60\nFROM Shared.Tenant t"
        );
    }

    #[test]
    fn unescape_passes_through_what_it_cannot_decode() {
        // A bare ampersand, an unknown entity, and an out-of-range code point
        // all survive intact rather than eating the text around them.
        assert_eq!(unescape_xml("a & b"), "a & b");
        assert_eq!(unescape_xml("&unknown;"), "&unknown;");
        assert_eq!(unescape_xml("&#xFFFFFFFF;"), "&#xFFFFFFFF;");
        // A distant semicolon must not be mistaken for an entity terminator.
        assert_eq!(
            unescape_xml("WHERE a & b = 1; SELECT 2"),
            "WHERE a & b = 1; SELECT 2"
        );
        // Named and numeric forms mix freely, and `&amp;` is not re-expanded.
        assert_eq!(unescape_xml("&amp;quot;&#65;&#x42;"), "&quot;AB");
    }

    #[test]
    fn rejects_missing_statement() {
        assert!(matches!(
            PlanParser::new().parse_xml("<ShowPlanXML />"),
            Err(PlanError::InvalidStructure(_))
        ));
    }

    #[test]
    fn parses_actual_rows_missing_index_and_searchable_output() {
        let plan = PlanParser::new()
            .parse_xml(r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="2"><RelOp NodeId="1" LogicalOp="Select" PhysicalOp="Nested Loops" EstimateRows="4" EstimatedTotalSubtreeCost="2"><RunTimeCountersPerThread ActualRows="7" /><OutputList><ColumnReference Column="OrderId" /></OutputList><MissingIndexes><MissingIndexGroup Impact="88"><MissingIndex Database="[db]" Schema="[dbo]" Table="[Orders]" /></MissingIndexGroup></MissingIndexes></RelOp><RelOp NodeId="2" LogicalOp="Scan" PhysicalOp="Table Scan" EstimateRows="4" EstimatedTotalSubtreeCost="1" /></StmtSimple></ShowPlanXML>"#)
            .unwrap();

        assert_eq!(plan.nodes[0].actual_rows, Some(7.0));
        assert_eq!(plan.edges.len(), 1);
        assert!(plan.text_tree().contains("Nested Loops"));
        assert_eq!(plan.search("OrderId").len(), 1);
        assert!(
            plan.warnings
                .iter()
                .any(|warning| matches!(warning, PlanWarning::MissingIndex(_)))
        );
    }

    #[test]
    fn builds_visual_layout_for_graphical_viewer() {
        let plan = PlanParser::new()
            .parse_xml(r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="4"><RelOp NodeId="1" LogicalOp="Select" PhysicalOp="Nested Loops" EstimateRows="4" EstimatedTotalSubtreeCost="3"><RunTimeCountersPerThread ActualRows="7" /><OutputList><ColumnReference Column="OrderId" /></OutputList></RelOp><RelOp NodeId="2" LogicalOp="Scan" PhysicalOp="Table Scan" EstimateRows="4" EstimatedTotalSubtreeCost="1" /></StmtSimple></ShowPlanXML>"#)
            .unwrap();

        let layout = plan.visual_layout();

        assert_eq!(layout.nodes.len(), 2);
        assert_eq!(layout.nodes[0].label, "Nested Loops");
        assert_eq!(layout.nodes[0].actual_rows, Some(7.0));
        assert_eq!(layout.nodes[0].cost_percent, 75.0);
        assert!(layout.nodes[1].x > layout.nodes[0].x);
        assert_eq!(layout.edges.len(), 1);
        assert_eq!(layout.edges[0].from_node, 1);
        assert_eq!(layout.edges[0].to_node, 2);
        assert!(layout.width > layout.nodes[1].x);
    }

    #[test]
    fn multi_statement_batches_parse_into_separate_plans() {
        let xml = r#"<ShowPlanXML><StmtSimple StatementText="SELECT 1" StatementType="SELECT" StatementSubTreeCost="1"><RelOp NodeId="1" LogicalOp="Select" PhysicalOp="Compute Scalar" EstimateRows="1" EstimatedTotalSubtreeCost="1" /></StmtSimple><StmtSimple StatementText="UPDATE t SET a = 1" StatementType="UPDATE" StatementSubTreeCost="2"><RelOp NodeId="5" LogicalOp="Update" PhysicalOp="Clustered Index Update" EstimateRows="9" EstimatedTotalSubtreeCost="2" /></StmtSimple></ShowPlanXML>"#;
        let plans = PlanParser::new().parse_xml_multi(xml).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[0].statement_type, "SELECT");
        assert_eq!(plans[0].nodes.len(), 1);
        assert_eq!(plans[0].nodes[0].physical_op, "Compute Scalar");
        assert_eq!(plans[1].statement_type, "UPDATE");
        assert_eq!(plans[1].nodes.len(), 1);
        assert_eq!(plans[1].nodes[0].id, 5);
        // Single-statement path stays the first statement for compatibility.
        let first = PlanParser::new().parse_xml(xml).unwrap();
        assert_eq!(first.statement_text, "SELECT 1");
    }

    #[test]
    fn missing_index_columns_are_extracted_per_usage() {
        let xml = r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="2"><RelOp NodeId="1" LogicalOp="Scan" PhysicalOp="Index Scan" EstimateRows="4" EstimatedTotalSubtreeCost="2"><MissingIndexes><MissingIndexGroup Impact="92.9"><MissingIndex Database="[db]" Schema="[dbo]" Table="[Orders]"><ColumnGroup Usage="EQUALITY"><Column Name="[CustomerId]" ColumnId="2" /></ColumnGroup><ColumnGroup Usage="INEQUALITY"><Column Name="[OrderDate]" ColumnId="3" /></ColumnGroup><ColumnGroup Usage="INCLUDE"><Column Name="[Total]" ColumnId="5" /></ColumnGroup></MissingIndex></MissingIndexGroup></MissingIndexes></RelOp></StmtSimple></ShowPlanXML>"#;
        let plan = PlanParser::new().parse_xml(xml).unwrap();
        let Some(PlanWarning::MissingIndex(index)) = plan.warnings.first() else {
            panic!("expected a missing-index warning, got {:?}", plan.warnings);
        };
        assert_eq!(index.equality_columns, vec!["CustomerId"]);
        assert_eq!(index.inequality_columns, vec!["OrderDate"]);
        assert_eq!(index.included_columns, vec!["Total"]);
        assert_eq!(index.impact, 92.9);
    }

    #[test]
    fn table_scan_and_key_lookup_pick_up_object_names() {
        let xml = r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="2"><RelOp NodeId="1" LogicalOp="Scan" PhysicalOp="Table Scan" EstimateRows="4" EstimatedTotalSubtreeCost="1"><Object Database="[db]" Schema="[dbo]" Table="[Orders]" /></RelOp><RelOp NodeId="2" LogicalOp="Lookup" PhysicalOp="Key Lookup" EstimateRows="1" EstimatedTotalSubtreeCost="1"><Object Database="[db]" Schema="[dbo]" Table="[Orders]" Index="[PK_Orders]" /></RelOp></StmtSimple></ShowPlanXML>"#;
        let plan = PlanParser::new().parse_xml(xml).unwrap();
        assert!(plan.warnings.iter().any(|w| matches!(
            w,
            PlanWarning::TableScan { table, schema } if table == "Orders" && schema == "dbo"
        )));
        assert!(plan.warnings.iter().any(|w| matches!(
            w,
            PlanWarning::KeyLookup { table, index } if table == "Orders" && index == "PK_Orders"
        )));
    }

    #[test]
    fn nested_relops_yield_parent_child_edges() {
        let xml = r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="3"><RelOp NodeId="0" LogicalOp="Join" PhysicalOp="Hash Match" EstimateRows="10" EstimatedTotalSubtreeCost="3"><RelOp NodeId="1" LogicalOp="Scan" PhysicalOp="Table Scan" EstimateRows="4" EstimatedTotalSubtreeCost="1"></RelOp><RelOp NodeId="2" LogicalOp="Scan" PhysicalOp="Index Scan" EstimateRows="6" EstimatedTotalSubtreeCost="1"></RelOp></RelOp></StmtSimple></ShowPlanXML>"#;
        let plan = PlanParser::new().parse_xml(xml).unwrap();
        assert_eq!(plan.nodes.len(), 3);
        let pairs: Vec<(i32, i32)> = plan
            .edges
            .iter()
            .map(|e| (e.from_node, e.to_node))
            .collect();
        assert_eq!(pairs, vec![(0, 1), (0, 2)]);
        assert!(
            !plan
                .edges
                .iter()
                .any(|e| e.from_node == 1 && e.to_node == 2),
            "siblings must not be chained: {pairs:?}"
        );
    }

    #[test]
    fn spill_and_missing_statistics_are_detected() {
        let xml = r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="2"><RelOp NodeId="1" LogicalOp="Sort" PhysicalOp="Sort" EstimateRows="4" EstimatedTotalSubtreeCost="1"><Warnings><SpillToTempDb SpillLevel="1" /></Warnings></RelOp><RelOp NodeId="2" LogicalOp="Scan" PhysicalOp="Index Scan" EstimateRows="4" EstimatedTotalSubtreeCost="1"><Warnings><ColumnsWithNoStatistics><ColumnReference Column="[a]" /></ColumnsWithNoStatistics></Warnings></RelOp></StmtSimple></ShowPlanXML>"#;
        let plan = PlanParser::new().parse_xml(xml).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::SpillToTempDb))
        );
        assert!(
            plan.warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::CardinalityEstimation))
        );
    }
}
