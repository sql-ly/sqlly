use super::*;
use crate::tooltip::AppTooltip;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::popover::Popover;
use gpui_component::select::Select;

/// Unscaled width of each query-bar connection picker (icon lives inside the select).
///
/// Fixed so a long server or database name truncates inside the control instead of stretching the bar and shifting Run, toggles, and the rest. The Select still sizes to its title unless the wrapper both clamps width and clips overflow.
pub(super) const CONNECTION_PICKER_WIDTH: f32 = 200.0;

/// Unscaled left inset of a Small select (`Size::Small::input_px`).
const CONNECTION_PICKER_INPUT_PX: f32 = 8.0;

/// Unscaled gap between the kind icon and the selected title inside the field.
const CONNECTION_PICKER_ICON_GAP: f32 = 4.0;

/// Left padding that leaves room for the kind icon inside the select.
fn connection_picker_title_pl() -> f32 {
    Theme::scaled(CONNECTION_PICKER_INPUT_PX)
        + Theme::toolbar_icon_size()
        + Theme::scaled(CONNECTION_PICKER_ICON_GAP)
}

/// Where a toolbar button's icon comes from.
///
/// Most are Lucide icons out of gpui-component; a few ideas that library has no
/// word for — capturing an execution plan, stacking result grids, procedure
/// mode — are SQLLY's own drawn glyphs, addressed by asset path (see
/// [`crate::icons`]). Both draw at whatever size the caller asks for, which is
/// the only property the toolbar cares about.
#[derive(Clone, Copy)]
pub struct ToolbarIcon(pub crate::icon_catalog::AppIcon);

impl ToolbarIcon {
    /// The icon at `size` px, ready to drop into a button. Takes its color from
    /// the enclosing element's text color, like any other icon.
    pub(super) fn render(self, size: f32) -> Icon {
        Icon::empty().path(self.0.path()).size(px(size))
    }

    /// The asset path this icon resolves to — its identity, for tests that care
    /// that two buttons do not wear the same picture.
    #[cfg(test)]
    pub(super) fn asset_path(self) -> String {
        self.0.path().to_string()
    }
}

/// Connection picker: kind glyph inside the select, truncated value, chevron.
///
/// gpui-component's Select has no prefix slot, so the glyph is painted over the
/// field's left padding. It has no element id and no mouse handlers, so clicks
/// fall through to the select. The tooltip lives on the cluster.
fn connection_select_cluster(
    icon: AppIcon,
    tooltip: &'static str,
    icon_id: &'static str,
    select: impl Styled + IntoElement,
    dimmed: bool,
) -> impl IntoElement {
    // Do not `overflow_hidden` this cluster: Select renders its popup as a
    // sibling of the field, and a clipping ancestor would eat the menu.
    div()
        .id(icon_id)
        .w(px(Theme::scaled(CONNECTION_PICKER_WIDTH)))
        .flex_shrink_0()
        .relative()
        .app_tooltip(move |_window, cx| tooltip_view(crate::localization::text(tooltip), cx))
        .child(select.pl(px(connection_picker_title_pl())).w_full())
        .child(
            div()
                .absolute()
                .left(px(Theme::scaled(CONNECTION_PICKER_INPUT_PX)))
                .top_0()
                .bottom_0()
                .flex()
                .items_center()
                .when(dimmed, |d| d.opacity(0.5))
                .child(
                    Icon::empty()
                        .path(icon.path())
                        .size(px(Theme::toolbar_icon_size()))
                        .text_color(Theme::text_dim()),
                ),
        )
}

/// Identifies each square toggle in the query toggle bar (parity with the
/// Swift `makeQueryToggleBar`). Order matches the Swift toggle ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryToggle {
    AutoFormat,
    AiAutocomplete,
    RunCurrentStatement,
    IncludeStatistics,
    ShowEstimatedPlan,
    ShowActualPlan,
    TrackQueryDifference,
    QueryResume,
    ProcedureMode,
}

impl QueryToggle {
    /// All query toggles: the Swift bar order, plus Procedure mode.
    pub fn all() -> [QueryToggle; 9] {
        [
            QueryToggle::AutoFormat,
            QueryToggle::AiAutocomplete,
            QueryToggle::RunCurrentStatement,
            QueryToggle::IncludeStatistics,
            QueryToggle::ShowEstimatedPlan,
            QueryToggle::ShowActualPlan,
            QueryToggle::TrackQueryDifference,
            QueryToggle::QueryResume,
            QueryToggle::ProcedureMode,
        ]
    }

    #[allow(dead_code)]
    /// Short text icon label (this app renders text-label icons, not SF Symbols).
    pub fn label(self) -> String {
        let key = match self {
            QueryToggle::AutoFormat => "toolbar.toggle.auto_format.label",
            QueryToggle::AiAutocomplete => "toolbar.toggle.ai_autocomplete.label",
            QueryToggle::RunCurrentStatement => "toolbar.toggle.run_current.label",
            QueryToggle::IncludeStatistics => "toolbar.toggle.statistics.label",
            QueryToggle::ShowEstimatedPlan => "toolbar.toggle.estimated_plan.label",
            QueryToggle::ShowActualPlan => "toolbar.toggle.actual_plan.label",
            QueryToggle::TrackQueryDifference => "toolbar.toggle.track_difference.label",
            QueryToggle::QueryResume => "toolbar.toggle.query_resume.label",
            QueryToggle::ProcedureMode => "toolbar.toggle.procedure_mode.label",
        };
        crate::localization::text(key)
    }

    /// Icon shown inside the toggle button.
    ///
    /// Drawn icons, not Unicode glyphs. The bar used to render characters like
    /// `⇢`, `⇄` and `P`, and a font gives each of those a different size and
    /// weight inside the em box — so buttons of identical size still held icons
    /// that visibly did not match, and no amount of per-glyph fudging fixed it.
    /// Every icon here is drawn on a grid at a shared stroke weight, so one size
    /// setting produces one visual size.
    pub fn icon(self) -> ToolbarIcon {
        match self {
            QueryToggle::AutoFormat => ToolbarIcon(AppIcon::Menu),
            QueryToggle::AiAutocomplete => ToolbarIcon(AppIcon::AiAssistant),
            QueryToggle::RunCurrentStatement => ToolbarIcon(AppIcon::Play),
            QueryToggle::IncludeStatistics => ToolbarIcon(AppIcon::Statistics),
            QueryToggle::ShowEstimatedPlan => ToolbarIcon(AppIcon::PlanMap),
            QueryToggle::ShowActualPlan => ToolbarIcon(AppIcon::ShowActualPlan),
            QueryToggle::TrackQueryDifference => ToolbarIcon(AppIcon::TrackDifference),
            QueryToggle::QueryResume => ToolbarIcon(AppIcon::Resume),
            QueryToggle::ProcedureMode => ToolbarIcon(AppIcon::ProcedureMode),
        }
    }

    /// Whether flipping this toggle actually changes what the app does.
    ///
    /// The toggle set was ported from the Swift bar wholesale, including the
    /// parts Swift never wired. A toggle that renders, lights up with the
    /// accent and persists across sessions, but changes no behavior, is worse
    /// than a missing feature: the user cannot tell it from a working one, so
    /// they conclude the app — or their server — is broken. Only implemented
    /// toggles are rendered (see `render_query_toggle_bar`); the rest stay here,
    /// state and all, so wiring one up is a one-line change to this match.
    ///
    pub fn is_implemented(self) -> bool {
        match self {
            // Drives `ResultsView::set_track_difference`.
            QueryToggle::TrackQueryDifference => true,
            // Drives the per-tab resume override + metadata flush.
            QueryToggle::QueryResume => true,
            // Switches the tab between plain-script execution and the
            // deploy + invoke loop (`AppView::execute_query` proc branch).
            QueryToggle::ProcedureMode => true,
            // Writes `Tab::fim_autocomplete_enabled`, which
            // `schedule_fim_completion` consults per request (alongside the
            // global `fim_prefs.enabled`), so the toggle genuinely turns
            // inline AI ghost text on and off for the tab.
            QueryToggle::AiAutocomplete => true,
            // Read by `execute_query`: runs only the statement under the
            // caret (statement_span_at) when no selection is active.
            QueryToggle::RunCurrentStatement => true,
            // Read by `execute_query`: wraps T-SQL runs in
            // SET STATISTICS TIME, IO so the Messages pane shows timings.
            QueryToggle::IncludeStatistics => true,
            // Read by `execute_query`: formats the buffer (one undo step)
            // right before running, unless a selection picks what runs.
            QueryToggle::AutoFormat => true,
            // Neither plan toggle is a toolbar control any more: estimated
            // plans are a dedicated menu/palette action with batch-isolated
            // capture, and the actual-plan capture is the plain Explain
            // button — there is no plan *kind* mode left to toggle.
            QueryToggle::ShowEstimatedPlan | QueryToggle::ShowActualPlan => false,
        }
    }

    /// Hover tooltip text. Parity with the Swift `NSButton.toolTip` strings.
    pub fn tooltip(self) -> String {
        let key = match self {
            QueryToggle::AutoFormat => "toolbar.toggle.auto_format.tooltip",
            QueryToggle::AiAutocomplete => "toolbar.toggle.ai_autocomplete.tooltip",
            QueryToggle::RunCurrentStatement => "toolbar.toggle.run_current.tooltip",
            QueryToggle::IncludeStatistics => "toolbar.toggle.statistics.tooltip",
            QueryToggle::ShowEstimatedPlan => "toolbar.toggle.estimated_plan.tooltip",
            QueryToggle::ShowActualPlan => "toolbar.toggle.actual_plan.tooltip",
            QueryToggle::TrackQueryDifference => "toolbar.toggle.track_difference.tooltip",
            QueryToggle::QueryResume => "toolbar.toggle.query_resume.tooltip",
            QueryToggle::ProcedureMode => "toolbar.toggle.procedure_mode.tooltip",
        };
        crate::localization::text(key)
    }
}

/// Severity of the shell's status line.
///
/// The status bar carries every report the shell makes — over a hundred call
/// sites funnel into one string — so without a level a failed connection and a
/// routine confirmation paint identically. That matters more here than in most
/// apps: a missed error in a database client means running the next statement
/// believing the last one succeeded.
///
/// Deliberately explicit at each call site rather than sniffed from the message
/// text at render time (which is what `results.rs` does with
/// `starts_with("Error: ")`, and it is a bug — any message that happens to
/// start that way is colored as a failure, and any real error that doesn't is
/// not).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusLevel {
    /// Neutral progress reporting. The default.
    #[default]
    Info,
    /// Work is in flight. Motion is banned by the design system, so this is
    /// signalled with a static accent dot rather than a spinner.
    Busy,
    /// An operation completed and the user should know it worked.
    Success,
    /// The action went through, but not the way the user probably meant it to.
    Warning,
    /// Something failed. The one level that must never be missable.
    Error,
}

/// Visual state of the query toggle bar. Mirrors the Swift toggle set; like the
/// Swift app, most toggles only track state visually (no behavior wired yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryToolbarState {
    pub auto_format: bool,
    pub ai_autocomplete: bool,
    pub run_current_statement: bool,
    pub include_statistics: bool,
    pub show_estimated_plan: bool,
    pub show_actual_plan: bool,
    pub track_query_difference: bool,
    pub query_resume: bool,
    /// Procedure mode: deploy the procedure script, then invoke it with the
    /// strip's parameter values. Per-tab state lives in `tab.proc_mode`; this
    /// field mirrors the active tab's toggle for rendering.
    pub procedure_mode: bool,
    /// `resultLayoutToggle` ("Stack result grids") — separate trailing button.
    pub result_layout_stacked: bool,
}

impl Default for QueryToolbarState {
    fn default() -> Self {
        Self {
            auto_format: false,
            ai_autocomplete: false,
            run_current_statement: false,
            include_statistics: false,
            // Plan kind is selected by separate Actual and Estimated actions,
            // not by these retired toolbar-state flags.
            show_estimated_plan: false,
            show_actual_plan: true,
            track_query_difference: false,
            query_resume: false,
            procedure_mode: false,
            result_layout_stacked: false,
        }
    }
}

impl QueryToolbarState {
    pub fn get(&self, t: QueryToggle) -> bool {
        match t {
            QueryToggle::AutoFormat => self.auto_format,
            QueryToggle::AiAutocomplete => self.ai_autocomplete,
            QueryToggle::RunCurrentStatement => self.run_current_statement,
            QueryToggle::IncludeStatistics => self.include_statistics,
            QueryToggle::ShowEstimatedPlan => self.show_estimated_plan,
            QueryToggle::ShowActualPlan => self.show_actual_plan,
            QueryToggle::TrackQueryDifference => self.track_query_difference,
            QueryToggle::QueryResume => self.query_resume,
            QueryToggle::ProcedureMode => self.procedure_mode,
        }
    }

    pub fn set(&mut self, t: QueryToggle, val: bool) {
        match t {
            QueryToggle::AutoFormat => self.auto_format = val,
            QueryToggle::AiAutocomplete => self.ai_autocomplete = val,
            QueryToggle::RunCurrentStatement => self.run_current_statement = val,
            QueryToggle::IncludeStatistics => self.include_statistics = val,
            QueryToggle::ShowEstimatedPlan => self.show_estimated_plan = val,
            QueryToggle::ShowActualPlan => self.show_actual_plan = val,
            QueryToggle::TrackQueryDifference => self.track_query_difference = val,
            QueryToggle::QueryResume => self.query_resume = val,
            QueryToggle::ProcedureMode => self.procedure_mode = val,
        }
    }

    pub fn toggle(&mut self, t: QueryToggle) {
        let v = self.get(t);
        self.set(t, !v);
    }
}

impl AppView {
    /// Sync the toolbar toggle + diff/FIM toggles to reflect the active tab's
    /// per-tab state. Called on tab switch. Mirrors the per-tab toolbar
    /// sync the Swift app does in `selectDocument(_:)`.
    pub(super) fn sync_toolbar_to_active_tab(&mut self) {
        if let Some(tab) = self.tabs.get(self.active_tab) {
            self.toolbar.query_resume = tab.resume_effective_enabled;
            self.toolbar.track_query_difference = tab.diff_mode_enabled;
            self.toolbar.ai_autocomplete = tab.fim_autocomplete_enabled;
            self.toolbar.procedure_mode = tab.proc_mode.enabled;
            self.toolbar.result_layout_stacked =
                tab.per_tab_result_layout.as_deref() == Some("stacked");
        }
    }
    /// The workbench's primary action, and the only accent-*filled* control on
    /// screen: running the active query. F5/⌘↩ has always worked, but until now
    /// nothing in the shell said so — the accent was spread across toggles and
    /// indicators while the app's whole reason to exist had no affordance.
    ///
    /// While a query is in flight this becomes Cancel, because that is then the
    /// primary action. Deliberately stays enabled while disconnected:
    /// `execute_query` is also the entry point for the resume-connection
    /// prompt, which only fires when there is no active profile.
    pub(super) fn render_run_button(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let running = self
            .tabs
            .get(self.active_tab)
            .is_some_and(|t| t.active_query_id.is_some());
        let (text, tip): (SharedString, SharedString) = if running {
            (
                crate::localization::text("toolbar.run.cancel_label").into(),
                crate::localization::text("toolbar.run.cancel_tooltip").into(),
            )
        } else if let Some(tab) = self.tabs.get(self.active_tab) {
            let editor = tab.editor.read(cx);
            let (label, tip) = crate::app::execute::run_label(crate::app::execute::RunLabelInput {
                running: false,
                proc_mode: self.toolbar.procedure_mode,
                has_selection: editor.has_selection(),
                run_current_statement: self.toolbar.run_current_statement,
                statement_count: editor.statement_count(),
                file_backed: tab.is_file_backed(),
            });
            (label.into(), tip.into())
        } else {
            (
                crate::localization::text("toolbar.run.default_label").into(),
                crate::localization::text("toolbar.run.default_tooltip").into(),
            )
        };
        div()
            .id("qbar-run")
            .h(px(Theme::control_height()))
            .min_w(px(Theme::scaled(64.0)))
            .px(px(Theme::scaled(12.0)))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .gap(px(Theme::scaled(2.0)))
            .rounded(px(Theme::scaled(4.0)))
            .cursor_pointer()
            .text_size(px(Theme::font_size()))
            .border_1()
            // The glyph rides with "Run" only. On the cancel state it would be
            // a picture of the thing the button no longer does. The play mark
            // keeps the inline size but carries a heavier stroke (see the
            // manifest) so it reads boldly next to the label.
            .when(!running, |d| {
                d.child(
                    // Nudge the play mark up a hair so it sits optically centred
                    // with the label rather than riding slightly low.
                    div()
                        .mt(px(Theme::scaled(-1.0)))
                        .child(ToolbarIcon(AppIcon::Play).render(Theme::inline_icon_size())),
                )
            })
            .when(!running, |d| {
                d.bg(Theme::accent())
                    .border_color(Theme::accent())
                    .text_color(Theme::bg())
            })
            .when(running, |d| {
                d.bg(Theme::panel_bg())
                    .border_color(Theme::border())
                    .text_color(Theme::error())
            })
            .child(text)
            .app_tooltip({
                let tip = tip.clone();
                move |_window, cx| tooltip_view(tip.clone(), cx)
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _ev, _window, cx| {
                    if running {
                        this.cancel_query_no_window(cx);
                    } else {
                        this.execute_query(this.active_tab, cx);
                    }
                }),
            )
    }

    pub(super) fn render_transaction_controls(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let tab = self.tabs.get(self.active_tab);
        let mode = tab
            .map(|t| t.transaction_mode)
            .unwrap_or(crate::tabs::TransactionMode::Auto);
        let manual_on = mode == crate::tabs::TransactionMode::Manual;
        let manual_tx = tab.and_then(|t| t.manual_tx.as_ref());
        let lost = manual_tx.is_some_and(|tx| tx.lost);
        let dirty = manual_tx.is_some_and(|tx| tx.dirty());
        let availability = self.manual_mode_availability(self.active_tab);
        let unavailable = availability.reason();
        let badge_tip: SharedString = if lost {
            crate::localization::text("toolbar.transaction.lost_tooltip").into()
        } else if let Some(reason) = unavailable.clone() {
            reason.into()
        } else if manual_on {
            crate::localization::text("toolbar.transaction.manual_tooltip").into()
        } else {
            crate::localization::text("toolbar.transaction.auto_tooltip").into()
        };
        let badge_label = if manual_on { "M" } else { "A" };
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::scaled(4.0)))
            .child(
                div()
                    .id("qbar-tx-mode")
                    .h(px(Theme::control_height()))
                    .w(px(Theme::control_height()))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .rounded(px(Theme::scaled(4.0)))
                    .when(unavailable.is_none(), |d| {
                        d.cursor_pointer()
                            .when(manual_on, crate::theme::accent_pill_style)
                    })
                    .when(unavailable.is_some(), |d| {
                        d.text_color(Theme::text_dim()).opacity(0.4)
                    })
                    .when(!manual_on && unavailable.is_none(), |d| {
                        d.text_color(Theme::text_dim())
                    })
                    .child(badge_label)
                    .app_tooltip(move |_window, cx| tooltip_view(badge_tip.clone(), cx))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _ev, _window, cx| {
                            if unavailable.is_none() {
                                this.toggle_transaction_mode(this.active_tab, cx);
                            }
                        }),
                    ),
            );
        if dirty {
            row = row.child(
                div()
                    .w(px(Theme::scaled(6.0)))
                    .h(px(Theme::scaled(6.0)))
                    .rounded_full()
                    .bg(Theme::warning()),
            );
        }
        if manual_on && manual_tx.is_some() && !lost {
            row = row
                .child(
                    div()
                        .id("qbar-tx-commit")
                        .h(px(Theme::control_height()))
                        .px(px(Theme::scaled(8.0)))
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .rounded(px(Theme::scaled(4.0)))
                        .cursor_pointer()
                        .border_1()
                        .border_color(Theme::border())
                        .bg(Theme::panel_bg())
                        .text_color(Theme::text())
                        .child(crate::localization::text(
                            "toolbar.transaction.commit_label",
                        ))
                        .app_tooltip(|_window, cx| {
                            tooltip_view(
                                crate::localization::text("toolbar.transaction.commit_tooltip"),
                                cx,
                            )
                        })
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _ev, _window, cx| {
                                this.commit_transaction(this.active_tab, cx);
                            }),
                        ),
                )
                .child(
                    div()
                        .id("qbar-tx-rollback")
                        .h(px(Theme::control_height()))
                        .px(px(Theme::scaled(8.0)))
                        .flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_center()
                        .rounded(px(Theme::scaled(4.0)))
                        .cursor_pointer()
                        .border_1()
                        .border_color(Theme::border())
                        .bg(Theme::panel_bg())
                        .text_color(Theme::text())
                        .child(crate::localization::text(
                            "toolbar.transaction.rollback_label",
                        ))
                        .app_tooltip(|_window, cx| {
                            tooltip_view(
                                crate::localization::text("toolbar.transaction.rollback_tooltip"),
                                cx,
                            )
                        })
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _ev, _window, cx| {
                                this.rollback_transaction(this.active_tab, cx);
                            }),
                        ),
                );
        }
        row
    }

    /// Edit the connection the query bar is currently pointed at, without a
    /// detour through the explorer's context menu.
    ///
    /// Sits with the server and database pickers because it edits the same
    /// thing they select: the settings that decide what this connection is
    /// allowed to do. Disabled (not hidden) while nothing is connected, so the
    /// affordance is discoverable before it is usable.
    pub(super) fn render_edit_connection_button(
        &self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let profile_id = self.active_profile.as_ref().map(|p| p.id.0);
        let tip = match self.active_profile.as_ref() {
            Some(profile) => crate::localization::format(
                "toolbar.connection.edit_tooltip",
                &[("name", &profile.name)],
            ),
            None => crate::localization::text("toolbar.connection.edit_disconnected_tooltip"),
        };
        div()
            .id("qbar-edit-connection")
            .w(px(Theme::control_height()))
            .h(px(Theme::control_height()))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .rounded(px(Theme::scaled(4.0)))
            .when(profile_id.is_some(), |d| {
                d.cursor_pointer()
                    .text_color(Theme::text_dim())
                    .hover(|s| s.bg(Theme::border()))
            })
            .when(profile_id.is_none(), |d| {
                d.text_color(Theme::text_dim()).opacity(0.4)
            })
            .child(ToolbarIcon(AppIcon::Settings).render(Theme::toolbar_icon_size()))
            .app_tooltip(move |_window, cx| tooltip_view(tip.clone(), cx))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _ev, window, cx| {
                    if let Some(id) = profile_id {
                        this.open_connection_editor(id, window, cx);
                    }
                }),
            )
    }
    /// Capture the current query's execution plan into the results-area plan
    /// pane (⌘L).
    ///
    /// One icon button in the toggle cluster, not a labelled button beside Run:
    /// the text "Explain" and a plan-kind toggle used to be two controls for one
    /// idea. This button is the explicit Actual action; Estimated remains a
    /// separate menu/palette action. It stays visible but disabled with the
    /// reason when the connection cannot produce a plan, so the affordance is
    /// discoverable before it is usable.
    pub(super) fn render_plan_button(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let unavailable =
            self.execution_plan_unavailable_reason(crate::execution_plan_view::PlanKind::Actual);
        let summary = self
            .active_profile
            .as_ref()
            .and_then(|profile| sqlly_services::plan_dialect(profile.database_type))
            .map(|dialect| {
                crate::explain_options_popover::options_summary(
                    &self.effective_explain_options(
                        self.active_profile.as_ref().expect("checked"),
                        crate::execution_plan_view::PlanKind::Actual,
                    ),
                    dialect,
                )
            })
            .unwrap_or_default();
        let tip = match &unavailable {
            Some(reason) => reason.clone(),
            None if summary.is_empty() => crate::localization::text("toolbar.plan.capture_tooltip"),
            None => crate::localization::format(
                "toolbar.plan.capture_with_options_tooltip",
                &[("summary", &summary)],
            ),
        };
        let host = cx.entity();
        let chevron = Button::new("qbar-plan-options-trigger")
            .ghost()
            .small()
            .child(ToolbarIcon(AppIcon::ChevronDown).render(Theme::scaled(10.0)));
        let popover = Popover::new("qbar-plan-options")
            .anchor(gpui::Anchor::TopLeft)
            .trigger(chevron)
            .on_open_change({
                let host = host.clone();
                move |open, _window, cx| {
                    if *open {
                        host.update(cx, |this, cx| {
                            if let Some(profile) = this.active_profile.clone() {
                                this.ensure_server_version(&profile, cx);
                            }
                        });
                    }
                }
            })
            .content({
                let host = host.clone();
                move |_state, _window, cx| {
                    let snapshot = host.read(cx).explain_popover_snapshot();
                    crate::explain_options_popover::render_explain_options(snapshot, host.clone())
                }
            });
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .items_center()
            .child(
                div()
                    .id("qbar-plan")
                    .w(px(Theme::control_height()))
                    .h(px(Theme::control_height()))
                    .flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_center()
                    .rounded(px(Theme::scaled(4.0)))
                    .when(unavailable.is_none(), |d| {
                        d.cursor_pointer()
                            .text_color(Theme::text_dim())
                            .hover(|s| s.bg(Theme::border()))
                    })
                    .when(unavailable.is_some(), |d| {
                        d.text_color(Theme::text_dim()).opacity(0.4)
                    })
                    .child(ToolbarIcon(AppIcon::ShowActualPlan).render(Theme::toolbar_icon_size()))
                    .app_tooltip(move |_window, cx| tooltip_view(tip.clone(), cx))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _ev, _window, cx| {
                            if unavailable.is_none() {
                                this.explain_active_query(cx);
                            }
                        }),
                    ),
            )
            .child(
                div()
                    .id("qbar-plan-options")
                    .w(px(Theme::scaled(14.0)))
                    .h(px(Theme::control_height()))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(popover),
            )
    }

    /// A single square toggle button matching the Swift `applyToggleAppearance`:
    /// accent text + accent@15% background + rounded corners when on; dim text
    /// when off. For the resume toggle, the button is disabled when scope=None
    /// and the tooltip is dynamic (shows scope/override/effective state).
    pub(super) fn render_toggle_button(
        &self,
        t: QueryToggle,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let on = self.toolbar.get(t);
        let is_resume = t == QueryToggle::QueryResume;
        let is_proc = t == QueryToggle::ProcedureMode;
        // Detection glow: the buffer starts with CREATE/ALTER PROCEDURE but
        // the mode is off — the toggle suggests itself in accent.
        let proc_detected = is_proc
            && !on
            && self
                .tabs
                .get(self.active_tab)
                .is_some_and(|tab| tab.proc_mode.detected);
        // While a query runs, the proc toggle locks (the sequence owns the tab).
        let proc_running = is_proc
            && self
                .tabs
                .get(self.active_tab)
                .is_some_and(|tab| tab.active_query_id.is_some());
        // Engine capability gate: toggles whose behavior only exists on some
        // engines disable with a truthful reason instead of silently no-oping.
        let engine_gate = {
            let db = self.active_db_type();
            match t {
                QueryToggle::ProcedureMode => crate::capabilities::procedure_mode(db).reason(),
                QueryToggle::IncludeStatistics => {
                    crate::capabilities::include_statistics(db).reason()
                }
                _ => None,
            }
        };
        let is_disabled = (is_resume
            && self.query_resume_prefs.scope == crate::preferences::QueryResumeScope::None)
            || proc_running
            || engine_gate.is_some();
        let tooltip_text: SharedString = if let Some(reason) = engine_gate {
            reason.into()
        } else if is_resume {
            self.resume_toggle_tooltip().into()
        } else if proc_detected {
            crate::localization::text("toolbar.toggle.procedure_detected_tooltip").into()
        } else {
            t.tooltip().into()
        };
        // One box, one glyph size, shared with every other icon button in the
        // strip and with Run/Explain's height (`Theme::toolbar_icon_size`).
        let icon_size = Theme::toolbar_icon_size();
        div()
            .id(("qtoggle", t as usize))
            .w(px(Theme::control_height()))
            .h(px(Theme::control_height()))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Theme::scaled(4.0)))
            .when(!is_disabled, |d| d.cursor_pointer())
            .when(is_disabled, |d| d.cursor_default())
            .child(t.icon().render(icon_size))
            .when(on && !is_disabled, crate::theme::accent_pill_style)
            .when(!on && !is_disabled && !proc_detected, |d| {
                d.text_color(Theme::text_dim())
                    .hover(|s| s.bg(Theme::border()))
            })
            .when(proc_detected && !is_disabled, |d| {
                d.border_1()
                    .border_color(Theme::accent())
                    .text_color(Theme::accent())
                    .hover(|s| s.bg(Theme::border()))
            })
            .when(is_disabled, |d| {
                d.text_color(Theme::text_dim()).opacity(0.4)
            })
            .app_tooltip(move |_window, cx| tooltip_view(tooltip_text.clone(), cx))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _ev, _window, cx| {
                    if is_disabled {
                        return;
                    }
                    if is_resume {
                        this.toggle_resume_override(cx);
                        return;
                    }
                    if is_proc {
                        this.toggle_proc_mode(cx);
                        return;
                    }
                    this.toolbar.toggle(t);
                    if t == QueryToggle::TrackQueryDifference
                        && let Some(tab) = this.tabs.get(this.active_tab)
                    {
                        let val = this.toolbar.track_query_difference;
                        tab.results.update(cx, |r, cx| {
                            r.set_track_difference(val, cx);
                        });
                        // Persist per-tab diff state when resume is ON.
                        if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                            tab.diff_mode_enabled = val;
                            if tab.resume_effective_enabled {
                                this.flush_tab_resume_metadata(this.active_tab);
                            }
                        }
                    }
                    if t == QueryToggle::AiAutocomplete {
                        // Persist per-tab FIM state when resume is ON.
                        let val = this.toolbar.ai_autocomplete;
                        if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                            tab.fim_autocomplete_enabled = val;
                            if tab.resume_effective_enabled {
                                this.flush_tab_resume_metadata(this.active_tab);
                            }
                        }
                        if !val {
                            // Off: drop the pending debounce, invalidate any
                            // in-flight request, and clear visible ghost text.
                            this.fim_debounce_task = None;
                            this.fim_request_token += 1;
                            if let Some(tab) = this.tabs.get(this.active_tab) {
                                tab.editor.update(cx, |e, cx| e.hide_fim_suggestion(cx));
                            }
                        }
                    }
                    cx.notify();
                }),
            )
    }

    /// Server and database Selects: kind icon inside the field, truncated name, chevron.
    ///
    /// Each cluster is `CONNECTION_PICKER_WIDTH` so a long name clips instead of shoving the rest of the command bar sideways.
    pub(super) fn render_qbar_connection_pickers(&self) -> impl IntoElement + use<> {
        // `.small()` sets the compact text and padding; the height comes from
        // `Theme::control_height()` so the pickers line up with Run and Explain
        // rather than sitting a few pixels short of them. gpui-component's own
        // size scale only offers fixed 24/32px steps and applies the height
        // before its style refinement, so overriding it here is what wins.
        let connected = self.active_profile.is_some();
        let has_database = self
            .active_profile
            .as_ref()
            .and_then(|p| p.database.as_deref())
            .is_some_and(|db| !db.is_empty());
        let server_select = Select::new(&self.qbar_server_select)
            .placeholder(crate::localization::text(
                "toolbar.connection.server_placeholder",
            ))
            .small()
            .h(px(Theme::control_height()))
            .w_full();
        let database_select = Select::new(&self.qbar_database_select)
            .placeholder(if connected && !has_database {
                crate::localization::text("toolbar.connection.choose_database_placeholder")
            } else {
                crate::localization::text("toolbar.connection.database_placeholder")
            })
            .small()
            .h(px(Theme::control_height()))
            .w_full()
            .disabled(!connected)
            .empty(|_, _| {
                div()
                    .px(px(Theme::scaled(8.0)))
                    .py(px(Theme::scaled(4.0)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("toolbar.connection.no_databases"))
            });
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::scaled(6.0)))
            .child(connection_select_cluster(
                AppIcon::Server,
                "toolbar.connection.server_tooltip",
                "qbar-server-icon",
                server_select,
                false,
            ))
            .child(connection_select_cluster(
                AppIcon::Database,
                "toolbar.connection.database_tooltip",
                "qbar-database-icon",
                database_select,
                !connected,
            ))
    }

    /// Legacy bordered toggle bar (pre–Phase 8). The live workbench uses the
    /// command bar + connection chip instead; this body is uncalled.
    #[allow(dead_code)]
    pub(super) fn render_query_toggle_bar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let layout_on = self.toolbar.result_layout_stacked;

        let server_names: Vec<String> = self
            .sidebar
            .read(cx)
            .connections()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let server_selected = self
            .active_profile
            .as_ref()
            .map(|p| p.name.clone())
            .filter(|s| !s.is_empty());

        let mut db_names = self.sidebar.update(cx, |s, cx| s.database_names(cx));
        // Ensure the currently-selected database is always present as an option
        // so the dropdown displays it even when the server's database list has
        // not been loaded yet (e.g. restored from a prior session without
        // reconnecting).
        if let Some(active_db) = self
            .active_profile
            .as_ref()
            .and_then(|p| p.database.clone())
            .filter(|s| !s.is_empty())
            && !db_names.contains(&active_db)
        {
            db_names.insert(0, active_db);
        }
        let database_selected = self
            .active_profile
            .as_ref()
            .and_then(|p| p.database.clone())
            .filter(|s| !s.is_empty());

        // When the current (server, database) data model was last pulled, for
        // the refresh button's tooltip. Copyable, so the tooltip closure can
        // own it and recompute the "how long ago" fresh on each hover.
        let datamodel_fetched_at = self
            .active_profile
            .as_ref()
            .map(Self::datamodel_key)
            .and_then(|key| {
                let fetched_at = self.datamodel_cache.get(&key).map(|c| c.fetched_at)?;
                self.touch_datamodel_cache(&key);
                Some(fetched_at)
            });

        // Sync items + selection into the dropdown state entities,
        // change-detected against the app-side caches so steady-state frames
        // leave the entities untouched (and no notify loops can form).
        let server_items_changed = self.qbar_server_items != server_names;
        if server_items_changed {
            self.qbar_server_items = server_names.clone();
            let items = server_names;
            self.qbar_server_select
                .update(cx, |state, cx| state.set_items(items, window, cx));
        }
        if select_needs_selection_write(
            server_items_changed,
            self.qbar_server_selected.as_deref(),
            server_selected.as_deref(),
        ) {
            self.qbar_server_selected = server_selected.clone();
            self.qbar_server_select
                .update(cx, |state, cx| match &server_selected {
                    Some(value) => state.set_selected_value(value, window, cx),
                    None => state.set_selected_index(None, window, cx),
                });
        }
        let database_items_changed = self.qbar_database_items != db_names;
        if database_items_changed {
            self.qbar_database_items = db_names.clone();
            let items = db_names;
            self.qbar_database_select
                .update(cx, |state, cx| state.set_items(items, window, cx));
        }
        if select_needs_selection_write(
            database_items_changed,
            self.qbar_database_selected.as_deref(),
            database_selected.as_deref(),
        ) {
            self.qbar_database_selected = database_selected.clone();
            self.qbar_database_select
                .update(cx, |state, cx| match &database_selected {
                    Some(value) => state.set_selected_value(value, window, cx),
                    None => state.set_selected_index(None, window, cx),
                });
        }

        #[cfg(not(target_arch = "wasm32"))]
        let history_panel_open = self.active_tab().is_some_and(|t| t.history_panel.is_some());

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::scaled(16.0)))
            .h(px(Theme::control_height() + Theme::scaled(9.0) * 2.0))
            .px(px(Theme::scaled(8.0)))
            .bg(Theme::sidebar_bg())
            .border_b_1()
            .border_color(Theme::border())
            .child(self.render_run_button(cx))
            // Connection cluster, the 1px rule, and the query toggles ride
            // together at one tight spacing so the edit-connection gear sits
            // centred between the database picker and the rule, and the plan
            // button mirrors it on the far side (equal gap to the rule).
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    // Spacing around the two rules — tighter than the 6px group
                    // gap so the buttons flanking each rule read evenly.
                    .gap(px(Theme::scaled(5.0)))
                    // Connection cluster: server + database + edit, grouped by
                    // spacing rather than a rule.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::scaled(6.0)))
                            .child(self.render_qbar_connection_pickers())
                            .child(self.render_edit_connection_button(cx)),
                    )
                    // The one rule in the bar. Everything to its left names
                    // *which* connection the query runs against; everything to
                    // its right acts on the query itself.
                    .child(
                        div()
                            .w(px(1.0))
                            .h(px(Theme::control_height() * 0.6))
                            .flex_shrink_0()
                            .bg(Theme::border()),
                    )
                    // Query toggles, tight as a single instrument group.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::scaled(2.0)))
                            .child(self.render_plan_button(cx))
                            .children(
                                QueryToggle::all()
                                    .into_iter()
                                    .filter(|t| t.is_implemented())
                                    .map(|t| self.render_toggle_button(t, cx))
                                    .collect::<Vec<_>>(),
                            ),
                    )
                    // Second rule: separates the query-action toggles from the
                    // view/schema actions, mirroring the connection rule so the
                    // two groups read as separate at a glance.
                    .child(
                        div()
                            .w(px(1.0))
                            .h(px(Theme::control_height() * 0.6))
                            .flex_shrink_0()
                            .bg(Theme::border()),
                    )
                    // Version history, refresh, and result-layout actions in one
                    // tight strip, sharing the toggles' spacing and hover.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(Theme::scaled(2.0)))
                            // Version history panel toggle (per-tab).
                            .child({
                                #[cfg(not(target_arch = "wasm32"))]
                                {
                                    let on = history_panel_open;
                                    div()
                                        .id("qbar-history")
                                        .w(px(Theme::control_height()))
                                        .h(px(Theme::control_height()))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(Theme::scaled(4.0)))
                                        .cursor_pointer()
                                        .child(
                                            ToolbarIcon(AppIcon::History)
                                                .render(Theme::toolbar_icon_size()),
                                        )
                                        .when(on, crate::theme::accent_pill_style)
                                        .when(!on, |d| {
                                            d.text_color(Theme::text_dim())
                                                .hover(|s| s.bg(Theme::border()))
                                        })
                                        .app_tooltip(move |_window, cx| {
                                            tooltip_view(
                                                crate::localization::text(
                                                    "toolbar.history.toggle_tooltip",
                                                ),
                                                cx,
                                            )
                                        })
                                        .on_mouse_down(
                                            gpui::MouseButton::Left,
                                            cx.listener(|this, _ev, _window, cx| {
                                                let active = this.active_tab;
                                                this.toggle_history_panel(active, cx);
                                            }),
                                        )
                                        .into_any_element()
                                }
                                #[cfg(target_arch = "wasm32")]
                                {
                                    div().into_any_element()
                                }
                            })
                            .child(
                                // Refresh schema metadata — reloads connection profiles and
                                // re-fetches the active server's entire schema tree.
                                div()
                                    .id("qbar-refresh")
                                    .w(px(Theme::control_height()))
                                    .h(px(Theme::control_height()))
                                    // Pull the layout button in a touch: the database
                                    // glyph carries more right-side whitespace than its
                                    // neighbours, so trim the gap to read even, with a
                                    // matching 1px of breathing room on its left.
                                    .ml(px(Theme::scaled(1.0)))
                                    .mr(px(Theme::scaled(-1.0)))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(Theme::scaled(4.0)))
                                    .cursor_pointer()
                                    .text_color(Theme::text_dim())
                                    .hover(|s| s.bg(Theme::border()))
                                    .child(
                                        ToolbarIcon(AppIcon::RefreshSchema)
                                            .render(Theme::toolbar_icon_size()),
                                    )
                                    .app_tooltip(move |_window, cx| {
                                        let text = match datamodel_fetched_at {
                                            Some(timestamp) => crate::localization::format(
                                                "toolbar.schema.refresh_with_age_tooltip",
                                                &[("age", &format_snapshot_age(timestamp))],
                                            ),
                                            None => crate::localization::text(
                                                "toolbar.schema.refresh_no_snapshot_tooltip",
                                            ),
                                        };
                                        tooltip_view(text, cx)
                                    })
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|this, _ev, _window, cx| {
                                            let active_id =
                                                this.active_profile.as_ref().map(|p| p.id.0);
                                            this.sidebar.update(cx, |sidebar, cx| {
                                                sidebar.refresh_connections(cx);
                                                if let Some(id) = active_id {
                                                    sidebar.refresh_server_schema(id, cx);
                                                }
                                            });
                                            if let Some(profile) = this.active_profile.clone() {
                                                this.refresh_completion_catalog(profile, cx);
                                            }
                                            this.set_status(
                                                StatusLevel::Success,
                                                crate::localization::text(
                                                    "toolbar.schema.refreshed_status",
                                                ),
                                            );
                                            cx.notify();
                                        }),
                                    ),
                            )
                            .child(
                                // Result layout toggle. The icon reports the layout in
                                // force rather than the one a click would bring: a
                                // toggle whose picture is of the *other* state reads as
                                // a lie every time it is glanced at. Stacked panels when
                                // stacked, a tabbed panel when tabbed.
                                div()
                                    .id("qbar-layout")
                                    .w(px(Theme::control_height()))
                                    .h(px(Theme::control_height()))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(Theme::scaled(4.0)))
                                    .cursor_pointer()
                                    .when(layout_on, crate::theme::accent_pill_style)
                                    .when(!layout_on, |d| {
                                        d.text_color(Theme::text_dim())
                                            .hover(|s| s.bg(Theme::border()))
                                    })
                                    .child(
                                        // Both layout glyphs read small against the
                                        // rounder icons — size them up to sit optically
                                        // level. The tabs glyph also sits a touch high,
                                        // so nudge it down 1px.
                                        div()
                                            .when(!layout_on, |d| d.mt(px(Theme::scaled(1.0))))
                                            .child(
                                                ToolbarIcon(if layout_on {
                                                    AppIcon::ResultsStacked
                                                } else {
                                                    AppIcon::ResultsInTabs
                                                })
                                                .render(Theme::toolbar_icon_size() * 1.12),
                                            ),
                                    )
                                    .app_tooltip(move |_window, cx| {
                                        tooltip_view(
                                            if layout_on {
                                                crate::localization::text(
                                                    "toolbar.results.stacked_tooltip",
                                                )
                                            } else {
                                                crate::localization::text(
                                                    "toolbar.results.tabbed_tooltip",
                                                )
                                            },
                                            cx,
                                        )
                                    })
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|this, _ev, _window, cx| {
                                            this.toolbar.result_layout_stacked =
                                                !this.toolbar.result_layout_stacked;
                                            let mode = if this.toolbar.result_layout_stacked {
                                                LayoutMode::Stacked
                                            } else {
                                                LayoutMode::Tabbed
                                            };
                                            if let Some(tab) = this.tabs.get(this.active_tab) {
                                                tab.results.update(cx, |r, cx| {
                                                    r.set_layout_mode(mode, cx);
                                                });
                                            }
                                            // Persist per-tab layout when resume is ON.
                                            let layout_str = if this.toolbar.result_layout_stacked {
                                                "stacked"
                                            } else {
                                                "tabular"
                                            };
                                            if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                                                tab.per_tab_result_layout =
                                                    Some(layout_str.to_string());
                                                if tab.resume_effective_enabled {
                                                    this.flush_tab_resume_metadata(this.active_tab);
                                                }
                                            }
                                            cx.notify();
                                        }),
                                    ),
                            ),
                    ),
            )
            // Right-aligned spacer pushes the account control to the far edge of
            // the bar: the one control that acts on *you* rather than on the
            // query or its connection.
            .child(div().flex_1())
            .child(self.render_account_button(cx))
    }
}

/// Whether a query-bar Select needs its selection written this frame.
///
/// `SelectState::set_items` swaps the option list but keeps the list cursor,
/// and the trigger paints whatever item now sits at that index. So a new item
/// list always needs the selection re-applied — including an explicit clear
/// when nothing is selected. Before, the clear was skipped: the items-changed
/// branch reset the app-side cache to `None`, which then matched a `None`
/// target, so the database picker kept showing the old row's name while the
/// active profile had no database at all and queries ran in the login's
/// default database.
pub(super) fn select_needs_selection_write(
    items_changed: bool,
    cached: Option<&str>,
    wanted: Option<&str>,
) -> bool {
    items_changed || cached != wanted
}

#[cfg(test)]
#[cfg(test)]
mod toolbar_tests {
    use super::toolbar::QueryToggle;
    use super::{
        AppView, QueryToolbarState, StatusLevel, restored_tab_connection_target,
        select_needs_selection_write,
    };

    #[test]
    fn select_sync_rewrites_selection_whenever_items_change() {
        // Steady state: nothing to do.
        assert!(!select_needs_selection_write(
            false,
            Some("Sales"),
            Some("Sales")
        ));
        assert!(!select_needs_selection_write(false, None, None));
        // Target moved: write.
        assert!(select_needs_selection_write(false, Some("Sales"), None));
        assert!(select_needs_selection_write(false, None, Some("Sales")));
        // The list was replaced: the widget's cursor survived `set_items`, so
        // the selection is re-applied even when it reads the same — and the
        // regression case: cleared when the active profile has no database.
        assert!(select_needs_selection_write(
            true,
            Some("Sales"),
            Some("Sales")
        ));
        assert!(select_needs_selection_write(true, None, None));
    }

    #[test]
    fn startup_restores_existing_tab_target_without_activating_a_profile() {
        let id = uuid::Uuid::new_v4();
        let state = crate::workspace_session::TabState {
            connection_id: Some(id.to_string()),
            database: Some("Reporting".to_string()),
            ..Default::default()
        };
        let target = restored_tab_connection_target(&state, |candidate| candidate == id)
            .expect("existing target is retained as inert tab metadata");
        assert_eq!(target.profile_id, id);
        assert_eq!(target.database.as_deref(), Some("Reporting"));
    }

    #[test]
    fn startup_drops_deleted_or_invalid_tab_targets() {
        let state = crate::workspace_session::TabState {
            connection_id: Some(uuid::Uuid::new_v4().to_string()),
            ..Default::default()
        };
        assert!(restored_tab_connection_target(&state, |_| false).is_none());
        let invalid = crate::workspace_session::TabState {
            connection_id: Some("not-a-uuid".to_string()),
            ..Default::default()
        };
        assert!(restored_tab_connection_target(&invalid, |_| true).is_none());
    }

    #[test]
    fn tab_switcher_uses_requested_window_fractions() {
        let (width, height) = AppView::tab_switcher_panel_size(1_000.0, 800.0);
        assert!((width - 400.0).abs() < f32::EPSILON);
        assert!((height - 480.0).abs() < 0.001);
    }

    #[test]
    fn tab_switcher_prefers_the_generated_long_summary() {
        assert_eq!(
            AppView::tab_switcher_summary_text(Some("generated"), Some("legacy")),
            "generated"
        );
        assert_eq!(
            AppView::tab_switcher_summary_text(None, Some("legacy")),
            "legacy"
        );
    }

    #[test]
    fn tab_switcher_row_height_fits_two_identity_lines() {
        // The row is the name line plus the chip line plus padding — sized
        // from the same metrics the row renders with, so chips cannot spill
        // into the next row (the old fixed 2.2 line heights did).
        let height = AppView::tab_switcher_row_height();
        let metrics = super::tab_switcher::switcher_row_metrics(crate::theme::Theme::font_size());
        assert_eq!(height, metrics.row_h);
        assert!(height >= metrics.title_h + metrics.gap + metrics.trail_h);
    }

    #[test]
    fn default_state_captures_actual_plans_rest_off() {
        let s = QueryToolbarState::default();
        // Plan kind is no longer represented by a toolbar toggle. Estimated
        // capture remains available through its dedicated action.
        assert!(s.get(QueryToggle::ShowActualPlan));
        assert!(!s.get(QueryToggle::ShowEstimatedPlan));
        for t in QueryToggle::all() {
            if t != QueryToggle::ShowActualPlan {
                assert!(!s.get(t), "{:?} should default off", t);
            }
        }
        assert!(!s.result_layout_stacked);
    }

    #[test]
    fn toggle_flips_only_target() {
        let mut s = QueryToolbarState::default();
        let before = s.clone();
        s.toggle(QueryToggle::AiAutocomplete);
        assert!(s.get(QueryToggle::AiAutocomplete));
        // No other toggle changed.
        for t in QueryToggle::all() {
            if t != QueryToggle::AiAutocomplete {
                assert_eq!(s.get(t), before.get(t), "{:?} should be unchanged", t);
            }
        }
        // Toggling again returns to off.
        s.toggle(QueryToggle::AiAutocomplete);
        assert!(!s.get(QueryToggle::AiAutocomplete));
    }

    #[test]
    fn plan_kind_is_no_longer_a_choice() {
        // Neither plan flag is a toolbar control; Actual and Estimated are
        // separate commands, so the legacy flags do not radio-pair.
        let mut s = QueryToolbarState::default();
        s.toggle(QueryToggle::ShowActualPlan);
        assert!(!s.get(QueryToggle::ShowActualPlan));
        assert!(
            !s.get(QueryToggle::ShowEstimatedPlan),
            "the estimated command must not mutate this retired toolbar flag"
        );
        assert!(
            !QueryToggle::ShowEstimatedPlan.is_implemented()
                && !QueryToggle::ShowActualPlan.is_implemented(),
            "neither plan flag is offered as a toggle button"
        );
    }

    #[test]
    fn set_get_roundtrip_each_toggle() {
        for t in QueryToggle::all() {
            let mut s = QueryToolbarState::default();
            s.set(t, true);
            assert!(s.get(t), "{:?} set true", t);
            s.set(t, false);
            assert!(!s.get(t), "{:?} set false", t);
        }
    }

    #[test]
    fn all_returns_nine_distinct_toggles_with_labels() {
        let all = QueryToggle::all();
        assert_eq!(all.len(), 9);
        for t in all {
            assert!(!t.label().is_empty(), "{:?} needs a label", t);
        }
    }

    #[test]
    fn all_toggles_have_icon_and_tooltip() {
        for t in QueryToggle::all() {
            assert!(!t.tooltip().is_empty(), "{:?} needs a tooltip", t);
        }
        // Two toggles wearing the same icon are indistinguishable in a bar that
        // has no labels, which is the whole reason each one gets its own.
        let icons: Vec<String> = QueryToggle::all()
            .into_iter()
            .map(|t| t.icon().asset_path())
            .collect();
        let mut unique = icons.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), icons.len(), "toggle icons must be distinct");
    }

    /// The toolbar must only offer controls that do something. This is the
    /// regression guard for the placebo-toggle bug: six toggles (seven counting
    /// AI autocomplete) rendered, lit up with the accent and persisted across
    /// sessions while changing no behavior at all, which is indistinguishable
    /// from a working control and taught users the app was broken.
    ///
    /// If you wire one up, flip it here — the toolbar picks it up automatically.
    #[test]
    fn only_implemented_toggles_are_offered() {
        let implemented: Vec<_> = QueryToggle::all()
            .into_iter()
            .filter(|t| t.is_implemented())
            .collect();
        assert_eq!(
            implemented,
            vec![
                QueryToggle::AutoFormat,
                QueryToggle::AiAutocomplete,
                QueryToggle::RunCurrentStatement,
                QueryToggle::IncludeStatistics,
                QueryToggle::TrackQueryDifference,
                QueryToggle::QueryResume,
                QueryToggle::ProcedureMode
            ],
            "the toolbar renders exactly the toggles that change behavior"
        );
    }

    /// Every toggle keeps its metadata even while hidden, so wiring one up is a
    /// one-line change rather than a resurrection.
    #[test]
    fn unimplemented_toggles_keep_state_and_metadata() {
        let mut s = QueryToolbarState::default();
        for t in QueryToggle::all() {
            assert!(!t.tooltip().is_empty(), "{t:?} needs a tooltip");
            s.set(t, true);
            assert!(s.get(t), "{t:?} must still round-trip its state");
        }
    }

    #[test]
    fn every_query_toggle_maps_to_an_action_bar_home() {
        use super::QueryToggleHome;
        let homes: Vec<_> = QueryToggle::all()
            .into_iter()
            .map(|t| (t, t.action_bar_home()))
            .collect();
        assert_eq!(homes.len(), QueryToggle::all().len());
        assert_eq!(
            QueryToggle::AutoFormat.action_bar_home(),
            QueryToggleHome::ActionBarFormat
        );
        assert_eq!(
            QueryToggle::RunCurrentStatement.action_bar_home(),
            QueryToggleHome::ActionBarRun
        );
        assert_eq!(
            QueryToggle::ShowActualPlan.action_bar_home(),
            QueryToggleHome::OutputTabPlan
        );
        assert_eq!(
            QueryToggle::ShowEstimatedPlan.action_bar_home(),
            QueryToggleHome::OutputTabPlan
        );
        assert_eq!(
            QueryToggle::AiAutocomplete.action_bar_home(),
            QueryToggleHome::AiDrawer
        );
        assert_eq!(
            QueryToggle::TrackQueryDifference.action_bar_home(),
            QueryToggleHome::Overflow
        );
        assert_eq!(
            QueryToggle::QueryResume.action_bar_home(),
            QueryToggleHome::Overflow
        );
        assert_eq!(
            QueryToggle::ProcedureMode.action_bar_home(),
            QueryToggleHome::Overflow
        );
        assert!(
            [
                QueryToggle::IncludeStatistics,
                QueryToggle::TrackQueryDifference,
                QueryToggle::QueryResume,
                QueryToggle::ProcedureMode,
            ]
            .into_iter()
            .all(|t| t.action_bar_home() == QueryToggleHome::Overflow),
            "remaining implemented toggles live in the overflow menu"
        );
    }

    #[test]
    fn status_level_defaults_to_info() {
        assert_eq!(StatusLevel::default(), StatusLevel::Info);
    }

    #[test]
    fn connection_picker_width_is_fixed() {
        assert!(
            (160.0..=220.0).contains(&super::CONNECTION_PICKER_WIDTH),
            "connection pickers stay a compact toolbar width so a later change cannot stretch them to the window"
        );
    }

    #[test]
    fn connection_picker_title_padding_clears_the_kind_icon() {
        let pl = super::connection_picker_title_pl();
        let icon = crate::theme::Theme::toolbar_icon_size();
        assert!(
            pl >= icon + super::CONNECTION_PICKER_INPUT_PX,
            "title padding must clear the in-field kind icon (pl={pl}, icon={icon})"
        );
    }
}
