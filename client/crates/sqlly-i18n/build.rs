use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::Path,
};

#[path = "src/catalog.rs"]
mod catalog;

/// English values that contain named-looking `{placeholders}` but are prose
/// documenting a brace syntax, shown only through `text` (never formatted).
/// They are exempt from the English format-syntax check.
const PROSE_BRACE_KEYS: &[&str] = &[
    "editor.template_explorer.body_help",
    "settings.snippets.hint.placeholder_syntax",
];

fn main() {
    println!("cargo:rerun-if-changed=translations/en.json");
    println!("cargo:rerun-if-changed=translations/es.json");
    println!("cargo:rerun-if-changed=src/catalog.rs");

    let manifest = env::var("CARGO_MANIFEST_DIR").expect("manifest directory");
    let catalog_dir = Path::new(&manifest).join("translations");
    let out_dir = env::var("OUT_DIR").expect("build output directory");
    let mut generated = String::from(
        "// Generated from translations/*.json; location metadata is deliberately omitted.\n",
    );
    let mut english: Option<BTreeMap<String, String>> = None;
    let mut values_by_language = Vec::new();

    for language in ["en", "es"] {
        let path = catalog_dir.join(format!("{language}.json"));
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
        let entries = catalog::parse(&source).unwrap_or_else(|error| {
            panic!("invalid translation catalog {}: {error}", path.display())
        });

        if let Some(english) = &english {
            let english_keys = english.keys().collect::<BTreeSet<_>>();
            let keys = entries.keys().collect::<BTreeSet<_>>();
            let missing = english_keys.difference(&keys).collect::<Vec<_>>();
            let extra = keys.difference(&english_keys).collect::<Vec<_>>();
            assert!(
                missing.is_empty() && extra.is_empty(),
                "English and Spanish catalogs must contain exactly the same keys; {language} differs from en: missing={missing:?}, extra={extra:?}"
            );
            let mismatched = entries
                .iter()
                .filter_map(|(key, value)| {
                    catalog::placeholder_mismatch(&english[key], value)
                        .map(|error| format!("{key}: {error}"))
                })
                .collect::<Vec<_>>();
            assert!(
                mismatched.is_empty(),
                "every {language} translation must use exactly the same named {{placeholders}} as English; {} key(s) differ:\n  {}",
                mismatched.len(),
                mismatched.join("\n  ")
            );
        } else {
            // English defines each message's format contract, so a value with
            // named placeholders must itself be formattable. Prose that only
            // documents brace syntax (and is shown through `text`) is listed.
            let malformed = entries
                .iter()
                .filter(|(key, value)| {
                    !PROSE_BRACE_KEYS.contains(&key.as_str())
                        && !catalog::placeholder_names(value).is_empty()
                        && !catalog::valid_format_syntax(value)
                })
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>();
            assert!(
                malformed.is_empty(),
                "English values with named {{placeholders}} must use valid format syntax (escape literal braces as {{{{ and }}}}); {} key(s) do not: {malformed:?}",
                malformed.len()
            );
            for key in PROSE_BRACE_KEYS {
                assert!(
                    entries.contains_key(*key),
                    "PROSE_BRACE_KEYS lists '{key}', which is not in the English catalog"
                );
            }
        }

        values_by_language.push((language, entries.values().cloned().collect::<Vec<_>>()));
        english.get_or_insert(entries);
    }

    // One sorted key table shared by every language; each language is a
    // value table in the same order, so a key's index is valid everywhere by
    // construction.
    let english = english.expect("English catalog");
    generated.push_str(&format!(
        "pub(crate) const KEY_COUNT: usize = {};\npub(crate) static KEYS: &[&str; KEY_COUNT] = &[\n",
        english.len()
    ));
    for key in english.keys() {
        generated.push_str(&format!("    {key:?},\n"));
    }
    generated.push_str("];\n");
    for (language, values) in values_by_language {
        generated.push_str(&format!(
            "pub(crate) static {}: &[&str; KEY_COUNT] = &[\n",
            language.to_uppercase()
        ));
        for value in values {
            generated.push_str(&format!("    {value:?},\n"));
        }
        generated.push_str("];\n");
    }

    fs::write(Path::new(&out_dir).join("compiled_catalogs.rs"), generated)
        .expect("write compiled translation catalogs");
}
