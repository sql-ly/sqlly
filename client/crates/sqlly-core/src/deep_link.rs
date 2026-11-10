//! `sqlly://` deep links. Parsing is pure and strict: unknown hosts, unknown
//! query keys, over-long values and control characters are errors, never
//! ignored. Nothing here touches the network, the file system, or SQL
//! execution — a link only ever *proposes* an action the user confirms.

use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

use crate::DatabaseType;
use crate::connection_url::{ParsedConnectionUrl, parse_connection_url};

pub const SCHEME: &str = "sqlly";
pub const MAX_SQL_BYTES: usize = 64 * 1024;
pub const MAX_NAME_BYTES: usize = 256;
pub const SQLITE_EXTENSIONS: [&str; 3] = ["sqlite", "db", "sqlite3"];
pub const DUCKDB_EXTENSIONS: [&str; 2] = ["duckdb", "ddb"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepLink {
    /// `sqlly://open?connection=<name>[&database=<db>][&table=<schema.name|name>]`
    Open {
        connection: String,
        database: Option<String>,
        table: Option<String>,
    },
    /// `sqlly://query?connection=<name>[&database=<db>]&sql=<urlencoded>` — opens an editor tab, never runs.
    Query {
        connection: String,
        database: Option<String>,
        sql: String,
    },
    /// `sqlly://connection/new?url=<urlencoded dsn>` — opens the connection editor pre-filled.
    /// Boxed: the parsed URL dwarfs the other variants (clippy::large_enum_variant).
    NewConnection { parsed: Box<ParsedConnectionUrl> },
    /// A raw connection URL (`mysql://user:pw@host/db`, see
    /// [`crate::connect_url`]): connect to the saved connection it names, or
    /// to an unsaved one built from it. Debug output never shows the
    /// password.
    ConnectUrl { parsed: Box<ParsedConnectionUrl> },
    /// A local SQLite or DuckDB database file (from argv, drag-drop, or
    /// Finder "Open With"); `engine` is decided by the file extension.
    OpenDatabaseFile { path: PathBuf, engine: DatabaseType },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeepLinkError {
    #[error("not a sqlly:// link")]
    NotSqllyScheme,
    #[error("unknown link action `{0}`")]
    UnknownAction(String),
    #[error("missing `{0}` parameter")]
    Missing(&'static str),
    #[error("unknown parameter `{0}`")]
    UnknownParameter(String),
    #[error("`{0}` is too long")]
    TooLong(&'static str),
    #[error("`{0}` contains control characters")]
    ControlCharacters(&'static str),
    #[error("connection string: {0}")]
    ConnectionUrl(String),
    #[error("not a SQLite or DuckDB database file")]
    NotDatabaseFile,
}

impl DeepLinkError {
    pub fn message(&self) -> crate::DisplayMessage {
        use crate::DisplayMessage;
        match self {
            Self::NotSqllyScheme => {
                DisplayMessage::new("services.deep_link.errors.not_sqlly", self.to_string())
            }
            Self::UnknownAction(action) => {
                DisplayMessage::new("services.deep_link.errors.unknown_action", self.to_string())
                    .with_arg("action", action)
            }
            Self::Missing(parameter) => DisplayMessage::new(
                "services.deep_link.errors.missing_parameter",
                self.to_string(),
            )
            .with_arg("parameter_name", *parameter),
            Self::UnknownParameter(parameter) => DisplayMessage::new(
                "services.deep_link.errors.unknown_parameter",
                self.to_string(),
            )
            .with_arg("parameter_name", parameter),
            Self::TooLong(parameter) => {
                DisplayMessage::new("services.deep_link.errors.too_long", self.to_string())
                    .with_arg("parameter_name", *parameter)
            }
            Self::ControlCharacters(parameter) => DisplayMessage::new(
                "services.deep_link.errors.control_characters",
                self.to_string(),
            )
            .with_arg("parameter_name", *parameter),
            Self::ConnectionUrl(detail) => {
                DisplayMessage::new("services.deep_link.errors.connection_url", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::NotDatabaseFile => DisplayMessage::new(
                "services.deep_link.errors.not_database_file",
                self.to_string(),
            ),
        }
    }
}

/// Parse one process argument or OS-delivered URL. Accepts `sqlly://…`, `file://`
/// URLs to SQLite/DuckDB files, raw connection URLs (`mysql://…`, see
/// [`crate::connect_url::RAW_CONNECTION_SCHEMES`]), and bare absolute paths
/// ending in a SQLite or DuckDB extension; everything else is an error.
pub fn parse_deep_link(input: &str) -> Result<DeepLink, DeepLinkError> {
    let input = input.trim();
    if input.starts_with("file://") {
        return parse_file_url(input);
    }
    if input.starts_with("sqlly://") {
        return parse_sqlly_url(input);
    }
    if crate::connect_url::is_raw_connection_uri(input) {
        return parse_raw_connection_url(input);
    }
    parse_absolute_database_path(input)
}

fn parse_raw_connection_url(input: &str) -> Result<DeepLink, DeepLinkError> {
    if input.len() > crate::connect_url::MAX_RAW_URL_BYTES {
        return Err(DeepLinkError::TooLong("url"));
    }
    if input.chars().any(char::is_control) {
        return Err(DeepLinkError::ControlCharacters("url"));
    }
    let parsed = parse_connection_url(input)
        .map_err(|error| DeepLinkError::ConnectionUrl(error.to_string()))?;
    Ok(DeepLink::ConnectUrl {
        parsed: Box::new(parsed),
    })
}

fn has_extension_in(path: &Path, candidates: &[&str]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| {
            candidates
                .iter()
                .any(|candidate| ext.eq_ignore_ascii_case(candidate))
        })
}

/// Case-insensitive extension check shared with drag-drop.
pub fn is_sqlite_path(path: &Path) -> bool {
    has_extension_in(path, &SQLITE_EXTENSIONS)
}

/// `.duckdb` / `.ddb` files open as DuckDB connections.
pub fn is_duckdb_path(path: &Path) -> bool {
    has_extension_in(path, &DUCKDB_EXTENSIONS)
}

/// The file engine a path's extension names, if any.
pub fn file_database_engine(path: &Path) -> Option<DatabaseType> {
    if is_sqlite_path(path) {
        Some(DatabaseType::Sqlite)
    } else if is_duckdb_path(path) {
        Some(DatabaseType::DuckDb)
    } else {
        None
    }
}

pub fn describe_message(link: &DeepLink) -> crate::DisplayMessage {
    let fallback = describe_fallback(link);
    let (key, connection, database, table, engine, file) = match link {
        DeepLink::Open {
            connection,
            database,
            table: Some(table),
        } => {
            let stem = match (table.contains('.'), database.is_some()) {
                (true, false) => "open_object",
                (true, true) => "open_object_database",
                (false, false) => "open_table",
                (false, true) => "open_table_database",
            };
            (
                stem,
                connection.as_str(),
                database.as_deref(),
                Some(table.as_str()),
                "",
                String::new(),
            )
        }
        DeepLink::Open {
            connection,
            database,
            table: None,
        } => {
            let stem = if database.is_some() {
                "open_connection_database"
            } else {
                "open_connection"
            };
            (
                stem,
                connection.as_str(),
                database.as_deref(),
                None,
                "",
                String::new(),
            )
        }
        DeepLink::Query {
            connection,
            database,
            ..
        } => {
            let stem = if database.is_some() {
                "open_query_database"
            } else {
                "open_query"
            };
            (
                stem,
                connection.as_str(),
                database.as_deref(),
                None,
                "",
                String::new(),
            )
        }
        DeepLink::NewConnection { parsed } => (
            "new_connection",
            "",
            None,
            None,
            parsed.engine.display_name(),
            String::new(),
        ),
        DeepLink::ConnectUrl { parsed } => (
            "connect_url",
            "",
            None,
            None,
            parsed.engine.display_name(),
            crate::connect_url::connection_url_label(parsed),
        ),
        DeepLink::OpenDatabaseFile { path, engine } => {
            let stem = if path.file_name().is_some() {
                "open_database_file"
            } else {
                "open_database_engine"
            };
            (
                stem,
                "",
                None,
                None,
                engine.display_name(),
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        }
    };
    let mut message = crate::DisplayMessage::new(
        match key {
            "open_object" => "services.deep_link.open_object",
            "open_object_database" => "services.deep_link.open_object_database",
            "open_table" => "services.deep_link.open_table",
            "open_table_database" => "services.deep_link.open_table_database",
            "open_connection" => "services.deep_link.open_connection",
            "open_connection_database" => "services.deep_link.open_connection_database",
            "open_query" => "services.deep_link.open_query",
            "open_query_database" => "services.deep_link.open_query_database",
            "new_connection" => "services.deep_link.new_connection",
            "connect_url" => "services.deep_link.connect_url",
            "open_database_file" => "services.deep_link.open_database_file",
            "open_database_engine" => "services.deep_link.open_database_engine",
            _ => unreachable!(),
        },
        fallback,
    );
    if !connection.is_empty() {
        message = message.with_arg("connection_name", connection);
    }
    if let Some(database) = database {
        message = message.with_arg("database_name", database);
    }
    if let Some(table) = table {
        message = message.with_arg("table_name", table);
    }
    if !engine.is_empty() {
        message = message.with_arg("engine_name", engine);
    }
    if !file.is_empty() {
        message = if matches!(link, DeepLink::ConnectUrl { .. }) {
            message.with_arg("server_name", file)
        } else {
            message.with_arg("file_name", file)
        };
    }
    message
}

/// English compatibility fallback; localized callers use [`describe_message`].
pub fn describe(link: &DeepLink) -> String {
    describe_message(link).into_fallback()
}

fn describe_fallback(link: &DeepLink) -> String {
    match link {
        DeepLink::Open {
            connection,
            database,
            table,
        } => {
            let mut line = if let Some(table) = table {
                let table_label = if table.contains('.') {
                    table.clone()
                } else {
                    format!("table {table}")
                };
                format!("Open {table_label} on \"{connection}\"")
            } else {
                format!("Open connection \"{connection}\"")
            };
            if let Some(database) = database {
                line.push_str(&format!(" (database {database})"));
            }
            line
        }
        DeepLink::Query {
            connection,
            database,
            ..
        } => {
            let mut line = format!("Open a query on \"{connection}\"");
            if let Some(database) = database {
                line.push_str(&format!(" (database {database})"));
            }
            line
        }
        DeepLink::NewConnection { parsed } => format!(
            "Open the connection editor with a new {} connection",
            parsed.engine.display_name()
        ),
        DeepLink::ConnectUrl { parsed } => format!(
            "Connect to the {} server {}",
            parsed.engine.display_name(),
            crate::connect_url::connection_url_label(parsed)
        ),
        DeepLink::OpenDatabaseFile { path, engine } => {
            let engine = engine.display_name();
            match path.file_name() {
                Some(name) => format!("Open {engine} database \"{}\"", name.to_string_lossy()),
                None => format!("Open {engine} database"),
            }
        }
    }
}

/// Connection name derived from a database file path: the file stem, or the
/// engine's name when empty.
pub fn file_connection_name(path: &Path, engine: DatabaseType) -> String {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| engine.display_name().to_string())
}

fn parse_file_url(input: &str) -> Result<DeepLink, DeepLinkError> {
    let url =
        url::Url::parse(input).map_err(|error| DeepLinkError::ConnectionUrl(error.to_string()))?;
    // `Url::to_file_path` is only compiled on unix/windows/wasi in the `url`
    // crate; on wasm32 it does not exist and local-file deep links are
    // meaningless in the browser anyway.
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = url
            .to_file_path()
            .map_err(|_| DeepLinkError::NotDatabaseFile)?;
        let engine = file_database_engine(&path).ok_or(DeepLinkError::NotDatabaseFile)?;
        Ok(DeepLink::OpenDatabaseFile { path, engine })
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = url;
        Err(DeepLinkError::NotDatabaseFile)
    }
}

fn parse_absolute_database_path(input: &str) -> Result<DeepLink, DeepLinkError> {
    let path = PathBuf::from(input);
    let Some(engine) = file_database_engine(&path) else {
        return Err(DeepLinkError::NotSqllyScheme);
    };
    if !path.is_absolute() {
        return Err(DeepLinkError::NotDatabaseFile);
    }
    Ok(DeepLink::OpenDatabaseFile { path, engine })
}

fn parse_sqlly_url(input: &str) -> Result<DeepLink, DeepLinkError> {
    let rest = input
        .strip_prefix("sqlly://")
        .ok_or(DeepLinkError::NotSqllyScheme)?;
    let (action, query) = match rest.split_once('?') {
        Some((action, query)) => (action, Some(query)),
        None => (rest, None),
    };
    let action = action.trim_end_matches('/');
    match action {
        "open" => parse_open(query),
        "query" => parse_query(query),
        "connection/new" => parse_new_connection(query),
        other => Err(DeepLinkError::UnknownAction(other.to_string())),
    }
}

fn parse_open(query: Option<&str>) -> Result<DeepLink, DeepLinkError> {
    let params = parse_params(query, &["connection", "database", "table"])?;
    let connection = required_name(params.get("connection"), "connection")?;
    let database = optional_name(params.get("database"), "database")?;
    let table = optional_name(params.get("table"), "table")?;
    Ok(DeepLink::Open {
        connection,
        database,
        table,
    })
}

fn parse_query(query: Option<&str>) -> Result<DeepLink, DeepLinkError> {
    let params = parse_params(query, &["connection", "database", "sql"])?;
    let connection = required_name(params.get("connection"), "connection")?;
    let database = optional_name(params.get("database"), "database")?;
    let sql = required_sql(params.get("sql"))?;
    Ok(DeepLink::Query {
        connection,
        database,
        sql,
    })
}

fn parse_new_connection(query: Option<&str>) -> Result<DeepLink, DeepLinkError> {
    let params = parse_params(query, &["url"])?;
    let url = params
        .get("url")
        .ok_or(DeepLinkError::Missing("url"))?
        .as_str();
    let parsed = parse_connection_url(url)
        .map_err(|error| DeepLinkError::ConnectionUrl(error.to_string()))?;
    Ok(DeepLink::NewConnection {
        parsed: Box::new(parsed),
    })
}

fn parse_params(
    query: Option<&str>,
    allowed: &[&str],
) -> Result<std::collections::BTreeMap<String, String>, DeepLinkError> {
    let Some(query) = query else {
        return Ok(std::collections::BTreeMap::new());
    };
    let mut out = std::collections::BTreeMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| DeepLinkError::UnknownParameter(pair.to_string()))?;
        if !allowed.contains(&key) {
            return Err(DeepLinkError::UnknownParameter(key.to_string()));
        }
        let decoded = percent_decode_str(value)
            .decode_utf8()
            .map_err(|_| DeepLinkError::ControlCharacters("parameter"))?
            .into_owned();
        out.insert(key.to_string(), decoded);
    }
    Ok(out)
}

fn required_name(value: Option<&String>, field: &'static str) -> Result<String, DeepLinkError> {
    let Some(value) = value else {
        return Err(DeepLinkError::Missing(field));
    };
    validate_name(value, field)
}

fn optional_name(
    value: Option<&String>,
    field: &'static str,
) -> Result<Option<String>, DeepLinkError> {
    match value {
        None => Ok(None),
        Some(value) => validate_name(value, field).map(Some),
    }
}

fn validate_name(value: &str, field: &'static str) -> Result<String, DeepLinkError> {
    if value.len() > MAX_NAME_BYTES {
        return Err(DeepLinkError::TooLong(field));
    }
    if value.chars().any(char::is_control) {
        return Err(DeepLinkError::ControlCharacters(field));
    }
    Ok(value.to_string())
}

fn required_sql(value: Option<&String>) -> Result<String, DeepLinkError> {
    let Some(value) = value else {
        return Err(DeepLinkError::Missing("sql"));
    };
    if value.len() > MAX_SQL_BYTES {
        return Err(DeepLinkError::TooLong("sql"));
    }
    if value.contains('\0') {
        return Err(DeepLinkError::ControlCharacters("sql"));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gridex #18: a raw connection URL as a startup argument is a link.
    #[test]
    fn raw_connection_urls_parse_into_connect_links() {
        let link = parse_deep_link("  mysql://user:pw@server:3306/database ").unwrap();
        let DeepLink::ConnectUrl { parsed } = &link else {
            panic!("expected ConnectUrl, got {link:?}");
        };
        assert_eq!(parsed.engine, DatabaseType::MySql);
        assert_eq!(parsed.server, "server");
        assert_eq!(parsed.port, Some(3306));
        assert_eq!(parsed.database.as_deref(), Some("database"));
        assert_eq!(parsed.username.as_deref(), Some("user"));
        assert_eq!(parsed.password.as_deref(), Some("pw"));
        let description = describe(&link);
        assert_eq!(
            description,
            "Connect to the MySQL server user@server:3306/database"
        );
        assert!(!description.contains("pw@"));
        let message = describe_message(&link);
        assert_eq!(message.key(), Some("services.deep_link.connect_url"));
        assert!(
            message
                .arguments()
                .any(|(name, value)| name == "server_name" && value == "user@server:3306/database")
        );

        for (url, engine) in [
            ("mariadb://h/db", DatabaseType::MariaDb),
            ("postgres://h/db", DatabaseType::Postgres),
            ("postgresql://h/db", DatabaseType::Postgres),
            ("sqlserver://h:1433", DatabaseType::SqlServer),
            ("jdbc:mysql://h/db", DatabaseType::MySql),
        ] {
            match parse_deep_link(url) {
                Ok(DeepLink::ConnectUrl { parsed }) => assert_eq!(parsed.engine, engine, "{url}"),
                other => panic!("{url}: {other:?}"),
            }
        }
    }

    #[test]
    fn raw_connection_url_errors_are_typed() {
        assert!(matches!(
            parse_deep_link("mysql://h:notaport/db"),
            Err(DeepLinkError::ConnectionUrl(_))
        ));
        assert_eq!(
            parse_deep_link("mysql://h/db\u{7}"),
            Err(DeepLinkError::ControlCharacters("url"))
        );
        let long = format!("mysql://h/{}", "a".repeat(9000));
        assert_eq!(parse_deep_link(&long), Err(DeepLinkError::TooLong("url")));
        // A database named like a SQLite file is still a URL, not a path.
        assert!(matches!(
            parse_deep_link("mysql://h/app.db"),
            Ok(DeepLink::ConnectUrl { .. })
        ));
    }
    use sqlly_lang::sqlmodel::DatabaseType;

    #[test]
    fn open_link_parses_connection_database_and_table() {
        let link = parse_deep_link("sqlly://open?connection=prod&database=Sales&table=dbo.Orders")
            .unwrap();
        assert_eq!(
            link,
            DeepLink::Open {
                connection: "prod".into(),
                database: Some("Sales".into()),
                table: Some("dbo.Orders".into()),
            }
        );
    }

    #[test]
    fn query_link_percent_decodes_sql_and_never_contains_plus_expansion() {
        let link = parse_deep_link("sqlly://query?connection=prod&sql=SELECT%201%2B1").unwrap();
        assert_eq!(
            link,
            DeepLink::Query {
                connection: "prod".into(),
                database: None,
                sql: "SELECT 1+1".into(),
            }
        );
        let link = parse_deep_link("sqlly://query?connection=prod&sql=a+b").unwrap();
        assert_eq!(
            link,
            DeepLink::Query {
                connection: "prod".into(),
                database: None,
                sql: "a+b".into(),
            }
        );
    }

    #[test]
    fn query_link_over_64k_is_rejected() {
        let sql = "x".repeat(MAX_SQL_BYTES + 1);
        let link = format!("sqlly://query?connection=prod&sql={sql}");
        assert!(matches!(
            parse_deep_link(&link),
            Err(DeepLinkError::TooLong("sql"))
        ));
    }

    #[test]
    fn unknown_action_and_unknown_parameter_are_errors() {
        assert!(matches!(
            parse_deep_link("sqlly://nope?connection=x"),
            Err(DeepLinkError::UnknownAction(_))
        ));
        assert!(matches!(
            parse_deep_link("sqlly://open?connection=x&nope=1"),
            Err(DeepLinkError::UnknownParameter(_))
        ));
    }

    #[test]
    fn control_characters_in_connection_name_are_rejected() {
        assert!(matches!(
            parse_deep_link("sqlly://open?connection=bad%0Aname"),
            Err(DeepLinkError::ControlCharacters("connection"))
        ));
    }

    #[test]
    fn new_connection_link_reuses_connection_url_parser() {
        let link = parse_deep_link("sqlly://connection/new?url=postgres%3A%2F%2Fu%40h%3A5432%2Fdb")
            .unwrap();
        let DeepLink::NewConnection { parsed } = link else {
            panic!("expected new connection link");
        };
        assert_eq!(parsed.engine, DatabaseType::Postgres);
        assert_eq!(parsed.server, "h");
        assert_eq!(parsed.database.as_deref(), Some("db"));
    }

    #[test]
    fn database_paths_are_recognised_case_insensitively_and_must_be_absolute() {
        assert!(is_sqlite_path(Path::new("/tmp/foo.SQLite")));
        assert!(is_duckdb_path(Path::new("/tmp/foo.DuckDB")));
        assert!(is_duckdb_path(Path::new("/tmp/foo.ddb")));
        assert!(!is_sqlite_path(Path::new("/tmp/foo.duckdb")));
        assert_eq!(
            file_database_engine(Path::new("/tmp/foo.duckdb")),
            Some(DatabaseType::DuckDb)
        );
        assert_eq!(file_database_engine(Path::new("/tmp/foo.txt")), None);
        assert!(matches!(
            parse_deep_link("relative.db"),
            Err(DeepLinkError::NotDatabaseFile)
        ));
        assert!(matches!(
            parse_deep_link("relative.duckdb"),
            Err(DeepLinkError::NotDatabaseFile)
        ));
        // Use a platform-appropriate absolute path: `/tmp/...` is not absolute
        // on Windows (an absolute path there needs a drive), so a unix literal
        // would be rejected by the is_absolute() gate and panic the test.
        let (sqlite, duckdb) = if cfg!(windows) {
            (r"C:\tmp\demo.sqlite", r"C:\tmp\demo.duckdb")
        } else {
            ("/tmp/demo.sqlite", "/tmp/demo.duckdb")
        };
        assert_eq!(
            parse_deep_link(sqlite).unwrap(),
            DeepLink::OpenDatabaseFile {
                path: PathBuf::from(sqlite),
                engine: DatabaseType::Sqlite,
            }
        );
        let link = parse_deep_link(duckdb).unwrap();
        assert_eq!(
            link,
            DeepLink::OpenDatabaseFile {
                path: PathBuf::from(duckdb),
                engine: DatabaseType::DuckDb,
            }
        );
        assert_eq!(describe(&link), "Open DuckDB database \"demo.duckdb\"");
        assert_eq!(
            file_connection_name(Path::new(duckdb), DatabaseType::DuckDb),
            "demo"
        );
    }

    #[test]
    fn describe_never_echoes_sql_or_dsn() {
        let link = parse_deep_link("sqlly://query?connection=prod&sql=DROP%20TABLE%20x").unwrap();
        let description = describe(&link);
        assert!(!description.contains("DROP"));
        let link = parse_deep_link("sqlly://connection/new?url=postgres%3A%2F%2Fsecret").unwrap();
        let description = describe(&link);
        assert!(!description.contains("secret"));
        assert!(!description.contains("postgres://"));
    }

    #[test]
    fn file_url_to_sqlite_path_is_decoded() {
        // A `file://` URL must carry a drive on Windows for `to_file_path()` to
        // resolve it; the unix form has no drive. Assert the decoded (space
        // un-escaped) path in the host's own path syntax.
        let (url, expected) = if cfg!(windows) {
            ("file:///C:/tmp/my%20data.db", r"C:\tmp\my data.db")
        } else {
            ("file:///tmp/my%20data.db", "/tmp/my data.db")
        };
        let link = parse_deep_link(url).unwrap();
        assert_eq!(
            link,
            DeepLink::OpenDatabaseFile {
                path: PathBuf::from(expected),
                engine: DatabaseType::Sqlite,
            }
        );
    }
}
