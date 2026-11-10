//! Saved Charts UI: the "Save chart to library…" prompt and the Saved Charts
//! panel (list + Open / Rename / Duplicate / Copy SQL / Delete / Dashboard).
//!
//! Both views are passive dialogs on the shared chrome: they mutate the
//! library store directly (it is local and synchronous) and emit events for
//! anything that needs the workbench — running a chart, opening the
//! dashboard, status text.

#![cfg(not(target_arch = "wasm32"))]

use gpui::{
    ClipboardItem, Context, Entity, EventEmitter, IntoElement, MouseButton, ParentElement, Render,
    Styled, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::saved_charts::{
    SavedChart, SavedChartDraft, SavedChartStore, next_refresh_choice, refresh_label, relative_age,
};
use crate::theme::Theme;

// ── Save prompt ─────────────────────────────────────────────────────────────

pub enum SaveChartDialogEvent {
    Saved(Box<SavedChart>),
    Closed,
}

/// The small "Save chart to library…" prompt: a name, a refresh-interval
/// cycle button, Save. The name input is materialized at render time
/// (renders have the `Window` the results event handler lacks).
pub struct SaveChartDialog {
    store: SavedChartStore,
    draft: SavedChartDraft,
    input: Option<Entity<InputState>>,
    error: Option<String>,
    frame: DialogFrame,
    _subscription: Option<gpui::Subscription>,
}

impl EventEmitter<SaveChartDialogEvent> for SaveChartDialog {}

impl SaveChartDialog {
    pub fn new(store: SavedChartStore, draft: SavedChartDraft) -> Self {
        Self {
            store,
            draft,
            input: None,
            error: None,
            frame: DialogFrame::new_auto(Theme::scaled(440.0)),
            _subscription: None,
        }
    }

    fn ensure_input(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(input) = &self.input {
            return input.clone();
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.saved_charts.save_dialog.name_placeholder",
            ))
        });
        let initial = self.draft.name.clone();
        if !initial.is_empty() {
            input.update(cx, |state, cx| state.set_value(initial, window, cx));
        }
        let subscription =
            cx.subscribe(&input, |this: &mut Self, state, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let name = state.read(cx).value().to_string();
                    this.save(name, cx);
                }
            });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.input = Some(input.clone());
        self._subscription = Some(subscription);
        input
    }

    fn save(&mut self, name: String, cx: &mut Context<Self>) {
        let mut draft = self.draft.clone();
        draft.name = name;
        match self.store.save_new(draft) {
            Ok(saved) => {
                cx.emit(SaveChartDialogEvent::Saved(Box::new(saved)));
                cx.emit(SaveChartDialogEvent::Closed);
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn cycle_refresh(&mut self, cx: &mut Context<Self>) {
        self.draft.refresh_secs = next_refresh_choice(self.draft.refresh_secs);
        cx.notify();
    }
}

impl Render for SaveChartDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let input = self.ensure_input(window, cx);
        let location = format!(
            "{} · {}",
            self.draft.connection_name,
            self.draft
                .database
                .clone()
                .filter(|db| !db.is_empty())
                .unwrap_or_else(|| {
                    crate::localization::text("results.saved_charts.save_dialog.default_database")
                })
        );
        Dialog::new("save-chart-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "results.saved_charts.save_dialog.title",
            ))
            .easy_close(true)
            .on_close(|_this, _window, cx| cx.emit(SaveChartDialogEvent::Closed))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "results.saved_charts.save_dialog.explanation",
                    )),
            )
            .child(Input::new(&input))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "results.saved_charts.save_dialog.run_location",
                                &[("connection_location", &location)],
                            )),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("save-chart-refresh")
                            .ghost()
                            .xsmall()
                            .label(refresh_label(self.draft.refresh_secs))
                            .on_click(cx.listener(|this, _ev, _w, cx| this.cycle_refresh(cx))),
                    ),
            )
            .children_body(self.error.clone().map(|error| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::error())
                    .child(error)
            }))
            .button(
                Button::new("save-chart-save")
                    .primary()
                    .label(crate::localization::text(
                        "results.saved_charts.save_dialog.save_button",
                    ))
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        let name = this
                            .input
                            .as_ref()
                            .map(|input| input.read(cx).value().to_string())
                            .unwrap_or_default();
                        this.save(name, cx);
                    })),
            )
            .build(self, window, cx)
    }
}

// ── Library panel ───────────────────────────────────────────────────────────

pub enum SavedChartsPanelEvent {
    /// Run the chart's SQL on its saved connection and open the Chart tab.
    Open(Box<SavedChart>),
    /// Open the Chart Dashboard.
    OpenDashboard,
    Status {
        level: StatusLevel,
        message: String,
    },
    Closed,
}

pub struct SavedChartsPanel {
    store: SavedChartStore,
    charts: Vec<SavedChart>,
    selected: Option<usize>,
    rename_input: Entity<InputState>,
    error: Option<String>,
    frame: DialogFrame,
    _subscription: gpui::Subscription,
}

impl EventEmitter<SavedChartsPanelEvent> for SavedChartsPanel {}

impl SavedChartsPanel {
    pub fn new(store: SavedChartStore, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let charts = store.list();
        let error = store.load_error();
        let rename_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.saved_charts.library.rename_placeholder",
            ))
        });
        let subscription = cx.subscribe(
            &rename_input,
            |this: &mut Self, state, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let value = state.read(cx).value().to_string();
                    this.apply_rename(value, cx);
                }
            },
        );
        Self {
            store,
            charts,
            selected: None,
            rename_input,
            error,
            frame: DialogFrame::new(Theme::scaled(680.0), Theme::scaled(460.0)),
            _subscription: subscription,
        }
    }

    /// Re-read the library (another surface — the save prompt, the
    /// dashboard — may have changed it).
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.charts = self.store.list();
        self.error = self.store.load_error();
        if self
            .selected
            .is_some_and(|selected| selected >= self.charts.len())
        {
            self.selected = None;
        }
        cx.notify();
    }

    fn selected_chart(&self) -> Option<&SavedChart> {
        self.selected.and_then(|i| self.charts.get(i))
    }

    fn apply_rename(&mut self, value: String, cx: &mut Context<Self>) {
        let Some(chart) = self.selected_chart() else {
            return;
        };
        let id = chart.id.clone();
        match self.store.rename(&id, &value) {
            Ok(()) => {
                self.error = None;
                self.charts = self.store.list();
                self.selected = self.charts.iter().position(|chart| chart.id == id);
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn open_selected(&mut self, cx: &mut Context<Self>) {
        let Some(chart) = self.selected_chart().cloned() else {
            return;
        };
        cx.emit(SavedChartsPanelEvent::Open(Box::new(chart)));
        cx.emit(SavedChartsPanelEvent::Closed);
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some(chart) = self.selected_chart() else {
            return;
        };
        let id = chart.id.clone();
        match self.store.delete(&id) {
            Ok(()) => {
                self.error = None;
                self.selected = None;
                self.charts = self.store.list();
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn duplicate_selected(&mut self, cx: &mut Context<Self>) {
        let Some(chart) = self.selected_chart() else {
            return;
        };
        let id = chart.id.clone();
        match self.store.duplicate(&id) {
            Ok(copy) => {
                self.error = None;
                self.charts = self.store.list();
                self.selected = self.charts.iter().position(|chart| chart.id == copy.id);
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn copy_selected_sql(&mut self, cx: &mut Context<Self>) {
        let Some(chart) = self.selected_chart() else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(chart.runnable_sql()));
        cx.emit(SavedChartsPanelEvent::Status {
            level: StatusLevel::Info,
            message: crate::localization::format(
                "results.saved_charts.library.sql_copied",
                &[("chart_name", &chart.name)],
            ),
        });
    }

    fn cycle_selected_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(chart) = self.selected_chart() else {
            return;
        };
        let id = chart.id.clone();
        let next = next_refresh_choice(chart.refresh_secs);
        match self.store.set_refresh(&id, next) {
            Ok(()) => {
                self.error = None;
                self.charts = self.store.list();
                self.selected = self.charts.iter().position(|chart| chart.id == id);
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(index);
        if let Some(chart) = self.charts.get(index) {
            let name = chart.name.clone();
            self.rename_input
                .update(cx, |state, cx| state.set_value(name, window, cx));
        }
        cx.notify();
    }
}

impl Render for SavedChartsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let has_selection = self.selected.is_some();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let selected_refresh = self.selected_chart().map(|chart| chart.refresh_secs);

        let mut list = div()
            .id("saved-charts-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col();
        if self.charts.is_empty() {
            list = list.child(div().p(px(s(12.0))).text_color(Theme::text_dim()).child(
                crate::localization::text("results.saved_charts.library.empty_state"),
            ));
        }
        for (i, chart) in self.charts.iter().enumerate() {
            let selected = self.selected == Some(i);
            let last_run = chart
                .last_run_unix_ms
                .map(|ms| {
                    crate::localization::format(
                        "results.saved_charts.library.last_run",
                        &[("relative_age", &relative_age(ms, now_ms))],
                    )
                })
                .unwrap_or_else(|| {
                    crate::localization::text("results.saved_charts.library.never_run")
                });
            let subtitle = format!(
                "{} · {} · {}",
                chart.location_label(),
                last_run,
                refresh_label(chart.refresh_secs)
            );
            let preview: String = chart
                .sql
                .lines()
                .find(|line| !line.trim().is_empty() && !line.trim_start().starts_with("--"))
                .unwrap_or("")
                .chars()
                .take(90)
                .collect();
            list = list.child(
                div()
                    .id(("saved-chart-row", i))
                    .px(px(s(12.0)))
                    .py(px(s(6.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .cursor_pointer()
                    .when(selected, |el| el.bg(Theme::row_hover()))
                    .hover(|style| style.bg(Theme::row_hover()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, window, cx| this.select(i, window, cx)),
                    )
                    .child(div().text_color(Theme::text()).child(chart.name.clone()))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.78)))
                            .text_color(Theme::text_dim())
                            .child(subtitle),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.78)))
                            .text_color(Theme::text_dim())
                            .truncate()
                            .font_family(Theme::editor_font_family())
                            .child(preview),
                    ),
            );
        }

        Dialog::new("saved-charts-panel", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "results.saved_charts.library.title",
            ))
            .easy_close(true)
            .bare_body()
            .on_close(|_this, _window, cx| cx.emit(SavedChartsPanelEvent::Closed))
            .child(list)
            .children_body(self.error.clone().map(|error| {
                div()
                    .px(px(s(12.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::error())
                    .child(error)
            }))
            .leading_button(
                div()
                    .w(px(s(180.0)))
                    .child(Input::new(&self.rename_input).small()),
            )
            .leading_button(
                Button::new("saved-chart-rename")
                    .label(crate::localization::text(
                        "results.saved_charts.library.rename_button",
                    ))
                    .ghost()
                    .xsmall()
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        let value = this.rename_input.read(cx).value().to_string();
                        this.apply_rename(value, cx);
                    })),
            )
            .leading_button(
                Button::new("saved-chart-refresh-interval")
                    .label(selected_refresh.map(refresh_label).unwrap_or_else(|| {
                        crate::localization::text("results.saved_charts.library.refresh_button")
                    }))
                    .ghost()
                    .xsmall()
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _ev, _w, cx| this.cycle_selected_refresh(cx))),
            )
            .button(
                Button::new("saved-chart-dashboard")
                    .label(crate::localization::text(
                        "results.saved_charts.library.dashboard_button",
                    ))
                    .ghost()
                    .xsmall()
                    .on_click(cx.listener(|_this, _ev, _w, cx| {
                        cx.emit(SavedChartsPanelEvent::OpenDashboard);
                        cx.emit(SavedChartsPanelEvent::Closed);
                    })),
            )
            .button(
                Button::new("saved-chart-copy-sql")
                    .label(crate::localization::text(
                        "results.saved_charts.library.copy_sql_button",
                    ))
                    .ghost()
                    .xsmall()
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _ev, _w, cx| this.copy_selected_sql(cx))),
            )
            .button(
                Button::new("saved-chart-duplicate")
                    .label(crate::localization::text(
                        "results.saved_charts.library.duplicate_button",
                    ))
                    .ghost()
                    .xsmall()
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _ev, _w, cx| this.duplicate_selected(cx))),
            )
            .button(
                Button::new("saved-chart-delete")
                    .label(crate::localization::text(
                        "results.saved_charts.library.delete_button",
                    ))
                    .ghost()
                    .xsmall()
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _ev, _w, cx| this.delete_selected(cx))),
            )
            .button(
                Button::new("saved-chart-open")
                    .label(crate::localization::text(
                        "results.saved_charts.library.open_button",
                    ))
                    .xsmall()
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _ev, _w, cx| this.open_selected(cx))),
            )
            .build(self, window, cx)
    }
}
