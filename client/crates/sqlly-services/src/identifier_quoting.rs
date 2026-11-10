use sqlly_core::{ConnectionKind, DatabaseType};

/// Quote one identifier part for the engine: `[x]` (SQL Server, `]`→`]]`),
/// `` `x` `` (MySQL/MariaDB and ClickHouse, `` ` ``→`` `` ``), `"x"`
/// (PostgreSQL/SQLite, `"`→`""`).
pub fn quote_identifier(db: DatabaseType, part: &str) -> String {
    match db.connection_kind() {
        ConnectionKind::Tds => format!("[{}]", part.replace(']', "]]")),
        ConnectionKind::MySqlWire => format!("`{}`", part.replace('`', "``")),
        // ClickHouse also reads backslash escapes inside backticks.
        ConnectionKind::ClickHouseHttp => db.quote_identifier(part),
        _ => format!("\"{}\"", part.replace('"', "\"\"")),
    }
}

/// Dotted, fully quoted name: `[dbo].[Orders]`, `"public"."orders"`, `` `db`.`orders` ``.
/// `None` parts are skipped.
pub fn qualified_identifier(db: DatabaseType, schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(schema) => format!(
            "{}.{}",
            quote_identifier(db, schema),
            quote_identifier(db, name)
        ),
        None => quote_identifier(db, name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ClickHouse reads `\` as an escape inside backquoted identifiers, so a
    /// name ending in a backslash must not be able to escape its backtick.
    #[test]
    fn clickhouse_identifiers_escape_backslashes() {
        assert_eq!(
            quote_identifier(DatabaseType::ClickHouse, r"x\`; DROP TABLE t; --"),
            r"`x\\``; DROP TABLE t; --`"
        );
        assert_eq!(quote_identifier(DatabaseType::MySql, r"a\"), r"`a\`");
    }

    #[test]
    fn quotes_clickhouse_with_backticks() {
        assert_eq!(
            qualified_identifier(DatabaseType::ClickHouse, Some("db"), "we`ird"),
            "`db`.`we``ird`"
        );
    }

    #[test]
    fn quotes_sql_server_with_brackets_and_escapes_close_bracket() {
        assert_eq!(
            quote_identifier(DatabaseType::SqlServer, "Orders"),
            "[Orders]"
        );
        assert_eq!(
            quote_identifier(DatabaseType::SqlServer, "weird]name"),
            "[weird]]name]"
        );
    }

    #[test]
    fn quotes_mysql_with_backticks() {
        assert_eq!(quote_identifier(DatabaseType::MySql, "orders"), "`orders`");
        assert_eq!(quote_identifier(DatabaseType::MySql, "we`ird"), "`we``ird`");
    }

    #[test]
    fn quotes_postgres_with_double_quotes() {
        assert_eq!(
            quote_identifier(DatabaseType::Postgres, "orders"),
            "\"orders\""
        );
        assert_eq!(
            quote_identifier(DatabaseType::Postgres, "we\"ird"),
            "\"we\"\"ird\""
        );
    }

    #[test]
    fn qualified_skips_missing_schema() {
        assert_eq!(
            qualified_identifier(DatabaseType::SqlServer, Some("dbo"), "Orders"),
            "[dbo].[Orders]"
        );
        assert_eq!(
            qualified_identifier(DatabaseType::SqlServer, None, "Orders"),
            "[Orders]"
        );
        assert_eq!(
            qualified_identifier(DatabaseType::Postgres, Some(""), "orders"),
            "\"orders\""
        );
    }
}
