//! Escaping and link checks shared by the markup renderers.

/// Escapes XML special characters (`&` `<` `>` `"` `'`), so the result is
/// safe in element content and in quoted attributes of XML, SVG and HTML.
/// Control characters XML 1.0 forbids are dropped.
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(ch),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            _ => out.push(ch),
        }
    }
    out
}

/// HTML escaping; `'` becomes `&#39;` because `&apos;` is not HTML 4.
pub fn html_escape(text: &str) -> String {
    xml_escape(text).replace("&apos;", "&#39;")
}

/// A link target safe to emit: `http(s)`, `mailto` and in-page anchors.
/// Everything else (`javascript:`, `data:`, `file:`, `vbscript:`, schemes
/// hidden behind entities or whitespace) is refused.
pub fn safe_link(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_control()) {
        return None;
    }
    if let Some(anchor) = trimmed.strip_prefix('#') {
        return anchor
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
            .then(|| trimmed.to_string());
    }
    let lower = trimmed.to_ascii_lowercase();
    let allowed = ["https://", "http://", "mailto:"];
    if allowed.iter().any(|p| lower.starts_with(p)) && !trimmed.contains(['<', '>', '"', '`', ' '])
    {
        return Some(trimmed.to_string());
    }
    None
}

/// An embedded image source safe for `<img src>`: base64 raster data only.
pub fn safe_image_data_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    let ok = [
        "data:image/png;base64,",
        "data:image/jpeg;base64,",
        "data:image/gif;base64,",
        "data:image/webp;base64,",
    ]
    .iter()
    .any(|p| lower.starts_with(p));
    let payload = trimmed.split_once(',').map(|(_, p)| p).unwrap_or("");
    (ok && payload
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=')))
    .then(|| trimmed.to_string())
}

/// A lowercase ASCII slug for anchors and file names.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
        if out.len() >= 48 {
            break;
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.is_empty() {
        "section".into()
    } else {
        trimmed.into()
    }
}

/// Unique anchor ids for a sequence of titles.
#[derive(Default)]
pub struct Anchors {
    used: std::collections::HashSet<String>,
}

impl Anchors {
    pub fn next(&mut self, text: &str) -> String {
        let base = slug(text);
        let mut id = base.clone();
        let mut n = 2;
        while !self.used.insert(id.clone()) {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }
}

/// Collapses whitespace runs (newlines included) to single spaces.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// True when SVG markup contains anything that could run or fetch:
/// scripts, foreign objects, event handlers, external references.
pub fn svg_is_unsafe(markup: &str) -> bool {
    let lower = markup.to_ascii_lowercase();
    let compact: String = lower.chars().filter(|c| !c.is_whitespace()).collect();
    [
        "<script",
        "<foreignobject",
        "javascript:",
        "<iframe",
        "<object",
        "<embed",
        "<!entity",
        "<!doctype",
        "<?xml-stylesheet",
        "@import",
        "<use",
        "url(http",
        "xlink:href=\"http",
        "href=\"http",
        "href='http",
        "<a",
    ]
    .iter()
    .any(|needle| compact.contains(&needle.replace(' ', "")))
        || has_event_attribute(&lower)
}

fn has_event_attribute(lower: &str) -> bool {
    let bytes = lower.as_bytes();
    let mut i = 0;
    while let Some(pos) = lower[i..].find("on") {
        let at = i + pos;
        let before = at.checked_sub(1).map(|b| bytes[b]);
        let boundary =
            before.is_some_and(|b| b.is_ascii_whitespace() || b == b'"' || b == b'\'' || b == b'/');
        if boundary {
            let rest = &lower[at + 2..];
            let name_len = rest.bytes().take_while(|b| b.is_ascii_alphabetic()).count();
            let after = rest[name_len..].trim_start();
            if name_len > 0 && after.starts_with('=') {
                return true;
            }
        }
        i = at + 2;
    }
    false
}

/// A PNG's pixel size when `bytes` is a well-formed PNG header.
pub fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() < 33 || bytes[..8] != SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

/// Largest embedded image a report accepts (pixels per side, bytes).
pub const MAX_IMAGE_SIDE: u32 = 8192;
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// A PNG small and well-formed enough to embed.
pub fn png_is_acceptable(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_IMAGE_BYTES
        && png_dimensions(bytes).is_some_and(|(w, h)| w <= MAX_IMAGE_SIDE && h <= MAX_IMAGE_SIDE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_covers_markup_and_attributes() {
        assert_eq!(
            xml_escape("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&apos;&amp;&apos;&lt;/a&gt;"
        );
        assert_eq!(html_escape("it's"), "it&#39;s");
        assert_eq!(xml_escape("a\u{0}b\u{7}c"), "abc");
    }

    #[test]
    fn only_safe_links_survive() {
        assert_eq!(
            safe_link("https://sqlly.app/docs"),
            Some("https://sqlly.app/docs".into())
        );
        assert_eq!(
            safe_link("#revenue-by-region"),
            Some("#revenue-by-region".into())
        );
        assert_eq!(
            safe_link("mailto:ops@example.com"),
            Some("mailto:ops@example.com".into())
        );
        for bad in [
            "javascript:alert(1)",
            " JavaScript:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html,<script>",
            "vbscript:x",
            "file:///etc/passwd",
            "https://x\" onmouseover=\"alert(1)",
            "#x\" onclick=\"y",
            "//evil.example",
        ] {
            assert_eq!(safe_link(bad), None, "{bad}");
        }
    }

    #[test]
    fn image_data_urls_must_be_raster_base64() {
        assert!(safe_image_data_url("data:image/png;base64,iVBORw0KGgo=").is_some());
        assert!(safe_image_data_url("data:image/svg+xml;base64,PHN2Zz4=").is_none());
        assert!(safe_image_data_url("data:image/png;base64,abc\" onerror=\"x").is_none());
        assert!(safe_image_data_url("https://tracker.example/pixel.png").is_none());
    }

    #[test]
    fn unsafe_svg_is_detected() {
        assert!(!svg_is_unsafe(
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><rect width=\"1\" height=\"1\"/></svg>"
        ));
        for bad in [
            "<svg><script>alert(1)</script></svg>",
            "<svg><foreignObject><div/></foreignObject></svg>",
            "<svg onload=\"alert(1)\"/>",
            "<svg><rect ONCLICK = 'x'/></svg>",
            "<svg><a href=\"javascript:alert(1)\"><text>x</text></a></svg>",
            "<svg><image href=\"https://tracker.example/p.png\"/></svg>",
            "<svg><use href=\"#a\"/></svg>",
            "<svg><style>@import url(http://x)</style></svg>",
        ] {
            assert!(svg_is_unsafe(bad), "{bad}");
        }
    }

    #[test]
    fn anchors_are_unique() {
        let mut anchors = Anchors::default();
        assert_eq!(anchors.next("Revenue by Region"), "revenue-by-region");
        assert_eq!(anchors.next("Revenue by region!"), "revenue-by-region-2");
        assert_eq!(anchors.next("日本"), "section");
    }
}
