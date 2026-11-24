//! Installing and removing a job with the OS scheduler. Everything that
//! decides what to write is in `render.rs`; this file only writes the files
//! and runs the scheduler's own command, and is not run by tests.

use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};

use chrono::Utc;

use super::Job;
use super::cron::Cron;
use super::render::{self, Command};
use crate::agent::CliError;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Launchd,
    Systemd,
    Cron,
    Schtasks,
}

impl Backend {
    pub fn detect() -> Backend {
        if cfg!(target_os = "macos") {
            Backend::Launchd
        } else if cfg!(target_os = "windows") {
            Backend::Schtasks
        } else if systemd_user_available() {
            Backend::Systemd
        } else {
            Backend::Cron
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            Backend::Launchd => "a launchd agent; it runs while you are logged in to this Mac",
            Backend::Systemd => {
                "a systemd user timer; it runs while you are logged in, or whenever this computer is on once lingering is enabled"
            }
            Backend::Cron => "your crontab; it runs while this computer is on",
            Backend::Schtasks => "Task Scheduler; it runs while you are logged in to this PC",
        }
    }
}

fn systemd_user_available() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR").is_some()
        && Process::new("systemctl")
            .args(["--user", "--version"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

/// Whether systemd keeps this user's timers running after logout
/// (`loginctl` lingering); `None` when it cannot be told.
pub fn lingering() -> Option<bool> {
    let user = std::env::var("USER").ok()?;
    let out = Process::new("loginctl")
        .args(["show-user", &user, "--property=Linger", "--value"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim() == "yes")
}

/// Keep this user's timers running after logout. Only on the user's own
/// request (`--enable-linger`); polkit may ask for a password.
pub fn enable_lingering() -> Result<(), CliError> {
    run("loginctl", &["enable-linger"], None).map(|_| ())
}

/// The machine's own time zone name, when it can be told.
pub fn system_zone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ") {
        let tz = tz.trim_start_matches(':').to_string();
        if tz.parse::<chrono_tz::Tz>().is_ok() {
            return Some(tz);
        }
    }
    let target = std::fs::read_link("/etc/localtime").ok()?;
    let text = target.to_string_lossy();
    let at = text.find("zoneinfo/")?;
    let name = text[at + "zoneinfo/".len()..].to_string();
    name.parse::<chrono_tz::Tz>().is_ok().then_some(name)
}

/// Whether the job's zone differs from the machine's, so calendar-based
/// schedulers must tick instead.
pub fn foreign_zone(job: &Job) -> bool {
    match &job.tz {
        None => false,
        Some(tz) => system_zone().as_deref() != Some(tz.as_str()),
    }
}

pub fn label(job_id: &str) -> String {
    format!("app.sqlly.notebook.{job_id}")
}

fn unit_name(job_id: &str) -> String {
    format!("sqlly-notebook-{job_id}")
}

fn task_name(job_id: &str) -> String {
    format!("SQLLY\\Notebook {job_id}")
}

fn launch_agents() -> Result<PathBuf, CliError> {
    dirs::home_dir()
        .map(|h| h.join("Library").join("LaunchAgents"))
        .ok_or_else(|| CliError::io("no home directory"))
}

fn systemd_dir() -> Result<PathBuf, CliError> {
    dirs::config_dir()
        .map(|c| c.join("systemd").join("user"))
        .ok_or_else(|| CliError::io("no config directory"))
}

fn run(program: &str, args: &[&str], stdin: Option<&str>) -> Result<String, CliError> {
    use std::io::Write;
    let mut child = Process::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CliError::io(format!("cannot run {program}: {e}")))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes())
            .map_err(|e| CliError::io(e.to_string()))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| CliError::io(e.to_string()))?;
    if !output.status.success() {
        return Err(CliError::io(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn write(path: &Path, text: &str) -> Result<(), CliError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| CliError::io(e.to_string()))?;
    }
    std::fs::write(path, text)
        .map_err(|e| CliError::io(format!("cannot write {}: {e}", path.display())))
}

/// The files (or crontab line) a job installs, rendered, with where they go.
pub fn rendered(
    job: &Job,
    backend: Backend,
    program: &str,
    logs: &Path,
) -> Result<Vec<(String, String)>, CliError> {
    let cron = Cron::parse(&job.cron).map_err(CliError::usage)?;
    let foreign = foreign_zone(job);
    let description = format!("SQLLY notebook job {} ({})", job.id, job.title);
    let command = Command::run_job(program, &job.id);
    let tick = Command::tick(program);
    Ok(match backend {
        Backend::Launchd => {
            let trigger = render::launchd_trigger(&cron, foreign);
            let cmd = if matches!(trigger, render::Trigger::Tick) {
                &tick
            } else {
                &command
            };
            let log = logs.join(format!("{}.log", job.id));
            let path = launch_agents()?.join(format!("{}.plist", label(&job.id)));
            vec![(
                path.display().to_string(),
                render::launchd_plist(&label(&job.id), cmd, &trigger, &log.to_string_lossy()),
            )]
        }
        Backend::Systemd => {
            let dir = systemd_dir()?;
            let unit = unit_name(&job.id);
            vec![
                (
                    dir.join(format!("{unit}.service")).display().to_string(),
                    render::systemd_service(&description, &command),
                ),
                (
                    dir.join(format!("{unit}.timer")).display().to_string(),
                    render::systemd_timer(
                        &format!("Schedule for {description}"),
                        &format!("{unit}.service"),
                        &render::systemd_on_calendar(&cron, job.tz.as_deref()),
                    ),
                ),
            ]
        }
        Backend::Cron => vec![(
            "crontab".to_string(),
            render::crontab_line(&cron, foreign, program, &job.id),
        )],
        Backend::Schtasks => {
            let trigger = render::schtasks_trigger(&cron, foreign);
            let cmd = if matches!(trigger, render::Trigger::Tick) {
                &tick
            } else {
                &command
            };
            vec![(
                task_name(&job.id),
                render::schtasks_xml(
                    &description,
                    cmd,
                    &trigger,
                    &job.created_at.format("%Y-%m-%d").to_string(),
                ),
            )]
        }
    })
}

pub fn install(job: &Job, backend: Backend, program: &str, logs: &Path) -> Result<(), CliError> {
    let files = rendered(job, backend, program, logs)?;
    match backend {
        Backend::Launchd => {
            let (path, text) = &files[0];
            write(Path::new(path), text)?;
            let _ = run("launchctl", &["unload", path], None);
            run("launchctl", &["load", "-w", path], None)?;
        }
        Backend::Systemd => {
            for (path, text) in &files {
                write(Path::new(path), text)?;
            }
            run("systemctl", &["--user", "daemon-reload"], None)?;
            let timer = format!("{}.timer", unit_name(&job.id));
            run("systemctl", &["--user", "enable", "--now", &timer], None)?;
        }
        Backend::Cron => {
            let existing = run("crontab", &["-l"], None).unwrap_or_default();
            let updated = render::crontab_with(&existing, &job.id, Some(&files[0].1));
            run("crontab", &["-"], Some(&updated))?;
        }
        Backend::Schtasks => {
            let xml = &files[0].1;
            let path = std::env::temp_dir().join(format!(
                "sqlly-{}-{}.xml",
                job.id,
                Utc::now().timestamp()
            ));
            let mut bytes = vec![0xFF, 0xFE];
            for unit in xml.encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
            std::fs::write(&path, bytes).map_err(|e| CliError::io(e.to_string()))?;
            let result = run(
                "schtasks",
                &[
                    "/Create",
                    "/TN",
                    &task_name(&job.id),
                    "/XML",
                    &path.to_string_lossy(),
                    "/F",
                ],
                None,
            );
            let _ = std::fs::remove_file(&path);
            result?;
        }
    }
    Ok(())
}

pub fn uninstall(job: &Job, backend: Backend) -> Result<(), CliError> {
    match backend {
        Backend::Launchd => {
            let path = launch_agents()?.join(format!("{}.plist", label(&job.id)));
            let _ = run("launchctl", &["unload", &path.to_string_lossy()], None);
            let _ = std::fs::remove_file(path);
        }
        Backend::Systemd => {
            let unit = unit_name(&job.id);
            let _ = run(
                "systemctl",
                &["--user", "disable", "--now", &format!("{unit}.timer")],
                None,
            );
            let dir = systemd_dir()?;
            for ext in ["service", "timer"] {
                let _ = std::fs::remove_file(dir.join(format!("{unit}.{ext}")));
            }
            let _ = run("systemctl", &["--user", "daemon-reload"], None);
        }
        Backend::Cron => {
            let existing = run("crontab", &["-l"], None).unwrap_or_default();
            let updated = render::crontab_with(&existing, &job.id, None);
            run("crontab", &["-"], Some(&updated))?;
        }
        Backend::Schtasks => {
            run(
                "schtasks",
                &["/Delete", "/TN", &task_name(&job.id), "/F"],
                None,
            )?;
        }
    }
    Ok(())
}
