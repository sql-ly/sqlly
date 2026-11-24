//! Alert rules for scheduled runs, the pure decision of what to send, and
//! the thin delivery shell-outs.
//!
//! A run produces signals (a failure, a failed check, a KPI past its
//! threshold, incomplete data). Each signal has a key; the job's state file
//! remembers which keys are firing, so an alert goes out only when a key
//! starts firing (and, optionally, a recovery note when it stops), never
//! on every run. During quiet hours only critical alerts go out; others
//! wait and are sent at the first run after quiet hours if still firing.
//!
//! Checks and KPIs computed from incomplete data (a capped, sampled or
//! partial input) never alert unless the job opts in, and checks whose
//! input failed are blocked, so they never alert either: only the failure
//! itself does.
//!
//! Email is not offered: it would need an SMTP client that is not one of
//! the vetted dependencies. Point a webhook at your mail or chat service
//! instead.

use std::collections::BTreeMap;

use chrono::NaiveTime;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlly_services::analytics::kpi::KpiStatus;

use crate::agent::notebook::records::RunDetails;
use crate::agent::notebook::runner::RunSummary;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    pub start: NaiveTime,
    pub end: NaiveTime,
}

impl QuietHours {
    /// `22:00-07:00` (may wrap past midnight).
    pub fn parse(text: &str) -> Result<Self, String> {
        let (a, b) = text
            .split_once('-')
            .ok_or_else(|| format!("quiet hours look like 22:00-07:00, got '{text}'"))?;
        let time = |t: &str| {
            NaiveTime::parse_from_str(t.trim(), "%H:%M")
                .map_err(|_| format!("'{}' is not a HH:MM time", t.trim()))
        };
        Ok(Self {
            start: time(a)?,
            end: time(b)?,
        })
    }

    pub fn contains(&self, now: NaiveTime) -> bool {
        if self.start <= self.end {
            now >= self.start && now < self.end
        } else {
            now >= self.start || now < self.end
        }
    }
}

fn yes() -> bool {
    true
}

fn warning() -> Severity {
    Severity::Warning
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertConfig {
    #[serde(default = "yes")]
    pub on_failure: bool,
    #[serde(default = "yes")]
    pub on_assertion: bool,
    #[serde(default = "yes")]
    pub on_kpi: bool,
    /// Alert when a cell's output was cut short. Off unless asked for.
    #[serde(default)]
    pub on_incomplete: bool,
    /// Let checks and KPIs over incomplete data alert. Off unless asked for.
    #[serde(default)]
    pub include_incomplete: bool,
    #[serde(default)]
    pub quiet_hours: Option<QuietHours>,
    #[serde(default = "warning")]
    pub min_severity: Severity,
    #[serde(default = "yes")]
    pub notify_recovery: bool,
}

impl Default for AlertConfig {
    fn default() -> Self {
        Self {
            on_failure: true,
            on_assertion: true,
            on_kpi: true,
            on_incomplete: false,
            include_incomplete: false,
            quiet_hours: None,
            min_severity: Severity::Warning,
            notify_recovery: true,
        }
    }
}

/// A condition that holds after a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    pub key: String,
    pub severity: Severity,
    pub title: String,
}

/// What this run's results say, per the job's rules. Titles carry cell
/// names and counts only: never values, SQL or connection details.
pub fn signals(summary: &RunSummary, details: &RunDetails, config: &AlertConfig) -> Vec<Signal> {
    let mut out = Vec::new();
    if config.on_failure && summary.has_errors() {
        let mut cells: Vec<&str> = summary
            .failed
            .iter()
            .chain(&summary.blocked)
            .chain(&summary.canceled)
            .map(String::as_str)
            .collect();
        cells.truncate(10);
        out.push(Signal {
            key: "failure".into(),
            severity: Severity::Critical,
            title: format!("cells did not run: {}", cells.join(", ")),
        });
    }
    if config.on_assertion {
        for (cell, outcomes) in &details.assertions {
            let failing: Vec<&Value> = outcomes
                .as_array()
                .into_iter()
                .flatten()
                .filter(|o| o.get("status").and_then(Value::as_str) == Some("fail"))
                .filter(|o| {
                    config.include_incomplete
                        || !o
                            .pointer("/evidence/partial")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                })
                .collect();
            if !failing.is_empty() {
                let names: Vec<&str> = failing
                    .iter()
                    .filter_map(|o| o.get("name").and_then(Value::as_str))
                    .collect();
                out.push(Signal {
                    key: format!("assertion:{cell}"),
                    severity: Severity::Warning,
                    title: format!(
                        "{cell}: {} check(s) failed ({})",
                        failing.len(),
                        names.join(", ")
                    ),
                });
            }
        }
    }
    if config.on_kpi {
        for kpi in &summary.kpis {
            if kpi.inconclusive && !config.include_incomplete {
                continue;
            }
            let severity = match kpi.status {
                KpiStatus::Critical => Severity::Critical,
                KpiStatus::Warning => Severity::Warning,
                _ => continue,
            };
            out.push(Signal {
                key: format!("kpi:{}", kpi.cell),
                severity,
                title: format!(
                    "{} is {}",
                    kpi.cell,
                    if severity == Severity::Critical {
                        "critical"
                    } else {
                        "in warning"
                    }
                ),
            });
        }
    }
    if config.on_incomplete {
        for cell in &summary.incomplete {
            out.push(Signal {
                key: format!("incomplete:{cell}"),
                severity: Severity::Warning,
                title: format!("{cell} returned incomplete data"),
            });
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeyState {
    /// A notification for the current firing spell went out (or was
    /// deliberately not sent, below the minimum severity).
    pub notified: bool,
    pub severity: Option<Severity>,
}

/// Firing keys, kept in the job's state file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AlertState {
    #[serde(default)]
    pub firing: BTreeMap<String, KeyState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    Firing,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notification {
    pub key: String,
    pub kind: NotificationKind,
    pub severity: Severity,
    pub title: String,
}

/// What to send now, and the state to keep.
pub fn decide(
    config: &AlertConfig,
    previous: &AlertState,
    current: &[Signal],
    now_local: NaiveTime,
) -> (Vec<Notification>, AlertState) {
    let quiet = config.quiet_hours.is_some_and(|q| q.contains(now_local));
    let mut next = AlertState::default();
    let mut out = Vec::new();
    for signal in current {
        let before = previous.firing.get(&signal.key);
        let below = signal.severity < config.min_severity;
        let held = quiet && signal.severity < Severity::Critical;
        let send = match before {
            Some(state) if state.notified => false,
            _ => !below && !held,
        };
        if send {
            out.push(Notification {
                key: signal.key.clone(),
                kind: NotificationKind::Firing,
                severity: signal.severity,
                title: signal.title.clone(),
            });
        }
        next.firing.insert(
            signal.key.clone(),
            KeyState {
                notified: before.is_some_and(|s| s.notified) || send || below,
                severity: Some(signal.severity),
            },
        );
    }
    for (key, state) in &previous.firing {
        if next.firing.contains_key(key) {
            continue;
        }
        let severity = state.severity.unwrap_or(Severity::Warning);
        if state.notified && config.notify_recovery && !quiet && severity >= config.min_severity {
            out.push(Notification {
                key: key.clone(),
                kind: NotificationKind::Resolved,
                severity: Severity::Info,
                title: format!("{key} is back to normal"),
            });
        }
    }
    (out, next)
}

// ---- delivery -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotifyTarget {
    Desktop,
    /// A webhook as given on the command line. Never written to disk:
    /// saving a job moves the URL into the credential store.
    Webhook {
        url: String,
    },
    /// A webhook whose URL (often carrying a token) is kept in the OS
    /// credential store under [`WEBHOOK_SERVICE`] and `account`.
    StoredWebhook {
        account: String,
    },
}

/// Credential-store service holding scheduled jobs' webhook URLs.
pub const WEBHOOK_SERVICE: &str = "SQLLY.notebook-schedule";

/// Credential-store account for the `index`th webhook of `job_id`.
pub fn webhook_account(job_id: &str, index: usize) -> String {
    match index {
        0 => format!("webhook.{job_id}"),
        n => format!("webhook.{job_id}.{n}"),
    }
}

/// Move `target`'s URL into the credential store; other targets as is.
pub fn store_webhook(target: &NotifyTarget, account: String) -> Result<NotifyTarget, String> {
    let NotifyTarget::Webhook { url } = target else {
        return Ok(target.clone());
    };
    keyring::Entry::new(WEBHOOK_SERVICE, &account)
        .and_then(|entry| entry.set_password(url))
        .map_err(|e| format!("cannot keep the webhook URL in the system credential store: {e}"))?;
    Ok(NotifyTarget::StoredWebhook { account })
}

/// Remove a stored webhook URL (the job was removed).
pub fn forget_webhook(target: &NotifyTarget) {
    if let NotifyTarget::StoredWebhook { account } = target
        && let Ok(entry) = keyring::Entry::new(WEBHOOK_SERVICE, account)
    {
        let _ = entry.delete_credential();
    }
}

/// The URL a webhook target posts to.
pub fn webhook_url(target: &NotifyTarget) -> Result<Option<String>, String> {
    match target {
        NotifyTarget::Desktop => Ok(None),
        NotifyTarget::Webhook { url } => Ok(Some(url.clone())),
        NotifyTarget::StoredWebhook { account } => keyring::Entry::new(WEBHOOK_SERVICE, account)
            .and_then(|entry| entry.get_password())
            .map(Some)
            .map_err(|e| {
                format!("the webhook URL for {account} is not in the credential store: {e}")
            }),
    }
}

impl NotifyTarget {
    /// `desktop` or `webhook:https://…`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("desktop") {
            return Ok(NotifyTarget::Desktop);
        }
        if let Some(url) = text
            .strip_prefix("webhook:")
            .or_else(|| text.strip_prefix("webhook="))
        {
            validate_webhook(url)?;
            return Ok(NotifyTarget::Webhook {
                url: url.to_string(),
            });
        }
        if text.starts_with("email") {
            return Err(
                "email alerts are not built in; send a webhook to your mail or chat service".into(),
            );
        }
        Err(format!(
            "unknown --notify target '{text}' (use desktop or webhook:https://…)"
        ))
    }
}

/// Webhooks must be HTTPS and carry no credentials in the URL.
pub fn validate_webhook(url: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url).map_err(|e| format!("invalid webhook URL: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("webhook URLs must use https".into());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(
            "put webhook credentials in a header-free secret URL path, not user:password@".into(),
        );
    }
    if parsed.host_str().is_none_or(str::is_empty) {
        return Err("the webhook URL has no host".into());
    }
    Ok(())
}

/// The JSON a webhook receives. Names and counts only.
pub fn webhook_payload(
    job_id: &str,
    notebook_title: &str,
    run_id: Option<&str>,
    notifications: &[Notification],
) -> Value {
    json!({
        "source": "sqlly-notebook",
        "job": job_id,
        "notebook": notebook_title,
        "run_id": run_id,
        "alerts": notifications,
    })
}

/// A program, its arguments and extra environment variables to run.
pub struct DesktopCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Environment variables the Windows toast script reads its text from.
const TOAST_TITLE_VAR: &str = "SQLLY_TOAST_TITLE";
const TOAST_BODY_VAR: &str = "SQLLY_TOAST_BODY";

/// The Windows toast: a fixed script that reads the (XML-escaped) title and
/// body from environment variables, so no notebook text is ever parsed as
/// PowerShell (which also treats U+2018–U+201B as quotes).
pub fn windows_toast_command(title: &str, body: &str) -> DesktopCommand {
    let script = format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; \
         $x = New-Object Windows.Data.Xml.Dom.XmlDocument; \
         $x.LoadXml('<toast><visual><binding template=\"ToastGeneric\"><text>' + $env:{TOAST_TITLE_VAR} + '</text><text>' + $env:{TOAST_BODY_VAR} + '</text></binding></visual></toast>'); \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('SQLLY').Show([Windows.UI.Notifications.ToastNotification]::new($x))"
    );
    DesktopCommand {
        program: "powershell".into(),
        args: vec![
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script,
        ],
        env: vec![
            (TOAST_TITLE_VAR.into(), super::render::xml_escape(title)),
            (TOAST_BODY_VAR.into(), super::render::xml_escape(body)),
        ],
    }
}

/// A command line that shows a desktop notification on this OS.
pub fn desktop_command(title: &str, body: &str) -> Option<DesktopCommand> {
    if cfg!(target_os = "macos") {
        let quote = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        Some(DesktopCommand {
            program: "osascript".into(),
            args: vec![
                "-e".into(),
                format!(
                    "display notification {} with title {}",
                    quote(body),
                    quote(title)
                ),
            ],
            env: Vec::new(),
        })
    } else if cfg!(target_os = "windows") {
        Some(windows_toast_command(title, body))
    } else if cfg!(target_os = "linux") {
        Some(DesktopCommand {
            program: "notify-send".into(),
            args: vec![
                "--app-name=SQLLY".into(),
                "--".into(),
                title.into(),
                body.into(),
            ],
            env: Vec::new(),
        })
    } else {
        None
    }
}

/// Send `notifications` to every target. Delivery problems are returned
/// as warnings; they never fail the run.
pub async fn deliver(
    targets: &[NotifyTarget],
    job_id: &str,
    notebook_title: &str,
    run_id: Option<&str>,
    notifications: &[Notification],
) -> Vec<String> {
    let mut warnings = Vec::new();
    if notifications.is_empty() {
        return warnings;
    }
    for target in targets {
        match target {
            NotifyTarget::Desktop => {
                let body: Vec<&str> = notifications.iter().map(|n| n.title.as_str()).collect();
                let title = format!("SQLLY: {notebook_title}");
                if let Some(DesktopCommand { program, args, env }) =
                    desktop_command(&title, &body.join("\n"))
                {
                    let status = tokio::process::Command::new(&program)
                        .args(&args)
                        .envs(env)
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .await;
                    if !status.is_ok_and(|s| s.success()) {
                        warnings.push(format!("desktop notification via {program} failed"));
                    }
                }
            }
            NotifyTarget::Webhook { .. } | NotifyTarget::StoredWebhook { .. } => {
                let url = match webhook_url(target) {
                    Ok(Some(url)) => url,
                    Ok(None) => continue,
                    Err(e) => {
                        warnings.push(e);
                        continue;
                    }
                };
                let url = url.as_str();
                if let Err(e) = validate_webhook(url) {
                    warnings.push(e);
                    continue;
                }
                let payload = webhook_payload(job_id, notebook_title, run_id, notifications);
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .redirect(reqwest::redirect::Policy::none())
                    .build();
                let sent = match client {
                    Ok(client) => client.post(url).json(&payload).send().await,
                    Err(e) => {
                        warnings.push(format!("webhook client: {e}"));
                        continue;
                    }
                };
                match sent {
                    Ok(response) if response.status().is_success() => {}
                    Ok(response) => {
                        warnings.push(format!("webhook answered {}", response.status()))
                    }
                    Err(e) => warnings.push(format!("webhook failed: {}", e.without_url())),
                }
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::notebook::runner::KpiLine;

    fn t(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    fn signal(key: &str, severity: Severity) -> Signal {
        Signal {
            key: key.into(),
            severity,
            title: key.into(),
        }
    }

    #[test]
    fn alerts_only_on_transitions() {
        let config = AlertConfig::default();
        let failing = [signal("failure", Severity::Critical)];
        let (sent, state) = decide(&config, &AlertState::default(), &failing, t(12, 0));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind, NotificationKind::Firing);
        // Still failing: nothing new.
        let (sent, state) = decide(&config, &state, &failing, t(13, 0));
        assert!(sent.is_empty());
        // Recovered: one recovery note, then silence.
        let (sent, state) = decide(&config, &state, &[], t(14, 0));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].kind, NotificationKind::Resolved);
        let (sent, _) = decide(&config, &state, &[], t(15, 0));
        assert!(sent.is_empty());
    }

    #[test]
    fn quiet_hours_hold_non_critical_alerts_until_they_end() {
        let config = AlertConfig {
            quiet_hours: Some(QuietHours::parse("22:00-07:00").unwrap()),
            ..AlertConfig::default()
        };
        let warn = [signal("assertion:checks", Severity::Warning)];
        let (sent, state) = decide(&config, &AlertState::default(), &warn, t(23, 30));
        assert!(sent.is_empty());
        assert!(!state.firing["assertion:checks"].notified);
        // Critical alerts break through.
        let crit = [signal("failure", Severity::Critical)];
        let (sent, _) = decide(&config, &AlertState::default(), &crit, t(2, 0));
        assert_eq!(sent.len(), 1);
        // After quiet hours, the held alert goes out once.
        let (sent, state) = decide(&config, &state, &warn, t(7, 5));
        assert_eq!(sent.len(), 1);
        let (sent, _) = decide(&config, &state, &warn, t(8, 0));
        assert!(sent.is_empty());
        // A held alert that cleared during quiet hours never goes out.
        let (_, held) = decide(&config, &AlertState::default(), &warn, t(23, 0));
        let (sent, _) = decide(&config, &held, &[], t(9, 0));
        assert!(sent.is_empty());
    }

    #[test]
    fn severity_floor_suppresses_without_repeating() {
        let config = AlertConfig {
            min_severity: Severity::Critical,
            ..AlertConfig::default()
        };
        let warn = [signal("kpi:revenue", Severity::Warning)];
        let (sent, state) = decide(&config, &AlertState::default(), &warn, t(12, 0));
        assert!(sent.is_empty());
        let (sent, state) = decide(&config, &state, &warn, t(13, 0));
        assert!(sent.is_empty());
        let (sent, _) = decide(&config, &state, &[], t(14, 0));
        assert!(sent.is_empty(), "no recovery for an alert never sent");
    }

    fn details_with(outcome: Value) -> RunDetails {
        let mut details = RunDetails::default();
        details.assertions.insert("checks".into(), json!([outcome]));
        details
    }

    #[test]
    fn incomplete_data_never_alerts_without_opting_in() {
        let config = AlertConfig::default();
        let summary = RunSummary::default();
        let partial = details_with(
            json!({"name": "unique", "status": "fail", "evidence": {"partial": true}}),
        );
        assert!(signals(&summary, &partial, &config).is_empty());
        let opted = AlertConfig {
            include_incomplete: true,
            ..AlertConfig::default()
        };
        assert_eq!(signals(&summary, &partial, &opted).len(), 1);
        // Inconclusive checks never alert.
        let inconclusive = details_with(json!({"name": "unique", "status": "inconclusive"}));
        assert!(signals(&summary, &inconclusive, &opted).is_empty());
        // Incomplete outputs alert only with on_incomplete.
        let mut cut = RunSummary::default();
        cut.incomplete.push("orders".into());
        assert!(signals(&cut, &RunDetails::default(), &config).is_empty());
        let on = AlertConfig {
            on_incomplete: true,
            ..AlertConfig::default()
        };
        assert_eq!(
            signals(&cut, &RunDetails::default(), &on)[0].key,
            "incomplete:orders"
        );
    }

    #[test]
    fn failures_and_kpi_thresholds_signal() {
        let config = AlertConfig::default();
        let mut summary = RunSummary::default();
        summary.failed.push("orders".into());
        summary.kpis.push(KpiLine {
            cell: "revenue".into(),
            status: KpiStatus::Critical,
            value: Some(1.0),
            inconclusive: false,
        });
        summary.kpis.push(KpiLine {
            cell: "margin".into(),
            status: KpiStatus::Unknown,
            value: None,
            inconclusive: true,
        });
        let found = signals(&summary, &RunDetails::default(), &config);
        let keys: Vec<&str> = found.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["failure", "kpi:revenue"]);
        let off = AlertConfig {
            on_failure: false,
            on_kpi: false,
            ..AlertConfig::default()
        };
        assert!(signals(&summary, &RunDetails::default(), &off).is_empty());
    }

    #[test]
    fn targets_and_webhooks_are_checked() {
        assert_eq!(
            NotifyTarget::parse("desktop").unwrap(),
            NotifyTarget::Desktop
        );
        assert!(matches!(
            NotifyTarget::parse("webhook:https://hooks.example.com/x").unwrap(),
            NotifyTarget::Webhook { .. }
        ));
        for bad in [
            "webhook:http://hooks.example.com/x",
            "webhook:https://user:pw@hooks.example.com/x",
            "webhook:nonsense",
            "email:me@example.com",
            "pager",
        ] {
            assert!(NotifyTarget::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn webhook_payload_carries_names_not_values() {
        let payload = webhook_payload(
            "j_1",
            "Revenue",
            Some("r_1"),
            &[Notification {
                key: "failure".into(),
                kind: NotificationKind::Firing,
                severity: Severity::Critical,
                title: "cells did not run: orders".into(),
            }],
        );
        assert_eq!(payload["alerts"][0]["severity"], "critical");
        assert_eq!(payload["job"], "j_1");
        let mut keys: Vec<&String> = payload.as_object().unwrap().keys().collect();
        keys.sort();
        assert_eq!(keys, ["alerts", "job", "notebook", "run_id", "source"]);
    }

    #[test]
    fn desktop_commands_escape_quotes() {
        if let Some(DesktopCommand { program, args, .. }) = desktop_command("SQLLY: \"Q4\"", "a\\b")
        {
            assert!(!program.is_empty());
            let joined = args.join(" ");
            if cfg!(target_os = "macos") {
                assert!(joined.contains("\\\"Q4\\\""), "{joined}");
                assert!(joined.contains("a\\\\b"), "{joined}");
            }
        }
    }

    #[test]
    fn windows_toasts_never_put_notebook_text_in_the_script() {
        let hostile = "Q4\u{2019}); Remove-Item -Recurse C:\\; #";
        for quote in ['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'] {
            let title = format!("SQLLY: x{quote}); calc; #");
            let cmd = windows_toast_command(&title, hostile);
            let script = cmd.args.last().unwrap();
            assert!(!script.contains("calc"), "{script}");
            assert!(!script.contains("Remove-Item"), "{script}");
            assert!(!script.contains(quote) || quote == '\'', "{script}");
            let (_, sent_title) = &cmd.env[0];
            assert!(sent_title.contains("calc"));
            assert!(!sent_title.contains('<') && !sent_title.contains('\''));
        }
        let cmd = windows_toast_command("<b>&", "x");
        assert_eq!(cmd.env[0].1, "&lt;b&gt;&amp;");
    }

    #[test]
    fn quiet_hours_parse_and_wrap() {
        let q = QuietHours::parse("22:00-07:00").unwrap();
        assert!(q.contains(t(23, 0)) && q.contains(t(3, 0)) && !q.contains(t(7, 0)));
        let day = QuietHours::parse("12:00-13:00").unwrap();
        assert!(day.contains(t(12, 30)) && !day.contains(t(13, 0)));
        assert!(QuietHours::parse("late").is_err());
    }
}
