//! Malicious-file defenses (spec 12.1 "malicious notebook files", 12.3 "a
//! malicious sqllynb file cannot run SQL or make network calls upon open",
//! 18 "malformed payloads, zip bombs, path traversal, malicious
//! attachments/Markdown").
//!
//! [`scan_notebook_file`] runs on the raw text of a `.sqllynb`,
//! `.sqlnb.json` or `.ipynb` file before it is loaded. It is pure and
//! linear: a byte pass checks size, nesting depth and string lengths
//! before anything is parsed, then one walk over the parsed JSON checks
//! cell, parameter and connection counts, credentials embedded in
//! connection references, attachment paths, active or remote content in
//! Markdown, auto-run requests and embedded outputs. A report with a
//! [`Severity::Reject`] finding must not be opened as is; warnings are
//! shown with the trust banner.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;
use sqlly_core::DisplayMessage;

use super::messages::message;
use super::redact::{contains_secret, is_secret_key};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    pub max_bytes: usize,
    pub max_depth: usize,
    pub max_string_bytes: usize,
    pub max_cells: usize,
    pub max_parameters: usize,
    pub max_connection_refs: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        ScanLimits {
            max_bytes: 32 * 1024 * 1024,
            max_depth: 64,
            max_string_bytes: 4 * 1024 * 1024,
            max_cells: 5_000,
            max_parameters: 500,
            max_connection_refs: 100,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    TooLarge {
        bytes: usize,
        limit: usize,
    },
    TooDeep {
        limit: usize,
    },
    StringTooLong {
        limit: usize,
    },
    NotJson,
    TooManyCells {
        count: usize,
        limit: usize,
    },
    TooManyParameters {
        count: usize,
        limit: usize,
    },
    TooManyConnections {
        count: usize,
        limit: usize,
    },
    /// A password, connection string or URL credential inside a connection
    /// reference.
    EmbeddedCredential,
    /// An attachment path that is absolute, climbs out with `..`, starts
    /// at the home directory or names a URL scheme.
    UnsafePath,
    /// Markdown that would fetch from the network when rendered.
    RemoteResource,
    /// Scripts, frames, `javascript:` or HTML data URLs in Markdown.
    ActiveContent,
    /// The file asks to run cells reactively; ignored until trusted.
    AutoRunRequested,
    /// The file carries result outputs (never shown as live).
    EmbeddedOutputs,
    /// A secret-shaped value outside connection references (cell SQL,
    /// specs, parameters).
    SecretInContent,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub severity: Severity,
    pub kind: FindingKind,
    /// JSON pointer to where it was found (`/cells/3/source`), empty for
    /// whole-file findings.
    pub location: String,
}

impl Finding {
    pub fn finding_key(&self) -> &'static str {
        match self.kind {
            FindingKind::TooLarge { .. } => "notebook.security.scan.too_large",
            FindingKind::TooDeep { .. } => "notebook.security.scan.too_deep",
            FindingKind::StringTooLong { .. } => "notebook.security.scan.string_too_long",
            FindingKind::NotJson => "notebook.security.scan.not_json",
            FindingKind::TooManyCells { .. } => "notebook.security.scan.too_many_cells",
            FindingKind::TooManyParameters { .. } => "notebook.security.scan.too_many_parameters",
            FindingKind::TooManyConnections { .. } => "notebook.security.scan.too_many_connections",
            FindingKind::EmbeddedCredential => "notebook.security.scan.embedded_credential",
            FindingKind::UnsafePath => "notebook.security.scan.unsafe_path",
            FindingKind::RemoteResource => "notebook.security.scan.remote_resource",
            FindingKind::ActiveContent => "notebook.security.scan.active_content",
            FindingKind::AutoRunRequested => "notebook.security.scan.auto_run_requested",
            FindingKind::EmbeddedOutputs => "notebook.security.scan.embedded_outputs",
            FindingKind::SecretInContent => "notebook.security.scan.secret_in_content",
        }
    }

    pub fn message(&self) -> DisplayMessage {
        let key = self.finding_key();
        let limit_args = |count: Option<usize>, limit: usize| {
            let mut args = vec![("limit", limit.to_string())];
            if let Some(count) = count {
                args.push(("count", count.to_string()));
            }
            args
        };
        match &self.kind {
            FindingKind::TooLarge { bytes, limit } => message(
                key,
                "The file is {count} bytes, more than the {limit}-byte limit for notebooks.",
                &limit_args(Some(*bytes), *limit),
            ),
            FindingKind::TooDeep { limit } => message(
                key,
                "The file nests data more than {limit} levels deep, so it wasn't opened.",
                &limit_args(None, *limit),
            ),
            FindingKind::StringTooLong { limit } => message(
                key,
                "The file contains a single value longer than {limit} bytes, so it wasn't opened.",
                &limit_args(None, *limit),
            ),
            FindingKind::NotJson => message(key, "The file isn't a readable notebook.", &[]),
            FindingKind::TooManyCells { count, limit } => message(
                key,
                "The notebook has {count} cells, more than the limit of {limit}.",
                &limit_args(Some(*count), *limit),
            ),
            FindingKind::TooManyParameters { count, limit } => message(
                key,
                "The notebook has {count} parameters, more than the limit of {limit}.",
                &limit_args(Some(*count), *limit),
            ),
            FindingKind::TooManyConnections { count, limit } => message(
                key,
                "The notebook has {count} connections, more than the limit of {limit}.",
                &limit_args(Some(*count), *limit),
            ),
            FindingKind::EmbeddedCredential => message(
                key,
                "A connection in this notebook contains a password or connection string. Notebooks must refer to connections only by name, so it wasn't opened.",
                &[],
            ),
            FindingKind::UnsafePath => message(
                key,
                "An attachment points outside the notebook's folder, so it wasn't opened.",
                &[],
            ),
            FindingKind::RemoteResource => message(
                key,
                "Text cells link to images or pages on the internet. Web images are never loaded, and links open only when you click them.",
                &[],
            ),
            FindingKind::ActiveContent => message(
                key,
                "Text cells contain scripts or embedded pages. They are shown as plain text and never run.",
                &[],
            ),
            FindingKind::AutoRunRequested => message(
                key,
                "The notebook asks to run cells automatically. Nothing runs until you trust it.",
                &[],
            ),
            FindingKind::EmbeddedOutputs => message(
                key,
                "The notebook includes saved results from another run. They are shown as a snapshot, not as live data.",
                &[],
            ),
            FindingKind::SecretInContent => message(
                key,
                "The notebook contains something that looks like a password or key.",
                &[],
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanReport {
    pub findings: Vec<Finding>,
    pub bytes: usize,
    pub cells: usize,
}

impl ScanReport {
    pub fn rejected(&self) -> bool {
        self.findings.iter().any(|f| f.severity == Severity::Reject)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.severity == Severity::Warning)
    }

    fn push(&mut self, severity: Severity, kind: FindingKind, location: &str) {
        let finding = Finding {
            severity,
            kind,
            location: location.to_string(),
        };
        if !self.findings.contains(&finding) {
            self.findings.push(finding);
        }
    }
}

/// Depth and longest string, without parsing. Stops early past the limits.
fn shape(text: &str, limits: &ScanLimits) -> Result<(), FindingKind> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut string_len = 0usize;
    for &b in text.as_bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
                continue;
            }
            string_len += 1;
            if string_len > limits.max_string_bytes {
                return Err(FindingKind::StringTooLong {
                    limit: limits.max_string_bytes,
                });
            }
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                string_len = 0;
            }
            b'{' | b'[' => {
                depth += 1;
                if depth > limits.max_depth {
                    return Err(FindingKind::TooDeep {
                        limit: limits.max_depth,
                    });
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

static REMOTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)!\[[^\]]*\]\(\s*<?(?:https?:)?//|<\s*(?:img|video|audio|source|link|embed|object)\b[^>]*\b(?:src|href|data|srcset)\s*=\s*["']?\s*(?:https?:)?//|url\(\s*["']?(?:https?:)?//"#)
        .expect("remote pattern")
});

static ACTIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<\s*(?:script|iframe|frame|object|embed|form|meta|base)\b|\bon[a-z]+\s*=\s*["']|(?:javascript|vbscript)\s*:|data\s*:\s*text/html"#)
        .expect("active pattern")
});

/// Whether an attachment path stays inside the notebook's folder.
pub fn is_safe_relative_path(path: &str) -> bool {
    let path = path.trim();
    if path.is_empty() || path.contains('\0') || path.starts_with('~') {
        return false;
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return false;
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return false;
    }
    if let Some(colon) = path.find(':') {
        // `scheme:` (file:, http:, smb:) — anything before the first
        // separator that ends in a colon.
        if !path[..colon].contains(['/', '\\']) {
            return false;
        }
    }
    !path
        .split(['/', '\\'])
        .any(|part| part.starts_with("..") && part.trim_end_matches(['.', ' ']).is_empty())
}

fn pointer(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}

/// Text of a source that may be a string or an ipynb list of lines.
fn source_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(lines) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// Credentials anywhere under a connection reference.
fn scan_connection(value: &Value, location: &str, report: &mut ScanReport) {
    match value {
        Value::String(text) => {
            if contains_secret(text) || looks_like_connection_string(text) {
                report.push(Severity::Reject, FindingKind::EmbeddedCredential, location);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                scan_connection(item, &pointer(location, &i.to_string()), report);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                let here = pointer(location, key);
                let lower = key.to_ascii_lowercase();
                let non_empty = !matches!(item, Value::Null) && item.as_str() != Some("");
                if non_empty
                    && (is_secret_key(key)
                        || lower.contains("password")
                        || lower.contains("secret")
                        || lower == "key"
                        || lower == "uri"
                        || lower == "url"
                        || lower == "dsn")
                {
                    report.push(Severity::Reject, FindingKind::EmbeddedCredential, &here);
                } else {
                    scan_connection(item, &here, report);
                }
            }
        }
        _ => {}
    }
}

/// `Server=…;User Id=…` style text (any `key=value;` pairs naming a host
/// plus a user or secret).
fn looks_like_connection_string(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let has = |k: &str| lower.contains(k);
    text.contains(';')
        && (has("server=") || has("data source=") || has("host=") || has("address="))
        && (has("user id=") || has("uid=") || has("user=") || has("password") || has("pwd="))
}

fn scan_attachment_paths(spec: &Value, location: &str, report: &mut ScanReport) {
    match spec {
        Value::Object(map) => {
            for (key, value) in map {
                let here = pointer(location, key);
                let lower = key.to_ascii_lowercase();
                match value {
                    Value::String(path)
                        if matches!(
                            lower.as_str(),
                            "path" | "file" | "src" | "href" | "uri" | "url" | "source"
                        ) =>
                    {
                        if !is_safe_relative_path(path) {
                            report.push(Severity::Reject, FindingKind::UnsafePath, &here);
                        }
                    }
                    other => scan_attachment_paths(other, &here, report),
                }
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                scan_attachment_paths(item, &pointer(location, &i.to_string()), report);
            }
        }
        _ => {}
    }
}

/// Secret-shaped strings and secret-named keys in cell content.
fn scan_content(value: &Value, location: &str, report: &mut ScanReport) {
    match value {
        Value::String(text) => {
            if contains_secret(text) {
                report.push(Severity::Warning, FindingKind::SecretInContent, location);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                scan_content(item, &pointer(location, &i.to_string()), report);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                let here = pointer(location, key);
                if is_secret_key(key) && item.as_str().is_some_and(|s| !s.is_empty()) {
                    report.push(Severity::Warning, FindingKind::SecretInContent, &here);
                } else {
                    scan_content(item, &here, report);
                }
            }
        }
        _ => {}
    }
}

fn cell_kind(cell: &Value) -> String {
    cell.get("kind")
        .or_else(|| cell.get("type"))
        .or_else(|| cell.get("cell_type"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn scan_cells(cells: &[Value], report: &mut ScanReport) {
    for (i, cell) in cells.iter().enumerate() {
        let at = format!("/cells/{i}");
        let kind = cell_kind(cell);
        let source = cell.get("source").map(source_text).unwrap_or_default();
        if kind == "markdown" || kind == "raw" {
            if REMOTE.is_match(&source) {
                report.push(
                    Severity::Warning,
                    FindingKind::RemoteResource,
                    &format!("{at}/source"),
                );
            }
            if ACTIVE.is_match(&source) {
                report.push(
                    Severity::Warning,
                    FindingKind::ActiveContent,
                    &format!("{at}/source"),
                );
            }
        } else if contains_secret(&source) {
            report.push(
                Severity::Warning,
                FindingKind::SecretInContent,
                &format!("{at}/source"),
            );
        }
        if let Some(spec) = cell.get("spec") {
            if kind == "attachment" {
                scan_attachment_paths(spec, &format!("{at}/spec"), report);
            }
            scan_content(spec, &format!("{at}/spec"), report);
        }
        if kind == "attachment" {
            for key in ["path", "file", "src"] {
                if let Some(path) = cell.get(key).and_then(Value::as_str)
                    && !is_safe_relative_path(path)
                {
                    report.push(
                        Severity::Reject,
                        FindingKind::UnsafePath,
                        &format!("{at}/{key}"),
                    );
                }
            }
        }
        if cell
            .get("outputs")
            .and_then(Value::as_array)
            .is_some_and(|o| !o.is_empty())
        {
            report.push(
                Severity::Warning,
                FindingKind::EmbeddedOutputs,
                &format!("{at}/outputs"),
            );
        }
        let sqlly_meta = cell.get("metadata").and_then(|m| m.get("sqlly"));
        for (path, slot) in [
            ("connection", cell.get("connection")),
            (
                "metadata/sqlly/connection",
                sqlly_meta.and_then(|m| m.get("connection")),
            ),
        ] {
            if let Some(slot) = slot.filter(|s| s.is_object()) {
                scan_connection(slot, &format!("{at}/{path}"), report);
            }
        }
        if let Some(meta) = sqlly_meta {
            if kind == "attachment"
                && let Some(spec) = meta.get("spec")
            {
                scan_attachment_paths(spec, &format!("{at}/metadata/sqlly/spec"), report);
            }
            if let Some(slot) = meta.get("connection_ref").filter(|s| s.is_string()) {
                scan_connection(slot, &format!("{at}/metadata/sqlly/connection_ref"), report);
            }
        }
        if let Some(slot) = cell.get("connection_ref").filter(|s| s.is_string()) {
            scan_connection(slot, &format!("{at}/connection_ref"), report);
        }
    }
}

/// Check raw notebook text against `limits`.
pub fn scan_with(text: &str, limits: &ScanLimits) -> ScanReport {
    let mut report = ScanReport {
        bytes: text.len(),
        ..ScanReport::default()
    };
    if text.len() > limits.max_bytes {
        report.push(
            Severity::Reject,
            FindingKind::TooLarge {
                bytes: text.len(),
                limit: limits.max_bytes,
            },
            "",
        );
        return report;
    }
    if let Err(kind) = shape(text, limits) {
        report.push(Severity::Reject, kind, "");
        return report;
    }
    let Ok(root) = serde_json::from_str::<Value>(text) else {
        report.push(Severity::Reject, FindingKind::NotJson, "");
        return report;
    };
    let Some(top) = root.as_object() else {
        report.push(Severity::Reject, FindingKind::NotJson, "");
        return report;
    };
    let cells = top.get("cells").and_then(Value::as_array);
    report.cells = cells.map_or(0, Vec::len);
    if report.cells > limits.max_cells {
        report.push(
            Severity::Reject,
            FindingKind::TooManyCells {
                count: report.cells,
                limit: limits.max_cells,
            },
            "/cells",
        );
    }
    let sqlly_meta = top.get("metadata").and_then(|m| m.get("sqlly"));
    let parameters = top
        .get("parameters")
        .or_else(|| sqlly_meta.and_then(|m| m.get("parameters")))
        .and_then(Value::as_array);
    if let Some(parameters) = parameters {
        if parameters.len() > limits.max_parameters {
            report.push(
                Severity::Reject,
                FindingKind::TooManyParameters {
                    count: parameters.len(),
                    limit: limits.max_parameters,
                },
                "/parameters",
            );
        }
        scan_content(
            &Value::Array(parameters.clone()),
            "/parameters",
            &mut report,
        );
    }
    for (path, refs) in [
        ("/connection_refs", top.get("connection_refs")),
        ("/connections", top.get("connections")),
        (
            "/metadata/sqlly/connection_refs",
            sqlly_meta.and_then(|m| m.get("connection_refs")),
        ),
        (
            "/metadata/sqlly/connections",
            sqlly_meta.and_then(|m| m.get("connections")),
        ),
    ] {
        let Some(refs) = refs else { continue };
        let count = refs
            .as_array()
            .map(Vec::len)
            .or_else(|| refs.as_object().map(serde_json::Map::len))
            .unwrap_or(0);
        if count > limits.max_connection_refs {
            report.push(
                Severity::Reject,
                FindingKind::TooManyConnections {
                    count,
                    limit: limits.max_connection_refs,
                },
                path,
            );
        }
        scan_connection(refs, path, &mut report);
    }
    let run_mode = top
        .get("settings")
        .and_then(|s| s.get("run_mode"))
        .or_else(|| sqlly_meta.and_then(|m| m.get("run_mode")));
    if run_mode.and_then(Value::as_str) == Some("reactive") {
        report.push(
            Severity::Warning,
            FindingKind::AutoRunRequested,
            "/settings/run_mode",
        );
    }
    if top.get("outputs").is_some_and(|o| !o.is_null())
        || top
            .get("data_retention")
            .and_then(|d| d.get("embed_outputs"))
            .and_then(Value::as_bool)
            == Some(true)
    {
        report.push(Severity::Warning, FindingKind::EmbeddedOutputs, "/outputs");
    }
    if let Some(cells) = cells {
        scan_cells(cells, &mut report);
    }
    report.findings.sort();
    report
}

/// Check raw notebook text against the default limits.
pub fn scan_notebook_file(text: &str) -> ScanReport {
    scan_with(text, &ScanLimits::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::{CellKind, NotebookDoc, RandomIds, SeededIds, migrate};
    use serde_json::json;
    use std::time::{Duration, Instant};

    fn kinds(report: &ScanReport) -> Vec<FindingKind> {
        report.findings.iter().map(|f| f.kind.clone()).collect()
    }

    fn clean_doc() -> NotebookDoc {
        let mut ids = SeededIds(21);
        let mut doc = NotebookDoc::new("Clean", &mut ids);
        doc.cells[0].source = "SELECT 1".into();
        let i = doc.add_cell(CellKind::Markdown, None, &mut ids);
        doc.cells[i].source = "# Title\n\n![chart](images/chart.png)".into();
        doc
    }

    #[test]
    fn a_clean_notebook_has_no_findings() {
        let report = scan_notebook_file(&clean_doc().to_canonical_json());
        assert!(report.findings.is_empty(), "{:?}", report.findings);
        assert_eq!(report.cells, 2);
    }

    #[test]
    fn size_depth_and_string_caps_reject_before_parsing() {
        let limits = ScanLimits {
            max_bytes: 1000,
            max_depth: 8,
            max_string_bytes: 50,
            ..ScanLimits::default()
        };
        let big = format!("{{\"cells\": [], \"pad\": \"{}\"}}", "x".repeat(2000));
        assert!(matches!(
            kinds(&scan_with(&big, &limits))[..],
            [FindingKind::TooLarge { .. }]
        ));
        let deep = format!("{}{}", "[".repeat(20), "]".repeat(20));
        assert_eq!(
            kinds(&scan_with(&deep, &limits)),
            [FindingKind::TooDeep { limit: 8 }]
        );
        let long = format!("{{\"cells\": [], \"a\": \"{}\"}}", "y".repeat(60));
        assert_eq!(
            kinds(&scan_with(&long, &limits)),
            [FindingKind::StringTooLong { limit: 50 }]
        );
        // Brackets inside strings don't count as depth.
        let quoted = format!("{{\"cells\": [], \"a\": \"{}\"}}", "[".repeat(40));
        assert!(scan_with(&quoted, &limits).findings.is_empty());
        assert_eq!(kinds(&scan_with("{ nope", &limits)), [FindingKind::NotJson]);
        assert_eq!(kinds(&scan_with("[1, 2]", &limits)), [FindingKind::NotJson]);
    }

    #[test]
    fn count_caps() {
        let limits = ScanLimits {
            max_cells: 2,
            max_parameters: 1,
            max_connection_refs: 1,
            ..ScanLimits::default()
        };
        let text = json!({
            "cells": [{"kind": "sql", "source": ""}, {"kind": "sql", "source": ""}, {"kind": "sql", "source": ""}],
            "parameters": [{"name": "a"}, {"name": "b"}],
            "connection_refs": [{"id": "a", "label": "A"}, {"id": "b", "label": "B"}],
        })
        .to_string();
        let found = kinds(&scan_with(&text, &limits));
        assert!(found.contains(&FindingKind::TooManyCells { count: 3, limit: 2 }));
        assert!(found.contains(&FindingKind::TooManyParameters { count: 2, limit: 1 }));
        assert!(found.contains(&FindingKind::TooManyConnections { count: 2, limit: 1 }));
    }

    #[test]
    fn credentials_in_connection_refs_are_rejected() {
        let cases = [
            json!({"id": "db", "label": "Prod", "password": "hunter2"}),
            json!({"id": "db", "label": "Server=prod;User Id=sa;Password=hunter2;"}),
            json!({"id": "db", "label": "Server=prod;Database=x;User Id=sa"}),
            json!({"id": "db", "label": "db", "database": "postgres://u:p4ss@h/db"}),
            json!({"id": "db", "label": "db", "connection_string": "Data Source=x"}),
            json!({"id": "db", "label": "db", "options": {"url": "mysql://root@h"}}),
            json!({"id": "db", "label": "db", "secret_ref": {"kind": "in_memory", "key": "pw"}}),
        ];
        for slot in cases {
            let text = json!({"cells": [], "connection_refs": [slot]}).to_string();
            let report = scan_notebook_file(&text);
            assert!(report.rejected(), "{text}");
            assert!(
                kinds(&report).contains(&FindingKind::EmbeddedCredential),
                "{text}"
            );
        }
        let ipynb = json!({"cells": [], "metadata": {"sqlly": {"connection_refs": [{"id": "x", "pwd": "p"}]}}}).to_string();
        assert!(scan_notebook_file(&ipynb).rejected());
        let fine = json!({"cells": [], "connection_refs": [{"id": "db", "profile_id": "p-1", "label": "Orders (prod)", "database": "sales"}]}).to_string();
        assert!(scan_notebook_file(&fine).findings.is_empty());
    }

    #[test]
    fn attachment_paths_must_stay_inside_the_notebook_folder() {
        for bad in [
            "../secrets.csv",
            "data/../../etc/passwd",
            "..\\..\\Windows\\win.ini",
            "/etc/passwd",
            "\\\\server\\share\\x.csv",
            "C:\\Users\\me\\x.csv",
            "c:x.csv",
            "~/.ssh/id_rsa",
            "file:///etc/passwd",
            "https://evil.example/x.csv",
            "data/..",
            "a\0b",
            "",
        ] {
            assert!(!is_safe_relative_path(bad), "{bad:?}");
            let text =
                json!({"cells": [{"kind": "attachment", "source": "", "spec": {"path": bad}}]})
                    .to_string();
            assert!(
                kinds(&scan_notebook_file(&text)).contains(&FindingKind::UnsafePath),
                "{bad:?}"
            );
        }
        for good in [
            "data/orders.csv",
            "images/a b.png",
            "x..y.csv",
            "./local.parquet",
        ] {
            assert!(is_safe_relative_path(good), "{good}");
        }
        let nested =
            json!({"cells": [{"kind": "attachment", "spec": {"files": [{"src": "../../x"}]}}]})
                .to_string();
        assert!(scan_notebook_file(&nested).rejected());
    }

    #[test]
    fn markdown_remote_and_active_content_is_flagged() {
        let cases = [
            (
                "![x](https://tracker.example/pixel.png)",
                FindingKind::RemoteResource,
            ),
            ("![x](//cdn.example/a.png)", FindingKind::RemoteResource),
            (
                "<img src=\"http://evil/x.png\">",
                FindingKind::RemoteResource,
            ),
            (
                "<div style=\"background:url(https://x/y)\">",
                FindingKind::RemoteResource,
            ),
            (
                "<script>fetch('https://x')</script>",
                FindingKind::ActiveContent,
            ),
            ("[click](javascript:alert(1))", FindingKind::ActiveContent),
            ("<img src=x onerror='alert(1)'>", FindingKind::ActiveContent),
            (
                "<iframe src=\"https://x\"></iframe>",
                FindingKind::ActiveContent,
            ),
            (
                "[x](data:text/html;base64,PHNjcmlwdD4=)",
                FindingKind::ActiveContent,
            ),
        ];
        for (source, kind) in cases {
            let text = json!({"cells": [{"kind": "markdown", "source": source}]}).to_string();
            let report = scan_notebook_file(&text);
            assert!(
                kinds(&report).contains(&kind),
                "{source}: {:?}",
                report.findings
            );
            assert!(!report.rejected());
        }
        let ipynb =
            json!({"cells": [{"cell_type": "markdown", "source": ["![a](", "https://x/y.png)"]}]})
                .to_string();
        assert!(kinds(&scan_notebook_file(&ipynb)).contains(&FindingKind::RemoteResource));
        let link =
            json!({"cells": [{"kind": "markdown", "source": "[docs](https://sqlly.app/docs)"}]})
                .to_string();
        assert!(
            scan_notebook_file(&link).findings.is_empty(),
            "plain links load nothing"
        );
    }

    #[test]
    fn auto_run_outputs_and_secrets_are_warnings() {
        let text = json!({
            "settings": {"run_mode": "reactive"},
            "data_retention": {"embed_outputs": true},
            "cells": [
                {"kind": "sql", "source": "CREATE LOGIN x WITH PASSWORD = 'p@ss-123'"},
                {"cell_type": "code", "source": ["SELECT 1"], "outputs": [{"output_type": "stream"}]},
                {"kind": "chart", "spec": {"webhook": {"token": "abc"}}},
            ],
        })
        .to_string();
        let report = scan_notebook_file(&text);
        let found = kinds(&report);
        assert!(found.contains(&FindingKind::AutoRunRequested));
        assert!(found.contains(&FindingKind::EmbeddedOutputs));
        assert!(found.contains(&FindingKind::SecretInContent));
        assert!(!report.rejected());
        let locations: Vec<&str> = report
            .findings
            .iter()
            .map(|f| f.location.as_str())
            .collect();
        assert!(locations.contains(&"/cells/0/source"));
        assert!(locations.contains(&"/cells/2/spec/webhook/token"));
    }

    #[test]
    fn every_finding_message_has_a_key_and_no_unfilled_placeholders() {
        let all = [
            FindingKind::TooLarge { bytes: 9, limit: 1 },
            FindingKind::TooDeep { limit: 1 },
            FindingKind::StringTooLong { limit: 1 },
            FindingKind::NotJson,
            FindingKind::TooManyCells { count: 2, limit: 1 },
            FindingKind::TooManyParameters { count: 2, limit: 1 },
            FindingKind::TooManyConnections { count: 2, limit: 1 },
            FindingKind::EmbeddedCredential,
            FindingKind::UnsafePath,
            FindingKind::RemoteResource,
            FindingKind::ActiveContent,
            FindingKind::AutoRunRequested,
            FindingKind::EmbeddedOutputs,
            FindingKind::SecretInContent,
        ];
        for kind in all {
            let f = Finding {
                severity: Severity::Warning,
                kind,
                location: String::new(),
            };
            let m = f.message();
            assert!(m.key().unwrap().starts_with("notebook.security.scan."));
            assert!(!m.fallback().contains('{'), "{}", m.fallback());
        }
    }

    /// xorshift64*, so failures reproduce from the seed.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    const FRAGMENTS: &[&str] = &[
        "{",
        "}",
        "[",
        "]",
        ",",
        ":",
        "\"",
        "\\",
        "\\u0000",
        "\\ud800",
        "null",
        "true",
        "1e999",
        "-0",
        "\"cells\"",
        "\"kind\"",
        "\"sql\"",
        "\"markdown\"",
        "\"attachment\"",
        "\"spec\"",
        "\"source\"",
        "\"format_version\"",
        "99",
        "2",
        "\"format\"",
        "\"sqlly.notebook\"",
        "\"connection_refs\"",
        "\"parameters\"",
        "\"type\"",
        "\"cell_type\"",
        "\"nbformat\"",
        "4",
        "\"../../x\"",
        "\"password\"",
        "\u{202e}",
        "\u{feff}",
        "{{",
        "}}",
        "{{param.x}}",
    ];

    fn mutate(seed: &str, rng: &mut Rng) -> String {
        let mut bytes: Vec<char> = seed.chars().collect();
        for _ in 0..1 + rng.below(8) {
            let at = rng.below(bytes.len() + 1);
            match rng.below(4) {
                0 if !bytes.is_empty() => {
                    let end = (at + rng.below(16)).min(bytes.len());
                    bytes.drain(at.min(bytes.len())..end);
                }
                1 => {
                    let fragment = FRAGMENTS[rng.below(FRAGMENTS.len())];
                    for (i, c) in fragment.chars().enumerate() {
                        bytes.insert((at + i).min(bytes.len()), c);
                    }
                }
                2 if !bytes.is_empty() => {
                    let i = at.min(bytes.len() - 1);
                    bytes[i] = char::from_u32(rng.next() as u32 % 0x80).unwrap_or('x');
                }
                _ => {
                    let n = rng.below(40);
                    let open = if rng.below(2) == 0 { "[" } else { "{\"a\":" };
                    for _ in 0..n {
                        bytes.insert(at.min(bytes.len()), open.chars().next().unwrap());
                    }
                }
            }
        }
        bytes.into_iter().collect()
    }

    #[test]
    fn randomized_hostile_files_never_panic_and_stay_fast() {
        let mut doc = clean_doc();
        doc.parameters
            .push(crate::notebook::params::ParameterDef::new(
                "region",
                crate::notebook::params::ParamType::String,
            ));
        let seeds = [
            doc.to_canonical_json(),
            migrate::to_ipynb(&doc),
            r#"{"cells":[{"cell_type":"sql","source":"SELECT 1"}],"params":{"a":"b"}}"#.to_string(),
        ];
        let mut rng = Rng(0x0005_eed0_f5ec);
        let started = Instant::now();
        let mut slowest = Duration::ZERO;
        let (mut accepted, mut loaded) = (0usize, 0usize);
        for round in 0..3000 {
            let text = mutate(&seeds[round % seeds.len()], &mut rng);
            let t = Instant::now();
            let report = scan_notebook_file(&text);
            accepted += usize::from(!report.rejected());
            // The loader must survive anything, scanned or not.
            if migrate::load(Some("x.sqllynb"), &text, &mut SeededIds(round as u64), None).is_ok() {
                loaded += 1;
            }
            let _ = migrate::load(Some("x.ipynb"), &text, &mut RandomIds, None);
            slowest = slowest.max(t.elapsed());
        }
        assert!(
            slowest < Duration::from_millis(500),
            "slowest case took {slowest:?}"
        );
        assert!(
            accepted > 100 && loaded > 100,
            "mutations too destructive: {accepted} / {loaded}"
        );
        assert!(started.elapsed() < Duration::from_secs(60));
    }

    #[test]
    fn pathological_but_valid_inputs_are_bounded() {
        let deep = format!("{}1{}", "[".repeat(100_000), "]".repeat(100_000));
        let t = Instant::now();
        assert!(scan_notebook_file(&deep).rejected());
        assert!(migrate::load(None, &deep, &mut SeededIds(1), None).is_err());
        let wide = format!(
            "{{\"cells\": [{}]}}",
            vec!["{\"kind\":\"sql\",\"source\":\"\"}"; 6000].join(",")
        );
        assert!(
            kinds(&scan_notebook_file(&wide)).contains(&FindingKind::TooManyCells {
                count: 6000,
                limit: 5000
            })
        );
        let markdown_bomb =
            json!({"cells": [{"kind": "markdown", "source": "![".repeat(200_000)}]}).to_string();
        let _ = scan_notebook_file(&markdown_bomb);
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    }
}
