pub mod clickhouse;
pub mod mysql;
pub mod oracle;
pub mod postgres;

/// Double-quote a PostgreSQL identifier, escaping embedded `"`.
pub(crate) fn pg_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Single-quote a PostgreSQL string literal, escaping embedded `'`.
pub(crate) fn pg_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Backtick-quote a MySQL identifier, escaping embedded `` ` ``.
pub(crate) fn my_ident(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

/// Single-quote a MySQL string literal, escaping `'` and `\`.
pub(crate) fn my_literal(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "''"))
}

/// MySQL account literal `'user'@'host'` with both parts escaped; host defaults to `%`.
pub fn mysql_account(user: &str, host: Option<&str>) -> String {
    let host = host.unwrap_or("%");
    format!("{}@{}", my_literal(user), my_literal(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_account_escapes_and_defaults_host() {
        assert_eq!(mysql_account("app", None), "'app'@'%'");
        assert_eq!(mysql_account("o'brien", Some("10.%")), "'o''brien'@'10.%'");
    }
}
