//! Apply to Query carries the grid's sort into the editor as a top-level
//! `ORDER BY` (Sequel Ace #2193).
//!
//! [`order_by_clause_for`] renders the sort keys in the engine's identifier
//! quoting; [`apply_order_by`] replaces the statement's own top-level
//! `ORDER BY` (or inserts one ahead of `LIMIT` / `OFFSET` / `FETCH` /
//! `OPTION` / `FOR UPDATE`-style tails), leaving window and subquery
//! `ORDER BY`s inside parentheses alone.

use sqlly_core::ResultColumn;
use sqlly_lang::sqlmodel::DatabaseType;

use crate::grid_sort::SortKey;
use crate::sql_literal::sql_identifier_for;

/// `ORDER BY a ASC, b DESC` for `keys`, whose columns index `columns` (the
/// result columns, staged-edit marker already removed). Keys past the result
/// columns (computed columns that exist only in the grid) are skipped; `None`
/// when no key is left.
///
/// A key names its column by the result label when that label is unique, so
/// select-list aliases and expressions sort exactly as shown. A duplicated or
/// empty label falls back to the source column the statement attributes it
/// to, and then to the column's 1-based position.
pub fn order_by_clause_for(
    db: DatabaseType,
    columns: &[ResultColumn],
    keys: &[SortKey],
    sql: Option<&str>,
) -> Option<String> {
    let provenance =
        sql.and_then(sqlly_intel::query::column_provenance::StatementProvenance::parse);
    let mut seen: Vec<usize> = Vec::new();
    let mut terms: Vec<String> = Vec::new();
    for key in keys {
        let Some(col) = columns.get(key.column) else {
            continue;
        };
        if seen.contains(&key.column) {
            continue;
        }
        seen.push(key.column);
        let occurrences = columns
            .iter()
            .filter(|c| c.name.eq_ignore_ascii_case(&col.name))
            .count();
        let name = if !col.name.trim().is_empty() && occurrences == 1 {
            sql_identifier_for(db, &col.name)
        } else if let Some(source) = provenance
            .as_ref()
            .and_then(|p| p.resolve(&col.name, key.column, occurrences))
        {
            crate::where_actions::qualified_identifier_for(db, Some(&source), &col.name)
        } else {
            (key.column + 1).to_string()
        };
        let direction = if key.descending { "DESC" } else { "ASC" };
        terms.push(format!("{name} {direction}"));
    }
    if terms.is_empty() {
        None
    } else {
        Some(format!("ORDER BY {}", terms.join(", ")))
    }
}

/// Puts `order_by` (a full `ORDER BY …` clause) on the last statement of
/// `sql`: replaces its top-level `ORDER BY` up to any paging/locking tail,
/// or inserts the clause ahead of that tail, or appends it. A trailing `;`
/// and trailing whitespace are kept.
pub fn apply_order_by(sql: &str, order_by: &str) -> String {
    let (words, _) = top_level_words(sql);
    let body_end = trailing_terminator_start(sql);
    let words: Vec<&Word> = words.iter().filter(|w| w.start < body_end).collect();
    let statement_start = words
        .iter()
        .rev()
        .find(|w| w.kind == WordKind::Semicolon)
        .map_or(0, |w| w.start + 1);
    let words: Vec<&Word> = words
        .into_iter()
        .filter(|w| w.start >= statement_start && w.kind == WordKind::Word)
        .collect();

    let existing = words.iter().enumerate().rev().find_map(|(i, w)| {
        (w.text.eq_ignore_ascii_case("ORDER")
            && words
                .get(i + 1)
                .is_some_and(|next| next.text.eq_ignore_ascii_case("BY")))
        .then_some((i, w.start))
    });
    let tail_after = |from: usize| {
        words
            .iter()
            .enumerate()
            .skip(from)
            .find(|(i, w)| is_tail_keyword(&words, *i, w))
            .map(|(_, w)| w.start)
    };

    let (start, end) = match existing {
        Some((i, start)) => (start, tail_after(i + 2).unwrap_or(body_end)),
        None => {
            let at = tail_after(0).unwrap_or(body_end);
            (at, at)
        }
    };

    let head = sql[..start].trim_end();
    let rest = &sql[end..];
    let mut out = String::with_capacity(sql.len() + order_by.len() + 2);
    out.push_str(head);
    if !head.is_empty() {
        // A clause after a trailing `-- comment` must start on its own line.
        let ends_in_comment = top_level_words(head).1;
        out.push(
            if ends_in_comment || (head.contains('\n') && end == body_end) {
                '\n'
            } else {
                ' '
            },
        );
    }
    out.push_str(order_by);
    if end < body_end {
        out.push(' ');
        out.push_str(rest.trim_start());
    } else {
        out.push_str(&sql[body_end..]);
    }
    out
}

/// Start of the trailing `;` + whitespace (or of trailing whitespace alone).
fn trailing_terminator_start(sql: &str) -> usize {
    let trimmed = sql.trim_end();
    let trimmed = trimmed.strip_suffix(';').unwrap_or(trimmed).trim_end();
    trimmed.len()
}

/// Keywords that end an `ORDER BY` clause and come after it.
fn is_tail_keyword(words: &[&Word], i: usize, word: &Word) -> bool {
    let text = word.text.as_str();
    let is = |kw: &str| text.eq_ignore_ascii_case(kw);
    if is("LIMIT") || is("OFFSET") || is("FETCH") || is("OPTION") {
        return true;
    }
    if is("FOR") {
        // Locking / output tails only; `FOR SYSTEM_TIME` belongs to FROM.
        return words.get(i + 1).is_some_and(|next| {
            [
                "UPDATE", "SHARE", "NO", "KEY", "XML", "JSON", "BROWSE", "READ",
            ]
            .iter()
            .any(|kw| next.text.eq_ignore_ascii_case(kw))
        });
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordKind {
    Word,
    Semicolon,
}

#[derive(Debug)]
struct Word {
    start: usize,
    text: String,
    kind: WordKind,
}

/// Bare words and `;` at parenthesis depth 0, outside strings, quoted
/// identifiers, and comments, plus whether `sql` ends inside a `--` comment.
fn top_level_words(sql: &str) -> (Vec<Word>, bool) {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut ends_in_line_comment = false;
    let mut depth: i32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth = (depth - 1).max(0);
                i += 1;
            }
            b';' => {
                if depth == 0 {
                    out.push(Word {
                        start: i,
                        text: ";".into(),
                        kind: WordKind::Semicolon,
                    });
                }
                i += 1;
            }
            b'\'' | b'"' | b'`' => i = skip_quoted(bytes, i, b),
            b'[' => i = skip_quoted(bytes, i, b']'),
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                ends_in_line_comment = i == bytes.len();
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            _ if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                if depth == 0 {
                    out.push(Word {
                        start,
                        text: sql[start..i].to_string(),
                        kind: WordKind::Word,
                    });
                }
            }
            _ => {
                // Multi-byte scalars contain no ASCII bytes, so stepping one
                // byte at a time never mistakes their tail for syntax.
                i += 1;
            }
        }
    }
    (out, ends_in_line_comment)
}

/// Index just past the quoted run opened at `open` and closed by `close`
/// (a doubled closer is an escaped one).
fn skip_quoted(bytes: &[u8], open: usize, close: u8) -> usize {
    let mut i = open + 1;
    while i < bytes.len() {
        if bytes[i] == close {
            if bytes.get(i + 1) == Some(&close) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        if close == b'\'' && bytes[i] == b'\\' && i + 1 < bytes.len() {
            // MySQL-style backslash escapes; harmless elsewhere because a
            // backslash before the closing quote is rare outside MySQL.
            i += 2;
            continue;
        }
        i += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: "int".into(),
            nullable: true,
        }
    }

    fn asc(column: usize) -> SortKey {
        SortKey {
            column,
            descending: false,
        }
    }

    fn desc(column: usize) -> SortKey {
        SortKey {
            column,
            descending: true,
        }
    }

    #[test]
    fn clause_quotes_per_dialect_and_keeps_key_order() {
        let columns = vec![col("id"), col("last name"), col("created")];
        let keys = vec![desc(2), asc(1)];
        assert_eq!(
            order_by_clause_for(DatabaseType::SqlServer, &columns, &keys, None).unwrap(),
            "ORDER BY [created] DESC, [last name] ASC"
        );
        assert_eq!(
            order_by_clause_for(DatabaseType::Postgres, &columns, &keys, None).unwrap(),
            "ORDER BY \"created\" DESC, \"last name\" ASC"
        );
        assert_eq!(
            order_by_clause_for(DatabaseType::MySql, &columns, &keys, None).unwrap(),
            "ORDER BY `created` DESC, `last name` ASC"
        );
        assert_eq!(
            order_by_clause_for(DatabaseType::MariaDb, &columns, &keys, None).unwrap(),
            "ORDER BY `created` DESC, `last name` ASC"
        );
    }

    #[test]
    fn clause_escapes_quote_characters_in_names() {
        let columns = vec![col("a]b"), col("c`d")];
        assert_eq!(
            order_by_clause_for(DatabaseType::SqlServer, &columns, &[asc(0)], None).unwrap(),
            "ORDER BY [a]]b] ASC"
        );
        assert_eq!(
            order_by_clause_for(DatabaseType::MySql, &columns, &[desc(1)], None).unwrap(),
            "ORDER BY `c``d` DESC"
        );
    }

    #[test]
    fn clause_skips_grid_only_columns_and_duplicates() {
        let columns = vec![col("id")];
        assert_eq!(
            order_by_clause_for(DatabaseType::Postgres, &columns, &[asc(5)], None),
            None
        );
        assert_eq!(
            order_by_clause_for(DatabaseType::Postgres, &columns, &[asc(0), desc(0)], None)
                .unwrap(),
            "ORDER BY \"id\" ASC"
        );
        assert_eq!(
            order_by_clause_for(DatabaseType::Postgres, &columns, &[], None),
            None
        );
    }

    #[test]
    fn clause_disambiguates_duplicate_labels() {
        let columns = vec![col("name"), col("name")];
        let sql = "SELECT p.name, c.name FROM people p JOIN cities c ON c.id = p.city_id";
        assert_eq!(
            order_by_clause_for(DatabaseType::Postgres, &columns, &[asc(1)], Some(sql)).unwrap(),
            "ORDER BY c.\"name\" ASC"
        );
        // No statement to attribute from: position.
        assert_eq!(
            order_by_clause_for(DatabaseType::Postgres, &columns, &[desc(1)], None).unwrap(),
            "ORDER BY 2 DESC"
        );
        // An unnamed expression column sorts by position.
        assert_eq!(
            order_by_clause_for(DatabaseType::SqlServer, &[col("")], &[asc(0)], None).unwrap(),
            "ORDER BY 1 ASC"
        );
    }

    #[test]
    fn appends_when_there_is_no_order_by() {
        assert_eq!(
            apply_order_by("SELECT * FROM t WHERE a = 1", "ORDER BY [a] ASC"),
            "SELECT * FROM t WHERE a = 1 ORDER BY [a] ASC"
        );
        assert_eq!(
            apply_order_by("SELECT * FROM t;\n", "ORDER BY `a` ASC"),
            "SELECT * FROM t ORDER BY `a` ASC;\n"
        );
        assert_eq!(
            apply_order_by("SELECT *\nFROM t\n", "ORDER BY \"a\" ASC"),
            "SELECT *\nFROM t\nORDER BY \"a\" ASC\n"
        );
    }

    #[test]
    fn replaces_an_existing_top_level_order_by() {
        assert_eq!(
            apply_order_by("SELECT * FROM t ORDER BY b DESC, c", "ORDER BY [a] ASC"),
            "SELECT * FROM t ORDER BY [a] ASC"
        );
        assert_eq!(
            apply_order_by("select * from t order by b;", "ORDER BY `a` DESC"),
            "select * from t ORDER BY `a` DESC;"
        );
    }

    #[test]
    fn keeps_paging_and_locking_tails() {
        assert_eq!(
            apply_order_by("SELECT * FROM t ORDER BY b LIMIT 10", "ORDER BY `a` ASC"),
            "SELECT * FROM t ORDER BY `a` ASC LIMIT 10"
        );
        assert_eq!(
            apply_order_by("SELECT * FROM t LIMIT 10 OFFSET 5", "ORDER BY \"a\" ASC"),
            "SELECT * FROM t ORDER BY \"a\" ASC LIMIT 10 OFFSET 5"
        );
        assert_eq!(
            apply_order_by(
                "SELECT * FROM t ORDER BY b OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY",
                "ORDER BY [a] DESC"
            ),
            "SELECT * FROM t ORDER BY [a] DESC OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY"
        );
        assert_eq!(
            apply_order_by("SELECT * FROM t FOR UPDATE", "ORDER BY \"a\" ASC"),
            "SELECT * FROM t ORDER BY \"a\" ASC FOR UPDATE"
        );
        assert_eq!(
            apply_order_by("SELECT * FROM t OPTION (RECOMPILE)", "ORDER BY [a] ASC"),
            "SELECT * FROM t ORDER BY [a] ASC OPTION (RECOMPILE)"
        );
        // Temporal FOR belongs to FROM, not a tail.
        assert_eq!(
            apply_order_by("SELECT * FROM t FOR SYSTEM_TIME ALL", "ORDER BY [a] ASC"),
            "SELECT * FROM t FOR SYSTEM_TIME ALL ORDER BY [a] ASC"
        );
    }

    #[test]
    fn ignores_nested_quoted_and_commented_order_by() {
        let sql = "SELECT ROW_NUMBER() OVER (ORDER BY x) AS rn, 'order by y' AS s \
                   FROM (SELECT x FROM t ORDER BY x LIMIT 3) q -- order by z\n";
        assert_eq!(
            apply_order_by(sql, "ORDER BY \"rn\" DESC"),
            format!("{}\nORDER BY \"rn\" DESC\n", sql.trim_end())
        );
        assert_eq!(
            apply_order_by("SELECT `order by` FROM t", "ORDER BY `a` ASC"),
            "SELECT `order by` FROM t ORDER BY `a` ASC"
        );
    }

    #[test]
    fn clause_after_a_trailing_line_comment_starts_a_new_line() {
        assert_eq!(
            apply_order_by("SELECT * FROM t -- newest first", "ORDER BY [a] ASC"),
            "SELECT * FROM t -- newest first\nORDER BY [a] ASC"
        );
    }

    #[test]
    fn targets_the_last_statement_only() {
        assert_eq!(
            apply_order_by(
                "SELECT 1 ORDER BY 1; SELECT * FROM t ORDER BY b;",
                "ORDER BY [a] ASC"
            ),
            "SELECT 1 ORDER BY 1; SELECT * FROM t ORDER BY [a] ASC;"
        );
    }

    #[test]
    fn union_order_by_is_the_final_one() {
        assert_eq!(
            apply_order_by(
                "SELECT a FROM t UNION SELECT a FROM u ORDER BY a",
                "ORDER BY \"a\" DESC"
            ),
            "SELECT a FROM t UNION SELECT a FROM u ORDER BY \"a\" DESC"
        );
    }

    #[test]
    fn survives_multibyte_text() {
        assert_eq!(
            apply_order_by("SELECT 'niño' AS señor FROM t", "ORDER BY \"señor\" ASC"),
            "SELECT 'niño' AS señor FROM t ORDER BY \"señor\" ASC"
        );
    }
}
