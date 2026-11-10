//! Bridge from the live metadata tree ([`MetadataNode`]) to the completion
//! engine's [`CompletionCatalog`] + procedure snippet list.
//!
//! The gpui app (and the LSP server) load schemas/tables/views/programmable
//! objects via [`MetadataService`](crate::metadata::MetadataService) and hand
//! the resulting nodes here to build the catalog the completion engine reads.
//! Columns are eagerly loaded for every table/view during
//! [`build_completion_state`] so alias-dot completion works immediately;
//! procedure parameters are filled on-demand when a parameter query is
//! available (see [`merge_procedure_parameters`]).

use sqlly_core::{ConnectionProfile, StudioResult};
use sqlly_intel::intellisense::CompletionCatalog;

use crate::intellisense::procedures::{ProcedureInfo, ProcedureParameter};
#[cfg(test)]
use crate::metadata::MetadataNode;
use crate::metadata::MetadataService;
#[cfg(test)]
use sqlly_core::ObjectKind;
#[cfg(test)]
use sqlly_intel::intellisense::CompletionObject;

/// The object kinds that surface as completion objects. Column / index /
/// trigger / constraint / statistic nodes are not objects in the completion
/// sense (columns are attached to their parent table/view).
#[cfg(test)]
fn is_completion_object(kind: &ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Table
            | ObjectKind::View
            | ObjectKind::StoredProcedure
            | ObjectKind::ScalarFunction
            | ObjectKind::TableFunction
            | ObjectKind::AggregateFunction
            | ObjectKind::Type
    )
}

/// Build a [`CompletionCatalog`] from the loaded databases, schemas, and object
/// nodes. Objects carry empty `columns`/`parameters` here; call
/// [`merge_columns`] / [`merge_procedure_parameters`] to enrich them
/// incrementally. Snippets default to the built-in set.
///
/// **Deprecated path**: This builds a catalog directly from metadata nodes,
/// bypassing the `ModelSnapshot` datamodel. It is kept `pub(crate)` for test
/// fixtures only. Production code must use [`build_completion_state`] which
/// routes metadata through the datamodel first.
#[cfg(test)]
pub(crate) fn catalog_from_metadata(
    databases: Vec<String>,
    schemas: Vec<String>,
    object_nodes: &[MetadataNode],
) -> CompletionCatalog {
    let objects = object_nodes
        .iter()
        .filter(|node| is_completion_object(&node.kind))
        .map(|node| CompletionObject {
            database: None,
            schema: node.schema.clone(),
            name: node.name.clone(),
            kind: node.kind.clone(),
            columns: Vec::new(),
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        })
        .collect();
    CompletionCatalog {
        databases,
        schemas,
        objects,
        filesystem_schemas: Vec::new(),
        snippets: sqlly_intel::intellisense::default_snippets(),
    }
}

/// Build the procedure-snippet list from programmable-object nodes. Procedures
/// start with empty parameters; fill them with [`merge_procedure_parameters`].
/// Only `StoredProcedure` nodes become [`ProcedureInfo`] (functions are offered
/// as plain function-name completions, not EXEC snippets).
///
/// **Deprecated path**: This builds procedures directly from metadata nodes,
/// bypassing the `ModelSnapshot` datamodel. It is kept `pub(crate)` for test
/// fixtures only. Production code must use [`procedures_from_snapshot`] which
/// derives procedures from the datamodel.
#[cfg(test)]
pub(crate) fn procedures_from_metadata(object_nodes: &[MetadataNode]) -> Vec<ProcedureInfo> {
    object_nodes
        .iter()
        .filter(|node| node.kind == ObjectKind::StoredProcedure)
        .map(|node| ProcedureInfo::new(node.schema.clone().unwrap_or_default(), &node.name))
        .collect()
}

/// Enrich every table/view object matching `(schema, name)` with the given
/// column names. Idempotent — safe to call again as more columns load. Returns
/// the number of objects updated.
pub fn merge_columns(
    catalog: &mut CompletionCatalog,
    schema: &str,
    name: &str,
    columns: Vec<String>,
) -> usize {
    let mut updated = 0;
    for object in &mut catalog.objects {
        if object.name.eq_ignore_ascii_case(name)
            && object
                .schema
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(schema))
        {
            object.columns = columns.clone();
            updated += 1;
        }
    }
    updated
}

/// Attach parsed parameter descriptors to the [`ProcedureInfo`] whose
/// `(schema, name)` matches. The caller is responsible for fetching parameter
/// metadata (name + data type + is_output) — this helper only does the
/// in-memory merge. Returns whether a matching procedure was found.
pub fn merge_procedure_parameters(
    procedures: &mut [ProcedureInfo],
    schema: &str,
    name: &str,
    parameters: Vec<ProcedureParameter>,
) -> bool {
    for proc in procedures.iter_mut() {
        if proc.name.eq_ignore_ascii_case(name) && proc.schema.eq_ignore_ascii_case(schema) {
            proc.parameters = parameters;
            return true;
        }
    }
    false
}

/// The completion inputs assembled from a live connection's metadata: the
/// [`CompletionCatalog`] the engine reads, the stored-procedure list used
/// for EXEC snippets, and the [`sqlly_lang::sqlmodel::ModelSnapshot`] that is the
/// single source of truth. The snapshot is the datamodel — future metadata
/// sources plug into it, and IntelliSense always reads from it.
#[derive(Debug, Clone, Default)]
pub struct CompletionState {
    pub catalog: CompletionCatalog,
    pub procedures: Vec<ProcedureInfo>,
    pub snapshot: sqlly_lang::sqlmodel::ModelSnapshot,
}

/// Load schemas + tables + views + programmable objects for `database` via the
/// [`MetadataService`] and assemble the [`CompletionState`]. This is the single
/// code path both the gpui app and the LSP server use to turn a connection into
/// completion inputs (DRY — the metadata→catalog sequence lives here, not in
/// each frontend).
///
/// The metadata is routed **through the datamodel** (`ModelSnapshot`) — the
/// single source of truth. The snapshot is built first, then converted to a
/// `CompletionCatalog` for the completion engine. Procedure descriptors are
/// derived from the snapshot. The snapshot is included in the `CompletionState`
/// so callers (GPUI app, LSP) can hold the datamodel and feed future sources
/// into it.
///
/// When `load_parameters` is true, each stored procedure's parameters are
/// eagerly fetched (the LSP has no lazy tree-expansion signal, so it loads them
/// up front). The gpui app passes `false` and fills parameters lazily as the
/// user expands a procedure node in the explorer (then calls
/// [`merge_procedure_parameters`]). Columns for every table/view are eagerly
/// loaded so alias-dot completion works immediately on connection.
///
/// `list_schemas` errors propagate; the per-kind object lists fall back to empty
/// on error so a partial catalog is still usable.
pub async fn build_completion_state(
    metadata: &dyn MetadataService,
    profile: &ConnectionProfile,
    database: &str,
    load_parameters: bool,
) -> StudioResult<CompletionState> {
    // Build the datamodel from metadata — the single source of truth.
    let snapshot =
        super::model_build::build_model_snapshot(metadata, profile, database, load_parameters)
            .await?;

    // Merge schemas from list_schemas — the snapshot only knows schemas that
    // contain objects; empty schemas still need to appear for schema-qualified
    // completion to work.
    let schemas = metadata.list_schemas(profile, database).await?;

    Ok(completion_state_from_snapshot(snapshot, database, &schemas))
}

/// Assemble a [`CompletionState`] from an already-built datamodel snapshot —
/// the metadata-free tail of [`build_completion_state`]. Used both by the live
/// build above and by the persistent data-model cache, which restores a
/// snapshot from disk and needs the same catalog/procedure derivation without
/// any server round-trip.
///
/// `extra_schemas` carries the catalog-level schema overlay (schemas with no
/// objects, which the snapshot cannot know about).
pub fn completion_state_from_snapshot(
    snapshot: sqlly_lang::sqlmodel::ModelSnapshot,
    database: &str,
    extra_schemas: &[String],
) -> CompletionState {
    // Convert the datamodel to the catalog the completion engine reads.
    let mut catalog = CompletionCatalog::from_model_snapshot(&snapshot);

    // --- Catalog-level overlays (not gaps in the datamodel) ---
    //
    // The `ModelSnapshot` models *objects* — tables, views, routines — which
    // live inside schemas. It does not model the catalog/container level
    // (database name) or empty schemas (schemas with no objects). These are
    // catalog metadata, not object metadata, so they are overlaid here after
    // the model-to-catalog conversion.
    //
    // The database name is needed for cross-database completion references.
    // Empty schemas are needed for schema-qualified completion (`dbo.` should
    // appear even if `dbo` has no loadable objects yet).
    catalog.databases.push(database.to_string());

    for schema in extra_schemas {
        if !catalog
            .schemas
            .iter()
            .any(|s| s.eq_ignore_ascii_case(schema))
        {
            catalog.schemas.push(schema.clone());
        }
    }

    // Derive procedure descriptors from the datamodel.
    let procedures = super::model_build::procedures_from_snapshot(&snapshot);

    CompletionState {
        catalog,
        procedures,
        snapshot,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, schema: Option<&str>, kind: ObjectKind) -> MetadataNode {
        let mut n = MetadataNode::new(name, kind);
        if let Some(s) = schema {
            n = n.with_schema(s);
        }
        n
    }

    #[test]
    fn catalog_from_metadata_maps_objects_and_skips_non_objects() {
        let nodes = vec![
            node("Orders", Some("dbo"), ObjectKind::Table),
            node("ActiveOrders", Some("dbo"), ObjectKind::View),
            node("GetUser", Some("dbo"), ObjectKind::StoredProcedure),
            node("OrderId", None, ObjectKind::Column), // skipped
            node("PK_Orders", None, ObjectKind::Index), // skipped
        ];
        let catalog = catalog_from_metadata(vec!["appdb".into()], vec!["dbo".into()], &nodes);
        assert_eq!(catalog.databases, vec!["appdb".to_string()]);
        assert_eq!(catalog.schemas, vec!["dbo".to_string()]);
        assert_eq!(catalog.objects.len(), 3);
        assert!(
            catalog
                .objects
                .iter()
                .any(|o| o.name == "Orders" && o.kind == ObjectKind::Table)
        );
        assert!(
            catalog
                .objects
                .iter()
                .any(|o| o.name == "GetUser" && o.kind == ObjectKind::StoredProcedure)
        );
        // Columns empty until merged.
        assert!(catalog.objects.iter().all(|o| o.columns.is_empty()));
    }

    #[test]
    fn merge_columns_enriches_matching_object() {
        let nodes = vec![node("Orders", Some("dbo"), ObjectKind::Table)];
        let mut catalog = catalog_from_metadata(vec![], vec!["dbo".into()], &nodes);
        let n = merge_columns(
            &mut catalog,
            "dbo",
            "Orders",
            vec!["OrderId".into(), "CustomerId".into()],
        );
        assert_eq!(n, 1);
        let orders = catalog.objects.iter().find(|o| o.name == "Orders").unwrap();
        assert_eq!(
            orders.columns,
            vec!["OrderId".to_string(), "CustomerId".to_string()]
        );
    }

    #[test]
    fn merge_columns_is_case_insensitive_and_idempotent() {
        let nodes = vec![node("Orders", Some("dbo"), ObjectKind::Table)];
        let mut catalog = catalog_from_metadata(vec![], vec![], &nodes);
        merge_columns(&mut catalog, "DBO", "orders", vec!["Id".into()]);
        assert_eq!(catalog.objects[0].columns, vec!["Id".to_string()]);
        merge_columns(&mut catalog, "dbo", "Orders", vec!["Id2".into()]);
        assert_eq!(catalog.objects[0].columns, vec!["Id2".to_string()]);
    }

    #[test]
    fn procedures_from_metadata_picks_only_stored_procedures() {
        let nodes = vec![
            node("GetUser", Some("dbo"), ObjectKind::StoredProcedure),
            node("udf_calc", Some("dbo"), ObjectKind::ScalarFunction),
            node("sp_other", None, ObjectKind::StoredProcedure),
        ];
        let procs = procedures_from_metadata(&nodes);
        assert_eq!(procs.len(), 2);
        assert!(
            procs
                .iter()
                .any(|p| p.schema == "dbo" && p.name == "GetUser")
        );
        assert!(procs.iter().any(|p| p.name == "sp_other"));
    }

    #[test]
    fn merge_procedure_parameters_attaches_to_matching_proc() {
        let nodes = vec![node("GetUser", Some("dbo"), ObjectKind::StoredProcedure)];
        let mut procs = procedures_from_metadata(&nodes);
        assert!(procs[0].parameters.is_empty());
        let ok = merge_procedure_parameters(
            &mut procs,
            "dbo",
            "GetUser",
            vec![ProcedureParameter::new("@id", "int")],
        );
        assert!(ok);
        assert_eq!(procs[0].parameters.len(), 1);
        assert_eq!(
            procs[0].declaration_snippet(),
            "EXEC [dbo].[GetUser] @id = 0"
        );
    }

    // --- build_completion_state: end-to-end metadata -> completion via a
    // fake MetadataService injected the same way the frontends inject it. ---

    use crate::metadata::MetadataService;
    use async_trait::async_trait;
    use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef, StudioResult};

    /// Canned metadata service: returns fixed schemas/tables/views/procs and
    /// records whether `list_procedure_parameters` was called (to prove the
    /// lazy-vs-eager `load_parameters` switch).
    #[derive(Default)]
    struct FakeMetadata {
        param_calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl MetadataService for FakeMetadata {
        async fn list_schemas(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
        ) -> StudioResult<Vec<String>> {
            Ok(vec!["dbo".into(), "sales".into()])
        }
        async fn list_tables(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: Option<&str>,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(vec![node("Orders", Some("dbo"), ObjectKind::Table)])
        }
        async fn list_views(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: Option<&str>,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(vec![node("ActiveOrders", Some("dbo"), ObjectKind::View)])
        }
        async fn list_programmable_objects(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: Option<&str>,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(vec![
                node("GetUser", Some("dbo"), ObjectKind::StoredProcedure),
                node("udf_calc", Some("dbo"), ObjectKind::ScalarFunction),
            ])
        }
        async fn list_columns(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: &str,
            _o: &str,
        ) -> StudioResult<Vec<MetadataNode>> {
            Ok(vec![])
        }
        async fn list_procedure_parameters(
            &self,
            _p: &ConnectionProfile,
            _db: &str,
            _s: &str,
            _o: &str,
        ) -> StudioResult<Vec<ProcedureParameter>> {
            self.param_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![ProcedureParameter::new("@id", "int")])
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
    async fn build_completion_state_lazy_skips_parameter_load() {
        let meta = FakeMetadata::default();
        let profile = test_profile();
        let state = build_completion_state(&meta, &profile, "appdb", false)
            .await
            .expect("build state");
        // Catalog carries the db, both schemas, and the table/view/proc/function
        // objects (5 schemas surfaced; columns empty).
        assert_eq!(state.catalog.databases, vec!["appdb".to_string()]);
        assert_eq!(
            state.catalog.schemas,
            vec!["dbo".to_string(), "sales".into()]
        );
        assert_eq!(state.catalog.objects.len(), 4);
        // Only the stored procedure becomes a ProcedureInfo.
        assert_eq!(state.procedures.len(), 1);
        assert_eq!(state.procedures[0].name, "GetUser");
        // Lazy mode: parameters NOT fetched.
        assert!(state.procedures[0].parameters.is_empty());
        assert_eq!(
            meta.param_calls.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn build_completion_state_eager_loads_parameters() {
        let meta = FakeMetadata::default();
        let profile = test_profile();
        let state = build_completion_state(&meta, &profile, "appdb", true)
            .await
            .expect("build state");
        assert_eq!(state.procedures.len(), 1);
        // Eager mode (LSP): parameters fetched and attached.
        assert_eq!(state.procedures[0].parameters.len(), 1);
        // The model loads parameters for all programmable objects (procedures
        // AND functions), so param_calls is 2: one for GetUser, one for udf_calc.
        assert_eq!(
            meta.param_calls.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        assert_eq!(
            state.procedures[0].declaration_snippet(),
            "EXEC [dbo].[GetUser] @id = 0"
        );
    }

    #[tokio::test]
    async fn build_completion_state_feeds_enriched_completion() {
        // The end-to-end contract the frontends rely on: a connection's
        // metadata, run through build_completion_state, produces a catalog that
        // makes the completion engine schema-aware.
        let meta = FakeMetadata::default();
        let profile = test_profile();
        let state = build_completion_state(&meta, &profile, "appdb", true)
            .await
            .expect("build state");
        let svc = crate::intellisense::DefaultIntellisense;
        let sql = "SELECT * FROM ";
        let items = <crate::intellisense::DefaultIntellisense as crate::intellisense::IntellisenseService>::complete_enriched(
            &svc,
            sql,
            sql.len(),
            &state.catalog,
            &state.procedures,
            &crate::intellisense::IntelliSenseRuleConfig::default(),
        );
        // The connected table surfaces as a completion (schema-qualified).
        assert!(
            items
                .iter()
                .any(|i| i.label == "dbo.Orders" || i.label == "Orders"),
            "expected table 'Orders' from live catalog, got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }
}
