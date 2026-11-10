use crate::StudioError;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerformanceBudget {
    pub startup_ms: u64,
    pub object_expand_ms: u64,
    pub first_result_row_ms: u64,
    pub max_buffered_rows: usize,
    pub backpressure_batch_size: usize,
}

impl Default for PerformanceBudget {
    fn default() -> Self {
        Self {
            startup_ms: 1_500,
            object_expand_ms: 750,
            first_result_row_ms: 1_000,
            max_buffered_rows: 10_000,
            backpressure_batch_size: 500,
        }
    }
}

impl PerformanceBudget {
    pub fn should_apply_backpressure(&self, buffered_rows: usize) -> bool {
        buffered_rows >= self.max_buffered_rows
    }

    pub fn next_batch_size(&self, buffered_rows: usize) -> usize {
        if self.should_apply_backpressure(buffered_rows) {
            self.backpressure_batch_size.max(1) / 2
        } else {
            self.backpressure_batch_size.max(1)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemediationHint {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl RemediationHint {
    pub fn display_message(&self) -> crate::DisplayMessage {
        use crate::DisplayMessage;
        match self.code {
            "connection" if self.message.starts_with("Check network") => DisplayMessage::new(
                "services.remediation.connection_timeout",
                self.message.clone(),
            ),
            "connection" => {
                DisplayMessage::new("services.remediation.connection", self.message.clone())
            }
            "authentication" => {
                DisplayMessage::new("services.remediation.authentication", self.message.clone())
            }
            "query" => DisplayMessage::new("services.remediation.query", self.message.clone())
                .with_arg(
                    "detail",
                    self.message
                        .strip_prefix("Review the SQL batch and server message: ")
                        .unwrap_or(&self.message),
                ),
            _ => DisplayMessage::new("services.remediation.general", self.message.clone()),
        }
    }
}

pub fn remediation_for_error(error: &StudioError) -> RemediationHint {
    match error {
        // Keyed `*Message` variants carry the same kind as their string
        // twins; the hint reads their English fallback (`detail()`), which is
        // what the string variants carry.
        StudioError::Connection(_) | StudioError::ConnectionMessage(_) => RemediationHint {
            code: "connection",
            message: if error.detail().to_lowercase().contains("timeout") {
                "Check network connectivity, firewall rules, and increase the connection timeout if needed."
                    .to_string()
            } else {
                "Verify server name, port, TLS settings, and network reachability.".to_string()
            },
            retryable: true,
        },
        StudioError::Authentication(_) | StudioError::AuthenticationMessage(_) => RemediationHint {
            code: "authentication",
            message:
                "Verify credentials, Entra tenant, MFA requirements, and database user mapping."
                    .to_string(),
            retryable: false,
        },
        StudioError::QueryExecution(_) | StudioError::QueryExecutionMessage(_) => RemediationHint {
            code: "query",
            message: format!(
                "Review the SQL batch and server message: {}",
                error.detail()
            ),
            retryable: false,
        },
        _ => RemediationHint {
            code: "general",
            message: "Review the operation details and retry if the server state changed."
                .to_string(),
            retryable: false,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconnectPolicy {
    pub max_attempts: u8,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay_ms: 250,
            max_delay_ms: 2_000,
        }
    }
}

impl ReconnectPolicy {
    pub fn delay_for_attempt(&self, attempt: u8) -> Option<Duration> {
        if attempt == 0 || attempt > self.max_attempts {
            return None;
        }
        let multiplier = 1_u64 << u32::from(attempt.saturating_sub(1));
        Some(Duration::from_millis(
            (self.initial_delay_ms * multiplier).min(self.max_delay_ms),
        ))
    }
}

/// Observed health of a live connection. Drives whether the UI shows a green
/// "connected" indicator, a transient "recovering" spinner, or a red "lost"
/// banner, and whether the session may auto-reconnect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionHealth {
    /// Never connected (or explicitly disconnected by the user).
    Disconnected,
    /// Connected and the last keepalive/probe succeeded.
    Healthy,
    /// A probe or operation failed; a reconnect is being attempted.
    Degraded,
    /// Reconnect attempts have been exhausted; manual action required.
    Lost,
}

/// What a session should do after a connection probe/operation failure, given
/// the current health, attempt count, and whether a transaction is open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconnectDecision {
    /// Reconnect after waiting `delay`, recording this as `attempt` (1-based).
    Retry { attempt: u8, delay: Duration },
    /// Do not auto-reconnect; surface `reason` to the user and require a manual
    /// reconnect. Used when attempts are exhausted or it is unsafe to retry.
    GiveUp { reason: String },
}

/// Tracks connection health and decides — using a [`ReconnectPolicy`] — whether
/// and when it is *safe* to auto-reconnect. The key safety rule: never silently
/// auto-reconnect while a transaction is open, because reconnecting starts a
/// fresh session and would silently lose the in-flight transaction's work; the
/// user must be told so they can decide.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionHealthMonitor {
    pub health: ConnectionHealth,
    pub policy: ReconnectPolicy,
    /// Failed reconnect attempts since the last healthy state.
    pub attempts: u8,
    /// True while a transaction is open on the session (set by the query layer).
    pub transaction_open: bool,
}

impl ConnectionHealthMonitor {
    pub fn new(policy: ReconnectPolicy) -> Self {
        Self {
            health: ConnectionHealth::Disconnected,
            policy,
            attempts: 0,
            transaction_open: false,
        }
    }

    /// Marks the connection healthy after a successful connect/probe, clearing
    /// the failed-attempt counter.
    pub fn mark_connected(&mut self) {
        self.health = ConnectionHealth::Healthy;
        self.attempts = 0;
    }

    /// Records that the user (or app) intentionally closed the connection.
    pub fn mark_disconnected(&mut self) {
        self.health = ConnectionHealth::Disconnected;
        self.attempts = 0;
    }

    /// Records the transaction state so reconnect safety can account for it.
    pub fn set_transaction_open(&mut self, open: bool) {
        self.transaction_open = open;
    }

    /// Reports a probe/operation failure and returns the recommended action.
    /// Refuses to auto-reconnect mid-transaction (data-loss hazard) and gives
    /// up — moving to [`ConnectionHealth::Lost`] — once attempts are exhausted.
    pub fn on_failure(&mut self) -> ReconnectDecision {
        if self.transaction_open {
            self.health = ConnectionHealth::Lost;
            return ReconnectDecision::GiveUp {
                reason: "Connection lost while a transaction was open; reconnecting would discard \
                         the in-flight transaction. Reconnect manually and re-run the work."
                    .to_string(),
            };
        }
        let next_attempt = self.attempts.saturating_add(1);
        match self.policy.delay_for_attempt(next_attempt) {
            Some(delay) => {
                self.attempts = next_attempt;
                self.health = ConnectionHealth::Degraded;
                ReconnectDecision::Retry {
                    attempt: next_attempt,
                    delay,
                }
            }
            None => {
                self.health = ConnectionHealth::Lost;
                ReconnectDecision::GiveUp {
                    reason: format!(
                        "Reconnect failed after {} attempt(s); check connectivity and reconnect \
                         manually.",
                        self.policy.max_attempts
                    ),
                }
            }
        }
    }

    /// Whether the session currently presents as usable for new work.
    pub fn is_usable(&self) -> bool {
        matches!(self.health, ConnectionHealth::Healthy)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeoutPolicy {
    pub connect_seconds: u64,
    pub query_seconds: u64,
    pub metadata_seconds: u64,
    pub cancellation_grace_ms: u64,
}

impl Default for TimeoutPolicy {
    fn default() -> Self {
        Self {
            connect_seconds: 30,
            query_seconds: 0,
            metadata_seconds: 15,
            cancellation_grace_ms: 1_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacySettings {
    pub query_history_enabled: bool,
    pub telemetry_enabled: bool,
    pub telemetry_collects_sql_text: bool,
}

impl PrivacySettings {
    pub fn clear_query_history(&mut self) {
        self.query_history_enabled = false;
    }
}

/// Keys whose values are secrets wherever they appear in diagnostic text —
/// JSON bodies (`"password":"…"`) and connection-string pairs
/// (`Password=…;`) alike. Matching is case-insensitive; JSON keys match
/// exactly (so `"secret_ref"` is handled structurally by
/// [`redact_secret_ref_objects`], not by the `secret` entry).
const SECRET_VALUE_KEYS: [&str; 6] = [
    "password",
    "pwd",
    "access_token",
    "client_secret",
    "authorization",
    "secret",
];

/// The marker substituted for every secret value.
const REDACTED: &str = "<redacted>";

/// Replace every credential value in `input` with [`REDACTED`], leaving all
/// non-secret content (ids, SQL text, servers, usernames, shape fields) intact
/// so the result keeps its diagnostic value. Handles JSON string values for
/// each of [`SECRET_VALUE_KEYS`], `key=value`/`key: value` connection-string
/// pairs, and the `key` field of serialized `SecretRef` objects (whose
/// `InMemory` form carries the raw secret in `key`).
pub fn redact_secrets(input: &str) -> String {
    let mut output = redact_secret_ref_objects(input);
    for key in SECRET_VALUE_KEYS {
        output = redact_json_string_values(&output, key);
        output = redact_kv_pair_values(&output, key);
    }
    output
}

/// Case-insensitive search for `needle` in `haystack` starting at `from`.
/// `lower` must be `haystack.to_ascii_lowercase()` (byte offsets are identical
/// because ASCII case folding is byte-wise); `needle` must already be
/// lowercase.
fn find_ci(lower: &str, needle: &str, from: usize) -> Option<usize> {
    if from > lower.len() {
        return None;
    }
    lower[from..].find(needle).map(|i| from + i)
}

/// Given `input` and the byte offset of an opening `"` of a JSON string,
/// return the offset of its closing `"`, honoring backslash escapes. `None`
/// when the string never closes (truncated/lossy text).
fn json_string_end(input: &str, open: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut i = open + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Redact every JSON occurrence of `"key"` (exact, case-insensitive) whose
/// value is a string: `"key" : "value"` becomes `"key" : "<redacted>"`.
/// Empty string values stay empty — "no password was supplied" is itself
/// diagnostic information and leaks nothing.
fn redact_json_string_values(input: &str, key: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let quoted = format!("\"{}\"", key.to_ascii_lowercase());
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(at) = find_ci(&lower, &quoted, cursor) {
        let after_key = at + quoted.len();
        let rest = &input[after_key..];
        let sep = rest.len() - rest.trim_start().len();
        let after_ws = after_key + sep;
        if !input[after_ws..].starts_with(':') {
            out.push_str(&input[cursor..after_key]);
            cursor = after_key;
            continue;
        }
        let after_colon = after_ws + 1;
        let rest = &input[after_colon..];
        let pad = rest.len() - rest.trim_start().len();
        let value_at = after_colon + pad;
        if !input[value_at..].starts_with('"') {
            out.push_str(&input[cursor..after_key]);
            cursor = after_key;
            continue;
        }
        let Some(close) = json_string_end(input, value_at) else {
            // Truncated string (lossy/cut-off text): redact through the end.
            out.push_str(&input[cursor..=value_at]);
            out.push_str(REDACTED);
            return out;
        };
        out.push_str(&input[cursor..=value_at]);
        if close > value_at + 1 {
            out.push_str(REDACTED);
        }
        out.push('"');
        cursor = close + 1;
    }
    out.push_str(&input[cursor..]);
    out
}

/// Redact `key=value` / `key: value` pairs outside JSON (connection strings,
/// ad-hoc kv text). The key must start at a word boundary; the value runs to
/// the next `;`, `,`, whitespace, or end of input.
fn redact_kv_pair_values(input: &str, key: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let key_lower = key.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(at) = find_ci(&lower, &key_lower, cursor) {
        let boundary_ok = at == 0
            || !input[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        let after_key = at + key.len();
        let rest = &input[after_key..];
        let pad = rest.len() - rest.trim_start().len();
        let sep_at = after_key + pad;
        let sep_ok = matches!(input[sep_at..].chars().next(), Some('=' | ':'));
        // `"key":` was already handled (and re-quoted) by the JSON pass; skip
        // anything directly preceded by a double quote.
        let json_shaped = at > 0 && input.as_bytes()[at - 1] == b'"';
        if !boundary_ok || !sep_ok || json_shaped {
            out.push_str(&input[cursor..after_key]);
            cursor = after_key;
            continue;
        }
        let value_at = sep_at + 1;
        let rest = &input[value_at..];
        let pad = rest.len() - rest.trim_start().len();
        let value_start = value_at + pad;
        let mut value_end = input[value_start..]
            .find([';', ',', ' ', '\t'])
            .map(|i| value_start + i)
            .unwrap_or(input.len());
        // `Authorization: Bearer <token>` — the scheme word is not the
        // secret; the token after it is. Extend through the next word so the
        // token itself is what gets scrubbed.
        let first_word = &lower[value_start..value_end];
        if matches!(first_word, "bearer" | "basic") {
            let rest = &input[value_end..];
            let pad = rest.len() - rest.trim_start().len();
            let token_start = value_end + pad;
            value_end = input[token_start..]
                .find([';', ',', ' ', '\t'])
                .map(|i| token_start + i)
                .unwrap_or(input.len());
        }
        out.push_str(&input[cursor..value_start]);
        if value_end > value_start {
            out.push_str(REDACTED);
        }
        cursor = value_end;
    }
    out.push_str(&input[cursor..]);
    out
}

/// Redact the `key` field of serialized `SecretRef` objects:
/// `"secret_ref":{"provider":"InMemory","key":"…"}`. The `InMemory` provider
/// carries the raw secret in `key` (hydrated profiles), so the whole object's
/// `key` value is scrubbed regardless of provider.
fn redact_secret_ref_objects(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let marker = "\"secret_ref\"";
    let mut out = String::with_capacity(input.len());
    let mut cursor = 0;
    while let Some(at) = find_ci(&lower, marker, cursor) {
        let after_key = at + marker.len();
        let rest = &input[after_key..];
        let pad = rest.len() - rest.trim_start().len();
        let after_ws = after_key + pad;
        if !input[after_ws..].starts_with(':') {
            out.push_str(&input[cursor..after_key]);
            cursor = after_key;
            continue;
        }
        let rest = &input[after_ws + 1..];
        let pad = rest.len() - rest.trim_start().len();
        let brace_at = after_ws + 1 + pad;
        if !input[brace_at..].starts_with('{') {
            out.push_str(&input[cursor..after_key]);
            cursor = after_key;
            continue;
        }
        let close = balanced_brace_end(input, brace_at).unwrap_or(input.len());
        let object = &input[brace_at..close];
        out.push_str(&input[cursor..brace_at]);
        out.push_str(&redact_json_string_values(object, "key"));
        cursor = close;
    }
    out.push_str(&input[cursor..]);
    out
}

/// Offset one past the `}` matching the `{` at `open`, skipping braces inside
/// JSON strings. `None` when unbalanced.
fn balanced_brace_end(input: &str, open: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut depth = 0usize;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b'"' => {
                i = json_string_end(input, i)?;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

pub fn threat_model_secret_flow() -> &'static str {
    "Secrets are referenced by key, loaded from platform stores at execution time, redacted from logs, and never persisted in query history or telemetry."
}

/// A virtualization window over a (possibly large) result set. The result grid
/// holds only `page_size` rows in memory at once and asks the core to compute
/// which absolute row range (`offset`..`offset+len`) backs a given page, so an
/// arbitrarily large result never has to be materialized in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultWindow {
    /// Number of rows fetched/displayed per page (clamped to >= 1).
    pub page_size: usize,
    /// Total rows known to be available (e.g. from `@@ROWCOUNT` or a count).
    pub total_rows: usize,
}

/// The concrete slice a [`ResultWindow`] resolves a page request to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowSlice {
    /// Zero-based page index this slice represents.
    pub page: usize,
    /// Absolute index of the first row in the slice.
    pub offset: usize,
    /// Number of rows in the slice (may be smaller than `page_size` on the
    /// final page).
    pub len: usize,
}

impl ResultWindow {
    pub fn new(page_size: usize, total_rows: usize) -> Self {
        Self {
            page_size: page_size.max(1),
            total_rows,
        }
    }

    /// Total number of pages needed to cover `total_rows`. An empty result still
    /// has a single (empty) page so the grid always has something to render.
    pub fn page_count(&self) -> usize {
        if self.total_rows == 0 {
            return 1;
        }
        self.total_rows.div_ceil(self.page_size.max(1))
    }

    /// Resolves a (possibly out-of-range) page request into the concrete row
    /// slice to fetch. The page is clamped into `0..page_count` and the final
    /// page is truncated so `offset + len <= total_rows`.
    pub fn slice_for_page(&self, page: usize) -> WindowSlice {
        let size = self.page_size.max(1);
        let last_page = self.page_count().saturating_sub(1);
        let page = page.min(last_page);
        let offset = page.saturating_mul(size);
        let len = if offset >= self.total_rows {
            0
        } else {
            size.min(self.total_rows - offset)
        };
        WindowSlice { page, offset, len }
    }

    /// The page index containing an absolute row, clamped to the valid range.
    pub fn page_of_row(&self, row: usize) -> usize {
        let page = row / self.page_size.max(1);
        page.min(self.page_count().saturating_sub(1))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub cancel_active_queries: bool,
    pub close_idle_connections: bool,
    pub recover_unsaved_tabs: bool,
}

impl Default for CleanupPlan {
    fn default() -> Self {
        Self {
            cancel_active_queries: true,
            close_idle_connections: true,
            recover_unsaved_tabs: true,
        }
    }
}

/// User-editable application preferences. This is the serializable model the
/// macOS Preferences panel binds to; it is persisted as JSON and composes the
/// existing privacy/performance policies so a single document captures the
/// full configuration surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppPreferences {
    /// Editor font point size (clamped on validation).
    pub editor_font_size: u8,
    /// Insert spaces instead of a tab character.
    pub editor_soft_tabs: bool,
    /// Tab width in columns.
    pub editor_tab_width: u8,
    /// Default rows fetched per result page (drives [`ResultWindow`]).
    pub result_page_size: usize,
    /// Seconds between crash-recovery autosaves (0 disables).
    pub autosave_interval_seconds: u64,
    /// Privacy/telemetry choices.
    pub privacy: PrivacySettings,
    /// Reliability/reconnect behaviour.
    pub reconnect: ReconnectPolicy,
    /// Operation timeouts.
    pub timeouts: TimeoutPolicy,
}

impl Default for AppPreferences {
    fn default() -> Self {
        Self {
            editor_font_size: 13,
            editor_soft_tabs: true,
            editor_tab_width: 4,
            result_page_size: 1_000,
            autosave_interval_seconds: 30,
            privacy: PrivacySettings::default(),
            reconnect: ReconnectPolicy::default(),
            timeouts: TimeoutPolicy::default(),
        }
    }
}

impl AppPreferences {
    /// Clamps every numeric field into a sane range, returning the corrected
    /// preferences. Called before persisting or applying so a hand-edited or
    /// stale settings file can never put the app into an unusable state.
    pub fn normalized(mut self) -> Self {
        self.editor_font_size = self.editor_font_size.clamp(8, 48);
        self.editor_tab_width = self.editor_tab_width.clamp(1, 16);
        self.result_page_size = self.result_page_size.clamp(1, 100_000);
        self
    }

    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| e.to_string())
    }

    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str::<Self>(json)
            .map(Self::normalized)
            .map_err(|e| e.to_string())
    }

    /// A result window sized from these preferences for a given total row count.
    pub fn result_window(&self, total_rows: usize) -> ResultWindow {
        ResultWindow::new(self.result_page_size, total_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn performance_budget_limits_buffers_and_backpressure() {
        let budget = PerformanceBudget::default();
        assert!(!budget.should_apply_backpressure(999));
        assert!(budget.should_apply_backpressure(budget.max_buffered_rows));
        assert!(budget.next_batch_size(budget.max_buffered_rows) < budget.backpressure_batch_size);
    }

    #[test]
    fn reliability_policies_provide_remediation_and_reconnect_backoff() {
        let hint = remediation_for_error(&StudioError::Connection("timeout".to_string()));
        assert_eq!(hint.code, "connection");
        assert!(hint.retryable);

        let policy = ReconnectPolicy::default();
        assert_eq!(
            policy.delay_for_attempt(1),
            Some(Duration::from_millis(250))
        );
        assert_eq!(policy.delay_for_attempt(4), None);
    }

    #[test]
    fn keyed_message_errors_get_the_same_remediation_as_their_string_twins() {
        use crate::DisplayMessage;

        let timeout = remediation_for_error(&StudioError::ConnectionMessage(DisplayMessage::raw(
            "Connection error: login timeout expired",
        )));
        assert_eq!(timeout.code, "connection");
        assert!(timeout.retryable);
        assert!(timeout.message.contains("increase the connection timeout"));

        let refused = remediation_for_error(&StudioError::ConnectionMessage(DisplayMessage::raw(
            "Connection error: refused",
        )));
        assert!(refused.message.starts_with("Verify server name"));

        let auth = remediation_for_error(&StudioError::AuthenticationMessage(DisplayMessage::raw(
            "Authentication failed: bad password",
        )));
        assert_eq!(auth.code, "authentication");
        assert!(!auth.retryable);

        let query = remediation_for_error(&StudioError::QueryExecutionMessage(
            DisplayMessage::raw("Query execution error: spid probe returned no row"),
        ));
        assert_eq!(query.code, "query");
        assert_eq!(
            query.message,
            "Review the SQL batch and server message: spid probe returned no row"
        );
        assert_eq!(
            query.message,
            remediation_for_error(&StudioError::QueryExecution(
                "spid probe returned no row".into()
            ))
            .message
        );
    }

    #[test]
    fn security_defaults_are_privacy_first_and_redact_secrets() {
        let settings = PrivacySettings::default();
        assert!(!settings.query_history_enabled);
        assert!(!settings.telemetry_enabled);
        assert!(!settings.telemetry_collects_sql_text);

        let redacted = redact_secrets("server=x;password=hunter2;access_token:abc");
        assert!(!redacted.contains("hunter2"));
        assert!(!redacted.contains("abc"));
        assert!(threat_model_secret_flow().contains("redacted"));
    }

    #[test]
    fn redact_secrets_scrubs_every_json_occurrence_and_keeps_shape() {
        let body = r#"{"id":"q-1","type":"execute_query","connection":{"server":"db.example.com","username":"sa","password":"hu\"nter2","access_token":"tok-123","encrypt":true},"sql":"SELECT 1","other":{"password":"second"}}"#;
        let redacted = redact_secrets(body);
        assert!(!redacted.contains("hunter2"), "{redacted}");
        assert!(!redacted.contains("tok-123"), "{redacted}");
        assert!(!redacted.contains("second"), "{redacted}");
        // Non-secret fields keep their diagnostic value.
        assert!(redacted.contains(r#""id":"q-1""#));
        assert!(redacted.contains("execute_query"));
        assert!(redacted.contains("db.example.com"));
        assert!(redacted.contains(r#""username":"sa""#));
        assert!(redacted.contains("SELECT 1"));
        assert!(redacted.contains(r#""password":"<redacted>""#));
    }

    #[test]
    fn redact_secrets_scrubs_in_memory_secret_ref_keys() {
        let body =
            r#"{"name":"prod","secret_ref":{"provider":"InMemory","key":"raw-pass"},"server":"s"}"#;
        let redacted = redact_secrets(body);
        assert!(!redacted.contains("raw-pass"), "{redacted}");
        assert!(redacted.contains("InMemory"));
        assert!(redacted.contains(r#""server":"s""#));
    }

    #[test]
    fn redact_secrets_keeps_empty_and_non_string_values() {
        let body = r#"{"password":"","access_token":null,"count":3}"#;
        let redacted = redact_secrets(body);
        assert_eq!(body, redacted);
    }

    #[test]
    fn redact_secrets_handles_bearer_and_mixed_case_kv() {
        let text = "Authorization: Bearer abc.def.ghi PWD=s3cret;Server=ok";
        let redacted = redact_secrets(text);
        assert!(!redacted.contains("abc.def.ghi"), "{redacted}");
        assert!(!redacted.contains("s3cret"), "{redacted}");
        assert!(redacted.contains("Server=ok"));
    }

    #[test]
    fn redact_secrets_is_idempotent_and_safe_on_truncated_text() {
        let once = redact_secrets(r#"{"password":"a"}"#);
        assert_eq!(once, redact_secrets(&once));
        // A frame cut off mid-secret must still scrub what is present.
        let truncated = redact_secrets(r#"{"sql":"SELECT 1","password":"hunt"#);
        assert!(!truncated.contains("hunt"), "{truncated}");
        assert!(truncated.contains("SELECT 1"));
    }

    #[test]
    fn cleanup_and_timeout_defaults_cover_session_shutdown() {
        let cleanup = CleanupPlan::default();
        assert!(cleanup.cancel_active_queries);
        assert!(cleanup.recover_unsaved_tabs);

        let timeout = TimeoutPolicy::default();
        assert_eq!(timeout.connect_seconds, 30);
        assert_eq!(timeout.metadata_seconds, 15);
    }

    #[test]
    fn result_window_paginates_and_truncates_final_page() {
        let window = ResultWindow::new(100, 250);
        assert_eq!(window.page_count(), 3);

        let first = window.slice_for_page(0);
        assert_eq!((first.offset, first.len), (0, 100));

        // Final page is truncated to the remaining 50 rows.
        let last = window.slice_for_page(2);
        assert_eq!((last.offset, last.len), (200, 50));

        // Out-of-range page clamps to the last page.
        let beyond = window.slice_for_page(99);
        assert_eq!(beyond.page, 2);
        assert_eq!(beyond.offset, 200);

        assert_eq!(window.page_of_row(150), 1);
    }

    #[test]
    fn result_window_handles_empty_and_zero_page_size() {
        let empty = ResultWindow::new(50, 0);
        assert_eq!(empty.page_count(), 1);
        let slice = empty.slice_for_page(0);
        assert_eq!((slice.offset, slice.len), (0, 0));

        // page_size of 0 is clamped to 1, never divides by zero.
        let degenerate = ResultWindow::new(0, 3);
        assert_eq!(degenerate.page_size, 1);
        assert_eq!(degenerate.page_count(), 3);
    }

    #[test]
    fn app_preferences_round_trip_and_normalize() {
        let prefs = AppPreferences::default();
        assert_eq!(prefs.result_page_size, 1_000);
        let json = prefs.to_json().unwrap();
        let restored = AppPreferences::from_json(&json).unwrap();
        assert_eq!(prefs, restored);

        // A partial / out-of-range file is filled with defaults and clamped.
        let restored =
            AppPreferences::from_json(r#"{"editor_font_size":200,"result_page_size":0}"#).unwrap();
        assert_eq!(restored.editor_font_size, 48);
        assert_eq!(restored.result_page_size, 1);
        // Missing fields fall back to defaults.
        assert_eq!(restored.editor_tab_width, 4);
        assert!(!restored.privacy.telemetry_enabled);

        let window = AppPreferences::default().result_window(2_500);
        assert_eq!(window.page_count(), 3);
    }

    #[test]
    fn health_monitor_retries_with_backoff_then_gives_up() {
        let mut monitor = ConnectionHealthMonitor::new(ReconnectPolicy::default());
        monitor.mark_connected();
        assert!(monitor.is_usable());
        assert_eq!(monitor.health, ConnectionHealth::Healthy);

        // First failure -> retry attempt 1 with the initial backoff.
        let d1 = monitor.on_failure();
        assert_eq!(
            d1,
            ReconnectDecision::Retry {
                attempt: 1,
                delay: Duration::from_millis(250)
            }
        );
        assert_eq!(monitor.health, ConnectionHealth::Degraded);
        assert!(!monitor.is_usable());

        // Subsequent failures escalate the attempt count.
        let _ = monitor.on_failure(); // attempt 2
        let _ = monitor.on_failure(); // attempt 3 (max)

        // Exhausted -> give up and mark Lost.
        let d4 = monitor.on_failure();
        assert!(matches!(d4, ReconnectDecision::GiveUp { .. }));
        assert_eq!(monitor.health, ConnectionHealth::Lost);

        // Recovery resets the counter and health.
        monitor.mark_connected();
        assert_eq!(monitor.attempts, 0);
        assert!(monitor.is_usable());
    }

    #[test]
    fn health_monitor_never_auto_reconnects_mid_transaction() {
        let mut monitor = ConnectionHealthMonitor::new(ReconnectPolicy::default());
        monitor.mark_connected();
        monitor.set_transaction_open(true);

        let decision = monitor.on_failure();
        match decision {
            ReconnectDecision::GiveUp { reason } => {
                assert!(reason.to_lowercase().contains("transaction"));
            }
            other => panic!("expected GiveUp mid-transaction, got {other:?}"),
        }
        assert_eq!(monitor.health, ConnectionHealth::Lost);
        // No silent retry attempt was consumed.
        assert_eq!(monitor.attempts, 0);
    }

    #[test]
    fn health_monitor_serializes_health_kebab_case() {
        let json = serde_json::to_string(&ConnectionHealth::Disconnected).unwrap();
        assert_eq!(json, "\"disconnected\"");
    }
}
