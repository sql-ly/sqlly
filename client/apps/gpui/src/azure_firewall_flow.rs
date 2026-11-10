//! End-to-end "add my IP to Azure SQL firewall" flow for the gpui app.
//!
//! The Rust `sqlly-security::azure_discovery::AzureDiscoveryClient` does the
//! portable ARM orchestration; this module wraps it with the production
//! pieces the gpui app injects:
//!
//! - public-IP lookup via `api.ipify.org` (single-IP flow; range is not
//!   surfaced in the menu-driven path),
//! - silent refresh of the cached Entra access token via
//!   [`crate::azure_keyring::KeyringTokenCacheStore`] +
//!   [`sqlly_services::security::token_flow::EntraTokenClient`],
//! - a typed [`FirewallFlowOutcome`] the UI can render as a single panel.
//!
//! The Swift `FirewallRuleDialogController` is the parity reference; the
//! gpui path is identical in semantics even though it stages the work in a
//! single async task rather than two-phase sheets.

use chrono::{DateTime, Utc};
use sqlly_services::security::authorize_url::{AZURE_BROWSER_SCOPE, AZURE_PUBLIC_CLIENT_ID};
use sqlly_services::security::azure::EntraAccountId;
use sqlly_services::security::azure_discovery::{AzureDiscoveryClient, FirewallProvisionOutcome};
use sqlly_services::security::token_cache::{EntraTokenCache, TokenCacheError};
use sqlly_services::security::token_flow::EntraTokenClient;

/// Public-IP echo service URL. The endpoint returns the requester's public
/// IPv4 as plain text; a 5-second timeout keeps the menu invocation
/// responsive even when the network is sandboxed/offline (we surface the
/// failure to the UI rather than blocking indefinitely).
pub const PUBLIC_IP_ENDPOINT: &str = "https://api.ipify.org";

/// Looks up the device's public IPv4 via `api.ipify.org`.
pub async fn lookup_public_ip(
    transport: &crate::azure_http::ReqwestHttpTransport,
) -> Result<String, String> {
    let raw = transport.lookup_public_ip().await.map_err(|e| {
        crate::localization::format(
            "connections.firewall.public_ip_network_error",
            &[("endpoint", PUBLIC_IP_ENDPOINT), ("error", &e)],
        )
    })?;
    Ok(raw)
}

/// High-level wrap of the firewall flow so the app view can render a
/// single message. Mirrors the `FirewallProvisionOutcome` cases plus the
/// pre-flight conditions (no active profile, missing Entra binding, sign-in
/// required) so the UI has a flat, ergonomic type to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirewallFlowOutcome {
    Applied {
        rule_name: String,
        start_ip: String,
        end_ip: String,
    },
    Forbidden,
    ServerNotFound(String),
    NoSubscriptions,
    #[allow(dead_code)]
    NoAzureProfile,
    NoEntraBinding,
    AccountNotRegistered,
    SignInRequired,
    /// Public IP lookup failed before the ARM put could run. The message
    /// is already redacted to a user-readable summary.
    IpLookupFailed(String),
    /// Anything else (token refresh failure, transport, ARM 5xx). Carries
    /// a user-readable summary.
    Failed(String),
}

impl FirewallFlowOutcome {
    /// Render the outcome as a multi-line, ready-to-display string for
    /// the tool-output panel.
    pub fn render(&self, server_fqdn: &str) -> String {
        let (key, arguments) = match self {
            FirewallFlowOutcome::Applied {
                rule_name,
                start_ip,
                end_ip,
            } => {
                let range = if start_ip == end_ip {
                    start_ip.clone()
                } else {
                    format!("{start_ip}–{end_ip}")
                };
                (
                    "connections.firewall.flow_applied",
                    vec![
                        ("server_fqdn", server_fqdn.to_string()),
                        ("range", range),
                        ("rule_name", rule_name.clone()),
                    ],
                )
            }
            FirewallFlowOutcome::Forbidden => (
                "connections.firewall.flow_forbidden",
                vec![("server_fqdn", server_fqdn.to_string())],
            ),
            FirewallFlowOutcome::ServerNotFound(fqdn) => (
                "connections.firewall.flow_server_not_found",
                vec![("server_fqdn", fqdn.clone())],
            ),
            FirewallFlowOutcome::NoSubscriptions => {
                ("connections.firewall.flow_no_subscriptions", Vec::new())
            }
            FirewallFlowOutcome::NoAzureProfile => {
                ("connections.firewall.flow_not_azure_profile", Vec::new())
            }
            FirewallFlowOutcome::NoEntraBinding => {
                ("connections.firewall.flow_no_entra_binding", Vec::new())
            }
            FirewallFlowOutcome::AccountNotRegistered => (
                "connections.firewall.flow_account_not_registered",
                Vec::new(),
            ),
            FirewallFlowOutcome::SignInRequired => {
                ("connections.firewall.flow_signin_required", Vec::new())
            }
            FirewallFlowOutcome::IpLookupFailed(why) => (
                "connections.firewall.flow_ip_lookup_failed",
                vec![("error", why.clone())],
            ),
            FirewallFlowOutcome::Failed(why) => (
                "connections.firewall.flow_failed",
                vec![("error", why.clone())],
            ),
        };
        let arguments: Vec<(&str, &str)> = arguments
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect();
        crate::localization::format(key, &arguments)
    }
}

/// Wraps the [`crate::azure_keyring::KeyringTokenCacheStore`] and a
/// [`EntraTokenClient`] so the caller can read a valid access token with
/// one call. The return type carries the bound so callers can get a
/// broadly usable handle.
pub fn build_token_cache(
    keyring: crate::azure_keyring::KeyringTokenCacheStore,
    transport: crate::azure_http::ReqwestHttpTransport,
    tenant_id: &str,
) -> EntraTokenCache<
    crate::azure_keyring::KeyringTokenCacheStore,
    crate::azure_http::ReqwestHttpTransport,
> {
    let client = EntraTokenClient::new(
        transport,
        tenant_id.to_string(),
        AZURE_PUBLIC_CLIENT_ID,
        AZURE_BROWSER_SCOPE,
    );
    EntraTokenCache::new(keyring, client)
}

/// Resolve the [`EntraAccountId`] that matches a binding's combined
/// `account_id` (`"<home>:<tenant>"`). Pure mapping — no I/O.
pub fn account_id_from_binding(account_id: &str) -> EntraAccountId {
    // `EntraAccountId` simply wraps a String. The registry uses the same
    // canonical form as the binding, so propagate verbatim — the BTreeMap
    // lookup normalizes keys on insertion, never changing the joined shape.
    EntraAccountId(account_id.to_string())
}

/// Stamps `now` to a `YYYYMMDD` UTC date string for the firewall rule
/// name policy documented in [`crate::azure_firewall::firewall_rule_name`].
pub fn yyyymmdd_stamp(now: DateTime<Utc>) -> String {
    now.format("%Y%m%d").to_string()
}

/// Convert a [`FirewallProvisionOutcome`] (returned by the core ARM flow)
/// into a UI-friendly [`FirewallFlowOutcome`].
pub fn lift_outcome(
    arm_outcome: FirewallProvisionOutcome,
    account_id: &EntraAccountId,
) -> FirewallFlowOutcome {
    match arm_outcome {
        FirewallProvisionOutcome::Applied {
            rule_name,
            start_ip,
            end_ip,
        } => FirewallFlowOutcome::Applied {
            rule_name,
            start_ip,
            end_ip,
        },
        FirewallProvisionOutcome::Forbidden => FirewallFlowOutcome::Forbidden,
        FirewallProvisionOutcome::ServerNotFound(fqdn) => FirewallFlowOutcome::ServerNotFound(fqdn),
        FirewallProvisionOutcome::NoSubscriptions => FirewallFlowOutcome::NoSubscriptions,
        FirewallProvisionOutcome::Failed(message) => {
            // A failed token refresh shows up under InteractiveSignInRequired
            // before we ever get here; a non-trivial `Failed` message means
            // ARM came back with a non-classified error. The registry has
            // the binding itself (it's a precondition for the call), so we
            // report the ARM message verbatim.
            let _ = account_id;
            FirewallFlowOutcome::Failed(message)
        }
    }
}

/// Run the entire firewall flow as one async function so the app view
/// only needs a single `cx.spawn(…)` to drive it. All branches surface as
/// a [`FirewallFlowOutcome`] for direct rendering.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub async fn run_firewall_flow(
    transport: crate::azure_http::ReqwestHttpTransport,
    keyring: crate::azure_keyring::KeyringTokenCacheStore,
    registry: crate::azure_registry::EntraAccountRegistryStore,
    account_id: &EntraAccountId,
    server_fqdn: &str,
    username_for_rule_name: &str,
    now: DateTime<Utc>,
) -> FirewallFlowOutcome {
    // 1. Get the saved profile so we know which tenant to mint tokens for.
    if registry.find(account_id).is_none() {
        return FirewallFlowOutcome::AccountNotRegistered;
    }
    // 2. Resolve the public IP. This is what makes the rule useful: the
    //    SQL gateway sees our public IP, not our local LAN address.
    let start_ip = match lookup_public_ip(&transport).await {
        Ok(ip) => ip,
        Err(why) => return FirewallFlowOutcome::IpLookupFailed(why),
    };
    let end_ip = start_ip.clone();
    let rule_name =
        crate::azure_firewall::firewall_rule_name(username_for_rule_name, &yyyymmdd_stamp(now));
    run_firewall_flow_with_rule(
        transport,
        keyring,
        registry,
        account_id,
        server_fqdn,
        &rule_name,
        &start_ip,
        &end_ip,
        now,
    )
    .await
}

/// The same flow, but with the rule name and address range already chosen —
/// the path the 40615 dialog drives, where the blocked IP comes out of the
/// server's own error message and the name is the user's to pick.
///
/// Kept separate from [`run_firewall_flow`] so the menu command keeps its
/// zero-prompt behaviour (look up my IP, name it after me, apply) while the
/// dialog never guesses at either value.
#[allow(clippy::too_many_arguments)]
pub async fn run_firewall_flow_with_rule(
    transport: crate::azure_http::ReqwestHttpTransport,
    keyring: crate::azure_keyring::KeyringTokenCacheStore,
    registry: crate::azure_registry::EntraAccountRegistryStore,
    account_id: &EntraAccountId,
    server_fqdn: &str,
    rule_name: &str,
    start_ip: &str,
    end_ip: &str,
    now: DateTime<Utc>,
) -> FirewallFlowOutcome {
    let profile = match registry.find(account_id) {
        Some(p) => p,
        None => return FirewallFlowOutcome::AccountNotRegistered,
    };
    let tenant_id = profile.tenant_id.clone();
    let cache_ref = profile.token_cache_ref.clone();
    // Resolve an access token, silently refreshing if possible.
    let cache = build_token_cache(keyring, transport.clone(), &tenant_id);
    let token = match cache.valid_access_token(&cache_ref, now).await {
        Ok(t) => t,
        Err(TokenCacheError::InteractiveSignInRequired) => {
            return FirewallFlowOutcome::SignInRequired;
        }
        Err(e) => {
            return FirewallFlowOutcome::Failed(crate::localization::format(
                "connections.firewall.token_refresh_failed",
                &[("error", &e.to_string())],
            ));
        }
    };
    // Build the ARM client and PUT the rule.
    let arm_client = AzureDiscoveryClient::new(transport);
    let arm_outcome = arm_client
        .add_firewall_rule_for_fqdn(
            account_id,
            server_fqdn,
            rule_name,
            start_ip,
            end_ip,
            &token.access_token,
            now,
        )
        .await;
    lift_outcome(arm_outcome, account_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::azure_firewall::firewall_rule_name;
    use chrono::TimeZone;

    #[test]
    fn rule_name_stamp_uses_yyyymmdd_utc() {
        let now = Utc.with_ymd_and_hms(2026, 6, 6, 4, 0, 0).unwrap();
        assert_eq!(yyyymmdd_stamp(now), "20260606");
        assert_eq!(
            firewall_rule_name("alice", &yyyymmdd_stamp(now)),
            "SQLLY-alice-20260606"
        );
    }

    #[test]
    fn account_id_from_binding_round_trips() {
        let id = account_id_from_binding("home-abc:tenant-xyz");
        assert_eq!(id.0, "home-abc:tenant-xyz");
    }

    #[test]
    fn render_applied_describes_range_and_rule_name() {
        let out = FirewallFlowOutcome::Applied {
            rule_name: "SQLLY-alice-20260606".into(),
            start_ip: "203.0.113.5".into(),
            end_ip: "203.0.113.5".into(),
        };
        let body = out.render("srv.database.windows.net");
        assert!(body.contains("SQLLY-alice-20260606"));
        assert!(body.contains("203.0.113.5"));
        assert!(body.contains("srv.database.windows.net"));
    }

    #[test]
    fn render_forbidden_mentions_permission_noun() {
        let body = FirewallFlowOutcome::Forbidden.render("srv.database.windows.net");
        assert!(body.contains("permission"));
        assert!(body.contains("Microsoft.Sql/servers/firewallRules/write"));
    }

    #[test]
    fn render_forbidden_says_who_to_ask_instead() {
        // A 403 is the user's own Azure RBAC talking. The message has to say
        // plainly that they do not have the permission and that someone else
        // must make the change — not imply a retry or a workaround.
        let body = FirewallFlowOutcome::Forbidden.render("srv.database.windows.net");
        assert!(body.contains("do not seem to have permission"));
        assert!(body.contains("ask someone"));
    }

    #[test]
    fn render_signin_required_guides_user() {
        let body = FirewallFlowOutcome::SignInRequired.render("srv.database.windows.net");
        assert!(body.contains("Azure SQL Connection"));
    }

    #[test]
    fn lift_outcome_passes_through_applied() {
        let arm = FirewallProvisionOutcome::Applied {
            rule_name: "r".into(),
            start_ip: "1.2.3.4".into(),
            end_ip: "1.2.3.4".into(),
        };
        let account = account_id_from_binding("h:t");
        match lift_outcome(arm, &account) {
            FirewallFlowOutcome::Applied {
                rule_name,
                start_ip,
                end_ip,
            } => {
                assert_eq!(rule_name, "r");
                assert_eq!(start_ip, "1.2.3.4");
                assert_eq!(end_ip, "1.2.3.4");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn lift_outcome_passes_failed_through_when_account_present() {
        let arm = FirewallProvisionOutcome::Failed("boom".into());
        let lifted = lift_outcome(arm, &account_id_from_binding("h:t"));
        assert_eq!(lifted, FirewallFlowOutcome::Failed("boom".into()));
    }
}
