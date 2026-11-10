//! Port: intellisense / completion.
//!
//! The trait exposes two completion entry points:
//! - [`IntellisenseService::complete`] — the hot path the editor calls on every
//!   keystroke. Synchronous, allocation-light, delegates straight to
//!   [`sqlly_intel::intellisense::complete`] and returns native [`CompletionItem`]s.
//! - [`IntellisenseService::complete_enriched`] — the shared path the gpui
//!   editor *and* the `sqlly-lsp` server both call. It runs the core
//!   completion, merges stored-procedure `EXEC` declaration snippets (see
//!   [`ProcedureInfo`]), then filters the result through the user's
//!   [`IntelliSenseRuleConfig`] (see [`apply_rules`]). Keeping this logic in the
//!   services layer (rather than duplicated per frontend) is what lets the
//!   editor and the LSP share one code path with no serialization.
//!
//! The additional pure helpers ([`slot_type`], [`procedures`], [`rules`]) are
//! moved here from the gpui app so they are reachable from every adapter.

pub mod catalog_build;
pub mod context;
pub mod hints;
pub mod joins;
pub mod model_build;
pub mod policy;
pub mod procedures;
pub mod rules;
pub mod slot_type;
pub mod value_lookup;

pub use catalog_build::CompletionState;
pub use catalog_build::build_completion_state;
pub use catalog_build::completion_state_from_snapshot;
pub use catalog_build::merge_columns;
pub use catalog_build::merge_procedure_parameters;
pub use context::CompletionContext;
pub use context::TableRef;
pub use context::ValueContext;
pub use context::ValueOperator;
pub use context::should_auto_trigger;
pub use hints::complete_hints;
pub use joins::join_candidates;
pub use model_build::build_catalog_from_metadata;
pub use model_build::build_model_snapshot;
pub use model_build::merge_columns_into_snapshot;
pub use model_build::merge_procedure_params_into_snapshot;
pub use model_build::procedures_from_snapshot;
pub use policy::PolicyDecision;
pub use policy::evaluate_policy;
pub use policy::filter_by_policy;
pub use procedures::ProcedureInfo;
pub use procedures::ProcedureParameter;
pub use rules::CountMatcher;
pub use rules::IntelliSenseClause;
pub use rules::IntelliSenseIntent;
pub use rules::IntelliSenseRule;
pub use rules::IntelliSenseRuleCondition;
pub use rules::IntelliSenseRuleConfig;
pub use rules::IntelliSenseRuleKeyboard;
pub use rules::IntelliSenseTriggerKind;
pub use rules::RuleContext;
pub use rules::apply_rules;
pub use rules::default_config_text;
pub use rules::kind_name;
pub use rules::parse;
pub use rules::strip_jsonc;
pub use slot_type::base_type;
pub use slot_type::placeholder_literal;
pub use sqlly_intel::intellisense::FkRelation;
use sqlly_intel::intellisense::{
    AcceptHistory, CompletionCatalog, CompletionItem, CompletionKind, CompletionSource, ValueKind,
    apply_identifier_quoting, apply_rank_boosts, column_value_kind_from_catalog,
    comparison_value_items, complete, current_prefix, default_snippets, qualified_object_score,
    quoted_path_at, value_kind_for_type,
};
pub use value_lookup::bracket;
pub use value_lookup::format_sql_literal;
pub use value_lookup::is_numeric_literal;
pub use value_lookup::matches_partial;
pub use value_lookup::value_lookup_cache_key;
pub use value_lookup::{
    ValueLookupEntry, contains_filter_clause, distinct_values_sql, distinct_values_sql_matching,
    distinct_word_lookup_sql, fk_values_sql, fk_values_sql_matching,
};

/// Score for a stored-procedure EXEC declaration snippet. Must rank above any
/// core completion item so the label-dedup in `complete_enriched` keeps the
/// richer snippet (with its `EXEC … = 0` body) over the bare dotted-path
/// object name. The max core score is `base(82) + CAMEL_SCORE(25) = 107`, so
/// this sits deliberately above it with a small margin.
const PROC_SNIPPET_SCORE: i32 = 110;

/// The canonical shared completion request. Both GPUI (in-process) and Swift
/// (via CLI) construct this and pass it to [`IntellisenseService::complete_enriched_response`].
#[derive(Debug, Clone)]
pub struct CompletionRequest<'a> {
    pub sql: &'a str,
    pub cursor: usize,
    pub trigger: IntelliSenseTriggerKind,
    pub manual: bool,
    pub catalog: &'a CompletionCatalog,
    pub procedures: &'a [ProcedureInfo],
    pub rules: &'a IntelliSenseRuleConfig,
    pub parameters: &'a [String],
    /// The connection's schema model, when the editor has one: supplies column
    /// types and nullability that the completion catalog (names only) lacks,
    /// so value templates fit the column (feedback ifb_9b71).
    pub model: Option<&'a sqlly_lang::sqlmodel::ModelSnapshot>,
    /// The connection's engine, when known. Accepted identifiers that need
    /// quoting (spaces, reserved words, a case the engine folds away) are
    /// inserted in its quoting style; `None` inserts catalog names verbatim.
    pub database_type: Option<sqlly_core::DatabaseType>,
}

impl<'a> CompletionRequest<'a> {
    /// Convenience builder for the common automatic-typing case.
    pub fn auto(
        sql: &'a str,
        cursor: usize,
        catalog: &'a CompletionCatalog,
        procedures: &'a [ProcedureInfo],
        rules: &'a IntelliSenseRuleConfig,
    ) -> Self {
        Self {
            sql,
            cursor,
            trigger: IntelliSenseTriggerKind::AutomaticTyping,
            manual: false,
            catalog,
            procedures,
            rules,
            parameters: &[],
            model: None,
            database_type: None,
        }
    }

    /// Convenience builder for the manual-trigger (Ctrl+Space) case.
    pub fn manual(
        sql: &'a str,
        cursor: usize,
        catalog: &'a CompletionCatalog,
        procedures: &'a [ProcedureInfo],
        rules: &'a IntelliSenseRuleConfig,
    ) -> Self {
        Self {
            sql,
            cursor,
            trigger: IntelliSenseTriggerKind::ManualShortcut,
            manual: true,
            catalog,
            procedures,
            rules,
            parameters: &[],
            model: None,
            database_type: None,
        }
    }
}

/// Session-scoped ranking inputs the editor supplies alongside a request.
/// Kept off `CompletionRequest` so CLI/LSP struct literals keep compiling.
#[derive(Debug, Clone, Copy, Default)]
pub struct RankingHints<'a> {
    pub accept_history: Option<&'a AcceptHistory>,
}

/// The canonical shared completion response. Carries the items, replacement
/// span, value context (if the cursor is in a value-literal position), and
/// debug/policy metadata that both GPUI and Swift consume.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub items: Vec<CompletionItem>,
    pub replace_start: usize,
    pub replace_end: usize,
    pub should_render: bool,
    pub reason: String,
    pub matched_rules: Vec<String>,
    pub debug_facts: Vec<String>,
    pub max_items: usize,
    /// Value context if the cursor is in a value-literal position (after `=` or `LIKE`).
    pub value_context: Option<ValueContext>,
}

impl CompletionResponse {
    /// Build a closed (empty) response with a reason.
    pub fn closed(reason: impl Into<String>) -> Self {
        Self {
            items: Vec::new(),
            replace_start: 0,
            replace_end: 0,
            should_render: false,
            reason: reason.into(),
            matched_rules: Vec::new(),
            debug_facts: Vec::new(),
            max_items: 0,
            value_context: None,
        }
    }

    /// Build a response from items + a completion context + policy decision.
    pub fn from_pipeline(
        items: Vec<CompletionItem>,
        ctx: &CompletionContext,
        decision: &PolicyDecision,
    ) -> Self {
        Self {
            items,
            replace_start: ctx.replace_start,
            replace_end: ctx.replace_end,
            should_render: decision.should_render,
            reason: decision.reason.clone(),
            matched_rules: decision.matched_rules.clone(),
            debug_facts: decision.debug_facts.clone(),
            max_items: decision.max_items,
            value_context: ctx.value_context.clone(),
        }
    }
}

/// Port: intellisense / completion. Synchronous and allocation-light — the hot
/// path the editor calls on every keystroke.
///
/// `complete_enriched` is the shared "additional intellisense" surface
/// (procedure snippets + user rules) that both the editor and the LSP server
/// consume.
pub trait IntellisenseService: Send + Sync {
    /// Core completion only — keywords, builtins, live/filesystem metadata,
    /// snippets, alias columns, parameters.
    fn complete(
        &self,
        sql: &str,
        cursor: usize,
        catalog: &CompletionCatalog,
    ) -> Vec<CompletionItem>;

    /// Enriched completion: core completion + procedure `EXEC` declaration
    /// snippets (filtered by the current prefix) + user-rule filtering.
    ///
    /// The caller supplies the in-scope procedures and the parsed rule config;
    /// the service merges the procedure snippets into the core list and then
    /// runs [`apply_rules`]. The result is sorted by score (desc) then label
    /// (asc) and de-duplicated by label so the caller can render it directly.
    fn complete_enriched(
        &self,
        sql: &str,
        cursor: usize,
        catalog: &CompletionCatalog,
        procedures: &[ProcedureInfo],
        rules: &IntelliSenseRuleConfig,
    ) -> Vec<CompletionItem> {
        let req = CompletionRequest::auto(sql, cursor, catalog, procedures, rules);
        self.complete_enriched_response(&req).items
    }

    /// The full enriched completion pipeline — context build, auto-trigger
    /// gate, policy evaluation, core completion, procedure snippets, policy
    /// filtering, rule application, sorting, dedup, and truncation.
    ///
    /// Returns a [`CompletionResponse`] with items, replacement span, and
    /// debug/policy metadata. Both GPUI (in-process) and Swift (via CLI)
    /// call this so behavior is identical across frontends.
    fn complete_enriched_response(&self, req: &CompletionRequest<'_>) -> CompletionResponse {
        self.complete_enriched_response_with(req, RankingHints::default())
    }

    /// Like [`Self::complete_enriched_response`] but accepts session-scoped
    /// ranking hints from the editor (accept history).
    fn complete_enriched_response_with(
        &self,
        req: &CompletionRequest<'_>,
        hints: RankingHints<'_>,
    ) -> CompletionResponse {
        // A path typed with quotes of its own (`[dbo].[Ord▏`) is completed on
        // a plain respelling, then mapped back so the insert lands inside the
        // user's quote instead of adding a second pair.
        if let Some(path) = quoted_path_at(req.sql, req.cursor, req.database_type) {
            let (plain_sql, plain_cursor) = path.plain_sql(req.sql, req.cursor);
            let plain_req = CompletionRequest {
                sql: &plain_sql,
                cursor: plain_cursor,
                database_type: None,
                ..req.clone()
            };
            let mut response = enriched_pipeline(self, &plain_req, hints);
            response.items = path.finish_items(response.items, req.cursor, req.database_type);
            response.replace_start = path.leaf_start;
            response.replace_end = req.cursor;
            response.value_context = None;
            return response;
        }
        let mut response = enriched_pipeline(self, req, hints);
        if let Some(db) = req.database_type {
            apply_identifier_quoting(&mut response.items, db);
        }
        response
    }

    fn default_snippets(&self) -> Vec<(String, String)>;
}

/// The enriched completion pipeline behind
/// [`IntellisenseService::complete_enriched_response_with`], before
/// identifier quoting.
fn enriched_pipeline<S: IntellisenseService + ?Sized>(
    svc: &S,
    req: &CompletionRequest<'_>,
    hints: RankingHints<'_>,
) -> CompletionResponse {
    let ctx =
        CompletionContext::build(req.sql, req.cursor, req.trigger, req.manual, req.parameters);

    // Hint completions: if the cursor is inside a recognized `--` hint
    // comment (result-layout canvas/placement or result-pivot grammar),
    // return hint items immediately and bypass SQL completion entirely.
    // Hints bypass the auto-trigger gate since they are in comments, not
    // SQL code.
    if let Some(hint_items) = complete_hints(req.sql, req.cursor, &[]) {
        let tail_len = ctx.replace_text.len();
        let replace_start = req.cursor.saturating_sub(tail_len);
        return CompletionResponse {
            items: hint_items,
            replace_start,
            replace_end: req.cursor,
            should_render: true,
            reason: "hint completion".to_string(),
            matched_rules: Vec::new(),
            debug_facts: vec!["hint: comment context".to_string()],
            max_items: 0,
            value_context: None,
        };
    }

    // Auto-trigger gate: suppress popup for automatic triggers that
    // don't make sense (line start, after *, completed keywords).
    if !req.manual && !should_auto_trigger(req.sql, req.cursor, req.trigger) {
        return CompletionResponse::closed("auto trigger suppressed");
    }

    // Evaluate policy (default decision by intent + user rule merging).
    let mut decision = evaluate_policy(&ctx, req.rules);
    if !decision.should_render {
        if req.manual {
            // An explicit request (Ctrl+Space / value lookup) always
            // renders; render-suppressing rules only gate automatic popups.
            decision.should_render = true;
        } else {
            return CompletionResponse::closed(&decision.reason);
        }
    }

    // Right after a FROM source the in-scope columns are offered, each
    // writing the WHERE clause (feedback ifb_92d9). The cursor still reads
    // as a FROM-source intent, which would filter them out.
    if sqlly_intel::intellisense::post_from_offers_columns(req.sql, req.cursor) {
        decision.allowed_kinds.insert(CompletionKind::Column);
    }

    // Core completion from sqlly-intellisense.
    let mut items = svc.complete(req.sql, req.cursor, req.catalog);

    // Built-in functions follow the connection's dialect (core completion
    // offers SQL Server's), and an argument slot that takes fixed words
    // (`DATEADD(|` → year, month, day, …) offers them first.
    let dialect = req
        .database_type
        .map(|t| t.family())
        .unwrap_or(sqlly_lang::sqlmodel::Dialect::SqlServer);
    if let Some(system_names) = sqlly_intel::intellisense::at_at_items(dialect, req.sql, req.cursor)
    {
        // After `@@` only the dialect's system functions / variables make
        // sense (feedback ifb_93e5).
        items = system_names;
        decision.allowed_kinds.insert(CompletionKind::Function);
        decision.allowed_kinds.insert(CompletionKind::Parameter);
    } else if let Some(settings) =
        sqlly_intel::intellisense::set_statement_items(dialect, req.sql, req.cursor)
    {
        // After `SET ▏` / `ALTER SESSION SET ▏` / `PRAGMA ▏` only what the
        // connection's engine can set applies — no functions, no tables —
        // and a head the engine does not use is steered to the one it does
        // (feedback ifb_0fbc).
        items = settings;
        decision.allowed_kinds = [CompletionKind::Keyword].into_iter().collect();
    } else {
        sqlly_intel::intellisense::replace_builtin_function_items(
            &mut items,
            dialect,
            current_prefix(req.sql, req.cursor),
        );
    }
    let slot_values = sqlly_intel::intellisense::slot_value_items(dialect, req.sql, req.cursor);
    if !slot_values.is_empty() {
        decision.allowed_kinds.insert(CompletionKind::Keyword);
        items.extend(slot_values);
    }

    // Merge stored-procedure EXEC declaration snippets (prefix-filtered only —
    // empty prefix skips every procedure to avoid O(all procs) snippet work).
    let prefix = current_prefix(req.sql, req.cursor).to_string();
    if !prefix.is_empty() {
        for proc in req.procedures {
            let label = proc.display_name();
            if let Some(fs) = qualified_object_score(&prefix, &label) {
                items.push(CompletionItem {
                    label,
                    insert_text: proc.declaration_snippet(),
                    kind: CompletionKind::Procedure,
                    source: CompletionSource::Snippet,
                    score: PROC_SNIPPET_SCORE + fs,
                    detail: proc.parameter_list_text(),
                    replace_span: None,
                });
            }
        }
    }

    // Merge FK join candidates (when in a JOIN/ON context).
    let join_items = joins::join_candidates_quoted(req.catalog, &ctx, req.database_type);
    if ctx.join_on_missing {
        // `JOIN T t ▏`: the joined table's condition comes first and nothing
        // else belongs here yet — no other tables, no columns (feedback
        // ifb_e275). The bare ON keyword stays for a hand-written condition.
        let prefix = current_prefix(req.sql, req.cursor).to_ascii_lowercase();
        let unresolved = join_items.is_empty();
        if unresolved {
            // The joined source has no catalog entry to pair keys from (a
            // temp table, CTE, or derived table): keep the clause's usual
            // keywords so `WH▏` still finds WHERE, with ON leading.
            items.retain(|i| {
                i.kind == CompletionKind::Keyword && !i.label.eq_ignore_ascii_case("on")
            });
            decision.allowed_kinds = [CompletionKind::Keyword].into_iter().collect();
        } else {
            items = join_items;
            decision.allowed_kinds = [CompletionKind::Join, CompletionKind::Keyword]
                .into_iter()
                .collect();
        }
        if "on".starts_with(&prefix) {
            // Ahead of the fallback keywords; below the conditions otherwise.
            let score = if unresolved {
                items.iter().map(|i| i.score).max().unwrap_or(0) + 1
            } else {
                0
            };
            items.push(CompletionItem {
                label: "ON".to_string(),
                insert_text: "ON ".to_string(),
                kind: CompletionKind::Keyword,
                source: CompletionSource::Keyword,
                score,
                detail: "join condition".to_string(),
                replace_span: None,
            });
        }
    } else if ctx.intent == IntelliSenseIntent::JoinCondition
        && !ctx.is_qualified_access
        && !join_items.is_empty()
    {
        // `ON ▏` with foreign keys linking the joined table to an earlier
        // source: offer those full conditions and nothing else — no tables,
        // no loose columns (feedback ifb_13aa). Without a matching key the
        // columns stay for a hand-written condition.
        items = join_items;
        decision.allowed_kinds = [CompletionKind::Join].into_iter().collect();
    } else {
        items.extend(join_items);
    }

    if let Some(vc) = &ctx.value_context
        && ctx.intent == IntelliSenseIntent::ValueLiteral
    {
        let mut kind = column_value_kind_from_catalog(
            req.catalog,
            vc.schema.as_deref(),
            vc.table.as_deref(),
            vc.column.as_deref(),
        );
        let model_column = req
            .model
            .and_then(|model| model_column(model, vc, &ctx.table_refs));
        if kind == ValueKind::Unknown
            && let Some(column) = model_column
        {
            kind = value_kind_for_type(column.ty.as_ref().map(|t| t.native.as_str()));
        }
        // NULL templates only for a column known to be nullable (feedback
        // ifb_8765; ifb_9b71 only dropped them for known NOT NULL).
        let not_null = !model_column.is_some_and(|column| column.nullable == Some(true));
        let op_span = operator_span(req.sql, vc.replace_start);
        items.extend(comparison_value_items(
            kind,
            vc.operator == ValueOperator::Like,
            vc.has_opening_quote,
            Some((vc.replace_start, vc.replace_end)),
            op_span,
            not_null,
        ));
    }

    let fk_related = joins::fk_related_table_keys(req.catalog, &ctx);
    for item in &mut items {
        if item.kind == CompletionKind::Table
            && fk_related.contains(&item.label.to_ascii_lowercase())
        {
            if item.detail.is_empty() {
                item.detail = "related via FK".to_string();
            }
            item.score += 10;
        }
    }

    apply_rank_boosts(&mut items, &ctx.prefix, hints.accept_history);

    // Filter by policy (allowed kinds for the current intent).
    items = filter_by_policy(items, &decision);

    // A word starting with a digit is a number literal: no function, column
    // or keyword can complete it, only the value templates (feedback
    // ifb_8765 — `= 1` offered `LOG10`).
    if context::typing_number_literal(req.sql, req.cursor) {
        items.retain(|item| item.kind == CompletionKind::Value);
    }

    // Apply user rules (additional filtering, blocking, max_items).
    let rule_ctx = RuleContext {
        prefix: ctx.prefix.clone(),
        clause: ctx.clause,
        trigger: ctx.trigger,
        has_qualifier: ctx.is_qualified_access,
        has_aliases: ctx.has_aliases,
        statement_lower: ctx.statement_lower.clone(),
        sql: req.sql,
        table_count: ctx.table_count,
        alias_count: ctx.alias_count,
        manual: ctx.manual,
    };
    items = apply_rules(items, &rule_ctx, req.rules);

    // Sort by score (desc) then label (asc), deduplicate by label.
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
    items.dedup_by(|a, b| a.label == b.label);

    // Truncate to max_items.
    if decision.max_items > 0 {
        items.truncate(decision.max_items);
    }

    CompletionResponse::from_pipeline(items, &ctx, &decision)
}

/// The model's definition of the column a value is being typed for: the
/// value context's own table when it names one, else the first of the query's
/// tables that has a column of that name. Names match case-insensitively.
fn model_column<'m>(
    model: &'m sqlly_lang::sqlmodel::ModelSnapshot,
    vc: &ValueContext,
    table_refs: &[TableRef],
) -> Option<&'m sqlly_lang::sqlmodel::ColumnDef> {
    let column = vc.column.as_deref()?;
    let candidates: Vec<(Option<&str>, &str)> = match vc.table.as_deref() {
        Some(table) => vec![(vc.schema.as_deref(), table)],
        None => table_refs
            .iter()
            .map(|t| (t.schema.as_deref(), t.name.as_str()))
            .collect(),
    };
    candidates.into_iter().find_map(|(schema, table)| {
        model.objects().find_map(|entry| {
            let name_matches = entry.reference.name.spelling().eq_ignore_ascii_case(table);
            let schema_matches = schema.is_none_or(|schema| {
                entry
                    .reference
                    .namespace
                    .as_ref()
                    .is_some_and(|ns| ns.spelling().eq_ignore_ascii_case(schema))
            });
            if !(name_matches && schema_matches) {
                return None;
            }
            entry
                .table()?
                .columns
                .iter()
                .find(|c| c.name.spelling().eq_ignore_ascii_case(column))
        })
    })
}

fn operator_span(sql: &str, replace_start: usize) -> Option<(usize, usize)> {
    let bytes = sql.as_bytes();
    let mut idx = replace_start.min(bytes.len());
    let mut steps = 0usize;
    while idx > 0 && steps < 8 {
        let b = bytes[idx - 1];
        if b.is_ascii_whitespace() {
            idx -= 1;
            steps += 1;
        } else if b == b'=' {
            return Some((idx - 1, replace_start));
        } else {
            break;
        }
    }
    None
}

#[derive(Clone, Copy)]
pub struct DefaultIntellisense;

impl IntellisenseService for DefaultIntellisense {
    fn complete(
        &self,
        sql: &str,
        cursor: usize,
        catalog: &CompletionCatalog,
    ) -> Vec<CompletionItem> {
        complete(sql, cursor, catalog)
    }

    fn default_snippets(&self) -> Vec<(String, String)> {
        default_snippets()
    }
}

impl Default for DefaultIntellisense {
    fn default() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::ObjectKind;
    use sqlly_intel::intellisense::{AcceptHistory, CompletionObject, FkRelation};
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::Instant;
    #[cfg(target_arch = "wasm32")]
    use web_time::Instant;

    fn catalog() -> CompletionCatalog {
        CompletionCatalog {
            databases: vec!["appdb".to_string()],
            schemas: vec!["dbo".to_string()],
            objects: vec![
                CompletionObject {
                    database: Some("appdb".to_string()),
                    schema: Some("dbo".to_string()),
                    name: "Orders".to_string(),
                    kind: ObjectKind::Table,
                    columns: vec!["OrderId".to_string()],
                    parameters: Vec::new(),
                    foreign_keys: Vec::new(),
                },
                CompletionObject {
                    database: Some("appdb".to_string()),
                    schema: Some("dbo".to_string()),
                    name: "GetUser".to_string(),
                    kind: ObjectKind::StoredProcedure,
                    columns: Vec::new(),
                    parameters: vec!["@id".to_string()],
                    foreign_keys: Vec::new(),
                },
            ],
            filesystem_schemas: Vec::new(),
            snippets: default_snippets(),
        }
    }

    #[test]
    fn complete_enriched_merges_procedure_snippet() {
        let svc = DefaultIntellisense;
        let mut proc = ProcedureInfo::new("dbo", "GetUser");
        proc.parameters.push(ProcedureParameter::new("@id", "int"));
        let cfg = parse(&default_config_text()).unwrap();
        let items = svc.complete_enriched("EXEC dbo.Get", 12, &catalog(), &[proc], &cfg);
        assert!(
            items.iter().any(|i| i.label == "dbo.GetUser"
                && i.kind == CompletionKind::Procedure
                && i.insert_text.contains("EXEC [dbo].[GetUser] @id = 0")),
            "expected the EXEC declaration snippet, got {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );
    }

    /// Feedback ifb_93e5: in the editor pipeline `@@` offers the connected
    /// dialect's system names (SQL Server `@@` functions, MySQL `@@`
    /// variables, nothing for Postgres), filtered by what follows; one `@`
    /// offers the user's own variables and never a system name.
    #[test]
    fn complete_enriched_at_at_offers_the_dialects_system_names() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = catalog();
        let run = |typed: &str, db: sqlly_core::DatabaseType, trigger, manual| {
            let sql = format!(
                "declare @Test bigint = 10;\nSELECT\n\ttop 10\n\t{typed}\nFROM\n\tSlot.Slot s\n"
            );
            let cursor = sql.find(&format!("\t{typed}\n")).unwrap() + 1 + typed.len();
            let req = CompletionRequest {
                sql: &sql,
                cursor,
                trigger,
                manual,
                catalog: &catalog,
                procedures: &[],
                rules: &cfg,
                parameters: &[],
                model: None,
                database_type: Some(db),
            };
            svc.complete_enriched_response(&req)
                .items
                .into_iter()
                .map(|i| i.label)
                .collect::<Vec<_>>()
        };
        use sqlly_core::DatabaseType::*;
        let manual = IntelliSenseTriggerKind::ManualShortcut;
        let typing = IntelliSenseTriggerKind::AutomaticTyping;
        let all = run("@@", SqlServer, manual, true);
        assert!(all.contains(&"@@ROWCOUNT".to_string()), "{all:?}");
        assert!(all.iter().all(|l| l.starts_with("@@")), "{all:?}");
        let filtered = run("@@row", SqlServer, typing, false);
        assert_eq!(
            filtered.first().map(String::as_str),
            Some("@@ROWCOUNT"),
            "{filtered:?}"
        );
        let mysql = run("@@sql", MySql, typing, false);
        assert_eq!(
            mysql.first().map(String::as_str),
            Some("@@sql_mode"),
            "{mysql:?}"
        );
        assert!(!run("@@", MySql, manual, true).contains(&"@@ROWCOUNT".to_string()));
        assert!(run("@@", Postgres, manual, true).is_empty());
        let single = run("@", SqlServer, IntelliSenseTriggerKind::AtSign, false);
        assert!(single.contains(&"@Test".to_string()), "{single:?}");
        assert!(!single.iter().any(|l| l.starts_with("@@")), "{single:?}");
    }

    /// Typing `COUNT(` in a SELECT list: the popup leads with the query's
    /// columns rather than a wall of `@@` system functions.
    #[test]
    fn complete_enriched_inside_a_function_call_leads_with_columns() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = catalog();
        let sql = "SELECT\n    top 10\n    o.OrderId,\n    COUNT(\nFROM\n    dbo.Orders o\n";
        let cursor = sql.find("COUNT(").unwrap() + 6;
        for (trigger, manual) in [
            (IntelliSenseTriggerKind::AutomaticTyping, false),
            (IntelliSenseTriggerKind::ManualShortcut, true),
        ] {
            let req = CompletionRequest {
                sql,
                cursor,
                trigger,
                manual,
                catalog: &catalog,
                procedures: &[],
                rules: &cfg,
                parameters: &[],
                model: None,
                database_type: Some(sqlly_core::DatabaseType::SqlServer),
            };
            let items = svc.complete_enriched_response(&req).items;
            assert_eq!(
                items.first().map(|i| i.label.as_str()),
                Some("o.OrderId"),
                "{trigger:?}: {:?}",
                items.iter().map(|i| &i.label).take(10).collect::<Vec<_>>()
            );
            assert!(
                !items.iter().take(5).any(|i| i.label.starts_with("@@")),
                "{trigger:?}: @@ functions must not lead"
            );
        }
    }

    /// Feedback ifb_9f91: after a finished SELECT list (`SELECT⏎⇥*⏎f▏`, a
    /// manual trigger) the editor pipeline leads with FROM, offers tables
    /// whose insert writes the FROM clause, and offers no columns.
    #[test]
    fn complete_enriched_after_select_list_leads_with_from_and_no_columns() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let mut catalog = catalog();
        catalog.objects.push(CompletionObject {
            database: Some("appdb".to_string()),
            schema: Some("Forms".to_string()),
            name: "Form".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["FormId".to_string(), "FieldData".to_string()],
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        });
        let sql = "SELECT\n\t*\nf";
        let req = CompletionRequest {
            sql,
            cursor: sql.len(),
            trigger: IntelliSenseTriggerKind::ManualShortcut,
            manual: true,
            catalog: &catalog,
            procedures: &[],
            rules: &cfg,
            parameters: &[],
            model: None,
            database_type: None,
        };
        let items = svc.complete_enriched_response(&req).items;
        assert_eq!(items.first().map(|i| i.label.as_str()), Some("FROM"));
        let form = items
            .iter()
            .find(|i| i.label == "Forms.Form")
            .expect("Forms.Form offered");
        assert_eq!(form.insert_text, "FROM\n\tForms.Form");
        assert!(
            items.iter().all(|i| i.kind != CompletionKind::Column),
            "no columns after a finished select list, got {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    /// Feedback ifb_92d9: after a FROM source on its own line
    /// (`…Shared.Settings⏎wh▏`, a manual trigger) the editor pipeline leads
    /// with WHERE, offers the source's columns with inserts that write the
    /// WHERE clause, and offers no tables or procedures.
    #[test]
    fn complete_enriched_after_from_source_leads_with_where_then_columns() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let mut catalog = catalog();
        catalog.objects.push(CompletionObject {
            database: Some("appdb".to_string()),
            schema: Some("Shared".to_string()),
            name: "Settings".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["Id".to_string(), "Name".to_string(), "Category".to_string()],
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        });
        let request = |sql: &'static str| CompletionRequest {
            sql,
            cursor: sql.len(),
            trigger: IntelliSenseTriggerKind::ManualShortcut,
            manual: true,
            catalog: &catalog,
            procedures: &[],
            rules: &cfg,
            parameters: &[],
            model: None,
            database_type: None,
        };

        let items = svc
            .complete_enriched_response(&request("SELECT\n\t*\nfrom\n\tShared.Settings\nwh"))
            .items;
        assert_eq!(items.first().map(|i| i.label.as_str()), Some("WHERE"));

        let items = svc
            .complete_enriched_response(&request("SELECT\n\t*\nfrom\n\tShared.Settings s\n"))
            .items;
        assert_eq!(items.first().map(|i| i.label.as_str()), Some("WHERE"));
        let category = items
            .iter()
            .find(|i| i.label == "s.Category")
            .expect("alias-qualified column offered");
        assert_eq!(category.insert_text, "WHERE\n\ts.Category");
        assert!(
            items.iter().all(|i| !matches!(
                i.kind,
                CompletionKind::Table | CompletionKind::Procedure | CompletionKind::Function
            )),
            "no catalog objects after a FROM source, got {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    /// Feedback ifb_e275 / ifb_13aa: right after `INNER JOIN Accounting.FinancialAccountingKey
    /// AS fak` with no `ON` yet, the popup offered joins to other tables. The
    /// joined table needs its condition first: every foreign key it shares
    /// with an earlier source is offered as `ON fak.X = a.X`, plus the bare
    /// ON keyword, and no other tables.
    #[test]
    fn complete_enriched_after_join_alias_offers_its_on_conditions_only() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let fk = |column: &str, table: &str, target: &str| FkRelation {
            column: column.to_string(),
            target_schema: "Accounting".to_string(),
            target_table: table.to_string(),
            target_column: target.to_string(),
            constraint: None,
        };
        let mut account = CompletionObject::table(
            "Accounting",
            "Account",
            vec!["AccountId".into(), "Name".into(), "BusinessUnitId".into()],
        );
        account.foreign_keys = vec![fk("BusinessUnitId", "BusinessUnit", "BusinessUnitId")];
        let mut key = CompletionObject::table(
            "Accounting",
            "FinancialAccountingKey",
            vec![
                "FinancialAccountingKeyId".into(),
                "AccountId".into(),
                "BusinessUnitId".into(),
            ],
        );
        key.foreign_keys = vec![
            fk("AccountId", "Account", "AccountId"),
            fk("BusinessUnitId", "BusinessUnit", "BusinessUnitId"),
        ];
        let mut journal_line = CompletionObject::table(
            "Accounting",
            "JournalLine",
            vec!["JournalLineId".into(), "FinancialAccountingKeyId".into()],
        );
        journal_line.foreign_keys = vec![fk(
            "FinancialAccountingKeyId",
            "FinancialAccountingKey",
            "FinancialAccountingKeyId",
        )];
        let unit = CompletionObject::table(
            "Accounting",
            "BusinessUnit",
            vec!["BusinessUnitId".into(), "Name".into()],
        );
        let catalog = CompletionCatalog {
            schemas: vec!["Accounting".into()],
            objects: vec![account, key, journal_line, unit],
            ..Default::default()
        };
        let complete = |sql: &str| {
            let cursor = sql.find('▏').expect("caret marker");
            let sql = sql.replace('▏', "");
            svc.complete_enriched_response(&CompletionRequest {
                sql: &sql,
                cursor,
                trigger: IntelliSenseTriggerKind::ManualShortcut,
                manual: true,
                catalog: &catalog,
                procedures: &[],
                rules: &cfg,
                parameters: &[],
                model: None,
                database_type: None,
            })
            .items
        };

        let items = complete(
            "SELECT\n\t*\nFROM\n\tAccounting.Account AS a\n\tINNER JOIN Accounting.FinancialAccountingKey AS fak\n\t\t▏\nwhere\n\ta.Name like '%x%'",
        );
        let first = items.first().expect("a suggestion");
        assert_eq!(first.label, "fak.AccountId = a.AccountId");
        assert_eq!(first.insert_text, "ON fak.AccountId = a.AccountId");
        assert!(
            items.iter().any(|i| i.label == "ON"),
            "the bare ON keyword stays"
        );
        assert!(
            items
                .iter()
                .all(|i| i.kind == CompletionKind::Keyword || i.insert_text.starts_with("ON ")),
            "nothing but conditions for the joined table, got {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );

        // Every earlier source is paired, not just the previous one.
        let items = complete(
            "SELECT * FROM Accounting.BusinessUnit bu JOIN Accounting.Account a ON a.BusinessUnitId = bu.BusinessUnitId JOIN Accounting.FinancialAccountingKey fak ▏",
        );
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"fak.AccountId = a.AccountId"),
            "{labels:?}"
        );
        assert!(
            labels.contains(&"fak.BusinessUnitId = bu.BusinessUnitId"),
            "{labels:?}"
        );

        // Feedback ifb_13aa: after `ON ▏` only the full conditions, the
        // joined table's alias first and the nearest source preferred — no
        // tables, no loose columns.
        let items = complete(
            "SELECT * FROM Accounting.BusinessUnit bu JOIN Accounting.Account a ON a.BusinessUnitId = bu.BusinessUnitId JOIN Accounting.FinancialAccountingKey AS fak\n\tON ▏\nwhere a.Name like '%x%'",
        );
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "fak.AccountId = a.AccountId",
                "fak.BusinessUnitId = bu.BusinessUnitId"
            ]
        );
        assert!(items.iter().all(|i| i.kind == CompletionKind::Join));
        assert_eq!(items[0].insert_text, "fak.AccountId = a.AccountId");

        // Typing the alias itself is not a condition yet.
        let items = complete(
            "SELECT * FROM Accounting.Account a JOIN Accounting.FinancialAccountingKey fa▏",
        );
        assert!(items.iter().all(|i| !i.insert_text.starts_with("ON ")));
    }

    /// After `JOIN T t ▏` whose source has no catalog entry (a temp table,
    /// CTE, or derived table) there are no key conditions to offer; the
    /// clause's usual keywords stay so `WH▏` still finds WHERE, with ON first.
    #[test]
    fn complete_enriched_after_unresolved_join_alias_keeps_the_clause_keywords() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = catalog();
        let complete = |sql: &str| {
            let cursor = sql.find('▏').expect("caret marker");
            let sql = sql.replace('▏', "");
            svc.complete_enriched_response(&CompletionRequest {
                sql: &sql,
                cursor,
                trigger: IntelliSenseTriggerKind::ManualShortcut,
                manual: true,
                catalog: &catalog,
                procedures: &[],
                rules: &cfg,
                parameters: &[],
                model: None,
                database_type: None,
            })
            .items
        };

        let items = complete("SELECT * FROM dbo.Orders o INNER JOIN #tmp t ▏");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels.first(), Some(&"ON"), "{labels:?}");
        assert!(labels.contains(&"WHERE"), "{labels:?}");
        assert!(
            items.iter().all(|i| i.kind == CompletionKind::Keyword),
            "no tables or columns before the join's condition, got {labels:?}"
        );

        let items = complete("SELECT * FROM dbo.Orders o INNER JOIN #tmp t WH▏");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"WHERE"), "{labels:?}");
        assert!(!labels.contains(&"ON"), "{labels:?}");
    }

    /// A CROSS JOIN or NATURAL JOIN takes no ON condition, so after its
    /// source and alias the usual keywords are offered, WHERE among them.
    #[test]
    fn complete_enriched_after_a_cross_or_natural_join_offers_the_usual_keywords() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = catalog();
        let complete = |sql: &str| {
            let cursor = sql.find('▏').expect("caret marker");
            let sql = sql.replace('▏', "");
            svc.complete_enriched_response(&CompletionRequest {
                sql: &sql,
                cursor,
                trigger: IntelliSenseTriggerKind::ManualShortcut,
                manual: true,
                catalog: &catalog,
                procedures: &[],
                rules: &cfg,
                parameters: &[],
                model: None,
                database_type: None,
            })
            .items
        };
        for sql in [
            "SELECT * FROM dbo.Orders o CROSS JOIN dbo.Orders p WH▏",
            "SELECT * FROM dbo.Orders o NATURAL JOIN dbo.Orders p WH▏",
            "SELECT * FROM dbo.Orders o NATURAL LEFT OUTER JOIN dbo.Orders p WH▏",
        ] {
            let items = complete(sql);
            let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
            assert!(labels.contains(&"WHERE"), "{sql}: {labels:?}");
            assert!(!labels.contains(&"ON"), "{sql}: {labels:?}");
        }
        let items = complete("SELECT * FROM dbo.Orders o CROSS JOIN dbo.Orders p ▏");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"WHERE"), "{labels:?}");
    }

    /// Feedback ifb_3798: `update ▏` after a finished `SELECT … FROM
    /// Accounting.ReportingPeriod AS arp` is a fresh statement. The popup used
    /// to show only the general keywords (the context read `update` as the
    /// word after the alias and fell to a keywords-only General intent); it
    /// now leads with the tables and carries none of the SELECT's clauses.
    #[test]
    fn complete_enriched_update_after_a_finished_select_offers_tables() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let mut catalog = catalog();
        catalog.objects.push(CompletionObject {
            database: Some("appdb".to_string()),
            schema: Some("Accounting".to_string()),
            name: "ReportingPeriod".to_string(),
            kind: ObjectKind::Table,
            columns: vec![
                "ReportingPeriodId".to_string(),
                "TenantId".to_string(),
                "Name".to_string(),
            ],
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        });
        for sql in [
            "SELECT\n\t*\nFROM\n\tAccounting.ReportingPeriod AS arp\n\t\nupdate ",
            "SELECT\n\t*\nFROM\n\tAccounting.ReportingPeriod\n\t\nupdate ",
            "update ",
        ] {
            let req = CompletionRequest {
                sql,
                cursor: sql.len(),
                trigger: IntelliSenseTriggerKind::ManualShortcut,
                manual: true,
                catalog: &catalog,
                procedures: &[],
                rules: &cfg,
                parameters: &[],
                model: None,
                database_type: None,
            };
            let items = svc.complete_enriched_response(&req).items;
            let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
            assert_eq!(
                items.first().map(|i| i.kind),
                Some(CompletionKind::Table),
                "{sql:?}: {labels:?}"
            );
            assert!(
                labels.contains(&"Accounting.ReportingPeriod"),
                "{sql:?}: {labels:?}"
            );
            for bad in [
                "WHERE",
                "JOIN",
                "INNER JOIN",
                "GROUP BY",
                "ORDER BY",
                "arp.Name",
                "Name",
            ] {
                assert!(!labels.contains(&bad), "{sql:?} offered {bad}: {labels:?}");
            }
        }
    }

    /// Feedback ifb_0fbc: `SET ▏` offered the user's functions and the
    /// built-ins. It now offers the connection's settable options only, and
    /// on an engine whose settings statement is spelled differently it
    /// leads with the item that rewrites the head: `ALTER SESSION SET` on
    /// Oracle, `PRAGMA` on SQLite, and `SET` again when `ALTER SESSION SET`
    /// is typed on SQL Server.
    #[test]
    fn complete_enriched_set_offers_the_engines_settings_only() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let mut catalog = catalog();
        catalog.objects.push(CompletionObject {
            database: Some("appdb".to_string()),
            schema: Some("Shared".to_string()),
            name: "fRoundCurrency".to_string(),
            kind: ObjectKind::ScalarFunction,
            columns: Vec::new(),
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        });
        let respond = |sql: &str, database_type: sqlly_core::DatabaseType| {
            let req = CompletionRequest {
                sql,
                cursor: sql.len(),
                trigger: IntelliSenseTriggerKind::AutomaticTyping,
                manual: false,
                catalog: &catalog,
                procedures: &[],
                rules: &cfg,
                parameters: &[],
                model: None,
                database_type: Some(database_type),
            };
            svc.complete_enriched_response(&req).items
        };

        let items = respond("SET ", sqlly_core::DatabaseType::SqlServer);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"NOCOUNT"), "{labels:?}");
        assert!(
            items.iter().all(|i| i.kind == CompletionKind::Keyword),
            "{labels:?}"
        );
        for bad in ["Shared.fRoundCurrency", "ABS", "GETDATE", "dbo.Orders"] {
            assert!(!labels.contains(&bad), "SET offered {bad}: {labels:?}");
        }

        let items = respond("SET ", sqlly_core::DatabaseType::Oracle);
        let first = items.first().expect("steering item");
        assert_eq!(first.label, "ALTER SESSION SET");
        assert_eq!(first.insert_text, "ALTER SESSION SET ");
        assert_eq!(first.replace_span, Some((0, "SET ".len())));
        assert!(
            first.detail.contains("ALTER SESSION SET"),
            "{}",
            first.detail
        );

        let items = respond("ALTER SESSION SET ", sqlly_core::DatabaseType::Oracle);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"NLS_DATE_FORMAT"), "{labels:?}");
        assert!(!labels.contains(&"ALTER SESSION SET"), "{labels:?}");

        let items = respond("SET ", sqlly_core::DatabaseType::Sqlite);
        assert_eq!(items.first().map(|i| i.label.as_str()), Some("PRAGMA"));
        let items = respond("PRAGMA jou", sqlly_core::DatabaseType::Sqlite);
        assert_eq!(
            items.first().map(|i| i.label.as_str()),
            Some("journal_mode")
        );

        let items = respond("ALTER SESSION SET ", sqlly_core::DatabaseType::SqlServer);
        let first = items.first().expect("steering item");
        assert_eq!(first.label, "SET");
        assert_eq!(first.replace_span, Some((0, "ALTER SESSION SET ".len())));
        assert!(first.detail.contains("SQL Server"), "{}", first.detail);
    }

    /// Feedback ifb_9b71: at `Id = ▏` for a `bigint NOT NULL` column (types
    /// from the model, the catalog has names only) the value templates fit the
    /// column: the number, and no text literal or NULL tests. A nullable text
    /// column keeps the text literal and the NULL tests.
    #[test]
    fn value_templates_fit_the_models_column_type_and_nullability() {
        use crate::intellisense::model_build::{BulkColumnInfo, typed_column_defs};
        use sqlly_lang::sqlmodel::{
            Dialect, Fidelity, IncompleteRegions, ModelSnapshot, ObjectBody, ObjectFragment,
            ObjectKind as ModelKind, ObjectRef, Provenance, SqlName, TableBody,
        };

        let dialect = Dialect::SqlServer;
        let column = |name: &str, ty: &str, nullable: bool| BulkColumnInfo {
            name: name.into(),
            type_name: Some(ty.into()),
            nullable: Some(nullable),
            identity: false,
        };
        let fragment = ObjectFragment {
            reference: ObjectRef::new(
                None,
                Some(SqlName::new("Shared", dialect, false)),
                SqlName::new("Settings", dialect, false),
            ),
            kind: ModelKind::Table,
            dialect,
            body: ObjectBody::Table(TableBody {
                columns: typed_column_defs(
                    &[
                        column("Id", "bigint", false),
                        column("Description", "nvarchar(500)", true),
                    ],
                    dialect,
                ),
                ..TableBody::default()
            }),
            provenance: Provenance::Database {
                profile: "p1".into(),
            },
            fidelity: Fidelity::Exact,
            references: vec![],
        };
        let model = ModelSnapshot::empty()
            .apply(
                1,
                vec![fragment],
                &std::collections::BTreeSet::new(),
                IncompleteRegions::default(),
            )
            .0;

        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let mut catalog = catalog();
        catalog.objects.push(CompletionObject {
            database: Some("appdb".to_string()),
            schema: Some("Shared".to_string()),
            name: "Settings".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["Id".to_string(), "Description".to_string()],
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        });
        let values = |sql: &str| -> Vec<String> {
            let mut req = CompletionRequest::auto(sql, sql.len(), &catalog, &[], &cfg);
            req.model = Some(&model);
            svc.complete_enriched_response(&req)
                .items
                .into_iter()
                .filter(|i| i.kind == CompletionKind::Value)
                .map(|i| i.label)
                .collect()
        };

        let id = values("SELECT\n\t*\nfrom\n\tShared.Settings\nwhere\n\tId = ");
        assert_eq!(
            id,
            vec!["0".to_string()],
            "bigint NOT NULL offers only a number"
        );

        let description = values("SELECT * FROM Shared.Settings s WHERE s.Description = ");
        for expected in ["''", "NULL", "IS NULL", "IS NOT NULL"] {
            assert!(
                description.iter().any(|l| l == expected),
                "nullable text keeps `{expected}`, got {description:?}"
            );
        }
        assert!(!description.iter().any(|l| l == "0"), "got {description:?}");
    }

    /// Feedback ifb_8765: at `po.PaymentMethodId = 1▏` the popup opened on its
    /// own with `LOG10` highlighted (contains-match on the digit). Typing a
    /// number never auto-opens it, a manual request offers only the value
    /// templates, and the NULL templates follow the column's nullability.
    #[test]
    fn typing_a_number_offers_no_functions_and_null_follows_nullability() {
        use crate::intellisense::model_build::{BulkColumnInfo, typed_column_defs};
        use sqlly_lang::sqlmodel::{
            Dialect, Fidelity, IncompleteRegions, ModelSnapshot, ObjectBody, ObjectFragment,
            ObjectKind as ModelKind, ObjectRef, Provenance, SqlName, TableBody,
        };

        let dialect = Dialect::SqlServer;
        let column = |name: &str, nullable: bool| BulkColumnInfo {
            name: name.into(),
            type_name: Some("int".into()),
            nullable: Some(nullable),
            identity: false,
        };
        let fragment = ObjectFragment {
            reference: ObjectRef::new(
                None,
                Some(SqlName::new("Accounting", dialect, false)),
                SqlName::new("PurchaseOrder", dialect, false),
            ),
            kind: ModelKind::Table,
            dialect,
            body: ObjectBody::Table(TableBody {
                columns: typed_column_defs(
                    &[
                        column("PaymentMethodId", false),
                        column("PaymentStatusId", true),
                    ],
                    dialect,
                ),
                ..TableBody::default()
            }),
            provenance: Provenance::Database {
                profile: "p1".into(),
            },
            fidelity: Fidelity::Exact,
            references: vec![],
        };
        let model = ModelSnapshot::empty()
            .apply(
                1,
                vec![fragment],
                &std::collections::BTreeSet::new(),
                IncompleteRegions::default(),
            )
            .0;

        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let mut catalog = catalog();
        catalog.objects.push(CompletionObject {
            database: Some("appdb".to_string()),
            schema: Some("Accounting".to_string()),
            name: "PurchaseOrder".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["PaymentMethodId".to_string(), "PaymentStatusId".to_string()],
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        });
        let respond = |sql: &str, manual: bool| {
            let mut req = if manual {
                CompletionRequest::manual(sql, sql.len(), &catalog, &[], &cfg)
            } else {
                CompletionRequest::auto(sql, sql.len(), &catalog, &[], &cfg)
            };
            req.model = Some(&model);
            svc.complete_enriched_response(&req)
        };
        let head = "select * from Accounting.PurchaseOrder AS po where po.InternalReferenceNumber in('OCMO-000003'\n,'OCMO-000422')\nand ";
        let labels = |sql: &str, manual: bool| -> Vec<String> {
            respond(sql, manual)
                .items
                .into_iter()
                .map(|i| i.label)
                .collect()
        };

        for typed in ["1", "10", "1."] {
            let sql = format!("{head}po.PaymentMethodId = {typed}");
            assert!(
                respond(&sql, false).items.is_empty(),
                "typing `{typed}` must not auto-open the popup"
            );
            let manual = respond(&sql, true).items;
            assert!(
                manual.iter().all(|i| i.kind == CompletionKind::Value),
                "a number offers only value templates, got {:?}",
                manual.iter().map(|i| &i.label).collect::<Vec<_>>()
            );
        }

        // Manual request with nothing typed yet still offers functions.
        let empty = respond(&format!("{head}po.PaymentMethodId = "), true).items;
        assert!(
            empty.iter().any(|i| i.kind == CompletionKind::Function),
            "an empty value slot keeps functions on request"
        );

        let not_null = labels(&format!("{head}po.PaymentMethodId = "), true);
        for null_template in ["NULL", "IS NULL", "IS NOT NULL"] {
            assert!(
                !not_null.iter().any(|l| l == null_template),
                "NOT NULL column must not offer `{null_template}`"
            );
        }
        let nullable = labels(&format!("{head}po.PaymentStatusId = "), true);
        for null_template in ["NULL", "IS NULL", "IS NOT NULL"] {
            assert!(
                nullable.iter().any(|l| l == null_template),
                "nullable column keeps `{null_template}`, got {nullable:?}"
            );
        }
    }

    #[test]
    fn typing_number_literal_spots_digit_led_words_only() {
        use context::typing_number_literal as number;
        for sql in ["x = 1", "x = 10", "x = 1.", "x = 1.5", "x = 1e3", "(3"] {
            assert!(number(sql, sql.len()), "{sql}");
        }
        for sql in ["x = ", "x = log1", "t1.", "Col2", "x = @p1", ""] {
            assert!(!number(sql, sql.len()), "{sql}");
        }
    }

    /// Feedback ifb_75bd: in a GROUP BY list the editor pipeline puts the
    /// ungrouped select-list column first, marked required, and offers no
    /// tables.
    #[test]
    fn complete_enriched_group_by_leads_with_the_required_column() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let mut catalog = catalog();
        catalog.objects.push(CompletionObject {
            database: Some("appdb".to_string()),
            schema: Some("Shared".to_string()),
            name: "Settings".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["Id".to_string(), "Category".to_string(), "Name".to_string()],
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        });
        let sql = "SELECT\n\tCategory,\n\tCOUNT(1)\nfrom\n\tShared.Settings\ngroup by\n\t";
        let req = CompletionRequest {
            sql,
            cursor: sql.len(),
            trigger: IntelliSenseTriggerKind::ManualShortcut,
            manual: true,
            catalog: &catalog,
            procedures: &[],
            rules: &cfg,
            parameters: &[],
            model: None,
            database_type: None,
        };
        let items = svc.complete_enriched_response(&req).items;
        let first = items.first().expect("items offered");
        assert_eq!(first.label, "Category");
        assert_eq!(
            first.detail,
            sqlly_intel::intellisense::REQUIRED_GROUP_BY_DETAIL
        );
        assert_eq!(items.iter().filter(|i| i.label == "Category").count(), 1);
        assert!(
            items.iter().all(|i| i.kind != CompletionKind::Table),
            "no tables in a GROUP BY list, got {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn complete_enriched_applies_blocked_kinds_rule() {
        let svc = DefaultIntellisense;
        let cfg = parse(
            r#"{"rules":[{"name":"no-keywords","when":{"clause":["select"]},"blockedKinds":["keyword"]}]}"#,
        )
        .unwrap();
        let items = svc.complete_enriched("SELECT c", 8, &catalog(), &[], &cfg);
        assert!(
            items.iter().all(|i| i.kind != CompletionKind::Keyword),
            "keywords should be blocked by the rule"
        );
    }

    #[test]
    fn complete_enriched_filters_procedure_by_prefix() {
        let svc = DefaultIntellisense;
        let proc = ProcedureInfo::new("dbo", "GetUser");
        let cfg = parse(&default_config_text()).unwrap();
        // Prefix "ord" must not surface the GetUser procedure snippet.
        let items = svc.complete_enriched("SELECT * FROM ord", 17, &catalog(), &[proc], &cfg);
        assert!(
            !items
                .iter()
                .any(|i| i.label == "dbo.GetUser" && i.source == CompletionSource::Snippet),
            "procedure snippet should not match prefix 'ord'"
        );
    }

    /// AGENTS.md requires the intellisense hot path stay performant. The
    /// enriched path adds procedure-snippet merging + rule filtering on top of
    /// core completion; this guards against a regression where that extra work
    /// blows the editor's per-keystroke budget. Mirrors the
    /// `cached_completion_stays_under_latency_budget` test in sqlly-intellisense
    /// (1000 objects, <50ms) but stresses the *enriched* additions: a large
    /// procedure set and a non-trivial rule config.
    #[test]
    fn complete_enriched_stays_under_latency_budget() {
        let svc = DefaultIntellisense;

        // Large live catalog: 1000 tables.
        let mut catalog = catalog();
        for idx in 0..1_000 {
            catalog.objects.push(CompletionObject::table(
                "dbo",
                format!("Table{idx}"),
                vec!["Id".to_string(), "Name".to_string()],
            ));
        }

        // Large procedure set: 500 stored procedures, each with parameters, so
        // the snippet-merge + prefix-filter loop is exercised at scale.
        let mut procedures = Vec::with_capacity(500);
        for idx in 0..500 {
            let mut proc = ProcedureInfo::new("dbo", format!("Proc{idx}"));
            proc.parameters.push(ProcedureParameter::new("@id", "int"));
            proc.parameters
                .push(ProcedureParameter::new("@name", "nvarchar(50)"));
            procedures.push(proc);
        }

        // Non-trivial rule config: several rules with conditions + kind filters
        // so apply_rules has real work per item.
        let cfg = parse(
            r#"{"rules":[
                {"name":"select-no-keyword","when":{"clause":["select"]},"blockedKinds":["keyword"]},
                {"name":"from-tables","when":{"clause":["from"]},"allowedKinds":["table","view","schema"]},
                {"name":"where-cols","when":{"clause":["where"]},"allowedKinds":["column"]},
                {"name":"dot-qualified","when":{"trigger":["dot"]},"blockedKinds":["snippet"]}
            ]}"#,
        )
        .unwrap();

        // Prefix "Table1" matches many tables (allowed by the FROM rule) so the
        // result is non-empty, while the 500-procedure merge loop + rule pass
        // still run at full scale.
        let started = Instant::now();
        let mut history = AcceptHistory::default();
        for i in 0..512 {
            history.record(&format!("dbo.HistoryTable{i:04}"));
        }
        let req = CompletionRequest::auto("SELECT * FROM Table1", 20, &catalog, &procedures, &cfg);
        let response = svc.complete_enriched_response_with(
            &req,
            RankingHints {
                accept_history: Some(&history),
            },
        );
        let items = response.items;
        let elapsed = started.elapsed();
        assert!(!items.is_empty());
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "enriched completion budget exceeded: {elapsed:?}"
        );
    }

    // ---- Tier 2A: complete_enriched integration tests ------------------

    #[test]
    fn complete_enriched_fuzzy_matches_procedure_snippet() {
        // Fuzzy matching should apply to procedure snippets: `dbo.GetU`
        // should match `dbo.GetUser` via fuzzy (prefix) scoring, not just
        // starts_with/contains.
        let svc = DefaultIntellisense;
        let mut proc = ProcedureInfo::new("dbo", "GetUser");
        proc.parameters.push(ProcedureParameter::new("@id", "int"));
        let cfg = parse(&default_config_text()).unwrap();
        let items = svc.complete_enriched("EXEC dbo.GetU", 13, &catalog(), &[proc], &cfg);
        assert!(
            items.iter().any(|i| i.label == "dbo.GetUser"
                && i.kind == CompletionKind::Procedure
                && i.insert_text.contains("EXEC [dbo].[GetUser] @id = 0")),
            "expected fuzzy-matched EXEC snippet for 'dbo.GetU', got {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );
    }

    #[test]
    fn complete_enriched_snippet_outranks_object_on_dedup() {
        // When both a core object completion and a procedure snippet share
        // the same label, the snippet (higher score) must survive the
        // label-dedup so the richer EXEC body is kept.
        let svc = DefaultIntellisense;
        let mut proc = ProcedureInfo::new("dbo", "GetUser");
        proc.parameters.push(ProcedureParameter::new("@id", "int"));
        let cfg = parse(&default_config_text()).unwrap();
        let items = svc.complete_enriched("EXEC dbo.Get", 12, &catalog(), &[proc], &cfg);
        let proc_item = items.iter().find(|i| i.label == "dbo.GetUser");
        if let Some(item) = proc_item {
            assert_eq!(
                item.source,
                CompletionSource::Snippet,
                "dedup should keep the EXEC snippet, not the bare object"
            );
            assert!(
                item.insert_text.contains("EXEC"),
                "surviving item should be the EXEC declaration snippet"
            );
        }
    }

    #[test]
    fn complete_enriched_fuzzy_ranks_above_non_fuzzy() {
        // A fuzzy (camel) match on a table should rank above a mere contains
        // match on another table when both share the same base score.
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = CompletionCatalog {
            databases: vec![],
            schemas: vec![],
            objects: vec![
                CompletionObject::table("s", "SalesOrder", vec![]),
                CompletionObject::table("s", "CustomerOrder", vec![]),
            ],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        // `SaOr` camel-matches `s.SalesOrder` (CAMEL_SCORE=25) but only
        // contains-matches `s.CustomerOrder` (CONTAINS_SCORE=0).
        let items = svc.complete_enriched("SELECT * FROM SaOr", 18, &catalog, &[], &cfg);
        let sales_idx = items.iter().position(|i| i.label == "s.SalesOrder");
        let customer_idx = items.iter().position(|i| i.label == "s.CustomerOrder");
        if let (Some(si), Some(ci)) = (sales_idx, customer_idx) {
            assert!(
                si < ci,
                "SalesOrder (camel match) should rank above CustomerOrder (contains), got {:?}",
                items
                    .iter()
                    .map(|i| (&i.label, i.score))
                    .collect::<Vec<_>>()
            );
        }
    }

    /// Twin of `complete_enriched_fuzzy_ranks_above_non_fuzzy` with hump
    /// matching off: `SaOr` is neither a prefix nor a substring of either
    /// table, so neither is offered, while `SALESORD` still finds SalesOrder.
    #[test]
    fn complete_enriched_hump_matching_off_uses_plain_matching() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = CompletionCatalog {
            databases: vec![],
            schemas: vec![],
            objects: vec![
                CompletionObject::table("s", "SalesOrder", vec![]),
                CompletionObject::table("s", "CustomerOrder", vec![]),
            ],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let options = sqlly_intel::matching::MatchOptions {
            hump_matching: false,
            schema_in_search: true,
        };
        sqlly_intel::matching::with_match_options(options, || {
            let items = svc.complete_enriched("SELECT * FROM SaOr", 18, &catalog, &[], &cfg);
            assert!(!items.iter().any(|i| i.label == "s.SalesOrder"));
            assert!(!items.iter().any(|i| i.label == "s.CustomerOrder"));
            let items = svc.complete_enriched("SELECT * FROM SALESORD", 22, &catalog, &[], &cfg);
            assert!(items.iter().any(|i| i.label == "s.SalesOrder"));
            assert!(!items.iter().any(|i| i.label == "s.CustomerOrder"));
        });
    }

    #[test]
    fn complete_enriched_fk_joins_rank_above_plain_tables() {
        // Port of Swift `testForeignKeyJoinsRankAbovePlainTables`: through the
        // full enriched pipeline (policy + rules + sort), a ready-to-use FK
        // join suggestion must appear before any plain table candidate.
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = CompletionCatalog {
            databases: Vec::new(),
            schemas: vec!["Sales".to_string()],
            objects: vec![
                CompletionObject {
                    database: None,
                    schema: Some("Sales".to_string()),
                    name: "Customers".to_string(),
                    kind: ObjectKind::Table,
                    columns: vec!["CustomerId".into(), "Name".into()],
                    parameters: Vec::new(),
                    foreign_keys: Vec::new(),
                },
                CompletionObject {
                    database: None,
                    schema: Some("Sales".to_string()),
                    name: "Orders".to_string(),
                    kind: ObjectKind::Table,
                    columns: vec!["OrderId".into(), "CustomerId".into()],
                    parameters: Vec::new(),
                    foreign_keys: vec![sqlly_intel::intellisense::FkRelation {
                        column: "CustomerId".to_string(),
                        target_schema: "Sales".to_string(),
                        target_table: "Customers".to_string(),
                        target_column: "CustomerId".to_string(),
                        constraint: None,
                    }],
                },
            ],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let sql = "SELECT * FROM Sales.Orders o JOIN ";
        let req = CompletionRequest::manual(sql, sql.len(), &catalog, &[], &cfg);
        let items = svc.complete_enriched_response(&req).items;
        let join_idx = items
            .iter()
            .position(|i| i.kind == CompletionKind::Join)
            .expect("expected an FK join item in the enriched response");
        assert_eq!(
            items[join_idx].insert_text,
            "Sales.Customers c ON c.CustomerId = o.CustomerId"
        );
        if let Some(table_idx) = items.iter().position(|i| i.kind == CompletionKind::Table) {
            assert!(
                join_idx < table_idx,
                "FK join suggestions should rank above plain table candidates, got: {:?}",
                items
                    .iter()
                    .map(|i| (&i.label, i.kind, i.score))
                    .collect::<Vec<_>>()
            );
        }
    }

    // DBeaver #42381: right after `ON`, the join's columns stay on offer when
    // a later clause already follows in the same statement.
    #[test]
    fn complete_enriched_join_on_offers_columns_when_a_later_clause_follows() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let fk = |column: &str, table: &str| FkRelation {
            column: column.to_string(),
            target_schema: "public".to_string(),
            target_table: table.to_string(),
            target_column: "id".to_string(),
            constraint: None,
        };
        let table = |name: &str, columns: &[&str], fks: Vec<FkRelation>| CompletionObject {
            database: None,
            schema: Some("public".to_string()),
            name: name.to_string(),
            kind: ObjectKind::Table,
            columns: columns.iter().map(|c| c.to_string()).collect(),
            parameters: Vec::new(),
            foreign_keys: fks,
        };
        let catalog = CompletionCatalog {
            databases: Vec::new(),
            schemas: vec!["public".to_string()],
            objects: vec![
                table(
                    "author",
                    &["id", "full_name", "created_on", "current_book_id"],
                    vec![fk("current_book_id", "book")],
                ),
                table(
                    "book",
                    &["id", "title", "author_id", "published_year"],
                    vec![fk("author_id", "author")],
                ),
            ],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let head = "select *\nfrom author a\njoin book b on ";
        let labels = |sql: &str| {
            let req = CompletionRequest::manual(sql, head.len(), &catalog, &[], &cfg);
            svc.complete_enriched_response(&req)
                .items
                .into_iter()
                .map(|i| (i.label, i.kind))
                .collect::<Vec<_>>()
        };
        let alone = labels(&format!("{head}\n;"));
        let followed = labels(&format!("{head}\nwhere a.id = 5\n;"));
        let grouped = labels(&format!("{head}\ngroup by a.id\n;"));
        for label in ["a.id", "b.author_id", "a.full_name", "b.title"] {
            assert!(
                alone.iter().any(|(l, _)| l == label),
                "{label} missing: {alone:?}"
            );
        }
        assert_eq!(alone, followed);
        assert_eq!(alone, grouped);
    }

    // DBeaver #42293: a `FROM` inside EXTRACT/SUBSTRING/TRIM or
    // `IS DISTINCT FROM` is part of an expression, not the query's FROM
    // clause, so the real source's alias still resolves.
    #[test]
    fn complete_enriched_from_inside_expressions_keeps_the_alias() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = CompletionCatalog {
            databases: Vec::new(),
            schemas: vec!["public".to_string()],
            objects: vec![CompletionObject {
                database: None,
                schema: Some("public".to_string()),
                name: "calendar".to_string(),
                kind: ObjectKind::Table,
                columns: vec!["dt_calendario".into(), "valor".into()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            }],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let complete = |sql_with_cursor: &str| {
            let cursor = sql_with_cursor.find('|').unwrap();
            let sql = sql_with_cursor.replacen('|', "", 1);
            let req = CompletionRequest::manual(&sql, cursor, &catalog, &[], &cfg);
            svc.complete_enriched_response(&req)
                .items
                .into_iter()
                .map(|i| i.label)
                .collect::<Vec<_>>()
        };
        for expr in [
            "EXTRACT(YEAR FROM cal.dt_calendario)::INTEGER AS ano_numero",
            "EXTRACT(EPOCH FROM cal.dt_calendario) AS epoch",
            "SUBSTRING(cal.valor FROM 2 FOR 3) AS s",
            "TRIM(BOTH 'x' FROM cal.valor) AS t",
            "OVERLAY(cal.valor PLACING 'ZZ' FROM 3 FOR 2) AS o",
        ] {
            let items = complete(&format!(
                "SELECT {expr},\n    cal.|\nFROM public.calendar AS cal\nWHERE cal.valor IS DISTINCT FROM NULL"
            ));
            for col in ["dt_calendario", "valor"] {
                assert!(
                    items
                        .iter()
                        .any(|l| l == col || l.ends_with(&format!(".{col}"))),
                    "{expr}: {col} missing: {items:?}"
                );
            }
            let items = complete(&format!(
                "SELECT {expr}\nFROM public.calendar AS cal\nWHERE cal.|"
            ));
            assert!(
                items.iter().any(|l| l == "valor" || l.ends_with(".valor")),
                "{expr} (WHERE): {items:?}"
            );
        }
        let cte = "WITH cte_datas AS (\n    SELECT d::DATE AS dt_calendario FROM GENERATE_SERIES('2025-01-01'::DATE, '2025-01-10'::DATE, INTERVAL '1 day') AS d\n),\ncte_atributos_base AS (\n    SELECT\n        EXTRACT(YEAR FROM cal.dt_calendario)::INTEGER AS ano_numero,\n        cal.|\n    FROM cte_datas AS cal\n)\nSELECT * FROM cte_atributos_base;";
        let has = |items: &[String], col: &str| {
            items
                .iter()
                .any(|l| l == col || l.ends_with(&format!(".{col}")))
        };
        let items = complete(cte);
        assert!(has(&items, "dt_calendario"), "CTE: {items:?}");
        let items = complete("WITH c AS (SELECT 1 AS dt_calendario)\nSELECT cal.| FROM c AS cal");
        assert!(has(&items, "dt_calendario"), "simple CTE: {items:?}");
        let items = complete("WITH RECURSIVE c (n, label) AS (SELECT 1, 'a')\nSELECT x.| FROM c x");
        assert!(
            has(&items, "n") && has(&items, "label"),
            "column list: {items:?}"
        );
        let items = complete("SELECT COUNT(cal.valor) AS n, cal.| FROM public.calendar AS cal");
        assert!(has(&items, "valor"), "call before alias: {items:?}");
    }

    #[test]
    fn complete_enriched_fuzzy_procedure_does_not_match_unrelated() {
        // Fuzzy matching should not surface a procedure when the prefix
        // doesn't fuzzy-match the procedure name.
        let svc = DefaultIntellisense;
        let proc = ProcedureInfo::new("dbo", "GetUser");
        let cfg = parse(&default_config_text()).unwrap();
        let items = svc.complete_enriched("SELECT * FROM xyz", 17, &catalog(), &[proc], &cfg);
        assert!(
            !items
                .iter()
                .any(|i| i.label == "dbo.GetUser" && i.source == CompletionSource::Snippet),
            "procedure snippet should not fuzzy-match prefix 'xyz'"
        );
    }

    #[test]
    fn complete_enriched_exact_prefix_ranks_matching_table_first() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = CompletionCatalog {
            databases: vec![],
            schemas: vec!["dbo".to_string()],
            objects: vec![
                CompletionObject::table("dbo", "Orders", vec!["OrderId".into()]),
                CompletionObject::table("dbo", "OrdersArchive", vec!["OrderId".into()]),
            ],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let sql = "SELECT * FROM Orders";
        let req = CompletionRequest::auto(sql, sql.len(), &catalog, &[], &cfg);
        let items = svc.complete_enriched_response(&req).items;
        let first_table = items
            .iter()
            .find(|item| item.kind == CompletionKind::Table)
            .expect("expected a table item");
        assert_eq!(first_table.label, "dbo.Orders");
    }

    #[test]
    fn complete_enriched_accept_history_boosts_habitual_table() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = CompletionCatalog {
            databases: vec![],
            schemas: vec!["dbo".to_string()],
            objects: vec![
                CompletionObject::table("dbo", "Orders", vec!["OrderId".into()]),
                CompletionObject::table("dbo", "OrdersArchive", vec!["OrderId".into()]),
            ],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let mut history = AcceptHistory::default();
        for _ in 0..3 {
            history.record("dbo.OrdersArchive");
        }
        let sql = "SELECT * FROM ord";
        let req = CompletionRequest::auto(sql, sql.len(), &catalog, &[], &cfg);
        let items = svc
            .complete_enriched_response_with(
                &req,
                RankingHints {
                    accept_history: Some(&history),
                },
            )
            .items;
        let orders_idx = items.iter().position(|i| i.label == "dbo.Orders");
        let archive_idx = items.iter().position(|i| i.label == "dbo.OrdersArchive");
        if let (Some(orders_idx), Some(archive_idx)) = (orders_idx, archive_idx) {
            assert!(
                archive_idx < orders_idx,
                "accepted table should rank above sibling, got {:?}",
                items
                    .iter()
                    .map(|i| (&i.label, i.score))
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn complete_enriched_value_literal_templates_after_equals() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = CompletionCatalog {
            databases: vec![],
            schemas: vec!["dbo".to_string()],
            objects: vec![CompletionObject {
                database: None,
                schema: Some("dbo".to_string()),
                name: "Orders".to_string(),
                kind: ObjectKind::Table,
                columns: vec!["OrderId".into(), "Name".into()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            }],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let sql = "SELECT * FROM dbo.Orders WHERE OrderId = ";
        let req = CompletionRequest::manual(sql, sql.len(), &catalog, &[], &cfg);
        let items = svc.complete_enriched_response(&req).items;
        assert!(items.iter().any(|i| i.label == "''"));
        assert!(items.iter().any(|i| i.label == "0"));
        // No model, so the column's nullability is unknown: the NULL
        // templates need a column known to be nullable (feedback ifb_8765).
        for null_template in ["NULL", "IS NULL", "IS NOT NULL"] {
            assert!(!items.iter().any(|i| i.label == null_template));
        }
    }

    #[test]
    fn complete_enriched_like_offers_pattern_not_numeric() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = CompletionCatalog {
            databases: vec![],
            schemas: vec!["dbo".to_string()],
            objects: vec![CompletionObject {
                database: None,
                schema: Some("dbo".to_string()),
                name: "Orders".to_string(),
                kind: ObjectKind::Table,
                columns: vec!["Name".into()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            }],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let sql = "SELECT * FROM dbo.Orders WHERE Name LIKE ";
        let req = CompletionRequest::manual(sql, sql.len(), &catalog, &[], &cfg);
        let items = svc.complete_enriched_response(&req).items;
        assert!(items.iter().any(|i| i.label == "'%%'"));
        assert!(!items.iter().any(|i| i.label == "0"));
    }

    #[test]
    fn complete_enriched_flags_fk_related_table_detail() {
        let svc = DefaultIntellisense;
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = CompletionCatalog {
            databases: Vec::new(),
            schemas: vec!["dbo".to_string()],
            objects: vec![
                CompletionObject {
                    database: None,
                    schema: Some("dbo".to_string()),
                    name: "Orders".to_string(),
                    kind: ObjectKind::Table,
                    columns: vec!["OrderId".into(), "CustomerId".into()],
                    parameters: Vec::new(),
                    foreign_keys: vec![FkRelation {
                        column: "CustomerId".to_string(),
                        target_schema: "dbo".to_string(),
                        target_table: "Customers".to_string(),
                        target_column: "Id".to_string(),
                        constraint: None,
                    }],
                },
                CompletionObject::table("dbo", "Customers", vec!["Id".into()]),
            ],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let sql = "SELECT * FROM dbo.Orders o JOIN Cus";
        let req = CompletionRequest::manual(sql, sql.len(), &catalog, &[], &cfg);
        let items = svc.complete_enriched_response(&req).items;
        let customers = items
            .iter()
            .find(|i| i.kind == CompletionKind::Table && i.label == "dbo.Customers")
            .expect("expected dbo.Customers table item");
        assert_eq!(customers.detail, "related via FK");
    }

    // ---- Identifier quoting --------------------------------------------

    fn quoting_catalog(schema: &str) -> CompletionCatalog {
        let table = |name: &str, columns: &[&str]| CompletionObject {
            database: Some("appdb".to_string()),
            schema: Some(schema.to_string()),
            name: name.to_string(),
            kind: ObjectKind::Table,
            columns: columns.iter().map(|c| c.to_string()).collect(),
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        };
        let mut details = table("Order Details", &["Order ID", "Unit Price", "Quantity"]);
        details.foreign_keys.push(FkRelation {
            column: "Order ID".to_string(),
            target_schema: schema.to_string(),
            target_table: "Orders".to_string(),
            target_column: "Id".to_string(),
            constraint: None,
        });
        CompletionCatalog {
            databases: vec!["appdb".to_string()],
            schemas: vec![schema.to_string()],
            objects: vec![
                details,
                table("Orders", &["Id", "OrderDate"]),
                table("order", &["key"]),
            ],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        }
    }

    fn quoting_response(
        sql: &str,
        cursor: usize,
        catalog: &CompletionCatalog,
        db: Option<sqlly_core::DatabaseType>,
    ) -> CompletionResponse {
        let cfg = parse(&default_config_text()).unwrap();
        let req = CompletionRequest {
            sql,
            cursor,
            trigger: IntelliSenseTriggerKind::ManualShortcut,
            manual: true,
            catalog,
            procedures: &[],
            rules: &cfg,
            parameters: &[],
            model: None,
            database_type: db,
        };
        DefaultIntellisense.complete_enriched_response(&req)
    }

    fn insert_for<'a>(items: &'a [CompletionItem], label: &str) -> &'a str {
        items
            .iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| {
                panic!(
                    "no item {label:?} in {:?}",
                    items.iter().map(|i| &i.label).collect::<Vec<_>>()
                )
            })
            .insert_text
            .as_str()
    }

    #[test]
    fn table_inserts_are_quoted_per_engine_only_when_needed() {
        use sqlly_core::DatabaseType::*;
        let sql = "SELECT * FROM ord";
        let cases = [
            (
                SqlServer,
                "dbo",
                "dbo.[Order Details]",
                "dbo.Orders",
                "dbo.[order]",
            ),
            (
                MySql,
                "shop",
                "shop.`Order Details`",
                "shop.Orders",
                "shop.`order`",
            ),
            (
                MariaDb,
                "shop",
                "shop.`Order Details`",
                "shop.Orders",
                "shop.`order`",
            ),
            (
                ClickHouse,
                "db",
                "db.`Order Details`",
                "db.Orders",
                "db.`order`",
            ),
            (
                Postgres,
                "public",
                "public.\"Order Details\"",
                "public.\"Orders\"",
                "public.\"order\"",
            ),
            (
                Oracle,
                "HR",
                "HR.\"Order Details\"",
                "HR.\"Orders\"",
                "HR.\"order\"",
            ),
            (
                Sqlite,
                "main",
                "main.\"Order Details\"",
                "main.Orders",
                "main.\"order\"",
            ),
            (
                LibSql,
                "main",
                "main.\"Order Details\"",
                "main.Orders",
                "main.\"order\"",
            ),
            (
                DuckDb,
                "main",
                "main.\"Order Details\"",
                "main.Orders",
                "main.\"order\"",
            ),
        ];
        for (db, schema, details, orders, order) in cases {
            let catalog = quoting_catalog(schema);
            let items = quoting_response(sql, sql.len(), &catalog, Some(db)).items;
            assert_eq!(
                insert_for(&items, &format!("{schema}.Order Details")),
                details,
                "{db:?}"
            );
            assert_eq!(
                insert_for(&items, &format!("{schema}.Orders")),
                orders,
                "{db:?}"
            );
            assert_eq!(
                insert_for(&items, &format!("{schema}.order")),
                order,
                "{db:?}"
            );
        }
    }

    #[test]
    fn unknown_engine_inserts_names_verbatim() {
        let sql = "SELECT * FROM ord";
        let catalog = quoting_catalog("dbo");
        let items = quoting_response(sql, sql.len(), &catalog, None).items;
        assert_eq!(insert_for(&items, "dbo.Order Details"), "dbo.Order Details");
    }

    #[test]
    fn dotted_and_alias_qualified_columns_are_quoted() {
        use sqlly_core::DatabaseType::SqlServer;
        let catalog = quoting_catalog("dbo");
        // Schema-qualified table: only the table part is inserted after `dbo.`.
        let sql = "SELECT * FROM dbo.Ord";
        let items = quoting_response(sql, sql.len(), &catalog, Some(SqlServer)).items;
        assert_eq!(insert_for(&items, "dbo.Order Details"), "[Order Details]");
        // Alias member access inserts the bare column after `od.`.
        let sql = "SELECT od.Un FROM dbo.[Order Details] od";
        let cursor = "SELECT od.Un".len();
        let items = quoting_response(sql, cursor, &catalog, Some(SqlServer)).items;
        assert_eq!(insert_for(&items, "od.Unit Price"), "[Unit Price]");
        // In-scope columns of an aliased source carry the alias.
        let sql = "SELECT * FROM dbo.[Order Details] od WHERE Un";
        let items = quoting_response(sql, sql.len(), &catalog, Some(SqlServer)).items;
        assert_eq!(insert_for(&items, "od.Unit Price"), "od.[Unit Price]");
    }

    #[test]
    fn fk_join_suggestions_quote_tables_and_columns() {
        use sqlly_core::DatabaseType::SqlServer;
        let catalog = quoting_catalog("dbo");
        let sql = "SELECT * FROM dbo.Orders o JOIN ";
        let items = quoting_response(sql, sql.len(), &catalog, Some(SqlServer)).items;
        let join = items
            .iter()
            .find(|i| i.kind == CompletionKind::Join)
            .expect("a FK join suggestion");
        assert_eq!(
            join.insert_text,
            "dbo.[Order Details] o2 ON o2.[Order ID] = o.Id"
        );
        assert!(join.label.contains("dbo.Order Details"));
    }

    #[test]
    fn opening_bracket_completes_inside_the_users_quote() {
        use sqlly_core::DatabaseType::SqlServer;
        let catalog = quoting_catalog("dbo");
        let sql = "SELECT * FROM [Order D";
        let response = quoting_response(sql, sql.len(), &catalog, Some(SqlServer));
        assert_eq!(response.replace_start, "SELECT * FROM ".len());
        let item = response
            .items
            .iter()
            .find(|i| i.label == "dbo.Order Details")
            .expect("the typed name still matches");
        assert_eq!(item.insert_text, "[dbo].[Order Details]");
        assert_eq!(item.replace_span, Some(("SELECT * FROM ".len(), sql.len())));
        assert!(
            response
                .items
                .iter()
                .all(|i| i.kind != CompletionKind::Keyword),
            "no keywords inside a quoted name"
        );

        // A quoted qualifier plus an auto-paired closer after the caret: the
        // leaf is completed inside its bracket and the closer is replaced.
        let sql = "SELECT * FROM [dbo].[Ord]";
        let cursor = sql.len() - 1;
        let response = quoting_response(sql, cursor, &catalog, Some(SqlServer));
        let leaf_start = "SELECT * FROM [dbo].".len();
        assert_eq!(response.replace_start, leaf_start);
        assert_eq!(insert_for(&response.items, "dbo.Orders"), "[Orders]");
        let details = response
            .items
            .iter()
            .find(|i| i.label == "dbo.Order Details")
            .unwrap();
        assert_eq!(details.insert_text, "[Order Details]");
        assert_eq!(details.replace_span, Some((leaf_start, sql.len())));
    }

    // Sequel Ace #2245: completing while editing inside an already-closed
    // quoted name replaces the whole name through its closing quote, so the
    // old name's tail and closer are never left behind.
    #[test]
    fn completing_inside_a_closed_quoted_name_replaces_the_whole_name() {
        use sqlly_core::DatabaseType::{MySql, SqlServer};
        let catalog = CompletionCatalog {
            databases: vec!["netru_database".into(), "staging_netru_database".into()],
            schemas: vec!["netru_database".into(), "staging_netru_database".into()],
            objects: Vec::new(),
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let apply = |sql: &str, cursor: usize, db, label: &str| {
            let r = quoting_response(sql, cursor, &catalog, Some(db));
            let item = r
                .items
                .iter()
                .find(|i| i.label == label)
                .unwrap_or_else(|| panic!("no {label}: {:?}", r.items));
            let (a, b) = item.replace_span.unwrap_or((r.replace_start, cursor));
            format!("{}{}{}", &sql[..a], item.insert_text, &sql[b..])
        };
        let tail =
            ".`campaign_rsids` ADD INDEX `sid2_20` (`sid2`(20)), ALGORITHM=INPLACE, LOCK=NONE;";
        let sql = format!("ALTER TABLE `stagingnetru_database`{tail}");
        let cursor = "ALTER TABLE `staging".len();
        assert_eq!(
            apply(&sql, cursor, MySql, "staging_netru_database"),
            format!("ALTER TABLE `staging_netru_database`{tail}")
        );
        let sql = "SELECT * FROM [stagnetru_database].dbo.t";
        let cursor = "SELECT * FROM [stag".len();
        assert_eq!(
            apply(sql, cursor, SqlServer, "staging_netru_database"),
            "SELECT * FROM [staging_netru_database].dbo.t"
        );
        // Text that is not part of one name (a space before the next quote)
        // is never swallowed.
        let sql = "SELECT * FROM `stag WHERE x = `y`";
        let cursor = "SELECT * FROM `stag".len();
        assert_eq!(
            apply(sql, cursor, MySql, "staging_netru_database"),
            "SELECT * FROM `staging_netru_database` WHERE x = `y`"
        );
    }

    // DbGate #1223: a DECLAREd variable is offered later in the batch — not
    // the half-typed word itself — and a statement can start with BEGIN
    // (or another statement head) without a procedure taking its place.
    #[test]
    fn declared_variables_and_statement_heads_complete() {
        use sqlly_core::DatabaseType::SqlServer;
        let mut catalog = catalog();
        catalog.objects.push(CompletionObject {
            database: Some("appdb".to_string()),
            schema: Some("dbo".to_string()),
            name: "BeginShift".to_string(),
            kind: ObjectKind::StoredProcedure,
            columns: Vec::new(),
            parameters: vec!["@id".to_string()],
            foreign_keys: Vec::new(),
        });
        let labels = |sql: &str| {
            quoting_response(sql, sql.len(), &catalog, Some(SqlServer))
                .items
                .into_iter()
                .map(|i| i.label)
                .collect::<Vec<_>>()
        };
        let declare = "declare @user_name  nvarchar(20) = ''\n";
        let items = labels(&format!(
            "{declare}\nselect * from dbo.Orders where OrderId = @us"
        ));
        assert_eq!(
            items.first().map(String::as_str),
            Some("@user_name"),
            "{items:?}"
        );
        assert!(!items.iter().any(|l| l == "@us"), "{items:?}");

        for sql in [
            "beg".to_string(),
            format!("{declare}beg"),
            "select 1;\nBEG".to_string(),
        ] {
            let items = labels(&sql);
            assert_eq!(
                items.first().map(String::as_str),
                Some("BEGIN"),
                "{sql:?}: {items:?}"
            );
        }
        for (typed, head) in [
            ("dec", "DECLARE"),
            ("wit", "WITH"),
            ("mer", "MERGE"),
            ("alt", "ALTER"),
            ("dro", "DROP"),
            ("tru", "TRUNCATE TABLE"),
            ("com", "COMMIT"),
            ("rol", "ROLLBACK"),
        ] {
            let items = labels(typed);
            assert!(items.iter().any(|l| l == head), "{typed}: {items:?}");
        }
        // Statement heads stay out of clauses.
        let items = labels("select * from dbo.Orders where OrderId = 1 and d");
        assert!(
            !items.iter().any(|l| l == "DECLARE" || l == "DROP"),
            "{items:?}"
        );
    }

    // HeidiSQL #2439: a single-table query offers its columns bare, with no
    // alias, whether the caret sits before or after the FROM clause.
    #[test]
    fn single_table_query_offers_bare_columns_without_an_alias() {
        let cfg = parse(&default_config_text()).unwrap();
        let catalog = CompletionCatalog {
            databases: Vec::new(),
            schemas: vec!["dbo".to_string()],
            objects: vec![CompletionObject {
                database: None,
                schema: Some("dbo".to_string()),
                name: "Customers".to_string(),
                kind: ObjectKind::Table,
                columns: vec!["CustomerId".into(), "FullName".into(), "City".into()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            }],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        for sql in [
            "SELECT Fu| FROM Customers",
            "SELECT CustomerId, Fu| FROM dbo.Customers WHERE City = 'Oslo'",
            "SELECT * FROM Customers WHERE Fu|",
            "SELECT * FROM Customers ORDER BY Fu|",
        ] {
            let cursor = sql.find('|').unwrap();
            let text = sql.replacen('|', "", 1);
            let req = CompletionRequest::manual(&text, cursor, &catalog, &[], &cfg);
            let items = DefaultIntellisense.complete_enriched_response(&req).items;
            assert_eq!(
                items
                    .first()
                    .map(|i| (i.label.as_str(), i.insert_text.as_str())),
                Some(("FullName", "FullName")),
                "{sql}: {:?}",
                items.iter().map(|i| &i.label).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn quoted_qualifier_resolves_columns_of_a_name_with_spaces() {
        use sqlly_core::DatabaseType::*;
        let catalog = quoting_catalog("public");
        let sql = "SELECT \"Order Details\".";
        let response = quoting_response(sql, sql.len(), &catalog, Some(Postgres));
        assert_eq!(response.replace_start, sql.len());
        assert_eq!(
            insert_for(&response.items, "Order Details.Unit Price"),
            "\"Unit Price\""
        );
        assert_eq!(
            insert_for(&response.items, "Order Details.Quantity"),
            "\"Quantity\""
        );

        let catalog = quoting_catalog("shop");
        let sql = "SELECT * FROM `shop`.`ord";
        let response = quoting_response(sql, sql.len(), &catalog, Some(MySql));
        assert_eq!(
            insert_for(&response.items, "shop.Order Details"),
            "`Order Details`"
        );
        assert_eq!(insert_for(&response.items, "shop.order"), "`order`");
    }
}
