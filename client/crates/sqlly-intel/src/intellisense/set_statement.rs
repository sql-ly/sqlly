//! Session-settings statements for the editor's completion path: after
//! `SET ▏`, `ALTER SESSION SET ▏` or `PRAGMA ▏` only the names the engine
//! can set belong in the popup — never a function or a table (feedback
//! ifb_0fbc) — and a head that is not the engine's form is steered to the
//! form it uses. The model engine owns the per-engine lists and the steering
//! rule; this adapts its items to the editor's.

use sqlly_lang::sqlmodel::Dialect;

use crate::intellisense::types::{CompletionItem, CompletionKind, CompletionSource};

/// Score of the first settings item; the rest follow in the engine's order.
const SET_STATEMENT_SCORE: i32 = 1_000;

/// The completions when `cursor` sits after a session-settings statement
/// head on `dialect`, best first, or `None` anywhere else. The steering item
/// (when the typed head is not the engine's) carries a `replace_span` that
/// covers the head, so accepting it rewrites `SET ` into `ALTER SESSION SET `
/// or `PRAGMA ` in place.
pub fn set_statement_items(
    dialect: Dialect,
    sql: &str,
    cursor: usize,
) -> Option<Vec<CompletionItem>> {
    let items = crate::modelengine::intellisense::set_statement_items(sql, cursor, dialect)?;
    Some(
        items
            .into_iter()
            .enumerate()
            .map(|(rank, item)| CompletionItem {
                label: item.label,
                insert_text: item.insert_text,
                kind: CompletionKind::Keyword,
                source: CompletionSource::Keyword,
                score: SET_STATEMENT_SCORE - rank as i32,
                detail: item.detail.unwrap_or_default(),
                replace_span: item.replace_span.map(|span| (span.start, span.end)),
            })
            .collect(),
    )
}
