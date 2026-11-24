//! SQL cells: a live [`crate::editor::SqlEditor`] while the cell is being
//! worked on, a static highlighted preview otherwise, and the cell's own
//! results grid in a pane the user can resize.

use gpui::{
    AnyElement, Context, CursorStyle, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, StyledText, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::ButtonVariants as _;

use super::{CellRenderCx, NotebookCell};
use crate::notebook::preview;
use crate::notebook::view::{CellStatus, NotebookView};
use crate::theme::Theme;

pub struct SqlCell;

/// Vertical padding around the SQL area, in unscaled pixels.
const EDITOR_PADDING: f32 = 12.0;

/// Height of a SQL area showing `source`: its lines up to the cap, at the
/// editor's line height.
pub fn editor_height(source: &str, line_height: f32) -> f32 {
    preview::visible_lines(source) as f32 * line_height + Theme::scaled(EDITOR_PADDING)
}

impl NotebookCell for SqlCell {
    fn kind(&self) -> &'static str {
        "sql"
    }

    fn label_key(&self) -> &'static str {
        "notebook.shell.cell.kind_sql"
    }

    fn runnable(&self) -> bool {
        true
    }

    fn uses_editor(&self) -> bool {
        true
    }

    fn render_body(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        let s = Theme::scaled;
        if cell.collapsed {
            return div()
                .px(px(s(10.0)))
                .py(px(s(4.0)))
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::text_dim())
                .child(crate::localization::format(
                    if preview::line_count(&cell.source) == 1 {
                        "notebook.shell.cell.collapsed_lines_one"
                    } else {
                        "notebook.shell.cell.collapsed_lines"
                    },
                    &[("count", preview::line_count(&cell.source))],
                ))
                .into_any_element();
        }
        let line_height = cell.metrics.line_height();
        let height = editor_height(&cell.source, line_height);
        let sql_area = if let Some(editor) = cell.editor.clone() {
            div()
                .id(("nb-sql-editor", cell.index))
                .h(px(height))
                .w_full()
                .child(super::frame_cached(editor))
                .into_any_element()
        } else {
            let id = cell.id.clone();
            let body = match &cell.preview {
                Some(p) if !cell.source.is_empty() => div()
                    .text_color(Theme::text())
                    .child(StyledText::new(p.text.clone()).with_highlights(p.highlights.clone()))
                    .into_any_element(),
                _ => div()
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "notebook.shell.cell.sql_placeholder",
                    ))
                    .into_any_element(),
            };
            div()
                .id(("nb-sql-preview", cell.index))
                .h(px(height))
                .w_full()
                .overflow_hidden()
                .px(px(s(10.0)))
                .py(px(s(EDITOR_PADDING / 2.0)))
                .cursor(CursorStyle::IBeam)
                .whitespace_nowrap()
                .font_family(cell.metrics.font_family.clone())
                .text_size(px(cell.metrics.font_size))
                .line_height(px(line_height))
                .on_click(cx.listener(move |this, _, _, cx| this.edit_cell(&id, cx)))
                .child(body)
                .into_any_element()
        };

        let results = results_pane(cell, cx);

        div()
            .flex()
            .flex_col()
            .w_full()
            .child(sql_area)
            .when_some(cell.message.clone(), |d, message| {
                d.child(
                    div()
                        .px(px(s(10.0)))
                        .py(px(s(3.0)))
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(
                            if cell.state == CellStatus::Failed || cell.state == CellStatus::Blocked
                            {
                                Theme::error()
                            } else {
                                Theme::text_dim()
                            },
                        )
                        .child(message),
                )
            })
            .when(cell.state == CellStatus::Failed, |d| {
                let id = cell.id.clone();
                let label = crate::localization::text("notebook.ai.explain.button");
                d.child(
                    div().px(px(s(10.0))).pb(px(s(4.0))).child(
                        gpui_component::button::Button::new(("nb-explain-ai", cell.index))
                            .ghost()
                            .xsmall()
                            .label(label.clone())
                            .accessibility_label(label)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.explain_with_ai(&id, cx)),
                            ),
                    ),
                )
            })
            .children(results)
            .into_any_element()
    }

    /// Only the result grid, without the results chrome (tabs, messages,
    /// history, export): a SQL cell shown alone never shows its SQL. A cell
    /// that has not run says so with a Run button; a failure stays visible.
    fn render_output(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let ran = !matches!(cell.state, CellStatus::Idle | CellStatus::Queued)
            || cell.elapsed_ms.is_some();
        let failed = matches!(cell.state, CellStatus::Failed | CellStatus::Blocked);
        let grid = cell
            .results
            .clone()
            .filter(|_| ran && !failed)
            .and_then(|results| results.update(cx, |r, cx| r.embedded_grid(cx)));
        if let Some(grid) = grid {
            let height = if cell.results_height > 0.0 {
                cell.results_height
            } else {
                crate::notebook::embedded::grid_view::fit_height(&grid, cx)
            };
            let selector = format!("nb-embedded-grid-{}", cell.id);
            return div()
                .debug_selector(move || selector.clone())
                .h(px(height))
                .w_full()
                .min_h_0()
                .child(super::frame_cached(grid))
                .into_any_element();
        }
        let line = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .px(px(s(10.0)))
            .py(px(s(6.0)))
            .text_size(px(Theme::font_size_ratio(0.85)));
        if failed {
            let selector = format!("nb-embedded-error-{}", cell.id);
            let text = cell
                .message
                .clone()
                .unwrap_or_else(|| crate::localization::text("notebook.shell.state.failed"));
            return line
                .debug_selector(move || selector.clone())
                .text_color(Theme::error())
                .child(text)
                .into_any_element();
        }
        if cell.state.is_active() {
            return line
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    crate::notebook::view::status_label_key(cell.state, false),
                ))
                .into_any_element();
        }
        if ran {
            return line
                .text_color(Theme::text_dim())
                .child(
                    cell.message.clone().unwrap_or_else(|| {
                        crate::localization::text("notebook.embedded.sql.no_rows")
                    }),
                )
                .into_any_element();
        }
        let id = cell.id.clone();
        let label = crate::localization::text("notebook.embedded.sql.run");
        line.text_color(Theme::text_dim())
            .child(crate::localization::text("notebook.embedded.sql.not_run"))
            .child(
                gpui_component::button::Button::new(("nb-embedded-sql-run", cell.index))
                    .ghost()
                    .xsmall()
                    .label(label.clone())
                    .accessibility_label(label)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.request_run_from_host(&id, cx)),
                    ),
            )
            .into_any_element()
    }
}

/// A cell's results grid in a pane the user can resize, once it ran.
pub(crate) fn results_pane(
    cell: &CellRenderCx,
    cx: &mut Context<NotebookView>,
) -> Option<gpui::Div> {
    let s = Theme::scaled;
    let has_output =
        !matches!(cell.state, CellStatus::Idle | CellStatus::Queued) || cell.elapsed_ms.is_some();
    cell.results.clone().filter(|_| has_output).map(|results| {
        let id = cell.id.clone();
        let selector = format!("nb-results-pane-{id}");
        div()
            .debug_selector(move || selector.clone())
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(Theme::border())
            .child(
                div()
                    .h(px(cell.results_height))
                    .w_full()
                    .min_h_0()
                    .child(super::frame_cached(results)),
            )
            .child(
                div()
                    .id(("nb-results-resize", cell.index))
                    .h(px(s(5.0)))
                    .w_full()
                    .cursor(CursorStyle::ResizeRow)
                    .hover(|style| style.bg(Theme::border_hover()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            this.begin_results_drag(&id, f32::from(event.position.y), cx);
                            cx.stop_propagation();
                        }),
                    ),
            )
    })
}
