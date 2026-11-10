//! Adversarial corpus for the security lexer. The recurring assertion is the
//! one every gate relies on: when an engine would execute a statement verb,
//! at least one reading exposes that verb as a bare word in its shadow.

use super::*;
use crate::tokenizer::TokenKind;

fn bare_words(view: &View) -> Vec<String> {
    view.tokens()
        .into_iter()
        .filter(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier))
        .filter(|t| !t.text.starts_with('['))
        .map(|t| t.text.to_ascii_uppercase())
        .collect()
}

/// Some reading of `sql` under `target` exposes `word` as live code.
fn exposed(sql: &str, target: LexTarget, word: &str) -> bool {
    views(sql, target)
        .views
        .iter()
        .any(|v| bare_words(v).iter().any(|w| w == word))
}

fn exposed_on(sql: &str, engine: LexEngine, word: &str) -> bool {
    exposed(sql, LexTarget::Engine(engine), word)
}

/// No reading of `sql` under `engine` exposes `word`.
fn hidden_on(sql: &str, engine: LexEngine, word: &str) -> bool {
    !exposed_on(sql, engine, word)
}

#[test]
fn reported_backtick_repro_exposes_the_delete_on_every_backtick_engine() {
    let sql = "SELECT 1 AS `'`; DELETE FROM t; SELECT 1 AS `'`";
    for engine in [LexEngine::MySql, LexEngine::Sqlite, LexEngine::ClickHouse] {
        assert!(exposed_on(sql, engine, "DELETE"), "{engine:?}");
    }
    assert!(exposed(sql, LexTarget::AnyEngine, "DELETE"));
    // The first half of the report: no separator between statements.
    let sql = "DELETE FROM t\nSELECT COUNT(*) FROM t";
    for engine in LexEngine::ALL {
        assert!(exposed_on(sql, engine, "DELETE"), "{engine:?}");
    }
}

#[test]
fn quote_characters_inside_every_identifier_quoting_style_stay_inside() {
    for (sql, engines) in [
        (
            "SELECT 1 AS \"'\"; DELETE FROM t; SELECT 1 AS \"'\"",
            &[
                LexEngine::TSql,
                LexEngine::Postgres,
                LexEngine::CockroachDb,
                LexEngine::DuckDb,
                LexEngine::Sqlite,
                LexEngine::Oracle,
                LexEngine::ClickHouse,
                LexEngine::MySql,
            ][..],
        ),
        (
            "SELECT 1 AS [']; DELETE FROM t; SELECT 1 AS [']",
            &[LexEngine::TSql, LexEngine::Sqlite][..],
        ),
        (
            "SELECT 1 AS [a]]']; DELETE FROM t; SELECT 1 AS [']",
            &[LexEngine::TSql][..],
        ),
    ] {
        for &engine in engines {
            assert!(exposed_on(sql, engine, "DELETE"), "{engine:?}: {sql}");
        }
    }
}

#[test]
fn mysql_backslash_escapes_are_read_both_ways() {
    // Default sql_mode: `\'` escapes, so the literal closes at the third quote
    // and the DELETE is live.
    let sql = r"SELECT '\''; DELETE FROM t; SELECT '";
    assert!(exposed_on(sql, LexEngine::MySql, "DELETE"));
    let sql = r"SELECT '\\'; DELETE FROM t";
    assert!(exposed_on(sql, LexEngine::MySql, "DELETE"));
    // NO_BACKSLASH_ESCAPES: the backslash is a character and the quote
    // closes the literal. The default reading hides this DELETE; the other
    // reading must expose it.
    let sql = r"SELECT 'a\'; DELETE FROM t; -- '";
    let all = views(sql, LexTarget::Engine(LexEngine::MySql));
    assert!(!bare_words(all.primary()).contains(&"DELETE".to_string()));
    assert!(exposed_on(sql, LexEngine::MySql, "DELETE"));
    // Same for double-quoted strings and for ANSI_QUOTES identifiers.
    let sql = r#"SELECT "a\"; DELETE FROM t; -- ""#;
    assert!(exposed_on(sql, LexEngine::MySql, "DELETE"));
}

#[test]
fn mysql_comment_rules() {
    // `--` needs whitespace after it on MySQL: `--1` is two minus signs.
    assert!(exposed_on(
        "SELECT 1 --1; DELETE FROM t",
        LexEngine::MySql,
        "DELETE"
    ));
    // `#` is a line comment on MySQL; its quote cannot open a literal.
    assert!(exposed_on(
        "SELECT 1 # '\nDELETE FROM t; -- '",
        LexEngine::MySql,
        "DELETE"
    ));
    // Block comments do not nest on MySQL.
    assert!(exposed_on(
        "SELECT 1 /* /* */ DELETE FROM t; -- */",
        LexEngine::MySql,
        "DELETE"
    ));
    // The shadow never contains a comment opener a reading did not see.
    for v in views("SELECT 1--1", LexTarget::Engine(LexEngine::MySql)).views {
        assert!(!v.shadow.contains("--"), "{:?}", v.shadow);
    }
}

#[test]
fn mysql_executable_comments_are_judged_at_every_version() {
    for sql in [
        "/*!50003 DELETE FROM t */ SELECT 1",
        "/*! DELETE FROM t */ SELECT 1",
        "/*M!100100 DELETE FROM t */ SELECT 1",
        "/*T![clustered_index] DELETE FROM t */ SELECT 1",
        // A version too new for the server: an ordinary comment, so its quote
        // does not open a literal and the DELETE after it runs.
        "/*!99999 ' */ DELETE FROM t; -- '",
        // A comment that does not run may hold one nested comment.
        "/*!99999 /* */ ' */ DELETE FROM t; -- '",
        // Mixed versions: the server runs the first and skips the second;
        // reading both as run or both as skipped hides the DELETE.
        "/*!10000 \"*/\"*/ /*!99999 ' */ ; DELETE FROM t; -- \"'",
    ] {
        assert!(exposed_on(sql, LexEngine::MySql, "DELETE"), "{sql}");
    }
    // Hints and ordinary comments stay inert; text in literals stays inert.
    for sql in [
        "/*+ DELETE */ SELECT 1",
        "/* DELETE */ SELECT 1",
        "SELECT '/*!50003 DELETE FROM t */'",
    ] {
        assert!(hidden_on(sql, LexEngine::MySql, "DELETE"), "{sql}");
    }
    // Executable comments are MySQL-only.
    assert!(hidden_on(
        "/*!50003 DELETE FROM t */ SELECT 1",
        LexEngine::Postgres,
        "DELETE"
    ));
}

#[test]
fn postgres_quoting_rules() {
    // Dollar quotes hold quotes; the DELETE between two bodies is live.
    assert!(exposed_on(
        "SELECT $$'$$; DELETE FROM t; SELECT $$'$$",
        LexEngine::Postgres,
        "DELETE"
    ));
    assert!(hidden_on(
        "SELECT $a$ ; DELETE FROM t; $a$",
        LexEngine::Postgres,
        "DELETE"
    ));
    // `$1` is a parameter, not a tag.
    assert!(exposed_on(
        "SELECT $1; DELETE FROM t; SELECT $1",
        LexEngine::Postgres,
        "DELETE"
    ));
    // A dollar quote cannot start inside an identifier.
    assert!(exposed_on(
        "SELECT a$$b; DELETE FROM t; SELECT 1 AS c$$",
        LexEngine::Postgres,
        "DELETE"
    ));
    // E'' strings take backslash escapes.
    assert!(exposed_on(
        r"SELECT E'\''; DELETE FROM t; SELECT '",
        LexEngine::Postgres,
        "DELETE"
    ));
    // standard_conforming_strings = off makes plain literals escape too.
    assert!(exposed_on(
        r"SELECT 'a\'; DELETE FROM t; -- '",
        LexEngine::Postgres,
        "DELETE"
    ));
    // Nested comments.
    assert!(exposed_on(
        "SELECT 1 /* /* */ ' */ DELETE FROM t; -- '",
        LexEngine::Postgres,
        "DELETE"
    ));
    assert!(hidden_on(
        "SELECT 1 /* /* */ DELETE FROM t; */",
        LexEngine::Postgres,
        "DELETE"
    ));
    // A PostgreSQL `--` comment ends at a bare carriage return.
    assert!(exposed_on(
        "SELECT 1 -- '\rDELETE FROM t; -- '",
        LexEngine::Postgres,
        "DELETE"
    ));
    // A function body in dollar quotes containing quotes and `;`.
    assert!(exposed_on(
        "CREATE FUNCTION f() RETURNS int AS $$ SELECT 'x'; $$ LANGUAGE sql; DELETE FROM t",
        LexEngine::Postgres,
        "DELETE"
    ));
}

#[test]
fn cockroach_byte_strings_take_escapes() {
    assert!(exposed_on(
        r"SELECT b'\''; DELETE FROM t; SELECT '",
        LexEngine::CockroachDb,
        "DELETE"
    ));
}

#[test]
fn tsql_rules() {
    // Nested comments hide on T-SQL…
    assert!(hidden_on(
        "/* /* */ DELETE FROM t */ SELECT 1",
        LexEngine::TSql,
        "DELETE"
    ));
    // …and the nesting cannot be used to smuggle a quote.
    assert!(exposed_on(
        "SELECT 1 /* /* */ ' */ DELETE FROM t; -- '",
        LexEngine::TSql,
        "DELETE"
    ));
    // No backslash escapes: `'a\'` is a complete literal.
    assert!(exposed_on(
        r"SELECT 'a\'; DELETE FROM t",
        LexEngine::TSql,
        "DELETE"
    ));
    // A bare carriage return is read both ways.
    assert!(exposed_on(
        "SELECT 1 -- '\rDELETE FROM t; -- '",
        LexEngine::TSql,
        "DELETE"
    ));
}

#[test]
fn sqlite_rules() {
    // TCL-style variables may contain quote characters.
    assert!(exposed_on(
        "SELECT $a(');DELETE FROM t;SELECT $a(')",
        LexEngine::Sqlite,
        "DELETE"
    ));
    // Brackets have no escape on SQLite.
    assert!(exposed_on(
        "SELECT [a]; DELETE FROM t; SELECT [b]",
        LexEngine::Sqlite,
        "DELETE"
    ));
    // Block comments do not nest.
    assert!(exposed_on(
        "SELECT 1 /* /* */ DELETE FROM t; -- */",
        LexEngine::Sqlite,
        "DELETE"
    ));
}

#[test]
fn oracle_alternative_quoting() {
    for sql in [
        "SELECT q'[']' FROM dual; DELETE FROM t",
        "SELECT Q'{'}' FROM dual; DELETE FROM t",
        "SELECT nq'('' )' FROM dual; DELETE FROM t",
        "SELECT q'<'>' FROM dual; DELETE FROM t",
        "SELECT q'!'!' FROM dual; DELETE FROM t",
    ] {
        assert!(exposed_on(sql, LexEngine::Oracle, "DELETE"), "{sql}");
    }
    assert!(hidden_on(
        "SELECT q'[ ; DELETE FROM t; ]' FROM dual",
        LexEngine::Oracle,
        "DELETE"
    ));
}

#[test]
fn clickhouse_rules() {
    // Backslash escapes in every quoting style.
    assert!(hidden_on(
        "SELECT 1 AS `a\\`; DELETE FROM t; --`",
        LexEngine::ClickHouse,
        "DELETE"
    ));
    assert!(exposed_on(
        r"SELECT 'a\\'; DELETE FROM t",
        LexEngine::ClickHouse,
        "DELETE"
    ));
    // `#` is a comment only as `# ` or `#!`.
    assert!(exposed_on(
        "SELECT 1 # '\nDELETE FROM t; -- '",
        LexEngine::ClickHouse,
        "DELETE"
    ));
    // Heredocs, and their absence on older servers.
    assert!(exposed_on(
        "SELECT $x$'$x$; DELETE FROM t; SELECT $x$'$x$",
        LexEngine::ClickHouse,
        "DELETE"
    ));
    assert!(exposed_on(
        "SELECT $x$ ; DELETE FROM t; $x$",
        LexEngine::ClickHouse,
        "DELETE"
    ));
    // Nested and flat block comments both judged.
    assert!(exposed_on(
        "SELECT 1 /* /* */ ; DELETE FROM t; -- */",
        LexEngine::ClickHouse,
        "DELETE"
    ));
}

#[test]
fn comment_openers_in_literals_and_literal_openers_in_comments() {
    for engine in LexEngine::ALL {
        for sql in [
            "SELECT '/*'; DELETE FROM t; SELECT '*/'",
            "SELECT '--'; DELETE FROM t",
            "SELECT 1 /* ' */; DELETE FROM t; -- '",
            "SELECT 1 -- '\nDELETE FROM t; -- '",
            "SELECT 1 -- c\r\nDELETE FROM t",
        ] {
            assert!(exposed_on(sql, engine, "DELETE"), "{engine:?}: {sql:?}");
        }
    }
}

#[test]
fn lookalike_quotes_are_not_quotes() {
    for engine in LexEngine::ALL {
        let sql = "SELECT \u{FF07}; DELETE FROM t; \u{FF07} \u{2019}";
        assert!(exposed_on(sql, engine, "DELETE"), "{engine:?}");
    }
}

#[test]
fn unterminated_constructs_hide_nothing_and_are_reported() {
    for engine in LexEngine::ALL {
        for sql in [
            "SELECT 'x; DELETE FROM t",
            "SELECT \"x; DELETE FROM t",
            "SELECT 1 /* DELETE FROM t",
        ] {
            let all = views(sql, LexTarget::Engine(engine));
            assert!(exposed_on(sql, engine, "DELETE"), "{engine:?}: {sql}");
            assert!(all.ambiguity().is_some(), "{engine:?}: {sql}");
        }
    }
    assert!(
        views("SELECT 1", LexTarget::AnyEngine)
            .ambiguity()
            .is_none(),
        "clean text is not ambiguous"
    );
}

#[test]
fn nul_bytes_are_ambiguous() {
    let all = views("SELECT 1\0; DELETE FROM t", LexTarget::AnyEngine);
    assert!(all.ambiguity().unwrap().contains("NUL"));
}

#[test]
fn ordinary_sql_has_one_reading_per_engine() {
    for engine in LexEngine::ALL {
        let all = views(
            "SELECT a, 'x' FROM t WHERE b = 1 -- note\n",
            LexTarget::Engine(engine),
        );
        assert_eq!(all.views.len(), 1, "{engine:?}");
    }
}

#[test]
fn engines_map_from_database_types() {
    use crate::sqlmodel::DatabaseType as D;
    assert_eq!(
        LexEngine::for_database(D::AzureSqlDatabase),
        Some(LexEngine::TSql)
    );
    assert_eq!(
        LexEngine::for_database(D::Supabase),
        Some(LexEngine::Postgres)
    );
    assert_eq!(
        LexEngine::for_database(D::CockroachDb),
        Some(LexEngine::CockroachDb)
    );
    assert_eq!(LexEngine::for_database(D::MariaDb), Some(LexEngine::MySql));
    assert_eq!(LexEngine::for_database(D::TiDb), Some(LexEngine::MySql));
    assert_eq!(LexEngine::for_database(D::LibSql), Some(LexEngine::Sqlite));
    assert_eq!(LexEngine::for_database(D::Snowflake), None);
    assert_eq!(LexTarget::for_database(D::Snowflake), LexTarget::AnyEngine);
}

mod properties {
    use super::*;
    use proptest::prelude::*;

    fn sqlish() -> impl Strategy<Value = String> {
        let atoms = prop::sample::select(vec![
            "'",
            "\"",
            "`",
            "[",
            "]",
            "\\",
            "--",
            "-",
            "#",
            "/*",
            "*/",
            "/*!",
            "/*!50003 ",
            "/*M!100100 ",
            "$$",
            "$a$",
            "$1",
            "E'",
            "q'[",
            "]'",
            "nq'{",
            "}'",
            ";",
            "\n",
            "\r",
            " ",
            "DELETE",
            "SELECT",
            "x",
            "1",
            "(",
            ")",
            "é",
            "\u{FF07}",
            "@a(",
            ":b",
            "GO\n",
        ]);
        prop::collection::vec(atoms, 0..40).prop_map(|parts| parts.concat())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(400))]

        /// The shadow contract every consumer relies on.
        #[test]
        fn shadows_keep_length_utf8_and_carry_no_hidden_openers(sql in sqlish()) {
            let all = views(&sql, LexTarget::AnyEngine);
            for view in &all.views {
                prop_assert_eq!(view.shadow.len(), sql.len());
                for token in view.tokens() {
                    prop_assert!(token.kind != TokenKind::Comment, "{:?} in {:?}", token.text, view.shadow);
                    if token.kind == TokenKind::StringLiteral {
                        // Adjacent literals merge into one (`'' ''` reads as
                        // a doubled quote); either way nothing but blanks and
                        // quotes is inside.
                        let inner = &token.text[1..token.text.len().saturating_sub(1).max(1)];
                        prop_assert!(
                            token.text.len() >= 2
                                && token.text.ends_with('\'')
                                && inner.bytes().all(|b| b.is_ascii_whitespace() || b == b'\''),
                            "{:?} in {:?}", token.text, view.shadow
                        );
                    }
                }
            }
        }

        /// A statement verb appended after a `;` in live code is exposed by
        /// the reading that has the prefix in code at that point — here we
        /// check the weaker, always-true direction: a verb the default
        /// reading exposes is exposed in the any-engine union too.
        #[test]
        fn engine_views_are_contained_in_the_union(sql in sqlish()) {
            for engine in LexEngine::ALL {
                let one = views(&sql, LexTarget::Engine(engine));
                let any = views(&sql, LexTarget::AnyEngine);
                for v in &one.views {
                    prop_assert!(any.views.iter().any(|a| a.shadow == v.shadow));
                }
            }
        }
    }
}

/// Rules pinned against live engines on 2026-09-28 (MySQL 8, PostgreSQL 16,
/// SQL Server 2025, Oracle Free 23/26ai, ClickHouse 26.9, SQLite 3). Each
/// assertion is what the *default* reading must say, since that is what the
/// server did.
mod verified_live {
    use super::*;

    fn primary_exposes(sql: &str, engine: LexEngine, word: &str) -> bool {
        let all = views(sql, LexTarget::Engine(engine));
        bare_words(all.primary()).iter().any(|w| w == word)
    }

    #[test]
    fn sql_server_ends_a_line_comment_at_a_bare_carriage_return() {
        assert!(primary_exposes(
            "SELECT 'A' -- x\rDELETE FROM t",
            LexEngine::TSql,
            "DELETE"
        ));
    }

    #[test]
    fn oracle_does_not() {
        assert!(!primary_exposes(
            "SELECT 'A' -- x\rDELETE FROM t",
            LexEngine::Oracle,
            "DELETE"
        ));
    }

    #[test]
    fn mysql_runs_the_first_versioned_comment_and_skips_the_second() {
        // Live: returned `1 | */` then VISIBLE_M8.
        let sql = "SELECT 1 /*!10000 , \"*/\"*/ /*!99999 ' */ ; DELETE FROM t; -- \"'";
        assert!(exposed_on(sql, LexEngine::MySql, "DELETE"));
    }

    #[test]
    fn clickhouse_identifiers_may_contain_dollar_signs() {
        // `b$x$a` is one identifier, so `$x$` there cannot open a heredoc.
        let sql = "SELECT b$x$a; DELETE FROM t; SELECT $x$";
        assert!(primary_exposes(sql, LexEngine::ClickHouse, "DELETE"));
        // Heredoc tags are ASCII word characters: `$a b$` quotes nothing.
        assert!(primary_exposes(
            "SELECT $a b$; DELETE FROM t; SELECT $a b$",
            LexEngine::ClickHouse,
            "DELETE"
        ));
        assert!(!primary_exposes(
            "SELECT $1$ ; DELETE FROM t; $1$",
            LexEngine::ClickHouse,
            "DELETE"
        ));
    }

    #[test]
    fn sqlite_accepts_an_unclosed_block_comment_but_it_hides_nothing_here() {
        let all = views(
            "SELECT 1; /* DELETE FROM t",
            LexTarget::Engine(LexEngine::Sqlite),
        );
        assert!(bare_words(all.primary()).contains(&"DELETE".to_string()));
    }
}

/// Coarse regression tripwire (AGENTS.md performance mandate), not a
/// benchmark: a ~1 MB script read under every engine stays linear.
#[test]
fn large_scripts_read_in_linear_time() {
    use std::time::{Duration, Instant};
    let sql = "SELECT a, 'x' FROM t WHERE b = 1; -- note\n".repeat(25_000);
    let started = Instant::now();
    let all = views(&sql, LexTarget::AnyEngine);
    let elapsed = started.elapsed();
    assert_eq!(all.views.len(), LexEngine::ALL.len());
    assert!(
        elapsed < Duration::from_millis(2_000),
        "reading {} bytes under every engine took {elapsed:?}",
        sql.len()
    );
}
