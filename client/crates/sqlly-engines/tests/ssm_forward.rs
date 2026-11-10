//! AWS Session Manager transport against a fake `aws` CLI
//! (`tests/fixtures/fake-aws.sh`). Unix only: the fake is a shell script.
//! One test function, because the CLI overrides are process-global.
#![cfg(all(unix, not(target_arch = "wasm32")))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use sqlly_core::connection::SsmPortForward;
use sqlly_engines::ssh::{AwsTool, TunnelManager, open_ssm_forward, set_aws_tool_override};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-aws.sh")
}

fn set_env(vars: &[(&str, &Path)], flags: &[&str]) {
    let store =
        sqlly_engines::ssh::ssm::AWS_EXTRA_ENV.get_or_init(|| parking_lot::RwLock::new(Vec::new()));
    let mut env = store.write();
    env.clear();
    for (key, value) in vars {
        env.push((key.to_string(), value.to_string_lossy().to_string()));
    }
    for flag in flags {
        env.push((flag.to_string(), "1".to_string()));
    }
}

fn cfg() -> SsmPortForward {
    SsmPortForward {
        target: "i-0fake".to_string(),
        region: Some("us-east-2".to_string()),
        profile: Some("ops".to_string()),
        remote_host: None,
    }
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn wait_dead(pid: u32) -> bool {
    for _ in 0..50 {
        if !process_alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn fake_aws_cli_forward_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let args_file = dir.path().join("args");
    let child_file = dir.path().join("child");
    let files = [
        ("FAKE_AWS_ARGS_FILE", args_file.as_path()),
        ("FAKE_AWS_CHILD_FILE", child_file.as_path()),
    ];
    set_aws_tool_override(AwsTool::Cli, Some(fixture()));
    set_aws_tool_override(AwsTool::SessionManagerPlugin, Some(fixture()));

    // Ready: the local port is the one passed as localPortNumber, and the
    // remote host/port are the ones asked for.
    set_env(&files, &[]);
    let handle = open_ssm_forward(&cfg(), "db.internal", 5432, Duration::from_secs(10))
        .await
        .expect("forward");
    let args = std::fs::read_to_string(&args_file).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(
        &args[..6],
        &[
            "ssm",
            "start-session",
            "--target",
            "i-0fake",
            "--document-name",
            "AWS-StartPortForwardingSessionToRemoteHost"
        ]
    );
    assert_eq!(
        args[7],
        format!(
            r#"{{"host":["db.internal"],"portNumber":["5432"],"localPortNumber":["{}"]}}"#,
            handle.local_port
        )
    );
    assert_eq!(&args[8..], &["--region", "us-east-2", "--profile", "ops"]);

    // Teardown ends the CLI and the helper it started.
    let cli_pid = handle.child_id().expect("cli pid");
    let helper_pid: u32 = std::fs::read_to_string(&child_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(process_alive(helper_pid));
    drop(handle);
    assert!(wait_dead(cli_pid).await, "aws CLI still alive after drop");
    assert!(
        wait_dead(helper_pid).await,
        "plugin helper still alive after drop"
    );

    // The CLI's own error reaches the user.
    set_env(&files, &["FAKE_AWS_FAIL"]);
    let err = match open_ssm_forward(&cfg(), "db.internal", 5432, Duration::from_secs(10)).await {
        Err(err) => err.to_string(),
        Ok(_) => panic!("expected failure"),
    };
    assert!(err.contains("TargetNotConnected"), "{err}");
    assert!(err.contains("could not start"), "{err}");

    // The CLI's "plugin not found" becomes the install hint.
    set_env(&files, &["FAKE_AWS_NO_PLUGIN"]);
    let err = match open_ssm_forward(&cfg(), "db.internal", 5432, Duration::from_secs(10)).await {
        Err(err) => err.to_string(),
        Ok(_) => panic!("expected failure"),
    };
    assert!(err.contains("session-manager-plugin"), "{err}");

    // The connection path: endpoint_for reuses one live session and forwards
    // to the profile's server unless a remote host is set.
    set_env(&files, &[]);
    let mut profile = sqlly_core::ConnectionProfile::sql_login(
        "ssm",
        "mydb.abc123.us-east-2.rds.amazonaws.com",
        "app",
        sqlly_core::SecretRef {
            provider: sqlly_core::SecretProvider::InMemory,
            key: "x".to_string(),
        },
    );
    profile.database_type = sqlly_core::DatabaseType::Postgres;
    profile.port = 5432;
    profile.transport = sqlly_core::TransportChain::ssm_only(cfg());
    let manager = TunnelManager::global();
    let (host1, port1) = manager.endpoint_for(&profile).await.expect("first");
    let (_, port2) = manager.endpoint_for(&profile).await.expect("second");
    assert_eq!(host1, "127.0.0.1");
    assert_eq!(port1, port2);
    let args = std::fs::read_to_string(&args_file).unwrap();
    assert!(
        args.contains(r#""host":["mydb.abc123.us-east-2.rds.amazonaws.com"]"#),
        "{args}"
    );
    manager.close(&profile.id);

    set_aws_tool_override(AwsTool::Cli, None);
    set_aws_tool_override(AwsTool::SessionManagerPlugin, None);
}
