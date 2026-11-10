use sqlly_core::ObjectKind;
use sqlly_lang::fsmodel::{
    FileSourceRef, FileSourcedSqlNode, FunctionSqlNode, NamedSqlNode, OperationSqlNode,
    ProcedureSqlNode, SchemaSqlNode, SqlColumnNode, SqlDataType, SqlNodeKind, SqlParameterNode,
    TableSqlNode, ViewSqlNode,
};
#[cfg(not(target_arch = "wasm32"))]
use std::time::SystemTime;
#[cfg(target_arch = "wasm32")]
use web_time::SystemTime;

pub use crate::intellisense::types::{CompletionCatalog, CompletionObject, FkRelation};

/// Build filesystem schema nodes from the model's `Provenance::File` entries
/// so file-defined objects surface with filesystem-specific completion
/// behavior (the `CompletionSource::Filesystem` badge and file-aware paths)
/// instead of appearing as generic live-database objects. Runs once per
/// catalog build, never per keystroke.
fn filesystem_schemas_from_snapshot(
    snapshot: &sqlly_lang::sqlmodel::ModelSnapshot,
) -> Vec<SchemaSqlNode> {
    use sqlly_lang::sqlmodel::{ObjectBody as B, ObjectKind as M, Provenance};

    let column_node = |schema: &str, c: &sqlly_lang::sqlmodel::ColumnDef| SqlColumnNode {
        node: NamedSqlNode::new(
            SqlNodeKind::Column,
            Some(schema.to_string()),
            None,
            c.name.spelling(),
        )
        .with_data_type(
            c.ty.as_ref()
                .map(|t| SqlDataType::built_in(t.native.clone()))
                .unwrap_or_default(),
        ),
        nullable: c.nullable,
        is_identity: c.identity,
        default: c.default.clone(),
        foreign_key: None,
        check_constraints: Vec::new(),
    };
    let param_node = |schema: &str, p: &sqlly_lang::sqlmodel::ParamDef| SqlParameterNode {
        node: NamedSqlNode::new(
            SqlNodeKind::Parameter,
            Some(schema.to_string()),
            None,
            p.name.spelling(),
        )
        .with_data_type(
            p.ty.as_ref()
                .map(|t| SqlDataType::built_in(t.native.clone()))
                .unwrap_or_default(),
        ),
        default: p.default.clone(),
        is_output: matches!(
            p.mode,
            sqlly_lang::sqlmodel::ParamMode::Out | sqlly_lang::sqlmodel::ParamMode::InOut
        ),
        is_readonly: false,
    };

    // Deterministic order regardless of snapshot map iteration.
    let mut entries: Vec<_> = snapshot
        .objects()
        .filter(|e| matches!(e.provenance, Provenance::File { .. }))
        .collect();
    entries.sort_by_key(|e| e.id);

    let mut schemas: Vec<SchemaSqlNode> = Vec::new();
    for entry in entries {
        let Provenance::File {
            path, content_hash, ..
        } = &entry.provenance
        else {
            continue;
        };
        let schema_name = entry
            .reference
            .namespace
            .as_ref()
            .map(|n| n.spelling().to_string())
            .unwrap_or_else(|| "dbo".to_string());
        let idx = match schemas
            .iter()
            .position(|s| s.node.name.eq_ignore_ascii_case(&schema_name))
        {
            Some(i) => i,
            None => {
                schemas.push(SchemaSqlNode {
                    node: NamedSqlNode::new(SqlNodeKind::Schema, None, None, schema_name.clone()),
                    tables: Vec::new(),
                    views: Vec::new(),
                    functions: Vec::new(),
                    types: Vec::new(),
                    procedures: Vec::new(),
                });
                schemas.len() - 1
            }
        };
        let source = FileSourceRef {
            source_file_name: std::path::Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.clone()),
            source_file_path: path.clone(),
            file_hash: format!("{content_hash:x}"),
        };
        let name = entry.reference.name.spelling();
        let node_kind = match entry.kind {
            M::Table => SqlNodeKind::Table,
            M::View | M::MaterializedView => SqlNodeKind::View,
            M::Procedure => SqlNodeKind::Procedure,
            M::Function | M::ScalarFunction | M::TableFunction | M::AggregateFunction => {
                SqlNodeKind::Function
            }
            _ => continue,
        };
        let source_node = FileSourcedSqlNode {
            node: NamedSqlNode::new(node_kind, Some(schema_name.clone()), None, name),
            source,
        };
        match (&entry.kind, &entry.body) {
            (M::Table, B::Table(body)) => schemas[idx].tables.push(TableSqlNode {
                source_node,
                columns: body
                    .columns
                    .iter()
                    .map(|c| column_node(&schema_name, c))
                    .collect(),
            }),
            (M::View | M::MaterializedView, B::View(body)) => {
                schemas[idx].views.push(ViewSqlNode {
                    source_node,
                    columns: body
                        .columns
                        .iter()
                        .map(|c| column_node(&schema_name, c))
                        .collect(),
                    select_span: None,
                })
            }
            (M::Procedure, B::Routine(body)) => schemas[idx].procedures.push(ProcedureSqlNode {
                operation: OperationSqlNode {
                    source_node,
                    input_parameters: body
                        .params
                        .iter()
                        .map(|p| param_node(&schema_name, p))
                        .collect(),
                    result_sets: Vec::new(),
                },
            }),
            (
                M::Function | M::ScalarFunction | M::TableFunction | M::AggregateFunction,
                B::Routine(body),
            ) => schemas[idx].functions.push(FunctionSqlNode {
                operation: OperationSqlNode {
                    source_node,
                    input_parameters: body
                        .params
                        .iter()
                        .map(|p| param_node(&schema_name, p))
                        .collect(),
                    result_sets: Vec::new(),
                },
                output: body
                    .returns
                    .as_ref()
                    .map(|t| SqlDataType::built_in(t.native.clone()))
                    .unwrap_or_default(),
            }),
            _ => {}
        }
    }
    schemas
}

/// Map `sqlly_lang::sqlmodel::ObjectKind` to the `sqlly_core::ObjectKind` used by
/// `CompletionObject`. The sqlmodel enum is a parser-level kind set; the core
/// enum is the metadata-level kind set used throughout the completion engine.
pub fn map_object_kind(kind: sqlly_lang::sqlmodel::ObjectKind) -> ObjectKind {
    use sqlly_lang::sqlmodel::ObjectKind as M;
    match kind {
        M::Table => ObjectKind::Table,
        M::View => ObjectKind::View,
        M::MaterializedView => ObjectKind::View,
        M::Procedure => ObjectKind::StoredProcedure,
        // A generic `Function` (file-parsed, subtype unknown) defaults to
        // scalar, matching the legacy `catalog_from_metadata` behavior;
        // metadata-sourced functions carry their real subtype through.
        M::Function | M::ScalarFunction => ObjectKind::ScalarFunction,
        M::TableFunction => ObjectKind::TableFunction,
        M::AggregateFunction => ObjectKind::AggregateFunction,
        M::Trigger => ObjectKind::Trigger,
        M::Sequence => ObjectKind::Sequence,
        M::Synonym => ObjectKind::Synonym,
        M::Index => ObjectKind::Index,
        M::Constraint => ObjectKind::Constraint,
        M::Default => ObjectKind::Default,
        M::Rule => ObjectKind::Rule,
        M::DomainType | M::CompositeType | M::EnumType | M::TableType => ObjectKind::Type,
        M::Package | M::PackageBody => ObjectKind::StoredProcedure,
        M::Collation | M::Extension | M::Event => ObjectKind::Type,
    }
}

/// Extract column names from a table or view body.
fn body_columns(body: &sqlly_lang::sqlmodel::ObjectBody) -> Vec<String> {
    match body {
        sqlly_lang::sqlmodel::ObjectBody::Table(t) => t
            .columns
            .iter()
            .map(|c| c.name.spelling().to_string())
            .collect(),
        sqlly_lang::sqlmodel::ObjectBody::View(v) => v
            .columns
            .iter()
            .map(|c| c.name.spelling().to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// Extract parameter names from a routine body.
fn body_parameters(body: &sqlly_lang::sqlmodel::ObjectBody) -> Vec<String> {
    match body {
        sqlly_lang::sqlmodel::ObjectBody::Routine(r) => r
            .params
            .iter()
            .map(|p| p.name.spelling().to_string())
            .collect(),
        _ => Vec::new(),
    }
}

/// Extract FK relations from a table body.
fn body_foreign_keys(body: &sqlly_lang::sqlmodel::ObjectBody) -> Vec<FkRelation> {
    match body {
        sqlly_lang::sqlmodel::ObjectBody::Table(t) => t
            .foreign_keys
            .iter()
            .enumerate()
            .flat_map(|(index, fk)| {
                // Every pair of one (possibly composite) key shares this, so
                // join builders can rejoin the pairs with AND.
                let constraint = fk
                    .name
                    .as_ref()
                    .map(|n| n.spelling().to_string())
                    .unwrap_or_else(|| format!("#fk{index}"));
                let target_schema = fk
                    .target
                    .namespace
                    .as_ref()
                    .map(|n| n.spelling().to_string())
                    .unwrap_or_default();
                let target_table = fk.target.name.spelling().to_string();
                fk.columns
                    .iter()
                    .zip(
                        fk.target_columns
                            .iter()
                            .chain(std::iter::once(&fk.target.name)),
                    )
                    .map(|(col, target_col)| FkRelation {
                        column: col.spelling().to_string(),
                        target_schema: target_schema.clone(),
                        target_table: target_table.clone(),
                        target_column: target_col.spelling().to_string(),
                        constraint: Some(constraint.clone()),
                    })
                    .collect::<Vec<_>>()
            })
            .collect(),
        _ => Vec::new(),
    }
}

impl CompletionCatalog {
    /// Build a `CompletionCatalog` from a `sqlly_lang::sqlmodel::ModelSnapshot`.
    ///
    /// This is the key bridge: the datamodel (`ModelSnapshot`) is the single
    /// source of truth, and this method converts it into the catalog shape the
    /// completion engine consumes. Every object in the snapshot becomes a
    /// `CompletionObject` with its columns, parameters, and foreign keys
    /// extracted from the typed `ObjectBody`.
    pub fn from_model_snapshot(snapshot: &sqlly_lang::sqlmodel::ModelSnapshot) -> Self {
        let mut databases: Vec<String> = Vec::new();
        let mut schemas: Vec<String> = Vec::new();
        let mut objects: Vec<CompletionObject> = Vec::new();

        for entry in snapshot.objects() {
            let database = entry
                .reference
                .catalog
                .as_ref()
                .map(|c| c.spelling().to_string());
            let schema = entry
                .reference
                .namespace
                .as_ref()
                .map(|n| n.spelling().to_string());
            let name = entry.reference.name.spelling().to_string();

            if let Some(db) = &database
                && !databases.iter().any(|d| d.eq_ignore_ascii_case(db))
            {
                databases.push(db.clone());
            }
            if let Some(sc) = &schema
                && !schemas.iter().any(|s| s.eq_ignore_ascii_case(sc))
            {
                schemas.push(sc.clone());
            }

            // File-sourced entries surface through `filesystem_schemas`
            // below (mirroring the legacy live/filesystem split) so they get
            // filesystem-specific behavior without duplicating as generic DB
            // objects.
            if matches!(
                entry.provenance,
                sqlly_lang::sqlmodel::Provenance::File { .. }
            ) {
                continue;
            }

            objects.push(CompletionObject {
                database,
                schema,
                name,
                kind: map_object_kind(entry.kind),
                columns: body_columns(&entry.body),
                parameters: body_parameters(&entry.body),
                foreign_keys: body_foreign_keys(&entry.body),
            });
        }

        Self {
            databases,
            schemas,
            objects,
            filesystem_schemas: filesystem_schemas_from_snapshot(snapshot),
            snippets: crate::intellisense::complete::default_snippets(),
        }
    }

    pub fn from_snapshot(snapshot: &SymbolSnapshot) -> Self {
        let mut schemas = snapshot.live.schemas.clone();
        for schema in &snapshot.filesystem_schemas {
            if !schemas
                .iter()
                .any(|item| item.eq_ignore_ascii_case(&schema.node.name))
            {
                schemas.push(schema.node.name.clone());
            }
        }
        Self {
            databases: snapshot.live.databases.clone(),
            schemas,
            objects: snapshot.live.objects.clone(),
            filesystem_schemas: snapshot.filesystem_schemas.clone(),
            snippets: crate::intellisense::complete::default_snippets(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LiveMetadataSnapshot {
    pub captured_at: Option<SystemTime>,
    pub active_database: Option<String>,
    pub databases: Vec<String>,
    pub schemas: Vec<String>,
    pub objects: Vec<CompletionObject>,
}

#[derive(Debug, Clone, Default)]
pub struct SymbolSnapshot {
    pub live: LiveMetadataSnapshot,
    pub filesystem_schemas: Vec<SchemaSqlNode>,
}

impl SymbolSnapshot {
    pub fn catalog(&self) -> CompletionCatalog {
        CompletionCatalog::from_snapshot(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intellisense::complete::complete;
    use crate::intellisense::types::{CompletionKind, CompletionSource};
    use sqlly_core::ObjectKind;

    // ---- ModelSnapshot → CompletionCatalog bridge tests ----

    /// Helper: create an `ObjectRef` from optional catalog/namespace and a name.
    fn mref(
        catalog: Option<&str>,
        namespace: Option<&str>,
        name: &str,
    ) -> sqlly_lang::sqlmodel::ObjectRef {
        let dialect = sqlly_lang::sqlmodel::Dialect::SqlServer;
        sqlly_lang::sqlmodel::ObjectRef {
            catalog: catalog.map(|c| sqlly_lang::sqlmodel::SqlName::new(c, dialect, false)),
            namespace: namespace.map(|n| sqlly_lang::sqlmodel::SqlName::new(n, dialect, false)),
            name: sqlly_lang::sqlmodel::SqlName::new(name, dialect, false),
        }
    }

    /// Helper: create a table fragment.
    fn table_fragment(
        namespace: &str,
        name: &str,
        columns: Vec<&str>,
        fks: Vec<(&str, &str, &str, &str)>,
    ) -> sqlly_lang::sqlmodel::ObjectFragment {
        let dialect = sqlly_lang::sqlmodel::Dialect::SqlServer;
        sqlly_lang::sqlmodel::ObjectFragment {
            reference: mref(None, Some(namespace), name),
            kind: sqlly_lang::sqlmodel::ObjectKind::Table,
            dialect,
            body: sqlly_lang::sqlmodel::ObjectBody::Table(sqlly_lang::sqlmodel::TableBody {
                columns: columns
                    .iter()
                    .enumerate()
                    .map(|(i, &c)| sqlly_lang::sqlmodel::ColumnDef {
                        name: sqlly_lang::sqlmodel::SqlName::new(c, dialect, false),
                        ty: None,
                        nullable: None,
                        default: None,
                        identity: false,
                        generated: None,
                        collation: None,
                        ordinal: i,
                    })
                    .collect(),
                primary_key: None,
                unique_keys: Vec::new(),
                foreign_keys: fks
                    .iter()
                    .map(|&(col, target_schema, target_table, target_col)| {
                        sqlly_lang::sqlmodel::ForeignKeyDef {
                            name: None,
                            columns: vec![sqlly_lang::sqlmodel::SqlName::new(col, dialect, false)],
                            target: mref(None, Some(target_schema), target_table),
                            target_columns: vec![sqlly_lang::sqlmodel::SqlName::new(
                                target_col, dialect, false,
                            )],
                        }
                    })
                    .collect(),
                checks: Vec::new(),
                options: std::collections::BTreeMap::new(),
            }),
            provenance: sqlly_lang::sqlmodel::Provenance::Database {
                profile: "test".to_string(),
            },
            fidelity: sqlly_lang::sqlmodel::Fidelity::Exact,
            references: Vec::new(),
        }
    }

    /// Helper: create a view fragment.
    fn view_fragment(
        namespace: &str,
        name: &str,
        columns: Vec<&str>,
    ) -> sqlly_lang::sqlmodel::ObjectFragment {
        let dialect = sqlly_lang::sqlmodel::Dialect::SqlServer;
        sqlly_lang::sqlmodel::ObjectFragment {
            reference: mref(None, Some(namespace), name),
            kind: sqlly_lang::sqlmodel::ObjectKind::View,
            dialect,
            body: sqlly_lang::sqlmodel::ObjectBody::View(sqlly_lang::sqlmodel::ViewBody {
                columns: columns
                    .iter()
                    .enumerate()
                    .map(|(i, &c)| sqlly_lang::sqlmodel::ColumnDef {
                        name: sqlly_lang::sqlmodel::SqlName::new(c, dialect, false),
                        ty: None,
                        nullable: None,
                        default: None,
                        identity: false,
                        generated: None,
                        collation: None,
                        ordinal: i,
                    })
                    .collect(),
                definition: None,
            }),
            provenance: sqlly_lang::sqlmodel::Provenance::Database {
                profile: "test".to_string(),
            },
            fidelity: sqlly_lang::sqlmodel::Fidelity::Exact,
            references: Vec::new(),
        }
    }

    /// Helper: create a procedure fragment.
    fn proc_fragment(
        namespace: &str,
        name: &str,
        params: Vec<&str>,
    ) -> sqlly_lang::sqlmodel::ObjectFragment {
        let dialect = sqlly_lang::sqlmodel::Dialect::SqlServer;
        sqlly_lang::sqlmodel::ObjectFragment {
            reference: mref(None, Some(namespace), name),
            kind: sqlly_lang::sqlmodel::ObjectKind::Procedure,
            dialect,
            body: sqlly_lang::sqlmodel::ObjectBody::Routine(sqlly_lang::sqlmodel::RoutineBody {
                params: params
                    .iter()
                    .map(|&p| sqlly_lang::sqlmodel::ParamDef {
                        name: sqlly_lang::sqlmodel::SqlName::new(p, dialect, false),
                        ty: None,
                        mode: sqlly_lang::sqlmodel::ParamMode::In,
                        default: None,
                    })
                    .collect(),
                returns: None,
                language: None,
                body_text: None,
            }),
            provenance: sqlly_lang::sqlmodel::Provenance::Database {
                profile: "test".to_string(),
            },
            fidelity: sqlly_lang::sqlmodel::Fidelity::Exact,
            references: Vec::new(),
        }
    }

    /// Helper: build a `ModelSnapshot` from fragments.
    fn snapshot_from(
        fragments: Vec<sqlly_lang::sqlmodel::ObjectFragment>,
    ) -> sqlly_lang::sqlmodel::ModelSnapshot {
        let empty = sqlly_lang::sqlmodel::ModelSnapshot::empty();
        let (snap, _) = empty.apply(
            1,
            fragments,
            &std::collections::BTreeSet::new(),
            sqlly_lang::sqlmodel::IncompleteRegions::default(),
        );
        snap
    }

    #[test]
    fn from_model_snapshot_converts_tables_with_columns() {
        let snap = snapshot_from(vec![table_fragment(
            "dbo",
            "Customers",
            vec!["CustomerId", "CustomerName", "CreatedAt"],
            Vec::new(),
        )]);
        let catalog = CompletionCatalog::from_model_snapshot(&snap);

        assert!(catalog.schemas.contains(&"dbo".to_string()));
        let obj = catalog
            .objects
            .iter()
            .find(|o| o.name == "Customers")
            .expect("Customers table should be in catalog");
        assert_eq!(obj.kind, ObjectKind::Table);
        assert_eq!(obj.schema.as_deref(), Some("dbo"));
        assert_eq!(obj.columns, vec!["CustomerId", "CustomerName", "CreatedAt"]);
    }

    #[test]
    fn from_model_snapshot_converts_views_with_columns() {
        let snap = snapshot_from(vec![view_fragment(
            "sales",
            "ActiveCustomers",
            vec!["CustomerId", "CustomerName"],
        )]);
        let catalog = CompletionCatalog::from_model_snapshot(&snap);

        let obj = catalog
            .objects
            .iter()
            .find(|o| o.name == "ActiveCustomers")
            .expect("ActiveCustomers view should be in catalog");
        assert_eq!(obj.kind, ObjectKind::View);
        assert_eq!(obj.columns, vec!["CustomerId", "CustomerName"]);
    }

    #[test]
    fn from_model_snapshot_converts_procedures_with_parameters() {
        let snap = snapshot_from(vec![proc_fragment(
            "dbo",
            "SearchOrders",
            vec!["@CustomerId", "@StartDate"],
        )]);
        let catalog = CompletionCatalog::from_model_snapshot(&snap);

        let obj = catalog
            .objects
            .iter()
            .find(|o| o.name == "SearchOrders")
            .expect("SearchOrders procedure should be in catalog");
        assert_eq!(obj.kind, ObjectKind::StoredProcedure);
        assert_eq!(obj.parameters, vec!["@CustomerId", "@StartDate"]);
    }

    #[test]
    fn from_model_snapshot_populates_filesystem_schemas_from_file_provenance() {
        let mut file_table = table_fragment("dbo", "FileOrders", vec!["Id", "Total"], Vec::new());
        file_table.provenance = sqlly_lang::sqlmodel::Provenance::File {
            path: "/proj/sql/FileOrders.sql".into(),
            content_hash: 42,
            span: sqlly_lang::sqlmodel::Span::new(0, 10),
        };
        let live_table = table_fragment("dbo", "LiveOrders", vec!["Id"], Vec::new());
        let snap = snapshot_from(vec![file_table, live_table]);
        let catalog = CompletionCatalog::from_model_snapshot(&snap);

        assert_eq!(catalog.filesystem_schemas.len(), 1, "one fs schema (dbo)");
        let fs = &catalog.filesystem_schemas[0];
        assert!(fs.node.name.eq_ignore_ascii_case("dbo"));
        assert_eq!(fs.tables.len(), 1, "only the file-sourced table");
        let t = &fs.tables[0];
        assert_eq!(t.source_node.node.name, "FileOrders");
        assert_eq!(t.source_node.source.source_file_name, "FileOrders.sql");
        assert_eq!(
            t.source_node.source.source_file_path,
            "/proj/sql/FileOrders.sql"
        );
        assert_eq!(t.columns.len(), 2);

        // The file-sourced object completes with the Filesystem source badge.
        let sql = "SELECT * FROM dbo.File";
        let items = complete(sql, sql.len(), &catalog);
        let hit = items
            .iter()
            .find(|i| i.label.contains("FileOrders"))
            .expect("file table completes");
        assert_eq!(hit.source, CompletionSource::Filesystem);
    }

    #[test]
    fn from_model_snapshot_converts_foreign_keys() {
        let snap = snapshot_from(vec![
            table_fragment(
                "dbo",
                "Orders",
                vec!["OrderId", "CustomerId"],
                vec![("CustomerId", "dbo", "Customers", "CustomerId")],
            ),
            table_fragment(
                "dbo",
                "Customers",
                vec!["CustomerId", "CustomerName"],
                Vec::new(),
            ),
        ]);
        let catalog = CompletionCatalog::from_model_snapshot(&snap);

        let orders = catalog
            .objects
            .iter()
            .find(|o| o.name == "Orders")
            .expect("Orders table should be in catalog");
        assert_eq!(orders.foreign_keys.len(), 1);
        let fk = &orders.foreign_keys[0];
        assert_eq!(fk.column, "CustomerId");
        assert_eq!(fk.target_schema, "dbo");
        assert_eq!(fk.target_table, "Customers");
        assert_eq!(fk.target_column, "CustomerId");
    }

    #[test]
    fn from_model_snapshot_keeps_composite_foreign_key_pairs_together() {
        let name = |t: &str| {
            sqlly_lang::sqlmodel::SqlName::new(t, sqlly_lang::sqlmodel::Dialect::SqlServer, false)
        };
        let mut lines = table_fragment(
            "dbo",
            "OrderLines",
            vec!["TenantId", "OrderNo", "ProductId"],
            vec![("ProductId", "dbo", "Products", "Id")],
        );
        if let sqlly_lang::sqlmodel::ObjectBody::Table(t) = &mut lines.body {
            // Unnamed composite key, then the single-column one again under a
            // name: three pairs, two keys.
            let mut product_fk = t.foreign_keys.remove(0);
            product_fk.name = Some(name("FK_Lines_Products"));
            t.foreign_keys = vec![
                sqlly_lang::sqlmodel::ForeignKeyDef {
                    name: None,
                    columns: vec![name("TenantId"), name("OrderNo")],
                    target: product_fk.target.clone(),
                    target_columns: vec![name("TenantId"), name("OrderNo")],
                },
                product_fk,
            ];
            t.foreign_keys[0].target.name = name("Orders");
        }
        let catalog = CompletionCatalog::from_model_snapshot(&snapshot_from(vec![lines]));
        let lines = catalog
            .objects
            .iter()
            .find(|o| o.name == "OrderLines")
            .expect("OrderLines in catalog");
        assert_eq!(lines.foreign_keys.len(), 3);
        let keys = FkRelation::group(&lines.foreign_keys);
        assert_eq!(keys.len(), 2, "{keys:#?}");
        let composite: Vec<_> = keys[0]
            .iter()
            .map(|fk| (fk.column.as_str(), fk.target_column.as_str()))
            .collect();
        assert_eq!(
            composite,
            vec![("TenantId", "TenantId"), ("OrderNo", "OrderNo")]
        );
        assert_eq!(keys[0][0].target_table, "Orders");
        assert_eq!(keys[1].len(), 1);
        assert_eq!(keys[1][0].constraint.as_deref(), Some("FK_Lines_Products"));
    }

    #[test]
    fn fk_group_treats_unconstrained_relations_as_single_keys() {
        let rel = |column: &str, constraint: Option<&str>| FkRelation {
            column: column.to_string(),
            target_schema: "dbo".to_string(),
            target_table: "Addresses".to_string(),
            target_column: "Id".to_string(),
            constraint: constraint.map(str::to_string),
        };
        let relations = vec![rel("BillingId", None), rel("ShippingId", None)];
        assert_eq!(FkRelation::group(&relations).len(), 2);
        let relations = vec![
            rel("A", Some("FK_1")),
            rel("B", Some("FK_2")),
            rel("C", Some("FK_1")),
        ];
        let keys = FkRelation::group(&relations);
        assert_eq!(keys.len(), 2);
        assert_eq!(
            keys[0]
                .iter()
                .map(|r| r.column.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "C"]
        );
    }

    #[test]
    fn from_model_snapshot_collects_unique_schemas() {
        let snap = snapshot_from(vec![
            table_fragment("dbo", "TableA", vec!["Id"], Vec::new()),
            table_fragment("dbo", "TableB", vec!["Id"], Vec::new()),
            table_fragment("sales", "TableC", vec!["Id"], Vec::new()),
        ]);
        let catalog = CompletionCatalog::from_model_snapshot(&snap);

        assert!(catalog.schemas.contains(&"dbo".to_string()));
        assert!(catalog.schemas.contains(&"sales".to_string()));
        // No duplicates
        let dbo_count = catalog
            .schemas
            .iter()
            .filter(|s| s.eq_ignore_ascii_case("dbo"))
            .count();
        assert_eq!(dbo_count, 1);
    }

    #[test]
    fn from_model_snapshot_maps_function_kind() {
        let dialect = sqlly_lang::sqlmodel::Dialect::SqlServer;
        let snap = snapshot_from(vec![sqlly_lang::sqlmodel::ObjectFragment {
            reference: mref(None, Some("dbo"), "CalculateTotal"),
            kind: sqlly_lang::sqlmodel::ObjectKind::Function,
            dialect,
            body: sqlly_lang::sqlmodel::ObjectBody::Routine(sqlly_lang::sqlmodel::RoutineBody {
                params: vec![],
                returns: None,
                language: None,
                body_text: None,
            }),
            provenance: sqlly_lang::sqlmodel::Provenance::Database {
                profile: "test".to_string(),
            },
            fidelity: sqlly_lang::sqlmodel::Fidelity::Exact,
            references: Vec::new(),
        }]);
        let catalog = CompletionCatalog::from_model_snapshot(&snap);

        let obj = catalog
            .objects
            .iter()
            .find(|o| o.name == "CalculateTotal")
            .expect("Function should be in catalog");
        assert_eq!(obj.kind, ObjectKind::ScalarFunction);
    }

    #[test]
    fn from_model_snapshot_empty_snapshot_produces_empty_catalog() {
        let snap = sqlly_lang::sqlmodel::ModelSnapshot::empty();
        let catalog = CompletionCatalog::from_model_snapshot(&snap);

        assert!(catalog.databases.is_empty());
        assert!(catalog.schemas.is_empty());
        assert!(catalog.objects.is_empty());
        // Snippets are always populated by default
        assert!(!catalog.snippets.is_empty());
    }

    #[test]
    fn from_model_snapshot_completions_work_after_from() {
        let snap = snapshot_from(vec![table_fragment(
            "dbo",
            "Orders",
            vec!["OrderId", "CustomerId", "CreatedAt"],
            Vec::new(),
        )]);
        let catalog = CompletionCatalog::from_model_snapshot(&snap);

        let items = complete("SELECT * FROM ord", 17, &catalog);
        assert!(
            items.iter().any(|item| item.label == "dbo.Orders"),
            "completion from model snapshot should find Orders table, got {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn from_model_snapshot_completions_work_for_columns() {
        let snap = snapshot_from(vec![table_fragment(
            "dbo",
            "Orders",
            vec!["OrderId", "CustomerId", "CreatedAt"],
            Vec::new(),
        )]);
        let catalog = CompletionCatalog::from_model_snapshot(&snap);

        let sql = "SELECT * FROM dbo.Orders o WHERE o.";
        let items = complete(sql, sql.len(), &catalog);
        assert!(
            items.iter().any(|item| item.kind == CompletionKind::Column),
            "completion from model snapshot should offer columns after alias dot, got {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }
}
