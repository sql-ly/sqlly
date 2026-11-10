//! Pure cell-value transform macros — UPPER / lower / Title Case / trim /
//! remove diacritics — shared by the row editor's field actions and the
//! staged-edit review bar. No gpui types: every function is unit-testable in
//! isolation.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// The transform macros offered on an edited cell value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellTransform {
    Upper,
    Lower,
    TitleCase,
    Trim,
    RemoveDiacritics,
}

impl CellTransform {
    /// Menu order in both surfaces that offer transforms.
    pub fn all() -> &'static [CellTransform] {
        &[
            CellTransform::Upper,
            CellTransform::Lower,
            CellTransform::TitleCase,
            CellTransform::Trim,
            CellTransform::RemoveDiacritics,
        ]
    }

    pub fn label(self) -> String {
        self.localized_label()
    }

    pub fn localized_label(self) -> String {
        crate::localization::text(match self {
            CellTransform::Upper => "results.cell_transform.uppercase",
            CellTransform::Lower => "results.cell_transform.lowercase",
            CellTransform::TitleCase => "results.cell_transform.title_case",
            CellTransform::Trim => "results.cell_transform.trim_whitespace",
            CellTransform::RemoveDiacritics => "results.cell_transform.remove_diacritics",
        })
    }

    /// Stable menu action id (grid context menu).
    pub fn action_id(self) -> &'static str {
        match self {
            CellTransform::Upper => "cell_transform_upper",
            CellTransform::Lower => "cell_transform_lower",
            CellTransform::TitleCase => "cell_transform_title",
            CellTransform::Trim => "cell_transform_trim",
            CellTransform::RemoveDiacritics => "cell_transform_diacritics",
        }
    }

    /// Inverse of [`CellTransform::action_id`].
    pub fn from_action_id(id: &str) -> Option<CellTransform> {
        CellTransform::all()
            .iter()
            .copied()
            .find(|transform| transform.action_id() == id)
    }

    pub fn apply(self, text: &str) -> String {
        match self {
            CellTransform::Upper => text.to_uppercase(),
            CellTransform::Lower => text.to_lowercase(),
            CellTransform::TitleCase => title_case(text),
            CellTransform::Trim => text.trim().to_string(),
            CellTransform::RemoveDiacritics => remove_diacritics(text),
        }
    }
}

/// Capitalize the first letter of every word, lowercasing the rest. Works on
/// the NFC form so a decomposed `e` + U+0301 is one letter, and a combining
/// mark never starts a new word (`école` stays `École`, not `ÉCole`). A word
/// boundary is any character that is not a letter, digit, combining mark, or
/// apostrophe, so "don't" keeps its interior lowercase while "san-francisco"
/// title-cases both halves. Word starts use *titlecase* mappings, which
/// differ from uppercase for ligatures and digraphs: `ß` → `Ss`, `ﬁ` → `Fi`,
/// `ǆ` → `ǅ`.
pub fn title_case(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut boundary = true;
    for ch in text.nfc() {
        if is_combining_mark(ch) {
            // Marks attach to the preceding letter and never break a word.
            out.push(ch);
        } else if ch.is_alphabetic() {
            if boundary {
                push_titlecase(ch, &mut out);
            } else {
                out.extend(ch.to_lowercase());
            }
            boundary = false;
        } else {
            out.push(ch);
            boundary = !(ch.is_alphanumeric() || ch == '\'' || ch == '\u{2019}');
        }
    }
    out
}

/// Append the Unicode titlecase mapping of `ch`. Rust's std has no
/// titlecase, so: the Latin digraph letters and Greek iota-subscript
/// capitals (whose titlecase is a distinct single code point) come from a
/// table; everything else is the uppercase mapping with any expansion past
/// the first character lowercased (`ß` → `SS` → `Ss`, `ﬃ` → `FFI` → `Ffi`),
/// except `ŉ`, whose titlecase is its full uppercase `ʼN`.
fn push_titlecase(ch: char, out: &mut String) {
    let single = match ch {
        '\u{01C4}'..='\u{01C6}' => Some('\u{01C5}'),
        '\u{01C7}'..='\u{01C9}' => Some('\u{01C8}'),
        '\u{01CA}'..='\u{01CC}' => Some('\u{01CB}'),
        '\u{01F1}'..='\u{01F3}' => Some('\u{01F2}'),
        // Greek with ypogegrammeni: lowercase → prosgegrammeni capital.
        '\u{1F80}'..='\u{1F87}' | '\u{1F90}'..='\u{1F97}' | '\u{1FA0}'..='\u{1FA7}' => {
            char::from_u32(ch as u32 + 8)
        }
        '\u{1F88}'..='\u{1F8F}'
        | '\u{1F98}'..='\u{1F9F}'
        | '\u{1FA8}'..='\u{1FAF}'
        | '\u{1FBC}'
        | '\u{1FCC}'
        | '\u{1FFC}' => Some(ch),
        '\u{1FB3}' => Some('\u{1FBC}'),
        '\u{1FC3}' => Some('\u{1FCC}'),
        '\u{1FF3}' => Some('\u{1FFC}'),
        _ => None,
    };
    if let Some(title) = single {
        out.push(title);
        return;
    }
    if ch == '\u{0149}' {
        out.extend(ch.to_uppercase());
        return;
    }
    let mut upper = ch.to_uppercase();
    if let Some(first) = upper.next() {
        out.push(first);
    }
    for rest in upper {
        out.extend(rest.to_lowercase());
    }
}

/// Strip diacritical marks from Latin, Greek, and Cyrillic letters: NFD-
/// decompose, drop combining marks that follow a base letter from those
/// scripts, then NFC-recompose, so "é" → "e" and "Ά" → "Α". Marks on any
/// other script are kept: in Devanagari, Thai, and many others the "marks"
/// are vowel signs and viramas that are part of the spelling, and Hangul
/// syllables decompose into jamo that must be recomposed intact. A handful
/// of Latin letters have no NFD decomposition (Ł, Ø, ß, …) — a supplementary
/// map folds those to their ASCII base first.
pub fn remove_diacritics(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    // Whether the most recent base (non-mark) character is one whose marks
    // are diacritics we strip.
    let mut strip_marks = false;
    for ch in text.chars() {
        if let Some(base) = supplementary_latin_base(ch) {
            stripped.push_str(base);
            strip_marks = true;
            continue;
        }
        for part in ch.nfd() {
            if is_combining_mark(part) {
                if !strip_marks {
                    stripped.push(part);
                }
            } else {
                strip_marks = has_strippable_diacritics(part);
                stripped.push(part);
            }
        }
    }
    stripped.nfc().collect()
}

/// Latin (incl. IPA and the extended blocks), Greek, and Cyrillic letters:
/// the scripts whose combining marks are removable accents rather than part
/// of the spelling.
fn has_strippable_diacritics(ch: char) -> bool {
    matches!(ch as u32,
        0x0041..=0x005A
        | 0x0061..=0x007A
        | 0x00C0..=0x024F // Latin-1 letters, Latin Extended-A/B
        | 0x0250..=0x02AF // IPA extensions
        | 0x1D00..=0x1DBF // phonetic extensions
        | 0x1E00..=0x1EFF // Latin Extended Additional
        | 0x2C60..=0x2C7F // Latin Extended-C
        | 0xA720..=0xA7FF // Latin Extended-D
        | 0xAB30..=0xAB6F // Latin Extended-E
        | 0xFF21..=0xFF3A
        | 0xFF41..=0xFF5A // fullwidth Latin
        | 0x0370..=0x03FF // Greek and Coptic
        | 0x1F00..=0x1FFF // Greek Extended
        | 0x0400..=0x052F // Cyrillic + supplement
        | 0x1C80..=0x1C8F // Cyrillic Extended-C
        | 0x2DE0..=0x2DFF // Cyrillic Extended-A
        | 0xA640..=0xA69F // Cyrillic Extended-B
    ) && ch != '\u{00D7}'
        && ch != '\u{00F7}'
}

/// ASCII base for Latin letters that carry a diacritic but have no canonical
/// NFD decomposition.
fn supplementary_latin_base(ch: char) -> Option<&'static str> {
    Some(match ch {
        'Ø' => "O",
        'ø' => "o",
        'Ł' => "L",
        'ł' => "l",
        'Ð' => "D",
        'ð' => "d",
        'Đ' => "D",
        'đ' => "d",
        'Þ' => "TH",
        'þ' => "th",
        'ß' => "ss",
        'ẞ' => "SS",
        'Æ' => "AE",
        'æ' => "ae",
        'Œ' => "OE",
        'œ' => "oe",
        'Ŋ' => "N",
        'ŋ' => "n",
        'Ħ' => "H",
        'ħ' => "h",
        'ı' => "i",
        'Ŧ' => "T",
        'ŧ' => "t",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upper_and_lower_are_unicode_aware() {
        assert_eq!(CellTransform::Upper.apply("café"), "CAFÉ");
        assert_eq!(CellTransform::Lower.apply("CAFÉ"), "café");
        assert_eq!(CellTransform::Upper.apply("straße"), "STRASSE");
    }

    #[test]
    fn title_case_capitalizes_each_word_and_lowercases_the_rest() {
        assert_eq!(title_case("hello world"), "Hello World");
        assert_eq!(title_case("ALICE SMITH"), "Alice Smith");
        assert_eq!(title_case("san-francisco"), "San-Francisco");
        assert_eq!(title_case(""), "");
    }

    #[test]
    fn title_case_treats_combining_marks_as_part_of_the_word() {
        // NFD input: "e" + U+0301 must title-case as one letter, not a word
        // boundary followed by a fresh capital.
        assert_eq!(title_case("e\u{301}cole normale"), "École Normale");
        assert_eq!(title_case("ÉCOLE"), "École");
        // A mark with no precomposed form stays attached, lowercase run intact.
        assert_eq!(title_case("q\u{307}ab"), "Q\u{307}ab");
    }

    #[test]
    fn title_case_uses_titlecase_not_uppercase_for_word_starts() {
        assert_eq!(title_case("ßig"), "Ssig");
        assert_eq!(title_case("ﬁne ﬂow"), "Fine Flow");
        assert_eq!(title_case("ǆungla"), "ǅungla");
        assert_eq!(title_case("ᾳ"), "ᾼ");
    }

    #[test]
    fn title_case_keeps_apostrophe_interiors_lower() {
        assert_eq!(title_case("DON'T PANIC"), "Don't Panic");
        assert_eq!(title_case("l’hôtel"), "L’hôtel");
    }

    #[test]
    fn trim_strips_edges_only() {
        assert_eq!(
            CellTransform::Trim.apply("  padded text  \n"),
            "padded text"
        );
        assert_eq!(CellTransform::Trim.apply("already clean"), "already clean");
        assert_eq!(CellTransform::Trim.apply("   "), "");
    }

    #[test]
    fn remove_diacritics_strips_latin_marks() {
        assert_eq!(remove_diacritics("café"), "cafe");
        assert_eq!(remove_diacritics("Crème Brûlée"), "Creme Brulee");
        assert_eq!(remove_diacritics("São Tomé"), "Sao Tome");
    }

    #[test]
    fn remove_diacritics_folds_non_decomposable_latin_letters() {
        // Ł, Ø, ß and friends have no NFD decomposition; the supplementary
        // map is what folds them.
        assert_eq!(remove_diacritics("Łódź"), "Lodz");
        assert_eq!(remove_diacritics("København"), "Kobenhavn");
        assert_eq!(remove_diacritics("straße"), "strasse");
    }

    #[test]
    fn remove_diacritics_strips_greek_and_cyrillic_accents() {
        assert_eq!(remove_diacritics("Ἀθῆναι"), "Αθηναι");
        assert_eq!(remove_diacritics("ё"), "е");
    }

    #[test]
    fn remove_diacritics_handles_decomposed_latin_input() {
        // Already-NFD input folds the same as precomposed input, and the
        // output is NFC.
        assert_eq!(remove_diacritics("Cre\u{300}me"), "Creme");
        assert_eq!(remove_diacritics("a\u{308}\u{301}"), "a");
    }

    #[test]
    fn remove_diacritics_keeps_marks_that_are_part_of_other_scripts() {
        // Devanagari vowel signs and virama are spelling, not accents.
        assert_eq!(remove_diacritics("हिन्दी"), "हिन्दी");
        // Thai vowel and tone marks likewise.
        assert_eq!(remove_diacritics("ภาษาไทย"), "ภาษาไทย");
        assert_eq!(remove_diacritics("สวัสดี"), "สวัสดี");
        // Hangul syllables decompose to jamo under NFD; they must come back
        // recomposed, not as loose jamo.
        assert_eq!(remove_diacritics("한국어"), "한국어");
        // Mixed text: only the Latin accent goes.
        assert_eq!(remove_diacritics("café 한국 हिन्दी"), "cafe 한국 हिन्दी");
        // A Latin combining mark after a non-Latin base stays with it.
        assert_eq!(remove_diacritics("日\u{301}"), "日\u{301}");
    }

    #[test]
    fn remove_diacritics_leaves_unmarked_text_alone() {
        assert_eq!(remove_diacritics("plain ASCII 123"), "plain ASCII 123");
        assert_eq!(remove_diacritics(""), "");
        // No decomposition exists for these scripts; they pass through.
        assert_eq!(remove_diacritics("日本語"), "日本語");
    }

    #[test]
    fn action_ids_round_trip_and_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for transform in CellTransform::all() {
            assert!(seen.insert(transform.action_id()));
            assert_eq!(
                CellTransform::from_action_id(transform.action_id()),
                Some(*transform)
            );
        }
        assert_eq!(CellTransform::from_action_id("cell_set_null"), None);
    }

    #[test]
    fn all_covers_every_variant_in_menu_order() {
        let all = CellTransform::all();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0], CellTransform::Upper);
        assert_eq!(all[4], CellTransform::RemoveDiacritics);
        for transform in all {
            assert!(!transform.label().is_empty());
        }
    }
}
