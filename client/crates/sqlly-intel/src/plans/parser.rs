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
    dialects::{PlanDialect, common},
    model::{
        PlanBuffers, PlanMemoryGrant, PlanParameter, PlanProperty, PlanSpill, PlanStatisticsUsage,
    },
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

    /// Parse every statement in the document into its own plan. A batch
    /// like `SELECT …; UPDATE …` produces one ShowPlan statement per SQL
    /// statement; keeping them separate lets the viewer group findings per
    /// statement instead of mixing every operator into one list. Statements
    /// SQL Server compiled without a plan (`SET`, `DECLARE`) come back with
    /// no operators.
    ///
    /// Besides `<StmtSimple>` this reads `<StmtCond>` (the query inside an
    /// `IF` condition; the statements of its branches are separate plans)
    /// and `<StmtCursor>` (one plan per cursor operation that has operators,
    /// since each operation numbers its operators from zero).
    ///
    /// `SET STATISTICS XML` returns one ShowPlan document per statement, so a
    /// copied batch is often several documents back to back; each is parsed
    /// and their statements are returned in order. Text before, between and
    /// after the documents (result rows, `(1 rows affected)`) is ignored.
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
            for statement in doc.descendants().filter(is_statement) {
                plans.extend(parse_statement(statement));
            }
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

/// Cut the ShowPlan documents out of `text`. Each `<ShowPlanXML` opens a
/// document (an `<?xml …?>` declaration right before it stays with it) that
/// runs to its `</ShowPlanXML>`, so whatever a client printed around the
/// documents is dropped. A text without that root element comes back whole
/// and fails in the XML parser with a real error.
fn split_documents(text: &str) -> Vec<&str> {
    const OPEN: &str = "<ShowPlanXML";
    const CLOSE: &str = "</ShowPlanXML>";
    let roots: Vec<usize> = text.match_indices(OPEN).map(|(start, _)| start).collect();
    if roots.is_empty() {
        return vec![text];
    }
    let mut out = Vec::new();
    for (i, &root) in roots.iter().enumerate() {
        let limit = roots.get(i + 1).copied().unwrap_or(text.len());
        let before = text[..root].trim_end();
        let start = match before.rfind("<?xml") {
            Some(decl) if before.ends_with("?>") && !before[decl + 1..].contains('<') => decl,
            _ => root,
        };
        let end = match text[root..limit].find(CLOSE) {
            Some(close) => root + close + CLOSE.len(),
            // `<ShowPlanXML />`, or a document cut short: the parser decides.
            None => match text[root..limit].find("/>") {
                Some(close) if !text[root..root + close].contains('>') => root + close + 2,
                _ => limit,
            },
        };
        let chunk = text[start..end].trim();
        if !chunk.is_empty() {
            out.push(chunk);
        }
    }
    out
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
    node.attribute(name)?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

fn whole(node: &Node, name: &str) -> Option<u32> {
    number(node, name)
        .filter(|value| *value >= 0.0)
        .map(|value| value as u32)
}

fn text(node: &Node, name: &str) -> Option<String> {
    node.attribute(name)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
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
    /// The operator ran in batch mode, where `ActualElapsedms` covers the
    /// operator alone. Row-mode operators report their inputs' time too.
    exclusive_time: bool,
    /// The operator's own expressions call a scalar user-defined function.
    calls_udf: bool,
}

/// Parse one statement element into its plans: one, except for a
/// `<StmtCursor>` whose `<CursorPlan>` holds several `<Operation>`s with
/// their own `<QueryPlan>` (populate and fetch queries).
fn parse_statement(statement: Node) -> Vec<ExecutionPlan> {
    if is(&statement, "StmtCursor") {
        let operations: Vec<Node> = own_descendants(statement, is_statement)
            .into_iter()
            .filter(|node| is(node, "Operation") && !statement_relops(*node).is_empty())
            .collect();
        if !operations.is_empty() {
            return operations
                .into_iter()
                .map(|operation| {
                    let mut plan = parse_scope(statement, operation);
                    if let Some(kind) = text(&operation, "OperationType") {
                        plan.notes.insert(0, format!("Cursor operation: {kind}"));
                    }
                    plan
                })
                .collect();
        }
    }
    vec![parse_scope(statement, statement)]
}

/// Parse the operators under `scope` (the statement itself, or one cursor
/// operation of it) into a plan. Operators, warnings, and missing indexes
/// are scoped to this statement so multi-statement batches stay grouped per
/// statement.
fn parse_scope(statement: Node, scope: Node) -> ExecutionPlan {
    let statement_text = statement
        .attribute("StatementText")
        .unwrap_or_default()
        .to_string();
    let statement_type = statement
        .attribute("StatementType")
        .unwrap_or_default()
        .to_string();
    let total_subtree_cost = number(&statement, "StatementSubTreeCost").unwrap_or_default();

    let relops = statement_relops(scope);
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

    attach_statement_warnings(scope, &mut operators);
    make_times_inclusive(&mut operators, &edges);

    let scope_own = own_descendants(scope, |n| is_statement(n) || is_relop(n));
    if let Some(stats) = scope_own.iter().find(|n| is(n, "QueryTimeStats")) {
        let target = operators
            .iter()
            .position(|op| op.calls_udf)
            .unwrap_or_default();
        if let Some(op) = operators.get_mut(target) {
            op.node.udf_cpu_ms = number(stats, "UdfCpuTime");
            op.node.udf_elapsed_ms = number(stats, "UdfElapsedTime");
        }
    }

    let nodes: Vec<PlanNode> = operators.into_iter().map(|op| op.node).collect();
    let estimated_rows = number(&statement, "StatementEstRows")
        .unwrap_or_else(|| nodes.iter().map(|node| node.estimated_rows).sum());
    let warnings = nodes
        .iter()
        .flat_map(|node| node.warnings.clone())
        .collect::<Vec<_>>();

    let mut plan = ExecutionPlan {
        statement_text,
        statement_type,
        nodes,
        edges,
        total_subtree_cost,
        estimated_rows,
        warnings,
        dialect: Some(PlanDialect::SqlServer),
        source_format: Some("sqlserver-xml".to_string()),
        ..Default::default()
    };
    fill_plan_details(&mut plan, statement, &scope_own);
    common::finish_plan(&mut plan);
    plan
}

/// Statement-level facts: optimizer attributes of the statement element and
/// everything the `<QueryPlan>` records next to its operators. Nothing here
/// is required: a plan from the plan cache or Query Store has no runtime
/// elements and still fills what it has.
fn fill_plan_details(plan: &mut ExecutionPlan, statement: Node, scope_own: &[Node]) {
    plan.optimization_level = text(&statement, "StatementOptmLevel");
    plan.early_abort_reason = text(&statement, "StatementOptmEarlyAbortReason");
    plan.cardinality_estimator_version = whole(&statement, "CardinalityEstimationModelVersion");
    plan.query_hash = text(&statement, "QueryHash");
    plan.query_plan_hash = text(&statement, "QueryPlanHash");
    if flag(&statement, "RetrievedFromCache") {
        plan.notes
            .push("Plan retrieved from the plan cache".to_string());
    }
    if flag(&statement, "BatchModeOnRowStoreUsed") {
        plan.notes.push("Batch mode on rowstore used".to_string());
    }

    let find = |name: &str| scope_own.iter().find(|n| is(n, name));
    let mut granted_by_plan = None;
    if let Some(query_plan) = find("QueryPlan") {
        plan.degree_of_parallelism = whole(query_plan, "DegreeOfParallelism");
        plan.non_parallel_reason = text(query_plan, "NonParallelPlanReason");
        plan.planning_time_ms = number(query_plan, "CompileTime");
        granted_by_plan = number(query_plan, "MemoryGrant");
        for (attribute, label, unit) in [
            ("CompileCPU", "Compile CPU", "ms"),
            ("CompileMemory", "Compile memory", "kB"),
            ("CachedPlanSize", "Cached plan size", "kB"),
        ] {
            if let Some(value) = number(query_plan, attribute) {
                plan.notes.push(format!("{label}: {value} {unit}"));
            }
        }
    }
    if let Some(stats) = find("QueryTimeStats") {
        plan.cpu_time_ms = number(stats, "CpuTime");
        plan.execution_time_ms = number(stats, "ElapsedTime");
        plan.has_actuals = true;
    }
    if let Some(grant) = find("MemoryGrantInfo") {
        plan.memory_grant = Some(PlanMemoryGrant {
            requested_kb: number(grant, "RequestedMemory"),
            granted_kb: number(grant, "GrantedMemory"),
            max_used_kb: number(grant, "MaxUsedMemory"),
            desired_kb: number(grant, "DesiredMemory"),
            required_kb: number(grant, "RequiredMemory"),
            serial_required_kb: number(grant, "SerialRequiredMemory"),
            serial_desired_kb: number(grant, "SerialDesiredMemory"),
            grant_wait_seconds: number(grant, "GrantWaitTime"),
            max_query_memory_kb: number(grant, "MaxQueryMemory"),
        });
        if let Some(feedback) = text(grant, "IsMemoryGrantFeedbackAdjusted") {
            plan.notes
                .push(format!("Memory grant feedback adjusted: {feedback}"));
        }
    } else if granted_by_plan.is_some() {
        plan.memory_grant = Some(PlanMemoryGrant {
            granted_kb: granted_by_plan,
            ..Default::default()
        });
    }
    if let Some(list) = find("ParameterList") {
        plan.parameters = list
            .children()
            .filter(|c| is(c, "ColumnReference"))
            .filter_map(|column| {
                Some(PlanParameter {
                    name: text(&column, "Column")?,
                    data_type: text(&column, "ParameterDataType"),
                    compiled_value: text(&column, "ParameterCompiledValue")
                        .map(|value| unwrap_parentheses(&value)),
                    runtime_value: text(&column, "ParameterRuntimeValue")
                        .map(|value| unwrap_parentheses(&value)),
                })
            })
            .collect();
    }
    plan.statistics_used = scope_own
        .iter()
        .filter(|n| is(n, "StatisticsInfo"))
        .map(|info| PlanStatisticsUsage {
            schema: info
                .attribute("Schema")
                .map(strip_brackets)
                .filter(|schema| !schema.is_empty()),
            table: strip_brackets(info.attribute("Table").unwrap_or_default()),
            statistics: strip_brackets(info.attribute("Statistics").unwrap_or_default()),
            modification_count: number(info, "ModificationCount"),
            sampling_percent: number(info, "SamplingPercent"),
            last_update: text(info, "LastUpdate"),
        })
        .collect();
    if let Some(options) = find("StatementSetOptions") {
        plan.settings = options
            .attributes()
            .map(|attribute| PlanProperty {
                name: attribute.name().to_string(),
                value: attribute.value().to_string(),
            })
            .collect();
    }
    if let Some(threads) = find("ThreadStat") {
        let reserved: f64 = scope_own
            .iter()
            .filter(|n| is(n, "ThreadReservation"))
            .filter_map(|n| number(n, "ReservedThreads"))
            .sum();
        if let Some(used) = number(threads, "UsedThreads") {
            plan.notes.push(if reserved > 0.0 {
                format!("Threads used: {used} of {reserved} reserved")
            } else {
                format!("Threads used: {used}")
            });
        }
    }
    for wait in scope_own.iter().filter(|n| is(n, "Wait")) {
        if let Some(kind) = text(wait, "WaitType") {
            plan.notes.push(format!(
                "Wait {kind}: {} ms in {} waits",
                number(wait, "WaitTimeMs").unwrap_or_default(),
                number(wait, "WaitCount").unwrap_or_default()
            ));
        }
    }
    if let Some(cursor) = find("CursorPlan") {
        let described: Vec<String> = [
            "CursorName",
            "CursorActualType",
            "CursorRequestedType",
            "CursorConcurrency",
        ]
        .iter()
        .filter_map(|name| Some(format!("{name}: {}", text(cursor, name)?)))
        .collect();
        if !described.is_empty() {
            plan.notes.push(described.join(", "));
        }
    }
}

/// `(4242)` → `4242`: ShowPlan wraps parameter values in parentheses.
fn unwrap_parentheses(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or(trimmed)
        .to_string()
}

/// Row-mode operators report `ActualElapsedms` including their inputs;
/// batch-mode operators report their own time only. Turn the batch-mode
/// numbers into inclusive ones (own time plus the inputs' inclusive time)
/// and keep the own time as the self time.
fn make_times_inclusive(operators: &mut [Operator], edges: &[PlanEdge]) {
    if !operators.iter().any(|op| op.exclusive_time) {
        return;
    }
    let index_of: std::collections::HashMap<i32, usize> = operators
        .iter()
        .enumerate()
        .map(|(index, op)| (op.node.id, index))
        .collect();
    // One pass over the edges, so a plan of thousands of operators is not
    // rescanned once per operator.
    let mut inputs_of: std::collections::HashMap<i32, Vec<usize>> =
        std::collections::HashMap::new();
    for edge in edges {
        if let Some(input) = index_of.get(&edge.to_node) {
            inputs_of.entry(edge.from_node).or_default().push(*input);
        }
    }
    // Document order puts every operator before its inputs, so walking
    // backwards sees the inputs first.
    for index in (0..operators.len()).rev() {
        if !operators[index].exclusive_time {
            continue;
        }
        let Some(own) = operators[index].node.actual_time_ms else {
            continue;
        };
        let id = operators[index].node.id;
        let inputs: f64 = inputs_of
            .get(&id)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|input| **input > index)
            .filter_map(|input| operators[*input].node.actual_time_ms)
            .sum();
        operators[index].node.actual_self_time_ms = Some(own);
        operators[index].node.actual_time_ms = Some(own + inputs);
    }
}

fn node_id(relop: &Node) -> i32 {
    relop
        .attribute("NodeId")
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_default()
}

/// The per-thread runtime counters of one operator, combined.
#[derive(Default)]
struct Runtime {
    rows: Option<f64>,
    rebinds: Option<f64>,
    rewinds: Option<f64>,
    executions: Option<f64>,
    rows_read: Option<f64>,
    /// Threads of a parallel operator run side by side: the longest one is
    /// the operator's wall-clock time.
    elapsed_ms: Option<f64>,
    cpu_ms: Option<f64>,
    logical_reads: Option<f64>,
    physical_reads: Option<f64>,
    read_aheads: Option<f64>,
    used_memory_kb: Option<f64>,
    /// `(thread, rows)` per counter element.
    thread_rows: Vec<(f64, f64)>,
    mode: Option<String>,
    /// Counters with no typed field, summed over threads.
    other: Vec<(&'static str, f64)>,
}

impl Runtime {
    fn read(own: &[Node]) -> Self {
        const OTHER: [(&str, &str); 7] = [
            ("ActualScans", "Actual Scans"),
            ("Batches", "Actual Batches"),
            ("ActualLobLogicalReads", "Actual LOB Logical Reads"),
            ("ActualLobPhysicalReads", "Actual LOB Physical Reads"),
            ("ActualLobReadAheads", "Actual LOB Read Aheads"),
            ("InputMemoryGrant", "Input Memory Grant kB"),
            ("OutputMemoryGrant", "Output Memory Grant kB"),
        ];
        let mut out = Self::default();
        let mut other = [None::<f64>; 7];
        for counter in own.iter().filter(|n| is(n, "RunTimeCountersPerThread")) {
            let add = |slot: &mut Option<f64>, name: &str| {
                // A sum that no longer fits an f64 keeps the last total
                // that did, so no infinity reaches the plan.
                if let Some(value) = number(counter, name) {
                    let total = slot.unwrap_or(0.0) + value;
                    if total.is_finite() {
                        *slot = Some(total);
                    }
                }
            };
            add(&mut out.rows, "ActualRows");
            add(&mut out.rebinds, "ActualRebinds");
            add(&mut out.rewinds, "ActualRewinds");
            add(&mut out.executions, "ActualExecutions");
            add(&mut out.rows_read, "ActualRowsRead");
            add(&mut out.cpu_ms, "ActualCPUms");
            add(&mut out.logical_reads, "ActualLogicalReads");
            add(&mut out.physical_reads, "ActualPhysicalReads");
            add(&mut out.read_aheads, "ActualReadAheads");
            add(&mut out.used_memory_kb, "UsedMemoryGrant");
            for (slot, (attribute, _)) in other.iter_mut().zip(OTHER) {
                add(slot, attribute);
            }
            if let Some(elapsed) = number(counter, "ActualElapsedms") {
                out.elapsed_ms = Some(out.elapsed_ms.map_or(elapsed, |max| max.max(elapsed)));
            }
            if let Some(rows) = number(counter, "ActualRows") {
                out.thread_rows
                    .push((number(counter, "Thread").unwrap_or_default(), rows));
            }
            if out.mode.is_none() {
                out.mode = text(counter, "ActualExecutionMode");
            }
        }
        out.other = other
            .iter()
            .zip(OTHER)
            .filter_map(|(value, (_, label))| Some((label, (*value)?)))
            .filter(|(_, value)| *value > 0.0)
            .collect();
        out
    }

    /// Rows per worker thread, in thread order. Thread 0 coordinates a
    /// parallel zone and reads nothing, so it is left out; a serial operator
    /// has no per-thread breakdown.
    fn worker_rows(&self) -> Vec<f64> {
        if self.thread_rows.len() < 2 {
            return Vec::new();
        }
        let mut workers: Vec<(f64, f64)> = self
            .thread_rows
            .iter()
            .copied()
            .filter(|(thread, _)| *thread != 0.0)
            .collect();
        workers.sort_by(|a, b| a.0.total_cmp(&b.0));
        workers.into_iter().map(|(_, rows)| rows).collect()
    }
}

/// Elements a `<RelOp>` holds besides the one that describes the operator
/// (`<IndexScan>`, `<Hash>`, `<Sort>` ...).
const RELOP_COMMON_CHILDREN: [&str; 8] = [
    "OutputList",
    "Warnings",
    "MemoryFractions",
    "RunTimeInformation",
    "RunTimeCountersPerThread",
    "RunTimePartitionSummary",
    "InternalInfo",
    "MissingIndexes",
];

fn push_property(properties: &mut Vec<PlanProperty>, name: &str, value: impl Into<String>) {
    let value = value.into();
    if !value.trim().is_empty() {
        properties.push(PlanProperty {
            name: name.to_string(),
            value,
        });
    }
}

/// Labels of the `<ColumnReference>` children of the first own element
/// called `name`.
fn columns_of(own: &[Node], name: &str) -> Vec<String> {
    own.iter()
        .find(|n| is(n, name))
        .map(|list| {
            list.children()
                .filter(|c| is(c, "ColumnReference"))
                .filter_map(|column| column_label(&column))
                .collect()
        })
        .unwrap_or_default()
}

/// `ScalarString` of the expression directly inside the first own element
/// called `name` (`<Predicate>`, `<ProbeResidual>`, `<TopExpression>` ...).
fn scalar_of(own: &[Node], name: &str) -> Option<String> {
    own.iter()
        .find(|n| is(n, name))?
        .children()
        .find(|c| is(c, "ScalarOperator"))
        .and_then(|scalar| text(&scalar, "ScalarString"))
}

/// The seek predicate as text. ShowPlan stores it as ranges of key columns
/// against expressions (`<Prefix ScanType="EQ">`, `<StartRange>`,
/// `<EndRange>`); several `<SeekKeys>` are alternative seeks.
fn seek_predicate(own: &[Node]) -> Option<String> {
    let mut seeks: Vec<Node> = own.iter().filter(|n| is(n, "SeekKeys")).copied().collect();
    if seeks.is_empty() {
        // SQL Server 2005 puts the ranges directly in `<SeekPredicate>`.
        seeks = own
            .iter()
            .filter(|n| is(n, "SeekPredicate"))
            .copied()
            .collect();
    }
    let mut alternatives = Vec::new();
    for seek in seeks {
        let mut parts = Vec::new();
        for range in seek.children().filter(Node::is_element) {
            let name = range.tag_name().name();
            if name == "IsNotNull" {
                parts.extend(
                    range
                        .children()
                        .filter(|c| is(c, "ColumnReference"))
                        .filter_map(|column| column_label(&column))
                        .map(|column| format!("{column} IS NOT NULL")),
                );
                continue;
            }
            if !matches!(name, "Prefix" | "StartRange" | "EndRange") {
                continue;
            }
            let operator = match range.attribute("ScanType").unwrap_or_default() {
                "EQ" => "=",
                "GT" => ">",
                "GE" => ">=",
                "LT" => "<",
                "LE" => "<=",
                other => other,
            };
            let columns: Vec<String> = range
                .children()
                .filter(|c| is(c, "RangeColumns"))
                .flat_map(|list| list.children().filter(|c| is(c, "ColumnReference")))
                .filter_map(|column| column_label(&column))
                .collect();
            let expressions: Vec<&str> = range
                .children()
                .filter(|c| is(c, "RangeExpressions"))
                .flat_map(|list| list.children().filter(|c| is(c, "ScalarOperator")))
                .map(|scalar| scalar.attribute("ScalarString").unwrap_or_default())
                .collect();
            for (index, column) in columns.iter().enumerate() {
                match expressions.get(index) {
                    Some(expression) if !expression.is_empty() => {
                        parts.push(format!("{column} {operator} {expression}"));
                    }
                    _ => parts.push(column.clone()),
                }
            }
        }
        if !parts.is_empty() {
            alternatives.push(parts.join(" AND "));
        }
    }
    match alternatives.len() {
        0 => None,
        1 => alternatives.pop(),
        _ => Some(
            alternatives
                .iter()
                .map(|seek| format!("({seek})"))
                .collect::<Vec<_>>()
                .join(" OR "),
        ),
    }
}

/// `<OrderBy>` columns as `column` / `column DESC`.
fn order_by(own: &[Node]) -> Vec<String> {
    own.iter()
        .find(|n| is(n, "OrderBy"))
        .map(|order| {
            order
                .children()
                .filter(|c| is(c, "OrderByColumn"))
                .filter_map(|key| {
                    let column = key
                        .children()
                        .find(|c| is(c, "ColumnReference"))
                        .and_then(|column| column_label(&column))?;
                    Some(match key.attribute("Ascending").map(str::trim) {
                        Some("0" | "false") => format!("{column} DESC"),
                        _ => column,
                    })
                })
                .collect()
        })
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
    let runtime = Runtime::read(&own);
    let actual_rows = runtime.rows;

    // `EstimateRows` is per execution. On an actual plan the comparison is
    // against the rows of every execution, so scale by the estimated
    // execution count (1 + rebinds + rewinds) — otherwise the inner side of
    // every nested loop reads as a wild misestimate.
    let per_execution = number(&relop, "EstimateRows").unwrap_or_default();
    let estimated_executions = (relop.attribute("EstimateRebinds").is_some()
        || relop.attribute("EstimateRewinds").is_some())
    .then(|| {
        (1.0 + number(&relop, "EstimateRebinds").unwrap_or_default()
            + number(&relop, "EstimateRewinds").unwrap_or_default())
        .max(1.0)
    })
    // Two huge counts can add up past what an f64 holds.
    .filter(|executions: &f64| executions.is_finite());
    let scaled = per_execution * estimated_executions.unwrap_or(1.0);
    let estimated_rows = if actual_rows.is_some() && scaled.is_finite() {
        scaled
    } else {
        per_execution
    };

    let objects: Vec<&Node> = own.iter().filter(|n| is(n, "Object")).collect();
    let first_object = objects.first().copied();
    let object = first_object.map(|object| {
        (
            strip_brackets(object.attribute("Schema").unwrap_or_default()),
            strip_brackets(object.attribute("Table").unwrap_or_default()),
        )
    });
    let index = first_object
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
    let spill_details = own.iter().find(|n| {
        is(n, "SortSpillDetails") || is(n, "HashSpillDetails") || is(n, "ExchangeSpillDetails")
    });
    let spill_marker = own.iter().find(|n| is(n, "SpillToTempDb"));
    let spills = spill_details.is_some()
        || spill_marker.is_some()
        || own
            .iter()
            .any(|n| is(n, "SpillOccurred") && flag(n, "Detail"));
    if spills {
        warnings.push(PlanWarning::SpillToTempDb);
    }
    let columns_without_statistics: Vec<String> = own
        .iter()
        .filter(|n| is(n, "ColumnsWithNoStatistics"))
        .flat_map(|list| list.children().filter(|c| is(c, "ColumnReference")))
        .filter_map(|column| column_label(&column))
        .collect();
    if own.iter().any(|n| is(n, "ColumnsWithNoStatistics")) {
        warnings.push(PlanWarning::CardinalityEstimation);
    }
    if matches!(physical_op.as_str(), "Table Spool" | "Index Spool") {
        warnings.push(PlanWarning::Spool);
    }

    let output_list: Vec<String> = own
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

    // ---- Typed detail ----------------------------------------------------
    let mut properties = Vec::new();
    let is_join = logical_op.ends_with("Join");
    let predicate = scalar_of(&own, "Predicate");
    let build_keys = columns_of(&own, "HashKeysBuild");
    let probe_keys = columns_of(&own, "HashKeysProbe");
    let outer_keys = columns_of(&own, "OuterSideJoinColumns");
    let inner_keys = columns_of(&own, "InnerSideJoinColumns");
    let pair = |left: &[String], right: &[String]| -> Option<String> {
        (!left.is_empty() && left.len() == right.len()).then(|| {
            left.iter()
                .zip(right)
                .map(|(l, r)| format!("{l} = {r}"))
                .collect::<Vec<_>>()
                .join(" AND ")
        })
    };
    let probe_residual = scalar_of(&own, "ProbeResidual");
    let residual = scalar_of(&own, "Residual");
    let (filter, join_predicate) = if is_join {
        let keys = pair(&build_keys, &probe_keys).or_else(|| pair(&outer_keys, &inner_keys));
        let join = keys
            .or_else(|| predicate.clone())
            .or_else(|| probe_residual.clone())
            .or_else(|| residual.clone());
        (None, join)
    } else {
        (predicate.clone(), None)
    };
    let join_type = is_join.then(|| logical_op.trim_end_matches("Join").trim().to_string());
    let mut group_keys = columns_of(&own, "GroupBy");
    if group_keys.is_empty() && !is_join {
        group_keys = build_keys.clone();
    }
    if let Some(text) = &probe_residual {
        push_property(&mut properties, "Probe Residual", text.clone());
    }
    if let Some(text) = scalar_of(&own, "BuildResidual") {
        push_property(&mut properties, "Build Residual", text);
    }
    if let Some(text) = &residual {
        push_property(&mut properties, "Residual", text.clone());
    }
    if is_join
        && join_predicate != predicate
        && let Some(text) = &predicate
    {
        push_property(&mut properties, "Predicate", text.clone());
    }
    for (element, label) in [
        ("OuterReferences", "Outer References"),
        ("PartitionColumns", "Partition Columns"),
        ("ProbeColumn", "Probe Column"),
    ] {
        push_property(&mut properties, label, columns_of(&own, element).join(", "));
    }
    for (element, label) in [
        ("TopExpression", "Top Expression"),
        ("SetPredicate", "Set Predicate"),
        ("StartupExpression", "Startup Expression"),
    ] {
        if let Some(text) = scalar_of(&own, element) {
            push_property(&mut properties, label, text);
        }
    }
    let defined: Vec<String> = own
        .iter()
        .filter(|n| is(n, "DefinedValue"))
        .filter_map(|value| {
            let scalar = value
                .children()
                .find(|c| is(c, "ScalarOperator"))
                .and_then(|scalar| text(&scalar, "ScalarString"))?;
            let column = value
                .children()
                .find(|c| is(c, "ColumnReference"))
                .and_then(|column| column_label(&column));
            Some(match column {
                Some(column) => format!("{column} = {scalar}"),
                None => scalar,
            })
        })
        .collect();
    push_property(&mut properties, "Defined Values", defined.join("; "));
    let calls_udf = own.iter().any(|n| is(n, "UserDefinedFunction"));
    for udf in own.iter().filter(|n| is(n, "UserDefinedFunction")) {
        if let Some(name) = text(udf, "FunctionName") {
            push_property(&mut properties, "User Defined Function", name);
        }
    }

    // The element describing the operator (`<IndexScan Ordered="1" ...>`).
    if let Some(detail) = relop
        .children()
        .find(|c| c.is_element() && !RELOP_COMMON_CHILDREN.contains(&c.tag_name().name()))
    {
        for attribute in detail.attributes() {
            push_property(&mut properties, attribute.name(), attribute.value());
        }
    }
    for name in [
        "TableCardinality",
        "EstimateRowsWithoutRowGoal",
        "EstimateRebinds",
        "EstimateRewinds",
    ] {
        if let Some(value) = relop.attribute(name) {
            push_property(&mut properties, name, value);
        }
    }
    if actual_rows.is_some() && estimated_executions.is_some_and(|n| n != 1.0) {
        // `estimated_rows` is scaled to all executions on an actual plan;
        // keep the number the optimizer wrote.
        push_property(&mut properties, "EstimateRows", per_execution.to_string());
    }
    if let Some(object) = first_object {
        for name in ["IndexKind", "Storage"] {
            if let Some(value) = object.attribute(name) {
                push_property(&mut properties, name, value);
            }
        }
    }
    for extra in objects.iter().skip(1) {
        let name: Vec<String> = ["Schema", "Table", "Index"]
            .iter()
            .filter_map(|part| extra.attribute(*part))
            .map(strip_brackets)
            .filter(|part| !part.is_empty())
            .collect();
        push_property(&mut properties, "Object", name.join("."));
    }
    let relation = (!table.is_empty()).then(|| table.clone());
    if table.starts_with('@') {
        push_property(&mut properties, "TableVariable", "true");
    }
    if let Some(fractions) = own.iter().find(|n| is(n, "MemoryFractions")) {
        for (attribute, label) in [
            ("Input", "Memory Fraction Input"),
            ("Output", "Memory Fraction Output"),
        ] {
            if let Some(value) = fractions.attribute(attribute) {
                push_property(&mut properties, label, value);
            }
        }
    }
    for (label, value) in &runtime.other {
        push_property(&mut properties, label, value.to_string());
    }

    let spill = spills.then(|| {
        let kind = match spill_details.map(|n| n.tag_name().name()) {
            Some("SortSpillDetails") => "sort",
            Some("HashSpillDetails") => "hash",
            Some("ExchangeSpillDetails") => "exchange",
            _ if physical_op.contains("Sort") => "sort",
            _ if physical_op.contains("Hash") => "hash",
            _ if physical_op == "Parallelism" => "exchange",
            _ => "",
        };
        if let Some(details) = spill_details {
            for (attribute, label) in [
                ("GrantedMemoryKb", "Spill Granted Memory kB"),
                ("UsedMemoryKb", "Spill Used Memory kB"),
                ("WritesToTempDb", "Spill Writes To TempDb"),
                ("ReadsFromTempDb", "Spill Reads From TempDb"),
            ] {
                if let Some(value) = details.attribute(attribute) {
                    push_property(&mut properties, label, value);
                }
            }
        }
        PlanSpill {
            kind: kind.to_string(),
            level: spill_marker.and_then(|n| whole(n, "SpillLevel")),
            threads: spill_marker.and_then(|n| whole(n, "SpilledThreadCount")),
            // tempdb pages are 8 kB.
            written_kb: spill_details
                .and_then(|n| number(n, "WritesToTempDb"))
                .map(|pages| pages * 8.0)
                .filter(|kb| kb.is_finite()),
        }
    });

    let (partitions_scanned, partitions) = partitions_accessed(&relop);

    let buffers = (runtime.logical_reads.is_some()
        || runtime.physical_reads.is_some()
        || runtime.read_aheads.is_some())
    .then(|| PlanBuffers {
        logical_reads: runtime.logical_reads,
        physical_reads: runtime.physical_reads,
        read_aheads: runtime.read_aheads,
        ..Default::default()
    });
    let execution_mode = runtime
        .mode
        .clone()
        .or_else(|| text(&relop, "EstimatedExecutionMode"));
    let exclusive_time = runtime.mode.as_deref() == Some("Batch") && runtime.elapsed_ms.is_some();
    let alias = first_object
        .and_then(|object| object.attribute("Alias"))
        .map(strip_brackets)
        .filter(|alias| !alias.is_empty() && !alias.eq_ignore_ascii_case(&table));

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
            actual_rewinds: runtime.rewinds.map(|n| n as i32),
            actual_rebinds: runtime.rebinds.map(|n| n as i32),
            row_count: actual_rows.map(|rows| rows as i32),
            output_columns: output_list.clone(),
            output_list,
            warnings,
            relation,
            schema: (!schema.is_empty()).then_some(schema),
            database: first_object
                .and_then(|object| object.attribute("Database"))
                .map(strip_brackets)
                .filter(|database| !database.is_empty()),
            alias,
            index_name: (!index.is_empty()).then_some(index),
            index_cond: seek_predicate(&own),
            filter,
            join_predicate,
            join_type,
            sort_keys: order_by(&own),
            group_keys,
            estimated_row_width: number(&relop, "AvgRowSize"),
            estimated_rows_read: number(&relop, "EstimatedRowsRead"),
            table_cardinality: number(&relop, "TableCardinality").filter(|rows| *rows >= 0.0),
            partitions_scanned,
            partitions,
            estimated_executions,
            loops: runtime.executions,
            actual_rows_total: actual_rows,
            rows_read: runtime.rows_read,
            actual_time_ms: runtime.elapsed_ms,
            actual_cpu_ms: runtime.cpu_ms,
            never_executed: runtime.executions == Some(0.0),
            peak_memory_kb: runtime.used_memory_kb.filter(|kb| *kb > 0.0),
            spill,
            buffers,
            parallel: flag(&relop, "Parallel"),
            thread_rows: runtime.worker_rows(),
            execution_mode,
            columns_without_statistics,
            properties,
            ..Default::default()
        },
        object,
        scalar_text,
        exclusive_time,
        calls_udf,
    }
}

/// Most partition ranges kept from one operator; a plan that touches more
/// separate ranges than this still reports the full count.
const MAX_PARTITION_RANGES: usize = 64;

/// The partitions an operator touched at run time, from an actual plan's
/// `<RunTimePartitionSummary><PartitionsAccessed PartitionCount="3">
/// <PartitionRange Start="1" End="3"/>`: the count, and each range as `2`
/// or `1..3`. The showplan schema carries no total partition count for the
/// table, so none is reported.
fn partitions_accessed(relop: &Node) -> (Option<u32>, Vec<String>) {
    let Some(accessed) = relop
        .children()
        .find(|c| is(c, "RunTimePartitionSummary"))
        .and_then(|summary| summary.children().find(|c| is(c, "PartitionsAccessed")))
    else {
        return (None, Vec::new());
    };
    let bound =
        |range: &Node, name: &str| -> Option<u64> { range.attribute(name)?.trim().parse().ok() };
    let ranges = accessed
        .children()
        .filter(|c| is(c, "PartitionRange"))
        .filter_map(|range| Some((bound(&range, "Start")?, bound(&range, "End")?)))
        .take(MAX_PARTITION_RANGES)
        .map(|(start, end)| {
            if start == end {
                start.to_string()
            } else {
                format!("{start}..{end}")
            }
        })
        .collect();
    let count = accessed
        .attribute("PartitionCount")
        .and_then(|count| count.trim().parse::<u64>().ok())
        .map(|count| u32::try_from(count).unwrap_or(u32::MAX));
    (count, ranges)
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
/// `statement` is the statement element or one cursor operation of it.
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

    /// An actual plan over a partitioned table says which partitions each
    /// read touched; the shape is the one SQL Server 2022 writes. The
    /// showplan schema has no total, so none is made up.
    #[test]
    fn partitions_accessed_and_table_cardinality_are_read_from_the_relop() {
        let plan = PlanParser::new()
            .parse_xml(r#"<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan"><BatchSequence><Batch><Statements><StmtSimple StatementText="SELECT COUNT(*) FROM dbo.sales WHERE sold_at &gt;= '2023-03-01'" StatementType="SELECT" StatementSubTreeCost="0.5"><QueryPlan><RelOp NodeId="0" PhysicalOp="Stream Aggregate" LogicalOp="Aggregate" EstimateRows="1" EstimatedTotalSubtreeCost="0.5"><RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="1" ActualExecutions="1"></RunTimeCountersPerThread></RunTimeInformation><StreamAggregate><RelOp NodeId="1" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="45097" EstimatedRowsRead="45097" TableCardinality="120000" EstimatedTotalSubtreeCost="0.4"><RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="45097" ActualExecutions="1"></RunTimeCountersPerThread></RunTimeInformation><RunTimePartitionSummary><PartitionsAccessed PartitionCount="4"><PartitionRange Start="1" End="3"></PartitionRange><PartitionRange Start="7" End="7"></PartitionRange></PartitionsAccessed></RunTimePartitionSummary><IndexScan Ordered="0"><Object Database="[Shop]" Schema="[dbo]" Table="[sales]" Index="[PK_sales]" IndexKind="Clustered" Storage="RowStore"></Object></IndexScan></RelOp></StreamAggregate></RelOp></QueryPlan></StmtSimple></Statements></Batch></BatchSequence></ShowPlanXML>"#)
            .unwrap();
        let scan = &plan.nodes[1];
        assert_eq!(scan.partitions_scanned, Some(4));
        assert_eq!(scan.partitions_total, None);
        assert_eq!(scan.partitions, ["1..3", "7"]);
        assert_eq!(scan.table_cardinality, Some(120_000.0));
        // The property the findings read stays.
        assert_eq!(scan.property("TableCardinality"), Some("120000"));
        let aggregate = &plan.nodes[0];
        assert_eq!(aggregate.partitions_scanned, None);
        assert!(aggregate.partitions.is_empty());
        assert_eq!(aggregate.table_cardinality, None);
    }

    /// Counters big enough to overflow when summed or scaled must not put
    /// an infinity into the plan.
    #[test]
    fn overflowing_counters_never_reach_the_plan_as_infinity() {
        let huge = "1e308";
        let xml = format!(
            r#"<ShowPlanXML><StmtSimple StatementText="SELECT 1" StatementType="SELECT" StatementSubTreeCost="1"><RelOp NodeId="1" LogicalOp="Sort" PhysicalOp="Sort" EstimateRows="{huge}" EstimateRebinds="{huge}" EstimateRewinds="{huge}" EstimatedTotalSubtreeCost="1"><RunTimeInformation><RunTimeCountersPerThread Thread="1" ActualRows="{huge}" ActualExecutions="1" ActualLogicalReads="{huge}" /><RunTimeCountersPerThread Thread="2" ActualRows="{huge}" ActualExecutions="1" ActualLogicalReads="{huge}" /></RunTimeInformation><Warnings><SpillToTempDb SpillLevel="1" /><SortSpillDetails WritesToTempDb="{huge}" /></Warnings></RelOp></StmtSimple></ShowPlanXML>"#
        );
        let plan = PlanParser::new().parse_xml(&xml).unwrap();
        let sort = &plan.nodes[0];
        assert_eq!(sort.actual_rows, Some(1e308));
        assert_eq!(sort.estimated_rows, 1e308);
        assert_eq!(sort.estimated_executions, None);
        assert_eq!(
            sort.buffers.as_ref().and_then(|b| b.logical_reads),
            Some(1e308)
        );
        assert_eq!(sort.spill.as_ref().and_then(|s| s.written_kb), None);
    }

    /// Batch-mode operators report their own time; the inclusive time is
    /// own plus inputs. A long chain checks the totals and that the pass
    /// over the edges stays linear.
    #[test]
    fn batch_mode_times_become_inclusive_down_a_long_chain() {
        let depth = 4000;
        let mut xml = String::from(
            r#"<ShowPlanXML><StmtSimple StatementText="SELECT 1" StatementType="SELECT" StatementSubTreeCost="1">"#,
        );
        for id in 0..depth {
            xml.push_str(&format!(
                r#"<RelOp NodeId="{id}" LogicalOp="Filter" PhysicalOp="Filter" EstimateRows="1" EstimatedTotalSubtreeCost="1"><RunTimeInformation><RunTimeCountersPerThread Thread="0" ActualRows="1" ActualExecutions="1" ActualElapsedms="2" ActualExecutionMode="Batch" /></RunTimeInformation><Filter>"#
            ));
        }
        for _ in 0..depth {
            xml.push_str("</Filter></RelOp>");
        }
        xml.push_str("</StmtSimple></ShowPlanXML>");
        let started = std::time::Instant::now();
        let plan = PlanParser::new().parse_xml(&xml).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert_eq!(plan.nodes.len(), depth);
        let root = &plan.nodes[0];
        assert_eq!(root.actual_self_time_ms, Some(2.0));
        assert_eq!(root.actual_time_ms, Some(2.0 * depth as f64));
        let leaf = &plan.nodes[depth - 1];
        assert_eq!(leaf.actual_time_ms, Some(2.0));
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
