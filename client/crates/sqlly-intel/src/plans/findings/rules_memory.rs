//! Rules about memory: sorts and hashes that went to disk, memory grants
//! that were far too large, and temporary tables built to group or
//! de-duplicate.

use super::advice::IndexSuggestion;
use super::facts::{Ctx, Op, Rel, count, mem_kb, ms, pct};
use super::predicate::TermKind;
use super::strings::{Args, Template};
use super::{Draft, FindingConfidence as C, FindingKind as K, FindingSeverity as S};
use crate::plans::PlanDialect;
use crate::plans::warning::PlanWarning;

pub(super) const TEMPLATES: &[Template] = &[
    // ---- spill
    (
        "spill.tempdb_underestimate.summary",
        "{operator} ran out of memory and spilled to tempdb: it was planned for {estimated_rows} rows and received {actual_rows}.",
        "summary: SQL Server sort or hash spilled to tempdb because of a low row estimate",
    ),
    (
        "spill.tempdb_underestimate.why",
        "SQL Server reserves memory for a sort or hash before the query starts, sized from the row estimate. More rows arrived than the grant could hold, so the rest was written to tempdb and read back, which is far slower than working in memory.",
        "why a tempdb spill caused by a low estimate costs time",
    ),
    (
        "spill.tempdb_underestimate.action",
        "Fix the row estimate, and the grant follows. Update statistics on the tables that feed this operator and look for the finding that explains the low estimate (a wrapped column, a table variable, a parameter value the plan was not compiled for).",
        "action for a tempdb spill caused by a low estimate",
    ),
    (
        "spill.tempdb.summary",
        "{operator} ran out of memory and spilled to tempdb.",
        "summary: SQL Server sort, hash or exchange spilled to tempdb",
    ),
    (
        "spill.tempdb.why",
        "The memory granted to the query did not hold this operator's rows, so the rest was written to tempdb and read back, which is far slower than working in memory.",
        "why a tempdb spill costs time",
    ),
    (
        "spill.tempdb.action",
        "Reduce what the operator has to hold: return fewer or narrower columns through it, or filter earlier. If the row estimate here is close to the actual count, the server may simply be short of query memory; check for memory grant waits.",
        "action for a tempdb spill with a correct estimate",
    ),
    (
        "spill.pg_sort.summary",
        "{operator} did not fit in work_mem and sorted {disk} on disk ({sort_method}).",
        "summary: PostgreSQL sort spilled to disk",
    ),
    (
        "spill.pg_sort.why",
        "PostgreSQL sorts in memory up to work_mem and switches to temporary files beyond it. Sorting on disk is several times slower than sorting the same rows in memory.",
        "why a PostgreSQL disk sort costs time",
    ),
    (
        "spill.pg_sort.action",
        "Raise work_mem for this session to about {work_mem} and run the query again. An in-memory sort needs more memory than the on-disk size, so this is a starting point. Every sort and hash in every parallel worker may use this much, so set it per session or per role, not server-wide.",
        "action: raise work_mem for a disk sort; {work_mem} is a size such as 64MB",
    ),
    (
        "spill.pg_sort_large",
        "This sort is too large for a memory setting to be the answer. Sort fewer rows or narrower rows (filter earlier, select fewer columns), or give the query an index that returns the rows in this order so no sort is needed.",
        "action for a very large PostgreSQL disk sort",
    ),
    (
        "spill.pg_hash.summary",
        "The hash table for this join did not fit in memory and was split into {batches} batches.",
        "summary: PostgreSQL hash join used more than one batch",
    ),
    (
        "spill.pg_hash.why",
        "With more than one batch, PostgreSQL writes both inputs of the join to temporary files and joins them one batch at a time. The rows are written and read once more than an in-memory hash join needs.",
        "why a multi-batch hash join costs time",
    ),
    (
        "spill.pg_hash.action",
        "Raise work_mem for this session to about {work_mem} so the hash table fits in one batch (on PostgreSQL 13 and later, hash_mem_multiplier scales the limit for hashes only). Every sort and hash in every parallel worker may use this much, so set it per session or per role, not server-wide.",
        "action: raise work_mem for a multi-batch hash; {work_mem} is a size such as 64MB",
    ),
    (
        "spill.pg_hash_text",
        "Raise work_mem for this session so the hash table fits in one batch (on PostgreSQL 13 and later, hash_mem_multiplier scales the limit for hashes only), or reduce the rows and columns on the hashed side of the join.",
        "action for a multi-batch hash when no memory figure is known",
    ),
    (
        "spill.pg_disk.summary",
        "{operator} did not fit in memory and wrote {disk} to disk.",
        "summary: PostgreSQL hash aggregate or other operator wrote to disk",
    ),
    (
        "spill.pg_disk.why",
        "PostgreSQL keeps this operator's working set in memory up to work_mem (times hash_mem_multiplier for hashes) and writes the rest to temporary files, which are then read back.",
        "why a PostgreSQL operator writing to disk costs time",
    ),
    (
        "spill.pg_disk.action",
        "Raise work_mem for this session to about {work_mem} and run the query again. Every sort and hash in every parallel worker may use this much, so set it per session or per role, not server-wide.",
        "action: raise work_mem for an operator that wrote to disk",
    ),
    (
        "spill.oracle_temp.summary",
        "{operator} did not fit in its work area and used {disk} of temporary space.",
        "summary: Oracle operation used temp space",
    ),
    (
        "spill.oracle_temp.why",
        "Oracle sizes sort and hash work areas from the PGA target. An operation that does not fit runs in one or more passes over the temporary tablespace, writing its rows out and reading them back.",
        "why an Oracle operation using temp space costs time",
    ),
    (
        "spill.oracle_temp.action",
        "Check how much PGA the instance may use for work areas. If the target is small for the workload, raise PGA_AGGREGATE_TARGET; otherwise reduce the rows and columns this operation handles.",
        "action for an Oracle operation that used temp space",
    ),
    (
        "spill.generic.summary",
        "{operator} did not fit in memory and wrote its work to disk.",
        "summary: an operator spilled to disk on an engine without specific advice",
    ),
    (
        "spill.generic.why",
        "Rows that do not fit in the operator's memory are written to temporary storage and read back, which is slower than working in memory.",
        "why a spill costs time",
    ),
    (
        "spill.generic.action",
        "Reduce the rows and columns this operator handles, or give the query more working memory.",
        "action for a spill on an engine without specific advice",
    ),
    // ---- memory_grant
    (
        "memory_grant.excessive.summary",
        "The query was granted {granted} of memory and used {used} ({used_percent} of the grant).",
        "summary: SQL Server memory grant far larger than what was used",
    ),
    (
        "memory_grant.excessive.why",
        "Memory granted to one query is unavailable to every other query until it finishes. With several such queries running, others wait for their grants (RESOURCE_SEMAPHORE waits) even though most of the reserved memory sits unused.",
        "why an oversized memory grant costs",
    ),
    (
        "memory_grant.excessive.action",
        "The grant is sized from the estimated rows and row width of the plan's sorts and hashes. Look for an operator whose estimate is far above its actual rows, update the statistics behind it, and avoid carrying wide columns through sorts.",
        "action for an oversized memory grant",
    ),
    (
        "memory_grant.alt_cap",
        "As a stopgap, cap the grant for this statement with a query hint. Too low a cap makes the sorts and hashes spill to tempdb.",
        "alternative: MAX_GRANT_PERCENT hint",
    ),
    (
        "memory_grant.wait.summary",
        "The query waited {wait} for its memory grant of {granted} before it started.",
        "summary: SQL Server query waited for a memory grant",
    ),
    (
        "memory_grant.wait.why",
        "The server did not have enough free query memory when this statement arrived, so it queued (a RESOURCE_SEMAPHORE wait). That wait is added to the run time without showing up in any operator.",
        "why a memory grant wait costs time",
    ),
    (
        "memory_grant.wait.action",
        "Find which queries hold large grants while this one waits, with the statement below, and reduce those grants first. If this statement's own grant is larger than it uses, reduce it the same way.",
        "action for a memory grant wait",
    ),
    // ---- temporary_table
    (
        "temporary_table.group.summary",
        "The plan groups or de-duplicates {rows} rows through an internal temporary table.",
        "summary: MySQL or MariaDB temporary table for GROUP BY or DISTINCT",
    ),
    (
        "temporary_table.group.why",
        "Every input row is looked up in an internal temporary table that holds one row per group, and no group is returned until all rows are read. A temporary table that outgrows tmp_table_size (or max_heap_table_size) is converted to an on-disk table, which is much slower.",
        "why a temporary table for grouping costs time",
    ),
    (
        "temporary_table.group.action",
        "Create an index on {table} that starts with the grouping columns ({index_columns}), so the rows arrive already grouped and no temporary table is needed.",
        "action: index the GROUP BY columns",
    ),
    (
        "temporary_table.text_only",
        "If the grouping columns come from one table, an index that starts with them lets the rows arrive already grouped. Otherwise reduce the rows before grouping, or check tmp_table_size and max_heap_table_size so the temporary table stays in memory.",
        "action for a grouping temporary table when no index can be named",
    ),
    (
        "temporary_table.sqlite.summary",
        "SQLite builds a temporary b-tree to group or de-duplicate the rows read from {table}.",
        "summary: SQLite USE TEMP B-TREE FOR GROUP BY or DISTINCT",
    ),
    (
        "temporary_table.sqlite.why",
        "Every row is inserted into a temporary index before the first result row is returned. On a large table that is extra work an index in the right order would avoid.",
        "why a SQLite temporary b-tree costs time",
    ),
    (
        "temporary_table.sqlite.action",
        "If {table} is large, create an index that starts with the grouped columns so SQLite reads the rows in order. The plan does not show row counts, so check the table size first.",
        "action for a SQLite temporary b-tree",
    ),
];

pub(super) fn run(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    for position in 0..ctx.len() {
        if !ctx.executed(position) {
            continue;
        }
        spill(ctx, position, drafts);
        temporary_table(ctx, position, drafts);
    }
    memory_grant(ctx, drafts);
}

/// A work_mem value about twice `kb`, as a power of two in MB, or `None`
/// when that is more than a session setting should be.
fn work_mem_for(kb: f64) -> Option<String> {
    if !kb.is_finite() || kb <= 0.0 {
        return None;
    }
    let megabytes = ((kb * 2.0) / 1_024.0).ceil().max(8.0) as u64;
    let rounded = megabytes.next_power_of_two();
    (rounded <= 2_048).then(|| {
        if rounded >= 1_024 {
            format!("{}GB", rounded / 1_024)
        } else {
            format!("{rounded}MB")
        }
    })
}

fn spill(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let node = ctx.node(position);
    let warned = node
        .warnings
        .iter()
        .any(|warning| matches!(warning, PlanWarning::SpillToTempDb));
    let Some(spill) = node.spill.as_ref().or(warned.then_some(&EMPTY_SPILL)) else {
        return;
    };
    let metrics = ctx.m(position);
    let rows = metrics.rows_in.or(metrics.rows_out).unwrap_or(0.0);
    let own = ctx.self_share(position);
    let operator = ctx.title(position);
    let written = spill.written_kb.filter(|kb| *kb > 0.0);
    let mut args: Args = vec![("operator", operator)];
    let big = rows >= 10_000.0 || own.is_some_and(|share| share >= 0.05);
    let draft = match ctx.dialect {
        PlanDialect::SqlServer => {
            let severity = if big { S::Warning } else { S::Info };
            // The grant is sized from the rows the operator expects to hold.
            let input = ctx.kids(position).first().copied().unwrap_or(position);
            let estimated = ctx
                .m(input)
                .estimated_rows_total
                .or(ctx.m(input).estimated_rows);
            let actual = ctx.m(input).rows_out;
            let mut draft = match (estimated, actual) {
                (Some(estimated), Some(actual)) if actual >= estimated.max(1.0) * 2.0 => {
                    args.push(("estimated_rows", count(estimated)));
                    args.push(("actual_rows", count(actual)));
                    Draft::new(K::Spill, severity, C::High, "tempdb_underestimate", args)
                        .also(input)
                        .ev("estimated_rows", count(estimated))
                        .ev("actual_rows", count(actual))
                }
                _ => Draft::new(K::Spill, severity, C::High, "tempdb", args),
            };
            let mut detail: Vec<String> = Vec::new();
            if let Some(level) = spill.level {
                detail.push(format!("spill level {level}"));
            }
            if let Some(threads) = spill.threads.filter(|threads| *threads > 0) {
                detail.push(format!("{threads} threads spilled"));
            }
            if !detail.is_empty() {
                draft = draft.ev("warning", format!("SpillToTempDb, {}", detail.join(", ")));
            } else {
                draft = draft.ev("warning", "SpillToTempDb");
            }
            draft.ev_opt("disk", written.map(mem_kb))
        }
        PlanDialect::Postgres => {
            if ctx.op(position) == Op::Sort && node.sort_space_type.as_deref() == Some("Disk") {
                let Some(disk) = written.or(node.sort_space_kb) else {
                    return;
                };
                // A sort of a megabyte is not worth a setting change.
                if disk < 1_024.0 || !(big || disk >= 8_192.0) {
                    return;
                }
                let severity = if own.is_some_and(|share| share >= 0.1) || disk >= 102_400.0 {
                    S::Warning
                } else {
                    S::Info
                };
                let method = node
                    .sort_method
                    .clone()
                    .unwrap_or_else(|| "external merge".to_string());
                args.push(("disk", mem_kb(disk)));
                args.push(("sort_method", method.clone()));
                let work_mem = work_mem_for(disk);
                args.push(("work_mem", work_mem.clone().unwrap_or_default()));
                let draft = Draft::new(K::Spill, severity, C::High, "pg_sort", args);
                let draft = match work_mem {
                    Some(value) => draft.sql(format!("SET work_mem = '{value}';")),
                    None => draft.action("pg_sort_large"),
                };
                draft
                    .ev("sort_method", method)
                    .ev("disk", mem_kb(disk))
                    .ev_opt(
                        "sort_keys",
                        (!node.sort_keys.is_empty()).then(|| node.sort_keys.join(", ")),
                    )
            } else if let Some(batches) = node.hash_batches.filter(|batches| *batches > 1) {
                if rows < 10_000.0 {
                    return;
                }
                let severity = if own.is_some_and(|share| share >= 0.1) || rows >= 1_000_000.0 {
                    S::Warning
                } else {
                    S::Info
                };
                args.push(("batches", batches.to_string()));
                // All batches in memory at once need about peak x batches.
                let needed = node
                    .peak_memory_kb
                    .filter(|kb| *kb > 0.0)
                    .map(|kb| kb * f64::from(batches) / 2.0)
                    .and_then(work_mem_for);
                args.push(("work_mem", needed.clone().unwrap_or_default()));
                let draft = Draft::new(K::Spill, severity, C::High, "pg_hash", args);
                let draft = match needed {
                    Some(value) => draft.sql(format!("SET work_mem = '{value}';")),
                    None => draft.action("pg_hash_text"),
                };
                let mut batches_text = batches.to_string();
                if let Some(original) = node.original_hash_batches.filter(|o| *o != batches) {
                    batches_text.push_str(&format!(" (planned {original})"));
                }
                draft
                    .ev("hash_batches", batches_text)
                    .ev_opt("memory", node.peak_memory_kb.map(mem_kb))
                    .ev_opt("disk", written.map(mem_kb))
                    .ev_opt("rows_read", (rows > 0.0).then(|| count(rows)))
            } else {
                let Some(disk) = written else {
                    return;
                };
                if disk < 1_024.0 || !(big || disk >= 8_192.0) {
                    return;
                }
                let Some(work_mem) = work_mem_for(disk) else {
                    return;
                };
                let severity = if own.is_some_and(|share| share >= 0.1) {
                    S::Warning
                } else {
                    S::Info
                };
                args.push(("disk", mem_kb(disk)));
                args.push(("work_mem", work_mem.clone()));
                Draft::new(K::Spill, severity, C::High, "pg_disk", args)
                    .sql(format!("SET work_mem = '{work_mem}';"))
                    .ev("disk", mem_kb(disk))
                    .ev_opt("memory", node.peak_memory_kb.map(mem_kb))
            }
        }
        PlanDialect::Oracle => {
            let Some(disk) = written else {
                return;
            };
            if disk < 1_024.0 {
                return;
            }
            let severity = if big { S::Warning } else { S::Info };
            args.push(("disk", mem_kb(disk)));
            Draft::new(K::Spill, severity, C::High, "oracle_temp", args)
                .sql(
                    "SELECT name, value FROM v$parameter\nWHERE name IN ('pga_aggregate_target', 'workarea_size_policy');",
                )
                .ev("disk", mem_kb(disk))
                .ev_opt("memory", node.peak_memory_kb.map(mem_kb))
        }
        _ => {
            if !big {
                return;
            }
            Draft::new(K::Spill, S::Info, C::High, "generic", args)
                .ev_opt("disk", written.map(mem_kb))
        }
    };
    drafts.push(
        draft
            .at(position)
            .own_share()
            .ev_opt("own_time", metrics.self_time_ms.map(ms)),
    );
}

static EMPTY_SPILL: crate::plans::model::PlanSpill = crate::plans::model::PlanSpill {
    kind: String::new(),
    level: None,
    threads: None,
    written_kb: None,
};

/// `(granted, used)` from the engine's "Excessive Grant" warning text.
fn excessive_grant_warning(ctx: &Ctx<'_>) -> Option<(f64, f64)> {
    let warnings = ctx
        .plan
        .warnings
        .iter()
        .chain(ctx.plan.nodes.iter().flat_map(|node| node.warnings.iter()));
    for warning in warnings {
        let PlanWarning::Other(text) = warning else {
            continue;
        };
        if !text.to_ascii_lowercase().starts_with("excessive grant") {
            continue;
        }
        let number_after = |label: &str| -> Option<f64> {
            let lower = text.to_ascii_lowercase();
            let start = lower.find(label)? + label.len();
            text[start..]
                .trim_start()
                .split(|c: char| !(c.is_ascii_digit() || c == '.'))
                .next()?
                .parse::<f64>()
                .ok()
        };
        if let (Some(granted), Some(used)) = (number_after("granted"), number_after("used")) {
            return Some((granted, used));
        }
    }
    None
}

fn memory_grant(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::SqlServer {
        return;
    }
    let grant = ctx.plan.memory_grant.as_ref();
    let flagged = excessive_grant_warning(ctx);
    let granted = grant
        .and_then(|grant| grant.granted_kb)
        .or(flagged.map(|f| f.0));
    let used = grant
        .and_then(|grant| grant.max_used_kb)
        .or(flagged.map(|f| f.1));
    if let (Some(granted), Some(used)) = (granted, used)
        && ctx.has_actuals()
        && granted > 0.0
    {
        let large = granted >= 102_400.0 && used < granted * 0.1;
        // Below 100 MB only the engine's own warning is worth repeating.
        if large || (flagged.is_some() && used < granted * 0.5) {
            let severity = if large { S::Warning } else { S::Info };
            let args: Args = vec![
                ("granted", mem_kb(granted)),
                ("used", mem_kb(used)),
                ("used_percent", pct(used / granted)),
            ];
            let mut draft = Draft::new(K::MemoryGrant, severity, C::High, "excessive", args);
            // Twice what was used, as a percent of the memory one query may
            // be granted.
            let cap = grant
                .and_then(|grant| grant.max_query_memory_kb)
                .filter(|max| *max > 0.0)
                .map(|max| ((used * 2.0 / max) * 100.0).ceil().clamp(1.0, 100.0));
            draft = draft.alt(
                "alt_cap",
                cap.filter(|cap| *cap < 25.0)
                    .map(|cap| format!("OPTION (MAX_GRANT_PERCENT = {cap:.0})")),
            );
            let mut detail = format!("granted {}, used {}", mem_kb(granted), mem_kb(used));
            if let Some(requested) = grant.and_then(|grant| grant.requested_kb) {
                detail = format!("requested {}, {detail}", mem_kb(requested));
            }
            draft = draft.ev("grant", detail);
            if flagged.is_some() {
                draft = draft.ev("warning", "MemoryGrantWarning: Excessive Grant");
            }
            // The operator that asked for the memory: the sort or hash whose
            // estimate is furthest above its actual rows.
            let culprit = (0..ctx.len())
                .filter(|position| {
                    matches!(
                        ctx.op(*position),
                        Op::Sort | Op::HashJoin | Op::Aggregate | Op::Distinct
                    ) && ctx.executed(*position)
                })
                .filter_map(|position| {
                    let input = ctx.kids(position).first().copied().unwrap_or(position);
                    let estimated = ctx.m(input).estimated_rows_total?;
                    let actual = ctx.m(input).rows_out?;
                    (estimated >= actual.max(1.0) * 10.0)
                        .then_some((position, estimated / actual.max(1.0)))
                })
                .max_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((position, _)) = culprit {
                draft = draft.at(position);
            }
            // Wasted memory costs the server, not this statement's run time.
            drafts.push(draft.no_impact());
        }
    }
    if let Some(grant) = grant
        && let Some(wait) = grant.grant_wait_seconds.filter(|wait| *wait >= 1.0)
    {
        let granted = grant.granted_kb.unwrap_or(0.0);
        let args: Args = vec![("wait", ms(wait * 1_000.0)), ("granted", mem_kb(granted))];
        let total = ctx.an.execution_time_ms.or(ctx.an.total_time_ms);
        let severity = if total.is_none_or(|total| wait * 1_000.0 >= total * 0.1) {
            S::Warning
        } else {
            S::Info
        };
        drafts.push(
            Draft::new(K::MemoryGrant, severity, C::High, "wait", args)
                .sql(
                    "SELECT session_id, requested_memory_kb, granted_memory_kb, used_memory_kb, wait_time_ms\nFROM sys.dm_exec_query_memory_grants\nORDER BY granted_memory_kb DESC;",
                )
                .ev(
                    "grant",
                    format!("waited {}, granted {}", ms(wait * 1_000.0), mem_kb(granted)),
                ),
        );
    }
}

/// The single table read below `position`, when there is exactly one.
fn only_table<'a>(ctx: &Ctx<'a>, position: usize) -> Option<(usize, Rel<'a>)> {
    let mut below = ctx.descendants(position);
    below.push(position);
    only_table_among(ctx, below)
}

/// The single table read by the nodes in `scope`, when there is exactly one.
fn only_table_among<'a>(ctx: &Ctx<'a>, scope: Vec<usize>) -> Option<(usize, Rel<'a>)> {
    let mut found: Option<(usize, Rel<'a>)> = None;
    for at in scope {
        if ctx.node(at).relation.is_none() {
            continue;
        }
        let Some(rel) = ctx.rel(at) else {
            // A CTE, derived table or function: not a single base table.
            if ctx.op(at).is_read() || matches!(ctx.op(at), Op::CteScan | Op::SubqueryScan) {
                return None;
            }
            continue;
        };
        if found.is_some() {
            return None;
        }
        found = Some((at, rel));
    }
    found
}

fn temporary_table(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let node = ctx.node(position);
    let name = ctx.op_name(position);
    match ctx.dialect {
        PlanDialect::MySql | PlanDialect::MariaDb => {
            let flagged = node
                .extra
                .iter()
                .any(|extra| extra.eq_ignore_ascii_case("Using temporary"));
            let named = !name.starts_with("TABLE SCAN ON")
                && (name.contains("USING TEMPORARY TABLE")
                    || name.starts_with("TEMPORARY TABLE WITH DEDUPLICATION"));
            if !(flagged || named) {
                return;
            }
            let metrics = ctx.m(position);
            let measured = metrics.rows_in.or_else(|| {
                let kids = ctx.kids(position);
                (!kids.is_empty())
                    .then(|| {
                        kids.iter()
                            .map(|kid| ctx.m(*kid).rows_out)
                            .sum::<Option<f64>>()
                    })
                    .flatten()
            });
            let (rows, severity, confidence) = match measured {
                Some(rows) => {
                    if rows < 10_000.0 || ctx.self_share(position).is_some_and(|share| share < 0.05)
                    {
                        return;
                    }
                    (rows, S::Warning, C::High)
                }
                None => {
                    if ctx.has_actuals() {
                        return;
                    }
                    let estimated = ctx
                        .kids(position)
                        .iter()
                        .filter_map(|kid| {
                            ctx.m(*kid)
                                .estimated_rows_total
                                .or(ctx.m(*kid).estimated_rows)
                        })
                        .fold(0.0, f64::max);
                    if estimated < 100_000.0 {
                        return;
                    }
                    (estimated, S::Info, C::Low)
                }
            };
            let mut args: Args = vec![("rows", count(rows)), ("index_columns", String::new())];
            let group_keys = &ctx.a(position).group_keys;
            // Name an index only when the grouping columns are plain columns
            // of the one table the statement reads.
            let suggestion = only_table(ctx, position).and_then(|(read, rel)| {
                if group_keys.is_empty() {
                    return None;
                }
                let mut suggestion = IndexSuggestion::for_rel(&rel);
                for text in [
                    ctx.scan(read).filter_raw,
                    ctx.a(read).seek_predicate.as_ref().map(|p| p.raw.clone()),
                ]
                .into_iter()
                .flatten()
                {
                    for term in ctx.terms(&text) {
                        if term.kind == TermKind::Equality
                            && let Some(column) = term.bare_column()
                            && rel.owns(column)
                        {
                            suggestion.equality.push(column.name.clone());
                        }
                    }
                }
                for key in group_keys {
                    let parts = crate::plans::schema::split_qualified(key.trim());
                    let column = parts.last()?.clone();
                    let plain = !column.is_empty()
                        && column
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '_' || c == '$');
                    let owned = match parts.len() {
                        1 => true,
                        2 => {
                            parts[0].eq_ignore_ascii_case(&rel.name)
                                || rel
                                    .alias
                                    .as_deref()
                                    .is_some_and(|alias| alias.eq_ignore_ascii_case(&parts[0]))
                        }
                        _ => false,
                    };
                    if !plain || !owned {
                        return None;
                    }
                    suggestion.sort.push((column, false));
                }
                Some((rel.display(), suggestion))
            });
            let mut draft = match suggestion {
                Some((table, suggestion)) => {
                    args.push(("table", table));
                    Draft::new(K::TemporaryTable, severity, confidence, "group", args)
                        .index(suggestion, "text_only")
                }
                None => {
                    args.push(("table", String::new()));
                    Draft::new(K::TemporaryTable, severity, confidence, "group", args)
                        .action("text_only")
                }
            };
            if measured.is_none() {
                draft = draft
                    .ev("estimated_rows", count(rows))
                    .note("common.note.estimated_only", &[]);
            } else {
                draft = draft.ev("rows_read", count(rows));
            }
            drafts.push(
                draft
                    .at(position)
                    .own_share()
                    .ev_opt(
                        "columns",
                        (!group_keys.is_empty()).then(|| group_keys.join(", ")),
                    )
                    .ev_opt("time", ctx.m(position).inclusive_time_ms.map(ms)),
            );
        }
        PlanDialect::Sqlite => {
            if !(name.starts_with("USE TEMP B-TREE FOR GROUP BY")
                || name.starts_with("USE TEMP B-TREE FOR DISTINCT"))
            {
                return;
            }
            // SQLite prints the temp b-tree as a sibling of the reads: under
            // the same parent, or at the top level of the statement.
            let found = match ctx.up(position) {
                Some(parent) => only_table(ctx, parent),
                None => only_table_among(ctx, (0..ctx.len()).collect()),
            };
            let Some((read, rel)) = found else {
                return;
            };
            // A grouped index search is already narrow.
            if ctx.op(read) != Op::FullScan {
                return;
            }
            let args: Args = vec![("table", rel.display())];
            drafts.push(
                Draft::new(K::TemporaryTable, S::Info, C::Low, "sqlite", args)
                    .at(position)
                    .also(read)
                    .ev("table", rel.display()),
            );
        }
        _ => {}
    }
}
