//! Process-wide switches for how completion candidates are matched against
//! the typed prefix. Both completion engines (`intellisense` and the model
//! engine) read these on every scoring call, so the host sets them once from
//! the user's preferences rather than threading them through each request.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

static HUMP_MATCHING: AtomicBool = AtomicBool::new(true);
static SCHEMA_IN_SEARCH: AtomicBool = AtomicBool::new(true);

thread_local! {
    static OVERRIDE: Cell<Option<MatchOptions>> = const { Cell::new(None) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchOptions {
    /// Camel-hump / acronym matching (`SoTa` and `ST` → `SomeTable`). When
    /// off, a candidate matches only when the typed text is a
    /// case-insensitive prefix or substring of it.
    pub hump_matching: bool,
    /// Match an unqualified query against the schema-qualified name too
    /// (`dboM` / `dbom` → `dbo.Make`). When off, only the object's own name
    /// is searched unless the query itself contains a `.`.
    pub schema_in_search: bool,
}

impl Default for MatchOptions {
    fn default() -> Self {
        Self {
            hump_matching: true,
            schema_in_search: true,
        }
    }
}

pub fn set_match_options(options: MatchOptions) {
    HUMP_MATCHING.store(options.hump_matching, Ordering::Relaxed);
    SCHEMA_IN_SEARCH.store(options.schema_in_search, Ordering::Relaxed);
}

pub fn match_options() -> MatchOptions {
    OVERRIDE.with(Cell::get).unwrap_or_else(|| MatchOptions {
        hump_matching: HUMP_MATCHING.load(Ordering::Relaxed),
        schema_in_search: SCHEMA_IN_SEARCH.load(Ordering::Relaxed),
    })
}

pub fn hump_matching() -> bool {
    match_options().hump_matching
}

/// Run `f` with `options` in force on this thread only, ignoring the
/// process-wide setting. Lets tests pin both settings without racing each
/// other through the global.
pub fn with_match_options<R>(options: MatchOptions, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<MatchOptions>);
    impl Drop for Restore {
        fn drop(&mut self) {
            OVERRIDE.with(|o| o.set(self.0));
        }
    }
    let _restore = Restore(OVERRIDE.with(|o| o.replace(Some(options))));
    f()
}

/// Case-insensitive prefix/substring score shared by both engines when hump
/// matching is off: `prefix` for a leading match, `contains` for any other.
pub(crate) fn plain_score(query: &str, label: &str, prefix: i32, contains: i32) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    if ci_starts_with(label, query) {
        return Some(prefix);
    }
    if ci_contains(label, query) {
        return Some(contains);
    }
    None
}

/// Score an unqualified query against a schema-qualified `display`
/// (`Sales.SalesOrder`) under `options`. `humped` is the engine's hump-aware
/// scorer for a (query, label) pair; it is only called when hump matching is
/// on. Shared so both engines apply identical schema/hump rules.
pub(crate) fn qualified_score(
    query: &str,
    display: &str,
    options: MatchOptions,
    prefix_score: i32,
    contains_score: i32,
    humped: impl Fn(&str, &str) -> Option<i32>,
) -> Option<i32> {
    let leaf_start = display.rfind('.').map(|dot| dot + 1);
    // Searching the whole qualified name: always when the user typed a
    // qualifier themselves, otherwise only when schema search is on.
    let whole = leaf_start.is_none() || query.contains('.') || options.schema_in_search;
    if options.hump_matching {
        let target = if whole {
            display
        } else {
            &display[leaf_start.unwrap_or(0)..]
        };
        return humped(query, target);
    }
    if whole {
        if let Some(score) = plain_score(query, display, prefix_score, contains_score) {
            return Some(score);
        }
        // `dbom` → `dbo.Make`: the schema and name read as one word.
        if let Some(start) = leaf_start
            && !query.contains('.')
            && ci_starts_with_joined(&display[..start - 1], &display[start..], query)
        {
            return Some(prefix_score);
        }
        return None;
    }
    plain_score(
        query,
        &display[leaf_start.unwrap_or(0)..],
        prefix_score,
        contains_score,
    )
}

fn ci_starts_with_joined(head: &str, tail: &str, needle: &str) -> bool {
    let mut h = head
        .chars()
        .chain(tail.chars())
        .flat_map(char::to_lowercase);
    let mut n = needle.chars().flat_map(char::to_lowercase);
    loop {
        match (n.next(), h.next()) {
            (None, _) => return true,
            (Some(a), Some(b)) if a == b => continue,
            _ => return false,
        }
    }
}

fn ci_starts_with(hay: &str, needle: &str) -> bool {
    ci_starts_with_joined(hay, "", needle)
}

fn ci_contains(hay: &str, needle: &str) -> bool {
    if hay.is_ascii() && needle.is_ascii() {
        let (h, n) = (hay.as_bytes(), needle.as_bytes());
        return n.is_empty()
            || (h.len() >= n.len()
                && h.windows(n.len())
                    .any(|w| w.iter().zip(n).all(|(a, b)| a.eq_ignore_ascii_case(b))));
    }
    hay.to_lowercase().contains(&needle.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: i32 = 15;
    const CONTAINS: i32 = 0;

    fn opts(hump_matching: bool, schema_in_search: bool) -> MatchOptions {
        MatchOptions {
            hump_matching,
            schema_in_search,
        }
    }

    // Stand-in for an engine's hump scorer: records what it was asked to
    // score so the tests can see which target the options picked.
    fn humped(query: &str, label: &str) -> Option<i32> {
        (query == "dboM" && label == "dbo.Make").then_some(25)
    }

    #[test]
    fn plain_score_ignores_case() {
        assert_eq!(plain_score("MAK", "Make", PREFIX, CONTAINS), Some(PREFIX));
        assert_eq!(plain_score("aKe", "Make", PREFIX, CONTAINS), Some(CONTAINS));
        assert_eq!(plain_score("ST", "SomeTable", PREFIX, CONTAINS), None);
    }

    #[test]
    fn schema_on_without_humps_matches_schema_joined_to_name() {
        for query in ["dbom", "dboM", "DBOMA", "dbo.ma"] {
            assert_eq!(
                qualified_score(
                    query,
                    "dbo.Make",
                    opts(false, true),
                    PREFIX,
                    CONTAINS,
                    humped
                ),
                Some(PREFIX),
                "{query}"
            );
        }
    }

    #[test]
    fn schema_off_searches_only_the_name() {
        for hump in [true, false] {
            assert_eq!(
                qualified_score(
                    "dbom",
                    "dbo.Make",
                    opts(hump, false),
                    PREFIX,
                    CONTAINS,
                    humped
                ),
                None
            );
            assert_eq!(
                qualified_score(
                    "dboM",
                    "dbo.Make",
                    opts(hump, false),
                    PREFIX,
                    CONTAINS,
                    humped
                ),
                None
            );
        }
        assert_eq!(
            qualified_score(
                "mak",
                "dbo.Make",
                opts(false, false),
                PREFIX,
                CONTAINS,
                humped
            ),
            Some(PREFIX)
        );
    }

    #[test]
    fn typed_qualifier_searches_the_whole_name_even_with_schema_off() {
        assert_eq!(
            qualified_score(
                "dbo.m",
                "dbo.Make",
                opts(false, false),
                PREFIX,
                CONTAINS,
                humped
            ),
            Some(PREFIX)
        );
    }

    #[test]
    fn hump_matching_on_delegates_to_the_engine_scorer() {
        assert_eq!(
            qualified_score(
                "dboM",
                "dbo.Make",
                opts(true, true),
                PREFIX,
                CONTAINS,
                humped
            ),
            Some(25)
        );
    }

    #[test]
    fn thread_override_wins_and_restores() {
        let off = opts(false, false);
        assert_eq!(with_match_options(off, match_options), off);
        assert_eq!(OVERRIDE.with(Cell::get), None);
    }

    #[test]
    fn defaults_are_on() {
        assert_eq!(MatchOptions::default(), opts(true, true));
    }
}
