//! Keeps every secret out of what iCloud sync writes, and keeps this Mac's
//! own keychain references when a synced item comes back in.
//!
//! A saved connection or tunnel profile holds its passwords, SSH key
//! passphrases and tokens as secret references (`{"provider": …, "key": …}`)
//! into the OS keychain. A keychain reference names an entry only this Mac
//! has, and an in-memory reference carries the clear-text value itself, so
//! neither may leave the Mac. Sync works on the serialized JSON: every
//! object shaped like a secret reference, wherever it sits (connection
//! sign-in, proxy, SSH target and jump host, Redis Sentinel, and any slot a
//! later version adds), is replaced by an empty in-memory reference, which
//! the app treats as "ask for the password on connect".

use serde_json::{Map, Value};

/// The `provider` values a secret reference can carry.
const SECRET_PROVIDERS: &[&str] = &["MacOsKeychain", "InMemory", "Environment"];

/// An empty in-memory reference: no secret, asked for on first connect.
pub fn placeholder() -> Value {
    let mut map = Map::new();
    map.insert("provider".into(), Value::String("InMemory".into()));
    map.insert("key".into(), Value::String(String::new()));
    Value::Object(map)
}

/// Whether `value` is a serialized secret reference.
pub fn is_secret_ref(value: &Value) -> bool {
    let Value::Object(map) = value else {
        return false;
    };
    map.len() == 2
        && map.get("key").is_some_and(Value::is_string)
        && map
            .get("provider")
            .and_then(Value::as_str)
            .is_some_and(|provider| SECRET_PROVIDERS.contains(&provider))
}

/// Replace every secret reference inside `value` with [`placeholder`].
pub fn scrub(value: &mut Value) {
    if is_secret_ref(value) {
        *value = placeholder();
        return;
    }
    match value {
        Value::Object(map) => map.values_mut().for_each(scrub),
        Value::Array(items) => items.iter_mut().for_each(scrub),
        _ => {}
    }
}

/// Whether `value` holds no secret: every secret reference in it is empty.
pub fn is_secret_free(value: &Value) -> bool {
    if is_secret_ref(value) {
        return value.get("key").and_then(Value::as_str) == Some("")
            && value.get("provider").and_then(Value::as_str) == Some("InMemory");
    }
    match value {
        Value::Object(map) => map.values().all(is_secret_free),
        Value::Array(items) => items.iter().all(is_secret_free),
        _ => true,
    }
}

/// Copy this Mac's secret references from `local` into `incoming` wherever
/// both have one at the same place, so applying a synced connection never
/// detaches the password this Mac already saved for it. Places only the
/// incoming copy has keep the placeholder (asked for on connect).
pub fn restore_local(incoming: &mut Value, local: &Value) {
    if is_secret_ref(incoming) {
        if is_secret_ref(local) {
            *incoming = local.clone();
        }
        return;
    }
    match (incoming, local) {
        (Value::Object(map), Value::Object(local_map)) => {
            for (key, child) in map.iter_mut() {
                if let Some(local_child) = local_map.get(key) {
                    restore_local(child, local_child);
                }
            }
        }
        (Value::Array(items), Value::Array(local_items)) => {
            for (child, local_child) in items.iter_mut().zip(local_items) {
                restore_local(child, local_child);
            }
        }
        _ => {}
    }
}
