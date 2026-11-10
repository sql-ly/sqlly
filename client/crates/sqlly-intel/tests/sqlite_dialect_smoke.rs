//! Hermetic proof that every SQLite arm of the dialect-dispatched metadata
//! SQL is genuinely valid SQLite: each variant is prepared and executed
//! against a real in-memory database with a representative schema. This
//! catches syntax errors, misspelled pragma table-functions, and wrong
//! column references that a text-only test cannot — the same class of bug a
//! live conformance run would catch, at zero infrastructure cost.

use rusqlite::Connection;
use sqlly_intel::metadata::MetadataQuery;
use sqlly_intel::metadata::dialect::{MetadataDialect, sql_for_dialect};

fn seeded_db() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, org_id INT,
             FOREIGN KEY (org_id) REFERENCES orgs(id));
         CREATE TABLE orgs (id INTEGER PRIMARY KEY, title TEXT);
         CREATE INDEX idx_users_name ON users (name);
         CREATE VIEW active_users AS SELECT id, name FROM users;
         CREATE TRIGGER trg_users_touch AFTER UPDATE ON users BEGIN
             UPDATE users SET name = name WHERE id = NEW.id;
         END;
         INSERT INTO users (name, org_id) VALUES ('a', NULL), ('b', NULL);",
    )
    .unwrap();
    conn
}

fn every_sqlite_variant() -> Vec<MetadataQuery> {
    vec![
        MetadataQuery::Schemas,
        MetadataQuery::SchemaObjectCounts,
        MetadataQuery::Tables { schema: None },
        MetadataQuery::Views { schema: None },
        MetadataQuery::ProgrammableObjects { schema: None },
        MetadataQuery::Columns {
            schema: "main".into(),
            object: "users".into(),
        },
        MetadataQuery::AllColumns,
        MetadataQuery::AllForeignKeys,
        MetadataQuery::AllPrimaryKeys,
        MetadataQuery::ProcedureParameters {
            schema: "main".into(),
            object: "nope".into(),
        },
        MetadataQuery::Indexes {
            schema: "main".into(),
            table: "users".into(),
        },
        MetadataQuery::Triggers {
            schema: "main".into(),
            table: "users".into(),
        },
        MetadataQuery::Constraints {
            schema: "main".into(),
            table: "users".into(),
        },
        MetadataQuery::Statistics {
            schema: "main".into(),
            table: "users".into(),
        },
        MetadataQuery::Types { schema: None },
        MetadataQuery::TypeDetail {
            schema: "main".into(),
            type_name: "nope".into(),
        },
        MetadataQuery::TableGeneral {
            schema: "main".into(),
            table: "users".into(),
        },
        MetadataQuery::ObjectGeneral {
            schema: "main".into(),
            object: "users".into(),
        },
        MetadataQuery::IndexGeneral {
            schema: "main".into(),
            table: "users".into(),
            index: "idx_users_name".into(),
        },
        MetadataQuery::IndexColumns {
            schema: "main".into(),
            table: "users".into(),
            index: "idx_users_name".into(),
        },
        MetadataQuery::StatisticGeneral {
            schema: "main".into(),
            table: "users".into(),
            statistic: "nope".into(),
        },
        MetadataQuery::StatisticColumns {
            schema: "main".into(),
            table: "users".into(),
            statistic: "nope".into(),
        },
        MetadataQuery::ObjectDefinition {
            schema: "main".into(),
            object: "active_users".into(),
        },
        MetadataQuery::ObjectPermissions {
            schema: "main".into(),
            object: "users".into(),
        },
        MetadataQuery::TableIndexColumns {
            schema: "main".into(),
            table: "users".into(),
        },
        MetadataQuery::ConstraintDefinition {
            schema: "main".into(),
            table: "users".into(),
            name: "sqlite_autoindex_users_1".into(),
        },
        MetadataQuery::ColumnDefaults {
            schema: "main".into(),
            table: "users".into(),
        },
        MetadataQuery::DependentViews {
            schema: "main".into(),
            table: "users".into(),
        },
        MetadataQuery::TriggerDefinition {
            schema: "main".into(),
            name: "trg_users_touch".into(),
        },
        MetadataQuery::DatabaseFiles {
            database: "main".into(),
        },
        MetadataQuery::ServerConfiguration,
        MetadataQuery::ServerStatus,
        MetadataQuery::Databases,
        MetadataQuery::ServerCapability,
    ]
}

/// The per-table index-column listing joins `pragma_index_xinfo` (the only
/// pragma that reports `desc`) and keeps just the key columns.
#[test]
fn table_index_columns_report_key_columns_with_direction() {
    let conn = seeded_db();
    let sql = sql_for_dialect(
        &MetadataQuery::TableIndexColumns {
            schema: "main".into(),
            table: "users".into(),
        },
        MetadataDialect::Sqlite,
    )
    .unwrap();
    let mut stmt = conn.prepare(&sql).unwrap();
    let rows: Vec<(String, String, i64, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(4)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        rows.contains(&("idx_users_name".to_string(), "name".to_string(), 1, 0)),
        "{rows:?}"
    );
    assert!(
        rows.iter().all(|(_, column, _, _)| column != "rowid"),
        "non-key rowid columns must be filtered: {rows:?}"
    );
}

#[test]
fn every_sqlite_dialect_query_executes_on_a_real_database() {
    let conn = seeded_db();
    for variant in every_sqlite_variant() {
        let sql = sql_for_dialect(&variant, MetadataDialect::Sqlite)
            .unwrap_or_else(|e| panic!("{variant:?} must have SQLite SQL: {e}"));
        let mut stmt = conn
            .prepare(&sql)
            .unwrap_or_else(|e| panic!("{variant:?} SQL failed to prepare: {e}\nSQL: {sql}"));
        let column_count = stmt.column_count();
        assert!(column_count > 0, "{variant:?} selects no columns");
        // Execute fully so runtime errors (bad pragma args, wrong types)
        // surface, not just parse errors.
        let mut rows = stmt.query([]).unwrap();
        let mut n = 0usize;
        while rows
            .next()
            .unwrap_or_else(|e| panic!("{variant:?} row iteration failed: {e}\nSQL: {sql}"))
            .is_some()
        {
            n += 1;
            if n > 10_000 {
                panic!("{variant:?} returned an implausible row flood");
            }
        }
    }
}

#[test]
fn sqlite_definition_and_shape_probes_return_expected_content() {
    let conn = seeded_db();

    // The view's stored source comes back verbatim.
    let sql = sql_for_dialect(
        &MetadataQuery::ObjectDefinition {
            schema: "main".into(),
            object: "active_users".into(),
        },
        MetadataDialect::Sqlite,
    )
    .unwrap();
    let definition: String = conn.query_row(&sql, [], |r| r.get(0)).unwrap();
    assert!(
        definition.contains("CREATE VIEW active_users"),
        "{definition}"
    );

    // Columns report the rowid-alias PK as not-nullable identity.
    let sql = sql_for_dialect(
        &MetadataQuery::Columns {
            schema: "main".into(),
            object: "users".into(),
        },
        MetadataDialect::Sqlite,
    )
    .unwrap();
    let mut stmt = conn.prepare(&sql).unwrap();
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(5)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let id_nullable = rows.iter().find(|(name, _)| name == "id").unwrap().1;
    assert_eq!(id_nullable, 0, "INTEGER PRIMARY KEY is implicitly NOT NULL");
}

/// Every `pragma_*` table-valued function name the SQLite dialect SQL uses.
fn pragma_functions_used() -> Vec<String> {
    let mut names = Vec::new();
    for variant in every_sqlite_variant() {
        let sql = sql_for_dialect(&variant, MetadataDialect::Sqlite).unwrap();
        let mut rest = sql.as_str();
        while let Some(at) = rest.find("pragma_") {
            let tail = &rest[at..];
            let end = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(tail.len());
            let name = tail[..end].to_string();
            if !names.contains(&name) {
                names.push(name);
            }
            rest = &tail[end..];
        }
    }
    names
}

fn all_rows(conn: &Connection, variant: &MetadataQuery) -> Result<Vec<String>, String> {
    let sql = sql_for_dialect(variant, MetadataDialect::Sqlite).unwrap();
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let width = stmt.column_count();
    let mut rows = stmt.query([]).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let cells: Vec<String> = (0..width)
            .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
            .collect();
        out.push(cells.join("|"));
    }
    Ok(out)
}

/// A user table named like one of SQLite's `pragma_*` functions (a TEMP
/// table shadows the built-in function) must not break or change the
/// metadata SQL. DBeaver #42370: a TEMP `pragma_table_list` broke all
/// metadata loading for the connection.
#[test]
fn pragma_named_temp_tables_do_not_hijack_metadata_queries() {
    let names = pragma_functions_used();
    assert!(
        names.len() >= 4,
        "expected several pragma functions: {names:?}"
    );

    let clean = seeded_db();
    let shadowed = seeded_db();
    for name in &names {
        shadowed
            .execute_batch(&format!("CREATE TEMP TABLE {name} (decoy TEXT);"))
            .unwrap();
    }
    for variant in every_sqlite_variant() {
        // The schema list legitimately gains `temp` once a TEMP table exists.
        if matches!(
            variant,
            MetadataQuery::Schemas | MetadataQuery::Databases | MetadataQuery::DatabaseFiles { .. }
        ) {
            all_rows(&shadowed, &variant).unwrap_or_else(|e| {
                panic!("{variant:?} broke under a pragma-named TEMP table: {e}")
            });
            continue;
        }
        let expected = all_rows(&clean, &variant).unwrap();
        let actual = all_rows(&shadowed, &variant)
            .unwrap_or_else(|e| panic!("{variant:?} broke under a pragma-named TEMP table: {e}"));
        assert_eq!(
            actual, expected,
            "{variant:?} changed under a pragma-named TEMP table"
        );
    }
}
