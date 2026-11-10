//! Pure helpers shared by entity editors.
//!
//! Port of the pure helpers in `EntityEditingSupport.swift`: name normalization,
//! list label fallback, and hex color parsing. These are kept UI-free so the
//! logic is unit-testable without AppKit.

/// Normalizes a user-entered name for display/validation: trims surrounding
/// whitespace. Returns `None` when the result is empty.
pub fn normalized_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// The label shown for an entity in a list: its name, or a placeholder when the
/// name is blank (e.g. a freshly-added row).
pub fn list_label(name: &str, placeholder: &str) -> String {
    normalized_name(name).unwrap_or_else(|| placeholder.to_string())
}

/// Convenience: `list_label` with the default "Untitled" placeholder.
pub fn list_label_default(name: &str) -> String {
    list_label(
        name,
        &crate::localization::text("connections.entities.untitled_row"),
    )
}

/// Parses a `#RRGGBB` / `RRGGBB` (and 8-digit `#RRGGBBAA` / `RRGGBBAA` alpha)
/// hex string into an `(r, g, b, a)` tuple (0–255). Returns `None` on any parse
/// failure.
pub fn parse_hex_color(hex: &str) -> Option<(u8, u8, u8, u8)> {
    let trimmed = hex.trim().trim_start_matches('#');
    // Hex digits are ASCII; a non-ASCII char would make the byte-length match
    // below slice mid-character and panic. Reject early.
    if !trimmed.is_ascii() {
        return None;
    }
    let (rgb, a) = match trimmed.len() {
        6 => (trimmed, "FF"),
        8 => (&trimmed[..6], &trimmed[6..8]),
        _ => return None,
    };
    let r = u8::from_str_radix(&rgb[0..2], 16).ok()?;
    let g = u8::from_str_radix(&rgb[2..4], 16).ok()?;
    let b = u8::from_str_radix(&rgb[4..6], 16).ok()?;
    let a = u8::from_str_radix(a, 16).ok()?;
    Some((r, g, b, a))
}

#[allow(dead_code)]
/// Formats an `(r, g, b)` tuple as a lowercase `#rrggbb` string.
pub fn format_hex_color(r: u8, g: u8, b: u8) -> String {
    format!("#{:02x}{:02x}{:02x}", r, g, b)
}

/// Validates a hex color string without parsing it (for live form validation).
pub fn is_valid_hex_color(hex: &str) -> bool {
    parse_hex_color(hex).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_name_trims() {
        assert_eq!(normalized_name("  hello  "), Some("hello".into()));
    }

    #[test]
    fn normalized_name_rejects_empty() {
        assert_eq!(normalized_name(""), None);
        assert_eq!(normalized_name("   "), None);
        assert_eq!(normalized_name("\t\n"), None);
    }

    #[test]
    fn list_label_uses_name_when_present() {
        assert_eq!(list_label("Alpha", "Untitled"), "Alpha");
    }

    #[test]
    fn list_label_falls_back_to_placeholder() {
        assert_eq!(list_label("", "Untitled"), "Untitled");
        assert_eq!(list_label("  ", "Untitled"), "Untitled");
    }

    #[test]
    fn list_label_default_uses_untitled() {
        assert_eq!(list_label_default("x"), "x");
        assert_eq!(list_label_default(""), "Untitled");
    }

    #[test]
    fn parse_hex_color_six_digit() {
        assert_eq!(parse_hex_color("#1E88E5"), Some((0x1E, 0x88, 0xE5, 0xFF)));
        assert_eq!(parse_hex_color("1E88E5"), Some((0x1E, 0x88, 0xE5, 0xFF)));
    }

    #[test]
    fn parse_hex_color_eight_digit_with_alpha() {
        assert_eq!(parse_hex_color("#1E88E580"), Some((0x1E, 0x88, 0xE5, 0x80)));
        assert_eq!(parse_hex_color("1E88E5FF"), Some((0x1E, 0x88, 0xE5, 0xFF)));
    }

    #[test]
    fn parse_hex_color_rejects_invalid() {
        assert_eq!(parse_hex_color("#12345"), None);
        assert_eq!(parse_hex_color("#1234567"), None);
        assert_eq!(parse_hex_color("#GGGGGG"), None);
        assert_eq!(parse_hex_color(""), None);
        assert_eq!(parse_hex_color("#"), None);
    }

    #[test]
    fn parse_hex_color_rejects_multibyte_without_panic() {
        // A non-ASCII char whose byte length lands the fixed-width slice
        // mid-scalar must be rejected, not panic. "12345ó7" is 8 bytes.
        assert_eq!(parse_hex_color("12345ó7"), None);
        assert_eq!(parse_hex_color("#áéíóF"), None);
    }

    #[test]
    fn parse_hex_color_trims_whitespace() {
        assert_eq!(
            parse_hex_color("  #FF0000  "),
            Some((0xFF, 0x00, 0x00, 0xFF))
        );
    }

    #[test]
    fn format_hex_color_lowercases() {
        assert_eq!(format_hex_color(0x1E, 0x88, 0xE5), "#1e88e5");
        assert_eq!(format_hex_color(0, 0, 0), "#000000");
        assert_eq!(format_hex_color(255, 255, 255), "#ffffff");
    }

    #[test]
    fn is_valid_hex_color_checks() {
        assert!(is_valid_hex_color("#FFFFFF"));
        assert!(is_valid_hex_color("ffffff"));
        assert!(is_valid_hex_color("#ffffff80"));
        assert!(!is_valid_hex_color("#FFF"));
        assert!(!is_valid_hex_color("nope"));
    }
}
