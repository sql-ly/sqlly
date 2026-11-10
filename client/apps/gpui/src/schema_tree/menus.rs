use gpui::{Context, Entity, Window};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::maintenance::{MaintenanceOp, MaintenanceScope, MaintenanceText, ops_for};
use sqlly_services::tool_dialects::maintenance_dialect;
use uuid::Uuid;

use super::model::NodeKind;
use super::model::TreeNode;
use super::{
    ExplorerActionContext, SchemaTree, SchemaTreeEvent, ScriptDestination, TableSaveFormat,
    TableScriptKind, script_object_kind,
};
use crate::model_codegen::ModelLanguage;

/// Typed context-menu action for the schema tree explorer.
#[derive(Clone, Debug, PartialEq)]
pub enum ExplorerMenuAction {
    NewQuery,
    Connect,
    /// Close this node's server connection and return it to the default
    /// (gray) state. Per node — other connected servers are untouched.
    Disconnect,
    ConnectToCurrentQuery,
    OpenInNewTab,
    Refresh,
    AddServer,
    #[allow(dead_code)]
    ManageConnections,
    EditConnection,
    /// Open the connection editor on an independent copy of this saved
    /// connection; nothing is written until the copy is saved.
    DuplicateConnection,
    DeleteConnection,
    Properties,
    /// Launch the engine's native CLI (psql, mysql, sqlcmd, redis-cli, …) in
    /// a terminal window with this connection's settings applied.
    OpenInTerminal,

    SelectTopRows {
        limit: usize,
    },
    DesignQuery,
    CreateTo {
        destination: ScriptDestination,
    },
    SaveDataAs {
        format: TableSaveFormat,
    },
    TransferDataTo,
    ScriptInsert,
    ScriptMerge,
    DeleteAllRows,
    TruncateTable,
    DropTable,

    // Tier 2: Common RDBMS features
    ViewData,
    FilterData,
    CountRows,
    ScriptAlter,
    EditStructure,
    ScriptDrop,
    Rename,
    EditDefinition,
    ViewDependencies,
    FindUsages,

    RunProcedure,
    CopyInvocation,

    CopyName,
    CopyQualifiedName,

    // Tier 3: SQLLY differentiators
    AskSchema,
    GenerateSql,
    FindJoinPath,
    PinToContext,
    FocusInRelationships,

    /// Export the whole database as a SQL dump file.
    ExportDatabaseSql,
    /// Batch-export every table of this database/schema to files.
    ExportTables,
    /// Open the import wizard with clipboard text, targeting this database.
    ImportFromClipboard,
    /// Stream a .sql script file against this database connection.
    RunSqlFile,
    /// Open the Object Browser inventory for this database or schema.
    BrowseObjects,
    /// Search every table in this database for a literal value.
    SearchDatabaseValue,
    /// Capture this database's schema as a saved snapshot.
    SaveSchemaSnapshot,
    /// Open the snapshot timeline for this database.
    SchemaHistory,

    /// Hide this schema from the explorer on this connection (remembered
    /// with the connection; queries and IntelliSense still see it).
    HideSchema,
    /// Bring back every schema hidden from the explorer in this database.
    ShowHiddenSchemas,

    /// Open the Create Database dialog for this server.
    CreateDatabase,
    /// Open the Drop Database dialog for this database.
    DropDatabase,

    // Redis key browser
    /// Open the key view: header strip, paged value, per-type edits.
    RedisOpenValue,
    /// Open a tab that reads the key with the command its type calls for
    /// (the pre-key-view behaviour, kept for a plain query result).
    RedisOpenInQueryTab,
    /// Copy that read command (or the folder's SCAN) to the clipboard.
    RedisCopyCommand,
    /// Set the key's time to live (EXPIRE) after a prompt.
    RedisSetTtl,
    /// Remove the key's time to live (PERSIST).
    RedisPersist,
    /// Delete the key (DEL) after confirmation.
    RedisDeleteKey,
    /// Prompt for the MATCH pattern the database node scans with.
    RedisFilterKeys,
    /// Scan every key again.
    RedisClearFilter,
    /// Materialize the next window of rows the listing already scanned (no
    /// rescan): the note row's click, offered on the note's menu too.
    RedisLoadMore,
    /// Continue the listing's SCAN from the cursor it stopped at, appending
    /// what it finds to the rows already shown.
    RedisScanMore,
    /// Open the memory analyzer report pane for this database (read-only
    /// bounded reports beside Disk Usage: memory by type, top keys, TTL
    /// distribution).
    RedisMemoryAnalyzer,
    /// Open the Redis Cluster topology pane for this connection.
    RedisClusterTopology,
    /// Delete every key matching this folder's prefix (or a database's
    /// pattern) after a dry-run count, in confirmed UNLINK batches.
    RedisBulkDelete,
    /// Set or remove the TTL on every key matching this folder's prefix (or
    /// a database's pattern) after a dry-run count.
    RedisBulkTtl,
    /// Run a local redis-cli command file against this database, line-wise
    /// with per-line error capture.
    RedisImportCommands,
    /// Rename the key (RENAMENX, or RENAME to overwrite) after a review.
    RedisRenameKey,
    /// Copy the key to another logical database (COPY … DB n).
    RedisCopyToDatabase,
    /// Copy the key to another connection (DUMP here, RESTORE there).
    RedisCopyToConnection,
    /// Export the folder's (or database's) keys as an importable command
    /// file (SQLLY.DUMPSCRIPT).
    RedisExportCommands,
    /// Name the logical database on this profile (`db3 — sessions`).
    RedisNameDatabase,
    /// Choose which logical databases the explorer lists for the profile.
    RedisVisibleDatabases,
    /// Open a tab that runs `FT.INFO` on the search index.
    RedisIndexInfo,
    /// Open a tab seeded with a match-everything `FT.SEARCH` over the index.
    RedisIndexQuery,
    /// Open a tab seeded with `FT.EXPLAIN` for the index.
    RedisIndexExplain,
    /// Open a tab seeded with `FT.PROFILE` for the index.
    RedisIndexProfile,
    /// Open a reviewed `FT.DROPINDEX` script for the index (destructive:
    /// running it confirms first).
    RedisIndexDrop,

    /// Script as CREATE for explorer leaf objects (triggers, constraints, etc.).
    ScriptCreate,
    /// Open a preview tab to install a PostgreSQL extension.
    InstallExtension,
    /// Open an ALTER EVENT preview for a MySQL/MariaDB event.
    EditEvent,
    /// Open a REFRESH MATERIALIZED VIEW preview.
    RefreshMaterializedView,

    /// Generate a typed data model (TypeScript interface, Prisma model, Go
    /// struct, …) from the table/view's live column metadata, into a new tab
    /// or the clipboard.
    GenerateModel {
        language: ModelLanguage,
        destination: ScriptDestination,
    },
    /// Generate one file of models for every table in the schema/database.
    GenerateModelsForAll {
        language: ModelLanguage,
    },

    ToggleFavorite,
    Unpin,
    MovePinnedUp,
    MovePinnedDown,
    /// Pin the object (if it is not pinned yet) into a named pin group, or
    /// into the ungrouped default when `group` is `None`.
    PinToGroup {
        group: Option<uuid::Uuid>,
    },
    /// Ask for a name, create a pin group on this server, and put the
    /// object in it.
    NewPinGroup,
    /// Rename the right-clicked pin group folder.
    RenamePinGroup,
    /// Delete the right-clicked pin group; its objects stay pinned.
    DeletePinGroup,

    /// Open one of the tool dialogs against the node that was right-clicked.
    /// These live in the server and database Tools submenus; the app retargets
    /// the connection to that node before opening the tool.
    Tool(ExplorerTool),

    /// One entry of the Maintenance submenu on a database, table, or index
    /// node: vacuum / analyze / reindex / update statistics / integrity check
    /// in the node's engine vocabulary. `Script` opens the statement in a new
    /// query tab bound to the node; `Run` confirms first, then executes it
    /// through the normal query path.
    Maintenance {
        op: MaintenanceOp,
        mode: MaintenanceMode,
    },

    /// Run a user template flagged as an explorer action against this object.
    /// The name re-resolves the template at dispatch time, so edits between
    /// menu build and click can't fire a stale body.
    RunTemplate {
        name: String,
    },

    /// Collect this object into the active Deploy Cart, optionally with its
    /// table data (tables only carry data).
    AddToDeployCart {
        include_data: bool,
    },
    /// Open the Deploy Cart window.
    OpenDeployCart,
}

/// Script-first (review in a tab) or run-now (confirm, then execute) form of
/// a Maintenance submenu entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaintenanceMode {
    Script,
    Run,
}

/// The scope a node's Maintenance submenu addresses, from its kind. `None`
/// for kinds that carry no maintenance menu.
pub fn maintenance_scope(kind: &NodeKind) -> Option<MaintenanceScope> {
    match kind {
        NodeKind::Database(_) => Some(MaintenanceScope::Database),
        NodeKind::Table { .. } => Some(MaintenanceScope::Table),
        NodeKind::Index { .. } => Some(MaintenanceScope::Index),
        _ => None,
    }
}

/// The Maintenance submenu for a scope, built from the node's engine: each
/// offered op appears twice — its Script form, then (after a separator) its
/// Run form. An engine without a maintenance vocabulary gets a single
/// disabled entry carrying the capability reason instead of items that would
/// fail; every entry needs a connection to run against.
fn maintenance_submenu(scope: MaintenanceScope, availability: &MenuAvailability) -> MenuEntry {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    let entries = match crate::capabilities::maintenance(availability.db) {
        unavailable @ crate::capabilities::Capability::Unavailable(_) => vec![M::ActionOwned {
            label: unavailable.reason().unwrap_or_default(),
            action: A::Refresh,
            enabled: false,
        }],
        crate::capabilities::Capability::Available => {
            let Some(dialect) = maintenance_dialect(availability.db) else {
                return M::SubmenuOwned {
                    label: crate::localization::text("explorer.menus.maintenance_group"),
                    entries: Vec::new(),
                    enabled: false,
                };
            };
            let ops: Vec<(MaintenanceOp, MaintenanceText)> = ops_for(dialect, scope)
                .into_iter()
                .filter_map(|op| op.describe(dialect, scope).map(|text| (op, text)))
                .collect();
            let mut entries: Vec<MenuEntry> = ops
                .iter()
                .map(|(op, text)| M::Action {
                    label: text.script_label,
                    action: A::Maintenance {
                        op: *op,
                        mode: MaintenanceMode::Script,
                    },
                    enabled: availability.has_connection,
                })
                .collect();
            entries.push(M::Separator);
            entries.extend(ops.iter().map(|(op, text)| M::Action {
                label: text.run_label,
                action: A::Maintenance {
                    op: *op,
                    mode: MaintenanceMode::Run,
                },
                // Running is a write: a SELECT-only connection scripts only.
                enabled: availability.has_connection && availability.can_mutate,
            }));
            entries
        }
    };
    let has_items = entries.iter().any(|e| {
        matches!(
            e,
            M::Action { .. } | M::ActionOwned { .. } | M::SubmenuOwned { .. }
        )
    });
    M::SubmenuOwned {
        label: crate::localization::text("explorer.menus.maintenance_group"),
        entries,
        enabled: has_items,
    }
}

/// A tool dialog reachable from a server or database node's Tools submenu.
///
/// These used to sit in the application Tools menu, where they always opened
/// against whatever connection happened to be active. Hanging them off the
/// node instead makes the target explicit: the server or database you
/// right-clicked is the one the tool opens on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExplorerTool {
    ServerDashboard,
    SqlAgent,
    Security,
    ConnectionHealth,
    BackupRestore,
    ActivityMonitor,
    Designers,
    ImportExport,
    SchemaCompare,
    DataCompare,
    #[cfg(not(target_arch = "wasm32"))]
    DataTransfer,
    QueryInsights,
    DiskUsage,
    IndexAnalyzer,
    TestDataGenerator,
    RelationshipGraph,
    QueryBuilder,
    DocumentSchema,
    TenantScoping,
    QueryHistory,
    ExtendedEvents,
    /// The Activity Monitor's Redis Profiler (MONITOR) tab.
    RedisProfiler,
    /// The Activity Monitor's Redis Pub/Sub tab.
    RedisPubSub,
}

impl ExplorerTool {
    /// Menu label, matching the wording these tools carried in the application
    /// Tools menu.
    pub fn label(self) -> String {
        crate::localization::text(match self {
            Self::ServerDashboard => "explorer.menus.tool_server_dashboard",
            Self::SqlAgent => "explorer.menus.tool_sql_agent",
            Self::Security => "explorer.menus.tool_security",
            Self::ConnectionHealth => "explorer.menus.tool_connection_health",
            Self::BackupRestore => "explorer.menus.tool_backup_restore",
            Self::ActivityMonitor => "explorer.menus.tool_activity_monitor",
            Self::Designers => "explorer.menus.tool_designers",
            Self::ImportExport => "explorer.menus.tool_import_export",
            Self::SchemaCompare => "explorer.menus.tool_schema_compare",
            Self::DataCompare => "explorer.menus.tool_data_compare",
            #[cfg(not(target_arch = "wasm32"))]
            Self::DataTransfer => "explorer.menus.tool_data_transfer",
            Self::QueryInsights => "explorer.menus.tool_query_insights",
            Self::DiskUsage => "explorer.menus.tool_disk_usage",
            Self::IndexAnalyzer => "explorer.menus.tool_index_analyzer",
            Self::TestDataGenerator => "explorer.menus.tool_test_data_generator",
            Self::RelationshipGraph => "explorer.menus.tool_relationship_graph",
            Self::QueryBuilder => "explorer.menus.tool_query_builder",
            Self::DocumentSchema => "explorer.menus.tool_document_schema",
            Self::TenantScoping => "explorer.menus.tool_tenant_scoping",
            Self::QueryHistory => "explorer.menus.tool_query_history",
            Self::ExtendedEvents => "explorer.menus.tool_extended_events",
            Self::RedisProfiler => "explorer.menus.tool_redis_profiler",
            Self::RedisPubSub => "explorer.menus.tool_redis_pubsub",
        })
    }
}

/// The tools a server node offers: instance-wide surfaces.
pub const SERVER_TOOLS: &[ExplorerTool] = &[
    ExplorerTool::ServerDashboard,
    ExplorerTool::SqlAgent,
    ExplorerTool::Security,
    ExplorerTool::ConnectionHealth,
    // Server-scoped Disk Usage lists every database; the database-node entry
    // (in `DATABASE_TOOLS`) scopes to a single database.
    ExplorerTool::DiskUsage,
];

/// The tools a database node offers. Security appears here as well as on the
/// server: users and roles are database-scoped too. The database menu sorts
/// them into its Data, Schema and Tools groups; a test checks it still offers
/// each of these exactly once.
#[cfg(test)]
pub const DATABASE_TOOLS: &[ExplorerTool] = &[
    ExplorerTool::Security,
    ExplorerTool::BackupRestore,
    ExplorerTool::ActivityMonitor,
    ExplorerTool::Designers,
    ExplorerTool::ImportExport,
    ExplorerTool::SchemaCompare,
    ExplorerTool::DataCompare,
    #[cfg(not(target_arch = "wasm32"))]
    ExplorerTool::DataTransfer,
    ExplorerTool::QueryInsights,
    ExplorerTool::DiskUsage,
    ExplorerTool::IndexAnalyzer,
    ExplorerTool::TestDataGenerator,
    ExplorerTool::RelationshipGraph,
    ExplorerTool::QueryBuilder,
    ExplorerTool::DocumentSchema,
    ExplorerTool::TenantScoping,
    ExplorerTool::QueryHistory,
    ExplorerTool::ExtendedEvents,
];

/// The tools a Redis database offers: the monitoring surfaces its `INFO`,
/// `CLIENT LIST`, `SLOWLOG` and keyspace memory sample feed.
pub const REDIS_DATABASE_TOOLS: &[ExplorerTool] = &[
    ExplorerTool::ServerDashboard,
    ExplorerTool::ActivityMonitor,
    ExplorerTool::QueryInsights,
    ExplorerTool::DiskUsage,
    ExplorerTool::QueryHistory,
    // The ACL page (ACL users and log).
    ExplorerTool::Security,
];

/// Context menus of the Redis key browser (everything under a Redis server
/// node). Databases scan and filter, folders copy their SCAN, keys open,
/// copy, expire and delete.
pub(crate) fn redis_menu_entries(
    ctx: &ExplorerActionContext,
    availability: &MenuAvailability,
) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;
    let action = |key, action, enabled| M::ActionOwned {
        label: crate::localization::text(key),
        action,
        enabled,
    };
    let mut entries = copy_name_only_entries();
    match &ctx.kind {
        NodeKind::Database(_) => {
            // New Query leads here too, as on every database menu; Copy Name
            // moves to the footer.
            entries.clear();
            let mut tools = vec![
                action(
                    "explorer.menus.redis_database_memory_analyzer",
                    A::RedisMemoryAnalyzer,
                    availability.has_connection,
                ),
                action(
                    "explorer.menus.redis_database_cluster_topology",
                    A::RedisClusterTopology,
                    availability.has_connection,
                ),
                action(
                    "explorer.menus.redis_database_profiler",
                    A::Tool(ExplorerTool::RedisProfiler),
                    availability.has_connection,
                ),
                action(
                    "explorer.menus.redis_database_pubsub",
                    A::Tool(ExplorerTool::RedisPubSub),
                    availability.has_connection,
                ),
                M::Separator,
            ];
            tools.extend(tool_items(REDIS_DATABASE_TOOLS, availability));
            entries.extend([
                action(
                    "explorer.menus.redis_database_new_query",
                    A::NewQuery,
                    availability.has_connection,
                ),
                action(
                    "explorer.menus.redis_database_filter_keys",
                    A::RedisFilterKeys,
                    availability.has_connection,
                ),
                action(
                    "explorer.menus.redis_database_clear_filter",
                    A::RedisClearFilter,
                    availability.has_connection,
                ),
                action(
                    "explorer.menus.redis_database_scan_more",
                    A::RedisScanMore,
                    availability.has_connection && availability.redis_scan_more,
                ),
                M::Separator,
                localized_group(
                    "explorer.menus.redis_database_bulk_edit_group",
                    vec![
                        action(
                            "explorer.menus.redis_database_delete_by_pattern",
                            A::RedisBulkDelete,
                            availability.can_mutate,
                        ),
                        action(
                            "explorer.menus.redis_database_set_ttl_by_pattern",
                            A::RedisBulkTtl,
                            availability.can_mutate,
                        ),
                    ],
                ),
                localized_group(
                    "explorer.menus.redis_database_import_export_group",
                    vec![
                        action(
                            "explorer.menus.redis_database_import_commands",
                            A::RedisImportCommands,
                            availability.can_mutate,
                        ),
                        action(
                            "explorer.menus.redis_database_export_commands",
                            A::RedisExportCommands,
                            availability.has_connection,
                        ),
                    ],
                ),
                localized_group("explorer.menus.redis_database_tools_group", tools),
                M::Separator,
                copy_name(),
                action(
                    "explorer.menus.redis_database_rename",
                    A::RedisNameDatabase,
                    true,
                ),
                action(
                    "explorer.menus.redis_database_visible_databases",
                    A::RedisVisibleDatabases,
                    true,
                ),
                refresh(availability),
            ]);
        }
        NodeKind::RedisKeyFolder { .. } => {
            entries.extend([
                action(
                    "explorer.menus.redis_folder_copy_scan",
                    A::RedisCopyCommand,
                    true,
                ),
                action(
                    "explorer.menus.redis_folder_scan_more",
                    A::RedisScanMore,
                    availability.has_connection && availability.redis_scan_more,
                ),
                M::Separator,
                action(
                    "explorer.menus.redis_folder_delete_keys",
                    A::RedisBulkDelete,
                    availability.can_mutate,
                ),
                action(
                    "explorer.menus.redis_folder_set_ttl",
                    A::RedisBulkTtl,
                    availability.can_mutate,
                ),
                action(
                    "explorer.menus.redis_folder_export_commands",
                    A::RedisExportCommands,
                    availability.has_connection,
                ),
                M::Separator,
                action(
                    "explorer.menus.redis_folder_refresh",
                    A::Refresh,
                    !availability.is_loading,
                ),
            ]);
        }
        NodeKind::RedisKey { .. } => {
            entries.clear();
            entries.extend([
                action(
                    "explorer.menus.redis_key_open_value",
                    A::RedisOpenValue,
                    availability.has_connection,
                ),
                action(
                    "explorer.menus.redis_key_open_query",
                    A::RedisOpenInQueryTab,
                    availability.has_connection,
                ),
                M::Separator,
                action(
                    "explorer.menus.redis_key_rename",
                    A::RedisRenameKey,
                    availability.can_mutate,
                ),
                localized_group(
                    "explorer.menus.redis_key_expiry_group",
                    vec![
                        action(
                            "explorer.menus.redis_key_set_ttl",
                            A::RedisSetTtl,
                            availability.can_mutate,
                        ),
                        action(
                            "explorer.menus.redis_key_remove_expiry",
                            A::RedisPersist,
                            availability.can_mutate,
                        ),
                    ],
                ),
                localized_group(
                    "explorer.menus.redis_key_copy_to_group",
                    vec![
                        action(
                            "explorer.menus.redis_key_copy_to_database",
                            A::RedisCopyToDatabase,
                            availability.can_mutate,
                        ),
                        action(
                            "explorer.menus.redis_key_copy_to_connection",
                            A::RedisCopyToConnection,
                            availability.has_connection,
                        ),
                    ],
                ),
                M::Separator,
                localized_group(
                    "explorer.menus.redis_key_copy_group",
                    vec![
                        copy_name(),
                        action(
                            "explorer.menus.redis_key_copy_command",
                            A::RedisCopyCommand,
                            true,
                        ),
                    ],
                ),
                M::Separator,
                action(
                    "explorer.menus.redis_key_delete",
                    A::RedisDeleteKey,
                    availability.can_mutate,
                ),
            ]);
        }
        NodeKind::RedisIndexesFolder => {
            entries.extend([action(
                "explorer.menus.redis_indexes_refresh",
                A::Refresh,
                !availability.is_loading,
            )]);
        }
        NodeKind::RedisIndex { .. } => {
            entries.extend([
                action(
                    "explorer.menus.redis_index_open",
                    A::RedisIndexInfo,
                    availability.has_connection,
                ),
                action(
                    "explorer.menus.redis_index_query",
                    A::RedisIndexQuery,
                    availability.has_connection,
                ),
                action(
                    "explorer.menus.redis_index_explain",
                    A::RedisIndexExplain,
                    availability.has_connection,
                ),
                action(
                    "explorer.menus.redis_index_profile",
                    A::RedisIndexProfile,
                    availability.has_connection,
                ),
                M::Separator,
                action(
                    "explorer.menus.redis_index_drop",
                    A::RedisIndexDrop,
                    availability.can_mutate,
                ),
            ]);
        }
        // The listing's note row is not an object: no Copy Name, only the
        // two ways to see more of the listing it describes.
        NodeKind::RedisNote { listing } => {
            entries.clear();
            entries.extend([
                action(
                    "explorer.menus.redis_note_load_more",
                    A::RedisLoadMore,
                    listing.hidden_rows() > 0,
                ),
                action(
                    "explorer.menus.redis_note_scan_more",
                    A::RedisScanMore,
                    availability.has_connection && listing.scan_incomplete(),
                ),
            ]);
        }
        _ => {
            entries.pop();
        }
    }
    entries
}

/// The Tools submenu for a node kind. Every tool runs SQL against the node, so
/// the whole submenu is disabled without a connection to run it on.
fn tools_submenu(tools: &'static [ExplorerTool], availability: &MenuAvailability) -> MenuEntry {
    MenuEntry::SubmenuOwned {
        label: crate::localization::text("explorer.menus.tools_group"),
        enabled: availability.has_connection,
        entries: tools
            .iter()
            .map(|tool| MenuEntry::ActionOwned {
                label: tool.label(),
                action: ExplorerMenuAction::Tool(*tool),
                enabled: availability.has_connection,
            })
            .collect(),
    }
}

fn localized_group(key: &'static str, entries: Vec<MenuEntry>) -> MenuEntry {
    MenuEntry::SubmenuOwned {
        label: crate::localization::text(key),
        entries,
        enabled: true,
    }
}

fn localized_action(key: &'static str, action: ExplorerMenuAction, enabled: bool) -> MenuEntry {
    MenuEntry::ActionOwned {
        label: crate::localization::text(key),
        action,
        enabled,
    }
}

/// The tool windows for a node kind as plain items, for menus that list them
/// inside a larger Tools group.
fn tool_items(tools: &'static [ExplorerTool], availability: &MenuAvailability) -> Vec<MenuEntry> {
    tools
        .iter()
        .map(|tool| MenuEntry::ActionOwned {
            label: tool.label(),
            action: ExplorerMenuAction::Tool(*tool),
            enabled: availability.has_connection,
        })
        .collect()
}

fn copy_name() -> MenuEntry {
    MenuEntry::ActionOwned {
        label: crate::localization::text("explorer.menus.copy_name"),
        action: ExplorerMenuAction::CopyName,
        enabled: true,
    }
}

/// Copy Name and Copy Qualified Name (plus a routine's invocation script),
/// the Copy group on the large object menus.
fn copy_submenu(extra: Option<MenuEntry>) -> MenuEntry {
    let mut entries = vec![
        copy_name(),
        MenuEntry::ActionOwned {
            label: crate::localization::text("explorer.menus.copy_qualified_name"),
            action: ExplorerMenuAction::CopyQualifiedName,
            enabled: true,
        },
    ];
    entries.extend(extra);
    localized_group("explorer.menus.copy_group", entries)
}

/// The large menus' footer Pin submenu: Pin / Unpin, then the pin-group
/// choices (kept in one entry so those menus' top level stays short).
fn pin(availability: &MenuAvailability) -> MenuEntry {
    let MenuEntry::SubmenuOwned {
        entries: groups, ..
    } = pin_group_submenu("explorer.menus.pin_to_group", availability)
    else {
        unreachable!("pin_group_submenu builds a submenu");
    };
    let mut entries = vec![
        MenuEntry::ActionOwned {
            label: crate::localization::text("explorer.menus.pin_unpin"),
            action: ExplorerMenuAction::ToggleFavorite,
            enabled: true,
        },
        MenuEntry::Separator,
    ];
    entries.extend(groups);
    localized_group("explorer.menus.pin_submenu", entries)
}

fn refresh(availability: &MenuAvailability) -> MenuEntry {
    MenuEntry::ActionOwned {
        label: crate::localization::text("explorer.menus.refresh"),
        action: ExplorerMenuAction::Refresh,
        enabled: !availability.is_loading,
    }
}

fn properties() -> MenuEntry {
    MenuEntry::ActionOwned {
        label: crate::localization::text("explorer.menus.properties"),
        action: ExplorerMenuAction::Properties,
        enabled: true,
    }
}

/// CREATE To ▸ New Query Window / Clipboard, inside Script As.
fn create_to_submenu() -> MenuEntry {
    MenuEntry::SubmenuOwned {
        label: crate::localization::text("explorer.menus.create_to_group"),
        enabled: true,
        entries: vec![
            MenuEntry::ActionOwned {
                label: crate::localization::text("explorer.menus.create_to_new_query"),
                action: ExplorerMenuAction::CreateTo {
                    destination: ScriptDestination::NewQuery,
                },
                enabled: true,
            },
            MenuEntry::ActionOwned {
                label: crate::localization::text("explorer.menus.create_to_clipboard"),
                action: ExplorerMenuAction::CreateTo {
                    destination: ScriptDestination::Clipboard,
                },
                enabled: true,
            },
        ],
    }
}

/// View Dependencies + Find Usages.
fn dependencies_submenu(enabled: bool) -> MenuEntry {
    localized_group(
        "explorer.menus.dependencies_group",
        vec![
            MenuEntry::ActionOwned {
                label: crate::localization::text("explorer.menus.view_dependencies"),
                action: ExplorerMenuAction::ViewDependencies,
                enabled,
            },
            MenuEntry::ActionOwned {
                label: crate::localization::text("explorer.menus.find_usages"),
                action: ExplorerMenuAction::FindUsages,
                enabled,
            },
        ],
    )
}

/// The AI submenu for an object; tables also offer Find Join Path.
fn ai_submenu(noun: &'static str, join_path: bool) -> MenuEntry {
    let (ask, generate) = match noun {
        "table" => (
            "explorer.menus.ai_ask_table",
            "explorer.menus.ai_generate_table",
        ),
        "view" => (
            "explorer.menus.ai_ask_view",
            "explorer.menus.ai_generate_view",
        ),
        "procedure" => (
            "explorer.menus.ai_ask_procedure",
            "explorer.menus.ai_generate_procedure",
        ),
        _ => (
            "explorer.menus.ai_ask_function",
            "explorer.menus.ai_generate_function",
        ),
    };
    let mut entries = vec![
        MenuEntry::ActionOwned {
            label: crate::localization::text(ask),
            action: ExplorerMenuAction::AskSchema,
            enabled: true,
        },
        MenuEntry::ActionOwned {
            label: crate::localization::text(generate),
            action: ExplorerMenuAction::GenerateSql,
            enabled: true,
        },
    ];
    if join_path {
        entries.push(MenuEntry::ActionOwned {
            label: crate::localization::text("explorer.menus.ai_find_join_path"),
            action: ExplorerMenuAction::FindJoinPath,
            enabled: true,
        });
    }
    entries.push(MenuEntry::ActionOwned {
        label: crate::localization::text("explorer.menus.ai_pin_context"),
        action: ExplorerMenuAction::PinToContext,
        enabled: true,
    });
    localized_group("explorer.menus.ai_group", entries)
}

/// Menu label shared by the table/view submenu and the results export menu.
#[cfg(test)]
pub const GENERATE_MODEL_LABEL: &str = "Generate Model";
/// Menu label for the schema/database node's every-table variant.
#[cfg(test)]
pub const GENERATE_ALL_MODELS_LABEL: &str = "Generate Models for All Tables";

/// The Generate Model submenu for a table or view: one entry per language
/// opening a new tab, then a Copy to Clipboard submenu with the same
/// languages. Reads live column metadata, so it needs a connection.
fn generate_model_submenu(availability: &MenuAvailability) -> MenuEntry {
    let enabled = availability.has_connection;
    let languages = |destination: ScriptDestination| -> Vec<MenuEntry> {
        ModelLanguage::all()
            .iter()
            .map(|language| MenuEntry::ActionOwned {
                label: language.label(),
                action: ExplorerMenuAction::GenerateModel {
                    language: *language,
                    destination: destination.clone(),
                },
                enabled,
            })
            .collect()
    };
    let mut entries = languages(ScriptDestination::NewQuery);
    entries.push(MenuEntry::Separator);
    entries.push(MenuEntry::SubmenuOwned {
        label: crate::localization::text("explorer.menus.generate_model_clipboard_group"),
        enabled,
        entries: languages(ScriptDestination::Clipboard),
    });
    MenuEntry::SubmenuOwned {
        label: crate::localization::text("explorer.menus.generate_model_group"),
        enabled,
        entries,
    }
}

/// The every-table variant for schema and database nodes: one file, one
/// model per table, into a new tab.
fn generate_all_models_submenu(availability: &MenuAvailability) -> MenuEntry {
    let enabled = availability.has_connection;
    MenuEntry::SubmenuOwned {
        label: crate::localization::text("explorer.menus.generate_all_models_group"),
        enabled,
        entries: ModelLanguage::all()
            .iter()
            .map(|language| MenuEntry::ActionOwned {
                label: language.label(),
                action: ExplorerMenuAction::GenerateModelsForAll {
                    language: *language,
                },
                enabled,
            })
            .collect(),
    }
}

/// A single entry in the context menu.
#[derive(Clone, Debug)]
pub enum MenuEntry {
    Action {
        label: &'static str,
        action: ExplorerMenuAction,
        enabled: bool,
    },
    /// An action whose label is data (a user template's name), not a literal.
    ActionOwned {
        label: String,
        action: ExplorerMenuAction,
        enabled: bool,
    },
    SubmenuOwned {
        label: String,
        entries: Vec<MenuEntry>,
        enabled: bool,
    },
    Separator,
}

/// One user template offered under an object menu's Custom Actions submenu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomActionEntry {
    pub name: String,
    pub mode: sqlly_services::ExplorerActionMode,
}

/// Feature-availability flags that gate menu items. The tree renderer
/// populates these from its own state without performing any database I/O.
#[derive(Clone, Debug, Default)]
pub struct MenuAvailability {
    /// This node's own server has been observed connected (per-server state —
    /// any number of servers can be connected at once).
    pub is_connected: bool,
    /// This node is currently fetching children.
    pub is_loading: bool,
    /// Whether data-changing SQL may be offered for this node — gates DELETE
    /// all rows / TRUNCATE / DROP TABLE.
    ///
    /// This asks the connection's [`SafetyFeatures`], which is the whole point
    /// of the guardrails the product is sold on. It used to be spelled
    /// `self.active.is_some()` — "some server is active" — so a connection with
    /// SELECT-only enforced still offered DROP TABLE, and the safety model that
    /// is loaded, persisted, editable and unit-tested had no bearing on the one
    /// menu that could hurt someone.
    ///
    /// [`SafetyFeatures`]: crate::mutation_review::SafetyFeatures
    pub can_mutate: bool,
    /// Whether there is a connection to run *anything* against — gates the
    /// read-only actions (Select Top 1000, scripting, properties).
    ///
    /// Distinct from `can_mutate`: reading is not writing. The two were
    /// assigned the identical expression, which is how a SELECT-only connection
    /// came to offer DROP TABLE.
    pub has_connection: bool,
    /// The node's engine, so engine-specific submenus (Maintenance) list
    /// that engine's vocabulary — VACUUM on PostgreSQL, DBCC CHECKDB on SQL
    /// Server — instead of one generic list that half-fails.
    pub db: DatabaseType,
    /// User templates flagged as explorer actions for this node's kind,
    /// listed under the Custom Actions submenu. Empty when the node kind is
    /// not actionable or no templates qualify.
    pub custom_actions: Vec<CustomActionEntry>,
    /// The saved connections come from a relay proxy and are read-only:
    /// Duplicate / Delete / Add Server disable (Edit still opens the editor,
    /// which shows the profile read-only).
    pub connections_read_only: bool,
    /// Redis key browser: this node's listing stopped at its SCAN budget and
    /// can be continued (its note row holds a live cursor).
    pub redis_scan_more: bool,
    /// This node's server's named pin groups (id, name) in display order,
    /// listed under Pin to Group / Move to Group.
    pub pin_groups: Vec<(uuid::Uuid, String)>,
}
pub(crate) fn menu_entries_for_context(
    ctx: &ExplorerActionContext,
    availability: &MenuAvailability,
) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;

    // Every data-model object leads with Copy Name + Copy Qualified Name.
    let with_copy = |mut rest: Vec<MenuEntry>| -> Vec<MenuEntry> {
        let mut entries = copy_menu_entries();
        entries.append(&mut rest);
        entries
    };

    // Kinds with no qualified form and nothing to pin — servers, databases,
    // schemas, grouping folders, Azure nodes — still lead with Copy Name, so
    // every row in the tree can hand you its name.
    let with_copy_name = |mut rest: Vec<MenuEntry>| -> Vec<MenuEntry> {
        let mut entries = copy_name_only_entries();
        entries.append(&mut rest);
        entries
    };

    // A Redis connection's tree is the key browser: its databases, prefix
    // folders and keys get command menus; the server node keeps the
    // ordinary connection menu.
    if availability.db.is_command_engine() && !matches!(ctx.kind, NodeKind::Server { .. }) {
        return redis_menu_entries(ctx, availability);
    }

    let mut entries = match &ctx.kind {
        // The large menus lay out their own groups (Copy and Pin sit in the
        // footer with Refresh and Properties) so the actions people reach for
        // first lead: New Query on a database, Select Top 1000 on a table.
        NodeKind::Server { .. } => server_menu_entries(ctx, availability),
        NodeKind::Database(database) => {
            let hidden = ctx
                .profile
                .as_deref()
                .map_or(0, |p| p.hidden_schema_count(database));
            database_menu_entries(availability, hidden)
        }
        NodeKind::SchemaFolder { .. } => with_copy_name(schema_folder_menu_entries(availability)),
        NodeKind::Table { .. } => table_menu_entries(ctx, availability),
        NodeKind::View { .. } => view_menu_entries(&ctx.kind, availability),
        NodeKind::Procedure { .. } => procedure_menu_entries(&ctx.kind, availability),
        NodeKind::Function { .. } => function_menu_entries(&ctx.kind, availability),
        NodeKind::Column => with_copy(column_menu_entries()),
        NodeKind::Parameter { .. } => with_copy(parameter_menu_entries()),
        NodeKind::Index { .. } => with_copy(index_menu_entries(availability)),
        NodeKind::Statistic { .. } | NodeKind::Type { .. } | NodeKind::ForeignKey { .. } => {
            with_copy(child_object_menu_entries())
        }
        NodeKind::Trigger { .. }
        | NodeKind::Sequence { .. }
        | NodeKind::Synonym { .. }
        | NodeKind::Event { .. }
        | NodeKind::MaterializedView { .. }
        | NodeKind::Constraint { .. }
        | NodeKind::Extension { .. } => {
            with_copy(scriptable_leaf_menu_entries(&ctx.kind, availability))
        }
        NodeKind::TablesFolder
        | NodeKind::ViewsFolder
        | NodeKind::ProgrammabilityFolder
        | NodeKind::IndexesFolder { .. }
        | NodeKind::StatisticsFolder { .. }
        | NodeKind::ForeignKeysFolder { .. }
        | NodeKind::TypesFolder
        | NodeKind::TriggersFolder { .. }
        | NodeKind::ConstraintsFolder { .. }
        | NodeKind::DatabaseTriggersFolder
        | NodeKind::SequencesFolder
        | NodeKind::SynonymsFolder
        | NodeKind::EventsFolder
        | NodeKind::MaterializedViewsFolder => with_copy_name(folder_menu_entries(availability)),
        NodeKind::ExtensionsFolder => {
            let mut entries = with_copy_name(folder_menu_entries(availability));
            entries.insert(
                3,
                localized_action(
                    "explorer.menus.extensions_install",
                    A::InstallExtension,
                    availability.can_mutate,
                ),
            );
            entries
        }
        NodeKind::Error => with_copy_name(vec![localized_action(
            "explorer.menus.error_refresh",
            A::Refresh,
            true,
        )]),
        NodeKind::AzureAccount | NodeKind::AzureSubscription | NodeKind::AzureSqlServer => {
            with_copy_name(vec![localized_action(
                "explorer.menus.azure_refresh",
                A::Refresh,
                true,
            )])
        }
        NodeKind::AzureDatabase(_) => with_copy_name(vec![localized_action(
            "explorer.menus.azure_database_link_connect",
            A::Connect,
            true,
        )]),
        NodeKind::PinnedFolder { .. } => with_copy_name(vec![
            localized_action("explorer.menus.pin_group_rename", A::RenamePinGroup, true),
            localized_action("explorer.menus.pin_group_delete", A::DeletePinGroup, true),
        ]),
        NodeKind::Pinned { .. } => with_copy_name(vec![
            localized_action("explorer.menus.pinned_unpin", A::Unpin, true),
            localized_action("explorer.menus.pinned_move_up", A::MovePinnedUp, true),
            localized_action("explorer.menus.pinned_move_down", A::MovePinnedDown, true),
            pin_group_submenu("explorer.menus.move_to_group", availability),
        ]),
        // Group headers and anything else without its own menu: the name is
        // still copyable, so a bare Copy Name (no trailing separator) stands in.
        _ => copy_name_only_entries_trimmed(),
    };
    if let Some(schema) = flat_mode_object_schema(ctx, availability) {
        add_hide_object_schema(&mut entries, schema, availability);
    }
    insert_pin_to_group(&mut entries, availability);
    entries
}

/// Pin to Group / Move to Group: Ungrouped, the server's named groups, then
/// New Group….
fn pin_group_submenu(label_key: &'static str, availability: &MenuAvailability) -> MenuEntry {
    use ExplorerMenuAction as A;
    let mut entries = vec![localized_action(
        "explorer.menus.pin_group_ungrouped",
        A::PinToGroup { group: None },
        true,
    )];
    if !availability.pin_groups.is_empty() {
        entries.push(MenuEntry::Separator);
        entries.extend(
            availability
                .pin_groups
                .iter()
                .map(|(id, name)| MenuEntry::ActionOwned {
                    label: name.clone(),
                    action: A::PinToGroup { group: Some(*id) },
                    enabled: true,
                }),
        );
    }
    entries.push(MenuEntry::Separator);
    entries.push(localized_action(
        "explorer.menus.pin_group_new",
        A::NewPinGroup,
        true,
    ));
    localized_group(label_key, entries)
}

/// The small object menus lead with Copy Name / Copy Qualified Name /
/// Pin / Unpin; offer Pin to Group right after that Pin / Unpin. (The large
/// menus carry the same choices in their footer Pin submenu.)
fn insert_pin_to_group(entries: &mut Vec<MenuEntry>, availability: &MenuAvailability) {
    let at = entries.iter().position(|entry| {
        matches!(
            entry,
            MenuEntry::Action { action, .. } | MenuEntry::ActionOwned { action, .. }
                if *action == ExplorerMenuAction::ToggleFavorite
        )
    });
    if let Some(at) = at {
        entries.insert(
            at + 1,
            pin_group_submenu("explorer.menus.pin_to_group", availability),
        );
    }
}

/// The schema of a schema-qualified object listed in the flat `schema.name`
/// layout, where no schema node exists to right-click. `None` in the
/// schema-folders layout (the object's id carries a `:sch:` scope and the
/// schema node has its own Hide Schema), for objects without a schema, and
/// on engines without schemas.
fn flat_mode_object_schema<'a>(
    ctx: &'a ExplorerActionContext,
    availability: &MenuAvailability,
) -> Option<&'a str> {
    let schema = match &ctx.kind {
        NodeKind::Table { schema, .. }
        | NodeKind::View { schema, .. }
        | NodeKind::Procedure { schema, .. }
        | NodeKind::Function { schema, .. }
        | NodeKind::MaterializedView { schema, .. }
        | NodeKind::Type { schema, .. }
        | NodeKind::Sequence { schema, .. }
        | NodeKind::Synonym { schema, .. }
        | NodeKind::Event { schema, .. } => schema.as_str(),
        _ => return None,
    };
    let flat = !ctx.node_id.contains(":sch:");
    let has_schemas = availability
        .db
        .supports_feature(sqlly_lang::sqlmodel::DbFeature::Schemas);
    (flat && has_schemas && !schema.is_empty()).then_some(schema)
}

/// Hide Schema "<schema>" on a flat-layout object. It goes inside the
/// object's Tools submenu (tables, views, procedures, functions), so the
/// top-level menu does not grow; the small leaf menus (types, sequences,
/// synonyms, events, materialized views) have no submenu, so there it sits
/// after Refresh, or at the end of the footer.
fn add_hide_object_schema(
    entries: &mut Vec<MenuEntry>,
    schema: &str,
    availability: &MenuAvailability,
) {
    let item = MenuEntry::ActionOwned {
        label: crate::localization::format(
            "explorer.menus.object_hide_schema",
            &[("schema", schema)],
        ),
        action: ExplorerMenuAction::HideSchema,
        enabled: !availability.connections_read_only,
    };
    if let Some(MenuEntry::SubmenuOwned { entries: tools, .. }) =
        entries.iter_mut().find(|e| is_tools_submenu(e))
    {
        if !matches!(tools.last(), Some(MenuEntry::Separator) | None) {
            tools.push(MenuEntry::Separator);
        }
        tools.push(item);
        return;
    }
    let after_refresh = entries.iter().position(|e| {
        matches!(
            e,
            MenuEntry::ActionOwned {
                action: ExplorerMenuAction::Refresh,
                ..
            }
        )
    });
    match after_refresh {
        Some(i) => entries.insert(i + 1, item),
        None => entries.push(item),
    }
}

/// An object menu's Tools submenu, recognized by what it holds (generate
/// model, maintenance, custom actions, the deploy cart) rather than by its
/// translated label.
fn is_tools_submenu(entry: &MenuEntry) -> bool {
    fn holds_tool_action(entries: &[MenuEntry]) -> bool {
        entries.iter().any(|e| match e {
            MenuEntry::Action { action, .. } | MenuEntry::ActionOwned { action, .. } => matches!(
                action,
                ExplorerMenuAction::GenerateModel { .. }
                    | ExplorerMenuAction::Maintenance { .. }
                    | ExplorerMenuAction::RunTemplate { .. }
                    | ExplorerMenuAction::AddToDeployCart { .. }
                    | ExplorerMenuAction::Tool(_)
            ),
            MenuEntry::SubmenuOwned { entries, .. } => holds_tool_action(entries),
            MenuEntry::Separator => false,
        })
    }
    matches!(entry, MenuEntry::SubmenuOwned { entries, .. } if holds_tool_action(entries))
}

pub(crate) fn server_menu_entries(
    ctx: &ExplorerActionContext,
    availability: &MenuAvailability,
) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    // The terminal route for this connection's engine: ready, the binary to
    // install, or the honest blocker — rendered the maintenance-menu way
    // (a disabled item carrying the reason).
    let (terminal_label, terminal_enabled) =
        crate::terminal_launch::menu_item(ctx.profile.as_deref(), availability.db);

    vec![
        localized_action("explorer.menus.server_open_query", A::OpenInNewTab, true),
        M::ActionOwned {
            label: terminal_label,
            action: A::OpenInTerminal,
            enabled: terminal_enabled,
        },
        localized_action(
            "explorer.menus.server_connect",
            A::Connect,
            !availability.is_connected,
        ),
        localized_action(
            "explorer.menus.server_disconnect",
            A::Disconnect,
            availability.is_connected,
        ),
        localized_action(
            "explorer.menus.server_connect_current_query",
            A::ConnectToCurrentQuery,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.server_refresh",
            A::Refresh,
            !availability.is_loading,
        ),
        M::Separator,
        localized_action(
            "explorer.menus.server_new_database",
            A::CreateDatabase,
            availability.is_connected && availability.can_mutate,
        ),
        localized_group(
            "explorer.menus.server_connection_group",
            vec![
                localized_action(
                    "explorer.menus.server_edit_connection",
                    A::EditConnection,
                    true,
                ),
                localized_action(
                    "explorer.menus.server_duplicate_connection",
                    A::DuplicateConnection,
                    !availability.connections_read_only,
                ),
                localized_action(
                    "explorer.menus.server_delete_connection",
                    A::DeleteConnection,
                    !availability.connections_read_only,
                ),
                M::Separator,
                localized_action(
                    "explorer.menus.server_add_server",
                    A::AddServer,
                    !availability.connections_read_only,
                ),
            ],
        ),
        tools_submenu(SERVER_TOOLS, availability),
        M::Separator,
        copy_name(),
        properties(),
    ]
}

pub(crate) fn schema_folder_menu_entries(availability: &MenuAvailability) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    vec![
        localized_action(
            "explorer.menus.schema_browse_objects",
            A::BrowseObjects,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.schema_export_tables",
            A::ExportTables,
            availability.has_connection,
        ),
        generate_all_models_submenu(availability),
        M::Separator,
        localized_action(
            "explorer.menus.schema_hide",
            A::HideSchema,
            !availability.connections_read_only,
        ),
        localized_action(
            "explorer.menus.schema_refresh",
            A::Refresh,
            !availability.is_loading,
        ),
    ]
}

/// The database menu. `hidden_schemas` is how many schemas this connection
/// hides from the explorer in the database; Show Hidden Schemas is offered
/// only when there are any.
pub(crate) fn database_menu_entries(
    availability: &MenuAvailability,
    hidden_schemas: usize,
) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use ExplorerTool as T;
    use MenuEntry as M;

    let tool = |t: ExplorerTool| M::ActionOwned {
        label: t.label(),
        action: A::Tool(t),
        enabled: availability.has_connection,
    };

    // Moving data in and out of the database.
    let mut data = vec![
        tool(T::ImportExport),
        localized_action(
            "explorer.menus.database_import_clipboard",
            A::ImportFromClipboard,
            availability.has_connection,
        ),
        M::Separator,
        localized_action(
            "explorer.menus.database_export_sql",
            A::ExportDatabaseSql,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.database_export_tables",
            A::ExportTables,
            availability.has_connection,
        ),
        M::Separator,
    ];
    #[cfg(not(target_arch = "wasm32"))]
    data.push(tool(T::DataTransfer));
    data.extend([
        tool(T::DataCompare),
        tool(T::TestDataGenerator),
        tool(T::BackupRestore),
    ]);

    // Designing and documenting the schema, and its history.
    let schema = vec![
        tool(T::Designers),
        tool(T::QueryBuilder),
        tool(T::RelationshipGraph),
        tool(T::DocumentSchema),
        M::Separator,
        tool(T::SchemaCompare),
        localized_action(
            "explorer.menus.database_save_snapshot",
            A::SaveSchemaSnapshot,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.database_schema_history",
            A::SchemaHistory,
            availability.has_connection,
        ),
        M::Separator,
        generate_all_models_submenu(availability),
    ];

    // Watching and administering it.
    let tools = vec![
        tool(T::ActivityMonitor),
        tool(T::QueryInsights),
        tool(T::DiskUsage),
        tool(T::IndexAnalyzer),
        tool(T::ExtendedEvents),
        tool(T::QueryHistory),
        M::Separator,
        tool(T::Security),
        tool(T::TenantScoping),
        M::Separator,
        maintenance_submenu(MaintenanceScope::Database, availability),
    ];

    let mut entries = vec![
        // Always first, on every engine (Redis included — see
        // `redis_menu_entries`): opening a query on the database is what a
        // right-click on one is for.
        localized_action(
            "explorer.menus.database_new_query",
            A::NewQuery,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.database_browse_objects",
            A::BrowseObjects,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.database_search_value",
            A::SearchDatabaseValue,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.database_run_sql_file",
            A::RunSqlFile,
            availability.has_connection,
        ),
        M::Separator,
        localized_group("explorer.menus.database_data_group", data),
        localized_group("explorer.menus.database_schema_group", schema),
        localized_group("explorer.menus.database_tools_group", tools),
        M::Separator,
        copy_name(),
        pin(availability),
        refresh(availability),
        localized_action(
            "explorer.menus.database_disconnect",
            A::Disconnect,
            availability.is_connected,
        ),
        properties(),
        M::Separator,
        localized_action(
            "explorer.menus.database_drop",
            A::DropDatabase,
            availability.can_mutate,
        ),
    ];
    if hidden_schemas > 0 {
        // Beside Refresh in the footer: it reshapes this database's listing.
        let at = entries
            .iter()
            .position(|e| {
                matches!(
                    e,
                    M::ActionOwned {
                        action: A::Refresh,
                        ..
                    }
                )
            })
            .map_or(entries.len(), |i| i + 1);
        entries.insert(
            at,
            M::ActionOwned {
                label: crate::localization::format(
                    "explorer.menus.database_show_hidden_schemas",
                    &[("count", &hidden_schemas.to_string())],
                ),
                action: A::ShowHiddenSchemas,
                enabled: !availability.connections_read_only,
            },
        );
    }
    entries
}

pub(crate) fn table_menu_entries(
    ctx: &ExplorerActionContext,
    availability: &MenuAvailability,
) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    let mut tools = vec![
        maintenance_submenu(MaintenanceScope::Table, availability),
        generate_model_submenu(availability),
    ];
    tools.extend(object_extras(&ctx.kind, availability));

    vec![
        localized_action(
            "explorer.menus.table_select_top",
            A::SelectTopRows { limit: 1000 },
            true,
        ),
        localized_action(
            "explorer.menus.table_view_data",
            A::ViewData,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.table_filter_data",
            A::FilterData,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.table_count_rows",
            A::CountRows,
            availability.has_connection,
        ),
        M::Separator,
        localized_action("explorer.menus.table_design_query", A::DesignQuery, true),
        localized_action(
            "explorer.menus.table_edit_structure",
            A::EditStructure,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.table_rename",
            A::Rename,
            availability.can_mutate,
        ),
        localized_group(
            "explorer.menus.table_delete_group",
            vec![
                localized_action(
                    "explorer.menus.table_delete_all_rows",
                    A::DeleteAllRows,
                    availability.can_mutate,
                ),
                localized_action(
                    "explorer.menus.table_truncate",
                    A::TruncateTable,
                    availability.can_mutate,
                ),
                M::Separator,
                localized_action(
                    "explorer.menus.table_drop",
                    A::DropTable,
                    availability.can_mutate,
                ),
            ],
        ),
        M::Separator,
        localized_group(
            "explorer.menus.table_script_group",
            vec![
                create_to_submenu(),
                localized_action("explorer.menus.table_script_alter", A::ScriptAlter, true),
                localized_action("explorer.menus.table_script_insert", A::ScriptInsert, true),
                localized_action("explorer.menus.table_script_merge", A::ScriptMerge, true),
            ],
        ),
        localized_group(
            "explorer.menus.table_data_group",
            vec![
                MenuEntry::SubmenuOwned {
                    label: crate::localization::text("explorer.menus.table_save_data_group"),
                    enabled: availability.has_connection,
                    entries: vec![
                        M::ActionOwned {
                            label: "CSV".to_string(),
                            action: A::SaveDataAs {
                                format: TableSaveFormat::Csv,
                            },
                            enabled: true,
                        },
                        M::ActionOwned {
                            label: "Excel".to_string(),
                            action: A::SaveDataAs {
                                format: TableSaveFormat::Excel,
                            },
                            enabled: true,
                        },
                        M::ActionOwned {
                            label: "SQLLY Export".to_string(),
                            action: A::SaveDataAs {
                                format: TableSaveFormat::SqllyExport,
                            },
                            enabled: true,
                        },
                        localized_action(
                            "explorer.menus.table_save_temp_import",
                            A::SaveDataAs {
                                format: TableSaveFormat::NewTempTableImport,
                            },
                            true,
                        ),
                        localized_action(
                            "explorer.menus.table_save_insert",
                            A::SaveDataAs {
                                format: TableSaveFormat::InsertInto,
                            },
                            true,
                        ),
                        localized_action(
                            "explorer.menus.table_save_update_insert",
                            A::SaveDataAs {
                                format: TableSaveFormat::UpdateOrInsert,
                            },
                            true,
                        ),
                    ],
                },
                localized_action(
                    "explorer.menus.table_transfer_data",
                    A::TransferDataTo,
                    availability.has_connection,
                ),
            ],
        ),
        localized_group(
            "explorer.menus.table_dependencies_group",
            vec![
                localized_action(
                    "explorer.menus.table_view_dependencies",
                    A::ViewDependencies,
                    availability.has_connection,
                ),
                localized_action(
                    "explorer.menus.table_find_usages",
                    A::FindUsages,
                    availability.has_connection,
                ),
                localized_action(
                    "explorer.menus.table_focus_relationships",
                    A::FocusInRelationships,
                    availability.has_connection,
                ),
            ],
        ),
        ai_submenu("table", true),
        localized_group("explorer.menus.table_tools_group", tools),
        M::Separator,
        copy_submenu(None),
        pin(availability),
        refresh(availability),
        properties(),
    ]
}

pub(crate) fn view_menu_entries(
    kind: &NodeKind,
    availability: &MenuAvailability,
) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    let mut tools = vec![generate_model_submenu(availability)];
    tools.extend(object_extras(kind, availability));

    vec![
        localized_action(
            "explorer.menus.view_select_top",
            A::SelectTopRows { limit: 1000 },
            true,
        ),
        localized_action(
            "explorer.menus.view_data",
            A::ViewData,
            availability.has_connection,
        ),
        M::Separator,
        localized_action("explorer.menus.view_design_query", A::DesignQuery, true),
        localized_action(
            "explorer.menus.view_edit_definition",
            A::EditDefinition,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.view_rename",
            A::Rename,
            availability.can_mutate,
        ),
        M::Separator,
        localized_group(
            "explorer.menus.view_script_group",
            vec![
                create_to_submenu(),
                localized_action(
                    "explorer.menus.view_script_alter",
                    A::ScriptAlter,
                    availability.has_connection,
                ),
                localized_action(
                    "explorer.menus.view_script_drop",
                    A::ScriptDrop,
                    availability.can_mutate,
                ),
            ],
        ),
        dependencies_submenu(availability.has_connection),
        ai_submenu("view", false),
        localized_group("explorer.menus.view_tools_group", tools),
        M::Separator,
        copy_submenu(None),
        pin(availability),
        refresh(availability),
        properties(),
    ]
}

pub(crate) fn procedure_menu_entries(
    kind: &NodeKind,
    availability: &MenuAvailability,
) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    let tools = object_extras(kind, availability);
    let mut entries = vec![
        localized_action("explorer.menus.procedure_run", A::RunProcedure, true),
        M::Separator,
        localized_action(
            "explorer.menus.procedure_edit_definition",
            A::EditDefinition,
            true,
        ),
        localized_action("explorer.menus.procedure_rename", A::Rename, true),
        M::Separator,
        localized_group(
            "explorer.menus.procedure_script_group",
            vec![
                localized_action(
                    "explorer.menus.procedure_script_alter",
                    A::ScriptAlter,
                    true,
                ),
                localized_action("explorer.menus.procedure_script_drop", A::ScriptDrop, true),
            ],
        ),
        dependencies_submenu(true),
        ai_submenu("procedure", false),
    ];
    if !tools.is_empty() {
        entries.push(localized_group(
            "explorer.menus.procedure_tools_group",
            tools,
        ));
    }
    entries.extend([
        M::Separator,
        copy_submenu(Some(localized_action(
            "explorer.menus.procedure_copy_exec",
            A::CopyInvocation,
            true,
        ))),
        pin(availability),
        properties(),
    ]);
    entries
}

pub(crate) fn function_menu_entries(
    kind: &NodeKind,
    availability: &MenuAvailability,
) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    let tools = object_extras(kind, availability);
    let mut entries = vec![
        localized_action("explorer.menus.function_execute", A::RunProcedure, true),
        M::Separator,
        localized_action(
            "explorer.menus.function_edit_definition",
            A::EditDefinition,
            true,
        ),
        localized_action("explorer.menus.function_rename", A::Rename, true),
        M::Separator,
        localized_group(
            "explorer.menus.function_script_group",
            vec![
                localized_action("explorer.menus.function_script_alter", A::ScriptAlter, true),
                localized_action("explorer.menus.function_script_drop", A::ScriptDrop, true),
            ],
        ),
        dependencies_submenu(true),
        ai_submenu("function", false),
    ];
    if !tools.is_empty() {
        entries.push(localized_group(
            "explorer.menus.function_tools_group",
            tools,
        ));
    }
    entries.extend([
        M::Separator,
        copy_submenu(Some(localized_action(
            "explorer.menus.function_copy_invocation",
            A::CopyInvocation,
            true,
        ))),
        pin(availability),
        properties(),
    ]);
    entries
}

/// The Custom Actions and Deploy Cart submenus for tables, views, procedures,
/// and functions, listed under their Tools submenu. Empty when neither
/// applies — custom actions only appear when a user template targets the
/// node's kind.
fn object_extras(kind: &NodeKind, availability: &MenuAvailability) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    let mut out: Vec<MenuEntry> = Vec::new();
    let Some(object_kind) = super::explorer_object_kind(kind) else {
        return out;
    };
    if !availability.custom_actions.is_empty() {
        out.push(M::SubmenuOwned {
            label: crate::localization::text("explorer.menus.custom_actions_group"),
            enabled: true,
            entries: availability
                .custom_actions
                .iter()
                .map(|ca| M::ActionOwned {
                    label: custom_action_label(ca),
                    action: A::RunTemplate {
                        name: ca.name.clone(),
                    },
                    enabled: availability.has_connection,
                })
                .collect(),
        });
    }
    // The cart's script generation is native-only (the dump pipeline writes
    // files), so the browser preview never collects into a cart it can't open.
    #[cfg(not(target_arch = "wasm32"))]
    out.push(M::SubmenuOwned {
        label: crate::localization::text("explorer.menus.deploy_cart_group"),
        enabled: true,
        entries: vec![
            localized_action(
                "explorer.menus.deploy_cart_add",
                A::AddToDeployCart {
                    include_data: false,
                },
                true,
            ),
            localized_action(
                "explorer.menus.deploy_cart_add_data",
                A::AddToDeployCart { include_data: true },
                object_kind == sqlly_services::ExplorerObjectKind::Table,
            ),
            M::Separator,
            localized_action("explorer.menus.deploy_cart_open", A::OpenDeployCart, true),
        ],
    });
    out
}

/// A Custom Actions entry's label: Script-mode templates show their name
/// (they open a reviewed tab); Run-mode ones say so, since picking them
/// executes.
pub(crate) fn custom_action_label(entry: &CustomActionEntry) -> String {
    match entry.mode {
        sqlly_services::ExplorerActionMode::Script => entry.name.clone(),
        sqlly_services::ExplorerActionMode::Run => crate::localization::format(
            "explorer.menus.custom_action_run_suffix",
            &[("name", &entry.name)],
        ),
    }
}

/// Copy Name + Copy Qualified Name + a separator — prepended to every
/// data-model object's context menu so a table, view, procedure, function,
/// type, column, index, etc. can all be copied by name or fully qualified.
pub(crate) fn copy_menu_entries() -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    vec![
        localized_action("explorer.menus.copy_name", A::CopyName, true),
        localized_action(
            "explorer.menus.copy_qualified_name",
            A::CopyQualifiedName,
            true,
        ),
        localized_action("explorer.menus.pin_unpin", A::ToggleFavorite, true),
        M::Separator,
    ]
}

/// Copy Name + a separator — prepended to the menus of nodes that carry a name
/// but no qualified form and nothing to pin: servers, databases, schemas,
/// grouping folders, Azure nodes, error rows.
pub(crate) fn copy_name_only_entries() -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    vec![
        localized_action("explorer.menus.copy_name", A::CopyName, true),
        M::Separator,
    ]
}

/// Copy Name on its own, for nodes whose menu would otherwise be empty — no
/// trailing separator, so the menu doesn't end in a rule.
pub(crate) fn copy_name_only_entries_trimmed() -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;

    vec![localized_action(
        "explorer.menus.copy_name",
        A::CopyName,
        true,
    )]
}

pub(crate) fn column_menu_entries() -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;

    vec![localized_action(
        "explorer.menus.properties",
        A::Properties,
        true,
    )]
}

pub(crate) fn parameter_menu_entries() -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;

    vec![localized_action(
        "explorer.menus.properties",
        A::Properties,
        true,
    )]
}

/// An index: the engine's rebuild / reorganize maintenance, then Properties.
pub(crate) fn index_menu_entries(availability: &MenuAvailability) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    vec![
        maintenance_submenu(MaintenanceScope::Index, availability),
        M::Separator,
        localized_action("explorer.menus.properties", A::Properties, true),
    ]
}

/// Statistics, types, and foreign keys: leaf objects whose whole story lives
/// in the Properties dialog.
pub(crate) fn child_object_menu_entries() -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;

    vec![localized_action(
        "explorer.menus.properties",
        A::Properties,
        true,
    )]
}

pub(crate) fn scriptable_leaf_menu_entries(
    kind: &NodeKind,
    availability: &MenuAvailability,
) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;
    use MenuEntry as M;

    let mut entries = vec![
        localized_action(
            "explorer.menus.leaf_script_create",
            A::ScriptCreate,
            availability.has_connection,
        ),
        localized_action(
            "explorer.menus.leaf_script_drop",
            A::ScriptDrop,
            availability.can_mutate,
        ),
    ];
    match kind {
        NodeKind::Event { .. } => entries.push(localized_action(
            "explorer.menus.event_edit",
            A::EditEvent,
            availability.has_connection,
        )),
        NodeKind::MaterializedView { .. } => {
            entries.push(localized_action(
                "explorer.menus.materialized_view_data",
                A::ViewData,
                availability.has_connection,
            ));
            entries.push(localized_action(
                "explorer.menus.materialized_view_refresh",
                A::RefreshMaterializedView,
                availability.can_mutate,
            ));
        }
        _ => {}
    }
    entries.push(M::Separator);
    entries.push(localized_action(
        "explorer.menus.properties",
        A::Properties,
        true,
    ));
    entries
}

pub(crate) fn folder_menu_entries(availability: &MenuAvailability) -> Vec<MenuEntry> {
    use ExplorerMenuAction as A;

    vec![
        localized_action(
            "explorer.menus.refresh",
            A::Refresh,
            !availability.is_loading,
        ),
        localized_action("explorer.menus.properties", A::Properties, true),
    ]
}

/// Map a typed `ExplorerMenuAction` to the appropriate `SchemaTreeEvent`.
/// SQL-bearing table actions reuse existing events (`OpenSqlInNewTab`,
/// `ScriptTableAs`) so the app handler needs no changes for them. All
/// other actions go through `ExplorerActionRequested`.
/// T-SQL flavored; the menu path resolves the node's engine and calls
/// `action_to_event_for` instead.
#[cfg(test)]
pub(crate) fn action_to_event(
    action: &ExplorerMenuAction,
    ctx: &ExplorerActionContext,
) -> SchemaTreeEvent {
    action_to_event_for(sqlly_lang::sqlmodel::DatabaseType::SqlServer, action, ctx)
}

/// `action_to_event` with the node's concrete engine, so SQL-bearing actions
/// render with that engine's pattern equivalents (row limiting, quoting).
pub(crate) fn action_to_event_for(
    db: sqlly_lang::sqlmodel::DatabaseType,
    action: &ExplorerMenuAction,
    ctx: &ExplorerActionContext,
) -> SchemaTreeEvent {
    use crate::table_actions::{
        QualifiedTable, ScriptObjectKind, alter_table_scaffold_sql_for, count_rows_sql_for,
        create_table_ddl_for, delete_sql_for, drop_constraint_sql_for, drop_object_sql_for,
        select_top_rows_sql_for, truncate_sql_for, view_data_sql_for,
    };

    let schema = ctx.schema.clone().unwrap_or_default();
    let name = ctx.name.clone().unwrap_or_default();
    let qt = QualifiedTable::new(&name).with_schema(&schema);
    // Every SQL-bearing event carries the node's server/database so the app
    // retargets the connection before opening or running the SQL.
    let open_sql = |sql: String| SchemaTreeEvent::OpenSqlInNewTab {
        sql,
        server_id: ctx.server_id,
        database: ctx.database.clone(),
    };
    let script_as = |kind: TableScriptKind| SchemaTreeEvent::ScriptTableAs {
        schema: schema.clone(),
        name: name.clone(),
        kind,
        server_id: ctx.server_id,
        database: ctx.database.clone(),
    };

    match action {
        ExplorerMenuAction::SelectTopRows { limit } => {
            open_sql(select_top_rows_sql_for(db, *limit, &qt))
        }
        ExplorerMenuAction::DesignQuery => SchemaTreeEvent::DesignQuery {
            schema: schema.clone(),
            name: name.clone(),
            server_id: ctx.server_id,
            database: ctx.database.clone(),
        },
        ExplorerMenuAction::ViewData => open_sql(view_data_sql_for(db, &qt)),
        ExplorerMenuAction::CountRows => open_sql(count_rows_sql_for(db, &qt)),
        ExplorerMenuAction::ScriptInsert => script_as(TableScriptKind::Insert),
        ExplorerMenuAction::ScriptMerge => script_as(TableScriptKind::Merge),
        ExplorerMenuAction::CreateTo { destination: _ } => {
            open_sql(create_table_ddl_for(db, &qt, &ctx.columns))
        }
        ExplorerMenuAction::DeleteAllRows => open_sql(delete_sql_for(db, &qt)),
        ExplorerMenuAction::TruncateTable => open_sql(truncate_sql_for(db, &qt)),
        // Guarded script-first drop (review comment + IF EXISTS).
        ExplorerMenuAction::DropTable => {
            open_sql(drop_object_sql_for(db, ScriptObjectKind::Table, &qt))
        }
        ExplorerMenuAction::ScriptDrop => match &ctx.kind {
            NodeKind::Constraint { table, name, .. } => {
                let table_qt = QualifiedTable::new(table).with_schema(&schema);
                open_sql(drop_constraint_sql_for(db, &table_qt, name))
            }
            NodeKind::Extension { name, .. } => {
                let quoted = db.quote_identifier(name);
                open_sql(format!("DROP EXTENSION IF EXISTS {quoted};"))
            }
            _ => {
                let kind = script_object_kind(&ctx.kind).unwrap_or(ScriptObjectKind::Table);
                open_sql(drop_object_sql_for(db, kind, &qt))
            }
        },
        ExplorerMenuAction::RefreshMaterializedView => {
            let target = qt.schema_qualified_table_for(db);
            open_sql(format!(
                "-- Review, then run.\nREFRESH MATERIALIZED VIEW {target};"
            ))
        }
        ExplorerMenuAction::ScriptCreate
        | ExplorerMenuAction::InstallExtension
        | ExplorerMenuAction::EditEvent => SchemaTreeEvent::ExplorerActionRequested {
            context: ctx.clone(),
            action: action.clone(),
        },
        // Rename routes to the app's reviewed rename dialog (validated new
        // name + SQL preview) instead of scripting a `<new_name>` placeholder.
        ExplorerMenuAction::Rename => SchemaTreeEvent::ExplorerActionRequested {
            context: ctx.clone(),
            action: ExplorerMenuAction::Rename,
        },
        ExplorerMenuAction::CreateDatabase | ExplorerMenuAction::DropDatabase => {
            SchemaTreeEvent::ExplorerActionRequested {
                context: ctx.clone(),
                action: action.clone(),
            }
        }
        ExplorerMenuAction::TransferDataTo => SchemaTreeEvent::ExplorerActionRequested {
            context: ctx.clone(),
            action: ExplorerMenuAction::TransferDataTo,
        },
        ExplorerMenuAction::ExportDatabaseSql => SchemaTreeEvent::ExportDatabaseSql {
            server_id: ctx.server_id,
            database: ctx.database.clone(),
        },
        ExplorerMenuAction::ExportTables => SchemaTreeEvent::ExplorerActionRequested {
            context: ctx.clone(),
            action: ExplorerMenuAction::ExportTables,
        },
        ExplorerMenuAction::ImportFromClipboard => SchemaTreeEvent::ExplorerActionRequested {
            context: ctx.clone(),
            action: ExplorerMenuAction::ImportFromClipboard,
        },
        ExplorerMenuAction::RunSqlFile => SchemaTreeEvent::RunSqlFile {
            server_id: ctx.server_id,
            database: ctx.database.clone(),
        },
        ExplorerMenuAction::BrowseObjects => SchemaTreeEvent::BrowseObjects {
            server_id: ctx.server_id,
            database: ctx.database.clone(),
            schema: ctx.schema.clone(),
        },
        ExplorerMenuAction::SearchDatabaseValue => SchemaTreeEvent::SearchDatabaseValue {
            server_id: ctx.server_id,
            database: ctx.database.clone(),
        },
        // Tables have no stored definition — the ALTER scaffold is generated
        // synchronously here; definition-carrying objects fall through to the
        // app handler, which fetches the source first.
        ExplorerMenuAction::ScriptAlter
            if matches!(script_object_kind(&ctx.kind), Some(ScriptObjectKind::Table)) =>
        {
            open_sql(alter_table_scaffold_sql_for(db, &qt))
        }
        // The key browser's Open Value opens the key view; Open in Query
        // Tab runs the reader at once (the user picked the key, and a read
        // has nothing to review).
        ExplorerMenuAction::RedisOpenValue => match &ctx.kind {
            NodeKind::RedisKey { key, kind, .. } => SchemaTreeEvent::OpenRedisKey {
                key: key.clone(),
                kind: kind.clone(),
                server_id: ctx.server_id,
                database: ctx.database.clone(),
            },
            _ => SchemaTreeEvent::ExplorerActionRequested {
                context: ctx.clone(),
                action: action.clone(),
            },
        },
        ExplorerMenuAction::RedisOpenInQueryTab => match &ctx.kind {
            NodeKind::RedisKey { key, kind, .. } => SchemaTreeEvent::RunCommandInNewTab {
                sql: crate::schema_tree::redis::read_command(key, kind),
                title: crate::schema_tree::redis::read_tab_title(key, kind),
                server_id: ctx.server_id,
                database: ctx.database.clone(),
            },
            _ => SchemaTreeEvent::ExplorerActionRequested {
                context: ctx.clone(),
                action: action.clone(),
            },
        },
        // The memory analyzer opens its read-only report pane on the
        // right-clicked database; the dispatcher retargets first.
        ExplorerMenuAction::RedisMemoryAnalyzer | ExplorerMenuAction::RedisClusterTopology => {
            SchemaTreeEvent::ExplorerActionRequested {
                context: ctx.clone(),
                action: action.clone(),
            }
        }
        // Search indexes: Open runs FT.INFO at once (a read the user asked
        // for); the query, explain, profile and drop scripts open for
        // editing and review, and the drop confirms when run.
        ExplorerMenuAction::RedisIndexInfo
        | ExplorerMenuAction::RedisIndexQuery
        | ExplorerMenuAction::RedisIndexExplain
        | ExplorerMenuAction::RedisIndexProfile
        | ExplorerMenuAction::RedisIndexDrop => match &ctx.kind {
            NodeKind::RedisIndex { name: index } => {
                use crate::schema_tree::redis as r;
                match action {
                    ExplorerMenuAction::RedisIndexInfo => SchemaTreeEvent::RunCommandInNewTab {
                        sql: r::index_info_command(index),
                        title: None,
                        server_id: ctx.server_id,
                        database: ctx.database.clone(),
                    },
                    ExplorerMenuAction::RedisIndexQuery => open_sql(r::index_query_script(index)),
                    ExplorerMenuAction::RedisIndexExplain => {
                        open_sql(r::index_explain_script(index))
                    }
                    ExplorerMenuAction::RedisIndexProfile => {
                        open_sql(r::index_profile_script(index))
                    }
                    _ => open_sql(r::index_drop_script(index)),
                }
            }
            _ => SchemaTreeEvent::ExplorerActionRequested {
                context: ctx.clone(),
                action: action.clone(),
            },
        },
        _ => SchemaTreeEvent::ExplorerActionRequested {
            context: ctx.clone(),
            action: action.clone(),
        },
    }
}

/// Depth-first search for a schema object's node among LOADED nodes,
/// returning the id path from root to the match (ancestors first). When
/// `server_id` is set the search is confined to that server's subtree;
/// otherwise the whole forest is searched in order. Name matching is
/// case-insensitive; a `None` schema matches any schema.
pub(crate) fn find_object_path(
    roots: &[TreeNode],
    server_id: Option<Uuid>,
    schema: Option<&str>,
    name: &str,
) -> Option<Vec<String>> {
    pub(crate) fn matches(kind: &NodeKind, want_schema: Option<&str>, want_name: &str) -> bool {
        let (schema, name) = match kind {
            NodeKind::Table { schema, name }
            | NodeKind::View { schema, name }
            | NodeKind::Procedure { schema, name }
            | NodeKind::Function { schema, name } => (schema, name),
            _ => return false,
        };
        name.eq_ignore_ascii_case(want_name)
            && want_schema.is_none_or(|s| schema.eq_ignore_ascii_case(s))
    }
    pub(crate) fn walk(
        nodes: &[TreeNode],
        want_schema: Option<&str>,
        want_name: &str,
        path: &mut Vec<String>,
    ) -> bool {
        for node in nodes {
            path.push(node.id.clone());
            if matches(&node.kind, want_schema, want_name)
                || walk(&node.children, want_schema, want_name, path)
            {
                return true;
            }
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    if let Some(server_id) = server_id {
        pub(crate) fn find_server<'a>(
            nodes: &'a [TreeNode],
            id: Uuid,
            path: &mut Vec<String>,
        ) -> Option<&'a TreeNode> {
            for node in nodes {
                path.push(node.id.clone());
                if matches!(&node.kind, NodeKind::Server { id: sid } if *sid == id) {
                    return Some(node);
                }
                if let Some(found) = find_server(&node.children, id, path) {
                    return Some(found);
                }
                path.pop();
            }
            None
        }
        let server = find_server(roots, server_id, &mut path)?;
        if !walk(&server.children, schema, name, &mut path) {
            return None;
        }
    } else if !walk(roots, schema, name, &mut path) {
        return None;
    }
    Some(path)
}

/// Expand the node with `id` (wherever it sits) so its children render.
pub(crate) fn mutations_allowed(safety: crate::mutation_review::SafetyFeatures) -> bool {
    !safety.only_allow_select
}

/// Append the tree's [`MenuEntry`] list to a [`PopupMenu`] under
/// construction. Plain entries become items (disabled where availability
/// gating says so), separators pass through, and submenu entries become real
/// nested popup menus (a disabled submenu collapses to a single disabled
/// item). Activating an item emits the same [`SchemaTreeEvent`] the entry's
/// action maps to via [`action_to_event`], then closes the menu.
pub(crate) fn add_menu_entries(
    mut menu: PopupMenu,
    entries: &[MenuEntry],
    tree: &Entity<SchemaTree>,
    menu_ctx: &ExplorerActionContext,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    for entry in entries {
        match entry {
            MenuEntry::Separator => {
                menu = menu.separator();
            }
            MenuEntry::Action {
                label,
                action,
                enabled,
            } => {
                menu = menu.item(menu_action_item(
                    (*label).into(),
                    action.clone(),
                    *enabled,
                    tree,
                    menu_ctx,
                ));
            }
            MenuEntry::ActionOwned {
                label,
                action,
                enabled,
            } => {
                menu = menu.item(menu_action_item(
                    label.clone().into(),
                    action.clone(),
                    *enabled,
                    tree,
                    menu_ctx,
                ));
            }
            MenuEntry::SubmenuOwned {
                label,
                entries,
                enabled,
            } => {
                if !*enabled {
                    menu = menu.item(PopupMenuItem::new(label.clone()).disabled(true));
                } else {
                    let child_entries = entries.clone();
                    let tree = tree.clone();
                    let menu_ctx = menu_ctx.clone();
                    let label = label.clone();
                    menu = menu.submenu(label, window, cx, move |submenu, window, cx| {
                        add_menu_entries(submenu, &child_entries, &tree, &menu_ctx, window, cx)
                    });
                }
            }
        }
    }
    menu
}

/// One clickable menu item that maps its action to the same [`SchemaTreeEvent`]
/// the entry would emit and closes the menu — shared by the static-label and
/// owned-label entry forms (template names are runtime data).
fn menu_action_item(
    label: gpui::SharedString,
    action: ExplorerMenuAction,
    enabled: bool,
    tree: &Entity<SchemaTree>,
    menu_ctx: &ExplorerActionContext,
) -> PopupMenuItem {
    let menu_ctx = menu_ctx.clone();
    let tree = tree.clone();
    PopupMenuItem::new(label).disabled(!enabled).on_click(
        move |_ev: &gpui::ClickEvent, _window, app| {
            tree.update(app, |this, cx| {
                if matches!(action, ExplorerMenuAction::ToggleFavorite) {
                    this.toggle_favorite(&menu_ctx, cx);
                    this.close_context_menu(cx);
                    return;
                }
                // The key browser's Load more / Scan more act on the tree's
                // own retained listing; nothing for the app to do.
                if matches!(
                    action,
                    ExplorerMenuAction::RedisLoadMore | ExplorerMenuAction::RedisScanMore
                ) {
                    let scan = matches!(action, ExplorerMenuAction::RedisScanMore);
                    this.redis_extend_listing(&menu_ctx.node_id, scan, cx);
                    this.close_context_menu(cx);
                    return;
                }
                match &action {
                    ExplorerMenuAction::PinToGroup { group } => {
                        this.pin_to_group(&menu_ctx, *group, cx);
                        this.close_context_menu(cx);
                        return;
                    }
                    ExplorerMenuAction::NewPinGroup => {
                        if let Some(server_id) = menu_ctx.server_id {
                            cx.emit(SchemaTreeEvent::PromptPinGroupName {
                                server_id,
                                rename: None,
                                pin: this.favorite_for_ctx(&menu_ctx),
                            });
                        }
                        this.close_context_menu(cx);
                        return;
                    }
                    ExplorerMenuAction::RenamePinGroup | ExplorerMenuAction::DeletePinGroup => {
                        if let NodeKind::PinnedFolder { server_id, group } = &menu_ctx.kind {
                            if matches!(action, ExplorerMenuAction::DeletePinGroup) {
                                this.delete_pin_group(*group, cx);
                            } else if let Some(name) = this.pin_group_name(*group) {
                                cx.emit(SchemaTreeEvent::PromptPinGroupName {
                                    server_id: *server_id,
                                    rename: Some((*group, name)),
                                    pin: None,
                                });
                            }
                        }
                        this.close_context_menu(cx);
                        return;
                    }
                    _ => {}
                }
                if let NodeKind::Pinned { index } = &menu_ctx.kind
                    && let Some(server_id) = menu_ctx.server_id
                {
                    match action {
                        ExplorerMenuAction::Unpin => {
                            this.unpin_at(server_id, *index, cx);
                            this.close_context_menu(cx);
                            return;
                        }
                        ExplorerMenuAction::MovePinnedUp => {
                            this.move_pinned_at(server_id, *index, -1, cx);
                            this.close_context_menu(cx);
                            return;
                        }
                        ExplorerMenuAction::MovePinnedDown => {
                            this.move_pinned_at(server_id, *index, 1, cx);
                            this.close_context_menu(cx);
                            return;
                        }
                        _ => {}
                    }
                }
                let db = this.database_type_for(menu_ctx.server_id);
                cx.emit(action_to_event_for(db, &action, &menu_ctx));
                this.close_context_menu(cx);
            });
        },
    )
}
/// Expand the node with `id` (wherever it sits) so its children render.
pub(crate) fn expand_node_by_id(nodes: &mut [TreeNode], id: &str) -> bool {
    for node in nodes {
        if node.id == id {
            node.expanded = true;
            return true;
        }
        if expand_node_by_id(&mut node.children, id) {
            return true;
        }
    }
    false
}
