//! Pure types, argv builders, and event-stream parsers for headless coding-agent
//! CLIs (Claude Code, Codex). No I/O — compiles on wasm.

mod claude_code;
mod codex;

pub use claude_code::parse_line as parse_claude_line;
pub use codex::parse_line as parse_codex_line;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CliAgentKind {
    ClaudeCode,
    Codex,
}

impl CliAgentKind {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    pub fn vendor(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Anthropic",
            Self::Codex => "OpenAI",
        }
    }

    pub fn default_exe_name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }

    #[cfg(not(target_os = "windows"))]
    pub fn exe_candidates(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCode => &["claude"],
            Self::Codex => &["codex"],
        }
    }

    #[cfg(target_os = "windows")]
    pub fn exe_candidates(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCode => &["claude.exe", "claude.cmd", "claude"],
            Self::Codex => &["codex.exe", "codex.cmd", "codex"],
        }
    }

    pub fn provider_id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "cli:claude-code",
            Self::Codex => "cli:codex",
        }
    }
}

/// Vendor-neutral event the transcript renders. Maps onto plan 172's `AgentEvent`.
#[derive(Debug, Clone, PartialEq)]
pub enum CliAgentEvent {
    Started {
        session_id: Option<String>,
        mcp_ok: Option<bool>,
    },
    Turn {
        n: usize,
    },
    AssistantText(String),
    ToolCall {
        id: Option<String>,
        name: String,
        arguments_json: String,
    },
    ToolResult {
        id: Option<String>,
        is_error: bool,
        content: String,
    },
    Final {
        text: String,
        is_error: bool,
    },
    Ignored,
}

pub const MAX_EVENT_BYTES: usize = 32 * 1024;
pub const MAX_TOTAL_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: std::path::PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin_prompt: Option<String>,
    pub cwd: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
}

const ENV_KEY_DENY_EXACT: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "NODE_OPTIONS",
    "PYTHONPATH",
    "DATABASE_URL",
];

const ENV_KEY_DENY_PREFIXES: &[&str] = &[
    "SQLLY_",
    "PG",
    "MYSQL",
    "MSSQL",
    "SQLCONNSTR_",
    "AWS_SECRET",
];

/// Keys removed from the child environment before spawn (exact + prefix rules).
pub const SCRUBBED_ENV_KEYS: &[&str] = &[
    "PATH",
    "HOME",
    "USERPROFILE",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "NODE_OPTIONS",
    "PYTHONPATH",
    "PGPASSWORD",
    "MYSQL_PWD",
    "DATABASE_URL",
    "SQLLY_KEYRING_PASSWORD",
];

pub const SCRUBBED_ENV_PREFIXES: &[&str] = ENV_KEY_DENY_PREFIXES;

pub fn validate_env_key(key: &str) -> Result<(), String> {
    if key.len() > 64 {
        return Err(format!("environment key `{key}` exceeds 64 characters"));
    }
    if !key
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        return Err(format!(
            "environment key `{key}` must start with a letter or underscore"
        ));
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "environment key `{key}` may only contain letters, digits, and underscores"
        ));
    }
    if ENV_KEY_DENY_EXACT.contains(&key) {
        return Err(format!("environment key `{key}` is not allowed"));
    }
    if ENV_KEY_DENY_PREFIXES
        .iter()
        .any(|prefix| key.starts_with(prefix))
    {
        return Err(format!(
            "environment key `{key}` matches a blocked prefix (database credentials, loader hijacks, or SQLLY internals)"
        ));
    }
    Ok(())
}

pub fn build_launch(
    kind: CliAgentKind,
    exe: &std::path::Path,
    mcp_config_path: &std::path::Path,
    mcp: &McpServerSpec,
    prompt: &str,
    extra_args: &[String],
    max_turns: usize,
) -> LaunchSpec {
    let args = match kind {
        CliAgentKind::ClaudeCode => build_claude_argv(mcp_config_path, max_turns, extra_args),
        CliAgentKind::Codex => build_codex_argv(mcp, extra_args),
    };
    LaunchSpec {
        program: exe.to_path_buf(),
        args,
        env: Vec::new(),
        stdin_prompt: Some(prompt.to_string()),
        cwd: None,
    }
}

fn build_claude_argv(
    mcp_config_path: &std::path::Path,
    max_turns: usize,
    extra_args: &[String],
) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        "--max-turns".to_string(),
        max_turns.to_string(),
        "--permission-mode".to_string(),
        "dontAsk".to_string(),
        "--strict-mcp-config".to_string(),
        "--mcp-config".to_string(),
        mcp_config_path.to_string_lossy().into_owned(),
        "--allowedTools".to_string(),
        "mcp__sqlly__*".to_string(),
        "--disallowedTools".to_string(),
        "Bash".to_string(),
        "Edit".to_string(),
        "Write".to_string(),
        "NotebookEdit".to_string(),
        "WebFetch".to_string(),
        "WebSearch".to_string(),
        "Agent".to_string(),
    ];
    args.extend(extra_args.iter().cloned());
    args
}

fn build_codex_argv(mcp: &McpServerSpec, extra_args: &[String]) -> Vec<String> {
    let args_json = toml_array(&mcp.args);
    let mut args = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--ephemeral".to_string(),
        "--skip-git-repo-check".to_string(),
        "--sandbox".to_string(),
        "read-only".to_string(),
        "-c".to_string(),
        "approval_policy=\"never\"".to_string(),
        "-c".to_string(),
        format!(
            "mcp_servers.{}.command={}",
            mcp.name,
            toml_string(&mcp.command)
        ),
        "-c".to_string(),
        format!("mcp_servers.{}.args={}", mcp.name, args_json),
        "-c".to_string(),
        format!("mcp_servers.{}.startup_timeout_sec=30", mcp.name),
        "-c".to_string(),
        format!("mcp_servers.{}.tool_timeout_sec=120", mcp.name),
    ];
    args.extend(extra_args.iter().cloned());
    args.push("-".to_string());
    args
}

pub fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn toml_array(values: &[String]) -> String {
    let inner = values
        .iter()
        .map(|value| toml_string(value))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

pub fn cap_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (truncated)", &text[..end])
}

pub fn cli_agent_egress_message(kind: CliAgentKind) -> sqlly_lang::DisplayMessage {
    let key = match kind {
        CliAgentKind::ClaudeCode => "services.ai.agent_cli.egress_claude",
        CliAgentKind::Codex => "services.ai.agent_cli.egress_codex",
    };
    sqlly_lang::DisplayMessage::new(key, cli_agent_egress_label(kind))
}

pub fn cli_agent_egress_label(kind: CliAgentKind) -> String {
    format!(
        "Agent mode runs a local {} CLI that talks to {}'s cloud; table names, column lists, sample rows and SELECT results pass through it.",
        kind.display_name(),
        kind.vendor()
    )
}

pub fn parse_events_for_kind(kind: CliAgentKind, line: &str) -> Result<Vec<CliAgentEvent>, String> {
    match kind {
        CliAgentKind::ClaudeCode => claude_code::parse_line(line),
        CliAgentKind::Codex => Ok(codex::parse_line(line)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_key_rejects_db_and_loader_keys() {
        let cases = [
            ("PATH", false),
            ("HOME", false),
            ("LD_PRELOAD", false),
            ("DYLD_INSERT_LIBRARIES", false),
            ("NODE_OPTIONS", false),
            ("PGPASSWORD", false),
            ("PGHOST", false),
            ("MYSQL_PWD", false),
            ("MYSQL_HOST", false),
            ("MSSQLSERVER", false),
            ("SQLCONNSTR_ODBC", false),
            ("AWS_SECRET_ACCESS_KEY", false),
            ("DATABASE_URL", false),
            ("SQLLY_ENGINE_LOG", false),
            ("ANTHROPIC_MODEL", true),
            ("CODEX_HOME", true),
            ("HTTPS_PROXY", true),
        ];
        for (key, ok) in cases {
            let result = validate_env_key(key).is_ok();
            assert_eq!(result, ok, "key={key}");
        }
    }

    #[test]
    fn env_key_accepts_vendor_keys() {
        for key in ["ANTHROPIC_MODEL", "CODEX_HOME", "HTTPS_PROXY"] {
            validate_env_key(key).unwrap_or_else(|e| panic!("{key}: {e}"));
        }
    }

    #[test]
    fn claude_argv_is_stable() {
        let mcp = McpServerSpec {
            name: "sqlly".to_string(),
            command: "/usr/bin/sqlly-cli".to_string(),
            args: vec!["mcp".to_string()],
        };
        let spec = build_launch(
            CliAgentKind::ClaudeCode,
            std::path::Path::new("/bin/claude"),
            std::path::Path::new("/tmp/mcp.json"),
            &mcp,
            "secret prompt",
            &[],
            30,
        );
        let expected = [
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--max-turns",
            "30",
            "--permission-mode",
            "dontAsk",
            "--strict-mcp-config",
            "--mcp-config",
            "/tmp/mcp.json",
            "--allowedTools",
            "mcp__sqlly__*",
            "--disallowedTools",
            "Bash",
            "Edit",
            "Write",
            "NotebookEdit",
            "WebFetch",
            "WebSearch",
            "Agent",
        ];
        assert_eq!(
            spec.args.iter().map(String::as_str).collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn codex_argv_is_stable() {
        let mcp = McpServerSpec {
            name: "sqlly".to_string(),
            command: "/usr/bin/sqlly-cli".to_string(),
            args: vec!["mcp".to_string()],
        };
        let spec = build_launch(
            CliAgentKind::Codex,
            std::path::Path::new("/bin/codex"),
            std::path::Path::new("/tmp/mcp.json"),
            &mcp,
            "secret prompt",
            &[],
            30,
        );
        let expected = vec![
            "exec",
            "--json",
            "--ephemeral",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "-c",
            "approval_policy=\"never\"",
            "-c",
            "mcp_servers.sqlly.command=\"/usr/bin/sqlly-cli\"",
            "-c",
            "mcp_servers.sqlly.args=[\"mcp\"]",
            "-c",
            "mcp_servers.sqlly.startup_timeout_sec=30",
            "-c",
            "mcp_servers.sqlly.tool_timeout_sec=120",
            "-",
        ];
        assert_eq!(
            spec.args.iter().map(String::as_str).collect::<Vec<_>>(),
            expected
        );
    }

    #[test]
    fn toml_string_escapes_quotes_and_backslashes() {
        assert_eq!(toml_string(r#"a\b"c"#), r#""a\\b\"c""#);
    }

    #[test]
    fn prompt_never_appears_in_argv() {
        let prompt = "this is the user secret prompt with spaces";
        let mcp = McpServerSpec {
            name: "sqlly".to_string(),
            command: "sqlly-cli".to_string(),
            args: vec!["mcp".to_string()],
        };
        for kind in [CliAgentKind::ClaudeCode, CliAgentKind::Codex] {
            let spec = build_launch(
                kind,
                std::path::Path::new("/bin/agent"),
                std::path::Path::new("/tmp/mcp.json"),
                &mcp,
                prompt,
                &[],
                10,
            );
            assert!(
                !spec.args.iter().any(|arg| arg.contains(prompt)),
                "{kind:?} leaked prompt into argv"
            );
            assert_eq!(spec.stdin_prompt.as_deref(), Some(prompt));
        }
    }
}
