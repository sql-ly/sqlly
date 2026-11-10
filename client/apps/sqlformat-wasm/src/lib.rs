//! SQLly's SQL formatter for the browser.
//!
//! The free online formatter at `sqlly.app/tools/sql-formatter` runs this
//! crate compiled to `wasm32-unknown-unknown`. It is a thin shell over
//! `sqlly_lang::sqlformat` — the same formatter the desktop app's Format SQL
//! command and the LSP's `textDocument/formatting` use — so the site formats
//! exactly like the app does.
//!
//! Options travel as JSON in the app's own `sql-formatting.json` shape
//! (camelCase keys, the same serde names), plus one extra key: `preset`
//! chooses the base bundle the other keys override. Everything here except
//! the `bindings` module is plain Rust, so it is unit-tested natively.
//!
//! Besides the plain `formatSql` / `minifySql` calls the page uses the
//! report variants ([`report`]): the output plus why it is unchanged when it
//! is, what changed when it is not, and the output's highlight tokens from
//! the formatter's own lexer. [`schema`] describes every layout setting for
//! the page's Advanced panel.

pub mod report;
pub mod schema;

use serde_json::{Map, Value};
use sqlly_lang::sqlformat::{
    Preset, SqlDialect, SqlFormattingPreferences, format_sql, format_sql_report, lex_dialect_for,
    minify,
};

/// Parse a dialect id (`"tsql"`, `"postgresql"`, `"autoDetect"`, …; the serde
/// names of [`SqlDialect`]). An empty id means auto-detect.
pub fn parse_dialect(id: &str) -> Result<SqlDialect, String> {
    let id = id.trim();
    if id.is_empty() {
        return Ok(SqlDialect::AutoDetect);
    }
    serde_json::from_value(Value::String(id.to_string()))
        .map_err(|_| format!("unknown dialect \"{id}\""))
}

/// Build the formatter preferences for `dialect` from an options object.
///
/// `options_json` is empty or a JSON object. Its optional `preset` key picks
/// the starting bundle ([`SqlFormattingPreferences::for_preset`]); every other
/// key overrides one setting of that bundle using the persisted setting names
/// (`keywordCase`, `indentStyle`, `indentWidth`, `commaStyle`, …). Unknown
/// keys are rejected rather than silently ignored so a typo in the page's
/// script surfaces instead of formatting with defaults.
pub fn resolve_prefs(
    dialect: &str,
    options_json: &str,
) -> Result<SqlFormattingPreferences, String> {
    let dialect = parse_dialect(dialect)?;
    let mut overrides = parse_options(options_json)?;

    let preset = match overrides.remove("preset") {
        None | Some(Value::Null) => Preset::Default,
        Some(v) => serde_json::from_value::<Preset>(v.clone())
            .map_err(|_| format!("unknown preset {v}"))?,
    };

    let base = SqlFormattingPreferences::for_preset(preset);
    let mut merged = match serde_json::to_value(&base) {
        Ok(Value::Object(map)) => map,
        _ => return Err("formatter preferences did not serialize to an object".into()),
    };

    for (key, value) in overrides {
        match merged.get_mut(&key) {
            None => return Err(format!("unknown option \"{key}\"")),
            // Nested groups (keywordGroupCasing) merge field by field.
            Some(Value::Object(existing)) => match value {
                Value::Object(inner) => existing.extend(inner),
                other => return Err(format!("option \"{key}\" must be an object, got {other}")),
            },
            Some(slot) => *slot = value,
        }
    }

    let mut prefs: SqlFormattingPreferences = serde_json::from_value(Value::Object(merged))
        .map_err(|e| format!("invalid formatter options: {e}"))?;
    // The explicit dialect argument wins over any `dialect` key.
    prefs.dialect = dialect;
    // Anything but a named bundle reads as Custom in the app; keep the marker
    // honest in case a caller serializes the result back out.
    let mut untouched = SqlFormattingPreferences::for_preset(preset);
    untouched.dialect = dialect;
    if prefs != untouched {
        prefs.preset = Preset::Custom;
    }
    Ok(prefs)
}

/// Format `sql` under `dialect` and `options_json` (see [`resolve_prefs`]).
///
/// Like the formatter itself this never corrupts SQL: input it cannot format
/// safely (an unterminated string, say) comes back unchanged. Only malformed
/// options are an error.
pub fn format(sql: &str, dialect: &str, options_json: &str) -> Result<String, String> {
    let prefs = resolve_prefs(dialect, options_json)?;
    Ok(format_sql(sql, &prefs, None))
}

/// Collapse `sql` onto as few lines as possible (the app's Minify SQL).
pub fn minify_sql(sql: &str, dialect: &str) -> Result<String, String> {
    let dialect = parse_dialect(dialect)?;
    Ok(minify(sql, lex_dialect_for(sql, dialect)))
}

/// [`format`] as a JSON report for the page (see
/// [`report::format_report_json`]).
pub fn format_report(sql: &str, dialect: &str, options_json: &str) -> Result<String, String> {
    let prefs = resolve_prefs(dialect, options_json)?;
    let report = format_sql_report(sql, &prefs, None);
    Ok(report::format_report_json(sql, &report).to_string())
}

/// [`minify_sql`] as a JSON report for the page.
pub fn minify_report(sql: &str, dialect: &str) -> Result<String, String> {
    let dialect = lex_dialect_for(sql, parse_dialect(dialect)?);
    let out = minify(sql, dialect);
    Ok(report::minify_report_json(sql, &out, dialect).to_string())
}

/// The Advanced panel's settings schema as JSON (see [`schema::schema_json`]).
pub fn schema_json() -> String {
    schema::schema_json().to_string()
}

fn parse_options(options_json: &str) -> Result<Map<String, Value>, String> {
    let trimmed = options_json.trim();
    if trimmed.is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(Value::Null) => Ok(Map::new()),
        Ok(other) => Err(format!(
            "formatter options must be a JSON object, got {other}"
        )),
        Err(e) => Err(format!("formatter options are not valid JSON: {e}")),
    }
}

/// The JavaScript surface, generated by wasm-bindgen. Present only on wasm.
#[cfg(target_arch = "wasm32")]
mod bindings {
    use wasm_bindgen::prelude::*;

    /// `formatSql(sql, dialect, optionsJson) -> string`; throws on bad options.
    #[wasm_bindgen(js_name = formatSql)]
    pub fn format_sql(sql: &str, dialect: &str, options_json: &str) -> Result<String, JsError> {
        super::format(sql, dialect, options_json).map_err(|e| JsError::new(&e))
    }

    /// `minifySql(sql, dialect) -> string`; throws on an unknown dialect.
    #[wasm_bindgen(js_name = minifySql)]
    pub fn minify_sql(sql: &str, dialect: &str) -> Result<String, JsError> {
        super::minify_sql(sql, dialect).map_err(|e| JsError::new(&e))
    }

    /// `formatSqlReport(sql, dialect, optionsJson) -> JSON string`: the
    /// output with its status, reason, changes and highlight tokens.
    #[wasm_bindgen(js_name = formatSqlReport)]
    pub fn format_sql_report(
        sql: &str,
        dialect: &str,
        options_json: &str,
    ) -> Result<String, JsError> {
        super::format_report(sql, dialect, options_json).map_err(|e| JsError::new(&e))
    }

    /// `minifySqlReport(sql, dialect) -> JSON string`.
    #[wasm_bindgen(js_name = minifySqlReport)]
    pub fn minify_sql_report(sql: &str, dialect: &str) -> Result<String, JsError> {
        super::minify_report(sql, dialect).map_err(|e| JsError::new(&e))
    }

    /// `formatterSchema() -> JSON string`: the Advanced panel's settings.
    #[wasm_bindgen(js_name = formatterSchema)]
    pub fn formatter_schema() -> String {
        super::schema_json()
    }

    /// Version of this wrapper crate, for the page footer / bug reports.
    #[wasm_bindgen(js_name = formatterVersion)]
    pub fn formatter_version() -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_lang::sqlformat::{CommaStyle, IndentStyle, KeywordCase};

    const QUERY: &str = "select id, name, email from customers c join orders o on o.customer_id = c.id where c.active = 1 and o.total > 100 order by name";

    #[test]
    fn every_dialect_id_the_page_offers_parses() {
        // Keep in sync with the dialect <select> in Pages/Tools/SqlFormatter.cshtml.
        for id in [
            "autoDetect",
            "tsql",
            "postgresql",
            "mysql",
            "sqlite",
            "oracle",
            "snowflake",
            "bigquery",
            "redshift",
        ] {
            assert!(parse_dialect(id).is_ok(), "{id}");
        }
        assert_eq!(parse_dialect("").unwrap(), SqlDialect::AutoDetect);
        assert!(parse_dialect("db2").is_err());
    }

    #[test]
    fn empty_options_are_the_app_defaults() {
        let prefs = resolve_prefs("tsql", "").unwrap();
        let expected = SqlFormattingPreferences {
            dialect: SqlDialect::Tsql,
            ..SqlFormattingPreferences::default()
        };
        assert_eq!(prefs, expected);
        assert_eq!(resolve_prefs("tsql", "{}").unwrap(), expected);
        assert_eq!(resolve_prefs("tsql", "null").unwrap(), expected);
    }

    #[test]
    fn preset_is_the_base_and_other_keys_override_it() {
        let prefs = resolve_prefs(
            "postgresql",
            r#"{"preset":"compact","keywordCase":"lowercase","indentStyle":"spaces","indentWidth":2,"commaStyle":"leading"}"#,
        )
        .unwrap();
        let compact = SqlFormattingPreferences::for_preset(Preset::Compact);
        assert_eq!(prefs.keyword_case, KeywordCase::Lowercase);
        assert_eq!(prefs.indent_style, IndentStyle::Spaces);
        assert_eq!(prefs.indent_width_raw, 2);
        assert_eq!(prefs.comma_style, CommaStyle::Leading);
        // Untouched settings keep the preset's value, not the global default.
        assert_eq!(prefs.join_layout, compact.join_layout);
        assert_eq!(prefs.preset, Preset::Custom);
        assert_eq!(prefs.dialect, SqlDialect::Postgresql);
    }

    #[test]
    fn a_bare_preset_keeps_its_marker() {
        let prefs = resolve_prefs("mysql", r#"{"preset":"expanded"}"#).unwrap();
        assert_eq!(prefs.preset, Preset::Expanded);
    }

    #[test]
    fn nested_group_casing_merges_field_by_field() {
        let prefs = resolve_prefs(
            "",
            r#"{"keywordGroupCasing":{"enabled":true,"dataTypes":"lowercase"}}"#,
        )
        .unwrap();
        assert!(prefs.keyword_group_casing.enabled);
        assert_eq!(
            prefs.keyword_group_casing.data_types,
            KeywordCase::Lowercase
        );
        assert_eq!(prefs.keyword_group_casing.clauses, KeywordCase::Uppercase);
    }

    #[test]
    fn bad_options_are_errors_not_silent_defaults() {
        assert!(resolve_prefs("tsql", r#"{"keywordCaes":"lowercase"}"#).is_err());
        assert!(resolve_prefs("tsql", r#"{"keywordCase":"shouty"}"#).is_err());
        assert!(resolve_prefs("tsql", r#"{"preset":"fancy"}"#).is_err());
        assert!(resolve_prefs("tsql", "[1,2]").is_err());
        assert!(resolve_prefs("tsql", "{not json").is_err());
        assert!(resolve_prefs("tsql", r#"{"keywordGroupCasing":3}"#).is_err());
    }

    #[test]
    fn formats_like_the_app() {
        let out = format(QUERY, "postgresql", "").unwrap();
        let prefs = resolve_prefs("postgresql", "").unwrap();
        assert_eq!(out, format_sql(QUERY, &prefs, None));
        assert!(out.contains("SELECT"), "{out}");
        assert!(out.lines().count() > 1, "{out}");
        // Idempotent, as the formatter's own corpus tests assert.
        assert_eq!(format(&out, "postgresql", "").unwrap(), out);
    }

    #[test]
    fn lowercase_option_is_applied() {
        let out = format(QUERY, "tsql", r#"{"keywordCase":"lowercase"}"#).unwrap();
        assert!(out.contains("select"), "{out}");
        assert!(!out.contains("SELECT"), "{out}");
    }

    #[test]
    fn broken_sql_comes_back_unchanged() {
        let broken = "select 'unterminated from t";
        assert_eq!(format(broken, "tsql", "").unwrap(), broken);
    }

    #[test]
    fn auto_detect_honours_tsql_brackets() {
        let out = format("select [Order Id] from [dbo].[Orders]", "autoDetect", "").unwrap();
        assert!(out.contains("[Order Id]"), "{out}");
    }

    #[test]
    fn minify_collapses_to_one_line() {
        let formatted = format(QUERY, "postgresql", "").unwrap();
        let min = minify_sql(&formatted, "postgresql").unwrap();
        assert!(!min.contains('\n'), "{min}");
        assert!(minify_sql(QUERY, "nope").is_err());
    }
}
