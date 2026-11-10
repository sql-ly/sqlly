#![forbid(unsafe_code)]
pub mod cache;
pub mod catalog;
pub mod database_ddl;
pub mod ddl;
pub mod dialect;
pub mod dialect_ddl;
pub mod error;
pub mod explorer;
pub mod node;

pub use dialect::{MetadataDialect, MetadataUnsupported, sql_for_dialect, sql_for_engine};

use serde::{Deserialize, Serialize};
use sqlly_core::{ObjectKind, ObjectNode, ScriptAction, ServerCapability, ServerCapabilityProfile};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataQuery {
    ServerCapability,
    ServerPermissions,
    /// Server-wide configuration settings, one row per setting. Row: name,
    /// value, source (where the engine records one — `sys.configurations`'
    /// description, PostgreSQL's `pg_settings.source`, …), is_dynamic (0/1 —
    /// whether the engine lets this value change at runtime from SQL, which
    /// gates the editor's Edit facet).
    ServerConfiguration,
    /// Server status counters, one row per counter. Row: name, value, source
    /// (the counter class or section the engine groups it under —
    /// `pg_stat_database`, a performance-counter object class, …). Engines
    /// without server-wide counters return an empty set of this shape.
    ServerStatus,
    Databases,
    Schemas,
    /// Every schema plus how many user-visible objects (tables, views,
    /// procedures, functions) it contains. Drives the server explorer's
    /// empty-schema grouping preferences.
    SchemaObjectCounts,
    Tables {
        schema: Option<String>,
    },
    Views {
        schema: Option<String>,
    },
    ProgrammableObjects {
        schema: Option<String>,
    },
    /// Flat inventory of user objects for the Object Browser: one row per table, view, procedure, function, trigger — kind, schema_name, name, row_estimate, size_kb, create_date, modify_date, comment. Estimates are catalog statistics, never COUNT(*).
    ObjectInventory {
        schema: Option<String>,
    },
    Columns {
        schema: String,
        object: String,
    },
    /// Columns of **every** table and view in the database in one round trip
    /// (schema_name, object_name, column_name; ordinal order within each
    /// object). Used to build the completion snapshot without issuing one
    /// `Columns` query per object.
    AllColumns,
    /// Every foreign-key column mapping in the database in one round trip:
    /// child (referencing) schema/table/column and parent (referenced)
    /// schema/table/column. Drives the FK-based JOIN suggestions in
    /// IntelliSense — the same query the Swift `SchemaProvider` runs.
    AllForeignKeys,
    /// Every primary-key column in the database in one round trip: schema,
    /// table, column, and the column's ordinal within the key. Used to fill the
    /// data model's `primary_key` so the editor can tell a single-row
    /// UPDATE/DELETE from an unconstrained one.
    AllPrimaryKeys,
    /// Parameters of a stored procedure or function (`sys.parameters`).
    ProcedureParameters {
        schema: String,
        object: String,
    },
    Indexes {
        schema: String,
        table: String,
    },
    Triggers {
        schema: String,
        table: String,
    },
    Statistics {
        schema: String,
        table: String,
    },
    Constraints {
        schema: String,
        table: String,
    },
    /// User-defined types (`sys.types` where `is_user_defined = 1`), with the
    /// base system type and nullability. Powers the Programmability → Types
    /// explorer folder and the type editor dialog.
    Types {
        schema: Option<String>,
    },
    /// Database-level (DDL) triggers, or all triggers in a schema scope for
    /// engines without database triggers. Row: schema_name, name, parent_name (nullable), is_disabled.
    DatabaseTriggers {
        schema: Option<String>,
    },
    /// Row: schema_name, name, data_type, start_value, increment, current_value (nullable).
    Sequences {
        schema: Option<String>,
    },
    /// Row: schema_name, name, base_object_name. SQL Server only; other dialects return an empty set.
    Synonyms {
        schema: Option<String>,
    },
    /// Row: name, version, schema_name, description. PostgreSQL only; other dialects return an empty set.
    Extensions,
    /// Row: name, version, description, installed (0/1). Powers the "Install extension…" chooser.
    AvailableExtensions,
    /// Row: schema_name, name, status, event_type, interval_or_at, definition. MySQL/MariaDB only.
    Events {
        schema: Option<String>,
    },
    /// Row: schema_name, name, is_populated. PostgreSQL only; others return an empty set.
    MaterializedViews {
        schema: Option<String>,
    },
    /// One trigger's definition text. Row: definition.
    TriggerDefinition {
        schema: String,
        name: String,
    },
    /// One constraint's definition (check expression or unique columns). Row: definition.
    ConstraintDefinition {
        schema: String,
        table: String,
        name: String,
    },
    /// One user-defined type's full detail row.
    TypeDetail {
        schema: String,
        type_name: String,
    },
    /// Table identity + row-count estimate (the editor's General page for
    /// tables).
    TableGeneral {
        schema: String,
        table: String,
    },
    /// One database's own properties (the editor's General page for a
    /// database): name, state, collation, recovery model, compatibility
    /// level, creation date, size on disk, owner. Engines without a concept
    /// return NULL for that column rather than omitting it, so the row shape
    /// is the same everywhere.
    DatabaseGeneral {
        database: String,
    },
    /// The storage a database occupies: one row per file on engines that have
    /// files as a first-class concept (SQL Server), one summary row on the
    /// engines that do not. Row: logical_name, type_desc, physical_name,
    /// size_mb, max_size_mb, growth — NULL wherever the engine has no such
    /// concept, so the row shape is the same everywhere.
    DatabaseFiles {
        database: String,
    },
    /// Effective database-scope permissions for the current principal. Row:
    /// permission_name, state_desc, class_desc — the same shape as
    /// [`MetadataQuery::ObjectPermissions`].
    DatabasePermissions {
        database: String,
    },
    /// One column's own properties (the editor's General page for a column):
    /// position, type, length/precision/scale, nullability, default, and the
    /// identity/computed/collation flags.
    ColumnGeneral {
        schema: String,
        table: String,
        column: String,
    },
    /// Generic `sys.objects` identity row (schema, name, type_desc, dates).
    /// Serves the General page of views, procedures, and functions.
    ObjectGeneral {
        schema: String,
        object: String,
    },
    /// One index's option row: type, uniqueness, PK/disabled flags, filter,
    /// fill factor.
    IndexGeneral {
        schema: String,
        table: String,
        index: String,
    },
    /// One index's key and included columns (`is_included_column` splits the
    /// two lists; `key_ordinal`/`index_column_id` order within each).
    IndexColumns {
        schema: String,
        table: String,
        index: String,
    },
    /// Default expressions per column for one table.
    ColumnDefaults {
        schema: String,
        table: String,
    },
    /// Index key/include columns for every index on one table.
    TableIndexColumns {
        schema: String,
        table: String,
    },
    /// Views whose definitions reference a table. Row: schema, name,
    /// is_materialized, definition — ordered so base views precede dependents
    /// where the engine can tell.
    DependentViews {
        schema: String,
        table: String,
    },
    /// One statistic's option row: auto/user created, NORECOMPUTE, last
    /// updated.
    StatisticGeneral {
        schema: String,
        table: String,
        statistic: String,
    },
    /// One statistic's columns in stats-column ordinal order.
    StatisticColumns {
        schema: String,
        table: String,
        statistic: String,
    },
    /// Stored SQL definition of a view, procedure, or function
    /// (`OBJECT_DEFINITION`).
    ObjectDefinition {
        schema: String,
        object: String,
    },
    /// Effective object permissions for the current principal
    /// (`sys.fn_my_permissions`).
    ObjectPermissions {
        schema: String,
        object: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplorerNodeState {
    Unloaded,
    Loading,
    Loaded,
    Refreshing,
    PermissionDenied,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectExplorerAction {
    NewQuery,
    SelectTopRows,
    Script(ScriptAction),
    Properties,
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectExplorerDetailsRow {
    pub name: String,
    pub kind: ObjectKind,
    pub schema: Option<String>,
    pub is_expandable: bool,
    pub state: ExplorerNodeState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectPropertiesSpec {
    pub kind: ObjectKind,
    pub schema: Option<String>,
    pub name: String,
    pub qualified_name: String,
    pub sections: Vec<ObjectPropertiesSection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectPropertiesSection {
    pub title: String,
    pub description: String,
    pub query: String,
    pub read_only: bool,
}

pub fn server_root_nodes() -> Vec<ObjectNode> {
    vec![
        ObjectNode::new(ObjectKind::Database, "Databases".to_string()).expandable(),
        ObjectNode::new(ObjectKind::User, "Security".to_string()).expandable(),
        ObjectNode::new(ObjectKind::Endpoint, "Server Objects".to_string()).expandable(),
        ObjectNode::new(ObjectKind::ErrorLog, "Management".to_string()).expandable(),
        ObjectNode::new(ObjectKind::Job, "SQL Server Agent".to_string()).expandable(),
    ]
}

pub fn server_root_nodes_for_profile(profile: &ServerCapabilityProfile) -> Vec<ObjectNode> {
    server_root_nodes()
        .into_iter()
        .filter(|node| match node.name.as_str() {
            "SQL Server Agent" => profile.supports(ServerCapability::SqlAgent),
            "Server Objects" => !profile.is_azure_sql || profile.is_azure_mi,
            "Management" => !profile.is_azure_sql || profile.is_azure_mi,
            _ => true,
        })
        .collect()
}

pub fn database_root_nodes() -> Vec<ObjectNode> {
    vec![
        ObjectNode::new(ObjectKind::Table, "Tables".to_string()).expandable(),
        ObjectNode::new(ObjectKind::View, "Views".to_string()).expandable(),
        ObjectNode::new(ObjectKind::StoredProcedure, "Programmability".to_string()).expandable(),
        ObjectNode::new(ObjectKind::User, "Security".to_string()).expandable(),
        ObjectNode::new(ObjectKind::Filegroup, "Storage".to_string()).expandable(),
        ObjectNode::new(ObjectKind::Service, "Service Broker".to_string()).expandable(),
    ]
}

pub fn database_root_nodes_for_profile(profile: &ServerCapabilityProfile) -> Vec<ObjectNode> {
    let mut nodes = database_root_nodes();
    if profile.supports(ServerCapability::QueryStore) {
        nodes.push(ObjectNode::new(ObjectKind::Service, "Query Store".to_string()).expandable());
    }
    if profile.is_azure_sql {
        nodes.retain(|node| node.name != "Service Broker");
        nodes.push(
            ObjectNode::new(ObjectKind::Credential, "External Resources".to_string()).expandable(),
        );
    }
    nodes
}

pub fn schema_child_nodes(parent: &ObjectNode) -> Vec<ObjectNode> {
    [
        (ObjectKind::Column, "Columns"),
        (ObjectKind::Key, "Keys"),
        (ObjectKind::Constraint, "Constraints"),
        (ObjectKind::Index, "Indexes"),
        (ObjectKind::Trigger, "Triggers"),
        (ObjectKind::Type, "Statistics"),
    ]
    .into_iter()
    .map(|(kind, name)| {
        ObjectNode::new(kind, name.to_string())
            .with_parent(parent.id.clone())
            .expandable()
    })
    .collect()
}

pub fn context_actions(node: &ObjectNode) -> Vec<ObjectExplorerAction> {
    let mut actions = vec![ObjectExplorerAction::NewQuery];
    if matches!(node.kind, ObjectKind::Table | ObjectKind::View) {
        actions.push(ObjectExplorerAction::SelectTopRows);
    }
    if is_scriptable(&node.kind) {
        actions.push(ObjectExplorerAction::Script(ScriptAction::Create));
        actions.push(ObjectExplorerAction::Script(ScriptAction::Drop));
    }
    actions.push(ObjectExplorerAction::Properties);
    actions.push(ObjectExplorerAction::Refresh);
    actions
}

pub fn details_rows(
    nodes: &[ObjectNode],
    state: ExplorerNodeState,
) -> Vec<ObjectExplorerDetailsRow> {
    nodes
        .iter()
        .map(|node| ObjectExplorerDetailsRow {
            name: node.name.clone(),
            kind: node.kind.clone(),
            schema: node.schema.clone(),
            is_expandable: node.is_expandable,
            state,
        })
        .collect()
}

pub fn filter_nodes(nodes: &[ObjectNode], query: &str) -> Vec<ObjectNode> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return nodes.to_vec();
    }
    nodes
        .iter()
        .filter(|node| {
            node.name.to_lowercase().contains(&needle)
                || node
                    .schema
                    .as_deref()
                    .is_some_and(|schema| schema.to_lowercase().contains(&needle))
                || format!("{:?}", node.kind).to_lowercase().contains(&needle)
        })
        .cloned()
        .collect()
}

pub fn object_properties_spec(
    kind: ObjectKind,
    schema: Option<&str>,
    name: &str,
) -> ObjectPropertiesSpec {
    let qualified_name = schema
        .filter(|schema| !schema.trim().is_empty())
        .map(|schema| format!("{schema}.{name}"))
        .unwrap_or_else(|| name.to_string());
    let sections = match kind {
        ObjectKind::Server => server_property_sections(),
        ObjectKind::Database => database_property_sections(name),
        ObjectKind::Table => table_property_sections(schema.unwrap_or("dbo"), name),
        ObjectKind::View => view_property_sections(schema.unwrap_or("dbo"), name),
        ObjectKind::StoredProcedure
        | ObjectKind::ScalarFunction
        | ObjectKind::TableFunction
        | ObjectKind::AggregateFunction => {
            programmable_property_sections(schema.unwrap_or("dbo"), name)
        }
        _ => generic_object_property_sections(schema.unwrap_or("dbo"), name),
    };
    ObjectPropertiesSpec {
        kind,
        schema: schema.map(str::to_string),
        name: name.to_string(),
        qualified_name,
        sections,
    }
}

pub fn sql_for(query: &MetadataQuery) -> String {
    match query {
        MetadataQuery::ServerCapability => server_capability_query(),
        MetadataQuery::ServerPermissions => server_permission_query(),
        MetadataQuery::ServerConfiguration => {
            // is_advanced: sp_configure refuses these (error 15123) until
            // 'show advanced options' is in use; the editor's plan wraps them.
            "SELECT name, CAST(value_in_use AS nvarchar(200)) AS value, description AS source, is_dynamic, is_advanced FROM sys.configurations ORDER BY name"
                .to_string()
        }
        MetadataQuery::ServerStatus => {
            // Bounded to curated object classes, and to the counter types that
            // read correctly on their own: instantaneous large raw counts
            // (65792) and per-second bulk counts (272696576). Ratio/average
            // counters need their base counter to mean anything and are left
            // out rather than shown as meaningless raw values.
            "SELECT RTRIM(counter_name) + CASE WHEN RTRIM(instance_name) IN ('', '_Total') THEN '' ELSE ' (' + RTRIM(instance_name) + ')' END AS name, CAST(cntr_value AS nvarchar(64)) AS value, SUBSTRING(RTRIM(object_name), CHARINDEX(':', RTRIM(object_name)) + 1, 256) AS source FROM sys.dm_os_performance_counters WHERE (object_name LIKE '%:Access Methods' OR object_name LIKE '%:Buffer Manager' OR object_name LIKE '%:General Statistics' OR object_name LIKE '%:Locks' OR object_name LIKE '%:Memory Manager' OR object_name LIKE '%:SQL Statistics') AND cntr_type IN (65792, 272696576) ORDER BY source, name"
                .to_string()
        }
        MetadataQuery::Databases => {
            "SELECT name, state_desc, compatibility_level FROM sys.databases ORDER BY name"
                .to_string()
        }
        // No USE — Azure SQL Database rejects it; the caller connects to the
        // target database instead (`profile_with_db`).
        MetadataQuery::Schemas => {
            "SELECT name FROM sys.schemas WHERE principal_id IS NOT NULL ORDER BY name".to_string()
        }
        MetadataQuery::SchemaObjectCounts => {
            // Counts come from `sys.all_objects`, not `sys.objects`, so the
            // system schemas (`sys` / `INFORMATION_SCHEMA`) count their
            // ms-shipped objects — the same population their folders enumerate.
            // Counting only user objects made them look empty and get hidden
            // under the "Only If Not Empty" placement. User schemas are
            // unaffected: ms-shipped objects live solely in the two system
            // schemas, so `all_objects` and `objects` agree everywhere else.
            "SELECT s.name, COUNT(o.object_id) AS object_count FROM sys.schemas s LEFT JOIN sys.all_objects o ON o.schema_id = s.schema_id AND o.type IN (N'U', N'V', N'P', N'FN', N'IF', N'TF', N'AF') WHERE s.principal_id IS NOT NULL GROUP BY s.name ORDER BY s.name"
                .to_string()
        }
        MetadataQuery::Tables { schema } => object_query("U", schema.as_deref()),
        MetadataQuery::Views { schema } => object_query("V", schema.as_deref()),
        MetadataQuery::ProgrammableObjects { schema } => programmable_query(schema.as_deref()),
        MetadataQuery::ObjectInventory { schema } => object_inventory_query(schema.as_deref()),
        MetadataQuery::Columns { schema, object } => {
            // System objects (in sys / INFORMATION_SCHEMA) live in
            // `sys.all_columns`, not `sys.columns`, so widen the source there
            // or a system view expands to zero columns.
            let column_source = if is_system_object_schema(schema) {
                "sys.all_columns"
            } else {
                "sys.columns"
            };
            format!(
                "SELECT c.name, t.name AS type_name, c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity, c.is_computed FROM {column_source} c JOIN sys.types t ON c.user_type_id = t.user_type_id WHERE c.object_id = OBJECT_ID(N'{}') ORDER BY c.column_id",
                escape_literal(&format!("{schema}.{object}"))
            )
        }
        MetadataQuery::AllColumns => {
            "SELECT SCHEMA_NAME(o.schema_id) AS schema_name, o.name AS object_name, c.name AS column_name, t.name AS type_name, c.is_nullable, c.is_identity FROM sys.columns c JOIN sys.objects o ON o.object_id = c.object_id JOIN sys.types t ON t.user_type_id = c.user_type_id WHERE o.type IN (N'U', N'V') ORDER BY schema_name, object_name, c.column_id"
                .to_string()
        }
        MetadataQuery::AllForeignKeys => {
            // `constraint_name` + ordering by it and `constraint_column_id`
            // lets the caller group multi-column (composite) foreign keys back
            // into a single relationship and derive stable node ids from the
            // constraint identity instead of a fragile row position.
            "SELECT fk.name AS constraint_name, SCHEMA_NAME(ct.schema_id) AS from_schema, ct.name AS from_table, cc.name AS from_column, SCHEMA_NAME(pt.schema_id) AS to_schema, pt.name AS to_table, pc.name AS to_column FROM sys.foreign_key_columns fkc JOIN sys.foreign_keys fk ON fk.object_id = fkc.constraint_object_id JOIN sys.tables ct ON ct.object_id = fkc.parent_object_id JOIN sys.columns cc ON cc.object_id = fkc.parent_object_id AND cc.column_id = fkc.parent_column_id JOIN sys.tables pt ON pt.object_id = fkc.referenced_object_id JOIN sys.columns pc ON pc.object_id = fkc.referenced_object_id AND pc.column_id = fkc.referenced_column_id ORDER BY constraint_name, fkc.constraint_column_id"
                .to_string()
        }
        MetadataQuery::AllPrimaryKeys => {
            "SELECT SCHEMA_NAME(t.schema_id) AS schema_name, t.name AS table_name, c.name AS column_name, ic.key_ordinal FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id JOIN sys.tables t ON t.object_id = i.object_id WHERE i.is_primary_key = 1 ORDER BY schema_name, table_name, ic.key_ordinal"
                .to_string()
        }
        MetadataQuery::ProcedureParameters { schema, object } => format!(
            "SELECT p.name, TYPE_NAME(p.user_type_id) AS type_name, p.max_length, p.precision, p.scale, p.is_output, p.has_default_value FROM sys.parameters p WHERE p.object_id = OBJECT_ID(N'{}') ORDER BY p.parameter_id",
            escape_literal(&format!("{schema}.{object}"))
        ),
        MetadataQuery::Indexes { schema, table } => format!(
            "SELECT i.name, i.type_desc, i.is_unique, i.is_primary_key, i.filter_definition, i.fill_factor FROM sys.indexes i WHERE i.object_id = OBJECT_ID(N'{}') AND i.index_id > 0 ORDER BY i.index_id",
            escape_literal(&format!("{schema}.{table}"))
        ),
        MetadataQuery::DependentViews { schema, table } => format!(
            "SELECT SCHEMA_NAME(v.schema_id), v.name, 0, OBJECT_DEFINITION(v.object_id) FROM sys.sql_expression_dependencies d JOIN sys.views v ON v.object_id = d.referencing_id WHERE d.referenced_id = OBJECT_ID(N'{}') ORDER BY 1, 2",
            escape_literal(&format!("{schema}.{table}"))
        ),
        MetadataQuery::Triggers { schema, table } => format!(
            "SELECT tr.name, tr.create_date, tr.modify_date, tr.is_disabled FROM sys.triggers tr WHERE tr.parent_id = OBJECT_ID(N'{}') ORDER BY tr.name",
            escape_literal(&format!("{schema}.{table}"))
        ),
        MetadataQuery::Statistics { schema, table } => format!(
            "SELECT s.name, s.auto_created, s.user_created, s.no_recompute, STATS_DATE(s.object_id, s.stats_id) AS updated_at FROM sys.stats s WHERE s.object_id = OBJECT_ID(N'{}') ORDER BY s.name",
            escape_literal(&format!("{schema}.{table}"))
        ),
        MetadataQuery::Constraints { schema, table } => format!(
            "SELECT kc.name, kc.type_desc FROM sys.key_constraints kc WHERE kc.parent_object_id = OBJECT_ID(N'{}') UNION ALL SELECT dc.name, 'DEFAULT_CONSTRAINT' FROM sys.default_constraints dc WHERE dc.parent_object_id = OBJECT_ID(N'{}') UNION ALL SELECT cc.name, 'CHECK_CONSTRAINT' FROM sys.check_constraints cc WHERE cc.parent_object_id = OBJECT_ID(N'{}') ORDER BY name",
            escape_literal(&format!("{schema}.{table}")),
            escape_literal(&format!("{schema}.{table}")),
            escape_literal(&format!("{schema}.{table}"))
        ),
        MetadataQuery::Types { schema } => {
            let schema_filter = schema
                .as_deref()
                .map(|schema| {
                    format!(
                        " AND SCHEMA_NAME(t.schema_id) = N'{}'",
                        escape_literal(schema)
                    )
                })
                .unwrap_or_default();
            format!(
                "SELECT SCHEMA_NAME(t.schema_id) AS schema_name, t.name, TYPE_NAME(t.system_type_id) AS base_type, t.max_length, t.precision, t.scale, t.is_nullable FROM sys.types t WHERE t.is_user_defined = 1{schema_filter} ORDER BY schema_name, t.name"
            )
        }
        MetadataQuery::DatabaseTriggers { schema } => {
            let database_triggers = "SELECT CAST(NULL AS sysname) AS schema_name, tr.name, CAST(NULL AS sysname) AS parent_name, tr.is_disabled FROM sys.triggers tr WHERE tr.parent_class = 0";
            match schema.as_deref() {
                Some(schema) => format!(
                    "{database_triggers} UNION ALL SELECT SCHEMA_NAME(o.schema_id) AS schema_name, tr.name, o.name AS parent_name, tr.is_disabled FROM sys.triggers tr JOIN sys.objects o ON o.object_id = tr.parent_id WHERE tr.parent_class = 1 AND SCHEMA_NAME(o.schema_id) = N'{}' ORDER BY schema_name, name",
                    escape_literal(schema)
                ),
                None => format!("{database_triggers} ORDER BY name"),
            }
        }
        MetadataQuery::Sequences { schema } => {
            let schema_filter = schema
                .as_deref()
                .map(|schema| {
                    format!(
                        " AND SCHEMA_NAME(s.schema_id) = N'{}'",
                        escape_literal(schema)
                    )
                })
                .unwrap_or_default();
            format!(
                "SELECT SCHEMA_NAME(s.schema_id) AS schema_name, s.name, TYPE_NAME(s.user_type_id) AS data_type, s.start_value, s.increment, s.current_value FROM sys.sequences s WHERE 1 = 1{schema_filter} ORDER BY schema_name, s.name"
            )
        }
        MetadataQuery::Synonyms { schema } => {
            let schema_filter = schema
                .as_deref()
                .map(|schema| {
                    format!(
                        " AND SCHEMA_NAME(syn.schema_id) = N'{}'",
                        escape_literal(schema)
                    )
                })
                .unwrap_or_default();
            format!(
                "SELECT SCHEMA_NAME(syn.schema_id) AS schema_name, syn.name, syn.base_object_name FROM sys.synonyms syn WHERE 1 = 1{schema_filter} ORDER BY schema_name, syn.name"
            )
        }
        MetadataQuery::Extensions => {
            "SELECT CAST('' AS sysname) AS name, CAST('' AS sysname) AS version, CAST('' AS sysname) AS schema_name, CAST('' AS nvarchar(max)) AS description WHERE 1 = 0".to_string()
        }
        MetadataQuery::AvailableExtensions => {
            "SELECT CAST('' AS sysname) AS name, CAST('' AS sysname) AS version, CAST('' AS nvarchar(max)) AS description, CAST(0 AS int) AS installed WHERE 1 = 0".to_string()
        }
        MetadataQuery::Events { .. } => {
            "SELECT CAST('' AS sysname) AS schema_name, CAST('' AS sysname) AS name, CAST('' AS nvarchar(60)) AS status, CAST('' AS nvarchar(60)) AS event_type, CAST('' AS nvarchar(max)) AS interval_or_at, CAST('' AS nvarchar(max)) AS definition WHERE 1 = 0".to_string()
        }
        MetadataQuery::MaterializedViews { .. } => {
            "SELECT CAST('' AS sysname) AS schema_name, CAST('' AS sysname) AS name, CAST(0 AS int) AS is_populated WHERE 1 = 0".to_string()
        }
        MetadataQuery::TriggerDefinition { schema, name } => format!(
            "SELECT OBJECT_DEFINITION(OBJECT_ID(N'{}')) AS definition",
            escape_literal(&format!("{schema}.{name}"))
        ),
        MetadataQuery::ConstraintDefinition { schema, table, name } => format!(
            "SELECT cc.definition FROM sys.check_constraints cc JOIN sys.objects o ON o.object_id = cc.parent_object_id JOIN sys.schemas s ON s.schema_id = o.schema_id WHERE s.name = N'{schema}' AND o.name = N'{table}' AND cc.name = N'{name}' UNION ALL SELECT 'UNIQUE (' + STRING_AGG(QUOTENAME(c.name), ', ') WITHIN GROUP (ORDER BY ic.key_ordinal) + ')' FROM sys.key_constraints kc JOIN sys.objects o ON o.object_id = kc.parent_object_id JOIN sys.schemas s ON s.schema_id = o.schema_id JOIN sys.index_columns ic ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id WHERE kc.type = 'UQ' AND s.name = N'{schema}' AND o.name = N'{table}' AND kc.name = N'{name}' GROUP BY kc.object_id",
            schema = escape_literal(schema),
            table = escape_literal(table),
            name = escape_literal(name)
        ),
        MetadataQuery::TypeDetail { schema, type_name } => format!(
            "SELECT SCHEMA_NAME(t.schema_id) AS schema_name, t.name, TYPE_NAME(t.system_type_id) AS base_type, t.max_length, t.precision, t.scale, t.is_nullable FROM sys.types t WHERE t.is_user_defined = 1 AND SCHEMA_NAME(t.schema_id) = N'{}' AND t.name = N'{}'",
            escape_literal(schema),
            escape_literal(type_name)
        ),
        MetadataQuery::TableGeneral { schema, table } => format!(
            "SELECT SCHEMA_NAME(o.schema_id) AS schema_name, o.name, o.create_date, o.modify_date, SUM(p.rows) AS row_count FROM sys.objects o LEFT JOIN sys.partitions p ON p.object_id = o.object_id AND p.index_id IN (0,1) WHERE o.object_id = OBJECT_ID(N'{}') GROUP BY o.schema_id, o.name, o.create_date, o.modify_date",
            escape_literal(&format!("{schema}.{table}"))
        ),
        MetadataQuery::ObjectGeneral { schema, object } => format!(
            "SELECT SCHEMA_NAME(schema_id) AS schema_name, name, type_desc, create_date, modify_date FROM sys.objects WHERE object_id = OBJECT_ID(N'{}')",
            escape_literal(&format!("{schema}.{object}"))
        ),
        MetadataQuery::DatabaseGeneral { database } => format!(
            "SELECT d.name, d.state_desc, d.collation_name, d.recovery_model_desc AS recovery_model, d.compatibility_level, d.create_date, (SELECT CAST(SUM(mf.size) * 8.0 / 1024 AS decimal(18,2)) FROM sys.master_files mf WHERE mf.database_id = d.database_id) AS size_mb, SUSER_SNAME(d.owner_sid) AS owner FROM sys.databases d WHERE d.name = N'{}'",
            escape_literal(database)
        ),
        // `sys.database_files` (not `sys.master_files`) so the query works on
        // Azure SQL Database too; the caller has already switched the
        // connection to the target database.
        MetadataQuery::DatabaseFiles { .. } => {
            "SELECT f.name AS logical_name, f.type_desc, f.physical_name, CAST(f.size * 8 / 1024.0 AS decimal(18,1)) AS size_mb, CASE WHEN f.max_size = -1 THEN NULL ELSE CAST(CAST(f.max_size AS bigint) * 8 / 1024.0 AS decimal(18,1)) END AS max_size_mb, CASE WHEN f.is_percent_growth = 1 THEN CAST(f.growth AS varchar(20)) + '%' ELSE CAST(CAST(f.growth * 8 / 1024.0 AS decimal(18,1)) AS varchar(24)) + ' MB' END AS growth FROM sys.database_files f ORDER BY f.type, f.name"
                .to_string()
        }
        MetadataQuery::DatabasePermissions { .. } => {
            "SELECT permission_name, 'GRANT' AS state_desc, 'DATABASE' AS class_desc FROM sys.fn_my_permissions(NULL, 'DATABASE') ORDER BY permission_name"
                .to_string()
        }
        MetadataQuery::ColumnGeneral {
            schema,
            table,
            column,
        } => format!(
            "SELECT SCHEMA_NAME(o.schema_id) AS schema_name, o.name AS table_name, c.name, c.column_id AS ordinal, ty.name AS data_type, c.max_length, c.precision, c.scale, c.is_nullable, dc.definition AS default_definition, c.is_identity, c.is_computed, c.collation_name FROM sys.columns c JOIN sys.objects o ON o.object_id = c.object_id JOIN sys.types ty ON ty.user_type_id = c.user_type_id LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id WHERE c.object_id = OBJECT_ID(N'{}') AND c.name = N'{}'",
            escape_literal(&format!("{schema}.{table}")),
            escape_literal(column)
        ),
        MetadataQuery::IndexGeneral {
            schema,
            table,
            index,
        } => format!(
            "SELECT i.name, i.type_desc, i.is_unique, i.is_primary_key, i.is_disabled, i.filter_definition, i.fill_factor FROM sys.indexes i WHERE i.object_id = OBJECT_ID(N'{}') AND i.name = N'{}'",
            escape_literal(&format!("{schema}.{table}")),
            escape_literal(index)
        ),
        MetadataQuery::IndexColumns {
            schema,
            table,
            index,
        } => format!(
            "SELECT c.name, ic.key_ordinal, ic.is_descending_key, ic.is_included_column FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id WHERE i.object_id = OBJECT_ID(N'{}') AND i.name = N'{}' ORDER BY ic.is_included_column, ic.key_ordinal, ic.index_column_id",
            escape_literal(&format!("{schema}.{table}")),
            escape_literal(index)
        ),
        MetadataQuery::ColumnDefaults { schema, table } => format!(
            "SELECT c.name, dc.definition, dc.name FROM sys.columns c JOIN sys.default_constraints dc ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id WHERE c.object_id = OBJECT_ID(N'{}') ORDER BY c.column_id",
            escape_literal(&format!("{schema}.{table}"))
        ),
        MetadataQuery::TableIndexColumns { schema, table } => format!(
            "SELECT i.name AS index_name, c.name AS column_name, ic.key_ordinal AS ordinal, ic.is_included_column AS is_included, ic.is_descending_key AS is_descending FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id WHERE i.object_id = OBJECT_ID(N'{}') AND i.index_id > 0 ORDER BY i.name, ic.is_included_column, ic.key_ordinal, ic.index_column_id",
            escape_literal(&format!("{schema}.{table}"))
        ),
        MetadataQuery::StatisticGeneral {
            schema,
            table,
            statistic,
        } => format!(
            "SELECT s.name, s.auto_created, s.user_created, s.no_recompute, STATS_DATE(s.object_id, s.stats_id) AS updated_at FROM sys.stats s WHERE s.object_id = OBJECT_ID(N'{}') AND s.name = N'{}'",
            escape_literal(&format!("{schema}.{table}")),
            escape_literal(statistic)
        ),
        MetadataQuery::StatisticColumns {
            schema,
            table,
            statistic,
        } => format!(
            "SELECT c.name FROM sys.stats s JOIN sys.stats_columns sc ON sc.object_id = s.object_id AND sc.stats_id = s.stats_id JOIN sys.columns c ON c.object_id = sc.object_id AND c.column_id = sc.column_id WHERE s.object_id = OBJECT_ID(N'{}') AND s.name = N'{}' ORDER BY sc.stats_column_id",
            escape_literal(&format!("{schema}.{table}")),
            escape_literal(statistic)
        ),
        MetadataQuery::ObjectDefinition { schema, object } => format!(
            "SELECT OBJECT_DEFINITION(OBJECT_ID(N'{}')) AS definition",
            escape_literal(&format!("{schema}.{object}"))
        ),
        MetadataQuery::ObjectPermissions { schema, object } => format!(
            "SELECT permission_name, 'GRANT' AS state_desc, 'OBJECT' AS class_desc FROM sys.fn_my_permissions(N'{}', 'OBJECT') ORDER BY permission_name",
            escape_literal(&format!("{schema}.{object}"))
        ),
    }
}

fn server_capability_query() -> String {
    // Every column is CAST to a concrete type: bare `SERVERPROPERTY(...)`
    // returns `sql_variant`, which the TDS driver cannot map — the producer
    // aborts mid-stream and the page loads zero rows ("No properties were
    // returned"). Casting to nvarchar (as the server-info probe already does)
    // keeps the result a plain string column.
    "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(256)) AS product_version, CAST(SERVERPROPERTY('Edition') AS nvarchar(256)) AS edition, CAST(SERVERPROPERTY('EngineEdition') AS nvarchar(256)) AS engine_edition, CAST(SERVERPROPERTY('ProductLevel') AS nvarchar(256)) AS product_level, CAST(SERVERPROPERTY('ProductUpdateLevel') AS nvarchar(256)) AS product_update_level, CAST(SERVERPROPERTY('MachineName') AS nvarchar(256)) AS machine_name, CAST(SERVERPROPERTY('IsIntegratedSecurityOnly') AS int) AS integrated_security_only".to_string()
}

fn section(title: &str, description: &str, query: String) -> ObjectPropertiesSection {
    ObjectPropertiesSection {
        title: title.to_string(),
        description: description.to_string(),
        query,
        read_only: true,
    }
}

fn server_property_sections() -> Vec<ObjectPropertiesSection> {
    vec![
        section(
            "General",
            "SQL Server instance and edition properties.",
            server_capability_query(),
        ),
        section(
            "Permissions",
            "Effective server permissions for the current principal.",
            server_permission_query(),
        ),
    ]
}

fn database_property_sections(name: &str) -> Vec<ObjectPropertiesSection> {
    let db = escape_literal(name);
    vec![
        section(
            "General",
            "Database state, recovery model, compatibility level, and ownership.",
            format!("SELECT name, state_desc, user_access_desc, recovery_model_desc, compatibility_level, containment_desc, create_date, collation_name, SUSER_SNAME(owner_sid) AS owner_name FROM sys.databases WHERE name = N'{db}'"),
        ),
        section(
            "Files",
            "Database files, growth settings, and current sizes.",
            "SELECT name, type_desc, physical_name, size * 8 / 1024 AS size_mb, max_size, growth, is_percent_growth FROM sys.database_files ORDER BY file_id".to_string(),
        ),
        section(
            "Permissions",
            "Effective database permissions for the current principal.",
            "SELECT permission_name, 'GRANT' AS state_desc, 'DATABASE' AS class_desc FROM sys.fn_my_permissions(NULL, 'DATABASE') ORDER BY permission_name".to_string(),
        ),
    ]
}

fn table_property_sections(schema: &str, table: &str) -> Vec<ObjectPropertiesSection> {
    let object = escape_literal(&format!("{schema}.{table}"));
    vec![
        section(
            "General",
            "Table identity, row count estimate, create date, and modify date.",
            format!(
                "SELECT SCHEMA_NAME(o.schema_id) AS schema_name, o.name, o.create_date, o.modify_date, SUM(p.rows) AS row_count FROM sys.objects o LEFT JOIN sys.partitions p ON p.object_id = o.object_id AND p.index_id IN (0,1) WHERE o.object_id = OBJECT_ID(N'{object}') GROUP BY o.schema_id, o.name, o.create_date, o.modify_date"
            ),
        ),
        section(
            "Columns",
            "Column type, nullability, identity, and computed-column metadata.",
            sql_for(&MetadataQuery::Columns {
                schema: schema.to_string(),
                object: table.to_string(),
            }),
        ),
        section(
            "Indexes",
            "Indexes, uniqueness, primary-key flags, and filters.",
            sql_for(&MetadataQuery::Indexes {
                schema: schema.to_string(),
                table: table.to_string(),
            }),
        ),
        section(
            "Constraints",
            "Primary key, unique, default, and check constraints.",
            sql_for(&MetadataQuery::Constraints {
                schema: schema.to_string(),
                table: table.to_string(),
            }),
        ),
        section(
            "Foreign Keys",
            "Relationships this table declares (references out) and relationships that point back at it (referenced by).",
            format!(
                "SELECT CASE WHEN fk.parent_object_id = OBJECT_ID(N'{object}') THEN 'References out' ELSE 'Referenced by' END AS direction, fk.name AS constraint_name, SCHEMA_NAME(pt.schema_id) + '.' + pt.name AS from_table, pc.name AS from_column, SCHEMA_NAME(rt.schema_id) + '.' + rt.name AS to_table, rc.name AS to_column FROM sys.foreign_keys fk JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id JOIN sys.tables pt ON pt.object_id = fk.parent_object_id JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id WHERE fk.parent_object_id = OBJECT_ID(N'{object}') OR fk.referenced_object_id = OBJECT_ID(N'{object}') ORDER BY direction, constraint_name, fkc.constraint_column_id"
            ),
        ),
        section(
            "Triggers",
            "DML triggers attached to this table.",
            sql_for(&MetadataQuery::Triggers {
                schema: schema.to_string(),
                table: table.to_string(),
            }),
        ),
        section(
            "Permissions",
            "Effective object permissions for the current principal.",
            format!(
                "SELECT permission_name, 'GRANT' AS state_desc, 'OBJECT' AS class_desc FROM sys.fn_my_permissions(N'{object}', 'OBJECT') ORDER BY permission_name"
            ),
        ),
    ]
}

fn view_property_sections(schema: &str, view: &str) -> Vec<ObjectPropertiesSection> {
    let object = escape_literal(&format!("{schema}.{view}"));
    vec![
        section(
            "General",
            "View identity, create date, and modify date.",
            format!(
                "SELECT SCHEMA_NAME(schema_id) AS schema_name, name, create_date, modify_date FROM sys.views WHERE object_id = OBJECT_ID(N'{object}')"
            ),
        ),
        section(
            "Columns",
            "Projected columns and SQL Server type metadata.",
            sql_for(&MetadataQuery::Columns {
                schema: schema.to_string(),
                object: view.to_string(),
            }),
        ),
        section(
            "Definition",
            "Stored SQL definition for this view.",
            format!("SELECT OBJECT_DEFINITION(OBJECT_ID(N'{object}')) AS definition"),
        ),
        section(
            "Dependencies",
            "Referenced entities discovered by SQL Server dependency tracking.",
            format!(
                "SELECT referenced_schema_name, referenced_entity_name, referenced_class_desc FROM sys.dm_sql_referenced_entities(N'{object}', 'OBJECT') ORDER BY referenced_schema_name, referenced_entity_name"
            ),
        ),
        section(
            "Permissions",
            "Effective object permissions for the current principal.",
            format!(
                "SELECT permission_name, 'GRANT' AS state_desc, 'OBJECT' AS class_desc FROM sys.fn_my_permissions(N'{object}', 'OBJECT') ORDER BY permission_name"
            ),
        ),
    ]
}

fn programmable_property_sections(schema: &str, object_name: &str) -> Vec<ObjectPropertiesSection> {
    let object = escape_literal(&format!("{schema}.{object_name}"));
    vec![
        section(
            "General",
            "Programmable object type, create date, and modify date.",
            format!(
                "SELECT SCHEMA_NAME(schema_id) AS schema_name, name, type_desc, create_date, modify_date FROM sys.objects WHERE object_id = OBJECT_ID(N'{object}')"
            ),
        ),
        section(
            "Parameters",
            "Input/output parameters and SQL Server type metadata.",
            format!(
                "SELECT p.name, TYPE_NAME(p.user_type_id) AS type_name, p.max_length, p.precision, p.scale, p.is_output, p.has_default_value FROM sys.parameters p WHERE p.object_id = OBJECT_ID(N'{object}') ORDER BY p.parameter_id"
            ),
        ),
        section(
            "Definition",
            "Stored SQL module definition.",
            format!("SELECT OBJECT_DEFINITION(OBJECT_ID(N'{object}')) AS definition"),
        ),
        section(
            "Dependencies",
            "Referenced entities discovered by SQL Server dependency tracking.",
            format!(
                "SELECT referenced_schema_name, referenced_entity_name, referenced_class_desc FROM sys.dm_sql_referenced_entities(N'{object}', 'OBJECT') ORDER BY referenced_schema_name, referenced_entity_name"
            ),
        ),
        section(
            "Permissions",
            "Effective object permissions for the current principal.",
            format!(
                "SELECT permission_name, 'GRANT' AS state_desc, 'OBJECT' AS class_desc FROM sys.fn_my_permissions(N'{object}', 'OBJECT') ORDER BY permission_name"
            ),
        ),
    ]
}

fn generic_object_property_sections(
    schema: &str,
    object_name: &str,
) -> Vec<ObjectPropertiesSection> {
    let object = escape_literal(&format!("{schema}.{object_name}"));
    vec![
        section(
            "General",
            "Object identity, schema, type, create date, and modify date.",
            format!(
                "SELECT SCHEMA_NAME(schema_id) AS schema_name, name, type_desc, create_date, modify_date FROM sys.objects WHERE object_id = OBJECT_ID(N'{object}')"
            ),
        ),
        section(
            "Permissions",
            "Effective object permissions for the current principal.",
            format!(
                "SELECT permission_name, 'GRANT' AS state_desc, 'OBJECT' AS class_desc FROM sys.fn_my_permissions(N'{object}', 'OBJECT') ORDER BY permission_name"
            ),
        ),
    ]
}

fn server_permission_query() -> String {
    "SELECT permission_name, 'GRANT' AS state_desc, 'SERVER' AS class_desc FROM sys.fn_my_permissions(NULL, 'SERVER') ORDER BY permission_name".to_string()
}

fn is_scriptable(kind: &ObjectKind) -> bool {
    matches!(
        kind,
        ObjectKind::Table
            | ObjectKind::View
            | ObjectKind::StoredProcedure
            | ObjectKind::ScalarFunction
            | ObjectKind::TableFunction
            | ObjectKind::AggregateFunction
            | ObjectKind::Trigger
            | ObjectKind::Schema
            | ObjectKind::Role
            | ObjectKind::User
            | ObjectKind::Index
            | ObjectKind::Constraint
    )
}

/// The `sys`/`INFORMATION_SCHEMA` schemas hold system objects, which are
/// ms-shipped and therefore *absent* from `sys.objects` / `sys.columns`
/// (those list user-defined objects only). Enumerating one of these has to
/// widen the catalog source to `sys.all_objects` / `sys.all_columns` or the
/// folder always comes back empty — which is exactly why nothing showed under
/// those schemas before.
///
/// Deliberately narrower than the explorer's *display* grouping, which also
/// files `guest` and the `db_*` role schemas under "System Schemas": those
/// schemas never contain ms-shipped objects, so anything in them is
/// user-created and `sys.objects` already lists it. Widening them would only
/// add noise exposure for zero coverage gain.
fn is_system_object_schema(schema: &str) -> bool {
    schema.eq_ignore_ascii_case("sys") || schema.eq_ignore_ascii_case("INFORMATION_SCHEMA")
}

/// Catalog view to enumerate objects from for a given schema scope. System
/// schemas use the wider `sys.all_objects`; every other scope keeps
/// `sys.objects` so ordinary user schemas never pull in system noise.
fn object_source(schema: Option<&str>) -> &'static str {
    match schema {
        Some(s) if is_system_object_schema(s) => "sys.all_objects",
        _ => "sys.objects",
    }
}

fn object_inventory_query(schema: Option<&str>) -> String {
    let schema_filter = schema
        .map(|schema| format!(" AND s.name = N'{}'", escape_literal(schema)))
        .unwrap_or_default();
    format!(
        "SELECT CASE o.type WHEN 'U' THEN 'TABLE' WHEN 'V' THEN 'VIEW' WHEN 'P' THEN 'PROCEDURE' WHEN 'FN' THEN 'FUNCTION' WHEN 'IF' THEN 'FUNCTION' WHEN 'TF' THEN 'FUNCTION' WHEN 'TR' THEN 'TRIGGER' END AS kind, s.name AS schema_name, o.name AS name, (SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0,1)) AS row_estimate, (SELECT SUM(a.total_pages) * 8 FROM sys.partitions p JOIN sys.allocation_units a ON a.container_id = p.partition_id WHERE p.object_id = o.object_id) AS size_kb, o.create_date, o.modify_date, (SELECT CAST(ep.value AS nvarchar(4000)) FROM sys.extended_properties ep WHERE ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = N'MS_Description') AS comment FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id WHERE o.is_ms_shipped = 0 AND o.type IN ('U','V','P','FN','IF','TF','TR'){schema_filter} ORDER BY kind, schema_name, name"
    )
}

fn object_query(object_type: &str, schema: Option<&str>) -> String {
    let source = object_source(schema);
    let schema_filter = schema
        .map(|schema| {
            format!(
                " AND SCHEMA_NAME(o.schema_id) = N'{}'",
                escape_literal(schema)
            )
        })
        .unwrap_or_default();
    format!(
        "SELECT SCHEMA_NAME(o.schema_id) AS schema_name, o.name, o.create_date, o.modify_date FROM {source} o WHERE o.type = N'{object_type}'{schema_filter} ORDER BY schema_name, o.name"
    )
}

fn programmable_query(schema: Option<&str>) -> String {
    let source = object_source(schema);
    let schema_filter = schema
        .map(|schema| {
            format!(
                " AND SCHEMA_NAME(o.schema_id) = N'{}'",
                escape_literal(schema)
            )
        })
        .unwrap_or_default();
    format!(
        "SELECT SCHEMA_NAME(o.schema_id) AS schema_name, o.name, o.type, o.create_date, o.modify_date FROM {source} o WHERE o.type IN (N'P', N'FN', N'IF', N'TF', N'TR'){schema_filter} ORDER BY schema_name, o.name"
    )
}

/// Bracket-quote an identifier for safe embedding in T-SQL (`[name]`,
/// with `]` doubled).
pub fn quote_name(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// Escape a string for safe embedding inside a T-SQL string literal
/// (single quotes doubled).
pub fn escape_literal(value: &str) -> String {
    value.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::DatabaseType;

    #[test]
    fn server_roots_are_expandable() {
        let roots = server_root_nodes();
        assert_eq!(roots.len(), 5);
        assert!(roots.iter().all(|node| node.is_expandable));
    }

    #[test]
    fn table_query_filters_schema_safely() {
        let sql = sql_for(&MetadataQuery::Tables {
            schema: Some("dbo'owned".to_string()),
        });
        assert!(sql.contains("SCHEMA_NAME(o.schema_id) = N'dbo''owned'"));
        assert!(sql.contains("o.type = N'U'"));
    }

    #[test]
    fn system_schemas_enumerate_from_all_objects() {
        // sys / INFORMATION_SCHEMA members are ms-shipped and absent from
        // sys.objects, so those scopes must widen to sys.all_objects or the
        // folder shows nothing.
        for schema in [
            "sys",
            "INFORMATION_SCHEMA",
            "information_schema",
            "Information_Schema",
        ] {
            let views = sql_for(&MetadataQuery::Views {
                schema: Some(schema.to_string()),
            });
            assert!(
                views.contains("FROM sys.all_objects o"),
                "views for {schema} should read sys.all_objects: {views}"
            );
            let progs = sql_for(&MetadataQuery::ProgrammableObjects {
                schema: Some(schema.to_string()),
            });
            assert!(
                progs.contains("FROM sys.all_objects o"),
                "programmable objects for {schema} should read sys.all_objects"
            );
            let cols = sql_for(&MetadataQuery::Columns {
                schema: schema.to_string(),
                object: "objects".to_string(),
            });
            assert!(
                cols.contains("FROM sys.all_columns c"),
                "columns for {schema} should read sys.all_columns"
            );
        }
    }

    #[test]
    fn user_schemas_keep_reading_sys_objects() {
        // A regular schema must never widen to the system catalog and pull in
        // ms-shipped noise.
        let sql = sql_for(&MetadataQuery::Tables {
            schema: Some("dbo".to_string()),
        });
        assert!(sql.contains("FROM sys.objects o"));
        assert!(!sql.contains("all_objects"));
        let cols = sql_for(&MetadataQuery::Columns {
            schema: "dbo".to_string(),
            object: "Orders".to_string(),
        });
        assert!(cols.contains("FROM sys.columns c"));
        assert!(!cols.contains("all_columns"));
    }

    #[test]
    fn role_and_guest_schemas_keep_reading_sys_objects() {
        // The explorer files `guest` and `db_*` role schemas under "System
        // Schemas" for *display*, but they hold no ms-shipped objects —
        // anything in them is user-created and already in sys.objects. The
        // enumeration source therefore must NOT widen for them.
        for schema in ["db_owner", "db_datareader", "DB_ACCESSADMIN", "guest"] {
            let views = sql_for(&MetadataQuery::Views {
                schema: Some(schema.to_string()),
            });
            assert!(
                views.contains("FROM sys.objects o") && !views.contains("all_objects"),
                "{schema} must keep sys.objects: {views}"
            );
            let cols = sql_for(&MetadataQuery::Columns {
                schema: schema.to_string(),
                object: "anything".to_string(),
            });
            assert!(
                cols.contains("FROM sys.columns c") && !cols.contains("all_columns"),
                "{schema} must keep sys.columns: {cols}"
            );
        }
    }

    #[test]
    fn schema_object_counts_include_system_objects() {
        // The count query must draw from the same population the schema
        // folders enumerate (sys.all_objects), or sys / INFORMATION_SCHEMA
        // count zero and get hidden by the "Only If Not Empty" placement.
        let sql = sql_for(&MetadataQuery::SchemaObjectCounts);
        assert!(sql.contains("LEFT JOIN sys.all_objects o"));
        assert!(!sql.contains("JOIN sys.objects"));
    }

    #[test]
    fn schema_query_has_no_use_statement() {
        // Azure SQL Database rejects USE; database context is connection-level.
        let sql = sql_for(&MetadataQuery::Schemas);
        assert!(!sql.contains("USE "));
        assert!(sql.contains("FROM sys.schemas"));
    }

    #[test]
    fn all_columns_query_covers_tables_and_views_in_ordinal_order() {
        let sql = sql_for(&MetadataQuery::AllColumns);
        assert!(sql.contains("FROM sys.columns c"));
        assert!(sql.contains("o.type IN (N'U', N'V')"));
        assert!(sql.contains("ORDER BY schema_name, object_name, c.column_id"));
    }

    #[test]
    fn all_primary_keys_query_selects_key_columns_in_ordinal_order() {
        let sql = sql_for(&MetadataQuery::AllPrimaryKeys);
        assert!(sql.contains("i.is_primary_key = 1"));
        assert!(sql.contains("sys.index_columns ic"));
        assert!(sql.contains("ic.key_ordinal"));
        assert!(sql.contains("ORDER BY schema_name, table_name, ic.key_ordinal"));
    }

    #[test]
    fn procedure_parameters_query_targets_sys_parameters() {
        let sql = sql_for(&MetadataQuery::ProcedureParameters {
            schema: "dbo".to_string(),
            object: "GetUser".to_string(),
        });
        assert!(sql.contains("FROM sys.parameters p"));
        assert!(sql.contains("p.is_output"));
        assert!(sql.contains("OBJECT_ID(N'dbo.GetUser')"));
        assert!(sql.contains("ORDER BY p.parameter_id"));
    }

    #[test]
    fn procedure_parameters_query_escapes_identifier() {
        let sql = sql_for(&MetadataQuery::ProcedureParameters {
            schema: "dbo".to_string(),
            object: "Weird'Proc".to_string(),
        });
        assert!(sql.contains("OBJECT_ID(N'dbo.Weird''Proc')"));
    }

    #[test]
    fn table_context_actions_include_select_and_scripts() {
        let node =
            ObjectNode::new(ObjectKind::Table, "Orders".to_string()).with_schema("dbo".to_string());
        let actions = context_actions(&node);
        assert!(actions.contains(&ObjectExplorerAction::SelectTopRows));
        assert!(actions.contains(&ObjectExplorerAction::Script(ScriptAction::Create)));
        assert!(actions.contains(&ObjectExplorerAction::Script(ScriptAction::Drop)));
        assert!(actions.contains(&ObjectExplorerAction::Refresh));
    }

    #[test]
    fn schema_child_nodes_cover_metadata_drilldowns() {
        let parent = ObjectNode::new(ObjectKind::Table, "Orders".to_string());
        let names = schema_child_nodes(&parent)
            .into_iter()
            .map(|node| node.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "Columns",
                "Keys",
                "Constraints",
                "Indexes",
                "Triggers",
                "Statistics"
            ]
        );
    }

    #[test]
    fn filter_nodes_searches_name_schema_and_kind() {
        let nodes = vec![
            ObjectNode::new(ObjectKind::Table, "Orders".to_string())
                .with_schema("sales".to_string()),
            ObjectNode::new(ObjectKind::View, "Inventory".to_string())
                .with_schema("dbo".to_string()),
        ];
        assert_eq!(filter_nodes(&nodes, "sales").len(), 1);
        assert_eq!(filter_nodes(&nodes, "view").len(), 1);
        assert_eq!(filter_nodes(&nodes, "").len(), 2);
    }

    #[test]
    fn capability_and_permission_queries_cover_server_probe() {
        let capability = sql_for(&MetadataQuery::ServerCapability);
        assert!(capability.contains("EngineEdition"));
        // Every SERVERPROPERTY must be CAST: a bare call returns `sql_variant`,
        // which the TDS driver cannot map, aborting the query mid-stream so the
        // General page loads zero rows. Regression guard for that empty page.
        assert!(capability.contains("CAST(SERVERPROPERTY('ProductVersion')"));
        assert_eq!(
            capability.matches("SERVERPROPERTY(").count(),
            capability.matches("CAST(SERVERPROPERTY(").count(),
            "every SERVERPROPERTY must be wrapped in CAST: {capability}"
        );

        // `sys.fn_my_permissions` exposes only entity_name/subentity_name/
        // permission_name — selecting `state_desc`/`class_desc` errors with
        // "Invalid column name". They are synthesized as literals instead.
        for query in [
            MetadataQuery::ServerPermissions,
            MetadataQuery::DatabasePermissions {
                database: "Sales".to_string(),
            },
            MetadataQuery::ObjectPermissions {
                schema: "dbo".to_string(),
                object: "Orders".to_string(),
            },
        ] {
            let sql = sql_for(&query);
            assert!(sql.contains("fn_my_permissions"), "{sql}");
            assert!(sql.contains("AS state_desc"), "{sql}");
            assert!(
                !sql.contains(", state_desc,") && !sql.contains(" state_desc FROM"),
                "bare state_desc column selected from fn_my_permissions: {sql}"
            );
        }
    }

    #[test]
    fn child_metadata_queries_escape_object_names() {
        let sql = sql_for(&MetadataQuery::Triggers {
            schema: "dbo".to_string(),
            table: "bad'name".to_string(),
        });
        assert!(sql.contains("dbo.bad''name"));
    }

    #[test]
    fn object_property_specs_cover_table_view_proc_database() {
        let table = object_properties_spec(ObjectKind::Table, Some("dbo"), "Orders");
        assert_eq!(table.qualified_name, "dbo.Orders");
        assert!(
            table
                .sections
                .iter()
                .any(|section| section.title == "Columns")
        );
        assert!(
            table
                .sections
                .iter()
                .any(|section| section.title == "Indexes")
        );
        let fk_section = table
            .sections
            .iter()
            .find(|section| section.title == "Foreign Keys")
            .expect("table properties should include a Foreign Keys section");
        // Both directions come from one query over sys.foreign_keys.
        assert!(fk_section.query.contains("References out"));
        assert!(fk_section.query.contains("Referenced by"));

        let view = object_properties_spec(ObjectKind::View, Some("sales"), "OpenOrders");
        assert!(
            view.sections
                .iter()
                .any(|section| section.title == "Definition")
        );

        let proc = object_properties_spec(ObjectKind::StoredProcedure, Some("dbo"), "RunReport");
        assert!(
            proc.sections
                .iter()
                .any(|section| section.title == "Parameters")
        );

        let db = object_properties_spec(ObjectKind::Database, None, "Sales");
        assert!(db.sections.iter().any(|section| section.title == "Files"));
    }

    #[test]
    fn azure_sql_profile_hides_unsupported_server_nodes() {
        let profile = ServerCapabilityProfile::azure_sql_database();
        let names = server_root_nodes_for_profile(&profile)
            .into_iter()
            .map(|node| node.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"Databases".to_string()));
        assert!(!names.contains(&"SQL Server Agent".to_string()));
        assert!(!names.contains(&"Server Objects".to_string()));
    }

    #[test]
    fn managed_instance_keeps_agent_node() {
        let profile = ServerCapabilityProfile::azure_sql_managed_instance();
        let names = server_root_nodes_for_profile(&profile)
            .into_iter()
            .map(|node| node.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"SQL Server Agent".to_string()));
    }

    #[test]
    fn azure_sql_database_nodes_include_cloud_resources() {
        let profile = ServerCapabilityProfile::azure_sql_database();
        let names = database_root_nodes_for_profile(&profile)
            .into_iter()
            .map(|node| node.name)
            .collect::<Vec<_>>();
        assert!(names.contains(&"Query Store".to_string()));
        assert!(names.contains(&"External Resources".to_string()));
        assert!(!names.contains(&"Service Broker".to_string()));
    }

    #[test]
    fn types_query_lists_user_defined_types_with_schema_filter() {
        let sql = sql_for(&MetadataQuery::Types {
            schema: Some("sales".to_string()),
        });
        assert!(sql.contains("FROM sys.types t"));
        assert!(sql.contains("t.is_user_defined = 1"));
        assert!(sql.contains("SCHEMA_NAME(t.schema_id) = N'sales'"));
        assert!(sql.contains("ORDER BY schema_name, t.name"));
    }

    #[test]
    fn types_query_without_schema_omits_filter() {
        let sql = sql_for(&MetadataQuery::Types { schema: None });
        assert!(sql.contains("t.is_user_defined = 1"));
        assert!(!sql.contains("SCHEMA_NAME(t.schema_id) ="));
    }

    #[test]
    fn type_detail_query_escapes_identifiers() {
        let sql = sql_for(&MetadataQuery::TypeDetail {
            schema: "dbo".to_string(),
            type_name: "Weird'Type".to_string(),
        });
        assert!(sql.contains("SCHEMA_NAME(t.schema_id) = N'dbo'"));
        assert!(sql.contains("t.name = N'Weird''Type'"));
        assert!(sql.contains("t.is_user_defined = 1"));
    }

    #[test]
    fn table_general_query_includes_row_count_estimate() {
        let sql = sql_for(&MetadataQuery::TableGeneral {
            schema: "dbo".to_string(),
            table: "Orders".to_string(),
        });
        assert!(sql.contains("FROM sys.objects o"));
        assert!(sql.contains("LEFT JOIN sys.partitions p"));
        assert!(sql.contains("SUM(p.rows) AS row_count"));
        assert!(sql.contains("OBJECT_ID(N'dbo.Orders')"));
    }

    #[test]
    fn object_general_query_reads_sys_objects() {
        let sql = sql_for(&MetadataQuery::ObjectGeneral {
            schema: "sales".to_string(),
            object: "OpenOrders".to_string(),
        });
        assert!(sql.contains("FROM sys.objects"));
        assert!(sql.contains("type_desc"));
        assert!(sql.contains("OBJECT_ID(N'sales.OpenOrders')"));
    }

    #[test]
    fn dependent_views_pg_sql_uses_pg_rewrite() {
        let sql = sql_for_engine(
            &MetadataQuery::DependentViews {
                schema: "public".to_string(),
                table: "orders".to_string(),
            },
            DatabaseType::Postgres,
        )
        .expect("sql");
        assert!(sql.contains("pg_rewrite"));
        assert!(sql.contains("to_regclass('public.orders')"));
    }

    #[test]
    fn dependent_views_sql_server_escapes_hostile_names() {
        let sql = sql_for(&MetadataQuery::DependentViews {
            schema: "dbo".to_string(),
            table: "O'Brien".to_string(),
        });
        assert!(sql.contains("sql_expression_dependencies"));
        assert!(sql.contains("O''Brien"));
    }

    #[test]
    fn indexes_query_includes_fill_factor_column() {
        let sql = sql_for(&MetadataQuery::Indexes {
            schema: "dbo".to_string(),
            table: "People".to_string(),
        });
        assert!(sql.contains("fill_factor"));
    }

    #[test]
    fn index_general_query_includes_disabled_and_fill_factor() {
        let sql = sql_for(&MetadataQuery::IndexGeneral {
            schema: "dbo".to_string(),
            table: "Orders".to_string(),
            index: "IX_Orders_Date".to_string(),
        });
        assert!(sql.contains("FROM sys.indexes i"));
        assert!(sql.contains("i.is_disabled"));
        assert!(sql.contains("i.fill_factor"));
        assert!(sql.contains("OBJECT_ID(N'dbo.Orders')"));
        assert!(sql.contains("i.name = N'IX_Orders_Date'"));
    }

    #[test]
    fn index_columns_query_splits_keys_and_includes_in_order() {
        let sql = sql_for(&MetadataQuery::IndexColumns {
            schema: "dbo".to_string(),
            table: "Orders".to_string(),
            index: "IX_Orders_Date".to_string(),
        });
        assert!(sql.contains("FROM sys.indexes i"));
        assert!(sql.contains("JOIN sys.index_columns ic"));
        assert!(sql.contains("ic.is_included_column"));
        assert!(sql.contains("ic.is_descending_key"));
        assert!(sql.contains("ORDER BY ic.is_included_column, ic.key_ordinal, ic.index_column_id"));
    }

    #[test]
    fn statistic_general_query_includes_no_recompute_and_updated_date() {
        let sql = sql_for(&MetadataQuery::StatisticGeneral {
            schema: "dbo".to_string(),
            table: "Orders".to_string(),
            statistic: "_WA_Sys_00000001".to_string(),
        });
        assert!(sql.contains("FROM sys.stats s"));
        assert!(sql.contains("s.no_recompute"));
        assert!(sql.contains("STATS_DATE(s.object_id, s.stats_id)"));
        assert!(sql.contains("s.name = N'_WA_Sys_00000001'"));
    }

    #[test]
    fn statistic_columns_query_orders_by_stats_column_ordinal() {
        let sql = sql_for(&MetadataQuery::StatisticColumns {
            schema: "dbo".to_string(),
            table: "Orders".to_string(),
            statistic: "Orders_Status".to_string(),
        });
        assert!(sql.contains("JOIN sys.stats_columns sc"));
        assert!(sql.contains("ORDER BY sc.stats_column_id"));
        assert!(sql.contains("OBJECT_ID(N'dbo.Orders')"));
        assert!(sql.contains("s.name = N'Orders_Status'"));
    }

    #[test]
    fn object_definition_and_permissions_queries_escape_object_names() {
        let def = sql_for(&MetadataQuery::ObjectDefinition {
            schema: "dbo".to_string(),
            object: "bad'name".to_string(),
        });
        assert!(def.contains("OBJECT_DEFINITION(OBJECT_ID(N'dbo.bad''name'))"));
        let perms = sql_for(&MetadataQuery::ObjectPermissions {
            schema: "dbo".to_string(),
            object: "bad'name".to_string(),
        });
        assert!(perms.contains("sys.fn_my_permissions(N'dbo.bad''name', 'OBJECT')"));
    }

    #[test]
    fn sql_server_sequences_query_reads_sys_sequences() {
        let sql = sql_for(&MetadataQuery::Sequences {
            schema: Some("sales".to_string()),
        });
        assert!(sql.contains("FROM sys.sequences s"));
        assert!(sql.contains("start_value"));
        assert!(sql.contains("increment"));
        assert!(sql.contains("current_value"));
        assert!(sql.contains("SCHEMA_NAME(s.schema_id) = N'sales'"));
    }

    #[test]
    fn literals_are_escaped_in_new_queries() {
        let trigger_def = sql_for(&MetadataQuery::TriggerDefinition {
            schema: "dbo".to_string(),
            name: "Weird'Trig".to_string(),
        });
        assert!(trigger_def.contains("OBJECT_ID(N'dbo.Weird''Trig')"));

        let constraint_def = sql_for(&MetadataQuery::ConstraintDefinition {
            schema: "dbo".to_string(),
            table: "bad'table".to_string(),
            name: "chk'name".to_string(),
        });
        assert!(constraint_def.contains("o.name = N'bad''table'"));
        assert!(constraint_def.contains("cc.name = N'chk''name'"));
    }

    #[test]
    fn object_inventory_sql_server_has_eight_columns_in_order() {
        let sql = sql_for(&MetadataQuery::ObjectInventory { schema: None });
        for needle in [
            " AS kind",
            " AS schema_name",
            " AS name",
            " AS row_estimate",
            " AS size_kb",
            "create_date",
            "modify_date",
            " AS comment",
        ] {
            assert!(sql.contains(needle), "missing {needle} in {sql}");
        }
        let kind = sql.find(" AS kind").expect("kind");
        let schema = sql.find(" AS schema_name").expect("schema_name");
        let name = sql.find(" AS name").expect("name");
        let rows = sql.find(" AS row_estimate").expect("row_estimate");
        let size = sql.find(" AS size_kb").expect("size_kb");
        let created = sql.find("create_date").expect("create_date");
        let modified = sql.find("modify_date").expect("modify_date");
        let comment = sql.find(" AS comment").expect("comment");
        assert!(kind < schema && schema < name && name < rows && rows < size);
        assert!(size < created && created < modified && modified < comment);
        assert!(sql.contains("FROM sys.objects o"));
        assert!(sql.contains("JOIN sys.schemas s"));
        assert!(sql.contains("o.is_ms_shipped = 0"));
        assert!(sql.contains("THEN 'TABLE'"));
        assert!(sql.contains("THEN 'VIEW'"));
        assert!(sql.contains("THEN 'PROCEDURE'"));
        assert!(sql.contains("THEN 'FUNCTION'"));
        assert!(sql.contains("THEN 'TRIGGER'"));
        assert!(sql.contains("sys.partitions"));
        assert!(sql.contains("sys.allocation_units"));
        assert!(sql.contains("MS_Description"));
        assert!(sql.contains("ORDER BY kind, schema_name, name"));
        assert!(!sql.contains("COUNT(*)"));
    }

    #[test]
    fn object_inventory_postgres_unions_relations_routines_triggers() {
        let sql = sql_for_dialect(
            &MetadataQuery::ObjectInventory { schema: None },
            MetadataDialect::Postgres,
        )
        .unwrap();
        assert!(sql.contains("UNION ALL"), "{sql}");
        assert!(sql.contains("pg_class"), "{sql}");
        assert!(sql.contains("pg_proc"), "{sql}");
        assert!(sql.contains("pg_trigger"), "{sql}");
        assert!(sql.contains("relkind IN ('r','p','v','m')"), "{sql}");
        assert!(sql.contains("GREATEST(c.reltuples, 0)::bigint"), "{sql}");
        assert!(sql.contains("pg_total_relation_size"), "{sql}");
        assert!(sql.contains("NOT t.tgisinternal"), "{sql}");
        assert!(sql.contains("MATERIALIZED VIEW"), "{sql}");
        assert!(sql.contains("obj_description"), "{sql}");
        assert!(sql.contains("pg_catalog"), "{sql}");
        assert!(sql.contains("information_schema"), "{sql}");
        assert!(sql.contains("ORDER BY kind, schema_name, name"), "{sql}");
    }

    #[test]
    fn object_inventory_mysql_uses_information_schema_sizes() {
        let sql = sql_for_dialect(
            &MetadataQuery::ObjectInventory { schema: None },
            MetadataDialect::MySql,
        )
        .unwrap();
        assert!(
            sql.contains("information_schema.TABLES") || sql.contains("information_schema.tables"),
            "{sql}"
        );
        assert!(
            sql.contains("information_schema.ROUTINES")
                || sql.contains("information_schema.routines"),
            "{sql}"
        );
        assert!(
            sql.contains("information_schema.TRIGGERS")
                || sql.contains("information_schema.triggers"),
            "{sql}"
        );
        assert!(
            sql.contains("DATA_LENGTH") || sql.contains("data_length"),
            "{sql}"
        );
        assert!(
            sql.contains("INDEX_LENGTH") || sql.contains("index_length"),
            "{sql}"
        );
        assert!(
            sql.contains("DIV 1024") || sql.contains("div 1024"),
            "{sql}"
        );
        assert!(sql.contains("DATABASE()"), "{sql}");
        assert!(sql.contains("UNION ALL"), "{sql}");
        assert!(sql.contains("ORDER BY kind, schema_name, name"), "{sql}");
    }

    #[test]
    fn object_inventory_sqlite_runs_against_rusqlite() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);
             CREATE VIEW active_users AS SELECT id, name FROM users;
             CREATE TRIGGER trg_users AFTER INSERT ON users BEGIN SELECT 1; END;",
        )
        .unwrap();
        let sql = sql_for_dialect(
            &MetadataQuery::ObjectInventory { schema: None },
            MetadataDialect::Sqlite,
        )
        .unwrap();
        assert!(!sql.contains("COUNT(*)"), "{sql}");
        let mut stmt = conn.prepare(&sql).unwrap();
        let mut kinds = Vec::new();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            assert_eq!(row.as_ref().column_count(), 8);
            kinds.push(row.get::<_, String>(0).unwrap());
        }
        kinds.sort();
        assert_eq!(kinds, vec!["TABLE", "TRIGGER", "VIEW"]);
    }

    #[test]
    fn object_inventory_schema_filter_escapes_quotes() {
        let query = MetadataQuery::ObjectInventory {
            schema: Some("o'neil".to_string()),
        };
        let tds = sql_for(&query);
        assert!(tds.contains("o''neil"), "{tds}");
        assert!(!tds.contains("o'neil"), "{tds}");
        let pg = sql_for_dialect(&query, MetadataDialect::Postgres).unwrap();
        assert!(pg.contains("o''neil"), "{pg}");
        let my = sql_for_dialect(&query, MetadataDialect::MySql).unwrap();
        assert!(my.contains("o''neil"), "{my}");
    }
}
