//! Command IntelliSense state for Redis connections: the vocabulary each
//! server answered (`SQLLY.COMMANDS`, or the bundled snapshot until it
//! does), the key names the explorer last scanned per logical database,
//! and the push of both into every editor tab.
//!
//! This is the command-engine counterpart of the data-model cache in
//! `completion_cache.rs`: a Redis profile has no schema to pull, so
//! `connect_data_model` / `load_data_model` route here instead.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use gpui::Context;
use sqlly_core::{CellValue, ConnectionProfile};
use sqlly_intel::redis_complete::{CommandVocabulary, KnownKey, RedisKeyIndex, bundled};

use super::AppView;
use crate::tool_dialog::{ToolDialogContext, ToolQueryOutcome};

/// `SQLLY.COMMANDS` answers one row per command; Redis Stack lists ~400,
/// so this cap only guards a runaway server.
const COMMANDS_ROW_CAP: usize = 4_000;

/// Everything an editor needs to complete on one Redis connection.
#[derive(Debug, Clone)]
pub struct RedisCompletionContext {
    pub vocabulary: Arc<CommandVocabulary>,
    pub keys: Arc<RedisKeyIndex>,
    /// The logical database the tab is on (`"0"`), which picks the key
    /// names offered.
    pub database: String,
}

impl Default for RedisCompletionContext {
    fn default() -> Self {
        Self {
            vocabulary: bundled_vocabulary(),
            keys: Arc::new(RedisKeyIndex::new()),
            database: "0".to_string(),
        }
    }
}

/// The bundled snapshot behind one shared `Arc`, so every editor and
/// profile without a server answer points at the same parse.
pub fn bundled_vocabulary() -> Arc<CommandVocabulary> {
    static SHARED: OnceLock<Arc<CommandVocabulary>> = OnceLock::new();
    SHARED.get_or_init(|| Arc::new(bundled().clone())).clone()
}

/// Per-profile vocabulary and key index, kept for the session.
#[derive(Debug, Default)]
pub struct RedisIntelliSenseState {
    vocabularies: HashMap<uuid::Uuid, Arc<CommandVocabulary>>,
    keys: HashMap<uuid::Uuid, Arc<RedisKeyIndex>>,
    in_flight: HashSet<uuid::Uuid>,
}

impl RedisIntelliSenseState {
    /// The context for `profile`: its server vocabulary when one arrived,
    /// else the bundle; its scanned keys; the profile's database.
    pub fn context_for(&self, profile: &ConnectionProfile) -> Arc<RedisCompletionContext> {
        Arc::new(RedisCompletionContext {
            vocabulary: self
                .vocabularies
                .get(&profile.id.0)
                .cloned()
                .unwrap_or_else(bundled_vocabulary),
            keys: self
                .keys
                .get(&profile.id.0)
                .cloned()
                .unwrap_or_else(|| Arc::new(RedisKeyIndex::new())),
            database: profile.database.clone().unwrap_or_else(|| "0".to_string()),
        })
    }

    pub fn has_vocabulary(&self, profile_id: uuid::Uuid) -> bool {
        self.vocabularies.contains_key(&profile_id)
    }

    pub fn set_vocabulary(&mut self, profile_id: uuid::Uuid, vocabulary: CommandVocabulary) {
        self.vocabularies.insert(profile_id, Arc::new(vocabulary));
    }

    /// Fold a scan's keys into the profile's index for `database`.
    pub fn merge_keys(
        &mut self,
        profile_id: uuid::Uuid,
        database: &str,
        keys: impl IntoIterator<Item = KnownKey>,
    ) {
        let index = self.keys.entry(profile_id).or_default();
        Arc::make_mut(index).merge(database, keys);
    }

    fn begin_fetch(&mut self, profile_id: uuid::Uuid) -> bool {
        self.in_flight.insert(profile_id)
    }

    fn end_fetch(&mut self, profile_id: uuid::Uuid) {
        self.in_flight.remove(&profile_id);
    }
}

/// Parse a `SQLLY.COMMANDS` outcome into a vocabulary; `None` when the
/// server refused `COMMAND` or answered nothing (the bundle stays).
pub fn vocabulary_from_outcome(outcome: &ToolQueryOutcome) -> Option<CommandVocabulary> {
    if outcome.error.is_some() {
        return None;
    }
    let set = outcome.sets.first()?;
    CommandVocabulary::from_server_rows(set.rows.iter().filter_map(|row| {
        match (row.first()?, row.get(1)?, row.get(2)?) {
            (CellValue::String(name), CellValue::String(docs), CellValue::String(info)) => {
                Some((name.as_str(), docs.as_str(), info.as_str()))
            }
            _ => None,
        }
    }))
}

impl AppView {
    /// The completion context for the active Redis profile (bundled
    /// vocabulary and no keys when nothing is connected).
    pub(super) fn redis_completion_context(&self) -> Arc<RedisCompletionContext> {
        match &self.active_profile {
            Some(profile) if profile.database_type.is_command_engine() => {
                self.redis_intellisense.context_for(profile)
            }
            _ => Arc::new(RedisCompletionContext::default()),
        }
    }

    /// Push the active profile's context to every editor tab.
    pub(super) fn push_redis_completion_to_editors(&mut self, cx: &mut Context<Self>) {
        let context = self.redis_completion_context();
        for tab in &self.tabs {
            tab.editor.update(cx, |e, cx| {
                e.set_redis_completion(context.clone(), cx);
            });
        }
    }

    /// Connect-time (and database-switch) path for a Redis profile: push
    /// what is known now, then ask the server for its own command docs
    /// once per profile per session. A refusal (ACL, or a server older
    /// than 7.0) leaves the bundled vocabulary in place.
    pub(super) fn connect_redis_intellisense(
        &mut self,
        profile: ConnectionProfile,
        cx: &mut Context<Self>,
    ) {
        // Whatever SQL catalog the previous connection left live describes
        // another server; clear it, and let the push stamp the engine on
        // every editor so they switch to command completion.
        self.completion_catalog = Arc::new(super::completion_cache::merge_user_snippets(
            sqlly_intel::intellisense::CompletionCatalog::default(),
            &self.snippet_prefs,
        ));
        self.completion_procedures = Arc::new(Vec::new());
        self.completion_snapshot = Arc::new(sqlly_lang::sqlmodel::ModelSnapshot::empty());
        self.push_completion_state_to_editors(cx);
        self.push_redis_completion_to_editors(cx);
        let profile_id = profile.id.0;
        if self.redis_intellisense.has_vocabulary(profile_id)
            || !self.redis_intellisense.begin_fetch(profile_id)
        {
            return;
        }
        let services = self.services_for_profile(&profile);
        let context = ToolDialogContext::new(profile, services);
        crate::tool_dialog::run_query_with_cap(
            &context,
            "SQLLY.COMMANDS".to_string(),
            COMMANDS_ROW_CAP,
            cx,
            move |this, outcome, cx| {
                this.redis_intellisense.end_fetch(profile_id);
                match vocabulary_from_outcome(&outcome) {
                    Some(vocabulary) => {
                        this.log.log_verbose(
                            "intellisense",
                            &format!(
                                "Redis command vocabulary from the server: {} commands",
                                vocabulary.len()
                            ),
                        );
                        this.redis_intellisense
                            .set_vocabulary(profile_id, vocabulary);
                        if this.active_profile.as_ref().map(|p| p.id.0) == Some(profile_id) {
                            this.push_redis_completion_to_editors(cx);
                        }
                    }
                    None => this.log.log_verbose(
                        "intellisense",
                        &format!(
                            "Redis command docs unavailable, keeping the bundled vocabulary: {}",
                            outcome
                                .error
                                .as_deref()
                                .unwrap_or("empty COMMAND DOCS reply")
                        ),
                    ),
                }
            },
        );
    }

    /// The explorer finished a key scan: remember the names for key-slot
    /// completion and refresh the editors when the scan was on the active
    /// profile.
    pub(super) fn merge_redis_keys_from_scan(
        &mut self,
        profile_id: uuid::Uuid,
        database: &str,
        keys: Vec<KnownKey>,
        cx: &mut Context<Self>,
    ) {
        self.redis_intellisense
            .merge_keys(profile_id, database, keys);
        if self.active_profile.as_ref().map(|p| p.id.0) == Some(profile_id) {
            self.push_redis_completion_to_editors(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_dialog::ToolQuerySet;

    fn profile(id: uuid::Uuid, database: Option<&str>) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "r",
            "localhost",
            "",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.id = sqlly_core::ConnectionId(id);
        p.database_type = sqlly_core::DatabaseType::Redis;
        p.database = database.map(str::to_string);
        p
    }

    #[test]
    fn context_falls_back_to_the_bundle_and_tracks_keys_per_profile_and_database() {
        let mut state = RedisIntelliSenseState::default();
        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        let ctx = state.context_for(&profile(a, Some("3")));
        assert!(ctx.vocabulary.get("HSET").is_some());
        assert_eq!(ctx.database, "3");
        assert!(ctx.keys.is_empty());
        state.merge_keys(
            a,
            "3",
            vec![KnownKey {
                name: "user:1".into(),
                kind: "hash".into(),
            }],
        );
        assert_eq!(state.context_for(&profile(a, Some("3"))).keys.len("3"), 1);
        assert_eq!(state.context_for(&profile(b, Some("3"))).keys.len("3"), 0);
        assert_eq!(state.context_for(&profile(a, None)).database, "0");
    }

    #[test]
    fn server_rows_become_the_profiles_vocabulary_and_errors_keep_the_bundle() {
        let row = |name: &str, docs: &str, info: &str| {
            vec![
                CellValue::String(name.into()),
                CellValue::String(docs.into()),
                CellValue::String(info.into()),
            ]
        };
        let outcome = ToolQueryOutcome {
            sets: vec![ToolQuerySet {
                columns: Vec::new(),
                rows: vec![
                    row(
                        "GET",
                        r#"{"summary":"Returns a value.","group":"string","arguments":[{"name":"key","type":"key"}]}"#,
                        r#"["get",2,["readonly"],1,1,1,["@read"],[],[],[]]"#,
                    ),
                    row(
                        "MYMODULE.DO",
                        "{}",
                        r#"["mymodule.do",-1,["write","module"],0,0,0,[],[],[],[]]"#,
                    ),
                ],
            }],
            error: None,
        };
        let vocabulary = vocabulary_from_outcome(&outcome).expect("parsed");
        assert!(vocabulary.get("GET").is_some());
        assert!(
            vocabulary.get("MYMODULE.DO").is_some(),
            "server-only commands complete"
        );
        assert!(vocabulary.get("HSET").is_none());
        assert!(vocabulary.get("SQLLY.KEYS").is_some());
        let mut state = RedisIntelliSenseState::default();
        let id = uuid::Uuid::new_v4();
        state.set_vocabulary(id, vocabulary);
        assert!(state.has_vocabulary(id));
        assert!(
            state
                .context_for(&profile(id, None))
                .vocabulary
                .get("MYMODULE.DO")
                .is_some()
        );
        let refused = ToolQueryOutcome {
            sets: Vec::new(),
            error: Some("NOPERM this user has no permissions to run the 'command' command".into()),
        };
        assert!(vocabulary_from_outcome(&refused).is_none());
        assert!(vocabulary_from_outcome(&ToolQueryOutcome::default()).is_none());
    }
}
