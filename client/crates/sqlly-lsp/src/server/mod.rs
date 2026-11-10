//! The LSP server core: holds the in-process [`sqlly_services::Services`],
//! tracks open documents + the active connection, and dispatches JSON-RPC
//! requests to the shared intellisense / catalog / metadata ports.
//!
//! ## Concurrency model
//!
//! The server process owns its own multi-thread tokio runtime. The read loop
//! ([`LspServer::run`]) reads one message at a time from stdin and dispatches
//! it, but it never blocks on database I/O:
//!
//! - **Pure-CPU requests** (`completion`, `hover`, `semanticTokens`) run inline
//!   against the cached catalog and reply immediately — the hot path.
//! - **I/O-bound requests** (`workspace/symbol`, `textDocument/diagnostic`,
//!   `sqlly/setConnection`, `sqlly/refreshCatalog`, and the push-diagnostics triggered by
//!   `didOpen`/`didChange`) are spawned onto the tokio runtime. Each spawned
//!   task computes its reply and hands it to the writer. The read loop continues
//!   immediately, so a slow `validate_sql` can never stall a subsequent
//!   `completion`. Responses may therefore complete out of order, which the
//!   JSON-RPC `id` correlation makes safe (and the LSP spec permits).
//!
//! All outbound messages flow through a single `mpsc` channel drained by a
//! dedicated writer thread that owns the sink, so writes are serialized and the
//! handlers never contend on the wire. Mutable state (open docs, active
//! connection, completion caches) lives behind an `Arc<Mutex<_>>` shared with
//! the spawned tasks; locks are never held across an `.await`.
//!
//! ## Module layout
//!
//! This file owns the server state and the read-loop/dispatch skeleton; the
//! handlers and helpers live in sibling submodules:
//!
//! - [`cancel`] — `$/cancelRequest` bookkeeping for spawned tasks.
//! - [`document`] — document text maintenance + byte/UTF-16 position
//!   conversions.
//! - [`profile`] — `sqlly/setConnection` params → [`ConnectionProfile`].
//! - [`kinds`] — completion-kind and symbol-kind wire mappings.
//! - [`lifecycle`] — `initialize` / capabilities + connection & catalog
//!   handlers.
//! - [`diagnostics`] — push and pull diagnostics.
//! - [`features`] — the pure-CPU language features (completion, hover,
//!   definition/references, semantic tokens, symbols, formatting) and the
//!   reusable building blocks the GPUI debug screen shares.

#[cfg(not(target_arch = "wasm32"))]
mod cancel;
mod diagnostics;
mod document;
mod features;
mod kinds;
mod lifecycle;
mod profile;

pub use diagnostics::compute_diagnostics;
pub use document::{
    apply_content_change, apply_did_change, offset_to_position, position_to_offset,
};
pub use features::{
    IdentOccurrence, build_hover, completion_to_lsp, definition_spans, document_symbols,
    format_document, identifier_at, scan_identifiers, semantic_token_legend, semantic_tokens,
    semantic_tokens_for, tokenize_semantic, tokenize_semantic_for,
};
pub use kinds::completion_kind_to_lsp;
pub use lifecycle::{initialize_result, server_capabilities};

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::io;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::Sender;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Mutex};
#[cfg(not(target_arch = "wasm32"))]
use std::thread::JoinHandle;

#[cfg(not(target_arch = "wasm32"))]
use sqlly_core::ConnectionProfile;
#[cfg(not(target_arch = "wasm32"))]
use sqlly_intel::intellisense::CompletionCatalog;
#[cfg(not(target_arch = "wasm32"))]
use sqlly_services::{IntelliSenseRuleConfig, ProcedureInfo, Services};
#[cfg(not(target_arch = "wasm32"))]
use tokio::runtime::Handle;

#[cfg(not(target_arch = "wasm32"))]
use crate::jsonrpc;
use crate::jsonrpc::{Id, Message, RpcError, error_code};
use crate::types::*;

/// Mutable server state shared between the read loop and the spawned async
/// tasks. Guarded by a `Mutex`; locks are taken briefly and never held across
/// an `.await`.
#[cfg(not(target_arch = "wasm32"))]
struct Shared {
    /// Active connection profile. `None` until the client sends
    /// `sqlly/setConnection`; completion then uses an empty catalog and
    /// validation is a no-op.
    profile: Option<ConnectionProfile>,
    /// URI -> full document state (text + version + validation generation).
    docs: HashMap<String, DocumentState>,
    /// Request ids cancelled via `$/cancelRequest`. Entries are consumed by
    /// the spawned task that owns the id (or pruned once the set grows past
    /// a small bound, since unknown/already-answered ids never get consumed).
    cancelled: std::collections::HashSet<String>,
    /// Cached completion catalog built from the active connection's metadata.
    completion_catalog: CompletionCatalog,
    /// Cached procedure descriptors (with parameters) for enriched completion.
    completion_procedures: Vec<ProcedureInfo>,
    /// User-configurable completion rules (empty = no filtering).
    completion_rules: IntelliSenseRuleConfig,
}

#[cfg(not(target_arch = "wasm32"))]
impl Shared {
    fn new() -> Self {
        Self {
            profile: None,
            docs: HashMap::new(),
            cancelled: std::collections::HashSet::new(),
            completion_catalog: CompletionCatalog::default(),
            completion_procedures: Vec::new(),
            completion_rules: IntelliSenseRuleConfig::default(),
        }
    }
}

/// One open document: the current full text (maintained by applying each
/// incremental or full-replacement change in order), the client's version
/// number, and a monotonically increasing validation generation. The
/// generation gates async diagnostics: a validation captures the generation
/// it started from and publishes only if the document has not changed since,
/// so a slow older validation can never overwrite a newer one.
#[cfg(not(target_arch = "wasm32"))]
struct DocumentState {
    text: String,
    version: Option<i32>,
    generation: u64,
}

/// Trailing-edge debounce for push diagnostics: rapid `didChange` bursts
/// yield one validation of the final text instead of one per keystroke.
#[cfg(not(target_arch = "wasm32"))]
const DIAGNOSTICS_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

/// The LSP server. Owns the shared services, the tokio runtime handle, the
/// outbound writer channel, and the shared mutable state.
#[cfg(not(target_arch = "wasm32"))]
pub struct LspServer {
    services: Services,
    rt: Handle,
    shared: Arc<Mutex<Shared>>,
    /// Outbound message channel. Cloned into each spawned task. Dropped at the
    /// end of [`run`](Self::run) so the writer thread sees EOF and exits.
    out_tx: Option<Sender<Message>>,
    /// The writer thread that owns the sink and serializes all outbound writes.
    writer_handle: Option<JoinHandle<io::Result<()>>>,
    /// In-flight async task handles, joined at the end of `run` so all replies
    /// are flushed before the writer channel closes (also makes tests
    /// deterministic).
    pending: Vec<tokio::task::JoinHandle<()>>,
    initialized: bool,
    shutting_down: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl LspServer {
    /// Construct with the shared services, a tokio runtime handle, and a writer
    /// for outbound messages (stdout in production, an in-memory sink in tests).
    ///
    /// The writer is moved onto a dedicated thread that drains the outbound
    /// channel; it must therefore be `Send + 'static`.
    pub fn new<W: io::Write + Send + 'static>(services: Services, rt: Handle, writer: W) -> Self {
        let (out_tx, out_rx) = std::sync::mpsc::channel::<Message>();
        let writer_handle = std::thread::spawn(move || -> io::Result<()> {
            let mut w = writer;
            while let Ok(msg) = out_rx.recv() {
                jsonrpc::write_message(&mut w, &msg)?;
            }
            w.flush()
        });
        Self {
            services,
            rt,
            shared: Arc::new(Mutex::new(Shared::new())),
            out_tx: Some(out_tx),
            writer_handle: Some(writer_handle),
            pending: Vec::new(),
            initialized: false,
            shutting_down: false,
        }
    }

    /// Drive the message loop until clean EOF or an `exit` notification. Slow
    /// async work is dispatched to the runtime and does not block this loop. On
    /// return, all in-flight tasks have been awaited and the writer thread has
    /// flushed and exited.
    pub fn run(&mut self, reader: &mut impl io::BufRead) -> io::Result<()> {
        while let Some(msg) = jsonrpc::read_message(reader)? {
            if self.handle(msg) {
                break; // `exit` notification
            }
        }
        // Wait for in-flight async tasks so their replies reach the channel.
        for handle in std::mem::take(&mut self.pending) {
            let _ = self.rt.block_on(handle);
        }
        // Drop our sender so the writer thread sees the channel close and exits.
        self.out_tx.take();
        if let Some(handle) = self.writer_handle.take() {
            match handle.join() {
                Ok(res) => res?,
                Err(_) => {
                    return Err(io::Error::other("lsp writer thread panicked"));
                }
            }
        }
        Ok(())
    }

    /// Queue a single outbound message for the writer thread.
    fn send(&self, msg: Message) {
        if let Some(tx) = &self.out_tx {
            let _ = tx.send(msg);
        }
    }

    fn send_all(&self, msgs: Vec<Message>) {
        for msg in msgs {
            self.send(msg);
        }
    }

    /// Clone of the outbound sender for a spawned task.
    fn out(&self) -> Sender<Message> {
        self.out_tx
            .as_ref()
            .expect("outbound channel open during run")
            .clone()
    }

    /// Dispatch a single inbound message. Returns `true` when the loop should
    /// terminate (an `exit` notification).
    fn handle(&mut self, msg: Message) -> bool {
        if msg.is_notification() {
            return self.handle_notification(&msg);
        }
        if msg.is_response() {
            // We never send requests, so responses are unexpected. Drop them.
            return false;
        }
        self.handle_request(msg);
        false
    }

    /// Returns `true` on an `exit` notification.
    fn handle_notification(&mut self, msg: &Message) -> bool {
        let Some(method) = &msg.method else {
            return false;
        };
        match method.as_str() {
            "initialized" => {
                self.initialized = true;
            }
            "exit" => return true,
            "textDocument/didOpen" => {
                if let Some(params) = parse_notification::<DidOpenTextDocumentParams>(msg) {
                    let uri = params.text_document.uri.clone();
                    {
                        let mut g = self.shared.lock().unwrap();
                        g.docs.insert(
                            uri.clone(),
                            DocumentState {
                                text: params.text_document.text,
                                version: Some(params.text_document.version),
                                generation: 0,
                            },
                        );
                    }
                    self.spawn_publish_diagnostics(uri);
                }
            }
            "textDocument/didChange" => {
                if let Some(params) = parse_notification::<DidChangeTextDocumentParams>(msg) {
                    let uri = params.text_document.uri.clone();
                    let mut changed = false;
                    {
                        let mut g = self.shared.lock().unwrap();
                        let (held_text, held_version, generation) = match g.docs.get(&uri) {
                            Some(doc) => (doc.text.clone(), doc.version, doc.generation + 1),
                            None => (String::new(), None, 0),
                        };
                        if let Some(text) = apply_did_change(
                            held_text,
                            held_version,
                            params.text_document.version,
                            &params.content_changes,
                        ) {
                            g.docs.insert(
                                uri.clone(),
                                DocumentState {
                                    text,
                                    version: Some(params.text_document.version),
                                    generation,
                                },
                            );
                            changed = true;
                        }
                    }
                    if changed {
                        self.spawn_publish_diagnostics(uri);
                    }
                }
            }
            "textDocument/didClose" => {
                if let Some(params) = parse_notification::<DidCloseTextDocumentParams>(msg) {
                    let uri = params.text_document.uri;
                    {
                        let mut g = self.shared.lock().unwrap();
                        g.docs.remove(&uri);
                    }
                    // Clear the closed document's diagnostics so the client's
                    // problems panel does not keep stale squiggles alive.
                    self.send(Message::notification(
                        "textDocument/publishDiagnostics",
                        serde_json::json!({ "uri": uri, "diagnostics": [] }),
                    ));
                }
            }
            "textDocument/didSave" => {
                if let Some(params) = parse_notification::<DidSaveTextDocumentParams>(msg) {
                    self.spawn_publish_diagnostics(params.text_document.uri);
                }
            }
            "$/cancelRequest" => {
                if let Some(params) = parse_notification::<cancel::CancelParams>(msg) {
                    let mut g = self.shared.lock().unwrap();
                    if g.cancelled.len() >= 1024 {
                        g.cancelled.clear();
                    }
                    g.cancelled.insert(cancel::id_key(&params.id));
                }
            }
            _ => {}
        }
        false
    }

    fn handle_request(&mut self, msg: Message) {
        let id = msg.id.clone().unwrap_or(Id::Int(0));
        let Some(method) = msg.method.clone() else {
            return;
        };
        match method.as_str() {
            // --- pure-CPU, inline ---
            "initialize" => self.send_all(self.handle_initialize(msg, id)),
            "shutdown" => {
                self.shutting_down = true;
                self.send(Message::response(id, serde_json::Value::Null));
            }
            "textDocument/completion" => self.send_all(self.handle_completion(msg, id)),
            "textDocument/hover" => self.send_all(self.handle_hover(msg, id)),
            "textDocument/definition" => self.send_all(self.handle_definition(msg, id)),
            "textDocument/references" => self.send_all(self.handle_references(msg, id)),
            "textDocument/semanticTokens/full" => {
                self.send_all(self.handle_semantic_tokens(msg, id))
            }
            "textDocument/documentSymbol" => self.send_all(self.handle_document_symbol(msg, id)),
            "textDocument/formatting" => self.send_all(self.handle_formatting(msg, id)),
            // --- I/O-bound, spawned (read loop continues) ---
            "textDocument/diagnostic" => self.spawn_diagnostic(msg, id),
            "workspace/symbol" => self.spawn_workspace_symbol(msg, id),
            "sqlly/setConnection" => self.spawn_set_connection(msg, id),
            "sqlly/refreshCatalog" => self.spawn_refresh_catalog(id),
            _ => self.send(Message::error(
                id,
                error_code::METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            )),
        }
    }
}

/// Parse request params, mapping failures to a JSON-RPC `INVALID_PARAMS` error
/// carrying the request id.
#[allow(clippy::result_large_err)]
fn parse<T: serde::de::DeserializeOwned>(msg: &Message) -> Result<T, (Id, RpcError)> {
    let id = msg.id.clone().unwrap_or(Id::Int(0));
    let params = msg.params.clone().unwrap_or(serde_json::Value::Null);
    serde_json::from_value::<T>(params).map_err(|e| {
        (
            id,
            RpcError {
                code: error_code::INVALID_PARAMS,
                message: format!("invalid params: {e}"),
                data: None,
            },
        )
    })
}

/// Parse notification params; notifications carry no id and expect no error
/// reply, so a parse failure is simply dropped.
fn parse_notification<T: serde::de::DeserializeOwned>(msg: &Message) -> Option<T> {
    serde_json::from_value::<T>(msg.params.clone().unwrap_or(serde_json::Value::Null)).ok()
}
