use crate::connection::{AuthenticationType, ConnectionEnvironment, ConnectionProfile};
use crate::connection_display::{
    auth_key, azure_database_badges, azure_sort_key, environment_label, environment_rank,
};
use crate::registered_servers::RegisteredServerStore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionManagementNodeId(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConnectionManagementNodeKind {
    AzureRoot,
    AzureAccount,
    AzureSubscription,
    AzureSqlServer,
    AzureDatabase,
    SqlServersRoot,
    Client,
    Project,
    Environment,
    SqlServer,
    SqlDatabase,
    SavedProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ConnectionManagementNodeSource {
    Saved,
    Discovered,
    Linked,
    Placeholder,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConnectionManagementNodeCapabilities {
    pub can_refresh: bool,
    pub can_create_profile: bool,
    pub can_edit_profile: bool,
    pub can_delete_profile: bool,
    pub can_test: bool,
    pub can_connect: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionManagementNode {
    pub id: ConnectionManagementNodeId,
    pub parent: Option<ConnectionManagementNodeId>,
    pub label: String,
    pub kind: ConnectionManagementNodeKind,
    pub source: ConnectionManagementNodeSource,
    pub capabilities: ConnectionManagementNodeCapabilities,
    pub badges: Vec<String>,
    pub profile_id: Option<Uuid>,
    pub account_id: Option<String>,
    pub resource_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConnectionManagementTree {
    pub nodes: Vec<ConnectionManagementNode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureSqlDiscovery {
    pub account_id: String,
    pub username: String,
    pub tenant_id: String,
    pub subscription_id: String,
    pub subscription_name: Option<String>,
    pub resource_group: String,
    pub server_id: String,
    pub server_name: String,
    pub server_fqdn: String,
    pub location: String,
    pub database_id: String,
    pub database_name: String,
    pub service_tier: Option<String>,
    pub status: Option<String>,
    pub linked_profile_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionOptionsFormMode {
    CreateManual,
    EditSaved,
    CreateFromDiscovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionOptionsForm {
    pub mode: ConnectionOptionsFormMode,
    pub name: String,
    pub server: String,
    pub port: u16,
    pub database: Option<String>,
    pub environment: ConnectionEnvironment,
    pub client: Option<String>,
    pub project: Option<String>,
    pub authentication: AuthenticationType,
    pub encrypt: bool,
    pub trust_server_certificate: bool,
    pub timeout_seconds: u64,
    pub application_name: String,
    pub auto_wrap_rollback: bool,
    pub block_all_but_select: bool,
    pub validation_errors: Vec<ConnectionOptionsValidationError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionOptionsValidationError {
    pub field: String,
    pub message: String,
}

struct GroupNodeSpec {
    key: String,
    label: String,
    kind: ConnectionManagementNodeKind,
    source: ConnectionManagementNodeSource,
    account_id: Option<String>,
    resource_id: Option<String>,
}

impl ConnectionManagementTree {
    pub fn from_registered_servers(store: &RegisteredServerStore) -> Self {
        let mut tree = Self::default();
        let root_id = ConnectionManagementNodeId("sql-root".to_string());
        tree.nodes.push(ConnectionManagementNode::section(
            root_id.clone(),
            "SQL Servers",
            ConnectionManagementNodeKind::SqlServersRoot,
        ));

        let mut group_ids = HashMap::<String, ConnectionManagementNodeId>::new();
        let mut servers = store.servers().iter().collect::<Vec<_>>();
        servers.sort_by(|a, b| {
            sql_sort_key(&a.profile, &a.display_name)
                .cmp(&sql_sort_key(&b.profile, &b.display_name))
        });

        for registered in servers {
            let profile = &registered.profile;
            let client = normalized_label(profile.client.as_deref(), "Unassigned Client");
            let project = normalized_label(profile.project.as_deref(), "Unassigned Project");
            let environment = environment_label(profile.environment);
            let auth = auth_key(&profile.authentication);
            // A server node carries the connection's own name, so the tree
            // reads the way the user named things; the host is the fallback for
            // a profile saved without one. The name is part of the bucket key
            // as well as its label — keying on the host alone would label a
            // bucket "Reporting" and then hang an "Analytics" profile under it
            // whenever two connections point at the same machine.
            let server_label = server_display_name(&registered.display_name, &profile.server);
            let server_key = format!(
                "{}|{}|{}|{}|{}|{}",
                client, project, environment, profile.server, server_label, auth
            );

            let client_id = tree.group_node(
                &mut group_ids,
                &root_id,
                format!("client:{client}"),
                client.clone(),
                ConnectionManagementNodeKind::Client,
            );
            let project_id = tree.group_node(
                &mut group_ids,
                &client_id,
                format!("project:{client}:{project}"),
                project.clone(),
                ConnectionManagementNodeKind::Project,
            );
            let environment_id = tree.group_node(
                &mut group_ids,
                &project_id,
                format!("environment:{client}:{project}:{environment}"),
                environment,
                ConnectionManagementNodeKind::Environment,
            );
            let server_id = tree.group_node(
                &mut group_ids,
                &environment_id,
                format!("server:{server_key}"),
                server_label.clone(),
                ConnectionManagementNodeKind::SqlServer,
            );
            let database_label = profile.database.as_deref().unwrap_or("master").to_string();
            let database_id = tree.group_node(
                &mut group_ids,
                &server_id,
                format!("database:{server_key}:{database_label}"),
                database_label,
                ConnectionManagementNodeKind::SqlDatabase,
            );

            tree.nodes.push(ConnectionManagementNode {
                id: ConnectionManagementNodeId(format!("profile:{}", registered.id)),
                parent: Some(database_id),
                label: registered.display_name.clone(),
                kind: ConnectionManagementNodeKind::SavedProfile,
                source: ConnectionManagementNodeSource::Saved,
                capabilities: ConnectionManagementNodeCapabilities::saved_profile(),
                badges: safety_badges(profile),
                profile_id: Some(registered.id),
                account_id: None,
                resource_id: None,
            });
        }

        tree
    }

    pub fn from_azure_discoveries(discoveries: &[AzureSqlDiscovery]) -> Self {
        let mut tree = Self::default();
        let root_id = ConnectionManagementNodeId("azure-root".to_string());
        tree.nodes.push(ConnectionManagementNode::section(
            root_id.clone(),
            "Azure",
            ConnectionManagementNodeKind::AzureRoot,
        ));

        let mut group_ids = HashMap::<String, ConnectionManagementNodeId>::new();
        let mut items = discoveries.iter().collect::<Vec<_>>();
        items.sort_by_key(|a| azure_sort_key(a));

        for discovery in items {
            let account_label = format!("{} ({})", discovery.username, discovery.tenant_id);
            let account_id = tree.group_node_with_source(
                &mut group_ids,
                &root_id,
                GroupNodeSpec {
                    key: format!("azure-account:{}", discovery.account_id),
                    label: account_label,
                    kind: ConnectionManagementNodeKind::AzureAccount,
                    source: ConnectionManagementNodeSource::Discovered,
                    account_id: Some(discovery.account_id.clone()),
                    resource_id: None,
                },
            );
            let subscription_label = discovery
                .subscription_name
                .as_deref()
                .filter(|name| !name.is_empty())
                .map(|name| format!("{} ({})", name, discovery.subscription_id))
                .unwrap_or_else(|| discovery.subscription_id.clone());
            let subscription_id = tree.group_node_with_source(
                &mut group_ids,
                &account_id,
                GroupNodeSpec {
                    key: format!(
                        "azure-subscription:{}:{}",
                        discovery.account_id, discovery.subscription_id
                    ),
                    label: subscription_label,
                    kind: ConnectionManagementNodeKind::AzureSubscription,
                    source: ConnectionManagementNodeSource::Discovered,
                    account_id: Some(discovery.account_id.clone()),
                    resource_id: Some(discovery.subscription_id.clone()),
                },
            );
            let server_label = format!("{} ({})", discovery.server_name, discovery.location);
            let server_id = tree.group_node_with_source(
                &mut group_ids,
                &subscription_id,
                GroupNodeSpec {
                    key: format!("azure-server:{}", discovery.server_id),
                    label: server_label,
                    kind: ConnectionManagementNodeKind::AzureSqlServer,
                    source: ConnectionManagementNodeSource::Discovered,
                    account_id: Some(discovery.account_id.clone()),
                    resource_id: Some(discovery.server_id.clone()),
                },
            );
            tree.nodes.push(ConnectionManagementNode {
                id: ConnectionManagementNodeId(format!("azure-database:{}", discovery.database_id)),
                parent: Some(server_id),
                label: discovery.database_name.clone(),
                kind: ConnectionManagementNodeKind::AzureDatabase,
                source: if discovery.linked_profile_id.is_some() {
                    ConnectionManagementNodeSource::Linked
                } else {
                    ConnectionManagementNodeSource::Discovered
                },
                capabilities: ConnectionManagementNodeCapabilities {
                    can_create_profile: discovery.linked_profile_id.is_none(),
                    can_edit_profile: discovery.linked_profile_id.is_some(),
                    can_test: discovery.linked_profile_id.is_some(),
                    can_connect: discovery.linked_profile_id.is_some(),
                    ..ConnectionManagementNodeCapabilities::default()
                },
                badges: azure_database_badges(discovery),
                profile_id: discovery.linked_profile_id,
                account_id: Some(discovery.account_id.clone()),
                resource_id: Some(discovery.database_id.clone()),
            });
        }

        tree
    }

    pub fn children_of(
        &self,
        parent: Option<&ConnectionManagementNodeId>,
    ) -> Vec<&ConnectionManagementNode> {
        self.nodes
            .iter()
            .filter(|node| node.parent.as_ref() == parent)
            .collect()
    }

    fn group_node(
        &mut self,
        ids: &mut HashMap<String, ConnectionManagementNodeId>,
        parent: &ConnectionManagementNodeId,
        key: String,
        label: String,
        kind: ConnectionManagementNodeKind,
    ) -> ConnectionManagementNodeId {
        self.group_node_with_source(
            ids,
            parent,
            GroupNodeSpec {
                key,
                label,
                kind,
                source: ConnectionManagementNodeSource::Placeholder,
                account_id: None,
                resource_id: None,
            },
        )
    }

    fn group_node_with_source(
        &mut self,
        ids: &mut HashMap<String, ConnectionManagementNodeId>,
        parent: &ConnectionManagementNodeId,
        spec: GroupNodeSpec,
    ) -> ConnectionManagementNodeId {
        let GroupNodeSpec {
            key,
            label,
            kind,
            source,
            account_id,
            resource_id,
        } = spec;
        if let Some(id) = ids.get(&key) {
            return id.clone();
        }
        let id = ConnectionManagementNodeId(key);
        self.nodes.push(ConnectionManagementNode {
            id: id.clone(),
            parent: Some(parent.clone()),
            label,
            kind,
            source,
            capabilities: match kind {
                ConnectionManagementNodeKind::AzureRoot
                | ConnectionManagementNodeKind::AzureAccount
                | ConnectionManagementNodeKind::AzureSubscription
                | ConnectionManagementNodeKind::AzureSqlServer => {
                    ConnectionManagementNodeCapabilities {
                        can_refresh: true,
                        ..ConnectionManagementNodeCapabilities::default()
                    }
                }
                _ => ConnectionManagementNodeCapabilities::default(),
            },
            badges: Vec::new(),
            profile_id: None,
            account_id,
            resource_id,
        });
        ids.insert(id.0.clone(), id.clone());
        id
    }
}

impl ConnectionManagementNode {
    fn section(
        id: ConnectionManagementNodeId,
        label: &str,
        kind: ConnectionManagementNodeKind,
    ) -> Self {
        Self {
            id,
            parent: None,
            label: label.to_string(),
            kind,
            source: ConnectionManagementNodeSource::Placeholder,
            capabilities: ConnectionManagementNodeCapabilities {
                can_refresh: true,
                ..ConnectionManagementNodeCapabilities::default()
            },
            badges: Vec::new(),
            profile_id: None,
            account_id: None,
            resource_id: None,
        }
    }
}

impl ConnectionManagementNodeCapabilities {
    fn saved_profile() -> Self {
        Self {
            can_edit_profile: true,
            can_delete_profile: true,
            can_test: true,
            can_connect: true,
            ..Self::default()
        }
    }
}

impl ConnectionOptionsForm {
    pub fn create_manual() -> Self {
        Self::from_profile(
            ConnectionOptionsFormMode::CreateManual,
            &ConnectionProfile::sql_login(
                "New Connection",
                "localhost",
                "sa",
                crate::connection::SecretRef {
                    provider: crate::connection::SecretProvider::InMemory,
                    key: String::new(),
                },
            ),
        )
    }

    pub fn from_profile(mode: ConnectionOptionsFormMode, profile: &ConnectionProfile) -> Self {
        let mut form = Self {
            mode,
            name: profile.name.clone(),
            server: profile.server.clone(),
            port: profile.port,
            database: profile.database.clone(),
            environment: profile.environment,
            client: profile.client.clone(),
            project: profile.project.clone(),
            authentication: profile.authentication.clone(),
            encrypt: profile.encrypt,
            trust_server_certificate: profile.trust_server_certificate,
            timeout_seconds: profile.timeout_seconds,
            application_name: profile.application_name.clone(),
            auto_wrap_rollback: profile.auto_wrap_rollback,
            block_all_but_select: profile.block_all_but_select,
            validation_errors: Vec::new(),
        };
        form.validation_errors = form.validate();
        form
    }

    pub fn validate(&self) -> Vec<ConnectionOptionsValidationError> {
        let mut errors = Vec::new();
        if self.name.trim().is_empty() {
            errors.push(ConnectionOptionsValidationError::new(
                "name",
                "Name is required",
            ));
        }
        if self.server.trim().is_empty() {
            errors.push(ConnectionOptionsValidationError::new(
                "server",
                "Server is required",
            ));
        }
        if self.port == 0 {
            errors.push(ConnectionOptionsValidationError::new(
                "port",
                "Port must be greater than zero",
            ));
        }
        if self.timeout_seconds == 0 {
            errors.push(ConnectionOptionsValidationError::new(
                "timeout_seconds",
                "Timeout must be greater than zero",
            ));
        }
        // Trusting the server certificate on a production profile is a
        // deliberate choice, not a form error: plenty of real production
        // servers sit behind self-signed or internal-CA certificates the client
        // machine has no way to chain. `ConnectionProfile::preview` still warns
        // that TLS identity goes unverified, so the risk stays visible without
        // the form refusing to save, test, or connect.
        errors
    }

    pub fn can_save(&self) -> bool {
        self.validate().is_empty()
    }
}

impl ConnectionOptionsValidationError {
    fn new(field: &str, message: &str) -> Self {
        Self {
            field: field.to_string(),
            message: message.to_string(),
        }
    }
}

fn normalized_label(value: Option<&str>, fallback: &str) -> String {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_string()
}

fn safety_badges(profile: &ConnectionProfile) -> Vec<String> {
    let mut badges = Vec::new();
    if profile.block_all_but_select {
        badges.push("SELECT only".to_string());
    }
    if profile.auto_wrap_rollback {
        badges.push("Rollback wrap".to_string());
    }
    badges
}

/// What a server node is called: the connection's display name, or the host it
/// points at when that name is blank. The host is a fallback, never the
/// headline — a tree full of `sql-prod-03.internal` says nothing about which
/// connection is which.
fn server_display_name(display_name: &str, host: &str) -> String {
    let trimmed = display_name.trim();
    if trimmed.is_empty() {
        host.to_string()
    } else {
        trimmed.to_string()
    }
}

fn sql_sort_key(
    profile: &ConnectionProfile,
    display_name: &str,
) -> (u8, String, String, u8, String, String, String) {
    (
        environment_rank(profile.environment),
        profile.client.clone().unwrap_or_default(),
        profile.project.clone().unwrap_or_default(),
        0,
        profile.server.clone(),
        profile.database.clone().unwrap_or_default(),
        display_name.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RegisteredServer, SecretProvider, SecretRef};

    fn profile(
        name: &str,
        server: &str,
        database: &str,
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
        profile.database = Some(database.to_string());
        profile.environment = environment;
        profile.client = Some("Client A".to_string());
        profile.project = Some("Project X".to_string());
        profile
    }

    #[test]
    fn sql_tree_groups_by_client_project_environment_server_database() {
        let mut store = RegisteredServerStore::new();
        store
            .add_server(RegisteredServer::new(
                "prod app",
                profile(
                    "prod",
                    "sql.contoso.test",
                    "app",
                    ConnectionEnvironment::Production,
                ),
            ))
            .unwrap();
        store
            .add_server(RegisteredServer::new(
                "dev app",
                profile("dev", "sql.contoso.test", "app", ConnectionEnvironment::Dev),
            ))
            .unwrap();

        let tree = ConnectionManagementTree::from_registered_servers(&store);
        assert!(
            tree.nodes.iter().any(|node| node.label == "Client A"
                && node.kind == ConnectionManagementNodeKind::Client)
        );
        assert!(
            tree.nodes.iter().any(|node| node.label == "Project X"
                && node.kind == ConnectionManagementNodeKind::Project)
        );
        assert!(tree.nodes.iter().any(|node| node.label == "PRODUCTION"
            && node.kind == ConnectionManagementNodeKind::Environment));
        assert!(
            tree.nodes.iter().any(|node| node.label == "DEV"
                && node.kind == ConnectionManagementNodeKind::Environment)
        );
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|node| node.kind == ConnectionManagementNodeKind::SavedProfile)
                .count(),
            2
        );
    }

    #[test]
    fn sql_tree_uses_unassigned_placeholders() {
        let mut store = RegisteredServerStore::new();
        let mut profile = profile("local", "localhost", "master", ConnectionEnvironment::Local);
        profile.client = None;
        profile.project = None;
        store
            .add_server(RegisteredServer::new("local", profile))
            .unwrap();

        let tree = ConnectionManagementTree::from_registered_servers(&store);
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.label == "Unassigned Client")
        );
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.label == "Unassigned Project")
        );
    }

    #[test]
    fn azure_tree_keeps_same_username_in_different_tenants_separate() {
        let discoveries = vec![
            AzureSqlDiscovery {
                account_id: "acct-tenant-a".to_string(),
                username: "dba@contoso.com".to_string(),
                tenant_id: "tenant-a".to_string(),
                subscription_id: "sub-a".to_string(),
                subscription_name: Some("Prod".to_string()),
                resource_group: "rg-a".to_string(),
                server_id: "/servers/prod-a".to_string(),
                server_name: "prod-a".to_string(),
                server_fqdn: "prod-a.database.windows.net".to_string(),
                location: "eastus".to_string(),
                database_id: "/servers/prod-a/databases/app".to_string(),
                database_name: "app".to_string(),
                service_tier: Some("Standard".to_string()),
                status: Some("Online".to_string()),
                linked_profile_id: None,
            },
            AzureSqlDiscovery {
                account_id: "acct-tenant-b".to_string(),
                username: "dba@contoso.com".to_string(),
                tenant_id: "tenant-b".to_string(),
                subscription_id: "sub-b".to_string(),
                subscription_name: None,
                resource_group: "rg-b".to_string(),
                server_id: "/servers/prod-b".to_string(),
                server_name: "prod-b".to_string(),
                server_fqdn: "prod-b.database.windows.net".to_string(),
                location: "westus".to_string(),
                database_id: "/servers/prod-b/databases/app".to_string(),
                database_name: "app".to_string(),
                service_tier: None,
                status: None,
                linked_profile_id: Some(Uuid::nil()),
            },
        ];

        let tree = ConnectionManagementTree::from_azure_discoveries(&discoveries);
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|node| node.kind == ConnectionManagementNodeKind::AzureAccount)
                .count(),
            2
        );
        assert!(tree.nodes.iter().any(|node| {
            node.kind == ConnectionManagementNodeKind::AzureSubscription
                && node.label == "Prod (sub-a)"
        }));
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.source == ConnectionManagementNodeSource::Linked
                    && node.badges == ["Linked"])
        );
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.badges == ["Discovered", "Standard", "Online"])
        );
    }

    #[test]
    fn server_nodes_are_labeled_with_the_display_name() {
        let mut store = RegisteredServerStore::new();
        store
            .add_server(RegisteredServer::new(
                "Reporting",
                profile(
                    "reporting",
                    "sql.contoso.test",
                    "app",
                    ConnectionEnvironment::Production,
                ),
            ))
            .unwrap();
        // Same host, same client/project/environment, different name: two
        // server nodes, each named after its own connection.
        store
            .add_server(RegisteredServer::new(
                "Analytics",
                profile(
                    "analytics",
                    "sql.contoso.test",
                    "app",
                    ConnectionEnvironment::Production,
                ),
            ))
            .unwrap();
        // No name at all — the host is what is left to show.
        store
            .add_server(RegisteredServer::new(
                "   ",
                profile(
                    "unnamed",
                    "sql.fabrikam.test",
                    "app",
                    ConnectionEnvironment::Production,
                ),
            ))
            .unwrap();

        let tree = ConnectionManagementTree::from_registered_servers(&store);
        let server_labels: Vec<&str> = tree
            .nodes
            .iter()
            .filter(|node| node.kind == ConnectionManagementNodeKind::SqlServer)
            .map(|node| node.label.as_str())
            .collect();
        assert!(server_labels.contains(&"Reporting"), "{server_labels:?}");
        assert!(server_labels.contains(&"Analytics"), "{server_labels:?}");
        assert!(
            server_labels.contains(&"sql.fabrikam.test"),
            "{server_labels:?}"
        );
        assert!(
            !server_labels.contains(&"sql.contoso.test"),
            "a named connection must not fall back to its host: {server_labels:?}"
        );
    }

    #[test]
    fn connection_options_form_validates_required_fields() {
        let mut form = ConnectionOptionsForm::create_manual();
        form.name.clear();
        form.server.clear();

        let errors = form.validate();
        assert!(errors.iter().any(|error| error.field == "name"));
        assert!(errors.iter().any(|error| error.field == "server"));
        assert!(!form.can_save());
    }

    #[test]
    fn production_profile_may_trust_the_server_certificate() {
        // Internal-CA and self-signed certs are common on production servers;
        // the profile warns about unverified TLS identity but must still save,
        // test, and connect.
        let mut form = ConnectionOptionsForm::create_manual();
        form.environment = ConnectionEnvironment::Production;
        form.trust_server_certificate = true;

        assert!(
            !form
                .validate()
                .iter()
                .any(|error| error.field == "trust_server_certificate")
        );
        assert!(form.can_save());
    }

    #[test]
    fn same_server_db_under_different_client_project_env_do_not_merge() {
        let mut store = RegisteredServerStore::new();
        // Same server + database name, but different client/project/env: these
        // must remain distinct saved entries, never collapsed into one row.
        let mut a = profile(
            "a",
            "sql.shared.test",
            "app",
            ConnectionEnvironment::Production,
        );
        a.client = Some("Contoso".to_string());
        a.project = Some("Billing".to_string());
        let mut b = profile("b", "sql.shared.test", "app", ConnectionEnvironment::Dev);
        b.client = Some("Fabrikam".to_string());
        b.project = Some("Analytics".to_string());

        store.add_server(RegisteredServer::new("a", a)).unwrap();
        store.add_server(RegisteredServer::new("b", b)).unwrap();

        let tree = ConnectionManagementTree::from_registered_servers(&store);
        // Two distinct client nodes.
        assert!(
            tree.nodes
                .iter()
                .any(|n| n.label == "Contoso" && n.kind == ConnectionManagementNodeKind::Client)
        );
        assert!(
            tree.nodes
                .iter()
                .any(|n| n.label == "Fabrikam" && n.kind == ConnectionManagementNodeKind::Client)
        );
        // Both saved profiles survive (no merge).
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|n| n.kind == ConnectionManagementNodeKind::SavedProfile)
                .count(),
            2
        );
    }

    #[test]
    fn discovered_database_links_to_saved_profile_when_profile_id_present() {
        let linked = Uuid::new_v4();
        let discoveries = vec![AzureSqlDiscovery {
            account_id: "acct".to_string(),
            username: "dba@contoso.com".to_string(),
            tenant_id: "tenant-a".to_string(),
            subscription_id: "sub".to_string(),
            subscription_name: Some("Prod".to_string()),
            resource_group: "rg".to_string(),
            server_id: "/servers/srv".to_string(),
            server_name: "srv".to_string(),
            server_fqdn: "srv.database.windows.net".to_string(),
            location: "eastus".to_string(),
            database_id: "/servers/srv/databases/app".to_string(),
            database_name: "app".to_string(),
            service_tier: Some("Standard".to_string()),
            status: Some("Online".to_string()),
            linked_profile_id: Some(linked),
        }];

        let tree = ConnectionManagementTree::from_azure_discoveries(&discoveries);
        let db_node = tree
            .nodes
            .iter()
            .find(|n| n.kind == ConnectionManagementNodeKind::AzureDatabase)
            .expect("database node exists");
        assert_eq!(db_node.source, ConnectionManagementNodeSource::Linked);
        assert_eq!(db_node.profile_id, Some(linked));
        assert!(db_node.badges.contains(&"Linked".to_string()));
    }

    #[test]
    fn form_snapshot_after_save_contains_no_raw_secret() {
        // A form built from a saved SQL-login profile references the secret only
        // through a SecretRef (provider + keychain lookup key), never a
        // plaintext password. With the MacOsKeychain provider the key is an
        // opaque lookup id, so no secret value is present in the snapshot.
        let mut profile = profile("p", "sql.test", "app", ConnectionEnvironment::Production);
        profile.authentication = AuthenticationType::SqlLogin {
            username: "appuser".to_string(),
            secret_ref: SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: "sqlly/appuser@sql.test".to_string(),
            },
        };
        let form =
            ConnectionOptionsForm::from_profile(ConnectionOptionsFormMode::EditSaved, &profile);
        let json = serde_json::to_string(&form).unwrap();
        // Username is present (needed for editing); the keychain lookup key is a
        // reference, not a secret; and there is no plaintext password field.
        assert!(json.contains("appuser"));
        assert!(!json.to_lowercase().contains("password"));
        assert!(!json.contains("\"plaintext\""));
    }
}
