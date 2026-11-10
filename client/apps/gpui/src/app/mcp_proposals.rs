use super::*;
use sqlly_services::mcp::proposals::{Proposal, ProposalKind, ProposalStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProposalAction {
    Reject(&'static str),
    Open,
    Write,
}

pub(super) fn classify_proposal(
    proposal: &Proposal,
    is_production: bool,
    profile_exists: bool,
) -> ProposalAction {
    if !profile_exists {
        return ProposalAction::Reject("connection no longer exists");
    }
    if is_production {
        return ProposalAction::Reject("production connections are never driven by MCP");
    }
    match proposal.kind {
        ProposalKind::Open => ProposalAction::Open,
        ProposalKind::Write => ProposalAction::Write,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn sql_hash(sql: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sql.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn write_proposal_audit(
    proposal: &Proposal,
    approval: sqlly_services::mcp::audit::AuditApproval,
    note: Option<String>,
) {
    use chrono::Utc;
    use sqlly_core::redact_sql;
    use sqlly_services::mcp::audit::{AuditOutcome, AuditRecord, append};

    let store = crate::connection::ConnectionStore::global();
    let profile = store.get(proposal.connection);
    let record = AuditRecord {
        ts: Utc::now(),
        tool: "proposal".into(),
        connection_id: Some(proposal.connection),
        connection_name: profile.as_ref().map(|profile| profile.name.clone()),
        database: proposal.database.clone(),
        sql_hash: proposal.sql.as_ref().map(|sql| sql_hash(sql)),
        sql_redacted: proposal
            .sql
            .as_ref()
            .map(|sql| redact_sql(sql).chars().take(2_000).collect()),
        duration_ms: None,
        rows: None,
        outcome: AuditOutcome::Ok,
        proposal_id: Some(proposal.id),
        approval: Some(approval),
    };
    if let Some(reason) = note {
        let _ = reason;
    }
    let _ = append(&record);
}

impl AppView {
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn poll_mcp_proposals(&mut self, cx: &mut Context<Self>) {
        use chrono::Utc;
        use sqlly_services::mcp::audit::AuditApproval;
        use sqlly_services::mcp::proposals::{list_pending, sweep_expired, update_status};

        let _ = sweep_expired(Utc::now());
        let Ok(pending) = list_pending() else {
            return;
        };

        let store = crate::connection::ConnectionStore::global();
        for proposal in pending {
            if self
                .mcp_pending_write
                .as_ref()
                .is_some_and(|(id, _)| *id == proposal.id)
            {
                continue;
            }

            let profile = store.get(proposal.connection);
            let profile_exists = profile.is_some();
            let is_production = profile.as_ref().is_some_and(|profile| {
                crate::mutation_review::SafetyFeatures::effective_for_profile(profile, store)
                    .is_production
            });

            match classify_proposal(&proposal, is_production, profile_exists) {
                ProposalAction::Reject(reason) => {
                    let _ = update_status(
                        proposal.id,
                        ProposalStatus::Rejected,
                        Some(reason.to_string()),
                    );
                    write_proposal_audit(&proposal, AuditApproval::Rejected, Some(reason.into()));
                    if is_production {
                        self.set_status(
                            StatusLevel::Error,
                            crate::localization::text("shell.mcp.production_rejected"),
                        );
                        cx.notify();
                    }
                }
                ProposalAction::Open => {
                    let Some(profile) = profile else {
                        continue;
                    };
                    let sql = proposal.sql.clone().unwrap_or_default();
                    let proposal_id = proposal.id;
                    self.handle_mcp_open_proposal(profile, sql, proposal_id, cx);
                }
                ProposalAction::Write => {
                    let Some(profile) = profile else {
                        continue;
                    };
                    let sql = proposal.sql.clone().unwrap_or_default();
                    let proposal_id = proposal.id;
                    self.handle_mcp_write_proposal(profile, sql, proposal_id, cx);
                }
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn poll_mcp_proposals(&mut self, _cx: &mut Context<Self>) {}

    #[cfg(not(target_arch = "wasm32"))]
    fn handle_mcp_open_proposal(
        &mut self,
        profile: sqlly_core::ConnectionProfile,
        sql: String,
        proposal_id: uuid::Uuid,
        cx: &mut Context<Self>,
    ) {
        use sqlly_services::mcp::proposals::{ProposalStatus, update_status};

        let needs_connect = self.active_profile.as_ref().map(|p| p.id) != Some(profile.id);
        let run = move |app: &mut Self, cx: &mut Context<Self>| {
            app.new_tab(cx, Some(sql));
            let _ = update_status(proposal_id, ProposalStatus::Opened, None);
            app.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.mcp.query_opened"),
            );
            cx.notify();
        };
        if needs_connect {
            self.connect_profile_then(profile, cx, run);
        } else {
            run(self, cx);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn handle_mcp_write_proposal(
        &mut self,
        profile: sqlly_core::ConnectionProfile,
        sql: String,
        proposal_id: uuid::Uuid,
        cx: &mut Context<Self>,
    ) {
        use sqlly_services::mcp::audit::AuditApproval;

        self.mcp_pending_write = Some((proposal_id, String::new()));
        let needs_connect = self.active_profile.as_ref().map(|p| p.id) != Some(profile.id);
        let services = self.services_for_profile(&profile);
        let sql_for_preflight = sql.clone();
        let mut profile_for_preflight = profile.clone();
        profile_for_preflight.block_all_but_select = true;

        cx.spawn(async move |app, cx| {
            let preflight = crate::mcp_preflight::run_preflight(
                &services,
                &profile_for_preflight,
                &sql_for_preflight,
            )
            .await;
            let summary = crate::mcp_preflight::preflight_summary(&preflight);
            let _ = app.update(cx, |app, cx| {
                app.notifications.push(
                    crate::notification_center::NotificationKind::QueryCompleted,
                    summary.clone(),
                    chrono::Utc::now(),
                );
                app.set_status(StatusLevel::Info, summary);
                let profile_for_grant = profile.clone();
                let run = move |app: &mut Self, cx: &mut Context<Self>| {
                    if let Ok(Some(proposal)) =
                        sqlly_services::mcp::proposals::read_proposal(proposal_id)
                    {
                        write_proposal_audit(&proposal, AuditApproval::Approved, None);
                    }
                    app.agent_write_grant = Some(crate::agent_grant::WriteGrant::new(
                        sql.as_str(),
                        &profile_for_grant,
                    ));
                    app.new_tab(cx, Some(sql.clone()));
                    let tab_id = app
                        .tabs
                        .get(app.active_tab)
                        .map(|tab| tab.id.clone())
                        .unwrap_or_default();
                    app.mcp_pending_write = Some((proposal_id, tab_id.clone()));
                    app.execute_query_with_origin(
                        app.active_tab,
                        super::execute::ExecuteOrigin::AgentGrant { sql },
                        cx,
                    );
                    if app
                        .mcp_pending_write
                        .as_ref()
                        .is_some_and(|(_, id)| id == &tab_id)
                        && app.pending.mutation_review.is_none()
                        && app.agent_write_grant.is_none()
                    {
                        app.reject_mcp_pending_write_if_tab(
                            &tab_id,
                            "could not execute MCP proposal",
                        );
                    }
                };
                if needs_connect {
                    app.connect_profile_then(profile, cx, run);
                } else {
                    run(app, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn reject_mcp_pending_write_if_tab(&mut self, tab_id: &str, note: &str) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use sqlly_services::mcp::audit::AuditApproval;
            use sqlly_services::mcp::proposals::{ProposalStatus, read_proposal, update_status};
            if self
                .mcp_pending_write
                .as_ref()
                .is_some_and(|(_, id)| id == tab_id)
                && let Some((proposal_id, _)) = self.mcp_pending_write.take()
            {
                let _ = update_status(
                    proposal_id,
                    ProposalStatus::Rejected,
                    Some(note.to_string()),
                );
                if let Ok(Some(proposal)) = read_proposal(proposal_id) {
                    write_proposal_audit(&proposal, AuditApproval::Rejected, Some(note.into()));
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (tab_id, note);
            self.mcp_pending_write = None;
        }
    }

    pub(super) fn complete_mcp_pending_write_if_tab(&mut self, tab_id: &str, note: Option<String>) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use sqlly_services::mcp::audit::AuditApproval;
            use sqlly_services::mcp::proposals::{ProposalStatus, read_proposal, update_status};
            if self
                .mcp_pending_write
                .as_ref()
                .is_some_and(|(_, id)| id == tab_id)
                && let Some((proposal_id, _)) = self.mcp_pending_write.take()
            {
                let _ = update_status(proposal_id, ProposalStatus::Executed, note.clone());
                if let Ok(Some(proposal)) = read_proposal(proposal_id) {
                    write_proposal_audit(&proposal, AuditApproval::Executed, note);
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (tab_id, note);
            self.mcp_pending_write = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::mcp::proposals::{ProposalKind, new_proposal};
    use uuid::Uuid;

    #[test]
    fn classify_missing_profile_rejects() {
        let proposal = new_proposal(
            ProposalKind::Open,
            Uuid::new_v4(),
            None,
            Some("SELECT 1".into()),
        );
        assert_eq!(
            classify_proposal(&proposal, false, false),
            ProposalAction::Reject("connection no longer exists")
        );
    }

    #[test]
    fn classify_production_rejects() {
        let proposal = new_proposal(
            ProposalKind::Write,
            Uuid::new_v4(),
            None,
            Some("UPDATE t SET x = 1".into()),
        );
        assert_eq!(
            classify_proposal(&proposal, true, true),
            ProposalAction::Reject("production connections are never driven by MCP")
        );
    }

    #[test]
    fn classify_open_opens() {
        let proposal = new_proposal(
            ProposalKind::Open,
            Uuid::new_v4(),
            None,
            Some("SELECT 1".into()),
        );
        assert_eq!(
            classify_proposal(&proposal, false, true),
            ProposalAction::Open
        );
    }

    #[test]
    fn classify_write_writes() {
        let proposal = new_proposal(
            ProposalKind::Write,
            Uuid::new_v4(),
            None,
            Some("UPDATE t SET x = 1".into()),
        );
        assert_eq!(
            classify_proposal(&proposal, false, true),
            ProposalAction::Write
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn terminal_transitions_write_audit() {
        use sqlly_services::mcp::audit::{AuditApproval, read_recent_in};
        use sqlly_services::mcp::proposals::ProposalStatus;

        let dir = tempfile::tempdir().unwrap();
        // SAFETY: test-only env override; restored before return.
        unsafe {
            std::env::set_var("SQLLY_MCP_AUDIT_DIR", dir.path());
        }

        let proposal = new_proposal(
            ProposalKind::Write,
            Uuid::new_v4(),
            Some("app".into()),
            Some("UPDATE t SET x = 1".into()),
        );
        write_proposal_audit(&proposal, AuditApproval::Approved, None);
        write_proposal_audit(&proposal, AuditApproval::Rejected, Some("declined".into()));
        write_proposal_audit(
            &sqlly_services::mcp::proposals::Proposal {
                status: ProposalStatus::Executed,
                ..proposal
            },
            AuditApproval::Executed,
            None,
        );

        let records = read_recent_in(dir.path(), 10).expect("audit readable");
        assert_eq!(records.len(), 3);
        assert!(records.iter().all(|record| record.tool == "proposal"));
        unsafe {
            std::env::remove_var("SQLLY_MCP_AUDIT_DIR");
        }
    }
}
