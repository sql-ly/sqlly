#![forbid(unsafe_code)]

pub mod capability;
pub mod command;
pub mod connect_url;
pub mod connection;
mod connection_display;
pub mod connection_management;
pub mod connection_management_commands;
pub mod connection_url;
pub mod credential_import;
pub mod credential_proxy;
pub mod deep_link;
pub mod error;
pub mod event;
pub mod formula_guard;
pub mod object_model;
pub mod operational;
pub mod redact;
// Writes the crash log with std::fs; a browser tab has no filesystem and
// panics already reach the console via console_error_panic_hook.
#[cfg(not(target_arch = "wasm32"))]
pub mod panic_hook;
pub mod profile_store;
pub mod provisioning;
pub mod recovery;
pub mod redis_cluster;
pub mod redis_flavor;
pub mod redis_keys;
pub mod redis_sentinel;
pub mod registered_servers;
pub mod row_stream;
pub mod secret_resolver;
pub mod servers_explorer;
pub mod session;
pub mod spatial;
pub mod startup_sql;
// Spill-to-disk. On wasm only the storage-generic constructors work (the
// browser backs them with OPFS); the `File` ones need a real filesystem.
pub mod spill;
#[cfg(not(target_arch = "wasm32"))]
pub mod tool_export;
#[cfg(not(target_arch = "wasm32"))]
pub mod tool_import;

pub use capability::{
    AzureSqlErrorCategory, AzureSqlErrorHint, EngineEdition, ServerCapabilities, ServerCapability,
    ServerCapabilityProfile, azure_sql_error_hint,
};
pub use command::{Command, CommandBus};
pub use connection::{
    AuthenticationType, AzureSqlConnectionDefaults, ConnectionEnvironment, ConnectionId,
    ConnectionPreview, ConnectionProfile, ConnectionProvenance, ConnectionStatus,
    ConnectionTestStatus, DatabaseSummary, DisplayTimeZone, DuplicatedProfile, EntraAccountBinding,
    KubePortForward, KubeTargetKind, MaskRule, MaskingOverrides, ProxyHop, ProxyKind, RuleScope,
    SecretCopy, SecretProvider, SecretRef, SshAuth, SshHop, SshKeepalive, SshTunnelConfig,
    SsmPortForward, StoredMessage, TlsMode, TransportChain, TunnelProfile, TunnelProfileId,
    copy_name, redact_error_summary, unique_copy_name,
};
pub use connection_management::*;
pub use connection_management_commands::{
    ConnectionManagementCommand, ConnectionManagementEvent, ConnectionManagementService,
    RefreshScope, WorkspaceExpansionState,
};
pub use connection_url::{ParseUrlError, ParsedConnectionUrl, parse_connection_url};
pub use deep_link::{
    DeepLink, DeepLinkError, describe as describe_deep_link, file_connection_name,
    file_database_engine, is_duckdb_path, is_sqlite_path, parse_deep_link,
};
pub use error::{StudioError, StudioResult};
pub use event::{
    CellValue, Event, EventBus, EventPayload, EventSubscription, MaterializedResultSet,
    ResultColumn, SqlParam,
};
pub use object_model::*;
pub use operational::*;
#[cfg(not(target_arch = "wasm32"))]
pub use panic_hook::{crash_log_path, install_panic_hook};
pub use profile_store::{
    DEFAULT_RECENT_LIMIT, ExportedAuthentication, ExportedClientEntity, ExportedEnvironmentEntity,
    ExportedProfile, ExportedProjectEntity, ExportedSafetyFeatures, ExportedSqlAuthAccount,
    PROFILE_EXPORT_VERSION, ProfileExportBundle, ProfileStore, export_profile,
};
pub use recovery::{RecoveredTab, RecoveryError, RecoverySnapshot};
pub use redact::redact_sql;
pub use redis_sentinel::RedisSentinelConfig;
pub use registered_servers::{
    GroupId, RegisteredServer, RegisteredServerError, RegisteredServerStore, ServerGroup,
};
pub use row_stream::{RowBatch, RowChannelClosed, RowReceiver, RowSender, row_channel};
pub use sqlly_lang::message::DisplayMessage;
// Engine identity/capability vocabulary, re-exported so downstream crates can
// dispatch on `profile.database_type` without a direct sqlmodel dependency.
pub use servers_explorer::{
    ExplorerAction, ExplorerNodeSource, FilesystemDatabaseSource, ServersExplorerNode,
    ServersExplorerNodeId, ServersExplorerNodeKind, ServersExplorerNodeState, ServersExplorerTree,
};
pub use session::{SessionId, SessionInfo, SessionManager, SessionState};
pub use spatial::{Geometry, GeometryKind, Shape, SpatialError};
pub use spill::{SpillError, SpillReader, SpillWriter};
pub use sqlly_lang::sqlmodel::{
    ConnectionKind, DatabaseType, DbTypeMetadata, Dialect, RowLimitSyntax,
};
pub use startup_sql::{startup_sql_batches, startup_sql_failed, startup_sql_text};
#[cfg(not(target_arch = "wasm32"))]
pub use tool_export::{ToolExport, ToolExportFormat, export_for_tool};
#[cfg(not(target_arch = "wasm32"))]
pub use tool_import::{
    ImportedConnection, SkippedConnection, ToolFileKind, ToolImportPreview, dedupe_key,
    detect_kind, parse_tableplus_groups_plist, parse_tableplus_plist_with_groups, parse_tool_file,
    parse_tool_file_bytes, preview_to_bundle,
};
