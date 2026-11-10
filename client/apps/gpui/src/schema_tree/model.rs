use super::load::LoadState;
use gpui::SharedString;
use sqlly_core::{AzureSqlDiscovery, ConnectionEnvironment, ConnectionProfile};
use uuid::Uuid;

use crate::entra_grouping::ServerGroupingMode;

/// Kind stored with a pinned explorer object. Defaults keep older
/// `explorer-favorites.json` files loading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PinnedKind {
    #[default]
    Table,
    View,
    Procedure,
    Function,
    Database,
    MaterializedView,
    Sequence,
    Synonym,
    Extension,
    Event,
    Trigger,
    Constraint,
    Index,
    Type,
}

/// One favorited schema object, persisted across sessions.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FavoriteObject {
    pub server_id: Uuid,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub schema: Option<String>,
    pub name: String,
    #[serde(default)]
    pub kind: PinnedKind,
    #[serde(default)]
    pub order: u32,
    /// Named pin group this object belongs to; `None` is the ungrouped
    /// default (every pin saved before groups existed loads there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<Uuid>,
}

pub const FAVORITES_CAP: usize = 50;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn favorites_path() -> std::path::PathBuf {
    let mut p = crate::preferences::preferences_path();
    p.push("explorer-favorites.json");
    p
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn load_favorites() -> Vec<FavoriteObject> {
    std::fs::read_to_string(favorites_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn load_favorites() -> Vec<FavoriteObject> {
    crate::host_storage::load_text(
        crate::host_storage::StorageArea::Data,
        "explorer-favorites.json",
    )
    .and_then(|text| serde_json::from_str(&text).ok())
    .unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn save_favorites(favorites: &[FavoriteObject]) {
    if let Ok(text) = serde_json::to_string_pretty(favorites) {
        let _ = std::fs::write(favorites_path(), text);
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn save_favorites(favorites: &[FavoriteObject]) {
    if let Ok(text) = serde_json::to_string_pretty(favorites) {
        let _ = crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            "explorer-favorites.json",
            &text,
        );
    }
}

pub(crate) fn same_pin(a: &FavoriteObject, b: &FavoriteObject) -> bool {
    a.server_id == b.server_id
        && a.database == b.database
        && a.schema == b.schema
        && a.name == b.name
        && a.kind == b.kind
}

pub(crate) fn renumber_pins(favorites: &mut [FavoriteObject]) {
    for (i, favorite) in favorites.iter_mut().enumerate() {
        favorite.order = i as u32;
    }
}

/// Pure toggle used by the tree: removes the favorite when present, else
/// appends it (bounded).
pub fn toggled_favorites(
    mut favorites: Vec<FavoriteObject>,
    favorite: FavoriteObject,
) -> Vec<FavoriteObject> {
    let before = favorites.len();
    favorites.retain(|existing| !same_pin(existing, &favorite));
    if favorites.len() == before {
        let mut favorite = favorite;
        favorite.order = 0;
        favorites.insert(0, favorite);
        favorites.truncate(FAVORITES_CAP);
    }
    renumber_pins(&mut favorites);
    favorites
}

/// Move a pinned entry from `from` to `to` (clamped) and renumber `order`.
pub fn move_pinned(
    mut favorites: Vec<FavoriteObject>,
    from: usize,
    to: usize,
) -> Vec<FavoriteObject> {
    if favorites.is_empty() {
        return favorites;
    }
    let last = favorites.len() - 1;
    let from = from.min(last);
    let to = to.min(last);
    if from != to {
        let item = favorites.remove(from);
        favorites.insert(to, item);
    }
    renumber_pins(&mut favorites);
    favorites
}

/// Pinned objects for one server, sorted by `order`.
pub fn pinned_for_server(favorites: &[FavoriteObject], server_id: Uuid) -> Vec<&FavoriteObject> {
    let mut pins: Vec<&FavoriteObject> = favorites
        .iter()
        .filter(|f| f.server_id == server_id)
        .collect();
    pins.sort_by_key(|f| f.order);
    pins
}

/// Float pinned objects to the top of the folder they naturally live in — a
/// pinned table to the top of its Tables folder, a pinned database to the top
/// of the server's database list — while preserving the order of everything
/// else. This replaces the old server-top "Pinned (N)" group so a pin stays
/// under its real section instead of being hoisted to the server root.
pub(crate) fn sort_pinned_first(roots: &mut [TreeNode], favorites: &[FavoriteObject]) {
    for node in roots.iter_mut() {
        sort_pinned_first_node(node, favorites, None, None);
    }
}

/// Recursive worker: tracks the enclosing server id and database as it
/// descends, then stably sorts each node's children so pinned ones lead.
pub(crate) fn sort_pinned_first_node(
    node: &mut TreeNode,
    favorites: &[FavoriteObject],
    server_id: Option<Uuid>,
    database: Option<&str>,
) {
    let server_id = match node.kind {
        NodeKind::Server { id } => Some(id),
        _ => server_id,
    };
    let db_here: Option<String> = match &node.kind {
        NodeKind::Database(name) => Some(name.clone()),
        _ => database.map(str::to_string),
    };
    for child in node.children.iter_mut() {
        sort_pinned_first_node(child, favorites, server_id, db_here.as_deref());
    }
    if let Some(sid) = server_id {
        let db = db_here.as_deref();
        // `sort_by_key` is stable: pinned rows float up by their `order`,
        // everything else keeps its existing (alphabetical) order. Non-pinnable
        // children (folders, columns) all rank `u32::MAX`, so they never move.
        node.children
            .sort_by_key(|child| pin_rank(child, favorites, sid, db));
    }
}

/// A node's rank for pinned-first sorting: the favorite's `order` when the node
/// is pinned in this (server, database) scope, else `u32::MAX`.
fn pin_rank(
    node: &TreeNode,
    favorites: &[FavoriteObject],
    server_id: Uuid,
    database: Option<&str>,
) -> u32 {
    let Some((schema, name, kind)) = node_favorite_key(&node.kind) else {
        return u32::MAX;
    };
    // A pinned database's favorite carries no database; any other object
    // carries the database it lives in (mirrors `toggle_favorite`).
    let fav_db: Option<&str> = if kind == PinnedKind::Database {
        None
    } else {
        database
    };
    favorites
        .iter()
        .find(|f| {
            f.server_id == server_id
                && f.kind == kind
                && f.name == name
                && f.schema.as_deref() == schema.as_deref()
                && f.database.as_deref() == fav_db
        })
        .map(|f| f.order)
        .unwrap_or(u32::MAX)
}

/// Pull the `(schema, name, kind)` pin identity out of an object node, matching
/// the normalization `toggle_favorite` uses (empty schema → `None`). Returns
/// `None` for non-pinnable nodes (folders, columns, parameters, …).
fn node_favorite_key(kind: &NodeKind) -> Option<(Option<String>, String, PinnedKind)> {
    let pinned = pinned_kind_from_node(kind)?;
    let (schema, name) = match kind {
        NodeKind::Database(name) => (None, name.clone()),
        NodeKind::Extension { name, .. } => (None, name.clone()),
        NodeKind::Table { schema, name }
        | NodeKind::View { schema, name }
        | NodeKind::Procedure { schema, name }
        | NodeKind::Function { schema, name }
        | NodeKind::Type { schema, name }
        | NodeKind::Sequence { schema, name }
        | NodeKind::Event { schema, name }
        | NodeKind::MaterializedView { schema, name }
        | NodeKind::Synonym { schema, name, .. }
        | NodeKind::Index { schema, name, .. }
        | NodeKind::Trigger { schema, name, .. }
        | NodeKind::Constraint { schema, name, .. } => (Some(schema.clone()), name.clone()),
        _ => return None,
    };
    Some((schema.filter(|s| !s.is_empty()), name, pinned))
}

pub(crate) fn pinned_kind_from_node(kind: &NodeKind) -> Option<PinnedKind> {
    Some(match kind {
        NodeKind::Table { .. } => PinnedKind::Table,
        NodeKind::View { .. } => PinnedKind::View,
        NodeKind::Procedure { .. } => PinnedKind::Procedure,
        NodeKind::Function { .. } => PinnedKind::Function,
        NodeKind::Database(_) => PinnedKind::Database,
        NodeKind::MaterializedView { .. } => PinnedKind::MaterializedView,
        NodeKind::Sequence { .. } => PinnedKind::Sequence,
        NodeKind::Synonym { .. } => PinnedKind::Synonym,
        NodeKind::Extension { .. } => PinnedKind::Extension,
        NodeKind::Event { .. } => PinnedKind::Event,
        NodeKind::Trigger { .. } => PinnedKind::Trigger,
        NodeKind::Constraint { .. } => PinnedKind::Constraint,
        NodeKind::Index { .. } => PinnedKind::Index,
        NodeKind::Type { .. } => PinnedKind::Type,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConstraintKind {
    Check,
    Unique,
    Other,
}

#[derive(Clone, Debug)]
pub enum NodeKind {
    /// A saved connection profile shown as a top-level server. Expanding it
    /// connects on demand and lists databases.
    Server {
        id: Uuid,
    },
    /// A named pin group at the top of a server (see `pin_groups`). Pins
    /// left ungrouped are not listed here; they float to the top of their
    /// own folder instead ([`sort_pinned_first`]).
    PinnedFolder {
        server_id: Uuid,
        group: Uuid,
    },
    /// One pinned object inside a pin group; `index` is into the sorted
    /// per-server list ([`pinned_for_server`]).
    Pinned {
        index: usize,
    },
    /// A non-selectable grouping header (client / project / environment /
    /// server bucket) whose children are `Server` nodes.
    ///
    /// Carries the identity the grouped entity was given in the entity editor,
    /// so the one surface built to answer "which environment am I in?" can
    /// actually answer it. Held as the user's raw `#RRGGBB` rather than a
    /// resolved `Hsla`: the Live Resolution Rule forbids caching a resolved
    /// color across frames, and the tint depends on the active palette anyway.
    Group {
        color_hex: Option<String>,
        image_path: Option<String>,
    },
    Database(String),
    /// Groups the system databases (master, tempdb, model, msdb) under one
    /// folder when the Data Model preference asks for it. Children are
    /// preloaded `Database` nodes.
    SystemDatabasesFolder,
    /// Groups system schemas (sys, INFORMATION_SCHEMA, db_* roles, guest)
    /// under one folder when the Data Model preference asks for it. Children
    /// are preloaded `SchemaFolder` nodes.
    SystemSchemasFolder,
    /// Groups schemas that contain no objects under one folder when the Data
    /// Model preference asks for it. Children are preloaded `SchemaFolder`
    /// nodes.
    EmptySchemasFolder,
    /// One schema under a database when the schema-folders display mode is
    /// active. Children are the per-schema Tables/Views/Programmability
    /// folders.
    SchemaFolder {
        /// Schema scope for context-menu actions (e.g. Export Tables…).
        schema: String,
    },
    TablesFolder,
    ViewsFolder,
    ProgrammabilityFolder,
    /// "Indexes" folder under a table. Lazy-loads `Index` children; carries
    /// its parent table so the loader needs no id parsing.
    IndexesFolder {
        schema: String,
        table: String,
    },
    /// "Statistics" folder under a table. Lazy-loads `Statistic` children.
    StatisticsFolder {
        schema: String,
        table: String,
    },
    /// "Foreign Keys" folder under a table. Lazy-loads `ForeignKey` children —
    /// both the keys this table declares (references out) and the keys other
    /// tables point back at it with (references in).
    ForeignKeysFolder {
        schema: String,
        table: String,
    },
    /// "Types" folder under Programmability. Lazy-loads user-defined `Type`
    /// children; schema scope comes from the node id like its parent folder.
    TypesFolder,
    /// "Triggers" folder under a table. Lazy-loads `Trigger` children.
    TriggersFolder {
        schema: String,
        table: String,
    },
    /// "Constraints" folder under a table (check + unique; PK/FK have their own homes).
    ConstraintsFolder {
        schema: String,
        table: String,
    },
    /// Database-level folders; shown only on engines that have the concept.
    DatabaseTriggersFolder,
    SequencesFolder,
    SynonymsFolder,
    ExtensionsFolder,
    EventsFolder,
    MaterializedViewsFolder,
    Table {
        schema: String,
        name: String,
    },
    View {
        schema: String,
        name: String,
    },
    Procedure {
        schema: String,
        name: String,
    },
    Function {
        schema: String,
        name: String,
    },
    /// One index under a table's Indexes folder. A leaf: index detail lives
    /// in the Properties dialog, not the tree.
    Index {
        schema: String,
        table: String,
        name: String,
    },
    /// One statistics object under a table's Statistics folder. A leaf.
    Statistic {
        schema: String,
        table: String,
        name: String,
    },
    /// One foreign-key relationship under a table's Foreign Keys folder. A leaf;
    /// `outgoing` distinguishes a key this table declares (→) from one that
    /// references it (←).
    ForeignKey {
        outgoing: bool,
    },
    /// One user-defined type under the Types folder. A leaf.
    Type {
        schema: String,
        name: String,
    },
    Trigger {
        schema: String,
        table: Option<String>,
        name: String,
    },
    Constraint {
        schema: String,
        table: String,
        name: String,
        kind: ConstraintKind,
    },
    Sequence {
        schema: String,
        name: String,
    },
    Synonym {
        schema: String,
        name: String,
        base_object: String,
    },
    Extension {
        name: String,
        #[allow(dead_code)]
        version: String,
    },
    Event {
        schema: String,
        name: String,
    },
    MaterializedView {
        schema: String,
        name: String,
    },
    Column,
    Parameter {
        data_type: String,
        is_output: bool,
    },
    /// A synthetic node used to surface a load failure (e.g. database
    /// enumeration failed). It is never a real schema object, so it is excluded
    /// from `database_names_from_roots` and cannot be selected as a database.
    #[allow(dead_code)]
    Error,
    /// Discovered Azure Entra account root (from ARM discovery). Children are
    /// subscription nodes.
    AzureAccount,
    /// Discovered Azure subscription. Children are server nodes.
    AzureSubscription,
    /// Discovered Azure SQL server. Children are database nodes.
    AzureSqlServer,
    /// Discovered Azure SQL database. Carries the full discovery record so a
    /// click can link it as a saved connection profile. Boxed to keep the enum
    /// size small.
    AzureDatabase(Box<AzureSqlDiscovery>),
    /// Redis key browser: a folder of the keys sharing `prefix` (the
    /// connection's delimiter included, e.g. `user:`). Expanding it scans
    /// the prefix and groups the next segment.
    RedisKeyFolder {
        prefix: String,
    },
    /// Redis key browser: one key with its type (`string`, `hash`, `list`,
    /// `set`, `zset`, `stream`, `ReJSON-RL`, …) and TTL in seconds (`None`
    /// when it never expires).
    RedisKey {
        key: String,
        kind: String,
        ttl_seconds: Option<i64>,
    },
    /// Redis key browser: the listing's note row — the walk stopped at its
    /// key budget and/or only a window of the scanned rows is materialized.
    /// It carries the whole scanned listing so Load more and Scan more
    /// extend the parent in place. Never an object; nothing to expand.
    RedisNote {
        listing: std::sync::Arc<super::redis::RedisListing>,
    },
    /// Redis key browser: the "Indexes" folder under a database on a server
    /// with the search module (`FT._LIST` answered). Expanding it lists the
    /// indexes.
    RedisIndexesFolder,
    /// Redis key browser: one search index (`FT.INFO`, `FT.SEARCH`, …).
    RedisIndex {
        name: String,
    },
}

impl NodeKind {
    /// Catalog key of the fixed folder title this kind is always built with,
    /// so a language switch can re-title folders already in the tree.
    /// `None` for kinds titled by data (servers, objects, schemas) or with
    /// computed wording.
    pub(crate) fn localized_title_key(&self) -> Option<&'static str> {
        Some(match self {
            NodeKind::SystemDatabasesFolder => "explorer.schema_tree.system_databases_folder",
            NodeKind::SystemSchemasFolder => "explorer.schema_tree.system_schemas_folder",
            NodeKind::EmptySchemasFolder => "explorer.schema_tree.empty_schemas_folder",
            NodeKind::TablesFolder => "explorer.schema_tree.tables_folder",
            NodeKind::ViewsFolder => "explorer.schema_tree.views_folder",
            NodeKind::ProgrammabilityFolder => "explorer.schema_tree.programmability_folder",
            NodeKind::IndexesFolder { .. } => "explorer.schema_tree.indexes_folder",
            NodeKind::StatisticsFolder { .. } => "explorer.schema_tree.statistics_folder",
            NodeKind::ForeignKeysFolder { .. } => "explorer.schema_tree.foreign_keys_folder",
            NodeKind::TypesFolder => "explorer.schema_tree.types_folder",
            NodeKind::TriggersFolder { .. } => "explorer.schema_tree.table_triggers_folder",
            NodeKind::ConstraintsFolder { .. } => "explorer.schema_tree.constraints_folder",
            NodeKind::DatabaseTriggersFolder => "explorer.schema_tree.database_triggers_folder",
            NodeKind::SequencesFolder => "explorer.schema_tree.sequences_folder",
            NodeKind::SynonymsFolder => "explorer.schema_tree.synonyms_folder",
            NodeKind::ExtensionsFolder => "explorer.schema_tree.extensions_folder",
            NodeKind::EventsFolder => "explorer.schema_tree.events_folder",
            NodeKind::MaterializedViewsFolder => "explorer.schema_tree.materialized_views_folder",
            NodeKind::RedisIndexesFolder => "explorer.schema_redis.indexes_folder",
            _ => return None,
        })
    }

    /// Legacy short text marker for a node's kind. Since v0.42.0 normal rows
    /// never render these letters: the gutter draws only real SVG glyphs
    /// ([`NodeKind::glyph_icon`]) and iconless kinds contribute no gutter
    /// child at all. The sole remaining render-path consumer is the Azure
    /// account row's fallback when the Entra ID image fails to load. Kind
    /// information for iconless rows is carried by the folder labels, the row
    /// tooltip (full qualified name), and position in the hierarchy — do not
    /// reintroduce these letters into row rendering.
    pub(crate) fn icon(&self) -> &'static str {
        match self {
            NodeKind::Server { .. } => "SRV",
            NodeKind::PinnedFolder { .. } => "PIN",
            NodeKind::Pinned { .. } => "★",
            NodeKind::Group { .. } => "",
            NodeKind::Database(_) => "DB",
            NodeKind::SystemDatabasesFolder => "SYS",
            NodeKind::SystemSchemasFolder => "SYS",
            // Not "SYS". These are the user's own schemas that happen to be
            // empty; labelling them "system" was simply false.
            NodeKind::EmptySchemasFolder => "SCH",
            // A schema carries no gutter icon (per the object-explorer icon
            // scheme); its kind is spelled out in the folder label instead.
            NodeKind::SchemaFolder { .. } => "",
            NodeKind::TablesFolder => "T",
            NodeKind::ViewsFolder => "V",
            NodeKind::ProgrammabilityFolder => "P",
            NodeKind::IndexesFolder { .. } => "IX",
            NodeKind::StatisticsFolder { .. } => "ST",
            NodeKind::ForeignKeysFolder { .. } => "FK",
            NodeKind::TypesFolder => "TY",
            NodeKind::TriggersFolder { .. } => "",
            NodeKind::ConstraintsFolder { .. } => "",
            NodeKind::DatabaseTriggersFolder
            | NodeKind::SequencesFolder
            | NodeKind::SynonymsFolder
            | NodeKind::ExtensionsFolder
            | NodeKind::EventsFolder
            | NodeKind::MaterializedViewsFolder => "",
            NodeKind::Table { .. } => "T",
            NodeKind::View { .. } => "V",
            NodeKind::Procedure { .. } => "SP",
            NodeKind::Function { .. } => "FN",
            NodeKind::Index { .. } => "IX",
            NodeKind::Statistic { .. } => "ST",
            NodeKind::ForeignKey { outgoing } => {
                if *outgoing {
                    "\u{2192}" // → references out
                } else {
                    "\u{2190}" // ← referenced by
                }
            }
            NodeKind::Type { .. } => "TY",
            NodeKind::Trigger { .. } => "TR",
            NodeKind::Constraint { kind, .. } => match kind {
                ConstraintKind::Check => "CK",
                ConstraintKind::Unique => "UQ",
                ConstraintKind::Other => "CK",
            },
            NodeKind::Sequence { .. } => "SQ",
            NodeKind::Synonym { .. } => "SYN",
            NodeKind::Extension { .. } => "EXT",
            NodeKind::Event { .. } => "EV",
            NodeKind::MaterializedView { .. } => "MV",
            NodeKind::Column => "C",
            NodeKind::Parameter { .. } => "@",
            NodeKind::Error => "!",
            NodeKind::AzureAccount => "AZ",
            NodeKind::AzureSubscription => "SUB",
            NodeKind::AzureSqlServer => "SRV",
            NodeKind::AzureDatabase(_) => "DB",
            NodeKind::RedisKeyFolder { .. } => "KF",
            NodeKind::RedisKey { .. } => "K",
            NodeKind::RedisNote { .. } => "i",
            NodeKind::RedisIndexesFolder => "",
            NodeKind::RedisIndex { .. } => "IX",
        }
    }

    /// The SVG glyph a row draws in its gutter, when it has one. Object-explorer
    /// entity kinds carry drawn icons; folders keep no gutter glyph and spell
    /// their kind out in the label. Do not paint [`NodeKind::icon`] letter
    /// markers on normal rows.
    pub(crate) fn glyph_icon(&self) -> Option<&'static str> {
        match self {
            NodeKind::Server { .. } | NodeKind::AzureSqlServer => {
                Some(crate::icon_catalog::AppIcon::Server.path())
            }
            NodeKind::PinnedFolder { .. } | NodeKind::Pinned { .. } => {
                Some(crate::icon_catalog::AppIcon::Heart.path())
            }
            NodeKind::Database(_) | NodeKind::AzureDatabase(_) => {
                Some(crate::icon_catalog::AppIcon::Database.path())
            }
            NodeKind::Table { .. } => Some(crate::icon_catalog::AppIcon::Table.path()),
            NodeKind::View { .. } => Some(crate::icon_catalog::AppIcon::View.path()),
            NodeKind::Procedure { .. } => Some(crate::icon_catalog::AppIcon::Procedure.path()),
            NodeKind::Function { .. } => Some(crate::icon_catalog::AppIcon::Function.path()),
            NodeKind::Column => Some(crate::icon_catalog::AppIcon::Column.path()),
            NodeKind::Index { .. } => Some(crate::icon_catalog::AppIcon::Index.path()),
            NodeKind::Trigger { .. } => Some(crate::icon_catalog::AppIcon::Trigger.path()),
            NodeKind::Constraint { .. } => Some(crate::icon_catalog::AppIcon::Constraint.path()),
            NodeKind::Sequence { .. } => Some(crate::icon_catalog::AppIcon::Sequence.path()),
            NodeKind::Synonym { .. } => Some(crate::icon_catalog::AppIcon::Synonym.path()),
            NodeKind::Extension { .. } => Some(crate::icon_catalog::AppIcon::Extension.path()),
            NodeKind::Event { .. } => Some(crate::icon_catalog::AppIcon::Event.path()),
            NodeKind::MaterializedView { .. } => {
                Some(crate::icon_catalog::AppIcon::MaterializedView.path())
            }
            // One glyph per Redis value type, drawn from the catalog's
            // existing vocabulary: a scalar, a field/value table, ordered
            // and unordered collections, a ranked set, a log, a document.
            NodeKind::RedisKey { kind, .. } => Some(match kind.as_str() {
                "hash" => crate::icon_catalog::AppIcon::Table.path(),
                "list" => crate::icon_catalog::AppIcon::Sequence.path(),
                "set" => crate::icon_catalog::AppIcon::View.path(),
                "zset" => crate::icon_catalog::AppIcon::Statistics.path(),
                "stream" => crate::icon_catalog::AppIcon::History.path(),
                "ReJSON-RL" | "json" => crate::icon_catalog::AppIcon::Inspect.path(),
                "TSDB-TYPE" => crate::icon_catalog::AppIcon::Statistics.path(),
                _ => crate::icon_catalog::AppIcon::Column.path(),
            }),
            NodeKind::RedisNote { .. } => Some(crate::icon_catalog::AppIcon::Info.path()),
            NodeKind::RedisIndex { .. } => Some(crate::icon_catalog::AppIcon::Index.path()),
            _ => None,
        }
    }

    pub(crate) fn is_expandable(&self) -> bool {
        matches!(
            self,
            NodeKind::Server { .. }
                | NodeKind::PinnedFolder { .. }
                | NodeKind::Group { .. }
                | NodeKind::Database(_)
                | NodeKind::SystemDatabasesFolder
                | NodeKind::SystemSchemasFolder
                | NodeKind::EmptySchemasFolder
                | NodeKind::SchemaFolder { .. }
                | NodeKind::TablesFolder
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
                | NodeKind::ExtensionsFolder
                | NodeKind::EventsFolder
                | NodeKind::MaterializedViewsFolder
                | NodeKind::Table { .. }
                | NodeKind::View { .. }
                | NodeKind::MaterializedView { .. }
                | NodeKind::Procedure { .. }
                | NodeKind::Function { .. }
                | NodeKind::AzureAccount
                | NodeKind::AzureSubscription
                | NodeKind::AzureSqlServer
                | NodeKind::RedisKeyFolder { .. }
                | NodeKind::RedisIndexesFolder
        )
    }
}

/// Rebuilds a node's translated title or detail in the current language,
/// from language-neutral data the closure owns. Lets a language switch
/// re-word counts and transient notes without reloading the tree.
#[derive(Clone)]
pub(crate) struct LocalizedWording(std::sync::Arc<dyn Fn() -> String + Send + Sync>);

impl LocalizedWording {
    pub(crate) fn new(wording: impl Fn() -> String + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(wording))
    }

    pub(crate) fn render(&self) -> String {
        (self.0)()
    }
}

impl std::fmt::Debug for LocalizedWording {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalizedWording")
    }
}

#[derive(Clone, Debug)]
pub struct TreeNode {
    pub id: String,
    pub title: String,
    /// Lowercased `title`, computed when the node is created or renamed so
    /// filter matching does not allocate per node per frame.
    pub(crate) title_lower: String,
    /// Hover group name (`row:{id}`), built with the node so flatten/render
    /// clone it instead of formatting on every rebuild.
    pub(crate) row_group: SharedString,
    pub detail: Option<String>,
    /// Source of a translated `detail` (counts and the like), re-run on a
    /// language switch. `None` for data details (key types, TTLs).
    pub(crate) detail_wording: Option<LocalizedWording>,
    /// Source of a translated title that overrides the kind's own wording
    /// (a Redis note's transient "scanning more…" / failure text), re-run on
    /// a language switch so the transient state survives it.
    pub(crate) title_wording: Option<LocalizedWording>,
    pub kind: NodeKind,
    pub depth: usize,
    pub expanded: bool,
    pub load_state: LoadState,
    pub children: Vec<TreeNode>,
    /// The environment identity color for a saved-connection server node, as
    /// the user's raw `#RRGGBB` — resolved against the live palette at render
    /// (the Live Resolution Rule, same as `NodeKind::Group::color_hex`).
    /// `None` for non-server nodes and for Local/Unset environments, which
    /// the status bar also treats as unassigned.
    pub env_color_hex: Option<String>,
    /// Bumped whenever this node's children are invalidated or a fresh load
    /// starts. A load task captures it at spawn and discards its result if the
    /// value has moved on.
    ///
    /// Loads are fire-and-forget tasks that write back by node id, and nothing
    /// stopped two of them being in flight at once: refreshing a node while its
    /// first load was still running spawned a second, and whichever finished
    /// last won. On a slow server — exactly when someone reaches for refresh —
    /// that means the stale answer can land on top of the fresh one and be
    /// marked `Loaded`, with no way to tell.
    pub(crate) load_generation: u64,
}

impl TreeNode {
    pub(crate) fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        kind: NodeKind,
        depth: usize,
    ) -> Self {
        let id = id.into();
        let title = title.into();
        Self {
            row_group: SharedString::from(format!("row:{id}")),
            id,
            title_lower: title.to_lowercase(),
            title,
            detail: None,
            detail_wording: None,
            title_wording: None,
            kind,
            depth,
            expanded: false,
            load_state: LoadState::Unloaded,
            children: Vec::new(),
            env_color_hex: None,
            load_generation: 0,
        }
    }

    pub(crate) fn set_title(&mut self, title: impl Into<String>) {
        let title = title.into();
        self.title_lower = title.to_lowercase();
        self.title = title;
    }

    /// Set a translated detail that follows later language switches.
    pub(crate) fn set_localized_detail(
        &mut self,
        wording: impl Fn() -> String + Send + Sync + 'static,
    ) {
        let wording = LocalizedWording::new(wording);
        self.detail = Some(wording.render());
        self.detail_wording = Some(wording);
    }

    /// Set a translated title (overriding the kind's own wording) that
    /// follows later language switches.
    pub(crate) fn set_localized_title(
        &mut self,
        wording: impl Fn() -> String + Send + Sync + 'static,
    ) {
        let wording = LocalizedWording::new(wording);
        self.set_title(wording.render());
        self.title_wording = Some(wording);
    }

    /// Invalidate any load currently in flight for this node: its result will be
    /// dropped instead of overwriting whatever replaces it.
    pub(crate) fn invalidate_pending_load(&mut self) {
        self.load_generation = self.load_generation.wrapping_add(1);
    }
}

/// Re-title every fixed-wording node in `nodes` (collapsed subtrees
/// included) in the current language, updating the lowercase filter copy
/// with it, and re-word translated details. Returns whether anything changed.
pub(crate) fn relocalize_titles(nodes: &mut [TreeNode]) -> bool {
    let mut changed = false;
    for node in nodes.iter_mut() {
        let title = match (&node.title_wording, &node.kind) {
            (Some(wording), _) => Some(wording.render()),
            (None, NodeKind::RedisNote { listing }) => Some(super::redis::note_text(listing)),
            (None, kind) => kind.localized_title_key().map(crate::localization::text),
        };
        if let Some(title) = title
            && title != node.title
        {
            node.set_title(title);
            changed = true;
        }
        if let Some(wording) = &node.detail_wording {
            let detail = wording.render();
            if node.detail.as_ref() != Some(&detail) {
                node.detail = Some(detail);
                changed = true;
            }
        }
        changed |= relocalize_titles(&mut node.children);
    }
    changed
}

/// Lightweight context captured at right-click time. Contains only stable,
/// cheap-to-clone data — no subtree clones, no database handles.
/// Find a node by id (immutable borrow).
pub(crate) fn find_node<'a>(nodes: &'a [TreeNode], id: &str) -> Option<&'a TreeNode> {
    for node in nodes {
        if node.id == id {
            return Some(node);
        }
        if node.expanded
            && let Some(found) = find_node(&node.children, id)
        {
            return Some(found);
        }
    }
    None
}

/// Find a node by id (mutable borrow), walking only the visible tree.
///
/// Test-only since click handling stopped needing it: a server click now
/// toggles expansion and reads nothing back.
#[cfg(test)]
pub(crate) fn find_node_mut<'a>(nodes: &'a mut [TreeNode], id: &str) -> Option<&'a mut TreeNode> {
    for node in nodes.iter_mut() {
        if node.id == id {
            return Some(node);
        }
        if node.expanded
            && let Some(found) = find_node_mut(&mut node.children, id)
        {
            return Some(found);
        }
    }
    None
}

/// Find a node by id anywhere in the tree, whether or not its ancestors are
/// expanded.
///
/// [`find_node`] deliberately walks only the *visible* tree, which is right for
/// click handling. The filter, though, reveals matches under collapsed folders,
/// and the keyboard has to be able to act on every row the user can actually
/// see.
pub(crate) fn find_node_any<'a>(nodes: &'a [TreeNode], id: &str) -> Option<&'a TreeNode> {
    for node in nodes {
        if node.id == id {
            return Some(node);
        }
        if let Some(found) = find_node_any(&node.children, id) {
            return Some(found);
        }
    }
    None
}

/// [`find_node_any`], mutably. Used by the actions a user can now aim at a row
/// the *filter* revealed — expanding it, or retrying its failed load — which
/// [`find_node_mut`] cannot reach, because such a row's parent is still
/// collapsed underneath.
pub(crate) fn find_node_any_mut<'a>(
    nodes: &'a mut [TreeNode],
    id: &str,
) -> Option<&'a mut TreeNode> {
    for node in nodes.iter_mut() {
        if node.id == id {
            return Some(node);
        }
        if let Some(found) = find_node_any_mut(&mut node.children, id) {
            return Some(found);
        }
    }
    None
}

/// The id of `id`'s parent, or `None` when it is a root or absent. Used by the
/// left-arrow "step out" move.
pub(crate) fn parent_id_of(nodes: &[TreeNode], id: &str) -> Option<String> {
    for node in nodes {
        if node.children.iter().any(|c| c.id == id) {
            return Some(node.id.clone());
        }
        if let Some(found) = parent_id_of(&node.children, id) {
            return Some(found);
        }
    }
    None
}

/// Parse a column node's `detail` string (e.g. `"int, nullable"`,
/// `"nvarchar(50), not null"`, `"int, identity, nullable"`) into
/// `(data_type, nullable)`. The first `, `-separated segment is the
/// data type; "nullable"/"not null" later segments set nullability.
pub(crate) fn parse_column_detail(detail: Option<&str>) -> (String, bool) {
    let Some(d) = detail else {
        return (String::new(), true);
    };
    let parts: Vec<&str> = d.split(", ").collect();
    if parts.is_empty() {
        return (String::new(), true);
    }
    let data_type = parts[0].trim().to_string();
    let mut nullable = true;
    for suffix in &parts[1..] {
        match suffix.trim().to_lowercase().as_str() {
            "not null" => nullable = false,
            "nullable" => nullable = true,
            _ => {}
        }
    }
    (data_type, nullable)
}

/// Find a server node by id, descending through group headers regardless of
/// expansion state (server subtrees must be reachable for `database_names`
/// even when a group is collapsed).
pub(crate) fn find_server_node<'a>(nodes: &'a [TreeNode], id: &str) -> Option<&'a TreeNode> {
    for node in nodes {
        if node.id == id {
            return Some(node);
        }
        if matches!(node.kind, NodeKind::Group { .. })
            && let Some(found) = find_server_node(&node.children, id)
        {
            return Some(found);
        }
    }
    None
}

/// Mutable counterpart to `find_server_node` — descends through group
/// headers regardless of expansion state so a collapsed group does not
/// prevent reaching the server node for a refresh.
pub(crate) fn find_server_node_mut<'a>(
    nodes: &'a mut [TreeNode],
    id: &str,
) -> Option<&'a mut TreeNode> {
    for node in nodes.iter_mut() {
        if node.id == id {
            return Some(node);
        }
        if matches!(node.kind, NodeKind::Group { .. })
            && let Some(found) = find_server_node_mut(&mut node.children, id)
        {
            return Some(found);
        }
    }
    None
}

/// Drain every `Server` node (with its whole subtree) out of `nodes` into
/// `out`, keyed by node id. Used to preserve drill-down state across a
/// `rebuild_roots`. Recurses into group headers (servers live one level down
/// when grouped).
pub(crate) fn collect_server_nodes(
    nodes: &mut Vec<TreeNode>,
    out: &mut std::collections::HashMap<String, TreeNode>,
) {
    for mut node in std::mem::take(nodes) {
        if matches!(node.kind, NodeKind::Server { .. }) {
            out.insert(node.id.clone(), node);
        } else if matches!(node.kind, NodeKind::Group { .. }) {
            collect_server_nodes(&mut node.children, out);
        }
    }
}

/// Build (or resurrect) a server root node for `profile` at `depth`. If a
/// preserved subtree exists for this server, it is reused with its depths
/// reassigned relative to the new position; otherwise a fresh collapsed node.
/// The environment identity color a saved-connection server node should wear,
/// looked up by the same case-insensitive name match the status bar's badges
/// and the group headers use. Local/Unset read as "unassigned" everywhere
/// else in the app, so they read that way here too.
pub(crate) fn env_color_for(
    profile: &ConnectionProfile,
    entities: &GroupIdentities,
) -> Option<String> {
    match profile.environment {
        ConnectionEnvironment::Local | ConnectionEnvironment::Unset => None,
        env => {
            entities
                .identity(
                    ServerGroupingMode::ByEnvironment,
                    crate::status_bar::environment_label(env),
                )
                .0
        }
    }
}

pub(crate) fn make_server_node(
    profile: &ConnectionProfile,
    depth: usize,
    preserved: &mut std::collections::HashMap<String, TreeNode>,
    entities: &GroupIdentities,
) -> TreeNode {
    let id = format!("srv:{}", profile.id.0);
    let env_color_hex = env_color_for(profile, entities);
    if let Some(mut existing) = preserved.remove(&id) {
        existing.set_title(profile.name.clone());
        // The environment assignment can change between rebuilds; the
        // preserved subtree keeps its drill-down, not its stale color.
        existing.env_color_hex = env_color_hex;
        reassign_depth(&mut existing, depth);
        existing
    } else {
        let mut node = TreeNode::new(
            id,
            profile.name.clone(),
            NodeKind::Server { id: profile.id.0 },
            depth,
        );
        node.env_color_hex = env_color_hex;
        node
    }
}

/// Build the Azure discovery tree (account → subscription → server → database)
/// from a flat list of discoveries. Nodes are pre-populated (no async loading);
/// `load_state` is set to `Loaded` so `toggle_expand` does not try to fetch
/// children.
pub(crate) fn make_azure_tree(
    discoveries: &[AzureSqlDiscovery],
    base_depth: usize,
) -> Vec<TreeNode> {
    use std::collections::BTreeMap;

    // Group: account_id → subscription_id → server_id → discoveries
    let mut by_account: BTreeMap<
        String,
        BTreeMap<String, BTreeMap<String, Vec<&AzureSqlDiscovery>>>,
    > = BTreeMap::new();
    for d in discoveries {
        by_account
            .entry(d.account_id.clone())
            .or_default()
            .entry(d.subscription_id.clone())
            .or_default()
            .entry(d.server_id.clone())
            .or_default()
            .push(d);
    }

    let mut account_nodes = Vec::new();
    for (account_id, subs) in by_account {
        // Use the first discovery for this account to get the label fields.
        let first = subs
            .values()
            .flat_map(|s| s.values())
            .flat_map(|v| v.iter())
            .copied()
            .next();
        let (username, tenant_id) = match first {
            Some(d) => (d.username.clone(), d.tenant_id.clone()),
            None => continue,
        };

        let account_label = format!("{} ({})", username, tenant_id);
        let mut account_node = TreeNode::new(
            format!("azure-account:{}", account_id),
            account_label,
            NodeKind::AzureAccount,
            base_depth,
        );
        account_node.expanded = true;
        account_node.load_state = LoadState::Loaded;

        let mut sub_nodes = Vec::new();
        for (subscription_id, servers) in subs {
            let sub_label = first
                .and_then(|d| {
                    if d.subscription_id == subscription_id {
                        d.subscription_name.clone()
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    servers
                        .values()
                        .flat_map(|v| v.iter())
                        .copied()
                        .find(|d| d.subscription_id == subscription_id)
                        .and_then(|d| d.subscription_name.clone())
                });
            let sub_label = sub_label
                .filter(|name| !name.is_empty())
                .map(|name| format!("{} ({})", name, subscription_id))
                .unwrap_or_else(|| subscription_id.clone());

            let mut sub_node = TreeNode::new(
                format!("azure-subscription:{}:{}", account_id, subscription_id),
                sub_label,
                NodeKind::AzureSubscription,
                base_depth + 1,
            );
            sub_node.expanded = true;
            sub_node.load_state = LoadState::Loaded;

            let mut server_nodes = Vec::new();
            for (server_id, db_discoveries) in servers {
                let first_db = db_discoveries.first().copied();
                let (server_name, location) = match first_db {
                    Some(d) => (d.server_name.clone(), d.location.clone()),
                    None => continue,
                };
                let server_label = format!("{} ({})", server_name, location);
                let mut server_node = TreeNode::new(
                    format!("azure-server:{}", server_id),
                    server_label,
                    NodeKind::AzureSqlServer,
                    base_depth + 2,
                );
                server_node.expanded = true;
                server_node.load_state = LoadState::Loaded;

                let mut db_nodes = Vec::new();
                for d in db_discoveries {
                    let db_node = TreeNode::new(
                        format!("azure-database:{}", d.database_id),
                        d.database_name.clone(),
                        NodeKind::AzureDatabase(Box::new((*d).clone())),
                        base_depth + 3,
                    );
                    db_nodes.push(db_node);
                }
                server_node.children = db_nodes;
                server_nodes.push(server_node);
            }
            sub_node.children = server_nodes;
            sub_nodes.push(sub_node);
        }
        account_node.children = sub_nodes;
        account_nodes.push(account_node);
    }
    account_nodes
}

/// Recompute a subtree's depths so `node` sits at `depth` and each descendant
/// is one deeper. Needed because a server can move between depth 0 (flat) and
/// depth 1 (under a group header) when the grouping mode changes.
pub(crate) fn reassign_depth(node: &mut TreeNode, depth: usize) {
    node.depth = depth;
    for child in &mut node.children {
        reassign_depth(child, depth + 1);
    }
}

/// Expand a server node in place for `activate`, recording its id into
/// `to_load` when its databases still need loading.
pub(crate) fn expand_server_for_activate(server: &mut TreeNode, to_load: &mut Option<String>) {
    server.expanded = true;
    // A server whose last enumeration failed is retried by connecting to it:
    // the failure may have been transient (a cold engine, a proxy that was
    // still coming up), and leaving the node red with no children while the
    // query bar says "Connected" sent users hunting for Refresh.
    if matches!(
        server.load_state,
        LoadState::Unloaded | LoadState::Failed(_) | LoadState::PermissionDenied(_)
    ) {
        *to_load = Some(server.id.clone());
    }
}

/// Parse the server uuid out of any node id of the form `srv:{uuid}[:...]`.
pub(crate) fn server_uuid_from_id(id: &str) -> Option<Uuid> {
    id.strip_prefix("srv:")
        .and_then(|s| s.split(':').next())
        .and_then(|s| Uuid::parse_str(s).ok())
}

/// A flattened, render-ready view of one visible tree node. Carries the node's
/// real `id` so click handlers can look the node back up in the tree (the
/// previous synthetic `depth:title` id never matched the real ids like
/// `db:master:tables`, so nothing below the database level could expand).
#[derive(Clone, Default)]
pub struct GroupIdentities {
    pub(crate) clients: EntityIdentityMap,
    pub(crate) projects: EntityIdentityMap,
    pub(crate) environments: EntityIdentityMap,
}

/// Lowercased entity name → (color hex, image path).
pub(crate) type EntityIdentityMap =
    std::collections::HashMap<String, (Option<String>, Option<String>)>;

impl GroupIdentities {
    /// Build from an already-loaded store. The caller holds the single read.
    pub fn from_store(store: &crate::connection::ConnectionStore) -> Self {
        let map = |entries: Vec<(String, Option<String>, Option<String>)>| -> EntityIdentityMap {
            entries
                .into_iter()
                .map(|(name, color, image)| (name.to_lowercase(), (color, image)))
                .collect()
        };
        Self {
            clients: map(store
                .clients()
                .into_iter()
                .map(|e| (e.name, e.color_hex, e.image_path))
                .collect()),
            projects: map(store
                .projects()
                .into_iter()
                .map(|e| (e.name, e.color_hex, e.image_path))
                .collect()),
            environments: map(store
                .environments()
                .into_iter()
                .map(|e| (e.name, e.color_hex, e.image_path))
                .collect()),
        }
    }

    /// Record one entity's identity directly, for tests that need a populated
    /// map without a store on disk behind it.
    #[cfg(test)]
    pub fn with_entity(
        mut self,
        mode: ServerGroupingMode,
        name: &str,
        color: Option<&str>,
        image: Option<&str>,
    ) -> Self {
        let map = match mode {
            ServerGroupingMode::ByClient => &mut self.clients,
            ServerGroupingMode::ByProject => &mut self.projects,
            ServerGroupingMode::ByEnvironment => &mut self.environments,
            ServerGroupingMode::ByServer | ServerGroupingMode::None => return self,
        };
        map.insert(
            name.to_lowercase(),
            (color.map(str::to_string), image.map(str::to_string)),
        );
        self
    }

    /// The (color hex, image path) recorded for `label` under `mode`, matched
    /// case-insensitively. Public because the status bar paints the same
    /// identities on its client / project / environment chips.
    pub fn identity(
        &self,
        mode: ServerGroupingMode,
        label: &str,
    ) -> (Option<String>, Option<String>) {
        self.for_label(mode, label)
    }

    pub(crate) fn for_label(
        &self,
        mode: ServerGroupingMode,
        label: &str,
    ) -> (Option<String>, Option<String>) {
        let map = match mode {
            ServerGroupingMode::ByClient => &self.clients,
            ServerGroupingMode::ByProject => &self.projects,
            ServerGroupingMode::ByEnvironment => &self.environments,
            // A hostname bucket has no entity behind it, and `None` builds no
            // headers at all.
            ServerGroupingMode::ByServer | ServerGroupingMode::None => return (None, None),
        };
        map.get(&label.to_lowercase())
            .cloned()
            .unwrap_or((None, None))
    }
}

/// The text a row's tooltip carries.
///
/// Rows ellipsize to fit the rail, so this is the only place the full label is
/// guaranteed to be readable — and it carries the *qualified* name, because
/// `Accounting.Account` and `Accounting.AccountBalance` are the same pixels once
#[cfg(test)]
mod favorites_tests {
    use super::*;

    pub(crate) fn favorite(name: &str) -> FavoriteObject {
        FavoriteObject {
            server_id: Uuid::from_u128(7),
            database: Some("db".to_string()),
            schema: Some("dbo".to_string()),
            name: name.to_string(),
            kind: PinnedKind::Table,
            order: 0,
            group: None,
        }
    }

    #[test]
    fn pins_saved_before_groups_load_ungrouped_and_round_trip() {
        let legacy = r#"[{"server_id":"00000000-0000-0000-0000-000000000007","database":"db","schema":"dbo","name":"Orders","kind":"Table","order":0}]"#;
        let pins: Vec<FavoriteObject> = serde_json::from_str(legacy).expect("legacy list loads");
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].group, None);
        // An ungrouped pin is written exactly as before, so an older build
        // still reads the file.
        let text = serde_json::to_string(&pins).unwrap();
        assert!(!text.contains("group"), "{text}");
        let mut grouped = pins[0].clone();
        grouped.group = Some(Uuid::from_u128(9));
        let text = serde_json::to_string(&[grouped.clone()]).unwrap();
        let back: Vec<FavoriteObject> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, vec![grouped]);
    }

    #[test]
    pub(crate) fn toggle_adds_then_removes_and_caps() {
        let favorites = toggled_favorites(Vec::new(), favorite("a"));
        assert_eq!(favorites.len(), 1);
        // Same object toggles off.
        let favorites = toggled_favorites(favorites, favorite("a"));
        assert!(favorites.is_empty());
        // Cap: newest stays, oldest falls off.
        let mut favorites = Vec::new();
        for i in 0..(FAVORITES_CAP + 5) {
            favorites = toggled_favorites(favorites, favorite(&format!("t{i}")));
        }
        assert_eq!(favorites.len(), FAVORITES_CAP);
        assert_eq!(favorites[0].name, format!("t{}", FAVORITES_CAP + 4));
        assert_eq!(favorites[0].order, 0);
        assert_eq!(
            favorites[FAVORITES_CAP - 1].order,
            (FAVORITES_CAP - 1) as u32
        );
    }

    #[test]
    fn move_pinned_reorders_and_renumbers() {
        let mut favorites = vec![favorite("a"), favorite("b"), favorite("c")];
        for (i, f) in favorites.iter_mut().enumerate() {
            f.order = i as u32;
        }
        let favorites = move_pinned(favorites, 0, 2);
        assert_eq!(
            favorites
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "c", "a"]
        );
        assert_eq!(
            favorites.iter().map(|f| f.order).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn move_pinned_clamps_out_of_range() {
        let favorites = vec![favorite("a"), favorite("b")];
        let moved = move_pinned(favorites.clone(), 99, 0);
        assert_eq!(moved[0].name, "b");
        assert_eq!(moved[1].name, "a");
        let same = move_pinned(favorites, 0, 0);
        assert_eq!(same[0].name, "a");
        assert_eq!(same[1].name, "b");
    }

    #[test]
    fn pinned_for_server_filters_and_sorts_by_order() {
        let server_a = Uuid::from_u128(7);
        let server_b = Uuid::from_u128(8);
        let mut a1 = favorite("first");
        a1.order = 2;
        let mut a2 = favorite("second");
        a2.order = 0;
        let mut other = favorite("other");
        other.server_id = server_b;
        other.order = 0;
        let list = [a1, other, a2];
        let pins = pinned_for_server(&list, server_a);
        assert_eq!(
            pins.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            vec!["second", "first"]
        );
    }

    #[test]
    fn legacy_favorites_json_deserializes_with_default_kind_and_order() {
        let json = r#"[{"server_id":"00000000-0000-0000-0000-000000000007","name":"Orders"}]"#;
        let favorites: Vec<FavoriteObject> = serde_json::from_str(json).unwrap();
        assert_eq!(favorites.len(), 1);
        assert_eq!(favorites[0].name, "Orders");
        assert_eq!(favorites[0].kind, PinnedKind::Table);
        assert_eq!(favorites[0].order, 0);
        assert!(favorites[0].database.is_none());
        assert!(favorites[0].schema.is_none());
    }
}

#[cfg(test)]
mod relocalize_tests {
    use super::*;

    #[test]
    fn language_switch_retitles_folders_and_their_filter_copy() {
        let mut tables = TreeNode::new("srv:s:db:d:tables", "Tables", NodeKind::TablesFolder, 2);
        let mut table = TreeNode::new(
            "srv:s:db:d:t:dbo.Orders",
            "dbo.Orders",
            NodeKind::Table {
                schema: "dbo".into(),
                name: "Orders".into(),
            },
            3,
        );
        // A collapsed table's already-loaded folders are re-titled too.
        table.children = vec![TreeNode::new(
            "srv:s:db:d:t:dbo.Orders:indexes",
            "Indexes",
            NodeKind::IndexesFolder {
                schema: "dbo".into(),
                table: "Orders".into(),
            },
            4,
        )];
        tables.children = vec![table];
        let mut roots = vec![tables];

        crate::localization::activate(crate::localization::Language::Spanish, None).unwrap();
        let changed = relocalize_titles(&mut roots);
        let spanish_tables = crate::localization::text("explorer.schema_tree.tables_folder");
        let spanish_indexes = crate::localization::text("explorer.schema_tree.indexes_folder");
        crate::localization::activate(crate::localization::Language::English, None).unwrap();

        assert!(changed);
        assert_ne!(spanish_tables, "Tables");
        assert_eq!(roots[0].title, spanish_tables);
        assert_eq!(roots[0].title_lower, spanish_tables.to_lowercase());
        assert_eq!(roots[0].children[0].title, "dbo.Orders");
        assert_eq!(roots[0].children[0].children[0].title, spanish_indexes);

        assert!(relocalize_titles(&mut roots));
        assert_eq!(roots[0].title, "Tables");
        assert_eq!(roots[0].title_lower, "tables");
        assert!(!relocalize_titles(&mut roots), "a second pass is a no-op");
    }

    #[test]
    fn language_switch_rewords_details_and_keeps_transient_titles() {
        let mut folder = super::super::redis::indexes_folder_node("srv:x:db:0", 2, 3);
        let mut note = TreeNode::new("srv:x:db:0:note", "summary", NodeKind::TablesFolder, 3);
        note.set_localized_title(|| {
            crate::localization::text("explorer.schema_tree.scanning_more")
        });
        folder.children = vec![note];
        let mut roots = vec![folder];
        let english_detail = roots[0].detail.clone();

        crate::localization::activate(crate::localization::Language::Spanish, None).unwrap();
        let changed = relocalize_titles(&mut roots);
        let spanish_detail =
            crate::localization::format("explorer.schema_redis.indexes_count", &[("count", "3")]);
        let spanish_scanning = crate::localization::text("explorer.schema_tree.scanning_more");
        crate::localization::activate(crate::localization::Language::English, None).unwrap();

        assert!(changed);
        assert_ne!(english_detail.as_deref(), Some(spanish_detail.as_str()));
        assert_eq!(roots[0].detail.as_deref(), Some(spanish_detail.as_str()));
        assert_eq!(
            roots[0].children[0].title, spanish_scanning,
            "the transient note is re-worded, not reset to its kind's wording"
        );

        assert!(relocalize_titles(&mut roots));
        assert_eq!(roots[0].detail, english_detail);
        assert_eq!(roots[0].children[0].title, "Scanning more…");
    }
}
