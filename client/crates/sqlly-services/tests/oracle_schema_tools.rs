//! Schema Compare / Schema History / Data Compare on a live Oracle
//! Database: the DBMS_METADATA collector batch runs on one session, every
//! fixture object comes back under its expected kind, and a deploy script
//! planned from a before/after diff actually recreates the dropped objects.
//!
//! Opt-in through `SQLLY_ORACLE_SERVER` / `SQLLY_ORACLE_USER` /
//! `SQLLY_ORACLE_PASSWORD` (optional `SQLLY_ORACLE_PORT`, `SQLLY_ORACLE_SERVICE`,
//! default `FREEPDB1`), exactly like `oracle_dialects.rs`. The fixture
//! objects carry a `SQLLY_SC_` prefix so this binary never collides with the
//! `SQLLY_` fixture of the dialect tests when both run against one schema.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{
    CellValue, ConnectionProfile, DatabaseType, EventBus, EventPayload, SecretProvider, SecretRef,
};
use sqlly_engines::oracle::{
    OracleClient, execute_batch_on_client, execute_on_client, open_client, query_rows_on_client,
};
use sqlly_services::SchemaObject;
use sqlly_services::admin::schema_plan::{
    DeployOptions, DifferenceStatus, ORACLE_OBJECT_KINDS, compare_objects, deploy_script,
    oracle_collect_objects_sql, plan_deploy,
};
use sqlly_services::datacompare::script::{SyncDirection, SyncScriptRequest, generate_sync_script};
use sqlly_services::datacompare::{ChangedPair, DataDiff};
use tokio::sync::OnceCell;

fn live_profile() -> Option<ConnectionProfile> {
    let server = std::env::var("SQLLY_ORACLE_SERVER").ok()?;
    let user = std::env::var("SQLLY_ORACLE_USER").ok()?;
    let mut p = ConnectionProfile::sql_login(
        "oracle-schema-tools",
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

const SCHEMA_SQL: &[&str] = &[
    "CREATE SEQUENCE sqlly_sc_seq START WITH 100 INCREMENT BY 5",
    "CREATE TYPE sqlly_sc_money AS OBJECT (amount NUMBER, currency VARCHAR2(3))",
    "CREATE TABLE sqlly_sc_customers (id NUMBER(10) PRIMARY KEY, name VARCHAR2(50) NOT NULL, CONSTRAINT sqlly_sc_customers_name_uq UNIQUE (name))",
    "CREATE TABLE sqlly_sc_orders (id NUMBER(10) DEFAULT sqlly_sc_seq.NEXTVAL PRIMARY KEY, customer_id NUMBER(10) NOT NULL, amount NUMBER(12,2), placed_at TIMESTAMP DEFAULT SYSTIMESTAMP, CONSTRAINT sqlly_sc_orders_amount_ck CHECK (amount >= 0), CONSTRAINT sqlly_sc_orders_customer_fk FOREIGN KEY (customer_id) REFERENCES sqlly_sc_customers (id) ON DELETE CASCADE)",
    "CREATE INDEX sqlly_sc_orders_customer_ix ON sqlly_sc_orders (customer_id, placed_at DESC)",
    "CREATE VIEW sqlly_sc_big_orders AS SELECT o.id, c.name, o.amount FROM sqlly_sc_orders o JOIN sqlly_sc_customers c ON c.id = o.customer_id WHERE o.amount > 100",
    "CREATE OR REPLACE PROCEDURE sqlly_sc_add_customer(p_id IN NUMBER, p_name IN VARCHAR2) AS BEGIN INSERT INTO sqlly_sc_customers (id, name) VALUES (p_id, p_name); END;",
    "CREATE OR REPLACE FUNCTION sqlly_sc_tax(p_amount IN NUMBER) RETURN NUMBER AS BEGIN RETURN p_amount * 0.2; END;",
    "CREATE OR REPLACE PACKAGE sqlly_sc_pkg AS PROCEDURE ping(p_x IN NUMBER); END sqlly_sc_pkg;",
    "CREATE OR REPLACE PACKAGE BODY sqlly_sc_pkg AS PROCEDURE ping(p_x IN NUMBER) IS BEGIN NULL; END; END sqlly_sc_pkg;",
    "CREATE OR REPLACE TRIGGER sqlly_sc_orders_biu BEFORE INSERT ON sqlly_sc_orders FOR EACH ROW BEGIN IF :NEW.amount IS NULL THEN :NEW.amount := 0; END IF; END;",
    "CREATE SYNONYM sqlly_sc_cust FOR sqlly_sc_customers",
    "INSERT INTO sqlly_sc_customers (id, name) VALUES (1, 'Ada')",
    "INSERT INTO sqlly_sc_orders (customer_id, amount) VALUES (1, 150)",
    "CREATE MATERIALIZED VIEW sqlly_sc_order_totals AS SELECT customer_id, SUM(amount) AS total FROM sqlly_sc_orders GROUP BY customer_id",
];

const DROP_SQL: &[&str] = &[
    "MATERIALIZED VIEW sqlly_sc_order_totals",
    "SYNONYM sqlly_sc_cust",
    "PACKAGE sqlly_sc_pkg",
    "FUNCTION sqlly_sc_tax",
    "PROCEDURE sqlly_sc_add_customer",
    "VIEW sqlly_sc_big_orders",
    "TABLE sqlly_sc_orders PURGE",
    "TABLE sqlly_sc_customers PURGE",
    "TYPE sqlly_sc_money",
    "SEQUENCE sqlly_sc_seq",
];

struct Fixture {
    profile: ConnectionProfile,
    schema: String,
}

static FIXTURE: OnceCell<Option<Fixture>> = OnceCell::const_new();

async fn drop_all(client: &OracleClient) {
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

async fn fixture() -> Option<&'static Fixture> {
    FIXTURE
        .get_or_init(|| async {
            let profile = live_profile()?;
            let client = open_client(&profile).await.expect("connect to Oracle");
            drop_all(&client).await;
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

fn text(cell: Option<&CellValue>) -> String {
    match cell {
        Some(CellValue::String(s)) => s.clone(),
        Some(CellValue::Null) | None => String::new(),
        Some(other) => format!("{other:?}"),
    }
}

/// Run a whole batch on one session (the PL/SQL transform block and every
/// SELECT must share a session for the transforms to apply) and gather the
/// four-column rows into schema objects, exactly as the Schema Compare pane
/// does from the tool-dialog outcome.
async fn run_batch(
    client: &OracleClient,
    profile: &ConnectionProfile,
    sql: &str,
) -> Vec<SchemaObject> {
    let events = EventBus::new(8192);
    let mut sub = events.subscribe();
    let started = std::time::Instant::now();
    execute_on_client(client, profile, sql, &events, None)
        .await
        .unwrap_or_else(|e| panic!("Oracle rejected the batch:\n{sql}\n{e}"));
    eprintln!("collector batch took {:?}", started.elapsed());
    let mut objects = Vec::new();
    while let Ok(event) = sub.try_recv() {
        if let EventPayload::Rows { rows, .. } = event.payload {
            for row in rows {
                objects.push(SchemaObject {
                    schema: text(row.first()),
                    name: text(row.get(1)),
                    kind: text(row.get(2)),
                    definition: text(row.get(3)),
                });
            }
        }
    }
    objects
}

/// Only the fixture's objects: the login's schema may hold other test
/// fixtures and stray objects.
fn fixture_objects(objects: &[SchemaObject]) -> Vec<SchemaObject> {
    objects
        .iter()
        .filter(|o| o.name.to_ascii_uppercase().contains("SQLLY_SC_"))
        .cloned()
        .collect()
}

fn find<'a>(objects: &'a [SchemaObject], kind: &str, name: &str) -> &'a SchemaObject {
    objects
        .iter()
        .find(|o| o.kind == kind && o.name == name)
        .unwrap_or_else(|| {
            panic!(
                "no {kind} {name} in {:?}",
                objects
                    .iter()
                    .map(|o| format!("{} {}", o.kind, o.name))
                    .collect::<Vec<_>>()
            )
        })
}

#[tokio::test]
async fn collector_batch_describes_every_fixture_object_under_its_kind() {
    let f = live!();
    let client = open_client(&f.profile).await.unwrap();
    let all = run_batch(&client, &f.profile, &oracle_collect_objects_sql()).await;
    assert!(
        all.iter().all(|o| o.schema == f.schema),
        "every object is scoped to the current schema"
    );
    assert!(
        all.iter()
            .all(|o| ORACLE_OBJECT_KINDS.contains(&o.kind.as_str())),
        "{:?}",
        all.iter().map(|o| &o.kind).collect::<Vec<_>>()
    );
    let objects = fixture_objects(&all);

    let table = find(&objects, "TABLE", "SQLLY_SC_ORDERS");
    assert!(
        table
            .definition
            .starts_with("CREATE TABLE \"SQLLY_SC_ORDERS\"")
    );
    assert!(
        table
            .definition
            .contains("\"CUSTOMER_ID\" NUMBER(10,0) NOT NULL ENABLE")
    );
    assert!(
        table
            .definition
            .contains("CONSTRAINT \"SQLLY_SC_ORDERS_AMOUNT_CK\" CHECK")
    );
    // No storage/tablespace noise, no foreign key inline. (A column
    // default such as `"SQLLY"."SQLLY_SC_SEQ"."NEXTVAL"` keeps the owner
    // Oracle stored in the expression; the header itself carries none.)
    for noise in ["TABLESPACE", "PCTFREE", "STORAGE(", "REFERENCES"] {
        assert!(
            !table.definition.contains(noise),
            "{noise} in {}",
            table.definition
        );
    }
    // The MV's container table is not listed as a table.
    assert!(
        !objects
            .iter()
            .any(|o| o.kind == "TABLE" && o.name == "SQLLY_SC_ORDER_TOTALS")
    );
    find(&objects, "MATERIALIZED VIEW", "SQLLY_SC_ORDER_TOTALS");

    let view = find(&objects, "VIEW", "SQLLY_SC_BIG_ORDERS");
    assert!(
        view.definition
            .starts_with("CREATE OR REPLACE FORCE EDITIONABLE VIEW \"SQLLY_SC_BIG_ORDERS\"")
    );
    assert!(sqlly_services::admin::schema_plan::definition_is_runnable(
        view
    ));

    let index = find(
        &objects,
        "INDEX",
        "SQLLY_SC_ORDERS.SQLLY_SC_ORDERS_CUSTOMER_IX",
    );
    assert!(
        index
            .definition
            .starts_with("CREATE INDEX \"SQLLY_SC_ORDERS_CUSTOMER_IX\" ON \"SQLLY_SC_ORDERS\"")
    );
    assert!(index.definition.contains("\"PLACED_AT\" DESC"));
    // Constraint-backing indexes are not duplicated as INDEX objects.
    assert!(
        !objects
            .iter()
            .any(|o| o.kind == "INDEX" && o.name.ends_with("SQLLY_SC_CUSTOMERS_NAME_UQ"))
    );

    // A generated PK name becomes a stable key; user names are kept.
    let pk = find(
        &objects,
        "PRIMARY KEY",
        "SQLLY_SC_ORDERS.(PRIMARY KEY \"ID\")",
    );
    assert_eq!(
        pk.definition,
        "ALTER TABLE \"SQLLY_SC_ORDERS\" ADD PRIMARY KEY (\"ID\")"
    );
    let uq = find(
        &objects,
        "UNIQUE CONSTRAINT",
        "SQLLY_SC_CUSTOMERS.SQLLY_SC_CUSTOMERS_NAME_UQ",
    );
    assert_eq!(
        uq.definition,
        "ALTER TABLE \"SQLLY_SC_CUSTOMERS\" ADD CONSTRAINT \"SQLLY_SC_CUSTOMERS_NAME_UQ\" UNIQUE (\"NAME\")"
    );
    let ck = find(
        &objects,
        "CHECK CONSTRAINT",
        "SQLLY_SC_ORDERS.SQLLY_SC_ORDERS_AMOUNT_CK",
    );
    assert_eq!(
        ck.definition,
        "ALTER TABLE \"SQLLY_SC_ORDERS\" ADD CONSTRAINT \"SQLLY_SC_ORDERS_AMOUNT_CK\" CHECK (amount >= 0)"
    );
    // NOT NULL system constraints are not separate objects.
    assert!(
        !objects
            .iter()
            .any(|o| o.kind == "CHECK CONSTRAINT" && o.definition.contains("IS NOT NULL"))
    );
    let fk = find(
        &objects,
        "FOREIGN KEY",
        "SQLLY_SC_ORDERS.SQLLY_SC_ORDERS_CUSTOMER_FK",
    );
    assert_eq!(
        fk.definition,
        "ALTER TABLE \"SQLLY_SC_ORDERS\" ADD CONSTRAINT \"SQLLY_SC_ORDERS_CUSTOMER_FK\" FOREIGN KEY (\"CUSTOMER_ID\") REFERENCES \"SQLLY_SC_CUSTOMERS\" (\"ID\") ON DELETE CASCADE"
    );

    for (name, head) in [
        (
            "SQLLY_SC_ADD_CUSTOMER",
            "CREATE OR REPLACE EDITIONABLE PROCEDURE",
        ),
        ("SQLLY_SC_TAX", "CREATE OR REPLACE EDITIONABLE FUNCTION"),
        (
            "SQLLY_SC_PKG",
            "CREATE OR REPLACE EDITIONABLE PACKAGE \"SQLLY_SC_PKG\"",
        ),
        (
            "SQLLY_SC_PKG (BODY)",
            "CREATE OR REPLACE EDITIONABLE PACKAGE BODY",
        ),
    ] {
        let routine = find(&objects, "ROUTINE", name);
        assert!(
            routine.definition.starts_with(head),
            "{name}: {}",
            routine.definition
        );
    }
    let ty = find(&objects, "TYPE", "SQLLY_SC_MONEY");
    assert!(
        ty.definition
            .starts_with("CREATE OR REPLACE EDITIONABLE TYPE \"SQLLY_SC_MONEY\"")
    );
    let trigger = find(&objects, "TRIGGER", "SQLLY_SC_ORDERS.SQLLY_SC_ORDERS_BIU");
    assert!(
        trigger
            .definition
            .starts_with("CREATE OR REPLACE EDITIONABLE TRIGGER \"SQLLY_SC_ORDERS_BIU\"")
    );
    let sequence = find(&objects, "SEQUENCE", "SQLLY_SC_SEQ");
    assert!(
        sequence.definition.contains("START WITH 1 "),
        "{}",
        sequence.definition
    );
    assert!(sequence.definition.contains("INCREMENT BY 5"));
    let synonym = find(&objects, "SYNONYM", "SQLLY_SC_CUST");
    assert!(
        synonym
            .definition
            .starts_with("CREATE OR REPLACE EDITIONABLE SYNONYM \"SQLLY_SC_CUST\" FOR")
    );
}

/// Before/after round trip: drop the view and the foreign key, diff the two
/// collections, and run the planned deploy script to bring them back; then
/// widen a table and check the diff reports the table for manual review
/// with both signatures.
#[tokio::test]
async fn diff_and_deploy_script_recreate_dropped_objects() {
    let f = live!();
    let client = open_client(&f.profile).await.unwrap();
    let db = DatabaseType::Oracle;
    let before =
        fixture_objects(&run_batch(&client, &f.profile, &oracle_collect_objects_sql()).await);

    execute_batch_on_client(&client, "DROP VIEW sqlly_sc_big_orders")
        .await
        .unwrap();
    execute_batch_on_client(
        &client,
        "ALTER TABLE sqlly_sc_orders DROP CONSTRAINT sqlly_sc_orders_customer_fk",
    )
    .await
    .unwrap();
    let after =
        fixture_objects(&run_batch(&client, &f.profile, &oracle_collect_objects_sql()).await);

    let diffs = compare_objects(&before, &after, db, db);
    let missing: Vec<(String, String)> = diffs
        .iter()
        .filter(|d| d.status == DifferenceStatus::MissingInTarget)
        .map(|d| (d.object.kind.clone(), d.object.name.clone()))
        .collect();
    assert_eq!(
        missing,
        vec![
            (
                "FOREIGN KEY".to_string(),
                "SQLLY_SC_ORDERS.SQLLY_SC_ORDERS_CUSTOMER_FK".to_string()
            ),
            ("VIEW".to_string(), "SQLLY_SC_BIG_ORDERS".to_string()),
        ]
    );
    assert!(
        diffs
            .iter()
            .all(|d| d.status == DifferenceStatus::MissingInTarget),
        "{diffs:?}"
    );

    let plan = plan_deploy(&diffs, db, db, DeployOptions::default());
    assert!(plan.steps.iter().all(|s| s.runnable));
    let script = deploy_script(&plan, "");
    execute_batch_on_client(&client, &script)
        .await
        .unwrap_or_else(|e| panic!("deploy script failed:\n{script}\n{e}"));
    let restored =
        fixture_objects(&run_batch(&client, &f.profile, &oracle_collect_objects_sql()).await);
    assert!(
        compare_objects(&before, &restored, db, db).is_empty(),
        "{:?}",
        compare_objects(&before, &restored, db, db)
    );

    execute_batch_on_client(
        &client,
        "ALTER TABLE sqlly_sc_orders ADD (note VARCHAR2(20))",
    )
    .await
    .unwrap();
    let widened =
        fixture_objects(&run_batch(&client, &f.profile, &oracle_collect_objects_sql()).await);
    let diffs = compare_objects(&widened, &before, db, db);
    assert_eq!(diffs.len(), 1, "{diffs:?}");
    assert_eq!(diffs[0].status, DifferenceStatus::Changed);
    assert_eq!(diffs[0].object.kind, "TABLE");
    let plan = plan_deploy(&diffs, db, db, DeployOptions::default());
    assert!(!plan.steps[0].runnable);
    assert!(plan.steps[0].sql.contains("MANUAL REVIEW"));
    assert!(plan.steps[0].sql.contains("\"NOTE\" VARCHAR2(20)"));
    execute_batch_on_client(&client, "ALTER TABLE sqlly_sc_orders DROP COLUMN note")
        .await
        .unwrap();
}

/// Data Compare's Oracle sync script runs as generated (no BEGIN, closing
/// COMMIT) and its source SQL reads the fixture with Oracle quoting.
#[tokio::test]
async fn data_compare_source_query_and_sync_script_run() {
    let f = live!();
    let client = open_client(&f.profile).await.unwrap();
    let db = DatabaseType::Oracle;
    let source =
        "SELECT * FROM (SELECT id, name FROM sqlly_sc_customers) sqlly_cmp ORDER BY \"ID\"";
    let source_rows = rows(&client, source).await;
    assert_eq!(source_rows.len(), 1);
    let columns = vec![
        sqlly_core::ResultColumn {
            name: "ID".into(),
            data_type: "NUMBER".into(),
            nullable: false,
        },
        sqlly_core::ResultColumn {
            name: "NAME".into(),
            data_type: "VARCHAR2".into(),
            nullable: false,
        },
    ];
    let diff = DataDiff {
        only_left: vec![vec![CellValue::I64(2), CellValue::String("O'Brien".into())]],
        only_right: Vec::new(),
        changed: vec![ChangedPair {
            key: vec![CellValue::I64(1)],
            left: vec![CellValue::I64(1), CellValue::String("Ada L.".into())],
            right: vec![CellValue::I64(1), CellValue::String("Ada".into())],
            changed_columns: vec![1],
        }],
        unchanged: 0,
        scanned_left: 2,
        scanned_right: 1,
        truncated: None,
    };
    let script = generate_sync_script(&SyncScriptRequest {
        db,
        destination_table: &format!("\"{}\".\"SQLLY_SC_CUSTOMERS\"", f.schema),
        columns: &columns,
        key_indexes: &[0],
        diff: &diff,
        direction: SyncDirection::LeftToRight,
        header_note: "live",
    });
    assert_eq!(script.inserts, 1);
    assert_eq!(script.updates, 1);
    execute_batch_on_client(&client, &script.sql)
        .await
        .unwrap_or_else(|e| panic!("sync script failed:\n{}\n{e}", script.sql));
    let names = rows(&client, "SELECT name FROM sqlly_sc_customers ORDER BY id").await;
    assert_eq!(text(names[0].first()), "Ada L.");
    assert_eq!(text(names[1].first()), "O'Brien");
    execute_batch_on_client(
        &client,
        "DELETE FROM sqlly_sc_customers WHERE id = 2; UPDATE sqlly_sc_customers SET name = 'Ada' WHERE id = 1; COMMIT;",
    )
    .await
    .unwrap();
}
