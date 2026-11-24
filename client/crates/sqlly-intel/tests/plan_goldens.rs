//! Pure-plan golden tests (ADR-011 §8.2/§8.3): per-dialect value-query
//! rendering (TOP/LIMIT/FETCH FIRST, quoting, parameter markers, LIKE
//! escaping) and the FK display-column resolution chain — no database needed.
//! Plus the merge property test: incremental generations == one rebuild.

use sqlly_intel::modelengine::intellisense::{
    CompletionItemKind, ValueCompletionParams, plan_value_query, shape_value_items,
};
use sqlly_intel::modelengine::{MatchType, SortOrder};
use sqlly_lang::sqlmodel::{
    Dialect, IncompleteRegions, ModelSnapshot, ObjectBody, ObjectFragment, ObjectKind, ObjectRef,
    Provenance, Span, SqlName,
};
use sqlly_lang::sqlparse::slots::slot_at;
use std::collections::BTreeSet;

fn snapshot_from_sql(sql: &str, dialect: Dialect) -> ModelSnapshot {
    let tree = sqlly_lang::sqlparse::parse(sql, dialect);
    let fragments = sqlly_lang::sqlparse::lower(
        &tree,
        Provenance::Database {
            profile: "test".into(),
        },
    );
    let (snap, _) =
        ModelSnapshot::empty().apply(1, fragments, &BTreeSet::new(), IncompleteRegions::default());
    snap
}

fn schema(dialect: Dialect) -> ModelSnapshot {
    let sql = match dialect {
        Dialect::SqlServer => {
            "CREATE TABLE dbo.Car (Id int PRIMARY KEY, Plate nvarchar(20), ModelId int REFERENCES dbo.Model(Id));\nCREATE TABLE dbo.Model (Id int PRIMARY KEY, Name nvarchar(50));\nCREATE TABLE dbo.Person (Id int PRIMARY KEY, Name nvarchar(100))"
        }
        Dialect::Oracle => {
            "CREATE TABLE CAR (ID NUMBER(10) PRIMARY KEY, PLATE VARCHAR2(20), MODELID NUMBER(10) REFERENCES MODEL(ID));\nCREATE TABLE MODEL (ID NUMBER(10) PRIMARY KEY, NAME VARCHAR2(50));\nCREATE TABLE PERSON (ID NUMBER(10) PRIMARY KEY, NAME VARCHAR2(100))"
        }
        _ => {
            "CREATE TABLE Car (Id integer PRIMARY KEY, Plate varchar(20), ModelId integer REFERENCES Model(Id));\nCREATE TABLE Model (Id integer PRIMARY KEY, Name varchar(50));\nCREATE TABLE Person (Id integer PRIMARY KEY, Name varchar(100))"
        }
    };
    snapshot_from_sql(sql, dialect)
}

fn plan_for(
    dialect: Dialect,
    query: &str,
    prefix: &str,
    params: ValueCompletionParams,
) -> sqlly_intel::modelengine::intellisense::ValueQueryPlan {
    let snap = schema(dialect);
    let slot = slot_at(query, dialect, query.len()).expect("slot at cursor");
    plan_value_query(&slot, prefix, &params, &snap, dialect, None, None).expect("plan")
}

#[test]
fn sqlserver_renders_top_and_brackets_and_named_params() {
    let plan = plan_for(
        Dialect::SqlServer,
        "select * from Person p where p.Name = ",
        "aa",
        ValueCompletionParams::default(),
    );
    assert_eq!(
        plan.sql,
        "SELECT DISTINCT TOP(100) [Name] FROM [dbo].[Person] WHERE [Name] LIKE @p1 ESCAPE '\\' AND [Name] IS NOT NULL ORDER BY [Name] ASC"
    );
    assert_eq!(plan.params, vec!["%aa%"]);
}

#[test]
fn postgres_renders_limit_and_double_quotes_and_dollar_params() {
    let plan = plan_for(
        Dialect::Postgres,
        "select * from Person p where p.Name = ",
        "aa",
        ValueCompletionParams::default(),
    );
    assert_eq!(
        plan.sql,
        "SELECT DISTINCT \"Name\" FROM \"Person\" WHERE \"Name\" LIKE $1 ESCAPE '\\' AND \"Name\" IS NOT NULL ORDER BY \"Name\" ASC LIMIT 100"
    );
    assert_eq!(plan.params, vec!["%aa%"]);
}

#[test]
fn mysql_renders_limit_and_backticks_and_question_params() {
    let plan = plan_for(
        Dialect::MySql,
        "select * from Person p where p.Name = ",
        "aa",
        ValueCompletionParams::default(),
    );
    assert_eq!(
        plan.sql,
        "SELECT DISTINCT `Name` FROM `Person` WHERE `Name` LIKE ? ESCAPE '\\' AND `Name` IS NOT NULL ORDER BY `Name` ASC LIMIT 100"
    );
}

#[test]
fn oracle_renders_fetch_first_and_colon_params() {
    let plan = plan_for(
        Dialect::Oracle,
        "select * from Person p where p.Name = ",
        "aa",
        ValueCompletionParams::default(),
    );
    assert_eq!(
        plan.sql,
        "SELECT DISTINCT \"NAME\" FROM \"PERSON\" WHERE \"NAME\" LIKE :p1 ESCAPE '\\' AND \"NAME\" IS NOT NULL ORDER BY \"NAME\" ASC FETCH FIRST 100 ROWS ONLY"
    );
    assert_eq!(plan.params, vec!["%aa%"]);
}

#[test]
fn like_wildcards_in_prefix_are_escaped() {
    let plan = plan_for(
        Dialect::SqlServer,
        "select * from Person p where p.Name = ",
        "50%_done",
        ValueCompletionParams::default(),
    );
    assert_eq!(plan.params, vec!["%50\\%\\_done%"]);
}

#[test]
fn match_type_operators_bind_prefix_as_param() {
    for (mt, op) in [
        (MatchType::Equals, "="),
        (MatchType::Gt, ">"),
        (MatchType::Lt, "<"),
        (MatchType::Ge, ">="),
        (MatchType::Le, "<="),
        (MatchType::Ne, "<>"),
    ] {
        let plan = plan_for(
            Dialect::SqlServer,
            "select * from Person p where p.Name = ",
            "x",
            ValueCompletionParams {
                match_type: Some(mt),
                ..Default::default()
            },
        );
        assert!(
            plan.sql.contains(&format!("[Name] {op} @p1")),
            "{mt:?}: {}",
            plan.sql
        );
        assert_eq!(plan.params, vec!["x"]);
    }
}

#[test]
fn between_uses_two_bounds_or_falls_back_to_like() {
    let plan = plan_for(
        Dialect::SqlServer,
        "select * from Person p where p.Name = ",
        "a..m",
        ValueCompletionParams {
            match_type: Some(MatchType::Between),
            ..Default::default()
        },
    );
    assert!(plan.sql.contains("BETWEEN @p1 AND @p2"), "{}", plan.sql);
    assert_eq!(plan.params, vec!["a", "m"]);

    // Malformed range falls back to LIKE rather than erroring (§8.2).
    let plan = plan_for(
        Dialect::SqlServer,
        "select * from Person p where p.Name = ",
        "justtext",
        ValueCompletionParams {
            match_type: Some(MatchType::Between),
            ..Default::default()
        },
    );
    assert!(plan.sql.contains("LIKE"), "{}", plan.sql);
}

#[test]
fn limit_and_sort_render() {
    let plan = plan_for(
        Dialect::SqlServer,
        "select * from Person p where p.Name = ",
        "",
        ValueCompletionParams {
            limit: Some(25),
            sort: Some(SortOrder::Desc),
            ..Default::default()
        },
    );
    assert!(plan.sql.contains("TOP(25)"));
    assert!(plan.sql.contains("ORDER BY [Name] DESC"));
    // Limit hard cap.
    let plan = plan_for(
        Dialect::SqlServer,
        "select * from Person p where p.Name = ",
        "",
        ValueCompletionParams {
            limit: Some(999999),
            ..Default::default()
        },
    );
    assert!(plan.sql.contains("TOP(1000)"), "cap applied: {}", plan.sql);
}

#[test]
fn fk_pivot_selects_key_and_display_and_matches_display() {
    let plan = plan_for(
        Dialect::SqlServer,
        "select * from Car c where c.ModelId = ",
        "fal",
        ValueCompletionParams::default(),
    );
    assert_eq!(
        plan.sql,
        "SELECT DISTINCT TOP(100) [Id], [Name] FROM [dbo].[Model] WHERE [Name] LIKE @p1 ESCAPE '\\' AND [Name] IS NOT NULL ORDER BY [Name] ASC"
    );
    assert_eq!(plan.key_index, 0);
    assert_eq!(plan.display_index, 1);
    assert!(plan.note.contains("foreign key"));
}

// ---------------------------------------------------------------------------
// Display-column resolution chain (§8.3)
// ---------------------------------------------------------------------------

fn fk_schema_with_target_columns(target_cols: &str) -> ModelSnapshot {
    snapshot_from_sql(
        &format!(
            "CREATE TABLE dbo.Car (Id int PRIMARY KEY, ModelId int REFERENCES dbo.Model(Id));\nCREATE TABLE dbo.Model ({target_cols})"
        ),
        Dialect::SqlServer,
    )
}

fn fk_plan(
    snap: &ModelSnapshot,
    display_column: Option<&str>,
) -> Result<sqlly_intel::modelengine::intellisense::ValueQueryPlan, String> {
    let query = "select * from Car c where c.ModelId = ";
    let slot = slot_at(query, Dialect::SqlServer, query.len()).expect("slot");
    plan_value_query(
        &slot,
        "",
        &ValueCompletionParams {
            display_column: display_column.map(|s| s.to_string()),
            ..Default::default()
        },
        snap,
        Dialect::SqlServer,
        None,
        None,
    )
}

#[test]
fn display_column_chain_explicit_param_wins() {
    let snap =
        fk_schema_with_target_columns("Id int PRIMARY KEY, Name nvarchar(50), Title nvarchar(50)");
    let plan = fk_plan(&snap, Some("Title")).unwrap();
    assert!(plan.sql.contains("[Id], [Title]"), "{}", plan.sql);
}

#[test]
fn display_column_invalid_param_errors() {
    let snap = fk_schema_with_target_columns("Id int PRIMARY KEY, Name nvarchar(50)");
    let err = fk_plan(&snap, Some("Nope")).unwrap_err();
    assert!(err.contains("does not exist"));
}

#[test]
fn display_column_conventional_names_in_order() {
    // name beats label.
    let snap =
        fk_schema_with_target_columns("Id int PRIMARY KEY, Label nvarchar(50), Name nvarchar(50)");
    assert!(fk_plan(&snap, None).unwrap().sql.contains("[Id], [Name]"));
    // label when no name.
    let snap = fk_schema_with_target_columns("Id int PRIMARY KEY, Label nvarchar(50)");
    assert!(fk_plan(&snap, None).unwrap().sql.contains("[Id], [Label]"));
    // description when no name/label/display.
    let snap = fk_schema_with_target_columns("Id int PRIMARY KEY, Description nvarchar(50)");
    assert!(
        fk_plan(&snap, None)
            .unwrap()
            .sql
            .contains("[Id], [Description]")
    );
    // code last of the conventional list.
    let snap = fk_schema_with_target_columns("Id int PRIMARY KEY, Code nvarchar(10)");
    assert!(fk_plan(&snap, None).unwrap().sql.contains("[Id], [Code]"));
}

#[test]
fn display_column_falls_back_to_first_character_non_key_then_key() {
    // No conventional names: first character-class non-key column.
    let snap =
        fk_schema_with_target_columns("Id int PRIMARY KEY, Quantity int, Remark nvarchar(80)");
    assert!(fk_plan(&snap, None).unwrap().sql.contains("[Id], [Remark]"));
    // Nothing character-typed: degrade to the key column itself.
    let snap = fk_schema_with_target_columns("Id int PRIMARY KEY, Quantity int");
    let plan = fk_plan(&snap, None).unwrap();
    assert!(plan.sql.contains("[Id], [Id]"), "{}", plan.sql);
}

/// Regression (docs screenshot `intellisense-fk-name`): Northwind's
/// Categories has no `Name` column, and the conventional list used to reach
/// `Description` (an ntext column) before ever considering `CategoryName`.
/// The table's own `<Singular>Name` column now wins, then any `…Name`.
#[test]
fn display_column_prefers_the_tables_own_name_column() {
    let snap = snapshot_from_sql(
        "CREATE TABLE dbo.Categories (CategoryID int PRIMARY KEY, CategoryName nvarchar(15), Description ntext);\nCREATE TABLE dbo.Suppliers (SupplierID int PRIMARY KEY, CompanyName nvarchar(40), ContactName nvarchar(30));\nCREATE TABLE dbo.Products (ProductID int PRIMARY KEY, ProductName nvarchar(40), SupplierID int REFERENCES dbo.Suppliers(SupplierID), CategoryID int REFERENCES dbo.Categories(CategoryID))",
        Dialect::SqlServer,
    );
    let plan_for_column = |column: &str| {
        let query = format!("select * from Products p where p.{column} = ");
        let slot = slot_at(&query, Dialect::SqlServer, query.len()).expect("slot");
        plan_value_query(
            &slot,
            "",
            &ValueCompletionParams::default(),
            &snap,
            Dialect::SqlServer,
            None,
            None,
        )
        .expect("plan")
    };
    let category = plan_for_column("CategoryID");
    assert!(
        category
            .sql
            .contains("[CategoryID], [CategoryName] FROM [dbo].[Categories]"),
        "{}",
        category.sql
    );
    // No SupplierName: the first `…Name` character column.
    let supplier = plan_for_column("SupplierID");
    assert!(
        supplier.sql.contains("[SupplierID], [CompanyName]"),
        "{}",
        supplier.sql
    );
    // The editor's plain-text entry point resolves the same pivot.
    let pivot = sqlly_intel::modelengine::intellisense::fk_value_pivot_for(
        &snap,
        None,
        "products",
        "categoryid",
        Dialect::SqlServer,
    )
    .expect("CategoryID is a foreign key");
    assert_eq!(pivot.key_column, "CategoryID");
    assert_eq!(pivot.display_column, "CategoryName");
    assert_eq!(pivot.target.name.spelling(), "Categories");
    assert!(
        sqlly_intel::modelengine::intellisense::fk_value_pivot_for(
            &snap,
            Some("dbo"),
            "Products",
            "ProductName",
            Dialect::SqlServer,
        )
        .is_none(),
        "a plain column has no pivot"
    );
}

// ---------------------------------------------------------------------------
// Merge property: incremental generations == one rebuild
// ---------------------------------------------------------------------------

fn fragment(name: &str, cols: usize, file: bool) -> ObjectFragment {
    let d = Dialect::SqlServer;
    let tree = sqlly_lang::sqlparse::parse(
        &format!(
            "CREATE TABLE dbo.{name} ({})",
            (0..cols)
                .map(|i| format!("C{i} int"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        d,
    );
    let provenance = if file {
        Provenance::File {
            path: format!("{name}.sql"),
            content_hash: cols as u64,
            span: Span::new(0, 1),
        }
    } else {
        Provenance::Database {
            profile: "p".into(),
        }
    };
    sqlly_lang::sqlparse::lower(&tree, provenance).remove(0)
}

#[test]
fn incremental_generations_equal_single_rebuild() {
    // Apply fragments one generation at a time…
    let mut incremental = ModelSnapshot::empty();
    let all = vec![
        fragment("A", 2, false),
        fragment("B", 3, false),
        fragment("A", 4, true), // file overrides db A
        fragment("C", 1, true),
        fragment("B", 3, false), // identical re-collection of B
    ];
    for (i, f) in all.iter().enumerate() {
        let (next, _) = incremental.apply(
            i as u64 + 1,
            vec![f.clone()],
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );
        incremental = next;
    }
    // …versus all at once.
    let (rebuilt, _) =
        ModelSnapshot::empty().apply(1, all, &BTreeSet::new(), IncompleteRegions::default());

    assert_eq!(incremental.len(), rebuilt.len());
    for entry in rebuilt.objects() {
        let counterpart = incremental
            .lookup(&entry.reference)
            .into_iter()
            .find(|e| e.kind == entry.kind)
            .unwrap_or_else(|| panic!("missing {}", entry.reference.display()));
        assert_eq!(
            counterpart.body,
            entry.body,
            "{}",
            entry.reference.display()
        );
        assert_eq!(
            counterpart.provenance.is_file(),
            entry.provenance.is_file(),
            "winner must match for {}",
            entry.reference.display()
        );
        assert_eq!(
            counterpart.shadow.is_some(),
            entry.shadow.is_some(),
            "shadow must match for {}",
            entry.reference.display()
        );
    }
}

#[test]
fn fragment_kind_lowering_sanity() {
    let f = fragment("X", 2, false);
    assert_eq!(f.kind, ObjectKind::Table);
    match &f.body {
        ObjectBody::Table(t) => assert_eq!(t.columns.len(), 2),
        other => panic!("{other:?}"),
    }
    let r = ObjectRef::new(
        None,
        Some(SqlName::new("dbo", Dialect::SqlServer, false)),
        SqlName::new("X", Dialect::SqlServer, false),
    );
    assert_eq!(f.reference, r);
}

// --- Value-completion metadata (parity with the client) ---------------------

#[test]
fn value_completion_items_carry_column_parent_schema_and_value_metadata() {
    // Distinct-value completion of dbo.Person.Name: each item's value belongs
    // to the column it was drawn from, with the owning table's schema.
    let plan = plan_for(
        Dialect::SqlServer,
        "select * from dbo.Person p where p.Name = ",
        "",
        ValueCompletionParams::default(),
    );
    let rows = vec![
        vec![Some("Ada".to_string())],
        vec![Some("Grace".to_string())],
    ];
    let items = shape_value_items(&rows, &plan, Dialect::SqlServer, None);
    let item = items.first().expect("a value item");
    assert_eq!(item.kind, CompletionItemKind::Value);
    assert_eq!(item.why.as_deref(), Some("value"));
    assert_eq!(item.name, "Ada");
    assert_eq!(item.value.as_deref(), Some("Ada"));
    assert_eq!(item.parent.as_deref(), Some("Name"), "parent is the column");
    assert_eq!(item.parent_type, Some(CompletionItemKind::Column));
    assert_eq!(item.schema.as_deref(), Some("dbo"));
}

#[test]
fn fk_value_completion_items_carry_source_column_parent_and_schema() {
    // FK pivot: dbo.Car.ModelId completes against dbo.Model, but the value's
    // parent stays the originating column (ModelId) on dbo.Car.
    let plan = plan_for(
        Dialect::SqlServer,
        "select * from dbo.Car c where c.ModelId = ",
        "",
        ValueCompletionParams::default(),
    );
    let rows = vec![vec![Some("1".to_string()), Some("Civic".to_string())]];
    let items = shape_value_items(&rows, &plan, Dialect::SqlServer, None);
    let item = items.first().expect("a value item");
    assert_eq!(item.why.as_deref(), Some("value"));
    assert_eq!(item.parent.as_deref(), Some("ModelId"));
    assert_eq!(item.parent_type, Some(CompletionItemKind::Column));
    assert_eq!(item.schema.as_deref(), Some("dbo"));
    assert_eq!(item.value.as_deref(), Some("1"));
}

/// Imported plan documents: every file under `tests/golden/plans/<engine>/`
/// goes through the full import pipeline (decode, clean, detect, parse) and
/// the result is compared with the `.golden` file next to it. Refresh with
/// `SQLLY_UPDATE_GOLDEN=1`.
mod plan_documents {
    use std::collections::HashMap;
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};

    use serde_json::Value;
    use sqlly_intel::plans::{
        ExecutionPlan, PlanEngine, PlanError, clean_plan_text, decode_plan_bytes,
        detect_plan_format, import_plan_document, import_plan_document_as,
    };
    use sqlly_testkit::GoldenTest;

    fn plans_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/plans")
    }

    /// The engine a fixture folder holds plans of.
    fn folder_engine(folder: &str) -> PlanEngine {
        match folder {
            "sqlserver" => PlanEngine::SqlServer,
            "postgresql" => PlanEngine::Postgres,
            "mysql" => PlanEngine::MySql,
            "mariadb" => PlanEngine::MariaDb,
            "oracle" => PlanEngine::Oracle,
            "sqlite" => PlanEngine::Sqlite,
            "duckdb" => PlanEngine::DuckDb,
            "clickhouse" => PlanEngine::ClickHouse,
            "cockroachdb" => PlanEngine::CockroachDb,
            "trino" => PlanEngine::Trino,
            "spark" => PlanEngine::Spark,
            other => panic!("no engine for fixture folder {other}"),
        }
    }

    /// Every fixture as `(folder, path)`, sorted.
    fn fixtures() -> Vec<(String, PathBuf)> {
        let mut out = Vec::new();
        let mut folders: Vec<PathBuf> = std::fs::read_dir(plans_dir())
            .expect("fixture folder")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        folders.sort();
        for folder in folders {
            let name = folder.file_name().unwrap().to_string_lossy().to_string();
            let mut files: Vec<PathBuf> = std::fs::read_dir(&folder)
                .expect("engine folder")
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.is_file()
                        && path.extension().is_none_or(|ext| ext != "golden")
                        && !path
                            .file_name()
                            .is_some_and(|name| name.to_string_lossy().starts_with('.'))
                })
                .collect();
            files.sort();
            out.extend(files.into_iter().map(|file| (name.clone(), file)));
        }
        out
    }

    fn read(path: &Path) -> String {
        decode_plan_bytes(&std::fs::read(path).expect("fixture"))
    }

    fn same_family(a: PlanEngine, b: PlanEngine) -> bool {
        let mysql = |engine| matches!(engine, PlanEngine::MySql | PlanEngine::MariaDb);
        a == b || (mysql(a) && mysql(b))
    }

    /// Numbers print with at most six decimals so sums and differences of
    /// times do not put float noise in the goldens.
    fn number(value: f64) -> String {
        let rounded = (value * 1e6).round() / 1e6;
        if rounded == rounded.trunc() && rounded.abs() < 1e15 {
            format!("{}", rounded as i64)
        } else {
            format!("{rounded}")
        }
    }

    fn short(text: &str) -> String {
        let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.chars().count() > 160 {
            let head: String = flat.chars().take(160).collect();
            format!("{head}...")
        } else {
            flat
        }
    }

    /// A value on one line. Empty, false and absent values return `None`
    /// so only what the document said shows up.
    fn compact(value: &Value) -> Option<String> {
        match value {
            Value::Null | Value::Bool(false) => None,
            Value::Bool(true) => Some("yes".into()),
            Value::Number(n) => n.as_f64().map(number),
            Value::String(text) if text.is_empty() => None,
            Value::String(text) => Some(format!("{:?}", short(text))),
            Value::Array(items) => {
                let parts: Vec<String> = items.iter().filter_map(compact).collect();
                (!parts.is_empty()).then(|| format!("[{}]", parts.join(", ")))
            }
            Value::Object(map) => {
                let parts: Vec<String> = sorted(map)
                    .into_iter()
                    .filter_map(|(key, value)| compact(value).map(|value| format!("{key}={value}")))
                    .collect();
                (!parts.is_empty()).then(|| format!("{{{}}}", parts.join(" ")))
            }
        }
    }

    /// Keys in name order. Whether serde_json keeps insertion order depends on
    /// its `preserve_order` feature, which a workspace-wide test build turns on
    /// and a per-crate build may not, so the dump must not depend on it.
    fn sorted(map: &serde_json::Map<String, Value>) -> Vec<(&String, &Value)> {
        let mut entries: Vec<_> = map.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        entries
    }

    fn fields(out: &mut String, indent: &str, value: &Value, skip: &[&str]) {
        let Some(map) = value.as_object() else { return };
        for (key, value) in sorted(map) {
            if skip.contains(&key.as_str()) {
                continue;
            }
            // Zero is the default of the required numeric fields.
            if value.as_f64() == Some(0.0) {
                continue;
            }
            if let Some(text) = compact(value) {
                let _ = writeln!(out, "{indent}{key}: {text}");
            }
        }
    }

    fn dump_plan(out: &mut String, plan: &ExecutionPlan) {
        let value = serde_json::to_value(plan).expect("plan serializes");
        fields(out, "", &value, &["nodes", "edges"]);
        let parents: HashMap<i32, i32> = plan
            .edges
            .iter()
            .map(|edge| (edge.to_node, edge.from_node))
            .collect();
        let _ = writeln!(out, "operators: {}", plan.nodes.len());
        for node in &plan.nodes {
            let mut depth = 0usize;
            let mut at = node.id;
            while let Some(parent) = parents.get(&at) {
                depth += 1;
                at = *parent;
                if depth > plan.nodes.len() {
                    panic!("edges form a cycle at node {}", node.id);
                }
            }
            let indent = "  ".repeat(depth + 1);
            let _ = writeln!(
                out,
                "{indent}#{} {} [{}]",
                node.id, node.physical_op, node.logical_op
            );
            let value = serde_json::to_value(node).expect("node serializes");
            fields(
                out,
                &format!("{indent}    "),
                &value,
                &["id", "physical_op", "logical_op"],
            );
        }
    }

    fn dump(text: &str) -> String {
        let mut out = String::new();
        match import_plan_document(text) {
            Ok(imported) => {
                let _ = writeln!(
                    out,
                    "format: {} ({:?})",
                    imported.format.id(),
                    imported.confidence
                );
                let _ = writeln!(out, "statements: {}", imported.plans.len());
                for (index, plan) in imported.plans.iter().enumerate() {
                    let _ = writeln!(out, "\n== statement {}", index + 1);
                    dump_plan(&mut out, plan);
                }
            }
            Err(error) => {
                let _ = writeln!(out, "error: {error}");
            }
        }
        out
    }

    #[test]
    fn every_fixture_matches_its_golden() {
        let mut mismatched = Vec::new();
        let mut count = 0usize;
        for (_, path) in fixtures() {
            count += 1;
            let actual = dump(&read(&path));
            let golden = PathBuf::from(format!("{}.golden", path.display()));
            let ok = GoldenTest::compare(&actual, &golden)
                .unwrap_or_else(|e| panic!("golden read failed for {}: {e}", golden.display()));
            if !ok {
                mismatched.push(golden.display().to_string());
            }
        }
        assert!(count > 100, "only {count} fixtures found");
        assert!(
            mismatched.is_empty(),
            "golden mismatch, re-run with SQLLY_UPDATE_GOLDEN=1:\n{}",
            mismatched.join("\n")
        );
    }

    /// A fixture is detected as its own engine with certainty, or it is one
    /// of the shapes that are not plans (bytecode, a diagram link) and the
    /// error says what it is. It is never taken for another engine's plan.
    #[test]
    fn every_fixture_is_detected_as_its_own_engine() {
        let mut problems = Vec::new();
        for (folder, path) in fixtures() {
            let engine = folder_engine(&folder);
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let cleaned = clean_plan_text(&read(&path));
            match detect_plan_format(&cleaned) {
                Some(found) if same_family(found.format.engine(), engine) => {
                    match import_plan_document(&read(&path)) {
                        Ok(imported) => {
                            for plan in &imported.plans {
                                if plan.source_format.is_none()
                                    || (plan.dialect.is_none() && engine.dialect().is_some())
                                {
                                    problems.push(format!(
                                        "{folder}/{name}: plan without source_format or dialect"
                                    ));
                                }
                            }
                        }
                        // A recognized document the engine printed no
                        // plan in: the error names the format.
                        Err(PlanError::Import(error)) if error.format_id.is_some() => {}
                        Err(error) => problems.push(format!("{folder}/{name}: {error}")),
                    }
                }
                Some(found) => problems.push(format!(
                    "{folder}/{name}: detected as {} ({})",
                    found.format.id(),
                    found.evidence
                )),
                None => match import_plan_document(&read(&path)) {
                    Err(PlanError::Import(error)) if error.format_label.is_some() => {}
                    other => problems.push(format!(
                        "{folder}/{name}: not detected, and no named reason ({:?})",
                        other.map(|imported| imported.format.id())
                    )),
                },
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }

    /// Picking the engine by hand reads the same plan auto-detection does.
    #[test]
    fn picking_the_engine_gives_the_detected_result() {
        for (folder, path) in fixtures() {
            let text = read(&path);
            let Ok(auto) = import_plan_document(&text) else {
                continue;
            };
            let picked = import_plan_document_as(folder_engine(&folder), &text)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(picked.format, auto.format, "{}", path.display());
            assert_eq!(picked.plans.len(), auto.plans.len(), "{}", path.display());
        }
    }

    /// Truncated and garbled variants of every fixture go through the whole
    /// pipeline. The importer may fail on them; it may not panic, and a
    /// watchdog fails the test if any single document takes too long.
    #[test]
    fn mangled_fixtures_never_panic_or_hang() {
        let (done_tx, done_rx) = std::sync::mpsc::channel::<String>();
        let worker = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(move || {
                for (folder, path) in fixtures() {
                    let sample = read(&path);
                    // Large captures are sampled; the per-parser tests cover
                    // them in full.
                    let sample: String = if sample.len() > 60_000 {
                        sample.chars().take(60_000).collect()
                    } else {
                        sample
                    };
                    for variant in variants(&sample) {
                        let _ = import_plan_document(&variant);
                        let _ = import_plan_document_as(folder_engine(&folder), &variant);
                    }
                    let bytes = std::fs::read(&path).expect("fixture");
                    for cut in [
                        1usize,
                        2,
                        3,
                        5,
                        7,
                        bytes.len() / 2,
                        bytes.len().saturating_sub(1),
                    ] {
                        let _ = import_plan_document(&decode_plan_bytes(
                            &bytes[..cut.min(bytes.len())],
                        ));
                    }
                    done_tx
                        .send(path.display().to_string())
                        .expect("watchdog alive");
                }
            })
            .expect("worker thread");
        let mut last = String::from("(none)");
        loop {
            match done_rx.recv_timeout(std::time::Duration::from_secs(120)) {
                Ok(path) => last = path,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!("the fixture after {last} took more than 120 s")
                }
            }
        }
        worker
            .join()
            .expect("a mangled fixture made the importer panic");
    }

    fn variants(sample: &str) -> Vec<String> {
        let mut out = Vec::new();
        let boundaries: Vec<usize> = sample
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(sample.len()))
            .collect();
        let step = (boundaries.len() / 24).max(1);
        for cut in boundaries.iter().step_by(step) {
            out.push(sample[..*cut].to_string());
            out.push(sample[*cut..].to_string());
        }
        let lines: Vec<&str> = sample.lines().collect();
        let line_step = (lines.len() / 12).max(1);
        for skip in (0..lines.len()).step_by(line_step) {
            let without: Vec<&str> = lines
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != skip)
                .map(|(_, line)| *line)
                .collect();
            out.push(without.join("\n"));
        }
        out.push(lines.iter().rev().copied().collect::<Vec<_>>().join("\n"));
        out.push(format!("{sample}\n{sample}"));
        for (from, to) in [
            ("(", ""),
            (")", ""),
            ("|", ""),
            (":", ""),
            ("=", ""),
            ("\"", ""),
            ("{", "["),
            ("}", ""),
            ("<", ""),
            (">", "<"),
            ("-", ""),
            (" ", ""),
            (" ", "\t"),
            ("\n", " "),
            ("\n", "\r\n"),
            ("0", "99999999999999999999999999"),
            ("1", "-1"),
            ("1", "NaN"),
            ("a", "\u{00e9}\u{4e2d}"),
            (",", ""),
        ] {
            out.push(sample.replace(from, to));
        }
        out.push(
            lines
                .iter()
                .map(|l| l.trim_start())
                .collect::<Vec<_>>()
                .join("\n"),
        );
        out.push(
            lines
                .iter()
                .enumerate()
                .map(|(index, line)| format!("{:>4}  {line}", index + 1))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        out.push(format!("```\n{sample}\n```"));
        out.push(sample.to_uppercase());
        out.push(serde_json::to_string(sample).expect("string serializes"));
        out
    }

    /// What surrounds a plan on its way to the clipboard does not change
    /// what is read: each wrapping of a fixture imports to the same plans.
    #[test]
    fn wrapped_fixtures_read_the_same() {
        let mut problems = Vec::new();
        for (_, path) in fixtures() {
            let text = read(&path);
            let Ok(plain) = import_plan_document(&text) else {
                continue;
            };
            let expected = dump(&text);
            let numbered = text
                .lines()
                .enumerate()
                .map(|(index, line)| format!("{:>3}  {line}", index + 1))
                .collect::<Vec<_>>()
                .join("\n");
            let wrappings = [
                ("CRLF", text.replace('\n', "\r\n")),
                ("BOM", format!("\u{feff}{text}")),
                (
                    "code fence",
                    format!("Here is the plan:\n\n```\n{text}\n```\n\nThanks!"),
                ),
                ("sql fence", format!("```sql\n{text}\n```")),
                ("line numbers", numbered),
                ("JSON string", serde_json::to_string(&text).expect("string")),
                (
                    "UTF-16",
                    decode_plan_bytes(
                        &[0xFF, 0xFE]
                            .into_iter()
                            .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
                            .collect::<Vec<u8>>(),
                    ),
                ),
            ];
            for (name, wrapped) in wrappings {
                // Line numbers on a document whose lines start with numbers
                // (plan table rows) cannot be told from data.
                if name == "line numbers"
                    && (text.lines().count() < 3
                        || matches!(
                            plain.format.id(),
                            "oracle-rows"
                                | "sqlite-eqp"
                                | "mysql-table"
                                | "mariadb-table"
                                | "sqlserver-text"
                        ))
                {
                    continue;
                }
                let got = dump(&wrapped);
                if got != expected {
                    let first = got.lines().next().unwrap_or_default();
                    problems.push(format!(
                        "{} wrapped as {name}: {}",
                        path.display(),
                        first.chars().take(160).collect::<String>()
                    ));
                }
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    }
}
