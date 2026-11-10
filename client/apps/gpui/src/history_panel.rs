//! Per-tab query version history side panel: a single-column timeline of the
//! tab's sealed versions (newest first), with run versions visually distinct
//! from edit snapshots, plus an optional read-only git local-history section
//! for file-backed tabs. The panel is passive — restore/delete/close actions
//! are emitted as events and handled by `app.rs`, which owns the store.

#![cfg(not(target_arch = "wasm32"))]

use gpui::{
    Context, Entity, EventEmitter, Hsla, IntoElement, MouseButton, Render, ScrollHandle,
    Subscription, Window, div, prelude::*, px,
};

use crate::query_version_history::{GitCommitInfo, VersionEntry, VersionKind, VersionStore};
use crate::theme::Theme;

pub struct HistoryPanelView {
    store_key: String,
    /// File backing the tab, when any: enables the git section and keeps the
    /// store alive across tab closures.
    file_path: Option<String>,
    versions: Vec<VersionEntry>,
    git_commits: Vec<GitCommitInfo>,
    selected: Option<usize>,
    /// The version currently loaded in the editor (history cursor), marked
    /// in the timeline so the user can see where they are.
    current_version_id: Option<String>,
    /// Whether result storage is on (drives the header indicator and the
    /// "no results" annotation).
    results_enabled: bool,
    scroll_handle: ScrollHandle,
}

#[derive(Debug, Clone)]
pub enum HistoryPanelEvent {
    RestoreRequested {
        version_id: String,
    },
    DeleteRequested {
        version_id: String,
    },
    /// Compare the selected sealed version against the live editor text.
    /// The app layer reads the version's SQL from the tab's `VersionStore`,
    /// diffs it against the current editor contents (see [`diff_lines`]),
    /// and presents the result.
    CompareRequested {
        version_id: String,
    },
    Closed,
}

impl EventEmitter<HistoryPanelEvent> for HistoryPanelView {}

impl HistoryPanelView {
    pub fn new(
        store_key: String,
        file_path: Option<String>,
        show_git_history: bool,
        results_enabled: bool,
        _cx: &mut Context<Self>,
    ) -> Self {
        let versions = Self::load_versions(&store_key);
        let git_commits = if show_git_history {
            file_path
                .as_ref()
                .map(|p| {
                    crate::query_version_history::git_local_history(std::path::Path::new(p), 25)
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Self {
            store_key,
            file_path,
            versions,
            git_commits,
            selected: None,
            current_version_id: None,
            results_enabled,
            scroll_handle: ScrollHandle::new(),
        }
    }

    fn load_versions(store_key: &str) -> Vec<VersionEntry> {
        let dir = VersionStore::base_dir().join(store_key);
        let Ok(store) =
            VersionStore::open(dir, crate::query_version_history::HistoryCaps::default())
        else {
            return Vec::new();
        };
        store.versions().to_vec()
    }

    /// Reload the timeline after a seal / attach / delete outside the panel.
    pub fn refresh(&mut self, results_enabled: bool, cx: &mut Context<Self>) {
        self.results_enabled = results_enabled;
        self.versions = Self::load_versions(&self.store_key);
        if let Some(sel) = self.selected
            && sel >= self.versions.len()
        {
            self.selected = None;
        }
        cx.notify();
    }

    pub fn set_current_version(&mut self, version_id: Option<String>, cx: &mut Context<Self>) {
        self.current_version_id = version_id;
        cx.notify();
    }

    /// Format a seal timestamp as `HH:MM:SS` today, `Mon DD HH:MM` otherwise.
    pub fn format_time(unix_ms: i64) -> String {
        let Some(dt) = chrono::DateTime::from_timestamp_millis(unix_ms) else {
            return String::new();
        };
        let local: chrono::DateTime<chrono::Local> = dt.into();
        let now = chrono::Local::now();
        if local.date_naive() == now.date_naive() {
            local.format("%H:%M:%S").to_string()
        } else {
            local.format("%b %d %H:%M").to_string()
        }
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected = Some(index);
        cx.notify();
    }

    fn restore_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(version_id) = self
            .selected
            .and_then(|i| self.versions.get(i))
            .map(|v| v.id.clone())
        {
            cx.emit(HistoryPanelEvent::RestoreRequested { version_id });
        }
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(version_id) = self
            .selected
            .and_then(|i| self.versions.get(i))
            .map(|v| v.id.clone())
        {
            cx.emit(HistoryPanelEvent::DeleteRequested { version_id });
        }
    }

    fn compare_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(version_id) = self
            .selected
            .and_then(|i| self.versions.get(i))
            .map(|v| v.id.clone())
        {
            cx.emit(HistoryPanelEvent::CompareRequested { version_id });
        }
    }

    fn render_row(
        &self,
        index: usize,
        entry: &VersionEntry,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        let is_selected = self.selected == Some(index);
        let is_current = self.current_version_id.as_deref() == Some(entry.id.as_str());
        let is_run = entry.kind == VersionKind::Run;

        // Kind glyph: run versions get a filled accent dot, edit snapshots a
        // hollow hairline dot — the only color signal in the row.
        let dot = if is_run {
            div()
                .w(px(s(6.0)))
                .h(px(s(6.0)))
                .rounded_full()
                .flex_shrink_0()
                .bg(Theme::accent())
        } else {
            div()
                .w(px(s(6.0)))
                .h(px(s(6.0)))
                .rounded_full()
                .flex_shrink_0()
                .border_1()
                .border_color(Theme::text_dim())
        };

        let summary: Option<String> = match &entry.results {
            Some(meta) => {
                let key = if meta.rows == 1 {
                    "query.history_panel.row_count_one"
                } else {
                    "query.history_panel.row_count_many"
                };
                Some(crate::localization::format(
                    key,
                    &[("count", &meta.rows.to_string())],
                ))
            }
            // Only call out missing results when storage is on — with it off,
            // every run row would carry the annotation and it becomes noise.
            None if is_run && self.results_enabled => {
                Some(crate::localization::text("query.history_panel.no_results"))
            }
            None => None,
        };

        // Foregrounds route through `selection_fg` so the selected row carries
        // full ink only — the pairing the theme's wcag matrix asserts.
        let fg = |resting: Hsla| Theme::selection_fg(is_selected, resting);
        div()
            .id(("history-row", index))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .px(px(s(10.0)))
            .py(px(s(5.0)))
            .cursor_pointer()
            .border_b_1()
            .border_color(Theme::border())
            .when(is_selected, |d| d.bg(Theme::selection()))
            .when(!is_selected, |d| d.hover(|s| s.bg(Theme::row_hover())))
            .child(dot)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .font(Theme::ui_font())
                            .text_color(fg(Theme::text()))
                            .child(Self::format_time(entry.sealed_at_unix_ms)),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(fg(Theme::text_dim()))
                            .child(entry.kind.label()),
                    )
                    .children(summary.map(|text| {
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(fg(Theme::text_dim()))
                            .child(text)
                    })),
            )
            // The currently loaded version carries the accent "current"
            // marker — the asserted accent-indicator-on-panel pairing.
            .when(is_current, |d| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(fg(Theme::accent()))
                        .child(crate::localization::text(
                            "query.history_panel.current_badge",
                        )),
                )
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _window, cx| {
                    this.select(index, cx);
                    if ev.click_count == 2 {
                        this.restore_selected(cx);
                    }
                }),
            )
    }

    fn render_git_section(&self, _cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.git_commits.is_empty() {
            return None;
        }
        let s = Theme::scaled;
        Some(
            div()
                .flex()
                .flex_col()
                .border_t_1()
                .border_color(Theme::border())
                .child(
                    div()
                        .px(px(s(10.0)))
                        .py(px(s(6.0)))
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text("query.history_panel.git_section")),
                )
                .children(self.git_commits.iter().map(|commit| {
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap(px(s(8.0)))
                        .px(px(s(10.0)))
                        .py(px(s(3.0)))
                        .child(
                            div()
                                .font_family(Theme::editor_font_family())
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text_dim())
                                .child(commit.hash[..7.min(commit.hash.len())].to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(Theme::font_size()))
                                .text_color(Theme::text())
                                .child(commit.subject.clone()),
                        )
                }))
                .into_any_element(),
        )
    }
}

impl Render for HistoryPanelView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let has_selection = self.selected.is_some();
        let file_backed = self.file_path.is_some();

        let timeline: Vec<gpui::AnyElement> = self
            .versions
            .iter()
            .enumerate()
            .rev()
            .map(|(i, entry)| self.render_row(i, entry, cx).into_any_element())
            .collect();

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::panel_bg())
            .border_l_1()
            .border_color(Theme::border())
            // Header: title + version count (+ results indicator) beside a
            // close button. Chrome matches the DDL panel header (px 12 /
            // py 8, ✕ close) so the two right-side panels read as one family.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .px(px(s(12.0)))
                    .py(px(s(8.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(s(2.0)))
                            .overflow_hidden()
                            .child(
                                div()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text())
                                    .child(crate::localization::text("query.history_panel.title")),
                            )
                            .child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.85)))
                                    .text_color(Theme::text_dim())
                                    .child(if self.versions.is_empty() {
                                        crate::localization::text("query.history_panel.no_versions")
                                    } else {
                                        let key = match (self.versions.len(), self.results_enabled)
                                        {
                                            (1, true) => "query.history_panel.version_one_results",
                                            (1, false) => "query.history_panel.version_one",
                                            (_, true) => "query.history_panel.version_many_results",
                                            (_, false) => "query.history_panel.version_many",
                                        };
                                        crate::localization::format(
                                            key,
                                            &[("count", &self.versions.len().to_string())],
                                        )
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .id("history-close")
                            .cursor_pointer()
                            .px(px(s(8.0)))
                            .py(px(s(3.0)))
                            .rounded(px(s(4.0)))
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size()))
                            .child("✕")
                            .on_click(cx.listener(|_, _event, _window, cx| {
                                cx.emit(HistoryPanelEvent::Closed);
                            })),
                    ),
            )
            // Timeline (scrollable) or the teaching empty state.
            .child(if timeline.is_empty() {
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(s(16.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "query.history_panel.empty_timeline",
                            )),
                    )
                    .into_any_element()
            } else {
                div()
                    .id("history-timeline")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children(timeline)
                    .into_any_element()
            })
            // Footer actions for the selected row. Restore is the primary
            // action and gets the house accent-pill button (same treatment
            // as the DDL header buttons); Delete stays quiet.
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .px(px(s(12.0)))
                    .py(px(s(6.0)))
                    .border_t_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .id("history-restore")
                            .px(px(s(10.0)))
                            .py(px(s(3.0)))
                            .rounded(px(s(4.0)))
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .when(has_selection, |d| {
                                crate::theme::accent_pill_style(d.cursor_pointer()).on_click(
                                    cx.listener(|this, _event, _window, cx| {
                                        this.restore_selected(cx)
                                    }),
                                )
                            })
                            .when(!has_selection, |d| {
                                d.text_color(Theme::text_dim()).opacity(0.4)
                            })
                            .child(crate::localization::text(
                                "query.history_panel.restore_button",
                            )),
                    )
                    .child(
                        div()
                            .id("history-compare")
                            .px(px(s(10.0)))
                            .py(px(s(3.0)))
                            .rounded(px(s(4.0)))
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .when(has_selection, |d| {
                                d.cursor_pointer()
                                    .text_color(Theme::text_dim())
                                    .hover(|s| s.bg(Theme::row_hover()))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.compare_selected(cx)
                                    }))
                            })
                            .when(!has_selection, |d| {
                                d.text_color(Theme::text_dim()).opacity(0.4)
                            })
                            .child(crate::localization::text(
                                "query.history_panel.compare_button",
                            )),
                    )
                    .child(
                        div()
                            .id("history-delete")
                            .px(px(s(10.0)))
                            .py(px(s(3.0)))
                            .rounded(px(s(4.0)))
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .when(has_selection, |d| {
                                d.cursor_pointer()
                                    .text_color(Theme::text_dim())
                                    .hover(|s| s.bg(Theme::row_hover()))
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.delete_selected(cx)
                                    }))
                            })
                            .when(!has_selection, |d| {
                                d.text_color(Theme::text_dim()).opacity(0.4)
                            })
                            .child(crate::localization::text(
                                "query.history_panel.delete_button",
                            )),
                    )
                    .child(div().flex_1())
                    .when(file_backed, |d| {
                        d.child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "query.history_panel.file_backed_badge",
                                )),
                        )
                    }),
            )
            .children(self.render_git_section(cx))
    }
}

// -- line diff ---------------------------------------------------------------
//
// Pure helper for the Compare-with-Current flow: the app layer reads a sealed
// version's SQL and diffs it against the live editor text, then presents the
// result. Kept UI-free so it is unit-testable and reusable.

/// Input bound for [`diff_lines`]: each side is truncated to this many lines
/// before diffing (documents here are SQL scripts, typically tiny).
#[allow(dead_code)] // consumed by the app layer's Compare-with-Current wiring
pub const DIFF_LINE_CAP: usize = 20_000;

/// When the unmatched middle (after common prefix/suffix trimming) is larger
/// than this many DP cells, the exact LCS is skipped and the middle is
/// reported as a whole-block replace. Keeps worst-case memory/time bounded.
#[allow(dead_code)] // consumed by the app layer's Compare-with-Current wiring
const DIFF_DP_CELL_BUDGET: usize = 4_000_000;

#[allow(dead_code)] // consumed by the app layer's Compare-with-Current wiring
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    Same,
    Added,
    Removed,
}

/// One line of a computed diff, in presentation order (removed lines precede
/// the added lines that replace them).
#[allow(dead_code)] // consumed by the app layer's Compare-with-Current wiring
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// A computed line diff plus whether either input was truncated at the cap.
#[allow(dead_code)] // consumed by the app layer's Compare-with-Current wiring
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineDiff {
    pub lines: Vec<DiffLine>,
    pub truncated: bool,
}

/// Line diff of `old` → `new` (LCS-based), capped at [`DIFF_LINE_CAP`] lines
/// per side. Use [`diff_lines_capped`] when the caller needs the truncation
/// flag.
#[allow(dead_code)] // consumed by the app layer's Compare-with-Current wiring
pub fn diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    diff_lines_capped(old, new, DIFF_LINE_CAP).lines
}

/// [`diff_lines`] with an explicit per-side line cap; `truncated` reports
/// whether either input exceeded it (the diff then covers only the first
/// `cap` lines of each side).
#[allow(dead_code)] // consumed by the app layer's Compare-with-Current wiring
pub fn diff_lines_capped(old: &str, new: &str, cap: usize) -> LineDiff {
    let mut old_lines: Vec<&str> = old.lines().collect();
    let mut new_lines: Vec<&str> = new.lines().collect();
    let truncated = old_lines.len() > cap || new_lines.len() > cap;
    old_lines.truncate(cap);
    new_lines.truncate(cap);

    // Trim the common prefix and suffix first: typical edits touch a small
    // region, so the quadratic LCS only ever sees the changed middle.
    let mut prefix = 0;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let same = |text: &&str| DiffLine {
        kind: DiffKind::Same,
        text: (*text).to_string(),
    };
    let mut lines: Vec<DiffLine> = Vec::new();
    lines.extend(old_lines[..prefix].iter().map(same));
    lines.extend(diff_middle(
        &old_lines[prefix..old_lines.len() - suffix],
        &new_lines[prefix..new_lines.len() - suffix],
    ));
    lines.extend(old_lines[old_lines.len() - suffix..].iter().map(same));
    LineDiff { lines, truncated }
}

/// Diff the fully-unmatched middle: exact LCS when it fits the DP budget,
/// whole-block replace otherwise. Removed lines are emitted before the added
/// lines that replace them.
#[allow(dead_code)] // consumed by the app layer's Compare-with-Current wiring
fn diff_middle(old: &[&str], new: &[&str]) -> Vec<DiffLine> {
    let line = |kind: DiffKind| {
        move |text: &&str| DiffLine {
            kind,
            text: (*text).to_string(),
        }
    };
    if old.is_empty() || new.is_empty() || old.len().saturating_mul(new.len()) > DIFF_DP_CELL_BUDGET
    {
        let mut lines: Vec<DiffLine> = old.iter().map(line(DiffKind::Removed)).collect();
        lines.extend(new.iter().map(line(DiffKind::Added)));
        return lines;
    }

    // LCS lengths, table[i][j] = LCS of old[i..] and new[j..].
    let (n, m) = (old.len(), new.len());
    let width = m + 1;
    let mut table = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i * width + j] = if old[i] == new[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }

    let mut lines = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old[i] == new[j] {
            lines.push(DiffLine {
                kind: DiffKind::Same,
                text: old[i].to_string(),
            });
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            lines.push(DiffLine {
                kind: DiffKind::Removed,
                text: old[i].to_string(),
            });
            i += 1;
        } else {
            lines.push(DiffLine {
                kind: DiffKind::Added,
                text: new[j].to_string(),
            });
            j += 1;
        }
    }
    lines.extend(old[i..].iter().map(line(DiffKind::Removed)));
    lines.extend(new[j..].iter().map(line(DiffKind::Added)));
    lines
}

#[allow(dead_code)]
fn _assert_subscription_send(_: Subscription) {}

#[allow(dead_code)]
fn _assert_entity_usable(_: Entity<HistoryPanelView>) {}

#[cfg(test)]
mod tests {
    use super::*;

    // -- diff_lines ----------------------------------------------------------

    fn kinds(lines: &[DiffLine]) -> Vec<(DiffKind, &str)> {
        lines
            .iter()
            .map(|line| (line.kind, line.text.as_str()))
            .collect()
    }

    #[test]
    fn diff_lines_empty_inputs() {
        assert!(diff_lines("", "").is_empty());
        let capped = diff_lines_capped("", "", 10);
        assert!(capped.lines.is_empty());
        assert!(!capped.truncated);
    }

    #[test]
    fn diff_lines_identical_inputs_are_all_same() {
        let text = "SELECT a\nFROM t\nWHERE x = 1";
        let diff = diff_lines(text, text);
        assert_eq!(
            kinds(&diff),
            vec![
                (DiffKind::Same, "SELECT a"),
                (DiffKind::Same, "FROM t"),
                (DiffKind::Same, "WHERE x = 1"),
            ]
        );
    }

    #[test]
    fn diff_lines_pure_additions_and_removals() {
        assert_eq!(
            kinds(&diff_lines("", "a\nb")),
            vec![(DiffKind::Added, "a"), (DiffKind::Added, "b")]
        );
        assert_eq!(
            kinds(&diff_lines("a\nb", "")),
            vec![(DiffKind::Removed, "a"), (DiffKind::Removed, "b")]
        );
        // Addition in the middle of shared context.
        assert_eq!(
            kinds(&diff_lines("a\nc", "a\nb\nc")),
            vec![
                (DiffKind::Same, "a"),
                (DiffKind::Added, "b"),
                (DiffKind::Same, "c"),
            ]
        );
        // Removal from the middle of shared context.
        assert_eq!(
            kinds(&diff_lines("a\nb\nc", "a\nc")),
            vec![
                (DiffKind::Same, "a"),
                (DiffKind::Removed, "b"),
                (DiffKind::Same, "c"),
            ]
        );
    }

    #[test]
    fn diff_lines_replacement_emits_removed_before_added() {
        assert_eq!(
            kinds(&diff_lines(
                "SELECT 1\nFROM old_table\nWHERE x = 1",
                "SELECT 1\nFROM new_table\nWHERE x = 1"
            )),
            vec![
                (DiffKind::Same, "SELECT 1"),
                (DiffKind::Removed, "FROM old_table"),
                (DiffKind::Added, "FROM new_table"),
                (DiffKind::Same, "WHERE x = 1"),
            ]
        );
    }

    #[test]
    fn diff_lines_interleaved_changes_keep_the_common_subsequence() {
        let old = "a\nb\nc\nd\ne";
        let new = "a\nx\nc\ny\ne\nz";
        assert_eq!(
            kinds(&diff_lines(old, new)),
            vec![
                (DiffKind::Same, "a"),
                (DiffKind::Removed, "b"),
                (DiffKind::Added, "x"),
                (DiffKind::Same, "c"),
                (DiffKind::Removed, "d"),
                (DiffKind::Added, "y"),
                (DiffKind::Same, "e"),
                (DiffKind::Added, "z"),
            ]
        );
    }

    #[test]
    fn diff_lines_capped_truncates_and_flags() {
        let old = "a\nb\nc\nd\ne";
        let new = "a\nb\nc\nd\ne\nf";
        let diff = diff_lines_capped(old, new, 3);
        assert!(diff.truncated);
        assert_eq!(
            kinds(&diff.lines),
            vec![
                (DiffKind::Same, "a"),
                (DiffKind::Same, "b"),
                (DiffKind::Same, "c"),
            ]
        );
        // Under the cap the flag stays off.
        assert!(!diff_lines_capped(old, new, 100).truncated);
    }

    #[test]
    fn diff_lines_falls_back_to_block_replace_over_the_dp_budget() {
        // Fully distinct sides just over the 4M-cell budget (2100 × 2100):
        // the exact LCS is skipped and the middle reads as remove-all then
        // add-all — still a valid diff, bounded in time and memory.
        let old: String = (0..2_100).fold(String::new(), |mut s, i| {
            s.push_str(&format!("old line {i}\n"));
            s
        });
        let new: String = (0..2_100).fold(String::new(), |mut s, i| {
            s.push_str(&format!("new line {i}\n"));
            s
        });
        let diff = diff_lines(&old, &new);
        assert_eq!(diff.len(), 4_200);
        assert!(
            diff[..2_100]
                .iter()
                .all(|line| line.kind == DiffKind::Removed)
        );
        assert!(
            diff[2_100..]
                .iter()
                .all(|line| line.kind == DiffKind::Added)
        );
    }

    #[test]
    fn format_time_invalid_timestamp_is_empty() {
        assert_eq!(HistoryPanelView::format_time(i64::MAX), "");
    }

    #[test]
    fn format_time_today_shows_clock_only() {
        let now_ms = chrono::Local::now().timestamp_millis();
        let text = HistoryPanelView::format_time(now_ms);
        // HH:MM:SS — two colons, no month name.
        assert_eq!(text.matches(':').count(), 2, "{text:?}");
    }

    #[test]
    fn format_time_old_date_includes_the_date() {
        // 2001-06-15 in ms — long past; must carry the month + day.
        let text = HistoryPanelView::format_time(992_649_600_000);
        assert!(text.starts_with("Jun"), "{text:?}");
        assert_eq!(text.matches(':').count(), 1, "{text:?}");
    }
}
