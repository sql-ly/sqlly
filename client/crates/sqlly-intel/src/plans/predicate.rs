//! Turns the predicate and expression text that query plans print into text
//! a person can read.
//!
//! Plans print predicates for the engine's benefit: SQL Server brackets every
//! name and repeats the full four-part path, PostgreSQL casts every literal
//! and wraps every comparison in parentheses, MySQL backticks names and
//! prints keywords in lower case, Oracle double-quotes every identifier. This
//! module removes that noise without changing what the predicate means.
//!
//! How it works:
//!
//! 1. [`tokenize`] splits the text into string literals, quoted identifiers,
//!    numbers, words, symbols and markers. String literals are opaque from
//!    then on: no later step looks inside one or alters one.
//! 2. [`build_tree`] pairs round parentheses into groups, once. Parentheses
//!    that do not pair up stay as plain symbols and are never removed.
//! 3. `Cleaner::simplify` walks the tree bottom-up. At each level it applies
//!    the engine-specific rewrites (name shortening, cast removal, operator
//!    names), substitutes plan-internal names, upper-cases logical keywords
//!    and finally drops the parentheses that are provably redundant.
//! 4. [`render`] prints the tree with normalised spacing.
//!
//! Every level is processed once and each step is linear in the number of
//! tokens at that level, so the whole thing is linear in the input (nesting
//! is capped at [`MAX_DEPTH`]; deeper parentheses are left as they are).
//!
//! The rules are deliberately conservative. A pair of parentheses is removed
//! only when one of four specific conditions holds (see
//! `Cleaner::removable`); anything the code does not recognise is printed as
//! it was.

use std::collections::HashMap;

use crate::plans::dialects::PlanDialect;

/// A predicate or expression in two forms.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadablePredicate {
    /// Cleaned up for reading.
    pub text: String,
    /// Exactly what the plan printed.
    pub raw: String,
}

/// Internal names a plan defines (`Expr1004`, `$0`, `$1`) mapped to the expression or description they stand for.
/// Keys are stored without brackets, case-sensitive: "Expr1004", "$0".
pub type ExpressionNames = HashMap<String, String>;

/// Clean `raw` for reading. `dialect` None means unknown: apply only the rules that are safe everywhere.
pub fn readable_predicate(
    dialect: Option<PlanDialect>,
    raw: &str,
    names: &ExpressionNames,
) -> ReadablePredicate {
    let trimmed = raw.trim();
    let text = if trimmed.is_empty() {
        String::new()
    } else {
        let mut cleaner = Cleaner::new(dialect, if names.is_empty() { None } else { Some(names) });
        let nodes = cleaner.clean(trimmed);
        render(&nodes)
    };
    ReadablePredicate {
        text,
        raw: raw.to_string(),
    }
}

/// Same cleaning for a single column / sort key / group key / output expression (e.g. `[Shop].[dbo].[Orders].[OrderDate] as [o].[OrderDate]` -> `o.OrderDate`, `o.created_at DESC` stays, `(date_trunc('month'::text, created_at))` -> `date_trunc('month', created_at)`).
pub fn readable_expression(
    dialect: Option<PlanDialect>,
    raw: &str,
    names: &ExpressionNames,
) -> String {
    readable_predicate(dialect, raw, names).text
}

/// Parentheses nested deeper than this are not paired into groups; they are
/// printed unchanged. This bounds the recursion depth of every pass.
const MAX_DEPTH: usize = 64;

/// A substituted expression longer than this (in characters, after cleaning)
/// is not inserted; the plan's own name stays.
const MAX_NAME_CHARS: usize = 60;

// ---------------------------------------------------------------------------
// Tokens and tree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Identifier, keyword or parameter (`o`, `AND`, `@email`, `$1`, `#0`).
    Word,
    /// Numeric literal, including suffixed forms such as `800_UInt16`.
    Num,
    /// String literal including its quotes. Never altered.
    Str,
    /// `[name]` (SQL Server only).
    Bracket,
    /// `"name"`.
    Double,
    /// `` `name` ``.
    Backtick,
    /// Operator or punctuation.
    Sym,
    /// MySQL / SQLite angle-bracket marker such as `<cache>` or `<expr>`.
    Marker,
    /// Text that is printed verbatim and blocks any rewrite around it: the
    /// rest of the input after an unterminated quote, or an inserted
    /// description.
    Raw,
}

#[derive(Debug, Clone)]
struct Tok {
    kind: Kind,
    text: String,
    /// Whitespace preceded this token in the source.
    sp: bool,
    /// For a string literal whose PostgreSQL array cast was removed:
    /// 1 = numeric element type, 2 = any other element type. 0 otherwise.
    note: u8,
}

impl Tok {
    fn new(kind: Kind, text: impl Into<String>, sp: bool) -> Self {
        Tok {
            kind,
            text: text.into(),
            sp,
            note: 0,
        }
    }

    fn is_sym(&self, s: &str) -> bool {
        self.kind == Kind::Sym && self.text == s
    }

    fn is_word(&self, w: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(w)
    }
}

/// How a parenthesised sequence behaves as an operand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// One literal, parameter or (qualified) name.
    Operand,
    /// One function call: a name directly followed by its argument list.
    Call,
    /// A comparison with no AND / OR at its top level.
    Tight,
    /// Operands joined by AND only.
    And,
    /// Operands joined by OR only.
    Or,
    /// AND and OR at the same level.
    Mixed,
    /// Anything else. Never unwrapped as an operand.
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Info {
    class: Class,
    has_comma: bool,
    /// Starts with SELECT / WITH / VALUES.
    subquery: bool,
    /// Contains an unpaired parenthesis.
    ragged: bool,
    empty: bool,
}

const NO_INFO: Info = Info {
    class: Class::Other,
    has_comma: false,
    subquery: false,
    ragged: false,
    empty: true,
};

#[derive(Debug, Clone)]
struct Group {
    sp: bool,
    items: Vec<Node>,
    info: Info,
    /// Created by name substitution rather than read from the plan.
    synthetic: bool,
}

#[derive(Debug, Clone)]
enum Node {
    T(Tok),
    G(Group),
}

impl Node {
    fn sp(&self) -> bool {
        match self {
            Node::T(t) => t.sp,
            Node::G(g) => g.sp,
        }
    }

    fn set_sp(&mut self, sp: bool) {
        match self {
            Node::T(t) => t.sp = sp,
            Node::G(g) => g.sp = sp,
        }
    }

    fn tok(&self) -> Option<&Tok> {
        match self {
            Node::T(t) => Some(t),
            Node::G(_) => None,
        }
    }

    fn is_sym(&self, s: &str) -> bool {
        self.tok().is_some_and(|t| t.is_sym(s))
    }

    fn is_word(&self, w: &str) -> bool {
        self.tok().is_some_and(|t| t.is_word(w))
    }

    fn is_any_word(&self, words: &[&str]) -> bool {
        self.tok().is_some_and(|t| {
            t.kind == Kind::Word && words.iter().any(|w| t.text.eq_ignore_ascii_case(w))
        })
    }

    /// A token that needs a space to separate it from another such token.
    fn is_atomish(&self) -> bool {
        self.tok().is_some_and(|t| t.kind != Kind::Sym)
    }

    /// A name part: a word or a quoted identifier.
    fn is_part(&self) -> bool {
        self.tok().is_some_and(|t| {
            matches!(
                t.kind,
                Kind::Word | Kind::Bracket | Kind::Double | Kind::Backtick
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

/// Multi-character operators, longest first.
const OPS: &[&str] = &[
    "!~~*", "<=>", "~~*", "!~~", "->>", "!~*", "#>>", "::", "<=", ">=", "<>", "!=", "^=", "==",
    "~~", "||", "&&", "->", ":=", "~*", "!~", "<<", ">>", "=>",
];

/// Operators that only PostgreSQL has. Elsewhere `@` and `#` start names.
const PG_OPS: &[&str] = &["@>", "<@", "#>", "@@"];

/// Marker names accepted even when the marker follows an operand.
const MARKER_PREFIXES: &[&str] = &[
    "subquery",
    "derived",
    "temporary",
    "auto_",
    "union",
    "materialize",
    "cache",
    "exists",
    "in_optimizer",
    "expr",
    "not",
    "nop",
    "min",
    "max",
    "is_not_null_test",
    "ref_null_helper",
    "primary_index_lookup",
    "index_lookup",
    "result",
    "intersect",
    "except",
];

fn starts_with_at(chars: &[char], at: usize, pat: &str) -> bool {
    pat.chars()
        .enumerate()
        .all(|(k, p)| chars.get(at + k) == Some(&p))
}

/// End (exclusive) of a quoted run starting at `start`, where the closing
/// quote is `close` and a doubled closing quote is an escape. `backslash`
/// makes `\x` an escape too. None when the quote never closes.
fn scan_quoted(chars: &[char], start: usize, close: char, backslash: bool) -> Option<usize> {
    let mut j = start + 1;
    while j < chars.len() {
        let c = chars[j];
        if backslash && c == '\\' {
            j += 2;
        } else if c == close {
            if chars.get(j + 1) == Some(&close) {
                j += 2;
            } else {
                return Some(j + 1);
            }
        } else {
            j += 1;
        }
    }
    None
}

/// End (exclusive) of a `<name>` marker starting at `start`, and its name.
fn scan_marker(chars: &[char], start: usize) -> Option<usize> {
    let mut j = start + 1;
    let first = *chars.get(j)?;
    if !(first.is_ascii_lowercase() || first == '_') {
        return None;
    }
    while j < chars.len()
        && (chars[j].is_ascii_lowercase() || chars[j].is_ascii_digit() || chars[j] == '_')
    {
        j += 1;
    }
    (chars.get(j) == Some(&'>')).then_some(j + 1)
}

fn tokenize(src: &str, dialect: Option<PlanDialect>) -> Vec<Tok> {
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mysql = matches!(dialect, Some(PlanDialect::MySql | PlanDialect::MariaDb));
    let pg = dialect == Some(PlanDialect::Postgres);
    let mssql = dialect == Some(PlanDialect::SqlServer);
    let word_continue =
        |c: char| c.is_alphanumeric() || c == '_' || c == '$' || (!pg && (c == '@' || c == '#'));

    let mut out: Vec<Tok> = Vec::new();
    let mut i = 0;
    let mut sp = false;
    while i < n {
        let c = chars[i];
        if c.is_whitespace() {
            sp = true;
            i += 1;
            continue;
        }
        let start = i;
        let next = chars.get(i + 1).copied();
        let kind;
        if c == '\'' || (c == '"' && mysql) {
            // E'..' strings (PostgreSQL) and MySQL strings use backslash escapes.
            let e_prefix = !sp
                && out
                    .last()
                    .is_some_and(|t| t.kind == Kind::Word && t.text.eq_ignore_ascii_case("e"));
            match scan_quoted(&chars, i, c, mysql || e_prefix) {
                Some(end) => {
                    kind = Kind::Str;
                    i = end;
                }
                None => {
                    kind = Kind::Raw;
                    i = n;
                }
            }
        } else if c == '"' || c == '`' {
            match scan_quoted(&chars, i, c, false) {
                Some(end) => {
                    kind = if c == '"' {
                        Kind::Double
                    } else {
                        Kind::Backtick
                    };
                    i = end;
                }
                None => {
                    kind = Kind::Raw;
                    i = n;
                }
            }
        } else if c == '[' && mssql {
            match scan_quoted(&chars, i, ']', false) {
                Some(end) => {
                    kind = Kind::Bracket;
                    i = end;
                }
                None => {
                    kind = Kind::Sym;
                    i += 1;
                }
            }
        } else if c.is_ascii_digit() {
            kind = Kind::Num;
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            if i + 1 < n && chars[i] == '.' && chars[i + 1].is_ascii_digit() {
                i += 1;
                while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
            }
        } else if c.is_alphabetic()
            || c == '_'
            || ((c == '@' || c == '#') && !pg && next.is_some_and(&word_continue))
            || (c == '$' && next.is_some_and(|x| x.is_alphanumeric() || x == '_'))
        {
            kind = Kind::Word;
            i += 1;
            while i < n && word_continue(chars[i]) {
                i += 1;
            }
        } else if c == '<'
            && let Some(end) = scan_marker(&chars, i)
            && marker_allowed(&chars, start, end, out.last(), mysql)
        {
            kind = Kind::Marker;
            i = end;
        } else {
            kind = Kind::Sym;
            let op = OPS
                .iter()
                .chain(if pg { PG_OPS.iter() } else { [].iter() })
                .find(|op| starts_with_at(&chars, i, op));
            i += op.map_or(1, |op| op.len());
        }
        out.push(Tok::new(
            kind,
            chars[start..i].iter().collect::<String>(),
            sp,
        ));
        sp = false;
    }
    out
}

/// Whether `<name>` at `start..end` is a marker and not two comparisons.
/// It is one when nothing that could be a left operand precedes it, when the
/// name is one the engines are known to print, or (MySQL) when an argument
/// list follows directly.
fn marker_allowed(
    chars: &[char],
    start: usize,
    end: usize,
    prev: Option<&Tok>,
    mysql: bool,
) -> bool {
    let prefix_position = match prev {
        None => true,
        Some(t) => t.kind == Kind::Sym && t.text != ")" && t.text != "]",
    };
    if prefix_position || (mysql && chars.get(end) == Some(&'(')) {
        return true;
    }
    let name: String = chars[start + 1..end - 1].iter().collect();
    MARKER_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Pair round parentheses into groups. Unpaired ones become plain symbols.
/// The flag is true when nesting went past [`MAX_DEPTH`].
fn build_tree(toks: Vec<Tok>) -> (Vec<Node>, bool) {
    let mut stack: Vec<(bool, Vec<Node>)> = vec![(false, Vec::new())];
    // Open parentheses seen beyond MAX_DEPTH and not yet closed.
    let mut flat = 0usize;
    let mut too_deep = false;
    for t in toks {
        if t.is_sym("(") {
            if stack.len() > MAX_DEPTH {
                flat += 1;
                too_deep = true;
                push_top(&mut stack, Node::T(t));
            } else {
                stack.push((t.sp, Vec::new()));
            }
        } else if t.is_sym(")") {
            if flat > 0 {
                flat -= 1;
                push_top(&mut stack, Node::T(t));
            } else if stack.len() > 1 {
                let (sp, items) = stack.pop().unwrap_or_default();
                push_top(
                    &mut stack,
                    Node::G(Group {
                        sp,
                        items,
                        info: NO_INFO,
                        synthetic: false,
                    }),
                );
            } else {
                push_top(&mut stack, Node::T(t));
            }
        } else {
            push_top(&mut stack, Node::T(t));
        }
    }
    // Unclosed groups: put the parenthesis back as a symbol.
    while stack.len() > 1 {
        let (sp, items) = stack.pop().unwrap_or_default();
        push_top(&mut stack, Node::T(Tok::new(Kind::Sym, "(", sp)));
        if let Some(top) = stack.last_mut() {
            top.1.extend(items);
        }
    }
    (
        stack.pop().map(|(_, items)| items).unwrap_or_default(),
        too_deep,
    )
}

fn push_top(stack: &mut [(bool, Vec<Node>)], node: Node) {
    if let Some(top) = stack.last_mut() {
        top.1.push(node);
    }
}

// ---------------------------------------------------------------------------
// Keyword tables
// ---------------------------------------------------------------------------

/// Words that are syntax, not names. Such a word is never an operand, never
/// a function name, and an identifier spelled like one keeps its quotes.
fn is_structural_kw(word: &str) -> bool {
    if word.len() > 8 {
        return false;
    }
    matches!(
        word.to_ascii_lowercase().as_str(),
        "and"
            | "or"
            | "not"
            | "in"
            | "any"
            | "all"
            | "some"
            | "exists"
            | "between"
            | "like"
            | "ilike"
            | "is"
            | "case"
            | "when"
            | "then"
            | "else"
            | "end"
            | "select"
            | "from"
            | "where"
            | "values"
            | "over"
            | "as"
            | "with"
            | "distinct"
            | "interval"
            | "on"
            | "by"
            | "having"
            | "union"
            | "asc"
            | "desc"
            | "xor"
            | "div"
            | "mod"
            | "regexp"
            | "rlike"
            | "escape"
            | "collate"
            | "using"
            | "filter"
            | "within"
            | "order"
            | "group"
            | "limit"
    )
}

/// Words that keep an identifier quoted: syntax words plus the value words.
fn keeps_quotes(word: &str) -> bool {
    is_structural_kw(word)
        || matches!(
            word.to_ascii_lowercase().as_str(),
            "null" | "true" | "false" | "default"
        )
}

/// Words at the top level of a sequence that make AND / OR analysis unsafe:
/// they either use AND themselves (BETWEEN) or bind more loosely than a
/// comparison in ways this module does not model.
fn is_blocking_kw(word: &str) -> bool {
    if word.len() > 7 {
        return false;
    }
    matches!(
        word.to_ascii_lowercase().as_str(),
        "between"
            | "case"
            | "when"
            | "then"
            | "else"
            | "end"
            | "select"
            | "with"
            | "from"
            | "where"
            | "as"
            | "xor"
            | "values"
            | "having"
            | "union"
            | "order"
            | "group"
            | "limit"
    )
}

/// Keywords after which a parenthesised single operand is plain grouping,
/// not an argument list.
fn is_grouping_kw(word: &str) -> bool {
    if word.len() > 7 {
        return false;
    }
    matches!(
        word.to_ascii_lowercase().as_str(),
        "and"
            | "or"
            | "not"
            | "like"
            | "ilike"
            | "between"
            | "when"
            | "then"
            | "else"
            | "where"
            | "on"
            | "having"
    )
}

fn is_cmp_sym(s: &str) -> bool {
    matches!(
        s,
        "=" | "<>" | "!=" | "<" | "<=" | ">" | ">=" | "==" | "<=>" | "^="
    )
}

/// Symbols that make a sequence a comparison without being spaced like one.
fn is_match_sym(s: &str) -> bool {
    matches!(
        s,
        "~" | "~*" | "!~" | "!~*" | "~~" | "~~*" | "!~~" | "!~~*" | "@>" | "<@"
    )
}

// ---------------------------------------------------------------------------
// Sequence analysis
// ---------------------------------------------------------------------------

/// Whether the node before `i` can be a left operand.
fn has_left(items: &[Node], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    match &items[i - 1] {
        Node::G(_) => true,
        Node::T(t) => t.kind != Kind::Sym || matches!(t.text.as_str(), ")" | "]" | "?" | "*"),
    }
}

/// Whether `items[i]` is AND / OR used as a binary operator. ClickHouse
/// prints `and(a, b)` as a function, which has no left operand.
fn is_logical(items: &[Node], i: usize) -> bool {
    items
        .get(i)
        .is_some_and(|n| n.is_word("and") || n.is_word("or"))
        && has_left(items, i)
}

#[derive(Debug, Clone, Copy, Default)]
struct Scan {
    has_comma: bool,
    has_and: bool,
    has_or: bool,
    has_cmp: bool,
    blocked: bool,
    ragged: bool,
}

/// End (exclusive) of the dotted name starting at `start`, which must be a
/// name part. Parts and dots must touch.
fn chain_end(get: &dyn Fn(usize) -> Option<bool>, start: usize) -> usize {
    // `get(i)` is Some(true) for a touching name part, Some(false) for a
    // touching dot, None for anything else.
    let mut j = start + 1;
    while get(j) == Some(false) && get(j + 1) == Some(true) {
        j += 2;
    }
    j
}

fn chain_probe(node: Option<&Node>) -> Option<bool> {
    let node = node?;
    if node.sp() {
        return None;
    }
    if node.is_sym(".") {
        Some(false)
    } else if node.is_part() {
        Some(true)
    } else {
        None
    }
}

/// Classify a sequence that is a single operand or a single call.
fn atom_class(items: &[Node]) -> Option<Class> {
    let first = items.first()?;
    match items {
        [Node::T(t)] => {
            return match t.kind {
                Kind::Str | Kind::Num | Kind::Bracket | Kind::Double | Kind::Backtick => {
                    Some(Class::Operand)
                }
                Kind::Word if !is_structural_kw(&t.text) => Some(Class::Operand),
                Kind::Sym if t.text == "?" => Some(Class::Operand),
                _ => None,
            };
        }
        // A prefixed literal: N'x', E'x', DATE'2025-03-01'.
        [Node::T(w), Node::T(s)] if w.kind == Kind::Word && s.kind == Kind::Str && !s.sp => {
            return (!is_structural_kw(&w.text)).then_some(Class::Operand);
        }
        [Node::T(m), Node::G(g)] if m.kind == Kind::Marker && !g.sp => return Some(Class::Call),
        _ => {}
    }
    if !first.is_part() {
        return None;
    }
    let end = chain_end(&|i| chain_probe(items.get(i)), 0);
    // No part of the name may be a syntax word.
    for node in items[..end].iter().step_by(2) {
        if node
            .tok()
            .is_some_and(|t| t.kind == Kind::Word && is_structural_kw(&t.text))
        {
            return None;
        }
    }
    if end == items.len() {
        return Some(Class::Operand);
    }
    if end + 1 == items.len()
        && let Node::G(g) = &items[end]
        && !g.sp
    {
        return Some(Class::Call);
    }
    None
}

// ---------------------------------------------------------------------------
// PostgreSQL types and array literals
// ---------------------------------------------------------------------------

struct TypeRef {
    name: String,
    has_mod: bool,
    is_array: bool,
}

impl TypeRef {
    /// A cast of a column to this type does not change the value in a way a
    /// reader needs: string relabels and exact-to-wider numeric conversions.
    /// Casts such as `::date`, `::integer` or `::numeric(10,2)` truncate or
    /// round, so they stay.
    fn harmless_on_column(&self) -> bool {
        !self.has_mod
            && !self.is_array
            && matches!(
                self.name.as_str(),
                "text"
                    | "varchar"
                    | "character varying"
                    | "bpchar"
                    | "numeric"
                    | "decimal"
                    | "double precision"
                    | "float8"
            )
    }

    fn is_numeric(&self) -> bool {
        matches!(
            self.name.as_str(),
            "integer"
                | "int"
                | "int2"
                | "int4"
                | "int8"
                | "smallint"
                | "bigint"
                | "numeric"
                | "decimal"
                | "real"
                | "double precision"
                | "float4"
                | "float8"
                | "oid"
        )
    }
}

type Slots = [Option<Node>];

fn slot(slots: &Slots, i: usize) -> Option<&Node> {
    slots.get(i).and_then(|s| s.as_ref())
}

fn slot_word(slots: &Slots, i: usize, w: &str) -> bool {
    slot(slots, i).is_some_and(|n| n.is_word(w))
}

fn slot_sym(slots: &Slots, i: usize, s: &str) -> bool {
    slot(slots, i).is_some_and(|n| n.is_sym(s))
}

/// A type modifier: `(10,2)` directly after the type name.
fn typmod_at(slots: &Slots, i: usize) -> bool {
    match slot(slots, i) {
        Some(Node::G(g)) => {
            !g.sp
                && !g.items.is_empty()
                && g.items
                    .iter()
                    .all(|n| n.is_sym(",") || n.tok().is_some_and(|t| t.kind == Kind::Num))
        }
        _ => false,
    }
}

/// Parse the type name that follows `::`, starting at `start`. Returns the
/// index after the type and what was read.
fn parse_type(slots: &Slots, start: usize) -> Option<(usize, TypeRef)> {
    let name_at = |i: usize| -> Option<String> {
        let t = slot(slots, i)?.tok()?;
        matches!(t.kind, Kind::Word | Kind::Double).then(|| t.text.to_ascii_lowercase())
    };
    let mut name = name_at(start)?;
    let mut k = start + 1;
    // Schema-qualified: pg_catalog.text.
    if slot_sym(slots, k, ".")
        && let Some(inner) = name_at(k + 1)
    {
        name = inner;
        k += 2;
    }
    let mut has_mod = false;
    match name.as_str() {
        "timestamp" | "time" => {
            if typmod_at(slots, k) {
                has_mod = true;
                k += 1;
            }
            let with = slot_word(slots, k, "with");
            if (with || slot_word(slots, k, "without"))
                && slot_word(slots, k + 1, "time")
                && slot_word(slots, k + 2, "zone")
            {
                name = format!("{name} {} time zone", if with { "with" } else { "without" });
                k += 3;
            }
        }
        "character" | "bit" => {
            if slot_word(slots, k, "varying") {
                name.push_str(" varying");
                k += 1;
            }
        }
        "national" => {
            if slot_word(slots, k, "character") {
                name.push_str(" character");
                k += 1;
                if slot_word(slots, k, "varying") {
                    name.push_str(" varying");
                    k += 1;
                }
            }
        }
        "double" => {
            if slot_word(slots, k, "precision") {
                name.push_str(" precision");
                k += 1;
            }
        }
        "interval" => {
            // Optional field restriction: interval day to second.
            while slot(slots, k).is_some_and(|n| {
                n.is_any_word(&["year", "month", "day", "hour", "minute", "second", "to"])
            }) {
                k += 1;
            }
        }
        _ => {}
    }
    if typmod_at(slots, k) {
        has_mod = true;
        k += 1;
    }
    let mut is_array = false;
    loop {
        if slot_sym(slots, k, "[") && slot_sym(slots, k + 1, "]") {
            k += 2;
        } else if slot_sym(slots, k, "[")
            && slot(slots, k + 1)
                .and_then(|n| n.tok())
                .is_some_and(|t| t.kind == Kind::Num)
            && slot_sym(slots, k + 2, "]")
        {
            k += 3;
        } else {
            break;
        }
        is_array = true;
    }
    Some((
        k,
        TypeRef {
            name,
            has_mod,
            is_array,
        },
    ))
}

fn is_plain_number(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    let mut parts = digits.splitn(2, '.');
    let whole = parts.next().unwrap_or("");
    let frac = parts.next();
    !whole.is_empty()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

/// The elements of a simple PostgreSQL array literal `'{a,b,c}'`, ready to
/// print in an IN list. None when the literal is anything but a flat list of
/// plain values: nested braces, quoted elements, escapes, NULL, padding.
fn array_elements(literal: &str, numeric: bool) -> Option<Vec<Tok>> {
    let inner = literal.strip_prefix("'{")?.strip_suffix("}'")?;
    if inner.is_empty() || inner.contains(['{', '}', '"', '\\', '\'']) {
        return None;
    }
    let mut out = Vec::new();
    for elem in inner.split(',') {
        if elem.is_empty() || elem.eq_ignore_ascii_case("null") || elem.trim() != elem {
            return None;
        }
        if numeric {
            if !is_plain_number(elem) {
                return None;
            }
            out.push(Tok::new(Kind::Num, elem, false));
        } else {
            out.push(Tok::new(Kind::Str, format!("'{elem}'"), false));
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// The cleaner
// ---------------------------------------------------------------------------

/// What a plan-internal name is replaced with.
#[derive(Clone)]
enum Sub {
    /// Leave the name: no usable value.
    Keep,
    /// A cleaned expression.
    Expr(Vec<Node>),
    /// A description in words, inserted as written.
    Desc(String),
}

struct Cleaner<'a> {
    dialect: Option<PlanDialect>,
    names: Option<&'a ExpressionNames>,
    subs: HashMap<String, Sub>,
    /// Leave every parenthesis alone. Set for ClickHouse (and unknown) text
    /// that contains square brackets: ClickHouse prints half-open ranges such
    /// as `(-Inf, 200]`, where parentheses do not pair up.
    keep_parens: bool,
}

impl<'a> Cleaner<'a> {
    fn new(dialect: Option<PlanDialect>, names: Option<&'a ExpressionNames>) -> Self {
        Cleaner {
            dialect,
            names,
            subs: HashMap::new(),
            keep_parens: false,
        }
    }

    fn is(&self, d: PlanDialect) -> bool {
        self.dialect == Some(d)
    }

    /// Engines (and unknown text) where `not(..)` may be a function.
    fn fn_style(&self) -> bool {
        matches!(
            self.dialect,
            None | Some(PlanDialect::ClickHouse | PlanDialect::DuckDb)
        )
    }

    fn mysql(&self) -> bool {
        self.is(PlanDialect::MySql) || self.is(PlanDialect::MariaDb)
    }

    fn clean(&mut self, src: &str) -> Vec<Node> {
        let toks = tokenize(src, self.dialect);
        self.keep_parens = matches!(self.dialect, None | Some(PlanDialect::ClickHouse))
            && toks.iter().any(|t| t.is_sym("[") || t.is_sym("]"));
        let (tree, too_deep) = build_tree(toks);
        // Removing a level would bring deeper ones into range on the next
        // run, so text nested this deeply keeps every parenthesis.
        self.keep_parens |= too_deep;
        self.simplify(tree, true)
    }

    /// Clean one level: children first, then this level's rewrites, then the
    /// parentheses of this level's groups.
    fn simplify(&mut self, items: Vec<Node>, top: bool) -> Vec<Node> {
        let mut items: Vec<Node> = items
            .into_iter()
            .map(|node| match node {
                Node::G(mut g) => {
                    g.items = self.simplify(std::mem::take(&mut g.items), false);
                    g.info = self.info_of(&g.items);
                    Node::G(g)
                }
                t => t,
            })
            .collect();
        if top && self.is(PlanDialect::SqlServer) {
            items = seek_keys(items);
        }
        items = self.rewrite_names(items);
        if matches!(
            self.dialect,
            Some(PlanDialect::Postgres | PlanDialect::DuckDb)
        ) {
            items = self.rewrite_pg(items);
        }
        if self.names.is_some() {
            items = self.substitute(items);
        }
        self.uppercase_keywords(&mut items);
        if !self.keep_parens {
            items = self.remove_parens(items);
        }
        items
    }

    fn scan(&self, items: &[Node]) -> Scan {
        let pg = self.is(PlanDialect::Postgres);
        let mut s = Scan::default();
        for (i, node) in items.iter().enumerate() {
            let Node::T(t) = node else { continue };
            match t.kind {
                Kind::Raw => s.blocked = true,
                Kind::Sym => match t.text.as_str() {
                    "," => s.has_comma = true,
                    "(" | ")" => {
                        s.ragged = true;
                        s.blocked = true;
                    }
                    ";" | ":=" => s.blocked = true,
                    // A lambda arrow outside PostgreSQL (where it is a JSON operator).
                    "->" if !pg => s.blocked = true,
                    // MySQL can read these as OR / AND.
                    "||" | "&&" if self.mysql() || self.dialect.is_none() => s.blocked = true,
                    x if is_cmp_sym(x) || is_match_sym(x) => s.has_cmp = true,
                    _ => {}
                },
                Kind::Word => {
                    if is_logical(items, i) {
                        if t.is_word("and") {
                            s.has_and = true;
                        } else {
                            s.has_or = true;
                        }
                    } else if is_blocking_kw(&t.text) {
                        s.blocked = true;
                    } else if node
                        .is_any_word(&["is", "in", "like", "ilike", "regexp", "rlike", "glob"])
                    {
                        s.has_cmp = true;
                    }
                }
                _ => {}
            }
        }
        s
    }

    fn info_of(&self, items: &[Node]) -> Info {
        let s = self.scan(items);
        let first = items.first();
        let subquery = first.is_some_and(|n| n.is_any_word(&["select", "with", "values"]));
        let starts_not = first.is_some_and(|n| n.is_word("not"));
        let class = if items.is_empty() || s.blocked || s.has_comma || starts_not {
            Class::Other
        } else if s.has_and && s.has_or {
            Class::Mixed
        } else if s.has_and {
            Class::And
        } else if s.has_or {
            Class::Or
        } else if let Some(c) = atom_class(items) {
            c
        } else if s.has_cmp {
            Class::Tight
        } else {
            Class::Other
        };
        Info {
            class,
            has_comma: s.has_comma,
            subquery,
            ragged: s.ragged,
            empty: items.is_empty(),
        }
    }

    // -- names --------------------------------------------------------------

    /// Remove quotes an identifier does not need.
    fn unquote(&self, t: &mut Tok) {
        let Some(dialect) = self.dialect else { return };
        let mssql = dialect == PlanDialect::SqlServer;
        let ok = match t.kind {
            Kind::Bracket => mssql,
            Kind::Backtick => true,
            Kind::Double => true,
            _ => false,
        };
        if !ok || t.text.len() < 3 {
            return;
        }
        let inner = &t.text[1..t.text.len() - 1];
        let mut chars = inner.chars();
        let Some(first) = chars.next() else { return };
        let plain = if dialect == PlanDialect::Oracle && t.kind == Kind::Double {
            // Oracle folds unquoted names to upper case, so only an
            // upper-case name means the same thing without its quotes.
            (first.is_ascii_uppercase() || first == '_')
                && chars.all(|c| {
                    c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '$' || c == '#'
                })
        } else if mssql {
            (first.is_ascii_alphabetic() || matches!(first, '_' | '@' | '#'))
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '@' | '#' | '$'))
        } else {
            (first.is_ascii_alphabetic() || first == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        };
        if plain && !keeps_quotes(inner) {
            t.text = inner.to_string();
            t.kind = Kind::Word;
        }
    }

    /// Shorten qualified names, drop the SQL Server `as [alias].[col]`
    /// repetition, unquote identifiers, and unwrap `Scalar Operator(..)` and
    /// `<cache>(..)`.
    fn rewrite_names(&self, items: Vec<Node>) -> Vec<Node> {
        let mssql = self.is(PlanDialect::SqlServer);
        let mysql = self.mysql();
        let n = items.len();
        let mut slots: Vec<Option<Node>> = items.into_iter().map(Some).collect();
        let mut out: Vec<Node> = Vec::with_capacity(n);
        let is_group = |slots: &Slots, i: usize| matches!(slot(slots, i), Some(Node::G(_)));
        let mut i = 0;
        while i < n {
            // Wrappers: drop the label, keep the group for the parenthesis rules.
            let mut skip = 0;
            if mssql {
                if slot_word(&slots, i, "scalar")
                    && slot_word(&slots, i + 1, "operator")
                    && is_group(&slots, i + 2)
                {
                    skip = 2;
                } else if slot_word(&slots, i, "scalaroperator") && is_group(&slots, i + 1) {
                    skip = 1;
                }
            }
            if mysql
                && slot(&slots, i)
                    .and_then(|n| n.tok())
                    .is_some_and(|t| t.kind == Kind::Marker && t.text == "<cache>")
                && is_group(&slots, i + 1)
            {
                skip = 1;
            }
            if skip > 0 {
                let sp = slot(&slots, i).is_some_and(|n| n.sp());
                if let Some(g) = slots[i + skip].as_mut() {
                    g.set_sp(sp);
                }
                i += skip;
                continue;
            }

            if slot(&slots, i).is_some_and(|n| n.is_part()) {
                let end = chain_end(&|k| chain_probe(slot(&slots, k)), i);
                let parts = (end - i).div_ceil(2);
                let bracketed = |from: usize, to: usize| {
                    (from..to).any(|k| {
                        slot(&slots, k)
                            .and_then(|n| n.tok())
                            .is_some_and(|t| t.kind == Kind::Bracket)
                    })
                };
                let call_follows = matches!(slot(&slots, end), Some(Node::G(g)) if !g.sp);
                let (mut from, mut to) = (i, end);
                if mssql && bracketed(i, end) {
                    let alias_start = end + 1;
                    if parts >= 2
                        && slot_word(&slots, end, "as")
                        && slot(&slots, alias_start)
                            .and_then(|n| n.tok())
                            .is_some_and(|t| t.kind == Kind::Bracket)
                    {
                        // [db].[schema].[table].[col] as [alias].[col]
                        let alias_end = chain_end(&|k| chain_probe(slot(&slots, k)), alias_start);
                        let alias_parts = (alias_end - alias_start).div_ceil(2);
                        to = alias_end;
                        from = alias_end - (alias_parts.min(2) * 2 - 1);
                    } else if parts >= 3 && !call_follows {
                        from = end - 3;
                    }
                } else if mysql && parts == 3 && !call_follows {
                    // schema.table.column
                    from = end - 3;
                }
                let sp = slot(&slots, i).is_some_and(|n| n.sp());
                for (k, slot) in slots.iter_mut().enumerate().take(to).skip(from) {
                    let Some(mut node) = slot.take() else {
                        continue;
                    };
                    if k == from {
                        node.set_sp(sp);
                    }
                    if let Node::T(t) = &mut node {
                        self.unquote(t);
                    }
                    out.push(node);
                }
                i = to;
                continue;
            }

            if let Some(node) = slots[i].take() {
                out.push(node);
            }
            i += 1;
        }
        out
    }

    // -- PostgreSQL ---------------------------------------------------------

    /// Remove redundant casts, turn `= ANY ('{..}')` into an IN list and
    /// spell out the LIKE operators.
    fn rewrite_pg(&self, items: Vec<Node>) -> Vec<Node> {
        let pg = self.is(PlanDialect::Postgres);
        let n = items.len();
        let mut slots: Vec<Option<Node>> = items.into_iter().map(Some).collect();
        let mut out: Vec<Node> = Vec::with_capacity(n);
        let mut space_next = false;
        let mut i = 0;
        while i < n {
            if pg
                && slot_sym(&slots, i, "::")
                && let Some((end, ty)) = parse_type(&slots, i + 1)
                && strip_cast(&mut out, &ty)
            {
                i = end;
                continue;
            }
            let Some(mut node) = slots[i].take() else {
                i += 1;
                continue;
            };
            i += 1;
            if std::mem::take(&mut space_next) {
                node.set_sp(true);
            }
            let like = match node.tok() {
                Some(t) if t.kind == Kind::Sym => match t.text.as_str() {
                    "~~" => Some((false, "LIKE")),
                    "!~~" => Some((true, "LIKE")),
                    "~~*" => Some((false, "ILIKE")),
                    "!~~*" => Some((true, "ILIKE")),
                    _ => None,
                },
                _ => None,
            };
            if let Some((negated, word)) = like {
                if negated {
                    out.push(Node::T(Tok::new(Kind::Word, "NOT", true)));
                }
                out.push(Node::T(Tok::new(Kind::Word, word, true)));
                space_next = true;
                continue;
            }
            if pg
                && let Node::G(g) = &node
                && let Some(list) = self.any_to_in(&out, g)
            {
                let negated = out.last().is_some_and(|n| n.is_word("all"));
                out.truncate(out.len() - 2);
                if negated {
                    out.push(Node::T(Tok::new(Kind::Word, "NOT", true)));
                }
                out.push(Node::T(Tok::new(Kind::Word, "IN", true)));
                out.push(Node::G(list));
                continue;
            }
            out.push(node);
        }
        out
    }

    /// For `= ANY (g)` / `<> ALL (g)` where `g` holds one simple array
    /// literal whose cast was just removed: the equivalent IN list.
    fn any_to_in(&self, out: &[Node], g: &Group) -> Option<Group> {
        let [Node::T(lit)] = g.items.as_slice() else {
            return None;
        };
        if lit.kind != Kind::Str || lit.note == 0 || out.len() < 2 {
            return None;
        }
        let quantifier = &out[out.len() - 1];
        let op = &out[out.len() - 2];
        let matches = (quantifier.is_word("any") && op.is_sym("="))
            || (quantifier.is_word("all") && (op.is_sym("<>") || op.is_sym("!=")));
        if !matches {
            return None;
        }
        let elems = array_elements(&lit.text, lit.note == 1)?;
        let mut items = Vec::with_capacity(elems.len() * 2);
        for (k, e) in elems.into_iter().enumerate() {
            if k > 0 {
                items.push(Node::T(Tok::new(Kind::Sym, ",", false)));
            }
            items.push(Node::T(e));
        }
        let info = self.info_of(&items);
        Some(Group {
            sp: true,
            items,
            info,
            synthetic: false,
        })
    }

    // -- name substitution --------------------------------------------------

    fn resolve(&mut self, key: &str, value: &str) -> Sub {
        if let Some(sub) = self.subs.get(key) {
            return sub.clone();
        }
        let v = value.trim();
        let sub = if v.is_empty() {
            Sub::Keep
        } else if is_description(v) {
            Sub::Desc(v.to_string())
        } else {
            // Cleaned without names, so a value cannot pull in other names.
            let nodes = Cleaner::new(self.dialect, None).clean(v);
            if nodes.is_empty() || render(&nodes).chars().count() > MAX_NAME_CHARS {
                Sub::Keep
            } else {
                Sub::Expr(nodes)
            }
        };
        self.subs.insert(key.to_string(), sub.clone());
        sub
    }

    /// Replace whole-token plan-internal names with what they stand for. An
    /// expression goes in as a parenthesised group; the parenthesis rules
    /// then drop the parentheses wherever they are not needed.
    fn substitute(&mut self, items: Vec<Node>) -> Vec<Node> {
        let Some(names) = self.names else {
            return items;
        };
        let n = items.len();
        let mut out: Vec<Node> = Vec::with_capacity(n);
        let mut iter = items.into_iter().peekable();
        while let Some(node) = iter.next() {
            let next = iter.peek();
            let value = match &node {
                Node::T(t) if t.kind == Kind::Word => names.get(&t.text),
                _ => None,
            };
            let qualified = out.last().is_some_and(|p| p.is_sym("."))
                || next.is_some_and(|x| !x.sp() && (x.is_sym(".") || matches!(x, Node::G(_))));
            let Some(value) = value.filter(|_| !qualified) else {
                out.push(node);
                continue;
            };
            let sp = node.sp();
            let key = node.tok().map(|t| t.text.clone()).unwrap_or_default();
            match self.resolve(&key, value) {
                Sub::Keep => out.push(node),
                Sub::Expr(nodes) => {
                    let info = self.info_of(&nodes);
                    out.push(Node::G(Group {
                        sp,
                        items: nodes,
                        info,
                        synthetic: true,
                    }));
                }
                Sub::Desc(text) => {
                    let raw = Tok::new(Kind::Raw, text, sp);
                    if is_operatorish(out.last()) || is_operatorish(next) {
                        out.push(Node::G(Group {
                            sp,
                            items: vec![Node::T(Tok { sp: false, ..raw })],
                            info: Info {
                                empty: false,
                                ..NO_INFO
                            },
                            synthetic: false,
                        }));
                    } else {
                        out.push(Node::T(raw));
                    }
                }
            }
        }
        out
    }

    // -- keywords -----------------------------------------------------------

    /// Upper-case the logical keywords MySQL and others print in lower case.
    fn uppercase_keywords(&self, items: &mut [Node]) {
        // ClickHouse and DuckDB print functions named `not(..)`; their names
        // are case-sensitive.
        let fn_style = self.fn_style();
        let mut hits: Vec<usize> = Vec::new();
        for i in 0..items.len() {
            let Some(t) = items[i].tok() else { continue };
            if t.kind != Kind::Word || t.text.len() > 7 {
                continue;
            }
            let prev_word = |back: usize, w: &str| i >= back && items[i - back].is_word(w);
            let hit = match t.text.as_str() {
                "and" | "or" => is_logical(items, i),
                "not" => !(fn_style && matches!(items.get(i + 1), Some(Node::G(g)) if !g.sp)),
                "is" | "in" | "between" | "like" => has_left(items, i),
                "null" => prev_word(1, "is") || (prev_word(1, "not") && prev_word(2, "is")),
                _ => false,
            };
            if hit {
                hits.push(i);
            }
        }
        for i in hits {
            if let Node::T(t) = &mut items[i] {
                t.text.make_ascii_uppercase();
            }
        }
    }

    // -- parentheses --------------------------------------------------------

    /// Whether the parentheses of group `g` at `items[i]` are redundant.
    ///
    /// A pair is removed only in these cases:
    ///
    /// * Whole item: the group is everything between two commas (or the whole
    ///   sequence), optionally followed by ASC / DESC / NULLS, and its content
    ///   has no top-level comma and is not a subquery. Commas bind loosest,
    ///   so nothing outside can reach in.
    /// * Single operand or call: the content is one literal, name or function
    ///   call, and the group is not an argument list (no name or closing
    ///   bracket directly before it) and nothing binds to it from the right
    ///   (`.`, `[`, another group).
    /// * Comparison in a chain: the surrounding sequence is a plain AND / OR
    ///   chain (no BETWEEN, CASE or similar at its level), the group is one
    ///   whole operand of that chain, and its content is a comparison with no
    ///   AND / OR of its own.
    /// * Same operator: an AND group that is a whole operand of a pure AND
    ///   chain, or an OR group in a pure OR chain.
    fn removable(&self, items: &[Node], i: usize, g: &Group, scan: &Scan) -> bool {
        if g.info.empty || g.info.ragged {
            return false;
        }
        let prev = i.checked_sub(1).map(|j| &items[j]);
        let next = items.get(i + 1);
        if next.is_some_and(|x| matches!(x, Node::G(_)) || x.is_sym(".") || x.is_sym("[")) {
            return false;
        }
        let class = g.info.class;

        let prev_edge = prev.is_none_or(|p| p.is_sym(","));
        let next_edge =
            next.is_none_or(|x| x.is_sym(",") || x.is_any_word(&["asc", "desc", "nulls"]));
        if prev_edge && next_edge && !g.info.has_comma && !g.info.subquery {
            return true;
        }

        if matches!(class, Class::Operand | Class::Call) {
            let prev_ok = match prev {
                None => true,
                Some(Node::G(_)) => false,
                Some(Node::T(p)) => match p.kind {
                    Kind::Sym => !matches!(p.text.as_str(), "." | "]" | ")" | "::" | ":" | "?"),
                    // `not(x)` is a function call in ClickHouse and DuckDB.
                    Kind::Word if p.text == "not" && !g.sp && self.fn_style() => false,
                    Kind::Word => g.synthetic || is_grouping_kw(&p.text),
                    _ => false,
                },
            };
            if prev_ok {
                return true;
            }
        }

        let chain = !scan.blocked && !scan.has_comma && (scan.has_and || scan.has_or);
        let prev_logical = prev.is_none() || is_logical(items, i.wrapping_sub(1));
        let next_logical = next.is_none() || is_logical(items, i + 1);
        if chain && prev_logical && next_logical {
            return match class {
                Class::Tight => true,
                Class::And => scan.has_and && !scan.has_or,
                Class::Or => scan.has_or && !scan.has_and,
                _ => false,
            };
        }
        false
    }

    fn remove_parens(&self, items: Vec<Node>) -> Vec<Node> {
        if !items.iter().any(|n| matches!(n, Node::G(_))) {
            return items;
        }
        let scan = self.scan(&items);
        let remove: Vec<bool> = items
            .iter()
            .enumerate()
            .map(|(i, node)| match node {
                Node::G(g) => self.removable(&items, i, g, &scan),
                Node::T(_) => false,
            })
            .collect();
        let mut out: Vec<Node> = Vec::with_capacity(items.len());
        let mut after_unwrap = false;
        for (i, node) in items.into_iter().enumerate() {
            match node {
                Node::G(g) if remove[i] => {
                    splice(&mut out, g);
                    after_unwrap = true;
                }
                mut other => {
                    // The token after `)` now follows the group's last token.
                    if after_unwrap && other.is_atomish() {
                        other.set_sp(true);
                    }
                    after_unwrap = false;
                    out.push(other);
                }
            }
        }
        out
    }
}

/// Append the content of `g` to `out` without its parentheses, keeping the
/// content apart from the token before it.
fn splice(out: &mut Vec<Node>, g: Group) {
    let prev_atom = out.last().is_some_and(|p| p.is_atomish());
    for (k, mut child) in g.items.into_iter().enumerate() {
        if k == 0 {
            child.set_sp(g.sp || prev_atom);
        }
        out.push(child);
    }
}

/// Remove the cast `::ty` that follows the last node of `out`, when that node
/// is a literal, or a plain column and the cast is harmless. Returns whether
/// the cast was removed (the caller then skips the type tokens).
fn strip_cast(out: &mut Vec<Node>, ty: &TypeRef) -> bool {
    #[derive(PartialEq)]
    enum Operand {
        Literal,
        Column,
        Neither,
    }
    fn operand(t: &Tok) -> Operand {
        match t.kind {
            Kind::Str | Kind::Num => Operand::Literal,
            Kind::Word if t.text.starts_with('$') => Operand::Neither,
            Kind::Word
                if ["null", "true", "false"]
                    .iter()
                    .any(|w| t.text.eq_ignore_ascii_case(w)) =>
            {
                Operand::Literal
            }
            Kind::Word if is_structural_kw(&t.text) => Operand::Neither,
            Kind::Word | Kind::Double | Kind::Backtick | Kind::Bracket => Operand::Column,
            _ => Operand::Neither,
        }
    }
    let strip = |kind: Operand| match kind {
        // `'1 mon'::interval` without its cast reads as a plain string.
        Operand::Literal => ty.name != "interval",
        Operand::Column => ty.harmless_on_column(),
        Operand::Neither => false,
    };
    let note = if !ty.is_array {
        0
    } else if ty.is_numeric() {
        1
    } else {
        2
    };
    let len = out.len();
    let before = len.checked_sub(2).map(|k| &out[k]);
    match out.last() {
        None => false,
        Some(Node::T(t)) => {
            // `x::date::text`: the operand of the second cast is a type name.
            if before.is_some_and(|b| b.is_sym("::")) || !strip(operand(t)) {
                return false;
            }
            if let Some(Node::T(t)) = out.last_mut()
                && t.kind == Kind::Str
            {
                t.note = note;
            }
            true
        }
        Some(Node::G(g)) => {
            if g.synthetic || g.info.class != Class::Operand {
                return false;
            }
            // `f(x)::date` casts the call, not `x`.
            let grouping = match before {
                None => true,
                Some(Node::G(_)) => false,
                Some(Node::T(p)) => match p.kind {
                    Kind::Sym => !matches!(p.text.as_str(), "." | "]" | ")" | "::"),
                    Kind::Word => is_grouping_kw(&p.text),
                    _ => false,
                },
            };
            let kind = match g.items.last() {
                Some(Node::T(t)) => operand(t),
                _ => Operand::Neither,
            };
            if !grouping || !strip(kind) {
                return false;
            }
            if let Some(Node::G(mut g)) = out.pop() {
                if let Some(Node::T(t)) = g.items.last_mut()
                    && t.kind == Kind::Str
                {
                    t.note = note;
                }
                splice(out, g);
            }
            true
        }
    }
}

/// A value that is words, not an expression: at least two words made only of
/// letters, digits and underscores.
fn is_description(v: &str) -> bool {
    v.split_whitespace().count() >= 2
        && v.chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c.is_whitespace())
}

/// An operator next to a substituted description, which then needs
/// parentheses to read as one operand.
fn is_operatorish(node: Option<&Node>) -> bool {
    let Some(Node::T(t)) = node else { return false };
    match t.kind {
        Kind::Sym => !matches!(t.text.as_str(), "," | "(" | ")" | ";"),
        Kind::Word => ["and", "or", "not", "like", "ilike", "in", "is", "between"]
            .iter()
            .any(|w| t.text.eq_ignore_ascii_case(w)),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// SQL Server seek keys
// ---------------------------------------------------------------------------

/// Rewrite SQL Server seek-predicate text
/// (`Seek Keys[1]: Prefix: a = Scalar Operator(x), Start: b >= Scalar Operator(y)`)
/// as `a = Scalar Operator(x) AND b >= Scalar Operator(y)`. The multi-column
/// form `Prefix: a, b = x, y` becomes `a = x AND b = y`. Text that is not
/// exactly this shape, or that lists more than one key range, is returned
/// unchanged.
fn seek_keys(items: Vec<Node>) -> Vec<Node> {
    let Some(ranges) = seek_ranges(&items) else {
        return items;
    };
    let mut slots: Vec<Option<Node>> = items.into_iter().map(Some).collect();
    let mut out: Vec<Node> = Vec::with_capacity(slots.len());
    let and = |out: &mut Vec<Node>| {
        if !out.is_empty() {
            out.push(Node::T(Tok::new(Kind::Word, "AND", true)));
        }
    };
    for (from, to) in ranges {
        let top =
            |s: &str| -> Vec<usize> { (from..to).filter(|&k| slot_sym(&slots, k, s)).collect() };
        let eq = top("=");
        let commas = top(",");
        let left = commas
            .iter()
            .filter(|&&c| eq.first().is_some_and(|&e| c < e))
            .count();
        if eq.len() == 1 && left > 0 && left * 2 == commas.len() {
            // cols = vals, pairwise.
            let e = eq[0];
            let mut bounds_l = vec![from];
            let mut bounds_r = vec![e + 1];
            for &c in &commas {
                if c < e {
                    bounds_l.push(c + 1);
                } else {
                    bounds_r.push(c + 1);
                }
            }
            let ends_l: Vec<usize> = commas
                .iter()
                .copied()
                .filter(|&c| c < e)
                .chain([e])
                .collect();
            let ends_r: Vec<usize> = commas
                .iter()
                .copied()
                .filter(|&c| c > e)
                .chain([to])
                .collect();
            let eq_tok = slots[e].take();
            for k in 0..bounds_l.len() {
                and(&mut out);
                let start = out.len();
                out.extend((bounds_l[k]..ends_l[k]).filter_map(|x| slots[x].take()));
                if let Some(eq_tok) = eq_tok.clone() {
                    out.push(eq_tok);
                }
                out.extend((bounds_r[k]..ends_r[k]).filter_map(|x| slots[x].take()));
                if let Some(first) = out.get_mut(start) {
                    first.set_sp(true);
                }
            }
        } else {
            and(&mut out);
            let start = out.len();
            out.extend((from..to).filter_map(|x| slots[x].take()));
            if let Some(first) = out.get_mut(start) {
                first.set_sp(true);
            }
        }
    }
    out
}

/// The token ranges of the individual seek conditions, or None when the text
/// is not seek-predicate text this module can restructure safely.
fn seek_ranges(items: &[Node]) -> Option<Vec<(usize, usize)>> {
    let header_len = |i: usize| -> Option<usize> {
        if !(items.get(i)?.is_word("seek") && items.get(i + 1)?.is_word("keys")) {
            return None;
        }
        let index = items.get(i + 2)?.tok()?;
        (index.kind == Kind::Bracket && items.get(i + 3)?.is_sym(":")).then_some(4)
    };
    let is_label = |i: usize| {
        items
            .get(i)
            .is_some_and(|n| n.is_any_word(&["prefix", "start", "end"]))
            && items.get(i + 1).is_some_and(|n| n.is_sym(":"))
    };
    let mut i = header_len(0)?;
    let mut ranges = Vec::new();
    let mut current: Option<usize> = None;
    let close = |current: &mut Option<usize>, at: usize, ranges: &mut Vec<(usize, usize)>| {
        if let Some(from) = current.take() {
            let mut to = at;
            while to > from && (items[to - 1].is_sym(",") || items[to - 1].is_sym(";")) {
                to -= 1;
            }
            if to > from {
                ranges.push((from, to));
            }
        }
    };
    while i < items.len() {
        if header_len(i).is_some() {
            // A second key range means the ranges are alternatives.
            return None;
        }
        if is_label(i) {
            close(&mut current, i, &mut ranges);
            current = Some(i + 2);
            i += 2;
            continue;
        }
        current?;
        if is_logical(items, i) || items[i].tok().is_some_and(|t| t.kind == Kind::Raw) {
            return None;
        }
        i += 1;
    }
    close(&mut current, items.len(), &mut ranges);
    (!ranges.is_empty()).then_some(ranges)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Prev {
    Start,
    Open,
    Comma,
    Cmp,
    Other,
}

struct Printer {
    out: String,
    prev: Prev,
}

impl Printer {
    /// Print one leaf. Spacing: none after `(` or before `)` and `,`; one
    /// space after a comma and on both sides of a comparison operator;
    /// everywhere else, one space where the source had whitespace.
    fn leaf(&mut self, text: &str, sp: bool, kind: Prev) {
        let close_or_comma = text == ")" && kind == Prev::Other || kind == Prev::Comma;
        let space = match self.prev {
            Prev::Start | Prev::Open => false,
            _ if close_or_comma => false,
            Prev::Comma | Prev::Cmp => true,
            Prev::Other => kind == Prev::Cmp || sp,
        };
        if space {
            self.out.push(' ');
        }
        self.out.push_str(text);
        self.prev = kind;
    }

    fn nodes(&mut self, items: &[Node]) {
        for node in items {
            match node {
                Node::T(t) => {
                    let kind = if t.kind != Kind::Sym {
                        Prev::Other
                    } else if t.text == "," {
                        Prev::Comma
                    } else if is_cmp_sym(&t.text) {
                        Prev::Cmp
                    } else {
                        Prev::Other
                    };
                    // An unpaired `(` is printed as written, but still
                    // suppresses the space after it.
                    self.leaf(&t.text, t.sp, kind);
                    if t.is_sym("(") {
                        self.prev = Prev::Open;
                    }
                }
                Node::G(g) => {
                    self.leaf("(", g.sp, Prev::Other);
                    self.prev = Prev::Open;
                    self.nodes(&g.items);
                    self.leaf(")", false, Prev::Other);
                }
            }
        }
    }
}

fn render(items: &[Node]) -> String {
    let mut p = Printer {
        out: String::new(),
        prev: Prev::Start,
    };
    p.nodes(items);
    p.out
}

/// A SQL Server bitmap filter found in a scan's predicate.
///
/// `PROBE(Opt_Bitmap1003, o.CustomerID)` is not a condition from the query:
/// a join higher in the plan built a bitmap of the join keys on its other
/// input and pushed it down to this read, so rows that cannot join are
/// dropped as they are read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BitmapProbe {
    /// The columns tested against the bitmap, as the plan writes them.
    pub columns: Vec<String>,
    /// What is left of the predicate without the bitmap tests, if anything.
    pub rest: Option<String>,
}

/// Split the bitmap tests out of a predicate cleaned by
/// [`readable_predicate`]. `None` when it has none.
pub(crate) fn bitmap_probe(text: &str) -> Option<BitmapProbe> {
    let mut columns: Vec<String> = Vec::new();
    let mut rest: Vec<&str> = Vec::new();
    for term in split_top_level(text, " AND ") {
        let trimmed = term.trim();
        let inner = trimmed
            .get(..6)
            .filter(|head| head.eq_ignore_ascii_case("PROBE("))
            .and_then(|_| trimmed[6..].strip_suffix(')'));
        match inner {
            Some(inner) => {
                // The first argument names the bitmap; quoted arguments are
                // flags such as N'[IN ROW]'.
                for argument in split_top_level(inner, ",").into_iter().skip(1) {
                    let argument = argument.trim();
                    if argument.is_empty() || argument.contains('\'') {
                        continue;
                    }
                    if !columns.iter().any(|known| known == argument) {
                        columns.push(argument.to_string());
                    }
                }
            }
            None => rest.push(trimmed),
        }
    }
    if columns.is_empty() {
        return None;
    }
    Some(BitmapProbe {
        columns,
        rest: (!rest.is_empty()).then(|| rest.join(" AND ")),
    })
}

/// Split `text` on `separator` (ASCII, matched without regard to case) where
/// it is outside parentheses and string literals.
fn split_top_level<'a>(text: &'a str, separator: &str) -> Vec<&'a str> {
    let bytes = text.as_bytes();
    let wanted = separator.as_bytes();
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut start = 0usize;
    let mut at = 0usize;
    while at < bytes.len() {
        let byte = bytes[at];
        if quoted {
            quoted = byte != b'\'';
        } else if byte == b'\'' {
            quoted = true;
        } else if byte == b'(' {
            depth += 1;
        } else if byte == b')' {
            depth = depth.saturating_sub(1);
        } else if depth == 0
            && bytes.len() - at >= wanted.len()
            && bytes[at..at + wanted.len()].eq_ignore_ascii_case(wanted)
        {
            parts.push(&text[start..at]);
            at += wanted.len();
            start = at;
            continue;
        }
        at += 1;
    }
    parts.push(&text[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    const MSSQL: Option<PlanDialect> = Some(PlanDialect::SqlServer);
    const PG: Option<PlanDialect> = Some(PlanDialect::Postgres);
    const MYSQL: Option<PlanDialect> = Some(PlanDialect::MySql);
    const MARIA: Option<PlanDialect> = Some(PlanDialect::MariaDb);
    const ORACLE: Option<PlanDialect> = Some(PlanDialect::Oracle);
    const SQLITE: Option<PlanDialect> = Some(PlanDialect::Sqlite);
    const DUCK: Option<PlanDialect> = Some(PlanDialect::DuckDb);
    const CH: Option<PlanDialect> = Some(PlanDialect::ClickHouse);

    const ALL: [Option<PlanDialect>; 9] = [None, MSSQL, PG, MYSQL, MARIA, ORACLE, SQLITE, DUCK, CH];

    type Case = (Option<PlanDialect>, &'static str, &'static str);

    /// (dialect, what the plan printed, what a person should read).
    const CASES: &[Case] = &[
        // ---- SQL Server: showplan ScalarString and predicate text ----
        (
            MSSQL,
            "[Shop].[dbo].[Orders].[OrderDate] as [o].[OrderDate]>=[@since]",
            "o.OrderDate >= @since",
        ),
        (MSSQL, "[Shop].[dbo].[Orders].[OrderID]", "Orders.OrderID"),
        (MSSQL, "[dbo].[Orders].[OrderID]", "Orders.OrderID"),
        (MSSQL, "[o].[OrderID]", "o.OrderID"),
        (MSSQL, "[@email]", "@email"),
        (MSSQL, "[Expr1004]", "Expr1004"),
        (
            MSSQL,
            "[Shop].[dbo].[Orders].[Status] as [o].[Status] = [Shop].[dbo].[Orders].[Status] as [o].[Status]",
            "o.Status = o.Status",
        ),
        (
            MSSQL,
            "CONVERT_IMPLICIT(nvarchar(200),[Shop].[dbo].[Customers].[Email] as [c].[Email],0)=[@email]",
            "CONVERT_IMPLICIT(nvarchar(200), c.Email, 0) = @email",
        ),
        (
            MSSQL,
            "CONVERT_IMPLICIT(nvarchar(200),[c].[Email],0)=[@email]",
            "CONVERT_IMPLICIT(nvarchar(200), c.Email, 0) = @email",
        ),
        (
            MSSQL,
            "CONVERT_IMPLICIT(int,[Expr1006],0)",
            "CONVERT_IMPLICIT(int, Expr1006, 0)",
        ),
        (
            MSSQL,
            "PROBE([Opt_Bitmap1003],[Shop].[dbo].[Orders].[CustomerID] as [o].[CustomerID])",
            "PROBE(Opt_Bitmap1003, o.CustomerID)",
        ),
        (
            MSSQL,
            "SUM([Shop].[dbo].[Orders].[Total] as [o].[Total])",
            "SUM(o.Total)",
        ),
        (MSSQL, "COUNT(*)", "COUNT(*)"),
        (MSSQL, "(20)", "20"),
        (MSSQL, "'2026-03-01'", "'2026-03-01'"),
        (MSSQL, "Scalar Operator([@since])", "@since"),
        (MSSQL, "ScalarOperator((1))", "1"),
        (
            MSSQL,
            "[Shop].[dbo].[Order Details].[Unit Price] as [od].[Unit Price]>(10.00)",
            "od.[Unit Price] > 10.00",
        ),
        (
            MSSQL,
            "[Shop].[dbo].[Order Details].[Qty]",
            "[Order Details].Qty",
        ),
        (
            MSSQL,
            "Seek Keys[1]: Prefix: [Shop].[dbo].[Orders].CustomerID = Scalar Operator([c].[CustomerID])",
            "Orders.CustomerID = c.CustomerID",
        ),
        (
            MSSQL,
            "Seek Keys[1]: Start: [Shop].[dbo].[Orders].OrderDate >= Scalar Operator('2026-03-01')",
            "Orders.OrderDate >= '2026-03-01'",
        ),
        (
            MSSQL,
            "Seek Keys[1]: Prefix: [Shop].[dbo].[Orders].CustomerID = Scalar Operator([c].[CustomerID]), Start: [Shop].[dbo].[Orders].OrderDate >= Scalar Operator('2026-03-01')",
            "Orders.CustomerID = c.CustomerID AND Orders.OrderDate >= '2026-03-01'",
        ),
        (
            MSSQL,
            "Seek Keys[1]: Prefix: [Shop].[dbo].[Orders].CustomerID, [Shop].[dbo].[Orders].Status = Scalar Operator((42)), Scalar Operator(N'paid')",
            "Orders.CustomerID = 42 AND Orders.Status = N'paid'",
        ),
        (
            MSSQL,
            "Seek Keys[1]: Start: [Shop].[dbo].[Orders].OrderDate > Scalar Operator([@from]), End: [Shop].[dbo].[Orders].OrderDate < Scalar Operator(dateadd(day,(1),[@to]))",
            "Orders.OrderDate > @from AND Orders.OrderDate < dateadd(day, 1, @to)",
        ),
        (
            MSSQL,
            "[Shop].[dbo].[Orders].[Total] as [o].[Total]>(500.00) AND [Shop].[dbo].[Orders].[Status] as [o].[Status]=N'paid'",
            "o.Total > 500.00 AND o.Status = N'paid'",
        ),
        (
            MSSQL,
            "[o].[Note] like N'%[x] (a = 1)%'",
            "o.Note LIKE N'%[x] (a = 1)%'",
        ),
        (MSSQL, "[o].[Group]=(1)", "o.[Group] = 1"),
        (
            MSSQL,
            "[Shop].[dbo].[fnTax]([o].[Total])",
            "Shop.dbo.fnTax(o.Total)",
        ),
        // ---- PostgreSQL: Filter, Index Cond, Hash Cond, Join Filter, Sort Key ----
        (PG, "(o.status = 'paid'::text)", "o.status = 'paid'"),
        (PG, "((status)::text = 'paid'::text)", "status = 'paid'"),
        (PG, "(quantity > 4)", "quantity > 4"),
        (PG, "(total > '500'::numeric)", "total > '500'"),
        (
            PG,
            "(total > (InitPlan 5).col1)",
            "total > (InitPlan 5).col1",
        ),
        (PG, "(o.customer_id = c.id)", "o.customer_id = c.id"),
        (
            PG,
            "((id >= 100) AND (id <= 140))",
            "id >= 100 AND id <= 140",
        ),
        (
            PG,
            "(customer_id = ANY ('{1,2,3}'::integer[]))",
            "customer_id IN (1, 2, 3)",
        ),
        (
            PG,
            "((status)::text = ANY ('{paid,shipped,refunded}'::text[]))",
            "status IN ('paid', 'shipped', 'refunded')",
        ),
        (
            PG,
            "(status <> ALL ('{a,b,c}'::text[]))",
            "status NOT IN ('a', 'b', 'c')",
        ),
        (
            PG,
            "(name = ANY ('{\"a b\",c}'::text[]))",
            "name = ANY ('{\"a b\",c}')",
        ),
        (
            PG,
            "(x = ANY ('{1,NULL}'::integer[]))",
            "x = ANY ('{1,NULL}')",
        ),
        (PG, "(tags = '{1,2,3}'::integer[])", "tags = '{1,2,3}'"),
        (PG, "(id = ANY ($1))", "id = ANY ($1)"),
        (
            PG,
            "(placed_at < '2023-01-02 00:00:00+00'::timestamp with time zone)",
            "placed_at < '2023-01-02 00:00:00+00'",
        ),
        (
            PG,
            "(created_at >= '2026-01-01 00:00:00'::timestamp without time zone)",
            "created_at >= '2026-01-01 00:00:00'",
        ),
        (
            PG,
            "((customer_id = ANY ('{1,2,3}'::integer[])) OR (placed_at < '2023-01-02 00:00:00+00'::timestamp with time zone))",
            "customer_id IN (1, 2, 3) OR placed_at < '2023-01-02 00:00:00+00'",
        ),
        (
            PG,
            "(lower(email) = 'user4242@example.com'::text)",
            "lower(email) = 'user4242@example.com'",
        ),
        (
            PG,
            "(sum(((oi.quantity)::numeric * oi.unit_price))) DESC",
            "sum(oi.quantity * oi.unit_price) DESC",
        ),
        (PG, "o.order_date DESC", "o.order_date DESC"),
        (PG, "c.country", "c.country"),
        (
            PG,
            "(date_trunc('month'::text, created_at))",
            "date_trunc('month', created_at)",
        ),
        (PG, "((o.total)::numeric > 100.5)", "o.total > 100.5"),
        (PG, "((a + b))::integer", "(a + b)::integer"),
        (PG, "(lower(x))::date", "lower(x)::date"),
        (PG, "lower(x)::date", "lower(x)::date"),
        (
            PG,
            "((created_at)::date = '2026-01-01'::date)",
            "created_at::date = '2026-01-01'",
        ),
        (
            PG,
            "((price)::numeric(10,2) > 5.00)",
            "price::numeric(10, 2) > 5.00",
        ),
        (
            PG,
            "((code)::character varying(20) = 'X'::character varying(20))",
            "code::character varying(20) = 'X'",
        ),
        (
            PG,
            "((name)::character varying = 'X'::character varying)",
            "name = 'X'",
        ),
        (PG, "(x = '1.5'::double precision)", "x = '1.5'"),
        (PG, "(kind = 'a'::pg_catalog.text)", "kind = 'a'"),
        (PG, "(flags = B'101'::bit varying)", "flags = B'101'"),
        (
            PG,
            "(t < '12:00:00'::time without time zone)",
            "t < '12:00:00'",
        ),
        (
            PG,
            "(t < '12:00:00+00'::time with time zone)",
            "t < '12:00:00+00'",
        ),
        (PG, "(name ~~ 'A%'::text)", "name LIKE 'A%'"),
        (PG, "(name !~~ 'A%'::text)", "name NOT LIKE 'A%'"),
        (
            PG,
            "((name)::text ~~* '%smith%'::text)",
            "name ILIKE '%smith%'",
        ),
        (PG, "(name !~~* 'x_'::text)", "name NOT ILIKE 'x_'"),
        (PG, "(customer_id = $1)", "customer_id = $1"),
        (PG, "(note = '(a = 1)'::text)", "note = '(a = 1)'"),
        (PG, "(note = 'it''s'::text)", "note = 'it''s'"),
        (PG, "(note = E'a\\'b)'::text)", "note = E'a\\'b)'"),
        (
            PG,
            "(((a = 1) OR (b = 2)) AND (c = 3))",
            "(a = 1 OR b = 2) AND c = 3",
        ),
        (
            PG,
            "((a = 1) OR ((b = 2) AND (c = 3)))",
            "a = 1 OR (b = 2 AND c = 3)",
        ),
        (
            PG,
            "(((a = 1) AND (b = 2)) AND ((c = 3) AND (d = 4)))",
            "a = 1 AND b = 2 AND c = 3 AND d = 4",
        ),
        (PG, "(NOT (a = 1))", "NOT (a = 1)"),
        (PG, "(NOT (deleted))", "NOT deleted"),
        (
            PG,
            "((NOT archived) AND (x > 1))",
            "(NOT archived) AND x > 1",
        ),
        (
            PG,
            "((deleted_at IS NULL) AND (qty > 0))",
            "deleted_at IS NULL AND qty > 0",
        ),
        (
            PG,
            "((d >= '2026-01-01'::date) AND (d < ('2026-01-01'::date + '1 mon'::interval)))",
            "d >= '2026-01-01' AND d < ('2026-01-01' + '1 mon'::interval)",
        ),
        (
            PG,
            "((x BETWEEN 1 AND 5) AND (y = 2))",
            "(x BETWEEN 1 AND 5) AND y = 2",
        ),
        (PG, "((a - (b)) > (-1))", "(a - b) > (-1)"),
        (
            PG,
            "((data ->> 'k'::text) = 'v'::text)",
            "(data ->> 'k') = 'v'",
        ),
        (PG, "(a.x = (SubPlan 1))", "a.x = (SubPlan 1)"),
        (
            PG,
            "((\"Order\".\"Id\" = 5) AND (\"select\" > 1))",
            "\"Order\".Id = 5 AND \"select\" > 1",
        ),
        // ---- MySQL: EXPLAIN FORMAT=TREE and JSON attached_condition ----
        (
            MYSQL,
            "((`shop`.`o`.`status` = 'refunded') and (`shop`.`o`.`total` > 1200.00))",
            "o.status = 'refunded' AND o.total > 1200.00",
        ),
        (
            MYSQL,
            "(`shop`.`c`.`id` between 100 and 140)",
            "c.id BETWEEN 100 AND 140",
        ),
        (MYSQL, "(`shop`.`c`.`id` is not null)", "c.id IS NOT NULL"),
        (
            MYSQL,
            "(`shop`.`events`.`happened_at` >= DATE'2025-03-01')",
            "events.happened_at >= DATE'2025-03-01'",
        ),
        (
            MYSQL,
            "(`shop`.`orders`.`placed_at` > TIMESTAMP'2023-01-20 00:00:00')",
            "orders.placed_at > TIMESTAMP'2023-01-20 00:00:00'",
        ),
        (
            MYSQL,
            "((o.`status` = 'refunded') and (o.total > 1200.00))",
            "o.status = 'refunded' AND o.total > 1200.00",
        ),
        (
            MYSQL,
            "(o.created_at >= TIMESTAMP'2026-01-01 00:00:00')",
            "o.created_at >= TIMESTAMP'2026-01-01 00:00:00'",
        ),
        (
            MYSQL,
            "((o.customer_id = c.id) and (o.total > 100))",
            "o.customer_id = c.id AND o.total > 100",
        ),
        (MYSQL, "`o`.`customer_id`", "o.customer_id"),
        (MYSQL, "`shop`.`order items`.`qty`", "`order items`.qty"),
        (
            MYSQL,
            "(`shop`.`o`.`created_at` > <cache>((now() - interval 1 day)))",
            "o.created_at > (now() - interval 1 day)",
        ),
        (
            MYSQL,
            "(`shop`.`o`.`total` > <cache>(500))",
            "o.total > 500",
        ),
        (
            MYSQL,
            "<in_optimizer>(`shop`.`o`.`customer_id`,<exists>(select 1 from `shop`.`customers`))",
            "<in_optimizer>(o.customer_id, <exists>(select 1 from shop.customers))",
        ),
        (
            MYSQL,
            "((`shop`.`o`.`status` = 'a') or ((`shop`.`o`.`status` = 'b') and (`shop`.`o`.`total` > 5)))",
            "o.status = 'a' OR (o.status = 'b' AND o.total > 5)",
        ),
        (MYSQL, "(o.note = 'it\\'s (ok)')", "o.note = 'it\\'s (ok)'"),
        (MYSQL, "(o.name like 'A%')", "o.name LIKE 'A%'"),
        (MYSQL, "(o.status in ('a','b'))", "o.status IN ('a', 'b')"),
        (MYSQL, "(100 <= id <= 140)", "100 <= id <= 140"),
        // ---- MariaDB ----
        (
            MARIA,
            "c.`id` between 100 and 140",
            "c.id BETWEEN 100 AND 140",
        ),
        (
            MARIA,
            "o.`status` = 'refunded' and o.total > 1200",
            "o.status = 'refunded' AND o.total > 1200",
        ),
        (
            MARIA,
            "`events`.happened_at >= '2025-03-01'",
            "events.happened_at >= '2025-03-01'",
        ),
        // ---- Oracle: DBMS_XPLAN predicate information ----
        (
            ORACLE,
            "\"O\".\"CUSTOMER_ID\"=\"C\".\"ID\"",
            "O.CUSTOMER_ID = C.ID",
        ),
        (
            ORACLE,
            "\"C\".\"ID\"=\"O\".\"CUSTOMER_ID\"",
            "C.ID = O.CUSTOMER_ID",
        ),
        (
            ORACLE,
            "(\"O\".\"TOTAL\">1200 AND \"O\".\"STATUS\"='refunded')",
            "O.TOTAL > 1200 AND O.STATUS = 'refunded'",
        ),
        (
            ORACLE,
            "\"O\".\"CREATED_AT\">=TO_DATE(' 2026-01-01 00:00:00', 'syyyy-mm-dd hh24:mi:ss')",
            "O.CREATED_AT >= TO_DATE(' 2026-01-01 00:00:00', 'syyyy-mm-dd hh24:mi:ss')",
        ),
        (
            ORACLE,
            "INTERNAL_FUNCTION(\"O\".\"STATUS\")='paid'",
            "INTERNAL_FUNCTION(O.STATUS) = 'paid'",
        ),
        (ORACLE, "\"O\".\"mixedCase\"=:B1", "O.\"mixedCase\" = :B1"),
        (
            ORACLE,
            "\"E\".\"DEPT_ID\"=\"D\".\"ID\"(+)",
            "E.DEPT_ID = D.ID(+)",
        ),
        (
            ORACLE,
            "(\"O\".\"STATUS\"='a' OR \"O\".\"STATUS\"='b')",
            "O.STATUS = 'a' OR O.STATUS = 'b'",
        ),
        // ---- SQLite ----
        (SQLITE, "(customer_id=?)", "customer_id = ?"),
        (SQLITE, "(rowid<?)", "rowid < ?"),
        (
            SQLITE,
            "(customer_id=? AND placed_at>?)",
            "customer_id = ? AND placed_at > ?",
        ),
        (SQLITE, "(<expr>=?)", "<expr> = ?"),
        // ---- DuckDB ----
        (
            DUCK,
            "status='paid' AND status IS NOT NULL",
            "status = 'paid' AND status IS NOT NULL",
        ),
        (
            DUCK,
            "customer_id>=100 AND customer_id<=200 AND customer_id IS NOT NULL",
            "customer_id >= 100 AND customer_id <= 200 AND customer_id IS NOT NULL",
        ),
        (DUCK, "customer_id = id", "customer_id = id"),
        (
            DUCK,
            "__internal_compress_string_uhugeint(#0)",
            "__internal_compress_string_uhugeint(#0)",
        ),
        (DUCK, "(name ~~ 'A%')", "name LIKE 'A%'"),
        // ---- ClickHouse ----
        (
            CH,
            "and(greater(__table1.total, 800_UInt16), greaterOrEquals(__table1.customer_id, 100_UInt8), lessOrEquals(__table1.customer_id, 200_UInt8))",
            "and(greater(__table1.total, 800_UInt16), greaterOrEquals(__table1.customer_id, 100_UInt8), lessOrEquals(__table1.customer_id, 200_UInt8))",
        ),
        (
            CH,
            "and((customer_id in (-Inf, 200]), (customer_id in [100, +Inf)))",
            "and((customer_id IN (-Inf, 200]), (customer_id IN [100, +Inf)))",
        ),
        (CH, "(total in ('800', +Inf))", "total IN ('800', +Inf)"),
        (
            CH,
            "(toYYYYMM(placed_at) in [202306, +Inf))",
            "(toYYYYMM(placed_at) IN [202306, +Inf))",
        ),
        (CH, "true", "true"),
        (
            CH,
            "not(equals(status, 'paid'))",
            "not(equals(status, 'paid'))",
        ),
        (
            CH,
            "(`status` = 'paid') AND (total > 800)",
            "status = 'paid' AND total > 800",
        ),
        // ---- Unknown dialect: only spacing, keywords and parentheses ----
        (None, "((a = 1) AND (b > 2))", "a = 1 AND b > 2"),
        (None, "[o].[x]=1", "[o].[x] = 1"),
        (None, "(\"A\".\"B\"=`c`)", "\"A\".\"B\" = `c`"),
        (None, "x  =\n  1   and   y<>2", "x = 1 AND y <> 2"),
        (None, "(a) = (b)", "a = b"),
        (None, "f((a), (b))", "f(a, b)"),
        (None, "x IN ((1), (2))", "x IN (1, 2)"),
        (None, "(a, b)", "(a, b)"),
        (None, "a = (SELECT 1)", "a = (SELECT 1)"),
        (
            None,
            "((a = 1) = (b = 2)) AND (c = 3)",
            "(a = 1) = (b = 2) AND c = 3",
        ),
        (None, "x BETWEEN 1 AND (b = c)", "x BETWEEN 1 AND (b = c)"),
        (
            None,
            "CASE WHEN (a = 1) AND (b = 2) THEN 1 END",
            "CASE WHEN (a = 1) AND (b = 2) THEN 1 END",
        ),
        (None, "a AND(b = 1)OR(c)", "a AND b = 1 OR c"),
        (None, "a-1 > b.c*2", "a-1 > b.c*2"),
        (None, "a >=-1", "a >= -1"),
        (None, "quantile(0.5)(x) > 1", "quantile(0.5)(x) > 1"),
        (None, "(a = 1) AND (b = 2", "(a = 1) AND (b = 2"),
        (None, "a = 1) AND (b = 2)", "a = 1) AND (b = 2)"),
        (
            None,
            "name = 'unterminated AND (x)",
            "name = 'unterminated AND (x)",
        ),
        (
            None,
            "(größe = 'süß') and (naïve > 1)",
            "größe = 'süß' AND naïve > 1",
        ),
    ];

    fn clean(dialect: Option<PlanDialect>, raw: &str) -> String {
        readable_predicate(dialect, raw, &ExpressionNames::new()).text
    }

    #[test]
    fn cases_clean_as_expected() {
        assert!(CASES.len() >= 60);
        let mut failures = Vec::new();
        for (dialect, raw, expected) in CASES {
            let got = clean(*dialect, raw);
            if got != *expected {
                failures.push(format!(
                    "{dialect:?}\n   raw: {raw}\n  want: {expected}\n   got: {got}"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} mismatches:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    #[test]
    fn cleaning_is_idempotent() {
        for (dialect, raw, _) in CASES {
            let once = clean(*dialect, raw);
            let twice = clean(*dialect, &once);
            assert_eq!(once, twice, "not stable for {dialect:?}: {raw}");
        }
    }

    #[test]
    fn raw_is_untouched_and_blank_is_empty() {
        let names = ExpressionNames::new();
        let p = readable_predicate(PG, "  (a = 1)  ", &names);
        assert_eq!(p.raw, "  (a = 1)  ");
        assert_eq!(p.text, "a = 1");
        for blank in ["", " ", "\t\n  "] {
            let p = readable_predicate(MSSQL, blank, &names);
            assert_eq!(p.text, "");
            assert_eq!(p.raw, blank);
        }
        assert_eq!(readable_expression(None, "   ", &names), "");
    }

    #[test]
    fn expressions_match_the_documented_examples() {
        let names = ExpressionNames::new();
        assert_eq!(
            readable_expression(
                MSSQL,
                "[Shop].[dbo].[Orders].[OrderDate] as [o].[OrderDate]",
                &names
            ),
            "o.OrderDate"
        );
        assert_eq!(
            readable_expression(PG, "o.created_at DESC", &names),
            "o.created_at DESC"
        );
        assert_eq!(
            readable_expression(PG, "(date_trunc('month'::text, created_at))", &names),
            "date_trunc('month', created_at)"
        );
    }

    #[test]
    fn string_literals_are_never_altered() {
        // Every literal in the input must appear byte for byte in the output.
        let inputs: &[(Option<PlanDialect>, &str, &[&str])] = &[
            (PG, "(note = '(a = 1)'::text)", &["'(a = 1)'"]),
            (
                PG,
                "((a = 'x  and  y') AND (b = 'it''s'))",
                &["'x  and  y'", "'it''s'"],
            ),
            (
                MSSQL,
                "[o].[Note]=N'[dbo].[x] as [y]'",
                &["N'[dbo].[x] as [y]'"],
            ),
            (
                MYSQL,
                "(o.note = '`a`.`b` <cache>(1)')",
                &["'`a`.`b` <cache>(1)'"],
            ),
            (ORACLE, "\"A\"=' \"B\"=1 '", &["' \"B\"=1 '"]),
            (
                PG,
                "(x = 'a'::text) AND (y = '::text'::text)",
                &["'a'", "'::text'"],
            ),
            (MYSQL, "(a = \"x and (y)\")", &["\"x and (y)\""]),
        ];
        for (dialect, raw, literals) in inputs {
            let got = clean(*dialect, raw);
            for lit in *literals {
                assert!(got.contains(lit), "{lit} missing from {got}");
            }
            assert_eq!(clean(*dialect, &got), got);
        }
        assert_eq!(
            clean(PG, "(x = 'a'::text) AND (y = '::text'::text)"),
            "x = 'a' AND y = '::text'"
        );
    }

    #[test]
    fn precedence_is_preserved() {
        // Parentheses that change the meaning must survive.
        let keep: &[(Option<PlanDialect>, &str, &str)] = &[
            (PG, "(((a = 1) OR (b = 2)) AND (c = 3))", "(a = 1 OR b = 2)"),
            (
                PG,
                "((a = 1) OR ((b = 2) AND (c = 3)))",
                "(b = 2 AND c = 3)",
            ),
            (PG, "(NOT ((a = 1) AND (b = 2)))", "NOT (a = 1 AND b = 2)"),
            (PG, "((a + b) * c > 1)", "(a + b) * c"),
            (PG, "(x = ((a + b))::integer)", "(a + b)::integer"),
            (MYSQL, "(not((a = 1)))", "NOT(a = 1)"),
            (MYSQL, "((a = 1) xor (b = 2))", "(a = 1) xor (b = 2)"),
            (None, "a - (b - c)", "a - (b - c)"),
            (
                None,
                "(a = 1 OR b = 2) AND (c = 3 OR d = 4)",
                "(a = 1 OR b = 2) AND (c = 3 OR d = 4)",
            ),
            (None, "x IN (1)", "x IN (1)"),
            (None, "NOT (a = 1) AND (b = 2)", "NOT (a = 1) AND b = 2"),
            (None, "(a AND b) OR c AND d", "(a AND b) OR c AND d"),
        ];
        for (dialect, raw, fragment) in keep {
            let got = clean(*dialect, raw);
            assert!(
                got.contains(fragment),
                "{fragment} missing from {got} (from {raw})"
            );
            assert_eq!(clean(*dialect, &got), got);
        }
    }

    #[test]
    fn names_are_translated() {
        let mut names = ExpressionNames::new();
        names.insert(
            "Expr1004".into(),
            "SUM([Shop].[dbo].[Orders].[Total] as [o].[Total])".into(),
        );
        names.insert("Expr1005".into(), "[o].[Total]*(1.1)".into());
        names.insert("Expr1006".into(), "COUNT(*)".into());
        names.insert("Expr1007".into(), "the number of matching rows".into());
        names.insert("Expr1008".into(), "[o].[Qty] + [o].[Bonus]".into());
        names.insert("Expr1009".into(), "[o].[A]=(1)".into());
        names.insert(
            "Expr1010".into(),
            "CASE WHEN [o].[AAAAAAAAAAAAAAAA]=(1) THEN [o].[BBBBBBBBBBBBBBBBBBBBBB] ELSE [o].[CCCCCCCCCCCCCCCCC] END".into(),
        );
        names.insert("Expr1011".into(), "   ".into());
        let cases = [
            ("[Expr1004]>(1000.00)", "SUM(o.Total) > 1000.00"),
            ("[Expr1004]", "SUM(o.Total)"),
            ("[Expr1004] DESC", "SUM(o.Total) DESC"),
            (
                "CONVERT_IMPLICIT(int,[Expr1006],0)",
                "CONVERT_IMPLICIT(int, COUNT(*), 0)",
            ),
            ("[Expr1008]>(5)", "(o.Qty + o.Bonus) > 5"),
            ("[Expr1008]", "o.Qty + o.Bonus"),
            ("[Expr1008] DESC", "o.Qty + o.Bonus DESC"),
            ("[Expr1005]>(5)", "(o.Total*1.1) > 5"),
            ("isnull([Expr1008],(0))", "isnull(o.Qty + o.Bonus, 0)"),
            ("[Expr1009] AND [o].[B]=(2)", "o.A = 1 AND o.B = 2"),
            ("NOT [Expr1009]", "NOT (o.A = 1)"),
            ("[Expr1007]>(5)", "(the number of matching rows) > 5"),
            ("[Expr1007]", "the number of matching rows"),
            // Too long, blank, unknown, or part of a longer name: left alone.
            ("[Expr1010]=(1)", "Expr1010 = 1"),
            ("[Expr1011]=(1)", "Expr1011 = 1"),
            ("[Expr1099]=(1)", "Expr1099 = 1"),
            ("[t].[Expr1004]=(1)", "t.Expr1004 = 1"),
            ("[o].[Note]=N'Expr1004'", "o.Note = N'Expr1004'"),
        ];
        for (raw, expected) in cases {
            let got = readable_predicate(MSSQL, raw, &names).text;
            assert_eq!(got, expected, "from {raw}");
            assert_eq!(
                readable_predicate(MSSQL, &got, &names).text,
                got,
                "not stable: {raw}"
            );
        }

        let mut pg = ExpressionNames::new();
        pg.insert("$0".into(), "(max(o.total))".into());
        pg.insert("$1".into(), "'2026-01-01'::date".into());
        assert_eq!(
            readable_predicate(PG, "(total > $0)", &pg).text,
            "total > max(o.total)"
        );
        assert_eq!(
            readable_predicate(PG, "(d >= $1)", &pg).text,
            "d >= '2026-01-01'"
        );
        assert_eq!(readable_predicate(PG, "(d >= $2)", &pg).text, "d >= $2");
        assert_eq!(
            readable_predicate(PG, "(note = '$0'::text)", &pg).text,
            "note = '$0'"
        );
    }

    #[test]
    fn arbitrary_input_never_panics() {
        let nasty = [
            "((a = 'x) AND (b = \"y)) OR [c].[d] = `e",
            "Seek Keys[1]: Prefix: [a].[b], [c = Scalar Operator((1)), Scalar Operator(",
            "(größe = 'süß'::text) AND (naïve ~~ '%é%'::text) OR 名前 = '日本語'::character varying(",
            "<cache>((<in_optimizer>(`a`.`b`.`c`,<exists>(select 1)))) and not(x) between 1 and",
            "x::timestamp(3) with time zone[] = ANY ('{a,b}'::text[]) AND y::numeric(10,2)::",
            "[db].[s].[t].[c] as [a].[b]>=[@p] AND [x] as [ AND N'q''' OR E'\\",
            "))))((((a = 1) AND (b = 2) ,, :: . . ' ' '' ''' \"\" `` [] [[]] <> < > <a> <a",
            "\"O\".\"A\"=:B1 AND \"O\".\"b c\"(+)=TO_DATE(' 2026', 'syyyy') AND -(-1) -- x /* y",
        ];
        let mut names = ExpressionNames::new();
        names.insert("a".into(), "(b = 'x".into());
        names.insert("x".into(), "y + (z)".into());
        names.insert("c".into(), "the row count".into());
        let mut runs = 0usize;
        for input in nasty {
            let bounds: Vec<usize> = input
                .char_indices()
                .map(|(i, _)| i)
                .chain([input.len()])
                .collect();
            for &b in &bounds {
                for part in [&input[..b], &input[b..]] {
                    for dialect in ALL {
                        let p = readable_predicate(dialect, part, &names);
                        assert_eq!(p.raw, part);
                        let _ = readable_expression(dialect, &p.text, &names);
                        runs += 1;
                    }
                }
            }
        }
        assert!(runs > 5_000);

        // Deep nesting must not overflow the stack, and loses nothing.
        let deep = format!("{}a = 1{}", "(".repeat(5_000), ")".repeat(5_000));
        for dialect in ALL {
            let got = clean(dialect, &deep);
            assert!(got.contains("a = 1"));
            assert_eq!(clean(dialect, &got), got);
        }
        let open = "(".repeat(5_000);
        let close = ")".repeat(5_000);
        for dialect in ALL {
            assert_eq!(clean(dialect, &open), open);
            assert_eq!(clean(dialect, &close), close);
        }
    }

    #[test]
    fn nothing_but_spacing_changes_inside_unknown_text() {
        // With no dialect, the non-space characters outside removed
        // parentheses are the input's, in order.
        for (_, raw, _) in CASES {
            let got = clean(None, raw);
            let strip = |s: &str| {
                s.chars()
                    .filter(|c| !c.is_whitespace() && *c != '(' && *c != ')')
                    .collect::<String>()
            };
            assert_eq!(
                strip(&got).to_lowercase(),
                strip(raw).to_lowercase(),
                "from {raw}"
            );
        }
    }

    #[test]
    fn a_long_predicate_is_fast() {
        let mut raw = String::from("(");
        for i in 0..2_000 {
            if i > 0 {
                raw.push_str(" AND ");
            }
            raw.push_str(&format!("(c{i} = {i})"));
        }
        raw.push(')');
        assert!(raw.len() >= 20_000);
        let names = ExpressionNames::new();
        for dialect in [PG, MSSQL, MYSQL, None] {
            let started = Instant::now();
            let got = readable_predicate(dialect, &raw, &names).text;
            let elapsed = started.elapsed();
            assert!(elapsed.as_millis() < 200, "{dialect:?} took {elapsed:?}");
            assert!(got.starts_with("c0 = 0 AND c1 = 1 AND "));
            assert!(got.ends_with("AND c1999 = 1999"));
            assert!(!got.contains('('));
        }
        // A long flat list of casts and quoted names as well.
        let wide = (0..2_000)
            .map(|i| format!("((t.c{i})::text = 'v{i}'::text)"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let started = Instant::now();
        let got = readable_predicate(PG, &wide, &names).text;
        assert!(started.elapsed().as_millis() < 200);
        assert!(got.starts_with("t.c0 = 'v0' OR t.c1 = 'v1' OR "));
    }
}

#[cfg(test)]
mod bitmap_probe_tests {
    use super::*;

    #[test]
    fn bitmap_tests_are_split_from_the_rest_of_a_predicate() {
        assert_eq!(bitmap_probe("o.status = 'shipped'"), None);
        assert_eq!(
            bitmap_probe("PROBE(Opt_Bitmap1003, o.CustomerID)"),
            Some(BitmapProbe {
                columns: vec!["o.CustomerID".into()],
                rest: None,
            })
        );
        assert_eq!(
            bitmap_probe("o.note = 'a AND b' and PROBE(Bitmap1007, c.customer_id, N'[IN ROW]')"),
            Some(BitmapProbe {
                columns: vec!["c.customer_id".into()],
                rest: Some("o.note = 'a AND b'".into()),
            })
        );
        // A PROBE inside another expression is left alone.
        assert_eq!(bitmap_probe("NOT (PROBE(Bitmap1, a.x))"), None);
    }
}
