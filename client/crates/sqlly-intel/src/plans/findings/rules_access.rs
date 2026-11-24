//! Rules about how tables and indexes are read: missing indexes, scans that
//! throw most rows away, lookups at volume, sorts an index could provide.

use super::advice::{self, IndexSuggestion};
use super::facts::{Ctx, Op, Rel, Scan, count, ms, pct};
use super::predicate::{Term, TermKind};
use super::strings::Template;
use super::{
    Draft, FindingConfidence as C, FindingImpact, FindingKind as K, FindingSeverity as S,
    ImpactBasis,
};
use crate::plans::PlanDialect;
use crate::plans::glossary::OperatorCategory;
use crate::plans::import::PlanEngine;
use crate::plans::schema::{bare_name, split_qualified};
use crate::plans::warning::PlanWarning;

pub(super) const TEMPLATES: &[Template] = &[
    // ---- missing_index
    (
        "missing_index.optimizer.summary",
        "SQL Server's optimizer asked for an index on {table} starting with {index_columns} while it compiled this statement.",
        "summary: the engine itself reported a missing index",
    ),
    (
        "missing_index.optimizer.why",
        "Without an index that starts with those columns the plan reads far more of {table} than the query needs. The impact figure is the optimizer's own estimate for this one statement; it does not count the cost of maintaining the index.",
        "why a missing index reported by the optimizer costs time",
    ),
    (
        "missing_index.optimizer.action",
        "Create the index, then capture the plan again to confirm the scan became a seek. If an index on {table} already starts with the same columns, widen that one instead of adding another.",
        "action: create the index the optimizer asked for, in the finding without measured row counts",
    ),
    (
        "missing_index.measured.summary",
        "The plan reads {rows_read} rows of {table} to return {rows_kept}, and SQL Server's optimizer asked for an index starting with {index_columns}.",
        "summary: missing index reported by the engine, with measured row counts",
    ),
    (
        "missing_index.measured.why",
        "Every one of those {rows_read} rows is read and tested because no index leads with {index_columns}. With the index the engine can go straight to the matching rows.",
        "why a missing index costs time, with measured row counts",
    ),
    (
        "missing_index.measured.action",
        "Create the index, then capture the plan again to confirm the scan became a seek. If an index on {table} already starts with the same columns, widen that one instead of adding another.",
        "action: create the index the optimizer asked for, in the finding with measured row counts",
    ),
    (
        "missing_index.recommended.summary",
        "CockroachDB recommends an index on {table} ({index_columns}) for this statement.",
        "summary: CockroachDB printed an index recommendation under the plan; {index_columns} is the key column list",
    ),
    (
        "missing_index.recommended.why",
        "The plan reads {table} without an index that fits this statement's filter or join. The recommendation is for this one statement and does not weigh the cost of maintaining the index on writes.",
        "why a CockroachDB index recommendation matters",
    ),
    (
        "missing_index.recommended.action",
        "Create the index, then run EXPLAIN again to confirm the full scan became a constrained scan. If the plan lists the recommendation as an index replacement, it extends an existing index: follow the plan's own statement in that case.",
        "action: create the index CockroachDB recommended",
    ),
    (
        "missing_index.text_only",
        "Create an index on {table} that starts with {index_columns}. The column names could not be matched to the supplied schema, so no statement is written here.",
        "action when the missing-index columns cannot be confirmed against the schema",
    ),
    // ---- table_scan
    (
        "table_scan.filtered.summary",
        "The plan reads all {rows_read} rows of {table} and keeps {rows_kept} ({kept_percent}).",
        "summary: a full table scan whose filter discards most rows",
    ),
    (
        "table_scan.filtered.why",
        "Each row is read and tested against {filter}. The plan uses no index to find the matching rows, so the work grows with the size of the table, not with the size of the result.",
        "why a full scan with a selective filter costs time",
    ),
    (
        "table_scan.filtered.action",
        "Create an index on {table} with {index_columns} as the leading key so the engine can seek to the matching rows.",
        "action: create an index for a filtered scan",
    ),
    (
        "table_scan.no_columns",
        "Look at the filter on {table}: no plain column comparison in it could be matched to an index key. Rewrite it so a column is compared directly with a value, or index the column it tests.",
        "action when the scan's filter has no column an index could use",
    ),
    (
        "table_scan.estimated.summary",
        "The optimizer expects to read about {rows_read} rows of {table} and keep about {rows_kept}.",
        "summary: estimated plan shows a full scan with a selective filter",
    ),
    (
        "table_scan.estimated.why",
        "The whole table is read and each row tested against {filter}. These are the optimizer's estimates, not measurements.",
        "why an estimated full scan with a selective filter costs time",
    ),
    (
        "table_scan.estimated.action",
        "If an executed plan confirms the numbers, create an index on {table} with {index_columns} as the leading key.",
        "action for an estimated full scan",
    ),
    (
        "table_scan.join.summary",
        "The plan reads all {rows_read} rows of {table} for a join that returns {join_rows}.",
        "summary: a full scan feeds a join that matches few rows",
    ),
    (
        "table_scan.join.why",
        "The other side of the join supplies only {other_rows} rows, but the plan uses no index on {join_column}, so the engine reads all of {table} to find their matches.",
        "why a full scan under a selective join costs time",
    ),
    (
        "table_scan.join.action",
        "Create an index on {table} with {index_columns} as the leading key so the join can look up the matching rows.",
        "action: index the join column",
    ),
    (
        "table_scan.join_text",
        "Index the column of {table} used in the join condition {join_condition}.",
        "action when the join column cannot be turned into a statement",
    ),
    // ---- filter_discards_rows
    (
        "filter_discards_rows.index.summary",
        "The read of {table} through {index} fetches {rows_read} rows and a filter then discards {rows_removed} of them.",
        "summary: an index read whose residual filter removes most rows",
    ),
    (
        "filter_discards_rows.index.why",
        "The index narrows the search only as far as its key columns go. {filter} is checked afterwards, one fetched row at a time, so most of the rows read are wasted work.",
        "why a residual filter after an index read costs time",
    ),
    (
        "filter_discards_rows.index.action",
        "Create an index on {table} whose key covers both conditions: {index_columns}.",
        "action: create an index that covers the residual filter",
    ),
    (
        "filter_discards_rows.text_only",
        "Extend the key of {index} (or add an index) so the columns tested in the filter are part of the index key.",
        "action when the filter columns cannot be turned into a statement",
    ),
    // ---- key_lookup
    (
        "key_lookup.volume.summary",
        "After finding rows in {index}, the plan goes back to {table} {lookups} times to fetch columns the index does not hold.",
        "summary: key or RID lookups at volume",
    ),
    (
        "key_lookup.volume.why",
        "Each lookup is a separate search of the table, usually on a different page. At {lookups} lookups this costs more than the index read that found the rows.",
        "why key lookups at volume cost time",
    ),
    (
        "key_lookup.volume.action",
        "Add the fetched columns ({columns}) to {index} so the index covers the query and the lookups disappear.",
        "action: make the index covering",
    ),
    (
        "key_lookup.wide.summary",
        "After finding rows in {index}, the plan goes back to {table} {lookups} times to fetch {column_count} columns.",
        "summary: key lookups that fetch many columns",
    ),
    (
        "key_lookup.wide.why",
        "Each lookup is a separate search of the table. With {column_count} columns fetched, an index that covered them all would be close to a second copy of the table.",
        "why wide key lookups cost time and are hard to cover",
    ),
    (
        "key_lookup.wide.action",
        "Select only the columns the query needs, so a narrow covering index becomes possible. If every column is needed, the lookups are the price of this index; a seek on the clustered key avoids them.",
        "action for wide key lookups",
    ),
    (
        "key_lookup.text_only",
        "Add the fetched columns ({columns}) to the INCLUDE list of {index}.",
        "action when the index definition is not known",
    ),
    // ---- heap_fetches
    (
        "heap_fetches.volume.summary",
        "The index-only scan of {index} on {table} still visited the table {heap_fetches} times for {rows} rows.",
        "summary: PostgreSQL index-only scan with many heap fetches",
    ),
    (
        "heap_fetches.volume.why",
        "PostgreSQL can skip the table only for pages marked all-visible in the visibility map. Pages changed since the last vacuum are not marked, so each of their rows costs a table visit, which is what an index-only scan is meant to avoid.",
        "why heap fetches on an index-only scan cost time",
    ),
    (
        "heap_fetches.volume.action",
        "Vacuum {table} to refresh its visibility map. If the table changes constantly, make autovacuum run more often on it.",
        "action: vacuum the table",
    ),
    // ---- expensive_sort
    (
        "expensive_sort.index.summary",
        "The plan sorts {rows} rows from {table} by {sort_keys}.",
        "summary: an explicit sort an index could provide",
    ),
    (
        "expensive_sort.index.why",
        "The rows come from one table and are sorted afterwards. An index whose key order matches the ORDER BY returns them already sorted, and with a LIMIT the engine can stop after the first rows.",
        "why a sort an index could provide costs time",
    ),
    (
        "expensive_sort.index.action",
        "Create an index on {table} keyed on {index_columns} so the rows are read in order.",
        "action: create an index matching the sort order",
    ),
    (
        "expensive_sort.text_only",
        "Create an index on {table} whose key columns match the ORDER BY, in the same order.",
        "action when the sort columns cannot be turned into a statement",
    ),
    (
        "expensive_sort.sqlite.summary",
        "SQLite sorts the rows of {table} in a temporary B-tree for the ORDER BY.",
        "summary: SQLite uses a temp B-tree for ORDER BY on a single table",
    ),
    (
        "expensive_sort.sqlite.why",
        "The plan uses no index that returns the rows of {table} in the requested order, so SQLite collects them all and sorts them before returning the first one. The plan does not say how many rows that is.",
        "why a SQLite temp B-tree sort can cost time",
    ),
    (
        "expensive_sort.sqlite.action",
        "If the query is slow, create an index on {table} whose key columns match the ORDER BY, in the same order.",
        "action for a SQLite temp B-tree sort",
    ),
    // ---- auto_index
    (
        "auto_index.sqlite.summary",
        "SQLite builds a temporary index on {table} every time this statement runs.",
        "summary: SQLite automatic index",
    ),
    (
        "auto_index.sqlite.why",
        "The join needs to look rows up in {table} and no permanent index fits, so SQLite builds one, uses it, and throws it away. The build reads the whole table on every execution.",
        "why a SQLite automatic index costs time",
    ),
    (
        "auto_index.sqlite.action",
        "Create the index permanently on {table} ({index_columns}) so it is built once.",
        "action: create the index SQLite builds automatically",
    ),
    (
        "auto_index.text_only",
        "Create a permanent index on {table} on the columns the join looks up.",
        "action when the automatic index columns are not readable",
    ),
    (
        "auto_index.spool.summary",
        "The plan builds a temporary index (Eager Index Spool) over {rows} rows of {table} while the statement runs.",
        "summary: SQL Server eager index spool",
    ),
    (
        "auto_index.spool.why",
        "SQL Server decided an index was worth building for this one execution: it reads the rows into tempdb, indexes them, and discards the result afterwards. The build is single-threaded and repeats on every run.",
        "why an eager index spool costs time",
    ),
    (
        "auto_index.spool.action",
        "Create the index permanently on {table} ({index_columns}) so the spool is not needed.",
        "action: create the index the spool builds",
    ),
    // ---- index_merge
    (
        "index_merge.intersect.summary",
        "The plan reads {table} through several single-column indexes and intersects the results ({rows} rows).",
        "summary: index merge intersection or BitmapAnd",
    ),
    (
        "index_merge.intersect.why",
        "Each index is read for its own condition and the row sets are then combined. One index with all the compared columns in its key finds the same rows in a single read.",
        "why intersecting several indexes costs time",
    ),
    (
        "index_merge.intersect.action",
        "Create one composite index on {table} keyed on {index_columns}.",
        "action: replace the index intersection with a composite index",
    ),
    (
        "index_merge.text_only",
        "Create one composite index on {table} whose key holds the columns of all the combined conditions, equality columns first.",
        "action when the merged columns are not readable",
    ),
    // ---- lossy_bitmap
    (
        "lossy_bitmap.recheck.summary",
        "The bitmap scan of {table} ran out of memory for an exact bitmap: {lossy_blocks} of {total_blocks} heap blocks were tracked per page instead of per row.",
        "summary: PostgreSQL bitmap heap scan with lossy pages",
    ),
    (
        "lossy_bitmap.recheck.why",
        "For a lossy page PostgreSQL only knows that some row on it matches, so it reads every row on the page and tests the condition again. The bitmap goes lossy when it does not fit in work_mem.",
        "why lossy bitmap pages cost time",
    ),
    (
        "lossy_bitmap.recheck.action",
        "Raise work_mem for this session and run the statement again until Heap Blocks shows no lossy pages. Change the server default only if many queries need it: every sort and hash in every session can use that much.",
        "action: raise work_mem for a lossy bitmap scan",
    ),
];

pub(super) fn run(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    missing_index(ctx, drafts);
    for position in 0..ctx.len() {
        if !ctx.executed(position) {
            continue;
        }
        table_scan(ctx, position, drafts);
        filter_discards_rows(ctx, position, drafts);
        key_lookup(ctx, position, drafts);
        heap_fetches(ctx, position, drafts);
        expensive_sort(ctx, position, drafts);
        auto_index(ctx, position, drafts);
        index_merge(ctx, position, drafts);
        lossy_bitmap(ctx, position, drafts);
    }
}

/// Evidence lines every read-based finding shows.
pub(super) fn with_scan(mut draft: Draft, ctx: &Ctx<'_>, position: usize, scan: &Scan) -> Draft {
    if let Some(rel) = ctx.rel(position) {
        draft = draft.ev("table", rel.display());
    }
    // A SQL Server bitmap test is not the query's filter: show it as what it
    // is.
    let probe = scan
        .filter_text
        .as_deref()
        .and_then(crate::plans::predicate::bitmap_probe);
    let (filter, bitmap) = match probe {
        Some(probe) => (probe.rest, Some(probe.columns.join(", "))),
        None => (scan.filter_text.clone(), None),
    };
    draft = draft
        .ev_opt("index", ctx.a(position).index.clone())
        .ev_opt(
            "index_condition",
            ctx.a(position)
                .seek_predicate
                .as_ref()
                .map(|p| p.text.clone()),
        )
        .ev_opt("filter", filter)
        .ev_opt("bitmap_filter", bitmap)
        .ev_opt("rows_read", scan.read.map(count))
        .ev_opt("rows_returned", scan.kept.map(count));
    if scan.read.is_none() {
        draft = draft.ev_opt("estimated_rows", scan.est_read.map(count));
    }
    draft = draft.ev_opt("executions", ctx.executions(position).map(count));
    if ctx.timed {
        draft = draft.ev_opt("time", ctx.m(scan.top).inclusive_time_ms.map(ms));
    } else {
        draft = draft.ev_opt("cost_share", ctx.share(scan.top).map(pct));
    }
    draft
}

fn kept_percent(read: f64, kept: f64) -> String {
    if read <= 0.0 {
        return "0%".to_string();
    }
    let percent = kept / read * 100.0;
    if percent > 0.0 && percent < 0.01 {
        crate::plans::phrases::styled("<0.01%".to_string())
    } else if percent < 1.0 {
        crate::plans::phrases::styled(format!("{percent:.2}%"))
    } else {
        format!("{percent:.0}%")
    }
}

fn is_bitmap_probe(text: &str) -> bool {
    text.trim_start().to_ascii_uppercase().starts_with("PROBE(")
}

/// Position of the most expensive read of `table` in the plan.
fn read_of_table(ctx: &Ctx<'_>, schema: &str, table: &str) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for position in 0..ctx.len() {
        if !ctx.op(position).is_read() {
            continue;
        }
        let Some(rel) = ctx.rel(position) else {
            continue;
        };
        if !rel.name.eq_ignore_ascii_case(table) {
            continue;
        }
        if let Some(rel_schema) = &rel.schema
            && !schema.is_empty()
            && !rel_schema.eq_ignore_ascii_case(schema)
        {
            continue;
        }
        let weight = ctx
            .share(position)
            .unwrap_or(0.0)
            .max(if ctx.op(position) == Op::IndexSeek {
                0.0
            } else {
                0.001
            });
        if best.is_none_or(|(_, top)| weight > top) {
            best = Some((position, weight));
        }
    }
    best.map(|(position, _)| position)
}

fn missing_index(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    let mut seen: Vec<String> = Vec::new();
    let warnings = ctx
        .plan
        .warnings
        .iter()
        .chain(ctx.plan.nodes.iter().flat_map(|node| node.warnings.iter()));
    for warning in warnings {
        let PlanWarning::MissingIndex(missing) = warning else {
            continue;
        };
        let table = bare_name(&missing.table);
        let schema = bare_name(&missing.schema);
        let clean = |columns: &[String]| -> Vec<String> {
            columns
                .iter()
                .map(|column| bare_name(column))
                .filter(|column| !column.is_empty())
                .collect()
        };
        let equality = clean(&missing.equality_columns);
        let range = clean(&missing.inequality_columns);
        let include = clean(&missing.included_columns);
        if table.is_empty() || (equality.is_empty() && range.is_empty()) {
            continue;
        }
        let signature = format!(
            "{schema}.{table}|{}|{}|{}",
            equality.join(","),
            range.join(","),
            include.join(",")
        )
        .to_ascii_lowercase();
        if seen.contains(&signature) {
            continue;
        }
        seen.push(signature);
        let label = if schema.is_empty() {
            table.clone()
        } else {
            format!("{schema}.{table}")
        };
        let columns: Vec<String> = equality.iter().chain(range.first()).cloned().collect();
        let suggestion = IndexSuggestion {
            schema: (!schema.is_empty()).then(|| schema.clone()),
            table: table.clone(),
            equality,
            range,
            include,
            ..Default::default()
        };
        let impact = (missing.impact / 100.0).clamp(0.0, 1.0);
        let severity = if impact >= 0.1 { S::Warning } else { S::Info };
        let node = read_of_table(ctx, &schema, &table);
        let scan = node.map(|node| ctx.scan(node));
        let measured = scan
            .as_ref()
            .and_then(|scan| Some((scan.read?, scan.kept?)))
            .filter(|(read, kept)| *read >= 1_000.0 && kept < read);
        let mut args = vec![
            ("table", label.clone()),
            ("index_columns", columns.join(", ")),
        ];
        // CockroachDB prints a recommendation, not an impact figure.
        let cockroach = ctx.engine == PlanEngine::CockroachDb;
        let variant = match measured {
            _ if cockroach => {
                args.retain(|arg| arg.0 != "index_columns");
                args.push((
                    "index_columns",
                    suggestion
                        .equality
                        .iter()
                        .chain(suggestion.range.iter())
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                ));
                "recommended"
            }
            Some((read, kept)) => {
                args.push(("rows_read", count(read)));
                args.push(("rows_kept", count(kept)));
                "measured"
            }
            None => "optimizer",
        };
        let mut draft = Draft::new(K::MissingIndex, severity, C::High, variant, args)
            .index(suggestion, "text_only");
        if cockroach {
            let node = cockroach_scan_of(ctx, &table);
            draft = draft.no_impact().ev("table", label);
            if let Some(node) = node {
                draft = draft.at(node);
            }
            drafts.push(draft);
            continue;
        }
        draft = draft.impact(FindingImpact {
            share: impact,
            basis: ImpactBasis::Cost,
            time_ms: None,
            optimizer_estimate: true,
        });
        if let (Some(node), Some(scan)) = (node, &scan) {
            draft = with_scan(draft.at(node), ctx, node, scan);
        } else {
            draft = draft.ev("table", label);
        }
        drafts.push(draft.ev(
            "missing_index_impact",
            crate::plans::phrases::styled(format!("{:.1}%", missing.impact)),
        ));
    }
}

/// The CockroachDB full scan of `table`, for an index recommendation to
/// point at.
fn cockroach_scan_of(ctx: &Ctx<'_>, table: &str) -> Option<usize> {
    (0..ctx.len()).find(|position| {
        ctx.node(*position)
            .property("spans")
            .is_some_and(|spans| spans.starts_with("FULL SCAN"))
            && ctx
                .rel(*position)
                .is_some_and(|rel| rel.name.eq_ignore_ascii_case(table))
    })
}

fn join_above(ctx: &Ctx<'_>, position: usize) -> Option<(usize, usize)> {
    // (join, the join's child on the path to `position`).
    let mut child = position;
    let mut current = ctx.up(position)?;
    loop {
        match ctx.op(current) {
            Op::HashJoin | Op::MergeJoin => return Some((current, child)),
            Op::HashBuild | Op::Gather | Op::Sort | Op::Materialize => {}
            Op::Filter if ctx.kids(current).len() == 1 => {}
            _ => return None,
        }
        child = current;
        current = ctx.up(current)?;
    }
}

fn table_scan(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.op(position) != Op::FullScan
        || matches!(ctx.dialect, PlanDialect::DuckDb | PlanDialect::ClickHouse)
    {
        return;
    }
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    let scan = ctx.scan(position);
    let share = ctx.share(scan.top);
    let filter = scan
        .filter_raw
        .as_deref()
        .filter(|filter| !is_bitmap_probe(filter));
    if let Some(filter) = filter {
        let terms = ctx.terms(filter);
        let suggestion = advice::suggestion_from_terms(&rel, &terms);
        let filter_text = scan.filter_text.clone().unwrap_or_default();
        if let (Some(read), Some(kept)) = (scan.read, scan.kept) {
            if !(ctx.matters(read, share) && kept <= read * 0.1) {
                return;
            }
            let severity = if share.is_some_and(|share| share >= 0.5) && read >= 100_000.0 {
                S::Critical
            } else {
                S::Warning
            };
            let args = vec![
                ("table", rel.display()),
                ("rows_read", count(read)),
                ("rows_kept", count(kept)),
                ("kept_percent", kept_percent(read, kept)),
                ("filter", filter_text),
                ("index_columns", String::new()),
            ];
            let draft = match suggestion {
                Some(suggestion) => Draft::new(K::TableScan, severity, C::High, "filtered", args)
                    .index(suggestion, "no_columns"),
                None => Draft::new(K::TableScan, severity, C::Medium, "filtered", args)
                    .action("no_columns"),
            };
            drafts.push(with_scan(
                draft.at(scan.top).also(position),
                ctx,
                position,
                &scan,
            ));
            return;
        }
        // No measured rows: only speak when the optimizer's own numbers say
        // the scan dominates the statement.
        if ctx.has_actuals() && scan.read.is_some() {
            return;
        }
        let (Some(est_read), Some(est_kept), Some(suggestion)) =
            (scan.est_read, scan.est_kept, suggestion)
        else {
            return;
        };
        if !(est_read >= 10_000.0
            && est_kept <= est_read * 0.1
            && ctx.an.has_costs
            && share.is_some_and(|share| share >= 0.25))
        {
            return;
        }
        // MySQL's `filtered` is a rule-of-thumb guess unless histograms exist.
        let guessed = matches!(ctx.dialect, PlanDialect::MySql | PlanDialect::MariaDb);
        let (severity, confidence) = if guessed {
            (S::Info, C::Low)
        } else {
            (S::Warning, C::Medium)
        };
        let args = vec![
            ("table", rel.display()),
            ("rows_read", count(est_read)),
            ("rows_kept", count(est_kept)),
            ("filter", filter_text),
            ("index_columns", String::new()),
        ];
        let draft = Draft::new(K::TableScan, severity, confidence, "estimated", args)
            .index(suggestion, "no_columns")
            .note("common.note.estimated_only", &[])
            .at(scan.top)
            .also(position);
        drafts.push(with_scan(draft, ctx, position, &scan));
        return;
    }
    // No filter on the scan: is it feeding a join that needs few of its rows?
    let Some(read) = scan.read.or(ctx.m(position).rows_out) else {
        return;
    };
    let Some((join, side)) = join_above(ctx, position) else {
        return;
    };
    let join_type = ctx
        .a(join)
        .join_type
        .as_deref()
        .unwrap_or("inner")
        .to_ascii_lowercase();
    if !(join_type.contains("inner") || join_type.contains("semi")) || join_type.contains("anti") {
        return;
    }
    let Some(join_rows) = ctx.m(join).rows_out else {
        return;
    };
    let other_rows: Option<f64> = ctx
        .kids(join)
        .iter()
        .filter(|kid| **kid != side)
        .map(|kid| ctx.m(*kid).rows_out)
        .sum();
    let Some(other_rows) = other_rows else {
        return;
    };
    if ctx.kids(join).len() != 2
        || !ctx.matters(read, share)
        || read < 10_000.0
        || join_rows > read * 0.01
        || other_rows > read * 0.01
    {
        return;
    }
    let Some(condition) = ctx.a(join).join_predicate.as_ref() else {
        return;
    };
    let join_column = join_column(ctx, &rel, &ctx.terms(&condition.raw));
    let confidence = if other_rows <= 1_000.0 {
        C::High
    } else {
        C::Medium
    };
    let mut args = vec![
        ("table", rel.display()),
        ("rows_read", count(read)),
        ("join_rows", count(join_rows)),
        ("other_rows", count(other_rows)),
        ("join_condition", condition.text.clone()),
        ("index_columns", String::new()),
    ];
    let Some(column) = join_column else {
        return;
    };
    args.push(("join_column", column.clone()));
    let mut suggestion = IndexSuggestion::for_rel(&rel);
    suggestion.equality.push(column);
    let draft = Draft::new(K::TableScan, S::Warning, confidence, "join", args)
        .index(suggestion, "join_text")
        .at(position)
        .also(join)
        .ev("join_condition", condition.text.clone());
    drafts.push(with_scan(draft, ctx, position, &scan));
}

/// The column of `rel` in an equi-join condition, when exactly one side of
/// one comparison belongs to it.
pub(super) fn join_column(ctx: &Ctx<'_>, rel: &Rel<'_>, terms: &[Term]) -> Option<String> {
    let mut found: Vec<String> = Vec::new();
    for term in terms {
        let Some((left, right)) = term.column_pair() else {
            continue;
        };
        let owned = |column: &super::predicate::ColumnRef| -> bool {
            match &column.qualifier {
                Some(_) => rel.owns(column),
                // Unqualified: only the schema can say whose column it is.
                None => rel
                    .table
                    .is_some_and(|table| table.column(&column.name).is_some()),
            }
        };
        match (owned(left), owned(right)) {
            (true, false) => found.push(left.name.clone()),
            (false, true) => found.push(right.name.clone()),
            _ => {}
        }
    }
    let _ = ctx;
    found.dedup();
    (found.len() == 1).then(|| found.remove(0))
}

fn filter_discards_rows(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let op = ctx.op(position);
    if !matches!(
        op,
        Op::IndexSeek | Op::IndexFullScan | Op::IndexOnly | Op::BitmapHeap | Op::RowLookup
    ) {
        return;
    }
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    let scan = ctx.scan(position);
    let Some(filter) = scan.filter_raw.as_deref().filter(|f| !is_bitmap_probe(f)) else {
        return;
    };
    // Oracle fetches the row by ROWID and filters it there: the rows read
    // are the rows the index below produced.
    let read = if op == Op::RowLookup {
        if ctx.dialect != PlanDialect::Oracle {
            return;
        }
        ctx.kids(position)
            .first()
            .and_then(|kid| ctx.m(*kid).rows_out)
    } else {
        scan.read
    };
    let kept = if op == Op::RowLookup {
        ctx.m(position).rows_out
    } else {
        scan.kept
    };
    let (Some(read), Some(kept)) = (read, kept) else {
        return;
    };
    let removed = read - kept;
    let share = ctx.share(scan.top);
    if !(removed >= 10_000.0 && removed >= read * 0.9 && share.is_none_or(|share| share >= 0.05)) {
        return;
    }
    let index_position = if op == Op::RowLookup {
        ctx.kids(position).first().copied().unwrap_or(position)
    } else {
        position
    };
    let index = ctx
        .a(index_position)
        .index
        .clone()
        .or_else(|| ctx.a(position).index.clone());
    let Some(index) = index else {
        return;
    };
    let mut terms: Vec<Term> = Vec::new();
    if let Some(seek) = &ctx.a(index_position).seek_predicate {
        terms.extend(ctx.terms(&seek.raw));
    }
    let seek_columns = advice::suggestion_from_terms(&rel, &terms);
    terms.extend(ctx.terms(filter));
    let suggestion = advice::suggestion_from_terms(&rel, &terms).filter(|all| {
        // Only worth a statement when the filter adds a column the index
        // condition did not already have.
        let before = seek_columns
            .as_ref()
            .map(|s| s.equality.len() + s.range.len())
            .unwrap_or(0);
        all.equality.len() + all.range.len() > before
    });
    let args = vec![
        ("table", rel.display()),
        ("index", index),
        ("rows_read", count(read)),
        ("rows_removed", count(removed)),
        ("filter", scan.filter_text.clone().unwrap_or_default()),
        ("index_columns", String::new()),
    ];
    let severity = if share.is_some_and(|share| share >= 0.5) && removed >= 100_000.0 {
        S::Critical
    } else {
        S::Warning
    };
    let draft = match suggestion {
        Some(suggestion) => Draft::new(K::FilterDiscardsRows, severity, C::High, "index", args)
            .index(suggestion, "text_only"),
        None => Draft::new(K::FilterDiscardsRows, severity, C::Medium, "index", args)
            .action("text_only"),
    };
    let draft = draft
        .at(scan.top)
        .also(position)
        .ev("rows_removed", count(removed));
    drafts.push(with_scan(draft, ctx, position, &scan));
}

fn plain_column(text: &str) -> Option<(Option<String>, String)> {
    let text = text.trim();
    if text.is_empty()
        || !text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_$#.\"`[]".contains(c))
    {
        return None;
    }
    let mut parts = split_qualified(text);
    let name = parts.pop()?;
    Some((parts.pop(), name))
}

fn key_lookup(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.op(position) != Op::RowLookup || ctx.dialect != PlanDialect::SqlServer {
        return;
    }
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    let metrics = ctx.m(position);
    let Some(lookups) = metrics.loops.or(metrics.rows_out) else {
        return;
    };
    let share = ctx.share(position);
    if !(lookups >= 1_000.0 && (share.is_some_and(|share| share >= 0.1) || lookups >= 10_000.0)) {
        return;
    }
    if share.is_some_and(|share| share < 0.02) {
        return;
    }
    // The index whose rows are looked up: the other input of the join above.
    let parent = ctx.up(position);
    let sibling = parent.and_then(|parent| {
        ctx.kids(parent)
            .iter()
            .copied()
            .flat_map(|kid| std::iter::once(kid).chain(ctx.descendants(kid)))
            .find(|kid| {
                *kid != position
                    && matches!(ctx.op(*kid), Op::IndexSeek | Op::IndexFullScan)
                    && ctx
                        .rel(*kid)
                        .is_some_and(|other| other.name.eq_ignore_ascii_case(&rel.name))
                    && ctx.a(*kid).index.is_some()
            })
    });
    let Some(sibling) = sibling else {
        return;
    };
    let index = ctx.a(sibling).index.clone().unwrap_or_default();
    let mut columns: Vec<String> = Vec::new();
    for column in &ctx.node(position).output_columns {
        if let Some((_, name)) = plain_column(column)
            && !columns.iter().any(|c| c.eq_ignore_ascii_case(&name))
        {
            columns.push(name);
        }
    }
    if columns.is_empty() {
        return;
    }
    let mut args = vec![
        ("table", rel.display()),
        ("index", index.clone()),
        ("lookups", count(lookups)),
        ("columns", columns.join(", ")),
        ("column_count", columns.len().to_string()),
        ("index_columns", String::new()),
    ];
    let severity = if share.is_some_and(|share| share >= 0.5) {
        S::Critical
    } else {
        S::Warning
    };
    let wide = columns.len() > 8;
    let mut draft = if wide {
        Draft::new(
            K::KeyLookup,
            severity,
            C::High,
            "wide",
            std::mem::take(&mut args),
        )
    } else {
        let known = rel.table.and_then(|table| table.index(&index));
        let draft = Draft::new(
            K::KeyLookup,
            severity,
            C::High,
            "volume",
            std::mem::take(&mut args),
        );
        match known {
            Some(known) if !known.columns.iter().any(|c| c.expression) => {
                let mut suggestion = IndexSuggestion::for_rel(&rel);
                suggestion.equality = known.key_names().iter().map(|k| k.to_string()).collect();
                suggestion.include = known
                    .include_columns
                    .iter()
                    .cloned()
                    .chain(columns.iter().cloned())
                    .collect();
                draft.index(suggestion, "text_only")
            }
            _ => {
                let draft = draft.action("text_only");
                if ctx.schema.is_none() {
                    draft.note("common.note.needs_schema", &[])
                } else {
                    draft
                }
            }
        }
    };
    draft = draft
        .at(position)
        .also(sibling)
        .ev("table", rel.display())
        .ev("index", index)
        .ev("executions", count(lookups))
        .ev("output_columns", columns.join(", "));
    if ctx.timed {
        draft = draft.ev_opt("time", metrics.inclusive_time_ms.map(ms));
    } else {
        draft = draft.ev_opt("cost_share", share.map(pct));
    }
    drafts.push(draft);
}

fn heap_fetches(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.op(position) != Op::IndexOnly {
        return;
    }
    let node = ctx.node(position);
    let (Some(fetches), Some(rows), Some(rel)) = (
        node.heap_fetches,
        ctx.m(position).rows_out,
        ctx.rel(position),
    ) else {
        return;
    };
    if !(fetches >= 1_000.0 && fetches >= rows * 0.1) {
        return;
    }
    let share = ctx.share(position);
    let severity = if fetches >= 10_000.0 && share.is_none_or(|share| share >= 0.05) {
        S::Warning
    } else {
        S::Info
    };
    let args = vec![
        ("table", rel.display()),
        ("index", ctx.a(position).index.clone().unwrap_or_default()),
        ("heap_fetches", count(fetches)),
        ("rows", count(rows)),
    ];
    let sql = advice::table_sql(ctx, &rel).map(|table| format!("VACUUM (ANALYZE) {table};"));
    let scan = ctx.scan(position);
    let draft = Draft::new(K::HeapFetches, severity, C::High, "volume", args)
        .maybe_sql(sql)
        .at(position)
        .ev("heap_fetches", count(fetches));
    drafts.push(with_scan(draft, ctx, position, &scan));
}

fn expensive_sort(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.op(position) != Op::Sort {
        return;
    }
    if ctx.dialect == PlanDialect::Sqlite {
        sqlite_sort(ctx, position, drafts);
        return;
    }
    if matches!(ctx.dialect, PlanDialect::DuckDb | PlanDialect::ClickHouse) {
        return;
    }
    let node = ctx.node(position);
    let metrics = ctx.m(position);
    let rows = metrics.rows_in.or(metrics.rows_out);
    let self_share = ctx.self_share(position);
    let heavy = self_share.is_some_and(|share| share >= 0.2);
    let large = rows.is_some_and(|rows| rows >= 100_000.0);
    if !(heavy || large) || rows.is_some_and(|rows| rows < 1_000.0) {
        return;
    }
    if ctx.has_actuals() && rows.is_none() {
        return;
    }
    // Every input must come from one table read, with nothing in between
    // that changes the rows (no join, no aggregate).
    let below = ctx.descendants(position);
    let mut reads = Vec::new();
    for other in &below {
        match ctx.op(*other) {
            op if op.is_read() => reads.push(*other),
            Op::Filter | Op::Gather | Op::BitmapIndex => {}
            Op::Other
                if matches!(
                    ctx.a(*other).category(),
                    OperatorCategory::Compute | OperatorCategory::Parallelism
                ) => {}
            _ => return,
        }
    }
    let [read] = reads.as_slice() else {
        return;
    };
    let Some(rel) = ctx.rel(*read) else {
        return;
    };
    let aliased = matches!(ctx.dialect, PlanDialect::MySql | PlanDialect::MariaDb);
    let mut keys: Vec<(String, bool)> = Vec::new();
    for key in &ctx.a(position).sort_keys {
        let lower = key.to_ascii_lowercase();
        let (text, descending) = if let Some(prefix) = lower.strip_suffix(" desc") {
            (&key[..prefix.len()], true)
        } else if let Some(prefix) = lower.strip_suffix(" asc") {
            (&key[..prefix.len()], false)
        } else {
            (key.as_str(), false)
        };
        let Some((qualifier, name)) = plain_column(text) else {
            return;
        };
        let column = super::predicate::ColumnRef {
            qualifier: qualifier.clone(),
            name: name.clone(),
        };
        let confirmed = rel.table.is_some_and(|table| table.column(&name).is_some());
        // MySQL prints output aliases as sort keys; take a name only when
        // it is qualified with this table or the schema has it.
        if !rel.owns(&column) || (aliased && qualifier.is_none() && !confirmed) {
            return;
        }
        if rel.table.is_some_and(|table| table.has_columns()) && !confirmed {
            return;
        }
        keys.push((name, descending));
    }
    if keys.is_empty() {
        return;
    }
    let scan = ctx.scan(*read);
    let mut suggestion = IndexSuggestion::for_rel(&rel);
    let mut terms: Vec<Term> = Vec::new();
    for text in [
        scan.filter_raw.as_deref(),
        ctx.a(*read).seek_predicate.as_ref().map(|p| p.raw.as_str()),
    ]
    .into_iter()
    .flatten()
    {
        terms.extend(ctx.terms(text));
    }
    // A range condition and a sort cannot both be served by one key order
    // unless they are on the same column, so stay silent.
    if terms
        .iter()
        .any(|term| !matches!(term.kind, TermKind::Equality | TermKind::IsNull))
    {
        return;
    }
    if let Some(from_filter) = advice::suggestion_from_terms(&rel, &terms) {
        suggestion.equality = from_filter.equality;
    }
    suggestion.sort = keys;
    let (severity, confidence) = if heavy {
        (S::Warning, C::Medium)
    } else {
        (S::Info, C::Medium)
    };
    let args = vec![
        ("table", rel.display()),
        (
            "rows",
            rows.map(count)
                .unwrap_or_else(|| count(node.estimated_rows)),
        ),
        ("sort_keys", ctx.a(position).sort_keys.join(", ")),
        ("index_columns", String::new()),
    ];
    let mut draft = Draft::new(K::ExpensiveSort, severity, confidence, "index", args)
        .index(suggestion, "text_only")
        .own_share()
        .at(position)
        .also(*read)
        .ev("table", rel.display())
        .ev("sort_keys", ctx.a(position).sort_keys.join(", "))
        .ev_opt("sort_method", node.sort_method.clone())
        .ev_opt("rows_read", rows.map(count));
    if ctx.timed {
        draft = draft.ev_opt("own_time", metrics.self_time_ms.map(ms));
    } else {
        draft = draft.ev_opt("cost_share", self_share.map(pct));
    }
    drafts.push(draft);
}

fn sqlite_sort(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let tables: Vec<usize> = (0..ctx.len())
        .filter(|other| ctx.node(*other).relation.is_some())
        .collect();
    let [only] = tables.as_slice() else {
        return;
    };
    if (0..ctx.len()).any(|other| ctx.a(other).subplan_name.is_some()) {
        return;
    }
    let Some(rel) = ctx.rel(*only) else {
        return;
    };
    let draft = Draft::new(
        K::ExpensiveSort,
        S::Info,
        C::Low,
        "sqlite",
        vec![("table", rel.display())],
    )
    .at(position)
    .also(*only)
    .ev("table", rel.display());
    drafts.push(if rel.alias_only {
        draft.note("common.note.alias", &[("table", rel.display())])
    } else {
        draft
    });
}

fn auto_index(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let name = ctx.op_name(position);
    match ctx.dialect {
        PlanDialect::Sqlite if name.contains("AUTOMATIC") && name.contains("INDEX") => {
            let Some(rel) = ctx.rel(position) else {
                return;
            };
            let suggestion = ctx
                .a(position)
                .seek_predicate
                .as_ref()
                .and_then(|seek| advice::suggestion_from_terms(&rel, &ctx.terms(&seek.raw)));
            let args = vec![("table", rel.display()), ("index_columns", String::new())];
            let draft = Draft::new(K::AutoIndex, S::Warning, C::High, "sqlite", args);
            let draft = match suggestion {
                Some(suggestion) => draft.index(suggestion, "text_only"),
                None => draft.action("text_only"),
            };
            drafts.push(
                draft.at(position).ev("table", rel.display()).ev_opt(
                    "index_condition",
                    ctx.a(position)
                        .seek_predicate
                        .as_ref()
                        .map(|p| p.text.clone()),
                ),
            );
        }
        PlanDialect::SqlServer if name.contains("INDEX SPOOL") => {
            let node = ctx.node(position);
            if !node.logical_op.to_ascii_uppercase().contains("EAGER") {
                return;
            }
            let metrics = ctx.m(position);
            let Some(rows) = metrics.rows_in.or(metrics.rows_out) else {
                return;
            };
            if rows < 1_000.0 {
                return;
            }
            let source = ctx
                .descendants(position)
                .into_iter()
                .find(|other| ctx.op(*other).is_read() && ctx.rel(*other).is_some());
            let Some(source) = source else {
                return;
            };
            let Some(rel) = ctx.rel(source) else {
                return;
            };
            let suggestion = ctx
                .a(position)
                .seek_predicate
                .as_ref()
                .and_then(|seek| advice::suggestion_from_terms(&rel, &ctx.terms(&seek.raw)));
            let args = vec![
                ("table", rel.display()),
                ("rows", count(rows)),
                ("index_columns", String::new()),
            ];
            let draft = Draft::new(K::AutoIndex, S::Warning, C::High, "spool", args);
            let draft = match suggestion {
                Some(suggestion) => draft.index(suggestion, "text_only"),
                None => draft.action("text_only"),
            };
            drafts.push(
                draft
                    .at(position)
                    .also(source)
                    .ev("table", rel.display())
                    .ev("rows_read", count(rows))
                    .ev_opt(
                        "index_condition",
                        ctx.a(position)
                            .seek_predicate
                            .as_ref()
                            .map(|p| p.text.clone()),
                    )
                    .ev_opt(
                        "time",
                        metrics.inclusive_time_ms.filter(|_| ctx.timed).map(ms),
                    ),
            );
        }
        _ => {}
    }
}

fn index_merge(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    let node = ctx.node(position);
    let name = ctx.op_name(position);
    // (node carrying the table, node carrying the filter)
    let target = match ctx.op(position) {
        Op::IndexMerge => {
            let intersect = name.contains("INTERSECT")
                || node
                    .index_name
                    .as_deref()
                    .is_some_and(|index| index.to_ascii_lowercase().contains("intersect("))
                || node
                    .extra
                    .iter()
                    .any(|extra| extra.to_ascii_lowercase().contains("using intersect"));
            if !intersect {
                return;
            }
            let table = if ctx.rel(position).is_some() {
                Some(position)
            } else {
                ctx.descendants(position)
                    .into_iter()
                    .find(|other| ctx.rel(*other).is_some())
            };
            let Some(table) = table else { return };
            table
        }
        Op::BitmapAnd => match ctx.up(position) {
            Some(parent) if ctx.op(parent) == Op::BitmapHeap => parent,
            _ => return,
        },
        _ => return,
    };
    let Some(rel) = ctx.rel(target) else {
        return;
    };
    // PostgreSQL prints rows=0 for BitmapAnd, so the rows come from the
    // heap scan itself, never from what its child returned.
    let rows = {
        let scan = ctx.scan(target);
        let out = ctx.m(target).rows_out;
        match (scan.read, out) {
            (Some(read), Some(out)) => Some(read.max(out)),
            (read, out) => read.or(out),
        }
    };
    let share = ctx.share(target);
    let enough = match (rows, share) {
        (Some(rows), Some(share)) => rows >= 10_000.0 && share >= 0.1,
        (Some(rows), None) => rows >= 10_000.0,
        (None, Some(share)) => !ctx.has_actuals() && share >= 0.25,
        (None, None) => false,
    };
    if !enough {
        return;
    }
    // The columns: every equality the merged index conditions test.
    let mut terms: Vec<Term> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    if ctx.op(position) == Op::BitmapAnd {
        for kid in ctx.descendants(position) {
            if let Some(seek) = &ctx.a(kid).seek_predicate {
                texts.push(seek.raw.clone());
            }
        }
    } else if let Some(filter) = ctx.scan(target).filter_raw {
        texts.push(filter);
    }
    for text in &texts {
        terms.extend(ctx.terms(text));
    }
    let suggestion = advice::suggestion_from_terms(&rel, &terms)
        .filter(|s| s.equality.len() + s.range.len() >= 2);
    let args = vec![
        ("table", rel.display()),
        (
            "rows",
            rows.map(count)
                .unwrap_or_else(|| count(node.estimated_rows)),
        ),
        ("index_columns", String::new()),
    ];
    let draft = Draft::new(K::IndexMerge, S::Info, C::Medium, "intersect", args);
    let draft = match suggestion {
        Some(suggestion) => draft.index(suggestion, "text_only"),
        None => draft.action("text_only"),
    };
    let scan = ctx.scan(target);
    drafts.push(with_scan(
        draft.at(target).also(position),
        ctx,
        target,
        &scan,
    ));
}

fn lossy_bitmap(ctx: &Ctx<'_>, position: usize, drafts: &mut Vec<Draft>) {
    if ctx.op(position) != Op::BitmapHeap {
        return;
    }
    let node = ctx.node(position);
    let Some(lossy) = node.lossy_heap_blocks.filter(|lossy| *lossy > 0.0) else {
        return;
    };
    let exact = node.exact_heap_blocks.unwrap_or(0.0);
    let total = exact + lossy;
    let rechecked = node.rows_removed_by_index_recheck.unwrap_or(0.0);
    if !((lossy >= 100.0 && lossy >= total * 0.1) || rechecked >= 10_000.0) {
        return;
    }
    let Some(rel) = ctx.rel(position) else {
        return;
    };
    let share = ctx.share(position);
    let severity = if share.is_none_or(|share| share >= 0.1) {
        S::Warning
    } else {
        S::Info
    };
    let args = vec![
        ("table", rel.display()),
        ("lossy_blocks", count(lossy)),
        ("total_blocks", count(total)),
    ];
    let scan = ctx.scan(position);
    let draft = Draft::new(K::LossyBitmap, severity, C::High, "recheck", args)
        .sql("SHOW work_mem;")
        .at(position)
        .ev(
            "heap_blocks",
            format!("exact={} lossy={}", count(exact), count(lossy)),
        )
        .ev_opt(
            "rows_removed",
            node.rows_removed_by_index_recheck
                .filter(|rows| *rows > 0.0)
                .map(count),
        );
    drafts.push(with_scan(draft, ctx, position, &scan));
}
