use crate::plans::dialects::PlanDialect;
use crate::plans::warning::PlanWarning;
use serde::{Deserialize, Serialize};

const PLAN_NODE_WIDTH: f64 = 220.0;
const PLAN_NODE_HEIGHT: f64 = 92.0;
const PLAN_NODE_X_SPACING: f64 = 280.0;
const PLAN_NODE_Y: f64 = 48.0;
const PLAN_MARGIN: f64 = 48.0;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub statement_text: String,
    pub statement_type: String,
    pub nodes: Vec<PlanNode>,
    pub edges: Vec<PlanEdge>,
    pub total_subtree_cost: f64,
    pub estimated_rows: f64,
    pub warnings: Vec<PlanWarning>,

    // ---- Everything below is optional detail a parser fills when the source
    // ---- document carries it. Construct with `..Default::default()`.
    /// Engine family the plan came from, when the parser knows it.
    #[serde(default)]
    pub dialect: Option<PlanDialect>,
    /// Stable id of the document shape the plan was read from, e.g.
    /// `postgresql-text`, `sqlserver-showplan-text`, `mysql-json-v2`. See
    /// `PlanFormat::id`.
    #[serde(default)]
    pub source_format: Option<String>,
    /// True when the document carried runtime statistics (an executed plan).
    #[serde(default)]
    pub has_actuals: bool,
    /// Planner / compile time in milliseconds (PostgreSQL `Planning Time`,
    /// SQL Server `CompileTime`).
    #[serde(default)]
    pub planning_time_ms: Option<f64>,
    /// Wall-clock execution time in milliseconds (PostgreSQL `Execution
    /// Time`, SQL Server `QueryTimeStats ElapsedTime` or the `SET STATISTICS
    /// TIME` elapsed time, DuckDB total latency).
    #[serde(default)]
    pub execution_time_ms: Option<f64>,
    /// CPU time in milliseconds for the execution (SQL Server `QueryTimeStats
    /// CpuTime` or `SET STATISTICS TIME` CPU time).
    #[serde(default)]
    pub cpu_time_ms: Option<f64>,
    /// PostgreSQL JIT `Timing: Total`, in milliseconds.
    #[serde(default)]
    pub jit_time_ms: Option<f64>,
    /// PostgreSQL JIT `Functions`.
    #[serde(default)]
    pub jit_functions: Option<u32>,
    /// SQL Server `DegreeOfParallelism`.
    #[serde(default)]
    pub degree_of_parallelism: Option<u32>,
    /// SQL Server `NonParallelPlanReason`.
    #[serde(default)]
    pub non_parallel_reason: Option<String>,
    /// SQL Server `StatementOptmLevel` (`TRIVIAL`, `FULL`).
    #[serde(default)]
    pub optimization_level: Option<String>,
    /// SQL Server `StatementOptmEarlyAbortReason` (`TimeOut`,
    /// `GoodEnoughPlanFound`, `MemoryLimitExceeded`).
    #[serde(default)]
    pub early_abort_reason: Option<String>,
    /// SQL Server `CardinalityEstimationModelVersion` (70, 120, 150, 160 ...).
    #[serde(default)]
    pub cardinality_estimator_version: Option<u32>,
    /// SQL Server `QueryHash` / `QueryPlanHash`.
    #[serde(default)]
    pub query_hash: Option<String>,
    #[serde(default)]
    pub query_plan_hash: Option<String>,
    /// SQL Server `MemoryGrantInfo`.
    #[serde(default)]
    pub memory_grant: Option<PlanMemoryGrant>,
    /// SQL Server `ParameterList`: compiled (sniffed) and runtime values.
    #[serde(default)]
    pub parameters: Vec<PlanParameter>,
    /// SQL Server `OptimizerStatsUsage`: the statistics the optimizer loaded.
    #[serde(default)]
    pub statistics_used: Vec<PlanStatisticsUsage>,
    /// Per-table I/O from `SET STATISTICS IO` text pasted with the plan.
    #[serde(default)]
    pub table_io: Vec<PlanTableIo>,
    /// PostgreSQL `Trigger <name>: time=... calls=...` lines.
    #[serde(default)]
    pub triggers: Vec<PlanTrigger>,
    /// Non-default planner settings the document lists (PostgreSQL
    /// `Settings:`), in document order.
    #[serde(default)]
    pub settings: Vec<PlanProperty>,
    /// Free-text notes the engine attached to the plan (Oracle `Note`
    /// section, CockroachDB `distribution:` header lines, and similar).
    #[serde(default)]
    pub notes: Vec<String>,
}

impl ExecutionPlan {
    pub fn text_tree(&self) -> String {
        self.nodes
            .iter()
            .map(|node| {
                format!(
                    "{}: {} / {} rows={} cost={}",
                    node.id,
                    node.logical_op,
                    node.physical_op,
                    node.estimated_rows,
                    node.estimated_cost
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn search(&self, query: &str) -> Vec<PlanSearchMatch> {
        let needle = query.to_ascii_lowercase();
        self.nodes
            .iter()
            .filter(|node| {
                node.logical_op.to_ascii_lowercase().contains(&needle)
                    || node.physical_op.to_ascii_lowercase().contains(&needle)
                    || node
                        .output_list
                        .iter()
                        .any(|output| output.to_ascii_lowercase().contains(&needle))
            })
            .map(|node| PlanSearchMatch {
                node_id: node.id,
                text: format!("{} {}", node.logical_op, node.physical_op),
            })
            .collect()
    }

    pub fn visual_layout(&self) -> ExecutionPlanVisualLayout {
        let nodes = self
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let x = PLAN_MARGIN + (index as f64 * PLAN_NODE_X_SPACING);
                PlanVisualNode {
                    id: node.id,
                    label: if node.physical_op.is_empty() {
                        node.logical_op.clone()
                    } else {
                        node.physical_op.clone()
                    },
                    subtitle: if node.logical_op == node.physical_op {
                        String::new()
                    } else {
                        node.logical_op.clone()
                    },
                    estimated_cost: node.estimated_cost,
                    cost_percent: if self.total_subtree_cost > 0.0 {
                        (node.estimated_cost / self.total_subtree_cost * 100.0).max(0.0)
                    } else {
                        0.0
                    },
                    estimated_rows: node.estimated_rows,
                    actual_rows: node.actual_rows,
                    warning_count: node.warnings.len(),
                    output_list: node.output_list.clone(),
                    warnings: node
                        .warnings
                        .iter()
                        .map(|warning| format!("{warning:?}"))
                        .collect(),
                    x,
                    y: PLAN_NODE_Y,
                    width: PLAN_NODE_WIDTH,
                    height: PLAN_NODE_HEIGHT,
                }
            })
            .collect::<Vec<_>>();

        let edges = self
            .edges
            .iter()
            .filter_map(|edge| {
                let from = nodes.iter().find(|node| node.id == edge.from_node)?;
                let to = nodes.iter().find(|node| node.id == edge.to_node)?;
                Some(PlanVisualEdge {
                    from_node: edge.from_node,
                    to_node: edge.to_node,
                    from_x: from.x + from.width,
                    from_y: from.y + from.height / 2.0,
                    to_x: to.x,
                    to_y: to.y + to.height / 2.0,
                })
            })
            .collect::<Vec<_>>();

        let width = nodes
            .last()
            .map(|node| node.x + node.width + PLAN_MARGIN)
            .unwrap_or(PLAN_MARGIN * 2.0);
        ExecutionPlanVisualLayout {
            statement_text: self.statement_text.clone(),
            statement_type: self.statement_type.clone(),
            total_subtree_cost: self.total_subtree_cost,
            estimated_rows: self.estimated_rows,
            warnings: self
                .warnings
                .iter()
                .map(|warning| format!("{warning:?}"))
                .collect(),
            nodes,
            edges,
            width,
            height: PLAN_NODE_Y + PLAN_NODE_HEIGHT + PLAN_MARGIN,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanNode {
    pub id: i32,
    pub logical_op: String,
    pub physical_op: String,
    /// Estimated rows per execution of the node, as every engine reports it.
    pub estimated_rows: f64,
    pub estimated_cost: f64,
    pub estimated_cpu: f64,
    pub estimated_io: f64,
    /// Actual rows exactly as the engine reports them: per loop on PostgreSQL
    /// and MySQL, total across executions on SQL Server and Oracle. Use
    /// [`PlanNode::actual_rows_total`] / [`PlanNode::actual_rows_per_loop`]
    /// for an engine-neutral number.
    pub actual_rows: Option<f64>,
    pub actual_rewinds: Option<i32>,
    pub actual_rebinds: Option<i32>,
    pub row_count: Option<i32>,
    pub output_list: Vec<String>,
    pub warnings: Vec<PlanWarning>,

    // ---- Everything below is optional detail a parser fills when the source
    // ---- document carries it. Construct with `..Default::default()`.
    /// Table, view, CTE or function the node reads or writes (no schema).
    #[serde(default)]
    pub relation: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub database: Option<String>,
    /// Alias the statement gave the relation, when it differs from its name.
    #[serde(default)]
    pub alias: Option<String>,
    /// True when `relation` is known to be the table's real name rather than
    /// a query alias: the plan format prints both (MySQL JSON format 2,
    /// SQLite before 3.36), or the name was resolved against the statement
    /// text or the schema. MySQL, MariaDB and SQLite otherwise print only
    /// the alias, and advice that names the table says so.
    #[serde(default)]
    pub relation_resolved: bool,
    #[serde(default)]
    pub index_name: Option<String>,
    /// Predicate the index or access path satisfies (PostgreSQL `Index
    /// Cond`, SQL Server `SeekPredicates`, Oracle `access(...)`, MySQL index
    /// lookup condition).
    #[serde(default)]
    pub index_cond: Option<String>,
    /// Residual predicate applied to rows after they are read (PostgreSQL
    /// `Filter`, SQL Server `Predicate`, Oracle `filter(...)`, MySQL
    /// `attached_condition`).
    #[serde(default)]
    pub filter: Option<String>,
    /// Join condition (PostgreSQL `Hash Cond` / `Merge Cond` / `Join Filter`,
    /// SQL Server hash keys or join predicate, MySQL join condition).
    #[serde(default)]
    pub join_predicate: Option<String>,
    /// `Inner`, `Left`, `Right`, `Full`, `Semi`, `Anti` and engine variants.
    #[serde(default)]
    pub join_type: Option<String>,
    #[serde(default)]
    pub sort_keys: Vec<String>,
    #[serde(default)]
    pub group_keys: Vec<String>,
    /// Columns or expressions the node emits (PostgreSQL VERBOSE `Output`,
    /// SQL Server `OutputList`, Oracle projection is not captured).
    #[serde(default)]
    pub output_columns: Vec<String>,

    /// Cost before the first row can be returned (PostgreSQL startup cost).
    #[serde(default)]
    pub estimated_startup_cost: Option<f64>,
    /// Estimated average row width in bytes.
    #[serde(default)]
    pub estimated_row_width: Option<f64>,
    /// Rows the optimizer expects the node to read before residual
    /// predicates (SQL Server `EstimatedRowsRead`, MySQL
    /// `rows_examined_per_scan`).
    #[serde(default)]
    pub estimated_rows_read: Option<f64>,
    /// Rows the optimizer believed the whole table or index held when it
    /// compiled the plan (SQL Server `TableCardinality` on a read
    /// operator). Not a count of rows read or returned.
    #[serde(default)]
    pub table_cardinality: Option<f64>,
    /// Estimated number of executions of the node (SQL Server
    /// `EstimateRebinds + EstimateRewinds + 1`, MySQL `rows_produced_per_join`
    /// is NOT this).
    #[serde(default)]
    pub estimated_executions: Option<f64>,
    /// MySQL / MariaDB `filtered`: percent of read rows expected to pass the
    /// table condition (0-100).
    #[serde(default)]
    pub filtered_percent: Option<f64>,

    /// Times the node ran (PostgreSQL `loops`, SQL Server
    /// `ActualExecutions`, MySQL `loops`, MariaDB `r_loops`, Oracle
    /// `Starts`).
    #[serde(default)]
    pub loops: Option<f64>,
    /// Actual rows produced across ALL loops, on every engine.
    #[serde(default)]
    pub actual_rows_total: Option<f64>,
    /// Rows the node actually read before residual predicates (SQL Server
    /// `ActualRowsRead`, MariaDB `r_rows` before `r_filtered`; PostgreSQL
    /// rows + rows removed by filter).
    #[serde(default)]
    pub rows_read: Option<f64>,
    /// Inclusive wall-clock time for the node and its inputs, summed over
    /// all loops, in milliseconds. For PostgreSQL nodes under a Gather the
    /// per-worker average is used, the way the engine reports it.
    #[serde(default)]
    pub actual_time_ms: Option<f64>,
    /// Time to the first row, per loop, in milliseconds.
    #[serde(default)]
    pub actual_startup_ms: Option<f64>,
    /// Time spent in this node alone: `actual_time_ms` minus the inclusive
    /// time of its inputs (never negative). DuckDB reports this directly.
    #[serde(default)]
    pub actual_self_time_ms: Option<f64>,
    /// CPU time for the node in milliseconds (SQL Server `ActualCPUms`,
    /// summed over threads).
    #[serde(default)]
    pub actual_cpu_ms: Option<f64>,
    /// True when an executed plan shows the node never ran (PostgreSQL and
    /// MySQL `never executed`, Oracle `Starts` = 0).
    #[serde(default)]
    pub never_executed: bool,
    /// Total across loops.
    #[serde(default)]
    pub rows_removed_by_filter: Option<f64>,
    /// Total across loops.
    #[serde(default)]
    pub rows_removed_by_join_filter: Option<f64>,
    /// Total across loops.
    #[serde(default)]
    pub rows_removed_by_index_recheck: Option<f64>,
    /// PostgreSQL index-only scan `Heap Fetches`, total across loops.
    #[serde(default)]
    pub heap_fetches: Option<f64>,
    /// PostgreSQL bitmap heap scan `Heap Blocks: exact=`.
    #[serde(default)]
    pub exact_heap_blocks: Option<f64>,
    /// PostgreSQL bitmap heap scan `Heap Blocks: lossy=`.
    #[serde(default)]
    pub lossy_heap_blocks: Option<f64>,

    /// `quicksort`, `top-N heapsort`, `external merge`, ...
    #[serde(default)]
    pub sort_method: Option<String>,
    #[serde(default)]
    pub sort_space_kb: Option<f64>,
    /// `Memory` or `Disk`.
    #[serde(default)]
    pub sort_space_type: Option<String>,
    #[serde(default)]
    pub hash_buckets: Option<f64>,
    #[serde(default)]
    pub hash_batches: Option<u32>,
    /// Batches the hash was planned with, when it had to grow at run time.
    #[serde(default)]
    pub original_hash_batches: Option<u32>,
    /// Peak memory the node used, in kB (PostgreSQL `Memory Usage`, Oracle
    /// `Used-Mem`, SQL Server per-operator memory is not exposed).
    #[serde(default)]
    pub peak_memory_kb: Option<f64>,
    /// Sort or hash work written to disk / tempdb by this node.
    #[serde(default)]
    pub spill: Option<PlanSpill>,
    /// Page / block counters for this node and its inputs.
    #[serde(default)]
    pub buffers: Option<PlanBuffers>,

    /// True when the node runs in parallel (PostgreSQL `Parallel Aware`,
    /// SQL Server `Parallel="true"`, Oracle `PX` operations).
    #[serde(default)]
    pub parallel: bool,
    #[serde(default)]
    pub workers_planned: Option<u32>,
    #[serde(default)]
    pub workers_launched: Option<u32>,
    /// SQL Server actual rows per thread (thread 0, the coordinator, is
    /// omitted when workers did the work), for skew detection.
    #[serde(default)]
    pub thread_rows: Vec<f64>,
    /// SQL Server `EstimatedExecutionMode` / actual mode: `Row` or `Batch`.
    #[serde(default)]
    pub execution_mode: Option<String>,

    /// PostgreSQL `Parent Relationship`: `Outer`, `Inner`, `InitPlan`,
    /// `SubPlan`, `Member`.
    #[serde(default)]
    pub parent_relationship: Option<String>,
    /// PostgreSQL `Subplan Name` (`InitPlan 1`, `SubPlan 2`, `CTE recent`).
    #[serde(default)]
    pub subplan_name: Option<String>,
    /// CTE the node scans or defines.
    #[serde(default)]
    pub cte_name: Option<String>,

    #[serde(default)]
    pub partitions_scanned: Option<u32>,
    #[serde(default)]
    pub partitions_total: Option<u32>,
    /// Partition names (MySQL `partitions`) or the Oracle `Pstart`..`Pstop`
    /// pair as two entries.
    #[serde(default)]
    pub partitions: Vec<String>,

    /// SQL Server columns the optimizer had no statistics for.
    #[serde(default)]
    pub columns_without_statistics: Vec<String>,
    /// SQL Server scalar UDF time inside this operator (`UdfCpuTime` /
    /// `UdfElapsedTime`), in milliseconds.
    #[serde(default)]
    pub udf_cpu_ms: Option<f64>,
    #[serde(default)]
    pub udf_elapsed_ms: Option<f64>,

    /// Short engine flags that have no typed field: MySQL `Extra` entries
    /// (`Using filesort`, `Using temporary`, ...), SQLite detail text.
    #[serde(default)]
    pub extra: Vec<String>,
    /// Remaining engine-specific `name: value` lines, in document order, so
    /// nothing the document said about the node is lost.
    #[serde(default)]
    pub properties: Vec<PlanProperty>,
}

impl PlanNode {
    /// Rows produced across all loops: the typed total when the parser set
    /// it, else `actual_rows` as reported.
    pub fn actual_rows_total(&self) -> Option<f64> {
        self.actual_rows_total.or(self.actual_rows)
    }

    /// Average rows produced per execution, comparable with
    /// `estimated_rows` on every engine.
    pub fn actual_rows_per_loop(&self) -> Option<f64> {
        match (self.actual_rows_total, self.loops) {
            (Some(total), Some(loops)) if loops > 0.0 => Some(total / loops),
            _ => self.actual_rows,
        }
    }

    /// Value of the first `properties` entry called `name` (ASCII
    /// case-insensitive).
    pub fn property(&self, name: &str) -> Option<&str> {
        self.properties
            .iter()
            .find(|property| property.name.eq_ignore_ascii_case(name))
            .map(|property| property.value.as_str())
    }
}

/// One engine-specific `name: value` pair.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanProperty {
    pub name: String,
    pub value: String,
}

/// Page / block counters. PostgreSQL fills the `shared_*`, `local_*` and
/// `temp_*` fields (8 kB blocks, inclusive of the node's inputs, total across
/// loops); SQL Server and Oracle fill `logical_reads` / `physical_reads`.
/// `logical_reads` and `physical_reads` are filled for PostgreSQL too
/// (`shared hit + shared read` and `shared read`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanBuffers {
    #[serde(default)]
    pub shared_hit: Option<f64>,
    #[serde(default)]
    pub shared_read: Option<f64>,
    #[serde(default)]
    pub shared_dirtied: Option<f64>,
    #[serde(default)]
    pub shared_written: Option<f64>,
    #[serde(default)]
    pub local_hit: Option<f64>,
    #[serde(default)]
    pub local_read: Option<f64>,
    #[serde(default)]
    pub local_dirtied: Option<f64>,
    #[serde(default)]
    pub local_written: Option<f64>,
    #[serde(default)]
    pub temp_read: Option<f64>,
    #[serde(default)]
    pub temp_written: Option<f64>,
    /// Pages read from cache or disk.
    #[serde(default)]
    pub logical_reads: Option<f64>,
    /// Pages read from disk.
    #[serde(default)]
    pub physical_reads: Option<f64>,
    /// SQL Server read-ahead pages.
    #[serde(default)]
    pub read_aheads: Option<f64>,
    /// Pages written (Oracle `Writes`).
    #[serde(default)]
    pub writes: Option<f64>,
    /// PostgreSQL `I/O Timings` read time in milliseconds (shared + local +
    /// temp), needs `track_io_timing`.
    #[serde(default)]
    pub io_read_ms: Option<f64>,
    /// PostgreSQL `I/O Timings` write time in milliseconds.
    #[serde(default)]
    pub io_write_ms: Option<f64>,
}

/// Work written to disk because it did not fit in memory.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanSpill {
    /// `sort`, `hash`, `exchange`, or empty when the engine does not say.
    #[serde(default)]
    pub kind: String,
    /// SQL Server `SpillLevel`.
    #[serde(default)]
    pub level: Option<u32>,
    /// SQL Server `SpilledThreadCount`.
    #[serde(default)]
    pub threads: Option<u32>,
    /// Size written, in kB (PostgreSQL disk sort size, SQL Server
    /// `WritesToTempDb` pages x 8, Oracle `Used-Tmp`).
    #[serde(default)]
    pub written_kb: Option<f64>,
}

/// SQL Server `MemoryGrantInfo`, all sizes in kB.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanMemoryGrant {
    #[serde(default)]
    pub requested_kb: Option<f64>,
    #[serde(default)]
    pub granted_kb: Option<f64>,
    #[serde(default)]
    pub max_used_kb: Option<f64>,
    #[serde(default)]
    pub desired_kb: Option<f64>,
    #[serde(default)]
    pub required_kb: Option<f64>,
    #[serde(default)]
    pub serial_required_kb: Option<f64>,
    #[serde(default)]
    pub serial_desired_kb: Option<f64>,
    /// Seconds the query waited for its grant.
    #[serde(default)]
    pub grant_wait_seconds: Option<f64>,
    /// `MaxQueryMemory` available to the query.
    #[serde(default)]
    pub max_query_memory_kb: Option<f64>,
}

/// One entry of SQL Server's `ParameterList`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanParameter {
    pub name: String,
    #[serde(default)]
    pub data_type: Option<String>,
    /// Value the plan was compiled (sniffed) for.
    #[serde(default)]
    pub compiled_value: Option<String>,
    /// Value this execution ran with (actual plans only).
    #[serde(default)]
    pub runtime_value: Option<String>,
}

/// One statistics object the SQL Server optimizer loaded
/// (`OptimizerStatsUsage/StatisticsInfo`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanStatisticsUsage {
    #[serde(default)]
    pub schema: Option<String>,
    pub table: String,
    pub statistics: String,
    #[serde(default)]
    pub modification_count: Option<f64>,
    #[serde(default)]
    pub sampling_percent: Option<f64>,
    /// As written in the plan, e.g. `2026-11-01T09:12:44.03`.
    #[serde(default)]
    pub last_update: Option<String>,
}

/// One table line of `SET STATISTICS IO` output.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanTableIo {
    pub table: String,
    #[serde(default)]
    pub scan_count: Option<f64>,
    #[serde(default)]
    pub logical_reads: Option<f64>,
    #[serde(default)]
    pub physical_reads: Option<f64>,
    #[serde(default)]
    pub read_ahead_reads: Option<f64>,
    #[serde(default)]
    pub lob_logical_reads: Option<f64>,
    #[serde(default)]
    pub lob_physical_reads: Option<f64>,
}

/// One PostgreSQL trigger line of an executed plan.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PlanTrigger {
    pub name: String,
    #[serde(default)]
    pub relation: Option<String>,
    #[serde(default)]
    pub time_ms: Option<f64>,
    #[serde(default)]
    pub calls: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEdge {
    pub from_node: i32,
    pub to_node: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSearchMatch {
    pub node_id: i32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionPlanVisualLayout {
    pub statement_text: String,
    pub statement_type: String,
    pub total_subtree_cost: f64,
    pub estimated_rows: f64,
    pub warnings: Vec<String>,
    pub nodes: Vec<PlanVisualNode>,
    pub edges: Vec<PlanVisualEdge>,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanVisualNode {
    pub id: i32,
    pub label: String,
    pub subtitle: String,
    pub estimated_cost: f64,
    pub cost_percent: f64,
    pub estimated_rows: f64,
    pub actual_rows: Option<f64>,
    pub warning_count: usize,
    pub output_list: Vec<String>,
    pub warnings: Vec<String>,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanVisualEdge {
    pub from_node: i32,
    pub to_node: i32,
    pub from_x: f64,
    pub from_y: f64,
    pub to_x: f64,
    pub to_y: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientStatistics {
    pub server_round_trips: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub client_processing_ms: u128,
    pub total_execution_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadlockGraph {
    pub victim_process: Option<String>,
    pub processes: Vec<String>,
    pub resources: Vec<String>,
}

impl DeadlockGraph {
    pub fn parse(xml: &str) -> Self {
        Self {
            victim_process: attr(xml, "victim", "id"),
            processes: collect_attr(xml, "process", "id"),
            resources: collect_resource_names(xml),
        }
    }
}

fn attr(xml: &str, element: &str, name: &str) -> Option<String> {
    let start = xml.find(&format!("<{element}"))?;
    let rest = &xml[start..];
    let end = rest.find('>')?;
    let tag = &rest[..end];
    let prefix = format!("{name}=\"");
    let value_start = tag.find(&prefix)? + prefix.len();
    let value = &tag[value_start..];
    Some(value[..value.find('"')?].to_string())
}

fn collect_attr(xml: &str, element: &str, name: &str) -> Vec<String> {
    xml.match_indices(&format!("<{element}"))
        .filter_map(|(start, _)| {
            let rest = &xml[start..];
            let tag = &rest[..rest.find('>')?];
            let prefix = format!("{name}=\"");
            let value_start = tag.find(&prefix)? + prefix.len();
            let value = &tag[value_start..];
            Some(value[..value.find('"')?].to_string())
        })
        .collect()
}

fn collect_resource_names(xml: &str) -> Vec<String> {
    [
        "keylock",
        "pagelock",
        "objectlock",
        "ridlock",
        "exchangeEvent",
    ]
    .into_iter()
    .flat_map(|name| collect_attr(xml, name, "objectname"))
    .collect()
}
