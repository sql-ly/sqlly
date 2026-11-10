//! Redis command text: the line-oriented grammar the Redis engine's editor
//! speaks, and the read-only / destructive / blocking classifications the
//! connection policy, the AI/MCP read-only gate, and the driver all share.
//!
//! Redis is not SQL, so none of the SQL tokenizer, the SELECT-only
//! classifier, or the rollback wrap applies to it. A script is one command
//! per line in `redis-cli` syntax (bare words, `"double"` quotes with
//! backslash escapes, `'single'` quotes), or a JSON array of arguments on a
//! line (`["SET", "k", "hello world"]`) for arguments the quoting cannot
//! express. Blank lines and lines starting with `#` or `--` are comments.
//!
//! Classification is by command name and subcommand against static tables
//! derived from `COMMAND INFO`'s `readonly` / `write` / `dangerous` flags,
//! so a host can refuse a write before it reaches the server and the driver
//! can refuse a command that would block the connection forever.

use std::fmt;

use sqlly_core::{ConnectionProfile, StudioError, StudioResult};

/// One parsed command: its arguments as raw bytes (Redis arguments are
/// binary-safe) and the 1-based line it came from, for error messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisCommand {
    pub line: usize,
    pub args: Vec<Vec<u8>>,
}

impl RedisCommand {
    /// The command name, upper-cased (`HGETALL`, `JSON.GET`).
    pub fn name(&self) -> String {
        self.arg_text(0).to_ascii_uppercase()
    }

    /// The first argument upper-cased, which is the subcommand for container
    /// commands (`CONFIG GET` → `GET`); empty when absent.
    pub fn subcommand(&self) -> String {
        self.arg_text(1).to_ascii_uppercase()
    }

    /// Argument `index` as lossy UTF-8 text (empty when absent).
    pub fn arg_text(&self, index: usize) -> String {
        self.args
            .get(index)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .unwrap_or_default()
    }

    /// For SQLly's node-targeting prefix `SQLLY.ON <target> <command …>`:
    /// the target text and the command it wraps (on the same line). `None`
    /// for any other command, and for a prefix with no command after it.
    pub fn on_node(&self) -> Option<(String, RedisCommand)> {
        if self.args.len() < 3 || self.name() != ON_NODE_COMMAND {
            return None;
        }
        Some((
            self.arg_text(1),
            RedisCommand {
                line: self.line,
                args: self.args[2..].to_vec(),
            },
        ))
    }

    /// The name of the command that actually runs: the wrapped command's
    /// for `SQLLY.ON <target> <command …>`, this command's otherwise.
    pub fn effective_name(&self) -> String {
        match self.on_node() {
            Some((_, inner)) => inner.effective_name(),
            None => self.name(),
        }
    }

    /// The command as a single `redis-cli` line, quoting arguments that need
    /// it — the inverse of [`parse_script`] for display and "Copy Command".
    pub fn to_cli_line(&self) -> String {
        self.args
            .iter()
            .map(|arg| quote_argument(arg))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// SQLly's node-targeting prefix: `SQLLY.ON <target> <command …>` runs the
/// command on the cluster nodes `target` names instead of routing it by key
/// slot. The safety gates classify the wrapped command, never the prefix.
pub const ON_NODE_COMMAND: &str = "SQLLY.ON";

/// The keyword targets of [`ON_NODE_COMMAND`] (a target may also be a
/// node's `host:port`, or several targets separated by commas).
pub const ON_NODE_KEYWORDS: [&str; 3] = ["masters", "replicas", "all"];

/// Render one argument the way `redis-cli` would accept it back: bare when
/// it is plain, double-quoted with escapes otherwise.
pub fn quote_argument(arg: &[u8]) -> String {
    let plain = !arg.is_empty()
        && arg.iter().all(|&b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'_' | b'-'
                        | b':'
                        | b'.'
                        | b'/'
                        | b'*'
                        | b'@'
                        | b'+'
                        | b'='
                        | b'~'
                        | b'?'
                        | b'!'
                        | b'%'
                        | b'^'
                        | b'&'
                        | b'('
                        | b')'
                        | b'['
                        | b']'
                        | b'{'
                        | b'}'
                        | b'|'
                        | b'<'
                        | b'>'
                        | b','
                        | b';'
                )
        });
    if plain {
        return String::from_utf8_lossy(arg).into_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for &b in arg {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x07 => out.push_str("\\a"),
            0x08 => out.push_str("\\b"),
            b if b.is_ascii_graphic() || b == b' ' => out.push(b as char),
            b => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out.push('"');
    out
}

/// Why a script could not be parsed. Every variant names the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedisParseError {
    /// The script holds no commands (only blanks and comments).
    Empty,
    /// A quoted argument never closed.
    UnterminatedQuote { line: usize },
    /// A closing quote was followed by text instead of whitespace.
    TextAfterQuote { line: usize },
    /// A `\x` escape without two hex digits.
    InvalidEscape { line: usize },
    /// A `[`/`{` line that is not a JSON command.
    InvalidJson { line: usize, message: String },
}

impl fmt::Display for RedisParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RedisParseError::Empty => write!(f, "no command to run (only blank and comment lines)"),
            RedisParseError::UnterminatedQuote { line } => {
                write!(f, "line {line}: unterminated quoted argument")
            }
            RedisParseError::TextAfterQuote { line } => {
                write!(
                    f,
                    "line {line}: a closing quote must be followed by a space"
                )
            }
            RedisParseError::InvalidEscape { line } => {
                write!(f, "line {line}: \\x escapes need two hex digits")
            }
            RedisParseError::InvalidJson { line, message } => {
                write!(f, "line {line}: not a JSON command line — {message}")
            }
        }
    }
}

impl std::error::Error for RedisParseError {}

impl From<RedisParseError> for StudioError {
    fn from(error: RedisParseError) -> Self {
        StudioError::InvalidInput(error.to_string())
    }
}

/// True for a line the script parser skips: blank, `#` comment, `--` comment.
pub fn is_comment_or_blank(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("--")
}

/// Parse a script into commands, one per non-comment line, in order.
pub fn parse_script(text: &str) -> Result<Vec<RedisCommand>, RedisParseError> {
    let mut commands = Vec::new();
    for (index, raw) in text.split('\n').enumerate() {
        let line = index + 1;
        let trimmed = raw.trim_end_matches('\r').trim();
        if is_comment_or_blank(trimmed) {
            continue;
        }
        let args = if trimmed.starts_with('[') || trimmed.starts_with('{') {
            parse_json_line(trimmed, line)?
        } else {
            tokenize_line(trimmed, line)?
        };
        if args.is_empty() {
            continue;
        }
        commands.push(RedisCommand { line, args });
    }
    if commands.is_empty() {
        return Err(RedisParseError::Empty);
    }
    Ok(commands)
}

/// `["SET", "k", "v"]`, or `{"command": "SET", "args": ["k", "v"]}`; numbers
/// and booleans become their text, anything else is refused.
fn parse_json_line(text: &str, line: usize) -> Result<Vec<Vec<u8>>, RedisParseError> {
    let invalid = |message: String| RedisParseError::InvalidJson { line, message };
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| invalid(e.to_string()))?;
    let items: Vec<serde_json::Value> = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(mut map) => {
            let command = map
                .remove("command")
                .ok_or_else(|| invalid("object form needs a \"command\" string".into()))?;
            let mut items = vec![command];
            match map.remove("args") {
                Some(serde_json::Value::Array(args)) => items.extend(args),
                Some(_) => return Err(invalid("\"args\" must be an array".into())),
                None => {}
            }
            items
        }
        _ => return Err(invalid("expected an array of arguments".into())),
    };
    items
        .into_iter()
        .map(|item| match item {
            serde_json::Value::String(s) => Ok(s.into_bytes()),
            serde_json::Value::Number(n) => Ok(n.to_string().into_bytes()),
            serde_json::Value::Bool(b) => Ok(b.to_string().into_bytes()),
            other => Err(invalid(format!(
                "argument {other} must be a string or number"
            ))),
        })
        .collect()
}

/// `redis-cli` argument splitting (`sdssplitargs`): whitespace separates
/// bare words; `"…"` understands `\n \r \t \b \a \xHH \\ \"`; `'…'` only
/// `\'`; a closing quote must be followed by whitespace or the end.
pub fn tokenize_line(text: &str, line: usize) -> Result<Vec<Vec<u8>>, RedisParseError> {
    let bytes = text.as_bytes();
    let mut args: Vec<Vec<u8>> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let mut current: Vec<u8> = Vec::new();
        let mut in_double = false;
        let mut in_single = false;
        loop {
            if in_double {
                let Some(&b) = bytes.get(i) else {
                    return Err(RedisParseError::UnterminatedQuote { line });
                };
                if b == b'\\' && i + 3 < bytes.len() && bytes[i + 1] == b'x' {
                    let hex = &text[i + 2..i + 4];
                    let byte = u8::from_str_radix(hex, 16)
                        .map_err(|_| RedisParseError::InvalidEscape { line })?;
                    current.push(byte);
                    i += 4;
                } else if b == b'\\' && i + 1 < bytes.len() {
                    let escaped = match bytes[i + 1] {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        b'b' => 0x08,
                        b'a' => 0x07,
                        b'x' => return Err(RedisParseError::InvalidEscape { line }),
                        other => other,
                    };
                    current.push(escaped);
                    i += 2;
                } else if b == b'"' {
                    if i + 1 < bytes.len() && !bytes[i + 1].is_ascii_whitespace() {
                        return Err(RedisParseError::TextAfterQuote { line });
                    }
                    i += 1;
                    break;
                } else {
                    current.push(b);
                    i += 1;
                }
            } else if in_single {
                let Some(&b) = bytes.get(i) else {
                    return Err(RedisParseError::UnterminatedQuote { line });
                };
                if b == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    current.push(b'\'');
                    i += 2;
                } else if b == b'\'' {
                    if i + 1 < bytes.len() && !bytes[i + 1].is_ascii_whitespace() {
                        return Err(RedisParseError::TextAfterQuote { line });
                    }
                    i += 1;
                    break;
                } else {
                    current.push(b);
                    i += 1;
                }
            } else {
                let Some(&b) = bytes.get(i) else {
                    break;
                };
                match b {
                    b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c => break,
                    b'"' => {
                        in_double = true;
                        i += 1;
                    }
                    b'\'' => {
                        in_single = true;
                        i += 1;
                    }
                    other => {
                        current.push(other);
                        i += 1;
                    }
                }
            }
        }
        args.push(current);
    }
    Ok(args)
}

/// How a command touches the server, for the safety gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandAccess {
    /// Reads or inspects only (`COMMAND INFO`'s `readonly`, plus the
    /// connection-level commands that change nothing on the server).
    ReadOnly,
    /// Writes keys or state.
    Write,
    /// Deletes data wholesale or reconfigures / stops the server.
    Destructive,
}

impl CommandAccess {
    pub fn is_read_only(self) -> bool {
        matches!(self, CommandAccess::ReadOnly)
    }
}

/// Container commands whose access depends on the subcommand.
fn container_access(name: &str, sub: &str) -> Option<CommandAccess> {
    use CommandAccess::*;
    let access = match name {
        "CONFIG" => match sub {
            "GET" | "HELP" => ReadOnly,
            _ => Destructive,
        },
        "CLIENT" => match sub {
            "ID" | "INFO" | "LIST" | "GETNAME" | "GETREDIR" | "TRACKINGINFO" | "HELP" => ReadOnly,
            "KILL" | "PAUSE" | "UNPAUSE" => Destructive,
            _ => Write,
        },
        "SLOWLOG" => match sub {
            "GET" | "LEN" | "HELP" => ReadOnly,
            _ => Write,
        },
        "LATENCY" => match sub {
            "RESET" => Write,
            _ => ReadOnly,
        },
        "ACL" => match sub {
            "WHOAMI" | "LIST" | "USERS" | "GETUSER" | "CAT" | "GENPASS" | "DRYRUN" | "HELP" => {
                ReadOnly
            }
            "LOG" => ReadOnly,
            "SETUSER" | "DELUSER" | "LOAD" | "SAVE" => Destructive,
            _ => Write,
        },
        "SCRIPT" => match sub {
            "EXISTS" | "HELP" => ReadOnly,
            "FLUSH" | "KILL" => Destructive,
            _ => Write,
        },
        "FUNCTION" => match sub {
            "LIST" | "STATS" | "DUMP" | "HELP" => ReadOnly,
            "FLUSH" | "DELETE" | "KILL" | "RESTORE" => Destructive,
            _ => Write,
        },
        "MEMORY" => match sub {
            "PURGE" => Write,
            _ => ReadOnly,
        },
        "CLUSTER" => match sub {
            "INFO"
            | "NODES"
            | "SLOTS"
            | "SHARDS"
            | "KEYSLOT"
            | "COUNTKEYSINSLOT"
            | "GETKEYSINSLOT"
            | "MYID"
            | "MYSHARDID"
            | "LINKS"
            | "REPLICAS"
            | "SLAVES"
            | "COUNT-FAILURE-REPORTS"
            | "HELP" => ReadOnly,
            "RESET" | "FORGET" | "FAILOVER" | "FLUSHSLOTS" | "DELSLOTS" | "DELSLOTSRANGE" => {
                Destructive
            }
            _ => Write,
        },
        "MODULE" => match sub {
            "LIST" | "HELP" => ReadOnly,
            _ => Destructive,
        },
        "OBJECT" | "XINFO" | "PUBSUB" | "COMMAND" => ReadOnly,
        "DEBUG" => Destructive,
        _ => return None,
    };
    Some(access)
}

/// Commands that read or inspect only. `redis_complete` checks that the
/// bundled command vocabulary covers every name here.
pub(crate) const READ_ONLY_COMMANDS: &[&str] = &[
    // connection
    "PING",
    "ECHO",
    "HELLO",
    "AUTH",
    "SELECT",
    "QUIT",
    "RESET",
    "TIME",
    "INFO",
    "ROLE",
    "LASTSAVE",
    "DBSIZE",
    "RANDOMKEY",
    "LOLWUT",
    "READONLY",
    "READWRITE",
    "WAIT",
    // keys
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "KEYS",
    "SCAN",
    "DUMP",
    "TOUCH",
    "SORT_RO",
    // strings
    "GET",
    "MGET",
    "GETRANGE",
    "SUBSTR",
    "STRLEN",
    "GETBIT",
    "BITCOUNT",
    "BITPOS",
    "BITFIELD_RO",
    "LCS",
    // hashes
    "HGET",
    "HGETALL",
    "HMGET",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HEXISTS",
    "HSTRLEN",
    "HRANDFIELD",
    "HSCAN",
    "HTTL",
    "HPTTL",
    "HEXPIRETIME",
    "HPEXPIRETIME",
    // lists
    "LRANGE",
    "LINDEX",
    "LLEN",
    "LPOS",
    // sets
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SCARD",
    "SRANDMEMBER",
    "SINTER",
    "SUNION",
    "SDIFF",
    "SINTERCARD",
    "SSCAN",
    // sorted sets
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGEBYLEX",
    "ZCARD",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZSCORE",
    "ZMSCORE",
    "ZRANK",
    "ZREVRANK",
    "ZRANDMEMBER",
    "ZINTER",
    "ZUNION",
    "ZDIFF",
    "ZINTERCARD",
    "ZSCAN",
    // streams
    "XRANGE",
    "XREVRANGE",
    "XLEN",
    "XPENDING",
    "XREAD",
    // geo / hll / scripting (read-only forms)
    "GEOPOS",
    "GEODIST",
    "GEOHASH",
    "GEOSEARCH",
    "GEORADIUS_RO",
    "GEORADIUSBYMEMBER_RO",
    "PFCOUNT",
    "EVAL_RO",
    "EVALSHA_RO",
    "FCALL_RO",
    // modules: JSON, search, timeseries, probabilistic, graph
    "JSON.GET",
    "JSON.MGET",
    "JSON.TYPE",
    "JSON.STRLEN",
    "JSON.ARRLEN",
    "JSON.ARRINDEX",
    "JSON.OBJKEYS",
    "JSON.OBJLEN",
    "JSON.RESP",
    "JSON.DEBUG",
    "FT.SEARCH",
    "FT.AGGREGATE",
    "FT.INFO",
    "FT._LIST",
    "FT.EXPLAIN",
    "FT.EXPLAINCLI",
    "FT.PROFILE",
    "FT.TAGVALS",
    "FT.SUGGET",
    "FT.SUGLEN",
    "FT.SYNDUMP",
    "FT.DICTDUMP",
    "TS.GET",
    "TS.MGET",
    "TS.RANGE",
    "TS.REVRANGE",
    "TS.MRANGE",
    "TS.MREVRANGE",
    "TS.INFO",
    "TS.QUERYINDEX",
    "BF.EXISTS",
    "BF.MEXISTS",
    "BF.INFO",
    "BF.CARD",
    "BF.SCANDUMP",
    "CF.EXISTS",
    "CF.MEXISTS",
    "CF.INFO",
    "CF.COUNT",
    "CF.SCANDUMP",
    "CMS.QUERY",
    "CMS.INFO",
    "TOPK.LIST",
    "TOPK.QUERY",
    "TOPK.INFO",
    "TDIGEST.QUANTILE",
    "TDIGEST.MIN",
    "TDIGEST.MAX",
    "TDIGEST.INFO",
    "TDIGEST.CDF",
    "TDIGEST.RANK",
    "TDIGEST.REVRANK",
    "TDIGEST.BYRANK",
    "TDIGEST.BYREVRANK",
    "TDIGEST.TRIMMED_MEAN",
    "GRAPH.RO_QUERY",
    "GRAPH.EXPLAIN",
    "GRAPH.LIST",
];

/// Commands that delete data wholesale, reconfigure, or stop the server.
pub(crate) const DESTRUCTIVE_COMMANDS: &[&str] = &[
    "FLUSHALL",
    "FLUSHDB",
    "DEL",
    "UNLINK",
    "SWAPDB",
    "SHUTDOWN",
    "REPLICAOF",
    "SLAVEOF",
    "MIGRATE",
    "RESTORE",
    "RENAME",
    "RENAMENX",
    "FAILOVER",
    "JSON.DEL",
    "JSON.FORGET",
    "FT.DROPINDEX",
    "FT.DROP",
    "GRAPH.DELETE",
    "TS.DEL",
];

/// Classify a command by name (and subcommand for container commands).
pub fn command_access(command: &RedisCommand) -> CommandAccess {
    let name = command.name();
    // `SQLLY.ON <target> <command …>` writes exactly what the command it
    // wraps writes; a prefix with nothing to run reads nothing.
    if name == ON_NODE_COMMAND {
        return command
            .on_node()
            .map(|(_, inner)| command_access(&inner))
            .unwrap_or(CommandAccess::ReadOnly);
    }
    let sub = command.subcommand();
    if let Some(access) = container_access(&name, &sub) {
        return access;
    }
    // The driver's SQLLY.* virtual commands compose read-only probes.
    if name.starts_with("SQLLY.") {
        return CommandAccess::ReadOnly;
    }
    if DESTRUCTIVE_COMMANDS.contains(&name.as_str()) {
        return CommandAccess::Destructive;
    }
    if READ_ONLY_COMMANDS.contains(&name.as_str()) {
        return CommandAccess::ReadOnly;
    }
    CommandAccess::Write
}

pub fn blocking_message(command: &RedisCommand) -> Option<sqlly_core::DisplayMessage> {
    if let Some((_, inner)) = command.on_node() {
        return blocking_message(&inner);
    }
    let name = command.name();
    let fallback = blocking_reason(command)?;
    let key = match name.as_str() {
        "SUBSCRIBE" | "PSUBSCRIBE" => "services.redis.blocking.subscribe",
        "SSUBSCRIBE" => "services.redis.blocking.sharded_subscribe",
        "MONITOR" => "services.redis.blocking.monitor",
        "SYNC" | "PSYNC" => "services.redis.blocking.replication",
        "XREAD" | "XREADGROUP" => "services.redis.blocking.stream",
        "WAIT" | "WAITAOF" => "services.redis.blocking.replicas",
        _ => "services.redis.blocking.timeout",
    };
    Some(sqlly_core::DisplayMessage::new(key, fallback).with_arg("command_name", name))
}

/// English compatibility fallback; localized callers use [`blocking_message`].
pub fn blocking_reason(command: &RedisCommand) -> Option<String> {
    if let Some((_, inner)) = command.on_node() {
        return blocking_reason(&inner);
    }
    let name = command.name();
    let last = command.arg_text(command.args.len().saturating_sub(1));
    let zero = |text: &str| text.trim().parse::<f64>().is_ok_and(|n| n <= 0.0);
    let reason = match name.as_str() {
        "SUBSCRIBE" | "PSUBSCRIBE" => {
            "puts the connection into subscriber mode and waits for messages forever; watch channels in the Activity Monitor's Pub/Sub tab (Tools > Redis Pub/Sub Viewer) instead"
        }
        // The viewer subscribes sharded channels entered as `#name`.
        "SSUBSCRIBE" => {
            "puts the connection into subscriber mode and waits for messages forever; watch sharded channels in the Activity Monitor's Pub/Sub tab (Tools > Redis Pub/Sub Viewer) instead, entering each as #channel"
        }
        "MONITOR" => {
            "streams every command the server receives until the connection closes; capture it in the Activity Monitor's Profiler tab (Tools > Redis Profiler) instead"
        }
        "SYNC" | "PSYNC" => "starts a replication stream that never ends",
        "BLPOP" | "BRPOP" | "BLMOVE" | "BRPOPLPUSH" | "BZPOPMIN" | "BZPOPMAX"
            if command.args.len() >= 2 && zero(&last) =>
        {
            "blocks until an element arrives; give it a positive timeout in seconds"
        }
        "BLMPOP" | "BZMPOP" if command.args.len() >= 2 && zero(&command.arg_text(1)) => {
            "blocks until an element arrives; give it a positive timeout in seconds"
        }
        "XREAD" | "XREADGROUP" => {
            let mut block_zero = false;
            for (i, arg) in command.args.iter().enumerate() {
                if arg.eq_ignore_ascii_case(b"BLOCK") && zero(&command.arg_text(i + 1)) {
                    block_zero = true;
                }
            }
            if block_zero {
                "BLOCK 0 waits for entries forever; give BLOCK a positive timeout in milliseconds"
            } else {
                return None;
            }
        }
        "WAIT" | "WAITAOF" if zero(&last) => {
            "a timeout of 0 waits for replicas forever; give it a positive timeout in milliseconds"
        }
        _ => return None,
    };
    Some(format!("{name} {reason}"))
}

/// The connection policy for a Redis profile: the SELECT-only lock and the
/// read-only enforcement both mean "read-only commands only", and the
/// rollback wrap is refused because Redis has nothing to roll back.
/// Returns the parsed commands so the driver does not parse twice.
pub fn apply_redis_policy(
    profile: &ConnectionProfile,
    text: &str,
) -> StudioResult<Vec<RedisCommand>> {
    if text.trim().is_empty() {
        return Err(StudioError::InvalidInput("Empty query".to_string()));
    }
    let commands = parse_script(text)?;
    if profile.block_all_but_select || profile.enforce_read_only {
        for command in &commands {
            if !command_access(command).is_read_only() {
                let prefix = if profile.enforce_read_only && !profile.block_all_but_select {
                    super::read_only::READ_ONLY_REFUSAL_PREFIX.to_string()
                } else {
                    "connection policy allows read-only commands only".to_string()
                };
                return Err(StudioError::PermissionDenied(format!(
                    "{prefix}; line {}: {} writes or changes the server",
                    command.line,
                    command.effective_name()
                )));
            }
        }
    }
    // The rollback wrap promises "nothing sticks". A read-only script has
    // nothing to roll back, so it runs; a script that writes is refused
    // because Redis cannot undo it.
    if profile.auto_wrap_rollback
        && !profile.enforce_read_only
        && commands.iter().any(|c| !command_access(c).is_read_only())
    {
        return Err(StudioError::PermissionDenied(
            "connection policy wraps execution in a rolled-back transaction, but Redis \
             has no transactions to roll back — changes could not be undone. Disable the \
             rollback wrap for this connection to run it."
                .to_string(),
        ));
    }
    Ok(commands)
}

/// The first command in `text` that is not read-only, as `(line, name)`, or
/// `None` when every command only reads. Unparsable text is reported as a
/// violation on line 1 so a gate never lets it through.
pub fn first_write_command(text: &str) -> Option<(usize, String)> {
    match parse_script(text) {
        Ok(commands) => commands
            .iter()
            .find(|c| !command_access(c).is_read_only())
            .map(|c| (c.line, c.effective_name())),
        Err(RedisParseError::Empty) => None,
        Err(error) => Some((1, error.to_string())),
    }
}

/// The first destructive command in `text`, as `(line, name)`, for the
/// mutation-confirmation dialog.
pub fn first_destructive_command(text: &str) -> Option<(usize, String)> {
    parse_script(text).ok().and_then(|commands| {
        commands
            .iter()
            .find(|c| command_access(c) == CommandAccess::Destructive)
            .map(|c| (c.line, c.to_cli_line()))
    })
}

/// True when any command in `text` writes (used to decide whether a run
/// needs the data-change confirmation at all).
pub fn has_write_command(text: &str) -> bool {
    first_write_command(text).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    fn args(text: &str) -> Vec<String> {
        tokenize_line(text, 1)
            .unwrap()
            .into_iter()
            .map(|a| String::from_utf8(a).unwrap())
            .collect()
    }

    #[test]
    fn tokenizes_bare_words_and_double_quotes_with_escapes() {
        assert_eq!(args("SET k v"), vec!["SET", "k", "v"]);
        assert_eq!(
            args("SET greeting \"hello world\""),
            vec!["SET", "greeting", "hello world"]
        );
        assert_eq!(
            args(r#"SET k "a\nb\t\"q\"\\""#),
            vec!["SET", "k", "a\nb\t\"q\"\\"]
        );
        assert_eq!(args(r#"SET k "\x41\x42""#), vec!["SET", "k", "AB"]);
        assert_eq!(args("  PING   "), vec!["PING"]);
        assert_eq!(args("SET k \"\""), vec!["SET", "k", ""]);
    }

    #[test]
    fn single_quotes_only_escape_the_quote() {
        assert_eq!(
            args(r"SET k 'it\'s \n raw'"),
            vec!["SET", "k", "it's \\n raw"]
        );
    }

    #[test]
    fn quote_errors_name_the_line() {
        assert_eq!(
            tokenize_line("SET k \"open", 3),
            Err(RedisParseError::UnterminatedQuote { line: 3 })
        );
        assert_eq!(
            tokenize_line("SET k \"a\"b", 2),
            Err(RedisParseError::TextAfterQuote { line: 2 })
        );
        assert_eq!(
            tokenize_line("SET k \"\\x4\"", 5),
            Err(RedisParseError::InvalidEscape { line: 5 })
        );
    }

    #[test]
    fn parses_scripts_skipping_comments_and_reading_json_lines() {
        let script = "# warm up\n\nPING\r\n-- also a comment\n[\"SET\", \"k\", \"two words\", 5]\n{\"command\": \"GET\", \"args\": [\"k\"]}\n";
        let commands = parse_script(script).unwrap();
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0].line, 3);
        assert_eq!(commands[0].name(), "PING");
        assert_eq!(commands[1].line, 5);
        assert_eq!(
            commands[1].args,
            vec![
                b"SET".to_vec(),
                b"k".to_vec(),
                b"two words".to_vec(),
                b"5".to_vec()
            ]
        );
        assert_eq!(commands[2].args, vec![b"GET".to_vec(), b"k".to_vec()]);
        assert_eq!(parse_script("# nothing\n\n"), Err(RedisParseError::Empty));
        assert!(matches!(
            parse_script("[1, {\"a\": 1}]"),
            Err(RedisParseError::InvalidJson { line: 1, .. })
        ));
        assert!(matches!(
            parse_script("{\"args\": []}"),
            Err(RedisParseError::InvalidJson { line: 1, .. })
        ));
    }

    #[test]
    fn cli_line_round_trips_through_the_tokenizer() {
        let original = RedisCommand {
            line: 1,
            args: vec![
                b"SET".to_vec(),
                b"say \"hi\"".to_vec(),
                b"line1\nline2\\".to_vec(),
                vec![0xff, 0x00],
            ],
        };
        let line = original.to_cli_line();
        assert_eq!(
            line,
            "SET \"say \\\"hi\\\"\" \"line1\\nline2\\\\\" \"\\xff\\x00\""
        );
        let back = tokenize_line(&line, 1).unwrap();
        assert_eq!(back, original.args);
        assert_eq!(quote_argument(b"user:1"), "user:1");
    }

    fn cmd(text: &str) -> RedisCommand {
        parse_script(text).unwrap().remove(0)
    }

    #[test]
    fn classifies_read_only_write_and_destructive() {
        use CommandAccess::*;
        assert_eq!(command_access(&cmd("get k")), ReadOnly);
        assert_eq!(command_access(&cmd("HGETALL user:1")), ReadOnly);
        assert_eq!(command_access(&cmd("CONFIG GET databases")), ReadOnly);
        assert_eq!(command_access(&cmd("CONFIG SET maxmemory 1")), Destructive);
        assert_eq!(command_access(&cmd("CLIENT LIST")), ReadOnly);
        assert_eq!(command_access(&cmd("CLIENT KILL ID 3")), Destructive);
        assert_eq!(command_access(&cmd("SLOWLOG GET 128")), ReadOnly);
        assert_eq!(command_access(&cmd("SLOWLOG RESET")), Write);
        assert_eq!(command_access(&cmd("SET k v")), Write);
        assert_eq!(command_access(&cmd("HSET h f v")), Write);
        assert_eq!(command_access(&cmd("DEL k")), Destructive);
        assert_eq!(command_access(&cmd("UNLINK k")), Destructive);
        assert_eq!(command_access(&cmd("FLUSHALL")), Destructive);
        assert_eq!(command_access(&cmd("flushdb async")), Destructive);
        assert_eq!(command_access(&cmd("DEBUG SLEEP 1")), Destructive);
        assert_eq!(command_access(&cmd("SHUTDOWN NOSAVE")), Destructive);
        assert_eq!(command_access(&cmd("JSON.GET doc $")), ReadOnly);
        assert_eq!(command_access(&cmd("JSON.SET doc $ {}")), Write);
        assert_eq!(command_access(&cmd("MEMORY USAGE k")), ReadOnly);
        assert_eq!(command_access(&cmd("ACL WHOAMI")), ReadOnly);
        assert_eq!(command_access(&cmd("ACL SETUSER x on")), Destructive);
        assert_eq!(command_access(&cmd("SELECT 3")), ReadOnly);
        assert_eq!(command_access(&cmd("EVAL \"return 1\" 0")), Write);
        assert_eq!(command_access(&cmd("EVAL_RO \"return 1\" 0")), ReadOnly);
        assert_eq!(command_access(&cmd("UNKNOWN.THING x")), Write);
    }

    /// `SQLLY.ON` is only a routing prefix: every gate sees the command it
    /// wraps, so a write or a FLUSHALL on chosen nodes is still a write.
    #[test]
    fn node_targeted_commands_are_classified_by_the_wrapped_command() {
        use CommandAccess::*;
        assert_eq!(
            command_access(&cmd("SQLLY.ON masters FLUSHALL")),
            Destructive
        );
        assert_eq!(command_access(&cmd("sqlly.on all flushdb")), Destructive);
        assert_eq!(
            command_access(&cmd("SQLLY.ON 10.0.0.1:7000 CONFIG SET maxmemory 1")),
            Destructive
        );
        assert_eq!(command_access(&cmd("SQLLY.ON replicas DEL k")), Destructive);
        assert_eq!(command_access(&cmd("SQLLY.ON masters SET k v")), Write);
        assert_eq!(command_access(&cmd("SQLLY.ON replicas DBSIZE")), ReadOnly);
        assert_eq!(
            command_access(&cmd("SQLLY.ON 10.0.0.1:7000 CONFIG GET maxmemory")),
            ReadOnly
        );
        assert_eq!(command_access(&cmd("SQLLY.ON masters KEYS *")), ReadOnly);
        // Nested prefixes unwrap all the way down.
        assert_eq!(
            command_access(&cmd("SQLLY.ON all SQLLY.ON masters FLUSHALL")),
            Destructive
        );
        // A prefix with nothing to run runs nothing.
        assert_eq!(command_access(&cmd("SQLLY.ON masters")), ReadOnly);
        let wrapped = cmd("SQLLY.ON 10.0.0.1:7000,10.0.0.2:7001 INFO replication");
        let (target, inner) = wrapped.on_node().expect("wrapped command");
        assert_eq!(target, "10.0.0.1:7000,10.0.0.2:7001");
        assert_eq!(inner.name(), "INFO");
        assert_eq!(inner.subcommand(), "REPLICATION");
        assert_eq!(wrapped.effective_name(), "INFO");
        assert!(cmd("GET k").on_node().is_none());
        // Blocking commands stay refused under the prefix.
        assert!(blocking_reason(&cmd("SQLLY.ON masters SUBSCRIBE news")).is_some());
        assert!(blocking_message(&cmd("SQLLY.ON all MONITOR")).is_some());
        assert!(blocking_reason(&cmd("SQLLY.ON masters DBSIZE")).is_none());
        // The connection policy, the finders, and the rollback wrap.
        let mut ro = profile();
        ro.enforce_read_only = true;
        let err = apply_redis_policy(&ro, "SQLLY.ON masters FLUSHALL").unwrap_err();
        assert!(err.to_string().contains("line 1: FLUSHALL"), "{err}");
        assert!(apply_redis_policy(&ro, "SQLLY.ON masters DBSIZE").is_ok());
        let mut select_only = profile();
        select_only.block_all_but_select = true;
        assert!(apply_redis_policy(&select_only, "SQLLY.ON replicas SET k v").is_err());
        let mut wrap = profile();
        wrap.auto_wrap_rollback = true;
        assert!(apply_redis_policy(&wrap, "SQLLY.ON masters FLUSHDB").is_err());
        assert_eq!(
            first_write_command("GET a\nSQLLY.ON masters FLUSHALL"),
            Some((2, "FLUSHALL".into()))
        );
        assert_eq!(
            first_destructive_command("SQLLY.ON masters FLUSHALL"),
            Some((1, "SQLLY.ON masters FLUSHALL".into()))
        );
        assert!(has_write_command("SQLLY.ON all CONFIG SET maxmemory 1"));
        assert!(!has_write_command("SQLLY.ON all INFO"));
    }

    #[test]
    fn refuses_commands_that_block_forever() {
        assert!(blocking_reason(&cmd("SUBSCRIBE news")).is_some());
        assert!(blocking_reason(&cmd("psubscribe n*")).is_some());
        assert!(blocking_reason(&cmd("MONITOR")).is_some());
        assert!(blocking_reason(&cmd("BLPOP q 0")).is_some());
        assert!(blocking_reason(&cmd("BLPOP q 5")).is_none());
        assert!(blocking_reason(&cmd("BLMPOP 0 1 q LEFT")).is_some());
        assert!(blocking_reason(&cmd("BLMPOP 2 1 q LEFT")).is_none());
        assert!(blocking_reason(&cmd("XREAD BLOCK 0 STREAMS s $")).is_some());
        assert!(blocking_reason(&cmd("XREAD BLOCK 500 STREAMS s $")).is_none());
        assert!(blocking_reason(&cmd("XREAD COUNT 2 STREAMS s 0")).is_none());
        assert!(blocking_reason(&cmd("WAIT 1 0")).is_some());
        assert!(blocking_reason(&cmd("GET k")).is_none());
    }

    #[test]
    fn live_command_refusals_point_to_the_live_tools() {
        let monitor = blocking_reason(&cmd("MONITOR")).expect("refused");
        assert!(monitor.contains("Profiler"), "{monitor}");
        assert!(!monitor.contains("redis-cli"), "{monitor}");
        for text in ["SUBSCRIBE news", "PSUBSCRIBE n*"] {
            let reason = blocking_reason(&cmd(text)).expect("refused");
            assert!(reason.contains("Pub/Sub tab"), "{reason}");
            assert!(!reason.contains("redis-cli"), "{reason}");
        }
        // The viewer subscribes sharded channels too (entered as #name),
        // so the editor's refusal points there instead of to redis-cli.
        let sharded = blocking_reason(&cmd("SSUBSCRIBE s")).expect("refused");
        assert!(sharded.contains("Pub/Sub tab"), "{sharded}");
        assert!(sharded.contains("#channel"), "{sharded}");
        assert!(!sharded.contains("redis-cli"), "{sharded}");
    }

    fn profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "r",
            "localhost",
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.database_type = sqlly_core::DatabaseType::Redis;
        p.port = 6379;
        p.auto_wrap_rollback = false;
        p
    }

    #[test]
    fn policy_maps_select_only_and_read_only_to_the_allowlist() {
        let mut p = profile();
        assert_eq!(apply_redis_policy(&p, "GET k\nSET k v").unwrap().len(), 2);
        p.block_all_but_select = true;
        assert!(apply_redis_policy(&p, "GET k\nHGETALL h").is_ok());
        let err = apply_redis_policy(&p, "GET k\nSET k v").unwrap_err();
        assert!(err.to_string().contains("line 2: SET"), "{err}");
        let mut ro = profile();
        ro.enforce_read_only = true;
        let err = apply_redis_policy(&ro, "DEL k").unwrap_err();
        assert!(
            err.to_string()
                .contains(super::super::read_only::READ_ONLY_REFUSAL_PREFIX),
            "{err}"
        );
        let mut wrap = profile();
        wrap.auto_wrap_rollback = true;
        assert!(apply_redis_policy(&wrap, "GET k\nSQLLY.KEYS *").is_ok());
        assert!(
            apply_redis_policy(&wrap, "GET k\nSET k v")
                .unwrap_err()
                .to_string()
                .contains("no transactions")
        );
        assert!(apply_redis_policy(&profile(), "  \n# c\n").is_err());
    }

    #[test]
    fn write_and_destructive_finders_report_lines() {
        assert_eq!(
            first_write_command("GET a\nSET a 1"),
            Some((2, "SET".into()))
        );
        assert_eq!(first_write_command("GET a\n# x\n"), None);
        assert_eq!(
            first_write_command("SET k \"open"),
            Some((1, "line 1: unterminated quoted argument".into()))
        );
        assert_eq!(
            first_destructive_command("SET a 1\nDEL a b"),
            Some((2, "DEL a b".into()))
        );
        assert_eq!(first_destructive_command("SET a 1"), None);
        assert!(has_write_command("INCR n"));
        assert!(!has_write_command("TTL n"));
    }
}
