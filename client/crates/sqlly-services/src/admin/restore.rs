use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RestoreOptions {
    pub database: String,
    pub source: String,
    pub replace: bool,
    pub recovery: bool,
    pub file_relocation: Vec<FileRelocation>,
    pub point_in_time: Option<chrono::NaiveDateTime>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRelocation {
    pub logical_name: String,
    pub new_path: String,
}

pub struct RestoreManager;

impl RestoreManager {
    pub fn validate(options: &RestoreOptions) -> Result<(), String> {
        if options.database.trim().is_empty() {
            return Err("Database name is required".to_string());
        }
        if options.source.trim().is_empty() {
            return Err("Restore source is required".to_string());
        }
        Ok(())
    }

    pub fn generate_restore_sql(options: &RestoreOptions) -> String {
        let mut sql = format!(
            "RESTORE DATABASE {} FROM DISK = {}",
            quote_identifier(&options.database),
            quote_literal(&options.source)
        );
        let mut with_parts = Vec::new();
        if options.replace {
            with_parts.push("REPLACE".to_string());
        }
        if options.recovery {
            with_parts.push("RECOVERY".to_string());
        } else {
            with_parts.push("NORECOVERY".to_string());
        }
        for relocation in &options.file_relocation {
            with_parts.push(format!(
                "MOVE {} TO {}",
                quote_literal(&relocation.logical_name),
                quote_literal(&relocation.new_path)
            ));
        }
        if let Some(pit) = options.point_in_time {
            with_parts.push(format!("STOPAT = N'{}'", pit.format("%Y-%m-%d %H:%M:%S")));
        }
        if !with_parts.is_empty() {
            sql.push_str(" WITH ");
            sql.push_str(&with_parts.join(", "));
        }
        sql
    }

    /// ClickHouse: `RESTORE DATABASE db FROM Disk('backups', 'name')`;
    /// `allow_non_empty` lets the restore add rows into tables that already
    /// hold data (the server refuses that by default).
    pub fn clickhouse_restore_sql(
        database: &str,
        backup_name: &str,
        allow_non_empty: bool,
    ) -> String {
        let mut sql = format!(
            "RESTORE DATABASE {} FROM Disk('backups', {})",
            ch_quote_identifier(database),
            ch_quote_literal(backup_name)
        );
        if allow_non_empty {
            sql.push_str(" SETTINGS allow_non_empty_tables = true");
        }
        sql.push(';');
        sql
    }

    pub fn filelistonly_query(source: &str) -> String {
        format!("RESTORE FILELISTONLY FROM DISK = {}", quote_literal(source))
    }

    pub fn headeronly_query(source: &str) -> String {
        format!("RESTORE HEADERONLY FROM DISK = {}", quote_literal(source))
    }

    pub fn restore_verifyonly_query(source: &str) -> String {
        format!("RESTORE VERIFYONLY FROM DISK = {}", quote_literal(source))
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
    fn clickhouse_restore_identifier_escapes_backslashes() {
        assert_eq!(ch_quote_identifier(r"db\"), r"`db\\`");
    }

    #[test]
    fn generates_restore_with_ordered_options_and_escaping() {
        let options = RestoreOptions {
            database: "Sales]DW".to_string(),
            source: "/backups/O'Brien.bak".to_string(),
            replace: true,
            recovery: false,
            file_relocation: vec![FileRelocation {
                logical_name: "Sales'Data".to_string(),
                new_path: "/data/Sales'Data.mdf".to_string(),
            }],
            point_in_time: None,
        };
        assert!(RestoreManager::validate(&options).is_ok());
        let sql = RestoreManager::generate_restore_sql(&options);

        assert_eq!(
            sql,
            "RESTORE DATABASE [Sales]]DW] FROM DISK = N'/backups/O''Brien.bak' WITH REPLACE, NORECOVERY, MOVE N'Sales''Data' TO N'/data/Sales''Data.mdf'"
        );
    }

    #[test]
    fn clickhouse_restore_reads_the_backups_disk() {
        assert_eq!(
            RestoreManager::clickhouse_restore_sql("shop", "shop.zip", false),
            "RESTORE DATABASE `shop` FROM Disk('backups', 'shop.zip');"
        );
        assert_eq!(
            RestoreManager::clickhouse_restore_sql("shop", "shop.zip", true),
            "RESTORE DATABASE `shop` FROM Disk('backups', 'shop.zip') SETTINGS allow_non_empty_tables = true;"
        );
    }

    #[test]
    fn escapes_restore_inspection_source() {
        assert_eq!(
            RestoreManager::filelistonly_query("O'Brien.bak"),
            "RESTORE FILELISTONLY FROM DISK = N'O''Brien.bak'"
        );
        assert!(RestoreManager::restore_verifyonly_query("O'Brien.bak").contains("VERIFYONLY"));
    }
}
