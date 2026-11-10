//! Value-mode completion (§8.2/§8.3): plan a live database query for the slot
//! at the cursor (with FK pivoting to (key, display) pairs), render per-dialect
//! SQL with bound parameters, shape rows into items, and filter cached rows.
//! Planning and shaping are pure so they stay golden-testable; only
//! [`complete_values`] touches the pool.

use super::types::{
    CompletionItem, CompletionItemKind, CompletionRequest, Completions, MatchType, SortOrder,
    ValueCompletionParams,
};
use crate::modelengine::pool::{ConnectionPool, PoolError};
use sqlly_lang::sqlmodel::{
    Dialect, ModelSnapshot, ObjectKind, ObjectRef, Span, SqlName, TypeClass,
};
use sqlly_lang::sqlparse::slots::slot_at;
use sqlly_lang::sqlparse::{Slot, SlotMember};
use std::borrow::Cow;
use std::sync::Arc;

pub const VALUE_LIMIT_DEFAULT: u32 = 100;
pub const VALUE_LIMIT_CAP: u32 = 1000;

/// Plan for a value-mode query: resolved target + rendered SQL + bound params.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueQueryPlan {
    pub sql: String,
    pub params: Vec<String>,
    /// Index of the key column in the result row (FK pivot), else 0.
    pub key_index: usize,
    /// Index of the display column in the result row.
    pub display_index: usize,
    pub key_type: Option<TypeClass>,
    pub note: String,
    /// The requested match semantics. Only `Like` supports the
    /// case-insensitive substring filter applied to cached rows; any other
    /// match type must re-query the database as the typed prefix changes.
    pub match_type: MatchType,
    /// Schema/namespace of the table the values are drawn from, when qualified.
    /// Forwarded as completion-item `schema` metadata.
    pub source_schema: Option<String>,
    /// Bare name of the table the values are drawn from. Carried for grounding;
    /// the value item's `parent` is the column (mirroring the client).
    pub source_table: String,
    /// Bare name of the column whose values are being completed. Forwarded as
    /// the value item's `parent` (with `parent_type = Column`).
    pub source_column: String,
}

impl ValueQueryPlan {
    /// Whether cached rows for this plan may be reused (locally filtered) as the
    /// typed prefix changes. Only the `Like` prefix scan is a pure substring
    /// match; comparison operators (`=`, `>`, `<`, …) treat the prefix as an
    /// operand, and `Between` parses the prefix into range bounds — both demand
    /// a fresh query when the prefix changes.
    pub fn supports_cached_filter(&self) -> bool {
        self.match_type == MatchType::Like
    }
}

/// Build the value-mode plan from the slot, snapshot, and params — pure, so
/// per-dialect rendering and FK pivoting are golden-testable without a
/// database.
pub fn plan_value_query(
    slot: &Slot,
    prefix: &str,
    params: &ValueCompletionParams,
    snapshot: &ModelSnapshot,
    dialect: Dialect,
    default_catalog: Option<&SqlName>,
    default_namespace: Option<&SqlName>,
) -> Result<ValueQueryPlan, String> {
    let member = match &slot.member {
        SlotMember::Named(n) => n.clone(),
        SlotMember::Positional(_) => return Err("positional slot has no column".into()),
    };
    let table_entry = snapshot
        .resolve_kind(
            &slot.container,
            ObjectKind::Table,
            default_catalog,
            default_namespace,
        )
        .ok_or_else(|| format!("table `{}` not in model", slot.container.display()))?;
    // Value mode needs a live counterpart: a file-only object that conflicts
    // with the database (shadow differs) is still queryable only in its
    // database shape; an object with no database provenance at all is not.
    let live_ok = matches!(
        table_entry.provenance,
        sqlly_lang::sqlmodel::Provenance::Database { .. }
    ) || table_entry.shadow.is_some();
    if !live_ok {
        return Err(format!(
            "`{}` exists only in project files; no live data to complete from",
            slot.container.display()
        ));
    }
    let table = table_entry.table().ok_or("not a table")?;
    let column = table
        .column(&member)
        .ok_or_else(|| format!("column `{}` not found", member.spelling()))?;

    // Ownership metadata for the value items: the values belong to a specific
    // column on a specific (possibly schema-qualified) table.
    let source_schema = table_entry
        .reference
        .namespace
        .as_ref()
        .map(|n| n.spelling().to_string());
    let source_table = table_entry.reference.name.spelling().to_string();
    let source_column = column.name.spelling().to_string();

    let limit = params
        .limit
        .unwrap_or(VALUE_LIMIT_DEFAULT)
        .min(VALUE_LIMIT_CAP);
    let sort = params.sort.unwrap_or(SortOrder::Asc);
    let match_type = params.match_type.unwrap_or(MatchType::Like);

    // FK pivot (§8.3): key column → referenced table (key, display) pairs.
    if let Some(pivot) = super::fk_pivot::resolve_fk_pivot(
        snapshot,
        table,
        &member,
        params.display_column.as_deref(),
        dialect,
        default_catalog,
        default_namespace,
    )? {
        let display = pivot.display_column.as_str();
        // Render the resolved entry's canonical reference (carries
        // schema qualification), not the name as written in the FK.
        let table_sql = render_table(&pivot.target, dialect);
        let (where_sql, bound) = render_match(display, prefix, match_type, dialect, 1)?;
        let sql = render_select(
            dialect,
            &format!(
                "{}, {}",
                quote_ident(&pivot.key_column, dialect),
                quote_ident(display, dialect)
            ),
            &table_sql,
            where_sql.as_deref(),
            &quote_ident(display, dialect),
            sort,
            limit,
        );
        return Ok(ValueQueryPlan {
            sql,
            params: bound,
            key_index: 0,
            display_index: 1,
            key_type: pivot.key_type,
            note: format!(
                "foreign key to {}: completing {} (display: {})",
                pivot.target_as_written, pivot.key_column, display
            ),
            match_type,
            source_schema,
            source_table,
            source_column,
        });
    }

    // Plain column: distinct values.
    let col_sql = quote_ident(column.name.spelling(), dialect);
    let table_sql = render_table(&table_entry.reference, dialect);
    let (where_sql, bound) = render_match(column.name.spelling(), prefix, match_type, dialect, 1)?;
    let sql = render_select(
        dialect,
        &col_sql,
        &table_sql,
        where_sql.as_deref(),
        &col_sql,
        sort,
        limit,
    );
    Ok(ValueQueryPlan {
        sql,
        params: bound,
        key_index: 0,
        display_index: 0,
        key_type: column.ty.as_ref().map(|t| t.class),
        note: format!(
            "distinct values of {}.{}",
            slot.container.display(),
            member.spelling()
        ),
        match_type,
        source_schema,
        source_table,
        source_column,
    })
}

/// Quote a catalog name for the auto-run value query, doubling the closing
/// delimiter so a name containing `]`, a backtick, or `"` stays one
/// identifier. The MySQL grammar also covers ClickHouse, which reads backslash
/// escapes inside backticks, so backslashes are doubled there too: on MySQL
/// proper that can only make a name with a backslash fail to resolve, never
/// let it escape.
fn quote_ident(name: &str, dialect: Dialect) -> String {
    match dialect {
        Dialect::SqlServer => format!("[{}]", name.replace(']', "]]")),
        Dialect::MySql => format!("`{}`", name.replace('\\', "\\\\").replace('`', "``")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

fn render_table(reference: &ObjectRef, dialect: Dialect) -> String {
    let mut parts = Vec::new();
    if let Some(c) = &reference.catalog {
        parts.push(quote_ident(c.spelling(), dialect));
    }
    if let Some(n) = &reference.namespace {
        parts.push(quote_ident(n.spelling(), dialect));
    }
    parts.push(quote_ident(reference.name.spelling(), dialect));
    parts.join(".")
}

fn param_marker(dialect: Dialect, n: usize) -> String {
    match dialect {
        Dialect::SqlServer => format!("@p{n}"),
        Dialect::Postgres => format!("${n}"),
        Dialect::Oracle => format!(":p{n}"),
        Dialect::MySql | Dialect::Sqlite => "?".to_string(),
    }
}

fn escape_like(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Render the WHERE predicate for the match type + typed prefix. The prefix
/// is ALWAYS bound as a parameter, never interpolated.
fn render_match(
    column: &str,
    prefix: &str,
    match_type: MatchType,
    dialect: Dialect,
    param_base: usize,
) -> Result<(Option<String>, Vec<String>), String> {
    let col = quote_ident(column, dialect);
    if prefix.is_empty() {
        return Ok((Some(format!("{col} IS NOT NULL")), vec![]));
    }
    let p1 = param_marker(dialect, param_base);
    match match_type {
        MatchType::Like => {
            let escaped = escape_like(prefix);
            let clause = format!("{col} LIKE {p1} ESCAPE '\\' AND {col} IS NOT NULL");
            Ok((Some(clause), vec![format!("%{escaped}%")]))
        }
        MatchType::Between => {
            let parts: Vec<&str> = prefix.splitn(2, "..").collect();
            if parts.len() != 2 {
                // Fall back to LIKE rather than erroring (spec §8.2).
                return render_match(column, prefix, MatchType::Like, dialect, param_base);
            }
            let p2 = param_marker(dialect, param_base + 1);
            Ok((
                Some(format!("{col} BETWEEN {p1} AND {p2}")),
                vec![parts[0].to_string(), parts[1].to_string()],
            ))
        }
        other => {
            let op = match other {
                MatchType::Equals => "=",
                MatchType::Gt => ">",
                MatchType::Lt => "<",
                MatchType::Ge => ">=",
                MatchType::Le => "<=",
                MatchType::Ne => "<>",
                MatchType::Like | MatchType::Between => unreachable!(),
            };
            Ok((Some(format!("{col} {op} {p1}")), vec![prefix.to_string()]))
        }
    }
}

/// Per-dialect SELECT DISTINCT … with row limiting:
/// TOP (mssql), LIMIT (mysql/sqlite/pg), FETCH FIRST (oracle 12c+).
fn render_select(
    dialect: Dialect,
    select_list: &str,
    table: &str,
    where_clause: Option<&str>,
    order_by: &str,
    sort: SortOrder,
    limit: u32,
) -> String {
    let dir = match sort {
        SortOrder::Asc => "ASC",
        SortOrder::Desc => "DESC",
    };
    let where_sql = where_clause
        .map(|w| format!(" WHERE {w}"))
        .unwrap_or_default();
    match dialect {
        Dialect::SqlServer => format!(
            "SELECT DISTINCT TOP({limit}) {select_list} FROM {table}{where_sql} ORDER BY {order_by} {dir}"
        ),
        Dialect::Oracle => format!(
            "SELECT DISTINCT {select_list} FROM {table}{where_sql} ORDER BY {order_by} {dir} FETCH FIRST {limit} ROWS ONLY"
        ),
        _ => format!(
            "SELECT DISTINCT {select_list} FROM {table}{where_sql} ORDER BY {order_by} {dir} LIMIT {limit}"
        ),
    }
}

/// Resolved value-mode slot: the planned query plus the live-slot facts the
/// engine needs to shape items. Computed without touching the database so the
/// engine can derive a cache key before deciding whether to run a query or
/// filter cached rows.
pub struct ValueSlotResolution {
    /// Plan for the live typed prefix. Used for cache-miss keystroke queries
    /// (which must respect what the user typed) and for the no-cache path.
    pub plan: ValueQueryPlan,
    /// Canonical plan with an EMPTY prefix: the full top-N candidate set. The
    /// trigger fetch uses this so keystrokes can filter locally, and its
    /// `sql` is the prefix-independent cache key. (`render_match` emits a
    /// different predicate for empty vs non-empty prefixes, so the live
    /// `plan.sql` is NOT stable across keystrokes and must not be the key.)
    pub cache_plan: ValueQueryPlan,
    /// The typed literal under the cursor (unquoted), used to filter cached rows.
    pub prefix: String,
    /// Span to replace when inserting an item, derived from the live slot.
    pub replace: Option<Span>,
}

/// Resolve the value slot at the cursor and plan its query. Returns `Ok(None)`
/// when there is no value slot, `Err(note)` when planning fails (the note is
/// surfaced as `slot_note`).
pub fn resolve_value_slot(
    req: &CompletionRequest,
    params: &ValueCompletionParams,
    snapshot: &ModelSnapshot,
) -> Result<Option<ValueSlotResolution>, String> {
    let slot = match slot_at(&req.text, req.dialect, req.cursor) {
        Some(s) => s,
        None => return Ok(None),
    };
    // Typed prefix: the slot's current literal text (unquoted), if the cursor
    // is inside it.
    let prefix = if !slot.value.is_empty() && slot.value.span.contains(req.cursor) {
        slot.value.unquoted()
    } else {
        String::new()
    };
    let plan = plan_value_query(
        &slot,
        &prefix,
        params,
        snapshot,
        req.dialect,
        req.default_catalog.as_ref(),
        req.default_namespace.as_ref(),
    )?;
    // Canonical (empty-prefix) plan for caching: same target/columns/sort but a
    // prefix-independent predicate, so the cache key is stable as the user types.
    let cache_plan = if prefix.is_empty() {
        plan.clone()
    } else {
        plan_value_query(
            &slot,
            "",
            params,
            snapshot,
            req.dialect,
            req.default_catalog.as_ref(),
            req.default_namespace.as_ref(),
        )?
    };
    let replace = if slot.value.is_empty() {
        None
    } else {
        Some(slot.value.span)
    };
    Ok(Some(ValueSlotResolution {
        plan,
        cache_plan,
        prefix,
        replace,
    }))
}

/// Shape raw database rows into completion items. Pure and reused by both the
/// live-query path and the cache-filter path, so caching never stores the
/// (live-slot-dependent) `replace_span` — it is applied here per request.
pub fn shape_value_items(
    rows: &[Vec<Option<String>>],
    plan: &ValueQueryPlan,
    dialect: Dialect,
    replace: Option<Span>,
) -> Vec<CompletionItem> {
    let is_fk = plan.key_index != plan.display_index;
    rows.iter()
        .filter_map(|row| {
            let key = row.get(plan.key_index).cloned().flatten()?;
            let display = row
                .get(plan.display_index)
                .cloned()
                .flatten()
                .unwrap_or_else(|| key.clone());
            let literal = render_literal(&key, plan.key_type, dialect);
            // For foreign keys we insert the id followed by the human-readable
            // display value as a trailing block comment (§8.3). Plain
            // distinct-value columns insert the bare literal.
            let insert_text = if is_fk {
                super::fk_pivot::fk_insert_text(&literal, &display)
            } else {
                literal
            };
            let mut item =
                CompletionItem::symbol(display.clone(), insert_text, CompletionItemKind::Value);
            item.detail = is_fk.then(|| format!("key: {key}"));
            item.replace_span = replace;
            item.name = display.clone();
            item.value = Some(key.clone());
            item.r#type = plan.key_type.map(|c| format!("{c:?}"));
            // The value belongs to the column it was drawn from (mirrors the
            // client's value-completion metadata): parent = column, schema =
            // owning table's schema.
            item.parent = Some(plan.source_column.clone());
            item.parent_type = Some(CompletionItemKind::Column);
            item.schema = plan.source_schema.clone();
            item.why = Some(Cow::Borrowed("value"));
            Some(item)
        })
        .collect()
}

/// Filter cached rows by a typed prefix, mirroring the server-side `LIKE
/// %prefix%` match (case-insensitive substring on the display column). An empty
/// prefix keeps every row.
///
/// Only valid for [`MatchType::Like`] plans; callers must consult
/// [`ValueQueryPlan::supports_cached_filter`] before reusing cached rows for a
/// changed prefix, since other match types compare against the prefix rather
/// than performing a substring scan.
pub fn filter_cached_rows(
    rows: &[Vec<Option<String>>],
    plan: &ValueQueryPlan,
    prefix: &str,
) -> Vec<Vec<Option<String>>> {
    if prefix.is_empty() {
        return rows.to_vec();
    }
    // Unicode-aware case folding so non-ASCII display values filter the same way
    // a case-insensitive collation would (e.g. `STRASSE` matches `straße`-ish
    // inputs only as well as full case-folding allows, but at least handles the
    // common accented-Latin/Greek/Cyrillic ranges that `to_ascii_lowercase`
    // ignores).
    let needle = prefix.to_lowercase();
    rows.iter()
        .filter(|row| {
            row.get(plan.display_index)
                .and_then(|cell| cell.as_ref())
                .is_some_and(|display| display.to_lowercase().contains(&needle))
        })
        .cloned()
        .collect()
}

/// Execute value-mode completion: resolve the slot at the cursor, plan the
/// query, run it through the pool, and shape the items. This is the
/// cache-unaware path; the engine layers per-trigger caching on top.
pub async fn complete_values(
    req: &CompletionRequest,
    params: &ValueCompletionParams,
    snapshot: &ModelSnapshot,
    pool: &Arc<ConnectionPool>,
) -> Result<Completions, PoolError> {
    let resolution = match resolve_value_slot(req, params, snapshot) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Ok(Completions {
                items: vec![],
                truncated: false,
                snapshot_generation: snapshot.generation,
                slot_note: Some("no value slot at the cursor".into()),
            });
        }
        Err(note) => {
            return Ok(Completions {
                items: vec![],
                truncated: false,
                snapshot_generation: snapshot.generation,
                slot_note: Some(note),
            });
        }
    };
    let ValueSlotResolution {
        plan,
        cache_plan: _,
        prefix: _,
        replace,
    } = resolution;

    let mut conn = pool.acquire().await?;
    let rows = conn.query(&plan.sql, &plan.params).await?;
    let limit = params
        .limit
        .unwrap_or(VALUE_LIMIT_DEFAULT)
        .min(VALUE_LIMIT_CAP) as usize;
    let truncated = rows.len() >= limit;

    let items = shape_value_items(&rows, &plan, req.dialect, replace);

    Ok(Completions {
        items,
        truncated,
        snapshot_generation: snapshot.generation,
        slot_note: Some(plan.note),
    })
}

/// Render a value as a SQL literal of the key's type class.
/// Numbers stay bare only when they are plain decimal digits (so `NaN` /
/// `inf` never become identifiers); strings follow the dialect's quoting —
/// on the MySQL grammar (MySQL and ClickHouse) a backslash is an escape, so
/// it is doubled and a trailing `\` cannot swallow the closing quote.
fn render_literal(value: &str, class: Option<TypeClass>, dialect: Dialect) -> String {
    let numeric = || {
        let digits = value.strip_prefix('-').unwrap_or(value);
        !digits.is_empty()
            && digits.bytes().any(|b| b.is_ascii_digit())
            && digits
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
            && value.parse::<f64>().is_ok_and(f64::is_finite)
    };
    match class {
        Some(TypeClass::Integer) | Some(TypeClass::Decimal) | Some(TypeClass::Float)
            if numeric() =>
        {
            value.to_string()
        }
        Some(TypeClass::Boolean)
            if matches!(
                value.to_ascii_lowercase().as_str(),
                "true" | "false" | "1" | "0"
            ) =>
        {
            value.to_string()
        }
        _ if dialect == Dialect::MySql => {
            format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
        }
        _ => format!("'{}'", value.replace('\'', "''")),
    }
}

#[cfg(test)]
mod value_filter_tests {
    use super::*;

    /// Value-completion queries run automatically while typing and splice
    /// catalog names into SQL: a hostile table/column name must stay one
    /// quoted identifier.
    #[test]
    fn value_query_identifiers_escape_their_closers() {
        assert_eq!(
            quote_ident("x]; DROP TABLE t; --", Dialect::SqlServer),
            "[x]]; DROP TABLE t; --]"
        );
        assert_eq!(quote_ident("a`b", Dialect::MySql), "`a``b`");
        assert_eq!(quote_ident(r"a\", Dialect::MySql), r"`a\\`");
        assert_eq!(quote_ident("a\"b", Dialect::Postgres), "\"a\"\"b\"");
    }

    /// Inserted value literals follow the dialect's string rules: on the
    /// MySQL grammar a trailing backslash must not escape the closing quote.
    #[test]
    fn value_literals_escape_backslashes_on_mysql() {
        assert_eq!(
            render_literal(
                r"x\' OR 1=1 -- ",
                Some(TypeClass::Character),
                Dialect::MySql
            ),
            r"'x\\'' OR 1=1 -- '"
        );
        assert_eq!(
            render_literal(r"x\", Some(TypeClass::Character), Dialect::Postgres),
            r"'x\'"
        );
        // Float-parseable words are not numbers to SQL.
        assert_eq!(
            render_literal("NaN", Some(TypeClass::Float), Dialect::Postgres),
            "'NaN'"
        );
    }

    fn plan(match_type: MatchType) -> ValueQueryPlan {
        ValueQueryPlan {
            sql: String::new(),
            params: vec![],
            key_index: 0,
            display_index: 0,
            key_type: None,
            note: String::new(),
            match_type,
            source_schema: None,
            source_table: String::new(),
            source_column: String::new(),
        }
    }

    fn rows(values: &[&str]) -> Vec<Vec<Option<String>>> {
        values
            .iter()
            .map(|v| vec![Some((*v).to_string())])
            .collect()
    }

    #[test]
    fn empty_prefix_keeps_every_row() {
        let r = rows(&["Falcon", "Aardvark"]);
        let kept = filter_cached_rows(&r, &plan(MatchType::Like), "");
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        let r = rows(&["Aardvark", "Macchiato", "Falcon"]);
        let kept = filter_cached_rows(&r, &plan(MatchType::Like), "cc");
        let labels: Vec<&str> = kept.iter().map(|row| row[0].as_deref().unwrap()).collect();
        assert_eq!(labels, vec!["Macchiato"]);
    }

    #[test]
    fn filter_folds_non_ascii_case() {
        // `to_ascii_lowercase` would miss the accented capital; Unicode folding
        // must match it.
        let r = rows(&["ÉCLAIR", "Brioche"]);
        let kept = filter_cached_rows(&r, &plan(MatchType::Like), "é");
        let labels: Vec<&str> = kept.iter().map(|row| row[0].as_deref().unwrap()).collect();
        assert_eq!(labels, vec!["ÉCLAIR"]);
    }

    /// Live-metadata names are raw text: a `]`, backtick, or `"` inside one
    /// is doubled so it can never close the quoted identifier early.
    #[test]
    fn identifiers_escape_their_closing_delimiter() {
        assert_eq!(quote_ident("Na]me", Dialect::SqlServer), "[Na]]me]");
        assert_eq!(quote_ident("a`b", Dialect::MySql), "`a``b`");
        assert_eq!(quote_ident("a\"b", Dialect::Postgres), "\"a\"\"b\"");
        assert_eq!(quote_ident("plain", Dialect::Sqlite), "\"plain\"");
    }

    #[test]
    fn only_like_supports_cached_filtering() {
        assert!(plan(MatchType::Like).supports_cached_filter());
        for mt in [
            MatchType::Equals,
            MatchType::Gt,
            MatchType::Lt,
            MatchType::Ge,
            MatchType::Le,
            MatchType::Ne,
            MatchType::Between,
        ] {
            assert!(
                !plan(mt).supports_cached_filter(),
                "{mt:?} must not be locally filterable"
            );
        }
    }
}
