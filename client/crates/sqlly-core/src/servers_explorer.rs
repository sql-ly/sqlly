//! Unified Servers / Object Explorer tree.
//!
//! The Servers Explorer is the central feature of the studio. It folds the three
//! distinct connection *source types* into a single navigable tree:
//!
//! 1. **Entra ID accounts** — top-level groups keyed by username, then
//!    subscription → server → database (from live Azure discovery). You can
//!    connect to any discovered database.
//! 2. **Connections** — manually-registered SQL Server profiles (from the
//!    registered-server store), optionally with a Recent Connections shortlist.
//! 3. **Filesystem Databases** — data models built from `.sql` files on disk
//!    (from [`sqlly_lang::fsmodel`]), with stale / parse-error markers.
//!
//! Each node carries an [`ExplorerNodeSource`] so the UI can render the right
//! affordances, badges, and context actions per source. The builder is a pure
//! function over its inputs so it is cheap to unit-test and to call off the UI
//! thread (per AGENTS.md performance guidance).

use crate::connection::ConnectionProfile;
use crate::connection_display::{
    auth_key, azure_database_badges, azure_sort_key, environment_label, environment_rank,
};
use crate::registered_servers::RegisteredServerStore;
use serde::{Deserialize, Serialize};
use sqlly_lang::fsmodel::{FilesystemDatabaseProfile, SchemaSqlNode, SqlModelSnapshot};
use std::collections::BTreeSet;
use uuid::Uuid;

/// Stable identifier for a node in the unified explorer tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ServersExplorerNodeId(pub String);

impl ServersExplorerNodeId {
    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

/// Which of the three source types a node ultimately belongs to. Group/section
/// scaffolding nodes carry the source of their subtree so the UI can theme a
/// whole branch consistently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExplorerNodeSource {
    /// A manually-registered connection profile (the `Connections` group).
    ManualConnection,
    /// A signed-in Entra ID account (top of an account → … → database branch).
    EntraAccount,
    /// A server/database discovered through Azure ARM under an Entra account.
    AzureDiscovered,
    /// Live catalog metadata expanded from a connected server.
    LiveMetadata,
    /// An object sourced from the filesystem data model.
    FilesystemDatabase,
}

/// The kind of a node, used by the UI to pick an icon and the catalog of
/// applicable [`ExplorerAction`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ServersExplorerNodeKind {
    // Top-level source groups.
    EntraAccountsRoot,
    ConnectionsRoot,
    RecentConnectionsRoot,
    FilesystemDatabasesRoot,
    // Entra / Azure branch.
    EntraAccount,
    AzureSubscription,
    AzureServer,
    AzureDatabase,
    // Manual connections branch.
    Connection,
    // Filesystem branch.
    FilesystemDatabase,
    FilesystemSchema,
    FilesystemTable,
    FilesystemView,
    FilesystemProcedure,
    FilesystemFunction,
    FilesystemType,
    FilesystemColumn,
    // Live server metadata branch (shape only; expansion is transport-driven).
    ServerObjectsFolder,
}

/// Loading / health state of a node, surfaced as a spinner or marker in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ServersExplorerNodeState {
    #[default]
    Idle,
    Loading,
    Loaded,
    /// Filesystem source has unparsed / partially-parsed content.
    ParseError,
    /// Filesystem source is out of date relative to disk.
    Stale,
    /// Catalog query was denied; the node renders but does not break the tree.
    PermissionDenied,
}

/// Actions a node can offer in its context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ExplorerAction {
    NewQuery,
    Connect,
    Disconnect,
    SelectTopRows,
    EditRows,
    ScriptCreate,
    ScriptAlter,
    ScriptDrop,
    Properties,
    Rename,
    Delete,
    Refresh,
    RemoveProfile,
}

/// A node in the unified Servers Explorer tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServersExplorerNode {
    pub id: ServersExplorerNodeId,
    pub parent: Option<ServersExplorerNodeId>,
    pub label: String,
    pub kind: ServersExplorerNodeKind,
    pub source: ExplorerNodeSource,
    pub state: ServersExplorerNodeState,
    pub is_expandable: bool,
    pub can_connect: bool,
    pub badges: Vec<String>,
    pub actions: Vec<ExplorerAction>,
    /// Set when the node maps to a saved/linked connection profile.
    pub profile_id: Option<Uuid>,
    /// Entra account id, for account / azure-discovered nodes.
    pub account_id: Option<String>,
    /// ARM resource id (subscription/server/database) for azure nodes.
    pub resource_id: Option<String>,
}

/// The unified Servers Explorer tree. Nodes are stored flat; navigate with
/// [`children_of`](Self::children_of).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ServersExplorerTree {
    pub nodes: Vec<ServersExplorerNode>,
}

/// A filesystem database source paired with its current parsed model snapshot.
pub struct FilesystemDatabaseSource<'a> {
    pub profile: &'a FilesystemDatabaseProfile,
    pub snapshot: &'a SqlModelSnapshot,
    /// True when the on-disk content has changed since this snapshot was built.
    pub is_stale: bool,
}

/// Re-export the Azure discovery descriptor the explorer consumes so callers can
/// build the tree from one crate.
pub use crate::connection_management::AzureSqlDiscovery;

impl ServersExplorerTree {
    /// Builds the full unified tree from all three source types. Any source may
    /// be empty; empty top-level groups are still emitted so the UI can show an
    /// onboarding affordance under each.
    pub fn build(
        store: &RegisteredServerStore,
        recent_profile_ids: &[Uuid],
        discoveries: &[AzureSqlDiscovery],
        filesystem_sources: &[FilesystemDatabaseSource<'_>],
    ) -> Self {
        let mut tree = Self::default();
        tree.add_entra_accounts(discoveries);
        tree.add_connections(store, recent_profile_ids);
        tree.add_filesystem_databases(filesystem_sources);
        tree
    }

    pub fn children_of(&self, parent: Option<&ServersExplorerNodeId>) -> Vec<&ServersExplorerNode> {
        self.nodes
            .iter()
            .filter(|node| node.parent.as_ref() == parent)
            .collect()
    }

    pub fn find(&self, id: &ServersExplorerNodeId) -> Option<&ServersExplorerNode> {
        self.nodes.iter().find(|node| &node.id == id)
    }

    // ----- Entra / Azure -----

    fn add_entra_accounts(&mut self, discoveries: &[AzureSqlDiscovery]) {
        let root = ServersExplorerNodeId::new("entra-accounts");
        self.push(ServersExplorerNode::group(
            root.clone(),
            None,
            "Entra ID Accounts",
            ServersExplorerNodeKind::EntraAccountsRoot,
            ExplorerNodeSource::EntraAccount,
        ));

        // Account grouping: by username, then tenant (same username across
        // tenants stays grouped but distinct), then subscription/server/db.
        let mut items: Vec<&AzureSqlDiscovery> = discoveries.iter().collect();
        items.sort_by_key(|a| azure_sort_key(a));

        let mut seen: BTreeSet<String> = BTreeSet::new();
        for d in &items {
            let account_id = ServersExplorerNodeId::new(format!("entra-account:{}", d.account_id));
            if seen.insert(account_id.0.clone()) {
                self.push(
                    ServersExplorerNode::group(
                        account_id.clone(),
                        Some(root.clone()),
                        &format!("{} ({})", d.username, d.tenant_id),
                        ServersExplorerNodeKind::EntraAccount,
                        ExplorerNodeSource::EntraAccount,
                    )
                    .with_account(d.account_id.clone())
                    .with_actions(vec![ExplorerAction::Refresh]),
                );
            }

            let sub_id = ServersExplorerNodeId::new(format!(
                "entra-sub:{}:{}",
                d.account_id, d.subscription_id
            ));
            if seen.insert(sub_id.0.clone()) {
                let label = d
                    .subscription_name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| d.subscription_id.clone());
                self.push(
                    ServersExplorerNode::group(
                        sub_id.clone(),
                        Some(account_id.clone()),
                        &label,
                        ServersExplorerNodeKind::AzureSubscription,
                        ExplorerNodeSource::AzureDiscovered,
                    )
                    .with_account(d.account_id.clone())
                    .with_resource(d.subscription_id.clone())
                    .with_actions(vec![ExplorerAction::Refresh]),
                );
            }

            let server_id = ServersExplorerNodeId::new(format!("entra-server:{}", d.server_id));
            if seen.insert(server_id.0.clone()) {
                self.push(
                    ServersExplorerNode::group(
                        server_id.clone(),
                        Some(sub_id.clone()),
                        &format!("{} ({})", d.server_name, d.location),
                        ServersExplorerNodeKind::AzureServer,
                        ExplorerNodeSource::AzureDiscovered,
                    )
                    .with_account(d.account_id.clone())
                    .with_resource(d.server_id.clone())
                    .with_actions(vec![ExplorerAction::Refresh]),
                );
            }

            let db_badges = azure_database_badges(d);
            self.push(ServersExplorerNode {
                id: ServersExplorerNodeId::new(format!("entra-db:{}", d.database_id)),
                parent: Some(server_id.clone()),
                label: d.database_name.clone(),
                kind: ServersExplorerNodeKind::AzureDatabase,
                source: ExplorerNodeSource::AzureDiscovered,
                state: ServersExplorerNodeState::Idle,
                is_expandable: false,
                can_connect: true,
                badges: db_badges,
                actions: vec![
                    ExplorerAction::Connect,
                    ExplorerAction::NewQuery,
                    ExplorerAction::Properties,
                ],
                profile_id: d.linked_profile_id,
                account_id: Some(d.account_id.clone()),
                resource_id: Some(d.database_id.clone()),
            });
        }
    }

    // ----- Manual connections -----

    fn add_connections(&mut self, store: &RegisteredServerStore, recent: &[Uuid]) {
        let root = ServersExplorerNodeId::new("connections");
        self.push(ServersExplorerNode::group(
            root.clone(),
            None,
            "Connections",
            ServersExplorerNodeKind::ConnectionsRoot,
            ExplorerNodeSource::ManualConnection,
        ));

        let mut servers: Vec<_> = store.servers().iter().collect();
        servers.sort_by(|a, b| {
            sql_sort_key(&a.profile, &a.display_name)
                .cmp(&sql_sort_key(&b.profile, &b.display_name))
        });

        for registered in &servers {
            self.push(connection_node(
                Some(root.clone()),
                &format!("conn:{}", registered.id),
                &registered.display_name,
                &registered.profile,
                Some(registered.id),
            ));
        }

        // Optional Recent Connections shortlist (only when there is history).
        if !recent.is_empty() {
            let recent_root = ServersExplorerNodeId::new("recent-connections");
            self.push(ServersExplorerNode::group(
                recent_root.clone(),
                None,
                "Recent Connections",
                ServersExplorerNodeKind::RecentConnectionsRoot,
                ExplorerNodeSource::ManualConnection,
            ));
            for id in recent {
                if let Some(registered) = servers.iter().find(|s| &s.id == id) {
                    self.push(connection_node(
                        Some(recent_root.clone()),
                        &format!("recent:{}", registered.id),
                        &registered.display_name,
                        &registered.profile,
                        Some(registered.id),
                    ));
                }
            }
        }
    }

    // ----- Filesystem databases -----

    fn add_filesystem_databases(&mut self, sources: &[FilesystemDatabaseSource<'_>]) {
        let root = ServersExplorerNodeId::new("filesystem-databases");
        self.push(ServersExplorerNode::group(
            root.clone(),
            None,
            "Filesystem Databases",
            ServersExplorerNodeKind::FilesystemDatabasesRoot,
            ExplorerNodeSource::FilesystemDatabase,
        ));

        for source in sources {
            let db_id = ServersExplorerNodeId::new(format!("fsdb:{}", source.profile.name));
            let parse_errors = source.snapshot.diagnostics.len();
            let mut badges = Vec::new();
            if let Some(env) = source.profile.environment.as_deref()
                && !env.is_empty()
            {
                badges.push(env.to_string());
            }
            if let Some(client) = source.profile.client.as_deref()
                && !client.is_empty()
            {
                badges.push(client.to_string());
            }
            let state = if source.is_stale {
                ServersExplorerNodeState::Stale
            } else if parse_errors > 0 {
                ServersExplorerNodeState::ParseError
            } else {
                ServersExplorerNodeState::Loaded
            };
            if source.is_stale {
                badges.push("Stale".to_string());
            }
            if parse_errors > 0 {
                badges.push(format!("{parse_errors} parse issue(s)"));
            }
            self.push(ServersExplorerNode {
                id: db_id.clone(),
                parent: Some(root.clone()),
                label: source.profile.name.clone(),
                kind: ServersExplorerNodeKind::FilesystemDatabase,
                source: ExplorerNodeSource::FilesystemDatabase,
                state,
                is_expandable: !source.snapshot.schemas.is_empty(),
                can_connect: false,
                badges,
                actions: vec![ExplorerAction::Refresh, ExplorerAction::NewQuery],
                profile_id: None,
                account_id: None,
                resource_id: None,
            });

            for schema in &source.snapshot.schemas {
                self.add_filesystem_schema(&db_id, &source.profile.name, schema);
            }
        }
    }

    fn add_filesystem_schema(
        &mut self,
        parent: &ServersExplorerNodeId,
        db_name: &str,
        schema: &SchemaSqlNode,
    ) {
        let schema_id =
            ServersExplorerNodeId::new(format!("fsschema:{}:{}", db_name, schema.node.name));
        self.push(ServersExplorerNode {
            id: schema_id.clone(),
            parent: Some(parent.clone()),
            label: schema.node.name.clone(),
            kind: ServersExplorerNodeKind::FilesystemSchema,
            source: ExplorerNodeSource::FilesystemDatabase,
            state: ServersExplorerNodeState::Loaded,
            is_expandable: true,
            can_connect: false,
            badges: Vec::new(),
            actions: vec![ExplorerAction::NewQuery],
            profile_id: None,
            account_id: None,
            resource_id: None,
        });

        for table in &schema.tables {
            self.add_fs_object(
                &schema_id,
                db_name,
                "table",
                &table.source_node.node.name,
                ServersExplorerNodeKind::FilesystemTable,
                true,
            );
        }
        for view in &schema.views {
            self.add_fs_object(
                &schema_id,
                db_name,
                "view",
                &view.source_node.node.name,
                ServersExplorerNodeKind::FilesystemView,
                false,
            );
        }
        for proc in &schema.procedures {
            self.add_fs_object(
                &schema_id,
                db_name,
                "proc",
                &proc.operation.source_node.node.name,
                ServersExplorerNodeKind::FilesystemProcedure,
                false,
            );
        }
        for func in &schema.functions {
            self.add_fs_object(
                &schema_id,
                db_name,
                "func",
                &func.operation.source_node.node.name,
                ServersExplorerNodeKind::FilesystemFunction,
                false,
            );
        }
        for ty in &schema.types {
            self.add_fs_object(
                &schema_id,
                db_name,
                "type",
                &ty.source_node.node.name,
                ServersExplorerNodeKind::FilesystemType,
                false,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn add_fs_object(
        &mut self,
        parent: &ServersExplorerNodeId,
        db_name: &str,
        prefix: &str,
        name: &str,
        kind: ServersExplorerNodeKind,
        expandable: bool,
    ) {
        let mut actions = vec![ExplorerAction::NewQuery, ExplorerAction::ScriptCreate];
        if matches!(
            kind,
            ServersExplorerNodeKind::FilesystemTable | ServersExplorerNodeKind::FilesystemView
        ) {
            actions.insert(1, ExplorerAction::SelectTopRows);
        }
        self.push(ServersExplorerNode {
            id: ServersExplorerNodeId::new(format!("fs{prefix}:{db_name}:{}:{name}", parent.0)),
            parent: Some(parent.clone()),
            label: name.to_string(),
            kind,
            source: ExplorerNodeSource::FilesystemDatabase,
            state: ServersExplorerNodeState::Loaded,
            is_expandable: expandable,
            can_connect: false,
            badges: Vec::new(),
            actions,
            profile_id: None,
            account_id: None,
            resource_id: None,
        });
    }

    fn push(&mut self, node: ServersExplorerNode) {
        self.nodes.push(node);
    }
}

impl ServersExplorerNode {
    fn group(
        id: ServersExplorerNodeId,
        parent: Option<ServersExplorerNodeId>,
        label: &str,
        kind: ServersExplorerNodeKind,
        source: ExplorerNodeSource,
    ) -> Self {
        Self {
            id,
            parent,
            label: label.to_string(),
            kind,
            source,
            state: ServersExplorerNodeState::Idle,
            is_expandable: true,
            can_connect: false,
            badges: Vec::new(),
            actions: Vec::new(),
            profile_id: None,
            account_id: None,
            resource_id: None,
        }
    }

    fn with_account(mut self, account_id: String) -> Self {
        self.account_id = Some(account_id);
        self
    }

    fn with_resource(mut self, resource_id: String) -> Self {
        self.resource_id = Some(resource_id);
        self
    }

    fn with_actions(mut self, actions: Vec<ExplorerAction>) -> Self {
        self.actions = actions;
        self
    }
}

fn connection_node(
    parent: Option<ServersExplorerNodeId>,
    id: &str,
    display_name: &str,
    profile: &ConnectionProfile,
    profile_id: Option<Uuid>,
) -> ServersExplorerNode {
    ServersExplorerNode {
        id: ServersExplorerNodeId::new(id),
        parent,
        label: display_name.to_string(),
        kind: ServersExplorerNodeKind::Connection,
        source: ExplorerNodeSource::ManualConnection,
        state: ServersExplorerNodeState::Idle,
        is_expandable: true,
        can_connect: true,
        badges: connection_badges(profile),
        actions: vec![
            ExplorerAction::Connect,
            ExplorerAction::Disconnect,
            ExplorerAction::NewQuery,
            ExplorerAction::Properties,
            ExplorerAction::RemoveProfile,
        ],
        profile_id,
        account_id: None,
        resource_id: None,
    }
}

/// Connection-manager metadata (environment / project / client / safety flags)
/// surfaced as node badges.
fn connection_badges(profile: &ConnectionProfile) -> Vec<String> {
    let mut badges = vec![environment_label(profile.environment)];
    if let Some(client) = profile.client.as_deref().filter(|c| !c.trim().is_empty()) {
        badges.push(client.to_string());
    }
    if let Some(project) = profile.project.as_deref().filter(|p| !p.trim().is_empty()) {
        badges.push(project.to_string());
    }
    if profile.block_all_but_select {
        badges.push("SELECT only".to_string());
    }
    if profile.auto_wrap_rollback {
        badges.push("Rollback wrap".to_string());
    }
    badges
}

fn sql_sort_key(
    profile: &ConnectionProfile,
    display_name: &str,
) -> (u8, String, String, String, String, String) {
    (
        environment_rank(profile.environment),
        profile.client.clone().unwrap_or_default(),
        profile.project.clone().unwrap_or_default(),
        profile.server.clone(),
        auth_key(&profile.authentication),
        display_name.to_string(),
    )
}

/// Filters loaded nodes by a free-text needle across label, source, and kind.
/// Empty needle returns every node (the caller decides whether to filter).
pub fn filter_nodes<'a>(
    nodes: &'a [ServersExplorerNode],
    query: &str,
) -> Vec<&'a ServersExplorerNode> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return nodes.iter().collect();
    }
    nodes
        .iter()
        .filter(|node| {
            node.label.to_lowercase().contains(&needle)
                || format!("{:?}", node.kind).to_lowercase().contains(&needle)
                || format!("{:?}", node.source)
                    .to_lowercase()
                    .contains(&needle)
                || node
                    .badges
                    .iter()
                    .any(|b| b.to_lowercase().contains(&needle))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::ConnectionEnvironment;
    use crate::{RegisteredServer, SecretProvider, SecretRef};
    use sqlly_lang::fsmodel::{
        FileSourceRef, FileSourcedSqlNode, NamedSqlNode, SqlModelDiagnostic, SqlNodeKind,
        TableSqlNode,
    };

    fn manual_profile(
        name: &str,
        server: &str,
        environment: ConnectionEnvironment,
    ) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            name,
            server,
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "secret".to_string(),
            },
        );
        profile.environment = environment;
        profile.client = Some("Contoso".to_string());
        profile.project = Some("Billing".to_string());
        profile
    }

    fn discovery(account: &str, username: &str, tenant: &str, db: &str) -> AzureSqlDiscovery {
        AzureSqlDiscovery {
            account_id: account.to_string(),
            username: username.to_string(),
            tenant_id: tenant.to_string(),
            subscription_id: "sub".to_string(),
            subscription_name: Some("Prod".to_string()),
            resource_group: "rg".to_string(),
            server_id: format!("/servers/{account}"),
            server_name: format!("srv-{account}"),
            server_fqdn: format!("srv-{account}.database.windows.net"),
            location: "eastus".to_string(),
            database_id: format!("/servers/{account}/databases/{db}"),
            database_name: db.to_string(),
            service_tier: Some("Standard".to_string()),
            status: Some("Online".to_string()),
            linked_profile_id: None,
        }
    }

    fn fs_snapshot(schema: &str, table: &str, diagnostics: usize) -> SqlModelSnapshot {
        let schema_node = NamedSqlNode::new(SqlNodeKind::Schema, None, None, schema);
        let table_node = NamedSqlNode::new(
            SqlNodeKind::Table,
            Some(schema.to_string()),
            Some(schema_node.id.clone()),
            table,
        );
        SqlModelSnapshot {
            schemas: vec![SchemaSqlNode {
                node: schema_node,
                tables: vec![TableSqlNode {
                    source_node: FileSourcedSqlNode {
                        node: table_node,
                        source: FileSourceRef {
                            source_file_name: "f.sql".into(),
                            source_file_path: "/f.sql".into(),
                            file_hash: "h".into(),
                        },
                    },
                    columns: vec![],
                }],
                views: vec![],
                functions: vec![],
                types: vec![],
                procedures: vec![],
            }],
            diagnostics: (0..diagnostics)
                .map(|i| SqlModelDiagnostic {
                    message: format!("issue {i}"),
                    span: None,
                })
                .collect(),
        }
    }

    #[test]
    fn build_emits_three_top_level_groups_even_when_empty() {
        let tree = ServersExplorerTree::build(&RegisteredServerStore::new(), &[], &[], &[]);
        let roots = tree.children_of(None);
        let kinds: Vec<_> = roots.iter().map(|n| n.kind).collect();
        assert!(kinds.contains(&ServersExplorerNodeKind::EntraAccountsRoot));
        assert!(kinds.contains(&ServersExplorerNodeKind::ConnectionsRoot));
        assert!(kinds.contains(&ServersExplorerNodeKind::FilesystemDatabasesRoot));
        // No Recent group without history.
        assert!(!kinds.contains(&ServersExplorerNodeKind::RecentConnectionsRoot));
    }

    #[test]
    fn entra_accounts_group_by_username_then_tenant_and_connect() {
        // Same username, two tenants -> two distinct account branches.
        let discoveries = vec![
            discovery("acct-a", "dba@contoso.com", "tenant-a", "app"),
            discovery("acct-b", "dba@contoso.com", "tenant-b", "app"),
        ];
        let tree =
            ServersExplorerTree::build(&RegisteredServerStore::new(), &[], &discoveries, &[]);
        let accounts: Vec<_> = tree
            .nodes
            .iter()
            .filter(|n| n.kind == ServersExplorerNodeKind::EntraAccount)
            .collect();
        assert_eq!(accounts.len(), 2);
        // Database leaves are connectable.
        let dbs: Vec<_> = tree
            .nodes
            .iter()
            .filter(|n| n.kind == ServersExplorerNodeKind::AzureDatabase)
            .collect();
        assert_eq!(dbs.len(), 2);
        assert!(dbs.iter().all(|n| n.can_connect));
        assert!(
            dbs.iter()
                .all(|n| n.actions.contains(&ExplorerAction::Connect))
        );
    }

    #[test]
    fn connections_group_lists_manual_profiles_with_metadata_badges() {
        let mut store = RegisteredServerStore::new();
        store
            .add_server(RegisteredServer::new(
                "prod app",
                manual_profile(
                    "prod",
                    "sql.contoso.test",
                    ConnectionEnvironment::Production,
                ),
            ))
            .unwrap();
        let tree = ServersExplorerTree::build(&store, &[], &[], &[]);
        let conn = tree
            .nodes
            .iter()
            .find(|n| n.kind == ServersExplorerNodeKind::Connection)
            .expect("connection node");
        assert!(conn.can_connect);
        assert!(conn.badges.contains(&"PRODUCTION".to_string()));
        assert!(conn.badges.contains(&"Contoso".to_string()));
        assert!(conn.badges.contains(&"Billing".to_string()));
        assert!(conn.actions.contains(&ExplorerAction::RemoveProfile));
    }

    #[test]
    fn recent_connections_group_appears_only_with_history() {
        let mut store = RegisteredServerStore::new();
        let registered = RegisteredServer::new(
            "prod app",
            manual_profile(
                "prod",
                "sql.contoso.test",
                ConnectionEnvironment::Production,
            ),
        );
        let id = registered.id;
        store.add_server(registered).unwrap();
        let tree = ServersExplorerTree::build(&store, &[id], &[], &[]);
        let recent_root = tree
            .nodes
            .iter()
            .find(|n| n.kind == ServersExplorerNodeKind::RecentConnectionsRoot)
            .expect("recent root");
        let recent_children = tree.children_of(Some(&recent_root.id));
        assert_eq!(recent_children.len(), 1);
        assert_eq!(recent_children[0].profile_id, Some(id));
    }

    #[test]
    fn filesystem_group_builds_objects_and_marks_parse_errors() {
        let profile = FilesystemDatabaseProfile::new("Repo SQL", vec!["/repo".into()]);
        let snapshot = fs_snapshot("dbo", "Orders", 2);
        let sources = vec![FilesystemDatabaseSource {
            profile: &profile,
            snapshot: &snapshot,
            is_stale: false,
        }];
        let tree = ServersExplorerTree::build(&RegisteredServerStore::new(), &[], &[], &sources);
        let db = tree
            .nodes
            .iter()
            .find(|n| n.kind == ServersExplorerNodeKind::FilesystemDatabase)
            .expect("fs db node");
        assert_eq!(db.state, ServersExplorerNodeState::ParseError);
        assert!(db.badges.iter().any(|b| b.contains("parse issue")));
        assert!(
            tree.nodes
                .iter()
                .any(|n| n.kind == ServersExplorerNodeKind::FilesystemSchema && n.label == "dbo")
        );
        let table = tree
            .nodes
            .iter()
            .find(|n| n.kind == ServersExplorerNodeKind::FilesystemTable)
            .expect("fs table node");
        assert_eq!(table.label, "Orders");
        assert!(table.actions.contains(&ExplorerAction::SelectTopRows));
    }

    #[test]
    fn filesystem_stale_source_is_marked_stale() {
        let profile = FilesystemDatabaseProfile::new("Repo SQL", vec!["/repo".into()]);
        let snapshot = fs_snapshot("dbo", "Orders", 0);
        let sources = vec![FilesystemDatabaseSource {
            profile: &profile,
            snapshot: &snapshot,
            is_stale: true,
        }];
        let tree = ServersExplorerTree::build(&RegisteredServerStore::new(), &[], &[], &sources);
        let db = tree
            .nodes
            .iter()
            .find(|n| n.kind == ServersExplorerNodeKind::FilesystemDatabase)
            .unwrap();
        assert_eq!(db.state, ServersExplorerNodeState::Stale);
        assert!(db.badges.contains(&"Stale".to_string()));
    }

    #[test]
    fn node_identity_is_stable_across_rebuilds() {
        let discoveries = vec![discovery("acct-a", "dba@contoso.com", "tenant-a", "app")];
        let a = ServersExplorerTree::build(&RegisteredServerStore::new(), &[], &discoveries, &[]);
        let b = ServersExplorerTree::build(&RegisteredServerStore::new(), &[], &discoveries, &[]);
        let ids_a: Vec<_> = a.nodes.iter().map(|n| n.id.clone()).collect();
        let ids_b: Vec<_> = b.nodes.iter().map(|n| n.id.clone()).collect();
        assert_eq!(ids_a, ids_b);
    }

    #[test]
    fn filter_nodes_matches_label_kind_source_and_badges() {
        let mut store = RegisteredServerStore::new();
        store
            .add_server(RegisteredServer::new(
                "prod app",
                manual_profile(
                    "prod",
                    "sql.contoso.test",
                    ConnectionEnvironment::Production,
                ),
            ))
            .unwrap();
        let tree = ServersExplorerTree::build(&store, &[], &[], &[]);
        assert!(!filter_nodes(&tree.nodes, "contoso").is_empty());
        assert!(!filter_nodes(&tree.nodes, "production").is_empty());
        assert_eq!(filter_nodes(&tree.nodes, "").len(), tree.nodes.len());
        assert!(filter_nodes(&tree.nodes, "zzz-nope").is_empty());
    }

    #[test]
    fn tree_round_trips_through_json() {
        let discoveries = vec![discovery("acct-a", "dba@contoso.com", "tenant-a", "app")];
        let tree =
            ServersExplorerTree::build(&RegisteredServerStore::new(), &[], &discoveries, &[]);
        let json = serde_json::to_string(&tree).unwrap();
        let back: ServersExplorerTree = serde_json::from_str(&json).unwrap();
        assert_eq!(tree, back);
    }
}
