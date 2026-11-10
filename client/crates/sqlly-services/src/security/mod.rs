#![forbid(unsafe_code)]
use async_trait::async_trait;
use sqlly_core::{SecretRef, StudioResult};

pub mod auth;
pub mod authorize_url;
pub mod azure;
pub mod azure_discovery;
pub mod error;
pub mod permissions;
pub mod provider;
pub mod secret;
pub mod token_cache;
pub mod token_flow;

// The interactive sign-in orchestration cannot exist in a browser tab: it
// binds a loopback TCP listener for the OAuth redirect and drives it with a
// tokio runtime. Everything else — token models/caches, the transport-generic
// Entra client, PKCE/authorize-URL building, discovery models — stays
// portable so the app's Azure surface type-checks on wasm; its sign-in UI
// reports that Azure authentication requires the desktop app.
#[cfg(not(target_arch = "wasm32"))]
pub mod browser_signin;
#[cfg(not(target_arch = "wasm32"))]
pub mod loopback_redirect;

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn get_secret(&self, secret_ref: &SecretRef) -> StudioResult<String>;
    async fn put_secret(&self, secret_ref: &SecretRef, secret: &str) -> StudioResult<()>;
}
