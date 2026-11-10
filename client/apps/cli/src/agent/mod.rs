//! Agent-friendly command surface: stable JSON envelope, fixed exit codes,
//! and one print path. Everything here writes to stdout exactly once per
//! invocation; diagnostics go to stderr (the `engine` subcommand owns stdout
//! as a binary channel, so nothing in this module may be reached from it).

pub mod connections;
pub mod context;
pub mod doctor;
pub mod dump_database;
pub mod format;
pub mod query;
pub mod restore_file;
pub mod run_file;
pub mod schema;
pub mod transfer;

use serde::Serialize;
use serde_json::Value;
use std::io::{IsTerminal, Write};

/// Process exit codes. Part of the CLI's documented contract; do not renumber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    Ok = 0,
    Error = 1,
    Usage = 2,
    PolicyRefused = 3,
    Cancelled = 130,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Envelope<T: Serialize> {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

#[derive(Debug)]
pub struct CliError {
    pub exit: ExitCode,
    pub body: ErrorBody,
}

impl CliError {
    pub fn usage(msg: impl Into<String>) -> Self {
        Self {
            exit: ExitCode::Usage,
            body: ErrorBody {
                code: "usage",
                message: msg.into(),
                hint: None,
            },
        }
    }

    pub fn policy(msg: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            exit: ExitCode::PolicyRefused,
            body: ErrorBody {
                code: "policy_refused",
                message: msg.into(),
                hint,
            },
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::error_code("not_found", msg, None)
    }

    pub fn connection(msg: impl Into<String>) -> Self {
        Self::error_code("connection", msg, None)
    }

    pub fn sql(msg: impl Into<String>) -> Self {
        Self::error_code("sql", msg, None)
    }

    pub fn io(msg: impl Into<String>) -> Self {
        Self::error_code("io", msg, None)
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::error_code("internal", msg, None)
    }

    pub fn cancelled(msg: impl Into<String>) -> Self {
        Self::error_code("cancelled", msg, None)
    }

    fn error_code(code: &'static str, msg: impl Into<String>, hint: Option<String>) -> Self {
        Self {
            exit: ExitCode::Error,
            body: ErrorBody {
                code,
                message: msg.into(),
                hint,
            },
        }
    }
}

impl From<anyhow::Error> for CliError {
    fn from(err: anyhow::Error) -> Self {
        Self::internal(err.to_string())
    }
}

impl From<sqlly_core::StudioError> for CliError {
    fn from(err: sqlly_core::StudioError) -> Self {
        use sqlly_core::StudioError;
        match &err {
            StudioError::Authentication(msg) | StudioError::Connection(msg) => {
                Self::connection(msg.clone())
            }
            StudioError::ConnectionMessage(message) => {
                Self::connection(message.fallback().to_string())
            }
            StudioError::AuthenticationMessage(message) => {
                Self::connection(message.fallback().to_string())
            }
            StudioError::QueryExecution(_)
            | StudioError::QueryExecutionMessage(_)
            | StudioError::QueryTimeout(_)
            | StudioError::QueryCancelled => Self::sql(err.to_string()),
            StudioError::ObjectNotFound(msg) => Self::not_found(msg.clone()),
            StudioError::PermissionDenied(msg) => Self::error_code("connection", msg.clone(), None),
            StudioError::PermissionDeniedMessage(message) => {
                Self::error_code("connection", message.fallback().to_string(), None)
            }
            StudioError::InvalidProfile(msg) | StudioError::InvalidInput(msg) => {
                Self::usage(msg.clone())
            }
            StudioError::InvalidInputMessage(message) => {
                Self::usage(message.fallback().to_string())
            }
            StudioError::InvalidProfileMessage(message) => {
                Self::usage(message.fallback().to_string())
            }
            StudioError::CapabilityNotSupported(_)
            | StudioError::CapabilityNotSupportedMessage(_)
            | StudioError::NotImplemented(_)
            | StudioError::Serialization(_)
            | StudioError::Io(_)
            | StudioError::Other(_)
            | StudioError::Message(_) => Self::internal(err.to_string()),
        }
    }
}

/// Print agent output once and return the process exit code.
pub fn emit(json: bool, ok_text: Option<String>, result: Result<Value, CliError>) -> ExitCode {
    match result {
        Ok(data) => {
            if json {
                let envelope = Envelope {
                    ok: true,
                    data: Some(data),
                    error: None,
                };
                print_json(&envelope);
            } else if let Some(text) = ok_text {
                let _ = writeln!(std::io::stdout(), "{text}");
            }
            ExitCode::Ok
        }
        Err(err) => {
            if json {
                let envelope = Envelope::<Value> {
                    ok: false,
                    data: None,
                    error: Some(err.body),
                };
                print_json(&envelope);
            } else {
                let mut stderr = std::io::stderr();
                let _ = writeln!(stderr, "error: {}", err.body.message);
                if let Some(hint) = err.body.hint {
                    let _ = writeln!(stderr, "hint: {hint}");
                }
            }
            err.exit
        }
    }
}

/// Print raw text (e.g. jsonl) without an envelope; errors still honor `json`.
pub fn emit_raw(json: bool, ok_text: String, result: Result<(), CliError>) -> ExitCode {
    match result {
        Ok(()) => {
            let _ = writeln!(std::io::stdout(), "{ok_text}");
            ExitCode::Ok
        }
        Err(err) => emit(json, None, Err(err)),
    }
}

fn print_json<T: Serialize>(value: &T) {
    let pretty = std::io::stdout().is_terminal();
    let text = if pretty {
        serde_json::to_string_pretty(value).expect("serialize envelope")
    } else {
        serde_json::to_string(value).expect("serialize envelope")
    };
    let _ = writeln!(std::io::stdout(), "{text}");
}

/// Dispatch agent-facing commands. Returns `None` for legacy harness commands.
pub async fn dispatch_agent(cli: &crate::Cli) -> Option<ExitCode> {
    use crate::Commands::{
        Connections, Context, Doctor, DumpDatabase, Query, RestoreFile, RunFile, Schema, Transfer,
    };
    let code = match &cli.command {
        Doctor => doctor::run(cli.json).await,
        Connections { action } => match action {
            crate::AgentConnectionsAction::List { format } => {
                context::list_connections(*format, cli.json).await
            }
        },
        Query {
            connection,
            database,
            sql,
            file,
            format,
            limit,
            timeout,
            allow_writes,
        } => {
            query::run(
                query::QueryArgs {
                    connection: connection.clone(),
                    database: database.clone(),
                    sql: sql.clone(),
                    file: file.clone(),
                    format: *format,
                    limit: *limit,
                    timeout: *timeout,
                    allow_writes: *allow_writes,
                },
                cli.json,
            )
            .await
        }
        Schema { action } => match action {
            crate::AgentSchemaAction::List {
                connection,
                database,
                schema,
                kind,
                format,
            } => {
                schema::list(
                    schema::ListArgs {
                        connection: connection.clone(),
                        database: database.clone(),
                        schema: schema.clone(),
                        kind: *kind,
                        format: *format,
                    },
                    cli.json,
                )
                .await
            }
            crate::AgentSchemaAction::Describe {
                connection,
                object,
                database,
                format,
            } => {
                schema::describe(
                    schema::DescribeArgs {
                        connection: connection.clone(),
                        object: object.clone(),
                        database: database.clone(),
                        format: *format,
                    },
                    cli.json,
                )
                .await
            }
        },
        Context {
            connection,
            database,
            budget_tokens,
            purpose,
        } => {
            context::run(
                context::ContextArgs {
                    connection: connection.clone(),
                    database: database.clone(),
                    budget_tokens: *budget_tokens,
                    purpose: *purpose,
                },
                cli.json,
            )
            .await
        }
        Transfer {
            connection,
            database,
            target_profile,
            target_database,
            target_schema,
            mode,
            casing,
            table,
        } => {
            transfer::run(
                transfer::TransferArgs {
                    connection: connection.clone(),
                    database: database.clone(),
                    target_profile: target_profile.clone(),
                    target_database: target_database.clone(),
                    target_schema: target_schema.clone(),
                    mode: mode.clone(),
                    casing: casing.clone(),
                    tables: table.clone(),
                },
                cli.json,
            )
            .await
        }
        DumpDatabase {
            connection,
            database,
            output,
            content,
            per_object,
            drop_if_exists,
            batch_size,
            no_views,
            no_routines,
            no_triggers,
            no_sequences,
            schema,
            native,
            pg_format,
            include_owner,
            no_single_transaction,
            mysqldump_option,
            strip_definer,
            include_privileges,
        } => {
            dump_database::run(
                dump_database::DumpArgs {
                    connection: connection.clone(),
                    database: database.clone(),
                    output: output.clone(),
                    content: *content,
                    per_object: *per_object,
                    drop_if_exists: *drop_if_exists,
                    batch_size: *batch_size,
                    no_views: *no_views,
                    no_routines: *no_routines,
                    no_triggers: *no_triggers,
                    no_sequences: *no_sequences,
                    schema: schema.clone(),
                    native: *native,
                    pg_format: *pg_format,
                    include_owner: *include_owner,
                    no_single_transaction: *no_single_transaction,
                    mysqldump_option: mysqldump_option.clone(),
                    strip_definer: *strip_definer,
                    include_privileges: *include_privileges,
                },
                cli.json,
            )
            .await
        }
        RestoreFile {
            connection,
            database,
            input,
            clean,
            if_exists,
            single_transaction,
            no_owner,
        } => {
            restore_file::run(
                restore_file::RestoreArgs {
                    connection: connection.clone(),
                    database: database.clone(),
                    input: input.clone(),
                    clean: *clean,
                    if_exists: *if_exists,
                    single_transaction: *single_transaction,
                    no_owner: *no_owner,
                },
                cli.json,
            )
            .await
        }
        RunFile {
            connection,
            database,
            file,
            on_error,
            transaction,
            coalesce_inserts,
            definer,
        } => {
            run_file::run(
                run_file::RunFileArgs {
                    connection: connection.clone(),
                    database: database.clone(),
                    file: file.clone(),
                    on_error: on_error.clone(),
                    transaction: transaction.clone(),
                    coalesce_inserts: *coalesce_inserts,
                    definer: definer.clone(),
                },
                cli.json,
            )
            .await
        }
        _ => return None,
    };
    Some(code)
}
