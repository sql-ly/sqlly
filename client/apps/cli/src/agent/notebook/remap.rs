//! Connection slots → saved connections on this machine.
//!
//! A notebook names its connections by slot (`connection_refs`), each with
//! an opaque profile id that only means something on the machine that
//! wrote it. A slot is used when its profile id is a saved connection here
//! or when `--connection slot=<name or id>` maps it; nothing is ever
//! matched by label, so opening someone else's notebook can never run it
//! as one of your connections by accident. The reserved slot `local` is
//! the notebook's own DuckDB workspace.

use std::collections::{BTreeMap, BTreeSet};

use sqlly_core::{AuthenticationType, ConnectionProfile, DatabaseType};
use sqlly_services::notebook::fingerprint::DEFAULT_CONNECTION;
use sqlly_services::notebook::{ConnectionIdentity, Dialects, ExecClass, NotebookDoc};

use crate::agent::CliError;
use crate::agent::connections::{StoreFile, is_production, resolve_connection};

/// The slot id a cell uses to run in the notebook's local DuckDB workspace.
pub const LOCAL_SLOT: &str = "local";

#[derive(Debug, Clone)]
pub enum SlotTarget {
    Remote {
        profile: Box<ConnectionProfile>,
        production: bool,
    },
    Local,
}

impl SlotTarget {
    pub fn engine(&self) -> DatabaseType {
        match self {
            SlotTarget::Remote { profile, .. } => profile.database_type,
            SlotTarget::Local => DatabaseType::DuckDb,
        }
    }

    pub fn label(&self) -> String {
        match self {
            SlotTarget::Remote { profile, .. } => profile.name.clone(),
            SlotTarget::Local => "Notebook data".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnmappedReason {
    /// A cell names a slot the notebook does not declare.
    NotDeclared,
    /// The slot carries no profile and no `--connection` maps it.
    NoProfile,
    /// The slot's profile id is not a saved connection here.
    ProfileNotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unmapped {
    pub slot: String,
    pub label: String,
    pub reason: UnmappedReason,
}

impl Unmapped {
    pub fn message(&self) -> String {
        let name = slot_display(&self.slot, &self.label);
        match self.reason {
            UnmappedReason::NotDeclared => {
                format!("connection slot {name} is used by a cell but not declared by the notebook")
            }
            UnmappedReason::NoProfile | UnmappedReason::ProfileNotFound => {
                format!(
                    "connection slot {name} is not mapped to a saved connection on this machine"
                )
            }
        }
    }

    pub fn hint(&self) -> String {
        let slot = if self.slot.is_empty() {
            String::new()
        } else {
            format!("{}=", self.slot)
        };
        format!("pass --connection {slot}<saved connection name or id> (see `connections list`)")
    }
}

fn slot_display(slot: &str, label: &str) -> String {
    match (slot.is_empty(), label.is_empty()) {
        (true, _) => "(default)".to_string(),
        (false, true) => format!("'{slot}'"),
        (false, false) => format!("'{slot}' ({label})"),
    }
}

/// Every slot a notebook can run on, mapped or not.
#[derive(Debug, Clone, Default)]
pub struct Slots {
    pub targets: BTreeMap<String, SlotTarget>,
    pub unmapped: BTreeMap<String, Unmapped>,
}

impl Slots {
    /// The slot key a cell runs on (`""` for the default connection).
    pub fn key_for(doc: &NotebookDoc, cell: &sqlly_services::notebook::Cell) -> String {
        if sqlly_services::notebook::cells::local::targets_local(cell) {
            return LOCAL_SLOT.to_string();
        }
        doc.effective_connection_ref(cell)
            .unwrap_or(DEFAULT_CONNECTION)
            .to_string()
    }

    pub fn get(&self, key: &str) -> Option<&SlotTarget> {
        self.targets.get(key)
    }

    /// Dialects for reference scanning, expansion and binding.
    pub fn dialects(&self) -> Dialects {
        let default = self
            .targets
            .get(DEFAULT_CONNECTION)
            .or_else(|| self.targets.values().next())
            .map_or(DatabaseType::DuckDb, SlotTarget::engine);
        Dialects {
            default,
            by_connection_ref: self
                .targets
                .iter()
                .filter(|(key, _)| !key.is_empty())
                .map(|(key, target)| (key.clone(), target.engine()))
                .collect(),
        }
    }

    /// Fingerprint identities per slot.
    pub fn identities(&self) -> BTreeMap<String, ConnectionIdentity> {
        self.targets
            .iter()
            .map(|(key, target)| (key.clone(), identity(target)))
            .collect()
    }
}

fn identity(target: &SlotTarget) -> ConnectionIdentity {
    match target {
        SlotTarget::Local => ConnectionIdentity {
            profile_id: LOCAL_SLOT.to_string(),
            engine: "DuckDB (notebook data)".to_string(),
            ..ConnectionIdentity::default()
        },
        SlotTarget::Remote {
            profile,
            production,
        } => {
            let mut policy_flags = BTreeSet::new();
            if *production {
                policy_flags.insert("production".to_string());
            }
            ConnectionIdentity {
                profile_id: profile.id.0.to_string(),
                database: profile.database.clone(),
                login_identity: login_identity(&profile.authentication),
                engine: profile.database_type.display_name().to_string(),
                policy_flags,
                session_settings: BTreeMap::new(),
            }
        }
    }
}

/// Who the queries run as: the sign-in kind and user name, never a secret.
fn login_identity(auth: &AuthenticationType) -> String {
    match auth {
        AuthenticationType::SqlLogin { username, .. } => format!("sql:{username}"),
        AuthenticationType::AwsRdsIam { username, .. } => format!("aws-rds-iam:{username}"),
        AuthenticationType::GcpCloudSqlIam { username } => format!("gcp-iam:{username}"),
        AuthenticationType::Integrated => "integrated".to_string(),
        AuthenticationType::EntraDeviceCode | AuthenticationType::EntraBrowser => {
            "entra".to_string()
        }
        AuthenticationType::AuthToken { .. } => "token".to_string(),
        AuthenticationType::PostgresOAuth { .. } => "pg-oauth".to_string(),
    }
}

/// One `--connection` flag: `slot=connection`, or a bare connection for the
/// notebook's only (or default) slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapping {
    pub slot: Option<String>,
    pub connection: String,
}

/// Parse `--connection` flags. `a=b` maps slot `a` when the notebook
/// declares `a`; otherwise the whole text is a connection name (names may
/// contain `=`).
pub fn parse_mappings(doc: &NotebookDoc, flags: &[String]) -> Result<Vec<Mapping>, CliError> {
    let mut out: Vec<Mapping> = Vec::new();
    for flag in flags {
        let mapping = match flag.split_once('=') {
            Some((slot, connection))
                if doc.connection(slot.trim()).is_some() && !connection.trim().is_empty() =>
            {
                Mapping {
                    slot: Some(slot.trim().to_string()),
                    connection: connection.trim().to_string(),
                }
            }
            Some((slot, _))
                if !doc.connection_refs.is_empty()
                    && doc.connection(slot.trim()).is_none()
                    && is_slot_like(slot) =>
            {
                let declared: Vec<&str> =
                    doc.connection_refs.iter().map(|c| c.id.as_str()).collect();
                return Err(CliError::usage(format!(
                    "the notebook has no connection slot '{}' (slots: {})",
                    slot.trim(),
                    declared.join(", ")
                )));
            }
            _ if flag.trim().is_empty() => {
                return Err(CliError::usage("--connection needs a value"));
            }
            _ => Mapping {
                slot: None,
                connection: flag.trim().to_string(),
            },
        };
        let key = mapping.slot.clone();
        if out.iter().any(|m| m.slot == key) {
            return Err(CliError::usage(format!(
                "--connection maps {} more than once",
                key.map_or("the default slot".to_string(), |s| format!("slot '{s}'"))
            )));
        }
        out.push(mapping);
    }
    Ok(out)
}

/// `primary` in `primary=Sales DB` looks like a slot id; `Sales=DB` with a
/// space or other punctuation on the left is taken as a connection name.
fn is_slot_like(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Map every slot the notebook's cells can run on.
pub fn resolve(
    doc: &NotebookDoc,
    mappings: &[Mapping],
    store: &StoreFile,
) -> Result<Slots, CliError> {
    let mut slots = Slots::default();
    let bare = mappings.iter().find(|m| m.slot.is_none());
    if bare.is_some() && doc.connection_refs.len() > 1 {
        return Err(CliError::usage(
            "this notebook has several connection slots; pass --connection <slot>=<connection>",
        ));
    }
    let profile_for = |connection: &str| -> Result<ConnectionProfile, CliError> {
        resolve_connection(store, connection)
    };
    for (index, slot) in doc.connection_refs.iter().enumerate() {
        if slot.id == LOCAL_SLOT {
            slots
                .targets
                .insert(LOCAL_SLOT.to_string(), SlotTarget::Local);
            continue;
        }
        let flagged = mappings
            .iter()
            .find(|m| m.slot.as_deref() == Some(slot.id.as_str()))
            .or(bare.filter(|_| index == 0));
        let profile = match flagged {
            Some(mapping) => Some(profile_for(&mapping.connection)?),
            None => slot.profile_id.as_deref().and_then(|id| {
                store
                    .profiles
                    .iter()
                    .find(|p| p.id.0.to_string().eq_ignore_ascii_case(id.trim()))
                    .cloned()
            }),
        };
        match profile {
            Some(mut profile) => {
                if let Some(database) = slot.database.as_ref().filter(|d| !d.trim().is_empty()) {
                    profile.database = Some(database.clone());
                }
                let production = is_production(store, &profile);
                slots.targets.insert(
                    slot.id.clone(),
                    SlotTarget::Remote {
                        profile: Box::new(profile),
                        production,
                    },
                );
            }
            None => {
                let reason = if slot.profile_id.is_some() {
                    UnmappedReason::ProfileNotFound
                } else {
                    UnmappedReason::NoProfile
                };
                slots.unmapped.insert(
                    slot.id.clone(),
                    Unmapped {
                        slot: slot.id.clone(),
                        label: slot.label.clone(),
                        reason,
                    },
                );
            }
        }
    }
    for cell in &doc.cells {
        let key = Slots::key_for(doc, cell);
        if slots.targets.contains_key(&key) || slots.unmapped.contains_key(&key) {
            continue;
        }
        if key == LOCAL_SLOT {
            slots.targets.insert(key, SlotTarget::Local);
            continue;
        }
        if cell.exec_class() != ExecClass::Remote {
            continue;
        }
        if key == DEFAULT_CONNECTION {
            match bare {
                Some(mapping) => {
                    let profile = profile_for(&mapping.connection)?;
                    let production = is_production(store, &profile);
                    slots.targets.insert(
                        key,
                        SlotTarget::Remote {
                            profile: Box::new(profile),
                            production,
                        },
                    );
                }
                None => {
                    slots.unmapped.insert(
                        key.clone(),
                        Unmapped {
                            slot: key,
                            label: String::new(),
                            reason: UnmappedReason::NoProfile,
                        },
                    );
                }
            }
            continue;
        }
        slots.unmapped.insert(
            key.clone(),
            Unmapped {
                slot: key,
                label: String::new(),
                reason: UnmappedReason::NotDeclared,
            },
        );
    }
    Ok(slots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{ConnectionEnvironment, SecretProvider, SecretRef};
    use sqlly_services::notebook::{CellKind, ConnectionRef, SeededIds};

    fn profile(name: &str, env: ConnectionEnvironment) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            name,
            "localhost",
            "reader",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "secret-marker-never-printed".into(),
            },
        );
        p.name = name.into();
        p.environment = env;
        p.database_type = DatabaseType::Postgres;
        p
    }

    fn store(profiles: Vec<ConnectionProfile>) -> StoreFile {
        StoreFile {
            version: 1,
            profiles,
            environments: vec![],
        }
    }

    fn slot(id: &str, profile_id: Option<String>) -> ConnectionRef {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "profile_id": profile_id,
            "label": format!("{id} label"),
        }))
        .unwrap()
    }

    fn doc_with_slots(slots: Vec<ConnectionRef>, cell_slots: &[Option<&str>]) -> NotebookDoc {
        let mut doc = NotebookDoc::empty("nb", "T");
        doc.connection_refs = slots;
        let mut ids = SeededIds(3);
        for s in cell_slots {
            let i = doc.add_cell(CellKind::Sql, None, &mut ids);
            doc.cells[i].connection_ref = s.map(str::to_string);
            doc.cells[i].source = "SELECT 1".into();
        }
        doc
    }

    fn flags(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn profile_ids_map_only_when_saved_here() {
        let sales = profile("Sales", ConnectionEnvironment::Dev);
        let id = sales.id.0.to_string();
        let doc = doc_with_slots(
            vec![
                slot("primary", Some(id)),
                slot("crm", Some("not-here".into())),
            ],
            &[Some("primary"), Some("crm")],
        );
        let slots = resolve(&doc, &[], &store(vec![sales])).unwrap();
        assert!(matches!(
            slots.get("primary"),
            Some(SlotTarget::Remote { .. })
        ));
        assert_eq!(
            slots.unmapped["crm"].reason,
            UnmappedReason::ProfileNotFound
        );
        assert!(slots.unmapped["crm"].hint().contains("--connection crm="));
    }

    #[test]
    fn flags_remap_slots_and_never_match_by_label() {
        let sales = profile("primary label", ConnectionEnvironment::Dev);
        let other = profile("Other", ConnectionEnvironment::Production);
        let doc = doc_with_slots(
            vec![slot("primary", None), slot("crm", None)],
            &[Some("primary"), Some("crm")],
        );
        let st = store(vec![sales, other]);
        // The saved connection is named like the slot's label: still unmapped.
        let slots = resolve(&doc, &[], &st).unwrap();
        assert_eq!(slots.unmapped.len(), 2);
        let mappings = parse_mappings(&doc, &flags(&["crm=Other"])).unwrap();
        let slots = resolve(&doc, &mappings, &st).unwrap();
        match slots.get("crm") {
            Some(SlotTarget::Remote {
                profile,
                production,
            }) => {
                assert_eq!(profile.name, "Other");
                assert!(*production);
            }
            other => panic!("{other:?}"),
        }
        assert!(slots.unmapped.contains_key("primary"));
    }

    #[test]
    fn bare_connection_maps_the_default_or_only_slot() {
        let sales = profile("Sales", ConnectionEnvironment::Dev);
        let st = store(vec![sales]);
        let v1 = doc_with_slots(vec![], &[None]);
        let mappings = parse_mappings(&v1, &flags(&["Sales"])).unwrap();
        let slots = resolve(&v1, &mappings, &st).unwrap();
        assert!(slots.get("").is_some());
        let one = doc_with_slots(vec![slot("primary", None)], &[None]);
        let mappings = parse_mappings(&one, &flags(&["Sales"])).unwrap();
        assert!(
            resolve(&one, &mappings, &st)
                .unwrap()
                .get("primary")
                .is_some()
        );
        let two = doc_with_slots(vec![slot("a", None), slot("b", None)], &[None]);
        let mappings = parse_mappings(&two, &flags(&["Sales"])).unwrap();
        assert_eq!(
            resolve(&two, &mappings, &st).unwrap_err().exit,
            crate::agent::ExitCode::Usage
        );
    }

    #[test]
    fn bad_flags_are_usage_errors() {
        let doc = doc_with_slots(vec![slot("primary", None)], &[None]);
        for bad in [vec!["nope=Sales"], vec!["primary=A", "primary=B"], vec![""]] {
            let err = parse_mappings(&doc, &flags(&bad)).unwrap_err();
            assert_eq!(err.exit, crate::agent::ExitCode::Usage, "{bad:?}");
        }
        // A connection name containing `=` is not a slot mapping.
        let m = parse_mappings(&doc, &flags(&["Sales = EU"])).unwrap();
        assert_eq!(m[0].slot, None);
    }

    #[test]
    fn local_and_undeclared_slots() {
        let doc = doc_with_slots(
            vec![slot("primary", None)],
            &[Some(LOCAL_SLOT), Some("ghost")],
        );
        let slots = resolve(&doc, &[], &store(vec![])).unwrap();
        assert!(matches!(slots.get(LOCAL_SLOT), Some(SlotTarget::Local)));
        assert_eq!(slots.unmapped["ghost"].reason, UnmappedReason::NotDeclared);
        assert_eq!(
            slots.dialects().by_connection_ref[LOCAL_SLOT],
            DatabaseType::DuckDb
        );
    }

    #[test]
    fn identities_never_carry_secrets() {
        let sales = profile("Sales", ConnectionEnvironment::Dev);
        let doc = doc_with_slots(vec![slot("primary", Some(sales.id.0.to_string()))], &[None]);
        let slots = resolve(&doc, &[], &store(vec![sales])).unwrap();
        let text = serde_json::to_string(&slots.identities()).unwrap();
        assert!(text.contains("sql:reader"));
        assert!(!text.contains("secret-marker-never-printed"));
    }
}
