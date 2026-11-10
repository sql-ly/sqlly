//! Dotted, multi-part identifier completion: resolving `schema.`,
//! `schema.table.`, `db.schema.table.` and the lateral `{table}.{column}`
//! short form against the catalog.

use sqlly_core::ObjectKind;
use std::fmt::Write;

use crate::intellisense::score::fuzzy;
use crate::intellisense::score::push_scored;
use crate::intellisense::types::{
    CompletionCatalog, CompletionItem, CompletionKind, CompletionSource,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DottedPrefix {
    path: Vec<String>,
    partial: String,
}

impl DottedPrefix {
    /// Returns `None` when the prefix is not an object path: no dot present, or
    /// it is a parameter member-access / variable reference starting with `@`.
    pub(crate) fn parse(prefix: &str) -> Option<Self> {
        if !prefix.contains('.') || prefix.starts_with('@') {
            return None;
        }
        let mut segments: Vec<String> = prefix
            .split('.')
            .map(|segment| {
                segment
                    .trim_matches(|ch| ch == '[' || ch == ']')
                    .to_string()
            })
            .collect();
        // `split('.')` always yields at least 2 parts when a dot is present, so
        // the final (possibly empty) partial segment is safe to pop.
        let partial = segments.pop().unwrap_or_default();
        let path: Vec<String> = segments
            .into_iter()
            .filter(|segment| !segment.is_empty())
            .collect();
        if path.is_empty() {
            return None;
        }
        Some(Self { path, partial })
    }
}

/// Resolves a dotted object path against the catalog and emits the appropriate
/// completions. This is the heart of the `{database}.{schema}.{table}.{column}`
/// support and its lateral short forms:
///
/// - `schema.`               -> tables/views in that schema
/// - `schema.table.`         -> columns of that table
/// - `table.`                -> columns (lateral `{table}.{column}` short form)
/// - `db.schema.`            -> tables/views in that schema
/// - `db.schema.table.`      -> columns of that table
///
/// The active database is implied, so a leading database segment is optional.
/// When a single leading segment is ambiguous (it could be either a schema or a
/// table name), both interpretations are offered.
pub(crate) fn add_dotted_items(
    items: &mut Vec<CompletionItem>,
    catalog: &CompletionCatalog,
    dotted: &DottedPrefix,
) {
    let partial = dotted.partial.as_str();
    let path = &dotted.path;

    match path.len() {
        1 => {
            let head = &path[0];
            // Interpretation A: `head` is a schema -> list its tables/views.
            emit_tables_for_schema(items, catalog, head, partial);
            // Interpretation B: `head` is a table/view (or alias resolved
            // elsewhere) -> list its columns via the `{table}.{column}` form.
            emit_columns_for_table(items, catalog, None, head, partial);
        }
        2 => {
            let (first, second) = (&path[0], &path[1]);
            // `schema.table.` -> columns.
            emit_columns_for_table(items, catalog, Some(first.as_str()), second, partial);
            // `database.schema.` -> tables/views (database is implied/ignored
            // for resolution; we only need the schema segment).
            emit_tables_for_schema(items, catalog, second, partial);
        }
        3 => {
            // `database.schema.table.` -> columns. Leading database segment is
            // implied; resolve on schema + table.
            let (schema, table) = (&path[1], &path[2]);
            emit_columns_for_table(items, catalog, Some(schema.as_str()), table, partial);
        }
        _ => {}
    }
}

/// Emits `schema.table` completions for every table/view whose schema matches
/// `schema` (case-insensitive). Covers both live-metadata objects and the
/// filesystem object model.
fn emit_tables_for_schema(
    items: &mut Vec<CompletionItem>,
    catalog: &CompletionCatalog,
    schema: &str,
    partial: &str,
) {
    let mut label_buf = String::new();
    for object in &catalog.objects {
        let matches_schema = object
            .schema
            .as_deref()
            .is_some_and(|object_schema| segment_matches(object_schema, schema));
        if !matches_schema {
            continue;
        }
        let kind = match object.kind {
            ObjectKind::Table | ObjectKind::View => CompletionKind::Table,
            ObjectKind::StoredProcedure => CompletionKind::Procedure,
            ObjectKind::ScalarFunction
            | ObjectKind::TableFunction
            | ObjectKind::AggregateFunction => CompletionKind::Function,
            ObjectKind::Type => CompletionKind::Type,
            _ => continue,
        };
        let object_schema = object.schema.as_deref().unwrap_or(schema);
        push_dotted_match(
            items,
            partial,
            &object.name,
            &object.name,
            kind,
            CompletionSource::LiveMetadata,
            82,
            fuzzy::CONTAINS_SCORE,
            &mut label_buf,
            |buf| {
                buf.clear();
                let _ = write!(buf, "{object_schema}.{}", object.name);
            },
        );
    }

    for fs_schema in &catalog.filesystem_schemas {
        if !segment_matches(&fs_schema.node.name, schema) {
            continue;
        }
        for table in &fs_schema.tables {
            push_dotted_match(
                items,
                partial,
                &table.source_node.node.name,
                &table.source_node.node.name,
                CompletionKind::Table,
                CompletionSource::Filesystem,
                80,
                fuzzy::CONTAINS_SCORE,
                &mut label_buf,
                |buf| {
                    buf.clear();
                    let _ = write!(
                        buf,
                        "{}.{}",
                        fs_schema.node.name, table.source_node.node.name
                    );
                },
            );
        }
        for view in &fs_schema.views {
            push_dotted_match(
                items,
                partial,
                &view.source_node.node.name,
                &view.source_node.node.name,
                CompletionKind::Table,
                CompletionSource::Filesystem,
                78,
                fuzzy::CONTAINS_SCORE,
                &mut label_buf,
                |buf| {
                    buf.clear();
                    let _ = write!(
                        buf,
                        "{}.{}",
                        fs_schema.node.name, view.source_node.node.name
                    );
                },
            );
        }
    }
}

/// Emits column completions for the table identified by `(schema, table)`.
/// When `schema` is `None`, the table is matched by name across all schemas
/// (lateral `{table}.{column}` form). The emitted label preserves the depth the
/// user typed: a schema-qualified path yields `schema.table.column`, while the
/// bare-table form yields `table.column`.
fn emit_columns_for_table(
    items: &mut Vec<CompletionItem>,
    catalog: &CompletionCatalog,
    schema: Option<&str>,
    table: &str,
    partial: &str,
) {
    let mut label_buf = String::new();
    for object in &catalog.objects {
        if !matches!(object.kind, ObjectKind::Table | ObjectKind::View) {
            continue;
        }
        if !segment_matches(&object.name, table) {
            continue;
        }
        if let Some(schema) = schema {
            let matches_schema = object
                .schema
                .as_deref()
                .is_some_and(|object_schema| segment_matches(object_schema, schema));
            if !matches_schema {
                continue;
            }
        }
        for column in &object.columns {
            push_dotted_match(
                items,
                partial,
                column,
                column,
                CompletionKind::Column,
                CompletionSource::LiveMetadata,
                84,
                i32::MIN,
                &mut label_buf,
                |buf| {
                    buf.clear();
                    match schema {
                        Some(_) => {
                            if let Some(object_schema) = object.schema.as_deref() {
                                let _ = write!(buf, "{object_schema}.{}.{}", object.name, column);
                            } else {
                                let _ = write!(buf, "{}.{}", object.name, column);
                            }
                        }
                        None => {
                            let _ = write!(buf, "{}.{}", object.name, column);
                        }
                    }
                },
            );
        }
    }

    for fs_schema in &catalog.filesystem_schemas {
        if let Some(schema) = schema
            && !segment_matches(&fs_schema.node.name, schema)
        {
            continue;
        }
        for fs_table in &fs_schema.tables {
            if !segment_matches(&fs_table.source_node.node.name, table) {
                continue;
            }
            for column in &fs_table.columns {
                push_dotted_match(
                    items,
                    partial,
                    &column.node.name,
                    &column.node.name,
                    CompletionKind::Column,
                    CompletionSource::Filesystem,
                    82,
                    i32::MIN,
                    &mut label_buf,
                    |buf| {
                        buf.clear();
                        match schema {
                            Some(_) => {
                                let _ = write!(
                                    buf,
                                    "{}.{}.{}",
                                    fs_schema.node.name,
                                    fs_table.source_node.node.name,
                                    column.node.name
                                );
                            }
                            None => {
                                let _ = write!(
                                    buf,
                                    "{}.{}",
                                    fs_table.source_node.node.name, column.node.name
                                );
                            }
                        }
                    },
                );
            }
        }
    }
}

/// Whether a catalog name matches a typed path segment, case-insensitively.
/// A name the prefix scanner cannot read whole (`Order Details`, `my-db`)
/// reaches here respelled without those characters (`OrderDetails`, `mydb`)
/// when the user typed it quoted, so that spelling matches too.
fn segment_matches(name: &str, segment: &str) -> bool {
    if name.eq_ignore_ascii_case(segment) {
        return true;
    }
    let plain = |c: &char| c.is_ascii_alphanumeric() || *c == '_';
    name.len() > segment.len()
        && name
            .chars()
            .filter(plain)
            .map(|c| c.to_ascii_lowercase())
            .eq(segment
                .chars()
                .filter(plain)
                .map(|c| c.to_ascii_lowercase()))
}

/// Like [`crate::intellisense::score::push_match`] but filters on the final segment
/// (`partial`) rather than the whole dotted label, so `sales.Orders.ord`
/// matches `OrderId` by its `ord` tail even though the full label starts with
/// `sales.`. The `min_score` parameter controls how loose the fuzzy match may
/// be; schema-qualified object names and columns both accept any fuzzy hit
/// down to contains (feedback ifb_2ff4), ranked by tier. The full
/// `label` is written into `label_buf` only after the segment passes the fuzzy
/// gate.
#[allow(clippy::too_many_arguments)]
fn push_dotted_match(
    items: &mut Vec<CompletionItem>,
    partial: &str,
    segment: &str,
    insert_text: &str,
    kind: CompletionKind,
    source: CompletionSource,
    base_score: i32,
    min_score: i32,
    label_buf: &mut String,
    write_label: impl FnOnce(&mut String),
) {
    let accepted = if partial.is_empty() {
        Some(0)
    } else {
        fuzzy::score(partial, segment).filter(|&s| s >= min_score)
    };
    if let Some(fs) = accepted {
        write_label(label_buf);
        push_scored(
            items,
            label_buf.as_str(),
            insert_text,
            kind,
            source,
            base_score + fs,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{cookbook_catalog, find_item, labels, multi_schema_catalog};
    use super::*;
    use crate::intellisense::complete::complete;
    use crate::intellisense::types::CompletionObject;
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::Instant;
    #[cfg(target_arch = "wasm32")]
    use web_time::Instant;

    #[test]
    fn schema_dot_lists_only_tables_of_that_schema() {
        // User's exact scenario: `SELECT * FROM Shared.`
        let sql = "SELECT\n\t*\nFROM\n\tShared.";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        let got = labels(&items);

        assert!(got.contains(&"Shared.Customer".to_string()));
        assert!(got.contains(&"Shared.Lookup".to_string()));
        // Tables from other schemas must NOT appear.
        assert!(!got.contains(&"Sales.Orders".to_string()));
        // Columns must NOT be offered at schema depth.
        assert!(
            !items.iter().any(|item| item.kind == CompletionKind::Column),
            "schema. should offer tables, not columns: {got:?}"
        );
        // The first/top suggestion is a table of the schema.
        assert!(items[0].label.starts_with("Shared."));
        assert_eq!(items[0].kind, CompletionKind::Table);
    }

    #[test]
    fn schema_dot_partial_filters_tables() {
        // `Shared.Cust` -> only Shared.Customer
        let sql = "SELECT * FROM Shared.Cust";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        let got = labels(&items);
        assert!(got.contains(&"Shared.Customer".to_string()));
        assert!(!got.contains(&"Shared.Lookup".to_string()));
    }

    #[test]
    fn schema_table_dot_lists_columns_qualified() {
        // `Shared.Customer.` -> three-part columns
        let sql = "SELECT Shared.Customer.";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        let got = labels(&items);
        assert!(got.contains(&"Shared.Customer.CustomerId".to_string()));
        assert!(got.contains(&"Shared.Customer.CustomerName".to_string()));
        assert!(got.contains(&"Shared.Customer.Region".to_string()));
        // No columns from a different table.
        assert!(!got.contains(&"Shared.Lookup.Code".to_string()));
        assert!(
            items.iter().all(|item| item.kind == CompletionKind::Column
                || item.source == CompletionSource::Inferred)
        );
    }

    #[test]
    fn schema_table_dot_partial_filters_columns() {
        // `Shared.Customer.Cust` -> CustomerId + CustomerName, not Region
        let sql = "SELECT Shared.Customer.Cust";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        let got = labels(&items);
        assert!(got.contains(&"Shared.Customer.CustomerId".to_string()));
        assert!(got.contains(&"Shared.Customer.CustomerName".to_string()));
        assert!(!got.contains(&"Shared.Customer.Region".to_string()));
    }

    #[test]
    fn table_dot_lists_columns_lateral_short_form() {
        // Lateral `{table}.{column}` form: `Customer.` resolves columns by
        // table name across schemas, emitting the two-part label.
        let sql = "SELECT Customer.";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        let got = labels(&items);
        assert!(got.contains(&"Customer.CustomerId".to_string()));
        assert!(got.contains(&"Customer.CustomerName".to_string()));
        assert!(got.contains(&"Customer.Region".to_string()));
    }

    #[test]
    fn table_dot_partial_filters_columns_lateral() {
        let sql = "SELECT Customer.Reg";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        let got = labels(&items);
        assert!(got.contains(&"Customer.Region".to_string()));
        assert!(!got.contains(&"Customer.CustomerId".to_string()));
    }

    #[test]
    fn database_schema_table_dot_lists_columns() {
        // Full four-part path with implied/explicit database:
        // `AppDb.Shared.Customer.` -> columns.
        let sql = "SELECT AppDb.Shared.Customer.";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        let got = labels(&items);
        assert!(got.contains(&"Shared.Customer.CustomerId".to_string()));
        assert!(got.contains(&"Shared.Customer.Region".to_string()));
    }

    #[test]
    fn database_schema_dot_lists_tables() {
        // `AppDb.Shared.` -> tables of Shared (database segment ignored).
        let sql = "SELECT * FROM AppDb.Shared.";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        let got = labels(&items);
        assert!(got.contains(&"Shared.Customer".to_string()));
        assert!(got.contains(&"Shared.Lookup".to_string()));
    }

    #[test]
    fn schema_qualified_table_disambiguates_columns() {
        // `Sales.Orders.` must only show Orders columns, even though another
        // schema also has tables. Confirms schema scoping on the column path.
        let sql = "SELECT Sales.Orders.";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        let got = labels(&items);
        assert!(got.contains(&"Sales.Orders.OrderId".to_string()));
        assert!(got.contains(&"Sales.Orders.CustomerId".to_string()));
        assert!(!got.iter().any(|label| label.starts_with("Shared.")));
    }

    #[test]
    fn dotted_completion_is_case_insensitive() {
        // Lowercase schema/table typed against capitalized catalog entries.
        let sql = "SELECT * FROM shared.";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        assert!(labels(&items).contains(&"Shared.Customer".to_string()));

        let sql2 = "SELECT shared.customer.";
        let items2 = complete(sql2, sql2.len(), &multi_schema_catalog());
        assert!(labels(&items2).contains(&"Shared.Customer.CustomerId".to_string()));
    }

    #[test]
    fn dotted_path_suppresses_keyword_noise() {
        // After a dot we resolve the object path only; SQL keywords/snippets
        // should not pollute the suggestions.
        let sql = "SELECT * FROM Shared.";
        let items = complete(sql, sql.len(), &multi_schema_catalog());
        assert!(items.iter().all(|item| {
            item.kind != CompletionKind::Keyword && item.kind != CompletionKind::Snippet
        }));
    }

    #[test]
    fn parameter_member_access_is_not_treated_as_object_path() {
        // `@var.` is not a schema/table path; parse must reject it so we don't
        // emit spurious table/column completions.
        assert_eq!(DottedPrefix::parse("@var."), None);
        assert_eq!(DottedPrefix::parse("sales"), None);
        assert_eq!(DottedPrefix::parse(""), None);

        let parsed = DottedPrefix::parse("shared.customer.cust").unwrap();
        assert_eq!(parsed.path, vec!["shared", "customer"]);
        assert_eq!(parsed.partial, "cust");

        let trailing = DottedPrefix::parse("shared.").unwrap();
        assert_eq!(trailing.path, vec!["shared"]);
        assert_eq!(trailing.partial, "");
    }

    #[test]
    fn bracket_quoted_segments_are_normalized() {
        let parsed = DottedPrefix::parse("[shared].[customer].").unwrap();
        assert_eq!(parsed.path, vec!["shared", "customer"]);
        assert_eq!(parsed.partial, "");
    }

    #[test]
    fn dotted_completion_stays_under_latency_budget() {
        let mut catalog = multi_schema_catalog();
        for idx in 0..1_000 {
            catalog.objects.push(CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Shared".to_string()),
                name: format!("Table{idx}"),
                kind: ObjectKind::Table,
                columns: vec!["Id".to_string(), "Name".to_string()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            });
        }
        let sql = "SELECT * FROM Shared.";
        let started = Instant::now();
        let items = complete(sql, sql.len(), &catalog);
        assert!(!items.is_empty());
        assert!(started.elapsed() < std::time::Duration::from_millis(50));
    }

    // ---- Schema-qualified fuzzy matching through the dotted path -------

    fn has_label(items: &[CompletionItem], expected: &str) -> bool {
        items.iter().any(|item| item.label == expected)
    }

    fn missing_label(items: &[CompletionItem], unexpected: &str) -> bool {
        !items.iter().any(|item| item.label == unexpected)
    }

    fn with_options<R>(hump_matching: bool, schema_in_search: bool, f: impl FnOnce() -> R) -> R {
        crate::matching::with_match_options(
            crate::matching::MatchOptions {
                hump_matching,
                schema_in_search,
            },
            f,
        )
    }

    /// Twins of the hump-dependent FROM tests: each camel/acronym query
    /// resolves with hump matching on and finds nothing with it off, while a
    /// case-insensitive prefix still works either way.
    #[test]
    fn hump_queries_on_and_off() {
        let catalog = cookbook_catalog();
        let cases = [
            ("SELECT * FROM SSO", "Sales.SalesOrder"),
            ("SELECT * FROM SaOr", "Sales.SalesOrder"),
            ("SELECT * FROM SaSSO", "Sales.SalesOrder"),
            ("SELECT * FROM SSaOr", "Sales.SalesOrder"),
            ("SELECT * FROM Sales.SaOr", "Sales.SalesOrder"),
        ];
        for (sql, label) in cases {
            with_options(true, true, || {
                assert!(
                    has_label(&complete(sql, sql.len(), &catalog), label),
                    "on: {sql}"
                );
            });
            with_options(false, true, || {
                assert!(
                    missing_label(&complete(sql, sql.len(), &catalog), label),
                    "off: {sql}"
                );
            });
        }
        for sql in [
            "SELECT * FROM salesor",
            "SELECT * FROM SALESORD",
            "SELECT * FROM Sales.salesO",
        ] {
            for humps in [true, false] {
                with_options(humps, true, || {
                    assert!(
                        has_label(&complete(sql, sql.len(), &catalog), "Sales.SalesOrder"),
                        "{humps}: {sql}"
                    );
                });
            }
        }
    }

    /// Feedback ifb_6f67 through the builtin engine: after a finished WHERE
    /// test, nothing while the caret touches it, then only connectors and
    /// clauses after whitespace; columns return after AND.
    #[test]
    fn after_a_finished_where_test_only_connectors_follow() {
        let catalog = cookbook_catalog();
        let no_columns = |items: &[CompletionItem]| {
            !items.iter().any(|item| item.kind == CompletionKind::Column)
        };
        let touching = "SELECT * FROM Sales.Customer c WHERE c.CustomerId = '5'";
        let items = complete(touching, touching.len(), &catalog);
        assert!(items.is_empty(), "{:?}", labels(&items));
        let spaced = "SELECT * FROM Sales.Customer c WHERE c.CustomerId = '5' ";
        let items = complete(spaced, spaced.len(), &catalog);
        assert!(has_label(&items, "AND") && has_label(&items, "OR"));
        assert!(no_columns(&items), "{:?}", labels(&items));
        let next = "SELECT * FROM Sales.Customer c WHERE c.CustomerId = '5' AND ";
        let items = complete(next, next.len(), &catalog);
        assert!(!no_columns(&items), "{:?}", labels(&items));
    }

    /// Schema search: `salessal` reads `Sales` + `SalesOrder` as one word.
    #[test]
    fn schema_in_search_on_and_off() {
        let catalog = cookbook_catalog();
        let sql = "SELECT * FROM salessal";
        for humps in [true, false] {
            with_options(humps, true, || {
                assert!(
                    has_label(&complete(sql, sql.len(), &catalog), "Sales.SalesOrder"),
                    "{humps}"
                );
            });
            with_options(humps, false, || {
                let items = complete(sql, sql.len(), &catalog);
                assert!(missing_label(&items, "Sales.SalesOrder"), "{humps}");
                let leaf = "SELECT * FROM salesord";
                assert!(
                    has_label(&complete(leaf, leaf.len(), &catalog), "Sales.SalesOrder"),
                    "{humps}"
                );
            });
        }
    }

    #[test]
    fn from_schema_qualified_acronym_so_matches_sales_sales_order() {
        let items = complete("SELECT * FROM Sales.SO", 21, &cookbook_catalog());
        assert!(has_label(&items, "Sales.SalesOrder"));
    }

    #[test]
    fn from_schema_qualified_partial_saor_matches_sales_sales_order() {
        let items = complete("SELECT * FROM Sales.SaOr", 22, &cookbook_catalog());
        assert!(has_label(&items, "Sales.SalesOrder"));
    }

    #[test]
    fn from_schema_qualified_partial_saor_does_not_match_other_sales_objects() {
        let items = complete("SELECT * FROM Sales.SaOr", 22, &cookbook_catalog());
        assert!(has_label(&items, "Sales.SalesOrder"));
        assert!(missing_label(&items, "Sales.Customer"));
        assert!(missing_label(&items, "Sales.vCustomerOrders"));
        assert!(missing_label(&items, "HR.Department"));
        assert!(missing_label(&items, "HR.Employee"));
        assert!(missing_label(&items, "HR.vDepartmentRoster"));
        assert!(missing_label(&items, "Archive.ArchivedOrder"));
        assert!(missing_label(&items, "Staging.RawCustomerImport"));
    }

    #[test]
    fn from_hr_schema_dot_partial_dep_ranks_hr_department_first() {
        let items = complete("SELECT * FROM HR.Dep", 20, &cookbook_catalog());
        let position = |label: &str| items.iter().position(|i| i.label == label);
        // `Dep` is a prefix of `Department` and a word start inside
        // `vDepartmentRoster`: the schema-dot list is a contains search
        // (feedback ifb_2ff4), with the prefix hit first.
        assert!(position("HR.Department") < position("HR.vDepartmentRoster"));
        assert!(has_label(&items, "HR.vDepartmentRoster"));
        assert!(missing_label(&items, "HR.Employee"));
        assert!(missing_label(&items, "Sales.Customer"));
        assert!(missing_label(&items, "Sales.SalesOrder"));
    }

    #[test]
    fn from_sales_schema_dot_does_not_cross_schema_match_acronym() {
        // `RCI` is the acronym of `Staging.RawCustomerImport` and matches it
        // unqualified, but qualified by `Sales.` it must stay inside Sales.
        let items = complete("SELECT * FROM Sales.RCI", 21, &cookbook_catalog());
        assert!(missing_label(&items, "Staging.RawCustomerImport"));
        assert!(missing_label(&items, "Archive.ArchivedOrder"));
        assert!(missing_label(&items, "HR.Department"));
        assert!(missing_label(&items, "HR.Employee"));
        assert!(missing_label(&items, "HR.vDepartmentRoster"));
    }

    #[test]
    fn from_hr_schema_dot_does_not_cross_schema_match_sales_order() {
        // `SaOr` matches `Sales.SalesOrder` unqualified, but `HR.` confines
        // the search to HR, so the Sales table must not cross over.
        let items = complete("SELECT * FROM HR.SaOr", 20, &cookbook_catalog());
        assert!(missing_label(&items, "Sales.SalesOrder"));
        assert!(missing_label(&items, "Sales.Customer"));
    }

    // ---- Tier 3A: Schema-dot listing tests (ported from cookbook) -------
    //
    // These mirror the cookbook's `from_{schema}_schema_dot_lists_only_*`
    // tests. The cookbook catalog has Sales, HR, Archive, Staging schemas.

    #[test]
    fn from_sales_schema_dot_lists_only_sales_tables_and_views() {
        let items = complete("SELECT * FROM Sales.", 20, &cookbook_catalog());
        let got = labels(&items);
        assert!(got.contains(&"Sales.Customer".to_string()));
        assert!(got.contains(&"Sales.SalesOrder".to_string()));
        assert!(got.contains(&"Sales.vCustomerOrders".to_string()));
        // Tables from other schemas must NOT appear.
        assert!(!got.contains(&"HR.Department".to_string()));
        assert!(!got.contains(&"HR.Employee".to_string()));
        assert!(!got.contains(&"Archive.ArchivedOrder".to_string()));
        assert!(!got.contains(&"Staging.RawCustomerImport".to_string()));
    }

    #[test]
    fn from_hr_schema_dot_lists_only_hr_tables_and_views() {
        let items = complete("SELECT * FROM HR.", 17, &cookbook_catalog());
        let got = labels(&items);
        assert!(got.contains(&"HR.Department".to_string()));
        assert!(got.contains(&"HR.Employee".to_string()));
        assert!(got.contains(&"HR.vDepartmentRoster".to_string()));
        assert!(!got.contains(&"Sales.Customer".to_string()));
        assert!(!got.contains(&"Sales.SalesOrder".to_string()));
        assert!(!got.contains(&"Archive.ArchivedOrder".to_string()));
    }

    #[test]
    fn from_archive_schema_dot_lists_only_archive_tables() {
        let items = complete("SELECT * FROM Archive.", 22, &cookbook_catalog());
        let got = labels(&items);
        assert!(got.contains(&"Archive.ArchivedOrder".to_string()));
        assert!(!got.contains(&"Sales.Customer".to_string()));
        assert!(!got.contains(&"HR.Department".to_string()));
        assert!(!got.contains(&"Staging.RawCustomerImport".to_string()));
    }

    #[test]
    fn from_staging_schema_dot_lists_only_staging_tables() {
        let items = complete("SELECT * FROM Staging.", 22, &cookbook_catalog());
        let got = labels(&items);
        assert!(got.contains(&"Staging.RawCustomerImport".to_string()));
        assert!(!got.contains(&"Sales.Customer".to_string()));
        assert!(!got.contains(&"HR.Department".to_string()));
        assert!(!got.contains(&"Archive.ArchivedOrder".to_string()));
    }

    #[test]
    fn from_schema_dot_does_not_offer_procedures_functions_or_columns() {
        let items = complete("SELECT * FROM HR.", 17, &cookbook_catalog());
        assert!(
            !items.iter().any(|item| item.kind == CompletionKind::Column),
            "schema. should offer tables/views, not columns"
        );
        assert!(
            !items
                .iter()
                .any(|item| item.kind == CompletionKind::Procedure),
            "schema. should not offer procedures"
        );
        // No keyword noise after a dot.
        assert!(
            !items
                .iter()
                .any(|item| item.kind == CompletionKind::Keyword),
            "schema. should not offer keywords"
        );
    }

    // ---- Tier 3C: Insert text tests (ported from cookbook) --------------
    //
    // Schema-qualified completions should insert only the bare object name
    // (the schema is already typed). Unqualified completions should insert
    // the full schema-qualified name.

    #[test]
    fn schema_qualified_partial_insert_text_is_table_name_only() {
        let items = complete("SELECT * FROM Sales.Sal", 22, &cookbook_catalog());
        let item = find_item(&items, "Sales.SalesOrder");
        assert_eq!(
            item.insert_text, "SalesOrder",
            "schema-qualified completion should insert only the table name when schema is already typed"
        );
    }

    #[test]
    fn schema_qualified_partial_customer_insert_text_is_table_name_only() {
        let items = complete("SELECT * FROM Sales.Cus", 22, &cookbook_catalog());
        let item = find_item(&items, "Sales.Customer");
        assert_eq!(
            item.insert_text, "Customer",
            "schema-qualified completion should insert only the table name when schema is already typed"
        );
    }

    #[test]
    fn schema_qualified_partial_view_insert_text_is_view_name_only() {
        let items = complete("SELECT * FROM Sales.vCus", 23, &cookbook_catalog());
        let item = find_item(&items, "Sales.vCustomerOrders");
        assert_eq!(
            item.insert_text, "vCustomerOrders",
            "schema-qualified completion should insert only the view name when schema is already typed"
        );
    }

    #[test]
    fn unqualified_partial_insert_text_includes_schema() {
        let items = complete("SELECT * FROM Sal", 17, &cookbook_catalog());
        let item = find_item(&items, "Sales.SalesOrder");
        assert_eq!(
            item.insert_text, "Sales.SalesOrder",
            "unqualified completion should insert the full schema-qualified object name"
        );
    }

    #[test]
    fn schema_qualified_partial_does_not_insert_duplicate_schema() {
        let items = complete("SELECT * FROM Sales.Sal", 22, &cookbook_catalog());
        let item = find_item(&items, "Sales.SalesOrder");
        assert_ne!(
            item.insert_text, "Sales.SalesOrder",
            "schema-qualified completion must not insert Sales.SalesOrder after Sales. or it becomes Sales.Sales.SalesOrder"
        );
        assert_ne!(
            item.insert_text, "Sales.Sales.SalesOrder",
            "schema-qualified completion must never duplicate the schema"
        );
    }

    #[test]
    fn object_name_insert_text_keeps_schema_qualifier_for_dotted_label() {
        // Filesystem tables also produce schema-qualified labels with bare
        // insert text in the schema-dot path.
        let catalog = CompletionCatalog {
            databases: vec![],
            schemas: vec!["dbo".to_string()],
            objects: vec![CompletionObject::table(
                "dbo",
                "Users",
                vec!["Id".to_string()],
            )],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let items = complete("SELECT * FROM dbo.U", 19, &catalog);
        let item = find_item(&items, "dbo.Users");
        assert_eq!(item.insert_text, "Users");
    }
}
