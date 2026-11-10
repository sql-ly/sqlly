//! Basic "can we connect?" smoke tests for the live databases the pipeline
//! provisions through the `TestDatabaseCredentials` variable group: PostgreSQL,
//! MySQL, MariaDB, and Oracle. Each test reads its own
//! `<DB>_HOST/PORT/USER/PASS/DATABASE` env vars (the exact variable names in
//! that group) and skips silently when they are absent, so the suite is a no-op
//! on any machine without those databases — local dev and the hosted CI agents
//! included.
//!
//! In the release pipeline these run ONLY on the macOS agent, which has all four
//! databases listening on localhost. The Linux and Windows jobs pass
//! `-- --skip live_db_connect` to `cargo test`, which excludes the whole file
//! (those agents host no databases). Every test name is therefore prefixed with
//! `live_db_connect_` so that one skip filter covers all of them.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_engines::mysql::{execute_params_on_client, query_rows_on_client};
use sqlly_services::alter::{CommentChange, CommentObjectKind, render_comment_change};

/// Build a SQL-login profile from a `<prefix>_*` env var family, or return
/// `None` (→ skip) when the host or user are not configured. The password is
/// resolved lazily by the driver from the `<prefix>_PASS` env var via
/// `SecretProvider::Environment`.
fn env_profile(
    prefix: &str,
    db_type: DatabaseType,
    default_port: u16,
) -> Option<ConnectionProfile> {
    let host = std::env::var(format!("{prefix}_HOST")).ok()?;
    let user = std::env::var(format!("{prefix}_USER")).ok()?;
    let mut profile = ConnectionProfile::sql_login(
        format!("{prefix}-connect"),
        host,
        user,
        SecretRef {
            provider: SecretProvider::Environment,
            key: format!("{prefix}_PASS"),
        },
    );
    profile.port = std::env::var(format!("{prefix}_PORT"))
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default_port);
    profile.database = std::env::var(format!("{prefix}_DATABASE")).ok();
    profile.database_type = db_type;
    // The pipeline's databases run without TLS on localhost.
    profile.encrypt = false;
    Some(profile)
}

#[tokio::test]
async fn live_db_connect_postgres() {
    let Some(profile) = env_profile("POSTGRES", DatabaseType::Postgres, 5432) else {
        eprintln!("skipping: POSTGRES_* env vars not set");
        return;
    };
    let client = sqlly_engines::pg::open_client(&profile)
        .await
        .expect("connect to PostgreSQL");
    drop(client);
}

#[tokio::test]
async fn live_db_connect_mysql() {
    let Some(profile) = env_profile("MYSQL", DatabaseType::MySql, 3306) else {
        eprintln!("skipping: MYSQL_* env vars not set");
        return;
    };
    let client = sqlly_engines::mysql::open_client(&profile)
        .await
        .expect("connect to MySQL");
    drop(client);
}

#[tokio::test]
async fn live_db_connect_mariadb() {
    let Some(profile) = env_profile("MARIADB", DatabaseType::MariaDb, 3306) else {
        eprintln!("skipping: MARIADB_* env vars not set");
        return;
    };
    // MariaDB speaks the MySQL wire protocol, so the MySQL driver connects to it.
    let client = sqlly_engines::mysql::open_client(&profile)
        .await
        .expect("connect to MariaDB");
    drop(client);
}

/// Fetch a table's `SHOW CREATE TABLE` text.
async fn show_create(client: &sqlly_engines::mysql::MySqlClient, table: &str) -> String {
    let definition = query_rows_on_client(client, &format!("SHOW CREATE TABLE {table}"), &[])
        .await
        .expect("read table definition");
    match &definition[0].rows[0][1] {
        CellValue::String(text) => text.clone(),
        other => panic!("unexpected SHOW CREATE TABLE cell: {other:?}"),
    }
}

/// Render a comment-only edit of `column` through the real renderer, from the
/// server's own `SHOW CREATE TABLE` text.
fn render_column_comment(db: DatabaseType, table: &str, column: &str, ddl: &str) -> String {
    render_comment_change(
        db,
        None,
        table,
        CommentObjectKind::Table,
        &CommentChange {
            column: Some(column.into()),
            from: None,
            to: Some("stable label".into()),
        },
        Some(ddl),
    )
    .expect("render comment change")
    .expect("comment change produces SQL")
}

#[tokio::test]
async fn live_db_connect_mysql_stored_generated_comment_does_not_rebuild() {
    let Some(profile) = env_profile("MYSQL", DatabaseType::MySql, 3306) else {
        eprintln!("skipping: MYSQL_* env vars not set");
        return;
    };
    let client = sqlly_engines::mysql::open_client(&profile)
        .await
        .expect("connect to MySQL");
    let table = format!("sqlly_comment_guard_{}", std::process::id());
    execute_params_on_client(
        &client,
        &format!("CREATE TABLE {table} (id INT PRIMARY KEY, label_stored VARCHAR(32) GENERATED ALWAYS AS (CONCAT('row-', id)) STORED) ENGINE=InnoDB"),
        &[],
    )
    .await
    .expect("create generated-column fixture");
    // InnoDB assigns a new TABLE_ID whenever it rebuilds the table, so an
    // unchanged id proves the comment edit stayed metadata-only.
    let table_id_sql = format!(
        "SELECT TABLE_ID FROM information_schema.INNODB_TABLES WHERE NAME = CONCAT(DATABASE(), '/{table}')"
    );
    let before = query_rows_on_client(&client, &table_id_sql, &[])
        .await
        .expect("read table id before comment edit");

    let sql = render_column_comment(
        DatabaseType::MySql,
        &table,
        "label_stored",
        &show_create(&client, &table).await,
    );
    let altered = execute_params_on_client(&client, sql.trim_end(), &[]).await;
    let ddl_after = show_create(&client, &table).await;
    let after = query_rows_on_client(&client, &table_id_sql, &[])
        .await
        .expect("read table id after comment edit");
    let _ = execute_params_on_client(&client, &format!("DROP TABLE {table}"), &[]).await;

    altered.expect("MySQL accepts the metadata-only comment edit");
    assert!(
        ddl_after.contains("COMMENT 'stable label'"),
        "comment edit did not apply: {ddl_after}"
    );
    assert_eq!(
        before[0].rows, after[0].rows,
        "comment edit rebuilt the table"
    );
}

#[tokio::test]
async fn live_db_connect_mariadb_stored_generated_comment_refuses_rebuild() {
    let Some(profile) = env_profile("MARIADB", DatabaseType::MariaDb, 3306) else {
        eprintln!("skipping: MARIADB_* env vars not set");
        return;
    };
    let client = sqlly_engines::mysql::open_client(&profile)
        .await
        .expect("connect to MariaDB");
    execute_params_on_client(
        &client,
        "CREATE TEMPORARY TABLE sqlly_comment_guard (id INT PRIMARY KEY, label_stored VARCHAR(32) AS (CONCAT('row-', id)) PERSISTENT) ENGINE=InnoDB",
        &[],
    )
    .await
    .expect("create generated-column fixture");

    let sql = render_column_comment(
        DatabaseType::MariaDb,
        "sqlly_comment_guard",
        "label_stored",
        &show_create(&client, "sqlly_comment_guard").await,
    );
    // MariaDB can only change a persistent column's comment by copying the
    // table, which regenerates every value; the rendered edit must refuse.
    let error = execute_params_on_client(&client, sql.trim_end(), &[])
        .await
        .expect_err("MariaDB must refuse the instant comment edit");
    let message = error.to_string().to_ascii_lowercase();
    assert!(
        message.contains("algorithm=instant") && message.contains("not supported"),
        "unexpected comment edit error: {error}"
    );
    let ddl_after = show_create(&client, "sqlly_comment_guard").await;
    assert!(
        !ddl_after.contains("stable label"),
        "the table was altered anyway: {ddl_after}"
    );
}

#[tokio::test]
async fn live_db_connect_oracle() {
    // A real logon through the Oracle driver (needs the Instant Client on
    // the agent); ORACLE_DATABASE is the service name.
    let Some(profile) = env_profile("ORACLE", DatabaseType::Oracle, 1521) else {
        eprintln!("skipping: ORACLE_* env vars not set");
        return;
    };
    let client = sqlly_engines::oracle::open_client(&profile)
        .await
        .expect("connect to Oracle");
    drop(client);
}
