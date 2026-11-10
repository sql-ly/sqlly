//! `sqlly-lsp` — Language Server Protocol server for T-SQL, backed by the
//! in-process [`sqlly_services::Services`] layer.
//!
//! The server consumes the same shared services the GPUI app uses — there is
//! no `sqlly-cli` subprocess and no binary-interchange protocol. The LSP wire
//! edge is JSON-RPC 2.0 over stdio only; everything inside the server works in
//! native Rust types.
//!
//! Run: `sqlly-lsp` reads JSON-RPC messages from stdin and writes responses +
//! `publishDiagnostics` notifications to stdout.

#![forbid(unsafe_code)]

pub mod jsonrpc;
pub mod server;
pub mod types;

// The stdio LSP server itself is native-only (tokio runtime); the pure
// helpers it shares with the GPUI debug screen stay portable.
#[cfg(not(target_arch = "wasm32"))]
pub use server::LspServer;
pub use server::{
    build_hover, completion_kind_to_lsp, completion_to_lsp, compute_diagnostics, initialize_result,
    offset_to_position, position_to_offset, semantic_token_legend, semantic_tokens,
    server_capabilities, tokenize_semantic,
};
pub use types::*;
