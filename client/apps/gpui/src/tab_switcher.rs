//! Pure state for an in-progress editor tab-switching gesture.
//!
//! Ported from the macOS app's `EditorTabSwitcherSession`
//! (the native macOS app's `EditorTabSwitcher.swift`), with the
//! session tests ported from `EditorTabSwitcherTests.swift`. The highlighted
//! index wraps at both ends. A session only exists when there is more than one
//! tab, so one-tab and zero-tab workspaces are no-ops. This is a pure value
//! type so the wrap/advance logic is unit-testable without building any views
//! (AGENTS.md test mandate).

/// Direction of travel through the open editor tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchDirection {
    Forward,
    Backward,
}

impl SwitchDirection {
    /// Signed step delta: forward advances by one, backward retreats by one.
    fn delta(self) -> isize {
        match self {
            SwitchDirection::Forward => 1,
            SwitchDirection::Backward => -1,
        }
    }
}

/// Pure state for an in-progress tab-switching gesture.
///
/// Indices are positions within the **order list** the caller passes in
/// (`new(tab_count, current_index, direction)`), not strip indices. The
/// highlighted index wraps at both ends. A session only exists when there is
/// more than one tab, so one-tab and zero-tab workspaces are no-ops. The live
/// selection is never changed while a session is active — only
/// `original_index` is remembered, so a cancel can restore the original focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabSwitcherSession {
    /// The tab that was selected when the gesture started. Preserved across
    /// advances so a cancel can restore the original focus without ever having
    /// touched the live selection. Position in the caller-supplied order list.
    pub original_index: usize,
    /// Total number of tabs in the order list, captured at session start.
    pub tab_count: usize,
    /// The tab currently highlighted by the overlay. Wraps at both ends.
    /// Position in the caller-supplied order list.
    pub highlighted_index: usize,
}

/// Activation-order (most-recently-used) stack of tab ids. Front = most
/// recently activated. Session-only; never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TabMru {
    ids: Vec<String>,
}

impl TabMru {
    /// Move `id` to the front (or insert it there).
    pub fn touch(&mut self, id: &str) {
        self.ids.retain(|x| x != id);
        self.ids.insert(0, id.to_string());
    }

    /// Drop a closed tab. Returns true when it was present.
    pub fn remove(&mut self, id: &str) -> bool {
        let n = self.ids.len();
        self.ids.retain(|x| x != id);
        n != self.ids.len()
    }

    /// Rebuild against the live tab list: keep the known order for ids that
    /// still exist, then append unknown ids in strip order (covers restore
    /// and any missed `touch`).
    pub fn reconcile<'a>(&mut self, live_ids: impl Iterator<Item = &'a str>) {
        let live: Vec<&'a str> = live_ids.collect();
        self.ids
            .retain(|id| live.iter().any(|live_id| *live_id == id));
        for id in live {
            if !self.ids.iter().any(|known| known == id) {
                self.ids.push(id.to_string());
            }
        }
    }

    /// MRU-ordered ids (front = most recent).
    pub fn order(&self) -> &[String] {
        &self.ids
    }

    /// The id to activate when `closing` is closed: the most recent id that is
    /// not `closing`, or `None` when nothing else exists.
    pub fn next_after_close(&self, closing: &str) -> Option<&str> {
        self.ids.iter().map(String::as_str).find(|x| *x != closing)
    }
}

/// Whether releasing ctrl should commit the highlighted tab and close the
/// overlay.
///
/// Ctrl+Tab is a hold-and-release gesture: let go of ctrl and you land on the
/// highlighted tab. But the overlay is also clickable, and reaching a row with
/// the mouse means letting go of ctrl first — which would close the overlay
/// before the click. So the release only commits while the gesture is still a
/// keyboard one: once the pointer has moved over the overlay, the switcher
/// waits to be clicked (or cancelled with Escape / a click outside), and the
/// next keyboard step hands it back to the keyboard.
pub fn modifier_release_commits(has_session: bool, last_input_was_keyboard: bool) -> bool {
    has_session && last_input_was_keyboard
}

/// Map an MRU id list onto live strip indices, then append any live tabs the
/// stack does not yet know (defensive against a missed `touch`).
pub fn mru_positions(order: &[String], live: &[String]) -> Vec<usize> {
    let mut out = Vec::with_capacity(live.len());
    for id in order {
        if let Some(i) = live.iter().position(|live_id| live_id == id)
            && !out.contains(&i)
        {
            out.push(i);
        }
    }
    for (i, _) in live.iter().enumerate() {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

impl TabSwitcherSession {
    /// Creates a session, returning `None` when switching is a no-op (fewer
    /// than two tabs or an out-of-range current index). The highlight starts at
    /// the tab after (forward) or before (backward) the current tab, wrapping.
    pub fn new(tab_count: usize, current_index: isize, direction: SwitchDirection) -> Option<Self> {
        if tab_count <= 1 {
            return None;
        }
        if current_index < 0 || current_index as usize >= tab_count {
            return None;
        }
        let current = current_index as usize;
        let highlighted =
            Self::step(current as isize, direction.delta(), tab_count as isize) as usize;
        Some(Self {
            original_index: current,
            tab_count,
            highlighted_index: highlighted,
        })
    }

    /// Moves the highlight one step in the given direction, wrapping.
    pub fn advance(&mut self, direction: SwitchDirection) {
        self.highlighted_index = Self::step(
            self.highlighted_index as isize,
            direction.delta(),
            self.tab_count as isize,
        ) as usize;
    }

    /// Mouse (or other) selection of a specific open tab.
    pub fn select_index(&mut self, index: usize) {
        if index < self.tab_count {
            self.highlighted_index = index;
        }
    }

    /// Modular step with positive-wrap semantics for negative deltas. Returns
    /// 0 for a non-positive count.
    pub fn step(index: isize, delta: isize, count: isize) -> isize {
        if count <= 0 {
            return 0;
        }
        ((index + delta) % count + count) % count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No-op for zero/one tab.
    #[test]
    fn session_is_nil_for_zero_tabs() {
        assert!(TabSwitcherSession::new(0, 0, SwitchDirection::Forward).is_none());
    }

    #[test]
    fn session_is_nil_for_single_tab() {
        assert!(TabSwitcherSession::new(1, 0, SwitchDirection::Forward).is_none());
        assert!(TabSwitcherSession::new(1, 0, SwitchDirection::Backward).is_none());
    }

    #[test]
    fn session_is_nil_for_out_of_range_current_index() {
        assert!(TabSwitcherSession::new(3, -1, SwitchDirection::Forward).is_none());
        assert!(TabSwitcherSession::new(3, 3, SwitchDirection::Forward).is_none());
    }

    // Initial highlight starts after/before current, wrapping.
    #[test]
    fn forward_highlight_starts_at_tab_after_current_wrapping() {
        for current in 0..4_usize {
            let session = TabSwitcherSession::new(4, current as isize, SwitchDirection::Forward);
            assert_eq!(
                session.unwrap().highlighted_index,
                (current + 1) % 4,
                "forward from {current} should highlight the next tab"
            );
        }
    }

    #[test]
    fn forward_highlight_preserves_original_index() {
        for current in 0..4_usize {
            let session = TabSwitcherSession::new(4, current as isize, SwitchDirection::Forward);
            assert_eq!(session.unwrap().original_index, current);
        }
    }

    #[test]
    fn backward_highlight_starts_at_tab_before_current_wrapping() {
        for current in 0..4_usize {
            let session = TabSwitcherSession::new(4, current as isize, SwitchDirection::Backward);
            assert_eq!(
                session.unwrap().highlighted_index,
                (current + 3) % 4,
                "backward from {current} should highlight the previous tab"
            );
        }
    }

    // Advance wraps in both directions.
    #[test]
    fn advance_forward_wraps_past_end() {
        let mut session = TabSwitcherSession::new(3, 2, SwitchDirection::Forward).unwrap();
        assert_eq!(session.highlighted_index, 0); // 2 -> 0 (wrap)
        session.advance(SwitchDirection::Forward);
        assert_eq!(session.highlighted_index, 1);
        session.advance(SwitchDirection::Forward);
        assert_eq!(session.highlighted_index, 2);
        session.advance(SwitchDirection::Forward);
        assert_eq!(session.highlighted_index, 0);
    }

    #[test]
    fn advance_backward_wraps_past_start() {
        let mut session = TabSwitcherSession::new(3, 0, SwitchDirection::Backward).unwrap();
        assert_eq!(session.highlighted_index, 2); // 0 -> 2 (wrap)
        session.advance(SwitchDirection::Backward);
        assert_eq!(session.highlighted_index, 1);
        session.advance(SwitchDirection::Backward);
        assert_eq!(session.highlighted_index, 0);
        session.advance(SwitchDirection::Backward);
        assert_eq!(session.highlighted_index, 2);
    }

    #[test]
    fn original_index_is_preserved_across_advances() {
        let mut session = TabSwitcherSession::new(5, 3, SwitchDirection::Forward).unwrap();
        session.advance(SwitchDirection::Forward);
        session.advance(SwitchDirection::Backward);
        assert_eq!(
            session.original_index, 3,
            "Escape must be able to restore the original tab"
        );
    }

    #[test]
    fn step_helper_uses_positive_wrap_for_negative_delta() {
        assert_eq!(TabSwitcherSession::step(0, -1, 4), 3);
        assert_eq!(TabSwitcherSession::step(3, 1, 4), 0);
        assert_eq!(TabSwitcherSession::step(0, 0, 0), 0);
    }

    #[test]
    fn mouse_select_index_highlights_that_tab() {
        let mut session = TabSwitcherSession::new(4, 0, SwitchDirection::Forward).unwrap();
        session.select_index(2);
        assert_eq!(session.highlighted_index, 2);
        session.select_index(99);
        assert_eq!(session.highlighted_index, 2);
    }

    #[test]
    fn modifier_release_commits_only_for_the_keyboard_gesture() {
        assert!(
            modifier_release_commits(true, true),
            "ctrl+tab then release still lands on the highlighted tab"
        );
        assert!(
            !modifier_release_commits(true, false),
            "once the pointer is on the overlay, releasing ctrl must leave it open to be clicked"
        );
        assert!(
            !modifier_release_commits(false, true),
            "no session, nothing to commit"
        );
    }

    #[test]
    fn touch_moves_to_front() {
        let mut mru = TabMru::default();
        mru.touch("a");
        mru.touch("b");
        mru.touch("c");
        assert_eq!(mru.order(), &["c", "b", "a"]);
        mru.touch("a");
        assert_eq!(mru.order(), &["a", "c", "b"]);
    }

    #[test]
    fn touch_dedupes() {
        let mut mru = TabMru::default();
        mru.touch("a");
        mru.touch("a");
        mru.touch("a");
        assert_eq!(mru.order(), &["a"]);
    }

    #[test]
    fn remove_drops_and_reports() {
        let mut mru = TabMru::default();
        mru.touch("a");
        mru.touch("b");
        assert!(mru.remove("a"));
        assert_eq!(mru.order(), &["b"]);
        assert!(!mru.remove("a"));
        assert_eq!(mru.order(), &["b"]);
    }

    #[test]
    fn reconcile_keeps_known_order_and_appends_new_in_strip_order() {
        let mut mru = TabMru::default();
        mru.touch("a");
        mru.touch("c");
        mru.touch("b");
        mru.reconcile(["a", "b", "c", "d"].into_iter());
        assert_eq!(mru.order(), &["b", "c", "a", "d"]);
    }

    #[test]
    fn reconcile_drops_missing_ids() {
        let mut mru = TabMru::default();
        mru.touch("a");
        mru.touch("gone");
        mru.touch("b");
        mru.reconcile(["a", "b"].into_iter());
        assert_eq!(mru.order(), &["b", "a"]);
    }

    #[test]
    fn next_after_close_skips_closing_id() {
        let mut mru = TabMru::default();
        mru.touch("a");
        mru.touch("b");
        mru.touch("c");
        assert_eq!(mru.next_after_close("c"), Some("b"));
        assert_eq!(mru.next_after_close("b"), Some("c"));
    }

    #[test]
    fn next_after_close_none_when_only_self() {
        let mut mru = TabMru::default();
        mru.touch("only");
        assert_eq!(mru.next_after_close("only"), None);
        assert_eq!(TabMru::default().next_after_close("missing"), None);
    }

    #[test]
    fn mru_positions_maps_ids_to_strip_indices() {
        let order = vec!["c".into(), "a".into(), "b".into()];
        let live = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(mru_positions(&order, &live), vec![2, 0, 1]);
    }

    #[test]
    fn mru_positions_appends_unknown_live_tabs() {
        let order = vec!["b".into()];
        let live = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(mru_positions(&order, &live), vec![1, 0, 2]);
    }
}
