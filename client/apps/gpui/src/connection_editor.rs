use crate::tooltip::AppTooltip;
use std::collections::BTreeMap;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, IntoElement, MouseButton, MouseDownEvent, Render,
    ScrollHandle, Styled, Subscription, Window, anchored, deferred, div, img, prelude::*, px,
};
use sqlly_core::connection::{
    ConnectionRoute, KubePortForward, KubeTargetKind, ProxyHop, ProxyKind, SshAuth, SshHop,
    SshTunnelConfig, SsmPortForward, TransportChain,
};
use sqlly_core::{
    AuthenticationType, ConnectionEnvironment, ConnectionProfile, DatabaseType, DuplicatedProfile,
    EntraAccountBinding, SecretProvider, SecretRef, TlsMode, unique_copy_name,
};
use sqlly_services::{IdentifierCase, ServerInfo, collect_server_info};
use uuid::Uuid;

use crate::azure_registry::EntraAccountRegistryStore;
use crate::connection::{
    ClientEntity, ConnectionStore, ProjectEntity, SafetyFeatures, SqlAuthAccount,
};
use crate::connection_management::AuthMode;
use crate::dialog::{Dialog, DialogFrame};
use crate::host_validator::{HostValidationResult, validate_host};
use crate::scrollbars::DraggableScrollbar;
use crate::service_router::ServiceRouter;
use crate::theme::Theme;
use gpui_component::Disableable;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::select::{SearchableVec, Select, SelectEvent, SelectState};

#[path = "connection_editor_sentinel.rs"]
mod sentinel;

/// UI label for a TLS mode. `Require` is explicitly labelled as NOT
/// verifying the server — it must never read as a verified option.
pub(crate) fn tls_mode_label(mode: TlsMode) -> String {
    crate::localization::text(match mode {
        TlsMode::Disable => "connections.editor.tls_disable",
        TlsMode::Require => "connections.editor.tls_require_unverified",
        TlsMode::VerifyCa => "connections.editor.tls_verify_ca",
        TlsMode::Legacy | TlsMode::VerifyFull => "connections.editor.tls_verify_full",
    })
}

fn redis_cluster_mode_label(mode: sqlly_core::redis_cluster::RedisClusterMode) -> String {
    crate::localization::text(match mode {
        sqlly_core::redis_cluster::RedisClusterMode::Auto => {
            "connections.editor.redis_cluster_auto"
        }
        sqlly_core::redis_cluster::RedisClusterMode::Standalone => {
            "connections.editor.redis_cluster_standalone"
        }
        sqlly_core::redis_cluster::RedisClusterMode::Cluster => {
            "connections.editor.redis_cluster_enabled"
        }
    })
}

/// Shown under the Redis cluster fields: what a relayed connection can and
/// cannot do with a cluster.
fn redis_cluster_relay_note() -> String {
    crate::localization::text("connections.editor.redis_cluster_relay_note")
}

/// Shown instead of the SSH tunnel fields when the engine or build cannot
/// open a tunnel.
fn ssh_tunnel_block_message(block: crate::capabilities::TransportBlock) -> String {
    use crate::capabilities::TransportBlock;
    crate::localization::text(match block {
        TransportBlock::DesktopRequired => "connections.editor.ssh_tunnel_desktop_required",
        TransportBlock::BrowserUnavailable => "connections.editor.ssh_tunnel_browser_unavailable",
        TransportBlock::SqliteFile => "connections.editor.ssh_tunnel_sqlite_unavailable",
        TransportBlock::DuckDbFile => "connections.editor.ssh_tunnel_duckdb_unavailable",
        TransportBlock::EngineUnsupported => "connections.editor.ssh_tunnel_engine_unavailable",
    })
}

/// Shown instead of the Kubernetes port-forward fields when the engine or
/// build cannot forward a port.
fn kube_block_message(block: crate::capabilities::TransportBlock) -> String {
    use crate::capabilities::TransportBlock;
    crate::localization::text(match block {
        TransportBlock::DesktopRequired => "connections.editor.kube_desktop_required",
        TransportBlock::BrowserUnavailable => "connections.editor.kube_browser_unavailable",
        TransportBlock::SqliteFile => "connections.editor.kube_sqlite_unavailable",
        TransportBlock::DuckDbFile => "connections.editor.kube_duckdb_unavailable",
        TransportBlock::EngineUnsupported => "connections.editor.kube_engine_unavailable",
    })
}

fn connection_engine_note(database_type: DatabaseType) -> Option<String> {
    use DatabaseType::*;
    let key = match database_type {
        CockroachDb => "connections.editor.compatibility_cockroachdb",
        YugabyteDb => "connections.editor.compatibility_yugabytedb",
        TiDb => "connections.editor.compatibility_tidb",
        PlanetScale => "connections.editor.compatibility_planetscale",
        AzurePostgresFlexible | Neon => "connections.editor.compatibility_postgres_tls",
        AzureMySqlFlexible => "connections.editor.compatibility_mysql_tls",
        AmazonAuroraPostgres | AmazonRdsPostgres | CloudSqlPostgres | Supabase => {
            "connections.editor.compatibility_postgres"
        }
        AmazonAurora | AmazonRds | CloudSqlMySql => "connections.editor.compatibility_mysql",
        Valkey | KeyDb | DragonflyDb => "connections.editor.compatibility_redis_protocol",
        AmazonElastiCache => "connections.editor.compatibility_elasticache",
        AmazonMemoryDb => "connections.editor.compatibility_memorydb",
        Oracle => "connections.editor.engine_note_oracle",
        _ => return None,
    };
    Some(crate::localization::text(key))
}

/// The cluster-mode choices the Redis connection editor offers.
pub(crate) fn redis_cluster_mode_labels() -> Vec<String> {
    sqlly_core::redis_cluster::RedisClusterMode::ALL
        .into_iter()
        .map(redis_cluster_mode_label)
        .collect()
}

/// The cluster mode at `row` of [`redis_cluster_mode_labels`]. Selects map
/// their confirmed row, never the translated label text.
pub(crate) fn redis_cluster_mode_from_row(
    row: usize,
) -> Option<sqlly_core::redis_cluster::RedisClusterMode> {
    sqlly_core::redis_cluster::RedisClusterMode::ALL
        .get(row)
        .copied()
}

/// Rows of the "Display time zone" select: Use app setting, UTC, Local,
/// then every named IANA zone.
const DISPLAY_ZONE_FIXED_ROWS: usize = 3;

/// The choices the "Display time zone" select offers, in row order.
pub(crate) fn display_zone_labels() -> Vec<String> {
    let mut labels = vec![
        crate::localization::text("connections.editor.display_zone_app_setting"),
        crate::localization::text("connections.editor.display_zone_utc"),
        crate::localization::text("connections.editor.display_zone_local"),
    ];
    labels.extend(
        chrono_tz::TZ_VARIANTS
            .iter()
            .map(|tz| tz.name().to_string()),
    );
    labels
}

/// The zone at `row` of [`display_zone_labels`] (`Some(None)` = use the app
/// setting); `None` when the row is out of range.
pub(crate) fn display_zone_from_row(row: usize) -> Option<Option<sqlly_core::DisplayTimeZone>> {
    use sqlly_core::DisplayTimeZone;
    match row {
        0 => Some(None),
        1 => Some(Some(DisplayTimeZone::Utc)),
        2 => Some(Some(DisplayTimeZone::Local)),
        _ => chrono_tz::TZ_VARIANTS
            .get(row - DISPLAY_ZONE_FIXED_ROWS)
            .map(|tz| Some(DisplayTimeZone::Named(tz.name().to_string()))),
    }
}

/// The select label for `zone`. A named zone the zone list does not know
/// (hand-edited profile) still shows its own name.
pub(crate) fn display_zone_label(zone: Option<&sqlly_core::DisplayTimeZone>) -> String {
    use sqlly_core::DisplayTimeZone;
    match zone {
        None => crate::localization::text("connections.editor.display_zone_app_setting"),
        Some(DisplayTimeZone::Utc) => {
            crate::localization::text("connections.editor.display_zone_utc")
        }
        Some(DisplayTimeZone::Local) => {
            crate::localization::text("connections.editor.display_zone_local")
        }
        Some(DisplayTimeZone::Named(name)) => name.clone(),
    }
}

/// Make select labels unique by suffixing repeats with their occurrence
/// number (`name (2)`). gpui-component re-resolves a confirmed value to the
/// first item with that text, so rows are only trustworthy when no two
/// labels are equal — e.g. a tunnel profile named like the "none" entry, or
/// two profiles sharing a name.
pub(crate) fn unique_select_labels(labels: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for label in labels {
        let mut candidate = label.clone();
        let mut n = 1;
        while out.contains(&candidate) {
            n += 1;
            candidate = format!("{label} ({n})");
        }
        out.push(candidate);
    }
    out
}

/// Row the select just confirmed (or currently shows).
pub(crate) fn select_row(
    select: &Entity<SelectState<Vec<String>>>,
    cx: &gpui::App,
) -> Option<usize> {
    select.read(cx).selected_index(cx).map(|ix| ix.row)
}

/// Host validation for the server field: a Redis-family server may list
/// several seed hosts (`h1:7000, h2:7001`), each validated on its own;
/// every other engine validates the field as one host.
pub(crate) fn validate_server_text(engine: DatabaseType, text: &str) -> HostValidationResult {
    if !crate::engine_form::uses_key_delimiter(engine) || !text.contains(',') {
        return validate_host(text);
    }
    let mut any = false;
    for entry in text.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        any = true;
        let host = sqlly_core::redis_cluster::split_host_port(entry, 0)
            .map(|(h, _)| h)
            .unwrap_or(entry);
        match validate_host(host) {
            HostValidationResult::Valid => {}
            other => return other,
        }
    }
    if any {
        HostValidationResult::Valid
    } else {
        HostValidationResult::Empty
    }
}

/// Order of the TLS mode select's labels.
const TLS_MODE_CHOICES: [TlsMode; 4] = [
    TlsMode::Disable,
    TlsMode::Require,
    TlsMode::VerifyCa,
    TlsMode::VerifyFull,
];

pub(crate) fn tls_mode_labels() -> Vec<String> {
    TLS_MODE_CHOICES
        .into_iter()
        .map(|m| tls_mode_label(m).to_string())
        .collect()
}

/// The TLS mode at `row` of [`tls_mode_labels`].
pub(crate) fn tls_mode_from_row(row: usize) -> Option<TlsMode> {
    TLS_MODE_CHOICES.get(row).copied()
}

/// The Kubernetes target kind at `row` of the kind select
/// ([`KubeTargetKind::ALL`] order).
fn kube_kind_from_row(row: usize) -> Option<KubeTargetKind> {
    KubeTargetKind::ALL.get(row).copied()
}

/// The tunnel profile picked at `row`: row 0 is "no tunnel profile", the
/// rest follow `profile_ids`. Resolved by position so a profile named like
/// the "none" entry (or two profiles sharing a name) maps to its own id.
fn tunnel_profile_from_row(row: Option<usize>, profile_ids: &[Uuid]) -> Option<Uuid> {
    match row {
        None | Some(0) => None,
        Some(row) => profile_ids.get(row - 1).copied(),
    }
}

pub enum ConnectionEditorEvent {
    /// The profile as it was written to the store. Carried on the event so the
    /// app can adopt the new settings on the spot — safety flags edited here
    /// used to sit unread until the next launch, because the running session
    /// holds its own clone of the profile it connected with.
    Saved(Box<ConnectionProfile>),
    Cancelled,
    RefreshAzureServersRequested,
    ManageTunnelProfiles,
}

impl EventEmitter<ConnectionEditorEvent> for ConnectionEditor {}

/// Which TLS path row a Browse… button fills.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
enum TlsPathField {
    Ca,
    ClientCert,
    ClientKey,
}

pub struct ConnectionEditor {
    /// Route-aware service resolution shared with the app: Test Connection and
    /// the database probe resolve through the route the form describes, so a
    /// Via-Relay profile is probed over its established relay session (and
    /// errors clearly when none is open) instead of dialing direct TCP.
    router: ServiceRouter,
    log: crate::log_console::DiagnosticLog,
    name_input: Entity<InputState>,
    server_input: Entity<InputState>,
    port_input: Entity<InputState>,
    engine_select: Entity<SelectState<SearchableVec<String>>>,
    database_input: Entity<InputState>,
    username_input: Entity<InputState>,
    password_input: Entity<InputState>,
    url_paste_input: Entity<InputState>,
    url_paste_open: bool,
    client_input: Entity<InputState>,
    project_input: Entity<InputState>,
    timeout_input: Entity<InputState>,
    startup_sql_input: Entity<TextareaState>,
    /// The connection's free-form notes (multi-line, display only).
    notes_input: Entity<TextareaState>,
    redis_delimiter_input: Entity<InputState>,
    redis_cluster_select: Entity<SelectState<Vec<String>>>,
    /// "Display time zone" for this connection's result grids.
    display_zone_select: Entity<SelectState<Vec<String>>>,
    redis_node_map_input: Entity<TextareaState>,
    clickhouse_native_port_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
    editing_id: Option<Uuid>,
    name: String,
    server: String,
    port: String,
    database_type: DatabaseType,
    /// The Redis-family "Auto-detect" picker entry is chosen:
    /// `database_type` follows the host name and the server's answer.
    engine_auto_detect: bool,
    database: String,
    username: String,
    aws_region: String,
    aws_profile: String,
    aws_region_input: Entity<InputState>,
    #[cfg(not(target_arch = "wasm32"))]
    aws_profile_select: Entity<SelectState<Vec<String>>>,
    #[cfg(not(target_arch = "wasm32"))]
    aws_profile_options: Vec<String>,
    #[cfg(not(target_arch = "wasm32"))]
    aws_profiles_loaded: bool,
    #[cfg(not(target_arch = "wasm32"))]
    aws_profile_select_stale: bool,
    #[cfg(not(target_arch = "wasm32"))]
    gcp_adc_status: String,
    sql_account: Option<Uuid>,
    client: String,
    project: String,
    timeout: String,
    startup_sql: String,
    /// Free-form notes kept on the connection (blank = none).
    notes: String,
    /// Redis key browser delimiter (blank = `:`).
    redis_key_delimiter: String,
    /// Redis Sentinel switch, master name, sentinels and password.
    sentinel: sentinel::SentinelFields,
    /// Redis standalone/cluster choice and the node-map text.
    redis_cluster_mode: sqlly_core::redis_cluster::RedisClusterMode,
    redis_node_map: String,
    /// Display time zone for result grids (`None` = the app setting).
    display_time_zone: Option<sqlly_core::DisplayTimeZone>,
    /// ClickHouse native-protocol port for Open in Terminal (blank = the
    /// conventional mapping from the HTTP port).
    clickhouse_native_port: String,
    environment: ConnectionEnvironment,
    auth_mode: AuthMode,
    /// Selected Entra account binding for `EntraBrowser`
    /// modes. `None` means "no account selected" — the editor will preserve
    /// the existing binding on the profile (or none on a fresh profile).
    entra_account: Option<sqlly_core::EntraAccountBinding>,
    /// Cached list of registered Entra accounts, fetched lazily from the
    /// registry when the user enters the EntraBrowser mode. Avoids blocking
    /// the editor's render path on registry I/O for SQL Login profiles.
    entra_accounts: Vec<EntraAccountSummary>,
    entra_accounts_registry: crate::azure_registry::EntraAccountRegistryStore,
    /// Shared SQL-token minter, injected by the app. Lets Test Connection
    /// re-mint an expired Entra token before dialing rather than using a stale
    /// one. `None` in tests.
    entra_sql_token_provider: Option<crate::azure_sql_token::EntraSqlTokenProvider>,
    encrypt: bool,
    trust_certificate: bool,
    /// TLS verification mode for PostgreSQL/MySQL engines. The editor always
    /// holds a resolved mode (never `Legacy`); the two legacy booleans above
    /// are kept in sync so older readers of a saved profile behave the same.
    tls_mode: TlsMode,
    tls_ca_file: String,
    tls_client_cert_file: String,
    tls_client_key_file: String,
    /// "Also trust the OS keychain roots" (Redis verifying modes).
    tls_trust_os_roots: bool,
    /// "Direct TLS (PostgreSQL 17+)" (PostgreSQL-wire encrypted modes).
    tls_direct: bool,
    tls_ca_input: Entity<InputState>,
    tls_cert_input: Entity<InputState>,
    tls_key_input: Entity<InputState>,
    tls_mode_select: Entity<SelectState<Vec<String>>>,
    tls_mode_select_stale: bool,
    block_select: bool,
    rollback: bool,
    confirm_changes: bool,
    allow_unbounded: bool,
    production_override: Option<bool>,
    testing: bool,
    test_result: Option<String>,
    test_success: bool,
    server_info: Option<ServerInfo>,
    error: Option<String>,
    /// Non-error information the form should show — today, that a duplicate
    /// opened without a secret the original's keychain entry would not give
    /// up. Rendered under the name field, where it is read before Save.
    notice: Option<String>,
    /// Set when this editor holds an unsaved duplicate: the id of the saved
    /// profile it was copied from. Save inserts the copy directly after it in
    /// the store; Cancel writes nothing. Never set together with
    /// `editing_id`.
    duplicate_of: Option<Uuid>,
    /// Live host-validation result for the "Server" field. Updated on every
    /// keystroke via `host_validator::validate_host`, mirroring the Swift
    /// `ServerListTabController.validateHostUI()` → `hostValidationLabel`.
    host_validation: HostValidationResult,
    /// Fingerprint of every user-editable field, captured on the first render
    /// after the editor opens (i.e. after any profile load populated the
    /// form). Divergence from it marks the dialog dirty, which suspends easy
    /// close so Escape / a stray outside click can't discard edits.
    baseline_fingerprint: Option<String>,
    /// Geometry of the unified dialog chrome (position/size + in-flight drag).
    frame: DialogFrame,
    scroll: ScrollHandle,
    database_all: bool,
    database_dropdown_open: bool,
    database_list: Option<Vec<String>>,
    database_loading: bool,
    database_list_error: Option<String>,
    azure_discoveries: Vec<sqlly_core::AzureSqlDiscovery>,
    azure_servers_loading: bool,
    azure_servers_error: Option<String>,
    azure_server_dropdown_open: bool,
    /// True once the "ask my admin for the Reader role" message has been copied
    /// to the clipboard from the info affordance, so the UI can confirm it.
    azure_server_help_copied: bool,
    /// Client/project names already known to the store, offered as completions
    /// under the two fields. Snapshotted when the editor opens: the store is
    /// only written when a connection is saved, which closes this editor.
    known_clients: Vec<String>,
    known_projects: Vec<String>,
    /// Which of the two organization fields is showing its completion list.
    /// `None` when neither is.
    entity_suggestions_open: Option<EntityField>,
    /// Routing: `false` connects straight to `server:port` (the back-compatible
    /// default); `true` routes the encrypted engine session through a relay
    /// device via an authorized route pair.
    via_relay: bool,
    /// Selected relay-connector device id (the bridge fronting the database).
    /// `None` until the user picks one from `relay_devices`.
    relay_device_id: Option<String>,
    /// Advisory cached relay endpoint carried on the profile. Never treated as
    /// connection identity — the authoritative endpoint comes from dynamic
    /// relay discovery. Preserved across a save/load round-trip.
    relay_hint: Option<String>,
    /// Whether a (non-expired) store session exists. Together with a non-empty
    /// `relay_devices` this gates whether the Via-Relay control is offered.
    relay_signed_in: bool,
    /// Registered relay-connector devices for the signed-in account, each with
    /// its resolved authorizing route pair (if the account has authorized one).
    relay_devices: Vec<RelayDeviceChoice>,
    /// One-shot guard so the async relay fetch is kicked off exactly once from
    /// the render path (the editor is constructed by the app, which we do not
    /// edit, so the fetch is started lazily on first render).
    relay_load_started: bool,
    relay_loading: bool,
    relay_error: Option<String>,
    /// The relay-device dropdown. Its items are display labels; the label →
    /// device-id mapping lives in `relay_select_choices`.
    relay_device_select: Entity<SelectState<Vec<String>>>,
    /// Label → device-id pairs backing `relay_device_select`, rebuilt by
    /// [`Self::sync_relay_select`] whenever `relay_devices` changes.
    relay_select_choices: Vec<(String, String)>,
    /// Set when `relay_devices` changed outside a `Window` context (async
    /// fetches); the next render syncs the select and clears it.
    relay_select_stale: bool,
    /// Pair id armed for revocation — the confirm step between clicking
    /// "Revoke" and the destructive `DELETE /api/relay/pairs/{id}`.
    pending_revoke_pair: Option<String>,
    /// SSH tunnel: off by default; when on, drivers dial a loopback listener
    /// that forwards through the configured hop(s).
    ssh_enabled: bool,
    ssh_host: String,
    ssh_port: String,
    ssh_user: String,
    ssh_auth: SshAuthMode,
    ssh_key_path: String,
    ssh_secret_id: Option<Uuid>,
    ssh_jump_enabled: bool,
    ssh_jump_host: String,
    ssh_jump_port: String,
    ssh_jump_user: String,
    /// Seconds between SSH keepalives as typed; blank is the default (30),
    /// 0 turns them off.
    ssh_keepalive: String,
    /// The profile's unanswered-keepalive limit, kept as loaded (the form
    /// does not show it).
    ssh_keepalive_max: Option<u32>,
    ssh_keepalive_input: Entity<InputState>,
    ssh_host_input: Entity<InputState>,
    ssh_port_input: Entity<InputState>,
    ssh_user_input: Entity<InputState>,
    ssh_key_path_input: Entity<InputState>,
    ssh_secret_input: Entity<InputState>,
    ssh_jump_host_input: Entity<InputState>,
    ssh_jump_port_input: Entity<InputState>,
    ssh_jump_user_input: Entity<InputState>,
    ssh_auth_select: Entity<SelectState<Vec<String>>>,
    /// Set when `ssh_auth` changes outside a `Window` context.
    ssh_auth_select_stale: bool,
    /// Selected named tunnel profile, or inline editing when `None`.
    selected_tunnel_profile: Option<Uuid>,
    tunnel_profile_select: Entity<SelectState<Vec<String>>>,
    pub(crate) tunnel_profile_select_stale: bool,
    /// Ids of the tunnel profiles behind rows 1.. of `tunnel_profile_select`
    /// (row 0 is "no tunnel profile").
    tunnel_profile_ids: Vec<Uuid>,
    proxy_enabled: bool,
    proxy_kind: ProxyKind,
    proxy_host: String,
    proxy_port: String,
    proxy_user: String,
    proxy_secret_id: Option<Uuid>,
    proxy_host_input: Entity<InputState>,
    proxy_port_input: Entity<InputState>,
    proxy_user_input: Entity<InputState>,
    proxy_secret_input: Entity<InputState>,
    proxy_kind_select: Entity<SelectState<Vec<String>>>,
    /// Kubernetes port-forward fields (inline transport only).
    kube_enabled: bool,
    kube_kubeconfig: String,
    kube_context: String,
    kube_namespace: String,
    kube_target_kind: KubeTargetKind,
    kube_target_name: String,
    kube_remote_port: String,
    kube_contexts: Vec<String>,
    kube_namespaces: Vec<String>,
    kube_targets: Vec<String>,
    kube_busy: Option<&'static str>,
    kube_error: Option<String>,
    kube_kubeconfig_input: Entity<InputState>,
    kube_context_select: Entity<SelectState<Vec<String>>>,
    kube_namespace_select: Entity<SelectState<Vec<String>>>,
    kube_kind_select: Entity<SelectState<Vec<String>>>,
    kube_target_select: Entity<SelectState<Vec<String>>>,
    kube_remote_port_input: Entity<InputState>,
    kube_context_select_stale: bool,
    kube_namespace_select_stale: bool,
    kube_kind_select_stale: bool,
    kube_target_select_stale: bool,
    /// AWS Session Manager port-forward fields (inline transport only).
    ssm_enabled: bool,
    ssm_target: String,
    ssm_region: String,
    ssm_profile: String,
    ssm_remote_host: String,
    ssm_target_input: Entity<InputState>,
    ssm_region_input: Entity<InputState>,
    ssm_profile_input: Entity<InputState>,
    ssm_remote_host_input: Entity<InputState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SshAuthMode {
    #[default]
    Agent,
    KeyFile,
    Password,
}

impl SshAuthMode {
    pub(crate) fn label(self) -> String {
        crate::localization::text(match self {
            SshAuthMode::Agent => "connections.editor.ssh_agent_option",
            SshAuthMode::KeyFile => "connections.editor.private_key_option",
            SshAuthMode::Password => "connections.editor.password_option",
        })
    }

    pub(crate) fn from_ssh_auth(auth: &SshAuth) -> Self {
        match auth {
            SshAuth::Agent => SshAuthMode::Agent,
            SshAuth::KeyFile { .. } => SshAuthMode::KeyFile,
            SshAuth::Password { .. } => SshAuthMode::Password,
        }
    }

    /// Order of every SSH auth select's labels (connection editor and tunnel
    /// profiles dialog).
    pub(crate) const SELECT_ORDER: [SshAuthMode; 3] = [
        SshAuthMode::Agent,
        SshAuthMode::KeyFile,
        SshAuthMode::Password,
    ];

    fn select_labels() -> Vec<String> {
        Self::SELECT_ORDER
            .into_iter()
            .map(|m| m.label().to_string())
            .collect()
    }

    /// The mode at `row` of the auth select.
    pub(crate) fn from_select_row(row: usize) -> Option<Self> {
        Self::SELECT_ORDER.get(row).copied()
    }
}

/// A relay-connector device offered in the routing picker, paired with the id
/// of an existing authorized route pair for it (if any). Secret-free: only
/// server-issued device/pair ids, safe to hold in the editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayDeviceChoice {
    pub device_id: String,
    pub name: String,
    /// Id of an active authorized route pair whose connector is this device,
    /// or `None` when the account has not authorized a pair for it yet.
    pub pair_id: Option<String>,
}

// Relay failure copy lives in `remote_session.rs` (delegating to
// `sqlly_remote::transport::RemoteError::relay_user_message`) — the one source both the
// establishment path and mid-session errors surface. This module carries none.

/// The two free-text organization fields that complete against names already
/// in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityField {
    Client,
    Project,
}

impl EntityField {
    fn label(self) -> String {
        crate::localization::text(match self {
            EntityField::Client => "connections.editor.client_label",
            EntityField::Project => "connections.editor.project_label",
        })
    }
}

/// Lightweight view of a registered Entra account — just the bits the
/// editor's EntraBrowser picker needs to render a row. Stored on the editor
/// as `Vec<EntraAccountSummary>` so the picker can stay simple (no entity
/// churn on every render).
#[derive(Debug, Clone)]
pub struct EntraAccountSummary {
    pub profile_id: String,
    pub tenant_id: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
}

impl ConnectionEditor {
    pub fn new(
        router: ServiceRouter,
        log: crate::log_console::DiagnosticLog,
        entra_accounts_registry: EntraAccountRegistryStore,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.editor.connection_name_placeholder",
            ))
        });
        let server_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("server.example.com"));
        let port_input = cx.new(|cx| InputState::new(window, cx).default_value("1433"));
        let engine_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(crate::engine_form::picker_labels()),
                None,
                window,
                cx,
            )
            .searchable(true)
        });
        engine_select.update(cx, |state, cx| {
            state.set_selected_value(
                &DatabaseType::SqlServer.display_name().to_string(),
                window,
                cx,
            )
        });
        let database_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.editor.optional_database_placeholder",
            ))
        });
        let username_input = cx.new(|cx| InputState::new(window, cx).placeholder("sa"));
        let aws_region_input = cx.new(|cx| InputState::new(window, cx).placeholder("us-west-2"));
        #[cfg(not(target_arch = "wasm32"))]
        let aws_profile_select = cx.new(|cx| {
            SelectState::new(
                vec![crate::cloud_iam_token::aws_profile_default_label()],
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "connections.editor.password_placeholder",
                ))
                .masked(true)
        });
        let url_paste_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("postgres://user@host:5432/db  ·  Server=…;Database=…  ·  jdbc:…")
        });
        let client_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.editor.optional_client_placeholder",
            ))
        });
        let project_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.editor.optional_project_placeholder",
            ))
        });
        let timeout_input = cx.new(|cx| InputState::new(window, cx).default_value("30"));
        let redis_delimiter_input = cx.new(|cx| InputState::new(window, cx).placeholder(":"));
        let sentinel_fields = sentinel::SentinelFields::new(window, cx);
        let redis_cluster_select =
            cx.new(|cx| SelectState::new(redis_cluster_mode_labels(), None, window, cx));
        let display_zone_select =
            cx.new(|cx| SelectState::new(display_zone_labels(), None, window, cx).searchable(true));
        display_zone_select.update(cx, |state, cx| {
            state.set_selected_value(&display_zone_label(None), window, cx)
        });
        redis_cluster_select.update(cx, |state, cx| {
            state.set_selected_value(
                &redis_cluster_mode_label(sqlly_core::redis_cluster::RedisClusterMode::Auto),
                window,
                cx,
            )
        });
        let redis_node_map_input = cx.new(|cx| {
            TextareaState::new(window, cx).soft_wrap(true).placeholder(
                crate::localization::input_placeholder(
                    window,
                    cx,
                    "connections.editor.redis_node_map_placeholder",
                ),
            )
        });
        let clickhouse_native_port_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.editor.clickhouse_native_port_placeholder",
            ))
        });
        let startup_sql_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .soft_wrap(true)
                .auto_grow(3, 8)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "connections.editor.startup_sql_placeholder",
                ))
        });
        let notes_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .soft_wrap(true)
                .auto_grow(3, 8)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "connections.editor.notes_placeholder",
                ))
        });

        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &name_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.name = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &engine_select,
            window,
            |this: &mut Self, _, ev: &SelectEvent<SearchableVec<String>>, window, cx| {
                // Typing in the picker filters the list, so the confirmed row
                // no longer indexes the engine table; resolve by label instead.
                let SelectEvent::Confirm(value) = ev;
                match value
                    .as_deref()
                    .and_then(crate::engine_form::choice_from_label)
                {
                    Some(crate::engine_form::EngineChoice::AutoDetect) => {
                        this.engine_auto_detect = true;
                        let engine = crate::engine_form::auto_detect_engine(
                            &this.server,
                            this.database_type,
                        );
                        this.apply_engine(engine, window, cx);
                    }
                    Some(crate::engine_form::EngineChoice::Engine(engine)) => {
                        this.engine_auto_detect = false;
                        this.apply_engine(engine, window, cx);
                    }
                    None => {}
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &server_input,
            window,
            |this: &mut Self, input, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event {
                    let text = input.read(cx).value().to_string();
                    this.server = text.clone();
                    this.host_validation = if crate::engine_form::is_file_engine(this.database_type)
                    {
                        if text.trim().is_empty() {
                            HostValidationResult::Empty
                        } else {
                            HostValidationResult::Valid
                        }
                    } else {
                        validate_server_text(this.database_type, &text)
                    };
                    if this.environment == ConnectionEnvironment::Unset && server_is_local(&text) {
                        this.set_environment(ConnectionEnvironment::Local, cx);
                    }
                    this.maybe_prefill_aws_region_from_server(window, cx);
                    if this.engine_auto_detect {
                        let engine =
                            crate::engine_form::auto_detect_engine(&text, this.database_type);
                        if engine != this.database_type {
                            this.apply_engine(engine, window, cx);
                        }
                    }
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &port_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.port = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &database_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.database = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &url_paste_input,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.apply_pasted_url(window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &username_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.username = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &aws_region_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.aws_region = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        #[cfg(not(target_arch = "wasm32"))]
        subscriptions.push(cx.subscribe_in(
            &aws_profile_select,
            window,
            |this: &mut Self, select, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                this.aws_profile = value
                    .as_ref()
                    .and_then(|_| select_row(select, cx))
                    .map(|row| {
                        crate::cloud_iam_token::aws_profile_from_row(row, &this.aws_profile_options)
                    })
                    .unwrap_or_default();
                cx.notify();
            },
        ));
        // Client and project complete against the names already in the store,
        // but stay free text: typing a name nobody has used yet is how a new
        // client or project comes into existence (it is registered on save).
        subscriptions.push(cx.subscribe_in(
            &client_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => {
                    this.client = input.read(cx).value().to_string();
                    this.entity_suggestions_open = Some(EntityField::Client);
                    cx.notify();
                }
                InputEvent::Focus => {
                    this.entity_suggestions_open = Some(EntityField::Client);
                    cx.notify();
                }
                _ => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &project_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => {
                    this.project = input.read(cx).value().to_string();
                    this.entity_suggestions_open = Some(EntityField::Project);
                    cx.notify();
                }
                InputEvent::Focus => {
                    this.entity_suggestions_open = Some(EntityField::Project);
                    cx.notify();
                }
                _ => {}
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &timeout_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.timeout = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &startup_sql_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.startup_sql = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &notes_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.notes = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &display_zone_select,
            window,
            |this: &mut Self, select, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                if let Some(zone) = value
                    .as_ref()
                    .and_then(|_| select_row(select, cx))
                    .and_then(display_zone_from_row)
                {
                    this.display_time_zone = zone;
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &redis_cluster_select,
            window,
            |this: &mut Self, select, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                if let Some(mode) = value
                    .as_ref()
                    .and_then(|_| select_row(select, cx))
                    .and_then(redis_cluster_mode_from_row)
                {
                    this.redis_cluster_mode = mode;
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &redis_node_map_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.redis_node_map = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &redis_delimiter_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.redis_key_delimiter = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &clickhouse_native_port_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.clickhouse_native_port = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));

        // Relay-device dropdown:
        // maps back to its device id through `relay_select_choices`.
        let relay_device_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        subscriptions.push(cx.subscribe_in(
            &relay_device_select,
            window,
            |this: &mut Self, _, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                let device_id = value.as_deref().and_then(|label| {
                    this.relay_select_choices
                        .iter()
                        .find(|(l, _)| l == label)
                        .map(|(_, id)| id.clone())
                });
                if let Some(device_id) = device_id {
                    this.select_relay_device(device_id, cx);
                }
            },
        ));

        // TLS mode select + certificate path rows (PostgreSQL/MySQL engines).
        let tls_mode_select = cx.new(|cx| SelectState::new(tls_mode_labels(), None, window, cx));
        tls_mode_select.update(cx, |state, cx| {
            state.set_selected_value(&tls_mode_label(TlsMode::VerifyFull).to_string(), window, cx)
        });
        subscriptions.push(cx.subscribe_in(
            &tls_mode_select,
            window,
            |this: &mut Self, select, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                let Some(mode) = value
                    .as_ref()
                    .and_then(|_| select_row(select, cx))
                    .and_then(tls_mode_from_row)
                else {
                    return;
                };
                this.set_tls_mode(mode, cx);
            },
        ));
        let tls_ca_input = cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/ca.pem"));
        let tls_cert_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/client-cert.pem"));
        let tls_key_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/client-key.pem"));
        for (input, field) in [
            (&tls_ca_input, "tls_ca"),
            (&tls_cert_input, "tls_cert"),
            (&tls_key_input, "tls_key"),
        ] {
            let input = input.clone();
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = state.read(cx).value().to_string();
                        match field {
                            "tls_ca" => this.tls_ca_file = value,
                            "tls_cert" => this.tls_client_cert_file = value,
                            "tls_key" => this.tls_client_key_file = value,
                            _ => {}
                        }
                        cx.notify();
                    }
                },
            ));
        }

        let ssh_host_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("bastion.example.com"));
        let ssh_port_input = cx.new(|cx| InputState::new(window, cx).default_value("22"));
        let ssh_user_input = cx.new(|cx| InputState::new(window, cx).placeholder("ubuntu"));
        let ssh_key_path_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/id_rsa"));
        let ssh_secret_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "connections.editor.ssh_secret_placeholder",
                ))
                .masked(true)
        });
        let ssh_jump_host_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("jump.example.com"));
        let ssh_jump_port_input = cx.new(|cx| InputState::new(window, cx).default_value("22"));
        let ssh_jump_user_input = cx.new(|cx| InputState::new(window, cx).placeholder("ubuntu"));
        let ssh_keepalive_input = cx.new(|cx| InputState::new(window, cx).placeholder("30"));
        let ssh_auth_select =
            cx.new(|cx| SelectState::new(SshAuthMode::select_labels(), None, window, cx));
        ssh_auth_select.update(cx, |state, cx| {
            state.set_selected_value(&SshAuthMode::Agent.label().to_string(), window, cx)
        });

        for (input, field) in [
            (&ssh_host_input, "ssh_host"),
            (&ssh_port_input, "ssh_port"),
            (&ssh_user_input, "ssh_user"),
            (&ssh_key_path_input, "ssh_key_path"),
            (&ssh_jump_host_input, "ssh_jump_host"),
            (&ssh_jump_port_input, "ssh_jump_port"),
            (&ssh_jump_user_input, "ssh_jump_user"),
            (&ssh_keepalive_input, "ssh_keepalive"),
        ] {
            let input = input.clone();
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = state.read(cx).value().to_string();
                        match field {
                            "ssh_host" => this.ssh_host = value,
                            "ssh_port" => this.ssh_port = value,
                            "ssh_user" => this.ssh_user = value,
                            "ssh_key_path" => this.ssh_key_path = value,
                            "ssh_jump_host" => this.ssh_jump_host = value,
                            "ssh_jump_port" => this.ssh_jump_port = value,
                            "ssh_jump_user" => this.ssh_jump_user = value,
                            "ssh_keepalive" => this.ssh_keepalive = value,
                            _ => {}
                        }
                        cx.notify();
                    }
                },
            ));
        }
        subscriptions.push(cx.subscribe_in(
            &ssh_secret_input,
            window,
            |_this: &mut Self, _, _: &InputEvent, _, _| {},
        ));
        subscriptions.push(cx.subscribe_in(
            &ssh_auth_select,
            window,
            |this: &mut Self, select, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                if value.is_none() {
                    return;
                }
                let Some(mode) = select_row(select, cx).and_then(SshAuthMode::from_select_row)
                else {
                    return;
                };
                this.ssh_auth = mode;
                cx.notify();
            },
        ));

        let proxy_host_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("proxy.example.com"));
        let proxy_port_input = cx.new(|cx| InputState::new(window, cx).default_value("1080"));
        let proxy_user_input = cx.new(|cx| InputState::new(window, cx).placeholder("username"));
        let proxy_secret_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "connections.editor.proxy_password_placeholder",
                ))
                .masked(true)
        });
        let proxy_kind_select = cx.new(|cx| {
            let mut s = SelectState::new(
                vec!["SOCKS5".to_string(), "HTTP CONNECT".to_string()],
                None,
                window,
                cx,
            );
            s.set_selected_value(&"SOCKS5".to_string(), window, cx);
            s
        });
        subscriptions.push(cx.subscribe_in(
            &proxy_kind_select,
            window,
            |this: &mut Self, _, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                this.proxy_kind = if value.as_deref() == Some("HTTP CONNECT") {
                    ProxyKind::HttpConnect
                } else {
                    ProxyKind::Socks5
                };
                cx.notify();
            },
        ));
        for (input, field) in [
            (&proxy_host_input, "proxy_host"),
            (&proxy_port_input, "proxy_port"),
            (&proxy_user_input, "proxy_user"),
        ] {
            let input = input.clone();
            subscriptions.push(cx.subscribe_in(
                &input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = state.read(cx).value().to_string();
                        match field {
                            "proxy_host" => this.proxy_host = value,
                            "proxy_port" => this.proxy_port = value,
                            "proxy_user" => this.proxy_user = value,
                            _ => {}
                        }
                        cx.notify();
                    }
                },
            ));
        }

        let tunnel_profile_select = cx.new(|cx| {
            SelectState::new(
                vec![crate::localization::text(
                    "connections.editor.no_tunnel_profile",
                )],
                None,
                window,
                cx,
            )
        });
        subscriptions.push(cx.subscribe_in(
            &tunnel_profile_select,
            window,
            |this: &mut Self, select, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                let row = value.as_ref().and_then(|_| select_row(select, cx));
                this.selected_tunnel_profile =
                    tunnel_profile_from_row(row, &this.tunnel_profile_ids);
                cx.notify();
            },
        ));

        let ssm_input = |window: &mut Window, cx: &mut Context<Self>, key: &'static str| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(crate::localization::input_placeholder(window, cx, key))
            })
        };
        let ssm_target_input = ssm_input(window, cx, "connections.editor.ssm_target_placeholder");
        let ssm_region_input = ssm_input(window, cx, "connections.editor.ssm_region_placeholder");
        let ssm_profile_input = ssm_input(window, cx, "connections.editor.ssm_profile_placeholder");
        let ssm_remote_host_input =
            ssm_input(window, cx, "connections.editor.ssm_remote_host_placeholder");
        for (input, field) in [
            (&ssm_target_input, SsmField::Target),
            (&ssm_region_input, SsmField::Region),
            (&ssm_profile_input, SsmField::Profile),
            (&ssm_remote_host_input, SsmField::RemoteHost),
        ] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this: &mut Self, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = input.read(cx).value().to_string();
                        *this.ssm_field_mut(field) = value;
                        cx.notify();
                    }
                },
            ));
        }

        let kube_kind_labels: Vec<String> = KubeTargetKind::ALL
            .iter()
            .map(|kind| {
                crate::localization::text(match kind {
                    KubeTargetKind::Pod => "connections.editor.kube_pod",
                    KubeTargetKind::Service => "connections.editor.kube_service",
                    KubeTargetKind::Deployment => "connections.editor.kube_deployment",
                })
            })
            .collect();
        let kube_kubeconfig_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "connections.editor.kubeconfig_placeholder",
            ))
        });
        let kube_remote_port_input = cx.new(|cx| InputState::new(window, cx).default_value("1433"));
        let kube_context_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let kube_namespace_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let kube_kind_select = cx.new(|cx| {
            let mut s = SelectState::new(kube_kind_labels.clone(), None, window, cx);
            s.set_selected_value(
                &crate::localization::text("connections.editor.kube_pod"),
                window,
                cx,
            );
            s
        });
        let kube_target_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        subscriptions.push(cx.subscribe_in(
            &kube_kubeconfig_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.kube_kubeconfig = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &kube_remote_port_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.kube_remote_port = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &kube_context_select,
            window,
            |this: &mut Self, _, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                if let Some(value) = value {
                    this.kube_context = value.clone();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &kube_namespace_select,
            window,
            |this: &mut Self, _, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                if let Some(value) = value {
                    this.kube_namespace = value.clone();
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &kube_kind_select,
            window,
            |this: &mut Self, select, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                if value.is_some() {
                    this.kube_target_kind = select_row(select, cx)
                        .and_then(kube_kind_from_row)
                        .unwrap_or(KubeTargetKind::Pod);
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &kube_target_select,
            window,
            |this: &mut Self, _, ev: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = ev;
                if let Some(value) = value {
                    this.kube_target_name = value.clone();
                    cx.notify();
                }
            },
        ));

        subscriptions.push(
            cx.observe_global_in::<crate::localization::LocalizationChanged>(
                window,
                |this, window, cx| {
                    this.sync_tls_mode_select(window, cx);
                    this.sync_ssh_auth_select(window, cx);
                    this.sync_kube_kind_select(window, cx);
                    this.sync_relay_select(window, cx);
                    this.sync_tunnel_profile_select(window, cx);
                    let selected = redis_cluster_mode_label(this.redis_cluster_mode);
                    this.redis_cluster_select.update(cx, |state, cx| {
                        state.set_items(redis_cluster_mode_labels(), window, cx);
                        state.set_selected_value(&selected, window, cx);
                    });
                    let zone = display_zone_label(this.display_time_zone.as_ref());
                    this.display_zone_select.update(cx, |state, cx| {
                        state.set_items(display_zone_labels(), window, cx);
                        state.set_selected_value(&zone, window, cx);
                    });
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        if let Some(first) = this.aws_profile_options.first_mut() {
                            *first = crate::cloud_iam_token::aws_profile_default_label();
                        }
                        this.sync_aws_profile_select(window, cx);
                    }
                    cx.notify();
                },
            ),
        );
        let entra_accounts = load_account_summaries(&entra_accounts_registry);
        let store = ConnectionStore::global();
        // Saved connections are part of the pool, not just registered entities:
        // this editor wrote client/project as free text long before it
        // registered anything, and those names are still real.
        let profiles = store.list();
        let known_clients = entity_names(
            store
                .clients()
                .into_iter()
                .map(|c| c.name)
                .chain(profiles.iter().filter_map(|p| p.client.clone())),
        );
        let known_projects = entity_names(
            store
                .projects()
                .into_iter()
                .map(|p| p.name)
                .chain(profiles.iter().filter_map(|p| p.project.clone())),
        );
        Self {
            router,
            log,
            name_input,
            server_input,
            port_input,
            engine_select,
            database_input,
            username_input,
            aws_region_input,
            #[cfg(not(target_arch = "wasm32"))]
            aws_profile_select,
            #[cfg(not(target_arch = "wasm32"))]
            aws_profile_options: vec![crate::cloud_iam_token::aws_profile_default_label()],
            #[cfg(not(target_arch = "wasm32"))]
            aws_profiles_loaded: false,
            #[cfg(not(target_arch = "wasm32"))]
            aws_profile_select_stale: false,
            #[cfg(not(target_arch = "wasm32"))]
            gcp_adc_status: String::new(),
            password_input,
            url_paste_input,
            url_paste_open: false,
            client_input,
            project_input,
            timeout_input,
            startup_sql_input,
            notes_input,
            redis_delimiter_input,
            sentinel: sentinel_fields,
            redis_cluster_select,
            display_zone_select,
            redis_node_map_input,
            clickhouse_native_port_input,
            _subscriptions: subscriptions,
            editing_id: None,
            name: String::new(),
            server: String::new(),
            port: "1433".to_string(),
            database_type: DatabaseType::SqlServer,
            engine_auto_detect: false,
            database: String::new(),
            username: String::new(),
            aws_region: String::new(),
            aws_profile: String::new(),
            sql_account: None,
            client: String::new(),
            project: String::new(),
            timeout: "30".to_string(),
            startup_sql: String::new(),
            notes: String::new(),
            redis_key_delimiter: String::new(),
            redis_cluster_mode: sqlly_core::redis_cluster::RedisClusterMode::Auto,
            redis_node_map: String::new(),
            display_time_zone: None,
            clickhouse_native_port: String::new(),
            environment: ConnectionEnvironment::Unset,
            auth_mode: AuthMode::SqlLogin,
            entra_account: None,
            entra_accounts,
            entra_accounts_registry,
            entra_sql_token_provider: None,
            encrypt: true,
            trust_certificate: false,
            tls_mode: TlsMode::VerifyFull,
            tls_ca_file: String::new(),
            tls_client_cert_file: String::new(),
            tls_client_key_file: String::new(),
            tls_trust_os_roots: false,
            tls_direct: false,
            tls_ca_input,
            tls_cert_input,
            tls_key_input,
            tls_mode_select,
            tls_mode_select_stale: false,
            block_select: false,
            rollback: false,
            confirm_changes: false,
            // Unbounded-statement protections are opt-in: new connections
            // allow unconstrained UPDATE/DELETE without a confirmation stop.
            allow_unbounded: true,
            production_override: None,
            testing: false,
            test_result: None,
            test_success: false,
            server_info: None,
            error: None,
            notice: None,
            duplicate_of: None,
            host_validation: HostValidationResult::Empty,
            baseline_fingerprint: None,
            frame: DialogFrame::new_auto(Theme::scaled(460.0)),
            scroll: ScrollHandle::new(),
            database_all: false,
            database_dropdown_open: false,
            database_list: None,
            database_loading: false,
            database_list_error: None,
            azure_discoveries: Vec::new(),
            azure_servers_loading: false,
            azure_servers_error: None,
            azure_server_dropdown_open: false,
            azure_server_help_copied: false,
            known_clients,
            known_projects,
            entity_suggestions_open: None,
            via_relay: false,
            relay_device_id: None,
            relay_hint: None,
            relay_signed_in: false,
            relay_devices: Vec::new(),
            relay_load_started: false,
            relay_loading: false,
            relay_error: None,
            relay_device_select,
            relay_select_choices: Vec::new(),
            relay_select_stale: false,
            pending_revoke_pair: None,
            ssh_enabled: false,
            ssh_host: String::new(),
            ssh_port: "22".to_string(),
            ssh_user: String::new(),
            ssh_auth: SshAuthMode::Agent,
            ssh_key_path: String::new(),
            ssh_secret_id: None,
            ssh_jump_enabled: false,
            ssh_jump_host: String::new(),
            ssh_jump_port: "22".to_string(),
            ssh_jump_user: String::new(),
            ssh_keepalive: String::new(),
            ssh_keepalive_max: None,
            ssh_keepalive_input,
            ssh_host_input,
            ssh_port_input,
            ssh_user_input,
            ssh_key_path_input,
            ssh_secret_input,
            ssh_jump_host_input,
            ssh_jump_port_input,
            ssh_jump_user_input,
            ssh_auth_select,
            ssh_auth_select_stale: false,
            selected_tunnel_profile: None,
            tunnel_profile_select,
            tunnel_profile_select_stale: true,
            tunnel_profile_ids: Vec::new(),
            proxy_enabled: false,
            proxy_kind: ProxyKind::Socks5,
            proxy_host: String::new(),
            proxy_port: "1080".to_string(),
            proxy_user: String::new(),
            proxy_secret_id: None,
            proxy_host_input,
            proxy_port_input,
            proxy_user_input,
            proxy_secret_input,
            proxy_kind_select,
            kube_enabled: false,
            kube_kubeconfig: String::new(),
            kube_context: String::new(),
            kube_namespace: "default".to_string(),
            kube_target_kind: KubeTargetKind::Pod,
            kube_target_name: String::new(),
            kube_remote_port: "1433".to_string(),
            kube_contexts: Vec::new(),
            kube_namespaces: Vec::new(),
            kube_targets: Vec::new(),
            kube_busy: None,
            kube_error: None,
            kube_kubeconfig_input,
            kube_context_select,
            kube_namespace_select,
            kube_kind_select,
            kube_target_select,
            kube_remote_port_input,
            kube_context_select_stale: true,
            kube_namespace_select_stale: true,
            kube_kind_select_stale: false,
            kube_target_select_stale: true,
            ssm_enabled: false,
            ssm_target: String::new(),
            ssm_region: String::new(),
            ssm_profile: String::new(),
            ssm_remote_host: String::new(),
            ssm_target_input,
            ssm_region_input,
            ssm_profile_input,
            ssm_remote_host_input,
        }
    }

    /// Inject the shared SQL-token provider so Test Connection re-mints an
    /// expired Entra token before dialing.
    pub fn set_entra_sql_token_provider(
        &mut self,
        provider: crate::azure_sql_token::EntraSqlTokenProvider,
    ) {
        self.entra_sql_token_provider = Some(provider);
    }

    pub fn from_profile(
        router: ServiceRouter,
        log: crate::log_console::DiagnosticLog,
        entra_accounts_registry: EntraAccountRegistryStore,
        profile: ConnectionProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let id = profile.id.0;
        let inline_chain = profile.inline_transport();
        let saved_tunnel_profile = profile.tunnel_profile;
        let mut editor = Self::new(router, log, entra_accounts_registry, window, cx);
        editor.editing_id = Some(id);
        editor.environment = profile.environment;
        editor.database_type = profile.database_type;
        editor.engine_auto_detect =
            profile.engine_auto_detect && profile.database_type.is_command_engine();
        editor.encrypt = profile.encrypt;
        editor.trust_certificate = profile.trust_server_certificate;
        // The editor always holds a resolved mode; a `Legacy` profile shows
        // the mode its booleans have always meant.
        editor.tls_mode = profile.effective_tls_mode();
        editor.tls_mode_select_stale = true;
        editor.tls_trust_os_roots = profile.tls_trust_os_roots;
        editor.tls_direct = profile.tls_direct;
        editor.tls_ca_file = profile
            .tls_ca_file
            .as_deref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        editor.tls_client_cert_file = profile
            .tls_client_cert_file
            .as_deref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        editor.tls_client_key_file = profile
            .tls_client_key_file
            .as_deref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        editor.block_select = profile.block_all_but_select;
        editor.rollback = profile.auto_wrap_rollback;
        editor.confirm_changes = profile.confirm_data_changes;
        editor.allow_unbounded = profile.allow_unbounded_mutations;
        editor.production_override = profile.production_override;
        editor.auth_mode = match &profile.authentication {
            AuthenticationType::SqlLogin { .. } => AuthMode::SqlLogin,
            AuthenticationType::Integrated => AuthMode::Integrated,
            AuthenticationType::EntraBrowser => AuthMode::EntraBrowser,
            AuthenticationType::EntraDeviceCode => AuthMode::EntraBrowser,
            AuthenticationType::AwsRdsIam { .. } => AuthMode::AwsRdsIam,
            AuthenticationType::GcpCloudSqlIam { .. } => AuthMode::GcpCloudSqlIam,
            AuthenticationType::AuthToken { .. } => AuthMode::AuthToken,
        };
        editor.apply_iam_auth_mode_side_effects(editor.auth_mode, window, cx);

        editor.name = profile.name;
        editor.server = profile.server.clone();
        editor.host_validation = if crate::engine_form::is_file_engine(profile.database_type) {
            HostValidationResult::Valid
        } else {
            validate_host(&profile.server)
        };
        editor.port = profile.port.to_string();
        let engine_label =
            crate::engine_form::picker_label(editor.database_type, editor.engine_auto_detect);
        editor.engine_select.update(cx, |state, cx| {
            state.set_selected_value(&engine_label, window, cx)
        });
        editor.database = profile.database.unwrap_or_default();
        editor.client = profile.client.unwrap_or_default();
        editor.project = profile.project.unwrap_or_default();
        editor.timeout = profile.timeout_seconds.to_string();
        editor.startup_sql = profile.startup_sql.clone().unwrap_or_default();
        editor.notes = profile.notes.clone().unwrap_or_default();
        editor.redis_key_delimiter = profile.redis_key_delimiter.clone().unwrap_or_default();
        editor
            .sentinel
            .load(profile.redis_sentinel.as_ref(), window, cx);
        editor.display_time_zone = profile.display_time_zone.clone();
        editor.redis_cluster_mode = profile
            .redis_cluster
            .as_ref()
            .map(|c| c.mode)
            .unwrap_or_default();
        editor.redis_node_map = profile
            .redis_cluster
            .as_ref()
            .map(|c| sqlly_core::redis_cluster::node_map_text(&c.node_map))
            .unwrap_or_default();
        editor.clickhouse_native_port = profile
            .clickhouse_native_port
            .map(|port| port.to_string())
            .unwrap_or_default();
        editor.entra_account = profile.entra_account.clone();
        if editor.auth_mode == AuthMode::EntraBrowser && editor.entra_account.is_none() {
            editor.entra_account = sole_entra_binding(&editor.entra_accounts);
        }

        // Load the saved routing choice. Seed a placeholder device entry so the
        // saved selection survives even before the async relay fetch returns
        // (the fetch replaces it with the real device name / pair state).
        if let ConnectionRoute::ViaRelay {
            relay_device_id,
            pair_id,
            relay_hint,
        } = &profile.route
        {
            editor.via_relay = true;
            editor.relay_device_id = Some(relay_device_id.clone());
            editor.relay_hint = relay_hint.clone();
            editor.relay_devices = vec![RelayDeviceChoice {
                device_id: relay_device_id.clone(),
                name: relay_device_id.clone(),
                pair_id: Some(pair_id.clone()),
            }];
            editor.relay_select_stale = true;
        }

        if let Some(tunnel) = profile.ssh_tunnel {
            editor.ssh_enabled = true;
            editor.ssh_host = tunnel.target.host;
            editor.ssh_port = tunnel.target.port.to_string();
            editor.ssh_user = tunnel.target.username;
            editor.ssh_auth = SshAuthMode::from_ssh_auth(&tunnel.target.auth);
            match &tunnel.target.auth {
                SshAuth::KeyFile { path, passphrase } => {
                    editor.ssh_key_path = path.clone();
                    load_ssh_secret_into_editor(
                        passphrase.as_ref(),
                        &mut editor.ssh_secret_id,
                        &editor.ssh_secret_input,
                        window,
                        cx,
                    );
                }
                SshAuth::Password { secret_ref } => {
                    load_ssh_secret_into_editor(
                        Some(secret_ref),
                        &mut editor.ssh_secret_id,
                        &editor.ssh_secret_input,
                        window,
                        cx,
                    );
                }
                SshAuth::Agent => {}
            }
            if let Some(jump) = tunnel.jump {
                editor.ssh_jump_enabled = true;
                editor.ssh_jump_host = jump.host;
                editor.ssh_jump_port = jump.port.to_string();
                editor.ssh_jump_user = jump.username;
            }
            editor.load_ssh_keepalive(tunnel.keepalive);
            editor.ssh_auth_select_stale = true;
        } else {
            if let Some(proxy) = inline_chain.proxy {
                editor.proxy_enabled = true;
                editor.proxy_kind = proxy.kind;
                editor.proxy_host = proxy.host;
                editor.proxy_port = proxy.port.to_string();
                editor.proxy_user = proxy.username.unwrap_or_default();
                load_ssh_secret_into_editor(
                    proxy.password.as_ref(),
                    &mut editor.proxy_secret_id,
                    &editor.proxy_secret_input,
                    window,
                    cx,
                );
            }
            if let Some(tunnel) = inline_chain.ssh {
                editor.ssh_enabled = true;
                editor.ssh_host = tunnel.target.host;
                editor.ssh_port = tunnel.target.port.to_string();
                editor.ssh_user = tunnel.target.username;
                editor.ssh_auth = SshAuthMode::from_ssh_auth(&tunnel.target.auth);
                if let SshAuth::KeyFile { path, passphrase } = &tunnel.target.auth {
                    editor.ssh_key_path = path.clone();
                    load_ssh_secret_into_editor(
                        passphrase.as_ref(),
                        &mut editor.ssh_secret_id,
                        &editor.ssh_secret_input,
                        window,
                        cx,
                    );
                }
                if let Some(jump) = tunnel.jump {
                    editor.ssh_jump_enabled = true;
                    editor.ssh_jump_host = jump.host;
                    editor.ssh_jump_port = jump.port.to_string();
                    editor.ssh_jump_user = jump.username;
                }
                editor.load_ssh_keepalive(tunnel.keepalive);
                editor.ssh_auth_select_stale = true;
            }
            if let Some(kube) = inline_chain.kube {
                editor.kube_enabled = true;
                editor.kube_kubeconfig = kube.kubeconfig.unwrap_or_default();
                editor.kube_context = kube.context.unwrap_or_default();
                editor.kube_namespace = kube.namespace;
                editor.kube_target_kind = kube.target_kind;
                editor.kube_target_name = kube.target_name.clone();
                editor.kube_remote_port = kube.remote_port.to_string();
                if !editor.kube_context.is_empty() {
                    editor.kube_contexts = vec![editor.kube_context.clone()];
                }
                editor.kube_namespaces = vec![editor.kube_namespace.clone()];
                editor.kube_targets = vec![editor.kube_target_name.clone()];
                editor.kube_context_select_stale = true;
                editor.kube_namespace_select_stale = true;
                editor.kube_target_select_stale = true;
            }
            if let Some(ssm) = inline_chain.ssm {
                editor.ssm_enabled = true;
                editor.ssm_target = ssm.target;
                editor.ssm_region = ssm.region.unwrap_or_default();
                editor.ssm_profile = ssm.profile.unwrap_or_default();
                editor.ssm_remote_host = ssm.remote_host.unwrap_or_default();
            }
        }

        editor.selected_tunnel_profile = saved_tunnel_profile;
        editor.tunnel_profile_select_stale = true;

        if let AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } = &profile.authentication
        {
            editor.username = username.clone();
            editor.sql_account = if secret_ref.provider == SecretProvider::MacOsKeychain {
                // A reference with nothing behind it (a blank password saved
                // before passwordless accounts existed, or an entry removed
                // from the keyring) opens as an empty field rather than a
                // mask that claims a password is stored; saving repairs it.
                Uuid::parse_str(&secret_ref.key)
                    .ok()
                    .filter(|id| !matches!(crate::sql_credentials::load_password(*id), Ok(None)))
            } else {
                None
            };
            if secret_ref.provider == SecretProvider::InMemory {
                let secret = secret_ref.key.clone();
                editor
                    .password_input
                    .update(cx, |f, cx| f.set_value(secret, window, cx));
            } else if editor.sql_account.is_some() {
                // The real password is in the keychain and never comes back
                // here; the field shows a fixed-length stand-in so the row does
                // not read as "no password saved".
                editor
                    .password_input
                    .update(cx, |f, cx| f.set_value(password_mask(), window, cx));
            }
        }

        // A libSQL auth token lives in the same secret field as a password:
        // a keyring reference keeps its entry id (so an untouched field does
        // not drop the saved token), an in-memory one shows its value.
        if let AuthenticationType::AuthToken { secret_ref } = &profile.authentication {
            editor.sql_account = if secret_ref.provider == SecretProvider::MacOsKeychain {
                Uuid::parse_str(&secret_ref.key).ok()
            } else {
                None
            };
            if secret_ref.provider == SecretProvider::InMemory {
                let secret = secret_ref.key.clone();
                editor
                    .password_input
                    .update(cx, |f, cx| f.set_value(secret, window, cx));
            } else if editor.sql_account.is_some() {
                editor
                    .password_input
                    .update(cx, |f, cx| f.set_value(password_mask(), window, cx));
            }
        }

        if let AuthenticationType::AwsRdsIam {
            username,
            region,
            profile: aws_profile,
        } = &profile.authentication
        {
            editor.username = username.clone();
            editor.aws_region = region.clone();
            editor.aws_profile = aws_profile.clone().unwrap_or_default();
        }
        if let AuthenticationType::GcpCloudSqlIam { username } = &profile.authentication {
            editor.username = username.clone();
        }
        if let Some(user) = &profile.redis_entra_user {
            editor.username = user.clone();
        }

        // Push the loaded profile values into the input states so the form
        // shows them (the plain String fields stay the canonical values,
        // kept in sync by the Change subscriptions).
        let values = [
            (editor.name_input.clone(), editor.name.clone()),
            (editor.server_input.clone(), editor.server.clone()),
            (editor.port_input.clone(), editor.port.clone()),
            (editor.database_input.clone(), editor.database.clone()),
            (editor.username_input.clone(), editor.username.clone()),
            (editor.aws_region_input.clone(), editor.aws_region.clone()),
            (editor.client_input.clone(), editor.client.clone()),
            (editor.project_input.clone(), editor.project.clone()),
            (editor.timeout_input.clone(), editor.timeout.clone()),
        ];
        for (input, value) in values {
            let w = &mut *window;
            input.update(cx, move |s, cx| s.set_value(value, w, cx));
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            editor.aws_profile_select_stale = true;
        }
        let startup_sql = editor.startup_sql.clone();
        editor
            .startup_sql_input
            .update(cx, |state, cx| state.set_value(startup_sql, window, cx));
        let notes = editor.notes.clone();
        editor
            .notes_input
            .update(cx, |state, cx| state.set_value(notes, window, cx));
        let redis_key_delimiter = editor.redis_key_delimiter.clone();
        editor.redis_delimiter_input.update(cx, |state, cx| {
            state.set_value(redis_key_delimiter, window, cx)
        });
        let zone_label = display_zone_label(editor.display_time_zone.as_ref());
        editor.display_zone_select.update(cx, |state, cx| {
            state.set_selected_value(&zone_label, window, cx)
        });
        let cluster_label = editor.redis_cluster_mode.label().to_string();
        editor.redis_cluster_select.update(cx, |state, cx| {
            state.set_selected_value(&cluster_label, window, cx)
        });
        let redis_node_map = editor.redis_node_map.clone();
        editor
            .redis_node_map_input
            .update(cx, |state, cx| state.set_value(redis_node_map, window, cx));
        let clickhouse_native_port = editor.clickhouse_native_port.clone();
        editor.clickhouse_native_port_input.update(cx, |state, cx| {
            state.set_value(clickhouse_native_port, window, cx)
        });

        if editor.tls_mode_select_stale {
            editor.sync_tls_mode_select(window, cx);
        }
        let tls_values = [
            (editor.tls_ca_input.clone(), editor.tls_ca_file.clone()),
            (
                editor.tls_cert_input.clone(),
                editor.tls_client_cert_file.clone(),
            ),
            (
                editor.tls_key_input.clone(),
                editor.tls_client_key_file.clone(),
            ),
        ];
        for (input, value) in tls_values {
            let w = &mut *window;
            input.update(cx, move |s, cx| s.set_value(value, w, cx));
        }

        if editor.ssh_auth_select_stale {
            editor.sync_ssh_auth_select(window, cx);
        }
        let ssh_values = [
            (editor.ssh_host_input.clone(), editor.ssh_host.clone()),
            (editor.ssh_port_input.clone(), editor.ssh_port.clone()),
            (editor.ssh_user_input.clone(), editor.ssh_user.clone()),
            (
                editor.ssh_key_path_input.clone(),
                editor.ssh_key_path.clone(),
            ),
            (
                editor.ssh_jump_host_input.clone(),
                editor.ssh_jump_host.clone(),
            ),
            (
                editor.ssh_jump_port_input.clone(),
                editor.ssh_jump_port.clone(),
            ),
            (
                editor.ssh_jump_user_input.clone(),
                editor.ssh_jump_user.clone(),
            ),
            (
                editor.ssh_keepalive_input.clone(),
                editor.ssh_keepalive.clone(),
            ),
        ];
        for (input, value) in ssh_values {
            let w = &mut *window;
            input.update(cx, move |s, cx| s.set_value(value, w, cx));
        }
        let kube_values = [
            (
                editor.kube_kubeconfig_input.clone(),
                editor.kube_kubeconfig.clone(),
            ),
            (
                editor.kube_remote_port_input.clone(),
                editor.kube_remote_port.clone(),
            ),
        ];
        for (input, value) in kube_values {
            let w = &mut *window;
            input.update(cx, move |s, cx| s.set_value(value, w, cx));
        }
        let ssm_values = [
            (editor.ssm_target_input.clone(), editor.ssm_target.clone()),
            (editor.ssm_region_input.clone(), editor.ssm_region.clone()),
            (editor.ssm_profile_input.clone(), editor.ssm_profile.clone()),
            (
                editor.ssm_remote_host_input.clone(),
                editor.ssm_remote_host.clone(),
            ),
        ];
        for (input, value) in ssm_values {
            let w = &mut *window;
            input.update(cx, move |s, cx| s.set_value(value, w, cx));
        }
        if editor.kube_context_select_stale {
            editor.sync_kube_context_select(window, cx);
        }
        if editor.kube_namespace_select_stale {
            editor.sync_kube_namespace_select(window, cx);
        }
        if editor.kube_target_select_stale {
            editor.sync_kube_target_select(window, cx);
        }

        editor
    }

    /// Open the editor on an independent, unsaved copy of `original`.
    ///
    /// The copy is [`ConnectionProfile::duplicate_named`] of the original
    /// (fresh id, `"<name> (copy)"` made unique against `existing_names`,
    /// every setting carried over, every keyring reference re-minted). The
    /// original's secrets are read from the keychain into the form's inputs
    /// so that Save — the ordinary save path — writes them under the copy's
    /// own references; a secret the keychain will not give up leaves its
    /// input empty and is named in the notice under the name field. Nothing
    /// touches the store or the keychain until Save.
    pub fn from_duplicate(
        router: ServiceRouter,
        log: crate::log_console::DiagnosticLog,
        entra_accounts_registry: EntraAccountRegistryStore,
        original: ConnectionProfile,
        existing_names: impl IntoIterator<Item = String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let original_id = original.id.0;
        let name = unique_copy_name(&original.name, existing_names);
        let DuplicatedProfile { profile, secrets } = original.duplicate_named(name);
        // The copy's re-minted password reference has no keyring entry until
        // Save; keep it, where an existing profile would open as blank.
        let fresh_account = profile
            .authentication
            .secret_ref()
            .filter(|r| r.provider == SecretProvider::MacOsKeychain)
            .and_then(|r| Uuid::parse_str(&r.key).ok());
        let mut editor =
            Self::from_profile(router, log, entra_accounts_registry, profile, window, cx);
        editor.editing_id = None;
        editor.duplicate_of = Some(original_id);
        editor.sql_account = fresh_account;

        // Each fresh keyring reference the form now holds maps back to the
        // original's entry; read that entry into the matching input.
        let source_of = |fresh: Option<Uuid>| -> Option<Uuid> {
            let fresh = fresh?.to_string();
            secrets
                .iter()
                .find(|copy| copy.to.key == fresh)
                .and_then(|copy| Uuid::parse_str(&copy.from.key).ok())
        };
        let slots = [
            (
                source_of(editor.sql_account),
                editor.password_input.clone(),
                crate::localization::text("connections.editor.duplicate_missing_password"),
            ),
            (
                source_of(editor.ssh_secret_id),
                editor.ssh_secret_input.clone(),
                crate::localization::text("connections.editor.duplicate_missing_ssh_secret"),
            ),
            (
                source_of(editor.proxy_secret_id),
                editor.proxy_secret_input.clone(),
                crate::localization::text("connections.editor.duplicate_missing_proxy_password"),
            ),
        ];
        let mut missing = Vec::new();
        for (source, input, label) in slots {
            let Some(source) = source else {
                continue;
            };
            let value = match crate::sql_credentials::load_password(source) {
                Ok(Some(secret)) => secret,
                Ok(None) | Err(_) => {
                    missing.push(label);
                    String::new()
                }
            };
            input.update(cx, |field, cx| field.set_value(value, window, cx));
        }
        editor.notice = duplicate_notice(&missing);
        editor
    }

    /// Show a loaded tunnel's keepalive interval (blank for the default) and
    /// keep its unanswered-keepalive limit for the save.
    fn load_ssh_keepalive(&mut self, keepalive: sqlly_core::SshKeepalive) {
        self.ssh_keepalive = keepalive
            .interval_seconds
            .map(|s| s.to_string())
            .unwrap_or_default();
        self.ssh_keepalive_max = keepalive.max_missed;
    }

    fn sync_ssh_auth_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ssh_auth_select_stale = false;
        let label = self.ssh_auth.label().to_string();
        self.ssh_auth_select.update(cx, |state, cx| {
            state.set_items(SshAuthMode::select_labels(), window, cx);
            state.set_selected_value(&label, window, cx);
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn handle_browse_ssh_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let file = rfd::FileDialog::new()
            .add_filter(
                crate::localization::text("connections.editor.private_key_file_filter"),
                &["pem", "key", "ppk", ""],
            )
            .set_title(crate::localization::text(
                "connections.editor.select_private_key_title",
            ))
            .pick_file();
        if let Some(path) = file {
            crate::sandbox_access::remember(&path);
            self.ssh_key_path = path.to_string_lossy().to_string();
            self.ssh_key_path_input.update(cx, |state, cx| {
                state.set_value(self.ssh_key_path.clone(), window, cx);
            });
            cx.notify();
        }
    }

    fn toggle_ssh(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.ssh_enabled = checked;
        if checked {
            self.kube_enabled = false;
            self.ssm_enabled = false;
        }
        cx.notify();
    }

    fn toggle_ssh_jump(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.ssh_jump_enabled = checked;
        cx.notify();
    }

    fn toggle_proxy(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.proxy_enabled = checked;
        if checked {
            self.kube_enabled = false;
            self.ssm_enabled = false;
        }
        cx.notify();
    }

    fn toggle_kube(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.kube_enabled = checked;
        if checked {
            self.ssh_enabled = false;
            self.proxy_enabled = false;
            self.ssm_enabled = false;
        }
        cx.notify();
    }

    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    fn toggle_ssm(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.ssm_enabled = checked;
        if checked {
            self.ssh_enabled = false;
            self.proxy_enabled = false;
            self.kube_enabled = false;
        }
        cx.notify();
    }

    fn ssm_field_mut(&mut self, field: SsmField) -> &mut String {
        match field {
            SsmField::Target => &mut self.ssm_target,
            SsmField::Region => &mut self.ssm_region,
            SsmField::Profile => &mut self.ssm_profile,
            SsmField::RemoteHost => &mut self.ssm_remote_host,
        }
    }

    fn sync_tunnel_profile_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tunnel_profile_select_stale = false;
        let profiles = ConnectionStore::global().tunnel_profiles();
        let items = unique_select_labels(
            std::iter::once(crate::localization::text(
                "connections.editor.no_tunnel_profile",
            ))
            .chain(profiles.iter().map(|p| p.name.clone())),
        );
        self.tunnel_profile_ids = profiles.iter().map(|p| p.id).collect();
        let row = self
            .selected_tunnel_profile
            .and_then(|id| self.tunnel_profile_ids.iter().position(|p| *p == id))
            .map_or(0, |ix| ix + 1);
        self.tunnel_profile_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            state.set_selected_index(Some(gpui_component::IndexPath::new(row)), window, cx);
        });
    }

    fn tunnel_profile_summary(id: Uuid) -> String {
        let Some(profile) = ConnectionStore::global().tunnel_profile(id) else {
            return crate::localization::text("connections.editor.tunnel_profile_missing");
        };
        let mut parts = Vec::new();
        if let Some(proxy) = &profile.chain.proxy {
            let kind = match proxy.kind {
                ProxyKind::Socks5 => "socks5",
                ProxyKind::HttpConnect => "http",
            };
            parts.push(format!("{kind} {}:{}", proxy.host, proxy.port));
        }
        if let Some(ssh) = &profile.chain.ssh {
            parts.push(format!(
                "ssh {}@{}:{}",
                ssh.target.username, ssh.target.host, ssh.target.port
            ));
        }
        parts.join(" → ")
    }

    /// Refresh the cached account summary list from the registry. Called when
    /// the user opens the EntraBrowser section so newly-signed-in accounts
    /// appear without reopening the whole editor.
    pub fn reload_entra_accounts(&mut self) {
        self.entra_accounts = load_account_summaries(&self.entra_accounts_registry);
    }

    pub fn set_azure_discoveries(
        &mut self,
        discoveries: Vec<sqlly_core::AzureSqlDiscovery>,
        cx: &mut Context<Self>,
    ) {
        self.azure_discoveries = discoveries;
        self.azure_servers_loading = false;
        self.azure_servers_error = None;
        self.azure_server_help_copied = false;
        cx.notify();
    }

    pub fn set_azure_discovery_error(
        &mut self,
        message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.azure_servers_loading = false;
        self.azure_servers_error = Some(message.into());
        // Do NOT force the dropdown open: a permission failure is not a
        // dead-end. The server field stays front-and-center with a friendly
        // "type it yourself" hint, so a user who can't list servers can still
        // connect by name.
        self.azure_server_dropdown_open = false;
        self.azure_server_help_copied = false;
        cx.notify();
    }

    /// Copies an admin-ready request for the Reader role to the clipboard, so a
    /// user who lacks permission to enumerate Azure SQL servers can ask for it
    /// instead of typing every server by hand.
    fn copy_azure_permission_request(&mut self, cx: &mut Context<Self>) {
        let account = self.azure_account_label();
        let message = crate::localization::format(
            "connections.editor.azure_permission_request",
            &[("account_label", &account)],
        );
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(message));
        self.azure_server_help_copied = true;
        cx.notify();
    }

    /// A human label for the Entra account this connection will authenticate as,
    /// for the admin-request message. Named from the SELECTED binding only, so a
    /// user with multiple accounts never sees the wrong identity.
    fn azure_account_label(&self) -> String {
        crate::azure_sql_token::permission_account_label(self.entra_account.as_ref())
    }

    /// Set the Entra account binding the editor will save into the profile
    /// on the next `build_profile`. `binding.account_id == ""` clears.
    pub fn select_entra_account(
        &mut self,
        binding: Option<EntraAccountBinding>,
        cx: &mut Context<Self>,
    ) {
        self.entra_account = binding;
        cx.notify();
    }

    /// Renders the picker that binds an `EntraBrowser`
    /// profile to a specific registered Entra account. Each row is a
    /// `div` with an `on_click` listener that mutates the editor's
    /// `entra_account` field — keeping the picker code pure of any
    /// rendering framework dependencies so the picker logic is testable in
    /// isolation. Returns an empty state when the user has not signed in
    /// any Entra account yet so the hint directs them to
    /// `Tools ▸ Azure SQL Connection…` to add one.
    fn render_entra_account_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let accounts = self.entra_accounts.clone();
        let selected = self.entra_account.as_ref().map(|b| b.account_id.clone());
        // Empty-state: no signed-in accounts yet. Show a "no accounts"
        // line so the user knows the picker is wired, not broken.
        if accounts.is_empty() {
            return div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .mt(px(s(4.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "connections.editor.no_entra_users",
                        )),
                )
                .into_any_element();
        }
        let title = crate::localization::format(
            "connections.editor.signed_in_entra_users",
            &[("count", &accounts.len().to_string())],
        );
        div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .mt(px(s(4.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(title),
            )
            .children(accounts.iter().map(|account| {
                let is_selected = selected.as_deref() == Some(account.profile_id.as_str());
                let label = account
                    .display_name
                    .clone()
                    .or_else(|| account.username.clone())
                    .unwrap_or_else(|| account.profile_id.clone());
                let tenant = account.tenant_id.clone();
                let binding = EntraAccountBinding {
                    account_id: account.profile_id.clone(),
                    tenant_id: account.tenant_id.clone(),
                    username: account.username.clone(),
                };
                let binding_for_select = binding.clone();
                div()
                    .id(("entra-account-choice", account.profile_id.len()))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .rounded(px(s(2.0)))
                    .bg(if is_selected {
                        Theme::accent()
                    } else {
                        Theme::panel_bg()
                    })
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|d| {
                        d.bg(if is_selected {
                            Theme::accent()
                        } else {
                            Theme::row_hover()
                        })
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_entra_account(Some(binding_for_select.clone()), cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(div().text_color(Theme::text()).child(label))
                            .child(div().text_color(Theme::text_dim()).child(
                                crate::localization::format(
                                    "connections.editor.entra_tenant_label",
                                    &[("tenant_id", &tenant)],
                                ),
                            )),
                    )
                    .into_any_element()
            }))
            // "Use no account" reset below the list, so the user can clear
            // the binding without re-saving.
            .child({
                let is_selected_none = selected.is_none();
                div()
                    .id("entra-account-choice-none")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .rounded(px(s(2.0)))
                    .bg(if is_selected_none {
                        Theme::accent()
                    } else {
                        Theme::panel_bg()
                    })
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|d| {
                        d.bg(if is_selected_none {
                            Theme::accent()
                        } else {
                            Theme::row_hover()
                        })
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.select_entra_account(None, cx);
                    }))
                    .child(crate::localization::text(
                        "connections.editor.use_no_account",
                    ))
            })
            .into_any_element()
    }

    fn build_profile(&self, cx: &mut Context<Self>) -> Result<ConnectionProfile, String> {
        let route = build_relay_route(
            self.via_relay,
            self.relay_device_id.as_deref(),
            &self.relay_devices,
            self.relay_hint.clone(),
        )?;
        let (tunnel_profile, transport) = self.build_transport_inputs(cx, true)?;
        let sql_account = Some(self.sql_account.unwrap_or_else(Uuid::new_v4));
        let (server, port) = self.sentinel_host_fallback(cx);
        let profile = assemble_profile(ProfileInputs {
            database_type: self.database_type,
            engine_auto_detect: self.engine_auto_detect,
            name: self.name.clone(),
            server,
            port,
            database: self.database.clone(),
            client: self.client.clone(),
            project: self.project.clone(),
            timeout: self.timeout.clone(),
            startup_sql: self.startup_sql.clone(),
            notes: self.notes.clone(),
            display_time_zone: self.display_time_zone.clone(),
            redis_key_delimiter: self.redis_key_delimiter.clone(),
            redis_cluster_mode: self.redis_cluster_mode,
            redis_node_map: self.redis_node_map.clone(),
            clickhouse_native_port: self.clickhouse_native_port.clone(),
            username: self.username.clone(),
            aws_region: self.aws_region.clone(),
            aws_profile: self.aws_profile.clone(),
            password: self.password_input.read(cx).value().to_string(),
            sql_account,
            auth_mode: self.auth_mode,
            entra_account_id: self.entra_account.clone(),
            environment: self.environment,
            encrypt: self.encrypt,
            trust: self.trust_certificate,
            tls_mode: crate::engine_form::uses_tls_modes(self.database_type)
                .then_some(self.tls_mode),
            tls_ca_file: self.tls_ca_file.clone(),
            tls_client_cert_file: self.tls_client_cert_file.clone(),
            tls_client_key_file: self.tls_client_key_file.clone(),
            tls_trust_os_roots: self.tls_trust_os_roots,
            tls_direct: self.tls_direct,
            block_select: self.block_select,
            rollback: self.rollback,
            confirm_changes: self.confirm_changes,
            allow_unbounded: self.allow_unbounded,
            production_override: self.production_override,
            editing_id: self.editing_id,
            persistable: true,
            route,
            tunnel_profile,
            transport,
        })?;
        self.with_sentinel(profile, true, cx)
    }

    /// Host and Port for the profile: a Sentinel profile with a blank Host
    /// shows its first sentinel there (it is never dialed for data).
    fn sentinel_host_fallback(&self, cx: &mut Context<Self>) -> (String, String) {
        if self.sentinel.enabled
            && sentinel::applies_to(self.database_type)
            && self.server.trim().is_empty()
            && let Ok(Some(config)) = self.sentinel.config(self.database_type, false, cx)
            && let Ok(addresses) = config.sentinel_addresses()
            && let Some((host, port)) = addresses.first()
        {
            return (host.clone(), port.to_string());
        }
        (self.server.clone(), self.port.clone())
    }

    /// Attach the form's Sentinel settings (or none) to a built profile.
    fn with_sentinel(
        &self,
        mut profile: ConnectionProfile,
        persistable: bool,
        cx: &mut Context<Self>,
    ) -> Result<ConnectionProfile, String> {
        profile.redis_sentinel = self.sentinel.config(self.database_type, persistable, cx)?;
        Ok(profile)
    }

    fn build_transport_inputs(
        &self,
        cx: &mut Context<Self>,
        persistable: bool,
    ) -> Result<(Option<Uuid>, TransportChain), String> {
        if let Some(id) = self.selected_tunnel_profile {
            return Ok((Some(id), TransportChain::default()));
        }
        if self.kube_enabled {
            let kube = build_kube_port_forward(
                true,
                &self.kube_kubeconfig,
                &self.kube_context,
                &self.kube_namespace,
                self.kube_target_kind,
                &self.kube_target_name,
                &self.kube_remote_port,
            )?;
            return Ok((
                None,
                kube.map(TransportChain::kube_only).unwrap_or_default(),
            ));
        }
        if self.ssm_enabled {
            let ssm = build_ssm_port_forward(
                true,
                &self.ssm_target,
                &self.ssm_region,
                &self.ssm_profile,
                &self.ssm_remote_host,
            )?;
            return Ok((None, ssm.map(TransportChain::ssm_only).unwrap_or_default()));
        }
        let transport = build_transport_chain(
            self.proxy_enabled,
            self.proxy_kind,
            &self.proxy_host,
            &self.proxy_port,
            &self.proxy_user,
            self.proxy_secret_input.read(cx).value().to_string(),
            self.proxy_secret_id,
            self.ssh_enabled,
            &self.ssh_host,
            &self.ssh_port,
            &self.ssh_user,
            self.ssh_auth,
            &self.ssh_key_path,
            self.ssh_secret_input.read(cx).value().to_string(),
            self.ssh_secret_id,
            self.ssh_jump_enabled,
            &self.ssh_jump_host,
            &self.ssh_jump_port,
            &self.ssh_jump_user,
            persistable,
        )?;
        let mut transport = transport;
        if let Some(ssh) = transport.ssh.as_mut() {
            ssh.keepalive = parse_ssh_keepalive(&self.ssh_keepalive, self.ssh_keepalive_max)?;
        }
        Ok((None, transport))
    }

    fn build_connectable_profile(
        &self,
        cx: &mut Context<Self>,
    ) -> Result<ConnectionProfile, String> {
        let name = self.name.trim().to_string();
        let name = if name.is_empty() {
            "__db_list_query__".to_string()
        } else {
            name
        };
        let (tunnel_profile, transport) = self.build_transport_inputs(cx, false)?;
        let (server, port) = self.sentinel_host_fallback(cx);
        let profile = assemble_profile(ProfileInputs {
            database_type: self.database_type,
            engine_auto_detect: self.engine_auto_detect,
            name,
            server,
            port,
            database: self.database.clone(),
            client: self.client.clone(),
            project: self.project.clone(),
            timeout: self.timeout.clone(),
            startup_sql: self.startup_sql.clone(),
            notes: self.notes.clone(),
            display_time_zone: self.display_time_zone.clone(),
            redis_key_delimiter: self.redis_key_delimiter.clone(),
            redis_cluster_mode: self.redis_cluster_mode,
            redis_node_map: self.redis_node_map.clone(),
            clickhouse_native_port: self.clickhouse_native_port.clone(),
            username: self.username.clone(),
            aws_region: self.aws_region.clone(),
            aws_profile: self.aws_profile.clone(),
            password: self.password_input.read(cx).value().to_string(),
            sql_account: self.sql_account,
            auth_mode: self.auth_mode,
            entra_account_id: self.entra_account.clone(),
            environment: self.environment,
            encrypt: self.encrypt,
            trust: self.trust_certificate,
            tls_mode: crate::engine_form::uses_tls_modes(self.database_type)
                .then_some(self.tls_mode),
            tls_ca_file: self.tls_ca_file.clone(),
            tls_client_cert_file: self.tls_client_cert_file.clone(),
            tls_client_key_file: self.tls_client_key_file.clone(),
            tls_trust_os_roots: self.tls_trust_os_roots,
            tls_direct: self.tls_direct,
            block_select: self.block_select,
            rollback: self.rollback,
            confirm_changes: self.confirm_changes,
            allow_unbounded: self.allow_unbounded,
            production_override: self.production_override,
            editing_id: self.editing_id,
            // The db-list probe honors the chosen route (a relay profile lists
            // its databases through the relay session). An unresolved relay
            // route is an error the probe surfaces — falling back to Direct
            // here would probe a server the route says is unreachable.
            route: build_relay_route(
                self.via_relay,
                self.relay_device_id.as_deref(),
                &self.relay_devices,
                self.relay_hint.clone(),
            )?,
            persistable: false,
            tunnel_profile,
            transport,
        })?;
        self.with_sentinel(profile, false, cx)
    }

    /// Whether this editor shows a relay proxy's read-only profile.
    fn read_only_proxied(&self) -> bool {
        crate::connection::connections_are_proxied()
    }

    fn handle_save(&mut self, cx: &mut Context<Self>) {
        if self.read_only_proxied() {
            self.error = Some(crate::localization::text(
                "connections.editor.proxy_save_read_only",
            ));
            cx.notify();
            return;
        }
        if self
            .editing_id
            .is_some_and(crate::connection::connection_is_provisioned)
        {
            self.error = Some(crate::localization::text(
                "connections.editor.provisioned_save_read_only",
            ));
            cx.notify();
            return;
        }
        let mut profile = match self.build_profile(cx) {
            Ok(profile) => profile,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };

        let password = self.password_input.read(cx).value().to_string();
        let mask = password_mask();
        // SQL-login passwords and libSQL auth tokens both live in the keyring.
        if let Some(secret_ref) = profile.authentication.secret_ref()
            && secret_ref.provider == SecretProvider::MacOsKeychain
        {
            let account_id = match Uuid::parse_str(&secret_ref.key) {
                Ok(id) => id,
                Err(_) => {
                    self.error = Some(crate::localization::text(
                        "connections.credentials.invalid_sql_account_id",
                    ));
                    cx.notify();
                    return;
                }
            };
            if !password.is_empty()
                && password != mask
                && let Err(e) = crate::sql_credentials::store_password(account_id, &password)
            {
                self.error = Some(e);
                cx.notify();
                return;
            }
        }

        if let Err(e) = self.sentinel.store_password(&profile, cx) {
            self.error = Some(e);
            cx.notify();
            return;
        }

        if let Some(tunnel) = &profile.ssh_tunnel
            && let Err(e) = store_ssh_secrets_from_tunnel(
                tunnel,
                self.ssh_secret_input.read(cx).value().as_ref(),
            )
        {
            self.error = Some(e);
            cx.notify();
            return;
        }

        let store = ConnectionStore::global();
        if let AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } = &profile.authentication
            && secret_ref.provider == SecretProvider::MacOsKeychain
        {
            let account_id = Uuid::parse_str(&secret_ref.key).expect("validated above");
            store.upsert_sql_auth_account(SqlAuthAccount {
                id: account_id,
                name: profile.name.clone(),
                username: username.clone(),
            });
        }
        if let Some(id) = self.editing_id {
            // The form does not expose observed status or the favorite star;
            // carry both forward so saving an edit never clears health history
            // or unstars the profile.
            if let Some(existing) = store.get(id) {
                profile.status = existing.status;
                profile.favorite = existing.favorite;
                // Redis database names and the visible-databases list are
                // set from the explorer, not this form.
                profile.redis_db_aliases = existing.redis_db_aliases;
                profile.redis_visible_databases = existing.redis_visible_databases;
                // Schemas hidden from the explorer are set there too.
                profile.hidden_schemas = existing.hidden_schemas;
            }
            store.update(id, profile.clone());
            // Saving a session-only connection (one opened from a URL) keeps
            // it; its password just went to the keyring above.
            store.keep_unsaved(id);
        } else if let Some(original) = self.duplicate_of {
            // A duplicate lands beside the profile it was copied from.
            store.add_after(original, profile.clone());
        } else {
            store.add(profile.clone());
        }
        Self::register_entities(&profile, store);
        cx.emit(ConnectionEditorEvent::Saved(Box::new(profile)));
    }

    fn handle_cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(ConnectionEditorEvent::Cancelled);
    }

    fn handle_test(&mut self, cx: &mut Context<Self>) {
        let mut profile = match self.build_connectable_profile(cx).and_then(|profile| {
            crate::sql_credentials::hydrate_profile(profile).map_err(crate::localization::message)
        }) {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(e);
                cx.notify();
                return;
            }
        };
        if let Err(e) = ConnectionStore::resolve_tunnel_profile(&mut profile) {
            self.error = Some(e);
            cx.notify();
            return;
        }

        self.testing = true;
        self.test_result = None;
        self.server_info = None;
        self.error = None;
        cx.notify();

        // Test over the route the form describes: a Via-Relay profile is
        // probed through its established relay session, and with no session
        // open the resolved services fail with the "connect the relay session
        // first" copy instead of dialing the server direct.
        let services = self.router.services_for(&profile);
        let log = self.log.clone();
        let provider = self.entra_sql_token_provider.clone();
        let profile_label = profile.name.clone();
        let profile_id = profile.id.0;
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            log.log_verbose(
                "connection_test",
                &format!("Testing connection \"{profile_label}\""),
            );
            // Re-mint the Entra SQL token before dialing so Test Connection
            // refreshes like a query window instead of using a stale token.
            let profile = match &provider {
                Some(provider) => {
                    match crate::azure_sql_token::hydrate_entra_token(provider, profile).await {
                        Ok(profile) => profile,
                        Err(message) => {
                            ConnectionStore::global().record_failure(
                                profile_id,
                                chrono::Utc::now(),
                                &message,
                            );
                            let mut cx = async_cx.clone();
                            weak_self
                                .update(&mut cx, |this, cx| {
                                    this.testing = false;
                                    this.test_success = false;
                                    this.test_result = Some(crate::localization::text(
                                "connections.editor.test_failed",
                            ));
                                    this.error = Some(message);
                                    cx.notify();
                                })
                                .ok();
                            return;
                        }
                    }
                }
                None => profile,
            };
            // web_time on wasm: std::time::Instant panics there.
            #[cfg(not(target_arch = "wasm32"))]
            use std::time::Instant;
            #[cfg(target_arch = "wasm32")]
            use web_time::Instant;
            let started = Instant::now();
            let result = run_connection_test(&services, &profile, &log).await;
            let latency_ms = started.elapsed().as_millis() as u64;
            // Sentinel: say which master answered and which sentinel named
            // it (direct profiles; a relay resolves the master on its side).
            #[cfg(not(target_arch = "wasm32"))]
            let sentinel_report = if result.is_ok()
                && profile.redis_sentinel.is_some()
                && profile.route == ConnectionRoute::Direct
            {
                let probe = profile.clone();
                crate::tokio_handle()
                    .spawn(async move {
                        sqlly_engines::redis::sentinel::test_connection_report(&probe).await
                    })
                    .await
                    .ok()
                    .and_then(|r| r.ok())
                    .flatten()
            } else {
                None
            };
            #[cfg(target_arch = "wasm32")]
            let sentinel_report: Option<String> = None;
            if let Some(report) = &sentinel_report {
                log.log_verbose("connection_test", report);
            }
            let info = if result.is_ok() {
                match collect_server_info(services.queries.as_ref(), &profile).await {
                    Ok(mut info) => {
                        info.latency_ms = Some(latency_ms);
                        Some(info)
                    }
                    Err(e) => {
                        log.log_verbose(
                            "connection_test",
                            &format!(
                                "Connection test succeeded for \"{profile_label}\" but server info probe failed: {e}"
                            ),
                        );
                        None
                    }
                }
            } else {
                None
            };
            match &result {
                Ok(()) => log.log_verbose(
                    "connection_test",
                    &format!("Connection test succeeded for \"{profile_label}\""),
                ),
                Err(e) => log.log_error(
                    "connection_test",
                    &format!("Connection test failed for \"{profile_label}\": {e}"),
                ),
            }
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.testing = false;
                    match result {
                        Ok(()) => {
                            ConnectionStore::global()
                                .record_test_success(profile_id, chrono::Utc::now());
                            this.test_success = true;
                            if this.engine_auto_detect
                                && let Some(info) = &info
                            {
                                this.database_type = sqlly_core::redis_flavor::detected(
                                    &this.server,
                                    Some(&info.product),
                                );
                            }
                            this.server_info = info;
                            this.test_result = Some(match &sentinel_report {
                                Some(report) => crate::localization::format(
                                    "connections.editor.test_connected_with_report",
                                    &[
                                        ("latency_ms", &latency_ms.to_string()),
                                        ("report", report),
                                    ],
                                ),
                                None => crate::localization::format(
                                    "connections.editor.test_connected",
                                    &[("latency_ms", &latency_ms.to_string())],
                                ),
                            });
                        }
                        Err(e) => {
                            ConnectionStore::global().record_failure_message(
                                profile_id,
                                chrono::Utc::now(),
                                &e.message(),
                            );
                            this.test_success = false;
                            this.test_result = Some(crate::localization::text(
                                "connections.editor.test_failed",
                            ));
                            // The failure reason (including the relay-session
                            // guidance) belongs in the form, not just the log.
                            this.error = Some(crate::localization::message(e.message()));
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Switch the form to `engine`: port, TLS, placeholder and sign-in
    /// defaults follow. An Auto-detect switch onto a managed AWS cache also
    /// turns TLS on (MemoryDB and IAM sign-in need it).
    fn apply_engine(&mut self, engine: DatabaseType, window: &mut Window, cx: &mut Context<Self>) {
        let old = self.database_type;
        self.database_type = engine;
        self.encrypt = crate::engine_form::encrypt_after_engine_change(self.encrypt, engine);
        let port = crate::engine_form::port_after_engine_change(&self.port, old, engine);
        if port != self.port {
            self.port = port.clone();
            self.port_input
                .update(cx, |state, cx| state.set_value(port, window, cx));
        }
        let database_placeholder = crate::engine_form::database_placeholder(engine);
        self.database_input.update(cx, |state, cx| {
            state.set_placeholder(database_placeholder, window, cx);
        });
        if crate::engine_form::is_file_engine(engine) {
            self.auth_mode = AuthMode::SqlLogin;
            self.host_validation = HostValidationResult::Valid;
        } else {
            // Moving within the Redis family (Auto-detect following the
            // host) keeps a sign-in the new flavor still accepts, e.g. IAM.
            let same_family_keeps_auth = old.is_command_engine()
                && engine.is_command_engine()
                && self.auth_mode.supported_for(engine);
            if !same_family_keeps_auth
                && (!matches!(
                    engine,
                    DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase
                ) || (engine == DatabaseType::AzureSqlDatabase
                    && self.auth_mode == AuthMode::Integrated))
            {
                // libSQL / Turso signs in with a token, everything
                // else with a SQL login.
                self.auth_mode = AuthMode::default_for(engine);
            }
            self.host_validation = validate_server_text(engine, &self.server);
        }
        if self.engine_auto_detect {
            let mode = crate::engine_form::tls_after_auto_detect(self.tls_mode, old, engine);
            if mode != self.tls_mode {
                self.set_tls_mode(mode, cx);
                self.sync_tls_mode_select(window, cx);
            }
        }
        self.test_result = None;
        self.server_info = None;
        self.error = None;
        cx.notify();
    }

    fn toggle_trust_os_roots(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.tls_trust_os_roots = checked;
        cx.notify();
    }

    fn toggle_tls_direct(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.tls_direct = checked;
        cx.notify();
    }

    fn toggle_encrypt(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.encrypt = checked;
        cx.notify();
    }

    /// Writes the selected TLS mode and keeps the two legacy booleans in
    /// sync so builds that predate `tls_mode` read a safe equivalent.
    fn set_tls_mode(&mut self, mode: TlsMode, cx: &mut Context<Self>) {
        self.tls_mode = mode;
        let (encrypt, trust) = mode.legacy_flags();
        self.encrypt = encrypt;
        self.trust_certificate = trust;
        cx.notify();
    }

    /// The TLS controls for the engines in `engine_form::uses_tls_modes`
    /// (PostgreSQL and MySQL wire, ClickHouse, libSQL / Turso): the four-way
    /// mode select plus the CA / client certificate / client key path rows,
    /// each with a Browse… button on native builds.
    fn render_tls_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let mut section = div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .child(Self::labeled_field(
                &crate::localization::text("connections.editor.tls_label"),
                Select::new(&self.tls_mode_select),
                cx,
            ));
        let rows = [
            (
                crate::localization::text("connections.editor.ca_file_label"),
                &self.tls_ca_input,
                TlsPathField::Ca,
                "ce-tls-browse-ca",
            ),
            (
                crate::localization::text("connections.editor.client_certificate_label"),
                &self.tls_cert_input,
                TlsPathField::ClientCert,
                "ce-tls-browse-cert",
            ),
            (
                crate::localization::text("connections.editor.client_key_label"),
                &self.tls_key_input,
                TlsPathField::ClientKey,
                "ce-tls-browse-key",
            ),
        ];
        for (label, input, field, browse_id) in rows {
            let input = Input::new(input).font_family(Theme::editor_font_family());
            #[cfg(not(target_arch = "wasm32"))]
            {
                let browse_entity = cx.entity();
                section = section.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(s(8.0)))
                        .items_end()
                        .child(div().flex_1().child(Self::labeled_field(&label, input, cx)))
                        .child(
                            Button::new(browse_id)
                                .ghost()
                                .label(crate::localization::text(
                                    "connections.editor.tls_browse_button",
                                ))
                                .on_click(move |_, window, app| {
                                    browse_entity.update(app, |this, cx| {
                                        this.handle_browse_tls_file(field, window, cx);
                                    });
                                }),
                        ),
                );
            }
            #[cfg(target_arch = "wasm32")]
            {
                let _ = (field, browse_id);
                section = section.child(Self::labeled_field(&label, input, cx));
            }
        }
        // Redis verifies against the bundled public roots (or the CA file
        // alone); this adds the machine's own trust store, so a company CA
        // installed there verifies without a CA file.
        if self.database_type.is_command_engine()
            && matches!(self.tls_mode, TlsMode::VerifyCa | TlsMode::VerifyFull)
        {
            section = section.child(Self::render_checkbox(
                "ce-cb-tls-os-roots",
                &crate::localization::text("connections.editor.trust_os_roots_label"),
                self.tls_trust_os_roots,
                false,
                cx,
                Self::toggle_trust_os_roots,
            ));
        }
        // PostgreSQL 17 can open the TLS handshake straight away, which a
        // TLS proxy routing databases by host name needs.
        if self.database_type.connection_kind() == sqlly_core::ConnectionKind::PostgresWire
            && self.tls_mode != TlsMode::Disable
        {
            section = section.child(Self::render_checkbox(
                "ce-cb-tls-direct",
                &crate::localization::text("connections.editor.direct_tls_label"),
                self.tls_direct,
                false,
                cx,
                Self::toggle_tls_direct,
            ));
        }
        section.into_any_element()
    }

    fn sync_tls_mode_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tls_mode_select_stale = false;
        let label = tls_mode_label(self.tls_mode).to_string();
        self.tls_mode_select.update(cx, |state, cx| {
            state.set_items(tls_mode_labels(), window, cx);
            state.set_selected_value(&label, window, cx);
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn handle_browse_tls_file(
        &mut self,
        field: TlsPathField,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = crate::localization::text(match field {
            TlsPathField::Ca => "connections.editor.select_ca_certificate_title",
            TlsPathField::ClientCert => "connections.editor.select_client_certificate_title",
            TlsPathField::ClientKey => "connections.editor.select_client_key_title",
        });
        let file = rfd::FileDialog::new()
            .add_filter(
                crate::localization::text("connections.editor.certificate_file_filter"),
                &["pem", "crt", "cer", "key"],
            )
            .set_title(title)
            .pick_file();
        if let Some(path) = file {
            crate::sandbox_access::remember(&path);
            let value = path.to_string_lossy().to_string();
            let input = match field {
                TlsPathField::Ca => {
                    self.tls_ca_file = value.clone();
                    &self.tls_ca_input
                }
                TlsPathField::ClientCert => {
                    self.tls_client_cert_file = value.clone();
                    &self.tls_cert_input
                }
                TlsPathField::ClientKey => {
                    self.tls_client_key_file = value.clone();
                    &self.tls_key_input
                }
            };
            input.update(cx, |state, cx| {
                state.set_value(value, window, cx);
            });
            cx.notify();
        }
    }

    fn toggle_trust(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.trust_certificate = checked;
        cx.notify();
    }

    fn toggle_block_select(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.block_select = checked;
        cx.notify();
    }

    fn toggle_confirm_changes(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.confirm_changes = checked;
        cx.notify();
    }

    fn toggle_allow_unbounded(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.allow_unbounded = checked;
        cx.notify();
    }

    fn toggle_rollback(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.rollback = checked;
        cx.notify();
    }

    fn toggle_database_all(&mut self, checked: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.database_all = checked;
        self.database_dropdown_open = false;
        if self.database_all {
            self.database.clear();
            self.database_input
                .update(cx, |s, cx| s.set_value("", window, cx));
        }
        cx.notify();
    }

    /// Opens/closes the database dropdown. `open` is computed from the
    /// render-time snapshot of `database_dropdown_open` (not re-read here), so
    /// the popup's capture-phase `on_mouse_down_out` dismissal and a click on
    /// the caret compose to "closed" instead of fighting and re-opening.
    fn set_database_dropdown_open(&mut self, open: bool, cx: &mut Context<Self>) {
        if self.database_all {
            return;
        }
        self.database_dropdown_open = open;
        if open && self.database_list.is_none() && !self.database_loading {
            self.load_databases(cx);
        }
        cx.notify();
    }

    fn select_database(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.database = name.clone();
        self.database_input
            .update(cx, |s, cx| s.set_value(name, window, cx));
        self.database_dropdown_open = false;
        cx.notify();
    }

    /// Completion candidates for `field`: the names already in the store that
    /// contain what has been typed so far.
    ///
    /// An exact match is filtered out — once the field says "Acme" there is
    /// nothing left for the list to offer, and leaving the row up covers the
    /// fields below it for no reason.
    fn entity_suggestions(&self, field: EntityField) -> Vec<String> {
        let (pool, typed) = match field {
            EntityField::Client => (&self.known_clients, self.client.trim()),
            EntityField::Project => (&self.known_projects, self.project.trim()),
        };
        let needle = typed.to_lowercase();
        pool.iter()
            .filter(|name| needle.is_empty() || name.to_lowercase().contains(&needle))
            .filter(|name| !name.eq_ignore_ascii_case(typed))
            .cloned()
            .collect()
    }

    /// Accept a completion. `set_value` deliberately does not emit `Change`, so
    /// this cannot bounce off the input's own subscription and reopen the list.
    fn select_entity_suggestion(
        &mut self,
        field: EntityField,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match field {
            EntityField::Client => {
                self.client = name.clone();
                self.client_input
                    .update(cx, |s, cx| s.set_value(name, window, cx));
            }
            EntityField::Project => {
                self.project = name.clone();
                self.project_input
                    .update(cx, |s, cx| s.set_value(name, window, cx));
            }
        }
        self.entity_suggestions_open = None;
        cx.notify();
    }

    /// Give a client/project name typed into this editor a home in the store,
    /// so the next connection can complete against it and the entity manager
    /// can hang a color and safety policy off it.
    ///
    /// Matching is case-insensitive: "Acme" typed over an existing "ACME" is
    /// the same client, not a second one.
    fn register_entities(profile: &ConnectionProfile, store: &ConnectionStore) {
        if let Some(name) = profile.client.as_deref()
            && !store
                .clients()
                .iter()
                .any(|c| c.name.eq_ignore_ascii_case(name))
        {
            store.upsert_client(ClientEntity {
                id: Uuid::new_v4(),
                name: name.to_string(),
                color_hex: None,
                image_path: None,
                safety: SafetyFeatures::default(),
            });
        }
        if let Some(name) = profile.project.as_deref()
            && !store
                .projects()
                .iter()
                .any(|p| p.name.eq_ignore_ascii_case(name))
        {
            store.upsert_project(ProjectEntity {
                id: Uuid::new_v4(),
                name: name.to_string(),
                color_hex: None,
                image_path: None,
                safety: SafetyFeatures::default(),
            });
        }
    }

    fn load_databases(&mut self, cx: &mut Context<Self>) {
        let profile = match self.build_connectable_profile(cx).and_then(|profile| {
            crate::sql_credentials::hydrate_profile(profile).map_err(crate::localization::message)
        }) {
            Ok(p) => p,
            Err(e) => {
                self.database_list_error = Some(e);
                self.database_loading = false;
                cx.notify();
                return;
            }
        };
        self.database_loading = true;
        self.database_list_error = None;
        cx.notify();
        // Same route-aware resolution as Test Connection (see `handle_test`).
        let services = self.router.services_for(&profile);
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let result = services.connections.list_databases(&profile).await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.database_loading = false;
                    match result {
                        Ok(dbs) => {
                            this.database_list = Some(dbs.into_iter().map(|d| d.name).collect());
                            this.database_list_error = None;
                        }
                        Err(e) => {
                            this.database_list_error =
                                Some(crate::localization::message(e.message()));
                            this.database_list = None;
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    fn set_environment(&mut self, env: ConnectionEnvironment, cx: &mut Context<Self>) {
        self.environment = env;
        // Mirror the Swift safety defaults: Production/Staging force the
        // rollback safety net on; Production also forces SELECT-only.
        let (force_block, force_rollback) = environment_safety_defaults(env);
        if force_block {
            self.block_select = true;
        }
        if force_rollback {
            self.rollback = true;
        }
        self.test_result = None;
        self.server_info = None;
        cx.notify();
    }

    fn maybe_prefill_aws_region_from_server(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        let _ = (window, cx);
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.auth_mode != AuthMode::AwsRdsIam || !self.aws_region.trim().is_empty() {
                return;
            }
            let Some(region) = crate::cloud_iam_token::aws_region_from_host(&self.server) else {
                return;
            };
            self.aws_region = region.clone();
            self.aws_region_input
                .update(cx, |state, cx| state.set_value(region, window, cx));
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn ensure_aws_profiles_loaded(&mut self, cx: &mut Context<Self>) {
        if self.aws_profiles_loaded {
            return;
        }
        self.aws_profiles_loaded = true;
        let mut async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let profiles = crate::tokio_handle()
                .spawn(async { sqlly_services::cloud_auth::aws::list_profiles().await })
                .await
                .unwrap_or_default();
            let mut labels = vec![crate::cloud_iam_token::aws_profile_default_label()];
            labels.extend(profiles);
            let _ = weak.update(&mut async_cx, |editor, cx| {
                editor.aws_profile_options = labels;
                editor.aws_profile_select_stale = true;
                cx.notify();
            });
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn sync_aws_profile_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.aws_profile_options.is_empty() {
            self.aws_profile_options = vec![crate::cloud_iam_token::aws_profile_default_label()];
        }
        let row = crate::cloud_iam_token::aws_profile_select_row(
            &self.aws_profile,
            &self.aws_profile_options,
        );
        let items = unique_select_labels(self.aws_profile_options.iter().cloned());
        self.aws_profile_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            state.set_selected_index(row.map(gpui_component::IndexPath::new), window, cx);
        });
        self.aws_profile_select_stale = false;
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn refresh_gcp_adc_status(&mut self, cx: &mut Context<Self>) {
        let mut async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            // The check runs on the tokio runtime; its text is resolved back
            // here, where the UI thread's language is active.
            let status = match crate::tokio_handle()
                .spawn(async {
                    sqlly_services::cloud_auth::gcp::adc_status()
                        .await
                        .map(drop)
                        .map_err(|error| error.to_string())
                })
                .await
            {
                Ok(Ok(())) => crate::localization::text("connections.editor.gcp_adc_found"),
                Ok(Err(detail)) => crate::localization::format(
                    "connections.editor.gcp_adc_error",
                    &[("error", detail.as_str())],
                ),
                Err(_) => crate::localization::text("connections.editor.gcp_adc_check_failed"),
            };
            let _ = weak.update(&mut async_cx, |editor, cx| {
                editor.gcp_adc_status = status;
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_iam_auth_mode_side_effects(
        &mut self,
        mode: AuthMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(mode, AuthMode::AwsRdsIam | AuthMode::GcpCloudSqlIam) {
            self.encrypt = true;
        }
        match mode {
            AuthMode::AwsRdsIam => {
                self.username_input.update(cx, |state, cx| {
                    state.set_placeholder("jane_doe", window, cx);
                });
                #[cfg(not(target_arch = "wasm32"))]
                self.ensure_aws_profiles_loaded(cx);
                self.maybe_prefill_aws_region_from_server(window, cx);
            }
            AuthMode::GcpCloudSqlIam => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let placeholder =
                        crate::cloud_iam_token::gcp_iam_username_placeholder(self.database_type);
                    self.username_input.update(cx, |state, cx| {
                        state.set_placeholder(placeholder, window, cx);
                    });
                    self.refresh_gcp_adc_status(cx);
                }
            }
            AuthMode::SqlLogin => {
                self.username_input.update(cx, |state, cx| {
                    state.set_placeholder("sa", window, cx);
                });
            }
            // Redis with Entra: the optional Redis user name.
            AuthMode::EntraBrowser if self.database_type.is_command_engine() => {
                self.username_input.update(cx, |state, cx| {
                    state.set_placeholder(
                        crate::localization::text("connections.editor.redis_user_placeholder"),
                        window,
                        cx,
                    );
                });
            }
            _ => {}
        }
    }

    fn set_auth_mode(&mut self, mode: AuthMode, window: &mut Window, cx: &mut Context<Self>) {
        self.auth_mode = mode;
        self.apply_iam_auth_mode_side_effects(mode, window, cx);
        if mode == AuthMode::EntraBrowser {
            self.reload_entra_accounts();
            if self.entra_accounts.len() == 1 {
                self.entra_account = sole_entra_binding(&self.entra_accounts);
            }
            // Discovery is best-effort: kick it off to populate the picker, but
            // don't open the dropdown or otherwise gate manual entry on it, so a
            // user without list permission can just type the server name.
            self.azure_servers_loading = true;
            self.azure_servers_error = None;
            self.azure_server_dropdown_open = false;
            self.azure_server_help_copied = false;
            cx.emit(ConnectionEditorEvent::RefreshAzureServersRequested);
        } else {
            self.azure_server_dropdown_open = false;
        }
        self.test_result = None;
        self.server_info = None;
        self.error = None;
        cx.notify();
    }

    fn apply_pasted_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.url_paste_input.read(cx).value().to_string();
        match sqlly_core::parse_connection_url(&raw) {
            Err(error) => {
                let error = crate::localization::message(error.message());
                self.error = Some(crate::localization::format(
                    "connections.editor.connection_string_read_failed",
                    &[("error", error.as_str())],
                ));
            }
            Ok(parsed) => {
                self.prefill_from_parsed(parsed, window, cx);
            }
        }
        cx.notify();
    }

    pub fn prefill_from_parsed(
        &mut self,
        parsed: sqlly_core::ParsedConnectionUrl,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !crate::engine_form::SELECTABLE_ENGINES.contains(&parsed.engine) {
            self.error = Some(crate::localization::format(
                "connections.editor.unsupported_engine",
                &[("engine_name", parsed.engine.display_name())],
            ));
            cx.notify();
            return;
        }
        // A generic `redis://` paste keeps Auto-detect; a flavored scheme
        // (`valkey://`) or any other engine is an explicit choice.
        self.engine_auto_detect &= parsed.engine == DatabaseType::Redis;
        self.database_type = if self.engine_auto_detect {
            crate::engine_form::auto_detect_engine(&parsed.server, self.database_type)
        } else {
            parsed.engine
        };
        let engine_label =
            crate::engine_form::picker_label(self.database_type, self.engine_auto_detect);
        self.engine_select.update(cx, |state, cx| {
            state.set_selected_value(&engine_label, window, cx)
        });
        self.server = parsed.server.clone();
        self.server_input
            .update(cx, |s, cx| s.set_value(parsed.server.clone(), window, cx));
        self.port = if crate::engine_form::is_file_engine(parsed.engine) {
            String::new()
        } else {
            parsed
                .port
                .map(|p| p.to_string())
                .unwrap_or_else(|| crate::engine_form::default_port(parsed.engine).to_string())
        };
        self.port_input
            .update(cx, |s, cx| s.set_value(self.port.clone(), window, cx));
        self.database = parsed.database.clone().unwrap_or_default();
        self.database_input
            .update(cx, |s, cx| s.set_value(self.database.clone(), window, cx));
        if let Some(username) = &parsed.username {
            self.username = username.clone();
            self.username_input
                .update(cx, |s, cx| s.set_value(username.clone(), window, cx));
        }
        let (mode, encrypt, trust) = crate::engine_form::tls_after_paste(
            (self.tls_mode, self.encrypt, self.trust_certificate),
            &parsed,
        );
        self.tls_mode = mode;
        self.encrypt = encrypt;
        self.trust_certificate = trust;
        self.sync_tls_mode_select(window, cx);
        if let Some(direct) = parsed.tls_direct {
            self.tls_direct = direct;
        }
        if let Some(path) = &parsed.tls_ca_file {
            self.tls_ca_file = path.to_string_lossy().to_string();
            let value = self.tls_ca_file.clone();
            self.tls_ca_input
                .update(cx, |s, cx| s.set_value(value, window, cx));
        }
        if let Some(path) = &parsed.tls_client_cert_file {
            self.tls_client_cert_file = path.to_string_lossy().to_string();
            let value = self.tls_client_cert_file.clone();
            self.tls_cert_input
                .update(cx, |s, cx| s.set_value(value, window, cx));
        }
        if let Some(path) = &parsed.tls_client_key_file {
            self.tls_client_key_file = path.to_string_lossy().to_string();
            let value = self.tls_client_key_file.clone();
            self.tls_key_input
                .update(cx, |s, cx| s.set_value(value, window, cx));
        }
        if parsed.integrated_security
            && matches!(
                parsed.engine,
                DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase
            )
        {
            self.set_auth_mode(AuthMode::Integrated, window, cx);
        } else if crate::engine_form::uses_auth_token(parsed.engine) {
            // A libsql:// URL's authToken lands in the token field below.
            self.set_auth_mode(AuthMode::AuthToken, window, cx);
            self.sql_account = None;
        } else if parsed.username.is_some() || parsed.password.is_some() {
            self.set_auth_mode(AuthMode::SqlLogin, window, cx);
            self.sql_account = None;
        }
        if self.name.trim().is_empty() {
            self.name = crate::engine_form::derived_connection_name(&parsed);
            self.name_input
                .update(cx, |s, cx| s.set_value(self.name.clone(), window, cx));
        }
        self.sentinel.prefill(&parsed, window, cx);
        if let Some(password) = parsed.password {
            self.password_input
                .update(cx, |f, cx| f.set_value(password, window, cx));
        }
        self.host_validation = if crate::engine_form::is_file_engine(parsed.engine) {
            if self.server.trim().is_empty() {
                HostValidationResult::Empty
            } else {
                HostValidationResult::Valid
            }
        } else {
            validate_host(&self.server)
        };
        self.url_paste_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.url_paste_open = false;
        let ignored = parsed.extra_options.len();
        self.error = None;
        self.test_result = Some(if ignored == 0 {
            crate::localization::text("connections.editor.connection_string_filled")
        } else {
            crate::localization::format(
                "connections.editor.connection_string_filled_ignored",
                &[("ignored_count", &ignored.to_string())],
            )
        });
        self.test_success = true;
        cx.notify();
    }

    fn apply_sql_account(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        // Clicking the account that is already chosen releases the credential
        // fields — the only way back to typing a one-off username/password once
        // an account has claimed them.
        if self.sql_account == Some(id) {
            self.clear_sql_account(window, cx);
            return;
        }
        let Some(account) = ConnectionStore::global()
            .sql_auth_accounts()
            .into_iter()
            .find(|account| account.id == id)
        else {
            return;
        };
        match crate::sql_credentials::load_password(id) {
            Ok(Some(_)) => {
                self.username = account.username.clone();
                self.username_input.update(cx, |field, cx| {
                    field.set_value(account.username, window, cx)
                });
                self.password_input
                    .update(cx, |field, cx| field.set_value(password_mask(), window, cx));
                self.sql_account = Some(id);
                self.error = None;
            }
            Ok(None) => {
                self.error = Some(crate::localization::format(
                    "connections.editor.sql_account_password_missing",
                    &[("account_name", &account.name)],
                ));
            }
            Err(error) => {
                self.error = Some(crate::localization::format(
                    "connections.editor.sql_account_password_read_failed",
                    &[("error", &error)],
                ));
            }
        }
        cx.notify();
    }

    /// Unbind the profile from its saved account, handing the username and
    /// password fields back to the user. The placeholder password goes with it:
    /// leaving the mask behind would save twenty bullets as a real password.
    fn clear_sql_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sql_account = None;
        self.password_input
            .update(cx, |field, cx| field.set_value("", window, cx));
        self.error = None;
        cx.notify();
    }

    /// Kick off the one-shot async fetch of the account's relay devices +
    /// route pairs. Started lazily from the render path (the app constructs the
    /// editor and we do not edit it), guarded so it runs at most once. On the
    /// browser build the store relay calls are unsupported, so this no-ops and
    /// the routing control stays gated closed.
    fn ensure_relay_devices_loaded(&mut self, cx: &mut Context<Self>) {
        if self.relay_load_started {
            return;
        }
        self.relay_load_started = true;

        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(session) = crate::store::session::load() else {
                self.relay_signed_in = false;
                return;
            };
            if session.is_expired() {
                self.relay_signed_in = false;
                return;
            }
            self.relay_signed_in = true;
            self.relay_loading = true;
            let token = session.access_token.clone();
            let control_device_id = session.device_id.clone();
            let async_cx = cx.to_async();
            let weak_self = cx.weak_entity();
            cx.spawn(async move |_this, _cx| {
                let devices = crate::store::list_relay_devices(&token).await;
                // Bridges shared with this account are additive: a failure
                // (e.g. an older server without the endpoint) must not break
                // the owned-device list, so it degrades to empty.
                let shared = crate::store::list_shared_relay_devices(&token)
                    .await
                    .unwrap_or_default();
                let pairs = crate::store::list_relay_pairs(&token).await;
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        this.relay_loading = false;
                        match (devices, pairs) {
                            (Ok(devices), Ok(pairs)) => {
                                this.relay_devices = merge_relay_devices(
                                    &devices,
                                    &shared,
                                    &pairs,
                                    control_device_id.as_deref(),
                                );
                                this.relay_select_stale = true;
                                this.relay_error = None;
                                // Auto-select the sole device when Via Relay is
                                // already on but nothing is selected yet.
                                if this.via_relay
                                    && this.relay_device_id.is_none()
                                    && this.relay_devices.len() == 1
                                {
                                    this.relay_device_id =
                                        Some(this.relay_devices[0].device_id.clone());
                                }
                            }
                            (Err(e), _) | (_, Err(e)) => {
                                this.relay_error = Some(crate::localization::message(
                                    crate::remote_session::store_error_message(e),
                                ));
                            }
                        }
                        cx.notify();
                    })
                    .ok();
            })
            .detach();
        }
    }

    fn toggle_via_relay(&mut self, checked: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.via_relay = checked;
        if checked && self.relay_device_id.is_none() && self.relay_devices.len() == 1 {
            self.relay_device_id = Some(self.relay_devices[0].device_id.clone());
            self.relay_select_stale = true;
        }
        self.test_result = None;
        self.server_info = None;
        self.error = None;
        cx.notify();
    }

    fn select_relay_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        self.relay_device_id = Some(device_id);
        // A confirm armed for the previously selected device must not carry
        // over to the new one.
        self.pending_revoke_pair = None;
        self.test_result = None;
        self.server_info = None;
        self.error = None;
        cx.notify();
    }

    /// Authorize a bridge: create a route pair between THIS app device (control)
    /// and the selected relay connector, then refresh the picker so the device
    /// resolves an active `pair_id` and becomes routable. The pairing is a live
    /// control-plane action (`POST /api/relay/pairs`).
    #[cfg(not(target_arch = "wasm32"))]
    fn authorize_relay_device(&mut self, connector_device_id: String, cx: &mut Context<Self>) {
        let Some(session) = crate::store::session::load().filter(|s| !s.is_expired()) else {
            self.relay_error = Some(crate::localization::text(
                "connections.editor.relay_signin_to_authorize",
            ));
            cx.notify();
            return;
        };
        let Some(control_device_id) = session.device_id.clone() else {
            self.relay_error = Some(crate::localization::text(
                "connections.editor.relay_device_not_registered",
            ));
            cx.notify();
            return;
        };
        let token = session.access_token.clone();
        self.relay_loading = true;
        self.relay_error = None;
        cx.notify();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let created =
                crate::store::create_relay_pair(&token, &control_device_id, &connector_device_id)
                    .await;
            // Re-read devices + pairs so the newly authorized connector resolves.
            let devices = crate::store::list_relay_devices(&token).await;
            let shared = crate::store::list_shared_relay_devices(&token)
                .await
                .unwrap_or_default();
            let pairs = crate::store::list_relay_pairs(&token).await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.relay_loading = false;
                    match created {
                        Ok(_) => {
                            if let (Ok(devices), Ok(pairs)) = (devices, pairs) {
                                this.relay_devices = merge_relay_devices(
                                    &devices,
                                    &shared,
                                    &pairs,
                                    Some(control_device_id.as_str()),
                                );
                                this.relay_select_stale = true;
                            }
                            this.relay_error = None;
                            // Select the just-authorized device for convenience.
                            this.relay_device_id = Some(connector_device_id.clone());
                        }
                        Err(e) => {
                            let detail = crate::localization::message(
                                crate::remote_session::store_error_message(e),
                            );
                            this.relay_error = Some(crate::localization::format(
                                "connections.editor.relay_authorize_failed",
                                &[("error", detail.as_str())],
                            ));
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Revoke an authorized route pair (`DELETE /api/relay/pairs/{id}`), then
    /// re-read devices + pairs so the picker reflects the device's new
    /// unpaired state. Reached only through the armed confirm step
    /// ([`Self::pending_revoke_pair`]).
    #[cfg(not(target_arch = "wasm32"))]
    fn revoke_relay_pair(&mut self, pair_id: String, cx: &mut Context<Self>) {
        let Some(session) = crate::store::session::load().filter(|s| !s.is_expired()) else {
            self.relay_error = Some(crate::localization::text(
                "connections.editor.relay_signin_to_revoke",
            ));
            self.pending_revoke_pair = None;
            cx.notify();
            return;
        };
        let token = session.access_token.clone();
        let control_device_id = session.device_id.clone();
        self.relay_loading = true;
        self.relay_error = None;
        self.pending_revoke_pair = None;
        cx.notify();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let revoked = crate::store::delete_relay_pair(&token, &pair_id).await;
            // Re-read devices + pairs so the revoked connector resolves as
            // unpaired (mirrors the authorize flow's refresh).
            let devices = crate::store::list_relay_devices(&token).await;
            let shared = crate::store::list_shared_relay_devices(&token)
                .await
                .unwrap_or_default();
            let pairs = crate::store::list_relay_pairs(&token).await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.relay_loading = false;
                    match revoked {
                        Ok(()) => {
                            if let (Ok(devices), Ok(pairs)) = (devices, pairs) {
                                this.relay_devices = merge_relay_devices(
                                    &devices,
                                    &shared,
                                    &pairs,
                                    control_device_id.as_deref(),
                                );
                                this.relay_select_stale = true;
                            }
                            this.relay_error = None;
                        }
                        Err(e) => {
                            let detail = crate::localization::message(
                                crate::remote_session::store_error_message(e),
                            );
                            this.relay_error = Some(crate::localization::format(
                                "connections.editor.relay_revoke_failed",
                                &[("error", detail.as_str())],
                            ));
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }

    /// Rebuild the relay-device dropdown's items from `relay_devices` and
    /// re-apply the current selection. Labels double as select values, so a
    /// duplicate device name gets its device id appended to stay unique.
    fn sync_relay_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.relay_select_stale = false;
        self.relay_select_choices = self
            .relay_devices
            .iter()
            .map(|device| {
                let duplicated = self
                    .relay_devices
                    .iter()
                    .filter(|d| d.name == device.name)
                    .count()
                    > 1;
                let label = if duplicated {
                    format!("{} ({})", device.name, device.device_id)
                } else {
                    device.name.clone()
                };
                (label, device.device_id.clone())
            })
            .collect();
        let items: Vec<String> = self
            .relay_select_choices
            .iter()
            .map(|(label, _)| label.clone())
            .collect();
        let selected = self.relay_device_id.as_deref().and_then(|id| {
            self.relay_select_choices
                .iter()
                .find(|(_, device_id)| device_id == id)
                .map(|(label, _)| label.clone())
        });
        self.relay_device_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            if let Some(label) = selected {
                state.set_selected_value(&label, window, cx);
            }
        });
    }

    /// The pairing state of the currently selected relay device, if one is
    /// selected: `(device, active_pair_id)`.
    fn selected_relay_device(&self) -> Option<&RelayDeviceChoice> {
        let id = self.relay_device_id.as_deref()?;
        self.relay_devices.iter().find(|d| d.device_id == id)
    }

    /// The routing section: a gate hint when the user cannot route via relay
    /// (not signed in, or no registered relay devices), otherwise a checkbox to
    /// enable Via Relay plus the relay-device picker.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    fn render_routing_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let eligible = self.relay_signed_in && !self.relay_devices.is_empty();
        let col = div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .child(Self::section_label(&crate::localization::text(
                "connections.editor.routing_section",
            )));

        if !eligible {
            let hint = if self.relay_loading {
                crate::localization::text("connections.editor.relay_devices_loading")
            } else if let Some(err) = &self.relay_error {
                crate::localization::format(
                    "connections.editor.relay_devices_load_failed",
                    &[("error", err)],
                )
            } else if !self.relay_signed_in {
                crate::localization::text("connections.editor.relay_signin_required_hint")
            } else {
                crate::localization::text("connections.editor.no_relay_devices_hint")
            };
            return col
                .child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(hint),
                )
                .into_any_element();
        }

        col.child(Self::render_checkbox(
            "ce-cb-via-relay",
            &crate::localization::text("connections.editor.route_via_relay_label"),
            self.via_relay,
            false,
            cx,
            Self::toggle_via_relay,
        ))
        .when(self.via_relay, |d| {
            d.child(self.render_relay_device_picker(cx))
        })
        .into_any_element()
    }

    /// SSH tunnel section: gated by engine capability; when available, a
    /// checkbox enables tunnel fields (target hop, auth, optional jump host).
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    fn render_ssh_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.ssh_auth_select_stale {
            self.sync_ssh_auth_select(window, cx);
        }
        if self.tunnel_profile_select_stale {
            self.sync_tunnel_profile_select(window, cx);
        }

        let s = Theme::scaled;
        let block = crate::capabilities::transport_block(self.database_type);
        let col = div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .child(Self::section_label(&crate::localization::text(
                "connections.editor.tunnel_proxy_section",
            )));

        if let Some(block) = block {
            return col
                .child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(ssh_tunnel_block_message(block)),
                )
                .into_any_element();
        }

        let tunnel_profile_select = Select::new(&self.tunnel_profile_select);
        let inline = self.selected_tunnel_profile.is_none();

        let ssh_host = Input::new(&self.ssh_host_input).font_family(Theme::editor_font_family());
        let ssh_port = Input::new(&self.ssh_port_input).font_family(Theme::editor_font_family());
        let ssh_user = Input::new(&self.ssh_user_input).font_family(Theme::editor_font_family());
        let ssh_key = Input::new(&self.ssh_key_path_input).font_family(Theme::editor_font_family());
        let ssh_secret = Input::new(&self.ssh_secret_input)
            .font_family(Theme::editor_font_family())
            .mask_toggle();
        let ssh_auth = Select::new(&self.ssh_auth_select);
        let jump_host =
            Input::new(&self.ssh_jump_host_input).font_family(Theme::editor_font_family());
        let jump_port =
            Input::new(&self.ssh_jump_port_input).font_family(Theme::editor_font_family());
        let jump_user =
            Input::new(&self.ssh_jump_user_input).font_family(Theme::editor_font_family());
        let ssh_keepalive =
            Input::new(&self.ssh_keepalive_input).font_family(Theme::editor_font_family());
        let proxy_host =
            Input::new(&self.proxy_host_input).font_family(Theme::editor_font_family());
        let proxy_port =
            Input::new(&self.proxy_port_input).font_family(Theme::editor_font_family());
        let proxy_user =
            Input::new(&self.proxy_user_input).font_family(Theme::editor_font_family());
        let proxy_secret = Input::new(&self.proxy_secret_input)
            .font_family(Theme::editor_font_family())
            .mask_toggle();
        let proxy_kind = Select::new(&self.proxy_kind_select);

        col.child(Self::labeled_field(
            &crate::localization::text("connections.editor.tunnel_profile_label"),
            tunnel_profile_select,
            cx,
        ))
        .child(
            Button::new("ce-manage-tunnel-profiles")
                .label(crate::localization::text(
                    "connections.editor.manage_tunnels_button",
                ))
                .on_click(cx.listener(|_, _, _, cx| {
                    cx.emit(ConnectionEditorEvent::ManageTunnelProfiles);
                })),
        )
        .when(!inline, |d| {
            d.child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(Self::tunnel_profile_summary(
                        self.selected_tunnel_profile.expect("checked above"),
                    )),
            )
        })
        .when(inline, |d| {
            d.when(
                crate::capabilities::proxy_transport(self.database_type).is_available(),
                |d| {
                    d.child(Self::render_checkbox(
                        "ce-cb-proxy",
                        &crate::localization::text("connections.editor.use_proxy_label"),
                        self.proxy_enabled,
                        false,
                        cx,
                        Self::toggle_proxy,
                    ))
                    .when(self.proxy_enabled, |d| {
                        d.child(Self::labeled_field(
                            &crate::localization::text("connections.editor.proxy_kind_label"),
                            proxy_kind,
                            cx,
                        ))
                        .child(Self::labeled_field(
                            &crate::localization::text("connections.editor.proxy_host_label"),
                            proxy_host,
                            cx,
                        ))
                        .child(Self::labeled_field(
                            &crate::localization::text("connections.editor.proxy_port_label"),
                            proxy_port,
                            cx,
                        ))
                        .child(Self::labeled_field(
                            &crate::localization::text("connections.editor.proxy_user_label"),
                            proxy_user,
                            cx,
                        ))
                        .child(Self::labeled_field(
                            &crate::localization::text("connections.editor.proxy_password_label"),
                            proxy_secret,
                            cx,
                        ))
                    })
                },
            )
            .child(Self::render_checkbox(
                "ce-cb-ssh",
                &crate::localization::text("connections.editor.use_ssh_tunnel_label"),
                self.ssh_enabled,
                false,
                cx,
                Self::toggle_ssh,
            ))
        })
        .when(inline && self.ssh_enabled, |d| {
            d.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(6.0)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(s(8.0)))
                            .child(div().flex_1().child(Self::labeled_field(
                                &crate::localization::text("connections.editor.ssh_host_label"),
                                ssh_host,
                                cx,
                            )))
                            .child(div().w(px(s(88.0))).child(Self::labeled_field(
                                &crate::localization::text("connections.editor.ssh_port_label"),
                                ssh_port,
                                cx,
                            ))),
                    )
                    .child(Self::labeled_field(
                        &crate::localization::text("connections.editor.ssh_username_label"),
                        ssh_user,
                        cx,
                    ))
                    .child(Self::labeled_field(
                        &crate::localization::text("connections.editor.ssh_authentication_label"),
                        ssh_auth,
                        cx,
                    ))
                    .when(self.ssh_auth == SshAuthMode::KeyFile, |d| {
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            let browse_entity = cx.entity();
                            d.child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(s(8.0)))
                                    .items_end()
                                    .child(div().flex_1().child(Self::labeled_field(
                                        &crate::localization::text(
                                            "connections.editor.private_key_file_label",
                                        ),
                                        ssh_key,
                                        cx,
                                    )))
                                    .child(
                                        Button::new("ce-ssh-browse-key")
                                            .label(crate::localization::text(
                                                "connections.editor.browse_private_key_button",
                                            ))
                                            .on_click(move |_, window, app| {
                                                browse_entity.update(app, |this, cx| {
                                                    this.handle_browse_ssh_key(window, cx);
                                                });
                                            }),
                                    ),
                            )
                        }
                        #[cfg(target_arch = "wasm32")]
                        {
                            d.child(Self::labeled_field(
                                &crate::localization::text(
                                    "connections.editor.private_key_file_label",
                                ),
                                ssh_key,
                                cx,
                            ))
                        }
                    })
                    .when(
                        matches!(self.ssh_auth, SshAuthMode::KeyFile | SshAuthMode::Password),
                        |d| {
                            let label = if self.ssh_auth == SshAuthMode::KeyFile {
                                crate::localization::text("connections.editor.ssh_passphrase_label")
                            } else {
                                crate::localization::text("connections.editor.ssh_password_label")
                            };
                            d.child(Self::labeled_field(&label, ssh_secret, cx))
                        },
                    )
                    .child(div().w(px(s(160.0))).child(Self::labeled_field(
                        &crate::localization::text("connections.editor.keepalive_label"),
                        ssh_keepalive,
                        cx,
                    )))
                    .child(Self::render_checkbox(
                        "ce-cb-ssh-jump",
                        &crate::localization::text("connections.editor.use_jump_host_label"),
                        self.ssh_jump_enabled,
                        false,
                        cx,
                        Self::toggle_ssh_jump,
                    ))
                    .when(self.ssh_jump_enabled, |d| {
                        d.child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "connections.editor.jump_host_shared_auth_note",
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(s(8.0)))
                                .child(div().flex_1().child(Self::labeled_field(
                                    &crate::localization::text(
                                        "connections.editor.jump_host_label",
                                    ),
                                    jump_host,
                                    cx,
                                )))
                                .child(div().w(px(s(88.0))).child(Self::labeled_field(
                                    &crate::localization::text(
                                        "connections.editor.jump_port_label",
                                    ),
                                    jump_port,
                                    cx,
                                ))),
                        )
                        .child(Self::labeled_field(
                            &crate::localization::text("connections.editor.jump_username_label"),
                            jump_user,
                            cx,
                        ))
                    }),
            )
        })
        .into_any_element()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn render_kube_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.kube_context_select_stale {
            self.sync_kube_context_select(window, cx);
        }
        if self.kube_namespace_select_stale {
            self.sync_kube_namespace_select(window, cx);
        }
        if self.kube_kind_select_stale {
            self.sync_kube_kind_select(window, cx);
        }
        if self.kube_target_select_stale {
            self.sync_kube_target_select(window, cx);
        }

        let s = Theme::scaled;
        let block = crate::capabilities::transport_block(self.database_type);
        let col = div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .child(Self::section_label(&crate::localization::text(
                "connections.editor.kube_section",
            )));

        if let Some(block) = block {
            return col
                .child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(kube_block_message(block)),
                )
                .into_any_element();
        }

        let inline = self.selected_tunnel_profile.is_none();
        let kubeconfig =
            Input::new(&self.kube_kubeconfig_input).font_family(Theme::editor_font_family());
        let context = Select::new(&self.kube_context_select);
        let namespace = Select::new(&self.kube_namespace_select);
        let kind = Select::new(&self.kube_kind_select);
        let target = Select::new(&self.kube_target_select);
        let remote_port =
            Input::new(&self.kube_remote_port_input).font_family(Theme::editor_font_family());
        let entity = cx.entity();

        col.when(inline, |d| {
            d.child(Self::render_checkbox(
                "ce-cb-kube",
                &crate::localization::text("connections.editor.use_kube_port_forward_label"),
                self.kube_enabled,
                false,
                cx,
                Self::toggle_kube,
            ))
        })
        .when(inline && self.kube_enabled, |d| {
            d.child(Self::labeled_field(
                &crate::localization::text("connections.editor.kubeconfig_label"),
                kubeconfig,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(s(8.0)))
                    .child(div().flex_1().child(Self::labeled_field(
                        &crate::localization::text("connections.editor.kube_context_label"),
                        context,
                        cx,
                    )))
                    .child(
                        Button::new("ce-kube-refresh-contexts")
                            .label(crate::localization::text(
                                "connections.editor.kube_refresh_contexts",
                            ))
                            .ghost()
                            .on_click({
                                let entity = entity.clone();
                                move |_, _window, app| {
                                    entity.update(app, |this, cx| {
                                        this.refresh_kube_discovery(
                                            crate::kube_discovery::DiscoveryKind::Contexts,
                                            cx,
                                        );
                                    });
                                }
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(s(8.0)))
                    .child(div().flex_1().child(Self::labeled_field(
                        &crate::localization::text("connections.editor.kube_namespace_label"),
                        namespace,
                        cx,
                    )))
                    .child(
                        Button::new("ce-kube-refresh-namespaces")
                            .label(crate::localization::text(
                                "connections.editor.kube_refresh_namespaces",
                            ))
                            .ghost()
                            .on_click({
                                let entity = entity.clone();
                                move |_, _window, app| {
                                    entity.update(app, |this, cx| {
                                        this.refresh_kube_discovery(
                                            crate::kube_discovery::DiscoveryKind::Namespaces,
                                            cx,
                                        );
                                    });
                                }
                            }),
                    ),
            )
            .child(Self::labeled_field(
                &crate::localization::text("connections.editor.kube_target_kind_label"),
                kind,
                cx,
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(s(8.0)))
                    .child(div().flex_1().child(Self::labeled_field(
                        &crate::localization::text("connections.editor.kube_target_label"),
                        target,
                        cx,
                    )))
                    .child(
                        Button::new("ce-kube-refresh-targets")
                            .label(crate::localization::text(
                                "connections.editor.kube_refresh_targets",
                            ))
                            .ghost()
                            .on_click({
                                let entity = entity.clone();
                                move |_, _window, app| {
                                    entity.update(app, |this, cx| {
                                        this.refresh_kube_discovery(
                                            crate::kube_discovery::DiscoveryKind::Targets(
                                                this.kube_target_kind,
                                            ),
                                            cx,
                                        );
                                    });
                                }
                            }),
                    ),
            )
            .child(Self::labeled_field(
                &crate::localization::text("connections.editor.kube_remote_port_label"),
                remote_port,
                cx,
            ))
        })
        .when(self.kube_enabled, |d| {
            let mut status = String::new();
            if let Some(busy) = self.kube_busy {
                let resource_kind = crate::localization::text(match busy {
                    "contexts" => "connections.editor.kube_refresh_contexts",
                    "namespaces" => "connections.editor.kube_refresh_namespaces",
                    _ => "connections.editor.kube_refresh_targets",
                });
                status = crate::localization::format(
                    "connections.editor.kube_refreshing",
                    &[("resource_kind", resource_kind.as_str())],
                );
            } else if let Some(err) = &self.kube_error {
                status = err.clone();
            } else if matches!(
                sqlly_engines::ssh::locate_kubectl(),
                sqlly_engines::ssh::KubectlLocation::NotFound
            ) {
                status = crate::localization::text("connections.kubernetes.kubectl_not_found");
            }
            if status.is_empty() {
                d
            } else {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(status),
                )
            }
        })
        .into_any_element()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn render_ssm_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let col = div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(6.0)))
            .child(Self::section_label(&crate::localization::text(
                "connections.editor.ssm_section",
            )));

        if let Some(block) = crate::capabilities::transport_block(self.database_type) {
            return col
                .child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(ssm_block_message(block)),
                )
                .into_any_element();
        }

        let inline = self.selected_tunnel_profile.is_none();
        let field =
            |state: &Entity<InputState>| Input::new(state).font_family(Theme::editor_font_family());
        let target = field(&self.ssm_target_input);
        let region = field(&self.ssm_region_input);
        let profile = field(&self.ssm_profile_input);
        let remote_host = field(&self.ssm_remote_host_input);

        col.when(inline, |d| {
            d.child(Self::render_checkbox(
                "ce-cb-ssm",
                &crate::localization::text("connections.editor.use_ssm_label"),
                self.ssm_enabled,
                false,
                cx,
                Self::toggle_ssm,
            ))
        })
        .when(inline && self.ssm_enabled, |d| {
            d.child(Self::labeled_field(
                &crate::localization::text("connections.editor.ssm_target_label"),
                target,
                cx,
            ))
            .child(Self::labeled_field(
                &crate::localization::text("connections.editor.ssm_region_label"),
                region,
                cx,
            ))
            .child(Self::labeled_field(
                &crate::localization::text("connections.editor.ssm_profile_label"),
                profile,
                cx,
            ))
            .child(Self::labeled_field(
                &crate::localization::text("connections.editor.ssm_remote_host_label"),
                remote_host,
                cx,
            ))
        })
        .when(self.ssm_enabled, |d| {
            let status = ssm_tool_status(
                sqlly_engines::ssh::locate_aws_tool(sqlly_engines::ssh::AwsTool::Cli).is_some(),
                sqlly_engines::ssh::locate_aws_tool(
                    sqlly_engines::ssh::AwsTool::SessionManagerPlugin,
                )
                .is_some(),
            );
            match status {
                None => d,
                Some(missing) => d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(ssm_missing_tool_message(missing)),
                ),
            }
        })
        .into_any_element()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn refresh_kube_discovery(
        &mut self,
        kind: crate::kube_discovery::DiscoveryKind,
        cx: &mut Context<Self>,
    ) {
        self.kube_busy = Some(match kind {
            crate::kube_discovery::DiscoveryKind::Contexts => "contexts",
            crate::kube_discovery::DiscoveryKind::Namespaces => "namespaces",
            crate::kube_discovery::DiscoveryKind::Targets(_) => "targets",
        });
        self.kube_error = None;
        cx.notify();
        let kubeconfig = opt_empty(&self.kube_kubeconfig);
        let context = opt_empty(&self.kube_context);
        let namespace = opt_empty(&self.kube_namespace);
        let mut async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let discovery_kind = kind;
            let result = crate::tokio_handle()
                .spawn(async move {
                    crate::kube_discovery::run_discovery(
                        discovery_kind,
                        kubeconfig,
                        context,
                        namespace,
                    )
                    .await
                })
                .await;
            let _ = weak.update(&mut async_cx, |editor, cx| {
                editor.kube_busy = None;
                match result {
                    Ok(Ok(items)) => {
                        editor.kube_error = None;
                        match discovery_kind {
                            crate::kube_discovery::DiscoveryKind::Contexts => {
                                editor.kube_contexts = items;
                                editor.kube_context_select_stale = true;
                            }
                            crate::kube_discovery::DiscoveryKind::Namespaces => {
                                editor.kube_namespaces = items;
                                editor.kube_namespace_select_stale = true;
                            }
                            crate::kube_discovery::DiscoveryKind::Targets(_) => {
                                editor.kube_targets = items;
                                editor.kube_target_select_stale = true;
                            }
                        }
                    }
                    Ok(Err(message)) => editor.kube_error = Some(message),
                    Err(e) => {
                        let detail = e.to_string();
                        editor.kube_error = Some(crate::localization::format(
                            "connections.kubernetes.discovery_failed",
                            &[("error", detail.as_str())],
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn sync_kube_context_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.kube_context_select_stale = false;
        let mut items = self.kube_contexts.clone();
        if !self.kube_context.is_empty() && !items.iter().any(|i| i == &self.kube_context) {
            items.insert(0, self.kube_context.clone());
        }
        let selected = if self.kube_context.is_empty() {
            items.first().cloned().unwrap_or_default()
        } else {
            self.kube_context.clone()
        };
        self.kube_context_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            if !selected.is_empty() {
                state.set_selected_value(&selected, window, cx);
            }
        });
    }

    fn sync_kube_namespace_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.kube_namespace_select_stale = false;
        let mut items = self.kube_namespaces.clone();
        if !self.kube_namespace.is_empty() && !items.iter().any(|i| i == &self.kube_namespace) {
            items.insert(0, self.kube_namespace.clone());
        }
        let selected = if self.kube_namespace.is_empty() {
            items
                .first()
                .cloned()
                .unwrap_or_else(|| "default".to_string())
        } else {
            self.kube_namespace.clone()
        };
        self.kube_namespace_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            state.set_selected_value(&selected, window, cx);
        });
    }

    fn sync_kube_kind_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.kube_kind_select_stale = false;
        let label = crate::localization::text(match self.kube_target_kind {
            KubeTargetKind::Pod => "connections.editor.kube_pod",
            KubeTargetKind::Service => "connections.editor.kube_service",
            KubeTargetKind::Deployment => "connections.editor.kube_deployment",
        });
        self.kube_kind_select.update(cx, |state, cx| {
            state.set_items(
                [
                    "connections.editor.kube_pod",
                    "connections.editor.kube_service",
                    "connections.editor.kube_deployment",
                ]
                .map(crate::localization::text)
                .to_vec(),
                window,
                cx,
            );
            state.set_selected_value(&label, window, cx);
        });
    }

    fn sync_kube_target_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.kube_target_select_stale = false;
        let mut items = self.kube_targets.clone();
        if !self.kube_target_name.is_empty() && !items.iter().any(|i| i == &self.kube_target_name) {
            items.insert(0, self.kube_target_name.clone());
        }
        let selected = if self.kube_target_name.is_empty() {
            items.first().cloned().unwrap_or_default()
        } else {
            self.kube_target_name.clone()
        };
        self.kube_target_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            if !selected.is_empty() {
                state.set_selected_value(&selected, window, cx);
            }
        });
    }

    /// The relay-device dropdown (gpui-component [`Select`], matching the rest
    /// of the form's controls) plus the selected device's pairing management:
    /// an unpaired device offers "Authorize" (create the route pair), a paired
    /// one offers "Revoke" behind a confirm step — the pairing itself is a
    /// live control-plane action either way.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    fn render_relay_device_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let selected = self.selected_relay_device().cloned();
        let pending_revoke = self.pending_revoke_pair.clone();

        let pairing_row: Option<AnyElement> = match &selected {
            None => None,
            Some(device) => match &device.pair_id {
                // Unpaired: authorize inline. On the wasm build (no live
                // control plane) a static hint stands in.
                None => {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        let authorize_id = device.device_id.clone();
                        let this = cx.entity();
                        Some(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(8.0)))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(px(Theme::font_size_ratio(0.85)))
                                        .text_color(Theme::text_dim())
                                        .child(crate::localization::text(
                                            "connections.editor.relay_not_paired_hint",
                                        )),
                                )
                                .child(
                                    Button::new("ce-relay-authorize")
                                        .label(crate::localization::text(
                                            "connections.editor.relay_authorize_button",
                                        ))
                                        .on_click(move |_, _window, app| {
                                            let authorize_id = authorize_id.clone();
                                            this.update(app, |this, cx| {
                                                this.authorize_relay_device(authorize_id, cx);
                                            });
                                        }),
                                )
                                .into_any_element(),
                        )
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        Some(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "connections.editor.relay_pairing_desktop_required",
                                ))
                                .into_any_element(),
                        )
                    }
                }
                // Paired: revocation, behind a confirm step consistent with
                // the app's other destructive confirmations.
                Some(pair_id) => {
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        let element = if pending_revoke.as_deref() == Some(pair_id.as_str()) {
                            let pair_for_confirm = pair_id.clone();
                            let confirm_entity = cx.entity();
                            let cancel_entity = cx.entity();
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(s(6.0)))
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.85)))
                                        .text_color(Theme::text())
                                        .child(crate::localization::format(
                                            "connections.editor.relay_revoke_confirmation",
                                            &[("device_name", &device.name)],
                                        )),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .flex_row()
                                        .justify_end()
                                        .gap(px(s(8.0)))
                                        .child(
                                            Button::new("ce-relay-revoke-cancel")
                                                .label(crate::localization::text(
                                                    "connections.editor.relay_revoke_cancel",
                                                ))
                                                .on_click(move |_, _window, app| {
                                                    cancel_entity.update(app, |this, cx| {
                                                        this.pending_revoke_pair = None;
                                                        cx.notify();
                                                    });
                                                }),
                                        )
                                        .child(
                                            Button::new("ce-relay-revoke-confirm")
                                                .label(crate::localization::text(
                                                    "connections.editor.relay_revoke_confirm",
                                                ))
                                                .danger()
                                                .on_click(move |_, _window, app| {
                                                    let pair_id = pair_for_confirm.clone();
                                                    confirm_entity.update(app, |this, cx| {
                                                        this.revoke_relay_pair(pair_id, cx);
                                                    });
                                                }),
                                        ),
                                )
                                .into_any_element()
                        } else {
                            let pair_to_arm = pair_id.clone();
                            let this = cx.entity();
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(8.0)))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(px(Theme::font_size_ratio(0.85)))
                                        .text_color(Theme::text_dim())
                                        .child(crate::localization::text(
                                            "connections.editor.relay_authorized",
                                        )),
                                )
                                .child(
                                    Button::new("ce-relay-revoke")
                                        .label(crate::localization::text(
                                            "connections.editor.relay_revoke_button",
                                        ))
                                        .on_click(move |_, _window, app| {
                                            let pair_id = pair_to_arm.clone();
                                            this.update(app, |this, cx| {
                                                this.pending_revoke_pair = Some(pair_id);
                                                cx.notify();
                                            });
                                        }),
                                )
                                .into_any_element()
                        };
                        Some(element)
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        let _ = (pair_id, &pending_revoke);
                        Some(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "connections.editor.relay_authorized",
                                ))
                                .into_any_element(),
                        )
                    }
                }
            },
        };

        div()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .child(
                Select::new(&self.relay_device_select).placeholder(crate::localization::text(
                    "connections.editor.relay_device_placeholder",
                )),
            )
            .children(pairing_row)
            .into_any_element()
    }

    fn azure_server_options(&self) -> Vec<(String, String)> {
        let selected_account = self
            .entra_account
            .as_ref()
            .map(|account| account.account_id.as_str());
        let mut servers = BTreeMap::new();
        for discovery in &self.azure_discoveries {
            if selected_account.is_some_and(|id| id != discovery.account_id) {
                continue;
            }
            servers
                .entry(discovery.server_fqdn.clone())
                .or_insert_with(|| discovery.server_name.clone());
        }
        servers.into_iter().collect()
    }

    fn select_azure_server(&mut self, server: String, window: &mut Window, cx: &mut Context<Self>) {
        self.server = server.clone();
        self.host_validation = validate_host(&server);
        self.server_input
            .update(cx, |s, cx| s.set_value(server, window, cx));
        self.azure_server_dropdown_open = false;
        self.database.clear();
        self.database_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.database_list = None;
        cx.notify();
    }

    /// Human-readable validation message for the "Server" field, mirroring the
    /// Swift `ServerListTabController.validateHostUI()` → `hostValidationLabel`.
    /// Returns `None` when the host is valid or empty (no message shown).
    fn host_validation_message(&self) -> Option<String> {
        match self.host_validation {
            HostValidationResult::Valid | HostValidationResult::Empty => None,
            HostValidationResult::InvalidHost => Some(crate::localization::text(
                "connections.editor.host_validation_invalid_host",
            )),
            HostValidationResult::InvalidInstance => Some(crate::localization::text(
                "connections.editor.host_validation_invalid_instance",
            )),
        }
    }

    fn labeled_field(
        label: &str,
        field: impl IntoElement,
        _cx: &mut Context<ConnectionEditor>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(4.0)))
            .child(
                div()
                    .font(Theme::ui_font())
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(label.to_string()),
            )
            .child(field)
    }

    fn section_label(label: &str) -> AnyElement {
        div()
            .font(Theme::ui_font())
            .text_size(px(Theme::font_size()))
            .text_color(Theme::text_dim())
            .child(label.to_string())
            .into_any_element()
    }

    /// Environment chooser rendered as a segmented button row (mirrors the
    /// Connection Management form and the Swift environment popup). Clicking the
    /// already-active chip clears the selection (sets `Unset`); clicking a
    /// different chip selects it.
    fn render_production_override(current: Option<bool>, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let options: &[(String, Option<bool>)] = &[
            (
                crate::localization::text("connections.editor.production_inherit"),
                None,
            ),
            (
                crate::localization::text("connections.editor.production_enabled"),
                Some(true),
            ),
            (
                crate::localization::text("connections.editor.production_disabled"),
                Some(false),
            ),
        ];
        div()
            .flex()
            .flex_row()
            .gap(px(s(4.0)))
            .children(options.iter().enumerate().map(|(i, (label, value))| {
                let selected = *value == current;
                let val = *value;
                div()
                    .id(("ce-production", i))
                    .px(px(s(8.0)))
                    .h(px(Theme::control_height()))
                    .flex()
                    .items_center()
                    .cursor(gpui::CursorStyle::PointingHand)
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(3.0)))
                    .text_size(px(Theme::font_size()))
                    .bg(if selected {
                        Theme::row_hover()
                    } else {
                        Theme::bg()
                    })
                    .text_color(Theme::text())
                    .hover(|st| st.bg(Theme::row_hover()))
                    .child(label.to_string())
                    .on_mouse_down(MouseButton::Left, {
                        cx.listener(move |this, _, _, cx| {
                            this.production_override = val;
                            cx.notify();
                        })
                    })
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn render_environment_segmented(
        current: ConnectionEnvironment,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let options: &[(&str, ConnectionEnvironment)] = &[
            ("PROD", ConnectionEnvironment::Production),
            ("STAGE", ConnectionEnvironment::Staging),
            ("TEST", ConnectionEnvironment::Test),
            ("DEV", ConnectionEnvironment::Dev),
            ("LOCAL", ConnectionEnvironment::Local),
        ];
        div()
            .flex()
            .flex_row()
            .gap(px(s(4.0)))
            .children(options.iter().enumerate().map(|(i, (label, env))| {
                let selected = *env == current;
                let val = *env;
                div()
                    .id(("ce-env", i))
                    .px(px(s(8.0)))
                    .h(px(s(24.0)))
                    .flex()
                    .items_center()
                    .cursor(gpui::CursorStyle::PointingHand)
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(3.0)))
                    .text_size(px(Theme::font_size()))
                    .bg(if selected {
                        Theme::accent()
                    } else {
                        Theme::bg()
                    })
                    .text_color(if selected {
                        Theme::bg()
                    } else {
                        Theme::text_dim()
                    })
                    .hover(|st| st.bg(Theme::border()))
                    .child(label.to_string())
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            if val == this.environment {
                                this.set_environment(ConnectionEnvironment::Unset, cx);
                            } else {
                                this.set_environment(val, cx);
                            }
                        }),
                    )
            }))
            .into_any_element()
    }

    fn render_auth_segmented(
        current: AuthMode,
        engine: DatabaseType,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        if crate::engine_form::is_file_engine(engine) {
            return div()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::engine_form::file_engine_credentials_note(engine))
                .into_any_element();
        }
        div()
            .flex()
            .flex_row()
            .gap(px(s(4.0)))
            .children(AuthMode::ALL.iter().enumerate().filter_map(|(i, mode)| {
                if !mode.supported_for(engine) {
                    return None;
                }
                let selected = *mode == current;
                let mode = *mode;
                Some(
                    div()
                        .id(("ce-auth", i))
                        .px(px(s(8.0)))
                        .h(px(s(24.0)))
                        .flex()
                        .items_center()
                        .cursor(gpui::CursorStyle::PointingHand)
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(s(3.0)))
                        .text_size(px(Theme::font_size()))
                        .bg(if selected {
                            Theme::accent()
                        } else {
                            Theme::bg()
                        })
                        .text_color(if selected {
                            Theme::bg()
                        } else {
                            Theme::text_dim()
                        })
                        .hover(|st| st.bg(Theme::border()))
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(3.0)))
                                .when(mode == AuthMode::EntraBrowser, |d| {
                                    d.children(
                                        crate::icons::entra_id_icon()
                                            .map(|icon| img(icon).w(px(s(14.0))).h(px(s(14.0)))),
                                    )
                                })
                                .child(mode.label().to_string()),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                this.set_auth_mode(mode, window, cx)
                            }),
                        ),
                )
            }))
            .into_any_element()
    }

    fn render_sql_account_picker(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let accounts = ConnectionStore::global().sql_auth_accounts();
        let selected = self.sql_account;
        if accounts.is_empty() {
            return div()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "connections.editor.sql_accounts_empty",
                ))
                .into_any_element();
        }
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(s(6.0)))
            .children(accounts.into_iter().enumerate().map(|(index, account)| {
                let id = account.id;
                div()
                    .id(("ce-sql-account-choice", index))
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .rounded(px(s(4.0)))
                    .border_1()
                    .border_color(if selected == Some(id) {
                        Theme::accent()
                    } else {
                        Theme::border()
                    })
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .cursor(gpui::CursorStyle::PointingHand)
                    .hover(|style| style.bg(Theme::border()))
                    .child(account.name)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.apply_sql_account(id, window, cx)
                        }),
                    )
            }))
            .into_any_element()
    }

    fn render_checkbox(
        id: &'static str,
        label: &str,
        checked: bool,
        disabled: bool,
        cx: &mut Context<Self>,
        handler: fn(&mut Self, bool, &mut Window, &mut Context<Self>),
    ) -> AnyElement {
        let this = cx.entity();
        let mut checkbox = Checkbox::new(id)
            .checked(checked)
            .label(label.to_string())
            .tab_stop(true);
        if disabled {
            checkbox = checkbox.disabled(true);
        } else {
            checkbox = checkbox.on_click(move |new_checked: &bool, window, app| {
                let checked = *new_checked;
                this.update(app, |this, cx| handler(this, checked, window, cx));
            });
        }
        checkbox.into_any_element()
    }

    fn render_database_dropdown(
        list: Option<Vec<String>>,
        loading: bool,
        error: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let body: AnyElement = if loading {
            div()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "connections.editor.databases_loading",
                ))
                .into_any_element()
        } else if let Some(e) = error {
            div()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::error())
                .child(e)
                .into_any_element()
        } else if let Some(dbs) = list {
            if dbs.is_empty() {
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "connections.editor.databases_empty",
                    ))
                    .into_any_element()
            } else {
                div()
                    .id("ce-db-dropdown-scroll")
                    .flex()
                    .flex_col()
                    .max_h(px(s(160.0)))
                    .overflow_y_scroll()
                    .children(dbs.into_iter().enumerate().map(|(i, name)| {
                        let n = name.clone();
                        div()
                            .id(("ce-db-item", i))
                            .px(px(s(8.0)))
                            .h(px(s(24.0)))
                            .flex()
                            .items_center()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .hover(|st| st.bg(Theme::row_hover()))
                            .child(name)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.select_database(n.clone(), window, cx)
                                }),
                            )
                    }))
                    .into_any_element()
            }
        } else {
            div()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "connections.editor.databases_loading",
                ))
                .into_any_element()
        };
        // `deferred(anchored(...))` lifts the popup out of the scrolling form
        // (`#ce_form_scroll` would otherwise clip it at the viewport edge) and
        // paints it above later siblings; `.occlude()` blocks click-through
        // and `.on_mouse_down_out()` dismisses on an outside click.
        let dismiss = cx.entity();
        div()
            .absolute()
            .top(px(s(30.0) + s(6.0)))
            .left(px(0.0))
            .child(
                deferred(
                    anchored().snap_to_window_with_margin(px(s(8.0))).child(
                        div()
                            .occlude()
                            .w(px(s(360.0)))
                            .bg(Theme::popup_bg())
                            .border_1()
                            .border_color(Theme::popup_border())
                            .rounded(px(s(4.0)))
                            .p(px(s(4.0)))
                            .child(body)
                            .on_mouse_down_out(move |_: &MouseDownEvent, _window, app| {
                                dismiss.update(app, |this, cx| {
                                    if this.database_dropdown_open {
                                        this.database_dropdown_open = false;
                                        cx.notify();
                                    }
                                });
                            }),
                    ),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// A client/project field: the text input, plus the completion list when
    /// that field is the one being edited and the store knows names matching
    /// what has been typed.
    ///
    /// Free text is the point. The list is a shortcut to a name that already
    /// exists, never a fence around the ones that do — a client the user has
    /// not created yet is typed straight into the field and registered on save.
    fn render_entity_field(
        &mut self,
        field: EntityField,
        input: Input,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let open = self.entity_suggestions_open == Some(field);
        let suggestions = if open {
            self.entity_suggestions(field)
        } else {
            Vec::new()
        };
        let (id_prefix, scroll_id) = match field {
            EntityField::Client => ("ce-client", "ce-client-scroll"),
            EntityField::Project => ("ce-project", "ce-project-scroll"),
        };
        let dismiss = cx.entity();

        div()
            .relative()
            .child(Self::labeled_field(&field.label(), input, cx))
            .when(!suggestions.is_empty(), |d| {
                d.child(
                    div()
                        .absolute()
                        // Clear the label above the input as well as the input
                        // itself; the field is a two-row stack. The input's own
                        // height and the 6px gap under it are the database
                        // dropdown's numbers, so the two popups hang alike.
                        .top(px(Theme::line_height() + s(4.0) + s(30.0) + s(6.0)))
                        .left(px(0.0))
                        .child(
                            deferred(
                                anchored().snap_to_window_with_margin(px(s(8.0))).child(
                                    div()
                                        .occlude()
                                        .w(px(s(220.0)))
                                        .bg(Theme::popup_bg())
                                        .border_1()
                                        .border_color(Theme::popup_border())
                                        .rounded(px(s(4.0)))
                                        .p(px(s(4.0)))
                                        .child(
                                            div()
                                                .id(scroll_id)
                                                .flex()
                                                .flex_col()
                                                .max_h(px(s(160.0)))
                                                .overflow_y_scroll()
                                                .children(suggestions.into_iter().enumerate().map(
                                                    |(i, name)| {
                                                        let picked = name.clone();
                                                        div()
                                                            .id((id_prefix, i))
                                                            .px(px(s(8.0)))
                                                            .h(px(s(24.0)))
                                                            .flex()
                                                            .items_center()
                                                            .text_size(px(Theme::font_size()))
                                                            .text_color(Theme::text())
                                                            .hover(|st| st.bg(Theme::row_hover()))
                                                            .child(name)
                                                            .on_mouse_down(
                                                                MouseButton::Left,
                                                                cx.listener(
                                                                    move |this, _, window, cx| {
                                                                        this.select_entity_suggestion(
                                                                            field,
                                                                            picked.clone(),
                                                                            window,
                                                                            cx,
                                                                        );
                                                                    },
                                                                ),
                                                            )
                                                    },
                                                )),
                                        )
                                        .on_mouse_down_out(
                                            move |_: &MouseDownEvent, _window, app| {
                                                dismiss.update(app, |this, cx| {
                                                    if this.entity_suggestions_open == Some(field) {
                                                        this.entity_suggestions_open = None;
                                                        cx.notify();
                                                    }
                                                });
                                            },
                                        ),
                                ),
                            )
                            .with_priority(1),
                        ),
                )
            })
            .into_any_element()
    }

    fn render_azure_server_control(&mut self, server: Input, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let open = self.azure_server_dropdown_open;
        let loading = self.azure_servers_loading;
        let error = self.azure_servers_error.clone();
        let has_error = error.is_some();
        let help_copied = self.azure_server_help_copied;
        let options = self.azure_server_options();
        let body = if loading {
            div()
                .p(px(s(8.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "connections.editor.azure_servers_loading",
                ))
                .into_any_element()
        } else if let Some(error) = error {
            div()
                .p(px(s(8.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::error())
                .child(error)
                .into_any_element()
        } else if options.is_empty() {
            div()
                .p(px(s(8.0)))
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "connections.editor.azure_servers_empty",
                ))
                .into_any_element()
        } else {
            div()
                .id("ce-azure-server-list")
                .flex()
                .flex_col()
                .max_h(px(s(190.0)))
                .overflow_y_scroll()
                .children(
                    options
                        .into_iter()
                        .enumerate()
                        .map(|(index, (fqdn, name))| {
                            let selected = fqdn.clone();
                            div()
                                .id(("ce-azure-server", index))
                                .px(px(s(8.0)))
                                .py(px(s(5.0)))
                                .flex()
                                .flex_col()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text())
                                .hover(|style| style.bg(Theme::row_hover()))
                                .child(name)
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.85)))
                                        .text_color(Theme::text_dim())
                                        .child(fqdn),
                                )
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |this, _, window, cx| {
                                        this.select_azure_server(selected.clone(), window, cx)
                                    }),
                                )
                        }),
                )
                .into_any_element()
        };
        let dropdown = if open {
            let dismiss = cx.entity();
            Some(
                div().absolute().top(px(s(36.0))).left_0().child(
                    deferred(
                        anchored().snap_to_window_with_margin(px(s(8.0))).child(
                            div()
                                .occlude()
                                .w(px(s(360.0)))
                                .bg(Theme::popup_bg())
                                .border_1()
                                .border_color(Theme::popup_border())
                                .rounded(px(s(4.0)))
                                .child(body)
                                .on_mouse_down_out(move |_: &MouseDownEvent, _window, app| {
                                    dismiss.update(app, |this, cx| {
                                        this.azure_server_dropdown_open = false;
                                        cx.notify();
                                    });
                                }),
                        ),
                    )
                    .with_priority(2),
                ),
            )
        } else {
            None
        };

        let field_row = div()
            .relative()
            .flex()
            .flex_row()
            .gap(px(s(6.0)))
            .child(div().flex_1().child(server))
            .child(
                div()
                    .id("ce-azure-server-caret")
                    .h(px(s(30.0)))
                    .px(px(s(8.0)))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor(gpui::CursorStyle::PointingHand)
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .text_color(Theme::text_dim())
                    .hover(|style| style.bg(Theme::border()))
                    .child("\u{25be}")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.azure_server_dropdown_open = !open;
                            cx.notify();
                        }),
                    ),
            )
            .children(dropdown);

        // On a discovery failure (typically a 403 for accounts that can't list
        // servers), keep the field usable and reassure the user they can type
        // the server name — with an info affordance that copies an admin-ready
        // request for the Reader role.
        let note = has_error.then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(s(3.0)))
                .mt(px(s(4.0)))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(5.0)))
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "connections.editor.azure_servers_manual_hint",
                        ))
                        .child(
                            Button::new("ce-azure-perm-info")
                                .ghost()
                                .xsmall()
                                .icon(
                                    gpui_component::Icon::empty()
                                        .path(crate::icon_catalog::AppIcon::Info.path()),
                                )
                                .app_tooltip_text(crate::localization::text(
                                    "connections.editor.azure_permission_copy_tooltip",
                                ))
                                .on_click(cx.listener(|this, _ev, _window, cx| {
                                    this.copy_azure_permission_request(cx);
                                })),
                        ),
                )
                .children(help_copied.then(|| {
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "connections.editor.azure_permission_copied",
                        ))
                }))
        });

        div()
            .flex()
            .flex_col()
            .child(field_row)
            .children(note)
            .into_any_element()
    }
}

/// How many characters the password field shows for a saved account. Fixed on
/// purpose: the keychain password never reaches this form, and a stand-in whose
/// length tracked the real one would leak it.
const PASSWORD_MASK_LEN: usize = 20;

/// The placeholder text a saved account's password field carries. Bullets
/// rather than filler letters, so revealing the field with the mask toggle
/// shows the same thing it showed masked — a stand-in, not a fake password.
///
/// It never reaches storage: `assemble_profile` writes the account id as the
/// secret reference whenever one is selected, and clearing the account clears
/// this text with it.
/// The notice a duplicate shows when one or more of the original's secrets
/// could not be read from the keychain; `None` when every secret came over.
fn duplicate_notice(missing: &[String]) -> Option<String> {
    let (key, arguments) = match missing {
        [] => return None,
        [one] => (
            "connections.editor.duplicate_notice_one",
            vec![("first", one.clone())],
        ),
        [first, second] => (
            "connections.editor.duplicate_notice_two",
            vec![("first", first.clone()), ("second", second.clone())],
        ),
        [first, second, third, ..] => (
            "connections.editor.duplicate_notice_three",
            vec![
                ("first", first.clone()),
                ("second", second.clone()),
                ("third", third.clone()),
            ],
        ),
    };
    let arguments: Vec<(&str, &str)> = arguments
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    Some(crate::localization::format(key, &arguments))
}

pub fn password_mask() -> String {
    "\u{2022}".repeat(PASSWORD_MASK_LEN)
}

/// A server hostname that implies a local connection. Used by the editor to
/// auto-select the `Local` environment chip when the user types one of these
/// into the server field while no environment is chosen yet.
fn server_is_local(server: &str) -> bool {
    let s = server.trim().to_lowercase();
    matches!(s.as_str(), "local" | "localhost" | "127.0.0.1")
}

impl ConnectionEditor {
    /// Every user-editable field, flattened for change detection. Lists,
    /// probe results, and other machinery the user does not edit are
    /// deliberately excluded so async loads can't fake an edit.
    fn editable_fingerprint(&self, cx: &gpui::App) -> String {
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|\
             {:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|\
             {:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|\
             {:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            self.name,
            self.server,
            self.port,
            self.database_type,
            self.database,
            self.database_all,
            self.username,
            self.password_input.read(cx).value(),
            self.client,
            self.project,
            self.timeout,
            (&self.startup_sql, &self.notes, &self.display_time_zone),
            (
                &self.redis_key_delimiter,
                self.redis_cluster_mode,
                &self.redis_node_map
            ),
            self.clickhouse_native_port,
            self.environment,
            self.auth_mode,
            self.entra_account,
            self.sql_account,
            self.encrypt,
            self.trust_certificate,
            self.tls_mode,
            self.tls_ca_file,
            self.tls_client_cert_file,
            self.tls_client_key_file,
            self.block_select,
            self.rollback,
            self.confirm_changes,
            self.allow_unbounded,
            self.production_override,
            self.via_relay,
            self.relay_device_id,
            self.relay_hint,
            self.selected_tunnel_profile,
            self.ssh_enabled,
            self.ssh_host,
            self.ssh_port,
            self.ssh_user,
            self.ssh_auth,
            self.ssh_key_path,
            self.ssh_secret_id,
            self.ssh_jump_enabled,
            self.ssh_jump_host,
            self.ssh_jump_port,
            self.ssh_jump_user,
            self.proxy_enabled,
            self.proxy_kind,
            self.proxy_host,
            self.proxy_port,
            self.proxy_user,
            self.proxy_secret_id,
            self.kube_enabled,
            self.kube_kubeconfig,
            self.kube_context,
            self.kube_namespace,
            self.kube_target_kind,
            self.kube_target_name,
            self.kube_remote_port,
            (
                self.ssm_enabled,
                &self.ssm_target,
                &self.ssm_region,
                &self.ssm_profile,
                &self.ssm_remote_host,
            ),
            self.aws_region,
            self.aws_profile,
            self.ssh_keepalive,
            (self.tls_trust_os_roots, self.tls_direct),
        )
    }
}

impl Render for ConnectionEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Dirty tracking: the first render (after any profile load populated
        // the form) captures the baseline; any divergence marks the dialog
        // dirty, which suspends easy close.
        let fingerprint = self.editable_fingerprint(cx);
        let baseline = self
            .baseline_fingerprint
            .get_or_insert_with(|| fingerprint.clone());
        let dirty = *baseline != fingerprint;
        self.frame.set_dirty(dirty);
        // Lazily fetch the account's relay devices on first render so the
        // routing control can gate itself on real data.
        self.ensure_relay_devices_loaded(cx);
        #[cfg(not(target_arch = "wasm32"))]
        if self.aws_profile_select_stale {
            self.sync_aws_profile_select(window, cx);
        }
        // Device lists change in async continuations, which have no `Window`;
        // the select's items are synced here where one exists.
        if self.relay_select_stale {
            self.sync_relay_select(window, cx);
        }
        if self.tls_mode_select_stale {
            self.sync_tls_mode_select(window, cx);
        }

        // A relay proxy's profile opens for inspection only: the banner says
        // so and Save is disabled (Test still works — it runs through the
        // proxy like any query).
        let proxied = self.read_only_proxied();
        let title = if proxied {
            crate::localization::text("connections.editor.title_view")
        } else if self.editing_id.is_some() {
            crate::localization::text("connections.editor.title_edit")
        } else if self.duplicate_of.is_some() {
            crate::localization::text("connections.editor.title_duplicate")
        } else {
            crate::localization::text("connections.editor.title_new")
        };

        let environment = self.environment;
        let auth_mode = self.auth_mode;
        let file_engine = crate::engine_form::is_file_engine(self.database_type);
        let database_type = self.database_type;
        // Wire-compatible labels carry an honesty note; first-party engines
        // with outside requirements (Oracle's Instant Client) a setup note.
        let compat_note = connection_engine_note(database_type);
        let iam_tls = matches!(auth_mode, AuthMode::AwsRdsIam | AuthMode::GcpCloudSqlIam);
        let tls_required = database_type.requires_tls() || iam_tls;
        // PostgreSQL/MySQL engines swap the two legacy checkboxes for the
        // TLS mode select plus certificate path rows.
        let tls_engine = crate::engine_form::uses_tls_modes(database_type);
        let tls_section = tls_engine.then(|| self.render_tls_section(cx));
        let encrypt = self.encrypt || tls_required;
        let trust = self.trust_certificate;
        let block_select = self.block_select;
        let rollback = self.rollback;
        let confirm_changes = self.confirm_changes;
        let allow_unbounded = self.allow_unbounded;
        let production_override = self.production_override;
        let testing = self.testing;
        let test_result = self.test_result.clone();
        let test_success = self.test_success;
        let server_info = self.server_info.clone();
        let error = self.error.clone();
        let notice = if proxied {
            Some(crate::localization::text(
                "connections.editor.proxy_read_only",
            ))
        } else {
            self.notice.clone()
        };

        let database_all = self.database_all;
        let database_dropdown_open = self.database_dropdown_open;
        let database_list = self.database_list.clone();
        let database_loading = self.database_loading;
        let database_list_error = self.database_list_error.clone();

        let name = Input::new(&self.name_input);
        let server = Input::new(&self.server_input).font_family(Theme::editor_font_family());
        let port = Input::new(&self.port_input)
            .font_family(Theme::editor_font_family())
            .disabled(file_engine);
        let engine = Select::new(&self.engine_select);
        let database = Input::new(&self.database_input)
            .font_family(Theme::editor_font_family())
            .disabled(database_all);
        // A saved account owns both credentials: the username is the account's
        // and the password lives in the keychain, so neither field is the
        // user's to edit here. Clicking the chosen account again releases them.
        let account_locked = self.sql_account.is_some() && auth_mode == AuthMode::SqlLogin;
        let sql_username = Input::new(&self.username_input)
            .font_family(Theme::editor_font_family())
            .disabled(account_locked);
        let aws_region =
            Input::new(&self.aws_region_input).font_family(Theme::editor_font_family());
        #[cfg(not(target_arch = "wasm32"))]
        let aws_profile = Select::new(&self.aws_profile_select);
        let password = Input::new(&self.password_input)
            .font_family(Theme::editor_font_family())
            .mask_toggle()
            .disabled(account_locked);
        let client = Input::new(&self.client_input);
        let project = Input::new(&self.project_input);
        let timeout = Input::new(&self.timeout_input).font_family(Theme::editor_font_family());
        let startup_sql =
            Textarea::new(&self.startup_sql_input).font_family(Theme::editor_font_family());
        let notes = Textarea::new(&self.notes_input);
        let redis_delimiter =
            Input::new(&self.redis_delimiter_input).font_family(Theme::editor_font_family());
        let key_delimiter_engine = crate::engine_form::uses_key_delimiter(database_type);
        // Redis Sentinel (native only: the browser build reaches Redis
        // through a relay, which resolves the master on its side).
        #[cfg(not(target_arch = "wasm32"))]
        let sentinel_section =
            sentinel::applies_to(database_type).then(|| self.render_sentinel_section(cx));
        #[cfg(target_arch = "wasm32")]
        let sentinel_section: Option<AnyElement> = None;
        let redis_cluster = Select::new(&self.redis_cluster_select);
        let display_zone = Select::new(&self.display_zone_select);
        let redis_node_map =
            Textarea::new(&self.redis_node_map_input).font_family(Theme::editor_font_family());
        let clickhouse_native_port =
            Input::new(&self.clickhouse_native_port_input).font_family(Theme::editor_font_family());

        let s = Theme::scaled;
        // The Azure server picker lists Azure SQL servers; a Redis profile
        // signing in with Entra types its cache host instead.
        let redis_entra = auth_mode == AuthMode::EntraBrowser && database_type.is_command_engine();
        let server_control = if auth_mode == AuthMode::EntraBrowser && !redis_entra {
            self.render_azure_server_control(server, cx)
        } else {
            server.into_any_element()
        };
        let client_field = self.render_entity_field(EntityField::Client, client, cx);
        let project_field = self.render_entity_field(EntityField::Project, project, cx);
        #[cfg(not(target_arch = "wasm32"))]
        let routing_section = Some(self.render_routing_section(cx));
        #[cfg(not(target_arch = "wasm32"))]
        let ssh_section = Some(self.render_ssh_section(window, cx));
        #[cfg(not(target_arch = "wasm32"))]
        let kube_section = Some(self.render_kube_section(window, cx));
        #[cfg(not(target_arch = "wasm32"))]
        let ssm_section = Some(self.render_ssm_section(cx));
        #[cfg(target_arch = "wasm32")]
        let routing_section: Option<gpui::AnyElement> = None;
        #[cfg(target_arch = "wasm32")]
        let ssh_section: Option<gpui::AnyElement> = None;
        #[cfg(target_arch = "wasm32")]
        let kube_section: Option<gpui::AnyElement> = None;
        #[cfg(target_arch = "wasm32")]
        let ssm_section: Option<gpui::AnyElement> = None;

        let viewport_h = f32::from(self.scroll.bounds().size.height);
        let max_scroll = f32::from(self.scroll.max_offset().y);
        let offset_y = f32::from(self.scroll.offset().y);
        let (scrollable, thumb_h, thumb_y) = if max_scroll > 0.0 && viewport_h > 0.0 {
            let content_total = max_scroll + viewport_h;
            let thumb_h = ((viewport_h * viewport_h) / content_total).max(s(20.0));
            let pct = (-offset_y / max_scroll).clamp(0.0, 1.0);
            let thumb_y = pct * (viewport_h - thumb_h);
            (true, thumb_h, thumb_y)
        } else {
            (false, 0.0, 0.0)
        };

        let redis_entra_hint = redis_entra.then(|| redis_entra_host_hint(&self.server));
        let auth_hint = match auth_mode {
            AuthMode::Integrated => {
                crate::localization::text("connections.editor.integrated_auth_hint")
            }
            AuthMode::EntraBrowser if redis_entra => redis_entra_hint.unwrap_or_default(),
            AuthMode::EntraBrowser => {
                crate::localization::text("connections.editor.entra_auth_hint")
            }
            AuthMode::SqlLogin | AuthMode::AwsRdsIam | AuthMode::GcpCloudSqlIam => String::new(),
            AuthMode::AuthToken => crate::localization::text("connections.editor.auth_token_hint"),
        };
        #[cfg(not(target_arch = "wasm32"))]
        let gcp_adc_status = self.gcp_adc_status.clone();

        // Form body wrapper (relative-positioned so the scrollbar thumb lives
        // outside the scroll container's content coordinate space — otherwise
        // the thumb scrolls with the content and appears detached from the
        // actual scroll position). Flex-constrained so the region shrinks with
        // the card when the user drag-resizes the dialog shorter.
        let form_body = div()
            .relative()
            .flex_1()
            .min_h(px(0.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .id("ce_form_scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.0))
                    .gap(px(s(12.0)))
                    .p(px(s(16.0)))
                    .max_h(px(s(520.0)))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    // 1. Name
                    .child(Self::labeled_field(
                        &crate::localization::text("connections.editor.name_label"),
                        name,
                        cx,
                    ))
                    .when_some(notice, |d, text| {
                        d.child(
                            div()
                                .px(px(s(4.0)))
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::warning())
                                .child(text),
                        )
                    })
                    .child(Self::labeled_field(
                        &crate::localization::text("connections.editor.database_engine_label"),
                        engine,
                        cx,
                    ))
                    .when(self.engine_auto_detect, |body| {
                        body.child(
                            div()
                                .px(px(s(4.0)))
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(crate::engine_form::auto_detect_hint(database_type)),
                        )
                    })
                    .child(
                        div().px(px(s(4.0))).child(
                            Button::new("ce-paste-url")
                                .ghost()
                                .small()
                                .label(crate::localization::text(
                                    "connections.editor.paste_url_button",
                                ))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.url_paste_open = !this.url_paste_open;
                                    cx.notify();
                                })),
                        ),
                    )
                    .when(self.url_paste_open, |body| {
                        body.child(Self::labeled_field(
                            &crate::localization::text(
                                "connections.editor.connection_string_label",
                            ),
                            Input::new(&self.url_paste_input).cleanable(true),
                            cx,
                        ))
                    })
                    // 2. Server + Port
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(s(4.0)))
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(s(8.0)))
                                    .child(div().flex_1().child(Self::labeled_field(
                                        &if file_engine {
                                            crate::localization::text(
                                                "connections.editor.database_file_label",
                                            )
                                        } else {
                                            crate::localization::text(
                                                "connections.editor.server_label",
                                            )
                                        },
                                        server_control,
                                        cx,
                                    )))
                                    .child(div().w(px(s(100.0))).child(Self::labeled_field(
                                        &crate::localization::text("connections.editor.port_label"),
                                        port,
                                        cx,
                                    ))),
                            )
                            .when_some(self.host_validation_message(), |d, msg| {
                                d.child(
                                    div()
                                        .text_size(px(Theme::font_size()))
                                        .text_color(Theme::error())
                                        .child(msg.to_string()),
                                )
                            }),
                    )
                    // 3. Database (with "all" checkbox + lazy dropdown)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(s(4.0)))
                            .child(
                                div()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text_dim())
                                    .child(crate::localization::text(
                                        "connections.editor.database_label",
                                    )),
                            )
                            .child(
                                div()
                                    .relative()
                                    .flex()
                                    .flex_row()
                                    .gap(px(s(6.0)))
                                    .items_center()
                                    .child(div().flex_1().child(database))
                                    .when(!database_all, |d| {
                                        d.child(
                                            div()
                                                .id("ce-db-caret")
                                                .h(px(s(30.0)))
                                                .px(px(s(8.0)))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .cursor(gpui::CursorStyle::PointingHand)
                                                .border_1()
                                                .border_color(Theme::border())
                                                .rounded(px(s(4.0)))
                                                .text_size(px(Theme::font_size()))
                                                .text_color(Theme::text_dim())
                                                .hover(|st| st.bg(Theme::border()))
                                                .child("\u{25be}")
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(move |this, _, _, cx| {
                                                        this.set_database_dropdown_open(
                                                            !database_dropdown_open,
                                                            cx,
                                                        );
                                                    }),
                                                ),
                                        )
                                    })
                                    .child(Self::render_checkbox(
                                        "ce-cb-dball",
                                        &crate::localization::text(
                                            "connections.editor.database_all_label",
                                        ),
                                        database_all,
                                        false,
                                        cx,
                                        Self::toggle_database_all,
                                    ))
                                    .when(database_dropdown_open && !database_all, |d| {
                                        d.child(Self::render_database_dropdown(
                                            database_list.clone(),
                                            database_loading,
                                            database_list_error.clone(),
                                            cx,
                                        ))
                                    }),
                            ),
                    )
                    // 4. Authentication options
                    .child(Self::section_label(&crate::localization::text(
                        "connections.editor.authentication_section",
                    )))
                    .child(Self::render_auth_segmented(
                        auth_mode,
                        self.database_type,
                        cx,
                    ))
                    // 5/6. Username / Password (SQL Login) or auth hint
                    .when(!file_engine && auth_mode == AuthMode::SqlLogin, |d| {
                        let d = d
                            .child(Self::render_sql_account_picker(self, cx))
                            .child(Self::labeled_field(
                                &crate::localization::text("connections.editor.username_label"),
                                sql_username,
                                cx,
                            ))
                            .child(Self::labeled_field(
                                &crate::localization::text("connections.editor.password_label"),
                                password,
                                cx,
                            ));
                        // Browser build only: credentials cannot reach an OS
                        // keychain from a tab, and users deserve to know
                        // exactly where their password goes.
                        #[cfg(target_arch = "wasm32")]
                        let d = d.child(
                            div()
                                .text_size(px(Theme::font_size() - 2.0))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "connections.editor.browser_credential_storage_hint",
                                )),
                        );
                        d
                    })
                    // libSQL / Turso: one masked token field (the same
                    // secret input a password uses) plus where to get one.
                    .when(!file_engine && auth_mode == AuthMode::AuthToken, |d| {
                        let token = Input::new(&self.password_input)
                            .font_family(Theme::editor_font_family())
                            .mask_toggle();
                        d.child(Self::labeled_field(
                            &crate::localization::text("connections.editor.auth_token_label"),
                            token,
                            cx,
                        ))
                        .child(
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(auth_hint.to_string()),
                        )
                    })
                    // For Integrated / Entra auth modes, render the auth hint
                    // text. Entra Browser additionally renders an Entra
                    // account picker so the profile can be bound to a
                    // specific signed-in account.
                    .when(!file_engine && auth_mode == AuthMode::Integrated, |d| {
                        d.child(
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(auth_hint.to_string()),
                        )
                    })
                    .when(!file_engine && auth_mode == AuthMode::EntraBrowser, |d| {
                        // Redis: an optional user name for an access policy
                        // that names the identity; blank uses the object id.
                        let d = if redis_entra {
                            let redis_user = Input::new(&self.username_input)
                                .font_family(Theme::editor_font_family())
                                .disabled(account_locked);
                            d.child(Self::labeled_field(
                                &crate::localization::text("connections.editor.redis_user_label"),
                                redis_user,
                                cx,
                            ))
                        } else {
                            d
                        };
                        d.child(
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(auth_hint.to_string()),
                        )
                        .child(Self::render_entra_account_picker(self, cx))
                    })
                    .when(!file_engine && auth_mode == AuthMode::AwsRdsIam, |d| {
                        let iam_user = Input::new(&self.username_input)
                            .font_family(Theme::editor_font_family())
                            .disabled(account_locked);
                        let d = d
                            .child(Self::labeled_field(
                                &crate::localization::text(
                                    "connections.editor.database_user_label",
                                ),
                                iam_user,
                                cx,
                            ))
                            .child(Self::labeled_field(
                                &crate::localization::text("connections.editor.aws_region_label"),
                                aws_region,
                                cx,
                            ));
                        #[cfg(not(target_arch = "wasm32"))]
                        let d = d.child(Self::labeled_field(
                            &crate::localization::text("connections.editor.aws_profile_label"),
                            aws_profile,
                            cx,
                        ));
                        d.child(
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "connections.editor.aws_iam_hint",
                                )),
                        )
                    })
                    .when(!file_engine && auth_mode == AuthMode::GcpCloudSqlIam, |d| {
                        let iam_user = Input::new(&self.username_input)
                            .font_family(Theme::editor_font_family())
                            .disabled(account_locked);
                        let d = d.child(Self::labeled_field(
                            &crate::localization::text("connections.editor.gcp_iam_user_label"),
                            iam_user,
                            cx,
                        ));
                        #[cfg(not(target_arch = "wasm32"))]
                        let d = d.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(s(6.0)))
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size()))
                                        .text_color(Theme::text_dim())
                                        .child(if gcp_adc_status.is_empty() {
                                            crate::localization::text(
                                                "connections.editor.gcp_adc_checking",
                                            )
                                        } else {
                                            gcp_adc_status.clone()
                                        }),
                                )
                                .child(
                                    Button::new("ce-gcp-adc-check")
                                        .ghost()
                                        .small()
                                        .label(crate::localization::text(
                                            "connections.editor.gcp_adc_check_again",
                                        ))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.refresh_gcp_adc_status(cx);
                                        })),
                                ),
                        );
                        d.child(
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "connections.editor.gcp_iam_hint",
                                )),
                        )
                    })
                    // 7. Connection options (timeout + checkboxes)
                    .child(Self::labeled_field(
                        &crate::localization::text("connections.editor.timeout_label"),
                        timeout,
                        cx,
                    ))
                    .child(Self::labeled_field(
                        &crate::localization::text("connections.editor.startup_sql_label"),
                        startup_sql,
                        cx,
                    ))
                    .when(!key_delimiter_engine, |d| {
                        d.child(Self::labeled_field(
                            &crate::localization::text("connections.editor.display_zone_label"),
                            display_zone,
                            cx,
                        ))
                    })
                    .when_some(sentinel_section, |d, section| d.child(section))
                    .when(key_delimiter_engine, |d| {
                        d.child(Self::labeled_field(
                            &crate::localization::text(
                                "connections.editor.redis_key_delimiter_label",
                            ),
                            redis_delimiter,
                            cx,
                        ))
                        .child(Self::labeled_field(
                            &crate::localization::text(
                                "connections.editor.redis_cluster_mode_label",
                            ),
                            redis_cluster,
                            cx,
                        ))
                        .child(Self::labeled_field(
                            &crate::localization::text("connections.editor.redis_node_map_label"),
                            redis_node_map,
                            cx,
                        ))
                        .child(
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(redis_cluster_relay_note()),
                        )
                    })
                    .when(database_type == DatabaseType::ClickHouse, |d| {
                        d.child(Self::labeled_field(
                            &crate::localization::text(
                                "connections.editor.clickhouse_native_port_label",
                            ),
                            clickhouse_native_port,
                            cx,
                        ))
                    })
                    .when(compat_note.is_some(), |d| {
                        d.child(
                            div()
                                .id("ce-compat-note")
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(compat_note.unwrap().to_string()),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(s(6.0)))
                            .when(!tls_engine, |d| {
                                d.child(Self::render_checkbox(
                                    "ce-cb-encrypt",
                                    &crate::localization::text(
                                        "connections.editor.encrypt_connection_label",
                                    ),
                                    encrypt,
                                    tls_required,
                                    cx,
                                    Self::toggle_encrypt,
                                ))
                                .child(Self::render_checkbox(
                                    "ce-cb-trust",
                                    &crate::localization::text(
                                        "connections.editor.trust_certificate_label",
                                    ),
                                    trust,
                                    false,
                                    cx,
                                    Self::toggle_trust,
                                ))
                            })
                            .when_some(tls_section, |d, section| d.child(section))
                            .when(iam_tls, |d| {
                                d.child(
                                    div()
                                        .text_size(px(Theme::font_size()))
                                        .text_color(Theme::text_dim())
                                        .child(crate::localization::text(
                                            "connections.editor.iam_tls_hint",
                                        )),
                                )
                            })
                            .child(Self::render_checkbox(
                                "ce-cb-block",
                                &crate::localization::text("connections.editor.block_select_label"),
                                block_select,
                                false,
                                cx,
                                Self::toggle_block_select,
                            ))
                            .child(Self::render_checkbox(
                                "ce-cb-rollback",
                                &crate::localization::text("connections.editor.rollback_label"),
                                rollback,
                                false,
                                cx,
                                Self::toggle_rollback,
                            ))
                            .child(Self::render_checkbox(
                                "ce-cb-confirm-changes",
                                &crate::localization::text(
                                    "connections.editor.confirm_changes_label",
                                ),
                                confirm_changes,
                                false,
                                cx,
                                Self::toggle_confirm_changes,
                            ))
                            .child(Self::render_checkbox(
                                "ce-cb-allow-unbounded",
                                &crate::localization::text(
                                    "connections.editor.allow_unbounded_label",
                                ),
                                allow_unbounded,
                                false,
                                cx,
                                Self::toggle_allow_unbounded,
                            ))
                            .child(Self::labeled_field(
                                &crate::localization::text(
                                    "connections.editor.production_mode_label",
                                ),
                                Self::render_production_override(production_override, cx),
                                cx,
                            )),
                    )
                    // 8. Environment (below connection options)
                    .child(Self::section_label(&crate::localization::text(
                        "connections.editor.environment_section",
                    )))
                    .child(Self::render_environment_segmented(environment, cx))
                    // 8b. Routing (Direct vs. Via Relay) — desktop
                    // only. The wasm demo cannot open a relay.
                    .children(routing_section)
                    .when(ssh_section.is_some(), |d| {
                        d.child(
                            div()
                                .border_t_1()
                                .border_color(Theme::border())
                                .my(px(s(8.0))),
                        )
                    })
                    .children(ssh_section)
                    .when(kube_section.is_some(), |d| {
                        d.child(
                            div()
                                .border_t_1()
                                .border_color(Theme::border())
                                .my(px(s(8.0))),
                        )
                    })
                    .children(kube_section)
                    .when(ssm_section.is_some(), |d| {
                        d.child(
                            div()
                                .border_t_1()
                                .border_color(Theme::border())
                                .my(px(s(8.0))),
                        )
                    })
                    .children(ssm_section)
                    // 9. Client & Project (row) — free text that
                    // completes against the names already saved.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(s(8.0)))
                            .child(div().flex_1().child(client_field))
                            .child(div().flex_1().child(project_field)),
                    )
                    // 10. Notes — free text the user keeps on the connection.
                    .child(Self::labeled_field(
                        &crate::localization::text("connections.editor.notes_label"),
                        notes,
                        cx,
                    ))
                    .when_some(error.clone(), |st, err| {
                        st.child(
                            div()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::error())
                                .child(err),
                        )
                    }),
            )
            // Visible scrollbar thumb on the right edge, riding inside a
            // full-height invisible hit strip: pressing the strip jumps
            // there; holding drags.
            .when(scrollable, |d| {
                d.child(
                    div()
                        .id("ce_form_scrollbar")
                        .absolute()
                        .top_0()
                        .right_0()
                        .w(px(s(10.0)))
                        .h_full()
                        .cursor(gpui::CursorStyle::PointingHand)
                        .scrollbar_track(&self.scroll, gpui::Axis::Vertical)
                        .child(
                            div()
                                .absolute()
                                .top(px(thumb_y))
                                .right(px(s(2.0)))
                                .w(px(s(4.0)))
                                .h(px(thumb_h))
                                .rounded(px(s(2.0)))
                                .bg(Theme::text_dim().opacity(0.5)),
                        ),
                )
            });

        // Probe feedback shown in the footer, left of the action buttons: the
        // Test Connection outcome plus any collected server-info rows.
        let status_block = div()
            .flex_1()
            .min_w(px(0.0))
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(2.0)))
            .when_some(test_result.clone(), |st, result| {
                st.child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(if test_success {
                            Theme::success()
                        } else {
                            Theme::error()
                        })
                        .child(result),
                )
            })
            .when_some(server_info.clone(), |st, info| {
                st.children(server_info_rows(&info).into_iter().map(|(label, value)| {
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(Theme::scaled(2.0)))
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .child(
                            div()
                                .text_color(Theme::text_dim())
                                .child(format!("{label}:")),
                        )
                        .child(div().text_color(Theme::text()).child(value))
                }))
            });

        let host_ok = self.host_validation == HostValidationResult::Valid;

        Dialog::new("ce-dialog", |this: &mut Self| &mut this.frame)
            .title(title)
            .easy_close(true)
            .bare_body()
            .on_close(|this, _window, cx| this.handle_cancel(cx))
            .children_body(crate::browser_preview::preview_banner(if proxied {
                crate::localization::text("connections.editor.browser_proxy_banner")
            } else {
                crate::localization::text("connections.editor.browser_sample_banner")
            }))
            .child(form_body)
            .leading_button(
                Button::new("ce-btn-test")
                    .label(if testing {
                        crate::localization::text("connections.editor.test_running_button")
                    } else {
                        crate::localization::text("connections.editor.test_button")
                    })
                    .on_click(cx.listener(|this, _, _window, cx| this.handle_test(cx))),
            )
            .leading_button(status_block)
            .button(
                Button::new("ce-btn-save")
                    .primary()
                    .label(crate::localization::text("connections.editor.save_button"))
                    .disabled(!host_ok || proxied)
                    .on_click(cx.listener(|this, _, _window, cx| this.handle_save(cx))),
            )
            .build(self, window, cx)
            // Keeps a scrollbar drag tracking once the pointer leaves the
            // narrow track (the form scrollbar).
            .scrollbar_drag_area()
    }
}

/// Plain-value snapshot of the editor form, used to assemble a
/// `ConnectionProfile` independently of any gpui `Context` so the
/// assembly + validation rules can be unit-tested.
struct ProfileInputs {
    database_type: DatabaseType,
    /// The Redis-family "Auto-detect" picker entry is chosen:
    /// `database_type` follows the host name and the server's answer.
    engine_auto_detect: bool,
    name: String,
    server: String,
    port: String,
    database: String,
    client: String,
    project: String,
    timeout: String,
    startup_sql: String,
    notes: String,
    display_time_zone: Option<sqlly_core::DisplayTimeZone>,
    redis_key_delimiter: String,
    redis_cluster_mode: sqlly_core::redis_cluster::RedisClusterMode,
    redis_node_map: String,
    clickhouse_native_port: String,
    username: String,
    aws_region: String,
    aws_profile: String,
    password: String,
    sql_account: Option<Uuid>,
    auth_mode: AuthMode,
    /// Binding to a registered Microsoft Entra account. Only meaningful for
    /// `AuthMode::EntraBrowser` — selecting
    /// an Entra account from the picker populates this field so the
    /// downstream connection layer can match the profile to the correct
    /// cached token bundle.
    entra_account_id: Option<sqlly_core::EntraAccountBinding>,
    environment: ConnectionEnvironment,
    encrypt: bool,
    trust: bool,
    /// `Some` for PostgreSQL/MySQL engines (the editor's TLS select);
    /// `None` keeps the two legacy booleans authoritative (SQL Server).
    tls_mode: Option<TlsMode>,
    tls_ca_file: String,
    tls_client_cert_file: String,
    tls_client_key_file: String,
    tls_trust_os_roots: bool,
    tls_direct: bool,
    block_select: bool,
    rollback: bool,
    confirm_changes: bool,
    allow_unbounded: bool,
    production_override: Option<bool>,
    editing_id: Option<Uuid>,
    /// When true, SQL Login secrets are emitted as keychain account ids for
    /// persistence. When false, a typed password may stay in-memory for
    /// runtime-only probes (Test Connection / database list).
    persistable: bool,
    /// How the profile reaches its engine: [`ConnectionRoute::Direct`] or
    /// [`ConnectionRoute::ViaRelay`]. Secret-free — only device/pair ids.
    route: ConnectionRoute,
    tunnel_profile: Option<Uuid>,
    transport: TransportChain,
}

/// Resolves the editor's routing choice into a [`ConnectionRoute`]. Pure so the
/// persistence rules are unit-testable without a gpui context.
///
/// `Direct` whenever Via Relay is off. When on, the selected device must exist
/// in `devices` and carry an authorized `pair_id`; otherwise this returns an
/// error message the form surfaces instead of silently saving a Direct route.
fn build_relay_route(
    via_relay: bool,
    selected_device_id: Option<&str>,
    devices: &[RelayDeviceChoice],
    relay_hint: Option<String>,
) -> Result<ConnectionRoute, String> {
    if !via_relay {
        return Ok(ConnectionRoute::Direct);
    }
    let device_id = selected_device_id
        .ok_or_else(|| crate::localization::text("connections.editor.relay_device_required"))?;
    let device = devices
        .iter()
        .find(|d| d.device_id == device_id)
        .ok_or_else(|| crate::localization::text("connections.editor.relay_device_required"))?;
    let pair_id = device
        .pair_id
        .clone()
        .ok_or_else(|| crate::localization::text("connections.editor.relay_device_unauthorized"))?;
    Ok(ConnectionRoute::ViaRelay {
        relay_device_id: device.device_id.clone(),
        pair_id,
        relay_hint,
    })
}

/// Joins the account's relay connectors — its OWN plus bridges SHARED with it
/// — with its route pairs, resolving each device to the id of an active
/// authorized pair fronting it (if any). Only pairs whose control side is THIS
/// app device count: the account may hold pairs authorizing *other* app
/// devices for the same connector, and routing with one of those gets denied
/// by the relay. Shared bridges are labelled "(shared)" and deduped against
/// owned ones. Pure so the pairing resolution is testable independently of the
/// store I/O.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn merge_relay_devices(
    devices: &[crate::store::RelayConnector],
    shared: &[crate::store::SharedRelayConnector],
    pairs: &[crate::store::RelayPair],
    control_device_id: Option<&str>,
) -> Vec<RelayDeviceChoice> {
    let pair_for = |connector_device_id: &str| {
        pairs
            .iter()
            .find(|p| {
                p.active
                    && p.connector_device_id == connector_device_id
                    && control_device_id == Some(p.control_device_id.as_str())
            })
            .map(|p| p.id.clone())
    };

    let mut choices: Vec<RelayDeviceChoice> = devices
        .iter()
        .map(|device| RelayDeviceChoice {
            device_id: device.device_id.clone(),
            name: device.display_name(),
            pair_id: pair_for(&device.device_id),
        })
        .collect();

    for bridge in shared {
        if choices.iter().any(|c| c.device_id == bridge.device_id) {
            continue;
        }
        choices.push(RelayDeviceChoice {
            device_id: bridge.device_id.clone(),
            name: crate::localization::format(
                "connections.editor.relay_shared_device",
                &[("device_name", bridge.name.as_str())],
            ),
            pair_id: pair_for(&bridge.device_id),
        });
    }

    choices
}

/// Loads the picker-visible view of every registered Entra account from the
/// on-disk registry. Pure so the editor's tests can substitute a deterministic
/// fixture without depending on the registry's file path.
/// Trim, drop the empties, fold case-insensitive duplicates, and sort — the
/// shape a completion list has to be in to be worth reading.
fn entity_names(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        let name = name.trim().to_string();
        if name.is_empty() || out.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            continue;
        }
        out.push(name);
    }
    out.sort_by_key(|n| n.to_lowercase());
    out
}

fn load_account_summaries(registry: &EntraAccountRegistryStore) -> Vec<EntraAccountSummary> {
    registry
        .list()
        .iter()
        .map(|profile| EntraAccountSummary {
            profile_id: profile.id.0.clone(),
            tenant_id: profile.tenant_id.clone(),
            username: Some(profile.username.clone()),
            display_name: profile.display_name.clone(),
        })
        .collect()
}

fn sole_entra_binding(accounts: &[EntraAccountSummary]) -> Option<sqlly_core::EntraAccountBinding> {
    let [account] = accounts else {
        return None;
    };
    Some(sqlly_core::EntraAccountBinding {
        account_id: account.profile_id.clone(),
        tenant_id: account.tenant_id.clone(),
        username: account.username.clone(),
    })
}

/// Safety defaults implied by an environment: `(force_block_select,
/// force_rollback)`. Production forces SELECT-only and rollback; Staging forces
/// only rollback; lower environments force neither. These only ever turn a
/// safety net ON (never off), matching the Swift behavior.
fn environment_safety_defaults(env: ConnectionEnvironment) -> (bool, bool) {
    match env {
        ConnectionEnvironment::Production => (true, true),
        ConnectionEnvironment::Staging => (false, true),
        _ => (false, false),
    }
}

/// Validate and assemble a `ConnectionProfile` from the form inputs. Pure: no
/// gpui dependencies, so it is directly unit-testable.
fn assemble_profile(input: ProfileInputs) -> Result<ConnectionProfile, String> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(crate::localization::text(
            "connections.editor.name_required",
        ));
    }

    let server = input.server.trim().to_string();
    if server.is_empty() {
        return Err(crate::localization::text(
            "connections.editor.server_required",
        ));
    }
    if !crate::engine_form::is_file_engine(input.database_type) {
        match validate_server_text(input.database_type, &server) {
            HostValidationResult::Valid => {}
            HostValidationResult::Empty => {
                return Err(crate::localization::text(
                    "connections.editor.server_required",
                ));
            }
            HostValidationResult::InvalidHost => {
                return Err(crate::localization::text(
                    "connections.editor.host_validation_invalid_host",
                ));
            }
            HostValidationResult::InvalidInstance => {
                return Err(crate::localization::text(
                    "connections.editor.host_validation_invalid_instance",
                ));
            }
        }
    }

    let port: u16 = if crate::engine_form::is_file_engine(input.database_type) {
        0
    } else {
        input
            .port
            .trim()
            .parse()
            .map_err(|_| crate::localization::text("connections.editor.port_invalid"))?
    };

    let opt = |s: &str| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };
    let database = opt(&input.database);
    let client = opt(&input.client);
    let project = opt(&input.project);

    let timeout: u64 = input.timeout.trim().parse().unwrap_or(30);

    let username = input.username.trim().to_string();
    let password = input.password.clone();

    // Username/password are only meaningful for SQL Login; the other auth
    // modes resolve credentials from the system/token cache.
    if input.auth_mode == AuthMode::SqlLogin
        && username.is_empty()
        && !crate::engine_form::is_file_engine(input.database_type)
    {
        return Err(crate::localization::text(
            "connections.editor.sql_username_required",
        ));
    }

    if input.database_type.requires_tls() && !input.encrypt {
        return Err(crate::localization::text(
            "connections.editor.encryption_required",
        ));
    }

    let mut profile = ConnectionStore::create_sql_login(
        name,
        server,
        port,
        database,
        username.clone(),
        password.clone(),
        input.encrypt,
        input.trust,
    );
    profile.environment = input.environment;
    profile.database_type = input.database_type;
    profile.client = client;
    profile.project = project;
    profile.timeout_seconds = if timeout == 0 { 30 } else { timeout };
    profile.display_time_zone = input.display_time_zone.clone();
    profile.startup_sql = {
        let text = input.startup_sql.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    };
    profile.notes = {
        let text = input.notes.trim();
        (!text.is_empty()).then(|| text.to_string())
    };
    profile.redis_key_delimiter = {
        let text = input.redis_key_delimiter.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    };
    profile.engine_auto_detect =
        input.engine_auto_detect && input.database_type.is_command_engine();
    profile.redis_cluster = if crate::engine_form::uses_key_delimiter(input.database_type) {
        let config = sqlly_core::redis_cluster::RedisClusterConfig {
            mode: input.redis_cluster_mode,
            node_map: sqlly_core::redis_cluster::parse_node_map(&input.redis_node_map).map_err(
                |e| {
                    crate::localization::format(
                        "connections.editor.redis_node_map_invalid",
                        &[("error", e.as_str())],
                    )
                },
            )?,
        };
        (!config.is_default()).then_some(config)
    } else {
        None
    };
    // Redis with Entra sign-in: an optional Redis user name (blank signs in
    // as the token's object id).
    profile.redis_entra_user = (input.auth_mode == AuthMode::EntraBrowser
        && input.database_type.is_command_engine()
        && !input.username.trim().is_empty())
    .then(|| input.username.trim().to_string());
    // Only ClickHouse has a separate native-protocol port; other engines
    // never keep one.
    profile.clickhouse_native_port = match input.clickhouse_native_port.trim() {
        text if text.is_empty() || input.database_type != DatabaseType::ClickHouse => None,
        text => Some(
            text.parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| {
                    crate::localization::text("connections.editor.clickhouse_native_port_invalid")
                })?,
        ),
    };
    profile.block_all_but_select = input.block_select;
    profile.auto_wrap_rollback = input.rollback;
    profile.confirm_data_changes = input.confirm_changes;
    profile.allow_unbounded_mutations = input.allow_unbounded;
    profile.production_override = input.production_override;

    if let Some(mode) = input.tls_mode {
        profile.tls_mode = mode;
        // Keep the two legacy booleans in sync so builds that predate
        // `tls_mode` read a safe equivalent of the selected mode.
        let (encrypt, trust) = mode.legacy_flags();
        profile.encrypt = encrypt;
        profile.trust_server_certificate = trust;
    }
    let tls_path = |s: &str| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(std::path::PathBuf::from(t))
        }
    };
    profile.tls_ca_file = tls_path(&input.tls_ca_file);
    profile.tls_trust_os_roots = input.tls_trust_os_roots;
    profile.tls_direct = input.tls_direct
        && input.database_type.connection_kind() == sqlly_core::ConnectionKind::PostgresWire;
    profile.tls_client_cert_file = tls_path(&input.tls_client_cert_file);
    profile.tls_client_key_file = tls_path(&input.tls_client_key_file);
    if profile.tls_client_cert_file.is_some() != profile.tls_client_key_file.is_some() {
        return Err(crate::localization::text(
            "connections.editor.tls_client_pair_required",
        ));
    }

    profile.authentication = match input.auth_mode {
        // An empty field is an account that has no password (a saved one
        // shows the mask, never an empty field): the profile carries an empty
        // in-memory secret and needs no keyring entry, like a blank libSQL
        // token below.
        AuthMode::SqlLogin => {
            let account_id = if password.is_empty() {
                None
            } else if input.persistable {
                Some(input.sql_account.unwrap_or_else(Uuid::new_v4))
            } else {
                input.sql_account
            };
            AuthenticationType::SqlLogin {
                username,
                secret_ref: SecretRef {
                    provider: if account_id.is_some() {
                        SecretProvider::MacOsKeychain
                    } else {
                        SecretProvider::InMemory
                    },
                    key: account_id.map(|id| id.to_string()).unwrap_or(password),
                },
            }
        }
        // A libSQL token is stored like a password (keyring entry keyed by a
        // fresh id, or the saved entry's id when editing); a blank field is
        // an anonymous local server and needs no keyring entry at all.
        AuthMode::AuthToken => {
            let blank = password.trim().is_empty();
            let account_id = if blank {
                None
            } else if input.persistable {
                Some(input.sql_account.unwrap_or_else(Uuid::new_v4))
            } else {
                input.sql_account
            };
            AuthenticationType::AuthToken {
                secret_ref: SecretRef {
                    provider: if account_id.is_some() {
                        SecretProvider::MacOsKeychain
                    } else {
                        SecretProvider::InMemory
                    },
                    key: account_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| password.trim().to_string()),
                },
            }
        }
        AuthMode::Integrated => AuthenticationType::Integrated,
        AuthMode::EntraBrowser => AuthenticationType::EntraBrowser,
        AuthMode::AwsRdsIam => AuthenticationType::AwsRdsIam {
            username,
            region: input.aws_region.trim().to_string(),
            profile: {
                let profile = input.aws_profile.trim();
                if profile.is_empty() {
                    None
                } else {
                    Some(profile.to_string())
                }
            },
        },
        AuthMode::GcpCloudSqlIam => AuthenticationType::GcpCloudSqlIam { username },
    };
    if matches!(
        profile.authentication,
        AuthenticationType::AwsRdsIam { .. } | AuthenticationType::GcpCloudSqlIam { .. }
    ) {
        profile.encrypt = true;
    }
    if !crate::engine_form::is_file_engine(input.database_type)
        && !crate::engine_form::authentication_supported(
            input.database_type,
            &profile.authentication,
        )
    {
        return Err(crate::localization::format(
            "connections.editor.authentication_unsupported",
            &[
                ("authentication", input.auth_mode.label().as_str()),
                ("engine", input.database_type.display_name()),
            ],
        ));
    }

    // Bind to the selected Entra account when the user picked one. Only
    // meaningful in `EntraBrowser` mode, but harmless
    // to persist otherwise (the runtime ignores `entra_account` on SQL
    // Login / Integrated profiles).
    profile.entra_account = input.entra_account_id;

    // Persist the routing choice. Secret-free by construction: `ViaRelay` only
    // ever carries device/pair ids and an advisory endpoint hint.
    profile.route = input.route;
    profile.tunnel_profile = input.tunnel_profile;
    if input.tunnel_profile.is_some() {
        profile.transport = TransportChain::default();
        profile.ssh_tunnel = None;
    } else {
        profile.transport = input.transport.clone();
        profile.ssh_tunnel = input.transport.ssh.clone();
    }

    if let Some(id) = input.editing_id {
        profile.id.0 = id;
    }
    Ok(profile)
}

pub(crate) fn parse_ssh_port(port: &str) -> Result<u16, String> {
    let port: u16 = port
        .trim()
        .parse()
        .map_err(|_| crate::localization::text("connections.editor.ssh_port_invalid"))?;
    if port == 0 {
        return Err(crate::localization::text(
            "connections.editor.ssh_port_invalid",
        ));
    }
    Ok(port)
}

pub(crate) fn build_ssh_hop(
    host: &str,
    port: &str,
    user: &str,
    auth: SshAuth,
) -> Result<SshHop, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err(crate::localization::text(
            "connections.editor.ssh_host_required",
        ));
    }
    let username = user.trim();
    if username.is_empty() {
        return Err(crate::localization::text(
            "connections.editor.ssh_username_required",
        ));
    }
    Ok(SshHop {
        host: host.to_string(),
        port: parse_ssh_port(port)?,
        username: username.to_string(),
        auth,
    })
}

fn build_ssh_secret_ref(
    secret_input: &str,
    secret_id: Option<Uuid>,
    persistable: bool,
    required: bool,
) -> Result<SecretRef, String> {
    let mask = password_mask();
    let trimmed = secret_input.trim();
    if !trimmed.is_empty() && trimmed != mask {
        if persistable {
            let id = secret_id.unwrap_or_else(Uuid::new_v4);
            return Ok(SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: id.to_string(),
            });
        }
        return Ok(SecretRef {
            provider: SecretProvider::InMemory,
            key: trimmed.to_string(),
        });
    }
    if let Some(id) = secret_id {
        return Ok(SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: id.to_string(),
        });
    }
    if required {
        return Err(crate::localization::text(
            "connections.editor.ssh_password_required",
        ));
    }
    Err(crate::localization::text(
        "connections.editor.ssh_secret_required",
    ))
}

fn build_optional_ssh_secret_ref(
    secret_input: &str,
    secret_id: Option<Uuid>,
    persistable: bool,
) -> Result<Option<SecretRef>, String> {
    let mask = password_mask();
    let trimmed = secret_input.trim();
    if trimmed.is_empty() || trimmed == mask {
        return Ok(secret_id.map(|id| SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: id.to_string(),
        }));
    }
    Ok(Some(build_ssh_secret_ref(
        secret_input,
        secret_id,
        persistable,
        false,
    )?))
}

pub(crate) fn build_ssh_auth(
    mode: SshAuthMode,
    key_path: &str,
    secret_input: &str,
    secret_id: Option<Uuid>,
    persistable: bool,
) -> Result<SshAuth, String> {
    match mode {
        SshAuthMode::Agent => Ok(SshAuth::Agent),
        SshAuthMode::KeyFile => {
            let path = key_path.trim();
            if path.is_empty() {
                return Err(crate::localization::text(
                    "connections.editor.ssh_private_key_required",
                ));
            }
            Ok(SshAuth::KeyFile {
                path: path.to_string(),
                passphrase: build_optional_ssh_secret_ref(secret_input, secret_id, persistable)?,
            })
        }
        SshAuthMode::Password => Ok(SshAuth::Password {
            secret_ref: build_ssh_secret_ref(secret_input, secret_id, persistable, true)?,
        }),
    }
}

fn transport_validation_message(message: &str) -> String {
    let key = match message {
        "Kubernetes namespace is required" => "connections.editor.kube_namespace_required",
        "Kubernetes namespace contains invalid characters" => {
            "connections.editor.kube_namespace_invalid"
        }
        "Kubernetes target name is required" => "connections.editor.kube_target_required",
        "Kubernetes target name contains invalid characters" => {
            "connections.editor.kube_target_invalid"
        }
        "Remote port must be 1–65535" => "connections.editor.kube_remote_port_invalid",
        "Kubernetes context cannot be empty when set" => "connections.editor.kube_context_empty",
        "Kubernetes context contains invalid characters" => {
            "connections.editor.kube_context_invalid"
        }
        "Kubeconfig path cannot be empty when set" => "connections.editor.kubeconfig_empty",
        "Kubernetes port-forward cannot be combined with SSH or proxy hops" => {
            "connections.editor.transport_kube_combination_invalid"
        }
        "AWS Session Manager cannot be combined with other tunnel or proxy hops" => {
            "connections.editor.transport_ssm_combination_invalid"
        }
        "AWS Session Manager target is required" => "connections.editor.ssm_target_required",
        "AWS Session Manager target contains invalid characters" => {
            "connections.editor.ssm_target_invalid"
        }
        "AWS region contains invalid characters" => "connections.editor.ssm_region_invalid",
        "AWS profile contains invalid characters" => "connections.editor.ssm_profile_invalid",
        "AWS Session Manager remote host contains invalid characters" => {
            "connections.editor.ssm_remote_host_invalid"
        }
        "Proxy host is required" => "connections.editor.proxy_host_required",
        "Proxy port must be 1–65535" => "connections.editor.proxy_port_invalid",
        "Proxy username is required when a password is set" => {
            "connections.editor.proxy_username_required"
        }
        "SSH host is required" => "connections.editor.ssh_host_required",
        "SSH port must be 1–65535" => "connections.editor.ssh_port_invalid",
        "SSH username is required" => "connections.editor.ssh_username_required",
        "SSH jump host is required" => "connections.editor.ssh_jump_host_required",
        "SSH jump port must be 1–65535" => "connections.editor.ssh_jump_port_invalid",
        "SSH jump username is required" => "connections.editor.ssh_jump_username_required",
        "Transport chain cannot exceed two hops" => "connections.editor.transport_too_many_hops",
        _ => return message.to_string(),
    };
    crate::localization::text(key)
}

/// Which Session Manager text field an input edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SsmField {
    Target,
    Region,
    Profile,
    RemoteHost,
}

/// Shown instead of the Session Manager fields when the engine or build
/// cannot use a TCP transport hop.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn ssm_block_message(block: crate::capabilities::TransportBlock) -> String {
    use crate::capabilities::TransportBlock;
    crate::localization::text(match block {
        TransportBlock::DesktopRequired => "connections.editor.ssm_desktop_required",
        TransportBlock::BrowserUnavailable => "connections.editor.ssm_browser_unavailable",
        TransportBlock::SqliteFile => "connections.editor.ssm_sqlite_unavailable",
        TransportBlock::DuckDbFile => "connections.editor.ssm_duckdb_unavailable",
        TransportBlock::EngineUnsupported => "connections.editor.ssm_engine_unavailable",
    })
}

/// A program a Session Manager port-forward needs that is not installed.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SsmMissingTool {
    AwsCli,
    Plugin,
}

/// Which program (if any) to tell the user to install; the CLI first.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn ssm_tool_status(aws_cli_found: bool, plugin_found: bool) -> Option<SsmMissingTool> {
    if !aws_cli_found {
        Some(SsmMissingTool::AwsCli)
    } else if !plugin_found {
        Some(SsmMissingTool::Plugin)
    } else {
        None
    }
}

/// The install hint under the Session Manager fields.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn ssm_missing_tool_message(missing: SsmMissingTool) -> String {
    crate::localization::text(match missing {
        SsmMissingTool::AwsCli => "connections.editor.ssm_aws_cli_missing",
        SsmMissingTool::Plugin => "connections.editor.ssm_plugin_missing",
    })
}

fn build_ssm_port_forward(
    enabled: bool,
    target: &str,
    region: &str,
    profile: &str,
    remote_host: &str,
) -> Result<Option<SsmPortForward>, String> {
    if !enabled {
        return Ok(None);
    }
    let ssm = SsmPortForward {
        target: target.trim().to_string(),
        region: opt_empty(region),
        profile: opt_empty(profile),
        remote_host: opt_empty(remote_host),
    };
    let errors = ssm.validate();
    if errors.is_empty() {
        Ok(Some(ssm))
    } else {
        Err(errors
            .iter()
            .map(|error| transport_validation_message(error))
            .collect::<Vec<_>>()
            .join("; "))
    }
}

fn build_kube_port_forward(
    enabled: bool,
    kubeconfig: &str,
    context: &str,
    namespace: &str,
    target_kind: KubeTargetKind,
    target_name: &str,
    remote_port: &str,
) -> Result<Option<KubePortForward>, String> {
    if !enabled {
        return Ok(None);
    }
    let remote_port: u16 = remote_port
        .trim()
        .parse()
        .map_err(|_| crate::localization::text("connections.editor.kube_remote_port_invalid"))?;
    let kube = KubePortForward {
        kubeconfig: opt_empty(kubeconfig),
        context: opt_empty(context),
        namespace: if namespace.trim().is_empty() {
            "default".to_string()
        } else {
            namespace.trim().to_string()
        },
        target_kind,
        target_name: target_name.trim().to_string(),
        remote_port,
    };
    let errors = kube.validate();
    if errors.is_empty() {
        Ok(Some(kube))
    } else {
        Err(errors
            .iter()
            .map(|error| transport_validation_message(error))
            .collect::<Vec<_>>()
            .join("; "))
    }
}

fn opt_empty(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
#[allow(clippy::too_many_arguments)]
fn build_ssh_tunnel(
    enabled: bool,
    host: &str,
    port: &str,
    user: &str,
    auth_mode: SshAuthMode,
    key_path: &str,
    secret_input: String,
    secret_id: Option<Uuid>,
    jump_enabled: bool,
    jump_host: &str,
    jump_port: &str,
    jump_user: &str,
    persistable: bool,
) -> Result<Option<SshTunnelConfig>, String> {
    if !enabled {
        return Ok(None);
    }
    let auth = build_ssh_auth(auth_mode, key_path, &secret_input, secret_id, persistable)?;
    let target = build_ssh_hop(host, port, user, auth.clone())?;
    let jump = if jump_enabled {
        Some(build_ssh_hop(jump_host, jump_port, jump_user, auth)?)
    } else {
        None
    };
    Ok(Some(SshTunnelConfig {
        target,
        jump,
        keepalive: Default::default(),
    }))
}

/// The SSH keepalive the form describes: blank is the default interval,
/// a whole number of seconds sets it (0 turns keepalives off).
fn parse_ssh_keepalive(
    interval: &str,
    max_missed: Option<u32>,
) -> Result<sqlly_core::SshKeepalive, String> {
    let interval = interval.trim();
    let interval_seconds = if interval.is_empty() {
        None
    } else {
        Some(
            interval
                .parse::<u64>()
                .ok()
                .filter(|s| *s <= 86_400)
                .ok_or_else(|| {
                    crate::localization::text("connections.editor.ssh_keepalive_invalid")
                })?,
        )
    };
    Ok(sqlly_core::SshKeepalive {
        interval_seconds,
        max_missed,
    })
}

/// Test Connection for the editor. A direct Redis connection runs the staged
/// trace (resolve, tunnel / proxy, TCP, TLS, AUTH, HELLO / PING, SELECT,
/// CLUSTER INFO, session) and writes each stage to the diagnostic log as it
/// finishes, so the log shows how far the connection got; its error is
/// headed by the stage that failed. Every other connection (and a Redis one
/// routed through a relay, which this machine cannot trace) tests through
/// its services as before.
async fn run_connection_test(
    services: &sqlly_services::Services,
    profile: &ConnectionProfile,
    log: &crate::log_console::DiagnosticLog,
) -> Result<(), sqlly_core::StudioError> {
    #[cfg(not(target_arch = "wasm32"))]
    if profile.database_type.is_command_engine()
        && matches!(
            profile.route,
            sqlly_core::connection::ConnectionRoute::Direct
        )
        && profile.provenance != sqlly_core::ConnectionProvenance::Proxied
    {
        use sqlly_engines::redis::diagnose::{Outcome, StageReport, trace_connection};
        let profile = profile.clone();
        let log = log.clone();
        let task = crate::tokio_handle().spawn(async move {
            let label = profile.name.clone();
            let mut sink = move |report: &StageReport| {
                let line = format!("\"{label}\" {}", report.line());
                if report.outcome == Outcome::Failed {
                    log.log_error("connection_test", &line);
                } else {
                    log.log_verbose("connection_test", &line);
                }
            };
            trace_connection(&profile, &mut sink).await.into_result()
        });
        return task.await.unwrap_or_else(|e| {
            let error = e.to_string();
            let message = crate::localization::format(
                "connections.editor.connection_test_stopped",
                &[("error", error.as_str())],
            );
            Err(sqlly_core::StudioError::Connection(message))
        });
    }
    let _ = log;
    services.connections.test_connection(profile).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_transport_chain(
    proxy_enabled: bool,
    proxy_kind: ProxyKind,
    proxy_host: &str,
    proxy_port: &str,
    proxy_user: &str,
    proxy_secret_input: String,
    proxy_secret_id: Option<Uuid>,
    ssh_enabled: bool,
    host: &str,
    port: &str,
    user: &str,
    auth_mode: SshAuthMode,
    key_path: &str,
    secret_input: String,
    secret_id: Option<Uuid>,
    jump_enabled: bool,
    jump_host: &str,
    jump_port: &str,
    jump_user: &str,
    persistable: bool,
) -> Result<TransportChain, String> {
    let proxy = if proxy_enabled {
        let host = proxy_host.trim();
        if host.is_empty() {
            return Err(crate::localization::text(
                "connections.editor.proxy_host_required",
            ));
        }
        let port: u16 = proxy_port
            .trim()
            .parse()
            .map_err(|_| crate::localization::text("connections.editor.proxy_port_invalid"))?;
        if port == 0 {
            return Err(crate::localization::text(
                "connections.editor.proxy_port_invalid",
            ));
        }
        let username = {
            let u = proxy_user.trim();
            if u.is_empty() {
                None
            } else {
                Some(u.to_string())
            }
        };
        let password = if username.is_some() {
            Some(build_ssh_secret_ref(
                &proxy_secret_input,
                proxy_secret_id,
                persistable,
                false,
            )?)
        } else if !proxy_secret_input.trim().is_empty()
            && proxy_secret_input.trim() != password_mask()
        {
            return Err(crate::localization::text(
                "connections.editor.proxy_username_required",
            ));
        } else {
            None
        };
        Some(ProxyHop {
            kind: proxy_kind,
            host: host.to_string(),
            port,
            username,
            password,
        })
    } else {
        None
    };
    let ssh = build_ssh_tunnel(
        ssh_enabled,
        host,
        port,
        user,
        auth_mode,
        key_path,
        secret_input,
        secret_id,
        jump_enabled,
        jump_host,
        jump_port,
        jump_user,
        persistable,
    )?;
    let chain = TransportChain {
        proxy,
        ssh,
        kube: None,
        ssm: None,
    };
    let errors = chain.validate();
    if errors.is_empty() {
        Ok(chain)
    } else {
        Err(errors
            .iter()
            .map(|error| transport_validation_message(error))
            .collect::<Vec<_>>()
            .join("; "))
    }
}

fn load_ssh_secret_into_editor(
    secret_ref: Option<&SecretRef>,
    secret_id: &mut Option<Uuid>,
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut Context<ConnectionEditor>,
) {
    let Some(secret_ref) = secret_ref else {
        return;
    };
    if secret_ref.provider == SecretProvider::MacOsKeychain {
        *secret_id = Uuid::parse_str(&secret_ref.key).ok();
        input.update(cx, |field, cx| {
            field.set_value(password_mask(), window, cx);
        });
    } else if secret_ref.provider == SecretProvider::InMemory {
        let secret = secret_ref.key.clone();
        input.update(cx, |field, cx| field.set_value(secret, window, cx));
    }
}

fn store_ssh_secrets_from_tunnel(
    tunnel: &SshTunnelConfig,
    secret_input: &str,
) -> Result<(), String> {
    let mask = password_mask();
    let typed = secret_input.trim();
    if typed.is_empty() || typed == mask {
        return Ok(());
    }
    store_ssh_auth_secret(&tunnel.target.auth, typed)?;
    if let Some(jump) = &tunnel.jump {
        store_ssh_auth_secret(&jump.auth, typed)?;
    }
    Ok(())
}

fn store_ssh_auth_secret(auth: &SshAuth, secret: &str) -> Result<(), String> {
    match auth {
        SshAuth::Password { secret_ref }
        | SshAuth::KeyFile {
            passphrase: Some(secret_ref),
            ..
        } if secret_ref.provider == SecretProvider::MacOsKeychain => {
            let id = Uuid::parse_str(&secret_ref.key).map_err(|_| {
                crate::localization::text("connections.editor.invalid_ssh_secret_id")
            })?;
            crate::sql_credentials::store_password(id, secret)
        }
        _ => Ok(()),
    }
}

pub(crate) fn server_info_rows(info: &ServerInfo) -> Vec<(String, String)> {
    let label = |key| crate::localization::text(key);
    let mut rows = Vec::new();
    if !info.product.is_empty() {
        rows.push((
            label("connections.editor.server_info_product"),
            info.product.clone(),
        ));
    }
    if !info.version.is_empty() {
        rows.push((
            label("connections.editor.server_info_version"),
            info.version.clone(),
        ));
    }
    if let Some(charset) = &info.server_charset {
        rows.push((
            label("connections.editor.server_info_charset"),
            charset.clone(),
        ));
    }
    if let Some(collation) = &info.server_collation {
        rows.push((
            label("connections.editor.server_info_collation"),
            collation.clone(),
        ));
    }
    match info.identifier_case {
        IdentifierCase::Sensitive => rows.push((
            label("connections.editor.server_info_identifiers"),
            label("connections.editor.server_info_case_sensitive"),
        )),
        IdentifierCase::Insensitive => rows.push((
            label("connections.editor.server_info_identifiers"),
            label("connections.editor.server_info_case_insensitive"),
        )),
        IdentifierCase::Unknown => {}
    }
    if let Some(user) = &info.current_user {
        rows.push((label("connections.editor.server_info_user"), user.clone()));
    }
    if let Some(database) = &info.database {
        rows.push((
            label("connections.editor.server_info_database"),
            database.clone(),
        ));
    }
    if let Some(tls) = info.tls_in_use {
        rows.push((
            label("connections.editor.server_info_tls"),
            if tls {
                label("connections.editor.server_info_encrypted")
            } else {
                label("connections.editor.server_info_not_encrypted")
            },
        ));
    }
    if let Some(latency_ms) = info.latency_ms {
        let milliseconds = latency_ms.to_string();
        rows.push((
            label("connections.editor.server_info_latency"),
            crate::localization::format(
                "connections.editor.server_info_latency_value",
                &[("milliseconds", milliseconds.as_str())],
            ),
        ));
    }
    rows
}

/// The Microsoft Entra hint for a Redis profile: what the sign-in does, and
/// a nudge when the host is not an Azure Redis endpoint.
pub(crate) fn redis_entra_host_hint(server: &str) -> String {
    let host = server.trim();
    #[cfg(not(target_arch = "wasm32"))]
    let azure = sqlly_engines::redis::entra::is_azure_redis_host(host);
    #[cfg(target_arch = "wasm32")]
    let azure = host.ends_with(".redis.cache.windows.net") || host.ends_with(".redis.azure.net");
    if host.is_empty() || azure {
        crate::localization::text("connections.editor.redis_entra_host_hint")
    } else {
        crate::localization::text("connections.editor.redis_entra_non_azure_host_hint")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn redis_entra_hint_flags_non_azure_hosts() {
        let azure = super::redis_entra_host_hint("contoso.redis.cache.windows.net");
        assert!(azure.starts_with("Signs in"), "{azure}");
        assert!(super::redis_entra_host_hint("").starts_with("Signs in"));
        let local = super::redis_entra_host_hint("127.0.0.1");
        assert!(local.contains("not one of them"), "{local}");
    }

    use super::*;

    #[test]
    fn localized_mode_labels_round_trip_in_spanish() {
        let _guard = crate::localization::test_language(crate::localization::Language::Spanish);
        let tls_labels = tls_mode_labels();
        let cluster_labels = redis_cluster_mode_labels();
        let tls_round_trips = TLS_MODE_CHOICES.into_iter().enumerate().all(|(row, mode)| {
            tls_mode_from_row(row) == Some(mode) && tls_labels[row] == tls_mode_label(mode)
        });
        let cluster_round_trips = sqlly_core::redis_cluster::RedisClusterMode::ALL
            .into_iter()
            .enumerate()
            .all(|(row, mode)| {
                redis_cluster_mode_from_row(row) == Some(mode)
                    && cluster_labels[row] == redis_cluster_mode_label(mode)
            });
        crate::localization::activate(crate::localization::Language::English, None).unwrap();
        assert!(tls_round_trips);
        assert!(cluster_round_trips);
        assert_eq!(tls_mode_from_row(TLS_MODE_CHOICES.len()), None);
    }

    #[test]
    fn display_zone_rows_round_trip_and_reach_the_profile() {
        use sqlly_core::DisplayTimeZone;
        let labels = display_zone_labels();
        for (row, label) in labels.iter().enumerate() {
            let zone = display_zone_from_row(row).expect("every row maps to a zone");
            assert_eq!(&display_zone_label(zone.as_ref()), label);
        }
        assert_eq!(display_zone_from_row(labels.len()), None);
        assert_eq!(display_zone_from_row(0), Some(None));
        assert_eq!(display_zone_from_row(1), Some(Some(DisplayTimeZone::Utc)));
        let berlin = labels
            .iter()
            .position(|l| l == "Europe/Berlin")
            .and_then(display_zone_from_row);
        assert_eq!(
            berlin,
            Some(Some(DisplayTimeZone::Named("Europe/Berlin".into())))
        );

        let mut input = inputs();
        input.display_time_zone = Some(DisplayTimeZone::Named("Asia/Tokyo".into()));
        let profile = assemble_profile(input).expect("valid inputs");
        assert_eq!(
            profile.display_time_zone,
            Some(DisplayTimeZone::Named("Asia/Tokyo".into()))
        );
        let profile = assemble_profile(inputs()).expect("valid inputs");
        assert_eq!(profile.display_time_zone, None);
    }

    #[test]
    fn choice_selects_map_rows_not_label_text() {
        assert_eq!(SshAuthMode::from_select_row(0), Some(SshAuthMode::Agent));
        assert_eq!(SshAuthMode::from_select_row(1), Some(SshAuthMode::KeyFile));
        assert_eq!(SshAuthMode::from_select_row(2), Some(SshAuthMode::Password));
        assert_eq!(SshAuthMode::from_select_row(3), None);
        assert_eq!(SshAuthMode::select_labels().len(), 3);
        assert_eq!(kube_kind_from_row(0), Some(KubeTargetKind::Pod));
        assert_eq!(kube_kind_from_row(1), Some(KubeTargetKind::Service));
        assert_eq!(kube_kind_from_row(2), Some(KubeTargetKind::Deployment));
    }

    #[test]
    fn unique_select_labels_suffixes_repeats() {
        let labels = unique_select_labels(
            ["(none)", "a", "(none)", "a", "a (2)"]
                .into_iter()
                .map(String::from),
        );
        assert_eq!(labels, ["(none)", "a", "(none) (2)", "a (2)", "a (2) (2)"]);
    }

    #[test]
    fn tunnel_profile_row_maps_by_position_not_name() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        // A profile literally named like the "no tunnel profile" entry sits
        // at row 1 and must resolve to its own id, not to "none".
        let ids = [a, b];
        assert_eq!(tunnel_profile_from_row(None, &ids), None);
        assert_eq!(tunnel_profile_from_row(Some(0), &ids), None);
        assert_eq!(tunnel_profile_from_row(Some(1), &ids), Some(a));
        assert_eq!(tunnel_profile_from_row(Some(2), &ids), Some(b));
        assert_eq!(tunnel_profile_from_row(Some(3), &ids), None);
    }

    #[test]
    fn redis_entra_user_is_saved_only_for_redis_entra_and_only_when_typed() {
        let mut input = inputs();
        input.database_type = DatabaseType::Redis;
        input.auth_mode = AuthMode::EntraBrowser;
        input.server = "contoso.redis.cache.windows.net".to_string();
        input.username = " cache-reader ".to_string();
        let profile = assemble_profile(input).unwrap();
        assert_eq!(profile.redis_entra_user.as_deref(), Some("cache-reader"));
        let mut blank = inputs();
        blank.database_type = DatabaseType::Redis;
        blank.auth_mode = AuthMode::EntraBrowser;
        blank.username = "  ".to_string();
        assert_eq!(assemble_profile(blank).unwrap().redis_entra_user, None);
        let mut sql = inputs();
        sql.database_type = DatabaseType::SqlServer;
        sql.auth_mode = AuthMode::EntraBrowser;
        sql.username = "me@corp.com".to_string();
        assert_eq!(assemble_profile(sql).unwrap().redis_entra_user, None);
    }

    #[test]
    fn ssh_keepalive_field_blank_is_default_zero_is_off_and_text_is_refused() {
        let blank = parse_ssh_keepalive("  ", Some(5)).unwrap();
        assert_eq!(blank.interval_seconds, None);
        assert_eq!(blank.max_missed, Some(5), "the hidden limit is kept");
        assert_eq!(
            parse_ssh_keepalive("0", None).unwrap().interval_seconds,
            Some(0)
        );
        assert_eq!(
            parse_ssh_keepalive("15", None).unwrap().interval_seconds,
            Some(15)
        );
        assert!(parse_ssh_keepalive("soon", None).is_err());
        assert!(parse_ssh_keepalive("-1", None).is_err());
    }

    /// A direct Redis test writes each stage to the diagnostic log and the
    /// failing stage heads the error: a closed port stops at TCP after
    /// Resolve and Tunnel / proxy are logged.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn redis_test_connection_logs_each_stage_and_heads_the_error_with_the_failure() {
        crate::ensure_test_tokio_runtime();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut profile = ConnectionProfile::sql_login(
            "Stage log",
            "127.0.0.1",
            "",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = DatabaseType::Redis;
        profile.port = port;
        profile.encrypt = false;
        profile.timeout_seconds = 3;
        let log =
            crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Verbose);
        let services = sqlly_services::Services::new();
        let result = crate::tokio_handle().block_on(run_connection_test(&services, &profile, &log));
        let error = result.unwrap_err().to_string();
        assert!(error.contains("TCP failed"), "{error}");
        let messages: Vec<String> = log.entries().into_iter().map(|e| e.message).collect();
        assert!(messages[0].contains("Resolve ok"), "{messages:#?}");
        assert!(
            messages[1].contains("Tunnel / proxy skipped"),
            "{messages:#?}"
        );
        assert!(messages[2].contains("TCP FAILED"), "{messages:#?}");
        assert!(messages.iter().all(|m| m.starts_with("\"Stage log\"")));
        let entries = log.entries();
        assert_eq!(
            entries[2].level,
            crate::log_console::DiagnosticEntryLevel::Error
        );
    }

    fn inputs() -> ProfileInputs {
        ProfileInputs {
            database_type: DatabaseType::SqlServer,
            engine_auto_detect: false,
            name: "MyConn".to_string(),
            server: "localhost".to_string(),
            port: "1433".to_string(),
            database: String::new(),
            client: String::new(),
            project: String::new(),
            timeout: "30".to_string(),
            startup_sql: String::new(),
            notes: String::new(),
            display_time_zone: None,
            redis_key_delimiter: String::new(),
            redis_cluster_mode: sqlly_core::redis_cluster::RedisClusterMode::Auto,
            redis_node_map: String::new(),
            clickhouse_native_port: String::new(),
            username: "sa".to_string(),
            aws_region: String::new(),
            aws_profile: String::new(),
            password: "pw".to_string(),
            sql_account: None,
            auth_mode: AuthMode::SqlLogin,
            entra_account_id: None,
            environment: ConnectionEnvironment::Local,
            encrypt: true,
            trust: false,
            tls_mode: None,
            tls_ca_file: String::new(),
            tls_client_cert_file: String::new(),
            tls_client_key_file: String::new(),
            tls_trust_os_roots: false,
            tls_direct: false,
            block_select: false,
            rollback: false,
            confirm_changes: false,
            allow_unbounded: false,
            production_override: None,
            editing_id: None,
            persistable: false,
            route: ConnectionRoute::Direct,
            tunnel_profile: None,
            transport: TransportChain::default(),
        }
    }

    fn relay_device(device_id: &str, pair_id: Option<&str>) -> RelayDeviceChoice {
        RelayDeviceChoice {
            device_id: device_id.to_string(),
            name: format!("{device_id} bridge"),
            pair_id: pair_id.map(|p| p.to_string()),
        }
    }

    #[test]
    fn notes_are_saved_trimmed_and_blank_notes_are_dropped() {
        let profile = assemble_profile(ProfileInputs {
            notes: "  Owned by billing.\nMaintenance on Sundays.  \n".to_string(),
            ..inputs()
        })
        .unwrap();
        assert_eq!(
            profile.notes.as_deref(),
            Some("Owned by billing.\nMaintenance on Sundays.")
        );
        let profile = assemble_profile(ProfileInputs {
            notes: " \n\t ".to_string(),
            ..inputs()
        })
        .unwrap();
        assert_eq!(profile.notes, None);
    }

    #[test]
    fn default_route_is_direct() {
        // A freshly assembled profile routes directly — the back-compatible
        // default no user has to opt into.
        let profile = assemble_profile(inputs()).unwrap();
        assert_eq!(profile.route, ConnectionRoute::Direct);
    }

    #[test]
    fn via_relay_off_builds_direct_route() {
        let devices = [relay_device("dev-1", Some("pair-1"))];
        let route = build_relay_route(false, Some("dev-1"), &devices, None).unwrap();
        assert_eq!(route, ConnectionRoute::Direct);
    }

    #[test]
    fn selecting_via_relay_persists_a_via_relay_route() {
        let devices = [relay_device("dev-1", Some("pair-1"))];
        let route = build_relay_route(
            true,
            Some("dev-1"),
            &devices,
            Some("relay-use2.sqlly.app".into()),
        )
        .unwrap();
        assert_eq!(
            route,
            ConnectionRoute::ViaRelay {
                relay_device_id: "dev-1".to_string(),
                pair_id: "pair-1".to_string(),
                relay_hint: Some("relay-use2.sqlly.app".to_string()),
            }
        );

        // …and the assembled profile carries it (secret-free, ids only).
        let mut i = inputs();
        i.route = route.clone();
        let profile = assemble_profile(i).unwrap();
        assert_eq!(profile.route, route);
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.to_lowercase().contains("token"));
        assert!(!json.to_lowercase().contains("secret_key"));
    }

    #[test]
    fn via_relay_route_round_trips_through_edit() {
        let devices = [relay_device("dev-9", Some("pair-9"))];
        let route = build_relay_route(true, Some("dev-9"), &devices, None).unwrap();
        let mut i = inputs();
        i.route = route.clone();
        let saved = assemble_profile(i).unwrap();

        // Reopen: the saved route feeds a fresh assemble unchanged.
        let mut round = inputs();
        round.editing_id = Some(saved.id.0);
        round.route = saved.route.clone();
        let reloaded = assemble_profile(round).unwrap();
        assert_eq!(reloaded.route, route);
    }

    #[test]
    fn via_relay_without_selection_is_an_error() {
        let devices = [relay_device("dev-1", Some("pair-1"))];
        let err = build_relay_route(true, None, &devices, None).unwrap_err();
        assert_eq!(err, "Select a relay device to route through.");
    }

    #[test]
    fn via_relay_unauthorized_device_is_an_error() {
        // A device with no authorized pair cannot carry a route.
        let devices = [relay_device("dev-1", None)];
        let err = build_relay_route(true, Some("dev-1"), &devices, None).unwrap_err();
        assert!(err.contains("not authorized"), "got: {err}");
    }

    fn connector(
        device_id: &str,
        name: Option<&str>,
        hostname: Option<&str>,
    ) -> crate::store::RelayConnector {
        use chrono::Utc;
        crate::store::RelayConnector {
            device_id: device_id.into(),
            name: name.map(Into::into),
            platform: "macos".into(),
            noise_public_key: String::new(),
            noise_public_key_fingerprint: String::new(),
            hostname: hostname.map(Into::into),
            registered_at: Utc::now(),
            last_seen_at: Utc::now(),
        }
    }

    #[test]
    fn merge_relay_devices_resolves_active_pair_ids() {
        use chrono::Utc;
        let devices = vec![
            connector("dev-1", Some("Office bridge"), None),
            connector("dev-2", Some("Unpaired bridge"), None),
        ];
        let pairs = vec![
            crate::store::RelayPair {
                id: "pair-1".into(),
                control_device_id: "ctrl".into(),
                connector_device_id: "dev-1".into(),
                created_at: Utc::now(),
                active: true,
                control_noise_public_key: None,
                connector_noise_public_key: None,
            },
            // An inactive pair for dev-2 must not resolve.
            crate::store::RelayPair {
                id: "pair-stale".into(),
                control_device_id: "ctrl".into(),
                connector_device_id: "dev-2".into(),
                created_at: Utc::now(),
                active: false,
                control_noise_public_key: None,
                connector_noise_public_key: None,
            },
        ];
        let merged = merge_relay_devices(&devices, &[], &pairs, Some("ctrl"));
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].device_id, "dev-1");
        assert_eq!(merged[0].name, "Office bridge");
        assert_eq!(merged[0].pair_id.as_deref(), Some("pair-1"));
        assert_eq!(merged[1].device_id, "dev-2");
        assert_eq!(merged[1].pair_id, None);
    }

    /// A bridge with no name override lists under its hostname (v0.30 servers
    /// return `name: null` for those), and a nameless hostless one falls back
    /// to its device id — never a blank row.
    #[test]
    fn merge_relay_devices_falls_back_to_hostname_then_id() {
        let devices = vec![
            connector("dev-1", None, Some("db-host-01")),
            connector("dev-2", None, None),
        ];
        let merged = merge_relay_devices(&devices, &[], &[], Some("ctrl"));
        assert_eq!(merged[0].name, "db-host-01");
        assert_eq!(merged[1].name, "dev-2");
    }

    /// Bridges SHARED with the account appear in the picker labelled
    /// "(shared)", resolve their own pairs, and dedupe against owned devices.
    #[test]
    fn merge_relay_devices_appends_shared_bridges() {
        use chrono::Utc;
        let devices = vec![connector("dev-own", Some("Mine"), None)];
        let shared = vec![
            crate::store::SharedRelayConnector {
                device_id: "dev-shared".into(),
                name: "Team bridge".into(),
                noise_public_key_fingerprint: "aa:bb".into(),
            },
            // A share of a bridge the account also owns must not duplicate.
            crate::store::SharedRelayConnector {
                device_id: "dev-own".into(),
                name: "Mine".into(),
                noise_public_key_fingerprint: String::new(),
            },
        ];
        let pairs = vec![crate::store::RelayPair {
            id: "pair-shared".into(),
            control_device_id: "ctrl".into(),
            connector_device_id: "dev-shared".into(),
            created_at: Utc::now(),
            active: true,
            control_noise_public_key: None,
            connector_noise_public_key: None,
        }];

        let merged = merge_relay_devices(&devices, &shared, &pairs, Some("ctrl"));
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name, "Mine");
        assert_eq!(merged[1].device_id, "dev-shared");
        assert_eq!(merged[1].name, "Team bridge (shared)");
        assert_eq!(
            merged[1].pair_id.as_deref(),
            Some("pair-shared"),
            "a member's own pair on a shared bridge must resolve"
        );
    }

    /// A pair authorizing a DIFFERENT app device must not make a connector look
    /// authorized on this device: the relay's route grants are per control
    /// device, so routing with another device's pair gets denied. This is the
    /// exact cross-machine case — the bridge was paired from machine A, and the
    /// app on machine B must be offered "Authorize", not a borrowed pair.
    #[test]
    fn merge_relay_devices_ignores_other_devices_pairs() {
        use chrono::Utc;
        let devices = vec![connector("dev-1", Some("Office bridge"), None)];
        let pairs = vec![crate::store::RelayPair {
            id: "pair-other".into(),
            control_device_id: "someone-elses-laptop".into(),
            connector_device_id: "dev-1".into(),
            created_at: Utc::now(),
            active: true,
            control_noise_public_key: None,
            connector_noise_public_key: None,
        }];

        let merged = merge_relay_devices(&devices, &[], &pairs, Some("this-machine"));
        assert_eq!(
            merged[0].pair_id, None,
            "another device's pair must not resolve"
        );

        // With no registered device id at all, nothing resolves either.
        let merged = merge_relay_devices(&devices, &[], &pairs, None);
        assert_eq!(merged[0].pair_id, None);
    }

    #[test]
    fn saved_sql_account_persists_only_keyring_reference() {
        let account_id = Uuid::new_v4();
        let mut input = inputs();
        input.password = "must-not-persist".to_string();
        input.sql_account = Some(account_id);

        let profile = assemble_profile(input).unwrap();
        let AuthenticationType::SqlLogin { secret_ref, .. } = profile.authentication else {
            panic!("expected SQL login");
        };
        assert_eq!(secret_ref.provider, SecretProvider::MacOsKeychain);
        assert_eq!(secret_ref.key, account_id.to_string());
    }

    #[test]
    fn account_password_mask_is_twenty_bullets_and_never_persists() {
        let mask = password_mask();
        assert_eq!(mask.chars().count(), 20);
        assert!(mask.chars().all(|c| c == '\u{2022}'));

        // The mask sits in the password field while an account is selected, so
        // the one thing it must never do is reach the saved profile.
        let account_id = Uuid::new_v4();
        let mut input = inputs();
        input.password = mask.clone();
        input.sql_account = Some(account_id);
        let profile = assemble_profile(input).unwrap();
        let AuthenticationType::SqlLogin { secret_ref, .. } = profile.authentication else {
            panic!("expected SQL login");
        };
        assert_eq!(secret_ref.key, account_id.to_string());
        assert_ne!(secret_ref.key, mask);
    }

    #[test]
    fn sql_login_requires_username() {
        let mut i = inputs();
        i.username = "   ".to_string();
        let err = assemble_profile(i).unwrap_err();
        assert_eq!(err, "Username is required for SQL Login");
    }

    #[test]
    fn sql_login_builds_sql_login_authentication() {
        let profile = assemble_profile(inputs()).unwrap();
        match profile.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                assert_eq!(username, "sa");
                assert_eq!(secret_ref.provider, SecretProvider::InMemory);
                assert_eq!(secret_ref.key, "pw");
            }
            other => panic!("expected SqlLogin, got {other:?}"),
        }
    }

    #[test]
    fn persistable_sql_login_mints_keychain_reference_without_cleartext_password() {
        let mut input = inputs();
        input.persistable = true;
        input.sql_account = None;
        input.password = "must-not-persist".to_string();

        let profile = assemble_profile(input).unwrap();
        let AuthenticationType::SqlLogin { secret_ref, .. } = profile.authentication else {
            panic!("expected SQL login");
        };
        assert_eq!(secret_ref.provider, SecretProvider::MacOsKeychain);
        assert_ne!(secret_ref.key, "must-not-persist");
        assert!(Uuid::parse_str(&secret_ref.key).is_ok());
    }

    #[test]
    fn blank_sql_login_password_saves_as_passwordless_without_a_keyring_entry() {
        for (persistable, sql_account) in [
            (true, None),
            (true, Some(Uuid::new_v4())),
            (false, None),
            (false, Some(Uuid::new_v4())),
        ] {
            let mut input = inputs();
            input.persistable = persistable;
            input.sql_account = sql_account;
            input.password = String::new();

            let profile = assemble_profile(input).unwrap();
            let AuthenticationType::SqlLogin { secret_ref, .. } = &profile.authentication else {
                panic!("expected SQL login");
            };
            assert_eq!(secret_ref.provider, SecretProvider::InMemory);
            assert_eq!(secret_ref.key, "");
            // Connecting resolves it to an empty password, not an error.
            let hydrated = crate::sql_credentials::hydrate_profile(profile).unwrap();
            let secret_ref = hydrated.authentication.secret_ref().unwrap();
            assert_eq!(
                sqlly_core::secret_resolver::resolve_secret(secret_ref).unwrap(),
                ""
            );
        }
    }

    #[test]
    fn untouched_mask_keeps_the_saved_keyring_password() {
        let account = Uuid::new_v4();
        let mut input = inputs();
        input.persistable = true;
        input.sql_account = Some(account);
        input.password = password_mask();

        let profile = assemble_profile(input).unwrap();
        let secret_ref = profile.authentication.secret_ref().unwrap();
        assert_eq!(secret_ref.provider, SecretProvider::MacOsKeychain);
        assert_eq!(secret_ref.key, account.to_string());
    }

    #[test]
    fn integrated_does_not_require_username_and_sets_integrated_auth() {
        let mut i = inputs();
        i.auth_mode = AuthMode::Integrated;
        i.username = String::new();
        let profile = assemble_profile(i).unwrap();
        assert!(matches!(
            profile.authentication,
            AuthenticationType::Integrated
        ));
    }

    #[test]
    fn empty_name_or_server_is_rejected() {
        let mut i = inputs();
        i.name = "  ".to_string();
        assert_eq!(assemble_profile(i).unwrap_err(), "Name is required");

        let mut i = inputs();
        i.server = String::new();
        assert_eq!(assemble_profile(i).unwrap_err(), "Server is required");
    }

    #[test]
    fn clickhouse_native_port_is_parsed_validated_and_engine_scoped() {
        let clickhouse = || {
            let mut i = inputs();
            i.database_type = DatabaseType::ClickHouse;
            i.port = "8123".to_string();
            i
        };
        let mut i = clickhouse();
        i.clickhouse_native_port = " 19000 ".to_string();
        assert_eq!(
            assemble_profile(i).unwrap().clickhouse_native_port,
            Some(19000)
        );
        assert_eq!(
            assemble_profile(clickhouse())
                .unwrap()
                .clickhouse_native_port,
            None,
            "blank means the conventional mapping"
        );
        for bad in ["0", "70000", "nine"] {
            let mut i = clickhouse();
            i.clickhouse_native_port = bad.to_string();
            assert_eq!(
                assemble_profile(i).unwrap_err(),
                "Native port must be a number from 1 to 65535",
                "{bad}"
            );
        }
        // Another engine never keeps a stale native port.
        let mut i = inputs();
        i.clickhouse_native_port = "9000".to_string();
        assert_eq!(assemble_profile(i).unwrap().clickhouse_native_port, None);
    }

    #[test]
    fn invalid_port_is_rejected_and_zero_timeout_defaults_to_30() {
        let mut i = inputs();
        i.port = "notaport".to_string();
        assert_eq!(
            assemble_profile(i).unwrap_err(),
            "Port must be a valid number"
        );

        let mut i = inputs();
        i.timeout = "0".to_string();
        assert_eq!(assemble_profile(i).unwrap().timeout_seconds, 30);
    }

    #[test]
    fn tls_required_engines_cannot_save_plaintext() {
        let mut input = inputs();
        input.database_type = DatabaseType::Neon;
        input.encrypt = false;
        assert_eq!(
            assemble_profile(input).unwrap_err(),
            "This engine requires an encrypted connection"
        );
        let mut ok = inputs();
        ok.database_type = DatabaseType::Neon;
        ok.encrypt = true;
        assert!(assemble_profile(ok).is_ok());
    }

    #[test]
    fn selected_engine_is_persisted_in_the_profile() {
        let mut input = inputs();
        input.database_type = DatabaseType::Postgres;
        input.port = "5432".to_string();
        let profile = assemble_profile(input).unwrap();
        assert_eq!(profile.database_type, DatabaseType::Postgres);
        assert_eq!(profile.port, 5432);
    }

    #[test]
    fn sqlite_uses_the_server_field_as_a_file_path_without_network_credentials() {
        let mut input = inputs();
        input.database_type = DatabaseType::Sqlite;
        input.server = "/tmp/sqlly picker test.sqlite".to_string();
        input.port.clear();
        input.username.clear();
        let profile = assemble_profile(input).unwrap();
        assert_eq!(profile.database_type, DatabaseType::Sqlite);
        assert_eq!(profile.server, "/tmp/sqlly picker test.sqlite");
        assert_eq!(profile.port, 0);
    }

    #[test]
    fn unsupported_engine_authentication_pair_is_rejected() {
        let mut input = inputs();
        input.database_type = DatabaseType::Postgres;
        input.port = "5432".to_string();
        input.auth_mode = AuthMode::Integrated;
        assert_eq!(
            assemble_profile(input).unwrap_err(),
            "Windows authentication is not supported for PostgreSQL"
        );
    }

    #[test]
    fn editing_id_is_preserved() {
        let id = Uuid::new_v4();
        let mut i = inputs();
        i.editing_id = Some(id);
        assert_eq!(assemble_profile(i).unwrap().id.0, id);
    }

    #[test]
    fn entra_browser_without_account_binding_leaves_field_none() {
        let mut i = inputs();
        i.auth_mode = AuthMode::EntraBrowser;
        i.username = String::new();
        i.password = String::new();
        i.entra_account_id = None;
        let profile = assemble_profile(i).unwrap();
        assert!(matches!(
            profile.authentication,
            AuthenticationType::EntraBrowser
        ));
        assert!(
            profile.entra_account.is_none(),
            "no account binding chosen → profile.entra_account should be None"
        );
    }

    #[test]
    fn aws_rds_iam_save_round_trip_includes_username_region_and_profile() {
        let mut input = inputs();
        input.database_type = DatabaseType::Postgres;
        input.port = "5432".to_string();
        input.auth_mode = AuthMode::AwsRdsIam;
        input.username = "jane".to_string();
        input.aws_region = "us-west-2".to_string();
        input.aws_profile = "dev".to_string();
        input.encrypt = false;
        let profile = assemble_profile(input).unwrap();
        let AuthenticationType::AwsRdsIam {
            username,
            region,
            profile: aws_profile,
        } = profile.authentication
        else {
            panic!("expected AwsRdsIam");
        };
        assert_eq!(username, "jane");
        assert_eq!(region, "us-west-2");
        assert_eq!(aws_profile.as_deref(), Some("dev"));
        assert!(profile.encrypt);
    }

    #[test]
    fn gcp_cloud_sql_iam_save_includes_username() {
        let mut input = inputs();
        input.database_type = DatabaseType::Postgres;
        input.port = "5432".to_string();
        input.auth_mode = AuthMode::GcpCloudSqlIam;
        input.username = "alice@example.com".to_string();
        input.encrypt = false;
        let profile = assemble_profile(input).unwrap();
        let AuthenticationType::GcpCloudSqlIam { username } = profile.authentication else {
            panic!("expected GcpCloudSqlIam");
        };
        assert_eq!(username, "alice@example.com");
        assert!(profile.encrypt);
    }

    #[test]
    fn iam_auth_forces_encrypt_even_when_checkbox_was_off() {
        let mut input = inputs();
        input.database_type = DatabaseType::MariaDb;
        input.port = "3306".to_string();
        input.auth_mode = AuthMode::AwsRdsIam;
        input.username = "jane".to_string();
        input.aws_region = "eu-west-1".to_string();
        input.encrypt = false;
        assert!(assemble_profile(input).unwrap().encrypt);
    }

    #[test]
    fn sole_entra_account_is_automatically_bindable() {
        let account = EntraAccountSummary {
            profile_id: "home.tenant".to_string(),
            tenant_id: "tenant".to_string(),
            username: Some("alex@contoso.com".to_string()),
            display_name: Some("Alex".to_string()),
        };
        let binding = sole_entra_binding(std::slice::from_ref(&account)).unwrap();
        assert_eq!(binding.account_id, "home.tenant");
        assert_eq!(binding.tenant_id, "tenant");
        assert_eq!(binding.username.as_deref(), Some("alex@contoso.com"));
        assert!(sole_entra_binding(&[]).is_none());
        assert!(sole_entra_binding(&[account.clone(), account]).is_none());
    }

    #[test]
    fn entra_browser_with_account_binding_sets_profile_field() {
        let mut i = inputs();
        i.auth_mode = AuthMode::EntraBrowser;
        i.entra_account_id = Some(EntraAccountBinding {
            account_id: "home-x.tenant-x".into(),
            tenant_id: "tenant-x".into(),
            username: Some("alex@contoso.com".into()),
        });
        let profile = assemble_profile(i).unwrap();
        let binding = profile
            .entra_account
            .expect("entra_account binding should be set");
        assert_eq!(binding.account_id, "home-x.tenant-x");
        assert_eq!(binding.tenant_id, "tenant-x");
        assert_eq!(binding.username.as_deref(), Some("alex@contoso.com"));
    }

    #[test]
    fn editing_existing_entra_browser_profile_round_trips_binding() {
        // Round-trip: build_profile → assemble_profile should round-trip
        // the EntraAccountBinding the user picked so reopening the editor
        // shows the same selection.
        let mut i = inputs();
        i.auth_mode = AuthMode::EntraBrowser;
        let binding = EntraAccountBinding {
            account_id: "home-y.tenant-y".into(),
            tenant_id: "tenant-y".into(),
            username: Some("bob@contoso.com".into()),
        };
        i.entra_account_id = Some(binding.clone());
        let profile = assemble_profile(i).unwrap();
        let mut round = inputs();
        round.editing_id = Some(profile.id.0);
        round.entra_account_id = profile.entra_account;
        let reloaded = assemble_profile(round).unwrap();
        assert_eq!(reloaded.entra_account, Some(binding));
    }

    #[test]
    fn environment_safety_defaults_match_swift() {
        assert_eq!(
            environment_safety_defaults(ConnectionEnvironment::Production),
            (true, true)
        );
        assert_eq!(
            environment_safety_defaults(ConnectionEnvironment::Staging),
            (false, true)
        );
        assert_eq!(
            environment_safety_defaults(ConnectionEnvironment::Test),
            (false, false)
        );
        assert_eq!(
            environment_safety_defaults(ConnectionEnvironment::Local),
            (false, false)
        );
        assert_eq!(
            environment_safety_defaults(ConnectionEnvironment::Unset),
            (false, false)
        );
    }

    #[test]
    fn server_is_local_recognises_local_hostnames() {
        assert!(server_is_local("local"));
        assert!(server_is_local("localhost"));
        assert!(server_is_local("127.0.0.1"));
        assert!(server_is_local("  LocalHost  "));
        assert!(!server_is_local("prod-sql.example.com"));
        assert!(!server_is_local(""));
    }

    #[test]
    fn server_info_rows_skips_unknowns_and_formats_flags() {
        let info = ServerInfo {
            product: "SQLite".into(),
            version: "3.45.0".into(),
            server_charset: Some("UTF-8".into()),
            server_collation: Some("BINARY".into()),
            identifier_case: IdentifierCase::Insensitive,
            current_user: None,
            database: Some("main".into()),
            tls_in_use: None,
            latency_ms: Some(12),
        };
        let rows = server_info_rows(&info);
        let labels: Vec<_> = rows.iter().map(|(label, _)| label.as_str()).collect();
        assert!(labels.contains(&"Product"));
        assert!(labels.contains(&"Identifiers"));
        assert!(!labels.contains(&"User"));
        assert!(!labels.contains(&"TLS"));
        assert_eq!(
            rows.iter()
                .find(|(label, _)| *label == "Identifiers")
                .map(|(_, value)| value.as_str()),
            Some("case-insensitive")
        );
        assert_eq!(
            rows.iter()
                .find(|(label, _)| *label == "Latency")
                .map(|(_, value)| value.as_str()),
            Some("12 ms")
        );
    }

    #[test]
    fn build_transport_chain_proxy_only() {
        let chain = build_transport_chain(
            true,
            ProxyKind::Socks5,
            "proxy.corp",
            "1080",
            "",
            String::new(),
            None,
            false,
            "",
            "22",
            "",
            SshAuthMode::Agent,
            "",
            String::new(),
            None,
            false,
            "",
            "22",
            "",
            false,
        )
        .unwrap();
        assert!(chain.proxy.is_some());
        assert!(chain.ssh.is_none());
    }

    #[test]
    fn build_transport_chain_proxy_then_ssh() {
        let chain = build_transport_chain(
            true,
            ProxyKind::HttpConnect,
            "proxy",
            "8080",
            "",
            String::new(),
            None,
            true,
            "bastion",
            "22",
            "jump",
            SshAuthMode::Agent,
            "",
            String::new(),
            None,
            false,
            "",
            "22",
            "",
            false,
        )
        .unwrap();
        assert!(chain.proxy.is_some());
        assert!(chain.ssh.is_some());
    }

    #[test]
    fn build_transport_chain_rejects_proxy_password_without_username() {
        let err = build_transport_chain(
            true,
            ProxyKind::Socks5,
            "proxy",
            "1080",
            "",
            "secret".to_string(),
            None,
            false,
            "",
            "22",
            "",
            SshAuthMode::Agent,
            "",
            String::new(),
            None,
            false,
            "",
            "22",
            "",
            false,
        )
        .unwrap_err();
        assert!(err.contains("username"));
    }

    #[test]
    fn build_ssm_port_forward_trims_and_drops_blank_optionals() {
        assert!(
            build_ssm_port_forward(false, "i-1", "", "", "")
                .unwrap()
                .is_none()
        );
        let ssm = build_ssm_port_forward(true, " i-0abc ", " eu-west-1 ", " ", "")
            .unwrap()
            .expect("ssm");
        assert_eq!(ssm.target, "i-0abc");
        assert_eq!(ssm.region.as_deref(), Some("eu-west-1"));
        assert_eq!(ssm.profile, None);
        assert_eq!(ssm.remote_host, None);
        let chain = TransportChain::ssm_only(ssm);
        assert!(chain.validate().is_empty());
    }

    #[test]
    fn build_ssm_port_forward_requires_a_target() {
        assert!(build_ssm_port_forward(true, "  ", "", "", "").is_err());
        assert!(build_ssm_port_forward(true, "i-1", "", "", "db;x").is_err());
    }

    #[test]
    fn ssm_tool_status_names_the_missing_program() {
        assert_eq!(ssm_tool_status(false, false), Some(SsmMissingTool::AwsCli));
        assert_eq!(ssm_tool_status(true, false), Some(SsmMissingTool::Plugin));
        assert_eq!(ssm_tool_status(true, true), None);
    }

    #[test]
    fn build_kube_port_forward_none_when_disabled() {
        let kube =
            build_kube_port_forward(false, "", "", "default", KubeTargetKind::Pod, "db", "5432")
                .unwrap();
        assert!(kube.is_none());
    }

    #[test]
    fn build_kube_port_forward_validates_port_and_target() {
        let err = build_kube_port_forward(true, "", "", "default", KubeTargetKind::Pod, "", "0")
            .unwrap_err();
        assert!(err.contains("target"));
        assert!(err.contains("port"));
    }

    #[test]
    fn kube_and_ssh_are_mutually_exclusive() {
        let chain = build_transport_chain(
            false,
            ProxyKind::Socks5,
            "",
            "1080",
            "",
            String::new(),
            None,
            true,
            "bastion",
            "22",
            "jump",
            SshAuthMode::Agent,
            "",
            String::new(),
            None,
            false,
            "",
            "22",
            "",
            false,
        )
        .unwrap();
        assert!(chain.ssh.is_some());
        let kube = build_kube_port_forward(
            true,
            "",
            "ctx",
            "default",
            KubeTargetKind::Pod,
            "db",
            "5432",
        )
        .unwrap()
        .expect("kube");
        let errors = TransportChain {
            proxy: None,
            ssh: chain.ssh,
            kube: Some(kube),
            ssm: None,
        }
        .validate();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("Kubernetes port-forward cannot be combined"))
        );
    }

    #[test]
    fn assemble_profile_writes_tls_mode_and_syncs_legacy_booleans() {
        let mut input = inputs();
        input.database_type = DatabaseType::Postgres;
        input.port = "5432".to_string();
        input.tls_mode = Some(TlsMode::VerifyCa);
        input.tls_ca_file = "/etc/ssl/company-ca.pem".to_string();
        let profile = assemble_profile(input).unwrap();
        assert_eq!(profile.tls_mode, TlsMode::VerifyCa);
        // VerifyCa degrades to full verification for legacy readers (never
        // looser).
        assert!(profile.encrypt);
        assert!(!profile.trust_server_certificate);
        assert_eq!(
            profile.tls_ca_file.as_deref(),
            Some(std::path::Path::new("/etc/ssl/company-ca.pem"))
        );

        let mut input = inputs();
        input.database_type = DatabaseType::MySql;
        input.port = "3306".to_string();
        input.tls_mode = Some(TlsMode::Require);
        let profile = assemble_profile(input).unwrap();
        assert_eq!(profile.tls_mode, TlsMode::Require);
        assert!(profile.encrypt);
        assert!(profile.trust_server_certificate);
    }

    #[test]
    fn assemble_profile_keeps_direct_tls_for_postgres_only() {
        let mut input = inputs();
        input.database_type = DatabaseType::Postgres;
        input.port = "5432".to_string();
        input.tls_mode = Some(TlsMode::VerifyFull);
        input.tls_direct = true;
        assert!(assemble_profile(input).unwrap().tls_direct);

        // Ticked on PostgreSQL, then the engine switched: not carried over.
        let mut input = inputs();
        input.database_type = DatabaseType::MySql;
        input.port = "3306".to_string();
        input.tls_mode = Some(TlsMode::VerifyFull);
        input.tls_direct = true;
        assert!(!assemble_profile(input).unwrap().tls_direct);
    }

    #[test]
    fn redis_cluster_editor_fields_assemble_into_the_profile() {
        let mut input = inputs();
        input.database_type = DatabaseType::Redis;
        input.username = "default".to_string();
        input.port = "7000".to_string();
        input.server = "10.0.0.1:7000, 10.0.0.2:7001".to_string();
        input.redis_cluster_mode = sqlly_core::redis_cluster::RedisClusterMode::Cluster;
        input.redis_node_map = "10.0.0.2:7001 => 127.0.0.1:17001".to_string();
        let profile = assemble_profile(input).unwrap();
        let cluster = profile
            .redis_cluster
            .clone()
            .expect("cluster settings kept");
        assert_eq!(
            cluster.mode,
            sqlly_core::redis_cluster::RedisClusterMode::Cluster
        );
        assert_eq!(cluster.node_map.len(), 1);
        assert_eq!(
            sqlly_core::redis_cluster::seed_nodes(&profile).len(),
            2,
            "both seed hosts survive"
        );
        let mut defaults = inputs();
        defaults.database_type = DatabaseType::Redis;
        defaults.username = "default".to_string();
        assert!(
            assemble_profile(defaults).unwrap().redis_cluster.is_none(),
            "Auto with no map stores nothing"
        );
        let mut bad = inputs();
        bad.database_type = DatabaseType::Redis;
        bad.username = "default".to_string();
        bad.redis_node_map = "nonsense".to_string();
        assert!(assemble_profile(bad).unwrap_err().contains("node map"));
        assert_eq!(
            validate_server_text(DatabaseType::Postgres, "h1,h2"),
            validate_host("h1,h2")
        );
        assert_eq!(
            validate_server_text(DatabaseType::Redis, "h1:7000, h2"),
            HostValidationResult::Valid
        );
    }

    #[test]
    fn assemble_profile_rejects_client_cert_without_key() {
        let mut input = inputs();
        input.database_type = DatabaseType::Postgres;
        input.port = "5432".to_string();
        input.tls_mode = Some(TlsMode::VerifyFull);
        input.tls_client_cert_file = "/certs/client.pem".to_string();
        let err = assemble_profile(input).unwrap_err();
        assert!(err.contains("both"), "unexpected error: {err}");
    }

    fn make_editor(
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::Entity<ConnectionEditor>,
        &mut gpui::VisualTestContext,
        tempfile::TempDir,
    ) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let registry = EntraAccountRegistryStore::from_path(temp.path().join("accounts.json"))
            .expect("registry opens on a temp path");
        let (view, cx) = cx.add_window_view(|window, cx| {
            ConnectionEditor::new(
                ServiceRouter::new(sqlly_services::Services::new()),
                crate::log_console::DiagnosticLog::new(
                    crate::log_console::DiagnosticLogLevel::Error,
                ),
                registry,
                window,
                cx,
            )
        });
        (view, cx, temp)
    }

    #[test]
    fn duplicate_notice_names_every_secret_that_did_not_come_over() {
        assert_eq!(duplicate_notice(&[]), None);
        let one = duplicate_notice(&["password".into()]).unwrap();
        assert!(one.contains("without a saved password:"), "{one}");
        let two = duplicate_notice(&["password".into(), "proxy password".into()]).unwrap();
        assert!(
            two.contains("without a saved password and proxy password:"),
            "{two}"
        );
        let three = duplicate_notice(&["a".into(), "b".into(), "c".into()]).unwrap();
        assert!(three.contains("without a saved a, b and c:"), "{three}");
    }

    /// Build an editor holding a duplicate of `original` the way the
    /// explorer's "Duplicate Connection…" does, with the keyring mocked.
    fn make_duplicate_editor(
        original: ConnectionProfile,
        existing_names: Vec<String>,
        cx: &mut gpui::TestAppContext,
    ) -> (
        gpui::Entity<ConnectionEditor>,
        &mut gpui::VisualTestContext,
        tempfile::TempDir,
    ) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let registry = EntraAccountRegistryStore::from_path(temp.path().join("accounts.json"))
            .expect("registry opens on a temp path");
        let (view, cx) = cx.add_window_view(|window, cx| {
            ConnectionEditor::from_duplicate(
                ServiceRouter::new(sqlly_services::Services::new()),
                crate::log_console::DiagnosticLog::new(
                    crate::log_console::DiagnosticLogLevel::Error,
                ),
                registry,
                original,
                existing_names,
                window,
                cx,
            )
        });
        (view, cx, temp)
    }

    fn keychain_login_profile(name: &str, account: Uuid) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            name,
            "db.example.com",
            "reporter",
            SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: account.to_string(),
            },
        );
        profile.port = 5432;
        profile.database = Some("analytics".into());
        profile.database_type = DatabaseType::Postgres;
        profile.environment = ConnectionEnvironment::Staging;
        profile.client = Some("Acme".into());
        profile.timeout_seconds = 45;
        profile.startup_sql = Some("SET search_path TO reporting".into());
        profile.notes = Some("Reporting replica.\nAsk #data before heavy queries.".into());
        profile.block_all_but_select = true;
        profile
    }

    #[gpui::test]
    fn from_duplicate_prefills_an_unsaved_copy_with_the_original_password(
        cx: &mut gpui::TestAppContext,
    ) {
        keyring::mock::install();
        let account = Uuid::new_v4();
        crate::sql_credentials::store_password(account, "hunter2").unwrap();
        let original = keychain_login_profile("Warehouse", account);
        let original_id = original.id.0;

        let (view, cx, _temp) = make_duplicate_editor(
            original,
            vec!["Warehouse".into(), "Warehouse (copy)".into()],
            cx,
        );
        view.update(cx, |editor, cx| {
            assert_eq!(editor.editing_id, None, "a duplicate is a new profile");
            assert_eq!(editor.duplicate_of, Some(original_id));
            assert_eq!(
                editor.name, "Warehouse (copy 2)",
                "bumps past the taken name"
            );
            assert_eq!(editor.server, "db.example.com");
            assert_eq!(editor.port, "5432");
            assert_eq!(editor.database, "analytics");
            assert_eq!(editor.database_type, DatabaseType::Postgres);
            assert_eq!(editor.environment, ConnectionEnvironment::Staging);
            assert_eq!(editor.client, "Acme");
            assert_eq!(editor.timeout, "45");
            assert_eq!(editor.startup_sql, "SET search_path TO reporting");
            assert_eq!(
                editor.notes,
                "Reporting replica.\nAsk #data before heavy queries."
            );
            assert!(editor.block_select);
            assert_eq!(editor.auth_mode, AuthMode::SqlLogin);
            assert_eq!(editor.username, "reporter");
            let fresh = editor.sql_account.expect("copy has its own account id");
            assert_ne!(fresh, account, "the copy never shares the keyring entry");
            assert_eq!(
                editor.password_input.read(cx).value().as_ref(),
                "hunter2",
                "the original's password is read into the form so Save stores it under the copy"
            );
            assert_eq!(editor.notice, None);

            // Saving-shaped build: the profile carries the fresh reference,
            // not the original's, and no cleartext.
            let built = editor.build_profile(cx).expect("copy is saveable as-is");
            let AuthenticationType::SqlLogin { secret_ref, .. } = &built.authentication else {
                panic!("auth mode preserved");
            };
            assert_eq!(secret_ref.provider, SecretProvider::MacOsKeychain);
            assert_eq!(secret_ref.key, fresh.to_string());
            assert_ne!(built.id.0, original_id);
        });
    }

    #[gpui::test]
    fn from_duplicate_without_a_readable_secret_opens_empty_and_says_so(
        cx: &mut gpui::TestAppContext,
    ) {
        keyring::mock::install();
        // Nothing stored under this account: the keychain has no entry.
        let original = keychain_login_profile("Warehouse", Uuid::new_v4());

        let (view, cx, _temp) = make_duplicate_editor(original, Vec::new(), cx);
        view.update(cx, |editor, cx| {
            assert_eq!(editor.name, "Warehouse (copy)");
            assert_eq!(editor.password_input.read(cx).value().as_ref(), "");
            let notice = editor
                .notice
                .clone()
                .expect("the form says the password is missing");
            assert!(notice.contains("without a saved password"), "{notice}");
        });
    }

    #[gpui::test]
    fn tls_select_writes_mode_and_legacy_booleans(cx: &mut gpui::TestAppContext) {
        let (view, cx, _temp) = make_editor(cx);
        view.update(cx, |editor, cx| {
            // The editor defaults to full verification (encrypt on, trust off).
            assert_eq!(editor.tls_mode, TlsMode::VerifyFull);
            assert!(editor.encrypt);
            assert!(!editor.trust_certificate);

            // Every mode keeps the two legacy booleans on its safe equivalent.
            editor.set_tls_mode(TlsMode::Require, cx);
            assert_eq!(editor.tls_mode, TlsMode::Require);
            assert!(editor.encrypt);
            assert!(editor.trust_certificate);

            editor.set_tls_mode(TlsMode::VerifyCa, cx);
            assert!(editor.encrypt);
            assert!(!editor.trust_certificate);

            editor.set_tls_mode(TlsMode::Disable, cx);
            assert!(!editor.encrypt);
            assert!(!editor.trust_certificate);
        });
    }
}
