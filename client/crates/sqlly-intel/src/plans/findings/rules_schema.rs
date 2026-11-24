//! Rules that need the table definitions: an index that exists and was not
//! used, foreign keys without an index, joins on columns of different types,
//! `NOT IN` over a nullable column, tables without a primary key, over-wide
//! indexes, and a schema that does not match the plan.
//!
//! Every rule still starts from something the plan does. The schema only
//! explains it or makes the advice exact.

use super::advice::{IndexSuggestion, stats_sql};
use super::facts::{Ctx, Op, Rel, count, ms};
use super::predicate::{ColumnRef, Side, Term, TermKind};
use super::rules_predicate::{Volume, scan_volume};
use super::strings::{Args, Template};
use super::{Draft, FindingConfidence as C, FindingKind as K, FindingSeverity as S};
use crate::plans::PlanDialect;
use crate::plans::schema::{SchemaColumn, SchemaIndex, SchemaTable, TypeFamily, split_qualified};

pub(super) const TEMPLATES: &[Template] = &[
    // ---- schema_index_not_used
    (
        "schema_index_not_used.misestimate.summary",
        "{table} has the index {index} on {column}, but the plan scans the table: the optimizer expected {estimated_rows} rows to match and {actual_rows} did.",
        "summary: an index from the schema fits the filter but a scan was chosen because of a high row estimate",
    ),
    (
        "schema_index_not_used.misestimate.why",
        "The optimizer uses an index only when it expects few enough rows to make the lookups cheaper than one pass over the table. With an estimate this far above the real count, the scan looked cheaper than it was.",
        "why an unused index caused by a wrong estimate costs",
    ),
    (
        "schema_index_not_used.misestimate.action",
        "Refresh the statistics on {table} so the optimizer sees how few rows match, then capture the plan again.",
        "action: refresh statistics so an existing index is chosen",
    ),
    (
        "schema_index_not_used.literal_type.summary",
        "{table} has the index {index} on {column}, but the plan scans the table: {column} is {column_type} and the query compares it with a number.",
        "summary: MySQL cannot use an index on a string column compared with a numeric literal",
    ),
    (
        "schema_index_not_used.literal_type.why",
        "To compare a string column with a number, MySQL converts the value of every row to a number. An index on the string values cannot be searched for the converted value, so the whole table is read.",
        "why comparing a string column with a number prevents index use",
    ),
    (
        "schema_index_not_used.literal_type.action",
        "Write the value as a string, in quotes, so it is compared with the column as stored. If the value comes from the application, bind it as a string parameter.",
        "action: quote the literal compared with a string column",
    ),
    (
        "schema_index_not_used.unused.summary",
        "{table} has the index {index} on {column}, but the plan scans the table and keeps {rows_returned} of {rows_read} rows.",
        "summary: an index from the schema fits a selective filter but a scan was chosen, reason not visible",
    ),
    (
        "schema_index_not_used.unused.why",
        "The filter is selective and an index leads with its column, so a search of the index would read far fewer rows than the scan did. The plan does not show why the optimizer passed it over.",
        "why an unused index on a selective filter costs",
    ),
    (
        "schema_index_not_used.unused.action",
        "Check, in this order: that the index is valid and enabled on this server (the supplied schema may be newer than the database), that the compared value has the column's type and collation, and that the table's statistics are current.",
        "action for an unused index when the reason is not visible",
    ),
    // ---- schema_fk_unindexed
    (
        "schema_fk_unindexed.join.summary",
        "{table}.{column} is a foreign key to {parent} with no index, and this plan joins on it by scanning {table}.",
        "summary: a join on a foreign key column that has no supporting index",
    ),
    (
        "schema_fk_unindexed.join.why",
        "A foreign key does not create an index on the referencing column on this engine. Every join from {parent} to {table} has to scan or hash the whole table, and every delete or key update on {parent} has to search {table} for referencing rows the same way.",
        "why an unindexed foreign key costs",
    ),
    (
        "schema_fk_unindexed.join.action",
        "Create an index on {table} ({index_columns}).",
        "action: index the foreign key column",
    ),
    (
        "schema_fk_unindexed.join_text",
        "Create an index on {table} that leads with the foreign key column {column}.",
        "action for an unindexed foreign key when no statement can be written",
    ),
    // ---- schema_type_mismatch
    (
        "schema_type_mismatch.join.summary",
        "This join compares {left_column} ({left_type}) with {right_column} ({right_type}): the two columns have different types.",
        "summary: join columns with different declared types",
    ),
    (
        "schema_type_mismatch.join.why",
        "To compare values of different types the engine converts one side for every row. An index on the converted column cannot be searched for the join, and the row estimate for the join is less reliable.",
        "why a join on mismatched types costs",
    ),
    (
        "schema_type_mismatch.join.action",
        "Give both columns the same type. Changing a column's type rewrites the table and its indexes, so the cheaper side to change is usually the smaller table. Until then, cast the column of the smaller input explicitly so the other side's index stays usable.",
        "action for a join on mismatched types",
    ),
    (
        "schema_type_mismatch.collation.summary",
        "This join compares {left_column} ({left_type}) with {right_column} ({right_type}): the two columns have different collations.",
        "summary: join columns with different collations",
    ),
    (
        "schema_type_mismatch.collation.why",
        "Strings in different collations cannot be compared directly. The engine converts one side for every row, and an index on the converted column cannot be searched for the join.",
        "why a join on mismatched collations costs",
    ),
    (
        "schema_type_mismatch.collation.action",
        "Give both columns the same collation. Changing a column's collation rewrites the table and its indexes, so the cheaper side to change is usually the smaller table.",
        "action for a join on mismatched collations",
    ),
    // ---- schema_nullable_not_in
    (
        "schema_nullable_not_in.anti.summary",
        "This NOT IN compares against {column}, which the schema declares nullable.",
        "summary: NOT IN subquery over a nullable column",
    ),
    (
        "schema_nullable_not_in.anti.why",
        "NOT IN has to treat NULL as a possible match: if the subquery returns even one NULL, the condition is true for no row and the query returns nothing. The engine has to carry that check through the plan, which rules out its fastest anti-join strategies.",
        "why NOT IN over a nullable column costs and can return wrong-looking results",
    ),
    (
        "schema_nullable_not_in.anti.action",
        "Rewrite the condition as NOT EXISTS with the comparison inside the subquery. NOT EXISTS ignores NULLs and can run as a plain anti-join. If {column} never holds NULL, declaring it NOT NULL has the same effect on the plan.",
        "action: rewrite NOT IN as NOT EXISTS",
    ),
    // ---- schema_heap_table
    (
        "schema_heap_table.heap.summary",
        "{table} has no clustered index (it is a heap), and this plan reads {rows} rows from it.",
        "summary: SQL Server table without a clustered index read at volume",
    ),
    (
        "schema_heap_table.heap.why",
        "A heap stores rows in no order. Every range or ordered read has to go through a nonclustered index plus one row lookup per row, and updated rows that no longer fit leave forwarding pointers that cost an extra read each.",
        "why a SQL Server heap costs",
    ),
    (
        "schema_heap_table.heap.action",
        "Create a clustered index on the column this table is most often searched or joined by; a narrow, ever-increasing key such as an identity column is the usual choice. Building it rewrites the table and every nonclustered index.",
        "action for a SQL Server heap",
    ),
    (
        "schema_heap_table.no_primary_key.summary",
        "{table} has no primary key in the supplied schema, and this plan reads {rows} rows from it.",
        "summary: a table without a primary key read at volume",
    ),
    (
        "schema_heap_table.no_primary_key.why",
        "Without a primary key the engine has no declared unique column to plan with: it cannot assume a join to this table returns at most one row, and replication and row-level tooling have nothing to identify a row by.",
        "why a table without a primary key costs",
    ),
    (
        "schema_heap_table.no_primary_key.action",
        "If one column or combination of columns identifies a row, declare it as the primary key. If the key was added in a statement that was not part of the supplied schema, ignore this finding.",
        "action for a table without a primary key",
    ),
    // ---- schema_wide_index
    (
        "schema_wide_index.keys.summary",
        "The index {index} that this plan uses has {key_count} key columns.",
        "summary: an index used by the plan has many key columns",
    ),
    (
        "schema_wide_index.keys.why",
        "Every key column is stored in every level of the index and has to be kept in order on each write. Past the columns a query searches or sorts by, extra key columns make the index larger and slower to maintain without making it faster to search.",
        "why an index with many key columns costs",
    ),
    (
        "schema_wide_index.keys.action",
        "Keep as key columns only the ones queries search or sort by, in that order. Where the engine supports it, move columns that are only read to the INCLUDE list.",
        "action for an index with many key columns",
    ),
    (
        "schema_wide_index.copy.summary",
        "The index {index} that this plan uses stores {stored_count} of the {column_count} columns of {table}.",
        "summary: an index used by the plan stores most columns of its table",
    ),
    (
        "schema_wide_index.copy.why",
        "An index that carries most of the table is close to a second copy of it: every insert and most updates are written twice, and it takes about as much disk and memory as the table.",
        "why an index that stores most of the table costs",
    ),
    (
        "schema_wide_index.copy.action",
        "Check which queries need each stored column. An index sized for one report is often paid for by every write to the table.",
        "action for an index that stores most of the table",
    ),
    // ---- schema_stale
    (
        "schema_stale.mismatch.summary",
        "The supplied schema does not match this plan: {items}.",
        "summary: plan names tables, indexes or columns the schema does not have; {items} is a list such as `table orders_archive, index ix_orders_date on orders`",
    ),
    (
        "schema_stale.mismatch.why",
        "Advice that depends on the schema, such as which indexes already exist, may be wrong for the objects listed here.",
        "why a schema that does not match the plan matters",
    ),
    (
        "schema_stale.mismatch.action",
        "Export the schema again from the database the plan was captured on.",
        "action for a schema that does not match the plan",
    ),
];

pub(super) fn run(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    let Some(schema) = ctx.schema else {
        return;
    };
    if schema.is_empty() {
        return;
    }
    let reads = plan_reads(ctx);
    let mut heap_tables: Vec<String> = Vec::new();
    let mut wide_indexes: Vec<String> = Vec::new();
    for (position, rel) in &reads {
        if !ctx.executed(*position) {
            continue;
        }
        index_not_used(ctx, *position, rel, drafts);
        heap_table(ctx, *position, rel, drafts, &mut heap_tables);
        wide_index(ctx, *position, rel, drafts, &mut wide_indexes);
    }
    let mut seen_pairs: Vec<(String, String)> = Vec::new();
    for position in 0..ctx.len() {
        if !ctx.executed(position) {
            continue;
        }
        for (text, _) in join_conditions(ctx, position) {
            for term in ctx.terms(&text) {
                let Some((left, right)) = term.column_pair() else {
                    continue;
                };
                let (Some(left), Some(right)) =
                    (resolve(ctx, &reads, left), resolve(ctx, &reads, right))
                else {
                    continue;
                };
                let key = pair_key(&left, &right);
                if seen_pairs.contains(&key) {
                    continue;
                }
                seen_pairs.push(key);
                fk_unindexed(ctx, position, &left, &right, drafts);
                fk_unindexed(ctx, position, &right, &left, drafts);
                type_mismatch(ctx, position, &left, &right, drafts);
            }
        }
        nullable_not_in(ctx, position, &reads, drafts);
    }
    stale(ctx, &reads, drafts);
}

/// Every node that reads a real table, with the table.
fn plan_reads<'a>(ctx: &Ctx<'a>) -> Vec<(usize, Rel<'a>)> {
    (0..ctx.len())
        .filter_map(|position| ctx.rel(position).map(|rel| (position, rel)))
        .collect()
}

/// A column of a table the plan reads, resolved against the schema.
struct Resolved<'a> {
    /// The node that reads the table.
    position: usize,
    table: &'a SchemaTable,
    column: &'a SchemaColumn,
    display: String,
}

fn pair_key(left: &Resolved<'_>, right: &Resolved<'_>) -> (String, String) {
    let mut names = [
        left.display.to_ascii_lowercase(),
        right.display.to_ascii_lowercase(),
    ];
    names.sort();
    let [a, b] = names;
    (a, b)
}

/// Finds whose column `column` is. A qualified column must name a table or
/// alias in the plan; an unqualified one must exist on exactly one table.
fn resolve<'a>(
    ctx: &Ctx<'a>,
    reads: &[(usize, Rel<'a>)],
    column: &ColumnRef,
) -> Option<Resolved<'a>> {
    let _ = ctx;
    let mut found: Option<Resolved<'a>> = None;
    for (position, rel) in reads {
        let Some(table) = rel.table else { continue };
        if column.qualifier.is_some() && !rel.owns(column) {
            continue;
        }
        let Some(schema_column) = table.column(&column.name) else {
            continue;
        };
        if let Some(existing) = &found {
            // The same table read twice is still one answer.
            if std::ptr::eq(existing.table, table) {
                continue;
            }
            return None;
        }
        found = Some(Resolved {
            position: *position,
            table,
            column: schema_column,
            display: format!("{}.{}", table.qualified_name(), schema_column.name),
        });
    }
    found
}

/// Join conditions printed at `position`: the join's own, and conditions
/// against another table pushed into a read.
fn join_conditions(ctx: &Ctx<'_>, position: usize) -> Vec<(String, String)> {
    let analysis = ctx.a(position);
    let mut out = Vec::new();
    for predicate in [
        &analysis.join_predicate,
        &analysis.seek_predicate,
        &analysis.filter,
    ]
    .into_iter()
    .flatten()
    {
        out.push((predicate.raw.clone(), predicate.text.clone()));
    }
    out
}

fn leading_index<'a>(table: &'a SchemaTable, column: &str) -> Option<&'a SchemaIndex> {
    table.all_indexes().find(|index| {
        index.is_ordered()
            && index.filter.is_none()
            && index.columns.first().is_some_and(|first| {
                !first.expression
                    && first.prefix_length.is_none()
                    && first.name.eq_ignore_ascii_case(column)
            })
    })
}

fn index_label(index: &SchemaIndex) -> String {
    if index.name.is_empty() {
        if index.primary {
            "PRIMARY KEY".to_string()
        } else {
            format!("({})", index.key_names().join(", "))
        }
    } else {
        index.name.clone()
    }
}

/// True for a constant the plan printed as a bare number.
fn numeric_literal(text: &str) -> bool {
    let text = text.trim().trim_matches(|c| c == '(' || c == ')');
    !text.is_empty() && text.parse::<f64>().is_ok()
}

fn index_not_used(ctx: &Ctx<'_>, position: usize, rel: &Rel<'_>, drafts: &mut Vec<Draft>) {
    if ctx.op(position) != Op::FullScan {
        return;
    }
    let Some(table) = rel.table else { return };
    let scan = ctx.scan(position);
    let Some(filter) = scan.filter_raw.as_deref() else {
        return;
    };
    let Volume::Measured {
        read,
        kept,
        severity,
    } = scan_volume(ctx, position, &scan)
    else {
        return;
    };
    // The first comparison of a bare column that an index leads with.
    // Equality first: it is the stronger reason to expect a seek.
    let terms = ctx.terms(filter);
    let candidate = |kinds: &[TermKind]| -> Option<(&Term, &SchemaColumn, &SchemaIndex)> {
        terms.iter().find_map(|term| {
            if !kinds.contains(&term.kind) || term.negated {
                return None;
            }
            let column = term.bare_column().filter(|column| rel.owns(column))?;
            let schema_column = table.column(&column.name)?;
            let index = leading_index(table, &schema_column.name)?;
            Some((term, schema_column, index))
        })
    };
    let Some((term, column, index)) =
        candidate(&[TermKind::Equality]).or_else(|| candidate(&[TermKind::In, TermKind::Range]))
    else {
        return;
    };
    let mut args: Args = vec![
        ("table", rel.display()),
        ("index", index_label(index)),
        ("column", column.name.clone()),
        ("column_type", column.type_name.clone()),
        ("rows_read", count(read)),
        ("rows_returned", count(kept)),
    ];
    let constant = match (&term.left, &term.right) {
        (Side::Constant(text), _) | (_, Side::Constant(text)) => Some(text.as_str()),
        _ => None,
    };
    let string_column = matches!(column.type_family(), TypeFamily::Text { .. });
    let estimated = scan.est_kept.or(ctx.m(scan.top).estimated_rows_total);
    let draft = if matches!(ctx.dialect, PlanDialect::MySql | PlanDialect::MariaDb)
        && string_column
        && constant.is_some_and(numeric_literal)
    {
        Draft::new(
            K::SchemaIndexNotUsed,
            severity,
            C::High,
            "literal_type",
            args,
        )
    } else if let Some(estimated) = estimated.filter(|rows| *rows >= kept.max(1.0) * 10.0) {
        args.push(("estimated_rows", count(estimated)));
        args.push(("actual_rows", count(kept)));
        Draft::new(
            K::SchemaIndexNotUsed,
            severity,
            C::Medium,
            "misestimate",
            args,
        )
        .maybe_sql(stats_sql(ctx, rel))
        .ev("estimated_rows", count(estimated))
        .ev("actual_rows", count(kept))
    } else {
        Draft::new(
            K::SchemaIndexNotUsed,
            S::Info.max(severity.min(S::Warning)),
            C::Low,
            "unused",
            args,
        )
    };
    drafts.push(
        draft
            .at(scan.top)
            .also(position)
            .ev("table", rel.display())
            .ev(
                "existing_index",
                format!("{} ({})", index_label(index), index.key_names().join(", ")),
            )
            .ev("types", format!("{} {}", column.name, column.type_name))
            .ev_opt("filter", scan.filter_text.clone())
            .ev("rows_read", count(read))
            .ev("rows_returned", count(kept))
            .ev_opt("time", ctx.m(scan.top).inclusive_time_ms.map(ms)),
    );
}

fn fk_unindexed(
    ctx: &Ctx<'_>,
    join: usize,
    child: &Resolved<'_>,
    parent: &Resolved<'_>,
    drafts: &mut Vec<Draft>,
) {
    // InnoDB creates the index with the foreign key.
    if matches!(ctx.dialect, PlanDialect::MySql | PlanDialect::MariaDb) {
        return;
    }
    let is_fk = child.table.foreign_keys.iter().any(|key| {
        key.columns.len() == 1
            && key.columns[0].eq_ignore_ascii_case(&child.column.name)
            && key
                .referenced_table
                .eq_ignore_ascii_case(&parent.table.name)
            && key
                .referenced_columns
                .first()
                .is_none_or(|column| column.eq_ignore_ascii_case(&parent.column.name))
    });
    if !is_fk || leading_index(child.table, &child.column.name).is_some() {
        return;
    }
    // The plan has to pay for it: the child table is read without an index.
    if !matches!(ctx.op(child.position), Op::FullScan | Op::IndexFullScan) {
        return;
    }
    let scan = ctx.scan(child.position);
    let measured = scan.read.or(ctx.m(child.position).rows_out);
    let rows = measured.or(scan.est_read).unwrap_or(0.0);
    if rows < 1_000.0 {
        return;
    }
    let share = ctx.share(child.position);
    let severity = if measured.is_some() && ctx.matters(rows, share) {
        S::Warning
    } else {
        S::Info
    };
    let Some(rel) = ctx.rel(child.position) else {
        return;
    };
    let args: Args = vec![
        ("table", child.table.qualified_name()),
        ("column", child.column.name.clone()),
        ("parent", parent.table.qualified_name()),
        ("index_columns", String::new()),
    ];
    let mut suggestion = IndexSuggestion::for_rel(&rel);
    suggestion.equality.push(child.column.name.clone());
    let mut draft = Draft::new(K::SchemaFkUnindexed, severity, C::High, "join", args)
        .index(suggestion, "join_text")
        .at(child.position)
        .also(join)
        .ev("table", child.table.qualified_name())
        .ev(
            "columns",
            format!("{} references {}", child.display, parent.display),
        );
    draft = match measured {
        Some(rows) => draft.ev("rows_read", count(rows)),
        None => draft.ev("estimated_rows", count(rows)),
    };
    drafts.push(draft.ev_opt("time", ctx.m(child.position).inclusive_time_ms.map(ms)));
}

/// How two declared types differ for a comparison, if they do.
enum Mismatch {
    Type,
    Collation,
}

fn mismatch(dialect: PlanDialect, left: &SchemaColumn, right: &SchemaColumn) -> Option<Mismatch> {
    use TypeFamily as T;
    let (a, b) = (left.type_family(), right.type_family());
    let numeric = |family: &T| matches!(family, T::Integer | T::Decimal | T::Float);
    let text = |family: &T| matches!(family, T::Text { .. });
    match (&a, &b) {
        (T::Other, _) | (_, T::Other) => None,
        (T::Text { unicode: ua }, T::Text { unicode: ub }) => {
            // varchar against nvarchar converts the varchar side on SQL
            // Server; elsewhere the two are the same type.
            if dialect == PlanDialect::SqlServer && ua != ub {
                return Some(Mismatch::Type);
            }
            match (&left.collation, &right.collation) {
                (Some(x), Some(y)) if !x.eq_ignore_ascii_case(y) => Some(Mismatch::Collation),
                _ => None,
            }
        }
        // Different numeric types compare without losing index use on the
        // engines covered here; not worth a finding.
        _ if numeric(&a) && numeric(&b) => None,
        _ if (text(&a) && !text(&b)) || (!text(&a) && text(&b)) => Some(Mismatch::Type),
        _ if std::mem::discriminant(&a) != std::mem::discriminant(&b) => Some(Mismatch::Type),
        _ => None,
    }
}

fn type_mismatch(
    ctx: &Ctx<'_>,
    join: usize,
    left: &Resolved<'_>,
    right: &Resolved<'_>,
    drafts: &mut Vec<Draft>,
) {
    if std::ptr::eq(left.table, right.table) {
        return;
    }
    let Some(kind) = mismatch(ctx.dialect, left.column, right.column) else {
        return;
    };
    // The join has to carry enough rows for the conversion to matter.
    let rows_of = |position: usize| {
        let metrics = ctx.m(position);
        ctx.scan(position)
            .read
            .or(metrics.rows_out)
            .or(metrics.estimated_rows_total)
            .or(metrics.estimated_rows)
            .unwrap_or(0.0)
    };
    let rows = rows_of(left.position).max(rows_of(right.position));
    if rows < 1_000.0 {
        return;
    }
    let share = ctx.share(join);
    let severity = if ctx.has_actuals() && rows >= 10_000.0 && share.is_some_and(|s| s >= 0.1) {
        S::Warning
    } else {
        S::Info
    };
    let type_of = |column: &SchemaColumn| match (&kind, &column.collation) {
        (Mismatch::Collation, Some(collation)) => format!("{} {collation}", column.type_name),
        _ => column.type_name.clone(),
    };
    let args: Args = vec![
        ("left_column", left.display.clone()),
        ("left_type", type_of(left.column)),
        ("right_column", right.display.clone()),
        ("right_type", type_of(right.column)),
    ];
    let variant = match kind {
        Mismatch::Type => "join",
        Mismatch::Collation => "collation",
    };
    drafts.push(
        Draft::new(K::SchemaTypeMismatch, severity, C::High, variant, args)
            .at(join)
            .also(left.position)
            .also(right.position)
            .ev(
                "types",
                format!(
                    "{} {}, {} {}",
                    left.display,
                    type_of(left.column),
                    right.display,
                    type_of(right.column)
                ),
            )
            .ev_opt(
                "join_condition",
                ctx.a(join).join_predicate.as_ref().map(|p| p.text.clone()),
            )
            .ev_opt("time", ctx.m(join).inclusive_time_ms.map(ms)),
    );
}

/// Column references directly in front of `IS NULL` in a predicate.
fn is_null_columns(raw: &str) -> Vec<ColumnRef> {
    let upper = raw.to_ascii_uppercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = upper[from..].find(" IS NULL") {
        let end = from + found;
        let before = &raw[..end];
        let start = before
            .rfind(|c: char| c.is_whitespace() || c == '(' || c == ',')
            .map_or(0, |at| at + 1);
        let parts = split_qualified(&before[start..]);
        if let Some(name) = parts.last() {
            let plain = name.chars().all(|c| c.is_alphanumeric() || c == '_');
            if plain && !name.is_empty() {
                out.push(ColumnRef {
                    qualifier: (parts.len() >= 2).then(|| parts[parts.len() - 2].clone()),
                    name: name.clone(),
                });
            }
        }
        from = end + " IS NULL".len();
    }
    out
}

fn nullable_not_in<'a>(
    ctx: &Ctx<'a>,
    position: usize,
    reads: &[(usize, Rel<'a>)],
    drafts: &mut Vec<Draft>,
) {
    let analysis = ctx.a(position);
    let column: Option<Resolved<'a>> = match ctx.dialect {
        PlanDialect::SqlServer => {
            // NOT IN over a nullable column: an anti semi join whose
            // condition also tests the column for NULL.
            let anti = ctx
                .node(position)
                .logical_op
                .to_ascii_uppercase()
                .contains("ANTI SEMI JOIN");
            if !anti {
                return;
            }
            let mut texts: Vec<String> = Vec::new();
            let mut below = ctx.descendants(position);
            below.insert(0, position);
            for at in below {
                let a = ctx.a(at);
                for predicate in [&a.join_predicate, &a.filter, &a.seek_predicate]
                    .into_iter()
                    .flatten()
                {
                    texts.push(predicate.raw.clone());
                }
            }
            texts
                .iter()
                .flat_map(|text| is_null_columns(text))
                .filter_map(|column| resolve(ctx, reads, &column))
                .find(|resolved| resolved.column.nullable)
        }
        PlanDialect::Postgres => {
            // PostgreSQL never turns NOT IN into an anti-join; the filter
            // names the subplan it probes instead.
            let Some(filter) = analysis.filter.as_ref() else {
                return;
            };
            let raw = filter.raw.as_str();
            let Some(start) = raw
                .find("NOT (hashed SubPlan ")
                .or_else(|| raw.find("NOT (SubPlan "))
            else {
                return;
            };
            let name: String = raw[start..]
                .split("SubPlan ")
                .nth(1)
                .unwrap_or("")
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if name.is_empty() {
                return;
            }
            let label = format!("SubPlan {name}");
            let Some(subplan) = (0..ctx.len())
                .find(|at| ctx.node(*at).subplan_name.as_deref() == Some(label.as_str()))
            else {
                return;
            };
            // The subquery's single output column, when the plan lists it.
            let outputs = &ctx.node(subplan).output_columns;
            let [output] = outputs.as_slice() else {
                return;
            };
            let parts = split_qualified(output);
            let Some(name) = parts.last().cloned() else {
                return;
            };
            let reference = ColumnRef {
                qualifier: (parts.len() >= 2).then(|| parts[parts.len() - 2].clone()),
                name,
            };
            let mut inside = ctx.descendants(subplan);
            inside.insert(0, subplan);
            let inner_reads: Vec<(usize, Rel<'a>)> = inside
                .into_iter()
                .filter_map(|at| ctx.rel(at).map(|rel| (at, rel)))
                .collect();
            resolve(ctx, &inner_reads, &reference).filter(|resolved| resolved.column.nullable)
        }
        _ => None,
    };
    let Some(column) = column else {
        return;
    };
    let rows = ctx
        .kids(position)
        .iter()
        .filter_map(|kid| ctx.m(*kid).rows_out.or(ctx.m(*kid).estimated_rows))
        .fold(ctx.m(position).rows_in.unwrap_or(0.0), f64::max);
    if rows < 1_000.0 {
        return;
    }
    let share = ctx.share(position);
    let severity = if ctx.has_actuals() && share.is_some_and(|share| share >= 0.1) {
        S::Warning
    } else {
        S::Info
    };
    let args: Args = vec![("column", column.display.clone())];
    drafts.push(
        Draft::new(K::SchemaNullableNotIn, severity, C::Medium, "anti", args)
            .at(position)
            .also(column.position)
            .ev(
                "types",
                format!("{} {} NULL", column.display, column.column.type_name),
            )
            .ev_opt("filter", analysis.filter.as_ref().map(|f| f.text.clone()))
            .ev_opt(
                "join_condition",
                analysis.join_predicate.as_ref().map(|p| p.text.clone()),
            )
            .ev_opt("time", ctx.m(position).inclusive_time_ms.map(ms)),
    );
}

fn heap_table(
    ctx: &Ctx<'_>,
    position: usize,
    rel: &Rel<'_>,
    drafts: &mut Vec<Draft>,
    seen: &mut Vec<String>,
) {
    let Some(table) = rel.table else { return };
    if !table.has_columns() {
        return;
    }
    let key = table.qualified_name().to_ascii_lowercase();
    if seen.contains(&key) {
        return;
    }
    let variant = match ctx.dialect {
        PlanDialect::SqlServer => {
            let clustered = table.all_indexes().any(|index| {
                index.clustered == Some(true) || (index.primary && index.clustered.is_none())
            }) || table.all_indexes().any(|index| {
                index
                    .method
                    .as_deref()
                    .is_some_and(|m| m.contains("columnstore"))
            });
            if clustered {
                return;
            }
            "heap"
        }
        _ => {
            if table.primary_key.is_some() || table.all_indexes().any(|index| index.unique) {
                return;
            }
            "no_primary_key"
        }
    };
    if !matches!(
        ctx.op(position),
        Op::FullScan | Op::RowLookup | Op::IndexSeek | Op::IndexFullScan | Op::BitmapHeap
    ) {
        return;
    }
    let scan = ctx.scan(position);
    let measured = scan.read.or(ctx.m(position).rows_out);
    let Some(rows) = measured.or(scan.est_read).filter(|rows| *rows >= 10_000.0) else {
        return;
    };
    seen.push(key);
    let args: Args = vec![("table", rel.display()), ("rows", count(rows))];
    let mut draft = Draft::new(K::SchemaHeapTable, S::Info, C::High, variant, args)
        .at(position)
        .ev("table", rel.display());
    draft = match measured {
        Some(rows) => draft.ev("rows_read", count(rows)),
        None => draft.ev("estimated_rows", count(rows)),
    };
    drafts.push(draft.no_impact());
}

fn wide_index(
    ctx: &Ctx<'_>,
    position: usize,
    rel: &Rel<'_>,
    drafts: &mut Vec<Draft>,
    seen: &mut Vec<String>,
) {
    let Some(table) = rel.table else { return };
    let Some(name) = ctx.a(position).index.as_deref() else {
        return;
    };
    let Some(index) = table.index(name) else {
        return;
    };
    // The clustered index or primary key is the table, not a copy of it.
    if index.primary || index.clustered == Some(true) {
        return;
    }
    let key = format!("{}.{}", table.qualified_name(), index.name).to_ascii_lowercase();
    if seen.contains(&key) {
        return;
    }
    let key_count = index.columns.len();
    let stored: Vec<&str> = index.stored_columns().collect();
    let column_count = table.columns.len();
    let variant = if key_count >= 6 {
        "keys"
    } else if stored.len() >= 10 && column_count > 0 && stored.len() * 10 >= column_count * 7 {
        "copy"
    } else {
        return;
    };
    seen.push(key);
    let args: Args = vec![
        ("index", index.name.clone()),
        ("table", rel.display()),
        ("key_count", key_count.to_string()),
        ("stored_count", stored.len().to_string()),
        ("column_count", column_count.to_string()),
    ];
    drafts.push(
        Draft::new(K::SchemaWideIndex, S::Info, C::Medium, variant, args)
            .at(position)
            .no_impact()
            .ev("table", rel.display())
            .ev(
                "existing_index",
                format!("{} ({})", index.name, index.key_names().join(", ")),
            )
            .ev_opt(
                "columns",
                (!index.include_columns.is_empty()).then(|| index.include_columns.join(", ")),
            ),
    );
}

fn stale(ctx: &Ctx<'_>, reads: &[(usize, Rel<'_>)], drafts: &mut Vec<Draft>) {
    let Some(schema) = ctx.schema else { return };
    // Only a schema that describes at least one table of this plan can be
    // called out of date; otherwise it is simply a different database.
    let matched = reads.iter().filter(|(_, rel)| rel.table.is_some()).count();
    let mut items: Vec<String> = Vec::new();
    let mut nodes: Vec<usize> = Vec::new();
    let mut push = |items: &mut Vec<String>, item: String, position: usize| {
        if !items.iter().any(|seen| seen.eq_ignore_ascii_case(&item)) {
            items.push(item);
            nodes.push(position);
        }
    };
    for (position, rel) in reads {
        match rel.table {
            None => {
                // Engines that print the query's alias cannot tell an
                // unknown table from an alias; temporary objects are never
                // part of a schema export.
                let temporary = rel.name.starts_with('#') || rel.name.starts_with('@');
                if rel.alias_only || temporary || matched == 0 || schema.tables.len() < 2 {
                    continue;
                }
                push(&mut items, format!("table {}", rel.display()), *position);
            }
            Some(table) => {
                if let Some(index) = ctx.a(*position).index.as_deref()
                    && !table.indexes.is_empty()
                    && table.index(index).is_none()
                    && !is_implicit_index(index)
                {
                    push(
                        &mut items,
                        format!("index {index} on {}", rel.display()),
                        *position,
                    );
                }
                if !table.has_columns() {
                    continue;
                }
                let analysis = ctx.a(*position);
                for predicate in [&analysis.filter, &analysis.seek_predicate]
                    .into_iter()
                    .flatten()
                {
                    for term in ctx.terms(&predicate.raw) {
                        for side in [&term.left, &term.right] {
                            let Some(column) = side.column() else {
                                continue;
                            };
                            // Only a column the plan ties to this table.
                            if column.qualifier.is_none() || !rel.owns(column) {
                                continue;
                            }
                            if table.column(&column.name).is_none() {
                                push(
                                    &mut items,
                                    format!("column {}.{}", rel.display(), column.name),
                                    *position,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    if items.is_empty() {
        return;
    }
    let listed = if items.len() > 6 {
        format!("{}, and {} more", items[..6].join(", "), items.len() - 6)
    } else {
        items.join(", ")
    };
    let mut draft = Draft::new(
        K::SchemaStale,
        S::Info,
        C::Medium,
        "mismatch",
        vec![("items", listed)],
    );
    for item in &items {
        draft = draft.ev("warning", item.clone());
    }
    for position in nodes.into_iter().take(8) {
        draft = draft.also(position);
    }
    drafts.push(draft);
}

/// Index names the engine makes up for constraints, which a schema export
/// does not spell the same way.
fn is_implicit_index(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "primary"
        || lower.starts_with("sqlite_autoindex_")
        || lower.starts_with("pk__")
        || lower.starts_with("uq__")
        || lower.starts_with("sys_c")
        || lower.ends_with("_pkey")
        || lower.ends_with("_key")
}
