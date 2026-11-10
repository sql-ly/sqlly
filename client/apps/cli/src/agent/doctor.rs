use crate::agent::connections::{load_store, store_path, summary};
use crate::agent::format::{OutputFormat, ResultSet};
use crate::agent::{CliError, ExitCode, emit};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
struct DoctorReport {
    config_dir: String,
    connections_file: ConnectionsFileStatus,
    keyring: KeyringStatus,
    catalog_root: Option<String>,
    drivers: Vec<DriverInfo>,
    version: String,
    profiles: Vec<Value>,
}

#[derive(Debug, Serialize)]
struct ConnectionsFileStatus {
    path: String,
    exists: bool,
    profiles: usize,
    environments: usize,
}

#[derive(Debug, Serialize)]
struct KeyringStatus {
    reachable: bool,
    detail: String,
}

/// One native driver the CLI links. `module` is the `sqlly-engines` module
/// that implements it; `status` is the runtime prerequisite check for the
/// drivers that have one (Oracle's Instant Client), `None` where the driver
/// is self-contained.
#[derive(Debug, Serialize)]
struct DriverInfo {
    engine: &'static str,
    /// The `sqlly-engines` module, spelled `sqlly-engines::<module>`.
    crate_name: &'static str,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<DriverStatus>,
}

#[derive(Debug, Serialize)]
struct DriverStatus {
    ready: bool,
    detail: String,
}

pub async fn run(json: bool) -> ExitCode {
    let result = build_report().map(|report| {
        let text = render_text(&report);
        let value = serde_json::to_value(&report).expect("doctor json");
        (text, value)
    });
    match result {
        Ok((text, value)) => emit(json, Some(text), Ok(value)),
        Err(err) => emit(json, None, Err(err)),
    }
}

fn build_report() -> Result<DoctorReport, CliError> {
    let path = store_path();
    let store = load_store(&path)?;
    let config_dir = path
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ".".to_string());

    Ok(DoctorReport {
        config_dir,
        connections_file: ConnectionsFileStatus {
            path: path.display().to_string(),
            exists: path.exists(),
            profiles: store.profiles.len(),
            environments: store.environments.len(),
        },
        keyring: probe_keyring(),
        catalog_root: Some(
            sqlly_services::catalog::catalog_root()
                .display()
                .to_string(),
        ),
        drivers: driver_rows(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        profiles: store.profiles.iter().map(summary).collect(),
    })
}

fn render_text(report: &DoctorReport) -> String {
    use comfy_table::{Table, presets::UTF8_FULL_CONDENSED};
    let mut table = Table::new();
    table.load_style(UTF8_FULL_CONDENSED);
    table.set_header(["Check", "Result"]);
    table.add_row(["Config dir", &report.config_dir]);
    table.add_row([
        "Connections file",
        &format!(
            "{} ({} profiles)",
            report.connections_file.path, report.connections_file.profiles
        ),
    ]);
    table.add_row([
        "Keyring",
        &format!(
            "{} — {}",
            if report.keyring.reachable {
                "reachable"
            } else {
                "unreachable"
            },
            report.keyring.detail
        ),
    ]);
    table.add_row([
        "Catalog root",
        report.catalog_root.as_deref().unwrap_or("unknown"),
    ]);
    table.add_row(["Version", &report.version]);
    for driver in &report.drivers {
        table.add_row([
            &format!("Driver: {}", driver.engine),
            &driver_status_line(driver),
        ]);
    }

    let mut out = table.to_string();
    out.push_str("\n\nSaved connections:\n");
    let list_set = vec![ResultSet {
        columns: vec![
            sqlly_core::ResultColumn {
                name: "name".into(),
                data_type: "text".into(),
                nullable: false,
            },
            sqlly_core::ResultColumn {
                name: "engine".into(),
                data_type: "text".into(),
                nullable: false,
            },
            sqlly_core::ResultColumn {
                name: "server".into(),
                data_type: "text".into(),
                nullable: false,
            },
            sqlly_core::ResultColumn {
                name: "database".into(),
                data_type: "text".into(),
                nullable: true,
            },
            sqlly_core::ResultColumn {
                name: "environment".into(),
                data_type: "text".into(),
                nullable: false,
            },
            sqlly_core::ResultColumn {
                name: "id".into(),
                data_type: "text".into(),
                nullable: false,
            },
        ],
        rows: report
            .profiles
            .iter()
            .map(|p| {
                vec![
                    sqlly_core::CellValue::String(p["name"].as_str().unwrap_or("").to_string()),
                    sqlly_core::CellValue::String(p["engine"].as_str().unwrap_or("").to_string()),
                    sqlly_core::CellValue::String(p["server"].as_str().unwrap_or("").to_string()),
                    sqlly_core::CellValue::String(p["database"].as_str().unwrap_or("").to_string()),
                    sqlly_core::CellValue::String(
                        p["environment"].as_str().unwrap_or("").to_string(),
                    ),
                    sqlly_core::CellValue::String(p["id"].as_str().unwrap_or("").to_string()),
                ]
            })
            .collect(),
        rows_affected: None,
        truncated: false,
    }];
    out.push_str(&crate::agent::format::render(
        OutputFormat::Table,
        &list_set,
    ));
    out
}

fn probe_keyring() -> KeyringStatus {
    match keyring::Entry::new("SQLLY.sql-auth", "doctor-probe") {
        Ok(entry) => match entry.get_password() {
            Ok(_) => KeyringStatus {
                reachable: true,
                detail: "read ok".to_string(),
            },
            Err(keyring::Error::NoEntry) => KeyringStatus {
                reachable: true,
                detail: "no probe entry (expected)".to_string(),
            },
            Err(err) => KeyringStatus {
                reachable: false,
                detail: err.to_string(),
            },
        },
        Err(err) => KeyringStatus {
            reachable: false,
            detail: err.to_string(),
        },
    }
}

/// The text-table cell for one driver: the module and, for drivers with a
/// runtime prerequisite, whether it was found.
fn driver_status_line(driver: &DriverInfo) -> String {
    match &driver.status {
        None => format!("{} — built in", driver.crate_name),
        Some(status) if status.ready => format!("{} — {}", driver.crate_name, status.detail),
        Some(status) => format!("{} — MISSING: {}", driver.crate_name, status.detail),
    }
}

/// One row per native engine family (Redis covers Valkey, KeyDB,
/// DragonflyDB, ElastiCache and MemoryDB, which speak its protocol), in the
/// order the connection editor lists them. Every driver except Oracle is
/// self-contained; Oracle needs the Instant Client on the machine, so its
/// row probes for the library.
fn driver_rows() -> Vec<DriverInfo> {
    let version = env!("CARGO_PKG_VERSION").to_string();
    let built_in = |engine: &'static str, crate_name: &'static str| DriverInfo {
        engine,
        crate_name,
        version: version.clone(),
        status: None,
    };
    vec![
        built_in("SQL Server / Azure SQL", "sqlly-engines::tds"),
        built_in("PostgreSQL", "sqlly-engines::pg"),
        built_in("MySQL/MariaDB", "sqlly-engines::mysql"),
        built_in("SQLite", "sqlly-engines::sqlite"),
        built_in("DuckDB", "sqlly-engines::duckdb"),
        built_in("ClickHouse", "sqlly-engines::clickhouse"),
        DriverInfo {
            engine: "Oracle Database",
            crate_name: "sqlly-engines::oracle",
            version: version.clone(),
            status: Some(oracle_client_status()),
        },
        built_in("libSQL / Turso", "sqlly-engines::libsql"),
        built_in("Redis", "sqlly-engines::redis"),
    ]
}

/// Probe the Oracle Instant Client the way a connect would, without
/// dialing anything.
fn oracle_client_status() -> DriverStatus {
    match sqlly_engines::oracle::client_library_version() {
        Ok(version) => DriverStatus {
            ready: true,
            detail: format!("Oracle Instant Client {version} found"),
        },
        Err(message) => DriverStatus {
            ready: false,
            detail: message,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::connections::StoreFile;
    use sqlly_core::{ConnectionEnvironment, ConnectionProfile, SecretProvider, SecretRef};
    use std::io::Write;

    /// One doctor row per native driver family, so the roster the CLI
    /// reports is the roster the engine can actually dial.
    #[test]
    fn driver_rows_cover_every_native_engine_family() {
        let rows = driver_rows();
        let engines: Vec<&str> = rows.iter().map(|r| r.engine).collect();
        assert_eq!(
            engines,
            [
                "SQL Server / Azure SQL",
                "PostgreSQL",
                "MySQL/MariaDB",
                "SQLite",
                "DuckDB",
                "ClickHouse",
                "Oracle Database",
                "libSQL / Turso",
                "Redis",
            ]
        );
        let native_kinds: std::collections::HashSet<_> = sqlly_core::DatabaseType::ALL
            .into_iter()
            .filter(|db| db.has_native_driver())
            .map(|db| db.connection_kind())
            .collect();
        assert_eq!(native_kinds.len(), rows.len());
        for row in &rows {
            assert!(
                row.crate_name.starts_with("sqlly-engines::"),
                "{}",
                row.engine
            );
        }
        // Only Oracle has a runtime prerequisite worth probing.
        let probed: Vec<&str> = rows
            .iter()
            .filter(|r| r.status.is_some())
            .map(|r| r.engine)
            .collect();
        assert_eq!(probed, ["Oracle Database"]);
    }

    #[test]
    fn driver_status_line_spells_out_a_missing_prerequisite() {
        let row = |status| DriverInfo {
            engine: "Oracle Database",
            crate_name: "sqlly-engines::oracle",
            version: "0.0.0".into(),
            status,
        };
        assert_eq!(
            driver_status_line(&row(None)),
            "sqlly-engines::oracle — built in"
        );
        let found = row(Some(DriverStatus {
            ready: true,
            detail: "Oracle Instant Client 23.3.0.0.0 found".into(),
        }));
        assert_eq!(
            driver_status_line(&found),
            "sqlly-engines::oracle — Oracle Instant Client 23.3.0.0.0 found"
        );
        let missing = row(Some(DriverStatus {
            ready: false,
            detail: "Oracle Instant Client not found — install it".into(),
        }));
        let line = driver_status_line(&missing);
        assert!(line.contains("MISSING"), "{line}");
        assert!(line.contains("Instant Client not found"), "{line}");
        // The JSON envelope carries the probe only where one exists.
        let json = serde_json::to_value(row(None)).unwrap();
        assert!(json.get("status").is_none());
        let json = serde_json::to_value(&missing).unwrap();
        assert_eq!(json["status"]["ready"], serde_json::Value::Bool(false));
    }

    #[test]
    fn doctor_report_serializes_without_secrets() {
        let dir = std::env::temp_dir().join(format!("sqlly-doctor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connections.json");
        let mut profile = ConnectionProfile::sql_login(
            "dev",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "unit-test-secret-marker".to_string(),
            },
        );
        profile.environment = ConnectionEnvironment::Dev;
        let store = StoreFile {
            version: 1,
            profiles: vec![profile],
            environments: vec![],
        };
        let mut file = std::fs::File::create(&path).unwrap();
        write!(
            file,
            "{}",
            serde_json::to_string(&serde_json::json!({
                "version": store.version,
                "profiles": store.profiles,
            }))
            .unwrap()
        )
        .unwrap();

        unsafe {
            std::env::set_var("SQLLY_CONNECTIONS_FILE", &path);
        }
        let report = build_report().unwrap();
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("unit-test-secret-marker"));
        unsafe {
            std::env::remove_var("SQLLY_CONNECTIONS_FILE");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
