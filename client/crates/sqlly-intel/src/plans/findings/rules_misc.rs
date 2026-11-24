//! Rules that fit no other group: CTEs computed in full and then filtered,
//! partitions that were not skipped, scalar functions, spools, parallelism
//! that did not happen or did not help, JIT compilation and planning time.

use super::facts::{Ctx, Op, count, ms, pct};
use super::rules_estimates::impact_at;
use super::strings::{Args, Template};
use super::{
    Draft, FindingConfidence as C, FindingImpact, FindingKind as K, FindingSeverity as S,
    ImpactBasis,
};
use crate::plans::PlanDialect;
use crate::plans::warning::PlanWarning;

pub(super) const TEMPLATES: &[Template] = &[
    // ---- cte_materialization
    (
        "cte_materialization.filtered.summary",
        "The CTE {cte} is computed in full ({rows_read} rows) and then filtered down to {rows_returned}.",
        "summary: PostgreSQL materialized CTE whose rows are mostly discarded by the outer query",
    ),
    (
        "cte_materialization.filtered.why",
        "A materialized CTE is an optimization fence: PostgreSQL runs it to completion and stores the result before the outer query's conditions apply, so the condition cannot reach the tables inside the CTE or use their indexes.",
        "why a materialized CTE that is filtered afterwards costs",
    ),
    (
        "cte_materialization.filtered.action",
        "Move the condition into the CTE, or on PostgreSQL 12 and later write WITH {cte} AS NOT MATERIALIZED (...) so the planner can push the condition down. A recursive CTE, or one that calls a volatile function, is always materialized.",
        "action for a materialized CTE that is filtered afterwards",
    ),
    // ---- partition_pruning
    (
        "partition_pruning.empty_branches.summary",
        "{operator} read {partitions} partitions or branches; {empty} of them were scanned and returned no rows for the filter.",
        "summary: PostgreSQL Append scanned many partitions that contributed nothing",
    ),
    (
        "partition_pruning.empty_branches.why",
        "When the planner can prove a partition holds no matching rows it skips the partition without reading it. Here {rows_read} rows were read from partitions that returned nothing, so the condition did not line up with the partition bounds.",
        "why scanning partitions that return nothing costs",
    ),
    (
        "partition_pruning.empty_branches.action",
        "Compare the partition key itself with a constant or parameter of the same type. A function or cast around the key, a comparison with another column, or a condition on a different column than the partition key prevents pruning. If these are UNION ALL branches over separate tables, add CHECK constraints that describe each table's range and keep constraint_exclusion at partition or on.",
        "action for partitions that were scanned for nothing",
    ),
    (
        "partition_pruning.all.summary",
        "{operator} read all {partitions} partitions of {table} although the query filters the rows.",
        "summary: every partition of a partitioned table was scanned under a filter",
    ),
    (
        "partition_pruning.all.why",
        "Partitioning saves work only when the engine can skip partitions. With every partition read, the filter is applied row by row to the whole table.",
        "why reading every partition costs",
    ),
    (
        "partition_pruning.all.action",
        "Add a condition on the partition key, compared with a constant or bind value of the same type, with no function or conversion around the key. If the filter is on a different column, an index on that column may serve the query better than partitioning does.",
        "action for a scan of every partition",
    ),
    (
        "partition_pruning.granules.summary",
        "ClickHouse reads every granule of {table} ({granules}, about {rows} rows): the filter cannot use the primary key.",
        "summary: ClickHouse ReadFromMergeTree selected all granules under a WHERE clause",
    ),
    (
        "partition_pruning.granules.why",
        "A MergeTree table skips data only through its sorting key, its partition key and its data skipping indexes. A filter on other columns is checked against every granule.",
        "why a ClickHouse read of all granules costs",
    ),
    (
        "partition_pruning.granules.action",
        "If this filter is common, make its column a leading column of the table's ORDER BY key, or add a data skipping index or a projection sorted by it. The statement below shows the current sorting and partition keys.",
        "action for a ClickHouse read of all granules",
    ),
    // ---- scalar_udf
    (
        "scalar_udf.time.summary",
        "Scalar user-defined functions took {udf_time} of this statement's {total_time} ({percent}).",
        "summary: SQL Server time spent inside scalar UDFs",
    ),
    (
        "scalar_udf.time.why",
        "A scalar function that is not inlined runs once per row as a separate call, its own queries are invisible in this plan, and it forces the whole statement to run on one thread.",
        "why scalar UDFs cost",
    ),
    (
        "scalar_udf.time.action",
        "Replace the function call with its logic written inline, or turn it into an inline table-valued function used with CROSS APPLY. On SQL Server 2019 and later a scalar function can be inlined automatically (database compatibility level 150 or higher); the statement below shows which functions qualify.",
        "action for time spent in scalar UDFs",
    ),
    // ---- spool
    (
        "spool.heavy.summary",
        "{operator} copies {rows} rows into a tempdb worktable.",
        "summary: SQL Server table or index spool with significant volume",
    ),
    (
        "spool.heavy.why",
        "A spool saves the rows of the operators below it in tempdb so they can be read again. Writing the worktable is extra work that the query pays for when the optimizer has no cheaper way to reuse or isolate those rows.",
        "why a heavy spool costs",
    ),
    (
        "spool.heavy.action",
        "A lazy spool on the inner side of a nested loop usually stands in for a missing index on the join column of the table below it. An eager spool in an UPDATE, DELETE or INSERT separates reading from writing the same table (Halloween protection); it goes away when the statement does not read the columns or the index it modifies.",
        "action for a heavy spool",
    ),
    // ---- parallel_workers
    (
        "parallel_workers.fewer.summary",
        "{operator} planned {planned} parallel workers and {launched} were started.",
        "summary: PostgreSQL Gather got fewer workers than planned",
    ),
    (
        "parallel_workers.fewer.why",
        "The plan was costed for {planned} workers sharing the work. With fewer, the same plan runs slower than planned, and a different plan might have been cheaper.",
        "why launching fewer workers than planned costs",
    ),
    (
        "parallel_workers.fewer.action",
        "Workers come from a server-wide pool. The pool was exhausted when this statement ran, either by other parallel queries or because max_parallel_workers or max_worker_processes is set low. Check the settings below against the number of parallel queries that run at once.",
        "action for fewer parallel workers than planned",
    ),
    // ---- parallel_skew
    (
        "parallel_skew.one_thread.summary",
        "{operator} ran on {threads} threads, and one of them processed {percent} of the {rows} rows.",
        "summary: SQL Server parallel operator with rows concentrated on one thread",
    ),
    (
        "parallel_skew.one_thread.why",
        "Parallelism helps only when the rows are spread over the threads. With most rows on one thread, the statement pays the overhead of a parallel plan and runs at about the speed of a serial one.",
        "why parallel skew costs",
    ),
    (
        "parallel_skew.one_thread.action",
        "Rows are split across threads by hashing the join or grouping column, so one very common value (often NULL) sends most rows to one thread. Check how the values of that column are distributed. To see whether parallelism pays off at all for this statement, compare it with a serial run.",
        "action for parallel skew",
    ),
    // ---- serial_plan_forced
    (
        "serial_plan_forced.udf.summary",
        "This statement could not use a parallel plan because it calls a scalar user-defined function.",
        "summary: SQL Server NonParallelPlanReason for a T-SQL scalar UDF",
    ),
    (
        "serial_plan_forced.udf.why",
        "A scalar function that is not inlined forces the whole statement to run on one thread, even the parts that never touch the function.",
        "why a UDF forcing a serial plan costs",
    ),
    (
        "serial_plan_forced.udf.action",
        "Write the function's logic inline in the query, or turn it into an inline table-valued function used with CROSS APPLY. On SQL Server 2019 and later, check whether the function can be inlined automatically.",
        "action for a serial plan forced by a UDF",
    ),
    (
        "serial_plan_forced.table_variable.summary",
        "This statement could not use a parallel plan because it modifies a table variable.",
        "summary: SQL Server NonParallelPlanReason for table variable modification",
    ),
    (
        "serial_plan_forced.table_variable.why",
        "A statement that inserts into, updates or deletes from a table variable always runs on one thread, including the query that produces the rows.",
        "why a table variable forcing a serial plan costs",
    ),
    (
        "serial_plan_forced.table_variable.action",
        "Use a temporary table (#name) in place of the table variable. A statement that fills a temporary table can run in parallel, and the temporary table also gets statistics.",
        "action for a serial plan forced by a table variable",
    ),
    (
        "serial_plan_forced.maxdop.summary",
        "This statement ran on one thread because the maximum degree of parallelism is set to 1.",
        "summary: SQL Server NonParallelPlanReason MaxDOPSetToOne",
    ),
    (
        "serial_plan_forced.maxdop.why",
        "With MAXDOP 1 the optimizer never considers a parallel plan. That is the right setting for some workloads, but for a statement with an estimated cost of {cost} it may leave most of the server's processors idle.",
        "why MAXDOP 1 can cost",
    ),
    (
        "serial_plan_forced.maxdop.action",
        "Find where the limit comes from: a MAXDOP 1 hint on the statement, the database-scoped setting, the server setting, or a Resource Governor workload group. The statements below show the server and database settings.",
        "action for a serial plan forced by MAXDOP 1",
    ),
    (
        "serial_plan_forced.other.summary",
        "This statement could not use a parallel plan. The reason SQL Server gives is {reason}.",
        "summary: SQL Server NonParallelPlanReason, other reasons; {reason} is the engine's reason code",
    ),
    (
        "serial_plan_forced.other.why",
        "The statement has an estimated cost of {cost}, high enough that a parallel plan would normally be considered. Something in the statement or its settings ruled that out, so it runs on one thread.",
        "why a forced serial plan can cost",
    ),
    (
        "serial_plan_forced.other.action",
        "Look up the reason code in the SQL Server documentation for NonParallelPlanReason and remove the construct it names if the statement needs to run faster.",
        "action for a serial plan forced for another reason",
    ),
    // ---- jit_overhead
    (
        "jit_overhead.slow.summary",
        "JIT compilation took {jit_time} of this statement's {total_time} execution time ({percent}).",
        "summary: PostgreSQL JIT compilation takes a large share of the run time",
    ),
    (
        "jit_overhead.slow.why",
        "PostgreSQL compiles parts of an expensive-looking query to machine code before running it. The decision is made from the estimated cost, so a query that turns out to be short pays for compilation it does not earn back.",
        "why JIT overhead costs",
    ),
    (
        "jit_overhead.slow.action",
        "Turn JIT off for this session and compare the run time. If it is faster without, raise jit_above_cost so that only costlier queries are compiled, or turn JIT off for the role or database that runs this workload.",
        "action for JIT overhead",
    ),
    (
        "jit_overhead.alt_threshold",
        "Keep JIT for expensive queries only by raising the cost at which it starts (the default is 100000)",
        "alternative: raise jit_above_cost",
    ),
    // ---- planning_time
    (
        "planning_time.dominant.summary",
        "Planning took {planning_time}, longer than the {execution_time} the statement took to run.",
        "summary: planning time exceeds execution time",
    ),
    (
        "planning_time.dominant.why",
        "Planning is paid every time the statement is planned. For a statement that runs often, this is most of its cost. The first statement in a new session also plans slower while the catalog caches fill, so repeat the measurement before acting on it.",
        "why long planning time costs",
    ),
    (
        "planning_time.dominant.action",
        "Run the statement as a prepared statement so the plan is reused. Planning time grows with the number of joined tables, partitions and indexes: prune partitions with a condition on the partition key, and drop indexes that are never used.",
        "action for long planning time",
    ),
];

pub(super) fn run(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    for position in 0..ctx.len() {
        if !ctx.executed(position) {
            continue;
        }
        cte_materialization(ctx, position, drafts);
        partition_pruning(ctx, position, drafts);
        spool(ctx, position, drafts);
        parallel_workers(ctx, position, drafts);
    }
    scalar_udf(ctx, drafts);
    parallel_skew(ctx, drafts);
    serial_plan_forced(ctx, drafts);
    jit_overhead(ctx, drafts);
    planning_time(ctx, drafts);
}

fn cte_materialization(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::Postgres || ctx.op(position) != Op::CteScan {
        return;
    }
    let node = ctx.node(position);
    let Some(cte) = node.cte_name.as_deref().filter(|name| !name.is_empty()) else {
        return;
    };
    let scan = ctx.scan(position);
    let (Some(read), Some(kept), Some(filter)) = (scan.read, scan.kept, scan.filter_text.clone())
    else {
        return;
    };
    let removed = read - kept;
    if removed < 10_000.0 || kept > read * 0.1 {
        return;
    }
    // The node that computes the CTE.
    let label = format!("CTE {cte}");
    let Some(producer) =
        (0..ctx.len()).find(|at| ctx.node(*at).subplan_name.as_deref() == Some(label.as_str()))
    else {
        return;
    };
    // A recursive CTE cannot be inlined.
    if ctx.op_name(producer).starts_with("RECURSIVE UNION") {
        return;
    }
    let share = ctx.share(producer);
    if share.is_some_and(|share| share < 0.1) {
        return;
    }
    let references = (0..ctx.len())
        .filter(|at| ctx.op(*at) == Op::CteScan && ctx.node(*at).cte_name.as_deref() == Some(cte))
        .count();
    let severity = if share.is_some_and(|share| share >= 0.25) {
        S::Warning
    } else {
        S::Info
    };
    let confidence = if references == 1 { C::High } else { C::Medium };
    let args: Args = vec![
        ("cte", cte.to_string()),
        ("rows_read", count(read)),
        ("rows_returned", count(kept)),
    ];
    let mut draft = Draft::new(
        K::CteMaterialization,
        severity,
        confidence,
        "filtered",
        args,
    )
    .at(position)
    .also(producer)
    .ev("filter", filter)
    .ev("rows_read", count(read))
    .ev("rows_returned", count(kept))
    .ev_opt("time", ctx.m(producer).inclusive_time_ms.map(ms));
    if let Some(impact) = impact_at(ctx, producer) {
        draft = draft.impact(impact);
    }
    drafts.push(draft);
}

/// `selected/initial` from a ClickHouse index property such as `74/74` or
/// `1,024/2,048`.
fn fraction(text: &str) -> Option<(f64, f64)> {
    let (selected, initial) = text.split_once('/')?;
    let number = |part: &str| part.trim().replace(',', "").parse::<f64>().ok();
    Some((number(selected)?, number(initial)?))
}

fn partition_pruning(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let node = ctx.node(position);
    match ctx.dialect {
        PlanDialect::Postgres => {
            if ctx.op(position) != Op::Append || !ctx.has_actuals() {
                return;
            }
            let kids = ctx.kids(position);
            if kids.len() < 4 {
                return;
            }
            // Branches that read rows and returned none of them.
            let mut empty = 0usize;
            let mut wasted = 0.0;
            let mut filter: Option<String> = None;
            let mut empty_nodes: Vec<usize> = Vec::new();
            for kid in kids {
                if !ctx.executed(*kid) || !ctx.op(*kid).is_read() {
                    continue;
                }
                let scan = ctx.scan(*kid);
                let (Some(read), Some(kept)) = (scan.read, scan.kept) else {
                    continue;
                };
                if kept == 0.0 && read >= 1.0 && scan.filter_raw.is_some() {
                    empty += 1;
                    wasted += read;
                    empty_nodes.push(*kid);
                    filter = filter.or(scan.filter_text);
                }
            }
            if empty * 2 < kids.len() || wasted < 10_000.0 {
                return;
            }
            let share = ctx.share(position);
            let severity = if share.is_some_and(|share| share >= 0.1) {
                S::Warning
            } else {
                S::Info
            };
            let args: Args = vec![
                ("operator", ctx.title(position)),
                ("partitions", kids.len().to_string()),
                ("empty", empty.to_string()),
                ("rows_read", count(wasted)),
            ];
            let mut draft = Draft::new(
                K::PartitionPruning,
                severity,
                C::Medium,
                "empty_branches",
                args,
            )
            .at(position)
            .ev(
                "partitions",
                format!("{} scanned, {empty} returned no rows", kids.len()),
            )
            .ev_opt("filter", filter)
            .ev("rows_read", count(wasted))
            .ev_opt("time", ctx.m(position).inclusive_time_ms.map(ms));
            for at in empty_nodes.into_iter().take(8) {
                draft = draft.also(at);
            }
            drafts.push(draft);
        }
        PlanDialect::ClickHouse => {
            if ctx.op(position) != Op::FullScan {
                return;
            }
            let Some(rel) = ctx.rel(position) else {
                return;
            };
            let Some((selected, initial)) = node
                .property("PrimaryKey index granules")
                .and_then(fraction)
            else {
                return;
            };
            // Other index kinds may still have skipped data.
            let pruned_elsewhere = node.properties.iter().any(|property| {
                property.name.ends_with(" granules")
                    && fraction(&property.value).is_some_and(|(kept, all)| kept < all)
            });
            // The read is filtered: by a WHERE step above it or a PREWHERE
            // on the read itself.
            let filtered = ctx.a(position).filter.is_some()
                || ctx
                    .ancestors(position)
                    .into_iter()
                    .any(|at| ctx.op(at) == Op::Filter && ctx.a(at).filter.is_some());
            if selected < initial || pruned_elsewhere || !filtered {
                return;
            }
            let rows = ctx
                .m(position)
                .rows_out
                .or(ctx.m(position).estimated_rows)
                .unwrap_or(selected * 8_192.0);
            if rows < 100_000.0 {
                return;
            }
            let severity = if rows >= 100_000_000.0 {
                S::Warning
            } else {
                S::Info
            };
            let granules = format!("{}/{}", count(selected), count(initial));
            let args: Args = vec![
                ("table", rel.display()),
                ("granules", granules.clone()),
                ("rows", count(rows)),
            ];
            let table_literal = |text: &str| text.replace('\'', "''");
            let sql = match rel.schema.as_deref().filter(|schema| !schema.is_empty()) {
                Some(schema) => format!(
                    "SELECT sorting_key, partition_key, primary_key\nFROM system.tables\nWHERE database = '{}' AND name = '{}';",
                    table_literal(schema),
                    table_literal(&rel.name)
                ),
                None => format!(
                    "SELECT sorting_key, partition_key, primary_key\nFROM system.tables\nWHERE database = currentDatabase() AND name = '{}';",
                    table_literal(&rel.name)
                ),
            };
            let filter_text = ctx
                .a(position)
                .filter
                .as_ref()
                .map(|f| f.text.clone())
                .or_else(|| {
                    ctx.ancestors(position)
                        .into_iter()
                        .find(|at| ctx.op(*at) == Op::Filter)
                        .and_then(|at| ctx.a(at).filter.as_ref().map(|f| f.text.clone()))
                });
            drafts.push(
                Draft::new(K::PartitionPruning, severity, C::Medium, "granules", args)
                    .sql(sql)
                    .at(position)
                    .ev("table", rel.display())
                    .ev("parts", format!("granules {granules}"))
                    .ev_opt("filter", filter_text)
                    .ev("estimated_rows", count(rows)),
            );
        }
        _ => {
            // Engines that report how many partitions were read: Oracle's
            // PARTITION ... ALL, or any plan with both counts.
            let name = ctx.op_name(position);
            let all_by_name = ctx.dialect == PlanDialect::Oracle
                && ctx.op(position) == Op::Partition
                && name.ends_with(" ALL");
            let scanned = node.partitions_scanned;
            let all_by_count = matches!(
                (scanned, node.partitions_total),
                (Some(scanned), Some(total)) if total >= 2 && scanned >= total
            );
            if !(all_by_name || all_by_count) {
                return;
            }
            let partitions = scanned.or(node.partitions_total).unwrap_or(0);
            if partitions < 2 {
                return;
            }
            // The table read under this operator, with a filter on it.
            let mut below = ctx.descendants(position);
            below.insert(0, position);
            let Some((read_at, rel)) = below
                .iter()
                .copied()
                .find_map(|at| ctx.rel(at).map(|rel| (at, rel)))
            else {
                return;
            };
            let scan = ctx.scan(read_at);
            let Some(filter) = scan.filter_text.clone() else {
                return;
            };
            let rows = scan.read.or(scan.est_read).unwrap_or(0.0);
            if rows < 10_000.0 {
                return;
            }
            let share = ctx.share(position);
            let severity = if ctx.has_actuals() && share.is_some_and(|share| share >= 0.25) {
                S::Warning
            } else {
                S::Info
            };
            let args: Args = vec![
                ("operator", ctx.title(position)),
                ("partitions", partitions.to_string()),
                ("table", rel.display()),
            ];
            let mut draft = Draft::new(K::PartitionPruning, severity, C::Medium, "all", args)
                .at(position)
                .also(read_at)
                .ev("table", rel.display())
                .ev("partitions", format!("{partitions} of {partitions}"))
                .ev("filter", filter);
            draft = if scan.read.is_some() {
                draft.ev("rows_read", count(rows))
            } else {
                draft
                    .ev("estimated_rows", count(rows))
                    .note("common.note.estimated_only", &[])
            };
            drafts.push(draft);
        }
    }
}

fn spool(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::SqlServer || ctx.op(position) != Op::Spool {
        return;
    }
    let metrics = ctx.m(position);
    let own = ctx.self_share(position);
    let rows = if ctx.has_actuals() {
        metrics.rows_out
    } else {
        metrics.estimated_rows_total.or(metrics.estimated_rows)
    };
    let Some(rows) = rows else {
        return;
    };
    let heavy = match own {
        Some(own) => own >= 0.2 || (rows >= 100_000.0 && own >= 0.05),
        None => rows >= 100_000.0,
    };
    if !heavy {
        return;
    }
    let severity = if ctx.has_actuals() && own.is_some_and(|own| own >= 0.25) {
        S::Warning
    } else {
        S::Info
    };
    let confidence = if ctx.has_actuals() { C::Medium } else { C::Low };
    let args: Args = vec![("operator", ctx.title(position)), ("rows", count(rows))];
    let warned = ctx
        .node(position)
        .warnings
        .iter()
        .any(|warning| matches!(warning, PlanWarning::Spool));
    let mut draft = Draft::new(K::Spool, severity, confidence, "heavy", args)
        .at(position)
        .own_share();
    draft = if ctx.has_actuals() {
        draft.ev("actual_rows", count(rows))
    } else {
        draft
            .ev("estimated_rows", count(rows))
            .note("common.note.estimated_only", &[])
    };
    draft = draft
        .ev_opt("executions", ctx.executions(position).map(count))
        .ev_opt("own_time", metrics.self_time_ms.map(ms))
        .ev_opt("cost_share", (!ctx.timed).then(|| own.map(pct)).flatten());
    if warned {
        draft = draft.ev("warning", "Spool");
    }
    drafts.push(draft);
}

fn parallel_workers(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::Postgres || ctx.op(position) != Op::Gather {
        return;
    }
    let node = ctx.node(position);
    let (Some(planned), Some(launched)) = (node.workers_planned, node.workers_launched) else {
        return;
    };
    if launched >= planned {
        return;
    }
    let metrics = ctx.m(position);
    let time = metrics.inclusive_time_ms;
    let rows = metrics.rows_out.unwrap_or(0.0);
    if !(time.is_some_and(|time| time >= 100.0) || rows >= 100_000.0) {
        return;
    }
    let share = ctx.share(position);
    let severity = if launched == 0 && share.is_some_and(|share| share >= 0.25) {
        S::Warning
    } else {
        S::Info
    };
    let args: Args = vec![
        ("operator", ctx.title(position)),
        ("planned", planned.to_string()),
        ("launched", launched.to_string()),
    ];
    drafts.push(
        Draft::new(K::ParallelWorkers, severity, C::High, "fewer", args)
            .sql(
                "SHOW max_worker_processes;\nSHOW max_parallel_workers;\nSHOW max_parallel_workers_per_gather;",
            )
            .at(position)
            .ev("workers", format!("{planned} planned, {launched} launched"))
            .ev_opt("time", time.map(ms)),
    );
}

fn scalar_udf(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::SqlServer {
        return;
    }
    // The engine reports the time once per statement; take the largest value
    // so a figure repeated on several operators is not counted twice.
    let found = (0..ctx.len())
        .filter_map(|at| ctx.node(at).udf_elapsed_ms.map(|time| (at, time)))
        .max_by(|a, b| a.1.total_cmp(&b.1));
    let Some((at, udf_time)) = found else {
        return;
    };
    let Some(total) = ctx
        .an
        .execution_time_ms
        .or(ctx.an.total_time_ms)
        .filter(|t| *t > 0.0)
    else {
        return;
    };
    if udf_time < 100.0 || udf_time < total * 0.1 {
        return;
    }
    let share = (udf_time / total).clamp(0.0, 1.0);
    let severity = if share >= 0.5 {
        S::Critical
    } else {
        S::Warning
    };
    let args: Args = vec![
        ("udf_time", ms(udf_time)),
        ("total_time", ms(total)),
        ("percent", pct(share)),
    ];
    drafts.push(
        Draft::new(K::ScalarUdf, severity, C::High, "time", args)
            .sql(UDF_INLINE_SQL)
            .at(at)
            .ev("udf_time", ms(udf_time))
            .ev_opt(
                "udf_time",
                ctx.node(at)
                    .udf_cpu_ms
                    .map(|cpu| format!("{} CPU", ms(cpu))),
            )
            .ev("execution_time", ms(total))
            .impact(FindingImpact {
                share,
                basis: ImpactBasis::Time,
                time_ms: Some(udf_time),
                optimizer_estimate: false,
            }),
    );
}

const UDF_INLINE_SQL: &str = "SELECT s.name AS schema_name, o.name AS function_name, m.is_inlineable\nFROM sys.sql_modules AS m\nJOIN sys.objects AS o ON o.object_id = m.object_id\nJOIN sys.schemas AS s ON s.schema_id = o.schema_id\nWHERE o.type = 'FN';";

fn parallel_skew(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::SqlServer {
        return;
    }
    // (position, threads, total rows, largest share), worst operator first.
    let mut worst: Option<(usize, usize, f64, f64)> = None;
    for position in 0..ctx.len() {
        let rows: Vec<f64> = ctx
            .node(position)
            .thread_rows
            .iter()
            .copied()
            .filter(|rows| rows.is_finite() && *rows >= 0.0)
            .collect();
        if rows.len() < 2 || !ctx.executed(position) {
            continue;
        }
        let total: f64 = rows.iter().sum();
        let largest = rows.iter().copied().fold(0.0, f64::max);
        if total < 100_000.0 {
            continue;
        }
        let top = largest / total;
        let skewed = if rows.len() >= 4 {
            top >= 0.8
        } else {
            top >= 0.95
        };
        // An operator that only reads what a skewed operator below it
        // produced repeats the same fact; keep the one with the most rows.
        if skewed && worst.is_none_or(|(_, _, rows, _)| total > rows) {
            worst = Some((position, rows.len(), total, top));
        }
    }
    let Some((position, threads, total, top)) = worst else {
        return;
    };
    let share = ctx.share(position);
    let severity = if share.is_some_and(|share| share >= 0.25) {
        S::Warning
    } else {
        S::Info
    };
    let args: Args = vec![
        ("operator", ctx.title(position)),
        ("threads", threads.to_string()),
        ("percent", pct(top)),
        ("rows", count(total)),
    ];
    let per_thread = ctx
        .node(position)
        .thread_rows
        .iter()
        .map(|rows| count(*rows))
        .collect::<Vec<_>>()
        .join(", ");
    drafts.push(
        Draft::new(K::ParallelSkew, severity, C::Medium, "one_thread", args)
            .sql("OPTION (MAXDOP 1)")
            .at(position)
            .ev("threads", per_thread)
            .ev_opt("time", ctx.m(position).inclusive_time_ms.map(ms)),
    );
}

fn serial_plan_forced(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::SqlServer {
        return;
    }
    let Some(reason) = ctx
        .plan
        .non_parallel_reason
        .as_deref()
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
    else {
        return;
    };
    // Not a problem with the statement: the plan was cheap enough to stay
    // serial, or the edition has no parallel plans.
    const BENIGN: &[&str] = &[
        "EstimatedDOPIsOne",
        "NoParallelPlansInDesktopOrExpressEdition",
        "NoParallelPlansDuringUpgrade",
    ];
    if BENIGN
        .iter()
        .any(|benign| benign.eq_ignore_ascii_case(reason))
    {
        return;
    }
    let cost = ctx
        .an
        .total_cost
        .or(Some(ctx.plan.total_subtree_cost).filter(|cost| *cost > 0.0));
    let time = ctx.an.execution_time_ms.or(ctx.an.total_time_ms);
    // Below the default cost threshold for parallelism (5) nothing would run
    // in parallel anyway; ask for a clear margin over it.
    if !(cost.is_some_and(|cost| cost >= 50.0) || time.is_some_and(|time| time >= 1_000.0)) {
        return;
    }
    let slow = time.is_some_and(|time| time >= 1_000.0);
    let cost_text = cost.map(|cost| format!("{cost:.0}")).unwrap_or_default();
    let args: Args = vec![("reason", reason.to_string()), ("cost", cost_text.clone())];
    let lower = reason.to_ascii_lowercase();
    let draft = if lower.contains("userdefinedfunction") {
        let severity = if slow { S::Warning } else { S::Info };
        Draft::new(K::SerialPlanForced, severity, C::High, "udf", args).sql(UDF_INLINE_SQL)
    } else if lower.contains("tablevariable") {
        let severity = if slow { S::Warning } else { S::Info };
        Draft::new(
            K::SerialPlanForced,
            severity,
            C::High,
            "table_variable",
            args,
        )
    } else if lower == "maxdopsettoone" {
        if cost.is_none() {
            return;
        }
        Draft::new(K::SerialPlanForced, S::Info, C::Medium, "maxdop", args).sql(
            "SELECT name, value_in_use FROM sys.configurations\nWHERE name IN ('max degree of parallelism', 'cost threshold for parallelism');\nSELECT name, value FROM sys.database_scoped_configurations WHERE name = 'MAXDOP';",
        )
    } else {
        if cost.is_none() {
            return;
        }
        Draft::new(K::SerialPlanForced, S::Info, C::Medium, "other", args)
    };
    drafts.push(
        draft
            .ev("reason", reason)
            .ev_opt("execution_time", time.map(ms)),
    );
}

fn jit_overhead(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::Postgres {
        return;
    }
    let (Some(jit), Some(total)) = (ctx.plan.jit_time_ms, ctx.plan.execution_time_ms) else {
        return;
    };
    if jit < 50.0 || total <= 0.0 || jit < total * 0.25 {
        return;
    }
    let share = (jit / total).clamp(0.0, 1.0);
    let severity = if share >= 0.5 { S::Warning } else { S::Info };
    let args: Args = vec![
        ("jit_time", ms(jit)),
        ("total_time", ms(total)),
        ("percent", pct(share)),
    ];
    drafts.push(
        Draft::new(K::JitOverhead, severity, C::High, "slow", args)
            .sql("SET jit = off;")
            .alt(
                "alt_threshold",
                Some("SET jit_above_cost = 1000000;".to_string()),
            )
            .ev("jit_time", ms(jit))
            .ev_opt(
                "jit_time",
                ctx.plan
                    .jit_functions
                    .map(|functions| format!("{functions} functions compiled")),
            )
            .ev("execution_time", ms(total))
            .impact(FindingImpact {
                share,
                basis: ImpactBasis::Time,
                time_ms: Some(jit),
                optimizer_estimate: false,
            }),
    );
}

fn planning_time(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    let (Some(planning), Some(execution)) = (ctx.plan.planning_time_ms, ctx.plan.execution_time_ms)
    else {
        return;
    };
    if planning < 20.0 || planning < execution {
        return;
    }
    let args: Args = vec![
        ("planning_time", ms(planning)),
        ("execution_time", ms(execution)),
    ];
    drafts.push(
        Draft::new(K::PlanningTime, S::Info, C::Medium, "dominant", args)
            .ev("planning_time", ms(planning))
            .ev("execution_time", ms(execution)),
    );
}
