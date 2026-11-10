//! GPUI font guard: no shipped view may ask for a CSS-style generic family
//! such as `"monospace"`. GPUI looks families up by their real name, and no
//! platform has a font called "monospace", so the lookup fails and the text
//! falls through GPUI's fallback stack to a proportional face (Helvetica on
//! macOS). SQL and command output then ignore the editor font the user
//! picked. Use `Theme::editor_font_family()` (or `Theme::font_family()` for
//! prose) instead.

use std::path::{Path, PathBuf};

const GENERIC_FAMILIES: &[&str] = &[
    "\"monospace\"",
    "\"sans-serif\"",
    "\"serif\"",
    "\"system-ui\"",
    "\"ui-monospace\"",
];

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
fn shipped_views_never_request_a_generic_font_family() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    files.sort();

    let mut offenders = Vec::new();
    for path in files {
        if path.file_name().and_then(|name| name.to_str()) == Some("tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read source file");
        // Unit tests sit in a trailing `#[cfg(test)]` module; they may use
        // any font, since the test text system never renders.
        let shipped = shipped_part(&text);
        for (index, line) in shipped.lines().enumerate() {
            let code = line.split("//").next().unwrap_or_default();
            if (code.contains("font_family(") || code.contains("font("))
                && GENERIC_FAMILIES.iter().any(|family| code.contains(family))
            {
                let relative = path.strip_prefix(&src).unwrap_or(&path);
                offenders.push(format!("{}:{}", relative.display(), index + 1));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "generic font family requested (renders proportional; use the editor font):\n{}",
        offenders.join("\n")
    );
}
