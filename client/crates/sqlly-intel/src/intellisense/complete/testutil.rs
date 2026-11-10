//! Shared test fixtures for the completion submodule tests: small catalogs
//! (single-schema, multi-schema, cookbook-shaped, accounting bug-repro) and
//! label helpers.

use sqlly_core::ObjectKind;

use super::default_snippets;
use crate::intellisense::types::{CompletionCatalog, CompletionItem, CompletionObject};

pub(crate) fn catalog() -> CompletionCatalog {
    CompletionCatalog {
        databases: vec!["appdb".to_string()],
        schemas: vec!["dbo".to_string(), "sales".to_string()],
        objects: vec![CompletionObject {
            database: Some("appdb".to_string()),
            schema: Some("sales".to_string()),
            name: "Orders".to_string(),
            kind: ObjectKind::Table,
            columns: vec!["OrderId".to_string(), "CustomerId".to_string()],
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        }],
        filesystem_schemas: Vec::new(),
        snippets: vec![(
            "select-top".to_string(),
            "SELECT TOP 100 * FROM ".to_string(),
        )],
    }
}

/// Catalog modelled after the user's report: a `Shared` schema plus a
/// `Sales` schema, each with tables/columns, and one table name (`Orders`)
/// reused across schemas to exercise schema-qualified disambiguation.
pub(crate) fn multi_schema_catalog() -> CompletionCatalog {
    CompletionCatalog {
        databases: vec!["AppDb".to_string()],
        schemas: vec!["Shared".to_string(), "Sales".to_string()],
        objects: vec![
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Shared".to_string()),
                name: "Customer".to_string(),
                kind: ObjectKind::Table,
                columns: vec![
                    "CustomerId".to_string(),
                    "CustomerName".to_string(),
                    "Region".to_string(),
                ],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Shared".to_string()),
                name: "Lookup".to_string(),
                kind: ObjectKind::Table,
                columns: vec!["Code".to_string(), "Value".to_string()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Sales".to_string()),
                name: "Orders".to_string(),
                kind: ObjectKind::Table,
                columns: vec!["OrderId".to_string(), "CustomerId".to_string()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
        ],
        filesystem_schemas: Vec::new(),
        snippets: default_snippets(),
    }
}

/// The test catalog mirrors the cookbook schema: Sales + HR (used) with
/// tables/views/procedures/functions, plus Archive + Staging (unused).
pub(crate) fn cookbook_catalog() -> CompletionCatalog {
    CompletionCatalog {
        databases: vec!["AppDb".to_string()],
        schemas: vec![
            "Sales".to_string(),
            "HR".to_string(),
            "Archive".to_string(),
            "Staging".to_string(),
        ],
        objects: vec![
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Sales".to_string()),
                name: "Customer".to_string(),
                kind: ObjectKind::Table,
                columns: vec![
                    "CustomerId".to_string(),
                    "FullName".to_string(),
                    "CreditLimit".to_string(),
                    "IsActive".to_string(),
                ],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Sales".to_string()),
                name: "SalesOrder".to_string(),
                kind: ObjectKind::Table,
                columns: vec![
                    "OrderId".to_string(),
                    "CustomerId".to_string(),
                    "OrderDate".to_string(),
                    "Amount".to_string(),
                ],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("HR".to_string()),
                name: "Department".to_string(),
                kind: ObjectKind::Table,
                columns: vec![
                    "DepartmentId".to_string(),
                    "DeptName".to_string(),
                    "HeadCount".to_string(),
                ],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("HR".to_string()),
                name: "Employee".to_string(),
                kind: ObjectKind::Table,
                columns: vec![
                    "EmployeeId".to_string(),
                    "DepartmentId".to_string(),
                    "AssignedCustomerId".to_string(),
                    "FullName".to_string(),
                ],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Sales".to_string()),
                name: "vCustomerOrders".to_string(),
                kind: ObjectKind::View,
                columns: vec![
                    "CustomerId".to_string(),
                    "FullName".to_string(),
                    "OrderId".to_string(),
                    "Amount".to_string(),
                ],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("HR".to_string()),
                name: "vDepartmentRoster".to_string(),
                kind: ObjectKind::View,
                columns: vec![
                    "DepartmentId".to_string(),
                    "DeptName".to_string(),
                    "EmployeeId".to_string(),
                    "FullName".to_string(),
                ],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Archive".to_string()),
                name: "ArchivedOrder".to_string(),
                kind: ObjectKind::Table,
                columns: vec!["OrderId".to_string(), "ArchivedAt".to_string()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Staging".to_string()),
                name: "RawCustomerImport".to_string(),
                kind: ObjectKind::Table,
                columns: vec!["RowId".to_string(), "Payload".to_string()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
        ],
        filesystem_schemas: Vec::new(),
        snippets: Vec::new(),
    }
}

pub(crate) fn accounting_catalog() -> CompletionCatalog {
    CompletionCatalog {
        databases: vec!["AppDb".to_string()],
        schemas: vec!["Accounting".to_string()],
        objects: vec![
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Accounting".to_string()),
                name: "JournalLine".to_string(),
                kind: ObjectKind::Table,
                columns: vec![
                    "JournalLineId".to_string(),
                    "JournalId".to_string(),
                    "Amount".to_string(),
                    "Description".to_string(),
                ],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
            CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Accounting".to_string()),
                name: "Journal".to_string(),
                kind: ObjectKind::Table,
                columns: vec![
                    "JournalId".to_string(),
                    "JournalDate".to_string(),
                    "Description".to_string(),
                ],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            },
        ],
        filesystem_schemas: Vec::new(),
        snippets: Vec::new(),
    }
}

pub(crate) fn labels(items: &[CompletionItem]) -> Vec<String> {
    items.iter().map(|item| item.label.clone()).collect()
}

pub(crate) fn find_item<'a>(items: &'a [CompletionItem], label: &str) -> &'a CompletionItem {
    items
        .iter()
        .find(|item| item.label == label)
        .unwrap_or_else(|| panic!("expected completion `{label}`, got: {:?}", labels(items)))
}
