//! Azure SQL resource discovery for the gpui app.
//!
//! Walks every registered Entra account, silently refreshes its access token,
//! calls [`AzureDiscoveryClient::discover_all`] to enumerate subscriptions →
//! servers → databases via the ARM REST API, and flattens the results into a
//! single [`Vec<AzureSqlDiscovery>`] the UI can render.

use std::collections::BTreeSet;

use chrono::{Duration, Utc};
use sqlly_core::{AuthenticationType, AzureSqlDiscovery};
use sqlly_services::security::azure::AzureSqlDiscoverySnapshot;
use sqlly_services::security::azure_discovery::AzureDiscoveryClient;
use sqlly_services::security::token_cache::TokenCacheError;

use crate::azure_firewall_flow::build_token_cache;
use crate::azure_http::ReqwestHttpTransport;
use crate::azure_keyring::KeyringTokenCacheStore;
use crate::azure_registry::EntraAccountRegistryStore;
use crate::connection::ConnectionStore;

#[derive(Debug, Default)]
pub struct AzureDiscoveryOutcome {
    pub discoveries: Vec<AzureSqlDiscovery>,
    pub attempted_accounts: usize,
    pub failed_accounts: usize,
}

impl AzureDiscoveryOutcome {
    pub fn failed_completely(&self) -> bool {
        self.attempted_accounts > 0
            && self.failed_accounts == self.attempted_accounts
            && self.discoveries.is_empty()
    }
}

/// Build the set of `server_fqdn|database_name` keys (lowercased) for
/// saved connection profiles that use Entra Browser authentication. Used to
/// mark discovered databases as already linked so the UI can suppress
/// duplicate "Link & Connect" actions.
fn build_linked_keys() -> BTreeSet<String> {
    let store = ConnectionStore::global();
    store
        .list()
        .into_iter()
        .filter(|p| matches!(p.authentication, AuthenticationType::EntraBrowser))
        .filter_map(|p| {
            let db = p.database.as_ref()?;
            Some(format_linked_key(&p.server, db))
        })
        .collect()
}

/// Canonical key for matching a saved profile to a discovered database:
/// `server_fqdn|database_name` (lowercased).
fn format_linked_key(server_fqdn: &str, database_name: &str) -> String {
    format!(
        "{}|{}",
        server_fqdn.to_lowercase(),
        database_name.to_lowercase()
    )
}

/// Discover all Azure SQL databases across every registered Entra account.
///
/// Accounts whose tokens cannot be silently refreshed are skipped (the user
/// would need to re-authenticate interactively). Individual account failures
/// are logged but do not abort the overall discovery. Databases that already
/// have a matching saved connection profile are marked as linked.
pub async fn discover_azure_sql_resources(
    registry: &EntraAccountRegistryStore,
    keyring: KeyringTokenCacheStore,
    transport: ReqwestHttpTransport,
) -> AzureDiscoveryOutcome {
    let accounts = registry.list();
    let attempted_accounts = accounts.len();
    let linked_keys = build_linked_keys();
    let mut all_discoveries = Vec::new();
    let mut failed_accounts = 0;
    let now = Utc::now();
    let ttl = Duration::seconds(3600);

    for account in accounts {
        let cache_ref = account.token_cache_ref.clone();
        let token_cache = build_token_cache(keyring.clone(), transport.clone(), &account.tenant_id);

        let token = match token_cache.valid_access_token(&cache_ref, now).await {
            Ok(t) => t,
            Err(TokenCacheError::InteractiveSignInRequired) => {
                failed_accounts += 1;
                tracing::info!(
                    account = %account.username,
                    "skipping Azure discovery: interactive sign-in required"
                );
                continue;
            }
            Err(e) => {
                failed_accounts += 1;
                tracing::warn!(
                    account = %account.username,
                    error = %e,
                    "Azure discovery: token refresh failed"
                );
                continue;
            }
        };

        let client = AzureDiscoveryClient::new(transport.clone());
        match client
            .discover_all(&account.id, &token.access_token, now, ttl)
            .await
        {
            Ok(cache) => {
                let snapshot = AzureSqlDiscoverySnapshot {
                    account: account.clone(),
                    cache,
                };
                let empty: BTreeSet<String> = BTreeSet::new();
                let mut discoveries = snapshot.to_connection_discoveries(&empty);
                let mut linked_count = 0;
                for d in &mut discoveries {
                    let key = format_linked_key(&d.server_fqdn, &d.database_name);
                    if linked_keys.contains(&key) {
                        d.linked_profile_id = Some(uuid::Uuid::nil());
                        linked_count += 1;
                    }
                }
                tracing::info!(
                    account = %account.username,
                    discovered = discoveries.len(),
                    linked = linked_count,
                    "Azure SQL discovery complete for account"
                );
                all_discoveries.extend(discoveries);
            }
            Err(e) => {
                failed_accounts += 1;
                tracing::warn!(
                    account = %account.username,
                    error = %e,
                    "Azure SQL discovery failed for account"
                );
            }
        }
    }

    AzureDiscoveryOutcome {
        discoveries: all_discoveries,
        attempted_accounts,
        failed_accounts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_failure_requires_every_attempted_account_to_fail() {
        assert!(
            AzureDiscoveryOutcome {
                discoveries: Vec::new(),
                attempted_accounts: 2,
                failed_accounts: 2,
            }
            .failed_completely()
        );
        assert!(
            !AzureDiscoveryOutcome {
                discoveries: Vec::new(),
                attempted_accounts: 2,
                failed_accounts: 1,
            }
            .failed_completely()
        );
        assert!(!AzureDiscoveryOutcome::default().failed_completely());
    }
}
