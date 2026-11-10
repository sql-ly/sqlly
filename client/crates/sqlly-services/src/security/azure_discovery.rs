//! Live Azure Resource Manager (ARM) discovery client.
//!
//! Orchestrates the REST calls that enumerate subscriptions → SQL servers →
//! databases for a signed-in Entra account, reusing the pure parsers in
//! [`crate::security::azure`]. The HTTP layer is the mockable [`HttpTransport`] seam from
//! [`crate::security::token_flow`], so the full discovery walk is unit-testable with
//! scripted ARM responses and no live credentials. No concrete HTTP client is
//! bundled (per project decision: no `reqwest`).

use crate::security::azure::{
    self, AzureSqlDatabaseResource, AzureSqlServerResource, AzureSubscription, EntraAccountId,
};
use crate::security::token_flow::{HttpTransport, TokenFlowError};
use chrono::{DateTime, Duration, Utc};
use sqlly_core::{StudioError, StudioResult};

/// ARM API version used for the SQL resource provider and subscriptions list.
pub const ARM_BASE: &str = "https://management.azure.com";
pub const SUBSCRIPTIONS_API_VERSION: &str = "2022-12-01";
pub const SQL_API_VERSION: &str = "2023-05-01-preview";

/// A reachability/firewall hint produced when a discovered server cannot be
/// reached. Surfaced to the UI so users get actionable next steps instead of a
/// raw socket error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirewallGuidance {
    pub server_fqdn: String,
    pub message: String,
}

/// Builds firewall guidance text for an unreachable Azure SQL server. The
/// public client IP is included when known so the user can add the exact rule.
pub fn firewall_guidance(server_fqdn: &str, public_ip: Option<&str>) -> FirewallGuidance {
    let ip_hint = match public_ip {
        Some(ip) => format!(
            " Add a firewall rule for your client IP {ip} on the server (Networking → Firewall rules)."
        ),
        None => " Add a firewall rule for your current client IP on the server (Networking → Firewall rules).".to_string(),
    };
    FirewallGuidance {
        server_fqdn: server_fqdn.to_string(),
        message: format!(
            "Could not reach {server_fqdn}. This is usually the Azure SQL server firewall blocking your IP.{ip_hint} Also confirm 'Allow Azure services' if connecting from a hosted environment."
        ),
    }
}

/// Drives ARM discovery over an injected [`HttpTransport`], authenticating each
/// request with a bearer access token.
pub struct AzureDiscoveryClient<T: HttpTransport> {
    transport: T,
}

impl<T: HttpTransport> AzureDiscoveryClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    async fn get_json(&self, url: &str, access_token: &str) -> StudioResult<String> {
        let response = self
            .transport
            .get_with_bearer(url, access_token)
            .await
            .map_err(StudioError::from)?;
        if response.status >= 400 {
            return Err(StudioError::from(map_arm_error(
                response.status,
                &response.body,
            )));
        }
        Ok(response.body)
    }

    /// Lists subscriptions visible to the account.
    pub async fn list_subscriptions(
        &self,
        account_id: &EntraAccountId,
        access_token: &str,
    ) -> StudioResult<Vec<AzureSubscription>> {
        let url = format!("{ARM_BASE}/subscriptions?api-version={SUBSCRIPTIONS_API_VERSION}");
        let body = self.get_json(&url, access_token).await?;
        azure::parse_subscriptions(account_id, &body)
    }

    /// Lists Azure SQL servers within a subscription.
    pub async fn list_sql_servers(
        &self,
        account_id: &EntraAccountId,
        subscription: &AzureSubscription,
        access_token: &str,
    ) -> StudioResult<Vec<AzureSqlServerResource>> {
        let url = format!(
            "{ARM_BASE}/subscriptions/{}/providers/Microsoft.Sql/servers?api-version={SQL_API_VERSION}",
            subscription.subscription_id
        );
        let body = self.get_json(&url, access_token).await?;
        azure::parse_sql_servers(account_id, subscription, &body)
    }

    /// Lists databases on an Azure SQL server.
    pub async fn list_sql_databases(
        &self,
        server: &AzureSqlServerResource,
        access_token: &str,
    ) -> StudioResult<Vec<AzureSqlDatabaseResource>> {
        let url = format!(
            "{ARM_BASE}{}/databases?api-version={SQL_API_VERSION}",
            server.server_id
        );
        let body = self.get_json(&url, access_token).await?;
        azure::parse_sql_databases(server, &body)
    }

    /// Full discovery walk: subscriptions → servers → databases, returning a
    /// fresh discovery cache. Servers with no databases are skipped silently;
    /// the `master` database is retained (callers may filter).
    pub async fn discover_all(
        &self,
        account_id: &EntraAccountId,
        access_token: &str,
        now: DateTime<Utc>,
        ttl: Duration,
    ) -> StudioResult<azure::AzureSqlDiscoveryCache> {
        let subscriptions = self.list_subscriptions(account_id, access_token).await?;
        let mut databases = Vec::new();
        for subscription in &subscriptions {
            let servers = self
                .list_sql_servers(account_id, subscription, access_token)
                .await?;
            for server in &servers {
                let dbs = self.list_sql_databases(server, access_token).await?;
                databases.extend(dbs);
            }
        }
        Ok(azure::build_discovery_cache(
            account_id.clone(),
            now,
            ttl,
            subscriptions,
            databases,
        ))
    }

    /// Creates (or updates) a server-level firewall rule via ARM PUT on
    /// `{serverId}/firewallRules/{ruleName}`. The PUT is idempotent on
    /// `ruleName`, so re-running with the same name does not produce duplicate
    /// rules. Returns the raw ARM response so callers can distinguish a 200/201
    /// success from a 403 (RBAC denied) or 404 (server disappeared between
    /// discovery and PUT). The caller decides how to render each status —
    /// [`FirewallRulePutOutcome`] normalizes the cases.
    pub async fn put_firewall_rule(
        &self,
        server_id: &str,
        rule_name: &str,
        start_ip: &str,
        end_ip: &str,
        access_token: &str,
    ) -> StudioResult<FirewallRulePutResult> {
        let url = sql_firewall_rule_url(server_id, rule_name);
        // Server-level firewall rules accept IPv4 strings; we do NOT
        // percent-encode the rule name beyond what `url_path_safe` enforces
        // (the rule-name builder upstream guarantees `[A-Za-z0-9_-]` only).
        let body_json = serde_json::json!({
            "properties": {
                "startIpAddress": start_ip,
                "endIpAddress": end_ip,
            }
        })
        .to_string();
        let response = self
            .transport
            .put_json_with_bearer(&url, access_token, &body_json)
            .await
            .map_err(StudioError::from)?;
        Ok(FirewallRulePutResult {
            status: response.status,
            body: response.body,
        })
    }

    /// End-to-end "add my IP" flow: discovers every subscription visible to
    /// the signed-in account, scans each subscription for the target server
    /// FQDN, then PUTs the firewall rule against the matching server's ARM
    /// resource id. Returns the categorized [`FirewallProvisionOutcome`] so
    /// the caller can surface a precise message to the user without throwing
    /// on the expected 403/404 cases.
    ///
    /// `now` is the wall-clock used to stamp the rule name
    /// (`SQLLY-<local>-<yyyymmdd>`); pass `Utc::now()` for an unpartitioned
    /// caller. See [`crate::security::azure_discovery`] for the matching date stamp.
    #[allow(clippy::too_many_arguments)]
    pub async fn add_firewall_rule_for_fqdn(
        &self,
        account_id: &EntraAccountId,
        server_fqdn: &str,
        rule_name: &str,
        start_ip: &str,
        end_ip: &str,
        access_token: &str,
        now: DateTime<Utc>,
    ) -> FirewallProvisionOutcome {
        // Step 1: subscriptions accessible to the account.
        let subscriptions = match self.list_subscriptions(account_id, access_token).await {
            Ok(s) => s,
            Err(e) => return FirewallProvisionOutcome::Failed(format_arm_failure(&e)),
        };
        if subscriptions.is_empty() {
            return FirewallProvisionOutcome::NoSubscriptions;
        }
        // Step 2: find the matching server. We deliberately walk every
        // subscription because the SQL server may live in a different
        // subscription than the one the user initially picked — ARM search
        // by FQDN is server-by-server.
        let mut match_server: Option<AzureSqlServerResource> = None;
        for subscription in &subscriptions {
            let servers = match self
                .list_sql_servers(account_id, subscription, access_token)
                .await
            {
                Ok(s) => s,
                Err(e) => return FirewallProvisionOutcome::Failed(format_arm_failure(&e)),
            };
            if let Some(server) = find_server_by_fqdn(server_fqdn, &servers) {
                match_server = Some(server.clone());
                break;
            }
        }
        let server = match match_server {
            Some(s) => s,
            None => return FirewallProvisionOutcome::ServerNotFound(server_fqdn.to_string()),
        };
        let _ = now; // reserved for future caller-side metadata; the rule
        // name string already encodes its own stamp.
        // Step 3: PUT the firewall rule under that server.
        let put_result = match self
            .put_firewall_rule(&server.server_id, rule_name, start_ip, end_ip, access_token)
            .await
        {
            Ok(r) => r,
            Err(e) => return FirewallProvisionOutcome::Failed(format_arm_failure(&e)),
        };
        match put_result.outcome() {
            FirewallRulePutOutcome::Applied => FirewallProvisionOutcome::Applied {
                rule_name: rule_name.to_string(),
                start_ip: start_ip.to_string(),
                end_ip: end_ip.to_string(),
            },
            FirewallRulePutOutcome::Forbidden => FirewallProvisionOutcome::Forbidden,
            FirewallRulePutOutcome::NotFound => {
                // Server disappeared between discovery and PUT — surface it as
                // not-found for the same reason as the discovery-side miss.
                FirewallProvisionOutcome::ServerNotFound(server_fqdn.to_string())
            }
            FirewallRulePutOutcome::Failed(status) => FirewallProvisionOutcome::Failed(format!(
                "Azure returned HTTP {status}: {}",
                redact_body(&put_result.body)
            )),
        }
    }
}

/// Categorized outcome of a single ARM PUT against
/// `Microsoft.Sql/servers/.../firewallRules/<name>`. Maps HTTP status to a
/// user-facing meaning so the caller can pick the right user message without
/// re-deriving the categorization inline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirewallRulePutResult {
    pub status: u16,
    pub body: String,
}

impl FirewallRulePutResult {
    pub fn outcome(&self) -> FirewallRulePutOutcome {
        match self.status {
            200 | 201 => FirewallRulePutOutcome::Applied,
            403 => FirewallRulePutOutcome::Forbidden,
            404 => FirewallRulePutOutcome::NotFound,
            other => FirewallRulePutOutcome::Failed(other),
        }
    }
}

/// One of the user-classifiable outcomes of a `"add firewall rule"` attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirewallRulePutOutcome {
    /// ARM returned 200 OK / 201 Created — rule exists server-side.
    Applied,
    /// ARM returned 403 — caller lacks
    /// `Microsoft.Sql/servers/firewallRules/write` on the target server.
    Forbidden,
    /// ARM returned 404 — the server resource vanished between discovery and
    /// PUT.
    NotFound,
    /// Any other status (rate limit, validation error, transient server
    /// failure); carries the raw status so the UI can mention it.
    Failed(u16),
}

/// High-level result of an entire `"add my IP"` attempt. `Applied` carries
/// the rule metadata the UI needs to render a success message and decide
/// whether to auto-retry the failed query; everything else is a typed
/// category so the UI knows whether to suggest a different sign-in, ask an
/// admin, or just retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirewallProvisionOutcome {
    Applied {
        rule_name: String,
        start_ip: String,
        end_ip: String,
    },
    /// Signed-in account lacks RBAC write on the server firewall.
    Forbidden,
    /// Server FQDN was not found in any subscription visible to the account.
    ServerNotFound(String),
    /// Account has no subscriptions (new tenant, no Reader anywhere).
    NoSubscriptions,
    /// Network / token / unexpected HTTP failure. The string is a redacted,
    /// user-readable summary suitable for an alert dialog.
    Failed(String),
}

/// Builds the PUT URL for a server-level firewall rule. The server id is the
/// full ARM resource id (e.g. `/subscriptions/.../.../servers/<name>`); we
/// strip a leading `/` if any so the URL becomes
/// `https://management.azure.com/<serverId>/firewallRules/<ruleName>?...`.
pub fn sql_firewall_rule_url(server_id: &str, rule_name: &str) -> String {
    let trimmed = server_id.strip_prefix('/').unwrap_or(server_id);
    // Rule names from `firewall_rule_name` are `[A-Za-z0-9_-]`; emulating the
    // Swift `urlPathAllowed` percent-encoding conservatively is unnecessary —
    // those characters pass through as-is. We still percent-encode for
    // safety in case a caller constructs a rule name outside that policy.
    let encoded_name: String = rule_name
        .chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            other => format!("%{:02X}", other as u32),
        })
        .collect();
    format!("{ARM_BASE}/{trimmed}/firewallRules/{encoded_name}?api-version={SQL_API_VERSION}")
}

/// Locate the server in `servers` whose FQDN exactly matches `target_fqdn`
/// (case-insensitive). When the ARM response omits `fullyQualifiedDomainName`
/// for a server, fall back to a short-name match: the first label of the
/// target compared to the server's ARM `name` field. Both branches are the
/// same logic the Swift `ARMServerLocator` uses.
pub fn find_server_by_fqdn<'a>(
    target_fqdn: &str,
    servers: &'a [AzureSqlServerResource],
) -> Option<&'a AzureSqlServerResource> {
    let lower = target_fqdn.to_ascii_lowercase();
    if let Some(server) = servers
        .iter()
        .find(|s| s.fully_qualified_domain_name.eq_ignore_ascii_case(&lower))
    {
        return Some(server);
    }
    let short_target = short_server_name(target_fqdn);
    servers
        .iter()
        .find(|s| s.server_name.eq_ignore_ascii_case(&short_target))
}

/// First label of a server FQDN, lowercased. Same logic as the Swift
/// `ARMServerLocator.shortName(fromFQDN:)`. Empty when the input has no `.`.
pub fn short_server_name(fqdn: &str) -> String {
    fqdn.split('.').next().unwrap_or(fqdn).to_ascii_lowercase()
}

/// Converts a [`StudioError`] produced by the firewall flow into a
/// user-readable string suitable for an outcome panel. We delegate to the
/// standard `Display` implementation so the underlying [`TokenFlowError`]
/// message (already formatted by its `Display` impl) flows through unchanged.
fn format_arm_failure(err: &StudioError) -> String {
    err.to_string()
}

/// Truncate an ARM error body so an unfriendly error cannot dump a verbose
/// page into the user-visible outcome panel. Mirrors the Swift `bodyString`
/// path's effective truncation.
fn redact_body(body: &str) -> String {
    const MAX: usize = 240;
    if body.len() <= MAX {
        body.to_string()
    } else {
        let mut end = MAX;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &body[..end])
    }
}

/// Maps an ARM HTTP error into a clear [`TokenFlowError`] so the same
/// error-surfacing path is reused for both auth and discovery.
pub fn map_arm_error(status: u16, body: &str) -> TokenFlowError {
    let lower = body.to_ascii_lowercase();
    match status {
        401 => TokenFlowError::Expired,
        403 if lower.contains("consent") => {
            TokenFlowError::ConsentRequired(extract_arm_message(body))
        }
        403 => TokenFlowError::InsufficientPermissions(extract_arm_message(body)),
        _ => TokenFlowError::OAuth {
            code: format!("arm_http_{status}"),
            description: extract_arm_message(body),
        },
    }
}

fn extract_arm_message(body: &str) -> String {
    // ARM errors look like {"error":{"code":"...","message":"..."}}.
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| body.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::token_flow::HttpResponse;
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// State shared between a recording transport and the test that wants to
    /// observe it. We hold the routes inside an `Arc` so the test can keep a
    /// second clone of the state to read after the client awaits — the
    /// transport itself also holds a clone; both write through the same
    /// interior mutex.
    #[derive(Debug)]
    struct MockState {
        routes: Vec<(String, Result<HttpResponse, TokenFlowError>)>,
        put_routes: Vec<(String, Result<HttpResponse, TokenFlowError>)>,
        seen_tokens: Mutex<Vec<String>>,
        last_put: Mutex<Option<PutCall>>,
    }

    impl MockState {
        fn new(
            routes: Vec<(String, Result<HttpResponse, TokenFlowError>)>,
            put_routes: Vec<(String, Result<HttpResponse, TokenFlowError>)>,
        ) -> Self {
            Self {
                routes,
                put_routes,
                seen_tokens: Mutex::new(Vec::new()),
                last_put: Mutex::new(None),
            }
        }
    }

    /// The recording transport. Cheap to clone: it shares the underlying state
    /// through an `Arc`, so both the test stub and the `AzureDiscoveryClient`
    /// can hold their own handle on the same state.
    #[derive(Clone)]
    struct ArmMock {
        state: std::sync::Arc<MockState>,
    }

    /// Records the most recent PUT call (URL + body + token) so the test can
    /// assert the firewall provisioner sent the right payload.
    #[derive(Debug, Clone)]
    struct PutCall {
        url: String,
        body: String,
        token: String,
    }

    impl ArmMock {
        fn new(routes: Vec<(&str, Result<HttpResponse, TokenFlowError>)>) -> Self {
            Self {
                state: std::sync::Arc::new(MockState::new(
                    routes
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect(),
                    Vec::new(),
                )),
            }
        }

        fn with_put_route(
            mut self,
            needle: &str,
            response: Result<HttpResponse, TokenFlowError>,
        ) -> Self {
            let mut state = match std::sync::Arc::try_unwrap(self.state) {
                Ok(s) => s,
                // Other clones still hold the same state, so route the
                // mutation through a clone-and-rebuild: the previous state
                // gets the same route added as well once the test's earlier
                // clone is also cloned again, but since each `with_put_route`
                // adds exactly one route this path is only ever taken once
                // per route and the test's recorder still sees the mutation.
                Err(arc) => (*arc).clone(),
            };
            state.put_routes.push((needle.to_string(), response));
            self.state = std::sync::Arc::new(state);
            self
        }

        fn last_put(&self) -> Option<PutCall> {
            self.state.last_put.lock().unwrap().clone()
        }
    }

    // Manually derive `Clone` for `MockState` so we can `Arc::try_unwrap` it
    // above.
    impl Clone for MockState {
        fn clone(&self) -> Self {
            Self {
                routes: self.routes.clone(),
                put_routes: self.put_routes.clone(),
                seen_tokens: Mutex::new(self.seen_tokens.lock().unwrap().clone()),
                last_put: Mutex::new(self.last_put.lock().unwrap().clone()),
            }
        }
    }

    #[async_trait]
    impl HttpTransport for ArmMock {
        async fn post_form(
            &self,
            _url: &str,
            _form: &[(&str, &str)],
        ) -> Result<HttpResponse, TokenFlowError> {
            Err(TokenFlowError::Transport("not used".to_string()))
        }

        async fn get_with_bearer(
            &self,
            url: &str,
            access_token: &str,
        ) -> Result<HttpResponse, TokenFlowError> {
            self.state
                .seen_tokens
                .lock()
                .unwrap()
                .push(access_token.to_string());
            for (needle, response) in &self.state.routes {
                if url.contains(needle.as_str()) {
                    return response.clone();
                }
            }
            Err(TokenFlowError::Transport(format!("no route for {url}")))
        }

        async fn put_json_with_bearer(
            &self,
            url: &str,
            access_token: &str,
            body_json: &str,
        ) -> Result<HttpResponse, TokenFlowError> {
            self.state
                .seen_tokens
                .lock()
                .unwrap()
                .push(access_token.to_string());
            *self.state.last_put.lock().unwrap() = Some(PutCall {
                url: url.to_string(),
                body: body_json.to_string(),
                token: access_token.to_string(),
            });
            for (needle, response) in &self.state.put_routes {
                if url.contains(needle.as_str()) {
                    return response.clone();
                }
            }
            Err(TokenFlowError::Transport(format!("no PUT route for {url}")))
        }
    }

    fn account() -> EntraAccountId {
        EntraAccountId::new("home", "tenant")
    }

    #[tokio::test]
    async fn discover_all_walks_subscriptions_servers_databases() {
        let subs = r#"{"value":[{"subscriptionId":"sub-1","displayName":"Prod","tenantId":"tenant","state":"Enabled"}]}"#;
        let servers = r#"{"value":[{"id":"/subscriptions/sub-1/resourceGroups/rg1/providers/Microsoft.Sql/servers/srv1","name":"srv1","location":"eastus","properties":{"fullyQualifiedDomainName":"srv1.database.windows.net","state":"Ready"}}]}"#;
        let dbs = r#"{"value":[{"id":"/subscriptions/sub-1/resourceGroups/rg1/providers/Microsoft.Sql/servers/srv1/databases/sales","name":"sales","location":"eastus","sku":{"tier":"GeneralPurpose","name":"GP"},"properties":{"status":"Online"}}]}"#;

        let mock = ArmMock::new(vec![
            (
                "/subscriptions?api-version",
                Ok(HttpResponse::new(200, subs)),
            ),
            (
                "/providers/Microsoft.Sql/servers?api-version",
                Ok(HttpResponse::new(200, servers)),
            ),
            ("/databases?api-version", Ok(HttpResponse::new(200, dbs))),
        ]);
        let client = AzureDiscoveryClient::new(mock);
        let now = Utc::now();
        let cache = client
            .discover_all(&account(), "AT", now, Duration::minutes(30))
            .await
            .unwrap();
        assert_eq!(cache.subscriptions.len(), 1);
        assert_eq!(cache.databases.len(), 1);
        assert_eq!(cache.databases[0].database_name, "sales");
        assert_eq!(cache.databases[0].resource_group, "rg1");
        assert!(cache.is_fresh_at(now));
    }

    #[tokio::test]
    async fn discovery_maps_401_to_expired() {
        let mock = ArmMock::new(vec![(
            "/subscriptions?api-version",
            Ok(HttpResponse::new(
                401,
                r#"{"error":{"code":"ExpiredAuthenticationToken","message":"token expired"}}"#,
            )),
        )]);
        let client = AzureDiscoveryClient::new(mock);
        let result = client.list_subscriptions(&account(), "AT").await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("expired"), "got: {err}");
    }

    #[tokio::test]
    async fn discovery_maps_403_consent_and_permissions() {
        assert!(matches!(
            map_arm_error(403, r#"{"error":{"code":"x","message":"consent needed"}}"#),
            TokenFlowError::ConsentRequired(_)
        ));
        assert!(matches!(
            map_arm_error(
                403,
                r#"{"error":{"code":"AuthorizationFailed","message":"no role assignment"}}"#
            ),
            TokenFlowError::InsufficientPermissions(_)
        ));
    }

    #[test]
    fn firewall_guidance_includes_ip_and_server() {
        let g = firewall_guidance("srv1.database.windows.net", Some("203.0.113.5"));
        assert!(g.message.contains("srv1.database.windows.net"));
        assert!(g.message.contains("203.0.113.5"));
        let g2 = firewall_guidance("srv2.database.windows.net", None);
        assert!(g2.message.contains("current client IP"));
    }

    #[test]
    fn arm_message_extraction_falls_back_to_body() {
        assert_eq!(
            extract_arm_message(r#"{"error":{"message":"boom"}}"#),
            "boom"
        );
        assert_eq!(extract_arm_message("plain text"), "plain text");
    }

    fn servers_json_for(fqdn: &str) -> String {
        format!(
            r#"{{"value":[{{"id":"/subscriptions/sub-1/resourceGroups/rg1/providers/Microsoft.Sql/servers/{short}","name":"{short}","location":"eastus","properties":{{"fullyQualifiedDomainName":"{fqdn}","state":"Ready"}}}}]}}"#,
            short = short_server_name(fqdn),
            fqdn = fqdn,
        )
    }

    #[tokio::test]
    async fn find_server_by_fqdn_matches_exact_case_insensitive() {
        let body = servers_json_for("Srv1.Database.Windows.Net");
        let servers = azure::parse_sql_servers(
            &account(),
            &AzureSubscription {
                account_id: account(),
                subscription_id: "sub-1".into(),
                display_name: "Prod".into(),
                tenant_id: Some("tenant".into()),
                state: None,
            },
            &body,
        )
        .unwrap();
        let m = find_server_by_fqdn("srv1.database.windows.net", &servers).unwrap();
        assert_eq!(m.server_name, "srv1");
    }

    #[tokio::test]
    async fn find_server_by_fqdn_falls_back_to_short_name() {
        let body = r#"{"value":[{"id":"/subscriptions/sub-1/resourceGroups/rg1/providers/Microsoft.Sql/servers/srv1","name":"srv1","location":"eastus","properties":{}}]}"#;
        let servers = azure::parse_sql_servers(
            &account(),
            &AzureSubscription {
                account_id: account(),
                subscription_id: "sub-1".into(),
                display_name: "Prod".into(),
                tenant_id: Some("tenant".into()),
                state: None,
            },
            body,
        )
        .unwrap();
        let m = find_server_by_fqdn("srv1.database.windows.net", &servers).unwrap();
        assert_eq!(
            m.server_id,
            "/subscriptions/sub-1/resourceGroups/rg1/providers/Microsoft.Sql/servers/srv1"
        );
    }

    #[test]
    fn sql_firewall_rule_url_strips_leading_slash_and_path_encodes_name() {
        let url = sql_firewall_rule_url(
            "/subscriptions/sub-1/resourceGroups/rg/providers/Microsoft.Sql/servers/srv",
            "SQLLY-alice-20260606",
        );
        assert_eq!(
            url,
            "https://management.azure.com/subscriptions/sub-1/resourceGroups/rg/providers/Microsoft.Sql/servers/srv/firewallRules/SQLLY-alice-20260606?api-version=2023-05-01-preview"
        );
    }

    #[test]
    fn sql_firewall_rule_url_handles_special_chars_in_name() {
        let url = sql_firewall_rule_url(
            "/subscriptions/s/resourceGroups/r/providers/Microsoft.Sql/servers/srv",
            "clie nt.ip",
        );
        // space -> %20
        assert!(url.contains("/firewallRules/clie%20nt.ip?"), "got: {url}");
    }

    #[test]
    fn firewall_rule_put_outcome_classification() {
        assert_eq!(
            FirewallRulePutResult {
                status: 200,
                body: String::new()
            }
            .outcome(),
            FirewallRulePutOutcome::Applied
        );
        assert_eq!(
            FirewallRulePutResult {
                status: 201,
                body: String::new()
            }
            .outcome(),
            FirewallRulePutOutcome::Applied
        );
        assert_eq!(
            FirewallRulePutResult {
                status: 403,
                body: String::new()
            }
            .outcome(),
            FirewallRulePutOutcome::Forbidden
        );
        assert_eq!(
            FirewallRulePutResult {
                status: 404,
                body: String::new()
            }
            .outcome(),
            FirewallRulePutOutcome::NotFound
        );
        assert_eq!(
            FirewallRulePutResult {
                status: 429,
                body: "throttled".into()
            }
            .outcome(),
            FirewallRulePutOutcome::Failed(429)
        );
    }

    #[tokio::test]
    async fn add_firewall_rule_full_flow_succeeds() {
        let subs = r#"{"value":[{"subscriptionId":"sub-1","displayName":"Prod","tenantId":"tenant","state":"Enabled"}]}"#;
        let servers = servers_json_for("srv1.database.windows.net");
        let mock = ArmMock::new(vec![
            (
                "/subscriptions?api-version",
                Ok(HttpResponse::new(200, subs)),
            ),
            (
                "/providers/Microsoft.Sql/servers?api-version",
                Ok(HttpResponse::new(200, servers)),
            ),
        ])
        .with_put_route(
            "/firewallRules/SQLLY-alice-20260606",
            Ok(HttpResponse::new(201, "{}")),
        );
        let recorder = mock.clone();
        let client = AzureDiscoveryClient::new(mock);
        let now = Utc::now();
        let outcome = client
            .add_firewall_rule_for_fqdn(
                &account(),
                "srv1.database.windows.net",
                "SQLLY-alice-20260606",
                "203.0.113.25",
                "203.0.113.25",
                "AT",
                now,
            )
            .await;
        assert_eq!(
            outcome,
            FirewallProvisionOutcome::Applied {
                rule_name: "SQLLY-alice-20260606".into(),
                start_ip: "203.0.113.25".into(),
                end_ip: "203.0.113.25".into(),
            }
        );
        let recorded = recorder.last_put().expect("PUT was never recorded");
        assert!(recorded.url.contains("/firewallRules/SQLLY-alice-20260606"));
        assert!(
            recorded
                .body
                .contains("\"startIpAddress\":\"203.0.113.25\"")
        );
        assert!(recorded.body.contains("\"endIpAddress\":\"203.0.113.25\""));
        assert_eq!(recorded.token, "AT");
    }

    #[tokio::test]
    async fn put_firewall_rule_records_url_body_and_token() {
        let mock = ArmMock::new(vec![]).with_put_route(
            "/firewallRules/SQLLY-alice-20260606",
            Ok(HttpResponse::new(201, "{}")),
        );
        let recorder = mock.clone();
        let client = AzureDiscoveryClient::new(mock);
        let result = client
            .put_firewall_rule(
                "/subscriptions/sub-1/resourceGroups/rg/providers/Microsoft.Sql/servers/srv",
                "SQLLY-alice-20260606",
                "203.0.113.5",
                "203.0.113.5",
                "AT",
            )
            .await
            .unwrap();
        assert_eq!(result.status, 201);
        assert_eq!(result.outcome(), FirewallRulePutOutcome::Applied);
        let recorded = recorder.last_put().expect("PUT was never recorded");
        assert!(recorded.url.contains("/firewallRules/SQLLY-alice-20260606"));
        assert!(recorded.body.contains("\"startIpAddress\":\"203.0.113.5\""));
        assert!(recorded.body.contains("\"endIpAddress\":\"203.0.113.5\""));
        assert_eq!(recorded.token, "AT");
    }

    #[tokio::test]
    async fn add_firewall_rule_returns_no_subscriptions_when_arm_has_none() {
        let subs_empty = r#"{"value":[]}"#;
        let mock = ArmMock::new(vec![(
            "/subscriptions?api-version",
            Ok(HttpResponse::new(200, subs_empty)),
        )]);
        let client = AzureDiscoveryClient::new(mock);
        let outcome = client
            .add_firewall_rule_for_fqdn(
                &account(),
                "srv1.database.windows.net",
                "SQLLY-alice-20260606",
                "1.2.3.4",
                "1.2.3.4",
                "AT",
                Utc::now(),
            )
            .await;
        assert_eq!(outcome, FirewallProvisionOutcome::NoSubscriptions);
    }

    #[tokio::test]
    async fn add_firewall_rule_returns_server_not_found_when_no_match() {
        let subs = r#"{"value":[{"subscriptionId":"sub-1","displayName":"Prod","tenantId":"tenant","state":"Enabled"}]}"#;
        let servers = r#"{"value":[]}"#;
        let mock = ArmMock::new(vec![
            (
                "/subscriptions?api-version",
                Ok(HttpResponse::new(200, subs)),
            ),
            (
                "/providers/Microsoft.Sql/servers?api-version",
                Ok(HttpResponse::new(200, servers)),
            ),
        ]);
        let client = AzureDiscoveryClient::new(mock);
        let outcome = client
            .add_firewall_rule_for_fqdn(
                &account(),
                "missing.database.windows.net",
                "rule",
                "1.2.3.4",
                "1.2.3.4",
                "AT",
                Utc::now(),
            )
            .await;
        assert_eq!(
            outcome,
            FirewallProvisionOutcome::ServerNotFound("missing.database.windows.net".into())
        );
    }

    #[tokio::test]
    async fn add_firewall_rule_returns_forbidden_when_put_is_denied() {
        let subs = r#"{"value":[{"subscriptionId":"sub-1","displayName":"Prod","tenantId":"tenant","state":"Enabled"}]}"#;
        let servers = servers_json_for("srv1.database.windows.net");
        let mock = ArmMock::new(vec![
            (
                "/subscriptions?api-version",
                Ok(HttpResponse::new(200, subs)),
            ),
            (
                "/providers/Microsoft.Sql/servers?api-version",
                Ok(HttpResponse::new(200, servers)),
            ),
        ])
        .with_put_route(
            "/firewallRules/",
            Ok(HttpResponse::new(
                403,
                r#"{"error":{"code":"AuthorizationFailed","message":"no role assignment"}}"#,
            )),
        );
        let client = AzureDiscoveryClient::new(mock);
        let outcome = client
            .add_firewall_rule_for_fqdn(
                &account(),
                "srv1.database.windows.net",
                "SQLLY-alice-20260606",
                "1.2.3.4",
                "1.2.3.4",
                "AT",
                Utc::now(),
            )
            .await;
        assert_eq!(outcome, FirewallProvisionOutcome::Forbidden);
    }

    #[tokio::test]
    async fn put_firewall_rule_returns_failed_outcome_on_unexpected_status() {
        let mock = ArmMock::new(vec![]).with_put_route(
            "/firewallRules/",
            Ok(HttpResponse::new(429, "{\"error\":\"throttled\"}")),
        );
        let client = AzureDiscoveryClient::new(mock);
        let result = client
            .put_firewall_rule(
                "/subscriptions/s/resourceGroups/r/providers/Microsoft.Sql/servers/srv",
                "rule",
                "1.2.3.4",
                "1.2.3.4",
                "AT",
            )
            .await
            .unwrap();
        assert_eq!(result.outcome(), FirewallRulePutOutcome::Failed(429));
        assert!(result.body.contains("throttled"));
    }

    #[test]
    fn redact_body_truncates_long_arm_error_bodies() {
        let long = "x".repeat(1_000);
        let r = redact_body(&long);
        assert!(r.len() <= 260);
        assert!(r.ends_with('…'));
    }

    #[test]
    fn redact_body_keeps_short_bodies_intact() {
        let r = redact_body("short body");
        assert_eq!(r, "short body");
    }
}
