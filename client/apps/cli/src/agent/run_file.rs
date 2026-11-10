use crate::agent::connections::{
    apply_database_override, is_production, load_store, resolve_connection, store_path,
};
use crate::agent::{CliError, ExitCode};
use serde_json::{Value, json};
use sqlly_lang::sqlmodel::Dialect;
use sqlly_services::mysql_definer::DefinerMode;
use sqlly_services::{
    OnError, ScriptCancel, ScriptOutcome, ScriptProgress, ScriptRunOptions, TransactionMode,
    open_script_file, run_script,
};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct RunFileArgs {
    pub connection: String,
    pub database: Option<String>,
    pub file: PathBuf,
    pub on_error: String,
    pub transaction: String,
    pub coalesce_inserts: bool,
    pub definer: String,
}

pub async fn run(args: RunFileArgs, json: bool) -> ExitCode {
    match run_inner(args).await {
        Ok(outcome) => finish_outcome(json, outcome),
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

fn finish_outcome(json: bool, outcome: ScriptOutcome) -> ExitCode {
    let data = outcome_to_json(&outcome);
    if outcome.cancelled || outcome.stopped_on_error {
        if json {
            print_json(&json!({
                "ok": false,
                "data": data,
                "error": {
                    "code": if outcome.cancelled { "cancelled" } else { "stopped_on_error" },
                    "message": if outcome.cancelled {
                        "script run cancelled"
                    } else {
                        "script run stopped on error"
                    },
                },
            }));
        } else {
            print_json(&data);
        }
        return ExitCode::Error;
    }
    if outcome.units_failed > 0 {
        if json {
            print_json(&json!({
                "ok": false,
                "data": data,
                "error": {
                    "code": "unit_failures",
                    "message": format!("{} statement(s) failed", outcome.units_failed),
                },
            }));
        } else {
            print_json(&data);
        }
        return ExitCode::Error;
    }
    crate::agent::emit(json, None, Ok(data))
}

async fn run_inner(args: RunFileArgs) -> Result<ScriptOutcome, CliError> {
    let store = load_store(&store_path())?;
    let mut profile = resolve_connection(&store, &args.connection)?;
    profile = apply_database_override(profile, args.database.clone());

    if is_production(&store, &profile) {
        return Err(CliError::policy(
            "running SQL scripts is refused on production connections from the CLI",
            Some(
                "use the app's production unlock, or run against a non-production connection"
                    .into(),
            ),
        ));
    }

    let on_error = parse_on_error(&args.on_error)?;
    let transaction = parse_transaction(&args.transaction)?;
    let dialect = profile.database_type.family();
    let definer = parse_definer(&args.definer, dialect)?;
    let session_key = format!("run-file:{}", uuid::Uuid::new_v4());

    let (reader, total_bytes) = open_script_file(&args.file)
        .map_err(|err| CliError::io(format!("could not open {}: {err}", args.file.display())))?;

    let services = sqlly_services::Services::new();
    let (progress_tx, mut progress_rx) = mpsc::channel::<ScriptProgress>(32);
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let cancel_for_signal = cancel_flag.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_for_signal.store(true, Ordering::Relaxed);
        }
    });
    let cancel = ScriptCancel::new();
    let cancel_for_poll = cancel.clone();
    let cancel_flag_poll = cancel_flag.clone();
    tokio::spawn(async move {
        loop {
            if cancel_flag_poll.load(Ordering::Relaxed) {
                cancel_for_poll.cancel();
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    let progress_task = tokio::spawn(async move {
        let mut last = Instant::now() - Duration::from_secs(1);
        while let Some(progress) = progress_rx.recv().await {
            let now = Instant::now();
            if progress.terminal || now.duration_since(last) >= Duration::from_millis(250) {
                eprintln!("{}", format_progress(&progress));
                last = now;
            }
        }
    });

    let options = ScriptRunOptions {
        dialect,
        on_error,
        transaction,
        coalesce_inserts: args.coalesce_inserts && dialect != Dialect::SqlServer,
        session_key,
        coalesce_max_rows: 500,
        coalesce_max_bytes: 1024 * 1024,
        definer,
    };

    let outcome = run_script(
        services.queries.as_ref(),
        &profile,
        reader,
        total_bytes,
        &options,
        progress_tx,
        cancel,
        |_| Box::pin(async { sqlly_services::ErrorDecision::Skip }),
    )
    .await
    .map_err(CliError::from)?;

    progress_task.abort();
    Ok(outcome)
}

fn parse_on_error(value: &str) -> Result<OnError, CliError> {
    match value.to_ascii_lowercase().as_str() {
        "stop" => Ok(OnError::Stop),
        "continue" => Ok(OnError::Continue),
        other => Err(CliError::usage(format!(
            "invalid --on-error {other:?}: expected stop or continue"
        ))),
    }
}

/// `--definer`: keep the clauses (default), strip them, or remap them to
/// CURRENT_USER. Only MySQL-family scripts have DEFINER clauses, so anything
/// but `keep` is refused elsewhere rather than silently ignored.
fn parse_definer(value: &str, dialect: Dialect) -> Result<Option<DefinerMode>, CliError> {
    let mode = match value.to_ascii_lowercase().as_str() {
        "keep" => return Ok(None),
        "strip" => DefinerMode::Strip,
        "current-user" => DefinerMode::CurrentUser,
        other => {
            return Err(CliError::usage(format!(
                "invalid --definer {other:?}: expected keep, strip or current-user"
            )));
        }
    };
    if dialect != Dialect::MySql {
        return Err(CliError::usage(
            "--definer strip/current-user applies only to MySQL and MariaDB connections",
        ));
    }
    Ok(Some(mode))
}

fn parse_transaction(value: &str) -> Result<TransactionMode, CliError> {
    match value.to_ascii_lowercase().as_str() {
        "none" => Ok(TransactionMode::None),
        "single" => Ok(TransactionMode::Single),
        other => Err(CliError::usage(format!(
            "invalid --transaction {other:?}: expected none or single"
        ))),
    }
}

fn format_progress(progress: &ScriptProgress) -> String {
    let pct = if progress.total_bytes == 0 {
        0
    } else {
        (progress.bytes_read.saturating_mul(100) / progress.total_bytes.max(1)) as u32
    };
    format!(
        "run-file: {}/{} statements, {}% bytes, line {}, {} rows affected",
        progress.units_done + progress.units_failed + progress.units_skipped,
        progress.units_done + progress.units_failed + progress.units_skipped,
        pct,
        progress.current_line,
        progress.rows_affected
    )
}

fn outcome_to_json(outcome: &ScriptOutcome) -> Value {
    serde_json::to_value(outcome).unwrap_or_else(|_| json!({}))
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into())
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_on_error_values() {
        assert_eq!(parse_on_error("stop").unwrap(), OnError::Stop);
        assert_eq!(parse_on_error("CONTINUE").unwrap(), OnError::Continue);
        assert!(parse_on_error("ask").is_err());
    }

    #[test]
    fn parse_definer_values() {
        assert_eq!(parse_definer("keep", Dialect::Postgres).unwrap(), None);
        assert_eq!(
            parse_definer("STRIP", Dialect::MySql).unwrap(),
            Some(DefinerMode::Strip)
        );
        assert_eq!(
            parse_definer("current-user", Dialect::MySql).unwrap(),
            Some(DefinerMode::CurrentUser)
        );
        assert!(parse_definer("strip", Dialect::Postgres).is_err());
        assert!(parse_definer("drop", Dialect::MySql).is_err());
    }

    #[test]
    fn parse_transaction_values() {
        assert_eq!(parse_transaction("none").unwrap(), TransactionMode::None);
        assert_eq!(
            parse_transaction("single").unwrap(),
            TransactionMode::Single
        );
        assert!(parse_transaction("auto").is_err());
    }
}
