//! Policy decisions for IntelliSense completion. Ports Swift's
//! `EditorIntelliSensePolicy.swift` — determines which completion kinds are
//! allowed in a given context, applies user rules, and decides whether to
//! render the popup at all.
//!
//! The policy is pure: it takes a [`CompletionContext`] and an optional
//! [`IntelliSenseRuleConfig`], and returns a [`PolicyDecision`]. Both the
//! GPUI editor and the CLI endpoint call the same function so behavior is
//! identical across frontends.

use crate::intellisense::context::CompletionContext;
use crate::intellisense::rules::{IntelliSenseIntent, IntelliSenseRuleConfig};
use sqlly_intel::intellisense::CompletionKind;
use std::collections::HashSet;

/// The policy decision: whether to render, which kinds to allow, and metadata
/// for debug panels.
#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub should_render: bool,
    pub allowed_kinds: HashSet<CompletionKind>,
    pub reason: String,
    pub debug_facts: Vec<String>,
    pub max_items: usize,
    pub matched_rules: Vec<String>,
}

impl PolicyDecision {
    /// A permissive decision that allows everything.
    pub fn allow_all() -> Self {
        Self {
            should_render: true,
            allowed_kinds: all_kinds(),
            reason: "default".to_string(),
            debug_facts: Vec::new(),
            max_items: 40,
            matched_rules: Vec::new(),
        }
    }

    /// A closed decision that suppresses the popup.
    pub fn closed(reason: impl Into<String>) -> Self {
        Self {
            should_render: false,
            allowed_kinds: all_kinds(),
            reason: reason.into(),
            debug_facts: Vec::new(),
            max_items: 0,
            matched_rules: Vec::new(),
        }
    }

    /// Whether a given completion kind passes the policy filter.
    pub fn allows_kind(&self, kind: CompletionKind) -> bool {
        self.allowed_kinds.contains(&kind)
    }
}

fn all_kinds() -> HashSet<CompletionKind> {
    [
        CompletionKind::Keyword,
        CompletionKind::Database,
        CompletionKind::Schema,
        CompletionKind::Table,
        CompletionKind::Column,
        CompletionKind::Function,
        CompletionKind::Procedure,
        CompletionKind::Type,
        CompletionKind::Parameter,
        CompletionKind::ResultSetColumn,
        CompletionKind::Snippet,
        CompletionKind::Value,
        CompletionKind::Join,
    ]
    .into_iter()
    .collect()
}

/// Evaluate the policy for a given context, optionally merging user rules.
///
/// This is the shared entry point both GPUI and Swift (via CLI) call.
/// It computes the default decision for the intent, then applies user rules
/// from the config (if any match the context).
pub fn evaluate_policy(ctx: &CompletionContext, rules: &IntelliSenseRuleConfig) -> PolicyDecision {
    let mut decision = default_decision(ctx);

    for rule in &rules.rules {
        if !rule_matches_context(rule, ctx) {
            continue;
        }
        decision.matched_rules.push(rule.name.clone());

        if let Some(render) = rule.render {
            decision.should_render = render;
        }

        if let Some(allowed) = &rule.allowed_kinds {
            let allowed_kinds: HashSet<CompletionKind> = allowed
                .iter()
                .filter_map(|name| kind_from_name(name))
                .collect();
            decision.allowed_kinds = decision
                .allowed_kinds
                .intersection(&allowed_kinds)
                .copied()
                .collect();
        }

        if !rule.blocked_kinds.is_empty() {
            let blocked: HashSet<CompletionKind> = rule
                .blocked_kinds
                .iter()
                .filter_map(|name| kind_from_name(name))
                .collect();
            decision.allowed_kinds = decision
                .allowed_kinds
                .difference(&blocked)
                .copied()
                .collect();
        }

        if let Some(max) = rule.max_items {
            decision.max_items = decision.max_items.min(max);
        }

        if let Some(reason) = &rule.reason
            && !reason.is_empty()
        {
            decision.reason = reason.clone();
        }

        if let Some(label) = &rule.debug_label {
            decision.debug_facts.push(label.clone());
        }
    }

    if !decision.should_render && decision.reason == "default" {
        decision.reason = "Suppressed by policy".to_string();
    }

    decision
}

/// Compute the default policy decision based on intent alone.
fn default_decision(ctx: &CompletionContext) -> PolicyDecision {
    let allowed = allowed_kinds_for_intent(ctx.intent);

    PolicyDecision {
        should_render: true,
        allowed_kinds: allowed,
        reason: "default".to_string(),
        debug_facts: vec![
            format!("intent={:?}", ctx.intent),
            format!("clause={:?}", ctx.clause),
        ],
        max_items: 40,
        matched_rules: Vec::new(),
    }
}

/// The default allowed completion kinds for a given intent.
/// Ports Swift's `defaultDecision(for:)`.
fn allowed_kinds_for_intent(intent: IntelliSenseIntent) -> HashSet<CompletionKind> {
    use IntelliSenseIntent::*;

    match intent {
        FromSource | JoinSource => [
            CompletionKind::Schema,
            CompletionKind::Table,
            CompletionKind::Function,
            CompletionKind::Keyword,
            CompletionKind::Join,
        ]
        .into_iter()
        .collect(),

        // A predicate or an ORDER BY / GROUP BY item list cannot name a table
        // or routine: only the in-scope columns and expressions over them
        // (feedback ifb_56ee, ifb_2592). A dotted `Schema.Table.` prefix is a
        // QualifiedMember intent and still resolves.
        WherePredicate | GroupBy | OrderBy | Having => [
            CompletionKind::Column,
            CompletionKind::Function,
            CompletionKind::Parameter,
            CompletionKind::Keyword,
            CompletionKind::ResultSetColumn,
        ]
        .into_iter()
        .collect(),

        // A join condition: the foreign-key pairs first, then the in-scope
        // columns for a hand-written one — never another table (feedback
        // ifb_13aa).
        JoinCondition => [
            CompletionKind::Join,
            CompletionKind::Column,
            CompletionKind::Function,
            CompletionKind::Parameter,
            CompletionKind::Keyword,
        ]
        .into_iter()
        .collect(),

        SelectList => [
            CompletionKind::Column,
            CompletionKind::Table,
            CompletionKind::Function,
            CompletionKind::Parameter,
            CompletionKind::Keyword,
            CompletionKind::ResultSetColumn,
        ]
        .into_iter()
        .collect(),

        ValueLiteral => [
            CompletionKind::Value,
            CompletionKind::Column,
            CompletionKind::Function,
        ]
        .into_iter()
        .collect(),

        Parameter => [CompletionKind::Parameter].into_iter().collect(),

        ExecProcedure => [CompletionKind::Schema, CompletionKind::Procedure]
            .into_iter()
            .collect(),

        QualifiedMember => [
            CompletionKind::Column,
            CompletionKind::Table,
            CompletionKind::Function,
        ]
        .into_iter()
        .collect(),

        SetAssignment => [
            CompletionKind::Column,
            CompletionKind::Parameter,
            CompletionKind::Function,
            CompletionKind::Keyword,
        ]
        .into_iter()
        .collect(),

        General | Unknown => {
            // When we have no idea what clause we're in (general intent +
            // unknown clause), surface only keywords to avoid flooding.
            if intent == General {
                [CompletionKind::Keyword].into_iter().collect()
            } else {
                all_kinds()
            }
        }

        // Object-type intents from the original enum — keep permissive.
        Table | View | Function | Procedure | Schema | Column | Keyword | Snippet | Alias => {
            all_kinds()
        }
    }
}

/// Check if a user rule matches the given context.
/// Reuses the rule matching logic from rules.rs but with the richer context.
fn rule_matches_context(
    rule: &crate::intellisense::rules::IntelliSenseRule,
    ctx: &CompletionContext,
) -> bool {
    let c = &rule.condition;

    if !c.clauses.is_empty() && !c.clauses.contains(&ctx.clause) {
        return false;
    }
    if !c.triggers.is_empty() && !c.triggers.contains(&ctx.trigger) {
        return false;
    }
    if let Some(pe) = &c.prefix_equals
        && ctx.prefix != pe.to_ascii_lowercase()
    {
        return false;
    }
    if let Some(pm) = &c.prefix_matches
        && !ctx.prefix.contains(&pm.to_ascii_lowercase())
    {
        return false;
    }
    if let Some(hq) = c.has_qualifier
        && ctx.is_qualified_access != hq
    {
        return false;
    }
    if let Some(ha) = c.has_aliases
        && ctx.has_aliases != ha
    {
        return false;
    }
    if let Some(needle) = &c.statement_contains
        && !ctx.statement_lower.contains(&needle.to_ascii_lowercase())
    {
        return false;
    }
    if let Some(tc) = &c.table_count
        && !tc.matches(ctx.table_count)
    {
        return false;
    }
    if let Some(ac) = &c.alias_count
        && !ac.matches(ctx.alias_count)
    {
        return false;
    }
    if let Some(manual) = c.manual
        && ctx.manual != manual
    {
        return false;
    }

    // Intent matching: map rule intents to kind names and check.
    if !c.intents.is_empty() {
        let context_intent_matches = c.intents.contains(&ctx.intent);
        if !context_intent_matches {
            return false;
        }
    }

    true
}

/// Map a kind name string to a `CompletionKind`.
fn kind_from_name(name: &str) -> Option<CompletionKind> {
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        "keyword" => Some(CompletionKind::Keyword),
        "database" => Some(CompletionKind::Database),
        "schema" => Some(CompletionKind::Schema),
        "table" => Some(CompletionKind::Table),
        "column" => Some(CompletionKind::Column),
        "function" => Some(CompletionKind::Function),
        "procedure" => Some(CompletionKind::Procedure),
        "type" => Some(CompletionKind::Type),
        "parameter" => Some(CompletionKind::Parameter),
        "resultsetcolumn" => Some(CompletionKind::ResultSetColumn),
        "snippet" => Some(CompletionKind::Snippet),
        "value" => Some(CompletionKind::Value),
        "join" => Some(CompletionKind::Join),
        _ => None,
    }
}

/// Filter completion items by the policy decision. Returns a filtered vec.
pub fn filter_by_policy(
    mut items: Vec<sqlly_intel::intellisense::CompletionItem>,
    decision: &PolicyDecision,
) -> Vec<sqlly_intel::intellisense::CompletionItem> {
    if !decision.should_render {
        return Vec::new();
    }
    items.retain(|item| decision.allowed_kinds.contains(&item.kind));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intellisense::context::CompletionContext;
    use crate::intellisense::rules::parse;

    // ---- Default decisions by intent ---------------------------------------

    #[test]
    fn policy_from_source_allows_tables_schemas() {
        let ctx = CompletionContext::auto("SELECT * FROM ord", 17);
        let decision = evaluate_policy(&ctx, &IntelliSenseRuleConfig::default());
        assert!(decision.should_render);
        assert!(decision.allows_kind(CompletionKind::Table));
        assert!(decision.allows_kind(CompletionKind::Schema));
        assert!(!decision.allows_kind(CompletionKind::Column));
    }

    #[test]
    fn policy_select_list_allows_columns() {
        let ctx = CompletionContext::auto("SELECT c", 8);
        let decision = evaluate_policy(&ctx, &IntelliSenseRuleConfig::default());
        assert!(decision.should_render);
        assert!(decision.allows_kind(CompletionKind::Column));
        assert!(decision.allows_kind(CompletionKind::Function));
    }

    #[test]
    fn policy_where_predicate_allows_columns() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c", 22);
        let decision = evaluate_policy(&ctx, &IntelliSenseRuleConfig::default());
        assert!(decision.allows_kind(CompletionKind::Column));
        assert!(decision.allows_kind(CompletionKind::Parameter));
    }

    #[test]
    fn policy_value_literal_allows_values() {
        let ctx = CompletionContext::auto("SELECT * FROM t WHERE c = ", 26);
        let decision = evaluate_policy(&ctx, &IntelliSenseRuleConfig::default());
        assert!(decision.allows_kind(CompletionKind::Value));
        assert!(decision.allows_kind(CompletionKind::Column));
        assert!(!decision.allows_kind(CompletionKind::Table));
    }

    #[test]
    fn policy_parameter_only_parameters() {
        let ctx = CompletionContext::auto("SELECT @p", 9);
        let decision = evaluate_policy(&ctx, &IntelliSenseRuleConfig::default());
        assert!(decision.allows_kind(CompletionKind::Parameter));
        assert!(!decision.allows_kind(CompletionKind::Table));
    }

    #[test]
    fn policy_exec_procedure_allows_procedures() {
        let ctx = CompletionContext::auto("EXEC dbo.Get", 12);
        let decision = evaluate_policy(&ctx, &IntelliSenseRuleConfig::default());
        assert!(decision.allows_kind(CompletionKind::Procedure));
        assert!(decision.allows_kind(CompletionKind::Schema));
    }

    #[test]
    fn policy_qualified_member_allows_columns() {
        let ctx = CompletionContext::auto("SELECT t.c FROM t", 14);
        let decision = evaluate_policy(&ctx, &IntelliSenseRuleConfig::default());
        assert!(decision.allows_kind(CompletionKind::Column));
        assert!(!decision.allows_kind(CompletionKind::Schema));
    }

    #[test]
    fn policy_general_unknown_keywords_only() {
        let ctx = CompletionContext::auto("", 0);
        let decision = evaluate_policy(&ctx, &IntelliSenseRuleConfig::default());
        assert!(decision.allows_kind(CompletionKind::Keyword));
        assert!(!decision.allows_kind(CompletionKind::Table));
    }

    // ---- User rule merging -------------------------------------------------

    #[test]
    fn policy_user_rule_blocks_kind() {
        let cfg = parse(
            r#"{"rules":[{"name":"no-keywords","when":{"clause":["select"]},"blockedKinds":["keyword"]}]}"#,
        )
        .unwrap();
        let ctx = CompletionContext::auto("SELECT c", 8);
        let decision = evaluate_policy(&ctx, &cfg);
        assert!(!decision.allows_kind(CompletionKind::Keyword));
        assert!(decision.matched_rules.contains(&"no-keywords".to_string()));
    }

    #[test]
    fn policy_user_rule_allows_only_tables() {
        let cfg = parse(
            r#"{"rules":[{"name":"only-tables","when":{"clause":["from"]},"allowedKinds":["table"]}]}"#,
        )
        .unwrap();
        let ctx = CompletionContext::auto("SELECT * FROM ord", 17);
        let decision = evaluate_policy(&ctx, &cfg);
        assert!(decision.allows_kind(CompletionKind::Table));
        assert!(!decision.allows_kind(CompletionKind::Schema));
    }

    #[test]
    fn policy_user_rule_suppresses_render() {
        let cfg =
            parse(r#"{"rules":[{"name":"hide","when":{"clause":["update"]},"render":false}]}"#)
                .unwrap();
        let ctx = CompletionContext::auto("UPDATE t ", 8);
        let decision = evaluate_policy(&ctx, &cfg);
        assert!(!decision.should_render);
    }

    #[test]
    fn policy_user_rule_max_items() {
        let cfg = parse(r#"{"rules":[{"name":"cap","when":{},"maxItems":5}]}"#).unwrap();
        let ctx = CompletionContext::auto("SELECT * FROM ord", 17);
        let decision = evaluate_policy(&ctx, &cfg);
        assert_eq!(decision.max_items, 5);
    }

    #[test]
    fn policy_user_rule_does_not_match_different_clause() {
        let cfg = parse(
            r#"{"rules":[{"name":"from-only","when":{"clause":["from"]},"blockedKinds":["keyword"]}]}"#,
        )
        .unwrap();
        let ctx = CompletionContext::auto("SELECT c", 8);
        let decision = evaluate_policy(&ctx, &cfg);
        // Rule wants clause=from, but we're in select → rule inactive.
        assert!(decision.allows_kind(CompletionKind::Keyword));
    }

    // ---- filter_by_policy --------------------------------------------------

    #[test]
    fn filter_by_policy_removes_disallowed() {
        let decision = PolicyDecision {
            should_render: true,
            allowed_kinds: [CompletionKind::Table].into_iter().collect(),
            reason: "test".to_string(),
            debug_facts: vec![],
            max_items: 40,
            matched_rules: vec![],
        };
        let items = vec![
            sqlly_intel::intellisense::CompletionItem {
                label: "Orders".to_string(),
                insert_text: "Orders".to_string(),
                kind: CompletionKind::Table,
                source: sqlly_intel::intellisense::CompletionSource::LiveMetadata,
                score: 50,
                detail: String::new(),
                replace_span: None,
            },
            sqlly_intel::intellisense::CompletionItem {
                label: "SELECT".to_string(),
                insert_text: "SELECT".to_string(),
                kind: CompletionKind::Keyword,
                source: sqlly_intel::intellisense::CompletionSource::Keyword,
                score: 50,
                detail: String::new(),
                replace_span: None,
            },
        ];
        let out = filter_by_policy(items, &decision);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "Orders");
    }

    #[test]
    fn filter_by_policy_closed_returns_empty() {
        let decision = PolicyDecision::closed("test");
        let items = vec![sqlly_intel::intellisense::CompletionItem {
            label: "x".to_string(),
            insert_text: "x".to_string(),
            kind: CompletionKind::Table,
            source: sqlly_intel::intellisense::CompletionSource::LiveMetadata,
            score: 50,
            detail: String::new(),
            replace_span: None,
        }];
        let out = filter_by_policy(items, &decision);
        assert!(out.is_empty());
    }
}
