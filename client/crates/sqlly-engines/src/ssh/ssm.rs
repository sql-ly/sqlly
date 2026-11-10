//! AWS Systems Manager Session Manager as a database transport. Like the
//! Kubernetes forward, we supervise the user's own `aws` CLI (with its
//! `session-manager-plugin`) so SSO, credential processes, and named profiles
//! behave exactly as in their terminal.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use sqlly_core::connection::SsmPortForward;
use sqlly_core::{DisplayMessage, StudioError, StudioResult};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::{oneshot, watch};
use tracing::info;

use crate::ssh::tunnel::TunnelHandle;

/// The two programs a Session Manager port-forward needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwsTool {
    /// AWS CLI v2 (`aws`).
    Cli,
    /// `session-manager-plugin`, which the CLI launches for the session.
    SessionManagerPlugin,
}

impl AwsTool {
    pub fn binary_name(self) -> &'static str {
        match (self, cfg!(windows)) {
            (Self::Cli, false) => "aws",
            (Self::Cli, true) => "aws.exe",
            (Self::SessionManagerPlugin, false) => "session-manager-plugin",
            (Self::SessionManagerPlugin, true) => "session-manager-plugin.exe",
        }
    }

    /// Install locations a GUI launch's PATH often misses.
    fn probe_dirs(self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if cfg!(windows) {
            let program_files = std::env::var_os("ProgramFiles")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
            dirs.push(match self {
                Self::Cli => program_files.join("Amazon").join("AWSCLIV2"),
                Self::SessionManagerPlugin => program_files
                    .join("Amazon")
                    .join("SessionManagerPlugin")
                    .join("bin"),
            });
            return dirs;
        }
        if cfg!(target_os = "macos") {
            dirs.push(PathBuf::from("/opt/homebrew/bin"));
        }
        dirs.push(PathBuf::from("/usr/local/bin"));
        if self == Self::SessionManagerPlugin {
            dirs.push(PathBuf::from("/usr/local/sessionmanagerplugin/bin"));
        }
        if let Some(home) = dirs::home_dir() {
            dirs.push(home.join(".local").join("bin"));
        }
        dirs
    }
}

static AWS_CLI_OVERRIDE: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();
static SSM_PLUGIN_OVERRIDE: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

/// Extra environment variables merged into the `aws` child (tests only).
#[doc(hidden)]
pub static AWS_EXTRA_ENV: OnceLock<RwLock<Vec<(String, String)>>> = OnceLock::new();

fn override_store(tool: AwsTool) -> &'static RwLock<Option<PathBuf>> {
    match tool {
        AwsTool::Cli => AWS_CLI_OVERRIDE.get_or_init(|| RwLock::new(None)),
        AwsTool::SessionManagerPlugin => SSM_PLUGIN_OVERRIDE.get_or_init(|| RwLock::new(None)),
    }
}

/// `Some(path)` wins over PATH lookup for `tool`.
pub fn set_aws_tool_override(tool: AwsTool, path: Option<PathBuf>) {
    *override_store(tool).write() = path;
}

/// First `name` in the `path_var` directories, then in `extra` directories.
fn find_in(name: &str, path_var: Option<OsString>, extra: &[PathBuf]) -> Option<PathBuf> {
    let on_path = path_var
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default();
    on_path
        .iter()
        .chain(extra.iter())
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Override → PATH → the tool's usual install directories.
pub fn locate_aws_tool(tool: AwsTool) -> Option<PathBuf> {
    if let Some(path) = override_store(tool).read().clone() {
        return Some(path);
    }
    find_in(
        tool.binary_name(),
        std::env::var_os("PATH"),
        &tool.probe_dirs(),
    )
}

/// Error shown when the AWS CLI is not installed.
pub fn aws_cli_not_found_error() -> StudioError {
    StudioError::ConnectionMessage(DisplayMessage::new(
        "services.connection.ssm.aws_cli_not_found",
        "The AWS CLI (aws) was not found. Install AWS CLI v2 to connect through AWS Session Manager.",
    ))
}

/// Error shown when the Session Manager plugin is not installed.
pub fn ssm_plugin_not_found_error() -> StudioError {
    StudioError::ConnectionMessage(DisplayMessage::new(
        "services.connection.ssm.plugin_not_found",
        "The AWS Session Manager plugin (session-manager-plugin) was not found. Install it to connect through AWS Session Manager.",
    ))
}

fn start_failed_error(detail: &str) -> StudioError {
    StudioError::ConnectionMessage(
        DisplayMessage::new(
            "services.connection.ssm.start_failed",
            format!("AWS Session Manager could not start the port-forward: {detail}"),
        )
        .with_arg("detail", detail),
    )
}

fn timed_out_error(timeout: Duration) -> StudioError {
    let seconds = timeout.as_secs().max(1).to_string();
    StudioError::ConnectionMessage(
        DisplayMessage::new(
            "services.connection.ssm.timed_out",
            format!("AWS Session Manager did not open the port-forward within {seconds} seconds."),
        )
        .with_arg("seconds", &seconds),
    )
}

/// Both programs, or the error naming the one that is missing.
pub fn preflight(
    aws: Option<PathBuf>,
    plugin: Option<PathBuf>,
) -> StudioResult<(PathBuf, PathBuf)> {
    let aws = aws.ok_or_else(aws_cli_not_found_error)?;
    let plugin = plugin.ok_or_else(ssm_plugin_not_found_error)?;
    Ok((aws, plugin))
}

/// `Port 49152 opened for sessionId …` or `Waiting for connections...`: the
/// plugin is listening on the local port.
pub fn is_ready_line(line: &str) -> bool {
    let line = line.trim();
    line.starts_with("Waiting for connections")
        || (line.starts_with("Port ") && line.contains(" opened for sessionId"))
}

/// The CLI's own words when it cannot find the plugin.
fn reports_missing_plugin(output: &str) -> bool {
    output.contains("SessionManagerPlugin is not found")
}

/// A loopback port nothing listens on right now, for `localPortNumber`.
fn free_local_port() -> StudioResult<u16> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .map_err(|error| start_failed_error(&error.to_string()))
}

/// PATH for the child with the plugin's directory first, so the CLI finds a
/// plugin we located outside the app's own PATH.
fn child_path(plugin: &Path) -> Option<OsString> {
    let dir = plugin.parent()?.to_path_buf();
    let mut dirs = vec![dir];
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(dirs).ok()
}

type Ring = Arc<Mutex<VecDeque<String>>>;

fn push_line(ring: &Ring, line: String) {
    let mut ring = ring.lock();
    if ring.len() == 20 {
        ring.pop_front();
    }
    ring.push_back(line);
}

fn ring_text(ring: &Ring) -> String {
    ring.lock().iter().cloned().collect::<Vec<_>>().join("\n")
}

fn read_lines<R: AsyncRead + Unpin + Send + 'static>(
    stream: R,
    ring: Ring,
    ready: Option<oneshot::Sender<()>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream).lines();
        let mut ready = ready;
        while let Ok(Some(line)) = reader.next_line().await {
            if is_ready_line(&line)
                && let Some(tx) = ready.take()
            {
                let _ = tx.send(());
            }
            push_line(&ring, line);
        }
    })
}

/// Start `aws ssm start-session` forwarding a free local port to
/// `remote_host:remote_port` through `cfg.target`, wait (bounded by
/// `timeout`) until the plugin listens, and return the handle. Dropping the
/// handle ends the CLI and its plugin.
pub async fn open_ssm_forward(
    cfg: &SsmPortForward,
    remote_host: &str,
    remote_port: u16,
    timeout: Duration,
) -> StudioResult<TunnelHandle> {
    let errors = cfg.validate();
    if !errors.is_empty() {
        return Err(StudioError::Connection(errors.join("; ")));
    }
    let (aws, plugin) = preflight(
        locate_aws_tool(AwsTool::Cli),
        locate_aws_tool(AwsTool::SessionManagerPlugin),
    )?;

    let local_port = free_local_port()?;
    let mut cmd = Command::new(&aws);
    cmd.args(cfg.aws_args(remote_host, remote_port, local_port))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(path) = child_path(&plugin) {
        cmd.env("PATH", path);
    }
    // Its own process group, so teardown also reaches the plugin the CLI
    // starts (killing only the CLI would leave the port forwarding).
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000);
    }
    if let Some(store) = AWS_EXTRA_ENV.get() {
        for (key, value) in store.read().iter() {
            cmd.env(key, value);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|error| start_failed_error(&error.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| start_failed_error("no stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| start_failed_error("no stderr"))?;

    let output: Ring = Arc::new(Mutex::new(VecDeque::with_capacity(20)));
    let (ready_tx, ready_rx) = oneshot::channel::<()>();
    let stdout_task = read_lines(stdout, Arc::clone(&output), Some(ready_tx));
    let stderr_task = read_lines(stderr, Arc::clone(&output), None);

    let failure = |output: &Ring, fallback: String| {
        let detail = ring_text(output);
        if reports_missing_plugin(&detail) {
            ssm_plugin_not_found_error()
        } else if detail.trim().is_empty() {
            start_failed_error(&fallback)
        } else {
            start_failed_error(detail.trim())
        }
    };

    // The ready line arrives on stdout; stdout closing first means the CLI
    // is exiting, and its own message explains why.
    let waited = tokio::time::timeout(timeout, async {
        match ready_rx.await {
            Ok(()) => Ok(()),
            Err(_) => {
                let status = child
                    .wait()
                    .await
                    .map(|s| s.to_string())
                    .unwrap_or_else(|e| e.to_string());
                let _ = tokio::time::timeout(Duration::from_millis(200), async {
                    while !stderr_task.is_finished() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await;
                Err(failure(&output, status))
            }
        }
    })
    .await;

    match waited {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            if let Some(pid) = child.id() {
                crate::ssh::tunnel::kill_process_tree(pid);
            }
            let _ = child.start_kill();
            let detail = ring_text(&output);
            if reports_missing_plugin(&detail) {
                return Err(ssm_plugin_not_found_error());
            }
            return Err(timed_out_error(timeout));
        }
    }

    info!(
        local_port,
        target = %cfg.summary(),
        remote_host,
        remote_port,
        "AWS Session Manager port-forward ready"
    );

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let pump = tokio::spawn(async move {
        tokio::select! {
            _ = stdout_task => {}
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    return;
                }
            }
        }
        let _ = stderr_task.await;
    });

    Ok(TunnelHandle::from_supervised_process_tree(
        local_port,
        shutdown_tx,
        pump,
        child,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_lines_from_the_plugin() {
        assert!(is_ready_line(
            "Port 49152 opened for sessionId jane-0a1b2c3d4e5f."
        ));
        assert!(is_ready_line("Waiting for connections..."));
        assert!(!is_ready_line("Starting session with SessionId: jane-0a1b"));
        assert!(!is_ready_line(
            "Connection accepted for session [jane-0a1b]"
        ));
    }

    #[test]
    fn missing_aws_cli_is_reported_first() {
        let err = preflight(None, None).unwrap_err();
        assert!(
            err.to_string().contains("AWS CLI (aws) was not found"),
            "{err}"
        );
        let StudioError::ConnectionMessage(message) = err else {
            panic!("expected a localized message");
        };
        assert_eq!(
            message.key(),
            Some("services.connection.ssm.aws_cli_not_found")
        );
    }

    #[test]
    fn missing_plugin_is_reported_when_the_cli_is_present() {
        let err = preflight(Some(PathBuf::from("/usr/local/bin/aws")), None).unwrap_err();
        assert!(err.to_string().contains("session-manager-plugin"), "{err}");
        let (aws, plugin) = preflight(
            Some(PathBuf::from("/a/aws")),
            Some(PathBuf::from("/b/session-manager-plugin")),
        )
        .unwrap();
        assert_eq!(aws, PathBuf::from("/a/aws"));
        assert_eq!(plugin, PathBuf::from("/b/session-manager-plugin"));
    }

    #[test]
    fn the_cli_message_for_a_missing_plugin_is_recognised() {
        assert!(reports_missing_plugin(
            "\nSessionManagerPlugin is not found. Please refer to SessionManager Documentation here: https://docs.aws.amazon.com/..."
        ));
        assert!(!reports_missing_plugin(
            "An error occurred (TargetNotConnected)"
        ));
    }

    #[test]
    fn find_in_checks_path_then_extra_dirs() {
        let path_dir = tempfile::tempdir().unwrap();
        let extra_dir = tempfile::tempdir().unwrap();
        let name = AwsTool::SessionManagerPlugin.binary_name();
        let extra = vec![extra_dir.path().to_path_buf()];
        assert_eq!(find_in(name, Some(path_dir.path().into()), &extra), None);
        std::fs::write(extra_dir.path().join(name), b"").unwrap();
        assert_eq!(
            find_in(name, Some(path_dir.path().into()), &extra),
            Some(extra_dir.path().join(name))
        );
        std::fs::write(path_dir.path().join(name), b"").unwrap();
        assert_eq!(
            find_in(name, Some(path_dir.path().into()), &extra),
            Some(path_dir.path().join(name))
        );
    }

    #[test]
    fn binary_names_match_platform() {
        if cfg!(windows) {
            assert_eq!(AwsTool::Cli.binary_name(), "aws.exe");
        } else {
            assert_eq!(AwsTool::Cli.binary_name(), "aws");
            assert_eq!(
                AwsTool::SessionManagerPlugin.binary_name(),
                "session-manager-plugin"
            );
        }
    }

    #[test]
    fn child_path_puts_the_plugin_directory_first() {
        let path = child_path(Path::new("/opt/ssm/bin/session-manager-plugin")).unwrap();
        let first = std::env::split_paths(&path).next().unwrap();
        assert_eq!(first, PathBuf::from("/opt/ssm/bin"));
    }

    #[tokio::test]
    async fn invalid_config_fails_before_looking_for_the_cli() {
        let cfg = SsmPortForward::default();
        let err = match open_ssm_forward(&cfg, "db", 5432, Duration::from_secs(1)).await {
            Err(err) => err,
            Ok(_) => panic!("expected a validation error"),
        };
        assert!(err.to_string().contains("target is required"), "{err}");
    }
}
