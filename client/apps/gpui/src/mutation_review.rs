//! Mutation review — pre-execution analysis of SQL that changes data.
//!
//! Port of `QueryMutationReview.swift`. Pure SQL scanning: detects
//! UPDATE/INSERT/DELETE/TRUNCATE/MERGE statements, flags unbounded
//! UPDATE/DELETE (no WHERE clause), masks comments and string literals so
//! keywords inside strings or comments don't produce false positives, and
//! splits batches on top-level semicolons. Fully unit-tested.
//!
//! The Swift `QueryMutationReviewPolicy::effectiveSafety` walked a hierarchical
//! `ConnectionStoreData` (server→database→environment→client→project safety
//! features). The GPUI app has a flat `ConnectionProfile` that carries
//! `auto_wrap_rollback` and `block_all_but_select` directly, so the policy
//! collapses to reading those flags.

use sqlly_core::ConnectionProfile;

/// Which kind of data-changing verb was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChangeType {
    Update,
    Insert,
    Delete,
    Truncate,
    Merge,
}

impl ChangeType {
    pub fn display_name(self) -> &'static str {
        match self {
            ChangeType::Update => "UPDATE",
            ChangeType::Insert => "INSERT",
            ChangeType::Delete => "DELETE",
            ChangeType::Truncate => "TRUNCATE",
            ChangeType::Merge => "MERGE",
        }
    }
}

/// One data-changing statement found in the batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementFinding {
    pub change_type: ChangeType,
    pub has_where_clause: bool,
}

impl StatementFinding {
    /// An UPDATE or DELETE without a WHERE clause affects every row.
    pub fn is_unbounded_dangerous(&self) -> bool {
        matches!(self.change_type, ChangeType::Update | ChangeType::Delete)
            && !self.has_where_clause
    }
}

/// Result of analyzing a SQL batch for data-changing operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Analysis {
    pub statements: Vec<StatementFinding>,
    /// Why the batch cannot be read reliably (a NUL byte, an unterminated
    /// literal or comment under the engine's default reading, …). An
    /// ambiguous batch always asks for confirmation: what it changes cannot
    /// be verified.
    pub ambiguity: Option<String>,
}

impl Analysis {
    pub fn change_types(&self) -> Vec<ChangeType> {
        let mut seen = Vec::new();
        for s in &self.statements {
            if !seen.contains(&s.change_type) {
                seen.push(s.change_type);
            }
        }
        seen
    }

    pub fn has_dangerous_unbounded_change(&self) -> bool {
        self.ambiguity.is_some() || self.statements.iter().any(|s| s.is_unbounded_dangerous())
    }

    /// Plain-language "bottom line" for the confirmation dialog, e.g.
    /// "WE WILL MAKE UPDATE CHANGES." or "WE WILL MAKE UPDATE, DELETE, AND
    /// TRUNCATE CHANGES."
    pub fn bottom_line(&self) -> String {
        if self.statements.is_empty() && self.ambiguity.is_some() {
            return crate::localization::text("results.mutation_review.unknown_changes");
        }
        let names: Vec<&str> = self
            .change_types()
            .iter()
            .map(|c| c.display_name())
            .collect();
        match names.as_slice() {
            [] => crate::localization::text("results.mutation_review.data_changes"),
            [name] => crate::localization::format(
                "results.mutation_review.one_change_type",
                &[("change_type", name)],
            ),
            [first, second] => crate::localization::format(
                "results.mutation_review.two_change_types",
                &[("first_type", first), ("second_type", second)],
            ),
            _ => {
                let (last, rest) = names.split_last().unwrap();
                crate::localization::format(
                    "results.mutation_review.multiple_change_types",
                    &[
                        ("leading_types", rest.join(", ").as_str()),
                        ("last_type", *last),
                    ],
                )
            }
        }
    }

    /// Rule-based summary shown above the AI explanation in the review panel.
    #[allow(dead_code)]
    pub fn rule_based_summary(&self) -> String {
        let names: Vec<&str> = self
            .change_types()
            .iter()
            .map(|c| c.display_name())
            .collect();
        let mut lines = vec![crate::localization::format(
            "results.mutation_review.change_explanation",
            &[("change_types", &names.join(", "))],
        )];
        if self.has_dangerous_unbounded_change() {
            lines.push(crate::localization::text(
                "results.mutation_review.unbounded_warning",
            ));
        }
        lines.join("\n")
    }
}

/// Analyze a SQL batch whose destination engine is unknown: read under every
/// engine's rules (a verb any engine would run is found) and with no routine
/// definition skipped. Prefer [`analyze_for`]; every app caller knows its
/// engine, so this is the conservative fallback kept for engine-less use.
#[allow(dead_code)]
pub fn analyze(sql: &str) -> Option<Analysis> {
    analyze_in(None, sql)
}

/// Analyze a SQL batch bound for `db`. Returns `None` when no data-changing
/// statements are found and the batch reads unambiguously.
///
/// The batch is read through the security lexer (`sqlly_lang::seclex`): split
/// into the execution units the driver sends, each read under every lexical
/// interpretation the engine could apply (sql_mode, standard_conforming_
/// strings, executable-comment versions, …), so literals, quoted identifiers
/// and comments are bounded exactly as the server bounds them and a verb any
/// reading exposes is found. Routine *definitions* are skipped only where the
/// definition's extent is exact — a T-SQL `GO` batch that is a CREATE/ALTER
/// PROCEDURE/FUNCTION/TRIGGER (the body runs to `GO`), and an Oracle or
/// libSQL unit the driver sends as one statement — since deploying a
/// definition executes none of its body's DML.
pub fn analyze_for(db: sqlly_core::DatabaseType, sql: &str) -> Option<Analysis> {
    analyze_in(Some(db), sql)
}

fn analyze_in(db: Option<sqlly_core::DatabaseType>, sql: &str) -> Option<Analysis> {
    use sqlly_core::ConnectionKind;
    let skip_definitions = db.is_some_and(|db| {
        matches!(
            db.connection_kind(),
            ConnectionKind::Tds | ConnectionKind::OracleNet | ConnectionKind::HranaHttp
        )
    });
    let mut findings = Vec::new();
    let mut ambiguity = None;
    for (_text, views) in review_views(db, sql) {
        if ambiguity.is_none() {
            ambiguity = views.ambiguity();
        }
        let mut unit_findings: Vec<StatementFinding> = Vec::new();
        for view in &views.views {
            let masked = mask_for_review(&view.shadow);
            let batches = if view.engine == sqlly_lang::seclex::LexEngine::TSql {
                split_go_batches(&masked)
            } else {
                vec![masked.as_str()]
            };
            let mut view_findings = Vec::new();
            for batch in batches {
                if skip_definitions && is_definition_batch(batch) {
                    continue;
                }
                for statement in split_statements(batch) {
                    view_findings.extend(statement_findings(&statement));
                }
            }
            // One finding list per unit: the readings usually agree; a verb
            // only one reading exposes is added to it.
            if unit_findings.is_empty() {
                unit_findings = view_findings;
            } else {
                for finding in view_findings {
                    if !unit_findings.contains(&finding) {
                        unit_findings.push(finding);
                    }
                }
            }
        }
        findings.extend(unit_findings);
    }
    if findings.is_empty() && ambiguity.is_none() {
        return None;
    }
    Some(Analysis {
        statements: findings,
        ambiguity,
    })
}

/// Every execution unit of `sql` for `db` (the whole text when the engine is
/// unknown), each with all of its readings.
fn review_views(
    db: Option<sqlly_core::DatabaseType>,
    sql: &str,
) -> Vec<(String, sqlly_lang::seclex::Views)> {
    use sqlly_lang::seclex::{self, LexTarget};
    match db {
        Some(db) => seclex::execution_units(sql, db)
            .into_iter()
            .map(|unit| {
                let views = seclex::views_for(&unit.text, db);
                (unit.text, views)
            })
            .collect(),
        None if sql.trim().is_empty() => Vec::new(),
        None => vec![(sql.to_string(), seclex::views(sql, LexTarget::AnyEngine))],
    }
}

/// Strict classifier for the "anything other than a SELECT" gates (production
/// read-only, notebook write detection), for an unknown engine: `true` if any
/// statement, under any engine's reading, is not a read-only SELECT.
pub fn contains_non_select_statement(sql: &str) -> bool {
    non_select_in(None, sql)
}

/// [`contains_non_select_statement`] for text bound for `db`. An ambiguous
/// batch counts as non-SELECT (fail closed).
pub fn contains_non_select_statement_for(db: sqlly_core::DatabaseType, sql: &str) -> bool {
    non_select_in(Some(db), sql)
}

fn non_select_in(db: Option<sqlly_core::DatabaseType>, sql: &str) -> bool {
    review_views(db, sql).iter().any(|(_, views)| {
        views.ambiguity().is_some()
            || views.views.iter().any(|view| {
                let masked = mask_for_review(&view.shadow);
                let batches = if view.engine == sqlly_lang::seclex::LexEngine::TSql {
                    split_go_batches(&masked)
                } else {
                    vec![masked.as_str()]
                };
                batches.into_iter().any(|batch| {
                    split_statements(batch)
                        .iter()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .any(|s| !is_read_only_select(s))
                })
            })
    })
}

/// Safety features derived from the flat `ConnectionProfile` model. The Swift
/// app had a richer hierarchical policy (server/database/environment/client/
/// project layers each contributing safety flags). The GPUI flat model carries
/// `auto_wrap_rollback`, `block_all_but_select`, and `confirm_data_changes`
/// directly; the remaining flags default to the conservative (safe) choice.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SafetyFeatures {
    pub use_outer_transaction_rollback_scope: bool,
    pub only_allow_select: bool,
    pub is_production: bool,
    /// Show the confirmation dialog for every data-changing batch. Per
    /// connection (`ConnectionProfile::confirm_data_changes`); off by default.
    /// Unbounded UPDATE/DELETE additionally confirm when the connection has
    /// unticked `allow_unbounded_mutations` (it is on — protection off — by
    /// default).
    pub use_confirmation_dialog: bool,
    pub require_ai_review_for_data_changes: bool,
    pub allow_unconstrained_update: bool,
    pub allow_unconstrained_delete: bool,
}

impl SafetyFeatures {
    /// Derive safety features from a flat `ConnectionProfile`.
    pub fn from_profile(profile: &ConnectionProfile) -> Self {
        Self {
            use_outer_transaction_rollback_scope: profile.auto_wrap_rollback,
            only_allow_select: profile.block_all_but_select,
            use_confirmation_dialog: profile.confirm_data_changes,
            allow_unconstrained_update: profile.allow_unbounded_mutations,
            allow_unconstrained_delete: profile.allow_unbounded_mutations,
            ..Default::default()
        }
    }

    /// The ADR-008 effective policy for a profile: its flat flags OR'd with
    /// the safety features of the client, project, and environment entities
    /// the profile is assigned to (matched by name in the connection store,
    /// the same resolution the connection-management dialog displays). The
    /// permissive `allow_unconstrained_*` flags stay profile-only — an
    /// entity must never LOOSEN a connection's safety.
    pub fn effective_for_profile(
        profile: &ConnectionProfile,
        store: &crate::connection::ConnectionStore,
    ) -> Self {
        let mut safety = Self::from_profile(profile);
        let mut fold = |entity: &crate::connection::SafetyFeatures| {
            safety.use_outer_transaction_rollback_scope |=
                entity.use_outer_transaction_rollback_scope;
            safety.only_allow_select |= entity.only_allow_select;
            safety.is_production |= entity.is_production;
            safety.use_confirmation_dialog |= entity.use_confirmation_dialog;
            safety.require_ai_review_for_data_changes |= entity.require_ai_review_for_data_changes;
        };
        if let Some(client) = profile
            .client
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            && let Some(e) = store
                .clients()
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(client))
        {
            fold(&e.safety);
        }
        if let Some(project) = profile
            .project
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            && let Some(e) = store
                .projects()
                .into_iter()
                .find(|e| e.name.eq_ignore_ascii_case(project))
        {
            fold(&e.safety);
        }
        let environments = store.environments();
        if let Some(e) = profile.environment.find_entity(&environments, |e| &e.name) {
            fold(&e.safety);
        }
        safety.is_production = profile.production_override.unwrap_or(safety.is_production);
        safety
    }

    pub fn production_guard(&self) -> sqlly_intel::query::safety::ProductionGuard {
        sqlly_intel::query::safety::ProductionGuard {
            is_production: self.is_production,
        }
    }

    /// Whether the user must confirm before running the given analysis.
    pub fn requires_confirmation(
        &self,
        analysis: &Analysis,
        production: sqlly_intel::query::safety::ProductionDecision,
    ) -> bool {
        if production == sqlly_intel::query::safety::ProductionDecision::WritesUnlocked {
            return true;
        }
        if self.only_allow_select {
            return true;
        }
        if self.use_confirmation_dialog {
            return true;
        }
        if self.require_ai_review_for_data_changes {
            return true;
        }
        if analysis.has_dangerous_unbounded_change() {
            if self.allow_unconstrained_update || self.allow_unconstrained_delete {
                return false;
            }
            return true;
        }
        false
    }
}

/// OR inherited client/project/environment SELECT-only and rollback-wrap flags
/// onto a per-run profile clone. Does not persist changes to the connection
/// store or `active_profile`.
pub fn apply_read_only_unlock(
    profile: &mut ConnectionProfile,
    unlock: Option<&sqlly_intel::query::safety::WriteUnlock>,
    now: sqlly_intel::query::safety::Instant,
) {
    if unlock.is_some_and(|u| u.is_active(now)) {
        profile.block_all_but_select = false;
    }
}

pub fn apply_inherited_execution_policy(
    profile: &mut ConnectionProfile,
    store: &crate::connection::ConnectionStore,
    unlock: Option<&sqlly_intel::query::safety::WriteUnlock>,
    now: sqlly_intel::query::safety::Instant,
) {
    let safety = SafetyFeatures::effective_for_profile(profile, store);
    profile.block_all_but_select |= safety.only_allow_select;
    profile.block_all_but_select |= safety.production_guard().forces_select_only(unlock, now);
    profile.auto_wrap_rollback |= safety.use_outer_transaction_rollback_scope;
}

#[cfg(test)]
mod effective_safety_tests {
    use super::*;
    use sqlly_intel::query::safety::{UnlockWindow, WriteUnlock};

    fn now() -> std::time::Instant {
        std::time::Instant::now()
    }

    fn store_with_client(
        name: &str,
        safety: crate::connection::SafetyFeatures,
    ) -> (tempfile::TempDir, crate::connection::ConnectionStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::connection::ConnectionStore::from_path(dir.path().join("conns.json"));
        store.upsert_client(crate::connection::ClientEntity {
            id: uuid::Uuid::new_v4(),
            name: name.to_string(),
            color_hex: None,
            image_path: None,
            safety,
        });
        (dir, store)
    }

    #[test]
    fn entity_safety_ors_into_the_profile() {
        let (_dir, store) = store_with_client(
            "Acme",
            crate::connection::SafetyFeatures {
                require_ai_review_for_data_changes: true,
                use_confirmation_dialog: true,
                ..Default::default()
            },
        );
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.client = Some("Acme".to_string());
        let effective = SafetyFeatures::effective_for_profile(&profile, &store);
        assert!(effective.require_ai_review_for_data_changes);
        assert!(effective.use_confirmation_dialog);
        // An unassigned profile stays at its flat flags.
        let bare = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        let bare_effective = SafetyFeatures::effective_for_profile(&bare, &store);
        assert!(!bare_effective.require_ai_review_for_data_changes);
    }

    #[test]
    fn entity_match_is_case_insensitive_and_never_loosens() {
        let (_dir, store) = store_with_client(
            "Acme",
            crate::connection::SafetyFeatures {
                require_ai_review_for_data_changes: true,
                ..Default::default()
            },
        );
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.client = Some("acme".to_string());
        profile.allow_unbounded_mutations = true;
        let effective = SafetyFeatures::effective_for_profile(&profile, &store);
        assert!(effective.require_ai_review_for_data_changes);
        // The permissive flags come only from the profile itself.
        assert!(effective.allow_unconstrained_update);
    }

    #[test]
    fn environment_entities_fold_by_built_in_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::connection::ConnectionStore::from_path(dir.path().join("conns.json"));
        // The built-in Production environment exists; flip its AI-review flag.
        let mut production = store
            .environments()
            .into_iter()
            .find(|e| e.name == "Production")
            .expect("built-in Production environment");
        production.safety.require_ai_review_for_data_changes = true;
        store.upsert_environment(production);
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.environment = sqlly_core::connection::ConnectionEnvironment::Production;
        let effective = SafetyFeatures::effective_for_profile(&profile, &store);
        assert!(effective.require_ai_review_for_data_changes);
    }

    #[test]
    fn inherited_select_only_sets_profile_block_flag() {
        let (_dir, store) = store_with_client(
            "Acme",
            crate::connection::SafetyFeatures {
                only_allow_select: true,
                ..Default::default()
            },
        );
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.client = Some("Acme".to_string());
        assert!(!profile.block_all_but_select);
        apply_inherited_execution_policy(&mut profile, &store, None, now());
        assert!(profile.block_all_but_select);
        assert!(
            store
                .clients()
                .into_iter()
                .find(|e| e.name == "Acme")
                .expect("client")
                .safety
                .only_allow_select
        );
    }

    #[test]
    fn inherited_rollback_wrap_sets_profile_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::connection::ConnectionStore::from_path(dir.path().join("conns.json"));
        let mut production = store
            .environments()
            .into_iter()
            .find(|e| e.name == "Production")
            .expect("built-in Production environment");
        production.safety.use_outer_transaction_rollback_scope = true;
        store.upsert_environment(production);
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.environment = sqlly_core::connection::ConnectionEnvironment::Production;
        assert!(!profile.auto_wrap_rollback);
        apply_inherited_execution_policy(&mut profile, &store, None, now());
        assert!(profile.auto_wrap_rollback);
    }

    #[test]
    fn unassigned_profile_stays_unchanged_by_inherited_policy() {
        let (_dir, store) = store_with_client(
            "Acme",
            crate::connection::SafetyFeatures {
                only_allow_select: true,
                ..Default::default()
            },
        );
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        apply_inherited_execution_policy(&mut profile, &store, None, now());
        assert!(!profile.block_all_but_select);
        assert!(!profile.auto_wrap_rollback);
    }

    #[test]
    fn inherited_select_only_or_never_clears_existing_block() {
        let (_dir, store) = store_with_client(
            "Acme",
            crate::connection::SafetyFeatures {
                only_allow_select: false,
                ..Default::default()
            },
        );
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.client = Some("Acme".to_string());
        profile.block_all_but_select = true;
        apply_inherited_execution_policy(&mut profile, &store, None, now());
        assert!(profile.block_all_but_select);
    }

    #[test]
    fn inherited_select_only_refuses_delete_at_driver() {
        let (_dir, store) = store_with_client(
            "Acme",
            crate::connection::SafetyFeatures {
                only_allow_select: true,
                ..Default::default()
            },
        );
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.client = Some("Acme".to_string());
        apply_inherited_execution_policy(&mut profile, &store, None, now());
        let err =
            sqlly_intel::query::safety::apply_connection_policy_for(&profile, "DELETE FROM t")
                .expect_err("DELETE should be refused");
        assert!(matches!(err, sqlly_core::StudioError::PermissionDenied(_)));
    }

    #[test]
    fn production_environment_forces_select_only_when_locked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::connection::ConnectionStore::from_path(dir.path().join("conns.json"));
        let mut production = store
            .environments()
            .into_iter()
            .find(|e| e.name == "Production")
            .expect("built-in Production environment");
        production.safety.is_production = true;
        store.upsert_environment(production);
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.environment = sqlly_core::connection::ConnectionEnvironment::Production;
        apply_inherited_execution_policy(&mut profile, &store, None, now());
        assert!(profile.block_all_but_select);
    }

    #[test]
    fn read_only_unlock_lifts_profile_flag_but_not_inherited() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::connection::ConnectionStore::from_path(dir.path().join("conns.json"));
        store.upsert_client(crate::connection::ClientEntity {
            id: uuid::Uuid::new_v4(),
            name: "Acme".to_string(),
            color_hex: None,
            image_path: None,
            safety: crate::connection::SafetyFeatures {
                only_allow_select: true,
                ..Default::default()
            },
        });
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.client = Some("Acme".to_string());
        profile.block_all_but_select = true;
        let unlock = WriteUnlock::start(UnlockWindow::FiveMinutes, now());
        apply_read_only_unlock(&mut profile, Some(&unlock), now());
        apply_inherited_execution_policy(&mut profile, &store, None, now());
        assert!(profile.block_all_but_select);

        let mut profile_only = ConnectionProfile::sql_login(
            "p2",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile_only.block_all_but_select = true;
        apply_read_only_unlock(&mut profile_only, Some(&unlock), now());
        apply_inherited_execution_policy(&mut profile_only, &store, None, now());
        assert!(!profile_only.block_all_but_select);

        let expired = WriteUnlock::start(
            UnlockWindow::OneMinute,
            now() - std::time::Duration::from_secs(120),
        );
        let mut expired_profile = ConnectionProfile::sql_login(
            "p3",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        expired_profile.block_all_but_select = true;
        apply_read_only_unlock(&mut expired_profile, Some(&expired), now());
        assert!(expired_profile.block_all_but_select);
    }

    #[test]
    fn production_unlock_does_not_force_select_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::connection::ConnectionStore::from_path(dir.path().join("conns.json"));
        let mut production = store
            .environments()
            .into_iter()
            .find(|e| e.name == "Production")
            .expect("built-in Production environment");
        production.safety.is_production = true;
        store.upsert_environment(production);
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.environment = sqlly_core::connection::ConnectionEnvironment::Production;
        let unlock = WriteUnlock::start(UnlockWindow::FiveMinutes, now());
        apply_inherited_execution_policy(&mut profile, &store, Some(&unlock), now());
        assert!(!profile.block_all_but_select);
    }

    #[test]
    fn production_unlock_never_clears_explicit_select_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::connection::ConnectionStore::from_path(dir.path().join("conns.json"));
        let mut production = store
            .environments()
            .into_iter()
            .find(|e| e.name == "Production")
            .expect("built-in Production environment");
        production.safety.is_production = true;
        production.safety.only_allow_select = true;
        store.upsert_environment(production);
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.environment = sqlly_core::connection::ConnectionEnvironment::Production;
        let unlock = WriteUnlock::start(UnlockWindow::FiveMinutes, now());
        apply_inherited_execution_policy(&mut profile, &store, Some(&unlock), now());
        assert!(profile.block_all_but_select);
    }

    #[test]
    fn profile_override_false_opts_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = crate::connection::ConnectionStore::from_path(dir.path().join("conns.json"));
        let mut production = store
            .environments()
            .into_iter()
            .find(|e| e.name == "Production")
            .expect("built-in Production environment");
        production.safety.is_production = true;
        store.upsert_environment(production);
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.environment = sqlly_core::connection::ConnectionEnvironment::Production;
        profile.production_override = Some(false);
        apply_inherited_execution_policy(&mut profile, &store, None, now());
        assert!(!profile.block_all_but_select);
    }

    #[test]
    fn profile_override_true_on_dev_env_is_production() {
        let (_dir, store) = store_with_client("Acme", crate::connection::SafetyFeatures::default());
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "server",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        profile.environment = sqlly_core::connection::ConnectionEnvironment::Dev;
        profile.production_override = Some(true);
        apply_inherited_execution_policy(&mut profile, &store, None, now());
        assert!(profile.block_all_but_select);
    }
}

// ---------------------------------------------------------------------------
// SQL scanning internals
// ---------------------------------------------------------------------------

/// Prepare a security-lexer shadow for word scanning. The shadow already
/// has every comment blanked and every literal reduced to `'   '`; quoted
/// identifiers arrive as `[name]`, whose *name* must not read as a word here
/// (a column named `[where]` is not a WHERE clause), so the name becomes a
/// neutral placeholder of the same length.
fn mask_for_review(shadow: &str) -> String {
    let mut out = String::with_capacity(shadow.len());
    let mut in_bracket = false;
    for c in shadow.chars() {
        match c {
            '[' if !in_bracket => {
                in_bracket = true;
                out.push('[');
            }
            ']' if in_bracket => {
                in_bracket = false;
                out.push(']');
            }
            _ if in_bracket => {
                for _ in 0..c.len_utf8() {
                    out.push('x');
                }
            }
            '\'' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Byte span of the line containing `cursor`, for command engines (Redis)
/// where every line is one statement. Excludes the line terminator; a
/// cursor at the very end of the buffer belongs to the last line.
pub fn command_line_span_at(text: &str, cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(text.len());
    let start = text[..cursor].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[cursor..]
        .find('\n')
        .map(|i| cursor + i)
        .unwrap_or(text.len());
    let end = if end > start && text.as_bytes()[end - 1] == b'\r' {
        end - 1
    } else {
        end
    };
    (start, end.max(start))
}

/// Byte spans of every command line in `text` (blank and comment lines
/// skipped), for the editor's statement navigation on command engines.
pub fn command_line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches('\n').trim_end_matches('\r');
        if !sqlly_intel::query::safety::redis::is_comment_or_blank(content) {
            spans.push((offset, offset + content.len()));
        }
        offset += line.len();
    }
    spans
}

/// The mutation analysis of a Redis script, in the same shape the SQL
/// analysis produces so the confirmation dialog and its policy apply
/// unchanged: plain writes count as UPDATE-like changes with a WHERE
/// (bounded), key deletions as DELETE with a WHERE, and the wholesale
/// commands (FLUSHDB, FLUSHALL, SWAPDB) as an unbounded DELETE. `None`
/// when nothing in the script writes.
pub fn analyze_redis(text: &str) -> Option<Analysis> {
    use sqlly_intel::query::safety::redis::{CommandAccess, command_access, parse_script};
    let commands = parse_script(text).ok()?;
    let mut statements = Vec::new();
    for command in &commands {
        let name = command.effective_name();
        match command_access(command) {
            CommandAccess::ReadOnly => {}
            CommandAccess::Write => statements.push(StatementFinding {
                change_type: ChangeType::Update,
                has_where_clause: true,
            }),
            CommandAccess::Destructive => {
                let wholesale = matches!(name.as_str(), "FLUSHDB" | "FLUSHALL" | "SWAPDB");
                let deletes = wholesale
                    || matches!(
                        name.as_str(),
                        "DEL"
                            | "UNLINK"
                            | "JSON.DEL"
                            | "JSON.FORGET"
                            | "TS.DEL"
                            | "FT.DROPINDEX"
                            | "FT.DROP"
                            | "GRAPH.DELETE"
                    );
                statements.push(StatementFinding {
                    change_type: if deletes {
                        ChangeType::Delete
                    } else {
                        ChangeType::Update
                    },
                    has_where_clause: !wholesale,
                })
            }
        }
    }
    if statements.is_empty() {
        None
    } else {
        Some(Analysis {
            statements,
            ambiguity: None,
        })
    }
}

/// Validate a Redis script for the editor: unparsable lines and commands
/// that would block the connection forever, as diagnostics on their lines.
pub fn redis_command_diagnostics(text: &str) -> Vec<crate::editor::ValidationDiagnostic> {
    use crate::editor::{DiagnosticSeverity, DiagnosticSource, ValidationDiagnostic};
    use sqlly_intel::query::safety::redis::{RedisParseError, blocking_reason, parse_script};
    let line_span = |line: usize| {
        let mut offset = 0;
        for (index, raw) in text.split_inclusive('\n').enumerate() {
            if index + 1 == line {
                let content = raw.trim_end_matches('\n').trim_end_matches('\r');
                return (offset, offset + content.len());
            }
            offset += raw.len();
        }
        (text.len(), text.len())
    };
    let diagnostic = |line: usize, severity: DiagnosticSeverity, message: String| {
        let (start, end) = line_span(line);
        ValidationDiagnostic {
            severity,
            source: DiagnosticSource::Validation,
            message,
            line: line.saturating_sub(1),
            identifier: None,
            start_byte: start,
            end_byte: end,
            explainer: None,
            fix: None,
        }
    };
    match parse_script(text) {
        Ok(commands) => commands
            .iter()
            .filter_map(|command| {
                blocking_reason(command)
                    .map(|reason| diagnostic(command.line, DiagnosticSeverity::Error, reason))
            })
            .collect(),
        Err(RedisParseError::Empty) => Vec::new(),
        Err(error) => {
            let line = match &error {
                RedisParseError::UnterminatedQuote { line }
                | RedisParseError::TextAfterQuote { line }
                | RedisParseError::InvalidEscape { line }
                | RedisParseError::InvalidJson { line, .. } => *line,
                RedisParseError::Empty => 1,
            };
            vec![diagnostic(
                line,
                DiagnosticSeverity::Error,
                error.to_string(),
            )]
        }
    }
}

/// Dialect-aware statement span: `sqlly_lang::sqllex::split_statements(sql, dialect)`
/// picks the slice containing `cursor` (or the last slice when `cursor == sql.len()`),
/// then the span is widened to include the terminator and trailing whitespace up to
/// the next slice's start so the contract `(start, end)` matches the legacy scanner
/// (spans tile the buffer; `end` of one == `start` of the next). Falls back to the
/// legacy scanner when the splitter returns no slices (empty/blank input) or for
/// `Dialect::Oracle`/other dialects the editor never sets.
pub fn statement_span_at_for(
    sql: &str,
    cursor: usize,
    dialect: sqlly_lang::sqlmodel::Dialect,
) -> (usize, usize) {
    if dialect == sqlly_lang::sqlmodel::Dialect::Oracle {
        return legacy_statement_span_at(sql, cursor);
    }
    let tiled = tiled_statement_spans(sql, dialect);
    if tiled.is_empty() {
        return legacy_statement_span_at(sql, cursor);
    }
    span_in_tiles(&tiled, sql.len(), cursor)
}

/// The tile of `tiled` (non-empty, from [`tiled_statement_spans`]) that
/// [`statement_span_at_for`] picks for `cursor`. Split out so a caller
/// walking every statement tiles the buffer once instead of once per
/// statement.
pub(crate) fn span_in_tiles(tiled: &[(usize, usize)], len: usize, cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(len);
    for &(start, end) in tiled {
        if cursor >= start && (cursor < end || (cursor == end && end == len)) {
            return (start, end);
        }
    }
    *tiled.last().unwrap_or(&(0, len))
}

#[cfg(test)]
thread_local! {
    /// Whole-buffer statement splits on this thread, so tests can pin how
    /// many a caller performs (a per-statement split made typing quadratic).
    pub(crate) static WHOLE_BUFFER_SPLITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Legacy entry point: T-SQL rules. Existing callers keep working; new callers pass the dialect.
#[allow(dead_code)]
pub fn statement_span_at(sql: &str, cursor: usize) -> (usize, usize) {
    statement_span_at_for(sql, cursor, sqlly_lang::sqlmodel::Dialect::SqlServer)
}

/// All statement spans that tile `sql` for `dialect` (each ends where the next begins).
pub(crate) fn tiled_statement_spans(
    sql: &str,
    dialect: sqlly_lang::sqlmodel::Dialect,
) -> Vec<(usize, usize)> {
    #[cfg(test)]
    WHOLE_BUFFER_SPLITS.with(|n| n.set(n.get() + 1));
    if dialect == sqlly_lang::sqlmodel::Dialect::Oracle {
        return Vec::new();
    }
    let slices = dialect_statement_slices(sql, dialect);
    if slices.is_empty() {
        return Vec::new();
    }
    tile_slices(sql, dialect, &slices)
}

fn dialect_statement_slices(
    sql: &str,
    dialect: sqlly_lang::sqlmodel::Dialect,
) -> Vec<sqlly_lang::sqllex::splitter::StatementSlice<'_>> {
    match dialect {
        sqlly_lang::sqlmodel::Dialect::SqlServer => {
            let mut out = Vec::new();
            for batch in sqlly_lang::sqllex::split_batches(sql, dialect) {
                let base = batch.span.start;
                for slice in sqlly_lang::sqllex::split_statements(batch.text, dialect) {
                    out.push(sqlly_lang::sqllex::splitter::StatementSlice {
                        text: slice.text,
                        span: sqlly_lang::sqlmodel::Span::new(
                            slice.span.start + base,
                            slice.span.end + base,
                        ),
                    });
                }
            }
            out
        }
        _ => sqlly_lang::sqllex::split_statements(sql, dialect),
    }
}

fn tile_slices(
    sql: &str,
    dialect: sqlly_lang::sqlmodel::Dialect,
    slices: &[sqlly_lang::sqllex::splitter::StatementSlice<'_>],
) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(slices.len());
    for (i, slice) in slices.iter().enumerate() {
        let start = if i == 0 { 0 } else { slice.span.start };
        let end = if i + 1 < slices.len() {
            let between_start = slice.span.end;
            let next_start = slices[i + 1].span.start;
            if dialect == sqlly_lang::sqlmodel::Dialect::SqlServer
                && let Some(go_off) =
                    go_separator_line_start_in_gap(&sql[between_start..next_start])
            {
                between_start + go_off
            } else {
                next_start
            }
        } else {
            sql.len()
        };
        out.push((start, end));
    }
    out
}

fn go_separator_line_start_in_gap(gap: &str) -> Option<usize> {
    let mut line_start = 0usize;
    for line in gap.split_inclusive('\n') {
        let line_body = line.trim_end_matches('\n');
        if is_go_separator_line(line_body) {
            return Some(line_start);
        }
        line_start += line.len();
    }
    None
}

/// Byte span `(start, end)` of the statement containing `cursor`, delimited
/// by top-level semicolons (outside parentheses) and `GO` separator lines,
/// with strings, quoted/bracketed identifiers, and comments respected. Falls
/// back to the whole buffer when no delimiter bounds the cursor — running
/// everything is the honest degradation for undelimited scripts.
fn legacy_statement_span_at(sql: &str, cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(sql.len());
    for (a, b) in legacy_statement_tiles(sql) {
        if cursor >= a && (cursor < b || (cursor == b && b == sql.len())) {
            return (a, b);
        }
    }
    (0, sql.len())
}

/// Every span the legacy scanner can return for `sql`, in order (they tile
/// the buffer). [`legacy_statement_span_at`] picks one; a caller walking all
/// statements takes the whole list instead of rescanning per statement.
pub(crate) fn legacy_statement_tiles(sql: &str) -> Vec<(usize, usize)> {
    #[cfg(test)]
    WHOLE_BUFFER_SPLITS.with(|n| n.set(n.get() + 1));
    let bytes = sql.as_bytes();
    let mut boundaries: Vec<usize> = vec![0];
    let mut i = 0usize;
    let mut in_line_comment = false;
    let mut block_depth = 0u32;
    let mut paren_depth = 0i32;
    let (mut in_single, mut in_double, mut in_bracket) = (false, false, false);
    let mut line_start = 0usize;
    let mut line_clean = true;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            if line_clean
                && !in_single
                && !in_double
                && !in_bracket
                && block_depth == 0
                && is_go_separator_line(&sql[line_start..i])
            {
                boundaries.push(line_start);
                boundaries.push(i + 1);
            }
            in_line_comment = false;
            i += 1;
            line_start = i;
            line_clean = block_depth == 0 && !in_single && !in_double && !in_bracket;
            continue;
        }
        if in_line_comment {
            i += 1;
            continue;
        }
        if block_depth > 0 {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                block_depth -= 1;
                i += 2;
            } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
                block_depth += 1;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_single {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        if in_bracket {
            if b == b']' {
                in_bracket = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                in_line_comment = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                block_depth = 1;
                i += 2;
            }
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'[' => {
                in_bracket = true;
                i += 1;
            }
            b'(' => {
                paren_depth += 1;
                i += 1;
            }
            b')' => {
                paren_depth = (paren_depth - 1).max(0);
                i += 1;
            }
            b';' if paren_depth == 0 => {
                boundaries.push(i + 1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    if line_clean
        && !in_single
        && !in_double
        && !in_bracket
        && block_depth == 0
        && is_go_separator_line(&sql[line_start..])
    {
        boundaries.push(line_start);
    }
    boundaries.push(sql.len());
    boundaries.sort_unstable();
    boundaries.dedup();
    boundaries
        .windows(2)
        .map(|pair| (pair[0], pair[1]))
        .collect()
}

/// Split a (masked) SQL batch on top-level semicolons. Semicolons inside
/// parentheses are not separators.
fn split_statements(sql: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut paren_depth = 0i32;
    for c in sql.chars() {
        match c {
            '(' => {
                paren_depth += 1;
                current.push(c);
            }
            ')' => {
                paren_depth -= 1;
                if paren_depth < 0 {
                    paren_depth = 0;
                }
                current.push(c);
            }
            ';' if paren_depth == 0 => {
                result.push(std::mem::take(&mut current));
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        result.push(current);
    }
    result
}

/// Split a (masked) buffer into `GO` batches. A separator is a line whose
/// content is `GO` alone — allowing the leftover `--` / `/*` shells of a
/// masked trailing comment, but nothing else (`GO 5` repeat counts and lines
/// merely containing "go" are not separators).
fn split_go_batches(masked: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut pos = 0usize;
    for line in masked.split_inclusive('\n') {
        if is_go_separator_line(line) {
            out.push(&masked[start..pos]);
            start = pos + line.len();
        }
        pos += line.len();
    }
    out.push(&masked[start..]);
    out
}

fn is_go_separator_line(line: &str) -> bool {
    let t = line.trim();
    if t.len() < 2 || !t.is_char_boundary(2) || !t[..2].eq_ignore_ascii_case("go") {
        return false;
    }
    t[2..]
        .chars()
        .all(|c| matches!(c, ' ' | '\t' | '-' | '/' | '*'))
}

/// Whether a (masked) batch deploys a routine definition: `CREATE [OR ALTER |
/// OR REPLACE] PROC|PROCEDURE|FUNCTION|TRIGGER|PACKAGE …` or the plain ALTER
/// forms. T-SQL requires these to be the only statement in their batch, so a
/// batch that starts as one *is* one — everything after it is the body.
fn is_definition_batch(batch: &str) -> bool {
    let mut words = batch
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|s| !s.is_empty())
        .take(4)
        .map(|s| s.to_ascii_lowercase());
    fn is_kind(w: &str) -> bool {
        matches!(w, "proc" | "procedure" | "function" | "trigger" | "package")
    }
    match words.next().as_deref() {
        Some("alter") => words.next().as_deref().is_some_and(is_kind),
        Some("create") => match words.next().as_deref() {
            // CREATE OR ALTER / CREATE OR REPLACE
            Some("or") => {
                words.next(); // alter | replace
                words.next().as_deref().is_some_and(is_kind)
            }
            Some(kind) => is_kind(kind),
            None => false,
        },
        _ => false,
    }
}

/// Tokenize a statement into lowercase keywords/identifiers. Whitespace and
/// punctuation are separators.
fn tokenize(statement: &str) -> Vec<String> {
    statement
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// A word token with the parenthesis depth it appears at, so the scanner can
/// tell an outer WHERE from one inside a subquery and a statement verb from
/// one inside `TOP (…)` / a function call.
struct ScanToken {
    text: String,
    depth: i32,
}

/// Tokenize into lowercase words tagged with paren depth. `#` and `@` are
/// word characters so temp tables (`#staging`) and table variables (`@rows`)
/// keep their session-scope marker.
fn scan_tokens(statement: &str) -> Vec<ScanToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    for c in statement.chars() {
        if c.is_alphanumeric() || matches!(c, '_' | '#' | '@') {
            current.push(c.to_ascii_lowercase());
        } else {
            if !current.is_empty() {
                tokens.push(ScanToken {
                    text: std::mem::take(&mut current),
                    depth,
                });
            }
            match c {
                '(' => depth += 1,
                ')' => depth = (depth - 1).max(0),
                _ => {}
            }
        }
    }
    if !current.is_empty() {
        tokens.push(ScanToken {
            text: current,
            depth,
        });
    }
    tokens
}

#[allow(dead_code)]
/// A statement is a read-only SELECT if its first significant token is
/// `select`, or it is a CTE (`with`) whose body is a SELECT and never
/// introduces a data-changing verb.
fn is_read_only_select(statement: &str) -> bool {
    let tokens = tokenize(statement);
    if tokens.is_empty() {
        return true;
    }
    match tokens[0].as_str() {
        "select" => true,
        "with" => {
            let data_changing = ["insert", "update", "delete", "merge", "truncate"];
            if tokens.iter().any(|t| data_changing.contains(&t.as_str())) {
                return false;
            }
            tokens.iter().any(|t| t == "select")
        }
        _ => false,
    }
}

/// Words that, immediately before a data-changing verb, mark it as part of a
/// larger construct rather than a statement of its own: FK actions
/// (`ON DELETE/UPDATE CASCADE`), row locking (`SELECT … FOR UPDATE`), MERGE
/// clauses (`WHEN … THEN UPDATE/DELETE/INSERT`, constrained by the merge's
/// ON), upserts (`ON DUPLICATE KEY UPDATE`, `ON CONFLICT … DO UPDATE`), and
/// trigger event lists (`AFTER/BEFORE/INSTEAD OF DELETE`).
fn verb_is_clause_context(tokens: &[ScanToken], i: usize) -> bool {
    match i.checked_sub(1).map(|p| tokens[p].text.as_str()) {
        // Reserved words on every engine: none can be a column alias that
        // ends a previous statement written without a separator.
        Some("on" | "then" | "for" | "key" | "of") => true,
        // Trigger event lists. BEFORE/AFTER are not reserved in T-SQL, so
        // `SELECT 1 AS after DELETE FROM t` is two statements there; only a
        // CREATE/ALTER (TRIGGER) statement makes them a clause.
        Some("before" | "after") => tokens
            .first()
            .is_some_and(|t| matches!(t.text.as_str(), "create" | "alter")),
        // PostgreSQL / SQLite upsert: `ON CONFLICT … DO UPDATE`.
        Some("do") => tokens[..i].iter().any(|t| t.text == "conflict"),
        _ => false,
    }
}

/// The verb's object: the first following word at the verb's own paren depth
/// that is not a syntactic filler (`FROM`, `INTO`, `TOP`, …). Contents of
/// `TOP (…)` sit deeper and are passed over.
fn target_after<'a>(tokens: &'a [ScanToken], i: usize, skip: &[&str]) -> Option<&'a str> {
    let depth = tokens[i].depth;
    tokens[i + 1..]
        .iter()
        .filter(|t| t.depth == depth)
        .map(|t| t.text.as_str())
        .find(|t| !skip.contains(t))
}

/// Whether the verb at `i` has a WHERE at its own depth before the next
/// data-changing verb. Bounding the scan keeps a following statement's WHERE
/// (`BEGIN TRAN UPDATE t SET x = 1 DELETE FROM u WHERE …`) from vouching for
/// this one, and requiring the verb's depth keeps a subquery's WHERE
/// (`UPDATE t SET x = (SELECT … WHERE …)`) from doing the same.
fn has_where_before_next_verb(tokens: &[ScanToken], i: usize) -> bool {
    let depth = tokens[i].depth;
    for t in &tokens[i + 1..] {
        if t.depth < depth {
            break;
        }
        if t.depth == depth {
            match t.text.as_str() {
                "where" => return true,
                "update" | "insert" | "delete" | "merge" | "truncate" => return false,
                // A statement that follows without a separator (T-SQL) must
                // not lend its WHERE: `DELETE FROM t SELECT * FROM u WHERE
                // id = 1` deletes every row of t. None of these words can
                // appear at the verb's own depth before its WHERE.
                "select" | "exec" | "execute" | "declare" | "print" | "begin" | "return"
                | "while" | "create" | "alter" | "drop" | "grant" | "revoke" | "deny" | "use"
                | "waitfor" | "commit" | "rollback" | "call" | "raiserror" | "throw" => {
                    return false;
                }
                _ => {}
            }
        }
    }
    false
}

/// Find data-changing verbs in a statement and whether each has a WHERE clause.
fn statement_findings(statement: &str) -> Vec<StatementFinding> {
    let tokens = scan_tokens(statement);
    let mut findings = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        let change_type = match token.text.as_str() {
            "update" => ChangeType::Update,
            "insert" => ChangeType::Insert,
            "delete" => ChangeType::Delete,
            "truncate" => ChangeType::Truncate,
            "merge" => ChangeType::Merge,
            _ => continue,
        };
        if verb_is_clause_context(&tokens, i) {
            continue;
        }
        // `UPDATE(col)` is the trigger-body column-test function, not a
        // statement: the verb is followed directly by a paren.
        if change_type == ChangeType::Update
            && tokens.get(i + 1).is_some_and(|t| t.depth > token.depth)
        {
            continue;
        }
        let skip: &[&str] = match change_type {
            ChangeType::Update => &["top"],
            ChangeType::Delete => &["top", "from", "only"],
            ChangeType::Insert => &["into", "top", "ignore"],
            ChangeType::Truncate => &["table", "only"],
            ChangeType::Merge => &["into", "top"],
        };
        let target = target_after(&tokens, i, skip);
        // UPDATE STATISTICS maintains an index, not data.
        if change_type == ChangeType::Update && target == Some("statistics") {
            continue;
        }
        // `#temp` tables and `@table` variables are session-local scratch
        // space, not real data.
        if target.is_some_and(|t| t.starts_with('#') || t.starts_with('@')) {
            continue;
        }
        findings.push(StatementFinding {
            change_type,
            has_where_clause: has_where_before_next_verb(&tokens, i),
        });
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{DatabaseType, SecretProvider, SecretRef};

    fn profile() -> ConnectionProfile {
        let secret = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".to_string(),
        };
        ConnectionProfile::sql_login("test", "host", "sa", secret)
    }

    #[test]
    fn analyze_select_returns_none() {
        assert!(analyze("SELECT 1").is_none());
    }

    #[test]
    fn analyze_update_with_where() {
        let a = analyze("UPDATE t SET x = 1 WHERE id = 5").unwrap();
        assert_eq!(a.statements.len(), 1);
        assert_eq!(a.statements[0].change_type, ChangeType::Update);
        assert!(a.statements[0].has_where_clause);
        assert!(!a.has_dangerous_unbounded_change());
    }

    #[test]
    fn analyze_update_without_where_is_dangerous() {
        let a = analyze("UPDATE t SET x = 1").unwrap();
        assert!(a.statements[0].is_unbounded_dangerous());
        assert!(a.has_dangerous_unbounded_change());
    }

    #[test]
    fn analyze_delete_without_where_is_dangerous() {
        let a = analyze("DELETE FROM t").unwrap();
        assert_eq!(a.statements[0].change_type, ChangeType::Delete);
        assert!(a.has_dangerous_unbounded_change());
    }

    #[test]
    fn analyze_delete_with_where_is_safe() {
        let a = analyze("DELETE FROM t WHERE id = 1").unwrap();
        assert!(!a.has_dangerous_unbounded_change());
    }

    #[test]
    fn analyze_insert() {
        let a = analyze("INSERT INTO t (x) VALUES (1)").unwrap();
        assert_eq!(a.statements[0].change_type, ChangeType::Insert);
        assert!(!a.has_dangerous_unbounded_change());
    }

    #[test]
    fn analyze_truncate() {
        let a = analyze("TRUNCATE TABLE t").unwrap();
        assert_eq!(a.statements[0].change_type, ChangeType::Truncate);
    }

    #[test]
    fn analyze_merge() {
        let a = analyze("MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET x = 1")
            .unwrap();
        assert_eq!(a.statements[0].change_type, ChangeType::Merge);
    }

    #[test]
    fn analyze_batch_multiple_statements() {
        let a = analyze("UPDATE t SET x = 1; DELETE FROM u WHERE id = 2").unwrap();
        assert_eq!(a.statements.len(), 2);
    }

    #[test]
    fn analyze_ignores_update_statistics() {
        assert!(analyze("UPDATE STATISTICS t").is_none());
    }

    #[test]
    fn analyze_ignores_keywords_in_comments() {
        assert!(analyze("-- DELETE everything\nSELECT 1").is_none());
    }

    #[test]
    fn analyze_ignores_keywords_in_strings() {
        assert!(analyze("SELECT 'DELETE FROM users'").is_none());
    }

    #[test]
    fn analyze_ignores_keywords_in_brackets() {
        // Brackets quote on SQL Server and SQLite. For an unknown engine the
        // word stays visible (on PostgreSQL or MySQL `[` is not a quote).
        assert!(analyze_for(DatabaseType::SqlServer, "SELECT [UPDATE] FROM t").is_none());
        assert!(analyze_for(DatabaseType::Sqlite, "SELECT [UPDATE] FROM t").is_none());
        assert!(analyze("SELECT [UPDATE] FROM t").is_some());
    }

    #[test]
    fn analyze_ignores_semicolons_in_subqueries() {
        let a =
            analyze("UPDATE t SET x = (SELECT COUNT(*) FROM u WHERE x = 1) WHERE id = 5").unwrap();
        assert_eq!(a.statements.len(), 1);
        assert!(a.statements[0].has_where_clause);
    }

    #[test]
    fn change_types_dedupes_and_preserves_order() {
        let a = analyze("UPDATE t SET x = 1; UPDATE u SET y = 2; DELETE FROM v").unwrap();
        let ct = a.change_types();
        assert_eq!(ct, vec![ChangeType::Update, ChangeType::Delete]);
    }

    #[test]
    fn bottom_line_single() {
        let a = analyze("UPDATE t SET x = 1").unwrap();
        assert_eq!(a.bottom_line(), "WE WILL MAKE UPDATE CHANGES.");
    }

    #[test]
    fn bottom_line_two() {
        let a = analyze("UPDATE t SET x = 1; DELETE FROM u").unwrap();
        assert_eq!(a.bottom_line(), "WE WILL MAKE UPDATE, AND DELETE CHANGES.");
    }

    #[test]
    fn bottom_line_three() {
        let a = analyze("UPDATE t SET x = 1; DELETE FROM u; TRUNCATE TABLE v").unwrap();
        assert_eq!(
            a.bottom_line(),
            "WE WILL MAKE UPDATE, DELETE, AND TRUNCATE CHANGES."
        );
    }

    #[test]
    fn rule_based_summary_includes_danger_warning() {
        let a = analyze("UPDATE t SET x = 1").unwrap();
        let s = a.rule_based_summary();
        assert!(s.contains("UPDATE"));
        assert!(s.contains("no WHERE clause"));
    }

    #[test]
    fn contains_non_select_select_only() {
        assert!(!contains_non_select_statement("SELECT 1"));
    }

    #[test]
    fn contains_non_select_update() {
        assert!(contains_non_select_statement("UPDATE t SET x = 1"));
    }

    #[test]
    fn contains_non_select_cte_select() {
        assert!(!contains_non_select_statement(
            "WITH c AS (SELECT 1) SELECT * FROM c"
        ));
    }

    #[test]
    fn contains_non_select_cte_delete() {
        assert!(contains_non_select_statement(
            "WITH c AS (SELECT 1) DELETE FROM t"
        ));
    }

    #[test]
    fn contains_non_select_empty() {
        assert!(!contains_non_select_statement(""));
    }

    #[test]
    fn safety_features_default() {
        let s = SafetyFeatures::default();
        assert!(!s.only_allow_select);
        assert!(!s.use_confirmation_dialog);
    }

    #[test]
    fn safety_features_from_profile() {
        let mut p = profile();
        p.auto_wrap_rollback = true;
        p.block_all_but_select = true;
        p.confirm_data_changes = true;
        let s = SafetyFeatures::from_profile(&p);
        assert!(s.use_outer_transaction_rollback_scope);
        assert!(s.only_allow_select);
        assert!(s.use_confirmation_dialog);
    }

    #[test]
    fn unconfigured_profile_does_not_confirm_bounded_changes() {
        let s = SafetyFeatures::from_profile(&profile());
        let a = analyze("INSERT INTO t (x) VALUES (1)").unwrap();
        assert!(!s.requires_confirmation(
            &a,
            sqlly_intel::query::safety::ProductionDecision::NotProduction
        ));
        let a = analyze(
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN UPDATE SET x = 1 WHERE t.id = 1",
        )
        .unwrap();
        assert!(!s.requires_confirmation(
            &a,
            sqlly_intel::query::safety::ProductionDecision::NotProduction
        ));
    }

    #[test]
    fn configured_profile_confirms_bounded_changes() {
        let mut p = profile();
        p.confirm_data_changes = true;
        let s = SafetyFeatures::from_profile(&p);
        let a = analyze("INSERT INTO t (x) VALUES (1)").unwrap();
        assert!(s.requires_confirmation(
            &a,
            sqlly_intel::query::safety::ProductionDecision::NotProduction
        ));
    }

    #[test]
    fn safety_requires_confirmation_for_dangerous() {
        // With the unbounded-statement protection opted in (allow off),
        // unbounded UPDATE/DELETE confirm even when the connection has the
        // confirmation dialog off.
        let mut p = profile();
        p.allow_unbounded_mutations = false;
        let s = SafetyFeatures::from_profile(&p);
        let a = analyze("UPDATE t SET x = 1").unwrap();
        assert!(s.requires_confirmation(
            &a,
            sqlly_intel::query::safety::ProductionDecision::NotProduction
        ));
    }

    #[test]
    fn allow_unbounded_mutations_skips_confirmation_for_unbounded() {
        let mut p = profile();
        p.allow_unbounded_mutations = true;
        let s = SafetyFeatures::from_profile(&p);
        assert!(s.allow_unconstrained_update);
        assert!(s.allow_unconstrained_delete);

        let a = analyze("UPDATE t SET x = 1").unwrap();
        assert!(!s.requires_confirmation(
            &a,
            sqlly_intel::query::safety::ProductionDecision::NotProduction
        ));
        let a = analyze("DELETE FROM t").unwrap();
        assert!(!s.requires_confirmation(
            &a,
            sqlly_intel::query::safety::ProductionDecision::NotProduction
        ));
    }

    #[test]
    fn allow_unbounded_mutations_does_not_override_confirm_data_changes() {
        // The two settings are independent: with the per-connection
        // confirmation dialog on, every data change still confirms.
        let mut p = profile();
        p.allow_unbounded_mutations = true;
        p.confirm_data_changes = true;
        let s = SafetyFeatures::from_profile(&p);
        let a = analyze("UPDATE t SET x = 1").unwrap();
        assert!(s.requires_confirmation(
            &a,
            sqlly_intel::query::safety::ProductionDecision::NotProduction
        ));
    }

    #[test]
    fn safety_no_confirm_for_safe_update_when_dialog_disabled() {
        let s = SafetyFeatures {
            use_confirmation_dialog: false,
            ..Default::default()
        };
        let a = analyze("UPDATE t SET x = 1 WHERE id = 5").unwrap();
        assert!(!s.requires_confirmation(
            &a,
            sqlly_intel::query::safety::ProductionDecision::NotProduction
        ));
    }

    #[test]
    fn safety_only_allow_select_blocks_everything() {
        let s = SafetyFeatures {
            use_confirmation_dialog: false,
            only_allow_select: true,
            ..Default::default()
        };
        let a = analyze("INSERT INTO t VALUES (1)").unwrap();
        assert!(s.requires_confirmation(
            &a,
            sqlly_intel::query::safety::ProductionDecision::NotProduction
        ));
    }

    #[test]
    fn pg_dollar_quoted_body_is_one_statement() {
        use sqlly_lang::sqlmodel::Dialect;
        let sql = "CREATE FUNCTION f() RETURNS int AS $$ BEGIN RETURN 1; END; $$ LANGUAGE plpgsql;\nSELECT f();";
        let cursor = sql.find("RETURN 1").unwrap();
        let (s, e) = statement_span_at_for(sql, cursor, Dialect::Postgres);
        assert!(sql[s..e].contains("CREATE FUNCTION"));
        assert!(sql[s..e].contains("RETURN 1;"));
        assert!(!sql[s..e].contains("SELECT f()"));
    }

    #[test]
    fn pg_tagged_dollar_quote() {
        use sqlly_lang::sqlmodel::Dialect;
        let sql = "SELECT $body$ inner ; still $body$;";
        let cursor = sql.find("inner").unwrap();
        let (s, e) = statement_span_at_for(sql, cursor, Dialect::Postgres);
        assert_eq!(sql[s..e].trim(), sql.trim());
    }

    #[test]
    fn mysql_delimiter_block() {
        use sqlly_lang::sqlmodel::Dialect;
        let sql = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END//\nDELIMITER ;\nSELECT 3;";
        let spans = crate::editor::statements::statement_spans_for(sql, Dialect::MySql);
        assert_eq!(spans.len(), 2, "spans: {:?}", spans);
        let proc_span = &spans[0];
        let proc_text = &sql[proc_span.start..proc_span.end];
        assert!(proc_text.contains("DELIMITER //"));
        assert!(proc_text.contains("SELECT 1;"));
        assert!(proc_text.contains("SELECT 2;"));
        assert_eq!(sql[spans[1].start..spans[1].end].trim(), "SELECT 3;");
    }

    #[test]
    fn tsql_go_still_splits_under_new_path() {
        let sql = "SELECT 1\nGO\nSELECT 2";
        let spans = crate::editor::statements::statement_spans(sql);
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn sqlite_semicolons() {
        use sqlly_lang::sqlmodel::Dialect;
        let sql = "SELECT 1;\nSELECT 2;";
        let spans = crate::editor::statements::statement_spans_for(sql, Dialect::Sqlite);
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn spans_tile_buffer_for_every_dialect() {
        use sqlly_lang::sqlmodel::Dialect;
        let sql = "SELECT 1;\nSELECT 2;";
        for dialect in Dialect::ALL {
            let spans = crate::editor::statements::statement_spans_for(sql, dialect);
            if spans.is_empty() {
                continue;
            }
            assert_eq!(spans[0].start, 0, "{dialect:?}");
            assert_eq!(spans.last().unwrap().end, sql.len(), "{dialect:?}");
            for pair in spans.windows(2) {
                assert_eq!(pair[0].end, pair[1].start, "{dialect:?}");
            }
        }
    }

    #[test]
    fn cursor_at_end_returns_last_statement() {
        use sqlly_lang::sqlmodel::Dialect;
        let sql = "SELECT 1;\nSELECT 2";
        let (s, e) = statement_span_at_for(sql, sql.len(), Dialect::Postgres);
        assert_eq!(sql[s..e].trim(), "SELECT 2");
    }

    #[test]
    fn statement_span_at_splits_on_semicolons() {
        let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
        // Cursor inside the second statement.
        let cursor = sql.find('2').unwrap();
        let (s, e) = statement_span_at(sql, cursor);
        assert_eq!(sql[s..e].trim(), "SELECT 2;");
    }

    #[test]
    fn statement_span_at_splits_on_go_lines() {
        let sql = "SELECT 1\nGO\nSELECT 2\nGO\nSELECT 3";
        let cursor = sql.find('3').unwrap();
        let (s, e) = statement_span_at(sql, cursor);
        assert_eq!(sql[s..e].trim(), "SELECT 3");
        let cursor = sql.find('1').unwrap();
        let (s, e) = statement_span_at(sql, cursor);
        assert_eq!(sql[s..e].trim(), "SELECT 1");
    }

    #[test]
    fn statement_span_at_ignores_semicolons_in_strings_and_parens() {
        let sql = "SELECT ';' FROM t WHERE x IN (1;2)\nSELECT 9;";
        // The odd `;` inside parens/string must not split; the second SELECT
        // head still starts its own statement, so the cursor on the first
        // statement spans only that statement.
        let cursor = sql.find('t').unwrap();
        let (s, e) = statement_span_at(sql, cursor);
        assert!(sql[s..e].contains("SELECT ';'"));
        assert!(sql[s..e].contains("(1;2)"));
        assert!(!sql[s..e].contains("SELECT 9"));
        let cursor = sql.find('9').unwrap();
        let (s, e) = statement_span_at(sql, cursor);
        assert_eq!(sql[s..e].trim(), "SELECT 9;");
    }

    #[test]
    fn statement_span_at_undelimited_returns_whole_buffer() {
        let sql = "SELECT a\nFROM t\nWHERE x = 1";
        let (s, e) = statement_span_at(sql, 5);
        assert_eq!((s, e), (0, sql.len()));
    }

    #[test]
    fn statement_span_at_cursor_at_end() {
        let sql = "SELECT 1;\nSELECT 2";
        let (s, e) = statement_span_at(sql, sql.len());
        assert_eq!(sql[s..e].trim(), "SELECT 2");
    }

    /// Perf smoke: the span scan runs on Execute over the whole buffer.
    /// Generous bound; guards against accidental O(n^2).
    #[test]
    fn statement_span_large_buffer_smoke() {
        let unit = "SELECT 'value' FROM dbo.Table1 WHERE x = 1;\n";
        let mut sql = String::new();
        while sql.len() < 400_000 {
            sql.push_str(unit);
        }
        let started = std::time::Instant::now();
        for i in 0..10 {
            let cursor = (sql.len() / 10) * i;
            let (s, e) = statement_span_at(&sql, cursor);
            assert!(e > s);
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "10 span scans of a 400KB buffer took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn review_mask_keeps_newlines_and_hides_quoted_names() {
        let views = sqlly_lang::seclex::views(
            "/* line1\nline2 */ SELECT [where] FROM t",
            sqlly_lang::seclex::LexTarget::Engine(sqlly_lang::seclex::LexEngine::TSql),
        );
        let masked = mask_for_review(&views.primary().shadow);
        assert!(masked.contains('\n'));
        assert!(!masked.contains("where"), "{masked}");
    }

    #[test]
    fn procedure_definition_body_is_not_analyzed() {
        // Deploying a procedure runs none of its body's DML — an unbounded
        // UPDATE/DELETE inside the body must not trigger the confirmation.
        for head in [
            "CREATE PROCEDURE dbo.Cleanup AS",
            "CREATE OR ALTER PROCEDURE dbo.Cleanup AS",
            "ALTER PROCEDURE dbo.Cleanup AS",
            "CREATE TRIGGER trg ON dbo.T AFTER INSERT AS",
            "CREATE FUNCTION dbo.F() RETURNS int AS",
        ] {
            let sql = format!("{head}\nBEGIN\n  UPDATE t SET x = 1;\n  DELETE FROM u;\nEND");
            assert!(
                analyze_for(DatabaseType::SqlServer, &sql).is_none(),
                "{head}"
            );
            // Where a definition's extent is not exact (engine unknown, or
            // MySQL where the server bounds the body), the body is analyzed
            // rather than trusting the definition to run to the end.
            assert!(analyze(&sql).is_some(), "{head}");
        }
    }

    #[test]
    fn dml_in_batch_after_procedure_deploy_is_still_analyzed() {
        let sql = "CREATE PROCEDURE dbo.P AS\nBEGIN\n  SELECT 1;\nEND\nGO\nUPDATE t SET x = 1";
        let a = analyze_for(DatabaseType::SqlServer, sql).expect("the post-GO UPDATE is real DML");
        assert_eq!(a.statements.len(), 1);
        assert!(a.has_dangerous_unbounded_change());
    }

    #[test]
    fn leading_comment_does_not_hide_procedure_definition() {
        let sql = "-- deploy\n/* v2 */\nCREATE PROCEDURE dbo.P AS\nBEGIN\n  DELETE FROM u;\nEND";
        assert!(analyze_for(DatabaseType::SqlServer, sql).is_none());
    }

    #[test]
    fn foreign_key_cascade_actions_are_not_mutations() {
        // `ON DELETE CASCADE` / `ON UPDATE CASCADE` in DDL are FK actions.
        let sql = "CREATE TABLE dbo.Child (Id int, ParentId int, \
            CONSTRAINT FK FOREIGN KEY (ParentId) REFERENCES dbo.Parent (Id) \
            ON DELETE CASCADE ON UPDATE CASCADE)";
        assert!(analyze(sql).is_none());
        let alter = "ALTER TABLE dbo.Child ADD CONSTRAINT FK FOREIGN KEY (ParentId) \
            REFERENCES dbo.Parent (Id) ON DELETE SET NULL";
        assert!(analyze(alter).is_none());
    }

    #[test]
    fn select_for_update_is_not_a_mutation() {
        assert!(analyze("SELECT * FROM t WHERE id = 1 FOR UPDATE").is_none());
    }

    #[test]
    fn merge_then_clauses_are_not_separate_unbounded_statements() {
        // The WHEN … THEN UPDATE/DELETE/INSERT arms are constrained by the
        // merge's ON — they must not read as free-standing unbounded DML.
        let sql = "MERGE INTO t USING s ON t.id = s.id \
            WHEN MATCHED THEN UPDATE SET t.x = s.x \
            WHEN NOT MATCHED THEN INSERT (id, x) VALUES (s.id, s.x) \
            WHEN NOT MATCHED BY SOURCE THEN DELETE;";
        let a = analyze(sql).unwrap();
        assert_eq!(a.change_types(), vec![ChangeType::Merge]);
        assert!(!a.has_dangerous_unbounded_change());
    }

    #[test]
    fn upsert_clauses_are_not_separate_updates() {
        // MySQL and Postgres/SQLite upsert forms: the UPDATE is a clause of
        // the INSERT, keyed by the conflict target.
        let mysql = "INSERT INTO t (id, x) VALUES (1, 2) ON DUPLICATE KEY UPDATE x = 2";
        let a = analyze(mysql).unwrap();
        assert_eq!(a.change_types(), vec![ChangeType::Insert]);
        assert!(!a.has_dangerous_unbounded_change());

        let pg = "INSERT INTO t (id, x) VALUES (1, 2) ON CONFLICT (id) DO UPDATE SET x = 2";
        let a = analyze(pg).unwrap();
        assert_eq!(a.change_types(), vec![ChangeType::Insert]);
        assert!(!a.has_dangerous_unbounded_change());
    }

    #[test]
    fn session_scoped_targets_are_not_mutations() {
        for sql in [
            "DELETE FROM #staging",
            "DELETE #staging",
            "UPDATE #staging SET x = 1",
            "INSERT INTO #staging VALUES (1)",
            "UPDATE @rows SET x = 1",
            "TRUNCATE TABLE #staging",
        ] {
            assert!(analyze_for(DatabaseType::SqlServer, sql).is_none(), "{sql}");
        }
        // A real table in the same batch is still found.
        let a = analyze_for(
            DatabaseType::SqlServer,
            "DELETE FROM #staging; DELETE FROM real_table",
        )
        .unwrap();
        assert_eq!(a.statements.len(), 1);
        assert!(a.has_dangerous_unbounded_change());
    }

    #[test]
    fn trigger_update_function_is_not_a_mutation() {
        // `IF UPDATE(col)` is the column-test function, not an UPDATE.
        assert!(analyze("IF UPDATE(Name) SELECT 1").is_none());
    }

    #[test]
    fn subquery_where_does_not_bound_the_outer_update() {
        // The only WHERE lives inside the scalar subquery; the UPDATE itself
        // touches every row.
        let a = analyze("UPDATE t SET x = (SELECT MAX(v) FROM u WHERE u.k = 1)").unwrap();
        assert!(a.has_dangerous_unbounded_change());
    }

    #[test]
    fn following_statements_where_does_not_bound_an_earlier_verb() {
        // No semicolons: both statements land in one scan. The DELETE's WHERE
        // must not vouch for the unbounded UPDATE before it.
        let a = analyze("BEGIN TRAN UPDATE t SET x = 1 DELETE FROM u WHERE id = 2 COMMIT").unwrap();
        assert_eq!(a.statements.len(), 2);
        let update = &a.statements[0];
        assert_eq!(update.change_type, ChangeType::Update);
        assert!(update.is_unbounded_dangerous());
        assert!(a.statements[1].has_where_clause);
    }

    /// The report that motivated the security lexer: a backtick identifier
    /// holding `'` opened a fake literal in the old masker and hid the DELETE.
    #[test]
    fn reported_backtick_quote_repro_is_found() {
        let sql = "SELECT 1 AS `'`; DELETE FROM t; SELECT 1 AS `'`";
        for db in [
            DatabaseType::MySql,
            DatabaseType::Sqlite,
            DatabaseType::ClickHouse,
        ] {
            let a = analyze_for(db, sql).unwrap_or_else(|| panic!("{db:?} must find the DELETE"));
            assert_eq!(a.change_types(), vec![ChangeType::Delete], "{db:?}");
            assert!(a.has_dangerous_unbounded_change(), "{db:?}");
            assert!(contains_non_select_statement_for(db, sql), "{db:?}");
        }
        assert!(analyze(sql).is_some());
        for (db, sql) in [
            (
                DatabaseType::Postgres,
                "SELECT 1 AS \"'\"; DELETE FROM t; SELECT 1 AS \"'\"",
            ),
            (
                DatabaseType::SqlServer,
                "SELECT 1 AS [']; DELETE FROM t; SELECT 1 AS [']",
            ),
            (
                DatabaseType::Postgres,
                "SELECT $$'$$; DELETE FROM t; SELECT $$'$$",
            ),
            (DatabaseType::MySql, "SELECT '\\''; DELETE FROM t; SELECT '"),
            (DatabaseType::MySql, "SELECT 1 # '\nDELETE FROM t; -- '"),
        ] {
            let a = analyze_for(db, sql).unwrap_or_else(|| panic!("{db:?}: {sql}"));
            assert!(a.has_dangerous_unbounded_change(), "{db:?}: {sql}");
        }
    }

    /// T-SQL runs statements without separators; a following statement's
    /// WHERE must not bound an earlier UPDATE/DELETE.
    #[test]
    fn a_following_selects_where_does_not_bound_a_delete() {
        for sql in [
            "DELETE FROM t\nSELECT COUNT(*) FROM t WHERE id = 1",
            "UPDATE t SET x = 1\nSELECT * FROM u WHERE id = 1",
            "DELETE FROM t EXEC dbo.P @where = 1",
        ] {
            let a = analyze_for(DatabaseType::SqlServer, sql).unwrap();
            assert!(a.has_dangerous_unbounded_change(), "{sql}");
        }
        // A table hint or a subquery is still part of the statement.
        let a = analyze_for(
            DatabaseType::SqlServer,
            "DELETE FROM t WITH (ROWLOCK) WHERE id IN (SELECT id FROM u)",
        )
        .unwrap();
        assert!(!a.has_dangerous_unbounded_change());
    }

    /// BEFORE/AFTER are unreserved in T-SQL: an alias named `after` ends the
    /// SELECT, and the DELETE after it is a statement of its own.
    #[test]
    fn unreserved_trigger_words_are_only_clauses_inside_ddl() {
        let a = analyze_for(DatabaseType::SqlServer, "SELECT 1 AS after\nDELETE FROM t").unwrap();
        assert!(a.has_dangerous_unbounded_change());
        let a = analyze_for(DatabaseType::SqlServer, "SELECT 1 AS do DELETE FROM t").unwrap();
        assert!(a.has_dangerous_unbounded_change());
        // Inside trigger DDL they are still clauses.
        assert!(
            analyze_for(
                DatabaseType::Postgres,
                "CREATE TRIGGER trg AFTER DELETE ON t FOR EACH ROW EXECUTE FUNCTION f()"
            )
            .is_none()
        );
    }

    /// A quoted identifier named like a keyword is a name, never a clause.
    #[test]
    fn quoted_names_never_vouch_as_where() {
        for (db, sql) in [
            (DatabaseType::SqlServer, "DELETE FROM [where]"),
            (DatabaseType::Postgres, "DELETE FROM \"where\""),
            (DatabaseType::MySql, "DELETE FROM `where`"),
        ] {
            let a = analyze_for(db, sql).unwrap();
            assert!(a.has_dangerous_unbounded_change(), "{db:?}: {sql}");
        }
    }

    /// MySQL bounds a routine body itself, so a statement after a CREATE
    /// PROCEDURE in the same batch is real DML and must be found.
    #[test]
    fn mysql_definition_does_not_hide_following_dml() {
        let sql = "CREATE PROCEDURE p() SELECT 1; DELETE FROM t";
        let a = analyze_for(DatabaseType::MySql, sql).unwrap();
        assert!(a.has_dangerous_unbounded_change());
    }

    /// Oracle and libSQL definitions travel as one unit (the driver's own
    /// split), so their bodies are skipped exactly and what follows is not.
    #[test]
    fn unit_split_engines_skip_only_the_definition_unit() {
        let sql =
            "CREATE OR REPLACE PROCEDURE p AS\nBEGIN\n  DELETE FROM u;\nEND;\n/\nDELETE FROM t";
        let a = analyze_for(DatabaseType::Oracle, sql).unwrap();
        assert_eq!(a.statements.len(), 1);
        assert!(a.has_dangerous_unbounded_change());
        let sql = "CREATE TRIGGER trg AFTER INSERT ON t BEGIN DELETE FROM u; END;\nSELECT 1";
        assert!(analyze_for(DatabaseType::LibSql, sql).is_none());
    }

    /// Text the engine cannot read reliably always asks for confirmation.
    #[test]
    fn ambiguous_batches_require_confirmation() {
        let a = analyze_for(DatabaseType::Postgres, "SELECT 'unterminated").unwrap();
        assert!(a.ambiguity.is_some());
        assert!(a.statements.is_empty());
        assert!(a.has_dangerous_unbounded_change());
        assert_eq!(a.bottom_line(), "WE CANNOT VERIFY WHAT THIS BATCH CHANGES.");
        let safety = SafetyFeatures {
            allow_unconstrained_update: false,
            allow_unconstrained_delete: false,
            ..Default::default()
        };
        assert!(safety.requires_confirmation(
            &a,
            sqlly_intel::query::safety::ProductionDecision::NotProduction
        ));
        let a = analyze_for(DatabaseType::MySql, "SELECT 1\0; DELETE FROM t").unwrap();
        assert!(a.ambiguity.unwrap().contains("NUL"));
    }

    #[test]
    fn go_separator_forms() {
        assert!(is_go_separator_line("GO\n"));
        assert!(is_go_separator_line("  go  \n"));
        // Masked trailing comment leaves only its shells.
        assert!(is_go_separator_line("GO --        \n"));
        assert!(!is_go_separator_line("GO 5\n"));
        assert!(!is_go_separator_line("goto x\n"));
        assert!(!is_go_separator_line("select go\n"));
    }
}
