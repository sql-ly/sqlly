//! Procedural layout: splitting a statement's items into [`Unit`]s — the
//! lines of a routine header, `BEGIN` / `END`, `IF` / `ELSE` / `WHILE`
//! heads, and the statements of a body — and giving each its indent.
//!
//! The clause emitter only knows queries: on its own it glues a routine
//! header, `AS`, and `BEGIN` onto one line and prints every statement of the
//! body at column 0. The [`Scanner`] runs ahead of it. It keeps the stack of
//! open blocks across the `;`-separated statements of a batch, so a body
//! statement is indented under the `BEGIN` that an earlier statement opened.
//!
//! T-SQL needs no `;` between statements, so there the scanner also finds
//! the statement starts inside one `;`-delimited run. The other dialects
//! terminate every body statement, and only the block words (`BEGIN`,
//! `THEN`, `ELSE`, `LOOP`, …) split a statement.
//!
//! Units are decided from tokens alone, never from line breaks, so
//! formatting the formatter's own output finds the same units.

use super::tree::Item;
use crate::sqllex::{IdentQuote, ParamStyle, Token, TokenKind};
use crate::sqlmodel::Dialect;

/// How a unit's nodes are laid out.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Layout {
    /// One flowing line (wrapped when long): headers, block words, DDL, and
    /// statements whose words would be mistaken for query clauses
    /// (`FETCH NEXT FROM c`, `GRANT SELECT ON t`).
    Flat,
    /// The query-block layout: clause detection, lists, one-line shortcuts.
    Auto,
}

/// A run of a statement's items that starts its own line at `indent`.
pub(super) struct Unit {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) indent: usize,
    pub(super) layout: Layout,
}

#[derive(Clone, Copy)]
enum Frame {
    /// `BEGIN … END`; the body sits one level in from `indent`. `exception`
    /// is set once a PL/SQL `EXCEPTION` section opened in it.
    Block { indent: usize, exception: bool },
    /// T-SQL `IF` / `ELSE` / `WHILE`, which owns the one statement after it.
    /// `filled` once that statement started.
    Ctl {
        indent: usize,
        is_if: bool,
        filled: bool,
    },
    /// A declaration section: PL/SQL `DECLARE`, or the `IS` / `AS` part of
    /// a routine or package, closed by `BEGIN` or `END`.
    Decl { indent: usize },
    /// The body of `IF … THEN`, `LOOP`, `WHILE … DO`, `REPEAT`, closed by
    /// `END IF` / `END LOOP` / ….
    Nest { indent: usize },
}

pub(super) struct Scanner {
    dialect: Dialect,
    frames: Vec<Frame>,
}

/// A unit under construction: where it starts (code position), its indent,
/// its head word, and the layout when the scanner already knows it.
struct Start {
    pos: usize,
    indent: usize,
    head: String,
    layout: Option<Layout>,
}

/// What the T-SQL walk remembers about the unit it is inside.
#[derive(Default)]
struct UnitState {
    /// Code position of the unit's first word.
    start: usize,
    head: String,
    indent: usize,
    /// `INSERT …` still waiting for its `SELECT` / `EXEC` / `VALUES`.
    insert_wait: bool,
    /// `UPDATE …` still waiting for its `SET`.
    set_wait: bool,
    /// `WITH cte AS (…)` still waiting for the statement it feeds.
    cte_pending: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum RoutineKind {
    Procedure,
    Function,
    Trigger,
}

struct Header {
    kind: RoutineKind,
    params_started: bool,
    seen_returns: bool,
    seen_on: bool,
    in_with: bool,
}

/// The code tokens of one statement with their lower-cased word text (empty
/// for anything that is not a keyword or unquoted identifier).
struct Code<'a> {
    toks: Vec<Token<'a>>,
    words: Vec<String>,
}

impl<'a> Code<'a> {
    fn new(items: &[Item<'a>], code: &[usize]) -> Self {
        let toks: Vec<Token<'a>> = code.iter().map(|&k| items[k].tok).collect();
        let words = toks
            .iter()
            .map(|t| match t.kind {
                TokenKind::Keyword | TokenKind::Ident(IdentQuote::None) => {
                    t.text.to_ascii_lowercase()
                }
                _ => String::new(),
            })
            .collect();
        Self { toks, words }
    }

    fn len(&self) -> usize {
        self.toks.len()
    }

    /// Lower-cased word at `p`; empty out of range or for a non-word.
    fn word(&self, p: usize) -> &str {
        self.words.get(p).map(String::as_str).unwrap_or("")
    }

    fn kind(&self, p: usize) -> Option<TokenKind> {
        self.toks.get(p).map(|t| t.kind)
    }

    fn is_param(&self, p: usize) -> bool {
        matches!(
            self.kind(p),
            Some(TokenKind::ParamMarker(ParamStyle::AtName))
        )
    }

    fn is_op(&self, p: usize, text: &str) -> bool {
        self.toks
            .get(p)
            .is_some_and(|t| t.kind == TokenKind::Operator && t.text == text)
    }

    /// First position in `from..to` at paren depth 0, outside any
    /// `CASE … END`, whose word is one of `words`.
    fn find_top(&self, from: usize, to: usize, words: &[&str]) -> Option<usize> {
        let mut depth = 0usize;
        let mut case_depth = 0usize;
        for p in from..to.min(self.len()) {
            match self.toks[p].kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => depth = depth.saturating_sub(1),
                _ if depth == 0 => {
                    let w = self.word(p);
                    if case_depth == 0 && words.contains(&w) {
                        return Some(p);
                    }
                    if w == "case" {
                        case_depth += 1;
                    } else if w == "end" {
                        case_depth = case_depth.saturating_sub(1);
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// Words after which the statement still needs an operand, so a statement
/// keyword right behind them is a name, not the start of a statement.
const OPERAND_EXPECTED: &[&str] = &[
    "select", "distinct", "from", "join", "where", "and", "or", "not", "by", "into", "having",
    "when", "in", "like", "between", "is", "case", "values", "table", "top", "set", "as",
];

/// T-SQL `SET` options, which the keyword tables leave as plain names.
const SET_OPTIONS: &[&str] = &[
    "nocount",
    "xact_abort",
    "ansi_nulls",
    "ansi_padding",
    "ansi_warnings",
    "ansi_defaults",
    "ansi_null_dflt_on",
    "ansi_null_dflt_off",
    "arithabort",
    "arithignore",
    "quoted_identifier",
    "concat_null_yields_null",
    "numeric_roundabort",
    "cursor_close_on_commit",
    "implicit_transactions",
    "remote_proc_transactions",
    "identity_insert",
    "statistics",
    "datefirst",
    "dateformat",
    "language",
    "lock_timeout",
    "deadlock_priority",
    "rowcount",
    "textsize",
    "noexec",
    "parseonly",
    "fmtonly",
    "forceplan",
    "showplan_all",
    "showplan_text",
    "showplan_xml",
];

/// Options of `DECLARE c CURSOR … FOR`.
const CURSOR_OPTIONS: &[&str] = &[
    "local",
    "global",
    "forward_only",
    "scroll",
    "static",
    "keyset",
    "dynamic",
    "fast_forward",
    "read_only",
    "scroll_locks",
    "optimistic",
    "type_warning",
    "insensitive",
];

/// Routine options of a T-SQL `WITH` header line.
const ROUTINE_OPTIONS: &[&str] = &[
    "recompile",
    "encryption",
    "schemabinding",
    "native_compilation",
    "caller",
    "owner",
    "self",
    "inline",
    "called",
    "input",
];

impl Scanner {
    pub(super) fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            frames: Vec::new(),
        }
    }

    /// A batch separator ends every open block.
    pub(super) fn reset(&mut self) {
        self.frames.clear();
    }

    /// Indent a lone `;` (a statement with no code) sits at.
    pub(super) fn body_indent(&self) -> usize {
        match self.frames.last() {
            Some(Frame::Block { indent, exception }) => indent + if *exception { 2 } else { 1 },
            Some(Frame::Ctl { indent, .. })
            | Some(Frame::Decl { indent })
            | Some(Frame::Nest { indent }) => indent + 1,
            None => 0,
        }
    }

    /// Split one statement's items into units, marking the words whose role
    /// (data type, keyword) only the routine grammar reveals.
    pub(super) fn scan(&mut self, items: &mut [Item<'_>]) -> Vec<Unit> {
        let code: Vec<usize> = (0..items.len())
            .filter(|&k| !matches!(items[k].tok.kind, TokenKind::Comment(_)))
            .collect();
        if code.is_empty() {
            if items.is_empty() {
                return Vec::new();
            }
            return vec![Unit {
                start: 0,
                end: items.len(),
                indent: self.body_indent(),
                layout: Layout::Auto,
            }];
        }
        let c = Code::new(items, &code);
        let mut marks = Marks::default();
        let starts = if self.dialect == Dialect::SqlServer {
            self.scan_tsql(&c, &mut marks)
        } else {
            self.scan_generic(&c, &mut marks)
        };
        for &p in &marks.types {
            items[code[p]].is_cast_type = true;
        }
        for &p in &marks.keywords {
            items[code[p]].is_context_keyword = true;
            items[code[p]].is_function_name = false;
        }
        // `retry:` keeps its colon against the label.
        for s in &starts {
            if matches!(c.kind(s.pos), Some(TokenKind::Ident(_))) && c.is_op(s.pos + 1, ":") {
                items[code[s.pos + 1]].attach = true;
            }
        }

        let mut units: Vec<Unit> = Vec::with_capacity(starts.len());
        for (k, s) in starts.iter().enumerate() {
            let end_pos = starts.get(k + 1).map(|n| n.pos).unwrap_or(c.len());
            let layout = s
                .layout
                .unwrap_or_else(|| layout_for(self.dialect, &c, s.pos, end_pos, &s.head));
            // Comments on their own lines right above a unit belong to it;
            // a comment trailing the previous unit's last line stays there.
            let mut start = if k == 0 {
                0
            } else {
                let after_prev = code[s.pos - 1] + 1;
                (after_prev..code[s.pos])
                    .find(|&i| items[i].newlines_before > 0)
                    .unwrap_or(code[s.pos])
            };
            if let Some(prev) = units.last_mut() {
                prev.end = start;
            }
            // Above a closing `END` they describe the body, and sit with it.
            if s.head == "end" && start < code[s.pos] {
                units.push(Unit {
                    start,
                    end: code[s.pos],
                    indent: s.indent + 1,
                    layout: Layout::Auto,
                });
                start = code[s.pos];
            }
            units.push(Unit {
                start,
                end: items.len(),
                indent: s.indent,
                layout,
            });
        }
        units
    }

    // -- T-SQL ---------------------------------------------------------------

    fn scan_tsql(&mut self, c: &Code<'_>, marks: &mut Marks) -> Vec<Start> {
        let mut out: Vec<Start> = Vec::new();
        let mut depth = 0usize;
        let mut case_depth = 0usize;
        let mut force_at = Some(0usize);
        let mut header: Option<Header> = None;
        let mut u = UnitState::default();

        for p in 0..c.len() {
            let kind = c.toks[p].kind;
            let w = c.word(p);
            let prev = p.wrapping_sub(1);

            if let Some(h) = header.as_mut()
                && depth <= 1
            {
                mark_header_word(c, p, h, marks);
            }
            if depth == 0 && case_depth == 0 {
                let mut handled = false;
                if let Some(h) = header.as_mut() {
                    handled = true;
                    let base = u.indent;
                    let flat_at = |indent: usize, head: &str, out: &mut Vec<Start>| {
                        out.push(Start {
                            pos: p,
                            indent,
                            head: head.to_string(),
                            layout: Some(Layout::Flat),
                        });
                    };
                    match w {
                        "as" if !matches!(c.word(prev), "exec" | "execute")
                            && !c.is_param(prev) =>
                        {
                            flat_at(base, "as", &mut out);
                            header = None;
                            force_at = Some(p + 1);
                        }
                        "begin" => {
                            header = None;
                            handled = false;
                        }
                        "returns" if !h.seen_returns => {
                            h.seen_returns = true;
                            h.in_with = false;
                            flat_at(base, "returns", &mut out);
                        }
                        "with" => {
                            h.in_with = true;
                            flat_at(base, "with", &mut out);
                        }
                        "on" if h.kind == RoutineKind::Trigger && !h.seen_on => {
                            h.seen_on = true;
                            flat_at(base, "on", &mut out);
                        }
                        "for" | "after" | "instead"
                            if h.kind == RoutineKind::Trigger
                                && h.seen_on
                                && c.word(prev) != "not" =>
                        {
                            h.in_with = false;
                            marks.keywords.push(p);
                            if w == "instead" && c.word(p + 1) == "of" {
                                marks.keywords.push(p + 1);
                            }
                            flat_at(base, w, &mut out);
                        }
                        _ if c.is_param(p)
                            && !h.seen_returns
                            && (c.kind(prev) == Some(TokenKind::Comma) || !h.params_started) =>
                        {
                            h.params_started = true;
                            flat_at(base + 1, "@", &mut out);
                        }
                        _ => {}
                    }
                }
                if !handled {
                    let forced = force_at == Some(p);
                    // `DECLARE c CURSOR FOR SELECT …`: the query hangs one
                    // level under the declaration.
                    let cursor_query =
                        u.head == "declare" && w == "select" && c.word(prev) == "for";
                    if cursor_query {
                        out.push(Start {
                            pos: p,
                            indent: u.indent + 1,
                            head: "select".into(),
                            layout: None,
                        });
                        let indent = u.indent;
                        u = UnitState {
                            start: p,
                            head: "select".into(),
                            indent,
                            ..UnitState::default()
                        };
                    } else if forced || tsql_starts_statement(c, p, &u) {
                        // Statement words the keyword tables miss (`PRINT`,
                        // `WHILE`, …) are keywords here whatever their kind.
                        if tsql_starts_statement(c, p, &UnitState::default()) {
                            marks.keywords.push(p);
                        }
                        force_at = None;
                        let start = self.tsql_begin_unit(c, p, &mut force_at, &mut header, marks);
                        u = UnitState {
                            start: p,
                            head: start.head.clone(),
                            indent: start.indent,
                            insert_wait: start.head == "insert",
                            set_wait: start.head == "update",
                            cte_pending: false,
                        };
                        out.push(start);
                    } else {
                        match w {
                            "select" | "exec" | "execute" | "values" | "default" => {
                                u.insert_wait = false
                            }
                            "set" => u.set_wait = false,
                            _ => {}
                        }
                        if matches!(w, "select" | "insert" | "update" | "delete" | "merge") {
                            u.cte_pending = false;
                        }
                    }
                    // `WITH name AS (` / `WITH name (cols) AS (`.
                    if w == "with"
                        && c.kind(p + 1)
                            .is_some_and(|k| matches!(k, TokenKind::Ident(_) | TokenKind::Keyword))
                        && (c.word(p + 2) == "as" || c.kind(p + 2) == Some(TokenKind::LParen))
                    {
                        u.cte_pending = true;
                    }
                    mark_tsql_word(c, p, &u, marks);
                }
            }
            match kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => depth = depth.saturating_sub(1),
                _ if depth == 0 => {
                    if w == "case" && c.kind(prev) != Some(TokenKind::Dot) {
                        case_depth += 1;
                    } else if w == "end" && case_depth > 0 {
                        case_depth -= 1;
                    }
                }
                _ => {}
            }
            // `varchar(max)`: MAX is not the aggregate here.
            if w == "max"
                && c.kind(prev) == Some(TokenKind::LParen)
                && c.kind(p + 1) == Some(TokenKind::RParen)
                && matches!(
                    c.word(p.wrapping_sub(2)),
                    "varchar" | "nvarchar" | "varbinary"
                )
            {
                marks.keywords.push(p);
            }
        }
        out
    }

    /// Open the unit starting at `p`: place it against the open blocks and
    /// push the frame it opens.
    fn tsql_begin_unit(
        &mut self,
        c: &Code<'_>,
        p: usize,
        force_at: &mut Option<usize>,
        header: &mut Option<Header>,
        marks: &mut Marks,
    ) -> Start {
        let w = c.word(p);
        let next = c.word(p + 1);
        let flat = |indent: usize, head: &str| Start {
            pos: p,
            indent,
            head: head.to_string(),
            layout: Some(Layout::Flat),
        };
        match w {
            "end" if next != "conversation" => {
                let indent = self.pop_block();
                let extra = usize::from(matches!(next, "try" | "catch"));
                if extra == 1 {
                    marks.keywords.push(p + 1);
                }
                *force_at = Some(p + 1 + extra);
                flat(indent, "end")
            }
            "else" => {
                // The IF this ELSE answers: the innermost finished IF among
                // the control frames on top of the stack.
                let mut found = None;
                for k in (0..self.frames.len()).rev() {
                    match self.frames[k] {
                        Frame::Ctl {
                            is_if: true,
                            filled: true,
                            ..
                        } => {
                            found = Some(k);
                            break;
                        }
                        Frame::Ctl { filled: true, .. } => {}
                        _ => break,
                    }
                }
                let indent = match found {
                    Some(k) => {
                        let Frame::Ctl { indent, .. } = self.frames[k] else {
                            unreachable!()
                        };
                        self.frames.truncate(k);
                        indent
                    }
                    None => self.tsql_statement_indent(false),
                };
                let else_if = next == "if";
                self.frames.push(Frame::Ctl {
                    indent,
                    is_if: else_if,
                    filled: false,
                });
                if !else_if {
                    *force_at = Some(p + 1);
                }
                flat(indent, if else_if { "if" } else { "else" })
            }
            "begin"
                if !matches!(
                    next,
                    "transaction" | "tran" | "distributed" | "dialog" | "conversation"
                ) =>
            {
                let indent = self.tsql_statement_indent(true);
                self.frames.push(Frame::Block {
                    indent,
                    exception: false,
                });
                let extra = usize::from(matches!(next, "try" | "catch"));
                if extra == 1 {
                    marks.keywords.push(p + 1);
                }
                *force_at = Some(p + 1 + extra);
                flat(indent, "begin")
            }
            "if" | "while" => {
                let indent = self.tsql_statement_indent(false);
                self.frames.push(Frame::Ctl {
                    indent,
                    is_if: w == "if",
                    filled: false,
                });
                flat(indent, w)
            }
            _ => {
                let indent = self.tsql_statement_indent(false);
                if let Some(kind) = tsql_routine_kind(c, p) {
                    *header = Some(Header {
                        kind,
                        params_started: false,
                        seen_returns: false,
                        seen_on: false,
                        in_with: false,
                    });
                    return flat(indent, w);
                }
                Start {
                    pos: p,
                    indent,
                    head: w.to_string(),
                    layout: None,
                }
            }
        }
    }

    /// Indent for a statement starting now. Finished `IF` / `WHILE` frames
    /// are dropped; an unfinished one takes this statement as its body.
    fn tsql_statement_indent(&mut self, is_begin: bool) -> usize {
        while let Some(Frame::Ctl { filled: true, .. }) = self.frames.last() {
            self.frames.pop();
        }
        match self.frames.last_mut() {
            Some(Frame::Ctl { indent, filled, .. }) => {
                *filled = true;
                // `IF x` / `BEGIN` line up; a single statement indents.
                if is_begin { *indent } else { *indent + 1 }
            }
            _ => self.body_indent(),
        }
    }

    /// Close the innermost `BEGIN` block (and any control frames inside
    /// it); the indent its `END` goes at. A stray `END` sits at column 0.
    fn pop_block(&mut self) -> usize {
        while let Some(frame) = self.frames.pop() {
            if let Frame::Block { indent, .. } | Frame::Decl { indent } = frame {
                return indent;
            }
        }
        0
    }

    // -- other dialects --------------------------------------------------------

    fn scan_generic(&mut self, c: &Code<'_>, marks: &mut Marks) -> Vec<Start> {
        let n = c.len();
        let oracle = self.dialect == Dialect::Oracle;
        let mut out: Vec<Start> = Vec::new();
        let mut p = 0usize;
        // The `BEGIN` found by a routine header opens a block whatever
        // follows it.
        let mut header_begin = false;
        // A `label:` already began the unit its loop or block belongs to.
        let mut labelled = false;
        while p < n {
            let w = c.word(p);
            let next = c.word(p + 1);
            let flat = |indent: usize, head: &str| Start {
                pos: p,
                indent,
                head: head.to_string(),
                layout: Some(Layout::Flat),
            };
            let top = self.frames.last().copied();
            let was_labelled = std::mem::take(&mut labelled);
            if !self.frames.is_empty()
                && !was_labelled
                && matches!(c.kind(p), Some(TokenKind::Ident(_)))
                && c.is_op(p + 1, ":")
                && matches!(c.word(p + 2), "loop" | "while" | "repeat" | "begin" | "for")
            {
                out.push(flat(self.body_indent(), "label"));
                labelled = true;
                p += 2;
                continue;
            }
            let mut push = |start: Start| {
                if !was_labelled {
                    out.push(start);
                }
            };
            match w {
                "end" => {
                    let indent = match next {
                        "if" | "loop" | "while" | "repeat" | "for" => match top {
                            Some(Frame::Nest { indent }) => {
                                self.frames.pop();
                                indent
                            }
                            _ => self.body_indent(),
                        },
                        "case" => self.body_indent(),
                        _ => self.pop_block(),
                    };
                    marks.keywords.push(p);
                    if matches!(next, "if" | "loop" | "while" | "repeat" | "for" | "case") {
                        marks.keywords.push(p + 1);
                    }
                    push(flat(indent, "end"));
                    break;
                }
                "begin" if header_begin || self.generic_begin_opens() => {
                    header_begin = false;
                    let indent = match top {
                        Some(Frame::Decl { indent }) => {
                            self.frames.pop();
                            indent
                        }
                        _ => self.body_indent(),
                    };
                    self.frames.push(Frame::Block {
                        indent,
                        exception: false,
                    });
                    marks.keywords.push(p);
                    push(flat(indent, "begin"));
                    p += 1 + usize::from(next == "atomic");
                }
                "if" | "elsif" | "elseif" | "while" | "for" | "when" => {
                    let closers: &[&str] = match w {
                        "while" | "for" => &["loop", "do"],
                        _ => &["then"],
                    };
                    let in_exception = matches!(
                        top,
                        Some(Frame::Block {
                            exception: true,
                            ..
                        })
                    );
                    let applies = match w {
                        "elsif" | "elseif" => matches!(top, Some(Frame::Nest { .. })),
                        "when" => in_exception,
                        _ => true,
                    };
                    let Some(t) = applies.then(|| c.find_top(p + 1, n, closers)).flatten() else {
                        push(self.generic_statement(c, p));
                        break;
                    };
                    let indent = match (w, top) {
                        ("elsif" | "elseif", Some(Frame::Nest { indent })) => indent,
                        ("when", Some(Frame::Block { indent, .. })) => indent + 1,
                        _ => {
                            let indent = self.body_indent();
                            self.frames.push(Frame::Nest { indent });
                            indent
                        }
                    };
                    marks.keywords.push(p);
                    marks.keywords.push(t);
                    push(flat(indent, w));
                    p = t + 1;
                }
                "else" if matches!(top, Some(Frame::Nest { .. })) => {
                    let Some(Frame::Nest { indent }) = top else {
                        unreachable!()
                    };
                    marks.keywords.push(p);
                    push(flat(indent, "else"));
                    p += 1;
                }
                "loop" | "repeat" if c.kind(p + 1) != Some(TokenKind::LParen) => {
                    let indent = self.body_indent();
                    self.frames.push(Frame::Nest { indent });
                    marks.keywords.push(p);
                    push(flat(indent, w));
                    p += 1;
                }
                "until" if matches!(top, Some(Frame::Nest { .. })) => {
                    let Some(Frame::Nest { indent }) = top else {
                        unreachable!()
                    };
                    if c.find_top(p + 1, n, &["end"]).is_some() {
                        self.frames.pop();
                    }
                    marks.keywords.push(p);
                    push(flat(indent, "until"));
                    break;
                }
                "declare" if oracle => {
                    let indent = self.body_indent();
                    self.frames.push(Frame::Decl { indent });
                    marks.keywords.push(p);
                    push(flat(indent, "declare"));
                    p += 1;
                }
                "exception"
                    if matches!(
                        top,
                        Some(Frame::Block {
                            exception: false,
                            ..
                        })
                    ) && matches!(next, "" | "when") =>
                {
                    let Some(Frame::Block { indent, .. }) = top else {
                        unreachable!()
                    };
                    *self.frames.last_mut().expect("top frame") = Frame::Block {
                        indent,
                        exception: true,
                    };
                    marks.keywords.push(p);
                    push(flat(indent, "exception"));
                    p += 1;
                }
                "create" | "alter" | "procedure" | "function"
                    if matches!(w, "create" | "alter")
                        || matches!(top, Some(Frame::Decl { .. })) =>
                {
                    let kind_at = generic_routine_kind_at(c, p);
                    let Some(kind_at) = kind_at else {
                        push(self.generic_statement(c, p));
                        break;
                    };
                    let begin = c.find_top(kind_at + 1, n, &["begin"]);
                    // PL/SQL: `IS` / `AS` ends the header and opens the
                    // declaration section.
                    let section =
                        (oracle && c.word(kind_at) != "trigger" && c.word(kind_at) != "event")
                            .then(|| c.find_top(kind_at + 1, begin.unwrap_or(n), &["is", "as"]))
                            .flatten()
                            .filter(|&t| !matches!(c.word(t + 1), "language" | "external"));
                    if let Some(t) = section {
                        marks.keywords.push(t);
                        let indent = self.body_indent();
                        self.frames.push(Frame::Decl { indent });
                        push(flat(indent, w));
                        p = t + 1;
                    } else if let Some(d) = oracle
                        .then(|| c.find_top(kind_at + 1, begin.unwrap_or(n), &["declare"]))
                        .flatten()
                    {
                        // A PL/SQL trigger may declare before its `BEGIN`.
                        push(flat(self.body_indent(), w));
                        p = d;
                    } else if let Some(b) = begin {
                        push(flat(self.body_indent(), w));
                        header_begin = true;
                        p = b;
                    } else {
                        push(self.generic_statement(c, p));
                        break;
                    }
                }
                _ => {
                    push(self.generic_statement(c, p));
                    break;
                }
            }
        }
        out
    }

    fn generic_statement(&self, c: &Code<'_>, p: usize) -> Start {
        Start {
            pos: p,
            indent: self.body_indent(),
            head: c.word(p).to_string(),
            layout: None,
        }
    }

    /// Whether the `BEGIN` at `p` (the first word of a statement) opens a
    /// block rather than a transaction.
    fn generic_begin_opens(&self) -> bool {
        self.dialect == Dialect::Oracle || !self.frames.is_empty()
    }
}

#[derive(Default)]
struct Marks {
    types: Vec<usize>,
    keywords: Vec<usize>,
}

/// Position of the routine kind word of the `CREATE …` / `ALTER …` (or, in
/// a PL/SQL declaration section, bare `PROCEDURE` / `FUNCTION`) at `p`.
fn generic_routine_kind_at(c: &Code<'_>, p: usize) -> Option<usize> {
    for k in p..c.len().min(p + 12) {
        match c.word(k) {
            "procedure" | "function" | "trigger" | "event" | "package" => return Some(k),
            "table" | "view" | "index" | "type" | "sequence" | "synonym" | "database"
            | "schema" | "user" | "role" | "policy" | "rule" | "extension" | "domain" => {
                return None;
            }
            _ if c.kind(k) == Some(TokenKind::LParen) => return None,
            _ => {}
        }
    }
    None
}

/// `CREATE [OR ALTER] PROC|PROCEDURE|FUNCTION|TRIGGER` / `ALTER …` at `p`.
fn tsql_routine_kind(c: &Code<'_>, p: usize) -> Option<RoutineKind> {
    let mut k = p + 1;
    match c.word(p) {
        "create" => {
            if c.word(k) == "or" && c.word(k + 1) == "alter" {
                k += 2;
            }
        }
        "alter" => {}
        _ => return None,
    }
    match c.word(k) {
        "proc" | "procedure" => Some(RoutineKind::Procedure),
        "function" => Some(RoutineKind::Function),
        "trigger" => Some(RoutineKind::Trigger),
        _ => None,
    }
}

/// Whether the word at `p` (paren depth 0, outside CASE) starts a new T-SQL
/// statement inside the unit `u`.
fn tsql_starts_statement(c: &Code<'_>, p: usize, u: &UnitState) -> bool {
    let w = c.word(p);
    if w.is_empty() {
        return false;
    }
    let prev = p.wrapping_sub(1);
    if matches!(
        c.kind(prev),
        Some(TokenKind::Comma | TokenKind::Operator | TokenKind::Dot)
    ) || matches!(c.kind(p + 1), Some(TokenKind::Dot))
        || c.is_op(p + 1, "=")
    {
        return false;
    }
    let pw = c.word(prev);
    let next = c.word(p + 1);
    if matches!(w, "begin" | "end" | "else") {
        return true;
    }
    // `INSERT INTO t DEFAULT VALUES` is complete; any other `VALUES` (and
    // the rest of the list) still wants its operand.
    let default_values = pw == "values" && c.word(p.wrapping_sub(2)) == "default";
    if OPERAND_EXPECTED.contains(&pw) && !default_values {
        return false;
    }
    // A permission statement lists statement words (`GRANT SELECT, INSERT`).
    // Once the grantee is named, the next statement word begins a new one.
    if matches!(u.head.as_str(), "grant" | "revoke" | "deny") {
        if matches!(w, "grant" | "revoke" | "deny") {
            return pw != "with";
        }
        if c.find_top(u.start, p, &["to", "from"]).is_none() {
            return false;
        }
    }
    match w {
        "declare" | "print" | "raiserror" | "return" | "while" | "break" | "continue" | "goto"
        | "waitfor" | "deallocate" | "use" | "commit" | "rollback" | "dbcc" | "open" | "close"
        | "revoke" | "deny" | "create" => true,
        "grant" => pw != "with",
        "save" => matches!(next, "tran" | "transaction"),
        "truncate" => next == "table",
        "throw" => {
            c.kind(p + 1).is_none()
                || c.is_param(p + 1)
                || matches!(c.kind(p + 1), Some(TokenKind::Number(_)))
        }
        "fetch" => !matches!(pw, "rows" | "row" | "only"),
        // `ELSE IF` is one head; `DROP TABLE IF EXISTS t` is not a branch.
        "if" => pw != "else" && !(next == "exists" && c.kind(p + 2) != Some(TokenKind::LParen)),
        "exec" | "execute" => !u.insert_wait && next != "as",
        "set" => {
            !matches!(pw, "update" | "delete")
                && !u.set_wait
                && !matches!(u.head.as_str(), "alter" | "create")
        }
        "select" => {
            !matches!(pw, "union" | "all" | "except" | "intersect" | "for")
                && !u.insert_wait
                && !u.cte_pending
        }
        "insert" => !matches!(pw, "then" | "bulk") && !u.cte_pending,
        "update" => {
            !matches!(pw, "then" | "for" | "on")
                && c.kind(p + 1) != Some(TokenKind::LParen)
                && !u.cte_pending
        }
        "delete" => !matches!(pw, "then" | "on") && !u.cte_pending,
        "merge" => next != "join" && !u.cte_pending,
        // `ALTER TABLE t ALTER COLUMN …` / `… DROP CONSTRAINT …` continue
        // the statement; `ALTER TABLE` / `DROP PROCEDURE` begin one.
        "alter" | "drop" => {
            u.head != "alter"
                || matches!(
                    next,
                    "table"
                        | "proc"
                        | "procedure"
                        | "function"
                        | "view"
                        | "trigger"
                        | "database"
                        | "schema"
                        | "sequence"
                        | "synonym"
                        | "type"
                        | "login"
                        | "role"
                )
        }
        _ => false,
    }
}

/// Marks inside a routine header: parameter and return types, parameter
/// modifiers, `WITH` options.
fn mark_header_word(c: &Code<'_>, p: usize, h: &Header, marks: &mut Marks) {
    let w = c.word(p);
    if w.is_empty() {
        return;
    }
    let prev = p.wrapping_sub(1);
    let after_param = c.is_param(prev) || (c.word(prev) == "as" && c.is_param(p.wrapping_sub(2)));
    let after_returns = c.word(prev) == "returns";
    if (after_param || after_returns)
        && w != "as"
        && c.kind(p + 1) != Some(TokenKind::Dot)
        && c.kind(prev) != Some(TokenKind::Dot)
    {
        marks.types.push(p);
    } else if matches!(w, "output" | "out" | "readonly" | "varying")
        || (h.in_with && ROUTINE_OPTIONS.contains(&w))
    {
        marks.keywords.push(p);
    }
}

/// Marks inside a T-SQL body: `DECLARE` types, `SET` options, `THROW`.
fn mark_tsql_word(c: &Code<'_>, p: usize, u: &UnitState, marks: &mut Marks) {
    let w = c.word(p);
    if w.is_empty() {
        return;
    }
    let prev = p.wrapping_sub(1);
    match u.head.as_str() {
        "declare" if c.word(u.start + 2) == "cursor" => {
            if CURSOR_OPTIONS.contains(&w) {
                marks.keywords.push(p);
            }
        }
        "fetch" => {
            if matches!(
                w,
                "next" | "prior" | "first" | "last" | "absolute" | "relative"
            ) && p == u.start + 1
            {
                marks.keywords.push(p);
            }
        }
        "set" if c.word(u.start + 1) == "transaction" => marks.keywords.push(p),
        "declare" => {
            let after_var =
                c.is_param(prev) || (c.word(prev) == "as" && c.is_param(p.wrapping_sub(2)));
            if after_var
                && w != "as"
                && c.kind(p + 1) != Some(TokenKind::Dot)
                && c.kind(p + 1) != Some(TokenKind::LParen)
            {
                marks.types.push(p);
            }
        }
        "set" => {
            let pw = c.word(prev);
            if (pw == "set" && SET_OPTIONS.contains(&w))
                || (pw == "statistics" && matches!(w, "io" | "time" | "xml" | "profile"))
                || (matches!(w, "on" | "off") && c.kind(p + 1).is_none())
            {
                marks.keywords.push(p);
            }
        }
        "throw" if c.word(p) == "throw" => marks.keywords.push(p),
        _ => {}
    }
}

/// Layout for a unit the scanner placed without deciding how it reads.
fn layout_for(dialect: Dialect, c: &Code<'_>, start: usize, end: usize, head: &str) -> Layout {
    let has_query = || c.find_top(start + 1, end, &["select"]).is_some();
    match head {
        "grant" | "revoke" | "deny" | "fetch" | "print" | "raiserror" | "throw" | "use"
        | "deallocate" | "close" | "break" | "continue" | "goto" | "waitfor" | "commit"
        | "rollback" | "save" | "dbcc" | "truncate" | "drop" | "begin" | "end" => Layout::Flat,
        "declare" | "open" | "return" | "set" | "exec" | "execute" | "call" => {
            if has_query() {
                Layout::Auto
            } else {
                Layout::Flat
            }
        }
        "create" | "alter" => {
            // `CREATE VIEW v AS SELECT …`, `CREATE TABLE t AS SELECT …`.
            let as_query = (start + 1..end).any(|p| {
                c.word(p) == "as"
                    && (matches!(c.word(p + 1), "select" | "with")
                        || (c.kind(p + 1) == Some(TokenKind::LParen)
                            && matches!(c.word(p + 2), "select" | "with")))
            });
            // A trigger / event / rule whose body is one statement.
            let has_body = dialect != Dialect::SqlServer
                && (start..end.min(start + 8))
                    .any(|p| matches!(c.word(p), "trigger" | "event" | "rule"));
            // An index's filter reads as a clause (`… WHERE active = 1`).
            let is_index = (start..end.min(start + 6)).any(|p| c.word(p) == "index");
            if as_query || has_body || is_index {
                Layout::Auto
            } else {
                Layout::Flat
            }
        }
        _ => Layout::Auto,
    }
}
