//! Every SQL generator the service layer points at libSQL / Turso, run
//! against a real `sqld`: the SQLite catalog SQL behind the explorer and
//! IntelliSense (reused for libSQL), the disk-usage and dashboard queries,
//! the server-info probe, the maintenance statements, plan capture +
//! parsing, and the schema-compare collector. A generator whose SQL the
//! libSQL server rejects fails here instead of in the app.
//!
//! Opt-in: set `SQLLY_LIBSQL_URL` (for example `http://127.0.0.1:8080`, the
//! docker container from `docker run -d --name sqlly-libsql -p 8080:8080
//! ghcr.io/tursodatabase/libsql-server:latest`); every test self-skips
//! without it.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType};
use sqlly_engines::libsql::{
    LibSqlClient, execute_batch_on_client, live_profile_from_env, open_client, query_rows_on_client,
};
use sqlly_intel::metadata::MetadataQuery;
use sqlly_intel::metadata::dialect::sql_for_engine;
use sqlly_services::admin::disk_usage::{DiskDialect, DiskUsage};
use sqlly_services::maintenance::{MaintenanceOp, MaintenanceTarget, maintenance_sql};
use sqlly_services::plans::{PlanDialect, estimated_capture_sql, parse_captured_plan};
use sqlly_services::{Dashboard, DashboardDialect};

/// The fixture is rebuilt once per test process (the tests share it
/// read-only), so row counts stay exact across runs. Table names carry a
/// prefix so the shared container's other tables never collide.
static FIXTURE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

const SCHEMA_SQL: &str = "DROP VIEW IF EXISTS sqllyfx_active_customers;
DROP TABLE IF EXISTS sqllyfx_orders;
DROP TABLE IF EXISTS sqllyfx_customers;
CREATE TABLE sqllyfx_customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, mood TEXT DEFAULT 'ok', created TEXT DEFAULT CURRENT_TIMESTAMP, CHECK (name <> ''));
CREATE TABLE sqllyfx_orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES sqllyfx_customers(id), amount REAL, placed_at TEXT);
CREATE INDEX sqllyfx_ix_orders_customer ON sqllyfx_orders (customer_id, placed_at);
CREATE VIEW sqllyfx_active_customers AS SELECT c.id, c.name, COUNT(o.id) AS orders FROM sqllyfx_customers c LEFT JOIN sqllyfx_orders o ON o.customer_id = c.id GROUP BY c.id, c.name;
CREATE TRIGGER sqllyfx_trg_touch AFTER UPDATE ON sqllyfx_customers BEGIN UPDATE sqllyfx_customers SET mood = mood WHERE id = NEW.id; END;
INSERT INTO sqllyfx_customers (id, name, mood) VALUES (1, 'Ada', 'happy'), (2, 'Grace', 'ok');
INSERT INTO sqllyfx_orders VALUES (1, 1, 150.0, '2024-01-02'), (2, 2, 20.5, '2024-01-03'), (3, 1, 999.99, '2024-02-01')";

struct Fixture {
    profile: ConnectionProfile,
    client: LibSqlClient,
}

async fn fixture() -> Option<Fixture> {
    let profile = live_profile_from_env("libsql-dialects")?;
    FIXTURE
        .get_or_init(|| async {
            let bootstrap = open_client(&profile).await.expect("connect");
            execute_batch_on_client(&bootstrap, SCHEMA_SQL)
                .await
                .expect("fixture schema");
        })
        .await;
    let client = open_client(&profile).await.expect("connect");
    Some(Fixture { profile, client })
}

async fn rows(client: &LibSqlClient, sql: &str) -> Vec<Vec<CellValue>> {
    match query_rows_on_client(client, sql).await {
        Ok(set) => set.rows,
        Err(e) => panic!("libSQL rejected generated SQL:\n{sql}\n{e}"),
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

fn names(rows: &[Vec<CellValue>], col: usize) -> Vec<String> {
    rows.iter().map(|r| text(&r[col])).collect()
}

/// Every variant of [`MetadataQuery`] with arguments that name the fixture.
fn every_query() -> Vec<MetadataQuery> {
    let s = || "main".to_string();
    vec![
        MetadataQuery::ServerCapability,
        MetadataQuery::ServerPermissions,
        MetadataQuery::ServerConfiguration,
        MetadataQuery::Databases,
        MetadataQuery::Schemas,
        MetadataQuery::SchemaObjectCounts,
        MetadataQuery::Tables { schema: None },
        MetadataQuery::Tables { schema: Some(s()) },
        MetadataQuery::Views { schema: None },
        MetadataQuery::ProgrammableObjects { schema: None },
        MetadataQuery::ObjectInventory { schema: None },
        MetadataQuery::Columns {
            schema: s(),
            object: "sqllyfx_customers".into(),
        },
        MetadataQuery::AllColumns,
        MetadataQuery::AllForeignKeys,
        MetadataQuery::AllPrimaryKeys,
        MetadataQuery::ProcedureParameters {
            schema: s(),
            object: "nope".into(),
        },
        MetadataQuery::Indexes {
            schema: s(),
            table: "sqllyfx_orders".into(),
        },
        MetadataQuery::Triggers {
            schema: s(),
            table: "sqllyfx_customers".into(),
        },
        MetadataQuery::Statistics {
            schema: s(),
            table: "sqllyfx_orders".into(),
        },
        MetadataQuery::Constraints {
            schema: s(),
            table: "sqllyfx_orders".into(),
        },
        MetadataQuery::Types { schema: None },
        MetadataQuery::DatabaseTriggers { schema: None },
        MetadataQuery::Sequences { schema: None },
        MetadataQuery::Synonyms { schema: None },
        MetadataQuery::Extensions,
        MetadataQuery::AvailableExtensions,
        MetadataQuery::Events { schema: None },
        MetadataQuery::MaterializedViews { schema: None },
        MetadataQuery::TriggerDefinition {
            schema: s(),
            name: "sqllyfx_trg_touch".into(),
        },
        MetadataQuery::ConstraintDefinition {
            schema: s(),
            table: "sqllyfx_orders".into(),
            name: "fk_sqllyfx_orders_0".into(),
        },
        MetadataQuery::TypeDetail {
            schema: s(),
            type_name: "x".into(),
        },
        MetadataQuery::TableGeneral {
            schema: s(),
            table: "sqllyfx_orders".into(),
        },
        MetadataQuery::DatabaseGeneral { database: s() },
        MetadataQuery::DatabaseFiles { database: s() },
        MetadataQuery::DatabasePermissions { database: s() },
        MetadataQuery::ColumnGeneral {
            schema: s(),
            table: "sqllyfx_orders".into(),
            column: "amount".into(),
        },
        MetadataQuery::ObjectGeneral {
            schema: s(),
            object: "sqllyfx_active_customers".into(),
        },
        MetadataQuery::IndexGeneral {
            schema: s(),
            table: "sqllyfx_orders".into(),
            index: "sqllyfx_ix_orders_customer".into(),
        },
        MetadataQuery::IndexColumns {
            schema: s(),
            table: "sqllyfx_orders".into(),
            index: "sqllyfx_ix_orders_customer".into(),
        },
        MetadataQuery::ColumnDefaults {
            schema: s(),
            table: "sqllyfx_customers".into(),
        },
        MetadataQuery::TableIndexColumns {
            schema: s(),
            table: "sqllyfx_orders".into(),
        },
        MetadataQuery::DependentViews {
            schema: s(),
            table: "sqllyfx_orders".into(),
        },
        MetadataQuery::StatisticGeneral {
            schema: s(),
            table: "sqllyfx_orders".into(),
            statistic: "x".into(),
        },
        MetadataQuery::StatisticColumns {
            schema: s(),
            table: "sqllyfx_orders".into(),
            statistic: "x".into(),
        },
        MetadataQuery::ObjectDefinition {
            schema: s(),
            object: "sqllyfx_active_customers".into(),
        },
        MetadataQuery::ObjectPermissions {
            schema: s(),
            object: "sqllyfx_orders".into(),
        },
    ]
}

#[tokio::test]
async fn every_metadata_query_runs_on_libsql() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_LIBSQL_URL not set");
        return;
    };
    // Collect every rejection so one run shows the whole gap, not the first.
    let mut failures = Vec::new();
    for query in every_query() {
        let sql = sql_for_engine(&query, DatabaseType::LibSql).unwrap();
        if let Err(error) = query_rows_on_client(&fx.client, &sql).await {
            failures.push(format!("{query:?}\n{sql}\n{error}"));
        }
    }
    assert!(
        failures.is_empty(),
        "libSQL rejected {} generated statement(s):\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

async fn run(fx: &Fixture, query: MetadataQuery) -> Vec<Vec<CellValue>> {
    let sql = sql_for_engine(&query, DatabaseType::LibSql).unwrap();
    rows(&fx.client, &sql).await
}

#[tokio::test]
async fn catalog_queries_describe_the_fixture() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_LIBSQL_URL not set");
        return;
    };
    let dbs = run(&fx, MetadataQuery::Databases).await;
    assert_eq!(names(&dbs, 0), vec!["main"]);
    let schemas = run(&fx, MetadataQuery::Schemas).await;
    assert_eq!(names(&schemas, 0), vec!["main"]);

    let tables = run(&fx, MetadataQuery::Tables { schema: None }).await;
    let table_names = names(&tables, 1);
    assert!(table_names.contains(&"sqllyfx_customers".to_string()));
    assert!(table_names.contains(&"sqllyfx_orders".to_string()));
    assert!(!table_names.contains(&"sqllyfx_active_customers".to_string()));

    let views = run(&fx, MetadataQuery::Views { schema: None }).await;
    assert!(names(&views, 1).contains(&"sqllyfx_active_customers".to_string()));

    let inventory = run(&fx, MetadataQuery::ObjectInventory { schema: None }).await;
    assert!(
        inventory
            .iter()
            .any(|r| text(&r[0]) == "TRIGGER" && text(&r[2]) == "sqllyfx_trg_touch")
    );

    let columns = run(
        &fx,
        MetadataQuery::Columns {
            schema: "main".into(),
            object: "sqllyfx_customers".into(),
        },
    )
    .await;
    assert_eq!(names(&columns, 0), vec!["id", "name", "mood", "created"]);
    assert_eq!(text(&columns[1][1]), "TEXT");

    let all_columns = run(&fx, MetadataQuery::AllColumns).await;
    assert!(all_columns.iter().any(|r| text(&r[1]) == "sqllyfx_orders"));

    let fks = run(&fx, MetadataQuery::AllForeignKeys).await;
    let order_fk = fks
        .iter()
        .find(|r| text(&r[2]) == "sqllyfx_orders")
        .expect("orders → customers foreign key");
    assert_eq!(text(&order_fk[3]), "customer_id");
    assert_eq!(text(&order_fk[5]), "sqllyfx_customers");
    assert_eq!(text(&order_fk[6]), "id");

    let pks = run(&fx, MetadataQuery::AllPrimaryKeys).await;
    assert!(
        pks.iter()
            .any(|r| text(&r[1]) == "sqllyfx_orders" && text(&r[2]) == "id")
    );

    let indexes = run(
        &fx,
        MetadataQuery::Indexes {
            schema: "main".into(),
            table: "sqllyfx_orders".into(),
        },
    )
    .await;
    assert!(names(&indexes, 0).contains(&"sqllyfx_ix_orders_customer".to_string()));
    let index_columns = run(
        &fx,
        MetadataQuery::IndexColumns {
            schema: "main".into(),
            table: "sqllyfx_orders".into(),
            index: "sqllyfx_ix_orders_customer".into(),
        },
    )
    .await;
    assert_eq!(names(&index_columns, 0), vec!["customer_id", "placed_at"]);

    let triggers = run(
        &fx,
        MetadataQuery::Triggers {
            schema: "main".into(),
            table: "sqllyfx_customers".into(),
        },
    )
    .await;
    assert_eq!(names(&triggers, 0), vec!["sqllyfx_trg_touch"]);
    let trigger = run(
        &fx,
        MetadataQuery::TriggerDefinition {
            schema: "main".into(),
            name: "sqllyfx_trg_touch".into(),
        },
    )
    .await;
    assert!(text(&trigger[0][0]).starts_with("CREATE TRIGGER"));

    let defaults = run(
        &fx,
        MetadataQuery::ColumnDefaults {
            schema: "main".into(),
            table: "sqllyfx_customers".into(),
        },
    )
    .await;
    assert!(defaults.iter().any(|r| text(&r[0]) == "mood"));

    let dependents = run(
        &fx,
        MetadataQuery::DependentViews {
            schema: "main".into(),
            table: "sqllyfx_orders".into(),
        },
    )
    .await;
    assert!(names(&dependents, 1).contains(&"sqllyfx_active_customers".to_string()));

    let general = run(
        &fx,
        MetadataQuery::TableGeneral {
            schema: "main".into(),
            table: "sqllyfx_orders".into(),
        },
    )
    .await;
    assert_eq!(general.len(), 1);

    let definition = run(
        &fx,
        MetadataQuery::ObjectDefinition {
            schema: "main".into(),
            object: "sqllyfx_active_customers".into(),
        },
    )
    .await;
    assert!(text(&definition[0][0]).starts_with("CREATE VIEW"));

    let capability = run(&fx, MetadataQuery::ServerCapability).await;
    assert_eq!(text(&capability[0][1]), "SQLite");
    assert!(text(&capability[0][0]).starts_with('3'));
    let settings = run(&fx, MetadataQuery::ServerConfiguration).await;
    assert!(settings.iter().any(|r| text(&r[0]) == "page_size"));
    let files = run(
        &fx,
        MetadataQuery::DatabaseFiles {
            database: "main".into(),
        },
    )
    .await;
    assert_eq!(text(&files[0][0]), "main");
}

#[tokio::test]
async fn admin_tool_queries_run_and_plans_parse() {
    let Some(fx) = fixture().await else {
        eprintln!("skipping: SQLLY_LIBSQL_URL not set");
        return;
    };
    let disk = rows(
        &fx.client,
        DiskUsage::databases_query_for(DiskDialect::Sqlite),
    )
    .await;
    assert_eq!(disk.len(), 1);
    assert!(text(&disk[0][2]).parse::<i64>().unwrap() > 0, "page_count");
    assert!(DiskUsage::files_query_for(DiskDialect::Sqlite).is_none());
    // `dbstat` ships with libSQL, so per-table sizes are measurable.
    let dbstat = rows(
        &fx.client,
        "SELECT name, SUM(pgsize) FROM dbstat WHERE name = 'sqllyfx_orders' GROUP BY name",
    )
    .await;
    assert_eq!(text(&dbstat[0][0]), "sqllyfx_orders");
    assert!(text(&dbstat[0][1]).parse::<i64>().unwrap() > 0);

    let mut keys = Vec::new();
    for group in Dashboard::metric_groups_for(DashboardDialect::Sqlite) {
        for row in rows(&fx.client, group.sql).await {
            keys.push(text(&row[0]));
            assert!(text(&row[1]).parse::<f64>().is_ok(), "{group:?}: {row:?}");
        }
    }
    for key in [
        "storage_total_mb",
        "reclaimable_mb",
        "objects_total",
        "indexes_total",
    ] {
        assert!(
            keys.contains(&key.to_string()),
            "{key} missing from {keys:?}"
        );
    }

    let probe = sqlly_services::server_info::probe_sql(DatabaseType::LibSql).unwrap();
    let info = rows(&fx.client, probe).await;
    assert_eq!(text(&info[0][0]), "libSQL");
    assert_eq!(text(&info[0][6]), "main");

    // Every maintenance verb the menu offers runs; the ones the server
    // refuses are not offered.
    let db_target = MaintenanceTarget::Database {
        name: "main".into(),
    };
    for op in [MaintenanceOp::Reindex, MaintenanceOp::CheckIntegrity] {
        let sql = maintenance_sql(DatabaseType::LibSql, op, &db_target, &[]).unwrap();
        let _ = rows(&fx.client, sql.trim_end_matches(';')).await;
    }
    let table_reindex = maintenance_sql(
        DatabaseType::LibSql,
        MaintenanceOp::Reindex,
        &MaintenanceTarget::Table {
            schema: Some("main".into()),
            name: "sqllyfx_orders".into(),
        },
        &[],
    )
    .unwrap();
    assert_eq!(table_reindex, "REINDEX \"sqllyfx_orders\";");
    execute_batch_on_client(&fx.client, &table_reindex)
        .await
        .unwrap();
    assert!(maintenance_sql(DatabaseType::LibSql, MaintenanceOp::Vacuum, &db_target, &[]).is_err());
    assert!(
        query_rows_on_client(&fx.client, "VACUUM").await.is_err(),
        "the server refuses VACUUM, which is why the menu omits it"
    );

    // Estimated plans: EXPLAIN QUERY PLAN rows parse into a tree that uses
    // the fixture index.
    let query = "SELECT customer_id, SUM(amount) FROM sqllyfx_orders WHERE customer_id = 1 GROUP BY customer_id";
    let capture = estimated_capture_sql(PlanDialect::Sqlite, query);
    let doc = rows(&fx.client, &capture).await;
    let lines: Vec<String> = doc
        .iter()
        .map(|row| format!("{}|{}|{}", text(&row[0]), text(&row[1]), text(&row[3])))
        .collect();
    let plans = parse_captured_plan(PlanDialect::Sqlite, &lines.join("\n")).unwrap();
    assert!(!plans.is_empty());
    let all_ops: Vec<String> = plans[0]
        .nodes
        .iter()
        .map(|n| format!("{} {}", n.logical_op, n.physical_op))
        .collect();
    assert!(
        all_ops
            .iter()
            .any(|op| op.contains("sqllyfx_ix_orders_customer")),
        "{all_ops:?}"
    );

    // The schema-compare collector's SQLite script runs unchanged.
    let collector = "SELECT '', name, UPPER(type), COALESCE(sql, '') FROM sqlite_master WHERE type IN ('table','view','index','trigger') AND name NOT LIKE 'sqlite_%' ORDER BY type, name";
    let objects = rows(&fx.client, collector).await;
    assert!(objects.iter().any(|r| text(&r[1]) == "sqllyfx_orders"));
    let _ = fx.profile;
}

// ───────────────────────── row editing and Data Transfer ─────────────────────────

mod editing {
    //! The row-editing lane and the transfer sink against the real server:
    //! provenance through the catalog, guarded writes on one stream, the
    //! read-only gates, atomic multi-statement saves, and a SQLite → libSQL
    //! transfer. Every test builds its own uniquely named tables so the
    //! shared container never sees a collision, and drops them at the end.

    use sqlly_core::{CellValue, ConnectionProfile, SqlParam};
    use sqlly_engines::libsql::{
        LibSqlClient, LibSqlStream, execute_batch_on_client, live_profile_from_env, open_client,
        query_rows_on_client,
    };
    use sqlly_services::row_editing::{
        BatchOutcome, BatchRequest, DefaultRowEditingService, DeleteRowOutcome, DeleteRowRequest,
        FkSearchRequest, InsertRowOutcome, InsertRowRequest, RowEditOpenOutcome,
        RowEditOpenRequest, RowEditSession, RowEditingService, StagedBatch, StagedOp, TableTarget,
        UpdateRowOutcome, UpdateRowRequest,
    };

    struct Lab {
        profile: ConnectionProfile,
        client: LibSqlClient,
        /// Unique suffix for this test's tables.
        tag: String,
        tables: Vec<String>,
    }

    impl Lab {
        async fn open() -> Option<Lab> {
            let profile = live_profile_from_env("libsql-editing")?;
            let client = open_client(&profile).await.expect("connect");
            Some(Lab {
                profile,
                client,
                tag: uuid::Uuid::new_v4().simple().to_string()[..8].to_string(),
                tables: Vec::new(),
            })
        }

        /// `name` + this test's tag, remembered for cleanup.
        fn table(&mut self, name: &str) -> String {
            let full = format!("sqllyed_{name}_{}", self.tag);
            if !self.tables.contains(&full) {
                self.tables.push(full.clone());
            }
            full
        }

        async fn run(&self, sql: &str) {
            execute_batch_on_client(&self.client, sql)
                .await
                .unwrap_or_else(|e| panic!("{sql}\n{e}"));
        }

        async fn scalar(&self, sql: &str) -> CellValue {
            query_rows_on_client(&self.client, sql)
                .await
                .unwrap_or_else(|e| panic!("{sql}\n{e}"))
                .rows
                .first()
                .and_then(|r| r.first())
                .cloned()
                .unwrap_or(CellValue::Null)
        }

        async fn names(&self, table: &str) -> CellValue {
            self.scalar(&format!(
                "SELECT group_concat(name, ',') FROM (SELECT name FROM {table} ORDER BY id)"
            ))
            .await
        }

        /// Builds the open request exactly as the grid would: the query's
        /// real projected columns plus one row's values.
        async fn grid_request(&self, sql: &str, row: usize) -> RowEditOpenRequest {
            let set = query_rows_on_client(&self.client, sql)
                .await
                .unwrap_or_else(|e| panic!("{sql}\n{e}"));
            RowEditOpenRequest {
                sql: sql.to_string(),
                result_set_index: 0,
                columns: set.columns,
                row: set.rows[row].clone(),
            }
        }

        async fn cleanup(self) {
            for table in self.tables.iter().rev() {
                let _ = execute_batch_on_client(
                    &self.client,
                    &format!("DROP VIEW IF EXISTS {table}; DROP TABLE IF EXISTS {table}"),
                )
                .await;
            }
        }
    }

    fn editable(outcome: RowEditOpenOutcome) -> RowEditSession {
        match outcome {
            RowEditOpenOutcome::Editable(session) => *session,
            RowEditOpenOutcome::NotEditable { reason, .. } => {
                panic!("expected editable session, got: {reason}")
            }
        }
    }

    fn not_editable_reason(outcome: RowEditOpenOutcome) -> String {
        match outcome {
            RowEditOpenOutcome::NotEditable { reason, .. } => reason,
            RowEditOpenOutcome::Editable(_) => panic!("expected a not-editable outcome"),
        }
    }

    fn target(table: &str) -> TableTarget {
        TableTarget {
            schema: "main".into(),
            table: table.into(),
        }
    }

    /// The driver's stream keeps a transaction open across HTTP requests
    /// and `ROLLBACK` discards it; a conditional batch on the stream stops
    /// at its first failing step.
    #[tokio::test]
    async fn stream_holds_a_transaction_across_requests() {
        let Some(mut lab) = Lab::open().await else {
            eprintln!("skipping: SQLLY_LIBSQL_URL not set");
            return;
        };
        let t = lab.table("stream");
        lab.run(&format!(
            "CREATE TABLE {t} (id INTEGER PRIMARY KEY, v TEXT)"
        ))
        .await;

        let mut stream = LibSqlStream::open(lab.client.clone());
        stream.execute("BEGIN IMMEDIATE", &[]).await.unwrap();
        let inserted = stream
            .execute(
                &format!("INSERT INTO {t} (v) VALUES (?) RETURNING id"),
                &[SqlParam::String("inside".into())],
            )
            .await
            .unwrap();
        assert_eq!(inserted.affected_rows, 1);
        assert_eq!(inserted.rows[0][0], CellValue::I64(1));
        // A second request on the same stream sees the uncommitted row …
        let seen = stream
            .execute(&format!("SELECT count(*) FROM {t}"), &[])
            .await
            .unwrap();
        assert_eq!(seen.rows[0][0], CellValue::I64(1));
        // … while a fresh stream (another connection) does not.
        assert_eq!(
            lab.scalar(&format!("SELECT count(*) FROM {t}")).await,
            CellValue::I64(0)
        );
        stream.execute_closing("ROLLBACK", &[]).await.unwrap();
        assert!(!stream.is_open());
        assert_eq!(
            lab.scalar(&format!("SELECT count(*) FROM {t}")).await,
            CellValue::I64(0)
        );

        let mut stream = LibSqlStream::open(lab.client.clone());
        stream.execute("BEGIN", &[]).await.unwrap();
        let outcome = stream
            .batch(&[
                (
                    format!("INSERT INTO {t} (v) VALUES (?)"),
                    vec![SqlParam::String("a".into())],
                ),
                (
                    format!("INSERT INTO {t} (nope) VALUES (?)"),
                    vec![SqlParam::String("b".into())],
                ),
                (
                    format!("INSERT INTO {t} (v) VALUES (?)"),
                    vec![SqlParam::String("c".into())],
                ),
            ])
            .await
            .unwrap();
        assert_eq!(outcome.outcomes.len(), 1, "only the first step ran");
        assert!(
            outcome.error.as_ref().unwrap().message.contains("nope"),
            "{outcome:?}"
        );
        stream.execute_closing("ROLLBACK", &[]).await.unwrap();
        assert_eq!(
            lab.scalar(&format!("SELECT count(*) FROM {t}")).await,
            CellValue::I64(0)
        );
        lab.cleanup().await;
    }

    #[tokio::test]
    async fn edit_lifecycle_on_a_table_with_a_primary_key() {
        let Some(mut lab) = Lab::open().await else {
            eprintln!("skipping: SQLLY_LIBSQL_URL not set");
            return;
        };
        let customer = lab.table("customer");
        let person = lab.table("person");
        lab.run(&format!(
            "CREATE TABLE {customer} (id INTEGER PRIMARY KEY, name TEXT);\
             CREATE TABLE {person} (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INT, \
             note VARCHAR(20), amount DECIMAL(10,2), customer_id INTEGER REFERENCES {customer}, \
             status TEXT DEFAULT 'new' CHECK (status IN ('new', 'paid')));\
             INSERT INTO {customer} (name) VALUES ('Acme'), ('Beta');\
             INSERT INTO {person} (name, age, customer_id) VALUES ('Ada', 36, 1), ('Grace', 45, 2);"
        ))
        .await;
        let service = DefaultRowEditingService::default();

        // Open: the aliased, differently cased projection traces back to the
        // catalog's own columns; key, identity, FK, CHECK enum and types
        // come from the pragmas and the CREATE TABLE text.
        let request = lab
            .grid_request(
                &format!(
                    "SELECT p.ID, p.name AS who, AGE, note, amount, customer_id, status \
                     FROM main.{person} p ORDER BY p.id"
                ),
                0,
            )
            .await;
        let session = editable(service.open_session(&lab.profile, request).await.unwrap());
        assert_eq!(session.target.schema, "main");
        assert_eq!(session.target.table, person);
        assert_eq!(session.key_columns, vec!["id"]);
        let by_name = |name: &str| session.columns.iter().find(|c| c.name == name).expect(name);
        assert!(
            by_name("id").is_primary_key && by_name("id").is_identity,
            "rowid alias"
        );
        assert_eq!(by_name("id").result_ordinal, Some(0));
        assert_eq!(by_name("name").result_ordinal, Some(1));
        assert_eq!(by_name("age").result_ordinal, Some(2));
        assert!(!by_name("name").nullable);
        assert_eq!(by_name("note").char_max_length, Some(20));
        assert_eq!(by_name("amount").precision, Some(10));
        assert_eq!(by_name("amount").scale, Some(2));
        assert_eq!(
            by_name("status").enum_values,
            vec!["new".to_string(), "paid".to_string()]
        );
        assert_eq!(session.check_constraints.len(), 1);
        assert_eq!(session.original_row[1], CellValue::String("Ada".into()));
        assert_eq!(session.foreign_keys.len(), 1);
        assert_eq!(session.foreign_keys[0].referenced.table, customer);
        assert_eq!(
            session.foreign_keys[0].column_pairs,
            vec![("customer_id".to_string(), "id".to_string())]
        );
        assert_eq!(
            session.foreign_keys[0].display_column.as_deref(),
            Some("name")
        );
        assert_eq!(session.fk_display[0].as_deref(), Some("Acme"));

        // Update: guarded, parameterized, returns the refetched row.
        let update = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("name".into(), CellValue::String("Bob".into()))],
            original_values: vec![("name".into(), CellValue::String("Ada".into()))],
            expressions: Vec::new(),
        };
        match service.update_row(&lab.profile, update).await.unwrap() {
            UpdateRowOutcome::Updated { columns, row } => {
                let at = columns.iter().position(|c| c == "name").unwrap();
                assert_eq!(row[at], CellValue::String("Bob".into()));
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        assert_eq!(
            lab.scalar(&format!("SELECT name FROM {person} WHERE id = 1"))
                .await,
            CellValue::String("Bob".into()),
            "committed"
        );

        // Conflict (row changed): stale originals match nothing; the current
        // row comes back so the caller can show what changed.
        let stale = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("name".into(), CellValue::String("Carol".into()))],
            original_values: vec![("name".into(), CellValue::String("Ada".into()))],
            expressions: Vec::new(),
        };
        match service.update_row(&lab.profile, stale).await.unwrap() {
            UpdateRowOutcome::Conflict {
                current_columns,
                current_row,
            } => {
                let at = current_columns.iter().position(|c| c == "name").unwrap();
                assert_eq!(
                    current_row.expect("row still exists")[at],
                    CellValue::String("Bob".into())
                );
            }
            other => panic!("expected Conflict, got {other:?}"),
        }

        // Conflict (row gone): out-of-band delete, then a guarded update.
        lab.run(&format!("DELETE FROM {person} WHERE id = 2")).await;
        let gone = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(2))],
            rowversion: None,
            changes: vec![("name".into(), CellValue::String("X".into()))],
            original_values: vec![("name".into(), CellValue::String("Grace".into()))],
            expressions: Vec::new(),
        };
        match service.update_row(&lab.profile, gone).await.unwrap() {
            UpdateRowOutcome::Conflict { current_row, .. } => assert!(current_row.is_none()),
            other => panic!("expected Conflict for a deleted row, got {other:?}"),
        }

        // A CHECK violation surfaces as the server's error; nothing is written.
        let bad = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(1))],
            rowversion: None,
            changes: vec![("status".into(), CellValue::String("bogus".into()))],
            original_values: vec![("status".into(), CellValue::String("new".into()))],
            expressions: Vec::new(),
        };
        let err = service.update_row(&lab.profile, bad).await.unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");

        // Insert: RETURNING resolves the engine-assigned rowid alias and
        // the defaults.
        let insert = InsertRowRequest {
            target: session.target.clone(),
            values: vec![
                ("name".into(), CellValue::String("Zed".into())),
                ("age".into(), CellValue::I64(30)),
            ],
            expressions: Vec::new(),
        };
        let inserted_id = match service.insert_row(&lab.profile, insert).await.unwrap() {
            InsertRowOutcome::Inserted { columns, row } => {
                let id_at = columns.iter().position(|c| c == "id").unwrap();
                let note_at = columns.iter().position(|c| c == "note").unwrap();
                let status_at = columns.iter().position(|c| c == "status").unwrap();
                assert_eq!(row[note_at], CellValue::Null);
                assert_eq!(row[status_at], CellValue::String("new".into()));
                match row[id_at] {
                    // Row 2 was deleted above, and without AUTOINCREMENT
                    // SQLite hands its rowid out again.
                    CellValue::I64(id) => {
                        assert!(id > 1, "identity resolved");
                        id
                    }
                    ref other => panic!("expected integer id, got {other:?}"),
                }
            }
        };

        // Delete: guarded by key + original values; a second delete reports
        // the row as gone rather than silently succeeding.
        let delete = DeleteRowRequest {
            target: session.target.clone(),
            key: vec![("id".into(), CellValue::I64(inserted_id))],
            rowversion: None,
            original_values: vec![
                ("name".into(), CellValue::String("Zed".into())),
                ("age".into(), CellValue::I64(30)),
                ("note".into(), CellValue::Null),
            ],
        };
        assert!(matches!(
            service
                .delete_row(&lab.profile, delete.clone())
                .await
                .unwrap(),
            DeleteRowOutcome::Deleted
        ));
        match service.delete_row(&lab.profile, delete).await.unwrap() {
            DeleteRowOutcome::Conflict { current_row, .. } => assert!(current_row.is_none()),
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert!(
            service
                .fetch_row(
                    &lab.profile,
                    &session.target,
                    &[("id".into(), CellValue::I64(inserted_id))]
                )
                .await
                .unwrap()
                .is_none()
        );

        // FK autocomplete, and the customer side's incoming FK.
        let search = service
            .fk_search(
                &lab.profile,
                FkSearchRequest {
                    referenced: target(&customer),
                    key_columns: vec!["id".into()],
                    display_column: Some("name".into()),
                    query: "ac".into(),
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(search.candidates.len(), 1);
        assert_eq!(search.candidates[0].label, "Acme");
        let request = lab
            .grid_request(&format!("SELECT * FROM {customer} ORDER BY id"), 0)
            .await;
        let session = editable(service.open_session(&lab.profile, request).await.unwrap());
        assert_eq!(session.incoming_foreign_keys.len(), 1);
        assert_eq!(session.incoming_foreign_keys[0].child.table, person);
        lab.cleanup().await;
    }

    /// A rowid table with no declared key edits through its hidden rowid,
    /// which the SELECT must project.
    #[tokio::test]
    async fn edit_lifecycle_on_a_rowid_only_table() {
        let Some(mut lab) = Lab::open().await else {
            eprintln!("skipping: SQLLY_LIBSQL_URL not set");
            return;
        };
        let t = lab.table("norowid");
        lab.run(&format!(
            "CREATE TABLE {t} (v TEXT, n INT); INSERT INTO {t} VALUES ('a', 1), ('b', 2)"
        ))
        .await;
        let service = DefaultRowEditingService::default();

        // Without the rowid in the projection the row cannot be identified.
        let request = lab.grid_request(&format!("SELECT v, n FROM {t}"), 0).await;
        let reason =
            not_editable_reason(service.open_session(&lab.profile, request).await.unwrap());
        assert!(
            reason.contains("\"rowid\"") && reason.contains("include it"),
            "{reason}"
        );

        let request = lab
            .grid_request(&format!("SELECT rowid, * FROM {t} ORDER BY rowid"), 1)
            .await;
        let session = editable(service.open_session(&lab.profile, request).await.unwrap());
        assert_eq!(session.key_columns, vec!["rowid"]);
        let rowid = &session.columns[0];
        assert!(rowid.is_primary_key && rowid.is_identity && !rowid.is_editable());
        assert_eq!(rowid.result_ordinal, Some(0));
        assert_eq!(
            session.original_row,
            vec![
                CellValue::I64(2),
                CellValue::String("b".into()),
                CellValue::I64(2)
            ]
        );

        let update = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("rowid".into(), CellValue::I64(2))],
            rowversion: None,
            changes: vec![("v".into(), CellValue::String("B".into()))],
            original_values: vec![("v".into(), CellValue::String("b".into()))],
            expressions: Vec::new(),
        };
        match service.update_row(&lab.profile, update).await.unwrap() {
            UpdateRowOutcome::Updated { columns, row } => {
                assert_eq!(columns[0], "rowid");
                assert_eq!(row[0], CellValue::I64(2));
                assert_eq!(row[1], CellValue::String("B".into()));
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        let stale = UpdateRowRequest {
            target: session.target.clone(),
            key: vec![("rowid".into(), CellValue::I64(2))],
            rowversion: None,
            changes: vec![("v".into(), CellValue::String("C".into()))],
            original_values: vec![("v".into(), CellValue::String("b".into()))],
            expressions: Vec::new(),
        };
        assert!(matches!(
            service.update_row(&lab.profile, stale).await.unwrap(),
            UpdateRowOutcome::Conflict {
                current_row: Some(_),
                ..
            }
        ));

        let insert = InsertRowRequest {
            target: session.target.clone(),
            values: vec![("v".into(), CellValue::String("c".into()))],
            expressions: Vec::new(),
        };
        let new_rowid = match service.insert_row(&lab.profile, insert).await.unwrap() {
            InsertRowOutcome::Inserted { columns, row } => {
                assert_eq!(columns[0], "rowid", "the new row carries its rowid");
                match row[0] {
                    CellValue::I64(id) => id,
                    ref other => panic!("{other:?}"),
                }
            }
        };
        assert_eq!(new_rowid, 3);
        let delete = DeleteRowRequest {
            target: session.target.clone(),
            key: vec![("rowid".into(), CellValue::I64(new_rowid))],
            rowversion: None,
            original_values: vec![("v".into(), CellValue::String("c".into()))],
        };
        assert!(matches!(
            service.delete_row(&lab.profile, delete).await.unwrap(),
            DeleteRowOutcome::Deleted
        ));
        assert_eq!(
            lab.scalar(&format!("SELECT count(*) FROM {t}")).await,
            CellValue::I64(2)
        );
        lab.cleanup().await;
    }

    #[tokio::test]
    async fn open_session_read_only_gates() {
        let Some(mut lab) = Lab::open().await else {
            eprintln!("skipping: SQLLY_LIBSQL_URL not set");
            return;
        };
        let a = lab.table("a");
        let b = lab.table("b");
        let va = lab.table("va");
        let wr = lab.table("wr");
        lab.run(&format!(
            "CREATE TABLE {a} (id INTEGER PRIMARY KEY, name TEXT);\
             CREATE TABLE {b} (id INTEGER PRIMARY KEY, a_id INT);\
             CREATE TABLE {wr} (code TEXT PRIMARY KEY, v INT) WITHOUT ROWID;\
             INSERT INTO {a} (name) VALUES ('x'); INSERT INTO {b} (a_id) VALUES (1);\
             INSERT INTO {wr} VALUES ('k', 1);\
             CREATE VIEW {va} AS SELECT id, name FROM {a};"
        ))
        .await;
        let service = DefaultRowEditingService::default();

        let join = lab
            .grid_request(
                &format!("SELECT {a}.id, {b}.id AS bid FROM {a} JOIN {b} ON {b}.a_id = {a}.id"),
                0,
            )
            .await;
        let reason = not_editable_reason(service.open_session(&lab.profile, join).await.unwrap());
        assert!(reason.contains("read-only"), "{reason}");

        let view = lab
            .grid_request(&format!("SELECT id, name FROM {va}"), 0)
            .await;
        let reason = not_editable_reason(service.open_session(&lab.profile, view).await.unwrap());
        assert!(reason.contains("views are read-only"), "{reason}");

        let expression = lab.grid_request("SELECT 1 + 1 AS calc", 0).await;
        let reason = not_editable_reason(
            service
                .open_session(&lab.profile, expression)
                .await
                .unwrap(),
        );
        assert!(reason.contains("read-only"), "{reason}");

        let missing_key = lab.grid_request(&format!("SELECT name FROM {a}"), 0).await;
        let reason = not_editable_reason(
            service
                .open_session(&lab.profile, missing_key)
                .await
                .unwrap(),
        );
        assert!(reason.contains("key column"), "{reason}");

        // A WITHOUT ROWID table has no hidden rowid but does have its key.
        let request = lab.grid_request(&format!("SELECT * FROM {wr}"), 0).await;
        let session = editable(service.open_session(&lab.profile, request).await.unwrap());
        assert_eq!(session.key_columns, vec!["code"]);
        assert!(!session.columns[0].is_identity);

        // Two statements are refused before anything reaches the server.
        let script = RowEditOpenRequest {
            sql: format!("SELECT id, name FROM {a}; DELETE FROM {a}"),
            result_set_index: 0,
            columns: lab
                .grid_request(&format!("SELECT id, name FROM {a}"), 0)
                .await
                .columns,
            row: vec![CellValue::I64(1), CellValue::String("x".into())],
        };
        let reason = not_editable_reason(service.open_session(&lab.profile, script).await.unwrap());
        assert!(reason.contains("several statements"), "{reason}");
        assert_eq!(
            lab.scalar(&format!("SELECT count(*) FROM {a}")).await,
            CellValue::I64(1),
            "DELETE never ran"
        );

        // A select-only connection refuses to open an editor at all.
        let mut locked = lab.profile.clone();
        locked.block_all_but_select = true;
        let request = lab.grid_request(&format!("SELECT * FROM {a}"), 0).await;
        let reason = not_editable_reason(service.open_session(&locked, request).await.unwrap());
        assert!(reason.contains("SELECT statements only"), "{reason}");
        lab.cleanup().await;
    }

    /// A staged multi-row save is one transaction: a failing statement or a
    /// stale row rolls every earlier statement back.
    #[tokio::test]
    async fn staged_batch_is_atomic() {
        let Some(mut lab) = Lab::open().await else {
            eprintln!("skipping: SQLLY_LIBSQL_URL not set");
            return;
        };
        let t = lab.table("batch");
        lab.run(&format!(
            "CREATE TABLE {t} (id INTEGER PRIMARY KEY, name TEXT NOT NULL, n INT);\
             INSERT INTO {t} (name, n) VALUES ('a', 1), ('b', 2), ('c', 3)"
        ))
        .await;
        let service = DefaultRowEditingService::default();
        let tt = target(&t);
        let update = |id: i64, from: &str, to: &str| UpdateRowRequest {
            target: tt.clone(),
            key: vec![("id".into(), CellValue::I64(id))],
            rowversion: None,
            changes: vec![("name".into(), CellValue::String(to.into()))],
            original_values: vec![("name".into(), CellValue::String(from.into()))],
            expressions: Vec::new(),
        };
        let batch = |ops: Vec<StagedOp>| BatchRequest {
            batch: StagedBatch {
                target: Some(tt.clone()),
                key_columns: vec!["id".into()],
                ops,
            },
        };

        // Statement 2 violates NOT NULL: the server stops the batch there
        // and statement 1 is rolled back.
        let failing = batch(vec![
            StagedOp::Update(update(1, "a", "A")),
            StagedOp::Update(UpdateRowRequest {
                target: tt.clone(),
                key: vec![("id".into(), CellValue::I64(2))],
                rowversion: None,
                changes: vec![("name".into(), CellValue::Null)],
                original_values: vec![("name".into(), CellValue::String("b".into()))],
                expressions: Vec::new(),
            }),
            StagedOp::Update(update(3, "c", "C")),
        ]);
        let err = service
            .apply_batch(&lab.profile, failing)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("NOT NULL"), "{err}");
        assert_eq!(
            lab.names(&t).await,
            CellValue::String("a,b,c".into()),
            "nothing from the failed batch persisted"
        );

        // A stale row (statement 2's original value no longer matches) is a
        // conflict at its own index, and statements 1 and 3 roll back.
        let stale = batch(vec![
            StagedOp::Update(update(1, "a", "A")),
            StagedOp::Update(update(2, "stale", "B")),
            StagedOp::Update(update(3, "c", "C")),
        ]);
        match service.apply_batch(&lab.profile, stale).await.unwrap() {
            BatchOutcome::Conflict {
                statement_index,
                affected,
            } => {
                assert_eq!(statement_index, 1);
                assert_eq!(affected, 0);
            }
            other => panic!("expected Conflict, got {other:?}"),
        }
        assert_eq!(lab.names(&t).await, CellValue::String("a,b,c".into()));

        // The whole edit set — a delete, updates and an insert — commits
        // together.
        let good = batch(vec![
            StagedOp::Update(update(1, "a", "A")),
            StagedOp::Delete {
                request: DeleteRowRequest {
                    target: tt.clone(),
                    key: vec![("id".into(), CellValue::I64(2))],
                    rowversion: None,
                    original_values: vec![("name".into(), CellValue::String("b".into()))],
                },
                original_row: vec![
                    ("id".into(), CellValue::I64(2)),
                    ("name".into(), CellValue::String("b".into())),
                    ("n".into(), CellValue::I64(2)),
                ],
            },
            StagedOp::Insert(InsertRowRequest {
                target: tt.clone(),
                values: vec![("name".into(), CellValue::String("d".into()))],
                expressions: Vec::new(),
            }),
            StagedOp::Update(update(3, "c", "C")),
        ]);
        assert_eq!(
            service.apply_batch(&lab.profile, good).await.unwrap(),
            BatchOutcome::Committed { statements: 4 }
        );
        assert_eq!(lab.names(&t).await, CellValue::String("A,C,d".into()));
        lab.cleanup().await;
    }

    /// SQLite file → libSQL through the real transfer runner: the sink
    /// creates the table with the SQLite type map, streams the rows as
    /// multi-row inserts inside one server transaction, and they read back
    /// intact.
    #[tokio::test]
    async fn transfer_from_sqlite_into_libsql() {
        use sqlly_core::{DatabaseType, SecretProvider, SecretRef};
        use sqlly_services::transfer::plan::{
            NameCasing, TransferColumn, TransferMode, TransferPlan, TransferTable,
        };
        use sqlly_services::transfer::run::{TransferCancel, TransferProgress, run_transfer};
        use sqlly_services::transfer::sink::{TransferSink, open_sink};

        let Some(mut lab) = Lab::open().await else {
            eprintln!("skipping: SQLLY_LIBSQL_URL not set");
            return;
        };
        let items = lab.table("items");
        let bulk = lab.table("bulk");
        let dir = tempfile::tempdir().unwrap();
        let mut source = ConnectionProfile::sql_login(
            "transfer-source",
            dir.path().join("source.db").to_string_lossy().as_ref(),
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        source.database_type = DatabaseType::Sqlite;
        let sqlite = sqlly_engines::sqlite::open_client(&source).await.unwrap();
        for statement in [
            format!(
                "CREATE TABLE {items} (id INTEGER PRIMARY KEY, name TEXT NOT NULL, price REAL, \
                 note TEXT, data BLOB, active INTEGER)"
            ),
            format!(
                "INSERT INTO {items} VALUES (1, 'apple', 1.25, NULL, X'01ff', 1), \
                 (2, 'pear', 2.5, 'ripe', NULL, 0), (3, 'o''brien', NULL, 'quote', X'', 1)"
            ),
            // Enough rows to need several INSERT statements on the sink.
            format!("CREATE TABLE {bulk} (id INTEGER PRIMARY KEY, v TEXT)"),
            format!(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 2500) \
                 INSERT INTO {bulk} SELECT x, 'v' || x FROM c"
            ),
        ] {
            sqlly_engines::sqlite::execute_params_on_client(&sqlite, &statement, &[])
                .await
                .unwrap();
        }
        drop(sqlite);

        let column = |name: &str, ty: &str, nullable: bool, pk: Option<i64>| TransferColumn {
            name: name.into(),
            source_type: ty.into(),
            nullable,
            is_identity: false,
            pk_ordinal: pk,
        };
        let plan = TransferPlan {
            source: DatabaseType::Sqlite,
            target: DatabaseType::LibSql,
            target_schema: Some("main".into()),
            mode: TransferMode::CreateTable,
            casing: NameCasing::AsIs,
            tables: vec![
                TransferTable {
                    schema: None,
                    name: items.clone(),
                    columns: vec![
                        column("id", "INTEGER", false, Some(1)),
                        column("name", "TEXT", false, None),
                        column("price", "REAL", true, None),
                        column("note", "TEXT", true, None),
                        column("data", "BLOB", true, None),
                        column("active", "INTEGER", true, None),
                    ],
                    depends_on: vec![],
                },
                TransferTable {
                    schema: None,
                    name: bulk.clone(),
                    columns: vec![
                        column("id", "INTEGER", false, Some(1)),
                        column("v", "TEXT", true, None),
                    ],
                    depends_on: vec![],
                },
            ],
        };
        let services = sqlly_services::services::Services::new();
        let mut sink = open_sink(&lab.profile).await.unwrap();
        assert_eq!(sink.database_type(), DatabaseType::LibSql);
        let (progress, mut events) = tokio::sync::mpsc::channel(256);
        let outcome = run_transfer(
            &services,
            &source,
            &mut sink,
            &plan,
            progress,
            TransferCancel::default(),
        )
        .await;
        assert_eq!(outcome.error, None, "{outcome:?}");
        assert!(!outcome.cancelled);
        let mut copied = outcome.tables.clone();
        copied.sort();
        assert_eq!(copied, vec![(bulk.clone(), 2500), (items.clone(), 3)]);
        let mut finished = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, TransferProgress::Finished(_)) {
                finished = true;
            }
        }
        assert!(finished);
        drop(sink);

        let rows = query_rows_on_client(
            &lab.client,
            &format!("SELECT id, name, price, note, data, active FROM {items} ORDER BY id"),
        )
        .await
        .unwrap()
        .rows;
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][1], CellValue::String("apple".into()));
        assert_eq!(rows[0][2], CellValue::F64(1.25));
        assert_eq!(rows[0][3], CellValue::Null);
        assert_eq!(rows[0][4], CellValue::Bytes(vec![0x01, 0xff]));
        assert_eq!(rows[1][3], CellValue::String("ripe".into()));
        assert_eq!(rows[1][4], CellValue::Null);
        assert_eq!(rows[2][1], CellValue::String("o'brien".into()));
        assert_eq!(rows[2][2], CellValue::Null);
        assert_eq!(rows[2][4], CellValue::Bytes(Vec::new()));
        assert_eq!(rows[2][5], CellValue::I64(1));
        assert_eq!(
            lab.scalar(&format!("SELECT count(*) || ':' || sum(id) FROM {bulk}"))
                .await,
            CellValue::String(format!("2500:{}", 2500 * 2501 / 2))
        );
        // The created table carries the primary key the plan declared.
        assert_eq!(
            lab.scalar(&format!(
                "SELECT count(*) FROM pragma_table_info('{items}') WHERE pk > 0"
            ))
            .await,
            CellValue::I64(1)
        );

        // A sink transaction rolls back as a unit when a statement fails.
        let mut sink = open_sink(&lab.profile).await.unwrap();
        sink.begin().await.unwrap();
        assert_eq!(
            sink.execute(
                &format!("INSERT INTO {bulk} (id, v) VALUES (?, ?), (?, ?)"),
                &[
                    SqlParam::I64(9001),
                    SqlParam::String("x".into()),
                    SqlParam::I64(9002),
                    SqlParam::String("y".into()),
                ],
            )
            .await
            .unwrap(),
            2
        );
        assert!(
            sink.execute(
                &format!("INSERT INTO {bulk} (id, v) VALUES (?, ?)"),
                &[SqlParam::I64(1), SqlParam::String("dup".into())]
            )
            .await
            .is_err(),
            "duplicate key"
        );
        sink.rollback().await.unwrap();
        drop(sink);
        assert_eq!(
            lab.scalar(&format!("SELECT count(*) FROM {bulk}")).await,
            CellValue::I64(2500)
        );
        lab.cleanup().await;
    }
}
