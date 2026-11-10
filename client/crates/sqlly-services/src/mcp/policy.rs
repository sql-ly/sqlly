use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpMode {
    #[default]
    ReadOnly,
    ReadWriteConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionMode {
    Locked,
    ReadOnly,
    ReadWriteConfirm,
}

impl ConnectionMode {
    fn rank(self) -> u8 {
        match self {
            Self::Locked => 0,
            Self::ReadOnly => 1,
            Self::ReadWriteConfirm => 2,
        }
    }

    fn min(self, other: Self) -> Self {
        if self.rank() <= other.rank() {
            self
        } else {
            other
        }
    }
}

impl From<McpMode> for ConnectionMode {
    fn from(mode: McpMode) -> Self {
        match mode {
            McpMode::ReadOnly => ConnectionMode::ReadOnly,
            McpMode::ReadWriteConfirm => ConnectionMode::ReadWriteConfirm,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionScope {
    All,
    Allow(Vec<Uuid>),
}

impl Serialize for ConnectionScope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::All => serializer.serialize_str("all"),
            Self::Allow(ids) => {
                #[derive(Serialize)]
                struct Allow<'a> {
                    allow: &'a [Uuid],
                }
                Allow { allow: ids }.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for ConnectionScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{self, Visitor};
        use std::fmt;

        struct ScopeVisitor;

        impl<'de> Visitor<'de> for ScopeVisitor {
            type Value = ConnectionScope;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("\"all\" or {\"allow\": [uuid, ...]}")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.eq_ignore_ascii_case("all") {
                    Ok(ConnectionScope::All)
                } else {
                    Err(de::Error::invalid_value(
                        de::Unexpected::Str(value),
                        &"\"all\"",
                    ))
                }
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut allow: Option<Vec<Uuid>> = None;
                while let Some(key) = map.next_key::<String>()? {
                    if key == "allow" {
                        allow = Some(map.next_value()?);
                    } else {
                        let _ = map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                allow
                    .map(ConnectionScope::Allow)
                    .ok_or_else(|| de::Error::missing_field("allow"))
            }
        }

        deserializer.deserialize_any(ScopeVisitor)
    }
}

fn one() -> u32 {
    1
}

fn all() -> ConnectionScope {
    ConnectionScope::All
}

fn d100() -> usize {
    100
}

fn d200() -> usize {
    200
}

fn d900() -> u64 {
    900
}

fn d60() -> u32 {
    60
}

fn d5() -> u32 {
    5
}

fn d2() -> u32 {
    2
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPolicy {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub mode: McpMode,
    #[serde(default = "all")]
    pub connections: ConnectionScope,
    #[serde(default = "d100")]
    pub max_rows: usize,
    #[serde(default = "d200")]
    pub cell_char_limit: usize,
    #[serde(default = "d900")]
    pub idle_timeout_secs: u64,
    #[serde(default = "d60")]
    pub max_queries_per_minute: u32,
    #[serde(default = "d5")]
    pub max_writes_per_minute: u32,
    #[serde(default = "d2")]
    pub max_ddl_per_minute: u32,
    #[serde(default)]
    pub connection_modes: BTreeMap<Uuid, ConnectionMode>,
}

impl Default for McpPolicy {
    fn default() -> Self {
        Self {
            version: 1,
            mode: McpMode::ReadOnly,
            connections: ConnectionScope::All,
            max_rows: 100,
            cell_char_limit: 200,
            idle_timeout_secs: 900,
            max_queries_per_minute: 60,
            max_writes_per_minute: 5,
            max_ddl_per_minute: 2,
            connection_modes: BTreeMap::new(),
        }
    }
}

pub const MAX_ROWS_HARD_CAP: usize = 100;
pub const MAX_CELL_CHARS_HARD_CAP: usize = 2_000;
pub const MAX_QUERIES_PER_MINUTE_HARD_CAP: u32 = 600;
pub const MAX_WRITES_PER_MINUTE_HARD_CAP: u32 = 60;
pub const MAX_DDL_PER_MINUTE_HARD_CAP: u32 = 20;
const MIN_IDLE_TIMEOUT_SECS: u64 = 30;
const MAX_IDLE_TIMEOUT_SECS: u64 = 86_400;

impl McpPolicy {
    pub fn clamped(self) -> Self {
        Self {
            version: self.version.max(1),
            mode: self.mode,
            connections: self.connections,
            max_rows: self.max_rows.clamp(1, MAX_ROWS_HARD_CAP),
            cell_char_limit: self.cell_char_limit.clamp(1, MAX_CELL_CHARS_HARD_CAP),
            idle_timeout_secs: self
                .idle_timeout_secs
                .clamp(MIN_IDLE_TIMEOUT_SECS, MAX_IDLE_TIMEOUT_SECS),
            max_queries_per_minute: self
                .max_queries_per_minute
                .min(MAX_QUERIES_PER_MINUTE_HARD_CAP),
            max_writes_per_minute: self
                .max_writes_per_minute
                .min(MAX_WRITES_PER_MINUTE_HARD_CAP),
            max_ddl_per_minute: self.max_ddl_per_minute.min(MAX_DDL_PER_MINUTE_HARD_CAP),
            connection_modes: self.connection_modes,
        }
    }

    pub fn narrowed_by_env<'a>(self, env: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut policy = self.clamped();
        for (key, value) in env {
            match key {
                "SQLLY_MCP_MODE" => {
                    if value == "read_only" {
                        policy.mode = McpMode::ReadOnly
                    }
                }
                "SQLLY_MCP_CONNECTIONS" => {
                    if let Some(env_ids) = parse_uuid_list(value) {
                        policy.connections = match &policy.connections {
                            ConnectionScope::All => ConnectionScope::Allow(env_ids),
                            ConnectionScope::Allow(file_ids) => {
                                let env_set: std::collections::HashSet<_> =
                                    env_ids.iter().copied().collect();
                                ConnectionScope::Allow(
                                    file_ids
                                        .iter()
                                        .copied()
                                        .filter(|id| env_set.contains(id))
                                        .collect(),
                                )
                            }
                        };
                    }
                }
                "SQLLY_MCP_MAX_ROWS" => {
                    if let Ok(n) = value.parse::<usize>() {
                        policy.max_rows = policy.max_rows.min(n);
                    }
                }
                "SQLLY_MCP_CELL_CHAR_LIMIT" => {
                    if let Ok(n) = value.parse::<usize>() {
                        policy.cell_char_limit = policy.cell_char_limit.min(n);
                    }
                }
                "SQLLY_MCP_IDLE_TIMEOUT_SECS" => {
                    if let Ok(n) = value.parse::<u64>() {
                        policy.idle_timeout_secs = policy.idle_timeout_secs.min(n);
                    }
                }
                "SQLLY_MCP_MAX_QUERIES_PER_MINUTE" => {
                    if let Ok(n) = value.parse::<u32>() {
                        policy.max_queries_per_minute = policy.max_queries_per_minute.min(n);
                    }
                }
                "SQLLY_MCP_MAX_WRITES_PER_MINUTE" => {
                    if let Ok(n) = value.parse::<u32>() {
                        policy.max_writes_per_minute = policy.max_writes_per_minute.min(n);
                    }
                }
                "SQLLY_MCP_MAX_DDL_PER_MINUTE" => {
                    if let Ok(n) = value.parse::<u32>() {
                        policy.max_ddl_per_minute = policy.max_ddl_per_minute.min(n);
                    }
                }
                "SQLLY_MCP_LOCK_CONNECTIONS" => {
                    if let Some(ids) = parse_uuid_list(value) {
                        for id in ids {
                            policy.connection_modes.insert(id, ConnectionMode::Locked);
                        }
                    }
                }
                _ => {}
            }
        }
        policy.clamped()
    }

    pub fn effective_mode(&self, id: Uuid) -> ConnectionMode {
        let global = ConnectionMode::from(self.mode);
        self.connection_modes
            .get(&id)
            .map(|mode| global.min(*mode))
            .unwrap_or(global)
    }

    pub fn allows_connection(&self, id: Uuid) -> bool {
        if self.effective_mode(id) == ConnectionMode::Locked {
            return false;
        }
        match &self.connections {
            ConnectionScope::All => true,
            ConnectionScope::Allow(ids) => ids.contains(&id),
        }
    }

    pub fn allows_writes_on(&self, id: Uuid) -> bool {
        self.effective_mode(id) == ConnectionMode::ReadWriteConfirm
    }

    pub fn allows_writes(&self) -> bool {
        self.mode == McpMode::ReadWriteConfirm
    }
}

fn parse_uuid_list(value: &str) -> Option<Vec<Uuid>> {
    let ids: Vec<Uuid> = value
        .split(',')
        .filter_map(|part| Uuid::parse_str(part.trim()).ok())
        .collect();
    if ids.is_empty() { None } else { Some(ids) }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn policy_path() -> std::path::PathBuf {
    if let Ok(path) = std::env::var("SQLLY_MCP_POLICY_PATH") {
        return std::path::PathBuf::from(path);
    }
    if let Some(dir) = dirs::data_dir() {
        let dir = dir.join("SQLLY");
        let _ = ensure_sqly_data_dir(&dir);
        return dir.join("mcp-policy.json");
    }
    std::path::PathBuf::from("mcp-policy.json")
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load_policy() -> McpPolicy {
    load_policy_from(&policy_path())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load_policy_from(path: &std::path::Path) -> McpPolicy {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(McpPolicy::clamped)
            .unwrap_or_default()
            .clamped(),
        Err(_) => McpPolicy::default().clamped(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn save_policy(policy: &McpPolicy) -> Result<(), String> {
    save_policy_to(&policy_path(), policy)
}

#[cfg(not(target_arch = "wasm32"))]
pub fn save_policy_to(path: &std::path::Path, policy: &McpPolicy) -> Result<(), String> {
    let policy = policy.clone().clamped();
    let bytes = serde_json::to_vec_pretty(&policy).map_err(|error| error.to_string())?;
    write_owner_only_atomic(path, &bytes).map_err(|error| error.to_string())
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
        .unwrap_or("mcp-policy.json");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_read_only() -> McpPolicy {
        McpPolicy {
            mode: McpMode::ReadOnly,
            ..Default::default()
        }
    }

    fn policy_read_write() -> McpPolicy {
        McpPolicy {
            mode: McpMode::ReadWriteConfirm,
            ..Default::default()
        }
    }

    #[test]
    fn mode_file_read_only_env_unset() {
        let p = policy_read_only().narrowed_by_env([]);
        assert_eq!(p.mode, McpMode::ReadOnly);
    }

    #[test]
    fn mode_file_read_only_env_read_write_ignored() {
        let p = policy_read_only().narrowed_by_env([("SQLLY_MCP_MODE", "read_write_confirm")]);
        assert_eq!(p.mode, McpMode::ReadOnly);
    }

    #[test]
    fn mode_file_read_write_env_unset() {
        let p = policy_read_write().narrowed_by_env([]);
        assert_eq!(p.mode, McpMode::ReadWriteConfirm);
    }

    #[test]
    fn mode_file_read_write_env_read_only() {
        let p = policy_read_write().narrowed_by_env([("SQLLY_MCP_MODE", "read_only")]);
        assert_eq!(p.mode, McpMode::ReadOnly);
    }

    #[test]
    fn mode_file_read_write_env_garbage() {
        let p = policy_read_write().narrowed_by_env([("SQLLY_MCP_MODE", "garbage")]);
        assert_eq!(p.mode, McpMode::ReadWriteConfirm);
    }

    #[test]
    fn connections_all_env_unset() {
        let p = McpPolicy::default().narrowed_by_env([]);
        assert_eq!(p.connections, ConnectionScope::All);
    }

    #[test]
    fn connections_all_env_list() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let list = format!("{a},{b}");
        let p = McpPolicy::default().narrowed_by_env([("SQLLY_MCP_CONNECTIONS", list.as_str())]);
        assert_eq!(p.connections, ConnectionScope::Allow(vec![a, b]));
    }

    #[test]
    fn connections_allow_intersect() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        let list = format!("{b},{c}");
        let p = McpPolicy {
            connections: ConnectionScope::Allow(vec![a, b]),
            ..Default::default()
        }
        .narrowed_by_env([("SQLLY_MCP_CONNECTIONS", list.as_str())]);
        assert_eq!(p.connections, ConnectionScope::Allow(vec![b]));
    }

    #[test]
    fn connections_allow_env_unparseable() {
        let a = Uuid::new_v4();
        let p = McpPolicy {
            connections: ConnectionScope::Allow(vec![a]),
            ..Default::default()
        }
        .narrowed_by_env([("SQLLY_MCP_CONNECTIONS", "zzz")]);
        assert_eq!(p.connections, ConnectionScope::Allow(vec![a]));
    }

    #[test]
    fn max_rows_clamped_to_hard_cap() {
        let p = McpPolicy {
            max_rows: 500,
            ..Default::default()
        }
        .clamped();
        assert_eq!(p.max_rows, MAX_ROWS_HARD_CAP);
    }

    #[test]
    fn max_rows_env_narrows() {
        let p = McpPolicy::default().narrowed_by_env([("SQLLY_MCP_MAX_ROWS", "50")]);
        assert_eq!(p.max_rows, 50);
    }

    #[test]
    fn max_rows_env_cannot_widen() {
        let p = McpPolicy {
            max_rows: 100,
            ..Default::default()
        }
        .narrowed_by_env([("SQLLY_MCP_MAX_ROWS", "1000")]);
        assert_eq!(p.max_rows, 100);
    }

    #[test]
    fn idle_timeout_minimum() {
        let p = McpPolicy {
            idle_timeout_secs: 5,
            ..Default::default()
        }
        .clamped();
        assert_eq!(p.idle_timeout_secs, MIN_IDLE_TIMEOUT_SECS);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn missing_file_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.json");
        let p = load_policy_from(&path);
        assert_eq!(p.mode, McpMode::ReadOnly);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn invalid_json_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "{not json").unwrap();
        let p = load_policy_from(&path);
        assert_eq!(p.mode, McpMode::ReadOnly);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("policy.json");
        let policy = McpPolicy {
            mode: McpMode::ReadWriteConfirm,
            connections: ConnectionScope::Allow(vec![Uuid::new_v4()]),
            max_rows: 42,
            ..Default::default()
        };
        save_policy_to(&path, &policy).unwrap();
        let loaded = load_policy_from(&path);
        assert_eq!(loaded.mode, McpMode::ReadWriteConfirm);
        assert_eq!(loaded.max_rows, 42);
    }

    #[test]
    fn allows_connection() {
        let id = Uuid::new_v4();
        let p = McpPolicy {
            connections: ConnectionScope::Allow(vec![id]),
            ..Default::default()
        };
        assert!(p.allows_connection(id));
        assert!(!p.allows_connection(Uuid::new_v4()));
    }

    #[test]
    fn defaults_have_rate_limits() {
        let p = McpPolicy::default();
        assert_eq!(p.max_queries_per_minute, 60);
        assert_eq!(p.max_writes_per_minute, 5);
        assert_eq!(p.max_ddl_per_minute, 2);
    }

    #[test]
    fn env_narrows_rate_limits_only_down() {
        let p = McpPolicy::default().narrowed_by_env([
            ("SQLLY_MCP_MAX_QUERIES_PER_MINUTE", "10"),
            ("SQLLY_MCP_MAX_WRITES_PER_MINUTE", "1"),
            ("SQLLY_MCP_MAX_DDL_PER_MINUTE", "0"),
        ]);
        assert_eq!(p.max_queries_per_minute, 10);
        assert_eq!(p.max_writes_per_minute, 1);
        assert_eq!(p.max_ddl_per_minute, 0);

        let wide = McpPolicy {
            max_queries_per_minute: 30,
            max_writes_per_minute: 3,
            max_ddl_per_minute: 1,
            ..Default::default()
        }
        .narrowed_by_env([
            ("SQLLY_MCP_MAX_QUERIES_PER_MINUTE", "100"),
            ("SQLLY_MCP_MAX_WRITES_PER_MINUTE", "10"),
            ("SQLLY_MCP_MAX_DDL_PER_MINUTE", "5"),
        ]);
        assert_eq!(wide.max_queries_per_minute, 30);
        assert_eq!(wide.max_writes_per_minute, 3);
        assert_eq!(wide.max_ddl_per_minute, 1);
    }

    #[test]
    fn effective_mode_is_min_of_global_and_connection() {
        let id = Uuid::new_v4();
        let p = McpPolicy {
            mode: McpMode::ReadWriteConfirm,
            connection_modes: BTreeMap::from([(id, ConnectionMode::ReadOnly)]),
            ..Default::default()
        };
        assert_eq!(p.effective_mode(id), ConnectionMode::ReadOnly);
        assert_eq!(
            p.effective_mode(Uuid::new_v4()),
            ConnectionMode::ReadWriteConfirm
        );
    }

    #[test]
    fn locked_connection_is_not_allowed() {
        let id = Uuid::new_v4();
        let p = McpPolicy {
            connection_modes: BTreeMap::from([(id, ConnectionMode::Locked)]),
            ..Default::default()
        };
        assert!(!p.allows_connection(id));
        assert_eq!(p.effective_mode(id), ConnectionMode::Locked);
    }

    #[test]
    fn env_lock_list_locks() {
        let id = Uuid::new_v4();
        let list = id.to_string();
        let p =
            McpPolicy::default().narrowed_by_env([("SQLLY_MCP_LOCK_CONNECTIONS", list.as_str())]);
        assert_eq!(p.effective_mode(id), ConnectionMode::Locked);
        assert!(!p.allows_connection(id));
    }

    #[test]
    fn old_policy_file_without_new_fields_loads() {
        let json = r#"{
            "version": 1,
            "mode": "read_only",
            "connections": "all",
            "max_rows": 100,
            "cell_char_limit": 200,
            "idle_timeout_secs": 900
        }"#;
        let p: McpPolicy = serde_json::from_str::<McpPolicy>(json).unwrap().clamped();
        assert_eq!(p.max_queries_per_minute, 60);
        assert_eq!(p.max_writes_per_minute, 5);
        assert_eq!(p.max_ddl_per_minute, 2);
        assert!(p.connection_modes.is_empty());
    }
}
