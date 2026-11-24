//! Sentence templates and number formatting shared by the plan story, the
//! derived metrics, the diagram model and the plan comparison.
//!
//! Every sentence the plan viewers show comes from one of the template
//! tables registered here. A template is the English catalog value
//! (`services.plans.<name>` in `sqlly-i18n`), so the fallback text of a
//! [`DisplayMessage`] and the catalog can never drift: both are formatted
//! from the same string.

use std::collections::HashMap;
use std::sync::LazyLock;

use sqlly_core::DisplayMessage;

/// One translatable sentence. `name` is the key below `services.plans.`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanPhrase {
    /// Full catalog key, `services.plans.<name>`.
    pub key: &'static str,
    /// English text with `{named}` placeholders.
    pub text: &'static str,
    /// Where the sentence appears, for translators.
    pub location: &'static str,
}

/// `phrase!("story.scan_table", "Scans {table}.", "where it appears")`.
macro_rules! phrase {
    ($name:literal, $text:literal, $location:literal $(,)?) => {
        $crate::plans::phrases::PlanPhrase {
            key: concat!("services.plans.", $name),
            text: $text,
            location: $location,
        }
    };
}
pub(crate) use phrase;

/// Every template table, for the catalog export and the lookup index.
pub fn all_plan_phrases() -> impl Iterator<Item = &'static PlanPhrase> {
    PHRASES
        .iter()
        .chain(crate::plans::analysis::PHRASES.iter())
        .chain(crate::plans::story::PHRASES.iter())
        .chain(crate::plans::story::STEP_PHRASES.iter())
        .chain(crate::plans::diagram::PHRASES.iter())
        .chain(crate::plans::projections::PHRASES.iter())
        .chain(crate::plans::comparison::PHRASES.iter())
}

fn index() -> &'static HashMap<&'static str, &'static PlanPhrase> {
    static INDEX: LazyLock<HashMap<&'static str, &'static PlanPhrase>> = LazyLock::new(|| {
        all_plan_phrases()
            .map(|phrase| {
                (
                    phrase
                        .key
                        .strip_prefix("services.plans.")
                        .unwrap_or(phrase.key),
                    phrase,
                )
            })
            .collect()
    });
    &INDEX
}

/// A placeholder value: plain text, or a message the UI translates before
/// placing it.
#[derive(Debug, Clone)]
pub(crate) enum Arg {
    Text(String),
    Message(DisplayMessage),
}

impl From<String> for Arg {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for Arg {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

impl From<&String> for Arg {
    fn from(value: &String) -> Self {
        Self::Text(value.clone())
    }
}

impl From<DisplayMessage> for Arg {
    fn from(value: DisplayMessage) -> Self {
        Self::Message(value)
    }
}

impl From<usize> for Arg {
    fn from(value: usize) -> Self {
        Self::Text(value.to_string())
    }
}

/// Build the message for template `name` (the key below `services.plans.`).
/// The English fallback is the template with its placeholders filled.
pub(crate) fn say(name: &str, args: Vec<(&'static str, Arg)>) -> DisplayMessage {
    let Some(phrase) = index().get(name) else {
        debug_assert!(false, "plan phrase `{name}` is not registered");
        return DisplayMessage::raw(name.to_string());
    };
    let mut fallback = String::with_capacity(phrase.text.len() + 32);
    let mut rest = phrase.text;
    while let Some(open) = rest.find('{') {
        fallback.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            fallback.push_str(&rest[open..]);
            rest = "";
            break;
        };
        let placeholder = &after[..close];
        match args.iter().find(|(arg, _)| *arg == placeholder) {
            Some((_, Arg::Text(text))) => fallback.push_str(text),
            Some((_, Arg::Message(message))) => fallback.push_str(message.fallback()),
            None => {
                debug_assert!(false, "plan phrase `{name}` got no `{placeholder}`");
            }
        }
        rest = &after[close + 1..];
    }
    fallback.push_str(rest);
    let mut message = DisplayMessage::new(phrase.key, fallback);
    for (arg, value) in args {
        message = match value {
            Arg::Text(text) => message.with_arg(arg, text),
            Arg::Message(nested) => message.with_message_arg(arg, nested),
        };
    }
    message
}

/// Message with no placeholders.
pub(crate) fn say0(name: &str) -> DisplayMessage {
    say(name, Vec::new())
}

// ---------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------

/// "no rows", "1 row", "8,210 rows", "1.25M rows" as a translatable phrase.
pub fn rows_phrase(rows: f64) -> DisplayMessage {
    let rounded = if rows.is_finite() { rows.max(0.0) } else { 0.0 };
    if rounded < 0.5 {
        say0("count.rows_zero")
    } else if rounded < 1.5 {
        say0("count.rows_one")
    } else {
        say("count.rows", vec![("count", human_number(rounded).into())])
    }
}

/// How plan numbers write the thousands and decimal separators.
///
/// Every number the plan viewers print (`8,210`, `1.25M`, `84.5 ms`, `7.1%`)
/// is built in English form and then passed through the style active on the
/// current thread. The default is English, so a host that never sets a
/// style (the online viewer, the tests) gets exactly the English text. The
/// desktop app sets the style of its interface language around plan
/// analysis with [`with_number_style`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumberStyle {
    /// Separator before the fraction: `.` in English, `,` in German.
    pub decimal: char,
    /// Separator between groups of thousands: `,` in English, `.` in
    /// German, a no-break space in French and Russian.
    pub group: char,
}

impl NumberStyle {
    /// `8,210` and `84.5`.
    pub const ENGLISH: Self = Self {
        decimal: '.',
        group: ',',
    };
}

impl Default for NumberStyle {
    fn default() -> Self {
        Self::ENGLISH
    }
}

thread_local! {
    static NUMBER_STYLE: std::cell::Cell<NumberStyle> =
        const { std::cell::Cell::new(NumberStyle::ENGLISH) };
}

/// The number style active on this thread.
pub fn number_style() -> NumberStyle {
    NUMBER_STYLE.with(std::cell::Cell::get)
}

/// Run `f` with `style` active on this thread, then put the previous style
/// back (also when `f` panics). Plans analysed on a background thread pass
/// the interface's style in, because the style is per thread.
pub fn with_number_style<R>(style: NumberStyle, f: impl FnOnce() -> R) -> R {
    struct Restore(NumberStyle);
    impl Drop for Restore {
        fn drop(&mut self) {
            NUMBER_STYLE.with(|cell| cell.set(self.0));
        }
    }
    let _restore = Restore(NUMBER_STYLE.with(|cell| cell.replace(style)));
    f()
}

/// Rewrite the separators of an English-formatted number (`1,250.5 ms`) in
/// the active style. Applied exactly once, by the public formatters below,
/// to text that holds only digits, separators and units.
pub(crate) fn styled(text: String) -> String {
    let style = number_style();
    if style == NumberStyle::ENGLISH {
        return text;
    }
    text.chars()
        .map(|ch| match ch {
            ',' => style.group,
            '.' => style.decimal,
            other => other,
        })
        .collect()
}

/// A count for a sentence: `8,210`, `50K`, `12.5K`, `1.25M`. Like
/// `human_count` without the trailing zeros (`50.0K`).
pub fn human_number(n: f64) -> String {
    let text = crate::plans::findings::human_count_plain(n);
    styled(match text.char_indices().last() {
        Some((at, suffix)) if suffix.is_ascii_alphabetic() => {
            format!("{}{suffix}", trim_zeros(&text[..at]))
        }
        _ => text,
    })
}

/// Milliseconds as a duration a person reads at a glance: `0.42 ms`,
/// `3.4 ms`, `84 ms`, `1.24 s`, `2 min 5 s`, `1 h 12 min`.
pub fn human_duration_ms(ms: f64) -> String {
    styled(duration_ms_plain(ms))
}

fn duration_ms_plain(ms: f64) -> String {
    if !ms.is_finite() || ms <= 0.0 {
        // A measured zero is a duration below the engine's clock resolution.
        return "<1 ms".to_string();
    }
    if ms < 0.001 {
        return "<0.001 ms".to_string();
    }
    if ms < 1.0 {
        return format!("{} ms", trim_zeros(&format!("{ms:.3}")));
    }
    if ms < 10.0 {
        return format!("{} ms", trim_zeros(&format!("{ms:.2}")));
    }
    if ms < 100.0 {
        return format!("{} ms", trim_zeros(&format!("{ms:.1}")));
    }
    if ms < 999.5 {
        return format!("{ms:.0} ms");
    }
    let seconds = ms / 1000.0;
    if seconds < 10.0 {
        return format!("{} s", trim_zeros(&format!("{seconds:.2}")));
    }
    if seconds < 59.95 {
        return format!("{} s", trim_zeros(&format!("{seconds:.1}")));
    }
    let total_seconds = seconds.round() as u64;
    if total_seconds < 3600 {
        let (minutes, rest) = (total_seconds / 60, total_seconds % 60);
        return if rest == 0 {
            format!("{minutes} min")
        } else {
            format!("{minutes} min {rest} s")
        };
    }
    let total_minutes = (seconds / 60.0).round() as u64;
    let (hours, minutes) = (total_minutes / 60, total_minutes % 60);
    if minutes == 0 {
        format!("{hours} h")
    } else {
        format!("{hours} h {minutes} min")
    }
}

/// A 0..1 share as a percentage: `0%`, `<1%`, `7%`, `42%`, `>99%`, `100%`.
pub fn human_share(share: f64) -> String {
    if !share.is_finite() || share <= 0.0 {
        return "0%".to_string();
    }
    let percent = share * 100.0;
    if percent < 0.5 {
        "<1%".to_string()
    } else if (99.5..100.0).contains(&percent) && share < 0.99999 {
        ">99%".to_string()
    } else if percent < 10.0 && (percent - percent.round()).abs() >= 0.05 {
        styled(format!("{percent:.1}%"))
    } else {
        format!("{:.0}%", percent.min(100.0))
    }
}

/// Kilobytes as `512 kB`, `9.2 MB`, `1.4 GB`.
pub fn human_size_kb(kb: f64) -> String {
    if !kb.is_finite() || kb <= 0.0 {
        return "0 kB".to_string();
    }
    if kb < 1024.0 {
        return format!("{kb:.0} kB");
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return styled(format!("{} MB", trim_zeros(&format!("{mb:.1}"))));
    }
    let gb = mb / 1024.0;
    styled(format!("{} GB", trim_zeros(&format!("{gb:.1}"))))
}

/// An optimizer cost: grouped integer for large values, more decimals the
/// smaller the number, so SQL Server's `0.0033` and PostgreSQL's `9968.44`
/// both stay meaningful.
pub fn human_cost(cost: f64) -> String {
    if !cost.is_finite() || cost <= 0.0 {
        return "0".to_string();
    }
    styled(if cost >= 10_000.0 {
        crate::plans::findings::human_count_plain(cost)
    } else if cost >= 100.0 {
        format!("{cost:.0}")
    } else if cost >= 1.0 {
        trim_zeros(&format!("{cost:.2}"))
    } else if cost >= 0.01 {
        trim_zeros(&format!("{cost:.3}"))
    } else {
        trim_zeros(&format!("{cost:.5}"))
    })
}

/// How many times larger one number is than another: `2.5`, `14`, `1,250`,
/// `30,000`. Written out in full, because "30.0K times" does not read as a
/// number of times.
pub fn human_factor(factor: f64) -> String {
    if !factor.is_finite() || factor <= 0.0 {
        return "0".to_string();
    }
    styled(if factor < 10.0 {
        trim_zeros(&format!("{factor:.1}"))
    } else {
        crate::plans::findings::whole_factor_plain(factor)
    })
}

fn trim_zeros(text: &str) -> String {
    if text.contains('.') {
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        text.to_string()
    }
}

/// Shorten `text` to at most `max` characters, ending in an ellipsis when it
/// was cut. Cuts on a character boundary.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.truncate(out.trim_end().len());
    out.push('\u{2026}');
    out
}

pub(crate) static PHRASES: &[PlanPhrase] = &[
    phrase!(
        "count.rows",
        "{count} rows",
        "Execution Plan → row counts inside sentences: a number of rows, two or more; {count} is a formatted number such as 8,210 or 1.25M. It is placed inside other plan sentences, for example 'returns {count} rows'."
    ),
    phrase!(
        "count.rows_one",
        "1 row",
        "Execution Plan → row counts inside sentences: exactly one row. It is placed inside other plan sentences, for example 'returns 1 row'."
    ),
    phrase!(
        "count.rows_zero",
        "no rows",
        "Execution Plan → row counts inside sentences: zero rows. It is placed inside other plan sentences, for example 'returns no rows'."
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A host that sets a style gets its separators everywhere a plan prints
    /// a number; the default, and the text after the scope ends, stay
    /// English, which is what the online viewer relies on.
    #[test]
    fn numbers_follow_the_active_style_and_default_to_english() {
        let german = NumberStyle {
            decimal: ',',
            group: '.',
        };
        assert_eq!(human_number(1760.0), "1,760");
        let styled = with_number_style(german, || {
            (
                human_number(1760.0),
                human_number(6_880_000.0),
                human_duration_ms(57.8),
                human_share(0.071),
                human_cost(9968.44),
                human_factor(37.4),
                human_size_kb(9420.0),
                crate::plans::findings::human_count(50_000.0),
                rows_phrase(8210.0).fallback().to_string(),
            )
        });
        assert_eq!(
            styled,
            (
                "1.760".to_string(),
                "6,88M".to_string(),
                "57,8 ms".to_string(),
                "7,1%".to_string(),
                "9968".to_string(),
                "37".to_string(),
                "9,2 MB".to_string(),
                "50,0K".to_string(),
                "8.210 rows".to_string(),
            )
        );
        assert_eq!(number_style(), NumberStyle::ENGLISH);
        assert_eq!(human_duration_ms(57.8), "57.8 ms");
        assert_eq!(human_number(6_880_000.0), "6.88M");
    }

    #[test]
    fn factors_are_written_out_in_full() {
        assert_eq!(human_factor(2.54), "2.5");
        assert_eq!(human_factor(3.0), "3");
        assert_eq!(human_factor(14.2), "14");
        assert_eq!(human_factor(1_250.0), "1,250");
        assert_eq!(human_factor(10_000.0), "10,000");
        assert_eq!(human_factor(30_000.0), "30,000");
        assert_eq!(human_factor(123_456.0), "123,000");
        assert_eq!(human_factor(1_234_567.0), "1,230,000");
    }

    #[test]
    fn durations_read_naturally() {
        // A measured zero is a timer that did not tick, not no time at all.
        assert_eq!(human_duration_ms(0.0), "<1 ms");
        assert_eq!(human_duration_ms(0.0004), "<0.001 ms");
        assert_eq!(human_duration_ms(0.042), "0.042 ms");
        assert_eq!(human_duration_ms(0.42), "0.42 ms");
        assert_eq!(human_duration_ms(3.4), "3.4 ms");
        assert_eq!(human_duration_ms(84.31), "84.3 ms");
        assert_eq!(human_duration_ms(840.0), "840 ms");
        assert_eq!(human_duration_ms(999.7), "1 s");
        assert_eq!(human_duration_ms(1240.0), "1.24 s");
        assert_eq!(human_duration_ms(42_300.0), "42.3 s");
        assert_eq!(human_duration_ms(125_000.0), "2 min 5 s");
        assert_eq!(human_duration_ms(120_000.0), "2 min");
        assert_eq!(human_duration_ms(4_320_000.0), "1 h 12 min");
    }

    #[test]
    fn shares_sizes_costs_and_factors() {
        assert_eq!(human_share(0.0), "0%");
        assert_eq!(human_share(0.001), "<1%");
        assert_eq!(human_share(0.074), "7.4%");
        assert_eq!(human_share(0.07), "7%");
        assert_eq!(human_share(0.4249), "42%");
        assert_eq!(human_share(1.3), "100%");
        assert_eq!(human_size_kb(512.0), "512 kB");
        assert_eq!(human_size_kb(9420.0), "9.2 MB");
        assert_eq!(human_size_kb(1_500_000.0), "1.4 GB");
        assert_eq!(human_cost(9968.44), "9968");
        assert_eq!(human_cost(67277.54), "67.3K");
        assert_eq!(human_cost(3.09639), "3.1");
        assert_eq!(human_cost(0.0721276), "0.072");
        assert_eq!(human_cost(0.0032831), "0.00328");
        assert_eq!(human_factor(2.54), "2.5");
        assert_eq!(human_factor(14.2), "14");
        assert_eq!(human_factor(1250.0), "1,250");
    }

    #[test]
    fn row_phrases_handle_zero_one_and_many() {
        assert_eq!(rows_phrase(0.0).fallback(), "no rows");
        assert_eq!(rows_phrase(1.0).fallback(), "1 row");
        assert_eq!(rows_phrase(12.0).fallback(), "12 rows");
        assert_eq!(rows_phrase(1_250_000.0).fallback(), "1.25M rows");
        assert_eq!(rows_phrase(12.0).key(), Some("services.plans.count.rows"));
    }

    #[test]
    fn say_fills_placeholders_and_keeps_arguments() {
        let message = say("count.rows", vec![("count", "42".into())]);
        assert_eq!(message.fallback(), "42 rows");
        assert_eq!(
            message.arguments().collect::<Vec<_>>(),
            vec![("count", "42")]
        );
    }

    #[test]
    fn truncate_cuts_on_characters() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("a long predicate here", 8), "a long\u{2026}");
        assert_eq!(truncate("ééééé", 3), "éé\u{2026}");
    }

    /// The catalog rules of AGENTS.md, checked on the source of truth.
    #[test]
    fn phrases_are_valid_catalog_entries() {
        let mut keys = HashSet::new();
        for phrase in all_plan_phrases() {
            assert!(keys.insert(phrase.key), "duplicate phrase {}", phrase.key);
            for segment in phrase.key.split('.') {
                assert!(
                    segment.starts_with(|ch: char| ch.is_ascii_lowercase())
                        && segment
                            .chars()
                            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
                        && !segment.ends_with('_')
                        && !segment.contains("__"),
                    "bad key segment in {}",
                    phrase.key
                );
            }
            assert!(!phrase.text.trim().is_empty(), "{} is empty", phrase.key);
            // The same rule the catalog build enforces: a UI path, a colon,
            // then what the text is.
            let (path, detail) = phrase
                .location
                .split_once(':')
                .unwrap_or_else(|| panic!("{}: location needs `path: detail`", phrase.key));
            assert!(
                phrase.location.starts_with("Execution Plan \u{2192} ")
                    && phrase.location.chars().count() >= 40
                    && path.split(" \u{2192} ").all(|part| !part.trim().is_empty())
                    && detail.trim().chars().count() >= 12,
                "{}: location must describe the surface",
                phrase.key
            );
            assert!(
                !phrase.text.contains(['\u{2014}', '\u{2013}']),
                "{}: no dashes in user-facing text",
                phrase.key
            );
            // Placeholders are well formed and named in snake_case.
            let mut rest = phrase.text;
            while let Some(open) = rest.find('{') {
                let after = &rest[open + 1..];
                let close = after
                    .find('}')
                    .unwrap_or_else(|| panic!("{}: unclosed placeholder", phrase.key));
                let name = &after[..close];
                assert!(
                    !name.is_empty() && name.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_'),
                    "{}: bad placeholder `{name}`",
                    phrase.key
                );
                rest = &after[close + 1..];
            }
            assert!(!rest.contains('}'), "{}: stray brace", phrase.key);
        }
    }

    /// Every table-driven catalog entry the plan modules render: `(key,
    /// English value, location)`. The phrase tables, the operator families,
    /// the operator glossary and the finding strings. Keys written out as
    /// whole literals (errors, schema notes, comparison labels) are found by
    /// [`shipped_plan_key_literals`] instead.
    fn catalog_entries() -> Vec<(String, String, String)> {
        use crate::plans::glossary::{OperatorCategory, all_operator_glossaries};
        let mut entries: Vec<(String, String, String)> = all_plan_phrases()
            .map(|phrase| {
                (
                    phrase.key.to_string(),
                    phrase.text.to_string(),
                    phrase.location.to_string(),
                )
            })
            .collect();
        for category in OperatorCategory::ALL {
            let label = category.label();
            entries.push((
                label.key().expect("category key").to_string(),
                label.fallback().to_string(),
                format!(
                    "Execution Plan \u{2192} Operator legend, filters and breakdown: name of the operator family `{}`.",
                    category.key()
                ),
            ));
        }
        for (engine, operators) in all_operator_glossaries() {
            for entry in operators {
                let name = entry.display_name();
                entries.push((
                    entry.description_key.to_string(),
                    entry.description.to_string(),
                    format!(
                        "Execution Plan \u{2192} Operator details: one line on what the {engine} operator `{name}` does."
                    ),
                ));
                entries.push((
                    entry.performance_key.to_string(),
                    entry.performance.to_string(),
                    format!(
                        "Execution Plan \u{2192} Operator details: when the {engine} operator `{name}` is fine and when it is a problem."
                    ),
                ));
            }
        }
        entries.push((
            "services.plans.glossary.unknown.description".into(),
            "{operator}: this operator is not in the glossary yet.".into(),
            "Execution Plan \u{2192} Operator details: shown for an operator the glossary has no entry for; {operator} is the engine's operator name.".into(),
        ));
        entries.push((
            "services.plans.glossary.unknown.performance".into(),
            "Judge it by its share of the plan's time or cost and by its row counts.".into(),
            "Execution Plan \u{2192} Operator details: performance advice for an operator the glossary has no entry for.".into(),
        ));
        entries.extend(crate::plans::findings::finding_catalog_entries());
        entries
    }

    /// Whole `"services.plans.…"` string literals in this crate's shipped
    /// sources, as `(key, file:line)`. Test files, `#[cfg(test)]` items and
    /// comment lines are skipped, so a key a test merely mentions does not
    /// count as emitted. Prefixes ending in `.`
    /// (the table macros' `concat!` heads) are not keys and are skipped.
    fn shipped_plan_key_literals() -> Vec<(String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read source dir") {
                let path = entry.expect("dir entry").path();
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if path.is_dir() {
                    if name != "tests" {
                        walk(&path, out);
                    }
                } else if name.ends_with(".rs")
                    && name != "tests.rs"
                    && !name.ends_with("_tests.rs")
                {
                    out.push(path);
                }
            }
        }
        const OPEN: &str = "\"services.plans.";
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&root, &mut files);
        files.sort();
        let mut found = Vec::new();
        for path in files {
            let source = std::fs::read_to_string(&path).expect("read source");
            let relative = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            // Inside a `#[cfg(test)]` item: `Some(None)` until the item's
            // first brace, then `Some(Some(depth))` until its braces close.
            // An item without braces ends at its `;`.
            let mut in_test: Option<Option<i64>> = None;
            for (number, line) in source.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed == "#[cfg(test)]" {
                    in_test = Some(None);
                    continue;
                }
                if let Some(state) = in_test {
                    let braces =
                        line.matches('{').count() as i64 - line.matches('}').count() as i64;
                    in_test = match state {
                        None if trimmed.starts_with("#[") || trimmed.is_empty() => Some(None),
                        None if line.contains('{') => (braces > 0).then_some(Some(braces)),
                        None if trimmed.ends_with(';') => None,
                        None => Some(None),
                        Some(depth) => (depth + braces > 0).then_some(Some(depth + braces)),
                    };
                    continue;
                }
                if trimmed.starts_with("//") {
                    continue;
                }
                let mut rest = line;
                while let Some(open) = rest.find(OPEN) {
                    let body = &rest[open + 1..];
                    let end = body.find('"').unwrap_or(body.len());
                    let key = &body[..end];
                    if !key.ends_with('.')
                        && key.chars().all(|ch| {
                            ch.is_ascii_lowercase() || ch.is_ascii_digit() || "._".contains(ch)
                        })
                    {
                        found.push((key.to_string(), format!("{relative}:{}", number + 1)));
                    }
                    rest = &body[end..];
                    rest = rest.strip_prefix('"').unwrap_or(rest);
                }
            }
        }
        found
    }

    fn english_plan_keys(catalog: &serde_json::Value) -> std::collections::BTreeSet<String> {
        fn walk(
            node: &serde_json::Value,
            prefix: &str,
            out: &mut std::collections::BTreeSet<String>,
        ) {
            let Some(object) = node.as_object() else {
                return;
            };
            if object
                .get("value")
                .is_some_and(serde_json::Value::is_string)
            {
                out.insert(prefix.to_string());
                return;
            }
            for (segment, child) in object {
                walk(child, &format!("{prefix}.{segment}"), out);
            }
        }
        let mut keys = std::collections::BTreeSet::new();
        walk(&catalog["services"]["plans"], "services.plans", &mut keys);
        keys
    }

    /// The reverse of [`the_english_catalog_holds_every_plan_phrase`]: every
    /// `services.plans.*` entry in the English catalog is one the plan core
    /// can emit, from a table or as a whole literal, and every such literal
    /// is in the catalog. An orphaned catalog entry fails here, so removing
    /// a phrase or finding string means removing its catalog entries too.
    /// The GPUI translation-coverage lint relies on this test (by name) for
    /// its `services.plans.` allowance.
    #[test]
    fn every_english_plan_key_is_emitted_by_the_core() {
        let catalog = english_catalog();
        let in_catalog = english_plan_keys(&catalog);
        let literals = shipped_plan_key_literals();
        assert!(
            literals
                .iter()
                .any(|(key, _)| key == "services.plans.errors.empty_plan"),
            "the literal scan must find the empty-plan error key"
        );
        let mut problems = Vec::new();
        for (key, at) in &literals {
            if !in_catalog.contains(key) {
                problems.push(format!("{at}: `{key}` is not in en.json"));
            }
        }
        let emitted: std::collections::BTreeSet<String> = catalog_entries()
            .into_iter()
            .map(|(key, _, _)| key)
            .chain(literals.into_iter().map(|(key, _)| key))
            .collect();
        for key in in_catalog.difference(&emitted) {
            problems.push(format!("en.json `{key}` is not emitted by the plan core"));
        }
        assert!(
            problems.is_empty(),
            "{} plan catalog problems:\n{}",
            problems.len(),
            problems.join("\n")
        );
    }

    fn english_catalog() -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../sqlly-i18n/translations/en.json");
        let text = std::fs::read_to_string(path).expect("en.json reads");
        serde_json::from_str(&text).expect("en.json parses")
    }

    /// Writes the entries as JSON to `PLAN_CATALOG_OUT`, for the script that
    /// adds new keys to the translation catalogs.
    #[test]
    #[ignore = "catalog export helper"]
    fn dump_plan_catalog() {
        let Ok(out) = std::env::var("PLAN_CATALOG_OUT") else {
            return;
        };
        let entries: Vec<serde_json::Value> = catalog_entries()
            .into_iter()
            .map(|(key, value, location)| {
                serde_json::json!({ "key": key, "value": value, "location": location })
            })
            .collect();
        std::fs::write(out, serde_json::to_string_pretty(&entries).expect("json")).expect("write");
    }

    /// The fallbacks compiled into this crate and the English catalog are
    /// the same text, so a translated build and an untranslated one agree.
    #[test]
    fn the_english_catalog_holds_every_plan_phrase() {
        let catalog = english_catalog();
        let mut keys = HashSet::new();
        let mut problems = Vec::new();
        for (key, value, location) in catalog_entries() {
            assert!(keys.insert(key.clone()), "duplicate catalog key {key}");
            let leaf = key
                .split('.')
                .try_fold(&catalog, |node, segment| node.get(segment));
            match leaf {
                None => problems.push(format!("missing {key}")),
                Some(leaf) => {
                    if leaf.get("value").and_then(|found| found.as_str()) != Some(value.as_str()) {
                        problems.push(format!("value differs for {key}"));
                    }
                    if leaf.get("location").and_then(|found| found.as_str())
                        != Some(location.as_str())
                    {
                        problems.push(format!("location differs for {key}"));
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "{} catalog problems, first: {:?}",
            problems.len(),
            &problems[..problems.len().min(8)]
        );
    }
}
