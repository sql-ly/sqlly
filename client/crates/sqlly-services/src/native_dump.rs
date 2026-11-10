//! Native pg_dump/pg_restore/mysqldump/mysql integration — argv builders,
//! credential handling, spawn/stream/cancel. Credentials never appear on argv.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use sqlly_core::connection::{AuthenticationType, ConnectionProfile};
use sqlly_core::{DatabaseType, StudioError, StudioResult};
use tempfile::NamedTempFile;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::dump::{DumpPhase, DumpProgress, DumpReport};
use crate::dump_render::{DumpContent, DumpOptions};
use crate::identifier_quoting::qualified_identifier;
use crate::services::Services;
use crate::tool_discovery::{NativeTool, ToolLocation};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PgFormat {
    Custom,
    Plain,
    Tar,
}

impl PgFormat {
    fn flag(self) -> &'static str {
        match self {
            PgFormat::Custom => "c",
            PgFormat::Plain => "p",
            PgFormat::Tar => "t",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeDumpRequest {
    pub database: String,
    pub output: PathBuf,
    pub options: DumpOptions,
    /// Explicit table list (schema, table); None = whole database.
    pub tables: Option<Vec<(String, String)>>,
    pub pg_format: PgFormat,
    pub pg_no_owner: bool,
    pub mysql_single_transaction: bool,
    /// Extra mysqldump options the user typed, already checked by
    /// [`parse_mysqldump_extra_args`]; each is its own argv entry.
    pub mysql_extra_args: Vec<String>,
    /// Remove `DEFINER=` clauses from the finished mysqldump file.
    pub mysql_strip_definer: bool,
}

/// Everything needed to spawn, minus the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub stdin_file: Option<PathBuf>,
    /// When set, the app creates this file and hands it to the tool as its
    /// stdout (see [`passes_files_by_handle`]).
    pub stdout_file: Option<PathBuf>,
}

/// Whether the tool should get its input/output files as open handles from
/// the app rather than as paths. True for tools bundled inside the app: only
/// the sandboxed Mac App Store edition bundles them, and a sandboxed helper
/// may not be allowed to open a path the user granted to the app itself — an
/// inherited handle always works.
pub fn passes_files_by_handle(tool: &ToolLocation) -> bool {
    tool.source == crate::tool_discovery::ToolSource::Bundled
}

/// Pure: drop the output-path arguments from a dump command so the tool
/// writes the dump to stdout (pg_dump `-f <path>`, mysqldump
/// `--result-file=<path>`).
pub fn strip_output_path(tool: NativeTool, args: &mut Vec<String>) {
    match tool {
        NativeTool::PgDump => {
            if let Some(i) = args.iter().position(|a| a == "-f") {
                let end = (i + 2).min(args.len());
                args.drain(i..end);
            }
        }
        NativeTool::MysqlDump => args.retain(|a| !a.starts_with("--result-file=")),
        _ => {}
    }
}

/// Pure: drop the trailing archive path from pg_restore arguments so it reads
/// the archive from stdin instead.
pub fn strip_restore_input(args: &mut Vec<String>, input: &Path) {
    if args.last().map(String::as_str) == Some(&*input.display().to_string()) {
        args.pop();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreRequest {
    pub database: String,
    pub input: PathBuf,
    pub clean: bool,
    pub if_exists: bool,
    pub single_transaction: bool,
    pub no_owner: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreKind {
    PgArchive,
    PlainSql,
}

pub fn pg_dump_args(req: &NativeDumpRequest, ep: &Endpoint) -> Vec<String> {
    let mut args = vec![
        "-h".into(),
        ep.host.clone(),
        "-p".into(),
        ep.port.to_string(),
        "-U".into(),
        ep.user.clone(),
        "-w".into(),
        "-F".into(),
        req.pg_format.flag().into(),
        "-f".into(),
        req.output.display().to_string(),
    ];
    match req.options.content {
        DumpContent::StructureOnly => args.push("--schema-only".into()),
        DumpContent::DataOnly => args.push("--data-only".into()),
        DumpContent::StructureAndData => {}
    }
    if req.pg_no_owner {
        args.push("--no-owner".into());
    }
    if req.options.drop_if_exists
        && req.pg_format == PgFormat::Plain
        && req.options.content != DumpContent::DataOnly
    {
        args.push("--clean".into());
        args.push("--if-exists".into());
    }
    if let Some(schemas) = &req.options.schemas {
        for schema in schemas {
            // `-n` takes a pattern; quoting makes the name literal (no `*`/`?`
            // wildcards, case preserved).
            args.push("-n".into());
            args.push(DatabaseType::Postgres.quote_identifier(schema));
        }
    }
    if let Some(tables) = &req.tables {
        for (schema, table) in tables {
            args.push("-t".into());
            args.push(qualified_identifier(
                DatabaseType::Postgres,
                Some(schema),
                table,
            ));
        }
    }
    args.push(pg_dbname_arg(&req.database));
    args
}

/// `--dbname=…` for pg_dump / pg_restore. libpq reads a dbname that contains
/// `=` or starts with a `postgres://` / `postgresql://` URI as a whole
/// connection string — a hostile database name could redirect the tool (with
/// `PGPASSWORD` in its environment) to another host. Such names are wrapped
/// as `dbname='…'` (backslash-escaped), which libpq never expands again; the
/// `--dbname=` form also keeps a leading `-` from reading as an option.
pub fn pg_dbname_arg(database: &str) -> String {
    let lower = database.trim_start().to_ascii_lowercase();
    let expands = database.contains('=')
        || lower.starts_with("postgres://")
        || lower.starts_with("postgresql://");
    if expands {
        let escaped = database.replace('\\', "\\\\").replace('\'', "\\'");
        format!("--dbname=dbname='{escaped}'")
    } else {
        format!("--dbname={database}")
    }
}

pub fn mysqldump_args(
    req: &NativeDumpRequest,
    ep: &Endpoint,
    defaults_extra_file: &Path,
) -> Vec<String> {
    let mut args = vec![format!(
        "--defaults-extra-file={}",
        defaults_extra_file.display()
    )];
    args.extend([
        "-h".into(),
        ep.host.clone(),
        "-P".into(),
        ep.port.to_string(),
        "--protocol=TCP".into(),
        format!("--result-file={}", req.output.display()),
    ]);
    if req.mysql_single_transaction {
        args.push("--single-transaction".into());
    }
    if req.options.include_routines {
        args.push("--routines".into());
    }
    if req.options.include_triggers {
        args.push("--triggers".into());
    } else {
        args.push("--skip-triggers".into());
    }
    match req.options.content {
        DumpContent::StructureOnly => args.push("--no-data".into()),
        DumpContent::DataOnly => args.push("--no-create-info".into()),
        DumpContent::StructureAndData => {}
    }
    if req.options.drop_if_exists {
        args.push("--add-drop-table".into());
    }
    args.extend(req.mysql_extra_args.iter().cloned());
    // End option parsing: the database and table names that follow are
    // positional, so a name starting with `-` can never read as a flag.
    args.push("--".into());
    args.push(req.database.clone());
    if let Some(tables) = &req.tables {
        for (_, table) in tables {
            args.push(table.clone());
        }
    }
    args
}

/// mysqldump options SQLLY sets itself or that would redirect where the dump
/// goes, which credentials it uses, or which server it reaches. Matched after
/// `_` → `-`, lower-casing and dropping MySQL's `loose-` / `skip-` /
/// `disable-` / `enable-` / `maximum-` modifiers, and also when the typed
/// name is a prefix of one of these (MySQL tools accept unambiguous
/// prefixes, so `--res=` would mean `--result-file=`).
const RESERVED_MYSQLDUMP_OPTIONS: &[&str] = &[
    "result-file",
    "defaults-file",
    "defaults-extra-file",
    "defaults-group-suffix",
    "no-defaults",
    "print-defaults",
    "login-path",
    "no-login-paths",
    "user",
    "password",
    "password1",
    "password2",
    "password3",
    "host",
    "port",
    "socket",
    "protocol",
    "pipe",
    "shared-memory-base-name",
    "tab",
    "plugin-dir",
    "default-auth",
];

const MYSQL_OPTION_MODIFIERS: &[&str] = &["loose-", "skip-", "disable-", "enable-", "maximum-"];

/// Split the extra mysqldump options typed in the dump dialog into argv
/// entries and check each one. Splitting follows familiar shell quoting
/// (whitespace separates; `'…'` is literal; `"…"` allows `\"` and `\\`) but no
/// shell ever runs, so `$(…)`, `;` and `|` are plain text. Every entry must
/// be a `--long-option[=value]`: positional arguments would add databases
/// and short options can hide reserved ones, so both are refused, as are the
/// options in [`RESERVED_MYSQLDUMP_OPTIONS`].
pub fn parse_mysqldump_extra_args(input: &str) -> Result<Vec<String>, sqlly_core::DisplayMessage> {
    let words = split_option_words(input).ok_or_else(|| {
        sqlly_core::DisplayMessage::new(
            "services.native_dump.extra_option_unclosed_quote",
            "A quote in the extra mysqldump options is not closed",
        )
    })?;
    validate_mysqldump_extra_args(&words)?;
    Ok(words)
}

/// Check already-separated extra mysqldump options (the CLI's repeated
/// `--mysqldump-option` values) by the rules of [`parse_mysqldump_extra_args`].
pub fn validate_mysqldump_extra_args(args: &[String]) -> Result<(), sqlly_core::DisplayMessage> {
    args.iter().try_for_each(|arg| check_mysqldump_option(arg))
}

fn split_option_words(input: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '\'' => break,
                        other => current.push(other),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            escaped @ ('"' | '\\') => current.push(escaped),
                            other => {
                                current.push('\\');
                                current.push(other);
                            }
                        },
                        other => current.push(other),
                    }
                }
            }
            other => {
                in_word = true;
                current.push(other);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    Some(words)
}

fn check_mysqldump_option(word: &str) -> Result<(), sqlly_core::DisplayMessage> {
    let not_an_option = || {
        sqlly_core::DisplayMessage::new(
            "services.native_dump.extra_option_not_an_option",
            format!("Each extra mysqldump option must be a long option starting with -- ({word})"),
        )
        .with_arg("option", word)
    };
    let body = word.strip_prefix("--").ok_or_else(not_an_option)?;
    let name = body.split_once('=').map_or(body, |(name, _)| name);
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(not_an_option());
    }
    let mut name = name.replace('_', "-").to_ascii_lowercase();
    while let Some(rest) = MYSQL_OPTION_MODIFIERS
        .iter()
        .find_map(|modifier| name.strip_prefix(modifier))
    {
        name = rest.to_string();
    }
    if RESERVED_MYSQLDUMP_OPTIONS
        .iter()
        .any(|reserved| reserved.starts_with(name.as_str()))
    {
        return Err(sqlly_core::DisplayMessage::new(
            "services.native_dump.extra_option_reserved",
            format!("SQLLY sets {word} itself, so it cannot be an extra mysqldump option"),
        )
        .with_arg("option", word));
    }
    Ok(())
}

pub fn pg_restore_args(req: &RestoreRequest, ep: &Endpoint) -> Vec<String> {
    let mut args = vec![
        "-h".into(),
        ep.host.clone(),
        "-p".into(),
        ep.port.to_string(),
        "-U".into(),
        ep.user.clone(),
        "-w".into(),
    ];
    if req.clean {
        args.push("--clean".into());
        if req.if_exists {
            args.push("--if-exists".into());
        }
    }
    if req.single_transaction {
        args.push("--single-transaction".into());
    }
    if req.no_owner {
        args.push("--no-owner".into());
    }
    args.push(pg_dbname_arg(&req.database));
    args.push(req.input.display().to_string());
    args
}

pub fn mysql_restore_args(
    req: &RestoreRequest,
    ep: &Endpoint,
    defaults_extra_file: &Path,
) -> Vec<String> {
    vec![
        format!("--defaults-extra-file={}", defaults_extra_file.display()),
        "-h".into(),
        ep.host.clone(),
        "-P".into(),
        ep.port.to_string(),
        "--protocol=TCP".into(),
        // `--database=` binds the name to the option, so a name starting
        // with `-` cannot inject a flag.
        format!("--database={}", req.database),
    ]
}

/// Classify a backup file from its first bytes: a `PGDMP` custom archive or a
/// POSIX tar (`ustar` magic at offset 257) is a PostgreSQL archive, anything
/// else is plain SQL. Safe on any prefix length, including a file that ends
/// inside the tar magic.
pub fn sniff_restore_kind(head: &[u8]) -> RestoreKind {
    if head.starts_with(b"PGDMP") || head.get(257..262) == Some(b"ustar".as_slice()) {
        RestoreKind::PgArchive
    } else {
        RestoreKind::PlainSql
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgressLine {
    Progress { phase: DumpPhase, object: String },
    Warning(String),
    Ignore,
}

pub fn parse_progress_line(tool: NativeTool, line: &str) -> ProgressLine {
    let lower = line.to_lowercase();
    if lower.contains("error") || line.starts_with("ERROR") {
        return ProgressLine::Warning(line.trim().to_string());
    }
    match tool {
        NativeTool::PgDump => {
            if let Some(rest) = line.strip_prefix("pg_dump: dumping contents of table ") {
                return ProgressLine::Progress {
                    phase: DumpPhase::Data,
                    object: rest.trim_matches('"').to_string(),
                };
            }
            if let Some(rest) = line.strip_prefix("pg_dump: processing item ") {
                return ProgressLine::Progress {
                    phase: DumpPhase::Structure,
                    object: rest.to_string(),
                };
            }
        }
        NativeTool::PgRestore => {
            if let Some(rest) = line.strip_prefix("pg_restore: creating TABLE ") {
                return ProgressLine::Progress {
                    phase: DumpPhase::Structure,
                    object: rest.trim_matches('"').to_string(),
                };
            }
            if let Some(rest) = line.strip_prefix("pg_restore: processing item ") {
                return ProgressLine::Progress {
                    phase: DumpPhase::Structure,
                    object: rest.to_string(),
                };
            }
        }
        NativeTool::MysqlDump => {
            if let Some(rest) = line.strip_prefix("-- Retrieving table structure for table ") {
                return ProgressLine::Progress {
                    phase: DumpPhase::Structure,
                    object: rest.trim_end_matches("...").to_string(),
                };
            }
            if let Some(rest) = line.strip_prefix("-- Sending SELECT query...") {
                let _ = rest;
                return ProgressLine::Progress {
                    phase: DumpPhase::Data,
                    object: "table data".into(),
                };
            }
        }
        _ => {}
    }
    ProgressLine::Ignore
}

fn resolve_credential(profile: &ConnectionProfile) -> StudioResult<String> {
    match &profile.authentication {
        AuthenticationType::SqlLogin { secret_ref, .. } => secret_ref.resolve(),
        AuthenticationType::AwsRdsIam { .. } | AuthenticationType::GcpCloudSqlIam { .. } => profile
            .runtime_cloud_iam_token
            .clone()
            .ok_or_else(|| {
                StudioError::AuthenticationMessage(sqlly_core::DisplayMessage::new(
                    "services.cloud_auth.iam_token_missing",
                    "Authentication failed: cloud IAM token missing; connect from SQLLY (or pass --access-token / SQLLY_CLOUD_IAM_TOKEN for the CLI)",
                ))
            }),
        _ => Err(StudioError::AuthenticationMessage(
            sqlly_core::DisplayMessage::new(
                "services.native_dump.authentication_unsupported",
                "Authentication failed: authentication type not supported for native database tools",
            ),
        )),
    }
}

/// Vendor tools dial the database themselves, so they need a network path the
/// local machine can reach. A relay-routed profile's host is the bridge side's
/// address, which pg_dump/mysqldump here cannot reach — refuse up front with a
/// clear reason instead of a confusing connect timeout.
fn refuse_relay_route(profile: &ConnectionProfile) -> StudioResult<()> {
    if matches!(
        profile.route,
        sqlly_core::connection::ConnectionRoute::ViaRelay { .. }
    ) {
        return Err(StudioError::InvalidProfileMessage(
            sqlly_core::DisplayMessage::new(
                "services.native_dump.relay_route_unsupported",
                "Invalid connection profile: native backup and restore tools need a direct connection; this connection is reached through a relay bridge",
            ),
        ));
    }
    Ok(())
}

/// Create the directory a native dump writes into (the vendor tool will not).
fn ensure_output_parent(output: &Path) -> StudioResult<()> {
    if let Some(parent) = output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(StudioError::Io)?;
    }
    Ok(())
}

async fn endpoint_for(profile: &ConnectionProfile) -> StudioResult<Endpoint> {
    refuse_relay_route(profile)?;
    let (host, port) = sqlly_engines::ssh::TunnelManager::global()
        .endpoint_for(profile)
        .await?;
    let user = profile
        .authentication
        .username()
        .ok_or_else(|| {
            StudioError::InvalidProfileMessage(sqlly_core::DisplayMessage::new(
                "services.native_dump.username_required",
                "username required",
            ))
        })?
        .to_string();
    Ok(Endpoint { host, port, user })
}

/// The `[client]` option file carrying the user name and password. Both go
/// here rather than on the command line: on Windows MySQL's tools decode
/// their arguments in the ANSI code page, which mangles a non-ASCII user
/// name, while the option file is read as the UTF-8 we write. MySQL parses option
/// files line by line: a raw newline would inject further options and an
/// unquoted `#` starts a comment. The value is therefore wrapped in quotes
/// (the parser strips one matching pair and ignores `#` inside it) — `"`
/// unless the password contains `"` but no `'` — and `\\`, newline, CR and
/// tab are written as the escape sequences the parser decodes. Other
/// characters, quotes included, pass through literally (the parser keeps an
/// unknown `\x` as two characters, so quotes must not be backslash-escaped).
pub fn mysql_defaults_file_content(user: &str, password: &str) -> String {
    format!(
        "[client]\nuser={}\npassword={}\n",
        mysql_option_value(user),
        mysql_option_value(password)
    )
}

fn mysql_option_value(value: &str) -> String {
    let quote = if value.contains('"') && !value.contains('\'') {
        '\''
    } else {
        '"'
    };
    let mut quoted = String::with_capacity(value.len() + 2);
    for ch in value.chars() {
        match ch {
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            other => quoted.push(other),
        }
    }
    format!("{quote}{quoted}{quote}")
}

fn write_mysql_defaults_file(user: &str, password: &str) -> StudioResult<NamedTempFile> {
    let file = tempfile::Builder::new()
        .prefix("sqlly-my-")
        .tempfile()
        .map_err(StudioError::Io)?;
    std::fs::write(file.path(), mysql_defaults_file_content(user, password))
        .map_err(StudioError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))
            .map_err(StudioError::Io)?;
    }
    // Windows: temp dir ACLs are per-user.
    Ok(file)
}

fn build_dump_spec(
    _profile: &ConnectionProfile,
    tool: &ToolLocation,
    req: &NativeDumpRequest,
    ep: &Endpoint,
    password: &str,
) -> StudioResult<(CommandSpec, Option<NamedTempFile>)> {
    let mut env = Vec::new();
    let mut mysql_file = None;
    let args = match tool.tool {
        NativeTool::PgDump => {
            env.push(("PGPASSWORD".into(), password.to_string()));
            let mut a = pg_dump_args(req, ep);
            a.insert(0, "-v".into());
            a
        }
        NativeTool::MysqlDump => {
            let file = write_mysql_defaults_file(&ep.user, password)?;
            let mut a = mysqldump_args(req, ep, file.path());
            a.insert(1, "-v".into());
            mysql_file = Some(file);
            a
        }
        _ => {
            return Err(StudioError::CapabilityNotSupportedMessage(
                sqlly_core::DisplayMessage::new(
                    "services.native_dump.unsupported_dump_tool",
                    "This native tool cannot create backups",
                ),
            ));
        }
    };
    let mut args = args;
    let stdout_file = if passes_files_by_handle(tool) {
        strip_output_path(tool.tool, &mut args);
        Some(req.output.clone())
    } else {
        None
    };
    Ok((
        CommandSpec {
            program: tool.path.clone(),
            args,
            env,
            stdin_file: None,
            stdout_file,
        },
        mysql_file,
    ))
}

fn build_restore_spec(
    profile: &ConnectionProfile,
    tool: &ToolLocation,
    req: &RestoreRequest,
    ep: &Endpoint,
    password: &str,
) -> StudioResult<(CommandSpec, Option<NamedTempFile>)> {
    let mut env = Vec::new();
    let mut mysql_file = None;
    let (args, stdin_file) = match tool.tool {
        NativeTool::PgRestore => {
            env.push(("PGPASSWORD".into(), password.to_string()));
            let mut a = pg_restore_args(req, ep);
            a.insert(0, "-v".into());
            if passes_files_by_handle(tool) {
                strip_restore_input(&mut a, &req.input);
                (a, Some(req.input.clone()))
            } else {
                (a, None)
            }
        }
        NativeTool::Mysql => {
            let file = write_mysql_defaults_file(&ep.user, password)?;
            let a = mysql_restore_args(req, ep, file.path());
            mysql_file = Some(file);
            (a, Some(req.input.clone()))
        }
        _ => {
            return Err(StudioError::CapabilityNotSupportedMessage(
                sqlly_core::DisplayMessage::new(
                    "services.native_dump.unsupported_restore_tool",
                    "This native tool cannot restore backups",
                ),
            ));
        }
    };
    let _ = profile;
    Ok((
        CommandSpec {
            program: tool.path.clone(),
            args,
            env,
            stdin_file,
            stdout_file: None,
        },
        mysql_file,
    ))
}

/// Runs the vendor dump tool.
pub async fn run_native_dump(
    _services: &Services,
    profile: &ConnectionProfile,
    tool: &ToolLocation,
    req: &NativeDumpRequest,
    cancel: Arc<AtomicBool>,
    progress: &mut dyn FnMut(DumpProgress),
) -> StudioResult<DumpReport> {
    let ep = endpoint_for(profile).await?;
    let password = resolve_credential(profile)?;
    ensure_output_parent(&req.output)?;
    let (spec, mysql_file) = build_dump_spec(profile, tool, req, &ep, &password)?;
    let mut report = run_child(
        spec,
        mysql_file,
        tool.tool,
        cancel,
        progress,
        Some(req.output.clone()),
    )
    .await?;
    if req.mysql_strip_definer && tool.tool == NativeTool::MysqlDump && !report.cancelled {
        let path = req.output.clone();
        tokio::task::spawn_blocking(move || {
            crate::mysql_definer::rewrite_definers_in_file(
                &path,
                crate::mysql_definer::DefinerMode::Strip,
            )
        })
        .await
        .map_err(|e| StudioError::Io(std::io::Error::other(e.to_string())))?
        .map_err(StudioError::Io)?;
        if let Ok(meta) = std::fs::metadata(&req.output) {
            report.bytes_written = meta.len();
        }
    }
    Ok(report)
}

/// Runs the vendor restore tool.
pub async fn run_native_restore(
    _services: &Services,
    profile: &ConnectionProfile,
    tool: &ToolLocation,
    req: &RestoreRequest,
    cancel: Arc<AtomicBool>,
    progress: &mut dyn FnMut(DumpProgress),
) -> StudioResult<DumpReport> {
    let ep = endpoint_for(profile).await?;
    let password = resolve_credential(profile)?;
    let (spec, mysql_file) = build_restore_spec(profile, tool, req, &ep, &password)?;
    run_child(spec, mysql_file, tool.tool, cancel, progress, None).await
}

async fn run_child(
    spec: CommandSpec,
    mysql_file: Option<NamedTempFile>,
    tool: NativeTool,
    cancel: Arc<AtomicBool>,
    progress: &mut dyn FnMut(DumpProgress),
    output_to_delete_on_cancel: Option<PathBuf>,
) -> StudioResult<DumpReport> {
    let started = Instant::now();
    let mut warnings = Vec::new();
    let mut objects_done = 0usize;
    let output_path = output_to_delete_on_cancel.clone();

    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.kill_on_drop(true);
    if let Some(stdin_path) = &spec.stdin_file {
        let file = tokio::fs::File::open(stdin_path)
            .await
            .map_err(StudioError::Io)?;
        let stdio: std::process::Stdio = file.into_std().await.into();
        cmd.stdin(stdio);
    }
    match &spec.stdout_file {
        Some(path) => {
            let file = std::fs::File::create(path).map_err(StudioError::Io)?;
            cmd.stdout(std::process::Stdio::from(file));
        }
        None => {
            cmd.stdout(std::process::Stdio::piped());
        }
    }
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        let program = spec.program.display().to_string();
        StudioError::ConnectionMessage(
            sqlly_core::DisplayMessage::new(
                "services.native_dump.spawn_failed",
                format!("Connection error: could not start {program}: {e}"),
            )
            .with_arg("program", program)
            .with_arg("detail", e.to_string()),
        )
    })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();

    if let Some(out) = stdout {
        let tx = line_tx.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(out);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line);
            }
        });
    }
    if let Some(err) = stderr {
        let tx = line_tx;
        tokio::spawn(async move {
            let reader = BufReader::new(err);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = tx.send(line);
            }
        });
    }

    let mut stderr_tail: VecDeque<String> = VecDeque::with_capacity(20);
    let mut cancel_interval = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut bytes_interval = tokio::time::interval(std::time::Duration::from_millis(500));
    bytes_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    cancel_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut cancelled = false;

    let wait_child = async {
        loop {
            tokio::select! {
                line = line_rx.recv() => {
                    match line {
                        Some(line) => {
                            if stderr_tail.len() == 20 {
                                stderr_tail.pop_front();
                            }
                            stderr_tail.push_back(line.clone());
                            match parse_progress_line(tool, &line) {
                                ProgressLine::Progress { phase, object } => {
                                    objects_done += 1;
                                    let bytes = output_bytes(&output_path).await;
                                    progress(DumpProgress {
                                        phase,
                                        object,
                                        objects_done,
                                        objects_total: 0,
                                        rows_written: 0,
                                        bytes_written: bytes,
                                    });
                                }
                                ProgressLine::Warning(w) => warnings.push(w),
                                ProgressLine::Ignore => {}
                            }
                        }
                        None => break,
                    }
                }
                _ = cancel_interval.tick() => {
                    if cancel.load(Ordering::Relaxed) {
                        cancelled = true;
                        let _ = child.start_kill();
                    }
                }
                _ = bytes_interval.tick() => {
                    let bytes = output_bytes(&output_path).await;
                    progress(DumpProgress {
                        phase: DumpPhase::Data,
                        object: String::new(),
                        objects_done,
                        objects_total: 0,
                        rows_written: 0,
                        bytes_written: bytes,
                    });
                }
            }
            if cancelled {
                break;
            }
        }
        child.wait().await
    };

    let status = wait_child.await.map_err(|e| {
        StudioError::ConnectionMessage(
            sqlly_core::DisplayMessage::new(
                "services.native_dump.wait_failed",
                format!("Connection error: waiting for the native database tool failed: {e}"),
            )
            .with_arg("detail", e.to_string()),
        )
    })?;

    let _mysql_guard = mysql_file;

    if cancelled {
        if let Some(path) = output_path {
            let _ = tokio::fs::remove_file(path).await;
        }
        return Ok(DumpReport {
            files: vec![],
            rows_written: 0,
            bytes_written: 0,
            warnings: warnings
                .into_iter()
                .map(sqlly_core::DisplayMessage::raw)
                .collect(),
            elapsed_ms: started.elapsed().as_millis(),
            cancelled: true,
        });
    }
    let bytes = output_bytes(&output_path).await;
    let files = output_path.map(|p| vec![p]).unwrap_or_default();

    if !status.success() {
        let detail = stderr_tail.into_iter().collect::<Vec<_>>().join("\n");
        let exit_code = status.code().unwrap_or(-1).to_string();
        return Err(StudioError::ConnectionMessage(
            sqlly_core::DisplayMessage::new(
                "services.native_dump.tool_failed",
                format!("Connection error: the native database tool exited with code {exit_code}: {detail}"),
            )
            .with_arg("exit_code", exit_code)
            .with_arg("detail", detail),
        ));
    }

    Ok(DumpReport {
        files,
        rows_written: 0,
        bytes_written: bytes,
        warnings: warnings
            .into_iter()
            .map(sqlly_core::DisplayMessage::raw)
            .collect(),
        elapsed_ms: started.elapsed().as_millis(),
        cancelled: false,
    })
}

async fn output_bytes(path: &Option<PathBuf>) -> u64 {
    if let Some(path) = path {
        tokio::fs::metadata(path)
            .await
            .map(|m| m.len())
            .unwrap_or(0)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- argument injection through database / schema names -------------

    /// libpq reads a dbname containing `=` (or a postgres:// URI) as a whole
    /// connection string, and a leading `-` as an option: a hostile database
    /// name must neither redirect the dump (with PGPASSWORD set) nor inject
    /// flags.
    #[test]
    fn pg_tools_never_pass_a_raw_database_name() {
        let mut req = base_req();
        req.database = "host=evil.example dbname=x".into();
        let args = pg_dump_args(&req, &sample_ep());
        assert_eq!(
            args.last().unwrap(),
            "--dbname=dbname='host=evil.example dbname=x'"
        );
        assert!(!args.contains(&"host=evil.example dbname=x".to_string()));

        req.database = "--file=/etc/passwd".into();
        let args = pg_dump_args(&req, &sample_ep());
        assert_eq!(args.last().unwrap(), "--dbname=dbname='--file=/etc/passwd'");

        let restore = RestoreRequest {
            database: r"postgresql://evil/x?password=it's\".into(),
            input: PathBuf::from("/tmp/in.dump"),
            clean: false,
            if_exists: false,
            single_transaction: false,
            no_owner: true,
        };
        let args = pg_restore_args(&restore, &sample_ep());
        assert!(
            args.contains(&r"--dbname=dbname='postgresql://evil/x?password=it\'s\\'".to_string()),
            "{args:?}"
        );
        // Ordinary names keep a readable form.
        assert!(pg_dump_args(&base_req(), &sample_ep()).contains(&"--dbname=mydb".to_string()));
    }

    #[test]
    fn pg_dump_schema_filters_are_literal_names_not_patterns() {
        let mut req = base_req();
        req.options.schemas = Some(vec!["sales*".into(), "a\"b".into()]);
        let args = pg_dump_args(&req, &sample_ep());
        assert!(args.contains(&"\"sales*\"".to_string()), "{args:?}");
        assert!(args.contains(&"\"a\"\"b\"".to_string()), "{args:?}");
    }

    #[test]
    fn mysql_tools_end_options_before_names() {
        let mut req = base_req();
        req.database = "--init-command=DROP DATABASE x".into();
        req.tables = Some(vec![(String::new(), "-t".into())]);
        let args = mysqldump_args(&req, &sample_ep(), Path::new("/tmp/my.cnf"));
        let end = args.iter().position(|a| a == "--").expect("-- separator");
        assert_eq!(args[end + 1], "--init-command=DROP DATABASE x");
        assert_eq!(args[end + 2], "-t");

        let restore = RestoreRequest {
            database: "--init-command=x".into(),
            input: PathBuf::from("/tmp/in.sql"),
            clean: false,
            if_exists: false,
            single_transaction: false,
            no_owner: true,
        };
        let args = mysql_restore_args(&restore, &sample_ep(), Path::new("/tmp/my.cnf"));
        assert_eq!(args.last().unwrap(), "--database=--init-command=x");
    }

    /// The option file is parsed line by line: a newline in the password
    /// would add options, and an unquoted `#` would truncate it.
    #[test]
    fn mysql_defaults_file_quotes_the_password() {
        assert_eq!(
            mysql_defaults_file_content("u", "p#w\\x\nplugin-dir=/tmp"),
            "[client]\nuser=\"u\"\npassword=\"p#w\\\\x\\nplugin-dir=/tmp\"\n"
        );
        // A double quote in the password switches to single quotes so the
        // quote pair (and `#` handling) stays intact.
        assert_eq!(
            mysql_defaults_file_content("u", "a\"#b"),
            "[client]\nuser=\"u\"\npassword='a\"#b'\n"
        );
    }

    /// On Windows, MySQL's tools read their command line in the ANSI code
    /// page, which mangles a user name such as `admin_björn` (the server
    /// then sees `admin_bj`). The user name therefore travels in the UTF-8
    /// option file, never on the command line.
    #[test]
    fn mysql_tools_pass_a_non_ascii_user_in_the_option_file() {
        let ep = Endpoint {
            host: "localhost".into(),
            port: 3306,
            user: "admin_björn".into(),
        };
        let restore = RestoreRequest {
            database: "mydb".into(),
            input: PathBuf::from("/tmp/in.sql"),
            clean: false,
            if_exists: false,
            single_transaction: false,
            no_owner: true,
        };
        for args in [
            mysqldump_args(&base_req(), &ep, Path::new("/tmp/my.cnf")),
            mysql_restore_args(&restore, &ep, Path::new("/tmp/my.cnf")),
        ] {
            assert!(!args.iter().any(|a| a.contains("björn")), "{args:?}");
            assert!(!args.iter().any(|a| a == "-u"), "{args:?}");
        }
        assert_eq!(
            mysql_defaults_file_content("admin_björn", "pw"),
            "[client]\nuser=\"admin_björn\"\npassword=\"pw\"\n"
        );
    }
    use crate::dump_render::DumpOptions;

    fn sample_ep() -> Endpoint {
        Endpoint {
            host: "localhost".into(),
            port: 5432,
            user: "alice".into(),
        }
    }

    fn base_req() -> NativeDumpRequest {
        NativeDumpRequest {
            database: "mydb".into(),
            output: PathBuf::from("/tmp/out.dump"),
            options: DumpOptions::default(),
            tables: None,
            pg_format: PgFormat::Custom,
            pg_no_owner: true,
            mysql_single_transaction: true,
            mysql_extra_args: Vec::new(),
            mysql_strip_definer: false,
        }
    }

    #[test]
    fn mysqldump_extra_args_are_separate_entries_before_the_names() {
        let mut req = base_req();
        req.mysql_extra_args = vec!["--skip-comments".into(), "--where=id > 5".into()];
        let args = mysqldump_args(&req, &sample_ep(), Path::new("/tmp/my.cnf"));
        let end = args.iter().position(|a| a == "--").unwrap();
        let skip = args.iter().position(|a| a == "--skip-comments").unwrap();
        let where_ = args.iter().position(|a| a == "--where=id > 5").unwrap();
        assert!(skip < end && where_ < end, "{args:?}");
        assert_eq!(args[end + 1], "mydb");
        // Never on pg_dump.
        assert!(!pg_dump_args(&req, &sample_ep()).contains(&"--skip-comments".to_string()));
    }

    #[test]
    fn extra_args_split_like_a_shell_without_one() {
        assert_eq!(
            parse_mysqldump_extra_args("  --skip-comments   --column-statistics=0 ").unwrap(),
            vec!["--skip-comments", "--column-statistics=0"]
        );
        assert_eq!(
            parse_mysqldump_extra_args(
                r#"--where="id > 5 and name = 'a b'" --ignore-table='db.t x'"#
            )
            .unwrap(),
            vec!["--where=id > 5 and name = 'a b'", "--ignore-table=db.t x"]
        );
        assert_eq!(
            parse_mysqldump_extra_args(r#"--where="say \"hi\"""#).unwrap(),
            vec![r#"--where=say "hi""#]
        );
        assert!(parse_mysqldump_extra_args("").unwrap().is_empty());
        // Shell metacharacters are plain text: nothing is ever run by a shell.
        assert_eq!(
            parse_mysqldump_extra_args("--where=1;rm -rf ~")
                .unwrap_err()
                .key(),
            Some("services.native_dump.extra_option_not_an_option")
        );
        assert_eq!(
            parse_mysqldump_extra_args("--comments=$(whoami)").unwrap(),
            vec!["--comments=$(whoami)"]
        );
    }

    #[test]
    fn extra_args_reject_unclosed_quotes_and_non_options() {
        let err = parse_mysqldump_extra_args("--where='id > 5").unwrap_err();
        assert_eq!(
            err.key(),
            Some("services.native_dump.extra_option_unclosed_quote")
        );
        for bad in ["otherdb", "-K", "--", "--=x", "--bad name", "--what!"] {
            let err = parse_mysqldump_extra_args(bad).unwrap_err();
            assert_eq!(
                err.key(),
                Some("services.native_dump.extra_option_not_an_option"),
                "{bad}"
            );
        }
    }

    #[test]
    fn extra_args_reject_options_sqlly_controls() {
        for bad in [
            "--result-file=/etc/passwd",
            "--result_file=/tmp/x",
            "--RESULT-FILE=/tmp/x",
            "--res=/tmp/x",
            "--loose-result-file=/tmp/x",
            "--defaults-file=/tmp/my.cnf",
            "--defaults-extra-file=/tmp/my.cnf",
            "--no-defaults",
            "--login-path=prod",
            "--user=root",
            "--password=x",
            "--password",
            "--host=evil.example.com",
            "--port=1",
            "--socket=/tmp/s",
            "--protocol=PIPE",
            "--tab=/tmp",
            "--plugin-dir=/tmp",
            "--p",
            "--skip-password",
        ] {
            let err = parse_mysqldump_extra_args(bad).unwrap_err();
            assert_eq!(
                err.key(),
                Some("services.native_dump.extra_option_reserved"),
                "{bad}"
            );
        }
        for good in [
            "--skip-comments",
            "--hex-blob",
            "--tables",
            "--set-gtid-purged=OFF",
            "--no-tablespaces",
            "--skip-add-locks",
            "--max-allowed-packet=512M",
            "--ssl-mode=REQUIRED",
        ] {
            assert!(parse_mysqldump_extra_args(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn pg_dump_custom_whole_db() {
        let args = pg_dump_args(&base_req(), &sample_ep());
        assert_eq!(
            args,
            vec![
                "-h",
                "localhost",
                "-p",
                "5432",
                "-U",
                "alice",
                "-w",
                "-F",
                "c",
                "-f",
                "/tmp/out.dump",
                "--no-owner",
                "--dbname=mydb"
            ]
        );
    }

    #[test]
    fn pg_dump_plain_schema_only_with_tables_and_clean() {
        let mut req = base_req();
        req.pg_format = PgFormat::Plain;
        req.options.content = DumpContent::StructureOnly;
        req.options.drop_if_exists = true;
        req.tables = Some(vec![("public".into(), "orders".into())]);
        let args = pg_dump_args(&req, &sample_ep());
        assert!(args.contains(&"--schema-only".to_string()));
        assert!(args.contains(&"--clean".to_string()));
        assert!(args.contains(&"--if-exists".to_string()));
        assert!(args.contains(&"-t".to_string()));
        assert!(args.contains(&"\"public\".\"orders\"".to_string()));
    }

    #[test]
    fn pg_dump_data_only_has_no_clean() {
        let mut req = base_req();
        req.pg_format = PgFormat::Plain;
        req.options.content = DumpContent::DataOnly;
        req.options.drop_if_exists = true;
        let args = pg_dump_args(&req, &sample_ep());
        assert!(args.contains(&"--data-only".to_string()));
        assert!(!args.contains(&"--clean".to_string()));
    }

    #[test]
    fn mysqldump_defaults_extra_file_is_first_arg() {
        let req = base_req();
        let args = mysqldump_args(&req, &sample_ep(), Path::new("/tmp/my.cnf"));
        assert!(args[0].starts_with("--defaults-extra-file="));
        assert_eq!(args[1], "-h");
    }

    #[test]
    fn mysqldump_structure_only_and_drop() {
        let mut req = base_req();
        req.options.content = DumpContent::StructureOnly;
        req.options.drop_if_exists = true;
        let args = mysqldump_args(&req, &sample_ep(), Path::new("/tmp/my.cnf"));
        assert!(args.contains(&"--no-data".to_string()));
        assert!(args.contains(&"--add-drop-table".to_string()));
    }

    #[test]
    fn mysqldump_selected_tables_follow_database() {
        let mut req = base_req();
        req.tables = Some(vec![("public".into(), "orders".into())]);
        let args = mysqldump_args(&req, &sample_ep(), Path::new("/tmp/my.cnf"));
        let db_idx = args.iter().position(|a| a == "mydb").unwrap();
        assert_eq!(args[db_idx + 1], "orders");
    }

    #[test]
    fn pg_restore_clean_if_exists_single_txn() {
        let req = RestoreRequest {
            database: "mydb".into(),
            input: PathBuf::from("/tmp/in.dump"),
            clean: true,
            if_exists: true,
            single_transaction: true,
            no_owner: true,
        };
        let args = pg_restore_args(&req, &sample_ep());
        assert!(args.contains(&"--clean".to_string()));
        assert!(args.contains(&"--if-exists".to_string()));
        assert!(args.contains(&"--single-transaction".to_string()));
        assert!(args.contains(&"--no-owner".to_string()));
    }

    #[test]
    fn pg_restore_if_exists_requires_clean() {
        let req = RestoreRequest {
            database: "mydb".into(),
            input: PathBuf::from("/tmp/in.dump"),
            clean: false,
            if_exists: true,
            single_transaction: false,
            no_owner: true,
        };
        let args = pg_restore_args(&req, &sample_ep());
        assert!(!args.contains(&"--if-exists".to_string()));
    }

    #[test]
    fn mysql_restore_args_shape() {
        let req = RestoreRequest {
            database: "mydb".into(),
            input: PathBuf::from("/tmp/in.sql"),
            clean: false,
            if_exists: false,
            single_transaction: false,
            no_owner: true,
        };
        let args = mysql_restore_args(&req, &sample_ep(), Path::new("/tmp/my.cnf"));
        assert_eq!(args[0], "--defaults-extra-file=/tmp/my.cnf");
        assert_eq!(args.last().unwrap(), "--database=mydb");
    }

    #[test]
    fn sniff_pgdmp() {
        assert_eq!(sniff_restore_kind(b"PGDMP\x01"), RestoreKind::PgArchive);
    }

    #[test]
    fn sniff_tar() {
        let mut buf = vec![0u8; 300];
        buf[257..262].copy_from_slice(b"ustar");
        assert_eq!(sniff_restore_kind(&buf), RestoreKind::PgArchive);
    }

    #[test]
    fn sniff_plain() {
        assert_eq!(sniff_restore_kind(b"-- SQL dump"), RestoreKind::PlainSql);
    }

    fn login_profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "remote",
            "10.0.0.5",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "x".to_string(),
            },
        )
    }

    #[test]
    fn relay_routed_profiles_are_refused_before_dialing() {
        let direct = login_profile();
        assert!(refuse_relay_route(&direct).is_ok());
        let mut relayed = login_profile();
        relayed.route = sqlly_core::connection::ConnectionRoute::ViaRelay {
            relay_device_id: "dev".into(),
            pair_id: "pair".into(),
            relay_hint: None,
        };
        let err = refuse_relay_route(&relayed).expect_err("relay refused");
        assert!(err.to_string().contains("direct connection"), "{err}");
    }

    #[test]
    fn pg_dump_output_path_is_stripped_for_stdout() {
        let mut args: Vec<String> = ["-v", "-h", "db", "-f", "/out/x.dump", "--dbname=app"]
            .map(String::from)
            .to_vec();
        strip_output_path(NativeTool::PgDump, &mut args);
        assert_eq!(args, ["-v", "-h", "db", "--dbname=app"]);
    }

    #[test]
    fn mysqldump_result_file_is_stripped_for_stdout() {
        let mut args: Vec<String> = ["-v", "--result-file=/out/x.sql", "--", "app"]
            .map(String::from)
            .to_vec();
        strip_output_path(NativeTool::MysqlDump, &mut args);
        assert_eq!(args, ["-v", "--", "app"]);
    }

    #[test]
    fn pg_restore_input_moves_to_stdin() {
        let input = Path::new("/in/x.dump");
        let mut args: Vec<String> = ["-v", "--dbname=app", "/in/x.dump"]
            .map(String::from)
            .to_vec();
        strip_restore_input(&mut args, input);
        assert_eq!(args, ["-v", "--dbname=app"]);
        // Not the trailing argument: left alone.
        let mut other: Vec<String> = ["--dbname=app"].map(String::from).to_vec();
        strip_restore_input(&mut other, input);
        assert_eq!(other, ["--dbname=app"]);
    }

    #[test]
    fn only_bundled_tools_take_files_by_handle() {
        use crate::tool_discovery::ToolSource;
        let at = |source| ToolLocation {
            tool: NativeTool::PgDump,
            path: PathBuf::from("/x/pg_dump"),
            version: None,
            source,
        };
        assert!(passes_files_by_handle(&at(ToolSource::Bundled)));
        assert!(!passes_files_by_handle(&at(ToolSource::Path)));
        assert!(!passes_files_by_handle(&at(ToolSource::Override)));
        assert!(!passes_files_by_handle(&at(ToolSource::WellKnownDir)));
    }

    #[test]
    fn ensure_output_parent_creates_missing_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = dir.path().join("a").join("b").join("sales.dump");
        ensure_output_parent(&output).expect("create parent");
        assert!(output.parent().expect("parent").is_dir());
        // A bare file name has no parent to create.
        ensure_output_parent(Path::new("sales.dump")).expect("bare name");
    }

    #[test]
    fn sniff_never_panics_on_short_heads_around_the_tar_magic() {
        // 261 bytes used to pass the old `len > 260` guard and then slice
        // `[257..262]` out of bounds.
        for len in 0..=262 {
            let mut buf = vec![b'x'; len];
            // As much of the magic as fits (a file cut inside it).
            let tail = len.saturating_sub(257).min(5);
            if tail > 0 {
                buf[257..257 + tail].copy_from_slice(&b"ustar"[..tail]);
            }
            let expected = if len >= 262 {
                RestoreKind::PgArchive
            } else {
                RestoreKind::PlainSql
            };
            assert_eq!(sniff_restore_kind(&buf), expected, "len {len}");
        }
    }

    #[test]
    fn no_password_on_argv() {
        let ep = sample_ep();
        let req = base_req();
        let restore = RestoreRequest {
            database: "mydb".into(),
            input: PathBuf::from("/tmp/in.sql"),
            clean: true,
            if_exists: true,
            single_transaction: true,
            no_owner: true,
        };
        let secret = "hunter2";
        let _ = secret;
        let all: Vec<Vec<String>> = vec![
            pg_dump_args(&req, &ep),
            mysqldump_args(&req, &ep, Path::new("/tmp/my.cnf")),
            pg_restore_args(&restore, &ep),
            mysql_restore_args(&restore, &ep, Path::new("/tmp/my.cnf")),
        ];
        for args in all {
            for arg in args {
                assert!(!arg.contains("hunter2"));
            }
        }
    }

    #[test]
    fn parse_progress_line_pg_dump_table() {
        match parse_progress_line(
            NativeTool::PgDump,
            "pg_dump: dumping contents of table \"public.orders\"",
        ) {
            ProgressLine::Progress { phase, object } => {
                assert_eq!(phase, DumpPhase::Data);
                assert_eq!(object, "public.orders");
            }
            _ => panic!("expected progress"),
        }
    }

    #[test]
    fn parse_progress_line_pg_restore_item() {
        match parse_progress_line(
            NativeTool::PgRestore,
            "pg_restore: processing item 2345 TABLE DATA orders",
        ) {
            ProgressLine::Progress { phase, .. } => assert_eq!(phase, DumpPhase::Structure),
            _ => panic!("expected progress"),
        }
    }

    #[test]
    fn parse_progress_line_mysqldump_retrieving() {
        match parse_progress_line(
            NativeTool::MysqlDump,
            "-- Retrieving table structure for table orders...",
        ) {
            ProgressLine::Progress { phase, object } => {
                assert_eq!(phase, DumpPhase::Structure);
                assert_eq!(object, "orders");
            }
            _ => panic!("expected progress"),
        }
    }

    #[test]
    fn parse_progress_line_error_is_warning() {
        match parse_progress_line(NativeTool::PgDump, "pg_dump: error: connection failed") {
            ProgressLine::Warning(w) => assert!(w.contains("error")),
            _ => panic!("expected warning"),
        }
    }

    #[test]
    fn parse_progress_line_noise_is_ignore() {
        assert_eq!(
            parse_progress_line(NativeTool::PgDump, "random noise"),
            ProgressLine::Ignore
        );
    }
}
