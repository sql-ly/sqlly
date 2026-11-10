//! Query AI summary state + per-document metadata persistence.
//!
//! Port of `QueryAISummaryState.swift` + `QueryAISummaryMetadata.swift`.
//! Pure, UI-free, serde-serializable. No I/O except the `MetadataStore`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

// ---------------------------------------------------------------------------
// Summary kind
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QueryAISummaryKind {
    #[allow(dead_code)]
    Short,
    Long,
}

impl QueryAISummaryKind {
    #[allow(dead_code)]
    pub fn all() -> &'static [QueryAISummaryKind] {
        &[QueryAISummaryKind::Short, QueryAISummaryKind::Long]
    }

    pub fn instructions(self) -> &'static str {
        match self {
            Self::Short => "Generate a 15 word max summary of what this does.",
            Self::Long => "Generate a 40 word max summary of what this does.",
        }
    }

    /// Instructions with an explicit word budget for the long summary. The
    /// short summary is always capped at 15 words regardless of the budget;
    /// the long summary uses `long_word_budget` (derived from the width of the
    /// query window, see [`word_budget_for_width`]) and falls back to the
    /// fixed instructions when no budget is known.
    pub fn instructions_for_budget(self, long_word_budget: Option<usize>) -> String {
        match (self, long_word_budget) {
            (Self::Long, Some(words)) => {
                format!("Generate a {words} word max summary of what this does.")
            }
            _ => self.instructions().to_string(),
        }
    }

    #[allow(dead_code)]
    pub fn pending_text(self) -> String {
        let key = match self {
            Self::Short => "query.ai_summary.short_pending",
            Self::Long => "query.ai_summary.long_pending",
        };
        crate::localization::text(key)
    }
}

/// Rough word budget for a summary that should fit on one line of the given
/// pixel width: ~7px per character at UI font sizes, ~6 characters per word
/// (incl. the trailing space), clamped to a sane 10–60 word range.
pub fn word_budget_for_width(width_px: f32) -> usize {
    let chars = width_px / 7.0;
    let words = (chars / 6.0).floor() as usize;
    words.clamp(10, 60)
}

/// Rough pixel width `text` occupies on one line, using the same ~7px per
/// character metric as [`word_budget_for_width`]. Used to decide whether a
/// summary will clip in the space available to it.
pub fn estimated_text_width(text: &str) -> f32 {
    text.chars().count() as f32 * 7.0
}

/// Flatten possibly block-level markdown to one inline line. The summary bar
/// renders in a fixed-height, overflow-clipped row, so an LLM answer that
/// arrives as headings, list items, blockquotes, or fenced code would paint
/// as a clipped first block with the rest invisible. Block markers are
/// stripped and lines joined with spaces; inline markdown (emphasis, inline
/// code) passes through for the `TextView::markdown` renderer.
pub fn flatten_markdown_to_inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim();
        // Fence markers toggle and vanish; fenced content joins as plain text.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let mut rest = trimmed;
        if !in_fence {
            // `# Heading` markers (only with the space — `#42` is content).
            let hashes = rest.bytes().take_while(|b| *b == b'#').count();
            if hashes > 0 && rest.as_bytes().get(hashes) == Some(&b' ') {
                rest = rest[hashes + 1..].trim_start();
            }
            // `> blockquote`, possibly nested.
            while let Some(unquoted) = rest.strip_prefix('>') {
                rest = unquoted.trim_start();
            }
            // Bulleted / ordered list markers.
            if let Some(unmarked) = rest
                .strip_prefix("- ")
                .or_else(|| rest.strip_prefix("* "))
                .or_else(|| rest.strip_prefix("+ "))
            {
                rest = unmarked.trim_start();
            } else {
                let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
                if digits > 0 && rest[digits..].starts_with(". ") {
                    rest = rest[digits + 2..].trim_start();
                }
            }
        }
        if rest.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(rest);
    }
    out
}

// ---------------------------------------------------------------------------
// Summary state (in-memory, not persisted)
// ---------------------------------------------------------------------------

/// Minimum time that must elapse between two AI summary runs for the same
/// document.
pub const MINIMUM_RUN_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryAISummaryState {
    pub short_summary: Option<String>,
    pub long_summary: Option<String>,
    short_needs_refresh: bool,
    long_needs_refresh: bool,
    last_run: Option<Instant>,
}

impl Default for QueryAISummaryState {
    fn default() -> Self {
        Self {
            short_summary: None,
            long_summary: None,
            short_needs_refresh: true,
            long_needs_refresh: true,
            last_run: None,
        }
    }
}

impl QueryAISummaryState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_record(record: &QueryAISummaryRecord) -> Self {
        Self {
            short_summary: record.short_summary.clone(),
            long_summary: record.long_summary.clone(),
            short_needs_refresh: record.short_summary.is_none(),
            long_needs_refresh: record.long_summary.is_none(),
            last_run: None,
        }
    }

    /// Build a record from the summary state plus optional connection and
    /// resume metadata. Mirrors Swift `makeAISummaryRecord()` which assembles
    /// the full record (summaries + connection + resume) when flushing.
    pub fn make_record_with(
        &self,
        connection: Option<&QueryConnectionMetadata>,
        resume: Option<&QueryResumeMetadata>,
    ) -> QueryAISummaryRecord {
        QueryAISummaryRecord {
            short_summary: self.short_summary.clone(),
            long_summary: self.long_summary.clone(),
            connection: connection.cloned(),
            resume: resume.cloned(),
        }
    }

    /// Convenience: build a record with only the summary (no resume or
    /// connection metadata). Kept for backward compatibility with call sites
    /// that have not yet been migrated.
    #[allow(dead_code)]
    pub fn make_record(&self) -> QueryAISummaryRecord {
        self.make_record_with(None, None)
    }

    pub fn mark_content_changed(&mut self) {
        self.short_needs_refresh = true;
        self.long_needs_refresh = true;
    }

    pub fn mark_needs_refresh(&mut self, kind: QueryAISummaryKind) {
        match kind {
            QueryAISummaryKind::Short => self.short_needs_refresh = true,
            QueryAISummaryKind::Long => self.long_needs_refresh = true,
        }
    }

    /// Returns `true` when a run for `kind` should start now: the kind must be
    /// dirty and the throttle interval must have elapsed.
    pub fn can_run(&self, kind: QueryAISummaryKind, now: Instant) -> bool {
        let dirty = match kind {
            QueryAISummaryKind::Short => self.short_needs_refresh,
            QueryAISummaryKind::Long => self.long_needs_refresh,
        };
        if !dirty {
            return false;
        }
        match self.last_run {
            None => true,
            Some(last) => now.duration_since(last) >= MINIMUM_RUN_INTERVAL,
        }
    }

    pub fn set_summary(&mut self, kind: QueryAISummaryKind, summary: String, now: Instant) {
        match kind {
            QueryAISummaryKind::Short => {
                self.short_summary = Some(summary);
                self.short_needs_refresh = false;
            }
            QueryAISummaryKind::Long => {
                self.long_summary = Some(summary);
                self.long_needs_refresh = false;
            }
        }
        self.last_run = Some(now);
    }

    #[allow(dead_code)]
    pub fn needs_refresh(&self, kind: QueryAISummaryKind) -> bool {
        match kind {
            QueryAISummaryKind::Short => self.short_needs_refresh,
            QueryAISummaryKind::Long => self.long_needs_refresh,
        }
    }

    pub fn summary(&self, kind: QueryAISummaryKind) -> Option<&str> {
        match kind {
            QueryAISummaryKind::Short => self.short_summary.as_deref(),
            QueryAISummaryKind::Long => self.long_summary.as_deref(),
        }
    }
}

// ---------------------------------------------------------------------------
// Connection metadata
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueryConnectionMetadata {
    pub server: String,
    pub database: Option<String>,
}

// ---------------------------------------------------------------------------
// Resume metadata (per-tab query-state fields)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct QueryResumeMetadata {
    /// Per-tab override (`nil` follows the global scope, `true`/`false` forces).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub override_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_layout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff_mode_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fim_autocomplete_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppress_first_run_confirmation: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multi_target_ids: Option<Vec<String>>,
}

impl QueryResumeMetadata {
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.override_enabled.is_none()
            && self.result_layout.is_none()
            && self.diff_mode_enabled.is_none()
            && self.fim_autocomplete_enabled.is_none()
            && self.suppress_first_run_confirmation.is_none()
            && self.multi_target_ids.is_none()
    }
}

// ---------------------------------------------------------------------------
// Per-document record
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct QueryAISummaryRecord {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub long_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<QueryConnectionMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<QueryResumeMetadata>,
}

impl QueryAISummaryRecord {
    pub fn is_empty(&self) -> bool {
        self.short_summary.is_none()
            && self.long_summary.is_none()
            && self.connection.is_none()
            && self.resume.as_ref().map(|r| r.is_empty()).unwrap_or(true)
    }
}

// ---------------------------------------------------------------------------
// Metadata collection (keyed by document key)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct QueryAISummaryMetadata {
    #[serde(default)]
    pub records: BTreeMap<String, QueryAISummaryRecord>,
}

impl QueryAISummaryMetadata {
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.records.values().all(|r| r.is_empty())
    }

    pub fn get(&self, key: &str) -> Option<&QueryAISummaryRecord> {
        self.records.get(key)
    }

    pub fn set(&mut self, key: impl Into<String>, record: QueryAISummaryRecord) {
        self.records.insert(key.into(), record);
    }

    #[allow(dead_code)]
    pub fn remove(&mut self, key: &str) {
        self.records.remove(key);
    }

    /// Move the record stored under `old_key` to `new_key`. No-op when the keys
    /// are equal or there is no source record. If a non-empty record already
    /// exists at `new_key` it is preserved (destination wins) and only the
    /// stale source record is removed.
    pub fn migrate(&mut self, old_key: &str, new_key: &str) {
        if old_key == new_key {
            return;
        }
        let Some(record) = self.records.remove(old_key) else {
            return;
        };
        if let Some(existing) = self.records.get(new_key)
            && !existing.is_empty()
        {
            return;
        }
        self.records.insert(new_key.to_string(), record);
    }

    /// The record under `key`; failing that, the first record found under
    /// one of `legacy_keys` (keys older builds used, such as a tab's display
    /// name), moved to `key` so later saves and lookups use the new key.
    pub fn get_or_adopt(
        &mut self,
        key: &str,
        legacy_keys: &[String],
    ) -> Option<&QueryAISummaryRecord> {
        if !self.records.contains_key(key)
            && let Some(legacy) = legacy_keys
                .iter()
                .find(|legacy| legacy.as_str() != key && self.records.contains_key(legacy.as_str()))
                .cloned()
        {
            self.migrate(&legacy, key);
        }
        self.records.get(key)
    }
}

// ---------------------------------------------------------------------------
// Store (file I/O, fault-tolerant)
// ---------------------------------------------------------------------------

pub struct QueryAISummaryMetadataStore;

impl QueryAISummaryMetadataStore {
    pub fn load(path: &Path) -> QueryAISummaryMetadata {
        #[cfg(target_arch = "wasm32")]
        return {
            let _ = path;
            crate::host_storage::load_text(
                crate::host_storage::StorageArea::Config,
                "query-ai-summary.json",
            )
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Ok(data) = std::fs::read(path) else {
                return QueryAISummaryMetadata::default();
            };
            serde_json::from_slice::<QueryAISummaryMetadata>(&data).unwrap_or_default()
        }
    }

    pub fn save(metadata: &QueryAISummaryMetadata, path: &Path) -> std::io::Result<()> {
        #[cfg(target_arch = "wasm32")]
        return {
            let _ = path;
            let json = serde_json::to_string_pretty(metadata).map_err(std::io::Error::other)?;
            crate::host_storage::store_text(
                crate::host_storage::StorageArea::Config,
                "query-ai-summary.json",
                &json,
            )
            .map_err(std::io::Error::other)
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let json = serde_json::to_string_pretty(metadata).map_err(std::io::Error::other)?;
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, json)?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP generation (Ollama /api/generate)
// ---------------------------------------------------------------------------

/// The local AI runtime (Ollama) endpoint used when no override is given.
pub const DEFAULT_OLLAMA_BASE_URL: &str = "http://127.0.0.1:11434";

/// Resolve a base URL: empty or whitespace-only falls back to the default
/// local runtime endpoint. Pure, so the Preferences pane can display exactly
/// the endpoint the summary requests will hit.
pub fn effective_base_url(base_url: &str) -> &str {
    let trimmed = base_url.trim();
    if trimmed.is_empty() {
        DEFAULT_OLLAMA_BASE_URL
    } else {
        trimmed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryError {
    InvalidRequest,
    HttpFailed(String),
    InvalidResponse,
}

impl std::fmt::Display for SummaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest => f.write_str(&crate::localization::text(
                "query.ai_summary.invalid_request",
            )),
            Self::HttpFailed(msg) => f.write_str(&crate::localization::format(
                "query.ai_summary.http_failed",
                &[("message", msg)],
            )),
            Self::InvalidResponse => f.write_str(&crate::localization::text(
                "query.ai_summary.invalid_response",
            )),
        }
    }
}

impl std::error::Error for SummaryError {}

#[derive(serde::Serialize)]
struct SummaryGenerateRequest {
    model: String,
    prompt: String,
    stream: bool,
    options: SummaryOptions,
}

#[derive(serde::Serialize)]
struct SummaryOptions {
    temperature: f32,
    num_predict: u32,
}

#[derive(serde::Deserialize)]
struct SummaryGenerateResponse {
    response: String,
}

/// Generate a natural-language summary of `sql` via Ollama's `/api/generate`
/// endpoint, mirroring the Swift `FIMCompletionService` / playground runner
/// pattern. `base_url` defaults to `http://127.0.0.1:11434` when empty.
#[allow(dead_code)]
pub async fn generate_summary(
    sql: &str,
    kind: QueryAISummaryKind,
    base_url: &str,
    model: &str,
) -> Result<String, SummaryError> {
    generate_summary_with_budget(sql, kind, base_url, model, None).await
}

/// Like [`generate_summary`], but sizes the long summary to an explicit word
/// budget (see [`QueryAISummaryKind::instructions_for_budget`]). The short
/// summary ignores the budget and stays at its fixed 15-word cap.
pub async fn generate_summary_with_budget(
    sql: &str,
    kind: QueryAISummaryKind,
    base_url: &str,
    model: &str,
    long_word_budget: Option<usize>,
) -> Result<String, SummaryError> {
    if sql.trim().is_empty() || model.is_empty() {
        return Err(SummaryError::InvalidRequest);
    }

    #[cfg(target_arch = "wasm32")]
    return Err(SummaryError::HttpFailed(
        crate::browser_preview::unavailable(&crate::localization::text(
            "query.ai_summary.browser_unavailable",
        ))
        .to_string(),
    ));

    #[cfg(not(target_arch = "wasm32"))]
    {
        let base = effective_base_url(base_url);
        let url = format!("{base}/api/generate");

        let prompt = format!(
            "{}\n\nSQL:\n```sql\n{}\n```\n",
            kind.instructions_for_budget(long_word_budget),
            sql
        );

        let body = serde_json::to_string(&SummaryGenerateRequest {
            model: model.to_string(),
            prompt,
            stream: false,
            options: SummaryOptions {
                temperature: 0.3,
                num_predict: 128,
            },
        })
        .map_err(|_| SummaryError::InvalidRequest)?;

        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| SummaryError::HttpFailed(e.to_string()))?;

        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| SummaryError::HttpFailed(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(SummaryError::HttpFailed(crate::localization::format(
                "query.ai_summary.generate_http_status",
                &[("status", &resp.status().to_string())],
            )));
        }

        let parsed: SummaryGenerateResponse = resp
            .json()
            .await
            .map_err(|_| SummaryError::InvalidResponse)?;

        Ok(parsed.response.trim().to_string())
    }
}

// ---------------------------------------------------------------------------
// Runtime health (Ollama /api/tags)
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagsModel>,
}

#[derive(serde::Deserialize)]
struct TagsModel {
    name: String,
}

/// Parse the JSON body of Ollama's `/api/tags` response into the installed
/// model names, in the order the runtime reports them. Pure — the HTTP fetch
/// lives in [`list_installed_models`] so this parsing stays unit-testable.
pub fn parse_installed_models(body: &str) -> Result<Vec<String>, SummaryError> {
    let parsed: TagsResponse =
        serde_json::from_str(body).map_err(|_| SummaryError::InvalidResponse)?;
    Ok(parsed.models.into_iter().map(|m| m.name).collect())
}

/// Probe the local AI runtime by fetching `{base}/api/tags` and return the
/// installed model names. An empty `base_url` uses the default local endpoint
/// (see [`effective_base_url`]). A connection failure or non-2xx status comes
/// back as [`SummaryError::HttpFailed`] with the underlying error text.
pub async fn list_installed_models(base_url: &str) -> Result<Vec<String>, SummaryError> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = base_url;
        return Err(SummaryError::HttpFailed(
            crate::browser_preview::unavailable(&crate::localization::text(
                "query.ai_summary.runtime_unreachable_browser",
            ))
            .to_string(),
        ));
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let base = effective_base_url(base_url);
        let url = format!("{base}/api/tags");

        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| SummaryError::HttpFailed(e.to_string()))?;

        let resp = client
            .get(&url)
            .send()
            .await
            .map_err(|e| SummaryError::HttpFailed(e.to_string()))?;

        if !resp.status().is_success() {
            return Err(SummaryError::HttpFailed(crate::localization::format(
                "query.ai_summary.models_http_status",
                &[("status", &resp.status().to_string())],
            )));
        }

        let body = resp
            .text()
            .await
            .map_err(|_| SummaryError::InvalidResponse)?;
        parse_installed_models(&body)
    }
}

/// Default file path for the per-query AI summary metadata store, mirroring
/// the Swift `~/Library/Application Support/SQLLY/query-ai-summary.json`.
pub fn ai_summary_metadata_file() -> Option<std::path::PathBuf> {
    #[cfg(not(target_arch = "wasm32"))]
    return dirs::config_dir().map(|d| d.join("SQLLY").join("query-ai-summary.json"));
    // Browser build: nominal path only — the store reads/writes localStorage.
    #[cfg(target_arch = "wasm32")]
    return Some(std::path::PathBuf::from("query-ai-summary.json"));
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_markdown_leaves_inline_text_alone() {
        assert_eq!(
            flatten_markdown_to_inline("Counts *orders* per `region`."),
            "Counts *orders* per `region`."
        );
    }

    #[test]
    fn flatten_markdown_strips_block_markers_and_joins_lines() {
        let text = "# Summary\n\n- counts orders\n- groups by region\n\n> top 10 only";
        assert_eq!(
            flatten_markdown_to_inline(text),
            "Summary counts orders groups by region top 10 only"
        );
    }

    #[test]
    fn flatten_markdown_unwraps_fenced_code_and_ordered_lists() {
        let text = "1. first\n2. second\n```sql\nSELECT 1\n```";
        assert_eq!(flatten_markdown_to_inline(text), "first second SELECT 1");
    }

    #[test]
    fn flatten_markdown_keeps_hash_content_without_space() {
        // `#42` is content, not a heading marker.
        assert_eq!(flatten_markdown_to_inline("#42 is open"), "#42 is open");
    }

    #[test]
    fn effective_base_url_empty_uses_default() {
        assert_eq!(effective_base_url(""), DEFAULT_OLLAMA_BASE_URL);
        assert_eq!(effective_base_url("   "), DEFAULT_OLLAMA_BASE_URL);
    }

    #[test]
    fn effective_base_url_passes_through_and_trims() {
        assert_eq!(
            effective_base_url("http://10.0.0.5:11434"),
            "http://10.0.0.5:11434"
        );
        assert_eq!(
            effective_base_url(" http://10.0.0.5:11434 "),
            "http://10.0.0.5:11434"
        );
    }

    #[test]
    fn parse_installed_models_extracts_names_in_order() {
        // A trimmed-down real /api/tags body: extra keys must be ignored.
        let body = r#"{
            "models": [
                {"name": "qwen2.5-coder:1.5b", "modified_at": "2026-08-01T00:00:00Z",
                 "size": 986061810, "digest": "abc"},
                {"name": "llama3.2:latest", "size": 2019393189}
            ]
        }"#;
        assert_eq!(
            parse_installed_models(body).unwrap(),
            vec![
                "qwen2.5-coder:1.5b".to_string(),
                "llama3.2:latest".to_string()
            ]
        );
    }

    #[test]
    fn parse_installed_models_empty_list_and_missing_key() {
        assert!(
            parse_installed_models(r#"{"models": []}"#)
                .unwrap()
                .is_empty()
        );
        // `models` absent entirely: serde(default) makes that an empty list,
        // not an error — a fresh install with no models yet is not a failure.
        assert!(parse_installed_models("{}").unwrap().is_empty());
    }

    #[test]
    fn parse_installed_models_invalid_json_is_invalid_response() {
        assert_eq!(
            parse_installed_models("not json"),
            Err(SummaryError::InvalidResponse)
        );
        assert_eq!(
            parse_installed_models(r#"{"models": "nope"}"#),
            Err(SummaryError::InvalidResponse)
        );
    }

    #[test]
    fn summary_kind_instructions() {
        assert!(QueryAISummaryKind::Short.instructions().contains("15"));
        assert!(QueryAISummaryKind::Long.instructions().contains("40"));
    }

    #[test]
    fn short_instructions_ignore_budget() {
        assert!(
            QueryAISummaryKind::Short
                .instructions_for_budget(Some(50))
                .contains("15")
        );
    }

    #[test]
    fn long_instructions_use_budget() {
        assert!(
            QueryAISummaryKind::Long
                .instructions_for_budget(Some(25))
                .contains("25")
        );
        // No budget known → fixed default.
        assert!(
            QueryAISummaryKind::Long
                .instructions_for_budget(None)
                .contains("40")
        );
    }

    #[test]
    fn word_budget_scales_with_width_and_clamps() {
        assert_eq!(word_budget_for_width(0.0), 10);
        assert_eq!(word_budget_for_width(10_000.0), 60);
        let narrow = word_budget_for_width(600.0);
        let wide = word_budget_for_width(1_400.0);
        assert!(narrow < wide, "{narrow} < {wide}");
    }

    #[test]
    fn estimated_text_width_counts_chars() {
        assert_eq!(estimated_text_width(""), 0.0);
        assert_eq!(estimated_text_width("abcd"), 28.0);
    }

    #[test]
    fn summary_kind_pending_text() {
        assert!(!QueryAISummaryKind::Short.pending_text().is_empty());
        assert!(!QueryAISummaryKind::Long.pending_text().is_empty());
    }

    #[test]
    fn summary_kind_all_has_two() {
        assert_eq!(QueryAISummaryKind::all().len(), 2);
    }

    #[test]
    fn state_defaults_need_refresh() {
        let s = QueryAISummaryState::new();
        assert!(s.needs_refresh(QueryAISummaryKind::Short));
        assert!(s.needs_refresh(QueryAISummaryKind::Long));
        assert!(s.can_run(QueryAISummaryKind::Short, Instant::now()));
    }

    #[test]
    fn state_mark_content_changed_sets_both_dirty() {
        let mut s = QueryAISummaryState::new();
        s.short_needs_refresh = false;
        s.long_needs_refresh = false;
        s.mark_content_changed();
        assert!(s.needs_refresh(QueryAISummaryKind::Short));
        assert!(s.needs_refresh(QueryAISummaryKind::Long));
    }

    #[test]
    fn state_mark_needs_refresh_single_kind() {
        let mut s = QueryAISummaryState::new();
        s.short_needs_refresh = false;
        s.long_needs_refresh = false;
        s.mark_needs_refresh(QueryAISummaryKind::Short);
        assert!(s.needs_refresh(QueryAISummaryKind::Short));
        assert!(!s.needs_refresh(QueryAISummaryKind::Long));
    }

    #[test]
    fn state_set_summary_clears_dirty() {
        let mut s = QueryAISummaryState::new();
        let now = Instant::now();
        s.set_summary(QueryAISummaryKind::Short, "summary".into(), now);
        assert!(!s.needs_refresh(QueryAISummaryKind::Short));
        assert_eq!(s.summary(QueryAISummaryKind::Short), Some("summary"));
    }

    #[test]
    fn state_can_run_throttle() {
        let mut s = QueryAISummaryState::new();
        let now = Instant::now();
        s.set_summary(QueryAISummaryKind::Short, "first".into(), now);
        s.mark_content_changed();
        // Immediately after a run, can't run again.
        assert!(!s.can_run(QueryAISummaryKind::Short, now));
        assert!(!s.can_run(QueryAISummaryKind::Short, now + Duration::from_secs(5)));
        // After the interval, can run.
        assert!(s.can_run(QueryAISummaryKind::Short, now + MINIMUM_RUN_INTERVAL));
    }

    #[test]
    fn state_from_record_restores_summaries() {
        let record = QueryAISummaryRecord {
            short_summary: Some("short".into()),
            long_summary: Some("long".into()),
            connection: None,
            resume: None,
        };
        let s = QueryAISummaryState::from_record(&record);
        assert_eq!(s.short_summary.as_deref(), Some("short"));
        assert_eq!(s.long_summary.as_deref(), Some("long"));
        assert!(!s.needs_refresh(QueryAISummaryKind::Short));
    }

    #[test]
    fn state_from_record_none_summaries_need_refresh() {
        let record = QueryAISummaryRecord::default();
        let s = QueryAISummaryState::from_record(&record);
        assert!(s.needs_refresh(QueryAISummaryKind::Short));
        assert!(s.needs_refresh(QueryAISummaryKind::Long));
    }

    #[test]
    fn state_make_record_extracts_summaries() {
        let mut s = QueryAISummaryState::new();
        s.short_summary = Some("short".into());
        s.long_summary = Some("long".into());
        let record = s.make_record();
        assert_eq!(record.short_summary.as_deref(), Some("short"));
        assert_eq!(record.long_summary.as_deref(), Some("long"));
    }

    #[test]
    fn multi_target_ids_round_trip_and_absent_is_none() {
        let with_ids = QueryResumeMetadata {
            multi_target_ids: Some(vec!["a".into(), "b".into()]),
            ..Default::default()
        };
        let json = serde_json::to_string(&with_ids).unwrap();
        let back: QueryResumeMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.multi_target_ids,
            Some(vec!["a".to_string(), "b".to_string()])
        );
        let empty: QueryResumeMetadata = serde_json::from_str("{}").unwrap();
        assert!(empty.multi_target_ids.is_none());
    }

    #[test]
    fn resume_metadata_is_empty_by_default() {
        assert!(QueryResumeMetadata::default().is_empty());
    }

    #[test]
    fn resume_metadata_not_empty_when_set() {
        let r = QueryResumeMetadata {
            override_enabled: Some(true),
            ..Default::default()
        };
        assert!(!r.is_empty());
    }

    #[test]
    fn resume_metadata_not_empty_when_false_override() {
        let r = QueryResumeMetadata {
            override_enabled: Some(false),
            ..Default::default()
        };
        assert!(!r.is_empty());
    }

    #[test]
    fn record_is_empty_by_default() {
        assert!(QueryAISummaryRecord::default().is_empty());
    }

    #[test]
    fn record_not_empty_with_summary() {
        let r = QueryAISummaryRecord {
            short_summary: Some("x".into()),
            ..Default::default()
        };
        assert!(!r.is_empty());
    }

    #[test]
    fn record_not_empty_with_connection() {
        let r = QueryAISummaryRecord {
            connection: Some(QueryConnectionMetadata {
                server: "srv".into(),
                database: None,
            }),
            ..Default::default()
        };
        assert!(!r.is_empty());
    }

    #[test]
    fn record_not_empty_with_non_empty_resume() {
        let r = QueryAISummaryRecord {
            resume: Some(QueryResumeMetadata {
                override_enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!r.is_empty());
    }

    #[test]
    fn metadata_get_set_remove() {
        let mut m = QueryAISummaryMetadata::default();
        m.set(
            "doc1",
            QueryAISummaryRecord {
                short_summary: Some("s".into()),
                ..Default::default()
            },
        );
        assert!(m.get("doc1").is_some());
        assert!(m.get("doc2").is_none());
        m.remove("doc1");
        assert!(m.get("doc1").is_none());
    }

    #[test]
    fn metadata_is_empty_when_all_records_empty() {
        let mut m = QueryAISummaryMetadata::default();
        m.set("doc1", QueryAISummaryRecord::default());
        assert!(m.is_empty());
    }

    #[test]
    fn metadata_is_not_empty_when_any_record_has_content() {
        let mut m = QueryAISummaryMetadata::default();
        m.set(
            "doc1",
            QueryAISummaryRecord {
                short_summary: Some("s".into()),
                ..Default::default()
            },
        );
        assert!(!m.is_empty());
    }

    #[test]
    fn metadata_migrate_moves_record() {
        let mut m = QueryAISummaryMetadata::default();
        m.set(
            "old",
            QueryAISummaryRecord {
                short_summary: Some("s".into()),
                ..Default::default()
            },
        );
        m.migrate("old", "new");
        assert!(m.get("old").is_none());
        assert!(m.get("new").is_some());
    }

    #[test]
    fn metadata_get_or_adopt_moves_a_legacy_name_keyed_record() {
        let record = |s: &str| QueryAISummaryRecord {
            short_summary: Some(s.into()),
            ..Default::default()
        };
        let mut m = QueryAISummaryMetadata::default();
        m.set("Consulta 3", record("legacy"));
        let legacy = vec!["Consulta 3".to_string(), "Query 3".to_string()];
        let found = m.get_or_adopt("sqlly:auto-query:3", &legacy).cloned();
        assert_eq!(
            found.and_then(|r| r.short_summary).as_deref(),
            Some("legacy")
        );
        assert!(m.get("Consulta 3").is_none(), "moved to the stable key");
        assert!(m.get("sqlly:auto-query:3").is_some());

        // The stable key wins over any legacy record.
        m.set("Query 3", record("older"));
        let found = m.get_or_adopt("sqlly:auto-query:3", &legacy).cloned();
        assert_eq!(
            found.and_then(|r| r.short_summary).as_deref(),
            Some("legacy")
        );
        assert!(m.get_or_adopt("sqlly:auto-query:9", &[]).is_none());
    }

    #[test]
    fn metadata_migrate_noop_same_keys() {
        let mut m = QueryAISummaryMetadata::default();
        m.set(
            "key",
            QueryAISummaryRecord {
                short_summary: Some("s".into()),
                ..Default::default()
            },
        );
        m.migrate("key", "key");
        assert!(m.get("key").is_some());
    }

    #[test]
    fn metadata_migrate_destination_wins_when_non_empty() {
        let mut m = QueryAISummaryMetadata::default();
        m.set(
            "old",
            QueryAISummaryRecord {
                short_summary: Some("old".into()),
                ..Default::default()
            },
        );
        m.set(
            "new",
            QueryAISummaryRecord {
                short_summary: Some("new".into()),
                ..Default::default()
            },
        );
        m.migrate("old", "new");
        assert_eq!(m.get("new").unwrap().short_summary.as_deref(), Some("new"));
        assert!(m.get("old").is_none());
    }

    #[test]
    fn metadata_migrate_no_source_is_noop() {
        let mut m = QueryAISummaryMetadata::default();
        m.migrate("nonexistent", "new");
        assert!(m.get("new").is_none());
    }

    #[test]
    fn store_load_missing_file_returns_default() {
        let path = Path::new("/tmp/sqlly-test-nonexistent-12345.json");
        let m = QueryAISummaryMetadataStore::load(path);
        assert!(m.records.is_empty());
    }

    #[test]
    fn store_load_corrupt_json_returns_default() {
        let path = std::env::temp_dir().join("sqlly-test-corrupt.json");
        std::fs::write(&path, b"{not valid json").unwrap();
        let m = QueryAISummaryMetadataStore::load(&path);
        assert!(m.records.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn store_save_load_round_trip() {
        let path = std::env::temp_dir().join("sqlly-test-summary-roundtrip.json");
        let mut m = QueryAISummaryMetadata::default();
        m.set(
            "doc1",
            QueryAISummaryRecord {
                short_summary: Some("short".into()),
                long_summary: Some("long".into()),
                connection: Some(QueryConnectionMetadata {
                    server: "srv".into(),
                    database: Some("db".into()),
                }),
                resume: None,
            },
        );
        QueryAISummaryMetadataStore::save(&m, &path).unwrap();
        let loaded = QueryAISummaryMetadataStore::load(&path);
        assert_eq!(
            loaded.get("doc1").unwrap().short_summary.as_deref(),
            Some("short")
        );
        assert_eq!(
            loaded.get("doc1").unwrap().long_summary.as_deref(),
            Some("long")
        );
        assert_eq!(
            loaded
                .get("doc1")
                .unwrap()
                .connection
                .as_ref()
                .unwrap()
                .server,
            "srv"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn record_serde_round_trip() {
        let record = QueryAISummaryRecord {
            short_summary: Some("s".into()),
            long_summary: Some("l".into()),
            connection: Some(QueryConnectionMetadata {
                server: "srv".into(),
                database: Some("db".into()),
            }),
            resume: Some(QueryResumeMetadata {
                override_enabled: Some(true),
                result_layout: Some("stacked".into()),
                diff_mode_enabled: None,
                fim_autocomplete_enabled: Some(false),
                suppress_first_run_confirmation: None,
                multi_target_ids: None,
            }),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: QueryAISummaryRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn record_serde_skips_none_fields() {
        let record = QueryAISummaryRecord::default();
        let json = serde_json::to_string(&record).unwrap();
        assert_eq!(json, "{}");
    }

    #[test]
    fn make_record_with_includes_connection_and_resume() {
        let state = QueryAISummaryState::new();
        let conn = QueryConnectionMetadata {
            server: "srv".into(),
            database: Some("db".into()),
        };
        let resume = QueryResumeMetadata {
            override_enabled: Some(true),
            result_layout: Some("stacked".into()),
            diff_mode_enabled: Some(false),
            fim_autocomplete_enabled: None,
            suppress_first_run_confirmation: Some(true),
            multi_target_ids: None,
        };
        let record = state.make_record_with(Some(&conn), Some(&resume));
        assert_eq!(record.connection.as_ref().unwrap().server, "srv");
        assert_eq!(record.resume.as_ref().unwrap().override_enabled, Some(true));
        assert_eq!(
            record.resume.as_ref().unwrap().result_layout.as_deref(),
            Some("stacked")
        );
        assert_eq!(
            record
                .resume
                .as_ref()
                .unwrap()
                .suppress_first_run_confirmation,
            Some(true)
        );
    }

    #[test]
    fn make_record_with_none_produces_empty_connection_resume() {
        let state = QueryAISummaryState::new();
        let record = state.make_record_with(None, None);
        assert!(record.connection.is_none());
        assert!(record.resume.is_none());
    }

    #[test]
    fn record_serde_resume_full_round_trip() {
        let record = QueryAISummaryRecord {
            short_summary: None,
            long_summary: Some("l".into()),
            connection: Some(QueryConnectionMetadata {
                server: "srv".into(),
                database: None,
            }),
            resume: Some(QueryResumeMetadata {
                override_enabled: Some(false),
                result_layout: Some("tabular".into()),
                diff_mode_enabled: Some(true),
                fim_autocomplete_enabled: Some(true),
                suppress_first_run_confirmation: Some(true),
                multi_target_ids: None,
            }),
        };
        let json = serde_json::to_string(&record).unwrap();
        let back: QueryAISummaryRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, back);
    }

    #[test]
    fn metadata_migrate_moves_record_to_new_key() {
        let mut m = QueryAISummaryMetadata::default();
        m.set(
            "Query 1",
            QueryAISummaryRecord {
                short_summary: Some("s".into()),
                long_summary: None,
                connection: Some(QueryConnectionMetadata {
                    server: "srv".into(),
                    database: None,
                }),
                resume: Some(QueryResumeMetadata {
                    override_enabled: Some(true),
                    result_layout: None,
                    diff_mode_enabled: None,
                    fim_autocomplete_enabled: None,
                    suppress_first_run_confirmation: None,
                    multi_target_ids: None,
                }),
            },
        );
        m.migrate("Query 1", "/path/to/file.sql");
        assert!(m.get("Query 1").is_none());
        assert!(m.get("/path/to/file.sql").is_some());
        assert_eq!(
            m.get("/path/to/file.sql").unwrap().short_summary.as_deref(),
            Some("s")
        );
        assert_eq!(
            m.get("/path/to/file.sql")
                .unwrap()
                .resume
                .as_ref()
                .unwrap()
                .override_enabled,
            Some(true)
        );
    }

    #[test]
    fn metadata_migrate_same_keys_is_noop() {
        let mut m = QueryAISummaryMetadata::default();
        m.set("key1", QueryAISummaryRecord::default());
        m.migrate("key1", "key1");
        assert!(m.get("key1").is_some());
    }

    #[test]
    fn metadata_migrate_missing_source_is_noop() {
        let mut m = QueryAISummaryMetadata::default();
        m.migrate("nonexistent", "new_key");
        assert!(m.get("new_key").is_none());
    }

    #[test]
    fn metadata_migrate_with_resume_metadata_preserves_all_fields() {
        let mut m = QueryAISummaryMetadata::default();
        m.set(
            "old_key",
            QueryAISummaryRecord {
                short_summary: Some("s".into()),
                long_summary: Some("l".into()),
                connection: Some(QueryConnectionMetadata {
                    server: "srv".into(),
                    database: Some("db".into()),
                }),
                resume: Some(QueryResumeMetadata {
                    override_enabled: Some(false),
                    result_layout: Some("stacked".into()),
                    diff_mode_enabled: Some(true),
                    fim_autocomplete_enabled: Some(true),
                    suppress_first_run_confirmation: Some(true),
                    multi_target_ids: None,
                }),
            },
        );
        m.migrate("old_key", "new_key");
        let record = m.get("new_key").unwrap();
        assert_eq!(record.short_summary.as_deref(), Some("s"));
        assert_eq!(record.long_summary.as_deref(), Some("l"));
        assert_eq!(record.connection.as_ref().unwrap().server, "srv");
        assert_eq!(
            record.connection.as_ref().unwrap().database.as_deref(),
            Some("db")
        );
        let resume = record.resume.as_ref().unwrap();
        assert_eq!(resume.override_enabled, Some(false));
        assert_eq!(resume.result_layout.as_deref(), Some("stacked"));
        assert_eq!(resume.diff_mode_enabled, Some(true));
        assert_eq!(resume.fim_autocomplete_enabled, Some(true));
        assert_eq!(resume.suppress_first_run_confirmation, Some(true));
    }
}
