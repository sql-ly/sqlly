//! MCP server preferences: a persisted list of Model Context Protocol
//! servers (stdio or HTTP) with a Test action that performs a real
//! `initialize` JSON-RPC handshake, reports the server's name and version,
//! and discovers the server's tools via `tools/list`.
//!
//! Scope: configuration, connectivity, and tool *discovery* only — SQLLY
//! does not execute MCP tools. Discovered tool names/descriptions (bounded,
//! non-secret) persist alongside the config so the pane and the AI panel's
//! tool digest work without re-running a handshake. Configuration persists
//! as JSON next to the other preference files; environment values are
//! stored in the system keychain (sessionStorage in the browser preview),
//! keyed by each server's stable id — never in the JSON file.

use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, IntoElement, ParentElement, Render,
    Styled, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};

use crate::field_input::FieldInput;
use serde::{Deserialize, Serialize};
use sqlly_services::mcp::policy::{self, ConnectionMode, ConnectionScope, McpMode, McpPolicy};
use uuid::Uuid;

use crate::connection::ConnectionStore;
use crate::theme::Theme;

const MCP_ENV_SERVICE: &str = crate::distribution::keychain_service!("SQLLY.mcp-env");

fn new_server_id() -> Uuid {
    Uuid::new_v4()
}

fn env_account_key(id: Uuid) -> String {
    format!("mcp-env.{id}")
}

/// Process-local cache so a round-trip works even when the keyring mock
/// (used in tests) does not persist across `Entry::new` calls. Same pattern
/// as [`crate::sql_credentials`].
fn env_cache() -> &'static parking_lot::Mutex<std::collections::BTreeMap<Uuid, String>> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::BTreeMap<Uuid, String>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Load one server's environment text from the credential store.
pub fn load_mcp_env(id: Uuid) -> Result<Option<String>, String> {
    if let Some(value) = env_cache().lock().get(&id).cloned() {
        return Ok(Some(value));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let entry = keyring::Entry::new(MCP_ENV_SERVICE, &env_account_key(id))
            .map_err(|error| error.to_string())?;
        match entry.get_password() {
            Ok(value) => {
                env_cache().lock().insert(id, value.clone());
                Ok(Some(value))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::secrets::get(MCP_ENV_SERVICE, &env_account_key(id))
    }
}

/// Persist one server's environment text; an empty string deletes the entry.
pub fn store_mcp_env(id: Uuid, env: &str) -> Result<(), String> {
    if env.is_empty() {
        return delete_mcp_env(id);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        keyring::Entry::new(MCP_ENV_SERVICE, &env_account_key(id))
            .map_err(|error| error.to_string())?
            .set_password(env)
            .map_err(|error| error.to_string())?;
        env_cache().lock().insert(id, env.to_string());
        Ok(())
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::secrets::set(MCP_ENV_SERVICE, &env_account_key(id), env)?;
        env_cache().lock().insert(id, env.to_string());
        Ok(())
    }
}

/// Remove a server's stored environment when the server row is deleted.
pub fn delete_mcp_env(id: Uuid) -> Result<(), String> {
    env_cache().lock().remove(&id);
    #[cfg(not(target_arch = "wasm32"))]
    {
        match keyring::Entry::new(MCP_ENV_SERVICE, &env_account_key(id))
            .map_err(|error| error.to_string())?
            .delete_credential()
        {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::secrets::delete(MCP_ENV_SERVICE, &env_account_key(id))
    }
}

/// Handshake wall-clock budget; a hung server is killed, not waited on.
const HANDSHAKE_TIMEOUT_SECS: u64 = 10;
/// Extra budget for the best-effort `tools/list` after a good initialize; a
/// server that never answers it still passes the connectivity test.
#[cfg(not(target_arch = "wasm32"))]
const TOOLS_LIST_TIMEOUT_SECS: u64 = 5;
/// MCP protocol revision the client announces.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
/// Tools remembered per server; the pane notes "+N more" past the cap.
const MAX_TOOLS_PER_SERVER: usize = 25;
/// Per-tool description budget when parsing `tools/list`.
const MAX_TOOL_DESCRIPTION_CHARS: usize = 200;
/// Per-tool description budget inside the AI-facing digest.
const DIGEST_DESCRIPTION_CHARS: usize = 100;
/// Total character budget for the AI-facing tool digest.
const MAX_TOOL_DIGEST_CHARS: usize = 1_500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Stable id for keychain-backed env storage and migration.
    #[serde(default = "new_server_id")]
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub transport: McpTransport,
    /// stdio: the executable to spawn.
    #[serde(default)]
    pub command: String,
    /// stdio: space-separated arguments, as typed.
    #[serde(default)]
    pub args: String,
    /// stdio: newline- or `;`-separated `KEY=VALUE` pairs. In memory only —
    /// persisted via [`store_mcp_env`], never written into the JSON file.
    #[serde(default, skip_serializing)]
    pub env: String,
    /// HTTP: the endpoint URL.
    #[serde(default)]
    pub url: String,
    /// Last-known tools from `tools/list`, capped at [`MAX_TOOLS_PER_SERVER`].
    /// Non-secret; optional so pre-discovery files still load.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<McpToolInfo>,
    /// How many tools the server reported beyond the stored cap.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tools_truncated: usize,
    /// Local time of the successful test that produced `tools`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_seen_at: Option<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            id: new_server_id(),
            name: String::new(),
            enabled: false,
            transport: McpTransport::default(),
            command: String::new(),
            args: String::new(),
            env: String::new(),
            url: String::new(),
            tools: Vec::new(),
            tools_truncated: 0,
            tools_seen_at: None,
        }
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// One discovered tool: name plus a bounded, whitespace-normalized
/// description.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

impl McpServerConfig {
    /// Field-level validation, returning the first problem.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err(crate::localization::text("settings.mcp.static.name_required").to_string());
        }
        match self.transport {
            McpTransport::Stdio => {
                if self.command.trim().is_empty() {
                    return Err(crate::localization::text(
                        "settings.mcp.static.command_required_stdio_server",
                    )
                    .to_string());
                }
                for pair in parse_env(&self.env) {
                    if pair.is_err() {
                        return Err(crate::localization::text(
                            "settings.mcp.static.env_format_invalid",
                        )
                        .to_string());
                    }
                }
            }
            McpTransport::Http => {
                let url = self.url.trim();
                if !mcp_http_url_allowed(url) {
                    return Err(
                        crate::localization::text("settings.mcp.static.url_scheme_error")
                            .to_string(),
                    );
                }
            }
        }
        Ok(())
    }
}

/// Whether an HTTP MCP endpoint URL is allowed for Test / discovery.
/// `https://` is permitted except link-local and cloud-metadata hosts unless
/// [`load_allow_private_network_mcp`] is true; plain `http://` only to loopback.
pub fn mcp_http_url_allowed(url: &str) -> bool {
    mcp_http_url_allowed_with_policy(url, load_allow_private_network_mcp())
}

/// Same as [`mcp_http_url_allowed`] but with an explicit metadata/link-local opt-in.
pub fn mcp_http_url_allowed_with_policy(url: &str, allow_private_network: bool) -> bool {
    let parsed = match url::Url::parse(url.trim()) {
        Ok(url) => url,
        Err(_) => return false,
    };
    match parsed.scheme() {
        "https" => parsed
            .host_str()
            .is_some_and(|host| allow_private_network || !mcp_https_host_denied(host)),
        "http" => parsed.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host == "127.0.0.1"
                || host == "::1"
                || host == "[::1]"
        }),
        _ => false,
    }
}

/// HTTPS hosts refused by default (link-local + well-known cloud metadata).
///
/// Denied hostnames: `169.254.169.254`, `metadata.google.internal`,
/// `fd00:ec2::254`. Denied ranges: IPv4 link-local `169.254.0.0/16`, IPv6
/// link-local `fe80::/10`. Private LAN (`10.0.0.0/8`, etc.) stays allowed.
fn mcp_https_host_denied(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.eq_ignore_ascii_case("metadata.google.internal")
        || host.eq_ignore_ascii_case("169.254.169.254")
        || host.eq_ignore_ascii_case("fd00:ec2::254")
    {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => v4.is_link_local(),
            std::net::IpAddr::V6(v6) => v6.is_unicast_link_local(),
        };
    }
    false
}

pub fn load_allow_private_network_mcp() -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Ok(text) = std::fs::read_to_string(servers_path()) else {
            return false;
        };
        serde_json::from_str::<McpServersFile>(&text)
            .map(|file| file.allow_private_network_mcp)
            .unwrap_or(false)
    }
    #[cfg(target_arch = "wasm32")]
    {
        false
    }
}

/// Parse the env text into pairs; each entry is `Ok((key, value))` or `Err(())`
/// for a malformed line.
fn parse_env(env: &str) -> Vec<Result<(String, String), ()>> {
    env.split(['\n', ';'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once('=') {
            Some((key, value)) if !key.trim().is_empty() => {
                Ok((key.trim().to_string(), value.trim().to_string()))
            }
            _ => Err(()),
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServersFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
    /// When true, HTTP Test may reach link-local and cloud-metadata hosts.
    #[serde(default)]
    pub allow_private_network_mcp: bool,
}

#[cfg(not(target_arch = "wasm32"))]
fn servers_path() -> std::path::PathBuf {
    if let Some(dir) = dirs::data_dir() {
        let dir = dir.join("SQLLY");
        let _ = ensure_sqly_data_dir(&dir);
        return dir.join("mcp-servers.json");
    }
    std::path::PathBuf::from("mcp-servers.json")
}

#[cfg(not(target_arch = "wasm32"))]
fn ensure_sqly_data_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    tighten_dir_permissions(dir, 0o700);
    Ok(())
}

#[cfg(unix)]
fn tighten_dir_permissions(path: &std::path::Path, target_mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o777 != target_mode {
            let _ = std::fs::set_permissions(
                path,
                std::fs::Permissions::from_mode(mode & !0o777 | target_mode),
            );
        }
    }
}

#[cfg(unix)]
fn tighten_owner_only_file(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & !0o077));
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn write_owner_only_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_sqly_data_dir(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mcp-servers.json");
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        #[cfg(unix)]
        tighten_owner_only_file(path);
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub fn load_servers() -> Vec<McpServerConfig> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Ok(text) = std::fs::read_to_string(servers_path()) else {
            return Vec::new();
        };
        let mut servers = parse_servers_json(&text);
        let mut dirty = servers_missing_stable_ids(&text);
        dirty |= finalize_loaded_servers(&mut servers);
        if dirty {
            save_servers(&servers);
        }
        servers
    }
    #[cfg(target_arch = "wasm32")]
    {
        Vec::new()
    }
}

fn servers_missing_stable_ids(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| {
            value
                .get("servers")
                .and_then(|servers| servers.as_array())
                .cloned()
        })
        .is_some_and(|servers| servers.iter().any(|server| server.get("id").is_none()))
}

/// Assign ids, lift legacy plaintext `env` from JSON into the keychain, and
/// hydrate in-memory env from the credential store. Returns whether the file
/// should be rewritten without plaintext env.
fn finalize_loaded_servers(servers: &mut [McpServerConfig]) -> bool {
    let mut dirty = false;
    for server in servers.iter_mut() {
        dirty |= finalize_loaded_server(server);
    }
    dirty
}

fn finalize_loaded_server(server: &mut McpServerConfig) -> bool {
    let mut dirty = false;
    let legacy_env = std::mem::take(&mut server.env);
    if !legacy_env.is_empty() {
        match store_mcp_env(server.id, &legacy_env) {
            Ok(()) => dirty = true,
            Err(_) => {
                server.env = legacy_env;
                return dirty;
            }
        }
    }
    match load_mcp_env(server.id) {
        Ok(Some(env)) => server.env = env,
        Ok(None) => {}
        Err(_) if !legacy_env.is_empty() => server.env = legacy_env,
        Err(_) => {}
    }
    dirty
}

/// Tolerant load: an unreadable file yields no servers rather than a panic.
pub fn parse_servers_json(text: &str) -> Vec<McpServerConfig> {
    serde_json::from_str::<McpServersFile>(text)
        .map(|file| file.servers)
        .unwrap_or_default()
}

#[allow(dead_code)]
pub fn servers_json(servers: &[McpServerConfig]) -> String {
    servers_json_with_policy(servers, load_allow_private_network_mcp())
}

fn servers_json_with_policy(
    servers: &[McpServerConfig],
    allow_private_network_mcp: bool,
) -> String {
    serde_json::to_string_pretty(&McpServersFile {
        version: 1,
        servers: servers.to_vec(),
        allow_private_network_mcp,
    })
    .unwrap_or_else(|_| "{}".to_string())
}

fn save_servers(servers: &[McpServerConfig]) {
    save_servers_with_policy(servers, load_allow_private_network_mcp());
}

fn save_servers_with_policy(servers: &[McpServerConfig], allow_private_network_mcp: bool) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        for server in servers {
            let _ = store_mcp_env(server.id, &server.env);
        }
        let path = servers_path();
        let _ = write_owner_only_atomic(
            &path,
            servers_json_with_policy(servers, allow_private_network_mcp).as_bytes(),
        );
    }
}

/// The JSON-RPC initialize request body (newline-delimited over stdio, POST
/// body over HTTP).
pub fn initialize_request() -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "SQLLY", "version": crate::about::APP_VERSION },
        },
    })
    .to_string()
}

/// Pull the server's identity out of an initialize response line. Errors are
/// the JSON-RPC error message when present, else a shape complaint.
pub fn parse_initialize_response(line: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|_| {
        crate::localization::text("settings.mcp.static.server_did_not_answer_json").to_string()
    })?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(crate::localization::format(
            "settings.mcp.message.initialize_rejected",
            &[("message", &(message).to_string())],
        ));
    }
    let result = value.get("result").ok_or(
        crate::localization::text("settings.mcp.static.server_s_answer_has_no_result").to_string(),
    )?;
    let name = result
        .pointer("/serverInfo/name")
        .and_then(|v| v.as_str())
        .unwrap_or("unnamed server");
    let version = result
        .pointer("/serverInfo/version")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let protocol = result
        .get("protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    Ok(crate::localization::format(
        "settings.mcp.message.server_info",
        &[
            ("name", &(name).to_string()),
            ("version", &(version).to_string()),
            ("protocol", &(protocol).to_string()),
        ],
    ))
}

/// The `notifications/initialized` message MCP requires between initialize
/// and the first request.
pub fn initialized_notification() -> String {
    serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string()
}

/// The `tools/list` request (id 2), sent only after initialize succeeds.
pub fn tools_list_request() -> String {
    serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} })
        .to_string()
}

/// Character-bounded copy; a cut ends in an ellipsis so truncation is visible.
fn clamp_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut clamped: String = text.chars().take(max.saturating_sub(1)).collect();
        clamped.push('…');
        clamped
    }
}

/// Parse a `tools/list` response line into `(tools, total_reported)`. Tools
/// are capped at [`MAX_TOOLS_PER_SERVER`]; descriptions are whitespace-
/// normalized and clamped to [`MAX_TOOL_DESCRIPTION_CHARS`]. Errors are the
/// JSON-RPC error message when present, else a shape complaint.
pub fn parse_tools_response(line: &str) -> Result<(Vec<McpToolInfo>, usize), String> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|_| {
        crate::localization::text("settings.mcp.static.tools_list_not_json").to_string()
    })?;
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        return Err(crate::localization::format(
            "settings.mcp.message.tools_list_rejected",
            &[("message", &(message).to_string())],
        ));
    }
    let tools = value
        .pointer("/result/tools")
        .and_then(|tools| tools.as_array())
        .ok_or(crate::localization::text("settings.mcp.static.tools_array_missing").to_string())?;
    let total = tools.len();
    let parsed = tools
        .iter()
        .take(MAX_TOOLS_PER_SERVER)
        .map(|tool| {
            let name = tool
                .get("name")
                .and_then(|name| name.as_str())
                .unwrap_or("unnamed")
                .to_string();
            let description = tool
                .get("description")
                .and_then(|description| description.as_str())
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            McpToolInfo {
                name,
                description: clamp_chars(&description, MAX_TOOL_DESCRIPTION_CHARS),
            }
        })
        .collect();
    Ok((parsed, total))
}

/// Bounded, AI-facing digest of the enabled servers' last-known tools. Pure;
/// [`tool_digest`] feeds it the persisted config. The phrasing tells the
/// model the tools can only be mentioned or recommended — SQLLY does not
/// execute them.
pub fn format_tool_digest(servers: &[McpServerConfig]) -> Option<String> {
    let segments: Vec<String> = servers
        .iter()
        .filter(|server| server.enabled && !server.tools.is_empty())
        .map(|server| {
            let tools: Vec<String> = server
                .tools
                .iter()
                .map(|tool| {
                    if tool.description.is_empty() {
                        tool.name.clone()
                    } else {
                        format!(
                            "{} ({})",
                            tool.name,
                            clamp_chars(&tool.description, DIGEST_DESCRIPTION_CHARS)
                        )
                    }
                })
                .collect();
            let more = if server.tools_truncated > 0 {
                crate::localization::format(
                    "settings.mcp.message.more",
                    &[("tools_truncated", &(server.tools_truncated).to_string())],
                )
            } else {
                String::new()
            };
            crate::localization::format(
                "settings.mcp.message.more_2",
                &[
                    ("name", &(server.name).to_string()),
                    ("value_2", &(tools.join(", ")).to_string()),
                    ("more", &(more).to_string()),
                ],
            )
        })
        .collect();
    if segments.is_empty() {
        return None;
    }
    let digest = crate::localization::format(
        "settings.mcp.message.ai_tool_digest",
        &[("value", &(segments.join("; ")).to_string())],
    );
    Some(clamp_chars(&digest, MAX_TOOL_DIGEST_CHARS))
}

/// Digest for the AI panel: reads the persisted server list. `None` when no
/// enabled server has a last-known tool list.
pub fn tool_digest() -> Option<String> {
    format_tool_digest(&load_servers())
}

/// What a successful Test learned: the server's identity line plus the tool
/// discovery result (empty when the server has no tools or never answered
/// `tools/list` — discovery is best-effort and never fails the test).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct HandshakeOutcome {
    identity: String,
    tools: Vec<McpToolInfo>,
    total_tools: usize,
}

/// Run the initialize handshake plus `tools/list` discovery against one
/// server config. Native only; the browser build reports the limitation
/// instead.
#[cfg(not(target_arch = "wasm32"))]
async fn run_handshake(config: McpServerConfig) -> Result<HandshakeOutcome, String> {
    let timeout = std::time::Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);
    match config.transport {
        McpTransport::Stdio => run_stdio_handshake(config, timeout).await,
        McpTransport::Http => run_http_handshake(config, timeout).await,
    }
}

/// Stdio handshake I/O extracted for unit tests (injectable timeout, no spawn).
#[cfg(not(target_arch = "wasm32"))]
async fn stdio_handshake_session<R, W>(
    stdin: &mut W,
    stdout: R,
    timeout: std::time::Duration,
) -> Result<HandshakeOutcome, String>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWriteExt + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    let handshake = async move {
        stdin
            .write_all(format!("{}\n", initialize_request()).as_bytes())
            .await
            .map_err(|error| {
                crate::localization::format(
                    "settings.mcp.message.could_not_write_server_error",
                    &[("error", &(error).to_string())],
                )
            })?;
        stdin.flush().await.map_err(|error| {
            crate::localization::format(
                "settings.mcp.message.stdio_flush_failed",
                &[("error", &(error).to_string())],
            )
        })?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        let mut identity = None;
        for _ in 0..32 {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
                        && value.get("id").and_then(|id| id.as_i64()) == Some(1)
                    {
                        identity = Some(parse_initialize_response(trimmed)?);
                        break;
                    }
                }
                Ok(None) => {
                    return Err(crate::localization::text(
                        "settings.mcp.static.exited_before_initialize",
                    )
                    .to_string());
                }
                Err(error) => {
                    return Err(crate::localization::format(
                        "settings.mcp.message.stdio_read_failed",
                        &[("error", &(error).to_string())],
                    ));
                }
            }
        }
        let Some(identity) = identity else {
            return Err(
                crate::localization::text("settings.mcp.static.initialize_no_answer").to_string(),
            );
        };
        let tools_stage = async {
            stdin
                .write_all(
                    format!("{}\n{}\n", initialized_notification(), tools_list_request())
                        .as_bytes(),
                )
                .await
                .ok()?;
            stdin.flush().await.ok()?;
            for _ in 0..64 {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
                            && value.get("id").and_then(|id| id.as_i64()) == Some(2)
                        {
                            return parse_tools_response(trimmed).ok();
                        }
                    }
                    _ => return None,
                }
            }
            None
        };
        let (tools, total_tools) = tokio::time::timeout(
            std::time::Duration::from_secs(TOOLS_LIST_TIMEOUT_SECS),
            tools_stage,
        )
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
        Ok(HandshakeOutcome {
            identity,
            tools,
            total_tools,
        })
    };
    tokio::time::timeout(timeout, handshake)
        .await
        .unwrap_or_else(|_| {
            Err(crate::localization::format(
                "settings.mcp.message.handshake_timeout",
                &[("value", &(timeout.as_secs()).to_string())],
            ))
        })
}

#[cfg(not(target_arch = "wasm32"))]
async fn run_stdio_handshake(
    config: McpServerConfig,
    timeout: std::time::Duration,
) -> Result<HandshakeOutcome, String> {
    if !crate::distribution::can_launch_user_programs() {
        return Err(crate::distribution::sandbox_blocked_message(
            crate::localization::text("settings.mcp.static.start_stdio_server"),
        ));
    }
    let args: Vec<String> = config.args.split_whitespace().map(str::to_string).collect();
    let mut command = tokio::process::Command::new(config.command.trim());
    command
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    for pair in parse_env(&config.env).into_iter().flatten() {
        command.env(pair.0, pair.1);
    }
    let mut child = command.spawn().map_err(|error| {
        crate::localization::format(
            "settings.mcp.message.could_not_start_server_error",
            &[("error", &(error).to_string())],
        )
    })?;
    let mut stdin = child.stdin.take().ok_or(crate::localization::text(
        "settings.mcp.static.could_not_open_server_s_input",
    ))?;
    let stdout = child.stdout.take().ok_or(crate::localization::text(
        "settings.mcp.static.open_stdout_failed",
    ))?;
    let outcome = stdio_handshake_session(&mut stdin, stdout, timeout).await;
    let _ = child.kill().await;
    outcome
}

#[cfg(not(target_arch = "wasm32"))]
async fn run_http_handshake(
    config: McpServerConfig,
    timeout: std::time::Duration,
) -> Result<HandshakeOutcome, String> {
    use reqwest::redirect::Policy;
    let url = config.url.trim();
    if !mcp_http_url_allowed(url) {
        return Err(
            crate::localization::text("settings.mcp.static.http_test_url_refused").to_string(),
        );
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(Policy::none())
        .build()
        .map_err(|error| {
            crate::localization::format(
                "settings.mcp.message.http_client_failed",
                &[("error", &(error).to_string())],
            )
        })?;
    let response = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(initialize_request())
        .send()
        .await
        .map_err(|error| {
            crate::localization::format(
                "settings.mcp.message.could_not_reach_server_error",
                &[("error", &(error).to_string())],
            )
        })?;
    let status = response.status();
    let session = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response.text().await.map_err(|error| {
        crate::localization::format(
            "settings.mcp.message.http_read_failed",
            &[("error", &(error).to_string())],
        )
    })?;
    if !status.is_success() {
        let brief: String = body.chars().take(160).collect();
        return Err(crate::localization::format(
            "settings.mcp.message.server_answered_status_brief",
            &[
                ("status", &(status).to_string()),
                ("brief", &(brief).to_string()),
            ],
        ));
    }
    let json_line = extract_json_line(&body).ok_or(crate::localization::text(
        "settings.mcp.static.answer_has_no_json",
    ))?;
    let identity = parse_initialize_response(json_line)?;
    let with_session = |builder: reqwest::RequestBuilder| match &session {
        Some(session) => builder.header("mcp-session-id", session),
        None => builder,
    };
    let _ = with_session(
        client
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(initialized_notification()),
    )
    .send()
    .await;
    let tools_response = with_session(
        client
            .post(url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .body(tools_list_request()),
    )
    .send()
    .await;
    let (tools, total_tools) = match tools_response {
        Ok(response) if response.status().is_success() => response
            .text()
            .await
            .ok()
            .and_then(|body| {
                extract_json_line(&body).and_then(|line| parse_tools_response(line).ok())
            })
            .unwrap_or_default(),
        _ => Default::default(),
    };
    Ok(HandshakeOutcome {
        identity,
        tools,
        total_tools,
    })
}

/// First JSON line in a plain or SSE-framed (`data:`-prefixed) HTTP body.
#[cfg(not(target_arch = "wasm32"))]
fn extract_json_line(body: &str) -> Option<&str> {
    body.lines()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .or_else(|| {
            body.lines()
                .map(str::trim)
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim)
                .find(|line| line.starts_with('{'))
        })
}

/// Per-server transient test state.
#[derive(Clone, Debug, PartialEq, Eq)]
enum TestState {
    Idle,
    Running,
    Ok(String),
    Failed(String),
}

#[cfg(not(target_arch = "wasm32"))]
const CLI_BINARY_NAME: &str = if cfg!(windows) {
    "sqlly-cli.exe"
} else {
    "sqlly-cli"
};

fn shell_quote_cli_path(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        format!("\"{}\"", path.replace('"', "\\\""))
    } else {
        path.to_string()
    }
}

/// Install snippet for Claude Code (`claude mcp add`).
pub fn install_snippet_claude(cli_path: &str) -> String {
    format!(
        "claude mcp add --transport stdio sqlly -- {} mcp",
        shell_quote_cli_path(cli_path)
    )
}

/// Install snippet for Cursor (`mcpServers` JSON).
pub fn install_snippet_cursor(cli_path: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": {
            "sqlly": {
                "command": cli_path,
                "args": ["mcp"]
            }
        }
    }))
    .unwrap_or_default()
}

/// Install snippet for Claude Desktop (`mcpServers` JSON).
pub fn install_snippet_claude_desktop(cli_path: &str) -> String {
    install_snippet_cursor(cli_path)
}

/// Install snippet for VS Code (`.vscode/mcp.json`).
pub fn install_snippet_vscode(cli_path: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "servers": {
            "sqlly": {
                "type": "stdio",
                "command": cli_path,
                "args": ["mcp"]
            }
        }
    }))
    .unwrap_or_default()
}

/// Install snippet for Windsurf (`mcpServers` JSON, same shape as Cursor).
pub fn install_snippet_windsurf(cli_path: &str) -> String {
    install_snippet_cursor(cli_path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionModeSelection {
    Inherit,
    Locked,
    ReadOnly,
    ReadWriteConfirm,
}

pub fn apply_connection_mode_selection(
    policy: &mut McpPolicy,
    id: Uuid,
    selection: ConnectionModeSelection,
) {
    match selection {
        ConnectionModeSelection::Inherit => {
            policy.connection_modes.remove(&id);
        }
        ConnectionModeSelection::Locked => {
            policy.connection_modes.insert(id, ConnectionMode::Locked);
        }
        ConnectionModeSelection::ReadOnly => {
            policy.connection_modes.insert(id, ConnectionMode::ReadOnly);
        }
        ConnectionModeSelection::ReadWriteConfirm => {
            policy
                .connection_modes
                .insert(id, ConnectionMode::ReadWriteConfirm);
        }
    }
}

pub fn connection_mode_selection(policy: &McpPolicy, id: Uuid) -> ConnectionModeSelection {
    match policy.connection_modes.get(&id) {
        None => ConnectionModeSelection::Inherit,
        Some(ConnectionMode::Locked) => ConnectionModeSelection::Locked,
        Some(ConnectionMode::ReadOnly) => ConnectionModeSelection::ReadOnly,
        Some(ConnectionMode::ReadWriteConfirm) => ConnectionModeSelection::ReadWriteConfirm,
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn find_cli_path() -> String {
    find_cli_path_from(
        std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
            .as_deref(),
    )
}

#[cfg(not(target_arch = "wasm32"))]
pub fn find_cli_path_from(exe_dir: Option<&std::path::Path>) -> String {
    if let Some(dir) = exe_dir {
        let sibling = dir.join(CLI_BINARY_NAME);
        if sibling.is_file() {
            return sibling.to_string_lossy().into_owned();
        }
    }
    if let Some(path) = find_cli_on_path(CLI_BINARY_NAME) {
        return path;
    }
    CLI_BINARY_NAME.to_string()
}

#[cfg(not(target_arch = "wasm32"))]
fn find_cli_on_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

pub enum McpSettingsEvent {
    OpenMcpActivity,
}
impl EventEmitter<McpSettingsEvent> for McpSettingsView {}

pub struct McpSettingsView {
    policy: McpPolicy,
    policy_status: Option<String>,
    cli_path_input: Entity<InputState>,
    snippet_input: Entity<TextareaState>,
    policy_max_rows_input: Entity<InputState>,
    policy_cell_chars_input: Entity<InputState>,
    policy_idle_timeout_input: Entity<InputState>,
    policy_max_queries_input: Entity<InputState>,
    policy_max_writes_input: Entity<InputState>,
    policy_max_ddl_input: Entity<InputState>,
    _policy_subscriptions: Vec<gpui::Subscription>,
    servers: Vec<McpServerConfig>,
    allow_private_network_mcp: bool,
    tests: Vec<TestState>,
    /// Index being edited, or `None` when the form adds a new server.
    editing: Option<usize>,
    form_transport: McpTransport,
    inputs: [FieldInput; 5],
    values: [String; 5],
    status: Option<String>,
    generation: u64,
    /// Server rows whose tool list is expanded in the pane.
    expanded_tools: std::collections::HashSet<usize>,
    /// Rows whose tool list came from a successful Test in this session;
    /// everything else is last-known from disk and rendered with a caveat.
    fresh_tools: std::collections::HashSet<usize>,
    _subscriptions: Vec<gpui::Subscription>,
}

const FIELD_NAME: usize = 0;
const FIELD_COMMAND: usize = 1;
const FIELD_ARGS: usize = 2;
const FIELD_ENV: usize = 3;
const FIELD_URL: usize = 4;
fn field_labels() -> [String; 5] {
    [
        crate::localization::text("settings.mcp.form.name"),
        crate::localization::text("settings.mcp.form.command"),
        crate::localization::text("settings.mcp.form.arguments"),
        crate::localization::text("settings.mcp.form.environment"),
        crate::localization::text("settings.mcp.form.url"),
    ]
}

impl McpSettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let policy = policy::load_policy();
        #[cfg(target_arch = "wasm32")]
        let policy = McpPolicy::default();

        #[cfg(not(target_arch = "wasm32"))]
        let cli_path = find_cli_path();
        #[cfg(target_arch = "wasm32")]
        let cli_path = "sqlly-cli".to_string();

        let cli_path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "settings.mcp_settings.placeholder.sqlly_cli_path",
                ))
                .default_value(cli_path.clone())
        });
        let snippet_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .soft_wrap(true)
                .default_value(install_snippet_claude(&cli_path))
        });
        let policy_max_rows_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("100")
                .default_value(policy.max_rows.to_string())
        });
        let policy_cell_chars_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("200")
                .default_value(policy.cell_char_limit.to_string())
        });
        let policy_idle_timeout_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("900")
                .default_value(policy.idle_timeout_secs.to_string())
        });
        let policy_max_queries_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("60")
                .default_value(policy.max_queries_per_minute.to_string())
        });
        let policy_max_writes_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("5")
                .default_value(policy.max_writes_per_minute.to_string())
        });
        let policy_max_ddl_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("2")
                .default_value(policy.max_ddl_per_minute.to_string())
        });

        let placeholders = [
            "my-mcp-server",
            "npx",
            "-y @modelcontextprotocol/server-filesystem /tmp",
            "LOG_LEVEL=info",
            "https://example.com/mcp",
        ];
        let inputs: [FieldInput; 5] = std::array::from_fn(|index| {
            if index == FIELD_ENV {
                FieldInput::Multi(cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .soft_wrap(true)
                        .placeholder(placeholders[index])
                }))
            } else {
                FieldInput::Single(
                    cx.new(|cx| InputState::new(window, cx).placeholder(placeholders[index])),
                )
            }
        });
        let mut subscriptions = Vec::new();
        for (index, input) in inputs.iter().enumerate() {
            subscriptions.push(input.subscribe(
                window,
                cx,
                move |this: &mut Self, event, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = this.inputs[index].value(cx);
                        this.values[index] = value;
                        cx.notify();
                    }
                },
            ));
        }
        let mut _policy_subscriptions = Vec::new();
        _policy_subscriptions.push(cx.subscribe_in(
            &cli_path_input,
            window,
            |this: &mut Self, state, event: &InputEvent, window, cx| {
                if let InputEvent::Change = event {
                    let path = state.read(cx).value().to_string();
                    this.snippet_input.update(cx, |input, cx| {
                        input.set_value(install_snippet_claude(&path), window, cx);
                    });
                    cx.notify();
                }
            },
        ));
        for (index, input) in [
            &policy_max_rows_input,
            &policy_cell_chars_input,
            &policy_idle_timeout_input,
            &policy_max_queries_input,
            &policy_max_writes_input,
            &policy_max_ddl_input,
        ]
        .into_iter()
        .enumerate()
        {
            _policy_subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.apply_policy_limits_from_inputs(
                            index,
                            state.read(cx).value().as_ref(),
                        );
                        this.persist_policy(cx);
                    }
                },
            ));
        }
        let servers = load_servers();
        let allow_private_network_mcp = load_allow_private_network_mcp();
        let tests = vec![TestState::Idle; servers.len()];
        Self {
            policy,
            policy_status: None,
            cli_path_input,
            snippet_input,
            policy_max_rows_input,
            policy_cell_chars_input,
            policy_idle_timeout_input,
            policy_max_queries_input,
            policy_max_writes_input,
            policy_max_ddl_input,
            _policy_subscriptions,
            servers,
            allow_private_network_mcp,
            tests,
            editing: None,
            form_transport: McpTransport::Stdio,
            inputs,
            values: std::array::from_fn(|_| String::new()),
            status: None,
            generation: 0,
            expanded_tools: Default::default(),
            fresh_tools: Default::default(),
            _subscriptions: subscriptions,
        }
    }

    fn form_config(&self) -> McpServerConfig {
        // Editing keeps the fields the form does not surface: enabled plus
        // the last-known tool list (still shown with its "last seen" caveat).
        let existing = self.editing.and_then(|index| self.servers.get(index));
        McpServerConfig {
            name: self.values[FIELD_NAME].trim().to_string(),
            enabled: existing.map(|server| server.enabled).unwrap_or(true),
            transport: self.form_transport,
            id: existing
                .map(|server| server.id)
                .unwrap_or_else(new_server_id),
            command: self.values[FIELD_COMMAND].trim().to_string(),
            args: self.values[FIELD_ARGS].trim().to_string(),
            env: self.values[FIELD_ENV].trim().to_string(),
            url: self.values[FIELD_URL].trim().to_string(),
            tools: existing
                .map(|server| server.tools.clone())
                .unwrap_or_default(),
            tools_truncated: existing.map(|server| server.tools_truncated).unwrap_or(0),
            tools_seen_at: existing.and_then(|server| server.tools_seen_at.clone()),
        }
    }

    fn set_form(&mut self, config: &McpServerConfig, window: &mut Window, cx: &mut Context<Self>) {
        let values = [
            config.name.clone(),
            config.command.clone(),
            config.args.clone(),
            config.env.clone(),
            config.url.clone(),
        ];
        self.form_transport = config.transport;
        for (input, value) in self.inputs.iter().zip(&values) {
            input.set_value(value.clone(), window, cx);
        }
        self.values = values;
    }

    fn save_form(&mut self, cx: &mut Context<Self>) {
        let config = self.form_config();
        if let Err(error) = config.validate() {
            self.status = Some(error);
            cx.notify();
            return;
        }
        if let Err(error) = store_mcp_env(config.id, &config.env) {
            self.status = Some(crate::localization::format(
                "settings.mcp.message.env_keychain_save_failed",
                &[("error", &(error).to_string())],
            ));
            cx.notify();
            return;
        }
        let duplicate = self.servers.iter().enumerate().any(|(index, server)| {
            Some(index) != self.editing && server.name.eq_ignore_ascii_case(&config.name)
        });
        if duplicate {
            self.status = Some(crate::localization::format(
                "settings.mcp.message.server_named_already_exists",
                &[("name", &(config.name).to_string())],
            ));
            cx.notify();
            return;
        }
        match self.editing {
            Some(index) if index < self.servers.len() => {
                self.servers[index] = config;
                self.tests[index] = TestState::Idle;
                self.status = Some(
                    crate::localization::text("settings.mcp.static.server_updated").to_string(),
                );
            }
            _ => {
                self.servers.push(config);
                self.tests.push(TestState::Idle);
                self.status =
                    Some(crate::localization::text("settings.mcp.static.server_added").to_string());
            }
        }
        self.editing = None;
        save_servers_with_policy(&self.servers, self.allow_private_network_mcp);
        cx.notify();
    }

    fn remove(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.servers.len() {
            if let Some(server) = self.servers.get(index) {
                let _ = delete_mcp_env(server.id);
            }
            self.servers.remove(index);
            self.tests.remove(index);
            // Index-keyed row state would point at the wrong servers after a
            // removal; drop it rather than reshuffle.
            self.expanded_tools.clear();
            self.fresh_tools.clear();
            if self.editing == Some(index) {
                self.editing = None;
            }
            save_servers_with_policy(&self.servers, self.allow_private_network_mcp);
            cx.notify();
        }
    }

    fn toggle_enabled(&mut self, index: usize, enabled: bool, cx: &mut Context<Self>) {
        if let Some(server) = self.servers.get_mut(index) {
            server.enabled = enabled;
            save_servers_with_policy(&self.servers, self.allow_private_network_mcp);
            cx.notify();
        }
    }

    fn toggle_allow_private_network(&mut self, allowed: bool, cx: &mut Context<Self>) {
        self.allow_private_network_mcp = allowed;
        save_servers_with_policy(&self.servers, self.allow_private_network_mcp);
        cx.notify();
    }

    fn persist_policy(&mut self, cx: &mut Context<Self>) {
        self.policy = self.policy.clone().clamped();
        #[cfg(not(target_arch = "wasm32"))]
        {
            match policy::save_policy(&self.policy) {
                Ok(()) => {
                    self.policy_status = Some(
                        crate::localization::text("settings.mcp.static.policy_saved").to_string(),
                    )
                }
                Err(error) => self.policy_status = Some(error),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.policy_status = Some(crate::browser_preview::unavailable(
                crate::localization::text("settings.mcp.static.not_available"),
            ));
        }
        cx.notify();
    }

    fn set_policy_mode(&mut self, mode: McpMode, cx: &mut Context<Self>) {
        self.policy.mode = mode;
        self.persist_policy(cx);
    }

    fn connections_all_enabled(&self) -> bool {
        matches!(self.policy.connections, ConnectionScope::All)
    }

    fn connection_allowed(&self, id: Uuid) -> bool {
        self.policy.effective_mode(id) != ConnectionMode::Locked
            && match &self.policy.connections {
                ConnectionScope::All => true,
                ConnectionScope::Allow(ids) => ids.contains(&id),
            }
    }

    fn cycle_connection_mode(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let next = match connection_mode_selection(&self.policy, id) {
            ConnectionModeSelection::Inherit => ConnectionModeSelection::Locked,
            ConnectionModeSelection::Locked => ConnectionModeSelection::ReadOnly,
            ConnectionModeSelection::ReadOnly => ConnectionModeSelection::ReadWriteConfirm,
            ConnectionModeSelection::ReadWriteConfirm => ConnectionModeSelection::Inherit,
        };
        apply_connection_mode_selection(&mut self.policy, id, next);
        self.persist_policy(cx);
    }

    fn connection_mode_label(&self, id: Uuid) -> String {
        match connection_mode_selection(&self.policy, id) {
            ConnectionModeSelection::Inherit => {
                crate::localization::text("settings.mcp.static.inherit_global")
            }
            ConnectionModeSelection::Locked => {
                crate::localization::text("settings.mcp.static.locked")
            }
            ConnectionModeSelection::ReadOnly => {
                crate::localization::text("settings.mcp.static.read_only")
            }
            ConnectionModeSelection::ReadWriteConfirm => {
                crate::localization::text("settings.mcp.static.read_write_confirm")
            }
        }
    }

    fn set_connections_all(&mut self, all: bool, cx: &mut Context<Self>) {
        if all {
            self.policy.connections = ConnectionScope::All;
        } else {
            let store = ConnectionStore::global();
            let ids = store
                .list()
                .iter()
                .filter(|profile| {
                    !crate::mutation_review::SafetyFeatures::effective_for_profile(profile, store)
                        .is_production
                })
                .map(|profile| profile.id.0)
                .collect();
            self.policy.connections = ConnectionScope::Allow(ids);
        }
        self.persist_policy(cx);
    }

    fn toggle_connection_allowed(&mut self, id: Uuid, allowed: bool, cx: &mut Context<Self>) {
        let store = ConnectionStore::global();
        let mut ids = match &self.policy.connections {
            ConnectionScope::All => store
                .list()
                .iter()
                .map(|profile| profile.id.0)
                .collect::<Vec<_>>(),
            ConnectionScope::Allow(ids) => ids.clone(),
        };
        if allowed {
            if !ids.contains(&id) {
                ids.push(id);
            }
        } else {
            ids.retain(|existing| *existing != id);
        }
        self.policy.connections = ConnectionScope::Allow(ids);
        self.persist_policy(cx);
    }

    fn apply_policy_limits_from_inputs(&mut self, index: usize, value: &str) {
        let parsed = value.trim().parse::<u64>().ok();
        match index {
            0 => {
                if let Some(max_rows) = parsed {
                    self.policy.max_rows = max_rows as usize;
                }
            }
            1 => {
                if let Some(cell_chars) = parsed {
                    self.policy.cell_char_limit = cell_chars as usize;
                }
            }
            2 => {
                if let Some(idle_timeout) = parsed {
                    self.policy.idle_timeout_secs = idle_timeout;
                }
            }
            3 => {
                if let Some(max_queries) = parsed {
                    self.policy.max_queries_per_minute = max_queries as u32;
                }
            }
            4 => {
                if let Some(max_writes) = parsed {
                    self.policy.max_writes_per_minute = max_writes as u32;
                }
            }
            5 => {
                if let Some(max_ddl) = parsed {
                    self.policy.max_ddl_per_minute = max_ddl as u32;
                }
            }
            _ => {}
        }
    }

    fn render_sqly_mcp_server_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        if !crate::distribution::can_launch_user_programs() {
            let _ = cx;
            return crate::app_store_edition_dialog::edition_notice(
                "In the Mac App Store edition, SQLLY can't run as an MCP server for other apps, and \
                 local (stdio) MCP servers can't be launched. MCP servers you reach over HTTP still work.",
            );
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let store = ConnectionStore::global();
            let profiles = store.list();
            let mut section = div()
                .flex()
                .flex_col()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.95)))
                        .text_color(Theme::text())
                        .child(crate::localization::text(
                            "settings.mcp_settings.child.sqlly_as_mcp_server",
                        )),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.84)))
                        .text_color(Theme::text_dim())
                        .whitespace_normal()
                        .child(crate::localization::text(
                            "settings.mcp.static.server_intro",
                        )),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "settings.mcp_settings.child.global_ceiling",
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(6.0)))
                        .children(
                            [
                                (
                                    McpMode::ReadOnly,
                                    crate::localization::text(
                                        "settings.mcp.static.read_only_default",
                                    ),
                                ),
                                (
                                    McpMode::ReadWriteConfirm,
                                    crate::localization::text(
                                        "settings.mcp.static.ceiling_read_write_confirm",
                                    ),
                                ),
                            ]
                            .into_iter()
                            .map(|(mode, label)| {
                                let selected = self.policy.mode == mode;
                                div()
                                    .id(label.clone())
                                    .px(px(s(10.0)))
                                    .py(px(s(3.0)))
                                    .rounded(px(s(4.0)))
                                    .cursor_pointer()
                                    .bg(if selected {
                                        Theme::selection()
                                    } else {
                                        gpui::transparent_black()
                                    })
                                    .text_color(if selected {
                                        Theme::text()
                                    } else {
                                        Theme::text_dim()
                                    })
                                    .child(label)
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(move |this, _, _window, cx| {
                                            this.set_policy_mode(mode, cx);
                                        }),
                                    )
                            }),
                        ),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::text_dim())
                        .whitespace_normal()
                        .child(crate::localization::text(
                            "settings.mcp_settings.child.ceiling_note",
                        )),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "settings.mcp_settings.child.connections",
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(6.0)))
                        .child(
                            Checkbox::new("mcp-policy-all-connections")
                                .checked(self.connections_all_enabled())
                                .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                                    this.set_connections_all(*checked, cx);
                                })),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.82)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "settings.mcp_settings.child.all_connections",
                                )),
                        ),
                );

            if !self.connections_all_enabled() {
                for profile in profiles {
                    let id = profile.id.0;
                    let production = crate::mutation_review::SafetyFeatures::effective_for_profile(
                        &profile, store,
                    )
                    .is_production;
                    let label = if production {
                        crate::localization::format(
                            "settings.mcp.message.production_always_refused",
                            &[("name", &(profile.name).to_string())],
                        )
                    } else {
                        profile.name.clone()
                    };
                    section = section.child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(6.0)))
                            .child(
                                Checkbox::new(format!("mcp-policy-conn-{id}"))
                                    .checked(self.connection_allowed(id))
                                    .disabled(production)
                                    .on_click(cx.listener(
                                        move |this, checked: &bool, _window, cx| {
                                            if !production {
                                                this.toggle_connection_allowed(id, *checked, cx);
                                            }
                                        },
                                    )),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.82)))
                                    .text_color(if production {
                                        Theme::text_dim()
                                    } else {
                                        Theme::text()
                                    })
                                    .child(label),
                            )
                            .child(
                                Button::new(format!("mcp-mode-{id}"))
                                    .small()
                                    .ghost()
                                    .label(self.connection_mode_label(id))
                                    .disabled(production)
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        if !production {
                                            this.cycle_connection_mode(id, cx);
                                        }
                                    })),
                            ),
                    );
                }
            }

            section = section
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("settings.mcp_settings.child.limits")),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(6.0)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(s(1.0)))
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.8)))
                                        .text_color(Theme::text_dim())
                                        .child(crate::localization::text("settings.mcp_settings.child.max_rows_100")),
                                )
                                .child(Input::new(&self.policy_max_rows_input)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(s(1.0)))
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.8)))
                                        .text_color(Theme::text_dim())
                                        .child(crate::localization::text("settings.mcp_settings.child.max_chars_per_cell_2000")),
                                )
                                .child(Input::new(&self.policy_cell_chars_input)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(s(1.0)))
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.8)))
                                        .text_color(Theme::text_dim())
                                        .child(crate::localization::text("settings.mcp_settings.child.idle_timeout_s")),
                                )
                                .child(Input::new(&self.policy_idle_timeout_input)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(s(1.0)))
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.8)))
                                        .text_color(Theme::text_dim())
                                        .child(crate::localization::text("settings.mcp_settings.child.max_queries_per_minute_600")),
                                )
                                .child(Input::new(&self.policy_max_queries_input)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(s(1.0)))
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.8)))
                                        .text_color(Theme::text_dim())
                                        .child(crate::localization::text("settings.mcp_settings.child.max_writes_per_minute_60")),
                                )
                                .child(Input::new(&self.policy_max_writes_input)),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(s(1.0)))
                                .child(
                                    div()
                                        .text_size(px(Theme::font_size_ratio(0.8)))
                                        .text_color(Theme::text_dim())
                                        .child(crate::localization::text("settings.mcp_settings.child.max_ddl_per_minute_20")),
                                )
                                .child(Input::new(&self.policy_max_ddl_input)),
                        ),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("settings.mcp_settings.child.install")),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(1.0)))
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.8)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text("settings.mcp_settings.child.sqlly_cli_path")),
                        )
                        .child(Input::new(&self.cli_path_input)),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::text_dim())
                        .whitespace_normal()
                        .child(crate::localization::text("settings.mcp_settings.child.install_command_hint")),
                )
                .child(
                    div().h(px(s(72.0))).child(
                        Textarea::new(&self.snippet_input)
                            .disabled(true)
                            .font_family(Theme::editor_font_family()),
                    ),
                )
                .child(
                    // Wraps: six buttons overflow the settings pane, and the
                    // last one — Open MCP Activity — was clipped out of reach.
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(s(6.0)))
                        .child(
                            Button::new("mcp-copy-claude")
                                .small()
                                .ghost()
                                .label(crate::localization::text("settings.mcp_settings.label.copy_claude_code"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    let path = this.cli_path_input.read(cx).value().to_string();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        install_snippet_claude(&path),
                                    ));
                                    this.policy_status =
                                        Some(crate::localization::text("settings.mcp.static.copied_claude_code_snippet").to_string());
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("mcp-copy-cursor")
                                .small()
                                .ghost()
                                .label(crate::localization::text("settings.mcp_settings.label.copy_cursor"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    let path = this.cli_path_input.read(cx).value().to_string();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        install_snippet_cursor(&path),
                                    ));
                                    this.policy_status =
                                        Some(crate::localization::text("settings.mcp.static.copied_cursor_install_snippet").to_string());
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("mcp-copy-claude-desktop")
                                .small()
                                .ghost()
                                .label(crate::localization::text("settings.mcp_settings.label.copy_claude_desktop"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    let path = this.cli_path_input.read(cx).value().to_string();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        install_snippet_claude_desktop(&path),
                                    ));
                                    this.policy_status =
                                        Some(crate::localization::text("settings.mcp.static.copied_claude_desktop_snippet").to_string());
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("mcp-copy-windsurf")
                                .small()
                                .ghost()
                                .label(crate::localization::text("settings.mcp_settings.label.copy_windsurf"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    let path = this.cli_path_input.read(cx).value().to_string();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        install_snippet_windsurf(&path),
                                    ));
                                    this.policy_status =
                                        Some(crate::localization::text("settings.mcp.static.copied_windsurf_snippet").to_string());
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("mcp-copy-vscode")
                                .small()
                                .ghost()
                                .label(crate::localization::text("settings.mcp_settings.label.copy_vs_code"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    let path = this.cli_path_input.read(cx).value().to_string();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        install_snippet_vscode(&path),
                                    ));
                                    this.policy_status =
                                        Some(crate::localization::text("settings.mcp.static.copied_vs_code_snippet").to_string());
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("mcp-open-activity")
                                .small()
                                .label(crate::localization::text("settings.mcp_settings.label.open_mcp_activity"))
                                .on_click(cx.listener(|_, _, _window, cx| {
                                    cx.emit(McpSettingsEvent::OpenMcpActivity);
                                })),
                        ),
                )
                .children(self.policy_status.clone().map(|status| {
                    div()
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::warning())
                        .whitespace_normal()
                        .child(status)
                }));

            section.into_any_element()
        }
        #[cfg(target_arch = "wasm32")]
        {
            div()
                .flex()
                .flex_col()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.95)))
                        .text_color(Theme::text())
                        .child(crate::localization::text(
                            "settings.mcp_settings.child.sqlly_as_mcp_server_2",
                        )),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.84)))
                        .text_color(Theme::text_dim())
                        .child(crate::browser_preview::unavailable(
                            crate::localization::text("settings.mcp.static.not_available_2"),
                        )),
                )
                .into_any_element()
        }
    }

    fn test(&mut self, index: usize, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = index;
            self.status = Some(
                crate::browser_preview::unavailable(crate::localization::text(
                    "settings.mcp.static.server_test_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(config) = self.servers.get(index).cloned() else {
                return;
            };
            self.generation += 1;
            let generation = self.generation;
            self.tests[index] = TestState::Running;
            let async_cx = cx.to_async();
            let weak = cx.weak_entity();
            cx.spawn(async move |_this, _cx| {
                let handle = crate::tokio_handle().spawn(run_handshake(config));
                let outcome = match handle.await {
                    Ok(outcome) => outcome,
                    Err(_) => Err(crate::localization::text(
                        "settings.mcp.static.handshake_task_failed",
                    )
                    .to_string()),
                };
                let mut cx = async_cx.clone();
                weak.update(&mut cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    let state = match outcome {
                        Ok(outcome) => {
                            let count = outcome.tools.len();
                            let label = if outcome.total_tools > count {
                                crate::localization::format(
                                    "settings.mcp.message.count_tools",
                                    &[
                                        ("identity", &(outcome.identity).to_string()),
                                        ("count", &(count).to_string()),
                                        ("total_tools", &(outcome.total_tools).to_string()),
                                    ],
                                )
                            } else {
                                crate::localization::format(
                                    "settings.mcp.message.count_tools_2",
                                    &[
                                        ("identity", &(outcome.identity).to_string()),
                                        ("count", &(count).to_string()),
                                    ],
                                )
                            };
                            if let Some(server) = this.servers.get_mut(index) {
                                server.tools = outcome.tools;
                                server.tools_truncated = outcome.total_tools.saturating_sub(count);
                                server.tools_seen_at =
                                    Some(chrono::Local::now().format("%Y-%m-%d %H:%M").to_string());
                            }
                            // Persist the last-known tool list (non-secret)
                            // so the pane and AI digest have it on reopen.
                            save_servers_with_policy(&this.servers, this.allow_private_network_mcp);
                            this.fresh_tools.insert(index);
                            TestState::Ok(label)
                        }
                        Err(error) => TestState::Failed(error),
                    };
                    if let Some(slot) = this.tests.get_mut(index) {
                        *slot = state;
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
            cx.notify();
        }
    }

    fn render_server_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let server = &self.servers[index];
        let detail = match server.transport {
            McpTransport::Stdio => format!("stdio · {} {}", server.command, server.args),
            McpTransport::Http => format!("HTTP · {}", server.url),
        };
        let test = self.tests.get(index).cloned().unwrap_or(TestState::Idle);
        let (test_text, test_color) = match &test {
            TestState::Idle => (String::new(), Theme::text_dim()),
            TestState::Running => (
                crate::localization::text("settings.mcp.static.testing").to_string(),
                Theme::text_dim(),
            ),
            TestState::Ok(identity) => (format!("✓ {identity}"), Theme::success()),
            TestState::Failed(error) => (format!("✗ {error}"), Theme::error()),
        };
        let enabled = server.enabled;
        let tool_count = server.tools.len();
        let tools_expanded = self.expanded_tools.contains(&index);
        let tools_fresh = self.fresh_tools.contains(&index);
        let tools_summary = if tool_count > 0 {
            let icon = if tools_expanded { "▾" } else { "▸" };
            let key = if tool_count == 1 {
                "settings.mcp.tools_summary.single"
            } else {
                "settings.mcp.tools_summary.plural"
            };
            let mut summary = crate::localization::format(
                key,
                &[("icon", icon), ("tool_count", &tool_count.to_string())],
            );
            if server.tools_truncated > 0 {
                summary.push_str(&crate::localization::format(
                    "settings.mcp.message.more_server",
                    &[("tools_truncated", &(server.tools_truncated).to_string())],
                ));
            }
            if !tools_fresh {
                summary.push_str(&crate::localization::format(
                    "settings.mcp.message.last_seen",
                    &[(
                        "value",
                        &(server
                            .tools_seen_at
                            .as_deref()
                            .unwrap_or("at an earlier test"))
                        .to_string(),
                    )],
                ));
            }
            Some(summary)
        } else {
            None
        };
        let tool_lines: Vec<String> = if tools_expanded {
            server
                .tools
                .iter()
                .map(|tool| {
                    if tool.description.is_empty() {
                        tool.name.clone()
                    } else {
                        format!("{} — {}", tool.name, tool.description)
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .py(px(s(4.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        Checkbox::new(("mcp-enabled", index))
                            .checked(enabled)
                            .on_click(cx.listener(move |this, checked: &bool, _window, cx| {
                                this.toggle_enabled(index, *checked, cx);
                            })),
                    )
                    .child(
                        div()
                            .w(px(s(170.0)))
                            .flex_shrink_0()
                            .truncate()
                            .text_color(Theme::text())
                            .child(server.name.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_size(px(Theme::font_size_ratio(0.84)))
                            .text_color(Theme::text_dim())
                            .child(detail),
                    )
                    .child(
                        Button::new(("mcp-test", index))
                            .small()
                            .label(crate::localization::text(
                                "settings.mcp_settings.label.test",
                            ))
                            .disabled(matches!(test, TestState::Running))
                            .on_click(
                                cx.listener(move |this, _, _window, cx| this.test(index, cx)),
                            ),
                    )
                    .child(
                        Button::new(("mcp-edit", index))
                            .small()
                            .label(crate::localization::text(
                                "settings.mcp_settings.label.edit",
                            ))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if let Some(config) = this.servers.get(index).cloned() {
                                    this.editing = Some(index);
                                    this.set_form(&config, window, cx);
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new(("mcp-remove", index))
                            .small()
                            .label(crate::localization::text(
                                "settings.mcp_settings.label.remove",
                            ))
                            .on_click(
                                cx.listener(move |this, _, _window, cx| this.remove(index, cx)),
                            ),
                    ),
            )
            .when(!test_text.is_empty(), |row| {
                row.child(
                    div()
                        .pl(px(s(28.0)))
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .text_color(test_color)
                        .whitespace_normal()
                        .child(test_text),
                )
            })
            .when_some(tools_summary, |row, summary| {
                row.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(1.0)))
                        .pl(px(s(28.0)))
                        .child(
                            div()
                                .id(("mcp-tools-toggle", index))
                                .cursor_pointer()
                                .text_size(px(Theme::font_size_ratio(0.78)))
                                .text_color(Theme::text_dim())
                                .whitespace_normal()
                                .child(summary)
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(move |this, _, _window, cx| {
                                        if !this.expanded_tools.remove(&index) {
                                            this.expanded_tools.insert(index);
                                        }
                                        cx.notify();
                                    }),
                                ),
                        )
                        .children(tool_lines.into_iter().map(|line| {
                            div()
                                .pl(px(s(12.0)))
                                .text_size(px(Theme::font_size_ratio(0.76)))
                                .text_color(Theme::text_dim())
                                .whitespace_normal()
                                .child(line)
                        })),
                )
            })
            .into_any_element()
    }
}

impl Render for McpSettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let is_stdio = self.form_transport == McpTransport::Stdio;

        let mut list = div().flex().flex_col();
        if self.servers.is_empty() {
            list = list.child(
                div()
                    .py(px(s(6.0)))
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "settings.mcp_settings.child.no_mcp_servers_configured_yet",
                    )),
            );
        }
        for index in 0..self.servers.len() {
            list = list.child(self.render_server_row(index, cx));
        }

        let mut form = div().flex().flex_col().gap(px(s(6.0)));
        form = form.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "settings.mcp_settings.child.transport",
                        )),
                )
                .children(
                    [(McpTransport::Stdio, "stdio"), (McpTransport::Http, "HTTP")]
                        .into_iter()
                        .map(|(transport, label)| {
                            let selected = self.form_transport == transport;
                            div()
                                .id(label)
                                .px(px(s(10.0)))
                                .py(px(s(3.0)))
                                .rounded(px(s(4.0)))
                                .cursor_pointer()
                                .bg(if selected {
                                    Theme::selection()
                                } else {
                                    gpui::transparent_black()
                                })
                                .text_color(if selected {
                                    Theme::text()
                                } else {
                                    Theme::text_dim()
                                })
                                .child(label)
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(move |this, _, _window, cx| {
                                        this.form_transport = transport;
                                        cx.notify();
                                    }),
                                )
                        }),
                ),
        );
        for (index, label) in field_labels().iter().enumerate() {
            let relevant = match index {
                FIELD_COMMAND | FIELD_ARGS | FIELD_ENV => is_stdio,
                FIELD_URL => !is_stdio,
                _ => true,
            };
            if !relevant {
                continue;
            }
            form = form.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(1.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::text_dim())
                            .child(label.clone()),
                    )
                    .child(
                        div()
                            .when(index == FIELD_ENV, |element| element.h(px(s(52.0))))
                            .child(self.inputs[index].element()),
                    ),
            );
        }
        form = form.child(
            div()
                .flex()
                .flex_row()
                .gap(px(s(6.0)))
                .child(
                    Button::new("mcp-save")
                        .small()
                        .label(if self.editing.is_some() {
                            crate::localization::text("settings.mcp.static.update_server")
                        } else {
                            crate::localization::text("settings.mcp.static.add_server")
                        })
                        .on_click(cx.listener(|this, _, _window, cx| this.save_form(cx))),
                )
                .when(self.editing.is_some(), |row| {
                    row.child(
                        Button::new("mcp-cancel-edit")
                            .small()
                            .label(crate::localization::text(
                                "settings.mcp_settings.label.cancel_edit",
                            ))
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.editing = None;
                                this.status = None;
                                cx.notify();
                            })),
                    )
                }),
        );

        div()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .font(Theme::ui_font())
            .child(self.render_sqly_mcp_server_section(cx))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.84)))
                    .text_color(Theme::text_dim())
                    .whitespace_normal()
                    .child(crate::localization::text(
                        "settings.mcp.static.external_servers_intro",
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .child(
                        Checkbox::new("mcp-allow-private-network")
                            .checked(self.allow_private_network_mcp)
                            .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                                this.toggle_allow_private_network(*checked, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .whitespace_normal()
                            .child(crate::localization::text(
                                "settings.mcp.static.allow_private_network_label",
                            )),
                    ),
            )
            .child(list)
            .child(form)
            .children(self.status.clone().map(|status| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::warning())
                    .whitespace_normal()
                    .child(status)
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_validation_by_transport() {
        let mut config = McpServerConfig {
            name: "fs".into(),
            transport: McpTransport::Stdio,
            command: "npx".into(),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        config.command.clear();
        assert!(config.validate().is_err());
        config.name.clear();
        assert!(config.validate().is_err());

        let http = McpServerConfig {
            name: "remote".into(),
            transport: McpTransport::Http,
            url: "https://example.com/mcp".into(),
            ..Default::default()
        };
        assert!(http.validate().is_ok());
        let bad = McpServerConfig {
            url: "http://remote.example/mcp".into(),
            ..http.clone()
        };
        assert!(bad.validate().is_err());
        let ftp = McpServerConfig {
            url: "ftp://x".into(),
            ..http.clone()
        };
        assert!(ftp.validate().is_err());
    }

    #[test]
    fn env_pairs_parse_and_reject_malformed() {
        let mut config = McpServerConfig {
            name: "s".into(),
            command: "cmd".into(),
            env: "A=1\nB = two; C=3".into(),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        let pairs: Vec<_> = parse_env(&config.env).into_iter().flatten().collect();
        assert_eq!(
            pairs,
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "two".to_string()),
                ("C".to_string(), "3".to_string()),
            ]
        );
        config.env = "NOEQUALS".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn servers_json_omits_env_and_round_trips_non_secret_fields() {
        let id = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
        let servers = vec![McpServerConfig {
            id,
            name: "fs".into(),
            enabled: true,
            transport: McpTransport::Stdio,
            command: "npx".into(),
            args: "-y server".into(),
            env: "SECRET=do-not-persist".into(),
            url: String::new(),
            tools: vec![McpToolInfo {
                name: "read_file".into(),
                description: "Read a file from disk".into(),
            }],
            tools_truncated: 3,
            tools_seen_at: Some("2026-08-25 10:00".into()),
        }];
        let json = servers_json(&servers);
        assert!(!json.contains("\"env\""));
        assert!(!json.contains("do-not-persist"));
        let loaded = parse_servers_json(&json);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, id);
        assert_eq!(loaded[0].name, servers[0].name);
        assert_eq!(loaded[0].tools, servers[0].tools);
        assert!(loaded[0].env.is_empty());
        assert!(parse_servers_json("not json").is_empty());
        assert!(parse_servers_json("{}").is_empty());
    }

    #[test]
    fn mcp_env_keychain_persistence_and_migration() {
        keyring::mock::install();

        let id = Uuid::new_v4();
        store_mcp_env(id, "API_KEY=test").unwrap();
        let json = servers_json(&[McpServerConfig {
            id,
            name: "s".into(),
            command: "cmd".into(),
            env: "API_KEY=test".into(),
            ..Default::default()
        }]);
        assert!(!json.contains("API_KEY"));
        let mut server = parse_servers_json(&json).pop().unwrap();
        finalize_loaded_server(&mut server);
        assert_eq!(server.env, "API_KEY=test");
        delete_mcp_env(id).unwrap();

        let legacy_id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let old = format!(
            r#"{{"version":1,"servers":[{{"id":"{legacy_id}","name":"fs","enabled":true,"transport":"stdio","command":"npx","args":"","env":"TOKEN=legacy","url":""}}]}}"#
        );
        let mut servers = parse_servers_json(&old);
        assert_eq!(servers[0].env, "TOKEN=legacy");
        assert!(finalize_loaded_servers(&mut servers));
        assert_eq!(servers[0].env, "TOKEN=legacy");
        let rewritten = servers_json(&servers);
        assert!(!rewritten.contains("TOKEN"));
        assert!(!rewritten.contains("\"env\""));
        delete_mcp_env(legacy_id).unwrap();
    }

    #[test]
    fn mcp_http_url_allowed_pins_scheme_and_loopback() {
        assert!(mcp_http_url_allowed_with_policy(
            "https://example.com/mcp",
            false
        ));
        assert!(mcp_http_url_allowed_with_policy(
            "http://127.0.0.1:8080/mcp",
            false
        ));
        assert!(mcp_http_url_allowed_with_policy(
            "http://localhost/mcp",
            false
        ));
        assert!(mcp_http_url_allowed_with_policy(
            "http://[::1]:9000/mcp",
            false
        ));
        assert!(!mcp_http_url_allowed_with_policy(
            "http://example.com/mcp",
            false
        ));
        assert!(!mcp_http_url_allowed_with_policy(
            "file:///etc/passwd",
            false
        ));
        assert!(!mcp_http_url_allowed_with_policy(
            "unix:///tmp/socket",
            false
        ));
        assert!(!mcp_http_url_allowed_with_policy(
            "ftp://example.com",
            false
        ));
    }

    #[test]
    fn mcp_http_url_denies_metadata_and_link_local_https_by_default() {
        assert!(!mcp_http_url_allowed_with_policy(
            "https://169.254.169.254/",
            false
        ));
        assert!(!mcp_http_url_allowed_with_policy(
            "https://metadata.google.internal/",
            false
        ));
        assert!(!mcp_http_url_allowed_with_policy(
            "https://[fd00:ec2::254]/",
            false
        ));
        assert!(!mcp_http_url_allowed_with_policy(
            "https://169.254.1.1/mcp",
            false
        ));
        assert!(!mcp_http_url_allowed_with_policy(
            "https://[fe80::1]/mcp",
            false
        ));
        assert!(!mcp_http_url_allowed_with_policy("http://10.0.0.1", false));
        assert!(mcp_http_url_allowed_with_policy(
            "https://10.0.0.1/mcp",
            false
        ));
        assert!(mcp_http_url_allowed_with_policy(
            "https://169.254.169.254/",
            true
        ));
    }

    #[cfg(all(not(target_arch = "wasm32"), unix))]
    #[test]
    fn mcp_servers_json_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-servers.json");
        write_owner_only_atomic(&path, b"{}").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(not(target_arch = "wasm32"))]
    mod handshake_tests {
        use super::*;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        #[tokio::test]
        async fn stdio_handshake_parses_initialize_response() {
            let (mut client_write, server_read) = tokio::io::duplex(4096);
            let (mut server_write, client_read) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(server_read);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert!(line.contains("initialize"));
                let response = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"fixture","version":"0.1"}}}"#;
                server_write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            });
            let outcome = stdio_handshake_session(
                &mut client_write,
                client_read,
                std::time::Duration::from_secs(1),
            )
            .await
            .unwrap();
            assert_eq!(outcome.identity, "fixture v0.1 (protocol 2024-11-05)");
        }

        #[tokio::test]
        async fn stdio_handshake_times_out_when_server_is_silent() {
            let (mut client_write, _server_read) = tokio::io::duplex(64);
            let (_server_write, client_read) = tokio::io::duplex(64);
            let error = stdio_handshake_session(
                &mut client_write,
                client_read,
                std::time::Duration::from_millis(50),
            )
            .await
            .unwrap_err();
            assert!(error.contains("did not answer"));
        }
    }

    #[test]
    fn pre_discovery_files_still_load() {
        // A file written before tool discovery existed has none of the
        // optional tool fields; it must load with empty defaults.
        let old = r#"{"version":1,"servers":[{"name":"fs","enabled":true,"transport":"stdio","command":"npx","args":"","env":"","url":""}]}"#;
        let servers = parse_servers_json(old);
        assert_eq!(servers.len(), 1);
        assert!(servers[0].tools.is_empty());
        assert_eq!(servers[0].tools_truncated, 0);
        assert!(servers[0].tools_seen_at.is_none());
    }

    #[test]
    fn initialize_request_and_response_parsing() {
        let request = initialize_request();
        assert!(request.contains("\"method\":\"initialize\""));
        assert!(request.contains(MCP_PROTOCOL_VERSION));

        let ok = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"filesystem","version":"1.2.3"}}}"#;
        assert_eq!(
            parse_initialize_response(ok).unwrap(),
            "filesystem v1.2.3 (protocol 2024-11-05)"
        );
        let error = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"bad request"}}"#;
        assert!(
            parse_initialize_response(error)
                .unwrap_err()
                .contains("bad request")
        );
        assert!(parse_initialize_response("nope").is_err());
        assert!(parse_initialize_response("{}").is_err());
    }

    #[test]
    fn tools_list_request_and_response_parsing() {
        let request = tools_list_request();
        assert!(request.contains("\"method\":\"tools/list\""));
        assert!(request.contains("\"id\":2"));
        assert!(initialized_notification().contains("notifications/initialized"));

        let ok = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"read_file","description":"Read  a\nfile"},{"name":"bare"}]}}"#;
        let (tools, total) = parse_tools_response(ok).unwrap();
        assert_eq!(total, 2);
        assert_eq!(tools[0].name, "read_file");
        // Whitespace is normalized to single spaces.
        assert_eq!(tools[0].description, "Read a file");
        assert_eq!(tools[1].name, "bare");
        assert!(tools[1].description.is_empty());

        let empty = r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}"#;
        let (tools, total) = parse_tools_response(empty).unwrap();
        assert!(tools.is_empty());
        assert_eq!(total, 0);

        assert!(parse_tools_response("nope").is_err());
        assert!(parse_tools_response(r#"{"jsonrpc":"2.0","id":2,"result":{}}"#).is_err());
        assert!(
            parse_tools_response(r#"{"jsonrpc":"2.0","id":2,"error":{"message":"no tools"}}"#)
                .unwrap_err()
                .contains("no tools")
        );
    }

    #[test]
    fn tools_parsing_caps_count_and_description_length() {
        let tools_json: Vec<serde_json::Value> = (0..30)
            .map(|i| {
                serde_json::json!({ "name": format!("tool-{i}"), "description": "d".repeat(500) })
            })
            .collect();
        let line =
            serde_json::json!({ "jsonrpc": "2.0", "id": 2, "result": { "tools": tools_json } })
                .to_string();
        let (tools, total) = parse_tools_response(&line).unwrap();
        assert_eq!(tools.len(), MAX_TOOLS_PER_SERVER);
        assert_eq!(total, 30);
        assert!(
            tools
                .iter()
                .all(|tool| tool.description.chars().count() <= MAX_TOOL_DESCRIPTION_CHARS)
        );
        assert!(tools[0].description.ends_with('…'));
    }

    #[test]
    fn tool_digest_formats_caps_and_skips_disabled() {
        let _guard = crate::localization::test_language_with(
            crate::localization::Language::English,
            Some(include_str!(
                "../../../crates/sqlly-i18n/translations/en.json"
            )),
        );
        assert_eq!(format_tool_digest(&[]), None);
        // A server with no last-known tools contributes nothing.
        let bare = McpServerConfig {
            name: "bare".into(),
            enabled: true,
            ..Default::default()
        };
        assert_eq!(format_tool_digest(std::slice::from_ref(&bare)), None);

        let tool = |name: &str, description: &str| McpToolInfo {
            name: name.into(),
            description: description.into(),
        };
        let files = McpServerConfig {
            name: "files".into(),
            enabled: true,
            tools: vec![tool("read_file", "Read a file"), tool("write_file", "")],
            tools_truncated: 2,
            ..Default::default()
        };
        let disabled = McpServerConfig {
            name: "disabled".into(),
            enabled: false,
            tools: vec![tool("secret", "should not appear")],
            ..Default::default()
        };
        let digest = format_tool_digest(&[bare, files.clone(), disabled]).unwrap();
        assert!(digest.starts_with("External tools available via MCP"));
        assert!(digest.contains("mention or recommend"));
        assert!(digest.contains("files: read_file (Read a file), write_file (+2 more)"));
        assert!(!digest.contains("secret"));

        // The digest stays within its total budget even for huge tool lists.
        let huge = McpServerConfig {
            name: "huge".into(),
            enabled: true,
            tools: (0..MAX_TOOLS_PER_SERVER)
                .map(|i| tool(&format!("tool-{i}"), &"d".repeat(200)))
                .collect(),
            ..Default::default()
        };
        let digest = format_tool_digest(&[huge, files]).unwrap();
        assert!(digest.chars().count() <= MAX_TOOL_DIGEST_CHARS);
        assert!(digest.ends_with('…'));
    }

    #[test]
    fn claude_snippet_quotes_paths_with_spaces() {
        let snippet = install_snippet_claude("/Applications/SQLLY App/sqlly-cli");
        assert_eq!(
            snippet,
            "claude mcp add --transport stdio sqlly -- \"/Applications/SQLLY App/sqlly-cli\" mcp"
        );
    }

    #[test]
    fn cursor_snippet_is_valid_json_with_mcp_args() {
        let snippet = install_snippet_cursor("/usr/local/bin/sqlly-cli");
        let value: serde_json::Value = serde_json::from_str(&snippet).expect("valid json");
        assert_eq!(
            value["mcpServers"]["sqlly"]["command"],
            "/usr/local/bin/sqlly-cli"
        );
        assert_eq!(
            value["mcpServers"]["sqlly"]["args"],
            serde_json::json!(["mcp"])
        );
    }

    #[test]
    fn claude_desktop_snippet_is_mcp_servers_json() {
        let snippet = install_snippet_claude_desktop("/usr/local/bin/sqlly-cli");
        let value: serde_json::Value = serde_json::from_str(&snippet).expect("valid json");
        assert!(value.get("mcpServers").is_some());
    }

    #[test]
    fn vscode_snippet_uses_servers_key_and_stdio_type() {
        let snippet = install_snippet_vscode("/usr/local/bin/sqlly-cli");
        let value: serde_json::Value = serde_json::from_str(&snippet).expect("valid json");
        assert_eq!(value["servers"]["sqlly"]["type"], "stdio");
    }

    #[test]
    fn windsurf_snippet_matches_cursor_shape() {
        assert_eq!(
            install_snippet_windsurf("/bin/sqlly-cli"),
            install_snippet_cursor("/bin/sqlly-cli")
        );
    }

    #[test]
    fn mode_select_persists_connection_mode() {
        let mut policy = McpPolicy::default();
        let id = Uuid::new_v4();
        apply_connection_mode_selection(&mut policy, id, ConnectionModeSelection::Locked);
        assert_eq!(
            policy.connection_modes.get(&id),
            Some(&ConnectionMode::Locked)
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn find_cli_path_prefers_sibling_binary() {
        let dir = tempfile::tempdir().unwrap();
        let sibling = dir.path().join(CLI_BINARY_NAME);
        std::fs::write(&sibling, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            find_cli_path_from(Some(dir.path())),
            sibling.to_string_lossy().into_owned()
        );
    }
}
