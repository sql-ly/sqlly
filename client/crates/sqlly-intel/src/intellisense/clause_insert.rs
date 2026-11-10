//! Completions that write the next clause for you. Two positions offer them:
//!
//! - Right after a finished SELECT list with no FROM yet (`SELECT * f▏`):
//!   `FROM` (and `INTO` in statement position) lead, then the table-like
//!   objects, each inserting `FROM`, a newline, one indent and the name. No
//!   columns. See feedback ifb_9f91.
//! - Right after a FROM source (`FROM t⏎wh▏`): `WHERE` leads the clause
//!   keywords, then the in-scope columns, each inserting `WHERE`, a newline,
//!   one indent and the column. No tables, views or routines. See feedback
//!   ifb_92d9.
//!
//! Position detection is shared with the model engine's cursor ladder so both
//! completers agree on where these apply.

/// `detail` carried by a table item whose insert text writes the `FROM`
/// clause. The editor keys off it to re-indent the insert with the user's
/// indent-unit preference, and it tells the reader what accepting does.
pub const FROM_CLAUSE_DETAIL: &str = "adds FROM";

/// `detail` carried by a column item whose insert text writes the `WHERE`
/// clause; the `WHERE` counterpart of [`FROM_CLAUSE_DETAIL`].
pub const WHERE_CLAUSE_DETAIL: &str = "adds WHERE";

/// The clause keyword a completion item's insert writes, keyed off its
/// `detail` marker, or `None` for an ordinary item.
pub fn clause_keyword_for_detail(detail: &str) -> Option<&'static str> {
    match detail {
        FROM_CLAUSE_DETAIL => Some("FROM"),
        WHERE_CLAUSE_DETAIL => Some("WHERE"),
        _ => None,
    }
}

/// The clause keywords legal at `cursor` when it sits after a finished SELECT
/// list with no FROM yet — `["FROM"]` or `["FROM", "INTO"]` — or `None` for
/// any other position.
pub fn after_select_list_keywords(sql: &str, cursor: usize) -> Option<Vec<&'static str>> {
    crate::modelengine::intellisense::after_select_list_keywords(sql, cursor)
}

/// True when `cursor` sits after a FROM source where the in-scope columns are
/// offered with WHERE-writing inserts — the `FROM t⏎▏` position, not a word
/// that may still be the table's alias.
pub fn post_from_offers_columns(sql: &str, cursor: usize) -> bool {
    crate::modelengine::intellisense::post_from_clause(sql, cursor)
        .is_some_and(|clause| clause.offer_columns)
}

/// `detail` carried by a column item that the query's GROUP BY still needs —
/// a plain select-list column not yet grouped. The popup marks these as
/// required (feedback ifb_75bd).
pub const REQUIRED_GROUP_BY_DETAIL: &str = "required by SELECT";

/// Score lifting required GROUP BY columns above every other candidate.
pub(crate) const REQUIRED_GROUP_BY_SCORE: i32 = 20_000;

/// The select-list columns still missing from the GROUP BY list the caret is
/// in, as written in the select list, filtered by the typed `prefix`; empty
/// when the caret is not in a GROUP BY list or nothing is missing.
pub fn required_group_by_labels(sql: &str, cursor: usize, prefix: &str) -> Vec<String> {
    let Some(required) = crate::query::group_by::required_group_by_columns(sql, cursor) else {
        return Vec::new();
    };
    let mut labels: Vec<String> = Vec::new();
    for column in required {
        let bare = column.text.rsplit('.').next().unwrap_or(&column.text);
        let matches = prefix.is_empty()
            || crate::modelengine::intellisense::fuzzy::score(prefix, &column.text).is_some()
            || crate::modelengine::intellisense::fuzzy::score(prefix, bare).is_some();
        if matches && !labels.iter().any(|l| l.eq_ignore_ascii_case(&column.text)) {
            labels.push(column.text);
        }
    }
    labels
}

/// The indent unit the document already uses: the leading whitespace of its
/// first indented line when that is a tab, else that line's run of spaces;
/// a tab when nothing is indented yet.
pub fn infer_indent_unit(sql: &str) -> String {
    for line in sql.lines() {
        let lead: &str = &line[..line.len() - line.trim_start_matches([' ', '\t']).len()];
        if lead.is_empty() || line.trim().is_empty() {
            continue;
        }
        if lead.starts_with('\t') {
            return "\t".to_string();
        }
        return " ".repeat(lead.len());
    }
    "\t".to_string()
}

/// Insert text for accepting `name` at `replace_start`: the clause `keyword`,
/// a newline, the caret line's own indentation plus one `unit`, then the name.
pub fn clause_insert_text(
    sql: &str,
    replace_start: usize,
    keyword: &str,
    name: &str,
    unit: &str,
) -> String {
    let at = replace_start.min(sql.len());
    let line_start = sql[..at].rfind('\n').map_or(0, |i| i + 1);
    let line = &sql[line_start..at];
    let indent = &line[..line.len() - line.trim_start_matches([' ', '\t']).len()];
    format!("{keyword}\n{indent}{unit}{name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indent_unit_follows_the_document() {
        assert_eq!(infer_indent_unit("SELECT\n\t*\nf"), "\t");
        assert_eq!(infer_indent_unit("SELECT\n    a,\n    b\nf"), "    ");
        assert_eq!(infer_indent_unit("SELECT * f"), "\t");
    }

    #[test]
    fn insert_keeps_the_caret_lines_indent_and_adds_one_unit() {
        let sql = "SELECT\n\t*\nf";
        assert_eq!(
            clause_insert_text(sql, sql.len() - 1, "FROM", "Forms.Form", "\t"),
            "FROM\n\tForms.Form"
        );
        let nested = "  SELECT *\n  f";
        assert_eq!(
            clause_insert_text(nested, nested.len() - 1, "FROM", "dbo.T", "  "),
            "FROM\n    dbo.T"
        );
    }

    #[test]
    fn position_is_only_after_a_finished_select_list() {
        let at = |s: &str| after_select_list_keywords(s, s.len());
        assert_eq!(at("SELECT\n\t*\nf"), Some(vec!["FROM", "INTO"]));
        assert_eq!(at("SELECT a, b "), Some(vec!["FROM", "INTO"]));
        assert!(at("SELECT a, ").is_none());
        assert!(at("SELECT DISTINCT f").is_none());
        assert!(at("SELECT TOP 10 f").is_none());
        assert!(at("SELECT a AS f").is_none());
        assert!(at("SELECT CASE WHEN x THEN y ELSE f").is_none());
        assert!(at("SELECT a * f").is_none());
        assert!(at("SELECT (a f").is_none());
        assert!(at("SELECT * FROM t WHERE f").is_none());
    }
}
