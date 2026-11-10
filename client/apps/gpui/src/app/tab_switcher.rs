use super::*;

use sqlly_core::{ConnectionKind, DatabaseType};

/// Upper bound on query-body lines drawn in the tab-switcher preview. The pane
/// clips anyway; this keeps rendering bounded for very large buffers and adds a
/// "… N more lines" marker past the cap.
const TAB_SWITCHER_PREVIEW_MAX_LINES: usize = 200;

/// The glyph shown for a database engine in the tab switcher. No brand logos
/// are vendored, so engines are distinguished by connection shape — networked
/// servers, embedded files, and HTTP/cloud endpoints — and carry their family
/// hue via [`Theme::engine_color`].
fn engine_icon(kind: ConnectionKind) -> crate::icon_catalog::AppIcon {
    use crate::icon_catalog::AppIcon;
    match kind {
        ConnectionKind::SqliteFile | ConnectionKind::DuckDbFile => AppIcon::Database,
        ConnectionKind::HttpApi | ConnectionKind::ClickHouseHttp | ConnectionKind::HranaHttp => {
            AppIcon::Globe
        }
        ConnectionKind::Resp => AppIcon::Inbox,
        _ => AppIcon::Server,
    }
}

/// Coarse "N ago" phrasing for a monotonic instant. The switcher's
/// `last_run_completed_at` is a monotonic `Instant`, so only elapsed time is
/// available — no wall-clock date.
fn format_relative_since(t: Instant) -> String {
    let secs = t.elapsed().as_secs();
    if secs < 5 {
        return crate::localization::text("shell.tabs.switcher.relative_just_now");
    }
    let (key, value) = if secs < 60 {
        ("shell.tabs.switcher.relative_seconds_ago", secs)
    } else if secs < 3600 {
        ("shell.tabs.switcher.relative_minutes_ago", secs / 60)
    } else if secs < 86_400 {
        ("shell.tabs.switcher.relative_hours_ago", secs / 3600)
    } else {
        ("shell.tabs.switcher.relative_days_ago", secs / 86_400)
    };
    crate::localization::format(key, &[("count", value)])
}

/// Thousands-grouped integer (`1240` → `1,240`) for the preview metadata line.
fn group_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let bytes = digits.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Geometry of one Ctrl+Tab switcher row, derived from the interface font
/// size. Every line gets an explicit height and the row is their sum, so the
/// identity chips — taller than a line of text because of their padding and
/// border — always fit, at any font size or UI scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SwitcherRowMetrics {
    /// Height of the tab-name line.
    pub title_h: f32,
    /// Line height of the chip labels (their font is the status bar's).
    pub chip_text_h: f32,
    /// Height of the connection-trail line: a chip with its padding and
    /// border.
    pub trail_h: f32,
    pub gap: f32,
    pub pad_y: f32,
    pub row_h: f32,
}

/// [`SwitcherRowMetrics`] for an interface font of `font_size` px (already
/// UI-scaled, i.e. [`Theme::font_size`]).
pub(crate) fn switcher_row_metrics(font_size: f32) -> SwitcherRowMetrics {
    let line = |size: f32| (size * 1.4).ceil();
    let title_h = line(font_size);
    // Chips use `status_bar::bar_font_size()` (0.92 of the interface font).
    let chip_text_h = line(font_size * 0.92);
    // One scaled pixel of vertical padding and a 1 px border on each side.
    let trail_h = chip_text_h + 2.0 * Theme::scaled(1.0) + 2.0;
    let gap = Theme::scaled(2.0);
    let pad_y = Theme::scaled(5.0);
    SwitcherRowMetrics {
        title_h,
        chip_text_h,
        trail_h,
        gap,
        pad_y,
        row_h: pad_y * 2.0 + title_h + gap + trail_h,
    }
}

/// What one switcher row shows.
pub(crate) struct SwitcherRowContent {
    pub name: SharedString,
    pub dirty: bool,
    pub is_original: bool,
    pub engine_kind: Option<ConnectionKind>,
    pub status: crate::status_bar::StatusBarState,
    pub is_connected: bool,
}

/// One switcher row, without its mouse handlers: a fixed `row_h` box whose
/// name line and single-line connection trail are clipped to their own
/// heights, with long names and chips shortened by an ellipsis.
pub(crate) fn render_switcher_row(
    pos: usize,
    is_highlighted: bool,
    content: &SwitcherRowContent,
    metrics: SwitcherRowMetrics,
) -> gpui::Stateful<gpui::Div> {
    let engine_glyph = {
        let (icon, color) = match content.engine_kind {
            Some(kind) => (engine_icon(kind), Theme::engine_color(kind)),
            None => (crate::icon_catalog::AppIcon::Database, Theme::text_dim()),
        };
        gpui_component::Icon::empty()
            .path(icon.path())
            .size(px(Theme::font_size_ratio(0.95)))
            .text_color(color)
            .flex_shrink_0()
    };
    let title_row = div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(metrics.title_h))
        .line_height(px(metrics.title_h))
        .min_w_0()
        .overflow_hidden()
        .gap(px(Theme::scaled(6.0)))
        .child(engine_glyph)
        .child(if content.dirty {
            div()
                .flex_shrink_0()
                .text_color(if dirty_dot_is_neutral() {
                    Theme::text()
                } else {
                    Theme::accent()
                })
                .text_size(px(Theme::font_size_ratio(0.85)))
                .child("•")
        } else {
            div().flex_shrink_0().w(px(Theme::scaled(8.0)))
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                // Body text — `font_size()`, matching the ratio ladder
                // (0.77 original bullet, 0.85 trail) it sits inside.
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text())
                .child(content.name.clone()),
        )
        .children(content.is_original.then(|| {
            div()
                .flex_shrink_0()
                .text_size(px(Theme::font_size_ratio(0.77)))
                .text_color(Theme::text_dim())
                .child("•")
        }));
    // Real chips, not a run-together `client|project|env` string: the
    // identity colours and logos are what make a row recognisable without
    // reading it. One line, clipped to its own height.
    let connection_line = div()
        .h(px(metrics.trail_h))
        .min_w_0()
        .overflow_hidden()
        .flex()
        .items_center()
        .debug_selector(move || format!("tab-switcher-trail-{pos}"))
        .child(crate::status_bar::render_connection_trail(
            &content.status,
            content.is_connected,
            metrics.chip_text_h,
        ));
    div()
        .id(("tab-switcher-row", pos))
        .debug_selector(move || format!("tab-switcher-row-{pos}"))
        .flex()
        .flex_row()
        .items_center()
        .px(px(Theme::scaled(16.0)))
        .h(px(metrics.row_h))
        .flex_shrink_0()
        .overflow_hidden()
        .rounded(px(Theme::scaled(6.0)))
        .cursor_pointer()
        .bg(if is_highlighted {
            Theme::completion_selected()
        } else {
            gpui::transparent_black()
        })
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(metrics.gap))
                .debug_selector(move || format!("tab-switcher-row-content-{pos}"))
                .child(title_row)
                .child(connection_line),
        )
}

impl AppView {
    /// The database engine backing a tab: its own connection target when set,
    /// otherwise the globally active connection it follows.
    fn tab_engine(&self, tab: &QueryTab) -> Option<DatabaseType> {
        self.tab_profile(tab).map(|p| p.database_type)
    }

    /// The connection a tab runs against: its own target when set, otherwise
    /// the globally active connection it follows.
    fn tab_profile(&self, tab: &QueryTab) -> Option<sqlly_core::ConnectionProfile> {
        tab.connection_target
            .as_ref()
            .and_then(|t| crate::connection::ConnectionStore::global().get(t.profile_id))
            .or_else(|| self.active_profile.clone())
    }

    pub(super) fn handle_next_editor_tab(
        &mut self,
        _action: &NextEditorTab,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.advance_tab_switcher(SwitchDirection::Forward, cx);
    }

    pub(super) fn handle_previous_editor_tab(
        &mut self,
        _action: &PreviousEditorTab,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.advance_tab_switcher(SwitchDirection::Backward, cx);
    }

    pub(super) fn handle_cancel_tab_switcher(
        &mut self,
        _action: &CancelTabSwitcher,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_tab_switcher(cx);
    }

    pub(super) fn handle_previous_tab_in_history(
        &mut self,
        _action: &PreviousTabInHistory,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_tab_history(true, cx);
    }

    pub(super) fn handle_next_tab_in_history(
        &mut self,
        _action: &NextTabInHistory,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_tab_history(false, cx);
    }

    fn navigate_tab_history(&mut self, going_back: bool, cx: &mut Context<Self>) {
        let Some(id) = (if going_back {
            self.tab_history.back()
        } else {
            self.tab_history.forward()
        })
        .map(str::to_string) else {
            return;
        };
        self.activate_history_id(&id, going_back, true, cx);
    }

    fn activate_history_id(
        &mut self,
        id: &str,
        going_back: bool,
        retry: bool,
        cx: &mut Context<Self>,
    ) {
        self.navigating_history = true;
        if let Some(i) = self.tab_index_by_id(id) {
            self.select_tab(i, cx);
            self.navigating_history = false;
            return;
        }
        self.tab_history.remove(id);
        self.navigating_history = false;
        if !retry {
            return;
        }
        let Some(next) = (if going_back {
            self.tab_history.back()
        } else {
            self.tab_history.forward()
        })
        .map(str::to_string) else {
            return;
        };
        self.activate_history_id(&next, going_back, false, cx);
    }

    fn tab_switcher_order(&self) -> Vec<usize> {
        let live: Vec<String> = self.tabs.iter().map(|t| t.id.clone()).collect();
        crate::tab_switcher::mru_positions(self.tab_mru.order(), &live)
    }

    /// Starts a switcher session at the tab after/before the current tab, or
    /// advances the highlight when a session is already active. No-op for
    /// zero/one tab. The overlay stays open while ctrl is held and commits
    /// when ctrl is released (via `on_modifiers_changed`).
    /// Mirrors `advanceTabSwitcher` in `QueryWorkspaceController.swift:2562`.
    pub(crate) fn advance_tab_switcher(
        &mut self,
        direction: SwitchDirection,
        cx: &mut Context<Self>,
    ) {
        if let Some(session) = self.tab_switcher_session.as_mut() {
            session.advance(direction);
            self.scroll_tab_switcher_to_highlighted();
            cx.notify();
            return;
        }
        let order = self.tab_switcher_order();
        if let Some(session) = TabSwitcherSession::new(order.len(), 0, direction) {
            self.tab_switcher_order = order;
            self.tab_switcher_session = Some(session);
            self.scroll_tab_switcher_to_highlighted();
            cx.notify();
        }
    }

    /// Tab-switcher row height, derived from the interface font (see
    /// [`switcher_row_metrics`]): the name line, the connection-trail chips,
    /// and the padding around them, so the chips never spill into the next
    /// row at any font size.
    pub(super) fn tab_switcher_row_height() -> f32 {
        switcher_row_metrics(Theme::font_size()).row_h
    }

    pub(super) fn tab_switcher_panel_size(window_width: f32, window_height: f32) -> (f32, f32) {
        (window_width * 0.4, window_height * 0.6)
    }

    pub(super) fn tab_switcher_summary_text(long: Option<&str>, short: Option<&str>) -> String {
        long.or(short)
            .map(str::to_string)
            .unwrap_or_else(|| crate::query_ai_summary::QueryAISummaryKind::Long.pending_text())
    }

    pub(super) fn tab_switcher_summary(tab: &QueryTab) -> SharedString {
        Self::tab_switcher_summary_text(
            tab.ai_summary.long_summary.as_deref(),
            tab.ai_summary.short_summary.as_deref(),
        )
        .into()
    }

    /// Scrolls the tab-switcher overlay so the highlighted row is visible.
    /// Computes the target y offset from the highlighted index and row height,
    /// clamped to the valid scroll range. Mirrors `scrollHighlightedRowIntoView`
    /// in `EditorTabSwitcherView.swift`.
    pub(super) fn scroll_tab_switcher_to_highlighted(&self) {
        let Some(session) = &self.tab_switcher_session else {
            return;
        };
        let row_h = Self::tab_switcher_row_height();
        let target_y = session.highlighted_index as f32 * row_h;
        let current = self.tab_switcher_scroll.offset();
        let max_y = f32::from(self.tab_switcher_scroll.max_offset().y);
        // Only scroll if the highlighted row is outside the current viewport.
        let viewport_h = f32::from(self.tab_switcher_scroll.bounds().size.height);
        if viewport_h <= 0.0 {
            // Pre-layout: just set the offset directly.
            self.tab_switcher_scroll
                .set_offset(Point::new(px(0.0), px(target_y.max(0.0).min(max_y))));
            return;
        }
        let viewport_top = f32::from(current.y);
        let viewport_bottom = viewport_top + viewport_h;
        let row_top = session.highlighted_index as f32 * row_h;
        let row_bottom = row_top + row_h;
        let new_y = if row_top < viewport_top {
            row_top
        } else if row_bottom > viewport_bottom {
            (row_bottom - viewport_h).max(0.0)
        } else {
            return; // Already visible
        };
        self.tab_switcher_scroll
            .set_offset(Point::new(current.x, px(new_y.max(0.0).min(max_y))));
    }

    /// Commits the highlighted tab: clears the session and selects the
    /// highlighted tab (which updates the editor/results/status-bar via
    /// `select_tab`). The live selection was never touched during the session,
    /// so this is the only point at which it changes. The commit timer task is
    /// left in place here because this runs inside it; it is a completed future
    /// and is overwritten on the next advance/teardown.
    pub(super) fn commit_tab_switcher(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.tab_switcher_session.take() else {
            return;
        };
        if self.tabs.len() != self.tab_switcher_order.len() {
            self.tab_switcher_order.clear();
            cx.notify();
            return;
        }
        let Some(&index) = self.tab_switcher_order.get(session.highlighted_index) else {
            self.tab_switcher_order.clear();
            cx.notify();
            return;
        };
        self.tab_switcher_order.clear();
        if index < self.tabs.len() {
            self.select_tab(index, cx);
        } else {
            cx.notify();
        }
    }

    /// Cancels the session and leaves the originally selected tab focused. The
    /// live selection is never changed during a session, so teardown alone
    /// restores the original focus. No-op when no session is active (so the
    /// Escape binding is safe to fire globally).
    pub(crate) fn cancel_tab_switcher(&mut self, cx: &mut Context<Self>) {
        if self.overlays.run_picker.is_some() {
            self.overlays.run_picker = None;
            cx.notify();
            return;
        }
        if self.tab_switcher_session.is_none() {
            if let Some(tab) = self.active_tab() {
                tab.editor.update(cx, |editor, cx| {
                    editor.close_completions(cx);
                });
            }
            return;
        }
        self.end_tab_switcher_session(cx);
    }

    /// Ends the session: drops the commit timer, clears the session, and hides
    /// the overlay (the next render sees `tab_switcher_session == None`).
    pub(super) fn end_tab_switcher_session(&mut self, cx: &mut Context<Self>) {
        self.tab_switcher_session = None;
        self.tab_switcher_order.clear();
        cx.notify();
    }

    /// Renders the tab-switcher overlay when a session is active, else `None`.
    /// A dimming backdrop covers the window; a centered panel lists open tabs
    /// beside a preview of the highlighted editor. Clicking the
    /// backdrop cancels the session (parity with the Swift click-away behavior
    /// in `QueryWorkspaceController.swift:2709-2718`); clicks inside the panel
    /// are stopped so they do not cancel.
    ///
    /// The panel occupies 40% of the window width and 60% of its height. Its
    /// equal-width panes keep tab identity and SQL preview visible together.
    pub(super) fn render_tab_switcher_overlay(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let session = self.tab_switcher_session?;
        let switcher_this = cx.entity();

        let metrics = switcher_row_metrics(Theme::font_size());
        let win_w = f32::from(window.bounds().size.width);
        let win_h = f32::from(window.bounds().size.height);
        let (content_w, content_h) = Self::tab_switcher_panel_size(win_w, win_h);
        let pane_w = content_w / 2.0;

        let identities = self.sidebar.read(cx).entities();
        let mut rows = div().flex().flex_col();
        for (pos, &i) in self.tab_switcher_order.iter().enumerate() {
            let Some(tab) = self.tabs.get(i) else {
                continue;
            };
            let name: SharedString = if tab.name.trim().is_empty() {
                format!("Query {}", i + 1).into()
            } else {
                tab.name.clone().into()
            };
            // Each row shows ITS tab's connection: a tab with its own target
            // is not the globally active connection.
            let profile = self.tab_profile(tab);
            let content = SwitcherRowContent {
                name,
                dirty: tab.dirty,
                is_original: pos == session.original_index,
                engine_kind: profile.as_ref().map(|p| p.database_type.connection_kind()),
                status: crate::status_bar::resolve_with_identities(profile.as_ref(), identities),
                is_connected: profile.is_some(),
            };
            let row = render_switcher_row(pos, pos == session.highlighted_index, &content, metrics)
                // Hovering moves the highlight, so the preview pane follows
                // the pointer and the row you are about to click is the row
                // you can see.
                .on_mouse_move({
                    let hover = switcher_this.clone();
                    move |_, _, app| {
                        hover.update(app, |this, cx| {
                            let Some(session) = &mut this.tab_switcher_session else {
                                return;
                            };
                            if session.highlighted_index == pos {
                                return;
                            }
                            session.select_index(pos);
                            cx.notify();
                        });
                    }
                })
                // Selecting with the mouse commits immediately — the click is
                // the whole gesture, so the overlay closes on the tab you
                // pressed. `stop_propagation` keeps the press from reaching
                // anything behind the row.
                .on_mouse_down(gpui::MouseButton::Left, {
                    let pick = switcher_this.clone();
                    move |_, _, app| {
                        pick.update(app, |this, cx| {
                            if let Some(session) = &mut this.tab_switcher_session {
                                session.select_index(pos);
                            }
                            this.commit_tab_switcher(cx);
                            cx.stop_propagation();
                        });
                    }
                });
            rows = rows.child(row);
        }

        let strip_i = *self.tab_switcher_order.get(session.highlighted_index)?;
        let highlighted = self.tabs.get(strip_i)?;
        let preview_name: SharedString = if highlighted.name.trim().is_empty() {
            format!("Query {}", strip_i + 1).into()
        } else {
            highlighted.name.clone().into()
        };
        let preview_summary = Self::tab_switcher_summary(highlighted);
        // Query body as readable SQL text. The switcher previously rasterised the
        // editor's on-screen viewport into an SVG, but that geometry only exists
        // for the tab currently laid out — every *background* tab (the ones you
        // Ctrl+Tab to) produced an empty snapshot. Rendering the buffer text
        // works for every tab and needs no layout.
        let preview_body: Vec<gpui::Div> = {
            let editor = highlighted.editor.read(cx);
            let body = editor.text();
            let trimmed = body.trim_end();
            let total = trimmed.lines().count();
            let mut els: Vec<gpui::Div> = trimmed
                .lines()
                .take(TAB_SWITCHER_PREVIEW_MAX_LINES)
                .map(|line| {
                    let text: SharedString = if line.trim().is_empty() {
                        // Keep blank lines visually tall.
                        SharedString::from("\u{00a0}")
                    } else {
                        SharedString::from(line.to_string())
                    };
                    div().whitespace_nowrap().child(text)
                })
                .collect();
            if total == 0 {
                els.push(
                    div()
                        .text_color(Theme::text_dim())
                        .child(SharedString::from(crate::localization::text(
                            "shell.tabs.switcher.preview_empty_query",
                        ))),
                );
            } else if total > TAB_SWITCHER_PREVIEW_MAX_LINES {
                let remaining = total - TAB_SWITCHER_PREVIEW_MAX_LINES;
                let key = if remaining == 1 {
                    "shell.tabs.switcher.preview_more_lines_singular"
                } else {
                    "shell.tabs.switcher.preview_more_lines_plural"
                };
                els.push(
                    div()
                        .text_color(Theme::text_dim())
                        .child(SharedString::from(crate::localization::format(
                            key,
                            &[("count", remaining)],
                        ))),
                );
            }
            els
        };
        let preview_kind = self.tab_engine(highlighted).map(|e| e.connection_kind());
        // Bottom caption: results volume + how long ago this tab last ran.
        // `last_run_completed_at` is monotonic, so timing is relative only.
        let preview_meta: SharedString = if let Some(ran_at) = highlighted.last_run_completed_at {
            let results = highlighted.results.read(cx);
            let rows = results.total_rows();
            let sets = results.result_set_count();
            let row_count = group_thousands(rows);
            let relative = format_relative_since(ran_at);
            if sets > 1 {
                let key = if rows == 1 {
                    "shell.tabs.switcher.preview_ran_row_singular_result_sets"
                } else {
                    "shell.tabs.switcher.preview_ran_row_plural_result_sets"
                };
                crate::localization::format(
                    key,
                    &[
                        ("relative", relative),
                        ("count", row_count),
                        ("result_set_count", sets.to_string()),
                    ],
                )
            } else {
                let key = if rows == 1 {
                    "shell.tabs.switcher.preview_ran_row_singular"
                } else {
                    "shell.tabs.switcher.preview_ran_row_plural"
                };
                crate::localization::format(key, &[("relative", relative), ("count", row_count)])
            }
            .into()
        } else {
            crate::localization::text("shell.tabs.switcher.preview_not_run").into()
        };
        let preview_glyph = {
            let (icon, color) = match preview_kind {
                Some(kind) => (engine_icon(kind), Theme::engine_color(kind)),
                None => (crate::icon_catalog::AppIcon::Database, Theme::text_dim()),
            };
            gpui_component::Icon::empty()
                .path(icon.path())
                .size(px(Theme::font_size()))
                .text_color(color)
                .flex_shrink_0()
        };

        let tab_list = div()
            .id("tab-switcher-list")
            .w(px(pane_w))
            .h_full()
            .min_h_0()
            .p(px(Theme::scaled(8.0)))
            .overflow_y_scroll()
            .track_scroll(&self.tab_switcher_scroll)
            .child(rows);

        let preview = div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex_none()
                    .p(px(Theme::scaled(16.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::scaled(6.0)))
                            .child(preview_glyph)
                            .child(
                                // Preview title — body text over its
                                // `font_size_ratio(0.85)` subtitle; tracks the
                                // user's font like the subtitle does.
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text())
                                    .child(preview_name),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(Theme::scaled(6.0)))
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .child(preview_summary),
                    ),
            )
            .child(
                div()
                    .id("tab-switcher-preview-body")
                    .flex_1()
                    .min_h_0()
                    .p(px(Theme::scaled(12.0)))
                    .bg(Theme::bg())
                    .overflow_hidden()
                    .flex()
                    .flex_col()
                    .font_family(Theme::editor_font_family())
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text())
                    .children(preview_body),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(Theme::scaled(16.0)))
                    .py(px(Theme::scaled(10.0)))
                    .border_t_1()
                    .border_color(Theme::border())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(preview_meta),
            );

        let panel = div()
            .id("tab-switcher-panel")
            .w(px(content_w))
            .h(px(content_h))
            .flex()
            .flex_row()
            .overflow_hidden()
            // Floats over a scrim, so it takes the popup surface + popup border
            // — the system's only "above" cue now that shadows are gone.
            .bg(Theme::popup_bg())
            .border_1()
            .border_color(Theme::popup_border())
            .rounded(px(Theme::scaled(12.0)))
            .child(tab_list)
            .child(preview)
            // Click-away cancels. Anchoring that on the panel ("a press landed
            // outside me") instead of on the backdrop keeps it in the capture
            // phase and out of the way of the row clicks, which are the only
            // presses inside the panel that do anything.
            .on_mouse_down_out(cx.listener(|this, _ev, _window, cx| {
                this.cancel_tab_switcher(cx);
            }))
            // Movement over the panel belongs to the switcher, not to whatever
            // it covers. Moving here also switches gpui's input modality to the
            // mouse, which is what stops a later ctrl release from committing
            // out from under the pointer (see the modifiers handler in
            // `app/mod.rs` and `modifier_release_commits`).
            .on_mouse_move(cx.listener(|_this, _ev, _window, cx| {
                cx.stop_propagation();
            }));

        let overlay = div()
            .id("tab-switcher-backdrop")
            // Block scroll/hover from reaching surfaces beneath the dialog.
            .occlude()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(Theme::scrim())
            .flex()
            .items_center()
            .justify_center()
            .child(panel);

        Some(overlay.into_any_element())
    }
}

#[cfg(test)]
mod row_layout_tests {
    use super::*;
    use crate::status_bar::{StatusBarBadge, StatusBarConnection, StatusBarState};

    /// Rows are the name line + the chip line + padding at every interface
    /// font size the preferences allow (8–32 px, UI-scaled), and a chip —
    /// its label line, padding and border — fits the trail line.
    #[test]
    fn row_metrics_fit_the_chips_at_every_font_size() {
        for size in 8..=32 {
            let font = Theme::scaled(size as f32);
            let m = switcher_row_metrics(font);
            assert!(m.title_h >= font, "{size}: name line shorter than its font");
            assert!(m.chip_text_h >= font * 0.92, "{size}: chip label clipped");
            let chip = m.chip_text_h + 2.0 * Theme::scaled(1.0) + 2.0;
            assert!(m.trail_h >= chip, "{size}: chip taller than its line");
            assert!(
                m.row_h >= m.pad_y * 2.0 + m.title_h + m.gap + m.trail_h,
                "{size}: row shorter than its content"
            );
        }
    }

    fn long_trail() -> StatusBarState {
        let badge = |name: &str, color: Option<&str>| {
            let mut badge = StatusBarBadge::new(name);
            badge.color_hex = color.map(str::to_string);
            badge
        };
        StatusBarState {
            connection: StatusBarConnection {
                server_friendly_name: Some(
                    "reporting-replica-eastus2-very-long-server-name".into(),
                ),
                server_host: Some("10.0.0.5".into()),
                database_friendly_name: Some("AdventureWorksDW2022_Archive_Readonly".into()),
                database_host: None,
            },
            client: Some(badge("Contoso International Holdings", Some("#1E88E5"))),
            project: Some(badge("Quarterly Revenue Reconciliation", None)),
            environment: Some(badge("Production (read replica)", Some("#E53935"))),
            ..StatusBarState::default()
        }
    }

    struct SwitcherRows {
        rows: Vec<SwitcherRowContent>,
        metrics: SwitcherRowMetrics,
    }

    impl Render for SwitcherRows {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().w(px(280.0)).flex().flex_col().children(
                self.rows
                    .iter()
                    .enumerate()
                    .map(|(pos, row)| render_switcher_row(pos, pos == 1, row, self.metrics)),
            )
        }
    }

    /// The docs-screenshot bug: long names and a full identity trail in a
    /// narrow switcher pane. Each row's content — name line and chips — must
    /// stay inside its own row, and consecutive rows must not overlap.
    #[gpui::test]
    fn switcher_rows_never_overlap_with_long_names_and_chips(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let metrics = switcher_row_metrics(Theme::font_size());
        let rows = (0..4)
            .map(|i| SwitcherRowContent {
                name: format!("Monthly revenue reconciliation by region and product line {i}")
                    .into(),
                dirty: i % 2 == 0,
                is_original: i == 0,
                engine_kind: Some(ConnectionKind::Tds),
                status: long_trail(),
                is_connected: true,
            })
            .collect();
        let (_view, cx) = cx.add_window_view(|_window, _cx| SwitcherRows { rows, metrics });
        cx.run_until_parked();
        // `debug_bounds` wants a `'static` selector.
        let selector = |name: String| -> &'static str { Box::leak(name.into_boxed_str()) };
        let mut previous_bottom = f32::MIN;
        for pos in 0..4 {
            let row = cx
                .debug_bounds(selector(format!("tab-switcher-row-{pos}")))
                .expect("row laid out");
            let content = cx
                .debug_bounds(selector(format!("tab-switcher-row-content-{pos}")))
                .expect("content laid out");
            let trail = cx
                .debug_bounds(selector(format!("tab-switcher-trail-{pos}")))
                .expect("trail laid out");
            let (row_top, row_bottom) = (f32::from(row.top()), f32::from(row.bottom()));
            assert!(
                (f32::from(row.size.height) - metrics.row_h).abs() < 0.5,
                "row {pos} is its computed height"
            );
            assert!(
                row_top >= previous_bottom - 0.01,
                "row {pos} overlaps the row above"
            );
            for (what, b) in [("content", content), ("trail", trail)] {
                assert!(
                    f32::from(b.top()) >= row_top - 0.01
                        && f32::from(b.bottom()) <= row_bottom + 0.01,
                    "row {pos}: {what} {b:?} spills out of the row {row:?}"
                );
                assert!(
                    f32::from(b.right()) <= f32::from(row.right()) + 0.01,
                    "row {pos}: {what} runs past the row's right edge"
                );
            }
            previous_bottom = row_bottom;
        }
    }
}
