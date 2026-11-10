//! Editor split (two tab groups / same-buffer two views) — pure state and
//! index bookkeeping.
//!
//! The app-wide invariant this module preserves: `AppView::active_tab` is
//! ALWAYS the focused pane's tab, exactly as it is without a split. A
//! two-tab split only remembers which tab the *unfocused* pane shows and
//! which visual side the focused tab occupies; a same-buffer split shows the
//! focused tab in BOTH panes (through two editor views), so `active_tab` is
//! trivially both panes' tab. Either way, every existing `active_tab` call
//! site (menus, keyboard commands, async completions) keeps targeting the
//! right tab with no split-awareness at all.

/// Orientation of the split. `Vertical` puts the panes side by side
/// (a vertical divider), `Horizontal` stacks them (a horizontal divider) —
/// matching the View-menu wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitAxis {
    Vertical,
    Horizontal,
}

/// What the second pane shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitKind {
    /// Stage 1: the unfocused pane shows a *different* tab (two tab groups).
    /// Carries that tab's index.
    OtherTab(usize),
    /// Stage 2: both panes show the focused tab's document through two
    /// editor views sharing one buffer/undo stream. No index to track — the
    /// second view always follows `active_tab`.
    SameBuffer,
}

/// Minimum leading-pane size in logical pixels; the trailing pane is a flex
/// child with its own minimum, so neither side can be collapsed to nothing.
pub const MIN_PANE_SIZE: f32 = 240.0;

/// Largest leading-pane size honoured from drags or restored sessions.
pub const MAX_PANE_SIZE: f32 = 4000.0;

#[derive(Debug, Clone, PartialEq)]
pub struct SplitState {
    pub axis: SplitAxis,
    /// What the second pane shows: another tab (stage 1) or a second view of
    /// the focused tab's buffer (stage 2).
    pub kind: SplitKind,
    /// True when the focused (active) tab occupies the leading (left/top)
    /// pane; false when it occupies the trailing (right/bottom) one. For a
    /// same-buffer split this tracks which *view* is focused: the leading
    /// (canonical) one or the trailing (second-view) one.
    pub active_first: bool,
    /// Size in logical pixels of the leading pane — width for a vertical
    /// split, height for a horizontal one — adjusted by dragging the divider.
    pub first_size: f32,
}

impl SplitState {
    pub fn new(axis: SplitAxis, other_tab: usize, active_first: bool, first_size: f32) -> Self {
        Self {
            axis,
            kind: SplitKind::OtherTab(other_tab),
            active_first,
            first_size: clamp_pane_size(first_size),
        }
    }

    /// A same-buffer split: both panes show the active tab through two views.
    pub fn new_same_buffer(axis: SplitAxis, active_first: bool, first_size: f32) -> Self {
        Self {
            axis,
            kind: SplitKind::SameBuffer,
            active_first,
            first_size: clamp_pane_size(first_size),
        }
    }

    pub fn is_same_buffer(&self) -> bool {
        matches!(self.kind, SplitKind::SameBuffer)
    }

    /// The unfocused pane's tab index for a two-tab split; `None` for a
    /// same-buffer split (both panes show the active tab).
    pub fn other_tab(&self) -> Option<usize> {
        match self.kind {
            SplitKind::OtherTab(i) => Some(i),
            SplitKind::SameBuffer => None,
        }
    }

    /// Tab shown in the leading (left/top) pane.
    pub fn first_tab(&self, active_tab: usize) -> usize {
        match self.kind {
            SplitKind::SameBuffer => active_tab,
            SplitKind::OtherTab(other) => {
                if self.active_first {
                    active_tab
                } else {
                    other
                }
            }
        }
    }

    /// Tab shown in the trailing (right/bottom) pane.
    pub fn second_tab(&self, active_tab: usize) -> usize {
        match self.kind {
            SplitKind::SameBuffer => active_tab,
            SplitKind::OtherTab(other) => {
                if self.active_first {
                    other
                } else {
                    active_tab
                }
            }
        }
    }

    /// Move focus to the other pane. Two-tab split: the unfocused tab becomes
    /// active and the focused side flips. Same-buffer split: only the focused
    /// side flips — both panes already show the active tab. Returns the new
    /// active tab index.
    pub fn focus_other(&mut self, active_tab: usize) -> usize {
        match self.kind {
            SplitKind::SameBuffer => {
                self.active_first = !self.active_first;
                active_tab
            }
            SplitKind::OtherTab(other) => {
                self.kind = SplitKind::OtherTab(active_tab);
                self.active_first = !self.active_first;
                other
            }
        }
    }

    /// Selecting `target` while `active_tab` is focused: when the target is
    /// the unfocused pane's tab, focus flips to that pane instead of
    /// duplicating the tab into the focused one (no tab moves between
    /// panes). Same-buffer splits carry no per-pane tab, so selection never
    /// mutates them — the app re-binds the second view to the new active tab
    /// instead. Call before assigning the new active index.
    pub fn on_select_tab(&mut self, active_tab: usize, target: usize) {
        if let SplitKind::OtherTab(other) = self.kind
            && other == target
            && active_tab != target
        {
            self.kind = SplitKind::OtherTab(active_tab);
            self.active_first = !self.active_first;
        }
    }

    /// Re-derive the split after the tab at `removed` was removed from the
    /// tab list. `active_after` is the already-fixed-up active index and
    /// `len` the remaining tab count. Returns `None` when the split can no
    /// longer stand: for a two-tab split, its unfocused tab is gone, fewer
    /// than two tabs remain, or both panes would show the same tab; for a
    /// same-buffer split, only when no tab remains at all (the split simply
    /// follows the surviving active tab otherwise).
    pub fn after_tab_removed(
        mut self,
        removed: usize,
        active_after: usize,
        len: usize,
    ) -> Option<Self> {
        match self.kind {
            SplitKind::SameBuffer => {
                if len == 0 || active_after >= len {
                    return None;
                }
                Some(self)
            }
            SplitKind::OtherTab(mut other) => {
                if other == removed {
                    return None;
                }
                if other > removed {
                    other -= 1;
                }
                if len < 2 || other == active_after || other >= len {
                    return None;
                }
                self.kind = SplitKind::OtherTab(other);
                Some(self)
            }
        }
    }

    /// True when the split's indices are coherent for `len` tabs with
    /// `active_tab` focused. A stale split (restored from an old session,
    /// or left behind by an unforeseen mutation) must be dropped, not drawn.
    pub fn is_valid(&self, active_tab: usize, len: usize) -> bool {
        match self.kind {
            SplitKind::SameBuffer => active_tab < len,
            SplitKind::OtherTab(other) => other < len && active_tab < len && other != active_tab,
        }
    }
}

/// Clamp a leading-pane size to its usable range.
pub fn clamp_pane_size(size: f32) -> f32 {
    size.clamp(MIN_PANE_SIZE, MAX_PANE_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(other: usize, active_first: bool) -> SplitState {
        SplitState::new(SplitAxis::Vertical, other, active_first, 600.0)
    }

    fn same_buffer(active_first: bool) -> SplitState {
        SplitState::new_same_buffer(SplitAxis::Vertical, active_first, 600.0)
    }

    #[test]
    fn new_clamps_first_size() {
        let s = SplitState::new(SplitAxis::Vertical, 1, true, 10.0);
        assert_eq!(s.first_size, MIN_PANE_SIZE);
        let s = SplitState::new(SplitAxis::Vertical, 1, true, 99_999.0);
        assert_eq!(s.first_size, MAX_PANE_SIZE);
        let s = SplitState::new_same_buffer(SplitAxis::Horizontal, true, 10.0);
        assert_eq!(s.first_size, MIN_PANE_SIZE);
    }

    #[test]
    fn pane_tabs_follow_the_focused_side() {
        let s = split(2, true);
        assert_eq!(s.first_tab(0), 0);
        assert_eq!(s.second_tab(0), 2);
        let s = split(2, false);
        assert_eq!(s.first_tab(0), 2);
        assert_eq!(s.second_tab(0), 0);
    }

    #[test]
    fn focus_other_swaps_tabs_and_flips_side() {
        let mut s = split(2, true);
        let new_active = s.focus_other(0);
        assert_eq!(new_active, 2);
        assert_eq!(s.other_tab(), Some(0));
        assert!(!s.active_first);
        // The visual panes keep their tabs: only focus moved.
        assert_eq!(s.first_tab(new_active), 0);
        assert_eq!(s.second_tab(new_active), 2);
    }

    #[test]
    fn selecting_the_unfocused_panes_tab_flips_focus() {
        let mut s = split(2, true);
        s.on_select_tab(0, 2);
        // The panes keep their tabs; only the focused side changed.
        assert_eq!(s.other_tab(), Some(0));
        assert!(!s.active_first);
        assert_eq!(s.first_tab(2), 0);
        assert_eq!(s.second_tab(2), 2);
    }

    #[test]
    fn selecting_any_other_tab_replaces_the_focused_panes_tab() {
        let mut s = split(2, true);
        s.on_select_tab(0, 1);
        // Untouched: tab 1 simply loads into the focused pane.
        assert_eq!(s.other_tab(), Some(2));
        assert!(s.active_first);
    }

    #[test]
    fn reselecting_the_active_tab_is_a_no_op() {
        let mut s = split(2, false);
        s.on_select_tab(0, 0);
        assert_eq!(s.other_tab(), Some(2));
        assert!(!s.active_first);
    }

    #[test]
    fn removing_the_unfocused_tab_closes_the_split() {
        assert_eq!(split(2, true).after_tab_removed(2, 0, 2), None);
    }

    #[test]
    fn removing_a_lower_tab_shifts_the_unfocused_index() {
        let s = split(2, true)
            .after_tab_removed(1, 0, 2)
            .expect("split survives");
        assert_eq!(s.other_tab(), Some(1));
    }

    #[test]
    fn removing_a_higher_tab_keeps_the_unfocused_index() {
        let s = split(1, true)
            .after_tab_removed(2, 0, 2)
            .expect("split survives");
        assert_eq!(s.other_tab(), Some(1));
    }

    #[test]
    fn collapsing_to_one_tab_closes_the_split() {
        assert_eq!(split(1, true).after_tab_removed(0, 0, 1), None);
    }

    #[test]
    fn colliding_panes_close_the_split() {
        // Removing the active tab fixes `active_tab` onto the unfocused
        // tab's index — both panes would show the same tab.
        assert_eq!(split(1, true).after_tab_removed(0, 0, 2), None);
    }

    #[test]
    fn is_valid_rejects_stale_indices() {
        assert!(split(1, true).is_valid(0, 2));
        assert!(!split(1, true).is_valid(1, 2), "same tab in both panes");
        assert!(
            !split(5, true).is_valid(0, 2),
            "unfocused index out of range"
        );
        assert!(!split(1, true).is_valid(9, 2), "active index out of range");
    }

    // ── Same-buffer split (stage 2) ────────────────────────────────────────

    #[test]
    fn same_buffer_shows_the_active_tab_in_both_panes() {
        let s = same_buffer(true);
        assert_eq!(s.first_tab(3), 3);
        assert_eq!(s.second_tab(3), 3);
        assert_eq!(s.other_tab(), None);
        assert!(s.is_same_buffer());
    }

    #[test]
    fn same_buffer_focus_other_flips_the_side_and_keeps_the_tab() {
        let mut s = same_buffer(true);
        let new_active = s.focus_other(3);
        assert_eq!(new_active, 3, "active tab never changes");
        assert!(!s.active_first);
        let new_active = s.focus_other(3);
        assert_eq!(new_active, 3);
        assert!(s.active_first);
    }

    #[test]
    fn same_buffer_select_tab_is_state_neutral() {
        // Selecting any tab leaves the split untouched — the app re-binds the
        // second view to the newly active tab instead of mutating indices.
        let mut s = same_buffer(false);
        s.on_select_tab(0, 2);
        assert_eq!(s, same_buffer(false));
        s.on_select_tab(2, 2);
        assert_eq!(s, same_buffer(false));
    }

    #[test]
    fn same_buffer_survives_tab_removal_while_any_tab_remains() {
        // Removing a tab below, above, or even the active one keeps the
        // split standing on the fixed-up active tab.
        let s = same_buffer(true)
            .after_tab_removed(0, 0, 2)
            .expect("split survives removal of a lower tab");
        assert!(s.is_same_buffer());
        let s = same_buffer(false)
            .after_tab_removed(2, 1, 2)
            .expect("split survives removal of a higher tab");
        assert!(!s.active_first, "focused side is untouched by removal");
    }

    #[test]
    fn same_buffer_closes_when_no_tab_remains() {
        assert_eq!(same_buffer(true).after_tab_removed(0, 0, 0), None);
    }

    #[test]
    fn same_buffer_is_valid_only_bounds_checks_the_active_tab() {
        assert!(same_buffer(true).is_valid(0, 1));
        assert!(same_buffer(true).is_valid(2, 3));
        assert!(!same_buffer(true).is_valid(1, 1), "active out of range");
        assert!(!same_buffer(true).is_valid(0, 0), "no tabs at all");
    }
}
