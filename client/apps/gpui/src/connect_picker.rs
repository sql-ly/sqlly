//! Pure, UI-free connection-option shaping shared by picker-style surfaces
//! (today: the welcome canvas's saved-connection rows).
//!
//! Port of `ConnectPickerModel.swift`, adapted to the flat GPUI
//! `ConnectionProfile` model. The Swift picker selected a server + database from
//! a hierarchical `ConnectionStoreData`; the GPUI model is flat — each profile
//! carries its own `database` — so an option maps to a `ConnectionProfile`
//! directly by id.
//!
//! Not migrated to `gpui_component::list::List`: this module is deliberately
//! UI-free (ordering + row shaping only), and its rendering consumers cannot
//! express their behavior through the kit list. The welcome canvas
//! (`crate::welcome`) draws these options as click-to-connect cards with a
//! per-row staggered entrance animation (each row animates its own top margin,
//! so rows do not have the uniform height the kit's virtualized list
//! requires), has no selection state at all — keyboard access is the global
//! Ctrl+Shift+1..9 jump via [`nth_connection_id`], not list-local arrow
//! navigation — and renders from a stateless function, whereas the kit's
//! `ListState` is a persistent, focus-owning entity whose arrow/enter
//! handling would add a selection highlight and focus behavior that the
//! canvas intentionally does not have. The multi-target dialog consumer
//! groups rows under environment headers with per-row checkboxes, which the
//! kit's single-`selected_index` model does not cover either.

use chrono::{DateTime, Utc};
use sqlly_core::{ConnectionEnvironment, ConnectionProfile};
use sqlly_lang::sqlmodel::DatabaseType;

use crate::status_bar::environment_label;

/// A connection choice presented in the picker. Built from a flat
/// `ConnectionProfile`: the display name is the profile's friendly name, and
/// the detail line summarizes server / database / environment / client /
/// project so the user can distinguish profiles that share a name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionOption {
    pub id: String,
    pub display_name: String,
    pub detail: String,
    pub favorite: bool,
    pub database_type: DatabaseType,
    pub environment: ConnectionEnvironment,
    pub last_connected_at: Option<DateTime<Utc>>,
}

/// Builds the connection option list from profiles: favorites first, then
/// most-recently-connected, then the rest by name. Ordering only reads what
/// the store already remembered — nothing dials out. Each profile is shown
/// by its friendly name (`profile.name`), with a detail line summarizing
/// server / database / environment / client / project.
pub fn connection_options(profiles: &[ConnectionProfile]) -> Vec<ConnectionOption> {
    let mut ordered: Vec<&ConnectionProfile> = profiles.iter().collect();
    ordered.sort_by(|a, b| {
        b.favorite
            .cmp(&a.favorite)
            .then_with(|| b.status.last_connected_at.cmp(&a.status.last_connected_at))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    ordered.into_iter().map(option_from_profile).collect()
}

/// 1-based index into the picker's ordered list. `None` when `n` is 0 or past the end.
pub fn nth_connection_id(profiles: &[ConnectionProfile], n: usize) -> Option<String> {
    if n == 0 {
        return None;
    }
    connection_options(profiles)
        .get(n - 1)
        .map(|option| option.id.clone())
}

fn option_from_profile(p: &ConnectionProfile) -> ConnectionOption {
    ConnectionOption {
        id: p.id.0.to_string(),
        display_name: p.name.clone(),
        detail: detail_for(p),
        favorite: p.favorite,
        database_type: p.database_type,
        environment: p.environment,
        last_connected_at: p.status.last_connected_at,
    }
}

fn detail_for(p: &ConnectionProfile) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !p.server.trim().is_empty() {
        parts.push(p.server.trim().to_string());
    }
    if let Some(db) = p.database.as_ref().filter(|d| !d.trim().is_empty()) {
        parts.push(db.trim().to_string());
    }
    if !matches!(
        p.environment,
        ConnectionEnvironment::Local | ConnectionEnvironment::Unset
    ) {
        parts.push(environment_label(p.environment).to_string());
    }
    if let Some(client) = p.client.as_ref().filter(|c| !c.trim().is_empty()) {
        parts.push(client.trim().to_string());
    }
    if let Some(project) = p.project.as_ref().filter(|pr| !pr.trim().is_empty()) {
        parts.push(project.trim().to_string());
    }
    parts.join(" / ")
}

/// One dialog row. `target_id` is `profile.id.0.to_string()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetRow {
    pub target_id: String,
    pub display_name: String,
    pub detail: String,
    pub environment: ConnectionEnvironment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentGroup {
    pub environment: ConnectionEnvironment,
    pub label: &'static str,
    pub rows: Vec<TargetRow>,
}

/// Groups profiles for the "Run on…" dialog: order Local, Dev, Test, Staging, Production, Unset;
/// rows sorted by name (case-insensitive) within a group; empty groups omitted.
pub fn grouped_by_environment(profiles: &[ConnectionProfile]) -> Vec<EnvironmentGroup> {
    use std::collections::HashMap;

    const ORDER: [ConnectionEnvironment; 6] = [
        ConnectionEnvironment::Local,
        ConnectionEnvironment::Dev,
        ConnectionEnvironment::Test,
        ConnectionEnvironment::Staging,
        ConnectionEnvironment::Production,
        ConnectionEnvironment::Unset,
    ];

    let mut by_env: HashMap<ConnectionEnvironment, Vec<TargetRow>> = HashMap::new();
    for p in profiles {
        by_env.entry(p.environment).or_default().push(TargetRow {
            target_id: p.id.0.to_string(),
            display_name: p.name.clone(),
            detail: detail_for(p),
            environment: p.environment,
        });
    }
    for rows in by_env.values_mut() {
        rows.sort_by(|a, b| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
        });
    }
    ORDER
        .into_iter()
        .filter_map(|environment| {
            let rows = by_env.remove(&environment)?;
            if rows.is_empty() {
                return None;
            }
            Some(EnvironmentGroup {
                environment,
                label: environment_label(environment),
                rows,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};

    fn profile(name: &str, server: &str, database: Option<&str>) -> ConnectionProfile {
        let secret = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".to_string(),
        };
        let mut p = ConnectionProfile::sql_login(name, server, "sa", secret);
        p.database = database.map(|s| s.to_string());
        p
    }

    #[test]
    fn empty_profiles_yields_empty_options() {
        assert!(connection_options(&[]).is_empty());
    }

    #[test]
    fn option_carries_display_name() {
        let p = profile("My Prod DB", "sql.example.com", None);
        let opts = connection_options(&[p]);
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].display_name, "My Prod DB");
    }

    #[test]
    fn detail_includes_server_and_database() {
        let p = profile("My DB", "sql.example.com", Some("Orders"));
        let opts = connection_options(&[p]);
        assert!(opts[0].detail.contains("sql.example.com"));
        assert!(opts[0].detail.contains("Orders"));
    }

    #[test]
    fn detail_omits_local_environment() {
        let p = profile("My DB", "host", None);
        let opts = connection_options(&[p]);
        assert!(!opts[0].detail.contains("Local"));
    }

    #[test]
    fn detail_omits_unset_environment() {
        let mut p = profile("My DB", "host", None);
        p.environment = ConnectionEnvironment::Unset;
        let opts = connection_options(&[p]);
        assert!(!opts[0].detail.contains("Unset"));
        assert_eq!(opts[0].detail, "host");
    }

    #[test]
    fn option_carries_type_environment_and_last_connected() {
        let mut p = profile("My DB", "host", None);
        p.environment = ConnectionEnvironment::Staging;
        let when = chrono::Utc::now() - chrono::Duration::minutes(7);
        p.status.record_connected(when);
        let expected_type = p.database_type;
        let opts = connection_options(&[p]);
        assert_eq!(opts[0].database_type, expected_type);
        assert_eq!(opts[0].environment, ConnectionEnvironment::Staging);
        assert_eq!(opts[0].last_connected_at, Some(when));
    }

    #[test]
    fn detail_includes_non_local_environment() {
        let mut p = profile("My DB", "host", None);
        p.environment = ConnectionEnvironment::Production;
        let opts = connection_options(&[p]);
        assert!(opts[0].detail.contains("Production"));
    }

    #[test]
    fn favorites_lead_then_recents_then_names() {
        let mut favorite = profile("Zulu Favorite", "h", None);
        favorite.favorite = true;
        let mut recent = profile("Recent", "h", None);
        recent
            .status
            .record_connected(chrono::Utc::now() - chrono::Duration::minutes(5));
        let mut older = profile("Older", "h", None);
        older
            .status
            .record_connected(chrono::Utc::now() - chrono::Duration::days(3));
        let alpha = profile("Alpha Never Connected", "h", None);
        let beta = profile("beta never connected", "h", None);
        let opts = connection_options(&[alpha, older, favorite, beta, recent]);
        let names: Vec<&str> = opts.iter().map(|o| o.display_name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Zulu Favorite",
                "Recent",
                "Older",
                "Alpha Never Connected",
                "beta never connected",
            ]
        );
        assert!(opts[0].favorite);
        assert!(!opts[1].favorite);
    }

    #[test]
    fn detail_includes_client_and_project() {
        let mut p = profile("My DB", "host", None);
        p.client = Some("Acme".to_string());
        p.project = Some("Apollo".to_string());
        let opts = connection_options(&[p]);
        assert!(opts[0].detail.contains("Acme"));
        assert!(opts[0].detail.contains("Apollo"));
    }

    #[test]
    fn groups_follow_fixed_environment_order_and_omit_empty() {
        let mut dev = profile("Dev DB", "h", None);
        dev.environment = ConnectionEnvironment::Dev;
        let local = profile("Local DB", "h", None);
        let groups = grouped_by_environment(&[dev, local.clone()]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].environment, ConnectionEnvironment::Local);
        assert_eq!(groups[1].environment, ConnectionEnvironment::Dev);
        assert_eq!(groups[0].rows[0].display_name, "Local DB");
    }

    #[test]
    fn rows_sorted_by_name_within_group() {
        let mut a = profile("Zulu", "h", None);
        let mut b = profile("alpha", "h", None);
        a.environment = ConnectionEnvironment::Test;
        b.environment = ConnectionEnvironment::Test;
        let groups = grouped_by_environment(&[a, b]);
        assert_eq!(groups[0].rows[0].display_name, "alpha");
        assert_eq!(groups[0].rows[1].display_name, "Zulu");
    }

    #[test]
    fn nth_follows_picker_order_and_is_none_past_end() {
        let mut favorite = profile("Zulu Favorite", "h", None);
        favorite.favorite = true;
        let mut recent = profile("Recent", "h", None);
        recent
            .status
            .record_connected(chrono::Utc::now() - chrono::Duration::minutes(5));
        let older = profile("Older", "h", None);
        let profiles = vec![older, favorite, recent];
        let first = nth_connection_id(&profiles, 1).unwrap();
        assert_eq!(first, connection_options(&profiles)[0].id);
        assert_eq!(
            nth_connection_id(&profiles, 2).as_deref(),
            Some(connection_options(&profiles)[1].id.as_str())
        );
        assert!(nth_connection_id(&profiles, 0).is_none());
        assert!(nth_connection_id(&profiles, 9).is_none());
        assert!(nth_connection_id(&[], 1).is_none());
    }

    #[test]
    fn production_group_is_last_before_unset() {
        let mut prod = profile("Prod", "h", None);
        prod.environment = ConnectionEnvironment::Production;
        let mut unset = profile("Unset", "h", None);
        unset.environment = ConnectionEnvironment::Unset;
        let groups = grouped_by_environment(&[unset, prod]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].environment, ConnectionEnvironment::Production);
        assert_eq!(groups[1].environment, ConnectionEnvironment::Unset);
    }
}
