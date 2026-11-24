//! Rules about row estimates and the statistics behind them.

use super::advice;
use super::facts::{Ctx, Op, count, ratio};
use super::predicate::TermKind;
use super::strings::{Args, Template};
use super::{
    Draft, FindingConfidence as C, FindingImpact, FindingKind as K, FindingSeverity as S,
    ImpactBasis,
};
use crate::plans::PlanDialect;
use crate::plans::schema::{bare_name, split_qualified};
use crate::plans::warning::PlanWarning;

pub(super) const TEMPLATES: &[Template] = &[
    // ---- estimate_mismatch
    (
        "estimate_mismatch.scan.summary",
        "The optimizer expected {estimated_rows} rows from the read of {table} and got {actual_rows}, off by a factor of {factor}.",
        "summary: a table read whose row estimate was far off",
    ),
    (
        "estimate_mismatch.scan.why",
        "Join order, join method and memory are chosen from this number, and the operators above inherit the error. When far more rows arrive than planned, loops repeat and sorts and hashes run out of memory; when far fewer arrive, the plan scans and reserves memory it did not need.",
        "why a wrong row estimate costs time",
    ),
    (
        "estimate_mismatch.scan.action",
        "Refresh the statistics on {table} and capture the plan again. If the estimate stays wrong, the filter probably combines columns whose values depend on each other, which single-column statistics cannot describe.",
        "action: refresh statistics after a wrong estimate on a table read",
    ),
    (
        "estimate_mismatch.unknown_value.summary",
        "The optimizer did not know the value of {parameter} when it compiled this plan: it guessed {estimated_rows} rows from {table} and {actual_rows} came back, off by a factor of {factor}.",
        "summary: SQL Server estimated a filter on a local variable without knowing its value",
    ),
    (
        "estimate_mismatch.unknown_value.why",
        "The plan records the value {parameter} ran with but no compiled value, which is what a local variable or OPTIMIZE FOR UNKNOWN produces. Without a value the optimizer cannot use the column's histogram and falls back to a fixed guess, such as 30% of the table for a comparison with < or >. Refreshing statistics does not change that guess.",
        "why a filter on a local variable is misestimated on SQL Server",
    ),
    (
        "estimate_mismatch.unknown_value.action",
        "Add OPTION (RECOMPILE) to the statement so it is compiled with the value it runs with. If the variable only copies a procedure parameter, use the parameter itself so its value is seen at compile time.",
        "action for a misestimated filter on a local variable",
    ),
    (
        "estimate_mismatch.alt_extended",
        "If the filter columns are correlated, create extended statistics on them, then analyze the table.",
        "alternative: PostgreSQL extended statistics",
    ),
    (
        "estimate_mismatch.join.summary",
        "The optimizer expected {estimated_rows} rows from the join on {join_condition} and got {actual_rows}, off by a factor of {factor}.",
        "summary: a join whose row estimate was far off while its inputs were estimated well",
    ),
    (
        "estimate_mismatch.join.why",
        "The inputs of this join were estimated well, so the error comes from the join itself: the optimizer guessed wrong about how many rows on one side match each row on the other. Everything above the join is planned for the wrong number of rows.",
        "why a wrong join estimate costs time",
    ),
    (
        "estimate_mismatch.join.action",
        "Check that statistics on the join columns are current on both tables. If the join compares expressions or columns of different types, compare plain columns of the same type so the optimizer can use their statistics.",
        "action for a wrong join estimate",
    ),
    (
        "estimate_mismatch.operator.summary",
        "The optimizer expected {estimated_rows} rows from {operator} and got {actual_rows}, off by a factor of {factor}.",
        "summary: an operator whose row estimate was far off while its inputs were estimated well",
    ),
    (
        "estimate_mismatch.operator.why",
        "The inputs of this operator were estimated well, so the error starts here. Everything above it is planned for the wrong number of rows.",
        "why a wrong estimate on an operator costs time",
    ),
    (
        "estimate_mismatch.operator.action",
        "Look at what this operator computes: estimates for grouped, distinct or function results are guesses unless statistics describe the columns involved. Refreshing statistics on the tables below is the first thing to try.",
        "action for a wrong estimate on a non-join operator",
    ),
    // ---- stale_statistics
    (
        "stale_statistics.rows.summary",
        "The optimizer expected {estimated_rows} rows in {table}; the scan read {actual_rows}.",
        "summary: an unfiltered scan whose estimate differs from the real table size",
    ),
    (
        "stale_statistics.rows.why",
        "This scan has no filter, so the estimate is the table's row count as the statistics recorded it. A number this far off means the statistics describe the table as it was, not as it is, and every estimate involving {table} starts from them.",
        "why out-of-date table statistics cost time",
    ),
    (
        "stale_statistics.rows.action",
        "Refresh the statistics on {table}, then capture the plan again.",
        "action: refresh table statistics",
    ),
    (
        "stale_statistics.modified.summary",
        "{modifications} rows of {table} have changed since its statistics {statistics} were last updated; the table has about {rows} rows.",
        "summary: SQL Server statistics with a high modification count",
    ),
    (
        "stale_statistics.modified.why",
        "The optimizer used these statistics to plan the statement. With this many changes since they were built, the value distribution they describe may no longer match the data.",
        "why statistics with many modifications matter",
    ),
    (
        "stale_statistics.modified.action",
        "Update the statistics on {table}, then capture the plan again.",
        "action: update statistics in SQL Server",
    ),
    // ---- cardinality
    (
        "cardinality.no_stats.summary",
        "SQL Server had no statistics for {columns} when it planned the read of {table}.",
        "summary: columns without statistics",
    ),
    (
        "cardinality.no_stats.why",
        "Without statistics the optimizer uses fixed guesses for how many rows a condition on these columns matches. SQL Server normally creates such statistics by itself; it could not here, for example because AUTO_CREATE_STATISTICS is off or the database is read-only.",
        "why missing column statistics cost time",
    ),
    (
        "cardinality.no_stats.action",
        "Create statistics on the columns, and check the database's AUTO_CREATE_STATISTICS setting.",
        "action: create column statistics",
    ),
    // ---- parameter_sniffing
    (
        "parameter_sniffing.compiled.summary",
        "The plan was compiled for {parameter} = {compiled_value} and ran with {runtime_value}; at {target} the optimizer expected {estimated_rows} rows and got {actual_rows}.",
        "summary: cached plan compiled for a different parameter value",
    ),
    (
        "parameter_sniffing.compiled.why",
        "SQL Server builds the plan for the parameter values of the first call and reuses it. The value this run used matches a very different number of rows, so the cached plan's join and access choices do not fit it.",
        "why parameter sniffing costs time",
    ),
    (
        "parameter_sniffing.compiled.action",
        "If this statement runs rarely or its values vary widely, compile it for each run by adding the hint below. This spends CPU on every execution.",
        "action: OPTION (RECOMPILE)",
    ),
    (
        "parameter_sniffing.alt_unknown",
        "Or plan for an average value instead of the first one seen. This gives one stable plan that is not tuned to any single value.",
        "alternative: OPTIMIZE FOR UNKNOWN",
    ),
    // ---- table_variable
    (
        "table_variable.rows.summary",
        "The table variable {table} was planned as {estimated_rows} row and held {actual_rows}.",
        "summary: table variable with a fixed low estimate",
    ),
    (
        "table_variable.rows.why",
        "Table variables have no statistics, and a statement compiled before the variable is filled assumes it holds one row. Joins against it are then planned as if it were tiny.",
        "why a table variable's fixed estimate costs time",
    ),
    (
        "table_variable.rows.action",
        "Use a temporary table (#name) instead, which has statistics, or add the hint below so the statement is compiled after the variable is filled. From SQL Server 2019 at compatibility level 150 the engine defers compiling such statements and estimates the real row count.",
        "action for a table variable estimate",
    ),
    // ---- row_goal
    (
        "row_goal.scan.summary",
        "The optimizer planned to stop the read of {table} after about {estimated_rows} rows, but it read {rows_read}.",
        "summary: a row goal that did not pay off",
    ),
    (
        "row_goal.scan.why",
        "The query asks for only the first rows (TOP, EXISTS, IN or a FAST hint), so the optimizer chose a plan that is cheap when matching rows turn up early. They did not, and the read went on far longer than planned.",
        "why a failed row goal costs time",
    ),
    (
        "row_goal.scan.action",
        "Index the columns the query filters and orders by so the first rows are found by a seek. To see the plan the optimizer would pick without the row goal, add the hint below.",
        "action for a failed row goal",
    ),
];

pub(super) fn run(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    for position in 0..ctx.len() {
        if !ctx.executed(position) {
            continue;
        }
        stale_rows(ctx, position, drafts);
        table_variable(ctx, position, drafts);
        row_goal(ctx, position, drafts);
        cardinality(ctx, position, drafts);
        estimate_mismatch(ctx, position, drafts);
    }
    stale_modified(ctx, drafts);
    parameter_sniffing(ctx, drafts);
}

/// The impact of a finding whose cost shows up at `position`.
pub(super) fn impact_at(ctx: &Ctx<'_>, position: usize) -> Option<FindingImpact> {
    let basis = ctx.basis()?;
    Some(FindingImpact {
        share: ctx.share(position)?.clamp(0.0, 1.0),
        basis,
        time_ms: (basis == ImpactBasis::Time)
            .then(|| ctx.m(position).inclusive_time_ms)
            .flatten(),
        optimizer_estimate: false,
    })
}

fn factor(ctx: &Ctx<'_>, position: usize) -> Option<(f64, f64, f64)> {
    // (factor, estimated, actual), compared like for like.
    let metrics = ctx.m(position);
    let factor = metrics.estimate_factor?;
    let estimated = metrics.estimated_rows?;
    let actual = metrics.estimate_compared_rows.or(metrics.rows_per_loop)?;
    Some((factor, estimated, actual))
}

/// True when something above `position` can stop it early, which makes its
/// actual row count lower than a correct estimate.
fn can_stop_early(ctx: &Ctx<'_>, position: usize) -> bool {
    let row_goal = |at: usize| {
        ctx.node(at)
            .warnings
            .iter()
            .any(|warning| matches!(warning, PlanWarning::RowGoal))
    };
    if row_goal(position) {
        return true;
    }
    ctx.ancestors(position).into_iter().any(|above| {
        let join_type = ctx
            .a(above)
            .join_type
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        matches!(ctx.op(above), Op::Limit | Op::MergeJoin)
            || join_type.contains("semi")
            || join_type.contains("anti")
            || row_goal(above)
    })
}

/// True when `text` mentions the variable `name` as a whole word.
fn mentions(text: &str, name: &str) -> bool {
    let text = text.to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    text.match_indices(&name).any(|(start, _)| {
        text[start + name.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
    })
}

/// A SQL Server variable in this node's predicates whose value the optimizer
/// did not have at compile time: the plan lists a runtime value for it and no
/// compiled value.
fn unknown_value_parameter(ctx: &Ctx<'_>, position: usize) -> Option<String> {
    if ctx.dialect != PlanDialect::SqlServer {
        return None;
    }
    let analysis = ctx.a(position);
    let scan = ctx.scan(position);
    let texts: Vec<&str> = [
        scan.filter_raw.as_deref(),
        analysis.seek_predicate.as_ref().map(|p| p.raw.as_str()),
        analysis.filter.as_ref().map(|p| p.raw.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect();
    ctx.plan
        .parameters
        .iter()
        .filter(|parameter| parameter.compiled_value.is_none() && parameter.runtime_value.is_some())
        .map(|parameter| parameter.name.trim())
        .find(|name| name.starts_with('@') && texts.iter().any(|text| mentions(text, name)))
        .map(str::to_string)
}

fn estimate_mismatch(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if matches!(
        ctx.dialect,
        PlanDialect::DuckDb | PlanDialect::ClickHouse | PlanDialect::Sqlite
    ) {
        return;
    }
    let Some((off, estimated, actual)) = factor(ctx, position) else {
        return;
    };
    if off < 10.0 || estimated <= 0.0 || estimated.max(actual) < 1_000.0 {
        return;
    }
    // Report the error where it starts, not on every operator above it.
    if ctx
        .descendants(position)
        .into_iter()
        .any(|below| ctx.m(below).estimate_factor.is_some_and(|f| f >= 10.0))
    {
        return;
    }
    let over = estimated > actual;
    if over && can_stop_early(ctx, position) {
        return;
    }
    // A filter on a window function's result (`WHERE rn <= 10`) has no
    // statistics behind it in any engine: refreshing them changes nothing.
    if ctx.a(position).flags.filters_window_result {
        return;
    }
    // It matters only when an operator above plans from this number.
    let affected = ctx.ancestors(position).into_iter().find(|above| {
        ctx.op(*above).is_join()
            || matches!(
                ctx.op(*above),
                Op::Sort | Op::Aggregate | Op::Distinct | Op::HashBuild
            )
    });
    let Some(affected) = affected else {
        return;
    };
    let top = ctx
        .ancestors(position)
        .into_iter()
        .filter(|above| {
            ctx.op(*above).is_join() || matches!(ctx.op(*above), Op::Sort | Op::Aggregate)
        })
        .max_by(|a, b| {
            ctx.share(*a)
                .unwrap_or(0.0)
                .total_cmp(&ctx.share(*b).unwrap_or(0.0))
        })
        .unwrap_or(affected);
    let share = ctx.share(top);
    if share.is_some_and(|share| share < 0.1) {
        return;
    }
    let severity = if !over && estimated.max(actual) >= 10_000.0 {
        S::Warning
    } else {
        S::Info
    };
    let mut args: Args = vec![
        ("estimated_rows", count(estimated)),
        ("actual_rows", count(actual)),
        ("factor", ratio(off)),
        ("operator", ctx.title(position)),
    ];
    let op = ctx.op(position);
    let mut draft = if let (true, Some(rel)) =
        (op.is_read() || op == Op::RowLookup, ctx.rel(position))
    {
        args.push(("table", rel.display()));
        let unknown = unknown_value_parameter(ctx, position);
        let mut draft = match &unknown {
            Some(parameter) => {
                args.push(("parameter", parameter.clone()));
                Draft::new(
                    K::EstimateMismatch,
                    severity,
                    C::High,
                    "unknown_value",
                    args,
                )
                .sql("OPTION (RECOMPILE)")
                .ev("parameter", format!("{parameter}: no compiled value"))
            }
            None => Draft::new(K::EstimateMismatch, severity, C::High, "scan", args)
                .maybe_sql(advice::stats_sql(ctx, &rel)),
        }
        .ev("table", rel.display());
        // Two or more equality columns in one filter: the classic case for
        // PostgreSQL extended statistics.
        if ctx.dialect == PlanDialect::Postgres
            && let Some(table) = advice::table_sql(ctx, &rel)
        {
            let scan = ctx.scan(position);
            let mut columns: Vec<String> = Vec::new();
            for text in [
                scan.filter_raw.as_deref(),
                ctx.a(position)
                    .seek_predicate
                    .as_ref()
                    .map(|p| p.raw.as_str()),
            ]
            .into_iter()
            .flatten()
            {
                for term in ctx.terms(text) {
                    if term.kind == TermKind::Equality
                        && let Some(column) = term.bare_column()
                        && rel.owns(column)
                        && !columns.iter().any(|c| c.eq_ignore_ascii_case(&column.name))
                    {
                        columns.push(column.name.clone());
                    }
                }
            }
            if columns.len() >= 2 && columns.len() <= 8 {
                let name = format!(
                    "st_{}_{}",
                    rel.name.to_ascii_lowercase(),
                    columns.join("_").to_ascii_lowercase()
                );
                let list = columns
                    .iter()
                    .map(|column| advice::quote(ctx, column))
                    .collect::<Vec<_>>()
                    .join(", ");
                draft = draft.alt(
                    "alt_extended",
                    Some(format!(
                        "CREATE STATISTICS {} (dependencies, ndistinct) ON {list} FROM {table};\nANALYZE {table};",
                        advice::quote(ctx, &name)
                    )),
                );
            }
        }
        draft
            .ev_opt("filter", ctx.scan(position).filter_text)
            .ev_opt(
                "index_condition",
                ctx.a(position)
                    .seek_predicate
                    .as_ref()
                    .map(|p| p.text.clone()),
            )
    } else if op.is_join() {
        let Some(condition) = ctx.a(position).join_predicate.as_ref() else {
            return;
        };
        args.push(("join_condition", condition.text.clone()));
        Draft::new(K::EstimateMismatch, severity, C::High, "join", args)
            .ev("join_condition", condition.text.clone())
    } else {
        // A grouped or computed row count that is off matters only when
        // the numbers are large enough to change what is planned above.
        if estimated.max(actual) < 10_000.0 {
            return;
        }
        Draft::new(K::EstimateMismatch, severity, C::Medium, "operator", args)
    };
    draft = draft
        .at(position)
        .also(top)
        .ev("estimated_rows", count(estimated))
        .ev("actual_rows", count(actual))
        .ev_opt("executions", ctx.executions(position).map(count));
    if let Some(impact) = impact_at(ctx, top) {
        draft = draft.impact(impact);
    }
    drafts.push(draft);
}

fn stale_rows(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.op(position) != Op::FullScan
        || matches!(
            ctx.dialect,
            PlanDialect::DuckDb | PlanDialect::ClickHouse | PlanDialect::Sqlite
        )
    {
        return;
    }
    let scan = ctx.scan(position);
    if scan.filter_raw.is_some() || ctx.a(position).seek_predicate.is_some() {
        return;
    }
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    let Some((off, estimated, actual)) = factor(ctx, position) else {
        return;
    };
    let metrics = ctx.m(position);
    let parallel = ctx.node(position).parallel || metrics.processes.is_some();
    let needed = if parallel { 4.0 } else { 2.0 };
    if off < needed
        || estimated.max(actual) < 10_000.0
        || metrics.loops.is_some_and(|loops| loops > 1.0) && metrics.processes.is_none()
        || can_stop_early(ctx, position)
    {
        return;
    }
    let severity = if off >= 10.0 { S::Warning } else { S::Info };
    let args: Args = vec![
        ("table", rel.display()),
        ("estimated_rows", count(estimated)),
        ("actual_rows", count(actual)),
    ];
    let mut draft = Draft::new(K::StaleStatistics, severity, C::High, "rows", args)
        .maybe_sql(advice::stats_sql(ctx, &rel))
        .at(position)
        .ev("table", rel.display())
        .ev("estimated_rows", count(estimated))
        .ev("actual_rows", count(actual));
    if rel.alias_only {
        draft = draft.note("common.note.alias", &[("table", rel.display())]);
    }
    // The cost of stale statistics is in the plan built from them.
    if let Some(root) = ctx.ancestors(position).last().copied()
        && let Some(impact) = impact_at(ctx, root)
    {
        draft = draft.impact(FindingImpact {
            share: impact.share.min(0.5),
            ..impact
        });
    }
    drafts.push(draft);
}

fn stale_modified(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::SqlServer {
        return;
    }
    let mut seen: Vec<String> = Vec::new();
    for used in &ctx.plan.statistics_used {
        let table = bare_name(&used.table);
        let Some(modified) = used.modification_count else {
            continue;
        };
        if table.is_empty() || modified < 1_000.0 || seen.contains(&table.to_ascii_lowercase()) {
            continue;
        }
        // The table's size, from a read of it in this plan.
        let read = (0..ctx.len()).find(|position| {
            ctx.rel(*position)
                .is_some_and(|rel| rel.name.eq_ignore_ascii_case(&table))
        });
        let Some(read) = read else {
            continue;
        };
        let rows = ctx
            .node(read)
            .property("TableCardinality")
            .and_then(|value| value.parse::<f64>().ok())
            .or_else(|| {
                (ctx.op(read) == Op::FullScan)
                    .then(|| ctx.node(read).rows_read)
                    .flatten()
            });
        let Some(rows) = rows.filter(|rows| *rows > 0.0) else {
            continue;
        };
        if modified < rows * 0.2 {
            continue;
        }
        // Already reported from the row counts.
        if drafts
            .iter()
            .any(|draft| draft.kind == K::StaleStatistics && draft.node == Some(read))
        {
            continue;
        }
        seen.push(table.to_ascii_lowercase());
        let Some(rel) = ctx.rel(read) else {
            continue;
        };
        let misestimated = (0..ctx.len()).any(|position| {
            ctx.rel(position)
                .is_some_and(|other| other.name.eq_ignore_ascii_case(&table))
                && ctx.m(position).estimate_factor.is_some_and(|f| f >= 10.0)
        });
        let statistics = bare_name(&used.statistics);
        let args: Args = vec![
            ("table", rel.display()),
            ("statistics", statistics.clone()),
            ("modifications", count(modified)),
            ("rows", count(rows)),
        ];
        let mut detail = format!("{statistics}, {} modifications", count(modified));
        if let Some(sampled) = used.sampling_percent {
            detail.push_str(&format!(", sampled {sampled:.0}%"));
        }
        if let Some(updated) = &used.last_update {
            detail.push_str(&format!(", last updated {updated}"));
        }
        let severity = if misestimated { S::Warning } else { S::Info };
        drafts.push(
            Draft::new(K::StaleStatistics, severity, C::Medium, "modified", args)
                .maybe_sql(advice::stats_sql(ctx, &rel))
                .at(read)
                .ev("table", rel.display())
                .ev("statistics", detail),
        );
    }
}

fn cardinality(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::SqlServer {
        return;
    }
    let node = ctx.node(position);
    if node.columns_without_statistics.is_empty() {
        return;
    }
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    let mut columns: Vec<String> = Vec::new();
    for raw in &node.columns_without_statistics {
        let mut parts = split_qualified(raw);
        let Some(column) = parts.pop() else { continue };
        // Only columns of the table this node reads.
        if let Some(table) = parts.pop()
            && !table.eq_ignore_ascii_case(&rel.name)
            && !rel
                .alias
                .as_deref()
                .is_some_and(|alias| alias.eq_ignore_ascii_case(&table))
        {
            continue;
        }
        if !columns.iter().any(|c| c.eq_ignore_ascii_case(&column)) {
            columns.push(column);
        }
    }
    if columns.is_empty() {
        return;
    }
    let off = ctx.m(position).estimate_factor.unwrap_or(1.0);
    let rows = ctx.m(position).rows_out.unwrap_or(0.0);
    let severity = if off >= 10.0 && rows >= 1_000.0 {
        S::Warning
    } else {
        S::Info
    };
    let sql = advice::table_sql(ctx, &rel).map(|table| {
        columns
            .iter()
            .map(|column| {
                format!(
                    "CREATE STATISTICS {} ON {table} ({});",
                    advice::quote(ctx, &format!("st_{}_{column}", rel.name)),
                    advice::quote(ctx, column)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });
    let args: Args = vec![("table", rel.display()), ("columns", columns.join(", "))];
    drafts.push(
        Draft::new(K::Cardinality, severity, C::High, "no_stats", args)
            .maybe_sql(sql)
            .at(position)
            .ev("table", rel.display())
            .ev("columns", columns.join(", "))
            .ev("warning", "ColumnsWithNoStatistics"),
    );
}

fn parameter_sniffing(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::SqlServer || !ctx.has_actuals() {
        return;
    }
    for parameter in &ctx.plan.parameters {
        let (Some(compiled), Some(runtime)) = (&parameter.compiled_value, &parameter.runtime_value)
        else {
            continue;
        };
        if compiled.trim() == runtime.trim() || parameter.name.trim().is_empty() {
            continue;
        }
        let name = parameter.name.trim();
        let lower = name.to_ascii_lowercase();
        let mentions = |text: &str| -> bool {
            let text = text.to_ascii_lowercase();
            text.match_indices(&lower).any(|(start, _)| {
                text[start + lower.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
            })
        };
        // A misestimated operator whose own predicate uses the parameter.
        let mut best: Option<(usize, f64)> = None;
        for position in 0..ctx.len() {
            if !ctx.executed(position) {
                continue;
            }
            let Some((off, estimated, actual)) = factor(ctx, position) else {
                continue;
            };
            if off < 10.0 || estimated.max(actual) < 1_000.0 {
                continue;
            }
            let analysis = ctx.a(position);
            let uses = [
                &analysis.filter,
                &analysis.seek_predicate,
                &analysis.join_predicate,
            ]
            .into_iter()
            .flatten()
            .any(|predicate| mentions(&predicate.raw));
            if uses && best.is_none_or(|(_, top)| off > top) {
                best = Some((position, off));
            }
        }
        let Some((position, _)) = best else {
            continue;
        };
        let Some((_, estimated, actual)) = factor(ctx, position) else {
            continue;
        };
        let target = ctx.title(position);
        let args: Args = vec![
            ("parameter", name.to_string()),
            ("compiled_value", compiled.trim().to_string()),
            ("runtime_value", runtime.trim().to_string()),
            ("target", target),
            ("estimated_rows", count(estimated)),
            ("actual_rows", count(actual)),
        ];
        let mut draft = Draft::new(K::ParameterSniffing, S::Warning, C::High, "compiled", args)
            .sql("OPTION (RECOMPILE)")
            .alt(
                "alt_unknown",
                Some(format!("OPTION (OPTIMIZE FOR ({name} UNKNOWN))")),
            )
            .at(position)
            .ev(
                "parameter",
                format!(
                    "{name}: compiled {}, ran with {}",
                    compiled.trim(),
                    runtime.trim()
                ),
            )
            .ev("estimated_rows", count(estimated))
            .ev("actual_rows", count(actual));
        if let Some(root) = ctx.ancestors(position).last().copied()
            && let Some(impact) = impact_at(ctx, root)
        {
            draft = draft.impact(FindingImpact {
                share: impact.share.min(0.5),
                ..impact
            });
        }
        drafts.push(draft);
        // One finding per plan: the remaining parameters tell the same story.
        return;
    }
}

fn table_variable(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::SqlServer {
        return;
    }
    let node = ctx.node(position);
    let Some(relation) = node.relation.as_deref().map(bare_name) else {
        return;
    };
    if !relation.starts_with('@') {
        return;
    }
    let metrics = ctx.m(position);
    let (Some(estimated), Some(actual)) = (metrics.estimated_rows, metrics.rows_out) else {
        return;
    };
    if estimated > 1.0 || actual < 1_000.0 {
        return;
    }
    let args: Args = vec![
        ("table", relation.clone()),
        ("estimated_rows", count(estimated.max(1.0))),
        ("actual_rows", count(actual)),
    ];
    let mut draft = Draft::new(K::TableVariable, S::Warning, C::High, "rows", args)
        .sql("OPTION (RECOMPILE)")
        .at(position)
        .ev("table", relation)
        .ev("estimated_rows", count(estimated))
        .ev("actual_rows", count(actual));
    if let Some(above) = ctx
        .ancestors(position)
        .into_iter()
        .find(|a| ctx.op(*a).is_join())
        && let Some(impact) = impact_at(ctx, above)
    {
        draft = draft.also(above).impact(impact);
    }
    drafts.push(draft);
}

fn row_goal(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::SqlServer || !ctx.op(position).is_read() {
        return;
    }
    let node = ctx.node(position);
    if !node
        .warnings
        .iter()
        .any(|warning| matches!(warning, PlanWarning::RowGoal))
    {
        return;
    }
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    let metrics = ctx.m(position);
    let Some(read) = node.rows_read.or(metrics.rows_out) else {
        return;
    };
    let Some(estimated) = metrics.estimated_rows_total.or(metrics.estimated_rows) else {
        return;
    };
    if read < 10_000.0 || read < estimated.max(1.0) * 100.0 {
        return;
    }
    let args: Args = vec![
        ("table", rel.display()),
        ("estimated_rows", count(estimated.max(1.0))),
        ("rows_read", count(read)),
    ];
    let severity = if ctx.share(position).is_none_or(|share| share >= 0.25) {
        S::Warning
    } else {
        S::Info
    };
    drafts.push(
        Draft::new(K::RowGoal, severity, C::High, "scan", args)
            .sql("OPTION (USE HINT ('DISABLE_OPTIMIZER_ROWGOAL'))")
            .at(position)
            .ev("table", rel.display())
            .ev("estimated_rows", count(estimated))
            .ev("rows_read", count(read))
            .ev_opt("filter", ctx.scan(position).filter_text),
    );
}
