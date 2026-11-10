//! Live SQL Server connection configuration for integration tests.
//!
//! Two sources, in priority order:
//!
//! 1. **Local dev file** — a gitignored JSON file (default name
//!    `.sqlly-local-sql.json`) located at the workspace root (or any ancestor
//!    of the current directory). This lets a developer point the live
//!    integration tests at a SQL Server they already have running, without
//!    Docker and without committing credentials.
//! 2. **Environment variables** — used by CI, where the pipeline stands up a
//!    SQL Server (e.g. in Docker) and exports `SQLLY_SQL_*` vars.
//!
//! If neither source is present, [`LiveSqlConfig::load`] returns `None` and the
//! caller should skip the live test (so the default `cargo test` run stays
//! hermetic and green with no server).
//!
//! ## Strict mode (CI)
//!
//! When `SQLLY_REQUIRE_LIVE_SQL` (legacy `OSXQL_REQUIRE_LIVE_SQL`) is set to
//! `1`, `true`, or `yes`, [`LiveSqlConfig::load`] **panics** instead of
//! returning `None` if no config was found. CI sets this so a lost server config
//! fails loudly instead of silently skipping live coverage.
//!
//! ## File shape (`.sqlly-local-sql.json`)
//!
//! ```json
//! {
//!   "server": "localhost",
//!   "port": 1433,
//!   "database": "MetaEngine.DB.Test",
//!   "username": "sa",
//!   "password": "Omega321!",
//!   "encrypt": true,
//!   "trust_server_certificate": true
//! }
//! ```
//!
//! ## Environment variables (CI)
//!
//! - `SQLLY_SQL_SERVER` (required to activate the env source)
//! - `SQLLY_SQL_PORT` (default 1433)
//! - `SQLLY_SQL_DATABASE` (default `master`)
//! - `SQLLY_SQL_USERNAME` (default `sa`)
//! - `SQLLY_SQL_PASSWORD` (required)
//! - `SQLLY_SQL_ENCRYPT` (default `true`)
//! - `SQLLY_SQL_TRUST_CERT` (default `true`)

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};

/// Default gitignored file name searched for from the current dir upward.
pub const LOCAL_SQL_CONFIG_FILE: &str = ".sqlly-local-sql.json";
pub const LEGACY_LOCAL_SQL_CONFIG_FILE: &str = ".osxql-local-sql.json";

/// Resolved live-SQL connection settings, decoupled from how they were sourced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSqlConfig {
    pub server: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_database")]
    pub database: String,
    #[serde(default = "default_username")]
    pub username: String,
    pub password: String,
    #[serde(default = "default_true")]
    pub encrypt: bool,
    #[serde(default = "default_true")]
    pub trust_server_certificate: bool,
}

fn default_port() -> u16 {
    1433
}
fn default_database() -> String {
    "master".to_string()
}
fn default_username() -> String {
    "sa".to_string()
}
fn default_true() -> bool {
    true
}

impl LiveSqlConfig {
    /// Loads the live config from (in order) the gitignored local file, then
    /// environment variables. Returns `None` when neither source is configured,
    /// so live tests can self-skip on a vanilla checkout. Panics when
    /// `SQLLY_REQUIRE_LIVE_SQL=1` is set and no config was found.
    pub fn load() -> Option<Self> {
        let start = std::env::current_dir().ok();
        Self::load_from(start.as_deref())
    }

    /// Same as [`load`](Self::load), but searches for the local file starting
    /// from an explicit directory (`None` skips the file source entirely).
    /// Lets tests run hermetically against a directory with no config.
    pub fn load_from(start: Option<&Path>) -> Option<Self> {
        let cfg = start
            .and_then(Self::from_local_file_at)
            .or_else(Self::from_env);
        if cfg.is_none() && env_bool("SQLLY_REQUIRE_LIVE_SQL", "OSXQL_REQUIRE_LIVE_SQL", false) {
            panic!(
                "SQLLY_REQUIRE_LIVE_SQL=1 but no live SQL config found: \
                 set SQLLY_SQL_SERVER + SQLLY_SQL_PASSWORD (see sqlly-testkit::live docs)"
            );
        }
        cfg
    }

    /// Reads the gitignored JSON file, searching the current directory and its
    /// ancestors (so it works regardless of which crate dir a test runs in).
    pub fn from_local_file() -> Option<Self> {
        let start = std::env::current_dir().ok()?;
        Self::from_local_file_at(&start)
    }

    /// Reads the gitignored JSON file, searching `start` and its ancestors.
    pub fn from_local_file_at(start: &Path) -> Option<Self> {
        let path = find_upward(start, LOCAL_SQL_CONFIG_FILE)
            .or_else(|| find_upward(start, LEGACY_LOCAL_SQL_CONFIG_FILE))?;
        let raw = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<LiveSqlConfig>(&raw) {
            Ok(cfg) => Some(cfg),
            Err(err) => {
                eprintln!("invalid {}: {err}", path.display());
                None
            }
        }
    }

    /// Builds the config from `SQLLY_SQL_*` env vars. Requires at least
    /// `SQLLY_SQL_SERVER` and `SQLLY_SQL_PASSWORD`.
    pub fn from_env() -> Option<Self> {
        let server = env_var("SQLLY_SQL_SERVER", "OSXQL_SQL_SERVER").ok()?;
        let password = env_var("SQLLY_SQL_PASSWORD", "OSXQL_SQL_PASSWORD").ok()?;
        Some(Self {
            server,
            port: env_var("SQLLY_SQL_PORT", "OSXQL_SQL_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(default_port),
            database: env_var("SQLLY_SQL_DATABASE", "OSXQL_SQL_DATABASE")
                .unwrap_or_else(|_| default_database()),
            username: env_var("SQLLY_SQL_USERNAME", "OSXQL_SQL_USERNAME")
                .unwrap_or_else(|_| default_username()),
            password,
            encrypt: env_bool("SQLLY_SQL_ENCRYPT", "OSXQL_SQL_ENCRYPT", true),
            trust_server_certificate: env_bool(
                "SQLLY_SQL_TRUST_CERT",
                "OSXQL_SQL_TRUST_CERT",
                true,
            ),
        })
    }

    /// Converts to a ready-to-use [`ConnectionProfile`] with an in-memory secret.
    pub fn to_profile(&self, name: impl Into<String>) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            name,
            self.server.clone(),
            self.username.clone(),
            SecretRef {
                provider: SecretProvider::InMemory,
                key: self.password.clone(),
            },
        );
        profile.port = self.port;
        profile.database = Some(self.database.clone());
        profile.encrypt = self.encrypt;
        profile.trust_server_certificate = self.trust_server_certificate;
        profile
    }
}

fn env_var(key: &str, legacy_key: &str) -> Result<String, std::env::VarError> {
    std::env::var(key).or_else(|_| std::env::var(legacy_key))
}

fn env_bool(key: &str, legacy_key: &str, default: bool) -> bool {
    match env_var(key, legacy_key) {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        Err(_) => default,
    }
}

/// Walks up from `start` looking for a file named `name`; returns its path.
fn find_upward(start: &Path, name: &str) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EnvGuard;

    #[test]
    fn parses_full_config_json() {
        let json = r#"{
            "server": "localhost",
            "port": 1433,
            "database": "MetaEngine.DB.Test",
            "username": "sa",
            "password": "Omega321!",
            "encrypt": true,
            "trust_server_certificate": true
        }"#;
        let cfg: LiveSqlConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.server, "localhost");
        assert_eq!(cfg.port, 1433);
        assert_eq!(cfg.database, "MetaEngine.DB.Test");
        assert!(cfg.encrypt && cfg.trust_server_certificate);
    }

    #[test]
    fn applies_defaults_for_optional_fields() {
        let json = r#"{ "server": "db", "password": "p" }"#;
        let cfg: LiveSqlConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.port, 1433);
        assert_eq!(cfg.database, "master");
        assert_eq!(cfg.username, "sa");
        assert!(cfg.encrypt);
        assert!(cfg.trust_server_certificate);
    }

    #[test]
    fn builds_connection_profile_with_secret() {
        let cfg = LiveSqlConfig {
            server: "localhost".into(),
            port: 1433,
            database: "MetaEngine.DB.Test".into(),
            username: "sa".into(),
            password: "Omega321!".into(),
            encrypt: true,
            trust_server_certificate: true,
        };
        let profile = cfg.to_profile("live");
        assert_eq!(profile.server, "localhost");
        assert_eq!(profile.database.as_deref(), Some("MetaEngine.DB.Test"));
        assert!(profile.encrypt);
        assert!(profile.trust_server_certificate);
        match &profile.authentication {
            sqlly_core::AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                assert_eq!(username, "sa");
                assert_eq!(secret_ref.key, "Omega321!");
            }
            other => panic!("expected SqlLogin auth, got {other:?}"),
        }
    }

    #[test]
    fn load_panics_when_required_and_missing() {
        let mut guard = EnvGuard::acquire();
        for key in [
            "SQLLY_REQUIRE_LIVE_SQL",
            "SQLLY_SQL_SERVER",
            "SQLLY_SQL_PASSWORD",
            "OSXQL_REQUIRE_LIVE_SQL",
            "OSXQL_SQL_SERVER",
            "OSXQL_SQL_PASSWORD",
        ] {
            guard.remove(key);
        }
        guard.set("SQLLY_REQUIRE_LIVE_SQL", "1");

        // Search from an empty temp dir so a developer's real local config
        // file (which lives above the repo or in it) can't satisfy load().
        let empty = std::env::temp_dir().join(format!("sqlly_no_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let result = std::panic::catch_unwind(|| LiveSqlConfig::load_from(Some(&empty)));

        assert!(
            result.is_err(),
            "expected panic when SQLLY_REQUIRE_LIVE_SQL=1 and no config"
        );
    }

    #[test]
    fn find_upward_locates_file_in_ancestor() {
        let tmp = std::env::temp_dir().join(format!("sqlly_findup_{}", std::process::id()));
        let nested = tmp.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let marker = tmp.join(LOCAL_SQL_CONFIG_FILE);
        std::fs::write(&marker, "{}").unwrap();
        let found = find_upward(&nested, LOCAL_SQL_CONFIG_FILE).unwrap();
        assert_eq!(found, marker);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
