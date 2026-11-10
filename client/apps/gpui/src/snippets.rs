//! User SQL snippets (LSP tab-stop subset) and shortcut-bound quick actions.
//!
//! Snippet bodies are stored raw. Parsing happens only on accept / prefix
//! expand — never on the IntelliSense `complete()` hot path.
//!
//! Grammar (authoritative): https://github.com/Microsoft/language-server-protocol/blob/main/snippetSyntax.md
//! Supported: `$n`, `${n}`, `${n:default}`, `$0` as the final stop, and
//! `\$` / `\}` / `\\` escapes. Rejected: choices `${1|a,b|}`, variables
//! `$TM_*`, transforms `${1/regex/fmt/}`, nested placeholders. `$0` must
//! not carry a default (https://github.com/microsoft/language-server-protocol/pull/2087).

use std::collections::BTreeMap;
use std::ops::Range;

use serde::{Deserialize, Serialize};

/// Parsed snippet: literal text plus stop ranges (byte offsets into `text`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSnippet {
    pub text: String,
    pub stops: Vec<SnippetStop>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetStop {
    pub number: u32,
    pub ranges: Vec<Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnippetError {
    UnterminatedPlaceholder(usize),
    FinalStopHasDefault,
    Unsupported(&'static str),
}

impl std::fmt::Display for SnippetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnterminatedPlaceholder(at) => crate::localization::format(
                "settings.editor.snippets.error.unterminated_placeholder",
                &[("byte", &at.to_string())],
            ),
            Self::FinalStopHasDefault => {
                crate::localization::text("settings.editor.snippets.error.final_stop_has_default")
            }
            Self::Unsupported(kind) => crate::localization::format(
                "settings.editor.snippets.error.unsupported_syntax",
                &[("kind", kind)],
            ),
        };
        f.write_str(&message)
    }
}

/// Fast scan for an unescaped `$` followed by a digit or `{`+digit.
pub fn has_tab_stops(body: &str) -> bool {
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' {
            i += if i + 1 < b.len() { 2 } else { 1 };
            continue;
        }
        if b[i] == b'$' {
            if i + 1 < b.len() && b[i + 1].is_ascii_digit() {
                return true;
            }
            if i + 2 < b.len() && b[i + 1] == b'{' && b[i + 2].is_ascii_digit() {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Render defaults and drop markers. On a parse error, returns `body` unchanged.
pub fn strip_tab_stops(body: &str) -> String {
    parse_snippet(body)
        .map(|parsed| parsed.text)
        .unwrap_or_else(|_| body.to_string())
}

pub fn parse_snippet(body: &str) -> Result<ParsedSnippet, SnippetError> {
    let bytes = body.as_bytes();
    let mut text = String::new();
    let mut by_number: BTreeMap<u32, Vec<Range<usize>>> = BTreeMap::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => match bytes[i + 1] {
                b'$' | b'}' | b'\\' => {
                    text.push(bytes[i + 1] as char);
                    i += 2;
                }
                _ => {
                    text.push('\\');
                    i += 1;
                }
            },
            b'$' => {
                i = parse_dollar(bytes, i, &mut text, &mut by_number)?;
            }
            _ => {
                let ch = body[i..].chars().next().expect("valid utf-8");
                text.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Ok(ParsedSnippet {
        text,
        stops: collect_stops(by_number),
    })
}

fn parse_dollar(
    bytes: &[u8],
    at: usize,
    text: &mut String,
    by_number: &mut BTreeMap<u32, Vec<Range<usize>>>,
) -> Result<usize, SnippetError> {
    let next = at + 1;
    if next >= bytes.len() {
        text.push('$');
        return Ok(next);
    }
    if bytes[next].is_ascii_digit() {
        let (number, after) = read_ascii_u32(bytes, next);
        push_stop(by_number, number, text.len()..text.len());
        return Ok(after);
    }
    if bytes[next] == b'{' {
        return parse_braced(bytes, at, text, by_number);
    }
    if bytes[next].is_ascii_alphabetic() || bytes[next] == b'_' {
        return Err(SnippetError::Unsupported("variable"));
    }
    text.push('$');
    Ok(next)
}

fn parse_braced(
    bytes: &[u8],
    at: usize,
    text: &mut String,
    by_number: &mut BTreeMap<u32, Vec<Range<usize>>>,
) -> Result<usize, SnippetError> {
    // `at` points at `$`; `{` is `at + 1`.
    let inner = at + 2;
    if inner >= bytes.len() {
        return Err(SnippetError::UnterminatedPlaceholder(at));
    }
    if bytes[inner].is_ascii_alphabetic() || bytes[inner] == b'_' {
        return Err(SnippetError::Unsupported("variable"));
    }
    if !bytes[inner].is_ascii_digit() {
        return Err(SnippetError::UnterminatedPlaceholder(at));
    }
    let (number, after_num) = read_ascii_u32(bytes, inner);
    if after_num >= bytes.len() {
        return Err(SnippetError::UnterminatedPlaceholder(at));
    }
    match bytes[after_num] {
        b'}' => {
            push_stop(by_number, number, text.len()..text.len());
            Ok(after_num + 1)
        }
        b'|' => Err(SnippetError::Unsupported("choice")),
        b'/' => Err(SnippetError::Unsupported("transform")),
        b':' => {
            if number == 0 {
                return Err(SnippetError::FinalStopHasDefault);
            }
            parse_placeholder_default(bytes, at, after_num + 1, number, text, by_number)
        }
        _ => Err(SnippetError::UnterminatedPlaceholder(at)),
    }
}

fn parse_placeholder_default(
    bytes: &[u8],
    at: usize,
    mut i: usize,
    number: u32,
    text: &mut String,
    by_number: &mut BTreeMap<u32, Vec<Range<usize>>>,
) -> Result<usize, SnippetError> {
    let start = text.len();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => match bytes[i + 1] {
                b'$' | b'}' | b'\\' => {
                    text.push(bytes[i + 1] as char);
                    i += 2;
                }
                _ => {
                    text.push('\\');
                    i += 1;
                }
            },
            b'$' if i + 1 < bytes.len() && bytes[i + 1] == b'{' => {
                return Err(SnippetError::Unsupported("nested"));
            }
            b'}' => {
                push_stop(by_number, number, start..text.len());
                return Ok(i + 1);
            }
            _ => {
                // Copy one UTF-8 scalar so multibyte defaults stay on char boundaries.
                let rest = std::str::from_utf8(&bytes[i..]).unwrap_or("");
                let Some(ch) = rest.chars().next() else {
                    break;
                };
                text.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Err(SnippetError::UnterminatedPlaceholder(at))
}

fn read_ascii_u32(bytes: &[u8], mut i: usize) -> (u32, usize) {
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let n = std::str::from_utf8(&bytes[start..i])
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (n, i)
}

fn push_stop(by_number: &mut BTreeMap<u32, Vec<Range<usize>>>, number: u32, range: Range<usize>) {
    by_number.entry(number).or_default().push(range);
}

fn collect_stops(by_number: BTreeMap<u32, Vec<Range<usize>>>) -> Vec<SnippetStop> {
    let mut numbered: Vec<SnippetStop> = by_number
        .into_iter()
        .map(|(number, mut ranges)| {
            ranges.sort_by_key(|r| r.start);
            SnippetStop { number, ranges }
        })
        .collect();
    numbered.sort_by(|a, b| match (a.number, b.number) {
        (0, 0) => std::cmp::Ordering::Equal,
        (0, _) => std::cmp::Ordering::Greater,
        (_, 0) => std::cmp::Ordering::Less,
        (x, y) => x.cmp(&y),
    });
    numbered
}

pub struct QuickActionContext<'a> {
    pub selection: Option<&'a str>,
    pub word_at_cursor: &'a str,
    pub database: Option<&'a str>,
}

/// Substitute `${selection}` / `${table}` / `${database}`. Unknown `${name}`
/// placeholders are left literal. Not LSP — no tab stops.
pub fn render_quick_action(body: &str, ctx: &QuickActionContext) -> String {
    let fallback = ctx.selection.unwrap_or(ctx.word_at_cursor);
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        let Some(end) = rest.find('}') else {
            out.push_str("${");
            out.push_str(rest);
            return out;
        };
        let name = &rest[..end];
        rest = &rest[end + 1..];
        match name {
            "selection" | "table" => out.push_str(fallback),
            "database" => out.push_str(ctx.database.unwrap_or("")),
            _ => {
                out.push_str("${");
                out.push_str(name);
                out.push('}');
            }
        }
    }
    out.push_str(rest);
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UserSnippet {
    pub prefix: String,
    pub body: String,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QuickActionTarget {
    #[default]
    ReplaceSelection,
    NewTab,
    RunInNewTab,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct QuickAction {
    pub slot: u8,
    pub title: String,
    pub body: String,
    pub target: QuickActionTarget,
}

impl Default for QuickAction {
    fn default() -> Self {
        Self {
            slot: 0,
            title: String::new(),
            body: String::new(),
            target: QuickActionTarget::ReplaceSelection,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SnippetPreferences {
    pub version: u32,
    pub snippets: Vec<UserSnippet>,
    pub quick_actions: Vec<QuickAction>,
}

impl SnippetPreferences {
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return crate::preferences::wasm_load_json("snippets-preferences.json");
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = snippets_prefs_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        return crate::preferences::wasm_save_json("snippets-preferences.json", self);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = snippets_prefs_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = std::fs::write(path, data);
            }
        }
    }

    pub fn command_id(slot: u8) -> String {
        format!("quickAction.slot{slot}")
    }

    pub fn quick_action_title(slot: u8, title: &str) -> String {
        if slot == 1 {
            return crate::localization::text("command_palette.command.quick_action.slot1");
        }
        crate::localization::format(
            "settings.snippets.message.quick_action",
            &[("title", title)],
        )
    }

    pub fn slot_from_command_id(id: &str) -> Option<u8> {
        let rest = id.strip_prefix("quickAction.slot")?;
        let slot: u8 = rest.parse().ok()?;
        (1..=9).contains(&slot).then_some(slot)
    }
}

/// Removes trailing commas before `}` / `]` outside string literals (JSONC
/// allows them, serde_json does not).
fn strip_trailing_commas(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for (i, &c) in chars.iter().enumerate() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        } else if c == ','
            && chars[i + 1..]
                .iter()
                .find(|n| !n.is_whitespace())
                .is_some_and(|n| *n == '}' || *n == ']')
        {
            continue;
        }
        out.push(c);
    }
    out
}

/// VS Code snippet `scope` languages that SQLly's editor can use.
const SQL_SCOPES: &[&str] = &["sql", "tsql", "mssql", "pgsql", "mysql", "plsql"];

fn string_or_lines(value: Option<&serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// Reads a VS Code / Azure Data Studio snippet file (`sql.json` or
/// `*.code-snippets`, JSONC). An array `prefix` yields one snippet per
/// prefix; snippets scoped only to non-SQL languages are skipped.
pub fn import_vscode_snippets(text: &str) -> Result<Vec<UserSnippet>, String> {
    let cleaned = strip_trailing_commas(&sqlly_services::strip_jsonc(text));
    let root: serde_json::Value = serde_json::from_str(&cleaned).map_err(|e| {
        crate::localization::format(
            "settings.editor.snippets.vscode.invalid_file",
            &[("error", &e.to_string())],
        )
    })?;
    let Some(entries) = root.as_object() else {
        return Err(crate::localization::text(
            "settings.editor.snippets.vscode.not_an_object",
        ));
    };
    let mut out = Vec::new();
    for (name, entry) in entries {
        if let Some(scope) = entry.get("scope").and_then(serde_json::Value::as_str)
            && !scope.trim().is_empty()
            && !scope
                .split(',')
                .any(|s| SQL_SCOPES.contains(&s.trim().to_ascii_lowercase().as_str()))
        {
            continue;
        }
        let body = string_or_lines(entry.get("body")).join("\n");
        let description = entry
            .get("description")
            .and_then(serde_json::Value::as_str)
            .filter(|d| !d.trim().is_empty())
            .unwrap_or(name.as_str())
            .to_string();
        for prefix in string_or_lines(entry.get("prefix")) {
            if prefix.trim().is_empty() {
                continue;
            }
            out.push(UserSnippet {
                prefix,
                body: body.clone(),
                description: description.clone(),
            });
        }
    }
    Ok(out)
}

/// Writes snippets as a VS Code `.code-snippets` file scoped to `sql`, which
/// VS Code and Azure Data Studio both load.
pub fn export_vscode_snippets(snippets: &[UserSnippet]) -> String {
    let mut root = serde_json::Map::new();
    for snippet in snippets {
        let base = if snippet.description.trim().is_empty() {
            snippet.prefix.clone()
        } else {
            snippet.description.clone()
        };
        let mut key = base.clone();
        let mut n = 2;
        while root.contains_key(&key) {
            key = format!("{base} ({n})");
            n += 1;
        }
        let body: Vec<serde_json::Value> = snippet
            .body
            .split('\n')
            .map(|line| serde_json::Value::String(line.to_string()))
            .collect();
        root.insert(
            key,
            serde_json::json!({
                "scope": "sql",
                "prefix": snippet.prefix,
                "body": body,
                "description": snippet.description,
            }),
        );
    }
    let mut out = serde_json::to_string_pretty(&serde_json::Value::Object(root))
        .unwrap_or_else(|_| "{}".into());
    out.push('\n');
    out
}

/// Appends imported snippets whose prefix isn't already taken (case-
/// insensitive, as prefix expansion matches). Returns (added, skipped).
pub fn merge_imported_snippets(
    existing: &mut Vec<UserSnippet>,
    imported: Vec<UserSnippet>,
) -> (usize, usize) {
    let mut taken: std::collections::HashSet<String> =
        existing.iter().map(|s| s.prefix.to_lowercase()).collect();
    let (mut added, mut skipped) = (0, 0);
    for snippet in imported {
        if taken.insert(snippet.prefix.to_lowercase()) {
            existing.push(snippet);
            added += 1;
        } else {
            skipped += 1;
        }
    }
    (added, skipped)
}

#[cfg(test)]
mod vscode_tests {
    use super::*;

    #[test]
    fn imports_jsonc_with_comments_arrays_and_scopes() {
        let text = r#"{
            // Azure Data Studio user snippets
            "Select top": {
                "prefix": ["sel", "seltop"],
                "body": ["SELECT TOP ${1:10} *", "FROM ${2:table} -- see https://example.com/x", "$0"],
                "description": "Select top rows",
            },
            /* block comment */
            "Python only": { "scope": "python", "prefix": "py", "body": "print()" },
            "Mixed scope": { "scope": "javascript, SQL", "prefix": "cnt", "body": "SELECT COUNT(*) FROM $1" },
        }"#;
        let mut snippets = import_vscode_snippets(text).unwrap();
        snippets.sort_by(|a, b| a.prefix.cmp(&b.prefix));
        assert_eq!(snippets.len(), 3);
        assert_eq!(snippets[0].prefix, "cnt");
        assert_eq!(snippets[0].description, "Mixed scope");
        assert_eq!(snippets[1].prefix, "sel");
        assert_eq!(snippets[2].prefix, "seltop");
        assert_eq!(
            snippets[1].body,
            "SELECT TOP ${1:10} *\nFROM ${2:table} -- see https://example.com/x\n$0"
        );
        assert_eq!(snippets[1].description, "Select top rows");
        assert!(parse_snippet(&snippets[1].body).is_ok());
    }

    #[test]
    fn rejects_non_object_files() {
        assert!(import_vscode_snippets("[1, 2]").is_err());
        assert!(import_vscode_snippets("{ nope").is_err());
    }

    #[test]
    fn export_round_trips_and_dedupes_keys() {
        let snippets = vec![
            UserSnippet {
                prefix: "ins".into(),
                body: "INSERT INTO $1 ($2)\nVALUES ($0)".into(),
                description: "Insert".into(),
            },
            UserSnippet {
                prefix: "ins2".into(),
                body: "INSERT INTO $1 DEFAULT VALUES".into(),
                description: "Insert".into(),
            },
            UserSnippet {
                prefix: "x".into(),
                body: "SELECT 1".into(),
                description: String::new(),
            },
        ];
        let text = export_vscode_snippets(&snippets);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["Insert"]["body"][1], "VALUES ($0)");
        assert_eq!(value["Insert (2)"]["prefix"], "ins2");
        assert_eq!(value["x"]["scope"], "sql");
        let mut back = import_vscode_snippets(&text).unwrap();
        back.sort_by(|a, b| a.prefix.cmp(&b.prefix));
        let mut expected = snippets.clone();
        expected[2].description = "x".into();
        assert_eq!(back, expected);
    }

    #[test]
    fn merge_skips_taken_prefixes() {
        let mut existing = vec![UserSnippet {
            prefix: "Sel".into(),
            ..Default::default()
        }];
        let imported = vec![
            UserSnippet {
                prefix: "sel".into(),
                ..Default::default()
            },
            UserSnippet {
                prefix: "upd".into(),
                ..Default::default()
            },
            UserSnippet {
                prefix: "upd".into(),
                ..Default::default()
            },
        ];
        assert_eq!(merge_imported_snippets(&mut existing, imported), (1, 2));
        assert_eq!(existing.len(), 2);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn snippets_prefs_path() -> std::path::PathBuf {
    crate::preferences::preferences_path().join("snippets-preferences.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop_at(stops: &[SnippetStop], number: u32) -> &SnippetStop {
        stops
            .iter()
            .find(|s| s.number == number)
            .unwrap_or_else(|| panic!("missing stop {number}"))
    }

    #[test]
    fn parses_plain_text_without_stops() {
        let parsed = parse_snippet("SELECT 1").unwrap();
        assert_eq!(parsed.text, "SELECT 1");
        assert!(parsed.stops.is_empty());
    }

    #[test]
    fn parses_numbered_tabstops_in_order() {
        let parsed = parse_snippet("$2 and $1").unwrap();
        assert_eq!(parsed.text, " and ");
        assert_eq!(parsed.stops.len(), 2);
        assert_eq!(parsed.stops[0].number, 1);
        assert_eq!(parsed.stops[0].ranges, vec![5..5]);
        assert_eq!(parsed.stops[1].number, 2);
        assert_eq!(parsed.stops[1].ranges, vec![0..0]);
    }

    #[test]
    fn parses_placeholder_defaults() {
        let parsed = parse_snippet("TOP ${1:100} FROM ${2:t}").unwrap();
        assert_eq!(parsed.text, "TOP 100 FROM t");
        assert_eq!(stop_at(&parsed.stops, 1).ranges, vec![4..7]);
        assert_eq!(stop_at(&parsed.stops, 2).ranges, vec![13..14]);
    }

    #[test]
    fn mirrors_repeated_numbers() {
        let parsed = parse_snippet("${1:a} = ${1:a}").unwrap();
        assert_eq!(parsed.text, "a = a");
        assert_eq!(parsed.stops.len(), 1);
        assert_eq!(parsed.stops[0].number, 1);
        assert_eq!(parsed.stops[0].ranges, vec![0..1, 4..5]);
    }

    #[test]
    fn final_stop_sorts_last() {
        let parsed = parse_snippet("$0 $1").unwrap();
        assert_eq!(
            parsed.stops.iter().map(|s| s.number).collect::<Vec<_>>(),
            vec![1, 0]
        );
    }

    #[test]
    fn unescapes_dollar_and_brace() {
        let parsed = parse_snippet("\\$1 \\}").unwrap();
        assert_eq!(parsed.text, "$1 }");
        assert!(parsed.stops.is_empty());
    }

    #[test]
    fn rejects_choice_variable_transform_nested() {
        assert_eq!(
            parse_snippet("${1|a,b|}"),
            Err(SnippetError::Unsupported("choice"))
        );
        assert_eq!(
            parse_snippet("$TM_SELECTED_TEXT"),
            Err(SnippetError::Unsupported("variable"))
        );
        assert_eq!(
            parse_snippet("${1/regex/fmt/}"),
            Err(SnippetError::Unsupported("transform"))
        );
        assert_eq!(
            parse_snippet("${1:hello ${2:x}}"),
            Err(SnippetError::Unsupported("nested"))
        );
    }

    #[test]
    fn rejects_default_on_final_stop() {
        assert_eq!(
            parse_snippet("${0:x}"),
            Err(SnippetError::FinalStopHasDefault)
        );
    }

    #[test]
    fn unterminated_placeholder_errors() {
        assert_eq!(
            parse_snippet("${1:abc"),
            Err(SnippetError::UnterminatedPlaceholder(0))
        );
    }

    #[test]
    fn has_tab_stops_is_cheap_and_correct() {
        assert!(has_tab_stops("$1"));
        assert!(has_tab_stops("${2:x}"));
        assert!(!has_tab_stops("$"));
        assert!(!has_tab_stops("\\$1"));
        assert!(!has_tab_stops("price$"));
    }

    #[test]
    fn strip_tab_stops_renders_defaults() {
        assert_eq!(
            strip_tab_stops("SELECT TOP ${1:100} FROM ${2:t}$0"),
            "SELECT TOP 100 FROM t"
        );
    }

    #[test]
    fn quick_action_substitutes_selection_table_database() {
        let ctx = QuickActionContext {
            selection: Some("Orders"),
            word_at_cursor: "ignored",
            database: Some("Sales"),
        };
        assert_eq!(
            render_quick_action(
                "EXEC sp_help '${table}'; USE ${database}; -- ${selection}",
                &ctx
            ),
            "EXEC sp_help 'Orders'; USE Sales; -- Orders"
        );
    }

    #[test]
    fn quick_action_falls_back_to_word_at_cursor() {
        let ctx = QuickActionContext {
            selection: None,
            word_at_cursor: "Customers",
            database: None,
        };
        assert_eq!(
            render_quick_action("SELECT COUNT(*) FROM ${table}", &ctx),
            "SELECT COUNT(*) FROM Customers"
        );
        assert_eq!(render_quick_action("USE ${database}", &ctx), "USE ");
    }

    #[test]
    fn quick_action_leaves_unknown_placeholders_literal() {
        let ctx = QuickActionContext {
            selection: Some("x"),
            word_at_cursor: "x",
            database: None,
        };
        assert_eq!(
            render_quick_action("SELECT ${foo} FROM ${selection}", &ctx),
            "SELECT ${foo} FROM x"
        );
    }

    #[test]
    fn preferences_round_trip_json() {
        let prefs = SnippetPreferences {
            version: 1,
            snippets: vec![UserSnippet {
                prefix: "sel".into(),
                body: "SELECT ${1:100}".into(),
                description: "top".into(),
            }],
            quick_actions: vec![QuickAction {
                slot: 1,
                title: "Describe table".into(),
                body: "EXEC sp_help '${table}'".into(),
                target: QuickActionTarget::NewTab,
            }],
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let back: SnippetPreferences = serde_json::from_str(&json).unwrap();
        assert_eq!(back, prefs);

        let defaults: SnippetPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults, SnippetPreferences::default());
    }

    #[test]
    fn command_id_round_trip() {
        assert_eq!(SnippetPreferences::command_id(3), "quickAction.slot3");
        assert_eq!(
            SnippetPreferences::slot_from_command_id("quickAction.slot3"),
            Some(3)
        );
        assert_eq!(
            SnippetPreferences::slot_from_command_id("quickAction.slot0"),
            None
        );
        assert_eq!(
            SnippetPreferences::slot_from_command_id("quickAction.slot10"),
            None
        );
        assert_eq!(
            SnippetPreferences::slot_from_command_id("query.execute"),
            None
        );
    }

    #[test]
    fn quick_action_titles_use_the_builtin_slot_and_generic_user_title() {
        let _guard = crate::localization::test_language(crate::localization::Language::English);
        assert_eq!(
            SnippetPreferences::quick_action_title(1, "ignored"),
            "Quick action: Describe table"
        );
        for slot in 2..=9 {
            assert_eq!(
                SnippetPreferences::quick_action_title(slot, "Custom action"),
                "Quick action: Custom action"
            );
        }
    }
}
