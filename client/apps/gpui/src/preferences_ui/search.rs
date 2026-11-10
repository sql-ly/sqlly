//! Content search for the Settings dialog.
//!
//! Three cooperating pieces:
//!
//! 1. **Index**: every pane's *rendered text* (section headers, setting names,
//!    hints, collection-row names) is captured by running the pane's render
//!    body with a text collector armed and the elements discarded. The sidebar
//!    filter matches against this text, so a search finds a setting by any
//!    word that would appear on the right side — not just by its tree title.
//! 2. **Highlight**: while a query is active, [`PreferencesWindow::match_text`]
//!    renders every piece of pane text, wrapping case-insensitive matches in a
//!    background highlight.
//! 3. **Reveal**: the first highlighted match in the visible pane carries a
//!    zero-size canvas marker that measures its own position against the pane
//!    scroll viewport and scrolls itself into view — no virtualization, so the
//!    measurement is exact.

use super::*;
use gpui::{Bounds, HighlightStyle, Pixels, StyledText, canvas};
use std::ops::Range;

/// Case-fold a char to a single comparison char. `to_lowercase` can expand to
/// multiple chars (ß → "ss"); folding to the first is a deliberate
/// simplification — settings text is plain UI English where 1:1 holds.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Byte ranges of case-insensitive, non-overlapping occurrences of `query` in
/// `text`. Ranges are aligned to char boundaries of the original text, so they
/// are safe to hand to `StyledText::with_highlights`.
pub(super) fn match_ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    let needle: Vec<char> = query.trim().chars().map(fold).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let hay: Vec<(usize, char)> = text.char_indices().map(|(i, c)| (i, fold(c))).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()]
            .iter()
            .map(|(_, c)| *c)
            .eq(needle.iter().copied())
        {
            let start = hay[i].0;
            let end = hay
                .get(i + needle.len())
                .map(|(b, _)| *b)
                .unwrap_or(text.len());
            out.push(start..end);
            i += needle.len();
        } else {
            i += 1;
        }
    }
    out
}

/// Every leaf pane in tree order — the set the search index covers and the
/// order "jump to first match" resolves in.
pub(super) fn collect_leaves(nodes: &[PreferencesTreeNode]) -> Vec<PreferencesPane> {
    let mut out = Vec::new();
    fn walk(node: &PreferencesTreeNode, out: &mut Vec<PreferencesPane>) {
        if let Some(p) = &node.pane {
            out.push(p.clone());
        }
        for child in &node.children {
            walk(child, out);
        }
    }
    for node in nodes {
        walk(node, &mut out);
    }
    out
}

/// Text fields whose contents are never searched: the search boxes
/// themselves, and fields that hold credentials or may carry them.
const UNSEARCHED_INPUTS: &[&str] = &[
    "prefs_search",
    "kb-search",
    "ai_profile_key",
    "decision_ai_key",
    "cli_agent_env",
];

/// Static search keywords for panes whose bodies are separate entities the
/// collector can't see into (their render text lives in another view).
fn extra_keywords(pane: &PreferencesPane) -> String {
    match pane {
        PreferencesPane::AiMcp => crate::localization::text("settings.search.keywords.mcp"),
        PreferencesPane::AiPlayground => {
            crate::localization::text("settings.search.option.playground_keywords")
        }
        _ => String::new(),
    }
}

impl PreferencesWindow {
    // ---- text collection --------------------------------------------------

    /// Append `text` to the armed collector (during an index pass). No-op in
    /// a normal render.
    pub(super) fn note_text(&self, text: &str) {
        if let Some(buf) = self.text_collector.borrow_mut().as_mut() {
            buf.push_str(text);
            buf.push('\n');
        }
    }

    /// Copy every searchable text field's current value into
    /// `input_values`, where the `&self` render helpers can read it.
    fn snapshot_input_values(&self, cx: &gpui::App) {
        let mut values = self.input_values.borrow_mut();
        values.clear();
        for (id, field) in &self.inputs {
            if !UNSEARCHED_INPUTS.contains(id) {
                values.insert(*id, field.value(cx));
            }
        }
    }

    /// Index (during an index pass) and, while searching, highlight a text
    /// field's value. Input widgets paint their own text, so a match is
    /// shown by outlining the field; the first match in the pane also
    /// scrolls it into view, like matched label text.
    pub(super) fn match_field(&self, value: &str, field: gpui::Div) -> AnyElement {
        self.note_text(value);
        if self.indexing() || match_ranges(value, &self.search_query).is_empty() {
            return field.into_any_element();
        }
        let field = field
            .border_2()
            .border_color(Theme::accent())
            .rounded(px(Theme::scaled(5.)));
        if self.pane_reveal_pending.get() && !self.pane_match_claimed.get() {
            self.pane_match_claimed.set(true);
            self.pane_reveal_pending.set(false);
            return field
                .relative()
                .child(Self::reveal_marker(self.pane_scroll.clone()))
                .into_any_element();
        }
        field.into_any_element()
    }

    /// The current value of prefs text field `id`, as last snapshotted.
    pub(super) fn input_value(&self, id: &str) -> Option<String> {
        self.input_values.borrow().get(id).cloned()
    }

    /// True while an index pass is running — render helpers can skip building
    /// highlight/marker elements that would be discarded anyway.
    pub(super) fn indexing(&self) -> bool {
        self.text_collector.borrow().is_some()
    }

    /// Rebuild the per-pane text index by rendering every pane body with the
    /// collector armed. Runs when a search begins (the index reflects the
    /// dialog's current dynamic content: overrides, snippets, providers…).
    pub(super) fn rebuild_search_index(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panes = collect_leaves(&self.tree);
        self.snapshot_input_values(cx);
        let mut index = std::collections::HashMap::new();
        for pane in panes {
            *self.text_collector.borrow_mut() = Some(String::new());
            self.note_text(&pane.display_title());
            self.note_text(&extra_keywords(&pane));
            if let Some(path) = preferences::tree_path_to(&self.tree, &pane) {
                for folder in path {
                    self.note_text(&folder);
                }
            }
            let _discarded = self.render_pane_body(&pane, window, cx);
            let text = self
                .text_collector
                .borrow_mut()
                .take()
                .unwrap_or_default()
                .to_lowercase();
            index.insert(pane, text);
        }
        self.search_index = index;
    }

    // ---- highlighted text -------------------------------------------------

    /// The single text renderer for pane content. Feeds the index collector,
    /// and while a search is active wraps matches in a background highlight;
    /// the first match in the visible pane also carries the scroll-to marker.
    /// Inherits color/size/weight from the enclosing container, so callers
    /// style the wrapper exactly as they styled a plain string child.
    pub(super) fn match_text(&self, text: &str) -> AnyElement {
        self.note_text(text);
        if self.indexing() {
            return div().into_any_element();
        }
        let ranges = match_ranges(text, &self.search_query);
        if ranges.is_empty() {
            return div().child(text.to_string()).into_any_element();
        }
        let highlight = HighlightStyle {
            background_color: Some(Theme::accent().alpha(0.30)),
            ..Default::default()
        };
        let styled = StyledText::new(text.to_string())
            .with_highlights(ranges.into_iter().map(|r| (r, highlight)));
        let el = div().child(styled);
        if self.pane_reveal_pending.get() && !self.pane_match_claimed.get() {
            self.pane_match_claimed.set(true);
            self.pane_reveal_pending.set(false);
            return el
                .relative()
                .child(Self::reveal_marker(self.pane_scroll.clone()))
                .into_any_element();
        }
        el.into_any_element()
    }

    /// A zero-size marker that, once laid out, scrolls `scroll` the minimal
    /// distance needed to bring its own position into the viewport (with a
    /// margin so the match doesn't sit flush against the edge). The offset
    /// takes effect on the next frame; `window.refresh()` requests it.
    fn reveal_marker(scroll: ScrollHandle) -> AnyElement {
        div()
            .absolute()
            .top_0()
            .left_0()
            .w_0()
            .h_0()
            .child(canvas(
                move |bounds: Bounds<Pixels>, window, _app| {
                    let viewport = scroll.bounds();
                    if viewport.size.height <= px(0.) {
                        return;
                    }
                    let margin = px(Theme::scaled(56.));
                    let y = bounds.origin.y - viewport.origin.y;
                    if y < px(0.) || y > viewport.size.height - margin {
                        let cur = scroll.offset();
                        let target = (cur.y - (y - margin)).min(px(0.));
                        scroll.set_offset(point(cur.x, target));
                        window.refresh();
                    }
                },
                |_, _, _, _| {},
            ))
            .into_any_element()
    }

    // ---- search-driven filtering (runs in render, where the window is) ----

    /// Recompute the filtered sidebar tree and, if the selected pane no longer
    /// matches, jump to the first pane that does. Called from `render` because
    /// (re)building the index needs the window.
    pub(super) fn refresh_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Build the index once per open, on the frame *after* first paint —
        // not on the first search keystroke. The build is a synchronous
        // render pass over every pane; running it mid-typing stalls input,
        // and running it before first paint delays the dialog appearing.
        if self.index_stale {
            if !self.index_warmup_frame_done {
                self.index_warmup_frame_done = true;
                cx.notify();
            } else {
                self.rebuild_search_index(window, cx);
                self.index_stale = false;
            }
        }
        if self.search_query.trim().is_empty() {
            return;
        }
        self.snapshot_input_values(cx);
        if self.index_stale {
            // Searched before the warm-up frame got to build (can only happen
            // within the first frames after open) — filter runs next frame.
            return;
        }
        if !self.search_dirty {
            return;
        }
        self.search_dirty = false;
        let filtered: Vec<PreferencesTreeNode> = {
            let q = self.search_query.trim().to_lowercase();
            let index = &self.search_index;
            let content = |p: &PreferencesPane| index.get(p).is_some_and(|text| text.contains(&q));
            self.tree
                .iter()
                .filter(|n| n.matches_with(&self.search_query, &content))
                .map(|n| n.filtered_with(&self.search_query, &content))
                .collect()
        };
        let visible = collect_leaves(&filtered);
        if !visible.contains(&self.selected_pane)
            && let Some(first) = visible.first()
        {
            self.selected_pane = first.clone();
            self.pane_scroll.set_offset(point(px(0.), px(0.)));
            self.sidebar_reveal_pending.set(true);
            // The jump happens mid-keystroke; whatever the new pane mounts
            // must not take the focus out from under the search field.
            if let Some(FieldInput::Single(input)) = self.inputs.get("prefs_search") {
                let handle = gpui::Focusable::focus_handle(input.read(cx), cx);
                window.focus(&handle, cx);
            }
        }
        self.filtered_tree = filtered;
        // Bring the first match in the (possibly newly selected) pane into
        // view on this render.
        self.pane_reveal_pending.set(true);
    }

    /// Schedule an index rebuild for the frame after next, so settings edited
    /// since the last build are findable by their new values. Called when
    /// the user leaves a pane or clears the search — never mid-typing, where
    /// the synchronous build would stall the keystroke.
    pub(super) fn mark_search_index_stale(&mut self) {
        self.index_stale = true;
        self.index_warmup_frame_done = false;
    }

    /// Make sure every folder above the selected pane is expanded, so the
    /// selection is visible when the full tree returns after a search clears.
    pub(super) fn expand_to_selected(&mut self) {
        if let Some(path) = preferences::tree_id_path_to(&self.tree, &self.selected_pane) {
            self.expanded.extend(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_ranges_finds_case_insensitive_occurrences() {
        assert_eq!(match_ranges("Thousands separator", "sep"), vec![10..13]);
        assert_eq!(match_ranges("ABC abc", "abc"), vec![0..3, 4..7]);
        assert_eq!(
            match_ranges("nothing here", "xyz"),
            Vec::<Range<usize>>::new()
        );
    }

    #[test]
    fn match_ranges_empty_query_matches_nothing() {
        assert_eq!(match_ranges("text", ""), Vec::<Range<usize>>::new());
        assert_eq!(match_ranges("text", "   "), Vec::<Range<usize>>::new());
    }

    #[test]
    fn match_ranges_is_char_boundary_safe() {
        // Multi-byte chars before and inside the match.
        let text = "héllo wörld";
        let ranges = match_ranges(text, "wörld");
        assert_eq!(ranges.len(), 1);
        let r = ranges[0].clone();
        assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
        assert_eq!(&text[r], "wörld");
    }

    #[test]
    fn match_ranges_does_not_overlap() {
        assert_eq!(match_ranges("aaaa", "aa"), vec![0..2, 2..4]);
    }

    #[test]
    fn collect_leaves_walks_the_whole_tree_in_order() {
        let tree = build_preferences_tree();
        let leaves = collect_leaves(&tree);
        assert!(leaves.contains(&PreferencesPane::Themes));
        assert!(leaves.contains(&PreferencesPane::DebugLogging));
        // Tree order: Appearance/Themes first.
        assert_eq!(leaves.first(), Some(&PreferencesPane::Themes));
    }

    #[test]
    fn matches_with_finds_content_only_matches() {
        let tree = PreferencesTreeNode::folder_with_id(
            "Results",
            "Results",
            vec![PreferencesTreeNode::leaf(
                "Integer",
                PreferencesPane::ResultsInteger,
            )],
        );
        let content = |p: &PreferencesPane| {
            *p == PreferencesPane::ResultsInteger // pretend its body says "thousands"
        };
        assert!(tree.matches_with("thousands", &content));
        let filtered = tree.filtered_with("thousands", &content);
        assert_eq!(filtered.children.len(), 1);
        // And a query matching nothing drops the leaf.
        let none = |_: &PreferencesPane| false;
        assert!(!tree.matches_with("thousands", &none));
    }

    #[test]
    fn filtered_with_keeps_whole_subtree_when_folder_title_matches() {
        let tree = PreferencesTreeNode::folder_with_id(
            "Results",
            "Results",
            vec![
                PreferencesTreeNode::leaf("Integer", PreferencesPane::ResultsInteger),
                PreferencesTreeNode::leaf("Decimal", PreferencesPane::ResultsDecimal),
            ],
        );
        let none = |_: &PreferencesPane| false;
        let filtered = tree.filtered_with("results", &none);
        assert_eq!(filtered.children.len(), 2);
    }

    #[test]
    fn sql_formatting_lives_under_editor_and_the_sql_folder_is_gone() {
        let tree = build_preferences_tree();
        assert!(tree.iter().all(|node| node.title != "SQL"));
        let path = preferences::tree_path_to(&tree, &PreferencesPane::EditorFormatting).unwrap();
        assert_eq!(path, vec!["Editor".to_string()]);
        // Saved dialog state from before the move still opens the pane.
        let pane: PreferencesPane = serde_json::from_str("\"SqlFormatting\"").unwrap();
        assert_eq!(pane, PreferencesPane::EditorFormatting);
    }

    #[test]
    fn credential_fields_are_never_searched() {
        for id in [
            "ai_profile_key",
            "decision_ai_key",
            "cli_agent_env",
            "prefs_search",
        ] {
            assert!(UNSEARCHED_INPUTS.contains(&id), "{id}");
        }
    }

    #[test]
    fn tree_path_to_returns_folder_chain() {
        let tree = build_preferences_tree();
        let path = preferences::tree_path_to(&tree, &PreferencesPane::ResultsInteger).unwrap();
        assert_eq!(path, vec!["Results".to_string()]);
        // Top-level leaf: empty path.
        let path = preferences::tree_path_to(&tree, &PreferencesPane::IntelliSenseCache).unwrap();
        assert!(path.is_empty());
    }
}
