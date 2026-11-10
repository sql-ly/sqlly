//! Tokenizer for natural-date phrases.
//!
//! The grammar matches whole token slices, so the lexer's job is to make
//! every surface spelling of the same thing look identical: lowercase words
//! with apostrophes dropped ("new year's" == "new years"), `5th` as one
//! ordinal token, `'24` as a short year, commas and "the" discarded.

/// An owned token, produced by [`lex`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Lexeme {
    Word(String),
    /// Digits as written. `digits` keeps the spelled length so "05" (an
    /// explicit 24-hour reading, a 2-digit year) differs from "5".
    /// Values too large for a `u64` saturate so later checked arithmetic
    /// rejects them instead of the lexer guessing.
    Num {
        value: u64,
        digits: usize,
    },
    Ordinal(u32),
    /// `'24`.
    ShortYear(u32),
    Colon,
    Slash,
    Dash,
    Dot,
}

/// A borrowed view of a [`Lexeme`] that slice patterns can match against
/// string literals directly (`[T::W("last"), rest @ ..]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum T<'a> {
    W(&'a str),
    N(u64, usize),
    Ord(u32),
    Yr2(u32),
    Colon,
    Slash,
    Dash,
    Dot,
}

impl Lexeme {
    pub(super) fn view(&self) -> T<'_> {
        match self {
            Lexeme::Word(w) => T::W(w),
            Lexeme::Num { value, digits } => T::N(*value, *digits),
            Lexeme::Ordinal(n) => T::Ord(*n),
            Lexeme::ShortYear(y) => T::Yr2(*y),
            Lexeme::Colon => T::Colon,
            Lexeme::Slash => T::Slash,
            Lexeme::Dash => T::Dash,
            Lexeme::Dot => T::Dot,
        }
    }
}

const QUOTES: &[char] = &[
    '"', '\'', '`', '\u{201c}', '\u{201d}', '\u{2018}', '\u{2019}',
];
const TRAILING_PUNCT: &[char] = &['.', ',', ';', '!', '?'];

/// Trim, lowercase, peel one layer of surrounding quotes and any trailing
/// sentence punctuation, and fold "a.m."/"p.m." to "am"/"pm" (otherwise the
/// trailing-dot strip would leave a dangling "p.m").
pub(super) fn normalize(input: &str) -> String {
    let mut s = input.trim().to_lowercase();
    loop {
        let t = s.trim();
        let mut chars = t.chars();
        let (first, last) = (chars.next(), chars.next_back());
        match (first, last) {
            (Some(a), Some(b)) if QUOTES.contains(&a) && QUOTES.contains(&b) => {
                let inner = &t[a.len_utf8()..t.len() - b.len_utf8()];
                s = inner.trim().to_string();
            }
            _ => break,
        }
    }
    s = s.replace("a.m.", "am").replace("p.m.", "pm");
    s = s.replace("a.m", "am").replace("p.m", "pm");
    s.trim()
        .trim_end_matches(|c: char| TRAILING_PUNCT.contains(&c) || c.is_whitespace())
        .to_string()
}

fn is_apostrophe(c: char) -> bool {
    c == '\'' || c == '\u{2019}'
}

/// Split normalized text into tokens. `None` means a character no phrase
/// uses, which makes the whole input unparseable (better than ignoring it).
pub(super) fn lex(s: &str) -> Option<Vec<Lexeme>> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() || c == ',' {
            i += 1;
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            let value = text.parse::<u64>().unwrap_or(u64::MAX);
            let suffix: String = chars[i..chars.len().min(i + 2)].iter().collect();
            let after_suffix = chars.get(i + 2).is_some_and(|c| c.is_alphabetic());
            if matches!(suffix.as_str(), "st" | "nd" | "rd" | "th") && !after_suffix {
                out.push(Lexeme::Ordinal(u32::try_from(value).unwrap_or(u32::MAX)));
                i += 2;
            } else {
                out.push(Lexeme::Num {
                    value,
                    digits: text.len(),
                });
            }
        } else if c.is_alphabetic() {
            let mut word = String::new();
            while i < chars.len() {
                let ch = chars[i];
                if ch.is_alphabetic() {
                    word.push(ch);
                } else if !is_apostrophe(ch) {
                    break;
                }
                i += 1;
            }
            out.push(Lexeme::Word(word));
        } else if is_apostrophe(c) {
            // `'24` — the only place a free-standing apostrophe is meaningful.
            let start = i + 1;
            let mut end = start;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            if end - start != 2 {
                return None;
            }
            let text: String = chars[start..end].iter().collect();
            out.push(Lexeme::ShortYear(text.parse().ok()?));
            i = end;
        } else {
            out.push(match c {
                ':' => Lexeme::Colon,
                '/' => Lexeme::Slash,
                '-' | '\u{2013}' | '\u{2014}' => Lexeme::Dash,
                '.' => Lexeme::Dot,
                '@' => Lexeme::Word("at".to_string()),
                _ => return None,
            });
            i += 1;
        }
    }
    out.retain(|l| !matches!(l, Lexeme::Word(w) if w == "the"));
    Some(out)
}
