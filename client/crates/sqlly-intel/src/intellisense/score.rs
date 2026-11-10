use std::cell::RefCell;
use std::fmt::Write;

use crate::intellisense::types::{CompletionItem, CompletionKind, CompletionSource};

const COMPLETION_PRE_SORT_CAP: usize = 2048;

thread_local! {
    static PUSH_CAP_WEAKEST: RefCell<Option<usize>> = const { RefCell::new(None) };
}

/// Clears the cached weakest index used while [`push_scored`] is at capacity.
/// Call at the start of each completion request.
pub(crate) fn reset_push_cap_state() {
    PUSH_CAP_WEAKEST.with(|cache| *cache.borrow_mut() = None);
}
// ---------------------------------------------------------------------------
// Fuzzy symbol matching — ported from sqlly-modelengine's `fuzzy` module so
// this lightweight crate gains the same matching quality (camelCase, acronym,
// word-anchored subsequence) that the intellisense cookbook validates, without
// pulling in the heavy modelengine dependency tree (rusqlite, tokio, …).
// ---------------------------------------------------------------------------

pub(crate) mod fuzzy {
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
        score_humped(query, label)
    }

    /// [`score`] with hump matching forced on, for rankers outside
    /// completion (explorer search) that the user's setting doesn't govern.
    pub fn score_humped(query: &str, label: &str) -> Option<i32> {
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
    pub(super) fn ci_starts_with(hay: &str, needle: &str) -> bool {
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
    pub(super) fn ci_contains(hay: &str, needle: &str) -> bool {
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
    pub(super) fn segments(label: &str) -> Vec<(usize, usize)> {
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
}

/// Score an unqualified object query against a fully-qualified display name
/// (`Sales.SalesOrder`). First tries the standard fuzzy ranker (so `SSO` ->
/// `Sales.SalesOrder`, `SaOr` -> `Sales.SalesOrder`). When that fails, falls
/// back to a word-anchored subsequence so an over-specified mixed query like
/// `SaSSO` still resolves, without admitting loose subsequences.
pub fn qualified_object_score(query: &str, display: &str) -> Option<i32> {
    crate::matching::qualified_score(
        query,
        display,
        crate::matching::match_options(),
        fuzzy::PREFIX_SCORE,
        fuzzy::CONTAINS_SCORE,
        humped_object_score,
    )
}

fn humped_object_score(query: &str, display: &str) -> Option<i32> {
    if let Some(score) = fuzzy::score(query, display) {
        return Some(score);
    }
    if last_run_word_anchored(query, display) {
        return Some(fuzzy::CONTAINS_SCORE);
    }
    None
}

/// Match tier for a bare object name, best first. `None` = no match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchTier {
    Exact = 0,
    Prefix = 1,
    WordPrefix = 2,
    Substring = 3,
    Abbreviation = 4,
    Fuzzy = 5,
}

/// Tiered, allocation-free-on-the-hot-path ranking of `query` against `label`.
/// Order: exact > prefix > word-prefix (a `segment_starts` boundary) >
/// substring > abbreviation (camel/acronym via `fuzzy::score`) > fuzzy
/// (`last_run_word_anchored`). Case-insensitive.
pub fn object_match_tier(query: &str, label: &str) -> Option<MatchTier> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    if query.eq_ignore_ascii_case(label) {
        return Some(MatchTier::Exact);
    }
    if fuzzy::ci_starts_with(label, query) {
        return Some(MatchTier::Prefix);
    }
    let segs = fuzzy::segments(label);
    for &(ss, se) in &segs {
        if ss == 0 {
            continue;
        }
        if fuzzy::ci_starts_with(&label[ss..se], query) {
            return Some(MatchTier::WordPrefix);
        }
    }
    if fuzzy::ci_contains(label, query) {
        return Some(MatchTier::Substring);
    }
    if let Some(score) = fuzzy::score_humped(query, label) {
        if score == fuzzy::CAMEL_SCORE || score == fuzzy::ACRONYM_SCORE {
            return Some(MatchTier::Abbreviation);
        }
        if score == fuzzy::PREFIX_SCORE {
            return Some(MatchTier::Prefix);
        }
    }
    if last_run_word_anchored(query, label) {
        return Some(MatchTier::Fuzzy);
    }
    None
}

/// Char offsets where each word-start segment of `label` begins. Mirrors the
/// segmentation in `fuzzy::segments` but keeps the positions.
fn segment_starts(label: &str) -> Vec<usize> {
    let chars: Vec<char> = label.chars().collect();
    let n = chars.len();
    let mut starts = Vec::new();
    let mut i = 0;
    while i < n {
        while i < n && !chars[i].is_alphanumeric() {
            i += 1;
        }
        if i >= n {
            break;
        }
        starts.push(i);
        i += 1;
        while i < n {
            let c = chars[i];
            if !c.is_alphanumeric() {
                break;
            }
            let prev = chars[i - 1];
            if c.is_uppercase() && (prev.is_lowercase() || prev.is_numeric()) {
                break;
            }
            if c.is_uppercase() && prev.is_uppercase() && i + 1 < n && chars[i + 1].is_lowercase() {
                break;
            }
            i += 1;
        }
    }
    starts
}

/// True when `query` is a subsequence of `label` whose final contiguous run of
/// matched characters begins on a word-start boundary, and whose leading
/// remainder is an anchored subsequence (first char pinned to `label[0]`).
pub(crate) fn last_run_word_anchored(query: &str, label: &str) -> bool {
    if query.is_empty() || label.is_empty() {
        return false;
    }
    // The anchored head pins to `label[0]`; bail before allocating when the
    // first query char cannot match.
    if !query.chars().next().is_some_and(|q| {
        label
            .chars()
            .next()
            .is_some_and(|l| q.eq_ignore_ascii_case(&l))
    }) {
        return false;
    }

    if query.is_ascii() && label.is_ascii() {
        return last_run_word_anchored_ascii(query.as_bytes(), label.as_bytes());
    }

    let q: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    let label_low: Vec<char> = label.chars().map(|c| c.to_ascii_lowercase()).collect();
    last_run_word_anchored_chars(&q, &label_low, label)
}

fn last_run_word_anchored_ascii(query: &[u8], label: &[u8]) -> bool {
    let starts = segment_starts_bytes(label);
    let m = query.len();
    let n = label.len();

    for tail_len in (1..=m).rev() {
        let head_len = m - tail_len;
        let tail = &query[head_len..];
        for &w in &starts {
            if w + tail_len <= n
                && label[w..w + tail_len]
                    .iter()
                    .zip(tail)
                    .all(|(a, b)| a.eq_ignore_ascii_case(b))
            {
                if head_len == 0 {
                    return true;
                }
                if anchored_subsequence_bytes(&query[..head_len], &label[..w]) {
                    return true;
                }
            }
        }
    }
    false
}

fn segment_starts_bytes(label: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i < label.len() {
        while i < label.len() && !label[i].is_ascii_alphanumeric() {
            i += 1;
        }
        if i >= label.len() {
            break;
        }
        starts.push(i);
        i += 1;
        while i < label.len() {
            let c = label[i];
            if !c.is_ascii_alphanumeric() {
                break;
            }
            let prev = label[i - 1];
            if c.is_ascii_uppercase() && (prev.is_ascii_lowercase() || prev.is_ascii_digit()) {
                break;
            }
            if c.is_ascii_uppercase()
                && prev.is_ascii_uppercase()
                && i + 1 < label.len()
                && label[i + 1].is_ascii_lowercase()
            {
                break;
            }
            i += 1;
        }
    }
    starts
}

/// `needle` is a subsequence of `hay` with its first character pinned to
/// `hay[0]` (ASCII case-insensitive).
fn anchored_subsequence_bytes(needle: &[u8], hay: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.is_empty() || !needle[0].eq_ignore_ascii_case(&hay[0]) {
        return false;
    }
    let mut ni = 1usize;
    let mut hi = 1usize;
    while ni < needle.len() && hi < hay.len() {
        if needle[ni].eq_ignore_ascii_case(&hay[hi]) {
            ni += 1;
        }
        hi += 1;
    }
    ni == needle.len()
}

fn last_run_word_anchored_chars(q: &[char], label_low: &[char], label: &str) -> bool {
    let n = label_low.len();
    let m = q.len();
    let starts = segment_starts(label);

    for tail_len in (1..=m).rev() {
        let head_len = m - tail_len;
        let tail = &q[head_len..];
        for &w in &starts {
            if w + tail_len <= n && label_low[w..w + tail_len] == *tail {
                if head_len == 0 {
                    return true;
                }
                if anchored_subsequence(&q[..head_len], &label_low[..w]) {
                    return true;
                }
            }
        }
    }
    false
}

/// `needle` is a subsequence of `hay` with its first character pinned to
/// `hay[0]` (case must already be normalized by the caller).
fn anchored_subsequence(needle: &[char], hay: &[char]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if hay.is_empty() || hay[0] != needle[0] {
        return false;
    }
    let mut ni = 1;
    let mut hi = 1;
    while ni < needle.len() && hi < hay.len() {
        if needle[ni] == hay[hi] {
            ni += 1;
        }
        hi += 1;
    }
    ni == needle.len()
}

pub(crate) fn push_match(
    items: &mut Vec<CompletionItem>,
    label: &str,
    insert_text: &str,
    kind: CompletionKind,
    source: CompletionSource,
    base_score: i32,
    prefix: &str,
) {
    if let Some(fs) = qualified_object_score(prefix, label) {
        push_scored(items, label, insert_text, kind, source, base_score + fs);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn push_match_joined(
    items: &mut Vec<CompletionItem>,
    head: &str,
    tail: &str,
    insert_text: &str,
    kind: CompletionKind,
    source: CompletionSource,
    base_score: i32,
    prefix: &str,
    buf: &mut String,
) {
    buf.clear();
    let _ = write!(buf, "{head}.{tail}");
    if let Some(fs) = qualified_object_score(prefix, buf.as_str()) {
        push_scored(
            items,
            buf.as_str(),
            insert_text,
            kind,
            source,
            base_score + fs,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn push_match_joined_scored(
    items: &mut Vec<CompletionItem>,
    head: &str,
    tail: &str,
    kind: CompletionKind,
    source: CompletionSource,
    base_score: i32,
    prefix: &str,
    buf: &mut String,
) {
    buf.clear();
    let _ = write!(buf, "{head}.{tail}");
    if let Some(fs) = qualified_object_score(prefix, buf.as_str()) {
        push_scored(
            items,
            buf.as_str(),
            buf.as_str(),
            kind,
            source,
            base_score + fs,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn push_match_joined3(
    items: &mut Vec<CompletionItem>,
    a: &str,
    b: &str,
    c: &str,
    insert_text: &str,
    kind: CompletionKind,
    source: CompletionSource,
    base_score: i32,
    prefix: &str,
    buf: &mut String,
) {
    buf.clear();
    let _ = write!(buf, "{a}.{b}.{c}");
    if let Some(fs) = qualified_object_score(prefix, buf.as_str()) {
        push_scored(
            items,
            buf.as_str(),
            insert_text,
            kind,
            source,
            base_score + fs,
        );
    }
}

pub(crate) fn push_match_at(
    items: &mut Vec<CompletionItem>,
    name: &str,
    kind: CompletionKind,
    source: CompletionSource,
    base_score: i32,
    prefix: &str,
    buf: &mut String,
) {
    buf.clear();
    let _ = write!(buf, "@{name}");
    if let Some(fs) = qualified_object_score(prefix, buf.as_str()) {
        push_scored(
            items,
            buf.as_str(),
            buf.as_str(),
            kind,
            source,
            base_score + fs,
        );
    }
}

pub(crate) fn push_scored(
    items: &mut Vec<CompletionItem>,
    label: &str,
    insert_text: &str,
    kind: CompletionKind,
    source: CompletionSource,
    score: i32,
) {
    if items.len() >= COMPLETION_PRE_SORT_CAP {
        let weakest_idx = PUSH_CAP_WEAKEST.with(|cache| {
            let mut cache = cache.borrow_mut();
            if let Some(idx) = *cache
                && idx < items.len()
            {
                idx
            } else {
                let idx = weakest_index(items);
                *cache = Some(idx);
                idx
            }
        });

        if !ranks_above(score, label, &items[weakest_idx]) {
            return;
        }
        items.swap_remove(weakest_idx);
        PUSH_CAP_WEAKEST.with(|cache| *cache.borrow_mut() = None);
    }

    items.push(CompletionItem {
        label: label.to_string(),
        insert_text: insert_text.to_string(),
        kind,
        source,
        score,
        detail: String::new(),
        replace_span: None,
    });

    if items.len() >= COMPLETION_PRE_SORT_CAP {
        PUSH_CAP_WEAKEST.with(|cache| {
            *cache.borrow_mut() = Some(weakest_index(items));
        });
    }
}

fn weakest_index(items: &[CompletionItem]) -> usize {
    items
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| completion_item_cmp(a, b))
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

fn completion_item_cmp(a: &CompletionItem, b: &CompletionItem) -> std::cmp::Ordering {
    b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label))
}

fn ranks_above(score: i32, label: &str, other: &CompletionItem) -> bool {
    score > other.score || (score == other.score && label < other.label.as_str())
}

pub(crate) fn qualified_object_score_qualified(
    query: &str,
    schema: Option<&str>,
    name: &str,
    buf: &mut String,
) -> Option<i32> {
    match schema {
        Some(schema) => {
            buf.clear();
            let _ = write!(buf, "{schema}.{name}");
            qualified_object_score(query, buf)
        }
        None => qualified_object_score(query, name),
    }
}

pub(crate) fn write_qualified_column_label<'a>(
    schema: Option<&str>,
    object_name: &str,
    column: &str,
    buf: &'a mut String,
) -> &'a str {
    buf.clear();
    match schema {
        Some(schema) => {
            let _ = write!(buf, "{schema}.{object_name}.{column}");
        }
        None => {
            let _ = write!(buf, "{object_name}.{column}");
        }
    }
    buf.as_str()
}

pub(crate) fn qualified_column_score(
    query: &str,
    schema: Option<&str>,
    object_name: &str,
    column: &str,
    buf: &mut String,
) -> Option<i32> {
    let qualified = write_qualified_column_label(schema, object_name, column, buf);
    qualified_object_score(query, qualified)
}

pub(crate) fn finalize_completion_items(items: &mut Vec<CompletionItem>) {
    let cmp = completion_item_cmp;
    if items.len() > COMPLETION_PRE_SORT_CAP {
        // Defensive trim if a caller bypassed push_scored.
        let pivot = items.len() - COMPLETION_PRE_SORT_CAP;
        items.select_nth_unstable_by(pivot, |a, b| cmp(b, a));
        items.drain(..pivot);
    }
    items.sort_by(cmp);
    items.dedup_by(|a, b| a.label == b.label);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intellisense::complete::{complete, current_prefix};
    use crate::intellisense::score::{push_scored, qualified_object_score_qualified};
    use crate::intellisense::types::{
        CompletionCatalog, CompletionItem, CompletionKind, CompletionObject, CompletionSource,
    };
    use sqlly_core::ObjectKind;

    fn with_options<R>(hump_matching: bool, schema_in_search: bool, f: impl FnOnce() -> R) -> R {
        crate::matching::with_match_options(
            crate::matching::MatchOptions {
                hump_matching,
                schema_in_search,
            },
            f,
        )
    }

    #[test]
    fn humps_off_fuzzy_score_is_case_insensitive_prefix_or_substring() {
        with_options(false, true, || {
            assert_eq!(fuzzy::score("SaOr", "Sales.SalesOrder"), None);
            assert_eq!(
                fuzzy::score("Dep", "HR.Department"),
                Some(fuzzy::CONTAINS_SCORE)
            );
            assert_eq!(fuzzy::score("SEL", "select"), Some(fuzzy::PREFIX_SCORE));
            assert_eq!(qualified_object_score("SaSSO", "Sales.SalesOrder"), None);
        });
        with_options(true, true, || {
            assert_eq!(
                fuzzy::score("SaOr", "Sales.SalesOrder"),
                Some(fuzzy::CAMEL_SCORE)
            );
            assert_eq!(
                fuzzy::score("Dep", "HR.Department"),
                Some(fuzzy::ACRONYM_SCORE)
            );
            assert_eq!(
                qualified_object_score("SaSSO", "Sales.SalesOrder"),
                Some(fuzzy::CONTAINS_SCORE)
            );
        });
    }

    /// Explorer search ranks with humps regardless of the completion setting.
    #[test]
    fn object_match_tier_ignores_the_completion_hump_setting() {
        for humps in [true, false] {
            with_options(humps, true, || {
                assert_eq!(
                    object_match_tier("SOH", "SalesOrderHeader"),
                    Some(MatchTier::Abbreviation)
                );
            });
        }
    }

    #[test]
    fn schema_in_search_on_and_off_for_dbo_make() {
        for humps in [true, false] {
            with_options(humps, true, || {
                assert!(qualified_object_score("dboM", "dbo.Make").is_some());
                assert!(qualified_object_score("dbom", "dbo.Make").is_some());
            });
            with_options(humps, false, || {
                assert_eq!(qualified_object_score("dboM", "dbo.Make"), None);
                assert_eq!(qualified_object_score("dbom", "dbo.Make"), None);
                assert!(qualified_object_score("make", "dbo.Make").is_some());
            });
        }
    }

    // ---- Fuzzy module unit tests -----------------------------------------

    #[test]
    fn fuzzy_score_empty_query_matches_all() {
        assert_eq!(fuzzy::score("", "Anything"), Some(0));
    }

    #[test]
    fn fuzzy_score_prefix() {
        assert_eq!(fuzzy::score("sel", "SELECT"), Some(fuzzy::PREFIX_SCORE));
    }

    #[test]
    fn fuzzy_score_camel_match() {
        // `SaOr` -> ["Sa", "Or"] against ["Sales", "Sales", "Order"]
        assert_eq!(
            fuzzy::score("SaOr", "Sales.SalesOrder"),
            Some(fuzzy::CAMEL_SCORE)
        );
    }

    #[test]
    fn fuzzy_score_acronym() {
        // `Dep` -> single chunk, prefix of "Department" segment
        assert_eq!(
            fuzzy::score("Dep", "HR.Department"),
            Some(fuzzy::ACRONYM_SCORE)
        );
    }

    #[test]
    fn fuzzy_score_contains() {
        assert_eq!(
            fuzzy::score("part", "department"),
            Some(fuzzy::CONTAINS_SCORE)
        );
    }

    #[test]
    fn fuzzy_score_no_match() {
        assert_eq!(fuzzy::score("xyz", "Sales.SalesOrder"), None);
    }

    #[test]
    fn qualified_object_score_word_anchored_fallback() {
        // `SaSSO` fails fuzzy::score but passes last_run_word_anchored
        assert_eq!(
            qualified_object_score("SaSSO", "Sales.SalesOrder"),
            Some(fuzzy::CONTAINS_SCORE)
        );
    }

    #[test]
    fn tier_exact_beats_prefix_beats_substring() {
        assert_eq!(object_match_tier("order", "Order"), Some(MatchTier::Exact));
        assert_eq!(
            object_match_tier("ord", "OrderHeader"),
            Some(MatchTier::Prefix)
        );
        assert_eq!(
            object_match_tier("head", "OrderHeader"),
            Some(MatchTier::WordPrefix)
        );
        assert_eq!(
            object_match_tier("derhe", "OrderHeader"),
            Some(MatchTier::Substring)
        );
    }

    #[test]
    fn tier_abbreviation_matches_camel_and_acronym() {
        assert_eq!(
            object_match_tier("SOH", "SalesOrderHeader"),
            Some(MatchTier::Abbreviation)
        );
        assert_eq!(
            object_match_tier("SaOr", "SalesOrder"),
            Some(MatchTier::Abbreviation)
        );
    }

    #[test]
    fn tier_none_for_unrelated() {
        assert_eq!(object_match_tier("zzz", "Person"), None);
    }

    #[test]
    fn tier_empty_query_is_exact_none() {
        assert_eq!(object_match_tier("", "Order"), None);
        assert_eq!(object_match_tier("   ", "Order"), None);
    }

    #[test]
    fn ci_starts_with_matches_to_lowercase_semantics() {
        for (hay, needle) in [("SELECT", "sel"), ("SalesOrder", "Sa"), ("über", "Üb")] {
            assert_eq!(
                fuzzy::ci_starts_with(hay, needle),
                hay.to_lowercase().starts_with(&needle.to_lowercase()),
                "hay={hay:?} needle={needle:?}"
            );
        }
    }

    #[test]
    fn ci_contains_matches_to_lowercase_semantics() {
        for (hay, needle) in [
            ("department", "part"),
            ("Sales.SalesOrder", "order"),
            ("café", "fé"),
        ] {
            assert_eq!(
                fuzzy::ci_contains(hay, needle),
                hay.to_lowercase().contains(&needle.to_lowercase()),
                "hay={hay:?} needle={needle:?}"
            );
        }
    }

    #[test]
    fn segment_ranges_match_legacy_owned_segments() {
        fn legacy_segments(label: &str) -> Vec<String> {
            fuzzy::segments(label)
                .into_iter()
                .map(|(s, e)| label[s..e].to_string())
                .collect()
        }
        for label in ["XMLParser", "sales_order_header", "Sales.SalesOrder"] {
            assert_eq!(
                legacy_segments(label),
                fuzzy::segments(label)
                    .into_iter()
                    .map(|(s, e)| label[s..e].to_string())
                    .collect::<Vec<_>>(),
                "label={label:?}"
            );
        }
    }

    #[test]
    fn completion_pre_sort_cap_limits_items_before_finalize() {
        let mut catalog = CompletionCatalog::default();
        for idx in 0..3_000 {
            catalog.objects.push(CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Sales".to_string()),
                name: format!("Table{idx:04}"),
                kind: ObjectKind::Table,
                columns: vec!["Id".to_string(), "Name".to_string()],
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            });
        }
        let sql = "SELECT ";
        let mut items = Vec::new();
        let prefix = current_prefix(sql, sql.len()).to_string();
        for object in &catalog.objects {
            for column in &object.columns {
                if let Some(fs) = qualified_object_score(&prefix, column) {
                    push_scored(
                        &mut items,
                        column,
                        column,
                        CompletionKind::Column,
                        CompletionSource::LiveMetadata,
                        70 + fs,
                    );
                }
            }
        }
        assert!(
            items.len() <= COMPLETION_PRE_SORT_CAP,
            "push_scored should cap before finalize, got {}",
            items.len()
        );
    }

    #[test]
    fn completion_pre_sort_cap_preserves_top_ranked_items() {
        let mut catalog = CompletionCatalog::default();
        for idx in 0..3_000 {
            catalog.objects.push(CompletionObject {
                database: Some("AppDb".to_string()),
                schema: Some("Sales".to_string()),
                name: format!("Table{idx:04}"),
                kind: ObjectKind::Table,
                columns: Vec::new(),
                parameters: Vec::new(),
                foreign_keys: Vec::new(),
            });
        }
        let sql = "SELECT * FROM ";
        let bounded = complete(sql, sql.len(), &catalog);
        let mut unbounded: Vec<CompletionItem> = Vec::new();
        let prefix = current_prefix(sql, sql.len()).to_string();
        let mut buf = String::new();
        for object in &catalog.objects {
            if let Some(fs) = qualified_object_score_qualified(
                &prefix,
                object.schema.as_deref(),
                &object.name,
                &mut buf,
            ) {
                let label = object
                    .schema
                    .as_ref()
                    .map(|_| buf.as_str())
                    .unwrap_or(object.name.as_str());
                push_scored(
                    &mut unbounded,
                    label,
                    label,
                    CompletionKind::Table,
                    CompletionSource::LiveMetadata,
                    80 + fs,
                );
            }
        }
        unbounded.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
        unbounded.dedup_by(|a, b| a.label == b.label);
        let take = 50.min(unbounded.len());
        assert_eq!(
            bounded
                .iter()
                .take(take)
                .map(|i| &i.label)
                .collect::<Vec<_>>(),
            unbounded
                .iter()
                .take(take)
                .map(|i| &i.label)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn last_run_word_anchored_rejects_mismatched_first_char() {
        assert!(!last_run_word_anchored("xSSO", "Sales.SalesOrder"));
    }
}
