//! Per-cell connections: the choices a cell can run on, the environment
//! badge each one wears, and how a cell's run target becomes the profile the
//! query actually runs under (read-only cells get the database-enforced
//! read-only mode on a cloned profile; the saved profile is never changed).

use gpui::{IntoElement, ParentElement, Styled, div, px};
use sqlly_core::{ConnectionEnvironment, ConnectionProfile, DatabaseType};

use crate::theme::Theme;
use sqlly_services::notebook::ConnectionRef;

/// One connection profile a cell can run on. Only display data, the
/// opaque id and a hash of who it signs in as; never credentials.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectionChoice {
    pub profile_id: String,
    pub label: String,
    pub environment: ConnectionEnvironment,
    pub database_type: DatabaseType,
    pub database: Option<String>,
    /// SHA-256 of the sign-in (method, user, tenant): switching a profile's
    /// login changes it, so another principal's cached rows never show.
    pub login_identity: String,
    /// Safety flags in force (`read_only`, `block_all_but_select`, masking).
    pub policy_flags: std::collections::BTreeSet<String>,
}

impl ConnectionChoice {
    pub fn from_profile(profile: &ConnectionProfile) -> Self {
        Self {
            profile_id: profile.id.0.to_string(),
            label: profile.name.clone(),
            environment: profile.environment,
            database_type: profile.database_type,
            database: profile.database.clone().filter(|d| !d.is_empty()),
            login_identity: login_identity(profile),
            policy_flags: policy_flags(profile),
        }
    }

    /// This profile as connection slot `slot_id` of a notebook.
    pub fn to_ref(&self, slot_id: &str) -> ConnectionRef {
        ConnectionRef {
            id: slot_id.to_string(),
            profile_id: Some(self.profile_id.clone()),
            label: self.label.clone(),
            database: self.database.clone(),
            extra: Default::default(),
        }
    }
}

/// A hash of who `profile` signs in as. Secret references are left out
/// (a rotated password is the same principal).
pub fn login_identity(profile: &ConnectionProfile) -> String {
    use sha2::{Digest, Sha256};
    let mut auth = serde_json::to_value(&profile.authentication).unwrap_or_default();
    strip_secret_refs(&mut auth);
    let entra = profile
        .entra_account
        .as_ref()
        .map(|a| format!("{}|{}", a.account_id, a.tenant_id))
        .unwrap_or_default();
    let digest = Sha256::digest(format!("{auth}\n{entra}").as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn strip_secret_refs(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|key, _| !key.to_ascii_lowercase().contains("secret"));
            map.values_mut().for_each(strip_secret_refs);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(strip_secret_refs),
        _ => {}
    }
}

/// The result-affecting safety settings of `profile`.
pub fn policy_flags(profile: &ConnectionProfile) -> std::collections::BTreeSet<String> {
    let mut flags = std::collections::BTreeSet::new();
    if profile.enforce_read_only {
        flags.insert("read_only".to_string());
    }
    if profile.block_all_but_select {
        flags.insert("block_all_but_select".to_string());
    }
    if profile.masking != Default::default() {
        let masking = serde_json::to_string(&profile.masking).unwrap_or_default();
        flags.insert(format!(
            "masking:{}",
            sqlly_services::notebook::run_record::sql_hash(&masking)
        ));
    }
    flags
}

/// Every saved connection profile, in the store's order.
pub fn local_choices() -> Vec<ConnectionChoice> {
    crate::connection::ConnectionStore::global()
        .list()
        .iter()
        .map(ConnectionChoice::from_profile)
        .collect()
}

pub fn environment_label_key(env: ConnectionEnvironment) -> &'static str {
    match env {
        ConnectionEnvironment::Local => "notebook.shell.environment.local",
        ConnectionEnvironment::Dev => "notebook.shell.environment.dev",
        ConnectionEnvironment::Test => "notebook.shell.environment.test",
        ConnectionEnvironment::Staging => "notebook.shell.environment.staging",
        ConnectionEnvironment::Production => "notebook.shell.environment.production",
        ConnectionEnvironment::Unset => "notebook.shell.environment.unset",
    }
}

/// The app's environment colours (the welcome canvas chips use the same).
pub fn environment_color(env: ConnectionEnvironment) -> gpui::Hsla {
    match env {
        ConnectionEnvironment::Production => Theme::error(),
        ConnectionEnvironment::Staging => Theme::warning(),
        ConnectionEnvironment::Dev | ConnectionEnvironment::Test => Theme::success(),
        ConnectionEnvironment::Local | ConnectionEnvironment::Unset => Theme::text_dim(),
    }
}

/// A small outlined chip naming the environment.
pub fn environment_badge(env: ConnectionEnvironment) -> impl IntoElement {
    let color = environment_color(env);
    div()
        .flex_none()
        .px(px(Theme::scaled(5.0)))
        .rounded(px(Theme::scaled(3.0)))
        .border_1()
        .border_color(color)
        .text_color(color)
        .text_size(px(Theme::font_size_ratio(0.75)))
        .child(crate::localization::text(environment_label_key(env)))
}

/// Where a notebook cell runs: a specific profile (or the tab's connection
/// when `None`) and whether the database's read-only mode applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellRunTarget {
    pub profile_id: Option<uuid::Uuid>,
    pub read_only: bool,
    /// The cell's statement timeout, overriding the profile's.
    pub timeout_seconds: Option<u32>,
    /// Runs on the notebook's local data (its read-only DuckDB workspace).
    pub local: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    /// The cell names a profile this machine does not have.
    MissingProfile(String),
    /// Neither the cell nor the tab has a connection.
    NoConnection,
}

/// The profile a notebook cell run executes under. `lookup` resolves a saved
/// profile by id; `tab_profile` is the tab's (or app's) active profile.
pub fn profile_for_target(
    target: &CellRunTarget,
    lookup: impl Fn(uuid::Uuid) -> Option<ConnectionProfile>,
    tab_profile: Option<&ConnectionProfile>,
) -> Result<ConnectionProfile, TargetError> {
    let mut profile = match target.profile_id {
        Some(id) if tab_profile.is_some_and(|p| p.id.0 == id) => tab_profile.cloned(),
        Some(id) => Some(lookup(id).ok_or_else(|| TargetError::MissingProfile(id.to_string()))?),
        None => tab_profile.cloned(),
    }
    .ok_or(TargetError::NoConnection)?;
    apply_policy(&mut profile, target);
    Ok(profile)
}

/// Apply the cell's execution policy to the profile its run uses.
pub fn apply_policy(profile: &mut ConnectionProfile, target: &CellRunTarget) {
    if target.read_only {
        apply_read_only(profile);
    }
    if let Some(seconds) = target.timeout_seconds {
        profile.statement_timeout_seconds = Some(u64::from(seconds));
    }
}

/// The read-only mode agent queries already run under: the SELECT-only gate
/// plus the mode the database itself enforces.
pub fn apply_read_only(profile: &mut ConnectionProfile) {
    profile.block_all_but_select = true;
    profile.enforce_read_only = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str, env: ConnectionEnvironment) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            name,
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "password".to_string(),
            },
        );
        p.environment = env;
        p
    }

    #[test]
    fn the_login_and_safety_flags_are_part_of_the_choice_but_secrets_are_not() {
        let base = profile("Dev", ConnectionEnvironment::Dev);
        let choice = ConnectionChoice::from_profile(&base);
        assert_eq!(choice.login_identity.len(), 64);
        assert!(!choice.login_identity.contains("sa"));
        let mut rotated = base.clone();
        if let sqlly_core::AuthenticationType::SqlLogin { secret_ref, .. } =
            &mut rotated.authentication
        {
            secret_ref.key = "another-password-ref".into();
        }
        assert_eq!(
            ConnectionChoice::from_profile(&rotated).login_identity,
            choice.login_identity
        );
        let mut other = base.clone();
        if let sqlly_core::AuthenticationType::SqlLogin { username, .. } = &mut other.authentication
        {
            *username = "reporting".into();
        }
        assert_ne!(
            ConnectionChoice::from_profile(&other).login_identity,
            choice.login_identity
        );
        let mut locked = base;
        locked.enforce_read_only = true;
        assert!(
            ConnectionChoice::from_profile(&locked)
                .policy_flags
                .contains("read_only")
        );
    }

    #[test]
    fn cell_profile_wins_over_the_tab_and_read_only_is_a_clone() {
        let tab = profile("Dev", ConnectionEnvironment::Dev);
        let prod = profile("Prod", ConnectionEnvironment::Production);
        let saved = prod.clone();
        let target = CellRunTarget {
            profile_id: Some(prod.id.0),
            read_only: true,
            timeout_seconds: None,
            local: false,
        };
        let target = CellRunTarget {
            timeout_seconds: Some(30),
            ..target
        };
        let resolved = profile_for_target(
            &target,
            |id| (id == saved.id.0).then(|| saved.clone()),
            Some(&tab),
        )
        .unwrap();
        assert_eq!(resolved.statement_timeout_seconds, Some(30));
        assert_eq!(resolved.id, prod.id);
        assert!(resolved.block_all_but_select);
        assert!(resolved.enforce_read_only);
        assert!(!saved.block_all_but_select, "saved profile untouched");
        assert!(!saved.enforce_read_only);
    }

    #[test]
    fn elevated_cell_keeps_the_profile_flags_and_falls_back_to_the_tab() {
        let tab = profile("Dev", ConnectionEnvironment::Dev);
        let target = CellRunTarget {
            profile_id: None,
            read_only: false,
            timeout_seconds: None,
            local: false,
        };
        let resolved = profile_for_target(&target, |_| None, Some(&tab)).unwrap();
        assert_eq!(resolved.id, tab.id);
        assert!(!resolved.block_all_but_select);
        assert!(!resolved.enforce_read_only);
    }

    #[test]
    fn missing_profile_and_no_connection_are_reported() {
        let id = uuid::Uuid::new_v4();
        let target = CellRunTarget {
            profile_id: Some(id),
            read_only: true,
            timeout_seconds: None,
            local: false,
        };
        assert_eq!(
            profile_for_target(&target, |_| None, None),
            Err(TargetError::MissingProfile(id.to_string()))
        );
        let target = CellRunTarget {
            profile_id: None,
            read_only: true,
            timeout_seconds: None,
            local: false,
        };
        assert_eq!(
            profile_for_target(&target, |_| None, None),
            Err(TargetError::NoConnection)
        );
    }
}
