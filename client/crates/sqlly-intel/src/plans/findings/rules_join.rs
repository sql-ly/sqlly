//! Rules about joins: joins with no condition, nested loops that run their
//! inner side a very large number of times, hash joins built on the larger
//! input, joins that multiply rows only for an aggregate to collapse them,
//! subqueries that run once per outer row, and MySQL's join buffer.

use super::advice::{IndexSuggestion, stats_sql, suggestion_from_terms};
use super::facts::{Ctx, Op, Rel, count, ms, pct, ratio};
use super::rules_access::join_column;
use super::rules_estimates::impact_at;
use super::strings::{Args, Template};
use super::{Draft, FindingConfidence as C, FindingKind as K, FindingSeverity as S};
use crate::plans::PlanDialect;
use crate::plans::warning::PlanWarning;

pub(super) const TEMPLATES: &[Template] = &[
    // ---- no_join
    (
        "no_join.rows.summary",
        "{operator} has no join condition: it pairs each of {left_rows} rows with each of {right_rows} rows and produces {rows}.",
        "summary: a join without a join condition (cartesian product), with measured or estimated row counts",
    ),
    (
        "no_join.rows.why",
        "Without a condition, the output is the row count of one input multiplied by the other. Everything above this join then has to process the multiplied rows.",
        "why a cartesian product costs, in the finding that states the row counts",
    ),
    (
        "no_join.rows.action",
        "Check the query for a missing join condition between these inputs: a comma join or CROSS JOIN with no matching WHERE clause, or an ON clause that compares the wrong tables. If every combination is what the query needs, nothing has to change.",
        "action for a cartesian product, in the finding that states the row counts",
    ),
    (
        "no_join.unknown.summary",
        "{operator} has no join condition, so it pairs every row of one input with every row of the other.",
        "summary: a join without a join condition when row counts are not known",
    ),
    (
        "no_join.unknown.why",
        "Without a condition, the output is the row count of one input multiplied by the other. Everything above this join then has to process the multiplied rows.",
        "why a cartesian product costs, in the finding where the row counts are not known",
    ),
    (
        "no_join.unknown.action",
        "Check the query for a missing join condition between these inputs: a comma join or CROSS JOIN with no matching WHERE clause, or an ON clause that compares the wrong tables. If every combination is what the query needs, nothing has to change.",
        "action for a cartesian product, in the finding where the row counts are not known",
    ),
    // ---- nested_loop_volume
    (
        "nested_loop_volume.inner_scan.summary",
        "The nested loop scans {table} {loops} times, once for each row of the other input, reading {rows_read} rows in total.",
        "summary: nested loop whose inner side is a full table scan repeated many times",
    ),
    (
        "nested_loop_volume.inner_scan.why",
        "A nested loop runs its inner side once per outer row. With a full scan on the inner side the work is the two row counts multiplied; with an index on the join column each run reads only the matching rows.",
        "why a repeated inner scan costs",
    ),
    (
        "nested_loop_volume.inner_scan.action",
        "Create an index on {table} ({index_columns}) so each run of the inner side is a seek instead of a scan.",
        "action: index the join column of the inner table",
    ),
    (
        "nested_loop_volume.inner_scan_text",
        "Index the column of {table} that the join compares, so each run of the inner side is a seek instead of a scan.",
        "action for a repeated inner scan when the join column cannot be named",
    ),
    (
        "nested_loop_volume.underestimate.summary",
        "The nested loop ran its inner side {loops} times; the optimizer planned for {estimated_rows} outer rows and got {actual_rows}.",
        "summary: nested loop chosen from a low outer-row estimate",
    ),
    (
        "nested_loop_volume.underestimate.why",
        "A nested loop is the cheapest join for a few outer rows and the most expensive for many, because every outer row costs one more index descent on the inner side. With the real row count the optimizer would likely have read the inner table once with a hash or merge join.",
        "why a nested loop with many more outer rows than planned costs",
    ),
    (
        "nested_loop_volume.underestimate.action",
        "Fix the estimate on the outer side: refresh the statistics on its table, then look at the filter there for something the optimizer cannot estimate (a wrapped column, several correlated conditions, a parameter value the plan was not compiled for).",
        "action for a nested loop chosen from a low estimate",
    ),
    // ---- hash_build_side
    (
        "hash_build_side.larger.summary",
        "The hash join built its hash table from {build_rows} rows and probed it with {probe_rows}; the optimizer expected {estimated_rows} rows on the build side.",
        "summary: hash join built on the larger input because of a low estimate",
    ),
    (
        "hash_build_side.larger.why",
        "A hash join is cheapest when the smaller input is the one held in memory. Built on the larger input, the hash table needs more memory than was planned and is more likely to spill to disk.",
        "why building the hash on the larger input costs",
    ),
    (
        "hash_build_side.larger.action",
        "The optimizer picks the build side from its row estimates, and the estimate for this side was {factor} too low. Refresh the statistics on the tables behind it and check the filters there.",
        "action for a hash join built on the larger input",
    ),
    // ---- join_fanout
    (
        "join_fanout.distinct.summary",
        "This join turns {input_rows} input rows into {rows}, and {reducer} above it brings them back down to {reduced_rows}.",
        "summary: join multiplies rows that a DISTINCT or GROUP BY then collapses; {reducer} is the operator name",
    ),
    (
        "join_fanout.distinct.why",
        "Each row matches about {per_row} rows on the other side, so the query builds {rows} rows only to throw most of them away. All operators between the join and {reducer} work on the multiplied rows.",
        "why a join fan-out hidden by DISTINCT or GROUP BY costs",
    ),
    (
        "join_fanout.distinct.action",
        "Check the join condition. If the extra matches are not wanted, the condition is missing a column. If only the existence of a match matters, use EXISTS instead of a join. If the totals are wanted, aggregate the many-rows side first and join the result.",
        "action for a join fan-out hidden by DISTINCT or GROUP BY",
    ),
    // ---- dependent_subquery
    (
        "dependent_subquery.loops.summary",
        "{subquery} ran {loops} times, once for each row of the outer query.",
        "summary: correlated subquery executed once per outer row; {subquery} is the engine's name for it, such as SubPlan 1",
    ),
    (
        "dependent_subquery.loops.why",
        "A correlated subquery is evaluated again for every outer row. Each run may be quick, but the total grows with the outer row count, and the optimizer cannot swap it for a hash join.",
        "why a correlated subquery run per row costs",
    ),
    (
        "dependent_subquery.loops.action",
        "Rewrite the subquery as a join so it runs once: join to the table directly, or to a derived table that groups by the correlated column. A NOT IN or IN subquery can usually become NOT EXISTS or EXISTS, which the optimizer can turn into a join.",
        "action: rewrite a correlated subquery as a join",
    ),
    (
        "dependent_subquery.loops_index",
        "Rewrite the subquery as a join so it runs once: join to the table directly, or to a derived table that groups by the correlated column. To keep the subquery, make each run cheap with an index on {table} ({index_columns}), so a run reads only its matching rows.",
        "action: rewrite a correlated subquery as a join, or index the table it scans",
    ),
    (
        "dependent_subquery.estimated.summary",
        "{subquery} is a dependent subquery: MySQL evaluates it once for each row of the outer query, estimated at {rows} rows.",
        "summary: MySQL DEPENDENT SUBQUERY in an estimated plan",
    ),
    (
        "dependent_subquery.estimated.why",
        "A dependent subquery is evaluated again for every outer row. Each run may be quick, but the total grows with the outer row count.",
        "why a dependent subquery costs",
    ),
    (
        "dependent_subquery.estimated.action",
        "Rewrite the subquery as a join so it runs once: join to the table directly, or to a derived table that groups by the correlated column.",
        "action: rewrite a dependent subquery as a join",
    ),
    (
        "dependent_subquery.sqlite_scan.summary",
        "A correlated subquery scans {table} from start to end for every row of the outer query.",
        "summary: SQLite CORRELATED SUBQUERY containing a full table scan",
    ),
    (
        "dependent_subquery.sqlite_scan.why",
        "SQLite runs a correlated subquery once per outer row. With a full scan inside it, the work is the outer row count multiplied by the size of {table}.",
        "why a SQLite correlated subquery with a scan costs",
    ),
    (
        "dependent_subquery.sqlite_scan.action",
        "If {table} is more than a few hundred rows, create an index on the column the subquery compares with the outer row, so each run is a search instead of a scan. The plan does not show row counts, so check the table size first.",
        "action for a SQLite correlated subquery with a scan",
    ),
    // ---- join_buffer
    (
        "join_buffer.block.summary",
        "MySQL joins {table} with a join buffer (block nested loop): no index matches the join condition, so {table} is scanned for each buffer of outer rows, about {rows} row comparisons.",
        "summary: MySQL or MariaDB Using join buffer (Block Nested Loop)",
    ),
    (
        "join_buffer.block.why",
        "A block nested loop compares every buffered outer row with every row of the scanned table. The work is the two row counts multiplied, where an index on the join column would read only the matching rows.",
        "why a block nested loop join costs",
    ),
    (
        "join_buffer.block.action",
        "Create an index on {table} ({index_columns}) so the join can look up the matching rows.",
        "action: index the join column for a block nested loop join",
    ),
    (
        "join_buffer.block_text",
        "Index the column of {table} that the join compares, so the join can look up the matching rows. If the two columns have different types or collations, MySQL cannot use an index for the join; make them match.",
        "action for a block nested loop join when the join column cannot be named",
    ),
];

pub(super) fn run(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    let mut fanout_reducers: Vec<usize> = Vec::new();
    for position in 0..ctx.len() {
        if !ctx.executed(position) {
            continue;
        }
        no_join(ctx, position, drafts);
        nested_loop_volume(ctx, position, drafts);
        hash_build_side(ctx, position, drafts);
        join_fanout(ctx, position, drafts, &mut fanout_reducers);
        dependent_subquery(ctx, position, drafts);
        join_buffer(ctx, position, drafts);
    }
}

fn estimated(ctx: &Ctx<'_>, position: usize) -> Option<f64> {
    let metrics = ctx.m(position);
    metrics.estimated_rows_total.or(metrics.estimated_rows)
}

fn no_join(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let node = ctx.node(position);
    let warned = node
        .warnings
        .iter()
        .any(|warning| matches!(warning, PlanWarning::NoJoinPredicate));
    if !(warned || ctx.op(position) == Op::CrossJoin) {
        return;
    }
    let kids = ctx.kids(position);
    if kids.len() != 2 {
        return;
    }
    let metrics = ctx.m(position);
    let measured = ctx.has_actuals() && metrics.rows_out.is_some();
    // Rows of each input per execution: the inner side of a nested loop
    // reports its total over all executions.
    let side = |kid: usize| -> Option<f64> {
        if measured {
            ctx.m(kid).rows_per_loop.or(ctx.m(kid).rows_out)
        } else {
            ctx.m(kid).estimated_rows
        }
    };
    let rows = if measured {
        metrics.rows_out
    } else {
        estimated(ctx, position)
    };
    let operator = ctx.title(position);
    let (Some(left), Some(right), Some(rows)) = (side(kids[0]), side(kids[1]), rows) else {
        // No counts at all: the engine's own statement is the evidence.
        if ctx.has_actuals() {
            return;
        }
        drafts.push(
            Draft::new(
                K::NoJoin,
                S::Info,
                C::Low,
                "unknown",
                vec![("operator", operator)],
            )
            .at(position)
            .also(kids[0])
            .also(kids[1]),
        );
        return;
    };
    // One row on either side multiplies nothing, and a join whose output is
    // no larger than its inputs has a condition somewhere the plan does not
    // print on this operator.
    if left <= 1.0 || right <= 1.0 || rows < 1_000.0 || rows < left.max(right) * 2.0 {
        return;
    }
    let share = ctx.share(position);
    let severity = if !measured {
        S::Info
    } else if rows >= 1_000_000.0 && share.is_none_or(|share| share >= 0.5) {
        S::Critical
    } else if rows >= 100_000.0 || share.is_some_and(|share| share >= 0.25) {
        S::Warning
    } else {
        S::Info
    };
    let confidence = if measured { C::High } else { C::Low };
    let args: Args = vec![
        ("operator", operator),
        ("left_rows", count(left)),
        ("right_rows", count(right)),
        ("rows", count(rows)),
    ];
    let mut draft = Draft::new(K::NoJoin, severity, confidence, "rows", args)
        .at(position)
        .also(kids[0])
        .also(kids[1]);
    if measured {
        draft = draft
            .ev("actual_rows", count(rows))
            .ev_opt("time", metrics.inclusive_time_ms.map(ms));
    } else {
        draft = draft
            .ev("estimated_rows", count(rows))
            .note("common.note.estimated_only", &[]);
    }
    if warned {
        draft = draft.ev("warning", "NoJoinPredicate");
    }
    drafts.push(draft);
}

fn nested_loop_volume(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.op(position) != Op::NestedLoop || !ctx.has_actuals() {
        return;
    }
    let kids = ctx.kids(position);
    if kids.len() != 2 {
        return;
    }
    let (outer, inner) = (kids[0], kids[1]);
    let Some(loops) = ctx.m(inner).loops else {
        return;
    };
    let inner_share = ctx.share(inner);

    // A full scan repeated on the inner side.
    let mut inner_nodes = ctx.descendants(inner);
    inner_nodes.insert(0, inner);
    let repeated_scan = inner_nodes.iter().copied().find_map(|at| {
        if ctx.op(at) != Op::FullScan || !ctx.executed(at) {
            return None;
        }
        let scan_loops = ctx.m(at).loops?;
        let scan = ctx.scan(at);
        let read = scan.read.or(ctx.m(at).rows_out)?;
        (scan_loops >= 100.0 && read >= 100_000.0).then_some((at, scan_loops, read))
    });
    if let Some((at, scan_loops, read)) = repeated_scan
        && let Some(rel) = ctx.rel(at)
        && inner_share.is_none_or(|share| share >= 0.1)
    {
        let mut args: Args = vec![
            ("table", rel.display()),
            ("loops", count(scan_loops)),
            ("rows_read", count(read)),
            ("index_columns", String::new()),
        ];
        let severity = if inner_share.is_some_and(|share| share >= 0.5) && read >= 10_000_000.0 {
            S::Critical
        } else {
            S::Warning
        };
        // The join column: from the loop's join condition, else from the
        // filter the engine pushed onto the scan.
        let scan = ctx.scan(at);
        let column = ctx
            .a(position)
            .join_predicate
            .as_ref()
            .and_then(|condition| join_column(ctx, &rel, &ctx.terms(&condition.raw)))
            .or_else(|| {
                scan.filter_raw
                    .as_deref()
                    .and_then(|filter| join_column(ctx, &rel, &ctx.terms(filter)))
            });
        let draft = match column {
            Some(column) => {
                let mut suggestion = IndexSuggestion::for_rel(&rel);
                suggestion.equality.push(column);
                Draft::new(K::NestedLoopVolume, severity, C::High, "inner_scan", args)
                    .index(suggestion, "inner_scan_text")
            }
            None => {
                args.retain(|arg| arg.0 != "index_columns");
                args.push(("index_columns", String::new()));
                Draft::new(K::NestedLoopVolume, severity, C::High, "inner_scan", args)
                    .action("inner_scan_text")
            }
        };
        let mut draft = draft
            .at(position)
            .also(at)
            .ev("table", rel.display())
            .ev("executions", count(scan_loops))
            .ev("rows_read", count(read))
            .ev_opt(
                "join_condition",
                ctx.a(position)
                    .join_predicate
                    .as_ref()
                    .map(|p| p.text.clone()),
            )
            .ev_opt("filter", scan.filter_text.clone())
            .ev_opt("time", ctx.m(inner).inclusive_time_ms.map(ms));
        if let Some(impact) = impact_at(ctx, inner) {
            draft = draft.impact(impact);
        }
        drafts.push(draft);
        return;
    }

    // Many more inner executions than the optimizer planned for.
    let enough = match inner_share {
        Some(share) => loops >= 10_000.0 && share >= 0.25,
        None => loops >= 100_000.0,
    };
    if !enough {
        return;
    }
    let (Some(planned), Some(actual)) = (estimated(ctx, outer), ctx.m(outer).rows_out) else {
        return;
    };
    if actual < planned.max(1.0) * 10.0 {
        return;
    }
    let args: Args = vec![
        ("loops", count(loops)),
        ("estimated_rows", count(planned)),
        ("actual_rows", count(actual)),
    ];
    // Statistics to refresh: the outer side's table when it reads just one.
    let mut outer_nodes = ctx.descendants(outer);
    outer_nodes.insert(0, outer);
    let outer_rels: Vec<_> = outer_nodes.iter().filter_map(|at| ctx.rel(*at)).collect();
    let sql = match outer_rels.as_slice() {
        [rel] => stats_sql(ctx, rel),
        _ => None,
    };
    let mut draft = Draft::new(
        K::NestedLoopVolume,
        S::Warning,
        C::Medium,
        "underestimate",
        args,
    )
    .maybe_sql(sql)
    .at(position)
    .also(outer)
    .also(inner)
    .ev("executions", count(loops))
    .ev("estimated_rows", count(planned))
    .ev("actual_rows", count(actual))
    .ev_opt("time", ctx.m(inner).inclusive_time_ms.map(ms));
    if let Some(impact) = impact_at(ctx, inner) {
        draft = draft.impact(impact);
    }
    drafts.push(draft);
}

fn hash_build_side(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.op(position) != Op::HashJoin || !ctx.has_actuals() {
        return;
    }
    let kids = ctx.kids(position);
    if kids.len() != 2 {
        return;
    }
    let join_type = ctx
        .a(position)
        .join_type
        .as_deref()
        .unwrap_or("inner")
        .to_ascii_lowercase();
    // Outer, semi and anti joins restrict which side may be built.
    if !join_type.contains("inner") {
        return;
    }
    let (build, probe) = match ctx.dialect {
        PlanDialect::SqlServer | PlanDialect::Oracle => (kids[0], kids[1]),
        PlanDialect::Postgres | PlanDialect::MySql | PlanDialect::DuckDb => (kids[1], kids[0]),
        _ => return,
    };
    let (Some(build_rows), Some(probe_rows), Some(planned)) = (
        ctx.m(build).rows_out,
        ctx.m(probe).rows_out,
        estimated(ctx, build),
    ) else {
        return;
    };
    if build_rows < 100_000.0
        || build_rows < probe_rows.max(1.0) * 10.0
        || planned.max(1.0) * 10.0 > build_rows
    {
        return;
    }
    let severity = if ctx.share(position).is_some_and(|share| share >= 0.1) {
        S::Warning
    } else {
        S::Info
    };
    let factor = build_rows / planned.max(1.0);
    let args: Args = vec![
        ("build_rows", count(build_rows)),
        ("probe_rows", count(probe_rows)),
        ("estimated_rows", count(planned)),
        ("factor", ratio(factor)),
    ];
    let mut below = ctx.descendants(build);
    below.insert(0, build);
    let rels: Vec<_> = below.iter().filter_map(|at| ctx.rel(*at)).collect();
    let sql = match rels.as_slice() {
        [rel] => stats_sql(ctx, rel),
        _ => None,
    };
    drafts.push(
        Draft::new(K::HashBuildSide, severity, C::Medium, "larger", args)
            .maybe_sql(sql)
            .at(position)
            .also(build)
            .also(probe)
            .own_share()
            .ev("estimated_rows", count(planned))
            .ev("actual_rows", count(build_rows))
            .ev_opt(
                "hash_batches",
                ctx.node(position)
                    .hash_batches
                    .or(ctx.node(build).hash_batches)
                    .filter(|batches| *batches > 1)
                    .map(|batches| batches.to_string()),
            )
            .ev_opt("own_time", ctx.m(position).self_time_ms.map(ms)),
    );
}

fn join_fanout(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>, reducers: &mut Vec<usize>) {
    let op = ctx.op(position);
    if !op.is_join() || op == Op::CrossJoin || !ctx.has_actuals() {
        return;
    }
    let kids = ctx.kids(position);
    if kids.len() != 2 {
        return;
    }
    let Some(rows) = ctx.m(position).rows_out else {
        return;
    };
    // The inner side of a nested loop reports all executions together, which
    // is the number of matches, not the size of the input. Use the outer side
    // alone there.
    let input_rows = if op == Op::NestedLoop {
        ctx.m(kids[0]).rows_out
    } else {
        match (ctx.m(kids[0]).rows_out, ctx.m(kids[1]).rows_out) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        }
    };
    let Some(input_rows) = input_rows else {
        return;
    };
    if rows < 100_000.0 || rows < input_rows.max(1.0) * 10.0 {
        return;
    }
    // The nearest DISTINCT or GROUP BY above that collapses the rows again.
    let reducer = ctx.ancestors(position).into_iter().find(|at| {
        matches!(ctx.op(*at), Op::Aggregate | Op::Distinct)
            && ctx.m(*at).rows_out.is_some_and(|out| out * 10.0 <= rows)
    });
    let Some(reducer) = reducer else {
        return;
    };
    // Report the lowest join under each reducer: joins above it only carry
    // the rows it multiplied.
    if reducers.contains(&reducer) {
        return;
    }
    let below_fans_out = ctx.descendants(position).into_iter().any(|at| {
        ctx.op(at).is_join()
            && ctx
                .m(at)
                .rows_out
                .is_some_and(|out| out >= 100_000.0 && out * 2.0 >= rows)
    });
    if below_fans_out {
        return;
    }
    reducers.push(reducer);
    let reduced = ctx.m(reducer).rows_out.unwrap_or(0.0);
    let distinct = ctx.op(reducer) == Op::Distinct;
    let share = ctx.share(reducer);
    let severity = if share.is_some_and(|share| share >= 0.25) {
        S::Warning
    } else {
        S::Info
    };
    let confidence = if distinct { C::Medium } else { C::Low };
    let args: Args = vec![
        ("input_rows", count(input_rows)),
        ("rows", count(rows)),
        ("reducer", ctx.title(reducer)),
        ("reduced_rows", count(reduced)),
        ("per_row", count((rows / input_rows.max(1.0)).round())),
    ];
    let mut draft = Draft::new(K::JoinFanout, severity, confidence, "distinct", args)
        .at(position)
        .also(reducer)
        .ev_opt(
            "join_condition",
            ctx.a(position)
                .join_predicate
                .as_ref()
                .or(ctx.a(kids[1]).seek_predicate.as_ref())
                .map(|p| p.text.clone()),
        )
        .ev("rows_read", count(input_rows))
        .ev("rows_returned", count(rows))
        .ev_opt("time", ctx.m(reducer).inclusive_time_ms.map(ms));
    if let Some(impact) = impact_at(ctx, reducer) {
        draft = draft.impact(impact);
    }
    drafts.push(draft);
}

/// The topmost node of the subquery `position` starts, when it is one.
fn subquery_name(ctx: &Ctx<'_>, position: usize) -> Option<String> {
    let node = ctx.node(position);
    let name = node.subplan_name.as_deref().unwrap_or("").trim();
    match ctx.dialect {
        PlanDialect::Postgres => {
            (node.parent_relationship.as_deref() == Some("SubPlan")).then(|| {
                if name.is_empty() {
                    "A subquery".to_string()
                } else {
                    name.to_string()
                }
            })
        }
        PlanDialect::MySql | PlanDialect::MariaDb => {
            let select_type = node.property("select_type").unwrap_or("");
            let dependent = select_type.eq_ignore_ascii_case("DEPENDENT SUBQUERY")
                || (name.to_ascii_lowercase().contains("dependent")
                    && name.to_ascii_lowercase().contains("subquery"));
            // Only the first table of the subquery carries the finding.
            let first = ctx.up(position).is_none_or(|parent| {
                ctx.node(parent)
                    .subplan_name
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    != name
            });
            (dependent && first).then(|| {
                if name.is_empty() {
                    "A subquery".to_string()
                } else {
                    name.to_string()
                }
            })
        }
        _ => None,
    }
}

/// The index for a scan inside a correlated subquery: the column compared
/// with the outer query's row leads, then the scan's own conditions.
fn correlated_suggestion(ctx: &Ctx<'_>, rel: &Rel<'_>, filter: &str) -> Option<IndexSuggestion> {
    let terms = ctx.terms(filter);
    let correlated = join_column(ctx, rel, &terms);
    let mut suggestion = match suggestion_from_terms(rel, &terms) {
        Some(suggestion) => suggestion,
        None => {
            correlated.as_ref()?;
            IndexSuggestion::for_rel(rel)
        }
    };
    if let Some(column) = correlated
        && !suggestion
            .equality
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&column))
    {
        suggestion
            .range
            .retain(|existing| !existing.eq_ignore_ascii_case(&column));
        suggestion.equality.insert(0, column);
    }
    Some(suggestion)
}

fn dependent_subquery(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.dialect == PlanDialect::Sqlite {
        sqlite_correlated(ctx, position, drafts);
        return;
    }
    let Some(subquery) = subquery_name(ctx, position) else {
        return;
    };
    let metrics = ctx.m(position);
    if let Some(loops) = metrics.loops.filter(|_| ctx.has_actuals()) {
        let share = ctx.share(position);
        if loops < 1_000.0 || share.is_some_and(|share| share < 0.1) {
            return;
        }
        // Without a share to go by, only a large count is worth reporting.
        if share.is_none() && loops < 10_000.0 {
            return;
        }
        let severity = if share.is_some_and(|share| share >= 0.5) {
            S::Critical
        } else {
            S::Warning
        };
        let args: Args = vec![
            ("subquery", subquery),
            ("loops", count(loops)),
            ("index_columns", String::new()),
        ];
        let mut draft = Draft::new(K::DependentSubquery, severity, C::High, "loops", args)
            .at(position)
            .ev("executions", count(loops))
            .ev_opt("time", metrics.inclusive_time_ms.map(ms))
            .ev_opt("cost_share", (!ctx.timed).then(|| share.map(pct)).flatten());
        // A full scan inside the subquery can also be made cheap per run.
        let mut inside = ctx.descendants(position);
        inside.insert(0, position);
        let scans: Vec<usize> = inside
            .iter()
            .copied()
            .filter(|at| ctx.op(*at) == Op::FullScan && ctx.executed(*at))
            .collect();
        if let [at] = scans.as_slice()
            && let Some(rel) = ctx.rel(*at)
            && let Some(filter) = ctx.scan(*at).filter_raw
            && let Some(suggestion) = correlated_suggestion(ctx, &rel, &filter)
        {
            draft = draft
                .also(*at)
                .arg("table", rel.display())
                .ev("table", rel.display())
                .ev_opt("filter", ctx.scan(*at).filter_text)
                .action("loops_index")
                .index(suggestion, "loops.action");
        }
        drafts.push(draft);
        return;
    }
    if ctx.has_actuals() || !matches!(ctx.dialect, PlanDialect::MySql | PlanDialect::MariaDb) {
        return;
    }
    // Estimated MySQL plan: the outer query's row estimate is the number of
    // times the subquery runs.
    let mut inside = ctx.descendants(position);
    inside.push(position);
    let outer_rows = (0..ctx.len())
        .filter(|at| !inside.contains(at) && ctx.node(*at).subplan_name.is_none())
        .filter(|at| ctx.op(*at).is_read())
        .filter_map(|at| ctx.node(at).estimated_rows_read.or(estimated(ctx, at)))
        .fold(0.0, f64::max);
    if outer_rows < 10_000.0 {
        return;
    }
    let args: Args = vec![("subquery", subquery), ("rows", count(outer_rows))];
    drafts.push(
        Draft::new(K::DependentSubquery, S::Info, C::Low, "estimated", args)
            .at(position)
            .ev("estimated_rows", count(outer_rows))
            .note("common.note.estimated_only", &[]),
    );
}

fn sqlite_correlated(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let name = ctx.op_name(position);
    let label = ctx
        .node(position)
        .subplan_name
        .as_deref()
        .unwrap_or("")
        .to_ascii_uppercase();
    let correlated = |text: &str| text.contains("CORRELATED") && text.contains("SUBQUERY");
    if !(correlated(&name) || correlated(&label)) {
        return;
    }
    // Only when this node starts the subquery.
    if ctx.up(position).is_some_and(|parent| {
        correlated(
            &ctx.node(parent)
                .subplan_name
                .as_deref()
                .unwrap_or("")
                .to_ascii_uppercase(),
        ) && !correlated(&name)
    }) {
        return;
    }
    let mut inside = ctx.descendants(position);
    inside.insert(0, position);
    let Some((at, rel)) = inside.iter().copied().find_map(|at| {
        (ctx.op(at) == Op::FullScan)
            .then(|| ctx.rel(at))
            .flatten()
            .map(|rel| (at, rel))
    }) else {
        return;
    };
    let args: Args = vec![("table", rel.display())];
    let mut draft = Draft::new(K::DependentSubquery, S::Info, C::Low, "sqlite_scan", args)
        .at(position)
        .also(at)
        .ev("table", rel.display());
    if rel.alias_only {
        draft = draft.note("common.note.alias", &[("table", rel.display())]);
    }
    drafts.push(draft);
}

fn join_buffer(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if !matches!(ctx.dialect, PlanDialect::MySql | PlanDialect::MariaDb) {
        return;
    }
    let node = ctx.node(position);
    let Some(buffer) = node.extra.iter().find(|extra| {
        let lower = extra.to_ascii_lowercase();
        lower.starts_with("using join buffer")
            && (lower.contains("block nested loop") || lower.contains("bnl"))
            && !lower.contains("hash")
    }) else {
        return;
    };
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    let scan = ctx.scan(position);
    let measured = ctx.has_actuals();
    let inner = if measured {
        ctx.m(position).rows_per_loop.or(scan.read)
    } else {
        scan.est_read.or(ctx.m(position).estimated_rows)
    };
    // The rows already joined before this table: the inputs listed before it
    // under the same join.
    let outer = ctx.up(position).and_then(|parent| {
        let before: Vec<usize> = ctx
            .kids(parent)
            .iter()
            .copied()
            .take_while(|kid| *kid != position)
            .collect();
        let last = *before.last()?;
        if measured {
            ctx.m(last).rows_out
        } else {
            estimated(ctx, last)
        }
    });
    let (Some(inner), Some(outer)) = (inner, outer) else {
        return;
    };
    let product = inner * outer;
    if product < 1_000_000.0 {
        return;
    }
    let severity = if measured || product >= 100_000_000.0 {
        S::Warning
    } else {
        S::Info
    };
    let confidence = if measured { C::High } else { C::Medium };
    let args: Args = vec![
        ("table", rel.display()),
        ("rows", count(product)),
        ("index_columns", String::new()),
    ];
    let column = scan
        .filter_raw
        .as_deref()
        .and_then(|filter| join_column(ctx, &rel, &ctx.terms(filter)));
    let draft = Draft::new(K::JoinBuffer, severity, confidence, "block", args);
    let draft = match column {
        Some(column) => {
            let mut suggestion = IndexSuggestion::for_rel(&rel);
            suggestion.equality.push(column);
            draft.index(suggestion, "block_text")
        }
        None => draft.action("block_text"),
    };
    let mut draft = draft
        .at(position)
        .ev("table", rel.display())
        .ev("reason", buffer.clone())
        .ev_opt("filter", scan.filter_text.clone());
    draft = if measured {
        draft.ev("rows_read", count(inner * 1.0))
    } else {
        draft
            .ev(
                "estimated_rows",
                format!("{} × {}", count(outer), count(inner)),
            )
            .note("common.note.estimated_only", &[])
    };
    drafts.push(draft);
}
