use crate::agent::connections::{
    apply_database_override, effective_database, is_production, load_store, resolve_connection,
    store_path,
};
use crate::agent::{CliError, ExitCode};
use serde_json::{Value, json};
use sqlly_lang::sqlmodel::ConnectionKind;
use sqlly_services::{RestoreKind, RestoreRequest, Services, sniff_restore_kind};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

#[derive(Debug, Clone)]
pub struct RestoreArgs {
    pub connection: String,
    pub database: String,
    pub input: PathBuf,
    pub clean: bool,
    pub if_exists: bool,
    pub single_transaction: bool,
    pub no_owner: bool,
}

/// What the restore can honestly do with the file, mirroring the app's
/// restore dialog: vendor archives go through pg_restore; plain SQL belongs
/// to `run-file`; a PostgreSQL archive cannot land on another engine.
enum RestorePlan {
    PgRestore,
    UseRunFile,
    WrongEngine(String),
}

fn plan_for(connection_kind: ConnectionKind, kind: RestoreKind) -> RestorePlan {
    match (connection_kind, kind) {
        (ConnectionKind::PostgresWire, RestoreKind::PgArchive) => RestorePlan::PgRestore,
        (_, RestoreKind::PlainSql) => RestorePlan::UseRunFile,
        (other, RestoreKind::PgArchive) => RestorePlan::WrongEngine(format!("{other:?}")),
    }
}

pub async fn run(args: RestoreArgs, json: bool) -> ExitCode {
    match run_inner(args).await {
        Ok(data) => {
            let text = (!json).then(|| restore_summary(&data));
            crate::agent::emit(json, text, Ok(data))
        }
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

/// One-line human summary printed on success in text mode.
fn restore_summary(data: &Value) -> String {
    let input = data["input"].as_str().unwrap_or_default();
    let database = data["database"].as_str().unwrap_or_default();
    if data["cancelled"].as_bool().unwrap_or(false) {
        return format!("Restore of {input} into {database} was cancelled");
    }
    let mut text = format!(
        "Restored {input} into {database} in {} ms",
        data["elapsed_ms"].as_u64().unwrap_or(0)
    );
    if let Some(warnings) = data["warnings"].as_array()
        && !warnings.is_empty()
    {
        text.push_str(&format!(" ({} warnings)", warnings.len()));
        for warning in warnings {
            text.push_str(&format!(
                "\nwarning: {}",
                warning.as_str().unwrap_or_default()
            ));
        }
    }
    text
}

/// The `run-file` line for a plain-SQL backup, as a copyable POSIX command.
fn run_file_hint(connection: &str, database: &str, input: &Path) -> String {
    format!(
        "run it with: sqlly-cli run-file {} --database {} --file {}",
        posix_quote(connection),
        posix_quote(database),
        posix_quote(&input.display().to_string())
    )
}

fn posix_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '@'))
        && !value.starts_with('-')
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

async fn run_inner(args: RestoreArgs) -> Result<Value, CliError> {
    let store = load_store(&store_path())?;
    let mut profile = resolve_connection(&store, &args.connection)?;
    profile = apply_database_override(profile, Some(args.database.clone()));
    let database = effective_database(&profile)?;

    if is_production(&store, &profile) {
        return Err(CliError::policy(
            "restores are refused on production connections from the CLI",
            Some("restore against a non-production connection".into()),
        ));
    }

    let head = read_head(&args.input)?;
    match plan_for(
        profile.database_type.connection_kind(),
        sniff_restore_kind(&head),
    ) {
        RestorePlan::PgRestore => {}
        RestorePlan::UseRunFile => {
            return Err(CliError::policy(
                format!(
                    "{} is a plain SQL script, not a vendor archive",
                    args.input.display()
                ),
                Some(run_file_hint(&args.connection, &database, &args.input)),
            ));
        }
        RestorePlan::WrongEngine(kind_label) => {
            return Err(CliError::policy(
                format!(
                    "{} is a PostgreSQL archive but the connection is {kind_label}",
                    args.input.display()
                ),
                None,
            ));
        }
    }

    let locations = sqlly_services::locate_all(&std::collections::HashMap::new()).await;
    let tool = sqlly_services::find_tool(&locations, sqlly_services::NativeTool::PgRestore)
        .cloned()
        .ok_or_else(|| {
            CliError::not_found(
                "pg_restore not found on PATH or in the usual install locations".to_string(),
            )
        })?;

    let req = RestoreRequest {
        database: database.clone(),
        input: args.input.clone(),
        clean: args.clean,
        if_exists: args.clean && args.if_exists,
        single_transaction: args.single_transaction,
        no_owner: args.no_owner,
    };
    let services = Services::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_for_signal.store(true, Ordering::Relaxed);
        }
    });
    let report = sqlly_services::run_native_restore(
        &services,
        &profile,
        &tool,
        &req,
        cancel,
        &mut |_progress| {},
    )
    .await
    .map_err(CliError::from)?;

    Ok(json!({
        "database": database,
        "input": args.input.display().to_string(),
        "bytes_written": report.bytes_written,
        "warnings": report.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "elapsed_ms": report.elapsed_ms,
        "cancelled": report.cancelled,
    }))
}

fn read_head(path: &Path) -> Result<Vec<u8>, CliError> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|err| CliError::io(err.to_string()))?;
    // `read` may return a short count; `take` + `read_to_end` fills the
    // sniff window whenever the file is long enough.
    let mut head = Vec::with_capacity(512);
    file.take(512)
        .read_to_end(&mut head)
        .map_err(|err| CliError::io(err.to_string()))?;
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_file_hint_uses_the_file_flag_and_quotes() {
        let hint = run_file_hint("Prod (RO)", "sales", Path::new("/tmp/my dump.sql"));
        assert_eq!(
            hint,
            "run it with: sqlly-cli run-file 'Prod (RO)' --database sales --file '/tmp/my dump.sql'"
        );
    }

    #[test]
    fn read_head_fills_the_sniff_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("x.tar");
        let mut bytes = vec![0u8; 600];
        bytes[257..262].copy_from_slice(b"ustar");
        std::fs::write(&path, &bytes).expect("write");
        let head = read_head(&path).expect("head");
        assert_eq!(head.len(), 512);
        assert_eq!(sniff_restore_kind(&head), RestoreKind::PgArchive);
    }

    #[test]
    fn restore_summary_reports_success_and_warnings() {
        let data = json!({
            "database": "sales",
            "input": "/b/s.dump",
            "elapsed_ms": 7,
            "warnings": ["role x does not exist"],
            "cancelled": false,
        });
        let text = restore_summary(&data);
        assert!(text.starts_with("Restored /b/s.dump into sales in 7 ms (1 warnings)"));
        assert!(text.contains("warning: role x does not exist"));
    }

    #[test]
    fn pg_archive_on_postgres_restores_natively() {
        assert!(matches!(
            plan_for(ConnectionKind::PostgresWire, RestoreKind::PgArchive),
            RestorePlan::PgRestore
        ));
    }

    #[test]
    fn plain_sql_always_routes_to_run_file() {
        for kind in [
            ConnectionKind::PostgresWire,
            ConnectionKind::MySqlWire,
            ConnectionKind::Tds,
        ] {
            assert!(matches!(
                plan_for(kind, RestoreKind::PlainSql),
                RestorePlan::UseRunFile
            ));
        }
    }

    #[test]
    fn pg_archive_on_mysql_is_refused() {
        assert!(matches!(
            plan_for(ConnectionKind::MySqlWire, RestoreKind::PgArchive),
            RestorePlan::WrongEngine(_)
        ));
    }
}
