use crate::agent::connections::{
    apply_database_override, effective_database, load_store, resolve_connection, store_path,
    summary,
};
use crate::agent::{CliError, ExitCode};
use serde_json::json;
use sqlly_ai::catalog::{DEFAULT_CONTEXT_TOKENS, SchemaPurpose, assemble_context_from_store};
use sqlly_services::catalog::DefaultCatalogService;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum ContextPurposeArg {
    #[default]
    General,
    #[value(name = "sql-generation")]
    SqlGeneration,
    Safety,
}

#[derive(Debug, Clone)]
pub struct ContextArgs {
    pub connection: String,
    pub database: Option<String>,
    pub budget_tokens: Option<usize>,
    pub purpose: ContextPurposeArg,
}

pub async fn run(args: ContextArgs, json: bool) -> ExitCode {
    let result = async {
        let store = load_store(&store_path())?;
        let mut profile = resolve_connection(&store, &args.connection)?;
        profile = apply_database_override(profile, args.database);
        let _database = effective_database(&profile)?;

        let catalog = DefaultCatalogService::new();
        let catalog_store = catalog.open_store(&profile)?;
        let objects = catalog_store
            .live_objects()
            .map_err(|err| CliError::internal(err.to_string()))?;
        if objects.is_empty() {
            return Err(CliError::not_found(
                "no cached schema for this connection; open it in the app once (Schema sync) or run `sqlly-cli schema list` to warm it",
            ));
        }

        let mut ordered = objects;
        ordered.sort_by(|a, b| a.object_key.cmp(&b.object_key));
        let purpose = purpose_from_arg(args.purpose);
        let budget = args.budget_tokens.unwrap_or(DEFAULT_CONTEXT_TOKENS);
        let context = assemble_context_from_store(&catalog_store, &ordered, budget, purpose)
            .map_err(|err| CliError::internal(err.to_string()))?;

        let data = json!({
            "tokens": context.tokens,
            "object_keys": context.object_keys,
            "used_prebuilt_cards": context.used_prebuilt_cards,
            "purpose": format!("{:?}", context.purpose),
            "text": context.text,
        });
        Ok::<_, CliError>((context.text.clone(), data))
    }
    .await;

    match result {
        Ok((text, data)) => crate::agent::emit(json, Some(text), Ok(data)),
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

pub fn purpose_from_arg(purpose: ContextPurposeArg) -> SchemaPurpose {
    match purpose {
        ContextPurposeArg::General => SchemaPurpose::General,
        ContextPurposeArg::SqlGeneration => SchemaPurpose::SqlGeneration,
        ContextPurposeArg::Safety => SchemaPurpose::SafetyCheck,
    }
}

pub async fn list_connections(format: crate::agent::format::OutputFormat, json: bool) -> ExitCode {
    let result = (|| {
        let store = load_store(&store_path())?;
        let summaries: Vec<_> = store.profiles.iter().map(summary).collect();
        let rows: Vec<_> = summaries
            .iter()
            .map(|p| {
                vec![
                    sqlly_core::CellValue::String(p["name"].as_str().unwrap_or("").to_string()),
                    sqlly_core::CellValue::String(p["engine"].as_str().unwrap_or("").to_string()),
                    sqlly_core::CellValue::String(p["server"].as_str().unwrap_or("").to_string()),
                    sqlly_core::CellValue::String(p["database"].as_str().unwrap_or("").to_string()),
                    sqlly_core::CellValue::String(
                        p["environment"].as_str().unwrap_or("").to_string(),
                    ),
                    sqlly_core::CellValue::String(p["id"].as_str().unwrap_or("").to_string()),
                ]
            })
            .collect();
        let set = crate::agent::format::ResultSet {
            columns: vec![
                sqlly_core::ResultColumn {
                    name: "name".into(),
                    data_type: "text".into(),
                    nullable: false,
                },
                sqlly_core::ResultColumn {
                    name: "engine".into(),
                    data_type: "text".into(),
                    nullable: false,
                },
                sqlly_core::ResultColumn {
                    name: "server".into(),
                    data_type: "text".into(),
                    nullable: false,
                },
                sqlly_core::ResultColumn {
                    name: "database".into(),
                    data_type: "text".into(),
                    nullable: true,
                },
                sqlly_core::ResultColumn {
                    name: "environment".into(),
                    data_type: "text".into(),
                    nullable: false,
                },
                sqlly_core::ResultColumn {
                    name: "id".into(),
                    data_type: "text".into(),
                    nullable: false,
                },
            ],
            rows,
            rows_affected: None,
            truncated: false,
        };
        let rendered = crate::agent::format::render(format, &[set]);
        Ok::<_, CliError>((rendered, json!(summaries)))
    })();

    match result {
        Ok((text, data)) => crate::agent::emit(json, Some(text), Ok(data)),
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_purpose_parses() {
        assert_eq!(
            purpose_from_arg(ContextPurposeArg::SqlGeneration),
            SchemaPurpose::SqlGeneration
        );
    }

    #[test]
    fn context_budget_default_matches_ask_rs() {
        let args = ContextArgs {
            connection: "dev".into(),
            database: None,
            budget_tokens: None,
            purpose: ContextPurposeArg::General,
        };
        assert_eq!(args.budget_tokens.unwrap_or(DEFAULT_CONTEXT_TOKENS), 4_000);
    }
}
