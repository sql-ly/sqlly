//! Pure dump rendering: options, ordering, and SQL text generation (no I/O).

use crate::scripting::dependency::{DependencyOrder, ScriptObject, topological_order};
use chrono::{DateTime, Utc};
use sqlly_core::{CellValue, ObjectKind};
use sqlly_lang::sqllex::{CommentShape, StrictLexer, TokenKind};
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType, Dialect};

use crate::metadata::ForeignKeyRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DumpContent {
    StructureOnly,
    DataOnly,
    StructureAndData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DumpLayout {
    SingleFile,
    FilePerObject,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DumpOptions {
    pub content: DumpContent,
    pub layout: DumpLayout,
    pub drop_if_exists: bool,
    pub include_views: bool,
    pub include_routines: bool,
    pub include_sequences: bool,
    pub include_triggers: bool,
    pub batch_size: usize,
    pub schemas: Option<Vec<String>>,
    /// Qualified object labels (`schema.name`) the dump is scoped to, e.g. a
    /// Deploy Cart group. `None` dumps every enumerated object.
    #[serde(default)]
    pub objects: Option<Vec<String>>,
    /// The subset of `objects` whose table data is written. `None` writes data
    /// for every in-scope table; `Some` restricts INSERTs to the listed tables
    /// while structure still covers the whole scope.
    #[serde(default)]
    pub data_objects: Option<Vec<String>>,
    /// An explicit structure order: qualified labels (`schema.name`) in the
    /// order their CREATE statements (and, per object, their numbered
    /// files) are written. A Deploy Cart passes its dependency order here
    /// so per-object files match the single script. In-scope objects the
    /// list does not name follow in the pipeline's own order. `None` keeps
    /// the pipeline's order (tables by foreign key, then views and routines
    /// by name).
    #[serde(default)]
    pub structure_order: Option<Vec<String>>,
    /// PostgreSQL: follow each table's, view's, and routine's CREATE with the
    /// GRANT / REVOKE statements that recreate its privileges (including
    /// REVOKEs of PostgreSQL's implicit defaults). Off by default, as the
    /// grantee roles may not exist where the dump is restored.
    #[serde(default)]
    pub include_privileges: bool,
}

impl Default for DumpOptions {
    fn default() -> Self {
        Self {
            content: DumpContent::StructureAndData,
            layout: DumpLayout::SingleFile,
            drop_if_exists: false,
            include_views: true,
            include_routines: true,
            include_sequences: true,
            include_triggers: true,
            batch_size: 500,
            schemas: None,
            objects: None,
            data_objects: None,
            structure_order: None,
            include_privileges: false,
        }
    }
}

impl DumpOptions {
    pub fn effective_batch_size(&self, db: DatabaseType) -> usize {
        let clamped = self.batch_size.clamp(1, 5000);
        if db.connection_kind() == ConnectionKind::Tds {
            clamped.min(1000)
        } else {
            clamped
        }
    }
}

pub fn dump_header(
    db: DatabaseType,
    database: &str,
    generated_at: DateTime<Utc>,
    opts: &DumpOptions,
) -> String {
    let content = match opts.content {
        DumpContent::StructureOnly => "structure only",
        DumpContent::DataOnly => "data only",
        DumpContent::StructureAndData => "structure and data",
    };
    let layout = match opts.layout {
        DumpLayout::SingleFile => "single file",
        DumpLayout::FilePerObject => "one file per object",
    };
    format!(
        "-- SQLLY database dump\n\
         -- Version: {}\n\
         -- Engine: {:?}\n\
         -- Database: {database}\n\
         -- Generated: {}\n\
         -- Content: {content}\n\
         -- Layout: {layout}\n\
         -- Drop if exists: {}\n\
         -- Include views: {}\n\
         -- Include routines: {}\n\
         -- Include triggers: {}\n\
         -- Include sequences: {}\n\
         -- Batch size: {}\n",
        env!("CARGO_PKG_VERSION"),
        db,
        generated_at.format("%Y-%m-%dT%H:%M:%SZ"),
        opts.drop_if_exists,
        opts.include_views,
        opts.include_routines,
        opts.include_triggers,
        opts.include_sequences,
        opts.batch_size,
    ) + if opts.include_privileges {
        "-- Include privileges: true\n"
    } else {
        ""
    }
}

pub fn session_prelude(db: DatabaseType) -> String {
    match db.connection_kind() {
        ConnectionKind::Tds => "SET NOCOUNT ON;\n".into(),
        ConnectionKind::MySqlWire => "SET FOREIGN_KEY_CHECKS=0;\nSET NAMES utf8mb4;\n".into(),
        ConnectionKind::PostgresWire => {
            "SET client_encoding = 'UTF8';\nSET session_replication_role = replica;\n".into()
        }
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => {
            "PRAGMA foreign_keys=OFF;\nBEGIN TRANSACTION;\n".into()
        }
        // DuckDB has no switch to defer foreign-key checks; the dump writes
        // parents before children, so one transaction is all it needs.
        ConnectionKind::DuckDbFile => "BEGIN TRANSACTION;\n".into(),
        _ => String::new(),
    }
}

pub fn session_epilogue(db: DatabaseType) -> String {
    match db.connection_kind() {
        ConnectionKind::MySqlWire => "SET FOREIGN_KEY_CHECKS=1;\n".into(),
        ConnectionKind::PostgresWire => "SET session_replication_role = DEFAULT;\n".into(),
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp | ConnectionKind::DuckDbFile => {
            "COMMIT;\n".into()
        }
        _ => String::new(),
    }
}

pub fn batch_separator(db: DatabaseType) -> &'static str {
    if db.connection_kind() == ConnectionKind::Tds {
        "GO"
    } else {
        ""
    }
}

fn qualified(db: DatabaseType, schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(schema) => format!(
            "{}.{}",
            db.quote_identifier(schema),
            db.quote_identifier(name)
        ),
        None => db.quote_identifier(name),
    }
}

pub fn drop_statement(
    db: DatabaseType,
    kind: ObjectKind,
    schema: Option<&str>,
    name: &str,
) -> String {
    let q = qualified(db, schema, name);
    let stmt = match kind {
        ObjectKind::Table => format!("DROP TABLE IF EXISTS {q}"),
        ObjectKind::View => format!("DROP VIEW IF EXISTS {q}"),
        ObjectKind::StoredProcedure => format!("DROP PROCEDURE IF EXISTS {q}"),
        ObjectKind::ScalarFunction | ObjectKind::TableFunction | ObjectKind::AggregateFunction => {
            format!("DROP FUNCTION IF EXISTS {q}")
        }
        ObjectKind::Trigger => format!("DROP TRIGGER IF EXISTS {q}"),
        _ => return format!("-- DROP not supported for {kind:?} {q}"),
    };
    format!("{stmt};")
}

pub fn sql_literal_for(db: DatabaseType, value: &CellValue) -> String {
    match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => db.bool_literal(*b).to_string(),
        CellValue::I64(v) => v.to_string(),
        CellValue::F64(v) => {
            if v.is_finite() {
                v.to_string()
            } else {
                "NULL".to_string()
            }
        }
        CellValue::String(s) => db.string_literal(s),
        CellValue::Bytes(b) => db.bytes_literal(b),
    }
}

pub fn render_insert_batch(
    db: DatabaseType,
    schema: Option<&str>,
    table: &str,
    columns: &[String],
    rows: &[Vec<CellValue>],
) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let table_q = qualified(db, schema, table);
    let cols = columns
        .iter()
        .map(|c| db.quote_identifier(c))
        .collect::<Vec<_>>()
        .join(", ");
    let values = rows
        .iter()
        .map(|row| {
            let literals = row
                .iter()
                .map(|v| sql_literal_for(db, v))
                .collect::<Vec<_>>();
            format!("({})", literals.join(", "))
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO {table_q} ({cols}) VALUES {values};")
}

/// DuckDB has no `setval`; the one way to move a sequence past the ids a
/// restore loads is to draw from it once per row: `nextval` runs for each
/// element of `range(MAX(id))`, so a fresh sequence ends exactly at the
/// highest loaded id (an already-advanced one only gains gaps). The
/// sequence name is spliced in as a constant because DuckDB rejects a
/// non-constant `nextval` argument.
pub fn duckdb_sequence_advance_sql(
    db: DatabaseType,
    qualified_table: &str,
    column: &str,
    sequence: &str,
) -> String {
    format!(
        "SELECT nextval('{}') FROM range((SELECT COALESCE(MAX({}), 0) FROM {qualified_table}));",
        sequence.replace('\'', "''"),
        db.quote_identifier(column)
    )
}

pub fn identity_guard(db: DatabaseType, qualified: &str, on: bool) -> Option<String> {
    if db.connection_kind() != ConnectionKind::Tds {
        return None;
    }
    Some(if on {
        format!("SET IDENTITY_INSERT {qualified} ON;")
    } else {
        format!("SET IDENTITY_INSERT {qualified} OFF;")
    })
}

pub fn table_key(schema: &str, name: &str) -> String {
    if schema.is_empty() {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

pub fn order_tables(tables: &[(String, String)], fks: &[ForeignKeyRow]) -> DependencyOrder {
    let table_keys: std::collections::BTreeSet<String> = tables
        .iter()
        .map(|(schema, name)| table_key(schema, name))
        .collect();

    let mut deps_by_table: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        table_keys
            .iter()
            .map(|k| (k.clone(), std::collections::BTreeSet::new()))
            .collect();

    for fk in fks {
        let child = table_key(&fk.from_schema, &fk.from_table);
        let parent = table_key(&fk.to_schema, &fk.to_table);
        if table_keys.contains(&child) && table_keys.contains(&parent) && child != parent {
            deps_by_table.entry(child).or_default().insert(parent);
        }
    }

    let objects: Vec<ScriptObject> = deps_by_table
        .into_iter()
        .map(|(key, deps)| ScriptObject::new(key, deps.into_iter().collect()))
        .collect();

    topological_order(&objects)
}

/// Prepare one fetched object definition for a script that will be run back
/// through a client. MySQL/MariaDB procedures, functions, triggers, and
/// events carry `;`-terminated statements inside their bodies, so the
/// `mysql` client (and SQLly's own splitter) would cut them at the first
/// inner `;`: those are wrapped in `DELIMITER $$ … $$` / `DELIMITER ;`.
/// Every other object, and every other engine, is returned as is.
pub fn script_definition(db: DatabaseType, kind: &ObjectKind, body: &str) -> String {
    let programmable = matches!(
        kind,
        ObjectKind::StoredProcedure
            | ObjectKind::ScalarFunction
            | ObjectKind::TableFunction
            | ObjectKind::AggregateFunction
            | ObjectKind::Trigger
            | ObjectKind::Event
    );
    if db.connection_kind() != ConnectionKind::MySqlWire || !programmable {
        return body.to_string();
    }
    let statement = body.trim_end().trim_end_matches(';').trim_end();
    let delimiter = ["$$", "//", "~~"]
        .into_iter()
        .find(|d| !statement.contains(d))
        .unwrap_or("$$");
    // A body whose last line is a `-- note` or `# note` comment would
    // swallow a delimiter written on that same line, so it goes on its own.
    let gap = if ends_with_line_comment(Dialect::MySql, statement) {
        "\n"
    } else {
        ""
    };
    format!("DELIMITER {delimiter}\n{statement}{gap}{delimiter}\nDELIMITER ;\n")
}

/// Whether the last token of `text` (whitespace aside) is a `--` or, on
/// MySQL/MariaDB, `#` line comment — the case where anything appended on
/// that line (`;`, a `DELIMITER` tag) becomes part of the comment. Strings
/// and quoted identifiers are respected: a `--` inside a literal is code.
pub(crate) fn ends_with_line_comment(dialect: Dialect, text: &str) -> bool {
    StrictLexer::tokenize(text, dialect)
        .iter()
        .rev()
        .find(|t| t.kind != TokenKind::Whitespace)
        .is_some_and(|t| t.kind == TokenKind::Comment(CommentShape::Line))
}

pub fn write_separator(out: &mut String, db: DatabaseType) {
    let sep = batch_separator(db);
    if sep.is_empty() {
        out.push('\n');
    } else {
        out.push_str(sep);
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::ConnectionProfile;

    #[test]
    fn privileges_are_off_by_default_and_named_in_the_header_when_on() {
        let opts = DumpOptions::default();
        assert!(!opts.include_privileges);
        let older: DumpOptions = serde_json::from_value(serde_json::json!({
            "content": "StructureOnly",
            "layout": "SingleFile",
            "drop_if_exists": false,
            "include_views": true,
            "include_routines": true,
            "include_sequences": true,
            "include_triggers": true,
            "batch_size": 500,
            "schemas": null
        }))
        .unwrap();
        assert!(
            !older.include_privileges,
            "older saved dump settings stay off"
        );
        let at = chrono::Utc::now();
        assert!(!dump_header(DatabaseType::Postgres, "db", at, &opts).contains("privileges"));
        let on = DumpOptions {
            include_privileges: true,
            ..DumpOptions::default()
        };
        assert!(
            dump_header(DatabaseType::Postgres, "db", at, &on)
                .ends_with("-- Include privileges: true\n")
        );
    }

    #[test]
    fn mysql_trigger_definition_is_wrapped_in_delimiter() {
        let body = "CREATE TRIGGER `t_ai` AFTER INSERT ON `t` FOR EACH ROW\nBEGIN\n  SET @a = 1;\n  SET @b = 2;\nEND;";
        let out = script_definition(DatabaseType::MySql, &ObjectKind::Trigger, body);
        assert_eq!(
            out,
            "DELIMITER $$\nCREATE TRIGGER `t_ai` AFTER INSERT ON `t` FOR EACH ROW\nBEGIN\n  SET @a = 1;\n  SET @b = 2;\nEND$$\nDELIMITER ;\n"
        );
        let script = format!("{out}SELECT 1;");
        let statements =
            sqlly_lang::sqllex::split_statements(&script, sqlly_lang::sqlmodel::Dialect::MySql);
        let texts: Vec<&str> = statements.iter().map(|s| s.text.trim()).collect();
        assert_eq!(texts.len(), 2, "{texts:#?}");
        assert!(texts[0].ends_with("END"), "{texts:#?}");
    }

    /// A routine whose last line is a comment (MySQL keeps the body
    /// verbatim) used to get `$$` appended to that comment line, so the
    /// closing delimiter vanished and the next statement ran into the body.
    #[test]
    fn mysql_definition_ending_in_a_line_comment_keeps_its_delimiter() {
        for (body, comment) in [
            (
                "CREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\nEND -- note",
                "END -- note",
            ),
            (
                "CREATE PROCEDURE p()\nBEGIN\n  SELECT 1;\nEND # note;",
                "END # note",
            ),
        ] {
            let out = script_definition(DatabaseType::MySql, &ObjectKind::StoredProcedure, body);
            assert!(
                out.contains(&format!("{comment}\n$$\nDELIMITER ;\n")),
                "{out}"
            );
            let script = format!("{out}SELECT 1;");
            let statements =
                sqlly_lang::sqllex::split_statements(&script, sqlly_lang::sqlmodel::Dialect::MySql);
            let texts: Vec<&str> = statements.iter().map(|s| s.text.trim()).collect();
            assert_eq!(texts.len(), 2, "{texts:#?}");
            assert!(texts[0].ends_with(comment), "{texts:#?}");
            assert_eq!(texts[1], "SELECT 1");
        }
    }

    #[test]
    fn ends_with_line_comment_respects_dialect_and_strings() {
        assert!(ends_with_line_comment(Dialect::MySql, "SELECT 1 -- x"));
        assert!(ends_with_line_comment(Dialect::MySql, "SELECT 1 # x\n  "));
        assert!(ends_with_line_comment(Dialect::Sqlite, "SELECT 1 -- x"));
        assert!(!ends_with_line_comment(Dialect::Sqlite, "SELECT 1 # x"));
        assert!(!ends_with_line_comment(Dialect::MySql, "SELECT '-- x'"));
        assert!(!ends_with_line_comment(Dialect::MySql, "SELECT 1 /* x */"));
        assert!(!ends_with_line_comment(Dialect::MySql, "-- x\nSELECT 1"));
        assert!(!ends_with_line_comment(Dialect::MySql, ""));
    }

    #[test]
    fn script_definition_leaves_views_and_other_engines_alone() {
        let view = "CREATE OR REPLACE VIEW `v` AS select 1;";
        assert_eq!(
            script_definition(DatabaseType::MySql, &ObjectKind::View, view),
            view
        );
        let func = "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql;";
        assert_eq!(
            script_definition(DatabaseType::Postgres, &ObjectKind::ScalarFunction, func),
            func
        );
        // A body that already uses `$$` gets a different delimiter.
        let odd = "CREATE PROCEDURE p() BEGIN SELECT '$$'; END";
        let out = script_definition(DatabaseType::MariaDb, &ObjectKind::StoredProcedure, odd);
        assert!(out.starts_with("DELIMITER //\n"), "{out}");
        assert!(out.contains("END//\nDELIMITER ;"), "{out}");
    }

    fn sample_tables() -> Vec<(String, String)> {
        vec![
            ("dbo".into(), "customers".into()),
            ("dbo".into(), "products".into()),
            ("dbo".into(), "orders".into()),
            ("dbo".into(), "order_items".into()),
        ]
    }

    fn fk(from: &str, to: &str) -> ForeignKeyRow {
        let (fs, ft) = from.split_once('.').unwrap();
        let (ts, tt) = to.split_once('.').unwrap();
        ForeignKeyRow {
            constraint_name: format!("fk_{from}_{to}"),
            from_schema: fs.into(),
            from_table: ft.into(),
            from_column: "id".into(),
            to_schema: ts.into(),
            to_table: tt.into(),
            to_column: "id".into(),
        }
    }

    #[test]
    fn orders_tables_by_foreign_keys() {
        let tables = sample_tables();
        let fks = vec![
            fk("dbo.orders", "dbo.customers"),
            fk("dbo.order_items", "dbo.orders"),
            fk("dbo.order_items", "dbo.products"),
        ];
        let order = order_tables(&tables, &fks);
        assert!(order.warnings.is_empty());
        assert_eq!(
            order.order,
            vec![
                "dbo.customers".to_string(),
                "dbo.orders".to_string(),
                "dbo.products".to_string(),
                "dbo.order_items".to_string(),
            ]
        );
    }

    #[test]
    fn cyclic_foreign_keys_warn_and_fall_back() {
        let tables = vec![("dbo".into(), "a".into()), ("dbo".into(), "b".into())];
        let fks = vec![fk("dbo.a", "dbo.b"), fk("dbo.b", "dbo.a")];
        let order = order_tables(&tables, &fks);
        assert_eq!(order.order.len(), 2);
        assert_eq!(order.warnings.len(), 1);
        assert!(order.warnings[0].contains("cycle"));
    }

    #[test]
    fn insert_batch_respects_batch_size() {
        let cols = vec!["id".into()];
        let rows: Vec<Vec<CellValue>> = (0..1250).map(|i| vec![CellValue::I64(i)]).collect();
        let batch = 500;
        let mut statements = Vec::new();
        for chunk in rows.chunks(batch) {
            statements.push(render_insert_batch(
                DatabaseType::Postgres,
                Some("dbo"),
                "t",
                &cols,
                chunk,
            ));
        }
        assert_eq!(statements.len(), 3);
        for stmt in &statements {
            assert!(stmt.starts_with("INSERT INTO"));
            assert!(stmt.ends_with(';'));
        }
        assert_eq!(statements[0].matches("VALUES").count(), 1);
    }

    #[test]
    fn sql_server_batch_is_clamped_to_1000() {
        let opts = DumpOptions {
            batch_size: 5000,
            ..Default::default()
        };
        assert_eq!(opts.effective_batch_size(DatabaseType::SqlServer), 1000);
        assert_eq!(opts.effective_batch_size(DatabaseType::Postgres), 5000);
    }

    #[test]
    fn literals_match_engine() {
        let s = CellValue::String("O'Brien".into());
        assert_eq!(
            sql_literal_for(DatabaseType::SqlServer, &s),
            DatabaseType::SqlServer.string_literal("O'Brien")
        );
        assert_eq!(
            sql_literal_for(DatabaseType::Postgres, &s),
            DatabaseType::Postgres.string_literal("O'Brien")
        );
        let t = CellValue::Bool(true);
        assert_eq!(sql_literal_for(DatabaseType::SqlServer, &t), "1");
        assert_eq!(sql_literal_for(DatabaseType::Postgres, &t), "TRUE");
        let bytes = CellValue::Bytes(vec![0xDE, 0xAD]);
        assert_eq!(
            sql_literal_for(DatabaseType::SqlServer, &bytes),
            DatabaseType::SqlServer.bytes_literal(&[0xDE, 0xAD])
        );
    }

    #[test]
    fn duckdb_session_wrapping_and_sequence_advance() {
        // No foreign-key pragma exists on DuckDB; the dump still runs as one
        // transaction.
        assert_eq!(
            session_prelude(DatabaseType::DuckDb),
            "BEGIN TRANSACTION;\n"
        );
        assert_eq!(session_epilogue(DatabaseType::DuckDb), "COMMIT;\n");
        assert!(session_prelude(DatabaseType::Sqlite).contains("PRAGMA foreign_keys=OFF"));
        assert_eq!(
            duckdb_sequence_advance_sql(
                DatabaseType::DuckDb,
                "\"main\".\"orders\"",
                "id",
                "main.o'seq"
            ),
            "SELECT nextval('main.o''seq') FROM range((SELECT COALESCE(MAX(\"id\"), 0) FROM \"main\".\"orders\"));"
        );
    }

    #[test]
    fn header_never_contains_host_or_user() {
        let profile = ConnectionProfile::sql_login(
            "secret-user",
            "secret-host.example",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        let _ = profile;
        let header = dump_header(
            DatabaseType::Postgres,
            "appdb",
            Utc::now(),
            &DumpOptions::default(),
        );
        assert!(!header.contains("secret-host"));
        assert!(!header.contains("secret-user"));
    }

    #[test]
    fn drop_if_exists_precedes_create() {
        let drop = drop_statement(
            DatabaseType::Postgres,
            ObjectKind::Table,
            Some("public"),
            "users",
        );
        assert!(drop.starts_with("DROP TABLE IF EXISTS"));
        assert!(drop.ends_with(';'));
    }
}
