//! Table definitions for the plan findings, taken from the data-model
//! snapshot IntelliSense already holds for the connection.
//!
//! Nothing here runs a query: the snapshot is whatever the completion cache
//! loaded earlier. It knows columns, types, nullability, primary keys, unique
//! constraints and foreign keys, but not ordinary secondary indexes, so the
//! findings that reason from "this table has no such index" are dropped again
//! by [`finding_is_reliable`].

use std::collections::HashSet;

use sqlly_lang::sqlmodel::{KeyDef, ModelSnapshot, ObjectKind, TableBody};
use sqlly_services::plans::{
    ExecutionPlan, FindingKind, PlanContext, PlanDialect, PlanFinding, SchemaColumn, SchemaContext,
    SchemaForeignKey, SchemaIndex, SchemaIndexColumn, SchemaTable, resolve_batch_aliases,
};

/// Build a schema context covering the tables `plans` read or write. `None`
/// when the snapshot knows none of them.
pub(crate) fn schema_for_plans(
    snapshot: &ModelSnapshot,
    dialect: PlanDialect,
    plans: &[ExecutionPlan],
    query_text: Option<&str>,
) -> Option<SchemaContext> {
    // MySQL, MariaDB and SQLite plans print the query's aliases; read the
    // real table names out of the SQL first so those tables are found.
    let mut named = plans.to_vec();
    resolve_batch_aliases(&mut named, &PlanContext::new().with_query_text(query_text));
    let wanted: HashSet<String> = named
        .iter()
        .flat_map(|plan| plan.nodes.iter())
        .filter_map(|node| node.relation.as_deref())
        .map(fold)
        .filter(|name| !name.is_empty())
        .collect();
    if wanted.is_empty() {
        return None;
    }
    let mut tables: Vec<SchemaTable> = snapshot
        .objects()
        .filter(|entry| entry.kind == ObjectKind::Table)
        .filter(|entry| wanted.contains(&fold(entry.reference.name.spelling())))
        .filter_map(|entry| {
            let body = entry.table()?;
            if body.columns.is_empty() {
                return None;
            }
            Some(table_from_body(
                entry
                    .reference
                    .namespace
                    .as_ref()
                    .map(|name| name.spelling())
                    .unwrap_or_default(),
                entry.reference.name.spelling(),
                body,
            ))
        })
        .collect();
    if tables.is_empty() {
        return None;
    }
    // Snapshot iteration order is by object id; sort so the same catalog
    // always yields the same findings order.
    tables.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
    Some(SchemaContext::from_tables(dialect, tables))
}

fn fold(name: &str) -> String {
    name.trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'))
        .to_lowercase()
}

fn key_index(key: &KeyDef, primary: bool) -> SchemaIndex {
    SchemaIndex {
        name: key
            .name
            .as_ref()
            .map(|name| name.spelling().to_string())
            .unwrap_or_default(),
        columns: key
            .columns
            .iter()
            .map(|column| SchemaIndexColumn {
                name: column.spelling().to_string(),
                ..SchemaIndexColumn::default()
            })
            .collect(),
        unique: true,
        primary,
        ..SchemaIndex::default()
    }
}

fn table_from_body(schema: &str, name: &str, body: &TableBody) -> SchemaTable {
    SchemaTable {
        schema: schema.to_string(),
        name: name.to_string(),
        columns: body
            .columns
            .iter()
            .map(|column| SchemaColumn {
                name: column.name.spelling().to_string(),
                type_name: column
                    .ty
                    .as_ref()
                    .map(|ty| ty.native.clone())
                    .unwrap_or_default(),
                // Unknown nullability is treated as nullable: the cautious
                // reading for the NOT IN finding is the only one that uses it.
                nullable: column.nullable.unwrap_or(true),
                collation: column.collation.clone(),
                identity: column.identity,
                computed: column.generated.is_some(),
            })
            .collect(),
        primary_key: body.primary_key.as_ref().map(|key| key_index(key, true)),
        indexes: body
            .unique_keys
            .iter()
            .map(|key| key_index(key, false))
            .collect(),
        foreign_keys: body
            .foreign_keys
            .iter()
            .map(|fk| SchemaForeignKey {
                name: fk
                    .name
                    .as_ref()
                    .map(|name| name.spelling().to_string())
                    .unwrap_or_default(),
                columns: fk
                    .columns
                    .iter()
                    .map(|c| c.spelling().to_string())
                    .collect(),
                referenced_schema: fk
                    .target
                    .namespace
                    .as_ref()
                    .map(|name| name.spelling().to_string())
                    .unwrap_or_default(),
                referenced_table: fk.target.name.spelling().to_string(),
                referenced_columns: fk
                    .target_columns
                    .iter()
                    .map(|c| c.spelling().to_string())
                    .collect(),
            })
            .collect(),
        row_count: None,
        partition_key: None,
    }
}

/// Whether a finding can be trusted when the schema came from the snapshot.
/// The snapshot lists keys and constraints only, so a rule that concludes
/// something from an index being absent would be guessing.
pub(crate) fn finding_is_reliable(finding: &PlanFinding) -> bool {
    !matches!(
        finding.kind,
        FindingKind::SchemaStale | FindingKind::SchemaFkUnindexed | FindingKind::SchemaHeapTable
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fold_without_quotes() {
        assert_eq!(fold("[Orders]"), "orders");
        assert_eq!(fold("\"Order Lines\""), "order lines");
        assert_eq!(fold("`t`"), "t");
    }

    #[test]
    fn an_empty_snapshot_gives_no_schema() {
        let plan = ExecutionPlan::default();
        assert!(
            schema_for_plans(
                &ModelSnapshot::empty(),
                PlanDialect::Postgres,
                &[plan],
                None
            )
            .is_none()
        );
    }
}
