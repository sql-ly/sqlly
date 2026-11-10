use crate::agent::connections::{
    apply_database_override, effective_database, is_production, load_store, resolve_connection,
    store_path,
};
use crate::agent::{CliError, ExitCode};
use serde_json::{Value, json};
use sqlly_services::transfer::plan::{
    NameCasing, TransferColumn, TransferMode, TransferPlan, TransferTable,
};
use sqlly_services::transfer::run::{
    TransferCancel, TransferOutcome, TransferProgress, run_transfer,
};
use sqlly_services::transfer::sink::{TransferSink, open_sink};
use std::collections::{HashMap, HashSet};
use std::io::{IsTerminal, Write};

#[derive(Debug, Clone)]
pub struct TransferArgs {
    pub connection: String,
    pub database: Option<String>,
    pub target_profile: String,
    pub target_database: Option<String>,
    pub target_schema: Option<String>,
    pub mode: String,
    pub casing: String,
    pub tables: Vec<String>,
}

pub async fn run(args: TransferArgs, json: bool) -> ExitCode {
    match run_inner(args).await {
        Ok(outcome) => finish_outcome(json, outcome),
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

fn finish_outcome(json: bool, outcome: TransferOutcome) -> ExitCode {
    let data = outcome_to_json(&outcome);
    if outcome.cancelled {
        if json {
            print_json(&json!({
                "ok": false,
                "data": data,
                "error": {
                    "code": "cancelled",
                    "message": "transfer cancelled",
                },
            }));
        } else {
            print_json(&data);
        }
        return ExitCode::Cancelled;
    }
    if outcome.error.is_some() {
        if json {
            print_json(&json!({
                "ok": false,
                "data": data,
                "error": {
                    "code": "transfer",
                    "message": outcome.error.as_ref().map(ToString::to_string),
                },
            }));
        } else {
            print_json(&data);
        }
        return ExitCode::Error;
    }
    crate::agent::emit(json, None, Ok(data))
}

async fn run_inner(args: TransferArgs) -> Result<TransferOutcome, CliError> {
    let store = load_store(&store_path())?;
    let mut source_profile = resolve_connection(&store, &args.connection)?;
    source_profile = apply_database_override(source_profile, args.database);
    let source_db = effective_database(&source_profile)?;

    let mut target_profile = resolve_connection(&store, &args.target_profile)?;
    target_profile = apply_database_override(target_profile, args.target_database);

    if is_production(&store, &target_profile) {
        return Err(CliError::policy(
            "writes are refused on production connections from the CLI",
            Some(
                "use the app's production unlock, or run against a non-production connection"
                    .into(),
            ),
        ));
    }

    let mode = parse_mode(&args.mode)?;
    let casing = parse_casing(&args.casing)?;
    let parsed_tables = parse_tables(&args.tables)?;

    let services = sqlly_services::Services::new();
    let plan = build_plan(
        &services,
        &source_profile,
        &source_db,
        &parsed_tables,
        &target_profile,
        args.target_schema,
        mode,
        casing,
    )
    .await?;

    let mut sink = open_sink(&target_profile).await.map_err(CliError::from)?;

    let cancel = TransferCancel::default();
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_for_signal.request();
        }
    });

    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel(64);
    let progress_task = tokio::spawn(async move {
        while let Some(event) = progress_rx.recv().await {
            eprintln!("{}", format_progress(&event));
            if matches!(event, TransferProgress::Finished(_)) {
                break;
            }
        }
    });

    let outcome = run_transfer(
        &services,
        &source_profile,
        &mut sink as &mut dyn TransferSink,
        &plan,
        progress_tx,
        cancel,
    )
    .await;

    let _ = progress_task.await;
    Ok(outcome)
}

#[allow(clippy::too_many_arguments)]
async fn build_plan(
    services: &sqlly_services::Services,
    source_profile: &sqlly_core::ConnectionProfile,
    source_db: &str,
    parsed_tables: &[(Option<String>, String)],
    target_profile: &sqlly_core::ConnectionProfile,
    target_schema: Option<String>,
    mode: TransferMode,
    casing: NameCasing,
) -> Result<TransferPlan, CliError> {
    let columns = services
        .metadata
        .list_all_columns(source_profile, source_db)
        .await?;
    let primary_keys = services
        .metadata
        .list_primary_keys(source_profile, source_db)
        .await?;
    let foreign_keys = services
        .metadata
        .list_foreign_keys(source_profile, source_db)
        .await?;

    let selection: HashSet<String> = parsed_tables
        .iter()
        .map(|(schema, name)| table_key(schema, name))
        .collect();

    let mut tables = Vec::new();
    for (schema, name) in parsed_tables {
        let schema_label = schema.as_deref().unwrap_or("dbo");
        let col_rows: Vec<_> = columns
            .iter()
            .filter(|row| {
                schema_matches(&row.schema, schema_label) && names_match(&row.object, name)
            })
            .collect();
        if col_rows.is_empty() {
            return Err(CliError::not_found(format!(
                "table not found: {}",
                display_table(schema, name)
            )));
        }

        let pk_ordinals: HashMap<String, i64> = primary_keys
            .iter()
            .filter(|row| {
                schema_matches(&row.schema, schema_label) && names_match(&row.table, name)
            })
            .map(|row| (row.column.clone(), row.key_ordinal))
            .collect();

        let transfer_columns: Vec<TransferColumn> = col_rows
            .into_iter()
            .map(|row| TransferColumn {
                name: row.column.clone(),
                source_type: row.type_name.clone().unwrap_or_else(|| "varchar".into()),
                nullable: row.nullable.unwrap_or(true),
                is_identity: row.is_identity,
                pk_ordinal: pk_ordinals.get(&row.column).copied(),
            })
            .collect();

        let mut depends_on = Vec::new();
        for fk in &foreign_keys {
            if schema_matches(&fk.from_schema, schema_label) && names_match(&fk.from_table, name) {
                let parent_key = table_key(&Some(fk.to_schema.clone()), &fk.to_table);
                if selection.contains(&parent_key) {
                    depends_on.push(format!("{}.{}", fk.to_schema, fk.to_table));
                }
            }
        }
        depends_on.sort();
        depends_on.dedup();

        tables.push(TransferTable {
            schema: schema.clone(),
            name: name.clone(),
            columns: transfer_columns,
            depends_on,
        });
    }

    Ok(TransferPlan {
        source: source_profile.database_type,
        target: target_profile.database_type,
        target_schema,
        mode,
        casing,
        tables,
    })
}

fn parse_tables(names: &[String]) -> Result<Vec<(Option<String>, String)>, CliError> {
    names.iter().map(|name| parse_table(name)).collect()
}

pub fn parse_table(name: &str) -> Result<(Option<String>, String), CliError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(CliError::usage("table name must not be empty"));
    }
    if let Some((schema, table)) = trimmed.rsplit_once('.') {
        if schema.is_empty() || table.is_empty() {
            return Err(CliError::usage(
                "table must be schema.name or name (default schema dbo)",
            ));
        }
        return Ok((Some(schema.to_string()), table.to_string()));
    }
    Ok((Some("dbo".to_string()), trimmed.to_string()))
}

fn parse_mode(mode: &str) -> Result<TransferMode, CliError> {
    match mode {
        "create" => Ok(TransferMode::CreateTable),
        "append" => Ok(TransferMode::Append),
        "truncate" => Ok(TransferMode::TruncateThenInsert),
        "upsert" => Ok(TransferMode::UpsertOnPrimaryKey),
        other => Err(CliError::usage(format!(
            "invalid mode '{other}'; expected create, append, truncate, or upsert"
        ))),
    }
}

fn parse_casing(casing: &str) -> Result<NameCasing, CliError> {
    match casing {
        "as-is" => Ok(NameCasing::AsIs),
        "lower" => Ok(NameCasing::Lower),
        "upper" => Ok(NameCasing::Upper),
        other => Err(CliError::usage(format!(
            "invalid casing '{other}'; expected as-is, lower, or upper"
        ))),
    }
}

fn table_key(schema: &Option<String>, name: &str) -> String {
    let schema = schema.as_deref().unwrap_or("").to_ascii_lowercase();
    let name = name.to_ascii_lowercase();
    if schema.is_empty() {
        name
    } else {
        format!("{schema}.{name}")
    }
}

fn display_table(schema: &Option<String>, name: &str) -> String {
    match schema {
        Some(schema) if !schema.is_empty() => format!("{schema}.{name}"),
        _ => name.to_string(),
    }
}

fn schema_matches(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn names_match(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn format_progress(event: &TransferProgress) -> String {
    match event {
        TransferProgress::TableStarted {
            table,
            index,
            total,
        } => format!("transfer: table {table} started ({index}/{total})"),
        TransferProgress::Rows { table, copied } => {
            format!("transfer: {table} copied {copied} rows")
        }
        TransferProgress::TableFinished { table, copied } => {
            format!("transfer: table {table} finished ({copied} rows)")
        }
        TransferProgress::Warning(message) => format!("transfer: warning: {message}"),
        TransferProgress::Finished(outcome) => {
            format!(
                "transfer: finished ({} tables, cancelled={})",
                outcome.tables.len(),
                outcome.cancelled
            )
        }
    }
}

fn outcome_to_json(outcome: &TransferOutcome) -> Value {
    json!({
        "tables": outcome
            .tables
            .iter()
            .map(|(table, rows)| json!({ "table": table, "rows": rows }))
            .collect::<Vec<_>>(),
        "warnings": outcome.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "error": outcome.error.as_ref().map(ToString::to_string),
        "cancelled": outcome.cancelled,
    })
}

fn print_json<T: serde::Serialize>(value: &T) {
    let pretty = std::io::stdout().is_terminal();
    let text = if pretty {
        serde_json::to_string_pretty(value).expect("serialize json")
    } else {
        serde_json::to_string(value).expect("serialize json")
    };
    let _ = writeln!(std::io::stdout(), "{text}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{ConnectionEnvironment, ConnectionProfile, SecretProvider, SecretRef};

    #[test]
    fn parse_table_uses_last_dot() {
        assert_eq!(
            parse_table("sales.dbo.Orders").unwrap(),
            (Some("sales.dbo".into()), "Orders".into())
        );
    }

    #[test]
    fn parse_table_defaults_schema() {
        assert_eq!(
            parse_table("Orders").unwrap(),
            (Some("dbo".into()), "Orders".into())
        );
    }

    #[test]
    fn parse_mode_and_casing() {
        assert_eq!(
            parse_mode("upsert").unwrap(),
            TransferMode::UpsertOnPrimaryKey
        );
        assert_eq!(parse_casing("lower").unwrap(), NameCasing::Lower);
    }

    #[test]
    fn production_target_refused() {
        let mut profile = ConnectionProfile::sql_login(
            "prod",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.environment = ConnectionEnvironment::Production;
        let store = crate::agent::connections::StoreFile {
            version: 1,
            profiles: vec![profile.clone()],
            environments: vec![],
        };
        assert!(is_production(&store, &profile));
    }
}
