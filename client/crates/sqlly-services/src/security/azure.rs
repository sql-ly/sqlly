use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlly_core::{AzureSqlDiscovery, StudioError, StudioResult};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntraAccountId(pub String);

impl EntraAccountId {
    pub fn new(home_account_id: &str, tenant_id: &str) -> Self {
        Self(format!(
            "{}:{}",
            normalize_key(home_account_id),
            normalize_key(tenant_id)
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntraTokenCacheRef {
    pub service: String,
    pub account: String,
}

impl EntraTokenCacheRef {
    pub fn keychain(account_id: &EntraAccountId) -> Self {
        Self {
            service: "sqlly.entra".to_string(),
            account: account_id.0.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntraTokenState {
    Missing,
    Valid,
    ExpiringSoon,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntraAccountProfile {
    pub id: EntraAccountId,
    pub username: String,
    pub tenant_id: String,
    pub home_account_id: String,
    pub display_name: Option<String>,
    pub last_login_at: DateTime<Utc>,
    pub access_token_expires_at: Option<DateTime<Utc>>,
    pub refresh_token_expires_at: Option<DateTime<Utc>>,
    pub token_cache_ref: EntraTokenCacheRef,
}

impl EntraAccountProfile {
    pub fn new(
        username: impl Into<String>,
        tenant_id: impl Into<String>,
        home_account_id: impl Into<String>,
        display_name: Option<String>,
        now: DateTime<Utc>,
    ) -> Self {
        let tenant_id = tenant_id.into();
        let home_account_id = home_account_id.into();
        let id = EntraAccountId::new(&home_account_id, &tenant_id);
        Self {
            token_cache_ref: EntraTokenCacheRef::keychain(&id),
            id,
            username: username.into(),
            tenant_id,
            home_account_id,
            display_name,
            last_login_at: now,
            access_token_expires_at: None,
            refresh_token_expires_at: None,
        }
    }

    pub fn token_state_at(&self, now: DateTime<Utc>) -> EntraTokenState {
        match self.access_token_expires_at {
            None => EntraTokenState::Missing,
            Some(expires_at) if expires_at <= now => EntraTokenState::Expired,
            Some(expires_at) if expires_at <= now + Duration::minutes(5) => {
                EntraTokenState::ExpiringSoon
            }
            Some(_) => EntraTokenState::Valid,
        }
    }
}

/// A selectable tenant for a given username in the multi-tenant selector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantChoice {
    pub account_id: EntraAccountId,
    pub tenant_id: String,
    pub username: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntraAccountRegistry {
    accounts: BTreeMap<EntraAccountId, EntraAccountProfile>,
}

impl EntraAccountRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&mut self, profile: EntraAccountProfile) -> EntraAccountId {
        let id = profile.id.clone();
        self.accounts.insert(id.clone(), profile);
        id
    }

    pub fn remove(&mut self, id: &EntraAccountId) -> Option<EntraAccountProfile> {
        self.accounts.remove(id)
    }

    pub fn get(&self, id: &EntraAccountId) -> Option<&EntraAccountProfile> {
        self.accounts.get(id)
    }

    pub fn accounts(&self) -> Vec<&EntraAccountProfile> {
        self.accounts.values().collect()
    }

    pub fn grouped_by_username(&self) -> BTreeMap<String, Vec<&EntraAccountProfile>> {
        let mut grouped = BTreeMap::<String, Vec<&EntraAccountProfile>>::new();
        for account in self.accounts.values() {
            grouped
                .entry(account.username.to_ascii_lowercase())
                .or_default()
                .push(account);
        }
        grouped
    }

    /// Lists the distinct tenants available for a username (case-insensitive),
    /// each rendered as a [`TenantChoice`]. Drives the multi-tenant selector:
    /// when a single username is registered against several tenants, the UI
    /// must ask which tenant to act on instead of silently picking one.
    pub fn tenants_for_username(&self, username: &str) -> Vec<TenantChoice> {
        let needle = username.to_ascii_lowercase();
        let mut choices: Vec<TenantChoice> = self
            .accounts
            .values()
            .filter(|a| a.username.to_ascii_lowercase() == needle)
            .map(|a| TenantChoice {
                account_id: a.id.clone(),
                tenant_id: a.tenant_id.clone(),
                username: a.username.clone(),
                display_name: a.display_name.clone(),
            })
            .collect();
        choices.sort_by(|a, b| a.tenant_id.cmp(&b.tenant_id));
        choices.dedup_by(|a, b| a.tenant_id == b.tenant_id);
        choices
    }

    /// True when the username maps to more than one tenant and the caller must
    /// prompt for an explicit tenant selection.
    pub fn requires_tenant_selection(&self, username: &str) -> bool {
        self.tenants_for_username(username).len() > 1
    }

    /// Resolves a username + tenant to a concrete account. Returns `None` when
    /// no account matches (the caller should trigger interactive sign-in).
    pub fn resolve_tenant(&self, username: &str, tenant_id: &str) -> Option<&EntraAccountProfile> {
        let needle = username.to_ascii_lowercase();
        self.accounts
            .values()
            .find(|a| a.username.to_ascii_lowercase() == needle && a.tenant_id == tenant_id)
    }

    pub fn mark_login(
        &mut self,
        id: &EntraAccountId,
        now: DateTime<Utc>,
        access_token_expires_at: Option<DateTime<Utc>>,
        refresh_token_expires_at: Option<DateTime<Utc>>,
    ) -> StudioResult<()> {
        let profile = self
            .accounts
            .get_mut(id)
            .ok_or_else(|| StudioError::ObjectNotFound(format!("Entra account {}", id.0)))?;
        profile.last_login_at = now;
        profile.access_token_expires_at = access_token_expires_at;
        profile.refresh_token_expires_at = refresh_token_expires_at;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureSubscription {
    pub account_id: EntraAccountId,
    pub subscription_id: String,
    pub display_name: String,
    pub tenant_id: Option<String>,
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureSqlServerResource {
    pub account_id: EntraAccountId,
    pub subscription_id: String,
    pub subscription_name: Option<String>,
    pub resource_group: String,
    pub server_id: String,
    pub server_name: String,
    pub fully_qualified_domain_name: String,
    pub location: String,
    pub state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureSqlDatabaseResource {
    pub account_id: EntraAccountId,
    pub subscription_id: String,
    pub subscription_name: Option<String>,
    pub resource_group: String,
    pub server_id: String,
    pub server_name: String,
    pub fully_qualified_domain_name: String,
    pub location: String,
    pub database_id: String,
    pub database_name: String,
    pub tier: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureSqlDiscoveryCache {
    pub account_id: EntraAccountId,
    pub fetched_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub subscriptions: Vec<AzureSubscription>,
    pub databases: Vec<AzureSqlDatabaseResource>,
}

impl AzureSqlDiscoveryCache {
    pub fn is_fresh_at(&self, now: DateTime<Utc>) -> bool {
        self.expires_at > now
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureSqlDiscoverySnapshot {
    pub account: EntraAccountProfile,
    pub cache: AzureSqlDiscoveryCache,
}

impl AzureSqlDiscoverySnapshot {
    pub fn to_connection_discoveries(
        &self,
        linked_database_ids: &BTreeSet<String>,
    ) -> Vec<AzureSqlDiscovery> {
        self.cache
            .databases
            .iter()
            .map(|database| AzureSqlDiscovery {
                account_id: self.account.id.0.clone(),
                username: self.account.username.clone(),
                tenant_id: self.account.tenant_id.clone(),
                subscription_id: database.subscription_id.clone(),
                subscription_name: database.subscription_name.clone(),
                resource_group: database.resource_group.clone(),
                server_id: database.server_id.clone(),
                server_name: database.server_name.clone(),
                server_fqdn: database.fully_qualified_domain_name.clone(),
                location: database.location.clone(),
                database_id: database.database_id.clone(),
                database_name: database.database_name.clone(),
                service_tier: database.tier.clone(),
                status: database.status.clone(),
                linked_profile_id: None,
            })
            .map(|mut discovery| {
                if linked_database_ids.contains(&discovery.database_id) {
                    // The caller owns the concrete registered-profile id. A linked
                    // source marker still lets UI disable duplicate creation when it
                    // only has resource IDs available.
                    discovery.linked_profile_id = Some(uuid::Uuid::nil());
                }
                discovery
            })
            .collect()
    }
}

pub fn parse_subscriptions(
    account_id: &EntraAccountId,
    body: &str,
) -> StudioResult<Vec<AzureSubscription>> {
    let parsed: SubscriptionListResponse = parse_json(body, "subscriptions")?;
    Ok(parsed
        .value
        .into_iter()
        .map(|item| AzureSubscription {
            account_id: account_id.clone(),
            subscription_id: item.subscription_id,
            display_name: item.display_name,
            tenant_id: item.tenant_id,
            state: item.state,
        })
        .collect())
}

pub fn parse_sql_servers(
    account_id: &EntraAccountId,
    subscription: &AzureSubscription,
    body: &str,
) -> StudioResult<Vec<AzureSqlServerResource>> {
    let parsed: ServerListResponse = parse_json(body, "SQL servers")?;
    Ok(parsed
        .value
        .into_iter()
        .map(|item| AzureSqlServerResource {
            account_id: account_id.clone(),
            subscription_id: subscription.subscription_id.clone(),
            subscription_name: Some(subscription.display_name.clone()),
            resource_group: resource_group_from_id(&item.id),
            server_id: item.id,
            server_name: item.name,
            fully_qualified_domain_name: item
                .properties
                .fully_qualified_domain_name
                .unwrap_or_default(),
            location: item.location,
            state: item.properties.state,
        })
        .collect())
}

pub fn parse_sql_databases(
    server: &AzureSqlServerResource,
    body: &str,
) -> StudioResult<Vec<AzureSqlDatabaseResource>> {
    let parsed: DatabaseListResponse = parse_json(body, "SQL databases")?;
    Ok(parsed
        .value
        .into_iter()
        .map(|item| AzureSqlDatabaseResource {
            account_id: server.account_id.clone(),
            subscription_id: server.subscription_id.clone(),
            subscription_name: server.subscription_name.clone(),
            resource_group: server.resource_group.clone(),
            server_id: server.server_id.clone(),
            server_name: server.server_name.clone(),
            fully_qualified_domain_name: server.fully_qualified_domain_name.clone(),
            location: item.location.unwrap_or_else(|| server.location.clone()),
            database_id: item.id,
            database_name: item.name,
            tier: item.sku.and_then(|sku| sku.tier.or(sku.name)),
            status: item.properties.status,
        })
        .collect())
}

pub fn build_discovery_cache(
    account_id: EntraAccountId,
    fetched_at: DateTime<Utc>,
    ttl: Duration,
    subscriptions: Vec<AzureSubscription>,
    databases: Vec<AzureSqlDatabaseResource>,
) -> AzureSqlDiscoveryCache {
    AzureSqlDiscoveryCache {
        account_id,
        fetched_at,
        expires_at: fetched_at + ttl,
        subscriptions,
        databases,
    }
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &str, label: &str) -> StudioResult<T> {
    serde_json::from_str(body)
        .map_err(|error| StudioError::Serialization(format!("invalid Azure {label} JSON: {error}")))
}

fn normalize_key(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

/// Claims extracted from an OAuth `id_token` JWT. Mirrors the
/// progress-comment in `AzureTokenService.swift` (the Ruby/Swift equivalent):
/// we read what we need to build an `EntraAccountProfile` and never expose the
/// raw token bytes. Decoded from the JWT payload; the signature is not
/// verified here (the token's provenance is the TLS channel to
/// login.microsoftonline.com, and signing-key validation is not actionable for
/// a public client). Returns partial claims on missing fields — callers fall
/// through to defaults rather than failing sign-in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdTokenClaims {
    /// Tenant ID (`tid`).
    pub tid: String,
    /// Object ID (`oid`) — the user's stable id within the tenant.
    pub oid: String,
    /// Subject (`sub`) — fallback when `oid` is absent.
    pub sub: String,
    /// `preferred_username` claim (preferred form).
    pub preferred_username: String,
    /// `upn` claim (user principal name) — fallback for username.
    pub upn: String,
    /// `email` claim — fallback for username when UPN is absent.
    pub email: String,
    /// `name` claim (display name).
    pub name: Option<String>,
}

impl IdTokenClaims {
    /// Decodes an `id_token` JWT into claims. Returns `None` when the token is
    /// absent, malformed (non-JSON payload), or shorter than two segments —
    /// callers fall back to whatever identity they already have.
    pub fn decode(id_token: &str) -> Option<Self> {
        use base64::Engine;
        let segments: Vec<&str> = id_token.split('.').collect();
        if segments.len() < 2 {
            return None;
        }
        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(segments[1])
            .ok()?;
        let object: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&payload_bytes).ok()?;
        Some(Self {
            tid: str_field(&object, "tid"),
            oid: str_field(&object, "oid"),
            sub: str_field(&object, "sub"),
            preferred_username: str_field(&object, "preferred_username"),
            upn: str_field(&object, "upn"),
            email: str_field(&object, "email"),
            name: object
                .get("name")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }

    /// Returns the username in AAD's normal priority order:
    /// `preferred_username` → `upn` → `email`. Empty string when no claim is
    /// set (callers then keep typing for the user, the same degenerate path
    /// the Swift app takes).
    pub fn username(&self) -> &str {
        if !self.preferred_username.is_empty() {
            &self.preferred_username
        } else if !self.upn.is_empty() {
            &self.upn
        } else {
            &self.email
        }
    }

    /// AAD's home account id is conventionally `<oid>.<tid>`. Falls back to
    /// `sub` when `oid` is absent, then to `oid` itself when neither is set.
    pub fn home_account_id(&self) -> String {
        if !self.oid.is_empty() && !self.tid.is_empty() {
            format!("{}.{}", self.oid, self.tid)
        } else if !self.sub.is_empty() {
            self.sub.clone()
        } else {
            self.oid.clone()
        }
    }
}

fn str_field(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_default()
}

fn resource_group_from_id(id: &str) -> String {
    let parts = id.split('/').collect::<Vec<_>>();
    parts
        .windows(2)
        .find_map(|pair| {
            if pair[0].eq_ignore_ascii_case("resourceGroups") {
                Some(pair[1].to_string())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

#[derive(Debug, Deserialize)]
struct SubscriptionListResponse {
    value: Vec<SubscriptionResponse>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscriptionResponse {
    subscription_id: String,
    display_name: String,
    tenant_id: Option<String>,
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ServerListResponse {
    value: Vec<ServerResponse>,
}

#[derive(Debug, Deserialize)]
struct ServerResponse {
    id: String,
    name: String,
    location: String,
    properties: ServerPropertiesResponse,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ServerPropertiesResponse {
    fully_qualified_domain_name: Option<String>,
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DatabaseListResponse {
    value: Vec<DatabaseResponse>,
}

#[derive(Debug, Deserialize)]
struct DatabaseResponse {
    id: String,
    name: String,
    location: Option<String>,
    sku: Option<DatabaseSkuResponse>,
    properties: DatabasePropertiesResponse,
}

#[derive(Debug, Deserialize)]
struct DatabaseSkuResponse {
    name: Option<String>,
    tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DatabasePropertiesResponse {
    status: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(now: DateTime<Utc>) -> EntraAccountProfile {
        EntraAccountProfile::new(
            "alex@example.com",
            "tenant-a",
            "home-a",
            Some("Alex".to_string()),
            now,
        )
    }

    #[test]
    fn registry_groups_multiple_tenants_by_username() {
        let now = Utc::now();
        let mut registry = EntraAccountRegistry::new();
        registry.upsert(account(now));
        registry.upsert(EntraAccountProfile::new(
            "Alex@Example.com",
            "tenant-b",
            "home-b",
            None,
            now,
        ));

        let grouped = registry.grouped_by_username();
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped["alex@example.com"].len(), 2);
    }

    #[test]
    fn multi_tenant_username_requires_tenant_selection() {
        let now = Utc::now();
        let mut registry = EntraAccountRegistry::new();
        registry.upsert(account(now)); // tenant-a
        registry.upsert(EntraAccountProfile::new(
            "Alex@Example.com",
            "tenant-b",
            "home-b",
            None,
            now,
        ));

        assert!(registry.requires_tenant_selection("alex@example.com"));
        let choices = registry.tenants_for_username("ALEX@example.com");
        assert_eq!(choices.len(), 2);
        // Sorted by tenant id, deduped.
        assert_eq!(choices[0].tenant_id, "tenant-a");
        assert_eq!(choices[1].tenant_id, "tenant-b");

        let resolved = registry
            .resolve_tenant("alex@example.com", "tenant-b")
            .expect("tenant-b account resolves");
        assert_eq!(resolved.tenant_id, "tenant-b");
        assert_eq!(resolved.home_account_id, "home-b");
    }

    #[test]
    fn single_tenant_username_does_not_require_selection() {
        let now = Utc::now();
        let mut registry = EntraAccountRegistry::new();
        registry.upsert(account(now));
        assert!(!registry.requires_tenant_selection("alex@example.com"));
        assert_eq!(registry.tenants_for_username("alex@example.com").len(), 1);
        assert!(
            registry
                .resolve_tenant("alex@example.com", "tenant-z")
                .is_none()
        );
    }

    #[test]
    fn token_state_transitions_are_time_based() {
        let now = Utc::now();
        let mut profile = account(now);
        assert_eq!(profile.token_state_at(now), EntraTokenState::Missing);
        profile.access_token_expires_at = Some(now + Duration::minutes(30));
        assert_eq!(profile.token_state_at(now), EntraTokenState::Valid);
        profile.access_token_expires_at = Some(now + Duration::minutes(3));
        assert_eq!(profile.token_state_at(now), EntraTokenState::ExpiringSoon);
        profile.access_token_expires_at = Some(now - Duration::seconds(1));
        assert_eq!(profile.token_state_at(now), EntraTokenState::Expired);
    }

    #[test]
    fn removal_preserves_unrelated_accounts() {
        let now = Utc::now();
        let mut registry = EntraAccountRegistry::new();
        let first = registry.upsert(account(now));
        let second = registry.upsert(EntraAccountProfile::new(
            "sam@example.com",
            "tenant-b",
            "home-b",
            None,
            now,
        ));

        registry.remove(&first);

        assert!(registry.get(&first).is_none());
        assert!(registry.get(&second).is_some());
    }

    #[test]
    fn parses_subscription_server_database_discovery_json() {
        let account_id = EntraAccountId::new("home", "tenant");
        let subscriptions = parse_subscriptions(
            &account_id,
            r#"{"value":[{"subscriptionId":"sub-1","displayName":"Prod","tenantId":"tenant","state":"Enabled"}]}"#,
        )
        .unwrap();
        let servers = parse_sql_servers(
            &account_id,
            &subscriptions[0],
            r#"{"value":[{"id":"/subscriptions/sub-1/resourceGroups/rg-prod/providers/Microsoft.Sql/servers/sql-prod","name":"sql-prod","location":"eastus","properties":{"fullyQualifiedDomainName":"sql-prod.database.windows.net","state":"Ready"}}]}"#,
        )
        .unwrap();
        let databases = parse_sql_databases(
            &servers[0],
            r#"{"value":[{"id":"/subscriptions/sub-1/resourceGroups/rg-prod/providers/Microsoft.Sql/servers/sql-prod/databases/app","name":"app","location":"eastus","sku":{"name":"S0","tier":"Standard"},"properties":{"status":"Online"}}]}"#,
        )
        .unwrap();

        assert_eq!(servers[0].resource_group, "rg-prod");
        assert_eq!(
            servers[0].fully_qualified_domain_name,
            "sql-prod.database.windows.net"
        );
        assert_eq!(databases[0].tier.as_deref(), Some("Standard"));
        assert_eq!(databases[0].status.as_deref(), Some("Online"));
    }

    #[test]
    fn cache_freshness_uses_expiry() {
        let now = Utc::now();
        let cache = build_discovery_cache(
            EntraAccountId::new("home", "tenant"),
            now,
            Duration::minutes(15),
            Vec::new(),
            Vec::new(),
        );
        assert!(cache.is_fresh_at(now + Duration::minutes(14)));
        assert!(!cache.is_fresh_at(now + Duration::minutes(16)));
    }

    fn make_jwt(payload: &str) -> String {
        use base64::Engine;
        let header = r#"{"alg":"RS256","kid":"k","typ":"JWT"}"#;
        let header_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(header.as_bytes());
        let payload_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
        let signature = "ignore-me";
        format!("{header_b64}.{payload_b64}.{signature}")
    }

    #[test]
    fn id_token_claims_extract_username_tenant_object_id() {
        let jwt = make_jwt(
            r#"{"tid":"tenant-a","oid":"object-a","preferred_username":"alex@contoso.com","name":"Alex"}"#,
        );
        let claims = IdTokenClaims::decode(&jwt).expect("claims");
        assert_eq!(claims.tid, "tenant-a");
        assert_eq!(claims.oid, "object-a");
        assert_eq!(claims.preferred_username, "alex@contoso.com");
        assert_eq!(claims.name.as_deref(), Some("Alex"));
        assert_eq!(claims.email, "");
        assert_eq!(claims.upn, "");
    }

    #[test]
    fn id_token_claims_falls_back_to_email_and_upn_for_username() {
        let jwt = make_jwt(
            r#"{"tid":"tenant-b","oid":"object-b","email":"bob@contoso.com","name":"Bob"}"#,
        );
        let claims = IdTokenClaims::decode(&jwt).expect("claims");
        assert_eq!(claims.tid, "tenant-b");
        // Falls back through preferred_username → upn → email in priority order.
        assert_eq!(claims.username(), "bob@contoso.com");
        assert_eq!(claims.name.as_deref(), Some("Bob"));
    }

    #[test]
    fn id_token_claims_uses_upn_when_email_absent() {
        let jwt = make_jpt_for_upn(r#"{"tid":"tenant","oid":"obj","upn":"carol@x.com"}"#);
        let claims = IdTokenClaims::decode(&jwt).expect("claims");
        assert_eq!(claims.username(), "carol@x.com");
    }

    fn make_jpt_for_upn(payload: &str) -> String {
        make_jwt(payload)
    }

    #[test]
    fn id_token_claims_home_account_id_concatenates_oid_and_tid() {
        let jwt = make_jwt(r#"{"tid":"tenant-x","oid":"object-x"}"#);
        let claims = IdTokenClaims::decode(&jwt).expect("claims");
        assert_eq!(claims.home_account_id(), "object-x.tenant-x");
    }

    #[test]
    fn id_token_claims_home_account_id_falls_back_to_sub() {
        let jwt = make_jwt(r#"{"tid":"tenant-y","sub":"subject-y"}"#);
        let claims = IdTokenClaims::decode(&jwt).expect("claims");
        assert_eq!(claims.home_account_id(), "subject-y");
    }

    #[test]
    fn id_token_claims_rejects_malformed_token() {
        assert!(IdTokenClaims::decode("not-a-jwt").is_none());
        assert!(IdTokenClaims::decode("a.b.c").is_none()); // garbage payload segment
        assert!(IdTokenClaims::decode("a.b").is_none()); // too few segments
    }
}
