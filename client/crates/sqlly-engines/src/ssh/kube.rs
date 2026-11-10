//! `kubectl port-forward` as a database transport. We supervise the user's own
//! kubectl instead of speaking to the API server: kubeconfig exec plugins,
//! proxies, and cloud CLIs then behave exactly as in their terminal.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use sqlly_core::connection::KubePortForward;
use sqlly_core::{StudioError, StudioResult};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{oneshot, watch};
use tracing::info;

use crate::ssh::tunnel::TunnelHandle;

static KUBECTL_OVERRIDE: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

/// Extra environment variables merged into the kubectl child (tests only).
#[doc(hidden)]
pub static KUBECTL_EXTRA_ENV: OnceLock<RwLock<Vec<(String, String)>>> = OnceLock::new();

fn override_store() -> &'static RwLock<Option<PathBuf>> {
    KUBECTL_OVERRIDE.get_or_init(|| RwLock::new(None))
}

/// Preference hook: `Some(path)` wins over PATH lookup. Called by the GPUI
/// preferences layer at startup and on change.
pub fn set_kubectl_override(path: Option<PathBuf>) {
    *override_store().write() = path;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KubectlLocation {
    Override(PathBuf),
    OnPath(PathBuf),
    NotFound,
}

fn kubectl_binary_name() -> &'static str {
    if cfg!(windows) {
        "kubectl.exe"
    } else {
        "kubectl"
    }
}

fn probe_kubectl_in_dir(dir: &Path) -> Option<PathBuf> {
    let name = kubectl_binary_name();
    let candidate = dir.join(name);
    candidate.is_file().then_some(candidate)
}

fn extra_probe_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local").join("bin"));
    }
    dirs
}

/// Override → each `std::env::split_paths(PATH)` dir joined with `kubectl`
/// (`kubectl.exe` on Windows) → common GUI-launch gaps on macOS/Linux.
pub fn locate_kubectl() -> KubectlLocation {
    if let Some(path) = override_store().read().clone() {
        return KubectlLocation::Override(path);
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if let Some(found) = probe_kubectl_in_dir(&dir) {
                return KubectlLocation::OnPath(found);
            }
        }
    }
    for dir in extra_probe_dirs() {
        if let Some(found) = probe_kubectl_in_dir(&dir) {
            return KubectlLocation::OnPath(found);
        }
    }
    KubectlLocation::NotFound
}

/// `Forwarding from 127.0.0.1:49152 -> 5432` → `Some(49152)`. Also accepts
/// `[::1]:PORT` lines and ignores everything else.
pub fn parse_forwarding_line(line: &str) -> Option<u16> {
    let line = line.trim();
    let rest = line.strip_prefix("Forwarding from ")?;
    let (addr, _) = rest.split_once(" -> ")?;
    let port_str = addr.rsplit(':').next()?;
    port_str.parse().ok()
}

fn kubectl_not_found_error() -> StudioError {
    StudioError::Connection(
        "kubectl was not found on PATH. Install kubectl or set its location in Preferences → Advanced → Kubernetes.".to_string(),
    )
}

fn merge_extra_env(cmd: &mut Command) {
    if let Some(store) = KUBECTL_EXTRA_ENV.get() {
        for (key, value) in store.read().iter() {
            cmd.env(key, value);
        }
    }
}

/// Spawn kubectl, wait for the first `Forwarding from 127.0.0.1:` line (bounded
/// by `timeout`), and return the handle.
pub async fn open_port_forward(
    cfg: &KubePortForward,
    timeout: Duration,
) -> StudioResult<TunnelHandle> {
    let errors = cfg.validate();
    if !errors.is_empty() {
        return Err(StudioError::Connection(errors.join("; ")));
    }

    let bin = match locate_kubectl() {
        KubectlLocation::Override(p) | KubectlLocation::OnPath(p) => p,
        KubectlLocation::NotFound => return Err(kubectl_not_found_error()),
    };

    let mut cmd = Command::new(&bin);
    cmd.args(cfg.kubectl_args())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000);
    }
    merge_extra_env(&mut cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| StudioError::Connection(format!("kubectl port-forward failed: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| StudioError::Connection("kubectl port-forward failed: no stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| StudioError::Connection("kubectl port-forward failed: no stderr".into()))?;

    let stderr_ring: Arc<Mutex<VecDeque<String>>> =
        Arc::new(Mutex::new(VecDeque::with_capacity(20)));
    let stderr_ring_reader = Arc::clone(&stderr_ring);
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = reader.next_line().await {
            let mut ring = stderr_ring_reader.lock();
            if ring.len() == 20 {
                ring.pop_front();
            }
            ring.push_back(line);
        }
    });

    let (ready_tx, ready_rx) = oneshot::channel::<u16>();
    let stdout_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        let mut ready_tx = Some(ready_tx);
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(port) = parse_forwarding_line(&line)
                && let Some(tx) = ready_tx.take()
            {
                let _ = tx.send(port);
            }
        }
    });

    let local_port = tokio::time::timeout(timeout, async {
        async fn stderr_detail(
            ring: &Arc<Mutex<VecDeque<String>>>,
            stderr_task: &tokio::task::JoinHandle<()>,
        ) -> String {
            let _ = tokio::time::timeout(Duration::from_millis(200), async {
                while ring.lock().is_empty() && !stderr_task.is_finished() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            ring.lock().iter().cloned().collect::<Vec<_>>().join("\n")
        }

        tokio::select! {
            port = ready_rx => match port {
                Ok(port) => Ok(port),
                Err(_) => {
                    let detail = stderr_detail(&stderr_ring, &stderr_task).await;
                    let msg = if detail.is_empty() {
                        "kubectl port-forward failed: process exited before becoming ready".to_string()
                    } else {
                        format!("kubectl port-forward failed: {detail}")
                    };
                    Err(StudioError::Connection(msg))
                }
            },
            status = child.wait() => {
                let detail = stderr_detail(&stderr_ring, &stderr_task).await;
                let status = status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|e| e.to_string());
                let msg = if detail.is_empty() {
                    format!("kubectl port-forward failed: {status}")
                } else {
                    format!("kubectl port-forward failed: {detail}")
                };
                Err(StudioError::Connection(msg))
            }
        }
    })
    .await
    .map_err(|_| {
        let _ = child.start_kill();
        let detail = stderr_ring.lock().iter().cloned().collect::<Vec<_>>().join("\n");
        let mut msg = format!("kubectl port-forward did not become ready within {timeout:?}");
        if !detail.is_empty() {
            msg.push('\n');
            msg.push_str(&detail);
        }
        StudioError::Connection(msg)
    })??;

    info!(
        local_port,
        target = %cfg.summary(),
        "kubectl port-forward ready"
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

    Ok(TunnelHandle::from_supervised_process(
        local_port,
        shutdown_tx,
        pump,
        Some(child),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forwarding_line_ipv4_and_ipv6() {
        assert_eq!(
            parse_forwarding_line("Forwarding from 127.0.0.1:49152 -> 5432"),
            Some(49152)
        );
        assert_eq!(
            parse_forwarding_line("Forwarding from [::1]:45999 -> 5432"),
            Some(45999)
        );
    }

    #[test]
    fn parse_forwarding_line_ignores_handling_connection_and_noise() {
        assert_eq!(parse_forwarding_line("Handling connection for 49152"), None);
        assert_eq!(parse_forwarding_line("kubectl help"), None);
    }

    #[test]
    fn locate_kubectl_prefers_override() {
        let path =
            std::env::temp_dir().join(format!("sqlly-kubectl-override-{}", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        set_kubectl_override(Some(path.clone()));
        assert_eq!(locate_kubectl(), KubectlLocation::Override(path));
        set_kubectl_override(None);
    }

    #[test]
    fn kubectl_binary_name_matches_platform() {
        if cfg!(windows) {
            assert_eq!(kubectl_binary_name(), "kubectl.exe");
        } else {
            assert_eq!(kubectl_binary_name(), "kubectl");
        }
    }
}
