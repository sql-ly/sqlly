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
