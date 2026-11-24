//! `sqlly notebook schedule`: notebook runs on this machine's own scheduler
//! (launchd, a systemd user timer or crontab, Task Scheduler).
//!
//! Jobs live in `<notebooks data dir>/schedules.json` (written atomically,
//! owner-only); each job's run state (consecutive failures, the circuit
//! breaker, which alerts are firing, when it was last evaluated) lives in
//! `schedule-state/<job id>.json`. The scheduler starts
//! `sqlly notebook schedule run-now <job> --scheduled`, or the one-minute
//! `schedule tick` where it cannot express the job's times. Scheduled runs
//! never write data, and run only while this computer is on: there is no
//! cloud runner.

pub mod alerts;
pub mod cron;
pub mod install;
pub mod render;

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlly_services::notebook::run_record::RunTrigger;

use self::alerts::{AlertConfig, AlertState, NotifyTarget, QuietHours, Severity};
use self::cron::Cron;
use self::install::Backend;
use super::NotebookFormat;
use super::records::{RunsDir, write_private};
use super::runner::{self, FailOn, RunArgs, RunEnv};
use crate::agent::connections::{load_store, store_path};
use crate::agent::{CliError, ExitCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Extra attempts after a failed run.
    pub attempts: u32,
    /// Seconds before the first retry; doubles each time, capped at an hour.
    pub backoff_seconds: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 2,
            backoff_seconds: 60,
        }
    }
}

/// The waits before each retry.
pub fn retry_delays(policy: RetryPolicy) -> Vec<Duration> {
    (0..policy.attempts)
        .map(|i| {
            let secs = policy
                .backoff_seconds
                .saturating_mul(1u64 << i.min(20))
                .min(3600);
            Duration::from_secs(secs)
        })
        .collect()
}

/// Only runs that hit errors are retried: a failed check, a policy refusal
/// or a usage problem will not change by trying again.
pub fn should_retry(exit: ExitCode) -> bool {
    exit == ExitCode::Error
}

fn default_breaker() -> u32 {
    5
}

fn default_retain() -> usize {
    20
}

fn default_timeout() -> u64 {
    300
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    /// The notebook's absolute path.
    pub notebook: PathBuf,
    /// The notebook's title when the job was added (for messages).
    #[serde(default)]
    pub title: String,
    pub cron: String,
    /// IANA zone; `None` is the machine's own zone.
    #[serde(default)]
    pub tz: Option<String>,
    #[serde(default)]
    pub param_set: Option<PathBuf>,
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default)]
    pub connections: Vec<String>,
    #[serde(default)]
    pub cells: Option<String>,
    #[serde(default)]
    pub allow_production: bool,
    #[serde(default = "default_retain")]
    pub retain: usize,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub notify: Vec<NotifyTarget>,
    #[serde(default)]
    pub alerts: AlertConfig,
    #[serde(default)]
    pub retry: RetryPolicy,
    /// Consecutive failed runs that pause the job.
    #[serde(default = "default_breaker")]
    pub breaker_after: u32,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub backend: Option<Backend>,
}

impl Job {
    pub fn cron(&self) -> Result<Cron, CliError> {
        Cron::parse(&self.cron).map_err(|e| CliError::usage(format!("job {}: {e}", self.id)))
    }

    /// The next `n` runs after `after`, in the job's zone.
    pub fn upcoming(&self, after: DateTime<Utc>, n: usize) -> Vec<DateTime<Utc>> {
        let Ok(cron) = self.cron() else {
            return Vec::new();
        };
        match self.tz.as_deref().map(str::parse::<chrono_tz::Tz>) {
            Some(Ok(tz)) => cron.upcoming(&tz, after, n),
            Some(Err(_)) => Vec::new(),
            None => cron.upcoming(&chrono::Local, after, n),
        }
    }

    /// The next run after `after`, in the job's zone.
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let cron = Cron::parse(&self.cron).ok()?;
        match &self.tz {
            Some(tz) => cron.next_after(&tz.parse::<chrono_tz::Tz>().ok()?, after),
            None => cron.next_after(&chrono::Local, after),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobsFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub jobs: Vec<Job>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobState {
    #[serde(default)]
    pub consecutive_failures: u32,
    #[serde(default)]
    pub circuit_open: bool,
    #[serde(default)]
    pub last_run_id: Option<String>,
    #[serde(default)]
    pub last_run_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_exit: Option<i32>,
    /// Due times up to here have been handled (for `tick`).
    #[serde(default)]
    pub evaluated_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub alerts: AlertState,
}

/// Whether a run may start: a scheduled run waits while the breaker is
/// open; a run asked for by hand always goes (and closes it on success).
pub fn breaker_allows(state: &JobState, manual: bool) -> bool {
    manual || !state.circuit_open
}

/// The state after a run; `true` when this run tripped the breaker.
pub fn record_outcome(state: &JobState, ok: bool, breaker_after: u32) -> (JobState, bool) {
    let mut next = state.clone();
    if ok {
        next.consecutive_failures = 0;
        next.circuit_open = false;
        return (next, false);
    }
    next.consecutive_failures = state.consecutive_failures.saturating_add(1);
    let trip =
        breaker_after > 0 && next.consecutive_failures >= breaker_after && !state.circuit_open;
    if trip {
        next.circuit_open = true;
    }
    (next, trip)
}

/// Whether a due time fell in `(since, now]`.
pub fn is_due(job: &Job, state: &JobState, now: DateTime<Utc>) -> bool {
    let since = state.evaluated_until.unwrap_or(job.created_at);
    job.next_after(since).is_some_and(|next| next <= now)
}

pub struct ScheduleStore {
    pub root: PathBuf,
}

impl ScheduleStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn jobs_path(&self) -> PathBuf {
        self.root.join("schedules.json")
    }

    fn state_path(&self, job_id: &str) -> PathBuf {
        self.root
            .join("schedule-state")
            .join(format!("{}.json", super::records::notebook_stem(job_id)))
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    pub fn load(&self) -> Result<JobsFile, CliError> {
        match std::fs::read_to_string(self.jobs_path()) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| CliError::io(format!("schedules.json is not readable: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(JobsFile {
                version: 1,
                jobs: Vec::new(),
            }),
            Err(e) => Err(CliError::io(e.to_string())),
        }
    }

    /// Write `file`. Webhook URLs go to the credential store; the file keeps
    /// only their account names.
    pub fn save(&self, file: &JobsFile) -> Result<(), CliError> {
        let mut file = file.clone();
        for job in &mut file.jobs {
            for (index, target) in job.notify.iter_mut().enumerate() {
                *target = alerts::store_webhook(target, alerts::webhook_account(&job.id, index))
                    .map_err(CliError::io)?;
            }
        }
        let text =
            serde_json::to_string_pretty(&file).map_err(|e| CliError::internal(e.to_string()))?;
        write_private(&self.jobs_path(), format!("{text}\n").as_bytes())
            .map_err(|e| CliError::io(format!("cannot save schedules.json: {e}")))
    }

    pub fn state(&self, job_id: &str) -> JobState {
        std::fs::read_to_string(self.state_path(job_id))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save_state(&self, job_id: &str, state: &JobState) -> Result<(), CliError> {
        let text =
            serde_json::to_vec_pretty(state).map_err(|e| CliError::internal(e.to_string()))?;
        write_private(&self.state_path(job_id), &text).map_err(|e| CliError::io(e.to_string()))
    }

    pub fn remove_state(&self, job_id: &str) {
        let _ = std::fs::remove_file(self.state_path(job_id));
    }
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum ScheduleAction {
    /// Run a notebook on a schedule on this computer.
    Add(Box<AddArgs>),
    /// List scheduled notebook jobs.
    List {
        #[arg(long, value_enum, default_value_t = NotebookFormat::Table)]
        format: NotebookFormat,
    },
    /// Remove a job and its OS schedule.
    Remove {
        /// Job id, or the notebook file of a single job.
        job: String,
    },
    /// Run a job now (retries, alerts and history as a scheduled run).
    RunNow {
        /// Job id, or the notebook file of a single job.
        job: String,
        /// Started by the OS scheduler: honours the circuit breaker.
        #[arg(long)]
        scheduled: bool,
    },
    /// Run every job whose time has come (started each minute where the OS
    /// scheduler cannot express a job's times itself).
    #[command(hide = true)]
    Tick,
}

#[derive(Debug, Clone, clap::Args)]
pub struct AddArgs {
    pub file: PathBuf,
    /// Five-field cron expression (`0 7 * * 1-5`) or @daily, @hourly, …
    #[arg(long)]
    pub cron: String,
    /// IANA time zone (`Europe/London`); default: this computer's zone.
    #[arg(long)]
    pub tz: Option<String>,
    #[arg(long)]
    pub param_set: Option<PathBuf>,
    #[arg(long = "param", value_name = "NAME=VALUE")]
    pub params: Vec<String>,
    #[arg(long = "connection", value_name = "SLOT=CONNECTION")]
    pub connections: Vec<String>,
    #[arg(long)]
    pub cells: Option<String>,
    /// Allow read-only cells on production connections.
    #[arg(long)]
    pub allow_production: bool,
    /// Output snapshots to keep.
    #[arg(long, default_value_t = 20)]
    pub retain: usize,
    #[arg(long, default_value_t = 300)]
    pub timeout: u64,
    /// `desktop` or `webhook:https://…`. Repeat for more.
    #[arg(long)]
    pub notify: Vec<String>,
    /// Alert rules, comma-separated: failure, assertion, kpi, incomplete.
    #[arg(long, default_value = "failure,assertion,kpi")]
    pub alert_on: String,
    /// Let checks and KPIs over incomplete data alert.
    #[arg(long)]
    pub include_incomplete: bool,
    /// Hold non-critical alerts during these hours, e.g. 22:00-07:00.
    #[arg(long)]
    pub quiet_hours: Option<String>,
    #[arg(long, value_enum, default_value_t = Severity::Warning)]
    pub min_severity: Severity,
    #[arg(long, default_value_t = 2)]
    pub retries: u32,
    #[arg(long, default_value_t = 60)]
    pub backoff: u64,
    /// Pause the job after this many failed runs in a row (0 = never).
    #[arg(long, default_value_t = 5)]
    pub breaker_after: u32,
    #[arg(long, value_enum)]
    pub backend: Option<Backend>,
    /// Save the job and print what would be installed, without installing.
    #[arg(long)]
    pub no_install: bool,
    /// Linux (systemd): turn on lingering for your user (`loginctl
    /// enable-linger`) so the job keeps running after you log out. Never
    /// done without this flag.
    #[arg(long)]
    pub enable_linger: bool,
    /// Trust the notebook as it will run (saved, so scheduled runs can
    /// start). Scheduled runs stop if the notebook changes afterwards.
    #[arg(long)]
    pub trust: bool,
}

/// What to tell the user about a systemd timer given whether lingering is
/// on: user timers stop at logout without it.
fn logout_warning(lingering: Option<bool>) -> Option<String> {
    match lingering {
        Some(true) => None,
        Some(false) => Some(
            "systemd user timers stop when you log out, so this job only runs while you are logged in; add --enable-linger (or run `loginctl enable-linger`) to keep it running after logout".into(),
        ),
        None => Some(
            "systemd user timers stop when you log out unless lingering is on for your user (`loginctl enable-linger`, or add --enable-linger)".into(),
        ),
    }
}

/// Refuse a schedule the notebook's trust does not cover.
fn authorize_schedule(job: &Job, trust_now: bool, runs: &RunsDir) -> Result<(), CliError> {
    use super::security;
    let doc = super::source::load_file(&job.notebook)?.doc;
    let store = load_store(&store_path())?;
    let mappings = super::remap::parse_mappings(&doc, &job.connections)?;
    let slots = super::remap::resolve(&doc, &mappings, &store)?;
    let effective = security::effective_doc(&doc, &slots);
    let trust = if trust_now {
        security::remember_trust(&effective, &job.notebook, &runs.trust_path())?
    } else {
        security::assess(&effective, &job.notebook, &runs.trust_path())
    };
    let decision = security::schedule_decision(&effective, &slots, &trust, job.allow_production);
    let actor = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".into());
    security::Auditor::new(
        &runs.audit_dir(),
        &format!("{actor} (sqlly CLI)"),
        &doc.notebook_id,
        &trust.executable_hash,
    )
    .schedule(&decision);
    if decision.is_allowed() {
        return Ok(());
    }
    let reason = decision
        .reason()
        .map_or_else(String::new, |r| r.message().fallback().to_string());
    Err(CliError::policy(
        format!("refused to schedule: {reason}"),
        (!trust_now).then(|| "pass --trust to trust the notebook as it will run".to_string()),
    ))
}

fn new_job_id() -> String {
    sqlly_services::notebook::document::new_cell_id(
        &mut sqlly_services::notebook::RandomIds,
        |_| false,
    )
    .replacen("c_", "j_", 1)
}

fn program() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "sqlly".into())
}

fn find_job<'a>(file: &'a JobsFile, key: &str) -> Result<&'a Job, CliError> {
    if let Some(job) = file.jobs.iter().find(|j| j.id == key) {
        return Ok(job);
    }
    let path = std::fs::canonicalize(key).unwrap_or_else(|_| PathBuf::from(key));
    let matching: Vec<&Job> = file.jobs.iter().filter(|j| j.notebook == path).collect();
    match matching.as_slice() {
        [one] => Ok(one),
        [] => Err(CliError::not_found(format!("no scheduled job {key}"))),
        _ => Err(CliError::usage(format!(
            "{key} has several jobs; give a job id (see `schedule list`)"
        ))),
    }
}

fn job_from(args: &AddArgs) -> Result<Job, CliError> {
    Cron::parse(&args.cron).map_err(CliError::usage)?;
    if let Some(tz) = &args.tz {
        tz.parse::<chrono_tz::Tz>()
            .map_err(|_| CliError::usage(format!("unknown time zone '{tz}'")))?;
    }
    let notebook = std::fs::canonicalize(&args.file)
        .map_err(|e| CliError::io(format!("cannot find {}: {e}", args.file.display())))?;
    let doc = super::source::load_file(&notebook)?.doc;
    let notify = args
        .notify
        .iter()
        .map(|t| NotifyTarget::parse(t).map_err(CliError::usage))
        .collect::<Result<Vec<_>, _>>()?;
    let mut alerts = AlertConfig {
        on_failure: false,
        on_assertion: false,
        on_kpi: false,
        on_incomplete: false,
        include_incomplete: args.include_incomplete,
        quiet_hours: args
            .quiet_hours
            .as_deref()
            .map(QuietHours::parse)
            .transpose()
            .map_err(CliError::usage)?,
        min_severity: args.min_severity,
        notify_recovery: true,
    };
    for rule in args
        .alert_on
        .split(',')
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        match rule {
            "failure" => alerts.on_failure = true,
            "assertion" => alerts.on_assertion = true,
            "kpi" => alerts.on_kpi = true,
            "incomplete" => alerts.on_incomplete = true,
            other => {
                return Err(CliError::usage(format!(
                    "unknown alert rule '{other}' (failure, assertion, kpi, incomplete)"
                )));
            }
        }
    }
    let param_set = args
        .param_set
        .as_ref()
        .map(|p| {
            std::fs::canonicalize(p).map_err(|e| CliError::io(format!("{}: {e}", p.display())))
        })
        .transpose()?;
    Ok(Job {
        id: new_job_id(),
        notebook,
        title: doc.title,
        cron: Cron::parse(&args.cron)
            .map_err(CliError::usage)?
            .expression(),
        tz: args.tz.clone(),
        param_set,
        params: args.params.clone(),
        connections: args.connections.clone(),
        cells: args.cells.clone(),
        allow_production: args.allow_production,
        retain: args.retain,
        timeout: args.timeout,
        notify,
        alerts,
        retry: RetryPolicy {
            attempts: args.retries,
            backoff_seconds: args.backoff,
        },
        breaker_after: args.breaker_after,
        created_at: Utc::now(),
        backend: None,
    })
}

fn run_args(job: &Job) -> RunArgs {
    let mut args = RunArgs::new(job.notebook.clone());
    args.params = job.params.clone();
    args.param_set = job.param_set.clone();
    args.connections = job.connections.clone();
    args.cells = job.cells.clone();
    args.allow_production = job.allow_production;
    args.retain = job.retain;
    args.timeout = job.timeout;
    args.fail_on = FailOn::Assertion;
    args
}

/// Run a job once, with retries, history, breaker and alerts.
async fn run_job(
    job: &Job,
    store: &ScheduleStore,
    runs: &RunsDir,
    scheduled: bool,
) -> Result<Value, CliError> {
    let state = store.state(&job.id);
    if !breaker_allows(&state, !scheduled) {
        return Ok(json!({
            "job": job.id,
            "skipped": "paused after repeated failures; run `schedule run-now` by hand to resume",
        }));
    }
    let env = RunEnv {
        store: load_store(&store_path())?,
        runs: runs.clone(),
        trigger: if scheduled {
            RunTrigger::Scheduled
        } else {
            RunTrigger::Cli
        },
    };
    let args = run_args(job);
    let mut delays = retry_delays(job.retry).into_iter();
    let (outcome, exit) = loop {
        let result = runner::run(&args, &env).await;
        let exit = match &result {
            Ok(report) => runner::exit_for(&report.summary, args.fail_on),
            Err(err) => err.exit,
        };
        if should_retry(exit)
            && let Some(delay) = delays.next()
        {
            tokio::time::sleep(delay).await;
            continue;
        }
        break (result, exit);
    };
    let ok = !matches!(
        exit,
        ExitCode::Error | ExitCode::PolicyRefused | ExitCode::Usage
    );
    let (mut next, tripped) = record_outcome(&state, ok, job.breaker_after);
    next.last_run_at = Some(Utc::now());
    next.last_exit = Some(exit as i32);
    let mut signals = Vec::new();
    let mut run_id = None;
    match &outcome {
        Ok(report) => {
            run_id = report.record.as_ref().map(|r| r.run_id.clone());
            signals = alerts::signals(&report.summary, &report.details, &job.alerts);
        }
        Err(err) if job.alerts.on_failure => signals.push(alerts::Signal {
            key: "failure".into(),
            severity: Severity::Critical,
            title: format!("the notebook could not run ({})", err.body.code),
        }),
        Err(_) => {}
    }
    if tripped {
        signals.push(alerts::Signal {
            key: "paused".into(),
            severity: Severity::Critical,
            title: format!(
                "paused after {} failed runs in a row",
                next.consecutive_failures
            ),
        });
    }
    next.last_run_id = run_id.clone();
    let now_local = match &job.tz {
        Some(tz) => tz
            .parse::<chrono_tz::Tz>()
            .map(|z| Utc::now().with_timezone(&z).time())
            .unwrap_or_else(|_| chrono::Local::now().time()),
        None => chrono::Local::now().time(),
    };
    let (notifications, alert_state) =
        alerts::decide(&job.alerts, &state.alerts, &signals, now_local);
    next.alerts = alert_state;
    let warnings = alerts::deliver(
        &job.notify,
        &job.id,
        &job.title,
        run_id.as_deref(),
        &notifications,
    )
    .await;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    store.save_state(&job.id, &next)?;
    let mut data = json!({
        "job": job.id,
        "exit_code": exit as i32,
        "run_id": run_id,
        "alerts_sent": notifications,
        "paused": next.circuit_open,
    });
    if let Err(err) = &outcome {
        data["error"] = json!({"code": err.body.code, "message": err.body.message});
    }
    Ok(data)
}

pub async fn command(action: &ScheduleAction, runs: &RunsDir, json: bool) -> ExitCode {
    let store = ScheduleStore::new(runs.root.clone());
    let result: Result<(String, Value, ExitCode), CliError> = async {
        match action {
            ScheduleAction::Add(args) => {
                let mut job = job_from(args)?;
                authorize_schedule(&job, args.trust, runs)?;
                let backend = args.backend.unwrap_or_else(Backend::detect);
                job.backend = Some(backend);
                let program = program();
                let files = install::rendered(&job, backend, &program, &store.logs_dir())?;
                let mut file = store.load()?;
                file.version = 1;
                file.jobs.push(job.clone());
                store.save(&file)?;
                let _ = std::fs::create_dir_all(store.logs_dir());
                if !args.no_install
                    && let Err(err) = install::install(&job, backend, &program, &store.logs_dir())
                {
                    file.jobs.retain(|j| j.id != job.id);
                    store.save(&file)?;
                    return Err(err);
                }
                let mut linger_warning = None;
                if backend == Backend::Systemd && !args.no_install {
                    if args.enable_linger
                        && let Err(err) = install::enable_lingering()
                    {
                        linger_warning = Some(format!(
                            "could not enable lingering: {}",
                            err.body.message
                        ));
                    }
                    linger_warning = linger_warning.or(logout_warning(install::lingering()));
                }
                let upcoming = job.upcoming(Utc::now(), 3);
                let mut text = format!(
                    "scheduled {} as job {} ({})\nruns through {}\nonly while this computer is on; scheduled runs never change data\n",
                    job.notebook.display(),
                    job.id,
                    job.cron,
                    backend.describe()
                );
                for at in &upcoming {
                    text.push_str(&format!("next: {at}\n"));
                }
                if let Some(warning) = &linger_warning {
                    text.push_str(&format!("warning: {warning}\n"));
                }
                if args.no_install {
                    for (path, body) in &files {
                        text.push_str(&format!("\n# {path}\n{body}"));
                    }
                }
                Ok((
                    text.trim_end().to_string(),
                    json!({"job": job, "next_runs": upcoming, "installed": !args.no_install, "warning": linger_warning, "files": files.iter().map(|(p, _)| p).collect::<Vec<_>>()}),
                    ExitCode::Ok,
                ))
            }
            ScheduleAction::List { format } => {
                let file = store.load()?;
                let mut rows = Vec::new();
                for job in &file.jobs {
                    let state = store.state(&job.id);
                    rows.push(json!({
                        "id": job.id,
                        "notebook": job.notebook,
                        "cron": job.cron,
                        "tz": job.tz,
                        "next_run": job.next_after(Utc::now()),
                        "last_run_at": state.last_run_at,
                        "last_exit": state.last_exit,
                        "paused": state.circuit_open,
                        "backend": job.backend,
                    }));
                }
                let text = if rows.is_empty() {
                    "no scheduled notebooks".to_string()
                } else {
                    rows.iter()
                        .map(|r| {
                            format!(
                                "{}  {}  {}{}  next {}{}",
                                r["id"].as_str().unwrap_or(""),
                                r["cron"].as_str().unwrap_or(""),
                                r["notebook"].as_str().unwrap_or(""),
                                r["tz"].as_str().map(|t| format!(" ({t})")).unwrap_or_default(),
                                r["next_run"].as_str().unwrap_or("-"),
                                if r["paused"].as_bool() == Some(true) { "  PAUSED" } else { "" }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let _ = format;
                Ok((text, json!({"jobs": rows}), ExitCode::Ok))
            }
            ScheduleAction::Remove { job } => {
                let mut file = store.load()?;
                let found = find_job(&file, job)?.clone();
                if let Some(backend) = found.backend
                    && let Err(err) = install::uninstall(&found, backend)
                {
                    eprintln!("warning: {}", err.body.message);
                }
                file.jobs.retain(|j| j.id != found.id);
                store.save(&file)?;
                store.remove_state(&found.id);
                found.notify.iter().for_each(alerts::forget_webhook);
                Ok((format!("removed job {}", found.id), json!({"removed": found.id}), ExitCode::Ok))
            }
            ScheduleAction::RunNow { job, scheduled } => {
                let file = store.load()?;
                let found = find_job(&file, job)?.clone();
                let mut data = run_job(&found, &store, runs, *scheduled).await?;
                let mut state = store.state(&found.id);
                state.evaluated_until = Some(Utc::now());
                store.save_state(&found.id, &state)?;
                let exit = match data["exit_code"].as_i64() {
                    Some(0) | None => ExitCode::Ok,
                    Some(4) => ExitCode::AssertionFailed,
                    Some(3) => ExitCode::PolicyRefused,
                    Some(130) => ExitCode::Cancelled,
                    _ => ExitCode::Error,
                };
                let text = match data.get("skipped").and_then(Value::as_str) {
                    Some(why) => format!("job {} skipped: {why}", found.id),
                    None => format!(
                        "job {} ran as {} (exit {})",
                        found.id,
                        data["run_id"].as_str().unwrap_or("-"),
                        data["exit_code"]
                    ),
                };
                data["job"] = json!(found.id);
                Ok((text, data, exit))
            }
            ScheduleAction::Tick => {
                let file = store.load()?;
                let now = Utc::now();
                let mut ran = Vec::new();
                for job in &file.jobs {
                    let state = store.state(&job.id);
                    if state.evaluated_until.is_none() {
                        // First tick: start counting from now, not creation.
                        let mut fresh = state.clone();
                        fresh.evaluated_until = Some(job.created_at.max(now - chrono::Duration::minutes(1)));
                        store.save_state(&job.id, &fresh)?;
                    }
                    let state = store.state(&job.id);
                    if !is_due(job, &state, now) {
                        continue;
                    }
                    let data = run_job(job, &store, runs, true).await?;
                    let mut after = store.state(&job.id);
                    after.evaluated_until = Some(now);
                    store.save_state(&job.id, &after)?;
                    ran.push(data);
                }
                Ok((format!("{} job(s) ran", ran.len()), json!({"ran": ran}), ExitCode::Ok))
            }
        }
    }
    .await;
    match result {
        Ok((text, data, exit)) => super::finish(json, NotebookFormat::Table, text, data, exit),
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn systemd_installs_say_timers_stop_at_logout_without_lingering() {
        assert_eq!(logout_warning(Some(true)), None);
        for lingering in [Some(false), None] {
            let warning = logout_warning(lingering).unwrap();
            assert!(
                warning.contains("log out") && warning.contains("--enable-linger"),
                "{warning}"
            );
        }
        assert!(
            Backend::Systemd.describe().contains("logged in"),
            "the install message never claims a timer runs whenever the computer is on"
        );
    }

    #[test]
    fn lingering_is_only_enabled_when_asked() {
        use clap::Parser;
        #[derive(clap::Parser)]
        struct Probe {
            #[command(flatten)]
            add: AddArgs,
        }
        let plain = Probe::try_parse_from(["probe", "nb.sqllynb", "--cron", "@daily"]).unwrap();
        assert!(!plain.add.enable_linger);
        let asked =
            Probe::try_parse_from(["probe", "nb.sqllynb", "--cron", "@daily", "--enable-linger"])
                .unwrap();
        assert!(asked.add.enable_linger);
    }

    fn job(cron: &str, tz: Option<&str>) -> Job {
        Job {
            id: "j_test".into(),
            notebook: PathBuf::from("/nb/report.sqllynb"),
            title: "Report".into(),
            cron: cron.into(),
            tz: tz.map(str::to_string),
            param_set: None,
            params: vec![],
            connections: vec![],
            cells: None,
            allow_production: false,
            retain: 5,
            timeout: 60,
            notify: vec![],
            alerts: AlertConfig::default(),
            retry: RetryPolicy::default(),
            breaker_after: 3,
            created_at: Utc.with_ymd_and_hms(2026, 11, 18, 6, 0, 0).unwrap(),
            backend: None,
        }
    }

    #[test]
    fn webhook_urls_never_reach_schedules_json() {
        keyring::mock::install();
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path());
        let mut j = job("0 7 * * *", None);
        j.id = "j_hook_test".into();
        j.notify = vec![
            NotifyTarget::Desktop,
            NotifyTarget::parse("webhook:https://hooks.example.com/services/T0K3N").unwrap(),
        ];
        store
            .save(&JobsFile {
                version: 1,
                jobs: vec![j],
            })
            .unwrap();
        let text = std::fs::read_to_string(store.jobs_path()).unwrap();
        assert!(!text.contains("T0K3N"), "{text}");
        assert!(!text.contains("hooks.example.com"), "{text}");
        let loaded = store.load().unwrap();
        let target = &loaded.jobs[0].notify[1];
        assert!(matches!(target, NotifyTarget::StoredWebhook { .. }));
        assert_eq!(
            alerts::webhook_url(target).unwrap().as_deref(),
            Some("https://hooks.example.com/services/T0K3N")
        );
        store.save(&loaded).unwrap();
        assert!(alerts::webhook_url(&loaded.jobs[0].notify[1]).is_ok());
        alerts::forget_webhook(target);
        assert!(alerts::webhook_url(target).is_err());
    }

    #[test]
    fn retries_back_off_and_only_for_errors() {
        let delays = retry_delays(RetryPolicy {
            attempts: 4,
            backoff_seconds: 900,
        });
        assert_eq!(
            delays.iter().map(Duration::as_secs).collect::<Vec<_>>(),
            [900, 1800, 3600, 3600]
        );
        assert!(should_retry(ExitCode::Error));
        for no in [
            ExitCode::Ok,
            ExitCode::AssertionFailed,
            ExitCode::PolicyRefused,
            ExitCode::Usage,
        ] {
            assert!(!should_retry(no));
        }
    }

    #[test]
    fn the_breaker_opens_after_repeated_failures_and_closes_on_success() {
        let mut state = JobState::default();
        let mut tripped_at = None;
        for i in 1..=4 {
            let (next, tripped) = record_outcome(&state, false, 3);
            if tripped {
                tripped_at = Some(i);
            }
            state = next;
        }
        assert_eq!(tripped_at, Some(3), "trips once, on the third failure");
        assert!(state.circuit_open);
        assert!(!breaker_allows(&state, false));
        assert!(breaker_allows(&state, true));
        let (state, _) = record_outcome(&state, true, 3);
        assert!(!state.circuit_open && state.consecutive_failures == 0);
        let (never, tripped) = record_outcome(
            &JobState {
                consecutive_failures: 99,
                ..JobState::default()
            },
            false,
            0,
        );
        assert!(!tripped && !never.circuit_open);
    }

    #[test]
    fn due_times_follow_the_jobs_zone() {
        let j = job("0 7 * * *", Some("Europe/London"));
        let state = JobState {
            evaluated_until: Some(Utc.with_ymd_and_hms(2026, 11, 18, 6, 59, 0).unwrap()),
            ..JobState::default()
        };
        assert!(!is_due(
            &j,
            &state,
            Utc.with_ymd_and_hms(2026, 11, 18, 6, 59, 30).unwrap()
        ));
        assert!(is_due(
            &j,
            &state,
            Utc.with_ymd_and_hms(2026, 11, 18, 7, 0, 0).unwrap()
        ));
        let ny = job("0 7 * * *", Some("America/New_York"));
        assert!(!is_due(
            &ny,
            &state,
            Utc.with_ymd_and_hms(2026, 11, 18, 7, 0, 0).unwrap()
        ));
        assert!(is_due(
            &ny,
            &state,
            Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).unwrap()
        ));
    }

    #[test]
    fn jobs_round_trip_and_store_privately() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::new(dir.path());
        assert!(store.load().unwrap().jobs.is_empty());
        let file = JobsFile {
            version: 1,
            jobs: vec![job("@daily", None)],
        };
        store.save(&file).unwrap();
        assert_eq!(store.load().unwrap(), file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("schedules.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let state = JobState {
            consecutive_failures: 2,
            ..JobState::default()
        };
        store.save_state("j_test", &state).unwrap();
        assert_eq!(store.state("j_test"), state);
        assert_eq!(find_job(&file, "j_test").unwrap().id, "j_test");
        assert!(find_job(&file, "j_nope").is_err());
    }
}
