//! Reusable harness for the full IntelliSense verification suite.
//!
//! This crate exists to hold **regression tests generated from real
//! IntelliSense-feedback reports**. Each report a user submits from the app can
//! be turned into a Rust test file under `tests/` (see the
//! maintainers' `intellisense-feedback-tests` workflow), and this module gives
//! those files everything they need so a generated test is just a schema plus a
//! query with a `{cursor}` marker and the completions it should / should not
//! offer.
//!
//! The API mirrors the worked example in
//! `client/crates/sqlly-intel/tests/intellisense_cookbook.rs` — the same
//! declarative schema builders (`table`, `view`, `proc`, `func`, `col`, `pk`,
//! `fk`, `param`) and the same three-line test shape:
//!
//! ```ignore
//! use sqlly_intel::intellisense_verify::*;
//!
//! fn test_schema() -> Vec<ObjectFragment> {
//!     vec![table(
//!         "Sales",
//!         "Customer",
//!         vec![col("CustomerId", "int", 0), col("FullName", "nvarchar(200)", 1)],
//!         Some(pk("CustomerId")),
//!         vec![],
//!     )]
//! }
//!
//! #[test]
//! fn customer_columns_after_alias_dot() {
//!     let snap = snapshot(test_schema());
//!     let c = complete_at("SELECT * FROM Sales.Customer x WHERE x.{cursor}", &snap);
//!     assert_has(&c, &["CustomerId", "FullName"]);
//!     assert_excludes(&c, &["OrderId"]);
//! }
//! ```
//!
//! The whole crate compiles to an empty test binary unless the
//! `SQLLY_INTELLISENSE_SUITE` environment variable is set at build time (see
//! `build.rs`), which is why the tests below carry `#![cfg(intellisense_full_suite)]`.

use std::collections::BTreeSet;

// Re-export everything a generated test file needs so it only has to
// `use crate::intellisense_verify::*;`.
pub use crate::modelengine::intellisense::CompletionItemKind;
pub use crate::modelengine::{
    CompletionItem, CompletionMode, CompletionRequest, Completions, complete_symbols,
};
pub use sqlly_lang::sqlmodel::{
    ColumnDef, Dialect, Fidelity, ForeignKeyDef, IncompleteRegions, KeyDef, ModelSnapshot,
    ObjectBody, ObjectFragment, ObjectKind, ObjectRef, ParamDef, ParamMode, Provenance,
    RoutineBody, Span, SqlName, TableBody, TypeRef, ViewBody,
};

/// Every object built here is SQL Server flavored, matching the cookbook.
pub const D: Dialect = Dialect::SqlServer;

// ===========================================================================
// Small builders — keep generated schema definitions declarative and readable.
// ===========================================================================

/// A dialect-flavored identifier.
pub fn n(s: &str) -> SqlName {
    SqlName::new(s, D, false)
}

/// A schema-qualified object reference, e.g. `Sales.Customer`.
pub fn obj(schema: &str, name: &str) -> ObjectRef {
    ObjectRef::new(None, Some(n(schema)), n(name))
}

/// A column with a real, parsed type (the native spelling shows up as the
/// completion item's `detail`).
pub fn col(name: &str, ty: &str, ordinal: usize) -> ColumnDef {
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

/// A single-column primary key.
pub fn pk(column: &str) -> KeyDef {
    KeyDef {
        name: None,
        columns: vec![n(column)],
    }
}

/// A single-column foreign key: `columns` on this table reference
/// `tgt_schema.tgt_table (tgt_cols)`.
pub fn fk(columns: &[&str], tgt_schema: &str, tgt_table: &str, tgt_cols: &[&str]) -> ForeignKeyDef {
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
            profile: "intellisense-verify".into(),
        },
        fidelity: Fidelity::Exact,
        references,
    }
}

/// A table with columns, an optional primary key, and foreign keys.
pub fn table(
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

/// A view with columns.
pub fn view(schema: &str, name: &str, columns: Vec<ColumnDef>) -> ObjectFragment {
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

/// A routine parameter.
pub fn param(name: &str, ty: &str, mode: ParamMode) -> ParamDef {
    ParamDef {
        name: n(name),
        ty: Some(TypeRef::parse(ty, D)),
        mode,
        default: None,
    }
}

/// A stored procedure with parameters.
pub fn proc(schema: &str, name: &str, params: Vec<ParamDef>) -> ObjectFragment {
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

/// A scalar function with parameters and a return type.
pub fn func(schema: &str, name: &str, params: Vec<ParamDef>, returns: &str) -> ObjectFragment {
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
// The harness — build a snapshot from fragments, then complete at a marker.
// ===========================================================================

/// Turn a set of schema fragments into a queryable [`ModelSnapshot`]. Pass a
/// fresh `Vec` per test so tests never share mutable state.
pub fn snapshot(fragments: Vec<ObjectFragment>) -> ModelSnapshot {
    let (snap, _delta) =
        ModelSnapshot::empty().apply(1, fragments, &BTreeSet::new(), IncompleteRegions::default());
    snap
}

/// Trigger symbol-mode completion where the literal marker `{cursor}` appears
/// in `query_with_marker`, and return the results. The marker must appear
/// exactly once.
pub fn complete_at(query_with_marker: &str, snap: &ModelSnapshot) -> Completions {
    complete_at_in(query_with_marker, snap, D)
}

/// [`complete_at`] for a specific dialect, for behaviour that differs per
/// engine (what a `SET` can name, say).
pub fn complete_at_in(
    query_with_marker: &str,
    snap: &ModelSnapshot,
    dialect: Dialect,
) -> Completions {
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
        dialect,
        default_catalog: None,
        default_namespace: Some(n("dbo")),
        mode: CompletionMode::Symbol,
    };
    complete_symbols(&req, snap)
}

/// All completion labels, in order.
pub fn labels(c: &Completions) -> Vec<String> {
    c.items.iter().map(|i| i.label.clone()).collect()
}

/// Completion labels narrowed to one kind (e.g. only columns).
pub fn labels_of(c: &Completions, kind: CompletionItemKind) -> Vec<String> {
    c.items
        .iter()
        .filter(|i| i.kind == kind)
        .map(|i| i.label.clone())
        .collect()
}

/// Print every triggered completion — call it while writing a test to see what
/// fired (`cargo test ... -- --nocapture`).
pub fn dump(title: &str, c: &Completions) {
    println!("--- {title} ({} items) ---", c.items.len());
    for item in &c.items {
        println!("  [{:?}] {}", item.kind, item.label);
    }
}

/// Assert every label in `expected` is offered.
pub fn assert_has(c: &Completions, expected: &[&str]) {
    let got = labels(c);
    for want in expected {
        assert!(
            got.iter().any(|l| l == want),
            "expected completion `{want}` not found.\n  got: {got:#?}"
        );
    }
}

/// Assert none of the labels in `forbidden` are offered.
pub fn assert_excludes(c: &Completions, forbidden: &[&str]) {
    let got = labels(c);
    for bad in forbidden {
        assert!(
            !got.iter().any(|l| l == bad),
            "completion `{bad}` should NOT be present.\n  got: {got:#?}"
        );
    }
}
