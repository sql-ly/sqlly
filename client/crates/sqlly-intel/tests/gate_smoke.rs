//! Smoke test + worked template for the full IntelliSense verification suite.
//!
//! This whole file is gated on `intellisense_full_suite` (emitted by build.rs
//! only when `SQLLY_INTELLISENSE_SUITE` is set), so it is empty on developer
//! machines and every CI leg except the one that opts in. Copy this shape when
//! turning an IntelliSense-feedback report into a regression test — see the
//! maintainers' `intellisense-feedback-tests` workflow.
#![cfg(intellisense_full_suite)]

use sqlly_intel::intellisense_verify::*;

/// The schema a report's queries touch. Rebuild it fresh per test.
fn test_schema() -> Vec<ObjectFragment> {
    vec![
        table(
            "Sales",
            "Customer",
            vec![
                col("CustomerId", "int", 0),
                col("FullName", "nvarchar(200)", 1),
                col("CreditLimit", "decimal(18,2)", 2),
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
                col("Amount", "money", 2),
            ],
            Some(pk("OrderId")),
            vec![fk(&["CustomerId"], "Sales", "Customer", &["CustomerId"])],
        ),
    ]
}

#[test]
fn alias_dot_offers_only_that_tables_columns() {
    let snap = snapshot(test_schema());
    let c = complete_at("SELECT * FROM Sales.Customer x WHERE x.{cursor}", &snap);
    assert_has(&c, &["CustomerId", "FullName", "CreditLimit"]);
    assert_excludes(&c, &["OrderId", "Amount"]);
}

#[test]
fn from_clause_offers_tables() {
    let snap = snapshot(test_schema());
    let c = complete_at("SELECT * FROM {cursor}", &snap);
    assert_has(&c, &["Sales.Customer", "Sales.SalesOrder"]);
}
