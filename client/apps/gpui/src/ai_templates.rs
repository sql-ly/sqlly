//! Prompt templates for the AI drawer. Builtins plus an optional JSON file.

use crate::host_storage::{StorageArea, load_text, store_text};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PromptTemplate {
    pub name: String,
    pub body: String,
    #[serde(default)]
    pub lane: Option<String>,
}

pub const TEMPLATES_FILE: &str = "ai-templates.json";
pub const MAX_TEMPLATES: usize = 50;
const MAX_SELECTION_CHARS: usize = 4_000;

pub fn builtin_templates() -> Vec<PromptTemplate> {
    vec![
        PromptTemplate {
            name: crate::localization::text("ai.templates.explain_selection_name"),
            body: crate::localization::text("ai.templates.explain_selection_body"),
            lane: Some("ask".into()),
        },
        PromptTemplate {
            name: crate::localization::text("ai.templates.optimize_selection_name"),
            body: crate::localization::text("ai.templates.optimize_selection_body"),
            lane: Some("generate".into()),
        },
        PromptTemplate {
            name: crate::localization::text("ai.templates.comment_selection_name"),
            body: crate::localization::text("ai.templates.comment_selection_body"),
            lane: Some("generate".into()),
        },
        PromptTemplate {
            name: crate::localization::text("ai.templates.describe_table_name"),
            body: crate::localization::text("ai.templates.describe_table_body"),
            lane: Some("ask".into()),
        },
    ]
}

pub fn load_templates() -> Vec<PromptTemplate> {
    let Some(text) = load_text(StorageArea::Config, TEMPLATES_FILE) else {
        return builtin_templates();
    };
    parse_templates_json(&text).unwrap_or_else(builtin_templates)
}

// Retained for future use.
#[allow(dead_code)]
pub fn save_templates(templates: &[PromptTemplate]) -> Result<(), String> {
    let capped: Vec<PromptTemplate> = templates.iter().take(MAX_TEMPLATES).cloned().collect();
    let json = serde_json::to_string_pretty(&capped).map_err(|e| e.to_string())?;
    store_text(StorageArea::Config, TEMPLATES_FILE, &json)
}

#[derive(Default)]
pub struct TemplateVars<'a> {
    pub selection: Option<&'a str>,
    pub table: Option<&'a str>,
    pub database: Option<&'a str>,
    pub dialect: Option<&'a str>,
}

/// Replace `${selection}`, `${table}`, `${database}`, `${dialect}`; unknown
/// placeholders are left verbatim; a missing value substitutes "".
/// Selection is bounded to 4_000 chars with "\n-- (truncated)".
pub fn substitute(body: &str, vars: &TemplateVars<'_>) -> String {
    let selection = bound_selection(vars.selection.unwrap_or(""));
    body.replace("${selection}", &selection)
        .replace("${table}", vars.table.unwrap_or(""))
        .replace("${database}", vars.database.unwrap_or(""))
        .replace("${dialect}", vars.dialect.unwrap_or(""))
}

fn bound_selection(sql: &str) -> String {
    let mut bounded: String = sql.chars().take(MAX_SELECTION_CHARS).collect();
    if sql.chars().count() > MAX_SELECTION_CHARS {
        bounded.push_str("\n-- (truncated)");
    }
    bounded
}

fn parse_templates_json(text: &str) -> Option<Vec<PromptTemplate>> {
    let parsed: Vec<PromptTemplate> = serde_json::from_str(text).ok()?;
    if parsed.is_empty() {
        return None;
    }
    Some(parsed.into_iter().take(MAX_TEMPLATES).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_replaces_known_placeholders_and_keeps_unknown() {
        let vars = TemplateVars {
            selection: Some("SELECT 1"),
            table: Some("dbo.Orders"),
            database: Some("sales"),
            dialect: Some("sqlserver"),
        };
        let out = substitute(
            "On ${database} (${dialect}) explain ${table}:\n${selection}\n${unknown}",
            &vars,
        );
        assert!(out.contains("sales"));
        assert!(out.contains("sqlserver"));
        assert!(out.contains("dbo.Orders"));
        assert!(out.contains("SELECT 1"));
        assert!(out.contains("${unknown}"));
    }

    #[test]
    fn substitute_bounds_selection() {
        let long = "x".repeat(5_000);
        let vars = TemplateVars {
            selection: Some(&long),
            ..TemplateVars::default()
        };
        let out = substitute("${selection}", &vars);
        assert!(out.contains("-- (truncated)"));
        assert!(out.chars().count() < 5_000);
        let empty = substitute("${table}${database}${dialect}", &TemplateVars::default());
        assert_eq!(empty, "");
    }

    #[test]
    fn templates_roundtrip_json() {
        let templates = builtin_templates();
        let json = serde_json::to_string_pretty(&templates).unwrap();
        let parsed: Vec<PromptTemplate> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, templates);
        assert_eq!(parsed.len(), 4);
    }

    #[test]
    fn load_falls_back_to_builtins_on_garbage() {
        assert_eq!(
            parse_templates_json("not json")
                .unwrap_or_else(builtin_templates)
                .len(),
            4
        );
        assert_eq!(
            parse_templates_json("[]")
                .unwrap_or_else(builtin_templates)
                .len(),
            4
        );
    }
}
