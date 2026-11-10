//! # sqlly-intellisense
//!
//! Completion + symbol-index crate. Builds a searchable symbol index from the
//! filesystem object model ([`sqlly_lang::fsmodel`] schemas/tables/views/procedures/
//! functions/types and their columns) and from live server metadata, then
//! answers editor completion queries (object names, columns, schema-qualified
//! identifiers). Kept as its own crate — separate from `sqlly-query` — so the
//! completion path can be benchmarked and tuned for latency independently (per
//! AGENTS.md: the tokenizer and IntelliSense must stay performant).

#![forbid(unsafe_code)]

mod alias;
pub mod builtins;
mod catalog;
pub(crate) mod clause_insert;
mod complete;
mod quoting;
mod rank;
mod score;
mod set_statement;
mod statement;
mod types;
mod values;

pub mod goto;
pub mod hover;
pub mod signature;
pub use builtins::{
    BuiltinCatalog, BuiltinFunction, FunctionParam, FunctionSignature, WordDoc, builtins,
};
pub use goto::{
    ClickedRole, NavigationTarget, RoutineKind, SymbolResolution, navigable_span_at,
    resolve_symbol_at,
};
pub use hover::{
    HoverColumn, HoverInfo, HoverParameter, MAX_HOVER_COLUMNS, RoutineSignature, builtin_hover_at,
    hover_at,
};
pub use signature::{
    CallContext, SignatureHelp, call_context_at, signature_help_at, signature_help_for,
};

pub use alias::{AliasStyle, is_reserved_alias_word, suggest_alias};
pub use catalog::{CompletionCatalog, LiveMetadataSnapshot, SymbolSnapshot, map_object_kind};
pub use clause_insert::{
    FROM_CLAUSE_DETAIL, REQUIRED_GROUP_BY_DETAIL, WHERE_CLAUSE_DETAIL, after_select_list_keywords,
    clause_insert_text, clause_keyword_for_detail, infer_indent_unit, post_from_offers_columns,
    required_group_by_labels,
};
pub use complete::{
    SLOT_VALUE_SCORE, add_builtin_function_items, at_at_items, at_drop_table_name, complete,
    complete_from_snapshot, current_prefix, default_snippets, join_awaiting_on,
    join_condition_without_on, query_aliases, replace_builtin_function_items, slot_value_items,
};
pub use quoting::{QuotedPath, apply_identifier_quoting, quote_path_if_needed, quoted_path_at};
pub use rank::{
    ACCEPT_BOOST_MAX, ACCEPT_BOOST_PER_HIT, ACCEPT_HISTORY_CAP, AcceptHistory, EXACT_LABEL_BOOST,
    apply_rank_boosts, exact_label_boost,
};
pub use score::{MatchTier, object_match_tier, qualified_object_score};
pub use set_statement::set_statement_items;
pub use statement::{statement_range, statement_region};
pub use types::{CompletionItem, CompletionKind, CompletionObject, CompletionSource, FkRelation};
pub use values::{
    ValueKind, column_value_kind_from_catalog, comparison_value_items, value_kind_for_type,
};
