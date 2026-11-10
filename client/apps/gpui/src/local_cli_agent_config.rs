//! Preferences for the local coding-agent CLI provider. Non-secret fields persist
//! in `cli-agent.json`; extra environment values live in the keyring only.

use sqlly_ai::catalog::{CliAgentKind, validate_env_key};

const KEYCHAIN_SERVICE: &str = crate::distribution::keychain_service!("SQLLY.cli-agent-env");

fn env_account_key(kind: CliAgentKind) -> String {
    format!("cli-agent.{kind:?}.env")
}

fn env_cache() -> &'static parking_lot::Mutex<std::collections::BTreeMap<String, String>> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::BTreeMap<String, String>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

pub fn load_cli_agent_env(kind: CliAgentKind) -> Result<Option<String>, String> {
    let account = env_account_key(kind);
    if let Some(value) = env_cache().lock().get(&account).cloned() {
        return Ok(Some(value));
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let entry =
            keyring::Entry::new(KEYCHAIN_SERVICE, &account).map_err(|error| error.to_string())?;
        match entry.get_password() {
            Ok(value) => {
                env_cache().lock().insert(account, value.clone());
                Ok(Some(value))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::secrets::get(KEYCHAIN_SERVICE, &account)
    }
}

pub fn store_cli_agent_env(kind: CliAgentKind, env: &str) -> Result<(), String> {
    if env.is_empty() {
        return delete_cli_agent_env(kind);
    }
    for line in env.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, _value)) = line.split_once('=') else {
            return Err(crate::localization::format(
                "settings.ai.coding_agent_cli.error.invalid_environment_on_save",
                &[("line", line)],
            ));
        };
        validate_env_key(key.trim())?;
    }
    let account = env_account_key(kind);
    #[cfg(not(target_arch = "wasm32"))]
    {
        keyring::Entry::new(KEYCHAIN_SERVICE, &account)
            .map_err(|error| error.to_string())?
            .set_password(env)
            .map_err(|error| error.to_string())?;
        env_cache().lock().insert(account, env.to_string());
        Ok(())
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::secrets::set(KEYCHAIN_SERVICE, &account, env)?;
        env_cache().lock().insert(account, env.to_string());
        Ok(())
    }
}

pub fn delete_cli_agent_env(kind: CliAgentKind) -> Result<(), String> {
    let account = env_account_key(kind);
    env_cache().lock().remove(&account);
    #[cfg(not(target_arch = "wasm32"))]
    {
        match keyring::Entry::new(KEYCHAIN_SERVICE, &account)
            .map_err(|error| error.to_string())?
            .delete_credential()
        {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::secrets::delete(KEYCHAIN_SERVICE, &account)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LocalCliAgentConfig {
    pub enabled: bool,
    pub kind: Option<CliAgentKind>,
    pub exe_path: String,
    /// KEY names only; values live in the keyring under `cli-agent.<kind>.env`.
    pub extra_env_keys: Vec<String>,
    pub extra_args: String,
    pub max_turns: usize,
    pub timeout_minutes: u64,
}

impl LocalCliAgentConfig {
    pub fn clamped_max_turns(&self) -> usize {
        self.max_turns.clamp(5, 100).max(5)
    }

    pub fn clamped_timeout_minutes(&self) -> u64 {
        self.timeout_minutes.clamp(1, 60).max(1)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn config_path() -> std::path::PathBuf {
    dirs::data_dir()
        .map(|dir| dir.join("SQLLY").join("cli-agent.json"))
        .unwrap_or_else(|| std::path::PathBuf::from("cli-agent.json"))
}

pub fn load_local_cli_agent_config() -> LocalCliAgentConfig {
    #[cfg(target_arch = "wasm32")]
    {
        return crate::host_storage::load_text(
            crate::host_storage::StorageArea::Data,
            "cli-agent.json",
        )
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        match std::fs::read_to_string(config_path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => LocalCliAgentConfig::default(),
        }
    }
}

pub fn save_local_cli_agent_config(config: &LocalCliAgentConfig) -> Result<(), String> {
    for key in &config.extra_env_keys {
        validate_env_key(key)?;
    }
    if let Some(kind) = config.kind
        && let Some(env) = load_cli_agent_env(kind)?
    {
        for line in env.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, _)) = line.split_once('=') else {
                return Err(crate::localization::format(
                    "settings.ai.coding_agent_cli.error.invalid_stored_environment",
                    &[("line", line)],
                ));
            };
            validate_env_key(key.trim())?;
        }
    }
    let text =
        crate::config_version::to_string_pretty(config).map_err(|error| error.to_string())?;
    #[cfg(test)]
    assert!(
        !text.contains("extra_env_values") && !text.contains("api_key"),
        "secret field leaked into serialized config"
    );
    #[cfg(target_arch = "wasm32")]
    {
        return crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            "cli-agent.json",
            &text,
        );
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(parent) = config_path().parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(config_path(), text).map_err(|error| error.to_string())?;
        Ok(())
    }
}

pub fn parse_extra_env_blob(env: &str) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::new();
    for line in env.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(crate::localization::format(
                "settings.ai.coding_agent_cli.error.invalid_environment_on_parse",
                &[("line", line)],
            ));
        };
        let key = key.trim().to_string();
        validate_env_key(&key)?;
        pairs.push((key, value.to_string()));
    }
    Ok(pairs)
}

pub fn split_extra_args(args: &str) -> Vec<String> {
    args.split_whitespace().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn use_mock_keyring() {
        keyring::mock::install();
    }

    #[test]
    fn config_round_trips_without_env_values() {
        use_mock_keyring();
        let config = LocalCliAgentConfig {
            enabled: true,
            kind: Some(CliAgentKind::ClaudeCode),
            exe_path: "/bin/claude".to_string(),
            extra_env_keys: vec!["ANTHROPIC_MODEL".to_string()],
            extra_args: "--foo".to_string(),
            max_turns: 30,
            timeout_minutes: 10,
        };
        store_cli_agent_env(CliAgentKind::ClaudeCode, "ANTHROPIC_MODEL=secret-model").unwrap();
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("secret-model"));
        let back: LocalCliAgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.extra_env_keys, config.extra_env_keys);
        assert!(back.exe_path.contains("claude"));
    }

    #[test]
    fn save_rejects_invalid_env_key() {
        let config = LocalCliAgentConfig {
            extra_env_keys: vec!["PGPASSWORD".to_string()],
            ..Default::default()
        };
        assert!(save_local_cli_agent_config(&config).is_err());
    }

    #[test]
    fn keyring_round_trip_for_env_blob() {
        use_mock_keyring();
        store_cli_agent_env(CliAgentKind::Codex, "CODEX_HOME=/tmp/codex").unwrap();
        let loaded = load_cli_agent_env(CliAgentKind::Codex).unwrap();
        assert_eq!(loaded.as_deref(), Some("CODEX_HOME=/tmp/codex"));
    }
}
