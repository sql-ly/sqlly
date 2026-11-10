//! Server lifecycle and connection management: the `initialize` handshake and
//! its advertised capabilities, plus the `sqlly/setConnection` and
//! `sqlly/refreshCatalog` handlers that (re)build the cached completion
//! catalog off the read loop.

use sqlly_core::ConnectionProfile;
use sqlly_intel::intellisense::CompletionCatalog;
use sqlly_services::{ProcedureInfo, Services};
#[cfg(not(target_arch = "wasm32"))]
use tracing::warn;

#[cfg(not(target_arch = "wasm32"))]
use crate::jsonrpc::{Id, Message, error_code};
use crate::types::*;

#[cfg(not(target_arch = "wasm32"))]
use super::cancel::respond_unless_cancelled;
use super::features::semantic_token_legend;
#[cfg(not(target_arch = "wasm32"))]
use super::profile::{ConnectionParams, build_profile};
#[cfg(not(target_arch = "wasm32"))]
use super::{LspServer, parse};

#[cfg(not(target_arch = "wasm32"))]
impl LspServer {
    pub(super) fn handle_initialize(&self, msg: Message, id: Id) -> Vec<Message> {
        // We accept initialization_options for an inline connection profile but
        // do not require it.
        let _params: InitializeParams = match parse(&msg) {
            Ok(p) => p,
            Err((id, err)) => return vec![Message::error(id, err.code, err.message)],
        };
        vec![Message::response(
            id,
            serde_json::to_value(initialize_result()).unwrap(),
        )]
    }

    pub(super) fn spawn_set_connection(&mut self, msg: Message, id: Id) {
        let params: ConnectionParams = match parse(&msg) {
            Ok(p) => p,
            Err((id, err)) => {
                self.send(Message::error(id, err.code, err.message));
                return;
            }
        };
        let profile = match build_profile(params) {
            Ok(p) => p,
            Err(e) => {
                self.send(Message::error(id, error_code::INVALID_PARAMS, e));
                return;
            }
        };
        {
            let mut g = self.shared.lock().unwrap();
            g.profile = Some(profile.clone());
        }
        let services = self.services.clone();
        let shared = self.shared.clone();
        let out = self.out();
        // Eagerly load the completion catalog + procedure parameters from the
        // connection so subsequent completion requests are pure-CPU (hot path
        // invariant). Runs off the read loop.
        let handle = self.rt.spawn(async move {
            match build_catalog(&services, &profile).await {
                Ok((catalog, procedures)) => {
                    let mut g = shared.lock().unwrap();
                    g.completion_catalog = catalog;
                    g.completion_procedures = procedures;
                }
                Err(e) => warn!("LSP: failed to load completion catalog: {e}"),
            }
            let _ = out.send(respond_unless_cancelled(
                &shared,
                id,
                || serde_json::json!({"connected": true}),
            ));
        });
        self.pending.push(handle);
    }

    /// Rebuilds the completion catalog from the currently-set connection
    /// profile. Useful after the schema changes (DDL) without re-sending the
    /// full connection params. No-op (returns `refreshed: false`) when no
    /// connection has been set yet. Runs off the read loop.
    pub(super) fn spawn_refresh_catalog(&mut self, id: Id) {
        let profile = { self.shared.lock().unwrap().profile.clone() };
        let Some(profile) = profile else {
            self.send(Message::response(
                id,
                serde_json::json!({"refreshed": false, "reason": "no connection set"}),
            ));
            return;
        };
        let services = self.services.clone();
        let shared = self.shared.clone();
        let out = self.out();
        let handle = self.rt.spawn(async move {
            match build_catalog(&services, &profile).await {
                Ok((catalog, procedures)) => {
                    let mut g = shared.lock().unwrap();
                    g.completion_catalog = catalog;
                    g.completion_procedures = procedures;
                }
                Err(e) => warn!("LSP: failed to refresh completion catalog: {e}"),
            }
            let _ = out.send(respond_unless_cancelled(
                &shared,
                id,
                || serde_json::json!({"refreshed": true}),
            ));
        });
        self.pending.push(handle);
    }
}

/// Build a fresh [`CompletionCatalog`] + procedure list (with parameters) from
/// the active connection's metadata. Free async fn so it can run in a spawned
/// task without borrowing `&mut self`.
/// Load the completion catalog + procedures from a connection. Delegates to the
/// shared [`sqlly_services::build_completion_state`] (the same path the gpui app
/// uses), with `load_parameters = true`: the LSP has no lazy tree-expansion
/// signal (unlike the GPUI explorer), so it eagerly loads each procedure's
/// parameters up front so EXEC declaration snippets carry real parameter slots.
async fn build_catalog(
    services: &Services,
    profile: &ConnectionProfile,
) -> Result<(CompletionCatalog, Vec<ProcedureInfo>), String> {
    let database = profile
        .database
        .clone()
        .unwrap_or_else(|| "master".to_string());
    let state = sqlly_services::build_completion_state(
        services.metadata.as_ref(),
        profile,
        &database,
        true,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok((state.catalog, state.procedures))
}

/// The exact `InitializeResult` the server returns for `initialize`. Builds the
/// advertised `ServerCapabilities` from [`server_capabilities`] so the debug
/// screen shows the same capability document the real server emits.
pub fn initialize_result() -> InitializeResult {
    InitializeResult {
        capabilities: server_capabilities(),
        server_info: ServerInfo {
            name: "sqlly-lsp".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    }
}

/// The `ServerCapabilities` the server advertises. Pure (no `self`), so it can
/// be called without a live server.
pub fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: TextDocumentSyncKind::Incremental,
        completion_provider: CompletionProvider {
            trigger_characters: vec![".".to_string(), "@".to_string()],
        },
        hover_provider: true,
        diagnostic_provider: DiagnosticOptions {
            inter_file_dependencies: false,
            workspace_diagnostics: false,
        },
        workspace_symbol_provider: true,
        semantic_tokens_provider: SemanticTokensOptions {
            legend: SemanticTokensLegend {
                token_types: semantic_token_legend(),
                token_modifiers: Vec::new(),
            },
            full: true,
        },
        definition_provider: true,
        references_provider: true,
        document_symbol_provider: true,
        document_formatting_provider: true,
    }
}
