//! Every database backend SQLLY talks to, behind one set of traits.
//!
//! `driver` holds the traits and shared connection plumbing; `tds`, `pg`,
//! `mysql`, `sqlite`, `libsql`, `duckdb`, `clickhouse`, `oracle`, and `redis`
//! implement them per engine; `ssh` is the tunnelling (SSH, SOCKS, HTTP
//! proxy) the network backends dial through.
//!
//! Native-only. The browser build swaps in `sqlly-services`' own wasm
//! executors instead of depending on this crate at all.

pub mod clickhouse;
pub mod connect_hint;
pub mod driver;
pub mod duckdb;
pub mod libsql;
pub mod mysql;
pub mod oracle;
pub mod pg;
pub mod redis;
pub(crate) mod sql_text;
pub mod sqlite;
pub mod ssh;
pub mod tcp_keepalive;
pub mod tds;
