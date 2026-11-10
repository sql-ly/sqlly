//! UI translations compiled from `translations/*.json` with per-thread runtime
//! overrides. The build script emits values only: location descriptions remain
//! in the source catalogs for translation tooling and never enter app data.

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::HashMap,
    fmt::{Display, Write as _},
    hash::{BuildHasherDefault, Hasher},
    sync::OnceLock,
};

mod catalog;

include!(concat!(env!("OUT_DIR"), "/compiled_catalogs.rs"));

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    #[default]
    English,
    Spanish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanguageInfo {
    pub language: Language,
    pub code: &'static str,
    pub native_name: &'static str,
}

pub const LANGUAGES: &[LanguageInfo] = &[
    LanguageInfo {
        language: Language::English,
        code: "en",
        native_name: "English",
    },
    LanguageInfo {
        language: Language::Spanish,
        code: "es",
        native_name: "Español",
    },
];

#[derive(Debug, Default)]
struct TranslationState {
    language: Language,
    overrides: HashMap<String, String>,
}

thread_local! {
    /// GPUI renders on one UI thread. Thread-local state avoids a process-wide
    /// lock on the rendering path and isolates native test runners.
    static STATE: RefCell<TranslationState> = RefCell::new(TranslationState::default());
}

impl Language {
    pub fn from_code(code: &str) -> Option<Self> {
        LANGUAGES
            .iter()
            .find(|info| info.code.eq_ignore_ascii_case(code.trim()))
            .map(|info| info.language)
    }

    pub fn code(self) -> &'static str {
        LANGUAGES
            .iter()
            .find(|info| info.language == self)
            .expect("registered language")
            .code
    }

    pub fn native_name(self) -> &'static str {
        LANGUAGES
            .iter()
            .find(|info| info.language == self)
            .expect("registered language")
            .native_name
    }

    fn defaults(self) -> &'static [&'static str; KEY_COUNT] {
        match self {
            Self::English => EN,
            Self::Spanish => ES,
        }
    }
}

pub fn current_language() -> Language {
    STATE.with(|state| state.borrow().language)
}

/// Switch the active language and drop every runtime override. Overrides are
/// written for one language, so keeping them across a switch would show
/// language A's custom strings inside language B. Callers that want overrides
/// for the new language apply them afterwards (or use [`activate`]).
pub fn set_language(language: Language) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.language = language;
        state.overrides.clear();
    });
}

pub fn set_language_code(code: &str) -> Result<Language, String> {
    let language = Language::from_code(code)
        .ok_or_else(|| format!("Unsupported language '{}'. Expected en or es.", code.trim()))?;
    set_language(language);
    Ok(language)
}

/// Resolve one key without file I/O or JSON parsing. Overrides use an in-memory
/// hash lookup; compiled defaults use binary search over sorted static data.
/// Unknown keys are returned verbatim so missing catalog entries stay visible.
pub fn text(key: &str) -> String {
    with_template(key, |template| template.unwrap_or(key).to_owned())
}

/// Like [`text`], but `None` for keys missing from the catalog so a caller
/// with its own fallback (the results-grid translator) needs one lookup, not a
/// separate [`contains_key`] check followed by [`text`].
pub fn try_text(key: &str) -> Option<String> {
    with_template(key, |template| template.map(str::to_owned))
}

/// [`try_text`] without allocating for compiled values: borrowed from the
/// static catalog, owned only when an override supplies the text. One lookup.
pub fn try_text_cow(key: &str) -> Option<Cow<'static, str>> {
    STATE.with(|state| {
        let state = state.borrow();
        if !state.overrides.is_empty()
            && let Some(value) = state.overrides.get(key)
        {
            return Some(Cow::Owned(value.clone()));
        }
        compiled_value(state.language, key).map(Cow::Borrowed)
    })
}

/// The compiled value for the active language without allocating, or `None`
/// when the key is unknown or currently overridden (call [`text`] then).
pub fn text_static(key: &str) -> Option<&'static str> {
    STATE.with(|state| {
        let state = state.borrow();
        if !state.overrides.is_empty() && state.overrides.contains_key(key) {
            return None;
        }
        compiled_value(state.language, key)
    })
}

/// Run `resolve` with the active template for `key` (override first, then the
/// compiled value) borrowed from the translation state, so lookups allocate at
/// most once for the caller's result.
fn with_template<R>(key: &str, resolve: impl FnOnce(Option<&str>) -> R) -> R {
    STATE.with(|state| {
        let state = state.borrow();
        if !state.overrides.is_empty()
            && let Some(value) = state.overrides.get(key)
        {
            return resolve(Some(value));
        }
        resolve(compiled_value(state.language, key))
    })
}

/// FxHash-style word-at-a-time hasher for the compiled key index. The keys
/// are trusted, build-time constants, so SipHash's DoS resistance buys nothing
/// on this per-lookup path; this hashes a long dotted key several times faster.
#[derive(Default, Clone, Copy)]
struct KeyHasher(u64);

impl KeyHasher {
    const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(Self::SEED);
    }
}

impl Hasher for KeyHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while let Some((chunk, rest)) = bytes.split_first_chunk::<8>() {
            self.add(u64::from_le_bytes(*chunk));
            bytes = rest;
        }
        if !bytes.is_empty() {
            let mut tail = [0u8; 8];
            tail[..bytes.len()].copy_from_slice(bytes);
            self.add(u64::from_le_bytes(tail));
        }
    }

    #[inline]
    fn write_u8(&mut self, byte: u8) {
        self.add(u64::from(byte));
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

type KeyIndex = HashMap<&'static str, u32, BuildHasherDefault<KeyHasher>>;

/// Position of `key` in every compiled catalog. The build script emits one
/// sorted `KEYS` table and a same-length value table per language, so one
/// index serves every language by construction.
fn key_index(key: &str) -> Option<usize> {
    static INDEX: OnceLock<KeyIndex> = OnceLock::new();
    INDEX
        .get_or_init(|| {
            KEYS.iter()
                .enumerate()
                .map(|(index, key)| (*key, index as u32))
                .collect()
        })
        .get(key)
        .map(|index| *index as usize)
}

pub fn compiled_value(language: Language, key: &str) -> Option<&'static str> {
    key_index(key).map(|index| language.defaults()[index])
}

pub fn contains_key(key: &str) -> bool {
    key_index(key).is_some()
}

/// The active language and overrides, captured so a caller can put them back
/// after a multi-step change (activate, then save) fails part-way.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    language: Language,
    overrides: HashMap<String, String>,
}

impl Snapshot {
    pub fn language(&self) -> Language {
        self.language
    }
}

pub fn snapshot() -> Snapshot {
    STATE.with(|state| {
        let state = state.borrow();
        Snapshot {
            language: state.language,
            overrides: state.overrides.clone(),
        }
    })
}

/// Restore a [`snapshot`] exactly: language and overrides together.
pub fn restore(snapshot: Snapshot) {
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.language = snapshot.language;
        state.overrides = snapshot.overrides;
    });
}

/// Replace both language and overrides only after the complete override file
/// has passed validation. A failure leaves the previous pair untouched.
pub fn activate(language: Language, overrides_json: Option<&str>) -> Result<(), String> {
    let overrides = match overrides_json {
        Some(json) => parse_overrides(json)?,
        None => HashMap::new(),
    };
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.language = language;
        state.overrides = overrides;
    });
    Ok(())
}

/// Replace runtime overrides with a partial catalog for the active language.
/// The nested value/location shape and validation match compiled catalogs.
pub fn set_overrides_json(json: &str) -> Result<(), String> {
    let overrides = parse_overrides(json)?;
    STATE.with(|state| state.borrow_mut().overrides = overrides);
    Ok(())
}

/// One override entry that [`set_overrides_json_lenient`] left out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedOverride {
    pub key: String,
    pub reason: String,
}

impl std::fmt::Display for SkippedOverride {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.key, self.reason)
    }
}

/// Replace runtime overrides from a host-pushed catalog (the website
/// translation manager), tolerating version skew between that catalog and
/// this build: keys this build does not know, and entries whose placeholders
/// differ from English, are skipped and reported instead of rejecting the
/// whole push. Malformed JSON or catalog shape is still an error that leaves
/// the current overrides untouched. Custom files loaded from Settings stay
/// strict through [`activate`] / [`set_overrides_json`].
pub fn set_overrides_json_lenient(json: &str) -> Result<Vec<SkippedOverride>, String> {
    let (overrides, skipped) = parse_overrides_lenient(json)?;
    STATE.with(|state| state.borrow_mut().overrides = overrides);
    Ok(skipped)
}

/// Lenient [`activate`]: switch language and apply every valid entry of
/// `overrides_json`, returning the skipped ones (unknown keys, placeholder
/// mismatches). Used at startup so one renamed key in a saved custom file
/// does not discard the whole file. Malformed JSON is an error that leaves
/// the previous language and overrides untouched.
pub fn activate_lenient(
    language: Language,
    overrides_json: &str,
) -> Result<Vec<SkippedOverride>, String> {
    let (overrides, skipped) = parse_overrides_lenient(overrides_json)?;
    STATE.with(|state| {
        let mut state = state.borrow_mut();
        state.language = language;
        state.overrides = overrides;
    });
    Ok(skipped)
}

pub fn clear_overrides() {
    STATE.with(|state| state.borrow_mut().overrides.clear());
}

fn parse_overrides(json: &str) -> Result<HashMap<String, String>, String> {
    let (overrides, skipped) = parse_overrides_lenient(json)?;
    match skipped.into_iter().next() {
        Some(skipped) if skipped.reason == UNKNOWN_KEY => {
            Err(format!("Unknown translation key '{}'", skipped.key))
        }
        Some(skipped) => Err(format!(
            "Translation '{}' does not match English: {}",
            skipped.key, skipped.reason
        )),
        None => Ok(overrides),
    }
}

const UNKNOWN_KEY: &str = "unknown translation key";

fn parse_overrides_lenient(
    json: &str,
) -> Result<(HashMap<String, String>, Vec<SkippedOverride>), String> {
    let entries = catalog::parse(json)?;
    let mut overrides = HashMap::with_capacity(entries.len());
    let mut skipped = Vec::new();
    for (key, value) in entries {
        let Some(english) = compiled_value(Language::English, &key) else {
            skipped.push(SkippedOverride {
                key,
                reason: UNKNOWN_KEY.into(),
            });
            continue;
        };
        if let Some(reason) = catalog::placeholder_mismatch(english, &value) {
            skipped.push(SkippedOverride { key, reason });
            continue;
        }
        overrides.insert(key, value);
    }
    Ok((overrides, skipped))
}

/// Format a complete translated message with named `{placeholder}` arguments.
/// Double braces emit literal braces. Missing, duplicate, unused, or malformed
/// arguments are errors rather than silently producing a broken UI message.
pub fn format(key: &str, arguments: &[(&str, &str)]) -> Result<String, String> {
    format_with(key, arguments)
}

/// [`format`] for any `Display` argument values, written straight into the
/// result so callers need not convert each value to a `String` first.
pub fn format_with<V: Display>(key: &str, arguments: &[(&str, V)]) -> Result<String, String> {
    with_template(key, |template| {
        format_template(key, template.unwrap_or(key), arguments)
    })
}

/// [`format_with`] with one catalog lookup for callers that need to know
/// whether the key exists: `None` for an unknown key, otherwise the result.
pub fn format_if_known<V: Display>(
    key: &str,
    arguments: &[(&str, V)],
) -> Option<Result<String, String>> {
    with_template(key, |template| {
        template.map(|template| format_template(key, template, arguments))
    })
}

/// Format `key` from one language's compiled catalog, ignoring the active
/// language and overrides. Thread-independent, so worker threads can build
/// the English fallback of a deferred message without the UI thread's state.
pub fn format_compiled(
    language: Language,
    key: &str,
    arguments: &[(&str, &str)],
) -> Result<String, String> {
    format_compiled_with(language, key, arguments)
}

/// [`format_compiled`] for any `Display` argument values.
pub fn format_compiled_with<V: Display>(
    language: Language,
    key: &str,
    arguments: &[(&str, V)],
) -> Result<String, String> {
    let template =
        compiled_value(language, key).ok_or_else(|| format!("Unknown translation key '{key}'"))?;
    format_template(key, template, arguments)
}

fn format_template<V: Display>(
    key: &str,
    template: &str,
    arguments: &[(&str, V)],
) -> Result<String, String> {
    // Which arguments the template used, as a bitmask: no per-call allocation.
    if arguments.len() > u64::BITS as usize {
        return Err(format!("Too many placeholder arguments for '{key}'"));
    }
    for (index, (name, _)) in arguments.iter().enumerate() {
        validate_placeholder_name(name).map_err(|error| {
            format!("Invalid placeholder argument '{name}' for '{key}': {error}")
        })?;
        if arguments[..index]
            .iter()
            .any(|(previous, _)| previous == name)
        {
            return Err(format!(
                "Duplicate placeholder argument '{name}' for '{key}'"
            ));
        }
    }

    let mut output = String::with_capacity(template.len());
    let mut remaining = template;
    let mut used = 0u64;
    while let Some(index) = remaining.find(['{', '}']) {
        output.push_str(&remaining[..index]);
        remaining = &remaining[index..];
        if remaining.starts_with("{{") {
            output.push('{');
            remaining = &remaining[2..];
            continue;
        }
        if remaining.starts_with("}}") {
            output.push('}');
            remaining = &remaining[2..];
            continue;
        }
        if remaining.starts_with('}') {
            return Err(format!("Unmatched closing brace in translation '{key}'"));
        }

        let end = remaining[1..]
            .find('}')
            .map(|end| end + 1)
            .ok_or_else(|| format!("Unclosed placeholder in translation '{key}'"))?;
        let name = &remaining[1..end];
        validate_placeholder_name(name)
            .map_err(|error| format!("Invalid placeholder in translation '{key}': {error}"))?;
        let argument = arguments
            .iter()
            .position(|(candidate, _)| *candidate == name)
            .ok_or_else(|| format!("Missing placeholder argument '{name}' for '{key}'"))?;
        // Writing into a String cannot fail.
        let _ = write!(output, "{}", arguments[argument].1);
        used |= 1 << argument;
        remaining = &remaining[end + 1..];
    }
    output.push_str(remaining);

    if let Some((name, _)) = arguments
        .iter()
        .enumerate()
        .find_map(|(index, argument)| (used & (1 << index) == 0).then_some(argument))
    {
        return Err(format!("Unused placeholder argument '{name}' for '{key}'"));
    }
    Ok(output)
}

fn validate_placeholder_name(name: &str) -> Result<(), &'static str> {
    let mut bytes = name.bytes();
    if !bytes.next().is_some_and(|byte| byte.is_ascii_lowercase())
        || name.ends_with('_')
        || name.contains("__")
        || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err("names must be snake_case ASCII beginning with a lowercase letter");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "settings.appearance.language.title";
    const LOCATION: &str =
        "Settings → Appearance → Language: custom override for the language pane title.";

    fn reset() {
        activate(Language::English, None).unwrap();
    }

    fn override_json(value: &str) -> String {
        format!(
            r#"{{"settings":{{"appearance":{{"language":{{"title":{{"value":{value:?},"location":{LOCATION:?}}}}}}}}}}}"#
        )
    }

    #[test]
    fn language_registry_uses_native_names_and_stable_codes() {
        assert_eq!(LANGUAGES.len(), 2);
        assert_eq!(Language::English.code(), "en");
        assert_eq!(Language::English.native_name(), "English");
        assert_eq!(Language::Spanish.code(), "es");
        assert_eq!(Language::Spanish.native_name(), "Español");
        assert_eq!(Language::from_code(" ES "), Some(Language::Spanish));
        assert_eq!(Language::from_code("fr"), None);
        assert!(set_language_code("unsupported").is_err());
    }

    #[test]
    fn compiled_defaults_change_with_language() {
        reset();
        assert_eq!(text(KEY), "Language");
        set_language(Language::Spanish);
        assert_eq!(text(KEY), "Idioma");
        assert_eq!(compiled_value(Language::English, KEY), Some("Language"));
        assert_eq!(compiled_value(Language::Spanish, KEY), Some("Idioma"));
        assert_eq!(text("missing.key"), "missing.key");
        reset();
    }

    #[test]
    fn overrides_merge_partially_and_clear_restores_defaults() {
        reset();
        set_overrides_json(&override_json("Custom language")).unwrap();
        assert_eq!(text(KEY), "Custom language");
        assert_eq!(text("settings.appearance.language.apply_button"), "Apply");
        clear_overrides();
        assert_eq!(text(KEY), "Language");
        reset();
    }

    #[test]
    fn invalid_overrides_preserve_existing_overrides_and_language() {
        reset();
        set_overrides_json(&override_json("Existing override")).unwrap();
        let malformed = [
            "not json".to_owned(),
            override_json(""),
            override_json("   "),
            override_json("Valid").replacen(LOCATION, "button", 1),
            r#"{"settings":{"appearance":{"language":{"title":{"value":"First","value":"Second","location":"Settings → Appearance → Language: duplicate values must be rejected."}}}}}"#.to_owned(),
            r#"{"unknown":{"title":{"value":"Bad","location":"Settings → Appearance → Language: this unknown key must be rejected."}}}"#.to_owned(),
            r#"{"settings":{"appearance":{"language":{"title":{"value":"Bad","location":"Settings → Appearance → Language: this extra field must be rejected.","extra":true}}}}}"#.to_owned(),
        ];
        for json in malformed {
            assert!(activate(Language::Spanish, Some(&json)).is_err(), "{json}");
            assert_eq!(current_language(), Language::English);
            assert_eq!(text(KEY), "Existing override");
            assert!(set_overrides_json(&json).is_err(), "{json}");
            assert_eq!(text(KEY), "Existing override");
        }
        reset();
    }

    #[test]
    fn activation_replaces_language_and_overrides_as_one_operation() {
        reset();
        set_overrides_json(&override_json("Existing override")).unwrap();
        activate(Language::Spanish, None).unwrap();
        assert_eq!(current_language(), Language::Spanish);
        assert_eq!(text(KEY), "Idioma");
        activate(Language::English, Some(&override_json("New override"))).unwrap();
        assert_eq!(current_language(), Language::English);
        assert_eq!(text(KEY), "New override");
        reset();
    }

    #[test]
    fn compiled_keys_are_sorted_unique_and_index_every_language() {
        assert!(!KEYS.is_empty());
        assert!(KEYS.windows(2).all(|pair| pair[0] < pair[1]));
        for (index, key) in KEYS.iter().enumerate() {
            assert_eq!(key_index(key), Some(index));
            assert_eq!(compiled_value(Language::English, key), Some(EN[index]));
            assert_eq!(compiled_value(Language::Spanish, key), Some(ES[index]));
        }
    }

    #[test]
    fn key_hasher_distinguishes_shared_prefixes_and_tails() {
        use std::hash::BuildHasher;
        let build = BuildHasherDefault::<KeyHasher>::default();
        let hashes = [
            "settings.appearance.language.title",
            "settings.appearance.language.titl",
            "settings.appearance.language.title2",
            "",
        ]
        .map(|key| build.hash_one(key));
        let unique = hashes.iter().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique.len(), hashes.len());
    }

    #[test]
    fn generated_application_data_excludes_location_descriptions() {
        let generated = include_str!(concat!(env!("OUT_DIR"), "/compiled_catalogs.rs"));
        assert!(!generated.contains("Settings → Appearance"));
        assert!(!generated.contains("title of the language preferences pane"));
        assert!(!generated.contains("explanatory paragraph immediately below"));
    }

    #[test]
    fn formats_named_placeholders_without_assembling_message_fragments() {
        reset();
        set_overrides_json(&position_override_json("Showing {current} / {total}")).unwrap();
        assert_eq!(
            format(POSITION_KEY, &[("current", "3"), ("total", "12")]).unwrap(),
            "Showing 3 / 12"
        );
        assert_eq!(
            format_template("test", "{{count}} = {count}", &[("count", "3")]).unwrap(),
            "{count} = 3"
        );
        reset();
    }

    const POSITION_KEY: &str = "datatable.grid.search.position";
    const POSITION_LOCATION: &str =
        "Results → Grid → Search bar: test override for the match position counter.";

    fn position_override_json(value: &str) -> String {
        format!(
            r#"{{"datatable":{{"grid":{{"search":{{"position":{{"value":{value:?},"location":{POSITION_LOCATION:?}}}}}}}}}}}"#
        )
    }

    fn unknown_override_json() -> &'static str {
        r#"{"unknown":{"title":{"value":"Bad","location":"Settings → Appearance → Language: this unknown key is not in the build."}}}"#
    }

    #[test]
    fn strict_overrides_reject_changed_placeholder_sets() {
        reset();
        for value in [
            "{current} de {total_count}",
            "Coincidencia {current}",
            "Sin números",
        ] {
            let error = set_overrides_json(&position_override_json(value)).unwrap_err();
            assert!(error.contains(POSITION_KEY), "{error}");
            assert!(activate(Language::Spanish, Some(&position_override_json(value))).is_err());
        }
        assert_eq!(current_language(), Language::English);
        set_overrides_json(&position_override_json("{total}: n.º {current}")).unwrap();
        assert_eq!(
            format(POSITION_KEY, &[("current", "2"), ("total", "9")]).unwrap(),
            "9: n.º 2"
        );
        reset();
    }

    #[test]
    fn strict_overrides_still_reject_unknown_keys() {
        reset();
        let error = set_overrides_json(unknown_override_json()).unwrap_err();
        assert_eq!(error, "Unknown translation key 'unknown.title'");
    }

    #[test]
    fn malformed_formatted_overrides_are_rejected_atomically() {
        reset();
        set_overrides_json(&position_override_json("{current}/{total}")).unwrap();
        for value in [
            "{current}/{total} }",
            "{current}/{total} {",
            "{current}/{total} {bad__name}",
        ] {
            assert!(set_overrides_json(&position_override_json(value)).is_err());
            assert_eq!(text(POSITION_KEY), "{current}/{total}");
        }
        assert_eq!(
            format(POSITION_KEY, &[("current", "2"), ("total", "9")]).unwrap(),
            "2/9"
        );
        reset();
    }

    #[test]
    fn lenient_overrides_skip_unknown_and_mismatched_keys_and_apply_the_rest() {
        reset();
        let json = serde_json::json!({
            "settings": { "appearance": { "language": { "title": {
                "value": "Idioma del anfitrión", "location": LOCATION } } } },
            "datatable": { "grid": { "search": { "position": {
                "value": "{current} de {cuenta}", "location": POSITION_LOCATION } } } },
            "future": { "screen": { "title": {
                "value": "Newer server key",
                "location": "Future → Screen: key added by a newer website catalog." } } }
        })
        .to_string();
        let skipped = set_overrides_json_lenient(&json).unwrap();
        let skipped_keys = skipped
            .iter()
            .map(|skip| skip.key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(skipped_keys, [POSITION_KEY, "future.screen.title"]);
        assert!(skipped[0].reason.contains("cuenta"), "{}", skipped[0]);
        assert_eq!(skipped[1].reason, UNKNOWN_KEY);
        assert_eq!(text(KEY), "Idioma del anfitrión");
        assert_eq!(text(POSITION_KEY), "{current} of {total}");
        reset();
    }

    #[test]
    fn lenient_overrides_keep_malformed_json_a_hard_error() {
        reset();
        set_overrides_json(&override_json("Existing override")).unwrap();
        for json in [
            "not json",
            "{}",
            r#"{"settings":{"title":{"value":"No location"}}}"#,
        ] {
            assert!(set_overrides_json_lenient(json).is_err(), "{json}");
            assert_eq!(text(KEY), "Existing override");
        }
        assert_eq!(
            set_overrides_json_lenient(unknown_override_json())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(text(KEY), "Language");
        reset();
    }

    #[test]
    fn snapshot_restores_language_and_overrides_together() {
        reset();
        set_overrides_json(&override_json("Before")).unwrap();
        let before = snapshot();
        activate(Language::Spanish, Some(&override_json("Después"))).unwrap();
        assert_eq!(text(KEY), "Después");
        restore(before);
        assert_eq!(current_language(), Language::English);
        assert_eq!(text(KEY), "Before");
        reset();
    }

    #[test]
    fn lenient_activation_switches_language_and_keeps_valid_entries() {
        reset();
        let json = serde_json::json!({
            "settings": { "appearance": { "language": { "title": {
                "value": "Mi idioma", "location": LOCATION } } } },
            "renamed": { "screen": { "title": {
                "value": "Old key",
                "location": "Renamed → Screen: key that a newer build renamed away." } } }
        })
        .to_string();
        let skipped = activate_lenient(Language::Spanish, &json).unwrap();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].key, "renamed.screen.title");
        assert_eq!(current_language(), Language::Spanish);
        assert_eq!(text(KEY), "Mi idioma");
        assert!(activate_lenient(Language::English, "not json").is_err());
        assert_eq!(current_language(), Language::Spanish);
        assert_eq!(text(KEY), "Mi idioma");
        reset();
    }

    #[test]
    fn format_with_display_values_and_known_key_lookup() {
        reset();
        assert_eq!(
            format_with(POSITION_KEY, &[("current", 3), ("total", 12)]).unwrap(),
            "3 of 12"
        );
        assert_eq!(
            format_if_known(POSITION_KEY, &[("current", 1), ("total", 2)]),
            Some(Ok("1 of 2".to_owned()))
        );
        assert_eq!(format_if_known("missing.key", &[("count", 1)]), None);
        assert!(matches!(
            format_if_known(POSITION_KEY, &[("current", 1)]),
            Some(Err(_))
        ));
        let too_many = (0..65)
            .map(|index| (["a", "b"][index % 2], index))
            .collect::<Vec<_>>();
        assert!(format_template("test", "{a}", &too_many).is_err());
    }

    #[test]
    fn switching_language_never_carries_overrides_across() {
        reset();
        set_overrides_json(&override_json("English-only custom title")).unwrap();
        set_language(Language::Spanish);
        assert_eq!(text(KEY), "Idioma");
        set_overrides_json(&override_json("Título personalizado")).unwrap();
        assert!(set_language_code("en").is_ok());
        assert_eq!(text(KEY), "Language");
        reset();
    }

    #[test]
    fn try_text_and_text_static_report_unknown_and_overridden_keys() {
        reset();
        assert_eq!(try_text(KEY).as_deref(), Some("Language"));
        assert_eq!(try_text("missing.key"), None);
        assert!(matches!(try_text_cow(KEY), Some(Cow::Borrowed("Language"))));
        assert_eq!(try_text_cow("missing.key"), None);
        assert_eq!(text_static(KEY), Some("Language"));
        assert_eq!(text_static("missing.key"), None);
        set_overrides_json(&override_json("Custom language")).unwrap();
        assert_eq!(text_static(KEY), None);
        assert_eq!(try_text(KEY).as_deref(), Some("Custom language"));
        assert!(matches!(try_text_cow(KEY), Some(Cow::Owned(value)) if value == "Custom language"));
        assert_eq!(
            text_static("settings.appearance.language.apply_button"),
            Some("Apply")
        );
        reset();
    }

    #[test]
    fn every_compiled_spanish_value_uses_the_english_placeholders() {
        let mismatched = KEYS
            .iter()
            .zip(EN.iter().zip(ES))
            .filter(|(_, (english, spanish))| {
                catalog::placeholder_mismatch(english, spanish).is_some()
            })
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        assert!(mismatched.is_empty(), "{mismatched:?}");
    }

    #[test]
    fn format_compiled_ignores_active_language_and_overrides() {
        reset();
        set_language(Language::Spanish);
        set_overrides_json(&position_override_json("{current}/{total}")).unwrap();
        let arguments = [("current", "2"), ("total", "9")];
        assert_eq!(
            format_compiled(Language::English, POSITION_KEY, &arguments).unwrap(),
            "2 of 9"
        );
        assert_eq!(
            format_compiled(Language::Spanish, POSITION_KEY, &arguments).unwrap(),
            "2 de 9"
        );
        assert!(format_compiled(Language::English, "missing.key", &[]).is_err());
        reset();
    }

    #[test]
    fn formatting_rejects_missing_unused_duplicate_and_malformed_arguments() {
        let valid = &[("count", "3")][..];
        assert!(format_template("test", "{count} {table}", valid).is_err());
        assert!(format_template("test", "No placeholders", valid).is_err());
        assert!(format_template("test", "{count} {count}", valid).is_ok());
        assert!(format_template("test", "{count}", &[("count", "3"), ("count", "4")]).is_err());
        for template in ["{count", "}count", "{Count}", "{count-name}", "{}"] {
            assert!(
                format_template("test", template, valid).is_err(),
                "{template}"
            );
        }
    }
}
