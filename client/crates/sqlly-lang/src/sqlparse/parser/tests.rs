use crate::sqllex::RecoveryKind;
use crate::sqlmodel::{Dialect, ObjectKind, ParamMode, Provenance, Span, TypeClass};

use crate::sqlparse::{
    CreateTableNode, LiteralShape, ParsedTree, StatementNode, UseKind, lower, parse,
};

#[test]
fn bare_columns_are_recorded_only_in_where_and_on() {
    let tree = parse(
        "SELECT name, total price FROM dbo.orders o JOIN dbo.users u ON userid = u.id WHERE status = 'x' AND LEN(city) > 2 ORDER BY happy",
        Dialect::SqlServer,
    );
    let bare: Vec<&str> = tree
        .references
        .iter()
        .filter(|r| r.use_kind == UseKind::ColumnBare)
        .map(|r| r.reference.name.spelling())
        .collect();
    // `name`, `total`, `price` are select-list (skipped); `happy` is
    // ORDER BY (skipped — could be an alias); `city` is a function
    // argument (skipped); `userid` (ON) and `status` (WHERE) qualify.
    assert_eq!(bare, vec!["userid", "status"], "{:?}", tree.references);
    assert!(
        tree.references
            .iter()
            .filter(|r| r.use_kind == UseKind::ColumnBare)
            .all(|r| r.in_predicate),
    );
}

#[test]
fn bare_columns_skip_subqueries_casts_and_registers() {
    let tree = parse(
        "SELECT 1 FROM t WHERE id IN (SELECT uid FROM t2) AND created < current_timestamp AND kind::int = 3",
        Dialect::Postgres,
    );
    let bare: Vec<&str> = tree
        .references
        .iter()
        .filter(|r| r.use_kind == UseKind::ColumnBare)
        .map(|r| r.reference.name.spelling())
        .collect();
    // `uid` sits in a subquery scope; `current_timestamp` is a register;
    // `int` follows a cast. `id`, `created`, and `kind` are real.
    assert_eq!(bare, vec!["id", "created", "kind"], "{:?}", tree.references);
}

#[test]
fn with_clause_harvests_ctes_and_body_tables() {
    let tree = parse(
        "WITH recent (id, ts) AS (SELECT o.id, o.created FROM dbo.orders o WHERE o.created > '2024'), tops AS (SELECT id FROM recent) SELECT r.id FROM recent r JOIN dbo.users u ON r.id = u.id",
        Dialect::SqlServer,
    );
    assert_eq!(tree.cte_definitions.len(), 2, "{:?}", tree.cte_definitions);
    assert_eq!(tree.cte_definitions[0].name, "recent");
    assert_eq!(tree.cte_definitions[0].columns, vec!["id", "ts"]);
    assert_eq!(tree.cte_definitions[1].name, "tops");
    // The CTE body's real table was harvested…
    assert!(
        tree.references.iter().any(|r| {
            r.use_kind == UseKind::TableSource && r.reference.name.spelling() == "orders"
        }),
        "{:?}",
        tree.references
    );
    // …and the main statement parsed as a Select with its sources.
    assert!(matches!(
        tree.statements.first(),
        Some(StatementNode::Select(_))
    ));
    assert!(
        tree.references.iter().any(|r| {
            r.use_kind == UseKind::TableSource && r.reference.name.spelling() == "users"
        })
    );
}

/// After UPDATE (a position where `(` is a column list, not a function
/// call), MySQL's `dual` in a multi-table list is still not a catalog table:
/// grounding it would flag it as an unknown table.
#[test]
fn mysql_dual_in_an_update_table_list_is_not_grounded() {
    let tree = parse(
        "UPDATE part, dual SET part.price = 1 WHERE part.id = 2",
        Dialect::MySql,
    );
    assert!(
        !tree
            .references
            .iter()
            .any(|r| r.reference.name.fold_key() == "dual"),
        "{:?}",
        tree.references
    );
    assert!(
        tree.references
            .iter()
            .any(|r| r.reference.name.fold_key() == "part"),
        "{:?}",
        tree.references
    );
}

/// MySQL's `FROM dual` is neither a table to ground nor an opaque source,
/// in a statement and inside a routine body alike.
#[test]
fn mysql_dual_is_neither_grounded_nor_opaque() {
    let tree = parse("SELECT 1 FROM dual d WHERE x = 1", Dialect::MySql);
    match tree.statements.first() {
        Some(StatementNode::Select(node)) => {
            assert!(node.derived_aliases.is_empty(), "{node:?}");
        }
        other => panic!("expected select, got {other:?}"),
    }
    assert!(
        !tree
            .references
            .iter()
            .any(|r| r.reference.name.fold_key() == "dual"),
        "{:?}",
        tree.references
    );

    let tree = parse(
        "CREATE PROCEDURE p() BEGIN SELECT 1 FROM dual; SELECT id FROM users u; END",
        Dialect::MySql,
    );
    assert!(
        !tree
            .references
            .iter()
            .any(|r| r.reference.name.fold_key() == "dual"),
        "{:?}",
        tree.references
    );
    assert!(
        tree.references.iter().any(|r| {
            r.use_kind == UseKind::TableSource && r.reference.name.spelling() == "users"
        })
    );
}

#[test]
fn derived_table_aliases_are_captured() {
    let tree = parse(
        "SELECT d.total FROM (SELECT SUM(amount) AS total FROM dbo.sales) AS d WHERE grand > 5",
        Dialect::SqlServer,
    );
    match tree.statements.first() {
        Some(StatementNode::Select(node)) => {
            assert_eq!(node.derived_aliases, vec!["d"], "{node:?}");
        }
        other => panic!("expected select, got {other:?}"),
    }
    // The derived body's table was harvested for reference tracking.
    assert!(
        tree.references.iter().any(|r| {
            r.use_kind == UseKind::TableSource && r.reference.name.spelling() == "sales"
        })
    );
}

#[test]
fn update_and_delete_predicates_record_columns() {
    let tree = parse(
        "UPDATE dbo.users SET active = 0 WHERE tenant_id = 7; DELETE FROM dbo.logs WHERE age > 90",
        Dialect::SqlServer,
    );
    let bare: Vec<(&str, usize)> = tree
        .references
        .iter()
        .filter(|r| r.use_kind == UseKind::ColumnBare)
        .map(|r| (r.reference.name.spelling(), r.statement_index))
        .collect();
    assert_eq!(
        bare,
        vec![("tenant_id", 0), ("age", 1)],
        "{:?}",
        tree.references
    );
}

#[test]
fn merge_in_routine_body_harvests_target_not_clause_keywords() {
    // MERGE's `WHEN MATCHED THEN UPDATE SET …` names no table after UPDATE;
    // harvesting must not record `SET` (or any clause keyword) as a table
    // source, which used to surface as a false "unknown table or view `SET`".
    let tree = parse(
        "ALTER PROCEDURE HR.pSync AS BEGIN \
         MERGE HR.EmployeeJobPosition AS Target \
         USING (SELECT MI.Id FROM @Items MI) AS Source \
         ON Target.Id = Source.Id \
         WHEN MATCHED THEN UPDATE SET IsActive = Source.IsActive \
         WHEN NOT MATCHED BY TARGET THEN INSERT (Id) VALUES (Source.Id) \
         WHEN NOT MATCHED BY SOURCE THEN DELETE \
         OUTPUT inserted.Id INTO @Out (Id); \
         END",
        Dialect::SqlServer,
    );
    let sources: Vec<&str> = tree
        .references
        .iter()
        .filter(|r| r.use_kind == UseKind::TableSource)
        .map(|r| r.reference.name.spelling())
        .collect();
    assert!(
        !sources
            .iter()
            .any(|s| s.eq_ignore_ascii_case("set") || s.eq_ignore_ascii_case("values")),
        "clause keyword harvested as a table: {sources:?}"
    );
    // The MERGE target itself is harvested (with its alias) so
    // `Target.`-completion can resolve.
    assert!(
        tree.references.iter().any(|r| {
            r.use_kind == UseKind::TableSource
                && r.reference.name.spelling() == "EmployeeJobPosition"
                && r.via_alias.as_deref() == Some("Target")
        }),
        "{:?}",
        tree.references
    );
}

#[test]
fn merge_into_target_is_harvested_once() {
    let tree = parse(
        "CREATE PROCEDURE dbo.p AS BEGIN \
         MERGE INTO dbo.t AS x USING dbo.s ON x.id = s.id \
         WHEN MATCHED THEN UPDATE SET n = s.n; \
         END",
        Dialect::SqlServer,
    );
    let t_sources = tree
        .references
        .iter()
        .filter(|r| r.use_kind == UseKind::TableSource && r.reference.name.spelling() == "t")
        .count();
    assert_eq!(t_sources, 1, "{:?}", tree.references);
}

/// Table-source harvesting across dialects and statement variants: each case
/// asserts the exact set of grounded table sources. The false-positive side
/// matters most — every name harvested here is checked against the catalog
/// and flagged "unknown table or view" when absent, so a keyword, function
/// name, cursor, or column mistaken for a table becomes a false error in the
/// editor.
#[test]
fn table_source_harvest_matrix() {
    use Dialect::*;
    fn sources(sql: &str, dialect: Dialect) -> Vec<String> {
        parse(sql, dialect)
            .references
            .iter()
            .filter(|r| r.use_kind == UseKind::TableSource)
            .map(|r| r.reference.display())
            .collect()
    }
    let cases: &[(&str, Dialect, &[&str])] = &[
        // Scalar functions carrying a FROM keyword in their argument list.
        (
            "SELECT EXTRACT(YEAR FROM created_at) FROM orders",
            Postgres,
            &["orders"],
        ),
        (
            "SELECT EXTRACT(YEAR FROM created_at) FROM orders",
            MySql,
            &["orders"],
        ),
        (
            "SELECT TRIM(LEADING 'x' FROM name) FROM users",
            Postgres,
            &["users"],
        ),
        (
            "SELECT SUBSTRING(name FROM 2 FOR 3) FROM users",
            Postgres,
            &["users"],
        ),
        (
            "SELECT id FROM orders WHERE EXTRACT(YEAR FROM created_at) = 2024",
            Postgres,
            &["orders"],
        ),
        // Table-valued / set-returning functions are not catalog tables.
        ("SELECT value FROM STRING_SPLIT(@csv, ',')", SqlServer, &[]),
        ("SELECT * FROM OPENJSON(@j) WITH (id INT)", SqlServer, &[]),
        ("SELECT * FROM dbo.fnGetItems(3) i", SqlServer, &[]),
        ("SELECT * FROM generate_series(1, 10) g", Postgres, &[]),
        ("SELECT * FROM unnest(ARRAY[1,2]) AS u(x)", Postgres, &[]),
        ("SELECT * FROM json_each(?1)", Sqlite, &[]),
        ("SELECT * FROM pragma_table_info('users')", Sqlite, &[]),
        (
            "SELECT * FROM dbo.orders o CROSS APPLY STRING_SPLIT(o.tags, ',') s",
            SqlServer,
            &["dbo.orders"],
        ),
        // A T-SQL legacy hint is not an argument list.
        (
            "SELECT * FROM dbo.t (NOLOCK) WHERE a = 1",
            SqlServer,
            &["dbo.t"],
        ),
        // LATERAL / ONLY are modifiers; DUAL is MySQL's dummy table.
        (
            "SELECT * FROM a JOIN LATERAL (SELECT 1) x ON true",
            Postgres,
            &["a"],
        ),
        ("SELECT * FROM ONLY parent_tbl", Postgres, &["parent_tbl"]),
        ("SELECT 1 FROM DUAL", MySql, &[]),
        // Locking/temporal/hint clauses don't produce phantom aliases/tables.
        ("SELECT * FROM t FOR UPDATE OF t NOWAIT", Postgres, &["t"]),
        ("SELECT * FROM t FOR UPDATE SKIP LOCKED", MySql, &["t"]),
        (
            "SELECT * FROM dbo.t FOR SYSTEM_TIME AS OF '2024-01-01'",
            SqlServer,
            &["dbo.t"],
        ),
        (
            "SELECT * FROM big TABLESAMPLE SYSTEM (10)",
            Postgres,
            &["big"],
        ),
        // UPDATE variants.
        (
            "UPDATE STATISTICS dbo.orders WITH FULLSCAN",
            SqlServer,
            &["dbo.orders"],
        ),
        ("UPDATE ONLY parent SET a = 1", Postgres, &["parent"]),
        // INSERT variants: SELECT sources ground; upsert tails stay quiet.
        (
            "INSERT INTO dbo.dst (a) SELECT a FROM dbo.src WHERE x = 1",
            SqlServer,
            &["dbo.dst", "dbo.src"],
        ),
        (
            "INSERT INTO t (a) VALUES (1) ON DUPLICATE KEY UPDATE a = a + 1",
            MySql,
            &["t"],
        ),
        (
            "INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO UPDATE SET a = excluded.a",
            Postgres,
            &["t"],
        ),
        ("REPLACE INTO t (a) VALUES (1)", MySql, &["t"]),
        // DELETE variants.
        ("DELETE FROM ONLY parent WHERE a = 1", Postgres, &["parent"]),
        // Top-level MERGE grounds its target and USING source.
        (
            "MERGE dbo.t AS x USING dbo.s ON x.id = s.id WHEN MATCHED THEN UPDATE SET n = s.n WHEN NOT MATCHED THEN INSERT (a) VALUES (s.a);",
            SqlServer,
            &["dbo.t", "dbo.s"],
        ),
    ];
    for (sql, dialect, expected) in cases {
        let got = sources(sql, *dialect);
        assert_eq!(&got, expected, "{dialect:?}: {sql}");
    }
}

/// The same guarantees inside tolerantly-harvested routine bodies, where a
/// keyword can appear far from the statement that gives it meaning.
#[test]
fn routine_body_harvest_matrix() {
    use Dialect::*;
    fn sources(sql: &str, dialect: Dialect) -> Vec<String> {
        parse(sql, dialect)
            .references
            .iter()
            .filter(|r| r.use_kind == UseKind::TableSource)
            .map(|r| r.reference.display())
            .collect()
    }
    let cases: &[(&str, Dialect, &[&str])] = &[
        // ON DUPLICATE KEY UPDATE assigns columns; the INSERT target grounds.
        (
            "CREATE PROCEDURE p() BEGIN INSERT INTO t (a) VALUES (1) ON DUPLICATE KEY UPDATE a = a + 1; END",
            MySql,
            &["t"],
        ),
        // FOR UPDATE [OF …/SKIP LOCKED] is a locking clause.
        (
            "CREATE PROCEDURE p() BEGIN ATOMIC SELECT * FROM t FOR UPDATE OF t NOWAIT; END",
            Postgres,
            &["t"],
        ),
        (
            "CREATE PROCEDURE p() BEGIN SELECT * FROM t FOR UPDATE SKIP LOCKED; END",
            MySql,
            &["t"],
        ),
        // FK actions and column defaults, not tables.
        (
            "CREATE PROCEDURE p() BEGIN CREATE TABLE c (id INT REFERENCES par(id) ON UPDATE CASCADE ON DELETE CASCADE); END",
            MySql,
            &[],
        ),
        (
            "CREATE PROCEDURE p() BEGIN ALTER TABLE t MODIFY ts DATETIME ON UPDATE CURRENT_TIMESTAMP; END",
            MySql,
            &[],
        ),
        // UPDATE STATISTICS grounds the table, not `STATISTICS`.
        (
            "CREATE PROCEDURE dbo.p AS BEGIN UPDATE STATISTICS dbo.orders; END",
            SqlServer,
            &["dbo.orders"],
        ),
        // FETCH … FROM names a cursor.
        (
            "CREATE PROCEDURE dbo.p AS BEGIN DECLARE c CURSOR FOR SELECT id FROM dbo.t; OPEN c; FETCH NEXT FROM c INTO @id; CLOSE c; END",
            SqlServer,
            &["dbo.t"],
        ),
        // INTO OUTFILE writes a file.
        (
            "CREATE PROCEDURE p() BEGIN SELECT a FROM t INTO OUTFILE '/tmp/f'; END",
            MySql,
            &["t"],
        ),
        // USING is only a source in MERGE/DELETE, not in index DDL or casts.
        (
            "CREATE PROCEDURE p() BEGIN CREATE INDEX idx ON t (a) USING BTREE; END",
            MySql,
            &[],
        ),
        (
            "CREATE PROCEDURE p() BEGIN ATOMIC ALTER TABLE t ALTER COLUMN a TYPE int USING a::int; END",
            Postgres,
            &[],
        ),
        // INSERT … SELECT inside a body grounds both ends.
        (
            "CREATE PROCEDURE dbo.p AS BEGIN INSERT INTO dbo.dst (a) SELECT a FROM dbo.src; END",
            SqlServer,
            &["dbo.dst", "dbo.src"],
        ),
    ];
    for (sql, dialect, expected) in cases {
        let got = sources(sql, *dialect);
        assert_eq!(&got, expected, "{dialect:?}: {sql}");
    }
}

#[test]
fn hostile_repeated_with_stays_iterative_and_terminates() {
    let bomb = "WITH ".repeat(5_000) + "SELECT 1";
    let tree = parse(&bomb, Dialect::SqlServer);
    assert!(!tree.statements.is_empty());
}

fn d() -> Dialect {
    Dialect::SqlServer
}

fn first_table(tree: &ParsedTree) -> &CreateTableNode {
    match &tree.statements[0] {
        StatementNode::CreateTable(t) => t,
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

#[test]
fn create_table_well_formed() {
    let sql = "CREATE TABLE dbo.Person (\n  Id int NOT NULL PRIMARY KEY IDENTITY(1,1),\n  Name nvarchar(100) NOT NULL,\n  Age int NULL,\n  ModelId int NOT NULL REFERENCES dbo.Model(Id),\n  CONSTRAINT UQ_Name UNIQUE (Name)\n)";
    let tree = parse(sql, d());
    assert!(
        tree.recovery.is_empty(),
        "no recovery on valid input: {:?}",
        tree.recovery
    );
    let t = first_table(&tree);
    assert_eq!(t.reference.display(), "dbo.Person");
    assert_eq!(t.body.columns.len(), 4);
    let id = &t.body.columns[0];
    assert_eq!(id.name.spelling(), "Id");
    assert_eq!(id.nullable, Some(false));
    assert!(id.identity);
    assert_eq!(id.ty.as_ref().unwrap().class, TypeClass::Integer);
    assert_eq!(
        t.body.columns[1].ty.as_ref().unwrap().params.length,
        Some(100)
    );
    assert_eq!(t.body.columns[2].nullable, Some(true));
    // PK from inline modifier; unique from table constraint.
    assert_eq!(
        t.body.primary_key.as_ref().unwrap().columns[0].spelling(),
        "Id"
    );
    assert_eq!(t.body.unique_keys.len(), 1);
    // FK from inline REFERENCES.
    assert_eq!(t.body.foreign_keys.len(), 1);
    assert_eq!(t.body.foreign_keys[0].target.display(), "dbo.Model");
    assert_eq!(t.body.foreign_keys[0].target_columns[0].spelling(), "Id");
    // FK target recorded as a reference.
    assert!(
        tree.references.iter().any(
            |u| u.use_kind == UseKind::ForeignKeyTarget && u.reference.display() == "dbo.Model"
        )
    );
}

#[test]
fn recovery_missing_commas_newline_separated() {
    // The canonical ADR-011 case: no commas between column definitions.
    let sql = "CREATE TABLE T (\n  Id int NOT NULL\n  Name varchar(50) NULL\n  Age int\n)";
    let tree = parse(sql, d());
    let t = first_table(&tree);
    assert_eq!(t.body.columns.len(), 3, "columns: {:?}", t.body.columns);
    assert_eq!(t.body.columns[0].name.spelling(), "Id");
    assert_eq!(t.body.columns[0].nullable, Some(false));
    assert_eq!(t.body.columns[1].name.spelling(), "Name");
    assert_eq!(
        t.body.columns[1].ty.as_ref().unwrap().params.length,
        Some(50)
    );
    assert_eq!(t.body.columns[1].nullable, Some(true));
    assert_eq!(t.body.columns[2].name.spelling(), "Age");
    // Recovery recorded as assumed commas.
    let assumed = tree
        .recovery
        .iter()
        .filter(|r| r.kind == RecoveryKind::AssumedComma)
        .count();
    assert_eq!(assumed, 2);
    assert!(!tree.fidelity().is_exact());
}

#[test]
fn recovery_column_missing_type_still_named() {
    let sql = "CREATE TABLE T (\n  Id int,\n  Mystery\n)";
    let tree = parse(sql, d());
    let t = first_table(&tree);
    assert_eq!(t.body.columns.len(), 2);
    assert_eq!(t.body.columns[1].name.spelling(), "Mystery");
    assert!(t.body.columns[1].ty.is_none());
    assert!(
        tree.recovery
            .iter()
            .any(|r| r.kind == RecoveryKind::MissingType)
    );
}

#[test]
fn recovery_garbage_goes_to_grab_bag() {
    let sql = "CREATE TABLE T (\n  Id int,\n  %% ?? !!,\n  Name varchar(10)\n)";
    let tree = parse(sql, d());
    let t = first_table(&tree);
    // Good columns still parsed around the garbage.
    let names: Vec<&str> = t.body.columns.iter().map(|c| c.name.spelling()).collect();
    assert!(names.contains(&"Id"), "{names:?}");
    assert!(names.contains(&"Name"), "{names:?}");
    assert!(!t.grab_bag.is_empty(), "garbage preserved as grab-bag");
}

#[test]
fn unbalanced_paren_closed_at_eof() {
    let sql = "CREATE TABLE T (\n  Id int";
    let tree = parse(sql, d());
    let t = first_table(&tree);
    assert_eq!(t.body.columns.len(), 1);
    assert!(
        tree.recovery
            .iter()
            .any(|r| r.kind == RecoveryKind::UnbalancedParenClosed)
    );
}

#[test]
fn create_procedure_tsql_params() {
    let sql = "CREATE PROCEDURE dbo.ImportAccounts @BatchId int, @Name nvarchar(50) = N'x', @Count int OUTPUT AS BEGIN SELECT 1 END";
    let tree = parse(sql, d());
    match &tree.statements[0] {
        StatementNode::CreateRoutine(r) => {
            assert_eq!(r.kind, ObjectKind::Procedure);
            assert_eq!(r.reference.display(), "dbo.ImportAccounts");
            assert_eq!(r.body.params.len(), 3, "{:?}", r.body.params);
            assert_eq!(r.body.params[0].name.spelling(), "@BatchId");
            assert_eq!(
                r.body.params[0].ty.as_ref().unwrap().class,
                TypeClass::Integer
            );
            assert!(r.body.params[1].default.is_some());
            assert_eq!(r.body.params[2].mode, ParamMode::Out);
        }
        other => panic!("expected routine, got {other:?}"),
    }
}

#[test]
fn alter_procedure_harvests_body_table_sources() {
    // Repro: cursor at `t.` in a JOIN ON clause should resolve `t` to
    // [Shared].[Contact]. That requires ALTER PROCEDURE bodies to harvest
    // FROM/JOIN table sources the same way CREATE PROCEDURE does.
    let sql = "ALTER PROCEDURE [Shared].[pContact_ReferenceList] \
            (@TenantId smallint) AS BEGIN \
            SELECT t.ContactId FROM [Shared].[Contact] t \
            INNER JOIN Shared.ContactPostalAddress cpa ON cpa.ContactId = t. \
            END";
    let tree = parse(sql, d());
    let t_source = tree
        .references
        .iter()
        .find(|u| u.use_kind == UseKind::TableSource && u.via_alias.as_deref() == Some("t"))
        .unwrap_or_else(|| {
            panic!(
                "alias `t` table source not harvested from ALTER PROCEDURE body: {:?}",
                tree.references
            )
        });
    assert_eq!(t_source.reference.display(), "Shared.Contact");
}

#[test]
fn alter_table_stays_opaque() {
    // ALTER TABLE is a mutation, not a definition: it must not route
    // through parse_create. It stays Opaque with an "alter table" head.
    let sql = "ALTER TABLE dbo.T ADD x int";
    let tree = parse(sql, d());
    match &tree.statements[0] {
        StatementNode::Opaque { head, .. } => assert_eq!(head, "alter table"),
        other => panic!("expected Opaque, got {other:?}"),
    }
}

#[test]
fn alter_view_harvests_body_table_sources() {
    // ALTER VIEW bodies must harvest FROM/JOIN table sources + aliases
    // the same way CREATE VIEW does.
    let sql = "ALTER VIEW Shared.vContacts AS \
            SELECT c.ContactId FROM [Shared].[Contact] c \
            INNER JOIN Shared.ContactPostalAddress cpa ON cpa.ContactId = c.ContactId";
    let tree = parse(sql, d());
    let c_source = tree
        .references
        .iter()
        .find(|u| u.use_kind == UseKind::TableSource && u.via_alias.as_deref() == Some("c"))
        .unwrap_or_else(|| {
            panic!(
                "alias `c` table source not harvested from ALTER VIEW body: {:?}",
                tree.references
            )
        });
    assert_eq!(c_source.reference.display(), "Shared.Contact");
}

#[test]
fn alter_function_harvests_body_table_sources() {
    // ALTER FUNCTION bodies must harvest FROM/JOIN table sources + aliases
    // the same way CREATE FUNCTION / ALTER PROCEDURE do.
    let sql = "ALTER FUNCTION Shared.fContactList (@TenantId smallint) \
            RETURNS TABLE AS RETURN \
            SELECT t.ContactId FROM [Shared].[Contact] t \
            INNER JOIN Shared.ContactPostalAddress cpa ON cpa.ContactId = t.ContactId";
    let tree = parse(sql, d());
    let t_source = tree
        .references
        .iter()
        .find(|u| u.use_kind == UseKind::TableSource && u.via_alias.as_deref() == Some("t"))
        .unwrap_or_else(|| {
            panic!(
                "alias `t` table source not harvested from ALTER FUNCTION body: {:?}",
                tree.references
            )
        });
    assert_eq!(t_source.reference.display(), "Shared.Contact");
}

#[test]
fn create_function_postgres() {
    let sql = "CREATE FUNCTION add_em(a integer, b integer) RETURNS integer LANGUAGE sql";
    let tree = parse(sql, Dialect::Postgres);
    match &tree.statements[0] {
        StatementNode::CreateRoutine(r) => {
            assert_eq!(r.kind, ObjectKind::Function);
            assert_eq!(r.body.params.len(), 2, "{:?}", r.body.params);
            assert_eq!(r.body.returns.as_ref().unwrap().class, TypeClass::Integer);
            assert_eq!(r.body.language.as_deref(), Some("sql"));
        }
        other => panic!("expected routine, got {other:?}"),
    }
}

#[test]
fn select_from_with_alias() {
    let sql = "select * from Person p where p.name = 'x'";
    let tree = parse(sql, d());
    match &tree.statements[0] {
        StatementNode::Select(q) => {
            assert_eq!(q.tables.len(), 1);
            assert_eq!(q.tables[0].reference.display(), "Person");
            assert_eq!(q.tables[0].alias.as_deref(), Some("p"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn insert_columns_and_arity() {
    let sql = "INSERT INTO dbo.T (A, B, C) VALUES (1, 'x', 3), (4, 'y')";
    let tree = parse(sql, d());
    match &tree.statements[0] {
        StatementNode::Insert(i) => {
            assert_eq!(i.columns.len(), 3);
            assert_eq!(i.value_row_arities, vec![3, 2]);
            assert_eq!(i.target.reference.display(), "dbo.T");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn create_function_kinds_by_return_shape() {
    // Inline TVF: RETURNS TABLE.
    let tree = parse(
        "CREATE FUNCTION dbo.fList() RETURNS TABLE AS RETURN SELECT 1 AS x",
        d(),
    );
    match &tree.statements[0] {
        StatementNode::CreateRoutine(r) => assert_eq!(r.kind, ObjectKind::TableFunction),
        other => panic!("{other:?}"),
    }
    // Multi-statement TVF: RETURNS @t TABLE (…).
    let tree = parse(
        "CREATE FUNCTION dbo.fMulti() RETURNS @t TABLE (x int) AS BEGIN RETURN END",
        d(),
    );
    match &tree.statements[0] {
        StatementNode::CreateRoutine(r) => assert_eq!(r.kind, ObjectKind::TableFunction),
        other => panic!("{other:?}"),
    }
    // Postgres SETOF is table-shaped too.
    let tree = parse(
        "CREATE FUNCTION fRows() RETURNS SETOF users AS $$ SELECT * FROM users $$ LANGUAGE sql",
        Dialect::Postgres,
    );
    match &tree.statements[0] {
        StatementNode::CreateRoutine(r) => assert_eq!(r.kind, ObjectKind::TableFunction),
        other => panic!("{other:?}"),
    }
    // Scalar return keeps the generic Function kind (subtype unknown).
    let tree = parse(
        "CREATE FUNCTION dbo.fOne() RETURNS int AS BEGIN RETURN 1 END",
        d(),
    );
    match &tree.statements[0] {
        StatementNode::CreateRoutine(r) => assert_eq!(r.kind, ObjectKind::Function),
        other => panic!("{other:?}"),
    }
}

#[test]
fn exec_named_and_positional_args() {
    let sql = "EXEC dbo.ImportAccounts @BatchId = 'abc', 42";
    let tree = parse(sql, d());
    match &tree.statements[0] {
        StatementNode::Exec(e) => {
            assert_eq!(e.target.display(), "dbo.ImportAccounts");
            assert_eq!(e.args.len(), 2, "{:?}", e.args);
            assert_eq!(e.args[0].0.as_deref(), Some("@BatchId"));
            assert!(matches!(e.args[0].1.shape, LiteralShape::String { .. }));
            assert_eq!(e.args[1].0, None);
            assert_eq!(e.args[1].1.shape, LiteralShape::Integer);
        }
        other => panic!("{other:?}"),
    }
    // RoutineCall reference recorded.
    assert!(
        tree.references
            .iter()
            .any(|u| u.use_kind == UseKind::RoutineCall)
    );
}

#[test]
fn negative_number_arg() {
    let sql = "EXEC dbo.P @x = -5";
    let tree = parse(sql, d());
    match &tree.statements[0] {
        StatementNode::Exec(e) => {
            assert_eq!(e.args[0].1.text, "-5");
            assert_eq!(e.args[0].1.shape, LiteralShape::Integer);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn lowering_produces_fragments() {
    let sql = "CREATE TABLE dbo.A (Id int);\nCREATE VIEW dbo.V AS SELECT Id FROM dbo.A";
    let tree = parse(sql, d());
    let frags = lower(
        &tree,
        Provenance::File {
            path: "a.sql".into(),
            content_hash: 1,
            span: Span::new(0, 0),
        },
    );
    assert_eq!(frags.len(), 2);
    assert_eq!(frags[0].kind, ObjectKind::Table);
    assert_eq!(frags[1].kind, ObjectKind::View);
    // View references its source table.
    assert!(frags[1].references.iter().any(|r| r.display() == "dbo.A"));
}

fn lowered_table(sql: &str, dialect: Dialect) -> (String, String, Vec<String>) {
    let frags = lower(
        &parse(sql, dialect),
        Provenance::File {
            path: "a.sql".into(),
            content_hash: 1,
            span: Span::new(0, 0),
        },
    );
    assert_eq!(frags.len(), 1, "{sql}");
    let frag = &frags[0];
    let crate::sqlmodel::ObjectBody::Table(body) = &frag.body else {
        panic!("not a table: {:?}", frag.body);
    };
    (
        frag.reference
            .namespace
            .as_ref()
            .map(|n| n.spelling().to_string())
            .unwrap_or_default(),
        frag.reference.name.spelling().to_string(),
        body.columns
            .iter()
            .map(|c| c.name.spelling().to_string())
            .collect(),
    )
}

#[test]
fn lowering_unescapes_doubled_bracket_quotes() {
    let (schema, name, cols) = lowered_table(
        "CREATE TABLE [s]]x].[a]]b] ([c]]1] int, [plain] int)",
        Dialect::SqlServer,
    );
    assert_eq!(schema, "s]x");
    assert_eq!(name, "a]b");
    assert_eq!(cols, vec!["c]1", "plain"]);
}

#[test]
fn lowering_unescapes_doubled_double_quotes() {
    let (schema, name, cols) = lowered_table(
        r#"CREATE TABLE "s""x"."a""b" ("c""1" int, "plain" int)"#,
        Dialect::Postgres,
    );
    assert_eq!(schema, r#"s"x"#);
    assert_eq!(name, r#"a"b"#);
    assert_eq!(cols, vec![r#"c"1"#, "plain"]);
}

#[test]
fn lowering_unescapes_doubled_backticks() {
    let (_, name, cols) = lowered_table(
        "CREATE TABLE `a``b` (`c``1` int, `plain` int)",
        Dialect::MySql,
    );
    assert_eq!(name, "a`b");
    assert_eq!(cols, vec!["c`1", "plain"]);
}

#[test]
fn escaped_quotes_resolve_to_the_same_name_as_the_catalog_spelling() {
    // A reference written with an escaped bracket must fold to the same key
    // as the real object name, or lookups against the catalog miss.
    let tree = parse("SELECT * FROM dbo.[a]]b]", Dialect::SqlServer);
    let r = tree
        .references
        .iter()
        .find(|u| u.use_kind == UseKind::TableSource)
        .expect("table reference");
    assert_eq!(r.reference.name.spelling(), "a]b");
    assert!(r.reference.name.matches(&crate::sqlmodel::SqlName::new(
        "a]b",
        Dialect::SqlServer,
        true
    )));
}

#[test]
fn parser_is_panic_free_on_junk() {
    let junk = [
        "",
        ";;;",
        "((((",
        "create",
        "create table",
        "create table (",
        "select from where",
        "exec",
        "insert into",
        "update set",
        "'",
        "\u{0}\u{1}",
        "create table T (a b c d e f g)",
        "1 2 3 4 5",
        "create table T (,,,,)",
    ];
    for j in junk {
        for dialect in Dialect::ALL {
            let _ = parse(j, dialect);
        }
    }
}

#[test]
fn mysql_backtick_table() {
    let sql = "CREATE TABLE `order` (`id` int auto_increment, primary key (`id`)) ENGINE=InnoDB";
    let tree = parse(sql, Dialect::MySql);
    let t = first_table(&tree);
    assert_eq!(t.reference.name.spelling(), "order");
    assert!(t.body.columns[0].identity);
    assert_eq!(
        t.body.primary_key.as_ref().unwrap().columns[0].spelling(),
        "id"
    );
}

#[test]
fn oracle_number_types() {
    let sql = "CREATE TABLE T (ID NUMBER(10), PRICE NUMBER(10,2), NOTE VARCHAR2(400))";
    let tree = parse(sql, Dialect::Oracle);
    let t = first_table(&tree);
    assert_eq!(
        t.body.columns[0].ty.as_ref().unwrap().class,
        TypeClass::Integer
    );
    assert_eq!(
        t.body.columns[1].ty.as_ref().unwrap().class,
        TypeClass::Decimal
    );
    assert_eq!(
        t.body.columns[2].ty.as_ref().unwrap().class,
        TypeClass::Character
    );
}
