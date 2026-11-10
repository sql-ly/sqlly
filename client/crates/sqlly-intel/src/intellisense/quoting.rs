//! Identifier quoting for completion inserts.
//!
//! Completion items carry catalog names verbatim in `label` (what the popup
//! shows and what ranking matches against). What gets *inserted* must be valid
//! SQL for the connection's engine, so a name that needs quoting — spaces,
//! dashes, a leading digit, a reserved word, or a case the engine would fold
//! away — is quoted part by part in the engine's style (`dbo.[Order Details]`,
//! `` `order` ``, `"OrderLines"`), and ordinary names are left alone. The
//! per-engine decision lives on [`DatabaseType::identifier_needs_quoting`].
//!
//! The second half handles a path the user is typing with quotes of their own
//! (`[dbo].[Ord▏`, `"Order Details".▏`). The completion scanner only reads
//! plain identifier characters, so [`QuotedPath`] recognizes the quoted path
//! before the caret, lets the completer run on a plain spelling of it, and then
//! completes *inside* the user's quote — closing it — instead of adding a
//! second pair.

use std::borrow::Cow;

use sqlly_lang::sqlmodel::{DatabaseType, Dialect, IdentifierQuoting};

use crate::intellisense::clause_insert::{REQUIRED_GROUP_BY_DETAIL, clause_keyword_for_detail};
use crate::intellisense::types::{CompletionItem, CompletionKind, CompletionSource};

/// `path` (dot-separated parts) with each part quoted only when the engine
/// needs it. Borrows `path` unchanged when no part needs quoting.
///
/// Parts are split on `.`, so a name that itself contains a dot cannot be
/// told apart from a qualifier here; such names are left as they are.
pub fn quote_path_if_needed(db: DatabaseType, path: &str) -> Cow<'_, str> {
    if !path
        .split('.')
        .any(|part| db.identifier_needs_quoting(part))
    {
        return Cow::Borrowed(path);
    }
    let mut out = String::with_capacity(path.len() + 8);
    for (index, part) in path.split('.').enumerate() {
        if index > 0 {
            out.push('.');
        }
        out.push_str(&db.quote_identifier_if_needed(part));
    }
    Cow::Owned(out)
}

/// Whether an item's insert text is an identifier path this module may
/// quote: catalog objects and columns (not keywords, builtins, snippets,
/// parameters, values, or FK joins, which build their own text), not already
/// quoted, and not a select-list expression echoed back for GROUP BY.
fn is_quotable_item(item: &CompletionItem) -> bool {
    let kind_ok = match item.kind {
        CompletionKind::Table
        | CompletionKind::Column
        | CompletionKind::Schema
        | CompletionKind::Database
        | CompletionKind::ResultSetColumn => true,
        CompletionKind::Function | CompletionKind::Procedure | CompletionKind::Type => !matches!(
            item.source,
            CompletionSource::Builtin | CompletionSource::Keyword | CompletionSource::Snippet
        ),
        _ => false,
    };
    kind_ok
        && item.detail != REQUIRED_GROUP_BY_DETAIL
        && !item.insert_text.contains(['(', ')', '\''])
        && !item.insert_text.split('.').any(is_already_quoted)
}

/// Whether one path part is already wrapped in a quote pair (`[x]`, `"x"`,
/// `` `x` ``) — text echoed back from the buffer rather than a catalog name.
fn is_already_quoted(part: &str) -> bool {
    part.len() >= 2
        && matches!(
            (part.as_bytes()[0], part.as_bytes()[part.len() - 1]),
            (b'[', b']') | (b'"', b'"') | (b'`', b'`')
        )
}

/// Quote the insert text of every identifier item that needs it for `db`.
/// Labels are untouched, so ranking, dedup, and accept history keep keying off
/// the plain catalog name. Items whose insert writes a clause (`FROM⏎\tname`)
/// get only their trailing name quoted.
pub fn apply_identifier_quoting(items: &mut [CompletionItem], db: DatabaseType) {
    for item in items.iter_mut() {
        if !is_quotable_item(item) {
            continue;
        }
        if clause_keyword_for_detail(&item.detail).is_some() {
            let Some(newline) = item.insert_text.rfind('\n') else {
                continue;
            };
            let line = &item.insert_text[newline + 1..];
            let split = newline + 1 + (line.len() - line.trim_start_matches([' ', '\t']).len());
            if let Cow::Owned(quoted) = quote_path_if_needed(db, &item.insert_text[split..]) {
                item.insert_text = format!("{}{quoted}", &item.insert_text[..split]);
            }
            continue;
        }
        if item.insert_text.contains('\n') {
            continue;
        }
        if let Cow::Owned(quoted) = quote_path_if_needed(db, &item.insert_text) {
            item.insert_text = quoted;
        }
    }
}

/// A dotted identifier path immediately before the caret that the user typed
/// with at least one quoted part: a closed quoted qualifier
/// (`[Order Details].Qu▏`) and/or an opening quote on the part being typed
/// (`[dbo].[Ord▏`, `"ord▏`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotedPath {
    /// Byte offset where the whole path (first qualifier) starts.
    pub path_start: usize,
    /// Byte offset where the part being typed starts — at its opening quote
    /// when it has one. A completion replaces from here to the caret.
    pub leaf_start: usize,
    /// The qualifier parts before the part being typed, unquoted.
    pub qualifier: Vec<String>,
    /// The typed part so far, unquoted and unescaped.
    pub partial: String,
    /// The quote pair the user opened on the part being typed, if any.
    pub open: Option<(char, char)>,
    /// The matching closing quote already follows the caret — right after
    /// it (for example auto-paired), or after the rest of a name being edited
    /// in place — so a completion replaces through it.
    pub closer_follows: bool,
    /// Bytes of name between the caret and that closing quote (`0` when the
    /// closer sits right after the caret).
    pub rest_len: usize,
}

impl QuotedPath {
    /// The SQL with this path respelled as plain identifiers (quotes removed,
    /// characters the completion scanner cannot read dropped) and the caret
    /// position within it. The completer runs on this; its results are then
    /// mapped back with [`Self::finish_items`].
    pub fn plain_sql(&self, sql: &str, cursor: usize) -> (String, usize) {
        let mut out = String::with_capacity(sql.len());
        out.push_str(&sql[..self.path_start]);
        for part in &self.qualifier {
            out.extend(part.chars().filter(|c| is_scanner_char(*c)));
            out.push('.');
        }
        out.extend(self.partial.chars().filter(|c| is_scanner_char(*c)));
        let plain_cursor = out.len();
        out.push_str(&sql[self.replace_end(cursor)..]);
        (out, plain_cursor)
    }

    /// Byte offset one past what a completion replaces: the caret, or past
    /// the closing quote that already follows it.
    pub fn replace_end(&self, cursor: usize) -> usize {
        match (self.open, self.closer_follows) {
            (Some((_, close)), true) => cursor + self.rest_len + close.len_utf8(),
            _ => cursor,
        }
    }

    /// Turn items completed on [`Self::plain_sql`] into items for the real
    /// buffer. With an open quote only identifier items survive (keywords
    /// and snippets make no sense inside a quoted name); each is written in
    /// the user's own quote style, every part quoted and closed, replacing
    /// from the opening quote. Without one, identifier parts are quoted as
    /// `db` requires.
    pub fn finish_items(
        &self,
        items: Vec<CompletionItem>,
        cursor: usize,
        db: Option<DatabaseType>,
    ) -> Vec<CompletionItem> {
        let span = Some((self.leaf_start, self.replace_end(cursor)));
        match self.open {
            Some((open, close)) => items
                .into_iter()
                .filter(|item| {
                    is_quotable_item(item) && clause_keyword_for_detail(&item.detail).is_none()
                })
                .map(|mut item| {
                    item.insert_text = quote_all_parts(&item.insert_text, open, close);
                    item.replace_span = span;
                    item
                })
                .collect(),
            None => {
                let mut items: Vec<CompletionItem> = items
                    .into_iter()
                    .filter(|item| clause_keyword_for_detail(&item.detail).is_none())
                    .map(|mut item| {
                        item.replace_span = None;
                        item
                    })
                    .collect();
                if let Some(db) = db {
                    apply_identifier_quoting(&mut items, db);
                }
                items
            }
        }
    }
}

/// Every dot-separated part of `path` wrapped in `open`/`close`, with
/// embedded closers doubled.
fn quote_all_parts(path: &str, open: char, close: char) -> String {
    let doubled: String = [close, close].iter().collect();
    let mut out = String::with_capacity(path.len() + 8);
    for (index, part) in path.split('.').enumerate() {
        if index > 0 {
            out.push('.');
        }
        out.push(open);
        out.push_str(&part.replace(close, &doubled));
        out.push(close);
    }
    out
}

/// Characters the completion prefix scanner accepts inside a name.
fn is_scanner_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The identifier-opening quote characters an engine accepts. With no engine
/// known, all three are recognized.
fn opening_quotes(db: Option<DatabaseType>) -> &'static [u8] {
    match db {
        None => b"[\"`",
        Some(db) => match db.identifier_quoting() {
            // SQL Server also reads "x" as an identifier (QUOTED_IDENTIFIER).
            IdentifierQuoting::Brackets => b"[\"",
            // MySQL-family `"` is a string delimiter by default.
            IdentifierQuoting::Backticks => b"`",
            IdentifierQuoting::DoubleQuotes if db.family() == Dialect::Sqlite => b"\"[`",
            IdentifierQuoting::DoubleQuotes => b"\"",
        },
    }
}

fn closing_quote(open: u8) -> u8 {
    if open == b'[' { b']' } else { open }
}

/// One lexical piece of the caret's line, as far as the path scan cares.
#[derive(Debug)]
enum Piece {
    Plain {
        start: usize,
        end: usize,
    },
    Quoted {
        start: usize,
        end: usize,
        inner: String,
        closed: bool,
        open: u8,
    },
    Dot {
        at: usize,
    },
    Other,
}

/// Recognize a quoted identifier path ending at `cursor`, or `None` when the
/// path before the caret is plain (the ordinary completion path handles it),
/// or the caret is inside a string literal or comment.
///
/// Only the caret's line is scanned, and only when it contains a quote
/// character at all, so ordinary typing pays a single short byte scan.
pub fn quoted_path_at(sql: &str, cursor: usize, db: Option<DatabaseType>) -> Option<QuotedPath> {
    let cursor = cursor.min(sql.len());
    if !sql.is_char_boundary(cursor) {
        return None;
    }
    let line_start = sql[..cursor].rfind('\n').map_or(0, |i| i + 1);
    let openers = opening_quotes(db);
    let line = &sql.as_bytes()[line_start..cursor];
    if !line.iter().any(|b| openers.contains(b)) {
        return None;
    }
    let pieces = scan_line(sql, line_start, cursor, openers)?;

    let mut index = pieces.len();
    let (leaf_start, partial, open) = match pieces.last()? {
        Piece::Quoted {
            start,
            end,
            inner,
            closed: false,
            open,
        } if *end == cursor => {
            index -= 1;
            (*start, inner.clone(), Some(*open))
        }
        Piece::Plain { start, end } if *end == cursor => {
            index -= 1;
            (*start, sql[*start..*end].to_string(), None)
        }
        Piece::Dot { at } if at + 1 == cursor => (cursor, String::new(), None),
        _ => return None,
    };
    if partial.starts_with('@') {
        return None;
    }

    let mut qualifier = Vec::new();
    let mut path_start = leaf_start;
    let mut any_quoted = open.is_some();
    while let Some(Piece::Dot { at }) = index.checked_sub(1).map(|i| &pieces[i]) {
        if at + 1 != path_start {
            break;
        }
        let dot = *at;
        let before = index.checked_sub(2).map(|i| &pieces[i])?;
        match before {
            Piece::Plain { start, end } if *end == dot => {
                let text = &sql[*start..*end];
                if text.starts_with('@') {
                    return None;
                }
                qualifier.push(text.to_string());
                path_start = *start;
            }
            Piece::Quoted {
                start,
                end,
                inner,
                closed: true,
                ..
            } if *end == dot => {
                qualifier.push(inner.clone());
                path_start = *start;
                any_quoted = true;
            }
            _ => return None,
        }
        index -= 2;
    }
    if !any_quoted
        || qualifier
            .iter()
            .any(|part| !part.chars().any(is_scanner_char))
    {
        return None;
    }
    qualifier.reverse();

    let open = open.map(|open| (open as char, closing_quote(open) as char));
    // Only name characters may sit between the caret and the closer, so a
    // half-typed quote never swallows text up to some later quote.
    let rest_len = sql[cursor..]
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_')
        .count();
    let closer_follows = open.is_some_and(|(_, close)| sql[cursor + rest_len..].starts_with(close));
    Some(QuotedPath {
        path_start,
        leaf_start,
        qualifier,
        partial,
        open,
        closer_follows,
        rest_len: if closer_follows { rest_len } else { 0 },
    })
}

/// Lex `sql[line_start..cursor]` into [`Piece`]s. `None` when the caret sits
/// inside a string literal or comment.
fn scan_line(sql: &str, line_start: usize, cursor: usize, openers: &[u8]) -> Option<Vec<Piece>> {
    let bytes = sql.as_bytes();
    let mut pieces = Vec::new();
    let mut i = line_start;
    while i < cursor {
        let b = bytes[i];
        if b == b'\'' || (b == b'"' && !openers.contains(&b'"')) {
            i += 1;
            loop {
                if i >= cursor {
                    return None;
                }
                if bytes[i] == b {
                    if i + 1 < cursor && bytes[i + 1] == b {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            pieces.push(Piece::Other);
            continue;
        }
        if b == b'-' && i + 1 < cursor && bytes[i + 1] == b'-' {
            return None;
        }
        if b == b'/' && i + 1 < cursor && bytes[i + 1] == b'*' {
            let close = sql[i + 2..cursor].find("*/")?;
            i += 2 + close + 2;
            pieces.push(Piece::Other);
            continue;
        }
        if openers.contains(&b) {
            let close = closing_quote(b);
            let start = i;
            i += 1;
            let mut inner = Vec::new();
            let mut closed = false;
            while i < cursor {
                if bytes[i] == close {
                    if i + 1 < cursor && bytes[i + 1] == close {
                        inner.push(close);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    closed = true;
                    break;
                }
                inner.push(bytes[i]);
                i += 1;
            }
            pieces.push(Piece::Quoted {
                start,
                end: i,
                inner: String::from_utf8_lossy(&inner).into_owned(),
                closed,
                open: b,
            });
            continue;
        }
        if b == b'.' {
            pieces.push(Piece::Dot { at: i });
            i += 1;
            continue;
        }
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'@' | b'$' | b'#') || b >= 0x80 {
            let start = i;
            while i < cursor
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'@' | b'$' | b'#')
                    || bytes[i] >= 0x80)
            {
                i += 1;
            }
            pieces.push(Piece::Plain { start, end: i });
            continue;
        }
        pieces.push(Piece::Other);
        i += 1;
    }
    Some(pieces)
}

#[cfg(test)]
mod tests {
    use super::*;
    use DatabaseType::*;

    fn item(label: &str, insert: &str, kind: CompletionKind) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            insert_text: insert.to_string(),
            kind,
            source: CompletionSource::LiveMetadata,
            score: 0,
            detail: String::new(),
            replace_span: None,
        }
    }

    fn quoted(db: DatabaseType, insert: &str, kind: CompletionKind) -> String {
        let mut items = vec![item(insert, insert, kind)];
        apply_identifier_quoting(&mut items, db);
        items.remove(0).insert_text
    }

    #[test]
    fn quotes_only_the_parts_that_need_it_per_engine() {
        let t = CompletionKind::Table;
        assert_eq!(quoted(SqlServer, "dbo.Orders", t), "dbo.Orders");
        assert_eq!(
            quoted(SqlServer, "dbo.Order Details", t),
            "dbo.[Order Details]"
        );
        assert_eq!(quoted(AzureSqlDatabase, "Sales.Order", t), "Sales.[Order]");
        assert_eq!(quoted(SqlServer, "my-schema.t", t), "[my-schema].t");
        assert_eq!(quoted(MySql, "shop.order", t), "shop.`order`");
        assert_eq!(quoted(MariaDb, "shop.2024 sales", t), "shop.`2024 sales`");
        assert_eq!(
            quoted(ClickHouse, "default.hits-v1", t),
            "default.`hits-v1`"
        );
        assert_eq!(
            quoted(Postgres, "public.OrderLines", t),
            "public.\"OrderLines\""
        );
        assert_eq!(
            quoted(Postgres, "public.order_lines", t),
            "public.order_lines"
        );
        assert_eq!(quoted(Postgres, "Sales.orders", t), "\"Sales\".orders");
        assert_eq!(quoted(Oracle, "HR.Employees", t), "HR.\"Employees\"");
        assert_eq!(quoted(Oracle, "HR.EMPLOYEES", t), "HR.EMPLOYEES");
        assert_eq!(quoted(Sqlite, "main.group", t), "main.\"group\"");
        assert_eq!(quoted(LibSql, "first name", t), "\"first name\"");
        assert_eq!(quoted(DuckDb, "main.My Table", t), "main.\"My Table\"");
        assert_eq!(quoted(DuckDb, "main.MyTable", t), "main.MyTable");
    }

    #[test]
    fn escapes_embedded_quote_characters() {
        let c = CompletionKind::Column;
        assert_eq!(quoted(SqlServer, "a]b", c), "[a]]b]");
        assert_eq!(quoted(Postgres, "say\"hi", c), "\"say\"\"hi\"");
        assert_eq!(quoted(MySql, "tick`tock", c), "`tick``tock`");
    }

    #[test]
    fn quotes_columns_schemas_databases_and_routines() {
        assert_eq!(
            quoted(SqlServer, "o.Unit Price", CompletionKind::Column),
            "o.[Unit Price]"
        );
        assert_eq!(
            quoted(SqlServer, "My Schema", CompletionKind::Schema),
            "[My Schema]"
        );
        assert_eq!(
            quoted(Postgres, "AppDb", CompletionKind::Database),
            "\"AppDb\""
        );
        assert_eq!(
            quoted(SqlServer, "dbo.usp Do Thing", CompletionKind::Procedure),
            "dbo.[usp Do Thing]"
        );
        assert_eq!(
            quoted(MySql, "calc-tax", CompletionKind::Function),
            "`calc-tax`"
        );
    }

    #[test]
    fn leaves_keywords_builtins_snippets_and_quoted_text_alone() {
        let mut items = vec![
            item("ORDER BY", "ORDER BY", CompletionKind::Keyword),
            CompletionItem {
                source: CompletionSource::Builtin,
                ..item("COUNT", "COUNT", CompletionKind::Function)
            },
            CompletionItem {
                source: CompletionSource::Snippet,
                ..item("dbo.p", "EXEC [dbo].[p] ", CompletionKind::Procedure)
            },
            item("[Order Details]", "[Order Details]", CompletionKind::Table),
            CompletionItem {
                detail: REQUIRED_GROUP_BY_DETAIL.to_string(),
                ..item("o.Unit Price", "o.Unit Price", CompletionKind::Column)
            },
        ];
        let before = items.clone();
        apply_identifier_quoting(&mut items, SqlServer);
        assert_eq!(items, before);
    }

    #[test]
    fn clause_writing_inserts_quote_only_the_trailing_name() {
        let mut from = item(
            "dbo.Order Details",
            "FROM\n\tdbo.Order Details",
            CompletionKind::Table,
        );
        from.detail = crate::intellisense::FROM_CLAUSE_DETAIL.to_string();
        let mut items = vec![from];
        apply_identifier_quoting(&mut items, SqlServer);
        assert_eq!(items[0].insert_text, "FROM\n\tdbo.[Order Details]");
        assert_eq!(items[0].label, "dbo.Order Details");
    }

    #[test]
    fn plain_paths_are_not_quoted_paths() {
        assert_eq!(
            quoted_path_at("SELECT * FROM dbo.Ord", 21, Some(SqlServer)),
            None
        );
        assert_eq!(quoted_path_at("SELECT o.", 9, Some(SqlServer)), None);
        // Inside a string literal or comment.
        let sql = "SELECT 'it [is";
        assert_eq!(quoted_path_at(sql, sql.len(), Some(SqlServer)), None);
        let sql = "SELECT 1 -- [x";
        assert_eq!(quoted_path_at(sql, sql.len(), Some(SqlServer)), None);
        let sql = "SELECT 1 /* [x";
        assert_eq!(quoted_path_at(sql, sql.len(), Some(SqlServer)), None);
        // A lone `/` or `-` right before the caret is not a comment opener.
        for sql in ["SELECT [a] /", "SELECT [a] -"] {
            assert_eq!(quoted_path_at(sql, sql.len(), Some(SqlServer)), None);
        }
        // A closed block comment before a quoted path is skipped.
        let sql = "SELECT /* c */ [Ord";
        assert!(quoted_path_at(sql, sql.len(), Some(SqlServer)).is_some());
        // MySQL reads `"` as a string delimiter, not an identifier quote.
        let sql = "SELECT * FROM \"ord";
        assert_eq!(quoted_path_at(sql, sql.len(), Some(MySql)), None);
        // A closed quoted name at the caret has nothing left to complete.
        let sql = "SELECT * FROM [Orders]";
        assert_eq!(quoted_path_at(sql, sql.len(), Some(SqlServer)), None);
    }

    #[test]
    fn recognizes_an_opening_bracket_on_the_leaf() {
        let sql = "SELECT * FROM [Order D";
        let path = quoted_path_at(sql, sql.len(), Some(SqlServer)).unwrap();
        assert_eq!(path.leaf_start, 14);
        assert_eq!(path.path_start, 14);
        assert!(path.qualifier.is_empty());
        assert_eq!(path.partial, "Order D");
        assert_eq!(path.open, Some(('[', ']')));
        assert!(!path.closer_follows);
        let (plain, cursor) = path.plain_sql(sql, sql.len());
        assert_eq!(plain, "SELECT * FROM OrderD");
        assert_eq!(cursor, plain.len());
    }

    #[test]
    fn recognizes_quoted_qualifiers_and_a_following_closer() {
        let sql = "SELECT * FROM [dbo].[Ord] WHERE 1=1";
        let cursor = "SELECT * FROM [dbo].[Ord".len();
        let path = quoted_path_at(sql, cursor, Some(SqlServer)).unwrap();
        assert_eq!(path.qualifier, vec!["dbo".to_string()]);
        assert_eq!(path.partial, "Ord");
        assert!(path.closer_follows);
        assert_eq!(path.replace_end(cursor), cursor + 1);
        let (plain, plain_cursor) = path.plain_sql(sql, cursor);
        assert_eq!(plain, "SELECT * FROM dbo.Ord WHERE 1=1");
        assert_eq!(&plain[..plain_cursor], "SELECT * FROM dbo.Ord");

        let sql = "SELECT \"Order Details\".qu";
        let path = quoted_path_at(sql, sql.len(), Some(Postgres)).unwrap();
        assert_eq!(path.qualifier, vec!["Order Details".to_string()]);
        assert_eq!(path.partial, "qu");
        assert_eq!(path.open, None);
        assert_eq!(path.leaf_start, sql.len() - 2);
        assert_eq!(path.plain_sql(sql, sql.len()).0, "SELECT OrderDetails.qu");

        let sql = "SELECT `my db`.`ord";
        let path = quoted_path_at(sql, sql.len(), Some(MySql)).unwrap();
        assert_eq!(path.qualifier, vec!["my db".to_string()]);
        assert_eq!(path.open, Some(('`', '`')));

        let sql = "SELECT [Order Details].";
        let path = quoted_path_at(sql, sql.len(), Some(SqlServer)).unwrap();
        assert_eq!(path.partial, "");
        assert_eq!(path.leaf_start, sql.len());
    }

    #[test]
    fn open_quote_items_complete_inside_the_users_quote() {
        let sql = "SELECT * FROM [Ord]";
        let cursor = sql.len() - 1;
        let path = quoted_path_at(sql, cursor, Some(SqlServer)).unwrap();
        let items = vec![
            item(
                "dbo.Order Details",
                "dbo.Order Details",
                CompletionKind::Table,
            ),
            item("dbo.Orders", "dbo.Orders", CompletionKind::Table),
            item("ORDER BY", "ORDER BY", CompletionKind::Keyword),
        ];
        let done = path.finish_items(items, cursor, Some(SqlServer));
        assert_eq!(done.len(), 2, "keywords are dropped inside a quoted name");
        assert_eq!(done[0].insert_text, "[dbo].[Order Details]");
        assert_eq!(done[1].insert_text, "[dbo].[Orders]");
        assert_eq!(done[0].replace_span, Some((14, cursor + 1)));

        // The user's own quote style wins, with the closer escaped.
        let sql = "SELECT \"we";
        let path = quoted_path_at(sql, sql.len(), Some(SqlServer)).unwrap();
        let done = path.finish_items(
            vec![item("we\"ird", "we\"ird", CompletionKind::Column)],
            sql.len(),
            Some(SqlServer),
        );
        assert_eq!(done[0].insert_text, "\"we\"\"ird\"");
        assert_eq!(done[0].replace_span, Some((7, sql.len())));
        assert_eq!(quote_all_parts("a]b", '[', ']'), "[a]]b]");
    }

    #[test]
    fn closed_qualifier_items_quote_the_leaf_as_needed() {
        let sql = "SELECT [Order Details].";
        let path = quoted_path_at(sql, sql.len(), Some(SqlServer)).unwrap();
        let done = path.finish_items(
            vec![
                item(
                    "Order Details.Unit Price",
                    "Unit Price",
                    CompletionKind::Column,
                ),
                item("Order Details.Quantity", "Quantity", CompletionKind::Column),
            ],
            sql.len(),
            Some(SqlServer),
        );
        assert_eq!(done[0].insert_text, "[Unit Price]");
        assert_eq!(done[1].insert_text, "Quantity");
        assert_eq!(done[1].replace_span, None);
    }
}
