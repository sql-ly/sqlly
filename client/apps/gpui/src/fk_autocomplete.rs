//! Foreign-key autocomplete state and request shaping for the row editor.
//!
//! This module holds the *pure*, unit-testable state of one FK field's
//! searchable dropdown (query text, candidates, loading/highlight, a
//! staleness generation) and the helper that turns a [`ForeignKeySpec`] plus
//! query text into a bounded [`FkSearchRequest`]. The async round trip and the
//! GPUI rendering live in `row_editor.rs`; keeping the bookkeeping here makes
//! debounce/staleness and keyboard navigation testable without a server or a
//! window.

use sqlly_services::row_editing::{FkCandidate, FkSearchRequest, ForeignKeySpec};

/// The number of FK candidates requested per search. The service reads one
/// extra internally to flag truncation.
pub const FK_SEARCH_LIMIT: usize = 50;

/// Per-FK-field dropdown state, keyed by form-field index in the row editor.
#[derive(Debug, Clone, Default)]
pub struct FkFieldState {
    /// The filter text last submitted to a search.
    pub query: String,
    /// Candidates from the most recent completed search.
    pub candidates: Vec<FkCandidate>,
    /// Whether the dropdown is visible.
    pub open: bool,
    /// A search is in flight.
    pub loading: bool,
    /// The referenced table had more matches than were returned.
    pub truncated: bool,
    /// A search error to surface in place of candidates.
    pub error: Option<String>,
    /// Keyboard-highlighted candidate index, when the list is non-empty.
    pub highlight: Option<usize>,
    /// Monotonic guard: a search result only applies if its generation still
    /// matches, so a slow earlier search cannot clobber a newer one.
    pub generation: u64,
}

impl FkFieldState {
    /// Opens the dropdown, marks a search in flight, and returns the generation
    /// token the caller must echo back to [`Self::apply_results`].
    pub fn begin_search(&mut self, query: String) -> u64 {
        self.query = query;
        self.open = true;
        self.loading = true;
        self.error = None;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Applies candidates from a completed search. Returns `false` (ignoring
    /// the result) when a newer search has since been started.
    pub fn apply_results(
        &mut self,
        generation: u64,
        candidates: Vec<FkCandidate>,
        truncated: bool,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        self.error = None;
        self.truncated = truncated;
        self.highlight = if candidates.is_empty() { None } else { Some(0) };
        self.candidates = candidates;
        true
    }

    /// Applies a search error. Returns `false` when superseded.
    pub fn apply_error(&mut self, generation: u64, message: String) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        self.candidates.clear();
        self.highlight = None;
        self.truncated = false;
        self.error = Some(message);
        true
    }

    /// Hides the dropdown and forgets transient results (but not the generation,
    /// so any in-flight search is still discarded on return).
    pub fn close(&mut self) {
        self.open = false;
        self.loading = false;
        self.candidates.clear();
        self.highlight = None;
        self.truncated = false;
        self.error = None;
        // Bump the generation so a pending search cannot reopen the dropdown.
        self.generation = self.generation.wrapping_add(1);
    }

    /// Moves the keyboard highlight by `delta`, clamped to the candidate list.
    /// No-op when the list is empty.
    pub fn move_highlight(&mut self, delta: isize) {
        if self.candidates.is_empty() {
            self.highlight = None;
            return;
        }
        let len = self.candidates.len() as isize;
        let current = self.highlight.map(|h| h as isize).unwrap_or(-1);
        let next = (current + delta).rem_euclid(len);
        self.highlight = Some(next as usize);
    }
}

/// Shapes the bounded referenced-table search for a foreign key and query text.
pub fn fk_search_request(fk: &ForeignKeySpec, query: &str, limit: usize) -> FkSearchRequest {
    FkSearchRequest {
        referenced: fk.referenced.clone(),
        key_columns: fk.referenced_columns(),
        display_column: fk.display_column.clone(),
        query: query.to_string(),
        limit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::CellValue;
    use sqlly_services::row_editing::TableTarget;

    fn candidate(id: i64, label: &str) -> FkCandidate {
        FkCandidate {
            key: vec![CellValue::I64(id)],
            label: label.to_string(),
        }
    }

    #[test]
    fn stale_results_are_ignored() {
        let mut state = FkFieldState::default();
        let g1 = state.begin_search("a".into());
        let g2 = state.begin_search("ab".into());
        assert_ne!(g1, g2);
        // The older search completes last but must not win.
        assert!(!state.apply_results(g1, vec![candidate(1, "old")], false));
        assert!(state.candidates.is_empty());
        assert!(state.apply_results(g2, vec![candidate(2, "new")], false));
        assert_eq!(state.candidates.len(), 1);
        assert_eq!(state.candidates[0].label, "new");
        assert_eq!(state.highlight, Some(0));
    }

    #[test]
    fn close_discards_pending_search() {
        let mut state = FkFieldState::default();
        let g = state.begin_search("x".into());
        state.close();
        assert!(!state.open);
        // A result from the search in flight when we closed is dropped.
        assert!(!state.apply_results(g, vec![candidate(1, "late")], false));
    }

    #[test]
    fn highlight_wraps_both_directions() {
        let mut state = FkFieldState::default();
        state.begin_search(String::new());
        state.apply_results(
            state.generation,
            vec![candidate(1, "a"), candidate(2, "b"), candidate(3, "c")],
            false,
        );
        assert_eq!(state.highlight, Some(0));
        state.move_highlight(-1);
        assert_eq!(state.highlight, Some(2), "wrap up from first to last");
        state.move_highlight(1);
        assert_eq!(state.highlight, Some(0), "wrap down from last to first");
        assert_eq!(state.candidates[state.highlight.unwrap()].label, "a");
    }

    #[test]
    fn request_uses_referenced_columns_and_display() {
        let fk = ForeignKeySpec {
            constraint_name: "FK".into(),
            referenced: TableTarget {
                schema: "dbo".into(),
                table: "Customer".into(),
            },
            column_pairs: vec![("CustomerId".into(), "Id".into())],
            display_column: Some("Name".into()),
        };
        let request = fk_search_request(&fk, "ac", 50);
        assert_eq!(request.referenced.table, "Customer");
        assert_eq!(request.key_columns, vec!["Id".to_string()]);
        assert_eq!(request.display_column.as_deref(), Some("Name"));
        assert_eq!(request.query, "ac");
        assert_eq!(request.limit, 50);
    }
}
