use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupOptions {
    pub database: String,
    pub backup_type: BackupType,
    pub destination: String,
    pub compress: bool,
    pub checksum: bool,
    pub copy_only: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackupType {
    Full,
    Differential,
    Log,
}

pub struct BackupManager;

impl BackupManager {
    pub fn validate(options: &BackupOptions) -> Result<(), String> {
        if options.database.trim().is_empty() {
            return Err("Database name is required".to_string());
        }
        if options.destination.trim().is_empty() {
            return Err("Backup destination is required".to_string());
        }
        Ok(())
    }

    pub fn generate_backup_sql(options: &BackupOptions) -> String {
        let type_clause = match options.backup_type {
            BackupType::Full | BackupType::Differential => "DATABASE",
            BackupType::Log => "LOG",
        };
        let mut sql = format!(
            "BACKUP {} {} TO DISK = {}",
            type_clause,
            quote_identifier(&options.database),
            quote_literal(&options.destination)
        );
        let mut with_parts: Vec<String> = Vec::new();
        if options.backup_type == BackupType::Differential {
            with_parts.push("DIFFERENTIAL".to_string());
        }
        if options.compress {
            with_parts.push("COMPRESSION".to_string());
        }
        if options.checksum {
            with_parts.push("CHECKSUM".to_string());
        }
        if options.copy_only {
            with_parts.push("COPY_ONLY".to_string());
        }
        if let Some(description) = &options.description {
            with_parts.push(format!("DESCRIPTION = {}", quote_literal(description)));
        }
        if !with_parts.is_empty() {
            sql.push_str(" WITH ");
            sql.push_str(&with_parts.join(", "));
        }
        sql
    }

    /// ClickHouse: `BACKUP DATABASE db TO Disk('backups', 'name')`, the
    /// server-side backup to the disk named `backups` in its storage
    /// configuration (every table's parts and metadata). `compress` asks
    /// for zstd inside the archive.
    pub fn clickhouse_backup_sql(database: &str, backup_name: &str, compress: bool) -> String {
        let mut sql = format!(
            "BACKUP DATABASE {} TO Disk('backups', {})",
            ch_quote_identifier(database),
            ch_quote_literal(backup_name)
        );
        if compress {
            sql.push_str(" SETTINGS compression_method = 'zstd'");
        }
        sql.push(';');
        sql
    }

    /// The checks behind the ClickHouse form: a database and a backup
    /// name on the `backups` disk, which must be a relative name.
    pub fn validate_clickhouse(database: &str, backup_name: &str) -> Result<(), String> {
        if database.trim().is_empty() {
            return Err("Database name is required".to_string());
        }
        let name = backup_name.trim();
        if name.is_empty() {
            return Err("Backup name on the 'backups' disk is required".to_string());
        }
        if name.starts_with('/') || name.contains("..") {
            return Err(
                "Backup name must be a relative path inside the 'backups' disk".to_string(),
            );
        }
        Ok(())
    }

    pub fn progress_percent(message: &str) -> Option<u8> {
        let marker = " percent processed.";
        let end = message.find(marker)?;
        message[..end].split_whitespace().last()?.parse().ok()
    }
}

/// ClickHouse identifier quoting (backslash-escaping included) — the one
/// shared rule in `DatabaseType::quote_identifier`.
fn ch_quote_identifier(identifier: &str) -> String {
    sqlly_core::DatabaseType::ClickHouse.quote_identifier(identifier)
}

fn ch_quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\\', "\\\\").replace('\'', "''"))
}

fn quote_identifier(identifier: &str) -> String {
    format!("[{}]", identifier.replace(']', "]]"))
}

fn quote_literal(value: &str) -> String {
    format!("N'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clickhouse_backup_identifier_escapes_backslashes() {
        assert_eq!(ch_quote_identifier(r"db\"), r"`db\\`");
    }

    #[test]
    fn generates_differential_backup_with_escaped_inputs() {
        let options = BackupOptions {
            database: "Sales]DW".to_string(),
            backup_type: BackupType::Differential,
            destination: "/backups/O'Brien.bak".to_string(),
            compress: true,
            checksum: true,
            copy_only: false,
            description: Some("Nightly O'Brien".to_string()),
        };
        assert!(BackupManager::validate(&options).is_ok());
        let sql = BackupManager::generate_backup_sql(&BackupOptions {
            database: "Sales]DW".to_string(),
            backup_type: BackupType::Differential,
            destination: "/backups/O'Brien.bak".to_string(),
            compress: true,
            checksum: true,
            copy_only: false,
            description: Some("Nightly O'Brien".to_string()),
        });

        assert_eq!(
            sql,
            "BACKUP DATABASE [Sales]]DW] TO DISK = N'/backups/O''Brien.bak' WITH DIFFERENTIAL, COMPRESSION, CHECKSUM, DESCRIPTION = N'Nightly O''Brien'"
        );
    }

    #[test]
    fn clickhouse_backup_targets_the_backups_disk() {
        assert_eq!(
            BackupManager::clickhouse_backup_sql("sh`op", "shop_2026-09-26.zip", true),
            "BACKUP DATABASE `sh``op` TO Disk('backups', 'shop_2026-09-26.zip') SETTINGS compression_method = 'zstd';"
        );
        assert_eq!(
            BackupManager::clickhouse_backup_sql("shop", "o'clock", false),
            "BACKUP DATABASE `shop` TO Disk('backups', 'o''clock');"
        );
        assert!(BackupManager::validate_clickhouse("shop", "x.zip").is_ok());
        assert!(BackupManager::validate_clickhouse("", "x.zip").is_err());
        assert!(BackupManager::validate_clickhouse("shop", "").is_err());
        assert!(BackupManager::validate_clickhouse("shop", "/etc/x").is_err());
        assert!(BackupManager::validate_clickhouse("shop", "../x").is_err());
    }

    #[test]
    fn rejects_invalid_backup_options_and_parses_progress() {
        let options = BackupOptions {
            database: "appdb".to_string(),
            backup_type: BackupType::Log,
            destination: "".to_string(),
            compress: false,
            checksum: false,
            copy_only: true,
            description: None,
        };
        assert!(BackupManager::validate(&options).is_err());
        assert_eq!(
            BackupManager::progress_percent("42 percent processed."),
            Some(42)
        );
    }
}
