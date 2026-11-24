//! Rules about predicates an index cannot serve: a column wrapped in a
//! function or a conversion, and `LIKE` patterns with a leading wildcard.

use super::advice::{self, IndexAdvice, IndexSuggestion};
use super::facts::{Ctx, Op, Rel, Scan, count};
use super::predicate::{ColumnRef, Side, Term, TermKind, Wrap};
use super::rules_access::with_scan;
use super::strings::{Args, Template};
use super::{Draft, FindingConfidence as C, FindingKind as K, FindingSeverity as S};
use crate::plans::PlanDialect;
use crate::plans::schema::TypeFamily;

pub(super) const TEMPLATES: &[Template] = &[
    // ---- implicit_conversion
    (
        "implicit_conversion.scan.summary",
        "SQL Server converts {column} to {type} on every row of {table} before comparing it with {value}: {rows_read} rows read, {rows_kept} kept.",
        "summary: SQL Server implicit conversion on a column, with measured rows",
    ),
    (
        "implicit_conversion.scan.why",
        "The column is the side being converted, so an index on {column} cannot be used for a seek. SQL Server converts the side whose data type has lower precedence. Here that is the column, which means the value it is compared with does not have the column's type.",
        "why an implicit conversion on a column costs time in SQL Server, in the finding with measured rows",
    ),
    (
        "implicit_conversion.scan.action",
        "Pass the value with the column's own type: declare the parameter or variable with the type of {column}, or cast the value instead of the column. Client libraries often send string parameters as nvarchar unless told otherwise.",
        "action: make the compared value match the column type, in the SQL Server finding with measured rows",
    ),
    (
        "implicit_conversion.column.summary",
        "SQL Server converts {column} to {type} before comparing it with {value} in the read of {table}.",
        "summary: SQL Server implicit conversion on a column, no measured volume",
    ),
    (
        "implicit_conversion.column.why",
        "The column is the side being converted, so an index on {column} cannot be used for a seek. SQL Server converts the side whose data type has lower precedence. Here that is the column, which means the value it is compared with does not have the column's type.",
        "why an implicit conversion on a column costs time in SQL Server, in the finding without measured rows",
    ),
    (
        "implicit_conversion.column.action",
        "Pass the value with the column's own type: declare the parameter or variable with the type of {column}, or cast the value instead of the column. Client libraries often send string parameters as nvarchar unless told otherwise.",
        "action: make the compared value match the column type, in the SQL Server finding without measured rows",
    ),
    (
        "implicit_conversion.match_type_known",
        "Pass the value as {column_type}, the declared type of {column}: declare the parameter or variable that way, or cast the value instead of the column. Client libraries often send string parameters as nvarchar unless told otherwise.",
        "action when the schema gives the column's declared type",
    ),
    (
        "implicit_conversion.needs_schema",
        "With the table definitions supplied, this advice could name the column's declared type and write the cast for you.",
        "note under an implicit conversion finding saying what supplying the schema would add",
    ),
    (
        "implicit_conversion.cast.summary",
        "The filter on {table} casts {column} to {type} before comparing it: {rows_read} rows read, {rows_kept} kept.",
        "summary: PostgreSQL cast on a column in a filter, with measured rows",
    ),
    (
        "implicit_conversion.cast.why",
        "An index on {column} holds the column's own values, so it cannot serve a comparison on the cast value. The cast is either written in the query or added by PostgreSQL because the compared value has a different type than the column.",
        "why a cast on a column costs time in PostgreSQL, in the finding with measured rows",
    ),
    (
        "implicit_conversion.cast.action",
        "Compare {column} with a value of the column's own type so the cast is not needed.",
        "action: remove the cast on the column, in the PostgreSQL finding with measured rows",
    ),
    (
        "implicit_conversion.cast_unmeasured.summary",
        "The filter on {table} casts {column} to {type} before comparing it.",
        "summary: PostgreSQL cast on a column in a filter, estimated plan",
    ),
    (
        "implicit_conversion.cast_unmeasured.why",
        "An index on {column} holds the column's own values, so it cannot serve a comparison on the cast value. The cast is either written in the query or added by PostgreSQL because the compared value has a different type than the column.",
        "why a cast on a column costs time in PostgreSQL, in the finding without measured rows",
    ),
    (
        "implicit_conversion.cast_unmeasured.action",
        "Compare {column} with a value of the column's own type so the cast is not needed.",
        "action: remove the cast on the column, in the PostgreSQL finding without measured rows",
    ),
    (
        "implicit_conversion.alt_expression_index",
        "If the cast has to stay, index the cast expression.",
        "alternative: expression index on the cast",
    ),
    (
        "implicit_conversion.oracle.summary",
        "Oracle applies {expression} to every row of {table} before comparing it: {rows_read} rows read, {rows_kept} kept.",
        "summary: Oracle conversion function on a column, with measured rows",
    ),
    (
        "implicit_conversion.oracle.why",
        "The conversion is applied to the column, so an ordinary index on {column} cannot be used for this condition. Oracle adds such a conversion when the column and the compared value have different types, for example a VARCHAR2 column compared with a number.",
        "why a conversion on a column costs time in Oracle, in the finding with measured rows",
    ),
    (
        "implicit_conversion.oracle.action",
        "Compare {column} with a value of the column's own type: quote the literal, or bind a variable of the matching type.",
        "action: make the compared value match the column type in Oracle, in the finding with measured rows",
    ),
    (
        "implicit_conversion.oracle_unmeasured.summary",
        "Oracle applies {expression} to the rows of {table} before comparing them.",
        "summary: Oracle conversion function on a column, no measured rows",
    ),
    (
        "implicit_conversion.oracle_unmeasured.why",
        "The conversion is applied to the column, so an ordinary index on {column} cannot be used for this condition. Oracle adds such a conversion when the column and the compared value have different types, for example a VARCHAR2 column compared with a number.",
        "why a conversion on a column costs time in Oracle, in the finding without measured rows",
    ),
    (
        "implicit_conversion.oracle_unmeasured.action",
        "Compare {column} with a value of the column's own type: quote the literal, or bind a variable of the matching type.",
        "action: make the compared value match the column type in Oracle, in the finding without measured rows",
    ),
    // ---- non_sargable
    (
        "non_sargable.function.summary",
        "The filter on {table} computes {expression} for every row before comparing it: {rows_read} rows read, {rows_kept} kept.",
        "summary: a function or arithmetic on a column in a filter, with measured rows",
    ),
    (
        "non_sargable.function.why",
        "An index on {column} is ordered by the column's stored values, not by {expression}, so the engine cannot seek on it. It reads each row, computes the expression and tests the result.",
        "why a function on a column costs time, in the finding with measured rows",
    ),
    (
        "non_sargable.function.action",
        "Create an index on the expression itself, so the engine can seek on the computed value.",
        "action: create an expression index, in the finding with measured rows",
    ),
    (
        "non_sargable.function_unmeasured.summary",
        "The filter on {table} computes {expression} for every row before comparing it.",
        "summary: a function or arithmetic on a column in a filter, estimated plan",
    ),
    (
        "non_sargable.function_unmeasured.why",
        "An index on {column} is ordered by the column's stored values, not by {expression}, so the engine cannot seek on it. It reads each row, computes the expression and tests the result.",
        "why a function on a column costs time, in the finding without measured rows",
    ),
    (
        "non_sargable.function_unmeasured.action",
        "Create an index on the expression itself, so the engine can seek on the computed value.",
        "action: create an expression index, in the finding without measured rows",
    ),
    (
        "non_sargable.rewrite",
        "Rewrite the condition as a range on the bare column, which an index on {column} can serve.",
        "action: rewrite the predicate; the rewritten predicate is shown as SQL",
    ),
    (
        "non_sargable.computed_column",
        "Rewrite the condition so {column} is compared directly. If the expression has to stay, add a computed (generated) column for {expression} and index that column.",
        "action for engines without expression indexes",
    ),
    (
        "non_sargable.text_only",
        "Rewrite the condition so {column} is compared directly, or create an index on the expression {expression}.",
        "action when no statement can be written",
    ),
    (
        "non_sargable.alt_expression_index",
        "Or keep the query as it is and index the expression.",
        "alternative: expression index",
    ),
    (
        "non_sargable.alt_plain_index",
        "The rewritten condition needs an index that starts with {column}.",
        "alternative shown with a rewritten predicate: the index it needs",
    ),
    (
        "non_sargable.note_mysql",
        "Indexes on expressions (functional key parts) need MySQL 8.0.13 or later.",
        "note under an expression index suggestion for MySQL",
    ),
    (
        "non_sargable.like.summary",
        "The filter on {table} matches {column} against a pattern that starts with a wildcard ({pattern}): {rows_read} rows read, {rows_kept} kept.",
        "summary: LIKE with a leading wildcard, with measured rows",
    ),
    (
        "non_sargable.like.why",
        "A B-tree index is ordered from the first character, so it helps only when the pattern fixes the start of the value. With a leading wildcard every row is read and matched.",
        "why a leading-wildcard LIKE costs time, in the finding with measured rows",
    ),
    (
        "non_sargable.like.action",
        "If the pattern can be anchored at the start ('abc%'), an ordinary index on {column} becomes usable. To search for words inside the text, use a full-text index instead of LIKE.",
        "action for a leading-wildcard LIKE, in the finding with measured rows",
    ),
    (
        "non_sargable.like_unmeasured.summary",
        "The filter on {table} matches {column} against a pattern that starts with a wildcard ({pattern}).",
        "summary: LIKE with a leading wildcard, estimated plan",
    ),
    (
        "non_sargable.like_unmeasured.why",
        "A B-tree index is ordered from the first character, so it helps only when the pattern fixes the start of the value. With a leading wildcard every row is read and matched.",
        "why a leading-wildcard LIKE costs time, in the finding without measured rows",
    ),
    (
        "non_sargable.like_unmeasured.action",
        "If the pattern can be anchored at the start ('abc%'), an ordinary index on {column} becomes usable. To search for words inside the text, use a full-text index instead of LIKE.",
        "action for a leading-wildcard LIKE, in the finding without measured rows",
    ),
    (
        "non_sargable.trigram",
        "If the search has to match anywhere in the value, a trigram index supports it. It needs the pg_trgm extension. If the pattern can be anchored at the start ('abc%'), an ordinary index is enough.",
        "action for a leading-wildcard LIKE on PostgreSQL",
    ),
];

/// How much work the scan at `position` does, as far as the plan shows.
pub(super) enum Volume {
    /// Measured rows read and kept.
    Measured { read: f64, kept: f64, severity: S },
    /// No measurement; the optimizer's numbers say the scan is a large part
    /// of the statement.
    Estimated,
    /// Too small, or not selective, or unknown.
    None,
}

pub(super) fn scan_volume(ctx: &Ctx<'_>, position: usize, scan: &Scan) -> Volume {
    let share = ctx.share(scan.top);
    if let (Some(read), Some(kept)) = (scan.read, scan.kept) {
        if ctx.matters(read, share) && kept <= read * 0.1 {
            let severity = if share.is_some_and(|share| share >= 0.5) && read >= 100_000.0 {
                S::Critical
            } else {
                S::Warning
            };
            return Volume::Measured {
                read,
                kept,
                severity,
            };
        }
        return Volume::None;
    }
    if ctx.has_actuals() || !ctx.an.has_costs || !share.is_some_and(|share| share >= 0.1) {
        return Volume::None;
    }
    let large = match scan.est_read {
        Some(rows) => rows >= 10_000.0,
        // PostgreSQL does not print the table size on an estimated plan; a
        // scan cost of 1,000 is tens of thousands of rows at default costs.
        None => {
            ctx.dialect == PlanDialect::Postgres
                && ctx
                    .m(position)
                    .inclusive_cost
                    .is_some_and(|cost| cost >= 1_000.0)
        }
    };
    if large {
        Volume::Estimated
    } else {
        Volume::None
    }
}

pub(super) fn run(ctx: &Ctx<'_>, drafts: &mut Vec<Draft>) {
    for position in 0..ctx.len() {
        if !ctx.executed(position)
            || !matches!(ctx.op(position), Op::FullScan | Op::IndexFullScan)
            || matches!(ctx.dialect, PlanDialect::DuckDb | PlanDialect::ClickHouse)
        {
            continue;
        }
        let Some(rel) = ctx.rel(position) else {
            continue;
        };
        let scan = ctx.scan(position);
        let Some(filter) = scan.filter_raw.clone() else {
            continue;
        };
        let terms = ctx.terms(&filter);
        // One finding per scan: the first conversion, else the first
        // function, else the first leading-wildcard LIKE.
        let conversion = terms.iter().find(|term| is_conversion(ctx, &rel, term));
        if let Some(term) = conversion {
            if let Some(draft) = implicit_conversion(ctx, position, &rel, &scan, term) {
                drafts.push(draft);
            }
            continue;
        }
        let function = terms.iter().find(|term| is_function(ctx, &rel, term));
        if let Some(term) = function {
            if let Some(draft) = function_on_column(ctx, position, &rel, &scan, term) {
                drafts.push(draft);
            }
            continue;
        }
        let like = terms.iter().find(|term| {
            term.leading_wildcard()
                && !term.negated
                && term.bare_column().is_some_and(|column| rel.owns(column))
        });
        if let Some(term) = like
            && let Some(draft) = leading_wildcard(ctx, position, &rel, &scan, term)
        {
            drafts.push(draft);
        }
    }
}

/// The other side of a wrapped comparison is a constant or parameter.
fn against_constant(term: &Term) -> Option<&str> {
    match (&term.left, &term.right) {
        (Side::Wrapped { .. }, Side::Constant(value))
        | (Side::Constant(value), Side::Wrapped { .. }) => Some(value.as_str()),
        _ => None,
    }
}

fn comparison(term: &Term) -> bool {
    matches!(
        term.kind,
        TermKind::Equality | TermKind::Range | TermKind::In | TermKind::Like
    ) && !term.negated
}

fn is_conversion(ctx: &Ctx<'_>, rel: &Rel<'_>, term: &Term) -> bool {
    if !comparison(term) || against_constant(term).is_none() {
        return false;
    }
    let Some((column, Wrap::Cast { ty, implicit }, _)) = term.wrapped() else {
        return false;
    };
    if !rel.owns(column) {
        return false;
    }
    match ctx.dialect {
        PlanDialect::SqlServer | PlanDialect::Oracle => *implicit,
        // PostgreSQL prints varchar columns as (col)::text; that cast is
        // free and an index on the column still serves it.
        PlanDialect::Postgres => !matches!(
            TypeFamily::of(ty),
            TypeFamily::Text { .. } | TypeFamily::Other
        ),
        _ => false,
    }
}

fn is_function(ctx: &Ctx<'_>, rel: &Rel<'_>, term: &Term) -> bool {
    if !comparison(term) || against_constant(term).is_none() {
        return false;
    }
    let Some((column, wrap, _)) = term.wrapped() else {
        return false;
    };
    if !rel.owns(column) {
        return false;
    }
    match wrap {
        Wrap::Function(_) | Wrap::Arithmetic => true,
        Wrap::Cast { ty, implicit } => match ctx.dialect {
            // SQL Server can still seek through CAST(datetime AS date).
            PlanDialect::SqlServer => !implicit && !ty.to_ascii_lowercase().contains("date"),
            PlanDialect::MySql | PlanDialect::MariaDb | PlanDialect::Sqlite => true,
            _ => false,
        },
    }
}

fn column_text(column: &ColumnRef) -> String {
    match &column.qualifier {
        Some(qualifier) => format!("{qualifier}.{}", column.name),
        None => column.name.clone(),
    }
}

fn plain_value(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .filter(|inner| !inner.contains('['))
        .unwrap_or(trimmed)
        .to_string()
}

/// `text` with this table's qualifier removed, or `None` when a qualifier
/// that is not this table's remains (the expression could not go into a
/// statement on this table as written).
fn unqualified(text: &str, rel: &Rel<'_>, column: &ColumnRef) -> Option<String> {
    let mut out = text.to_string();
    let mut names: Vec<&str> = vec![rel.name.as_str()];
    names.extend(rel.alias.as_deref());
    names.extend(column.qualifier.as_deref());
    for name in names {
        for pattern in [
            format!("[{name}]."),
            format!("\"{name}\"."),
            format!("`{name}`."),
            format!("{name}."),
        ] {
            let mut result = String::with_capacity(out.len());
            let lower = out.to_ascii_lowercase();
            let needle = pattern.to_ascii_lowercase();
            let mut index = 0;
            while let Some(found) = lower[index..].find(&needle) {
                let start = index + found;
                let boundary = out[..start]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'));
                result.push_str(&out[index..start]);
                if !boundary {
                    result.push_str(&out[start..start + pattern.len()]);
                }
                index = start + pattern.len();
            }
            result.push_str(&out[index..]);
            out = result;
        }
    }
    // Any identifier.identifier left is someone else's qualifier.
    let chars: Vec<char> = out.chars().collect();
    for (position, ch) in chars.iter().enumerate() {
        if *ch != '.' {
            continue;
        }
        let before = position.checked_sub(1).map(|p| chars[p]);
        let after = chars.get(position + 1).copied();
        let numeric =
            before.is_some_and(|c| c.is_ascii_digit()) || after.is_some_and(|c| c.is_ascii_digit());
        if !numeric {
            return None;
        }
    }
    Some(out)
}

fn expression_index(
    ctx: &Ctx<'_>,
    rel: &Rel<'_>,
    column: &ColumnRef,
    expression: &str,
) -> Option<String> {
    if rel.alias_only
        || !matches!(
            ctx.dialect,
            PlanDialect::Postgres | PlanDialect::MySql | PlanDialect::Oracle | PlanDialect::Sqlite
        )
    {
        return None;
    }
    let text = unqualified(expression, rel, column)?;
    let is_call = text.ends_with(')')
        && text.split_once('(').is_some_and(|(name, _)| {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    let key = if is_call || ctx.dialect == PlanDialect::MySql {
        text
    } else {
        format!("({text})")
    };
    let mut suggestion = IndexSuggestion::for_rel(rel);
    suggestion.expression = Some(key);
    match advice::index_advice(ctx, &suggestion) {
        IndexAdvice::Create(built) => Some(built.sql),
        _ => None,
    }
}

fn measured_args(args: &mut Args, read: f64, kept: f64) {
    args.push(("rows_read", count(read)));
    args.push(("rows_kept", count(kept)));
}

fn implicit_conversion(
    ctx: &Ctx<'_>,
    position: usize,
    rel: &Rel<'_>,
    scan: &Scan,
    term: &Term,
) -> Option<Draft> {
    let (column, Wrap::Cast { ty, .. }, text) = term.wrapped()? else {
        return None;
    };
    let value = plain_value(against_constant(term)?);
    let volume = scan_volume(ctx, position, scan);
    let mut args: Args = vec![
        ("table", rel.display()),
        ("column", column_text(column)),
        ("type", ty.clone()),
        ("value", value.clone()),
        ("expression", text.to_string()),
    ];
    let known_type = rel
        .table
        .and_then(|table| table.column(&column.name))
        .map(|column| column.type_name.clone())
        .filter(|ty| !ty.is_empty());
    let draft = match ctx.dialect {
        PlanDialect::SqlServer => {
            let (variant, severity, confidence) = match volume {
                Volume::Measured {
                    read,
                    kept,
                    severity,
                } => {
                    measured_args(&mut args, read, kept);
                    ("scan", severity, C::High)
                }
                Volume::Estimated => ("column", S::Warning, C::Medium),
                Volume::None => ("column", S::Info, C::High),
            };
            let mut draft = Draft::new(K::ImplicitConversion, severity, confidence, variant, args);
            if let Some(column_type) = known_type {
                let rewritten = (term.kind == TermKind::Equality)
                    .then(|| format!("{} = CAST({value} AS {column_type})", column_text(column)));
                draft = draft
                    .arg("column_type", column_type.clone())
                    .action("match_type_known")
                    .maybe_sql(rewritten)
                    .ev("types", format!("{}: {column_type}", column.name));
            } else if ctx.schema.is_none() {
                draft = draft.note("implicit_conversion.needs_schema", &[]);
            }
            draft
        }
        PlanDialect::Postgres => {
            let (variant, severity, confidence) = match volume {
                Volume::Measured {
                    read,
                    kept,
                    severity,
                } => {
                    measured_args(&mut args, read, kept);
                    ("cast", severity, C::Medium)
                }
                Volume::Estimated => ("cast_unmeasured", S::Warning, C::Low),
                Volume::None => return None,
            };
            let index = expression_index(ctx, rel, column, text);
            let mut draft = Draft::new(K::ImplicitConversion, severity, confidence, variant, args);
            if index.is_some() {
                draft = draft.alt("alt_expression_index", index);
            }
            if let Some(column_type) = known_type {
                draft = draft.ev("types", format!("{}: {column_type}", column.name));
            }
            draft
        }
        PlanDialect::Oracle => {
            let (variant, severity, confidence) = match volume {
                Volume::Measured {
                    read,
                    kept,
                    severity,
                } => {
                    measured_args(&mut args, read, kept);
                    ("oracle", severity, C::High)
                }
                Volume::Estimated => ("oracle_unmeasured", S::Warning, C::Medium),
                Volume::None => ("oracle_unmeasured", S::Info, C::Medium),
            };
            let mut draft = Draft::new(K::ImplicitConversion, severity, confidence, variant, args);
            if let Some(column_type) = known_type {
                draft = draft.ev("types", format!("{}: {column_type}", column.name));
            }
            draft
        }
        _ => return None,
    };
    let draft = if matches!(scan_volume(ctx, position, scan), Volume::Estimated) {
        draft.note("common.note.estimated_only", &[])
    } else {
        draft
    };
    Some(with_scan(
        draft.at(scan.top).also(position),
        ctx,
        position,
        scan,
    ))
}

/// `YEAR(col) = 2024` and `DATE(col) = '2024-05-01'` as ranges on the column.
fn range_rewrite(
    ctx: &Ctx<'_>,
    column: &ColumnRef,
    wrap: &Wrap,
    text: &str,
    term: &Term,
) -> Option<String> {
    if term.kind != TermKind::Equality {
        return None;
    }
    // MySQL and MariaDB print DATE(col) as cast(col as date); SQL Server
    // prints YEAR(col) as datepart(year, col).
    let upper = match wrap {
        Wrap::Function(name) if name.eq_ignore_ascii_case("datepart") => {
            let part = text.split_once('(')?.1.split_once(',')?.0.trim();
            if !matches!(part.to_ascii_lowercase().as_str(), "year" | "yy" | "yyyy") {
                return None;
            }
            "YEAR".to_string()
        }
        Wrap::Function(name) => name.to_ascii_uppercase(),
        Wrap::Cast { ty, .. } if ty.trim().eq_ignore_ascii_case("date") => "DATE".to_string(),
        _ => return None,
    };
    let value = against_constant(term)?.trim();
    let column = column_text(column);
    if upper == "YEAR"
        && matches!(
            ctx.dialect,
            PlanDialect::SqlServer | PlanDialect::MySql | PlanDialect::MariaDb
        )
    {
        let year: u32 = value.trim_matches(|c| c == '(' || c == ')').parse().ok()?;
        if !(1000..=9998).contains(&year) {
            return None;
        }
        let (from, to) = if ctx.dialect == PlanDialect::SqlServer {
            (format!("'{year}0101'"), format!("'{}0101'", year + 1))
        } else {
            (format!("'{year}-01-01'"), format!("'{}-01-01'", year + 1))
        };
        return Some(format!("{column} >= {from} AND {column} < {to}"));
    }
    if upper == "DATE" && matches!(ctx.dialect, PlanDialect::MySql | PlanDialect::MariaDb) {
        let literal = value.strip_prefix("DATE").map(str::trim).unwrap_or(value);
        let inner = literal.strip_prefix('\'')?.strip_suffix('\'')?;
        let bytes = inner.as_bytes();
        let shaped = bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit());
        if !shaped {
            return None;
        }
        return Some(format!(
            "{column} >= '{inner}' AND {column} < '{inner}' + INTERVAL 1 DAY"
        ));
    }
    None
}

fn function_on_column(
    ctx: &Ctx<'_>,
    position: usize,
    rel: &Rel<'_>,
    scan: &Scan,
    term: &Term,
) -> Option<Draft> {
    let (column, wrap, text) = term.wrapped()?;
    let mut args: Args = vec![
        ("table", rel.display()),
        ("column", column_text(column)),
        ("expression", text.to_string()),
    ];
    let volume = scan_volume(ctx, position, scan);
    let (variant, severity, confidence) = match volume {
        Volume::Measured {
            read,
            kept,
            severity,
        } => {
            measured_args(&mut args, read, kept);
            ("function", severity, C::High)
        }
        Volume::Estimated => ("function_unmeasured", S::Warning, C::Medium),
        Volume::None => return None,
    };
    let rewrite = range_rewrite(ctx, column, wrap, text, term);
    let index = expression_index(ctx, rel, column, text);
    let mut draft = Draft::new(K::NonSargable, severity, confidence, variant, args);
    if let Some(rewrite) = rewrite {
        draft = draft.action("rewrite").sql(rewrite);
        // The rewritten condition still needs an index on the column.
        if !rel.alias_only {
            let mut suggestion = IndexSuggestion::for_rel(rel);
            suggestion.range.push(column.name.clone());
            match advice::index_advice(ctx, &suggestion) {
                IndexAdvice::Create(built) => {
                    draft = draft.alt("alt_plain_index", Some(built.sql));
                }
                IndexAdvice::Exists(existing) => {
                    draft = draft.ev("existing_index", existing);
                }
                IndexAdvice::None => {}
            }
        }
        if index.is_some() {
            draft = draft.alt("alt_expression_index", index);
        }
    } else if let Some(sql) = index {
        draft = draft.sql(sql);
        if ctx.dialect == PlanDialect::MySql {
            draft = draft.note("non_sargable.note_mysql", &[]);
        }
        draft = draft.note("common.note.write_cost", &[("table", rel.display())]);
    } else if matches!(ctx.dialect, PlanDialect::SqlServer | PlanDialect::MariaDb) {
        draft = draft.action("computed_column");
    } else {
        draft = draft.action("text_only");
        if rel.alias_only {
            draft = draft.note("common.note.alias", &[("table", rel.display())]);
        }
    }
    if matches!(volume, Volume::Estimated) {
        draft = draft.note("common.note.estimated_only", &[]);
    }
    Some(with_scan(
        draft.at(scan.top).also(position),
        ctx,
        position,
        scan,
    ))
}

fn leading_wildcard(
    ctx: &Ctx<'_>,
    position: usize,
    rel: &Rel<'_>,
    scan: &Scan,
    term: &Term,
) -> Option<Draft> {
    let column = term.bare_column()?;
    let pattern = term.pattern.clone()?;
    let mut args: Args = vec![
        ("table", rel.display()),
        ("column", column_text(column)),
        ("pattern", format!("'{pattern}'")),
    ];
    let volume = scan_volume(ctx, position, scan);
    let (variant, severity, confidence) = match volume {
        Volume::Measured {
            read,
            kept,
            severity,
        } => {
            measured_args(&mut args, read, kept);
            ("like", severity, C::High)
        }
        Volume::Estimated => ("like_unmeasured", S::Warning, C::Medium),
        Volume::None => return None,
    };
    let mut draft = Draft::new(K::NonSargable, severity, confidence, variant, args);
    if ctx.dialect == PlanDialect::Postgres
        && let Some(table) = advice::table_sql(ctx, rel)
    {
        let confirmed = rel
            .table
            .is_none_or(|table| !table.has_columns() || table.column(&column.name).is_some());
        if confirmed {
            let name = format!(
                "ix_{}_{}_trgm",
                rel.name.to_ascii_lowercase(),
                column.name.to_ascii_lowercase()
            );
            draft = draft
                .action("trigram")
                .sql(format!(
                    "CREATE EXTENSION IF NOT EXISTS pg_trgm;\nCREATE INDEX {} ON {table} USING gin ({} gin_trgm_ops);",
                    advice::quote(ctx, &name),
                    advice::quote(ctx, &column.name),
                ))
                .note("common.note.write_cost", &[("table", rel.display())]);
        }
    }
    if matches!(volume, Volume::Estimated) {
        draft = draft.note("common.note.estimated_only", &[]);
    }
    Some(with_scan(
        draft.at(scan.top).also(position),
        ctx,
        position,
        scan,
    ))
}
