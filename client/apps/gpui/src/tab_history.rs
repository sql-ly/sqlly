//! Browser-style back/forward navigation across editor tabs, keyed by stable
//! tab id. Bounded, session-only, and tolerant of tabs closing underneath it.

/// Back/forward stack of tab activations. Cursor points at the current entry.
/// `back`/`forward` move the cursor and must not call `push` — the caller
/// activates with a flag so `select_tab` does not re-push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabHistory {
    entries: Vec<String>,
    cursor: usize,
    cap: usize,
}

impl TabHistory {
    pub const DEFAULT_CAP: usize = 50;

    pub fn new(cap: usize) -> Self {
        Self {
            entries: Vec::new(),
            cursor: 0,
            cap: cap.max(1),
        }
    }

    /// Record an activation caused by the user (click, switcher commit, new
    /// tab). Truncates any forward entries, dedupes an immediate repeat, and
    /// trims the oldest entry past `cap`.
    pub fn push(&mut self, id: &str) {
        if !self.entries.is_empty() {
            self.entries.truncate(self.cursor + 1);
        }
        if self.entries.last().map(String::as_str) == Some(id) {
            self.cursor = self.entries.len() - 1;
            return;
        }
        self.entries.push(id.to_string());
        if self.entries.len() > self.cap {
            let drop = self.entries.len() - self.cap;
            self.entries.drain(..drop);
        }
        self.cursor = self.entries.len() - 1;
    }

    /// Move the cursor back one entry and return the id to activate; `None`
    /// at the oldest entry.
    pub fn back(&mut self) -> Option<&str> {
        if !self.can_go_back() {
            return None;
        }
        self.cursor -= 1;
        self.entries.get(self.cursor).map(String::as_str)
    }

    pub fn forward(&mut self) -> Option<&str> {
        if !self.can_go_forward() {
            return None;
        }
        self.cursor += 1;
        self.entries.get(self.cursor).map(String::as_str)
    }

    pub fn can_go_back(&self) -> bool {
        !self.entries.is_empty() && self.cursor > 0
    }

    pub fn can_go_forward(&self) -> bool {
        !self.entries.is_empty() && self.cursor + 1 < self.entries.len()
    }

    /// Remove every occurrence of a closed tab, collapsing adjacent duplicates
    /// that removal creates, and keep the cursor on the same logical entry
    /// (or clamp to the end).
    pub fn remove(&mut self, id: &str) {
        let current = self.entries.get(self.cursor).cloned();
        self.entries.retain(|entry| entry != id);
        let mut collapsed = Vec::with_capacity(self.entries.len());
        for entry in self.entries.drain(..) {
            if collapsed.last() == Some(&entry) {
                continue;
            }
            collapsed.push(entry);
        }
        self.entries = collapsed;
        if self.entries.is_empty() {
            self.cursor = 0;
            return;
        }
        if let Some(cur) = current.filter(|cur| cur != id)
            && let Some(i) = self.entries.iter().rposition(|entry| entry == &cur)
        {
            self.cursor = i;
            return;
        }
        self.cursor = self.entries.len() - 1;
    }

    #[allow(dead_code)] // Retained for future use.
    pub fn current(&self) -> Option<&str> {
        self.entries.get(self.cursor).map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_back_returns_previous() {
        let mut h = TabHistory::new(TabHistory::DEFAULT_CAP);
        h.push("a");
        h.push("b");
        assert_eq!(h.back(), Some("a"));
        assert_eq!(h.current(), Some("a"));
    }

    #[test]
    fn back_then_push_truncates_forward() {
        let mut h = TabHistory::new(TabHistory::DEFAULT_CAP);
        h.push("a");
        h.push("b");
        h.push("c");
        assert_eq!(h.back(), Some("b"));
        h.push("d");
        assert_eq!(h.current(), Some("d"));
        assert!(!h.can_go_forward());
        assert_eq!(h.back(), Some("b"));
        assert_eq!(h.back(), Some("a"));
        assert_eq!(h.forward(), Some("b"));
        assert_eq!(h.forward(), Some("d"));
        assert_eq!(h.forward(), None);
    }

    #[test]
    fn forward_after_back_returns_to_newer() {
        let mut h = TabHistory::new(TabHistory::DEFAULT_CAP);
        h.push("a");
        h.push("b");
        assert_eq!(h.back(), Some("a"));
        assert_eq!(h.forward(), Some("b"));
        assert_eq!(h.current(), Some("b"));
    }

    #[test]
    fn back_at_oldest_is_none() {
        let mut h = TabHistory::new(TabHistory::DEFAULT_CAP);
        h.push("a");
        assert_eq!(h.back(), None);
        assert_eq!(h.current(), Some("a"));
        assert!(!h.can_go_back());
    }

    #[test]
    fn push_same_id_twice_dedupes() {
        let mut h = TabHistory::new(TabHistory::DEFAULT_CAP);
        h.push("a");
        h.push("a");
        assert_eq!(h.current(), Some("a"));
        assert_eq!(h.back(), None);
    }

    #[test]
    fn cap_trims_oldest_and_keeps_cursor_valid() {
        let mut h = TabHistory::new(3);
        h.push("a");
        h.push("b");
        h.push("c");
        h.push("d");
        assert_eq!(h.current(), Some("d"));
        assert_eq!(h.back(), Some("c"));
        assert_eq!(h.back(), Some("b"));
        assert_eq!(h.back(), None);
        assert_eq!(h.current(), Some("b"));
    }

    #[test]
    fn remove_closed_tab_collapses_duplicates() {
        let mut h = TabHistory::new(TabHistory::DEFAULT_CAP);
        h.push("a");
        h.push("b");
        h.push("a");
        h.push("c");
        h.remove("b");
        assert_eq!(h.current(), Some("c"));
        assert_eq!(h.back(), Some("a"));
        assert_eq!(h.back(), None);
    }

    #[test]
    fn remove_current_clamps_cursor() {
        let mut h = TabHistory::new(TabHistory::DEFAULT_CAP);
        h.push("a");
        h.push("b");
        h.push("c");
        assert_eq!(h.back(), Some("b"));
        h.remove("b");
        assert_eq!(h.current(), Some("c"));
        assert!(!h.can_go_forward());
        assert_eq!(h.back(), Some("a"));
    }

    #[test]
    fn remove_all_leaves_empty_and_no_navigation() {
        let mut h = TabHistory::new(TabHistory::DEFAULT_CAP);
        h.push("a");
        h.push("b");
        h.remove("a");
        h.remove("b");
        assert_eq!(h.current(), None);
        assert!(!h.can_go_back());
        assert!(!h.can_go_forward());
        assert_eq!(h.back(), None);
        assert_eq!(h.forward(), None);
    }
}
