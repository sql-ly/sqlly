//! Request/response types for the completion wire contract: what the client
//! sends ([`CompletionRequest`]) and what the engine returns ([`Completions`]
//! of [`CompletionItem`]s), plus the shared `why` reason vocabulary.

use serde::{Deserialize, Serialize};
use sqlly_lang::sqlmodel::{Dialect, Span, SqlName};
use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchType {
    Like,
    Equals,
    Gt,
    Lt,
    Ge,
    Le,
    Ne,
    Between,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValueCompletionParams {
    pub limit: Option<u32>,
    pub sort: Option<SortOrder>,
    pub match_type: Option<MatchType>,
    /// FK display-column override (§8.3 resolution step 1).
    pub display_column: Option<String>,
    /// `true` when the user explicitly invoked the value menu (e.g.
    /// Ctrl+Shift+Space). An explicit trigger always runs a fresh database
    /// query and (re)populates the per-slot value cache; subsequent keystrokes
    /// send `trigger: false`, which filters the cached candidate set locally
    /// without touching the database. Defaults to `false` so older serialized
    /// payloads deserialize unchanged.
    #[serde(default)]
    pub trigger: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CompletionMode {
    Symbol,
    Value(ValueCompletionParams),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub text: String,
    pub cursor: usize, // byte offset
    pub dialect: Dialect,
    pub default_catalog: Option<SqlName>,
    pub default_namespace: Option<SqlName>,
    pub mode: CompletionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompletionItemKind {
    Table,
    View,
    Column,
    Procedure,
    Function,
    Parameter,
    Keyword,
    Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionItem {
    pub label: String,
    pub insert_text: String,
    pub kind: CompletionItemKind,
    pub detail: Option<String>,
    /// Range to replace (the typed prefix), when one exists.
    pub replace_span: Option<Span>,
    /// From a Recovered-fidelity object — render as tentative.
    pub tentative: bool,
    /// When the label is qualified (e.g. `pm.Description`) but the query should
    /// match against the bare member (`Description`), this carries the text to
    /// score against. The wire forwards it so the client re-scores on the bare
    /// name (prefix) instead of the qualified label (acronym), keeping scoped
    /// columns at the top of the list.
    pub match_text: Option<String>,

    // --- Passive metadata (ADR completion enrichment) -------------------
    // These describe *what* an offering is and *why* it was offered. They
    // never affect ranking or insertion; the client forwards them for richer
    // UI, "why suggested" explanations, and AI grounding. `label`/`insert_text`/
    // `kind` remain the canonical display/replacement/type fields.
    /// Bare object name (column/table/keyword/value), distinct from the
    /// possibly-qualified `label` (e.g. `Description` for label `pm.Description`).
    #[serde(default)]
    pub name: String,
    /// Cheap, stable reason code for why this item was offered (e.g. `column`,
    /// `alias`, `fk`, `keyword`, `value`). Borrowed-static when built in-engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<Cow<'static, str>>,
    /// Table alias this item was reached through (alias-dot columns) or the
    /// generated join alias (FK join candidates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Owning schema/namespace of the object, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Parent object's bare name: a column's owning table/view, a routine
    /// parameter's procedure/function. `None` for top-level objects, keywords,
    /// and variables (which have no parent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Kind of the parent object (e.g. `Table`/`View` for a column).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_type: Option<CompletionItemKind>,
    /// Data type of the object, when known (column/parameter/value).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// Concrete value, used only for variables or value-mode (table
    /// introspection) completions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Cheap, stable reason code for why an item of a given kind was offered.
/// Borrowed-static so building items allocates nothing for `why`.
///
/// # Canonical `why` vocabulary (shared wire contract)
///
/// The `why` field is a small, stable, lower-case vocabulary shared by the Rust
/// engine and the Swift client (mirror this list in `CompletionKind.defaultWhy`
/// on the Swift side). Consumers may match on these exact strings.
///
/// Kind-default reasons (returned here, used when no more specific context
/// applies): `table`, `view`, `column`, `procedure`, `function`, `parameter`,
/// `keyword`, `value`.
///
/// Context-override reasons (set explicitly at a construction site, never the
/// kind default):
/// - `alias`  — a column reached through a table alias, or an alias item.
/// - `scope`  — a column/table in the statement's local scope.
/// - `fk`     — a foreign-key join candidate.
/// - `variable` — a declared `@variable` (has no parent object).
/// - `parameter` — a routine (procedure/function) parameter (parent = routine).
/// - `cte`    — a common-table-expression name (client-only source).
/// - `temp`   — a `#temp`/temp-table name (client-only source).
/// - `result` — a prior result-set column (client-only source).
/// - `hint`   — a layout/pivot result hint (client-only source).
///
/// `variable` vs `parameter`: both render with `CompletionItemKind::Parameter`,
/// but a variable has `parent == None` while a routine parameter carries its
/// routine as `parent`/`parent_type`. The Rust engine currently emits neither
/// (it has no DECLARE awareness and no parameter-completion site); `cte`/`temp`/
/// `result`/`hint` are likewise produced only by the client's local completion
/// sources. They are part of the shared vocabulary so both sides agree.
pub const fn why_for_kind(kind: CompletionItemKind) -> &'static str {
    match kind {
        CompletionItemKind::Table => "table",
        CompletionItemKind::View => "view",
        CompletionItemKind::Column => "column",
        CompletionItemKind::Procedure => "procedure",
        CompletionItemKind::Function => "function",
        CompletionItemKind::Parameter => "parameter",
        CompletionItemKind::Keyword => "keyword",
        CompletionItemKind::Value => "value",
    }
}

impl CompletionItem {
    /// Base completion item with all metadata defaulted: `name` mirrors `label`,
    /// `why` is the kind's default reason, and every optional metadata field is
    /// `None`. Callers override the specifics (bare `name`, `parent`, `schema`,
    /// `alias`, `r#type`, `value`, a more specific `why`) at the construction
    /// site. Keeps the ten construction sites terse and makes future metadata
    /// additions a single-place change. The sites: outgoing FK join, incoming
    /// FK join, alias-dot column, ObjectName objects, Keyword, PostFromTable
    /// keyword, General objects, General scoped-columns, ExecArg routine
    /// parameters, and value items.
    pub(crate) fn symbol(label: String, insert_text: String, kind: CompletionItemKind) -> Self {
        let name = label.clone();
        CompletionItem {
            label,
            insert_text,
            kind,
            detail: None,
            replace_span: None,
            tentative: false,
            match_text: None,
            name,
            why: Some(Cow::Borrowed(why_for_kind(kind))),
            alias: None,
            schema: None,
            parent: None,
            parent_type: None,
            r#type: None,
            value: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completions {
    pub items: Vec<CompletionItem>,
    pub truncated: bool,
    pub snapshot_generation: u64,
    /// What the engine decided the cursor position is (for value mode).
    pub slot_note: Option<String>,
}
