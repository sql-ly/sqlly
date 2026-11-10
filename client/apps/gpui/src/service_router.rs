//! Route-aware [`Services`] resolution, shared by the app and its connection
//! dialogs.
//!
//! One rule, applied everywhere an engine-backed operation picks its services:
//!
//! * [`ConnectionRoute::Direct`] always resolves to the in-process services —
//!   byte-for-byte the behavior from before routing existed.
//! * [`ConnectionRoute::ViaRelay`] resolves to the remote-backed services
//!   installed for that profile's established relay session. With **no**
//!   established session it resolves to an erroring bundle
//!   ([`sqlly_services::remote::unavailable_services`]) whose every
//!   engine-backed call fails with [`RELAY_SESSION_NOT_OPEN_MESSAGE`] — never
//!   to a silent direct-TCP attempt, which is unreachable by definition for a
//!   relay-routed profile.
//!
//! The session map is shared (`Arc`) so dialogs the app spawns (the connection
//! editor's Test Connection / database probe) resolve against the app's LIVE
//! session state, not a snapshot taken when the dialog opened.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use sqlly_core::connection::{ConnectionProvenance, ConnectionRoute};
use sqlly_core::{ConnectionId, ConnectionProfile};
use sqlly_services::Services;

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;

/// Every engine-backed call on a proxied profile fails with this until the
/// relay proxy session is open (or after it drops).
pub fn proxy_session_not_open_message() -> String {
    crate::localization::text("connections.relay.proxy_session_not_open")
}

#[cfg(not(target_arch = "wasm32"))]
pub use crate::remote_session::relay_session_not_open_message;

#[derive(Clone)]
pub struct ServiceRouter {
    /// The in-process services every Direct profile (and all client-side
    /// ports) use.
    in_process: Services,
    /// Remote-backed `Services` for established Via-Relay sessions, keyed by
    /// connection id. Shared with spawned dialogs so resolution is live.
    /// Native only — the browser build has no relay client.
    #[cfg(not(target_arch = "wasm32"))]
    remote_sessions: Arc<StdMutex<HashMap<ConnectionId, Services>>>,
    /// The one relay-proxy session's services (browser build): every profile
    /// the proxy handed over ([`ConnectionProvenance::Proxied`]) runs through
    /// it. Shared with spawned dialogs like the relay map.
    proxy: Arc<StdMutex<Option<Services>>>,
}

impl ServiceRouter {
    pub fn new(in_process: Services) -> Self {
        Self {
            in_process,
            #[cfg(not(target_arch = "wasm32"))]
            remote_sessions: Arc::new(StdMutex::new(HashMap::new())),
            proxy: Arc::new(StdMutex::new(None)),
        }
    }

    /// Resolve the [`Services`] that drive engine operations for `profile`,
    /// per the routing rule in the module docs.
    pub fn services_for(&self, profile: &ConnectionProfile) -> Services {
        // A proxied profile is secret-free by construction: the only thing
        // that can open it is the proxy session it came from, so it never
        // falls through to the in-process services.
        if profile.provenance == ConnectionProvenance::Proxied {
            return self
                .proxy
                .lock()
                .expect("proxy session poisoned")
                .clone()
                .unwrap_or_else(|| {
                    sqlly_services::remote::unavailable_services(proxy_session_not_open_message())
                });
        }
        match &profile.route {
            ConnectionRoute::Direct => self.in_process.clone(),
            ConnectionRoute::ViaRelay { .. } => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    self.remote_sessions
                        .lock()
                        .expect("remote session map poisoned")
                        .get(&profile.id)
                        .cloned()
                        .unwrap_or_else(|| {
                            sqlly_services::remote::unavailable_services(
                                relay_session_not_open_message(),
                            )
                        })
                }
                // Browser build: no relay client exists, and the in-process
                // services answer from the in-tab sample database — nothing
                // real can be dialed, so the preview stays functional.
                #[cfg(target_arch = "wasm32")]
                {
                    self.in_process.clone()
                }
            }
        }
    }

    /// Install the remote-backed [`Services`] for an established Via-Relay
    /// session.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn install(&self, id: ConnectionId, services: Services) {
        self.remote_sessions
            .lock()
            .expect("remote session map poisoned")
            .insert(id, services);
    }

    /// Drop any cached remote session for `id` (on disconnect or profile
    /// deletion) so a later reconnect re-establishes the relay session cleanly.
    pub fn remove(&self, id: &ConnectionId) {
        #[cfg(not(target_arch = "wasm32"))]
        self.remote_sessions
            .lock()
            .expect("remote session map poisoned")
            .remove(id);
        #[cfg(target_arch = "wasm32")]
        let _ = id;
    }

    /// Install the relay proxy session's services for every proxied profile.
    pub fn install_proxy(&self, services: Services) {
        *self.proxy.lock().expect("proxy session poisoned") = Some(services);
    }

    /// Forget the relay proxy session (it closed); proxied profiles then
    /// resolve to the erroring bundle until a new one is installed.
    pub fn clear_proxy(&self) {
        *self.proxy.lock().expect("proxy session poisoned") = None;
    }

    /// Whether a relay proxy session is installed.
    pub fn has_proxy(&self) -> bool {
        self.proxy.lock().expect("proxy session poisoned").is_some()
    }

    /// Whether a remote session is currently installed for `id`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn has_session(&self, id: &ConnectionId) -> bool {
        self.remote_sessions
            .lock()
            .expect("remote session map poisoned")
            .contains_key(id)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    fn profile(route: ConnectionRoute) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "p",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".to_string(),
            },
        );
        profile.route = route;
        profile
    }

    fn via_relay() -> ConnectionRoute {
        ConnectionRoute::ViaRelay {
            relay_device_id: "dev".to_string(),
            pair_id: "pair".to_string(),
            relay_hint: None,
        }
    }

    /// Same underlying port instances = same services. `Arc::ptr_eq` on the
    /// query executor is the identity check `Services` itself supports.
    fn same_services(a: &Services, b: &Services) -> bool {
        Arc::ptr_eq(&a.queries, &b.queries)
    }

    /// A proxied profile resolves to the installed proxy session, and to the
    /// erroring bundle (never in-process) while none is installed.
    #[test]
    fn proxied_profiles_resolve_only_through_the_proxy_session() {
        let in_process = Services::new();
        let router = ServiceRouter::new(in_process.clone());
        let mut proxied = profile(ConnectionRoute::Direct);
        proxied.provenance = ConnectionProvenance::Proxied;

        let resolved = router.services_for(&proxied);
        assert!(!same_services(&resolved, &in_process));
        assert!(!router.has_proxy());

        let proxy = Services::new();
        router.install_proxy(proxy.clone());
        assert!(router.has_proxy());
        assert!(same_services(&router.services_for(&proxied), &proxy));
        // Non-proxied profiles are untouched by the proxy session.
        assert!(same_services(
            &router.services_for(&profile(ConnectionRoute::Direct)),
            &in_process
        ));

        router.clear_proxy();
        assert!(!same_services(&router.services_for(&proxied), &in_process));
    }

    #[test]
    fn direct_always_resolves_in_process() {
        let in_process = Services::new();
        let router = ServiceRouter::new(in_process.clone());
        let resolved = router.services_for(&profile(ConnectionRoute::Direct));
        assert!(same_services(&resolved, &in_process));
    }

    /// The no-silent-fallback rule: a Via-Relay profile with no established
    /// session must NOT resolve to the in-process (direct-dialing) services —
    /// its engine calls fail with the "connect the relay session" copy.
    #[test]
    fn via_relay_without_session_never_falls_back_to_direct() {
        let in_process = Services::new();
        let router = ServiceRouter::new(in_process.clone());
        let profile = profile(via_relay());
        let resolved = router.services_for(&profile);
        assert!(!same_services(&resolved, &in_process));

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        match rt.block_on(resolved.queries.execute(&profile, "SELECT 1", 8)) {
            Err(err) => assert_eq!(
                err.to_string(),
                format!("Connection error: {}", relay_session_not_open_message())
            ),
            Ok(_) => panic!("no session → engine calls must fail"),
        }
    }

    #[test]
    fn install_and_remove_swap_the_via_relay_resolution() {
        let router = ServiceRouter::new(Services::new());
        let profile = profile(via_relay());
        assert!(!router.has_session(&profile.id));

        // Any distinct Services stands in for a relay-backed bundle here; the
        // router only stores and resolves it.
        let session = Services::new();
        router.install(profile.id, session.clone());
        assert!(router.has_session(&profile.id));
        assert!(same_services(&router.services_for(&profile), &session));

        router.remove(&profile.id);
        assert!(!router.has_session(&profile.id));
        assert!(!same_services(&router.services_for(&profile), &session));
    }
}
