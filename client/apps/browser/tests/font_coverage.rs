//! The browser build draws text only with the fonts in `apps/browser/fonts`,
//! so a catalog character missing from them renders as a tofu box. This test
//! checks every catalog value against the face its language uses (see
//! `src/fonts.rs`) together with the embedded subsets, and requires
//! ideographs, kana and Hangul to come from the language's own face so they
//! take its regional shapes. When it fails, rerun `fonts/build-fonts.py` and
//! then `fonts/build-symbols.py`.

use sqlly_browser::fonts::{ALL, InterfaceFont, interface_font};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn fonts_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts")
}

fn codepoints(font: InterfaceFont) -> BTreeSet<u32> {
    font_codepoints(&std::fs::read(fonts_dir().join(font.file)).expect("read font"))
}

/// Ideographs, kana and Hangul: characters whose shape is regional.
fn needs_regional_face(cp: u32) -> bool {
    matches!(cp, 0x3040..=0x30FF | 0x3130..=0x318F | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF)
}

/// Every character a catalog's values draw, mapped to one key using it.
fn catalog_characters(path: &Path) -> BTreeMap<char, String> {
    fn walk(node: &serde_json::Value, key: &str, found: &mut BTreeMap<char, String>) {
        let serde_json::Value::Object(map) = node else {
            return;
        };
        if let Some(serde_json::Value::String(value)) = map.get("value") {
            for ch in value.chars().filter(|ch| *ch > ' ') {
                found.entry(ch).or_insert_with(|| key.to_owned());
            }
            return;
        }
        for (name, child) in map {
            let child_key = if key.is_empty() {
                name.clone()
            } else {
                format!("{key}.{name}")
            };
            walk(child, &child_key, found);
        }
    }
    let text = std::fs::read_to_string(path).expect("read catalog");
    let catalog: serde_json::Value = serde_json::from_str(&text).expect("parse catalog");
    let mut found = BTreeMap::new();
    walk(&catalog, "", &mut found);
    found
}

#[test]
fn every_catalog_renders_in_its_browser_interface_font() {
    let embedded: BTreeSet<u32> = ALL
        .iter()
        .filter(|font| font.embedded)
        .flat_map(|font| codepoints(*font))
        .collect();
    let catalogs =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/sqlly-i18n/translations");
    let mut checked = 0;
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&catalogs).expect("read catalogs") {
        let path = entry.expect("catalog entry").path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let language = path.file_stem().unwrap().to_str().unwrap().to_owned();
        let font = interface_font(&language);
        let own = codepoints(font);
        for (ch, key) in catalog_characters(&path) {
            let cp = ch as u32;
            let covered = if needs_regional_face(cp) {
                own.contains(&cp)
            } else {
                own.contains(&cp) || embedded.contains(&cp)
            };
            if !covered {
                missing.push(format!(
                    "{language}: U+{cp:04X} {ch} ({key}) not in {}",
                    font.file
                ));
            }
        }
        checked += 1;
    }
    assert!(checked >= 10, "expected every catalog, found {checked}");
    assert!(
        missing.is_empty(),
        "catalog characters the browser cannot draw; rerun apps/browser/fonts/build-fonts.py \
         and build-symbols.py:\n{}",
        missing.join("\n")
    );
}

#[test]
fn lazy_faces_are_staged_beside_the_page() {
    let script = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../build-browser.sh"),
    )
    .expect("read build-browser.sh");
    for font in ALL.iter().filter(|font| !font.embedded) {
        assert!(
            fonts_dir().join(font.file).is_file(),
            "{} is missing",
            font.file
        );
        let face = font
            .file
            .trim_start_matches("NotoSans")
            .split('-')
            .next()
            .unwrap();
        assert!(
            script.contains("for face in") && script.contains(face),
            "build-browser.sh does not copy {} into dist/fonts",
            font.file
        );
    }
}

/// The set of codepoints a TrueType font maps to glyphs, read straight from
/// its `cmap` table (format 4 and format 12 subtables cover every Noto
/// subset here). Hand-rolled so the test adds no font-parsing dependency,
/// which the dependency audit would flag.
fn font_codepoints(data: &[u8]) -> std::collections::BTreeSet<u32> {
    let u16_at = |at: usize| u16::from_be_bytes([data[at], data[at + 1]]);
    let u32_at =
        |at: usize| u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]);
    let table_count = u16_at(4) as usize;
    let cmap = (0..table_count)
        .map(|i| 12 + i * 16)
        .find(|&rec| &data[rec..rec + 4] == b"cmap")
        .map(|rec| u32_at(rec + 8) as usize)
        .expect("font has a cmap table");
    let mut codepoints = std::collections::BTreeSet::new();
    let subtables = u16_at(cmap + 2) as usize;
    for i in 0..subtables {
        let sub = cmap + u32_at(cmap + 4 + i * 8 + 4) as usize;
        match u16_at(sub) {
            4 => {
                let seg_count = u16_at(sub + 6) as usize / 2;
                let ends = sub + 14;
                let starts = ends + seg_count * 2 + 2;
                let deltas = starts + seg_count * 2;
                let range_offsets = deltas + seg_count * 2;
                for seg in 0..seg_count {
                    let (start, end) = (u16_at(starts + seg * 2), u16_at(ends + seg * 2));
                    if start == 0xFFFF {
                        continue;
                    }
                    let delta = u16_at(deltas + seg * 2);
                    let range_offset = u16_at(range_offsets + seg * 2) as usize;
                    for cp in start..=end {
                        let glyph = if range_offset == 0 {
                            cp.wrapping_add(delta)
                        } else {
                            let at =
                                range_offsets + seg * 2 + range_offset + (cp - start) as usize * 2;
                            match u16_at(at) {
                                0 => 0,
                                g => g.wrapping_add(delta),
                            }
                        };
                        if glyph != 0 {
                            codepoints.insert(cp as u32);
                        }
                    }
                }
            }
            12 => {
                let groups = u32_at(sub + 12) as usize;
                for g in 0..groups {
                    let at = sub + 16 + g * 12;
                    let (start, end, glyph) = (u32_at(at), u32_at(at + 4), u32_at(at + 8));
                    if glyph != 0 {
                        codepoints.extend(start..=end);
                    }
                }
            }
            _ => {}
        }
    }
    codepoints
}
