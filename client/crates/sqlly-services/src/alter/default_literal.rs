//! Column defaults as the designer's user types them versus the SQL a
//! `DEFAULT` clause needs.
//!
//! A default typed into a text, ENUM or SET column's field is usually a plain
//! value (`a`), which every engine rejects unquoted (`DEFAULT a` reads as a
//! column reference). Such a value is written as a string literal; anything
//! that already reads as SQL (a quoted literal, a number, `NULL`, a
//! `CURRENT_TIMESTAMP`-style keyword, a parenthesised expression or a
//! function call) is kept as typed. Defaults read back from the catalog are
//! already SQL, so passing them through here leaves them unchanged.

use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};

use crate::alter::script::comments::string_literal;
use crate::alter::snapshot::TableDraft;

/// The SQL for `raw` as the default of a column of type `data_type`.
pub fn column_default_sql(db: DatabaseType, data_type: &str, raw: &str) -> String {
    let value = raw.trim();
    if value.is_empty() || !is_string_like_type(data_type) || reads_as_sql(value) {
        return raw.to_string();
    }
    let literal = string_literal(db, value);
    // SQL Server keeps non-ASCII text in a national column only from N'…'.
    if db.connection_kind() == ConnectionKind::Tds && is_national_type(data_type) {
        return format!("N{literal}");
    }
    literal
}

fn is_national_type(data_type: &str) -> bool {
    let ty = data_type.trim().to_ascii_lowercase();
    ty.starts_with("nchar") || ty.starts_with("nvarchar") || ty.starts_with("ntext")
}

/// Rewrite every draft column's default with [`column_default_sql`].
pub fn normalize_draft_defaults(db: DatabaseType, draft: &mut TableDraft) {
    for col in &mut draft.columns {
        if let Some(def) = col.def.default.as_mut() {
            *def = column_default_sql(db, &col.def.data_type, def);
        }
    }
}

/// Text, ENUM and SET columns: the types whose plain-word default is a
/// string value.
fn is_string_like_type(data_type: &str) -> bool {
    let mut ty = data_type.trim().to_ascii_lowercase();
    // ClickHouse wrappers: `Nullable(String)`, `LowCardinality(String)`.
    loop {
        let inner = ["nullable(", "lowcardinality("]
            .iter()
            .find_map(|w| ty.strip_prefix(w).map(|rest| rest.to_string()));
        match inner {
            Some(rest) => ty = rest,
            None => break,
        }
    }
    let base = ty
        .split(['(', '['])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    const NAMES: &[&str] = &[
        "enum",
        "set",
        "char",
        "varchar",
        "character",
        "character varying",
        "national char",
        "national character",
        "national character varying",
        "national varchar",
        "nchar",
        "nvarchar",
        "bpchar",
        "varchar2",
        "nvarchar2",
        "text",
        "tinytext",
        "mediumtext",
        "longtext",
        "ntext",
        "citext",
        "string",
        "fixedstring",
        "clob",
        "nclob",
    ];
    NAMES
        .iter()
        .any(|name| base == *name || base.starts_with(&format!("{name} ")))
}

/// Whether `value` (trimmed, non-empty) is already SQL rather than a plain
/// value: quoted literals (with or without a prefix such as `N`, `E`, `b`,
/// `x` or `_utf8mb4`), quoted identifiers, numbers, parenthesised
/// expressions, function calls, casts and value keywords.
fn reads_as_sql(value: &str) -> bool {
    let first = value.chars().next().unwrap_or_default();
    if matches!(first, '\'' | '"' | '`' | '[' | '(') {
        return true;
    }
    if let Some(at) = value.find('\'') {
        let prefix = &value[..at];
        let lower = prefix.to_ascii_lowercase();
        if matches!(lower.as_str(), "n" | "e" | "b" | "x" | "u&")
            || (lower.starts_with('_') && lower[1..].chars().all(|c| c.is_ascii_alphanumeric()))
        {
            return true;
        }
    }
    if value.ends_with(')') && value.contains('(') {
        return true;
    }
    if value.contains("::") {
        return true;
    }
    let unsigned = value.trim_start_matches(['-', '+']);
    if unsigned.parse::<f64>().is_ok()
        || (unsigned.len() > 2
            && unsigned[..2].eq_ignore_ascii_case("0x")
            && unsigned[2..].chars().all(|c| c.is_ascii_hexdigit()))
    {
        return true;
    }
    const KEYWORDS: &[&str] = &[
        "NULL",
        "TRUE",
        "FALSE",
        "DEFAULT",
        "CURRENT_TIMESTAMP",
        "CURRENT_DATE",
        "CURRENT_TIME",
        "CURRENT_USER",
        "CURRENT_ROLE",
        "CURRENT_SCHEMA",
        "CURRENT_CATALOG",
        "SESSION_USER",
        "SYSTEM_USER",
        "USER",
        "LOCALTIME",
        "LOCALTIMESTAMP",
        "SYSDATE",
        "SYSTIMESTAMP",
        "UTC_TIMESTAMP",
        "UTC_DATE",
        "UTC_TIME",
    ];
    KEYWORDS.iter().any(|k| value.eq_ignore_ascii_case(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_value_on_enum_column_is_quoted() {
        assert_eq!(
            column_default_sql(DatabaseType::MySql, "enum('a','b')", "a"),
            "'a'"
        );
        assert_eq!(
            column_default_sql(DatabaseType::MariaDb, "set('x','y')", "x"),
            "'x'"
        );
    }

    #[test]
    fn plain_value_on_text_columns_is_quoted_and_escaped() {
        assert_eq!(
            column_default_sql(DatabaseType::MySql, "varchar(20)", "it's"),
            "'it''s'"
        );
        assert_eq!(
            column_default_sql(DatabaseType::MySql, "varchar(20)", "a\\b"),
            "'a\\\\b'"
        );
        assert_eq!(
            column_default_sql(DatabaseType::Postgres, "character varying(20)", "new"),
            "'new'"
        );
        assert_eq!(
            column_default_sql(DatabaseType::Postgres, "text", "a\\b"),
            "'a\\b'"
        );
        assert_eq!(
            column_default_sql(DatabaseType::SqlServer, "nvarchar(50)", "hello world"),
            "N'hello world'"
        );
        assert_eq!(
            column_default_sql(DatabaseType::SqlServer, "varchar(50)", "hello"),
            "'hello'"
        );
        assert_eq!(
            column_default_sql(DatabaseType::Sqlite, "TEXT", "pending"),
            "'pending'"
        );
    }

    #[test]
    fn expressions_and_literals_are_kept() {
        for (ty, value) in [
            ("varchar(20)", "'a'"),
            ("varchar(20)", "''"),
            ("nvarchar(20)", "N'a'"),
            ("varchar(20)", "_utf8mb4'a'"),
            ("text", "E'a\\n'"),
            ("varchar(20)", "NULL"),
            ("varchar(20)", "null"),
            ("varchar(20)", "CURRENT_USER"),
            ("varchar(36)", "(UUID())"),
            ("varchar(36)", "uuid()"),
            ("text", "'a'::text"),
            ("text", "concat('a', 'b')"),
            ("varchar(20)", "0"),
            ("varchar(20)", "-1.5"),
            ("nvarchar(20)", "('a')"),
            ("enum('a','b')", "'a'"),
        ] {
            assert_eq!(
                column_default_sql(DatabaseType::MySql, ty, value),
                value,
                "{ty} {value}"
            );
        }
    }

    #[test]
    fn non_string_columns_are_left_alone() {
        for (ty, value) in [
            ("int", "0"),
            ("timestamp", "CURRENT_TIMESTAMP"),
            ("datetime(3)", "CURRENT_TIMESTAMP(3)"),
            ("boolean", "true"),
            ("int", "abc"),
        ] {
            assert_eq!(column_default_sql(DatabaseType::MySql, ty, value), value);
        }
    }
}
