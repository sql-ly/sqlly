//! Browser stub for the local coding-agent CLI runner.

use std::future::{Future, ready};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlly_ai::catalog::{CliAgentEvent, CliAgentKind, McpServerSpec};

pub enum StopKind {
    Completed,
    Cancelled,
    TimedOut,
    OutputCapExceeded,
    SpawnFailed,
}

pub struct RunOutcome {
    pub exit_code: Option<i32>,
    pub final_text: Option<String>,
    pub is_error: bool,
    pub stderr_tail: sqlly_core::DisplayMessage,
    pub stopped: StopKind,
}

pub struct HealthReport {
    pub found: bool,
    pub version: Option<String>,
    pub mcp_ok: Option<bool>,
    pub detail: sqlly_core::DisplayMessage,
}

pub struct RunHandle;

impl RunHandle {
    pub fn cancel(&self) {}
}

pub struct RunRequest {
    pub kind: CliAgentKind,
    pub exe_path: PathBuf,
    pub extra_env: Vec<(String, String)>,
    pub extra_args: Vec<String>,
    pub prompt: String,
    pub mcp: McpServerSpec,
    pub max_turns: usize,
    pub timeout: Duration,
}

impl RunRequest {
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
        }
    }
}

pub fn start_run(
    _req: RunRequest,
    _on_event: impl FnMut(CliAgentEvent) + Send + 'static,
) -> Result<(RunHandle, std::future::Ready<RunOutcome>), String> {
    // The browser never runs a local CLI; a concrete `Ready` future keeps the
    // signature nameable (an `impl Future` return can't be inferred from a
    // body that only ever returns `Err`).
    Err(crate::localization::text(
        "ai.local_cli_agent.browser_unavailable",
    ))
}

pub fn detect_executable(_kind: CliAgentKind) -> Option<PathBuf> {
    None
}

pub async fn health_check(_kind: CliAgentKind, _exe: &Path, _mcp: &McpServerSpec) -> HealthReport {
    HealthReport {
        found: false,
        version: None,
        mcp_ok: None,
        detail: crate::browser_preview::unavailable(&crate::localization::text(
            "ai.local_cli_agent.browser_unavailable",
        ))
        .into(),
    }
}

#[allow(dead_code)]
fn _noop_future() -> impl Future<Output = RunOutcome> + Send {
    ready(RunOutcome {
        exit_code: None,
        final_text: None,
        is_error: true,
        stderr_tail: String::new().into(),
        stopped: StopKind::SpawnFailed,
    })
}
