#![forbid(unsafe_code)]

//! SQL Server (TDS) driver built on `tiberius`.
//!
//! Module map:
//! - [`transport`] — the [`SqlServerTransport`] contract, the concrete
//!   [`TiberiusTransport`], and the event-publishing [`ConnectionService`].
//! - [`client`] — opening a warm [`TdsClient`] from a profile (config, auth,
//!   Azure gateway login routing) and server-side cancellation.
//! - [`execute`] — the streaming execution engine ([`execute_on_client`]) plus
//!   the parameterized helpers ([`query_rows_on_client`], …).
//! - [`convert`] — tiberius column/type → [`sqlly_core::CellValue`] mapping.
//! - [`sql_text`] — cheap SQL-text classification gates (`USE`, transactions).
//! - [`error`] — error types and tiberius error mapping/formatting.
//! - [`catalog`] — schema/catalog queries for the explorer tree.

pub mod catalog;
mod client;
mod convert;
pub mod error;
mod execute;
pub mod result_set;
pub mod transport;

pub use client::{TdsCancelHandle, TdsClient, cancel_handle, open_client};
pub use execute::{
    ExecuteOutcome, execute_batch_on_client, execute_on_client, execute_on_client_keep_transaction,
    execute_params_on_client, query_rows_on_client,
};
pub use transport::{ConnectionService, SqlServerTransport, TiberiusTransport};

// `SqlParam` and `MaterializedResultSet` moved to `sqlly-core` so the pure SQL
// builders in `sqlly-services::row_editing` compile on wasm; the re-export
// keeps every existing `crate::tds::SqlParam` path working.
pub use sqlly_core::{MaterializedResultSet, SqlParam};

// Crate-internal shorthands so sibling modules (e.g. `catalog`) can keep using
// `crate::tds::connect` / `crate::tds::column_data_to_cell` paths.
pub(crate) use client::connect;
pub(crate) use convert::column_data_to_cell;
