//! Reusable read-only DDL/source viewer for "go to definition" navigation.
//!
//! One `DdlViewerView` entity backs all three presentation modes (new tab,
//! per-tab right panel, popup window) so they never drift apart. It renders
//! a header (object name, clicked role, provenance) over a scrollable,
//! syntax-highlighted source body, with Copy and "Open as Query" actions.
//! The view is display-only — generated DDL is never dropped into an
//! editable query tab unless the user explicitly asks via "Open as Query".
//! A find bar (Find button or Cmd/Ctrl+F) highlights every match of a
//! case-insensitive search through the shared `find_replace` matcher and
//! steps through them, scrolling the current one into view.

use std::ops::Range;

use gpui::{
    ClipboardItem, Context, Entity, EventEmitter, FocusHandle, IntoElement, KeyDownEvent, Render,
    ScrollHandle, Styled, Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use sqlly_intel::intellisense::{ClickedRole, NavigationTarget};
use sqlly_services::DdlDocument;

use crate::find_replace::{self, FindQuery};
use crate::sql_syntax::{SyntaxToken, SyntaxTokenKind};
use crate::theme::Theme;

/// Search state of the viewer's find bar: the query and its matches in the
/// shown source, plus which match is current.
#[derive(Clone, Debug, Default)]
pub(crate) struct DdlFind {
    query: String,
    matches: Vec<Range<usize>>,
    truncated: bool,
    current: Option<usize>,
}

impl DdlFind {
    /// Search `body` for `query` (case-insensitive, literal) and make the
    /// first match current. An empty query clears the matches.
    pub(crate) fn set_query(&mut self, query: &str, body: &str) {
        self.query = query.to_string();
        self.refresh(body);
    }

    /// Re-run the current query against a new body (a refreshed definition).
    pub(crate) fn refresh(&mut self, body: &str) {
        let query = FindQuery::new(self.query.clone());
        let (matches, truncated) = match query.compile() {
            Ok(compiled) if !query.is_empty() => compiled.find_matches(body),
            _ => (Vec::new(), false),
        };
        self.current = if matches.is_empty() { None } else { Some(0) };
        self.matches = matches;
        self.truncated = truncated;
    }

    pub(crate) fn next(&mut self) {
        self.current = find_replace::next_index(self.matches.len(), self.current);
    }

    pub(crate) fn prev(&mut self) {
        self.current = find_replace::prev_index(self.matches.len(), self.current);
    }

    pub(crate) fn match_count(&self) -> usize {
        self.matches.len()
    }

    pub(crate) fn current(&self) -> Option<usize> {
        self.current
    }

    pub(crate) fn matches(&self) -> &[Range<usize>] {
        &self.matches
    }

    /// Zero-based line of the current match in `body`.
    pub(crate) fn current_line(&self, body: &str) -> Option<usize> {
        let range = self.matches.get(self.current?)?;
        let start = range.start.min(body.len());
        Some(
            body.as_bytes()[..start]
                .iter()
                .filter(|&&b| b == b'\n')
                .count(),
        )
    }

    fn count_label(&self) -> String {
        find_replace::count_label(self.current, self.matches.len(), self.truncated)
    }
}

/// How a run of source text relates to the find matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatchMark {
    None,
    Match,
    Current,
}

/// One run of a source line with a single color and match mark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LineSegment {
    /// Absolute byte range in the source.
    pub(crate) range: Range<usize>,
    pub(crate) kind: Option<SyntaxTokenKind>,
    pub(crate) mark: MatchMark,
}

/// Split the line `line_start..line_end` wherever its syntax color or find
/// mark changes. `matches` are absolute, sorted ranges; `current` indexes
/// into them.
pub(crate) fn line_segments(
    line_start: usize,
    line_end: usize,
    spans: &[SyntaxToken],
    matches: &[Range<usize>],
    current: Option<usize>,
) -> Vec<LineSegment> {
    if line_end <= line_start {
        return Vec::new();
    }
    let first_match = matches.partition_point(|m| m.end <= line_start);
    let line_matches: Vec<(usize, &Range<usize>)> = matches
        .iter()
        .enumerate()
        .skip(first_match)
        .take_while(|(_, m)| m.start < line_end)
        .collect();
    let line_spans: Vec<&SyntaxToken> = spans
        .iter()
        .filter(|sp| sp.end > line_start && sp.start < line_end)
        .collect();
    let mut cuts = vec![line_start, line_end];
    for sp in &line_spans {
        cuts.push(sp.start.clamp(line_start, line_end));
        cuts.push(sp.end.clamp(line_start, line_end));
    }
    for (_, m) in &line_matches {
        cuts.push(m.start.clamp(line_start, line_end));
        cuts.push(m.end.clamp(line_start, line_end));
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut segments: Vec<LineSegment> = Vec::new();
    for pair in cuts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let kind = line_spans
            .iter()
            .find(|sp| sp.start <= a && a < sp.end)
            .map(|sp| sp.kind);
        let mark = match line_matches.iter().find(|(_, m)| m.start <= a && a < m.end) {
            Some((i, _)) if Some(*i) == current => MatchMark::Current,
            Some(_) => MatchMark::Match,
            None => MatchMark::None,
        };
        match segments.last_mut() {
            Some(last) if last.kind == kind && last.mark == mark => last.range.end = b,
            _ => segments.push(LineSegment {
                range: a..b,
                kind,
                mark,
            }),
        }
    }
    segments
}

/// The open find bar: its search model and the lazily made input.
struct FindBar {
    model: DdlFind,
    input: Option<Entity<InputState>>,
    _subscription: Option<Subscription>,
    /// Focus the input on the next render (it needs a `Window`).
    focus_pending: bool,
}

/// Actions the hosting surface (app/tab/panel/popup) must handle.
pub enum DdlViewerEvent {
    /// The panel close button was clicked (side-panel mode only).
    Close,
    /// "Open as Query": put this SQL into a fresh editable query tab.
    OpenAsQuery(String),
}

impl EventEmitter<DdlViewerEvent> for DdlViewerView {}

enum ViewerState {
    Loading,
    Loaded { document: Box<DdlDocument> },
    Failed { message: String },
}

pub struct DdlViewerView {
    /// Title shown while loading and as the header fallback.
    title: String,
    state: ViewerState,
    /// Whether to render a close button (side-panel mode).
    show_close: bool,
    /// Engine family of the connection this DDL came from, so comments and
    /// strings are highlighted by that engine's rules rather than T-SQL's.
    dialect: sqlly_lang::sqlmodel::Dialect,
    /// The find bar, while open.
    find: Option<FindBar>,
    /// Scroll state of the source body, so a match can be scrolled to.
    scroll: ScrollHandle,
    /// Made on first render (constructors run without a `Context`).
    focus: Option<FocusHandle>,
}

impl DdlViewerView {
    pub fn loading(
        title: String,
        show_close: bool,
        dialect: sqlly_lang::sqlmodel::Dialect,
    ) -> Self {
        Self {
            title,
            state: ViewerState::Loading,
            show_close,
            dialect,
            find: None,
            scroll: ScrollHandle::new(),
            focus: None,
        }
    }

    /// Open the find bar (or re-focus it) with the cursor in its input.
    fn open_find(&mut self, cx: &mut Context<Self>) {
        match self.find.as_mut() {
            Some(bar) => bar.focus_pending = true,
            None => {
                self.find = Some(FindBar {
                    model: DdlFind::default(),
                    input: None,
                    _subscription: None,
                    focus_pending: true,
                })
            }
        }
        cx.notify();
    }

    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find = None;
        if let Some(focus) = &self.focus {
            window.focus(focus, cx);
        }
        cx.notify();
    }

    fn step_find(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        if forward {
            bar.model.next();
        } else {
            bar.model.prev();
        }
        self.scroll_to_current_match();
        cx.notify();
    }

    fn scroll_to_current_match(&self) {
        let (Some(bar), Some(body)) = (self.find.as_ref(), self.body_text()) else {
            return;
        };
        if let Some(line) = bar.model.current_line(body) {
            self.scroll.scroll_to_item(line);
        }
    }

    /// Make the find input (needs a `Window`) and focus it when asked.
    fn ensure_find_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        if bar.input.is_none() {
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "editor.ddl_viewer.find_placeholder",
                ))
            });
            let subscription = cx.subscribe_in(
                &input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| match event {
                    InputEvent::Change => {
                        let query = state.read(cx).value().to_string();
                        let body = this.body_text().unwrap_or_default().to_string();
                        // The input also reports a Change after Enter with
                        // the text untouched; re-searching then would snap
                        // back to the first match.
                        match this.find.as_mut() {
                            Some(bar) if bar.model.query != query => {
                                bar.model.set_query(&query, &body)
                            }
                            _ => return,
                        }
                        this.scroll_to_current_match();
                        cx.notify();
                    }
                    InputEvent::PressEnter { shift, .. } => this.step_find(!*shift, cx),
                    _ => {}
                },
            );
            bar.input = Some(input);
            bar._subscription = Some(subscription);
        }
        if bar.focus_pending {
            bar.focus_pending = false;
            if let Some(input) = &bar.input {
                input.update(cx, |state, cx| state.focus(window, cx));
            }
        }
    }

    /// Cmd+F (Ctrl+F off macOS) opens find; Escape closes it.
    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let modifiers = keystroke.modifiers;
        let command = if cfg!(target_os = "macos") {
            modifiers.platform && !modifiers.control
        } else {
            modifiers.control && !modifiers.platform
        };
        if command && !modifiers.alt && keystroke.key.eq_ignore_ascii_case("f") {
            if self.body_text().is_some() {
                self.open_find(cx);
                cx.stop_propagation();
            }
        } else if keystroke.key == "escape" && self.find.is_some() {
            self.close_find(window, cx);
            cx.stop_propagation();
        }
    }

    fn render_find_bar(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let s = Theme::scaled;
        let bar = self.find.as_ref()?;
        let input = bar.input.clone()?;
        let label = bar.model.count_label();
        let has_matches = bar.model.match_count() > 0;
        let show_label = !bar.model.query.is_empty();
        Some(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .w_full()
                .px(px(s(12.0)))
                .py(px(s(6.0)))
                .bg(Theme::panel_bg())
                .border_b(px(1.0))
                .border_color(Theme::border())
                .child(div().w(px(s(260.0))).child(Input::new(&input).small()))
                .children(show_label.then(|| {
                    div()
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::text_dim())
                        .child(label)
                }))
                .child(
                    Button::new("ddl-find-prev")
                        .ghost()
                        .xsmall()
                        .label("↑")
                        .tooltip(crate::localization::text("editor.ddl_viewer.find_previous"))
                        .disabled(!has_matches)
                        .on_click(cx.listener(|this, _, _window, cx| this.step_find(false, cx))),
                )
                .child(
                    Button::new("ddl-find-next")
                        .ghost()
                        .xsmall()
                        .label("↓")
                        .tooltip(crate::localization::text("editor.ddl_viewer.find_next"))
                        .disabled(!has_matches)
                        .on_click(cx.listener(|this, _, _window, cx| this.step_find(true, cx))),
                )
                .child(
                    Button::new("ddl-find-close")
                        .ghost()
                        .xsmall()
                        .label("✕")
                        .tooltip(crate::localization::text("editor.ddl_viewer.find_close"))
                        .on_click(cx.listener(|this, _, window, cx| this.close_find(window, cx))),
                )
                .into_any_element(),
        )
    }

    /// Swap in the fetch result. Documents with a domain error render the
    /// error state; documents without any source body do too.
    pub fn set_document(&mut self, document: DdlDocument, cx: &mut Context<Self>) {
        self.title = document.title.clone();
        if let Some(error) = document.error.clone() {
            self.state = ViewerState::Failed { message: error };
        } else if document.primary_source().is_some() {
            self.state = ViewerState::Loaded {
                document: Box::new(document),
            };
            let body = self.body_text().unwrap_or_default().to_string();
            if let Some(bar) = self.find.as_mut() {
                bar.model.refresh(&body);
            }
        } else {
            self.state = ViewerState::Failed {
                message: crate::localization::text("editor.ddl_viewer.no_source"),
            };
        }
        cx.notify();
    }

    pub fn set_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.state = ViewerState::Failed { message };
        cx.notify();
    }

    fn body_text(&self) -> Option<&str> {
        match &self.state {
            ViewerState::Loaded { document } => document.primary_source().map(|s| s.body.as_str()),
            _ => None,
        }
    }

    fn header_subtitle(&self) -> Option<String> {
        let ViewerState::Loaded { document } = &self.state else {
            return None;
        };
        let role = match &document.target {
            NavigationTarget::Table { clicked, .. } => match clicked {
                ClickedRole::View => crate::localization::text("editor.ddl_viewer.role_view"),
                ClickedRole::Column { name } => {
                    crate::localization::format("editor.ddl_viewer.role_column", &[("name", name)])
                }
                ClickedRole::Index { name } => {
                    crate::localization::format("editor.ddl_viewer.role_index", &[("name", name)])
                }
                _ => crate::localization::text("editor.ddl_viewer.role_table"),
            },
            NavigationTarget::Type { .. } => {
                crate::localization::text("editor.ddl_viewer.role_type")
            }
            NavigationTarget::Routine { clicked, .. } => match clicked {
                ClickedRole::ProcedureParameter { name } => crate::localization::format(
                    "editor.ddl_viewer.role_parameter",
                    &[("name", name)],
                ),
                ClickedRole::Function => {
                    crate::localization::text("editor.ddl_viewer.role_function")
                }
                _ => crate::localization::text("editor.ddl_viewer.role_procedure"),
            },
        };
        let source_label = document
            .primary_source()
            .map(|s| s.label.clone())
            .unwrap_or_default();
        Some(crate::localization::format(
            "editor.ddl_viewer.header_subtitle",
            &[
                ("role", &role),
                ("source", &source_label),
                ("database", &document.database),
                ("connection", &document.connection_label),
            ],
        ))
    }

    fn toolbar_button(
        &self,
        id: &'static str,
        label: String,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        crate::theme::accent_pill_button(id, label)
            .on_click(cx.listener(move |this, _event, _window, cx| on_click(this, cx)))
    }

    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let subtitle = self.header_subtitle();
        let has_body = self.body_text().is_some();

        let mut actions = div().flex().flex_row().items_center().gap(px(s(8.0)));
        if has_body {
            actions = actions.child(self.toolbar_button(
                "ddl-find",
                crate::localization::text("editor.ddl_viewer.find_button"),
                cx,
                |this, cx| this.open_find(cx),
            ));
            actions = actions.child(self.toolbar_button(
                "ddl-copy",
                crate::localization::text("editor.ddl_viewer.copy_button"),
                cx,
                |this, cx| {
                    if let Some(text) = this.body_text() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
                    }
                },
            ));
            actions = actions.child(self.toolbar_button(
                "ddl-open-as-query",
                crate::localization::text("editor.ddl_viewer.open_query_button"),
                cx,
                |this, cx| {
                    if let Some(text) = this.body_text() {
                        cx.emit(DdlViewerEvent::OpenAsQuery(text.to_string()));
                    }
                },
            ));
        }
        if self.show_close {
            actions = actions.child(
                div()
                    .id("ddl-close")
                    .cursor_pointer()
                    .px(px(s(8.0)))
                    .py(px(s(3.0)))
                    .rounded(px(s(4.0)))
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size()))
                    .child("✕")
                    .on_click(cx.listener(|_, _event, _window, cx| {
                        cx.emit(DdlViewerEvent::Close);
                    })),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .w_full()
            .px(px(s(12.0)))
            .py(px(s(8.0)))
            .bg(Theme::panel_bg())
            .border_b(px(1.0))
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(2.0)))
                    .overflow_hidden()
                    .child(
                        div()
                            .font(Theme::ui_font())
                            .text_size(px(Theme::font_size()))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Theme::text())
                            .child(self.title.clone()),
                    )
                    .children(subtitle.map(|s| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .child(s)
                    })),
            )
            .child(actions)
    }

    fn render_body(&mut self) -> impl IntoElement {
        let s = Theme::scaled;
        // Loading/Failed render a simple message; Loaded renders the read-only
        // source editor built below. Extract the state up front so the editor
        // bookkeeping (which mutates `self`) isn't fighting a borrow of
        // `self.state`.
        enum Body {
            Message {
                color: gpui::Hsla,
                text: String,
                center: bool,
            },
            Source(String),
        }
        let body = match &self.state {
            ViewerState::Loading => Body::Message {
                color: Theme::text_dim(),
                text: crate::localization::text("editor.ddl_viewer.loading"),
                center: true,
            },
            ViewerState::Failed { message } => Body::Message {
                color: Theme::warning(),
                text: message.clone(),
                center: false,
            },
            ViewerState::Loaded { document } => Body::Source(
                document
                    .primary_source()
                    .map(|src| src.body.clone())
                    .unwrap_or_default(),
            ),
        };

        let content = match body {
            Body::Message {
                color,
                text,
                center: true,
            } => div()
                .flex()
                .items_center()
                .justify_center()
                .size_full()
                .text_color(color)
                .text_size(px(Theme::font_size()))
                .child(text)
                .into_any_element(),
            Body::Message {
                color,
                text,
                center: false,
            } => div()
                .flex()
                .flex_col()
                .gap(px(s(8.0)))
                .p(px(s(16.0)))
                .child(
                    div()
                        .text_color(color)
                        .text_size(px(Theme::font_size()))
                        .child(text),
                )
                .into_any_element(),
            Body::Source(source) => {
                // Colorize with the app's own SQL highlighter — the same
                // `sql_syntax`/`Theme::syntax_color` palette the query editor
                // uses — so the DDL matches the editor. (The gpui-component
                // read-only editor doesn't highlight here: its SQL grammar is
                // behind a cargo feature this app doesn't enable.)
                let spans = crate::sql_syntax::classify_for(self.dialect, &source);
                let (matches, current) = self
                    .find
                    .as_ref()
                    .map(|bar| (bar.model.matches().to_vec(), bar.model.current()))
                    .unwrap_or_default();
                let mut line_start = 0usize;
                let mut lines: Vec<gpui::Div> = Vec::new();
                for line in source.split('\n') {
                    lines.push(if matches.is_empty() {
                        render_sql_line(line, line_start, &spans)
                    } else {
                        render_marked_line(
                            &source,
                            line_start,
                            line.len(),
                            &spans,
                            &matches,
                            current,
                        )
                    });
                    line_start += line.len() + 1; // + the '\n'
                }
                div()
                    .id("ddl-viewer-source")
                    .size_full()
                    .track_scroll(&self.scroll)
                    .overflow_scroll()
                    .p(px(s(12.0)))
                    .flex()
                    .flex_col()
                    .children(lines)
                    .into_any_element()
            }
        };

        div()
            .id("ddl-viewer-body")
            .flex_1()
            .w_full()
            .min_h_0()
            .bg(Theme::bg())
            .child(content)
    }
}

/// A source line split by syntax color and find mark: matches get the same
/// accent wash the query editor's find bar paints, stronger on the current
/// one.
fn render_marked_line(
    source: &str,
    line_start: usize,
    line_len: usize,
    spans: &[SyntaxToken],
    matches: &[Range<usize>],
    current: Option<usize>,
) -> gpui::Div {
    let segments = line_segments(line_start, line_start + line_len, spans, matches, current)
        .into_iter()
        .filter_map(|segment| {
            let text = source.get(segment.range.clone())?;
            let mut piece = div()
                .text_color(segment.kind.map_or_else(Theme::text, token_color))
                .child(text.to_string());
            piece = match segment.mark {
                MatchMark::Current => piece.bg(Theme::accent().opacity(0.45)),
                MatchMark::Match => piece.bg(Theme::accent().opacity(0.18)),
                MatchMark::None => piece,
            };
            Some(piece)
        });
    div()
        .flex()
        .flex_row()
        .w_full()
        .min_w_full()
        .min_h(px(Theme::scaled(18.0)))
        .whitespace_nowrap()
        .font_family(Theme::editor_font_family())
        .text_size(px(Theme::font_size_ratio(0.96)))
        .children(segments)
}

/// Map an SQL syntax token to the editor's theme palette (same colors the
/// query editor uses). Shared with the project-file-picker preview.
pub(crate) fn token_color(kind: SyntaxTokenKind) -> gpui::Hsla {
    Theme::syntax_color(kind)
}

/// One source line split into colored segments by the classified spans that
/// overlap it (same segmentation approach as `structured_data_viewer`).
pub(crate) fn render_sql_line(line: &str, line_start: usize, spans: &[SyntaxToken]) -> gpui::Div {
    let line_end = line_start + line.len();
    let mut segments: Vec<gpui::Div> = Vec::new();
    let mut cursor = line_start;

    for span in spans {
        if span.end <= line_start || span.start >= line_end || span.end <= cursor {
            continue;
        }
        if span.start > cursor {
            let gap = &line[cursor - line_start..span.start.min(line_end) - line_start];
            segments.push(div().text_color(Theme::text()).child(gap.to_string()));
        }
        let seg_start = span.start.max(cursor) - line_start;
        let seg_end = span.end.min(line_end) - line_start;
        segments.push(
            div()
                .text_color(token_color(span.kind))
                .child(line[seg_start..seg_end].to_string()),
        );
        cursor = span.end.min(line_end).max(cursor);
    }

    if cursor < line_end {
        let remaining = &line[cursor - line_start..];
        segments.push(div().text_color(Theme::text()).child(remaining.to_string()));
    }

    div()
        .flex()
        .flex_row()
        .w_full()
        .min_w_full()
        .min_h(px(Theme::scaled(18.0)))
        .whitespace_nowrap()
        .font_family(Theme::editor_font_family())
        .text_size(px(Theme::font_size_ratio(0.96)))
        .children(segments)
}

impl Render for DdlViewerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focus = self.focus.get_or_insert_with(|| cx.focus_handle()).clone();
        self.ensure_find_input(window, cx);
        let find_bar = self.render_find_bar(cx);
        div()
            .id("ddl-viewer")
            .key_context("DdlViewer")
            .track_focus(&focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                if !focus.contains_focused(window, cx) {
                    window.focus(&focus, cx);
                }
            })
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::bg())
            .font(Theme::ui_font())
            .text_color(Theme::text())
            .child(self.render_header(cx))
            .children(find_bar)
            .child(self.render_body())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_syntax::SyntaxTokenKind;

    #[test]
    fn token_color_uses_theme_syntax_accessors() {
        assert_eq!(token_color(SyntaxTokenKind::Keyword), Theme::keyword());
        assert_eq!(token_color(SyntaxTokenKind::Function), Theme::function());
        assert_eq!(token_color(SyntaxTokenKind::String), Theme::string());
    }

    const BODY: &str = "CREATE TABLE dbo.Orders (\n    OrderId int NOT NULL,\n    CustomerId int NULL,\n    OrderDate datetime2 NOT NULL\n);";

    #[test]
    fn find_counts_matches_ignoring_case_and_starts_on_the_first() {
        let mut find = DdlFind::default();
        find.set_query("ORDER", BODY);
        assert_eq!(find.match_count(), 3);
        assert_eq!(find.current(), Some(0));
        assert_eq!(find.current_line(BODY), Some(0));
        find.set_query("", BODY);
        assert_eq!(find.match_count(), 0);
        assert_eq!(find.current(), None);
        assert_eq!(find.current_line(BODY), None);
        find.set_query("nothing like this", BODY);
        assert_eq!(find.current(), None);
    }

    #[test]
    fn find_next_and_previous_wrap_and_track_the_line() {
        let mut find = DdlFind::default();
        find.set_query("int", BODY);
        assert_eq!(find.match_count(), 2);
        assert_eq!(find.current_line(BODY), Some(1));
        find.next();
        assert_eq!(find.current(), Some(1));
        assert_eq!(find.current_line(BODY), Some(2));
        find.next();
        assert_eq!(find.current(), Some(0), "wraps to the first match");
        find.prev();
        assert_eq!(find.current(), Some(1), "wraps to the last match");
        // A new body (a refreshed definition) re-runs the search.
        find.refresh("int int int");
        assert_eq!(find.match_count(), 3);
        assert_eq!(find.current(), Some(0));
    }

    #[test]
    fn line_segments_split_on_syntax_and_matches() {
        use crate::sql_syntax::SyntaxToken;
        // "SELECT OrderId" on one line: keyword span 0..6, match on "Order".
        let line = "SELECT OrderId";
        let spans = vec![SyntaxToken {
            start: 0,
            end: 6,
            kind: SyntaxTokenKind::Keyword,
        }];
        let matches: Vec<std::ops::Range<usize>> = std::iter::once(7..12).collect();
        let segs = line_segments(0, line.len(), &spans, &matches, Some(0));
        let shape: Vec<(std::ops::Range<usize>, Option<SyntaxTokenKind>, MatchMark)> = segs
            .into_iter()
            .map(|s| (s.range, s.kind, s.mark))
            .collect();
        assert_eq!(
            shape,
            vec![
                (0..6, Some(SyntaxTokenKind::Keyword), MatchMark::None),
                (6..7, None, MatchMark::None),
                (7..12, None, MatchMark::Current),
                (12..14, None, MatchMark::None),
            ]
        );
        // A match on another line leaves this one unmarked; an ordinary
        // (non-current) match is marked as such.
        let segs = line_segments(100, 100 + line.len(), &[], &[0..3, 107..112], Some(0));
        assert!(segs.iter().any(|s| s.mark == MatchMark::Match));
        assert!(segs.iter().all(|s| s.mark != MatchMark::Current));
        // A match that spans a line break marks each piece.
        let spanning: Vec<std::ops::Range<usize>> = std::iter::once(2..8).collect();
        let segs = line_segments(0, 4, &[], &spanning, Some(0));
        assert_eq!(
            segs.last().map(|s| (s.range.clone(), s.mark)),
            Some((2..4, MatchMark::Current))
        );
    }

    fn document(body: &str) -> DdlDocument {
        DdlDocument {
            title: "DDL: dbo.Orders".into(),
            target: NavigationTarget::Table {
                catalog: None,
                schema: "dbo".into(),
                name: "Orders".into(),
                clicked: ClickedRole::View,
            },
            sources: vec![sqlly_services::DdlSource {
                kind: sqlly_services::DdlSourceKind::Generated,
                label: "Generated DDL".into(),
                body: body.into(),
            }],
            connection_label: "dev".into(),
            database: "sales".into(),
            error: None,
        }
    }

    /// Cmd/Ctrl+F opens the bar with the cursor in it, typing searches,
    /// Enter / Shift+Enter step, Escape closes.
    #[gpui::test]
    fn find_bar_searches_steps_and_closes_from_the_keyboard(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let mut viewer = None;
        let (_root, cx) = cx.add_window_view(|window, cx| {
            let view = cx.new(|cx| {
                let mut v = DdlViewerView::loading(
                    "dbo.Orders".into(),
                    false,
                    sqlly_lang::sqlmodel::Dialect::SqlServer,
                );
                v.set_document(document(BODY), cx);
                v
            });
            viewer = Some(view.clone());
            gpui_component::Root::new(view, window, cx)
        });
        let viewer = viewer.expect("viewer");
        cx.run_until_parked();
        cx.update(|window, app| {
            let focus = viewer
                .read(app)
                .focus
                .clone()
                .expect("focus handle made on render");
            window.focus(&focus, app);
        });
        let open = if cfg!(target_os = "macos") {
            "cmd-f"
        } else {
            "ctrl-f"
        };
        cx.simulate_keystrokes(open);
        cx.run_until_parked();
        assert!(
            viewer.read_with(cx, |v, _| v.find.is_some()),
            "find bar opens"
        );

        cx.simulate_input("int");
        cx.run_until_parked();
        viewer.read_with(cx, |v, _| {
            let bar = v.find.as_ref().expect("bar");
            assert_eq!(bar.model.match_count(), 2);
            assert_eq!(bar.model.current(), Some(0));
        });
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(
            viewer.read_with(cx, |v, _| v.find.as_ref().and_then(|b| b.model.current())),
            Some(1)
        );
        cx.simulate_keystrokes("shift-enter");
        cx.run_until_parked();
        assert_eq!(
            viewer.read_with(cx, |v, _| v.find.as_ref().and_then(|b| b.model.current())),
            Some(0)
        );
        cx.simulate_keystrokes("escape");
        cx.run_until_parked();
        assert!(
            viewer.read_with(cx, |v, _| v.find.is_none()),
            "Escape closes"
        );
    }

    #[test]
    fn loading_state_has_no_body_or_subtitle() {
        let viewer = DdlViewerView::loading(
            "dbo.Orders".to_string(),
            true,
            sqlly_lang::sqlmodel::Dialect::SqlServer,
        );
        assert_eq!(viewer.title, "dbo.Orders");
        assert!(viewer.show_close);
        assert!(matches!(viewer.state, ViewerState::Loading));
        assert!(viewer.body_text().is_none());
        assert!(viewer.header_subtitle().is_none());
    }
}
