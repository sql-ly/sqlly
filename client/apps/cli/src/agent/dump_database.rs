use crate::agent::connections::{
    apply_database_override, effective_database, is_production, load_store, resolve_connection,
    store_path,
};
use crate::agent::{CliError, ExitCode};
use crate::commands::{DumpContentArg, PgFormatArg, dump_options_from_args};
use serde_json::{Value, json};
use sqlly_services::{DumpPhase, DumpProgress, DumpReport, Services, run_database_dump};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct DumpArgs {
    pub connection: String,
    pub database: String,
    pub output: PathBuf,
    pub content: DumpContentArg,
    pub per_object: bool,
    pub drop_if_exists: bool,
    pub batch_size: usize,
    pub no_views: bool,
    pub no_routines: bool,
    pub no_triggers: bool,
    pub no_sequences: bool,
    pub schema: Vec<String>,
    pub native: bool,
    pub pg_format: PgFormatArg,
    pub include_owner: bool,
    pub no_single_transaction: bool,
    pub mysqldump_option: Vec<String>,
    pub strip_definer: bool,
    pub include_privileges: bool,
}

pub async fn run(args: DumpArgs, json: bool) -> ExitCode {
    match run_inner(args).await {
        Ok(report) => finish_report(json, report),
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

fn finish_report(json: bool, report: DumpReport) -> ExitCode {
    let data = report_to_json(&report);
    if !json && !report.cancelled {
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
        return crate::agent::emit(false, Some(report_summary(&report)), Ok(data));
    }
    if report.cancelled {
        if json {
            print_json(&json!({
                "ok": false,
                "data": data,
                "error": {
                    "code": "cancelled",
                    "message": "dump cancelled",
                },
            }));
        } else {
            print_json(&data);
        }
        return ExitCode::Cancelled;
    }
    crate::agent::emit(json, None, Ok(data))
}

/// One-line human summary printed on success in text mode.
fn report_summary(report: &DumpReport) -> String {
    let target = match report.files.as_slice() {
        [] => "no files".to_string(),
        [one] => one.display().to_string(),
        [first, rest @ ..] => format!("{} (+{} more files)", first.display(), rest.len()),
    };
    format!(
        "Dumped {} rows, {} bytes in {} ms to {target}",
        report.rows_written, report.bytes_written, report.elapsed_ms
    )
}

/// `--native` hands the whole dump to pg_dump / mysqldump, which write one
/// file and have no batch size, so those options are refused rather than
/// silently dropped.
fn refuse_native_only_conflicts(args: &DumpArgs) -> Result<(), CliError> {
    if !args.native {
        if !args.mysqldump_option.is_empty() || args.strip_definer {
            return Err(CliError::usage(
                "--mysqldump-option and --strip-definer need --native: they apply to mysqldump",
            ));
        }
        return Ok(());
    }
    sqlly_services::validate_mysqldump_extra_args(&args.mysqldump_option)
        .map_err(|err| CliError::usage(err.into_fallback()))?;
    if args.per_object {
        return Err(CliError::usage(
            "--per-object is not available with --native: pg_dump and mysqldump write one file",
        ));
    }
    if args.batch_size != 500 {
        return Err(CliError::usage(
            "--batch-size is not available with --native: the vendor tool batches rows itself",
        ));
    }
    Ok(())
}

/// Options the chosen vendor tool has no switch for. They are reported on
/// stderr (the dump still runs) so a script never believes they applied.
fn native_ignored_options(
    args: &DumpArgs,
    kind: sqlly_lang::sqlmodel::ConnectionKind,
) -> Vec<&'static str> {
    use sqlly_lang::sqlmodel::ConnectionKind;
    let mut ignored = Vec::new();
    if args.no_views {
        ignored.push("--no-views");
    }
    if args.no_sequences {
        ignored.push("--no-sequences");
    }
    match kind {
        ConnectionKind::PostgresWire => {
            if args.no_routines {
                ignored.push("--no-routines");
            }
            if args.no_triggers {
                ignored.push("--no-triggers");
            }
            if args.no_single_transaction {
                ignored.push("--no-single-transaction");
            }
            if !args.mysqldump_option.is_empty() {
                ignored.push("--mysqldump-option");
            }
            if args.strip_definer {
                ignored.push("--strip-definer");
            }
        }
        ConnectionKind::MySqlWire => {
            if !args.schema.is_empty() {
                ignored.push("--schema");
            }
            if args.include_privileges {
                ignored.push("--include-privileges");
            }
            if args.include_owner {
                ignored.push("--include-owner");
            }
        }
        _ => {}
    }
    ignored
}

async fn run_inner(args: DumpArgs) -> Result<DumpReport, CliError> {
    refuse_native_only_conflicts(&args)?;
    let store = load_store(&store_path())?;
    let mut profile = resolve_connection(&store, &args.connection)?;
    profile = apply_database_override(profile, Some(args.database.clone()));
    let database = effective_database(&profile)?;

    if is_production(&store, &profile) {
        return Err(CliError::policy(
            "database dumps are refused on production connections from the CLI",
            Some(
                "use the app's production unlock, or run against a non-production connection"
                    .into(),
            ),
        ));
    }

    let mut opts = dump_options_from_args(
        args.content,
        args.per_object,
        args.drop_if_exists,
        args.batch_size,
        args.no_views,
        args.no_routines,
        args.no_triggers,
        args.no_sequences,
        args.schema.clone(),
    );
    opts.include_privileges = args.include_privileges;

    let services = Services::new();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_for_signal.store(true, Ordering::Relaxed);
        }
    });

    if args.native {
        let tool = native_dump_tool(&profile).await?;
        let ignored = native_ignored_options(&args, profile.database_type.connection_kind());
        if !ignored.is_empty() {
            eprintln!(
                "warning: {} has no equivalent for {}; ignored",
                tool.tool.binary_name(),
                ignored.join(", ")
            );
        }
        let req = sqlly_services::NativeDumpRequest {
            database: database.clone(),
            output: args.output.clone(),
            options: opts,
            tables: None,
            pg_format: args.pg_format.to_pg_format(),
            pg_no_owner: !args.include_owner,
            mysql_single_transaction: !args.no_single_transaction,
            mysql_extra_args: args.mysqldump_option.clone(),
            mysql_strip_definer: args.strip_definer,
        };
        let mut last_progress = Instant::now() - Duration::from_secs(1);
        let report = sqlly_services::run_native_dump(
            &services,
            &profile,
            &tool,
            &req,
            cancel,
            &mut |progress| {
                let now = Instant::now();
                if now.duration_since(last_progress) >= Duration::from_secs(1) {
                    eprintln!("{}", format_progress(&progress));
                    last_progress = now;
                }
            },
        )
        .await
        .map_err(CliError::from)?;
        return Ok(report);
    }

    let mut last_progress = Instant::now() - Duration::from_secs(1);
    let report = run_database_dump(
        &services,
        &profile,
        &database,
        &args.output,
        &opts,
        cancel,
        &mut |progress| {
            let now = Instant::now();
            if now.duration_since(last_progress) >= Duration::from_secs(1) {
                eprintln!("{}", format_progress(&progress));
                last_progress = now;
            }
        },
    )
    .await
    .map_err(CliError::from)?;

    Ok(report)
}

/// The vendor dump tool for this connection's engine, located on PATH or in
/// the well-known install dirs (the CLI has no per-tool override store).
async fn native_dump_tool(
    profile: &sqlly_core::ConnectionProfile,
) -> Result<sqlly_services::ToolLocation, CliError> {
    use sqlly_lang::sqlmodel::ConnectionKind;
    let tool_kind = match profile.database_type.connection_kind() {
        ConnectionKind::PostgresWire => sqlly_services::NativeTool::PgDump,
        ConnectionKind::MySqlWire => sqlly_services::NativeTool::MysqlDump,
        _ => {
            return Err(CliError::policy(
                format!(
                    "native dumps need PostgreSQL or MySQL/MariaDB — {} has no vendor dump tool",
                    profile.database_type.display_name()
                ),
                Some("drop --native for the portable SQL dump".into()),
            ));
        }
    };
    let locations = sqlly_services::locate_all(&std::collections::HashMap::new()).await;
    sqlly_services::find_tool(&locations, tool_kind)
        .cloned()
        .ok_or_else(|| {
            CliError::not_found(format!(
                "{} not found on PATH or in the usual install locations",
                tool_kind.binary_name()
            ))
        })
}

fn format_progress(progress: &DumpProgress) -> String {
    let phase = match progress.phase {
        DumpPhase::Enumerating => "enumerating",
        DumpPhase::Structure => "structure",
        DumpPhase::Data => "data",
        DumpPhase::Finishing => "finishing",
    };
    if progress.object.is_empty() {
        format!(
            "dump: {phase} {}/{} rows={}",
            progress.objects_done, progress.objects_total, progress.rows_written
        )
    } else {
        format!(
            "dump: {phase} {} {}/{} rows={}",
            progress.object, progress.objects_done, progress.objects_total, progress.rows_written
        )
    }
}

fn report_to_json(report: &DumpReport) -> Value {
    json!({
        "files": report
            .files
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>(),
        "rows_written": report.rows_written,
        "bytes_written": report.bytes_written,
        "warnings": report.warnings.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "elapsed_ms": report.elapsed_ms,
        "cancelled": report.cancelled,
    })
}

fn print_json<T: serde::Serialize>(value: &T) {
    use std::io::{IsTerminal, Write};
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

    fn args(native: bool) -> DumpArgs {
        DumpArgs {
            connection: "dev".into(),
            database: "sales".into(),
            output: PathBuf::from("out.sql"),
            content: DumpContentArg::StructureAndData,
            per_object: false,
            drop_if_exists: false,
            batch_size: 500,
            no_views: false,
            no_routines: false,
            no_triggers: false,
            no_sequences: false,
            schema: Vec::new(),
            native,
            pg_format: PgFormatArg::Custom,
            include_owner: false,
            no_single_transaction: false,
            mysqldump_option: Vec::new(),
            strip_definer: false,
            include_privileges: false,
        }
    }

    #[test]
    fn mysqldump_options_need_native_and_pass_validation() {
        let builtin = DumpArgs {
            strip_definer: true,
            ..args(false)
        };
        assert_eq!(
            refuse_native_only_conflicts(&builtin).unwrap_err().exit,
            ExitCode::Usage
        );
        let reserved = DumpArgs {
            mysqldump_option: vec!["--result-file=/etc/x".into()],
            ..args(true)
        };
        assert_eq!(
            refuse_native_only_conflicts(&reserved).unwrap_err().exit,
            ExitCode::Usage
        );
        let fine = DumpArgs {
            mysqldump_option: vec!["--where=id > 5".into(), "--skip-comments".into()],
            strip_definer: true,
            ..args(true)
        };
        assert!(refuse_native_only_conflicts(&fine).is_ok());
    }

    #[test]
    fn native_refuses_per_object_and_batch_size() {
        assert!(refuse_native_only_conflicts(&args(true)).is_ok());
        let per_object = DumpArgs {
            per_object: true,
            ..args(true)
        };
        let err = refuse_native_only_conflicts(&per_object).unwrap_err();
        assert_eq!(err.exit, ExitCode::Usage);
        let batch = DumpArgs {
            batch_size: 50,
            ..args(true)
        };
        assert_eq!(
            refuse_native_only_conflicts(&batch).unwrap_err().exit,
            ExitCode::Usage
        );
        // The built-in dump takes both.
        let builtin = DumpArgs {
            per_object: true,
            batch_size: 50,
            ..args(false)
        };
        assert!(refuse_native_only_conflicts(&builtin).is_ok());
    }

    #[test]
    fn native_ignored_options_are_engine_specific() {
        use sqlly_lang::sqlmodel::ConnectionKind;
        let a = DumpArgs {
            no_triggers: true,
            no_routines: true,
            schema: vec!["public".into()],
            ..args(true)
        };
        // mysqldump honors routines/triggers but has no schema filter.
        assert_eq!(
            native_ignored_options(&a, ConnectionKind::MySqlWire),
            vec!["--schema"]
        );
        // pg_dump honors the schema filter but not routine/trigger toggles.
        assert_eq!(
            native_ignored_options(&a, ConnectionKind::PostgresWire),
            vec!["--no-routines", "--no-triggers"]
        );
        // pg_dump writes privileges itself; mysqldump has nothing to match.
        let p = DumpArgs {
            include_privileges: true,
            ..args(true)
        };
        assert!(native_ignored_options(&p, ConnectionKind::PostgresWire).is_empty());
        assert_eq!(
            native_ignored_options(&p, ConnectionKind::MySqlWire),
            vec!["--include-privileges"]
        );
    }

    #[test]
    fn text_summary_names_rows_and_output() {
        let report = DumpReport {
            files: vec![PathBuf::from("out.sql")],
            rows_written: 12,
            bytes_written: 340,
            warnings: Vec::new(),
            elapsed_ms: 5,
            cancelled: false,
        };
        let text = report_summary(&report);
        assert!(text.contains("12 rows"), "{text}");
        assert!(text.contains("out.sql"), "{text}");
    }

    #[test]
    fn production_connection_refused() {
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
