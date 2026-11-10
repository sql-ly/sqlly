//! User-configurable IntelliSense completion rules + JSONC parser. Port of
//! `IntelliSenseRuleStore.swift`. The pure parser (`parse`, `strip_jsonc`) is
//! fully unit-tested; the on-disk store is a thin wrapper.
//!
//! This module also provides [`apply_rules`], the filter step that was missing
//! in the gpui port: it prunes a completion list according to a parsed
//! [`IntelliSenseRuleConfig`] and the current [`RuleContext`]. Both the gpui
//! editor and the LSP server call `apply_rules` so rule behavior is identical
//! across frontends.

use serde::{Deserialize, Serialize};
use sqlly_intel::intellisense::{CompletionItem, CompletionKind};
use std::collections::HashSet;

/// A completion intent the rule engine can match on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IntelliSenseIntent {
    Table,
    View,
    Function,
    Procedure,
    Schema,
    Column,
    Keyword,
    Snippet,
    Alias,
    SelectList,
    FromSource,
    WherePredicate,
    JoinSource,
    JoinCondition,
    GroupBy,
    OrderBy,
    Having,
    SetAssignment,
    ValueLiteral,
    Parameter,
    ExecProcedure,
    QualifiedMember,
    General,
    Unknown,
}

/// A SQL clause context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IntelliSenseClause {
    Select,
    From,
    Where,
    Join,
    Insert,
    Update,
    Delete,
    Create,
    Alter,
    Drop,
    On,
    GroupBy,
    OrderBy,
    Having,
    Set,
    Values,
    Exec,
    Unknown,
}

/// What triggered the completion request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IntelliSenseTriggerKind {
    Dot,
    Space,
    Identifier,
    Explicit,
    AutomaticTyping,
    ManualShortcut,
    AtSign,
    Quote,
    Whitespace,
    ManualValueLookup,
}

/// Lower/upper bound matcher for count-based rule conditions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CountMatcher {
    pub min: Option<i32>,
    pub max: Option<i32>,
}

impl CountMatcher {
    pub fn matches(&self, count: i32) -> bool {
        if let Some(lo) = self.min
            && count < lo
        {
            return false;
        }
        if let Some(hi) = self.max
            && count > hi
        {
            return false;
        }
        true
    }
}

/// The match condition for a rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct IntelliSenseRuleCondition {
    #[serde(default, rename = "intent")]
    pub intents: HashSet<IntelliSenseIntent>,
    #[serde(default, rename = "clause")]
    pub clauses: HashSet<IntelliSenseClause>,
    #[serde(default, rename = "trigger")]
    pub triggers: HashSet<IntelliSenseTriggerKind>,
    pub prefix_equals: Option<String>,
    pub prefix_matches: Option<String>,
    pub has_qualifier: Option<bool>,
    pub qualifier_kind: Option<String>,
    pub has_aliases: Option<bool>,
    pub has_value_context: Option<bool>,
    pub statement_contains: Option<String>,
    pub table_count: Option<CountMatcher>,
    pub alias_count: Option<CountMatcher>,
    pub manual: Option<bool>,
}

/// A single user-configurable completion rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntelliSenseRule {
    pub name: String,
    #[serde(default, rename = "when")]
    pub condition: IntelliSenseRuleCondition,
    pub render: Option<bool>,
    pub allowed_kinds: Option<HashSet<String>>,
    #[serde(default)]
    pub blocked_kinds: HashSet<String>,
    pub keyboard: Option<IntelliSenseRuleKeyboard>,
    pub reason: Option<String>,
    pub debug_label: Option<String>,
    pub max_items: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntelliSenseRuleKeyboard {
    pub key: Option<String>,
    pub modifiers: Option<HashSet<String>>,
}

/// The parsed root document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntelliSenseRuleConfig {
    #[serde(default)]
    pub version: Option<i64>,
    #[serde(default)]
    pub rules: Vec<IntelliSenseRule>,
}

/// The editor/LSP context a completion request fires in. Built from the SQL
/// text + cursor; `apply_rules` consults it to decide which rules are active.
#[derive(Debug, Clone)]
pub struct RuleContext<'a> {
    pub prefix: String,
    pub clause: IntelliSenseClause,
    pub trigger: IntelliSenseTriggerKind,
    pub has_qualifier: bool,
    pub has_aliases: bool,
    pub statement_lower: String,
    pub sql: &'a str,
    pub table_count: i32,
    pub alias_count: i32,
    pub manual: bool,
}

impl<'a> RuleContext<'a> {
    /// Build the context from the SQL text + cursor. The prefix is derived with
    /// `sqlly_intel::intellisense::current_prefix` and lowercased to match the
    /// completion engine's case-insensitive prefix comparison.
    pub fn from(sql: &'a str, cursor: usize) -> Self {
        let prefix = sqlly_intel::intellisense::current_prefix(sql, cursor).to_ascii_lowercase();
        let has_qualifier = prefix.contains('.');
        let trigger = if has_qualifier {
            IntelliSenseTriggerKind::Dot
        } else if prefix.is_empty() {
            IntelliSenseTriggerKind::Space
        } else if prefix.starts_with('@') {
            IntelliSenseTriggerKind::AtSign
        } else {
            IntelliSenseTriggerKind::Identifier
        };
        let clause = current_clause(sql, cursor);
        let has_aliases = has_alias_in_scope(sql, cursor);
        let statement_lower = sql.to_ascii_lowercase();
        let aliases = sqlly_intel::intellisense::query_aliases(sql, cursor);
        let alias_count = aliases.len() as i32;
        let table_count = count_table_refs(sql, cursor);
        Self {
            prefix,
            clause,
            trigger,
            has_qualifier,
            has_aliases,
            statement_lower,
            sql,
            table_count,
            alias_count,
            manual: false,
        }
    }
}

/// Strip JSONC comments (`//` line and `/* */` block) outside of string
/// literals, leaving valid JSON. Pure.
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
                continue;
            }
            if c == '\\' {
                escaped = true;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' => match chars.peek() {
                Some('/') => {
                    // line comment: skip to end of line
                    chars.next();
                    for c in chars.by_ref() {
                        if c == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                }
                Some('*') => {
                    // block comment: skip to closing */
                    chars.next();
                    let mut prev = '\0';
                    for c in chars.by_ref() {
                        if prev == '*' && c == '/' {
                            break;
                        }
                        if c == '\n' {
                            out.push('\n'); // preserve line structure
                        }
                        prev = c;
                    }
                }
                _ => out.push(c),
            },
            _ => out.push(c),
        }
    }
    out
}

/// Parse a JSONC config document into rules. Returns the parsed config or an
/// error. Pure.
pub fn parse(text: &str) -> Result<IntelliSenseRuleConfig, serde_json::Error> {
    let stripped = strip_jsonc(text);
    serde_json::from_str(&stripped)
}

/// Default config text used when the on-disk file is missing or unreadable.
pub fn default_config_text() -> String {
    "{\"version\":1,\"rules\":[]}".to_string()
}

/// The canonical lowercased name for a [`CompletionKind`], used to match
/// against a rule's `allowed_kinds` / `blocked_kinds` string sets.
pub fn kind_name(kind: CompletionKind) -> &'static str {
    match kind {
        CompletionKind::Keyword => "keyword",
        CompletionKind::Database => "database",
        CompletionKind::Schema => "schema",
        CompletionKind::Table => "table",
        CompletionKind::Column => "column",
        CompletionKind::Function => "function",
        CompletionKind::Procedure => "procedure",
        CompletionKind::Type => "type",
        CompletionKind::Parameter => "parameter",
        CompletionKind::ResultSetColumn => "resultsetcolumn",
        CompletionKind::Snippet => "snippet",
        CompletionKind::Value => "value",
        CompletionKind::Join => "join",
    }
}

/// Maps an [`IntelliSenseIntent`] to the same kind-name space used by
/// [`kind_name`]. `View` collapses to `"table"` (table + view share a
/// [`CompletionKind`]); `Alias` maps to `None` (no direct completion kind).
pub fn intent_to_kind_name(intent: IntelliSenseIntent) -> Option<&'static str> {
    match intent {
        IntelliSenseIntent::Table => Some("table"),
        IntelliSenseIntent::View => Some("table"),
        IntelliSenseIntent::Function => Some("function"),
        IntelliSenseIntent::Procedure => Some("procedure"),
        IntelliSenseIntent::Schema => Some("schema"),
        IntelliSenseIntent::Column => Some("column"),
        IntelliSenseIntent::Keyword => Some("keyword"),
        IntelliSenseIntent::Snippet => Some("snippet"),
        IntelliSenseIntent::Alias => None,
        IntelliSenseIntent::SelectList
        | IntelliSenseIntent::FromSource
        | IntelliSenseIntent::WherePredicate
        | IntelliSenseIntent::JoinSource
        | IntelliSenseIntent::JoinCondition
        | IntelliSenseIntent::GroupBy
        | IntelliSenseIntent::OrderBy
        | IntelliSenseIntent::Having
        | IntelliSenseIntent::SetAssignment
        | IntelliSenseIntent::ValueLiteral
        | IntelliSenseIntent::Parameter
        | IntelliSenseIntent::ExecProcedure
        | IntelliSenseIntent::QualifiedMember
        | IntelliSenseIntent::General
        | IntelliSenseIntent::Unknown => None,
    }
}

/// Filters `items` according to `cfg`, given the request `ctx`.
///
/// A rule is *active* when its `when` condition matches `ctx` (clause, trigger,
/// prefix, qualifier, statement-contains). An active rule then:
/// - drops items whose kind is in `blocked_kinds`;
/// - keeps only items whose kind is in `allowed_kinds` (when set);
/// - additionally constrains by `intents` (mapped to kind names) when present;
/// - when `render == Some(false)` and no kind/intent filter is specified, hides
///   *all* completions (a "suppress suggestions" rule).
///
/// Rules are applied in order; later rules operate on the survivors of earlier
/// ones. Returns the filtered list (ownership is taken and returned to avoid a
/// copy when no rules are active).
pub fn apply_rules(
    mut items: Vec<CompletionItem>,
    ctx: &RuleContext<'_>,
    cfg: &IntelliSenseRuleConfig,
) -> Vec<CompletionItem> {
    for rule in &cfg.rules {
        if !rule_matches(rule, ctx) {
            continue;
        }
        // Intent → kind-name allowlist (only when intents are specified).
        let intent_kinds: Option<HashSet<&'static str>> = if rule.condition.intents.is_empty() {
            None
        } else {
            let mapped: HashSet<&'static str> = rule
                .condition
                .intents
                .iter()
                .filter_map(|i| intent_to_kind_name(*i))
                .collect();
            // If intents only named unmappable intents (e.g. `Alias`), don't
            // add a constraint.
            if mapped.is_empty() {
                None
            } else {
                Some(mapped)
            }
        };

        // `render: Some(false)` with no kind/intent constraint => suppress all.
        if rule.render == Some(false)
            && rule.allowed_kinds.is_none()
            && rule.blocked_kinds.is_empty()
            && intent_kinds.is_none()
        {
            return Vec::new();
        }

        if !rule.blocked_kinds.is_empty() {
            items.retain(|it| !rule.blocked_kinds.contains(kind_name(it.kind)));
        }
        if let Some(allowed) = &rule.allowed_kinds {
            items.retain(|it| allowed.contains(kind_name(it.kind)));
        }
        if let Some(intent_kinds) = &intent_kinds {
            items.retain(|it| intent_kinds.contains(kind_name(it.kind)));
        }
        if let Some(max) = rule.max_items {
            items.truncate(max);
        }
    }
    items
}

/// Whether `rule` is active in `ctx`. Pure, no allocation beyond comparisons.
fn rule_matches(rule: &IntelliSenseRule, ctx: &RuleContext<'_>) -> bool {
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
        && ctx.has_qualifier != hq
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
    true
}

/// Best-effort detection of the SQL clause at the cursor. Scans the text before
/// the cursor for the most recent clause keyword.
fn current_clause(sql: &str, cursor: usize) -> IntelliSenseClause {
    let cursor = cursor.min(sql.len());
    let before = sql[..cursor].to_ascii_uppercase();
    let words: Vec<&str> = before
        .split(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';')
        .filter(|w| !w.is_empty())
        .collect();
    let mut idx = words.len();
    while idx > 0 {
        idx -= 1;
        match words[idx] {
            "SELECT" => return IntelliSenseClause::Select,
            "FROM" => return IntelliSenseClause::From,
            "WHERE" => return IntelliSenseClause::Where,
            "JOIN" | "INNER" | "LEFT" | "RIGHT" | "OUTER" | "CROSS" => {
                return IntelliSenseClause::Join;
            }
            "ON" => return IntelliSenseClause::On,
            "HAVING" => return IntelliSenseClause::Having,
            "SET" => return IntelliSenseClause::Set,
            "VALUES" => return IntelliSenseClause::Values,
            "EXEC" | "EXECUTE" => return IntelliSenseClause::Exec,
            "INSERT" => return IntelliSenseClause::Insert,
            "UPDATE" => return IntelliSenseClause::Update,
            "DELETE" => return IntelliSenseClause::Delete,
            "CREATE" => return IntelliSenseClause::Create,
            "ALTER" => return IntelliSenseClause::Alter,
            "DROP" => return IntelliSenseClause::Drop,
            "BY" if idx > 0 => {
                let prev = words[idx - 1];
                if prev == "GROUP" {
                    return IntelliSenseClause::GroupBy;
                }
                if prev == "ORDER" {
                    return IntelliSenseClause::OrderBy;
                }
            }
            _ => {}
        }
    }
    IntelliSenseClause::Unknown
}

/// Best-effort: is there a `FROM <object> <alias>` in scope before the cursor?
fn has_alias_in_scope(sql: &str, cursor: usize) -> bool {
    let cursor = cursor.min(sql.len());
    let before = &sql[..cursor];
    let upper = before.to_ascii_uppercase();
    let mut found_from = false;
    for w in upper.split_whitespace() {
        if w == "FROM" || w == "JOIN" {
            found_from = true;
            continue;
        }
        if found_from {
            return true;
        }
    }
    false
}

fn count_table_refs(sql: &str, cursor: usize) -> i32 {
    let cursor = cursor.min(sql.len());
    let before = &sql[..cursor];
    let upper = before.to_ascii_uppercase();
    let mut count = 0;
    let mut expect_table = false;
    for w in upper.split_whitespace() {
        if w == "FROM" || w == "JOIN" {
            expect_table = true;
            continue;
        }
        if expect_table {
            let is_join_kw = matches!(
                w,
                "INNER" | "LEFT" | "RIGHT" | "OUTER" | "CROSS" | "FULL" | "JOIN"
            );
            if is_join_kw {
                continue;
            }
            let is_clause_kw = matches!(
                w,
                "WHERE"
                    | "GROUP"
                    | "ORDER"
                    | "HAVING"
                    | "SET"
                    | "VALUES"
                    | "ON"
                    | "AND"
                    | "OR"
                    | "AS"
            );
            if is_clause_kw {
                expect_table = false;
                continue;
            }
            if !w.is_empty() {
                count += 1;
                expect_table = false;
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_jsonc_line_comment() {
        let input = "{\n  // comment\n  \"a\": 1\n}";
        assert_eq!(strip_jsonc(input), "{\n  \n  \"a\": 1\n}");
    }

    #[test]
    fn strip_jsonc_block_comment() {
        let input = "{\n  /* block\n  comment */\n  \"a\": 1\n}";
        assert_eq!(strip_jsonc(input), "{\n  \n\n  \"a\": 1\n}");
    }

    #[test]
    fn strip_jsonc_keeps_comment_in_string() {
        let input = "{\"a\": \"// not a comment\"}";
        assert_eq!(strip_jsonc(input), input);
    }

    #[test]
    fn strip_jsonc_keeps_block_comment_in_string() {
        let input = "{\"a\": \"/* not a comment */\"}";
        assert_eq!(strip_jsonc(input), input);
    }

    #[test]
    fn strip_jsonc_handles_escaped_quote_in_string() {
        let input = "{\"a\": \"\\\"// not comment\"}";
        assert_eq!(strip_jsonc(input), input);
    }

    #[test]
    fn parse_empty_config() {
        let cfg = parse("{\"version\":1,\"rules\":[]}").unwrap();
        assert_eq!(cfg.version, Some(1));
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn parse_rule_with_condition() {
        let json = r#"{"rules":[{"name":"test","when":{"intent":["table"],"clause":["select"],"prefixEquals":"S"},"render":true,"blockedKinds":["view"]}]}"#;
        let cfg = parse(json).unwrap();
        assert_eq!(cfg.rules.len(), 1);
        let rule = &cfg.rules[0];
        assert_eq!(rule.name, "test");
        assert!(rule.condition.intents.contains(&IntelliSenseIntent::Table));
        assert!(rule.condition.clauses.contains(&IntelliSenseClause::Select));
        assert_eq!(rule.condition.prefix_equals.as_deref(), Some("S"));
        assert_eq!(rule.render, Some(true));
        assert!(rule.blocked_kinds.contains("view"));
    }

    #[test]
    fn parse_jsonc_with_comments() {
        let json = r#"{
            // a comment
            "rules": [
                {
                    "name": "r1" /* inline */,
                    "when": {}
                }
            ]
        }"#;
        let cfg = parse(json).unwrap();
        assert_eq!(cfg.rules.len(), 1);
        assert_eq!(cfg.rules[0].name, "r1");
    }

    #[test]
    fn parse_missing_rules_defaults_to_empty() {
        let cfg = parse("{\"version\":2}").unwrap();
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn parse_missing_version_is_none() {
        let cfg = parse("{\"rules\":[]}").unwrap();
        assert_eq!(cfg.version, None);
    }

    #[test]
    fn parse_invalid_json_errors() {
        assert!(parse("{not valid}").is_err());
    }

    #[test]
    fn parse_keyboard_section() {
        let json =
            r#"{"rules":[{"name":"k","when":{},"keyboard":{"key":"Tab","modifiers":["shift"]}}]}"#;
        let cfg = parse(json).unwrap();
        let kb = cfg.rules[0].keyboard.as_ref().unwrap();
        assert_eq!(kb.key.as_deref(), Some("Tab"));
        assert!(kb.modifiers.as_ref().unwrap().contains("shift"));
    }

    #[test]
    fn default_config_text_parses() {
        let cfg = parse(&default_config_text()).unwrap();
        assert!(cfg.rules.is_empty());
    }

    #[test]
    fn strip_jsonc_preserves_newlines_in_block_comment() {
        let input = "{/* a\nb\nc */\"x\":1}";
        let out = strip_jsonc(input);
        assert_eq!(out.matches('\n').count(), 2);
        assert!(out.contains("\"x\":1"));
    }

    #[test]
    fn strip_jsonc_trailing_slash_in_string() {
        let input = "{\"path\": \"C:/Users/\"}";
        assert_eq!(strip_jsonc(input), input);
    }

    // ---- apply_rules behavior -------------------------------------------

    fn item(label: &str, kind: CompletionKind) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            insert_text: label.to_string(),
            kind,
            source: sqlly_intel::intellisense::CompletionSource::LiveMetadata,
            score: 50,
            detail: String::new(),
            replace_span: None,
        }
    }

    #[test]
    fn apply_rules_no_rules_returns_input_unchanged() {
        let cfg = IntelliSenseRuleConfig {
            version: Some(1),
            rules: Vec::new(),
        };
        let ctx = RuleContext::from("SELECT * FROM ", 14);
        let items = vec![
            item("Orders", CompletionKind::Table),
            item("SELECT", CompletionKind::Keyword),
        ];
        let out = apply_rules(items, &ctx, &cfg);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn apply_rules_blocked_kinds_drops_matching_items() {
        let cfg = parse(
            r#"{"rules":[{"name":"no-keywords","when":{"clause":["from"]},"blockedKinds":["keyword"]}]}"#,
        )
        .unwrap();
        let ctx = RuleContext::from("SELECT * FROM ord", 17);
        let items = vec![
            item("Orders", CompletionKind::Table),
            item("SELECT", CompletionKind::Keyword),
            item("FROM", CompletionKind::Keyword),
        ];
        let out = apply_rules(items, &ctx, &cfg);
        assert!(out.iter().all(|i| i.kind != CompletionKind::Keyword));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "Orders");
    }

    #[test]
    fn apply_rules_allowed_kinds_keeps_only_listed() {
        let cfg = parse(r#"{"rules":[{"name":"only-tables","when":{},"allowedKinds":["table"]}]}"#)
            .unwrap();
        let ctx = RuleContext::from("SELECT c", 8);
        let items = vec![
            item("Orders", CompletionKind::Table),
            item("CustomerId", CompletionKind::Column),
            item("SELECT", CompletionKind::Keyword),
        ];
        let out = apply_rules(items, &ctx, &cfg);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "Orders");
    }

    #[test]
    fn apply_rules_render_false_with_no_kinds_suppresses_all() {
        let cfg =
            parse(r#"{"rules":[{"name":"hide","when":{"clause":["update"]},"render":false}]}"#)
                .unwrap();
        // Clause is Update.
        let ctx = RuleContext::from("UPDATE ", 7);
        let items = vec![item("Orders", CompletionKind::Table)];
        let out = apply_rules(items, &ctx, &cfg);
        assert!(out.is_empty());
    }

    #[test]
    fn apply_rules_intent_table_keeps_only_tables() {
        let cfg = parse(r#"{"rules":[{"name":"tables","when":{"intent":["table"]}}]}"#).unwrap();
        let ctx = RuleContext::from("SELECT * FROM ", 14);
        let items = vec![
            item("Orders", CompletionKind::Table),
            item("sp_get", CompletionKind::Procedure),
            item("SELECT", CompletionKind::Keyword),
        ];
        let out = apply_rules(items, &ctx, &cfg);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "Orders");
    }

    #[test]
    fn apply_rules_rule_not_matching_clause_is_inactive() {
        let cfg = parse(
            r#"{"rules":[{"name":"from-only","when":{"clause":["from"]},"blockedKinds":["keyword"]}]}"#,
        )
        .unwrap();
        // Clause is Select, rule wants From => inactive => keyword survives.
        let ctx = RuleContext::from("SELECT c", 8);
        let items = vec![item("SELECT", CompletionKind::Keyword)];
        let out = apply_rules(items, &ctx, &cfg);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn rule_context_detects_clause_and_qualifier() {
        let ctx = RuleContext::from("SELECT sales.ord", 16);
        assert_eq!(ctx.clause, IntelliSenseClause::Select);
        assert!(ctx.has_qualifier);
        assert_eq!(ctx.trigger, IntelliSenseTriggerKind::Dot);
        assert_eq!(ctx.prefix, "sales.ord");
    }

    #[test]
    fn kind_name_maps_all_variants() {
        assert_eq!(kind_name(CompletionKind::Table), "table");
        assert_eq!(kind_name(CompletionKind::Procedure), "procedure");
        assert_eq!(kind_name(CompletionKind::Snippet), "snippet");
        assert_eq!(kind_name(CompletionKind::Column), "column");
    }

    #[test]
    fn intent_view_collapses_to_table() {
        assert_eq!(intent_to_kind_name(IntelliSenseIntent::View), Some("table"));
        assert_eq!(intent_to_kind_name(IntelliSenseIntent::Alias), None);
    }
}
