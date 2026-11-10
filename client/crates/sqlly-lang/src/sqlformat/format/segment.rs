//! Statement and clause segmentation: splitting the item stream into
//! [`Statement`]s, finding [`Clause`] boundaries inside a query block, and
//! the comma/CASE/alias scanning helpers the emitters share.

use super::casing::word_in;
use super::tree::{Item, Node, is_word, word_text};
use crate::sqllex::{NumberShape, TokenKind};
use crate::sqlmodel::Dialect;

pub(super) struct Statement<'a> {
    pub(super) items: Vec<Item<'a>>,
    pub(super) semicolon: Option<Item<'a>>,
    /// The T-SQL `GO` batch separator that ends this statement, if any.
    pub(super) separator: Option<BatchSeparator<'a>>,
    /// Source byte range covering the statement (incl. semicolon, excl. the
    /// batch separator), for verbatim passthrough of formatter-off regions.
    pub(super) src_start: usize,
    pub(super) src_end: usize,
}

/// A T-SQL batch separator line: `GO`, an optional repeat count (`GO 5`), and
/// any comments that share its line.
pub(super) struct BatchSeparator<'a> {
    pub(super) go: Item<'a>,
    pub(super) count: Option<Item<'a>>,
    pub(super) trailing_comments: Vec<Item<'a>>,
}

/// Split the item stream into statements at `;` and at batch-separator
/// lines: T-SQL's `GO` under the SQL Server dialect, and the `/` that ends
/// a PL/SQL block in an Oracle (SQL*Plus / SQLcl) script.
pub(super) fn split_statements_items<'a>(
    items: &[Item<'a>],
    dialect: Dialect,
) -> Vec<Statement<'a>> {
    let batch_separators = matches!(dialect, Dialect::SqlServer | Dialect::Oracle);
    let mut out: Vec<Statement<'a>> = Vec::new();
    let mut cur: Vec<Item<'a>> = Vec::new();
    let mut depth = 0usize;
    let mut i = 0usize;
    while i < items.len() {
        let item = &items[i];
        if item.tok.kind == TokenKind::Semicolon {
            let src_start = cur
                .first()
                .map(|i| i.tok.span.start)
                .unwrap_or(item.tok.span.start);
            out.push(Statement {
                items: std::mem::take(&mut cur),
                semicolon: Some(*item),
                separator: None,
                src_start,
                src_end: item.tok.span.end,
            });
            depth = 0;
            i += 1;
            continue;
        }
        if batch_separators
            && depth == 0
            && let Some(end) = go_separator_end(items, i, &cur, dialect)
        {
            let mut tail = items[i + 1..end].iter().copied().peekable();
            let count = tail.next_if(|t| matches!(t.tok.kind, TokenKind::Number(_)));
            let separator = BatchSeparator {
                go: *item,
                count,
                trailing_comments: tail.collect(),
            };
            match out.last_mut() {
                // `SELECT 1;` then `GO`: the separator closes that statement.
                Some(prev) if cur.is_empty() && prev.separator.is_none() => {
                    prev.separator = Some(separator);
                }
                _ => {
                    let src_start = cur
                        .first()
                        .map(|i| i.tok.span.start)
                        .unwrap_or(item.tok.span.start);
                    let src_end = cur
                        .last()
                        .map(|i| i.tok.span.end)
                        .unwrap_or(item.tok.span.start);
                    out.push(Statement {
                        items: std::mem::take(&mut cur),
                        semicolon: None,
                        separator: Some(separator),
                        src_start,
                        src_end,
                    });
                }
            }
            depth = 0;
            i = end;
            continue;
        }
        match item.tok.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth = depth.saturating_sub(1),
            _ => {}
        }
        cur.push(*item);
        i += 1;
    }
    if !cur.is_empty() {
        let src_start = cur.first().map(|i| i.tok.span.start).unwrap_or(0);
        let src_end = cur.last().map(|i| i.tok.span.end).unwrap_or(0);
        out.push(Statement {
            items: cur,
            semicolon: None,
            separator: None,
            src_start,
            src_end,
        });
    }
    out
}

/// Words after which the statement still needs an operand, so a `GO` line
/// following them is a column/alias named `go`, not a batch separator.
const OPERAND_EXPECTED_AFTER: &[&str] = &[
    "select", "distinct", "top", "from", "join", "where", "and", "or", "not", "by", "as", "set",
    "into", "having", "when", "then", "else", "in", "like", "between", "is", "case", "values",
    "update", "exec", "execute",
];

/// Words that can only continue a statement, never begin one — a `GO` line
/// right before them is part of the statement.
const CONTINUATION_WORDS: &[&str] = &[
    "from",
    "where",
    "as",
    "and",
    "or",
    "join",
    "inner",
    "left",
    "right",
    "full",
    "cross",
    "outer",
    "on",
    "group",
    "order",
    "having",
    "union",
    "intersect",
    "except",
    "then",
    "when",
    "else",
    "is",
    "in",
    "like",
    "between",
    "into",
];

/// When `items[i]` starts a T-SQL `GO` batch-separator line, the index just
/// past that line (the word `GO`, an optional integer repeat count, and any
/// same-line comments). `cur` holds the statement collected so far.
///
/// The line must hold nothing else, and the surrounding code must read as a
/// statement boundary: a `go` column on its own line inside a select list
/// (`a,\ngo\n, b` or `select\ngo\nfrom t`) is left alone. Quoted, string,
/// and comment `go`s never reach here — they are not words.
fn go_separator_end(
    items: &[Item<'_>],
    i: usize,
    cur: &[Item<'_>],
    dialect: Dialect,
) -> Option<usize> {
    let item = &items[i];
    let oracle = dialect == Dialect::Oracle;
    let is_separator = if oracle {
        item.tok.kind == TokenKind::Operator && item.tok.text == "/"
    } else {
        is_word(&item.tok, "go")
    };
    if !is_separator || (i > 0 && item.newlines_before == 0) {
        return None;
    }
    let mut end = i + 1;
    // `GO 5` repeats the batch; Oracle's `/` takes no count.
    if !oracle
        && let Some(n) = items.get(end)
        && n.newlines_before == 0
        && matches!(n.tok.kind, TokenKind::Number(NumberShape::Integer))
    {
        end += 1;
    }
    while let Some(c) = items.get(end)
        && c.newlines_before == 0
        && matches!(c.tok.kind, TokenKind::Comment(_))
    {
        end += 1;
    }
    if items.get(end).is_some_and(|n| n.newlines_before == 0) {
        return None;
    }

    let prev = cur
        .iter()
        .rev()
        .find(|t| !matches!(t.tok.kind, TokenKind::Comment(_)));
    if let Some(prev) = prev
        && (matches!(
            prev.tok.kind,
            TokenKind::Comma | TokenKind::Operator | TokenKind::Dot | TokenKind::LParen
        ) || word_text(&prev.tok).is_some_and(|w| word_in(OPERAND_EXPECTED_AFTER, w)))
    {
        return None;
    }
    let next = items[end..]
        .iter()
        .find(|t| !matches!(t.tok.kind, TokenKind::Comment(_)));
    if let Some(next) = next {
        let continues = match next.tok.kind {
            TokenKind::Comma | TokenKind::Dot | TokenKind::RParen => true,
            // `:setvar`/`:r` sqlcmd commands may follow a GO line.
            TokenKind::Operator => !next.tok.text.starts_with(':'),
            _ => word_text(&next.tok).is_some_and(|w| word_in(CONTINUATION_WORDS, w)),
        };
        if continues {
            return None;
        }
    }
    Some(end)
}

/// True when `items` contains a `GO` batch-separator line — a strong T-SQL
/// signal for dialect auto-detection.
pub(super) fn has_batch_separator(items: &[Item<'_>]) -> bool {
    split_statements_items(items, Dialect::SqlServer)
        .iter()
        .any(|s| s.separator.is_some())
}

/// `-- formatter:off` / `/* formatter: on */` (prefix configurable). Returns
/// `Some(false)` for off, `Some(true)` for on.
pub(super) fn control_comment_state(text: &str, prefix: &str) -> Option<bool> {
    let inner = if let Some(rest) = text.strip_prefix("--") {
        rest
    } else if let Some(rest) = text.strip_prefix('#') {
        rest
    } else if text.starts_with("/*") {
        text.trim_start_matches("/*").trim_end_matches("*/")
    } else {
        return None;
    };
    let lower = inner.trim().to_ascii_lowercase();
    let prefix = prefix.trim().to_ascii_lowercase();
    if prefix.is_empty() {
        return None;
    }
    let rest = lower.strip_prefix(&prefix)?;
    let rest = rest.trim_start_matches([':', ' ']).trim();
    match rest {
        "off" | "disable" => Some(false),
        "on" | "enable" => Some(true),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Clause segmentation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum ClauseKind {
    Select,
    From,
    Where,
    GroupBy,
    Having,
    OrderBy,
    LimitOffset,
    With,
    SetOp,
    UpdateSet,
    Values,
    MergeWhen,
    Using,
    On,
    Returning,
    Other,
}

pub(super) struct Clause<'a, 'n> {
    pub(super) kind: ClauseKind,
    pub(super) nodes: &'n [Node<'a>],
}

/// True when `nodes[i]` starts a new major clause at the top level of a
/// query block.
fn clause_start(
    nodes: &[Node<'_>],
    i: usize,
    in_update: bool,
    in_merge: bool,
) -> Option<ClauseKind> {
    let Node::Tok(item) = &nodes[i] else {
        return None;
    };
    let w = word_text(&item.tok)?;
    let next_word = |k: usize| -> Option<String> {
        nodes[i + 1..]
            .iter()
            .filter(|n| !n.is_comment())
            .nth(k)
            .and_then(|n| match n {
                Node::Tok(it) => word_text(&it.tok).map(|w| w.to_ascii_lowercase()),
                _ => None,
            })
    };
    let only_comments_before = nodes[..i].iter().all(|n| n.is_comment());
    let kind = match w.to_ascii_lowercase().as_str() {
        "select" => ClauseKind::Select,
        "from" => ClauseKind::From,
        "where" => ClauseKind::Where,
        "group" if next_word(0).as_deref() == Some("by") => ClauseKind::GroupBy,
        "having" => ClauseKind::Having,
        // Snowflake / BigQuery / Databricks `QUALIFY`: a condition on window
        // results, laid out like HAVING. Only when it reads as a clause
        // (see `mark_context_keywords`), never for a column named qualify.
        "qualify" if item.is_context_keyword => ClauseKind::Having,
        "order" if next_word(0).as_deref() == Some("by") => ClauseKind::OrderBy,
        "limit" | "offset" | "fetch" => ClauseKind::LimitOffset,
        "with" if only_comments_before => ClauseKind::With,
        "union" | "intersect" | "except" | "minus" => ClauseKind::SetOp,
        "set" if in_update => ClauseKind::UpdateSet,
        "values" => ClauseKind::Values,
        "when" if in_merge => ClauseKind::MergeWhen,
        "using" if in_merge => ClauseKind::Using,
        "on" if in_merge => ClauseKind::On,
        "returning" | "output" => ClauseKind::Returning,
        _ => return None,
    };
    Some(kind)
}

pub(super) fn segment_clauses<'a, 'n>(nodes: &'n [Node<'a>]) -> Vec<Clause<'a, 'n>> {
    let first_word = nodes.iter().find_map(|n| match n {
        Node::Tok(i) if !matches!(i.tok.kind, TokenKind::Comment(_)) => {
            word_text(&i.tok).map(|w| w.to_ascii_lowercase())
        }
        _ => None,
    });
    let in_update = first_word.as_deref() == Some("update");
    let in_merge = first_word.as_deref() == Some("merge");

    // CASE…END spans may contain WHEN/ELSE words that must not be treated as
    // clause starts (MERGE) — track case depth.
    let mut case_depth = 0usize;
    let mut boundaries: Vec<(usize, ClauseKind)> = Vec::new();
    for i in 0..nodes.len() {
        if nodes[i].is_word("case") {
            case_depth += 1;
            continue;
        }
        if nodes[i].is_word("end") && case_depth > 0 {
            case_depth -= 1;
            continue;
        }
        if case_depth > 0 {
            continue;
        }
        if let Some(kind) = clause_start(nodes, i, in_update, in_merge) {
            // A directly-preceding comment run belongs to this clause.
            let mut start = i;
            let floor = boundaries.last().map(|&(b, _)| b + 1).unwrap_or(0);
            while start > floor && nodes[start - 1].is_comment() {
                start -= 1;
            }
            if boundaries.last().map(|&(b, _)| b) == Some(start) {
                if let Some(last) = boundaries.last_mut() {
                    last.1 = kind;
                }
            } else {
                boundaries.push((start, kind));
            }
        }
    }
    if boundaries.is_empty() {
        boundaries.push((0, ClauseKind::Other));
    } else if boundaries[0].0 != 0 {
        boundaries.insert(0, (0, ClauseKind::Other));
    }

    let mut clauses = Vec::new();
    for (bi, &(start, kind)) in boundaries.iter().enumerate() {
        let end = boundaries
            .get(bi + 1)
            .map(|&(s, _)| s)
            .unwrap_or(nodes.len());
        if start < end {
            clauses.push(Clause {
                kind,
                nodes: &nodes[start..end],
            });
        }
    }
    clauses
}

/// Split top-level comma-separated elements. Returns (element, comma node).
pub(super) fn split_commas<'a, 'n>(
    nodes: &'n [Node<'a>],
) -> Vec<(&'n [Node<'a>], Option<&'n Node<'a>>)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, n) in nodes.iter().enumerate() {
        if n.is_kind(TokenKind::Comma) {
            out.push((&nodes[start..i], Some(n)));
            start = i + 1;
        }
    }
    if start < nodes.len() || out.is_empty() {
        out.push((&nodes[start..], None));
    }
    out
}

/// Index just past the END matching the CASE at `start`.
pub(super) fn find_case_end(nodes: &[Node], start: usize) -> usize {
    let mut depth = 0usize;
    for (i, n) in nodes.iter().enumerate().skip(start) {
        if n.is_word("case") {
            depth += 1;
        } else if n.is_word("end") {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return i + 1;
            }
        }
    }
    nodes.len()
}

/// Index of the token starting the alias tail of a select-list element:
/// the `AS`, or a bare trailing identifier directly after an expression.
pub(super) fn find_alias_pos(elem: &[Node]) -> Option<usize> {
    let mut end = elem.len();
    while end > 0 && elem[end - 1].is_comment() {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let mut case_depth = 0usize;
    for (i, n) in elem[..end].iter().enumerate() {
        if n.is_word("case") {
            case_depth += 1;
        } else if n.is_word("end") && case_depth > 0 {
            case_depth -= 1;
        } else if case_depth == 0 && n.is_word("as") {
            return Some(i);
        }
    }
    if end >= 2
        && let Node::Tok(last) = &elem[end - 1]
        && matches!(last.tok.kind, TokenKind::Ident(_))
        && !word_in(
            &["asc", "desc", "null", "not", "true", "false"],
            last.tok.text,
        )
    {
        if let Node::Tok(prev) = &elem[end - 2] {
            if !matches!(
                prev.tok.kind,
                TokenKind::Operator | TokenKind::Dot | TokenKind::Comma
            ) && !is_word(&prev.tok, "as")
            {
                return Some(end - 1);
            }
        } else {
            return Some(end - 1);
        }
    }
    None
}
