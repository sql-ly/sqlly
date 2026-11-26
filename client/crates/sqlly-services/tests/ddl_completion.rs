//! Completion inside DDL: `CREATE` / `ALTER` / `DROP` / `TRUNCATE` heads,
//! column and parameter definitions, key column lists, and the bodies of
//! procedures, functions, views and triggers.
//!
//! Each DDL slot takes one specific thing — a type after a column name, a
//! table after `ALTER TABLE`, the table's own columns in a key list — and
//! never the query path's built-in functions. These run through the full
//! service pipeline, the way the editor asks.

use sqlly_core::{DatabaseType, ObjectKind};
use sqlly_intel::intellisense::{CompletionCatalog, CompletionKind, CompletionObject};
use sqlly_services::intellisense::*;

fn catalog() -> CompletionCatalog {
    let o = |name: &str, kind, cols: &[&str]| CompletionObject {
        database: Some("appdb".to_string()),
        schema: Some("dbo".to_string()),
        name: name.to_string(),
        kind,
        columns: cols.iter().map(|c| c.to_string()).collect(),
        parameters: Vec::new(),
        foreign_keys: Vec::new(),
    };
    CompletionCatalog {
        databases: vec!["appdb".to_string()],
        schemas: vec!["dbo".to_string(), "Sales".to_string()],
        objects: vec![
            o(
                "Orders",
                ObjectKind::Table,
                &["OrderId", "CustomerId", "Total", "CreatedAt"],
            ),
            o(
                "Customers",
                ObjectKind::Table,
                &["CustomerId", "Name", "Email"],
            ),
            o("ActiveOrders", ObjectKind::View, &["OrderId", "Total"]),
            o("GetUser", ObjectKind::StoredProcedure, &[]),
            o("CalcTotal", ObjectKind::ScalarFunction, &[]),
        ],
        filesystem_schemas: Vec::new(),
        snippets: sqlly_services::intellisense::DefaultIntellisense.default_snippets(),
    }
}

struct Got {
    labels: Vec<String>,
    kinds: Vec<CompletionKind>,
}

/// Completes at the `|` in `case` the way Ctrl+Space does, checking the
/// automatic popup against it.
fn complete_at(db: DatabaseType, case: &str) -> Got {
    let cursor = case.find('|').expect("case marks the caret with |");
    let sql = case.replacen('|', "", 1);
    let cat = catalog();
    let rules = IntelliSenseRuleConfig::default();
    let run = |manual: bool| {
        let mut req = if manual {
            CompletionRequest::manual(&sql, cursor, &cat, &[], &rules)
        } else {
            CompletionRequest::auto(&sql, cursor, &cat, &[], &rules)
        };
        req.database_type = Some(db);
        let response = DefaultIntellisense.complete_enriched_response(&req);
        Got {
            labels: response.items.iter().map(|i| i.label.clone()).collect(),
            kinds: response.items.iter().map(|i| i.kind).collect(),
        }
    };
    let auto = run(false);
    let manual = run(true);
    // The automatic popup stays shut in a few places (a fresh line, a typed
    // word that is itself a keyword); where it opens it shows the same list.
    if !auto.labels.is_empty() {
        assert_eq!(
            auto.labels, manual.labels,
            "auto and manual differ at {case:?}"
        );
    }
    manual
}

/// The first items at the caret are exactly `lead`, in order.
#[track_caller]
fn assert_leads(db: DatabaseType, case: &str, lead: &[&str]) {
    let got = complete_at(db, case);
    let head: Vec<&str> = got
        .labels
        .iter()
        .take(lead.len())
        .map(String::as_str)
        .collect();
    assert_eq!(head, lead, "{db:?} {case:?} offered {:?}", got.labels);
}

/// The caret offers exactly `all`, in order, and nothing else.
#[track_caller]
fn assert_offers(db: DatabaseType, case: &str, all: &[&str]) {
    let got = complete_at(db, case);
    assert_eq!(got.labels, all, "{db:?} {case:?}");
}

/// Every item at the caret is one of `kinds`, and there is at least one.
#[track_caller]
fn assert_only(db: DatabaseType, case: &str, kinds: &[CompletionKind]) {
    let got = complete_at(db, case);
    assert!(!got.labels.is_empty(), "{db:?} {case:?} offered nothing");
    for (label, kind) in got.labels.iter().zip(&got.kinds) {
        assert!(
            kinds.contains(kind),
            "{db:?} {case:?} offered {label} ({kind:?})"
        );
    }
}

const TSQL: DatabaseType = DatabaseType::SqlServer;
const PG: DatabaseType = DatabaseType::Postgres;
const MYSQL: DatabaseType = DatabaseType::MySql;

const EXISTING_TABLES: &[&str] = &["dbo.Customers", "dbo.Orders", "Sales", "dbo", "appdb"];
const ORDER_COLUMNS: &[&str] = &["CreatedAt", "CustomerId", "OrderId", "Total"];
const TSQL_TYPES: &[&str] = &["INT", "BIGINT", "NVARCHAR", "VARCHAR", "BIT", "DATETIME2"];

#[test]
fn create_offers_the_object_kinds_not_functions() {
    assert_only(TSQL, "CREATE |", &[CompletionKind::Keyword]);
    assert_leads(
        TSQL,
        "CREATE |",
        &["TABLE", "VIEW", "PROCEDURE", "FUNCTION", "INDEX"],
    );
    assert_leads(TSQL, "CREATE T|", &["TABLE", "TRIGGER", "TYPE"]);
    assert_offers(TSQL, "CREATE OR |", &["ALTER"]);
    assert_offers(PG, "CREATE OR |", &["REPLACE"]);
    assert_offers(MYSQL, "CREATE OR |", &["REPLACE"]);
    assert_offers(
        TSQL,
        "CREATE UNIQUE |",
        &["INDEX", "CLUSTERED INDEX", "NONCLUSTERED INDEX"],
    );
    assert_offers(TSQL, "TRUNCATE |", &["TABLE"]);
    assert_only(TSQL, "DROP |", &[CompletionKind::Keyword]);
    assert_leads(TSQL, "DROP |", &["TABLE", "VIEW", "PROCEDURE", "FUNCTION"]);
}

#[test]
fn a_new_objects_name_offers_schemas_only() {
    for case in [
        "CREATE TABLE |",
        "CREATE PROCEDURE |",
        "CREATE FUNCTION |",
        "CREATE VIEW |",
        "CREATE TRIGGER |",
    ] {
        assert_offers(TSQL, case, &["Sales", "dbo"]);
    }
    // The name itself is new: existing objects are not suggestions for it.
    for case in [
        "CREATE TABLE dbo.|",
        "CREATE TABLE Cu|",
        "CREATE PROCEDURE dbo.Ge|",
        "CREATE VIEW dbo.Ac|",
        "CREATE INDEX |",
        "CREATE UNIQUE NONCLUSTERED INDEX |",
        "CREATE SCHEMA |",
        "CREATE DATABASE |",
        "DROP INDEX |",
    ] {
        assert_offers(TSQL, case, &[]);
    }
}

#[test]
fn a_column_name_is_free_text_and_its_type_comes_next() {
    for case in [
        "CREATE TABLE dbo.NewThing (\n    Id|",
        "CREATE TABLE dbo.NewThing (\n    Cou|",
        "CREATE TABLE dbo.NewThing (\n    Id INT NOT NULL,\n    Na|",
        "CREATE TABLE dbo.NewThing (\n    Id INT NOT NULL,\n    CONSTRAINT |",
        "CREATE VIEW dbo.NewView (|",
        "CREATE FUNCTION dbo.NewFn(|",
        "CREATE PROCEDURE dbo.NewProc\n    @|",
    ] {
        assert_offers(TSQL, case, &[]);
    }
    for case in [
        "CREATE TABLE dbo.NewThing (\n    Id |",
        "CREATE TABLE #tmp (a int, b |",
        "CREATE TYPE dbo.MyList AS TABLE (Id |",
        "DECLARE @t TABLE (Id |",
        "CREATE FUNCTION dbo.f() RETURNS @t TABLE (Id |",
        "CREATE PROC p AS BEGIN\n CREATE TABLE #x (Id |",
        "ALTER TABLE dbo.Orders ADD NewCol |",
        "ALTER TABLE dbo.Orders ALTER COLUMN Total |",
        "CREATE SEQUENCE dbo.Seq AS |",
    ] {
        assert_only(TSQL, case, &[CompletionKind::Type]);
        assert_leads(TSQL, case, TSQL_TYPES);
    }
    assert_leads(
        TSQL,
        "CREATE TABLE dbo.NewThing (\n    Id in|",
        &["INT", "BIGINT", "INTEGER"],
    );
    assert_leads(
        TSQL,
        "ALTER TABLE dbo.Orders ADD NewCol in|",
        &["INT", "BIGINT", "INTEGER"],
    );
    assert_offers(
        TSQL,
        "CREATE TABLE t (\n    Id INT NOT NULL,\n    Name NVARCHAR(|",
        &["MAX"],
    );
}

#[test]
fn an_empty_column_slot_offers_the_table_constraints() {
    let constraints = [
        "CONSTRAINT",
        "PRIMARY KEY",
        "FOREIGN KEY",
        "UNIQUE",
        "CHECK",
    ];
    for case in [
        "CREATE TABLE dbo.NewThing (|",
        "CREATE TABLE dbo.NewThing (\n    |",
        "CREATE TABLE t (\n    a int,\n    |",
        "ALTER TABLE dbo.Orders ADD |",
    ] {
        assert_offers(TSQL, case, &constraints);
    }
}

#[test]
fn after_a_columns_type_come_its_constraints() {
    let lead = [
        "NOT NULL",
        "NULL",
        "PRIMARY KEY",
        "DEFAULT",
        "REFERENCES",
        "UNIQUE",
        "CHECK",
    ];
    assert_only(
        TSQL,
        "CREATE TABLE t (\n    Id INT |",
        &[CompletionKind::Keyword],
    );
    assert_leads(TSQL, "CREATE TABLE t (\n    Id INT |", &lead);
    assert_leads(
        TSQL,
        "CREATE TABLE t (\n    Id INT,\n    Name NVARCHAR(100) |",
        &lead,
    );
    assert_leads(PG, "CREATE TABLE t (id int |", &lead);
    assert_leads(MYSQL, "CREATE TABLE t (id int |", &lead);
    assert_offers(TSQL, "CREATE TABLE t (\n    Id INT NOT |", &["NULL"]);
    assert_offers(
        TSQL,
        "ALTER TABLE dbo.Orders ALTER COLUMN Total decimal(18,2) |",
        &["NULL", "NOT NULL", "COLLATE"],
    );
}

#[test]
fn key_lists_offer_the_tables_own_columns() {
    assert_offers(
        TSQL,
        "CREATE TABLE t (\n    Id INT NOT NULL,\n    CONSTRAINT PK_x PRIMARY KEY (|",
        &["Id"],
    );
    // A column already in the key is not offered twice.
    assert_offers(
        TSQL,
        "CREATE TABLE t (a int, b int, PRIMARY KEY (a, |",
        &["b"],
    );
    for case in [
        "CREATE INDEX IX_Orders_Cust ON dbo.Orders (|",
        "CREATE INDEX IX_Orders_Cust ON dbo.Orders (CustomerId) INCLUDE (|",
        "CREATE INDEX IX_Orders_Cust ON dbo.Orders (CustomerId) WHERE |",
        "ALTER TABLE dbo.Orders ADD CONSTRAINT FK_x FOREIGN KEY (|",
        "ALTER TABLE dbo.Orders DROP COLUMN |",
        "ALTER TABLE dbo.Orders ALTER COLUMN |",
    ] {
        assert_offers(TSQL, case, ORDER_COLUMNS);
    }
    assert_offers(
        TSQL,
        "CREATE INDEX IX_Orders_Cust ON dbo.Orders (Cu|",
        &["CustomerId"],
    );
    assert_offers(
        TSQL,
        "CREATE INDEX ix ON dbo.Orders (CustomerId DE|",
        &["DESC"],
    );
    assert_offers(
        TSQL,
        "CREATE INDEX ix ON dbo.Orders (CustomerId) |",
        &["INCLUDE", "WHERE", "WITH"],
    );
    for db in [PG, MYSQL] {
        assert_offers(db, "CREATE INDEX ix ON Orders (|", ORDER_COLUMNS);
        assert_offers(db, "ALTER TABLE Orders MODIFY COLUMN |", ORDER_COLUMNS);
        assert_offers(db, "ALTER TABLE Orders RENAME COLUMN |", ORDER_COLUMNS);
    }
}

#[test]
fn references_offers_tables_then_the_referenced_tables_columns() {
    for case in [
        "CREATE TABLE t (\n    Id INT NOT NULL,\n    CustomerId INT REFERENCES |",
        "ALTER TABLE dbo.Orders ADD CONSTRAINT FK_x FOREIGN KEY (CustomerId) REFERENCES |",
    ] {
        assert_offers(TSQL, case, EXISTING_TABLES);
    }
    for case in [
        "CREATE TABLE t (\n    Id INT NOT NULL,\n    CustomerId INT REFERENCES dbo.Customers (|",
        "ALTER TABLE dbo.Orders ADD CONSTRAINT FK_x FOREIGN KEY (CustomerId) REFERENCES dbo.Customers (|",
    ] {
        assert_offers(TSQL, case, &["CustomerId", "Email", "Name"]);
    }
    let fk = "ALTER TABLE dbo.Orders ADD CONSTRAINT FK_x FOREIGN KEY (CustomerId) REFERENCES dbo.Customers (CustomerId) ON ";
    assert_offers(TSQL, &format!("{fk}|"), &["DELETE", "UPDATE"]);
    assert_offers(
        TSQL,
        &format!("{fk}DELETE |"),
        &["CASCADE", "SET NULL", "SET DEFAULT", "NO ACTION"],
    );
}

#[test]
fn defaults_checks_and_computed_columns_take_expressions() {
    // A default leads with what defaults usually call.
    for case in [
        "CREATE TABLE t (\n    Id INT NOT NULL,\n    CreatedAt DATETIME2 DEFAULT |",
        "CREATE TABLE t (a int, b int DEFAULT |",
    ] {
        assert_only(
            TSQL,
            case,
            &[CompletionKind::Function, CompletionKind::Keyword],
        );
        assert_leads(
            TSQL,
            case,
            &[
                "SYSUTCDATETIME",
                "SYSDATETIME",
                "GETUTCDATE",
                "GETDATE",
                "NEWID",
            ],
        );
    }
    assert_leads(
        TSQL,
        "CREATE TABLE t (a int, b datetime2 DEFAULT (SYS|",
        &["SYSUTCDATETIME", "SYSDATETIME"],
    );
    // A check and a computed column see the columns defined so far.
    assert_leads(
        TSQL,
        "CREATE TABLE t (a int, b int, CONSTRAINT ck CHECK (|",
        &["a", "b", "NULL"],
    );
    assert_leads(
        TSQL,
        "CREATE TABLE t (a int, b int, CONSTRAINT ck CHECK (a > 0 AN|",
        &["AND"],
    );
    assert_leads(
        TSQL,
        "CREATE TABLE t (a int, b int, c AS (|",
        &["a", "b", "c", "NULL"],
    );
    assert_leads(
        TSQL,
        "CREATE TABLE t (a int, b int, c AS |",
        &["a", "b", "c", "NULL"],
    );
}

#[test]
fn alter_drop_and_truncate_offer_existing_objects_not_functions() {
    for case in [
        "ALTER TABLE |",
        "DROP TABLE |",
        "DROP TABLE IF EXISTS |",
        "DROP TABLE dbo.Orders, |",
        "TRUNCATE TABLE |",
        "CREATE INDEX IX_Orders_Cust ON |",
        "DROP INDEX IX_x ON |",
        "CREATE PROC p AS BEGIN\n ALTER TABLE |",
        "CREATE PROC p AS BEGIN\n DROP TABLE |",
        "CREATE PROC p AS BEGIN\n TRUNCATE TABLE |",
        "IF OBJECT_ID('x') IS NOT NULL DROP TABLE |",
    ] {
        assert_offers(TSQL, case, EXISTING_TABLES);
    }
    for db in [PG, MYSQL] {
        assert_offers(db, "ALTER TABLE |", EXISTING_TABLES);
        assert_offers(db, "CREATE INDEX ix ON |", EXISTING_TABLES);
    }
    assert_offers(TSQL, "DROP TABLE dbo.|", &["dbo.Customers", "dbo.Orders"]);
    assert_offers(TSQL, "ALTER TABLE dbo.O|", &["dbo.Orders", "dbo.Customers"]);
    // Each routine kind offers only its own kind.
    assert_offers(TSQL, "ALTER PROCEDURE |", &["dbo.GetUser", "Sales", "dbo"]);
    assert_offers(
        TSQL,
        "CREATE OR ALTER PROCEDURE |",
        &["dbo.GetUser", "Sales", "dbo"],
    );
    assert_offers(TSQL, "ALTER PROCEDURE dbo.Ge|", &["dbo.GetUser"]);
    assert_offers(TSQL, "DROP PROCEDURE |", &["dbo.GetUser", "Sales", "dbo"]);
    assert_offers(TSQL, "ALTER FUNCTION |", &["dbo.CalcTotal", "Sales", "dbo"]);
    assert_offers(TSQL, "DROP FUNCTION |", &["dbo.CalcTotal", "Sales", "dbo"]);
    assert_offers(TSQL, "ALTER VIEW |", &["dbo.ActiveOrders", "Sales", "dbo"]);
    assert_offers(TSQL, "DROP VIEW |", &["dbo.ActiveOrders", "Sales", "dbo"]);
    assert_offers(
        PG,
        "CREATE OR REPLACE VIEW |",
        &["dbo.ActiveOrders", "Sales", "dbo"],
    );
}

#[test]
fn alter_table_actions_follow_the_dialect() {
    assert_only(TSQL, "ALTER TABLE dbo.Orders |", &[CompletionKind::Keyword]);
    assert_leads(
        TSQL,
        "ALTER TABLE dbo.Orders |",
        &[
            "ADD",
            "ALTER COLUMN",
            "DROP COLUMN",
            "DROP CONSTRAINT",
            "ADD CONSTRAINT",
        ],
    );
    assert_offers(
        TSQL,
        "ALTER TABLE dbo.Orders DROP |",
        &["COLUMN", "CONSTRAINT"],
    );
    assert_leads(
        PG,
        "ALTER TABLE Orders |",
        &["ADD COLUMN", "ALTER COLUMN", "DROP COLUMN"],
    );
    assert_leads(
        MYSQL,
        "ALTER TABLE Orders |",
        &["ADD COLUMN", "MODIFY COLUMN", "CHANGE COLUMN"],
    );
    assert_offers(
        PG,
        "ALTER TABLE Orders ALTER COLUMN Total |",
        &[
            "TYPE",
            "SET DATA TYPE",
            "SET DEFAULT",
            "DROP DEFAULT",
            "SET NOT NULL",
            "DROP NOT NULL",
        ],
    );
    // MySQL's ALTER COLUMN only moves the default; MODIFY retypes.
    assert_offers(
        MYSQL,
        "ALTER TABLE Orders ALTER COLUMN Total |",
        &["SET DEFAULT", "DROP DEFAULT"],
    );
    assert_leads(
        MYSQL,
        "ALTER TABLE Orders MODIFY COLUMN Total |",
        &["INT", "BIGINT", "VARCHAR"],
    );
    assert_leads(
        PG,
        "ALTER TABLE Orders ALTER COLUMN Total TYPE |",
        &["INTEGER", "BIGINT", "TEXT"],
    );
    for db in [PG, MYSQL] {
        assert_offers(db, "ALTER TABLE Orders ADD COLUMN |", &[]);
        assert_only(
            db,
            "ALTER TABLE Orders ADD COLUMN x |",
            &[CompletionKind::Type],
        );
    }
}

#[test]
fn procedure_and_function_heads_offer_types_and_the_next_clause() {
    for case in [
        "CREATE PROCEDURE dbo.NewProc\n    @Id |",
        "CREATE PROCEDURE dbo.NewProc\n    @Id INT,\n    @Name |",
        "CREATE FUNCTION dbo.NewFn(@a |",
    ] {
        assert_only(TSQL, case, &[CompletionKind::Type]);
        assert_leads(TSQL, case, TSQL_TYPES);
    }
    assert_leads(
        TSQL,
        "CREATE PROCEDURE dbo.NewProc\n    @Id in|",
        &["INT", "BIGINT", "INTEGER"],
    );
    assert_offers(
        TSQL,
        "CREATE PROCEDURE dbo.NewProc |",
        &["AS", "WITH RECOMPILE", "WITH EXECUTE AS"],
    );
    assert_offers(
        TSQL,
        "CREATE PROCEDURE dbo.NewProc\n    @Id INT = |",
        &["NULL"],
    );
    // On a fresh line after the parameters the body's AS is what comes next.
    assert_leads(
        TSQL,
        "CREATE PROCEDURE dbo.NewProc\n    @Id INT\n|",
        &["AS"],
    );
    assert_leads(
        TSQL,
        "CREATE PROCEDURE dbo.NewProc\n    @Id INT\nA|",
        &["AS"],
    );
    assert_offers(TSQL, "CREATE FUNCTION dbo.NewFn(@a INT) |", &["RETURNS"]);
    assert_leads(
        TSQL,
        "CREATE FUNCTION dbo.NewFn(@a INT) RETURNS |",
        &["TABLE", "INT", "BIGINT"],
    );
    assert_leads(
        TSQL,
        "CREATE FUNCTION dbo.NewFn(@a INT) RETURNS in|",
        &["INT", "BIGINT", "INTEGER"],
    );
    assert_offers(
        TSQL,
        "CREATE FUNCTION dbo.NewFn(@a INT) RETURNS INT |",
        &["AS", "WITH SCHEMABINDING", "BEGIN"],
    );
    assert_leads(
        PG,
        "CREATE FUNCTION f(a int) RETURNS |",
        &["TABLE", "SETOF", "VOID", "TRIGGER", "INTEGER"],
    );
    for db in [PG, MYSQL] {
        assert_only(db, "CREATE FUNCTION f(a |", &[CompletionKind::Type]);
        assert_only(db, "CREATE PROCEDURE p(IN a |", &[CompletionKind::Type]);
        assert_offers(db, "CREATE FUNCTION f(a int) |", &["RETURNS"]);
        assert_only(
            db,
            "CREATE PROCEDURE p(IN a INT)\nBEGIN\n  DECLARE x |",
            &[CompletionKind::Type],
        );
        assert_only(
            db,
            "CREATE TEMPORARY TABLE tmp (a |",
            &[CompletionKind::Type],
        );
    }
    assert_leads(
        PG,
        "CREATE TABLE t (id |",
        &["INTEGER", "BIGINT", "TEXT", "VARCHAR", "BOOLEAN"],
    );
    assert_leads(
        MYSQL,
        "CREATE TABLE t (id |",
        &["INT", "BIGINT", "VARCHAR", "TEXT", "DATETIME"],
    );
}

#[test]
fn declare_offers_types_then_hands_the_value_to_the_expression_path() {
    for case in [
        "DECLARE @x |",
        "DECLARE @x int, @y |",
        "CREATE PROCEDURE dbo.NewProc\n    @Id INT\nAS\nBEGIN\n    DECLARE @x |",
    ] {
        assert_only(TSQL, case, &[CompletionKind::Type, CompletionKind::Keyword]);
        assert_leads(TSQL, case, &["TABLE", "CURSOR", "INT", "BIGINT"]);
    }
    assert_offers(TSQL, "CREATE PROC p @Id INT AS BEGIN\n    DECLARE @|", &[]);
    // The initial value is an ordinary expression.
    let got = complete_at(TSQL, "DECLARE @x int = |");
    assert!(got.kinds.contains(&CompletionKind::Function));
}

#[test]
fn view_index_trigger_and_sequence_heads_offer_their_next_keyword() {
    assert_offers(
        TSQL,
        "CREATE VIEW dbo.NewView |",
        &["AS", "WITH SCHEMABINDING"],
    );
    assert_offers(TSQL, "CREATE VIEW dbo.NewView AS |", &["SELECT", "WITH"]);
    assert_offers(TSQL, "CREATE VIEW v AS S|", &["SELECT"]);
    assert_offers(PG, "CREATE MATERIALIZED VIEW mv AS |", &["SELECT", "WITH"]);
    assert_offers(TSQL, "CREATE INDEX IX_Orders_Cust |", &["ON"]);
    assert_offers(
        PG,
        "CREATE INDEX ix ON Orders USING |",
        &["BTREE", "HASH", "GIN", "GIST", "BRIN"],
    );
    assert_offers(
        MYSQL,
        "CREATE INDEX ix ON Orders USING |",
        &["BTREE", "HASH"],
    );
    assert_leads(
        TSQL,
        "CREATE SEQUENCE dbo.Seq |",
        &["AS", "START WITH", "INCREMENT BY"],
    );

    let on_table = [
        "dbo.ActiveOrders",
        "dbo.Customers",
        "dbo.Orders",
        "Sales",
        "dbo",
        "appdb",
    ];
    assert_offers(TSQL, "CREATE TRIGGER trg_x ON |", &on_table);
    assert_offers(
        TSQL,
        "CREATE TRIGGER trg_x ON dbo.Orders |",
        &["AFTER", "FOR", "INSTEAD OF"],
    );
    assert_offers(
        TSQL,
        "CREATE TRIGGER trg_x ON dbo.Orders AFTER |",
        &["INSERT", "UPDATE", "DELETE"],
    );
    assert_offers(
        TSQL,
        "CREATE TRIGGER trg ON dbo.Orders AFTER INSERT, |",
        &["INSERT", "UPDATE", "DELETE"],
    );
    assert_offers(
        TSQL,
        "CREATE TRIGGER trg ON dbo.Orders AFTER INSERT |",
        &["AS"],
    );
    assert_offers(
        PG,
        "CREATE TRIGGER trg |",
        &["BEFORE", "AFTER", "INSTEAD OF"],
    );
    // MySQL has no INSTEAD OF triggers.
    assert_offers(MYSQL, "CREATE TRIGGER trg |", &["BEFORE", "AFTER"]);
    for db in [PG, MYSQL] {
        assert_offers(
            db,
            "CREATE TRIGGER trg BEFORE |",
            &["INSERT", "UPDATE", "DELETE"],
        );
        assert_offers(db, "CREATE TRIGGER trg BEFORE INSERT |", &["ON"]);
        assert_offers(db, "CREATE TRIGGER trg BEFORE INSERT ON |", &on_table);
    }
    assert_offers(
        MYSQL,
        "CREATE TRIGGER trg BEFORE INSERT ON Orders |",
        &["FOR EACH ROW"],
    );
    assert_offers(
        PG,
        "CREATE TRIGGER trg BEFORE INSERT ON Orders |",
        &["FOR EACH ROW", "FOR EACH STATEMENT", "EXECUTE FUNCTION"],
    );
}

#[test]
fn routine_bodies_complete_like_ordinary_queries() {
    let body = "CREATE PROCEDURE dbo.NewProc\n    @Id INT\nAS\nBEGIN\n    ";
    assert_offers(TSQL, &format!("{body}S|"), &["SELECT", "INSERT"]);
    assert_leads(
        TSQL,
        &format!("{body}SELECT |"),
        &["@Id", "CreatedAt", "CustomerId"],
    );
    assert_offers(
        TSQL,
        &format!("{body}SELECT * FROM dbo.Orders o WHERE o.|"),
        &["o.CreatedAt", "o.CustomerId", "o.OrderId", "o.Total"],
    );
    assert_leads(
        TSQL,
        "CREATE FUNCTION dbo.NewFn(@a INT) RETURNS TABLE AS RETURN (SELECT |",
        &["@a", "CreatedAt"],
    );
    assert_leads(
        TSQL,
        "CREATE VIEW dbo.NewView AS\nSELECT |",
        &["CreatedAt", "CustomerId"],
    );
    assert_offers(
        TSQL,
        "CREATE VIEW dbo.NewView AS\nSELECT o.| FROM dbo.Orders o",
        &["o.CreatedAt", "o.CustomerId", "o.OrderId", "o.Total"],
    );
}

#[test]
fn a_bare_at_sign_offers_the_routines_parameters() {
    for case in [
        "CREATE PROC p @Id int AS BEGIN\n SELECT * FROM dbo.Orders WHERE OrderId = @|",
        "CREATE PROC p @Id int AS BEGIN\n SELECT * FROM dbo.Orders WHERE OrderId = @I|",
        "CREATE PROCEDURE dbo.NewProc\n    @Id INT\nAS\nBEGIN\n    SELECT * FROM dbo.Orders WHERE OrderId = @|",
    ] {
        assert_offers(TSQL, case, &["@Id"]);
    }
}

#[test]
fn end_closes_an_open_block_or_case() {
    assert_offers(TSQL, "CREATE PROC p AS BEGIN\n SELECT 1;\nEN|", &["END"]);
    assert_leads(TSQL, "SELECT CASE WHEN 1=1 THEN 1 EN|", &["END"]);
    // Nothing is open here, so END is not a suggestion.
    for case in [
        "BEGIN TRAN\nEN|",
        "CREATE PROC p AS SET NOCOUNT ON;\n SELECT 1;\nEN|",
    ] {
        let got = complete_at(TSQL, case);
        assert!(
            !got.labels.iter().any(|l| l == "END"),
            "{case:?} offered END"
        );
    }
}

#[test]
fn a_new_statement_after_ddl_is_not_part_of_it() {
    for case in [
        "CREATE TABLE t (a int,\n    b int\n)\nSEL|",
        "ALTER TABLE dbo.Orders ADD NewCol int\nSEL|",
        "DECLARE @x int\nSEL|",
    ] {
        assert_offers(TSQL, case, &["SELECT"]);
    }
    for case in [
        "CREATE TABLE t (a int)\n|",
        "ALTER TABLE dbo.Orders ADD NewCol int\n|",
    ] {
        let got = complete_at(TSQL, case);
        assert!(
            got.labels.iter().any(|l| l == "SELECT"),
            "{case:?} offered {:?}",
            got.labels
        );
        assert!(
            !got.kinds.contains(&CompletionKind::Type),
            "{case:?} offered types"
        );
    }
}

/// A FROM / JOIN source holds rows: tables, views, the user's own functions
/// and the built-ins that return a rowset — never `ABS`.
#[test]
fn a_from_slot_offers_no_scalar_built_in_functions() {
    for (db, case, rowset) in [
        (TSQL, "SELECT * FROM |", "OPENJSON"),
        (
            TSQL,
            "CREATE PROC p AS BEGIN\n SELECT * FROM |",
            "STRING_SPLIT",
        ),
        (
            TSQL,
            "CREATE TRIGGER trg_x ON dbo.Orders AFTER INSERT AS BEGIN SELECT * FROM |",
            "OPENJSON",
        ),
        (TSQL, "SELECT * FROM dbo.Orders o JOIN |", "OPENJSON"),
        (
            PG,
            "CREATE PROCEDURE p(IN a INT)\nBEGIN\n  SELECT * FROM |",
            "generate_series",
        ),
        (
            MYSQL,
            "CREATE PROCEDURE p(IN a INT)\nBEGIN\n  SELECT * FROM |",
            "JSON_TABLE",
        ),
    ] {
        let got = complete_at(db, case);
        for scalar in [
            "ABS", "abs", "ACOS", "UPPER", "upper", "GETDATE", "COALESCE", "coalesce",
        ] {
            assert!(
                !got.labels.iter().any(|l| l == scalar),
                "{db:?} {case:?} offered {scalar}"
            );
        }
        assert!(
            got.labels.iter().any(|l| l == rowset),
            "{db:?} {case:?} lost {rowset}: {:?}",
            got.labels
        );
        assert!(
            got.labels.iter().any(|l| l == "dbo.Orders"),
            "{db:?} {case:?} lost its tables"
        );
        // The user's own function stays: it may be table-valued.
        assert!(
            got.labels.iter().any(|l| l == "dbo.CalcTotal"),
            "{db:?} {case:?} lost dbo.CalcTotal"
        );
    }
}

/// The engine the LSP and the browser build call directly agrees with the
/// service pipeline on the DDL slots.
#[test]
fn the_core_engine_owns_the_same_slots() {
    let complete = |case: &str| -> Vec<(String, CompletionKind)> {
        let cursor = case.find('|').unwrap();
        let sql = case.replacen('|', "", 1);
        sqlly_intel::intellisense::complete(&sql, cursor, &catalog())
            .into_iter()
            .map(|i| (i.label, i.kind))
            .collect()
    };
    for case in [
        "ALTER TABLE |",
        "TRUNCATE TABLE |",
        "CREATE INDEX ix ON |",
        "CREATE TABLE t (a int REFERENCES |",
    ] {
        let items = complete(case);
        assert!(!items.is_empty(), "{case:?} offered nothing");
        assert!(
            !items
                .iter()
                .any(|(_, kind)| *kind == CompletionKind::Function),
            "{case:?} offered functions: {items:?}"
        );
    }
    let labels = |case: &str| {
        complete(case)
            .into_iter()
            .map(|(l, _)| l)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        labels("CREATE PROC p @Id int AS BEGIN\n SELECT * FROM dbo.Orders WHERE OrderId = @|"),
        ["@Id"]
    );
    assert_eq!(labels("SELECT CASE WHEN 1=1 THEN 1 EN|")[0], "END");
    assert!(!labels("SELECT * FROM |").iter().any(|l| l == "ABS"));
    assert!(labels("SELECT * FROM |").iter().any(|l| l == "OPENJSON"));
}
