//! Via-Relay session establishment for the GPUI app.
//!
//! Turns a `ViaRelay` [`ConnectionProfile`] plus the store's relay data (the
//! public endpoint list, the chosen relay-device pair, the peer bridge's pinned
//! Noise public key, and an opaque relay bearer token) and the app's own local
//! Noise identity into a live, remote-backed [`sqlly_services::Services`] — a
//! drop-in for the in-process one that the rest of the app already holds.
//!
//! The flow mirrors the on-prem bridge's `run::connect_and_serve`, but as the
//! **initiator**: select a relay endpoint, `connect_tls`, `authenticate`,
//! [`PeerSession::establish`] as [`Role::Initiator`], then assemble a
//! [`RemoteServices`] over the resulting transport and hand back its
//! [`Services`] facade.
//!
//! # Testability
//!
//! A fully live round-trip needs a real account + relay + bridge, which cannot
//! run in the unit-test harness. So the deterministic pieces are separated out
//! and unit-tested here **without** any live infrastructure:
//!
//! * [`select_relay`] — pure endpoint-selection policy (reused shape from the
//!   bridge's `discovery`),
//! * [`assemble_services_from_engine`] — builds the remote [`Services`] from *any*
//!   `FramedTransport` (a `PeerSession` in production, a fake in tests),
//! * [`user_facing_error`] — maps relay/session failures to clear UI copy.
//!
//! Only [`establish_services`] itself touches the network; it composes the
//! tested pieces and is exercised end-to-end against live relay infrastructure.

#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::ConnectionProfile;
use sqlly_core::connection::ConnectionRoute;
use sqlly_remote::engine_protocol::FramedTransport;
use sqlly_remote::noise::{KEY_LEN, SecretStore, StaticKeypair};
use sqlly_remote::transport::{
    FramedStream, PeerSession, RemoteError, Role, SessionConfig, TlsTarget, authenticate,
    connect_tls,
};
use sqlly_services::Services;
use sqlly_services::remote::{RemoteEngine, RemoteServices};
use sqlly_wire::{RelayErrorCode, device_uuid};
use uuid::Uuid;

/// The keyring scope under which THIS app device's Noise static identity is
/// persisted. Distinct from the on-prem bridge's `"bridge"` scope so the two
/// never collide on a shared machine.
pub const APP_NOISE_SCOPE: &str = "app";

/// Load (or first-run generate + persist) this app device's Noise static
/// keypair from the OS keyring, scoped by the control-plane app device id.
///
/// The private key never leaves the keyring-backed [`StaticKeypair`]; only the
/// [`public_key`](StaticKeypair::public_key) is ever registered with the store
/// (see [`crate::store::register_app_noise_key`]).
#[cfg(not(target_arch = "wasm32"))]
pub fn app_noise_keypair(
    app_device_id: &str,
) -> Result<StaticKeypair, sqlly_remote::noise::NoiseError> {
    app_noise_keypair_with(&sqlly_remote::noise::OsKeyringStore, app_device_id)
}

/// Like [`app_noise_keypair`] but against an injected [`SecretStore`], so tests
/// (and any headless build) can supply an [`sqlly_remote::noise::InMemorySecretStore`]
/// instead of touching the real OS keyring.
pub fn app_noise_keypair_with<S: SecretStore + ?Sized>(
    store: &S,
    app_device_id: &str,
) -> Result<StaticKeypair, sqlly_remote::noise::NoiseError> {
    StaticKeypair::load_or_generate_with(store, APP_NOISE_SCOPE, app_device_id)
}

/// Assemble the [`RelaySessionInputs`] for a `ViaRelay` profile from the store's
/// relay data plus this app device's control-plane id.
///
/// This is the single place control-plane device-id *strings* become wire
/// [`Uuid`]s, and it uses [`device_uuid`] for **every** conversion so the app,
/// the bridge, and the relay all agree:
///
/// * `local_id` = `device_uuid(app_device_id)` — the app's authenticated source,
/// * `peer_id`  = `device_uuid(connector_device_id)` — the pinned bridge peer.
///
/// The relay's own wire id is per-endpoint ([`RelayEndpoint::relay_id`], from
/// the heartbeat-fed directory) and is consumed at selection time — endpoints
/// that have not reported one cannot be authenticated to and are skipped.
///
/// Pure and secret-free apart from carrying the opaque `relay_token` through.
#[must_use]
pub fn build_relay_inputs(
    app_device_id: &str,
    connector_device_id: &str,
    connector_noise_public_key: [u8; KEY_LEN],
    endpoints: Vec<RelayEndpoint>,
    relay_token: String,
    relay_port: u16,
) -> RelaySessionInputs {
    RelaySessionInputs {
        endpoints,
        local_id: device_uuid(app_device_id),
        peer_id: device_uuid(connector_device_id),
        peer_static_key: connector_noise_public_key,
        token: relay_token,
        relay_port,
    }
}

/// A public relay endpoint from `GET /remote-endpoint-list`.
///
/// Mirrors the shape the bridge decodes (`RelayEndpointInfo` in
/// the store's relay API models). Kept as a plain local struct so
/// endpoint selection has no dependency on the bridge binary crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayEndpoint {
    /// The relay server's wire identity — the frame `destination` the app
    /// authenticates to. `None` when the control plane has not yet learned it
    /// from the relay's heartbeat; such endpoints cannot carry an Auth frame
    /// and are skipped by selection (same rule as the bridge's discovery).
    pub relay_id: Option<Uuid>,
    /// DNS host name (SNI + certificate verification + dial target).
    pub hostname: String,
    /// Human-readable location label (advisory).
    pub location: String,
    /// Status label: `online` / `degraded` / `draining` / `offline` / `full`.
    pub status: String,
    /// Load percentage; `100` means at capacity.
    pub capacity_pct: i32,
}

/// The relay-server statuses advertised by the control plane. Selection accepts
/// only [`RelayStatus::Online`]; every other status (including an unknown
/// label) is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayStatus {
    Online,
    Degraded,
    Draining,
    Offline,
    Full,
    /// An unrecognized status label — treated as ineligible.
    Unknown,
}

impl RelayStatus {
    /// Parse the API's status label (case-insensitive).
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "online" => Self::Online,
            "degraded" => Self::Degraded,
            "draining" => Self::Draining,
            "offline" => Self::Offline,
            "full" => Self::Full,
            _ => Self::Unknown,
        }
    }
}

/// Whether an endpoint may be connected to: online, below the hard full mark,
/// and carrying the relay's wire id (without one the Auth frame cannot be
/// addressed, so the endpoint is unusable no matter its status).
fn is_eligible(ep: &RelayEndpoint) -> bool {
    ep.relay_id.is_some()
        && RelayStatus::parse(&ep.status) == RelayStatus::Online
        && ep.capacity_pct < 100
}

/// Select a relay endpoint, preferring `hint` when it is eligible.
///
/// Pure and deterministic (the same policy the bridge uses):
/// 1. only `online`, non-full endpoints are eligible;
/// 2. an eligible `hint` hostname wins;
/// 3. otherwise the lowest `capacity_pct`, ties broken by hostname ascending.
///
/// Returns `None` when nothing is eligible.
#[must_use]
pub fn select_relay<'a>(
    endpoints: &'a [RelayEndpoint],
    hint: Option<&str>,
) -> Option<&'a RelayEndpoint> {
    if let Some(hint) = hint
        && let Some(ep) = endpoints
            .iter()
            .find(|ep| ep.hostname.eq_ignore_ascii_case(hint) && is_eligible(ep))
    {
        return Some(ep);
    }

    endpoints
        .iter()
        .filter(|ep| is_eligible(ep))
        .min_by(|a, b| {
            a.capacity_pct
                .cmp(&b.capacity_pct)
                .then_with(|| a.hostname.cmp(&b.hostname))
        })
}

/// Everything the store must hand over to open a Via-Relay session, on top of
/// the profile itself and the app's local Noise keypair.
///
/// Secret-free except for [`token`](Self::token): the store issues the relay
/// bearer token, this layer treats it as opaque, and it is never logged.
#[derive(Clone)]
pub struct RelaySessionInputs {
    /// Public relay endpoints from `GET /remote-endpoint-list`. Each carries
    /// its own relay wire id (the Auth-frame `destination`); selection only
    /// ever yields an endpoint that has one.
    pub endpoints: Vec<RelayEndpoint>,
    /// This app device's authenticated id (the relay frame `source`).
    pub local_id: Uuid,
    /// The destination relay-connector (bridge) device id — the Noise peer we
    /// pin and hand-shake with through the relay.
    pub peer_id: Uuid,
    /// The bridge's pinned Noise static public key (from the account's
    /// public-key directory).
    pub peer_static_key: [u8; KEY_LEN],
    /// Opaque relay bearer token. **Secret** — never logged.
    pub token: String,
    /// TLS port to dial the selected relay endpoint on.
    pub relay_port: u16,
}

impl std::fmt::Debug for RelaySessionInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelaySessionInputs")
            .field("endpoints", &self.endpoints)
            .field("local_id", &self.local_id)
            .field("peer_id", &self.peer_id)
            .field("token", &"<redacted>")
            .field("relay_port", &self.relay_port)
            .finish()
    }
}

/// A failure while opening or using a Via-Relay session. [`user_facing_error`]
/// turns one into UI copy.
#[derive(Debug, thiserror::Error)]
pub enum RemoteSessionError {
    /// The profile's route is not `ViaRelay`, so there is nothing to establish.
    #[error("connection is not routed via a relay")]
    NotViaRelay,
    /// No relay endpoint was online and below capacity.
    #[error("no relay server is currently available")]
    NoRelayAvailable,
    /// A transport/auth/handshake failure from the remote session layer.
    #[error(transparent)]
    Remote(#[from] RemoteError),
}

/// The advisory relay-endpoint hint carried on a `ViaRelay` profile, if any.
///
/// The hint may be a bare host or `host:port`; only the host portion is used
/// for endpoint matching (the port comes from [`RelaySessionInputs::relay_port`]).
#[must_use]
pub fn profile_relay_hint(profile: &ConnectionProfile) -> Option<&str> {
    match &profile.route {
        ConnectionRoute::ViaRelay { relay_hint, .. } => relay_hint
            .as_deref()
            .map(|h| h.split(':').next().unwrap_or(h)),
        ConnectionRoute::Direct => None,
    }
}

/// How long session establishment waits for the bridge engine's `Ready`
/// frame (which carries its advertised capabilities) before handing the
/// session to the app. The bridge spawns its engine only once the Noise
/// handshake completes, so this covers engine start-up. On timeout the
/// session is still used; capability-gated features stay off until `Ready`
/// arrives.
pub const READY_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Assemble a remote-backed [`Services`] over an established engine handle
/// (the caller holds it first so it can wait for `Ready`), also returning the
/// session-closed watch so the host can observe connection loss the moment
/// the transport dies (flips to `true` when the underlying demux ends)
/// instead of waiting for a failed request.
///
/// Production wraps a [`PeerSession`]; tests wrap any [`FramedTransport`].
/// The wiring of the individual remote ports lives in `sqlly-services::remote`
/// and is covered by that crate's tests — this is the single seam the app
/// uses to get a `Services` from a transport.
#[must_use]
pub fn assemble_services_from_engine<T: FramedTransport + 'static>(
    engine: RemoteEngine<T>,
) -> (Services, tokio::sync::watch::Receiver<bool>) {
    let closed = engine.closed_signal();
    let services = RemoteServices::from_engine_with_tenant(
        engine,
        Some(std::sync::Arc::new(|profile| {
            crate::tenant_scoping::policy_for(profile)
        })),
    )
    .into_services();
    (services, closed)
}

/// Open a live Via-Relay session for `profile` and return a remote-backed
/// [`Services`] plus the hostname of the relay endpoint the session actually
/// landed on (selection may override the profile's advisory hint, so only
/// this layer knows the real hop — the status bar's route tooltip shows it).
///
/// Steps: select an endpoint (preferring the profile's advisory hint), open
/// TLS, authenticate to the relay, run the Noise XX handshake with the pinned
/// bridge peer as [`Role::Initiator`], then [`assemble_services_from_engine`] over the
/// session.
///
/// Requires a real relay + bridge; unit tests exercise the deterministic pieces
/// ([`select_relay`], [`assemble_services_from_engine`], [`user_facing_error`]) directly.
pub async fn establish_services(
    profile: &ConnectionProfile,
    inputs: &RelaySessionInputs,
    local_key: &StaticKeypair,
) -> Result<(Services, String, tokio::sync::watch::Receiver<bool>), RemoteSessionError> {
    if !matches!(profile.route, ConnectionRoute::ViaRelay { .. }) {
        return Err(RemoteSessionError::NotViaRelay);
    }

    let endpoint = select_relay(&inputs.endpoints, profile_relay_hint(profile))
        .ok_or(RemoteSessionError::NoRelayAvailable)?;
    // Selection only yields endpoints carrying a relay wire id; this guard
    // keeps the invariant local instead of trusting it at a distance.
    let relay_id = endpoint
        .relay_id
        .ok_or(RemoteSessionError::NoRelayAvailable)?;

    let relay_hostname = endpoint.hostname.clone();
    let target = TlsTarget::new(endpoint.hostname.clone(), inputs.relay_port);
    let stream = connect_tls(&target).await?;

    // Authenticate over a framed view, then hand that framed stream (and any
    // bytes it buffered past the Auth ack) to the session
    // — the same ordering the bridge uses on the responder side. The Auth
    // frame is addressed to the selected endpoint's own wire identity.
    let mut framed = FramedStream::new(stream);
    authenticate(
        &mut framed,
        inputs.local_id,
        relay_id,
        inputs.token.as_bytes(),
    )
    .await?;

    let session = PeerSession::establish_framed(
        Role::Initiator,
        framed,
        local_key,
        &inputs.peer_static_key,
        inputs.local_id,
        inputs.peer_id,
        SessionConfig::default(),
    )
    .await?;

    // Wait (bounded) for the bridge engine's Ready so its advertised
    // capabilities are known before any UI asks whether, e.g., the
    // server-evaluated row-edit generators may be offered.
    let engine = RemoteEngine::new(session);
    if engine.wait_for_capabilities(READY_WAIT).await.is_none() {
        tracing::warn!(
            "relay bridge engine did not announce itself within {}s; capability-gated features stay off until it does",
            READY_WAIT.as_secs()
        );
    }
    let (services, closed) = assemble_services_from_engine(engine);
    Ok((services, relay_hostname, closed))
}

/// The default relay TLS port (matches the relay server's default bind and the
/// bridge's `relay_port` default). Overridden per-profile only when the advisory
/// relay hint carries an explicit `host:port`.
pub const DEFAULT_RELAY_PORT: u16 = 8443;

/// The relay TLS port to dial for `profile`: the port from a `host:port` relay
/// hint when present, otherwise [`DEFAULT_RELAY_PORT`].
#[must_use]
pub fn profile_relay_port(profile: &ConnectionProfile) -> u16 {
    match &profile.route {
        ConnectionRoute::ViaRelay {
            relay_hint: Some(hint),
            ..
        } => hint
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            .unwrap_or(DEFAULT_RELAY_PORT),
        _ => DEFAULT_RELAY_PORT,
    }
}

/// Convert a store-layer relay endpoint into the session-layer one used for
/// selection. Same fields, kept as separate types so endpoint selection has no
/// dependency on the store client.
fn to_endpoint(ep: crate::store::RelayEndpoint) -> RelayEndpoint {
    RelayEndpoint {
        relay_id: ep.relay_id,
        hostname: ep.hostname,
        location: ep.location,
        status: ep.status,
        capacity_pct: ep.capacity_pct,
    }
}

/// A successfully opened Via-Relay session: the remote-backed services plus
/// the route facts the status bar's indicator shows — which relay endpoint the
/// session landed on and which bridge answered.
pub struct OpenedRelaySession {
    pub services: Services,
    /// Hostname of the relay endpoint the session was established through.
    pub relay_hostname: String,
    /// Display name of the bridge (relay connector): the owner's name
    /// override, else the machine hostname, else the raw device id.
    pub bridge_name: String,
    /// Session-closed watch: flips to `true` the moment the relay transport
    /// dies, so the host can mark the server unreachable without waiting for
    /// the next failed request.
    pub closed: tokio::sync::watch::Receiver<bool>,
}

/// Best-effort human label for the bridge (relay connector) `device_id`: the
/// account's own connector list first, then bridges shared with this account,
/// falling back to the raw device id when neither lookup can name it. Purely
/// cosmetic (the status bar's route tooltip), so lookup failures never fail
/// the session.
#[cfg(not(target_arch = "wasm32"))]
async fn resolve_bridge_display_name(access_token: &str, device_id: &str) -> String {
    if let Ok(devices) = crate::store::list_relay_devices(access_token).await
        && let Some(device) = devices.iter().find(|d| d.device_id == device_id)
    {
        return device.display_name();
    }
    if let Ok(shared) = crate::store::list_shared_relay_devices(access_token).await
        && let Some(device) = shared.iter().find(|d| d.device_id == device_id)
        && !device.name.trim().is_empty()
    {
        return device.name.trim().to_string();
    }
    device_id.to_string()
}

/// End-to-end open of a Via-Relay session for `profile`, orchestrating every
/// store call the transport needs and returning a remote-backed [`Services`].
///
/// Steps (all network, so unit tests exercise the deterministic sub-pieces —
/// [`build_relay_inputs`], [`select_relay`], [`app_noise_keypair_with`] — not
/// this):
/// 1. register this app device's *public* Noise key so the bridge can pin us;
/// 2. fetch the public relay endpoint directory;
/// 3. mint a sqlly-relay token for this app device;
/// 4. resolve the profile's authorized pair and the connector's pinned key;
/// 5. [`build_relay_inputs`] (every device id → wire id via `device_uuid`);
/// 6. [`establish_services`] over the relay.
///
/// On failure returns UI-ready copy (via [`user_facing_error`] / store mapping).
/// Never logs the relay token or any key material.
#[cfg(not(target_arch = "wasm32"))]
pub async fn open_via_relay(
    profile: &ConnectionProfile,
    store_access_token: &str,
    app_device_id: &str,
    local_key: &StaticKeypair,
) -> Result<OpenedRelaySession, sqlly_core::DisplayMessage> {
    let ConnectionRoute::ViaRelay {
        relay_device_id, ..
    } = &profile.route
    else {
        return Err(user_facing_error(&RemoteSessionError::NotViaRelay));
    };

    // Publish our public key first so the bridge can pin the app before the
    // Noise handshake. Only the public key leaves this process.
    let public_key = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(local_key.public_key())
    };
    crate::store::register_app_noise_key(store_access_token, app_device_id, &public_key)
        .await
        .map_err(store_error_message)?;

    let endpoints = crate::store::list_relay_endpoints()
        .await
        .map_err(store_error_message)?;
    let token = crate::store::get_relay_token(store_access_token, app_device_id)
        .await
        .map_err(store_error_message)?;
    let pairs = crate::store::list_relay_pairs(store_access_token)
        .await
        .map_err(store_error_message)?;

    // Resolve the authorizing pair by IDENTITY — control side is THIS app
    // device, connector side is the profile's relay device — never by the
    // stored pair id alone. A profile can outlive its pair (revoked and
    // re-authorized) or be opened on a different machine than the one that
    // created it; what the relay actually checks is the (this device → that
    // connector) grant, so that is what must exist here.
    let pair = pairs
        .iter()
        .find(|p| {
            p.active
                && p.control_device_id == app_device_id
                && p.connector_device_id == *relay_device_id
        })
        .ok_or_else(|| {
            crate::localization::deferred_text("connections.relay.device_not_authorized")
        })?;
    let connector_key = pair
        .connector_noise_public_key_bytes()
        .map_err(store_error_message)?
        .ok_or_else(|| {
            crate::localization::deferred_text("connections.relay.security_key_missing")
        })?;

    let endpoints = endpoints.into_iter().map(to_endpoint).collect();
    let inputs = build_relay_inputs(
        app_device_id,
        relay_device_id,
        connector_key,
        endpoints,
        token.access_token,
        profile_relay_port(profile),
    );

    let (services, relay_hostname, closed) = establish_services(profile, &inputs, local_key)
        .await
        .map_err(|e| user_facing_error(&e))?;

    // Only name the bridge once the session is up: the label is cosmetic, and
    // a failed open should not spend two more store round-trips.
    let bridge_name = resolve_bridge_display_name(store_access_token, relay_device_id).await;

    Ok(OpenedRelaySession {
        services,
        relay_hostname,
        bridge_name,
        closed,
    })
}

/// Map a store client error to secret-free, user-facing copy. `StoreError`
/// carries no key material, but we still translate to friendly guidance.
/// Keyed (thread-safe): the session opens on the tokio runtime and the UI
/// resolves the message with `localization::message`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn store_error_message(err: crate::store::StoreError) -> sqlly_core::DisplayMessage {
    use crate::store::StoreError;
    crate::localization::deferred_text(match err {
        StoreError::Network => "connections.relay.account_unreachable",
        StoreError::Server { status, .. } if status == 401 || status == 403 => {
            "connections.relay.account_not_authorized"
        }
        StoreError::BrowserUnsupported => "connections.relay.browser_connections_unavailable",
        _ => "connections.relay.open_session_failed",
    })
}

/// Map a [`RemoteSessionError`] to a clear, user-facing message for the status
/// bar. Relay error codes (`DEST_OFFLINE`, `ROUTING_DENIED`, auth) become
/// actionable copy; everything else falls back to a generic, secret-free line.
#[must_use]
pub fn user_facing_error(err: &RemoteSessionError) -> sqlly_core::DisplayMessage {
    crate::localization::deferred_text(match err {
        RemoteSessionError::NotViaRelay => "connections.relay.not_routed",
        RemoteSessionError::NoRelayAvailable => "connections.relay.no_server_available",
        RemoteSessionError::Remote(remote) => return remote_error_message(remote),
    })
}

fn remote_error_message(err: &RemoteError) -> sqlly_core::DisplayMessage {
    crate::localization::deferred_text(match err {
        RemoteError::AuthFailed => "connections.relay.authentication_failed",
        RemoteError::Relay(code) => return relay_code_message(*code),
        RemoteError::RouteRevoked => "connections.relay.access_revoked",
        RemoteError::PeerMismatch => "connections.relay.peer_identity_mismatch",
        RemoteError::Noise(_) => "connections.relay.noise_identity_mismatch",
        RemoteError::HandshakeClosed => "connections.relay.handshake_closed",
        RemoteError::Io(_) => "connections.relay.server_unreachable",
        _ => "connections.relay.session_failed",
    })
}

/// Copy shown when an engine operation is attempted on a `ViaRelay` profile
/// with no established relay session (see `ServiceRouter`). Lives here with
/// the rest of the relay UI copy — one source of truth.
pub fn relay_session_not_open_message() -> String {
    crate::localization::text("connections.relay.session_not_open")
}

fn relay_code_message(code: RelayErrorCode) -> sqlly_core::DisplayMessage {
    crate::localization::deferred_text(match code {
        RelayErrorCode::DestOffline => "connections.relay.device_offline",
        RelayErrorCode::RoutingDenied => "connections.relay.routing_denied",
        RelayErrorCode::AuthFailed => "connections.relay.code_authentication_failed",
        RelayErrorCode::ProtocolError => "connections.relay.protocol_error",
        RelayErrorCode::RateLimited => "connections.relay.rate_limited",
        RelayErrorCode::RelayFull => "connections.relay.server_at_capacity",
        RelayErrorCode::SessionLimit => "connections.relay.session_limit",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use sqlly_remote::engine_protocol::{ProtocolError, RawFrame};

    fn ep(hostname: &str, status: &str, capacity_pct: i32) -> RelayEndpoint {
        RelayEndpoint {
            relay_id: Some(Uuid::from_u128(0xA1)),
            hostname: hostname.to_string(),
            location: "test".to_string(),
            status: status.to_string(),
            capacity_pct,
        }
    }

    /// An endpoint without a relay wire id cannot be authenticated to (the
    /// Auth frame has no destination), so selection must skip it even when it
    /// is otherwise the best — or the hinted — choice. Mirrors the bridge's
    /// discovery rule.
    #[test]
    fn selection_skips_endpoints_without_relay_id() {
        let mut anonymous = ep("anonymous.relay", "online", 0);
        anonymous.relay_id = None;
        let eps = vec![anonymous.clone(), ep("known.relay", "online", 90)];
        assert_eq!(select_relay(&eps, None).unwrap().hostname, "known.relay");
        assert_eq!(
            select_relay(&eps, Some("anonymous.relay"))
                .unwrap()
                .hostname,
            "known.relay",
            "a hinted endpoint without a relay id must still be skipped"
        );
        assert!(select_relay(std::slice::from_ref(&anonymous), None).is_none());
    }

    #[test]
    fn selection_skips_non_online_and_full() {
        let eps = vec![
            ep("offline.relay", "offline", 0),
            ep("draining.relay", "draining", 10),
            ep("full.relay", "online", 100),
            ep("good.relay", "online", 42),
        ];
        assert_eq!(select_relay(&eps, None).unwrap().hostname, "good.relay");
    }

    #[test]
    fn selection_none_when_all_ineligible() {
        let eps = vec![
            ep("a", "offline", 0),
            ep("b", "full", 50),
            ep("c", "online", 100),
        ];
        assert!(select_relay(&eps, None).is_none());
    }

    #[test]
    fn selection_prefers_eligible_hint() {
        let eps = vec![
            ep("cheap.relay", "online", 5),
            ep("hint.relay", "online", 80),
        ];
        assert_eq!(
            select_relay(&eps, Some("hint.relay")).unwrap().hostname,
            "hint.relay"
        );
    }

    #[test]
    fn selection_ignores_ineligible_hint() {
        let eps = vec![
            ep("hint.relay", "offline", 5),
            ep("cheap.relay", "online", 30),
        ];
        assert_eq!(
            select_relay(&eps, Some("hint.relay")).unwrap().hostname,
            "cheap.relay"
        );
    }

    #[test]
    fn selection_lowest_capacity_then_hostname() {
        let eps = vec![
            ep("zeta", "online", 20),
            ep("alpha", "online", 20),
            ep("busy", "online", 90),
        ];
        assert_eq!(select_relay(&eps, None).unwrap().hostname, "alpha");
    }

    #[test]
    fn profile_hint_strips_port_and_ignores_direct() {
        use sqlly_core::{SecretProvider, SecretRef};

        let mut profile = ConnectionProfile::sql_login(
            "p",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "x".to_string(),
            },
        );
        // A Direct profile has no relay hint.
        assert_eq!(profile_relay_hint(&profile), None);

        profile.route = ConnectionRoute::ViaRelay {
            relay_device_id: "dev".to_string(),
            pair_id: "pair".to_string(),
            relay_hint: Some("relay-eu.sqlly.app:443".to_string()),
        };
        assert_eq!(profile_relay_hint(&profile), Some("relay-eu.sqlly.app"));

        profile.route = ConnectionRoute::ViaRelay {
            relay_device_id: "dev".to_string(),
            pair_id: "pair".to_string(),
            relay_hint: None,
        };
        assert_eq!(profile_relay_hint(&profile), None);
    }

    /// A `FramedTransport` that never yields a frame — enough to prove
    /// [`assemble_services_from_engine`] builds a `Services` from an arbitrary transport
    /// (not just a `PeerSession`) without a live relay.
    struct SilentTransport;

    #[async_trait]
    impl FramedTransport for SilentTransport {
        async fn recv(&mut self) -> Result<Option<RawFrame>, ProtocolError> {
            Ok(None)
        }
        async fn send(&mut self, _frame: &RawFrame) -> Result<(), ProtocolError> {
            Ok(())
        }
    }

    #[test]
    fn assemble_services_from_engine_builds_a_services_from_any_transport() {
        // `RemoteServices::from_transport` spawns a demux task, so assembly must
        // run inside a Tokio runtime context (as it always does in the app).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = rt.enter();
        // Constructing the bundle exercises the full remote-port wiring; if the
        // assembly seam ever stops producing a usable `Services` this fails to
        // compile or panics here rather than in the field.
        let (services, _closed) = assemble_services_from_engine(RemoteEngine::new(SilentTransport));
        // The engine-backed ports are present (Arc pointers are non-null by
        // construction); a trivial use keeps the value observed.
        let _ = &services.queries;
        let _ = &services.catalog;
    }

    #[test]
    fn build_inputs_maps_every_device_id_through_device_uuid() {
        let inputs = build_relay_inputs(
            "app-dev-123",
            "bridge-dev-456",
            [9u8; KEY_LEN],
            vec![ep("relay.sqlly.app", "online", 10)],
            "relay.jwt.token".to_string(),
            8443,
        );
        // Own id and peer id are the UUIDv5 of their control-plane strings — the
        // exact values the relay and bridge independently derive.
        assert_eq!(inputs.local_id, device_uuid("app-dev-123"));
        assert_eq!(inputs.peer_id, device_uuid("bridge-dev-456"));
        assert_eq!(inputs.peer_static_key, [9u8; KEY_LEN]);
        assert_eq!(inputs.relay_port, 8443);
        assert_eq!(inputs.endpoints.len(), 1);
        // The Auth-frame destination is the endpoint's own reported wire id —
        // real and non-nil (a nil destination cannot be encoded into an Auth
        // frame; endpoints that haven't reported one are skipped by selection).
        let selected = select_relay(&inputs.endpoints, None).unwrap();
        let relay_id = selected.relay_id.unwrap();
        assert!(!relay_id.is_nil());
        assert_eq!(relay_id, Uuid::from_u128(0xA1));
    }

    #[test]
    fn build_inputs_debug_redacts_the_token() {
        let inputs = build_relay_inputs(
            "app-dev",
            "bridge-dev",
            [0u8; KEY_LEN],
            vec![],
            "super-secret".to_string(),
            8443,
        );
        let rendered = format!("{inputs:?}");
        assert!(!rendered.contains("super-secret"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn app_noise_keypair_is_stable_across_loads_for_the_same_device() {
        let store = sqlly_remote::noise::InMemorySecretStore::new();
        let first = app_noise_keypair_with(&store, "app-dev-abc").unwrap();
        // A second load returns the *same* persisted identity (same public key),
        // and a different device id derives a distinct one.
        let again = app_noise_keypair_with(&store, "app-dev-abc").unwrap();
        assert_eq!(first.public_key(), again.public_key());
        let other = app_noise_keypair_with(&store, "app-dev-xyz").unwrap();
        assert_ne!(first.public_key(), other.public_key());
    }

    #[test]
    fn error_mapping_is_actionable_for_relay_codes() {
        let offline = user_facing_error(&RemoteSessionError::Remote(RemoteError::Relay(
            RelayErrorCode::DestOffline,
        )));
        assert!(offline.to_lowercase().contains("offline"));

        let denied = user_facing_error(&RemoteSessionError::Remote(RemoteError::Relay(
            RelayErrorCode::RoutingDenied,
        )));
        assert!(denied.to_lowercase().contains("not authorized"));

        let auth = user_facing_error(&RemoteSessionError::Remote(RemoteError::AuthFailed));
        assert!(auth.to_lowercase().contains("authentication failed"));

        let full = user_facing_error(&RemoteSessionError::Remote(RemoteError::Relay(
            RelayErrorCode::RelayFull,
        )));
        assert!(full.to_lowercase().contains("capacity"));

        let none = user_facing_error(&RemoteSessionError::NoRelayAvailable);
        assert!(none.to_lowercase().contains("no relay"));

        let revoked = user_facing_error(&RemoteSessionError::Remote(RemoteError::RouteRevoked));
        assert!(revoked.to_lowercase().contains("revoked"));
    }
}
