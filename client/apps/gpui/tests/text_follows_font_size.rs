//! GPUI font-size guard: text follows the interface font size.
//!
//! `Theme::scaled` (and the `s` alias views bind to it) is a fixed UI scale,
//! not the user's font size, so `text_size(px(s(12.0)))` pins a label at one
//! size no matter what Settings ▸ Appearance says. Readers who raise the
//! interface font for legibility then find error messages, empty states and
//! headlines stuck small. Size text with `Theme::font_size()` /
//! `Theme::font_size_ratio(..)` instead.
//!
//! The one exception is a glyph drawn to fit a fixed-size box (a window
//! control, a checkbox tick, an icon tile): growing it would overflow its box.
//! Mark those with a comment containing the word `glyph` on the line or
//! within the four lines above it.

use std::path::{Path, PathBuf};

/// Fixed glyphs in files this guard must not edit.
const KNOWN_GLYPH_SITES: &[(&str, &str)] = &[("app/account.rs", "🐷")];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// The shipped part of a source file: everything before its trailing
/// `#[cfg(test)] mod …` unit-test module. A `#[cfg(test)]` on a single item
/// inside the file does not end it.
fn shipped_part(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let end = lines
        .windows(2)
        .position(|pair| pair[0] == "#[cfg(test)]" && pair[1].trim_start().starts_with("mod "))
        .unwrap_or(lines.len());
    lines[..end].join("\n")
}

#[test]
fn text_sizes_follow_the_interface_font_size() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    files.sort();

    let mut offenders = Vec::new();
    for path in files {
        if path.file_name().and_then(|name| name.to_str()) == Some("tests.rs") {
            continue;
        }
        let relative = path.strip_prefix(&src).unwrap_or(&path).to_path_buf();
        let relative_str = relative.to_string_lossy().replace('\\', "/");
        let text = std::fs::read_to_string(&path).expect("read source file");
        let shipped = shipped_part(&text);
        let lines: Vec<&str> = shipped.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if !(line.contains("text_size(px(s(") || line.contains("text_size(px(Theme::scaled(")) {
                continue;
            }
            let window = &lines[index.saturating_sub(4)..(index + 2).min(lines.len())];
            let marked = window.iter().any(|l| l.to_lowercase().contains("glyph"));
            let known = KNOWN_GLYPH_SITES.iter().any(|(file, needle)| {
                *file == relative_str && window.iter().any(|l| l.contains(needle))
            });
            if !marked && !known {
                offenders.push(format!("{relative_str}:{}", index + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "text pinned to a fixed size (use Theme::font_size_ratio, or mark a fixed glyph):\n{}",
        offenders.join("\n")
    );
}
