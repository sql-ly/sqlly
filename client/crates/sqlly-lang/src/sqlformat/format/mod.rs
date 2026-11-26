//! SQL formatter engine.
//!
//! Consumes the full [`SqlFormattingPreferences`] surface (until now a pure
//! data model) and pretty-prints SQL on top of the `sqlly-sqllex`
//! full-fidelity token stream. Token-based and structure-aware rather than
//! AST-based: a lightweight paren tree plus clause segmentation is enough to
//! honor every layout option while remaining robust on SQL that no parser in
//! this workspace fully models (vendor DDL, hints, partial input).
//!
//! # Safety contract
//!
//! The formatter must never lose or corrupt SQL. Guarantees, in order:
//!
//! 1. Input that fails to lex cleanly (unterminated strings/comments/illegal
//!    chars) is returned unchanged — broken SQL is not reflowed.
//! 2. After formatting, the output is re-lexed and compared token-by-token
//!    against the input (modulo the transforms the active options permit:
//!    case changes, quote changes, parens inserted by
//!    `parenthesize_mixed_boolean_logic`, whitespace inside block comments
//!    when `format_inside_block_comments`). Any mismatch — a dropped comment,
//!    a mangled literal — makes [`format_sql`] return the input unchanged.
//! 3. Comments are never dropped; `-- formatter:off` / `-- formatter:on`
//!    regions (prefix configurable) pass through verbatim.
//!
//! Idempotency (`format(format(x)) == format(x)`) is asserted by tests across
//! a corpus × every preset.

mod casing;
mod emit;
mod procedural;
#[cfg(test)]
mod procedural_tests;
mod segment;
#[cfg(test)]
mod tests;
mod tree;
mod verify;
mod writer;

use crate::sqlformat::prefs::{IndentStyle, SqlDialect, SqlFormattingPreferences};
use crate::sqllex::{IdentQuote, LexErrorKind, ParamStyle, StrictLexer, Token, TokenKind};
use crate::sqlmodel::{
    Dialect, is_builtin_function, is_builtin_function_in_any_dialect, is_builtin_type,
    is_builtin_type_in_any_dialect,
};
use emit::Formatter;
use tree::collect_items;
use verify::tokens_equivalent;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// The editor's own indentation settings, used when `use_editor_indentation`
/// is on. Callers without an editor at hand pass `None` and the formatter's
/// §18 settings apply.
#[derive(Debug, Clone, Copy)]
pub struct EditorIndent {
    pub style: IndentStyle,
    pub width: i32,
}

/// Format `input` under `prefs`. Infallible: on any safety-check failure the
/// input comes back unchanged.
pub fn format_sql(
    input: &str,
    prefs: &SqlFormattingPreferences,
    editor_indent: Option<EditorIndent>,
) -> String {
    format_sql_report(input, prefs, editor_indent).output
}

/// Why [`format_sql`] handed its input back untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatDecline {
    /// The input does not lex cleanly (an unterminated string, quoted name
    /// or block comment, or a character the dialect does not allow), so it
    /// was not reflowed. `offset` is the byte offset where the bad token
    /// starts.
    LexError { kind: LexErrorKind, offset: usize },
    /// Empty or whitespace-only input: nothing to lay out.
    Empty,
    /// The formatted text did not re-lex to the same SQL (safety contract
    /// guarantee 2), so the input was kept.
    SafetyCheck,
}

/// [`format_sql`]'s result plus how it got there, for surfaces that explain
/// the outcome (the web formatter's status line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatReport {
    /// The formatted SQL, or the input unchanged when `declined` is set.
    pub output: String,
    /// The lexer dialect the input was read with (`AutoDetect` resolved).
    pub dialect: Dialect,
    /// Set when the formatter returned the input unchanged on purpose.
    /// `None` with `output == input` means the SQL was already formatted.
    pub declined: Option<FormatDecline>,
}

/// [`format_sql`], reporting why the input came back unchanged when it did.
pub fn format_sql_report(
    input: &str,
    prefs: &SqlFormattingPreferences,
    editor_indent: Option<EditorIndent>,
) -> FormatReport {
    let (dialect, probe_tokens) = resolve_dialect(input, prefs);
    let unchanged = |declined| FormatReport {
        output: input.to_string(),
        dialect,
        declined: Some(declined),
    };
    let tokens = probe_tokens.unwrap_or_else(|| StrictLexer::tokenize(input, dialect));
    if let Some((kind, offset)) = tokens.iter().find_map(|t| match t.kind {
        TokenKind::Error(kind) => Some((kind, t.span.start)),
        _ => None,
    }) {
        return unchanged(FormatDecline::LexError { kind, offset });
    }

    let surrounding_upper = surrounding_style_is_upper(&tokens);
    let opts = Options::resolve(prefs, editor_indent, dialect, surrounding_upper);

    let items = collect_items(input, &tokens, dialect);
    if items.is_empty() {
        return unchanged(FormatDecline::Empty);
    }

    let out = Formatter::new(input, &opts).run(&items);

    // Safety net: the output must contain the same SQL.
    let out_tokens = StrictLexer::tokenize(&out, opts.dialect);
    if tokens_equivalent(&tokens, &out_tokens, prefs) {
        FormatReport {
            output: out,
            dialect,
            declined: None,
        }
    } else {
        debug_assert!(
            false,
            "sql_formatter safety check failed; returning input unchanged.\ninput: {input:?}\noutput: {out:?}"
        );
        unchanged(FormatDecline::SafetyCheck)
    }
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

struct Options<'p> {
    p: &'p SqlFormattingPreferences,
    dialect: Dialect,
    indent_style: IndentStyle,
    indent_width: i32,
    /// Resolution of `KeywordCase::MatchSurroundingStyle`: true when the
    /// input's existing keywords are mostly uppercase.
    surrounding_upper: bool,
}

impl<'p> Options<'p> {
    fn resolve(
        p: &'p SqlFormattingPreferences,
        editor_indent: Option<EditorIndent>,
        dialect: Dialect,
        surrounding_upper: bool,
    ) -> Self {
        let (indent_style, indent_width) = match (p.use_editor_indentation, editor_indent) {
            (true, Some(e)) => (e.style, e.width.clamp(1, 16)),
            _ => (p.indent_style, p.indent_width()),
        };
        Self {
            p,
            dialect,
            indent_style,
            indent_width,
            surrounding_upper,
        }
    }

    /// Settings whose engine has no catalog of its own (BigQuery,
    /// Snowflake, Redshift), or whose dialect is only a guess, check every
    /// dialect's built-in names.
    fn checks_every_catalog(&self) -> bool {
        matches!(
            self.p.dialect,
            SqlDialect::AutoDetect
                | SqlDialect::Bigquery
                | SqlDialect::Snowflake
                | SqlDialect::Redshift
        )
    }

    /// Whether a call target spelled `name` is a built-in (system)
    /// function. Only those take the function casing; a user's function
    /// keeps the spelling it was written with.
    fn is_system_function(&self, name: &str) -> bool {
        if casing::word_in(casing::GRAMMAR_CALL_WORDS, name) {
            true
        } else if self.checks_every_catalog() {
            is_builtin_function_in_any_dialect(name)
        } else {
            is_builtin_function(self.dialect, name)
        }
    }

    /// Whether `name` in a type position is a built-in data type. A
    /// user-defined type (`OrderIdList`, `dbo.Money2`) keeps its spelling.
    fn is_system_type(&self, name: &str) -> bool {
        casing::word_in(casing::TYPE_WORDS, name)
            || if self.checks_every_catalog() {
                is_builtin_type_in_any_dialect(name)
            } else {
                is_builtin_type(self.dialect, name)
            }
    }

    fn indent_unit(&self) -> String {
        match self.indent_style {
            IndentStyle::Tabs => "\t".to_string(),
            IndentStyle::Spaces => " ".repeat(self.indent_width as usize),
        }
    }

    /// Visual width of one indent unit (tabs count as `indent_width`).
    fn indent_unit_width(&self) -> usize {
        self.indent_width as usize
    }

    fn max_line(&self) -> usize {
        let m = self.p.max_line_length();
        if m == 0 { usize::MAX } else { m as usize }
    }
}

/// The lexer dialect the formatter uses for `input` under the user-facing
/// `dialect` setting (`AutoDetect` sniffs the input). Lets callers that
/// minify rather than format (the web formatter) lex exactly as the formatter
/// would.
pub fn lex_dialect_for(input: &str, dialect: SqlDialect) -> Dialect {
    match dialect {
        SqlDialect::AutoDetect => detect_dialect(input).0,
        fixed => fixed_dialect(fixed),
    }
}

fn resolve_dialect<'a>(
    input: &'a str,
    prefs: &SqlFormattingPreferences,
) -> (Dialect, Option<Vec<Token<'a>>>) {
    match prefs.dialect {
        SqlDialect::AutoDetect => detect_dialect(input),
        fixed => (fixed_dialect(fixed), None),
    }
}

/// Lexer dialect for every setting except `AutoDetect` (which needs the input).
fn fixed_dialect(dialect: SqlDialect) -> Dialect {
    match dialect {
        SqlDialect::AutoDetect
        | SqlDialect::Postgresql
        | SqlDialect::Snowflake
        | SqlDialect::Redshift => Dialect::Postgres,
        SqlDialect::Mysql | SqlDialect::Bigquery => Dialect::MySql,
        SqlDialect::Sqlite => Dialect::Sqlite,
        SqlDialect::Tsql => Dialect::SqlServer,
        SqlDialect::Oracle => Dialect::Oracle,
    }
}

/// Best-effort dialect sniffing for `SqlDialect::AutoDetect`. When the probe
/// lexer's dialect matches the detected dialect, the token vec is returned for
/// reuse by the main format pass.
fn detect_dialect<'a>(input: &'a str) -> (Dialect, Option<Vec<Token<'a>>>) {
    let probe = StrictLexer::tokenize(input, Dialect::SqlServer);
    let mut prev_word: Option<&str> = None;
    for t in &probe {
        match t.kind {
            TokenKind::Ident(IdentQuote::Bracket) => return (Dialect::SqlServer, Some(probe)),
            TokenKind::ParamMarker(ParamStyle::AtName) => return (Dialect::SqlServer, Some(probe)),
            // A `#temp` / `##global` table where a table name goes. Read as
            // PostgreSQL the `#` is an operator and `CREATE TABLE #t` comes
            // back as `# t`, which no longer runs.
            TokenKind::Ident(IdentQuote::None)
                if t.text.starts_with('#')
                    && prev_word.is_some_and(|w| casing::word_in(TEMP_TABLE_LEADERS, w)) =>
            {
                return (Dialect::SqlServer, Some(probe));
            }
            TokenKind::Whitespace | TokenKind::Comment(_) => continue,
            // `dbo.Orders`: SQL Server's default schema.
            TokenKind::Dot if prev_word.is_some_and(|w| w.eq_ignore_ascii_case("dbo")) => {
                return (Dialect::SqlServer, Some(probe));
            }
            // `NVARCHAR(MAX)`, `SET NOCOUNT ON`, `UNIQUEIDENTIFIER`: words
            // only T-SQL spells. Read as PostgreSQL, `ALTER TABLE … add` and
            // a trigger's `after` are plain names and keep the typed case.
            _ if t.is_word() && casing::word_in(TSQL_ONLY_WORDS, t.text) => {
                return (Dialect::SqlServer, Some(probe));
            }
            // `AS BEGIN` / `ELSE BEGIN`: a T-SQL block (PostgreSQL bodies sit
            // in a dollar-quoted string, MySQL ones follow `)` or `DO`), and
            // `CREATE PROC`.
            _ if t.is_word()
                && prev_word.is_some_and(|w| {
                    if t.text.eq_ignore_ascii_case("begin") {
                        w.eq_ignore_ascii_case("as") || w.eq_ignore_ascii_case("else")
                    } else {
                        t.text.eq_ignore_ascii_case("proc")
                            && casing::word_in(&["create", "alter", "drop"], w)
                    }
                }) =>
            {
                return (Dialect::SqlServer, Some(probe));
            }
            _ => {}
        }
        prev_word = t.is_word().then_some(t.text);
    }
    // `GO` on its own line is the T-SQL batch separator.
    if segment::has_batch_separator(&collect_items(input, &probe, Dialect::SqlServer)) {
        return (Dialect::SqlServer, Some(probe));
    }
    let probe = StrictLexer::tokenize(input, Dialect::MySql);
    if probe
        .iter()
        .any(|t| matches!(t.kind, TokenKind::Ident(IdentQuote::Backtick)))
    {
        return (Dialect::MySql, Some(probe));
    }
    // A `DELIMITER //` client line: a MySQL routine script.
    let mut line_start = true;
    for t in &probe {
        match t.kind {
            TokenKind::Whitespace => line_start |= t.text.contains('\n'),
            _ if line_start && t.is_word() && t.text.eq_ignore_ascii_case("delimiter") => {
                return (Dialect::MySql, Some(probe));
            }
            _ => line_start = false,
        }
    }
    // A `?` placeholder (`WHERE id=?`) would glue into a PostgreSQL operator
    // (`=?`), so it reads as MySQL unless the text also shows PostgreSQL
    // (`::` casts, the `?|` / `?&` jsonb operators).
    if probe
        .iter()
        .any(|t| matches!(t.kind, TokenKind::ParamMarker(ParamStyle::Question)))
        && !input.contains("::")
        && !input.contains("?|")
        && !input.contains("?&")
    {
        return (Dialect::MySql, Some(probe));
    }
    (Dialect::Postgres, None)
}

/// Type and option names no other engine has.
const TSQL_ONLY_WORDS: &[&str] = &[
    "nvarchar",
    "datetime2",
    "datetimeoffset",
    "smalldatetime",
    "uniqueidentifier",
    "nocount",
    "xact_abort",
];

/// Words a T-SQL `#temp` table name directly follows.
const TEMP_TABLE_LEADERS: &[&str] = &[
    "table", "into", "from", "join", "update", "exists", "apply", "insert", "delete",
];

fn surrounding_style_is_upper(tokens: &[Token<'_>]) -> bool {
    let mut upper = 0usize;
    let mut lower = 0usize;
    for t in tokens {
        if matches!(t.kind, TokenKind::Keyword) {
            if t.text.chars().any(|c| c.is_ascii_lowercase()) {
                lower += 1;
            } else if t.text.chars().any(|c| c.is_ascii_uppercase()) {
                upper += 1;
            }
        }
    }
    upper >= lower
}
