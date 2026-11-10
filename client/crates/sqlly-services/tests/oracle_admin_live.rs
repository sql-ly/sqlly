//! Live Oracle checks for the admin tools that read the dictionary or write
//! DDL: the Index Analyzer usage query and its DROP / REBUILD scripts, the
//! structure editor (a real snapshot through the metadata dialect, an
//! ALTER script rendered from a change set, executed and verified), and the
//! Security editor's catalog queries and account scripts.
//!
//! Opt-in through `SQLLY_ORACLE_SERVER` / `SQLLY_ORACLE_USER` /
//! `SQLLY_ORACLE_PASSWORD` (optional `SQLLY_ORACLE_PORT`, `SQLLY_ORACLE_SERVICE`,
//! default `FREEPDB1`). The login needs `SELECT ANY DICTIONARY` (or SELECT
//! on `DBA_INDEX_USAGE`, `DBA_USERS`, `DBA_ROLES`, `DBA_ROLE_PRIVS`,
//! `DBA_SYS_PRIVS`, `DBA_TAB_PRIVS`, `DBA_PROFILES`), `CREATE USER`, `ALTER
//! USER`, `DROP USER`, `CREATE ROLE`, `DROP ANY ROLE`, `GRANT ANY ROLE`, and
//! `CREATE SESSION WITH ADMIN OPTION`; the account and index checks that
//! need them are skipped with a note when they are missing, unless
//! `SQLLY_ORACLE_EXPECT_CATALOG=1`.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_engines::oracle::{
    OracleClient, execute_batch_on_client, open_client, query_rows_on_client,
};
use sqlly_services::admin::index_usage::{IndexDialect, IndexUsage};
use sqlly_services::admin::security_sql::oracle as security;
use sqlly_services::alter::{DraftColumn, IndexDef, RenderOptions, TableDraft, diff, render};
use sqlly_services::maintenance::{MaintenanceOp, MaintenanceTarget, maintenance_sql};
use sqlly_services::{AdminToolsService, DefaultAdminToolsService, DefaultQueryExecutor};
use tokio::sync::OnceCell;

fn live_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_ORACLE_SERVER").ok()?;
    let user = std::env::var("SQLLY_ORACLE_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "oracle-admin",
        server,
        user,
        SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_ORACLE_PASSWORD".into(),
        },
    );
    p.port = std::env::var("SQLLY_ORACLE_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1521);
    p.database = Some(std::env::var("SQLLY_ORACLE_SERVICE").unwrap_or_else(|_| "FREEPDB1".into()));
    p.database_type = DatabaseType::Oracle;
    p.encrypt = false;
    Some(p)
}

/// Fixture objects carry a `SQLLY_AD_` prefix so this binary never collides
/// with the other Oracle test binaries running against the same schema.
const SCHEMA_SQL: &[&str] = &[
    "CREATE TABLE sqlly_ad_dept (id NUMBER(10) CONSTRAINT sqlly_ad_dept_pk PRIMARY KEY, name VARCHAR2(50) NOT NULL)",
    "CREATE TABLE sqlly_ad_emp (id NUMBER(10) CONSTRAINT sqlly_ad_emp_pk PRIMARY KEY, name VARCHAR2(50) NOT NULL, dept_id NUMBER(10), salary NUMBER(12,2) DEFAULT 0, CONSTRAINT sqlly_ad_emp_dept_fk FOREIGN KEY (dept_id) REFERENCES sqlly_ad_dept (id))",
    "CREATE INDEX sqlly_ad_emp_name_ix ON sqlly_ad_emp (name)",
    "CREATE VIEW sqlly_ad_emp_v AS SELECT e.id, e.name FROM sqlly_ad_emp e",
    "INSERT INTO sqlly_ad_dept (id, name) VALUES (1, 'Eng')",
    "INSERT INTO sqlly_ad_emp (id, name, dept_id, salary) VALUES (1, 'Ada', 1, 10)",
];

const DROP_SQL: &[&str] = &[
    "VIEW sqlly_ad_emp_v",
    "TABLE sqlly_ad_emp",
    "TABLE sqlly_ad_dept",
    "USER sqlly_ad_reader CASCADE",
    "ROLE sqlly_ad_readers",
];

struct Fixture {
    profile: ConnectionProfile,
    schema: String,
}

static FIXTURE: OnceCell<Option<Fixture>> = OnceCell::const_new();

/// Held by every test that changes `SQLLY_AD_EMP`'s indexes or columns. The
/// Index Analyzer test drops and recreates `SQLLY_AD_EMP_NAME_IX` while the
/// structure-editor test re-keys it (drop + create) and renames its column;
/// run in parallel, one saw the index missing mid-change — ORA-01418 on the
/// REBUILD, or an empty index list in the snapshot. Libtest runs the tests
/// of one binary on parallel threads, so they take turns on the table.
static EMP_TABLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn fixture() -> Option<&'static Fixture> {
    FIXTURE
        .get_or_init(|| async {
            let profile = live_profile()?;
            let client = open_client(&profile).await.expect("connect to Oracle");
            drop_fixture(&client).await;
            for sql in SCHEMA_SQL {
                execute_batch_on_client(&client, sql)
                    .await
                    .unwrap_or_else(|e| panic!("fixture statement failed:\n{sql}\n{e}"));
            }
            let schema = match &rows(&client, "SELECT USER FROM DUAL").await[0][0] {
                CellValue::String(s) => s.clone(),
                other => panic!("USER: {other:?}"),
            };
            Some(Fixture { profile, schema })
        })
        .await
        .as_ref()
}

async fn drop_fixture(client: &OracleClient) {
    for object in DROP_SQL {
        execute_batch_on_client(
            client,
            &format!(
                "BEGIN EXECUTE IMMEDIATE 'DROP {object}'; EXCEPTION WHEN OTHERS THEN NULL; END;"
            ),
        )
        .await
        .unwrap();
    }
}

macro_rules! live {
    () => {
        match fixture().await {
            Some(f) => f,
            None => {
                eprintln!("skipping: SQLLY_ORACLE_SERVER/USER not set");
                return;
            }
        }
    };
}

async fn rows(client: &OracleClient, sql: &str) -> Vec<Vec<CellValue>> {
    match query_rows_on_client(client, sql, &[]).await {
        Ok(mut sets) => sets.remove(0).rows,
        Err(e) => panic!("Oracle rejected generated SQL:\n{sql}\n{e}"),
    }
}

fn text(cell: &CellValue) -> String {
    match cell {
        CellValue::String(s) => s.clone(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => f.to_string(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::Null => String::new(),
        CellValue::Bytes(_) => "<bytes>".into(),
    }
}

fn privilege_error(error: &str) -> bool {
    std::env::var("SQLLY_ORACLE_EXPECT_CATALOG").is_err()
        && (error.contains("ORA-00942") || error.contains("ORA-01031"))
}

/// Run a generated script, tolerating a missing privilege unless the
/// environment says the catalog must be there.
async fn run_script(client: &OracleClient, sql: &str) -> bool {
    match execute_batch_on_client(client, sql).await {
        Ok(()) => true,
        Err(e) if privilege_error(&e.to_string()) => {
            eprintln!("skipping (privilege): {sql}\n{e}");
            false
        }
        Err(e) => panic!("Oracle rejected generated SQL:\n{sql}\n{e}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn index_usage_query_flags_enforcing_indexes_and_scripts_drop_rebuild() {
    let f = live!();
    let _table = EMP_TABLE.lock().await;
    let client = open_client(&f.profile).await.unwrap();
    let sql = IndexUsage::usage_query_for(IndexDialect::Oracle);
    let sets = match query_rows_on_client(&client, sql, &[]).await {
        Ok(sets) => sets,
        Err(e) if privilege_error(&e.to_string()) => {
            eprintln!("skipping: DBA_INDEX_USAGE not granted: {e}");
            return;
        }
        Err(e) => panic!("index usage query failed:\n{sql}\n{e}"),
    };
    let set = &sets[0];
    let col = |name: &str| {
        set.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
            .unwrap_or_else(|| panic!("missing column {name}"))
    };
    let (name_at, enforcing_at, table_at, schema_at, unusable_at) = (
        col("index_name"),
        col("is_enforcing"),
        col("table_name"),
        col("schema_name"),
        col("is_unusable"),
    );
    let find = |index: &str| {
        set.rows
            .iter()
            .find(|r| text(&r[name_at]) == index)
            .unwrap_or_else(|| panic!("{index} missing from usage rows"))
    };
    let plain = find("SQLLY_AD_EMP_NAME_IX");
    assert_eq!(text(&plain[enforcing_at]), "0");
    assert_eq!(text(&plain[unusable_at]), "0");
    assert_eq!(text(&plain[table_at]), "SQLLY_AD_EMP");
    assert_eq!(text(&plain[schema_at]), f.schema);
    let pk = find("SQLLY_AD_EMP_PK");
    assert_eq!(text(&pk[enforcing_at]), "1");

    // REBUILD then DROP the plain index with the scripts the pane offers.
    let rebuild = maintenance_sql(
        DatabaseType::Oracle,
        MaintenanceOp::Reindex,
        &MaintenanceTarget::Index {
            schema: Some(f.schema.clone()),
            table: "SQLLY_AD_EMP".into(),
            name: "SQLLY_AD_EMP_NAME_IX".into(),
        },
        &[],
    )
    .unwrap();
    execute_batch_on_client(&client, &rebuild).await.unwrap();
    let admin = DefaultAdminToolsService;
    let drop = admin
        .index_drop_sql_for(
            DatabaseType::Oracle,
            &f.schema,
            "SQLLY_AD_EMP",
            "SQLLY_AD_EMP_NAME_IX",
        )
        .unwrap();
    execute_batch_on_client(&client, &drop).await.unwrap();
    let left = rows(
        &client,
        "SELECT COUNT(*) FROM user_indexes WHERE index_name = 'SQLLY_AD_EMP_NAME_IX'",
    )
    .await;
    assert_eq!(text(&left[0][0]), "0");
    execute_batch_on_client(&client, SCHEMA_SQL[2])
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn alter_script_from_a_real_snapshot_executes() {
    let f = live!();
    let _table = EMP_TABLE.lock().await;
    let queries = DefaultQueryExecutor::new();
    let snapshot = sqlly_services::load_table_snapshot(
        &queries,
        &f.profile,
        "",
        Some(&f.schema),
        "SQLLY_AD_EMP",
    )
    .await
    .expect("load Oracle snapshot");
    assert_eq!(snapshot.primary_key, vec!["ID".to_string()]);
    assert!(snapshot.columns.iter().any(|c| c.name == "NAME"));
    assert!(
        snapshot
            .foreign_keys
            .iter()
            .any(|fk| fk.name == "SQLLY_AD_EMP_DEPT_FK"),
        "{:?}",
        snapshot.foreign_keys
    );
    assert!(
        snapshot
            .indexes
            .iter()
            .any(|i| i.name == "SQLLY_AD_EMP_NAME_IX"),
        "{:?}",
        snapshot.indexes
    );

    // Rename NAME -> FULL_NAME, widen it, add NOTE, re-key the index onto the
    // renamed column, and add a second index.
    let mut draft = TableDraft {
        schema: snapshot.schema.clone(),
        table: snapshot.table.clone(),
        columns: snapshot
            .columns
            .iter()
            .map(|c| DraftColumn {
                origin: Some(c.name.clone()),
                def: c.clone(),
            })
            .collect(),
        primary_key: snapshot.primary_key.clone(),
        pk_constraint_name: snapshot.pk_constraint_name.clone(),
        indexes: snapshot.indexes.clone(),
        foreign_keys: snapshot.foreign_keys.clone(),
        table_comment: snapshot.table_comment.clone(),
        column_comments: snapshot.column_comments.clone(),
        source_ddl: snapshot.stored_table_ddl.clone(),
    };
    let name = draft
        .columns
        .iter_mut()
        .find(|c| c.def.name == "NAME")
        .unwrap();
    name.def.name = "FULL_NAME".into();
    name.def.data_type = "VARCHAR2(80)".into();
    draft.columns.push(DraftColumn {
        origin: None,
        def: sqlly_services::alter::ColumnDef {
            name: "NOTE".into(),
            data_type: "VARCHAR2(20)".into(),
            nullable: true,
            identity: false,
            default: Some("'n/a'".into()),
            default_constraint_name: None,
        },
    });
    for index in &mut draft.indexes {
        if index.name == "SQLLY_AD_EMP_NAME_IX" {
            index.columns = vec!["FULL_NAME".into()];
        }
    }
    draft.indexes.push(IndexDef {
        name: "SQLLY_AD_EMP_SALARY_IX".into(),
        columns: vec!["SALARY".into()],
        unique: false,
        include: vec![],
        filter: None,
        method: None,
        fillfactor: None,
    });
    let set = diff(&snapshot, &draft);
    let script = render(
        DatabaseType::Oracle,
        &snapshot,
        &draft,
        &set,
        &RenderOptions::default(),
    )
    .unwrap();
    eprintln!("{}", script.sql);
    assert!(
        script
            .sql
            .contains("RENAME COLUMN \"NAME\" TO \"FULL_NAME\"")
    );

    let client = open_client(&f.profile).await.unwrap();
    execute_batch_on_client(&client, &script.sql).await.unwrap();
    let columns = rows(
        &client,
        "SELECT column_name, data_type, data_length, data_default FROM user_tab_columns WHERE table_name = 'SQLLY_AD_EMP' ORDER BY column_id",
    )
    .await;
    let names: Vec<String> = columns.iter().map(|r| text(&r[0])).collect();
    assert!(names.contains(&"FULL_NAME".to_string()), "{names:?}");
    assert!(names.contains(&"NOTE".to_string()), "{names:?}");
    assert!(!names.contains(&"NAME".to_string()), "{names:?}");
    let full_name = columns.iter().find(|r| text(&r[0]) == "FULL_NAME").unwrap();
    assert_eq!(text(&full_name[2]), "80");
    let indexes = rows(
        &client,
        "SELECT index_name FROM user_indexes WHERE table_name = 'SQLLY_AD_EMP' ORDER BY 1",
    )
    .await;
    let index_names: Vec<String> = indexes.iter().map(|r| text(&r[0])).collect();
    assert!(index_names.contains(&"SQLLY_AD_EMP_SALARY_IX".to_string()));
    assert!(index_names.contains(&"SQLLY_AD_EMP_NAME_IX".to_string()));
    // The dependent view was left alone (not dropped) but the rename broke
    // its `e.name` reference, exactly as the script's warning predicts:
    // Oracle marks it INVALID and the recompile fails until it is rewritten.
    assert!(script.sql.contains("invalidated by this change"));
    let status = rows(
        &client,
        "SELECT status FROM user_objects WHERE object_name = 'SQLLY_AD_EMP_V'",
    )
    .await;
    assert_eq!(text(&status[0][0]), "INVALID");
    let select = query_rows_on_client(&client, "SELECT COUNT(*) FROM sqlly_ad_emp_v", &[]).await;
    assert!(
        select.is_err_and(|e| e.to_string().contains("ORA-04063")),
        "the view should fail to recompile after the rename"
    );

    // Put the fixture back for the other tests in this binary, including
    // the view the rename invalidated.
    execute_batch_on_client(
        &client,
        "DROP INDEX sqlly_ad_emp_salary_ix;\nALTER TABLE sqlly_ad_emp DROP (note);\nALTER TABLE sqlly_ad_emp RENAME COLUMN full_name TO name;\nALTER TABLE sqlly_ad_emp MODIFY (name VARCHAR2(50));\nCREATE OR REPLACE VIEW sqlly_ad_emp_v AS SELECT e.id, e.name FROM sqlly_ad_emp e",
    )
    .await
    .unwrap();
    let view_rows = rows(&client, "SELECT COUNT(*) FROM sqlly_ad_emp_v").await;
    assert_eq!(text(&view_rows[0][0]), "1");
}

#[tokio::test(flavor = "multi_thread")]
async fn security_queries_run_and_account_scripts_round_trip() {
    let f = live!();
    let client = open_client(&f.profile).await.unwrap();
    let admin = DefaultAdminToolsService;
    let db = DatabaseType::Oracle;

    // Every catalog set the Roles tab (and its compat fallback) reads.
    for kind in [
        "roles",
        "roles-compat",
        "role-members",
        "role-members-compat",
        "db-connect",
        "db-connect-compat",
        "schema-usage",
        "tables",
        "users",
        "users-compat",
        "dba-roles",
        "profiles",
    ] {
        let sql = admin.security_query_for(db, kind).unwrap();
        match query_rows_on_client(&client, &sql, &[]).await {
            Ok(_) => {}
            Err(e) if privilege_error(&e.to_string()) => eprintln!("{kind}: skipped ({e})"),
            Err(e) => panic!("{kind} query failed:\n{sql}\n{e}"),
        }
    }
    for sql in [
        admin
            .security_table_grants_query_for(db, &f.schema)
            .unwrap(),
        security::table_grants_query_compat(&f.schema),
        security::system_privileges_query(&f.schema),
        security::system_privileges_query_compat().to_string(),
    ] {
        match query_rows_on_client(&client, &sql, &[]).await {
            Ok(_) => {}
            Err(e) if privilege_error(&e.to_string()) => eprintln!("skipped ({e}): {sql}"),
            Err(e) => panic!("query failed:\n{sql}\n{e}"),
        }
    }
    // The compat sets are answerable by any login and list the session user.
    let compat = rows(&client, security::roles_query_compat()).await;
    assert!(compat.iter().any(|r| text(&r[0]) == f.schema), "{compat:?}");
    let tables = rows(&client, security::tables_query()).await;
    assert!(
        tables
            .iter()
            .any(|r| text(&r[0]) == f.schema && text(&r[1]) == "SQLLY_AD_EMP"),
        "{tables:?}"
    );

    // Create a role and a user with the generated scripts, grant, lock,
    // unlock, reset the password, revoke, and drop — each verified against
    // the dictionary.
    if !run_script(&client, &security::create_role_sql("sqlly_ad_readers")).await {
        return;
    }
    if !run_script(
        &client,
        &security::create_user_sql("sqlly_ad_reader", "Sqlly#Ad1", None, None),
    )
    .await
    {
        return;
    }
    let scripts = [
        security::grant_create_session_sql("SQLLY_AD_READER"),
        security::grant_role_sql("SQLLY_AD_READERS", "SQLLY_AD_READER", false),
        security::grant_table_sql(
            &f.schema,
            "SQLLY_AD_EMP",
            &[
                sqlly_services::admin::security_sql::postgres::TablePrivilege::Select,
                sqlly_services::admin::security_sql::postgres::TablePrivilege::Update,
            ],
            "SQLLY_AD_READER",
        ),
        security::alter_user_lock_sql("SQLLY_AD_READER", true),
    ];
    for sql in &scripts {
        if !run_script(&client, sql).await {
            return;
        }
    }
    if let Ok(sets) = query_rows_on_client(&client, security::roles_query(), &[]).await {
        let reader = sets[0]
            .rows
            .iter()
            .find(|r| text(&r[0]) == "SQLLY_AD_READER")
            .expect("new user listed");
        assert_eq!(text(&reader[5]), "1", "users can log in");
        let role = sets[0]
            .rows
            .iter()
            .find(|r| text(&r[0]) == "SQLLY_AD_READERS")
            .expect("new role listed");
        assert_eq!(text(&role[5]), "0", "roles cannot log in");
        let members = rows(&client, &security::role_members_query()).await;
        assert!(
            members
                .iter()
                .any(|r| text(&r[0]) == "SQLLY_AD_READERS" && text(&r[1]) == "SQLLY_AD_READER")
        );
        let connect = rows(&client, &security::database_connect_grants_query()).await;
        assert!(connect.iter().any(|r| text(&r[1]) == "SQLLY_AD_READER"));
        let grants = rows(&client, &security::table_grants_query("SQLLY_AD_READER")).await;
        let privs: Vec<String> = grants.iter().map(|r| text(&r[2])).collect();
        assert!(privs.contains(&"SELECT".to_string()) && privs.contains(&"UPDATE".to_string()));
        let status = rows(
            &client,
            "SELECT account_status FROM dba_users WHERE username = 'SQLLY_AD_READER'",
        )
        .await;
        assert_eq!(text(&status[0][0]), "LOCKED");
    }
    let scripts = [
        security::alter_user_lock_sql("SQLLY_AD_READER", false),
        security::alter_user_password_sql("SQLLY_AD_READER", "Sqlly#Ad2"),
        security::revoke_table_sql(
            &f.schema,
            "SQLLY_AD_EMP",
            &[sqlly_services::admin::security_sql::postgres::TablePrivilege::Update],
            "SQLLY_AD_READER",
        ),
        security::revoke_role_sql("SQLLY_AD_READERS", "SQLLY_AD_READER"),
        security::revoke_create_session_sql("SQLLY_AD_READER"),
        security::drop_user_sql("SQLLY_AD_READER", true),
        security::drop_role_sql("SQLLY_AD_READERS"),
    ];
    for sql in &scripts {
        if !run_script(&client, sql).await {
            return;
        }
    }
    let gone = rows(
        &client,
        "SELECT COUNT(*) FROM all_users WHERE username = 'SQLLY_AD_READER'",
    )
    .await;
    assert_eq!(text(&gone[0][0]), "0");
}
