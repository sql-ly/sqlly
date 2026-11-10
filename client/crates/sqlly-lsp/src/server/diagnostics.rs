//! Diagnostics: the shared validation → LSP-diagnostic conversion, the
//! debounced push path (`textDocument/publishDiagnostics`), and the pull path
//! (`textDocument/diagnostic`).

use sqlly_ai::catalog::SqlValidationResult;
use sqlly_core::ConnectionProfile;
use sqlly_services::Services;
use tracing::warn;

#[cfg(not(target_arch = "wasm32"))]
use crate::jsonrpc::{Id, Message};
use crate::types::*;

#[cfg(not(target_arch = "wasm32"))]
use super::cancel::{cancelled_reply, respond_unless_cancelled};
use super::document::byte_span_to_range;
#[cfg(not(target_arch = "wasm32"))]
use super::{DIAGNOSTICS_DEBOUNCE, LspServer, parse};

#[cfg(not(target_arch = "wasm32"))]
impl LspServer {
    /// Spawn a push-model `textDocument/publishDiagnostics` for `uri`. No-op
    /// if the document is unknown. Debounced (trailing edge) and generation
    /// gated: a burst of changes yields one validation of the final text, and
    /// a validation that raced with a newer change never publishes.
    pub(super) fn spawn_publish_diagnostics(&mut self, uri: String) {
        let generation = {
            let g = self.shared.lock().unwrap();
            match g.docs.get(&uri) {
                Some(doc) => doc.generation,
                None => return,
            }
        };
        let services = self.services.clone();
        let shared = self.shared.clone();
        let out = self.out();
        let handle = self.rt.spawn(async move {
            tokio::time::sleep(DIAGNOSTICS_DEBOUNCE).await;
            // Only the task belonging to the newest change proceeds.
            let (profile, sql, version) = {
                let g = shared.lock().unwrap();
                match g.docs.get(&uri) {
                    Some(doc) if doc.generation == generation => {
                        (g.profile.clone(), doc.text.clone(), doc.version)
                    }
                    _ => return,
                }
            };
            let diagnostics = compute_diagnostics(&services, profile.as_ref(), Some(&sql)).await;
            // The document may have changed while validation ran: publishing
            // now would overwrite newer state with stale results.
            {
                let g = shared.lock().unwrap();
                match g.docs.get(&uri) {
                    Some(doc) if doc.generation == generation => {}
                    _ => return,
                }
            }
            let _ = out.send(Message::notification(
                "textDocument/publishDiagnostics",
                serde_json::to_value(&PublishDiagnosticsParams {
                    uri,
                    diagnostics,
                    version,
                })
                .unwrap(),
            ));
        });
        self.pending.push(handle);
    }

    /// Pull-model diagnostics: `textDocument/diagnostic`. Spawns the validation
    /// and replies with a full document diagnostic report so the advertised
    /// `diagnosticProvider` capability is honored.
    pub(super) fn spawn_diagnostic(&mut self, msg: Message, id: Id) {
        let params: DocumentDiagnosticParams = match parse(&msg) {
            Ok(p) => p,
            Err((id, err)) => {
                self.send(Message::error(id, err.code, err.message));
                return;
            }
        };
        let (profile, sql) = {
            let g = self.shared.lock().unwrap();
            (
                g.profile.clone(),
                g.docs
                    .get(&params.text_document.uri)
                    .map(|d| d.text.clone()),
            )
        };
        let services = self.services.clone();
        let shared = self.shared.clone();
        let out = self.out();
        let handle = self.rt.spawn(async move {
            // Fast path: a `$/cancelRequest` that beat this task to the
            // runtime means nobody wants the result — skip the validation
            // work entirely instead of computing a report only to suppress it.
            if let Some(reply) = cancelled_reply(&shared, &id) {
                let _ = out.send(reply);
                return;
            }
            let items = compute_diagnostics(&services, profile.as_ref(), sql.as_deref()).await;
            let _ = out.send(respond_unless_cancelled(&shared, id, || {
                serde_json::to_value(&DocumentDiagnosticReport {
                    kind: "full".to_string(),
                    items,
                })
                .unwrap()
            }));
        });
        self.pending.push(handle);
    }
}

/// Compute the diagnostic list for a document. Returns an empty list when there
/// is no active connection or the document is empty/missing. Shared by the push
/// path (`publishDiagnostics`) and the pull path (`textDocument/diagnostic`),
/// and by the GPUI IntelliSense debug screen (which re-runs the exact same
/// `Services` calls the LSP makes, so the screen shows precisely what the
/// language server would emit).
pub async fn compute_diagnostics(
    services: &Services,
    profile: Option<&ConnectionProfile>,
    sql: Option<&str>,
) -> Vec<DiagnosticLsp> {
    let Some(profile) = profile else {
        return Vec::new();
    };
    let Some(sql) = sql else {
        return Vec::new();
    };
    if sql.trim().is_empty() {
        return Vec::new();
    }
    // Validate in the dialect the profile actually speaks, not a hardcoded
    // one — dialect-specific rules (quoting, destructive-op syntax) differ.
    let dialect = profile.database_type.family();
    let result: Result<SqlValidationResult, String> = services
        .catalog
        .validate_sql(profile, sql, dialect)
        .await
        .map_err(|e| e.to_string());
    match result {
        Ok(validation) => validation
            .issues
            .iter()
            .map(|issue| DiagnosticLsp {
                range: issue
                    .span
                    .map(|span| byte_span_to_range(sql, span.start, span.end))
                    .unwrap_or_default(),
                severity: match issue.severity {
                    sqlly_ai::catalog::SqlIssueSeverity::Error => DiagnosticSeverity::Error,
                    sqlly_ai::catalog::SqlIssueSeverity::Warning => DiagnosticSeverity::Warning,
                    sqlly_ai::catalog::SqlIssueSeverity::Info => DiagnosticSeverity::Information,
                    sqlly_ai::catalog::SqlIssueSeverity::RequireConfirmation => {
                        DiagnosticSeverity::Warning
                    }
                },
                message: format!("{}: {}", issue_code_label(issue.code), issue.message),
                source: "sqlly".to_string(),
            })
            .collect::<Vec<_>>(),
        Err(e) => {
            warn!("LSP: validation failed: {e}");
            Vec::new()
        }
    }
}

fn issue_code_label(code: sqlly_ai::catalog::SqlIssueCode) -> &'static str {
    use sqlly_ai::catalog::SqlIssueCode::*;
    match code {
        ParseError => "ParseError",
        EmptyStatement => "EmptyStatement",
        MultipleStatements => "MultipleStatements",
        UnresolvedTable => "UnresolvedTable",
        AmbiguousTable => "AmbiguousTable",
        UnresolvedColumn => "UnresolvedColumn",
        AmbiguousColumn => "AmbiguousColumn",
        TenantScopeMissingPredicate => "TenantScopeMissingPredicate",
        UpdateWithoutWhere => "UpdateWithoutWhere",
        DeleteWithoutWhere => "DeleteWithoutWhere",
        DestructiveRequiresConfirmation => "DestructiveRequiresConfirmation",
        CrossDatabaseReference => "CrossDatabaseReference",
        LexicalAmbiguity => "LexicalAmbiguity",
    }
}
