//! Source-level guard for plan 305's direct, contextual translation contract.
//! This intentionally scans shipped Rust tokens instead of matching lines: raw
//! strings, comments, `format!` messages, and inline test-only items otherwise
//! make a source lint both noisy and easy to bypass.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Keys consumed through an indirect mechanism that a direct-call scan cannot
/// observe. Every entry must name the mechanism and why a direct key is
/// impossible; do not use this list to preserve unused catalog entries.
const INDIRECT_KEY_ALLOWLIST: &[(&str, &str)] = &[];
const EXTERNAL_KEY_PREFIX_ALLOWLIST: &[(&str, &str)] = &[(
    "datatable.",
    "sqlly-datatable consumes these keys inside its external crate through the translator installed by GPUI; the dependency, installer, and behavior test are required below",
)];
const PARAMETERIZED_HELPER_CONTRACTS: &[(&str, &str, &str, &str)] = &[(
    "src/snippets.rs",
    "quick_action_title",
    "settings.snippets.message.quick_action",
    "title",
)];

const UI_METHODS: &[&str] = &[
    "aria_label",
    "accessibility_label",
    "caption",
    "child",
    "description",
    "empty_message",
    "empty_state",
    "error_message",
    "label",
    "menu_title",
    "message",
    "notification",
    "notify_error",
    "notify_success",
    "placeholder",
    "prompt",
    "push_notification",
    "set_error",
    "set_status",
    "show_error",
    "show_message",
    "show_notification",
    "status_message",
    "subtitle",
    "tab_title",
    "text",
    "title",
    "toast",
    "tooltip",
    "window_title",
];
const UI_CONSTRUCTORS: &[&str] = &[
    "Menu",
    "MenuItem",
    "Notification",
    "PopupMenuItem",
    "Tab",
    "Toast",
];
const PROPER_NAMES: &[&str] = &[
    "Anthropic",
    "Azure",
    "CSV",
    "Docker",
    "Entra ID",
    "Excel",
    "Git",
    "GitHub",
    "HTML",
    "HTTP",
    "HTTPS",
    "JSON",
    "Kubernetes",
    "Linux",
    "MariaDB",
    "MongoDB",
    "MySQL",
    "Ollama",
    "OpenAI",
    "Oracle",
    "Parquet",
    "PostgreSQL",
    "Redis",
    "SQL",
    "SQL Server",
    "SQLite",
    "SQLLY",
    "TOML",
    "TSV",
    "WASM",
    "Windows",
    "XML",
    "YAML",
    "macOS",
];
const TECHNICAL_TOKENS: &[&str] = &[
    "ALTER",
    "ANALYZE",
    "BEGIN",
    "CALL",
    "COMMIT",
    "CREATE",
    "DELETE",
    "DESCRIBE",
    "DROP",
    "EXEC",
    "EXECUTE",
    "EXPLAIN",
    "GRANT",
    "INSERT",
    "MERGE",
    "NULL",
    "PRAGMA",
    "REVOKE",
    "ROLLBACK",
    "SELECT",
    "SET",
    "SHOW",
    "TRUNCATE",
    "UPDATE",
    "USE",
    "VACUUM",
    "WITH",
    "Esc",
    "NORECOMPUTE",
    "false",
    "seed",
    "step",
    "true",
];
const TECHNICAL_LITERALS: &[&str] = &[
    "#rrggbb",
    "(SPDX:)",
    "Aa",
    "DDL",
    "DISTINCT",
    "ErDiagramView",
    "GIT",
    "IDENTITY",
    "Markdown",
    "NewTable",
    "PK",
    "PROC",
    "col",
    "dbml",
    "fx",
    "hh:MM:ss",
    "identity",
    "id",
    "linux",
    "mermaid",
    "mt",
    "mymaster",
    "null",
    "pk",
    "redis",
    "sa",
    "stacked",
    "stdio",
    "svg",
    "tabular",
    "txt",
    "ubuntu",
    "username",
    "var",
];

#[derive(Clone, Debug)]
enum TokenKind {
    Ident(String),
    String(String),
    Symbol(char),
}

#[derive(Clone, Debug)]
struct Token {
    kind: TokenKind,
    line: usize,
}

impl Token {
    fn ident(&self, expected: &str) -> bool {
        matches!(&self.kind, TokenKind::Ident(value) if value == expected)
    }

    fn symbol(&self, expected: char) -> bool {
        matches!(self.kind, TokenKind::Symbol(value) if value == expected)
    }
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read GPUI source directory") {
        let path = entry.expect("GPUI source directory entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name == "tests") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs")
            && path.file_name().is_none_or(|name| name != "tests.rs")
        {
            out.push(path);
        }
    }
}

fn lex_rust(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    let mut line = 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\n' => {
                line += 1;
                at += 1;
            }
            b' ' | b'\t' | b'\r' => at += 1,
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at += 2;
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                let mut depth = 1;
                while at < bytes.len() && depth > 0 {
                    if bytes[at] == b'\n' {
                        line += 1;
                        at += 1;
                    } else if bytes.get(at..at + 2) == Some(b"/*") {
                        depth += 1;
                        at += 2;
                    } else if bytes.get(at..at + 2) == Some(b"*/") {
                        depth -= 1;
                        at += 2;
                    } else {
                        at += 1;
                    }
                }
            }
            b'r' if raw_string_start(bytes, at).is_some() => {
                let token_line = line;
                let (value, next, newlines) = read_raw_string(source, at);
                tokens.push(Token {
                    kind: TokenKind::String(value),
                    line: token_line,
                });
                line += newlines;
                at = next;
            }
            b'"' => {
                let token_line = line;
                let (value, next, newlines) = read_string(source, at);
                tokens.push(Token {
                    kind: TokenKind::String(value),
                    line: token_line,
                });
                line += newlines;
                at = next;
            }
            b'\'' => {
                if let Some((next, newlines)) = skip_char_or_lifetime(bytes, at) {
                    line += newlines;
                    at = next;
                } else {
                    at += 1;
                }
            }
            byte if byte == b'_' || byte.is_ascii_alphabetic() || byte >= 0x80 => {
                let start = at;
                at += 1;
                while at < bytes.len()
                    && (bytes[at] == b'_' || bytes[at].is_ascii_alphanumeric() || bytes[at] >= 0x80)
                {
                    at += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Ident(source[start..at].to_string()),
                    line,
                });
            }
            byte => {
                tokens.push(Token {
                    kind: TokenKind::Symbol(char::from(byte)),
                    line,
                });
                at += 1;
            }
        }
    }
    tokens
}

fn raw_string_start(bytes: &[u8], at: usize) -> Option<usize> {
    let mut cursor = at + 1;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'"')).then_some(cursor - at - 1)
}

fn read_raw_string(source: &str, at: usize) -> (String, usize, usize) {
    let bytes = source.as_bytes();
    let hashes = raw_string_start(bytes, at).expect("raw string start");
    let value_start = at + hashes + 2;
    let mut cursor = value_start;
    loop {
        assert!(cursor < bytes.len(), "unterminated raw Rust string");
        if bytes[cursor] == b'"'
            && (0..hashes).all(|offset| bytes.get(cursor + 1 + offset) == Some(&b'#'))
        {
            let value = source[value_start..cursor].to_string();
            let next = cursor + hashes + 1;
            let newlines = bytes[value_start..cursor]
                .iter()
                .filter(|byte| **byte == b'\n')
                .count();
            return (value, next, newlines);
        }
        cursor += 1;
    }
}

fn read_string(source: &str, at: usize) -> (String, usize, usize) {
    let bytes = source.as_bytes();
    let mut value = String::new();
    let mut cursor = at + 1;
    let mut newlines = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'"' => return (value, cursor + 1, newlines),
            b'\n' => {
                newlines += 1;
                value.push('\n');
                cursor += 1;
            }
            b'\\' => {
                cursor += 1;
                assert!(cursor < bytes.len(), "unterminated Rust string escape");
                let escaped = bytes[cursor];
                match escaped {
                    b'n' => value.push('\n'),
                    b'r' => value.push('\r'),
                    b't' => value.push('\t'),
                    b'0' => value.push('\0'),
                    b'\n' => newlines += 1,
                    b'u' if bytes.get(cursor + 1) == Some(&b'{') => {
                        let end = (cursor + 2..bytes.len())
                            .find(|at| bytes[*at] == b'}')
                            .expect("terminated Unicode escape");
                        let code = u32::from_str_radix(&source[cursor + 2..end], 16)
                            .expect("hexadecimal Unicode escape");
                        value.push(char::from_u32(code).expect("valid Unicode escape"));
                        cursor = end;
                    }
                    other => value.push(char::from(other)),
                }
                cursor += 1;
            }
            byte if byte.is_ascii() => {
                value.push(char::from(byte));
                cursor += 1;
            }
            _ => {
                let ch = source[cursor..].chars().next().expect("UTF-8 character");
                value.push(ch);
                cursor += ch.len_utf8();
            }
        }
    }
    panic!("unterminated Rust string")
}

fn skip_char_or_lifetime(bytes: &[u8], at: usize) -> Option<(usize, usize)> {
    let next = *bytes.get(at + 1)?;
    if (next == b'_' || next.is_ascii_alphabetic()) && bytes.get(at + 2) != Some(&b'\'') {
        let mut cursor = at + 2;
        while bytes
            .get(cursor)
            .is_some_and(|byte| *byte == b'_' || byte.is_ascii_alphanumeric())
        {
            cursor += 1;
        }
        return Some((cursor, 0));
    }
    let mut cursor = at + 1;
    let mut newlines = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' => return Some((cursor + 1, newlines)),
            b'\n' => {
                newlines += 1;
                cursor += 1;
            }
            b'\\' => cursor += 2,
            _ => cursor += 1,
        }
    }
    None
}

fn cfg_attribute_end(tokens: &[Token], at: usize) -> usize {
    let mut depth = 0;
    for (index, token) in tokens.iter().enumerate().skip(at) {
        if token.symbol('[') {
            depth += 1;
        } else if token.symbol(']') {
            depth -= 1;
            if depth == 0 {
                return index + 1;
            }
        }
    }
    tokens.len()
}

fn shipped_tokens(tokens: &[Token]) -> Vec<Token> {
    let mut shipped = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        if is_cfg_test_attribute(tokens, at) {
            at = test_item_end(tokens, cfg_attribute_end(tokens, at));
        } else {
            shipped.push(tokens[at].clone());
            at += 1;
        }
    }
    shipped
}

fn is_cfg_test_attribute(tokens: &[Token], at: usize) -> bool {
    if ![(0, '#'), (1, '['), (3, '(')]
        .into_iter()
        .all(|(offset, symbol)| {
            tokens
                .get(at + offset)
                .is_some_and(|token| token.symbol(symbol))
        })
        || !tokens.get(at + 2).is_some_and(|token| token.ident("cfg"))
    {
        return false;
    }
    if tokens.get(at + 4).is_some_and(|token| token.ident("test"))
        && tokens.get(at + 5).is_some_and(|token| token.symbol(')'))
    {
        return true;
    }
    if !(tokens.get(at + 4).is_some_and(|token| token.ident("all"))
        && tokens.get(at + 5).is_some_and(|token| token.symbol('(')))
    {
        return false;
    }
    let mut depth = 0;
    for token in tokens.iter().skip(at + 6) {
        if token.symbol('(') {
            depth += 1;
        } else if token.symbol(')') {
            if depth == 0 {
                return false;
            }
            depth -= 1;
        } else if depth == 0 && token.ident("test") {
            return true;
        } else if depth == 0 && token.symbol(']') {
            return false;
        }
    }
    false
}

fn test_item_end(tokens: &[Token], mut at: usize) -> usize {
    let mut round = 0;
    let mut square = 0;
    while at < tokens.len() {
        if round == 0 && square == 0 && (tokens[at].symbol(';') || tokens[at].symbol(',')) {
            return at + 1;
        }
        if round == 0 && square == 0 && tokens[at].symbol('{') {
            let mut depth = 1;
            at += 1;
            while at < tokens.len() && depth > 0 {
                if tokens[at].symbol('{') {
                    depth += 1;
                } else if tokens[at].symbol('}') {
                    depth -= 1;
                }
                at += 1;
            }
            return at;
        }
        if tokens[at].symbol('(') {
            round += 1;
        } else if tokens[at].symbol(')') {
            round -= 1;
        } else if tokens[at].symbol('[') {
            square += 1;
        } else if tokens[at].symbol(']') {
            square -= 1;
        }
        at += 1;
    }
    at
}

fn catalog_keys() -> BTreeSet<String> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/sqlly-i18n/translations/en.json");
    let json = std::fs::read_to_string(path)
        .expect("read authoritative English catalog")
        .replace("\r\n", "\n");
    let catalog: serde_json::Value =
        serde_json::from_str(&json).expect("parse authoritative English catalog");
    let mut keys = BTreeSet::new();
    flatten_catalog(&catalog, "", &mut keys);
    keys
}

fn flatten_catalog(value: &serde_json::Value, prefix: &str, keys: &mut BTreeSet<String>) {
    let object = value.as_object().expect("catalog groups must be objects");
    for (segment, child) in object {
        let key = if prefix.is_empty() {
            segment.clone()
        } else {
            format!("{prefix}.{segment}")
        };
        if child.get("value").is_some() {
            keys.insert(key);
        } else {
            flatten_catalog(child, &key, keys);
        }
    }
}

fn catalog_locations(
    value: &serde_json::Value,
    prefix: &str,
    locations: &mut BTreeMap<String, Vec<String>>,
) {
    let object = value.as_object().expect("catalog groups must be objects");
    for (segment, child) in object {
        let key = if prefix.is_empty() {
            segment.clone()
        } else {
            format!("{prefix}.{segment}")
        };
        match child.get("location").and_then(serde_json::Value::as_str) {
            Some(location) if child.get("value").is_some() => locations
                .entry(location.trim().to_string())
                .or_default()
                .push(key),
            _ => catalog_locations(child, &key, locations),
        }
    }
}

/// Translators work from the location alone, so two keys described by the
/// same location are indistinguishable: each must say which control or
/// message it is.
#[test]
fn english_location_descriptions_are_unique() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/sqlly-i18n/translations/en.json");
    let json = std::fs::read_to_string(path).expect("read authoritative English catalog");
    let catalog: serde_json::Value =
        serde_json::from_str(&json).expect("parse authoritative English catalog");
    let mut locations = BTreeMap::new();
    catalog_locations(&catalog, "", &mut locations);
    let duplicates = locations
        .into_iter()
        .filter(|(_, keys)| keys.len() > 1)
        .map(|(location, keys)| format!("{location}\n    {}", keys.join("\n    ")))
        .collect::<Vec<_>>();
    assert!(
        duplicates.is_empty(),
        "{} location descriptions are shared by more than one key:\n{}",
        duplicates.len(),
        duplicates.join("\n")
    );
}

fn relative_path(path: &Path) -> String {
    path.strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[derive(Clone)]
struct TranslationCall {
    keys: BTreeSet<String>,
    location: String,
    token_at: usize,
    is_format: bool,
    fallback: bool,
    /// A keyed helper (`deferred`, `try_format`, `input_placeholder`,
    /// `DisplayMessage::new`, …) checked only for key existence, not for one
    /// semantic call site per key.
    existence_only: bool,
}

/// `localization::` helpers besides `text`/`format` that take a catalog key.
const KEYED_LOCALIZATION_HELPERS: &[&str] = &[
    "try_format",
    "deferred",
    "deferred_text",
    "input_placeholder",
];

/// Names a file imports from `localization` (`use crate::localization::{a, b}`
/// or `use crate::localization::a`) and may then call bare.
fn imported_localization_names(tokens: &[Token]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for at in 0..tokens.len().saturating_sub(3) {
        if !tokens[at].ident("localization")
            || !tokens[at + 1].symbol(':')
            || !tokens[at + 2].symbol(':')
            || !(0..at)
                .rev()
                .take(8)
                .any(|before| tokens[before].ident("use"))
        {
            continue;
        }
        let mut index = at + 3;
        let braced = tokens[index].symbol('{');
        if braced {
            index += 1;
        }
        while let Some(token) = tokens.get(index) {
            match &token.kind {
                TokenKind::Ident(name) => {
                    names.insert(name.clone());
                }
                _ if token.symbol(',') => {}
                _ => break,
            }
            index += 1;
            if !braced {
                break;
            }
        }
    }
    names
}

fn local_key_bindings(tokens: &[Token], include_let: bool) -> BTreeMap<String, BTreeSet<String>> {
    let mut bindings: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for at in 0..tokens.len() {
        if !(include_let && tokens[at].ident("let") || tokens[at].ident("const")) {
            continue;
        }
        let Some(equals) = (at + 1..tokens.len()).find(|index| tokens[*index].symbol('=')) else {
            continue;
        };
        if tokens[at + 1..equals]
            .iter()
            .any(|token| token.symbol(';') || token.symbol('{') || token.symbol('}'))
        {
            continue;
        }
        let names: Vec<String> = tokens[at + 1..equals]
            .iter()
            .filter_map(|token| match &token.kind {
                TokenKind::Ident(name) if name != "mut" => Some(name.clone()),
                _ => None,
            })
            .collect();
        let end = (equals + 1..tokens.len())
            .find(|index| tokens[*index].symbol(';'))
            .unwrap_or(tokens.len());
        let values: BTreeSet<String> = tokens[equals + 1..end]
            .iter()
            .filter_map(|token| match &token.kind {
                TokenKind::String(value) if looks_like_catalog_key(value) => Some(value.clone()),
                _ => None,
            })
            .collect();
        for name in names {
            bindings.entry(name).or_default().extend(values.clone());
        }
    }
    bindings
}

fn macro_definitions(tokens: &[Token]) -> (BTreeSet<String>, Vec<(usize, usize)>) {
    let mut names = BTreeSet::new();
    let mut ranges = Vec::new();
    for at in 0..tokens.len().saturating_sub(3) {
        if tokens[at].ident("macro_rules")
            && tokens[at + 1].symbol('!')
            && let TokenKind::Ident(name) = &tokens[at + 2].kind
            && tokens[at + 3].symbol('{')
        {
            let mut depth = 1;
            let mut end = at + 4;
            while end < tokens.len() && depth > 0 {
                if tokens[end].symbol('{') {
                    depth += 1;
                } else if tokens[end].symbol('}') {
                    depth -= 1;
                }
                end += 1;
            }
            names.insert(name.clone());
            ranges.push((at, end));
        }
    }
    (names, ranges)
}

fn indirect_helper_name(tokens: &[Token], call: usize) -> Option<String> {
    let argument = call + 5;
    let (name_at, name) = match tokens.get(argument).map(|token| &token.kind) {
        Some(TokenKind::Ident(name)) => (argument, name),
        Some(TokenKind::Symbol('&')) => match tokens.get(argument + 1).map(|token| &token.kind) {
            Some(TokenKind::Ident(name)) => (argument + 1, name),
            _ => return None,
        },
        _ => return None,
    };
    for at in (0..call).rev() {
        if !tokens[at].ident("fn") {
            continue;
        }
        let TokenKind::Ident(helper) = &tokens.get(at + 1)?.kind else {
            return None;
        };
        let open = (at + 2..call).find(|index| tokens[*index].symbol('('))?;
        let close = call_end(tokens, open);
        if tokens[open + 1..close.saturating_sub(1)]
            .iter()
            .any(|token| token.ident(name))
        {
            return Some(helper.clone());
        }
        break;
    }
    for at in (0..name_at).rev() {
        if !tokens[at].symbol('|') {
            continue;
        }
        let close = (at + 1..name_at).find(|index| tokens[*index].symbol('|'))?;
        if at >= 3
            && tokens[at - 3].ident("let")
            && tokens[at - 1].symbol('=')
            && tokens[at + 1..close].iter().any(|token| token.ident(name))
            && let TokenKind::Ident(helper) = &tokens[at - 2].kind
        {
            return Some(helper.clone());
        }
    }
    None
}

fn translation_calls(tokens: &[Token], relative: &str) -> (Vec<TranslationCall>, Vec<String>) {
    let constants = local_key_bindings(tokens, false);
    let (macros, _) = macro_definitions(tokens);
    let mut calls = Vec::new();
    let violations = Vec::new();
    let mut indirect_helpers = BTreeSet::new();
    let imported = imported_localization_names(tokens);
    for at in 0..tokens.len().saturating_sub(5) {
        let semantic = tokens[at].ident("localization")
            && tokens[at + 1].symbol(':')
            && tokens[at + 2].symbol(':')
            && (tokens[at + 3].ident("text") || tokens[at + 3].ident("format"))
            && tokens[at + 4].symbol('(');
        if !semantic {
            let path_helper = tokens[at + 1].symbol(':')
                && tokens[at + 2].symbol(':')
                && tokens[at + 4].symbol('(')
                && (tokens[at].ident("localization")
                    && KEYED_LOCALIZATION_HELPERS
                        .iter()
                        .any(|helper| tokens[at + 3].ident(helper))
                    || tokens[at].ident("DisplayMessage") && tokens[at + 3].ident("new"));
            let bare_import = matches!(&tokens[at].kind, TokenKind::Ident(name) if imported.contains(name))
                && tokens[at + 1].symbol('(')
                && (at == 0
                    || !(tokens[at - 1].symbol(':')
                        || tokens[at - 1].symbol('.')
                        || tokens[at - 1].ident("fn")));
            if path_helper || bare_import {
                let open = if path_helper { at + 4 } else { at + 1 };
                let end = call_end(tokens, open);
                let keys: BTreeSet<String> = match &tokens[open + 1].kind {
                    TokenKind::String(key) => BTreeSet::from([key.clone()]),
                    _ => tokens[open + 1..end.saturating_sub(1)]
                        .iter()
                        .filter_map(|token| match &token.kind {
                            TokenKind::String(value) if looks_like_catalog_key(value) => {
                                Some(value.clone())
                            }
                            _ => None,
                        })
                        .collect(),
                };
                if !keys.is_empty() {
                    calls.push(TranslationCall {
                        keys,
                        location: format!("{}:{}", relative, tokens[at].line),
                        token_at: at,
                        is_format: false,
                        fallback: false,
                        existence_only: true,
                    });
                }
            }
            continue;
        }
        let is_format = tokens[at + 3].ident("format");
        let end = call_end(tokens, at + 4);
        let helper = indirect_helper_name(tokens, at);
        if let Some(helper) = helper.as_ref() {
            indirect_helpers.insert(helper.clone());
        }
        let function_start = (0..at)
            .rev()
            .find(|index| tokens[*index].ident("fn"))
            .unwrap_or(0);
        let bindings = local_key_bindings(&tokens[function_start..at], true);
        let mut keys = BTreeSet::new();
        match &tokens[at + 5].kind {
            TokenKind::String(key) => {
                keys.insert(key.clone());
            }
            TokenKind::Ident(name) => {
                if let Some(values) = bindings.get(name).or_else(|| constants.get(name)) {
                    keys.extend(values.clone());
                }
            }
            _ => {}
        }
        if keys.is_empty() {
            keys = tokens[at + 5..end.saturating_sub(1)]
                .iter()
                .filter_map(|token| match &token.kind {
                    TokenKind::String(value) if looks_like_catalog_key(value) => {
                        Some(value.clone())
                    }
                    _ => None,
                })
                .collect();
        }
        if helper.is_some() {
            keys.clear();
        }
        if let Some((_, _, key, _)) =
            PARAMETERIZED_HELPER_CONTRACTS
                .iter()
                .find(|(file, function, _, _)| {
                    *file == relative && enclosing_function(tokens, at) == *function
                })
        {
            keys = BTreeSet::from([(*key).to_string()]);
        }
        if !keys.is_empty() {
            calls.push(TranslationCall {
                keys,
                location: format!("{}:{}", relative, tokens[at].line),
                token_at: at,
                is_format,
                fallback: false,
                existence_only: false,
            });
        }
    }
    for at in 0..tokens.len().saturating_sub(3) {
        if let TokenKind::Ident(name) = &tokens[at].kind
            && macros.contains(name)
            && tokens[at + 1].symbol('!')
            && tokens[at + 2].symbol('(')
            && let TokenKind::String(key) = &tokens[at + 3].kind
        {
            calls.push(TranslationCall {
                keys: BTreeSet::from([key.clone()]),
                location: format!("{}:{}", relative, tokens[at].line),
                token_at: at,
                is_format: false,
                fallback: false,
                existence_only: false,
            });
        }
    }
    for at in 0..tokens.len().saturating_sub(1) {
        if let TokenKind::Ident(name) = &tokens[at].kind
            && indirect_helpers.contains(name)
            && tokens[at + 1].symbol('(')
        {
            let end = call_end(tokens, at + 1);
            for (key_at, token) in tokens[at + 2..end.saturating_sub(1)].iter().enumerate() {
                if let TokenKind::String(key) = &token.kind
                    && looks_like_catalog_key(key)
                {
                    calls.push(TranslationCall {
                        keys: BTreeSet::from([key.clone()]),
                        location: format!("{}:{}", relative, token.line),
                        token_at: at + 2 + key_at,
                        is_format: false,
                        fallback: false,
                        existence_only: false,
                    });
                }
            }
        }
    }
    for at in 0..calls.len() {
        if calls[at].is_format || calls[at].existence_only {
            continue;
        }
        let fallback = calls.iter().enumerate().any(|(other, candidate)| {
            other != at
                && candidate.is_format
                && candidate.keys == calls[at].keys
                && candidate.token_at.abs_diff(calls[at].token_at) < 100
                && tokens[candidate.token_at.min(calls[at].token_at)
                    ..candidate.token_at.max(calls[at].token_at)]
                    .iter()
                    .any(|token| token.ident("unwrap_or_else"))
        });
        calls[at].fallback = fallback;
    }
    (calls, violations)
}

fn source_tokens() -> Vec<(String, Vec<Token>)> {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let source = std::fs::read_to_string(&path)
                .expect("read GPUI source")
                .replace("\r\n", "\n");
            (relative_path(&path), shipped_tokens(&lex_rust(&source)))
        })
        .collect()
}

type KeyUse = (String, String);

fn component_mapping_key_uses() -> Vec<KeyUse> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/localization.rs");
    let source = std::fs::read_to_string(&path)
        .expect("read component localization mapping")
        .replace("\r\n", "\n");
    let tokens = shipped_tokens(&lex_rust(&source));
    let start = tokens
        .iter()
        .position(|token| token.ident("COMPONENT_KEYS"))
        .expect("COMPONENT_KEYS mapping table");
    let end = (start..tokens.len())
        .find(|at| tokens[*at].symbol(';'))
        .expect("end of COMPONENT_KEYS mapping table");
    let uses: Vec<KeyUse> = tokens[start..end]
        .iter()
        .filter_map(|token| match &token.kind {
            TokenKind::String(key) if key.starts_with("components.") => {
                Some((key.clone(), format!("src/localization.rs:{}", token.line)))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        uses.len(),
        116,
        "the stock-component mapping must expose all 116 known keys to the coverage scan"
    );
    assert_eq!(
        uses.iter()
            .map(|(key, _)| key)
            .collect::<BTreeSet<_>>()
            .len(),
        uses.len(),
        "duplicate stock-component catalog mappings"
    );
    uses
}

fn browser_boot_key_uses() -> Vec<KeyUse> {
    let uses = javascript_key_uses("../browser/www/boot.js", "browser.boot.");
    assert!(uses.iter().any(|(key, _)| key == "browser.boot.preparing"));
    uses
}

/// The website's translation workbench lives outside the open-source tree.
/// Where its sources are absent (the public checkout), `None` tells the
/// caller to treat every `translation_manager.` key as used by it.
fn translation_manager_key_uses() -> Option<Vec<KeyUse>> {
    let website = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../server/Sqlly.Api");
    if !website.is_dir() {
        return None;
    }
    let uses = javascript_key_uses(
        "../../../server/Sqlly.Api/wwwroot/assets/pages/translations.js",
        "translation_manager.",
    );
    assert!(
        uses.iter()
            .any(|(key, _)| key == "translation_manager.stale_notice")
    );
    let page = razor_page_key_uses(
        "../../../server/Sqlly.Api/Pages/Translations.cshtml",
        "translation_manager.",
    );
    assert!(
        uses.iter()
            .chain(page.iter())
            .any(|(key, _)| key == "translation_manager.result_limit")
    );
    Some(uses.into_iter().chain(page).collect())
}

/// `@T("key")` markup calls (and plain `T("key")` calls inside the page's
/// `@{ }` code block): the helper prefixes each segment with the namespace
/// (`string T(string key) => Model.Messages["prefix" + key]`), so the
/// coverage scan must resolve them the same way.
fn razor_page_key_uses(relative: &str, prefix: &str) -> Vec<KeyUse> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let source = std::fs::read_to_string(&path)
        .expect("read translation workbench page")
        .replace("\r\n", "\n");
    let mut uses = Vec::new();
    let mut at = 0;
    while let Some(found) = source[at..].find("T(\"") {
        let call = at + found;
        let preceded_by_ident = call > 0
            && (source.as_bytes()[call - 1].is_ascii_alphanumeric()
                || source.as_bytes()[call - 1] == b'_');
        let start = call + 3;
        let Some(end) = source[start..].find('"') else {
            break;
        };
        if !preceded_by_ident {
            let line = source[..start].matches('\n').count() + 1;
            uses.push((
                format!("{prefix}{}", &source[start..start + end]),
                format!("{relative}:{line}"),
            ));
        }
        at = start + end;
    }
    assert!(
        uses.iter()
            .any(|(key, _)| key == "translation_manager.search_placeholder")
            && uses
                .iter()
                .any(|(key, _)| key == "translation_manager.page_title"),
        "the Razor page must expose its @T keys to the coverage scan"
    );
    uses
}

fn javascript_key_uses(relative: &str, prefix: &str) -> Vec<KeyUse> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let source = std::fs::read_to_string(&path)
        .expect("read browser boot script")
        .replace("\r\n", "\n");
    let bytes = source.as_bytes();
    let mut uses = Vec::new();
    let mut at = 0;
    let mut line = 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\n' => {
                line += 1;
                at += 1;
            }
            b'/' if bytes.get(at + 1) == Some(&b'/') => {
                at += 2;
                while at < bytes.len() && bytes[at] != b'\n' {
                    at += 1;
                }
            }
            b'/' if bytes.get(at + 1) == Some(&b'*') => {
                at += 2;
                while at < bytes.len() && bytes.get(at..at + 2) != Some(b"*/") {
                    if bytes[at] == b'\n' {
                        line += 1;
                    }
                    at += 1;
                }
                at = (at + 2).min(bytes.len());
            }
            quote @ (b'\'' | b'"' | b'`') => {
                let string_line = line;
                at += 1;
                let mut value = String::new();
                while at < bytes.len() && bytes[at] != quote {
                    match bytes[at] {
                        b'\\' => {
                            at += 2;
                        }
                        b'\n' => {
                            line += 1;
                            value.push('\n');
                            at += 1;
                        }
                        byte if byte.is_ascii() => {
                            value.push(char::from(byte));
                            at += 1;
                        }
                        _ => {
                            let ch = source[at..].chars().next().expect("UTF-8 character");
                            value.push(ch);
                            at += ch.len_utf8();
                        }
                    }
                }
                assert!(at < bytes.len(), "unterminated JavaScript string");
                at += 1;
                if value.starts_with(prefix) {
                    uses.push((value, format!("{relative}:{string_line}")));
                }
            }
            _ => at += 1,
        }
    }
    uses
}

fn command_palette_title_key(id: &str) -> String {
    let mut key = String::from("command_palette.command.");
    for (segment_index, segment) in id.split('.').enumerate() {
        if segment_index > 0 {
            key.push('.');
        }
        for (character_index, character) in segment.chars().enumerate() {
            if character.is_ascii_uppercase() {
                if character_index > 0 {
                    key.push('_');
                }
                key.push(character.to_ascii_lowercase());
            } else {
                key.push(character);
            }
        }
    }
    key
}

fn command_palette_key_uses() -> Vec<KeyUse> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/command_palette.rs");
    let source = std::fs::read_to_string(&path)
        .expect("read command palette registry")
        .replace("\r\n", "\n");
    assert!(
        source.contains("fn command_title_key(id: &str) -> String")
            && source.contains("String::from(\"command_palette.command.\")"),
        "command-palette derived-key scanner requires the command_title_key contract"
    );
    let tokens = shipped_tokens(&lex_rust(&source));
    let mut uses = Vec::new();
    for at in 0..tokens.len().saturating_sub(1) {
        if tokens[at].ident("register") && tokens[at + 1].symbol('(') {
            let end = call_end(&tokens, at + 1);
            for field in at + 2..end.saturating_sub(3) {
                if tokens[field].ident("id")
                    && tokens[field + 1].symbol(':')
                    && let TokenKind::String(id) = &tokens[field + 2].kind
                {
                    uses.push((
                        command_palette_title_key(id),
                        format!("src/command_palette.rs:{}", tokens[field + 2].line),
                    ));
                }
            }
        }
        if tokens[at].ident("more") && at > 0 && tokens[at - 1].ident("let") {
            let end = (at + 1..tokens.len())
                .find(|index| tokens[*index].symbol(';'))
                .unwrap_or(tokens.len());
            for entry in at + 1..end.saturating_sub(2) {
                if tokens[entry].symbol('(')
                    && let TokenKind::String(id) = &tokens[entry + 1].kind
                    && tokens[entry + 2].symbol(',')
                {
                    uses.push((
                        command_palette_title_key(id),
                        format!("src/command_palette.rs:{}", tokens[entry + 1].line),
                    ));
                }
            }
        }
    }
    assert!(
        uses.iter()
            .any(|(key, _)| key == "command_palette.command.file.new_query"),
        "command-palette scanner must find file.newQuery"
    );
    assert!(
        uses.iter()
            .any(|(key, _)| key == "command_palette.command.file.new_window"),
        "command-palette scanner must find the tuple-table file.newWindow entry"
    );
    let snippets =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/snippets.rs"))
            .expect("read parameterized quick-action title producer")
            .replace("\r\n", "\n");
    assert!(
        snippets.contains("pub fn quick_action_title(slot: u8, title: &str) -> String")
            && snippets.contains("settings.snippets.message.quick_action")
            && snippets.contains("(\"title\", title)")
            && source.contains("SnippetPreferences::quick_action_title(slot, &action.title)"),
        "quick-action titles must use the parameterized quick_action key with the title argument"
    );
    uses.push((
        "settings.snippets.message.quick_action".to_string(),
        "src/snippets.rs:quick_action_title".to_string(),
    ));
    let preferences = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/preferences_ui/snippets.rs"),
    )
    .expect("read parameterized quick-action slot label producer")
    .replace("\r\n", "\n");
    assert!(
        preferences.contains("settings.snippets.message.quick_action_slot")
            && preferences.contains("(\"slot\", &(action.slot).to_string())"),
        "quick-action slot labels must use quick_action_slot with the slot argument"
    );
    uses.push((
        "settings.snippets.message.quick_action_slot".to_string(),
        "src/preferences_ui/snippets.rs:quick_action_slot".to_string(),
    ));
    uses
}

fn run_label_key_uses() -> Vec<KeyUse> {
    let source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app/execute.rs"))
            .expect("read toolbar run-label producer")
            .replace("\r\n", "\n");
    assert!(
        source.contains("pub(crate) fn run_label(input: RunLabelInput)")
            && source.contains("crate::localization::text(&format!(\"{key}_label\"))")
            && source.contains("crate::localization::text(&format!(\"{key}_tooltip\"))"),
        "run-label scanner requires the dynamic _label and _tooltip suffix contract"
    );
    let mut uses = Vec::new();
    for variant in ["file", "selection", "statement", "procedure"] {
        let base = format!("toolbar.run.{variant}");
        assert!(
            source.contains(&format!("\"{base}\"")),
            "run-label variant `{base}` must remain an explicit run_label branch"
        );
        for suffix in ["label", "tooltip"] {
            uses.push((
                format!("{base}_{suffix}"),
                format!("src/app/execute.rs:run_label:{base}"),
            ));
        }
    }
    uses
}

fn service_descriptor_key_uses() -> Vec<KeyUse> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates");
    let mut files = Vec::new();
    rust_files(&crates, &mut files);
    files.sort();
    let mut uses = Vec::new();
    for path in files {
        let source = std::fs::read_to_string(&path)
            .expect("read service crate source")
            .replace("\r\n", "\n");
        let relative = path
            .strip_prefix(Path::new(env!("CARGO_MANIFEST_DIR")))
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        for token in shipped_tokens(&lex_rust(&source)) {
            if let TokenKind::String(key) = &token.kind
                && key.starts_with("services.")
            {
                uses.push((key.clone(), format!("{relative}:{}", token.line)));
            }
        }
    }
    assert!(
        uses.iter()
            .any(|(key, _)| key == "services.errors.connection"),
        "service descriptor scanner must find the core connection error key"
    );
    uses
}

fn datatable_contract_violations() -> Vec<String> {
    let localization =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/localization.rs"))
            .expect("read datatable localization installer")
            .replace("\r\n", "\n");
    let manifest =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .expect("read GPUI dependency manifest")
            .replace("\r\n", "\n");
    let source_requirements = [
        "sqlly_datatable::localization::install_translator",
        "fn datatable_translator_follows_language_changes_and_formats_messages()",
        "sqlly_datatable::localization::text(\"datatable.grid.search.no_matches\")",
    ];
    let mut violations = Vec::new();
    for required in source_requirements {
        if !localization.contains(required) {
            violations.push(format!(
                "datatable.* external allowance requires localization.rs contract `{required}`"
            ));
        }
    }
    if !manifest.contains("sqlly-datatable =") {
        violations.push(
            "datatable.* external allowance requires the sqlly-datatable dependency".to_string(),
        );
    }
    violations
}

#[test]
fn direct_translation_keys_are_known_and_catalog_keys_are_referenced() {
    let catalog = catalog_keys();
    let allowed: BTreeMap<&str, &str> = INDIRECT_KEY_ALLOWLIST.iter().copied().collect();
    assert_eq!(
        allowed.len(),
        INDIRECT_KEY_ALLOWLIST.len(),
        "duplicate indirect translation allowlist keys"
    );

    let mut referenced = BTreeSet::new();
    let mut violations = Vec::new();
    for (relative, tokens) in source_tokens() {
        for token in &tokens {
            if let TokenKind::String(key) = &token.kind
                && catalog.contains(key)
            {
                referenced.insert(key.clone());
            }
        }
        let (calls, mut call_violations) = translation_calls(&tokens, &relative);
        violations.append(&mut call_violations);
        for call in calls {
            for key in call.keys {
                referenced.insert(key.clone());
                if !catalog.contains(&key) {
                    violations.push(format!("{}: unknown catalog key `{key}`", call.location));
                }
            }
        }
    }

    let translation_manager_uses = translation_manager_key_uses().unwrap_or_else(|| {
        catalog
            .iter()
            .filter(|key| key.starts_with("translation_manager."))
            .map(|key| (key.clone(), "website translation workbench".to_string()))
            .collect()
    });
    let indirect_uses = component_mapping_key_uses()
        .into_iter()
        .chain(browser_boot_key_uses())
        .chain(translation_manager_uses)
        .chain(command_palette_key_uses())
        .chain(run_label_key_uses())
        .chain(service_descriptor_key_uses());
    for (key, location) in indirect_uses {
        referenced.insert(key.clone());
        if !catalog.contains(&key) {
            violations.push(format!("{location}: unknown catalog key `{key}`"));
        }
    }
    violations.extend(datatable_contract_violations());

    for key in catalog.difference(&referenced) {
        if let Some(justification) = allowed.get(key.as_str()) {
            assert!(
                !justification.trim().is_empty(),
                "indirect key `{key}` needs a justification"
            );
        } else if let Some((_, justification)) = EXTERNAL_KEY_PREFIX_ALLOWLIST
            .iter()
            .find(|(prefix, _)| key.starts_with(prefix))
        {
            assert!(
                !justification.trim().is_empty(),
                "external key prefix for `{key}` needs a justification"
            );
        } else {
            violations.push(format!("unreferenced catalog key `{key}`"));
        }
    }
    for key in allowed.keys() {
        if !catalog.contains(*key) {
            violations.push(format!(
                "stale indirect allowlist key `{key}` (not in catalog)"
            ));
        } else if referenced.contains(*key) {
            violations.push(format!(
                "stale indirect allowlist key `{key}` (directly referenced)"
            ));
        }
    }

    violations.sort();
    violations.dedup();
    assert!(
        violations.is_empty(),
        "translation key coverage violations:\n{}",
        violations.join("\n")
    );
}

fn ui_call_ranges(tokens: &[Token]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for at in 0..tokens.len() {
        let is_method = at >= 2
            && tokens[at - 1].symbol('.')
            && matches!(&tokens[at].kind, TokenKind::Ident(name) if UI_METHODS.contains(&name.as_str()))
            && tokens.get(at + 1).is_some_and(|token| token.symbol('('));
        let is_constructor = at >= 3
            && matches!(&tokens[at - 3].kind, TokenKind::Ident(name) if UI_CONSTRUCTORS.contains(&name.as_str()))
            && tokens[at - 2].symbol(':')
            && tokens[at - 1].symbol(':')
            && (tokens[at].ident("new")
                || tokens[at].ident("action")
                || tokens[at].ident("os_action"))
            && tokens.get(at + 1).is_some_and(|token| token.symbol('('));
        if is_method || is_constructor {
            ranges.push((at, call_end(tokens, at + 1)));
        }
    }
    ranges
}

/// Local helpers that render their text parameter as UI. Preference panes
/// build almost all of their copy through these instead of `.label(` or
/// `.child(`, so literals passed to them are checked like `UI_METHODS`.
/// Each entry is verified below to still be a shipped `fn` taking text, so a
/// rename cannot silently drop it from the scan.
const TEXT_HELPERS: &[&str] = &[
    "heading",
    "hint",
    "intro",
    "label_row",
    "match_text",
    "read_only_value",
    "section_header",
    "warning",
];

/// Raw prose reached through `TEXT_HELPERS` when they joined the scan, waiting
/// on catalog keys: `(file, literal prefix)`. Remove an entry when its call
/// site is localized; a stale entry fails the test so this list only shrinks.
const PENDING_TEXT_HELPER_PROSE: &[(&str, &str)] = &[];

/// Names among `candidates` defined in `files` as an `fn` that takes text
/// (`&str`, `SharedString`, or `impl Into<SharedString>`).
fn text_helper_names(files: &[(String, Vec<Token>)], candidates: &[&str]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (_, tokens) in files {
        for at in 0..tokens.len().saturating_sub(2) {
            if !tokens[at].ident("fn") {
                continue;
            }
            let TokenKind::Ident(name) = &tokens[at + 1].kind else {
                continue;
            };
            if !candidates.contains(&name.as_str()) {
                continue;
            }
            let Some(open) = (at + 2..tokens.len()).find(|index| tokens[*index].symbol('(')) else {
                continue;
            };
            let parameters = &tokens[open..call_end(tokens, open)];
            let takes_text = parameters
                .windows(2)
                .any(|pair| pair[0].symbol('&') && pair[1].ident("str"))
                || parameters.iter().any(|token| token.ident("SharedString"));
            if takes_text {
                names.insert(name.clone());
            }
        }
    }
    names
}

/// Calls of text helpers: `.hint(`, `Self::hint(`, or `hint(`.
fn text_helper_call_ranges(tokens: &[Token], helpers: &BTreeSet<String>) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for at in 1..tokens.len() {
        let TokenKind::Ident(name) = &tokens[at].kind else {
            continue;
        };
        if helpers.contains(name)
            && !tokens[at - 1].ident("fn")
            && tokens.get(at + 1).is_some_and(|token| token.symbol('('))
        {
            ranges.push((at, call_end(tokens, at + 1)));
        }
    }
    ranges
}

fn enclosing_function(tokens: &[Token], at: usize) -> String {
    (0..at)
        .rev()
        .find(|index| tokens[*index].ident("fn"))
        .and_then(|index| match &tokens.get(index + 1)?.kind {
            TokenKind::Ident(name) => Some(name.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "<module>".to_string())
}

#[test]
fn direct_translation_keys_have_one_semantic_callsite() {
    let mut callsites: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
    for (relative, tokens) in source_tokens() {
        let (calls, _) = translation_calls(&tokens, &relative);
        let ui_ranges = ui_call_ranges(&tokens);
        for call in calls
            .into_iter()
            .filter(|call| !call.fallback && !call.existence_only)
        {
            let ui_call = ui_ranges
                .iter()
                .filter(|(start, end)| call.token_at > *start && call.token_at < *end)
                .min_by_key(|(start, end)| end - start);
            let is_menu_macro = tokens
                .get(call.token_at + 1)
                .is_some_and(|token| token.symbol('!'));
            let method = if let Some((start, _)) = ui_call {
                match &tokens[*start].kind {
                    TokenKind::Ident(name) => name.as_str(),
                    _ => continue,
                }
            } else if is_menu_macro {
                "menu_action"
            } else {
                continue;
            };
            let semantic_location = format!(
                "{}:{}:{method}",
                relative,
                enclosing_function(&tokens, call.token_at)
            );
            for key in call.keys {
                callsites
                    .entry(key)
                    .or_default()
                    .entry(semantic_location.clone())
                    .or_default()
                    .push(call.location.clone());
            }
        }
    }
    let duplicates: Vec<String> = callsites
        .into_iter()
        .filter(|(_, sites)| sites.len() > 1)
        .map(|(key, sites)| {
            let locations = sites
                .into_values()
                .flat_map(|locations| locations.into_iter())
                .collect::<Vec<_>>();
            format!("`{key}`: {}", locations.join(", "))
        })
        .collect();
    assert!(
        duplicates.is_empty(),
        "a direct key may live in one helper and be reused, but separate direct callsites need contextual keys:\n{}",
        duplicates.join("\n")
    );
}

#[test]
fn explorer_surface_keys_do_not_reuse_retired_names() {
    let referenced: BTreeSet<String> = source_tokens()
        .into_iter()
        .flat_map(|(relative, tokens)| {
            translation_calls(&tokens, &relative)
                .0
                .into_iter()
                .flat_map(|call| call.keys)
        })
        .collect();
    for retired in [
        "explorer.redis_key_dialog.more_not_shown",
        "explorer.security.default_status",
        "explorer.security.password_strength_weak",
        "explorer.security.password_strength_fair",
        "explorer.security.password_strength_strong",
    ] {
        assert!(!referenced.contains(retired), "retired key used: {retired}");
    }
    for current in [
        "explorer.redis_key_dialog.flagged_more_not_shown",
        "explorer.redis_key_dialog.failures_more_not_shown",
        "explorer.security.clickhouse.read_only_status",
        "explorer.security.clickhouse.read_only_detail",
        "explorer.security.initial_status",
        "explorer.security.preview_status",
        "explorer.security.login_catalog_ready",
        "explorer.security.role_catalog_ready",
        "explorer.security.account_catalog_ready",
        "explorer.security.drop_cancelled",
        "explorer.security.password.strength_weak",
        "explorer.security.password.strength_fair",
        "explorer.security.password.strength_strong",
        "explorer.tenant_scoping.save_hint",
    ] {
        assert!(referenced.contains(current), "missing key use: {current}");
    }
}

fn localization_format_ranges(tokens: &[Token]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for at in 0..tokens.len().saturating_sub(1) {
        // Any helper whose first argument is a catalog key (for example
        // `layout_text("results.layout.…", &[("placement", …)])`) formats a
        // translated message: its `(name, value)` names are placeholders.
        let keyed_helper = matches!(&tokens[at].kind, TokenKind::Ident(_))
            && matches!(
                tokens.get(at + 2).map(|token| &token.kind),
                Some(TokenKind::String(key)) if looks_like_catalog_key(key)
            );
        if (tokens[at].ident("localized_format") || keyed_helper) && tokens[at + 1].symbol('(') {
            ranges.push((at, call_end(tokens, at + 1)));
        }
    }
    ranges
}

fn non_visible_call_ranges(tokens: &[Token]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    for at in 0..tokens.len() {
        let internal_method = at >= 2
            && tokens[at - 1].symbol('.')
            && [
                "expect",
                "id",
                "debug_selector",
                "path",
                "key_context",
                "strip_prefix",
                "strip_suffix",
            ]
            .iter()
            .any(|name| tokens[at].ident(name))
            && tokens.get(at + 1).is_some_and(|token| token.symbol('('));
        let internal_constructor = at >= 3
            && matches!(&tokens[at - 3].kind, TokenKind::Ident(name) if name == "Button" || name == "Dialog" || name == "Checkbox")
            && tokens[at - 2].symbol(':')
            && tokens[at - 1].symbol(':')
            && tokens[at].ident("new")
            && tokens.get(at + 1).is_some_and(|token| token.symbol('('));
        let localization = at >= 4
            && tokens[at - 4].ident("localization")
            && tokens[at - 3].symbol(':')
            && tokens[at - 2].symbol(':')
            && (tokens[at - 1].ident("text") || tokens[at - 1].ident("format"))
            && tokens[at].symbol('(');
        // `cfg!(target_os = "macos")` and friends: configuration predicates.
        let cfg_macro = tokens[at].ident("cfg")
            && tokens.get(at + 1).is_some_and(|token| token.symbol('!'))
            && tokens.get(at + 2).is_some_and(|token| token.symbol('('));
        if cfg_macro {
            ranges.push((at, call_end(tokens, at + 2)));
        } else if internal_method || internal_constructor {
            ranges.push((at, call_end(tokens, at + 1)));
        } else if localization {
            ranges.push((at - 1, call_end(tokens, at)));
        }
    }
    ranges
}

fn is_internal_argument_name(
    tokens: &[Token],
    at: usize,
    format_ranges: &[(usize, usize)],
) -> bool {
    let named_helper = at >= 2
        && ["with_arg", "mode_button", "zoom_button"]
            .iter()
            .any(|name| tokens[at - 2].ident(name))
        && tokens[at - 1].symbol('(')
        && tokens.get(at + 1).is_some_and(|token| token.symbol(','));
    let format_argument = at >= 1
        && tokens[at - 1].symbol('(')
        && tokens.get(at + 1).is_some_and(|token| token.symbol(','))
        && format_ranges
            .iter()
            .any(|(start, end)| at > *start && at < *end);
    let match_sentinel = tokens.get(at + 1).is_some_and(|token| token.symbol('='))
        && tokens.get(at + 2).is_some_and(|token| token.symbol('>'))
        || at >= 2 && tokens[at - 1].symbol('=') && tokens[at - 2].symbol('=')
        || tokens.get(at + 1).is_some_and(|token| token.symbol('='))
            && tokens.get(at + 2).is_some_and(|token| token.symbol('='))
        || at >= 2 && tokens[at - 1].symbol('=') && tokens[at - 2].symbol('!')
        || tokens.get(at + 1).is_some_and(|token| token.symbol('!'))
            && tokens.get(at + 2).is_some_and(|token| token.symbol('='));
    named_helper || format_argument || match_sentinel
}

fn is_stable_diff_class_filter_id(tokens: &[Token], at: usize) -> bool {
    let TokenKind::String(value) = &tokens[at].kind else {
        return false;
    };
    if !matches!(value.as_str(), "Removed" | "Added" | "Changed")
        || at < 2
        || !tokens[at - 2].ident("Some")
        || !tokens[at - 1].symbol('(')
        || !tokens.get(at + 1).is_some_and(|token| token.symbol(')'))
        || enclosing_function(tokens, at) != "render_class_filters"
        || !tokens.windows(4).any(|window| {
            window[0].ident("class_filter")
                && window[1].symbol('=')
                && window[2].symbol('=')
                && window[3].ident("filter")
        })
    {
        return false;
    }
    let Some(open) = (1..at.saturating_sub(1))
        .rev()
        .find(|index| tokens[*index].ident("mk") && tokens[*index + 1].symbol('('))
        .map(|index| index + 1)
    else {
        return false;
    };
    let mut depth = 0;
    let mut argument = 0;
    for token in &tokens[open + 1..at] {
        if token.symbol('(') {
            depth += 1;
        } else if token.symbol(')') {
            depth -= 1;
        } else if depth == 0 && token.symbol(',') {
            argument += 1;
        }
    }
    argument == 2
}

fn raw_prose_offenders(
    tokens: &[Token],
    relative: &str,
    text_helpers: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut offenders = BTreeSet::new();
    let format_ranges = localization_format_ranges(tokens);
    let non_visible_ranges = non_visible_call_ranges(tokens);
    let mut calls = ui_call_ranges(tokens);
    calls.extend(text_helper_call_ranges(tokens, text_helpers));
    for (at, end) in calls {
        for (offset, token) in tokens[at + 2..end.saturating_sub(1)].iter().enumerate() {
            let token_at = at + 2 + offset;
            if let TokenKind::String(value) = &token.kind
                && !is_stable_diff_class_filter_id(tokens, token_at)
                && !non_visible_ranges
                    .iter()
                    .any(|(start, end)| token_at > *start && token_at < *end)
                && !is_internal_argument_name(tokens, token_at, &format_ranges)
                && is_raw_prose(value)
            {
                offenders.insert(format!(
                    "{}:{}: `{}`",
                    relative,
                    token.line,
                    value.replace('\n', "\\n")
                ));
            }
        }
    }
    offenders
}

#[test]
fn shipped_ui_calls_do_not_contain_raw_prose() {
    let mut offenders = BTreeSet::new();
    let files = source_tokens();
    let text_helpers = text_helper_names(&files, TEXT_HELPERS);
    let missing = TEXT_HELPERS
        .iter()
        .filter(|helper| !text_helpers.contains(**helper))
        .collect::<Vec<_>>();
    assert!(
        missing.is_empty(),
        "TEXT_HELPERS entries no longer defined as text-taking fns: {missing:?}"
    );
    for (relative, tokens) in &files {
        offenders.extend(raw_prose_offenders(tokens, relative, &text_helpers));
    }
    let mut stale_pending = Vec::new();
    for (file, prefix) in PENDING_TEXT_HELPER_PROSE {
        let before = offenders.len();
        offenders.retain(|line| {
            !(line.starts_with(&format!("{file}:")) && line.contains(&format!("`{prefix}")))
        });
        if offenders.len() == before {
            stale_pending.push(format!("{file}: `{prefix}`"));
        }
    }
    assert!(
        stale_pending.is_empty(),
        "PENDING_TEXT_HELPER_PROSE entries no longer match raw prose; remove them:\n{}",
        stale_pending.join("\n")
    );
    assert!(
        offenders.is_empty(),
        "raw prose in shipped UI calls; use localization::text/format with a contextual key:\n{}",
        offenders.into_iter().collect::<Vec<_>>().join("\n")
    );
}

fn call_end(tokens: &[Token], open: usize) -> usize {
    let mut depth = 0;
    for (at, token) in tokens.iter().enumerate().skip(open) {
        if token.symbol('(') {
            depth += 1;
        } else if token.symbol(')') {
            depth -= 1;
            if depth == 0 {
                return at + 1;
            }
        }
    }
    tokens.len()
}

fn looks_like_catalog_key(value: &str) -> bool {
    let mut segments = value.split('.');
    let mut count = 0;
    let valid = segments.all(|segment| {
        count += 1;
        segment.starts_with(|ch: char| ch.is_ascii_lowercase())
            && segment
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
            && !segment.starts_with('_')
            && !segment.ends_with('_')
            && !segment.contains("__")
    });
    valid && count > 1
}

fn is_internal_identifier(value: &str) -> bool {
    (value.contains('_') || value.contains('-'))
        && value.starts_with(|ch: char| ch.is_ascii_lowercase())
        && value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn is_raw_prose(literal: &str) -> bool {
    let without_placeholders = remove_format_placeholders(literal);
    let text = without_placeholders.trim();
    if text.is_empty()
        || !text.chars().any(char::is_alphabetic)
        || looks_like_catalog_key(text)
        || is_internal_identifier(text)
        || literal.trim().starts_with("v{")
            && text
                .strip_prefix('v')
                .is_some_and(|rest| rest.chars().all(|ch| !ch.is_alphabetic()))
    {
        return false;
    }
    let proper = text.trim_end_matches(['…', '.', ':']);
    if PROPER_NAMES.contains(&proper)
        || TECHNICAL_TOKENS.contains(&text)
        || TECHNICAL_LITERALS.contains(&text)
        || text.starts_with("$.")
        || text
            .strip_prefix("(SPDX:")
            .is_some_and(|rest| rest.chars().all(|ch| !ch.is_alphabetic()))
        || literal.contains('{')
            && !literal.chars().any(char::is_whitespace)
            && text.chars().all(|ch| {
                ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '-' | '.')
            })
    {
        return false;
    }
    if text.contains("://")
        || text.starts_with('/')
        || text.starts_with("~/")
        || text.starts_with("./")
        || text.starts_with("../")
        || text.as_bytes().get(1) == Some(&b':') && text.as_bytes().get(2) == Some(&b'\\')
    {
        return false;
    }
    let first = text.split_whitespace().next().unwrap_or_default();
    if TECHNICAL_TOKENS.contains(&first)
        && first
            .chars()
            .all(|c| !c.is_alphabetic() || c.is_uppercase())
    {
        return false;
    }
    // A one-word UI label such as "Name:" or "Host:" is prose even though it
    // ends with a colon; all-caps stems ("ID:", "SQL:") stay technical.
    if let Some(stem) = text.strip_suffix(':')
        && !stem.is_empty()
        && stem.chars().all(char::is_alphabetic)
        && stem.starts_with(char::is_uppercase)
        && stem.chars().any(char::is_lowercase)
    {
        return true;
    }
    if !text.chars().any(char::is_whitespace)
        && (text.contains("::")
            || text.contains('/')
            || text.contains('\\')
            || text.starts_with('.')
            || text.ends_with(':')
            || text.chars().any(|c| c.is_ascii_digit()))
    {
        return false;
    }
    true
}

fn remove_format_placeholders(literal: &str) -> String {
    let mut output = String::new();
    let mut chars = literal.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                output.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                output.push('}');
            }
            '{' => {
                let mut depth = 1;
                for inner in chars.by_ref() {
                    if inner == '{' {
                        depth += 1;
                    } else if inner == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
            }
            _ => output.push(ch),
        }
    }
    output
}

#[test]
fn scanner_handles_raw_strings_named_placeholders_and_test_modules() {
    let tokens = shipped_tokens(&lex_rust(
        r##"
        struct Example {
            #[cfg(test)]
            test_only_field: String,
            shipped_field: String,
        }
        fn shipped() {
            view.label(format!("Deleted {count} rows from {table_name}"));
            view.title(r#"Raw title"#);
        }
        #[cfg(test)]
        mod tests {
            #[test]
            fn ignored() { view.label("Test only"); }
        }
        #[cfg(all(test, not(target_arch = "wasm32")))]
        mod platform_tests {
            fn ignored_platform() { view.label("Platform test only"); }
        }
        "##,
    ));
    assert!(tokens.iter().any(|token| token.ident("format")));
    assert!(tokens.iter().any(|token| token.ident("shipped_field")));
    assert!(!tokens.iter().any(|token| token.ident("test_only_field")));
    assert!(!tokens.iter().any(|token| token.ident("ignored")));
    assert!(!tokens.iter().any(|token| token.ident("ignored_platform")));
    assert!(is_raw_prose("Deleted {count} rows from {table_name}"));
    assert!(!is_raw_prose("{schema_name}.{table_name}"));
    assert!(!is_raw_prose("PostgreSQL"));
    assert!(!is_raw_prose("SELECT * FROM {table_name}"));
    assert!(is_raw_prose("Open in Spreadsheet"));
    assert!(!is_raw_prose("row_count"));
    assert!(!is_raw_prose("settings.appearance.language.title"));

    let tokens = lex_rust(
        r#"
        macro_rules! action {
            ($key:literal) => { crate::localization::text($key) };
        }
        fn helper(key: &str) -> String { crate::localization::text(key) }
        fn shipped() {
            action!("menu.file.open");
            helper("menu.file.close");
            let key = if ready { "editor.status.ready" } else { "editor.status.busy" };
            crate::localization::text(key);
            crate::localization::text(match state {
                State::Open => "editor.state.open",
                State::Closed => "editor.state.closed",
            });
        }
        "#,
    );
    assert!(is_raw_prose("Name:"));
    assert!(is_raw_prose("Host:"));
    assert!(!is_raw_prose("ID:"));
    assert!(!is_raw_prose("SQL:"));
    assert!(!is_raw_prose("host:"));
    let keyed = lex_rust(
        r#"
        fn work() {
            use crate::localization::{deferred, deferred_text};
            deferred_text("worker.state.idle");
            deferred("worker.state.busy", &[("count", 2)]);
            crate::localization::try_format("worker.state.done", &[("count", 2)]);
            crate::localization::input_placeholder(window, cx, "worker.input.placeholder");
            sqlly_core::DisplayMessage::new("worker.message.lost", "Lost");
            gpui::deferred(anchored());
            self.deferred("not.a.localization_call");
        }
        "#,
    );
    let (keyed_calls, _) = translation_calls(&keyed, "fixture.rs");
    assert!(keyed_calls.iter().all(|call| call.existence_only));
    let keyed_keys: BTreeSet<String> = keyed_calls.into_iter().flat_map(|call| call.keys).collect();
    assert_eq!(
        keyed_keys,
        BTreeSet::from([
            "worker.input.placeholder".to_string(),
            "worker.message.lost".to_string(),
            "worker.state.busy".to_string(),
            "worker.state.done".to_string(),
            "worker.state.idle".to_string(),
        ])
    );

    let (calls, violations) = translation_calls(&tokens, "fixture.rs");
    assert!(violations.is_empty(), "{violations:?}");
    let keys: BTreeSet<String> = calls.into_iter().flat_map(|call| call.keys).collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "editor.state.closed".to_string(),
            "editor.state.open".to_string(),
            "editor.status.busy".to_string(),
            "editor.status.ready".to_string(),
            "menu.file.close".to_string(),
            "menu.file.open".to_string(),
        ])
    );

    let tokens = lex_rust(
        r#"
        fn render_class_filters(&self) {
            let selected = self.class_filter == filter;
            div()
                .child(mk("dc-filter-left", localized(), Some("Removed")))
                .child(mk("dc-filter-right", localized(), Some("Added")))
                .child(mk("dc-filter-changed", localized(), Some("Changed")))
                .child(button.label("Delete"));
        }
        "#,
    );
    let offenders = raw_prose_offenders(&tokens, "fixture.rs", &BTreeSet::new());
    assert_eq!(offenders.len(), 1, "{offenders:?}");
    assert!(offenders.iter().any(|line| line.contains("`Delete`")));

    let tokens = shipped_tokens(&lex_rust(
        r#"
        impl Pane {
            fn hint(&self, text: &str) -> AnyElement { div().child(self.match_text(text)).into_any_element() }
            fn badge(label: impl Into<SharedString>) -> impl IntoElement { div() }
            fn width(&self, value: f32) -> AnyElement { div().into_any_element() }
            fn render(&self) {
                self.hint("Shown under the toggle");
                Self::badge("Beta feature");
                badge("row_count");
                self.hint(&crate::localization::text("settings.pane.hint"));
            }
        }
        "#,
    ));
    let files = vec![("fixture.rs".to_string(), tokens.clone())];
    let helpers = text_helper_names(&files, &["hint", "badge", "width", "missing"]);
    assert_eq!(
        helpers,
        BTreeSet::from(["badge".to_string(), "hint".to_string()])
    );
    let offenders = raw_prose_offenders(&tokens, "fixture.rs", &helpers);
    assert_eq!(offenders.len(), 2, "{offenders:?}");
    assert!(
        offenders
            .iter()
            .any(|line| line.contains("`Shown under the toggle`"))
    );
    assert!(offenders.iter().any(|line| line.contains("`Beta feature`")));
}

// ---------------------------------------------------------------------------
// Placeholder contract: every formatted key's English `{placeholder}` set must
// equal the argument names its call site passes, and keys read through
// `text()` must not expect any. At runtime a mismatch makes `format` fall
// back to English or to the key, so it has to fail here instead.
// ---------------------------------------------------------------------------

/// Format call sites whose key or arguments cannot be read from the source:
/// `(file, enclosing fn, justification)`. Every entry must still match an
/// unresolved call site, so the list only shrinks.
const FORMAT_CONTRACT_UNRESOLVED: &[(&str, &str, &str)] = &[
    (
        "src/app/mod.rs",
        "render",
        "the information-panel title key arrives through the overlays tuple (\"about.window_title\" / \"open_source.window_title\", both brace-free) and is read through text()",
    ),
    (
        "src/app/execute.rs",
        "run_label",
        "toolbar.run.* label/tooltip keys are derived with format!(\"{key}_label\"); run_label_key_uses checks they exist and text() takes no arguments",
    ),
    (
        "src/app_update.rs",
        "part",
        "the caller passes a singular/plural key pair as parameters, both formatted with {count}",
    ),
    (
        "src/cell_actions.rs",
        "fmt",
        "LocalizedReason wraps a &'static str key read through text()",
    ),
    (
        "src/command_palette.rs",
        "register",
        "command titles use keys derived from command ids; command_palette_key_uses checks them",
    ),
    (
        "src/default_picks.rs",
        "fmt",
        "DefaultPickLabel::Localized carries a &'static str key read through text()",
    ),
    (
        "src/designer.rs",
        "render",
        "row_checkbox receives &'static str label keys from its callers and reads them through text()",
    ),
    (
        "src/editor/dialogs.rs",
        "column_format_choices",
        "choice label keys come from the local (key, directive) table matched on the column type and are read through text()",
    ),
    (
        "src/menu_bar.rs",
        "<module>",
        "menu macros take the key as a macro argument ($key) and read it through text()",
    ),
    (
        "src/preferences_ui/editor.rs",
        "month_name",
        "indexes a local array of month-name keys read through text()",
    ),
    (
        "src/project_file_picker.rs",
        "render_preview",
        "type/source chip keys come from enum localization_key() methods, read through text()",
    ),
    (
        "src/project_file_picker.rs",
        "render_row",
        "type/source chip keys come from enum localization_key() methods, read through text()",
    ),
    (
        "src/project_file_picker_model.rs",
        "resolve",
        "a deferred message struct formats its stored key with its own with_arg list",
    ),
    (
        "src/schema_tree/render.rs",
        "object_tooltip",
        "the node-kind label key comes from NodeKind and is read through text()",
    ),
    (
        "src/self_update.rs",
        "localized_error",
        "re-resolves an UpdateError DisplayMessage's own key and arguments",
    ),
    (
        "src/server_dashboard.rs",
        "update_tiles",
        "tile label keys come from the static tile spec table and are read through text()",
    ),
    (
        "src/sql_agent.rs",
        "render_editor",
        "field label keys come from the editor's field table and are read through text()",
    ),
    (
        "src/workspace_session.rs",
        "resolve",
        "a deferred message struct formats its stored key with its own with_arg list",
    ),
];

/// Keys read through `text()` whose English value shows brace syntax to the
/// user on purpose (template variables, filename tokens), so the braces are
/// prose rather than placeholders: `(key, justification)`.
const TEXT_LITERAL_BRACES: &[(&str, &str)] = &[
    (
        "ai.templates.comment_selection_body",
        "AI prompt template stored verbatim; ai_templates::expand fills ${selection} later",
    ),
    (
        "ai.templates.describe_table_body",
        "AI prompt template stored verbatim; ai_templates::expand fills ${table}/${database} later",
    ),
    (
        "ai.templates.describe_table_name",
        "AI prompt template name stored verbatim; ai_templates::expand fills ${table} later",
    ),
    (
        "ai.templates.explain_selection_body",
        "AI prompt template stored verbatim; ai_templates::expand fills ${selection} later",
    ),
    (
        "ai.templates.optimize_selection_body",
        "AI prompt template stored verbatim; ai_templates::expand fills ${dialect}/${database}/${selection} later",
    ),
    (
        "editor.format_directives.docs.variable_value",
        "directive reference documenting the ${name} variable syntax",
    ),
    (
        "editor.format_directives.docs.variables_overview",
        "directive reference documenting the ${name} variable syntax",
    ),
    (
        "editor.template_explorer.body_help",
        "help text documenting the template variables {server}/{database}/{schema}/{object}/{kind}",
    ),
    (
        "help.tip_of_the_day.tips.variables_body",
        "tip text documenting the ${schema} script-variable syntax",
    ),
    (
        "settings.results.hint.filename_pattern_tokens",
        "hint listing the export filename tokens {schema}/{table}/{date}/{format}",
    ),
    (
        "settings.snippets.hint.placeholder_syntax",
        "hint documenting snippet placeholder syntax ${selection}/${table}/${database}",
    ),
];

fn catalog_values() -> BTreeMap<String, String> {
    fn walk(value: &serde_json::Value, prefix: &str, out: &mut BTreeMap<String, String>) {
        for (segment, child) in value.as_object().expect("catalog groups must be objects") {
            let key = if prefix.is_empty() {
                segment.clone()
            } else {
                format!("{prefix}.{segment}")
            };
            match child.get("value").and_then(serde_json::Value::as_str) {
                Some(text) => {
                    out.insert(key, text.to_string());
                }
                None => walk(child, &key, out),
            }
        }
    }
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/sqlly-i18n/translations/en.json");
    let json = std::fs::read_to_string(path).expect("read authoritative English catalog");
    let catalog: serde_json::Value =
        serde_json::from_str(&json).expect("parse authoritative English catalog");
    let mut out = BTreeMap::new();
    walk(&catalog, "", &mut out);
    out
}

/// The `sqlly_i18n` placeholder grammar: `{{`/`}}` are literal braces and a
/// placeholder is a snake_case ASCII name; other brace text is prose.
fn catalog_placeholders(value: &str) -> BTreeSet<String> {
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
            if is_placeholder_name(name) {
                names.insert(name.to_string());
                remaining = &remaining[end + 2..];
                continue;
            }
        }
        remaining = &remaining[1..];
    }
    names
}

fn is_placeholder_name(name: &str) -> bool {
    name.starts_with(|ch: char| ch.is_ascii_lowercase())
        && !name.ends_with('_')
        && !name.contains("__")
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

fn is_open(token: &Token) -> bool {
    token.symbol('(') || token.symbol('[') || token.symbol('{')
}

fn is_close(token: &Token) -> bool {
    token.symbol(')') || token.symbol(']') || token.symbol('}')
}

/// Top-level comma-separated argument ranges of the call whose `(` is at
/// `open`, as half-open token ranges.
fn call_arguments(tokens: &[Token], open: usize) -> Vec<(usize, usize)> {
    let end = call_end(tokens, open).saturating_sub(1);
    let mut ranges = Vec::new();
    let mut depth = 0i32;
    let mut start = open + 1;
    for (at, token) in tokens.iter().enumerate().take(end).skip(open + 1) {
        if is_open(token) {
            depth += 1;
        } else if is_close(token) {
            depth -= 1;
        } else if depth == 0 && token.symbol(',') {
            ranges.push((start, at));
            start = at + 1;
        }
    }
    if start < end {
        ranges.push((start, end));
    }
    ranges
}

/// End of the statement starting at `at`: the first `;` outside brackets, or
/// the bracket that closes the enclosing block.
fn statement_end(tokens: &[Token], at: usize) -> usize {
    let mut depth = 0i32;
    for (index, token) in tokens.iter().enumerate().skip(at) {
        if is_open(token) {
            depth += 1;
        } else if is_close(token) {
            depth -= 1;
            if depth < 0 {
                return index;
            }
        } else if depth == 0 && token.symbol(';') {
            return index;
        }
    }
    tokens.len()
}

/// `("name", value)` tuple names inside a token range: a placeholder-shaped
/// literal opening a tuple that starts an array element.
fn tuple_argument_names(tokens: &[Token], start: usize, end: usize) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for at in start.max(2)..end.saturating_sub(1) {
        if let TokenKind::String(name) = &tokens[at].kind
            && tokens[at - 1].symbol('(')
            && (tokens[at - 2].symbol('[') || tokens[at - 2].symbol(','))
            && tokens[at + 1].symbol(',')
            && is_placeholder_name(name)
        {
            names.insert(name.clone());
        }
    }
    names
}

struct FnScope {
    start: usize,
    name: String,
    /// Parameter names, blank for parameters that cannot carry a key or
    /// `(name, value)` arguments (anything but text or a slice).
    params: Vec<String>,
    public: bool,
}

fn fn_scope(tokens: &[Token], at: usize) -> Option<FnScope> {
    // Skip `fn(…)` pointer types inside signatures.
    let start = (0..at).rev().find(|index| {
        tokens[*index].ident("fn")
            && matches!(
                tokens.get(*index + 1).map(|token| &token.kind),
                Some(TokenKind::Ident(_))
            )
    })?;
    let TokenKind::Ident(name) = &tokens.get(start + 1)?.kind else {
        return None;
    };
    let open = (start + 2..at).find(|index| tokens[*index].symbol('('))?;
    let params = call_arguments(tokens, open)
        .into_iter()
        .filter_map(|(from, to)| {
            let colon = (from..to).find(|index| tokens[*index].symbol(':'))?;
            let name = tokens[from..colon]
                .iter()
                .rev()
                .find_map(|token| match &token.kind {
                    TokenKind::Ident(name) if name != "mut" => Some(name.clone()),
                    _ => None,
                })?;
            let carries_text = tokens[colon + 1..to].iter().any(|token| {
                token.ident("str") || token.ident("SharedString") || token.symbol('[')
            });
            Some(if carries_text { name } else { String::new() })
        })
        .collect();
    let public = tokens[start.saturating_sub(6)..start]
        .iter()
        .rev()
        .take_while(|token| !token.symbol(';') && !token.symbol('{') && !token.symbol('}'))
        .any(|token| token.ident("pub"));
    Some(FnScope {
        start,
        name: name.clone(),
        params,
        public,
    })
}

/// A `let` binding: the `let` token, the `=` token, the statement end, and
/// whether the pattern binds several names.
#[derive(Clone, Copy, PartialEq, Eq)]
struct LetBinding {
    at: usize,
    equals: usize,
    end: usize,
    destructures: bool,
}

/// The most recent `let` in `scope_start..at` binding `name`. Control-flow
/// bindings (`if let` / `while let`) are skipped: their "statement" is the
/// whole control-flow block, so its extent would swallow every later literal
/// in the function instead of the bound expression.
fn let_binding(tokens: &[Token], scope_start: usize, at: usize, name: &str) -> Option<LetBinding> {
    for index in (scope_start..at).rev() {
        if !tokens[index].ident("let") {
            continue;
        }
        if index > 0 && (tokens[index - 1].ident("if") || tokens[index - 1].ident("while")) {
            continue;
        }
        let Some(equals) = (index + 1..at)
            .find(|candidate| tokens[*candidate].symbol('=') || tokens[*candidate].symbol(';'))
        else {
            continue;
        };
        if !tokens[equals].symbol('=') {
            continue;
        }
        let pattern = &tokens[index + 1..equals];
        let colon = pattern
            .iter()
            .position(|token| token.symbol(':'))
            .unwrap_or(pattern.len());
        let pattern = &pattern[..colon];
        if pattern.iter().any(|token| token.ident(name)) {
            let bound = pattern
                .iter()
                .filter(|token| matches!(&token.kind, TokenKind::Ident(other) if other != "mut"))
                .count();
            return Some(LetBinding {
                at: index,
                equals,
                end: statement_end(tokens, equals + 1),
                destructures: bound > 1,
            });
        }
    }
    None
}

/// The binding a name ultimately comes from, following re-bindings such as
/// `let arguments = arguments.iter().map(…).collect::<Vec<_>>();`.
fn root_binding(tokens: &[Token], scope_start: usize, at: usize, name: &str) -> Option<LetBinding> {
    let mut binding = let_binding(tokens, scope_start, at, name)?;
    loop {
        let first = tokens[binding.equals + 1..binding.end]
            .iter()
            .find(|token| !token.symbol('&'))?;
        let TokenKind::Ident(source) = &first.kind else {
            return Some(binding);
        };
        // `let refs = arguments.iter().map(…)` re-shapes another binding.
        let reshapes = tuple_argument_names(tokens, binding.equals + 1, binding.end).is_empty()
            && tokens
                .get(
                    tokens[binding.equals + 1..binding.end]
                        .iter()
                        .position(|token| std::ptr::eq(token, first))
                        .map_or(0, |offset| binding.equals + 2 + offset),
                )
                .is_some_and(|token| token.symbol('.'));
        if !reshapes {
            return Some(binding);
        }
        match let_binding(tokens, scope_start, binding.at, source) {
            Some(previous) => binding = previous,
            None => return Some(binding),
        }
    }
}

/// The identifier an expression names: `x`, `&x`, `&x[..]`, `x.as_slice()`.
fn single_ident(tokens: &[Token], start: usize, end: usize) -> Option<String> {
    let rest: Vec<&Token> = tokens[start..end]
        .iter()
        .filter(|token| !token.symbol('&') && !token.ident("mut"))
        .collect();
    let (first, rest) = rest.split_first()?;
    let TokenKind::Ident(name) = &first.kind else {
        return None;
    };
    rest.iter()
        .all(|token| {
            token.symbol('[')
                || token.symbol(']')
                || token.symbol('.')
                || token.symbol('(')
                || token.symbol(')')
                || token.ident("as_slice")
        })
        .then(|| name.clone())
}

/// Catalog keys written as literals in a token range.
fn catalog_literals(
    tokens: &[Token],
    start: usize,
    end: usize,
    catalog: &BTreeMap<String, String>,
) -> Vec<(usize, String)> {
    (start..end)
        .filter_map(|index| match &tokens[index].kind {
            TokenKind::String(value) if catalog.contains_key(value) => Some((index, value.clone())),
            _ => None,
        })
        .collect()
}

#[derive(Clone, Debug)]
enum Resolved<T> {
    Value(T),
    Param(usize),
    /// A parameter of `let closure = |…| …` in the current function.
    ClosureParam(String, usize),
    Unresolved(String),
}

/// Functions in a file that return a key (`-> &'static str`, `-> &str`, or
/// an `Option` of one), with the catalog literals in their bodies.
fn key_functions(
    tokens: &[Token],
    catalog: &BTreeMap<String, String>,
) -> BTreeMap<String, Vec<String>> {
    let mut functions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for at in 0..tokens.len().saturating_sub(2) {
        if !tokens[at].ident("fn") {
            continue;
        }
        let TokenKind::Ident(name) = &tokens[at + 1].kind else {
            continue;
        };
        let Some(open) = (at + 2..tokens.len()).find(|index| tokens[*index].symbol('(')) else {
            continue;
        };
        let close = call_end(tokens, open);
        let Some(body) = (close..tokens.len())
            .find(|index| tokens[*index].symbol('{') || tokens[*index].symbol(';'))
        else {
            continue;
        };
        if tokens[body].symbol(';') {
            continue;
        }
        let returns_str = tokens[close..body]
            .windows(2)
            .any(|pair| pair[0].symbol('-') && pair[1].symbol('>'))
            && tokens[close..body].iter().any(|token| token.ident("str"))
            && !tokens[close..body]
                .iter()
                .any(|token| token.ident("String") || token.ident("Vec"));
        if !returns_str {
            continue;
        }
        let mut depth = 0;
        let mut end = body;
        while end < tokens.len() {
            if tokens[end].symbol('{') {
                depth += 1;
            } else if tokens[end].symbol('}') {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            end += 1;
        }
        let keys: Vec<String> = catalog_literals(tokens, body, end, catalog)
            .into_iter()
            .map(|(_, key)| key)
            .collect();
        if !keys.is_empty() {
            functions.entry(name.clone()).or_default().extend(keys);
        }
    }
    functions
}

/// `const`/`static` tables of keys (`const TITLES: [&str; 3] = ["a.b", …];`),
/// with the catalog literals they hold.
fn key_tables(
    tokens: &[Token],
    catalog: &BTreeMap<String, String>,
) -> BTreeMap<String, Vec<String>> {
    let mut tables: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for at in 0..tokens.len().saturating_sub(2) {
        if !(tokens[at].ident("const") || tokens[at].ident("static")) {
            continue;
        }
        let TokenKind::Ident(name) = &tokens[at + 1].kind else {
            continue;
        };
        if !name
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
        {
            continue;
        }
        let Some(equals) = (at + 2..tokens.len()).find(|index| tokens[*index].symbol('=')) else {
            continue;
        };
        let end = statement_end(tokens, equals + 1);
        let keys: Vec<String> = catalog_literals(tokens, equals + 1, end, catalog)
            .into_iter()
            .map(|(_, key)| key)
            .collect();
        if !keys.is_empty() {
            tables.entry(name.clone()).or_default().extend(keys);
        }
    }
    tables
}

/// For a parameter of an inline closure (`.map(|name| …)`), the token range
/// of the statement leading up to that closure: the iterated keys live there.
fn inline_closure_source(
    tokens: &[Token],
    scope_start: usize,
    at: usize,
    param: &str,
) -> Option<(usize, usize)> {
    for close in (scope_start..at).rev() {
        if !tokens[close].symbol('|') {
            continue;
        }
        let open = (scope_start..close)
            .rev()
            .find(|index| tokens[*index].symbol('|'))?;
        if !tokens[open + 1..close]
            .iter()
            .any(|token| token.ident(param))
            || open == 0
        {
            return None;
        }
        if !tokens[open - 1].symbol('(') {
            return None;
        }
        let mut depth = 0i32;
        let mut start = open;
        while start > scope_start {
            let token = &tokens[start - 1];
            if is_close(token) {
                depth += 1;
            } else if is_open(token) {
                if depth == 0 && token.symbol('{') {
                    break;
                }
                depth -= 1;
            } else if depth == 0 && (token.symbol(';') || token.ident("let")) {
                break;
            }
            start -= 1;
        }
        return Some((start, open));
    }
    None
}

/// The closure `let name = |…, param, …|` in `scope_start..at` that takes
/// `param`, and the parameter's position.
fn closure_param(
    tokens: &[Token],
    scope_start: usize,
    at: usize,
    param: &str,
) -> Option<(String, usize)> {
    for open in (scope_start..at).rev() {
        if !tokens[open].symbol('|') || open < 3 {
            continue;
        }
        if !(tokens[open - 3].ident("let") || tokens[open - 2].ident("let"))
            || !tokens[open - 1].symbol('=')
        {
            continue;
        }
        let TokenKind::Ident(name) = &tokens[open - 2].kind else {
            continue;
        };
        let close = (open + 1..at).find(|index| tokens[*index].symbol('|'))?;
        let mut index = 0;
        let mut depth = 0i32;
        let mut expect_name = true;
        for token in &tokens[open + 1..close] {
            if token.symbol('<') || token.symbol('(') || token.symbol('[') {
                depth += 1;
            } else if token.symbol('>') || token.symbol(')') || token.symbol(']') {
                depth -= 1;
            } else if depth == 0 && token.symbol(',') {
                index += 1;
                expect_name = true;
            } else if depth == 0 && token.symbol(':') {
                expect_name = false;
            } else if expect_name && token.ident(param) {
                return Some((name.clone(), index));
            }
        }
    }
    None
}

/// The iterated expression of an enclosing `for <pattern> in <expr> {` that
/// binds `name`.
fn for_binding(
    tokens: &[Token],
    scope_start: usize,
    at: usize,
    name: &str,
) -> Option<(usize, usize)> {
    for index in (scope_start..at).rev() {
        if !tokens[index].ident("for") {
            continue;
        }
        let Some(in_at) = (index + 1..at).find(|candidate| tokens[*candidate].ident("in")) else {
            continue;
        };
        if !tokens[index + 1..in_at]
            .iter()
            .any(|token| token.ident(name))
        {
            continue;
        }
        let mut depth = 0i32;
        for (body, token) in tokens.iter().enumerate().take(at).skip(in_at + 1) {
            if token.symbol('(') || token.symbol('[') {
                depth += 1;
            } else if token.symbol(')') || token.symbol(']') {
                depth -= 1;
            } else if depth == 0 && token.symbol('{') {
                return Some((in_at + 1, body));
            }
        }
    }
    None
}

/// Keys a call can format, and the `let` they were chosen in (calls sharing
/// one binding are checked together, see `check_groups`).
#[derive(Clone, Debug)]
struct Keys {
    keys: Vec<String>,
    binding: Option<usize>,
}

/// Argument names a call passes: always, and only on some paths (`push`ed
/// after the binding).
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Arguments {
    required: BTreeSet<String>,
    optional: BTreeSet<String>,
}

impl Arguments {
    fn exact(required: BTreeSet<String>) -> Self {
        Self {
            required,
            optional: BTreeSet::new(),
        }
    }

    fn accepts(&self, expected: &BTreeSet<String>) -> bool {
        self.required.is_subset(expected)
            && expected
                .iter()
                .all(|name| self.required.contains(name) || self.optional.contains(name))
    }

    fn describe(&self) -> String {
        if self.optional.is_empty() {
            format!("{:?}", self.required)
        } else {
            format!("{:?} + optional {:?}", self.required, self.optional)
        }
    }
}

struct CallContext<'a> {
    tokens: &'a [Token],
    at: usize,
    catalog: &'a BTreeMap<String, String>,
    constants: &'a BTreeMap<String, BTreeSet<String>>,
    key_functions: &'a BTreeMap<String, Vec<String>>,
    key_tables: &'a BTreeMap<String, Vec<String>>,
    scope: Option<FnScope>,
}

impl<'a> CallContext<'a> {
    fn new(
        tokens: &'a [Token],
        at: usize,
        catalog: &'a BTreeMap<String, String>,
        constants: &'a BTreeMap<String, BTreeSet<String>>,
        key_functions: &'a BTreeMap<String, Vec<String>>,
        key_tables: &'a BTreeMap<String, Vec<String>>,
    ) -> Self {
        Self {
            tokens,
            at,
            catalog,
            constants,
            key_functions,
            key_tables,
            scope: fn_scope(tokens, at),
        }
    }

    /// Keys returned by key functions called, or read from key tables, in a
    /// token range.
    fn called_keys(&self, start: usize, end: usize) -> Vec<String> {
        (start..end)
            .filter_map(|index| match &self.tokens[index].kind {
                TokenKind::Ident(name)
                    if self
                        .tokens
                        .get(index + 1)
                        .is_some_and(|token| token.symbol('(')) =>
                {
                    self.key_functions.get(name)
                }
                TokenKind::Ident(name) => self.key_tables.get(name),
                _ => None,
            })
            .flatten()
            .cloned()
            .collect()
    }

    fn function(&self) -> String {
        self.scope
            .as_ref()
            .map_or_else(|| "<module>".to_string(), |scope| scope.name.clone())
    }

    fn scope_start(&self) -> usize {
        self.scope.as_ref().map_or(0, |scope| scope.start)
    }

    fn param(&self, name: &str) -> Option<usize> {
        self.scope
            .as_ref()?
            .params
            .iter()
            .position(|param| param == name)
    }

    fn keys(&self, (start, end): (usize, usize)) -> Resolved<Keys> {
        let literals = catalog_literals(self.tokens, start, end, self.catalog);
        if !literals.is_empty() {
            return Resolved::Value(Keys {
                keys: literals.into_iter().map(|(_, key)| key).collect(),
                binding: None,
            });
        }
        let called = self.called_keys(start, end);
        if !called.is_empty() {
            return Resolved::Value(Keys {
                keys: called,
                binding: None,
            });
        }
        let Some(name) = single_ident(self.tokens, start, end) else {
            return Resolved::Unresolved("key expression".into());
        };
        if let Some(binding) = let_binding(self.tokens, self.scope_start(), self.at, &name) {
            let mut keys: Vec<String> =
                catalog_literals(self.tokens, binding.equals + 1, binding.end, self.catalog)
                    .into_iter()
                    .map(|(_, key)| key)
                    .collect();
            if keys.is_empty() {
                keys = self.called_keys(binding.equals + 1, binding.end);
            }
            return if keys.is_empty() {
                Resolved::Unresolved(format!("key `{name}` bound without catalog literals"))
            } else {
                Resolved::Value(Keys {
                    keys,
                    binding: Some(binding.at),
                })
            };
        }
        if let Some(index) = self.param(&name) {
            return Resolved::Param(index);
        }
        if let Some((from, to)) = for_binding(self.tokens, self.scope_start(), self.at, &name) {
            let mut keys: Vec<String> = catalog_literals(self.tokens, from, to, self.catalog)
                .into_iter()
                .map(|(_, key)| key)
                .collect();
            if keys.is_empty() {
                keys = self.called_keys(from, to);
            }
            if !keys.is_empty() {
                return Resolved::Value(Keys {
                    keys,
                    binding: None,
                });
            }
        }
        if let Some((closure, index)) =
            closure_param(self.tokens, self.scope_start(), self.at, &name)
        {
            return Resolved::ClosureParam(closure, index);
        }
        if let Some((from, to)) =
            inline_closure_source(self.tokens, self.scope_start(), self.at, &name)
        {
            let mut keys: Vec<String> = catalog_literals(self.tokens, from, to, self.catalog)
                .into_iter()
                .map(|(_, key)| key)
                .collect();
            if keys.is_empty() {
                keys = self.called_keys(from, to);
            }
            if !keys.is_empty() {
                return Resolved::Value(Keys {
                    keys,
                    binding: None,
                });
            }
        }
        if let Some(values) = self.constants.get(&name) {
            let keys: Vec<String> = values
                .iter()
                .filter(|key| self.catalog.contains_key(*key))
                .cloned()
                .collect();
            if !keys.is_empty() {
                return Resolved::Value(Keys {
                    keys,
                    binding: None,
                });
            }
        }
        Resolved::Unresolved(format!("key `{name}`"))
    }

    fn arguments(&self, (start, end): (usize, usize)) -> Resolved<Arguments> {
        let names = tuple_argument_names(self.tokens, start, end);
        if !names.is_empty() {
            return Resolved::Value(Arguments::exact(names));
        }
        if self.tokens[start..end]
            .iter()
            .all(|token| token.symbol('&') || token.symbol('[') || token.symbol(']'))
        {
            return Resolved::Value(Arguments::default());
        }
        let Some(name) = single_ident(self.tokens, start, end) else {
            return Resolved::Unresolved("argument expression".into());
        };
        if let Some(binding) = root_binding(self.tokens, self.scope_start(), self.at, &name) {
            let required = tuple_argument_names(self.tokens, binding.equals + 1, binding.end);
            let mut optional = BTreeSet::new();
            // The root binding's own name (`arguments` behind `argument_refs`).
            let root = self.tokens[binding.at + 1..binding.equals]
                .iter()
                .find_map(|token| match &token.kind {
                    TokenKind::Ident(root) if root != "mut" => Some(root.clone()),
                    _ => None,
                })
                .unwrap_or_else(|| name.clone());
            for index in binding.end..self.at.saturating_sub(3) {
                if self.tokens[index].ident(&root)
                    && self.tokens[index + 1].symbol('.')
                    && (self.tokens[index + 2].ident("push")
                        || self.tokens[index + 2].ident("extend"))
                {
                    let open = index + 3;
                    let close = call_end(self.tokens, open);
                    if self.tokens[open + 1].symbol('(')
                        && let Some(TokenKind::String(argument)) =
                            self.tokens.get(open + 2).map(|token| &token.kind)
                        && is_placeholder_name(argument)
                    {
                        optional.insert(argument.clone());
                    }
                    optional.extend(tuple_argument_names(self.tokens, open, close));
                }
            }
            let empty = self.tokens[binding.equals + 1..binding.end]
                .iter()
                .all(|token| {
                    token.symbol('&')
                        || token.symbol('[')
                        || token.symbol(']')
                        || token.symbol('!')
                        || token.symbol(':')
                        || token.ident("vec")
                        || token.ident("Vec")
                        || token.ident("new")
                        || token.symbol('(')
                        || token.symbol(')')
                });
            if required.is_empty() && optional.is_empty() && !empty {
                return Resolved::Unresolved(format!("arguments `{name}` built without literals"));
            }
            return Resolved::Value(Arguments { required, optional });
        }
        if let Some(index) = self.param(&name) {
            return Resolved::Param(index);
        }
        if let Some((closure, index)) =
            closure_param(self.tokens, self.scope_start(), self.at, &name)
        {
            return Resolved::ClosureParam(closure, index);
        }
        Resolved::Unresolved(format!("arguments `{name}`"))
    }

    /// Keys and arguments chosen together, e.g.
    /// `let (key, arguments) = match … { A => ("k1", vec![("a", …)]), … }`:
    /// each key literal pairs with the tuple names that follow it.
    fn paired(
        &self,
        key: (usize, usize),
        arguments: (usize, usize),
    ) -> Option<Vec<(String, Arguments)>> {
        let key_name = single_ident(self.tokens, key.0, key.1)?;
        let argument_name = single_ident(self.tokens, arguments.0, arguments.1)?;
        let key_binding = let_binding(self.tokens, self.scope_start(), self.at, &key_name)?;
        let argument_binding =
            root_binding(self.tokens, self.scope_start(), self.at, &argument_name)?;
        if key_binding != argument_binding || !key_binding.destructures {
            return None;
        }
        let literals = catalog_literals(
            self.tokens,
            key_binding.equals + 1,
            key_binding.end,
            self.catalog,
        );
        if literals.is_empty() {
            return None;
        }
        Some(
            literals
                .iter()
                .enumerate()
                .map(|(position, (index, key))| {
                    let next = literals
                        .get(position + 1)
                        .map_or(key_binding.end, |(next, _)| *next);
                    (
                        key.clone(),
                        Arguments::exact(tuple_argument_names(self.tokens, *index, next)),
                    )
                })
                .collect(),
        )
    }
}

#[derive(Clone, Debug)]
enum WrapperArguments {
    Fixed(Arguments),
    Param(usize),
    WithArgChain,
}

#[derive(Clone, Debug)]
struct FormatWrapper {
    file: String,
    name: String,
    public: bool,
    key_param: usize,
    arguments: WrapperArguments,
    /// Forwards to `text()`: callers' keys must not expect placeholders.
    text: bool,
    /// For a `let` closure, the `fn` token of the function that defines it:
    /// only calls inside that function reach it.
    closure_fn: Option<usize>,
}

/// `.with_arg("name", …)` names chained directly after a call ending at `end`.
fn with_arg_chain(tokens: &[Token], mut end: usize) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    while end + 3 < tokens.len()
        && tokens[end].symbol('.')
        && tokens[end + 1].ident("with_arg")
        && tokens[end + 2].symbol('(')
    {
        if let TokenKind::String(name) = &tokens[end + 3].kind {
            names.insert(name.clone());
        }
        end = call_end(tokens, end + 2);
    }
    names
}

/// `message = message.with_arg("name", …)` names added to a binding later in
/// its function.
fn later_with_args(tokens: &[Token], binding_end: usize, name: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut depth = 0i32;
    for at in binding_end..tokens.len().saturating_sub(4) {
        if is_open(&tokens[at]) {
            depth += 1;
        } else if is_close(&tokens[at]) {
            depth -= 1;
            if depth < 0 {
                break;
            }
        }
        if tokens[at].ident(name) && tokens[at + 1].symbol('.') && tokens[at + 2].ident("with_arg")
        {
            names.extend(with_arg_chain(tokens, at + 1));
        }
    }
    names
}

fn is_path_call(tokens: &[Token], at: usize, path: &str, name: &str) -> bool {
    tokens[at].ident(path)
        && tokens.get(at + 1).is_some_and(|token| token.symbol(':'))
        && tokens.get(at + 2).is_some_and(|token| token.symbol(':'))
        && tokens.get(at + 3).is_some_and(|token| token.ident(name))
        && tokens.get(at + 4).is_some_and(|token| token.symbol('('))
}

/// `localization::format(`-style calls the direct pass already checked.
fn is_direct_contract_call(tokens: &[Token], at: usize) -> bool {
    at >= 3
        && tokens[at - 1].symbol(':')
        && tokens[at - 2].symbol(':')
        && ["localization", "sqlly_i18n", "DisplayMessage"]
            .iter()
            .any(|path| tokens[at - 3].ident(path))
}

fn placeholder_contract_files() -> Vec<(String, Vec<Token>)> {
    let mut files = source_tokens();
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates");
    let mut paths = Vec::new();
    rust_files(&crates, &mut paths);
    paths.sort();
    for path in paths {
        let source = std::fs::read_to_string(&path)
            .expect("read client crate source")
            .replace("\r\n", "\n");
        if source.contains("DisplayMessage") {
            files.push((relative_path(&path), shipped_tokens(&lex_rust(&source))));
        }
    }
    files
}

/// One formatted (or `text()`) use of catalog keys.
struct FormatUse {
    location: String,
    keys: Vec<String>,
    arguments: Arguments,
    /// `(file, let index)` when the key was chosen in a `let` that other
    /// calls in the same function share.
    group: Option<(String, usize)>,
    /// Read through `text()` (no formatting at all).
    text: bool,
}

#[derive(Default)]
struct ContractReport {
    uses: Vec<FormatUse>,
    unresolved: BTreeSet<(String, String, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CallKind {
    Format,
    Text,
    Message,
}

impl ContractReport {
    fn unresolved(&mut self, file: &str, function: String, reason: String) {
        self.unresolved.insert((file.to_string(), function, reason));
    }

    /// Record a call whose key and arguments resolved, or register the
    /// enclosing function as a wrapper when it forwards its parameters.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        wrappers: &mut Vec<FormatWrapper>,
        file: &str,
        context: &CallContext<'_>,
        location: String,
        keys: Resolved<Keys>,
        arguments: Resolved<Arguments>,
        kind: CallKind,
        report_unresolved: bool,
    ) {
        match (keys, arguments) {
            (Resolved::Value(keys), Resolved::Value(arguments)) => self.uses.push(FormatUse {
                location,
                keys: keys.keys,
                arguments,
                group: keys.binding.map(|binding| (file.to_string(), binding)),
                text: kind == CallKind::Text,
            }),
            (Resolved::ClosureParam(closure, key_param), arguments) => {
                let arguments = match arguments {
                    Resolved::Value(arguments) => WrapperArguments::Fixed(arguments),
                    Resolved::ClosureParam(ref argument_closure, index)
                        if *argument_closure == closure =>
                    {
                        WrapperArguments::Param(index)
                    }
                    _ => {
                        if report_unresolved {
                            self.unresolved(
                                file,
                                context.function(),
                                format!("arguments of closure `{closure}`"),
                            );
                        }
                        return;
                    }
                };
                let closure_fn = context.scope.as_ref().map(|scope| scope.start);
                if !wrappers.iter().any(|known| {
                    known.file == file
                        && known.name == closure
                        && known.key_param == key_param
                        && known.closure_fn == closure_fn
                }) {
                    wrappers.push(FormatWrapper {
                        file: file.to_string(),
                        name: closure,
                        public: false,
                        key_param,
                        arguments,
                        text: kind == CallKind::Text,
                        closure_fn: context.scope.as_ref().map(|scope| scope.start),
                    });
                }
            }
            (Resolved::Param(key_param), arguments) => {
                let arguments = match arguments {
                    Resolved::Value(arguments)
                        if kind == CallKind::Message && arguments == Arguments::default() =>
                    {
                        WrapperArguments::WithArgChain
                    }
                    Resolved::Value(arguments) => WrapperArguments::Fixed(arguments),
                    Resolved::Param(index) => WrapperArguments::Param(index),
                    Resolved::ClosureParam(..) | Resolved::Unresolved(_) => {
                        let reason = match arguments {
                            Resolved::Unresolved(reason) => reason,
                            _ => "arguments from a closure parameter".to_string(),
                        };
                        if report_unresolved {
                            self.unresolved(file, context.function(), reason);
                        }
                        return;
                    }
                };
                let scope = context
                    .scope
                    .as_ref()
                    .expect("a parameter implies a function");
                if !wrappers.iter().any(|known| {
                    known.file == file && known.name == scope.name && known.key_param == key_param
                }) {
                    wrappers.push(FormatWrapper {
                        file: file.to_string(),
                        name: scope.name.clone(),
                        public: scope.public,
                        key_param,
                        arguments,
                        text: kind == CallKind::Text,
                        closure_fn: None,
                    });
                }
            }
            (Resolved::Unresolved(reason), _) | (_, Resolved::Unresolved(reason)) => {
                if report_unresolved {
                    self.unresolved(file, context.function(), reason);
                }
            }
            (Resolved::Value(_), Resolved::Param(_) | Resolved::ClosureParam(..)) => {
                if report_unresolved {
                    self.unresolved(
                        file,
                        context.function(),
                        "arguments forwarded from a parameter with a fixed key".into(),
                    );
                }
            }
        }
    }
}

fn scan_placeholder_contract(
    files: &[(String, Vec<Token>)],
    catalog: &BTreeMap<String, String>,
) -> ContractReport {
    let mut report = ContractReport::default();
    let mut wrappers: Vec<FormatWrapper> = Vec::new();
    // Key functions and key tables are matched by name across files: keys
    // are often defined beside a type and read from another module.
    let mut functions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut tables: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (_, tokens) in files {
        for (name, keys) in key_functions(tokens, catalog) {
            functions.entry(name).or_default().extend(keys);
        }
        for (name, keys) in key_tables(tokens, catalog) {
            tables.entry(name).or_default().extend(keys);
        }
    }

    // Direct calls: format/deferred (key, arguments), text/deferred_text
    // (key), and `DisplayMessage::new(key, fallback).with_arg(…)` chains.
    for (relative, tokens) in files {
        if relative == "src/localization.rs" {
            continue;
        }
        let constants = local_key_bindings(tokens, false);
        for at in 0..tokens.len().saturating_sub(5) {
            let kind = if is_path_call(tokens, at, "localization", "format")
                || is_path_call(tokens, at, "localization", "deferred")
                || is_path_call(tokens, at, "sqlly_i18n", "format")
            {
                CallKind::Format
            } else if is_path_call(tokens, at, "localization", "text")
                || is_path_call(tokens, at, "localization", "deferred_text")
                || is_path_call(tokens, at, "sqlly_i18n", "text")
            {
                CallKind::Text
            } else if is_path_call(tokens, at, "DisplayMessage", "new") {
                CallKind::Message
            } else {
                continue;
            };
            let open = at + 4;
            let context = CallContext::new(tokens, at, catalog, &constants, &functions, &tables);
            let arguments = call_arguments(tokens, open);
            let Some(&key_range) = arguments.first() else {
                continue;
            };
            let location = format!("{relative}:{}", tokens[at].line);
            let names = match kind {
                CallKind::Format => match arguments.get(1) {
                    Some(&range) => {
                        if let Some(pairs) = context.paired(key_range, range) {
                            for (key, arguments) in pairs {
                                report.uses.push(FormatUse {
                                    location: location.clone(),
                                    keys: vec![key],
                                    arguments,
                                    group: None,
                                    text: false,
                                });
                            }
                            continue;
                        }
                        context.arguments(range)
                    }
                    None => Resolved::Unresolved("missing arguments".into()),
                },
                CallKind::Text => Resolved::Value(Arguments::default()),
                CallKind::Message => {
                    let end = call_end(tokens, open);
                    let mut arguments = Arguments::exact(with_arg_chain(tokens, end));
                    // `let mut message = DisplayMessage::new(…);` followed by
                    // conditional `message = message.with_arg(…)`.
                    let mut path_start = at;
                    while path_start >= 3
                        && tokens[path_start - 1].symbol(':')
                        && tokens[path_start - 2].symbol(':')
                        && matches!(tokens[path_start - 3].kind, TokenKind::Ident(_))
                    {
                        path_start -= 3;
                    }
                    if path_start >= 2
                        && tokens[path_start - 1].symbol('=')
                        && let TokenKind::Ident(binding) = &tokens[path_start - 2].kind
                    {
                        arguments.optional = later_with_args(tokens, end, binding);
                    }
                    Resolved::Value(arguments)
                }
            };
            let message = matches!(kind, CallKind::Message);
            // A DisplayMessage built from a runtime key (a descriptor passed
            // through) is not a catalog call site of its own.
            let keys = context.keys(key_range);
            let report_unresolved = !message;
            report.record(
                &mut wrappers,
                relative,
                &context,
                location,
                keys,
                names,
                kind,
                report_unresolved,
            );
        }
    }

    // Wrapper calls, to a fixpoint: a function forwarding its own parameters
    // to a wrapper becomes a wrapper itself. Private wrappers are only called
    // from their own file, so same-named functions elsewhere are unrelated.
    let mut processed = 0;
    while processed < wrappers.len() {
        let wrapper = wrappers[processed].clone();
        processed += 1;
        for (relative, tokens) in files {
            let same_file = *relative == wrapper.file;
            if !same_file && !wrapper.public {
                continue;
            }
            // A file's own fn or closure binding of the same name shadows a
            // public wrapper defined elsewhere (a private `localized` helper
            // is not `DetachedOptions::localized`).
            if !same_file
                && tokens.windows(2).any(|pair| {
                    (pair[0].ident("fn") || pair[0].ident("let")) && pair[1].ident(&wrapper.name)
                })
            {
                continue;
            }
            let constants = local_key_bindings(tokens, false);
            for at in 1..tokens.len().saturating_sub(1) {
                if !tokens[at].ident(&wrapper.name)
                    || !tokens[at + 1].symbol('(')
                    || tokens[at - 1].ident("fn")
                    || is_direct_contract_call(tokens, at)
                {
                    continue;
                }
                let context =
                    CallContext::new(tokens, at, catalog, &constants, &functions, &tables);
                if wrapper.closure_fn.is_some()
                    && (!same_file
                        || context.scope.as_ref().map(|scope| scope.start) != wrapper.closure_fn)
                {
                    continue;
                }
                let arguments = call_arguments(tokens, at + 1);
                let Some(&key_range) = arguments.get(wrapper.key_param) else {
                    continue;
                };
                let location = format!("{relative}:{}", tokens[at].line);
                let names = match &wrapper.arguments {
                    WrapperArguments::Fixed(arguments) => Resolved::Value(arguments.clone()),
                    WrapperArguments::WithArgChain => Resolved::Value(Arguments::exact(
                        with_arg_chain(tokens, call_end(tokens, at + 1)),
                    )),
                    WrapperArguments::Param(index) => match arguments.get(*index) {
                        Some(&range) => {
                            if let Some(pairs) = context.paired(key_range, range) {
                                for (key, arguments) in pairs {
                                    report.uses.push(FormatUse {
                                        location: location.clone(),
                                        keys: vec![key],
                                        arguments,
                                        group: None,
                                        text: false,
                                    });
                                }
                                continue;
                            }
                            context.arguments(range)
                        }
                        None => continue,
                    },
                };
                let keys = context.keys(key_range);
                // A same-named function elsewhere that passes no catalog key
                // is not this wrapper.
                let report_unresolved = same_file
                    || matches!(&keys, Resolved::Value(_))
                        && matches!(&names, Resolved::Unresolved(_));
                let mut reason_names = names;
                if let Resolved::Unresolved(reason) = &mut reason_names {
                    *reason = format!("{reason} via `{}`", wrapper.name);
                }
                let keys = match keys {
                    Resolved::Unresolved(reason) => {
                        Resolved::Unresolved(format!("{reason} via `{}`", wrapper.name))
                    }
                    other => other,
                };
                report.record(
                    &mut wrappers,
                    relative,
                    &context,
                    location,
                    keys,
                    reason_names,
                    if wrapper.text {
                        CallKind::Text
                    } else {
                        CallKind::Format
                    },
                    report_unresolved,
                );
            }
        }
    }
    report
}

/// Calls sharing one key binding (`let key = if … { "a" } else { "b" };`
/// followed by a `format` on one branch and a `text` on another) are checked
/// together: each key must match one of the calls exactly and each call one
/// of the keys. Any other call must match every key it can format.
fn placeholder_contract_violations(
    uses: &[FormatUse],
    catalog: &BTreeMap<String, String>,
    literal_braces: &BTreeSet<&str>,
) -> Vec<String> {
    let expected = |call: &FormatUse, key: &str, value: &str| {
        if call.text && literal_braces.contains(key) {
            BTreeSet::new()
        } else {
            catalog_placeholders(value)
        }
    };
    let mut violations = Vec::new();
    let mut groups: BTreeMap<(String, usize), Vec<&FormatUse>> = BTreeMap::new();
    for format_use in uses {
        match &format_use.group {
            Some(group) => groups.entry(group.clone()).or_default().push(format_use),
            None => groups
                .entry((format_use.location.clone(), usize::MAX))
                .or_default()
                .push(format_use),
        }
    }
    for (_, calls) in groups {
        let shared = calls.len() > 1;
        for call in &calls {
            for key in &call.keys {
                let Some(value) = catalog.get(key) else {
                    violations.push(format!("{}: unknown catalog key `{key}`", call.location));
                    continue;
                };
                // `text()` returns the value verbatim: `{{`/`}}` would show
                // doubled braces, so such a key must be formatted instead.
                if call.text && (value.contains("{{") || value.contains("}}")) {
                    violations.push(format!(
                        "{}: `{key}` is read through text() but its English value has escaped braces ({{{{ or }}}}) that only format() unescapes (en: {value:?})",
                        call.location
                    ));
                }
                let matched = if shared {
                    calls
                        .iter()
                        .any(|other| other.arguments.accepts(&expected(other, key, value)))
                } else {
                    call.arguments.accepts(&expected(call, key, value))
                };
                if !matched {
                    violations.push(format!(
                        "{}: `{key}` expects {:?} but the call passes {} (en: {value:?})",
                        call.location,
                        expected(call, key, value),
                        call.arguments.describe()
                    ));
                }
            }
            if shared
                && !call.keys.iter().any(|key| {
                    catalog
                        .get(key)
                        .is_some_and(|value| call.arguments.accepts(&expected(call, key, value)))
                })
            {
                violations.push(format!(
                    "{}: no key chosen here accepts the arguments {}",
                    call.location,
                    call.arguments.describe()
                ));
            }
        }
    }
    violations
}

#[test]
fn formatted_keys_match_their_call_site_arguments() {
    let catalog = catalog_values();
    let report = scan_placeholder_contract(&placeholder_contract_files(), &catalog);
    let checked: usize = report
        .uses
        .iter()
        .map(|format_use| format_use.keys.len())
        .sum();
    assert!(
        checked > 5000,
        "placeholder scan found only {checked} key uses"
    );
    let allowed: BTreeSet<(String, String)> = FORMAT_CONTRACT_UNRESOLVED
        .iter()
        .map(|(file, function, justification)| {
            assert!(
                !justification.trim().is_empty(),
                "{file}:{function} needs a justification"
            );
            ((*file).to_string(), (*function).to_string())
        })
        .collect();
    let literal_braces: BTreeSet<&str> = TEXT_LITERAL_BRACES
        .iter()
        .map(|(key, justification)| {
            assert!(
                !justification.trim().is_empty(),
                "{key} needs a justification"
            );
            *key
        })
        .collect();
    let mut violations = placeholder_contract_violations(&report.uses, &catalog, &literal_braces);
    for key in &literal_braces {
        if catalog
            .get(*key)
            .is_none_or(|value| catalog_placeholders(value).is_empty())
        {
            violations.push(format!(
                "stale TEXT_LITERAL_BRACES entry `{key}` (unknown or without braces)"
            ));
        }
    }
    let mut seen = BTreeSet::new();
    for (file, function, reason) in &report.unresolved {
        seen.insert((file.clone(), function.clone()));
        if !allowed.contains(&(file.clone(), function.clone())) {
            violations.push(format!(
                "{file} fn {function}: unresolved format call ({reason})"
            ));
        }
    }
    for (file, function) in allowed.difference(&seen) {
        violations.push(format!(
            "stale FORMAT_CONTRACT_UNRESOLVED entry ({file}, {function})"
        ));
    }
    violations.sort();
    violations.dedup();
    assert!(
        violations.is_empty(),
        "{} placeholder contract violations:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

#[test]
fn placeholder_contract_scanner_resolves_bindings_pairs_and_wrappers() {
    let catalog: BTreeMap<String, String> = [
        ("t.one", "One row"),
        ("t.many", "{count} rows"),
        ("t.named", "{name} at {server}"),
        ("t.bad", "{rows} rows affected"),
        ("t.plain", "Plain"),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value.to_string()))
    .collect();
    let source = r#"
        fn counted(count: usize) -> String {
            let key = if count == 1 { "t.one" } else { "t.many" };
            if count == 1 {
                crate::localization::text(key)
            } else {
                crate::localization::format(key, &[("count", count)])
            }
        }
        fn paired(flag: bool) -> String {
            let (key, arguments) = if flag {
                ("t.many", vec![("count", "1".to_string())])
            } else {
                ("t.plain", Vec::new())
            };
            let arguments = arguments.iter().map(|(n, v)| (*n, v.as_str())).collect::<Vec<_>>();
            crate::localization::format(key, &arguments)
        }
        fn layout(key: &'static str, arguments: &[(&str, &str)]) -> String {
            crate::localization::format(key, arguments)
        }
        fn uses() {
            layout("t.named", &[("name", "a"), ("server", "b")]);
            crate::localization::format("t.bad", &[("count", 3)]);
            crate::localization::text("t.many");
        }
    "#;
    let files = vec![(
        "src/example.rs".to_string(),
        shipped_tokens(&lex_rust(source)),
    )];
    let report = scan_placeholder_contract(&files, &catalog);
    assert!(report.unresolved.is_empty(), "{:?}", report.unresolved);
    let violations = placeholder_contract_violations(&report.uses, &catalog, &BTreeSet::new());
    assert_eq!(violations.len(), 2, "{violations:#?}");
    assert!(violations.iter().any(|line| line.contains("`t.bad`")));
    assert!(
        violations
            .iter()
            .any(|line| line.contains("`t.many` expects {\"count\"} but the call passes {}"))
    );
}
