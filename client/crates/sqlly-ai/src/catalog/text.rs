//! Shared, parser-free text utilities for SQL keyword scanning.
//!
//! `sqlly_lang::sqlparse` does not structure WHERE/ON predicates, so the safety checks
//! in [`crate::catalog::validation`] fall back to deterministic text scans. These helpers
//! centralise the scanning primitives (whole-word containment, string/comment
//! stripping, and paren-depth-aware keyword/predicate detection) so the rules
//! stay consistent and are unit-tested in one place.

/// Whole-word containment over a haystack, so "set" does not match "asset" and
/// "where" does not match "elsewhere". Caller is responsible for case-folding.
pub(crate) fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let at = start + pos;
        let before_ok = at == 0
            || !haystack[..at]
                .chars()
                .next_back()
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false);
        let after = at + needle.len();
        let after_ok = after >= haystack.len()
            || !haystack[after..]
                .chars()
                .next()
                .map(|c| c.is_alphanumeric() || c == '_')
                .unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        start = at + needle.len();
    }
    false
}

/// Remove string literals and comments exactly as `dialect`'s engine bounds
/// them (the security lexer's editor reading: backslash escapes and `#`
/// comments on MySQL, nested comments on SQL Server and PostgreSQL, dollar
/// quotes, `q'[…]'`, …). Each literal/comment becomes one space. Quoted
/// identifiers stay visible so quoted column names still match, normalized
/// to `"name"` with any inner quote/bracket replaced — so a quote character
/// inside `` `'` `` or `"'"` can never open a fake literal downstream. An
/// unterminated literal or comment swallows the rest of the text (these
/// scans look for WHERE clauses and key pins, which unread text must never
/// supply); validation reports such text as lexically ambiguous.
pub(crate) fn strip_strings_and_comments_for(
    sql: &str,
    dialect: sqlly_lang::sqlmodel::Dialect,
) -> String {
    use sqlly_lang::seclex::{ConstructKind, editor_construct_at, editor_word_continue};
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut pos = 0;
    while pos < bytes.len() {
        let b = bytes[pos];
        if let Some(c) = editor_construct_at(bytes, pos, dialect) {
            if !c.terminated {
                // An unterminated literal or comment runs to the end: these
                // scans answer "is there a WHERE / a key pin", where seeing
                // text the engine never reads as SQL would vouch falsely.
                // (Validation separately flags the text as ambiguous.)
                out.push(' ');
                break;
            }
            match c.kind {
                ConstructKind::QuotedIdent { .. } => {
                    out.push('"');
                    for ch in sql[c.start + 1..c.end - 1].chars() {
                        out.push(if matches!(ch, '"' | '[' | ']' | '`' | '\'') {
                            '_'
                        } else {
                            ch
                        });
                    }
                    out.push('"');
                }
                ConstructKind::Variable => out.push_str(&sql[c.start..c.end]),
                _ => out.push(' '),
            }
            pos = c.end;
            continue;
        }
        if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 {
            let start = pos;
            pos += 1;
            while pos < bytes.len() && editor_word_continue(bytes[pos], dialect) {
                pos += 1;
            }
            while pos < bytes.len() && !sql.is_char_boundary(pos) {
                pos += 1;
            }
            out.push_str(&sql[start..pos]);
            continue;
        }
        out.push(b as char);
        pos += 1;
    }
    out
}

/// Whether `keyword` appears as a whole word at parenthesis depth 0 in `slice`.
///
/// Subquery contents live at depth > 0, so this distinguishes an *outer* clause
/// keyword (e.g. the `WHERE` of an `UPDATE`/`DELETE`) from one buried inside a
/// subquery. The caller must pass text that has already been
/// [`strip_strings_and_comments_for`]'d and lower-cased.
pub(crate) fn has_top_level_keyword(stripped_lower: &str, keyword: &str) -> bool {
    let bytes = stripped_lower.as_bytes();
    let mut depth: i32 = 0;
    // A quoted identifier (`"where"`, `[order]`) is a name, never a clause.
    let mut in_quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        if let Some(close) = in_quote {
            if bytes[i] == close {
                in_quote = None;
            }
            i += 1;
            continue;
        }
        match bytes[i] {
            b'"' => in_quote = Some(b'"'),
            b'[' => in_quote = Some(b']'),
            b'(' => depth += 1,
            b')' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ => {
                if depth == 0 && word_at(stripped_lower, i, keyword) {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Extract the text of every predicate region (`WHERE …` and `… ON …`) so
/// keyword scans there ignore SELECT-list / projection occurrences.
///
/// The `WHERE` region begins at a depth-0 `where` and runs until a depth-0
/// clause terminator (`group`/`order`/`having`/`union`/`except`/`intersect`/
/// `for`/`option`/`window`); subquery contents inside it (depth > 0) are kept,
/// since a tenant predicate in a correlated subquery still scopes the row set.
/// An `ON` region begins at a depth-0 `on` and runs until the next depth-0
/// join/clause keyword. Input must be [`strip_strings_and_comments_for`]'d and
/// lower-cased.
// Retained as a tested text utility; its former caller (the tenant-scoping
// check) has been removed, so nothing in the non-test build references it.
#[allow(dead_code)]
pub(crate) fn predicate_text(stripped_lower: &str) -> String {
    const WHERE_TERMINATORS: &[&str] = &[
        "group",
        "order",
        "having",
        "union",
        "except",
        "intersect",
        "for",
        "option",
        "window",
    ];
    const ON_TERMINATORS: &[&str] = &[
        "where",
        "group",
        "order",
        "having",
        "union",
        "except",
        "intersect",
        "join",
        "inner",
        "left",
        "right",
        "full",
        "cross",
        "outer",
    ];

    let bytes = stripped_lower.as_bytes();
    let mut depth: i32 = 0;
    let mut out = String::new();
    let mut region: Option<&[&str]> = None; // active region's terminator set
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => {
                depth += 1;
                if region.is_some() {
                    out.push('(');
                }
            }
            b')' => {
                if depth > 0 {
                    depth -= 1;
                }
                if region.is_some() {
                    out.push(')');
                }
            }
            _ => {
                if depth == 0 {
                    if let Some(terminators) = region
                        && terminators.iter().any(|t| word_at(stripped_lower, i, t))
                    {
                        region = None;
                        // Re-examine this position: an ON terminator may itself
                        // open a new region (e.g. a following WHERE).
                        continue;
                    }
                    if region.is_none() {
                        if word_at(stripped_lower, i, "where") {
                            region = Some(WHERE_TERMINATORS);
                        } else if word_at(stripped_lower, i, "on") {
                            region = Some(ON_TERMINATORS);
                        }
                    }
                }
                if region.is_some() {
                    out.push(bytes[i] as char);
                }
            }
        }
        i += 1;
    }
    out
}

/// Extract the text of the outer `WHERE` clause only (not `JOIN … ON`
/// predicates, and not an `UPDATE`'s `SET` list). Begins at the first depth-0
/// `where` and runs until a depth-0 clause terminator; subquery contents inside
/// it (depth > 0) are kept. Returns an empty string when there is no outer
/// `WHERE`. Input must be [`strip_strings_and_comments_for`]'d and lower-cased.
///
/// This is deliberately narrower than [`predicate_text`]: the primary-key
/// constraint check must never read an `UPDATE t SET id = 5 …` assignment or a
/// join condition as if it filtered the target rows.
pub(crate) fn where_region(stripped_lower: &str) -> String {
    const WHERE_TERMINATORS: &[&str] = &[
        "group",
        "order",
        "having",
        "union",
        "except",
        "intersect",
        "for",
        "option",
        "window",
    ];
    let bytes = stripped_lower.as_bytes();
    let mut depth: i32 = 0;
    let mut out = String::new();
    let mut in_region = false;
    // Bracketed (`[...]`) and double-quoted (`"..."`) identifiers can contain
    // reserved words and parentheses — a primary-key column literally named
    // `[order]` or `[group]` must not be read as a clause terminator, and
    // `[a(b)]` must not shift paren depth. Track them so keyword/paren logic is
    // suppressed inside a quoted identifier.
    let mut in_bracket = false;
    let mut in_dquote = false;
    let mut i = 0;
    while i < bytes.len() {
        let quoted = in_bracket || in_dquote;
        match bytes[i] {
            b'[' if !in_dquote => {
                in_bracket = true;
                if in_region {
                    out.push('[');
                }
            }
            b']' if in_bracket => {
                in_bracket = false;
                if in_region {
                    out.push(']');
                }
            }
            b'"' if !in_bracket => {
                in_dquote = !in_dquote;
                if in_region {
                    out.push('"');
                }
            }
            b'(' if !quoted => {
                depth += 1;
                if in_region {
                    out.push('(');
                }
            }
            b')' if !quoted => {
                if depth > 0 {
                    depth -= 1;
                }
                if in_region {
                    out.push(')');
                }
            }
            _ => {
                if depth == 0 && !quoted {
                    if in_region
                        && WHERE_TERMINATORS
                            .iter()
                            .any(|t| word_at(stripped_lower, i, t))
                    {
                        break;
                    }
                    if !in_region && word_at(stripped_lower, i, "where") {
                        in_region = true;
                        // Skip the "where" keyword itself so it is not part of
                        // the returned predicate text.
                        i += "where".len();
                        continue;
                    }
                }
                if in_region {
                    out.push(bytes[i] as char);
                }
            }
        }
        i += 1;
    }
    out
}

/// Strip a fully-balanced enclosing parenthesis pair (repeatedly): `(id = 1)` →
/// `id = 1`, `((a = 1))` → `a = 1`. Only a pair that wraps the ENTIRE trimmed
/// predicate is removed, so `(a = 1) and (b = 2)` is left intact.
fn unwrap_balanced_outer_parens(predicate: &str) -> &str {
    let mut p = predicate.trim();
    loop {
        let bytes = p.as_bytes();
        if bytes.first() != Some(&b'(') || bytes.last() != Some(&b')') {
            return p;
        }
        // The leading `(` must match the trailing `)` — i.e. depth returns to 0
        // only at the very end — for the pair to wrap everything.
        let mut depth: i32 = 0;
        let mut wraps_all = true;
        for (idx, &b) in bytes.iter().enumerate() {
            match b {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 && idx != bytes.len() - 1 {
                        wraps_all = false;
                        break;
                    }
                }
                _ => {}
            }
        }
        if wraps_all && depth == 0 {
            p = p[1..p.len() - 1].trim();
        } else {
            return p;
        }
    }
}

/// Remove identifier-quoting delimiters (`[`, `]`, `"`, `` ` ``) so a
/// bracket/quote-wrapped column reads as its bare name: `[order]` → `order`,
/// `[t].[id]` → `t.id`. Safe here because string/comment literals were already
/// stripped upstream, so the only remaining delimiters wrap identifiers.
fn remove_identifier_delimiters(predicate: &str) -> String {
    predicate
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | '"' | '`'))
        .collect()
}

/// The qualifier immediately before the column starting at `i` — the identifier
/// in `qualifier.col` — or `None` when the column is unqualified.
fn qualifier_before(s: &str, i: usize) -> Option<&str> {
    let bytes = s.as_bytes();
    if i == 0 || bytes[i - 1] != b'.' {
        return None;
    }
    let dot = i - 1;
    let mut j = dot;
    while j > 0 && (bytes[j - 1].is_ascii_alphanumeric() || bytes[j - 1] == b'_') {
        j -= 1;
    }
    Some(&s[j..dot])
}

/// Whether `col` appears in `predicate` (already stripped/lower-cased) as one
/// side of a top-level (`depth == 0`) equality — `col = …` or `… = col` — that
/// is not part of `<=`, `>=`, `!=`, or `<>`. Used to decide whether a `WHERE`
/// pins a primary-key column to a single value.
#[cfg(test)]
pub(crate) fn top_level_equality_on(predicate: &str, col: &str) -> bool {
    top_level_equality_on_qualified(predicate, col, None)
}

/// Qualifier-aware [`top_level_equality_on`]. When `accepted_quals` is `Some`, a
/// qualified reference `q.col` counts only if `q` is one of the accepted
/// qualifiers (the target table name or its alias) — so `WHERE u.id = 1` no
/// longer looks like it pins the target `t`'s `id`. `None` accepts any
/// qualifier (the plain single-table behavior). Outer balanced parentheses are
/// unwrapped (`(id = 1)`) and identifier delimiters removed (`[order]`) first so
/// neither hides a genuine equality.
pub(crate) fn top_level_equality_on_qualified(
    predicate: &str,
    col: &str,
    accepted_quals: Option<&[String]>,
) -> bool {
    let unwrapped = unwrap_balanced_outer_parens(predicate.trim());
    let cleaned = remove_identifier_delimiters(unwrapped);
    let col = col.trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'));
    let bytes = cleaned.as_bytes();
    let mut depth: i32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ => {
                if depth == 0 && word_at(&cleaned, i, col) {
                    // A qualified reference to a different table does not pin the
                    // target's column.
                    if let Some(quals) = accepted_quals
                        && let Some(q) = qualifier_before(&cleaned, i)
                        && !quals.iter().any(|acc| acc.eq_ignore_ascii_case(q))
                    {
                        i += 1;
                        continue;
                    }
                    let after = i + col.len();
                    // Right side: `col =` (skip spaces, next non-space is a lone
                    // '='). The operand on the far side of the `=` must be a
                    // single value (literal/parameter/function), not another
                    // column, or `id = other.id` would falsely read as a pin.
                    let eq = skip_spaces(bytes, after);
                    if is_eq_at(bytes, eq)
                        && operand_is_value(&cleaned[skip_spaces(bytes, eq + 1)..])
                    {
                        return true;
                    }
                    // Left side: `= col` (skip spaces before, previous is a lone '=').
                    if let Some(prev) = skip_spaces_back(bytes, i)
                        && is_eq_at(bytes, prev)
                        && operand_is_value_before(&cleaned, prev)
                    {
                        return true;
                    }
                }
            }
        }
        i += 1;
    }
    false
}

fn skip_spaces(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    i
}

/// Index of the last non-space byte strictly before `i`, or `None`.
fn skip_spaces_back(bytes: &[u8], i: usize) -> Option<usize> {
    let mut j = i;
    while j > 0 {
        j -= 1;
        if bytes[j] != b' ' {
            return Some(j);
        }
    }
    None
}

/// Whether `bytes[i]` is a standalone `=` (not part of `<=`, `>=`, `!=`, `<>`,
/// or `==`).
fn is_eq_at(bytes: &[u8], i: usize) -> bool {
    if i >= bytes.len() || bytes[i] != b'=' {
        return false;
    }
    let prev_bad = i > 0 && matches!(bytes[i - 1], b'<' | b'>' | b'!' | b'=');
    let next_bad = i + 1 < bytes.len() && bytes[i + 1] == b'=';
    !prev_bad && !next_bad
}

/// Whether an equality operand — the text on the side of `=` opposite the key
/// column, reading forward from its first non-space char — is a *single value*
/// rather than a bare column reference. Literals (numbers, and string literals
/// which upstream stripping has already blanked out), parameters (`@p`), and
/// function calls (`ident(`) are values; a plain column name (`id`, `t.id`) is
/// not. This is what separates `id = 1` / `id = @p` (pins one row) from
/// `id = id` / `id = other.id` (equates two columns, matches many).
fn operand_is_value(text: &str) -> bool {
    let t = text.trim_start();
    let Some(first) = t.chars().next() else {
        // Nothing here: a stripped string literal or the clause edge — a value.
        return true;
    };
    if first == '@' || first.is_ascii_digit() {
        return true;
    }
    if !(first.is_ascii_alphabetic() || first == '_') {
        // Operators/punctuation begin an expression, not a bare column.
        return true;
    }
    // An identifier: a following `(` marks a function call (value); otherwise it
    // is a column reference (not a value).
    let ident_end = t
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
        .unwrap_or(t.len());
    matches!(t[ident_end..].trim_start().chars().next(), Some('('))
}

/// [`operand_is_value`] for the operand ending immediately before byte `eq` (the
/// `=`), i.e. the left-hand side of `= col`. Reads the trailing token backward.
fn operand_is_value_before(cleaned: &str, eq: usize) -> bool {
    let bytes = cleaned.as_bytes();
    let Some(last) = skip_spaces_back(bytes, eq) else {
        return true; // Nothing before `=`: a stripped literal or the edge.
    };
    if bytes[last] == b')' {
        return true; // A function call / parenthesized expression.
    }
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'.';
    if !is_ident(bytes[last]) {
        return true;
    }
    let mut start = last;
    while start > 0 && is_ident(bytes[start - 1]) {
        start -= 1;
    }
    // A `@` immediately before the token makes it a parameter (value).
    if start > 0 && bytes[start - 1] == b'@' {
        return true;
    }
    operand_is_value(&cleaned[start..=last])
}

/// Whether `word` starts at byte offset `at` and is bounded as a whole word.
fn word_at(haystack: &str, at: usize, word: &str) -> bool {
    if !haystack[at..].starts_with(word) {
        return false;
    }
    let before_ok = at == 0
        || !haystack[..at]
            .chars()
            .next_back()
            .map(|c| c.is_alphanumeric() || c == '_')
            .unwrap_or(false);
    let after = at + word.len();
    let after_ok = after >= haystack.len()
        || !haystack[after..]
            .chars()
            .next()
            .map(|c| c.is_alphanumeric() || c == '_')
            .unwrap_or(false);
    before_ok && after_ok
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(sql: &str) -> String {
        strip_strings_and_comments_for(sql, sqlly_lang::sqlmodel::Dialect::SqlServer).to_lowercase()
    }

    #[test]
    fn nested_block_comments_strip_as_one_comment() {
        // T-SQL nests block comments; ending at the first */ would leave
        // "hidden */" visible to keyword scans.
        assert!(!contains_word(
            &norm("select 1 /* a /* hidden */ still comment */ from t"),
            "hidden"
        ));
        assert!(!contains_word(
            &norm("select 1 /* a /* hidden */ still comment */ from t"),
            "still"
        ));
        // Text after the comment truly ends is kept.
        assert!(contains_word(
            &norm("select 1 /* a /* b */ c */ where x = 1"),
            "where"
        ));
        // Unterminated comment swallows the rest of the text.
        assert!(!contains_word(
            &norm("select 1 /* open /* never closed"),
            "closed"
        ));
    }

    #[test]
    fn contains_word_is_whole_word() {
        assert!(contains_word("a where b", "where"));
        assert!(!contains_word("elsewhere", "where"));
        assert!(!contains_word("asset", "set"));
    }

    #[test]
    fn top_level_where_detected() {
        assert!(has_top_level_keyword(
            &norm("delete from t where id = 1"),
            "where"
        ));
    }

    #[test]
    fn subquery_where_is_not_top_level() {
        let sql = "update t set x = (select v from y where y.id = 1)";
        assert!(!has_top_level_keyword(&norm(sql), "where"));
    }

    #[test]
    fn outer_where_with_subquery_is_top_level() {
        let sql = "delete from t where id in (select id from y where y.f = 1)";
        assert!(has_top_level_keyword(&norm(sql), "where"));
    }

    #[test]
    fn predicate_text_excludes_select_list() {
        // Tenant column only appears in the projection -> not a predicate.
        let region = predicate_text(&norm("select tenantid from invoices"));
        assert!(!contains_word(&region, "tenantid"));
    }

    #[test]
    fn predicate_text_includes_where() {
        let region = predicate_text(&norm("select id from invoices where tenantid = 1"));
        assert!(contains_word(&region, "tenantid"));
    }

    #[test]
    fn predicate_text_includes_join_on() {
        let region = predicate_text(&norm(
            "select i.id from invoices i join t on t.tenantid = i.tenantid",
        ));
        assert!(contains_word(&region, "tenantid"));
    }

    #[test]
    fn where_region_excludes_update_set_list() {
        // The SET assignment must not leak into the WHERE region, or an
        // `UPDATE t SET id = 5 WHERE name = 'x'` would look PK-pinned.
        let region = where_region(&norm("update t set id = 5 where name = 'x'"));
        assert!(!top_level_equality_on(&region, "id"));
        assert!(top_level_equality_on(&region, "name"));
    }

    #[test]
    fn where_region_empty_without_where() {
        assert_eq!(where_region(&norm("delete from t")), "");
    }

    #[test]
    fn where_region_excludes_join_on() {
        // A join predicate is not the WHERE clause.
        let region = where_region(&norm(
            "delete t from t join s on t.id = s.id where s.flag = 1",
        ));
        assert!(!top_level_equality_on(&region, "t.id") && !top_level_equality_on(&region, "id"));
        assert!(top_level_equality_on(&region, "flag"));
    }

    #[test]
    fn top_level_equality_detects_both_orders() {
        assert!(top_level_equality_on(" id = 1 ", "id"));
        assert!(top_level_equality_on(" 1 = id ", "id"));
    }

    #[test]
    fn top_level_equality_rejects_comparison_operators() {
        assert!(!top_level_equality_on(" id >= 1 ", "id"));
        assert!(!top_level_equality_on(" id <= 1 ", "id"));
        assert!(!top_level_equality_on(" id <> 1 ", "id"));
        assert!(!top_level_equality_on(" id != 1 ", "id"));
    }

    #[test]
    fn top_level_equality_ignores_nested_and_qualified() {
        // Qualified column still matches (word boundary at '.').
        assert!(top_level_equality_on(" t.id = 1 ", "id"));
        // An equality buried in a subquery is not top-level.
        assert!(!top_level_equality_on(
            " x in (select id from y where id = 1) ",
            "id"
        ));
    }

    #[test]
    fn top_level_equality_unwraps_outer_parens() {
        // `WHERE (id = 1)` pins the key: the wrapping parens must be unwrapped so
        // the equality reads as top-level.
        assert!(top_level_equality_on("(id = 1)", "id"));
        assert!(top_level_equality_on("((id = 1))", "id"));
        // But a paren that does NOT wrap the whole predicate is left intact, so a
        // genuinely nested equality stays non-top-level.
        assert!(!top_level_equality_on("x = 2 and (id = 1)", "id"));
    }

    #[test]
    fn where_region_keeps_bracketed_keyword_column() {
        // A PK column literally named `[order]` must not truncate the WHERE
        // region at the `order` (ORDER BY) terminator, and the bracketed name
        // must still register as pinned.
        let region = where_region(&norm("delete from t where [order] = 1"));
        assert!(top_level_equality_on(&region, "order"));
        let region2 = where_region(&norm("update t set x = 1 where [group] = 5"));
        assert!(top_level_equality_on(&region2, "group"));
    }

    #[test]
    fn qualified_equality_respects_target() {
        let target = ["t".to_string()];
        // `t.id = 1` pins the target `t`.
        assert!(top_level_equality_on_qualified(
            "t.id = 1",
            "id",
            Some(&target)
        ));
        // A bare `id = 1` also pins.
        assert!(top_level_equality_on_qualified(
            "id = 1",
            "id",
            Some(&target)
        ));
        // But `u.id = 1` filters a DIFFERENT table and must NOT count as pinning
        // the target's `id`.
        assert!(!top_level_equality_on_qualified(
            "u.id = 1",
            "id",
            Some(&target)
        ));
        // Bracketed target qualifier still resolves after delimiter stripping.
        assert!(top_level_equality_on_qualified(
            "[t].[id] = 1",
            "id",
            Some(&target)
        ));
    }

    #[test]
    fn equality_to_a_column_is_not_a_pin() {
        // Column-to-column equality (either order) can match many rows.
        assert!(!top_level_equality_on("id = id", "id"));
        assert!(!top_level_equality_on("id = other.id", "id"));
        assert!(!top_level_equality_on("other.id = id", "id"));
        // Literal, parameter, and function-call operands DO pin.
        assert!(top_level_equality_on("id = 1", "id"));
        assert!(top_level_equality_on("id = @p", "id"));
        assert!(top_level_equality_on("@p = id", "id"));
        assert!(top_level_equality_on("id = newid()", "id"));
    }

    #[test]
    fn predicate_text_stops_at_group_by() {
        // tenantid after WHERE terminator (in GROUP BY) still counts because it is
        // inside the WHERE region only until 'group'; here it is in the WHERE.
        let region = predicate_text(&norm(
            "select id from invoices where tenantid = 1 group by id",
        ));
        assert!(contains_word(&region, "tenantid"));
        // A projection-only tenant ref before WHERE must be excluded.
        let region2 = predicate_text(&norm(
            "select tenantid from invoices where amount > 0 group by tenantid",
        ));
        // 'group by tenantid' is past the WHERE terminator, and the select-list
        // tenantid is before WHERE: neither is a filtering predicate.
        assert!(!contains_word(&region2, "tenantid"));
    }
}
