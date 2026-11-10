//! `sqlly-cli mcp`: stdio MCP server. Headless peer of `engine` (ADR-018 tier
//! two) that calls in-process `sqlly_services::Services` directly.

mod server;
mod tools;

pub use server::run;
