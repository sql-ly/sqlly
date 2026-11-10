//! Statement scoping for completion.
//!
//! A document can hold several statements (separated by `;` or a `GO` batch
//! separator). Completion at the cursor must only see the table sources and
//! columns of the statement the cursor sits in — a fresh `SELECT` is a fresh
//! context, so the previous statement's FROM tables must not leak in as join
//! sources or in-scope columns.
//!
//! The parser stamps every [`SymbolUse`](sqlly_lang::sqlparse::SymbolUse) with the
//! index of its owning statement, so scoping is a filter on that index once we
//! know which statement contains the cursor.

use sqlly_lang::sqlmodel::Dialect;
use sqlly_lang::sqlparse::{ParsedTree, StatementNode};

/// Byte offset where a top-level statement begins.
fn statement_start(stmt: &StatementNode) -> usize {
    match stmt {
        StatementNode::CreateTable(n) => n.span.start,
        StatementNode::CreateView(n) => n.span.start,
        StatementNode::CreateRoutine(n) => n.span.start,
        StatementNode::Select(n) => n.span.start,
        StatementNode::Insert(n) => n.span.start,
        StatementNode::Update(n) => n.span.start,
        StatementNode::Delete(n) => n.span.start,
        StatementNode::Exec(n) => n.span.start,
        StatementNode::Opaque { span, .. } => span.start,
    }
}

/// Index (into [`ParsedTree::statements`]) of the statement that contains
/// `cursor`: the last statement that begins inside the caret's splitter
/// region (see [`crate::intellisense::statement_region`]) at or before the
/// cursor. Returns `None` when the cursor precedes the first statement —
/// callers treat that as "do not scope" so an unparsed lead never hides
/// otherwise-valid candidates.
///
/// The parser produces one node per splitter region, so when the caret's
/// region produced none (a bare head keyword still being typed, say
/// `merge ▏`, parses to nothing) the result is [`NO_STATEMENT`], which
/// matches no use: the previous statement is out of scope even though it is
/// the last node before the cursor (feedback ifb_3798). A routine definition
/// is one region, body and all, so inside it the routine's node is the scope.
///
/// Statement `.span`s cover only the statement head (not the full extent), so
/// this keys off start offsets in source order rather than span containment.
pub(crate) fn cursor_statement_index(
    tree: &ParsedTree,
    text: &str,
    cursor: usize,
    dialect: Dialect,
) -> Option<usize> {
    let region = crate::intellisense::statement_region(text, cursor, dialect);
    let starts_before_cursor = |s: &StatementNode| statement_start(s) <= cursor;
    tree.statements
        .iter()
        .enumerate()
        .filter(|(_, s)| starts_before_cursor(s) && statement_start(s) >= region.start)
        .map(|(i, _)| i)
        .next_back()
        .or_else(|| {
            tree.statements
                .iter()
                .any(starts_before_cursor)
                .then_some(NO_STATEMENT)
        })
}

/// A statement index no use carries: the caret's statement has no node.
pub(crate) const NO_STATEMENT: usize = usize::MAX;

/// Whether a use stamped with `use_stmt` is in scope for a cursor resolved to
/// `cursor_stmt`. When `cursor_stmt` is `None` (see [`cursor_statement_index`]),
/// everything is in scope — scoping degrades to the previous, unscoped behavior
/// rather than dropping candidates.
pub(crate) fn use_in_scope(use_stmt: usize, cursor_stmt: Option<usize>) -> bool {
    match cursor_stmt {
        Some(i) => use_stmt == i,
        None => true,
    }
}
