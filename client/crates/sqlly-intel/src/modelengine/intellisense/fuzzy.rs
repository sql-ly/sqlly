//! Fuzzy symbol matching, mirroring the Swift `CompletionMatcher` semantics:
//! non-keyword candidates always match on *contains* as the floor, word-start
//! capital-letter (camel-hump) matching is supported, and a query typed with
//! interior capitals (`PuOr`) is explicit camel intent — each chunk must begin
//! a word, and a hit outranks a plain prefix hit, so `PuOr` puts
//! `PurchaseOrder` above `Purple`.

/// Capital-intent camel match where every lowercase continuation also
/// fits (`SoTa` on `SomeTable`).
pub const CAMEL_SCORE: i32 = 25;
pub const PREFIX_SCORE: i32 = 15;
pub const ACRONYM_SCORE: i32 = 9;
pub const CONTAINS_SCORE: i32 = 0;

/// Score of `query` against `label`, or None when nothing matches.
/// Empty queries match everything with no bonus.
///
/// Runs once per candidate per keystroke, so it must not allocate on the
/// common paths (AGENTS.md): comparisons case-fold char-wise instead of
/// building lowercased copies, and segments are byte ranges, not Strings.
pub fn score(query: &str, label: &str) -> Option<i32> {
    if !crate::matching::hump_matching() {
        return crate::matching::plain_score(query, label, PREFIX_SCORE, CONTAINS_SCORE);
    }
    if query.is_empty() {
        return Some(0);
    }

    // Interior capitals (more than one chunk) = explicit camel intent.
    let chunks = query_chunks(query);
    if chunks.len() > 1 {
        let segs = segments(label);
        // Every chunk must begin a word: `GeNe` finds `GeoNetworks` but
        // `GeNw` does not, since no word starts `Nw` (feedback ifb_2ff4).
        if place_chunks(query, &chunks, label, &segs) {
            return Some(CAMEL_SCORE);
        }
    }
    if ci_starts_with(label, query) {
        return Some(PREFIX_SCORE);
    }
    if !chunks.is_empty() && place_chunks(query, &chunks, label, &segments(label)) {
        return Some(ACRONYM_SCORE);
    }
    if ci_contains(label, query) {
        return Some(CONTAINS_SCORE);
    }
    None
}

/// Case-insensitive "starts with", allocation-free: both sides fold
/// char-wise (identical to `str::to_lowercase` comparison for ASCII, and
/// for all inputs without the final-sigma special case).
fn ci_starts_with(hay: &str, needle: &str) -> bool {
    let mut h = hay.chars().flat_map(char::to_lowercase);
    let mut n = needle.chars().flat_map(char::to_lowercase);
    loop {
        match (n.next(), h.next()) {
            (None, _) => return true,
            (Some(a), Some(b)) if a == b => continue,
            _ => return false,
        }
    }
}

/// Case-insensitive "contains" with an allocation-free ASCII fast path.
fn ci_contains(hay: &str, needle: &str) -> bool {
    if hay.is_ascii() && needle.is_ascii() {
        if needle.is_empty() {
            return true;
        }
        let h = hay.as_bytes();
        let n = needle.as_bytes();
        if h.len() < n.len() {
            return false;
        }
        return h
            .windows(n.len())
            .any(|w| w.iter().zip(n).all(|(a, b)| a.eq_ignore_ascii_case(b)));
    }
    hay.to_lowercase().contains(&needle.to_lowercase())
}

/// Word-start segments of a label across camel humps, `XMLParser`-style
/// runs, and `_`/punctuation separators, as byte ranges into `label`.
fn segments(label: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut iter = label.char_indices().peekable();
    let mut prev: Option<char> = None;
    let mut seg_start: Option<usize> = None;
    while let Some((i, c)) = iter.next() {
        if !c.is_alphanumeric() {
            if let Some(s) = seg_start.take() {
                out.push((s, i));
            }
            prev = Some(c);
            continue;
        }
        if let Some(s) = seg_start {
            let boundary = match prev {
                Some(p) if c.is_uppercase() && (p.is_lowercase() || p.is_numeric()) => true,
                Some(p)
                    if c.is_uppercase()
                        && p.is_uppercase()
                        && iter.peek().is_some_and(|(_, next)| next.is_lowercase()) =>
                {
                    true
                }
                _ => false,
            };
            if boundary {
                out.push((s, i));
                seg_start = Some(i);
            }
        } else {
            seg_start = Some(i);
        }
        prev = Some(c);
    }
    if let Some(s) = seg_start {
        out.push((s, label.len()));
    }
    out
}

/// Query chunks: each an initial plus its lowercase continuation, as byte
/// ranges into `query`. `PoOr` -> [Po, Or]; `ST` -> [S, T]; `user` ->
/// [user].
fn query_chunks(query: &str) -> Vec<(usize, usize)> {
    let mut chunks = Vec::new();
    let mut start = 0usize;
    for (i, c) in query.char_indices() {
        if i > 0 && c.is_uppercase() {
            chunks.push((start, i));
            start = i;
        }
    }
    if start < query.len() {
        chunks.push((start, query.len()));
    }
    chunks
}

/// Greedy placement: every chunk (initial + continuation) must be a
/// case-insensitive prefix of some later segment.
fn place_chunks(
    query: &str,
    chunks: &[(usize, usize)],
    label: &str,
    segs: &[(usize, usize)],
) -> bool {
    let mut seg_index = 0;
    'chunks: for &(cs, ce) in chunks {
        let want = &query[cs..ce];
        while seg_index < segs.len() {
            let (ss, se) = segs[seg_index];
            seg_index += 1;
            if ci_starts_with(&label[ss..se], want) {
                continue 'chunks;
            }
        }
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use crate::modelengine::intellisense::fuzzy;

    fn with_humps<R>(hump_matching: bool, f: impl FnOnce() -> R) -> R {
        crate::matching::with_match_options(
            crate::matching::MatchOptions {
                hump_matching,
                ..Default::default()
            },
            f,
        )
    }

    /// Hump matching off: only case-insensitive prefix/substring hits, so the
    /// camel/acronym queries the tests below rely on stop matching.
    #[test]
    fn humps_off_matches_case_insensitive_prefix_and_substring_only() {
        with_humps(false, || {
            assert_eq!(fuzzy::score("PoOr", "PurchaseOrder"), None);
            assert_eq!(fuzzy::score("PoOr", "Poor"), Some(fuzzy::PREFIX_SCORE));
            assert_eq!(fuzzy::score("SoTa", "SomeTable"), None);
            assert_eq!(fuzzy::score("ST", "SomeTable"), None);
            assert_eq!(fuzzy::score("ST", "some_table"), None);
            assert_eq!(
                fuzzy::score("sometab", "SomeTable"),
                Some(fuzzy::PREFIX_SCORE)
            );
            assert_eq!(
                fuzzy::score("TABLE", "SomeTable"),
                Some(fuzzy::CONTAINS_SCORE)
            );
            assert_eq!(
                fuzzy::score("user", "CreatedByUser"),
                Some(fuzzy::CONTAINS_SCORE)
            );
            assert_eq!(fuzzy::score("", "Whatever"), Some(0));
        });
    }

    #[test]
    fn humps_on_keeps_camel_and_acronym_tiers() {
        with_humps(true, || {
            assert_eq!(
                fuzzy::score("PuOr", "PurchaseOrder"),
                Some(fuzzy::CAMEL_SCORE)
            );
            assert_eq!(fuzzy::score("ST", "SomeTable"), Some(fuzzy::CAMEL_SCORE));
            assert_eq!(
                fuzzy::score("user", "CreatedByUser"),
                Some(fuzzy::ACRONYM_SCORE)
            );
        });
    }

    #[test]
    fn contains_is_the_floor_for_symbols() {
        // `user` matches CreatedByUser (word start) and anything with "user".
        assert_eq!(
            fuzzy::score("user", "CreatedByUser"),
            Some(fuzzy::ACRONYM_SCORE)
        );
        assert_eq!(
            fuzzy::score("user", "TenantUserName"),
            Some(fuzzy::ACRONYM_SCORE)
        );
        assert_eq!(
            fuzzy::score("user", "superuser_audit"),
            Some(fuzzy::CONTAINS_SCORE)
        );
        assert_eq!(fuzzy::score("user", "UserName"), Some(fuzzy::PREFIX_SCORE));
        assert_eq!(fuzzy::score("user", "Account"), None);
    }

    #[test]
    fn interior_capitals_mean_camel_first() {
        // PuOr: each chunk begins a word of PurchaseOrder, so it is a camel
        // hit, well above the literal prefix match on Purple.
        let purchase = fuzzy::score("PuOr", "PurchaseOrder").expect("camel hit");
        let purple = fuzzy::score("Pur", "Purple").expect("prefix hit");
        assert_eq!(purchase, fuzzy::CAMEL_SCORE);
        assert_eq!(purple, fuzzy::PREFIX_SCORE);
        assert!(purchase > purple);
    }

    /// Every chunk must begin a word: `So` does not begin `Same` and `Nw`
    /// does not begin `Networks`, so neither is a camel hit (feedback
    /// ifb_2ff4); `PoOr` then falls back to its literal prefix on `Poor`.
    #[test]
    fn a_chunk_that_begins_no_word_is_not_a_camel_match() {
        assert_eq!(fuzzy::score("SoTa", "SomeTable"), Some(fuzzy::CAMEL_SCORE));
        assert_eq!(fuzzy::score("SoTa", "SameTable"), None);
        assert_eq!(
            fuzzy::score("GeNe", "GeoNetworks"),
            Some(fuzzy::CAMEL_SCORE)
        );
        assert_eq!(fuzzy::score("GeNw", "GeoNetworks"), None);
        assert_eq!(fuzzy::score("PoOr", "PurchaseOrder"), None);
        assert_eq!(fuzzy::score("PoOr", "Poor"), Some(fuzzy::PREFIX_SCORE));
    }

    /// A camel hit outranks a case-insensitive prefix: `GEONe` lands on every
    /// word of `GeoEngineOrderNetwork` but is only a prefix of `GeoNetworks`.
    #[test]
    fn camel_hit_outranks_case_insensitive_prefix() {
        let camel = fuzzy::score("GEONe", "GeoEngineOrderNetwork").unwrap();
        let prefix = fuzzy::score("GEONe", "GeoNetworks").unwrap();
        assert_eq!(camel, fuzzy::CAMEL_SCORE);
        assert_eq!(prefix, fuzzy::PREFIX_SCORE);
    }

    #[test]
    fn bare_initials_match_snake_and_pascal() {
        assert_eq!(fuzzy::score("ST", "SomeTable"), Some(fuzzy::CAMEL_SCORE));
        // Typed capitals land on snake_case word starts too.
        assert_eq!(fuzzy::score("ST", "some_table"), Some(fuzzy::CAMEL_SCORE));
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(fuzzy::score("", "Whatever"), Some(0));
    }
}
