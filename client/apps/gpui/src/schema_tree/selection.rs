//! Multi-select state for the schema tree's bulk operations: ctrl/cmd-click
//! toggles, shift-click ranges over the visible row order, and any plain click
//! collapses the set back to nothing. The keyboard mirrors it: Shift+arrows
//! range from the anchor, Space (or Cmd/Ctrl+Space) toggles the cursor row,
//! plain arrows move the cursor and re-anchor without dropping the set, and
//! Esc clears.
//!
//! Membership is restricted by the caller (only tables and views are ever
//! passed in); this type owns ordering, anchoring, and range math only.

/// An ordered set of selected node ids plus the anchor shift-click ranges from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MultiSelection {
    /// Member node ids in the order they were added. A Vec, not a HashSet:
    /// selections are small (hand-picked), and insertion order keeps the
    /// banner count and the emitted object list deterministic.
    members: Vec<String>,
    /// The row the last plain or toggle click landed on. Shift-click ranges
    /// from here; it need not be a member (a plain click sets it too).
    anchor: Option<String>,
}

impl MultiSelection {
    pub(crate) fn len(&self) -> usize {
        self.members.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.members.iter().any(|m| m == id)
    }

    pub(crate) fn members(&self) -> &[String] {
        &self.members
    }

    pub(crate) fn clear(&mut self) {
        self.members.clear();
    }

    /// Keep only the members `keep` accepts (order preserved). Returns
    /// whether any member was dropped. A dropped anchor is forgotten too, so
    /// a later shift-click cannot range from a row that no longer exists.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&str) -> bool) -> bool {
        let before = self.members.len();
        self.members.retain(|m| keep(m));
        if self.anchor.as_deref().is_some_and(|a| !keep(a)) {
            self.anchor = None;
        }
        self.members.len() != before
    }

    /// Plain click: the set collapses and the clicked row becomes the anchor.
    /// Returns whether any members were dropped (a redraw is needed).
    pub(crate) fn plain_click(&mut self, id: &str) -> bool {
        let had = !self.members.is_empty();
        self.members.clear();
        self.anchor = Some(id.to_string());
        had
    }

    /// Ctrl/Cmd-click: toggle the row's membership; it becomes the anchor
    /// either way, so a following shift-click ranges from it.
    pub(crate) fn toggle(&mut self, id: &str) {
        match self.members.iter().position(|m| m == id) {
            Some(index) => {
                self.members.remove(index);
            }
            None => self.members.push(id.to_string()),
        }
        self.anchor = Some(id.to_string());
    }

    /// Shift-click: the set becomes every selectable row between the anchor
    /// and `id` in `visible` (the visible rows' ids, already filtered to
    /// selectable kinds, in display order). With no anchor — or an anchor the
    /// current view no longer contains — this degrades to toggling nothing and
    /// selecting just the clicked row, matching file-manager behavior.
    pub(crate) fn range_to(&mut self, id: &str, visible: &[String]) {
        let anchor_index = self
            .anchor
            .as_deref()
            .and_then(|anchor| visible.iter().position(|v| v == anchor));
        let Some(clicked_index) = visible.iter().position(|v| v == id) else {
            return;
        };
        let Some(anchor_index) = anchor_index else {
            self.members.clear();
            self.members.push(id.to_string());
            self.anchor = Some(id.to_string());
            return;
        };
        let (lo, hi) = if anchor_index <= clicked_index {
            (anchor_index, clicked_index)
        } else {
            (clicked_index, anchor_index)
        };
        self.members = visible[lo..=hi].to_vec();
        // The anchor stays put: repeated shift-clicks extend from the same row.
    }

    /// Plain keyboard movement: the cursor row becomes the anchor a later
    /// Shift+arrow ranges from. The set itself is kept, so Space can go on
    /// toggling rows the arrows walk to (the Windows list convention).
    pub(crate) fn set_anchor(&mut self, id: &str) {
        self.anchor = Some(id.to_string());
    }

    /// Shift+Up/Down (and Shift+Home/End/Page keys): the set becomes every
    /// selectable row between the anchor and `cursor`, inclusive, in `visible`
    /// (every visible row in display order, flagged selectable or not). Unlike
    /// [`Self::range_to`], the ends need not be selectable themselves, so a
    /// range can start on a Tables folder and run into its tables. With no
    /// anchor — or one the view no longer shows — `from` (the row the cursor
    /// just left) becomes the anchor first. A cursor that is not visible
    /// changes nothing.
    pub(crate) fn extend_keyboard(&mut self, from: &str, cursor: &str, visible: &[(String, bool)]) {
        let position = |id: &str| visible.iter().position(|(v, _)| v == id);
        let Some(cursor_index) = position(cursor) else {
            return;
        };
        let anchor_index = match self.anchor.as_deref().and_then(position) {
            Some(index) => index,
            None => {
                let Some(index) = position(from) else {
                    return;
                };
                self.anchor = Some(from.to_string());
                index
            }
        };
        let (lo, hi) = if anchor_index <= cursor_index {
            (anchor_index, cursor_index)
        } else {
            (cursor_index, anchor_index)
        };
        self.members = visible[lo..=hi]
            .iter()
            .filter(|(_, selectable)| *selectable)
            .map(|(id, _)| id.clone())
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn toggle_adds_and_removes_in_order() {
        let mut sel = MultiSelection::default();
        sel.toggle("a");
        sel.toggle("b");
        sel.toggle("c");
        assert_eq!(sel.members(), &ids(&["a", "b", "c"]));
        assert_eq!(sel.len(), 3);
        sel.toggle("b");
        assert_eq!(sel.members(), &ids(&["a", "c"]));
        assert!(sel.contains("a") && !sel.contains("b"));
        // Toggling the last member out empties the set.
        sel.toggle("a");
        sel.toggle("c");
        assert!(sel.is_empty());
    }

    #[test]
    fn plain_click_collapses_and_reanchors() {
        let mut sel = MultiSelection::default();
        sel.toggle("a");
        sel.toggle("b");
        assert!(sel.plain_click("z"));
        assert!(sel.is_empty());
        // The anchor moved: a shift range now extends from z, not a/b.
        sel.range_to("y", &ids(&["z", "m", "y"]));
        assert_eq!(sel.members(), &ids(&["z", "m", "y"]));
        // Plain-clicking an empty set reports nothing changed.
        assert!(sel.plain_click("q"), "the range was non-empty");
        assert!(!sel.plain_click("r"));
    }

    #[test]
    fn shift_ranges_forward_and_backward_over_visible_order() {
        let visible = ids(&["a", "b", "c", "d", "e"]);
        let mut sel = MultiSelection::default();
        sel.plain_click("b");
        sel.range_to("d", &visible);
        assert_eq!(sel.members(), &ids(&["b", "c", "d"]));
        // Backward ranges work the same way.
        sel.range_to("a", &visible);
        assert_eq!(sel.members(), &ids(&["a", "b"]));
        // The anchor is still b: extending again starts from b, not a.
        sel.range_to("e", &visible);
        assert_eq!(sel.members(), &ids(&["b", "c", "d", "e"]));
    }

    #[test]
    fn shift_without_anchor_selects_just_the_clicked_row() {
        let visible = ids(&["a", "b", "c"]);
        let mut sel = MultiSelection::default();
        sel.range_to("b", &visible);
        assert_eq!(sel.members(), &ids(&["b"]));
        // …and that row becomes the anchor for the next shift-click.
        sel.range_to("a", &visible);
        assert_eq!(sel.members(), &ids(&["a", "b"]));
    }

    #[test]
    fn shift_with_anchor_missing_from_view_selects_just_the_clicked_row() {
        let mut sel = MultiSelection::default();
        sel.plain_click("gone");
        sel.range_to("b", &ids(&["a", "b", "c"]));
        assert_eq!(sel.members(), &ids(&["b"]));
    }

    #[test]
    fn range_ignores_rows_not_in_the_visible_list() {
        let visible = ids(&["a", "b"]);
        let mut sel = MultiSelection::default();
        sel.plain_click("a");
        // Clicking a row the filter currently hides changes nothing.
        sel.range_to("hidden", &visible);
        assert!(sel.is_empty());
    }

    #[test]
    fn retain_prunes_stale_members_and_their_anchor() {
        let mut sel = MultiSelection::default();
        sel.toggle("a");
        sel.toggle("gone");
        sel.toggle("b");
        assert!(sel.retain(|id| id != "a"));
        assert_eq!(sel.members(), &ids(&["gone", "b"]));
        // The anchor ("b", last toggled) survives a prune that keeps it…
        assert!(!sel.retain(|_| true));
        sel.range_to("c", &ids(&["b", "c"]));
        assert_eq!(sel.members(), &ids(&["b", "c"]));
        // …and is forgotten when its row goes, so shift-click restarts.
        assert!(sel.retain(|id| id == "c"));
        sel.range_to("d", &ids(&["c", "d"]));
        assert_eq!(sel.members(), &ids(&["d"]));
    }

    /// Visible rows for keyboard ranging: a folder (not selectable) followed
    /// by four tables, then another folder and a view.
    fn keyboard_rows() -> Vec<(String, bool)> {
        [
            ("tables", false),
            ("a", true),
            ("b", true),
            ("c", true),
            ("d", true),
            ("views", false),
            ("v", true),
        ]
        .iter()
        .map(|(id, sel)| (id.to_string(), *sel))
        .collect()
    }

    #[test]
    fn shift_arrow_anchors_on_the_row_the_cursor_left_and_skips_folders() {
        let rows = keyboard_rows();
        let mut sel = MultiSelection::default();
        // Cursor starts on the Tables folder; Shift+Down lands on `a`.
        sel.extend_keyboard("tables", "a", &rows);
        assert_eq!(sel.members(), &ids(&["a"]));
        // Keep going: the folder anchor holds, every table in between joins.
        sel.extend_keyboard("a", "c", &rows);
        assert_eq!(sel.members(), &ids(&["a", "b", "c"]));
        // Crossing the Views folder picks up the view but not the folder.
        sel.extend_keyboard("c", "v", &rows);
        assert_eq!(sel.members(), &ids(&["a", "b", "c", "d", "v"]));
        // Shift+Up back past the anchor shrinks, then flips direction.
        sel.extend_keyboard("v", "b", &rows);
        assert_eq!(sel.members(), &ids(&["a", "b"]));
    }

    #[test]
    fn plain_arrows_reanchor_but_keep_the_set_for_space_toggling() {
        let rows = keyboard_rows();
        let mut sel = MultiSelection::default();
        sel.set_anchor("a");
        sel.extend_keyboard("a", "b", &rows);
        assert_eq!(sel.members(), &ids(&["a", "b"]));
        // Plain Down twice to `d`: the set survives, and Space adds `d`.
        sel.set_anchor("c");
        sel.set_anchor("d");
        assert_eq!(sel.members(), &ids(&["a", "b"]));
        sel.toggle("d");
        assert_eq!(sel.members(), &ids(&["a", "b", "d"]));
        // A following Shift+Up ranges from the new anchor `d`.
        sel.extend_keyboard("d", "c", &rows);
        assert_eq!(sel.members(), &ids(&["c", "d"]));
    }

    #[test]
    fn shift_arrow_with_an_invisible_cursor_or_origin_changes_nothing() {
        let rows = keyboard_rows();
        let mut sel = MultiSelection::default();
        sel.toggle("a");
        sel.extend_keyboard("a", "hidden", &rows);
        assert_eq!(sel.members(), &ids(&["a"]));
        // Anchor gone from view and the origin too: nothing to range from.
        let mut fresh = MultiSelection::default();
        fresh.extend_keyboard("gone", "b", &rows);
        assert!(fresh.is_empty());
    }

    #[test]
    fn ctrl_then_shift_uses_the_toggled_row_as_anchor() {
        let visible = ids(&["a", "b", "c", "d"]);
        let mut sel = MultiSelection::default();
        sel.toggle("c");
        sel.range_to("a", &visible);
        assert_eq!(sel.members(), &ids(&["a", "b", "c"]));
        // Toggling a member OFF still moves the anchor to it.
        sel.toggle("b");
        assert!(!sel.contains("b"));
        sel.range_to("d", &visible);
        assert_eq!(sel.members(), &ids(&["b", "c", "d"]));
    }
}
