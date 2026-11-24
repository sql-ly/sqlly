//! Turns what a rule wants (an index on these columns, fresh statistics on
//! this table) into SQL in the plan's dialect, checked against the supplied
//! schema when there is one. Nothing here invents a name: a statement is
//! written only from identifiers the plan or the schema spelled out.

use super::facts::{Ctx, Rel};
use super::predicate::{Term, TermKind};
use crate::plans::PlanDialect;
use crate::plans::index_ddl::{IndexDdl, IndexDdlDialect};
use crate::plans::schema::{SchemaIndex, SchemaTable, TypeFamily, bare_name};

/// An index a finding would like to exist.
#[derive(Debug, Clone, Default)]
pub(crate) struct IndexSuggestion {
    pub schema: Option<String>,
    pub table: String,
    /// The table name may be a query alias.
    pub alias_only: bool,
    /// Columns compared with `=` (or `IN`, `IS NULL`): leading key columns.
    pub equality: Vec<String>,
    /// Columns compared with a range: one follows the equality columns.
    pub range: Vec<String>,
    /// `ORDER BY` columns with their direction (`true` = descending).
    pub sort: Vec<(String, bool)>,
    /// Columns the query only reads: stored in the index, not part of the key.
    pub include: Vec<String>,
    /// A key expression (function-based index), written as the engine
    /// expects it.
    pub expression: Option<String>,
}

impl IndexSuggestion {
    pub fn for_rel(rel: &Rel<'_>) -> Self {
        Self {
            schema: rel.schema.clone(),
            table: rel.name.clone(),
            alias_only: rel.alias_only,
            ..Default::default()
        }
    }

    pub fn table_key(&self) -> String {
        self.label().to_ascii_lowercase()
    }

    pub fn label(&self) -> String {
        match &self.schema {
            Some(schema) if !schema.is_empty() => format!("{schema}.{}", self.table),
            _ => self.table.clone(),
        }
    }

    /// Key columns in order: equality columns, then the sort columns, or
    /// the first range column when the query does not sort (a range column
    /// ends the useful part of a key, so only one goes in).
    fn key(&self) -> (Vec<String>, Vec<String>) {
        let mut key: Vec<String> = Vec::new();
        for column in &self.equality {
            push_unique(&mut key, column);
        }
        let mut leftover: Vec<String> = Vec::new();
        if self.range.is_empty() {
            for (column, _) in &self.sort {
                push_unique(&mut key, column);
            }
        } else {
            for (position, column) in self.range.iter().enumerate() {
                if position == 0 {
                    push_unique(&mut key, column);
                } else if !contains(&key, column) {
                    push_unique(&mut leftover, column);
                }
            }
        }
        (key, leftover)
    }
}

pub(crate) struct BuiltIndex {
    pub sql: String,
    pub key: Vec<String>,
    pub include: Vec<String>,
    /// Existing index whose key is a prefix of the new one.
    pub extends: Option<String>,
    pub drop_sql: Option<String>,
    /// Existing index the statement rebuilds in place with more columns.
    pub rebuilds: Option<String>,
}

pub(crate) enum IndexAdvice {
    Create(BuiltIndex),
    /// The schema already has an index that serves the suggestion.
    Exists(String),
    None,
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    if !value.is_empty() && !contains(list, value) {
        list.push(value.to_string());
    }
}

fn contains(list: &[String], value: &str) -> bool {
    list.iter().any(|item| item.eq_ignore_ascii_case(value))
}

fn starts_with(long: &[String], short: &[String]) -> bool {
    short.len() <= long.len()
        && short
            .iter()
            .zip(long)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

/// True when an index already suggested (`key`, `include`) serves a second
/// suggestion (`new_key`, `new_include`) on the same table.
pub(crate) fn covers(
    key: &[String],
    include: &[String],
    new_key: &[String],
    new_include: &[String],
) -> bool {
    starts_with(key, new_key)
        && new_include
            .iter()
            .all(|column| contains(key, column) || contains(include, column))
}

fn ddl_dialect(dialect: PlanDialect) -> Option<IndexDdlDialect> {
    match dialect {
        // DuckDB and ClickHouse are column stores: a b-tree index is not the
        // fix for a slow scan there.
        PlanDialect::DuckDb | PlanDialect::ClickHouse => None,
        other => IndexDdlDialect::for_plan(other),
    }
}

/// Quoted `schema.table` for statements, or `None` where the engine has no
/// quoting rule here.
pub(crate) fn table_sql(ctx: &Ctx<'_>, rel: &Rel<'_>) -> Option<String> {
    let schema = rel.schema.as_deref().unwrap_or("");
    match ctx.dialect {
        PlanDialect::ClickHouse => Some(if schema.is_empty() {
            format!("`{}`", rel.name.replace('`', "``"))
        } else {
            format!(
                "`{}`.`{}`",
                schema.replace('`', "``"),
                rel.name.replace('`', "``")
            )
        }),
        PlanDialect::Sqlite => Some(IndexDdlDialect::Sqlite.quote(&rel.name)),
        other => {
            IndexDdlDialect::for_plan(other).map(|dialect| dialect.qualified(schema, &rel.name))
        }
    }
}

/// Quote one column name for the plan's dialect.
pub(crate) fn quote(ctx: &Ctx<'_>, name: &str) -> String {
    match IndexDdlDialect::for_plan(ctx.dialect) {
        Some(dialect) => dialect.quote(name),
        None => format!("`{}`", name.replace('`', "``")),
    }
}

/// The statement that refreshes optimizer statistics on `rel`, where the
/// engine has one.
pub(crate) fn stats_sql(ctx: &Ctx<'_>, rel: &Rel<'_>) -> Option<String> {
    if rel.alias_only {
        return None;
    }
    let table = table_sql(ctx, rel)?;
    match ctx.dialect {
        PlanDialect::Postgres => Some(format!("ANALYZE {table};")),
        PlanDialect::SqlServer => Some(format!("UPDATE STATISTICS {table} WITH FULLSCAN;")),
        PlanDialect::MySql => Some(format!("ANALYZE TABLE {table};")),
        PlanDialect::MariaDb => Some(format!("ANALYZE TABLE {table} PERSISTENT FOR ALL;")),
        PlanDialect::Sqlite => Some(format!("ANALYZE {table};")),
        PlanDialect::Oracle => {
            let owner = match rel.schema.as_deref().filter(|schema| !schema.is_empty()) {
                Some(schema) => format!("'{}'", schema.replace('\'', "''")),
                None => "USER".to_string(),
            };
            Some(format!(
                "BEGIN\n  DBMS_STATS.GATHER_TABLE_STATS(ownname => {owner}, tabname => '{}');\nEND;\n/",
                rel.name.replace('\'', "''")
            ))
        }
        PlanDialect::DuckDb | PlanDialect::ClickHouse => None,
    }
}

/// Columns of `rel` a predicate compares, split into equality and range
/// columns. `None` when the predicate names no usable column of this table.
pub(crate) fn suggestion_from_terms(rel: &Rel<'_>, terms: &[Term]) -> Option<IndexSuggestion> {
    let mut suggestion = IndexSuggestion::for_rel(rel);
    let mut list: Vec<String> = Vec::new();
    for term in terms {
        let Some(column) = term.bare_column() else {
            continue;
        };
        if !rel.owns(column) || term.negated {
            continue;
        }
        match term.kind {
            TermKind::Equality | TermKind::IsNull => {
                push_unique(&mut suggestion.equality, &column.name)
            }
            TermKind::In => push_unique(&mut list, &column.name),
            TermKind::Range => push_unique(&mut suggestion.range, &column.name),
            TermKind::Like if !term.leading_wildcard() && term.pattern.is_some() => {
                push_unique(&mut suggestion.range, &column.name)
            }
            _ => {}
        }
    }
    // IN lists seek like equality but go after the single-value columns.
    for column in list {
        push_unique(&mut suggestion.equality, &column);
    }
    suggestion
        .range
        .retain(|column| !contains(&suggestion.equality, column));
    (!suggestion.equality.is_empty() || !suggestion.range.is_empty()).then_some(suggestion)
}

fn index_name(dialect: IndexDdlDialect, table: &str, key: &[String], expression: bool) -> String {
    let clean = |text: &str| -> String {
        text.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
            .trim_matches('_')
            .to_string()
    };
    let mut parts: Vec<String> = vec![clean(table)];
    parts.extend(key.iter().map(|column| clean(column)));
    if expression {
        parts.push("expr".to_string());
    }
    parts.retain(|part| !part.is_empty());
    let limit = if dialect == IndexDdlDialect::Oracle {
        30
    } else {
        60
    };
    let (prefix, body) = match dialect {
        IndexDdlDialect::SqlServer => ("IX_", parts.join("_")),
        IndexDdlDialect::Oracle => ("IX_", parts.join("_").to_ascii_uppercase()),
        _ => ("ix_", parts.join("_").to_ascii_lowercase()),
    };
    let mut name = format!("{prefix}{body}");
    if name.len() > limit {
        name.truncate(limit);
        name = name.trim_end_matches('_').to_string();
    }
    name
}

fn drop_index_sql(dialect: IndexDdlDialect, schema: &str, table: &str, index: &str) -> String {
    match dialect {
        IndexDdlDialect::SqlServer | IndexDdlDialect::MySql | IndexDdlDialect::MariaDb => format!(
            "DROP INDEX {} ON {};",
            dialect.quote(index),
            dialect.qualified(schema, table)
        ),
        IndexDdlDialect::Postgres | IndexDdlDialect::Oracle => {
            format!("DROP INDEX {};", dialect.qualified(schema, index))
        }
        IndexDdlDialect::Sqlite | IndexDdlDialect::DuckDb => {
            format!("DROP INDEX {};", dialect.quote(index))
        }
    }
}

/// True when the column's type cannot be an index key.
fn unindexable(dialect: IndexDdlDialect, table: &SchemaTable, column: &str) -> bool {
    table.column(column).is_some_and(|column| {
        let lower = column.type_name.to_ascii_lowercase();
        // `text` is the ordinary string type on PostgreSQL, SQLite and
        // DuckDB. SQL Server's legacy text types cannot be index keys, and
        // MySQL's TEXT family needs a prefix length this advice cannot pick.
        let text_family = match dialect {
            IndexDdlDialect::SqlServer => {
                matches!(lower.as_str(), "text" | "ntext" | "image")
            }
            IndexDdlDialect::MySql | IndexDdlDialect::MariaDb => {
                matches!(
                    lower.as_str(),
                    "text" | "tinytext" | "mediumtext" | "longtext"
                )
            }
            _ => false,
        };
        matches!(column.type_family(), TypeFamily::Json)
            || lower.contains("(max)")
            || text_family
            || lower.contains("blob")
            || lower.contains("clob")
            || lower == "xml"
    })
}

/// True when the engine stores the primary key in every secondary index, so
/// naming those columns again adds nothing.
fn carries_primary_key(dialect: IndexDdlDialect, table: &SchemaTable) -> bool {
    match dialect {
        IndexDdlDialect::MySql | IndexDdlDialect::MariaDb => true,
        IndexDdlDialect::SqlServer => table
            .primary_key
            .as_ref()
            .is_some_and(|key| key.clustered != Some(false)),
        _ => false,
    }
}

fn index_has_all(
    dialect: IndexDdlDialect,
    table: &SchemaTable,
    index: &SchemaIndex,
    columns: &[String],
) -> bool {
    // A clustered index (and an InnoDB primary key) is the table: it holds
    // every column.
    let is_table = match dialect {
        IndexDdlDialect::SqlServer => {
            index.clustered == Some(true) || (index.primary && index.clustered != Some(false))
        }
        IndexDdlDialect::MySql | IndexDdlDialect::MariaDb => index.primary,
        _ => false,
    };
    if is_table {
        return true;
    }
    let primary: Vec<&str> = if carries_primary_key(dialect, table) {
        table
            .primary_key
            .as_ref()
            .map(|key| key.key_names())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    columns.iter().all(|column| {
        index
            .stored_columns()
            .chain(primary.iter().copied())
            .any(|stored| stored.eq_ignore_ascii_case(column))
    })
}

/// Build the statement for `suggestion`, or say why there is none.
pub(crate) fn index_advice(ctx: &Ctx<'_>, suggestion: &IndexSuggestion) -> IndexAdvice {
    let Some(dialect) = ddl_dialect(ctx.dialect) else {
        return IndexAdvice::None;
    };
    let table_name = bare_name(&suggestion.table);
    if table_name.is_empty() {
        return IndexAdvice::None;
    }
    let schema_name = suggestion
        .schema
        .as_deref()
        .map(bare_name)
        .unwrap_or_default();
    let (mut key, leftover) = suggestion.key();
    let mut include: Vec<String> = Vec::new();
    for column in leftover.iter().chain(&suggestion.include) {
        if !contains(&key, column) {
            push_unique(&mut include, column);
        }
    }
    if key.is_empty() && suggestion.expression.is_none() {
        return IndexAdvice::None;
    }
    let table = ctx
        .schema
        .and_then(|context| {
            context.table(
                Some(schema_name.as_str()).filter(|s| !s.is_empty()),
                &table_name,
            )
        })
        .filter(|table| table.has_columns());
    let mut extends = None;
    let mut drop_sql = None;
    let mut rebuilds = None;
    let mut name = None;
    let mut real_table = table_name.clone();
    let mut real_schema = schema_name.clone();
    if let Some(table) = table {
        real_table = table.name.clone();
        if !table.schema.is_empty() {
            real_schema = table.schema.clone();
        }
        // Use the schema's spelling, and refuse columns it does not have.
        let mut spelled = Vec::new();
        for column in &key {
            match table.column(column) {
                Some(found) if !unindexable(dialect, table, column) => {
                    spelled.push(found.name.clone())
                }
                _ => return IndexAdvice::None,
            }
        }
        key = spelled;
        include = include
            .iter()
            .filter_map(|column| table.column(column).map(|found| found.name.clone()))
            .collect();
        if carries_primary_key(dialect, table)
            && let Some(primary) = &table.primary_key
        {
            let primary = primary.key_names();
            include.retain(|column| !primary.iter().any(|key| key.eq_ignore_ascii_case(column)));
        }
        if suggestion.expression.is_none() {
            for index in table.all_indexes() {
                if !index.is_ordered()
                    || index.filter.is_some()
                    || index.columns.iter().any(|column| column.expression)
                    || index
                        .columns
                        .iter()
                        .any(|column| column.prefix_length.is_some())
                {
                    continue;
                }
                let index_key: Vec<String> = index
                    .key_names()
                    .iter()
                    .map(|name| name.to_string())
                    .collect();
                let label = if index.name.is_empty() {
                    index_key.join(", ")
                } else {
                    index.name.clone()
                };
                if starts_with(&index_key, &key) {
                    if index_has_all(dialect, table, index, &include) {
                        return IndexAdvice::Exists(label);
                    }
                    // Same key, missing stored columns. SQL Server can
                    // rebuild the index in place with the extra columns.
                    if dialect == IndexDdlDialect::SqlServer
                        && !index.primary
                        && !index.unique
                        && index.clustered != Some(true)
                        && !index.name.is_empty()
                    {
                        let mut merged = index.include_columns.clone();
                        for column in &include {
                            if !contains(&index_key, column) {
                                push_unique(&mut merged, column);
                            }
                        }
                        key = index_key;
                        include = merged;
                        name = Some(index.name.clone());
                        rebuilds = Some(index.name.clone());
                        break;
                    }
                } else if starts_with(&key, &index_key)
                    && !index_key.is_empty()
                    && !index.primary
                    && !index.unique
                    && index.clustered != Some(true)
                    && !index.name.is_empty()
                    && extends.is_none()
                {
                    // The new index makes this narrower one redundant, as
                    // long as it also stores what the old one stored.
                    for column in &index.include_columns {
                        if !contains(&key, column) && dialect.supports_include() {
                            push_unique(&mut include, column);
                        }
                    }
                    extends = Some(index.name.clone());
                    drop_sql = Some(drop_index_sql(
                        dialect,
                        &real_schema,
                        &real_table,
                        &index.name,
                    ));
                }
            }
        }
    }
    let descending: Vec<String> = {
        let used: Vec<&(String, bool)> = suggestion
            .sort
            .iter()
            .filter(|(column, _)| contains(&key, column))
            .collect();
        // An index can be read backwards, so directions only matter when
        // they are mixed.
        let mixed = used.iter().any(|(_, desc)| *desc) && used.iter().any(|(_, desc)| !*desc);
        if mixed && suggestion.range.is_empty() {
            used.iter()
                .filter(|(_, desc)| *desc)
                .map(|(column, _)| column.clone())
                .collect()
        } else {
            Vec::new()
        }
    };
    let name = name
        .unwrap_or_else(|| index_name(dialect, &real_table, &key, suggestion.expression.is_some()));
    let ddl = IndexDdl {
        schema: real_schema,
        table: real_table,
        name,
        columns: key.clone(),
        include_columns: include.clone(),
        descending_columns: descending,
        key_expressions: suggestion.expression.iter().cloned().collect(),
        drop_existing: rebuilds.is_some(),
        ..Default::default()
    };
    let mut built_key = key;
    if let Some(expression) = &suggestion.expression {
        built_key.push(expression.clone());
    }
    IndexAdvice::Create(BuiltIndex {
        sql: ddl.render(dialect),
        key: built_key,
        include,
        extends,
        drop_sql,
        rebuilds,
    })
}
