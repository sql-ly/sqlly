//! Export redaction and secret detection (spec 12.2 "no credential values
//! in notebook files, run logs, telemetry, prompt bodies, or exports",
//! 12.3 "import/export performs redaction checks and clear sensitivity
//! warnings").
//!
//! [`find_secrets`] locates secret-shaped substrings (connection-string
//! passwords, URL credentials, private keys, cloud access keys and tokens,
//! JWTs, bearer tokens, SQL `PASSWORD = '…'` literals); [`mask_secrets`]
//! replaces them and then runs `sqlly_core::redact_secrets` for JSON
//! `"password": "…"` fields and serialized secret references.
//! [`redact_for_export`] applies that to a copy of the document and its
//! outputs, drops sensitive parameter values and sensitive columns, and
//! says what it removed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::Value;
use sqlly_core::{CellValue, DisplayMessage, ResultColumn};

use super::messages::message;
use crate::notebook::{CellOutput, NotebookDoc};

/// What a masked secret is replaced with.
pub const REDACTED: &str = "<redacted>";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SecretKind {
    ConnectionStringPassword,
    UrlCredentials,
    PrivateKey,
    CloudAccessKey,
    AccessToken,
    Jwt,
    BearerToken,
    SqlSecretLiteral,
    StorageAccountKey,
    /// Found by `sqlly_core::redact_secrets` (JSON secret fields).
    SecretField,
}

/// A secret-shaped substring at `start..end` (byte offsets of the secret
/// value itself, not its key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFinding {
    pub kind: SecretKind,
    pub start: usize,
    pub end: usize,
}

struct Detector {
    kind: SecretKind,
    regex: Regex,
    /// Capture group holding the secret; 0 = the whole match.
    group: usize,
}

static DETECTORS: LazyLock<Vec<Detector>> = LazyLock::new(|| {
    let d = |kind, pattern: &str, group| Detector {
        kind,
        regex: Regex::new(pattern).expect("secret pattern"),
        group,
    };
    vec![
        d(
            SecretKind::PrivateKey,
            r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----(?s:.*?)(?:-----END (?:[A-Z0-9]+ )*PRIVATE KEY-----|\z)",
            0,
        ),
        d(
            SecretKind::UrlCredentials,
            r"(?i)\b[a-z][a-z0-9+.\-]*://[^/\s:@]*:([^@\s/]+)@",
            1,
        ),
        d(
            SecretKind::ConnectionStringPassword,
            r#"(?i)(?:^|[\s;'"{(,])(?:password|pwd|passwd)\s*=\s*("[^"]*"|\{[^}]*\}|[^;\s"'(),]+)"#,
            1,
        ),
        d(
            SecretKind::SqlSecretLiteral,
            r"(?i)\b(?:password|secret|identity_value|key_source)\s*(?:=\s*)?N?'((?:[^']|'')+)'",
            1,
        ),
        d(
            SecretKind::StorageAccountKey,
            r"(?i)\b(?:AccountKey|SharedAccessKey|SharedAccessSignature|sig)\s*=\s*([A-Za-z0-9+/%]{20,}={0,2})",
            1,
        ),
        d(
            SecretKind::CloudAccessKey,
            r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b|\bAIza[0-9A-Za-z_\-]{35}\b",
            0,
        ),
        d(
            SecretKind::AccessToken,
            r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,}|xox[abprs]-[A-Za-z0-9\-]{10,}|(?:sk|rk)_live_[A-Za-z0-9]{16,}|sk-(?:ant-|proj-)?[A-Za-z0-9_\-]{20,})",
            0,
        ),
        d(
            SecretKind::Jwt,
            r"\beyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,}",
            0,
        ),
        d(
            SecretKind::BearerToken,
            r"(?i)\bbearer\s+([A-Za-z0-9._~+/\-]{20,}=*)",
            1,
        ),
    ]
});

/// Secret-shaped substrings in `text`, sorted, overlaps merged.
pub fn find_secrets(text: &str) -> Vec<SecretFinding> {
    let mut found: Vec<SecretFinding> = Vec::new();
    for detector in DETECTORS.iter() {
        for captures in detector.regex.captures_iter(text) {
            if let Some(m) = captures.get(detector.group).filter(|m| !m.is_empty()) {
                found.push(SecretFinding {
                    kind: detector.kind,
                    start: m.start(),
                    end: m.end(),
                });
            }
        }
    }
    found.sort_by_key(|f| (f.start, std::cmp::Reverse(f.end)));
    let mut merged: Vec<SecretFinding> = Vec::with_capacity(found.len());
    for finding in found {
        match merged.last_mut() {
            Some(last) if finding.start < last.end => last.end = last.end.max(finding.end),
            _ => merged.push(finding),
        }
    }
    merged
}

/// `text` with every secret replaced by [`REDACTED`], and the kinds found.
pub fn mask_secrets(text: &str) -> (String, BTreeSet<SecretKind>) {
    let findings = find_secrets(text);
    let mut kinds: BTreeSet<SecretKind> = findings.iter().map(|f| f.kind).collect();
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for finding in &findings {
        out.push_str(&text[last..finding.start]);
        out.push_str(REDACTED);
        last = finding.end;
    }
    out.push_str(&text[last..]);
    let fields = sqlly_core::redact_secrets(&out);
    if fields != out {
        kinds.insert(SecretKind::SecretField);
        out = fields;
    }
    (out, kinds)
}

/// Whether `text` holds anything [`mask_secrets`] would change.
pub fn contains_secret(text: &str) -> bool {
    !mask_secrets(text).1.is_empty()
}

/// JSON object keys whose string values are secrets wherever they appear.
const SECRET_KEYS: &[&str] = &[
    "password",
    "pwd",
    "passwd",
    "secret",
    "client_secret",
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "token",
    "private_key",
    "connection_string",
    "connectionstring",
];

pub(crate) fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase().replace(['-', ' '], "_");
    SECRET_KEYS.contains(&key.as_str()) || SECRET_KEYS.contains(&key.replace('_', "").as_str())
}

/// Mask secrets inside a JSON value in place; returns the kinds found.
fn mask_json(value: &mut Value, kinds: &mut BTreeSet<SecretKind>) {
    match value {
        Value::String(text) => {
            let (masked, found) = mask_secrets(text);
            if !found.is_empty() {
                *text = masked;
                kinds.extend(found);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| mask_json(v, kinds)),
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if is_secret_key(key) && !matches!(v, Value::Null | Value::Object(_)) {
                    if v.as_str() != Some("") {
                        *v = Value::String(REDACTED.into());
                        kinds.insert(SecretKind::SecretField);
                    }
                } else {
                    mask_json(v, kinds);
                }
            }
        }
        _ => {}
    }
}

fn mask_extra(extra: &mut BTreeMap<String, Value>, kinds: &mut BTreeSet<SecretKind>) {
    let mut wrapped = Value::Object(std::mem::take(extra).into_iter().collect());
    mask_json(&mut wrapped, kinds);
    if let Value::Object(map) = wrapped {
        *extra = map.into_iter().collect();
    }
}

fn mask_string(text: &mut String, kinds: &mut BTreeSet<SecretKind>) {
    let (masked, found) = mask_secrets(text);
    if !found.is_empty() {
        *text = masked;
        kinds.extend(found);
    }
}

/// Column names treated as sensitive without being classified.
const SENSITIVE_COLUMN_WORDS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "private_key",
    "ssn",
    "social_security",
    "credit_card",
    "card_number",
    "cvv",
    "iban",
    "pin_code",
];

pub(crate) fn looks_sensitive_column(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace(['-', ' '], "_");
    SENSITIVE_COLUMN_WORDS.iter().any(|word| {
        name == *word
            || name.split('_').any(|p| p == *word)
            || name.contains(word) && word.contains('_')
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColumnAction {
    #[default]
    Drop,
    Mask,
}

/// What to strip from an export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPolicy {
    /// Columns classified sensitive (case-insensitive names), across cells.
    pub sensitive_columns: BTreeSet<String>,
    /// Also treat columns named like secrets or identifiers (password,
    /// token, ssn, card number, …) as sensitive.
    pub detect_sensitive_names: bool,
    pub column_action: ColumnAction,
    /// Strip machine-local profile mappings from connection slots.
    pub strip_profile_mappings: bool,
}

impl Default for ExportPolicy {
    fn default() -> Self {
        ExportPolicy {
            sensitive_columns: BTreeSet::new(),
            detect_sensitive_names: true,
            column_action: ColumnAction::Drop,
            strip_profile_mappings: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WarningKind {
    SensitiveParameterRemoved,
    SensitiveColumnRemoved,
    SensitiveColumnMasked,
    SecretInSource,
    SecretInNotebook,
    SecretInConnection,
    SecretInOutput,
    IncompleteOutput,
}

/// One thing the export removed or the reader should know. `subject` is a
/// parameter or column name, never a value.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExportWarning {
    pub kind: WarningKind,
    pub cell_id: Option<String>,
    pub subject: Option<String>,
}

impl ExportWarning {
    pub fn warning_key(&self) -> &'static str {
        match self.kind {
            WarningKind::SensitiveParameterRemoved => {
                "notebook.security.export.sensitive_parameter_removed"
            }
            WarningKind::SensitiveColumnRemoved => {
                "notebook.security.export.sensitive_column_removed"
            }
            WarningKind::SensitiveColumnMasked => {
                "notebook.security.export.sensitive_column_masked"
            }
            WarningKind::SecretInSource => "notebook.security.export.secret_in_source",
            WarningKind::SecretInNotebook => "notebook.security.export.secret_in_notebook",
            WarningKind::SecretInConnection => "notebook.security.export.secret_in_connection",
            WarningKind::SecretInOutput => "notebook.security.export.secret_in_output",
            WarningKind::IncompleteOutput => "notebook.security.export.incomplete_output",
        }
    }

    pub fn message(&self) -> DisplayMessage {
        let subject = self.subject.clone().unwrap_or_default();
        let cell = self.cell_id.clone().unwrap_or_default();
        let (template, args): (&str, Vec<(&str, String)>) = match self.kind {
            WarningKind::SensitiveParameterRemoved => (
                "The value of the sensitive parameter \"{name}\" was left out.",
                vec![("name", subject)],
            ),
            WarningKind::SensitiveColumnRemoved => (
                "The sensitive column \"{name}\" was removed from cell {cell}.",
                vec![("name", subject), ("cell", cell)],
            ),
            WarningKind::SensitiveColumnMasked => (
                "The sensitive column \"{name}\" was masked in cell {cell}.",
                vec![("name", subject), ("cell", cell)],
            ),
            WarningKind::SecretInSource => (
                "Something that looks like a password or key was hidden in cell {cell}.",
                vec![("cell", cell)],
            ),
            WarningKind::SecretInNotebook => (
                "Something that looks like a password or key was hidden in the notebook's title or settings.",
                vec![],
            ),
            WarningKind::SecretInConnection => (
                "Something that looks like a password or key was hidden in the connection \"{name}\".",
                vec![("name", subject)],
            ),
            WarningKind::SecretInOutput => (
                "Values that look like passwords or keys were hidden in column \"{name}\" of cell {cell}.",
                vec![("name", subject), ("cell", cell)],
            ),
            WarningKind::IncompleteOutput => (
                "The result of cell {cell} is incomplete (sampled, truncated or stopped early).",
                vec![("cell", cell)],
            ),
        };
        message(self.warning_key(), template, &args)
    }
}

/// A redacted copy ready for an export renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct RedactedExport {
    pub doc: NotebookDoc,
    pub outputs: BTreeMap<String, CellOutput>,
    pub warnings: Vec<ExportWarning>,
}

impl RedactedExport {
    /// Whether anything sensitive was found (the export authorization's
    /// `sensitive_present`). Incomplete outputs alone are not sensitive.
    pub fn sensitive_present(&self) -> bool {
        self.warnings
            .iter()
            .any(|w| w.kind != WarningKind::IncompleteOutput)
    }
}

fn redact_doc(
    doc: &NotebookDoc,
    policy: &ExportPolicy,
    warnings: &mut BTreeSet<ExportWarning>,
) -> NotebookDoc {
    let mut doc = doc.clone();
    for param in &mut doc.parameters {
        if param.is_redacted() || !param.shareable {
            let had_value = param.default.is_some() || !param.presets.is_empty();
            param.default = None;
            param.presets.clear();
            if had_value {
                warnings.insert(ExportWarning {
                    kind: WarningKind::SensitiveParameterRemoved,
                    cell_id: None,
                    subject: Some(param.name.clone()),
                });
            }
        }
        let mut kinds = BTreeSet::new();
        mask_extra(&mut param.extra, &mut kinds);
        if let Some(default) = param.default.as_mut() {
            mask_json(default, &mut kinds);
        }
        if !kinds.is_empty() {
            warnings.insert(ExportWarning {
                kind: WarningKind::SensitiveParameterRemoved,
                cell_id: None,
                subject: Some(param.name.clone()),
            });
        }
    }
    for slot in &mut doc.connection_refs {
        if policy.strip_profile_mappings {
            slot.profile_id = None;
        }
        let mut kinds = BTreeSet::new();
        mask_string(&mut slot.label, &mut kinds);
        if let Some(database) = slot.database.as_mut() {
            mask_string(database, &mut kinds);
        }
        mask_extra(&mut slot.extra, &mut kinds);
        if !kinds.is_empty() {
            warnings.insert(ExportWarning {
                kind: WarningKind::SecretInConnection,
                cell_id: None,
                subject: Some(slot.id.clone()),
            });
        }
    }
    for cell in &mut doc.cells {
        let mut kinds = BTreeSet::new();
        mask_string(&mut cell.source, &mut kinds);
        mask_json(&mut cell.spec, &mut kinds);
        mask_extra(&mut cell.extra, &mut kinds);
        if !kinds.is_empty() {
            warnings.insert(ExportWarning {
                kind: WarningKind::SecretInSource,
                cell_id: Some(cell.id.clone()),
                subject: None,
            });
        }
    }
    let mut kinds = BTreeSet::new();
    mask_extra(&mut doc.extra, &mut kinds);
    mask_string(&mut doc.title, &mut kinds);
    if !kinds.is_empty() {
        warnings.insert(ExportWarning {
            kind: WarningKind::SecretInNotebook,
            cell_id: None,
            subject: None,
        });
    }
    doc
}

fn redact_output(
    cell_id: &str,
    output: &CellOutput,
    policy: &ExportPolicy,
    warnings: &mut BTreeSet<ExportWarning>,
) -> CellOutput {
    let sensitive: Vec<bool> = output
        .columns
        .iter()
        .map(|c| {
            policy
                .sensitive_columns
                .iter()
                .any(|s| s.eq_ignore_ascii_case(&c.name))
                || (policy.detect_sensitive_names && looks_sensitive_column(&c.name))
        })
        .collect();
    let keep: Vec<bool> = sensitive
        .iter()
        .map(|s| !*s || policy.column_action == ColumnAction::Mask)
        .collect();
    for (column, is_sensitive) in output.columns.iter().zip(&sensitive) {
        if *is_sensitive {
            warnings.insert(ExportWarning {
                kind: match policy.column_action {
                    ColumnAction::Drop => WarningKind::SensitiveColumnRemoved,
                    ColumnAction::Mask => WarningKind::SensitiveColumnMasked,
                },
                cell_id: Some(cell_id.to_string()),
                subject: Some(column.name.clone()),
            });
        }
    }
    let columns: Vec<ResultColumn> = output
        .columns
        .iter()
        .zip(&keep)
        .filter(|(_, k)| **k)
        .map(|(c, _)| c.clone())
        .collect();
    let mut secret_columns = BTreeSet::new();
    let rows: Vec<Vec<CellValue>> = output
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .filter(|(i, _)| keep.get(*i).copied().unwrap_or(true))
                .map(|(i, value)| {
                    if sensitive.get(i).copied().unwrap_or(false) {
                        return match value {
                            CellValue::Null => CellValue::Null,
                            _ => CellValue::String(REDACTED.into()),
                        };
                    }
                    match value {
                        CellValue::String(text) => {
                            let (masked, kinds) = mask_secrets(text);
                            if kinds.is_empty() {
                                value.clone()
                            } else {
                                secret_columns.insert(i);
                                CellValue::String(masked)
                            }
                        }
                        other => other.clone(),
                    }
                })
                .collect()
        })
        .collect();
    for i in secret_columns {
        warnings.insert(ExportWarning {
            kind: WarningKind::SecretInOutput,
            cell_id: Some(cell_id.to_string()),
            subject: output.columns.get(i).map(|c| c.name.clone()),
        });
    }
    if !output.completeness.supports_summaries() {
        warnings.insert(ExportWarning {
            kind: WarningKind::IncompleteOutput,
            cell_id: Some(cell_id.to_string()),
            subject: None,
        });
    }
    CellOutput {
        columns,
        rows: Arc::new(rows),
        ..output.clone()
    }
}

/// A copy of `doc` and `outputs` that is safe to hand to an export
/// renderer, plus what was removed.
pub fn redact_for_export(
    doc: &NotebookDoc,
    outputs: &BTreeMap<String, CellOutput>,
    policy: &ExportPolicy,
) -> RedactedExport {
    let mut warnings = BTreeSet::new();
    let doc = redact_doc(doc, policy, &mut warnings);
    let outputs = outputs
        .iter()
        .map(|(id, output)| (id.clone(), redact_output(id, output, policy, &mut warnings)))
        .collect();
    RedactedExport {
        doc,
        outputs,
        warnings: warnings.into_iter().collect(),
    }
}

/// Whether an export of `doc` and `outputs` would include anything
/// sensitive.
pub fn sensitive_present(
    doc: &NotebookDoc,
    outputs: &BTreeMap<String, CellOutput>,
    policy: &ExportPolicy,
) -> bool {
    redact_for_export(doc, outputs, policy).sensitive_present()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::params::{ParamType, ParameterDef};
    use crate::notebook::{CellKind, Completeness, ConnectionRef, SeededIds};

    pub(crate) const SECRETS: &[(&str, SecretKind)] = &[
        (
            "Server=db;User Id=sa;Password=Hunter2-pass;",
            SecretKind::ConnectionStringPassword,
        ),
        (
            "Data Source=x;pwd={br;ace}",
            SecretKind::ConnectionStringPassword,
        ),
        (
            "postgres://app:s3cr3t-pw@db.internal:5432/sales",
            SecretKind::UrlCredentials,
        ),
        (
            "CREATE LOGIN bob WITH PASSWORD = N'Sup3r''Secret'",
            SecretKind::SqlSecretLiteral,
        ),
        (
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----",
            SecretKind::PrivateKey,
        ),
        (
            concat!("key AKIA", "IOSFODNN7EXAMPLE here"),
            SecretKind::CloudAccessKey,
        ),
        (
            concat!("token ghp", "_0123456789abcdefghijABCDEFGHIJ012345"),
            SecretKind::AccessToken,
        ),
        (
            "sk-ant-api03-abcdefghijklmnopqrstuvwx",
            SecretKind::AccessToken,
        ),
        (
            concat!("xoxb", "-1234567890-abcdefghij"),
            SecretKind::AccessToken,
        ),
        (
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N",
            SecretKind::Jwt,
        ),
        (
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz012345",
            SecretKind::BearerToken,
        ),
        (
            concat!(
                "DefaultEndpointsProtocol",
                "=https;AccountName=a;AccountKey",
                "=bm90LWEtcmVhbC1rZXktYXQtYWxs=="
            ),
            SecretKind::StorageAccountKey,
        ),
    ];

    #[test]
    fn detectors_find_each_secret_kind_and_mask_only_the_value() {
        for (text, kind) in SECRETS {
            let findings = find_secrets(text);
            assert!(
                findings.iter().any(|f| f.kind == *kind),
                "{text}: {findings:?}"
            );
            let (masked, kinds) = mask_secrets(text);
            assert!(kinds.contains(kind), "{text}");
            assert!(masked.contains(REDACTED), "{masked}");
        }
        let (masked, _) = mask_secrets("Server=db;User Id=sa;Password=Hunter2-pass;");
        assert_eq!(masked, "Server=db;User Id=sa;Password=<redacted>;");
        let (masked, _) = mask_secrets("postgres://app:s3cr3t-pw@db.internal:5432/sales");
        assert_eq!(masked, "postgres://app:<redacted>@db.internal:5432/sales");
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        for text in [
            "SELECT password_hash, last_login FROM users WHERE id = 1",
            "https://docs.example.com/guide?page=2",
            "Remember to rotate your password every 90 days.",
            "SELECT * FROM tokens WHERE expires_at < now()",
            "user@example.com",
        ] {
            assert!(
                find_secrets(text).is_empty(),
                "{text}: {:?}",
                find_secrets(text)
            );
            assert_eq!(mask_secrets(text).0, text);
        }
    }

    #[test]
    fn json_secret_fields_are_masked_through_core_redaction() {
        let (masked, kinds) = mask_secrets(r#"{"user":"sa","password":"pw-123"}"#);
        assert!(kinds.contains(&SecretKind::SecretField));
        assert!(!masked.contains("pw-123"));
    }

    fn output(
        columns: &[&str],
        rows: Vec<Vec<CellValue>>,
        completeness: Completeness,
    ) -> CellOutput {
        CellOutput {
            columns: columns
                .iter()
                .map(|n| ResultColumn {
                    name: n.to_string(),
                    data_type: "nvarchar".into(),
                    nullable: true,
                })
                .collect(),
            row_count: rows.len() as u64,
            rows: Arc::new(rows),
            completeness,
            elapsed_ms: 1,
            run_id: "r_1".into(),
            fingerprint: crate::notebook::Fingerprint("f".into()),
            provenance: Default::default(),
            freshness: Default::default(),
        }
    }

    pub(crate) fn leaky_doc() -> NotebookDoc {
        let mut ids = SeededIds(9);
        let mut doc = NotebookDoc::new("Audit", &mut ids);
        doc.cells[0].source = "CREATE LOGIN bob WITH PASSWORD = 'Hunter2-Secret'".into();
        doc.cells[0].spec = serde_json::json!({"webhook": {"token": "tok-live-123"}});
        let i = doc.add_cell(CellKind::Markdown, None, &mut ids);
        doc.cells[i].source = "Connect with postgres://app:pg-pass-77@db/sales".into();
        let mut api = ParameterDef::new("api_key", ParamType::String);
        api.sensitive = true;
        api.default = Some(serde_json::json!("key-value-999"));
        doc.parameters.push(api);
        let mut region = ParameterDef::new("region", ParamType::String);
        region.default = Some(serde_json::json!("emea"));
        doc.parameters.push(region);
        doc.connection_refs.push(ConnectionRef {
            id: "db".into(),
            profile_id: Some("p-local".into()),
            label: "Server=prod;Password=conn-pass-55".into(),
            database: Some("sales".into()),
            extra: [("password".to_string(), serde_json::json!("extra-pass-66"))].into(),
        });
        doc
    }

    #[test]
    fn export_drops_sensitive_parameters_secrets_and_mappings() {
        let doc = leaky_doc();
        let redacted = redact_for_export(&doc, &BTreeMap::new(), &ExportPolicy::default());
        let text = redacted.doc.to_canonical_json();
        for secret in [
            "Hunter2-Secret",
            "tok-live-123",
            "pg-pass-77",
            "key-value-999",
            "conn-pass-55",
            "extra-pass-66",
            "p-local",
        ] {
            assert!(!text.contains(secret), "{secret} leaked");
        }
        assert!(text.contains("emea"), "ordinary defaults stay");
        let kinds: BTreeSet<WarningKind> = redacted.warnings.iter().map(|w| w.kind).collect();
        assert!(kinds.contains(&WarningKind::SensitiveParameterRemoved));
        assert!(kinds.contains(&WarningKind::SecretInSource));
        assert!(kinds.contains(&WarningKind::SecretInConnection));
        assert!(redacted.sensitive_present());
        for warning in &redacted.warnings {
            let m = warning.message();
            assert!(!m.fallback().contains("Hunter2"));
            assert!(!m.fallback().contains('{'), "{}", m.fallback());
        }
        // The original is untouched.
        assert!(doc.to_canonical_json().contains("Hunter2-Secret"));
    }

    #[test]
    fn export_removes_or_masks_sensitive_columns_and_secret_values() {
        let mut ids = SeededIds(2);
        let doc = NotebookDoc::new("x", &mut ids);
        let rows = vec![
            vec![
                CellValue::I64(1),
                CellValue::String("alice".into()),
                CellValue::String("pw-a".into()),
                CellValue::String("Password=leak-1;".into()),
                CellValue::String("123-45-6789".into()),
            ],
            vec![
                CellValue::I64(2),
                CellValue::String("bob".into()),
                CellValue::Null,
                CellValue::String("fine".into()),
                CellValue::String("987-65-4321".into()),
            ],
        ];
        let out = output(
            &["id", "name", "user_password", "notes", "tax_id"],
            rows,
            Completeness::Sampled { n: 2 },
        );
        let outputs: BTreeMap<String, CellOutput> = [("c1".to_string(), out)].into();
        let mut policy = ExportPolicy::default();
        policy.sensitive_columns.insert("TAX_ID".into());
        let dropped = redact_for_export(&doc, &outputs, &policy);
        let o = &dropped.outputs["c1"];
        let names: Vec<&str> = o.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "name", "notes"]);
        assert_eq!(o.rows[0].len(), 3);
        assert_eq!(
            o.rows[0][2],
            CellValue::String("Password=<redacted>;".into())
        );
        let kinds: Vec<(WarningKind, Option<String>)> = dropped
            .warnings
            .iter()
            .map(|w| (w.kind, w.subject.clone()))
            .collect();
        assert!(kinds.contains(&(
            WarningKind::SensitiveColumnRemoved,
            Some("user_password".into())
        )));
        assert!(kinds.contains(&(WarningKind::SensitiveColumnRemoved, Some("tax_id".into()))));
        assert!(kinds.contains(&(WarningKind::SecretInOutput, Some("notes".into()))));
        assert!(kinds.contains(&(WarningKind::IncompleteOutput, None)));

        policy.column_action = ColumnAction::Mask;
        let masked = redact_for_export(&doc, &outputs, &policy);
        let o = &masked.outputs["c1"];
        assert_eq!(o.columns.len(), 5);
        assert_eq!(o.rows[0][2], CellValue::String(REDACTED.into()));
        assert_eq!(o.rows[1][2], CellValue::Null);
        assert_eq!(o.rows[1][4], CellValue::String(REDACTED.into()));
    }

    #[test]
    fn clean_exports_report_nothing_sensitive() {
        let mut ids = SeededIds(3);
        let mut doc = NotebookDoc::new("Clean", &mut ids);
        doc.cells[0].source = "SELECT region, SUM(total) FROM orders GROUP BY region".into();
        let out = output(
            &["region", "total"],
            vec![vec![CellValue::String("emea".into()), CellValue::I64(3)]],
            Completeness::Complete,
        );
        let outputs: BTreeMap<String, CellOutput> = [("c".to_string(), out)].into();
        assert!(!sensitive_present(&doc, &outputs, &ExportPolicy::default()));
    }

    #[test]
    fn sensitive_column_names_are_recognized() {
        for name in [
            "password",
            "user_password",
            "API_KEY",
            "ssn",
            "credit_card_number",
            "auth-token",
        ] {
            assert!(looks_sensitive_column(name), "{name}");
        }
        for name in ["id", "name", "tokenizer_version", "passage", "total"] {
            assert!(!looks_sensitive_column(name), "{name}");
        }
    }
}
