//! `:name` and `?` placeholders lex as parameter markers on every engine that
//! accepts them, so the formatter keeps them whole. Chat2DB's formatter turned
//! `id = :id` into `id =: id` on MySQL, which breaks the query; these pin that
//! SQLly never does.

use sqlly_lang::sqlformat::{SqlDialect, SqlFormattingPreferences, format_sql};
use sqlly_lang::sqllex::{ParamStyle, StrictLexer, TokenKind};
use sqlly_lang::sqlmodel::Dialect;

fn markers(sql: &str, dialect: Dialect) -> Vec<(ParamStyle, String)> {
    StrictLexer::tokenize(sql, dialect)
        .into_iter()
        .filter_map(|t| match t.kind {
            TokenKind::ParamMarker(style) => Some((style, t.text.to_string())),
            _ => None,
        })
        .collect()
}

fn format(sql: &str, dialect: SqlDialect) -> String {
    let prefs = SqlFormattingPreferences {
        dialect,
        ..SqlFormattingPreferences::default()
    };
    format_sql(sql, &prefs, None).replace('\n', " ")
}

#[test]
fn colon_names_are_markers_on_mysql_and_sql_server() {
    for dialect in [Dialect::MySql, Dialect::SqlServer] {
        assert_eq!(
            markers("SELECT * FROM t WHERE id = :id AND s=:status", dialect),
            vec![
                (ParamStyle::ColonName, ":id".to_string()),
                (ParamStyle::ColonName, ":status".to_string()),
            ],
            "{dialect:?}"
        );
    }
}

#[test]
fn colon_touching_a_name_is_not_a_marker() {
    // A MySQL label written tight against its loop, and a T-SQL static
    // method call, keep their own meaning.
    assert!(markers("lbl:LOOP SELECT 1; END LOOP lbl", Dialect::MySql).is_empty());
    assert!(markers("SELECT geography::Point(1, 2, 4326)", Dialect::SqlServer).is_empty());
    // MySQL's assignment operator is not a placeholder either.
    assert!(
        markers("SELECT @n := 1", Dialect::MySql)
            .iter()
            .all(|(style, _)| *style == ParamStyle::AtName)
    );
}

#[test]
fn question_marks_are_markers_except_on_postgres() {
    for dialect in [
        Dialect::MySql,
        Dialect::Sqlite,
        Dialect::SqlServer,
        Dialect::Oracle,
    ] {
        assert_eq!(
            markers("SELECT * FROM t WHERE a = ? AND b=?", dialect).len(),
            2,
            "{dialect:?}"
        );
    }
    // PostgreSQL's `?` is a jsonb operator.
    assert!(markers("SELECT data ? 'key' FROM t", Dialect::Postgres).is_empty());
}

#[test]
fn formatter_keeps_placeholders_whole_on_every_dialect() {
    let sql = "select * from beneficiaries where id=:id and status = :status and x=?";
    for dialect in [
        SqlDialect::AutoDetect,
        SqlDialect::Mysql,
        SqlDialect::Tsql,
        SqlDialect::Sqlite,
        SqlDialect::Oracle,
    ] {
        let out = format(sql, dialect);
        assert!(out.contains("id = :id"), "{dialect:?}: {out}");
        assert!(out.contains("status = :status"), "{dialect:?}: {out}");
        assert!(out.contains("x = ?"), "{dialect:?}: {out}");
    }
    // PostgreSQL keeps `:name` whole; its `?` stays an operator character.
    let out = format(
        "select * from t where id=:id and x = $1",
        SqlDialect::Postgresql,
    );
    assert!(out.contains("id = :id") && out.contains("x = $1"), "{out}");
}

#[test]
fn auto_detect_still_reads_jsonb_operators_as_postgres() {
    let out = format(
        "select * from t where data ?| array['a'] and id::int = 1",
        SqlDialect::AutoDetect,
    );
    assert!(out.contains("?|"), "{out}");
    assert!(out.contains("::"), "{out}");
}
