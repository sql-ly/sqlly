//! IntelliSense completion cookbook.
//!
//! This file is a *worked example* of how to test symbol-mode IntelliSense.
//! It does three things:
//!
//!   1. Defines one shared, realistic test schema in a single place
//!      (`test_schema()`), and a `snapshot()` that hands every test its own
//!      fresh clone — so tests never share mutable state.
//!   2. Gives you a tiny harness, `complete_at("... {cursor} ...")`, that
//!      places the cursor wherever you write the literal marker `{cursor}`,
//!      triggers completion there, and returns the results.
//!   3. Prints what got triggered (`dump(...)`) and offers assertion helpers
//!      (`assert_has`, `assert_excludes`, `labels_of`) so a new test is just a
//!      query string plus the labels you expect.
//!
//! Run it and watch the completions print:
//!
//! ```sh
//! cargo test -p sqlly-modelengine --test intellisense_cookbook -- --nocapture
//! ```
//!
//! ## The shared schema at a glance
//!
//! Two *used* schemas, `Sales` and `HR`, hold four FK-linked tables with mixed
//! data types:
//!
//! ```text
//!   Sales.Customer    (CustomerId PK, ...)
//!   Sales.SalesOrder  (OrderId PK, CustomerId -> Sales.Customer)        [intra-schema FK]
//!   HR.Department     (DepartmentId PK, ...)
//!   HR.Employee       (EmployeeId PK, DepartmentId -> HR.Department,    [intra-schema FK]
//!                                      AssignedCustomerId -> Sales.Customer) [cross-schema FK]
//! ```
//!
//! Plus, in the used schemas: two views, two stored procedures, two functions,
//! and one `Sales.TypeShowcase` table that carries exactly one column of every
//! `TypeClass` — this is the "one variable of each type" requirement. (Symbol
//! completion has no concept of a free-standing SQL `@variable`; the closest
//! faithful modeling is one typed column per data-type class, whose native
//! spelling shows up in a completion item's `detail`.)
//!
//! Two *unused* schemas, `Archive` and `Staging`, each hold one table. They
//! exist in the model but no demonstration query references them — handy for
//! asserting that a prefix filter excludes objects you didn't ask for.

use sqlly_intel::modelengine::intellisense::CompletionItemKind;
use sqlly_intel::modelengine::{CompletionMode, CompletionRequest, Completions, complete_symbols};
use sqlly_lang::sqlmodel::{
    ColumnDef, Dialect, Fidelity, ForeignKeyDef, IncompleteRegions, KeyDef, ModelSnapshot,
    ObjectBody, ObjectFragment, ObjectKind, ObjectRef, ParamDef, ParamMode, Provenance,
    RoutineBody, SqlName, TableBody, TypeRef, ViewBody,
};
use std::collections::BTreeSet;

/// Every object in this cookbook is SQL Server flavored.
const D: Dialect = Dialect::SqlServer;

// ===========================================================================
// Small builders — keep the schema definition declarative and readable.
// ===========================================================================

fn n(s: &str) -> SqlName {
    SqlName::new(s, D, false)
}

/// A schema-qualified object reference, e.g. `Sales.Customer`.
fn obj(schema: &str, name: &str) -> ObjectRef {
    ObjectRef::new(None, Some(n(schema)), n(name))
}

/// A column with a real, parsed type (the native spelling shows up as the
/// completion item's `detail`).
fn col(name: &str, ty: &str, ordinal: usize) -> ColumnDef {
    ColumnDef {
        name: n(name),
        ty: Some(TypeRef::parse(ty, D)),
        nullable: Some(true),
        default: None,
        identity: false,
        generated: None,
        collation: None,
        ordinal,
    }
}

fn pk(column: &str) -> KeyDef {
    KeyDef {
        name: None,
        columns: vec![n(column)],
    }
}

/// A single-column foreign key: `columns` on this table reference
/// `tgt_schema.tgt_table (tgt_cols)`.
fn fk(columns: &[&str], tgt_schema: &str, tgt_table: &str, tgt_cols: &[&str]) -> ForeignKeyDef {
    ForeignKeyDef {
        name: None,
        columns: columns.iter().map(|c| n(c)).collect(),
        target: obj(tgt_schema, tgt_table),
        target_columns: tgt_cols.iter().map(|c| n(c)).collect(),
    }
}

fn frag(
    reference: ObjectRef,
    kind: ObjectKind,
    body: ObjectBody,
    references: Vec<ObjectRef>,
) -> ObjectFragment {
    ObjectFragment {
        reference,
        kind,
        dialect: D,
        body,
        provenance: Provenance::Database {
            profile: "cookbook".into(),
        },
        fidelity: Fidelity::Exact,
        references,
    }
}

fn table(
    schema: &str,
    name: &str,
    columns: Vec<ColumnDef>,
    primary_key: Option<KeyDef>,
    foreign_keys: Vec<ForeignKeyDef>,
) -> ObjectFragment {
    // A table's `references` (for the dependency/symbol index) are its FK
    // targets.
    let references = foreign_keys.iter().map(|f| f.target.clone()).collect();
    frag(
        obj(schema, name),
        ObjectKind::Table,
        ObjectBody::Table(TableBody {
            columns,
            primary_key,
            foreign_keys,
            ..TableBody::default()
        }),
        references,
    )
}

fn view(schema: &str, name: &str, columns: Vec<ColumnDef>) -> ObjectFragment {
    frag(
        obj(schema, name),
        ObjectKind::View,
        ObjectBody::View(ViewBody {
            columns,
            definition: None,
        }),
        Vec::new(),
    )
}

fn param(name: &str, ty: &str, mode: ParamMode) -> ParamDef {
    ParamDef {
        name: n(name),
        ty: Some(TypeRef::parse(ty, D)),
        mode,
        default: None,
    }
}

fn proc(schema: &str, name: &str, params: Vec<ParamDef>) -> ObjectFragment {
    frag(
        obj(schema, name),
        ObjectKind::Procedure,
        ObjectBody::Routine(RoutineBody {
            params,
            returns: None,
            language: None,
            body_text: None,
        }),
        Vec::new(),
    )
}

fn func(schema: &str, name: &str, params: Vec<ParamDef>, returns: &str) -> ObjectFragment {
    frag(
        obj(schema, name),
        ObjectKind::Function,
        ObjectBody::Routine(RoutineBody {
            params,
            returns: Some(TypeRef::parse(returns, D)),
            language: None,
            body_text: None,
        }),
        Vec::new(),
    )
}

// ===========================================================================
// THE shared schema — defined once, here.
// ===========================================================================

/// Build the full set of fragments for the test database. Each call produces a
/// brand-new `Vec`, so callers get an independent copy they may mutate freely.
fn test_schema() -> Vec<ObjectFragment> {
    vec![
        // --- Used schema #1: Sales --------------------------------------
        table(
            "Sales",
            "Customer",
            vec![
                col("CustomerId", "int", 0),
                col("FullName", "nvarchar(200)", 1),
                col("CreditLimit", "decimal(18,2)", 2),
                col("IsActive", "bit", 3),
                col("ExternalGuid", "uniqueidentifier", 4),
                col("CreatedAt", "datetime2", 5),
            ],
            Some(pk("CustomerId")),
            vec![],
        ),
        table(
            "Sales",
            "SalesOrder",
            vec![
                col("OrderId", "int", 0),
                col("CustomerId", "int", 1),
                col("OrderDate", "date", 2),
                col("Amount", "money", 3),
                col("Notes", "nvarchar(max)", 4),
            ],
            Some(pk("OrderId")),
            // intra-schema FK: SalesOrder.CustomerId -> Sales.Customer.CustomerId
            vec![fk(&["CustomerId"], "Sales", "Customer", &["CustomerId"])],
        ),
        // --- Used schema #2: HR -----------------------------------------
        table(
            "HR",
            "Department",
            vec![
                col("DepartmentId", "int", 0),
                col("DeptName", "varchar(100)", 1),
                col("HeadCount", "smallint", 2),
            ],
            Some(pk("DepartmentId")),
            vec![],
        ),
        table(
            "HR",
            "Employee",
            vec![
                col("EmployeeId", "int", 0),
                col("DepartmentId", "int", 1),
                col("AssignedCustomerId", "int", 2),
                col("FullName", "nvarchar(150)", 3),
                col("Salary", "decimal(12,2)", 4),
                col("Birthdate", "date", 5),
            ],
            Some(pk("EmployeeId")),
            vec![
                // intra-schema FK: Employee.DepartmentId -> HR.Department
                fk(&["DepartmentId"], "HR", "Department", &["DepartmentId"]),
                // cross-schema FK: Employee.AssignedCustomerId -> Sales.Customer
                fk(
                    &["AssignedCustomerId"],
                    "Sales",
                    "Customer",
                    &["CustomerId"],
                ),
            ],
        ),
        // --- One column of every data-type class ("variable of each type") ---
        type_showcase_table(),
        // --- Two views (views expose columns, so alias-dot works on them) ---
        view(
            "Sales",
            "vCustomerOrders",
            vec![
                col("CustomerId", "int", 0),
                col("FullName", "nvarchar(200)", 1),
                col("OrderId", "int", 2),
                col("Amount", "money", 3),
            ],
        ),
        view(
            "HR",
            "vDepartmentRoster",
            vec![
                col("DepartmentId", "int", 0),
                col("DeptName", "varchar(100)", 1),
                col("EmployeeId", "int", 2),
                col("FullName", "nvarchar(150)", 3),
            ],
        ),
        // --- Two stored procedures --------------------------------------
        proc(
            "Sales",
            "pGetCustomerOrders",
            vec![param("@CustomerId", "int", ParamMode::In)],
        ),
        proc(
            "HR",
            "pAddEmployee",
            vec![
                param("@FullName", "nvarchar(150)", ParamMode::In),
                param("@DepartmentId", "int", ParamMode::In),
            ],
        ),
        // --- Two functions ----------------------------------------------
        func(
            "Sales",
            "fnOrderTotal",
            vec![param("@OrderId", "int", ParamMode::In)],
            "money",
        ),
        func(
            "HR",
            "fnEmployeeFullName",
            vec![param("@EmployeeId", "int", ParamMode::In)],
            "nvarchar(300)",
        ),
        // --- Two UNUSED schemas: present but referenced by no demo query ---
        table(
            "Archive",
            "ArchivedOrder",
            vec![col("OrderId", "int", 0), col("ArchivedAt", "datetime2", 1)],
            Some(pk("OrderId")),
            vec![],
        ),
        table(
            "Staging",
            "RawCustomerImport",
            vec![col("RowId", "int", 0), col("Payload", "nvarchar(max)", 1)],
            Some(pk("RowId")),
            vec![],
        ),
    ]
}

/// One column for every `TypeClass`, each with a native spelling that parses to
/// that class. This is the "one variable of each type" object.
fn type_showcase_table() -> ObjectFragment {
    // (column name, native type spelling) — TypeRef::parse classifies each.
    let specs: &[(&str, &str)] = &[
        ("IntegerCol", "int"),
        ("DecimalCol", "decimal(18,2)"),
        ("FloatCol", "float"),
        ("CharacterCol", "nvarchar(100)"),
        ("BinaryCol", "varbinary(max)"),
        ("DateTimeCol", "datetime2"),
        ("BooleanCol", "boolean"),
        ("UuidCol", "uniqueidentifier"),
        ("JsonCol", "json"),
        ("XmlCol", "xml"),
        ("SpatialCol", "geometry"),
        ("RowIdCol", "rowversion"),
        ("IntervalCol", "interval"),
        ("OtherCol", "MyCustomType"),
    ];
    let columns = specs
        .iter()
        .enumerate()
        .map(|(i, (name, ty))| col(name, ty, i))
        .collect();
    table("Sales", "TypeShowcase", columns, None, vec![])
}

/// Apply the shared schema into a fresh snapshot. Every test calls this to get
/// its own independent copy of the model.
fn snapshot() -> ModelSnapshot {
    let (snap, _delta) = ModelSnapshot::empty().apply(
        1,
        test_schema(),
        &BTreeSet::new(),
        IncompleteRegions::default(),
    );
    snap
}

// ===========================================================================
// The harness: write a query with a `{cursor}` marker, get completions.
// ===========================================================================

/// Trigger symbol completion at the `{cursor}` marker in `query_with_marker`.
///
/// The marker is removed before parsing; the cursor byte-offset is the marker's
/// position. Exactly one marker must be present.
fn complete_at(query_with_marker: &str, snap: &ModelSnapshot) -> Completions {
    let cursor = query_with_marker
        .find("{cursor}")
        .expect("query must contain a `{cursor}` marker");
    assert!(
        !query_with_marker[cursor + "{cursor}".len()..].contains("{cursor}"),
        "query must contain exactly one `{{cursor}}` marker"
    );
    let text = query_with_marker.replacen("{cursor}", "", 1);
    let req = CompletionRequest {
        text,
        cursor, // byte offset; bytes before the marker are unchanged by removal
        dialect: D,
        default_catalog: None,
        default_namespace: Some(n("dbo")),
        mode: CompletionMode::Symbol,
    };
    let completions = complete_symbols(&req, snap);
    assert_label_insert_text_parity(&completions);
    completions
}

/// Invariant enforced on *every* completion this cookbook triggers: what the
/// user sees (`label`) is exactly what ends up in the buffer. For unqualified
/// objects (`FROM Cust`) that means `insert_text == label`, so a schema-qualified
/// object such as `Sales.Customer` inserts the full `Sales.Customer` and resolves
/// regardless of the active default schema.
///
/// Four completion shapes intentionally diverge and are exempt:
///   * FK-aware join candidates (Table kind, `detail == "fk"`): the label reads
///     `Target ON <conds>` while the insert carries the generated alias and the
///     alias-qualified `ON` clause (`Target a ON a.X = ...`).
///   * Clause-writing items — tables after a finished SELECT list
///     (`detail == "adds FROM"`) and columns after a FROM source
///     (`detail == "adds WHERE"`): the insert is the keyword, a newline and an
///     indent ahead of the label.
///   * Value-mode items (Value kind): the label is the human-readable value
///     while the insert is the SQL literal (FK values append a `/* display */`
///     comment).
///   * Schema-qualified partials (`FROM Sales.Cus`): the user already typed the
///     `Sales.` qualifier, so the label still shows `Sales.Customer` for clarity
///     while the insert is the bare `Customer` (the name-only replace span keeps
///     the schema). The buffer still reads `Sales.Customer` — parity holds at the
///     buffer level, just not in the `insert_text` string.
fn assert_label_insert_text_parity(c: &Completions) {
    for it in &c.items {
        let is_fk_join = it.kind == CompletionItemKind::Table && it.detail.as_deref() == Some("fk");
        let is_value = it.kind == CompletionItemKind::Value;
        let is_schema_qualified_partial = it
            .schema
            .as_deref()
            .map(|s| it.label == format!("{s}.{}", it.name) && it.insert_text == it.name)
            .unwrap_or(false);
        // A table offered after a finished SELECT list inserts `FROM⏎⇥<label>`
        // and a column offered after a FROM source `WHERE⏎⇥<label>`, so
        // accepting either writes the whole clause (feedback ifb_9f91,
        // ifb_92d9).
        let is_clause_insert = match it.detail.as_deref() {
            Some("adds FROM") => it.insert_text.starts_with("FROM\n"),
            Some("adds WHERE") => it.insert_text.starts_with("WHERE\n"),
            _ => false,
        } && it.insert_text.ends_with(&it.label);
        if is_fk_join || is_value || is_schema_qualified_partial || is_clause_insert {
            continue;
        }
        assert_eq!(
            it.insert_text, it.label,
            "label/insert_text parity violated for {:?} `{}`: insert_text was `{}`",
            it.kind, it.label, it.insert_text
        );
    }
}

// ===========================================================================
// Output + assertion helpers.
// ===========================================================================

/// Print every triggered completion. Visible with `-- --nocapture`.
fn dump(title: &str, c: &Completions) {
    println!("\n=== {title} ===");
    println!(
        "generation={} truncated={} items={}",
        c.snapshot_generation,
        c.truncated,
        c.items.len()
    );
    for it in &c.items {
        let detail = it
            .detail
            .as_ref()
            .map(|d| format!("  ({d})"))
            .unwrap_or_default();
        let tentative = if it.tentative { "  <tentative>" } else { "" };
        println!("  [{:?}] {}{}{}", it.kind, it.label, detail, tentative);
    }
}

fn labels(c: &Completions) -> Vec<String> {
    c.items.iter().map(|i| i.label.clone()).collect()
}

fn labels_of(c: &Completions, kind: CompletionItemKind) -> Vec<String> {
    c.items
        .iter()
        .filter(|i| i.kind == kind)
        .map(|i| i.label.clone())
        .collect()
}

/// Assert each expected label appears at least once (regardless of kind).
fn assert_has(c: &Completions, expected: &[&str]) {
    let got = labels(c);
    for want in expected {
        assert!(
            got.iter().any(|l| l == want),
            "expected completion `{want}` not found.\n  got: {got:#?}"
        );
    }
}

/// Assert none of these labels appear.
fn assert_excludes(c: &Completions, forbidden: &[&str]) {
    let got = labels(c);
    for bad in forbidden {
        assert!(
            !got.iter().any(|l| l == bad),
            "completion `{bad}` should NOT be present.\n  got: {got:#?}"
        );
    }
}

// ===========================================================================
// HOW TO ADD MORE TESTS
// ---------------------------------------------------------------------------
// A completion test is three lines:
//
//   let snap = snapshot();                                 // fresh schema copy
//   let c = complete_at("SELECT x.{cursor} FROM ...", &snap);
//   assert_has(&c, &["SomeColumn"]);                       // what you expect
//
// Tips:
//   * Put the marker exactly where the user's caret would be. The text right
//     before it decides the context:
//       - `alias.{cursor}`            -> columns of the aliased table/view
//       - `FROM {cursor}` / `JOIN {cursor}` -> tables + views (+ FK joins)
//       - `EXEC {cursor}`             -> procedures + functions
//       - anywhere else               -> all objects + in-scope columns
//   * For `alias.{cursor}`, keep the FROM clause complete to the *left* of the
//     marker (e.g. `... FROM T a WHERE a.{cursor}`). A dangling `a.` placed
//     immediately before `FROM` makes the parser swallow `FROM` as the member,
//     so the table source isn't harvested and you get an empty list.
//   * Matching is "contains", not prefix-only: prefix `Cust` matches both
//     `Customer` *and* `RawCustomerImport`. Choose a discriminating prefix when
//     you want to exclude something (`SalesOrd`, `Employee`, ...).
//   * Call `dump("label", &c)` while iterating to see exactly what fired, then
//     turn the interesting labels into `assert_has` / `assert_excludes`.
//   * `labels_of(&c, CompletionItemKind::Column)` narrows to one kind.
//
// The fully worked example below (`example_write_your_own_test`) is a template
// you can copy.
// ===========================================================================

#[test]
fn example_write_your_own_test() {
    // 1. Get a private copy of the shared schema.
    let snap = snapshot();

    // 2. Write the query and drop a `{cursor}` where the caret is.
    //    Here: just after the alias `e.` for HR.Employee (FROM clause already
    //    complete to the left of the marker).
    let completions = complete_at("SELECT * FROM HR.Employee e WHERE e.{cursor}", &snap);

    // 3. Print what fired (visible with `-- --nocapture`)...
    dump(
        "example: columns of HR.Employee via alias `e`",
        &completions,
    );

    // 4. ...and assert what should be in the list.
    assert_has(
        &completions,
        &[
            "EmployeeId",
            "DepartmentId",
            "AssignedCustomerId",
            "Salary",
            "Birthdate",
        ],
    );
    // Columns of *other* tables must not leak in.
    assert_excludes(&completions, &["DeptName", "Notes"]);
    // Every item here is a column.
    for it in &completions.items {
        assert_eq!(it.kind, CompletionItemKind::Column);
    }
}

// ===========================================================================
// Demonstration tests (one per completion context).
// ===========================================================================

#[test]
fn alias_dot_completes_table_columns_with_type_detail() {
    let snap = snapshot();
    let c = complete_at("SELECT * FROM Sales.Customer c WHERE c.{cursor}", &snap);
    dump("alias-dot: Sales.Customer columns", &c);

    assert_has(
        &c,
        &[
            "CustomerId",
            "FullName",
            "CreditLimit",
            "IsActive",
            "ExternalGuid",
            "CreatedAt",
        ],
    );
    // The native type rides along as `detail` — verify a couple.
    let detail_of = |label: &str| {
        c.items
            .iter()
            .find(|i| i.label == label)
            .and_then(|i| i.detail.clone())
    };
    assert_eq!(detail_of("FullName").as_deref(), Some("nvarchar(200)"));
    assert_eq!(detail_of("CreditLimit").as_deref(), Some("decimal(18,2)"));
}

#[test]
fn alias_dot_completes_view_columns() {
    let snap = snapshot();
    let c = complete_at(
        "SELECT * FROM Sales.vCustomerOrders v WHERE v.{cursor}",
        &snap,
    );
    dump("alias-dot: Sales.vCustomerOrders columns", &c);
    assert_has(&c, &["CustomerId", "FullName", "OrderId", "Amount"]);
}

#[test]
fn from_join_offers_fk_join_for_related_table() {
    let snap = snapshot();
    // SalesOrder has an FK to Customer, so completing the JOIN target offers a
    // ready-made `Sales.Customer ... ON ...` suggestion (detail == "fk").
    let c = complete_at(
        "SELECT * FROM Sales.SalesOrder so INNER JOIN {cursor}",
        &snap,
    );
    dump("from/join: FK-aware join candidates", &c);

    let fk_items: Vec<&str> = c
        .items
        .iter()
        .filter(|i| i.detail.as_deref() == Some("fk"))
        .map(|i| i.label.as_str())
        .collect();
    assert!(
        fk_items.iter().any(|l| l.starts_with("Sales.Customer ON")),
        "expected an FK join suggestion to Sales.Customer, got: {fk_items:#?}"
    );
    // Plain table/view names are offered too (cross-schema is fine).
    assert_has(&c, &["HR.Employee", "HR.Department"]);
}

#[test]
fn cross_schema_fk_join_is_offered() {
    let snap = snapshot();
    // Employee.AssignedCustomerId is a *cross-schema* FK into Sales.Customer.
    let c = complete_at("SELECT * FROM HR.Employee e INNER JOIN {cursor}", &snap);
    dump("from/join: cross-schema FK candidates", &c);
    let fk_items: Vec<&str> = c
        .items
        .iter()
        .filter(|i| i.detail.as_deref() == Some("fk"))
        .map(|i| i.label.as_str())
        .collect();
    assert!(
        fk_items.iter().any(|l| l.starts_with("Sales.Customer ON")),
        "cross-schema FK to Sales.Customer expected, got: {fk_items:#?}"
    );
    assert!(
        fk_items.iter().any(|l| l.starts_with("HR.Department ON")),
        "intra-schema FK to HR.Department expected, got: {fk_items:#?}"
    );
}

#[test]
fn exec_context_offers_procedures_and_functions() {
    let snap = snapshot();
    let c = complete_at("EXEC {cursor}", &snap);
    dump("exec: procedures + functions", &c);

    let procs = labels_of(&c, CompletionItemKind::Procedure);
    let funcs = labels_of(&c, CompletionItemKind::Function);
    assert!(
        procs.contains(&"Sales.pGetCustomerOrders".to_string())
            && procs.contains(&"HR.pAddEmployee".to_string()),
        "procs: {procs:#?}"
    );
    assert!(
        funcs.contains(&"Sales.fnOrderTotal".to_string())
            && funcs.contains(&"HR.fnEmployeeFullName".to_string()),
        "funcs: {funcs:#?}"
    );
    // EXEC context must not offer tables/views.
    assert_excludes(&c, &["Sales.Customer", "Sales.vCustomerOrders"]);
}

#[test]
fn prefix_filters_out_unused_schema_objects() {
    let snap = snapshot();
    // Typing `Sales` after FROM matches Sales.SalesOrder (its bare name starts
    // with "Sales") but not the unused Archive/Staging tables, whose names
    // don't contain that substring. (Matching is contains-based, so pick a
    // discriminating prefix.)
    let c = complete_at("SELECT * FROM Sales{cursor}", &snap);
    dump("from: prefix `Sales` filtering", &c);
    assert_has(&c, &["Sales.SalesOrder"]);
    assert_excludes(&c, &["Archive.ArchivedOrder", "Staging.RawCustomerImport"]);
}

#[test]
fn general_context_offers_in_scope_columns() {
    let snap = snapshot();
    // In the SELECT list, columns of the FROM tables are in scope (qualified by
    // their alias), alongside object names.
    let c = complete_at("SELECT {cursor} FROM Sales.Customer c", &snap);
    dump("general: SELECT-list in-scope columns + objects", &c);
    // Alias-qualified columns of the in-scope table.
    assert_has(&c, &["c.CustomerId", "c.FullName"]);
    // Object names are still offered in the general context.
    assert_has(&c, &["Sales.SalesOrder"]);
}

#[test]
fn object_name_insert_text_keeps_schema_qualifier() {
    // Regression: the label showed `Sales.Customer` but the insert cut the
    // schema and only wrote `Customer`. Accepting the completion must insert the
    // fully-qualified `Sales.Customer` so the SQL resolves regardless of the
    // session's default schema. `complete_at` already enforces label/insert_text
    // parity for every item; this test pins the exact object and value.
    let snap = snapshot();
    let c = complete_at("SELECT * FROM Cust{cursor}", &snap);
    dump("object-name: schema-qualified insert_text", &c);
    let customer = c
        .items
        .iter()
        .find(|i| i.label == "Sales.Customer")
        .expect("expected a `Sales.Customer` table completion");
    assert_eq!(customer.kind, CompletionItemKind::Table);
    assert_eq!(
        customer.insert_text, "Sales.Customer",
        "insert_text must keep the schema, not collapse to the bare name"
    );
    // Bare-name metadata is still available separately for clients that want it.
    assert_eq!(customer.name, "Customer");
    assert_eq!(customer.schema.as_deref(), Some("Sales"));
}

#[test]
fn general_context_object_insert_text_keeps_schema_qualifier() {
    // Same parity guarantee in the General context (SELECT list), where objects
    // are offered alongside in-scope columns.
    let snap = snapshot();
    let c = complete_at("SELECT {cursor} FROM Sales.Customer c", &snap);
    dump("general: schema-qualified object insert_text", &c);
    let order = c
        .items
        .iter()
        .find(|i| i.label == "Sales.SalesOrder")
        .expect("expected a `Sales.SalesOrder` object completion");
    assert_eq!(
        order.insert_text, "Sales.SalesOrder",
        "general-context objects must also insert the schema-qualified name"
    );
}

#[test]
fn fk_join_candidate_insert_text_intentionally_differs_from_label() {
    // The label/insert_text parity invariant exempts FK-aware join candidates on
    // purpose: the label is a readable `Target ON <conds>` while the insert adds
    // the generated alias and the alias-qualified ON clause. This test documents
    // and pins that intentional divergence so the exemption never silently masks
    // a real regression.
    let snap = snapshot();
    let c = complete_at(
        "SELECT * FROM Sales.SalesOrder so INNER JOIN {cursor}",
        &snap,
    );
    dump("from/join: FK insert_text carries alias + ON clause", &c);
    let fk = c
        .items
        .iter()
        .find(|i| i.detail.as_deref() == Some("fk") && i.label.starts_with("Sales.Customer ON"))
        .expect("expected an FK join candidate to Sales.Customer");
    assert_ne!(
        fk.insert_text, fk.label,
        "FK join insert_text is expected to differ from its label"
    );
    assert!(
        fk.insert_text.starts_with("Sales.Customer "),
        "FK insert must start with the qualified target, got: {}",
        fk.insert_text
    );
    assert!(
        fk.insert_text.contains(" ON "),
        "FK insert must carry an alias-qualified ON clause, got: {}",
        fk.insert_text
    );
}

#[test]
fn type_showcase_exposes_one_column_per_type_class() {
    let snap = snapshot();
    let c = complete_at("SELECT * FROM Sales.TypeShowcase t WHERE t.{cursor}", &snap);
    dump("alias-dot: one column per TypeClass", &c);
    assert_has(
        &c,
        &[
            "IntegerCol",
            "DecimalCol",
            "FloatCol",
            "CharacterCol",
            "BinaryCol",
            "DateTimeCol",
            "BooleanCol",
            "UuidCol",
            "JsonCol",
            "XmlCol",
            "SpatialCol",
            "RowIdCol",
            "IntervalCol",
            "OtherCol",
        ],
    );
    // 14 TypeClass variants -> 14 columns, all of kind Column.
    assert_eq!(labels_of(&c, CompletionItemKind::Column).len(), 14);
}

#[test]
fn each_test_gets_an_independent_schema_copy() {
    // Mutating one snapshot's source vec doesn't affect another: `test_schema()`
    // builds a fresh Vec every call, and `apply` clones into the snapshot.
    let a = snapshot();
    let b = snapshot();
    assert_eq!(a.len(), b.len());
    assert!(
        a.len() >= 13,
        "expected the full cookbook schema, got {}",
        a.len()
    );
}

#[test]
fn from_only_offers_tables_and_views_inline() {
    let snap = snapshot();
    // Typing `Sales` after FROM matches Sales.SalesOrder (its bare name starts
    // with "Sales") but not the unused Archive/Staging tables, whose names
    // don't contain that substring. (Matching is contains-based, so pick a
    // discriminating prefix.)
    let c = complete_at("SELECT * FROM {cursor}", &snap);
    dump("from: prefix `Sales` filtering", &c);
    // check for explicit inclusion of a known table, to verify the filter isn't too aggressive
    assert_has(&c, &["Sales.SalesOrder"]);
    assert_has(&c, &["HR.Employee"]);
    assert_has(&c, &["HR.Department"]);
    assert_has(&c, &["Sales.vCustomerOrders"]);

    // get all the procedure types by looking at the kinds of the items make that a collection
    let proc_kinds: Vec<CompletionItemKind> = c.items.iter().map(|i| i.kind).collect();
    // should have tables and view
    assert!(proc_kinds.contains(&CompletionItemKind::Table));
    assert!(proc_kinds.contains(&CompletionItemKind::View));
    assert!(!proc_kinds.contains(&CompletionItemKind::Procedure));
    assert!(!proc_kinds.contains(&CompletionItemKind::Function));
}

#[test]
fn from_only_offers_tables_and_views_newline() {
    let snap = snapshot();
    // Typing `Sales` after FROM matches Sales.SalesOrder (its bare name starts
    // with "Sales") but not the unused Archive/Staging tables, whose names
    // don't contain that substring. (Matching is contains-based, so pick a
    // discriminating prefix.)
    let c = complete_at(
        "SELECT \
    * \
FROM \
    {cursor}",
        &snap,
    );
    // check for explicit inclusion of a known table, to verify the filter isn't too aggressive
    assert_has(&c, &["Sales.SalesOrder"]);
    assert_has(&c, &["HR.Employee"]);
    assert_has(&c, &["HR.Department"]);
    assert_has(&c, &["Sales.vCustomerOrders"]);

    // get all the procedure types by looking at the kinds of the items make that a collection
    let proc_kinds: Vec<CompletionItemKind> = c.items.iter().map(|i| i.kind).collect();
    // should have tables and view
    assert!(proc_kinds.contains(&CompletionItemKind::Table));
    assert!(proc_kinds.contains(&CompletionItemKind::View));
    assert!(!proc_kinds.contains(&CompletionItemKind::Procedure));
    assert!(!proc_kinds.contains(&CompletionItemKind::Function));
    assert!(!proc_kinds.contains(&CompletionItemKind::Column));
    assert!(!proc_kinds.contains(&CompletionItemKind::Value));
    assert!(!proc_kinds.contains(&CompletionItemKind::Keyword));
}
fn assert_only_keywords(c: &Completions, expected: &[&str]) {
    // After a finished SELECT list the clause keywords lead, followed only by
    // the table-like objects whose insert writes the whole `FROM` clause
    // (feedback ifb_9f91). Nothing else — no routines, columns or values.
    let kinds: Vec<CompletionItemKind> = c.items.iter().map(|i| i.kind).collect();

    assert!(!kinds.contains(&CompletionItemKind::Procedure));
    assert!(!kinds.contains(&CompletionItemKind::Function));
    assert!(!kinds.contains(&CompletionItemKind::Column));
    assert!(!kinds.contains(&CompletionItemKind::Value));
    assert!(kinds.contains(&CompletionItemKind::Keyword));

    let keyword_count = kinds
        .iter()
        .take_while(|k| **k == CompletionItemKind::Keyword)
        .count();
    let mut keywords: Vec<String> = c.items[..keyword_count]
        .iter()
        .map(|i| i.label.clone())
        .collect();

    keywords.sort();

    let mut expected: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
    expected.sort();

    assert_eq!(keywords, expected, "the clause keywords must lead");

    for it in &c.items[keyword_count..] {
        assert!(
            matches!(
                it.kind,
                CompletionItemKind::Table | CompletionItemKind::View
            ),
            "unexpected {:?} `{}` after a finished select list",
            it.kind,
            it.label
        );
        assert!(
            it.insert_text.starts_with("FROM\n") && it.insert_text.ends_with(&it.label),
            "`{}` should insert its FROM clause, inserted `{}`",
            it.label,
            it.insert_text
        );
    }
}

#[test]
fn select_star_offers_from_and_into_else_new_line() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT \
    * \
 {cursor}",
        &snap,
    );

    dump(
        "top-level SELECT * newline should offer only FROM and INTO",
        &c,
    );

    assert_only_keywords(&c, &["FROM", "INTO"]);
}

#[test]
fn select_star_offers_from_and_into_else_new_inline() {
    let snap = snapshot();

    let c = complete_at("SELECT * {cursor}", &snap);

    dump(
        "top-level SELECT * inline should offer only FROM and INTO",
        &c,
    );

    assert_only_keywords(&c, &["FROM", "INTO"]);
}

#[test]
fn select_star_inside_view_only_offers_from_not_into() {
    let snap = snapshot();

    let c = complete_at(
        "CREATE VIEW dbo.vTest AS
         SELECT * {cursor}",
        &snap,
    );

    dump("inside view should only offer FROM", &c);

    assert_only_keywords(&c, &["FROM"]);
}

#[test]
fn select_star_inside_function_only_offers_from_not_into() {
    let snap = snapshot();

    let c = complete_at(
        "CREATE FUNCTION dbo.fTest()
         RETURNS TABLE
         AS
         RETURN
         (
             SELECT * {cursor}
         )",
        &snap,
    );

    dump("inside function should only offer FROM", &c);

    assert_only_keywords(&c, &["FROM"]);
}

#[test]
fn select_star_inside_cte_only_offers_from_not_into() {
    let snap = snapshot();

    let c = complete_at(
        "WITH cte AS
         (
             SELECT * {cursor}
         )
         SELECT *
         FROM cte",
        &snap,
    );

    dump("inside CTE should only offer FROM", &c);

    assert_only_keywords(&c, &["FROM"]);
}

#[test]
fn select_star_inside_subquery_only_offers_from_not_into() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT *
         FROM
         (
             SELECT * {cursor}
         ) x",
        &snap,
    );

    dump("inside subquery should only offer FROM", &c);

    assert_only_keywords(&c, &["FROM"]);
}

#[test]
fn select_star_inside_procedure_offers_from_and_into() {
    let snap = snapshot();

    let c = complete_at(
        "CREATE PROCEDURE dbo.pTest
         AS
         BEGIN
             SELECT * {cursor}
         END",
        &snap,
    );

    dump("inside procedure should offer only FROM and INTO", &c);

    assert_only_keywords(&c, &["FROM", "INTO"]);
}

const JOIN_KEYWORDS: &[&str] = &[
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "LEFT OUTER JOIN",
    "RIGHT JOIN",
    "RIGHT OUTER JOIN",
    "FULL JOIN",
    "FULL OUTER JOIN",
];

fn assert_missing_keywords(c: &Completions, unexpected: &[&str]) {
    let keywords = keyword_labels(c);

    for unexpected_keyword in unexpected {
        assert!(
            !keywords.iter().any(|k| k == unexpected_keyword),
            "unexpected keyword `{}` was offered. actual keywords: {:?}",
            unexpected_keyword,
            keywords
        );
    }
}

fn assert_has_all_join_keywords(c: &Completions) {
    assert_has_keywords(c, JOIN_KEYWORDS);
}

fn assert_missing_all_join_keywords(c: &Completions) {
    assert_missing_keywords(c, JOIN_KEYWORDS);
}

fn assert_keyword_is_first(c: &Completions, expected_first: &str) {
    let keywords = keyword_labels(c);

    assert_eq!(
        keywords.first().map(|s| s.as_str()),
        Some(expected_first),
        "expected `{}` to be the first keyword. actual keywords: {:?}",
        expected_first,
        keywords
    );
}

#[allow(dead_code)]
fn assert_keyword_before(c: &Completions, earlier: &str, later: &str) {
    let keywords = keyword_labels(c);

    let earlier_index = keywords
        .iter()
        .position(|k| k == earlier)
        .unwrap_or_else(|| {
            panic!(
                "expected keyword `{}` was not offered. actual keywords: {:?}",
                earlier, keywords
            )
        });

    let later_index = keywords.iter().position(|k| k == later).unwrap_or_else(|| {
        panic!(
            "expected keyword `{}` was not offered. actual keywords: {:?}",
            later, keywords
        )
    });

    assert!(
        earlier_index < later_index,
        "expected `{}` to appear before `{}`. actual keywords: {:?}",
        earlier,
        later,
        keywords
    );
}

fn assert_no_non_keyword_suggestions(c: &Completions) {
    // After a FROM source the clause keywords lead, followed only by the
    // in-scope columns whose insert writes the WHERE clause (feedback
    // ifb_92d9). No catalog objects or values.
    let kinds: Vec<CompletionItemKind> = c.items.iter().map(|i| i.kind).collect();

    assert!(!kinds.contains(&CompletionItemKind::Table));
    assert!(!kinds.contains(&CompletionItemKind::View));
    assert!(!kinds.contains(&CompletionItemKind::Procedure));
    assert!(!kinds.contains(&CompletionItemKind::Function));
    assert!(!kinds.contains(&CompletionItemKind::Value));
    assert!(kinds.contains(&CompletionItemKind::Keyword));

    let keyword_count = kinds
        .iter()
        .take_while(|k| **k == CompletionItemKind::Keyword)
        .count();
    for it in &c.items[keyword_count..] {
        assert_eq!(
            it.kind,
            CompletionItemKind::Column,
            "unexpected {:?} `{}` among the post-FROM columns",
            it.kind,
            it.label
        );
        assert!(
            it.insert_text.starts_with("WHERE\n") && it.insert_text.ends_with(&it.label),
            "`{}` should insert its WHERE clause, inserted `{}`",
            it.label,
            it.insert_text
        );
    }
}

#[test]
fn from_table_with_alias_inline_offers_join_where_group_by_order_by() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.SalesOrder so {cursor}",
        &snap,
    );

    dump(
        "from table with alias inline should offer post-FROM keywords",
        &c,
    );

    assert_no_non_keyword_suggestions(&c);
    assert_has_all_join_keywords(&c);
    assert_has_keywords(&c, &["WHERE", "GROUP BY", "ORDER BY"]);
}

#[test]
fn from_table_with_alias_newline_offers_join_where_group_by_order_by() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.SalesOrder so
         {cursor}",
        &snap,
    );

    dump(
        "from table with alias newline should offer post-FROM keywords",
        &c,
    );

    assert_no_non_keyword_suggestions(&c);
    assert_has_all_join_keywords(&c);
    assert_has_keywords(&c, &["WHERE", "GROUP BY", "ORDER BY"]);
}

#[test]
fn from_table_without_alias_inline_offers_join_where_group_by_order_by() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.SalesOrder {cursor}",
        &snap,
    );

    dump(
        "from table without alias inline should offer post-FROM keywords",
        &c,
    );

    assert_no_non_keyword_suggestions(&c);
    assert_has_all_join_keywords(&c);
    assert_has_keywords(&c, &["WHERE", "GROUP BY", "ORDER BY"]);
}

#[test]
fn from_table_without_alias_newline_offers_join_where_group_by_order_by() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.SalesOrder
         {cursor}",
        &snap,
    );

    dump(
        "from table without alias newline should offer post-FROM keywords",
        &c,
    );

    assert_no_non_keyword_suggestions(&c);
    assert_has_all_join_keywords(&c);
    assert_has_keywords(&c, &["WHERE", "GROUP BY", "ORDER BY"]);
}

#[test]
fn from_table_with_aggregate_puts_group_by_right_after_where() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            so.CustomerId,
            COUNT(*) AS OrderCount
         FROM
            Sales.SalesOrder so {cursor}",
        &snap,
    );

    dump(
        "aggregate select should prioritize GROUP BY after FROM table",
        &c,
    );

    assert_no_non_keyword_suggestions(&c);
    assert_has_keywords(&c, &["GROUP BY", "WHERE", "ORDER BY"]);
    // WHERE always leads (feedback ifb_92d9); GROUP BY is promoted to just
    // after it.
    assert_keyword_is_first(&c, "WHERE");
    assert_eq!(
        keyword_labels(&c).get(1).map(String::as_str),
        Some("GROUP BY")
    );
}

#[test]
fn from_table_with_aggregate_and_newline_puts_group_by_right_after_where() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            so.CustomerId,
            COUNT(*) AS OrderCount
         FROM
            Sales.SalesOrder so
         {cursor}",
        &snap,
    );

    dump(
        "aggregate select newline should prioritize GROUP BY after FROM table",
        &c,
    );

    assert_no_non_keyword_suggestions(&c);
    assert_has_keywords(&c, &["GROUP BY", "WHERE", "ORDER BY"]);
    // WHERE always leads (feedback ifb_92d9); GROUP BY is promoted to just
    // after it.
    assert_keyword_is_first(&c, "WHERE");
    assert_eq!(
        keyword_labels(&c).get(1).map(String::as_str),
        Some("GROUP BY")
    );
}

#[test]
fn from_table_with_no_foreign_keys_does_not_offer_join_keywords() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Staging.ImportBatch ib {cursor}",
        &snap,
    );

    dump(
        "table without foreign keys should not offer JOIN keywords",
        &c,
    );

    assert_no_non_keyword_suggestions(&c);
    assert_missing_all_join_keywords(&c);
    assert_has_keywords(&c, &["WHERE", "GROUP BY", "ORDER BY"]);
}

#[test]
fn from_table_with_no_foreign_keys_puts_group_by_right_after_where() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Staging.ImportBatch ib {cursor}",
        &snap,
    );

    dump("table without foreign keys should prioritize GROUP BY", &c);

    assert_no_non_keyword_suggestions(&c);
    assert_missing_all_join_keywords(&c);
    assert_has_keywords(&c, &["GROUP BY", "WHERE", "ORDER BY"]);
    // WHERE always leads (feedback ifb_92d9); GROUP BY is promoted to just
    // after it.
    assert_keyword_is_first(&c, "WHERE");
    assert_eq!(
        keyword_labels(&c).get(1).map(String::as_str),
        Some("GROUP BY")
    );
}

#[test]
fn from_table_inside_union_branch_does_not_offer_order_by() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.SalesOrder so {cursor}
         UNION
         SELECT
            *
         FROM
            Sales.SalesOrderArchive soa",
        &snap,
    );

    dump("inside UNION branch should not offer ORDER BY", &c);

    assert_no_non_keyword_suggestions(&c);
    assert_has_keywords(&c, &["WHERE", "GROUP BY"]);
    assert_missing_keywords(&c, &["ORDER BY"]);
}

#[test]
fn from_table_inside_union_branch_still_offers_safe_join_where_group_by() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.SalesOrder so {cursor}
         UNION
         SELECT
            *
         FROM
            Sales.SalesOrderArchive soa",
        &snap,
    );

    dump(
        "inside UNION branch should offer safe post-FROM keywords",
        &c,
    );

    assert_no_non_keyword_suggestions(&c);
    assert_has_all_join_keywords(&c);
    assert_has_keywords(&c, &["WHERE", "GROUP BY"]);
    assert_missing_keywords(&c, &["ORDER BY"]);
}

#[test]
fn from_table_after_union_allows_order_by_at_final_query_tail() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.SalesOrder so
         UNION
         SELECT
            *
         FROM
            Sales.SalesOrderArchive soa {cursor}",
        &snap,
    );

    dump("final UNION query tail should allow ORDER BY", &c);

    assert_no_non_keyword_suggestions(&c);
    assert_has_keywords(&c, &["WHERE", "GROUP BY", "ORDER BY"]);
}

const FK_GATED_JOIN_KEYWORDS: &[&str] = &[
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "LEFT OUTER JOIN",
    "RIGHT JOIN",
    "RIGHT OUTER JOIN",
    "FULL JOIN",
    "FULL OUTER JOIN",
];

fn keyword_labels(c: &Completions) -> Vec<String> {
    c.items
        .iter()
        .filter(|i| i.kind == CompletionItemKind::Keyword)
        .map(|i| i.label.clone())
        .collect()
}

fn assert_has_keywords(c: &Completions, expected: &[&str]) {
    let keywords = keyword_labels(c);

    for expected_keyword in expected {
        assert!(
            keywords.iter().any(|k| k == expected_keyword),
            "expected keyword `{}` was not offered. actual keywords: {:?}",
            expected_keyword,
            keywords
        );
    }
}

fn assert_has_fk_gated_join_keywords(c: &Completions) {
    assert_has_keywords(c, FK_GATED_JOIN_KEYWORDS);
}

fn assert_missing_fk_gated_join_keywords(c: &Completions) {
    assert_missing_keywords(c, FK_GATED_JOIN_KEYWORDS);
}

fn assert_has_post_from_keywords(c: &Completions) {
    assert_has_keywords(c, &["WHERE", "GROUP BY", "ORDER BY"]);
}

#[test]
fn sales_order_with_alias_offers_joins_for_outgoing_fk() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.SalesOrder so {cursor}",
        &snap,
    );

    dump(
        "Sales.SalesOrder has outgoing FK to Sales.Customer, so JOIN keywords should be offered",
        &c,
    );

    assert_has_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn sales_order_without_alias_offers_joins_for_outgoing_fk() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.SalesOrder {cursor}",
        &snap,
    );

    dump(
        "Sales.SalesOrder without alias has outgoing FK, so JOIN keywords should be offered",
        &c,
    );

    assert_has_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn sales_order_with_alias_newline_offers_joins_for_outgoing_fk() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.SalesOrder so
         {cursor}",
        &snap,
    );

    dump(
        "Sales.SalesOrder alias newline has outgoing FK, so JOIN keywords should be offered",
        &c,
    );

    assert_has_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn sales_customer_with_alias_offers_joins_for_incoming_fks() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.Customer c {cursor}",
        &snap,
    );

    dump(
        "Sales.Customer has incoming FKs from Sales.SalesOrder and HR.Employee, so JOIN keywords should be offered",
        &c,
    );

    assert_has_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn sales_customer_without_alias_offers_joins_for_incoming_fks() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.Customer {cursor}",
        &snap,
    );

    dump(
        "Sales.Customer without alias has incoming FKs, so JOIN keywords should be offered",
        &c,
    );

    assert_has_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn hr_employee_with_alias_offers_joins_for_multiple_outgoing_fks() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            HR.Employee e {cursor}",
        &snap,
    );

    dump(
        "HR.Employee has outgoing FKs to HR.Department and Sales.Customer, so JOIN keywords should be offered",
        &c,
    );

    assert_has_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn hr_department_with_alias_offers_joins_for_incoming_fk() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            HR.Department d {cursor}",
        &snap,
    );

    dump(
        "HR.Department has incoming FK from HR.Employee, so JOIN keywords should be offered",
        &c,
    );

    assert_has_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn archive_archived_order_with_alias_does_not_offer_joins_without_fks() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Archive.ArchivedOrder ao {cursor}",
        &snap,
    );

    dump(
        "Archive.ArchivedOrder has no incoming or outgoing FKs, so JOIN keywords should not be offered",
        &c,
    );

    assert_missing_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn archive_archived_order_without_alias_does_not_offer_joins_without_fks() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Archive.ArchivedOrder {cursor}",
        &snap,
    );

    dump(
        "Archive.ArchivedOrder without alias has no incoming or outgoing FKs, so JOIN keywords should not be offered",
        &c,
    );

    assert_missing_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn staging_raw_customer_import_with_alias_does_not_offer_joins_without_fks() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Staging.RawCustomerImport rci {cursor}",
        &snap,
    );

    dump(
        "Staging.RawCustomerImport has no incoming or outgoing FKs, so JOIN keywords should not be offered",
        &c,
    );

    assert_missing_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn staging_raw_customer_import_without_alias_does_not_offer_joins_without_fks() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Staging.RawCustomerImport {cursor}",
        &snap,
    );

    dump(
        "Staging.RawCustomerImport without alias has no incoming or outgoing FKs, so JOIN keywords should not be offered",
        &c,
    );

    assert_missing_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn sales_view_with_alias_does_not_offer_joins_without_fk_metadata() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Sales.vCustomerOrders vco {cursor}",
        &snap,
    );

    dump(
        "Sales.vCustomerOrders is a view with no FK metadata, so FK-gated JOIN keywords should not be offered",
        &c,
    );

    assert_missing_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

#[test]
fn hr_view_with_alias_does_not_offer_joins_without_fk_metadata() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            HR.vDepartmentRoster vdr {cursor}",
        &snap,
    );

    dump(
        "HR.vDepartmentRoster is a view with no FK metadata, so FK-gated JOIN keywords should not be offered",
        &c,
    );

    assert_missing_fk_gated_join_keywords(&c);
    assert_has_post_from_keywords(&c);
}

// ===========================================================================
// Passive completion metadata.
// ---------------------------------------------------------------------------
// Every offering carries structural metadata (name/why/parent/schema/alias/
// type/value) alongside the canonical label/insert_text/kind. These never
// affect ranking; they describe *what* an item is and *why* it was offered.
// ===========================================================================

/// Find the first completion item whose label exactly matches `label`.
fn item_with_label<'a>(
    c: &'a Completions,
    label: &str,
) -> &'a sqlly_intel::modelengine::intellisense::CompletionItem {
    c.items
        .iter()
        .find(|i| i.label == label)
        .unwrap_or_else(|| {
            panic!(
                "no completion item with label `{label}`.\n  got: {:#?}",
                labels(c)
            )
        })
}

#[test]
fn metadata_table_object_carries_name_schema_and_why() {
    let snap = snapshot();
    // Object-name context after FROM: bare-name contains-match on `Cust`.
    let c = complete_at("SELECT * FROM Cust{cursor}", &snap);
    let item = item_with_label(&c, "Sales.Customer");
    assert_eq!(item.name, "Customer", "name is the bare object name");
    assert_eq!(item.schema.as_deref(), Some("Sales"));
    assert_eq!(item.why.as_deref(), Some("table"));
    assert!(item.parent.is_none(), "a table has no parent");
    assert!(item.value.is_none());
}

#[test]
fn metadata_alias_dot_column_carries_parent_alias_and_type() {
    let snap = snapshot();
    let c = complete_at("SELECT * FROM Sales.Customer c WHERE c.{cursor}", &snap);
    let item = item_with_label(&c, "FullName");
    assert_eq!(item.name, "FullName");
    assert_eq!(item.kind, CompletionItemKind::Column);
    assert_eq!(item.why.as_deref(), Some("alias"));
    assert_eq!(item.alias.as_deref(), Some("c"));
    assert_eq!(
        item.parent.as_deref(),
        Some("Customer"),
        "column's parent is its table"
    );
    assert_eq!(item.parent_type, Some(CompletionItemKind::Table));
    assert_eq!(item.schema.as_deref(), Some("Sales"));
    assert_eq!(item.r#type.as_deref(), Some("nvarchar(200)"));
}

#[test]
fn metadata_scoped_column_unaliased_uses_scope_reason_and_parent() {
    let snap = snapshot();
    // Unaliased table in scope -> bare column names, why = "scope".
    let c = complete_at("SELECT Full{cursor} FROM Sales.Customer", &snap);
    let item = item_with_label(&c, "FullName");
    assert_eq!(item.name, "FullName");
    assert_eq!(item.why.as_deref(), Some("scope"));
    assert!(item.alias.is_none(), "unaliased scope column has no alias");
    assert_eq!(item.parent.as_deref(), Some("Customer"));
    assert_eq!(item.parent_type, Some(CompletionItemKind::Table));
    assert_eq!(item.r#type.as_deref(), Some("nvarchar(200)"));
}

#[test]
fn metadata_keyword_carries_name_and_keyword_reason() {
    let snap = snapshot();
    // Post-FROM context emits Keyword-kind items (WHERE / GROUP BY / ...).
    let c = complete_at("SELECT * FROM Sales.Customer {cursor}", &snap);
    let item = item_with_label(&c, "WHERE");
    assert_eq!(item.kind, CompletionItemKind::Keyword);
    assert_eq!(item.name, "WHERE");
    assert_eq!(item.why.as_deref(), Some("keyword"));
    assert!(item.parent.is_none());
    assert!(item.schema.is_none());
}

#[test]
fn metadata_fk_join_candidate_carries_fk_reason_alias_and_schema() {
    let snap = snapshot();
    // From a present table, completing a join target surfaces FK candidates.
    let c = complete_at("SELECT * FROM Sales.SalesOrder JOIN {cursor}", &snap);
    // The outgoing FK SalesOrder.CustomerId -> Sales.Customer yields a join
    // candidate whose detail is "fk".
    let item = c
        .items
        .iter()
        .find(|i| i.detail.as_deref() == Some("fk"))
        .expect("expected at least one FK join candidate");
    assert_eq!(item.why.as_deref(), Some("fk"));
    assert!(
        item.alias.is_some(),
        "FK join candidate carries a generated alias"
    );
    assert_eq!(item.kind, CompletionItemKind::Table);
    assert!(!item.name.is_empty());
}

#[test]
fn metadata_exec_arg_completes_routine_parameters() {
    let snap = snapshot();
    // After `EXEC <proc> @`, the offering is the routine's parameters, each
    // carrying the routine as parent and its declared data type.
    let c = complete_at("EXEC HR.pAddEmployee @{cursor}", &snap);
    let names: Vec<&str> = c.items.iter().map(|i| i.name.as_str()).collect();
    assert!(
        names.contains(&"@FullName") && names.contains(&"@DepartmentId"),
        "expected routine params; got {names:?}"
    );
    let item = item_with_label(&c, "@FullName");
    assert_eq!(item.kind, CompletionItemKind::Parameter);
    assert_eq!(item.why.as_deref(), Some("parameter"));
    assert_eq!(item.name, "@FullName");
    assert_eq!(item.parent.as_deref(), Some("pAddEmployee"));
    assert_eq!(item.parent_type, Some(CompletionItemKind::Procedure));
    assert_eq!(item.schema.as_deref(), Some("HR"));
    assert_eq!(item.r#type.as_deref(), Some("nvarchar(150)"));
}

fn object_labels(c: &Completions) -> Vec<String> {
    c.items.iter().map(|i| i.label.clone()).collect()
}

fn assert_missing_completion_label(c: &Completions, unexpected: &str) {
    let labels = object_labels(c);

    assert!(
        !labels.iter().any(|label| label == unexpected),
        "unexpected completion `{}` was offered. actual labels: {:?}",
        unexpected,
        labels
    );
}

fn assert_has_table(c: &Completions, expected_label: &str) {
    assert!(
        c.items
            .iter()
            .any(|i| { i.kind == CompletionItemKind::Table && i.label == expected_label }),
        "expected table `{}` was not offered. actual items: {:?}",
        expected_label,
        c.items
    );
}

#[test]
fn from_partial_dep_matches_hr_department() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Dep{cursor}", &snap);

    dump("partial table name Dep should match HR.Department", &c);

    assert_has_table(&c, "HR.Department");
}

#[test]
fn from_partial_dep_does_not_match_unrelated_tables() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Dep{cursor}", &snap);

    dump(
        "partial table name Dep should not match unrelated tables",
        &c,
    );

    assert_has_table(&c, "HR.Department");

    assert_missing_completion_label(&c, "Sales.Customer");
    assert_missing_completion_label(&c, "Sales.SalesOrder");
    assert_missing_completion_label(&c, "HR.Employee");
    assert_missing_completion_label(&c, "Archive.ArchivedOrder");
    assert_missing_completion_label(&c, "Staging.RawCustomerImport");
}

#[test]
fn from_partial_dep_newline_matches_hr_department() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            Dep{cursor}",
        &snap,
    );

    dump(
        "partial table name Dep on newline should match HR.Department",
        &c,
    );

    assert_has_table(&c, "HR.Department");
}

#[test]
fn from_partial_dep_is_case_insensitive() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM dep{cursor}", &snap);

    dump(
        "partial table name dep should match HR.Department case-insensitively",
        &c,
    );

    assert_has_table(&c, "HR.Department");
}

fn assert_has_item(c: &Completions, kind: CompletionItemKind, expected_label: &str) {
    assert!(
        c.items
            .iter()
            .any(|i| i.kind == kind && i.label == expected_label),
        "expected {:?} `{}` was not offered. actual labels: {:?}",
        kind,
        expected_label,
        labels(c)
    );
}

fn assert_missing_label(c: &Completions, unexpected_label: &str) {
    assert!(
        !c.items.iter().any(|i| i.label == unexpected_label),
        "unexpected completion `{}` was offered. actual labels: {:?}",
        unexpected_label,
        labels(c)
    );
}

fn assert_missing_kind(c: &Completions, unexpected_kind: CompletionItemKind) {
    assert!(
        !c.items.iter().any(|i| i.kind == unexpected_kind),
        "unexpected kind `{:?}` was offered. actual items: {:?}",
        unexpected_kind,
        c.items
    );
}

#[test]
fn from_hr_schema_dot_lists_hr_tables_and_views() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM HR.{cursor}", &snap);

    dump("HR schema dot should list HR tables and views", &c);

    assert_has_item(&c, CompletionItemKind::Table, "HR.Department");
    assert_has_item(&c, CompletionItemKind::Table, "HR.Employee");
    assert_has_item(&c, CompletionItemKind::View, "HR.vDepartmentRoster");
}

#[test]
fn from_hr_schema_dot_newline_lists_hr_tables_and_views() {
    let snap = snapshot();

    let c = complete_at(
        "SELECT
            *
         FROM
            HR.{cursor}",
        &snap,
    );

    dump(
        "HR schema dot on newline should list HR tables and views",
        &c,
    );

    assert_has_item(&c, CompletionItemKind::Table, "HR.Department");
    assert_has_item(&c, CompletionItemKind::Table, "HR.Employee");
    assert_has_item(&c, CompletionItemKind::View, "HR.vDepartmentRoster");
}

#[test]
fn from_hr_schema_dot_does_not_list_other_schema_objects() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM HR.{cursor}", &snap);

    dump("HR schema dot should not list non-HR tables or views", &c);

    assert_missing_label(&c, "Sales.Customer");
    assert_missing_label(&c, "Sales.SalesOrder");
    assert_missing_label(&c, "Sales.vCustomerOrders");
    assert_missing_label(&c, "Archive.ArchivedOrder");
    assert_missing_label(&c, "Staging.RawCustomerImport");
}

#[test]
fn from_hr_schema_dot_does_not_list_procedures_functions_or_columns() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM HR.{cursor}", &snap);

    dump(
        "HR schema dot in FROM should only list table-like objects",
        &c,
    );

    assert_missing_label(&c, "HR.pAddEmployee");
    assert_missing_label(&c, "HR.fnEmployeeFullName");

    assert_missing_kind(&c, CompletionItemKind::Procedure);
    assert_missing_kind(&c, CompletionItemKind::Function);
    assert_missing_kind(&c, CompletionItemKind::Column);
    assert_missing_kind(&c, CompletionItemKind::Value);
}

#[test]
fn from_hr_schema_dot_partial_d_ranks_hr_department_first() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM HR.D{cursor}", &snap);

    dump("HR.D should rank HR.Department first", &c);

    assert_has_item(&c, CompletionItemKind::Table, "HR.Department");

    // Schema-dot is a contains search (feedback ifb_2ff4): the view offers
    // too, below the prefix hit.
    let position = |label: &str| c.items.iter().position(|i| i.label == label);
    assert!(position("HR.Department") < position("HR.vDepartmentRoster"));
    assert!(position("HR.vDepartmentRoster").is_some());
    assert_missing_label(&c, "HR.Employee");
    assert_missing_label(&c, "Sales.Customer");
    assert_missing_label(&c, "Sales.SalesOrder");
}

#[test]
fn from_partial_saor_matches_sales_sales_order() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM SaOr{cursor}", &snap);

    dump("SaOr should fuzzy-match Sales.SalesOrder", &c);

    assert_has_item(&c, CompletionItemKind::Table, "Sales.SalesOrder");
}

#[test]
fn from_schema_qualified_partial_saor_matches_sales_sales_order() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Sales.SaOr{cursor}", &snap);

    dump("Sales.SaOr should fuzzy-match Sales.SalesOrder", &c);

    assert_has_item(&c, CompletionItemKind::Table, "Sales.SalesOrder");
}

#[test]
fn from_acronym_sso_matches_sales_sales_order() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM SSO{cursor}", &snap);

    dump("SSO should acronym-match Sales.SalesOrder", &c);

    assert_has_item(&c, CompletionItemKind::Table, "Sales.SalesOrder");
}

#[test]
fn from_mixed_schema_and_acronym_sasso_matches_sales_sales_order() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM SaSSO{cursor}", &snap);

    dump(
        "SaSSO should mixed fuzzy/acronym-match Sales.SalesOrder",
        &c,
    );

    assert_has_item(&c, CompletionItemKind::Table, "Sales.SalesOrder");
}

#[test]
fn from_mixed_schema_and_name_ssaor_matches_sales_sales_order() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM SSaOr{cursor}", &snap);

    dump("SSaOr should mixed fuzzy-match Sales.SalesOrder", &c);

    assert_has_item(&c, CompletionItemKind::Table, "Sales.SalesOrder");
}

#[test]
fn from_schema_qualified_acronym_so_matches_sales_sales_order() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Sales.SO{cursor}", &snap);

    dump(
        "Sales.SO should acronym-match Sales.SalesOrder within Sales schema",
        &c,
    );

    assert_has_item(&c, CompletionItemKind::Table, "Sales.SalesOrder");
}

#[test]
fn from_schema_qualified_partial_saor_does_not_match_other_sales_objects() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Sales.SaOr{cursor}", &snap);

    dump("Sales.SaOr should not match unrelated Sales objects", &c);

    assert_has_item(&c, CompletionItemKind::Table, "Sales.SalesOrder");

    assert_missing_label(&c, "Sales.Customer");
    assert_missing_label(&c, "Sales.vCustomerOrders");
    assert_missing_label(&c, "HR.Department");
    assert_missing_label(&c, "HR.Employee");
    assert_missing_label(&c, "HR.vDepartmentRoster");
    assert_missing_label(&c, "Archive.ArchivedOrder");
    assert_missing_label(&c, "Staging.RawCustomerImport");
}

#[test]
fn from_acronym_sso_does_not_match_unrelated_objects() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM SSO{cursor}", &snap);

    dump("SSO should not match unrelated objects", &c);

    assert_has_item(&c, CompletionItemKind::Table, "Sales.SalesOrder");

    assert_missing_label(&c, "Sales.Customer");
    assert_missing_label(&c, "HR.Department");
    assert_missing_label(&c, "HR.Employee");
    assert_missing_label(&c, "Archive.ArchivedOrder");
    assert_missing_label(&c, "Staging.RawCustomerImport");
}

#[test]
fn from_sales_schema_dot_lists_only_sales_tables_and_views() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Sales.{cursor}", &snap);

    dump(
        "Sales schema dot should list only Sales tables and views",
        &c,
    );

    assert_has_item(&c, CompletionItemKind::Table, "Sales.Customer");
    assert_has_item(&c, CompletionItemKind::Table, "Sales.SalesOrder");
    assert_has_item(&c, CompletionItemKind::Table, "Sales.TypeShowcase");
    assert_has_item(&c, CompletionItemKind::View, "Sales.vCustomerOrders");

    assert_missing_label(&c, "HR.Department");
    assert_missing_label(&c, "HR.Employee");
    assert_missing_label(&c, "HR.vDepartmentRoster");
    assert_missing_label(&c, "Archive.ArchivedOrder");
    assert_missing_label(&c, "Staging.RawCustomerImport");
}

#[test]
fn from_hr_schema_dot_lists_only_hr_tables_and_views() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM HR.{cursor}", &snap);

    dump("HR schema dot should list only HR tables and views", &c);

    assert_has_item(&c, CompletionItemKind::Table, "HR.Department");
    assert_has_item(&c, CompletionItemKind::Table, "HR.Employee");
    assert_has_item(&c, CompletionItemKind::View, "HR.vDepartmentRoster");

    assert_missing_label(&c, "Sales.Customer");
    assert_missing_label(&c, "Sales.SalesOrder");
    assert_missing_label(&c, "Sales.TypeShowcase");
    assert_missing_label(&c, "Sales.vCustomerOrders");
    assert_missing_label(&c, "Archive.ArchivedOrder");
    assert_missing_label(&c, "Staging.RawCustomerImport");
}

#[test]
fn from_archive_schema_dot_lists_only_archive_tables_and_views() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Archive.{cursor}", &snap);

    dump(
        "Archive schema dot should list only Archive tables and views",
        &c,
    );

    assert_has_item(&c, CompletionItemKind::Table, "Archive.ArchivedOrder");

    assert_missing_label(&c, "Sales.Customer");
    assert_missing_label(&c, "Sales.SalesOrder");
    assert_missing_label(&c, "HR.Department");
    assert_missing_label(&c, "HR.Employee");
    assert_missing_label(&c, "Staging.RawCustomerImport");
}

#[test]
fn from_staging_schema_dot_lists_only_staging_tables_and_views() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Staging.{cursor}", &snap);

    dump(
        "Staging schema dot should list only Staging tables and views",
        &c,
    );

    assert_has_item(&c, CompletionItemKind::Table, "Staging.RawCustomerImport");

    assert_missing_label(&c, "Sales.Customer");
    assert_missing_label(&c, "Sales.SalesOrder");
    assert_missing_label(&c, "HR.Department");
    assert_missing_label(&c, "HR.Employee");
    assert_missing_label(&c, "Archive.ArchivedOrder");
}

#[test]
fn from_sales_schema_dot_partial_saor_matches_only_sales_sales_order() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Sales.SaOr{cursor}", &snap);

    dump("Sales.SaOr should match only Sales.SalesOrder", &c);

    assert_has_item(&c, CompletionItemKind::Table, "Sales.SalesOrder");

    assert_missing_label(&c, "Sales.Customer");
    assert_missing_label(&c, "Sales.TypeShowcase");
    assert_missing_label(&c, "Sales.vCustomerOrders");
    assert_missing_label(&c, "HR.Department");
    assert_missing_label(&c, "HR.Employee");
    assert_missing_label(&c, "Archive.ArchivedOrder");
    assert_missing_label(&c, "Staging.RawCustomerImport");
}

#[test]
fn from_hr_schema_dot_partial_dep_ranks_hr_department_first() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM HR.Dep{cursor}", &snap);

    dump("HR.Dep should rank HR.Department first", &c);

    assert_has_item(&c, CompletionItemKind::Table, "HR.Department");

    // `Dep` is a prefix of `Department` and a word start inside
    // `vDepartmentRoster`: schema-dot is a contains search (feedback
    // ifb_2ff4), prefix hits first.
    let position = |label: &str| c.items.iter().position(|i| i.label == label);
    assert!(position("HR.Department") < position("HR.vDepartmentRoster"));
    assert!(position("HR.vDepartmentRoster").is_some());
    assert_missing_label(&c, "HR.Employee");
    assert_missing_label(&c, "Sales.Customer");
    assert_missing_label(&c, "Sales.SalesOrder");
}

#[test]
fn from_sales_schema_dot_does_not_cross_schema_match_acronym() {
    let snap = snapshot();

    // `RCI` is the acronym of `Staging.RawCustomerImport` and matches it when
    // typed unqualified — but qualified by `Sales.` it must stay inside Sales.
    let c = complete_at("SELECT * FROM Sales.RCI{cursor}", &snap);

    dump(
        "Sales.RCI must not cross-schema match Staging.RawCustomerImport",
        &c,
    );

    assert_missing_label(&c, "Staging.RawCustomerImport");
    assert_missing_label(&c, "Archive.ArchivedOrder");
    assert_missing_label(&c, "HR.Department");
    assert_missing_label(&c, "HR.Employee");
    assert_missing_label(&c, "HR.vDepartmentRoster");
}

#[test]
fn from_hr_schema_dot_does_not_cross_schema_match_sales_order() {
    let snap = snapshot();

    // `SaOr` matches `Sales.SalesOrder` unqualified, but `HR.` confines the
    // search to HR, so the Sales table must not cross over.
    let c = complete_at("SELECT * FROM HR.SaOr{cursor}", &snap);

    dump("HR.SaOr must not cross-schema match Sales.SalesOrder", &c);

    assert_missing_label(&c, "Sales.SalesOrder");
    assert_missing_label(&c, "Sales.Customer");
    assert_missing_label(&c, "Archive.ArchivedOrder");
    assert_missing_label(&c, "Staging.RawCustomerImport");
}

#[test]
fn from_schema_dot_does_not_offer_procedures_functions_columns_or_values() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Sales.{cursor}", &snap);

    dump(
        "schema dot in FROM should only offer table-like objects",
        &c,
    );

    assert_missing_label(&c, "Sales.pGetCustomerOrders");
    assert_missing_label(&c, "Sales.fnOrderTotal");

    assert_missing_kind(&c, CompletionItemKind::Procedure);
    assert_missing_kind(&c, CompletionItemKind::Function);
    assert_missing_kind(&c, CompletionItemKind::Column);
    assert_missing_kind(&c, CompletionItemKind::Value);
}

#[test]
fn from_schema_qualified_partial_sales_order_insert_text_is_table_name_only() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Sales.Sal{cursor}", &snap);

    dump(
        "Sales.Sal should complete Sales.SalesOrder but insert only SalesOrder",
        &c,
    );

    let item = c
        .items
        .iter()
        .find(|i| i.kind == CompletionItemKind::Table && i.label == "Sales.SalesOrder")
        .expect("expected Sales.SalesOrder table completion");

    assert_eq!(
        item.insert_text.as_str(),
        "SalesOrder",
        "schema-qualified completion should insert only the table name when schema is already typed"
    );
}

#[test]
fn from_schema_qualified_partial_customer_insert_text_is_table_name_only() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Sales.Cus{cursor}", &snap);

    dump(
        "Sales.Cus should complete Sales.Customer but insert only Customer",
        &c,
    );

    let item = c
        .items
        .iter()
        .find(|i| i.kind == CompletionItemKind::Table && i.label == "Sales.Customer")
        .expect("expected Sales.Customer table completion");

    assert_eq!(
        item.insert_text.as_str(),
        "Customer",
        "schema-qualified completion should insert only the table name when schema is already typed"
    );
}

#[test]
fn from_schema_qualified_partial_view_insert_text_is_view_name_only() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Sales.vCus{cursor}", &snap);

    dump(
        "Sales.vCus should complete Sales.vCustomerOrders but insert only vCustomerOrders",
        &c,
    );

    let item = c
        .items
        .iter()
        .find(|i| i.kind == CompletionItemKind::View && i.label == "Sales.vCustomerOrders")
        .expect("expected Sales.vCustomerOrders view completion");

    assert_eq!(
        item.insert_text.as_str(),
        "vCustomerOrders",
        "schema-qualified completion should insert only the view name when schema is already typed"
    );
}

#[test]
fn from_unqualified_partial_sales_order_insert_text_includes_schema() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Sal{cursor}", &snap);

    dump(
        "unqualified Sal should complete Sales.SalesOrder with full schema-qualified insert text",
        &c,
    );

    let item = c
        .items
        .iter()
        .find(|i| i.kind == CompletionItemKind::Table && i.label == "Sales.SalesOrder")
        .expect("expected Sales.SalesOrder table completion");

    assert_eq!(
        item.insert_text.as_str(),
        "Sales.SalesOrder",
        "unqualified completion should insert the full schema-qualified object name"
    );
}

#[test]
fn from_schema_qualified_partial_does_not_insert_duplicate_schema() {
    let snap = snapshot();

    let c = complete_at("SELECT * FROM Sales.Sal{cursor}", &snap);

    dump(
        "Sales.Sal should not produce insert text that duplicates the schema",
        &c,
    );

    let item = c
        .items
        .iter()
        .find(|i| i.kind == CompletionItemKind::Table && i.label == "Sales.SalesOrder")
        .expect("expected Sales.SalesOrder table completion");

    assert_ne!(
        item.insert_text.as_str(),
        "Sales.SalesOrder",
        "schema-qualified completion must not insert Sales.SalesOrder after Sales. or it becomes Sales.Sales.SalesOrder"
    );

    assert_ne!(
        item.insert_text.as_str(),
        "Sales.Sales.SalesOrder",
        "schema-qualified completion must never duplicate the schema"
    );
}

/// Over-limit tie-breaking: with more identically-labeled, identically-scored
/// candidates than SYMBOL_LIMIT, the returned page must be deterministic and
/// keep the first-inserted candidates — pinning that the top-N partition in
/// `complete_symbols` reproduces the old full stable sort exactly.
#[test]
fn over_limit_ties_resolve_deterministically() {
    let fragments = (0..600)
        .map(|i| ObjectFragment {
            reference: ObjectRef::new(
                None,
                Some(SqlName::new(format!("s{i:03}"), Dialect::SqlServer, false)),
                SqlName::new("Orders", Dialect::SqlServer, false),
            ),
            kind: ObjectKind::Table,
            dialect: Dialect::SqlServer,
            body: ObjectBody::Table(TableBody::default()),
            provenance: Provenance::Database {
                profile: "test".into(),
            },
            fidelity: Fidelity::Exact,
            references: Vec::new(),
        })
        .collect();
    let (snapshot, _) = ModelSnapshot::empty().apply(
        1,
        fragments,
        &std::collections::BTreeSet::new(),
        IncompleteRegions::default(),
    );

    let text = "SELECT * FROM Ord".to_string();
    let request = CompletionRequest {
        cursor: text.len(),
        text,
        dialect: Dialect::SqlServer,
        default_catalog: None,
        default_namespace: None,
        mode: CompletionMode::Symbol,
    };

    let first = complete_symbols(&request, &snapshot);
    let second = complete_symbols(&request, &snapshot);
    assert!(first.truncated);
    assert_eq!(first.items.len(), second.items.len());
    let schemas = |c: &Completions| c.items.iter().map(|i| i.schema.clone()).collect::<Vec<_>>();
    assert_eq!(
        schemas(&first),
        schemas(&second),
        "tie-heavy completion pages must be deterministic"
    );
    // Ties beyond (score, label) fall back to insertion order, so the lowest
    // schema numbers survive the cut.
    assert_eq!(first.items[0].schema.as_deref(), Some("s000"));
}

/// Table-valued functions are row sources: they complete after FROM, while
/// scalar and aggregate functions stay out of row-source positions.
#[test]
fn table_valued_functions_complete_after_from() {
    let mut fragments = test_schema();
    let mut tvf = func("Sales", "fOrderLines", Vec::new(), "int");
    tvf.kind = ObjectKind::TableFunction;
    let mut scalar = func("Sales", "fOrderTotal", Vec::new(), "int");
    scalar.kind = ObjectKind::ScalarFunction;
    fragments.push(tvf);
    fragments.push(scalar);
    let (snapshot, _) = ModelSnapshot::empty().apply(
        1,
        fragments,
        &std::collections::BTreeSet::new(),
        IncompleteRegions::default(),
    );

    let text = "SELECT * FROM fOrder".to_string();
    let request = CompletionRequest {
        cursor: text.len(),
        text,
        dialect: Dialect::SqlServer,
        default_catalog: None,
        default_namespace: None,
        mode: CompletionMode::Symbol,
    };
    let completions = complete_symbols(&request, &snapshot);
    let labels: Vec<&str> = completions.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.iter().any(|l| l.ends_with("fOrderLines")),
        "TVF must complete after FROM: {labels:?}"
    );
    assert!(
        !labels.iter().any(|l| l.ends_with("fOrderTotal")),
        "scalar function must not complete after FROM: {labels:?}"
    );
}

// ---------------------------------------------------------------------------
// Hump-matching / schema-search settings: each hump-dependent FROM case above
// with the setting on and off.
// ---------------------------------------------------------------------------

fn with_options<R>(hump_matching: bool, schema_in_search: bool, f: impl FnOnce() -> R) -> R {
    sqlly_intel::matching::with_match_options(
        sqlly_intel::matching::MatchOptions {
            hump_matching,
            schema_in_search,
        },
        f,
    )
}

fn has_label(c: &Completions, label: &str) -> bool {
    c.items.iter().any(|item| item.label == label)
}

#[test]
fn hump_queries_match_only_with_hump_matching_on() {
    let snap = snapshot();
    for query in [
        "SELECT * FROM SSO{cursor}",
        "SELECT * FROM SaOr{cursor}",
        "SELECT * FROM SaSSO{cursor}",
        "SELECT * FROM SSaOr{cursor}",
        "SELECT * FROM Sales.SaOr{cursor}",
    ] {
        with_options(true, true, || {
            assert!(
                has_label(&complete_at(query, &snap), "Sales.SalesOrder"),
                "on: {query}"
            );
        });
        with_options(false, true, || {
            assert!(
                !has_label(&complete_at(query, &snap), "Sales.SalesOrder"),
                "off: {query}"
            );
        });
    }
}

#[test]
fn case_insensitive_prefix_matches_with_hump_matching_on_or_off() {
    let snap = snapshot();
    for query in [
        "SELECT * FROM salesor{cursor}",
        "SELECT * FROM SALESORD{cursor}",
    ] {
        for humps in [true, false] {
            with_options(humps, true, || {
                assert!(
                    has_label(&complete_at(query, &snap), "Sales.SalesOrder"),
                    "{humps}: {query}"
                );
            });
        }
    }
}

#[test]
fn schema_joined_query_matches_only_with_schema_in_search_on() {
    let snap = snapshot();
    for humps in [true, false] {
        for query in [
            "SELECT * FROM salesS{cursor}",
            "SELECT * FROM salessalesor{cursor}",
        ] {
            with_options(humps, true, || {
                assert!(
                    has_label(&complete_at(query, &snap), "Sales.SalesOrder"),
                    "{humps}: {query}"
                );
            });
            with_options(humps, false, || {
                assert!(
                    !has_label(&complete_at(query, &snap), "Sales.SalesOrder"),
                    "{humps}: {query}"
                );
            });
        }
    }
}
