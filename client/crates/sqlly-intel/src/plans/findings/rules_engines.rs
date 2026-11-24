//! Rules for the engines whose plans carry their own evidence: CockroachDB,
//! Trino and Spark (which have no PostgreSQL-style operator classification),
//! and DuckDB's filtered scans.
//!
//! Each rule fires only on something the plan states outright: a
//! `spans: FULL SCAN` line, an `Input` and `Filtered` figure, an empty
//! `PushedFilters` list, an estimated next to an actual cardinality. None of
//! them borrows advice written for another engine.

use super::facts::{Ctx, Op, count, ms, pct, ratio};
use super::strings::{Args, Template};
use super::{Draft, FindingConfidence as C, FindingKind as K, FindingSeverity as S};
use crate::plans::PlanDialect;
use crate::plans::analysis::engine_text;
use crate::plans::import::PlanEngine;
use crate::plans::warning::PlanWarning;

pub(super) const TEMPLATES: &[Template] = &[
    // ---- table_scan (CockroachDB)
    (
        "table_scan.cockroach_filtered.summary",
        "The scan of {table} reads all {rows_read} rows (spans: FULL SCAN) and the filter keeps {rows_kept} ({kept_percent}).",
        "summary: CockroachDB full scan under a filter that discards most rows, measured",
    ),
    (
        "table_scan.cockroach_filtered.why",
        "A full scan reads every row of the table from the key-value layer, so the work grows with the table and not with the rows the query returns.",
        "why a CockroachDB full scan under a selective filter costs, in the finding with measured rows",
    ),
    (
        "table_scan.cockroach_filtered.action",
        "Create an index whose leading columns are the ones the filter compares, and list the other columns the query reads in its STORING clause so the index covers the query. EXPLAIN prints an index recommendation under the plan when CockroachDB can work one out.",
        "action for a CockroachDB full scan under a selective filter, in the finding with measured rows",
    ),
    (
        "table_scan.cockroach_estimated.summary",
        "CockroachDB expects to read all {rows_read} rows of {table} (spans: FULL SCAN) and keep about {rows_kept}.",
        "summary: CockroachDB full scan under a filter that discards most rows, from estimates",
    ),
    (
        "table_scan.cockroach_estimated.why",
        "A full scan reads every row of the table from the key-value layer, so the work grows with the table and not with the rows the query returns.",
        "why a CockroachDB full scan under a selective filter costs, in the finding from estimated row counts",
    ),
    (
        "table_scan.cockroach_estimated.action",
        "Create an index whose leading columns are the ones the filter compares, and list the other columns the query reads in its STORING clause so the index covers the query. EXPLAIN prints an index recommendation under the plan when CockroachDB can work one out.",
        "action for a CockroachDB full scan under a selective filter, in the finding from estimated row counts",
    ),
    // ---- filter_discards_rows (Trino)
    (
        "filter_discards_rows.trino_scan.summary",
        "{operator} received {rows_read} rows from the connector and its filter kept {rows_kept} ({kept_percent}).",
        "summary: Trino ScanFilter or ScanFilterProject whose filter removed most of the rows the connector returned",
    ),
    (
        "filter_discards_rows.trino_scan.why",
        "The filter ran inside Trino after the connector had produced the rows, so all {rows_read} rows were read and decoded to find the ones that match.",
        "why a Trino filter over connector rows costs",
    ),
    (
        "filter_discards_rows.trino_scan.action",
        "Check whether the connector can apply this condition itself: through a partition or bucketing column for Hive, Iceberg and Delta Lake tables, or as a predicate the source database evaluates for JDBC connectors. Compare the column directly, with no function or cast around it, so the condition can be pushed down.",
        "action for a Trino filter over connector rows",
    ),
    // ---- non_sargable (Spark)
    (
        "non_sargable.spark_not_pushed.summary",
        "The scan of {table} filters on {filter}, and none of it is pushed to the {format} reader.",
        "summary: Spark file scan with DataFilters but no matching PushedFilters; {format} is Parquet or ORC",
    ),
    (
        "non_sargable.spark_not_pushed.why",
        "Spark can skip row groups using the file's minimum and maximum statistics only for conditions it pushes to the reader. This condition is evaluated after the rows are read, so every row group is read.",
        "why a Spark filter that is not pushed down costs",
    ),
    (
        "non_sargable.spark_not_pushed.action",
        "Compare the column itself with a literal of the column's type. A function or cast around the column, or a literal of another type that makes Spark cast the column, keeps the condition out of PushedFilters.",
        "action for a Spark filter that is not pushed down",
    ),
    // ---- estimate_mismatch (DuckDB)
    (
        "estimate_mismatch.duckdb_scan.summary",
        "DuckDB estimated {estimated_rows} rows from the filtered scan of {table} and {actual_rows} came out, off by a factor of {factor}.",
        "summary: DuckDB filtered table scan whose estimated cardinality is far from the actual one",
    ),
    (
        "estimate_mismatch.duckdb_scan.why",
        "DuckDB chooses the join order, and which input of each hash join to build the hash table from, by these estimates. An estimate this far off can put the larger input on the build side.",
        "why a wrong DuckDB scan estimate matters",
    ),
    (
        "estimate_mismatch.duckdb_scan.action",
        "Check the joins above this scan: the build side of a hash join, its right-hand input in DuckDB's own plan output, should be the smaller one. If it is not, write the filtered rows to a temporary table first and join that, so the real row count is known when the join is planned.",
        "action for a wrong DuckDB scan estimate",
    ),
];

pub(super) fn run(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    for position in 0..ctx.len() {
        if !ctx.executed(position) {
            continue;
        }
        match ctx.engine {
            PlanEngine::CockroachDb => cockroach_full_scan(ctx, position, drafts),
            PlanEngine::Trino => trino_scan_filter(ctx, position, drafts),
            PlanEngine::Spark => {
                spark_cross_join(ctx, position, drafts);
                spark_filter_not_pushed(ctx, position, drafts);
            }
            PlanEngine::DuckDb => duckdb_scan_estimate(ctx, position, drafts),
            _ => {}
        }
    }
}

fn kept_percent(read: f64, kept: f64) -> String {
    if read <= 0.0 {
        return "0%".to_string();
    }
    let percent = kept / read * 100.0;
    if percent <= 0.0 {
        "0%".to_string()
    } else if percent < 0.01 {
        crate::plans::phrases::styled("<0.01%".to_string())
    } else if percent < 1.0 {
        crate::plans::phrases::styled(format!("{percent:.2}%"))
    } else {
        format!("{percent:.0}%")
    }
}

/// A CockroachDB `scan` with `spans: FULL SCAN` whose rows a filter then
/// mostly throws away.
fn cockroach_full_scan(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let node = ctx.node(position);
    if !node
        .property("spans")
        .is_some_and(|spans| spans.starts_with("FULL SCAN"))
    {
        return;
    }
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    // The recommendation CockroachDB printed is the better finding.
    let recommended = ctx.plan.warnings.iter().any(|warning| {
        matches!(warning, PlanWarning::MissingIndex(missing)
            if missing.table.eq_ignore_ascii_case(&rel.name))
    });
    if recommended {
        return;
    }
    // The filter is on the scan itself or on a `filter` operator directly
    // above it.
    let own = ctx.a(position).filter.as_ref().map(|f| f.text.clone());
    let above = ctx.up(position).filter(|parent| {
        ctx.kids(*parent).len() == 1
            && ctx.op_name(*parent).starts_with("FILTER")
            && ctx.a(*parent).filter.is_some()
    });
    let (top, filter) = match (own, above) {
        (Some(filter), _) => (position, filter),
        (None, Some(parent)) => {
            let Some(filter) = ctx.a(parent).filter.as_ref().map(|f| f.text.clone()) else {
                return;
            };
            (parent, filter)
        }
        (None, None) => return,
    };
    let measured = if top == position {
        node.rows_read.zip(ctx.m(position).rows_out)
    } else {
        ctx.m(position).rows_out.zip(ctx.m(top).rows_out)
    };
    let estimated = if top == position {
        None
    } else {
        ctx.m(position)
            .estimated_rows
            .zip(ctx.m(top).estimated_rows)
    };
    let (variant, read, kept, confidence) = match (measured, estimated) {
        (Some((read, kept)), _) => ("cockroach_filtered", read, kept, C::High),
        (None, Some((read, kept))) => ("cockroach_estimated", read, kept, C::Medium),
        (None, None) => return,
    };
    if read < 1_000.0 || kept > read * 0.1 {
        return;
    }
    let severity =
        if variant == "cockroach_filtered" && ctx.share(top).is_some_and(|share| share >= 0.3) {
            S::Warning
        } else {
            S::Info
        };
    let args: Args = vec![
        ("table", rel.display()),
        ("rows_read", count(read)),
        ("rows_kept", count(kept)),
        ("kept_percent", kept_percent(read, kept)),
    ];
    let mut draft = Draft::new(K::TableScan, severity, confidence, variant, args)
        .at(top)
        .also(position)
        .ev("table", rel.display())
        .ev_opt("index", ctx.a(position).index.clone())
        .ev("filter", filter);
    if variant == "cockroach_filtered" {
        draft = draft
            .ev("rows_read", count(read))
            .ev("rows_returned", count(kept));
        if ctx.timed {
            draft = draft.ev_opt("time", ctx.m(top).inclusive_time_ms.map(ms));
        }
    } else {
        draft = draft
            .ev("estimated_rows", count(read))
            .note("common.note.estimated_only", &[]);
    }
    drafts.push(draft);
}

/// A Trino `ScanFilter` / `ScanFilterProject` whose `Input` is large and
/// whose filter removed nearly all of it.
fn trino_scan_filter(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if !ctx.op_name(position).starts_with("SCANFILTER") {
        return;
    }
    let node = ctx.node(position);
    let Some(filter) = ctx.a(position).filter.as_ref().map(|f| f.text.clone()) else {
        return;
    };
    let (Some(read), Some(removed)) = (node.rows_read, node.rows_removed_by_filter) else {
        return;
    };
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    if read < 10_000.0 || removed < read * 0.9 {
        return;
    }
    let kept = (read - removed).max(0.0);
    let severity = if ctx.share(position).is_some_and(|share| share >= 0.3) {
        S::Warning
    } else {
        S::Info
    };
    let args: Args = vec![
        ("operator", ctx.title(position)),
        ("rows_read", count(read)),
        ("rows_kept", count(kept)),
        ("kept_percent", kept_percent(read, kept)),
    ];
    let mut draft = Draft::new(K::FilterDiscardsRows, severity, C::High, "trino_scan", args)
        .at(position)
        .ev("table", rel.display())
        .ev("filter", filter)
        .ev("rows_read", count(read))
        .ev("rows_returned", count(kept))
        .ev("rows_removed", count(removed));
    if ctx.timed {
        draft = draft.ev_opt("time", ctx.m(position).inclusive_time_ms.map(ms));
    } else {
        draft = draft.ev_opt("cost_share", ctx.share(position).map(pct));
    }
    drafts.push(draft);
}

/// Spark's `CartesianProduct`, or a `BroadcastNestedLoopJoin` with no
/// condition: every row of one input is paired with every row of the other.
fn spark_cross_join(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let name = ctx.op_name(position);
    if !(name.starts_with("CARTESIANPRODUCT") || name.starts_with("BROADCASTNESTEDLOOPJOIN")) {
        return;
    }
    let node = ctx.node(position);
    let has_condition = ctx.a(position).join_predicate.is_some()
        || node.filter.as_deref().is_some_and(|f| !f.trim().is_empty());
    // An outer, semi or anti nested loop join always carries a condition
    // or is an existence test; only an inner or cross pairing is a product.
    let join_type = node.join_type.as_deref().unwrap_or("").to_ascii_lowercase();
    let pairing =
        join_type.is_empty() || join_type.contains("inner") || join_type.contains("cross");
    if has_condition || !pairing || ctx.kids(position).len() != 2 {
        return;
    }
    let args: Args = vec![("operator", ctx.title(position))];
    drafts.push(Draft::new(K::NoJoin, S::Info, C::Medium, "unknown", args).at(position));
}

/// Entries of a Spark bracketed list such as `[isnotnull(id#5L), (id#5L = 42)]`.
fn spark_list(text: &str) -> Vec<String> {
    let inner = text.trim();
    let inner = inner.strip_prefix('[').unwrap_or(inner);
    let inner = inner.strip_suffix(']').unwrap_or(inner);
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();
    for c in inner.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(current.trim().to_string());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    out.push(current.trim().to_string());
    out.retain(|entry| !entry.is_empty());
    out
}

/// A Spark Parquet or ORC scan that filters rows (`DataFilters`) without
/// handing any of the comparison to the reader (`PushedFilters`).
fn spark_filter_not_pushed(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let node = ctx.node(position);
    let (Some(data), Some(pushed)) = (node.property("DataFilters"), node.property("PushedFilters"))
    else {
        return;
    };
    // A truncated list (`...`) proves nothing about what was pushed.
    if data.contains("...") || pushed.contains("...") {
        return;
    }
    let format = match node.property("Format") {
        Some(format) if format.eq_ignore_ascii_case("parquet") => "Parquet",
        Some(format) if format.eq_ignore_ascii_case("orc") => "ORC",
        _ => return,
    };
    let conditions: Vec<String> = spark_list(data)
        .into_iter()
        .filter(|entry| !entry.to_ascii_lowercase().starts_with("isnotnull("))
        .collect();
    let pushed_conditions = spark_list(pushed)
        .into_iter()
        .filter(|entry| !entry.starts_with("IsNotNull("))
        .count();
    if conditions.is_empty() || pushed_conditions > 0 {
        return;
    }
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    let filter = conditions
        .iter()
        .map(|condition| engine_text(PlanEngine::Spark, condition))
        .collect::<Vec<_>>()
        .join(" AND ");
    let args: Args = vec![
        ("table", rel.display()),
        ("filter", filter.clone()),
        ("format", format.to_string()),
    ];
    drafts.push(
        Draft::new(K::NonSargable, S::Info, C::Medium, "spark_not_pushed", args)
            .at(position)
            .no_impact()
            .ev("table", rel.display())
            .ev("filter", filter),
    );
}

/// A DuckDB table scan with a pushed-down filter, below a join, that
/// returned at least ten times the rows DuckDB estimated.
fn duckdb_scan_estimate(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.dialect != PlanDialect::DuckDb || ctx.op(position) != Op::FullScan {
        return;
    }
    let Some(filter) = ctx.a(position).filter.as_ref().map(|f| f.text.clone()) else {
        return;
    };
    let metrics = ctx.m(position);
    let (Some(estimated), Some(actual)) = (
        metrics.estimated_rows.filter(|rows| *rows > 0.0),
        metrics.rows_out,
    ) else {
        return;
    };
    // Only an underestimate is reported: it is the direction that makes
    // DuckDB build a hash table from an input far larger than it planned
    // for. An overestimate costs at most a cautious join order.
    let factor = actual / estimated.max(1.0);
    if factor < 10.0 || actual < 10_000.0 {
        return;
    }
    // The estimate matters where a join is planned from it.
    if !ctx
        .ancestors(position)
        .into_iter()
        .any(|at| ctx.op(at).is_join())
    {
        return;
    }
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    let args: Args = vec![
        ("table", rel.display()),
        ("estimated_rows", count(estimated)),
        ("actual_rows", count(actual)),
        ("factor", ratio(factor)),
    ];
    drafts.push(
        Draft::new(K::EstimateMismatch, S::Info, C::Medium, "duckdb_scan", args)
            .at(position)
            .no_impact()
            .ev("table", rel.display())
            .ev("filter", filter)
            .ev("estimated_rows", count(estimated))
            .ev("actual_rows", count(actual)),
    );
}
