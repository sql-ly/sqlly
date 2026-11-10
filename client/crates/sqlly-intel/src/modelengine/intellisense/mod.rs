//! IntelliSense (ADR-011 §8): cursor-aware completion over the same parse +
//! slot-resolution pipeline as validation. Symbol mode is a pure snapshot
//! lookup (no I/O, per-keystroke safe). Value mode queries the live database
//! through the profile's connection pool — explicitly invoked, never
//! per-keystroke — and pivots foreign keys to (key, display) pairs.
//!
//! Split by concern; every previously-public path is re-exported here so
//! `crate::modelengine::intellisense::…` imports are unchanged:
//! - [`types`]: request/response wire types ([`CompletionRequest`],
//!   [`CompletionItem`], …).
//! - [`context`]: cursor-context detection (the precedence-ordered ladder in
//!   `cursor_context`).
//! - [`fuzzy`] + [`scoring`]: candidate ranking.
//! - [`joins`]: FK-based join suggestions.
//! - [`set_options`]: what a statement-initial `SET` can name, per engine.
//! - [`symbols`]: symbol-mode orchestration ([`complete_symbols`]).
//! - [`values`]: value-mode planning/rendering/shaping ([`complete_values`]).

mod context;
// Pure FK value pivot: shared by native value mode and the editor's value
// lookup (which also runs in the browser build).
mod fk_pivot;
pub mod fuzzy;
mod joins;
mod scope;
mod scoring;
mod set_options;
mod symbols;
mod types;
// Value mode queries through the native connection pool.
#[cfg(not(target_arch = "wasm32"))]
mod values;

pub(crate) use context::{
    PostFromClause, after_predicate_keywords, after_select_list_keywords, post_from_clause,
};
pub use fk_pivot::{
    FkValuePivot, column_type_for, fk_insert_text, fk_value_pivot_for, resolve_display_column,
};
pub use symbols::complete_symbols;
pub(crate) use symbols::set_statement_items;
pub use types::{
    CompletionItem, CompletionItemKind, CompletionMode, CompletionRequest, Completions, MatchType,
    SortOrder, ValueCompletionParams, why_for_kind,
};
#[cfg(not(target_arch = "wasm32"))]
pub use values::{
    VALUE_LIMIT_CAP, VALUE_LIMIT_DEFAULT, ValueQueryPlan, ValueSlotResolution, complete_values,
    filter_cached_rows, plan_value_query, resolve_value_slot, shape_value_items,
};
