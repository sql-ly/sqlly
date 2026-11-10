//! The "SQLLY Formatting Directives" reference window.
//!
//! Pure presentation: the directive grammar, hover span detection,
//! diagnostics, and the documentation table all live in
//! [`crate::sqlly_directive`] — the single lexer every parser and editor
//! surface shares. This module only renders that table: a filterable family
//! list on the left, the selected family's card on the right.

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, FontWeight, IntoElement, KeyDownEvent, MouseButton,
    Render, ScrollHandle, SharedString, Styled, Subscription, Task, TitlebarOptions, Window,
    WindowBounds, WindowOptions, div, point, prelude::*, px, size,
};
use gpui_component::Sizable;
use gpui_component::input::{Input, InputEvent, InputState};

use crate::about::APP_ID;
use crate::sqlly_directive::{DIRECTIVES, DirectiveDoc};
use crate::theme::Theme;

/// How long a sample row keeps its "copied" confirmation before reverting.
const COPIED_CONFIRMATION_MS: u64 = 1600;

/// The reference panel rendered in its own window.
pub struct FormatDirectivesPanel {
    filter_input: Entity<InputState>,
    /// Current filter text, mirrored from `filter_input`; blank shows all.
    filter: String,
    /// Index into [`DIRECTIVES`] of the family shown in the detail pane.
    selected: usize,
    /// Detail-pane scroll, reset to the top when the selection changes.
    detail_scroll: ScrollHandle,
    /// One tab stop per sample row so copy is reachable without a mouse.
    sample_focus: Vec<Vec<FocusHandle>>,
    /// The sample that most recently confirmed a copy (transient feedback).
    copied: Option<(usize, usize)>,
    _clear_copied: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl FormatDirectivesPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "editor.format_directives.filter_placeholder",
            ))
        });
        let subscription = cx.subscribe_in(
            &filter_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.filter = input.read(cx).value().to_string();
                    this.keep_selection_visible();
                    cx.notify();
                }
            },
        );
        let sample_focus = DIRECTIVES
            .iter()
            .map(|doc| doc.samples.iter().map(|_| cx.focus_handle()).collect())
            .collect();
        Self {
            filter_input,
            filter: String::new(),
            selected: 0,
            detail_scroll: ScrollHandle::new(),
            sample_focus,
            copied: None,
            _clear_copied: None,
            _subscriptions: vec![subscription],
        }
    }

    /// Focus the filter field — called right after opening the window so
    /// typing narrows the reference immediately.
    pub fn focus_filter(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_input.update(cx, |state, cx| {
            state.focus(window, cx);
        });
    }

    /// Indices of the families the current filter matches, in table order.
    fn visible(&self) -> Vec<usize> {
        visible_families(&self.filter)
    }

    /// When the filter hides the selected family, select the first match so
    /// the detail pane never shows something the list no longer offers.
    fn keep_selection_visible(&mut self) {
        self.select(visible_selection(&self.visible(), self.selected));
    }

    fn select(&mut self, index: usize) {
        if index != self.selected {
            self.selected = index;
            self.detail_scroll.set_offset(point(px(0.0), px(0.0)));
        }
    }

    /// Up/Down step through the visible families.
    fn step_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.select(stepped_selection(&self.visible(), self.selected, delta));
        cx.notify();
    }

    /// Copy one sample to the clipboard and flash the confirmation on its row.
    fn copy_sample(&mut self, directive_index: usize, sample_index: usize, cx: &mut Context<Self>) {
        let Some(sample) = DIRECTIVES
            .get(directive_index)
            .and_then(|doc| doc.samples.get(sample_index))
        else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(sample.to_string()));
        self.copied = Some((directive_index, sample_index));
        // Revert the confirmation shortly after; copying another sample first
        // replaces the pending revert (dropping the old task cancels it).
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        self._clear_copied = Some(cx.spawn(async move |_this, _cx| {
            async_cx
                .background_executor()
                .timer(std::time::Duration::from_millis(COPIED_CONFIRMATION_MS))
                .await;
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    if this.copied == Some((directive_index, sample_index)) {
                        this.copied = None;
                        cx.notify();
                    }
                })
                .ok();
        }));
        cx.notify();
    }

    fn render_header(&self) -> impl IntoElement {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(s(12.0)))
            .px(px(s(20.0)))
            .py(px(s(14.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(s(220.0)))
                    .gap(px(s(3.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(1.15)))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(Theme::text())
                            .child(crate::localization::text(
                                "editor.format_directives.header_title",
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "editor.format_directives.header_description",
                            )),
                    ),
            )
            .child(
                div()
                    .w(px(s(220.0)))
                    .child(Input::new(&self.filter_input).cleanable(true).small()),
            )
    }

    fn render_nav(&self, view: &Entity<Self>, visible: &[usize]) -> impl IntoElement {
        let s = Theme::scaled;
        let mut nav = div()
            .id("format-directives-nav")
            .flex()
            .flex_col()
            .flex_shrink_0()
            .w(px(s(210.0)))
            .h_full()
            .overflow_y_scroll()
            .py(px(s(8.0)))
            .px(px(s(8.0)))
            .gap(px(s(2.0)))
            .bg(Theme::sidebar_bg())
            .border_r_1()
            .border_color(Theme::border());
        if visible.is_empty() {
            return nav.child(
                div()
                    .px(px(s(8.0)))
                    .py(px(s(6.0)))
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "editor.format_directives.no_matches",
                        &[("query", &display_query(&self.filter))],
                    )),
            );
        }
        for &index in visible {
            let doc = &DIRECTIVES[index];
            let selected = index == self.selected;
            let view = view.clone();
            nav = nav.child(
                div()
                    .id(("directive-nav", index))
                    .flex()
                    .flex_col()
                    .gap(px(s(1.0)))
                    .px(px(s(10.0)))
                    .py(px(s(6.0)))
                    .rounded(px(s(5.0)))
                    .cursor(gpui::CursorStyle::PointingHand)
                    .when(selected, |row| row.bg(Theme::selection_fill()))
                    .when(!selected, |row| {
                        row.hover(|style| style.bg(Theme::row_hover()))
                    })
                    .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
                        view.update(cx, |this, cx| {
                            this.select(index);
                            cx.notify();
                        });
                    })
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .when(selected, |el| el.font_weight(FontWeight::SEMIBOLD))
                            .text_color(Theme::text())
                            .child(doc.title),
                    )
                    .child(
                        div()
                            .font_family(Theme::editor_font_family())
                            .text_size(px(Theme::font_size_ratio(0.76)))
                            .text_color(Theme::text_dim())
                            .truncate()
                            .child(doc.heading),
                    ),
            );
        }
        nav
    }

    /// A small uppercase label over each section of the detail card.
    fn section_label(text: &str) -> impl IntoElement {
        div()
            .text_size(px(Theme::font_size_ratio(0.72)))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(Theme::text_dim())
            .child(text.to_uppercase())
    }

    /// A monospace block on a raised surface — syntax and samples.
    fn code_block() -> gpui::Div {
        let s = Theme::scaled;
        div()
            .px(px(s(12.0)))
            .py(px(s(8.0)))
            .rounded(px(s(6.0)))
            .border_1()
            .border_color(Theme::border())
            .bg(Theme::panel_bg())
            .font_family(Theme::editor_font_family())
            .text_size(px(Theme::font_size_ratio(0.85)))
            .text_color(Theme::text())
    }

    fn render_detail(&self, view: &Entity<Self>, window: &Window) -> impl IntoElement {
        let s = Theme::scaled;
        let directive_index = self.selected;
        let doc = &DIRECTIVES[directive_index];

        let params = doc.params.iter().enumerate().fold(
            div()
                .flex()
                .flex_col()
                .rounded(px(s(6.0)))
                .border_1()
                .border_color(Theme::border())
                .overflow_hidden(),
            |table, (row, (name, _desc))| {
                table.child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_x(px(s(14.0)))
                        .gap_y(px(s(2.0)))
                        .px(px(s(12.0)))
                        .py(px(s(7.0)))
                        .when(row > 0, |r| r.border_t_1().border_color(Theme::border()))
                        .when(row % 2 == 1, |r| r.bg(Theme::row_alt()))
                        .child(
                            div()
                                .w(px(s(200.0)))
                                .flex_shrink_0()
                                .font_family(Theme::editor_font_family())
                                .text_size(px(Theme::font_size_ratio(0.82)))
                                .text_color(Theme::text())
                                .child(name.to_string()),
                        )
                        .child(
                            div()
                                // flex_1 + min width: the description wraps
                                // beside the name, or drops under it when
                                // the window is narrow.
                                .flex_1()
                                .min_w(px(s(160.0)))
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text_dim())
                                .child(directive_param(directive_index, row)),
                        ),
                )
            },
        );

        div()
            .id("format-directives-detail")
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.detail_scroll)
            .px(px(s(26.0)))
            .py(px(s(22.0)))
            .gap(px(s(20.0)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .items_center()
                            .gap(px(s(10.0)))
                            .child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(1.3)))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(Theme::text())
                                    .child(directive_title(directive_index)),
                            )
                            .child(
                                div()
                                    .px(px(s(8.0)))
                                    .py(px(s(2.0)))
                                    .rounded(px(s(4.0)))
                                    .border_1()
                                    .border_color(Theme::border())
                                    .bg(Theme::panel_bg())
                                    .font_family(Theme::editor_font_family())
                                    .text_size(px(Theme::font_size_ratio(0.8)))
                                    .text_color(Theme::text_dim())
                                    .child(doc.heading),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.95)))
                            .text_color(Theme::text())
                            .child(directive_overview(directive_index)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(6.0)))
                    .child(Self::section_label(&crate::localization::text(
                        "editor.format_directives.syntax_section",
                    )))
                    .child(Self::code_block().child(doc.syntax.to_string())),
            )
            .when(!doc.params.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(6.0)))
                        .child(Self::section_label(&crate::localization::text(
                            "editor.format_directives.parameters_section",
                        )))
                        .child(params),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(6.0)))
                    .child(Self::section_label(&crate::localization::text(
                        "editor.format_directives.examples_section",
                    )))
                    .children(
                        doc.samples
                            .iter()
                            .enumerate()
                            .map(|(sample_index, sample)| {
                                self.render_sample(
                                    view,
                                    directive_index,
                                    sample_index,
                                    sample,
                                    window,
                                )
                            }),
                    ),
            )
    }

    fn render_sample(
        &self,
        view: &Entity<Self>,
        directive_index: usize,
        sample_index: usize,
        sample: &'static str,
        window: &Window,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        let focus_handle = &self.sample_focus[directive_index][sample_index];
        let focused = focus_handle.is_focused(window);
        let is_copied = self.copied == Some((directive_index, sample_index));
        let group = SharedString::from(format!("dir-sample-{directive_index}-{sample_index}"));

        let click_view = view.clone();
        let key_view = view.clone();

        // Each sample is click-to-copy (or focus + Enter/Space) so directives
        // can be pasted into the editor instead of retyped. The copy chip is
        // always visible (never hover-only), brightens on hover or focus, and
        // confirms the copy in place.
        Self::code_block()
            // The formatted id is unique by construction.
            .id(group.clone())
            .track_focus(focus_handle)
            .group(group.clone())
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(10.0)))
            .when(focused, |el| el.border_color(Theme::accent()))
            .cursor(gpui::CursorStyle::PointingHand)
            .hover(|style| style.border_color(Theme::border_hover()))
            .on_mouse_down(MouseButton::Left, move |_event, _window, cx| {
                click_view.update(cx, |this, cx| {
                    this.copy_sample(directive_index, sample_index, cx);
                });
            })
            .on_key_down(move |event: &KeyDownEvent, _window, cx| {
                let key = event.keystroke.key.as_str();
                if key == "enter" || key == "space" {
                    key_view.update(cx, |this, cx| {
                        this.copy_sample(directive_index, sample_index, cx);
                    });
                }
            })
            .child(
                // flex_1: long samples wrap inside the block and the copy
                // chip docks at a consistent right edge.
                div().flex_1().child(sample.to_string()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .px(px(s(8.0)))
                    .py(px(s(2.0)))
                    .rounded(px(s(4.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .bg(Theme::bg())
                    .font_family(Theme::ui_font().family)
                    .text_size(px(Theme::font_size_ratio(0.75)))
                    // The confirmation stays in body ink: success green on a
                    // raised surface only measures ~3.8:1 in the light
                    // palettes (the wcag matrix rejects it); ink is 7:1.
                    .text_color(if is_copied || focused {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .when(is_copied, |el| el.font_weight(FontWeight::SEMIBOLD))
                    .group_hover(group, |style| style.text_color(Theme::text()))
                    .child(if is_copied {
                        crate::localization::text("editor.format_directives.copied_button")
                    } else if focused {
                        crate::localization::text("editor.format_directives.enter_to_copy_button")
                    } else {
                        crate::localization::text("editor.format_directives.copy_button")
                    }),
            )
    }
}

impl Render for FormatDirectivesPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let visible = self.visible();

        let body = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_h_0()
            .child(self.render_nav(&view, &visible))
            .when(!visible.is_empty(), |body| {
                body.child(self.render_detail(&view, window))
            })
            .when(visible.is_empty(), |body| {
                body.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .px(px(Theme::scaled(24.0)))
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::format(
                            "editor.format_directives.clear_filter",
                            &[("count", &DIRECTIVES.len().to_string())],
                        )),
                )
            });

        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::bg())
            .text_color(Theme::text())
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                match event.keystroke.key.as_str() {
                    "up" => this.step_selection(-1, cx),
                    "down" => this.step_selection(1, cx),
                    _ => {}
                }
            }))
            .child(self.render_header())
            .child(body)
    }
}

/// Indices into [`DIRECTIVES`] of the families `query` matches.
fn visible_families(query: &str) -> Vec<usize> {
    DIRECTIVES
        .iter()
        .enumerate()
        .filter(|(_, doc)| directive_matches(doc, query))
        .map(|(index, _)| index)
        .collect()
}

/// `selected` if the filter still shows it, else the first visible family
/// (or `selected` unchanged when nothing matches).
fn visible_selection(visible: &[usize], selected: usize) -> usize {
    if visible.contains(&selected) {
        selected
    } else {
        visible.first().copied().unwrap_or(selected)
    }
}

/// The visible family `delta` rows from `selected`, clamped to the list ends.
fn stepped_selection(visible: &[usize], selected: usize, delta: isize) -> usize {
    let Some(pos) = visible.iter().position(|&i| i == selected) else {
        return visible_selection(visible, selected);
    };
    visible[pos.saturating_add_signed(delta).min(visible.len() - 1)]
}

/// The empty state echoes the query back to the user; trim it and bound its
/// length so a very long query cannot blow out the message.
fn display_query(query: &str) -> String {
    const MAX_CHARS: usize = 32;
    let trimmed = query.trim();
    if trimmed.chars().count() <= MAX_CHARS {
        trimmed.to_string()
    } else {
        format!("{}…", trimmed.chars().take(MAX_CHARS).collect::<String>())
    }
}

const DIRECTIVE_TITLE_KEYS: [&str; 16] = [
    "editor.format_directives.docs.column_format_title",
    "editor.format_directives.docs.result_names_title",
    "editor.format_directives.docs.result_view_title",
    "editor.format_directives.docs.pivot_title",
    "editor.format_directives.docs.chart_title",
    "editor.format_directives.docs.conditional_title",
    "editor.format_directives.docs.computed_column_title",
    "editor.format_directives.docs.grid_columns_title",
    "editor.format_directives.docs.canvas_layout_title",
    "editor.format_directives.docs.result_layout_title",
    "editor.format_directives.docs.default_result_title",
    "editor.format_directives.docs.parameters_title",
    "editor.format_directives.docs.variables_title",
    "editor.format_directives.docs.pivot_number_title",
    "editor.format_directives.docs.natural_date_title",
    "map_view.directive_docs.title",
];

const DIRECTIVE_OVERVIEW_KEYS: [&str; 16] = [
    "editor.format_directives.docs.column_format_overview",
    "editor.format_directives.docs.result_names_overview",
    "editor.format_directives.docs.result_view_overview",
    "editor.format_directives.docs.pivot_overview",
    "editor.format_directives.docs.chart_overview",
    "editor.format_directives.docs.conditional_overview",
    "editor.format_directives.docs.computed_column_overview",
    "editor.format_directives.docs.grid_columns_overview",
    "editor.format_directives.docs.canvas_layout_overview",
    "editor.format_directives.docs.result_layout_overview",
    "editor.format_directives.docs.default_result_overview",
    "editor.format_directives.docs.parameters_overview",
    "editor.format_directives.docs.variables_overview",
    "editor.format_directives.docs.pivot_number_overview",
    "editor.format_directives.docs.natural_date_overview",
    "map_view.directive_docs.overview",
];

const DIRECTIVE_PARAM_KEYS: [&[&str]; 16] = [
    &[
        "editor.format_directives.docs.format_column",
        "editor.format_directives.docs.format_number",
        "editor.format_directives.docs.format_thousands",
        "editor.format_directives.docs.format_date",
        "editor.format_directives.docs.format_boolean",
        "editor.format_directives.docs.format_binary",
        "editor.format_directives.docs.format_text",
        "editor.format_directives.docs.format_width",
    ],
    &[
        "editor.format_directives.docs.name_index",
        "editor.format_directives.docs.name_text",
    ],
    &[
        "editor.format_directives.docs.view_grid",
        "editor.format_directives.docs.view_pivot",
        "editor.format_directives.docs.view_chart",
        "map_view.directive_docs.view_map",
    ],
    &[
        "editor.format_directives.docs.pivot_rows",
        "editor.format_directives.docs.pivot_columns",
        "editor.format_directives.docs.pivot_value",
        "editor.format_directives.docs.pivot_format",
        "editor.format_directives.docs.pivot_valueformat",
        "editor.format_directives.docs.pivot_result",
    ],
    &[
        "editor.format_directives.docs.chart_kind",
        "editor.format_directives.docs.chart_label",
        "editor.format_directives.docs.chart_values",
        "editor.format_directives.docs.chart_aggregate",
        "editor.format_directives.docs.chart_order",
        "editor.format_directives.docs.chart_top",
        "editor.format_directives.docs.chart_navigate",
        "editor.format_directives.docs.chart_result",
    ],
    &[
        "editor.format_directives.docs.conditional_column",
        "editor.format_directives.docs.conditional_when",
        "editor.format_directives.docs.conditional_text",
        "editor.format_directives.docs.conditional_fill",
        "editor.format_directives.docs.conditional_bold",
        "editor.format_directives.docs.conditional_scale",
        "editor.format_directives.docs.conditional_bar",
    ],
    &[
        "editor.format_directives.docs.computed_name",
        "editor.format_directives.docs.computed_formula",
        "editor.format_directives.docs.computed_row_refs",
        "editor.format_directives.docs.computed_range",
        "editor.format_directives.docs.computed_value",
        "editor.format_directives.docs.computed_after",
        "editor.format_directives.docs.computed_result",
    ],
    &[
        "editor.format_directives.docs.grid_result",
        "editor.format_directives.docs.grid_order",
        "editor.format_directives.docs.grid_hide",
        "editor.format_directives.docs.grid_freeze",
        "editor.format_directives.docs.grid_sort",
        "editor.format_directives.docs.grid_group",
        "editor.format_directives.docs.grid_filter",
        "editor.format_directives.docs.grid_label",
    ],
    &[
        "editor.format_directives.docs.canvas_dimensions",
        "editor.format_directives.docs.canvas_result",
    ],
    &[
        "editor.format_directives.docs.layout_mode",
        "editor.format_directives.docs.layout_lines",
        "editor.format_directives.docs.layout_lines_result",
    ],
    &["editor.format_directives.docs.focus_result"],
    &[
        "editor.format_directives.docs.param_name",
        "editor.format_directives.docs.param_type",
        "editor.format_directives.docs.param_default",
    ],
    &[
        "editor.format_directives.docs.variable_name",
        "editor.format_directives.docs.variable_value",
    ],
    &[
        "editor.format_directives.docs.number_align",
        "editor.format_directives.docs.number_decimals",
        "editor.format_directives.docs.number_red",
        "editor.format_directives.docs.number_parens",
        "editor.format_directives.docs.number_thousands",
    ],
    &["editor.format_directives.docs.date_phrase"],
    &[
        "map_view.directive_docs.index",
        "map_view.directive_docs.lat_lon",
        "map_view.directive_docs.geom",
        "map_view.directive_docs.srid",
        "map_view.directive_docs.label",
        "map_view.directive_docs.tooltip",
        "map_view.directive_docs.mode",
        "map_view.directive_docs.color",
        "map_view.directive_docs.size",
        "map_view.directive_docs.weight",
        "map_view.directive_docs.radius_blur",
        "map_view.directive_docs.intensity_opacity",
        "map_view.directive_docs.palette",
        "map_view.directive_docs.cluster",
        "map_view.directive_docs.basemap",
        "map_view.directive_docs.center_zoom",
        "map_view.directive_docs.limit",
        "map_view.directive_docs.click",
        "map_view.directive_docs.open",
    ],
];

fn directive_title(index: usize) -> String {
    crate::localization::text(DIRECTIVE_TITLE_KEYS[index])
}

fn directive_overview(index: usize) -> String {
    crate::localization::text(DIRECTIVE_OVERVIEW_KEYS[index])
}

fn directive_param(index: usize, param: usize) -> String {
    crate::localization::text(DIRECTIVE_PARAM_KEYS[index][param])
}

/// Case-insensitive substring match across everything a family shows:
/// title, heading, overview, syntax, params, and samples. Blank matches all.
fn directive_matches(doc: &DirectiveDoc, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let hits = |text: &str| text.to_lowercase().contains(query.as_str());
    hits(doc.title)
        || hits(doc.heading)
        || hits(doc.overview)
        || hits(doc.syntax)
        || doc
            .params
            .iter()
            .any(|(name, desc)| hits(name) || hits(desc))
        || doc.samples.iter().any(|sample| hits(sample))
}

/// Open the formatting-directives reference in its own centered window.
pub fn open_format_directives_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(860.), px(640.)), cx);
    let _ = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(600.), px(420.))),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(crate::localization::text(
                    "editor.format_directives.window_title",
                ))),
                ..Default::default()
            }),
            app_id: Some(APP_ID.into()),
            is_resizable: true,
            ..Default::default()
        },
        |window, cx| {
            let panel = cx.new(|cx| FormatDirectivesPanel::new(window, cx));
            panel.update(cx, |this, cx| this.focus_filter(window, cx));
            cx.new(|cx| gpui_component::Root::new(panel, window, cx))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_filter_matches_every_family() {
        assert_eq!(DIRECTIVES.len(), 16);
        assert_eq!(DIRECTIVE_TITLE_KEYS.len(), DIRECTIVES.len());
        assert_eq!(DIRECTIVE_OVERVIEW_KEYS.len(), DIRECTIVES.len());
        assert_eq!(DIRECTIVE_PARAM_KEYS.len(), DIRECTIVES.len());
        for (index, doc) in DIRECTIVES.iter().enumerate() {
            assert_eq!(DIRECTIVE_PARAM_KEYS[index].len(), doc.params.len());
            assert!(!directive_title(index).is_empty());
            assert!(!directive_overview(index).is_empty());
        }
        assert!(DIRECTIVES.iter().all(|doc| directive_matches(doc, "")));
        assert!(DIRECTIVES.iter().all(|doc| directive_matches(doc, "   ")));
    }

    #[test]
    fn filter_is_case_insensitive_and_matches_a_heading() {
        // "pivot:" (with the colon) matches the pivot family's own text and
        // the view family, whose param text references "-- sqlly pivot:".
        let matches: Vec<_> = DIRECTIVES
            .iter()
            .filter(|doc| directive_matches(doc, "PIVOT:"))
            .collect();
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().any(|doc| doc.heading == "-- sqlly pivot:"));
        assert!(matches.iter().any(|doc| doc.heading == "-- sqlly view:"));
    }

    #[test]
    fn filter_searches_params_and_samples_not_just_headings() {
        // "decimals" never appears in a heading — only in the format and
        // options-fragment params and the pivot family's sample.
        let matches: Vec<_> = DIRECTIVES
            .iter()
            .filter(|doc| directive_matches(doc, "decimals"))
            .collect();
        assert_eq!(matches.len(), 3);
        assert!(matches.iter().any(|doc| doc.heading == "-- sqlly format:"));
        assert!(matches.iter().any(|doc| doc.heading == "-- sqlly pivot:"));
        assert!(
            matches
                .iter()
                .any(|doc| doc.heading == "format / valueformat options")
        );
    }

    #[test]
    fn unmatched_query_matches_nothing() {
        assert!(
            !DIRECTIVES
                .iter()
                .any(|doc| directive_matches(doc, "zzz-no-such-directive"))
        );
    }

    #[test]
    fn display_query_trims_and_bounds_the_echoed_filter() {
        assert_eq!(display_query("   pivot  "), "pivot");
        assert_eq!(display_query("short"), "short");
        let shown = display_query(&"x".repeat(80));
        assert!(shown.ends_with('…'));
        assert_eq!(shown.chars().count(), 33); // 32 kept + ellipsis
    }

    #[test]
    fn filter_matching_survives_extreme_queries() {
        // Emoji, CJK, RTL text, and a 10k-character query must not panic or
        // produce false positives — the filter is the window's one input.
        for query in ["🔍🔍🔍", "ピボット", "عربية"] {
            assert!(!DIRECTIVES.iter().any(|doc| directive_matches(doc, query)));
        }
        let huge = "z".repeat(10_000);
        assert!(!DIRECTIVES.iter().any(|doc| directive_matches(doc, &huge)));
    }

    #[test]
    fn every_family_has_a_distinct_title_the_filter_finds() {
        let mut titles: Vec<_> = DIRECTIVES.iter().map(|doc| doc.title).collect();
        titles.sort_unstable();
        titles.dedup();
        assert_eq!(titles.len(), DIRECTIVES.len());
        assert_eq!(visible_families("canvas layout").len(), 1);
    }

    #[test]
    fn filtering_out_the_selection_selects_the_first_match() {
        let charts = visible_families("chart");
        assert!(!charts.contains(&0));
        assert_eq!(visible_selection(&charts, 0), charts[0]);
        assert_eq!(visible_selection(&charts, charts[1]), charts[1]);
        assert_eq!(visible_selection(&[], 3), 3);
    }

    #[test]
    fn stepping_the_selection_clamps_at_both_ends() {
        let all = visible_families("");
        assert_eq!(stepped_selection(&all, 0, -1), 0);
        assert_eq!(stepped_selection(&all, 0, 1), 1);
        let last = *all.last().unwrap();
        assert_eq!(stepped_selection(&all, last, 1), last);
        let charts = visible_families("chart");
        assert_eq!(stepped_selection(&charts, charts[0], 1), charts[1]);
        assert_eq!(stepped_selection(&[], 2, 1), 2);
    }

    #[test]
    fn display_query_truncates_on_char_boundaries() {
        // Multi-byte scalars: the cut must not panic or split a character.
        let shown = display_query(&"🔍".repeat(40));
        assert!(shown.ends_with('…'));
        assert_eq!(shown.chars().count(), 33);
    }
}
