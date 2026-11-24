//! How an edited JSON value is written back (DbGate #1477): minified,
//! pretty-printed, or exactly as typed.
//!
//! Reformatting is lexical: the text is first checked to be valid JSON, then
//! only the whitespace between tokens is rewritten. Strings, numbers, key
//! order and duplicate keys come through byte-for-byte, so a value never
//! loses precision or escapes by being edited.

use serde::{Deserialize, Serialize};

/// Settings ▸ Results ▸ General ▸ "Save edited JSON as".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum JsonEditStorage {
    /// Save the editor text unchanged (the behavior before this setting).
    #[default]
    KeepAsTyped,
    Minified,
    PrettyPrinted,
}

impl JsonEditStorage {
    pub const ALL: [JsonEditStorage; 3] = [
        JsonEditStorage::KeepAsTyped,
        JsonEditStorage::Minified,
        JsonEditStorage::PrettyPrinted,
    ];

    pub fn label(self) -> String {
        crate::localization::text(match self {
            JsonEditStorage::KeepAsTyped => "settings.results.json_edit_storage.keep_as_typed",
            JsonEditStorage::Minified => "settings.results.json_edit_storage.minified",
            JsonEditStorage::PrettyPrinted => "settings.results.json_edit_storage.pretty_printed",
        })
    }

    /// The saved setting.
    pub fn current() -> Self {
        crate::preferences::ResultFormattingPreferences::load().json_edit_storage
    }

    /// Whether the editor should open JSON values pretty-printed.
    pub fn edits_pretty(self) -> bool {
        self != JsonEditStorage::KeepAsTyped
    }

    /// The text to store for `text`. Invalid JSON, and every text under
    /// Keep as typed, is returned unchanged.
    pub fn apply(self, text: &str) -> String {
        match self {
            JsonEditStorage::KeepAsTyped => text.to_string(),
            JsonEditStorage::Minified => minify_json(text).unwrap_or_else(|| text.to_string()),
            JsonEditStorage::PrettyPrinted => pretty_json(text).unwrap_or_else(|| text.to_string()),
        }
    }
}

/// True for column types that hold JSON documents.
pub fn is_json_type(data_type: &str) -> bool {
    matches!(
        data_type.trim().to_ascii_lowercase().as_str(),
        "json" | "jsonb"
    )
}

fn is_valid_json(text: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

/// `text` with all insignificant whitespace removed, or `None` when it is
/// not valid JSON.
pub fn minify_json(text: &str) -> Option<String> {
    is_valid_json(text).then(|| reformat(text, false))
}

/// `text` indented two spaces per level, or `None` when it is not valid
/// JSON. Empty objects and arrays stay `{}` / `[]`.
pub fn pretty_json(text: &str) -> Option<String> {
    is_valid_json(text).then(|| reformat(text, true))
}

/// Rewrites the whitespace between tokens of already-validated JSON.
fn reformat(text: &str, pretty: bool) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut depth = 0usize;
    let newline = |out: &mut Vec<u8>, depth: usize| {
        out.push(b'\n');
        out.extend(std::iter::repeat_n(b' ', depth * 2));
    };
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                out.extend_from_slice(&bytes[start..i.min(bytes.len())]);
                continue;
            }
            b' ' | b'\t' | b'\n' | b'\r' => {}
            b'{' | b'[' => {
                let close = if b == b'{' { b'}' } else { b']' };
                let mut j = i + 1;
                while j < bytes.len() && matches!(bytes[j], b' ' | b'\t' | b'\n' | b'\r') {
                    j += 1;
                }
                out.push(b);
                if bytes.get(j) == Some(&close) {
                    out.push(close);
                    i = j + 1;
                    continue;
                }
                depth += 1;
                if pretty {
                    newline(&mut out, depth);
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if pretty {
                    newline(&mut out, depth);
                }
                out.push(b);
            }
            b',' => {
                out.push(b);
                if pretty {
                    newline(&mut out, depth);
                }
            }
            b':' => {
                out.push(b);
                if pretty {
                    out.push(b' ');
                }
            }
            _ => out.push(b),
        }
        i += 1;
    }
    // Only ASCII whitespace was dropped and ASCII punctuation added, so the
    // bytes are still the input's UTF-8.
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minify_strips_whitespace_and_keeps_order_numbers_and_strings() {
        let text = "{\n  \"z\": 1.50,\n  \"a\": [ 1e3 , 12345678901234567890123 ],\n  \"s\": \"a b\\\" , { }\"\n}";
        assert_eq!(
            minify_json(text).unwrap(),
            r#"{"z":1.50,"a":[1e3,12345678901234567890123],"s":"a b\" , { }"}"#
        );
    }

    #[test]
    fn pretty_indents_two_spaces_and_keeps_empty_containers() {
        let text = r#"{"b":{"x":[1,2]},"a":[],"e":{ },"u":"é\u00e9"}"#;
        assert_eq!(
            pretty_json(text).unwrap(),
            "{\n  \"b\": {\n    \"x\": [\n      1,\n      2\n    ]\n  },\n  \"a\": [],\n  \"e\": {},\n  \"u\": \"é\\u00e9\"\n}"
        );
    }

    #[test]
    fn pretty_then_minify_round_trips() {
        let text = r#"{"k":[true,null,{"n":-0.0}],"dup":1,"dup":2}"#;
        let pretty = pretty_json(text).unwrap();
        assert_eq!(minify_json(&pretty).unwrap(), text);
    }

    #[test]
    fn scalars_are_trimmed() {
        assert_eq!(minify_json("  42 \n").unwrap(), "42");
        assert_eq!(pretty_json(" \"x\" ").unwrap(), "\"x\"");
    }

    #[test]
    fn invalid_json_is_not_reformatted() {
        for bad in ["{\"a\": 1,}", "{ a: 1 }", "", "[1, 2", "{} {}"] {
            assert_eq!(minify_json(bad), None, "{bad}");
            assert_eq!(pretty_json(bad), None, "{bad}");
            for mode in JsonEditStorage::ALL {
                assert_eq!(mode.apply(bad), bad);
            }
        }
    }

    #[test]
    fn apply_follows_the_mode() {
        let text = "{ \"a\" : [1, 2] }";
        assert_eq!(JsonEditStorage::KeepAsTyped.apply(text), text);
        assert_eq!(JsonEditStorage::Minified.apply(text), r#"{"a":[1,2]}"#);
        assert_eq!(
            JsonEditStorage::PrettyPrinted.apply(text),
            "{\n  \"a\": [\n    1,\n    2\n  ]\n}"
        );
        assert!(!JsonEditStorage::KeepAsTyped.edits_pretty());
        assert!(JsonEditStorage::Minified.edits_pretty());
    }

    #[test]
    fn json_types() {
        assert!(is_json_type("json"));
        assert!(is_json_type("JSONB"));
        assert!(!is_json_type("text"));
        assert!(!is_json_type("nvarchar"));
    }

    #[test]
    fn preference_defaults_to_keep_as_typed_and_round_trips() {
        use crate::preferences::ResultFormattingPreferences;
        assert_eq!(JsonEditStorage::default(), JsonEditStorage::KeepAsTyped);
        assert_eq!(
            ResultFormattingPreferences::default().json_edit_storage,
            JsonEditStorage::KeepAsTyped
        );
        // A settings file written before the option existed.
        let old: ResultFormattingPreferences =
            serde_json::from_str(r#"{"exportsMasked":true}"#).unwrap();
        assert_eq!(old.json_edit_storage, JsonEditStorage::KeepAsTyped);
        let prefs = ResultFormattingPreferences {
            json_edit_storage: JsonEditStorage::Minified,
            ..Default::default()
        };
        let json = serde_json::to_string(&prefs).unwrap();
        assert!(json.contains(r#""jsonEditStorage":"minified""#), "{json}");
        let back: ResultFormattingPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back.json_edit_storage, JsonEditStorage::Minified);
        assert_eq!(
            serde_json::from_str::<JsonEditStorage>(r#""prettyPrinted""#).unwrap(),
            JsonEditStorage::PrettyPrinted
        );
    }
}
