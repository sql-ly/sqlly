#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::connection::KubeTargetKind;
use sqlly_engines::ssh::{KubectlLocation, locate_kubectl};

/// `namespace/data` → `data`; `pod/postgres-0` → `postgres-0`; lines without `/` are kept as-is; blank lines dropped.
pub fn parse_names(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.rsplit_once('/')
                .map(|(_, name)| name)
                .unwrap_or(line)
                .to_string()
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryKind {
    Contexts,
    Namespaces,
    Targets(KubeTargetKind),
}

/// argv for kubectl: contexts → `config get-contexts -o name`; namespaces → `get ns -o name`;
/// targets → `get {pods|svc|deploy} -n NS -o name`; always prefixed by `--kubeconfig`/`--context` when set.
pub fn discovery_args(
    kind: DiscoveryKind,
    kubeconfig: Option<&str>,
    context: Option<&str>,
    namespace: Option<&str>,
) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(path) = kubeconfig.filter(|p| !p.trim().is_empty()) {
        args.push("--kubeconfig".to_string());
        args.push(path.trim().to_string());
    }
    if let Some(ctx) = context.filter(|c| !c.trim().is_empty()) {
        args.push("--context".to_string());
        args.push(ctx.trim().to_string());
    }
    match kind {
        DiscoveryKind::Contexts => {
            args.extend([
                "config".to_string(),
                "get-contexts".to_string(),
                "-o".to_string(),
                "name".to_string(),
            ]);
        }
        DiscoveryKind::Namespaces => {
            args.extend([
                "get".to_string(),
                "ns".to_string(),
                "-o".to_string(),
                "name".to_string(),
            ]);
        }
        DiscoveryKind::Targets(target_kind) => {
            let resource = match target_kind {
                KubeTargetKind::Pod => "pods",
                KubeTargetKind::Service => "svc",
                KubeTargetKind::Deployment => "deploy",
            };
            args.push("get".to_string());
            args.push(resource.to_string());
            if let Some(ns) = namespace.filter(|n| !n.trim().is_empty()) {
                args.push("-n".to_string());
                args.push(ns.trim().to_string());
            }
            args.extend(["-o".to_string(), "name".to_string()]);
        }
    }
    args
}

/// Runs kubectl on the tokio runtime with a 10 s timeout; Err carries stderr.
pub async fn run_discovery(
    kind: DiscoveryKind,
    kubeconfig: Option<String>,
    context: Option<String>,
    namespace: Option<String>,
) -> Result<Vec<String>, String> {
    if !crate::distribution::can_launch_user_programs() {
        return Err(crate::distribution::sandbox_blocked_message(
            "Kubernetes discovery (kubectl)",
        ));
    }
    let bin = match locate_kubectl() {
        KubectlLocation::Override(p) | KubectlLocation::OnPath(p) => p,
        KubectlLocation::NotFound => {
            return Err(crate::localization::text(
                "connections.kubernetes.kubectl_not_found",
            ));
        }
    };
    let args = discovery_args(
        kind,
        kubeconfig.as_deref(),
        context.as_deref(),
        namespace.as_deref(),
    );
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000);
    }
    let discovery_error = |error: &str| {
        crate::localization::format(
            "connections.kubernetes.discovery_failed",
            &[("error", error)],
        )
    };
    let child = cmd.spawn().map_err(|e| discovery_error(&e.to_string()))?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await
        .map_err(|_| crate::localization::text("connections.kubernetes.discovery_timed_out"))?
        .map_err(|e| discovery_error(&e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            discovery_error(&output.status.to_string())
        } else {
            stderr
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_names(&stdout))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_names_strips_kind_prefix() {
        assert_eq!(
            parse_names("namespace/default\npod/postgres-0"),
            vec!["default".to_string(), "postgres-0".to_string()]
        );
    }

    #[test]
    fn parse_names_ignores_blank_lines() {
        assert_eq!(parse_names(" \n\npod/a\n"), vec!["a".to_string()]);
    }

    #[test]
    fn discovery_args_for_each_kind() {
        assert_eq!(
            discovery_args(DiscoveryKind::Contexts, None, None, None),
            vec!["config", "get-contexts", "-o", "name"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            discovery_args(DiscoveryKind::Namespaces, None, None, None),
            vec!["get", "ns", "-o", "name"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            discovery_args(
                DiscoveryKind::Targets(KubeTargetKind::Pod),
                None,
                None,
                Some("data")
            ),
            vec!["get", "pods", "-n", "data", "-o", "name"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn discovery_args_include_kubeconfig_and_context() {
        assert_eq!(
            discovery_args(DiscoveryKind::Contexts, Some("/cfg"), Some("dev"), None),
            vec![
                "--kubeconfig",
                "/cfg",
                "--context",
                "dev",
                "config",
                "get-contexts",
                "-o",
                "name"
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
    }
}
