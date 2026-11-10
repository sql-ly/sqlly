//! Desktop runner for headless coding-agent CLIs. Spawns the vendor binary with
//! a temp MCP config pointing at `sqlly-cli mcp`, scrubs credential-shaped env
//! vars, and streams JSONL events back to the AI drawer.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlly_ai::catalog::{
    CliAgentEvent, CliAgentKind, LaunchSpec, MAX_EVENT_BYTES, MAX_TOTAL_OUTPUT_BYTES,
    McpServerSpec, SCRUBBED_ENV_KEYS, SCRUBBED_ENV_PREFIXES, build_launch, parse_events_for_kind,
    validate_env_key,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    Completed,
    Cancelled,
    TimedOut,
    OutputCapExceeded,
    SpawnFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub exit_code: Option<i32>,
    pub final_text: Option<String>,
    pub is_error: bool,
    /// The CLI's own stderr (raw), or SQLLY's keyed reason when the run
    /// never started properly. Built on the runtime, so the UI thread words
    /// it with [`crate::localization::message`].
    pub stderr_tail: sqlly_core::DisplayMessage,
    pub stopped: StopKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthReport {
    pub found: bool,
    pub version: Option<String>,
    pub mcp_ok: Option<bool>,
    /// Built on the runtime; worded on the UI thread with
    /// [`crate::localization::message`].
    pub detail: sqlly_core::DisplayMessage,
}

pub struct RunHandle {
    cancel: Arc<AtomicBool>,
    #[cfg(test)]
    pub(crate) raw_lines: Arc<Mutex<Vec<String>>>,
}

impl RunHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone)]
pub struct RunRequest {
    pub kind: CliAgentKind,
    pub exe_path: PathBuf,
    pub extra_env: Vec<(String, String)>,
    pub extra_args: Vec<String>,
    pub prompt: String,
    pub mcp: McpServerSpec,
    pub max_turns: usize,
    pub timeout: Duration,
    #[cfg(test)]
    raw_launch: Option<LaunchSpec>,
}

impl RunRequest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: CliAgentKind,
        exe_path: PathBuf,
        extra_env: Vec<(String, String)>,
        extra_args: Vec<String>,
        prompt: String,
        mcp: McpServerSpec,
        max_turns: usize,
        timeout: Duration,
    ) -> Self {
        Self {
            kind,
            exe_path,
            extra_env,
            extra_args,
            prompt,
            mcp,
            max_turns,
            timeout,
            #[cfg(test)]
            raw_launch: None,
        }
    }

    #[cfg(test)]
    pub fn raw(program: PathBuf, args: Vec<String>, extra_env: Vec<(String, String)>) -> Self {
        Self {
            kind: CliAgentKind::Codex,
            exe_path: program.clone(),
            extra_env: extra_env.clone(),
            extra_args: Vec::new(),
            prompt: String::new(),
            mcp: McpServerSpec {
                name: "sqlly".to_string(),
                command: "sqlly-cli".to_string(),
                args: vec!["mcp".to_string()],
            },
            max_turns: 1,
            timeout: Duration::from_secs(30),
            raw_launch: Some(LaunchSpec {
                program,
                args,
                env: extra_env,
                stdin_prompt: None,
                cwd: None,
            }),
        }
    }
}

/// Must be called on the UI thread: it resolves the user-facing text the run
/// needs here, because the returned future runs on a tokio worker where the
/// thread-local language is never set (it would always render English).
pub fn start_run(
    req: RunRequest,
    on_event: impl FnMut(CliAgentEvent) + Send + 'static,
) -> Result<(RunHandle, impl Future<Output = RunOutcome> + Send), String> {
    if !crate::distribution::can_launch_user_programs() {
        return Err(crate::distribution::sandbox_blocked_message(
            crate::localization::text("ai.local_cli_agent.sandbox_action"),
        ));
    }
    for (key, _) in &req.extra_env {
        validate_env_key(key)?;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let raw_lines = Arc::new(Mutex::new(Vec::new()));
    let handle = RunHandle {
        cancel: Arc::clone(&cancel),
        #[cfg(test)]
        raw_lines: Arc::clone(&raw_lines),
    };
    let truncated_marker = crate::localization::text("ai.local_cli_agent.output_truncated_marker");
    let future = run_inner(req, cancel, raw_lines, truncated_marker, on_event);
    Ok((handle, future))
}

pub fn detect_executable(kind: CliAgentKind) -> Option<PathBuf> {
    if !crate::distribution::can_launch_user_programs() {
        return None;
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in kind.exe_candidates() {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

pub async fn health_check(kind: CliAgentKind, exe: &Path, mcp: &McpServerSpec) -> HealthReport {
    if !exe.is_file() {
        return HealthReport {
            found: false,
            version: None,
            mcp_ok: None,
            detail: crate::localization::deferred(
                "ai.local_cli_agent.executable_not_found",
                &[("path", &exe.display().to_string())],
            ),
        };
    }
    let version = match tokio::time::timeout(
        Duration::from_secs(10),
        Command::new(exe).arg("--version").output(),
    )
    .await
    {
        Ok(Ok(output)) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_string),
        _ => None,
    };
    let mcp_ok = Arc::new(Mutex::new(None::<bool>));
    let probe = RunRequest::new(
        kind,
        exe.to_path_buf(),
        Vec::new(),
        Vec::new(),
        "Reply with the single word OK.".to_string(),
        mcp.clone(),
        1,
        Duration::from_secs(60),
    );
    let mcp_ok_capture = Arc::clone(&mcp_ok);
    if let Ok((_, future)) = start_run(probe, move |event| {
        if let CliAgentEvent::Started { mcp_ok: status, .. } = event {
            *mcp_ok_capture.lock().unwrap() = status;
        }
    }) {
        let _ = future.await;
    }
    let mcp_ok = *mcp_ok.lock().unwrap();
    let detail_key = match (version.as_deref(), mcp_ok) {
        (Some(_), Some(true)) => "ai.local_cli_agent.health_version_mcp_ok",
        (Some(_), Some(false)) => "ai.local_cli_agent.health_version_mcp_failed",
        (Some(_), None) if kind == CliAgentKind::Codex => {
            "ai.local_cli_agent.health_version_mcp_ok"
        }
        (Some(_), None) => "ai.local_cli_agent.health_version_mcp_unknown",
        (None, Some(true)) => "ai.local_cli_agent.health_mcp_ok",
        (None, Some(false)) => "ai.local_cli_agent.health_mcp_failed",
        (None, None) => "ai.local_cli_agent.health_found",
    };
    let detail = if let Some(version) = version.as_deref() {
        crate::localization::deferred(detail_key, &[("version", version)])
    } else {
        crate::localization::deferred_text(detail_key)
    };
    HealthReport {
        found: true,
        version,
        mcp_ok,
        detail,
    }
}

async fn run_inner(
    req: RunRequest,
    cancel: Arc<AtomicBool>,
    raw_lines: Arc<Mutex<Vec<String>>>,
    truncated_marker: String,
    mut on_event: impl FnMut(CliAgentEvent) + Send + 'static,
) -> RunOutcome {
    #[cfg(test)]
    let (launch, _mcp_temp) = if let Some(raw) = req.raw_launch.clone() {
        (raw, None)
    } else {
        match prepare_launch(&req) {
            Ok(pair) => pair,
            Err(message) => {
                return RunOutcome {
                    exit_code: None,
                    final_text: None,
                    is_error: true,
                    stderr_tail: message,
                    stopped: StopKind::SpawnFailed,
                };
            }
        }
    };
    #[cfg(not(test))]
    let (launch, _mcp_temp) = match prepare_launch(&req) {
        Ok(pair) => pair,
        Err(message) => {
            return RunOutcome {
                exit_code: None,
                final_text: None,
                is_error: true,
                stderr_tail: message,
                stopped: StopKind::SpawnFailed,
            };
        }
    };

    let mut command = Command::new(&launch.program);
    command
        .args(&launch.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &launch.cwd {
        command.current_dir(cwd);
    }
    scrub_command_env(&mut command);
    for (key, value) in launch.env.iter().chain(req.extra_env.iter()) {
        command.env(key, value);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.as_std_mut().process_group(0);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return RunOutcome {
                exit_code: None,
                final_text: None,
                is_error: true,
                stderr_tail: crate::localization::deferred(
                    "ai.local_cli_agent.start_failed",
                    &[
                        ("path", &req.exe_path.display().to_string()),
                        ("error", &error.to_string()),
                    ],
                ),
                stopped: StopKind::SpawnFailed,
            };
        }
    };

    if let Some(prompt) = launch.stdin_prompt.as_deref()
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(prompt.as_bytes()).await;
    }

    let pid = child.id();
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let stderr_task = {
        let stderr_tail = Arc::clone(&stderr_tail);
        let mut stderr = child.stderr.take();
        tokio::spawn(async move {
            if let Some(mut stderr) = stderr.take() {
                let mut buf = vec![0u8; 4096];
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let chunk = String::from_utf8_lossy(&buf[..n]);
                            let mut tail = stderr_tail.lock().unwrap();
                            tail.push_str(&chunk);
                            if tail.len() > 8 * 1024 {
                                let drop = tail.len() - 8 * 1024;
                                tail.replace_range(..drop, "");
                            }
                        }
                    }
                }
            }
        })
    };

    let mut stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
    let mut total_bytes = 0usize;
    let mut parse_failures = 0usize;
    let mut last_assistant = None::<String>;
    let mut final_event = None::<CliAgentEvent>;
    let timeout = tokio::time::sleep(req.timeout);
    tokio::pin!(timeout);

    loop {
        if cancel.load(Ordering::Relaxed) {
            terminate_child(&mut child, pid).await;
            let _ = stderr_task.await;
            return RunOutcome {
                exit_code: Some(143),
                final_text: last_assistant.clone(),
                is_error: false,
                stderr_tail: stderr_tail.lock().unwrap().clone().into(),
                stopped: StopKind::Cancelled,
            };
        }

        let mut line = String::new();
        tokio::select! {
            _ = &mut timeout => {
                terminate_child(&mut child, pid).await;
                let _ = stderr_task.await;
                return RunOutcome {
                    exit_code: None,
                    final_text: last_assistant.clone(),
                    is_error: true,
                    stderr_tail: stderr_tail.lock().unwrap().clone().into(),
                    stopped: StopKind::TimedOut,
                };
            }
            read = stdout.read_line(&mut line) => {
                match read {
                    Ok(0) => break,
                    Ok(_) => {
                        raw_lines.lock().unwrap().push(line.clone());
                        if line.len() > MAX_EVENT_BYTES {
                            line.truncate(MAX_EVENT_BYTES);
                            line.push_str(&truncated_marker);
                        }
                        total_bytes += line.len();
                        if total_bytes > MAX_TOTAL_OUTPUT_BYTES {
                            terminate_child(&mut child, pid).await;
                            let _ = stderr_task.await;
                            return RunOutcome {
                                exit_code: None,
                                final_text: last_assistant,
                                is_error: true,
                                stderr_tail: stderr_tail.lock().unwrap().clone().into(),
                                stopped: StopKind::OutputCapExceeded,
                            };
                        }
                        match parse_events_for_kind(req.kind, line.trim_end()) {
                            Ok(events) => {
                                parse_failures = 0;
                                for event in events {
                                    if let CliAgentEvent::AssistantText(text) = &event {
                                        last_assistant = Some(text.clone());
                                    }
                                    if let CliAgentEvent::Final { .. } = &event {
                                        final_event = Some(event.clone());
                                    }
                                    on_event(event);
                                }
                            }
                            Err(_) => {
                                parse_failures += 1;
                                on_event(CliAgentEvent::Ignored);
                                if parse_failures > 20 {
                                    terminate_child(&mut child, pid).await;
                                    let _ = stderr_task.await;
                                    return RunOutcome {
                                        exit_code: None,
                                        final_text: None,
                                        is_error: true,
                                        stderr_tail: crate::localization::deferred_text(
                                            "ai.local_cli_agent.invalid_stream_json",
                                        ),
                                        stopped: StopKind::Completed,
                                    };
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    }

    let status = child.wait().await.ok();
    let _ = stderr_task.await;
    let exit_code = status.and_then(|status| status.code());
    let stderr_text = stderr_tail.lock().unwrap().clone();

    if let Some(CliAgentEvent::Final { text, is_error }) = final_event {
        return RunOutcome {
            exit_code,
            final_text: Some(text),
            is_error,
            stderr_tail: stderr_text.into(),
            stopped: StopKind::Completed,
        };
    }

    let is_error = exit_code.is_some_and(|code| code != 0);
    RunOutcome {
        exit_code,
        final_text: if is_error {
            Some(if stderr_text.is_empty() {
                last_assistant.unwrap_or_default()
            } else {
                stderr_text.clone()
            })
        } else {
            last_assistant
        },
        is_error,
        stderr_tail: stderr_text.into(),
        stopped: if exit_code == Some(143) {
            StopKind::Cancelled
        } else {
            StopKind::Completed
        },
    }
}

fn prepare_launch(
    req: &RunRequest,
) -> Result<(LaunchSpec, Option<tempfile::NamedTempFile>), sqlly_core::DisplayMessage> {
    // Runs on the runtime: the I/O error text stays raw inside a keyed
    // sentence the UI thread words.
    let temp = write_mcp_config(&req.mcp).map_err(|detail| {
        crate::localization::deferred(
            "ai.local_cli_agent.mcp_config_failed",
            &[("detail", detail)],
        )
    })?;
    let mut spec = build_launch(
        req.kind,
        &req.exe_path,
        temp.path(),
        &req.mcp,
        &req.prompt,
        &req.extra_args,
        req.max_turns,
    );
    spec.env = req.extra_env.clone();
    Ok((spec, Some(temp)))
}

fn write_mcp_config(mcp: &McpServerSpec) -> Result<tempfile::NamedTempFile, String> {
    let tmp_dir = sqly_tmp_dir();
    std::fs::create_dir_all(&tmp_dir).map_err(|error| error.to_string())?;
    let file = tempfile::Builder::new()
        .prefix("sqlly-mcp-")
        .suffix(".json")
        .tempfile_in(&tmp_dir)
        .map_err(|error| error.to_string())?;
    let body = serde_json::json!({
        "mcpServers": {
            mcp.name.clone(): {
                "type": "stdio",
                "command": mcp.command,
                "args": mcp.args,
            }
        }
    });
    let bytes = serde_json::to_vec_pretty(&body).map_err(|error| error.to_string())?;
    std::fs::write(file.path(), bytes).map_err(|error| error.to_string())?;
    tighten_owner_only_file(file.path());
    Ok(file)
}

fn sqly_tmp_dir() -> PathBuf {
    dirs::data_dir()
        .map(|dir| dir.join("SQLLY").join("tmp"))
        .unwrap_or_else(|| PathBuf::from("tmp"))
}

#[cfg(unix)]
fn tighten_owner_only_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o077));
        }
    }
}

#[cfg(not(unix))]
fn tighten_owner_only_file(_path: &Path) {}

fn scrub_command_env(command: &mut Command) {
    for key in SCRUBBED_ENV_KEYS {
        command.env_remove(key);
    }
    for (key, _) in std::env::vars() {
        if SCRUBBED_ENV_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
        {
            command.env_remove(&key);
        }
    }
}

// pid drives the cfg(unix) process-group kill; unused on other targets.
#[cfg_attr(not(unix), allow(unused_variables))]
async fn terminate_child(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    }
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    // ENV_LOCK serializes whole tests that mutate process env vars; holding the
    // std guard across .await is deliberate serialization, not a deadlock risk.
    #![allow(clippy::await_holding_lock)]
    use super::*;
    use std::sync::Mutex as StdMutex;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    fn shell_mcp() -> McpServerSpec {
        McpServerSpec {
            name: "sqlly".to_string(),
            command: "sqlly-cli".to_string(),
            args: vec!["mcp".to_string()],
        }
    }

    // These tests drive `/bin/sh`, `env`, and Unix process groups — none of
    // which exist on Windows — so they skip at runtime off unix. They stay
    // compiled everywhere so the shared `#[cfg(test)]` capture harness they use
    // isn't flagged as dead code on non-unix targets.
    #[tokio::test]
    #[cfg_attr(
        feature = "app-store",
        ignore = "the App Store edition never launches agent CLIs"
    )]
    async fn cancel_kills_child_and_its_group() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        if !cfg!(unix) {
            return;
        }
        let req = RunRequest::raw(
            PathBuf::from("/bin/sh"),
            vec!["-c".to_string(), "sleep 30 & sleep 30; wait".to_string()],
            Vec::new(),
        );
        let (handle, future) = start_run(req, |_| {}).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle.cancel();
        let outcome = tokio::time::timeout(Duration::from_secs(3), future)
            .await
            .expect("cancel should finish quickly");
        assert_eq!(outcome.stopped, StopKind::Cancelled);
    }

    #[tokio::test]
    #[cfg_attr(
        feature = "app-store",
        ignore = "the App Store edition never launches agent CLIs"
    )]
    async fn timeout_stops_run() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        if !cfg!(unix) {
            return;
        }
        let mut req = RunRequest::raw(
            PathBuf::from("/bin/sh"),
            vec!["-c".to_string(), "sleep 30".to_string()],
            Vec::new(),
        );
        req.timeout = Duration::from_millis(300);
        let (_handle, future) = start_run(req, |_| {}).unwrap();
        let outcome = future.await;
        assert_eq!(outcome.stopped, StopKind::TimedOut);
    }

    #[tokio::test]
    #[cfg_attr(
        feature = "app-store",
        ignore = "the App Store edition never launches agent CLIs"
    )]
    async fn output_cap_kills_run() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        if !cfg!(unix) {
            return;
        }
        let req = RunRequest::raw(
            PathBuf::from("/bin/sh"),
            vec![
                "-c".to_string(),
                r#"while true; do printf '{"type":"x"}\n'; done"#.to_string(),
            ],
            Vec::new(),
        );
        let (_handle, future) = start_run(req, |_| {}).unwrap();
        let outcome = future.await;
        assert_eq!(outcome.stopped, StopKind::OutputCapExceeded);
    }

    #[cfg(feature = "app-store")]
    #[test]
    fn app_store_edition_refuses_to_launch_agent_clis() {
        let req = RunRequest::new(
            CliAgentKind::ClaudeCode,
            PathBuf::from("/no/such/binary"),
            Vec::new(),
            Vec::new(),
            "hi".to_string(),
            shell_mcp(),
            1,
            Duration::from_secs(5),
        );
        let Err(message) = start_run(req, |_| {}) else {
            panic!("the App Store edition must not launch agent CLIs");
        };
        assert!(message.contains("Mac App Store edition"), "{message}");
        assert!(detect_executable(CliAgentKind::ClaudeCode).is_none());
    }

    #[tokio::test]
    #[cfg_attr(
        feature = "app-store",
        ignore = "the App Store edition never launches agent CLIs"
    )]
    async fn spawn_failure_reports_path() {
        let req = RunRequest::new(
            CliAgentKind::ClaudeCode,
            PathBuf::from("/no/such/binary"),
            Vec::new(),
            Vec::new(),
            "hi".to_string(),
            shell_mcp(),
            1,
            Duration::from_secs(5),
        );
        let (_handle, future) = start_run(req, |_| {}).unwrap();
        let outcome = future.await;
        assert_eq!(outcome.stopped, StopKind::SpawnFailed);
        assert!(outcome.stderr_tail.contains("/no/such/binary"));
    }

    #[tokio::test]
    #[cfg_attr(
        feature = "app-store",
        ignore = "the App Store edition never launches agent CLIs"
    )]
    async fn no_secret_tripwire_on_spawned_env() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
        if !cfg!(unix) {
            return;
        }
        unsafe {
            std::env::set_var("PGPASSWORD", "secret");
            std::env::set_var("MYSQL_PWD", "secret");
            std::env::set_var("SQLLY_TEST_SECRET", "secret");
        }
        let req = RunRequest::raw(
            PathBuf::from("/bin/sh"),
            vec!["-c".to_string(), "env".to_string()],
            vec![("ANTHROPIC_MODEL".to_string(), "test".to_string())],
        );
        let (handle, future) = start_run(req, |_| {}).unwrap();
        let _ = future.await;
        let captured = handle.raw_lines.lock().unwrap().join("\n");
        unsafe {
            std::env::remove_var("PGPASSWORD");
            std::env::remove_var("MYSQL_PWD");
            std::env::remove_var("SQLLY_TEST_SECRET");
        }
        assert!(!captured.contains("PGPASSWORD"));
        assert!(!captured.contains("MYSQL_PWD"));
        assert!(!captured.contains("SQLLY_TEST_SECRET"));
        assert!(captured.contains("ANTHROPIC_MODEL=test"));
    }
}
