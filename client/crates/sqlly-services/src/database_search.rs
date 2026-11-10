//! Database-wide value search: generate one bounded, read-only SELECT per
//! table for a literal, run them through the `QueryExecutor` port, and
//! stream hits back with throttled progress. Pure SQL generation lives in
//! this module so GPUI, CLI, and MCP share it.

use std::collections::HashSet;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use sqlly_core::{CellValue, ConnectionProfile, ResultColumn, StudioError, StudioResult};
use sqlly_lang::sqlmodel::{DatabaseType, Dialect};
use tokio::sync::mpsc;

use crate::metadata::{BulkColumnRow, profile_with_db};
use crate::query::QueryEvent;
use crate::script_runner::run::ProgressThrottle;
use crate::script_runner::run::ScriptCancel;
use crate::services::Services;

const MAX_HITS: usize = 2000;

#[derive(Debug, Clone, PartialEq)]
pub struct SearchOptions {
    pub value: String,
    pub kind: SearchKind,
    pub match_mode: MatchMode,
    pub case_insensitive: bool,
    pub max_tables: usize,
    pub rows_per_table: usize,
    pub schema_filter: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchKind {
    Text,
    Number,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    Exact,
    Contains,
}

/// One searchable table with the subset of its columns that can hold the literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTarget {
    pub schema: String,
    pub table: String,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub schema: String,
    pub table: String,
    pub column: String,
    pub columns: Vec<ResultColumn>,
    pub row: Vec<CellValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchProgress {
    pub tables_done: usize,
    pub tables_total: usize,
    pub hits: usize,
    pub current_table: Option<String>,
    pub errors: usize,
    pub terminal: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchSummary {
    pub hits: Vec<SearchHit>,
    pub tables_searched: usize,
    pub tables_skipped: usize,
    pub errors: Vec<(String, String)>,
    pub cancelled: bool,
    pub truncated_tables: bool,
}

fn normalize_type_name(type_name: &str) -> String {
    let lower = type_name.trim().to_lowercase();
    if let Some(idx) = lower.find('(') {
        lower[..idx].trim().to_string()
    } else {
        lower
    }
}

/// Whether a column's engine type can participate in a search of `kind`.
pub fn column_is_searchable(kind: SearchKind, type_name: &str) -> bool {
    let t = normalize_type_name(type_name);
    match kind {
        SearchKind::Text => {
            if t.is_empty() {
                return true;
            }
            matches!(
                t.as_str(),
                "char"
                    | "varchar"
                    | "nchar"
                    | "nvarchar"
                    | "text"
                    | "ntext"
                    | "character"
                    | "character varying"
                    | "bpchar"
                    | "citext"
                    | "name"
                    | "tinytext"
                    | "mediumtext"
                    | "longtext"
                    | "enum"
                    | "set"
                    | "uuid"
                    | "uniqueidentifier"
                    | "json"
                    | "jsonb"
                    | "xml"
                    | "sysname"
            )
        }
        SearchKind::Number => matches!(
            t.as_str(),
            "int"
                | "integer"
                | "bigint"
                | "smallint"
                | "tinyint"
                | "int2"
                | "int4"
                | "int8"
                | "decimal"
                | "numeric"
                | "money"
                | "smallmoney"
                | "float"
                | "real"
                | "double"
                | "double precision"
                | "float4"
                | "float8"
                | "serial"
                | "bigserial"
                | "number"
        ),
    }
}

/// Intersect bulk column metadata with concrete tables and honour limits.
pub fn build_targets(
    kind: SearchKind,
    tables: &[(String, String)],
    columns: &[BulkColumnRow],
    max_tables: usize,
    schema_filter: Option<&str>,
) -> Vec<SearchTarget> {
    let table_set: HashSet<(String, String)> = tables
        .iter()
        .filter(|(schema, _)| {
            schema_filter.is_none_or(|filter| schema.eq_ignore_ascii_case(filter))
        })
        .cloned()
        .collect();

    let mut by_table: std::collections::BTreeMap<(String, String), Vec<String>> =
        std::collections::BTreeMap::new();
    for col in columns {
        let schema = col.schema.clone();
        if schema_filter.is_some_and(|filter| !schema.eq_ignore_ascii_case(filter)) {
            continue;
        }
        let key = (schema, col.object.clone());
        if !table_set.contains(&key) {
            continue;
        }
        let type_name = col.type_name.as_deref().unwrap_or("");
        if !column_is_searchable(kind, type_name) {
            continue;
        }
        by_table.entry(key).or_default().push(col.column.clone());
    }

    let mut targets: Vec<SearchTarget> = by_table
        .into_iter()
        .filter(|(_, cols)| !cols.is_empty())
        .map(|((schema, table), columns)| SearchTarget {
            schema,
            table,
            columns,
        })
        .collect();
    targets.sort_by(|a, b| (&a.schema, &a.table).cmp(&(&b.schema, &b.table)));
    targets.truncate(max_tables);
    targets
}

/// Render a dialect-safe string literal, optionally escaped for LIKE.
pub fn sql_literal(db: DatabaseType, value: &str, like: bool) -> String {
    let mut escaped = value.replace('\'', "''");
    if db.family() == Dialect::MySql {
        escaped = escaped.replace('\\', "\\\\");
    }
    if like {
        let mut out = String::with_capacity(escaped.len());
        for ch in escaped.chars() {
            match ch {
                '%' | '_' | '\\' => {
                    out.push('\\');
                    out.push(ch);
                }
                other => out.push(other),
            }
        }
        escaped = out;
    }
    let quoted = format!("'{escaped}'");
    if db.family() == Dialect::SqlServer {
        format!("N{quoted}")
    } else {
        quoted
    }
}

fn table_from_clause(db: DatabaseType, schema: &str, table: &str) -> String {
    if db.family() == Dialect::Sqlite && schema.eq_ignore_ascii_case("main") {
        format!("FROM {} AS t", db.quote_identifier(table))
    } else {
        format!(
            "FROM {}.{} AS t",
            db.quote_identifier(schema),
            db.quote_identifier(table)
        )
    }
}

fn pg_text_expr(db: DatabaseType, column: &str) -> String {
    format!("t.{}::text", db.quote_identifier(column))
}

fn column_ref(db: DatabaseType, column: &str) -> String {
    format!("t.{}", db.quote_identifier(column))
}

fn tds_collate(expr: &str, case_insensitive: bool) -> String {
    if case_insensitive {
        format!("{expr} COLLATE Latin1_General_CI_AS")
    } else {
        expr.to_string()
    }
}

fn like_pattern(db: DatabaseType, value: &str) -> String {
    let inner = sql_literal(db, value, true);
    let body = inner.trim_start_matches('N').trim_matches('\'');
    if db.family() == Dialect::SqlServer {
        format!("N'%{}%'", body)
    } else {
        format!("'%{}%'", body)
    }
}

fn text_predicate(db: DatabaseType, column: &str, options: &SearchOptions) -> Option<String> {
    let literal = sql_literal(db, &options.value, false);
    let col = column_ref(db, column);
    match db.family() {
        Dialect::SqlServer => {
            let expr = tds_collate(&col, options.case_insensitive);
            Some(match options.match_mode {
                MatchMode::Exact => format!("{expr} = {literal}"),
                MatchMode::Contains => {
                    format!(
                        "{expr} LIKE {} ESCAPE '\\'",
                        like_pattern(db, &options.value)
                    )
                }
            })
        }
        Dialect::Postgres => {
            let expr = pg_text_expr(db, column);
            Some(match (options.match_mode, options.case_insensitive) {
                (MatchMode::Contains, true) => {
                    format!("{expr} ILIKE {}", like_pattern(db, &options.value))
                }
                (MatchMode::Contains, false) => format!(
                    "{expr} LIKE {} ESCAPE '\\'",
                    like_pattern(db, &options.value)
                ),
                (MatchMode::Exact, true) => format!("LOWER({expr}) = LOWER({literal})"),
                (MatchMode::Exact, false) => format!("{expr} = {literal}"),
            })
        }
        Dialect::MySql => {
            let expr = if options.case_insensitive {
                col
            } else {
                format!("BINARY {col}")
            };
            Some(match options.match_mode {
                MatchMode::Exact => format!("{expr} = {literal}"),
                MatchMode::Contains => format!(
                    "{expr} LIKE {} ESCAPE '\\\\'",
                    like_pattern(db, &options.value)
                ),
            })
        }
        Dialect::Sqlite => {
            let expr = pg_text_expr(db, column);
            Some(match (options.match_mode, options.case_insensitive) {
                (MatchMode::Contains, _) => format!(
                    "{expr} LIKE {} ESCAPE '\\'",
                    like_pattern(db, &options.value)
                ),
                (MatchMode::Exact, true) => format!("LOWER({expr}) = LOWER({literal})"),
                (MatchMode::Exact, false) => format!("{expr} = {literal}"),
            })
        }
        _ => None,
    }
}

fn number_predicate(db: DatabaseType, column: &str, value: &str) -> Option<String> {
    let parsed = value.trim().parse::<f64>().ok()?;
    let col = column_ref(db, column);
    if parsed.fract() == 0.0 && parsed >= i64::MIN as f64 && parsed <= i64::MAX as f64 {
        Some(format!("{col} = {}", parsed as i64))
    } else {
        Some(format!("{col} = {parsed}"))
    }
}

/// One bounded SELECT for a single table.
pub fn build_table_sql(db: DatabaseType, target: &SearchTarget, options: &SearchOptions) -> String {
    let preds: Vec<String> = target
        .columns
        .iter()
        .filter_map(|column| match options.kind {
            SearchKind::Text => text_predicate(db, column, options),
            SearchKind::Number => number_predicate(db, column, &options.value),
        })
        .collect();

    let where_clause = if preds.is_empty() {
        "1 = 0".to_string()
    } else {
        preds.join(" OR ")
    };

    let limit = options.rows_per_table;
    let from = table_from_clause(db, &target.schema, &target.table);
    match db.family() {
        Dialect::SqlServer => format!("SELECT TOP ({limit}) * {from} WHERE ({where_clause})"),
        _ => format!("SELECT * {from} WHERE ({where_clause}) LIMIT {limit}"),
    }
}

/// Attribute a returned row to the first matching column.
pub fn column_matches(kind: SearchKind, options: &SearchOptions, cell: &CellValue) -> bool {
    match kind {
        SearchKind::Number => number_cell_matches(&options.value, cell),
        SearchKind::Text => text_cell_matches(options, cell),
    }
}

fn number_cell_matches(needle: &str, cell: &CellValue) -> bool {
    let Ok(target) = needle.trim().parse::<f64>() else {
        return false;
    };
    match cell {
        CellValue::I64(v) => (*v as f64 - target).abs() < f64::EPSILON,
        CellValue::F64(v) => (*v - target).abs() < f64::EPSILON,
        CellValue::String(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .is_some_and(|v| (v - target).abs() < f64::EPSILON),
        _ => false,
    }
}

fn text_cell_matches(options: &SearchOptions, cell: &CellValue) -> bool {
    let CellValue::String(hay) = cell else {
        return false;
    };
    match options.match_mode {
        MatchMode::Exact => {
            if options.case_insensitive {
                hay.eq_ignore_ascii_case(&options.value)
            } else {
                hay == &options.value
            }
        }
        MatchMode::Contains => {
            if options.case_insensitive {
                hay.to_ascii_lowercase()
                    .contains(&options.value.to_ascii_lowercase())
            } else {
                hay.contains(&options.value)
            }
        }
    }
}

fn qualified_table_name(schema: &str, table: &str) -> String {
    if schema.is_empty() {
        table.to_string()
    } else {
        format!("{schema}.{table}")
    }
}

async fn send_search_progress(
    tx: &mpsc::Sender<SearchProgress>,
    throttle: &mut ProgressThrottle,
    force: bool,
    snap: SearchProgress,
) {
    if throttle.should_send(Instant::now(), force) {
        let _ = tx.send(snap).await;
    }
}

/// Run a database-wide search through the services ports.
pub async fn run_search(
    services: &Services,
    profile: &ConnectionProfile,
    database: &str,
    options: &SearchOptions,
    progress: mpsc::Sender<SearchProgress>,
    cancel: ScriptCancel,
) -> StudioResult<SearchSummary> {
    let profile_db = profile_with_db(profile, database);
    let tables = services
        .metadata
        .list_tables(&profile_db, database, options.schema_filter.as_deref())
        .await?
        .into_iter()
        .map(|node| (node.schema.unwrap_or_else(|| "dbo".to_string()), node.name))
        .collect::<Vec<_>>();

    let columns = match services
        .metadata
        .list_all_columns(&profile_db, database)
        .await
    {
        Ok(cols) => cols,
        Err(StudioError::NotImplemented(_)) => {
            return Err(StudioError::NotImplemented("database_search"));
        }
        Err(err) => return Err(err),
    };

    let all_targets = build_targets(
        options.kind,
        &tables,
        &columns,
        usize::MAX,
        options.schema_filter.as_deref(),
    );
    let tables_skipped = all_targets.len().saturating_sub(options.max_tables);
    let truncated_by_tables = tables_skipped > 0;
    let targets = build_targets(
        options.kind,
        &tables,
        &columns,
        options.max_tables,
        options.schema_filter.as_deref(),
    );

    let db = profile.database_type;
    let mut summary = SearchSummary {
        hits: Vec::new(),
        tables_searched: 0,
        tables_skipped,
        errors: Vec::new(),
        cancelled: false,
        truncated_tables: truncated_by_tables,
    };
    let mut throttle = ProgressThrottle::new(Duration::from_millis(100));
    let tables_total = targets.len();

    for (index, target) in targets.iter().enumerate() {
        if cancel.is_cancelled() {
            summary.cancelled = true;
            break;
        }

        let current_table = qualified_table_name(&target.schema, &target.table);
        send_search_progress(
            &progress,
            &mut throttle,
            false,
            SearchProgress {
                tables_done: index,
                tables_total,
                hits: summary.hits.len(),
                current_table: Some(current_table.clone()),
                errors: summary.errors.len(),
                terminal: false,
            },
        )
        .await;

        let sql = build_table_sql(db, target, options);
        let handle = services.queries.execute(&profile_db, &sql, 64).await?;

        let mut result_columns: Vec<ResultColumn> = Vec::new();
        let task_id = handle.task_id.clone();
        let mut events = handle.events;
        loop {
            if cancel.is_cancelled() {
                summary.cancelled = true;
                let _ = services.queries.cancel(&task_id).await;
                break;
            }
            let Some(event) = events.recv().await else {
                break;
            };
            match event {
                QueryEvent::ResultSet { columns, .. } => result_columns = columns,
                QueryEvent::Rows { rows, .. } => {
                    for row in rows {
                        if summary.hits.len() >= MAX_HITS {
                            summary.truncated_tables = true;
                            summary.cancelled = false;
                            send_search_progress(
                                &progress,
                                &mut throttle,
                                true,
                                SearchProgress {
                                    tables_done: index + 1,
                                    tables_total,
                                    hits: summary.hits.len(),
                                    current_table: Some(current_table.clone()),
                                    errors: summary.errors.len(),
                                    terminal: true,
                                },
                            )
                            .await;
                            return Ok(summary);
                        }
                        let column = target
                            .columns
                            .iter()
                            .find(|col| {
                                result_columns
                                    .iter()
                                    .position(|c| c.name == **col)
                                    .and_then(|idx| row.get(idx))
                                    .is_some_and(|cell| column_matches(options.kind, options, cell))
                            })
                            .cloned()
                            .unwrap_or_else(|| target.columns[0].clone());
                        summary.hits.push(SearchHit {
                            schema: target.schema.clone(),
                            table: target.table.clone(),
                            column,
                            columns: result_columns.clone(),
                            row,
                        });
                    }
                }
                QueryEvent::Error { message } => {
                    summary.errors.push((current_table.clone(), message));
                }
                QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
                _ => {}
            }
        }

        if summary.cancelled {
            break;
        }
        summary.tables_searched += 1;
    }

    send_search_progress(
        &progress,
        &mut throttle,
        true,
        SearchProgress {
            tables_done: summary.tables_searched,
            tables_total,
            hits: summary.hits.len(),
            current_table: None,
            errors: summary.errors.len(),
            terminal: true,
        },
    )
    .await;

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::query::safety::classify_select_only;

    fn opts(kind: SearchKind, value: &str) -> SearchOptions {
        SearchOptions {
            value: value.into(),
            kind,
            match_mode: MatchMode::Exact,
            case_insensitive: true,
            max_tables: 200,
            rows_per_table: 10,
            schema_filter: None,
        }
    }

    #[test]
    fn text_columns_are_searchable_and_binary_is_not() {
        assert!(column_is_searchable(SearchKind::Text, "varchar"));
        assert!(column_is_searchable(SearchKind::Text, "nvarchar(50)"));
        assert!(column_is_searchable(SearchKind::Text, "jsonb"));
        assert!(!column_is_searchable(SearchKind::Text, "varbinary"));
        assert!(!column_is_searchable(SearchKind::Text, "datetime2"));
        assert!(!column_is_searchable(SearchKind::Text, "geometry"));
        assert!(!column_is_searchable(SearchKind::Text, "bytea"));
        assert!(column_is_searchable(SearchKind::Number, "int"));
        assert!(column_is_searchable(SearchKind::Number, "numeric(10,2)"));
        assert!(!column_is_searchable(SearchKind::Number, "varchar"));
    }

    #[test]
    fn build_targets_drops_views_and_honours_limit() {
        let tables = vec![
            ("dbo".into(), "alpha".into()),
            ("dbo".into(), "beta".into()),
        ];
        let columns = vec![
            BulkColumnRow {
                schema: "dbo".into(),
                object: "alpha".into(),
                column: "id".into(),
                type_name: Some("int".into()),
                nullable: Some(false),
                is_identity: true,
            },
            BulkColumnRow {
                schema: "dbo".into(),
                object: "view_only".into(),
                column: "name".into(),
                type_name: Some("varchar".into()),
                nullable: Some(true),
                is_identity: false,
            },
        ];
        let targets = build_targets(SearchKind::Number, &tables, &columns, 1, None);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].table, "alpha");
    }

    #[test]
    fn sql_literal_doubles_quotes_and_escapes_like_wildcards() {
        assert_eq!(
            sql_literal(DatabaseType::Postgres, "O'Neil", false),
            "'O''Neil'"
        );
        assert_eq!(
            sql_literal(DatabaseType::SqlServer, "O'Neil", false),
            "N'O''Neil'"
        );
        assert_eq!(sql_literal(DatabaseType::MySql, r"a\b", false), r"'a\\b'");
        assert_eq!(sql_literal(DatabaseType::Postgres, "50%", true), "'50\\%'");
    }

    #[test]
    fn tds_sql_uses_top_and_pg_uses_limit() {
        let target = SearchTarget {
            schema: "dbo".into(),
            table: "Users".into(),
            columns: vec!["email".into()],
        };
        let tds = build_table_sql(
            DatabaseType::SqlServer,
            &target,
            &SearchOptions {
                match_mode: MatchMode::Contains,
                ..opts(SearchKind::Text, "x")
            },
        );
        assert!(tds.starts_with("SELECT TOP (10) *"));
        let pg = build_table_sql(
            DatabaseType::Postgres,
            &target,
            &opts(SearchKind::Text, "x"),
        );
        assert!(pg.ends_with("LIMIT 10"));
    }

    #[test]
    fn mysql_exact_case_sensitive_uses_binary() {
        let target = SearchTarget {
            schema: "app".into(),
            table: "users".into(),
            columns: vec!["name".into()],
        };
        let sql = build_table_sql(
            DatabaseType::MySql,
            &target,
            &SearchOptions {
                case_insensitive: false,
                ..opts(SearchKind::Text, "Ann")
            },
        );
        assert!(sql.contains("BINARY t.`name`"));
    }

    #[test]
    fn sqlite_main_schema_is_not_prefixed() {
        let target = SearchTarget {
            schema: "main".into(),
            table: "items".into(),
            columns: vec!["label".into()],
        };
        let sql = build_table_sql(DatabaseType::Sqlite, &target, &opts(SearchKind::Text, "x"));
        assert!(sql.contains(r#"FROM "items" AS t"#));
        assert!(!sql.contains(r#""main"."items""#));
    }

    #[test]
    fn contains_mode_emits_like_with_escape() {
        let target = SearchTarget {
            schema: "dbo".into(),
            table: "t".into(),
            columns: vec!["c".into()],
        };
        let sql = build_table_sql(
            DatabaseType::Postgres,
            &target,
            &SearchOptions {
                match_mode: MatchMode::Contains,
                case_insensitive: false,
                ..opts(SearchKind::Text, "a")
            },
        );
        assert!(sql.contains("LIKE"));
        assert!(sql.contains("ESCAPE"));
    }

    #[test]
    fn generated_sql_is_select_only() {
        let target = SearchTarget {
            schema: "dbo".into(),
            table: "t".into(),
            columns: vec!["c".into(), "n".into()],
        };
        for db in [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::Sqlite,
        ] {
            let text_sql = build_table_sql(
                db,
                &target,
                &SearchOptions {
                    match_mode: MatchMode::Contains,
                    ..opts(SearchKind::Text, "needle")
                },
            );
            assert!(
                classify_select_only(&text_sql).select_only,
                "{db:?} text: {text_sql}"
            );
            let number_sql = build_table_sql(db, &target, &opts(SearchKind::Number, "42"));
            assert!(
                classify_select_only(&number_sql).select_only,
                "{db:?} number: {number_sql}"
            );
        }
    }

    #[test]
    fn column_matches_attributes_number_and_text_hits() {
        let text_opts = SearchOptions {
            match_mode: MatchMode::Contains,
            case_insensitive: true,
            ..opts(SearchKind::Text, "foo")
        };
        assert!(column_matches(
            SearchKind::Text,
            &text_opts,
            &CellValue::String("FooBar".into())
        ));
        let num_opts = opts(SearchKind::Number, "42");
        assert!(column_matches(
            SearchKind::Number,
            &num_opts,
            &CellValue::I64(42)
        ));
        assert!(column_matches(
            SearchKind::Number,
            &num_opts,
            &CellValue::String("42".into())
        ));
    }
}
