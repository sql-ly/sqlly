//! Bridge from live metadata ([`MetadataService`]) into the canonical datamodel
//! ([`sqlly_lang::sqlmodel::ModelSnapshot`]).
//!
//! This is the layer that routes database metadata *into* the datamodel so
//! that IntelliSense always reads from a single source of truth. The resulting
//! `ModelSnapshot` can then be converted to a [`CompletionCatalog`] via
//! [`CompletionCatalog::from_model_snapshot`] for the completion engine, or
//! consumed directly by the model-engine's own completion path.
//!
//! Future metadata sources (filesystem DDL, catalog sync, etc.) plug into the
//! datamodel through `ModelSnapshot::apply`; IntelliSense always reads from the
//! snapshot, never from the raw source.

use sqlly_core::{ConnectionProfile, ObjectKind as CoreObjectKind, StudioResult};
use sqlly_intel::intellisense::CompletionCatalog;
use sqlly_lang::sqlmodel::{
    ColumnDef, Dialect, ForeignKeyDef, IncompleteRegions, KeyDef, ModelSnapshot, ObjectBody,
    ObjectFragment, ObjectKind as ModelObjectKind, ObjectRef, ParamDef, ParamMode, Provenance,
    RoutineBody, SqlName, TableBody, TypeRef, ViewBody,
};
use std::collections::BTreeSet;

use crate::intellisense::procedures::{ProcedureInfo, ProcedureParameter};
use crate::metadata::{MetadataNode, MetadataService};

/// Map `sqlly_core::ObjectKind` (metadata tree) to `sqlly_lang::sqlmodel::ObjectKind`
/// (datamodel). The core enum is the metadata-level kind set; the sqlmodel enum
/// is the parser-level kind set used by the datamodel.
fn core_to_model_kind(kind: &CoreObjectKind) -> Option<ModelObjectKind> {
    match kind {
        CoreObjectKind::Table => Some(ModelObjectKind::Table),
        CoreObjectKind::View => Some(ModelObjectKind::View),
        CoreObjectKind::StoredProcedure => Some(ModelObjectKind::Procedure),
        CoreObjectKind::ScalarFunction => Some(ModelObjectKind::ScalarFunction),
        CoreObjectKind::TableFunction => Some(ModelObjectKind::TableFunction),
        CoreObjectKind::AggregateFunction => Some(ModelObjectKind::AggregateFunction),
        CoreObjectKind::Trigger => Some(ModelObjectKind::Trigger),
        CoreObjectKind::Sequence => Some(ModelObjectKind::Sequence),
        CoreObjectKind::Synonym => Some(ModelObjectKind::Synonym),
        CoreObjectKind::Extension => Some(ModelObjectKind::Extension),
        CoreObjectKind::Event => Some(ModelObjectKind::Event),
        CoreObjectKind::MaterializedView => Some(ModelObjectKind::MaterializedView),
        CoreObjectKind::Type => Some(ModelObjectKind::DomainType),
        // Kinds that don't have a datamodel equivalent — not completion objects.
        CoreObjectKind::Server
        | CoreObjectKind::Database
        | CoreObjectKind::Schema
        | CoreObjectKind::Column
        | CoreObjectKind::Key
        | CoreObjectKind::Index
        | CoreObjectKind::Statistic
        | CoreObjectKind::Constraint
        | CoreObjectKind::User
        | CoreObjectKind::Role
        | CoreObjectKind::Login
        | CoreObjectKind::Filegroup
        | CoreObjectKind::PartitionScheme
        | CoreObjectKind::PartitionFunction
        | CoreObjectKind::Rule
        | CoreObjectKind::Default
        | CoreObjectKind::Assembly
        | CoreObjectKind::Job
        | CoreObjectKind::Schedule
        | CoreObjectKind::Operator
        | CoreObjectKind::Alert
        | CoreObjectKind::Endpoint
        | CoreObjectKind::LinkedServer
        | CoreObjectKind::Credential
        | CoreObjectKind::Certificate
        | CoreObjectKind::SymmetricKey
        | CoreObjectKind::AsymmetricKey
        | CoreObjectKind::Service
        | CoreObjectKind::Queue
        | CoreObjectKind::Contract
        | CoreObjectKind::MessageType
        | CoreObjectKind::Route
        | CoreObjectKind::BrokerPriority
        | CoreObjectKind::ErrorLog => None,
    }
}

/// Build an `ObjectRef` from a metadata node's schema and name.
fn ref_for(node: &MetadataNode, dialect: Dialect) -> ObjectRef {
    ObjectRef {
        catalog: None,
        namespace: node
            .schema
            .as_ref()
            .map(|s| SqlName::new(s, dialect, false)),
        name: SqlName::new(&node.name, dialect, false),
    }
}

/// Build a `TableBody` from column names (ordinal order), the table's
/// foreign-key column mappings, and its primary-key columns (in key order).
fn table_body(
    columns: &[BulkColumnInfo],
    foreign_keys: Vec<ForeignKeyDef>,
    primary_key: Vec<String>,
    dialect: Dialect,
) -> TableBody {
    let primary_key = if primary_key.is_empty() {
        None
    } else {
        Some(KeyDef {
            name: None,
            columns: primary_key
                .iter()
                .map(|c| SqlName::new(c, dialect, false))
                .collect(),
        })
    };
    TableBody {
        columns: typed_column_defs(columns, dialect),
        primary_key,
        unique_keys: Vec::new(),
        foreign_keys,
        checks: Vec::new(),
        options: std::collections::BTreeMap::new(),
    }
}

/// Build a `ViewBody` from bulk column info (ordinal order).
fn view_body(columns: &[BulkColumnInfo], dialect: Dialect) -> ViewBody {
    ViewBody {
        columns: typed_column_defs(columns, dialect),
        definition: None,
    }
}

/// Lower bulk column info into `ColumnDef`s, carrying the type, nullability,
/// and identity the dialect query reported (names-only fallbacks leave them
/// unset, exactly as before).
pub(crate) fn typed_column_defs(columns: &[BulkColumnInfo], dialect: Dialect) -> Vec<ColumnDef> {
    columns
        .iter()
        .enumerate()
        .map(|(i, column)| ColumnDef {
            name: SqlName::new(&column.name, dialect, false),
            ty: column
                .type_name
                .as_deref()
                .map(|t| TypeRef::parse(t, dialect)),
            nullable: column.nullable,
            default: None,
            identity: column.identity,
            generated: None,
            collation: None,
            ordinal: i,
        })
        .collect()
}

/// One column as the bulk listing reports it: name plus whatever typed
/// metadata the dialect query carries (absent on the per-object fallback).
#[derive(Debug, Clone)]
pub struct BulkColumnInfo {
    pub name: String,
    pub type_name: Option<String>,
    pub nullable: Option<bool>,
    pub identity: bool,
}

impl BulkColumnInfo {
    fn name_only(name: String) -> Self {
        Self {
            name,
            type_name: None,
            nullable: None,
            identity: false,
        }
    }
}

/// Columns for every table/view, keyed by lower-cased `(schema, name)`.
/// Loaded in one bulk round trip when the service supports it; `None` means
/// the caller must fall back to per-object `list_columns`.
type BulkColumnMap = std::collections::HashMap<(String, String), Vec<BulkColumnInfo>>;

async fn load_bulk_columns(
    metadata: &dyn MetadataService,
    profile: &ConnectionProfile,
    database: &str,
) -> Option<BulkColumnMap> {
    match metadata.list_all_columns(profile, database).await {
        Ok(rows) => {
            let mut map = BulkColumnMap::new();
            for row in rows {
                map.entry((
                    row.schema.to_ascii_lowercase(),
                    row.object.to_ascii_lowercase(),
                ))
                .or_default()
                .push(BulkColumnInfo {
                    name: row.column,
                    type_name: row.type_name,
                    nullable: row.nullable,
                    identity: row.is_identity,
                });
            }
            Some(map)
        }
        Err(e) => {
            tracing::debug!("bulk column listing unavailable, using per-object loads: {e}");
            None
        }
    }
}

/// Foreign-key defs for every table, keyed by lower-cased `(schema, name)` of
/// the child (referencing) table. Loaded in one bulk round trip; `None` when
/// the service doesn't support it or the query failed (FK metadata is a
/// best-effort enrichment, mirroring the Swift `SchemaProvider`).
type BulkForeignKeyMap = std::collections::HashMap<(String, String), Vec<ForeignKeyDef>>;

async fn load_bulk_foreign_keys(
    metadata: &dyn MetadataService,
    profile: &ConnectionProfile,
    database: &str,
    dialect: Dialect,
) -> Option<BulkForeignKeyMap> {
    match metadata.list_foreign_keys(profile, database).await {
        Ok(rows) => {
            let mut map = BulkForeignKeyMap::new();
            // One `ForeignKeyDef` per column mapping (compound keys yield one
            // def per column pair) — the same flattening the Swift
            // `SchemaProvider.parseForeignKeys` applies.
            for row in rows {
                map.entry((
                    row.from_schema.to_ascii_lowercase(),
                    row.from_table.to_ascii_lowercase(),
                ))
                .or_default()
                .push(ForeignKeyDef {
                    name: None,
                    columns: vec![SqlName::new(&row.from_column, dialect, false)],
                    target: ObjectRef {
                        catalog: None,
                        namespace: Some(SqlName::new(&row.to_schema, dialect, false)),
                        name: SqlName::new(&row.to_table, dialect, false),
                    },
                    target_columns: vec![SqlName::new(&row.to_column, dialect, false)],
                });
            }
            Some(map)
        }
        Err(e) => {
            tracing::debug!("foreign-key listing unavailable, joins get no FK hints: {e}");
            None
        }
    }
}

/// Primary-key column names (in key order) for every table, keyed by
/// lower-cased `(schema, name)`. Loaded in one best-effort bulk round trip;
/// `None` when the service doesn't support it or the query failed — in which
/// case tables get no primary key and the editor's unconstrained-mutation check
/// falls back to "any WHERE is constraint enough".
type BulkPrimaryKeyMap = std::collections::HashMap<(String, String), Vec<String>>;

async fn load_bulk_primary_keys(
    metadata: &dyn MetadataService,
    profile: &ConnectionProfile,
    database: &str,
) -> Option<BulkPrimaryKeyMap> {
    match metadata.list_primary_keys(profile, database).await {
        Ok(mut rows) => {
            // `key_ordinal` orders the columns within each key.
            rows.sort_by_key(|r| r.key_ordinal);
            let mut map = BulkPrimaryKeyMap::new();
            for row in rows {
                map.entry((
                    row.schema.to_ascii_lowercase(),
                    row.table.to_ascii_lowercase(),
                ))
                .or_default()
                .push(row.column);
            }
            Some(map)
        }
        Err(e) => {
            tracing::debug!("primary-key listing unavailable, tables get no key hints: {e}");
            None
        }
    }
}

/// Column names for one table/view: from the bulk map when available, else
/// one per-object `list_columns` round trip.
async fn columns_for(
    metadata: &dyn MetadataService,
    profile: &ConnectionProfile,
    database: &str,
    node: &MetadataNode,
    bulk: Option<&BulkColumnMap>,
) -> Vec<BulkColumnInfo> {
    let Some(schema) = &node.schema else {
        return Vec::new();
    };
    if let Some(map) = bulk {
        return map
            .get(&(schema.to_ascii_lowercase(), node.name.to_ascii_lowercase()))
            .cloned()
            .unwrap_or_default();
    }
    metadata
        .list_columns(profile, database, schema, &node.name)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|c| BulkColumnInfo::name_only(c.name))
        .collect()
}

/// Build a `RoutineBody` from procedure parameter descriptors.
fn routine_body(
    params: &[crate::intellisense::procedures::ProcedureParameter],
    dialect: Dialect,
) -> RoutineBody {
    RoutineBody {
        params: params
            .iter()
            .map(|p| ParamDef {
                name: SqlName::new(&p.name, dialect, false),
                ty: if p.data_type.is_empty() {
                    None
                } else {
                    Some(sqlly_lang::sqlmodel::TypeRef::parse(&p.data_type, dialect))
                },
                mode: if p.is_output {
                    ParamMode::Out
                } else {
                    ParamMode::In
                },
                default: None,
            })
            .collect(),
        returns: None,
        language: None,
        body_text: None,
    }
}

/// Determine the parser dialect from a connection profile: the nearest parser
/// family for the profile's database type (SQL Server family → `SqlServer`,
/// PostgreSQL wire family → `Postgres`, MySQL/MariaDB → `MySql`, SQLite →
/// `Sqlite`, …), so snapshots built from live metadata fold identifiers with
/// the engine's own rules.
fn dialect_for(profile: &ConnectionProfile) -> Dialect {
    profile.database_type.family()
}

/// Build a [`sqlly_lang::sqlmodel::ModelSnapshot`] from a live connection's metadata
/// via [`MetadataService`]. This is the key Layer 2 function: it routes
/// database metadata *into* the datamodel so IntelliSense always reads from a
/// single source of truth.
///
/// Schemas, tables, views, and programmable objects are loaded. Columns are
/// eagerly loaded for every table/view — via one bulk `list_all_columns`
/// round trip when the service supports it (the default executor opens a
/// fresh connection per query, so per-object loading takes minutes on large
/// databases), falling back to per-object `list_columns` otherwise.
/// Procedure parameters are loaded when `load_parameters` is true.
///
/// `list_schemas` errors propagate; per-kind object lists fall back to empty
/// on error so a partial snapshot is still usable (matching the existing
/// `build_completion_state` behavior).
pub async fn build_model_snapshot(
    metadata: &dyn MetadataService,
    profile: &ConnectionProfile,
    database: &str,
    load_parameters: bool,
) -> StudioResult<ModelSnapshot> {
    let dialect = dialect_for(profile);
    let profile_id = profile.id.0.to_string();

    let tables = metadata
        .list_tables(profile, database, None)
        .await
        .unwrap_or_default();
    let views = metadata
        .list_views(profile, database, None)
        .await
        .unwrap_or_default();
    let programmable = metadata
        .list_programmable_objects(profile, database, None)
        .await
        .unwrap_or_default();

    let mut fragments: Vec<ObjectFragment> = Vec::new();

    // Columns for every table/view: one bulk round trip when supported;
    // otherwise the per-object fallback below (one query per object).
    let bulk_columns = load_bulk_columns(metadata, profile, database).await;

    // Foreign keys for every table: one best-effort bulk round trip. They
    // feed the FK-based JOIN suggestions; a failure just means no FK hints.
    let mut bulk_fks = load_bulk_foreign_keys(metadata, profile, database, dialect)
        .await
        .unwrap_or_default();

    // Primary keys for every table: one best-effort bulk round trip. They let
    // the editor tell a single-row UPDATE/DELETE from an unconstrained one.
    let mut bulk_pks = load_bulk_primary_keys(metadata, profile, database)
        .await
        .unwrap_or_default();

    // Tables → ObjectFragment with TableBody (columns eagerly loaded)
    for node in &tables {
        if core_to_model_kind(&node.kind).is_none() {
            continue;
        }
        let columns = columns_for(metadata, profile, database, node, bulk_columns.as_ref()).await;
        let foreign_keys = node
            .schema
            .as_ref()
            .and_then(|schema| {
                bulk_fks.remove(&(schema.to_ascii_lowercase(), node.name.to_ascii_lowercase()))
            })
            .unwrap_or_default();
        let primary_key = node
            .schema
            .as_ref()
            .and_then(|schema| {
                bulk_pks.remove(&(schema.to_ascii_lowercase(), node.name.to_ascii_lowercase()))
            })
            .unwrap_or_default();
        fragments.push(ObjectFragment {
            reference: ref_for(node, dialect),
            kind: ModelObjectKind::Table,
            dialect,
            body: ObjectBody::Table(table_body(&columns, foreign_keys, primary_key, dialect)),
            provenance: Provenance::Database {
                profile: profile_id.clone(),
            },
            fidelity: sqlly_lang::sqlmodel::Fidelity::Exact,
            references: Vec::new(),
        });
    }

    // Views → ObjectFragment with ViewBody (columns eagerly loaded)
    for node in &views {
        if core_to_model_kind(&node.kind).is_none() {
            continue;
        }
        let columns = columns_for(metadata, profile, database, node, bulk_columns.as_ref()).await;
        fragments.push(ObjectFragment {
            reference: ref_for(node, dialect),
            kind: ModelObjectKind::View,
            dialect,
            body: ObjectBody::View(view_body(&columns, dialect)),
            provenance: Provenance::Database {
                profile: profile_id.clone(),
            },
            fidelity: sqlly_lang::sqlmodel::Fidelity::Exact,
            references: Vec::new(),
        });
    }

    // Programmable objects → ObjectFragment with RoutineBody
    for node in &programmable {
        let Some(model_kind) = core_to_model_kind(&node.kind) else {
            continue;
        };
        let params = if load_parameters {
            if let Some(schema) = &node.schema {
                metadata
                    .list_procedure_parameters(profile, database, schema, &node.name)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        fragments.push(ObjectFragment {
            reference: ref_for(node, dialect),
            kind: model_kind,
            dialect,
            body: ObjectBody::Routine(routine_body(&params, dialect)),
            provenance: Provenance::Database {
                profile: profile_id.clone(),
            },
            fidelity: sqlly_lang::sqlmodel::Fidelity::Exact,
            references: Vec::new(),
        });
    }

    let empty = ModelSnapshot::empty();
    let (snapshot, _) = empty.apply(1, fragments, &BTreeSet::new(), IncompleteRegions::default());

    Ok(snapshot)
}

/// Extract stored-procedure descriptors from a `ModelSnapshot`. Procedures in
/// the model (`ObjectKind::Procedure`) become `ProcedureInfo` entries with
/// their parameters extracted from the `RoutineBody`. This is the datamodel-
/// sourced equivalent of `procedures_from_metadata`.
pub fn procedures_from_snapshot(snapshot: &ModelSnapshot) -> Vec<ProcedureInfo> {
    snapshot
        .objects()
        .filter(|entry| entry.kind == ModelObjectKind::Procedure)
        .map(|entry| {
            let schema = entry
                .reference
                .namespace
                .as_ref()
                .map(|n| n.spelling().to_string())
                .unwrap_or_default();
            let name = entry.reference.name.spelling().to_string();
            let params = entry
                .routine()
                .map(|r| {
                    r.params
                        .iter()
                        .map(|p| ProcedureParameter {
                            name: p.name.spelling().to_string(),
                            data_type: p.ty.as_ref().map(|t| t.native.clone()).unwrap_or_default(),
                            is_output: p.mode == ParamMode::Out || p.mode == ParamMode::InOut,
                        })
                        .collect()
                })
                .unwrap_or_default();
            ProcedureInfo {
                schema,
                name,
                parameters: params,
            }
        })
        .collect()
}

/// Incrementally update a table/view's columns in the `ModelSnapshot`. This
/// keeps the snapshot in sync with the catalog when the schema tree lazily
/// loads columns (`SchemaTreeEvent::ColumnsLoaded`). The snapshot is the single
/// source of truth — the catalog is derived from it, so both must stay aligned.
///
/// Returns `true` if a matching table/view was found and updated.
/// Rebuild a column list from refreshed names, carrying over the typed
/// metadata (type, nullability, identity, …) of columns the snapshot already
/// knew — a names-only refresh must never strip what the typed bulk load
/// established.
fn merged_column_defs(
    existing: &[ColumnDef],
    names: &[String],
    dialect: Dialect,
) -> Vec<ColumnDef> {
    names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            match existing
                .iter()
                .find(|c| c.name.spelling().eq_ignore_ascii_case(name))
            {
                Some(known) => ColumnDef {
                    ordinal: i,
                    ..known.clone()
                },
                None => ColumnDef {
                    name: SqlName::new(name, dialect, false),
                    ty: None,
                    nullable: None,
                    default: None,
                    identity: false,
                    generated: None,
                    collation: None,
                    ordinal: i,
                },
            }
        })
        .collect()
}

pub fn merge_columns_into_snapshot(
    snapshot: &mut ModelSnapshot,
    dialect: Dialect,
    schema: &str,
    name: &str,
    columns: Vec<String>,
) -> bool {
    let reference = ObjectRef::new(
        None,
        Some(SqlName::new(schema, dialect, false)),
        SqlName::new(name, dialect, false),
    );

    let entries = snapshot.lookup(&reference);
    if entries.is_empty() {
        return false;
    }

    let entry = &entries[0];
    let entry_dialect = entry.dialect;

    let new_body = match &entry.body {
        ObjectBody::Table(t) => {
            let mut new_table = t.clone();
            new_table.columns = merged_column_defs(&t.columns, &columns, entry_dialect);
            ObjectBody::Table(new_table)
        }
        ObjectBody::View(v) => {
            let mut new_view = v.clone();
            new_view.columns = merged_column_defs(&v.columns, &columns, entry_dialect);
            ObjectBody::View(new_view)
        }
        _ => return false,
    };

    let fragment = ObjectFragment {
        reference: entry.reference.clone(),
        kind: entry.kind,
        dialect: entry_dialect,
        body: new_body,
        provenance: entry.provenance.clone(),
        fidelity: entry.fidelity.clone(),
        references: Vec::new(),
    };

    let (new_snapshot, _) = snapshot.apply(
        snapshot.generation + 1,
        vec![fragment],
        &BTreeSet::new(),
        IncompleteRegions::default(),
    );
    *snapshot = new_snapshot;
    true
}

/// Incrementally update a stored procedure's parameters in the
/// `ModelSnapshot`. This keeps the snapshot in sync with the procedure list
/// when the schema tree lazily loads parameters
/// (`SchemaTreeEvent::ProcedureParametersLoaded`).
///
/// Returns `true` if a matching routine was found and updated.
pub fn merge_procedure_params_into_snapshot(
    snapshot: &mut ModelSnapshot,
    dialect: Dialect,
    schema: &str,
    name: &str,
    parameters: Vec<ProcedureParameter>,
) -> bool {
    let reference = ObjectRef::new(
        None,
        Some(SqlName::new(schema, dialect, false)),
        SqlName::new(name, dialect, false),
    );

    let entries = snapshot.lookup(&reference);
    if entries.is_empty() {
        return false;
    }

    let entry = &entries[0];
    let entry_dialect = entry.dialect;

    let new_body = match &entry.body {
        ObjectBody::Routine(r) => {
            let mut new_routine = r.clone();
            new_routine.params = parameters
                .iter()
                .map(|p| ParamDef {
                    name: SqlName::new(&p.name, entry_dialect, false),
                    ty: if p.data_type.is_empty() {
                        None
                    } else {
                        Some(TypeRef::parse(&p.data_type, entry_dialect))
                    },
                    mode: if p.is_output {
                        ParamMode::Out
                    } else {
                        ParamMode::In
                    },
                    default: None,
                })
                .collect();
            ObjectBody::Routine(new_routine)
        }
        _ => return false,
    };

    let fragment = ObjectFragment {
        reference: entry.reference.clone(),
        kind: entry.kind,
        dialect: entry_dialect,
        body: new_body,
        provenance: entry.provenance.clone(),
        fidelity: entry.fidelity.clone(),
        references: Vec::new(),
    };

    let (new_snapshot, _) = snapshot.apply(
        snapshot.generation + 1,
        vec![fragment],
        &BTreeSet::new(),
        IncompleteRegions::default(),
    );
    *snapshot = new_snapshot;
    true
}

/// Convenience: build a `ModelSnapshot` and immediately convert it to a
/// `CompletionCatalog`, merging in the schema list from `list_schemas`. This
/// is the Layer 2 + Layer 1 bridge combined, for callers that want the catalog
/// without keeping the snapshot.
pub async fn build_catalog_from_metadata(
    metadata: &dyn MetadataService,
    profile: &ConnectionProfile,
    database: &str,
    load_parameters: bool,
) -> StudioResult<CompletionCatalog> {
    let snapshot = build_model_snapshot(metadata, profile, database, load_parameters).await?;
    let mut catalog = CompletionCatalog::from_model_snapshot(&snapshot);

    // Merge schemas from list_schemas — the snapshot only knows schemas that
    // contain objects; empty schemas still need to appear for schema-qualified
    // completion to work.
    let schemas = metadata.list_schemas(profile, database).await?;
    for schema in schemas {
        if !catalog
            .schemas
            .iter()
            .any(|s| s.eq_ignore_ascii_case(&schema))
        {
            catalog.schemas.push(schema);
        }
    }

    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intellisense::procedures::ProcedureParameter;
    use async_trait::async_trait;
    use sqlly_core::{SecretProvider, SecretRef};

    fn node(name: &str, schema: Option<&str>, kind: CoreObjectKind) -> MetadataNode {
        let mut n = MetadataNode::new(name, kind);
        if let Some(s) = schema {
            n = n.with_schema(s);
        }
        n
    }

    #[derive(Default)]
    struct FakeMeta {
        col_calls: std::sync::atomic::AtomicUsize,
        /// When true, `list_all_columns` succeeds (bulk path); when false it
        /// keeps the trait default (`NotImplemented`) so the per-object
        /// fallback is exercised.
        bulk_columns: bool,
        /// When true, `list_foreign_keys` reports Orders.CustomerId ->
        /// Customers.CustomerId; when false it keeps the trait default
        /// (`NotImplemented`) so the best-effort fallback is exercised.
        foreign_keys: bool,
    }

    #[async_trait]
    impl MetadataService for FakeMeta {
        async fn list_schemas(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
        ) -> StudioResult<Vec<String>> {
            Ok(vec!["dbo".into(), "sales".into()])
        }
        async fn list_all_columns(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
        ) -> StudioResult<Vec<crate::metadata::BulkColumnRow>> {
            if !self.bulk_columns {
                return Err(sqlly_core::StudioError::NotImplemented("list_all_columns"));
            }
            let row = |object: &str, column: &str| crate::metadata::BulkColumnRow {
                schema: "dbo".into(),
                object: object.into(),
                column: column.into(),
                type_name: Some("int".into()),
                nullable: Some(true),
                is_identity: false,
            };
            Ok(vec![
                row("Orders", "OrderId"),
                row("Orders", "CustomerId"),
                row("Orders", "CreatedAt"),
                row("Customers", "CustomerId"),
                row("Customers", "CustomerName"),
                row("ActiveOrders", "OrderId"),
                row("ActiveOrders", "Status"),
            ])
        }
        async fn list_foreign_keys(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
        ) -> StudioResult<Vec<crate::metadata::ForeignKeyRow>> {
            if !self.foreign_keys {
                return Err(sqlly_core::StudioError::NotImplemented("list_foreign_keys"));
            }
            Ok(vec![crate::metadata::ForeignKeyRow {
                constraint_name: "FK_Orders_Customers".into(),
                from_schema: "dbo".into(),
                from_table: "Orders".into(),
                from_column: "CustomerId".into(),
                to_schema: "dbo".into(),
                to_table: "Customers".into(),
                to_column: "CustomerId".into(),
            }])
        }
        async fn list_tables(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: Option<&str>,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(vec![
                node("Orders", Some("dbo"), CoreObjectKind::Table),
                node("Customers", Some("dbo"), CoreObjectKind::Table),
            ])
        }
        async fn list_views(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: Option<&str>,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(vec![node(
                "ActiveOrders",
                Some("dbo"),
                CoreObjectKind::View,
            )])
        }
        async fn list_programmable_objects(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: Option<&str>,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(vec![
                node("GetUser", Some("dbo"), CoreObjectKind::StoredProcedure),
                node("udf_calc", Some("dbo"), CoreObjectKind::ScalarFunction),
            ])
        }
        async fn list_columns(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: &str,
            obj: &str,
        ) -> StudioResult<Vec<MetadataNode>> {
            self.col_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match obj {
                "Orders" => Ok(vec![
                    node("OrderId", None, CoreObjectKind::Column),
                    node("CustomerId", None, CoreObjectKind::Column),
                    node("CreatedAt", None, CoreObjectKind::Column),
                ]),
                "Customers" => Ok(vec![
                    node("CustomerId", None, CoreObjectKind::Column),
                    node("CustomerName", None, CoreObjectKind::Column),
                ]),
                "ActiveOrders" => Ok(vec![
                    node("OrderId", None, CoreObjectKind::Column),
                    node("Status", None, CoreObjectKind::Column),
                ]),
                _ => Ok(vec![]),
            }
        }
        async fn list_procedure_parameters(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: &str,
            _o: &str,
        ) -> StudioResult<Vec<ProcedureParameter>> {
            Ok(vec![
                ProcedureParameter::new("@id", "int"),
                ProcedureParameter::new("@name", "nvarchar(100)"),
            ])
        }
        async fn list_indexes(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: &str,
            _t: &str,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(vec![])
        }
        async fn list_triggers(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: &str,
            _t: &str,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(vec![])
        }
        async fn list_constraints(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: &str,
            _t: &str,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(vec![])
        }
        async fn list_statistics(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: &str,
            _t: &str,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(vec![])
        }
    }

    fn test_profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "test",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        )
    }

    #[tokio::test]
    async fn build_model_snapshot_loads_tables_with_columns() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");

        assert_eq!(snap.len(), 5); // 2 tables + 1 view + 2 programmable

        let catalog = CompletionCatalog::from_model_snapshot(&snap);
        let orders = catalog
            .objects
            .iter()
            .find(|o| o.name == "Orders")
            .expect("Orders should be in snapshot");
        assert_eq!(orders.kind, CoreObjectKind::Table);
        assert_eq!(orders.columns, vec!["OrderId", "CustomerId", "CreatedAt"]);

        let customers = catalog
            .objects
            .iter()
            .find(|o| o.name == "Customers")
            .expect("Customers should be in snapshot");
        assert_eq!(customers.columns, vec!["CustomerId", "CustomerName"]);
    }

    #[tokio::test]
    async fn build_model_snapshot_loads_foreign_keys() {
        // FK metadata flows: MetadataService -> ModelSnapshot table body ->
        // CompletionCatalog FkRelation -> ready-to-use join suggestion.
        let meta = FakeMeta {
            foreign_keys: true,
            ..Default::default()
        };
        let profile = test_profile();
        let snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");

        let catalog = CompletionCatalog::from_model_snapshot(&snap);
        let orders = catalog
            .objects
            .iter()
            .find(|o| o.name == "Orders")
            .expect("Orders should be in snapshot");
        assert_eq!(orders.foreign_keys.len(), 1);
        let fk = &orders.foreign_keys[0];
        assert_eq!(fk.column, "CustomerId");
        assert_eq!(fk.target_schema, "dbo");
        assert_eq!(fk.target_table, "Customers");
        assert_eq!(fk.target_column, "CustomerId");

        // And the catalog produces the Swift-parity join suggestion.
        let svc = crate::intellisense::DefaultIntellisense;
        let cfg = crate::intellisense::parse(&crate::intellisense::default_config_text()).unwrap();
        let sql = "SELECT * FROM dbo.Orders o JOIN ";
        let req =
            crate::intellisense::CompletionRequest::manual(sql, sql.len(), &catalog, &[], &cfg);
        let items =
            crate::intellisense::IntellisenseService::complete_enriched_response(&svc, &req).items;
        assert!(
            items
                .iter()
                .any(|i| i.insert_text == "dbo.Customers c ON c.CustomerId = o.CustomerId"),
            "expected a ready-to-use FK join from live metadata, got: {:?}",
            items.iter().map(|i| &i.insert_text).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn build_model_snapshot_missing_fk_support_yields_empty_fks() {
        // The default `list_foreign_keys` (NotImplemented) must not fail the
        // snapshot build — FK hints are a best-effort enrichment.
        let meta = FakeMeta::default();
        let profile = test_profile();
        let snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");
        let catalog = CompletionCatalog::from_model_snapshot(&snap);
        let orders = catalog.objects.iter().find(|o| o.name == "Orders").unwrap();
        assert!(orders.foreign_keys.is_empty());
    }

    #[tokio::test]
    async fn build_model_snapshot_loads_views_with_columns() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");

        let catalog = CompletionCatalog::from_model_snapshot(&snap);
        let view = catalog
            .objects
            .iter()
            .find(|o| o.name == "ActiveOrders")
            .expect("ActiveOrders view should be in snapshot");
        assert_eq!(view.kind, CoreObjectKind::View);
        assert_eq!(view.columns, vec!["OrderId", "Status"]);
    }

    #[tokio::test]
    async fn build_model_snapshot_loads_procedures_with_parameters_when_eager() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let snap = build_model_snapshot(&meta, &profile, "appdb", true)
            .await
            .expect("build snapshot");

        let catalog = CompletionCatalog::from_model_snapshot(&snap);
        let proc = catalog
            .objects
            .iter()
            .find(|o| o.name == "GetUser")
            .expect("GetUser procedure should be in snapshot");
        assert_eq!(proc.kind, CoreObjectKind::StoredProcedure);
        assert_eq!(proc.parameters, vec!["@id", "@name"]);
    }

    #[tokio::test]
    async fn build_model_snapshot_skips_parameters_when_lazy() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");

        let catalog = CompletionCatalog::from_model_snapshot(&snap);
        let proc = catalog
            .objects
            .iter()
            .find(|o| o.name == "GetUser")
            .expect("GetUser procedure should be in snapshot");
        assert!(proc.parameters.is_empty());
    }

    #[tokio::test]
    async fn build_model_snapshot_eagerly_loads_all_columns() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let _snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");

        // 2 tables + 1 view = 3 list_columns calls
        assert_eq!(meta.col_calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn build_model_snapshot_prefers_bulk_column_listing() {
        let meta = FakeMeta {
            bulk_columns: true,
            ..FakeMeta::default()
        };
        let profile = test_profile();
        let snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");

        // Bulk path: zero per-object list_columns round trips…
        assert_eq!(meta.col_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        // …and the columns still land on every table/view.
        let catalog = CompletionCatalog::from_model_snapshot(&snap);
        let orders = catalog.objects.iter().find(|o| o.name == "Orders").unwrap();
        assert_eq!(orders.columns, vec!["OrderId", "CustomerId", "CreatedAt"]);
        let view = catalog
            .objects
            .iter()
            .find(|o| o.name == "ActiveOrders")
            .unwrap();
        assert_eq!(view.columns, vec!["OrderId", "Status"]);
    }

    #[tokio::test]
    async fn build_model_snapshot_completions_work() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");
        let catalog = CompletionCatalog::from_model_snapshot(&snap);

        let items = sqlly_intel::intellisense::complete("SELECT * FROM ord", 17, &catalog);
        assert!(
            items.iter().any(|i| i.label == "dbo.Orders"),
            "completion from model snapshot should find Orders, got {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn build_catalog_from_metadata_works() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let catalog = build_catalog_from_metadata(&meta, &profile, "appdb", false)
            .await
            .expect("build catalog");

        assert!(catalog.schemas.contains(&"dbo".to_string()));
        assert!(catalog.schemas.contains(&"sales".to_string()));
        assert!(catalog.objects.iter().any(|o| o.name == "Orders"));
        assert!(catalog.objects.iter().any(|o| o.name == "ActiveOrders"));
        assert!(catalog.objects.iter().any(|o| o.name == "GetUser"));
    }

    #[test]
    fn typed_column_defs_carry_bulk_metadata() {
        let columns = vec![
            BulkColumnInfo {
                name: "id".into(),
                type_name: Some("int".into()),
                nullable: Some(false),
                identity: true,
            },
            BulkColumnInfo::name_only("note".into()),
        ];
        let defs = typed_column_defs(&columns, Dialect::SqlServer);
        assert_eq!(defs[0].ty.as_ref().map(|t| t.native.as_str()), Some("int"));
        assert_eq!(defs[0].nullable, Some(false));
        assert!(defs[0].identity);
        assert!(defs[1].ty.is_none());
        assert_eq!(defs[1].nullable, None);
    }

    #[test]
    fn merged_column_defs_preserve_known_types_on_name_refresh() {
        let existing = typed_column_defs(
            &[BulkColumnInfo {
                name: "id".into(),
                type_name: Some("bigint".into()),
                nullable: Some(false),
                identity: true,
            }],
            Dialect::SqlServer,
        );
        let merged = merged_column_defs(
            &existing,
            &["id".to_string(), "added".to_string()],
            Dialect::SqlServer,
        );
        assert_eq!(
            merged[0].ty.as_ref().map(|t| t.native.as_str()),
            Some("bigint"),
            "a names-only refresh must not strip known types"
        );
        assert!(merged[0].identity);
        assert!(merged[1].ty.is_none());
        assert_eq!(merged[1].ordinal, 1);
    }

    #[tokio::test]
    async fn merge_columns_into_snapshot_updates_table() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let mut snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");

        // Verify initial columns from eager load
        let catalog = CompletionCatalog::from_model_snapshot(&snap);
        let orders = catalog.objects.iter().find(|o| o.name == "Orders").unwrap();
        assert_eq!(orders.columns, vec!["OrderId", "CustomerId", "CreatedAt"]);

        // Now merge new columns
        let updated = merge_columns_into_snapshot(
            &mut snap,
            Dialect::SqlServer,
            "dbo",
            "Orders",
            vec!["NewCol1".into(), "NewCol2".into()],
        );
        assert!(updated, "should find and update Orders");

        // Verify the snapshot now has the new columns
        let catalog = CompletionCatalog::from_model_snapshot(&snap);
        let orders = catalog.objects.iter().find(|o| o.name == "Orders").unwrap();
        assert_eq!(orders.columns, vec!["NewCol1", "NewCol2"]);
    }

    #[tokio::test]
    async fn merge_columns_into_snapshot_updates_view() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let mut snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");

        let updated = merge_columns_into_snapshot(
            &mut snap,
            Dialect::SqlServer,
            "dbo",
            "ActiveOrders",
            vec!["ViewCol1".into(), "ViewCol2".into()],
        );
        assert!(updated, "should find and update ActiveOrders view");

        let catalog = CompletionCatalog::from_model_snapshot(&snap);
        let view = catalog
            .objects
            .iter()
            .find(|o| o.name == "ActiveOrders")
            .unwrap();
        assert_eq!(view.columns, vec!["ViewCol1", "ViewCol2"]);
    }

    #[tokio::test]
    async fn merge_columns_into_snapshot_returns_false_for_missing() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let mut snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");

        let updated = merge_columns_into_snapshot(
            &mut snap,
            Dialect::SqlServer,
            "dbo",
            "NonExistent",
            vec!["Col".into()],
        );
        assert!(!updated, "should return false for missing table");
    }

    #[tokio::test]
    async fn merge_procedure_params_into_snapshot_updates_routine() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let mut snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");

        // Verify initial params are empty (lazy load)
        let procs = procedures_from_snapshot(&snap);
        let get_user = procs.iter().find(|p| p.name == "GetUser").unwrap();
        assert!(get_user.parameters.is_empty());

        // Now merge params
        let updated = merge_procedure_params_into_snapshot(
            &mut snap,
            Dialect::SqlServer,
            "dbo",
            "GetUser",
            vec![
                ProcedureParameter::new("@id", "int"),
                ProcedureParameter::new("@name", "nvarchar(100)"),
            ],
        );
        assert!(updated, "should find and update GetUser");

        // Verify the snapshot now has params
        let procs = procedures_from_snapshot(&snap);
        let get_user = procs.iter().find(|p| p.name == "GetUser").unwrap();
        assert_eq!(get_user.parameters.len(), 2);
        assert_eq!(get_user.parameters[0].name, "@id");
        assert_eq!(get_user.parameters[1].name, "@name");
    }

    #[tokio::test]
    async fn merge_procedure_params_into_snapshot_returns_false_for_missing() {
        let meta = FakeMeta::default();
        let profile = test_profile();
        let mut snap = build_model_snapshot(&meta, &profile, "appdb", false)
            .await
            .expect("build snapshot");

        let updated = merge_procedure_params_into_snapshot(
            &mut snap,
            Dialect::SqlServer,
            "dbo",
            "NonExistent",
            vec![ProcedureParameter::new("@id", "int")],
        );
        assert!(!updated, "should return false for missing procedure");
    }
}
