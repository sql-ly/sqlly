//! "Improve IntelliSense" feedback: capture the full completion context when
//! the user flags a suggestion popup (F2), Brotli-compress it, and sign the
//! submission with the baked-in app key so the store can verify it came from
//! a genuine SQLly build.
//!
//! The capture path is pure and fully unit-tested; only the final HTTP POST
//! (in `store.rs`) touches the network.
//!
//! Signing: release builds embed an ECDSA P-256 private key; the server holds
//! only the matching public keys. This is proof-of-origin to keep anonymous
//! junk out of the feedback queue — it is not a secret, a determined attacker
//! can extract the key from the binary. The key is not in the source: the
//! release pipeline injects it at compile time through the
//! `SQLLY_FEEDBACK_SIGNING_KEY` environment variable (64 hex characters, read
//! from Key Vault; `build.rs` validates it). A build without it still works,
//! but cannot sign feedback, so it never offers to upload any. The
//! `standalone` feature never embeds the key at all.

use std::collections::{BTreeSet, HashMap, HashSet};

use serde::Serialize;
use sqlly_intel::intellisense::{
    CompletionCatalog, CompletionItem, CompletionKind, CompletionSource, query_aliases,
};
use sqlly_services::{IntelliSenseRuleConfig, IntelliSenseTriggerKind, ProcedureInfo};

/// The app version sent with every submission (and covered by the signature).
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The compile-time signing key: 64 hex characters from
/// `SQLLY_FEEDBACK_SIGNING_KEY`, or `None` when the build was made without
/// it. Never embedded in standalone builds, browser builds, or tests.
#[cfg(not(any(test, feature = "standalone", target_arch = "wasm32")))]
const APP_SIGNING_KEY_HEX: Option<&str> = option_env!("SQLLY_FEEDBACK_SIGNING_KEY");
#[cfg(all(not(test), any(feature = "standalone", target_arch = "wasm32")))]
const APP_SIGNING_KEY_HEX: Option<&str> = None;
/// Tests sign with a throwaway key; its public half exists only here.
#[cfg(test)]
const APP_SIGNING_KEY_HEX: Option<&str> =
    Some("3b6a27bcceb6a42d62a3a8d02a6f0d73fd6e1b4b8a2f7f6e5d4c3b2a19087f6e");

/// Parses a 64-hex-character P-256 scalar. Blank means "no key".
fn parse_signing_key(hex: Option<&str>) -> Option<[u8; 32]> {
    let hex = hex?.trim();
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    p256::ecdsa::SigningKey::from_bytes((&key).into()).ok()?;
    Some(key)
}

fn app_signing_key() -> Option<[u8; 32]> {
    static KEY: std::sync::OnceLock<Option<[u8; 32]>> = std::sync::OnceLock::new();
    *KEY.get_or_init(|| parse_signing_key(APP_SIGNING_KEY_HEX))
}

/// Whether this build can send feedback to sqlly.app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackUpload {
    /// A signed release build: feedback uploads normally.
    Available,
    /// Built with the `standalone` feature: nothing talks to sqlly.app.
    Standalone,
    /// Built without the signing key: sqlly.app would reject the upload.
    Unsigned,
}

impl FeedbackUpload {
    /// The user-facing explanation when feedback is off, or `None` when it
    /// works.
    pub fn disabled_message(self) -> Option<String> {
        match self {
            Self::Available => None,
            Self::Standalone => Some(crate::localization::text(
                "feedback.disabled.standalone_build",
            )),
            Self::Unsigned => Some(crate::localization::text(
                "feedback.disabled.unsigned_build",
            )),
        }
    }
}

/// Pure decision behind [`feedback_upload`].
fn feedback_upload_for(standalone: bool, has_key: bool) -> FeedbackUpload {
    if standalone {
        FeedbackUpload::Standalone
    } else if has_key {
        FeedbackUpload::Available
    } else {
        FeedbackUpload::Unsigned
    }
}

/// Whether this build can send feedback (Send Feedback and F2).
pub fn feedback_upload() -> FeedbackUpload {
    feedback_upload_for(
        crate::distribution::is_standalone(),
        app_signing_key().is_some(),
    )
}

/// The signing scheme string; the server rejects payloads that do not start
/// with it. Bump when the signed fields change.
pub const SIGNING_SCHEME: &str = "sqlly-feedback-v1";

/// One suggestion exactly as it appeared in the popup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestionSnapshot {
    pub label: String,
    pub insert_text: String,
    pub kind: String,
    pub source: String,
    pub score: i32,
    pub detail: String,
}

/// A foreign key going *out* of a reported table: one of its columns
/// references `target_schema.target_table.target_column`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForeignKeySnapshot {
    pub column: String,
    pub target_schema: String,
    pub target_table: String,
    pub target_column: String,
}

/// A foreign key coming *into* a reported table: another table's column
/// references this table's `target_column`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IncomingForeignKeySnapshot {
    pub source_schema: String,
    pub source_table: String,
    pub source_column: String,
    pub target_column: String,
}

/// One hop of a multi-hop foreign-key chain: a column of `from` references a
/// column of `to`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FkChainHop {
    pub from_schema: String,
    pub from_table: String,
    pub column: String,
    pub to_schema: String,
    pub to_table: String,
    pub to_column: String,
}

/// One parameter of a captured stored procedure, as the completion engine
/// knew it (the `EXEC` snippet is built from these).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcedureParameterSnapshot {
    pub name: String,
    pub data_type: String,
    pub is_output: bool,
}

/// A stored procedure available to the completion engine, with its full
/// parameter list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcedureSnapshot {
    pub schema: String,
    pub name: String,
    pub parameters: Vec<ProcedureParameterSnapshot>,
}

impl From<&ProcedureInfo> for ProcedureSnapshot {
    fn from(info: &ProcedureInfo) -> Self {
        Self {
            schema: info.schema.clone(),
            name: info.name.clone(),
            parameters: info
                .parameters
                .iter()
                .map(|p| ProcedureParameterSnapshot {
                    name: p.name.clone(),
                    data_type: p.data_type.clone(),
                    is_output: p.is_output,
                })
                .collect(),
        }
    }
}

/// A chain is an ordered path of hops starting at a reported table, e.g.
/// Orders → Customers → Regions.
pub type FkChain = Vec<FkChainHop>;

/// How far a foreign-key chain is followed from a reported table.
const MAX_FK_CHAIN_HOPS: usize = 3;

/// Cap on the number of chains captured — chains grow combinatorially in a
/// densely keyed model, and the payload must stay bounded.
const MAX_FK_CHAINS: usize = 512;

/// A compact snapshot of one catalog object (table/view/routine) that was
/// available to the completion engine at capture time.
///
/// Whenever a table rides in a report — whole table or any single column of
/// it — every foreign key touching it rides along too, in both directions.
/// This is unconditional: there is no UI to exclude FKs, and the disclosure
/// filter never strips them from an included table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogObjectSnapshot {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
    pub kind: String,
    pub columns: Vec<String>,
    pub parameters: Vec<String>,
    pub foreign_keys: Vec<ForeignKeySnapshot>,
    pub incoming_foreign_keys: Vec<IncomingForeignKeySnapshot>,
    /// True when this object backs at least one item the completion popup
    /// offered (a table suggestion, the parent of a column suggestion, a
    /// table named by an FK join, ...). Such objects are part of what the
    /// user was shown, so they always ride in the report — the disclosure
    /// dialog locks them checked and the submit filter keeps them no matter
    /// what is ticked. Without them a regression test built from the report
    /// could not recreate the popup it is meant to verify.
    pub backs_suggestion: bool,
}

/// The catalog summary shipped with the feedback. Capped so a huge schema
/// cannot blow past the server's 100 MB body cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogSnapshot {
    pub databases: Vec<String>,
    pub schemas: Vec<String>,
    pub objects: Vec<CatalogObjectSnapshot>,
    pub snippet_names: Vec<String>,
    pub total_objects: usize,
    pub truncated: bool,
    /// True when the user ticked "include all" in the disclosure dialog (or
    /// the snapshot is the raw capture); false means the user hand-picked
    /// what `crate::feedback_tree` kept.
    pub include_all: bool,
}

/// What the size budget cut from a report. Recorded inside the payload
/// itself, so reviewers see that the context is incomplete — and what is
/// missing — instead of trusting a silently trimmed snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TruncationReport {
    /// The serialized-JSON budget the report ran into, in bytes.
    pub budget_bytes: usize,
    /// The full, untrimmed serialization size, in bytes.
    pub original_bytes: usize,
    /// Human-readable list of what was cut ("dropped all 512 fk chains",
    /// "dropped 12000 of 25000 catalog objects").
    pub cut: Vec<String>,
}

/// Serialized-context size budget (JSON bytes, pre-compression). Well under
/// the server's decompression cap, so a budgeted payload can never be
/// rejected for size after upload.
pub const CONTEXT_BUDGET_BYTES: usize = 16 * 1024 * 1024;

/// Maximum number of catalog objects embedded in the context JSON. Each object
/// stays lightweight — names, columns, and parameter names only, no types or
/// bodies. By default only statement-referenced objects are included; the
/// `truncated` flag is set when the scoped (or opt-in full) snapshot omits
/// objects from the live catalog.
const MAX_CATALOG_OBJECTS: usize = 25_000;

pub use sqlly_core::redact_sql;

/// Everything captured at the moment the user flags a completion popup.
/// Serialized to JSON, Brotli-compressed, and sent as the `contextBrotli`
/// field of the submit request.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackSnapshot {
    pub version: u32,
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub app_version: String,
    pub platform: String,
    /// Everything before the caret. String/binary literals and comment bodies
    /// are redacted; `cursor`, `replace_start`, and `selected_index` still
    /// refer to byte offsets in the pre-redaction buffer.
    pub sql_before_cursor: String,
    /// Everything from the caret to the end of the buffer (redacted the same
    /// way as `sql_before_cursor`).
    pub sql_after_cursor: String,
    /// Byte offset of the caret in the original, pre-redaction buffer.
    pub cursor: usize,
    /// Start of the range an accepted completion would have replaced (pre-
    /// redaction buffer).
    pub replace_start: usize,
    /// Index of the highlighted suggestion when the popup was flagged.
    pub selected_index: usize,
    pub suggestions: Vec<SuggestionSnapshot>,
    /// How the popup was triggered, when known (dot, manual shortcut, ...).
    pub trigger: Option<IntelliSenseTriggerKind>,
    pub manual: bool,
    /// The user's IntelliSense rule configuration (the JSONC rules file,
    /// parsed) — the "intellisense related configuration" reviewers need.
    pub rules: IntelliSenseRuleConfig,
    /// The user's app configuration settings, scrubbed of credentials and
    /// API keys (see `settings_snapshot`). Filled in by the editor at capture
    /// time; `None` only in unit-test captures.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<crate::settings_snapshot::SettingsSnapshot>,
    pub catalog: CatalogSnapshot,
    /// Multi-hop foreign-key chains rooted at the captured tables (Orders →
    /// Customers → Regions). Captured for the dialog's opt-in "relationship
    /// chains" toggle; the submit path clears this unless the user opted in.
    pub fk_chains: Vec<FkChain>,
    /// Set when the serialized context hit the size budget: what was cut and
    /// how much, so a trimmed report never masquerades as a complete one.
    pub truncation: Option<TruncationReport>,
    /// Stored procedures available to the completion engine (bare names;
    /// kept for servers that predate `procedures`).
    pub procedure_names: Vec<String>,
    /// The same procedures with schema and full parameter lists (name, type,
    /// OUTPUT), so a reviewer can recreate an `EXEC` completion.
    pub procedures: Vec<ProcedureSnapshot>,
    /// A facsimile PNG of the completion popup as it was shown (base64).
    /// GPUI exposes no in-app screenshot API, so this is rendered from the
    /// captured suggestions rather than grabbed from the framebuffer. `None`
    /// when there were no suggestions to draw. It rides inside the compressed
    /// context, so it is covered by the request signature and needs no
    /// separate transport.
    pub popup_image_png: Option<String>,
    /// The query exactly as typed, literals and comments intact. Held
    /// in memory only — never serialized — so the dialog can offer to mask
    /// literals; the submit path swaps it into `sql_before_cursor` /
    /// `sql_after_cursor` unless the user chose Mask literals.
    #[serde(skip)]
    pub unredacted_sql: UnredactedSql,
}

/// The pre-redaction query split at the caret. See
/// [`FeedbackSnapshot::unredacted_sql`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnredactedSql {
    pub before_cursor: String,
    pub after_cursor: String,
}

impl FeedbackSnapshot {
    /// True when redaction changed the query, i.e. there are literals or
    /// comments the user could choose to mask.
    pub fn has_redacted_literals(&self) -> bool {
        self.unredacted_sql.before_cursor != self.sql_before_cursor
            || self.unredacted_sql.after_cursor != self.sql_after_cursor
    }

    /// Replaces the redacted query with the one the user typed.
    pub fn reveal_literals(&mut self) {
        self.sql_before_cursor = self.unredacted_sql.before_cursor.clone();
        self.sql_after_cursor = self.unredacted_sql.after_cursor.clone();
    }
}

pub(crate) fn sql_identifier_tokens(sql: &str) -> std::collections::HashSet<String> {
    let mut tokens = std::collections::HashSet::new();
    let mut current = String::new();
    for ch in sql.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.insert(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.insert(current);
    }
    tokens
}

fn catalog_object_referenced(
    object: &sqlly_intel::intellisense::CompletionObject,
    tokens: &std::collections::HashSet<String>,
    suggestion_labels: &[String],
) -> bool {
    if tokens.contains(&object.name.to_ascii_lowercase()) {
        return true;
    }
    if object
        .schema
        .as_ref()
        .is_some_and(|schema| tokens.contains(&schema.to_ascii_lowercase()))
    {
        return true;
    }
    if object
        .database
        .as_ref()
        .is_some_and(|database| tokens.contains(&database.to_ascii_lowercase()))
    {
        return true;
    }
    let name = object.name.to_ascii_lowercase();
    suggestion_labels.iter().any(|label| {
        label.eq_ignore_ascii_case(&object.name) || label.to_ascii_lowercase().contains(&name)
    })
}

fn snapshot_catalog_objects(
    sql: &str,
    catalog: &CompletionCatalog,
    suggestion_labels: &[String],
    required: &BTreeSet<usize>,
    full_catalog: bool,
) -> (Vec<CatalogObjectSnapshot>, bool) {
    let tokens = sql_identifier_tokens(sql);
    // Objects backing a popup suggestion are always selected and never lose
    // their place to the object cap: the cap only trims the optional rest.
    let optional_budget = MAX_CATALOG_OBJECTS.saturating_sub(required.len());
    let mut optional_taken = 0usize;
    let selected: Vec<(usize, &sqlly_intel::intellisense::CompletionObject)> = catalog
        .objects
        .iter()
        .enumerate()
        .filter(|(index, object)| {
            if required.contains(index) {
                return true;
            }
            let wanted =
                full_catalog || catalog_object_referenced(object, &tokens, suggestion_labels);
            if wanted && optional_taken < optional_budget {
                optional_taken += 1;
                true
            } else {
                false
            }
        })
        .collect();
    let include_all = full_catalog;
    // Incoming FKs are the *other* tables' outgoing FKs pointing at a table,
    // so index the whole live catalog (not just the selected objects): a
    // reported table must carry every FK touching it even when the table on
    // the far end is not itself in the report.
    let mut incoming: std::collections::HashMap<(String, String), Vec<IncomingForeignKeySnapshot>> =
        std::collections::HashMap::new();
    for object in &catalog.objects {
        for fk in &object.foreign_keys {
            incoming
                .entry((
                    fk.target_schema.to_ascii_lowercase(),
                    fk.target_table.to_ascii_lowercase(),
                ))
                .or_default()
                .push(IncomingForeignKeySnapshot {
                    source_schema: object.schema.clone().unwrap_or_default(),
                    source_table: object.name.clone(),
                    source_column: fk.column.clone(),
                    target_column: fk.target_column.clone(),
                });
        }
    }
    let objects = selected
        .into_iter()
        .map(|(index, object)| CatalogObjectSnapshot {
            database: object.database.clone(),
            schema: object.schema.clone(),
            name: object.name.clone(),
            kind: format!("{:?}", object.kind).to_ascii_lowercase(),
            columns: object.columns.clone(),
            parameters: object.parameters.clone(),
            foreign_keys: object
                .foreign_keys
                .iter()
                .map(|fk| ForeignKeySnapshot {
                    column: fk.column.clone(),
                    target_schema: fk.target_schema.clone(),
                    target_table: fk.target_table.clone(),
                    target_column: fk.target_column.clone(),
                })
                .collect(),
            incoming_foreign_keys: incoming
                .get(&(
                    object.schema.as_deref().unwrap_or("").to_ascii_lowercase(),
                    object.name.to_ascii_lowercase(),
                ))
                .cloned()
                .unwrap_or_default(),
            backs_suggestion: required.contains(&index),
        })
        .collect::<Vec<_>>();
    (objects, include_all)
}

/// Splits text into dotted identifier chains: `JOIN dbo.Customers c ON
/// c.Id = o.CustomerId` yields `[dbo, Customers]`, `[c]`, `[ON]`, `[c, Id]`,
/// `[o, CustomerId]`. Bracketed, double-quoted, and backticked parts are
/// unwrapped (`[Order Details]` is one part).
fn identifier_chains(text: &str) -> Vec<Vec<String>> {
    let is_ident = |c: char| c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$');
    let chars: Vec<char> = text.chars().collect();
    let mut chains: Vec<Vec<String>> = Vec::new();
    let mut chain: Vec<String> = Vec::new();
    // True right after a `.`: the next part extends the current chain.
    let mut joined = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let part = if matches!(c, '[' | '"' | '`') {
            let close = if c == '[' { ']' } else { c };
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && chars[end] != close {
                end += 1;
            }
            i = (end + 1).min(chars.len());
            Some(chars[start..end].iter().collect::<String>())
        } else if is_ident(c) {
            let start = i;
            while i < chars.len() && is_ident(chars[i]) {
                i += 1;
            }
            Some(chars[start..i].iter().collect::<String>())
        } else {
            None
        };
        match part {
            Some(part) => {
                if !joined && !chain.is_empty() {
                    chains.push(std::mem::take(&mut chain));
                }
                if !part.is_empty() {
                    chain.push(part);
                }
                joined = false;
            }
            None => {
                if c == '.' && !chain.is_empty() {
                    joined = true;
                } else {
                    if !chain.is_empty() {
                        chains.push(std::mem::take(&mut chain));
                    }
                    joined = false;
                }
                i += 1;
            }
        }
    }
    if !chain.is_empty() {
        chains.push(chain);
    }
    chains
}

/// Resolves names seen in popup items back to live catalog objects (indices
/// into `CompletionCatalog::objects`).
struct SuggestionObjectResolver<'a> {
    catalog: &'a CompletionCatalog,
    by_name: HashMap<String, Vec<usize>>,
    by_column: HashMap<String, Vec<usize>>,
    by_parameter: HashMap<String, Vec<usize>>,
    /// Statement aliases (lowercased) → the table reference's name parts.
    aliases: HashMap<String, Vec<String>>,
    /// Objects the statement itself names (directly or through an alias).
    in_statement: HashSet<usize>,
}

impl<'a> SuggestionObjectResolver<'a> {
    fn new(sql: &str, cursor: usize, catalog: &'a CompletionCatalog) -> Self {
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_column: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_parameter: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, object) in catalog.objects.iter().enumerate() {
            by_name
                .entry(object.name.to_lowercase())
                .or_default()
                .push(index);
            for column in &object.columns {
                by_column
                    .entry(column.to_lowercase())
                    .or_default()
                    .push(index);
            }
            for parameter in &object.parameters {
                by_parameter
                    .entry(parameter.to_lowercase())
                    .or_default()
                    .push(index);
            }
        }
        let mut resolver = Self {
            catalog,
            by_name,
            by_column,
            by_parameter,
            aliases: HashMap::new(),
            in_statement: HashSet::new(),
        };
        for (alias, name) in query_aliases(sql, cursor) {
            if let Some(parts) = identifier_chains(&name).into_iter().next() {
                resolver.aliases.insert(alias.to_lowercase(), parts);
            }
        }
        let tokens = sql_identifier_tokens(sql);
        let mut in_statement: HashSet<usize> = catalog
            .objects
            .iter()
            .enumerate()
            .filter(|(_, object)| tokens.contains(&object.name.to_ascii_lowercase()))
            .map(|(index, _)| index)
            .collect();
        for parts in resolver.aliases.values() {
            in_statement.extend(resolver.resolve_chain(parts));
        }
        resolver.in_statement = in_statement;
        resolver
    }

    /// Objects named by `[db.][schema.]name` — the last part is the object
    /// name, any preceding parts must match its schema (and database).
    fn resolve_chain(&self, parts: &[String]) -> Vec<usize> {
        let Some(name) = parts.last() else {
            return Vec::new();
        };
        let Some(candidates) = self.by_name.get(&name.to_lowercase()) else {
            return Vec::new();
        };
        let schema = parts.len().checked_sub(2).map(|i| &parts[i]);
        let database = parts.len().checked_sub(3).map(|i| &parts[i]);
        candidates
            .iter()
            .copied()
            .filter(|&index| {
                let object = &self.catalog.objects[index];
                let schema_ok = schema.is_none_or(|schema| {
                    object
                        .schema
                        .as_deref()
                        .is_some_and(|s| s.eq_ignore_ascii_case(schema))
                });
                let database_ok = database.is_none_or(|database| {
                    object
                        .database
                        .as_deref()
                        .is_none_or(|d| d.eq_ignore_ascii_case(database))
                });
                schema_ok && database_ok
            })
            .collect()
    }

    /// The object(s) a column qualifier names: a statement alias first, then
    /// a `[schema.]table` chain.
    fn resolve_qualifier(&self, parts: &[String]) -> Vec<usize> {
        if let [alias] = parts
            && let Some(target) = self.aliases.get(&alias.to_lowercase())
        {
            let resolved = self.resolve_chain(target);
            if !resolved.is_empty() {
                return resolved;
            }
        }
        self.resolve_chain(parts)
    }

    /// Objects that own `member` (a column or parameter, via `index`):
    /// the ones the statement names when any of them do, otherwise every
    /// owner — the popup offered the name without saying which object it
    /// came from, so any owner may be the one that produced it.
    fn owners(&self, index: &HashMap<String, Vec<usize>>, member: &str) -> Vec<usize> {
        let Some(owners) = index.get(&member.to_lowercase()) else {
            return Vec::new();
        };
        let scoped: Vec<usize> = owners
            .iter()
            .copied()
            .filter(|owner| self.in_statement.contains(owner))
            .collect();
        if scoped.is_empty() {
            owners.clone()
        } else {
            scoped
        }
    }

    fn has_column(&self, index: usize, column: &str) -> bool {
        self.catalog.objects[index]
            .columns
            .iter()
            .any(|c| c.eq_ignore_ascii_case(column))
    }

    /// Catalog objects backing one popup item.
    fn resolve_item(&self, item: &CompletionItem) -> Vec<usize> {
        if matches!(
            item.source,
            CompletionSource::Keyword | CompletionSource::Builtin | CompletionSource::Snippet
        ) {
            return Vec::new();
        }
        let first_chain = |text: &str| identifier_chains(text).into_iter().next();
        match item.kind {
            CompletionKind::Table
            | CompletionKind::Procedure
            | CompletionKind::Function
            | CompletionKind::Type => {
                for text in [&item.label, &item.insert_text] {
                    if let Some(parts) = first_chain(text) {
                        let resolved = self.resolve_chain(&parts);
                        if !resolved.is_empty() {
                            return resolved;
                        }
                    }
                }
                Vec::new()
            }
            CompletionKind::Column | CompletionKind::ResultSetColumn => {
                let Some(parts) = first_chain(&item.label) else {
                    return Vec::new();
                };
                let Some((column, qualifier)) = parts.split_last() else {
                    return Vec::new();
                };
                if !qualifier.is_empty() {
                    let resolved = self.resolve_qualifier(qualifier);
                    let owning: Vec<usize> = resolved
                        .iter()
                        .copied()
                        .filter(|&index| self.has_column(index, column))
                        .collect();
                    if !owning.is_empty() {
                        return owning;
                    }
                    if !resolved.is_empty() {
                        return resolved;
                    }
                }
                self.owners(&self.by_column, column)
            }
            CompletionKind::Parameter => first_chain(&item.label)
                .and_then(|parts| parts.last().cloned())
                .map(|parameter| self.owners(&self.by_parameter, &parameter))
                .unwrap_or_default(),
            CompletionKind::Join => {
                // FK join items name the joined table and qualify the ON
                // columns with statement aliases; pull in every table either
                // side names.
                let mut resolved = Vec::new();
                for text in [&item.label, &item.insert_text] {
                    for parts in identifier_chains(text) {
                        resolved.extend(self.resolve_chain(&parts));
                        if parts.len() >= 2 {
                            resolved.extend(self.resolve_qualifier(&parts[..parts.len() - 1]));
                        }
                    }
                }
                resolved
            }
            CompletionKind::Keyword
            | CompletionKind::Database
            | CompletionKind::Schema
            | CompletionKind::Snippet
            | CompletionKind::Value => Vec::new(),
        }
    }
}

/// Every live catalog object (index into `catalog.objects`) that backs an
/// item the popup offered: a table/view/routine suggestion resolves to that
/// object, a column to its parent table or view (through a statement alias
/// when qualified; for a bare column name, the statement's own tables that
/// own it, else every object that owns it), a parameter to its routine, and
/// an FK join to every table either side of the join names. These objects
/// must ride in the report — they are part of what the user was shown.
pub(crate) fn suggestion_backing_objects(
    sql: &str,
    cursor: usize,
    completions: &[CompletionItem],
    catalog: &CompletionCatalog,
) -> BTreeSet<usize> {
    if completions.is_empty() || catalog.objects.is_empty() {
        return BTreeSet::new();
    }
    let resolver = SuggestionObjectResolver::new(sql, cursor, catalog);
    completions
        .iter()
        .flat_map(|item| resolver.resolve_item(item))
        .collect()
}

/// Follows outgoing foreign keys from each captured table through the whole
/// live catalog, collecting multi-hop chains (two hops or more — the single
/// hops already ride on the objects themselves). Depth and count are capped;
/// cycles stop at the first revisited table.
fn fk_chains_from(objects: &[CatalogObjectSnapshot], catalog: &CompletionCatalog) -> Vec<FkChain> {
    type Key = (String, String);
    let key = |schema: &str, table: &str| -> Key {
        (schema.to_ascii_lowercase(), table.to_ascii_lowercase())
    };
    // Outgoing FK edges of every table the engine knows about.
    let mut edges: std::collections::HashMap<Key, Vec<FkChainHop>> =
        std::collections::HashMap::new();
    for object in &catalog.objects {
        let schema = object.schema.as_deref().unwrap_or("");
        for fk in &object.foreign_keys {
            edges
                .entry(key(schema, &object.name))
                .or_default()
                .push(FkChainHop {
                    from_schema: schema.to_string(),
                    from_table: object.name.clone(),
                    column: fk.column.clone(),
                    to_schema: fk.target_schema.clone(),
                    to_table: fk.target_table.clone(),
                    to_column: fk.target_column.clone(),
                });
        }
    }

    let mut chains: Vec<FkChain> = Vec::new();
    for object in objects {
        let start = key(object.schema.as_deref().unwrap_or(""), &object.name);
        // Depth-first over hop paths from this table.
        let mut stack: Vec<(Key, Vec<FkChainHop>)> = vec![(start.clone(), Vec::new())];
        while let Some((at, path)) = stack.pop() {
            if chains.len() >= MAX_FK_CHAINS {
                return chains;
            }
            if path.len() >= MAX_FK_CHAIN_HOPS {
                continue;
            }
            for hop in edges.get(&at).map(|e| e.as_slice()).unwrap_or_default() {
                let next = key(&hop.to_schema, &hop.to_table);
                // Cycle guard: never revisit a table already on the path.
                if next == start
                    || path
                        .iter()
                        .any(|h| key(&h.from_schema, &h.from_table) == next)
                {
                    continue;
                }
                let mut extended = path.clone();
                extended.push(hop.clone());
                if extended.len() >= 2 {
                    chains.push(extended.clone());
                }
                stack.push((next, extended));
            }
        }
    }
    chains
}

/// Captures the completion context with a scoped catalog snapshot (only
/// objects referenced by the current statement and captured suggestions).
/// `cursor` is a byte offset into `sql`; anything that is not a char boundary
/// clamps down to one (defensive — the editor always supplies a boundary).
#[allow(clippy::too_many_arguments)]
pub fn capture(
    sql: &str,
    cursor: usize,
    completions: &[CompletionItem],
    replace_start: usize,
    selected_index: usize,
    rules: &IntelliSenseRuleConfig,
    catalog: &CompletionCatalog,
    procedures: &[ProcedureInfo],
    trigger: Option<IntelliSenseTriggerKind>,
    manual: bool,
) -> FeedbackSnapshot {
    capture_with_full_catalog(
        sql,
        cursor,
        completions,
        replace_start,
        selected_index,
        rules,
        catalog,
        procedures,
        trigger,
        manual,
        false,
    )
}

/// Like [`capture`], but when `full_catalog` is true the snapshot embeds the
/// full schema (up to [`MAX_CATALOG_OBJECTS`]) and sets `include_all: true`.
/// Opt-in hook for a future disclosure-dialog toggle.
#[allow(clippy::too_many_arguments)]
pub fn capture_with_full_catalog(
    sql: &str,
    cursor: usize,
    completions: &[CompletionItem],
    replace_start: usize,
    selected_index: usize,
    rules: &IntelliSenseRuleConfig,
    catalog: &CompletionCatalog,
    procedures: &[ProcedureInfo],
    trigger: Option<IntelliSenseTriggerKind>,
    manual: bool,
    full_catalog: bool,
) -> FeedbackSnapshot {
    let cursor = clamp_to_char_boundary(sql, cursor.min(sql.len()));
    let suggestions: Vec<SuggestionSnapshot> = completions
        .iter()
        .map(|item| SuggestionSnapshot {
            label: item.label.clone(),
            insert_text: item.insert_text.clone(),
            kind: format!("{:?}", item.kind).to_ascii_lowercase(),
            source: format!("{:?}", item.source).to_ascii_lowercase(),
            score: item.score,
            detail: item.detail.clone(),
        })
        .collect();

    // Draw a facsimile of the popup from what we just captured (no screenshot
    // API in-app), and carry it as base64 inside the snapshot.
    let popup_image_png = render_popup_png(&suggestions, selected_index)
        .map(|png| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png));

    let suggestion_labels: Vec<String> = suggestions.iter().map(|s| s.label.clone()).collect();
    let total_objects = catalog.objects.len();
    let required = suggestion_backing_objects(sql, cursor, completions, catalog);
    let (objects, include_all) =
        snapshot_catalog_objects(sql, catalog, &suggestion_labels, &required, full_catalog);
    let objects_kept = objects.len();

    let fk_chains = fk_chains_from(&objects, catalog);

    FeedbackSnapshot {
        // v4: catalog objects carry foreignKeys / incomingForeignKeys.
        // v5: fkChains (opt-in multi-hop chains) and truncation (size-budget
        // report); both additive and optional for the server.
        // v6: settings (the user's scrubbed app configuration).
        // v7: procedures (schema + typed parameter lists).
        version: 7,
        captured_at: chrono::Utc::now(),
        app_version: APP_VERSION.to_string(),
        platform: platform_name().to_string(),
        sql_before_cursor: redact_sql(&sql[..cursor]),
        sql_after_cursor: redact_sql(&sql[cursor..]),
        cursor,
        replace_start,
        selected_index,
        suggestions,
        trigger,
        manual,
        rules: rules.clone(),
        settings: None,
        catalog: CatalogSnapshot {
            databases: catalog.databases.clone(),
            schemas: catalog.schemas.clone(),
            objects,
            snippet_names: catalog
                .snippets
                .iter()
                .map(|(name, _)| name.clone())
                .collect(),
            total_objects,
            truncated: objects_kept < total_objects,
            include_all,
        },
        fk_chains,
        truncation: None,
        procedure_names: procedures.iter().map(|p| p.name.clone()).collect(),
        procedures: procedures.iter().map(ProcedureSnapshot::from).collect(),
        popup_image_png,
        unredacted_sql: UnredactedSql {
            before_cursor: sql[..cursor].to_string(),
            after_cursor: sql[cursor..].to_string(),
        },
    }
}

/// Embedded copy of the app's UI font, so the popup facsimile matches what the
/// user actually saw.
const POPUP_FONT: &[u8] = include_bytes!("../resources/fonts/JetBrainsMono-Regular.ttf");

/// Max suggestion rows drawn into the facsimile (the real popup scrolls; the
/// picture just needs to show the top of the list).
const POPUP_MAX_ROWS: usize = 14;

/// Renders a facsimile PNG of the completion popup from the captured
/// suggestions: one row per suggestion, the selected row highlighted, the kind
/// right-aligned. Returns `None` when there is nothing to draw or the font
/// fails to load.
pub fn render_popup_png(
    suggestions: &[SuggestionSnapshot],
    selected_index: usize,
) -> Option<Vec<u8>> {
    use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
    use image::{ImageFormat, Rgba, RgbaImage};

    if suggestions.is_empty() {
        return None;
    }
    let font = FontRef::try_from_slice(POPUP_FONT).ok()?;

    // Layout constants (device pixels). Rendered at 2x for a crisp image.
    let scale = PxScale::from(28.0);
    let scaled = font.as_scaled(scale);
    let ascent = scaled.ascent();
    let line_h = (scaled.height() + scaled.line_gap()).ceil() as i32 + 12;
    let pad_x = 24_i32;
    let pad_y = 14_i32;
    let marker_w = 28_i32;

    let rows: Vec<&SuggestionSnapshot> = suggestions.iter().take(POPUP_MAX_ROWS).collect();

    // Measure text width of a string at the current scale.
    let text_width = |text: &str| -> i32 {
        let mut w = 0.0_f32;
        for ch in text.chars() {
            w += scaled.h_advance(font.glyph_id(ch));
        }
        w.ceil() as i32
    };

    let label_w = rows.iter().map(|s| text_width(&s.label)).max().unwrap_or(0);
    let kind_w = rows
        .iter()
        .map(|s| text_width(&format!("({})", s.kind)))
        .max()
        .unwrap_or(0);
    let width = (pad_x + marker_w + label_w + 32 + kind_w + pad_x).max(320);
    let height = pad_y * 2 + line_h * rows.len() as i32;

    let mut img = RgbaImage::from_pixel(width as u32, height as u32, Rgba([30, 30, 36, 255]));

    // Draw a single glyph run at (x, baseline) in the given color.
    let draw_text = |img: &mut RgbaImage, x: i32, baseline: i32, text: &str, color: [u8; 3]| {
        let mut pen_x = x as f32;
        for ch in text.chars() {
            let glyph = font
                .glyph_id(ch)
                .with_scale_and_position(scale, ab_glyph::point(pen_x, baseline as f32));
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                outlined.draw(|gx, gy, coverage| {
                    let px = bounds.min.x as i32 + gx as i32;
                    let py = bounds.min.y as i32 + gy as i32;
                    if px < 0 || py < 0 || px >= width || py >= height {
                        return;
                    }
                    let bg = img.get_pixel(px as u32, py as u32).0;
                    let a = coverage.clamp(0.0, 1.0);
                    let blend = |fg: u8, bg: u8| (fg as f32 * a + bg as f32 * (1.0 - a)) as u8;
                    img.put_pixel(
                        px as u32,
                        py as u32,
                        Rgba([
                            blend(color[0], bg[0]),
                            blend(color[1], bg[1]),
                            blend(color[2], bg[2]),
                            255,
                        ]),
                    );
                });
            }
            pen_x += scaled.h_advance(font.glyph_id(ch));
        }
    };

    for (i, s) in rows.iter().enumerate() {
        let row_top = pad_y + i as i32 * line_h;
        let selected = i == selected_index;
        if selected {
            // Highlight the selected row.
            for y in row_top..(row_top + line_h) {
                for x in 0..width {
                    img.put_pixel(x as u32, y as u32, Rgba([55, 90, 150, 255]));
                }
            }
        }
        let baseline = row_top + 6 + ascent as i32;
        let text_color = if selected {
            [235, 240, 255]
        } else {
            [220, 220, 226]
        };
        let kind_color = if selected {
            [200, 214, 240]
        } else {
            [140, 140, 152]
        };
        if selected {
            draw_text(&mut img, pad_x, baseline, "\u{25B8}", text_color);
        }
        draw_text(&mut img, pad_x + marker_w, baseline, &s.label, text_color);
        let kind = format!("({})", s.kind);
        let kx = width - pad_x - text_width(&kind);
        draw_text(&mut img, kx, baseline, &kind, kind_color);
    }

    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

impl FeedbackSnapshot {
    /// Serializes the snapshot to JSON bytes (pre-compression).
    pub fn to_json_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("FeedbackSnapshot serialization is infallible")
    }

    /// Serializes within `budget` bytes of JSON. A snapshot that fits is
    /// returned untouched. One that does not is trimmed — FK chains first,
    /// then the procedure list (procedures the query names last), then
    /// catalog objects from the end — and the result carries a
    /// [`TruncationReport`] stating the budget, the original size, and
    /// exactly what was cut.
    pub fn to_json_bytes_within(&self, budget: usize) -> Vec<u8> {
        let bytes = self.to_json_bytes();
        if bytes.len() <= budget {
            return bytes;
        }
        let original_bytes = bytes.len();
        let mut trimmed = self.clone();

        let dropped_chains = trimmed.fk_chains.len();
        trimmed.fk_chains.clear();
        // Objects backing a popup suggestion are the last to go: move them to
        // the front (stable, so the rest keep their order) before halving.
        trimmed
            .catalog
            .objects
            .sort_by_key(|object| !object.backs_suggestion);
        let total_objects = trimmed.catalog.objects.len();
        // Procedures the query names are kept longest, same as objects.
        let tokens = sql_identifier_tokens(&format!(
            "{}{}",
            trimmed.sql_before_cursor, trimmed.sql_after_cursor
        ));
        trimmed
            .procedures
            .sort_by_key(|p| !tokens.contains(&p.name.to_ascii_lowercase()));
        let total_procedures = trimmed.procedures.len();
        loop {
            let mut cut = Vec::new();
            if dropped_chains > 0 {
                cut.push(crate::localization::format(
                    "feedback.intellisense.truncation_dropped_chains",
                    &[("count", &dropped_chains.to_string())],
                ));
            }
            let dropped_procedures = total_procedures - trimmed.procedures.len();
            if dropped_procedures > 0 {
                cut.push(crate::localization::format(
                    "feedback.intellisense.truncation_dropped_procedures",
                    &[
                        ("dropped", &dropped_procedures.to_string()),
                        ("total", &total_procedures.to_string()),
                    ],
                ));
            }
            let dropped_objects = total_objects - trimmed.catalog.objects.len();
            if dropped_objects > 0 {
                cut.push(crate::localization::format(
                    "feedback.intellisense.truncation_dropped_objects",
                    &[
                        ("dropped", &dropped_objects.to_string()),
                        ("total", &total_objects.to_string()),
                    ],
                ));
                trimmed.catalog.truncated = true;
            }
            trimmed.truncation = Some(TruncationReport {
                budget_bytes: budget,
                original_bytes,
                cut,
            });
            let bytes = trimmed.to_json_bytes();
            if bytes.len() <= budget
                || (trimmed.catalog.objects.is_empty() && trimmed.procedures.is_empty())
            {
                return bytes;
            }
            if !trimmed.procedures.is_empty() {
                let keep = trimmed.procedures.len() / 2;
                trimmed.procedures.truncate(keep);
                continue;
            }
            // Still over: halve the object list and re-measure. Suggestion-
            // backing objects (sorted to the front) survive until nothing
            // else is left to cut.
            let len = trimmed.catalog.objects.len();
            let required = trimmed
                .catalog
                .objects
                .iter()
                .filter(|object| object.backs_suggestion)
                .count();
            let keep = if len > required {
                (len / 2).max(required)
            } else {
                len / 2
            };
            trimmed.catalog.objects.truncate(keep);
        }
    }
}

/// The platform token the store expects. The desktop OS mapping is single-
/// sourced from [`crate::store::platform_name`] so the feedback payload can
/// never drift from the update/download/registration tokens; only the wasm
/// build reports its own `browser` value.
fn platform_name() -> &'static str {
    if cfg!(target_arch = "wasm32") {
        "browser"
    } else {
        crate::store::platform_name()
    }
}

fn clamp_to_char_boundary(sql: &str, mut offset: usize) -> usize {
    while offset > 0 && !sql.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

/// Brotli-compresses the context JSON. Quality 5 keeps capture latency
/// unnoticeable while still crushing repetitive catalog JSON.
pub fn brotli_compress(data: &[u8]) -> Vec<u8> {
    let mut input = std::io::Cursor::new(data);
    let mut output = Vec::with_capacity(data.len() / 4 + 64);
    let params = brotli::enc::BrotliEncoderParams {
        quality: 5,
        ..Default::default()
    };
    brotli::BrotliCompress(&mut input, &mut output, &params)
        .expect("Brotli compression of an in-memory buffer cannot fail");
    output
}

/// The exact string the app signs:
/// `"sqlly-feedback-v1\n{appVersion}\n{timestampUnix}\n{sha256hex(body)}"`.
/// Pure for testing.
pub fn signing_payload(app_version: &str, timestamp_unix: i64, body: &[u8]) -> String {
    format!(
        "{}\n{}\n{}\n{}",
        SIGNING_SCHEME,
        app_version,
        timestamp_unix,
        crate::store::sha256_hex(body)
    )
}

/// Signs the request body with the build's app key, returning the 64-byte
/// raw (r || s, IEEE P1363) signature as base64 for the
/// `X-Sqlly-App-Signature` header. `None` when this build has no key (see
/// [`feedback_upload`]).
pub fn sign_request(app_version: &str, timestamp_unix: i64, body: &[u8]) -> Option<String> {
    use p256::ecdsa::{Signature, SigningKey, signature::Signer};

    if crate::distribution::is_standalone() {
        return None;
    }
    let key = SigningKey::from_bytes((&app_signing_key()?).into()).ok()?;
    let payload = signing_payload(app_version, timestamp_unix, body);
    let signature: Signature = key.sign(payload.as_bytes());
    Some(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        signature.to_bytes(),
    ))
}

/// Builds the JSON request body for `POST /api/intellisense-feedback`
/// (`{ submissionId, expectedText, contextBrotli, deviceId }` — context as
/// base64). The device id identifies the paired device the submission is
/// owned by; the submission id is the client-generated GUID the server
/// dedupes on, so a retried POST cannot double-submit.
pub fn build_request_body(
    submission_id: &str,
    expected_text: &str,
    context_brotli: &[u8],
    device_id: Option<&str>,
) -> Vec<u8> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Body<'a> {
        submission_id: &'a str,
        expected_text: &'a str,
        context_brotli: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        device_id: Option<&'a str>,
    }

    serde_json::to_vec(&Body {
        submission_id,
        expected_text,
        context_brotli: base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            context_brotli,
        ),
        device_id,
    })
    .expect("feedback request body serialization is infallible")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::intellisense::{
        CompletionItem, CompletionKind, CompletionObject, CompletionSource,
    };

    fn item(label: &str, kind: CompletionKind, score: i32) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            insert_text: label.to_string(),
            kind,
            source: CompletionSource::LiveMetadata,
            score,
            detail: String::new(),
            replace_span: None,
        }
    }

    fn catalog_with_objects(count: usize) -> CompletionCatalog {
        CompletionCatalog {
            databases: vec!["appdb".to_string()],
            schemas: vec!["dbo".to_string()],
            objects: (0..count)
                .map(|i| {
                    CompletionObject::table(
                        "dbo",
                        format!("Table{i}"),
                        vec!["Id".to_string(), "Name".to_string()],
                    )
                })
                .collect(),
            filesystem_schemas: Vec::new(),
            snippets: vec![("ssf".to_string(), "SELECT * FROM".to_string())],
        }
    }

    fn fk(column: &str, table: &str) -> sqlly_intel::intellisense::FkRelation {
        sqlly_intel::intellisense::FkRelation {
            column: column.to_string(),
            target_schema: "dbo".to_string(),
            target_table: table.to_string(),
            target_column: "Id".to_string(),
            constraint: None,
        }
    }

    /// Orders → Customers → Regions: the two-hop chain is captured even
    /// though only Orders is referenced by the statement; single hops are
    /// not duplicated as chains (they already ride on the object).
    #[test]
    fn multi_hop_fk_chains_ride_with_the_capture() {
        let mut orders = CompletionObject::table("dbo", "Orders", vec!["Id".to_string()]);
        orders.foreign_keys = vec![fk("CustomerId", "Customers")];
        let mut customers = CompletionObject::table("dbo", "Customers", vec!["Id".to_string()]);
        customers.foreign_keys = vec![fk("RegionId", "Regions")];
        let regions = CompletionObject::table("dbo", "Regions", vec!["Id".to_string()]);
        let catalog = CompletionCatalog {
            databases: Vec::new(),
            schemas: Vec::new(),
            objects: vec![orders, customers, regions],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let snapshot = capture(
            "SELECT * FROM Orders",
            20,
            &[],
            14,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
        );
        assert_eq!(snapshot.catalog.objects.len(), 1, "scoped to Orders");
        assert_eq!(snapshot.fk_chains.len(), 1);
        let chain = &snapshot.fk_chains[0];
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].from_table, "Orders");
        assert_eq!(chain[0].to_table, "Customers");
        assert_eq!(chain[1].from_table, "Customers");
        assert_eq!(chain[1].to_table, "Regions");
    }

    /// A ↔ B cycles terminate instead of looping.
    #[test]
    fn fk_chain_walk_survives_cycles() {
        let mut a = CompletionObject::table("dbo", "A", vec!["Id".to_string()]);
        a.foreign_keys = vec![fk("BId", "B")];
        let mut b = CompletionObject::table("dbo", "B", vec!["Id".to_string()]);
        b.foreign_keys = vec![fk("AId", "A")];
        let catalog = CompletionCatalog {
            databases: Vec::new(),
            schemas: Vec::new(),
            objects: vec![a, b],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let snapshot = capture(
            "SELECT * FROM A",
            15,
            &[],
            14,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
        );
        assert!(snapshot.fk_chains.is_empty());
    }

    /// Over budget: FK chains are cut first, then catalog objects, and the
    /// payload records exactly what was trimmed instead of shipping silently.
    #[test]
    fn oversized_context_is_trimmed_with_a_truncation_report() {
        let mut snapshot = capture(
            "SELECT * FROM dbo.",
            18,
            &[],
            14,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog_with_objects(500),
            &[],
            None,
            false,
        );
        snapshot.fk_chains = vec![vec![FkChainHop {
            from_schema: "dbo".to_string(),
            from_table: "A".to_string(),
            column: "BId".to_string(),
            to_schema: "dbo".to_string(),
            to_table: "B".to_string(),
            to_column: "Id".to_string(),
        }]];

        let full = snapshot.to_json_bytes();
        // Within budget: byte-identical, no truncation report.
        assert_eq!(snapshot.to_json_bytes_within(full.len()), full);

        let budget = full.len() / 2;
        let trimmed = snapshot.to_json_bytes_within(budget);
        assert!(trimmed.len() <= budget, "payload respects the budget");
        let json: serde_json::Value = serde_json::from_slice(&trimmed).unwrap();
        assert_eq!(json["truncation"]["budgetBytes"], budget);
        assert_eq!(json["truncation"]["originalBytes"], full.len());
        let cut: Vec<String> = json["truncation"]["cut"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(cut.iter().any(|c| c.contains("fk chains")), "cut: {cut:?}");
        assert!(
            cut.iter().any(|c| c.contains("catalog objects")),
            "cut: {cut:?}"
        );
        assert!(json["fkChains"].as_array().unwrap().is_empty());
        assert_eq!(json["catalog"]["truncated"], true);
        assert!(json["catalog"]["objects"].as_array().unwrap().len() < 500);
    }

    #[test]
    fn capture_splits_sql_at_the_cursor() {
        let snapshot = capture(
            "SELECT * FROM dbo.Users WHERE ",
            17,
            &[],
            7,
            0,
            &IntelliSenseRuleConfig::default(),
            &CompletionCatalog::default(),
            &[],
            None,
            false,
        );
        assert_eq!(snapshot.sql_before_cursor, "SELECT * FROM dbo");
        assert_eq!(snapshot.sql_after_cursor, ".Users WHERE ");
        assert_eq!(snapshot.cursor, 17);
        assert_eq!(snapshot.replace_start, 7);
    }

    #[test]
    fn capture_clamps_a_mid_character_cursor() {
        // "é" is two UTF-8 bytes; offset 1 lands mid-character.
        let snapshot = capture(
            "éSELECT",
            1,
            &[],
            0,
            0,
            &IntelliSenseRuleConfig::default(),
            &CompletionCatalog::default(),
            &[],
            None,
            false,
        );
        assert_eq!(snapshot.cursor, 0);
        assert_eq!(snapshot.sql_before_cursor, "");
        assert_eq!(snapshot.sql_after_cursor, "éSELECT");
    }

    #[test]
    fn capture_maps_suggestions_with_kind_and_source() {
        let items = [
            item("Users", CompletionKind::Table, 90),
            item("UserId", CompletionKind::Column, 50),
        ];
        let snapshot = capture(
            "SELECT U",
            8,
            &items,
            7,
            1,
            &IntelliSenseRuleConfig::default(),
            &CompletionCatalog::default(),
            &[],
            Some(IntelliSenseTriggerKind::Identifier),
            false,
        );
        assert_eq!(snapshot.suggestions.len(), 2);
        assert_eq!(snapshot.suggestions[0].kind, "table");
        assert_eq!(snapshot.suggestions[0].source, "livemetadata");
        assert_eq!(snapshot.suggestions[0].score, 90);
        assert_eq!(snapshot.selected_index, 1);
        assert_eq!(snapshot.trigger, Some(IntelliSenseTriggerKind::Identifier));
    }

    #[test]
    fn capture_truncates_huge_catalogs_but_reports_the_total() {
        let catalog = catalog_with_objects(MAX_CATALOG_OBJECTS + 10);
        let snapshot = capture_with_full_catalog(
            "SELECT ",
            7,
            &[],
            7,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
            true,
        );
        assert_eq!(snapshot.catalog.objects.len(), MAX_CATALOG_OBJECTS);
        assert_eq!(snapshot.catalog.total_objects, MAX_CATALOG_OBJECTS + 10);
        assert!(snapshot.catalog.truncated);
        assert!(snapshot.catalog.include_all);
        assert_eq!(snapshot.catalog.snippet_names, vec!["ssf"]);
    }

    #[test]
    fn snapshot_json_uses_camel_case_field_names() {
        let snapshot = capture(
            "SELECT ",
            7,
            &[],
            7,
            0,
            &IntelliSenseRuleConfig::default(),
            &CompletionCatalog::default(),
            &[],
            None,
            false,
        );
        let json: serde_json::Value = serde_json::from_slice(&snapshot.to_json_bytes()).unwrap();
        assert!(json.get("sqlBeforeCursor").is_some());
        assert!(json.get("sqlAfterCursor").is_some());
        assert!(json.get("appVersion").is_some());
        assert!(json.get("replaceStart").is_some());
        assert_eq!(json["version"], 7);
        assert!(json.get("settings").is_none(), "unset settings are omitted");

        let mut snapshot = snapshot;
        snapshot.settings = Some(crate::settings_snapshot::capture_from(|name| {
            (name == "fim.json").then(|| r#"{ "enabled": true }"#.to_string())
        }));
        let json: serde_json::Value = serde_json::from_slice(&snapshot.to_json_bytes()).unwrap();
        assert_eq!(json["settings"]["files"]["fim.json"]["enabled"], true);
    }

    #[test]
    fn capture_redacts_sql_literals_and_uses_version_seven() {
        let sql = "SELECT * FROM dbo.Users WHERE password = 'hunter2' AND ";
        let snapshot = capture(
            sql,
            sql.len(),
            &[],
            0,
            0,
            &IntelliSenseRuleConfig::default(),
            &CompletionCatalog::default(),
            &[],
            None,
            false,
        );
        assert!(!snapshot.sql_before_cursor.contains("hunter2"));
        assert!(!snapshot.sql_after_cursor.contains("hunter2"));
        assert_eq!(snapshot.version, 7);
    }

    #[test]
    fn unredacted_sql_stays_local_until_the_user_reveals_it() {
        let sql = "SELECT * FROM dbo.Users WHERE password = 'hunter2' AND ";
        let mut snapshot = capture(
            sql,
            20,
            &[],
            0,
            0,
            &IntelliSenseRuleConfig::default(),
            &CompletionCatalog::default(),
            &[],
            None,
            false,
        );
        assert!(snapshot.has_redacted_literals());
        let json = String::from_utf8(snapshot.to_json_bytes()).unwrap();
        assert!(
            !json.contains("hunter2"),
            "the raw query is never serialized"
        );

        snapshot.reveal_literals();
        assert_eq!(snapshot.sql_before_cursor, &sql[..20]);
        assert_eq!(snapshot.sql_after_cursor, &sql[20..]);
        let json = String::from_utf8(snapshot.to_json_bytes()).unwrap();
        assert!(json.contains("hunter2"), "revealed literals are sent");
    }

    #[test]
    fn capture_scopes_catalog_to_statement_references() {
        let mut orders = CompletionObject::table("dbo", "Orders", vec!["Id".to_string()]);
        orders.database = Some("appdb".to_string());
        let mut customers = CompletionObject::table("dbo", "Customers", vec!["Id".to_string()]);
        customers.database = Some("appdb".to_string());
        let mut unrelated = CompletionObject::table("dbo", "Unrelated", vec!["Id".to_string()]);
        unrelated.database = Some("appdb".to_string());
        let catalog = CompletionCatalog {
            databases: vec!["appdb".to_string()],
            schemas: vec!["dbo".to_string()],
            objects: vec![orders, customers, unrelated],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let snapshot = capture(
            "SELECT * FROM Orders JOIN Customers ON Orders.Id = Customers.Id",
            7,
            &[],
            7,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
        );
        let names: Vec<_> = snapshot
            .catalog
            .objects
            .iter()
            .map(|object| object.name.as_str())
            .collect();
        assert!(names.contains(&"Orders"));
        assert!(names.contains(&"Customers"));
        assert!(!names.contains(&"Unrelated"));
        assert!(!snapshot.catalog.include_all);
        assert_eq!(snapshot.catalog.total_objects, 3);
        assert!(snapshot.catalog.truncated);
    }

    #[test]
    fn capture_full_catalog_includes_everything_and_sets_include_all() {
        let mut orders = CompletionObject::table("dbo", "Orders", vec!["Id".to_string()]);
        orders.database = Some("appdb".to_string());
        let mut customers = CompletionObject::table("dbo", "Customers", vec!["Id".to_string()]);
        customers.database = Some("appdb".to_string());
        let mut unrelated = CompletionObject::table("dbo", "Unrelated", vec!["Id".to_string()]);
        unrelated.database = Some("appdb".to_string());
        let catalog = CompletionCatalog {
            databases: vec!["appdb".to_string()],
            schemas: vec!["dbo".to_string()],
            objects: vec![orders, customers, unrelated],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let snapshot = capture_with_full_catalog(
            "SELECT * FROM Orders",
            7,
            &[],
            7,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
            true,
        );
        assert_eq!(snapshot.catalog.objects.len(), 3);
        assert!(snapshot.catalog.include_all);
        assert!(!snapshot.catalog.truncated);
    }

    /// A reported table carries every FK touching it: its own outgoing FKs
    /// and the incoming ones from other tables — even when the table on the
    /// far end is not itself in the report.
    #[test]
    fn capture_carries_foreign_keys_in_both_directions() {
        use sqlly_intel::intellisense::FkRelation;

        let mut orders = CompletionObject::table(
            "dbo",
            "Orders",
            vec!["Id".to_string(), "UserId".to_string()],
        );
        orders.foreign_keys = vec![FkRelation {
            column: "UserId".to_string(),
            target_schema: "dbo".to_string(),
            target_table: "Users".to_string(),
            target_column: "Id".to_string(),
            constraint: None,
        }];
        let users = CompletionObject::table("dbo", "Users", vec!["Id".to_string()]);
        let catalog = CompletionCatalog {
            databases: vec![],
            schemas: vec!["dbo".to_string()],
            objects: vec![orders, users],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        // Only Users is referenced by the statement, so only Users is
        // captured — but its incoming FK from Orders must still ride along.
        let snapshot = capture(
            "SELECT * FROM Users",
            7,
            &[],
            7,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
        );
        let users = snapshot
            .catalog
            .objects
            .iter()
            .find(|o| o.name == "Users")
            .expect("Users captured");
        assert!(users.foreign_keys.is_empty());
        assert_eq!(users.incoming_foreign_keys.len(), 1);
        let incoming = &users.incoming_foreign_keys[0];
        assert_eq!(incoming.source_table, "Orders");
        assert_eq!(incoming.source_column, "UserId");
        assert_eq!(incoming.target_column, "Id");

        // A full capture keeps Orders' outgoing FK too.
        let snapshot = capture_with_full_catalog(
            "SELECT * FROM Users",
            7,
            &[],
            7,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
            true,
        );
        let orders = snapshot
            .catalog
            .objects
            .iter()
            .find(|o| o.name == "Orders")
            .expect("Orders captured");
        assert_eq!(orders.foreign_keys.len(), 1);
        assert_eq!(orders.foreign_keys[0].target_table, "Users");
    }

    #[test]
    fn brotli_roundtrips_the_context() {
        let data = b"{\"sqlBeforeCursor\":\"SELECT * FROM dbo.Users\"}".repeat(50);
        let compressed = brotli_compress(&data);
        assert!(compressed.len() < data.len());
        let mut decompressed = Vec::new();
        std::io::Read::read_to_end(
            &mut brotli::Decompressor::new(std::io::Cursor::new(&compressed), 4096),
            &mut decompressed,
        )
        .unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    #[cfg(not(feature = "standalone"))]
    fn signature_verifies_against_the_build_public_key() {
        use p256::ecdsa::{Signature, SigningKey, signature::Verifier};

        let secret = parse_signing_key(APP_SIGNING_KEY_HEX).expect("test key parses");
        let key = *SigningKey::from_bytes((&secret).into())
            .unwrap()
            .verifying_key();

        let body = b"{\"expectedText\":\"x\"}";
        let signature_b64 = sign_request("1.2.3", 1_700_000_000, body).expect("test builds sign");
        let signature_bytes =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &signature_b64)
                .unwrap();
        assert_eq!(signature_bytes.len(), 64);

        let signature = Signature::from_slice(&signature_bytes).unwrap();
        let payload = signing_payload("1.2.3", 1_700_000_000, body);
        assert!(key.verify(payload.as_bytes(), &signature).is_ok());

        // A different body must not verify.
        let other = signing_payload("1.2.3", 1_700_000_000, b"{}");
        assert!(key.verify(other.as_bytes(), &signature).is_err());
    }

    #[test]
    fn signing_key_parsing_rejects_blank_and_malformed_values() {
        assert!(parse_signing_key(None).is_none());
        assert!(parse_signing_key(Some("")).is_none());
        assert!(parse_signing_key(Some("   ")).is_none());
        assert!(parse_signing_key(Some("abc")).is_none());
        assert!(parse_signing_key(Some(&"zz".repeat(32))).is_none());
        // Zero is not a valid P-256 scalar.
        assert!(parse_signing_key(Some(&"00".repeat(32))).is_none());
        assert!(parse_signing_key(APP_SIGNING_KEY_HEX).is_some());
    }

    #[test]
    fn feedback_upload_needs_a_key_and_a_non_standalone_build() {
        assert_eq!(feedback_upload_for(false, true), FeedbackUpload::Available);
        assert_eq!(feedback_upload_for(false, false), FeedbackUpload::Unsigned);
        assert_eq!(feedback_upload_for(true, true), FeedbackUpload::Standalone);
        assert_eq!(feedback_upload_for(true, false), FeedbackUpload::Standalone);
        assert!(FeedbackUpload::Available.disabled_message().is_none());
        assert!(FeedbackUpload::Unsigned.disabled_message().is_some());
        assert!(FeedbackUpload::Standalone.disabled_message().is_some());
    }

    #[test]
    fn signing_payload_matches_the_server_construction() {
        let payload = signing_payload("1.0.0", 42, b"abc");
        assert_eq!(
            payload,
            "sqlly-feedback-v1\n1.0.0\n42\nba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn request_body_is_json_with_base64_context() {
        let body = build_request_body(
            "b8a2c3d4-0000-0000-0000-000000000001",
            "should offer tables",
            &[1, 2, 3, 4],
            Some("dev_1"),
        );
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["submissionId"], "b8a2c3d4-0000-0000-0000-000000000001");
        assert_eq!(json["expectedText"], "should offer tables");
        assert_eq!(json["contextBrotli"], "AQIDBA==");
        assert_eq!(json["deviceId"], "dev_1");
    }

    #[test]
    fn request_body_omits_device_id_when_absent() {
        let body = build_request_body("id", "x", &[1], None);
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("deviceId").is_none());
    }

    fn suggestion(label: &str, kind: &str) -> SuggestionSnapshot {
        SuggestionSnapshot {
            label: label.to_string(),
            insert_text: label.to_string(),
            kind: kind.to_string(),
            source: "livemetadata".to_string(),
            score: 0,
            detail: String::new(),
        }
    }

    #[test]
    fn popup_png_is_none_when_there_are_no_suggestions() {
        assert!(render_popup_png(&[], 0).is_none());
    }

    #[test]
    fn popup_png_is_a_valid_png_for_suggestions() {
        let rows = [
            suggestion("Users", "table"),
            suggestion("UserId", "column"),
            suggestion("GetUser", "routine"),
        ];
        let png = render_popup_png(&rows, 0).expect("should render");
        // PNG magic number.
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        // Decodes back to a non-empty image.
        let decoded = image::load_from_memory(&png).expect("valid png");
        assert!(decoded.width() > 0 && decoded.height() > 0);
    }

    #[test]
    fn capture_embeds_a_popup_image_when_suggestions_exist() {
        let snapshot = capture(
            "SELECT * FROM ",
            14,
            &[item("Users", CompletionKind::Table, 10)],
            14,
            0,
            &IntelliSenseRuleConfig::default(),
            &CompletionCatalog::default(),
            &[],
            None,
            false,
        );
        assert!(snapshot.popup_image_png.is_some());
    }

    /// Guards the JSON field names the server's `FeedbackContext` DTO parses.
    /// If a rename drifts here, the admin detail page silently loses data — so
    /// pin the camelCase contract on the Rust side.
    #[test]
    fn snapshot_json_uses_the_camel_case_server_contract() {
        let mut catalog = catalog_with_objects(1);
        catalog.databases = vec!["appdb".to_string()];
        let snapshot = capture(
            "SELECT * FROM dbo.",
            18,
            &[item("Users", CompletionKind::Table, 10)],
            14,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
        );
        let json: serde_json::Value = serde_json::from_slice(&snapshot.to_json_bytes()).unwrap();

        for key in [
            "sqlBeforeCursor",
            "sqlAfterCursor",
            "replaceStart",
            "selectedIndex",
            "suggestions",
            "catalog",
            "fkChains",
            "truncation",
            "procedureNames",
            "popupImagePng",
        ] {
            assert!(json.get(key).is_some(), "missing top-level key {key}");
        }
        let obj = &json["catalog"]["objects"][0];
        for key in [
            "database",
            "schema",
            "name",
            "kind",
            "columns",
            "parameters",
            "foreignKeys",
            "incomingForeignKeys",
            "backsSuggestion",
        ] {
            assert!(obj.get(key).is_some(), "missing catalog object key {key}");
        }
        assert!(json["catalog"].get("snippetNames").is_some());
        assert!(json["catalog"].get("totalObjects").is_some());
        assert_eq!(json["suggestions"][0]["insertText"], "Users");
    }

    fn join_item(label: &str, insert_text: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            insert_text: insert_text.to_string(),
            kind: CompletionKind::Join,
            source: CompletionSource::Inferred,
            score: 50,
            detail: "fk".to_string(),
            replace_span: None,
        }
    }

    /// A small model with overlapping column names: Users, Orders, and an
    /// unrelated Audit table that also has an `Id` column, plus a routine.
    fn backing_catalog() -> CompletionCatalog {
        let users =
            CompletionObject::table("dbo", "Users", vec!["Id".to_string(), "Email".to_string()]);
        let mut orders = CompletionObject::table(
            "sales",
            "Orders",
            vec!["Id".to_string(), "UserId".to_string(), "Total".to_string()],
        );
        orders.foreign_keys = vec![sqlly_intel::intellisense::FkRelation {
            column: "UserId".to_string(),
            target_schema: "dbo".to_string(),
            target_table: "Users".to_string(),
            target_column: "Id".to_string(),
            constraint: None,
        }];
        let audit =
            CompletionObject::table("dbo", "Audit", vec!["Id".to_string(), "Email".to_string()]);
        let get_user = CompletionObject {
            database: None,
            schema: Some("dbo".to_string()),
            name: "GetUser".to_string(),
            kind: sqlly_core::ObjectKind::StoredProcedure,
            columns: Vec::new(),
            parameters: vec!["@userId".to_string()],
            foreign_keys: Vec::new(),
        };
        CompletionCatalog {
            databases: Vec::new(),
            schemas: vec!["dbo".to_string(), "sales".to_string()],
            objects: vec![users, orders, audit, get_user],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        }
    }

    fn backing_names(sql: &str, items: &[CompletionItem]) -> Vec<String> {
        let catalog = backing_catalog();
        suggestion_backing_objects(sql, sql.len(), items, &catalog)
            .into_iter()
            .map(|i| catalog.objects[i].name.clone())
            .collect()
    }

    #[test]
    fn table_suggestions_back_their_own_object() {
        assert_eq!(
            backing_names(
                "SELECT * FROM ",
                &[item("sales.Orders", CompletionKind::Table, 90)]
            ),
            ["Orders"]
        );
        // A schema-qualified label never matches a same-named object in
        // another schema.
        assert!(
            backing_names(
                "SELECT * FROM ",
                &[item("sales.Users", CompletionKind::Table, 90)]
            )
            .is_empty()
        );
    }

    #[test]
    fn alias_qualified_column_suggestions_back_the_aliased_table() {
        assert_eq!(
            backing_names(
                "SELECT o. FROM sales.Orders o",
                &[item("o.Total", CompletionKind::Column, 70)]
            ),
            ["Orders"]
        );
    }

    #[test]
    fn bare_column_suggestions_prefer_the_statements_own_tables() {
        // `Email` lives in Users and Audit; the statement reads Users.
        assert_eq!(
            backing_names(
                "SELECT  FROM dbo.Users",
                &[item("Email", CompletionKind::Column, 70)]
            ),
            ["Users"]
        );
    }

    #[test]
    fn bare_column_suggestions_with_no_statement_tables_back_every_owner() {
        // `SELECT * f` style popups list columns with no FROM to scope them;
        // every object owning an offered column was part of what was shown.
        let mut names = backing_names("SELECT ", &[item("Email", CompletionKind::Column, 70)]);
        names.sort();
        assert_eq!(names, ["Audit", "Users"]);
    }

    #[test]
    fn fk_join_suggestions_back_both_tables() {
        let mut names = backing_names(
            "SELECT * FROM sales.Orders o JOIN ",
            &[join_item(
                "dbo.Users ON Id = o.UserId",
                "dbo.Users u ON u.Id = o.UserId",
            )],
        );
        names.sort();
        assert_eq!(names, ["Orders", "Users"]);
    }

    #[test]
    fn parameter_and_routine_suggestions_back_the_routine() {
        assert_eq!(
            backing_names(
                "EXEC dbo.GetUser ",
                &[item("@userId", CompletionKind::Parameter, 72)]
            ),
            ["GetUser"]
        );
        assert_eq!(
            backing_names(
                "EXEC ",
                &[item("dbo.GetUser", CompletionKind::Procedure, 72)]
            ),
            ["GetUser"]
        );
    }

    #[test]
    fn keyword_and_builtin_suggestions_back_nothing() {
        let mut keyword = item("Users", CompletionKind::Keyword, 10);
        keyword.source = CompletionSource::Keyword;
        let mut builtin = item("Users", CompletionKind::Function, 10);
        builtin.source = CompletionSource::Builtin;
        assert!(backing_names("SELECT ", &[keyword, builtin]).is_empty());
    }

    #[test]
    fn identifier_chains_split_dotted_and_bracketed_names() {
        assert_eq!(
            identifier_chains("[dbo].[Order Details] od ON od.Id = o.x"),
            vec![
                vec!["dbo".to_string(), "Order Details".to_string()],
                vec!["od".to_string()],
                vec!["ON".to_string()],
                vec!["od".to_string(), "Id".to_string()],
                vec!["o".to_string(), "x".to_string()],
            ]
        );
    }

    /// The ifb_9f91 shape: the popup listed live-metadata columns whose
    /// tables the statement never named, so the scoped snapshot used to
    /// drop them. Every backing table must now ride along, flagged.
    #[test]
    fn capture_always_includes_objects_backing_the_popup() {
        let catalog = backing_catalog();
        let snapshot = capture(
            "SELECT * f",
            10,
            &[
                item("Total", CompletionKind::Column, 70),
                item("GetUser", CompletionKind::Procedure, 40),
            ],
            9,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
        );
        let mut backing: Vec<&str> = snapshot
            .catalog
            .objects
            .iter()
            .filter(|o| o.backs_suggestion)
            .map(|o| o.name.as_str())
            .collect();
        backing.sort();
        assert_eq!(backing, ["GetUser", "Orders"]);
        let orders = snapshot
            .catalog
            .objects
            .iter()
            .find(|o| o.name == "Orders")
            .unwrap();
        assert_eq!(
            orders.columns,
            ["Id", "UserId", "Total"],
            "all columns ride"
        );
        assert!(
            snapshot
                .catalog
                .objects
                .iter()
                .filter(|o| !o.backs_suggestion)
                .all(|o| o.name != "Orders" && o.name != "GetUser")
        );
        let json: serde_json::Value = serde_json::from_slice(&snapshot.to_json_bytes()).unwrap();
        assert!(
            json["catalog"]["objects"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["name"] == "Orders" && o["backsSuggestion"] == true)
        );
    }

    /// The size budget cuts optional objects before suggestion-backing ones.
    #[test]
    fn truncation_keeps_suggestion_backing_objects() {
        let catalog = catalog_with_objects(400);
        let snapshot = capture_with_full_catalog(
            "SELECT * FROM ",
            14,
            &[item("dbo.Table399", CompletionKind::Table, 90)],
            14,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
            true,
        );
        let budget = snapshot.to_json_bytes().len() / 4;
        let json: serde_json::Value =
            serde_json::from_slice(&snapshot.to_json_bytes_within(budget)).unwrap();
        let objects = json["catalog"]["objects"].as_array().unwrap();
        assert!(objects.len() < 400, "the budget did trim objects");
        assert!(
            objects
                .iter()
                .any(|o| o["name"] == "Table399" && o["backsSuggestion"] == true),
            "the suggested table survives the trim"
        );
    }

    /// A captured procedure carries its schema and full parameter list —
    /// name, type, and OUTPUT — not just its name.
    #[test]
    fn procedures_ride_with_their_parameters() {
        let mut info = ProcedureInfo::new("dbo", "GetOrder");
        info.parameters = vec![
            sqlly_services::ProcedureParameter::new("@id", "int"),
            sqlly_services::ProcedureParameter::new("@total", "decimal(10,2)").output(),
        ];
        let catalog = CompletionCatalog {
            databases: Vec::new(),
            schemas: Vec::new(),
            objects: Vec::new(),
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let snapshot = capture(
            "EXEC dbo.",
            9,
            &[],
            9,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[info],
            None,
            false,
        );
        let json: serde_json::Value = serde_json::from_slice(&snapshot.to_json_bytes()).unwrap();
        assert_eq!(json["version"], 7);
        assert_eq!(json["procedureNames"], serde_json::json!(["GetOrder"]));
        assert_eq!(
            json["procedures"],
            serde_json::json!([{
                "schema": "dbo",
                "name": "GetOrder",
                "parameters": [
                    { "name": "@id", "dataType": "int", "isOutput": false },
                    { "name": "@total", "dataType": "decimal(10,2)", "isOutput": true },
                ],
            }])
        );
    }

    /// Over budget, the procedure list is cut before catalog objects, and
    /// procedures the query names outlast the rest.
    #[test]
    fn truncation_cuts_procedures_before_objects() {
        let procedures: Vec<ProcedureInfo> = (0..200)
            .map(|i| {
                let mut info = ProcedureInfo::new("dbo", format!("Proc{i}"));
                info.parameters = (0..20)
                    .map(|p| {
                        sqlly_services::ProcedureParameter::new(format!("@p{p}"), "nvarchar(4000)")
                    })
                    .collect();
                info
            })
            .collect();
        let catalog = CompletionCatalog {
            databases: Vec::new(),
            schemas: Vec::new(),
            objects: vec![CompletionObject::table(
                "dbo",
                "Proc199",
                vec!["Id".to_string()],
            )],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let snapshot = capture(
            "EXEC dbo.Proc199 ",
            17,
            &[],
            17,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &procedures,
            None,
            false,
        );
        let budget = snapshot.to_json_bytes().len() / 4;
        let json: serde_json::Value =
            serde_json::from_slice(&snapshot.to_json_bytes_within(budget)).unwrap();
        let kept = json["procedures"].as_array().unwrap();
        assert!(kept.len() < 200);
        assert_eq!(kept[0]["name"], "Proc199", "the named procedure survives");
        assert_eq!(json["catalog"]["objects"].as_array().unwrap().len(), 1);
        assert!(
            json["truncation"]["cut"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c.as_str().unwrap().contains("of 200 procedures"))
        );
    }
}
