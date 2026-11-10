//! # sqlly-tokenizer
//!
//! Simple SQL object tokenizer. Scans `.sql` source text and extracts the
//! object model defined in [`sqlly-fsmodel`] — `CREATE TABLE/VIEW/PROCEDURE/
//! FUNCTION/TYPE` definitions with their columns (type, default, foreign key,
//! check, identity, nullability), parameters (in/out), and result sets. The
//! [`SimpleSqlTokenizer`] implements the [`SqlTokenizer`] trait
//! ([`SqlTokenizer::tokenize_source`]) and produces a [`crate::fsmodel::SqlModelSnapshot`]
//! plus diagnostics. It is intentionally a single linear pass with no
//! backtracking so it stays fast on large source trees (per AGENTS.md).
//!
//! This file is the lexical core: the raw token scanner ([`tokenize`]) and its
//! paren-balance diagnostics ([`parse_diagnostics`]). The DDL statement parser
//! that builds the model snapshot lives in [`ddl_model`]
//! ([`simple_tokenize_model`]), re-exported here.
//!
//! Two consumers build on it:
//! - [`indexer`] — directory discovery, per-file tokenization, snapshot merge,
//!   delta computation, and a debounced [`indexer::FilesystemWatcher`].
//! - `seclex` feeds it *shadows* — text in which the security lexer has
//!   already bounded every literal, quoted identifier and comment the way a
//!   real engine does — so the safety classifier can use this keyword table
//!   without ever tokenizing raw SQL (ADR-016, as amended 2026-09-28).
//!
//! This crate is the **dialect-invariant** lexer of ADR-016: it deliberately
//! takes no dialect parameter so its consumers (parameter scanning,
//! diagnostics, DDL indexing) behave identically regardless of which
//! database the app is connected to. It has no quoting rules beyond `'…'`
//! and `[…]`, so it must never bound literals for a security decision on raw
//! SQL — that is the security lexer's job (`seclex`). Dialect-aware lexing
//! lives in the `sqllex` module; do not add a dialect parameter here.

#![forbid(unsafe_code)]

use crate::fsmodel::{FileSourceRef, SqlModelSnapshot};
use serde::{Deserialize, Serialize};

mod ddl_model;
pub use ddl_model::simple_tokenize_model;

pub mod indexer;
pub use indexer::{
    FileSnapshot, FileSnapshotCache, FilesystemWatcher, IndexOutcome, discover_files,
    index_profile, tokenize_file,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TokenKind {
    Keyword,
    Identifier,
    StringLiteral,
    Number,
    Symbol,
    Comment,
    Whitespace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token<'a> {
    pub kind: TokenKind,
    pub text: &'a str,
    pub start: usize,
    pub end: usize,
}

pub trait SqlTokenizer {
    fn tokenize_source(&self, sql: &str, source: FileSourceRef) -> TokenizationResult;
}

#[derive(Debug, Clone, Default)]
pub struct SimpleSqlTokenizer;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenizationResult {
    pub snapshot: SqlModelSnapshot,
}

impl SqlTokenizer for SimpleSqlTokenizer {
    fn tokenize_source(&self, sql: &str, source: FileSourceRef) -> TokenizationResult {
        simple_tokenize_model(sql, source)
    }
}

pub fn tokenize(sql: &str) -> Vec<Token<'_>> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let start = i;
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Whitespace,
                text: &sql[start..i],
                start,
                end: i,
            });
        } else if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Comment,
                text: &sql[start..i],
                start,
                end: i,
            });
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            tokens.push(Token {
                kind: TokenKind::Comment,
                text: &sql[start..i],
                start,
                end: i,
            });
        } else if b == b'\'' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    i += 1;
                    if bytes.get(i) != Some(&b'\'') {
                        break;
                    }
                }
                i += 1;
            }
            tokens.push(Token {
                kind: TokenKind::StringLiteral,
                text: &sql[start..i],
                start,
                end: i,
            });
        } else if b.is_ascii_digit() {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            tokens.push(Token {
                kind: TokenKind::Number,
                text: &sql[start..i],
                start,
                end: i,
            });
        } else if is_identifier_start(b) || b == b'[' {
            if b == b'[' {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b']' {
                        i += 1;
                        if bytes.get(i) != Some(&b']') {
                            break;
                        }
                    }
                    i += 1;
                }
            } else {
                i += 1;
                while i < bytes.len() && is_identifier_continue(bytes[i]) {
                    i += 1;
                }
            }
            let text = &sql[start..i];
            let kind = if is_keyword(text) {
                TokenKind::Keyword
            } else {
                TokenKind::Identifier
            };
            tokens.push(Token {
                kind,
                text,
                start,
                end: i,
            });
        } else {
            i += next_char_len(sql, start);
            tokens.push(Token {
                kind: TokenKind::Symbol,
                text: &sql[start..i],
                start,
                end: i,
            });
        }
    }

    tokens
}

pub fn parse_diagnostics(sql: &str) -> Vec<String> {
    let tokens = tokenize(sql);
    let mut stack = Vec::new();
    for token in tokens
        .iter()
        .filter(|token| token.kind == TokenKind::Symbol)
    {
        match token.text {
            "(" => stack.push(token.start),
            ")" if stack.pop().is_none() => {
                return vec![format!("Unmatched ')' at byte {}", token.start)];
            }
            _ => {}
        }
    }
    if let Some(start) = stack.pop() {
        return vec![format!("Unmatched '(' at byte {start}")];
    }
    Vec::new()
}

fn next_char_len(sql: &str, start: usize) -> usize {
    sql[start..]
        .chars()
        .next()
        .map(|ch| ch.len_utf8())
        .unwrap_or(1)
}

fn is_identifier_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'#' || b == b'@'
}

fn is_identifier_continue(b: u8) -> bool {
    is_identifier_start(b) || b.is_ascii_digit() || b == b'$'
}

/// The tokenizer's keyword vocabulary, already uppercase. Deliberately NOT
/// shared with the `sqllex` module's per-dialect tables (ADR-016): this list backs
/// the dialect-invariant surfaces, and coupling it to per-dialect table edits
/// would let a dialect tweak silently shift safety classification.
const KEYWORDS: &[&str] = &[
    "SELECT",
    "WITH",
    "FROM",
    "WHERE",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "ON",
    "GROUP",
    "ORDER",
    "BY",
    "HAVING",
    "INTO",
    "INSERT",
    "UPDATE",
    "DELETE",
    "MERGE",
    "TRUNCATE",
    "CREATE",
    "OR",
    "ALTER",
    "PROC",
    "PROCEDURE",
    "VIEW",
    "TABLE",
    "TYPE",
    "FUNCTION",
    "RETURNS",
    "AS",
    "DROP",
    "EXEC",
    "EXECUTE",
    "GRANT",
    "REVOKE",
    "DENY",
    "BACKUP",
    "RESTORE",
    "USE",
    "DBCC",
    "DECLARE",
    "SET",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "TRANSACTION",
    "END",
    "TRY",
    "CATCH",
    "WAITFOR",
    "DEFAULT",
    "CONSTRAINT",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "CHECK",
    "NULL",
    "NOT",
    "IDENTITY",
    "OUTPUT",
    "OUT",
    "READONLY",
    "GO",
    "MAX",
];

const BUILTIN_TYPES: &[&str] = &[
    "BIGINT",
    "BINARY",
    "BIT",
    "CHAR",
    "DATE",
    "DATETIME",
    "DATETIME2",
    "DATETIMEOFFSET",
    "DECIMAL",
    "FLOAT",
    "GEOGRAPHY",
    "GEOMETRY",
    "HIERARCHYID",
    "IMAGE",
    "INT",
    "MONEY",
    "NCHAR",
    "NTEXT",
    "NUMERIC",
    "NVARCHAR",
    "REAL",
    "ROWVERSION",
    "SMALLDATETIME",
    "SMALLINT",
    "SMALLMONEY",
    "SQL_VARIANT",
    "TEXT",
    "TIME",
    "TIMESTAMP",
    "TINYINT",
    "UNIQUEIDENTIFIER",
    "VARBINARY",
    "VARCHAR",
    "XML",
];

/// Longest entry in either table ("UNIQUEIDENTIFIER", 16 bytes) with
/// headroom; longer words can never match and skip the fold entirely.
const MAX_TABLE_WORD_LEN: usize = 24;

fn keyword_and_type_sets() -> &'static [std::collections::HashSet<&'static [u8]>; 2] {
    static SETS: std::sync::OnceLock<[std::collections::HashSet<&'static [u8]>; 2]> =
        std::sync::OnceLock::new();
    SETS.get_or_init(|| {
        let set = |words: &'static [&'static str]| {
            words
                .iter()
                .map(|w| w.as_bytes())
                .collect::<std::collections::HashSet<_>>()
        };
        [set(KEYWORDS), set(BUILTIN_TYPES)]
    })
}

/// Allocation-free membership test against one of the tables above: the word
/// is case-folded into a stack buffer and looked up by bytes. `is_keyword`
/// runs once per word token on the tokenize hot path, so it must not allocate
/// per call (AGENTS.md keeps the tokenizer performance-critical).
fn folded_lookup(text: &str, table: usize) -> bool {
    if text.len() > MAX_TABLE_WORD_LEN {
        return false;
    }
    let mut buf = [0u8; MAX_TABLE_WORD_LEN];
    for (dst, b) in buf.iter_mut().zip(text.as_bytes()) {
        *dst = b.to_ascii_uppercase();
    }
    keyword_and_type_sets()[table].contains(&buf[..text.len()])
}

fn is_keyword(text: &str) -> bool {
    folded_lookup(text, 0)
}

pub(crate) fn is_builtin_type(name: &str) -> bool {
    folded_lookup(name, 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The folded lookup assumes every table entry is uppercase and within
    /// [`MAX_TABLE_WORD_LEN`]; a violating entry would silently never match.
    #[test]
    fn lookup_tables_are_uppercase_and_bounded() {
        for word in KEYWORDS.iter().chain(BUILTIN_TYPES) {
            assert!(
                word.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "table entry {word:?} is not uppercase"
            );
            assert!(
                word.len() <= MAX_TABLE_WORD_LEN,
                "table entry {word:?} exceeds MAX_TABLE_WORD_LEN"
            );
        }
        // Case-insensitive at every case mix, and oversized words bail out.
        assert!(is_keyword("select") && is_keyword("Select") && is_keyword("SELECT"));
        assert!(is_builtin_type("uniqueidentifier"));
        assert!(!is_keyword("not_a_keyword_and_much_too_long_to_fold"));
    }

    #[test]
    fn tokenizes_comments_strings_and_identifiers() {
        let tokens = tokenize("SELECT [a]]b], 'it''s' -- ok");
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Keyword && token.text == "SELECT")
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::Identifier && token.text == "[a]]b]")
        );
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::StringLiteral)
        );
        assert!(tokens.iter().any(|token| token.kind == TokenKind::Comment));
    }

    #[test]
    fn detects_unmatched_parentheses() {
        assert!(parse_diagnostics("SELECT (1")[0].contains("Unmatched '("));
        assert!(parse_diagnostics("SELECT 1").is_empty());
    }

    #[test]
    fn tokenizer_handles_large_input_linearly() {
        let sql = "SELECT a FROM dbo.t WHERE a = 1;\n".repeat(10_000);
        let tokens = tokenize(&sql);
        assert!(tokens.len() > 50_000);
        assert_eq!(tokens.first().unwrap().text, "SELECT");
        assert_eq!(tokens.last().unwrap().text, "\n");
    }

    #[test]
    fn tokenizer_large_buffer_stays_within_time_budget() {
        // Coarse regression tripwire for AGENTS.md's performance mandate — NOT a
        // benchmark. Roughly 1.5 MB of SQL must tokenize in a small fraction of
        // a second even in a debug build; the ceiling is deliberately generous
        // (a healthy run is a few tens of milliseconds) so ordinary CI load
        // never trips it, while a non-linear regression would blow past it by
        // orders of magnitude and fail here loudly.
        use std::time::{Duration, Instant};
        let sql = "SELECT a, b, c FROM dbo.some_table WHERE a = 1 AND b <> 'x';\n".repeat(25_000);
        assert!(sql.len() > 1_000_000, "sanity: buffer is multi-MB");
        let started = Instant::now();
        let tokens = tokenize(&sql);
        let elapsed = started.elapsed();
        assert!(tokens.len() > 100_000, "expected many tokens");
        assert!(
            elapsed < Duration::from_millis(750),
            "tokenizing {} bytes took {elapsed:?}; expected well under 750ms \
             (possible non-linear tokenizer regression)",
            sql.len()
        );
    }

    #[test]
    fn parse_diagnostics_large_buffer_stays_within_time_budget() {
        // Same tripwire for the other hot public entry point, which runs on the
        // editor's validation path over the whole buffer.
        use std::time::{Duration, Instant};
        let sql = "SELECT a FROM dbo.t WHERE a = (1 + 2) * 3;\n".repeat(25_000);
        let started = Instant::now();
        let _ = parse_diagnostics(&sql);
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(750),
            "parse_diagnostics over {} bytes took {elapsed:?}; expected well under 750ms",
            sql.len()
        );
    }

    /// ADR-016 tripwire: this crate is the dialect-INVARIANT lexer. The exact
    /// token streams below are pinned for dialect-flavored inputs so that any
    /// future attempt to route this crate through a dialect-parameterized
    /// lexer (whose output would then vary with the connected database) fails
    /// here loudly. Do not "fix" these expectations to match the `sqllex` module;
    /// revisit ADR-016 instead.
    #[test]
    fn dialect_flavored_inputs_lex_identically_regardless_of_backend() {
        let kinds = |sql: &str| {
            tokenize(sql)
                .into_iter()
                .filter(|t| t.kind != TokenKind::Whitespace)
                .map(|t| (t.kind, t.text.to_string()))
                .collect::<Vec<_>>()
        };

        // T-SQL temp tables: `#` is an identifier char, never a comment start.
        assert_eq!(
            kinds("SELECT x FROM #temp"),
            vec![
                (TokenKind::Keyword, "SELECT".into()),
                (TokenKind::Identifier, "x".into()),
                (TokenKind::Keyword, "FROM".into()),
                (TokenKind::Identifier, "#temp".into()),
            ]
        );

        // MySQL-style `#` line comment is NOT a comment here: the DELETE after
        // it stays visible to the safety classifier (conservative direction).
        let hash_line = kinds("# DELETE FROM t");
        assert!(
            hash_line
                .iter()
                .any(|(k, t)| *k == TokenKind::Keyword && t == "DELETE"),
            "DELETE must remain visible after '#': {hash_line:?}"
        );

        // Postgres dollar-quoting is not a string form here; `$` stays a plain
        // symbol/identifier-continue byte and the body tokens stay visible.
        let dollar = kinds("$tag$ UPDATE t SET x = 1 $tag$");
        assert!(
            dollar
                .iter()
                .any(|(k, t)| *k == TokenKind::Keyword && t == "UPDATE"),
            "UPDATE inside $-quoting must remain visible: {dollar:?}"
        );

        // MySQL backtick identifiers: backticks are plain symbols, the inner
        // word is an ordinary identifier.
        assert_eq!(
            kinds("SELECT `name` FROM t"),
            vec![
                (TokenKind::Keyword, "SELECT".into()),
                (TokenKind::Symbol, "`".into()),
                (TokenKind::Identifier, "name".into()),
                (TokenKind::Symbol, "`".into()),
                (TokenKind::Keyword, "FROM".into()),
                (TokenKind::Identifier, "t".into()),
            ]
        );

        // T-SQL national strings: the N prefix is an identifier, the quoted
        // part is one string literal with doubled-quote escapes.
        assert_eq!(
            kinds("SELECT N'it''s'"),
            vec![
                (TokenKind::Keyword, "SELECT".into()),
                (TokenKind::Identifier, "N".into()),
                (TokenKind::StringLiteral, "'it''s'".into()),
            ]
        );

        // Backslash before a closing quote is a literal character (ANSI), not
        // an escape: the string ends at the first un-doubled quote.
        assert_eq!(
            kinds(r"SELECT 'a\' , 1"),
            vec![
                (TokenKind::Keyword, "SELECT".into()),
                (TokenKind::StringLiteral, r"'a\'".into()),
                (TokenKind::Symbol, ",".into()),
                (TokenKind::Number, "1".into()),
            ]
        );
    }
}
