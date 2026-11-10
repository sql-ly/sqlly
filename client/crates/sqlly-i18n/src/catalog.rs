use std::collections::{BTreeMap, BTreeSet};

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

#[derive(Debug)]
enum JsonValue {
    String(String),
    Object(BTreeMap<String, JsonValue>),
    Other,
}

impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(JsonValueVisitor)
    }
}

struct JsonValueVisitor;

impl<'de> Visitor<'de> for JsonValueVisitor {
    type Value = JsonValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(JsonValue::Other)
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(JsonValue::Other)
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(JsonValue::Other)
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(JsonValue::Other)
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(JsonValue::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(JsonValue::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Other)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(JsonValue::Other)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element::<JsonValue>()?.is_some() {}
        Ok(JsonValue::Other)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries = BTreeMap::new();
        while let Some((key, value)) = map.next_entry::<String, JsonValue>()? {
            if entries.insert(key.clone(), value).is_some() {
                return Err(de::Error::custom(format!("duplicate object key '{key}'")));
            }
        }
        Ok(JsonValue::Object(entries))
    }
}

pub(crate) fn parse(json: &str) -> Result<BTreeMap<String, String>, String> {
    let root: JsonValue =
        serde_json::from_str(json).map_err(|error| format!("invalid translation JSON: {error}"))?;
    let mut entries = BTreeMap::new();
    flatten(&root, "", &mut entries)?;
    if entries.is_empty() {
        return Err("the catalog must contain at least one translation".into());
    }
    Ok(entries)
}

/// Named `{placeholder}` arguments a catalog value expects, using the same
/// grammar as `format`: `{{` and `}}` are literal braces, and a placeholder is
/// a snake_case ASCII name. Any other brace text (for example a JSON sample
/// shown through `text`) is ordinary prose and contributes nothing, so this
/// compares translations without rejecting values that never get formatted.
pub(crate) fn placeholder_names(value: &str) -> BTreeSet<&str> {
    let mut names = BTreeSet::new();
    let mut remaining = value;
    while let Some(index) = remaining.find(['{', '}']) {
        remaining = &remaining[index..];
        if remaining.starts_with("{{") || remaining.starts_with("}}") {
            remaining = &remaining[2..];
            continue;
        }
        if remaining.starts_with('{')
            && let Some(end) = remaining[1..].find(['{', '}'])
            && remaining.as_bytes()[end + 1] == b'}'
        {
            let name = &remaining[1..=end];
            if is_snake_case(name) {
                names.insert(name);
                remaining = &remaining[end + 2..];
                continue;
            }
        }
        remaining = &remaining[1..];
    }
    names
}

/// Describe how `translated` changes the placeholder set of `source`, or
/// `None` when both expect exactly the same named arguments.
pub(crate) fn placeholder_mismatch(source: &str, translated: &str) -> Option<String> {
    let expected = placeholder_names(source);
    // Literal-brace documentation is plain text. A source with named arguments
    // and valid format syntax establishes the format contract for its overrides.
    if !expected.is_empty() && valid_format_syntax(source) && !valid_format_syntax(translated) {
        return Some("invalid format syntax; escape literal braces as {{ and }}".into());
    }
    let found = placeholder_names(translated);
    if expected == found {
        return None;
    }
    let missing = expected.difference(&found).collect::<Vec<_>>();
    let extra = found.difference(&expected).collect::<Vec<_>>();
    Some(format!(
        "placeholders must match English {expected:?}; missing={missing:?}, extra={extra:?}"
    ))
}

pub(crate) fn valid_format_syntax(mut value: &str) -> bool {
    while let Some(index) = value.find(['{', '}']) {
        value = &value[index..];
        if value.starts_with("{{") || value.starts_with("}}") {
            value = &value[2..];
        } else if value.starts_with('{') {
            let Some(end) = value.find('}') else {
                return false;
            };
            if !is_snake_case(&value[1..end]) {
                return false;
            }
            value = &value[end + 1..];
        } else {
            return false;
        }
    }
    true
}

fn is_snake_case(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        && !name.ends_with('_')
        && !name.contains("__")
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn flatten(
    node: &JsonValue,
    prefix: &str,
    entries: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let JsonValue::Object(object) = node else {
        return Err(format!("'{}' must be an object", display_key(prefix)));
    };

    if object.contains_key("value") || object.contains_key("location") {
        if prefix.is_empty() {
            return Err("the catalog root must contain screen or menu groups".into());
        }
        if object.len() != 2 || !object.contains_key("value") || !object.contains_key("location") {
            return Err(format!(
                "'{prefix}' must contain exactly the string fields 'value' and 'location'"
            ));
        }
        let value = string_field(object.get("value"), prefix, "value")?;
        let location = string_field(object.get("location"), prefix, "location")?;
        if value.trim().is_empty() {
            return Err(format!("'{prefix}.value' cannot be empty"));
        }
        validate_location(prefix, location)?;
        entries.insert(prefix.to_owned(), value.to_owned());
        return Ok(());
    }

    if object.is_empty() {
        return Err(format!(
            "'{}' cannot be an empty group",
            display_key(prefix)
        ));
    }
    for (segment, child) in object {
        validate_segment(segment)
            .map_err(|error| format!("'{}.{segment}': {error}", display_key(prefix)))?;
        let key = if prefix.is_empty() {
            segment.clone()
        } else {
            format!("{prefix}.{segment}")
        };
        flatten(child, &key, entries)?;
    }
    Ok(())
}

fn string_field<'a>(
    field: Option<&'a JsonValue>,
    prefix: &str,
    name: &str,
) -> Result<&'a str, String> {
    match field {
        Some(JsonValue::String(value)) => Ok(value),
        _ => Err(format!("'{prefix}.{name}' must be a string")),
    }
}

fn display_key(prefix: &str) -> &str {
    if prefix.is_empty() { "root" } else { prefix }
}

fn validate_location(key: &str, location: &str) -> Result<(), String> {
    let location = location.trim();
    let Some((path, detail)) = location.split_once(':') else {
        return Err(format!(
            "'{key}.location' must name the full UI path followed by the precise control or message"
        ));
    };
    if location.chars().count() < 40
        || !path.contains(" → ")
        || path.split(" → ").any(|part| part.trim().is_empty())
        || detail.trim().chars().count() < 12
        || location == key
    {
        return Err(format!(
            "'{key}.location' must describe the full UI path with → and at least 12 characters of control or message context"
        ));
    }
    Ok(())
}

fn validate_segment(segment: &str) -> Result<(), &'static str> {
    let mut bytes = segment.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        || segment.ends_with('_')
        || segment.contains("__")
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(
            "key segments must be snake_case ASCII beginning with a lowercase letter and without trailing or repeated underscores",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse, placeholder_mismatch, placeholder_names};

    fn catalog(value: &str, location: &str) -> String {
        format!(
            r#"{{"settings":{{"appearance":{{"title":{{"value":{value},"location":{location}}}}}}}}}"#
        )
    }

    const LOCATION: &str =
        r#""Settings → Appearance: descriptive title for the appearance preferences pane.""#;

    #[test]
    fn flattens_nested_entries_and_discards_locations() {
        let entries = parse(&catalog(r#""Appearance""#, LOCATION)).unwrap();
        assert_eq!(
            entries.get("settings.appearance.title").map(String::as_str),
            Some("Appearance")
        );
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn rejects_duplicate_keys_at_every_object_depth() {
        let duplicate_leaf = r#"{"settings":{"title":{"value":"One","value":"Two","location":"Settings → General: title for the duplicate key validation test."}}}"#;
        let duplicate_group = r#"{"settings":{"title":{"value":"One","location":"Settings → General: first title for duplicate group testing."}},"settings":{"title":{"value":"Two","location":"Settings → General: second title for duplicate group testing."}}}"#;
        let duplicate_location = r#"{"settings":{"title":{"value":"One","location":"Settings → General: first location for duplicate location testing.","location":"Settings → General: second location for duplicate location testing."}}}"#;
        for json in [duplicate_leaf, duplicate_group, duplicate_location] {
            assert!(
                parse(json).unwrap_err().contains("duplicate object key"),
                "{json}"
            );
        }
    }

    #[test]
    fn rejects_malformed_json_and_non_object_roots() {
        for json in ["not json", "[]", "null", r#""text""#] {
            assert!(parse(json).is_err(), "{json}");
        }
    }

    #[test]
    fn rejects_invalid_key_segments() {
        for segment in [
            "Settings",
            "settings-appearance",
            "settings.appearance",
            "_settings",
            "settings_",
            "settings__appearance",
            "1settings",
            "",
        ] {
            let json = format!(
                r#"{{"{segment}":{{"title":{{"value":"Title","location":"Settings → General: title for key segment validation testing."}}}}}}"#
            );
            assert!(
                parse(&json).is_err(),
                "segment {segment:?} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_inexact_or_mistyped_leaf_fields() {
        let cases = [
            r#"{"settings":{"title":{"value":"Title"}}}"#,
            r#"{"settings":{"title":{"location":"Settings → General: descriptive title without a value field."}}}"#,
            r#"{"settings":{"title":{"value":"Title","location":"Settings → General: descriptive title with an extra field.","note":"Extra"}}}"#,
            r#"{"settings":{"title":{"value":1,"location":"Settings → General: descriptive title with a numeric value."}}}"#,
            r#"{"settings":{"title":{"value":"Title","location":false}}}"#,
            r#"{"value":"Title","location":"Settings → General: invalid translation at the catalog root."}"#,
        ];
        for json in cases {
            assert!(parse(json).is_err(), "{json}");
        }
    }

    #[test]
    fn rejects_empty_values_groups_and_catalogs() {
        for value in [r#"""""#, r#""   ""#] {
            assert!(parse(&catalog(value, LOCATION)).is_err());
        }
        for json in ["{}", r#"{"settings":{}}"#] {
            assert!(parse(json).is_err());
        }
    }

    #[test]
    fn rejects_inadequate_location_descriptions() {
        let locations = [
            r#""button""#,
            r#""settings""#,
            r#""client/apps/gpui/src/settings.rs:42""#,
            r#""settings.appearance.title""#,
            r#""Settings → Appearance: short""#,
            r#""Settings Appearance: this description omits the required surface path separators.""#,
            r#""Settings → Appearance""#,
        ];
        for location in locations {
            assert!(
                parse(&catalog(r#""Title""#, location)).is_err(),
                "{location}"
            );
        }
    }

    #[test]
    fn placeholder_names_follow_the_format_grammar() {
        let names = |value| placeholder_names(value).into_iter().collect::<Vec<_>>();
        assert_eq!(
            names("Deleted {count} rows from {table_name}; {count} again."),
            ["count", "table_name"]
        );
        assert_eq!(names("{{count}} = {count}"), ["count"]);
        assert_eq!(names("{{{count}}}"), ["count"]);
        assert!(names("No placeholders").is_empty());
        assert!(names(r#"Example: { "key": "value" }"#).is_empty());
        for prose in [
            "{Count}",
            "{count-name}",
            "{}",
            "{count",
            "count}",
            "{_count}",
            "{a__b}",
        ] {
            assert!(names(prose).is_empty(), "{prose}");
        }
        assert_eq!(names("{outer {inner}}"), ["inner"]);
        assert_eq!(names("Slide %{current} of %{total}"), ["current", "total"]);
    }

    #[test]
    fn placeholder_mismatch_compares_sets_not_order_or_repetition() {
        assert!(placeholder_mismatch("{a} of {b}", "{b} de {a} ({a})").is_none());
        assert!(placeholder_mismatch(r#"{ "k": 1 }"#, "texto sin llaves").is_none());
        let error = placeholder_mismatch("{count} rows", "{cuenta} filas").unwrap();
        assert!(error.contains(r#"missing=["count"]"#), "{error}");
        assert!(error.contains(r#"extra=["cuenta"]"#), "{error}");
        assert!(placeholder_mismatch("{count} rows", "filas").is_some());
        assert!(placeholder_mismatch("rows", "{count} filas").is_some());
    }
}
