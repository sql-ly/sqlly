//! IntelliSense CLI entry points.
//!
//! **Canonical path (plan 064):** `intellisense complete-enriched` — shared
//! `IntellisenseService` / `sqlly-intellisense` pipeline (same as GPUI and
//! `sqlly-lsp`). Frozen macOS `RustEngineCompletionProvider` calls this.
//!
//! **Legacy shim:** `intellisense complete` — ADR-011 `sqlly-modelengine`
//! `complete_symbols`. Kept for protocol compatibility: its JSON wire shape
//! (`truncated`, `tentative`, `match_text`, metadata aliases) differs from
//! `complete-enriched` and must not be aliased without a breaking wire change.
//! New integrations must use `complete-enriched` or link `IntellisenseService`
//! in-process (ADR-046).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlly_intel::modelengine::intellisense::{CompletionMode, CompletionRequest, complete_symbols};
use sqlly_lang::sqlmodel::{
    ColumnDef, Dialect, Fidelity, ForeignKeyDef, IncompleteRegions, ModelSnapshot, ObjectBody,
    ObjectFragment, ObjectKind, ObjectRef, ParamDef, ParamMode, Provenance, RoutineBody, SqlName,
    TableBody, TypeRef, ViewBody,
};
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Request/response wire shapes (mirrored by RustEngineCompletionProvider.swift)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct IntelliSenseRequest {
    pub sql: String,
    /// Byte offset of the caret within `sql`.
    pub cursor: usize,
    /// Parser dialect: "sqlserver" | "postgres" | "mysql" | "oracle" |
    /// "sqlite" (default sqlserver). There are five grammars, not one per
    /// engine — pick the family a connection's engine parses as: SQL Server
    /// and Azure SQL → sqlserver; PostgreSQL, its compatible services, and
    /// DuckDB → postgres; MySQL, MariaDB, their compatible services, and
    /// ClickHouse → mysql; Oracle Database → oracle; SQLite and libSQL /
    /// Turso → sqlite. Redis, Valkey and the other command engines take
    /// `redis`: `complete-enriched` then completes command names, option
    /// tokens and subcommands from the Redis command vocabulary (objects of
    /// kind `key` supply key names), while this legacy `complete` shim
    /// refuses the dialect because it has no symbol model to walk.
    #[serde(default)]
    pub dialect: Option<String>,
    #[serde(default)]
    pub default_schema: Option<String>,
    #[serde(default)]
    pub objects: Vec<SchemaObject>,
    #[serde(default)]
    pub foreign_keys: Vec<SchemaForeignKey>,
}

#[derive(Debug, Deserialize)]
pub struct SchemaObject {
    pub schema: String,
    pub name: String,
    /// "table" | "view" | "sproc" | "function" | "type".
    pub kind: String,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub parameters: Vec<SchemaParameter>,
}

#[derive(Debug, Deserialize)]
pub struct SchemaParameter {
    pub name: String,
    #[serde(default)]
    pub type_name: String,
    #[serde(default)]
    pub is_output: bool,
}

#[derive(Debug, Deserialize)]
pub struct SchemaForeignKey {
    pub from_schema: String,
    pub from_table: String,
    pub from_column: String,
    pub to_schema: String,
    pub to_table: String,
    pub to_column: String,
}

#[derive(Debug, Serialize)]
struct IntelliSenseResponse {
    items: Vec<ResponseItem>,
    truncated: bool,
    /// Replacement range (byte offsets into `sql`) for the typed prefix, when
    /// the engine identified one.
    replace_start: Option<usize>,
    replace_end: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ResponseItem {
    label: String,
    insert_text: String,
    /// "table" | "view" | "column" | "procedure" | "function" | "keyword" | "value"
    kind: String,
    detail: Option<String>,
    tentative: bool,
    /// Text the client should score/highlight against instead of `label`, for
    /// qualified items (e.g. a `pm.Description` column scored as `Description`).
    #[serde(skip_serializing_if = "Option::is_none")]
    match_text: Option<String>,

    // --- Passive completion metadata (forwarded verbatim to the client) -----
    // `display`/`replacement`/`completion_type` are convenience aliases of
    // `label`/`insert_text`/`kind` so external consumers can read the offering
    // shape directly; the rest carry the engine's structural facts about the
    // item. Absent optionals are omitted from the JSON. The alias/`name` fields
    // default on deserialize so a payload from a version-skewed engine that
    // omits them still decodes (they can be re-derived from the canonical
    // `label`/`insert_text`/`kind`).
    /// What the UI shows (mirror of `label`).
    #[serde(default)]
    display: String,
    /// What gets inserted on accept (mirror of `insert_text`).
    #[serde(default)]
    replacement: String,
    /// The completion item kind (mirror of `kind`).
    #[serde(default)]
    completion_type: String,
    /// Bare object name, distinct from a possibly-qualified `label`.
    #[serde(default)]
    name: String,
    /// Cheap reason code for why this was offered.
    #[serde(skip_serializing_if = "Option::is_none")]
    why: Option<String>,
    /// Table alias the item was reached through.
    #[serde(skip_serializing_if = "Option::is_none")]
    alias: Option<String>,
    /// Owning schema/namespace.
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<String>,
    /// Parent object's bare name (a column's table/view, a parameter's routine).
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    /// Parent object kind (same vocabulary as `kind`).
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_type: Option<String>,
    /// Data type, when known.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    type_name: Option<String>,
    /// Concrete value (value-mode completions only).
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
}

// ---------------------------------------------------------------------------
// Wire request logging
// ---------------------------------------------------------------------------
//
// Each completion is served by a fresh one-shot `sqlly-cli intellisense
// complete` process (the macOS app spawns one per keystroke and pipes the JSON
// request over stdin). To diagnose a crash that produces no response — the user
// suspects a `<` character triggers it — we append a `REQ` line to a shared log
// file BEFORE running completion (and flush), then append a matching `RES` line
// AFTER. A panic or hard crash mid-completion therefore leaves a `REQ` line with
// no `RES` line carrying the same id: the offending request is the last
// unanswered one. A panic hook records the panic payload on the same id.
//
// Disabled unless opted in, so the hot path stays free of file I/O:
//   * env var `SQLLY_INTELLISENSE_LOG=/path/to/file` — explicit path (lets the
//     macOS app enable logging without changing CLI args), or
//   * `--log-requests` flag — enables logging to the default path
//     (`$TMPDIR/sqlly-intellisense-requests.log`).

const LOG_ENV: &str = "SQLLY_INTELLISENSE_LOG";

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Append-mode logger bound to a single log file path. Each writer call opens,
/// appends, flushes, and closes so partial writes survive a crash and so
/// concurrent one-shot processes interleave at line boundaries.
#[derive(Clone)]
pub struct RequestLog {
    path: PathBuf,
}

impl RequestLog {
    /// Resolve the active logger from the env var and/or the `--log-requests`
    /// flag. Returns `None` when logging is disabled (the default, hot path).
    pub fn resolve(flag_enabled: bool) -> Option<RequestLog> {
        if let Some(path) = std::env::var_os(LOG_ENV) {
            let path = PathBuf::from(path);
            if !path.as_os_str().is_empty() {
                return Some(RequestLog { path });
            }
        }
        if flag_enabled {
            return Some(RequestLog {
                path: default_log_path(),
            });
        }
        None
    }

    /// For tests: build a logger pointed at an explicit path.
    #[cfg(test)]
    pub fn at(path: impl AsRef<Path>) -> RequestLog {
        RequestLog {
            path: path.as_ref().to_path_buf(),
        }
    }

    fn append_line(&self, line: &str) {
        // Logging is best-effort diagnostics: never fail a completion because
        // the log file could not be written.
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }

    fn log_request(&self, id: u64, request_json: &str) {
        self.append_line(&format!(
            "{ts} REQ #{id} {request}",
            ts = now_millis(),
            request = single_line(request_json),
        ));
    }

    fn log_response(&self, id: u64, response_json: &str) {
        self.append_line(&format!(
            "{ts} RES #{id} {response}",
            ts = now_millis(),
            response = single_line(response_json),
        ));
    }

    fn log_error(&self, id: u64, message: &str) {
        self.append_line(&format!(
            "{ts} ERR #{id} {message}",
            ts = now_millis(),
            message = single_line(message),
        ));
    }
}

fn default_log_path() -> PathBuf {
    std::env::temp_dir().join("sqlly-intellisense-requests.log")
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Collapse newlines so each logged record stays on exactly one line, keeping
/// the "REQ with no matching RES" scan trivially grep-able.
fn single_line(s: &str) -> String {
    s.replace('\n', "\\n").replace('\r', "\\r")
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Wire entry used by the CLI dispatcher. `flag_enabled` comes from the
/// `--log-requests` flag; the env var can independently enable logging.
pub fn intellisense_complete_logged(request_json: &str, flag_enabled: bool) -> Result<String> {
    let log = RequestLog::resolve(flag_enabled);
    intellisense_complete_with_log(request_json, log.as_ref())
}

/// Logging-free variant retained for tests and direct callers.
#[allow(dead_code)]
pub fn intellisense_complete(request_json: &str) -> Result<String> {
    intellisense_complete_with_log(request_json, None)
}

fn intellisense_complete_with_log(request_json: &str, log: Option<&RequestLog>) -> Result<String> {
    let id = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);

    if let Some(log) = log {
        // Record the raw request and flush BEFORE doing any parsing or
        // completion work, then install a panic hook so a crash inside
        // `complete_symbols` is attributed to this id. The REQ line with no RES
        // line is the smoking gun for a crashing request.
        log.log_request(id, request_json);
        install_panic_logger(log.clone(), id);
    }

    let result = run_completion(request_json);

    if let Some(log) = log {
        match &result {
            Ok(response) => log.log_response(id, response),
            Err(err) => log.log_error(id, &format!("{err:#}")),
        }
    }

    result
}

/// Install a panic hook that appends the panic payload for `id` to the log,
/// preserving the previously installed hook (so stderr backtraces still fire).
fn install_panic_logger(log: RequestLog, id: u64) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log.append_line(&format!(
            "{ts} PANIC #{id} {info}",
            ts = now_millis(),
            info = single_line(&info.to_string()),
        ));
        previous(info);
    }));
}

/// Legacy modelengine completion shim (plan 064). See module docs — do not
/// route new callers here; use [`run_enriched_completion`] instead.
fn run_completion(request_json: &str) -> Result<String> {
    let request: IntelliSenseRequest =
        serde_json::from_str(request_json).context("invalid intellisense request JSON")?;
    if is_redis_dialect(request.dialect.as_deref()) {
        anyhow::bail!(
            "Redis scripts complete through `intellisense complete-enriched` with dialect `redis`; \
             the legacy `complete` shim walks a SQL symbol model Redis does not have"
        );
    }
    let dialect = parse_dialect(request.dialect.as_deref())?;
    let snapshot = build_snapshot(&request.objects, &request.foreign_keys, dialect);

    let completions = complete_symbols(
        &CompletionRequest {
            text: request.sql.clone(),
            cursor: request.cursor.min(request.sql.len()),
            dialect,
            default_catalog: None,
            default_namespace: request
                .default_schema
                .as_deref()
                .or(Some("dbo"))
                .map(|s| SqlName::new(s, dialect, false)),
            mode: CompletionMode::Symbol,
        },
        &snapshot,
    );

    let replace = completions.items.iter().find_map(|i| i.replace_span);
    let response = IntelliSenseResponse {
        items: completions
            .items
            .into_iter()
            .map(|i| {
                let kind = kind_name(i.kind).to_string();
                ResponseItem {
                    display: i.label.clone(),
                    replacement: i.insert_text.clone(),
                    completion_type: kind.clone(),
                    label: i.label,
                    insert_text: i.insert_text,
                    kind,
                    detail: i.detail,
                    tentative: i.tentative,
                    match_text: i.match_text,
                    name: i.name,
                    why: i.why.map(|w| w.into_owned()),
                    alias: i.alias,
                    schema: i.schema,
                    parent: i.parent,
                    parent_type: i.parent_type.map(|k| kind_name(k).to_string()),
                    type_name: i.r#type,
                    value: i.value,
                }
            })
            .collect(),
        truncated: completions.truncated,
        replace_start: replace.map(|s| s.start),
        replace_end: replace.map(|s| s.end),
    };
    Ok(serde_json::to_string(&response)?)
}

// ---------------------------------------------------------------------------
// `intellisense complete-enriched` — shared-services pipeline
// ---------------------------------------------------------------------------

use sqlly_intel::intellisense::{
    CompletionCatalog, CompletionKind as SharedCompletionKind,
    CompletionSource as SharedCompletionSource,
};
use sqlly_services::intellisense::rules::{self, IntelliSenseRuleConfig};
use sqlly_services::{
    CompletionRequest as SharedCompletionRequest, DefaultIntellisense, IntelliSenseTriggerKind,
    IntellisenseService, procedures_from_snapshot,
};

#[derive(Debug, Deserialize)]
pub struct EnrichedRequest {
    pub sql: String,
    pub cursor: usize,
    #[serde(default)]
    pub dialect: Option<String>,
    #[serde(default)]
    pub default_schema: Option<String>,
    #[serde(default)]
    pub objects: Vec<SchemaObject>,
    #[serde(default)]
    pub foreign_keys: Vec<SchemaForeignKey>,
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub manual: Option<bool>,
    #[serde(default)]
    pub rules_jsonc: Option<String>,
    #[serde(default)]
    pub parameters: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct EnrichedResponse {
    items: Vec<EnrichedResponseItem>,
    replace_start: usize,
    replace_end: usize,
    should_render: bool,
    reason: String,
    matched_rules: Vec<String>,
    debug_facts: Vec<String>,
    max_items: usize,
}

#[derive(Debug, Serialize)]
struct EnrichedResponseItem {
    label: String,
    insert_text: String,
    kind: String,
    source: String,
    score: i32,
    detail: String,
}

pub fn intellisense_complete_enriched_logged(
    request_json: &str,
    flag_enabled: bool,
) -> Result<String> {
    let log = RequestLog::resolve(flag_enabled);
    intellisense_complete_enriched_with_log(request_json, log.as_ref())
}

fn intellisense_complete_enriched_with_log(
    request_json: &str,
    log: Option<&RequestLog>,
) -> Result<String> {
    let id = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);

    if let Some(log) = log {
        log.log_request(id, request_json);
        install_panic_logger(log.clone(), id);
    }

    let result = run_enriched_completion(request_json);

    if let Some(log) = log {
        match &result {
            Ok(response) => log.log_response(id, response),
            Err(err) => log.log_error(id, &format!("{err:#}")),
        }
    }

    result
}

fn run_enriched_completion(request_json: &str) -> Result<String> {
    let request: EnrichedRequest =
        serde_json::from_str(request_json).context("invalid enriched intellisense request JSON")?;

    if is_redis_dialect(request.dialect.as_deref()) {
        return Ok(serde_json::to_string(&redis_enriched_response(&request))?);
    }
    let dialect = parse_dialect(request.dialect.as_deref())?;

    // Route through the datamodel (ModelSnapshot) — the single source of truth.
    // The same `build_snapshot` is used by `intellisense complete` (symbol mode),
    // so both CLI completion paths share the same datamodel foundation.
    let snapshot = build_snapshot(&request.objects, &request.foreign_keys, dialect);
    let catalog = CompletionCatalog::from_model_snapshot(&snapshot);
    let procedures = procedures_from_snapshot(&snapshot);
    let rules_config = parse_rules(request.rules_jsonc.as_deref());
    let trigger = parse_trigger(request.trigger.as_deref());
    let manual = request.manual.unwrap_or(false);

    let service = DefaultIntellisense;
    let shared_req = SharedCompletionRequest {
        sql: &request.sql,
        cursor: request.cursor.min(request.sql.len()),
        trigger,
        manual,
        catalog: &catalog,
        procedures: &procedures,
        rules: &rules_config,
        parameters: request.parameters.as_deref().unwrap_or(&[]),
        model: None,
        database_type: None,
    };

    let response = service.complete_enriched_response(&shared_req);

    let enriched = EnrichedResponse {
        items: response
            .items
            .iter()
            .map(|i| EnrichedResponseItem {
                label: i.label.clone(),
                insert_text: i.insert_text.clone(),
                kind: shared_kind_name(i.kind).to_string(),
                source: shared_source_name(&i.source).to_string(),
                score: i.score,
                detail: i.detail.clone(),
            })
            .collect(),
        replace_start: response.replace_start,
        replace_end: response.replace_end,
        should_render: response.should_render,
        reason: response.reason.clone(),
        matched_rules: response.matched_rules.clone(),
        debug_facts: response.debug_facts.clone(),
        max_items: response.max_items,
    };

    Ok(serde_json::to_string(&enriched)?)
}

/// `redis` and the engines that speak its protocol: command completion,
/// not a SQL grammar.
fn is_redis_dialect(name: Option<&str>) -> bool {
    matches!(
        name.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("redis" | "valkey" | "memorydb" | "elasticache" | "keydb" | "dragonfly" | "resp")
    )
}

/// Command completion for a Redis script over the bundled vocabulary.
/// Request objects of kind `key` (name = the key, schema = its Redis type,
/// or empty) seed key-name completion for logical database `0`.
fn redis_enriched_response(request: &EnrichedRequest) -> EnrichedResponse {
    use sqlly_intel::redis_complete::{CompletionOptions, KnownKey, RedisKeyIndex, complete};
    let mut keys = RedisKeyIndex::new();
    keys.replace(
        "0",
        request
            .objects
            .iter()
            .filter(|o| o.kind.eq_ignore_ascii_case("key"))
            .map(|o| KnownKey {
                name: o.name.clone(),
                kind: if o.schema.is_empty() {
                    "unknown".to_string()
                } else {
                    o.schema.clone()
                },
            }),
    );
    let vocabulary = sqlly_intel::redis_complete::bundled();
    let cursor = request.cursor.min(request.sql.len());
    let response = complete(
        &request.sql,
        cursor,
        vocabulary,
        CompletionOptions {
            database: "0",
            keys: &keys,
            history: None,
        },
    );
    let max_items = response.items.len();
    EnrichedResponse {
        items: response
            .items
            .iter()
            .map(|i| EnrichedResponseItem {
                label: i.label.clone(),
                insert_text: i.insert_text.clone(),
                kind: shared_kind_name(i.kind).to_string(),
                source: shared_source_name(&i.source).to_string(),
                score: i.score,
                detail: i.detail.clone(),
            })
            .collect(),
        replace_start: response.replace_start,
        replace_end: cursor,
        should_render: !response.items.is_empty(),
        reason: response.reason,
        matched_rules: Vec::new(),
        debug_facts: vec![format!(
            "redis command vocabulary: bundled, {} commands",
            vocabulary.len()
        )],
        max_items,
    }
}

fn parse_rules(rules_jsonc: Option<&str>) -> IntelliSenseRuleConfig {
    match rules_jsonc {
        Some(text) => rules::parse(text).unwrap_or_default(),
        None => IntelliSenseRuleConfig::default(),
    }
}

fn parse_trigger(trigger: Option<&str>) -> IntelliSenseTriggerKind {
    match trigger.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("manual_shortcut") | Some("manualshortcut") => IntelliSenseTriggerKind::ManualShortcut,
        Some("dot") => IntelliSenseTriggerKind::Dot,
        Some("at_sign") | Some("atsign") | Some("at") => IntelliSenseTriggerKind::AtSign,
        Some("quote") => IntelliSenseTriggerKind::Quote,
        Some("whitespace") | Some("space") => IntelliSenseTriggerKind::Whitespace,
        Some("manual_value_lookup") | Some("manualvaluelookup") => {
            IntelliSenseTriggerKind::ManualValueLookup
        }
        Some("explicit") | Some("identifier") => IntelliSenseTriggerKind::Explicit,
        Some("automatic_typing") | Some("automatictyping") | Some("auto") | None => {
            IntelliSenseTriggerKind::AutomaticTyping
        }
        _ => IntelliSenseTriggerKind::AutomaticTyping,
    }
}

fn shared_kind_name(kind: SharedCompletionKind) -> &'static str {
    match kind {
        SharedCompletionKind::Keyword => "keyword",
        SharedCompletionKind::Database => "database",
        SharedCompletionKind::Schema => "schema",
        SharedCompletionKind::Table => "table",
        SharedCompletionKind::Column => "column",
        SharedCompletionKind::Function => "function",
        SharedCompletionKind::Procedure => "procedure",
        SharedCompletionKind::Type => "type",
        SharedCompletionKind::Parameter => "parameter",
        SharedCompletionKind::ResultSetColumn => "resultSetColumn",
        SharedCompletionKind::Snippet => "snippet",
        SharedCompletionKind::Value => "value",
        SharedCompletionKind::Join => "join",
    }
}

fn shared_source_name(source: &SharedCompletionSource) -> &'static str {
    match source {
        SharedCompletionSource::Keyword => "keyword",
        SharedCompletionSource::Builtin => "builtin",
        SharedCompletionSource::LiveMetadata => "liveMetadata",
        SharedCompletionSource::Filesystem => "filesystem",
        SharedCompletionSource::Inferred => "inferred",
        SharedCompletionSource::Snippet => "snippet",
    }
}

#[cfg(test)]
mod enriched_tests {
    use super::*;

    fn temp_log_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sqlly-test-{tag}-{nanos}-{seq}.log"))
    }

    fn enriched_request(sql: &str, cursor: usize) -> String {
        serde_json::json!({
            "sql": sql,
            "cursor": cursor,
            "dialect": "sqlserver",
            "default_schema": "dbo",
            "objects": [
                {"schema": "dbo", "name": "Person", "kind": "table",
                 "columns": ["Id", "Name", "Age"]},
                {"schema": "dbo", "name": "Model", "kind": "table",
                 "columns": ["Id", "Name"]},
                {"schema": "dbo", "name": "ActivePeople", "kind": "view",
                 "columns": ["Id", "Name"]},
                {"schema": "dbo", "name": "ImportAccounts", "kind": "sproc",
                 "columns": [],
                 "parameters": [{"name": "@BatchId", "type_name": "int", "is_output": false}]}
            ],
            "foreign_keys": []
        })
        .to_string()
    }

    #[test]
    fn enriched_tables_after_from() {
        let sql = "select * from Pe";
        let out = run_enriched_completion(&enriched_request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["should_render"], true);
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(labels.iter().any(|l| l.contains("Person")), "{labels:?}");
        assert!(
            !labels.iter().any(|l| l.contains("Model")),
            "prefix filter: {labels:?}"
        );
    }

    #[test]
    fn enriched_columns_after_alias_dot() {
        let sql = "select * from dbo.Person p where p.";
        let out = run_enriched_completion(&enriched_request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(labels.contains(&"p.Id"), "{labels:?}");
        assert!(labels.contains(&"p.Name"), "{labels:?}");
        assert!(labels.contains(&"p.Age"), "{labels:?}");
    }

    #[test]
    fn enriched_procedure_snippet() {
        let sql = "EXEC Imp";
        let out = run_enriched_completion(&enriched_request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let items = v["items"].as_array().unwrap();
        assert!(
            items
                .iter()
                .any(|i| i["label"].as_str().unwrap().contains("ImportAccounts")
                    && i["kind"] == "procedure"),
            "{items:?}"
        );
    }

    #[test]
    fn enriched_procedure_detail() {
        let sql = "EXEC Imp";
        let out = run_enriched_completion(&enriched_request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let items = v["items"].as_array().unwrap();
        let proc_item = items
            .iter()
            .find(|i| i["label"].as_str().unwrap().contains("ImportAccounts"))
            .unwrap();
        assert!(
            proc_item["detail"].as_str().unwrap().contains("@BatchId"),
            "procedure detail should carry parameter list: {proc_item}"
        );
    }

    #[test]
    fn enriched_replace_span() {
        let sql = "select * from Pe";
        let out = run_enriched_completion(&enriched_request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(
            v["replace_start"].as_u64().is_some(),
            "replace_start present"
        );
        assert!(v["replace_end"].as_u64().is_some(), "replace_end present");
    }

    #[test]
    fn enriched_auto_trigger_suppressed_at_line_start() {
        let sql = "";
        let out = run_enriched_completion(&enriched_request(sql, 0)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["should_render"], false,
            "auto trigger should be suppressed at empty input"
        );
    }

    #[test]
    fn enriched_manual_trigger_bypasses_gate() {
        let sql = "";
        let req = serde_json::json!({
            "sql": sql,
            "cursor": 0,
            "dialect": "sqlserver",
            "default_schema": "dbo",
            "objects": [
                {"schema": "dbo", "name": "Person", "kind": "table", "columns": ["Id"]}
            ],
            "foreign_keys": [],
            "manual": true
        })
        .to_string();
        let out = run_enriched_completion(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["should_render"], true,
            "manual trigger should bypass auto-trigger gate"
        );
    }

    #[test]
    fn enriched_trigger_field_respected() {
        let sql = "select * from dbo.Person p where p.";
        let req = serde_json::json!({
            "sql": sql,
            "cursor": sql.len(),
            "dialect": "sqlserver",
            "default_schema": "dbo",
            "objects": [
                {"schema": "dbo", "name": "Person", "kind": "table", "columns": ["Id", "Name"]}
            ],
            "foreign_keys": [],
            "trigger": "dot"
        })
        .to_string();
        let out = run_enriched_completion(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["should_render"], true);
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(
            labels.contains(&"p.Id"),
            "dot trigger should emit columns: {labels:?}"
        );
    }

    #[test]
    fn enriched_rules_jsonc_applied() {
        let sql = "select * from Pe";
        let req = serde_json::json!({
            "sql": sql,
            "cursor": sql.len(),
            "dialect": "sqlserver",
            "default_schema": "dbo",
            "objects": [
                {"schema": "dbo", "name": "Person", "kind": "table", "columns": ["Id"]}
            ],
            "foreign_keys": [],
            "rules_jsonc": "{\"rules\":[{\"name\":\"blockTables\",\"when\":{\"intents\":[\"fromSource\"]},\"render\":false,\"reason\":\"test rule\"}]}"
        })
        .to_string();
        let out = run_enriched_completion(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["should_render"], false,
            "rule with render=false should suppress popup"
        );
        assert!(
            v["reason"].as_str().unwrap().contains("test rule"),
            "reason should carry rule label: {}",
            v["reason"]
        );
    }

    #[test]
    fn enriched_bad_json_errors() {
        assert!(run_enriched_completion("{not json").is_err());
    }

    /// A Redis script completes command names, option tokens by position,
    /// subcommands, and key names supplied as `key` objects — through the
    /// canonical path only; the legacy shim says where to go.
    #[test]
    fn enriched_completes_redis_scripts_from_the_command_vocabulary() {
        let request = |sql: &str, cursor: usize| {
            serde_json::json!({
                "sql": sql,
                "cursor": cursor,
                "dialect": "redis",
                "objects": [
                    {"schema": "hash", "name": "user:1", "kind": "key"},
                    {"schema": "", "name": "user:2", "kind": "key"}
                ]
            })
            .to_string()
        };
        let labels = |json: String| -> Vec<String> {
            let response: serde_json::Value =
                serde_json::from_str(&run_enriched_completion(&json).unwrap()).unwrap();
            response["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["label"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(labels(request("HSE", 3)), vec!["HSET", "HSETNX"]);
        assert_eq!(
            labels(request("ZRANGE k 0 -1 ", 14)),
            vec!["BYLEX", "BYSCORE", "LIMIT", "REV", "WITHSCORES"]
        );
        assert_eq!(
            labels(request("XINFO ", 6)),
            vec!["CONSUMERS", "GROUPS", "HELP", "STREAM"]
        );
        assert_eq!(labels(request("GET us", 6)), vec!["user:1", "user:2"]);
        let response: serde_json::Value =
            serde_json::from_str(&run_enriched_completion(&request("GET us", 6)).unwrap()).unwrap();
        assert_eq!(response["replace_start"], 4);
        assert_eq!(response["replace_end"], 6);
        assert_eq!(response["items"][0]["detail"], "hash key");
        assert!(response["should_render"].as_bool().unwrap());
        assert!(
            response["debug_facts"][0]
                .as_str()
                .unwrap()
                .contains("bundled")
        );
        // Valkey speaks the same commands.
        let valkey =
            serde_json::json!({"sql": "PIN", "cursor": 3, "dialect": "valkey"}).to_string();
        assert_eq!(labels(valkey), vec!["PING"]);
        // The legacy shim refuses with directions rather than a dialect error.
        let legacy = serde_json::json!({"sql": "HSE", "cursor": 3, "dialect": "redis"}).to_string();
        let err = run_completion(&legacy).unwrap_err().to_string();
        assert!(err.contains("complete-enriched"), "{err}");
    }

    #[test]
    fn enriched_bad_dialect_errors() {
        let bad = serde_json::json!({
            "sql": "x",
            "cursor": 0,
            "dialect": "db2",
            "objects": []
        })
        .to_string();
        assert!(run_enriched_completion(&bad).is_err());
    }

    /// Engines that borrow another family's grammar are accepted as aliases
    /// of that family; an unknown name says which families exist.
    #[test]
    fn parse_dialect_maps_engine_names_onto_the_five_families() {
        assert_eq!(parse_dialect(None).unwrap(), Dialect::SqlServer);
        assert_eq!(parse_dialect(Some("duckdb")).unwrap(), Dialect::Postgres);
        assert_eq!(parse_dialect(Some("ClickHouse")).unwrap(), Dialect::MySql);
        assert_eq!(parse_dialect(Some("mariadb")).unwrap(), Dialect::MySql);
        assert_eq!(parse_dialect(Some("libsql")).unwrap(), Dialect::Sqlite);
        assert_eq!(parse_dialect(Some("turso")).unwrap(), Dialect::Sqlite);
        assert_eq!(parse_dialect(Some("oracle")).unwrap(), Dialect::Oracle);
        let err = parse_dialect(Some("db2")).unwrap_err().to_string();
        assert!(
            err.contains("sqlserver, postgres, mysql, oracle, or sqlite"),
            "{err}"
        );
    }

    #[test]
    fn enriched_response_carries_debug_facts() {
        let sql = "select * from Pe";
        let out = run_enriched_completion(&enriched_request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(
            v["debug_facts"].is_array(),
            "debug_facts should be an array"
        );
    }

    #[test]
    fn enriched_response_carries_score_and_source() {
        let sql = "select * from Pe";
        let out = run_enriched_completion(&enriched_request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let items = v["items"].as_array().unwrap();
        assert!(!items.is_empty(), "should have items");
        let first = &items[0];
        assert!(first["score"].as_i64().is_some(), "score should be present");
        assert!(
            first["source"].as_str().is_some(),
            "source should be present"
        );
    }

    #[test]
    fn enriched_logging_writes_req_and_res() {
        let path = temp_log_path("enriched-reqres");
        let log = RequestLog::at(&path);
        let sql = "select * from Pe";
        let _ =
            intellisense_complete_enriched_with_log(&enriched_request(sql, sql.len()), Some(&log))
                .unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(contents.contains(" REQ #"), "request logged: {contents:?}");
        assert!(contents.contains(" RES #"), "response logged: {contents:?}");
    }
}

/// Map a dialect name onto one of the five parser families (see the
/// `dialect` field on [`IntelliSenseRequest`] for which engines use which).
/// Engine names that are not grammars of their own (`duckdb`, `clickhouse`,
/// `libsql`, `mariadb`) are accepted as aliases of the family they parse as.
pub fn parse_dialect(name: Option<&str>) -> Result<Dialect> {
    Ok(
        match name.unwrap_or("sqlserver").to_ascii_lowercase().as_str() {
            "sqlserver" | "mssql" | "tsql" => Dialect::SqlServer,
            "postgres" | "postgresql" | "pg" | "duckdb" => Dialect::Postgres,
            "mysql" | "mariadb" | "clickhouse" => Dialect::MySql,
            "oracle" => Dialect::Oracle,
            "sqlite" | "libsql" | "turso" => Dialect::Sqlite,
            other => anyhow::bail!(
                "unknown dialect `{other}` (expected sqlserver, postgres, mysql, oracle, or sqlite; \
                 duckdb parses as postgres, clickhouse and mariadb as mysql, libsql as sqlite; \
                 redis scripts complete through `complete-enriched` with dialect `redis`)"
            ),
        },
    )
}

fn kind_name(kind: sqlly_intel::modelengine::intellisense::CompletionItemKind) -> &'static str {
    use sqlly_intel::modelengine::intellisense::CompletionItemKind as K;
    match kind {
        K::Table => "table",
        K::View => "view",
        K::Column => "column",
        K::Procedure => "procedure",
        K::Function => "function",
        K::Parameter => "parameter",
        K::Keyword => "keyword",
        K::Value => "value",
    }
}

/// Lower the app's editor schema into model fragments and apply them to an
/// empty snapshot. The editor schema carries column names without types, so
/// columns get no `TypeRef` (the engine treats type detail as optional).
///
/// Shared by `intellisense complete` and the persistent engine's `validate`
/// verb: both build an offline `ModelSnapshot` from the app's cached editor
/// schema with no database I/O.
pub fn build_snapshot(
    objects: &[SchemaObject],
    foreign_keys: &[SchemaForeignKey],
    dialect: Dialect,
) -> ModelSnapshot {
    let provenance = Provenance::Database {
        profile: "editor-schema".into(),
    };
    let name = |s: &str| SqlName::new(s, dialect, false);

    let mut fragments: Vec<ObjectFragment> = Vec::new();
    for object in objects {
        let reference = ObjectRef::new(None, Some(name(&object.schema)), name(&object.name));
        let columns: Vec<ColumnDef> = object
            .columns
            .iter()
            .enumerate()
            .map(|(ordinal, column)| ColumnDef {
                name: name(column),
                ty: None,
                nullable: None,
                default: None,
                identity: false,
                generated: None,
                collation: None,
                ordinal,
            })
            .collect();
        let (kind, body) = match object.kind.as_str() {
            "table" => {
                let mut table = TableBody {
                    columns,
                    ..TableBody::default()
                };
                for fk in foreign_keys {
                    if fk.from_schema.eq_ignore_ascii_case(&object.schema)
                        && fk.from_table.eq_ignore_ascii_case(&object.name)
                    {
                        table.foreign_keys.push(ForeignKeyDef {
                            name: None,
                            columns: vec![name(&fk.from_column)],
                            target: ObjectRef::new(
                                None,
                                Some(name(&fk.to_schema)),
                                name(&fk.to_table),
                            ),
                            target_columns: vec![name(&fk.to_column)],
                        });
                    }
                }
                (ObjectKind::Table, ObjectBody::Table(table))
            }
            "view" => (
                ObjectKind::View,
                ObjectBody::View(ViewBody {
                    columns,
                    definition: None,
                }),
            ),
            "sproc" | "procedure" | "proc" | "stored_procedure" | "storedprocedure" => (
                ObjectKind::Procedure,
                ObjectBody::Routine(routine_body(object, dialect)),
            ),
            "function" | "scalar_function" | "table_function" => (
                ObjectKind::Function,
                ObjectBody::Routine(routine_body(object, dialect)),
            ),
            "type" | "domain_type" => (ObjectKind::DomainType, ObjectBody::Opaque),
            _ => continue,
        };
        fragments.push(ObjectFragment {
            reference,
            kind,
            dialect,
            body,
            provenance: provenance.clone(),
            fidelity: Fidelity::Exact,
            references: Vec::new(),
        });
    }

    let (snapshot, _) =
        ModelSnapshot::empty().apply(1, fragments, &BTreeSet::new(), IncompleteRegions::default());
    snapshot
}

fn routine_body(object: &SchemaObject, dialect: Dialect) -> RoutineBody {
    RoutineBody {
        params: object
            .parameters
            .iter()
            .map(|p| ParamDef {
                name: SqlName::new(&p.name, dialect, false),
                ty: (!p.type_name.is_empty()).then(|| TypeRef::parse(&p.type_name, dialect)),
                mode: if p.is_output {
                    ParamMode::Out
                } else {
                    ParamMode::In
                },
                default: None,
            })
            .collect(),
        returns: None,
        language: None,
        body_text: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(sql: &str, cursor: usize) -> String {
        serde_json::json!({
            "sql": sql,
            "cursor": cursor,
            "dialect": "sqlserver",
            "default_schema": "dbo",
            "objects": [
                {"schema": "dbo", "name": "Person", "kind": "table",
                 "columns": ["Id", "Name", "Age"]},
                {"schema": "dbo", "name": "Model", "kind": "table",
                 "columns": ["Id", "Name"]},
                {"schema": "dbo", "name": "ActivePeople", "kind": "view",
                 "columns": ["Id", "Name"]},
                {"schema": "dbo", "name": "ImportAccounts", "kind": "sproc",
                 "columns": [],
                 "parameters": [{"name": "@BatchId", "type_name": "int", "is_output": false}]}
            ],
            "foreign_keys": []
        })
        .to_string()
    }

    #[test]
    fn tables_after_from_with_prefix() {
        let sql = "select * from Pe";
        let out = intellisense_complete(&request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(labels.iter().any(|l| l.contains("Person")), "{labels:?}");
        assert!(
            !labels.iter().any(|l| l.contains("Model")),
            "prefix filter: {labels:?}"
        );
        // Replace span covers the typed prefix "Pe".
        assert_eq!(v["replace_start"].as_u64(), Some(14));
        assert_eq!(v["replace_end"].as_u64(), Some(16));
    }

    #[test]
    fn columns_after_alias_dot() {
        let sql = "select * from dbo.Person p where p.";
        let out = intellisense_complete(&request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(labels.contains(&"Id"), "{labels:?}");
        assert!(labels.contains(&"Name"));
        assert!(labels.contains(&"Age"));
        let kinds: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["kind"].as_str().unwrap())
            .collect();
        assert!(kinds.iter().all(|k| *k == "column"));
    }

    #[test]
    fn where_unqualified_emits_alias_qualified_columns() {
        // Typing a bare column prefix in a value context (WHERE) must surface
        // the in-scope columns, qualified with the table alias so accepting
        // inserts `p.Name`. match_text carries the bare name for client scoring.
        let sql = "select * from dbo.Person p where Na";
        let out = intellisense_complete(&request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let items = v["items"].as_array().unwrap();
        let name_item = items
            .iter()
            .find(|i| i["label"].as_str() == Some("p.Name"))
            .unwrap_or_else(|| panic!("missing p.Name: {items:?}"));
        assert_eq!(name_item["kind"].as_str(), Some("column"));
        assert_eq!(name_item["insert_text"].as_str(), Some("p.Name"));
        assert_eq!(name_item["match_text"].as_str(), Some("Name"));
        // Unrelated columns are not present.
        assert!(
            !items.iter().any(|i| i["label"].as_str() == Some("p.Age")),
            "prefix filter: {items:?}"
        );
    }

    #[test]
    fn where_unqualified_unaliased_emits_bare_columns() {
        // Without an alias the columns are bare and carry no match_text.
        let sql = "select * from dbo.Person where Na";
        let out = intellisense_complete(&request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let items = v["items"].as_array().unwrap();
        let name_item = items
            .iter()
            .find(|i| i["label"].as_str() == Some("Name"))
            .unwrap_or_else(|| panic!("missing Name: {items:?}"));
        assert_eq!(name_item["kind"].as_str(), Some("column"));
        // No qualifier => no match_text (bare label already scores as a prefix).
        assert!(name_item.get("match_text").is_none() || name_item["match_text"].is_null());
    }

    #[test]
    fn view_columns_after_alias_dot() {
        let sql = "select * from ActivePeople ap where ap.";
        let out = intellisense_complete(&request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(
            labels.contains(&"Id"),
            "view columns must complete: {labels:?}"
        );
    }

    #[test]
    fn procedures_after_exec() {
        let sql = "EXEC Imp";
        let out = intellisense_complete(&request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let items = v["items"].as_array().unwrap();
        assert!(
            items
                .iter()
                .any(|i| i["label"].as_str().unwrap().contains("ImportAccounts")
                    && i["kind"] == "procedure"),
            "{items:?}"
        );
    }

    #[test]
    fn bad_json_and_bad_dialect_error() {
        assert!(intellisense_complete("{not json").is_err());
        let bad = serde_json::json!({"sql": "x", "cursor": 0, "dialect": "db2"}).to_string();
        assert!(intellisense_complete(&bad).is_err());
    }

    fn temp_log_path(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sqlly-test-{tag}-{nanos}-{seq}.log"))
    }

    #[test]
    fn logging_writes_request_and_response_lines() {
        let path = temp_log_path("reqres");
        let log = RequestLog::at(&path);
        let sql = "select * from Pe";
        let out = intellisense_complete_with_log(&request(sql, sql.len()), Some(&log)).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        let req_lines: Vec<&str> = contents.lines().filter(|l| l.contains(" REQ #")).collect();
        let res_lines: Vec<&str> = contents.lines().filter(|l| l.contains(" RES #")).collect();
        assert_eq!(req_lines.len(), 1, "one request line: {contents:?}");
        assert_eq!(res_lines.len(), 1, "one response line: {contents:?}");
        // The request and response share the same id, so an unanswered crash is
        // detectable as a REQ id with no matching RES id.
        let req_id = req_lines[0]
            .split(" REQ #")
            .nth(1)
            .unwrap()
            .split(' ')
            .next();
        let res_id = res_lines[0]
            .split(" RES #")
            .nth(1)
            .unwrap()
            .split(' ')
            .next();
        assert_eq!(req_id, res_id, "ids must match");
        // The logged request carries the raw SQL; the response carries the items.
        assert!(req_lines[0].contains("select * from Pe"), "{contents:?}");
        assert!(res_lines[0].contains(&out[..out.len().min(10)]) || res_lines[0].contains("items"));
    }

    #[test]
    fn logging_is_single_line_even_with_newlines() {
        let path = temp_log_path("oneline");
        let log = RequestLog::at(&path);
        let sql = "select 1\nfrom Pe";
        let _ = intellisense_complete_with_log(&request(sql, sql.len()), Some(&log)).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        // Newlines inside the SQL are escaped so each record stays one line.
        assert!(contents.contains("\\n"), "newline escaped: {contents:?}");
        let req_lines = contents.lines().filter(|l| l.contains(" REQ #")).count();
        assert_eq!(req_lines, 1, "request stays on one line: {contents:?}");
    }

    #[test]
    fn logging_records_error_for_bad_request() {
        let path = temp_log_path("err");
        let log = RequestLog::at(&path);
        let result = intellisense_complete_with_log("{not json", Some(&log));
        assert!(result.is_err());

        let contents = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);

        // A bad request still gets a REQ line (logged before parsing) plus an
        // ERR line — never silently dropped.
        assert!(contents.contains(" REQ #"), "{contents:?}");
        assert!(contents.contains(" ERR #"), "{contents:?}");
    }

    #[test]
    fn resolve_disabled_by_default() {
        let mut env = sqlly_testkit::EnvGuard::acquire();
        env.remove(LOG_ENV);
        assert!(RequestLog::resolve(false).is_none(), "off by default");
        assert!(
            RequestLog::resolve(true).is_some(),
            "flag enables default-path logging"
        );
    }

    #[test]
    fn no_log_file_created_when_disabled() {
        let before = default_log_path();
        let existed = before.exists();
        let sql = "select * from Pe";
        let _ = intellisense_complete_with_log(&request(sql, sql.len()), None).unwrap();
        // Disabled path must not create the default log file as a side effect.
        if !existed {
            assert!(
                !before.exists(),
                "disabled completion must not write a log file"
            );
        }
    }

    #[test]
    fn procedure_body_emits_alias_qualified_columns() {
        // IntelliSense inside a stored-procedure body must resolve the in-scope
        // table sources the same way it does outside. The routine body is
        // tolerantly scanned (control flow + statements the parser can't fully
        // model) and FROM/JOIN table references are harvested, so a WHERE column
        // prefix inside the body completes as `p.Name`.
        let sql =
            "CREATE PROCEDURE dbo.Thing AS\nBEGIN\n    SELECT * FROM dbo.Person p WHERE Na\nEND";
        let cursor = sql.find("Na").map(|i| i + 2).unwrap();
        let out = intellisense_complete(&request(sql, cursor)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let items = v["items"].as_array().unwrap();
        let name_item = items
            .iter()
            .find(|i| i["label"].as_str() == Some("p.Name"))
            .unwrap_or_else(|| panic!("missing p.Name inside proc body: {items:?}"));
        assert_eq!(name_item["kind"].as_str(), Some("column"));
        assert_eq!(name_item["insert_text"].as_str(), Some("p.Name"));
        assert_eq!(name_item["match_text"].as_str(), Some("Name"));
    }

    #[test]
    fn procedure_body_alias_dot_emits_columns() {
        // `alias.` access inside a proc body maps the alias to its table.
        let sql = "CREATE PROCEDURE dbo.Thing AS\n    SELECT * FROM dbo.Person p WHERE p.";
        let out = intellisense_complete(&request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(labels.contains(&"Id"), "alias-dot inside proc: {labels:?}");
        assert!(labels.contains(&"Name"));
    }

    #[test]
    fn procedure_body_tables_after_from() {
        // Table completion inside a proc body after FROM.
        let sql = "CREATE PROCEDURE dbo.Thing AS\nBEGIN\n    SELECT * FROM Pe\nEND";
        let cursor = sql.find("Pe").map(|i| i + 2).unwrap();
        let out = intellisense_complete(&request(sql, cursor)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(labels.iter().any(|l| l.contains("Person")), "{labels:?}");
    }

    #[test]
    fn procedure_body_join_harvests_second_table() {
        // A JOIN inside the proc body is an independent table source; its
        // columns resolve under the JOIN'd table's alias. `m.` lists Model's
        // columns (Id, Name) and excludes Person-only Age, proving the JOIN'd
        // table — not just the FROM table — was harvested.
        let sql = "CREATE PROCEDURE dbo.Thing AS\n    SELECT * FROM dbo.Person p JOIN dbo.Model m ON m.Id = p.Id WHERE m.";
        let out = intellisense_complete(&request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(
            labels.contains(&"Id"),
            "Model Id via JOIN alias: {labels:?}"
        );
        assert!(
            labels.contains(&"Name"),
            "Model Name via JOIN alias: {labels:?}"
        );
        assert!(
            !labels.contains(&"Age"),
            "Person-only column must not appear for alias m: {labels:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Cross-construct IntelliSense: the same completion behavior must hold
    // inside CREATE PROCEDURE, CREATE FUNCTION, CREATE VIEW, and CREATE TABLE
    // bodies as it does for a top-level statement. The schema fixture (see
    // `schema()`) provides dbo.Person[Id,Name,Age], dbo.Model[Id,Name],
    // dbo.ActivePeople(view)[Id,Name], and dbo.ImportAccounts(sproc).
    // -----------------------------------------------------------------------

    fn labels_at(sql: &str, cursor: usize) -> Vec<String> {
        let out = intellisense_complete(&request(sql, cursor)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap().to_string())
            .collect()
    }

    fn column_items_at(sql: &str, cursor: usize) -> Vec<serde_json::Value> {
        let out = intellisense_complete(&request(sql, cursor)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        v["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["kind"].as_str() == Some("column"))
            .cloned()
            .collect()
    }

    fn cursor_after(sql: &str, needle: &str) -> usize {
        sql.find(needle)
            .unwrap_or_else(|| panic!("needle {needle:?} not in sql"))
            + needle.len()
    }

    // ---- CREATE PROCEDURE -------------------------------------------------

    #[test]
    fn proc_body_control_flow_does_not_break_harvest() {
        // IF/ELSE control flow wraps the statement; the FROM inside the branch
        // is still harvested.
        let sql = "CREATE PROCEDURE dbo.X AS\nBEGIN\n  IF @a = 1\n    SELECT * FROM dbo.Person p WHERE Na\n  ELSE\n    SELECT 1\nEND";
        let cursor = cursor_after(sql, "WHERE Na");
        let items = column_items_at(sql, cursor);
        assert!(
            items.iter().any(|i| i["label"].as_str() == Some("p.Name")),
            "IF branch FROM must be harvested: {items:?}"
        );
    }

    #[test]
    fn proc_body_multi_statement_merges_table_scopes() {
        // Two SELECTs in one body: both tables are in scope for column
        // completion (known approximation; dedup handles exact dupes).
        let sql = "CREATE PROCEDURE dbo.X AS\nBEGIN\n  SELECT * FROM dbo.Person p\n  SELECT * FROM dbo.Model m WHERE Na\nEND";
        let cursor = cursor_after(sql, "WHERE Na");
        let items = column_items_at(sql, cursor);
        // Both p.Name (Person) and m.Name (Model) qualify on "Name".
        assert!(
            items.iter().any(|i| i["label"].as_str() == Some("p.Name")),
            "first statement table: {items:?}"
        );
        assert!(
            items.iter().any(|i| i["label"].as_str() == Some("m.Name")),
            "second statement table: {items:?}"
        );
    }

    // ---- CREATE FUNCTION --------------------------------------------------

    #[test]
    fn function_body_begin_end_emits_alias_qualified_columns() {
        // CREATE FUNCTION shares the routine-body harvest path with procedures.
        let sql = "CREATE FUNCTION dbo.Fn() RETURNS INT AS\nBEGIN\n  DECLARE @r INT\n  SELECT @r = Age FROM dbo.Person p WHERE Na\n  RETURN @r\nEND";
        let cursor = cursor_after(sql, "WHERE Na");
        let items = column_items_at(sql, cursor);
        assert!(
            items.iter().any(|i| i["label"].as_str() == Some("p.Name")),
            "function body must qualify columns with alias: {items:?}"
        );
        assert_eq!(
            items
                .iter()
                .find(|i| i["label"].as_str() == Some("p.Name"))
                .unwrap()["match_text"],
            "Name"
        );
    }

    #[test]
    fn function_inline_tvf_parenthesized_body_harvests_from() {
        // Inline table-valued function: RETURN (SELECT ... FROM ...). The FROM
        // sits inside parentheses; the harvest scan is paren-agnostic.
        let sql = "CREATE FUNCTION dbo.Tvf() RETURNS TABLE AS\nRETURN (SELECT * FROM dbo.Person p WHERE p.)";
        let cursor = cursor_after(sql, "WHERE p.");
        let labels = labels_at(sql, cursor);
        assert!(
            labels.contains(&"Id".to_string()),
            "TVF alias-dot: {labels:?}"
        );
        assert!(labels.contains(&"Name".to_string()));
        assert!(labels.contains(&"Age".to_string()));
    }

    #[test]
    fn function_body_alias_dot_resolves() {
        let sql = "CREATE FUNCTION dbo.Fn() RETURNS INT AS\nBEGIN\n  SELECT * FROM dbo.Model m WHERE m.\nEND";
        let cursor = cursor_after(sql, "WHERE m.");
        let labels = labels_at(sql, cursor);
        assert!(labels.contains(&"Id".to_string()), "{labels:?}");
        assert!(labels.contains(&"Name".to_string()));
    }

    // ---- CREATE VIEW ------------------------------------------------------

    #[test]
    fn view_body_tables_after_from() {
        let sql = "CREATE VIEW dbo.V AS SELECT * FROM Pe";
        let cursor = cursor_after(sql, "FROM Pe");
        let labels = labels_at(sql, cursor);
        assert!(
            labels.iter().any(|l| l.contains("Person")),
            "view FROM must complete tables: {labels:?}"
        );
    }

    #[test]
    fn view_body_unqualified_columns_after_where() {
        // Unaliased view body: bare columns complete.
        let sql = "CREATE VIEW dbo.V AS SELECT * FROM dbo.Person WHERE Na";
        let cursor = cursor_after(sql, "WHERE Na");
        let items = column_items_at(sql, cursor);
        assert!(
            items.iter().any(|i| i["label"].as_str() == Some("Name")),
            "view WHERE must complete columns: {items:?}"
        );
    }

    #[test]
    fn view_body_alias_dot_resolves() {
        // The view parser captures aliases (fixed), so `p.` resolves Person.
        let sql = "CREATE VIEW dbo.V AS SELECT * FROM dbo.Person p WHERE p.";
        let labels = labels_at(sql, sql.len());
        assert!(
            labels.contains(&"Id".to_string()),
            "view alias-dot: {labels:?}"
        );
        assert!(labels.contains(&"Name".to_string()));
        assert!(labels.contains(&"Age".to_string()));
    }

    #[test]
    fn view_body_aliased_where_qualifies_columns() {
        // Aliased view body: a bare WHERE prefix emits alias-qualified columns.
        let sql = "CREATE VIEW dbo.V AS SELECT * FROM dbo.Person p WHERE Na";
        let cursor = cursor_after(sql, "WHERE Na");
        let items = column_items_at(sql, cursor);
        assert!(
            items.iter().any(|i| i["label"].as_str() == Some("p.Name")),
            "view aliased body must qualify columns: {items:?}"
        );
    }

    #[test]
    fn view_body_join_harvests_second_table() {
        let sql = "CREATE VIEW dbo.V AS SELECT * FROM dbo.Person p JOIN dbo.Model m ON m.Id = p.Id WHERE m.";
        let labels = labels_at(sql, sql.len());
        assert!(
            labels.contains(&"Id".to_string()),
            "view JOIN alias: {labels:?}"
        );
        assert!(labels.contains(&"Name".to_string()));
        assert!(
            !labels.contains(&"Age".to_string()),
            "Person-only column must not appear for alias m: {labels:?}"
        );
    }

    // ---- CREATE TABLE -----------------------------------------------------

    #[test]
    fn create_table_fk_references_completes_target_table() {
        // Inside a CREATE TABLE column definition, `REFERENCES <table>` must
        // still complete tables (general object completion path).
        let sql =
            "CREATE TABLE dbo.Orders (\n  Id INT PRIMARY KEY,\n  PersonId INT REFERENCES Pe\n)";
        let cursor = cursor_after(sql, "REFERENCES Pe");
        let labels = labels_at(sql, cursor);
        assert!(
            labels.iter().any(|l| l.contains("Person")),
            "FK REFERENCES must complete target table: {labels:?}"
        );
    }

    #[test]
    fn create_table_body_does_not_emit_its_own_columns() {
        // The table being declared is not yet in the model, so its in-progress
        // column names must not self-complete (only existing schema objects do).
        let sql = "CREATE TABLE dbo.NewThing (\n  Id INT,\n  Secret\n)";
        let cursor = cursor_after(sql, "Secret");
        let labels = labels_at(sql, cursor);
        assert!(
            !labels.iter().any(|l| l == "Secret"),
            "undeclared column must not self-complete: {labels:?}"
        );
    }

    // ---- Parity: inside-body == top-level ---------------------------------

    #[test]
    fn proc_body_completion_matches_top_level() {
        // The same SELECT, with and without a proc wrapper, yields the same
        // qualifying column completion.
        let bare = "select * from dbo.Person p where Na";
        let wrapped = format!("CREATE PROCEDURE dbo.X AS BEGIN {} END", bare);
        let top = column_items_at(bare, cursor_after(bare, "where Na"));
        let inside = column_items_at(&wrapped, cursor_after(&wrapped, "where Na"));
        let top_labels: Vec<String> = top
            .iter()
            .map(|i| i["label"].as_str().unwrap().to_string())
            .collect();
        let inside_labels: Vec<String> = inside
            .iter()
            .map(|i| i["label"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            top_labels, inside_labels,
            "inside-proc completion must match top-level: {top_labels:?} vs {inside_labels:?}"
        );
    }

    #[test]
    fn metadata_aliases_mirror_canonical_fields_on_wire() {
        // An object completion after FROM carries display/replacement/
        // completion_type aliases that mirror label/insert_text/kind, plus the
        // structural metadata (name, schema, why).
        let sql = "select * from Pe";
        let out = intellisense_complete(&request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let items = v["items"].as_array().unwrap();
        let person = items
            .iter()
            .find(|i| i["label"].as_str() == Some("dbo.Person"))
            .unwrap_or_else(|| panic!("missing Person: {items:?}"));

        // Aliases mirror the canonical fields verbatim.
        assert_eq!(person["display"], person["label"]);
        assert_eq!(person["replacement"], person["insert_text"]);
        assert_eq!(person["completion_type"], person["kind"]);
        assert_eq!(person["completion_type"].as_str(), Some("table"));
        // display (qualified label) and replacement (qualified insert) match:
        // accepting the completion inserts the schema-qualified name, never the
        // bare name with the schema cut off.
        assert_eq!(person["display"].as_str(), Some("dbo.Person"));
        assert_eq!(person["replacement"].as_str(), Some("dbo.Person"));

        // Structural metadata.
        assert_eq!(person["name"].as_str(), Some("Person"));
        assert_eq!(person["schema"].as_str(), Some("dbo"));
        assert_eq!(person["why"].as_str(), Some("table"));

        // A table has no parent/value: those keys are omitted entirely.
        assert!(person.get("parent").is_none(), "parent omitted: {person}");
        assert!(person.get("parent_type").is_none());
        assert!(person.get("value").is_none());
    }

    #[test]
    fn metadata_alias_dot_column_carries_parent_and_reason_on_wire() {
        let sql = "select * from dbo.Person p where p.";
        let out = intellisense_complete(&request(sql, sql.len())).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let items = v["items"].as_array().unwrap();
        let name = items
            .iter()
            .find(|i| i["label"].as_str() == Some("Name"))
            .unwrap_or_else(|| panic!("missing Name: {items:?}"));

        assert_eq!(name["completion_type"].as_str(), Some("column"));
        assert_eq!(name["name"].as_str(), Some("Name"));
        assert_eq!(name["why"].as_str(), Some("alias"));
        assert_eq!(name["alias"].as_str(), Some("p"));
        assert_eq!(name["parent"].as_str(), Some("Person"));
        // parent_type uses the same vocabulary as `kind`.
        assert_eq!(name["parent_type"].as_str(), Some("table"));
        assert_eq!(name["schema"].as_str(), Some("dbo"));
    }

    #[test]
    fn metadata_type_field_is_named_type_on_wire() {
        // The data-type field must serialize as exactly "type" (not "type_name"
        // or "r#type"). Value-mode completions carry it; here we just assert the
        // serde rename never leaks the Rust identifier for any item that has it.
        let sql = "select * from dbo.Person p where p.";
        let out = intellisense_complete(&request(sql, sql.len())).unwrap();
        assert!(
            !out.contains("type_name") && !out.contains("r#type"),
            "type field must serialize as `type`, got: {out}"
        );
    }

    #[test]
    fn unknown_object_kind_is_skipped_not_table() {
        let req = serde_json::json!({
            "sql": "select * from ",
            "cursor": 14,
            "dialect": "sqlserver",
            "default_schema": "dbo",
            "objects": [
                {"schema": "dbo", "name": "RealTable", "kind": "table", "columns": ["Id"]},
                {"schema": "dbo", "name": "GhostTrigger", "kind": "trigger", "columns": []}
            ],
            "foreign_keys": []
        })
        .to_string();
        let out = intellisense_complete(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(labels.iter().any(|l| l.contains("RealTable")), "{labels:?}");
        assert!(
            !labels.iter().any(|l| l.contains("GhostTrigger")),
            "unknown kind must not appear as table: {labels:?}"
        );
    }

    #[test]
    fn procedure_aliases_are_recognized() {
        for alias in &[
            "sproc",
            "procedure",
            "proc",
            "stored_procedure",
            "storedprocedure",
        ] {
            let req = serde_json::json!({
                "sql": "exec ",
                "cursor": 5,
                "dialect": "sqlserver",
                "default_schema": "dbo",
                "objects": [
                    {"schema": "dbo", "name": "DoWork", "kind": alias, "columns": [],
                     "parameters": [{"name": "@Id", "type_name": "int", "is_output": false}]}
                ],
                "foreign_keys": []
            })
            .to_string();
            let out = intellisense_complete(&req).unwrap();
            let v: serde_json::Value = serde_json::from_str(&out).unwrap();
            let items = v["items"].as_array().unwrap();
            let proc = items
                .iter()
                .find(|i| i["label"].as_str() == Some("dbo.DoWork"));
            assert!(
                proc.is_some(),
                "alias `{alias}` must produce a procedure completion: {items:?}"
            );
            assert_eq!(
                proc.unwrap()["kind"].as_str(),
                Some("procedure"),
                "alias `{alias}` must map to procedure kind"
            );
        }
    }

    #[test]
    fn empty_kind_is_skipped_not_table() {
        let req = serde_json::json!({
            "sql": "select * from ",
            "cursor": 14,
            "dialect": "sqlserver",
            "default_schema": "dbo",
            "objects": [
                {"schema": "dbo", "name": "RealTable", "kind": "table", "columns": ["Id"]},
                {"schema": "dbo", "name": "MysteryObj", "kind": "", "columns": []}
            ],
            "foreign_keys": []
        })
        .to_string();
        let out = intellisense_complete(&req).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let labels: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["label"].as_str().unwrap())
            .collect();
        assert!(
            !labels.iter().any(|l| l.contains("MysteryObj")),
            "empty kind must not appear as table: {labels:?}"
        );
    }
}
