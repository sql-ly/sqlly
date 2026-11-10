//! Pure find/replace match engine for the SQL editor.
//!
//! Everything here operates on byte offsets into the editor's UTF-8 buffer
//! (the same coordinate space as `SqlEditor::cursor` / `selection_anchor`)
//! and is free of gpui types, so the whole module is unit-testable without a
//! window. The editor's find bar is a thin shell over these functions.

use std::ops::Range;

use regex::{Regex, RegexBuilder};

/// Maximum compiled regex size. Rust's regex engine is linear-time, and this
/// additional bound prevents an enormous generated pattern from consuming an
/// unreasonable amount of memory before the match cap can help.
const REGEX_PATTERN_BYTES_LIMIT: usize = 64 * 1024;
const REGEX_SIZE_LIMIT: usize = 1 << 20;
const REGEX_DFA_SIZE_LIMIT: usize = 2 << 20;

/// A find-bar query. Compiling it once lets repeated buffer rescans reuse the
/// same regex program while the user edits the document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindQuery {
    pub text: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

impl FindQuery {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            case_sensitive: false,
            whole_word: false,
            regex: false,
        }
    }

    pub fn case_sensitive(mut self, on: bool) -> Self {
        self.case_sensitive = on;
        self
    }

    pub fn whole_word(mut self, on: bool) -> Self {
        self.whole_word = on;
        self
    }

    pub fn regex(mut self, on: bool) -> Self {
        self.regex = on;
        self
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn compile(&self) -> Result<CompiledFindQuery, FindCompileError> {
        let matcher = if self.regex {
            if self.text.len() > REGEX_PATTERN_BYTES_LIMIT {
                return Err(FindCompileError {
                    message: crate::localization::format(
                        "editor.find_replace.pattern_too_large",
                        &[("limit_kib", &(REGEX_PATTERN_BYTES_LIMIT / 1024).to_string())],
                    ),
                });
            }
            let regex = RegexBuilder::new(&self.text)
                .case_insensitive(!self.case_sensitive)
                // ^ and $ address lines in the editor buffer; `.` deliberately
                // does not cross a newline unless the pattern opts in with
                // `(?s)`, matching mainstream editor behavior.
                .multi_line(true)
                .size_limit(REGEX_SIZE_LIMIT)
                .dfa_size_limit(REGEX_DFA_SIZE_LIMIT)
                .build()
                .map_err(|error| FindCompileError {
                    message: error.to_string(),
                })?;
            Matcher::Regex(regex)
        } else {
            Matcher::Plain(self.text.chars().collect())
        };
        Ok(CompiledFindQuery {
            source: self.clone(),
            matcher,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindCompileError {
    pub message: String,
}

impl std::fmt::Display for FindCompileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for FindCompileError {}

#[derive(Clone)]
enum Matcher {
    Plain(Vec<char>),
    Regex(Regex),
}

/// A query compiled independently of the document text. Regex values are
/// cheap to clone and retain their compiled program, so the editor can keep
/// this beside the find-bar state and reuse it across debounced rescans.
#[derive(Clone)]
pub struct CompiledFindQuery {
    source: FindQuery,
    matcher: Matcher,
}

impl CompiledFindQuery {
    pub fn source(&self) -> &FindQuery {
        &self.source
    }

    pub fn find_matches(&self, text: &str) -> (Vec<Range<usize>>, bool) {
        self.find_matches_capped(text, MAX_MATCHES)
    }

    pub fn find_matches_capped(&self, text: &str, cap: usize) -> (Vec<Range<usize>>, bool) {
        let mut matches = Vec::new();
        if self.source.is_empty() || cap == 0 {
            return (matches, false);
        }
        match &self.matcher {
            Matcher::Plain(needle) => {
                let mut start = 0;
                while start <= text.len() {
                    if !text.is_char_boundary(start) {
                        start += 1;
                        continue;
                    }
                    if let Some(end) = match_at(text, start, needle, self.source.case_sensitive) {
                        let range = start..end;
                        if !self.source.whole_word || whole_word_ok(text, &range) {
                            matches.push(range);
                            if matches.len() >= cap {
                                return (matches, true);
                            }
                        }
                        start = end;
                    } else {
                        start += 1;
                    }
                }
            }
            Matcher::Regex(regex) => {
                for found in regex.find_iter(text) {
                    let range = found.range();
                    if self.source.whole_word && !whole_word_ok(text, &range) {
                        continue;
                    }
                    matches.push(range);
                    if matches.len() >= cap {
                        return (matches, true);
                    }
                }
            }
        }
        (matches, false)
    }

    /// Expand a replacement for one exact match. Plain searches return the
    /// replacement verbatim; regex searches support `$1` and `${name}` using
    /// the capture groups from this occurrence.
    pub fn replacement_for(&self, text: &str, range: &Range<usize>, replacement: &str) -> String {
        let Matcher::Regex(regex) = &self.matcher else {
            return replacement.to_string();
        };
        let Some(captures) = regex.captures_at(text, range.start) else {
            return replacement.to_string();
        };
        let Some(whole) = captures.get(0) else {
            return replacement.to_string();
        };
        if whole.start() != range.start || whole.end() != range.end {
            return replacement.to_string();
        }
        let mut expanded = String::new();
        captures.expand(replacement, &mut expanded);
        expanded
    }

    pub fn replace_all(&self, text: &str, matches: &[Range<usize>], replacement: &str) -> String {
        if matches.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut last_end = 0;
        for matched in matches {
            out.push_str(&text[last_end..matched.start]);
            out.push_str(&self.replacement_for(text, matched, replacement));
            last_end = matched.end;
        }
        out.push_str(&text[last_end..]);
        out
    }
}

/// Hard cap on collected matches. Highlighting and navigation degrade
/// gracefully past this (the bar shows "10,000+"), and the cap keeps a
/// one-character query over a huge script from allocating unbounded state.
pub const MAX_MATCHES: usize = 10_000;

/// Word characters for whole-word matching: the SQL identifier alphabet.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn chars_equal(a: char, b: char, case_sensitive: bool) -> bool {
    if case_sensitive {
        a == b
    } else if a.is_ascii() && b.is_ascii() {
        a.eq_ignore_ascii_case(&b)
    } else {
        a.to_lowercase().eq(b.to_lowercase())
    }
}

/// If the needle matches `text` starting at byte `start` (a char boundary),
/// returns the byte offset just past the match.
fn match_at(text: &str, start: usize, needle: &[char], case_sensitive: bool) -> Option<usize> {
    let mut end = start;
    let mut hay = text[start..].chars();
    for &n in needle {
        let h = hay.next()?;
        if !chars_equal(h, n, case_sensitive) {
            return None;
        }
        end += h.len_utf8();
    }
    Some(end)
}

fn whole_word_ok(text: &str, range: &Range<usize>) -> bool {
    let before_is_word = text[..range.start]
        .chars()
        .next_back()
        .is_some_and(is_word_char);
    let after_is_word = text[range.end..].chars().next().is_some_and(is_word_char);
    !before_is_word && !after_is_word
}

/// All matches of `query` in `text`, ascending and non-overlapping, capped at
/// [`MAX_MATCHES`]. The bool is `true` when the cap cut the scan short.
#[cfg(test)]
pub fn find_matches(text: &str, query: &FindQuery) -> (Vec<Range<usize>>, bool) {
    find_matches_capped(text, query, MAX_MATCHES)
}

#[cfg(test)]
pub fn find_matches_capped(text: &str, query: &FindQuery, cap: usize) -> (Vec<Range<usize>>, bool) {
    match query.compile() {
        Ok(compiled) => compiled.find_matches_capped(text, cap),
        Err(_) => (Vec::new(), false),
    }
}

/// Splice `replacement` in for every range in `matches` (ascending,
/// non-overlapping — the shape `find_matches` returns) in a single pass.
#[cfg(test)]
pub fn replace_all(text: &str, matches: &[Range<usize>], replacement: &str) -> String {
    if matches.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut last_end = 0;
    for m in matches {
        out.push_str(&text[last_end..m.start]);
        out.push_str(replacement);
        last_end = m.end;
    }
    out.push_str(&text[last_end..]);
    out
}

/// Index of the first match starting at or after `offset`, wrapping to the
/// first match when the cursor sits past every match. `None` when empty.
pub fn initial_index_at_or_after(matches: &[Range<usize>], offset: usize) -> Option<usize> {
    if matches.is_empty() {
        return None;
    }
    Some(matches.partition_point(|m| m.start < offset) % matches.len())
}

/// Next match index with wrap-around. `None` when there are no matches.
pub fn next_index(len: usize, current: Option<usize>) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        Some(i) => (i + 1) % len,
        None => 0,
    })
}

/// Previous match index with wrap-around. `None` when there are no matches.
pub fn prev_index(len: usize, current: Option<usize>) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match current {
        Some(i) => (i + len - 1) % len,
        None => len - 1,
    })
}

/// "N of M" counter text for the find bar, with a "+" suffix on the total
/// when the match scan was truncated at the cap.
pub fn count_label(current: Option<usize>, total: usize, truncated: bool) -> String {
    if total == 0 {
        return crate::localization::text("editor.find_replace.no_matches");
    }
    let current = current.map(|i| i + 1).unwrap_or(1).to_string();
    let total = total.to_string();
    let key = if truncated {
        "editor.find_replace.count_truncated"
    } else {
        "editor.find_replace.count"
    };
    crate::localization::format(key, &[("current", &current), ("total", &total)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(text: &str, q: &FindQuery) -> Vec<Range<usize>> {
        find_matches(text, q).0
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert!(ranges("SELECT 1", &FindQuery::new("")).is_empty());
        let (_, truncated) = find_matches("SELECT 1", &FindQuery::new(""));
        assert!(!truncated);
    }

    #[test]
    fn plain_substring_finds_all_in_order() {
        let m = ranges("foo bar foo baz foo", &FindQuery::new("foo"));
        assert_eq!(m, vec![0..3, 8..11, 16..19]);
    }

    #[test]
    fn case_insensitive_by_default() {
        let m = ranges("Select SELECT select", &FindQuery::new("select"));
        assert_eq!(m, vec![0..6, 7..13, 14..20]);
    }

    #[test]
    fn case_sensitive_filters() {
        let m = ranges(
            "Select SELECT select",
            &FindQuery::new("select").case_sensitive(true),
        );
        assert_eq!(m, vec![14..20]);
    }

    #[test]
    fn whole_word_rejects_substrings() {
        let q = FindQuery::new("order").whole_word(true);
        let m = ranges("order Orders ORDER_BY order", &q);
        assert_eq!(m, vec![0..5, 22..27]);
    }

    #[test]
    fn whole_word_treats_underscore_as_word_char() {
        let q = FindQuery::new("id").whole_word(true);
        let m = ranges("id user_id id2 id", &q);
        assert_eq!(m, vec![0..2, 15..17]);
    }

    #[test]
    fn matches_do_not_overlap() {
        let m = ranges("aaaa", &FindQuery::new("aa"));
        assert_eq!(m, vec![0..2, 2..4]);
    }

    #[test]
    fn unicode_offsets_are_char_boundaries() {
        let m = ranges("sélét select sélect", &FindQuery::new("sél"));
        // é is two bytes; offsets must land on char boundaries. "select" does
        // not match (e ≠ é), while "sélect" at byte 15 does.
        assert_eq!(m, vec![0..4, 15..19]);
        let m = ranges("sélét", &FindQuery::new("lét"));
        assert_eq!(m, vec![3..7]);
    }

    #[test]
    fn case_insensitive_unicode() {
        let m = ranges("SÉLÉT", &FindQuery::new("sélét"));
        assert_eq!(m, vec![0..7]);
    }

    #[test]
    fn cap_truncates() {
        let text = "a ".repeat(20_000);
        let (m, truncated) = find_matches(&text, &FindQuery::new("a"));
        assert_eq!(m.len(), MAX_MATCHES);
        assert!(truncated);
        let (m, truncated) = find_matches_capped(&text, &FindQuery::new("a"), 5);
        assert_eq!(m.len(), 5);
        assert!(truncated);
    }

    #[test]
    fn replace_all_splices_every_match() {
        let (m, _) = find_matches("foo bar foo", &FindQuery::new("foo"));
        assert_eq!(replace_all("foo bar foo", &m, "qux"), "qux bar qux");
    }

    #[test]
    fn replace_all_with_empty_replacement_deletes() {
        let (m, _) = find_matches("a-b-c", &FindQuery::new("-"));
        assert_eq!(replace_all("a-b-c", &m, ""), "abc");
    }

    #[test]
    fn replace_all_empty_matches_is_identity() {
        assert_eq!(replace_all("untouched", &[], "x"), "untouched");
    }

    #[test]
    fn initial_index_starts_at_or_after_cursor_and_wraps() {
        let (m, _) = find_matches("a a a", &FindQuery::new("a"));
        assert_eq!(initial_index_at_or_after(&m, 0), Some(0));
        assert_eq!(initial_index_at_or_after(&m, 1), Some(1));
        assert_eq!(initial_index_at_or_after(&m, 5), Some(0));
        assert_eq!(initial_index_at_or_after(&[], 0), None);
    }

    #[test]
    fn next_and_prev_wrap() {
        assert_eq!(next_index(3, Some(0)), Some(1));
        assert_eq!(next_index(3, Some(2)), Some(0));
        assert_eq!(next_index(3, None), Some(0));
        assert_eq!(prev_index(3, Some(0)), Some(2));
        assert_eq!(prev_index(3, Some(1)), Some(0));
        assert_eq!(prev_index(3, None), Some(2));
        assert_eq!(next_index(0, None), None);
        assert_eq!(prev_index(0, None), None);
    }

    #[test]
    fn count_labels() {
        assert_eq!(count_label(None, 0, false), "No matches");
        assert_eq!(count_label(Some(0), 3, false), "1 of 3");
        assert_eq!(count_label(Some(2), 3, false), "3 of 3");
        assert_eq!(count_label(Some(0), MAX_MATCHES, true), "1 of 10000+");
    }

    #[test]
    fn invalid_regex_is_a_compile_error() {
        let error = FindQuery::new("(")
            .regex(true)
            .compile()
            .err()
            .expect("unclosed group must fail");
        assert!(!error.message.is_empty());
    }

    #[test]
    fn oversized_regex_is_rejected_before_compilation() {
        let error = FindQuery::new("a".repeat(REGEX_PATTERN_BYTES_LIMIT + 1))
            .regex(true)
            .compile()
            .err()
            .expect("oversized patterns must fail");
        assert!(error.message.contains("64 KiB"));
    }

    #[test]
    fn regex_respects_case_and_whole_word_options() {
        let insensitive = FindQuery::new("foo|bar").regex(true).compile().unwrap();
        assert_eq!(
            insensitive.find_matches("FOO foobar bar").0,
            vec![0..3, 4..7, 7..10, 11..14]
        );

        let strict = FindQuery::new("foo|bar")
            .regex(true)
            .case_sensitive(true)
            .whole_word(true)
            .compile()
            .unwrap();
        assert_eq!(
            strict.find_matches("FOO foobar bar foo").0,
            vec![11..14, 15..18]
        );
    }

    #[test]
    fn regex_matches_unicode_and_multiline_anchors_on_byte_boundaries() {
        let words = FindQuery::new(r"\p{L}+").regex(true).compile().unwrap();
        assert_eq!(words.find_matches("éclair 東京").0, vec![0..7, 8..14]);

        let starts = FindQuery::new("^").regex(true).compile().unwrap();
        let matches = starts.find_matches("α\nβ").0;
        assert_eq!(matches, vec![0..0, 3..3]);
        assert!(
            matches
                .iter()
                .all(|range| "α\nβ".is_char_boundary(range.start))
        );
    }

    #[test]
    fn regex_replacement_expands_numbered_and_named_captures() {
        let numbered = FindQuery::new(r"([a-z]+)-(\d+)")
            .regex(true)
            .case_sensitive(true)
            .compile()
            .unwrap();
        let (matches, _) = numbered.find_matches("item-12 other-3");
        assert_eq!(
            numbered.replace_all("item-12 other-3", &matches, "$2:$1"),
            "12:item 3:other"
        );

        let named = FindQuery::new(r"(?P<word>[a-z]+)")
            .regex(true)
            .compile()
            .unwrap();
        let (matches, _) = named.find_matches("one two");
        assert_eq!(
            named.replace_all("one two", &matches, "${word}_x"),
            "one_x two_x"
        );
    }

    #[test]
    fn zero_width_regex_replacement_preserves_the_buffer() {
        let compiled = FindQuery::new("^").regex(true).compile().unwrap();
        let text = "one\ntwo";
        let (matches, _) = compiled.find_matches(text);
        assert_eq!(compiled.replace_all(text, &matches, "> "), "> one\n> two");
    }

    #[test]
    fn large_script_scan_stays_bounded() {
        // 100k-line script; a 1-char query hits the cap quickly and a
        // multi-char query scans once. Both must complete without blow-up.
        let text = "SELECT column_name FROM schema.table_name WHERE id = 42;\n".repeat(100_000);
        let (m, truncated) = find_matches(&text, &FindQuery::new("e"));
        assert!(truncated);
        assert_eq!(m.len(), MAX_MATCHES);
        let (m, truncated) = find_matches_capped(&text, &FindQuery::new("table_name"), 200_000);
        assert_eq!(m.len(), 100_000);
        assert!(!truncated);

        let compiled = FindQuery::new(r"^SELECT\b").regex(true).compile().unwrap();
        let (m, truncated) = compiled.find_matches(&text);
        assert_eq!(m.len(), MAX_MATCHES);
        assert!(truncated);
    }
}
