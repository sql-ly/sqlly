//! Docker-backed SQL Server container for integration tests.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};

/// A disposable SQL Server 2022 container started via Docker.
pub struct SqlServerContainer {
    id: String,
    port: u16,
    sa_password: String,
}

impl SqlServerContainer {
    /// Start a container. `sa_password` is the `MSSQL_SA_PASSWORD` value — callers
    /// keep secrets in their own test file; testkit never embeds credentials.
    pub fn start(sa_password: &str) -> Self {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("sqlly-sqlserver-{suffix}");
        let output = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &name,
                "-e",
                "ACCEPT_EULA=Y",
                "-e",
                &format!("MSSQL_SA_PASSWORD={sa_password}"),
                "-p",
                "1433",
                "mcr.microsoft.com/mssql/server:2022-latest",
            ])
            .output()
            .expect("docker run should execute");
        assert!(
            output.status.success(),
            "docker run failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let port = docker_port(&id);
        Self {
            id,
            port,
            sa_password: sa_password.to_string(),
        }
    }

    /// Host port mapped for TDS (1433 inside the container).
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Build a `ConnectionProfile` for this container (callers poll readiness).
    pub fn profile(&self) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "container",
            "127.0.0.1",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: self.sa_password.clone(),
            },
        );
        profile.port = self.port;
        profile.database = Some("master".to_string());
        profile.encrypt = true;
        profile.trust_server_certificate = true;
        profile
    }
}

impl Drop for SqlServerContainer {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.id]).status();
    }
}

fn docker_port(container_id: &str) -> u16 {
    let output = Command::new("docker")
        .args(["port", container_id, "1433/tcp"])
        .output()
        .expect("docker port should execute");
    assert!(
        output.status.success(),
        "docker port failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let port = text
        .trim()
        .rsplit(':')
        .next()
        .expect("docker port output should contain host port");
    port.parse().expect("host port should be numeric")
}
