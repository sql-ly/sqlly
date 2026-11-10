//! Scoring of unqualified object queries against fully-qualified display
//! names, layered on the [`fuzzy`](super::fuzzy) ranker: a word-anchored
//! subsequence fallback lets over-specified mixed queries (`SaSSO`) resolve
//! against `Sales.SalesOrder` without admitting loose subsequences.

use super::fuzzy;

/// Score an unqualified object query against a fully-qualified display name
/// (`Sales.SalesOrder`). First tries the standard fuzzy ranker (so `SSO` ->
/// `Sales.SalesOrder`, `SaOr` -> `Sales.SalesOrder`). When that fails, falls
/// back to a word-anchored subsequence so an over-specified mixed query like
/// `SaSSO` (more capitals than there are word-starts) still resolves, *without*
/// admitting loose subsequences such as `SSO` -> `Sales.Customer`.
pub(crate) fn qualified_object_score(query: &str, display: &str) -> Option<i32> {
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

/// Char offsets where each word-start segment of `label` begins. Mirrors the
/// segmentation in `fuzzy::segments` (camel humps, `XML|Parser` runs, and
/// non-alphanumeric separators) but keeps the positions.
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
///
/// This admits `SaSSO` -> `Sales.SalesOrder` (tail `o` lands on the `Order`
/// word-start; head `sass` is an anchored subsequence of `Sales.Sales`) while
/// rejecting `SSO` -> `Sales.Customer` / `Staging.RawCustomerImport` (their
/// trailing `o` only occurs mid-segment, never on a word-start).
fn last_run_word_anchored(query: &str, label: &str) -> bool {
    let q: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    if q.is_empty() {
        return false;
    }
    let label_low: Vec<char> = label.chars().map(|c| c.to_ascii_lowercase()).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn with_humps<R>(hump_matching: bool, f: impl FnOnce() -> R) -> R {
        crate::matching::with_match_options(
            crate::matching::MatchOptions {
                hump_matching,
                schema_in_search: true,
            },
            f,
        )
    }

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
    fn humps_off_drops_acronym_and_anchored_fallback() {
        with_humps(false, || {
            assert_eq!(qualified_object_score("SSO", "Sales.SalesOrder"), None);
            assert_eq!(qualified_object_score("SaSSO", "Sales.SalesOrder"), None);
            assert_eq!(
                qualified_object_score("SALES.SAL", "Sales.SalesOrder"),
                Some(fuzzy::PREFIX_SCORE)
            );
            assert_eq!(
                qualified_object_score("order", "Sales.SalesOrder"),
                Some(fuzzy::CONTAINS_SCORE)
            );
        });
        with_humps(true, || {
            assert_eq!(
                qualified_object_score("SSO", "Sales.SalesOrder"),
                Some(fuzzy::CAMEL_SCORE)
            );
        });
    }

    /// `dboM` / `dbom` find `dbo.Make` only while schema search is on, with
    /// hump matching either way.
    #[test]
    fn schema_in_search_on_and_off() {
        for humps in [true, false] {
            with_options(humps, true, || {
                assert!(
                    qualified_object_score("dboM", "dbo.Make").is_some(),
                    "{humps}"
                );
                assert!(
                    qualified_object_score("dbom", "dbo.Make").is_some(),
                    "{humps}"
                );
            });
            with_options(humps, false, || {
                assert_eq!(qualified_object_score("dboM", "dbo.Make"), None, "{humps}");
                assert_eq!(qualified_object_score("dbom", "dbo.Make"), None, "{humps}");
                assert!(
                    qualified_object_score("Mak", "dbo.Make").is_some(),
                    "{humps}"
                );
                assert!(
                    qualified_object_score("dbo.M", "dbo.Make").is_some(),
                    "{humps}"
                );
            });
        }
    }

    #[test]
    fn fuzzy_tiers_pass_through() {
        // Schema-spanning acronym resolves via the fuzzy ranker itself.
        assert_eq!(
            qualified_object_score("SSO", "Sales.SalesOrder"),
            Some(fuzzy::CAMEL_SCORE)
        );
        assert_eq!(
            qualified_object_score("sales.sal", "Sales.SalesOrder"),
            Some(fuzzy::PREFIX_SCORE)
        );
    }

    #[test]
    fn over_specified_mixed_query_falls_back_to_anchored_subsequence() {
        // More capitals than word-starts: fuzzy fails, the anchored fallback
        // admits it at the contains tier.
        assert_eq!(
            qualified_object_score("SaSSO", "Sales.SalesOrder"),
            Some(fuzzy::CONTAINS_SCORE)
        );
    }

    #[test]
    fn loose_subsequences_stay_rejected() {
        assert_eq!(qualified_object_score("SSO", "Sales.Customer"), None);
        assert_eq!(
            qualified_object_score("SSO", "Staging.RawCustomerImport"),
            None
        );
    }

    #[test]
    fn segment_starts_mirror_fuzzy_segmentation() {
        assert_eq!(segment_starts("Sales.SalesOrder"), vec![0, 6, 11]);
        assert_eq!(segment_starts("some_table"), vec![0, 5]);
        assert_eq!(segment_starts("XMLParser"), vec![0, 3]);
    }

    #[test]
    fn anchored_subsequence_pins_first_char() {
        let q: Vec<char> = "sass".chars().collect();
        let hay: Vec<char> = "sales.sales".chars().collect();
        assert!(anchored_subsequence(&q, &hay));
        // Unpinned head (first char mismatch) is rejected.
        let q: Vec<char> = "ass".chars().collect();
        assert!(!anchored_subsequence(&q, &hay));
    }
}
