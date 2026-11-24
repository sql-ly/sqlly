//! `sqlly notebook …`: run, validate, diff, history, schedules and exports
//! for `.sqllynb` notebooks (also `.sqlnb.json` and `.ipynb`), headless.

pub mod compare;
pub mod diff;
pub mod export;
pub mod params;
pub mod policy;
pub mod records;
pub mod remap;
pub mod runner;
pub mod schedule;
pub mod security;
pub mod select;
pub mod source;
pub mod specs;
pub mod textdiff;
pub mod validate;

#[cfg(test)]
mod e2e_tests;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use serde_json::Value;
use sqlly_services::notebook::run_record::RunTrigger;

use crate::agent::connections::{load_store, store_path};
use crate::agent::{CliError, ExitCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum NotebookFormat {
    #[default]
    Table,
    Json,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum NotebookAction {
    /// Run a notebook headlessly and record the run. Exit codes: 0 ok, 1 a
    /// cell failed, 2 usage, 3 refused by policy, 4 a check failed, 130
    /// interrupted.
    Run(runner::RunArgs),
    /// Check a notebook without running it: references, cycles,
    /// connections, policy, parameters and cell settings. For CI.
    Validate(validate::ValidateArgs),
    /// Compare two notebooks cell by cell (Git-friendly).
    Diff(diff::DiffArgs),
    /// Print a notebook as stable text (a Git `textconv` driver).
    Cat { file: PathBuf },
    /// List a notebook's runs, show one, or compare two.
    Runs(compare::RunsArgs),
    /// Run a notebook on this computer's scheduler, with alerts.
    Schedule {
        #[command(subcommand)]
        action: schedule::ScheduleAction,
    },
    /// Write a recorded run as a report.
    Export(export::ExportArgs),
}

/// Print a command's result: the JSON envelope (`--json` or
/// `--format json`, always carrying `data`) or the text.
pub fn finish(
    json: bool,
    format: NotebookFormat,
    text: String,
    data: Value,
    exit: ExitCode,
) -> ExitCode {
    if json || format == NotebookFormat::Json {
        let envelope = serde_json::json!({"ok": exit == ExitCode::Ok, "data": data});
        let rendered = if std::io::stdout().is_terminal() {
            serde_json::to_string_pretty(&envelope)
        } else {
            serde_json::to_string(&envelope)
        }
        .unwrap_or_default();
        let _ = writeln!(std::io::stdout(), "{rendered}");
    } else if !text.is_empty() {
        let _ = writeln!(std::io::stdout(), "{text}");
    }
    exit
}

fn runs_dir() -> records::RunsDir {
    records::RunsDir::new(records::notebooks_root())
}

async fn run_command(args: &runner::RunArgs, json: bool) -> ExitCode {
    let store = match load_store(&store_path()) {
        Ok(store) => store,
        Err(err) => return crate::agent::emit(json, None, Err(err)),
    };
    let env = runner::RunEnv {
        store,
        runs: runs_dir(),
        trigger: RunTrigger::Cli,
    };
    match runner::run(args, &env).await {
        Ok(report) => {
            let data = runner::report_json(&report);
            if let Some(out) = &args.out {
                let text = serde_json::to_string_pretty(&data).unwrap_or_default();
                if let Err(e) = std::fs::write(out, text + "\n") {
                    return crate::agent::emit(
                        json,
                        None,
                        Err(CliError::io(format!("cannot write {}: {e}", out.display()))),
                    );
                }
            }
            let exit = if report.dry_run {
                ExitCode::Ok
            } else {
                runner::exit_for(&report.summary, args.fail_on)
            };
            finish(json, args.format, runner::render_text(&report), data, exit)
        }
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

pub async fn dispatch(action: &NotebookAction, json: bool) -> ExitCode {
    match action {
        NotebookAction::Run(args) => run_command(args, json).await,
        NotebookAction::Validate(args) => match load_store(&store_path()) {
            Ok(store) => validate::command(args, &store, json),
            Err(err) => crate::agent::emit(json, None, Err(err)),
        },
        NotebookAction::Diff(args) => diff::command(args, json),
        NotebookAction::Cat { file } => diff::cat_command(file, json),
        NotebookAction::Runs(args) => compare::command(args, &runs_dir(), json),
        NotebookAction::Schedule { action } => schedule::command(action, &runs_dir(), json).await,
        NotebookAction::Export(args) => export::command(args, &runs_dir(), json),
    }
}
