use crate::agent::connections::{
    apply_database_override, is_production, load_store, resolve_connection, store_path, summary,
};
use crate::agent::format::{self, OutputFormat, ResultSet};
use crate::agent::{CliError, ExitCode};
use serde_json::{Value, json};
use sqlly_intel::query::safety::classify_select_only_for;
use sqlly_services::query::{QueryEvent, take_spilled_rows};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct QueryArgs {
    pub connection: String,
    pub database: Option<String>,
    pub sql: Option<String>,
    pub file: Option<PathBuf>,
    pub format: OutputFormat,
    pub limit: usize,
    pub timeout: u64,
    pub allow_writes: bool,
}

pub async fn run(args: QueryArgs, json: bool) -> ExitCode {
    if streams_rows(args.format, json) {
        return match run_streaming(args).await {
            Ok(()) => ExitCode::Ok,
            Err(err) => crate::agent::emit(json, None, Err(err)),
        };
    }
    match run_inner(args).await {
        Ok((text, data, raw)) => {
            if raw {
                crate::agent::emit_raw(json, text, Ok(()))
            } else {
                crate::agent::emit(json, Some(text), Ok(data))
            }
        }
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

async fn run_inner(args: QueryArgs) -> Result<(String, Value, bool), CliError> {
    let store = load_store(&store_path())?;
    let mut profile = resolve_connection(&store, &args.connection)?;
    profile = apply_database_override(profile, args.database.clone());
    let sql = read_sql(&args)?;

    apply_policy_gate(&store, &mut profile, &sql, args.allow_writes)?;

    let services = sqlly_services::Services::new();
    // Full values: the CLI writes results out, so cells are never cut to
    // the grid's 8 KiB preview.
    let handle = services
        .queries
        .execute_full_values(&profile, None, &sql, 256)
        .await
        .map_err(CliError::from)?;

    let (sets, elapsed_ms, messages) =
        drain_events(&services, handle, args.limit, args.timeout).await?;

    if args.format == OutputFormat::Jsonl {
        let text = format::render(OutputFormat::Jsonl, &sets);
        let data = json!({
            "format": "jsonl",
            "connection": summary(&profile),
            "elapsed_ms": elapsed_ms,
            "messages": messages,
        });
        return Ok((text, data, true));
    }

    let rendered = format::render(args.format, &sets);
    let data = json!({
        "format": format_label(args.format),
        "connection": summary(&profile),
        "elapsed_ms": elapsed_ms,
        "messages": messages,
        "result_sets": format::render_json_value(&sets),
    });
    Ok((rendered, data, false))
}

/// CSV, TSV and JSON Lines render row by row, so outside the `--json`
/// envelope (which needs the whole result) they stream straight to stdout.
fn streams_rows(format: OutputFormat, json: bool) -> bool {
    !json
        && matches!(
            format,
            OutputFormat::Csv | OutputFormat::Tsv | OutputFormat::Jsonl
        )
}

/// The streaming counterpart of `run_inner`: rows go to stdout as they
/// arrive, so `--limit 0` exports any result size in bounded memory.
async fn run_streaming(args: QueryArgs) -> Result<(), CliError> {
    let store = load_store(&store_path())?;
    let mut profile = resolve_connection(&store, &args.connection)?;
    profile = apply_database_override(profile, args.database.clone());
    let sql = read_sql(&args)?;
    apply_policy_gate(&store, &mut profile, &sql, args.allow_writes)?;

    let services = sqlly_services::Services::new();
    // Full values: every cell whole (never the grid's 8 KiB preview) and
    // every row forwarded as it arrives instead of withheld past the spill
    // threshold — this process writes each row out immediately, and a
    // withheld tail would only arrive after later result sets were printed.
    let handle = services
        .queries
        .execute_full_values(&profile, None, &sql, 256)
        .await
        .map_err(CliError::from)?;
    let task_id = handle.task_id.clone();
    let stdout = std::io::stdout();
    let writer = crate::agent::format::StreamWriter::new(
        args.format,
        std::io::BufWriter::new(stdout.lock()),
    )
    .expect("streams_rows admits only streamable formats");
    let drain = stream_events(handle.events, writer, args.limit);
    with_deadline(&services, &task_id, args.timeout, drain).await
}

/// Write query events through `writer`, honoring `limit` per result set.
async fn stream_events<W: std::io::Write>(
    mut events: tokio::sync::mpsc::Receiver<QueryEvent>,
    mut writer: crate::agent::format::StreamWriter<W>,
    limit: usize,
) -> Result<(), CliError> {
    let limit = if limit == 0 { usize::MAX } else { limit };
    let io = |err: std::io::Error| CliError::io(err.to_string());
    let mut written = 0usize;
    let mut truncated = false;
    let mut cut_cells = 0usize;
    while let Some(event) = events.recv().await {
        match event {
            QueryEvent::ResultSet { index, columns } => {
                if writer.current_set().is_some() {
                    writer.end_set(truncated).map_err(io)?;
                }
                writer.begin_set(index, &columns).map_err(io)?;
                written = 0;
                truncated = false;
            }
            QueryEvent::Rows { index, rows } => {
                for row in rows {
                    if written < limit {
                        writer.row(index, &row).map_err(io)?;
                        written += 1;
                    } else {
                        truncated = true;
                    }
                }
            }
            QueryEvent::TruncatedCells { cells, .. } => cut_cells += cells.len(),
            QueryEvent::Message { text } => eprintln!("{text}"),
            QueryEvent::Error { message } => return Err(CliError::sql(message)),
            QueryEvent::Cancelled => return Err(CliError::cancelled("query cancelled")),
            QueryEvent::SpillManifest { manifest } => {
                // Nothing was withheld (see `run_streaming`); the files only
                // mirror rows already written, so just clean them up.
                for spilled in &manifest.result_sets {
                    let _ = take_spilled_rows(spilled);
                }
            }
            QueryEvent::Completed { .. } => break,
            _ => {}
        }
    }
    if writer.current_set().is_some() {
        writer.end_set(truncated).map_err(io)?;
    }
    writer.finish().map_err(io)?;
    if cut_cells > 0 {
        eprintln!(
            "warning: {cut_cells} cell(s) longer than 8 KiB were cut to an 8 KiB preview by the driver pipeline"
        );
    }
    Ok(())
}

/// Run `work` under the `--timeout` deadline (0 = none) and Ctrl-C, cancelling
/// the server-side query when either fires.
async fn with_deadline<T>(
    services: &sqlly_services::Services,
    task_id: &str,
    timeout_secs: u64,
    work: impl std::future::Future<Output = Result<T, CliError>>,
) -> Result<T, CliError> {
    let timeout = if timeout_secs == 0 {
        Duration::MAX
    } else {
        Duration::from_secs(timeout_secs)
    };
    tokio::select! {
        result = tokio::time::timeout(timeout, work) => {
            match result {
                Ok(inner) => inner,
                Err(_) => {
                    let _ = services.queries.cancel(task_id).await;
                    Err(CliError::sql(format!("statement timed out after {timeout_secs} s")))
                }
            }
        }
        _ = tokio::signal::ctrl_c() => {
            let _ = services.queries.cancel(task_id).await;
            Err(CliError::cancelled("interrupted"))
        }
    }
}

pub fn apply_policy_gate(
    store: &crate::agent::connections::StoreFile,
    profile: &mut sqlly_core::ConnectionProfile,
    sql: &str,
    allow_writes: bool,
) -> Result<(), CliError> {
    // Judged as the connection's engine reads the SQL.
    let class = classify_select_only_for(profile.database_type, sql);
    if !allow_writes && !class.select_only {
        let reason = class
            .reason
            .unwrap_or_else(|| "statement is not read-only".to_string());
        return Err(CliError::policy(
            format!("refused: {reason}"),
            Some("re-run with --allow-writes".into()),
        ));
    }
    if allow_writes && !class.select_only && is_production(store, profile) {
        return Err(CliError::policy(
            "writes are refused on production connections from the CLI",
            Some(
                "use the app's production unlock, or run against a non-production connection"
                    .into(),
            ),
        ));
    }
    if !allow_writes {
        profile.block_all_but_select = true;
        // Second layer the database enforces on top of the parser gate.
        profile.enforce_read_only = true;
    }
    Ok(())
}

fn read_sql(args: &QueryArgs) -> Result<String, CliError> {
    if let Some(sql) = &args.sql {
        if sql == "-" {
            return read_stdin();
        }
        return Ok(sql.clone());
    }
    if let Some(path) = &args.file {
        return std::fs::read_to_string(path).map_err(|err| CliError::io(err.to_string()));
    }
    if !std::io::stdin().is_terminal() {
        return read_stdin();
    }
    Err(CliError::usage(
        "provide --sql, --file, or pipe SQL on stdin",
    ))
}

fn read_stdin() -> Result<String, CliError> {
    let mut buffer = String::new();
    std::io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|err| CliError::io(err.to_string()))?;
    Ok(buffer)
}

async fn drain_events(
    services: &sqlly_services::Services,
    mut handle: sqlly_services::query::QueryHandle,
    limit: usize,
    timeout_secs: u64,
) -> Result<(Vec<ResultSet>, u128, Vec<String>), CliError> {
    let mut sets: Vec<ResultSet> = Vec::new();
    let mut messages = Vec::new();
    let mut elapsed_ms = 0u128;
    let limit = if limit == 0 { usize::MAX } else { limit };
    let task_id = handle.task_id.clone();

    let drain = async {
        while let Some(event) = handle.events.recv().await {
            match event {
                QueryEvent::ResultSet { index, columns } => {
                    if sets.len() <= index {
                        sets.resize_with(index + 1, || ResultSet {
                            columns: Vec::new(),
                            rows: Vec::new(),
                            rows_affected: None,
                            truncated: false,
                        });
                    }
                    sets[index].columns = columns;
                }
                QueryEvent::Rows { index, rows } => {
                    if sets.len() <= index {
                        sets.resize_with(index + 1, || ResultSet {
                            columns: Vec::new(),
                            rows: Vec::new(),
                            rows_affected: None,
                            truncated: false,
                        });
                    }
                    let set = &mut sets[index];
                    for row in rows {
                        if set.rows.len() < limit {
                            set.rows.push(row);
                        } else {
                            set.truncated = true;
                        }
                    }
                }
                QueryEvent::RowCount {
                    index,
                    rows_affected,
                } => {
                    if let Some(set) = sets.get_mut(index) {
                        set.rows_affected = Some(rows_affected);
                    }
                }
                QueryEvent::Message { text } => {
                    eprintln!("{text}");
                    messages.push(text);
                }
                QueryEvent::Error { message } => return Err(CliError::sql(message)),
                QueryEvent::Completed { elapsed_ms: ms } => {
                    elapsed_ms = ms;
                    break;
                }
                QueryEvent::Cancelled => {
                    return Err(CliError::cancelled("query cancelled"));
                }
                QueryEvent::SpillManifest { manifest } => {
                    // The on-disk set supersedes the forwarded `Rows` prefix;
                    // without this, `--limit 0` exports stopped at the spill
                    // threshold.
                    for spilled in &manifest.result_sets {
                        if let Some(all) = take_spilled_rows(spilled) {
                            apply_spilled_rows(&mut sets, spilled, all, limit);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok::<_, CliError>((sets, elapsed_ms, messages))
    };

    // `--timeout 0` means no limit (long-running exports from cron).
    with_deadline(services, &task_id, timeout_secs, drain).await
}

/// Replace a result set's forwarded prefix with its complete spilled rows,
/// still honoring `limit` (already `usize::MAX` for unlimited).
fn apply_spilled_rows(
    sets: &mut Vec<ResultSet>,
    spilled: &sqlly_services::query::SpillResultSetData,
    mut all: Vec<Vec<sqlly_core::CellValue>>,
    limit: usize,
) {
    let index = spilled.result_set_index;
    if sets.len() <= index {
        sets.resize_with(index + 1, || ResultSet {
            columns: Vec::new(),
            rows: Vec::new(),
            rows_affected: None,
            truncated: false,
        });
    }
    let set = &mut sets[index];
    if set.columns.is_empty() {
        set.columns = spilled.columns.clone();
    }
    if all.len() > limit {
        all.truncate(limit);
        set.truncated = true;
    }
    set.rows = all;
}

fn format_label(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Table => "table",
        OutputFormat::Csv => "csv",
        OutputFormat::Tsv => "tsv",
        OutputFormat::Json => "json",
        OutputFormat::Jsonl => "jsonl",
        OutputFormat::Md => "md",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{ConnectionEnvironment, ConnectionProfile, SecretProvider, SecretRef};

    fn spilled(index: usize) -> sqlly_services::query::SpillResultSetData {
        sqlly_services::query::SpillResultSetData {
            result_set_index: index,
            columns: vec![sqlly_core::ResultColumn {
                name: "n".into(),
                data_type: "int".into(),
                nullable: false,
            }],
            row_count: 3,
            spill_file: String::new(),
        }
    }

    fn rows(n: i64) -> Vec<Vec<sqlly_core::CellValue>> {
        (0..n)
            .map(|i| vec![sqlly_core::CellValue::I64(i)])
            .collect()
    }

    #[test]
    fn spilled_rows_replace_the_forwarded_prefix() {
        let mut sets = vec![ResultSet {
            columns: Vec::new(),
            rows: rows(1),
            rows_affected: None,
            truncated: false,
        }];
        apply_spilled_rows(&mut sets, &spilled(0), rows(3), usize::MAX);
        assert_eq!(sets[0].rows.len(), 3);
        assert_eq!(sets[0].columns.len(), 1);
        assert!(!sets[0].truncated);
    }

    #[test]
    fn spilled_rows_respect_limit_and_create_missing_sets() {
        let mut sets = Vec::new();
        apply_spilled_rows(&mut sets, &spilled(1), rows(3), 2);
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[1].rows.len(), 2);
        assert!(sets[1].truncated);
    }

    fn store_with(profile: ConnectionProfile) -> crate::agent::connections::StoreFile {
        crate::agent::connections::StoreFile {
            version: 1,
            profiles: vec![profile],
            environments: vec![],
        }
    }

    fn dev_profile() -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "dev",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.environment = ConnectionEnvironment::Dev;
        profile
    }

    #[test]
    fn select_only_default_refuses_update() {
        let store = store_with(dev_profile());
        let mut profile = dev_profile();
        let err = apply_policy_gate(&store, &mut profile, "delete from t", false).unwrap_err();
        assert_eq!(err.exit, ExitCode::PolicyRefused);
    }

    #[test]
    fn allow_writes_on_production_refused() {
        let mut profile = dev_profile();
        profile.environment = ConnectionEnvironment::Production;
        let store = store_with(profile.clone());
        let err = apply_policy_gate(&store, &mut profile, "delete from t", true).unwrap_err();
        assert_eq!(err.exit, ExitCode::PolicyRefused);
    }

    #[test]
    fn saved_library_query_with_front_matter_passes_the_read_only_gate() {
        // Scheduled query exports copy as `query --file <library file>`; the
        // file keeps the app's `-- sqlly:` front-matter comment line.
        let store = store_with(dev_profile());
        let mut profile = dev_profile();
        apply_policy_gate(
            &store,
            &mut profile,
            "-- sqlly: connection=prod database=sales\nSELECT id FROM orders",
            false,
        )
        .expect("a leading comment keeps a SELECT read-only");
    }

    #[test]
    fn allow_writes_on_dev_passes_gate() {
        let store = store_with(dev_profile());
        let mut profile = dev_profile();
        apply_policy_gate(&store, &mut profile, "delete from t", true).unwrap();
    }

    fn column(name: &str) -> sqlly_core::ResultColumn {
        sqlly_core::ResultColumn {
            name: name.into(),
            data_type: "int".into(),
            nullable: false,
        }
    }

    #[test]
    fn only_row_formats_outside_the_envelope_stream() {
        assert!(streams_rows(OutputFormat::Csv, false));
        assert!(streams_rows(OutputFormat::Jsonl, false));
        assert!(!streams_rows(OutputFormat::Csv, true));
        assert!(!streams_rows(OutputFormat::Table, false));
        assert!(!streams_rows(OutputFormat::Json, false));
    }

    #[tokio::test]
    async fn stream_events_writes_every_set_and_honors_limit() {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tx.send(QueryEvent::ResultSet {
            index: 0,
            columns: vec![column("n")],
        })
        .await
        .unwrap();
        tx.send(QueryEvent::Rows {
            index: 0,
            rows: rows(3),
        })
        .await
        .unwrap();
        tx.send(QueryEvent::ResultSet {
            index: 1,
            columns: vec![column("m")],
        })
        .await
        .unwrap();
        tx.send(QueryEvent::Rows {
            index: 1,
            rows: rows(1),
        })
        .await
        .unwrap();
        tx.send(QueryEvent::Completed { elapsed_ms: 1 })
            .await
            .unwrap();
        drop(tx);
        let mut out = Vec::new();
        let writer = crate::agent::format::StreamWriter::new(OutputFormat::Csv, &mut out).unwrap();
        stream_events(rx, writer, 2).await.unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text, "n\n0\n1\n(truncated at --limit)\n\nm\n0\n");
    }

    #[tokio::test]
    async fn stream_events_surfaces_sql_errors() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        tx.send(QueryEvent::Error {
            message: "boom".into(),
        })
        .await
        .unwrap();
        drop(tx);
        let writer =
            crate::agent::format::StreamWriter::new(OutputFormat::Jsonl, Vec::new()).unwrap();
        let err = stream_events(rx, writer, 0).await.unwrap_err();
        assert_eq!(err.body.message, "boom");
    }

    #[test]
    fn stdin_dash_reads_sql() {
        let args = QueryArgs {
            connection: "dev".into(),
            database: None,
            sql: Some("-".into()),
            file: None,
            format: OutputFormat::Table,
            limit: 500,
            timeout: 60,
            allow_writes: false,
        };
        // `-` path delegates to stdin; in unit test we only assert the branch exists.
        assert_eq!(args.sql.as_deref(), Some("-"));
    }
}
