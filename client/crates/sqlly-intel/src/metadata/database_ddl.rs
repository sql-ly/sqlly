//! CREATE DATABASE / DROP DATABASE text per engine, plus the catalog queries
//! that fill the dialog's pickers and typed fallbacks for when they cannot run.

use sqlly_core::{ConnectionKind, DatabaseType};

use crate::metadata::escape_literal;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateDatabaseSpec {
    pub name: String,
    /// PostgreSQL OWNER; ignored elsewhere.
    pub owner: Option<String>,
    /// PostgreSQL ENCODING (e.g. UTF8); MySQL CHARACTER SET; ignored on SQL Server.
    pub encoding: Option<String>,
    /// PostgreSQL LC_COLLATE/LC_CTYPE (both set to this), MySQL COLLATE, SQL Server COLLATE.
    pub collation: Option<String>,
    /// PostgreSQL TEMPLATE; `template0` is required when LC_* differ from the cluster default.
    pub template: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DropDatabaseSpec {
    pub name: String,
    /// PostgreSQL 13+: `WITH (FORCE)`.
    pub force: bool,
    /// SQL Server: prepend `ALTER DATABASE .. SET SINGLE_USER WITH ROLLBACK IMMEDIATE;`.
    pub single_user: bool,
    /// Emit `IF EXISTS` where the engine supports it (PG, MySQL, SQL Server 2016+).
    pub if_exists: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DatabaseDdlError {
    #[error("SQLite databases are files — create one from New Connection")]
    Unsupported,
    #[error("Enter a database name")]
    EmptyName,
    #[error("{0}")]
    InvalidOption(String),
}

impl DatabaseDdlError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        match self {
            Self::Unsupported => sqlly_core::DisplayMessage::new(
                "services.database_ddl.errors.unsupported",
                self.to_string(),
            ),
            Self::EmptyName => sqlly_core::DisplayMessage::new(
                "services.database_ddl.errors.empty_name",
                self.to_string(),
            ),
            Self::InvalidOption(detail) => sqlly_core::DisplayMessage::new(
                "services.database_ddl.errors.invalid_option",
                self.to_string(),
            )
            .with_arg(
                "option",
                detail.strip_prefix("Invalid option: ").unwrap_or(detail),
            ),
        }
    }
}

pub fn create_database_sql(
    db: DatabaseType,
    spec: &CreateDatabaseSpec,
) -> Result<String, DatabaseDdlError> {
    match db.connection_kind() {
        ConnectionKind::PostgresWire => create_postgres(db, spec),
        ConnectionKind::MySqlWire => create_mysql(db, spec),
        ConnectionKind::Tds => create_tds(db, spec),
        ConnectionKind::SqliteFile => Err(DatabaseDdlError::Unsupported),
        _ => Err(DatabaseDdlError::Unsupported),
    }
}

pub fn drop_database_sql(
    db: DatabaseType,
    spec: &DropDatabaseSpec,
) -> Result<String, DatabaseDdlError> {
    match db.connection_kind() {
        ConnectionKind::PostgresWire => drop_postgres(db, spec),
        ConnectionKind::MySqlWire => drop_mysql(db, spec),
        ConnectionKind::Tds => drop_tds(db, spec),
        ConnectionKind::SqliteFile => Err(DatabaseDdlError::Unsupported),
        _ => Err(DatabaseDdlError::Unsupported),
    }
}

pub fn collations_sql(db: DatabaseType) -> Option<String> {
    match db.connection_kind() {
        ConnectionKind::PostgresWire => Some(
            "SELECT collname FROM pg_collation WHERE collnamespace = 'pg_catalog'::regnamespace ORDER BY 1"
                .to_string(),
        ),
        ConnectionKind::MySqlWire => Some("SHOW COLLATION".to_string()),
        ConnectionKind::Tds => {
            Some("SELECT name FROM fn_helpcollations() ORDER BY name".to_string())
        }
        _ => None,
    }
}

pub fn encodings_sql(db: DatabaseType) -> Option<String> {
    match db.connection_kind() {
        ConnectionKind::PostgresWire => Some(
            "SELECT pg_encoding_to_char(n) FROM generate_series(0, 41) n WHERE pg_encoding_to_char(n) <> '' ORDER BY 1"
                .to_string(),
        ),
        ConnectionKind::MySqlWire => Some("SHOW CHARACTER SET".to_string()),
        _ => None,
    }
}

pub fn owners_sql(db: DatabaseType) -> Option<String> {
    match db.connection_kind() {
        ConnectionKind::PostgresWire => {
            Some("SELECT rolname FROM pg_roles WHERE rolcanlogin ORDER BY 1".to_string())
        }
        _ => None,
    }
}

pub fn templates_sql(db: DatabaseType) -> Option<String> {
    match db.connection_kind() {
        ConnectionKind::PostgresWire => {
            Some("SELECT datname FROM pg_database WHERE datistemplate ORDER BY 1".to_string())
        }
        _ => None,
    }
}

pub fn fallback_collations(db: DatabaseType) -> &'static [&'static str] {
    match db.connection_kind() {
        ConnectionKind::PostgresWire => &["C", "C.UTF-8", "en_US.utf8", "POSIX"],
        ConnectionKind::MySqlWire => &[
            "utf8mb4_0900_ai_ci",
            "utf8mb4_general_ci",
            "utf8mb4_unicode_ci",
            "utf8mb4_bin",
            "latin1_swedish_ci",
        ],
        ConnectionKind::Tds => &[
            "SQL_Latin1_General_CP1_CI_AS",
            "Latin1_General_CI_AS",
            "Latin1_General_100_CI_AS_SC_UTF8",
            "Latin1_General_BIN2",
        ],
        _ => &[],
    }
}

pub fn fallback_encodings(db: DatabaseType) -> &'static [&'static str] {
    match db.connection_kind() {
        ConnectionKind::PostgresWire => &["UTF8", "LATIN1", "SQL_ASCII", "WIN1252"],
        ConnectionKind::MySqlWire => &["utf8mb4", "utf8mb3", "latin1", "ascii", "binary"],
        _ => &[],
    }
}

pub fn maintenance_database(db: DatabaseType) -> Option<&'static str> {
    match db.connection_kind() {
        ConnectionKind::PostgresWire => Some("postgres"),
        ConnectionKind::Tds => Some("master"),
        ConnectionKind::MySqlWire => None,
        _ => None,
    }
}

fn validate_name(name: &str) -> Result<&str, DatabaseDdlError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 128 || trimmed.chars().any(|c| c.is_control()) {
        return Err(DatabaseDdlError::EmptyName);
    }
    Ok(trimmed)
}

fn validate_word(value: &str) -> Result<&str, DatabaseDdlError> {
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(DatabaseDdlError::InvalidOption(format!(
            "Invalid option: {value}"
        )));
    }
    Ok(value)
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|s| !s.trim().is_empty())
}

fn create_postgres(
    db: DatabaseType,
    spec: &CreateDatabaseSpec,
) -> Result<String, DatabaseDdlError> {
    let name = validate_name(&spec.name)?;
    let mut sql = format!("CREATE DATABASE {}", db.quote_identifier(name));
    let mut options = Vec::new();

    if let Some(owner) = non_empty(&spec.owner) {
        options.push(format!(
            " OWNER = {}",
            db.quote_identifier(validate_name(owner)?)
        ));
    }
    if let Some(enc) = non_empty(&spec.encoding) {
        options.push(format!(" ENCODING = '{}'", escape_literal(enc)));
    }
    if let Some(coll) = non_empty(&spec.collation) {
        let lit = escape_literal(coll);
        options.push(format!(" LC_COLLATE = '{lit}' LC_CTYPE = '{lit}'"));
    }
    let template = non_empty(&spec.template).or_else(|| {
        if non_empty(&spec.collation).is_some() {
            Some("template0")
        } else {
            None
        }
    });
    if let Some(template) = template {
        options.push(format!(" TEMPLATE = {}", db.quote_identifier(template)));
    }
    if !options.is_empty() {
        sql.push_str(" WITH");
        sql.push_str(&options.concat());
    }
    sql.push_str(";\n");
    Ok(sql)
}

fn create_mysql(db: DatabaseType, spec: &CreateDatabaseSpec) -> Result<String, DatabaseDdlError> {
    let name = validate_name(&spec.name)?;
    let mut sql = format!("CREATE DATABASE {}", db.quote_identifier(name));
    if let Some(enc) = non_empty(&spec.encoding) {
        sql.push_str(&format!(" CHARACTER SET {}", validate_word(enc)?));
    }
    if let Some(coll) = non_empty(&spec.collation) {
        sql.push_str(&format!(" COLLATE {}", validate_word(coll)?));
    }
    sql.push_str(";\n");
    Ok(sql)
}

fn create_tds(db: DatabaseType, spec: &CreateDatabaseSpec) -> Result<String, DatabaseDdlError> {
    let name = validate_name(&spec.name)?;
    let mut sql = format!("CREATE DATABASE {}", db.quote_identifier(name));
    if let Some(coll) = non_empty(&spec.collation) {
        sql.push_str(&format!(" COLLATE {}", validate_word(coll)?));
    }
    sql.push_str(";\n");
    Ok(sql)
}

fn drop_postgres(db: DatabaseType, spec: &DropDatabaseSpec) -> Result<String, DatabaseDdlError> {
    let name = validate_name(&spec.name)?;
    let mut sql = String::from("DROP DATABASE");
    if spec.if_exists {
        sql.push_str(" IF EXISTS");
    }
    sql.push_str(&format!(" {}", db.quote_identifier(name)));
    if spec.force {
        sql.push_str(" WITH (FORCE)");
    }
    sql.push_str(";\n");
    Ok(sql)
}

fn drop_mysql(db: DatabaseType, spec: &DropDatabaseSpec) -> Result<String, DatabaseDdlError> {
    let name = validate_name(&spec.name)?;
    let mut sql = String::from("DROP DATABASE");
    if spec.if_exists {
        sql.push_str(" IF EXISTS");
    }
    sql.push_str(&format!(" {}", db.quote_identifier(name)));
    sql.push_str(";\n");
    Ok(sql)
}

fn drop_tds(db: DatabaseType, spec: &DropDatabaseSpec) -> Result<String, DatabaseDdlError> {
    let name = validate_name(&spec.name)?;
    let mut sql = String::new();
    if spec.single_user {
        sql.push_str(&format!(
            "ALTER DATABASE {} SET SINGLE_USER WITH ROLLBACK IMMEDIATE;\n",
            db.quote_identifier(name)
        ));
    }
    sql.push_str("DROP DATABASE");
    if spec.if_exists {
        sql.push_str(" IF EXISTS");
    }
    sql.push_str(&format!(" {};\n", db.quote_identifier(name)));
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::DatabaseType;

    #[test]
    fn empty_name_errors() {
        let err = create_database_sql(
            DatabaseType::Postgres,
            &CreateDatabaseSpec {
                name: "   ".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err, DatabaseDdlError::EmptyName);
    }

    #[test]
    fn sqlite_is_unsupported() {
        let err = create_database_sql(
            DatabaseType::Sqlite,
            &CreateDatabaseSpec {
                name: "x".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert_eq!(err, DatabaseDdlError::Unsupported);
    }

    #[test]
    fn pg_collation_implies_template0() {
        let sql = create_database_sql(
            DatabaseType::Postgres,
            &CreateDatabaseSpec {
                name: "app".into(),
                collation: Some("en_US.utf8".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(sql.contains("TEMPLATE = \"template0\""));
    }

    #[test]
    fn mysql_invalid_collation_rejected() {
        let err = create_database_sql(
            DatabaseType::MySql,
            &CreateDatabaseSpec {
                name: "app".into(),
                collation: Some("bad;drop".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, DatabaseDdlError::InvalidOption(_)));
    }

    #[test]
    fn pg_drop_with_force() {
        let sql = drop_database_sql(
            DatabaseType::Postgres,
            &DropDatabaseSpec {
                name: "app".into(),
                force: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(sql, "DROP DATABASE \"app\" WITH (FORCE);\n");
    }

    #[test]
    fn tds_drop_single_user_prefix() {
        let sql = drop_database_sql(
            DatabaseType::SqlServer,
            &DropDatabaseSpec {
                name: "app".into(),
                single_user: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(sql.starts_with(
            "ALTER DATABASE [app] SET SINGLE_USER WITH ROLLBACK IMMEDIATE;\nDROP DATABASE [app];\n"
        ));
    }
}
