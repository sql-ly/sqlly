//! Catalog text must be precomposed (Unicode NFC). A decomposed "ü" (u plus a
//! combining diaeresis) looks right in an editor but compares unequal to the
//! precomposed form, and fonts without the combining mark draw it as a box.
//! None of the catalog languages needs a combining mark once text is NFC, so
//! any in the files is a decomposed character pasted in.

use std::path::Path;

fn is_combining_mark(ch: char) -> bool {
    // Combining diacritical marks, and the combining kana voicing marks that
    // NFC folds into the preceding kana.
    matches!(ch, '\u{0300}'..='\u{036F}' | '\u{3099}' | '\u{309A}')
}

#[test]
fn catalogs_use_precomposed_characters() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("translations");
    let mut found = Vec::new();
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("read translations") {
        let path = entry.expect("catalog entry").path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        checked += 1;
        let text = std::fs::read_to_string(&path).expect("read catalog");
        for (index, line) in text.lines().enumerate() {
            if let Some(mark) = line.chars().find(|ch| is_combining_mark(*ch)) {
                found.push(format!(
                    "{}:{}: U+{:04X} in {}",
                    path.file_name().unwrap().to_string_lossy(),
                    index + 1,
                    mark as u32,
                    line.trim()
                ));
            }
        }
    }
    assert!(checked >= 10, "expected every catalog, found {checked}");
    assert!(
        found.is_empty(),
        "decomposed characters in catalogs; replace them with the precomposed (NFC) form:\n{}",
        found.join("\n")
    );
}
