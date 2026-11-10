//! `sqlly-remote-services`: remote-backed implementations of the `sqlly-services`
//! ports for connections routed **Via Relay**. Engine-backed operations
//! (`QueryExecutor`, `CatalogService`, and the metadata/connection queries that
//! can be composed over them) are driven through a `sqlly-remote` initiator
//! session + `sqlly-engine-protocol::EngineClient` to the on-prem `sqlly-cli
//! engine`. Purely client-side ports (`IntellisenseService`,
//! `AdminToolsService`) delegate to the in-process defaults, fed by remotely
//! synced catalog data.
//!
//! This crate lets the GPUI app (Phase 3/4) swap in a remote service set when a
//! `ConnectionProfile`'s route is `ViaRelay`, so every database feature that
//! works in-process also works through the relay — without the UI knowing which
//! transport is underneath.
//!
//! # Port coverage
//!
//! | Port | Strategy |
//! | --- | --- |
//! | [`QueryExecutor`] | REMOTE — `execute*` → `REQUEST_TAG_EXECUTE_QUERY`, `cancel` → `REQUEST_TAG_CANCEL`. |
//! | [`CatalogService`] | REMOTE — all nine methods map 1:1 to engine requests. |
//! | [`MetadataService`] | REMOTE (composed) — reuses `DefaultMetadataService` over the remote query executor. |
//! | [`ConnectionManager`] | REMOTE (composed) — `test_connection` / `list_databases` over the remote query path. |
//! | [`IntellisenseService`] | DELEGATE — in-process `DefaultIntellisense` over a synced catalog snapshot. |
//! | [`AdminToolsService`] | DELEGATE — in-process `DefaultAdminToolsService` (pure SQL-string generation). |
//! | [`RowEditingService`](crate::row_editing::RowEditingService) | REMOTE — every op → `REQUEST_TAG_ROW_EDIT`, executed by the bridge-side engine's `DefaultRowEditingService` with the same guards as direct editing. |
//!
//! Construct a bundle with [`RemoteServices::from_transport`] (a `sqlly-remote`
//! `PeerSession`, or a `sqlly-cli engine` stdio transport in tests) or
//! [`RemoteServices::from_engine`], then read [`RemoteServices::services`] for a
//! `crate::Services` the app can hold exactly like the in-process one.

#![forbid(unsafe_code)]

mod catalog;
mod connection;
mod engine;
mod query;
mod row_editing;
pub(crate) mod rt;
mod unavailable;

use std::sync::Arc;

use crate::{
    DefaultAdminToolsService, DefaultIntellisense, DefaultMetadataService, Services,
    catalog::TenantScopeResolver,
};
use sqlly_remote::engine_protocol::{EngineClient, FramedTransport};

pub use crate::remote::catalog::RemoteCatalogService;
pub use crate::remote::connection::RemoteConnectionManager;
pub use crate::remote::engine::RemoteEngine;
pub use crate::remote::query::RemoteQueryExecutor;
pub use crate::remote::row_editing::{
    RemoteRowEditingService, UnavailableRowEditingService, bridge_too_old_for_batch_save_message,
    bridge_too_old_for_server_expressions_message,
};
pub use crate::remote::unavailable::unavailable_services;

/// Native relay-session errors have typed codes and localized guidance. The
/// browser uses a relay-proxy socket instead, where these transport codes do
/// not exist, so no raw-English compatibility stub is exposed on wasm.
#[cfg(not(target_arch = "wasm32"))]
pub fn relay_display_message(code: sqlly_wire::RelayErrorCode) -> sqlly_core::DisplayMessage {
    let key = match code {
        sqlly_wire::RelayErrorCode::DestOffline => "services.remote.relay.dest_offline",
        sqlly_wire::RelayErrorCode::RoutingDenied => "services.remote.relay.routing_denied",
        sqlly_wire::RelayErrorCode::AuthFailed => "services.remote.relay.auth_failed",
        sqlly_wire::RelayErrorCode::RelayFull => "services.remote.relay.full",
        sqlly_wire::RelayErrorCode::SessionLimit => "services.remote.relay.session_limit",
        sqlly_wire::RelayErrorCode::RateLimited => "services.remote.relay.rate_limited",
        sqlly_wire::RelayErrorCode::ProtocolError => "services.remote.relay.protocol_error",
    };
    sqlly_core::DisplayMessage::new(
        key,
        sqlly_remote::transport::RemoteError::relay_user_message(code),
    )
}

/// A remote-backed mirror of [`crate::Services`]. Holds the shared
/// [`RemoteEngine`] plus a ready-to-use [`Services`] facade whose ports are the
/// remote/delegated implementations.
#[derive(Clone)]
pub struct RemoteServices<T> {
    engine: RemoteEngine<T>,
    services: Services,
}

impl<T: FramedTransport + 'static> RemoteServices<T> {
    /// Build a bundle from a connected [`FramedTransport`] (production: a
    /// `sqlly-remote` `PeerSession`; tests: a `sqlly-cli engine` stdio duplex).
    pub fn from_transport(transport: T) -> Self {
        Self::from_engine(RemoteEngine::new(transport))
    }

    /// Build a bundle from an already-constructed [`EngineClient`].
    pub fn from_engine_client(client: EngineClient<T>) -> Self {
        Self::from_engine(RemoteEngine::from_client(client))
    }

    /// Build a bundle from a shared [`RemoteEngine`] handle.
    pub fn from_engine(engine: RemoteEngine<T>) -> Self {
        Self::from_engine_with_tenant(engine, None)
    }

    /// Like [`from_engine`](Self::from_engine), but threads tenant-scope policy
    /// into catalog validate/generate/review requests.
    pub fn from_engine_with_tenant(
        engine: RemoteEngine<T>,
        tenant_scope: Option<TenantScopeResolver>,
    ) -> Self {
        let mut catalog_service = RemoteCatalogService::new(engine.clone());
        if let Some(resolver) = tenant_scope {
            catalog_service = catalog_service.with_tenant_scope_resolver(resolver);
        }
        let queries: Arc<dyn crate::QueryExecutor> =
            Arc::new(RemoteQueryExecutor::new(engine.clone()));
        let catalog: Arc<dyn crate::catalog::CatalogService> = Arc::new(catalog_service);
        // Metadata composes over the SAME remote query executor: it runs the
        // `sqlly_intel::metadata::sql_for` SQL through the engine and parses the
        // native rows, identical to the in-process default but tunneled.
        let metadata: Arc<dyn crate::MetadataService> =
            Arc::new(DefaultMetadataService::new(Arc::clone(&queries)));
        let connections: Arc<dyn crate::ConnectionManager> =
            Arc::new(RemoteConnectionManager::new(Arc::clone(&queries)));

        let services = Services::from_parts(
            connections,
            // Intellisense and admin tools are client-side: synchronous
            // completion over a catalog snapshot / pure SQL-string generation.
            // They delegate to the in-process defaults, fed remotely-synced
            // catalog data via the remote CatalogService.
            Arc::new(DefaultIntellisense),
            queries,
            catalog,
            metadata,
            Arc::new(DefaultAdminToolsService),
            Arc::new(RemoteRowEditingService::new(engine.clone())),
        );

        Self { engine, services }
    }

    /// The shared engine handle (for an engine-liveness `ping`, or to build
    /// further ports over the same session).
    pub fn engine(&self) -> &RemoteEngine<T> {
        &self.engine
    }

    /// The assembled [`Services`] facade — a drop-in for the in-process one.
    pub fn services(&self) -> &Services {
        &self.services
    }

    /// Consume the bundle and return the [`Services`] facade.
    pub fn into_services(self) -> Services {
        self.services
    }

    /// Engine-liveness probe (`ping`/`pong`); does not touch the database.
    pub async fn ping(&self) -> sqlly_core::StudioResult<()> {
        self.engine.ping().await
    }
}
