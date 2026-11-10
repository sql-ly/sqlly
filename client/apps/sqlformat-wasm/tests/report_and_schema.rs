//! The JS-facing reports (`formatSqlReport`, `minifySqlReport`) and the
//! Advanced panel schema (`formatterSchema`), tested natively.

use serde_json::{Map, Value};
use sqlly_lang::sqlformat::{Preset, SqlFormattingPreferences};
use sqlly_lang::sqlmodel::Dialect;
use sqlly_sqlformat_wasm::report::{Class, highlight, position};
use sqlly_sqlformat_wasm::schema::{NOT_ON_THE_PAGE, PAGE_PRESETS, flatten, groups, schema_json};
use sqlly_sqlformat_wasm::{format, format_report, minify_report, resolve_prefs};

fn report(sql: &str, dialect: &str, options: &str) -> Value {
    serde_json::from_str(&format_report(sql, dialect, options).unwrap()).unwrap()
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

#[test]
fn formatted_report_carries_output_lines_changes_and_tokens() {
    let sql = "select a, b from t where x = 1";
    let r = report(sql, "postgresql", r#"{"keepShortQueriesOnOneLine":false}"#);
    assert_eq!(r["status"], "formatted");
    assert_eq!(
        r["output"],
        format(sql, "postgresql", r#"{"keepShortQueriesOnOneLine":false}"#).unwrap()
    );
    assert_eq!(r["dialect"], "postgresql");
    assert_eq!(r["lines"][0], 1);
    assert!(r["lines"][1].as_u64().unwrap() > 1);
    assert_eq!(r["changes"]["wordsRecased"], 3); // select, from, where
    assert_eq!(r["changes"]["namesRequoted"], 0);
    assert!(r.get("reason").is_none());
    assert_eq!(r["tokens"].as_array().unwrap().len() % 3, 0);
}

#[test]
fn already_formatted_sql_reports_unchanged_not_declined() {
    let once = format("select a from t", "tsql", "").unwrap();
    let r = report(&once, "tsql", "");
    assert_eq!(r["status"], "unchanged");
    assert!(r.get("reason").is_none());
    assert!(r.get("changes").is_none());
}

#[test]
fn declined_report_names_the_problem_and_where_it_is() {
    let sql = "select a,\n  'it''s fine', 'unterminated from t";
    let r = report(sql, "postgresql", "");
    assert_eq!(r["status"], "declined");
    assert_eq!(r["output"], sql);
    let reason = &r["reason"];
    assert_eq!(reason["code"], "lexError");
    assert_eq!(reason["kind"], "unterminatedString");
    assert_eq!(reason["line"], 2);
    assert_eq!(reason["column"], 17);

    let r = report("select `a` from [t]", "autoDetect", "");
    assert_eq!(r["reason"]["kind"], "illegalCharacter");
    assert_eq!(r["reason"]["character"], "`");
    assert_eq!(r["dialect"], "tsql", "auto-detect reports what it picked");

    let r = report("   ", "mysql", "");
    assert_eq!(r["reason"]["code"], "empty");
}

#[test]
fn error_offsets_are_utf16_for_the_browser() {
    // "é" is 2 bytes / 1 UTF-16 unit, "😀" 4 bytes / 2 units.
    let sql = "select 'é😀', \"x";
    let r = report(sql, "postgresql", "");
    let quote = sql.rfind('"').unwrap();
    assert_eq!(r["reason"]["offset"], sql[..quote].encode_utf16().count());
    assert_eq!(r["reason"]["column"], sql[..quote].chars().count() + 1);
    assert_eq!(position("ab\ncé\nd", 6), (2, 3, 5));
}

#[test]
fn changes_count_requoting_and_added_parentheses() {
    let r = report(
        "select [Name] from t where p = 1 and q = 2 or r = 3",
        "tsql",
        r#"{"identifierQuoting":"removeUnnecessaryQuotes","parenthesizeMixedBooleanLogic":true,"keepShortQueriesOnOneLine":false}"#,
    );
    assert_eq!(r["status"], "formatted");
    assert_eq!(r["changes"]["namesRequoted"], 1);
    assert_eq!(r["changes"]["parenthesesAdded"], 2);
}

#[test]
fn minify_report_matches_minify_and_explains_declines() {
    let r: Value =
        serde_json::from_str(&minify_report("select a,\n  b\nfrom t", "mysql").unwrap()).unwrap();
    assert_eq!(r["status"], "formatted");
    assert_eq!(r["output"], "select a, b from t");
    assert_eq!(r["lines"], serde_json::json!([3, 1]));
    let r: Value = serde_json::from_str(&minify_report("select 'open", "mysql").unwrap()).unwrap();
    assert_eq!(r["status"], "declined");
    assert_eq!(r["reason"]["kind"], "unterminatedString");
    assert!(minify_report("select 1", "db2").is_err());
}

#[test]
fn highlight_classes_and_utf16_offsets() {
    let sql = "SELECT COUNT(*), 'é' AS [x], @p -- c\nFROM t WHERE a >= 1.5";
    let spans = highlight(sql, Dialect::SqlServer);
    let utf16: Vec<u16> = sql.encode_utf16().collect();
    let mut seen = Vec::new();
    for s in spans.chunks(3) {
        let text = String::from_utf16(&utf16[s[1] as usize..s[2] as usize]).unwrap();
        seen.push((s[0], text));
    }
    let has = |class: Class, text: &str| seen.iter().any(|(c, t)| *c == class as u32 && t == text);
    assert!(has(Class::Keyword, "SELECT"), "{seen:?}");
    assert!(has(Class::Function, "COUNT"), "{seen:?}");
    assert!(has(Class::String, "'é'"), "{seen:?}");
    assert!(has(Class::QuotedName, "[x]"), "{seen:?}");
    assert!(has(Class::Param, "@p"), "{seen:?}");
    assert!(has(Class::Comment, "-- c"), "{seen:?}");
    assert!(has(Class::Operator, ">="), "{seen:?}");
    assert!(has(Class::Number, "1.5"), "{seen:?}");
    // Plain names carry no span.
    assert!(!seen.iter().any(|(_, t)| t == "t" || t == "a"), "{seen:?}");
    // Spans are ordered and in bounds.
    for w in spans.chunks(3).collect::<Vec<_>>().windows(2) {
        assert!(w[0][2] <= w[1][1]);
    }
    assert!(spans.chunks(3).all(|s| s[2] as usize <= utf16.len()));
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

fn schema_keys() -> Vec<&'static str> {
    groups()
        .iter()
        .flat_map(|g| g.fields.iter().map(|f| f.key))
        .collect()
}

#[test]
fn schema_covers_every_formatter_setting_exactly_once() {
    let keys = schema_keys();
    let mut sorted = keys.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), keys.len(), "a setting is listed twice");

    let all = flatten(&SqlFormattingPreferences::default());
    for key in all.keys() {
        let top = key.split('.').next().unwrap();
        if NOT_ON_THE_PAGE.contains(&top) && top != "keywordGroupCasing" {
            continue;
        }
        if key == "keywordGroupCasing" {
            continue;
        }
        assert!(
            keys.contains(&key.as_str()),
            "setting {key} is missing from the page schema"
        );
    }
    for key in &keys {
        assert!(
            all.contains_key(*key),
            "schema key {key} is not a formatter setting"
        );
    }
}

/// Builds the nested options object the page sends for one dotted key.
fn options_for(key: &str, value: Value) -> String {
    let mut obj = Map::new();
    match key.split_once('.') {
        Some((group, field)) => {
            let mut inner = Map::new();
            inner.insert(field.to_string(), value);
            obj.insert(group.to_string(), Value::Object(inner));
        }
        None => {
            obj.insert(key.to_string(), value);
        }
    }
    Value::Object(obj).to_string()
}

#[test]
fn every_schema_choice_is_accepted_by_the_formatter() {
    let schema = schema_json();
    for group in schema["groups"].as_array().unwrap() {
        assert!(group["label"].as_str().is_some_and(|l| !l.is_empty()));
        for field in group["fields"].as_array().unwrap() {
            let key = field["key"].as_str().unwrap();
            let values: Vec<Value> = match field["type"].as_str().unwrap() {
                "choice" => field["options"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|o| {
                        assert!(o["label"].as_str().is_some_and(|l| !l.is_empty()));
                        o["value"].clone()
                    })
                    .collect(),
                "toggle" => vec![Value::Bool(true), Value::Bool(false)],
                "number" => vec![field["min"].clone(), field["max"].clone()],
                "text" => vec![Value::String("fmt".into())],
                other => panic!("unknown field type {other}"),
            };
            for v in values {
                let opts = options_for(key, v.clone());
                let prefs =
                    resolve_prefs("tsql", &opts).unwrap_or_else(|e| panic!("{key}={v}: {e}"));
                assert_eq!(flatten(&prefs)[key], v, "{key} did not take {v}");
                format("select a from t", "tsql", &opts).unwrap();
            }
        }
    }
}

#[test]
fn schema_presets_are_the_formatters_presets() {
    let schema = schema_json();
    let keys = schema_keys();
    for preset in PAGE_PRESETS {
        let id = serde_json::to_value(preset).unwrap();
        let values = &schema["presets"][id.as_str().unwrap()];
        let expected = flatten(&SqlFormattingPreferences::for_preset(preset));
        assert_eq!(values.as_object().unwrap().len(), keys.len());
        for key in &keys {
            assert_eq!(values[*key], expected[*key], "{preset:?}.{key}");
        }
    }
    assert!(schema["presets"].get("custom").is_none());
    assert_eq!(PAGE_PRESETS[0], Preset::Default);
}

#[test]
fn preset_plus_overrides_from_the_schema_round_trip() {
    // What the page sends: a preset and only the keys that differ from it.
    let prefs = resolve_prefs(
        "mysql",
        r#"{"preset":"expanded","keywordCase":"lowercase","keywordGroupCasing":{"enabled":true,"ddl":"titleCase"},"maxLineLength":0}"#,
    )
    .unwrap();
    let flat = flatten(&prefs);
    let expanded = flatten(&SqlFormattingPreferences::for_preset(Preset::Expanded));
    assert_eq!(flat["keywordCase"], "lowercase");
    assert_eq!(flat["keywordGroupCasing.enabled"], true);
    assert_eq!(flat["keywordGroupCasing.ddl"], "titleCase");
    assert_eq!(
        flat["keywordGroupCasing.dml"],
        expanded["keywordGroupCasing.dml"]
    );
    assert_eq!(flat["maxLineLength"], 0);
    assert_eq!(flat["commaStyle"], expanded["commaStyle"]);
}
