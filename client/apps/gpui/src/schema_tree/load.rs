use sqlly_core::{ConnectionProfile, ObjectKind};
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType, DbFeature};
use sqlly_services::{ForeignKeyRow, ProcedureParameter, Services};
use uuid::Uuid;

use super::model::{ConstraintKind, NodeKind, TreeNode, find_node_any};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadState {
    Unloaded,
    Loading,
    Loaded,
    /// The load genuinely failed — a dropped connection, a timeout, a malformed
    /// response. Something is wrong and the user may want to retry.
    Failed(String),
    /// The server understood the request and refused it for lack of permission.
    ///
    /// Distinct from [`LoadState::Failed`] on purpose: "the SELECT permission
    /// was denied on msdb" is not a malfunction, it is the normal and correct
    /// condition of a locked-down production box. Painting it in red error text
    /// next to real failures is how error blindness starts — the user learns
    /// that red in this tree means nothing, and then misses the red that
    /// mattered.
    PermissionDenied(String),
}

/// Sort a load error into [`LoadState::PermissionDenied`] or
/// [`LoadState::Failed`] by what SQL Server actually said.
///
/// Matching on message text is unlovely, but the engine surfaces these as plain
/// strings by the time they reach the tree, and the distinction is worth more to
/// the user than the purity is to us.
pub(crate) fn classify_load_error(error: String) -> LoadState {
    let lower = error.to_lowercase();
    // Msg 229 (permission denied on object), Msg 262 (permission denied in
    // database), Msg 916 (principal cannot access the database under the
    // current security context), Msg 4060 (cannot open database).
    let denied = [
        "permission was denied",
        "permission denied",
        "is not able to access the database",
        "cannot open database",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    if denied {
        LoadState::PermissionDenied(error)
    } else {
        LoadState::Failed(error)
    }
}
/// Databases SQL Server ships with, matched for the system-database placement
/// preference on any engine (harmless on those that have no such database).
const SYSTEM_DATABASES: [&str; 4] = ["master", "tempdb", "model", "msdb"];

/// The databases a fresh PostgreSQL cluster is born with: the two template
/// databases and the `postgres` maintenance database that `initdb` creates.
/// The direct analog of SQL Server's shipped [`SYSTEM_DATABASES`].
const PG_SYSTEM_DATABASES: [&str; 3] = ["template0", "template1", "postgres"];

/// Schemas SQL Server ships with, for the system-schema placement preference.
/// `db_*` role schemas are matched by prefix in [`is_system_schema`].
const SYSTEM_SCHEMAS: [&str; 3] = ["sys", "INFORMATION_SCHEMA", "guest"];

/// Whether `name` is a database the engine ships with, for the System Databases
/// placement preference. The SQL Server names are matched on every engine (they
/// don't collide with real user databases elsewhere); PostgreSQL adds its
/// template + maintenance databases.
pub(crate) fn is_system_database(name: &str, db_type: DatabaseType) -> bool {
    if SYSTEM_DATABASES
        .iter()
        .any(|s| s.eq_ignore_ascii_case(name))
    {
        return true;
    }
    if db_type.connection_kind() == ConnectionKind::PostgresWire {
        return PG_SYSTEM_DATABASES
            .iter()
            .any(|s| s.eq_ignore_ascii_case(name));
    }
    false
}

/// Whether `name` is a schema the engine ships with, for the System Schemas
/// placement preference. SQL Server: `sys` / `INFORMATION_SCHEMA` / `guest` and
/// the `db_*` fixed-role schemas. PostgreSQL: `information_schema` (matched by
/// the shared list) plus every `pg_`-prefixed internal schema (`pg_catalog`,
/// `pg_toast`, `pg_temp_*`) and CockroachDB's `crdb_internal`.
pub(crate) fn is_system_schema(name: &str, db_type: DatabaseType) -> bool {
    if SYSTEM_SCHEMAS.iter().any(|s| s.eq_ignore_ascii_case(name))
        || name.to_ascii_lowercase().starts_with("db_")
    {
        return true;
    }
    if db_type.connection_kind() == ConnectionKind::PostgresWire {
        let lower = name.to_ascii_lowercase();
        return lower.starts_with("pg_") || lower == "crdb_internal";
    }
    false
}

/// The Tables/Views/Programmability folder trio under a database (or under a
/// schema folder when the schema-folders display mode is active).
pub(crate) fn object_type_folders(node_id: &str, child_depth: usize) -> Vec<TreeNode> {
    vec![
        TreeNode::new(
            format!("{node_id}:tables"),
            crate::localization::text("explorer.schema_tree.tables_folder"),
            NodeKind::TablesFolder,
            child_depth,
        ),
        TreeNode::new(
            format!("{node_id}:views"),
            crate::localization::text("explorer.schema_tree.views_folder"),
            NodeKind::ViewsFolder,
            child_depth,
        ),
        TreeNode::new(
            format!("{node_id}:prog"),
            crate::localization::text("explorer.schema_tree.programmability_folder"),
            NodeKind::ProgrammabilityFolder,
            child_depth,
        ),
    ]
}

/// Engine-specific folders at the database (or schema) level beyond the
/// Tables/Views/Programmability trio.
pub(crate) fn database_extra_folders(
    node_id: &str,
    child_depth: usize,
    db: DatabaseType,
) -> Vec<TreeNode> {
    let mut folders = Vec::new();
    if db.supports_feature(DbFeature::MaterializedViews) && db != DatabaseType::CockroachDb {
        folders.push(TreeNode::new(
            format!("{node_id}:matviews"),
            crate::localization::text("explorer.schema_tree.materialized_views_folder"),
            NodeKind::MaterializedViewsFolder,
            child_depth,
        ));
    }
    if db.supports_feature(DbFeature::Sequences) {
        folders.push(TreeNode::new(
            format!("{node_id}:sequences"),
            crate::localization::text("explorer.schema_tree.sequences_folder"),
            NodeKind::SequencesFolder,
            child_depth,
        ));
    }
    if matches!(db, DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase) {
        folders.push(TreeNode::new(
            format!("{node_id}:synonyms"),
            crate::localization::text("explorer.schema_tree.synonyms_folder"),
            NodeKind::SynonymsFolder,
            child_depth,
        ));
    }
    if db.connection_kind() == ConnectionKind::PostgresWire && db != DatabaseType::CockroachDb {
        folders.push(TreeNode::new(
            format!("{node_id}:extensions"),
            crate::localization::text("explorer.schema_tree.extensions_folder"),
            NodeKind::ExtensionsFolder,
            child_depth,
        ));
    }
    if matches!(db, DatabaseType::MySql | DatabaseType::MariaDb) {
        folders.push(TreeNode::new(
            format!("{node_id}:events"),
            crate::localization::text("explorer.schema_tree.events_folder"),
            NodeKind::EventsFolder,
            child_depth,
        ));
    }
    if db.supports_feature(DbFeature::Triggers) {
        folders.push(TreeNode::new(
            format!("{node_id}:dbtriggers"),
            crate::localization::text("explorer.schema_tree.database_triggers_folder"),
            NodeKind::DatabaseTriggersFolder,
            child_depth,
        ));
    }
    folders
}

fn object_folders_for_db(node_id: &str, child_depth: usize, db: DatabaseType) -> Vec<TreeNode> {
    let mut folders = object_type_folders(node_id, child_depth);
    folders.extend(database_extra_folders(node_id, child_depth, db));
    folders
}

/// A per-(connection, database) cache of the database-wide foreign-key listing.
/// Expanding a table's Foreign Keys folder issues a whole-database FK query;
/// without this, expanding N tables meant N identical full-DB queries. Cloned
/// into each lazy-load task (cheap `Rc`), and cleared on any tree refresh /
/// connection change so a schema edit is never served from a stale listing.
/// (connection id, database) → the database's foreign-key listing.
pub(crate) type FkCacheMap =
    std::collections::HashMap<(Uuid, String), std::rc::Rc<Vec<ForeignKeyRow>>>;

#[derive(Clone, Default)]
pub(crate) struct ForeignKeyCache {
    inner: std::rc::Rc<std::cell::RefCell<FkCacheMap>>,
}

impl ForeignKeyCache {
    pub(crate) async fn foreign_keys(
        &self,
        services: &Services,
        profile: &ConnectionProfile,
        database: &str,
    ) -> sqlly_core::StudioResult<std::rc::Rc<Vec<ForeignKeyRow>>> {
        let key = (profile.id.0, database.to_string());
        // Borrow only to read/clone the Rc; never hold it across the await.
        if let Some(hit) = self.inner.borrow().get(&key).cloned() {
            return Ok(hit);
        }
        let rows = std::rc::Rc::new(
            services
                .metadata
                .list_foreign_keys(profile, database)
                .await?,
        );
        self.inner
            .borrow_mut()
            .insert(key, std::rc::Rc::clone(&rows));
        Ok(rows)
    }

    pub(crate) fn clear(&self) {
        self.inner.borrow_mut().clear();
    }
}

/// The schema a node *is* or belongs to as a database-level listing entry
/// (a schema folder, or a table/view/routine/… row in an object folder).
/// `None` for folders, columns and every other kind that is not itself a
/// schema-owned object.
fn listed_schema(kind: &NodeKind) -> Option<&str> {
    match kind {
        NodeKind::SchemaFolder { schema }
        | NodeKind::Table { schema, .. }
        | NodeKind::View { schema, .. }
        | NodeKind::Procedure { schema, .. }
        | NodeKind::Function { schema, .. }
        | NodeKind::MaterializedView { schema, .. }
        | NodeKind::Type { schema, .. }
        | NodeKind::Sequence { schema, .. }
        | NodeKind::Synonym { schema, .. }
        | NodeKind::Event { schema, .. }
        | NodeKind::Trigger { schema, .. } => Some(schema),
        _ => None,
    }
}

/// Folders whose children are schemas or schema-owned objects, so pruning
/// descends into them (preloaded grouping folders and hydrated object
/// folders).
fn lists_schema_entries(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::SystemSchemasFolder
            | NodeKind::EmptySchemasFolder
            | NodeKind::TablesFolder
            | NodeKind::ViewsFolder
            | NodeKind::ProgrammabilityFolder
            | NodeKind::TypesFolder
            | NodeKind::MaterializedViewsFolder
            | NodeKind::SequencesFolder
            | NodeKind::SynonymsFolder
            | NodeKind::EventsFolder
            | NodeKind::DatabaseTriggersFolder
    )
}

/// Remove the schemas a connection hides from the explorer: their schema
/// folders (wherever the placement rules put them) and, in the flat
/// `schema.name` mode, their objects in the object folders. A System or
/// Empty Schemas folder left with nothing in it is dropped too. Only the
/// tree is affected; nothing else reads this.
pub(crate) fn prune_hidden_schemas(
    nodes: &mut Vec<TreeNode>,
    hidden: &std::collections::BTreeSet<String>,
) {
    if hidden.is_empty() {
        return;
    }
    nodes.retain_mut(|node| {
        if listed_schema(&node.kind).is_some_and(|schema| hidden.contains(schema)) {
            return false;
        }
        if lists_schema_entries(&node.kind) {
            prune_hidden_schemas(&mut node.children, hidden);
            if matches!(
                node.kind,
                NodeKind::SystemSchemasFolder | NodeKind::EmptySchemasFolder
            ) && node.children.is_empty()
            {
                return false;
            }
        }
        true
    })
}

/// Whether a load of `kind` lists schemas or schema-owned objects that the
/// connection's hidden schemas apply to.
fn level_lists_schemas(kind: &NodeKind) -> bool {
    matches!(kind, NodeKind::Database(_)) || lists_schema_entries(kind)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn load_children_async(
    services: &Services,
    profile: &ConnectionProfile,
    kind: &NodeKind,
    node_id: &str,
    parent_depth: usize,
    prefs: crate::preferences::DataModelPreferences,
    fk_cache: &ForeignKeyCache,
    redis_filter: Option<&str>,
) -> Result<Vec<TreeNode>, String> {
    let mut children = load_children_unfiltered(
        services,
        profile,
        kind,
        node_id,
        parent_depth,
        prefs,
        fk_cache,
        redis_filter,
    )
    .await?;
    if level_lists_schemas(kind) {
        let database = match kind {
            NodeKind::Database(name) => name.clone(),
            _ => extract_db_from_id(node_id),
        };
        if let Some(hidden) = profile.hidden_schemas_in(&database) {
            prune_hidden_schemas(&mut children, hidden);
        }
    }
    Ok(children)
}

#[allow(clippy::too_many_arguments)]
async fn load_children_unfiltered(
    services: &Services,
    profile: &ConnectionProfile,
    kind: &NodeKind,
    node_id: &str,
    parent_depth: usize,
    prefs: crate::preferences::DataModelPreferences,
    fk_cache: &ForeignKeyCache,
    redis_filter: Option<&str>,
) -> Result<Vec<TreeNode>, String> {
    use crate::preferences::{SchemaDisplayMode, SystemDatabasePlacement};
    let child_depth = parent_depth + 1;
    let db_type = profile.database_type;
    // Redis has no catalog: its tree is the key browser, built from the
    // driver's SCAN walk rather than the metadata service.
    if db_type.is_command_engine() {
        return super::redis::load_redis_children(
            services,
            profile,
            kind,
            node_id,
            child_depth,
            redis_filter,
        )
        .await;
    }
    match kind {
        NodeKind::Server { .. } => {
            // Connect on demand and list databases as this server's children.
            let dbs = services
                .connections
                .list_databases(profile)
                .await
                .map_err(|e| e.to_string())?;
            let make_db = |name: String, depth: usize| {
                TreeNode::new(
                    format!("{node_id}:db:{}", escape_id_segment(&name)),
                    name.clone(),
                    NodeKind::Database(name),
                    depth,
                )
            };
            let is_system = |name: &str| is_system_database(name, db_type);
            match prefs.system_databases {
                SystemDatabasePlacement::Inline => Ok(dbs
                    .into_iter()
                    .map(|db| make_db(db.name, child_depth))
                    .collect()),
                SystemDatabasePlacement::Hidden => Ok(dbs
                    .into_iter()
                    .filter(|db| !is_system(&db.name))
                    .map(|db| make_db(db.name, child_depth))
                    .collect()),
                SystemDatabasePlacement::Folder => {
                    let (system, user): (Vec<_>, Vec<_>) =
                        dbs.into_iter().partition(|db| is_system(&db.name));
                    let mut children: Vec<TreeNode> = user
                        .into_iter()
                        .map(|db| make_db(db.name, child_depth))
                        .collect();
                    if !system.is_empty() {
                        let mut folder = TreeNode::new(
                            format!("{node_id}:sysdbs"),
                            crate::localization::text(
                                "explorer.schema_tree.system_databases_folder",
                            ),
                            NodeKind::SystemDatabasesFolder,
                            child_depth,
                        );
                        folder.load_state = LoadState::Loaded;
                        folder.children = system
                            .into_iter()
                            .map(|db| make_db(db.name, child_depth + 1))
                            .collect();
                        children.push(folder);
                    }
                    Ok(children)
                }
            }
        }
        // Children are preloaded when the folder is built; nothing to fetch.
        NodeKind::SystemDatabasesFolder
        | NodeKind::SystemSchemasFolder
        | NodeKind::EmptySchemasFolder => Ok(Vec::new()),
        NodeKind::Database(db_name) => match prefs.schema_display {
            SchemaDisplayMode::FlatPrefixed => {
                Ok(object_folders_for_db(node_id, child_depth, db_type))
            }
            SchemaDisplayMode::SchemaFolders => {
                use crate::preferences::{GeneralSchemaPlacement, SystemSchemaPlacement};
                let mut schemas = services
                    .metadata
                    .list_schemas_with_counts(profile, db_name)
                    .await
                    .map_err(|e| e.to_string())?;
                schemas.sort_by(|a, b| cmp_object_names(("", &a.0), ("", &b.0)));
                // Unknown counts (backend without the counts query) are
                // treated as non-empty so nothing silently disappears.
                let is_empty = |count: &Option<u64>| *count == Some(0);
                let make_schema = |schema: String, depth: usize| {
                    TreeNode::new(
                        format!("{node_id}:sch:{}", escape_id_segment(&schema)),
                        schema.clone(),
                        NodeKind::SchemaFolder { schema },
                        depth,
                    )
                };
                let make_group = |suffix: &str, title: &'static str, kind: NodeKind| {
                    TreeNode::new(
                        format!("{node_id}:{suffix}"),
                        crate::localization::text(title),
                        kind,
                        child_depth,
                    )
                };

                let (system, general): (Vec<_>, Vec<_>) = schemas
                    .into_iter()
                    .partition(|(name, _)| is_system_schema(name, db_type));

                let mut children: Vec<TreeNode> = Vec::new();
                let mut empty_general: Vec<TreeNode> = Vec::new();
                for (name, count) in general {
                    match prefs.general_schemas {
                        GeneralSchemaPlacement::Show => {
                            children.push(make_schema(name, child_depth));
                        }
                        GeneralSchemaPlacement::ShowIfNotEmpty => {
                            if !is_empty(&count) {
                                children.push(make_schema(name, child_depth));
                            }
                        }
                        GeneralSchemaPlacement::ShowWithEmptyFolder => {
                            if is_empty(&count) {
                                empty_general.push(make_schema(name, child_depth + 1));
                            } else {
                                children.push(make_schema(name, child_depth));
                            }
                        }
                    }
                }
                if !empty_general.is_empty() {
                    let mut folder = make_group(
                        "emptyschemas",
                        "explorer.schema_tree.empty_schemas_folder",
                        NodeKind::EmptySchemasFolder,
                    );
                    folder.load_state = LoadState::Loaded;
                    folder.children = empty_general;
                    children.push(folder);
                }

                match prefs.system_schemas {
                    SystemSchemaPlacement::Hidden => {}
                    SystemSchemaPlacement::OnlyIfNotEmpty => {
                        for (name, count) in system {
                            if !is_empty(&count) {
                                children.push(make_schema(name, child_depth));
                            }
                        }
                    }
                    SystemSchemaPlacement::Folder => {
                        let system_children: Vec<TreeNode> = system
                            .into_iter()
                            .map(|(name, _)| make_schema(name, child_depth + 1))
                            .collect();
                        if !system_children.is_empty() {
                            let mut folder = make_group(
                                "sysschemas",
                                "explorer.schema_tree.system_schemas_folder",
                                NodeKind::SystemSchemasFolder,
                            );
                            folder.load_state = LoadState::Loaded;
                            folder.children = system_children;
                            children.push(folder);
                        }
                    }
                }
                Ok(children)
            }
        },
        NodeKind::SchemaFolder { .. } => Ok(object_folders_for_db(node_id, child_depth, db_type)),
        NodeKind::TablesFolder => {
            let db = extract_db_from_id(node_id);
            let schema_scope = extract_schema_from_id(node_id);
            let mut tables = services
                .metadata
                .list_tables(profile, &db, schema_scope.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            sort_listed_objects(&mut tables);
            Ok(tables
                .into_iter()
                .map(|t| {
                    let schema = t.schema.clone().unwrap_or_default();
                    let name = t.name;
                    TreeNode::new(
                        format!(
                            "{node_id}:t:{}:{}",
                            escape_id_segment(&schema),
                            escape_id_segment(&name)
                        ),
                        object_title(&schema, &name, schema_scope.is_some()),
                        NodeKind::Table { schema, name },
                        child_depth,
                    )
                })
                .collect())
        }
        NodeKind::ViewsFolder => {
            let db = extract_db_from_id(node_id);
            let schema_scope = extract_schema_from_id(node_id);
            let mut views = services
                .metadata
                .list_views(profile, &db, schema_scope.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            sort_listed_objects(&mut views);
            Ok(views
                .into_iter()
                .map(|v| {
                    let schema = v.schema.clone().unwrap_or_default();
                    let name = v.name;
                    TreeNode::new(
                        format!(
                            "{node_id}:v:{}:{}",
                            escape_id_segment(&schema),
                            escape_id_segment(&name)
                        ),
                        object_title(&schema, &name, schema_scope.is_some()),
                        NodeKind::View { schema, name },
                        child_depth,
                    )
                })
                .collect())
        }
        NodeKind::ProgrammabilityFolder => {
            let db = extract_db_from_id(node_id);
            let schema_scope = extract_schema_from_id(node_id);
            let mut procs = services
                .metadata
                .list_programmable_objects(profile, &db, schema_scope.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            sort_listed_objects(&mut procs);
            // User-defined types sit in their own lazy folder, ahead of the
            // procedure/function rows like SSMS's Programmability layout.
            let mut children = vec![TreeNode::new(
                format!("{node_id}:types"),
                crate::localization::text("explorer.schema_tree.types_folder"),
                NodeKind::TypesFolder,
                child_depth,
            )];
            children.extend(procs.into_iter().map(|p| {
                let schema = p.schema.clone().unwrap_or_default();
                let name = p.name;
                let kind = match p.kind {
                    ObjectKind::ScalarFunction | ObjectKind::TableFunction => NodeKind::Function {
                        schema: schema.clone(),
                        name: name.clone(),
                    },
                    _ => NodeKind::Procedure {
                        schema: schema.clone(),
                        name: name.clone(),
                    },
                };
                let prefix = match &kind {
                    NodeKind::Function { .. } => "f",
                    _ => "p",
                };
                TreeNode::new(
                    format!(
                        "{node_id}:{prefix}:{}:{}",
                        escape_id_segment(&schema),
                        escape_id_segment(&name)
                    ),
                    object_title(&schema, &name, schema_scope.is_some()),
                    kind,
                    child_depth,
                )
            }));
            Ok(children)
        }
        NodeKind::IndexesFolder { schema, table } => {
            let db = extract_db_from_id(node_id);
            let indexes = services
                .metadata
                .list_indexes(profile, &db, schema, table)
                .await
                .map_err(|e| e.to_string())?;
            Ok(indexes
                .into_iter()
                .map(|i| {
                    let mut n = TreeNode::new(
                        format!("{node_id}:ix:{}", escape_id_segment(&i.name)),
                        i.name.clone(),
                        NodeKind::Index {
                            schema: schema.clone(),
                            table: table.clone(),
                            name: i.name,
                        },
                        child_depth,
                    );
                    n.detail = i.detail;
                    n
                })
                .collect())
        }
        NodeKind::StatisticsFolder { schema, table } => {
            let db = extract_db_from_id(node_id);
            let stats = services
                .metadata
                .list_statistics(profile, &db, schema, table)
                .await
                .map_err(|e| e.to_string())?;
            Ok(stats
                .into_iter()
                .map(|s| {
                    let mut n = TreeNode::new(
                        format!("{node_id}:st:{}", escape_id_segment(&s.name)),
                        s.name.clone(),
                        NodeKind::Statistic {
                            schema: schema.clone(),
                            table: table.clone(),
                            name: s.name,
                        },
                        child_depth,
                    );
                    n.detail = s.detail;
                    n
                })
                .collect())
        }
        NodeKind::ForeignKeysFolder { schema, table } => {
            let db = extract_db_from_id(node_id);
            // One DB-wide fetch gives both directions: keys this table declares
            // (outgoing) and keys other tables aim back at it (incoming).
            let rows = fk_cache
                .foreign_keys(services, profile, &db)
                .await
                .map_err(|e| e.to_string())?;
            let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);

            // One relationship per FK constraint, not per column pair: group the
            // column rows (which arrive in `constraint_column_id` order) so a
            // composite key renders `ColA, ColB → RefA, RefB` on a single node.
            pub(crate) struct FkGroup {
                outgoing: bool,
                // The constraint's own schema + name. A FK name is unique only
                // within its owning (child-table) schema, so both are needed to
                // key it — otherwise a `dbo.FK_X` and a `sales.FK_X` touching the
                // same table would merge into one wrong node.
                owner_schema: String,
                constraint_name: String,
                other_schema: String,
                other_table: String,
                // (this-table column, other-table column), in key order.
                pairs: Vec<(String, String)>,
            }
            let mut order: Vec<(String, String)> = Vec::new();
            let mut groups: std::collections::HashMap<(String, String), FkGroup> =
                std::collections::HashMap::new();
            for fk in rows.iter() {
                let outgoing = eq(&fk.from_schema, schema) && eq(&fk.from_table, table);
                let incoming = eq(&fk.to_schema, schema) && eq(&fk.to_table, table);
                if !outgoing && !incoming {
                    continue;
                }
                // The constraint is owned by its child (referencing) table, whose
                // schema is `from_schema` regardless of direction.
                let key = (fk.from_schema.clone(), fk.constraint_name.clone());
                // A self-referencing key (both sides this table) is shown once,
                // as outgoing.
                let group = groups.entry(key.clone()).or_insert_with(|| {
                    order.push(key.clone());
                    if outgoing {
                        FkGroup {
                            outgoing: true,
                            owner_schema: fk.from_schema.clone(),
                            constraint_name: fk.constraint_name.clone(),
                            other_schema: fk.to_schema.clone(),
                            other_table: fk.to_table.clone(),
                            pairs: Vec::new(),
                        }
                    } else {
                        FkGroup {
                            outgoing: false,
                            owner_schema: fk.from_schema.clone(),
                            constraint_name: fk.constraint_name.clone(),
                            other_schema: fk.from_schema.clone(),
                            other_table: fk.from_table.clone(),
                            pairs: Vec::new(),
                        }
                    }
                });
                if group.outgoing {
                    group
                        .pairs
                        .push((fk.from_column.clone(), fk.to_column.clone()));
                } else {
                    group
                        .pairs
                        .push((fk.to_column.clone(), fk.from_column.clone()));
                }
            }

            let children: Vec<TreeNode> = order
                .into_iter()
                .map(|key| {
                    let group = &groups[&key];
                    let this_cols = group
                        .pairs
                        .iter()
                        .map(|(a, _)| a.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let other_cols = group
                        .pairs
                        .iter()
                        .map(|(_, b)| b.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let title = format!("{}.{}", group.other_schema, group.other_table);
                    let detail = if group.outgoing {
                        format!("{this_cols} → {other_cols}")
                    } else {
                        format!("{this_cols} ← {other_cols}")
                    };
                    // Id from the constraint's full identity (owner schema +
                    // name), so unrelated FK changes never renumber this node
                    // (stable expansion/selection) and same-named constraints in
                    // different schemas get distinct ids.
                    let mut n = TreeNode::new(
                        format!(
                            "{node_id}:fk:{}:{}.{}",
                            if group.outgoing { "out" } else { "in" },
                            escape_id_segment(&group.owner_schema),
                            escape_id_segment(&group.constraint_name)
                        ),
                        title,
                        NodeKind::ForeignKey {
                            outgoing: group.outgoing,
                        },
                        child_depth,
                    );
                    n.detail = Some(detail);
                    n
                })
                .collect();
            Ok(children)
        }
        NodeKind::TypesFolder => {
            let db = extract_db_from_id(node_id);
            let schema_scope = extract_schema_from_id(node_id);
            let mut types = services
                .metadata
                .list_types(profile, &db, schema_scope.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            sort_listed_objects(&mut types);
            Ok(types
                .into_iter()
                .map(|t| {
                    let schema = t.schema.clone().unwrap_or_default();
                    let mut n = TreeNode::new(
                        format!(
                            "{node_id}:ty:{}:{}",
                            escape_id_segment(&schema),
                            escape_id_segment(&t.name)
                        ),
                        object_title(&schema, &t.name, schema_scope.is_some()),
                        NodeKind::Type {
                            schema,
                            name: t.name,
                        },
                        child_depth,
                    );
                    n.detail = t.detail;
                    n
                })
                .collect())
        }
        NodeKind::DatabaseTriggersFolder => {
            let db = extract_db_from_id(node_id);
            let schema_scope = extract_schema_from_id(node_id);
            let mut triggers = services
                .metadata
                .list_database_triggers(profile, &db, schema_scope.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            sort_listed_objects(&mut triggers);
            Ok(triggers
                .into_iter()
                .map(|t| {
                    let schema = t.schema.clone().unwrap_or_default();
                    let mut n = TreeNode::new(
                        format!(
                            "{node_id}:tr:{}:{}",
                            escape_id_segment(&schema),
                            escape_id_segment(&t.name)
                        ),
                        object_title(&schema, &t.name, schema_scope.is_some()),
                        NodeKind::Trigger {
                            schema,
                            table: None,
                            name: t.name,
                        },
                        child_depth,
                    );
                    n.detail = t.detail;
                    n
                })
                .collect())
        }
        NodeKind::SequencesFolder => {
            let db = extract_db_from_id(node_id);
            let schema_scope = extract_schema_from_id(node_id);
            let mut sequences = services
                .metadata
                .list_sequences(profile, &db, schema_scope.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            sort_listed_objects(&mut sequences);
            Ok(sequences
                .into_iter()
                .map(|s| {
                    let schema = s.schema.clone().unwrap_or_default();
                    let mut n = TreeNode::new(
                        format!(
                            "{node_id}:sq:{}:{}",
                            escape_id_segment(&schema),
                            escape_id_segment(&s.name)
                        ),
                        object_title(&schema, &s.name, schema_scope.is_some()),
                        NodeKind::Sequence {
                            schema,
                            name: s.name,
                        },
                        child_depth,
                    );
                    n.detail = s.detail;
                    n
                })
                .collect())
        }
        NodeKind::SynonymsFolder => {
            let db = extract_db_from_id(node_id);
            let schema_scope = extract_schema_from_id(node_id);
            let mut synonyms = services
                .metadata
                .list_synonyms(profile, &db, schema_scope.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            sort_listed_objects(&mut synonyms);
            Ok(synonyms
                .into_iter()
                .map(|s| {
                    let schema = s.schema.clone().unwrap_or_default();
                    let base_object = s.detail.clone().unwrap_or_default();
                    let mut n = TreeNode::new(
                        format!(
                            "{node_id}:syn:{}:{}",
                            escape_id_segment(&schema),
                            escape_id_segment(&s.name)
                        ),
                        object_title(&schema, &s.name, schema_scope.is_some()),
                        NodeKind::Synonym {
                            schema,
                            name: s.name,
                            base_object: base_object.clone(),
                        },
                        child_depth,
                    );
                    n.detail = Some(base_object);
                    n
                })
                .collect())
        }
        NodeKind::ExtensionsFolder => {
            let db = extract_db_from_id(node_id);
            let mut extensions = services
                .metadata
                .list_extensions(profile, &db)
                .await
                .map_err(|e| e.to_string())?;
            sort_listed_objects(&mut extensions);
            Ok(extensions
                .into_iter()
                .map(|e| {
                    let version = e.detail.clone().unwrap_or_default();
                    let mut n = TreeNode::new(
                        format!("{node_id}:ext:{}", escape_id_segment(&e.name)),
                        e.name.clone(),
                        NodeKind::Extension {
                            name: e.name,
                            version: version.clone(),
                        },
                        child_depth,
                    );
                    n.detail = Some(version);
                    n
                })
                .collect())
        }
        NodeKind::EventsFolder => {
            let db = extract_db_from_id(node_id);
            let schema_scope = extract_schema_from_id(node_id);
            let mut events = services
                .metadata
                .list_events(profile, &db, schema_scope.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            sort_listed_objects(&mut events);
            Ok(events
                .into_iter()
                .map(|e| {
                    let schema = e.schema.clone().unwrap_or_default();
                    let mut n = TreeNode::new(
                        format!(
                            "{node_id}:ev:{}:{}",
                            escape_id_segment(&schema),
                            escape_id_segment(&e.name)
                        ),
                        object_title(&schema, &e.name, schema_scope.is_some()),
                        NodeKind::Event {
                            schema,
                            name: e.name,
                        },
                        child_depth,
                    );
                    n.detail = e.detail;
                    n
                })
                .collect())
        }
        NodeKind::MaterializedViewsFolder => {
            let db = extract_db_from_id(node_id);
            let schema_scope = extract_schema_from_id(node_id);
            let mut matviews = services
                .metadata
                .list_materialized_views(profile, &db, schema_scope.as_deref())
                .await
                .map_err(|e| e.to_string())?;
            sort_listed_objects(&mut matviews);
            Ok(matviews
                .into_iter()
                .map(|v| {
                    let schema = v.schema.clone().unwrap_or_default();
                    let mut n = TreeNode::new(
                        format!(
                            "{node_id}:mv:{}:{}",
                            escape_id_segment(&schema),
                            escape_id_segment(&v.name)
                        ),
                        object_title(&schema, &v.name, schema_scope.is_some()),
                        NodeKind::MaterializedView {
                            schema,
                            name: v.name,
                        },
                        child_depth,
                    );
                    n.detail = v.detail;
                    n
                })
                .collect())
        }
        NodeKind::TriggersFolder { schema, table } => {
            let db = extract_db_from_id(node_id);
            let triggers = services
                .metadata
                .list_triggers(profile, &db, schema, table)
                .await
                .map_err(|e| e.to_string())?;
            Ok(triggers
                .into_iter()
                .map(|t| {
                    let mut n = TreeNode::new(
                        format!("{node_id}:tr:{}", escape_id_segment(&t.name)),
                        t.name.clone(),
                        NodeKind::Trigger {
                            schema: schema.clone(),
                            table: Some(table.clone()),
                            name: t.name,
                        },
                        child_depth,
                    );
                    n.detail = t.detail;
                    n
                })
                .collect())
        }
        NodeKind::ConstraintsFolder { schema, table } => {
            let db = extract_db_from_id(node_id);
            let constraints = services
                .metadata
                .list_constraints(profile, &db, schema, table)
                .await
                .map_err(|e| e.to_string())?;
            Ok(constraints
                .into_iter()
                .filter_map(|c| {
                    let type_desc = c.detail.as_deref().unwrap_or_default();
                    if type_desc.contains("PRIMARY_KEY") || type_desc.contains("FOREIGN_KEY") {
                        return None;
                    }
                    let kind = if type_desc.contains("CHECK") {
                        ConstraintKind::Check
                    } else if type_desc.contains("UNIQUE") {
                        ConstraintKind::Unique
                    } else {
                        ConstraintKind::Other
                    };
                    let mut n = TreeNode::new(
                        format!("{node_id}:ck:{}", escape_id_segment(&c.name)),
                        c.name.clone(),
                        NodeKind::Constraint {
                            schema: schema.clone(),
                            table: table.clone(),
                            name: c.name,
                            kind,
                        },
                        child_depth,
                    );
                    n.detail = c.detail;
                    Some(n)
                })
                .collect())
        }
        NodeKind::Table { schema, name } => {
            let db = extract_db_from_id(node_id);
            let columns = services
                .metadata
                .list_columns(profile, &db, schema, name)
                .await
                .map_err(|e| e.to_string())?;
            let mut children: Vec<TreeNode> = columns
                .into_iter()
                .map(|c| {
                    let mut n = TreeNode::new(
                        format!("{node_id}:c:{}", escape_id_segment(&c.name)),
                        c.name,
                        NodeKind::Column,
                        child_depth,
                    );
                    n.detail = c.detail;
                    n
                })
                .collect();
            // Folders trail the columns: columns are the everyday drill-down,
            // and a table with no indexes still shows both folders (their own
            // loads report the emptiness instead of hiding it here).
            children.push(TreeNode::new(
                format!("{node_id}:indexes"),
                crate::localization::text("explorer.schema_tree.indexes_folder"),
                NodeKind::IndexesFolder {
                    schema: schema.clone(),
                    table: name.clone(),
                },
                child_depth,
            ));
            children.push(TreeNode::new(
                format!("{node_id}:statistics"),
                crate::localization::text("explorer.schema_tree.statistics_folder"),
                NodeKind::StatisticsFolder {
                    schema: schema.clone(),
                    table: name.clone(),
                },
                child_depth,
            ));
            children.push(TreeNode::new(
                format!("{node_id}:foreignkeys"),
                crate::localization::text("explorer.schema_tree.foreign_keys_folder"),
                NodeKind::ForeignKeysFolder {
                    schema: schema.clone(),
                    table: name.clone(),
                },
                child_depth,
            ));
            children.push(TreeNode::new(
                format!("{node_id}:triggers"),
                crate::localization::text("explorer.schema_tree.table_triggers_folder"),
                NodeKind::TriggersFolder {
                    schema: schema.clone(),
                    table: name.clone(),
                },
                child_depth,
            ));
            children.push(TreeNode::new(
                format!("{node_id}:constraints"),
                crate::localization::text("explorer.schema_tree.constraints_folder"),
                NodeKind::ConstraintsFolder {
                    schema: schema.clone(),
                    table: name.clone(),
                },
                child_depth,
            ));
            Ok(children)
        }
        NodeKind::MaterializedView { schema, name } => {
            let db = extract_db_from_id(node_id);
            let columns = services
                .metadata
                .list_columns(profile, &db, schema, name)
                .await
                .map_err(|e| e.to_string())?;
            Ok(columns
                .into_iter()
                .map(|c| {
                    let mut n = TreeNode::new(
                        format!("{node_id}:c:{}", escape_id_segment(&c.name)),
                        c.name,
                        NodeKind::Column,
                        child_depth,
                    );
                    n.detail = c.detail;
                    n
                })
                .collect())
        }
        NodeKind::View { schema, name } => {
            let db = extract_db_from_id(node_id);
            let columns = services
                .metadata
                .list_columns(profile, &db, schema, name)
                .await
                .map_err(|e| e.to_string())?;
            Ok(columns
                .into_iter()
                .map(|c| {
                    let mut n = TreeNode::new(
                        format!("{node_id}:c:{}", escape_id_segment(&c.name)),
                        c.name,
                        NodeKind::Column,
                        child_depth,
                    );
                    n.detail = c.detail;
                    n
                })
                .collect())
        }
        NodeKind::Procedure { schema, name } | NodeKind::Function { schema, name } => {
            let db = extract_db_from_id(node_id);
            let params = services
                .metadata
                .list_procedure_parameters(profile, &db, schema, name)
                .await
                .map_err(|e| e.to_string())?;
            Ok(params
                .into_iter()
                .map(|p| {
                    let mut n = TreeNode::new(
                        format!("{node_id}:param:{}", escape_id_segment(&p.name)),
                        p.name.clone(),
                        NodeKind::Parameter {
                            data_type: p.data_type.clone(),
                            is_output: p.is_output,
                        },
                        child_depth,
                    );
                    n.detail = Some(if p.is_output {
                        format!("{} OUTPUT", p.data_type)
                    } else {
                        p.data_type.clone()
                    });
                    n
                })
                .collect())
        }
        _ => Ok(Vec::new()),
    }
}

/// Escape a user-supplied identifier for embedding in a `:`-delimited node id.
///
/// SQL Server identifiers may legally contain a colon — `CREATE DATABASE
/// [Sales:2024]` is valid — and node ids are colon-delimited and parsed by
/// splitting on it. Unescaped, a database named `Sales:2024` produced the id
/// `srv:{uuid}:db:Sales:2024`, which [`extract_db_from_id`] read back as
/// `Sales`, and every metadata query under that node then ran against the wrong
/// database (or failed). `%` is escaped first so the encoding is reversible.
pub(crate) fn escape_id_segment(segment: &str) -> String {
    segment.replace('%', "%25").replace(':', "%3A")
}

/// Build the (`name`, `qualified`) clipboard payloads for a copy action from
/// already-resolved parts. `name` is the raw leaf identifier; `qualified` chains
/// the present segments (`schema` → `parent` → `leaf`), each bracket-quoted with
/// `]` doubled via [`sqlly_intel::metadata::quote_name`], so a name containing spaces,
/// dots, `]`, or a reserved word stays a single valid SQL Server identifier.
/// A `qualified` of `None` means the caller supplied no schema or parent, so
/// there is nothing to qualify with (e.g. a schema/database folder).
pub(crate) fn copy_names(
    schema: Option<&str>,
    parent: Option<&str>,
    leaf: &str,
) -> (String, Option<String>) {
    let q = sqlly_intel::metadata::quote_name;
    let qualified = match (schema, parent) {
        (Some(s), Some(p)) => Some(format!("{}.{}.{}", q(s), q(p), q(leaf))),
        (Some(s), None) => Some(format!("{}.{}", q(s), q(leaf))),
        (None, Some(p)) => Some(format!("{}.{}", q(p), q(leaf))),
        (None, None) => None,
    };
    (leaf.to_string(), qualified)
}

/// Resolve the copy payloads for a node: its raw leaf name and a fully
/// bracket-quoted qualified identifier. Leaf kinds whose own [`NodeKind`]
/// carries no name (columns, parameters, foreign keys) are qualified by walking
/// to their owning table/routine in `roots`.
pub(crate) fn copy_identity_for(
    roots: &[TreeNode],
    node_id: &str,
    kind: &NodeKind,
) -> (Option<String>, Option<String>) {
    // The raw leaf name, straight off the node (never the sanitized title).
    let leaf = || find_node_any(roots, node_id).map(|n| n.title.clone());
    let (name, qualified) = match kind {
        NodeKind::Table { schema, name }
        | NodeKind::View { schema, name }
        | NodeKind::Procedure { schema, name }
        | NodeKind::Function { schema, name }
        | NodeKind::Type { schema, name } => copy_names(Some(schema), None, name),
        NodeKind::Index {
            schema,
            table,
            name,
        }
        | NodeKind::Statistic {
            schema,
            table,
            name,
        } => copy_names(Some(schema), Some(table), name),
        NodeKind::Trigger { schema, name, .. } => copy_names(Some(schema), None, name),
        NodeKind::Constraint {
            schema,
            table,
            name,
            ..
        } => copy_names(Some(schema), Some(table), name),
        NodeKind::Sequence { schema, name }
        | NodeKind::Synonym { schema, name, .. }
        | NodeKind::Event { schema, name } => copy_names(Some(schema), None, name),
        NodeKind::MaterializedView { schema, name } => copy_names(Some(schema), None, name),
        NodeKind::Extension { name, .. } => copy_names(None, None, name),
        NodeKind::Column | NodeKind::Parameter { .. } => {
            let Some(leaf) = leaf() else {
                return (None, None);
            };
            // The owning table/routine carries the schema + object name.
            match find_parent(roots, node_id).map(|p| &p.kind) {
                Some(
                    NodeKind::Table { schema, name }
                    | NodeKind::View { schema, name }
                    | NodeKind::Procedure { schema, name }
                    | NodeKind::Function { schema, name },
                ) => copy_names(Some(schema), Some(name), &leaf),
                _ => (leaf, None),
            }
        }
        NodeKind::ForeignKey { .. } => {
            let Some(leaf) = leaf() else {
                return (None, None);
            };
            // The FK's parent is its table's Foreign Keys folder, which carries
            // the owning table's schema + name.
            match find_parent(roots, node_id).map(|p| &p.kind) {
                Some(NodeKind::ForeignKeysFolder { schema, table }) => {
                    copy_names(Some(schema), Some(table), &leaf)
                }
                _ => (leaf, None),
            }
        }
        // A schema or database folder has a canonical name but nothing to
        // qualify it with; copy the name, omit the qualified form.
        NodeKind::SchemaFolder { schema } => (schema.clone(), None),
        NodeKind::Database(db) => (db.clone(), None),
        // Redis: a folder copies its full prefix (the row shows only the last
        // segment), a key its exact name.
        NodeKind::RedisKeyFolder { prefix } => (prefix.clone(), None),
        NodeKind::RedisKey { key, .. } => (key.clone(), None),
        // Everything else — servers, grouping folders, Azure nodes, group
        // headers — has no qualified form, but every node still shows a name
        // and every name is copyable. Fall back to the node's own title.
        _ => {
            let Some(leaf) = leaf() else {
                return (None, None);
            };
            (leaf, None)
        }
    };
    (Some(name), qualified)
}

/// Find the parent of `id` anywhere in the tree (regardless of expansion), i.e.
/// the node whose direct children include `id`. Used to qualify a column /
/// parameter / foreign-key leaf by its owning table or routine, which the leaf's
/// own [`NodeKind`] does not carry.
pub(crate) fn find_parent<'a>(nodes: &'a [TreeNode], id: &str) -> Option<&'a TreeNode> {
    for node in nodes {
        if node.children.iter().any(|c| c.id == id) {
            return Some(node);
        }
        if let Some(found) = find_parent(&node.children, id) {
            return Some(found);
        }
    }
    None
}

/// Inverse of [`escape_id_segment`]. `%3A` before `%25`, or a literal `%3A`
/// typed into an object name would decode as a colon.
pub(crate) fn unescape_id_segment(segment: &str) -> String {
    segment.replace("%3A", ":").replace("%25", "%")
}

/// Extract the database name from a node id like
/// `srv:{uuid}:db:master` or `srv:{uuid}:db:master:tables`. Returns "" when the
/// id carries no `:db:` segment.
pub(crate) fn extract_db_from_id(id: &str) -> String {
    id.split(":db:")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .map(unescape_id_segment)
        .unwrap_or_default()
}

/// The schema scope embedded in a node id (`…:sch:{schema}:…`) when the
/// schema-folders display mode built this subtree. `None` in flat mode.
pub(crate) fn extract_schema_from_id(id: &str) -> Option<String> {
    id.split(":sch:")
        .nth(1)
        .and_then(|s| s.split(':').next())
        .filter(|s| !s.is_empty())
        .map(unescape_id_segment)
}

/// Snapshot every node's expansion state in a subtree, keyed by id.
pub(crate) fn collect_expansion(
    nodes: &[TreeNode],
    out: &mut std::collections::HashMap<String, bool>,
) {
    for node in nodes {
        out.insert(node.id.clone(), node.expanded);
        collect_expansion(&node.children, out);
    }
}

/// Re-apply remembered expansion to a freshly rebuilt subtree. Nodes with no
/// entry keep whatever the builder gave them, so newly discovered ones still
/// open by default.
pub(crate) fn apply_expansion(
    nodes: &mut [TreeNode],
    remembered: &std::collections::HashMap<String, bool>,
) {
    for node in nodes.iter_mut() {
        if let Some(&expanded) = remembered.get(&node.id) {
            node.expanded = expanded;
        }
        apply_expansion(&mut node.children, remembered);
    }
}

/// Collect the ids of every expanded node in a subtree (depth-first), used to
/// restore the drill-down after a refresh drops and re-fetches the subtree.
pub(crate) fn collect_expanded_ids(
    nodes: &[TreeNode],
    out: &mut std::collections::HashSet<String>,
) {
    for node in nodes {
        if node.expanded {
            out.insert(node.id.clone());
        }
        collect_expanded_ids(&node.children, out);
    }
}

/// Collapse every node in the tree without dropping cached children, so the
/// next expand is instant.
pub(crate) fn collapse_all_nodes(nodes: &mut [TreeNode]) {
    for node in nodes.iter_mut() {
        node.expanded = false;
        collapse_all_nodes(&mut node.children);
    }
}

/// Connection-row health: green when we verified we can talk to this server,
/// amber while a load is in flight, red after a failure to reach it. Idle
/// (disconnected) servers show no status dot (environment color still can).
/// Derived purely from the node's own state — which server is "active" for
/// the focused query window plays no part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ServerHealth {
    Connected,
    Connecting,
    Failed,
    /// Persisted last-known green from a previous session — rendered dimmed.
    StaleConnected,
    /// Persisted last-known red from a previous session — rendered dimmed.
    StaleFailed,
}

pub(crate) fn server_health(
    kind: &NodeKind,
    load_state: &LoadState,
    connection: super::ConnectionState,
) -> Option<ServerHealth> {
    if !matches!(kind, NodeKind::Server { .. } | NodeKind::AzureSqlServer) {
        return None;
    }
    match load_state {
        LoadState::Loading => Some(ServerHealth::Connecting),
        LoadState::Failed(_) | LoadState::PermissionDenied(_) => Some(ServerHealth::Failed),
        _ => match connection {
            super::ConnectionState::Connected => Some(ServerHealth::Connected),
            super::ConnectionState::Unreachable => Some(ServerHealth::Failed),
            super::ConnectionState::StaleConnected => Some(ServerHealth::StaleConnected),
            super::ConnectionState::StaleUnreachable => Some(ServerHealth::StaleFailed),
            super::ConnectionState::Disconnected => None,
        },
    }
}

/// Collapse a subtree and mark every expandable level unloaded, so the next
/// expand re-fetches children under the current Data Model preferences.
pub(crate) fn reset_subtree_loads(node: &mut TreeNode) {
    if matches!(
        node.kind,
        NodeKind::Server { .. }
            | NodeKind::Database(_)
            | NodeKind::SystemDatabasesFolder
            | NodeKind::SystemSchemasFolder
            | NodeKind::EmptySchemasFolder
            | NodeKind::SchemaFolder { .. }
            | NodeKind::TablesFolder
            | NodeKind::ViewsFolder
            | NodeKind::ProgrammabilityFolder
            | NodeKind::DatabaseTriggersFolder
            | NodeKind::SequencesFolder
            | NodeKind::SynonymsFolder
            | NodeKind::ExtensionsFolder
            | NodeKind::EventsFolder
            | NodeKind::MaterializedViewsFolder
    ) {
        node.expanded = false;
        node.load_state = LoadState::Unloaded;
        node.children.clear();
        // A load in flight was built under the old preferences and would
        // rebuild the level in the shape the user just changed away from.
        node.invalidate_pending_load();
        return;
    }
    for child in &mut node.children {
        reset_subtree_loads(child);
    }
}

/// The explorer's one ordering for schemas and schema-owned objects:
/// case-insensitive by schema then name, ties (names differing only by case,
/// legal on case-sensitive servers) broken by the exact spelling so the order
/// is total and stable. Live loads and snapshot hydration both use it, so a
/// database lists the same way whichever filled it — whatever collation the
/// server sorted with (MySQL `lower_case_table_names=0`, Postgres `C`, SQLite
/// `BINARY` all put every upper-case name first).
pub(crate) fn cmp_object_names(a: (&str, &str), b: (&str, &str)) -> std::cmp::Ordering {
    fn fold(s: &str) -> impl Iterator<Item = char> + '_ {
        s.chars().flat_map(char::to_lowercase)
    }
    fold(a.0)
        .cmp(fold(b.0))
        .then_with(|| fold(a.1).cmp(fold(b.1)))
        .then_with(|| a.0.cmp(b.0))
        .then_with(|| a.1.cmp(b.1))
}

/// Sort a server listing of schema-owned objects into [`cmp_object_names`]
/// order. Only for folders whose order carries no meaning of its own —
/// never columns (ordinal), index/key/constraint lists, or FK groups.
fn sort_listed_objects(items: &mut [sqlly_services::metadata::MetadataNode]) {
    items.sort_by(|a, b| {
        cmp_object_names(
            (a.schema.as_deref().unwrap_or_default(), &a.name),
            (b.schema.as_deref().unwrap_or_default(), &b.name),
        )
    });
}

/// Display title for a schema object: bare name inside a schema folder,
/// `schema.name` in the flat display mode so every schema's objects are
/// distinguishable in the shared folder.
pub(crate) fn object_title(schema: &str, name: &str, schema_scoped: bool) -> String {
    if schema_scoped || schema.is_empty() {
        sanitize_display(name)
    } else {
        format!("{}.{}", sanitize_display(schema), sanitize_display(name))
    }
}

/// Make an identifier safe to *render*, without changing what it is.
///
/// `CREATE TABLE [My` + newline + `Table]` is legal SQL Server, and so are tabs
/// and other control characters. Painted raw into a fixed-height row, the label
/// wraps and the second line is clipped — so the table reads as "My" and its
/// real name is nowhere. In a tooltip the break reads as structure the name
/// doesn't have.
///
/// Only the display string is touched: node ids, the `NodeKind` schema/name that
/// generate SQL, and the qualified name in a tooltip all keep the true
/// identifier, so scripting and lookups are unaffected. A control character
/// becomes `·` rather than vanishing, because a name silently missing a
/// character is worse than one that shows it has something unusual in it.
pub(crate) fn sanitize_display(raw: &str) -> String {
    if !raw.chars().any(|c| c.is_control()) {
        return raw.to_string();
    }
    raw.chars()
        .map(|c| if c.is_control() { '\u{00B7}' } else { c })
        .collect()
}

/// Build a database node's children from an IntelliSense data-model snapshot,
/// mirroring the shapes (ids, titles, folder layout) the lazy loader produces
/// so a later server fetch of the same level is indistinguishable. Everything
/// the snapshot knows arrives `Loaded`; levels it cannot describe (Types,
/// Indexes, Statistics, Foreign Keys, procedure parameters when the snapshot
/// carries none) stay `Unloaded` and keep their lazy fetch.
pub(crate) fn build_hydrated_db_children(
    db_node_id: &str,
    db_depth: usize,
    snapshot: &sqlly_lang::sqlmodel::ModelSnapshot,
    schemas: &[String],
    prefs: crate::preferences::DataModelPreferences,
    db_type: DatabaseType,
) -> Vec<TreeNode> {
    use crate::preferences::{GeneralSchemaPlacement, SchemaDisplayMode, SystemSchemaPlacement};
    let child_depth = db_depth + 1;
    match prefs.schema_display {
        SchemaDisplayMode::FlatPrefixed => {
            let mut folders = object_folders_for_db(db_node_id, child_depth, db_type);
            for folder in &mut folders {
                fill_folder_from_snapshot(folder, snapshot, None, db_type);
            }
            folders
        }
        SchemaDisplayMode::SchemaFolders => {
            // Object counts per schema, for the empty-schema placement rules.
            // `schemas` is the full list (including empty ones); the snapshot
            // only knows schemas that contain objects.
            let mut counts: std::collections::HashMap<String, u64> =
                std::collections::HashMap::new();
            for entry in snapshot.objects() {
                let schema = entry
                    .reference
                    .namespace
                    .as_ref()
                    .map(|n| n.spelling().to_lowercase())
                    .unwrap_or_default();
                *counts.entry(schema).or_default() += 1;
            }
            let mut all: Vec<(String, u64)> = schemas
                .iter()
                .map(|s| {
                    (
                        s.clone(),
                        counts.get(&s.to_lowercase()).copied().unwrap_or(0),
                    )
                })
                .collect();
            all.sort_by(|a, b| cmp_object_names(("", &a.0), ("", &b.0)));

            let make_schema = |schema: String, depth: usize| {
                let mut node = TreeNode::new(
                    format!("{db_node_id}:sch:{}", escape_id_segment(&schema)),
                    schema.clone(),
                    NodeKind::SchemaFolder {
                        schema: schema.clone(),
                    },
                    depth,
                );
                let mut trio = object_folders_for_db(&node.id, depth + 1, db_type);
                for folder in &mut trio {
                    fill_folder_from_snapshot(folder, snapshot, Some(&schema), db_type);
                }
                node.children = trio;
                node.load_state = LoadState::Loaded;
                node
            };

            let (system, general): (Vec<_>, Vec<_>) = all
                .into_iter()
                .partition(|(name, _)| is_system_schema(name, db_type));

            let mut children: Vec<TreeNode> = Vec::new();
            let mut empty_general: Vec<TreeNode> = Vec::new();
            for (name, count) in general {
                match prefs.general_schemas {
                    GeneralSchemaPlacement::Show => {
                        children.push(make_schema(name, child_depth));
                    }
                    GeneralSchemaPlacement::ShowIfNotEmpty => {
                        if count > 0 {
                            children.push(make_schema(name, child_depth));
                        }
                    }
                    GeneralSchemaPlacement::ShowWithEmptyFolder => {
                        if count == 0 {
                            empty_general.push(make_schema(name, child_depth + 1));
                        } else {
                            children.push(make_schema(name, child_depth));
                        }
                    }
                }
            }
            if !empty_general.is_empty() {
                let mut folder = TreeNode::new(
                    format!("{db_node_id}:emptyschemas"),
                    crate::localization::text("explorer.schema_tree.empty_schemas_folder"),
                    NodeKind::EmptySchemasFolder,
                    child_depth,
                );
                folder.load_state = LoadState::Loaded;
                folder.children = empty_general;
                children.push(folder);
            }
            match prefs.system_schemas {
                SystemSchemaPlacement::Hidden => {}
                SystemSchemaPlacement::OnlyIfNotEmpty => {
                    for (name, count) in system {
                        if count > 0 {
                            children.push(make_schema(name, child_depth));
                        }
                    }
                }
                SystemSchemaPlacement::Folder => {
                    let system_children: Vec<TreeNode> = system
                        .into_iter()
                        .map(|(name, _)| make_schema(name, child_depth + 1))
                        .collect();
                    if !system_children.is_empty() {
                        let mut folder = TreeNode::new(
                            format!("{db_node_id}:sysschemas"),
                            crate::localization::text("explorer.schema_tree.system_schemas_folder"),
                            NodeKind::SystemSchemasFolder,
                            child_depth,
                        );
                        folder.load_state = LoadState::Loaded;
                        folder.children = system_children;
                        children.push(folder);
                    }
                }
            }
            children
        }
    }
}

/// Fill one Tables/Views/Programmability folder with the snapshot's objects
/// (optionally scoped to one schema), marking the folder `Loaded`.
pub(crate) fn fill_folder_from_snapshot(
    folder: &mut TreeNode,
    snapshot: &sqlly_lang::sqlmodel::ModelSnapshot,
    schema_scope: Option<&str>,
    db_type: DatabaseType,
) {
    use sqlly_lang::sqlmodel::ObjectKind as MK;
    let node_id = folder.id.clone();
    let child_depth = folder.depth + 1;
    let scoped = schema_scope.is_some();
    let matviews_in_own_folder = db_type.supports_feature(DbFeature::MaterializedViews)
        && db_type != DatabaseType::CockroachDb;

    let mut entries: Vec<(
        String,
        String,
        &std::sync::Arc<sqlly_lang::sqlmodel::ObjectEntry>,
    )> = snapshot
        .objects()
        .filter(|e| match (&folder.kind, e.kind) {
            (NodeKind::TablesFolder, MK::Table) => true,
            (NodeKind::ViewsFolder, MK::View) => true,
            (NodeKind::ViewsFolder, MK::MaterializedView) => !matviews_in_own_folder,
            (NodeKind::MaterializedViewsFolder, MK::MaterializedView) => matviews_in_own_folder,
            (NodeKind::ProgrammabilityFolder, MK::Procedure | MK::Function) => true,
            _ => false,
        })
        .map(|e| {
            let schema = e
                .reference
                .namespace
                .as_ref()
                .map(|n| n.spelling().to_string())
                .unwrap_or_default();
            let name = e.reference.name.spelling().to_string();
            (schema, name, e)
        })
        .filter(|(schema, _, _)| match schema_scope {
            Some(scope) => schema.eq_ignore_ascii_case(scope),
            None => true,
        })
        .collect();
    entries.sort_by(|a, b| cmp_object_names((&a.0, &a.1), (&b.0, &b.1)));

    let mut children: Vec<TreeNode> = Vec::new();
    // Programmability keeps its lazy Types folder ahead of the routine rows,
    // exactly like the server-loaded layout.
    if matches!(folder.kind, NodeKind::ProgrammabilityFolder) {
        children.push(TreeNode::new(
            format!("{node_id}:types"),
            crate::localization::text("explorer.schema_tree.types_folder"),
            NodeKind::TypesFolder,
            child_depth,
        ));
    }
    for (schema, name, entry) in entries {
        children.push(match &folder.kind {
            NodeKind::TablesFolder => {
                hydrated_table_node(&node_id, &schema, &name, entry, child_depth, scoped)
            }
            NodeKind::ViewsFolder => {
                hydrated_view_node(&node_id, &schema, &name, entry, child_depth, scoped)
            }
            NodeKind::MaterializedViewsFolder => hydrated_materialized_view_node(
                &node_id,
                &schema,
                &name,
                entry,
                child_depth,
                scoped,
            ),
            _ => hydrated_routine_node(&node_id, &schema, &name, entry, child_depth, scoped),
        });
    }
    folder.children = children;
    folder.load_state = LoadState::Loaded;
}

/// Column node detail string in the lazy loader's format
/// (`"int, nullable"`, `"nvarchar(50), not null"`, `"int, identity, nullable"`),
/// so `parse_column_detail` and the scripting actions read hydrated columns
/// exactly like fetched ones.
pub(crate) fn hydrated_column_detail(col: &sqlly_lang::sqlmodel::ColumnDef) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(ty) = &col.ty {
        parts.push(ty.native.clone());
    }
    if col.identity {
        parts.push("identity".to_string());
    }
    parts.push(match col.nullable {
        Some(false) => "not null".to_string(),
        _ => "nullable".to_string(),
    });
    parts.join(", ")
}

pub(crate) fn hydrated_column_nodes(
    parent_id: &str,
    columns: &[sqlly_lang::sqlmodel::ColumnDef],
    depth: usize,
) -> Vec<TreeNode> {
    let mut cols: Vec<&sqlly_lang::sqlmodel::ColumnDef> = columns.iter().collect();
    cols.sort_by_key(|c| c.ordinal);
    cols.into_iter()
        .map(|c| {
            let name = c.name.spelling().to_string();
            let mut n = TreeNode::new(
                format!("{parent_id}:c:{}", escape_id_segment(&name)),
                name,
                NodeKind::Column,
                depth,
            );
            n.detail = Some(hydrated_column_detail(c));
            n
        })
        .collect()
}

pub(crate) fn hydrated_table_node(
    folder_id: &str,
    schema: &str,
    name: &str,
    entry: &sqlly_lang::sqlmodel::ObjectEntry,
    depth: usize,
    schema_scoped: bool,
) -> TreeNode {
    let id = format!(
        "{folder_id}:t:{}:{}",
        escape_id_segment(schema),
        escape_id_segment(name)
    );
    let mut node = TreeNode::new(
        id.clone(),
        object_title(schema, name, schema_scoped),
        NodeKind::Table {
            schema: schema.to_string(),
            name: name.to_string(),
        },
        depth,
    );
    if let Some(body) = entry.table() {
        let mut children = hydrated_column_nodes(&id, &body.columns, depth + 1);
        children.push(TreeNode::new(
            format!("{id}:indexes"),
            crate::localization::text("explorer.schema_tree.indexes_folder"),
            NodeKind::IndexesFolder {
                schema: schema.to_string(),
                table: name.to_string(),
            },
            depth + 1,
        ));
        children.push(TreeNode::new(
            format!("{id}:statistics"),
            crate::localization::text("explorer.schema_tree.statistics_folder"),
            NodeKind::StatisticsFolder {
                schema: schema.to_string(),
                table: name.to_string(),
            },
            depth + 1,
        ));
        children.push(TreeNode::new(
            format!("{id}:foreignkeys"),
            crate::localization::text("explorer.schema_tree.foreign_keys_folder"),
            NodeKind::ForeignKeysFolder {
                schema: schema.to_string(),
                table: name.to_string(),
            },
            depth + 1,
        ));
        children.push(TreeNode::new(
            format!("{id}:triggers"),
            crate::localization::text("explorer.schema_tree.table_triggers_folder"),
            NodeKind::TriggersFolder {
                schema: schema.to_string(),
                table: name.to_string(),
            },
            depth + 1,
        ));
        children.push(TreeNode::new(
            format!("{id}:constraints"),
            crate::localization::text("explorer.schema_tree.constraints_folder"),
            NodeKind::ConstraintsFolder {
                schema: schema.to_string(),
                table: name.to_string(),
            },
            depth + 1,
        ));
        node.children = children;
        node.load_state = LoadState::Loaded;
    }
    node
}

pub(crate) fn hydrated_view_node(
    folder_id: &str,
    schema: &str,
    name: &str,
    entry: &sqlly_lang::sqlmodel::ObjectEntry,
    depth: usize,
    schema_scoped: bool,
) -> TreeNode {
    let id = format!(
        "{folder_id}:v:{}:{}",
        escape_id_segment(schema),
        escape_id_segment(name)
    );
    let mut node = TreeNode::new(
        id.clone(),
        object_title(schema, name, schema_scoped),
        NodeKind::View {
            schema: schema.to_string(),
            name: name.to_string(),
        },
        depth,
    );
    if let sqlly_lang::sqlmodel::ObjectBody::View(body) = &entry.body {
        node.children = hydrated_column_nodes(&id, &body.columns, depth + 1);
        node.load_state = LoadState::Loaded;
    }
    node
}

pub(crate) fn hydrated_materialized_view_node(
    folder_id: &str,
    schema: &str,
    name: &str,
    entry: &sqlly_lang::sqlmodel::ObjectEntry,
    depth: usize,
    schema_scoped: bool,
) -> TreeNode {
    let id = format!(
        "{folder_id}:mv:{}:{}",
        escape_id_segment(schema),
        escape_id_segment(name)
    );
    let mut node = TreeNode::new(
        id.clone(),
        object_title(schema, name, schema_scoped),
        NodeKind::MaterializedView {
            schema: schema.to_string(),
            name: name.to_string(),
        },
        depth,
    );
    if let sqlly_lang::sqlmodel::ObjectBody::View(body) = &entry.body {
        node.children = hydrated_column_nodes(&id, &body.columns, depth + 1);
        node.load_state = LoadState::Loaded;
    }
    node
}

pub(crate) fn hydrated_routine_node(
    folder_id: &str,
    schema: &str,
    name: &str,
    entry: &sqlly_lang::sqlmodel::ObjectEntry,
    depth: usize,
    schema_scoped: bool,
) -> TreeNode {
    let is_function = matches!(
        entry.kind,
        sqlly_lang::sqlmodel::ObjectKind::Function
            | sqlly_lang::sqlmodel::ObjectKind::ScalarFunction
            | sqlly_lang::sqlmodel::ObjectKind::TableFunction
            | sqlly_lang::sqlmodel::ObjectKind::AggregateFunction
    );
    let prefix = if is_function { "f" } else { "p" };
    let id = format!(
        "{folder_id}:{prefix}:{}:{}",
        escape_id_segment(schema),
        escape_id_segment(name)
    );
    let kind = if is_function {
        NodeKind::Function {
            schema: schema.to_string(),
            name: name.to_string(),
        }
    } else {
        NodeKind::Procedure {
            schema: schema.to_string(),
            name: name.to_string(),
        }
    };
    let mut node = TreeNode::new(
        id.clone(),
        object_title(schema, name, schema_scoped),
        kind,
        depth,
    );
    // Parameters are lazy in the GPUI snapshot (load_parameters=false), so
    // routines usually stay Unloaded and fetch on expand. When the snapshot
    // does carry parameters (e.g. enriched by a previous expansion and then
    // persisted), hydrate them too.
    if let Some(body) = entry.routine()
        && !body.params.is_empty()
    {
        node.children = body
            .params
            .iter()
            .map(|p| {
                let pname = p.name.spelling().to_string();
                let data_type = p.ty.as_ref().map(|t| t.native.clone()).unwrap_or_default();
                let is_output = matches!(
                    p.mode,
                    sqlly_lang::sqlmodel::ParamMode::Out | sqlly_lang::sqlmodel::ParamMode::InOut
                );
                let mut n = TreeNode::new(
                    format!("{id}:param:{}", escape_id_segment(&pname)),
                    pname,
                    NodeKind::Parameter {
                        data_type: data_type.clone(),
                        is_output,
                    },
                    depth + 1,
                );
                n.detail = Some(if is_output {
                    format!("{data_type} OUTPUT")
                } else {
                    data_type
                });
                n
            })
            .collect();
        node.load_state = LoadState::Loaded;
    }
    node
}

/// Merge a hydrated child set into an existing node. An `Unloaded` node adopts
/// the hydrated children wholesale and becomes `Loaded`; a node the server (or
/// an earlier hydration) already populated keeps its own children, and the
/// merge recurses into matching ids so deeper `Unloaded` levels still fill.
/// `Loading` nodes recurse too — the in-flight fetch owns the node and will
/// overwrite it, but its already-present children can still gain depth.
pub(crate) fn merge_hydrated_children(existing: &mut TreeNode, hydrated: Vec<TreeNode>) {
    if matches!(existing.load_state, LoadState::Unloaded) {
        if !hydrated.is_empty() {
            existing.children = hydrated;
            existing.load_state = LoadState::Loaded;
        }
        return;
    }
    for h in hydrated {
        if let Some(e) = existing.children.iter_mut().find(|c| c.id == h.id) {
            merge_hydrated_children(e, h.children);
        }
    }
}

/// If `parent` is a table/view and `children` are column nodes, return the
/// `(schema, name, column_names)` triple so the parent view can enrich the
/// live completion catalog (incremental column completion). Returns `None` for
/// non-table/view nodes.
pub(crate) fn column_load_payload(
    parent: &NodeKind,
    children: &[TreeNode],
) -> Option<(String, String, Vec<String>)> {
    let (schema, name) = match parent {
        NodeKind::Table { schema, name } => (schema.clone(), name.clone()),
        NodeKind::View { schema, name } => (schema.clone(), name.clone()),
        _ => return None,
    };
    let columns: Vec<String> = children
        .iter()
        .filter(|c| matches!(c.kind, NodeKind::Column))
        .map(|c| c.title.clone())
        .collect();
    Some((schema, name, columns))
}

/// If `parent` is a procedure/function and `children` are parameter nodes,
/// return the `(schema, name, parameters)` triple so the parent view can enrich
/// the live completion procedures (EXEC snippets with real parameter slots).
/// Returns `None` for non-procedure/function nodes.
pub(crate) fn parameter_load_payload(
    parent: &NodeKind,
    children: &[TreeNode],
) -> Option<(String, String, Vec<ProcedureParameter>)> {
    let (schema, name) = match parent {
        NodeKind::Procedure { schema, name } => (schema.clone(), name.clone()),
        NodeKind::Function { schema, name } => (schema.clone(), name.clone()),
        _ => return None,
    };
    let parameters: Vec<ProcedureParameter> = children
        .iter()
        .filter_map(|c| match &c.kind {
            NodeKind::Parameter {
                data_type,
                is_output,
            } => {
                let mut p = ProcedureParameter::new(c.title.clone(), data_type.clone());
                if *is_output {
                    p = p.output();
                }
                Some(p)
            }
            _ => None,
        })
        .collect();
    Some((schema, name, parameters))
}
