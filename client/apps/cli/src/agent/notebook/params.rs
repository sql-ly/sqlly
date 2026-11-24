//! `--param name=value` and `--param-set file.json` → typed parameter
//! values, plus scrubbing redacted values out of text that leaves the run.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;
use sqlly_services::notebook::params::{ParamValue, ParameterDef, from_json, parse_text};
use sqlly_services::notebook::{NotebookDoc, ResolvedParams};

use crate::agent::CliError;

fn definition<'a>(doc: &'a NotebookDoc, name: &str) -> Result<&'a ParameterDef, CliError> {
    doc.parameter(name).ok_or_else(|| {
        let known: Vec<&str> = doc.parameters.iter().map(|p| p.name.as_str()).collect();
        CliError::usage(if known.is_empty() {
            format!("the notebook has no parameter '{name}'")
        } else {
            format!(
                "the notebook has no parameter '{name}' (parameters: {})",
                known.join(", ")
            )
        })
    })
}

fn invalid(def: &ParameterDef, error: &sqlly_services::notebook::ParamError) -> CliError {
    CliError::usage(format!(
        "parameter '{}': {}",
        def.name,
        error.message().fallback()
    ))
}

/// Read a parameter set: `{"name": value, …}` or `{"params": {…}}`, values
/// in the notebook's saved JSON form (dates as `"2026-11-01"`, ranges as
/// `{"start": …, "end": …}`, lists as arrays).
pub fn read_param_set(path: &Path) -> Result<BTreeMap<String, Value>, CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::io(format!("cannot read {}: {e}", path.display())))?;
    parse_param_set(&text)
        .map_err(|message| CliError::usage(format!("{}: {message}", path.display())))
}

pub fn parse_param_set(text: &str) -> Result<BTreeMap<String, Value>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let object = match value {
        Value::Object(mut map) => match map.remove("params") {
            Some(Value::Object(inner)) if map.is_empty() => inner,
            Some(_) => return Err("`params` must be the only key and hold an object".into()),
            None => map,
        },
        _ => return Err("a parameter set is a JSON object of name → value".into()),
    };
    Ok(object.into_iter().collect())
}

/// Values the caller supplied: the parameter set first, then `--param`
/// flags (which win).
pub fn collect(
    doc: &NotebookDoc,
    param_set: &BTreeMap<String, Value>,
    flags: &[String],
) -> Result<BTreeMap<String, ParamValue>, CliError> {
    let mut supplied = BTreeMap::new();
    for (name, value) in param_set {
        let def = definition(doc, name)?;
        let parsed = from_json(def, value).map_err(|e| invalid(def, &e))?;
        supplied.insert(def.name.clone(), parsed);
    }
    for flag in flags {
        let (name, raw) = flag
            .split_once('=')
            .ok_or_else(|| CliError::usage(format!("--param expects name=value, got '{flag}'")))?;
        let def = definition(doc, name.trim())?;
        let parsed = parse_text(def, raw).map_err(|e| invalid(def, &e))?;
        supplied.insert(def.name.clone(), parsed);
    }
    Ok(supplied)
}

/// A supplied value that fails validation is the caller's mistake: refuse
/// before running anything. (A bad default only blocks the cells using it.)
pub fn check_supplied(
    doc: &NotebookDoc,
    supplied: &BTreeMap<String, ParamValue>,
    resolved: &ResolvedParams,
) -> Result<(), CliError> {
    for name in supplied.keys() {
        if let (Some(def), Some(error)) = (doc.parameter(name), resolved.error(name)) {
            return Err(invalid(def, error));
        }
    }
    Ok(())
}

/// The text forms of every redacted (sensitive or secret) value, longest
/// first, for [`scrub`].
pub fn redacted_texts(doc: &NotebookDoc, resolved: &ResolvedParams) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for def in doc.parameters.iter().filter(|d| d.is_redacted()) {
        if let Some(value) = resolved.get(&def.name) {
            collect_texts(&value.to_json(), &mut out);
        }
    }
    out.retain(|t| t.chars().count() >= 3);
    out.sort_by_key(|t| std::cmp::Reverse(t.len()));
    out.dedup();
    out
}

fn collect_texts(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Number(n) => out.push(n.to_string()),
        Value::Array(items) => items.iter().for_each(|v| collect_texts(v, out)),
        Value::Object(map) => map.values().for_each(|v| collect_texts(v, out)),
        Value::Bool(_) | Value::Null => {}
    }
}

/// `text` with every redacted value replaced by `[redacted]`, so a server
/// error that echoes the SQL never carries a sensitive literal.
pub fn scrub(text: &str, redacted: &[String]) -> String {
    let mut out = text.to_string();
    for secret in redacted {
        out = out.replace(secret.as_str(), "[redacted]");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::notebook::params::{ParamType, SystemClock, resolve_all};

    fn doc() -> NotebookDoc {
        let mut doc = NotebookDoc::empty("nb", "T");
        doc.parameters
            .push(ParameterDef::new("region", ParamType::String));
        doc.parameters
            .push(ParameterDef::new("day", ParamType::Date));
        let mut token = ParameterDef::new("tenant", ParamType::String);
        token.sensitive = true;
        doc.parameters.push(token);
        doc
    }

    #[test]
    fn flags_override_the_set_and_parse_by_type() {
        let d = doc();
        let set = parse_param_set(r#"{"region": "EU", "day": "2026-11-01"}"#).unwrap();
        let supplied = collect(&d, &set, &["region=US".into()]).unwrap();
        assert_eq!(supplied["region"], ParamValue::Text("US".into()));
        assert!(matches!(supplied["day"], ParamValue::Date(_)));
        let wrapped = parse_param_set(r#"{"params": {"region": "EU"}}"#).unwrap();
        assert_eq!(wrapped["region"], Value::String("EU".into()));
    }

    #[test]
    fn unknown_names_and_bad_values_are_usage_errors() {
        let d = doc();
        let empty = BTreeMap::new();
        for flags in [vec!["nope=1"], vec!["day=yesterday-ish"], vec!["region"]] {
            let flags: Vec<String> = flags.into_iter().map(String::from).collect();
            let err = collect(&d, &empty, &flags).unwrap_err();
            assert_eq!(err.exit, crate::agent::ExitCode::Usage, "{flags:?}");
        }
        assert!(parse_param_set("[1]").is_err());
        assert!(parse_param_set(r#"{"params": 1}"#).is_err());
    }

    #[test]
    fn sensitive_values_are_scrubbed() {
        let d = doc();
        let supplied = collect(&d, &BTreeMap::new(), &["tenant=acme-secret-42".into()]).unwrap();
        let resolved = resolve_all(&d.parameters, &supplied, &SystemClock);
        let redacted = redacted_texts(&d, &resolved);
        assert_eq!(redacted, vec!["acme-secret-42".to_string()]);
        let message = scrub(
            "syntax error near 'acme-secret-42' in WHERE tenant = N'acme-secret-42'",
            &redacted,
        );
        assert!(!message.contains("acme-secret-42"));
        assert!(message.contains("[redacted]"));
    }
}
