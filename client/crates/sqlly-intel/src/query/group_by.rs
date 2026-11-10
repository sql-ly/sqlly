//! Which select-list columns a grouped query is missing from its GROUP BY.
//!
//! A query that aggregates (`COUNT`, `SUM`, …) or has a GROUP BY may only
//! select columns it groups by; SQL Server rejects anything else with error
//! 8120 ("Column '…' is invalid in the select list because it is not contained
//! in either an aggregate function or the GROUP BY clause"), and PostgreSQL,
//! Oracle and MySQL's `ONLY_FULL_GROUP_BY` do the same. This module finds those
//! columns so the editor can underline them and offer a fix that writes the
//! GROUP BY, and so completion inside a GROUP BY list can put them first.
//!
//! The analysis is tokenizer-based and deliberately conservative: only plain
//! column references (`Category`, `s.Category`, `[dbo].[T].[Col]`) are ever
//! reported. Expressions, stars, literals, variables, subqueries and window
//! functions are left alone, and a column counts as grouped when any GROUP BY
//! key names it — by the same qualified name, by its bare name, or by the
//! select item's alias — so an ambiguous case is never flagged.
//!
//! Each `SELECT` is its own query block: subqueries, CTE bodies and the
//! branches of a `UNION` are analysed separately.

use crate::query::tokenizer::{Token, TokenKind, tokenize};

/// Aggregate functions: a call to one of these (without `OVER`) makes the
/// query grouped.
const AGGREGATES: &[&str] = &[
    "count",
    "count_big",
    "sum",
    "avg",
    "min",
    "max",
    "stdev",
    "stdevp",
    "var",
    "varp",
    "string_agg",
    "group_concat",
    "array_agg",
    "json_agg",
    "jsonb_agg",
    "checksum_agg",
    "approx_count_distinct",
    "listagg",
    "bool_and",
    "bool_or",
    "every",
    "bit_and",
    "bit_or",
];

/// Words that end a query block's select list and start its next clause.
const CLAUSE_WORDS: &[&str] = &[
    "from", "into", "where", "group", "having", "order", "window", "option", "for", "offset",
    "limit", "qualify",
];

/// Words that end a query block outright: set operators and the statements
/// that can follow a SELECT in a batch.
const BLOCK_END_WORDS: &[&str] = &[
    "union",
    "except",
    "intersect",
    "minus",
    "insert",
    "update",
    "delete",
    "merge",
    "create",
    "alter",
    "drop",
    "exec",
    "execute",
    "declare",
    "if",
    "else",
    "begin",
    "end",
    "return",
    "print",
    "while",
    "go",
    "truncate",
    "use",
    "raiserror",
    "throw",
    "commit",
    "rollback",
    "waitfor",
];

/// A select-list column the GROUP BY does not cover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UngroupedColumn {
    /// Byte span of the column reference in the SQL.
    pub start: usize,
    pub end: usize,
    /// The reference exactly as written (`s.Category`, `[Category]`).
    pub text: String,
}

/// The GROUP BY analysis of one query block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupByFinding {
    /// Byte offset of the block's `SELECT`.
    pub select_start: usize,
    /// The select-list columns missing from the GROUP BY, in select order.
    pub missing: Vec<UngroupedColumn>,
    /// The span of the GROUP BY item list (after `GROUP BY`), when there is
    /// one. Completion uses it to know the caret is in the list.
    pub group_by_list: Option<(usize, usize)>,
    /// The whole SQL with the missing columns added to the GROUP BY — appended
    /// to the existing list, or a new clause inserted after FROM/WHERE.
    pub fixed_sql: String,
    /// Where the fix's inserted text ends in `fixed_sql`, for the caret.
    pub fixed_cursor: usize,
}

/// Every query block in `sql` that selects columns its GROUP BY does not
/// cover. Blocks without a problem are not returned.
pub fn ungrouped_columns(sql: &str) -> Vec<GroupByFinding> {
    analyze(sql)
        .into_iter()
        .filter(|finding| !finding.missing.is_empty())
        .collect()
}

/// The select-list columns still missing from the GROUP BY list the caret is
/// in, or `None` when the caret is not inside a GROUP BY list. The key being
/// typed at the caret is not counted as grouping anything, so a partial
/// `GROUP BY Cat▏` still reports `Category`.
pub fn required_group_by_columns(sql: &str, cursor: usize) -> Option<Vec<UngroupedColumn>> {
    let cursor = cursor.min(sql.len());
    let prefix_start = sql[..cursor]
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '[' || c == ']'))
        .map_or(0, |i| i + 1);
    let prefix_start = if sql.is_char_boundary(prefix_start) {
        prefix_start
    } else {
        cursor
    };
    // Blank the partial key so it neither covers a column nor breaks parsing.
    let blanked = format!(
        "{}{}{}",
        &sql[..prefix_start],
        " ".repeat(cursor - prefix_start),
        &sql[cursor..]
    );
    analyze(&blanked).into_iter().find_map(|finding| {
        let (start, end) = finding.group_by_list?;
        (start <= cursor && cursor <= end.max(start)).then_some(finding.missing)
    })
}

/// A significant token (no whitespace or comments) with its paren depth.
struct Tok<'a> {
    token: Token<'a>,
    depth: i32,
}

impl Tok<'_> {
    fn word(&self) -> Option<String> {
        matches!(self.token.kind, TokenKind::Keyword | TokenKind::Identifier)
            .then(|| self.token.text.to_ascii_lowercase())
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.token.kind, TokenKind::Keyword | TokenKind::Identifier)
            && self.token.text.eq_ignore_ascii_case(word)
    }

    fn is_symbol(&self, symbol: &str) -> bool {
        self.token.kind == TokenKind::Symbol && self.token.text == symbol
    }
}

fn significant_tokens(sql: &str) -> Vec<Tok<'_>> {
    let mut depth = 0;
    let mut out = Vec::new();
    for token in tokenize(sql) {
        if matches!(token.kind, TokenKind::Whitespace | TokenKind::Comment) {
            continue;
        }
        if token.kind == TokenKind::Symbol && token.text == ")" {
            depth -= 1;
        }
        out.push(Tok { token, depth });
        if token.kind == TokenKind::Symbol && token.text == "(" {
            depth += 1;
        }
    }
    out
}

/// Token-index ranges of one query block's clauses.
struct Block {
    select: usize,
    /// First select-list token and one past the last.
    list: (usize, usize),
    /// `from` keyword index, when present.
    from: Option<usize>,
    /// `group` keyword index and the item-list token range after `by`.
    group: Option<(usize, (usize, usize))>,
    /// Index of the first token after FROM/WHERE (the GROUP BY insertion
    /// point when there is no GROUP BY yet): the next clause or block end.
    tail: usize,
}

fn analyze(sql: &str) -> Vec<GroupByFinding> {
    let toks = significant_tokens(sql);
    let mut findings = Vec::new();
    for (i, tok) in toks.iter().enumerate() {
        if tok.is_word("select")
            && let Some(block) = read_block(&toks, i)
            && let Some(finding) = analyze_block(sql, &toks, &block)
        {
            findings.push(finding);
        }
    }
    findings
}

/// The extent and clauses of the query block starting at `toks[select]`.
fn read_block(toks: &[Tok<'_>], select: usize) -> Option<Block> {
    let depth = toks[select].depth;
    // The block ends at a closing paren below its depth, a `;`, a set operator
    // or next statement at its depth, or the next SELECT at its depth.
    let mut end = toks.len();
    for (j, tok) in toks.iter().enumerate().skip(select + 1) {
        if tok.depth < depth || (tok.depth == depth && tok.is_symbol(";")) {
            end = j;
            break;
        }
        if tok.depth == depth
            && let Some(word) = tok.word()
            && (word == "select" || BLOCK_END_WORDS.contains(&word.as_str()))
        {
            end = j;
            break;
        }
    }

    // Skip the SELECT modifiers: DISTINCT / ALL / TOP n | TOP (n) [PERCENT]
    // [WITH TIES].
    let mut i = select + 1;
    while i < end {
        let tok = &toks[i];
        if tok.is_word("distinct") || tok.is_word("all") || tok.is_word("percent") {
            i += 1;
        } else if tok.is_word("top") {
            i += 1;
            if i < end && toks[i].is_symbol("(") {
                while i < end && !(toks[i].is_symbol(")") && toks[i].depth == depth) {
                    i += 1;
                }
                i += 1;
            } else {
                i += 1;
            }
        } else if tok.is_word("with") && toks.get(i + 1).is_some_and(|t| t.is_word("ties")) {
            i += 2;
        } else {
            break;
        }
    }
    let list_start = i;

    let clause_at = |j: usize| {
        toks[j].depth == depth
            && toks[j]
                .word()
                .is_some_and(|w| CLAUSE_WORDS.contains(&w.as_str()))
    };
    let list_end = (list_start..end).find(|&j| clause_at(j)).unwrap_or(end);

    let mut from = None;
    let mut group = None;
    let mut tail = end;
    let mut j = list_end;
    while j < end {
        if toks[j].depth != depth {
            j += 1;
            continue;
        }
        match toks[j].word().as_deref() {
            Some("from") => from = Some(j),
            Some("group") if toks.get(j + 1).is_some_and(|t| t.is_word("by")) => {
                let items_start = j + 2;
                let items_end = (items_start..end)
                    .find(|&k| {
                        toks[k].depth == depth
                            && toks[k].word().is_some_and(|w| {
                                matches!(
                                    w.as_str(),
                                    "having"
                                        | "order"
                                        | "window"
                                        | "option"
                                        | "for"
                                        | "offset"
                                        | "limit"
                                        | "qualify"
                                )
                            })
                    })
                    .unwrap_or(end);
                group = Some((j, (items_start, items_end)));
                if tail == end {
                    tail = j;
                }
                j = items_end;
                continue;
            }
            Some(
                "having" | "order" | "window" | "option" | "for" | "offset" | "limit" | "qualify",
            ) if tail == end => {
                tail = j;
            }
            _ => {}
        }
        j += 1;
    }

    Some(Block {
        select,
        list: (list_start, list_end),
        from,
        group,
        tail,
    })
}

/// One select-list item: its token range, whether it aggregates, its plain
/// column reference (when it is one), and its alias.
struct Item {
    aggregated: bool,
    windowed: bool,
    column: Option<(usize, usize)>,
    alias: Option<String>,
}

fn analyze_block(sql: &str, toks: &[Tok<'_>], block: &Block) -> Option<GroupByFinding> {
    // A block with no FROM has no columns to group.
    block.from?;
    let depth = toks[block.select].depth;
    let (list_start, list_end) = block.list;
    if list_start >= list_end {
        return None;
    }

    // Split the select list on top-level commas.
    let mut items = Vec::new();
    let mut item_start = list_start;
    for j in list_start..=list_end {
        if j == list_end || (toks[j].depth == depth && toks[j].is_symbol(",")) {
            if item_start < j {
                items.push(read_item(toks, item_start, j, depth));
            }
            item_start = j + 1;
        }
    }

    let has_aggregate = items.iter().any(|item| item.aggregated && !item.windowed);
    if !has_aggregate && block.group.is_none() {
        return None;
    }

    // Every column name the GROUP BY mentions, qualified and bare.
    let mut grouped: Vec<String> = Vec::new();
    if let Some((_, (start, end))) = block.group {
        let mut k = start;
        while k < end {
            if let Some((chain_end, parts)) = column_chain(toks, k, end) {
                grouped.push(parts.join("."));
                if let Some(last) = parts.last() {
                    grouped.push(last.clone());
                }
                k = chain_end;
            } else {
                k += 1;
            }
        }
    }

    let mut missing = Vec::new();
    for item in &items {
        if item.aggregated || item.windowed {
            continue;
        }
        let Some((first, last)) = item.column else {
            continue;
        };
        let Some((_, parts)) = column_chain(toks, first, last + 1) else {
            continue;
        };
        let qualified = parts.join(".");
        let bare = parts.last().cloned().unwrap_or_default();
        let covered = grouped.iter().any(|g| *g == qualified || *g == bare)
            || item
                .alias
                .as_ref()
                .is_some_and(|alias| grouped.iter().any(|g| g == alias));
        if covered {
            continue;
        }
        let start = toks[first].token.start;
        let end = toks[last].token.end;
        missing.push(UngroupedColumn {
            start,
            end,
            text: sql[start..end].to_string(),
        });
    }

    let group_by_list = block.group.map(|(group, (start, end))| {
        let list_start = toks.get(group + 1).map_or(sql.len(), |by| by.token.end);
        let list_end = if end > start {
            toks[end - 1].token.end
        } else {
            list_start
        };
        let next = toks.get(end).map_or(sql.len(), |t| t.token.start);
        // The list runs to the start of the next clause, so trailing
        // whitespace after the last key still counts as inside it.
        (list_start, list_end.max(next.min(sql.len())))
    });

    let (fixed_sql, fixed_cursor) = fix(sql, toks, block, &missing);
    Some(GroupByFinding {
        select_start: toks[block.select].token.start,
        missing,
        group_by_list,
        fixed_sql,
        fixed_cursor,
    })
}

fn read_item(toks: &[Tok<'_>], start: usize, end: usize, depth: i32) -> Item {
    let mut aggregated = false;
    let mut windowed = false;
    for j in start..end {
        if toks[j].depth == depth && toks[j].is_word("over") {
            windowed = true;
        }
        if let Some(word) = toks[j].word()
            && AGGREGATES.contains(&word.as_str())
            && toks.get(j + 1).is_some_and(|t| t.is_symbol("("))
        {
            aggregated = true;
        }
    }

    // `alias = expr` (T-SQL), `expr AS alias`, `expr alias`.
    let mut expr = (start, end);
    let mut alias = None;
    if end - start >= 3
        && toks[start + 1].is_symbol("=")
        && toks[start].token.kind == TokenKind::Identifier
    {
        alias = Some(normalize(toks[start].token.text));
        expr = (start + 2, end);
    } else if end - start >= 3 && toks[end - 2].is_word("as") && toks[end - 2].depth == depth {
        alias = Some(normalize(toks[end - 1].token.text));
        expr = (start, end - 2);
    } else if end - start >= 2
        && toks[end - 1].token.kind == TokenKind::Identifier
        && !toks[end - 2].is_symbol(".")
        && (toks[end - 2].token.kind == TokenKind::Identifier
            || toks[end - 2].is_symbol(")")
            || toks[end - 2].token.kind == TokenKind::Keyword)
    {
        alias = Some(normalize(toks[end - 1].token.text));
        expr = (start, end - 1);
    }

    // A plain column reference is the whole expression.
    let column = column_chain(toks, expr.0, expr.1)
        .filter(|(chain_end, _)| *chain_end == expr.1)
        .map(|_| (expr.0, expr.1 - 1));
    Item {
        aggregated,
        windowed,
        column,
        alias,
    }
}

/// A dotted identifier chain starting at `toks[start]` (`a`, `a.b`,
/// `[a].[b].[c]`), ending before `end`: the index after it and its
/// normalized parts. A star, a call, a variable or a keyword is not a column.
fn column_chain(toks: &[Tok<'_>], start: usize, end: usize) -> Option<(usize, Vec<String>)> {
    let mut parts = Vec::new();
    let mut k = start;
    loop {
        let tok = toks.get(k).filter(|_| k < end)?;
        // The tokenizer marks a few words as keywords that are also ordinary
        // column names (`Type`, `Key`); after a `.` any word is a name.
        let name_like = match tok.token.kind {
            TokenKind::Identifier => !tok.token.text.starts_with('@'),
            TokenKind::Keyword => {
                !parts.is_empty()
                    || tok.token.text.eq_ignore_ascii_case("type")
                    || tok.token.text.eq_ignore_ascii_case("key")
            }
            _ => false,
        };
        if !name_like {
            return None;
        }
        parts.push(normalize(tok.token.text));
        k += 1;
        if k < end && toks[k].is_symbol(".") {
            k += 1;
            continue;
        }
        break;
    }
    if k < end && toks[k].is_symbol("(") {
        return None;
    }
    Some((k, parts))
}

fn normalize(ident: &str) -> String {
    ident
        .trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'))
        .to_ascii_lowercase()
}

/// The SQL with the missing columns added to the block's GROUP BY, and the
/// offset just past the inserted text.
fn fix(sql: &str, toks: &[Tok<'_>], block: &Block, missing: &[UngroupedColumn]) -> (String, usize) {
    if missing.is_empty() {
        return (sql.to_string(), 0);
    }
    let names: Vec<&str> = missing.iter().map(|m| m.text.as_str()).collect();

    // An existing GROUP BY: append after its last key.
    if let Some((group, (start, end))) = block.group
        && end > start
    {
        let at = toks[end - 1].token.end;
        let multiline = sql[toks[group].token.start..at].contains('\n');
        let insert = if multiline {
            let indent = line_indent(sql, toks[end - 1].token.start);
            names
                .iter()
                .map(|n| format!(",\n{indent}{n}"))
                .collect::<String>()
        } else {
            names.iter().map(|n| format!(", {n}")).collect::<String>()
        };
        return splice(sql, at, &insert);
    }

    // No GROUP BY (or an empty one): write the clause after FROM/WHERE, in
    // the layout and keyword case the FROM keyword uses.
    let from = block.from.expect("analysed blocks have a FROM");
    let keyword = if toks[from]
        .token
        .text
        .chars()
        .any(|c| c.is_ascii_uppercase())
    {
        "GROUP BY"
    } else {
        "group by"
    };
    let from_line_indent = line_indent(sql, toks[from].token.start);
    let from_on_own_line = toks
        .get(from + 1)
        .is_some_and(|next| sql[toks[from].token.end..next.token.start].contains('\n'));

    if let Some((group, _)) = block.group {
        // `GROUP BY` with no keys yet: fill them in after `BY`.
        let at = toks[group + 1].token.end;
        let insert = if from_on_own_line {
            let unit = indent_unit(sql, &from_line_indent);
            format!(
                "\n{}",
                names
                    .iter()
                    .map(|n| format!("{from_line_indent}{unit}{n}"))
                    .collect::<Vec<_>>()
                    .join(",\n")
            )
        } else {
            format!(" {}", names.join(", "))
        };
        return splice(sql, at, &insert);
    }

    let at = toks[block.tail - 1].token.end;
    let insert = if from_on_own_line {
        let unit = indent_unit(sql, &from_line_indent);
        format!(
            "\n{from_line_indent}{keyword}\n{}",
            names
                .iter()
                .map(|n| format!("{from_line_indent}{unit}{n}"))
                .collect::<Vec<_>>()
                .join(",\n")
        )
    } else {
        format!(" {keyword} {}", names.join(", "))
    };
    splice(sql, at, &insert)
}

fn splice(sql: &str, at: usize, insert: &str) -> (String, usize) {
    (
        format!("{}{insert}{}", &sql[..at], &sql[at..]),
        at + insert.len(),
    )
}

/// Leading whitespace of the line holding byte `at`.
fn line_indent(sql: &str, at: usize) -> String {
    let line_start = sql[..at].rfind('\n').map_or(0, |i| i + 1);
    let line = &sql[line_start..];
    line[..line.len() - line.trim_start_matches([' ', '\t']).len()].to_string()
}

/// One indent step: the first line indented deeper than `base`, minus `base`;
/// a tab when the document uses tabs; four spaces otherwise.
fn indent_unit(sql: &str, base: &str) -> String {
    for line in sql.lines() {
        let lead = &line[..line.len() - line.trim_start_matches([' ', '\t']).len()];
        if line.trim().is_empty() || lead.len() <= base.len() || !lead.starts_with(base) {
            continue;
        }
        return lead[base.len()..].to_string();
    }
    if sql.contains("\n\t") {
        "\t".to_string()
    } else {
        "    ".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn missing(sql: &str) -> Vec<String> {
        ungrouped_columns(sql)
            .into_iter()
            .flat_map(|f| f.missing.into_iter().map(|m| m.text))
            .collect()
    }

    fn fixed(sql: &str) -> String {
        let findings = ungrouped_columns(sql);
        assert_eq!(findings.len(), 1, "expected one finding for {sql:?}");
        findings[0].fixed_sql.clone()
    }

    const REPORTED: &str = "SELECT\n    Category,\n    COUNT(1)\nfrom\n    Shared.Settings\n";

    #[test]
    fn the_reported_query_is_missing_category() {
        assert_eq!(missing(REPORTED), vec!["Category"]);
        let finding = &ungrouped_columns(REPORTED)[0];
        assert_eq!(
            &REPORTED[finding.missing[0].start..finding.missing[0].end],
            "Category"
        );
    }

    #[test]
    fn the_fix_writes_group_by_in_the_querys_layout_and_case() {
        assert_eq!(
            fixed(REPORTED),
            "SELECT\n    Category,\n    COUNT(1)\nfrom\n    Shared.Settings\ngroup by\n    Category\n"
        );
        assert_eq!(
            fixed("SELECT Category, COUNT(*) FROM Shared.Settings WHERE Id > 1 ORDER BY 2"),
            "SELECT Category, COUNT(*) FROM Shared.Settings WHERE Id > 1 GROUP BY Category ORDER BY 2"
        );
        assert_eq!(
            fixed("select s.Category, s.Name, count(1) from Shared.Settings s;"),
            "select s.Category, s.Name, count(1) from Shared.Settings s group by s.Category, s.Name;"
        );
        let fixed_sql = fixed(REPORTED);
        assert!(
            missing(&fixed_sql).is_empty(),
            "the fix resolves the finding"
        );
    }

    #[test]
    fn the_fix_appends_to_an_existing_group_by() {
        assert_eq!(
            fixed("SELECT a, b, SUM(c) FROM t GROUP BY a HAVING SUM(c) > 1"),
            "SELECT a, b, SUM(c) FROM t GROUP BY a, b HAVING SUM(c) > 1"
        );
        assert_eq!(
            fixed("SELECT\n\ta,\n\tb,\n\tSUM(c)\nFROM t\nGROUP BY\n\ta\n"),
            "SELECT\n\ta,\n\tb,\n\tSUM(c)\nFROM t\nGROUP BY\n\ta,\n\tb\n"
        );
    }

    #[test]
    fn grouped_and_aggregated_items_are_fine() {
        assert!(missing("SELECT Category, COUNT(1) FROM t GROUP BY Category").is_empty());
        assert!(missing("SELECT s.Category, COUNT(1) FROM t s GROUP BY Category").is_empty());
        assert!(missing("SELECT Category, COUNT(1) FROM t s GROUP BY s.Category").is_empty());
        assert!(missing("SELECT [Category], COUNT(1) FROM t GROUP BY Category").is_empty());
        assert!(missing("SELECT COUNT(1), MAX(Name) FROM t").is_empty());
        assert!(missing("SELECT Category AS C, COUNT(1) FROM t GROUP BY C").is_empty());
        assert!(
            missing("SELECT a, b FROM t").is_empty(),
            "no aggregate, no GROUP BY"
        );
    }

    #[test]
    fn keyword_shaped_column_names_are_columns() {
        assert_eq!(missing("SELECT Type, COUNT(1) FROM t"), vec!["Type"]);
        assert_eq!(missing("SELECT t.Key, COUNT(1) FROM t"), vec!["t.Key"]);
        assert!(missing("SELECT Type, COUNT(1) FROM t GROUP BY Type").is_empty());
    }

    #[test]
    fn a_group_by_without_aggregates_still_needs_every_column() {
        assert_eq!(missing("SELECT a, b FROM t GROUP BY a"), vec!["b"]);
    }

    #[test]
    fn expressions_stars_literals_and_windows_are_left_alone() {
        assert!(missing("SELECT UPPER(Name), COUNT(1) FROM t").is_empty());
        assert!(missing("SELECT *, COUNT(1) FROM t").is_empty());
        assert!(missing("SELECT 'x', @v, NULL, COUNT(1) FROM t").is_empty());
        assert!(missing("SELECT Name, COUNT(1) OVER (PARTITION BY Name) FROM t").is_empty());
        assert!(missing("SELECT (SELECT MAX(x) FROM u), COUNT(1) FROM t").is_empty());
    }

    #[test]
    fn aliases_are_not_columns() {
        assert_eq!(
            missing("SELECT Category AS Cat, COUNT(1) AS n FROM t"),
            vec!["Category"]
        );
        assert_eq!(
            missing("SELECT Cat = Category, COUNT(1) FROM t"),
            vec!["Category"]
        );
        assert_eq!(
            missing("SELECT Category Cat, COUNT(1) n FROM t"),
            vec!["Category"]
        );
    }

    #[test]
    fn subqueries_and_union_branches_are_separate_blocks() {
        assert_eq!(
            missing("SELECT x FROM (SELECT a, COUNT(1) AS x FROM t) d"),
            vec!["a"]
        );
        assert!(missing("SELECT a FROM t UNION SELECT COUNT(1) FROM u").is_empty());
        assert_eq!(
            missing("SELECT a FROM t; SELECT b, COUNT(1) FROM u"),
            vec!["b"]
        );
    }

    #[test]
    fn required_columns_at_a_group_by_caret() {
        let sql = "SELECT\n\tCategory,\n\tCOUNT(1)\nfrom\n\tShared.Settings\ngroup by\n\t";
        let required = required_group_by_columns(sql, sql.len()).expect("in the GROUP BY list");
        assert_eq!(
            required.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            vec!["Category"]
        );
        // A partial key does not count as grouping the column yet.
        let partial = "SELECT Category, COUNT(1) FROM t GROUP BY Cat";
        let required = required_group_by_columns(partial, partial.len()).unwrap();
        assert_eq!(required.len(), 1);
        // Once grouped, nothing is required; outside the list, no answer.
        let done = "SELECT Category, COUNT(1) FROM t GROUP BY Category, ";
        assert_eq!(required_group_by_columns(done, done.len()), Some(vec![]));
        let outside = "SELECT Category, COUNT(1) FROM t WHERE ";
        assert_eq!(required_group_by_columns(outside, outside.len()), None);
    }
}
