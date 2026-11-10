//! Pure helpers for the value-lookup feature: identifier quoting, dialect-aware
//! distinct-value query construction, SQL literal formatting, and
//! numeric-literal detection. Ports Swift's `DistinctValueProvider.swift`.

use crate::intellisense::context::{ValueContext, ValueOperator};
use sqlly_core::{DatabaseType, Dialect};

/// Bracket-quote a SQL Server identifier, doubling any `]` inside.
pub fn bracket(identifier: &str) -> String {
    format!("[{}]", identifier.replace(']', "]]"))
}

/// Build a distinct-values query for a column in `db`'s own dialect: identifier
/// quoting and row limiting both follow the engine (`TOP` on SQL Server —
/// byte-identical to the historical output — `LIMIT` on PostgreSQL, MySQL, and
/// SQLite, `FETCH FIRST` where that is the native form).
pub fn distinct_values_sql(
    db: DatabaseType,
    context: &ValueContext,
    limit: usize,
) -> Option<String> {
    distinct_values_sql_matching(db, context, "", limit)
}

/// [`distinct_values_sql`] narrowed to the values containing `partial`
/// (case-insensitive, matching the popup's local filter). The first fetch of
/// a value lookup runs without a partial and is capped at `limit` rows, so
/// when the user keeps typing past what that sample covers the popup
/// re-queries with the typed text as a server-side filter instead of showing
/// nothing (feedback ifb_2fc6). An empty `partial` is the plain query.
pub fn distinct_values_sql_matching(
    db: DatabaseType,
    context: &ValueContext,
    partial: &str,
    limit: usize,
) -> Option<String> {
    let column = context.column.as_ref()?;
    let table = context.table.as_ref()?;

    let table_part = match &context.schema {
        Some(schema) => format!(
            "{}.{}",
            db.quote_identifier(schema),
            db.quote_identifier(table)
        ),
        None => db.quote_identifier(table),
    };
    let col = db.quote_identifier(column);
    let filter = contains_filter_clause(db, &col, partial);
    Some(distinct_limited(
        db,
        &col,
        &table_part,
        &format!("WHERE {col} IS NOT NULL{filter} ORDER BY {col}"),
        limit,
    ))
}

/// ` AND <column contains partial>` in `db`'s dialect, or empty for an empty
/// partial. The match is case-insensitive like the popup's own filter:
/// `ILIKE` on the PostgreSQL family (whose `LIKE` is case-sensitive),
/// `UPPER()` on both sides for Oracle, and the engine's collation elsewhere.
/// The typed text is embedded as an escaped literal with its LIKE wildcards
/// neutralised, so a `%`, `_`, or quote the user types matches literally.
pub fn contains_filter_clause(db: DatabaseType, column_expr: &str, partial: &str) -> String {
    if partial.is_empty() {
        return String::new();
    }
    let mut pattern = String::with_capacity(partial.len() + 2);
    pattern.push('%');
    for ch in partial.chars() {
        if db.like_special_characters().contains(&ch) {
            pattern.push('\\');
        }
        pattern.push(ch);
    }
    pattern.push('%');
    let literal = db.string_literal(&pattern);
    match db.family() {
        Dialect::Postgres => format!(" AND {column_expr} ILIKE {literal} ESCAPE '\\'"),
        Dialect::Oracle => {
            format!(" AND UPPER({column_expr}) LIKE UPPER({literal}) ESCAPE '\\'")
        }
        // A MySQL string literal decodes `\\` to one backslash, so the
        // escape character is spelled doubled there.
        Dialect::MySql => format!(" AND {column_expr} LIKE {literal} ESCAPE '\\\\'"),
        Dialect::SqlServer | Dialect::Sqlite => {
            format!(" AND {column_expr} LIKE {literal} ESCAPE '\\'")
        }
    }
}

/// One value-lookup candidate: the value inserted and, for a foreign-key id
/// column, the referenced row's readable label (the name shown and searched,
/// while the key is what gets inserted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueLookupEntry {
    pub value: String,
    pub label: Option<String>,
}

impl ValueLookupEntry {
    /// A plain distinct value (no label).
    pub fn plain(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: None,
        }
    }
}

/// Build the (key, display) lookup for a foreign-key id column: the
/// referenced table's key and readable column, ordered by the readable one,
/// in `db`'s own dialect. `target` is the referenced table's name parts
/// (catalog, schema, table — whichever are known), each quoted. No DISTINCT:
/// the key is unique, and DISTINCT would fail on a LOB display column.
pub fn fk_values_sql(
    db: DatabaseType,
    target: &[&str],
    key_column: &str,
    display_column: &str,
    limit: usize,
) -> String {
    fk_values_sql_matching(db, target, key_column, display_column, "", limit)
}

/// [`fk_values_sql`] narrowed to the rows whose readable column contains
/// `partial` (see [`distinct_values_sql_matching`]).
pub fn fk_values_sql_matching(
    db: DatabaseType,
    target: &[&str],
    key_column: &str,
    display_column: &str,
    partial: &str,
    limit: usize,
) -> String {
    use sqlly_core::RowLimitSyntax;
    let from = target
        .iter()
        .filter(|part| !part.is_empty())
        .map(|part| db.quote_identifier(part))
        .collect::<Vec<_>>()
        .join(".");
    let key = db.quote_identifier(key_column);
    let display = db.quote_identifier(display_column);
    let projection = format!("{key}, {display}");
    let filter = contains_filter_clause(db, &display, partial);
    let tail = format!("WHERE {display} IS NOT NULL{filter} ORDER BY {display}");
    match db.row_limit_syntax() {
        RowLimitSyntax::Top => format!("SELECT TOP {limit} {projection} FROM {from} {tail}"),
        RowLimitSyntax::First => format!("SELECT FIRST {limit} {projection} FROM {from} {tail}"),
        RowLimitSyntax::Limit => format!("SELECT {projection} FROM {from} {tail} LIMIT {limit}"),
        RowLimitSyntax::FetchFirst => {
            format!("SELECT {projection} FROM {from} {tail} FETCH FIRST {limit} ROWS ONLY")
        }
    }
}

/// Build the editor's word-under-cursor distinct lookup against a raw table
/// reference lifted from the user's own SQL (already engine-correct text, so it
/// is embedded verbatim); only the column identifier and limit clause are
/// rendered per dialect.
pub fn distinct_word_lookup_sql(
    db: DatabaseType,
    column: &str,
    table_reference: &str,
    limit: usize,
) -> String {
    let col = db.quote_identifier(column);
    distinct_limited(db, &col, table_reference, "", limit)
}

fn distinct_limited(
    db: DatabaseType,
    projection: &str,
    from: &str,
    tail: &str,
    limit: usize,
) -> String {
    use sqlly_core::RowLimitSyntax;
    let sep = if tail.is_empty() { "" } else { " " };
    match db.row_limit_syntax() {
        RowLimitSyntax::Top => {
            format!("SELECT DISTINCT TOP {limit} {projection} FROM {from}{sep}{tail}")
        }
        RowLimitSyntax::First => {
            format!("SELECT FIRST {limit} DISTINCT {projection} FROM {from}{sep}{tail}")
        }
        RowLimitSyntax::Limit => {
            format!("SELECT DISTINCT {projection} FROM {from}{sep}{tail} LIMIT {limit}")
        }
        RowLimitSyntax::FetchFirst => format!(
            "SELECT DISTINCT {projection} FROM {from}{sep}{tail} FETCH FIRST {limit} ROWS ONLY"
        ),
    }
}

/// Format a raw value as a SQL literal for insertion into the editor.
///
/// - Numeric values with `Equals` → bare (e.g. `42`, `-3.14`).
/// - String values with `Equals` → single-quoted with `''` escaping.
/// - Any value with `Like` → `'%value%'` with `''` escaping.
pub fn format_sql_literal(value: &str, operator: ValueOperator) -> String {
    let escaped = value.replace('\'', "''");
    match operator {
        ValueOperator::Equals if is_numeric_literal(value) => value.to_string(),
        ValueOperator::Equals => format!("'{escaped}'"),
        ValueOperator::Like => format!("'%{escaped}%'"),
    }
}

/// [`format_sql_literal`] in the engine's own string rules: the SQL Server
/// family gets `N'…'`, the MySQL lineage and ClickHouse double backslashes
/// (a value ending in `\` would otherwise escape the closing quote), and so
/// on — see `DatabaseType::string_literal`. Values come from the database, so
/// they are treated as hostile.
pub fn format_sql_literal_for(db: DatabaseType, value: &str, operator: ValueOperator) -> String {
    match operator {
        ValueOperator::Equals if is_numeric_literal(value) => value.trim().to_string(),
        ValueOperator::Equals => db.string_literal(value),
        ValueOperator::Like => db.string_literal(&format!("%{value}%")),
    }
}

/// The comma-separated list "Look up values" inserts into the editor (for
/// building `IN (…)` lists). Plain numbers stay bare; every other value —
/// including one that already starts with a quote — is rendered as a fresh
/// engine literal, so stored text can never become runnable SQL.
pub fn distinct_values_list(db: DatabaseType, values: &[String]) -> String {
    values
        .iter()
        .map(|value| {
            if is_numeric_literal(value) {
                value.trim().to_string()
            } else {
                db.string_literal(value)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Check if a value looks like a numeric literal: optional leading sign,
/// digits, at most one dot, and at least one digit.
pub fn is_numeric_literal(value: &str) -> bool {
    let s = value.trim();
    if s.is_empty() {
        return false;
    }
    let bytes = s.as_bytes();
    let mut i = 0;
    // Optional leading sign.
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mut has_digit = false;
    let mut has_dot = false;
    while i < bytes.len() {
        match bytes[i] {
            b'0'..=b'9' => {
                has_digit = true;
            }
            b'.' if !has_dot => {
                has_dot = true;
            }
            _ => return false,
        }
        i += 1;
    }
    has_digit
}

/// Case-insensitive substring match for filtering values by partial input.
pub fn matches_partial(value: &str, partial: &str) -> bool {
    if partial.is_empty() {
        return true;
    }
    value
        .to_ascii_lowercase()
        .contains(&partial.to_ascii_lowercase())
}

/// Cache key for distinct-value lookups, canonicalized to lowercase.
pub fn value_lookup_cache_key(
    connection_id: &str,
    database: &str,
    schema: Option<&str>,
    table: &str,
    column: &str,
) -> String {
    let schema = schema.unwrap_or("");
    format!(
        "{}:{}:{}:{}:{}",
        connection_id.to_ascii_lowercase(),
        database.to_ascii_lowercase(),
        schema.to_ascii_lowercase(),
        table.to_ascii_lowercase(),
        column.to_ascii_lowercase(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- engine-aware literals (hostile database values) ------------------

    #[test]
    fn value_completion_literal_follows_the_engine() {
        // MySQL: a value ending in `\` must not escape the closing quote.
        assert_eq!(
            format_sql_literal_for(
                DatabaseType::MySql,
                r"x\' OR 1=1 -- ",
                ValueOperator::Equals
            ),
            r"'x\\'' OR 1=1 -- '"
        );
        assert_eq!(
            format_sql_literal_for(DatabaseType::SqlServer, "O'Brien", ValueOperator::Equals),
            "N'O''Brien'"
        );
        assert_eq!(
            format_sql_literal_for(DatabaseType::Postgres, "42", ValueOperator::Equals),
            "42"
        );
        assert_eq!(
            format_sql_literal_for(DatabaseType::ClickHouse, r"a\", ValueOperator::Like),
            r"'%a\\%'"
        );
    }

    #[test]
    fn distinct_value_list_never_trusts_a_leading_quote() {
        // A database value that merely starts with a quote used to be
        // inserted verbatim, so stored text became runnable SQL.
        let values = vec![
            "'); DROP TABLE t; --".to_string(),
            "42".to_string(),
            "NaN".to_string(),
            r"back\".to_string(),
        ];
        assert_eq!(
            distinct_values_list(DatabaseType::MySql, &values),
            r"'''); DROP TABLE t; --', 42, 'NaN', 'back\\'"
        );
        assert_eq!(
            distinct_values_list(DatabaseType::SqlServer, &values[..1]),
            "N'''); DROP TABLE t; --'"
        );
    }

    // ---- bracket quoting --------------------------------------------------

    #[test]
    fn bracket_simple() {
        assert_eq!(bracket("abc"), "[abc]");
    }

    #[test]
    fn bracket_doubles_closing_bracket() {
        assert_eq!(bracket("a]b"), "[a]]b]");
    }

    #[test]
    fn bracket_multiple_closing_brackets() {
        assert_eq!(bracket("a]]b"), "[a]]]]b]");
    }

    #[test]
    fn bracket_empty() {
        assert_eq!(bracket(""), "[]");
    }

    // ---- fk_values_sql ------------------------------------------------------

    #[test]
    fn fk_values_sql_selects_key_and_label_in_the_engines_dialect() {
        assert_eq!(
            fk_values_sql(
                DatabaseType::SqlServer,
                &["dbo", "Categories"],
                "CategoryID",
                "CategoryName",
                100
            ),
            "SELECT TOP 100 [CategoryID], [CategoryName] FROM [dbo].[Categories] WHERE [CategoryName] IS NOT NULL ORDER BY [CategoryName]"
        );
        assert_eq!(
            fk_values_sql(DatabaseType::Postgres, &["", "cats"], "id", "name", 50),
            "SELECT \"id\", \"name\" FROM \"cats\" WHERE \"name\" IS NOT NULL ORDER BY \"name\" LIMIT 50"
        );
    }

    #[test]
    fn fk_values_sql_keeps_hostile_names_inside_their_quotes() {
        let sql = fk_values_sql(
            DatabaseType::SqlServer,
            &["x]; DROP TABLE t;--"],
            "k",
            "d",
            1,
        );
        assert!(sql.contains("[x]]; DROP TABLE t;--]"), "{sql}");
    }

    // ---- distinct_values_sql -----------------------------------------------

    fn vc(table: &str, column: &str) -> ValueContext {
        ValueContext {
            column: Some(column.to_string()),
            table: Some(table.to_string()),
            schema: None,
            operator: ValueOperator::Equals,
            partial_value: String::new(),
            replace_start: 0,
            replace_end: 0,
            has_opening_quote: false,
        }
    }

    fn vc_schema(schema: &str, table: &str, column: &str) -> ValueContext {
        ValueContext {
            column: Some(column.to_string()),
            table: Some(table.to_string()),
            schema: Some(schema.to_string()),
            operator: ValueOperator::Equals,
            partial_value: String::new(),
            replace_start: 0,
            replace_end: 0,
            has_opening_quote: false,
        }
    }

    #[test]
    fn distinct_sql_no_schema() {
        let ctx = vc("Users", "status");
        let sql = distinct_values_sql(DatabaseType::SqlServer, &ctx, 100).unwrap();
        assert_eq!(
            sql,
            "SELECT DISTINCT TOP 100 [status] FROM [Users] WHERE [status] IS NOT NULL ORDER BY [status]"
        );
    }

    #[test]
    fn distinct_sql_with_schema() {
        let ctx = vc_schema("dbo", "Users", "status");
        let sql = distinct_values_sql(DatabaseType::SqlServer, &ctx, 50).unwrap();
        assert_eq!(
            sql,
            "SELECT DISTINCT TOP 50 [status] FROM [dbo].[Users] WHERE [status] IS NOT NULL ORDER BY [status]"
        );
    }

    /// A value lookup whose first (unfiltered) fetch hit the row cap
    /// re-queries with the typed text as a server-side filter, so values
    /// past the sample still show up (feedback ifb_2fc6). The match is
    /// case-insensitive on every engine and the typed text is literal.
    #[test]
    fn distinct_sql_matching_filters_by_typed_partial() {
        let ctx = ValueContext {
            column: Some("Name".into()),
            table: Some("Item".into()),
            schema: Some("Inventory".into()),
            operator: ValueOperator::Like,
            partial_value: "Ton".into(),
            replace_start: 0,
            replace_end: 0,
            has_opening_quote: false,
        };
        assert_eq!(
            distinct_values_sql_matching(DatabaseType::SqlServer, &ctx, "Ton", 100).unwrap(),
            "SELECT DISTINCT TOP 100 [Name] FROM [Inventory].[Item] WHERE [Name] IS NOT NULL AND [Name] LIKE N'%Ton%' ESCAPE '\\' ORDER BY [Name]"
        );
        assert_eq!(
            distinct_values_sql_matching(DatabaseType::Postgres, &ctx, "Ton", 100).unwrap(),
            "SELECT DISTINCT \"Name\" FROM \"Inventory\".\"Item\" WHERE \"Name\" IS NOT NULL AND \"Name\" ILIKE '%Ton%' ESCAPE '\\' ORDER BY \"Name\" LIMIT 100"
        );
        assert_eq!(
            distinct_values_sql_matching(DatabaseType::MySql, &ctx, "Ton", 100).unwrap(),
            "SELECT DISTINCT `Name` FROM `Inventory`.`Item` WHERE `Name` IS NOT NULL AND `Name` LIKE '%Ton%' ESCAPE '\\\\' ORDER BY `Name` LIMIT 100"
        );
        assert_eq!(
            distinct_values_sql_matching(DatabaseType::Oracle, &ctx, "Ton", 100).unwrap(),
            "SELECT DISTINCT \"Name\" FROM \"Inventory\".\"Item\" WHERE \"Name\" IS NOT NULL AND UPPER(\"Name\") LIKE UPPER('%Ton%') ESCAPE '\\' ORDER BY \"Name\" FETCH FIRST 100 ROWS ONLY"
        );
        // An empty partial is the plain query.
        assert_eq!(
            distinct_values_sql_matching(DatabaseType::SqlServer, &ctx, "", 100),
            distinct_values_sql(DatabaseType::SqlServer, &ctx, 100)
        );
    }

    #[test]
    fn contains_filter_neutralises_wildcards_and_quotes() {
        assert_eq!(
            contains_filter_clause(DatabaseType::SqlServer, "[c]", "50%_[a]'b"),
            " AND [c] LIKE N'%50\\%\\_\\[a\\]''b%' ESCAPE '\\'"
        );
        // PostgreSQL needs the E'' form once a backslash is in the literal.
        assert_eq!(
            contains_filter_clause(DatabaseType::Postgres, "\"c\"", "a_b"),
            " AND \"c\" ILIKE E'%a\\\\_b%' ESCAPE '\\'"
        );
        assert_eq!(
            contains_filter_clause(DatabaseType::Sqlite, "\"c\"", ""),
            ""
        );
    }

    #[test]
    fn fk_sql_matching_filters_on_the_display_column() {
        assert_eq!(
            fk_values_sql_matching(
                DatabaseType::SqlServer,
                &["dbo", "Categories"],
                "CategoryID",
                "CategoryName",
                "bev",
                100
            ),
            "SELECT TOP 100 [CategoryID], [CategoryName] FROM [dbo].[Categories] WHERE [CategoryName] IS NOT NULL AND [CategoryName] LIKE N'%bev%' ESCAPE '\\' ORDER BY [CategoryName]"
        );
    }

    #[test]
    fn distinct_sql_postgres_uses_limit_and_double_quotes() {
        let ctx = vc_schema("public", "users", "status");
        let sql = distinct_values_sql(DatabaseType::Postgres, &ctx, 100).unwrap();
        assert_eq!(
            sql,
            "SELECT DISTINCT \"status\" FROM \"public\".\"users\" WHERE \"status\" IS NOT NULL ORDER BY \"status\" LIMIT 100"
        );
    }

    #[test]
    fn distinct_sql_mysql_uses_limit_and_backticks() {
        let ctx = vc("users", "status");
        let sql = distinct_values_sql(DatabaseType::MySql, &ctx, 20).unwrap();
        assert_eq!(
            sql,
            "SELECT DISTINCT `status` FROM `users` WHERE `status` IS NOT NULL ORDER BY `status` LIMIT 20"
        );
    }

    #[test]
    fn distinct_sql_sqlite_uses_limit_and_double_quotes() {
        let ctx = vc("users", "status");
        let sql = distinct_values_sql(DatabaseType::Sqlite, &ctx, 20).unwrap();
        assert_eq!(
            sql,
            "SELECT DISTINCT \"status\" FROM \"users\" WHERE \"status\" IS NOT NULL ORDER BY \"status\" LIMIT 20"
        );
    }

    #[test]
    fn distinct_sql_brackets_special_chars() {
        let ctx = vc("My Table", "col]name");
        let sql = distinct_values_sql(DatabaseType::SqlServer, &ctx, 100).unwrap();
        assert!(sql.contains("[My Table]"));
        assert!(sql.contains("[col]]name]"));
    }

    #[test]
    fn distinct_word_lookup_matches_engine() {
        assert_eq!(
            distinct_word_lookup_sql(DatabaseType::SqlServer, "status", "[dbo].[Users]", 20),
            "SELECT DISTINCT TOP 20 [status] FROM [dbo].[Users]"
        );
        assert_eq!(
            distinct_word_lookup_sql(DatabaseType::Postgres, "status", "users", 20),
            "SELECT DISTINCT \"status\" FROM users LIMIT 20"
        );
        assert_eq!(
            distinct_word_lookup_sql(DatabaseType::MariaDb, "status", "users", 20),
            "SELECT DISTINCT `status` FROM users LIMIT 20"
        );
    }

    #[test]
    fn distinct_sql_missing_column_returns_none() {
        let ctx = ValueContext {
            column: None,
            table: Some("Users".to_string()),
            schema: None,
            operator: ValueOperator::Equals,
            partial_value: String::new(),
            replace_start: 0,
            replace_end: 0,
            has_opening_quote: false,
        };
        assert!(distinct_values_sql(DatabaseType::SqlServer, &ctx, 100).is_none());
    }

    #[test]
    fn distinct_sql_missing_table_returns_none() {
        let ctx = ValueContext {
            column: Some("status".to_string()),
            table: None,
            schema: None,
            operator: ValueOperator::Equals,
            partial_value: String::new(),
            replace_start: 0,
            replace_end: 0,
            has_opening_quote: false,
        };
        assert!(distinct_values_sql(DatabaseType::SqlServer, &ctx, 100).is_none());
    }

    // ---- format_sql_literal ------------------------------------------------

    #[test]
    fn format_numeric_equals_bare() {
        assert_eq!(format_sql_literal("42", ValueOperator::Equals), "42");
    }

    #[test]
    fn format_negative_numeric_equals_bare() {
        assert_eq!(format_sql_literal("-3.14", ValueOperator::Equals), "-3.14");
    }

    #[test]
    fn format_string_equals_quoted() {
        assert_eq!(
            format_sql_literal("active", ValueOperator::Equals),
            "'active'"
        );
    }

    #[test]
    fn format_string_with_apostrophe_escaped() {
        assert_eq!(
            format_sql_literal("O'Brien", ValueOperator::Equals),
            "'O''Brien'"
        );
    }

    #[test]
    fn format_string_like_wrapped_with_wildcards() {
        assert_eq!(
            format_sql_literal("active", ValueOperator::Like),
            "'%active%'"
        );
    }

    #[test]
    fn format_string_like_with_apostrophe_escaped() {
        assert_eq!(
            format_sql_literal("O'Brien", ValueOperator::Like),
            "'%O''Brien%'"
        );
    }

    #[test]
    fn format_empty_string_equals() {
        assert_eq!(format_sql_literal("", ValueOperator::Equals), "''");
    }

    // ---- is_numeric_literal ------------------------------------------------

    #[test]
    fn numeric_integer() {
        assert!(is_numeric_literal("42"));
    }

    #[test]
    fn numeric_negative() {
        assert!(is_numeric_literal("-42"));
    }

    #[test]
    fn numeric_decimal() {
        assert!(is_numeric_literal("3.14"));
    }

    #[test]
    fn numeric_negative_decimal() {
        assert!(is_numeric_literal("-3.14"));
    }

    #[test]
    fn numeric_with_plus_sign() {
        assert!(is_numeric_literal("+42"));
    }

    #[test]
    fn numeric_just_dot_not_numeric() {
        assert!(!is_numeric_literal("."));
    }

    #[test]
    fn numeric_empty_not_numeric() {
        assert!(!is_numeric_literal(""));
    }

    #[test]
    fn numeric_alpha_not_numeric() {
        assert!(!is_numeric_literal("abc"));
    }

    #[test]
    fn numeric_two_dots_not_numeric() {
        assert!(!is_numeric_literal("1.2.3"));
    }

    #[test]
    fn numeric_dot_then_digits() {
        assert!(is_numeric_literal(".5"));
    }

    // ---- matches_partial ---------------------------------------------------

    #[test]
    fn partial_empty_matches_all() {
        assert!(matches_partial("active", ""));
    }

    #[test]
    fn partial_case_insensitive() {
        assert!(matches_partial("Active", "act"));
        assert!(matches_partial("ACTIVE", "act"));
        assert!(matches_partial("active", "ACT"));
    }

    #[test]
    fn partial_no_match() {
        assert!(!matches_partial("active", "xyz"));
    }

    #[test]
    fn partial_substring() {
        assert!(matches_partial("inactive", "active"));
    }

    // ---- cache key ---------------------------------------------------------

    #[test]
    fn cache_key_lowercase() {
        let key = value_lookup_cache_key("Conn1", "DB1", Some("dbo"), "Users", "Status");
        assert_eq!(key, "conn1:db1:dbo:users:status");
    }

    #[test]
    fn cache_key_no_schema() {
        let key = value_lookup_cache_key("conn", "db", None, "Users", "Status");
        assert_eq!(key, "conn:db::users:status");
    }
}
