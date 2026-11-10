//! Provisioned connections: connections an administrator defines outside the
//! app — in a mounted file or in environment variables — that every launch
//! rebuilds and that are never written to the user's saved connections.
//!
//! Sources, in order (a later duplicate name is skipped with a warning):
//!
//! 1. The file named by [`PROVISIONED_FILE_ENV`], or else
//!    [`PROVISIONED_FILE_NAME`] in the app's config directory when present.
//!    It is a SQLLY connection file (the export format read by
//!    [`crate::credential_import`]) whose `format` / `version` may be
//!    omitted, and whose connections may give a `url` (any form
//!    [`crate::parse_connection_url`] reads, `redis+sentinel://` included)
//!    instead of `engine` / `server` / `port` / `database` / sign-in fields.
//! 2. Every `SQLLY_CONNECTION_<NAME>=<url>` environment variable, named
//!    `<NAME>`.
//!
//! Every string may say `${VAR}` to splice in an environment variable
//! (`$${` writes a literal `${`). A password slot whose whole value is
//! `${VAR}` — `authentication.password`, `authentication.token`,
//! `redis_sentinel_password`, or the password / `sentinelPassword` part of a
//! `url` — becomes a [`SecretProvider::Environment`] reference instead, so
//! the secret is read from the environment when connecting and is never
//! copied into a profile. A referenced variable that is not set, an unknown
//! field, or any other problem skips that one connection with a warning.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use uuid::Uuid;

use crate::connection::{
    AuthenticationType, ConnectionId, ConnectionProfile, ConnectionProvenance, SecretProvider,
    SecretRef, TlsMode,
};
use crate::connection_url::parse_connection_url;
use crate::credential_import::{
    CREDENTIAL_FILE_FORMAT, CREDENTIAL_FILE_VERSION, CredentialConnection, CredentialFile,
    engine_export_name, plan_credential_file,
};
use sqlly_lang::sqlmodel::DatabaseType;

/// Environment variable naming the provisioning file.
pub const PROVISIONED_FILE_ENV: &str = "SQLLY_PROVISIONED_CONNECTIONS_FILE";
/// File read from the app's config directory when the variable is unset.
pub const PROVISIONED_FILE_NAME: &str = "provisioned-connections.json";
/// Prefix of the one-variable-per-connection form.
pub const CONNECTION_ENV_PREFIX: &str = "SQLLY_CONNECTION_";

/// Namespace for the stable ids provisioned profiles get from their names,
/// so tabs and recents that point at one survive a restart.
const PROVISIONED_ID_NAMESPACE: Uuid = Uuid::from_u128(0x5a11_7e0d_9b1c_4c6e_8f2a_3d4b_5c6d_7e8f);

/// Placeholder a `${VAR}` password inside a URL is swapped for before
/// parsing, so the parsed password can be recognised as a reference.
const URL_ENV_REF_TOKEN: &str = "SQLLYPROVISIONEDENVREF";

/// One provisioned connection.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvisionedConnection {
    /// The profile the app lists: provenance [`ConnectionProvenance::Provisioned`],
    /// a stable id, and `${VAR}` passwords as [`SecretProvider::Environment`]
    /// references (other secrets are [`SecretProvider::InMemory`]).
    pub profile: ConnectionProfile,
    /// The same connection as a credential-file entry with every secret
    /// resolved, for consumers that take credential documents (the embedded
    /// bridge). Hold it in memory only.
    pub document: Value,
}

/// Everything the provisioning sources yielded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Provisioning {
    pub connections: Vec<ProvisionedConnection>,
    /// One line per skipped entry or ignored setting, saying where and why.
    pub warnings: Vec<String>,
}

impl Provisioning {
    pub fn profiles(&self) -> Vec<ConnectionProfile> {
        self.connections.iter().map(|c| c.profile.clone()).collect()
    }

    /// A credential document holding every connection, or `None` when
    /// there are none.
    pub fn credential_document(&self) -> Option<Value> {
        if self.connections.is_empty() {
            return None;
        }
        Some(serde_json::json!({
            "format": CREDENTIAL_FILE_FORMAT,
            "version": CREDENTIAL_FILE_VERSION,
            "connections": self.connections.iter().map(|c| c.document.clone()).collect::<Vec<_>>(),
        }))
    }
}

/// Where the provisioning file is: [`PROVISIONED_FILE_ENV`] when set, else
/// `default_file` (the app's `<config dir>/SQLLY/provisioned-connections.json`).
/// The flag says whether the path was named explicitly (and so must exist).
pub fn provisioned_file_path(
    vars: &BTreeMap<String, String>,
    default_file: Option<PathBuf>,
) -> Option<(PathBuf, bool)> {
    match vars.get(PROVISIONED_FILE_ENV).map(|p| p.trim()) {
        Some(path) if !path.is_empty() => Some((PathBuf::from(path), true)),
        _ => default_file.map(|path| (path, false)),
    }
}

/// Load from the real process environment and filesystem. `default_file`
/// is read when [`PROVISIONED_FILE_ENV`] is unset, and only if it exists.
pub fn load_provisioned_connections(default_file: Option<PathBuf>) -> Provisioning {
    let vars: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    let mut file = None;
    let mut read_warning = None;
    if let Some((path, explicit)) = provisioned_file_path(&vars, default_file) {
        match std::fs::read_to_string(&path) {
            Ok(text) => file = Some((path, text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !explicit => {}
            Err(error) => {
                read_warning = Some(format!(
                    "provisioned connections file {}: {error}",
                    path.display()
                ));
            }
        }
    }
    let mut out = provision(file.as_ref().map(|(p, t)| (p.as_path(), t.as_str())), &vars);
    if let Some(warning) = read_warning {
        out.warnings.insert(0, warning);
    }
    out
}

/// Build provisioned connections from an optional file `(path, text)` and
/// an environment snapshot. Pure: no filesystem or process-env access.
pub fn provision(file: Option<(&Path, &str)>, vars: &BTreeMap<String, String>) -> Provisioning {
    let mut out = Provisioning::default();
    let mut names = HashSet::new();
    if let Some((path, text)) = file {
        let label = path.display().to_string();
        for (at, entry) in file_entries(&label, text, &mut out.warnings) {
            add_entry(&at, entry, vars, &mut names, &mut out);
        }
    }
    for (key, value) in vars {
        let Some(name) = key.strip_prefix(CONNECTION_ENV_PREFIX) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let entry = serde_json::json!({ "name": name, "url": value });
        add_entry(key, entry, vars, &mut names, &mut out);
    }
    out
}

/// The connection entries of a provisioning file, each labelled for
/// warnings. A file that cannot be read as a whole yields none.
fn file_entries(label: &str, text: &str, warnings: &mut Vec<String>) -> Vec<(String, Value)> {
    let root: Value = match serde_json::from_str(text) {
        Ok(root) => root,
        Err(error) => {
            warnings.push(format!("{label}: not valid JSON: {error}"));
            return Vec::new();
        }
    };
    let Value::Object(mut root) = root else {
        warnings.push(format!(
            "{label}: expected a JSON object with a \"connections\" list"
        ));
        return Vec::new();
    };
    if let Some(format) = root.remove("format")
        && format.as_str() != Some(CREDENTIAL_FILE_FORMAT)
    {
        warnings.push(format!(
            "{label}: format must be \"{CREDENTIAL_FILE_FORMAT}\" when given (found {format})"
        ));
        return Vec::new();
    }
    if let Some(version) = root.remove("version")
        && !version
            .as_u64()
            .is_some_and(|v| v >= 1 && v <= u64::from(CREDENTIAL_FILE_VERSION))
    {
        warnings.push(format!(
            "{label}: version {version} is not supported (expected {CREDENTIAL_FILE_VERSION})"
        ));
        return Vec::new();
    }
    let connections = root.remove("connections");
    for key in root.keys() {
        warnings.push(format!(
            "{label}: \"{key}\" is ignored; a provisioning file only defines connections"
        ));
    }
    match connections {
        None => Vec::new(),
        Some(Value::Array(items)) => items
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty());
                let at = match name {
                    Some(name) => format!("{label}: connections[{index}] \"{name}\""),
                    None => format!("{label}: connections[{index}]"),
                };
                (at, item)
            })
            .collect(),
        Some(_) => {
            warnings.push(format!("{label}: \"connections\" must be a list"));
            Vec::new()
        }
    }
}

fn add_entry(
    at: &str,
    entry: Value,
    vars: &BTreeMap<String, String>,
    names: &mut HashSet<String>,
    out: &mut Provisioning,
) {
    match build_entry(at, entry, vars, &mut out.warnings) {
        Ok(connection) => {
            if names.insert(connection.profile.name.to_lowercase()) {
                out.connections.push(connection);
            } else {
                out.warnings.push(format!(
                    "{at}: skipped, another provisioned connection is already named \"{}\"",
                    connection.profile.name
                ));
            }
        }
        Err(problem) => out.warnings.push(format!("{at}: skipped, {problem}")),
    }
}

/// Which secret slots of an entry name an environment variable.
#[derive(Default)]
struct EnvRefs {
    password: Option<String>,
    token: Option<String>,
    sentinel_password: Option<String>,
}

fn build_entry(
    at: &str,
    entry: Value,
    vars: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) -> Result<ProvisionedConnection, String> {
    let Value::Object(mut object) = entry else {
        return Err("a connection must be a JSON object".to_string());
    };
    let mut refs = EnvRefs::default();

    // Secret slots first: a whole-value `${VAR}` is a reference, held as a
    // placeholder until the rest of the entry is expanded.
    if let Some(Value::Object(auth)) = object.get_mut("authentication") {
        refs.password = take_secret_ref(auth, "password");
        refs.token = take_secret_ref(auth, "token");
    }
    refs.sentinel_password = take_secret_ref(&mut object, "redis_sentinel_password");
    let url = object.remove("url");
    let mut entry = Value::Object(object);
    expand_value_tree(&mut entry, vars)?;
    let Value::Object(mut object) = entry else {
        unreachable!("expansion keeps the object");
    };
    if let Some(url) = url {
        let Value::String(url) = url else {
            return Err("\"url\" must be a string".to_string());
        };
        apply_url(&mut object, &url, vars, &mut refs, at, warnings)?;
    }
    if let Some(Value::Object(auth)) = object.get_mut("authentication") {
        resolve_placeholder(auth, "password", &refs.password, vars)?;
        resolve_placeholder(auth, "token", &refs.token, vars)?;
    }
    resolve_placeholder(
        &mut object,
        "redis_sentinel_password",
        &refs.sentinel_password,
        vars,
    )?;
    let entry = Value::Object(object);
    let mut profile = plan_single_profile(&entry)?;
    profile.id = ConnectionId(Uuid::new_v5(
        &PROVISIONED_ID_NAMESPACE,
        profile.name.to_lowercase().as_bytes(),
    ));
    profile.provenance = ConnectionProvenance::Provisioned;
    let env_ref = |name: &String| SecretRef {
        provider: SecretProvider::Environment,
        key: name.clone(),
    };
    match &mut profile.authentication {
        AuthenticationType::SqlLogin { secret_ref, .. } => {
            if let Some(name) = &refs.password {
                *secret_ref = env_ref(name);
            }
        }
        AuthenticationType::AuthToken { secret_ref } => {
            if let Some(name) = refs.token.as_ref().or(refs.password.as_ref()) {
                *secret_ref = env_ref(name);
            }
        }
        _ => {}
    }
    if let Some(name) = &refs.sentinel_password {
        match profile.redis_sentinel.as_mut() {
            Some(sentinel) => sentinel.sentinel_password_ref = Some(env_ref(name)),
            None => warnings.push(format!(
                "{at}: redis_sentinel_password is ignored without Sentinel settings"
            )),
        }
    }
    Ok(ProvisionedConnection {
        profile,
        document: entry,
    })
}

/// A profile for a connection URL the user opened directly (a raw
/// `mysql://…` startup argument): named `name`, a fresh id, `Manual`
/// provenance, and every secret held `InMemory`. The caller keeps it out of
/// the saved connections file.
pub fn profile_from_parsed_url(
    name: &str,
    parsed: &crate::ParsedConnectionUrl,
) -> Result<ConnectionProfile, String> {
    let mut object = Map::new();
    object.insert("name".to_string(), Value::String(name.to_string()));
    let mut refs = EnvRefs::default();
    let mut warnings = Vec::new();
    fill_from_parsed(
        &mut object,
        parsed,
        parsed.password.clone(),
        parsed.sentinel_password.clone(),
        &mut refs,
        (None, None),
        "url",
        &mut warnings,
    )?;
    let mut profile = plan_single_profile(&Value::Object(object))?;
    profile.id = ConnectionId::new();
    profile.provenance = ConnectionProvenance::Manual;
    Ok(profile)
}

/// Plan one credential-file connection entry into a profile.
fn plan_single_profile(entry: &Value) -> Result<ConnectionProfile, String> {
    let connection: CredentialConnection =
        serde_json::from_value(entry.clone()).map_err(|error| error.to_string())?;
    let file = CredentialFile {
        format: CREDENTIAL_FILE_FORMAT.to_string(),
        version: CREDENTIAL_FILE_VERSION,
        clients: Vec::new(),
        projects: Vec::new(),
        environments: Vec::new(),
        sql_auth_accounts: Vec::new(),
        tunnel_profiles: Vec::new(),
        connections: vec![connection],
    };
    let plan =
        plan_credential_file(&file, crate::credential_proxy::default_port).map_err(|error| {
            // The planner prefixes each problem with `connections[0] "name": `.
            error
                .problems
                .iter()
                .map(|p| p.split_once(": ").map_or(p.as_str(), |(_, rest)| rest))
                .collect::<Vec<_>>()
                .join("; ")
        })?;
    let Some(planned) = plan.connections.into_iter().next() else {
        return Err("no connection could be built".to_string());
    };
    if let Some(account) = planned.sql_account {
        return Err(format!(
            "sql-login account \"{account}\" cannot be used here; give username and password"
        ));
    }
    if let Some(tunnel) = planned.tunnel_profile {
        return Err(format!(
            "tunnel_profile \"{tunnel}\" cannot be used here; give the transport inline"
        ));
    }
    Ok(planned.profile)
}

/// When `object[key]` is exactly `${VAR}`, swap it for a placeholder and
/// return `VAR`.
fn take_secret_ref(object: &mut Map<String, Value>, key: &str) -> Option<String> {
    let Some(Value::String(text)) = object.get(key) else {
        return None;
    };
    let name = whole_reference(text)?;
    object.insert(key.to_string(), Value::String(URL_ENV_REF_TOKEN.into()));
    Some(name)
}

/// Put the variable's value where [`take_secret_ref`] left its placeholder,
/// so the resolved document carries the secret.
fn resolve_placeholder(
    object: &mut Map<String, Value>,
    key: &str,
    name: &Option<String>,
    vars: &BTreeMap<String, String>,
) -> Result<(), String> {
    if let (Some(Value::String(text)), Some(name)) = (object.get(key), name)
        && text == URL_ENV_REF_TOKEN
    {
        object.insert(key.to_string(), Value::String(lookup(vars, name)?));
    }
    Ok(())
}

/// `VAR` when `text` is exactly `${VAR}`.
fn whole_reference(text: &str) -> Option<String> {
    let name = text.trim().strip_prefix("${")?.strip_suffix('}')?;
    valid_var_name(name).then(|| name.to_string())
}

fn valid_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn lookup(vars: &BTreeMap<String, String>, name: &str) -> Result<String, String> {
    vars.get(name)
        .cloned()
        .ok_or_else(|| format!("environment variable {name} is not set"))
}

/// Replace every `${VAR}` in `text` (`$${` is a literal `${`).
pub fn expand_env(text: &str, vars: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        if let Some(after) = tail.strip_prefix("$${") {
            out.push_str("${");
            rest = after;
        } else if let Some(after) = tail.strip_prefix("${")
            && let Some(end) = after.find('}')
            && valid_var_name(&after[..end])
        {
            out.push_str(&lookup(vars, &after[..end])?);
            rest = &after[end + 1..];
        } else {
            out.push('$');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

fn expand_value_tree(value: &mut Value, vars: &BTreeMap<String, String>) -> Result<(), String> {
    match value {
        Value::String(text) => *text = expand_env(text, vars)?,
        Value::Array(items) => {
            for item in items {
                expand_value_tree(item, vars)?;
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                expand_value_tree(item, vars)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Swap a whole-`${VAR}` URL password and `sentinelPassword` value for a
/// placeholder, recording the names, so expansion leaves them alone.
fn mark_url_secret_refs(url: &str) -> (String, Option<String>, Option<String>) {
    let mut out = url.to_string();
    let mut password = None;
    let mut sentinel = None;
    if let Some(scheme_end) = out.find("://") {
        let start = scheme_end + 3;
        let authority_end = out[start..]
            .find(['/', '?', '#'])
            .map_or(out.len(), |i| start + i);
        if let Some(at) = out[start..authority_end].rfind('@') {
            let userinfo = &out[start..start + at];
            if let Some(colon) = userinfo.find(':')
                && let Some(name) = whole_reference(&userinfo[colon + 1..])
            {
                let from = start + colon + 1;
                out.replace_range(from..start + at, URL_ENV_REF_TOKEN);
                password = Some(name);
            }
        }
    }
    if let Some(query_start) = out.find('?') {
        let query = out[query_start + 1..].to_string();
        let rebuilt: Vec<String> = query
            .split('&')
            .map(|pair| match pair.split_once('=') {
                Some((key, value))
                    if matches!(
                        key.to_ascii_lowercase().as_str(),
                        "sentinelpassword" | "sentinel_password"
                    ) && whole_reference(value).is_some() =>
                {
                    sentinel = whole_reference(value);
                    format!("{key}={URL_ENV_REF_TOKEN}")
                }
                _ => pair.to_string(),
            })
            .collect();
        out.replace_range(query_start + 1.., &rebuilt.join("&"));
    }
    (out, password, sentinel)
}

/// Fill the entry's connection fields from `url`, leaving any field the
/// entry already sets alone.
fn apply_url(
    object: &mut Map<String, Value>,
    url: &str,
    vars: &BTreeMap<String, String>,
    refs: &mut EnvRefs,
    at: &str,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let (marked, password_ref, sentinel_ref) = mark_url_secret_refs(url);
    let expanded = expand_env(&marked, vars)?;
    let parsed = parse_connection_url(&expanded).map_err(|e| format!("url: {e}"))?;
    let secret = |value: Option<String>, name: &Option<String>| -> Result<Option<String>, String> {
        match (value, name) {
            (Some(v), Some(name)) if v == URL_ENV_REF_TOKEN => lookup(vars, name).map(Some),
            (value, _) => Ok(value),
        }
    };
    let password = secret(parsed.password.clone(), &password_ref)?;
    let sentinel_password = secret(parsed.sentinel_password.clone(), &sentinel_ref)?;

    fill_from_parsed(
        object,
        &parsed,
        password,
        sentinel_password,
        refs,
        (password_ref, sentinel_ref),
        at,
        warnings,
    )
}

/// Fill the entry's connection fields from a parsed URL whose secrets are
/// already resolved, leaving any field the entry already sets alone.
/// `env_refs` names the environment variables the password and Sentinel
/// password came from, when they did.
#[allow(clippy::too_many_arguments)]
fn fill_from_parsed(
    object: &mut Map<String, Value>,
    parsed: &crate::ParsedConnectionUrl,
    password: Option<String>,
    sentinel_password: Option<String>,
    refs: &mut EnvRefs,
    env_refs: (Option<String>, Option<String>),
    at: &str,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    let (password_ref, sentinel_ref) = env_refs;
    let mut set = |key: &str, value: Value| {
        object.entry(key.to_string()).or_insert(value);
    };
    set("engine", Value::String(engine_export_name(parsed.engine)));
    set("server", Value::String(parsed.server.clone()));
    if let Some(port) = parsed.port {
        set("port", Value::from(port));
    }
    if let Some(database) = &parsed.database {
        set("database", Value::String(database.clone()));
    }
    let file_engine = matches!(parsed.engine, DatabaseType::Sqlite | DatabaseType::DuckDb);
    let authentication = if parsed.integrated_security {
        serde_json::json!({ "type": "integrated" })
    } else if parsed.engine == DatabaseType::LibSql {
        let mut auth = serde_json::json!({ "type": "auth-token" });
        if let Some(token) = &password {
            auth["token"] = Value::String(token.clone());
        }
        auth
    } else if file_engine {
        serde_json::json!({ "type": "none" })
    } else {
        let mut auth = serde_json::json!({ "type": "sql-login" });
        if let Some(user) = parsed.username.as_ref().filter(|u| !u.is_empty()) {
            auth["username"] = Value::String(user.clone());
        }
        if let Some(password) = &password {
            auth["password"] = Value::String(password.clone());
        }
        auth
    };
    if !object.contains_key("authentication") {
        if parsed.engine == DatabaseType::LibSql {
            refs.token = refs.token.take().or(password_ref);
        } else {
            refs.password = refs.password.take().or(password_ref);
        }
        object.insert("authentication".to_string(), authentication);
    }
    if !object.contains_key("tls") {
        let mut tls = Map::new();
        if let Some(mode) = parsed.tls_mode {
            let mode = match mode {
                TlsMode::Disable => Some("disable"),
                TlsMode::Require => Some("require"),
                TlsMode::VerifyCa => Some("verify-ca"),
                TlsMode::VerifyFull => Some("verify-full"),
                _ => None,
            };
            if let Some(mode) = mode {
                tls.insert("mode".into(), Value::String(mode.into()));
            }
        } else {
            if let Some(encrypt) = parsed.encrypt {
                tls.insert("encrypt".into(), Value::Bool(encrypt));
            }
            if let Some(trust) = parsed.trust_server_certificate {
                tls.insert("trust_server_certificate".into(), Value::Bool(trust));
            }
        }
        let path = |p: &PathBuf| Value::String(p.display().to_string());
        if let Some(ca) = &parsed.tls_ca_file {
            tls.insert("ca_file".into(), path(ca));
        }
        if let Some(cert) = &parsed.tls_client_cert_file {
            tls.insert("client_cert_file".into(), path(cert));
        }
        if let Some(key) = &parsed.tls_client_key_file {
            tls.insert("client_key_file".into(), path(key));
        }
        if let Some(direct) = parsed.tls_direct {
            tls.insert("direct".into(), Value::Bool(direct));
        }
        object.insert("tls".to_string(), Value::Object(tls));
    }
    if let Some(sentinel) = &parsed.redis_sentinel
        && !object.contains_key("redis_sentinel")
    {
        let value =
            serde_json::to_value(sentinel.without_secret()).map_err(|e| format!("url: {e}"))?;
        object.insert("redis_sentinel".to_string(), value);
        if let Some(password) = sentinel_password
            && !object.contains_key("redis_sentinel_password")
        {
            object.insert(
                "redis_sentinel_password".to_string(),
                Value::String(password),
            );
            refs.sentinel_password = refs.sentinel_password.take().or(sentinel_ref);
        }
    }
    for (key, _) in &parsed.extra_options {
        warnings.push(format!("{at}: URL option \"{key}\" is ignored"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_import::parse_credential_file;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn file(text: &str, vars: &BTreeMap<String, String>) -> Provisioning {
        provision(Some((Path::new("prov.json"), text)), vars)
    }

    fn env_ref(name: &str) -> SecretRef {
        SecretRef {
            provider: SecretProvider::Environment,
            key: name.to_string(),
        }
    }

    #[test]
    fn sentinel_url_entry_keeps_passwords_as_environment_references() {
        let env = vars(&[
            ("REDIS_PASSWORD", "data-s3cret"),
            ("SENTINEL_PASSWORD", "sent-s3cret"),
            ("SENTINEL_HOST", "sentinel-1.internal"),
        ]);
        let out = file(
            r#"{ "connections": [ {
                "name": "Cache",
                "url": "redis+sentinel://app:${REDIS_PASSWORD}@sentinel-0.internal:26379,${SENTINEL_HOST}:26380/mymaster/2?sentinelPassword=${SENTINEL_PASSWORD}",
                "environment": "production"
            } ] }"#,
            &env,
        );
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        let [connection] = out.connections.as_slice() else {
            panic!("one connection");
        };
        let profile = &connection.profile;
        assert_eq!(profile.name, "Cache");
        assert_eq!(profile.database_type, DatabaseType::Redis);
        assert_eq!(profile.database.as_deref(), Some("2"));
        assert_eq!(profile.provenance, ConnectionProvenance::Provisioned);
        assert_eq!(profile.tls_mode, TlsMode::Disable);
        let AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } = &profile.authentication
        else {
            panic!("sql login");
        };
        assert_eq!(username, "app");
        assert_eq!(secret_ref, &env_ref("REDIS_PASSWORD"));
        let sentinel = profile.redis_sentinel.as_ref().expect("sentinel");
        assert_eq!(sentinel.master_name, "mymaster");
        assert_eq!(
            sentinel.sentinels,
            vec![
                "sentinel-0.internal:26379".to_string(),
                "sentinel-1.internal:26380".to_string()
            ]
        );
        assert_eq!(
            sentinel.sentinel_password_ref,
            Some(env_ref("SENTINEL_PASSWORD"))
        );
        // Nothing secret is in the profile itself.
        let json = serde_json::to_string(profile).unwrap();
        assert!(
            !json.contains("data-s3cret") && !json.contains("sent-s3cret"),
            "{json}"
        );
        // The resolved document carries the secrets for the embedded bridge.
        let document = out.credential_document().unwrap().to_string();
        assert!(document.contains("data-s3cret") && document.contains("sent-s3cret"));
    }

    #[test]
    fn tls_and_valkey_sentinel_schemes_and_default_user() {
        let out = provision(
            None,
            &vars(&[
                ("SQLLY_CONNECTION_SECURE", "rediss+sentinel://:pw@s1/cache"),
                (
                    "SQLLY_CONNECTION_VALKEY",
                    "valkey+sentinel://s2:5000/vmaster?db=4",
                ),
            ]),
        );
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        let secure = &out.connections[0].profile;
        assert_eq!(secure.name, "SECURE");
        assert_eq!(secure.tls_mode, TlsMode::Require);
        assert!(secure.encrypt);
        // No user: Redis signs in as `default`; a literal password stays in memory.
        let AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } = &secure.authentication
        else {
            panic!("sql login");
        };
        assert!(username.is_empty());
        assert_eq!(secret_ref.provider, SecretProvider::InMemory);
        assert_eq!(secret_ref.key, "pw");
        assert_eq!(
            secure.redis_sentinel.as_ref().unwrap().sentinels,
            vec!["s1:26379".to_string()]
        );
        let valkey = &out.connections[1].profile;
        assert_eq!(valkey.database_type, DatabaseType::Valkey);
        assert_eq!(valkey.database.as_deref(), Some("4"));
        assert_eq!(
            valkey.redis_sentinel.as_ref().unwrap().master_name,
            "vmaster"
        );
    }

    #[test]
    fn field_entries_expand_variables_and_reference_passwords() {
        let env = vars(&[
            ("PGHOST", "db.internal"),
            ("PGPASSWORD", "pg-secret"),
            ("STAGE", "blue"),
        ]);
        let out = file(
            r#"{ "format": "sqlly-credentials", "version": 1, "connections": [ {
                "name": "Orders ${STAGE}",
                "engine": "postgres",
                "server": "${PGHOST}",
                "database": "orders",
                "authentication": { "type": "sql-login", "username": "app", "password": "${PGPASSWORD}" }
            } ] }"#,
            &env,
        );
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        let profile = &out.connections[0].profile;
        assert_eq!(profile.name, "Orders blue");
        assert_eq!(profile.server, "db.internal");
        assert_eq!(profile.port, 5432);
        assert_eq!(
            profile.authentication.secret_ref(),
            Some(&env_ref("PGPASSWORD"))
        );
        assert!(
            !serde_json::to_string(profile)
                .unwrap()
                .contains("pg-secret")
        );
    }

    #[test]
    fn bad_entries_are_skipped_with_a_warning_and_the_rest_load() {
        let out = file(
            r#"{ "clients": [], "connections": [
                { "name": "Typo", "engine": "postgres", "server": "h",
                  "authentication": { "type": "sql-login", "username": "u", "pasword": "x" } },
                { "name": "Missing var", "url": "redis://:${NOT_SET_ANYWHERE}@h" },
                { "name": "Bad url", "url": "nosuchengine://h" },
                "not an object",
                { "name": "Good", "url": "redis://cache:6380/1?client_name=x" }
            ] }"#,
            &vars(&[]),
        );
        let names: Vec<_> = out
            .connections
            .iter()
            .map(|c| c.profile.name.as_str())
            .collect();
        assert_eq!(names, vec!["Good"]);
        assert_eq!(out.connections[0].profile.port, 6380);
        let all = out.warnings.join("\n");
        assert!(all.contains("\"clients\" is ignored"), "{all}");
        assert!(
            all.contains("\"Typo\": skipped") && all.contains("pasword"),
            "{all}"
        );
        assert!(all.contains("NOT_SET_ANYWHERE is not set"), "{all}");
        assert!(all.contains("\"Bad url\": skipped"), "{all}");
        assert!(all.contains("connections[3]: skipped"), "{all}");
        assert!(
            all.contains("URL option \"client_name\" is ignored"),
            "{all}"
        );
    }

    #[test]
    fn unreadable_files_warn_and_environment_connections_still_load() {
        let env = vars(&[("SQLLY_CONNECTION_CACHE", "redis://h")]);
        for text in [
            "{ not json",
            "[]",
            r#"{ "format": "other" }"#,
            r#"{ "version": 9 }"#,
        ] {
            let out = file(text, &env);
            assert_eq!(out.warnings.len(), 1, "{text}: {:?}", out.warnings);
            assert_eq!(out.connections.len(), 1);
        }
    }

    #[test]
    fn duplicate_names_keep_the_first_and_ids_are_stable() {
        let env = vars(&[("SQLLY_CONNECTION_Cache", "redis://env-host")]);
        let text = r#"{ "connections": [ { "name": "cache", "url": "redis://file-host" } ] }"#;
        let first = file(text, &env);
        assert_eq!(first.connections.len(), 1);
        assert_eq!(first.connections[0].profile.server, "file-host");
        assert!(
            first.warnings[0].contains("already named"),
            "{:?}",
            first.warnings
        );
        let again = file(text, &env);
        assert_eq!(
            first.connections[0].profile.id,
            again.connections[0].profile.id
        );
        assert_ne!(first.connections[0].profile.id.0, Uuid::nil());
    }

    #[test]
    fn credential_document_is_a_valid_connection_file() {
        let out = provision(
            None,
            &vars(&[
                ("PW", "p"),
                ("SQLLY_CONNECTION_A", "redis+sentinel://:${PW}@s1,s2/m"),
                (
                    "SQLLY_CONNECTION_B",
                    "postgres://u:${PW}@h/db?sslmode=require",
                ),
            ]),
        );
        assert!(out.warnings.is_empty(), "{:?}", out.warnings);
        let text = out.credential_document().unwrap().to_string();
        let plan = parse_credential_file(&text, crate::credential_proxy::default_port).unwrap();
        assert_eq!(plan.connections.len(), 2);
        assert_eq!(
            plan.connections[0]
                .profile
                .authentication
                .secret_ref()
                .unwrap()
                .key,
            "p"
        );
        assert!(Provisioning::default().credential_document().is_none());
    }

    #[test]
    fn expansion_rules() {
        let env = vars(&[("A", "1"), ("B_2", "two")]);
        assert_eq!(expand_env("x${A}y${B_2}", &env).unwrap(), "x1ytwo");
        assert_eq!(
            expand_env("$${A} $5 ${ } ${", &env).unwrap(),
            "${A} $5 ${ } ${"
        );
        assert!(
            expand_env("${MISSING}", &env)
                .unwrap_err()
                .contains("MISSING")
        );
        assert_eq!(whole_reference(" ${A} ").as_deref(), Some("A"));
        assert_eq!(whole_reference("p${A}"), None);
    }

    #[test]
    fn file_path_prefers_the_variable() {
        let default = PathBuf::from("/cfg/SQLLY/provisioned-connections.json");
        assert_eq!(
            provisioned_file_path(
                &vars(&[(PROVISIONED_FILE_ENV, "/etc/sqlly.json")]),
                Some(default.clone())
            ),
            Some((PathBuf::from("/etc/sqlly.json"), true))
        );
        assert_eq!(
            provisioned_file_path(&vars(&[]), Some(default.clone())),
            Some((default, false))
        );
    }
}
