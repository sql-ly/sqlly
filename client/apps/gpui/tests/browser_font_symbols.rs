//! The browser build has no system font fallback: only the two subset fonts
//! embedded by `apps/browser` are ever consulted, so a symbol the UI draws as
//! text (`"\u{270E}"`, `"⤢"`, …) that is missing from them renders as a tofu
//! box. This test scans the UI sources for every symbol-range codepoint and
//! checks both subsets carry a glyph for it. When it fails, add the codepoint
//! to `SYMBOL_CODEPOINTS` in `apps/browser/fonts/build-symbols.py` and rerun
//! that script (the same scan is `python3 build-symbols.py --check`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// General Punctuation through Miscellaneous Symbols and Arrows: the blocks
/// the UI draws as literal text. Natural-language sample strings (accents,
/// Greek, CJK) are outside it and rely on the fonts' own coverage.
const SYMBOL_RANGE: std::ops::RangeInclusive<u32> = 0x2000..=0x2BFF;

/// Model-facing or test-only sources whose non-ASCII text is never drawn.
const SKIP_FILES: &[&str] = &["tests.rs", "fim_completion.rs"];

fn source_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "tests") {
                continue;
            }
            source_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && !SKIP_FILES.contains(&path.file_name().unwrap().to_str().unwrap())
        {
            out.push(path);
        }
    }
}

/// Every symbol-range codepoint the UI sources use, literally or as a Rust
/// `\u{..}` escape, mapped to the first place it appears.
fn ui_symbols() -> BTreeMap<u32, String> {
    let mut files = Vec::new();
    source_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    let mut found = BTreeMap::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read source");
        for (index, line) in text.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let location = || format!("{}:{}", file.display(), index + 1);
            let mut rest = line;
            while let Some(start) = rest.find("\\u{") {
                let after = &rest[start + 3..];
                if let Some(end) = after.find('}')
                    && let Ok(cp) = u32::from_str_radix(&after[..end], 16)
                    && SYMBOL_RANGE.contains(&cp)
                {
                    found.entry(cp).or_insert_with(location);
                }
                rest = after;
            }
            for ch in line.chars() {
                if SYMBOL_RANGE.contains(&(ch as u32)) {
                    found.entry(ch as u32).or_insert_with(location);
                }
            }
        }
    }
    found
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

#[test]
fn cmap_reader_sees_ascii_and_the_dashes_in_both_subsets() {
    let fonts_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../browser/fonts");
    for name in [
        "NotoSans-Regular-subset.ttf",
        "NotoSansSC-Regular-subset.ttf",
    ] {
        let set = font_codepoints(&std::fs::read(fonts_dir.join(name)).expect("read font"));
        for ch in ['m', '0', '\u{2013}', '\u{2014}', '\u{2192}'] {
            assert!(set.contains(&(ch as u32)), "{name} lacks {ch:?}");
        }
        assert!(!set.contains(&0x1F600), "{name} unexpectedly maps an emoji");
    }
}

#[test]
fn every_ui_symbol_has_a_glyph_in_the_browser_subset_fonts() {
    let fonts_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../browser/fonts");
    let fonts = [
        "NotoSans-Regular-subset.ttf",
        "NotoSansSC-Regular-subset.ttf",
    ]
    .map(|name| {
        (
            name,
            std::fs::read(fonts_dir.join(name)).expect("read subset font"),
        )
    });
    let faces: Vec<(&str, std::collections::BTreeSet<u32>)> = fonts
        .iter()
        .map(|(name, bytes)| (*name, font_codepoints(bytes)))
        .collect();

    let symbols = ui_symbols();
    assert!(
        symbols.contains_key(&0x270E),
        "the results-tab rename pencil should be among the scanned symbols"
    );

    let mut missing = Vec::new();
    for (&cp, location) in &symbols {
        let ch = char::from_u32(cp).expect("valid scalar");
        let lacking: Vec<&str> = faces
            .iter()
            .filter(|(_, codepoints)| !codepoints.contains(&cp))
            .map(|(name, _)| *name)
            .collect();
        if !lacking.is_empty() {
            missing.push(format!(
                "U+{cp:04X} {ch} ({location}) missing from {}",
                lacking.join(", ")
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "UI symbols missing from the browser subset fonts; add them to SYMBOL_CODEPOINTS in \
         apps/browser/fonts/build-symbols.py and rerun it:\n{}",
        missing.join("\n")
    );
}
