#![cfg(not(target_arch = "wasm32"))]

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use parking_lot::Mutex;
use sqlly_core::connection::{KubePortForward, KubeTargetKind};
use sqlly_engines::ssh::{TunnelManager, open_port_forward, set_kubectl_override};

static FAKE_KUBECTL_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn with_fake_kubectl_lock(f: impl FnOnce()) {
    let guard = FAKE_KUBECTL_TEST_LOCK.get_or_init(|| Mutex::new(())).lock();
    reset_fake_kubectl();
    drop(guard);
    f();
}

fn reset_fake_kubectl() {
    set_kubectl_override(Some(fixture_path()));
    if let Some(store) = sqlly_engines::ssh::kube::KUBECTL_EXTRA_ENV.get() {
        store.write().clear();
    }
}

fn fixture_path() -> PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    if cfg!(windows) {
        dir.join("fake-kubectl.cmd")
    } else {
        dir.join("fake-kubectl.sh")
    }
}

fn base_cfg() -> KubePortForward {
    KubePortForward {
        kubeconfig: None,
        context: Some("dev-cluster".to_string()),
        namespace: "default".to_string(),
        target_kind: KubeTargetKind::Pod,
        target_name: "postgres-0".to_string(),
        remote_port: 5432,
    }
}

fn require_fake_kubectl() {
    if std::env::var("SQLLY_FAKE_KUBECTL").is_err() {
        eprintln!("skipping: set SQLLY_FAKE_KUBECTL=1");
        std::process::exit(0);
    }
}

#[tokio::test]
async fn fake_kubectl_forward_reports_port_and_kills_child() {
    require_fake_kubectl();
    with_fake_kubectl_lock(|| {});
    let handle = open_port_forward(&base_cfg(), Duration::from_secs(10))
        .await
        .expect("forward");
    assert_eq!(handle.local_port, 45999);
    let child_id = handle.child_id();
    drop(handle);
    if let Some(pid) = child_id {
        for _ in 0..20 {
            if !process_alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("kubectl child still alive after drop");
    }
}

#[tokio::test]
async fn fake_kubectl_not_found_target_surfaces_stderr() {
    require_fake_kubectl();
    with_fake_kubectl_lock(|| {});
    let mut cfg = base_cfg();
    cfg.target_name = "missing".to_string();
    let err = match open_port_forward(&cfg, Duration::from_secs(10)).await {
        Err(e) => e,
        Ok(_) => panic!("expected failure"),
    };
    let msg = err.to_string();
    assert!(msg.contains("kubectl port-forward failed"));
    assert!(msg.contains("NotFound"));
}

#[tokio::test]
async fn fake_kubectl_readiness_timeout() {
    require_fake_kubectl();
    with_fake_kubectl_lock(|| {
        sqlly_engines::ssh::kube::KUBECTL_EXTRA_ENV
            .get_or_init(|| parking_lot::RwLock::new(Vec::new()))
            .write()
            .push(("FAKE_KUBECTL_HANG".to_string(), "1".to_string()));
    });
    let err = match open_port_forward(&base_cfg(), Duration::from_secs(1)).await {
        Err(e) => e,
        Ok(_) => panic!("expected timeout"),
    };
    let msg = err.to_string();
    assert!(msg.contains("did not become ready"));
    if let Some(store) = sqlly_engines::ssh::kube::KUBECTL_EXTRA_ENV.get() {
        store.write().clear();
    }
}

#[tokio::test]
async fn endpoint_for_reuses_live_forward() {
    require_fake_kubectl();
    with_fake_kubectl_lock(|| {});
    let mut profile = sqlly_core::ConnectionProfile::sql_login(
        "kube",
        "ignored",
        "sa",
        sqlly_core::SecretRef {
            provider: sqlly_core::SecretProvider::InMemory,
            key: "x".to_string(),
        },
    );
    profile.database_type = sqlly_core::DatabaseType::Postgres;
    profile.transport = sqlly_core::TransportChain::kube_only(base_cfg());
    let manager = TunnelManager::global();
    let (host1, port1) = manager.endpoint_for(&profile).await.expect("first");
    let (_host2, port2) = manager.endpoint_for(&profile).await.expect("second");
    assert_eq!(host1, "127.0.0.1");
    assert_eq!(port1, port2);
    manager.close(&profile.id);
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use std::process::Command;
    let output = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}")])
        .output()
        .ok();
    output
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}
