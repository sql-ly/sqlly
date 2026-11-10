//! Server explorer grouping — buckets connections under synthetic group
//! folders by the chosen dimension (client / project / environment / server).
//!
//! Port of `EntraServerGrouping.swift`. The Swift version groups `EntraNode`s
//! (a hierarchical type with Azure override resolution). The GPUI app uses a
//! flat `ConnectionProfile` list, so this module buckets profiles directly:
//! each profile's `client` / `project` / `environment` / `name` field is the
//! bucket key. The algorithm (preserve first-seen order within buckets, sort
//! labels case-insensitively with "Unassigned" pinned last) is identical.

use sqlly_core::{ConnectionEnvironment, ConnectionProfile};

use crate::status_bar::environment_label;

/// The grouping dimension selected in the explorer's toolbar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServerGroupingMode {
    None,
    ByClient,
    ByProject,
    ByEnvironment,
    ByServer,
}

/// `None` is the default grouping, matching Swift's
/// `ConnectionStoreData.serverGrouping = .none` default. Enables
/// `#[serde(default)]` on the persisted store field.
impl Default for ServerGroupingMode {
    fn default() -> Self {
        Self::None
    }
}

impl ServerGroupingMode {
    pub fn all() -> &'static [ServerGroupingMode] {
        &[
            Self::None,
            Self::ByClient,
            Self::ByProject,
            Self::ByEnvironment,
            Self::ByServer,
        ]
    }

    #[allow(dead_code)]
    pub fn display_name(self) -> String {
        crate::localization::text(match self {
            Self::None => "connections.grouping.no_grouping",
            Self::ByClient => "connections.grouping.by_client",
            Self::ByProject => "connections.grouping.by_project",
            Self::ByEnvironment => "connections.grouping.by_environment",
            Self::ByServer => "connections.grouping.by_server",
        })
    }
}

/// The label shown for servers that have no value in the active grouping
/// dimension.
pub fn unassigned_label() -> String {
    crate::localization::text("connections.grouping.unassigned")
}

/// Stable id prefix for synthetic group folders so they never collide with
/// real connection ids.
pub const GROUP_ID_PREFIX: &str = "group";

/// The resolved grouping assignment for one profile. Each field is `None`
/// when the profile sets no value in that dimension.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupingAssignment {
    pub client_name: Option<String>,
    pub project_name: Option<String>,
    pub environment_name: Option<String>,
    pub server_name: Option<String>,
}

/// A synthetic group folder containing the indices of profiles that belong
/// to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileGroup {
    pub id: String,
    pub label: String,
    pub profile_indices: Vec<usize>,
}

/// The grouped result: either flat (no grouping) or a list of group folders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupedProfiles {
    Flat(Vec<usize>),
    Grouped(Vec<ProfileGroup>),
}

impl GroupedProfiles {
    #[allow(dead_code)]
    pub fn is_grouped(&self) -> bool {
        matches!(self, Self::Grouped(_))
    }

    #[allow(dead_code)]
    pub fn group_count(&self) -> usize {
        match self {
            Self::Flat(_) => 0,
            Self::Grouped(g) => g.len(),
        }
    }

    #[allow(dead_code)]
    pub fn total_profiles(&self) -> usize {
        match self {
            Self::Flat(indices) => indices.len(),
            Self::Grouped(groups) => groups.iter().map(|g| g.profile_indices.len()).sum(),
        }
    }
}

/// Resolves the grouping assignment for a single flat profile.
pub fn assignment_for(profile: &ConnectionProfile) -> GroupingAssignment {
    GroupingAssignment {
        client_name: profile
            .client
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        project_name: profile
            .project
            .as_ref()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        environment_name: environment_name_for(profile.environment),
        server_name: {
            let name = profile.name.trim();
            if name.is_empty() {
                let server = profile.server.trim();
                if server.is_empty() {
                    None
                } else {
                    Some(server.to_string())
                }
            } else {
                Some(name.to_string())
            }
        },
    }
}

/// The bucket label for a profile under a given mode. Returns `None` only
/// for `None` mode. Empty/whitespace values fall back to `unassigned_label()`.
pub fn bucket_label(assignment: &GroupingAssignment, mode: ServerGroupingMode) -> Option<String> {
    let raw = match mode {
        ServerGroupingMode::None => return None,
        ServerGroupingMode::ByClient => &assignment.client_name,
        ServerGroupingMode::ByProject => &assignment.project_name,
        ServerGroupingMode::ByEnvironment => &assignment.environment_name,
        ServerGroupingMode::ByServer => &assignment.server_name,
    };
    match raw {
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Some(unassigned_label().to_string())
            } else {
                Some(trimmed.to_string())
            }
        }
        None => Some(unassigned_label().to_string()),
    }
}

/// Groups profiles by the selected dimension. For `None` mode, returns
/// `Flat` with all indices. Otherwise returns `Grouped` with synthetic
/// group folders sorted case-insensitively, `Unassigned` pinned last.
pub fn group(profiles: &[ConnectionProfile], mode: ServerGroupingMode) -> GroupedProfiles {
    if mode == ServerGroupingMode::None {
        return GroupedProfiles::Flat((0..profiles.len()).collect());
    }

    let mut order: Vec<String> = Vec::new();
    let mut buckets: std::collections::HashMap<String, Vec<usize>> =
        std::collections::HashMap::new();

    for (i, p) in profiles.iter().enumerate() {
        let assign = assignment_for(p);
        let label = bucket_label(&assign, mode).unwrap_or_else(|| unassigned_label().to_string());
        if !buckets.contains_key(&label) {
            order.push(label.clone());
        }
        buckets.entry(label).or_default().push(i);
    }

    order.sort_by(|lhs, rhs| {
        if lhs == &unassigned_label() {
            std::cmp::Ordering::Greater
        } else if rhs == &unassigned_label() {
            std::cmp::Ordering::Less
        } else {
            lhs.to_lowercase().cmp(&rhs.to_lowercase())
        }
    });

    let groups = order
        .into_iter()
        .map(|label| {
            let id = format!("{}:{}:{}", GROUP_ID_PREFIX, mode_id_str(mode), label);
            let indices = buckets.remove(&label).unwrap_or_default();
            ProfileGroup {
                id,
                label,
                profile_indices: indices,
            }
        })
        .collect();

    GroupedProfiles::Grouped(groups)
}

fn mode_id_str(mode: ServerGroupingMode) -> &'static str {
    match mode {
        ServerGroupingMode::None => "none",
        ServerGroupingMode::ByClient => "byClient",
        ServerGroupingMode::ByProject => "byProject",
        ServerGroupingMode::ByEnvironment => "byEnvironment",
        ServerGroupingMode::ByServer => "byServer",
    }
}

fn environment_name_for(env: ConnectionEnvironment) -> Option<String> {
    match env {
        ConnectionEnvironment::Local => None,
        ConnectionEnvironment::Unset => None,
        other => Some(environment_label(other).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{ConnectionEnvironment, ConnectionProfile, SecretProvider, SecretRef};

    fn profile(name: &str, server: &str) -> ConnectionProfile {
        let secret = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".into(),
        };
        ConnectionProfile::sql_login(name, server, "sa", secret)
    }

    fn profile_with(
        name: &str,
        server: &str,
        client: Option<&str>,
        project: Option<&str>,
        env: ConnectionEnvironment,
    ) -> ConnectionProfile {
        let mut p = profile(name, server);
        p.client = client.map(String::from);
        p.project = project.map(String::from);
        p.environment = env;
        p
    }

    #[test]
    fn mode_display_names() {
        assert_eq!(ServerGroupingMode::None.display_name(), "No Grouping");
        assert_eq!(ServerGroupingMode::ByClient.display_name(), "By Client");
        assert_eq!(ServerGroupingMode::ByProject.display_name(), "By Project");
        assert_eq!(
            ServerGroupingMode::ByEnvironment.display_name(),
            "By Environment"
        );
        assert_eq!(ServerGroupingMode::ByServer.display_name(), "By Server");
    }

    #[test]
    fn mode_all_covers_five() {
        assert_eq!(ServerGroupingMode::all().len(), 5);
    }

    #[test]
    fn assignment_for_populates_all_fields() {
        let p = profile_with(
            "My DB",
            "host",
            Some("Acme"),
            Some("Apollo"),
            ConnectionEnvironment::Production,
        );
        let a = assignment_for(&p);
        assert_eq!(a.client_name.as_deref(), Some("Acme"));
        assert_eq!(a.project_name.as_deref(), Some("Apollo"));
        assert_eq!(a.environment_name.as_deref(), Some("Production"));
        assert_eq!(a.server_name.as_deref(), Some("My DB"));
    }

    #[test]
    fn assignment_local_environment_is_none() {
        let p = profile("My DB", "host");
        let a = assignment_for(&p);
        assert!(a.environment_name.is_none());
    }

    #[test]
    fn assignment_server_falls_back_to_host_when_name_blank() {
        let mut p = profile("  ", "my-host");
        p.name = "  ".to_string();
        let a = assignment_for(&p);
        assert_eq!(a.server_name.as_deref(), Some("my-host"));
    }

    #[test]
    fn bucket_label_none_mode_returns_none() {
        let a = GroupingAssignment::default();
        assert!(bucket_label(&a, ServerGroupingMode::None).is_none());
    }

    #[test]
    fn bucket_label_empty_falls_back_to_unassigned() {
        let a = GroupingAssignment {
            client_name: Some("  ".into()),
            ..Default::default()
        };
        assert_eq!(
            bucket_label(&a, ServerGroupingMode::ByClient),
            Some(unassigned_label().to_string())
        );
    }

    #[test]
    fn bucket_label_none_value_is_unassigned() {
        let a = GroupingAssignment::default();
        assert_eq!(
            bucket_label(&a, ServerGroupingMode::ByClient),
            Some(unassigned_label().to_string())
        );
    }

    #[test]
    fn group_none_mode_is_flat() {
        let profiles = vec![profile("a", "h1"), profile("b", "h2")];
        let result = group(&profiles, ServerGroupingMode::None);
        assert!(matches!(result, GroupedProfiles::Flat(_)));
        assert_eq!(result.total_profiles(), 2);
        assert!(!result.is_grouped());
    }

    #[test]
    fn group_by_client_buckets_correctly() {
        let profiles = vec![
            profile_with("a", "h1", Some("Acme"), None, ConnectionEnvironment::Local),
            profile_with("b", "h2", Some("Beta"), None, ConnectionEnvironment::Local),
            profile_with("c", "h3", Some("Acme"), None, ConnectionEnvironment::Local),
        ];
        let result = group(&profiles, ServerGroupingMode::ByClient);
        let groups = match result {
            GroupedProfiles::Grouped(g) => g,
            _ => panic!("expected grouped"),
        };
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].label, "Acme");
        assert_eq!(groups[0].profile_indices.len(), 2);
        assert_eq!(groups[1].label, "Beta");
        assert_eq!(groups[1].profile_indices.len(), 1);
    }

    #[test]
    fn group_unassigned_pinned_last() {
        let profiles = vec![
            profile_with("a", "h1", Some("Zeta"), None, ConnectionEnvironment::Local),
            profile_with("b", "h2", None, None, ConnectionEnvironment::Local),
            profile_with("c", "h3", Some("Alpha"), None, ConnectionEnvironment::Local),
        ];
        let result = group(&profiles, ServerGroupingMode::ByClient);
        let groups = match result {
            GroupedProfiles::Grouped(g) => g,
            _ => panic!("expected grouped"),
        };
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].label, "Alpha");
        assert_eq!(groups[1].label, "Zeta");
        assert_eq!(groups[2].label, unassigned_label());
    }

    #[test]
    fn group_by_environment_excludes_local() {
        let profiles = vec![
            profile_with("a", "h1", None, None, ConnectionEnvironment::Production),
            profile_with("b", "h2", None, None, ConnectionEnvironment::Local),
            profile_with("c", "h3", None, None, ConnectionEnvironment::Dev),
        ];
        let result = group(&profiles, ServerGroupingMode::ByEnvironment);
        let groups = match result {
            GroupedProfiles::Grouped(g) => g,
            _ => panic!("expected grouped"),
        };
        assert_eq!(groups.len(), 3);
        let labels: Vec<&str> = groups.iter().map(|g| g.label.as_str()).collect();
        assert!(labels.contains(&"Dev"));
        assert!(labels.contains(&"Production"));
        assert!(labels.contains(&unassigned_label().as_str()));
    }

    #[test]
    fn group_by_server_uses_friendly_name() {
        let profiles = vec![
            profile_with("Prod DB", "h1", None, None, ConnectionEnvironment::Local),
            profile_with("Dev DB", "h2", None, None, ConnectionEnvironment::Local),
        ];
        let result = group(&profiles, ServerGroupingMode::ByServer);
        let groups = match result {
            GroupedProfiles::Grouped(g) => g,
            _ => panic!("expected grouped"),
        };
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].label, "Dev DB");
        assert_eq!(groups[1].label, "Prod DB");
    }

    #[test]
    fn group_preserves_order_within_bucket() {
        let profiles = vec![
            profile_with("a", "h1", Some("Acme"), None, ConnectionEnvironment::Local),
            profile_with("b", "h2", Some("Acme"), None, ConnectionEnvironment::Local),
            profile_with("c", "h3", Some("Acme"), None, ConnectionEnvironment::Local),
        ];
        let result = group(&profiles, ServerGroupingMode::ByClient);
        let groups = match result {
            GroupedProfiles::Grouped(g) => g,
            _ => panic!("expected grouped"),
        };
        assert_eq!(groups[0].profile_indices, vec![0, 1, 2]);
    }

    #[test]
    fn group_id_includes_prefix_and_mode() {
        let profiles = vec![profile_with(
            "a",
            "h1",
            Some("Acme"),
            None,
            ConnectionEnvironment::Local,
        )];
        let result = group(&profiles, ServerGroupingMode::ByClient);
        let groups = match result {
            GroupedProfiles::Grouped(g) => g,
            _ => panic!("expected grouped"),
        };
        assert!(groups[0].id.starts_with("group:byClient:"));
    }

    #[test]
    fn group_total_profiles_preserved() {
        let profiles = vec![
            profile_with("a", "h1", Some("A"), None, ConnectionEnvironment::Local),
            profile_with("b", "h2", Some("B"), None, ConnectionEnvironment::Local),
            profile_with("c", "h3", Some("A"), None, ConnectionEnvironment::Local),
        ];
        let result = group(&profiles, ServerGroupingMode::ByClient);
        assert_eq!(result.total_profiles(), 3);
    }

    #[test]
    fn group_empty_profiles() {
        let result = group(&[], ServerGroupingMode::ByClient);
        assert_eq!(result.group_count(), 0);
        assert_eq!(result.total_profiles(), 0);
    }

    #[test]
    fn group_case_insensitive_sort() {
        let profiles = vec![
            profile_with(
                "a",
                "h1",
                Some("banana"),
                None,
                ConnectionEnvironment::Local,
            ),
            profile_with("b", "h2", Some("Apple"), None, ConnectionEnvironment::Local),
        ];
        let result = group(&profiles, ServerGroupingMode::ByClient);
        let groups = match result {
            GroupedProfiles::Grouped(g) => g,
            _ => panic!("expected grouped"),
        };
        assert_eq!(groups[0].label, "Apple");
        assert_eq!(groups[1].label, "banana");
    }
}
