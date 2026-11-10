//! Offline command implementations: each returns a `String` (SQL, JSON, or a
//! human-readable listing) derived purely from the tested core crates, so the
//! whole surface is unit-testable without a live SQL Server. The `main.rs`
//! dispatcher prints whatever these return.

use anyhow::Result;
use chrono::{Duration, Utc};
use serde::Deserialize;
use sqlly_services::{DumpContent, DumpLayout, DumpOptions};
use std::collections::{BTreeMap, BTreeSet, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum DumpContentArg {
    #[default]
    #[value(name = "structure-and-data")]
    StructureAndData,
    #[value(name = "structure-only")]
    StructureOnly,
    #[value(name = "data-only")]
    DataOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Default)]
pub enum PgFormatArg {
    #[default]
    Custom,
    Plain,
    Tar,
}

impl PgFormatArg {
    pub fn to_pg_format(self) -> sqlly_services::PgFormat {
        match self {
            PgFormatArg::Custom => sqlly_services::PgFormat::Custom,
            PgFormatArg::Plain => sqlly_services::PgFormat::Plain,
            PgFormatArg::Tar => sqlly_services::PgFormat::Tar,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn dump_options_from_args(
    content: DumpContentArg,
    per_object: bool,
    drop_if_exists: bool,
    batch_size: usize,
    no_views: bool,
    no_routines: bool,
    no_triggers: bool,
    no_sequences: bool,
    schemas: Vec<String>,
) -> DumpOptions {
    DumpOptions {
        content: match content {
            DumpContentArg::StructureOnly => DumpContent::StructureOnly,
            DumpContentArg::DataOnly => DumpContent::DataOnly,
            DumpContentArg::StructureAndData => DumpContent::StructureAndData,
        },
        layout: if per_object {
            DumpLayout::FilePerObject
        } else {
            DumpLayout::SingleFile
        },
        drop_if_exists,
        include_views: !no_views,
        include_routines: !no_routines,
        include_sequences: !no_sequences,
        include_triggers: !no_triggers,
        batch_size,
        schemas: if schemas.is_empty() {
            None
        } else {
            Some(schemas)
        },
        ..DumpOptions::default()
    }
}

// ---------------------------------------------------------------------------
// Entra / Azure SQL discovery (sqlly-security)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AzureDiscoveryFixture {
    account: sqlly_services::security::azure::EntraAccountProfile,
    subscriptions_response: String,
    servers_by_subscription: BTreeMap<String, String>,
    databases_by_server_id: BTreeMap<String, String>,
    #[serde(default)]
    linked_database_ids: BTreeSet<String>,
}

pub fn azure_accounts_list(registry_json: &str) -> Result<String> {
    let registry: sqlly_services::security::azure::EntraAccountRegistry =
        serde_json::from_str(registry_json)?;
    Ok(serde_json::to_string_pretty(&registry.accounts())?)
}

pub fn azure_account_add(
    registry_json: Option<&str>,
    username: &str,
    tenant_id: &str,
    home_account_id: &str,
    display_name: Option<&str>,
) -> Result<String> {
    let mut registry = match registry_json {
        Some(json) => serde_json::from_str(json)?,
        None => sqlly_services::security::azure::EntraAccountRegistry::new(),
    };
    let profile = sqlly_services::security::azure::EntraAccountProfile::new(
        username,
        tenant_id,
        home_account_id,
        display_name.map(str::to_string),
        Utc::now(),
    );
    registry.upsert(profile);
    Ok(serde_json::to_string_pretty(&registry)?)
}

pub fn azure_account_remove(registry_json: &str, account_id: &str) -> Result<String> {
    let mut registry: sqlly_services::security::azure::EntraAccountRegistry =
        serde_json::from_str(registry_json)?;
    registry.remove(&sqlly_services::security::azure::EntraAccountId(
        account_id.to_string(),
    ));
    Ok(serde_json::to_string_pretty(&registry)?)
}

pub fn azure_discover_fixture(fixture_json: &str) -> Result<String> {
    let fixture: AzureDiscoveryFixture = serde_json::from_str(fixture_json)?;
    let discoveries = fixture_discoveries(&fixture)?;
    Ok(serde_json::to_string_pretty(&discoveries)?)
}

/// Builds the Servers Explorer tree (account → subscription → server → database)
/// from a discovery fixture and emits the full node list as JSON. This is the
/// CLI smoke for "list discovered SQL servers for a signed-in account": it wires
/// the parsed discoveries through `ConnectionManagementTree::from_azure_discoveries`
/// (the same model the macOS Servers Explorer renders) so node hierarchy,
/// sources, capabilities, and badges are exercised end-to-end offline.
pub fn azure_discover_tree(fixture_json: &str) -> Result<String> {
    let fixture: AzureDiscoveryFixture = serde_json::from_str(fixture_json)?;
    let discoveries = fixture_discoveries(&fixture)?;
    let tree = sqlly_core::ConnectionManagementTree::from_azure_discoveries(&discoveries);
    Ok(serde_json::to_string_pretty(&tree)?)
}

/// Fixture for the unified Servers Explorer smoke: optional manual connection
/// profiles, an optional Azure discovery fixture, and optional filesystem
/// database sources (profile + parsed model snapshot). Any section may be
/// omitted; the tree always emits the three top-level source groups.
#[derive(Debug, Deserialize, Default)]
struct ServersExplorerFixture {
    #[serde(default)]
    connections: Vec<sqlly_core::ConnectionProfile>,
    #[serde(default)]
    recent_profile_ids: Vec<uuid::Uuid>,
    #[serde(default)]
    azure: Option<AzureDiscoveryFixture>,
    #[serde(default)]
    filesystem: Vec<FilesystemSourceFixture>,
}

#[derive(Debug, Deserialize)]
struct FilesystemSourceFixture {
    profile: sqlly_lang::fsmodel::FilesystemDatabaseProfile,
    snapshot: sqlly_lang::fsmodel::SqlModelSnapshot,
    #[serde(default)]
    is_stale: bool,
}

/// Builds the unified Servers Explorer tree from all three source types and
/// emits the full node list as JSON. This is the CLI smoke for the central
/// feature: "sign in with Entra → list servers/databases; manual connections;
/// filesystem databases" all rendered through the same `ServersExplorerTree`
/// model the macOS UI consumes, exercised offline.
pub fn servers_explorer_tree(fixture_json: &str) -> Result<String> {
    let fixture: ServersExplorerFixture = serde_json::from_str(fixture_json)?;

    let mut store = sqlly_core::RegisteredServerStore::new();
    for profile in &fixture.connections {
        store.add_server(sqlly_core::RegisteredServer::new(
            profile.name.clone(),
            profile.clone(),
        ))?;
    }

    let discoveries = match &fixture.azure {
        Some(azure) => fixture_discoveries(azure)?,
        None => Vec::new(),
    };

    let fs_sources: Vec<sqlly_core::FilesystemDatabaseSource<'_>> = fixture
        .filesystem
        .iter()
        .map(|fs| sqlly_core::FilesystemDatabaseSource {
            profile: &fs.profile,
            snapshot: &fs.snapshot,
            is_stale: fs.is_stale,
        })
        .collect();

    let tree = sqlly_core::ServersExplorerTree::build(
        &store,
        &fixture.recent_profile_ids,
        &discoveries,
        &fs_sources,
    );
    Ok(serde_json::to_string_pretty(&tree)?)
}

/// Request for a *live* Servers Explorer build. Unlike [`ServersExplorerFixture`]
/// (which carries a pre-parsed snapshot for each filesystem source), this request
/// carries only the [`FilesystemDatabaseProfile`] for each filesystem database;
/// the engine performs a real on-disk scan of the profile's `root_paths` through
/// the tokenizer indexer to produce each snapshot. This is the "live equivalent"
/// the macOS UI needs to render real `Filesystem Databases` nodes from a
/// user-picked folder instead of a baked fixture.
///
/// Manual connections and the Azure discovery (already authenticated/fetched by
/// the app) are accepted verbatim so a single call yields the full three-source
/// tree.
#[derive(Debug, Deserialize, Default)]
struct ServersExplorerScanRequest {
    #[serde(default)]
    connections: Vec<sqlly_core::ConnectionProfile>,
    #[serde(default)]
    recent_profile_ids: Vec<uuid::Uuid>,
    #[serde(default)]
    azure: Option<AzureDiscoveryFixture>,
    #[serde(default)]
    filesystem: Vec<sqlly_lang::fsmodel::FilesystemDatabaseProfile>,
}

/// Performs a live filesystem scan for each requested profile and emits the
/// unified Servers Explorer tree JSON.
///
/// For every [`FilesystemDatabaseProfile`] the indexer walks the configured
/// root paths, reads/hashes/tokenizes matching files, and folds them into a
/// fresh [`SqlModelSnapshot`]. Because the scan is performed here and now, every
/// resulting filesystem source is marked `is_stale = false`; the tree itself
/// then derives a `ParseError` state for any database whose snapshot carries
/// diagnostics (see `ServersExplorerTree::build`). The manual connections and
/// Azure discovery sources are built identically to [`servers_explorer_tree`].
pub fn servers_explorer_live_scan(request_json: &str) -> Result<String> {
    let request: ServersExplorerScanRequest = serde_json::from_str(request_json)?;

    let mut store = sqlly_core::RegisteredServerStore::new();
    for profile in &request.connections {
        store.add_server(sqlly_core::RegisteredServer::new(
            profile.name.clone(),
            profile.clone(),
        ))?;
    }

    let discoveries = match &request.azure {
        Some(azure) => fixture_discoveries(azure)?,
        None => Vec::new(),
    };

    // Live scan: walk each profile's root paths into a fresh snapshot. A live
    // scan starts from empty prior state so the indexer reads every matching
    // file rather than reusing cached snapshots.
    let scanned: Vec<(
        sqlly_lang::fsmodel::FilesystemDatabaseProfile,
        sqlly_lang::fsmodel::SqlModelSnapshot,
    )> = request
        .filesystem
        .into_iter()
        .map(|profile| {
            let outcome = sqlly_lang::tokenizer::indexer::index_profile(
                &profile,
                &sqlly_lang::fsmodel::SqlModelSnapshot::default(),
                &sqlly_lang::fsmodel::FileIndex::new(),
                &sqlly_lang::tokenizer::indexer::FileSnapshotCache::new(),
            );
            (profile, outcome.snapshot)
        })
        .collect();

    let fs_sources: Vec<sqlly_core::FilesystemDatabaseSource<'_>> = scanned
        .iter()
        .map(|(profile, snapshot)| sqlly_core::FilesystemDatabaseSource {
            profile,
            snapshot,
            is_stale: false,
        })
        .collect();

    let tree = sqlly_core::ServersExplorerTree::build(
        &store,
        &request.recent_profile_ids,
        &discoveries,
        &fs_sources,
    );
    Ok(serde_json::to_string_pretty(&tree)?)
}

/// Envelope for the lazy, per-level Servers Explorer expansion. The macOS app
/// owns Azure auth + HTTP (URLSession + Keychain); it fetches raw ARM JSON for a
/// single level and pipes it here. We parse it through the same
/// `sqlly_services::security::azure` parsers the offline tree uses, then emit the explorer
/// nodes for *only that level* with ids/kinds/actions identical to
/// [`ServersExplorerTree::build`], so the UI renders a lazily-grown tree that is
/// byte-for-byte compatible with the all-at-once tree.
///
/// `level` discriminates which parser + node shape to use. Parent context
/// (account/subscription/server identifiers) is carried explicitly because each
/// level is fetched independently and the parsers need their parent resource.
#[derive(Debug, Deserialize)]
#[serde(tag = "level", rename_all = "kebab-case")]
enum AzureExplorerChildrenRequest {
    /// Children of an `entra-account:{account_id}` node: subscription nodes.
    Subscriptions {
        account_id: String,
        /// Raw ARM `GET /subscriptions?api-version=2022-12-01` response body.
        arm_json: String,
    },
    /// Children of an `entra-sub:{account_id}:{subscription_id}` node: SQL server
    /// nodes.
    Servers {
        account_id: String,
        subscription_id: String,
        #[serde(default)]
        subscription_name: Option<String>,
        /// Raw ARM `GET .../providers/Microsoft.Sql/servers` response body.
        arm_json: String,
    },
    /// Children of an `entra-server:{server_id}` node: SQL database (leaf) nodes.
    Databases {
        account_id: String,
        subscription_id: String,
        #[serde(default)]
        subscription_name: Option<String>,
        resource_group: String,
        server_id: String,
        server_name: String,
        #[serde(default)]
        fully_qualified_domain_name: String,
        location: String,
        /// Database resource ids already linked to a registered connection
        /// profile (so the node is badged `Linked` rather than `Discovered`).
        #[serde(default)]
        linked_database_ids: BTreeSet<String>,
        /// Raw ARM `GET .../servers/{name}/databases` response body.
        arm_json: String,
    },
}

/// Parses one level of raw ARM JSON and emits the Servers Explorer nodes for the
/// children of the expanded parent. This is the lazy counterpart to
/// `azure_discover_tree` / `servers_explorer_tree`: the app calls it once per
/// node expansion instead of building the whole tree up front.
pub fn azure_explorer_children(request_json: &str) -> Result<String> {
    use sqlly_core::servers_explorer::{
        ExplorerAction, ExplorerNodeSource, ServersExplorerNode, ServersExplorerNodeId,
        ServersExplorerNodeKind, ServersExplorerNodeState,
    };
    use sqlly_services::security::azure::{
        self, AzureSqlServerResource, AzureSubscription, EntraAccountId,
    };

    let request: AzureExplorerChildrenRequest = serde_json::from_str(request_json)?;

    let nodes: Vec<ServersExplorerNode> = match request {
        AzureExplorerChildrenRequest::Subscriptions {
            account_id,
            arm_json,
        } => {
            let parent = ServersExplorerNodeId(format!("entra-account:{account_id}"));
            let account = EntraAccountId(account_id.clone());
            let subscriptions = azure::parse_subscriptions(&account, &arm_json)?;
            subscriptions
                .into_iter()
                .map(|sub| {
                    // Show the friendly subscription name only; fall back to
                    // the subscription id (GUID) when no display name exists.
                    let label = if sub.display_name.is_empty() {
                        sub.subscription_id.clone()
                    } else {
                        sub.display_name.clone()
                    };
                    ServersExplorerNode {
                        id: ServersExplorerNodeId(format!(
                            "entra-sub:{}:{}",
                            account_id, sub.subscription_id
                        )),
                        parent: Some(parent.clone()),
                        label,
                        kind: ServersExplorerNodeKind::AzureSubscription,
                        source: ExplorerNodeSource::AzureDiscovered,
                        state: ServersExplorerNodeState::Idle,
                        is_expandable: true,
                        can_connect: false,
                        badges: Vec::new(),
                        actions: vec![ExplorerAction::Refresh],
                        profile_id: None,
                        account_id: Some(account_id.clone()),
                        resource_id: Some(sub.subscription_id.clone()),
                    }
                })
                .collect()
        }
        AzureExplorerChildrenRequest::Servers {
            account_id,
            subscription_id,
            subscription_name,
            arm_json,
        } => {
            let parent = ServersExplorerNodeId(format!("entra-sub:{account_id}:{subscription_id}"));
            let account = EntraAccountId(account_id.clone());
            // parse_sql_servers only reads subscription_id + display_name.
            let subscription = AzureSubscription {
                account_id: account.clone(),
                subscription_id: subscription_id.clone(),
                display_name: subscription_name.clone().unwrap_or_default(),
                tenant_id: None,
                state: None,
            };
            let servers = azure::parse_sql_servers(&account, &subscription, &arm_json)?;
            servers
                .into_iter()
                .map(|server| ServersExplorerNode {
                    id: ServersExplorerNodeId(format!("entra-server:{}", server.server_id)),
                    parent: Some(parent.clone()),
                    label: format!("{} ({})", server.server_name, server.location),
                    kind: ServersExplorerNodeKind::AzureServer,
                    source: ExplorerNodeSource::AzureDiscovered,
                    state: ServersExplorerNodeState::Idle,
                    is_expandable: true,
                    can_connect: false,
                    badges: Vec::new(),
                    actions: vec![ExplorerAction::Refresh],
                    profile_id: None,
                    account_id: Some(account_id.clone()),
                    resource_id: Some(server.server_id.clone()),
                })
                .collect()
        }
        AzureExplorerChildrenRequest::Databases {
            account_id,
            subscription_id,
            subscription_name,
            resource_group,
            server_id,
            server_name,
            fully_qualified_domain_name,
            location,
            linked_database_ids,
            arm_json,
        } => {
            let parent = ServersExplorerNodeId(format!("entra-server:{server_id}"));
            let account = EntraAccountId(account_id.clone());
            let server = AzureSqlServerResource {
                account_id: account,
                subscription_id,
                subscription_name,
                resource_group,
                server_id,
                server_name,
                fully_qualified_domain_name,
                location,
                state: None,
            };
            let databases = azure::parse_sql_databases(&server, &arm_json)?;
            databases
                .into_iter()
                .map(|db| {
                    let linked = linked_database_ids.contains(&db.database_id);
                    let mut badges = vec![if linked { "Linked" } else { "Discovered" }.to_string()];
                    if let Some(tier) = db.tier.as_ref().filter(|t| !t.is_empty()) {
                        badges.push(tier.clone());
                    }
                    if let Some(status) = db.status.as_ref().filter(|s| !s.is_empty()) {
                        badges.push(status.clone());
                    }
                    ServersExplorerNode {
                        id: ServersExplorerNodeId(format!("entra-db:{}", db.database_id)),
                        parent: Some(parent.clone()),
                        label: db.database_name.clone(),
                        kind: ServersExplorerNodeKind::AzureDatabase,
                        source: ExplorerNodeSource::AzureDiscovered,
                        state: ServersExplorerNodeState::Idle,
                        is_expandable: false,
                        can_connect: true,
                        badges,
                        actions: vec![
                            ExplorerAction::Connect,
                            ExplorerAction::NewQuery,
                            ExplorerAction::Properties,
                        ],
                        profile_id: None,
                        account_id: Some(account_id.clone()),
                        resource_id: Some(db.database_id.clone()),
                    }
                })
                .collect()
        }
    };

    Ok(serde_json::to_string_pretty(&nodes)?)
}

/// Shared helper: parse a discovery fixture into the secret-free connection
/// discovery rows consumed by both the flat listing and the tree builder.
fn fixture_discoveries(
    fixture: &AzureDiscoveryFixture,
) -> Result<Vec<sqlly_core::AzureSqlDiscovery>> {
    let account_id = fixture.account.id.clone();
    let subscriptions = sqlly_services::security::azure::parse_subscriptions(
        &account_id,
        &fixture.subscriptions_response,
    )?;
    let mut databases = Vec::new();
    for subscription in &subscriptions {
        let Some(servers_json) = fixture
            .servers_by_subscription
            .get(&subscription.subscription_id)
        else {
            continue;
        };
        let servers = sqlly_services::security::azure::parse_sql_servers(
            &account_id,
            subscription,
            servers_json,
        )?;
        for server in &servers {
            let Some(databases_json) = fixture.databases_by_server_id.get(&server.server_id) else {
                continue;
            };
            databases.extend(sqlly_services::security::azure::parse_sql_databases(
                server,
                databases_json,
            )?);
        }
    }
    let cache = sqlly_services::security::azure::build_discovery_cache(
        account_id,
        Utc::now(),
        Duration::minutes(15),
        subscriptions,
        databases,
    );
    let snapshot = sqlly_services::security::azure::AzureSqlDiscoverySnapshot {
        account: fixture.account.clone(),
        cache,
    };
    Ok(snapshot.to_connection_discoveries(&fixture.linked_database_ids))
}

// ---------------------------------------------------------------------------
// Object Explorer (sqlly-metadata)
// ---------------------------------------------------------------------------

/// JSON of the server-level Object Explorer root nodes.
pub fn explorer_server_nodes() -> Result<String> {
    Ok(serde_json::to_string_pretty(
        &sqlly_intel::metadata::server_root_nodes(),
    )?)
}

/// JSON of the database-level Object Explorer root nodes.
pub fn explorer_database_nodes() -> Result<String> {
    Ok(serde_json::to_string_pretty(
        &sqlly_intel::metadata::database_root_nodes(),
    )?)
}

/// JSON of the child nodes expanded under a named schema folder node.
pub fn explorer_schema_children(parent_name: &str) -> Result<String> {
    let parent = sqlly_intel::metadata::database_root_nodes()
        .into_iter()
        .find(|n| n.name.eq_ignore_ascii_case(parent_name))
        .ok_or_else(|| anyhow::anyhow!("no database root node named \"{parent_name}\""))?;
    Ok(serde_json::to_string_pretty(
        &sqlly_intel::metadata::schema_child_nodes(&parent),
    )?)
}

/// JSON of the Object Explorer nodes matching a case-insensitive filter.
pub fn explorer_filter(query: &str) -> Result<String> {
    let nodes = sqlly_intel::metadata::database_root_nodes();
    Ok(serde_json::to_string_pretty(
        &sqlly_intel::metadata::filter_nodes(&nodes, query),
    )?)
}

pub fn object_properties(kind: &str, schema: Option<&str>, name: &str) -> Result<String> {
    let kind = parse_object_kind(kind)?;
    Ok(serde_json::to_string_pretty(
        &sqlly_intel::metadata::object_properties_spec(kind, schema, name),
    )?)
}

fn parse_object_kind(kind: &str) -> Result<sqlly_core::ObjectKind> {
    let normalized = kind
        .trim()
        .to_ascii_lowercase()
        .replace([' ', '-', '_'], "");
    let kind = match normalized.as_str() {
        "server" => sqlly_core::ObjectKind::Server,
        "database" | "db" => sqlly_core::ObjectKind::Database,
        "table" => sqlly_core::ObjectKind::Table,
        "view" => sqlly_core::ObjectKind::View,
        "procedure" | "proc" | "storedprocedure" => sqlly_core::ObjectKind::StoredProcedure,
        "function" | "scalarfunction" => sqlly_core::ObjectKind::ScalarFunction,
        "tablefunction" | "tablevaluedfunction" => sqlly_core::ObjectKind::TableFunction,
        "aggregatefunction" => sqlly_core::ObjectKind::AggregateFunction,
        other => return Err(anyhow::anyhow!("unknown object kind \"{other}\"")),
    };
    Ok(kind)
}

// ---------------------------------------------------------------------------
// Execution plans (sqlly-plans)
// ---------------------------------------------------------------------------

pub fn plan_estimated_sql(sql: &str) -> String {
    sqlly_services::plans::estimated_plan_sql(sql)
}

pub fn plan_actual_sql(sql: &str) -> String {
    sqlly_services::plans::include_actual_plan_sql(sql)
}

pub fn plan_summarize(showplan_xml: &str) -> Result<String> {
    Ok(serde_json::to_string_pretty(
        &sqlly_services::plans::summarize_showplan_xml(showplan_xml),
    )?)
}

pub fn plan_render(showplan_xml: &str) -> Result<String> {
    Ok(serde_json::to_string_pretty(
        &sqlly_services::plans::render_showplan_xml(showplan_xml)?,
    )?)
}

// ---------------------------------------------------------------------------
// Backup / Restore (sqlly-admin)
// ---------------------------------------------------------------------------

pub fn backup_sql(
    database: &str,
    destination: &str,
    backup_type: sqlly_services::admin::backup::BackupType,
    compress: bool,
    checksum: bool,
    copy_only: bool,
) -> Result<String> {
    use sqlly_services::admin::backup::{BackupManager, BackupOptions};
    let options = BackupOptions {
        database: database.to_string(),
        backup_type,
        destination: destination.to_string(),
        compress,
        checksum,
        copy_only,
        description: None,
    };
    BackupManager::validate(&options).map_err(|e| anyhow::anyhow!(e))?;
    Ok(BackupManager::generate_backup_sql(&options))
}

pub fn restore_sql(database: &str, source: &str, replace: bool, recovery: bool) -> Result<String> {
    use sqlly_services::admin::restore::{RestoreManager, RestoreOptions};
    let options = RestoreOptions {
        database: database.to_string(),
        source: source.to_string(),
        replace,
        recovery,
        file_relocation: Vec::new(),
        point_in_time: None,
    };
    RestoreManager::validate(&options).map_err(|e| anyhow::anyhow!(e))?;
    Ok(RestoreManager::generate_restore_sql(&options))
}

// ---------------------------------------------------------------------------
// Activity Monitor (sqlly-admin)
// ---------------------------------------------------------------------------

pub fn activity_query(kind: &str) -> Result<String> {
    use sqlly_services::admin::activity::ActivityMonitor;
    let sql = match kind {
        "sessions" => ActivityMonitor::sessions_query().to_string(),
        "blocking" => ActivityMonitor::blocking_query().to_string(),
        "expensive" => ActivityMonitor::expensive_queries_query().to_string(),
        "waits" => ActivityMonitor::waits_query().to_string(),
        other => {
            return Err(anyhow::anyhow!(
                "unknown activity kind \"{other}\" (expected sessions|blocking|expensive|waits)"
            ));
        }
    };
    Ok(sql)
}

pub fn activity_kill_sql(session_id: i32) -> String {
    sqlly_services::admin::activity::ActivityMonitor::kill_session_sql(session_id)
}

// ---------------------------------------------------------------------------
// Error logs (sqlly-admin)
// ---------------------------------------------------------------------------

pub fn logs_query(kind: &str) -> Result<String> {
    use sqlly_services::admin::logs::ErrorLogReader;
    let sql = match kind {
        "current" => ErrorLogReader::current_log_query().to_string(),
        "agent" => ErrorLogReader::agent_log_query().to_string(),
        other => {
            return Err(anyhow::anyhow!(
                "unknown log kind \"{other}\" (expected current|agent)"
            ));
        }
    };
    Ok(sql)
}

// ---------------------------------------------------------------------------
// Table data editing (sqlly-admin)
// ---------------------------------------------------------------------------

pub fn table_select_top(schema: &str, table: &str, rows: u32) -> String {
    use sqlly_services::admin::editing::{TableRef, select_top_sql};
    select_top_sql(
        &TableRef {
            schema: schema.to_string(),
            name: table.to_string(),
        },
        rows,
    )
}

pub fn table_edit_preview(
    schema: &str,
    table: &str,
    rows: u32,
    key_columns: &[String],
) -> Result<String> {
    use sqlly_services::admin::editing::{TableKey, TableRef, edit_top_sql};
    let key = if key_columns.is_empty() {
        None
    } else {
        Some(TableKey {
            columns: key_columns.to_vec(),
        })
    };
    let preview = edit_top_sql(
        &TableRef {
            schema: schema.to_string(),
            name: table.to_string(),
        },
        rows,
        key.as_ref(),
    );
    Ok(serde_json::to_string_pretty(&preview)?)
}

// ---------------------------------------------------------------------------
// Query Store / Extended Events / Reports (sqlly-admin advanced_tools)
// ---------------------------------------------------------------------------

pub fn query_store_top(database: &str, limit: u32) -> String {
    sqlly_services::admin::advanced_tools::QueryStore::top_queries_query(database, limit)
}

pub fn query_store_plans(database: &str, query_id: i64) -> String {
    sqlly_services::admin::advanced_tools::QueryStore::plan_history_query(database, query_id)
}

pub fn xevents_query(kind: &str, session: Option<&str>) -> Result<String> {
    use sqlly_services::admin::advanced_tools::ExtendedEvents;
    let sql = match (kind, session) {
        ("sessions", _) => ExtendedEvents::sessions_query(),
        ("events", Some(name)) => ExtendedEvents::session_events_query(name),
        ("ring-buffer", Some(name)) => ExtendedEvents::live_ring_buffer_query(name),
        ("events", None) | ("ring-buffer", None) => {
            return Err(anyhow::anyhow!("--session is required for kind \"{kind}\""));
        }
        (other, _) => {
            return Err(anyhow::anyhow!(
                "unknown xevents kind \"{other}\" (expected sessions|events|ring-buffer)"
            ));
        }
    };
    Ok(sql)
}

pub fn report_query(report: &str) -> Result<String> {
    use sqlly_services::admin::advanced_tools::{StandardReport, standard_report_query};
    let report = match report {
        "disk-usage" => StandardReport::DiskUsage,
        "backup-history" => StandardReport::BackupHistory,
        "blocking" => StandardReport::BlockingSessions,
        other => {
            return Err(anyhow::anyhow!(
                "unknown report \"{other}\" (expected disk-usage|backup-history|blocking)"
            ));
        }
    };
    Ok(standard_report_query(report).to_string())
}

// ---------------------------------------------------------------------------
// Import / Export (sqlly-admin advanced_tools)
// ---------------------------------------------------------------------------

pub fn import_csv_sql(schema: &str, table: &str, file: &str, has_header: bool) -> String {
    use sqlly_services::admin::advanced_tools::{CsvImportPlan, TableTarget};
    CsvImportPlan {
        target: TableTarget::new(schema, table),
        file_path: file.to_string(),
        has_header,
        delimiter: ',',
        batch_size: 1000,
    }
    .bulk_insert_sql()
}

pub fn export_csv_hint(source_sql: &str, file: &str, include_header: bool) -> String {
    use sqlly_services::admin::advanced_tools::CsvExportPlan;
    CsvExportPlan {
        source_sql: source_sql.to_string(),
        file_path: file.to_string(),
        include_header,
        delimiter: ',',
    }
    .command_line_hint()
}

// ---------------------------------------------------------------------------
// SQL Agent (sqlly-agent)
// ---------------------------------------------------------------------------

pub fn agent_list_jobs_query() -> String {
    sqlly_ai::agent::jobs::JobBrowser::list_jobs_query().to_string()
}

pub fn agent_start_job_sql(name: &str) -> String {
    sqlly_ai::agent::jobs::JobWriter::start_job_sql(name)
}

pub fn agent_stop_job_sql(name: &str) -> String {
    sqlly_ai::agent::jobs::JobWriter::stop_job_sql(name)
}

pub fn agent_create_job_sql(name: &str, step_name: &str, command: &str) -> String {
    use sqlly_ai::agent::jobs::{JobDefinition, JobStepDefinition, JobWriter};
    let job = JobDefinition {
        name: name.to_string(),
        enabled: true,
        description: String::new(),
        owner_login: None,
    };
    let step = JobStepDefinition {
        step_name: step_name.to_string(),
        subsystem: "TSQL".to_string(),
        command: command.to_string(),
        database_name: None,
    };
    JobWriter::create_job_sql(&job, &[step])
}

// ---------------------------------------------------------------------------
// Security (sqlly-security)
// ---------------------------------------------------------------------------

pub fn security_query(kind: &str) -> Result<String> {
    use sqlly_services::security::permissions::SecurityCatalog;
    let sql = match kind {
        "logins" => SecurityCatalog::server_logins_query().to_string(),
        "users" => SecurityCatalog::database_users_query().to_string(),
        "roles" => SecurityCatalog::roles_query().to_string(),
        "role-members" => SecurityCatalog::role_membership_query().to_string(),
        "permissions" => SecurityCatalog::permissions_query(None),
        other => {
            return Err(anyhow::anyhow!(
                "unknown security kind \"{other}\" (expected logins|users|roles|role-members|permissions)"
            ));
        }
    };
    Ok(sql)
}

pub fn security_create_login_sql(name: &str, password: &str) -> String {
    sqlly_services::security::permissions::SecurityWriter::create_login_sql(name, password)
}

pub fn security_create_user_sql(user: &str, login: Option<&str>, schema: Option<&str>) -> String {
    sqlly_services::security::permissions::SecurityWriter::create_user_sql(user, login, schema)
}

pub fn security_permission_sql(
    action: &str,
    permission: &str,
    object: &str,
    principal: &str,
) -> Result<String> {
    use sqlly_services::security::permissions::{PermissionAction, SecurityWriter};
    let action = match action {
        "grant" => PermissionAction::Grant,
        "deny" => PermissionAction::Deny,
        "revoke" => PermissionAction::Revoke,
        other => {
            return Err(anyhow::anyhow!(
                "unknown action \"{other}\" (expected grant|deny|revoke)"
            ));
        }
    };
    Ok(SecurityWriter::permission_sql(
        action, permission, object, principal,
    ))
}

// ---------------------------------------------------------------------------
// Table designer (sqlly-admin long_tail)
// ---------------------------------------------------------------------------

/// Builds a CREATE TABLE from a compact column spec: each entry is
/// `name:type[:pk][:identity][:null]`.
pub fn designer_create_table_sql(schema: &str, table: &str, columns: &[String]) -> Result<String> {
    use sqlly_services::admin::long_tail::{DesignerColumn, TableDesignerModel};
    let mut designer_columns = Vec::new();
    let mut primary_key = Vec::new();
    for spec in columns {
        let parts: Vec<&str> = spec.split(':').collect();
        if parts.len() < 2 || parts[0].is_empty() {
            return Err(anyhow::anyhow!(
                "invalid column spec \"{spec}\" (expected name:type[:pk][:identity][:null])"
            ));
        }
        let name = parts[0].to_string();
        let flags: Vec<&str> = parts[2..].to_vec();
        let is_pk = flags.contains(&"pk");
        if is_pk {
            primary_key.push(name.clone());
        }
        designer_columns.push(DesignerColumn {
            name,
            data_type: parts[1].to_string(),
            nullable: flags.contains(&"null"),
            identity: flags.contains(&"identity"),
        });
    }
    let model = TableDesignerModel {
        schema: schema.to_string(),
        table: table.to_string(),
        columns: designer_columns,
        primary_key,
    };
    Ok(model.create_table_sql())
}

pub fn designer_create_index_sql(
    schema: &str,
    table: &str,
    name: &str,
    columns: &[String],
    include_columns: &[String],
    unique: bool,
) -> Result<String> {
    if schema.trim().is_empty() || table.trim().is_empty() || name.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "schema, table, and index name are required"
        ));
    }
    if columns.is_empty() || columns.iter().any(|column| column.trim().is_empty()) {
        return Err(anyhow::anyhow!("at least one index column is required"));
    }
    let model = sqlly_services::admin::long_tail::IndexDesignerModel {
        schema: schema.to_string(),
        table: table.to_string(),
        name: name.to_string(),
        columns: columns.to_vec(),
        include_columns: include_columns.to_vec(),
        unique,
        filter: None,
        fill_factor: None,
        clustered: false,
    };
    Ok(model.create_index_sql())
}

pub fn designer_create_relationship_sql(
    name: &str,
    child_schema: &str,
    child_table: &str,
    child_columns: &[String],
    parent_schema: &str,
    parent_table: &str,
    parent_columns: &[String],
) -> Result<String> {
    if name.trim().is_empty()
        || child_schema.trim().is_empty()
        || child_table.trim().is_empty()
        || parent_schema.trim().is_empty()
        || parent_table.trim().is_empty()
    {
        return Err(anyhow::anyhow!(
            "relationship name, child table, and parent table are required"
        ));
    }
    if child_columns.is_empty()
        || parent_columns.is_empty()
        || child_columns.len() != parent_columns.len()
        || child_columns.iter().any(|column| column.trim().is_empty())
        || parent_columns.iter().any(|column| column.trim().is_empty())
    {
        return Err(anyhow::anyhow!(
            "child and parent columns must be non-empty and have matching counts"
        ));
    }
    let model = sqlly_services::admin::long_tail::RelationshipDesignerModel {
        name: name.to_string(),
        child_schema: child_schema.to_string(),
        child_table: child_table.to_string(),
        child_columns: child_columns.to_vec(),
        parent_schema: parent_schema.to_string(),
        parent_table: parent_table.to_string(),
        parent_columns: parent_columns.to_vec(),
    };
    Ok(model.add_foreign_key_sql())
}

// ---------------------------------------------------------------------------
// Schema compare (sqlly-admin advanced_tools)
// ---------------------------------------------------------------------------

/// Compares two schema-object JSON files and emits the migration diff script.
pub fn schema_compare_script(source_json: &str, target_json: &str) -> Result<String> {
    use sqlly_services::admin::advanced_tools::{SchemaObject, compare_schema, script_diff};
    let source: Vec<SchemaObjectDto> = serde_json::from_str(source_json)?;
    let target: Vec<SchemaObjectDto> = serde_json::from_str(target_json)?;
    let source: Vec<SchemaObject> = source.into_iter().map(Into::into).collect();
    let target: Vec<SchemaObject> = target.into_iter().map(Into::into).collect();
    let differences = compare_schema(&source, &target);
    Ok(script_diff(&differences))
}

/// Serializable mirror of `SchemaObject` (which is not itself `Deserialize`).
#[derive(serde::Deserialize)]
pub struct SchemaObjectDto {
    pub schema: String,
    pub name: String,
    pub kind: String,
    pub definition: String,
}

impl From<SchemaObjectDto> for sqlly_services::admin::advanced_tools::SchemaObject {
    fn from(dto: SchemaObjectDto) -> Self {
        Self {
            schema: dto.schema,
            name: dto.name,
            kind: dto.kind,
            definition: dto.definition,
        }
    }
}

// ---------------------------------------------------------------------------
// Registered Servers (sqlly-core) — persisted to a JSON store file
// ---------------------------------------------------------------------------

use sqlly_core::{RegisteredServer, RegisteredServerStore, ServerGroup};

fn load_store(path: &str) -> Result<RegisteredServerStore> {
    match std::fs::read_to_string(path) {
        Ok(json) => Ok(serde_json::from_str(&json)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RegisteredServerStore::new()),
        Err(e) => Err(e.into()),
    }
}

fn save_store(path: &str, store: &RegisteredServerStore) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(store)?)?;
    Ok(())
}

pub fn registered_add_group(path: &str, name: &str) -> Result<String> {
    let mut store = load_store(path)?;
    let id = store
        .add_group(ServerGroup::new(name, None))
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    save_store(path, &store)?;
    Ok(format!("added group \"{name}\" ({})", id.0))
}

pub fn registered_add_server(path: &str, display_name: &str, server: &str) -> Result<String> {
    let mut store = load_store(path)?;
    let profile = ConnectionProfileHelper::sql_login(server);
    let registered = RegisteredServer::new(display_name, profile);
    let id = store
        .add_server(registered)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    save_store(path, &store)?;
    Ok(format!("registered \"{display_name}\" -> {server} ({id})"))
}

pub fn registered_list(path: &str) -> Result<String> {
    let store = load_store(path)?;
    Ok(serde_json::to_string_pretty(&store)?)
}

pub fn registered_search(path: &str, query: &str) -> Result<String> {
    let store = load_store(path)?;
    let matches: Vec<_> = store.search(query).into_iter().cloned().collect();
    Ok(serde_json::to_string_pretty(&matches)?)
}

/// Minimal helper to build an in-memory-secret SQL login profile for a server,
/// used when registering a server from the CLI.
struct ConnectionProfileHelper;

impl ConnectionProfileHelper {
    fn sql_login(server: &str) -> sqlly_core::ConnectionProfile {
        use sqlly_core::{SecretProvider, SecretRef};
        sqlly_core::ConnectionProfile::sql_login(
            server,
            server,
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        )
    }
}

// ---------------------------------------------------------------------------
// Template Explorer (sqlly-scripting)
// ---------------------------------------------------------------------------

pub fn templates_list() -> String {
    use sqlly_services::scripting::TemplateExplorer;
    let explorer = TemplateExplorer::with_built_in();
    let mut out = String::new();
    for category in explorer.categories() {
        out.push_str(&category.name);
        out.push('\n');
        for template in &category.templates {
            out.push_str(&format!(
                "  - {} — {}\n",
                template.name, template.description
            ));
        }
    }
    out.trim_end().to_string()
}

pub fn templates_search(query: &str) -> String {
    use sqlly_services::scripting::TemplateExplorer;
    let explorer = TemplateExplorer::with_built_in();
    let matches = explorer.search(query);
    if matches.is_empty() {
        return format!("(no templates match \"{query}\")");
    }
    matches
        .iter()
        .map(|t| format!("{} [{}] — {}", t.name, t.category, t.description))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn templates_render(name: &str, values: HashMap<String, String>) -> Result<String> {
    use sqlly_services::scripting::TemplateExplorer;
    let explorer = TemplateExplorer::with_built_in();
    let template = explorer
        .find(name)
        .ok_or_else(|| anyhow::anyhow!("no template named \"{name}\""))?;
    Ok(template.render(&values))
}

// ---------------------------------------------------------------------------
// Crash recovery (sqlly-core::recovery)
// ---------------------------------------------------------------------------

/// Input tab shape the UI serialises for autosave. Mirrors `RecoveredTab`
/// without requiring the UI to know the snapshot envelope/versioning.
#[derive(serde::Deserialize)]
pub struct RecoveryTabInput {
    /// Optional stable identity supplied by the UI so a restored tab keeps the
    /// same id (and therefore the same sidecar metadata key) across relaunches.
    #[serde(default)]
    pub id: Option<uuid::Uuid>,
    pub title: String,
    pub sql: String,
    #[serde(default)]
    pub file_path: Option<String>,
    #[serde(default = "default_true")]
    pub dirty: bool,
}

fn default_true() -> bool {
    true
}

/// Wraps the UI's tab list into a canonical, versioned `RecoverySnapshot` JSON
/// (the format the app writes to disk on its autosave timer).
pub fn recovery_snapshot(tabs_json: &str) -> Result<String> {
    use sqlly_core::{RecoveredTab, RecoverySnapshot};
    let inputs: Vec<RecoveryTabInput> = serde_json::from_str(tabs_json)?;
    let tabs = inputs
        .into_iter()
        .map(|i| {
            let mut tab = RecoveredTab::new(i.title, i.sql);
            if let Some(id) = i.id {
                tab.id = id;
            }
            if let Some(path) = i.file_path {
                tab = tab.with_file(path);
            }
            if !i.dirty {
                tab = tab.saved();
            }
            tab
        })
        .collect();
    RecoverySnapshot::new(tabs)
        .to_json()
        .map_err(|e| anyhow::anyhow!(e.to_string()))
}

/// Parses a stored snapshot and lists the tabs worth restoring (dirty buffers).
pub fn recovery_recoverable(snapshot_json: &str) -> Result<String> {
    use sqlly_core::RecoverySnapshot;
    let snapshot =
        RecoverySnapshot::from_json(snapshot_json).map_err(|e| anyhow::anyhow!(e.to_string()))?;
    let recoverable = snapshot.recoverable_tabs();
    Ok(serde_json::to_string_pretty(&recoverable)?)
}

/// Emits the default application preferences as JSON (what the Preferences panel
/// shows on first launch / "Restore Defaults").
pub fn preferences_default() -> Result<String> {
    sqlly_core::AppPreferences::default()
        .to_json()
        .map_err(|e| anyhow::anyhow!(e))
}

/// Normalizes a (possibly hand-edited or partial) preferences JSON document,
/// clamping out-of-range fields and filling missing ones with defaults.
pub fn preferences_normalize(json: &str) -> Result<String> {
    sqlly_core::AppPreferences::from_json(json)
        .and_then(|p| p.to_json())
        .map_err(|e| anyhow::anyhow!(e))
}

/// Computes the virtualization slice (page/offset/len) for a requested page of a
/// result set, given the per-page size and total row count. The result grid
/// calls this to fetch only the visible window of large results.
pub fn result_window_slice(page_size: usize, total_rows: usize, page: usize) -> Result<String> {
    let window = sqlly_core::ResultWindow::new(page_size, total_rows);
    let slice = window.slice_for_page(page);
    Ok(serde_json::json!({
        "page_size": window.page_size,
        "total_rows": window.total_rows,
        "page_count": window.page_count(),
        "slice": { "page": slice.page, "offset": slice.offset, "len": slice.len },
    })
    .to_string())
}

/// Evaluates the connection safety policy against a SQL batch and returns the
/// effective-SQL preview as JSON, without executing anything. The macOS query
/// toolbar consumes this to show a safety badge / rollback banner / inline block
/// reason. `environment` is one of LOCAL|DEV|TEST|STAGING|PRODUCTION.
pub fn safety_preview(
    sql: &str,
    environment: &str,
    block_all_but_select: bool,
    auto_wrap_rollback: bool,
) -> Result<String> {
    use sqlly_core::{ConnectionEnvironment, SecretProvider, SecretRef};
    let env: ConnectionEnvironment =
        serde_json::from_str(&format!("\"{}\"", environment.trim().to_uppercase()))
            .map_err(|_| anyhow::anyhow!("unknown environment: {environment}"))?;
    let mut profile = sqlly_core::ConnectionProfile::sql_login(
        "safety-preview",
        "localhost",
        "preview",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: "unused".to_string(),
        },
    );
    profile.environment = env;
    profile.block_all_but_select = block_all_but_select;
    profile.auto_wrap_rollback = auto_wrap_rollback;

    let effective = sqlly_intel::query::safety::preview_effective_sql(&profile, sql);
    let classification = sqlly_intel::query::safety::classify_select_only(sql);
    let blocked_span = classification
        .blocked_span
        .map(|s| serde_json::json!({ "start": s.start, "end": s.end }));
    Ok(serde_json::json!({
        "environment": environment.trim().to_uppercase(),
        "block_all_but_select": block_all_but_select,
        "auto_wrap_rollback": auto_wrap_rollback,
        "allowed": effective.allowed,
        "reason": effective.reason,
        "wrapped_in_rollback": effective.wrapped_in_rollback,
        "select_only": classification.select_only,
        "blocked_span": blocked_span,
        "effective_sql": effective.sql,
    })
    .to_string())
}

/// Parse/validate T-SQL without executing it. Runs the tokenizer's lightweight
/// diagnostic pass (balanced parens/brackets, unterminated strings/comments,
/// etc.) and returns a JSON document the editor can surface as a parse/validate-
/// only result without touching a live server.
pub fn parse_sql(sql: &str) -> Result<String> {
    let diagnostics = sqlly_intel::query::tokenizer::parse_diagnostics(sql);
    Ok(serde_json::json!({
        "ok": diagnostics.is_empty(),
        "diagnostic_count": diagnostics.len(),
        "diagnostics": diagnostics,
    })
    .to_string())
}

/// Renders a [`sqlly_core::CellValue`] to the same display string the macOS
/// grid uses (`ResultCellValue.displayText`), so spilled rows page straight into
/// the grid's `[[String]]` model without re-deriving formatting.
pub fn cell_display(cell: &sqlly_core::CellValue) -> String {
    use sqlly_core::CellValue;
    match cell {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => {
            if *b {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(x) => x.to_string(),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(bytes) => {
            let mut out = String::with_capacity(2 + bytes.len() * 2);
            out.push_str("0x");
            for b in bytes {
                out.push_str(&format!("{b:02X}"));
            }
            out
        }
    }
}

/// Reads one page of rows back from a spill file written by `query --spill`,
/// seeking straight to the window so peak memory is O(len), not O(total rows).
pub fn spill_result_page(spill: &std::path::Path, offset: usize, len: usize) -> Result<String> {
    let mut reader = sqlly_core::SpillReader::open(spill)
        .map_err(|e| anyhow::anyhow!("open spill {}: {e}", spill.display()))?;
    let rows: Vec<Vec<String>> = reader
        .page(offset, len)
        .map_err(|e| anyhow::anyhow!("read page: {e}"))?
        .iter()
        .map(|row| row.iter().map(cell_display).collect())
        .collect();
    Ok(serde_json::json!({
        "offset": offset,
        "len": rows.len(),
        "total_rows": reader.row_count(),
        "rows": rows,
    })
    .to_string())
}
/// the connection editor's "preview / validate" affordance for SQL, Azure SQL,
/// and Microsoft Entra profiles.
pub fn connection_preview(
    name: &str,
    server: &str,
    database: Option<&str>,
    auth: &str,
) -> Result<String> {
    use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};
    let mut profile = match auth {
        "sql" => ConnectionProfile::sql_login(
            name,
            server,
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        ),
        "entra-browser" => {
            ConnectionProfile::azure_sql_entra_browser(name, server, database.unwrap_or("master"))
        }
        "entra-device-code" => ConnectionProfile::azure_sql_entra_device_code(
            name,
            server,
            database.unwrap_or("master"),
        ),
        other => {
            return Err(anyhow::anyhow!(
                "unknown auth mode \"{other}\" (expected sql|entra-browser|entra-device-code)"
            ));
        }
    };
    if let Some(db) = database {
        profile.database = Some(db.to_string());
    }
    Ok(serde_json::to_string_pretty(&profile.preview())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explorer_nodes_are_json() {
        assert!(explorer_server_nodes().unwrap().contains('['));
        assert!(explorer_database_nodes().unwrap().contains('['));
    }

    #[test]
    fn object_properties_emit_dialog_spec_json() {
        let table = object_properties("table", Some("dbo"), "Orders").unwrap();
        assert!(table.contains("dbo.Orders"));
        assert!(table.contains("Columns"));
        assert!(table.contains("Indexes"));

        let database = object_properties("database", None, "Sales").unwrap();
        assert!(database.contains("Files"));
        assert!(object_properties("bogus", None, "x").is_err());
    }

    #[test]
    fn plan_sql_wraps_statements() {
        assert!(
            plan_estimated_sql("SELECT 1")
                .to_uppercase()
                .contains("SHOWPLAN")
        );
        assert!(
            plan_actual_sql("SELECT 1")
                .to_uppercase()
                .contains("STATISTICS")
        );
    }

    #[test]
    fn plan_render_returns_graph_layout_json() {
        let xml = r#"<ShowPlanXML><StmtSimple StatementText="SELECT 1" StatementType="SELECT" StatementSubTreeCost="2"><RelOp NodeId="1" LogicalOp="Select" PhysicalOp="Constant Scan" EstimateRows="1" EstimatedTotalSubtreeCost="2" /></StmtSimple></ShowPlanXML>"#;
        let rendered = plan_render(xml).unwrap();

        assert!(rendered.contains("Constant Scan"));
        assert!(rendered.contains("cost_percent"));
        assert!(rendered.contains("\"nodes\""));
    }

    #[test]
    fn backup_sql_includes_options() {
        let sql = backup_sql(
            "Sales",
            "/var/backups/sales.bak",
            sqlly_services::admin::backup::BackupType::Full,
            true,
            true,
            false,
        )
        .unwrap();
        assert!(sql.contains("BACKUP DATABASE"));
        assert!(sql.contains("COMPRESSION"));
        assert!(sql.contains("CHECKSUM"));
    }

    #[test]
    fn backup_sql_rejects_empty_database() {
        assert!(
            backup_sql(
                "",
                "x",
                sqlly_services::admin::backup::BackupType::Full,
                false,
                false,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn restore_sql_includes_replace() {
        let sql = restore_sql("Sales", "/var/backups/sales.bak", true, true).unwrap();
        assert!(sql.contains("RESTORE DATABASE"));
        assert!(sql.contains("REPLACE"));
    }

    #[test]
    fn activity_queries_dispatch() {
        assert!(
            activity_query("sessions")
                .unwrap()
                .to_lowercase()
                .contains("select")
        );
        assert!(activity_query("blocking").is_ok());
        assert!(activity_query("nope").is_err());
        assert!(activity_kill_sql(57).to_uppercase().contains("KILL"));
    }

    #[test]
    fn table_select_and_preview() {
        assert!(table_select_top("dbo", "Orders", 200).contains("TOP (200)"));
        let preview = table_edit_preview("dbo", "Orders", 200, &[]).unwrap();
        assert!(preview.contains("read-only") || preview.contains("warnings"));
        let with_key = table_edit_preview("dbo", "Orders", 200, &["id".to_string()]).unwrap();
        assert!(with_key.contains("sql"));
    }

    #[test]
    fn query_store_and_xevents_and_reports() {
        assert!(query_store_top("Sales", 10).contains("query_store_query"));
        assert!(query_store_plans("Sales", 42).contains("query_store_plan"));
        assert!(
            xevents_query("sessions", None)
                .unwrap()
                .contains("server_event_sessions")
        );
        assert!(xevents_query("events", Some("xe")).unwrap().contains("xe"));
        assert!(xevents_query("events", None).is_err());
        assert!(
            report_query("disk-usage")
                .unwrap()
                .contains("database_files")
        );
        assert!(report_query("bogus").is_err());
    }

    #[test]
    fn import_export_helpers() {
        assert!(import_csv_sql("dbo", "Orders", "/tmp/o.csv", true).contains("BULK INSERT"));
        assert!(export_csv_hint("SELECT 1", "/tmp/o.csv", true).contains("bcp"));
    }

    #[test]
    fn agent_job_sql() {
        assert!(agent_list_jobs_query().contains("sysjobs"));
        assert!(agent_start_job_sql("Nightly").contains("sp_start_job"));
        assert!(agent_stop_job_sql("Nightly").contains("sp_stop_job"));
        let create = agent_create_job_sql("Nightly", "step1", "SELECT 1");
        assert!(create.contains("sp_add_job"));
        assert!(create.contains("sp_add_jobstep"));
    }

    #[test]
    fn security_queries_and_writes() {
        assert!(
            security_query("logins")
                .unwrap()
                .contains("server_principals")
        );
        assert!(
            security_query("permissions")
                .unwrap()
                .contains("database_permissions")
        );
        assert!(security_query("bogus").is_err());
        assert!(security_create_login_sql("app", "p@ss").contains("CREATE LOGIN"));
        assert!(
            security_create_user_sql("app", Some("login"), Some("dbo")).contains("DEFAULT_SCHEMA")
        );
        assert!(
            security_permission_sql("grant", "SELECT", "dbo.Orders", "app")
                .unwrap()
                .contains("GRANT SELECT")
        );
        assert!(security_permission_sql("nope", "SELECT", "o", "p").is_err());
    }

    #[test]
    fn designer_builds_create_table() {
        let sql = designer_create_table_sql(
            "dbo",
            "Orders",
            &[
                "id:int:pk:identity".to_string(),
                "name:nvarchar(100):null".to_string(),
            ],
        )
        .unwrap();
        assert!(sql.contains("CREATE TABLE"));
        assert!(sql.contains("IDENTITY(1,1)"));
        assert!(sql.contains("PRIMARY KEY"));
        assert!(designer_create_table_sql("dbo", "x", &["bad".to_string()]).is_err());
    }

    #[test]
    fn designer_builds_index_and_relationship() {
        let index = designer_create_index_sql(
            "dbo",
            "Orders",
            "IX_Orders_Customer",
            &["CustomerId".to_string()],
            &["Total".to_string()],
            true,
        )
        .unwrap();
        assert!(index.contains("CREATE UNIQUE INDEX"));
        assert!(index.contains("INCLUDE ([Total])"));
        assert!(designer_create_index_sql("dbo", "Orders", "IX", &[], &[], false).is_err());

        let relationship = designer_create_relationship_sql(
            "FK_Orders_Customers",
            "dbo",
            "Orders",
            &["CustomerId".to_string()],
            "dbo",
            "Customers",
            &["CustomerId".to_string()],
        )
        .unwrap();
        assert!(relationship.contains("ALTER TABLE [dbo].[Orders]"));
        assert!(relationship.contains("REFERENCES [dbo].[Customers]"));
        assert!(
            designer_create_relationship_sql(
                "FK",
                "dbo",
                "Orders",
                &["CustomerId".to_string()],
                "dbo",
                "Customers",
                &[],
            )
            .is_err()
        );
    }

    #[test]
    fn schema_compare_emits_diff() {
        let source = r#"[{"schema":"dbo","name":"Orders","kind":"table","definition":"CREATE TABLE dbo.Orders(id int)"}]"#;
        let target = r#"[]"#;
        let script = schema_compare_script(source, target).unwrap();
        assert!(script.contains("Missing in target"));
    }

    #[test]
    fn registered_servers_round_trip_through_file() {
        let dir = std::env::temp_dir();
        let path = dir
            .join(format!("sqlly-reg-{}.json", uuid_like()))
            .to_string_lossy()
            .to_string();
        let _ = std::fs::remove_file(&path);

        assert!(
            registered_add_group(&path, "Production")
                .unwrap()
                .contains("Production")
        );
        assert!(
            registered_add_server(&path, "rpt", "rpt.contoso.com")
                .unwrap()
                .contains("rpt")
        );
        assert!(registered_list(&path).unwrap().contains("rpt.contoso.com"));
        assert!(registered_search(&path, "contoso").unwrap().contains("rpt"));
        assert!(registered_search(&path, "missing").unwrap().trim() == "[]");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn azure_registry_and_fixture_discovery_are_json_driven() {
        let registry =
            azure_account_add(None, "dba@contoso.com", "tenant-a", "home-a", Some("DBA")).unwrap();
        assert!(
            azure_accounts_list(&registry)
                .unwrap()
                .contains("dba@contoso.com")
        );

        let fixture = serde_json::json!({
            "account": {
                "id": "home-a:tenant-a",
                "username": "dba@contoso.com",
                "tenant_id": "tenant-a",
                "home_account_id": "home-a",
                "display_name": "DBA",
                "last_login_at": "2026-01-01T00:00:00Z",
                "access_token_expires_at": null,
                "refresh_token_expires_at": null,
                "token_cache_ref": { "service": "sqlly.entra", "account": "home-a:tenant-a" }
            },
            "subscriptions_response": "{\"value\":[{\"subscriptionId\":\"sub-1\",\"displayName\":\"Prod\",\"tenantId\":\"tenant-a\",\"state\":\"Enabled\"}]}",
            "servers_by_subscription": {
                "sub-1": "{\"value\":[{\"id\":\"/subscriptions/sub-1/resourceGroups/rg-prod/providers/Microsoft.Sql/servers/sql-prod\",\"name\":\"sql-prod\",\"location\":\"eastus\",\"properties\":{\"fullyQualifiedDomainName\":\"sql-prod.database.windows.net\",\"state\":\"Ready\"}}]}"
            },
            "databases_by_server_id": {
                "/subscriptions/sub-1/resourceGroups/rg-prod/providers/Microsoft.Sql/servers/sql-prod": "{\"value\":[{\"id\":\"/subscriptions/sub-1/resourceGroups/rg-prod/providers/Microsoft.Sql/servers/sql-prod/databases/app\",\"name\":\"app\",\"location\":\"eastus\",\"sku\":{\"name\":\"S0\",\"tier\":\"Standard\"},\"properties\":{\"status\":\"Online\"}}]}"
            }
        })
        .to_string();
        let discovered = azure_discover_fixture(&fixture).unwrap();
        assert!(discovered.contains("sql-prod.database.windows.net"));
        assert!(discovered.contains("Standard"));

        // CLI smoke: the same fixture wires a full Servers Explorer tree with
        // account → subscription → server → database nodes.
        let tree = azure_discover_tree(&fixture).unwrap();
        assert!(tree.contains("\"AzureAccount\""));
        assert!(tree.contains("\"AzureSubscription\""));
        assert!(tree.contains("\"AzureSqlServer\""));
        assert!(tree.contains("\"AzureDatabase\""));
        assert!(tree.contains("dba@contoso.com"));
        assert!(tree.contains("sql-prod"));
        assert!(tree.contains("\"app\""));
        // No secret material in the discovered tree.
        assert!(!tree.contains("password"));
    }

    #[test]
    fn servers_explorer_tree_combines_all_three_sources() {
        // A manual SQL-login connection (secret-free SecretRef), an Azure
        // discovery fixture, and a filesystem database source, all folded into
        // the one unified Servers Explorer tree.
        let manual = serde_json::json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "name": "Reporting",
            "server": "rpt.contoso.com",
            "port": 1433,
            "database": "Reports",
            "environment": "PRODUCTION",
            "client": "Contoso",
            "project": "Billing",
            "authentication": { "SqlLogin": {
                "username": "appuser",
                "secret_ref": { "provider": "MacOsKeychain", "key": "sqlly/appuser" }
            }},
            "encrypt": true,
            "trust_server_certificate": false,
            "timeout_seconds": 30,
            "application_name": "SQLLY",
            "auto_wrap_rollback": true,
            "block_all_but_select": true,
            "rollback_policy_defaulted": false,
            "entra_account": null,
            "provenance": "manual",
            "status": {
                "last_connected_at": null,
                "last_tested_at": null,
                "last_test_status": null,
                "last_error_summary": null
            }
        });

        let azure = serde_json::json!({
            "account": {
                "id": "home-a:tenant-a",
                "username": "dba@contoso.com",
                "tenant_id": "tenant-a",
                "home_account_id": "home-a",
                "display_name": "DBA",
                "last_login_at": "2026-01-01T00:00:00Z",
                "access_token_expires_at": null,
                "refresh_token_expires_at": null,
                "token_cache_ref": { "service": "sqlly.entra", "account": "home-a:tenant-a" }
            },
            "subscriptions_response": "{\"value\":[{\"subscriptionId\":\"sub-1\",\"displayName\":\"Prod\",\"tenantId\":\"tenant-a\",\"state\":\"Enabled\"}]}",
            "servers_by_subscription": {
                "sub-1": "{\"value\":[{\"id\":\"/subscriptions/sub-1/resourceGroups/rg-prod/providers/Microsoft.Sql/servers/sql-prod\",\"name\":\"sql-prod\",\"location\":\"eastus\",\"properties\":{\"fullyQualifiedDomainName\":\"sql-prod.database.windows.net\",\"state\":\"Ready\"}}]}"
            },
            "databases_by_server_id": {
                "/subscriptions/sub-1/resourceGroups/rg-prod/providers/Microsoft.Sql/servers/sql-prod": "{\"value\":[{\"id\":\"/subscriptions/sub-1/resourceGroups/rg-prod/providers/Microsoft.Sql/servers/sql-prod/databases/app\",\"name\":\"app\",\"location\":\"eastus\",\"sku\":{\"name\":\"S0\",\"tier\":\"Standard\"},\"properties\":{\"status\":\"Online\"}}]}"
            }
        });

        // A filesystem source built directly from a model snapshot.
        let schema_node = sqlly_lang::fsmodel::NamedSqlNode::new(
            sqlly_lang::fsmodel::SqlNodeKind::Schema,
            None,
            None,
            "dbo",
        );
        let table_node = sqlly_lang::fsmodel::NamedSqlNode::new(
            sqlly_lang::fsmodel::SqlNodeKind::Table,
            Some("dbo".to_string()),
            Some(schema_node.id.clone()),
            "Orders",
        );
        let snapshot = sqlly_lang::fsmodel::SqlModelSnapshot {
            schemas: vec![sqlly_lang::fsmodel::SchemaSqlNode {
                node: schema_node,
                tables: vec![sqlly_lang::fsmodel::TableSqlNode {
                    source_node: sqlly_lang::fsmodel::FileSourcedSqlNode {
                        node: table_node,
                        source: sqlly_lang::fsmodel::FileSourceRef {
                            source_file_name: "orders.sql".into(),
                            source_file_path: "/repo/orders.sql".into(),
                            file_hash: "h".into(),
                        },
                    },
                    columns: vec![],
                }],
                views: vec![],
                functions: vec![],
                types: vec![],
                procedures: vec![],
            }],
            diagnostics: vec![],
        };

        let fixture = serde_json::json!({
            "connections": [manual],
            "recent_profile_ids": ["11111111-1111-1111-1111-111111111111"],
            "azure": azure,
            "filesystem": [{
                "profile": sqlly_lang::fsmodel::FilesystemDatabaseProfile::new("Repo SQL", vec!["/repo".into()]),
                "snapshot": snapshot,
                "is_stale": false
            }]
        })
        .to_string();

        let tree = servers_explorer_tree(&fixture).unwrap();
        // All three top-level groups present.
        assert!(tree.contains("\"EntraAccountsRoot\""));
        assert!(tree.contains("\"ConnectionsRoot\""));
        assert!(tree.contains("\"FilesystemDatabasesRoot\""));
        assert!(tree.contains("\"RecentConnectionsRoot\""));
        // Entra branch reaches a connectable database.
        assert!(tree.contains("\"AzureDatabase\""));
        assert!(tree.contains("dba@contoso.com"));
        // Manual connection with metadata badges.
        assert!(tree.contains("Reporting"));
        assert!(tree.contains("PRODUCTION"));
        assert!(tree.contains("SELECT only"));
        // Filesystem objects.
        assert!(tree.contains("Repo SQL"));
        assert!(tree.contains("\"FilesystemTable\""));
        assert!(tree.contains("Orders"));
        // No secret material leaks.
        assert!(!tree.to_lowercase().contains("password"));
        assert!(!tree.contains("\"plaintext\""));
    }

    #[test]
    fn servers_explorer_live_scan_walks_a_real_folder() {
        // The live path differs from the fixture path: we hand it only a
        // FilesystemDatabaseProfile pointing at a real directory of .sql files,
        // and the indexer must walk + parse them into the tree. This is the
        // "live equivalent" the macOS Servers Explorer needs for a user-picked
        // folder.
        let mut root = std::env::temp_dir();
        root.push(format!(
            "cli_fs_scan_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("orders.sql"),
            "CREATE TABLE dbo.Orders (Id INT PRIMARY KEY, Total MONEY);\n",
        )
        .unwrap();
        std::fs::write(
            root.join("customers.sql"),
            "CREATE TABLE dbo.Customers (Id INT PRIMARY KEY, Name NVARCHAR(200));\n",
        )
        .unwrap();

        let request = serde_json::json!({
            "filesystem": [
                sqlly_lang::fsmodel::FilesystemDatabaseProfile::new(
                    "Repo SQL",
                    vec![root.to_string_lossy().to_string()],
                )
            ]
        })
        .to_string();

        let tree = servers_explorer_live_scan(&request).unwrap();

        // The filesystem group and the scanned objects are present.
        assert!(tree.contains("\"FilesystemDatabasesRoot\""));
        assert!(tree.contains("Repo SQL"));
        assert!(tree.contains("\"FilesystemTable\""));
        assert!(tree.contains("Orders"));
        assert!(tree.contains("Customers"));
        // A freshly-scanned source is not stale and (with valid DDL) has no
        // parse-error state.
        assert!(!tree.contains("\"Stale\""));
        assert!(!tree.contains("\"ParseError\""));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn azure_explorer_children_subscriptions_level() {
        let request = serde_json::json!({
            "level": "subscriptions",
            "account_id": "home-a:tenant-a",
            "arm_json": "{\"value\":[{\"subscriptionId\":\"sub-1\",\"displayName\":\"Prod\",\"tenantId\":\"tenant-a\",\"state\":\"Enabled\"}]}"
        })
        .to_string();

        let out = azure_explorer_children(&request).unwrap();
        let nodes: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = nodes.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let sub = &arr[0];
        // Id/parent/kind match the all-at-once builder's schemes exactly.
        assert_eq!(sub["id"], "entra-sub:home-a:tenant-a:sub-1");
        assert_eq!(sub["parent"], "entra-account:home-a:tenant-a");
        assert_eq!(sub["kind"], "AzureSubscription");
        assert_eq!(sub["source"], "AzureDiscovered");
        assert_eq!(sub["label"], "Prod");
        assert_eq!(sub["is_expandable"], true);
        assert_eq!(sub["can_connect"], false);
        assert_eq!(sub["actions"], serde_json::json!(["Refresh"]));
        assert_eq!(sub["account_id"], "home-a:tenant-a");
        assert_eq!(sub["resource_id"], "sub-1");
    }

    #[test]
    fn azure_explorer_children_subscription_label_omits_guid() {
        // The friendly display name is shown alone; the subscription GUID must
        // not appear in the label when a display name exists.
        let request = serde_json::json!({
            "level": "subscriptions",
            "account_id": "home-a:tenant-a",
            "arm_json": "{\"value\":[{\"subscriptionId\":\"00000000-0000-0000-0000-000000000001\",\"displayName\":\"Prod\",\"tenantId\":\"tenant-a\",\"state\":\"Enabled\"}]}"
        })
        .to_string();

        let out = azure_explorer_children(&request).unwrap();
        let nodes: serde_json::Value = serde_json::from_str(&out).unwrap();
        let sub = &nodes.as_array().unwrap()[0];
        assert_eq!(sub["label"], "Prod");
        assert_eq!(sub["resource_id"], "00000000-0000-0000-0000-000000000001");
    }

    #[test]
    fn azure_explorer_children_subscription_label_falls_back_to_id_when_unnamed() {
        // With no display name the GUID is the only available identifier.
        let request = serde_json::json!({
            "level": "subscriptions",
            "account_id": "home-a:tenant-a",
            "arm_json": "{\"value\":[{\"subscriptionId\":\"sub-9\",\"displayName\":\"\",\"tenantId\":\"tenant-a\",\"state\":\"Enabled\"}]}"
        })
        .to_string();

        let out = azure_explorer_children(&request).unwrap();
        let nodes: serde_json::Value = serde_json::from_str(&out).unwrap();
        let sub = &nodes.as_array().unwrap()[0];
        assert_eq!(sub["label"], "sub-9");
    }

    #[test]
    fn azure_explorer_children_servers_level() {
        let request = serde_json::json!({
            "level": "servers",
            "account_id": "home-a:tenant-a",
            "subscription_id": "sub-1",
            "subscription_name": "Prod",
            "arm_json": "{\"value\":[{\"id\":\"/subscriptions/sub-1/resourceGroups/rg-prod/providers/Microsoft.Sql/servers/sql-prod\",\"name\":\"sql-prod\",\"location\":\"eastus\",\"properties\":{\"fullyQualifiedDomainName\":\"sql-prod.database.windows.net\",\"state\":\"Ready\"}}]}"
        })
        .to_string();

        let out = azure_explorer_children(&request).unwrap();
        let nodes: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = nodes.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let server = &arr[0];
        assert_eq!(
            server["id"],
            "entra-server:/subscriptions/sub-1/resourceGroups/rg-prod/providers/Microsoft.Sql/servers/sql-prod"
        );
        assert_eq!(server["parent"], "entra-sub:home-a:tenant-a:sub-1");
        assert_eq!(server["kind"], "AzureServer");
        assert_eq!(server["label"], "sql-prod (eastus)");
        assert_eq!(server["is_expandable"], true);
        assert_eq!(server["can_connect"], false);
        assert_eq!(server["actions"], serde_json::json!(["Refresh"]));
    }

    #[test]
    fn azure_explorer_children_databases_level_with_linked_badge() {
        let server_id =
            "/subscriptions/sub-1/resourceGroups/rg-prod/providers/Microsoft.Sql/servers/sql-prod";
        let db_id = format!("{server_id}/databases/app");
        let request = serde_json::json!({
            "level": "databases",
            "account_id": "home-a:tenant-a",
            "subscription_id": "sub-1",
            "subscription_name": "Prod",
            "resource_group": "rg-prod",
            "server_id": server_id,
            "server_name": "sql-prod",
            "fully_qualified_domain_name": "sql-prod.database.windows.net",
            "location": "eastus",
            "linked_database_ids": [db_id],
            "arm_json": format!(
                "{{\"value\":[{{\"id\":\"{db_id}\",\"name\":\"app\",\"location\":\"eastus\",\"sku\":{{\"name\":\"S0\",\"tier\":\"Standard\"}},\"properties\":{{\"status\":\"Online\"}}}}]}}"
            )
        })
        .to_string();

        let out = azure_explorer_children(&request).unwrap();
        let nodes: serde_json::Value = serde_json::from_str(&out).unwrap();
        let arr = nodes.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        let db = &arr[0];
        assert_eq!(db["id"], format!("entra-db:{db_id}"));
        assert_eq!(db["parent"], format!("entra-server:{server_id}"));
        assert_eq!(db["kind"], "AzureDatabase");
        assert_eq!(db["label"], "app");
        // Leaf: not expandable, connectable, full action set.
        assert_eq!(db["is_expandable"], false);
        assert_eq!(db["can_connect"], true);
        assert_eq!(
            db["actions"],
            serde_json::json!(["Connect", "NewQuery", "Properties"])
        );
        // Linked id => "Linked" badge (not "Discovered") + tier + status.
        let badges = db["badges"].as_array().unwrap();
        assert!(badges.contains(&serde_json::json!("Linked")));
        assert!(!badges.contains(&serde_json::json!("Discovered")));
        assert!(badges.contains(&serde_json::json!("Standard")));
        assert!(badges.contains(&serde_json::json!("Online")));
    }

    #[test]
    fn azure_explorer_children_databases_unlinked_is_discovered() {
        let server_id =
            "/subscriptions/sub-1/resourceGroups/rg/providers/Microsoft.Sql/servers/srv";
        let db_id = format!("{server_id}/databases/db1");
        let request = serde_json::json!({
            "level": "databases",
            "account_id": "home-a:tenant-a",
            "subscription_id": "sub-1",
            "resource_group": "rg",
            "server_id": server_id,
            "server_name": "srv",
            "location": "westus",
            "arm_json": format!(
                "{{\"value\":[{{\"id\":\"{db_id}\",\"name\":\"db1\",\"location\":\"westus\",\"properties\":{{\"status\":\"Online\"}}}}]}}"
            )
        })
        .to_string();

        let out = azure_explorer_children(&request).unwrap();
        let nodes: serde_json::Value = serde_json::from_str(&out).unwrap();
        let db = &nodes.as_array().unwrap()[0];
        let badges = db["badges"].as_array().unwrap();
        assert!(badges.contains(&serde_json::json!("Discovered")));
        assert!(!badges.contains(&serde_json::json!("Linked")));
    }

    #[test]
    fn azure_explorer_children_rejects_invalid_arm_json() {
        let request = serde_json::json!({
            "level": "subscriptions",
            "account_id": "home-a:tenant-a",
            "arm_json": "not json"
        })
        .to_string();
        assert!(azure_explorer_children(&request).is_err());
    }

    #[test]
    fn template_helpers() {
        assert!(templates_list().contains("Create Table"));
        assert!(templates_search("procedure").contains("Stored Procedure"));
        assert!(templates_search("zzz").contains("no templates"));
        let mut values = HashMap::new();
        values.insert("table_name".to_string(), "Orders".to_string());
        assert!(
            templates_render("Create Table", values)
                .unwrap()
                .contains("Orders")
        );
        assert!(templates_render("Nope", HashMap::new()).is_err());
    }

    // Tiny unique-ish suffix so parallel test runs don't collide on the temp file.
    fn uuid_like() -> u128 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn recovery_snapshot_wraps_tabs_into_versioned_envelope() {
        let tabs = r#"[{"title":"Query 1","sql":"SELECT 1"},{"title":"saved","sql":"SELECT 2","file_path":"/tmp/x.sql","dirty":false}]"#;
        let snapshot = recovery_snapshot(tabs).unwrap();
        assert!(snapshot.contains("\"version\": 1"));
        assert!(snapshot.contains("Query 1"));
        // Round-trips and only the dirty tab is recoverable.
        let recoverable = recovery_recoverable(&snapshot).unwrap();
        assert!(recoverable.contains("Query 1"));
        assert!(!recoverable.contains("saved"));
    }

    #[test]
    fn recovery_recoverable_rejects_bad_json() {
        assert!(recovery_recoverable("not json").is_err());
    }

    #[test]
    fn recovery_snapshot_preserves_ui_supplied_id() {
        let id = "11111111-2222-3333-4444-555555555555";
        let tabs = format!(r#"[{{"id":"{id}","title":"Query 1","sql":"SELECT 1"}}]"#);
        let snapshot = recovery_snapshot(&tabs).unwrap();
        // The stable id round-trips through the snapshot envelope...
        assert!(snapshot.contains(id));
        // ...and back out through the recoverable list, so the UI can re-key its
        // sidecar metadata to the same document after a relaunch.
        let recoverable = recovery_recoverable(&snapshot).unwrap();
        assert!(recoverable.contains(id));
    }

    #[test]
    fn recovery_snapshot_generates_id_when_absent() {
        let tabs = r#"[{"title":"Query 1","sql":"SELECT 1"}]"#;
        let snapshot = recovery_snapshot(tabs).unwrap();
        // A fresh uuid is still emitted for back-compat callers.
        assert!(snapshot.contains("\"id\":"));
    }

    #[test]
    fn preferences_default_is_valid_json_with_known_fields() {
        let json = preferences_default().unwrap();
        assert!(json.contains("\"result_page_size\": 1000"));
        assert!(json.contains("\"editor_font_size\": 13"));
        // Round-trips through normalize unchanged.
        let normalized = preferences_normalize(&json).unwrap();
        assert!(normalized.contains("\"result_page_size\": 1000"));
    }

    #[test]
    fn preferences_normalize_clamps_and_fills_defaults() {
        let out =
            preferences_normalize(r#"{"editor_font_size":200,"result_page_size":0}"#).unwrap();
        assert!(out.contains("\"editor_font_size\": 48"));
        assert!(out.contains("\"result_page_size\": 1"));
        assert!(out.contains("\"editor_tab_width\": 4"));
    }

    #[test]
    fn result_window_slice_reports_truncated_final_page() {
        let out = result_window_slice(100, 250, 2).unwrap();
        assert!(out.contains("\"page_count\":3"));
        assert!(out.contains("\"offset\":200"));
        assert!(out.contains("\"len\":50"));
    }

    #[test]
    fn safety_preview_blocks_non_select_and_previews_rollback() {
        // Production + block-all-but-SELECT: a mutation is blocked with a span.
        let blocked = safety_preview("UPDATE t SET x=1", "PRODUCTION", true, false).unwrap();
        assert!(blocked.contains("\"allowed\":false"));
        assert!(blocked.contains("\"select_only\":false"));
        assert!(blocked.contains("\"blocked_span\":{"));
        assert!(blocked.contains("UPDATE"));

        // Staging + auto-wrap rollback: a SELECT is allowed and wrapped.
        let wrapped = safety_preview("SELECT 1", "STAGING", false, true).unwrap();
        assert!(wrapped.contains("\"allowed\":true"));
        assert!(wrapped.contains("\"wrapped_in_rollback\":true"));
        assert!(wrapped.contains("BEGIN TRANSACTION"));
        assert!(wrapped.contains("ROLLBACK TRANSACTION"));

        // Unknown environment is a clean error, not a panic.
        assert!(safety_preview("SELECT 1", "BOGUS", false, false).is_err());
    }

    #[test]
    fn parse_sql_reports_ok_for_valid_and_diagnostics_for_malformed() {
        // Well-formed SQL yields no diagnostics.
        let ok = parse_sql("SELECT a, b FROM dbo.t WHERE a = 1;").unwrap();
        assert!(ok.contains("\"ok\":true"));
        assert!(ok.contains("\"diagnostic_count\":0"));

        // Malformed SQL (unterminated string / unbalanced paren) produces
        // diagnostics and ok:false, without executing anything.
        let bad = parse_sql("SELECT (a FROM t WHERE name = 'unterminated").unwrap();
        assert!(bad.contains("\"ok\":false"));
        assert!(!bad.contains("\"diagnostic_count\":0"));
    }

    #[test]
    fn cell_display_matches_macos_grid_formatting() {
        use sqlly_core::CellValue;
        assert_eq!(cell_display(&CellValue::Null), "NULL");
        assert_eq!(cell_display(&CellValue::Bool(true)), "true");
        assert_eq!(cell_display(&CellValue::Bool(false)), "false");
        assert_eq!(cell_display(&CellValue::I64(-42)), "-42");
        assert_eq!(cell_display(&CellValue::String("hi".into())), "hi");
        assert_eq!(
            cell_display(&CellValue::Bytes(vec![0x0a, 0xff, 0x00])),
            "0x0AFF00"
        );
    }

    #[test]
    fn spill_result_page_reads_window_from_disk() {
        // Write a spill file, then page it back through the public command —
        // this is the path the macOS grid uses to stay O(window) in memory.
        let mut path = std::env::temp_dir();
        path.push(format!(
            "cli_spill_test_{}_{}.bin",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut w = sqlly_core::SpillWriter::create(&path).unwrap();
        for i in 0..5_000i64 {
            w.append(&[
                sqlly_core::CellValue::I64(i),
                sqlly_core::CellValue::String(format!("name-{i}")),
            ])
            .unwrap();
        }
        w.finish().unwrap();

        let out = spill_result_page(&path, 1000, 50).unwrap();
        assert!(out.contains("\"total_rows\":5000"));
        assert!(out.contains("\"offset\":1000"));
        assert!(out.contains("\"len\":50"));
        assert!(out.contains("name-1000"));
        assert!(out.contains("name-1049"));
        assert!(!out.contains("name-1050"));
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(format!("{}.idx", path.display())).ok();
    }

    #[test]
    fn dump_options_from_args_maps_flags() {
        let opts = dump_options_from_args(
            DumpContentArg::StructureOnly,
            true,
            true,
            250,
            true,
            true,
            true,
            true,
            vec!["dbo".to_string(), "sales".to_string()],
        );
        assert_eq!(opts.content, DumpContent::StructureOnly);
        assert_eq!(opts.layout, DumpLayout::FilePerObject);
        assert!(opts.drop_if_exists);
        assert_eq!(opts.batch_size, 250);
        assert!(!opts.include_views);
        assert!(!opts.include_routines);
        assert!(!opts.include_triggers);
        assert!(!opts.include_sequences);
        assert_eq!(
            opts.schemas,
            Some(vec!["dbo".to_string(), "sales".to_string()])
        );
        let opts = dump_options_from_args(
            DumpContentArg::StructureAndData,
            false,
            false,
            500,
            false,
            false,
            false,
            false,
            Vec::new(),
        );
        assert!(opts.include_sequences);
        assert_eq!(opts.schemas, None);
    }

    #[test]
    fn connection_preview_flags_azure_entra_and_hides_secret() {
        let out = connection_preview(
            "prod",
            "prod.database.windows.net",
            Some("sales"),
            "entra-browser",
        )
        .unwrap();
        assert!(out.contains("\"is_azure\": true"));
        assert!(out.contains("Microsoft Entra (browser)"));
        assert!(out.contains("tcp:prod.database.windows.net,1433"));

        // SQL mode against a local host is not Azure and leaks no secret.
        let local = connection_preview("local", "localhost", None, "sql").unwrap();
        assert!(local.contains("\"is_azure\": false"));

        assert!(connection_preview("x", "y", None, "bogus").is_err());
    }
}
