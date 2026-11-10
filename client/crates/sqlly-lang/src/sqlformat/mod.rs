//! # sqlly-sqlformat
//!
//! The token-based, dialect-aware SQL formatter and its preference types.
//! Extracted from the GPUI app so external-editor surfaces (the LSP's
//! `textDocument/formatting`) format identically to the in-app editor. The
//! formatter is infallible by contract: a re-lex safety net returns the input
//! unchanged whenever the output would not tokenize to the same SQL.

#![forbid(unsafe_code)]

mod format;
mod minify;
mod prefs;

pub use format::{
    EditorIndent, FormatDecline, FormatReport, format_sql, format_sql_report, lex_dialect_for,
};
pub use minify::minify;
pub use prefs::*;
