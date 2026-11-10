use crate::connection::ConnectionProfile;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifier for a registered-servers group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GroupId(pub Uuid);

impl GroupId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for GroupId {
    fn default() -> Self {
        Self::new()
    }
}

/// A named, hierarchical container for registered servers, mirroring SSMS's
/// "Registered Servers" tree. Groups may nest arbitrarily deep via
/// [`RegisteredServerStore`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerGroup {
    pub id: GroupId,
    pub name: String,
    /// `None` for the root group; otherwise the parent group's id.
    pub parent: Option<GroupId>,
    pub description: String,
}

impl ServerGroup {
    pub fn new(name: impl Into<String>, parent: Option<GroupId>) -> Self {
        Self {
            id: GroupId::new(),
            name: name.into(),
            parent,
            description: String::new(),
        }
    }
}

/// A saved server registration: a friendly name plus the connection profile to
/// use, optionally filed under a [`ServerGroup`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredServer {
    pub id: Uuid,
    pub display_name: String,
    pub group: Option<GroupId>,
    pub profile: ConnectionProfile,
}

impl RegisteredServer {
    pub fn new(display_name: impl Into<String>, profile: ConnectionProfile) -> Self {
        Self {
            id: Uuid::new_v4(),
            display_name: display_name.into(),
            group: None,
            profile,
        }
    }

    pub fn in_group(mut self, group: GroupId) -> Self {
        self.group = Some(group);
        self
    }
}

/// In-memory, serializable store for the Registered Servers tree. The UI layer
/// persists this (e.g. to JSON on disk); the core only owns the model and the
/// invariants (no duplicate group names within a parent, no orphaned servers).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredServerStore {
    groups: Vec<ServerGroup>,
    servers: Vec<RegisteredServer>,
}

impl RegisteredServerStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a group, rejecting a duplicate name under the same parent so the
    /// tree stays unambiguous (SSMS enforces the same rule).
    pub fn add_group(&mut self, group: ServerGroup) -> Result<GroupId, RegisteredServerError> {
        if let Some(parent) = group.parent
            && !self.groups.iter().any(|g| g.id == parent)
        {
            return Err(RegisteredServerError::GroupNotFound(parent));
        }
        if self
            .groups
            .iter()
            .any(|g| g.parent == group.parent && g.name.eq_ignore_ascii_case(&group.name))
        {
            return Err(RegisteredServerError::DuplicateGroupName(group.name));
        }
        let id = group.id;
        self.groups.push(group);
        Ok(id)
    }

    /// Registers a server, validating that its group (if any) exists.
    pub fn add_server(&mut self, server: RegisteredServer) -> Result<Uuid, RegisteredServerError> {
        if let Some(group) = server.group
            && !self.groups.iter().any(|g| g.id == group)
        {
            return Err(RegisteredServerError::GroupNotFound(group));
        }
        let id = server.id;
        self.servers.push(server);
        Ok(id)
    }

    /// Removes a group, all of its descendant groups, and every server filed
    /// under any of them, so the tree never retains orphans.
    pub fn remove_group(&mut self, id: GroupId) {
        let mut doomed = vec![id];
        let mut idx = 0;
        while idx < doomed.len() {
            let current = doomed[idx];
            for child in self.groups.iter().filter(|g| g.parent == Some(current)) {
                doomed.push(child.id);
            }
            idx += 1;
        }
        self.groups.retain(|g| !doomed.contains(&g.id));
        self.servers
            .retain(|s| s.group.map(|g| !doomed.contains(&g)).unwrap_or(true));
    }

    pub fn remove_server(&mut self, id: Uuid) {
        self.servers.retain(|s| s.id != id);
    }

    /// Replaces an existing registered server (matched by its `id`) in place,
    /// preserving ordering. Validates any referenced group still exists.
    /// Returns `ServerNotFound` when no server with that id is registered.
    pub fn replace_server(
        &mut self,
        server: RegisteredServer,
    ) -> Result<(), RegisteredServerError> {
        if let Some(group) = server.group
            && !self.groups.iter().any(|g| g.id == group)
        {
            return Err(RegisteredServerError::GroupNotFound(group));
        }
        let slot = self
            .servers
            .iter_mut()
            .find(|s| s.id == server.id)
            .ok_or(RegisteredServerError::ServerNotFound(server.id))?;
        *slot = server;
        Ok(())
    }

    pub fn groups(&self) -> &[ServerGroup] {
        &self.groups
    }

    pub fn servers(&self) -> &[RegisteredServer] {
        &self.servers
    }

    /// Direct child groups of `parent` (or the root groups when `None`).
    pub fn child_groups(&self, parent: Option<GroupId>) -> Vec<&ServerGroup> {
        self.groups.iter().filter(|g| g.parent == parent).collect()
    }

    /// Servers filed directly under `group` (or ungrouped servers when `None`).
    pub fn servers_in(&self, group: Option<GroupId>) -> Vec<&RegisteredServer> {
        self.servers.iter().filter(|s| s.group == group).collect()
    }

    /// Case-insensitive substring search across server display names and the
    /// underlying server host, for the explorer's filter box.
    pub fn search(&self, query: &str) -> Vec<&RegisteredServer> {
        let needle = query.to_ascii_lowercase();
        self.servers
            .iter()
            .filter(|s| {
                s.display_name.to_ascii_lowercase().contains(&needle)
                    || s.profile.server.to_ascii_lowercase().contains(&needle)
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisteredServerError {
    GroupNotFound(GroupId),
    DuplicateGroupName(String),
    ServerNotFound(Uuid),
}

impl std::fmt::Display for RegisteredServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::GroupNotFound(id) => write!(f, "registered-server group not found: {}", id.0),
            Self::DuplicateGroupName(name) => {
                write!(f, "duplicate registered-server group name: {name}")
            }
            Self::ServerNotFound(id) => write!(f, "registered server not found: {id}"),
        }
    }
}

impl std::error::Error for RegisteredServerError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{ConnectionProfile, SecretProvider, SecretRef};

    fn sample_profile(server: &str) -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "sample",
            server,
            "sa",
            SecretRef {
                provider: SecretProvider::Environment,
                key: "SA_PASSWORD".to_string(),
            },
        )
    }

    #[test]
    fn nests_groups_and_files_servers() {
        let mut store = RegisteredServerStore::new();
        let prod = store
            .add_group(ServerGroup::new("Production", None))
            .unwrap();
        let east = store
            .add_group(ServerGroup::new("East", Some(prod)))
            .unwrap();

        store
            .add_server(
                RegisteredServer::new("east-1", sample_profile("east1.contoso.com")).in_group(east),
            )
            .unwrap();

        assert_eq!(store.child_groups(None).len(), 1);
        assert_eq!(store.child_groups(Some(prod)).len(), 1);
        assert_eq!(store.servers_in(Some(east)).len(), 1);
    }

    #[test]
    fn rejects_server_in_unknown_group() {
        let mut store = RegisteredServerStore::new();
        let err = store
            .add_server(RegisteredServer::new("x", sample_profile("x")).in_group(GroupId::new()))
            .unwrap_err();
        assert!(matches!(err, RegisteredServerError::GroupNotFound(_)));
    }

    #[test]
    fn rejects_duplicate_group_name_under_same_parent() {
        let mut store = RegisteredServerStore::new();
        store.add_group(ServerGroup::new("Dev", None)).unwrap();
        let err = store.add_group(ServerGroup::new("dev", None)).unwrap_err();
        assert!(matches!(err, RegisteredServerError::DuplicateGroupName(_)));
    }

    #[test]
    fn removing_group_cascades_to_descendants_and_servers() {
        let mut store = RegisteredServerStore::new();
        let prod = store
            .add_group(ServerGroup::new("Production", None))
            .unwrap();
        let east = store
            .add_group(ServerGroup::new("East", Some(prod)))
            .unwrap();
        store
            .add_server(RegisteredServer::new("east-1", sample_profile("east1")).in_group(east))
            .unwrap();

        store.remove_group(prod);

        assert!(store.groups().is_empty());
        assert!(store.servers().is_empty());
    }

    #[test]
    fn search_matches_display_name_and_host() {
        let mut store = RegisteredServerStore::new();
        store
            .add_server(RegisteredServer::new(
                "Reporting",
                sample_profile("rpt.contoso.com"),
            ))
            .unwrap();

        assert_eq!(store.search("report").len(), 1);
        assert_eq!(store.search("CONTOSO").len(), 1);
        assert_eq!(store.search("missing").len(), 0);
    }

    #[test]
    fn store_round_trips_through_json() {
        let mut store = RegisteredServerStore::new();
        let g = store.add_group(ServerGroup::new("G", None)).unwrap();
        store
            .add_server(RegisteredServer::new("s", sample_profile("h")).in_group(g))
            .unwrap();

        let json = serde_json::to_string(&store).unwrap();
        let restored: RegisteredServerStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store, restored);
    }
}
