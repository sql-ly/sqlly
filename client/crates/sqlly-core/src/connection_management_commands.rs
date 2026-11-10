//! Named command verbs, change events, metadata reclassification, and
//! per-workspace expansion persistence for the Connection Management Screen.
//!
//! The connection-management screen is UI-agnostic at its core: the AppKit
//! controller (and the CLI) drive it through the small, explicit command
//! surface in this module rather than reaching into [`ConnectionManagementTree`]
//! internals. Every mutating command returns a [`ConnectionManagementEvent`] so
//! the UI can react granularly (re-render one subtree, mark discovery stale,
//! etc.) instead of rebuilding the whole tree.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

use crate::connection::ConnectionEnvironment;
use crate::connection_management::{
    AzureSqlDiscovery, ConnectionManagementNode, ConnectionManagementNodeId,
    ConnectionManagementTree,
};
use crate::registered_servers::{RegisteredServerError, RegisteredServerStore};

/// The named, stable command verbs the connection-management screen exposes.
/// These names are part of the command contract (telemetry, keybindings, CLI),
/// so they are spelled out explicitly rather than derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionManagementCommand {
    /// `connection_management_tree_list` — list children of a node (or roots).
    ConnectionManagementTreeList,
    /// `connection_management_refresh` — re-discover at a node scope.
    ConnectionManagementRefresh,
    /// `connection_management_reclassify` — move a saved profile's
    /// client/project/environment metadata.
    ConnectionManagementReclassify,
    /// `connection_management_set_expanded` — persist a node's expansion state.
    ConnectionManagementSetExpanded,
}

impl ConnectionManagementCommand {
    /// The stable verb string used by CLI / telemetry / keybindings.
    pub fn verb(self) -> &'static str {
        match self {
            Self::ConnectionManagementTreeList => "connection_management_tree_list",
            Self::ConnectionManagementRefresh => "connection_management_refresh",
            Self::ConnectionManagementReclassify => "connection_management_reclassify",
            Self::ConnectionManagementSetExpanded => "connection_management_set_expanded",
        }
    }
}

/// The scope at which a refresh / re-discovery is requested. Lets the UI ask
/// for the minimal live work: a single server's databases vs. an entire
/// account's subscriptions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshScope {
    /// Refresh every Azure account (top-level Azure root).
    AzureRoot,
    /// Refresh a single account's subscriptions/servers/databases.
    AzureAccount { account_id: String },
    /// Refresh a single server's databases.
    AzureServer {
        account_id: String,
        server_id: String,
    },
}

/// Granular change events emitted by mutating commands so the UI can update a
/// subtree (or a status badge) instead of rebuilding everything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ConnectionManagementEvent {
    /// The SQL-servers subtree changed because a profile was reclassified.
    ProfileReclassified {
        profile_id: Uuid,
        client: Option<String>,
        project: Option<String>,
        environment: ConnectionEnvironment,
    },
    /// A refresh was requested; the discovery cache for the scope is now stale
    /// and the UI should show a loading affordance until live data returns.
    DiscoveryMarkedStale { scope: RefreshScope },
    /// A node's expansion state was persisted for the workspace.
    ExpansionChanged {
        node_id: ConnectionManagementNodeId,
        expanded: bool,
    },
}

/// Persisted, per-workspace expansion state for the tree. Stored separately
/// from the tree itself (which is derived/ephemeral) so expansion survives
/// re-discovery and app restarts. Serialized into the workspace document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceExpansionState {
    expanded: BTreeSet<String>,
}

impl WorkspaceExpansionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records whether `node` is expanded. Returns the change event.
    pub fn set_expanded(
        &mut self,
        node: &ConnectionManagementNodeId,
        expanded: bool,
    ) -> ConnectionManagementEvent {
        if expanded {
            self.expanded.insert(node.0.clone());
        } else {
            self.expanded.remove(&node.0);
        }
        ConnectionManagementEvent::ExpansionChanged {
            node_id: node.clone(),
            expanded,
        }
    }

    pub fn is_expanded(&self, node: &ConnectionManagementNodeId) -> bool {
        self.expanded.contains(&node.0)
    }

    pub fn len(&self) -> usize {
        self.expanded.len()
    }

    pub fn is_empty(&self) -> bool {
        self.expanded.is_empty()
    }
}

/// The connection-management command facade. Wraps the stores the screen
/// operates on (registered servers + discovery snapshot + expansion state) and
/// exposes the named verbs, each returning a tree view or a change event.
pub struct ConnectionManagementService {
    store: RegisteredServerStore,
    discoveries: Vec<AzureSqlDiscovery>,
    expansion: WorkspaceExpansionState,
}

impl ConnectionManagementService {
    pub fn new(store: RegisteredServerStore) -> Self {
        Self {
            store,
            discoveries: Vec::new(),
            expansion: WorkspaceExpansionState::new(),
        }
    }

    pub fn with_discoveries(mut self, discoveries: Vec<AzureSqlDiscovery>) -> Self {
        self.discoveries = discoveries;
        self
    }

    pub fn store(&self) -> &RegisteredServerStore {
        &self.store
    }

    pub fn expansion(&self) -> &WorkspaceExpansionState {
        &self.expansion
    }

    /// `connection_management_tree_list` — build the saved-servers tree and
    /// list the children of `parent` (or the roots when `parent` is `None`).
    pub fn tree_list(
        &self,
        parent: Option<&ConnectionManagementNodeId>,
    ) -> Vec<ConnectionManagementNode> {
        let tree = ConnectionManagementTree::from_registered_servers(&self.store);
        tree.children_of(parent).into_iter().cloned().collect()
    }

    /// `connection_management_tree_list` for the Azure side.
    pub fn azure_tree_list(
        &self,
        parent: Option<&ConnectionManagementNodeId>,
    ) -> Vec<ConnectionManagementNode> {
        let tree = ConnectionManagementTree::from_azure_discoveries(&self.discoveries);
        tree.children_of(parent).into_iter().cloned().collect()
    }

    /// `connection_management_refresh` — signal that the discovery cache for a
    /// scope is stale. The actual live re-discovery is performed by the caller
    /// (which owns the network seam); this records intent and emits the event
    /// so the UI can show progress. Returns the event.
    pub fn refresh(&mut self, scope: RefreshScope) -> ConnectionManagementEvent {
        ConnectionManagementEvent::DiscoveryMarkedStale { scope }
    }

    /// `connection_management_reclassify` — move a saved profile's
    /// organization metadata (client / project / environment). Returns the
    /// change event so the UI can re-render just the SQL-servers subtree.
    pub fn reclassify(
        &mut self,
        profile_id: Uuid,
        client: Option<String>,
        project: Option<String>,
        environment: ConnectionEnvironment,
    ) -> Result<ConnectionManagementEvent, RegisteredServerError> {
        let server = self
            .store
            .servers()
            .iter()
            .find(|s| s.profile.id.0 == profile_id)
            .ok_or(RegisteredServerError::ServerNotFound(profile_id))?;
        let mut updated = server.clone();
        updated.profile.client = client.clone().filter(|v| !v.trim().is_empty());
        updated.profile.project = project.clone().filter(|v| !v.trim().is_empty());
        updated.profile.environment = environment;
        self.store.replace_server(updated)?;
        Ok(ConnectionManagementEvent::ProfileReclassified {
            profile_id,
            client,
            project,
            environment,
        })
    }

    /// `connection_management_set_expanded` — persist expansion state.
    pub fn set_expanded(
        &mut self,
        node: &ConnectionManagementNodeId,
        expanded: bool,
    ) -> ConnectionManagementEvent {
        self.expansion.set_expanded(node, expanded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{ConnectionProfile, SecretProvider, SecretRef};
    use crate::registered_servers::RegisteredServer;

    fn sql_profile(name: &str, server: &str) -> ConnectionProfile {
        ConnectionProfile::sql_login(
            name,
            server,
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "x".to_string(),
            },
        )
    }

    #[test]
    fn command_verbs_are_stable_strings() {
        assert_eq!(
            ConnectionManagementCommand::ConnectionManagementTreeList.verb(),
            "connection_management_tree_list"
        );
        assert_eq!(
            ConnectionManagementCommand::ConnectionManagementReclassify.verb(),
            "connection_management_reclassify"
        );
    }

    #[test]
    fn reclassify_moves_profile_metadata_and_emits_event() {
        let mut store = RegisteredServerStore::new();
        let profile = sql_profile("prod-db", "prod.example.com");
        let profile_id = profile.id.0;
        store
            .add_server(RegisteredServer::new("prod-db", profile))
            .unwrap();

        let mut service = ConnectionManagementService::new(store);
        let event = service
            .reclassify(
                profile_id,
                Some("Contoso".to_string()),
                Some("Billing".to_string()),
                ConnectionEnvironment::Production,
            )
            .expect("reclassify succeeds");

        assert_eq!(
            event,
            ConnectionManagementEvent::ProfileReclassified {
                profile_id,
                client: Some("Contoso".to_string()),
                project: Some("Billing".to_string()),
                environment: ConnectionEnvironment::Production,
            }
        );
        let updated = service
            .store()
            .servers()
            .iter()
            .find(|s| s.profile.id.0 == profile_id)
            .unwrap();
        assert_eq!(updated.profile.client.as_deref(), Some("Contoso"));
        assert_eq!(updated.profile.project.as_deref(), Some("Billing"));
        assert_eq!(
            updated.profile.environment,
            ConnectionEnvironment::Production
        );
    }

    #[test]
    fn reclassify_unknown_profile_errors() {
        let mut service = ConnectionManagementService::new(RegisteredServerStore::new());
        let err = service
            .reclassify(Uuid::new_v4(), None, None, ConnectionEnvironment::Local)
            .unwrap_err();
        assert!(matches!(err, RegisteredServerError::ServerNotFound(_)));
    }

    #[test]
    fn reclassify_blank_metadata_clears_to_none() {
        let mut store = RegisteredServerStore::new();
        let mut profile = sql_profile("db", "db.example.com");
        profile.client = Some("Old".to_string());
        let profile_id = profile.id.0;
        store
            .add_server(RegisteredServer::new("db", profile))
            .unwrap();
        let mut service = ConnectionManagementService::new(store);
        service
            .reclassify(
                profile_id,
                Some("   ".to_string()),
                None,
                ConnectionEnvironment::Dev,
            )
            .unwrap();
        let updated = &service.store().servers()[0];
        assert_eq!(updated.profile.client, None);
    }

    #[test]
    fn expansion_state_round_trips_and_persists() {
        let mut state = WorkspaceExpansionState::new();
        let node = ConnectionManagementNodeId("azure:account:alex".to_string());
        assert!(!state.is_expanded(&node));

        let event = state.set_expanded(&node, true);
        assert_eq!(
            event,
            ConnectionManagementEvent::ExpansionChanged {
                node_id: node.clone(),
                expanded: true,
            }
        );
        assert!(state.is_expanded(&node));

        // Survives a serde round-trip (workspace persistence).
        let json = serde_json::to_string(&state).unwrap();
        let restored: WorkspaceExpansionState = serde_json::from_str(&json).unwrap();
        assert!(restored.is_expanded(&node));

        state.set_expanded(&node, false);
        assert!(!state.is_expanded(&node));
    }

    #[test]
    fn refresh_emits_scoped_stale_event() {
        let mut service = ConnectionManagementService::new(RegisteredServerStore::new());
        let scope = RefreshScope::AzureServer {
            account_id: "acct".to_string(),
            server_id: "srv".to_string(),
        };
        let event = service.refresh(scope.clone());
        assert_eq!(
            event,
            ConnectionManagementEvent::DiscoveryMarkedStale { scope }
        );
    }
}
