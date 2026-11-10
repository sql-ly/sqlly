//! SQL Server ShowPlan XML (`SET STATISTICS XML`, `SET SHOWPLAN_XML`, and
//! the `.sqlplan` files SSMS saves) into the shared plan model.
//!
//! The document is parsed as real XML, so every fact is read from the element
//! it belongs to. A `<RelOp>` owns its attributes, its `<OutputList>`,
//! `<Warnings>`, `<RunTimeInformation>` and the `<Object>` it reads, but not
//! anything under the `<RelOp>`s nested inside it: a scan's warning stays on
//! the scan instead of also landing on every operator above it.
//!
//! Some facts live on the statement rather than an operator. ShowPlan puts
//! `<MissingIndexes>` and the `PlanAffectingConvert` warnings on the
//! `<QueryPlan>`; they are attached to the operator they describe (the
//! costliest reader of the table, or the operator whose predicate contains
//! the conversion) and to the root operator when no operator matches.

use roxmltree::{Document, Node, ParsingOptions};

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
    /// statement instead of mixing every operator into one list. Statements
    /// SQL Server compiled without a plan (`SET`, `DECLARE`) come back with
    /// no operators.
    ///
    /// `SET STATISTICS XML` returns one ShowPlan document per statement, so a
    /// copied batch is often several documents back to back; each is parsed
    /// and their statements are returned in order.
    pub fn parse_xml_multi(&self, xml: &str) -> PlanResult<Vec<ExecutionPlan>> {
        let text = xml.trim_start_matches('\u{feff}').trim();
        if text.is_empty() {
            return Err(PlanError::EmptyPlan);
        }
        let mut plans = Vec::new();
        for document in split_documents(text) {
            let options = ParsingOptions {
                allow_dtd: false,
                ..ParsingOptions::default()
            };
            let doc = Document::parse_with_options(document, options)
                .map_err(|e| PlanError::XmlParse(e.to_string()))?;
            plans.extend(
                doc.descendants()
                    .filter(|node| is(node, "StmtSimple"))
                    .map(parse_statement),
            );
        }
        if plans.is_empty() {
            return Err(PlanError::InvalidStructure(
                "missing StmtSimple element".to_string(),
            ));
        }
        Ok(plans)
    }
}

impl Default for PlanParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Split back-to-back ShowPlan documents. Each `<ShowPlanXML` opens a new
/// document (an `<?xml …?>` declaration right before it stays with it); a
/// text with one root comes back whole.
fn split_documents(text: &str) -> Vec<&str> {
    let mut starts: Vec<usize> = text
        .match_indices("<ShowPlanXML")
        .map(|(start, _)| {
            let before = text[..start].trim_end();
            match before.rfind("<?xml") {
                Some(decl) if before.ends_with("?>") && !before[decl + 1..].contains('<') => decl,
                _ => start,
            }
        })
        .collect();
    if starts.len() <= 1 {
        return vec![text];
    }
    starts[0] = 0;
    starts
        .iter()
        .enumerate()
        .map(|(i, &start)| {
            let end = starts.get(i + 1).copied().unwrap_or(text.len());
            text[start..end].trim()
        })
        .filter(|chunk| !chunk.is_empty())
        .collect()
}

/// Strip one layer of surrounding brackets: `[dbo]` → `dbo`. ShowPlan XML
/// quotes every identifier; findings read better without the noise.
pub(crate) fn strip_brackets(name: &str) -> String {
    name.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string()
}

/// Element test by local name: ShowPlan documents carry the
/// `http://schemas.microsoft.com/sqlserver/2004/07/showplan` default
/// namespace, hand-written ones often none.
fn is(node: &Node, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}

fn number(node: &Node, name: &str) -> Option<f64> {
    node.attribute(name)?.trim().parse::<f64>().ok()
}

/// `xs:boolean`: ShowPlan writes `true`/`false`, older tools `1`/`0`.
fn flag(node: &Node, name: &str) -> bool {
    matches!(node.attribute(name).map(str::trim), Some("1" | "true"))
}

/// Statement-like elements: a `<RelOp>` under one of these belongs to that
/// statement, not to the statement that encloses it (stored procedure calls
/// nest their own statements inside the caller's).
fn is_statement(node: &Node) -> bool {
    node.is_element() && node.tag_name().name().starts_with("Stmt")
}

/// The elements that belong to `owner` itself: its descendants minus
/// anything inside a nested element that `stop` claims (a nested `<RelOp>`
/// for an operator, a nested statement for a statement).
fn own_descendants<'a, 'input>(
    owner: Node<'a, 'input>,
    stop: fn(&Node) -> bool,
) -> Vec<Node<'a, 'input>> {
    let mut out = Vec::new();
    let mut stack: Vec<Node> = owner.children().filter(Node::is_element).collect();
    stack.reverse();
    while let Some(node) = stack.pop() {
        if stop(&node) {
            continue;
        }
        out.push(node);
        let mut kids: Vec<Node> = node.children().filter(Node::is_element).collect();
        kids.reverse();
        stack.extend(kids);
    }
    out
}

fn is_relop(node: &Node) -> bool {
    is(node, "RelOp")
}

/// Every `<RelOp>` of `statement`, in document order, with its parent
/// operator (the nearest enclosing `<RelOp>`).
fn statement_relops<'a, 'input>(
    statement: Node<'a, 'input>,
) -> Vec<(Node<'a, 'input>, Option<Node<'a, 'input>>)> {
    let mut out = Vec::new();
    let mut stack: Vec<(Node, Option<Node>)> = statement
        .children()
        .filter(Node::is_element)
        .map(|n| (n, None))
        .collect();
    stack.reverse();
    while let Some((node, parent)) = stack.pop() {
        if is_statement(&node) {
            continue;
        }
        let parent_for_children = if is_relop(&node) {
            out.push((node, parent));
            Some(node)
        } else {
            parent
        };
        let mut kids: Vec<(Node, Option<Node>)> = node
            .children()
            .filter(Node::is_element)
            .map(|n| (n, parent_for_children))
            .collect();
        kids.reverse();
        stack.extend(kids);
    }
    out
}

/// What one operator reads and says about itself, before statement-level
/// facts are attached.
struct Operator {
    node: PlanNode,
    /// Bracket-stripped `(schema, table)` of the operator's own `<Object>`.
    object: Option<(String, String)>,
    /// The operator's own predicate and expression text (`ScalarString`),
    /// used to find the operator a statement-level conversion warning means.
    scalar_text: String,
}

/// Parse a single `<StmtSimple>` into a plan. Operators, warnings, and
/// missing indexes are scoped to this statement so multi-statement batches
/// stay grouped per statement.
fn parse_statement(statement: Node) -> ExecutionPlan {
    let statement_text = statement
        .attribute("StatementText")
        .unwrap_or_default()
        .to_string();
    let statement_type = statement
        .attribute("StatementType")
        .unwrap_or_default()
        .to_string();
    let total_subtree_cost = number(&statement, "StatementSubTreeCost").unwrap_or_default();

    let relops = statement_relops(statement);
    let mut row_goal_seen = false;
    let mut operators: Vec<Operator> = relops
        .iter()
        .map(|(relop, _)| {
            let mut op = parse_relop(*relop);
            // Every operator under a row goal carries the attribute; the
            // finding belongs once, on the topmost.
            if relop.attribute("EstimateRowsWithoutRowGoal").is_some() && !row_goal_seen {
                row_goal_seen = true;
                op.node.warnings.push(PlanWarning::RowGoal);
            }
            op
        })
        .collect();

    let mut edges: Vec<PlanEdge> = relops
        .iter()
        .filter_map(|(relop, parent)| {
            let parent = parent.as_ref()?;
            Some(PlanEdge {
                from_node: node_id(parent),
                to_node: node_id(relop),
            })
        })
        .collect();
    if edges.is_empty() && operators.len() > 1 {
        edges = sequential_edges(&operators);
    }

    attach_statement_warnings(statement, &mut operators);

    let nodes: Vec<PlanNode> = operators.into_iter().map(|op| op.node).collect();
    let estimated_rows = number(&statement, "StatementEstRows")
        .unwrap_or_else(|| nodes.iter().map(|node| node.estimated_rows).sum());
    let warnings = nodes
        .iter()
        .flat_map(|node| node.warnings.clone())
        .collect::<Vec<_>>();

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

fn node_id(relop: &Node) -> i32 {
    relop
        .attribute("NodeId")
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_default()
}

fn parse_relop(relop: Node) -> Operator {
    let own = own_descendants(relop, is_relop);
    let physical_op = relop
        .attribute("PhysicalOp")
        .unwrap_or_default()
        .to_string();
    let logical_op = relop.attribute("LogicalOp").unwrap_or_default().to_string();

    // Runtime counters come one element per thread; a parallel operator's
    // real row count is the sum (thread 0 of a parallel zone reads nothing).
    let mut actual_rows: Option<f64> = None;
    let mut rebinds: Option<i32> = None;
    let mut rewinds: Option<i32> = None;
    for counter in own.iter().filter(|n| is(n, "RunTimeCountersPerThread")) {
        if let Some(rows) = number(counter, "ActualRows") {
            *actual_rows.get_or_insert(0.0) += rows;
        }
        if let Some(n) = number(counter, "ActualRebinds") {
            *rebinds.get_or_insert(0) += n as i32;
        }
        if let Some(n) = number(counter, "ActualRewinds") {
            *rewinds.get_or_insert(0) += n as i32;
        }
    }

    // `EstimateRows` is per execution. On an actual plan the comparison is
    // against the rows of every execution, so scale by the estimated
    // execution count (1 + rebinds + rewinds) — otherwise the inner side of
    // every nested loop reads as a wild misestimate.
    let per_execution = number(&relop, "EstimateRows").unwrap_or_default();
    let estimated_rows = if actual_rows.is_some() {
        let executions = 1.0
            + number(&relop, "EstimateRebinds").unwrap_or_default()
            + number(&relop, "EstimateRewinds").unwrap_or_default();
        per_execution * executions.max(1.0)
    } else {
        per_execution
    };

    let object = own.iter().find(|n| is(n, "Object")).map(|object| {
        (
            strip_brackets(object.attribute("Schema").unwrap_or_default()),
            strip_brackets(object.attribute("Table").unwrap_or_default()),
        )
    });
    let index = own
        .iter()
        .find(|n| is(n, "Object"))
        .and_then(|object| object.attribute("Index"))
        .map(strip_brackets)
        .unwrap_or_default();
    let index_scan = own.iter().find(|n| is(n, "IndexScan"));

    let mut warnings = Vec::new();
    if own.iter().any(|n| flag(n, "NoJoinPredicate")) {
        warnings.push(PlanWarning::NoJoinPredicate);
    }
    let (schema, table) = object.clone().unwrap_or_default();
    // A heap scan always reads every row; an index scan only "searches" the
    // whole table when it filters with a residual predicate.
    let filtering_scan = matches!(physical_op.as_str(), "Clustered Index Scan" | "Index Scan")
        && index_scan.is_some_and(|scan| scan.children().any(|c| is(&c, "Predicate")));
    if physical_op == "Table Scan" || filtering_scan {
        warnings.push(PlanWarning::TableScan {
            table: table.clone(),
            schema: schema.clone(),
        });
    }
    let lookup = physical_op == "Key Lookup"
        || physical_op == "RID Lookup"
        || index_scan.is_some_and(|scan| flag(scan, "Lookup"));
    if lookup {
        warnings.push(PlanWarning::KeyLookup {
            table: table.clone(),
            index: index.clone(),
        });
    }
    for group in own.iter().filter(|n| is(n, "MissingIndexGroup")) {
        warnings.extend(missing_indexes(group));
    }
    for convert in own.iter() {
        if let Some(warning) = conversion_warning(convert) {
            warnings.push(warning);
        }
    }
    let spills = own.iter().any(|n| {
        is(n, "SpillToTempDb")
            || is(n, "SortSpillDetails")
            || is(n, "HashSpillDetails")
            || is(n, "ExchangeSpillDetails")
            || (is(n, "SpillOccurred") && flag(n, "Detail"))
    });
    if spills {
        warnings.push(PlanWarning::SpillToTempDb);
    }
    if own.iter().any(|n| is(n, "ColumnsWithNoStatistics")) {
        warnings.push(PlanWarning::CardinalityEstimation);
    }
    if matches!(physical_op.as_str(), "Table Spool" | "Index Spool") {
        warnings.push(PlanWarning::Spool);
    }

    let output_list = own
        .iter()
        .filter(|n| is(n, "OutputList"))
        .flat_map(|list| list.children().filter(|c| is(c, "ColumnReference")))
        .filter_map(|column| column_label(&column))
        .collect();
    let scalar_text = own
        .iter()
        .filter(|n| is(n, "ScalarOperator"))
        .filter_map(|n| n.attribute("ScalarString"))
        .collect::<Vec<_>>()
        .join("\n");

    Operator {
        node: PlanNode {
            id: node_id(&relop),
            logical_op,
            physical_op,
            estimated_rows,
            estimated_cost: number(&relop, "EstimatedTotalSubtreeCost").unwrap_or_default(),
            estimated_cpu: number(&relop, "EstimateCPU").unwrap_or_default(),
            estimated_io: number(&relop, "EstimateIO").unwrap_or_default(),
            actual_rows,
            actual_rewinds: rewinds,
            actual_rebinds: rebinds,
            row_count: actual_rows.map(|rows| rows as i32),
            output_list,
            warnings,
        },
        object,
        scalar_text,
    }
}

/// `alias.Column` (or `Table.Column`) for an output column; computed
/// expressions (`Expr1003`) have no table.
fn column_label(column: &Node) -> Option<String> {
    let name = strip_brackets(column.attribute("Column")?);
    let owner = column
        .attribute("Alias")
        .or_else(|| column.attribute("Table"))
        .map(strip_brackets)
        .filter(|owner| !owner.is_empty());
    Some(match owner {
        Some(owner) => format!("{owner}.{name}"),
        None => name,
    })
}

/// `<PlanAffectingConvert ConvertIssue="Seek Plan" Expression="…">`. Only a
/// seek-plan issue stops an index seek; a cardinality-estimate issue is
/// reported as what it is.
fn conversion_warning(node: &Node) -> Option<PlanWarning> {
    let issue = node.attribute("ConvertIssue")?;
    let expression = node.attribute("Expression").unwrap_or_default().to_string();
    if issue.eq_ignore_ascii_case("Cardinality Estimate") {
        Some(PlanWarning::Other(if expression.is_empty() {
            "A type conversion may affect the cardinality estimate".to_string()
        } else {
            format!("Type conversion in {expression} may affect the cardinality estimate")
        }))
    } else {
        Some(PlanWarning::ImplicitConversion { expression })
    }
}

fn missing_indexes(group: &Node) -> Vec<PlanWarning> {
    let impact = number(group, "Impact").unwrap_or_default();
    group
        .descendants()
        .filter(|n| is(n, "MissingIndex"))
        .map(|index| {
            let columns = |usage: &str| -> Vec<String> {
                index
                    .children()
                    .filter(|g| is(g, "ColumnGroup") && g.attribute("Usage") == Some(usage))
                    .flat_map(|g| g.children().filter(|c| is(c, "Column")))
                    .filter_map(|c| c.attribute("Name").map(strip_brackets))
                    .collect()
            };
            PlanWarning::MissingIndex(MissingIndex {
                database: index.attribute("Database").unwrap_or_default().to_string(),
                schema: index.attribute("Schema").unwrap_or_default().to_string(),
                table: index.attribute("Table").unwrap_or_default().to_string(),
                equality_columns: columns("EQUALITY"),
                inequality_columns: columns("INEQUALITY"),
                included_columns: columns("INCLUDE"),
                impact,
            })
        })
        .collect()
}

/// Facts ShowPlan records on the `<QueryPlan>` rather than an operator:
/// missing indexes, plan-affecting conversions, and memory grant warnings.
fn attach_statement_warnings(statement: Node, operators: &mut [Operator]) {
    if operators.is_empty() {
        return;
    }
    let in_statement = own_descendants(statement, |n| is_statement(n) || is_relop(n));
    for node in &in_statement {
        if is(node, "MissingIndexGroup") {
            for warning in missing_indexes(node) {
                let target = match &warning {
                    PlanWarning::MissingIndex(index) => {
                        reader_of(operators, &index.schema, &index.table)
                    }
                    _ => 0,
                };
                operators[target].node.warnings.push(warning);
            }
        } else if let Some(warning) = conversion_warning(node) {
            let target = match &warning {
                PlanWarning::ImplicitConversion { expression } => {
                    converting_operator(operators, expression)
                }
                _ => 0,
            };
            operators[target].node.warnings.push(warning);
        } else if is(node, "MemoryGrantWarning") {
            let kind = node.attribute("GrantWarningKind").unwrap_or("Memory grant");
            let detail = match (
                number(node, "RequestedMemory"),
                number(node, "GrantedMemory"),
                number(node, "MaxUsedMemory"),
            ) {
                (Some(requested), Some(granted), Some(used)) => format!(
                    "{kind}: requested {requested:.0} KB, granted {granted:.0} KB, used {used:.0} KB"
                ),
                _ => kind.to_string(),
            };
            operators[0].node.warnings.push(PlanWarning::Other(detail));
        }
    }
}

/// The operator a statement-level `PlanAffectingConvert` is about: the one
/// whose own predicate converts the same column. The warning spells the
/// column by alias (`CONVERT_IMPLICIT(nvarchar(200),[c].[Email],0)`) while
/// the predicate spells it in full (`…[Customers].[Email] as [c].[Email]`),
/// so the match is on the converted column, not the whole expression.
fn converting_operator(operators: &[Operator], expression: &str) -> usize {
    let converts = |op: &&Operator| op.scalar_text.contains("CONVERT_IMPLICIT");
    let column = converted_column(expression);
    operators
        .iter()
        .position(|op| {
            converts(&op)
                && (op.scalar_text.contains(expression)
                    || column.is_some_and(|column| op.scalar_text.contains(column)))
        })
        .or_else(|| operators.iter().position(|op| converts(&op)))
        .unwrap_or(0)
}

/// The second argument of the first `CONVERT_IMPLICIT(type,column,style)`.
fn converted_column(expression: &str) -> Option<&str> {
    let start = expression.find("CONVERT_IMPLICIT(")? + "CONVERT_IMPLICIT(".len();
    let rest = &expression[start..];
    // The type may carry its own parentheses: nvarchar(200).
    let mut depth = 0usize;
    let mut first_comma = None;
    for (i, ch) in rest.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' if depth == 0 => break,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                first_comma = Some(i);
                break;
            }
            _ => {}
        }
    }
    let after = &rest[first_comma? + 1..];
    let end = after.find([',', ')']).unwrap_or(after.len());
    let column = after[..end].trim();
    (!column.is_empty()).then_some(column)
}

/// The costliest operator that reads `schema.table` itself, else the root.
fn reader_of(operators: &[Operator], schema: &str, table: &str) -> usize {
    let schema = strip_brackets(schema);
    let table = strip_brackets(table);
    operators
        .iter()
        .enumerate()
        .filter(|(_, op)| {
            op.object.as_ref().is_some_and(|(s, t)| {
                t.eq_ignore_ascii_case(&table)
                    && (schema.is_empty() || s.eq_ignore_ascii_case(&schema))
            })
        })
        .max_by(|(_, a), (_, b)| a.node.estimated_cost.total_cmp(&b.node.estimated_cost))
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn sequential_edges(operators: &[Operator]) -> Vec<PlanEdge> {
    operators
        .windows(2)
        .map(|window| PlanEdge {
            from_node: window[0].node.id,
            to_node: window[1].node.id,
        })
        .collect()
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

    /// The shape SQL Server 2025 really emits (trimmed): namespace, missing
    /// indexes and plan-affecting converts on the `<QueryPlan>`, operators
    /// nested inside their parents, runtime counters per thread.
    const REAL_SHAPE: &str = r#"<?xml version="1.0" encoding="utf-16"?>
<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" Version="1.599" Build="17.0.4035.5"><BatchSequence><Batch><Statements>
<StmtSimple StatementText="SELECT c.Email, o.Total&#xa;FROM dbo.Customers AS c JOIN dbo.Orders AS o ON o.CustomerID = c.CustomerID WHERE c.Email = @email" StatementId="1" StatementType="SELECT" StatementSubTreeCost="3.45" StatementEstRows="12.04">
<QueryPlan DegreeOfParallelism="1">
<MissingIndexes><MissingIndexGroup Impact="91.8066"><MissingIndex Database="[Shop]" Schema="[dbo]" Table="[Orders]"><ColumnGroup Usage="EQUALITY"><Column Name="[CustomerID]" ColumnId="2"/></ColumnGroup><ColumnGroup Usage="INCLUDE"><Column Name="[Total]" ColumnId="5"/></ColumnGroup></MissingIndex></MissingIndexGroup></MissingIndexes>
<Warnings><PlanAffectingConvert ConvertIssue="Seek Plan" Expression="CONVERT_IMPLICIT(nvarchar(200),[c].[Email],0)=[@email]"/><PlanAffectingConvert ConvertIssue="Cardinality Estimate" Expression="CONVERT_IMPLICIT(nvarchar(200),[c].[Email],0)"/><MemoryGrantWarning GrantWarningKind="Excessive Grant" RequestedMemory="67776" GrantedMemory="67776" MaxUsedMemory="1360"/></Warnings>
<RelOp NodeId="1" PhysicalOp="Hash Match" LogicalOp="Inner Join" EstimateRows="12.04" EstimatedTotalSubtreeCost="3.45" EstimateCPU="0.01" EstimateIO="0" EstimateRebinds="0" EstimateRewinds="0">
<OutputList><ColumnReference Database="[Shop]" Schema="[dbo]" Table="[Customers]" Alias="[c]" Column="Email"/></OutputList>
<RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="12" ActualExecutions="1"/></RunTimeInformation>
<Hash>
<RelOp NodeId="3" PhysicalOp="Index Scan" LogicalOp="Index Scan" EstimateRows="1" EstimatedTotalSubtreeCost="0.2" EstimateRebinds="0" EstimateRewinds="0">
<OutputList><ColumnReference Database="[Shop]" Schema="[dbo]" Table="[Customers]" Alias="[c]" Column="CustomerID"/></OutputList>
<RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="1"/></RunTimeInformation>
<IndexScan Ordered="0"><Object Database="[Shop]" Schema="[dbo]" Table="[Customers]" Index="[IX_Customers_Email]" Alias="[c]"/><Predicate><ScalarOperator ScalarString="CONVERT_IMPLICIT(nvarchar(200),[Shop].[dbo].[Customers].[Email] as [c].[Email],0)=[@email]"/></Predicate></IndexScan>
</RelOp>
<RelOp NodeId="5" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="12" EstimatedTotalSubtreeCost="3.1" EstimateRebinds="0" EstimateRewinds="0">
<OutputList><ColumnReference Database="[Shop]" Schema="[dbo]" Table="[Orders]" Alias="[o]" Column="Total"/></OutputList>
<RunTimeInformation><RunTimeCountersPerThread Thread="1" ActualRows="7"/><RunTimeCountersPerThread Thread="2" ActualRows="5"/><RunTimeCountersPerThread Thread="0" ActualRows="0"/></RunTimeInformation>
<IndexScan Ordered="0"><Object Database="[Shop]" Schema="[dbo]" Table="[Orders]" Index="[PK_Orders]" Alias="[o]"/></IndexScan>
</RelOp>
</Hash></RelOp></QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#;

    /// `TOP (20) … ORDER BY` over a nonclustered index: a row goal and a key
    /// lookup that runs once per row. Its own document, as `SET STATISTICS
    /// XML` returns one per statement.
    const LOOKUP_DOC: &str = r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" Version="1.599"><BatchSequence><Batch><Statements><StmtSimple StatementText="SELECT TOP (20) OrderID, Total FROM dbo.Orders ORDER BY OrderDate" StatementType="SELECT" StatementSubTreeCost="0.07"><QueryPlan>
<RelOp NodeId="0" PhysicalOp="Top" LogicalOp="Top" EstimateRows="20" EstimatedTotalSubtreeCost="0.07"><RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="20"/></RunTimeInformation><Top>
<RelOp NodeId="1" PhysicalOp="Nested Loops" LogicalOp="Inner Join" EstimateRows="20" EstimateRowsWithoutRowGoal="224000" EstimatedTotalSubtreeCost="0.07"><RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="20"/></RunTimeInformation><NestedLoops Optimized="0">
<RelOp NodeId="3" PhysicalOp="Index Seek" LogicalOp="Index Seek" EstimateRows="20" EstimateRowsWithoutRowGoal="224000" EstimatedTotalSubtreeCost="0.003"><RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="20"/></RunTimeInformation><IndexScan Ordered="1"><Object Database="[Shop]" Schema="[dbo]" Table="[Orders]" Index="[IX_Orders_OrderDate]"/></IndexScan></RelOp>
<RelOp NodeId="5" PhysicalOp="Clustered Index Seek" LogicalOp="Clustered Index Seek" EstimateRows="1" EstimateRebinds="19.9998" EstimateRewinds="0" EstimatedTotalSubtreeCost="0.06"><RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="20" ActualExecutions="20"/></RunTimeInformation><IndexScan Lookup="1" Ordered="1"><Object Database="[Shop]" Schema="[dbo]" Table="[Orders]" Index="[PK_Orders]" TableReferenceId="-1" IndexKind="Clustered"/></IndexScan></RelOp>
</NestedLoops></RelOp></Top></RelOp></QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#;

    fn node(plan: &ExecutionPlan, id: i32) -> &PlanNode {
        plan.nodes.iter().find(|n| n.id == id).expect("node")
    }

    #[test]
    fn statement_level_missing_index_lands_on_the_table_reader() {
        let plan = PlanParser::new().parse_xml(REAL_SHAPE).unwrap();
        assert_eq!(plan.estimated_rows, 12.04);
        let orders = node(&plan, 5);
        let Some(PlanWarning::MissingIndex(index)) = orders
            .warnings
            .iter()
            .find(|w| matches!(w, PlanWarning::MissingIndex(_)))
        else {
            panic!("missing index not on the Orders scan: {:?}", plan.nodes);
        };
        assert_eq!(index.equality_columns, vec!["CustomerID"]);
        assert_eq!(index.included_columns, vec!["Total"]);
        assert_eq!(index.impact, 91.8066);
        assert_eq!(
            plan.warnings
                .iter()
                .filter(|w| matches!(w, PlanWarning::MissingIndex(_)))
                .count(),
            1
        );
    }

    #[test]
    fn seek_plan_conversion_lands_on_the_converting_scan() {
        let plan = PlanParser::new().parse_xml(REAL_SHAPE).unwrap();
        let customers = node(&plan, 3);
        assert!(customers.warnings.iter().any(|w| matches!(
            w,
            PlanWarning::ImplicitConversion { expression } if expression.starts_with("CONVERT_IMPLICIT(nvarchar(200),[c].[Email]")
        )), "{:?}", customers.warnings);
        // The cardinality-estimate flavor is reported as such, never as a
        // blocked seek.
        let implicit = plan
            .warnings
            .iter()
            .filter(|w| matches!(w, PlanWarning::ImplicitConversion { .. }))
            .count();
        assert_eq!(implicit, 1);
        assert!(plan.warnings.iter().any(
            |w| matches!(w, PlanWarning::Other(text) if text.contains("cardinality estimate"))
        ));
        assert!(node(&plan, 1).warnings.iter().any(
            |w| matches!(w, PlanWarning::Other(text) if text.starts_with("Excessive Grant: requested 67776 KB"))
        ));
    }

    #[test]
    fn child_warnings_do_not_leak_onto_parents_and_threads_sum() {
        let plan = PlanParser::new().parse_xml(REAL_SHAPE).unwrap();
        let join = node(&plan, 1);
        assert!(
            !join.warnings.iter().any(|w| matches!(
                w,
                PlanWarning::ImplicitConversion { .. } | PlanWarning::MissingIndex(_)
            )),
            "{:?}",
            join.warnings
        );
        // Parallel counters: 7 + 5 + 0.
        assert_eq!(node(&plan, 5).actual_rows, Some(12.0));
        assert_eq!(join.output_list, vec!["c.Email"]);
        let edges: Vec<(i32, i32)> = plan
            .edges
            .iter()
            .map(|e| (e.from_node, e.to_node))
            .collect();
        assert_eq!(edges, vec![(1, 3), (1, 5)]);
        assert_eq!(
            plan.statement_text,
            "SELECT c.Email, o.Total\nFROM dbo.Customers AS c JOIN dbo.Orders AS o ON o.CustomerID = c.CustomerID WHERE c.Email = @email"
        );
    }

    #[test]
    fn lookups_row_goals_and_back_to_back_documents() {
        let batch = format!("{REAL_SHAPE}\n{LOOKUP_DOC}\n");
        let plans = PlanParser::new().parse_xml_multi(&batch).unwrap();
        assert_eq!(plans.len(), 2);
        let plan = &plans[1];
        let lookup = node(plan, 5);
        assert!(lookup.warnings.iter().any(|w| matches!(
            w,
            PlanWarning::KeyLookup { table, index } if table == "Orders" && index == "PK_Orders"
        )));
        // 1 row per execution × ~21 estimated executions, against 20 read:
        // no false misestimate on the inner side of the loop.
        assert!(
            (lookup.estimated_rows - 20.9998).abs() < 1e-6,
            "{}",
            lookup.estimated_rows
        );
        let row_goals = plan
            .warnings
            .iter()
            .filter(|w| matches!(w, PlanWarning::RowGoal))
            .count();
        assert_eq!(row_goals, 1);
        assert!(
            node(plan, 1)
                .warnings
                .iter()
                .any(|w| matches!(w, PlanWarning::RowGoal))
        );
    }

    #[test]
    fn malformed_xml_is_a_parse_error() {
        let truncated = &REAL_SHAPE[..REAL_SHAPE.len() / 2];
        assert!(matches!(
            PlanParser::new().parse_xml(truncated),
            Err(PlanError::XmlParse(_))
        ));
    }
}
