//! SQL syntax tokenizer — port of `SQLSyntax.swift`.
//!
//! Pure, UI-free. Classifies runs of SQL text into token kinds (keywords, types,
//! functions, constants, variables, numbers, strings, comments) with byte
//! ranges. Which words are keywords, functions, constants, and types comes
//! from the dialect's built-in vocabulary (`sqlly_intel::intellisense::builtins`),
//! the same catalog hover cards and signature help read. Later token kinds win
//! on overlap: words → variables → numbers → strings → comments. Colors for
//! these kinds are applied by [`crate::theme::Theme::syntax_color`].

use sqlly_lang::sqlmodel::Dialect;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyntaxTokenKind {
    Keyword,
    Type,
    Function,
    Constant,
    Variable,
    Number,
    String,
    Comment,
}

impl SyntaxTokenKind {
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::Type => "type",
            Self::Function => "function",
            Self::Constant => "constant",
            Self::Variable => "variable",
            Self::Number => "number",
            Self::String => "string",
            Self::Comment => "comment",
        }
    }
}

/// How one SQL dialect spells comments and string escapes. Comment syntax is
/// the part of highlighting that genuinely differs between engines, and
/// getting it wrong is loud: a missed `#` leaves a MySQL comment painted as
/// SQL, and a spurious one turns `FROM #tmp` in T-SQL into a green line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommentSyntax {
    /// `#` opens a line comment. MySQL/MariaDB only — in T-SQL `#name` is a
    /// temp table.
    pub hash_line: bool,
    /// `--` opens a line comment only when followed by whitespace or the end
    /// of input. MySQL requires that space (bare `a--b` is arithmetic);
    /// everywhere else `--` always opens a comment.
    pub dash_needs_space: bool,
    /// `/* /* */ */` nests rather than ending at the first `*/`. True for
    /// T-SQL and Postgres, false for MySQL, SQLite, and Oracle.
    pub nested_blocks: bool,
    /// A backslash escapes the next byte inside a string literal, so `'a\''`
    /// is one string. MySQL's default; standard SQL doubles the quote.
    pub backslash_escapes: bool,
    /// `` `name` `` is a delimited identifier (MySQL, SQLite).
    pub backtick_idents: bool,
    /// `[name]` is a delimited identifier (T-SQL, SQLite).
    pub bracket_idents: bool,
    /// `"…"` is a string literal rather than an identifier (MySQL without
    /// ANSI_QUOTES).
    pub double_quote_strings: bool,
    /// `E'…'` strings take backslash escapes and `$tag$…$tag$` quotes a
    /// string (Postgres).
    pub postgres_strings: bool,
    /// `q'[…]'`-style alternative quoting (Oracle).
    pub q_quotes: bool,
}

/// The comment and quoting rules for `dialect`.
pub fn comment_syntax(dialect: Dialect) -> CommentSyntax {
    let base = CommentSyntax {
        hash_line: false,
        dash_needs_space: false,
        nested_blocks: false,
        backslash_escapes: false,
        backtick_idents: false,
        bracket_idents: false,
        double_quote_strings: false,
        postgres_strings: false,
        q_quotes: false,
    };
    match dialect {
        Dialect::SqlServer => CommentSyntax {
            nested_blocks: true,
            bracket_idents: true,
            ..base
        },
        Dialect::Postgres => CommentSyntax {
            nested_blocks: true,
            postgres_strings: true,
            ..base
        },
        Dialect::MySql => CommentSyntax {
            hash_line: true,
            dash_needs_space: true,
            backslash_escapes: true,
            backtick_idents: true,
            double_quote_strings: true,
            ..base
        },
        Dialect::Sqlite => CommentSyntax {
            backtick_idents: true,
            bracket_idents: true,
            ..base
        },
        Dialect::Oracle => CommentSyntax {
            q_quotes: true,
            ..base
        },
    }
}

/// End (exclusive) of a quoted run opened at `start` by `quote`, where a
/// doubled closing quote is an escaped one and, with `backslash`, a
/// backslash escapes the next byte. Runs to the end of the line when
/// unterminated.
fn quoted_end(bytes: &[u8], start: usize, close: u8, backslash: bool) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        if backslash && bytes[i] == b'\\' {
            i += 2;
        } else if bytes[i] == close {
            if bytes.get(i + 1) == Some(&close) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// A Postgres dollar-quote tag at `i` (`$$` or `$tag$`, the tag an
/// identifier that does not start with a digit), as its byte length.
fn dollar_tag_len(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i) != Some(&b'$') {
        return None;
    }
    let mut j = i + 1;
    if bytes.get(j).is_some_and(|b| b.is_ascii_digit()) {
        return None;
    }
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    (bytes.get(j) == Some(&b'$')).then_some(j + 1 - i)
}

/// Whether the byte before `i` continues an identifier, so a quote or `q`
/// there is not the start of a new literal.
fn after_word(bytes: &[u8], i: usize) -> bool {
    i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_' || bytes[i - 1] == b'$')
}

/// Lexer state carried across a line boundary: how many block comments are
/// still open where the next line starts. `0` is ordinary SQL.
///
/// The editor shapes one line at a time, so without this a `/* … */` spanning
/// three lines would only paint its first line as a comment and highlight the
/// rest as live SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CommentState {
    pub depth: u16,
}

impl CommentState {
    /// True when the line starts inside an unterminated block comment.
    pub fn is_inside(self) -> bool {
        self.depth > 0
    }
}

/// A classified byte range in the source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxToken {
    pub start: usize,
    pub end: usize,
    pub kind: SyntaxTokenKind,
}

/// The function whose first argument starts at `start` — `DATEDIFF ( MINUTE`
/// with any whitespace between the name, the paren, and the word — or `None`
/// when the word is not the first argument of a call. Scans backwards so the
/// single forward pass in [`classify_line`] stays context-free.
fn first_arg_callee(text: &str, start: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut i = start;
    while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b'(' {
        return None;
    }
    i -= 1;
    while i > 0 && (bytes[i - 1] as char).is_ascii_whitespace() {
        i -= 1;
    }
    let name_end = i;
    while i > 0 && ((bytes[i - 1] as char).is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        i -= 1;
    }
    (i < name_end).then(|| &text[i..name_end])
}

/// Whether the next non-blank byte after `end` opens a call.
fn followed_by_paren(bytes: &[u8], end: usize) -> bool {
    bytes[end..]
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .is_some_and(|b| *b == b'(')
}

/// Whether the word ending just before `start` (skipping blanks) is `AS` —
/// `CAST(x AS char(10))` names a type even though `CHAR(` is also a function.
fn preceded_by_as(bytes: &[u8], start: usize) -> bool {
    let mut i = start;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i >= 2
        && bytes[i - 2..i].eq_ignore_ascii_case(b"as")
        && (i == 2 || !(bytes[i - 3].is_ascii_alphanumeric() || bytes[i - 3] == b'_'))
}

/// Classify one bare word of the dialect's vocabulary. A word can belong to
/// several sets (`LEFT` is a keyword and a function, `CHAR` a type and a
/// function, `DAY` a function and a date part), so the surrounding text
/// decides: a fixed word in the argument slot that takes it is a constant, a
/// word followed by `(` is a call, and a function without parentheses only
/// counts when the dialect calls it that way (`CURRENT_TIMESTAMP`) — a column
/// named `year` or `age` stays plain text.
fn classify_word(
    lexicon: &sqlly_intel::intellisense::BuiltinCatalog,
    text: &str,
    start: usize,
    end: usize,
) -> Option<SyntaxTokenKind> {
    let bytes = text.as_bytes();
    let word = &text[start..end];
    let lower = word.to_ascii_lowercase();
    // A qualified name part (`t.date`, `dbo.count`) is an object or column.
    if start > 0 && bytes[start - 1] == b'.' {
        return None;
    }
    if let Some(callee) = first_arg_callee(text, start)
        && lexicon.slot_param(callee, 0, word).is_some()
    {
        return Some(SyntaxTokenKind::Constant);
    }
    if lexicon.is_constant_word(&lower) {
        return Some(SyntaxTokenKind::Constant);
    }
    let is_function = lexicon.is_function_word(&lower);
    let is_type = lexicon.is_type_word(&lower);
    if is_function && followed_by_paren(bytes, end) && !(is_type && preceded_by_as(bytes, start)) {
        return Some(SyntaxTokenKind::Function);
    }
    if lexicon.is_keyword_word(&lower) {
        return Some(SyntaxTokenKind::Keyword);
    }
    if is_type {
        return Some(SyntaxTokenKind::Type);
    }
    if is_function && lexicon.function(word).is_some_and(|f| f.niladic) {
        return Some(SyntaxTokenKind::Function);
    }
    None
}

/// Classifies SQL text in T-SQL, the app's default dialect.
pub fn classify(text: &str) -> Vec<SyntaxToken> {
    classify_for(Dialect::SqlServer, text)
}

/// Classifies a whole SQL document in `dialect`. Block comments spanning
/// several lines are handled because the scan sees the whole text.
pub fn classify_for(dialect: Dialect, text: &str) -> Vec<SyntaxToken> {
    classify_line(dialect, text, CommentState::default()).0
}

/// Classifies one line that starts in `entry`, returning its tokens and the
/// state the next line starts in. This is what the editor calls: it shapes a
/// line at a time, so the open-block-comment depth has to be threaded through
/// by hand.
pub fn classify_line(
    dialect: Dialect,
    text: &str,
    entry: CommentState,
) -> (Vec<SyntaxToken>, CommentState) {
    let syntax = comment_syntax(dialect);
    let mut tokens: Vec<SyntaxToken> = Vec::new();

    // 1. Keywords / types / functions / constants — word-level, from the
    // dialect's vocabulary.
    let lexicon = sqlly_intel::intellisense::builtins(dialect);
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < bytes.len()
                && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
            // Part of `@name` / `@@name`: the variable pass owns it.
            if start > 0 && bytes[start - 1] == b'@' {
                continue;
            }
            if let Some(kind) = classify_word(lexicon, text, start, i) {
                tokens.push(SyntaxToken {
                    start,
                    end: i,
                    kind,
                });
            }
        } else if c.is_ascii_digit() {
            // Skip a number (or `1st`-style run) whole, so `x1` style words
            // are never split into a word after a digit.
            while i < bytes.len()
                && ((bytes[i] as char).is_ascii_alphanumeric() || bytes[i] == b'_')
            {
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    // 2. Variables — `@name` / `@@name`. The `@@` system functions the
    // dialect documents (`@@ROWCOUNT`) are functions, not variables.
    for m in variable_regex().find_iter(text) {
        let kind = if m.as_str().starts_with("@@") && lexicon.function(m.as_str()).is_some() {
            SyntaxTokenKind::Function
        } else {
            SyntaxTokenKind::Variable
        };
        tokens.push(SyntaxToken {
            start: m.start(),
            end: m.end(),
            kind,
        });
    }

    // 3. Numbers — integer/decimal/scientific.
    for m in number_regex().find_iter(text) {
        tokens.push(SyntaxToken {
            start: m.start(),
            end: m.end(),
            kind: SyntaxTokenKind::Number,
        });
    }

    // 4-5. Strings and comments in one pass, so neither can open inside the
    // other: `'has -- dashes'` stays a string, and `-- it's fine` stays a
    // comment to end of line.
    let mut identifiers = Vec::new();
    let exit = scan_quotes_and_comments(text, syntax, entry, &mut tokens, &mut identifiers);
    // A delimited identifier is a name: `[select]` or `` `count` `` is not a
    // keyword or a function, whatever it spells.
    if !identifiers.is_empty() {
        tokens.retain(|t| {
            !identifiers
                .iter()
                .any(|&(start, end)| t.start >= start && t.end <= end)
        });
    }

    // Apply later-wins resolution: sort by start, and where starts are equal,
    // keep the later-applied (higher priority) token. Where a later kind
    // fully contains an earlier kind, the later wins.
    tokens.sort_by_key(|t| (t.start, t.end));

    (resolve_overlaps(text.len(), tokens), exit)
}

/// The state each line of `text` starts in, one entry per line (the first is
/// always the default). Feeds [`classify_line`] when the caller renders lines
/// independently.
pub fn comment_states(dialect: Dialect, text: &str) -> Vec<CommentState> {
    let syntax = comment_syntax(dialect);
    let mut states = Vec::with_capacity(text.bytes().filter(|b| *b == b'\n').count() + 1);
    let mut state = CommentState::default();
    let mut scratch = Vec::new();
    for line in text.split('\n') {
        states.push(state);
        scratch.clear();
        state = scan_quotes_and_comments(line, syntax, state, &mut scratch, &mut Vec::new());
    }
    states
}

/// Priority of a token kind when ranges overlap — mirrors the application
/// order documented on [`classify`]: later passes win, so a keyword or number
/// inside a string/comment takes the container's kind.
fn overlap_priority(kind: SyntaxTokenKind) -> u8 {
    match kind {
        SyntaxTokenKind::Keyword
        | SyntaxTokenKind::Type
        | SyntaxTokenKind::Function
        | SyntaxTokenKind::Constant => 0,
        SyntaxTokenKind::Variable => 1,
        SyntaxTokenKind::Number => 2,
        SyntaxTokenKind::String => 3,
        SyntaxTokenKind::Comment => 4,
    }
}

/// Flattens possibly-overlapping tokens into disjoint, sorted tokens.
///
/// The scanning passes are independent (a number regex match lands inside a
/// string literal, a keyword lands inside a comment), so the raw token list
/// can contain overlaps. Consumers turn tokens into styled text runs whose
/// lengths must sum to exactly the line length — an overlapping token would
/// double-count bytes and panic the shaper — so overlaps are resolved here,
/// once, for every consumer.
fn resolve_overlaps(len: usize, tokens: Vec<SyntaxToken>) -> Vec<SyntaxToken> {
    // Fast path: already disjoint (the common case — no strings/comments
    // containing words or numbers). Input is sorted by (start, end).
    let disjoint = tokens.windows(2).all(|pair| pair[1].start >= pair[0].end);
    if disjoint {
        return tokens;
    }

    // Painter's algorithm: mark each byte with the highest-priority kind
    // covering it, then coalesce consecutive equal kinds back into tokens.
    let mut map: Vec<Option<SyntaxTokenKind>> = vec![None; len];
    let mut ordered = tokens;
    ordered.sort_by_key(|t| overlap_priority(t.kind));
    for t in &ordered {
        let end = t.end.min(len);
        for slot in &mut map[t.start.min(end)..end] {
            *slot = Some(t.kind);
        }
    }

    let mut resolved = Vec::new();
    let mut i = 0;
    while i < len {
        let Some(kind) = map[i] else {
            i += 1;
            continue;
        };
        let start = i;
        while i < len && map[i] == Some(kind) {
            i += 1;
        }
        resolved.push(SyntaxToken {
            start,
            end: i,
            kind,
        });
    }
    resolved
}

/// Scans `text` for string literals and comments in one left-to-right pass,
/// starting in `entry` and returning the state the text ends in.
///
/// One pass, not two, because the two constructs mask each other: a `--`
/// inside a string literal does not open a comment, and a quote inside a
/// comment does not open a string. Scanning them separately (as this used to)
/// let `SELECT 'a -- b'` paint the rest of the line as a comment.
fn scan_quotes_and_comments(
    text: &str,
    syntax: CommentSyntax,
    entry: CommentState,
    tokens: &mut Vec<SyntaxToken>,
    identifiers: &mut Vec<(usize, usize)>,
) -> CommentState {
    let bytes = text.as_bytes();
    let mut depth = entry.depth;
    let mut i = 0;
    // A line that opens already inside a block comment runs its comment from
    // column zero.
    let mut block_start = 0;

    while i < bytes.len() {
        if depth > 0 {
            if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                depth -= 1;
                i += 2;
                if depth == 0 {
                    tokens.push(SyntaxToken {
                        start: block_start,
                        end: i,
                        kind: SyntaxTokenKind::Comment,
                    });
                }
            } else if syntax.nested_blocks && bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                depth = depth.saturating_add(1);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        match bytes[i] {
            // Oracle alternative quoting: q'[ … ]', q'{…}', q'X…X' (and nq'…').
            b'q' | b'Q'
                if syntax.q_quotes
                    && bytes.get(i + 1) == Some(&b'\'')
                    && bytes.get(i + 2).is_some()
                    && (!after_word(bytes, i)
                        || (i > 0
                            && bytes[i - 1].eq_ignore_ascii_case(&b'n')
                            && !after_word(bytes, i - 1))) =>
            {
                let start = if i > 0 && bytes[i - 1].eq_ignore_ascii_case(&b'n') {
                    i - 1
                } else {
                    i
                };
                let open = bytes[i + 2];
                let close = match open {
                    b'[' => b']',
                    b'{' => b'}',
                    b'(' => b')',
                    b'<' => b'>',
                    other => other,
                };
                let mut j = i + 3;
                while j < bytes.len() && !(bytes[j] == close && bytes.get(j + 1) == Some(&b'\'')) {
                    j += 1;
                }
                let end = (j + 2).min(bytes.len());
                tokens.push(SyntaxToken {
                    start,
                    end,
                    kind: SyntaxTokenKind::String,
                });
                i = end;
            }
            b'\'' => {
                // A one-letter prefix joins the literal: `N'…'` (national),
                // `X'…'` / `B'…'` (hex / bit), and Postgres `E'…'`, which
                // also takes backslash escapes.
                let prefix = (i > 0 && !after_word(bytes, i - 1))
                    .then(|| bytes[i - 1].to_ascii_lowercase())
                    .filter(|p| {
                        matches!(p, b'n' | b'x' | b'b') || (*p == b'e' && syntax.postgres_strings)
                    });
                let start = if prefix.is_some() { i - 1 } else { i };
                let end = quoted_end(
                    bytes,
                    i,
                    b'\'',
                    syntax.backslash_escapes || prefix == Some(b'e'),
                );
                tokens.push(SyntaxToken {
                    start,
                    end,
                    kind: SyntaxTokenKind::String,
                });
                i = end;
            }
            b'"' if syntax.double_quote_strings => {
                let end = quoted_end(bytes, i, b'"', syntax.backslash_escapes);
                tokens.push(SyntaxToken {
                    start: i,
                    end,
                    kind: SyntaxTokenKind::String,
                });
                i = end;
            }
            // Delimited identifiers: nothing inside them is a quote, a
            // comment, or a keyword.
            b'"' => {
                let end = quoted_end(bytes, i, b'"', false);
                identifiers.push((i, end));
                i = end;
            }
            b'`' if syntax.backtick_idents => {
                let end = quoted_end(bytes, i, b'`', false);
                identifiers.push((i, end));
                i = end;
            }
            b'[' if syntax.bracket_idents => {
                let end = quoted_end(bytes, i, b']', false);
                identifiers.push((i, end));
                i = end;
            }
            b'$' if syntax.postgres_strings
                && !after_word(bytes, i)
                && dollar_tag_len(bytes, i).is_some() =>
            {
                let tag_len = dollar_tag_len(bytes, i).unwrap_or(2);
                let tag = &bytes[i..i + tag_len];
                let body = i + tag_len;
                let end = bytes[body..]
                    .windows(tag_len)
                    .position(|w| w == tag)
                    .map(|p| body + p + tag_len)
                    .unwrap_or(bytes.len());
                tokens.push(SyntaxToken {
                    start: i,
                    end,
                    kind: SyntaxTokenKind::String,
                });
                i = end;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') && opens_line_comment(bytes, i, syntax) => {
                tokens.push(line_comment_token(bytes, i));
                i = line_comment_end(bytes, i);
            }
            b'#' if syntax.hash_line => {
                tokens.push(line_comment_token(bytes, i));
                i = line_comment_end(bytes, i);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                block_start = i;
                depth = 1;
                i += 2;
            }
            _ => i += 1,
        }
    }

    if depth > 0 {
        // Unterminated: paint to the end and hand the depth to the next line.
        tokens.push(SyntaxToken {
            start: block_start,
            end: bytes.len(),
            kind: SyntaxTokenKind::Comment,
        });
    }
    CommentState { depth }
}

/// Whether the `--` at `i` opens a comment. MySQL needs whitespace (or the
/// end of the line) after it; every other dialect does not.
fn opens_line_comment(bytes: &[u8], i: usize, syntax: CommentSyntax) -> bool {
    if !syntax.dash_needs_space {
        return true;
    }
    match bytes.get(i + 2) {
        None => true,
        Some(next) => next.is_ascii_whitespace(),
    }
}

fn line_comment_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

fn line_comment_token(bytes: &[u8], from: usize) -> SyntaxToken {
    SyntaxToken {
        start: from,
        end: line_comment_end(bytes, from),
        kind: SyntaxTokenKind::Comment,
    }
}

fn variable_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"@@?[a-zA-Z_][a-zA-Z0-9_]*").unwrap())
}

fn number_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\b\d+\.?\d*([eE][+-]?\d+)?\b").unwrap())
}

/// What a plain word in the editor names in the connection's data model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticKind {
    Schema,
    /// A table or view.
    Table,
    Procedure,
    UserFunction,
    UserType,
}

const NAME_TABLE: u8 = 1;
const NAME_PROCEDURE: u8 = 2;
const NAME_FUNCTION: u8 = 4;
const NAME_TYPE: u8 = 8;

/// Data-model names the editor can recognize in plain text, folded to
/// lowercase. Built from the IntelliSense catalog; empty when not connected.
#[derive(Debug, Default)]
pub struct SemanticNames {
    schemas: std::collections::HashSet<String>,
    objects: std::collections::HashMap<String, u8>,
}

impl SemanticNames {
    pub fn from_catalog(catalog: &sqlly_intel::intellisense::CompletionCatalog) -> Self {
        use sqlly_core::ObjectKind as K;
        let mut names = Self::default();
        for schema in &catalog.schemas {
            names.schemas.insert(schema.to_lowercase());
        }
        for object in &catalog.objects {
            let flag = match object.kind {
                K::Table | K::View | K::MaterializedView => NAME_TABLE,
                K::StoredProcedure => NAME_PROCEDURE,
                K::ScalarFunction | K::TableFunction | K::AggregateFunction => NAME_FUNCTION,
                K::Type => NAME_TYPE,
                _ => continue,
            };
            if let Some(schema) = &object.schema {
                names.schemas.insert(schema.to_lowercase());
            }
            *names.objects.entry(object.name.to_lowercase()).or_default() |= flag;
        }
        names
    }

    pub fn is_empty(&self) -> bool {
        self.schemas.is_empty() && self.objects.is_empty()
    }

    /// What `word` (already unquoted) names, given the next non-blank byte
    /// after it: before `.` a schema wins, before `(` a function or
    /// procedure does.
    fn kind_of(&self, word: &str, next: Option<u8>) -> Option<SemanticKind> {
        let fold = word.to_lowercase();
        if next == Some(b'.') && self.schemas.contains(&fold) {
            return Some(SemanticKind::Schema);
        }
        let flags = self.objects.get(&fold).copied().unwrap_or(0);
        if flags == 0 {
            return None;
        }
        let order: &[(u8, SemanticKind)] = if next == Some(b'(') {
            &[
                (NAME_FUNCTION, SemanticKind::UserFunction),
                (NAME_PROCEDURE, SemanticKind::Procedure),
                (NAME_TABLE, SemanticKind::Table),
                (NAME_TYPE, SemanticKind::UserType),
            ]
        } else {
            &[
                (NAME_TABLE, SemanticKind::Table),
                (NAME_PROCEDURE, SemanticKind::Procedure),
                (NAME_FUNCTION, SemanticKind::UserFunction),
                (NAME_TYPE, SemanticKind::UserType),
            ]
        };
        order
            .iter()
            .find(|(flag, _)| flags & flag != 0)
            .map(|(_, kind)| *kind)
    }
}

/// Words in `line` outside every `tokens` range that name a data-model
/// object, with what they name. Bare words and `[x]` / `"x"` / `` `x` ``
/// delimited identifiers are both recognized; the span covers the
/// delimiters.
pub fn semantic_spans(
    line: &str,
    tokens: &[SyntaxToken],
    names: &SemanticNames,
) -> Vec<(usize, usize, SemanticKind)> {
    let mut spans = Vec::new();
    if names.is_empty() {
        return spans;
    }
    let bytes = line.as_bytes();
    let covered = |at: usize| tokens.iter().any(|t| at >= t.start && at < t.end);
    let next_non_blank = |from: usize| {
        bytes[from..]
            .iter()
            .copied()
            .find(|b| !b.is_ascii_whitespace())
    };
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'#';
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if covered(i) {
            i += 1;
            continue;
        }
        let close = match b {
            b'[' => Some(b']'),
            b'"' => Some(b'"'),
            b'`' => Some(b'`'),
            _ => None,
        };
        let (start, inner_start, inner_end, end) = if let Some(close) = close {
            let Some(len) = bytes[i + 1..].iter().position(|&c| c == close) else {
                break;
            };
            (i, i + 1, i + 1 + len, i + 2 + len)
        } else if b.is_ascii_alphabetic() || b == b'_' || b == b'#' {
            let mut j = i;
            while j < bytes.len() && is_word(bytes[j]) {
                j += 1;
            }
            // Part of `@name`: a variable, never an object.
            if i > 0 && bytes[i - 1] == b'@' {
                i = j;
                continue;
            }
            (i, i, j, j)
        } else {
            if b.is_ascii_digit() {
                while i < bytes.len() && is_word(bytes[i]) {
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        };
        if (start..end).all(|at| !covered(at))
            && let Some(kind) = names.kind_of(&line[inner_start..inner_end], next_non_blank(end))
        {
            spans.push((start, end, kind));
        }
        i = end;
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_keywords() {
        let tokens = classify("SELECT * FROM Users");
        let kinds: Vec<_> = tokens.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&SyntaxTokenKind::Keyword));
    }

    #[test]
    fn classifies_types() {
        let tokens = classify("DECLARE @x INT = 0");
        let kinds: Vec<_> = tokens.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&SyntaxTokenKind::Type));
        assert!(kinds.contains(&SyntaxTokenKind::Keyword));
        assert!(kinds.contains(&SyntaxTokenKind::Number));
        assert!(kinds.contains(&SyntaxTokenKind::Variable));
    }

    /// Finds the kind covering the first occurrence of `needle`.
    fn kind_of(sql: &str, needle: &str) -> Option<SyntaxTokenKind> {
        let at = sql.find(needle).expect("needle present");
        classify(sql)
            .into_iter()
            .find(|t| t.start <= at && at < t.end)
            .map(|t| t.kind)
    }

    #[test]
    fn datepart_argument_is_a_constant() {
        for (sql, needle) in [
            ("SELECT DATEDIFF(MINUTE, a, b) FROM t", "MINUTE"),
            ("SELECT DATEDIFF (MINUTE, a, b) FROM t", "MINUTE"),
            ("SELECT DATEDIFF(\n    MINUTE, a, b) FROM t", "MINUTE"),
            ("SELECT datepart(minute, a) FROM t", "minute"),
            ("SELECT DATEDIFF(mi, a, b) FROM t", "mi"),
        ] {
            assert_eq!(
                kind_of(sql, needle),
                Some(SyntaxTokenKind::Constant),
                "{sql}"
            );
        }
    }

    #[test]
    fn datepart_words_that_double_as_functions_switch_kind_by_slot() {
        assert_eq!(
            kind_of("SELECT DATEADD(DAY, 1, x) FROM t", "DAY"),
            Some(SyntaxTokenKind::Constant),
        );
        assert_eq!(
            kind_of("SELECT DAY(x) FROM t", "DAY"),
            Some(SyntaxTokenKind::Function),
        );
    }

    #[test]
    fn datepart_words_outside_the_slot_are_not_constants() {
        // Not the first argument, and not a datepart function at all.
        assert_ne!(
            kind_of("SELECT DATEDIFF(DAY, minute, b) FROM t", "minute"),
            Some(SyntaxTokenKind::Constant),
        );
        assert_ne!(
            kind_of("SELECT MAX(minute) FROM t", "minute"),
            Some(SyntaxTokenKind::Constant),
        );
        assert_ne!(
            kind_of("SELECT minute FROM t", "minute"),
            Some(SyntaxTokenKind::Constant),
        );
    }

    #[test]
    fn datepart_inside_a_string_or_comment_keeps_the_container_kind() {
        assert_eq!(
            kind_of("SELECT 'DATEDIFF(MINUTE' FROM t", "MINUTE"),
            Some(SyntaxTokenKind::String),
        );
        assert_eq!(
            kind_of("-- DATEDIFF(MINUTE, a, b)", "MINUTE"),
            Some(SyntaxTokenKind::Comment),
        );
    }

    /// The kind covering `needle` when `sql` is classified in `dialect`.
    fn kind_in(dialect: Dialect, sql: &str, needle: &str) -> Option<SyntaxTokenKind> {
        let at = sql.find(needle).expect("needle present");
        classify_for(dialect, sql)
            .into_iter()
            .find(|t| t.start <= at && at < t.end)
            .map(|t| t.kind)
    }

    #[test]
    fn dashes_inside_a_string_do_not_open_a_comment() {
        // The string and comment scans used to be independent passes, so the
        // `--` here opened a comment that swallowed the rest of the line.
        let sql = "SELECT 'a -- b', COUNT(*) FROM t";
        assert_eq!(
            kind_in(Dialect::SqlServer, sql, "-- b"),
            Some(SyntaxTokenKind::String)
        );
        assert_eq!(
            kind_in(Dialect::SqlServer, sql, "COUNT"),
            Some(SyntaxTokenKind::Function),
        );
    }

    #[test]
    fn quote_inside_a_comment_does_not_open_a_string() {
        let sql = "-- it's fine\nSELECT 1";
        assert_eq!(
            kind_in(Dialect::SqlServer, sql, "it's"),
            Some(SyntaxTokenKind::Comment)
        );
        assert_eq!(
            kind_in(Dialect::SqlServer, sql, "SELECT"),
            Some(SyntaxTokenKind::Keyword)
        );
    }

    #[test]
    fn block_comment_spans_lines() {
        let sql = "SELECT 1\n/* one\n   SELECT 2\n   two */\nSELECT 3";
        // The middle line is entirely inside the comment.
        assert_eq!(
            kind_in(Dialect::SqlServer, sql, "SELECT 2"),
            Some(SyntaxTokenKind::Comment)
        );
        // …and the line after the close is live SQL again.
        assert_eq!(
            kind_in(Dialect::SqlServer, sql, "SELECT 3"),
            Some(SyntaxTokenKind::Keyword)
        );
    }

    #[test]
    fn comment_states_track_depth_per_line() {
        let sql = "SELECT 1\n/* open\nstill open\nclosed */\nSELECT 2";
        let states = comment_states(Dialect::SqlServer, sql);
        let depths: Vec<u16> = states.iter().map(|s| s.depth).collect();
        assert_eq!(depths, vec![0, 0, 1, 1, 0]);
    }

    /// The editor's per-line path must agree with a whole-document scan.
    #[test]
    fn classify_line_threads_state_like_a_whole_document_scan() {
        let sql = "/* a\nSELECT 1\n*/ SELECT 2";
        let states = comment_states(Dialect::SqlServer, sql);
        for (index, line) in sql.split('\n').enumerate() {
            let (tokens, _) = classify_line(Dialect::SqlServer, line, states[index]);
            let inside_comment = tokens
                .iter()
                .any(|t| t.kind == SyntaxTokenKind::Comment && t.start == 0);
            assert_eq!(inside_comment, index <= 2, "line {index}: {line:?}");
        }
        // After the close, the last line's `SELECT` is a keyword again.
        let (tokens, exit) = classify_line(Dialect::SqlServer, "*/ SELECT 2", states[2]);
        assert_eq!(exit, CommentState::default());
        assert!(tokens.iter().any(|t| t.kind == SyntaxTokenKind::Keyword));
    }

    /// DBeaver #42372: lowercase built-ins are still built-ins.
    #[test]
    fn lowercase_mysql_builtins_are_functions() {
        let sql = "select if(true, 1, 0), greatest(1,2), instr(name, 'a'), test.f() from dual";
        for name in ["if", "greatest", "instr"] {
            assert_eq!(
                kind_in(Dialect::MySql, sql, name),
                Some(SyntaxTokenKind::Function),
                "{name}"
            );
        }
        // A user function behind a schema stays plain text, not an error.
        assert_eq!(kind_in(Dialect::MySql, sql, "f()"), None);
    }

    /// MariaDB's `EXECUTE IMMEDIATE 'sql'` highlights both words.
    #[test]
    fn execute_immediate_is_a_keyword_in_mysql() {
        let sql = "EXECUTE IMMEDIATE CONCAT('SELECT * FROM ', @t)";
        assert_eq!(
            kind_in(Dialect::MySql, sql, "EXECUTE"),
            Some(SyntaxTokenKind::Keyword)
        );
        assert_eq!(
            kind_in(Dialect::MySql, sql, "IMMEDIATE"),
            Some(SyntaxTokenKind::Keyword)
        );
    }

    #[test]
    fn hash_opens_a_comment_only_in_mysql() {
        let sql = "SELECT 1 # note";
        assert_eq!(
            kind_in(Dialect::MySql, sql, "# note"),
            Some(SyntaxTokenKind::Comment)
        );
        assert_eq!(kind_in(Dialect::SqlServer, sql, "# note"), None);
    }

    #[test]
    fn tsql_temp_tables_are_not_comments() {
        // The reason `#` has to be dialect-gated.
        let sql = "SELECT * FROM #tmp WHERE id = 1";
        assert_eq!(kind_in(Dialect::SqlServer, sql, "#tmp"), None);
        assert_eq!(
            kind_in(Dialect::SqlServer, sql, "WHERE"),
            Some(SyntaxTokenKind::Keyword),
        );
    }

    #[test]
    fn mysql_requires_a_space_after_the_double_dash() {
        // MySQL reads `1--1` as arithmetic, not a comment.
        assert_eq!(kind_in(Dialect::MySql, "SELECT 1--1", "--1"), None);
        assert_eq!(
            kind_in(Dialect::MySql, "SELECT 1 -- note", "-- note"),
            Some(SyntaxTokenKind::Comment),
        );
        // Every other dialect takes a bare `--`.
        assert_eq!(
            kind_in(Dialect::SqlServer, "SELECT 1--1", "--1"),
            Some(SyntaxTokenKind::Comment),
        );
    }

    #[test]
    fn block_comments_nest_only_where_the_dialect_says_so() {
        let sql = "/* a /* b */ still */ SELECT 1";
        // T-SQL and Postgres nest: the whole prefix is one comment and the
        // trailing SELECT is live.
        for dialect in [Dialect::SqlServer, Dialect::Postgres] {
            assert_eq!(
                kind_in(dialect, sql, "still"),
                Some(SyntaxTokenKind::Comment)
            );
            assert_eq!(
                kind_in(dialect, sql, "SELECT"),
                Some(SyntaxTokenKind::Keyword)
            );
        }
        // MySQL and SQLite close at the first `*/`, so `still` is loose text.
        for dialect in [Dialect::MySql, Dialect::Sqlite] {
            assert_eq!(kind_in(dialect, sql, "still"), None);
        }
    }

    #[test]
    fn mysql_backslash_escape_keeps_one_string() {
        // `'a\'b'` is a single MySQL string; treating the middle quote as a
        // terminator would leave the rest of the line mis-colored.
        let sql = r"SELECT 'a\'b', COUNT(*) FROM t";
        assert_eq!(
            kind_in(Dialect::MySql, sql, "COUNT"),
            Some(SyntaxTokenKind::Function),
        );
    }

    #[test]
    fn unterminated_block_comment_runs_to_the_end() {
        let (tokens, exit) = classify_line(
            Dialect::SqlServer,
            "SELECT 1 /* oops",
            CommentState::default(),
        );
        assert!(exit.is_inside());
        assert_eq!(
            tokens.last().map(|t| t.kind),
            Some(SyntaxTokenKind::Comment)
        );
    }

    #[test]
    fn classifies_functions() {
        let tokens = classify("SELECT COUNT(*) FROM t");
        let kinds: Vec<_> = tokens.iter().map(|t| t.kind).collect();
        assert!(kinds.contains(&SyntaxTokenKind::Function));
    }

    #[test]
    fn classifies_variables() {
        let tokens = classify("SELECT @@some_setting, @id");
        let vars: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == SyntaxTokenKind::Variable)
            .collect();
        assert_eq!(vars.len(), 2);
    }

    #[test]
    fn classifies_numbers() {
        let tokens = classify("WHERE age > 42 AND ratio = 3.14");
        let nums: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == SyntaxTokenKind::Number)
            .collect();
        assert_eq!(nums.len(), 2);
    }

    #[test]
    fn classifies_strings() {
        let tokens = classify("WHERE name = 'O''Brien'");
        let strs: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == SyntaxTokenKind::String)
            .collect();
        assert_eq!(strs.len(), 1);
        let s = &strs[0];
        let text = &"WHERE name = 'O''Brien'"[s.start..s.end];
        assert_eq!(text, "'O''Brien'");
    }

    #[test]
    fn classifies_line_comments() {
        let tokens = classify("SELECT 1 -- this is a comment");
        let comments: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == SyntaxTokenKind::Comment)
            .collect();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].end, "SELECT 1 -- this is a comment".len());
    }

    #[test]
    fn classifies_block_comments() {
        let tokens = classify("SELECT /* inline */ 1");
        let comments: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == SyntaxTokenKind::Comment)
            .collect();
        assert_eq!(comments.len(), 1);
    }

    #[test]
    fn classifies_nested_block_comments() {
        let tokens = classify("/* outer /* inner */ still outer */");
        let comments: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == SyntaxTokenKind::Comment)
            .collect();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].end, "/* outer /* inner */ still outer */".len());
    }

    #[test]
    fn keyword_inside_string_is_string() {
        let tokens = classify("SET @x = 'SELECT FROM WHERE'");
        // The string token should cover the whole quoted part.
        let strs: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == SyntaxTokenKind::String)
            .collect();
        assert_eq!(strs.len(), 1);
    }

    #[test]
    fn keyword_inside_comment_is_comment() {
        let tokens = classify("-- SELECT FROM\nSELECT 1");
        let comments: Vec<_> = tokens
            .iter()
            .filter(|t| t.kind == SyntaxTokenKind::Comment)
            .collect();
        assert_eq!(comments.len(), 1);
        let text_in_comment = &"-- SELECT FROM\nSELECT 1"[comments[0].start..comments[0].end];
        assert!(text_in_comment.contains("SELECT"));
    }

    #[test]
    fn token_labels() {
        assert_eq!(SyntaxTokenKind::Keyword.label(), "keyword");
        assert_eq!(SyntaxTokenKind::Comment.label(), "comment");
    }

    /// Tokens must be disjoint and sorted: consumers convert them into text
    /// runs whose lengths sum to the text length, and an overlap aborts the
    /// app inside the shaper (issue seen with numbers/keywords in strings).
    fn assert_disjoint_sorted(text: &str) {
        let tokens = classify(text);
        let mut prev_end = 0;
        for t in &tokens {
            assert!(
                t.start >= prev_end,
                "token {:?} overlaps previous (prev_end {}) in {:?}",
                t,
                prev_end,
                text
            );
            assert!(t.end <= text.len());
            assert!(t.start < t.end);
            prev_end = t.end;
        }
    }

    #[test]
    fn number_inside_string_does_not_overlap() {
        assert_disjoint_sorted("SELECT 1 AS n, 'row 1' AS label FROM t");
    }

    #[test]
    fn keyword_inside_string_does_not_overlap() {
        assert_disjoint_sorted("SELECT 'row' AS label FROM sys.objects");
    }

    #[test]
    fn number_inside_comment_does_not_overlap() {
        assert_disjoint_sorted("SELECT x FROM t; -- line 1");
    }

    #[test]
    fn keyword_inside_comment_does_not_overlap() {
        assert_disjoint_sorted("/* SELECT 42 FROM t */ SELECT 'a 1 b'");
    }

    #[test]
    fn string_wins_over_inner_tokens() {
        let text = "SET @x = 'SELECT 42'";
        let tokens = classify(text);
        let quote = text.find('\'').unwrap();
        // Everything inside the quotes must be classified String, never
        // Keyword or Number.
        for t in &tokens {
            if t.start >= quote {
                assert_eq!(t.kind, SyntaxTokenKind::String, "token {:?}", t);
            }
        }
    }

    #[test]
    fn empty_text() {
        let tokens = classify("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn words_that_do_double_duty_follow_their_context() {
        let join = "SELECT a FROM t LEFT JOIN u ON t.id = u.id";
        assert_eq!(kind_of(join, "LEFT"), Some(SyntaxTokenKind::Keyword));
        let call = "SELECT LEFT(name, 3) FROM t";
        assert_eq!(kind_of(call, "LEFT"), Some(SyntaxTokenKind::Function));
        // `CHAR(` is a function, but after AS it names a type.
        assert_eq!(
            kind_of("SELECT CHAR(65)", "CHAR"),
            Some(SyntaxTokenKind::Function)
        );
        assert_eq!(
            kind_of("SELECT CAST(x AS char(10))", "char"),
            Some(SyntaxTokenKind::Type)
        );
    }

    #[test]
    fn literal_words_are_constants_not_keywords() {
        assert_eq!(
            kind_of("SELECT a FROM t WHERE b IS NULL", "NULL"),
            Some(SyntaxTokenKind::Constant)
        );
        for dialect in [Dialect::Postgres, Dialect::MySql, Dialect::Sqlite] {
            assert_eq!(
                kind_in(dialect, "SELECT TRUE, false", "TRUE"),
                Some(SyntaxTokenKind::Constant),
                "{dialect:?}"
            );
            assert_eq!(
                kind_in(dialect, "SELECT TRUE, false", "false"),
                Some(SyntaxTokenKind::Constant),
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn function_names_without_a_call_are_plain_unless_niladic() {
        // A column that happens to share a function's name.
        assert_eq!(kind_of("SELECT year, age FROM t", "year"), None);
        assert_eq!(
            kind_of("SELECT CURRENT_TIMESTAMP", "CURRENT_TIMESTAMP"),
            Some(SyntaxTokenKind::Function)
        );
        assert_eq!(
            kind_in(Dialect::Oracle, "SELECT SYSDATE FROM dual", "SYSDATE"),
            Some(SyntaxTokenKind::Function)
        );
    }

    #[test]
    fn qualified_name_parts_are_plain() {
        assert_eq!(kind_of("SELECT t.date, t.count FROM t", "date"), None);
        assert_eq!(kind_of("SELECT t.date, t.count FROM t", "count"), None);
    }

    #[test]
    fn at_at_system_functions_are_functions() {
        assert_eq!(
            kind_of("SELECT @@ROWCOUNT, @id", "@@ROWCOUNT"),
            Some(SyntaxTokenKind::Function)
        );
        assert_eq!(
            kind_of("SELECT @@ROWCOUNT, @id", "@id"),
            Some(SyntaxTokenKind::Variable)
        );
    }

    #[test]
    fn each_dialect_knows_its_own_vocabulary() {
        for (dialect, sql, needle, kind) in [
            (
                Dialect::Postgres,
                "SELECT now()",
                "now",
                SyntaxTokenKind::Function,
            ),
            (
                Dialect::Postgres,
                "WHERE a ILIKE 'x%'",
                "ILIKE",
                SyntaxTokenKind::Keyword,
            ),
            (
                Dialect::Postgres,
                "SELECT x::jsonb",
                "jsonb",
                SyntaxTokenKind::Type,
            ),
            (
                Dialect::MySql,
                "SELECT 1 LIMIT 5",
                "LIMIT",
                SyntaxTokenKind::Keyword,
            ),
            (
                Dialect::MySql,
                "SELECT IFNULL(a, 0)",
                "IFNULL",
                SyntaxTokenKind::Function,
            ),
            (
                Dialect::Sqlite,
                "SELECT strftime('%Y', d)",
                "strftime",
                SyntaxTokenKind::Function,
            ),
            (
                Dialect::Oracle,
                "SELECT NVL(a, 0) FROM dual",
                "NVL",
                SyntaxTokenKind::Function,
            ),
            (
                Dialect::Oracle,
                "SELECT * FROM t CONNECT BY PRIOR id = pid",
                "CONNECT",
                SyntaxTokenKind::Keyword,
            ),
            (
                Dialect::SqlServer,
                "SELECT TOP 5 * FROM t",
                "TOP",
                SyntaxTokenKind::Keyword,
            ),
            (
                Dialect::SqlServer,
                "DECLARE @x NVARCHAR(10)",
                "NVARCHAR",
                SyntaxTokenKind::Type,
            ),
            (
                Dialect::SqlServer,
                "SELECT ISNULL(a, 0)",
                "ISNULL",
                SyntaxTokenKind::Function,
            ),
        ] {
            assert_eq!(
                kind_in(dialect, sql, needle),
                Some(kind),
                "{dialect:?}: {needle} in {sql}"
            );
        }
    }

    #[test]
    fn a_quote_inside_a_delimited_identifier_opens_no_string() {
        // The user's report: a `'` inside backticks used to open a string
        // that swallowed the DELETE after it.
        let sql = "SELECT 1 AS `'`; DELETE FROM t; SELECT 1 AS `'`";
        assert_eq!(
            kind_in(Dialect::MySql, sql, "DELETE"),
            Some(SyntaxTokenKind::Keyword)
        );
        assert_eq!(
            kind_in(Dialect::Sqlite, sql, "DELETE"),
            Some(SyntaxTokenKind::Keyword)
        );
        let brackets = "SELECT [it's] FROM t WHERE x = 1";
        assert_eq!(
            kind_in(Dialect::SqlServer, brackets, "WHERE"),
            Some(SyntaxTokenKind::Keyword)
        );
        let dq = "SELECT \"it's -- here\" FROM t";
        assert_eq!(
            kind_in(Dialect::Postgres, dq, "FROM"),
            Some(SyntaxTokenKind::Keyword)
        );
    }

    #[test]
    fn words_inside_delimited_identifiers_are_names() {
        assert_eq!(
            kind_in(Dialect::SqlServer, "SELECT [select] FROM t", "select]"),
            None
        );
        assert_eq!(
            kind_in(Dialect::MySql, "SELECT `count` FROM t", "count"),
            None
        );
        assert_eq!(
            kind_in(Dialect::Postgres, "SELECT \"order\" FROM t", "order"),
            None
        );
    }

    #[test]
    fn each_dialect_bounds_its_own_string_forms() {
        // MySQL: double quotes are strings and backslash escapes the quote.
        let my = r#"SELECT "a\" FROM", 'b\'' FROM t"#;
        assert_eq!(
            kind_in(Dialect::MySql, my, "FROM\""),
            Some(SyntaxTokenKind::String)
        );
        assert_eq!(
            kind_in(Dialect::MySql, my, "FROM t"),
            Some(SyntaxTokenKind::Keyword)
        );
        // Postgres: E'' escapes, and dollar quoting.
        let pg = r"SELECT E'it\'s FROM', $fn$ don't FROM $fn$ FROM t";
        assert_eq!(
            kind_in(Dialect::Postgres, pg, "FROM'"),
            Some(SyntaxTokenKind::String)
        );
        assert_eq!(
            kind_in(Dialect::Postgres, pg, "don't"),
            Some(SyntaxTokenKind::String)
        );
        assert_eq!(
            kind_in(Dialect::Postgres, pg, "FROM t"),
            Some(SyntaxTokenKind::Keyword)
        );
        // `$1` is a parameter, not a dollar quote.
        assert_eq!(
            kind_in(Dialect::Postgres, "SELECT $1 FROM t", "FROM"),
            Some(SyntaxTokenKind::Keyword)
        );
        // Oracle: q'[…]' may contain lone quotes.
        let ora = "SELECT q'[it's FROM]' FROM dual";
        assert_eq!(
            kind_in(Dialect::Oracle, ora, "it's"),
            Some(SyntaxTokenKind::String)
        );
        assert_eq!(
            kind_in(Dialect::Oracle, ora, "FROM dual"),
            Some(SyntaxTokenKind::Keyword)
        );
        // A national-character prefix belongs to its literal.
        assert_eq!(
            kind_in(Dialect::SqlServer, "SELECT N'x' FROM t", "N'x'"),
            Some(SyntaxTokenKind::String)
        );
        // Standard SQL: backslash is an ordinary character.
        assert_eq!(
            kind_in(Dialect::SqlServer, r"SELECT 'a\' FROM t", "FROM"),
            Some(SyntaxTokenKind::Keyword)
        );
    }

    fn names() -> SemanticNames {
        use sqlly_intel::intellisense::{CompletionCatalog, CompletionObject};
        let mut proc_ = CompletionObject::table("dbo", "usp_Load", Vec::new());
        proc_.kind = sqlly_core::ObjectKind::StoredProcedure;
        let mut func = CompletionObject::table("dbo", "fn_Total", Vec::new());
        func.kind = sqlly_core::ObjectKind::ScalarFunction;
        let mut ty = CompletionObject::table("dbo", "Money2", Vec::new());
        ty.kind = sqlly_core::ObjectKind::Type;
        SemanticNames::from_catalog(&CompletionCatalog {
            schemas: vec!["sales".into()],
            objects: vec![
                CompletionObject::table("sales", "Orders", Vec::new()),
                proc_,
                func,
                ty,
            ],
            ..Default::default()
        })
    }

    #[test]
    fn semantic_spans_name_schemas_tables_routines_and_types() {
        let line =
            "EXEC usp_Load; SELECT dbo.fn_Total(x) FROM sales.[Orders] CAST(1 AS Money2) -- orders";
        let tokens = classify_line(Dialect::SqlServer, line, CommentState::default()).0;
        let spans: Vec<(&str, SemanticKind)> = semantic_spans(line, &tokens, &names())
            .into_iter()
            .map(|(s, e, k)| (&line[s..e], k))
            .collect();
        assert_eq!(
            spans,
            vec![
                ("usp_Load", SemanticKind::Procedure),
                ("dbo", SemanticKind::Schema),
                ("fn_Total", SemanticKind::UserFunction),
                ("sales", SemanticKind::Schema),
                ("[Orders]", SemanticKind::Table),
                ("Money2", SemanticKind::UserType),
            ]
        );
    }

    #[test]
    fn semantic_spans_skip_strings_variables_and_unknown_words() {
        let line = "SELECT 'Orders', @Orders, Orders2 FROM t";
        let tokens = classify_line(Dialect::SqlServer, line, CommentState::default()).0;
        assert!(semantic_spans(line, &tokens, &names()).is_empty());
        assert!(semantic_spans("Orders", &[], &SemanticNames::default()).is_empty());
    }
}
