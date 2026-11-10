//! Types shared by the `sqlly-cli mcp` server and the app's Settings ▸ AI ▸ MCP
//! pane: the server policy file, the read-only view of connections.json the
//! server needs, result windowing, and write/open proposals the app consumes.

pub mod audit;
pub mod policy;
pub mod proposals;
pub mod rate_limit;
pub mod store_view;
pub mod windowing;

#[cfg(not(target_arch = "wasm32"))]
pub mod host;
