//! Saved chart library entry points: the Tools-menu / palette actions, the
//! "Save chart to library…" prompt raised by a results view, and the Open
//! flow that re-runs a saved chart on its own connection with the Chart tab
//! applied.

use super::*;

impl AppView {
    /// Open the Saved Charts panel (Tools menu / palette).
    pub(super) fn handle_saved_charts(
        &mut self,
        _action: &SavedCharts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = window;
            self.set_status(
                StatusLevel::Info,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.charts.saved_unavailable",
                )),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.overlays.saved_charts_panel.is_some() {
                return;
            }
            let store = crate::saved_charts::SavedChartStore::default_store();
            let view =
                cx.new(|cx| crate::saved_charts_panel::SavedChartsPanel::new(store, window, cx));
            cx.subscribe(&view, Self::handle_saved_charts_panel_event)
                .detach();
            self.overlays.saved_charts_panel = Some(view);
            cx.notify();
        }
    }

    /// Open the Chart Dashboard (Tools menu / palette).
    pub(super) fn handle_chart_dashboard(
        &mut self,
        _action: &ChartDashboardTool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Info,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.charts.dashboard_unavailable",
                )),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.open_chart_dashboard(cx);
        }
    }

    /// Raised by a results view's "Save chart to library…": the workbench
    /// owns the connection, so it fills in the draft and shows the prompt.
    pub(super) fn open_save_chart_prompt(
        &mut self,
        spec: crate::result_chart::ResultChartSpec,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (spec, sql);
            self.set_status(
                StatusLevel::Info,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.charts.save_prompt_unavailable",
                )),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(profile) = self.active_profile.clone() else {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.charts.connect_before_save"),
                );
                cx.notify();
                return;
            };
            let draft = crate::saved_charts::SavedChartDraft {
                name: String::new(),
                connection_id: Some(profile.id.0.to_string()),
                connection_name: profile.name.clone(),
                database: profile.database.clone(),
                sql,
                spec,
                refresh_secs: None,
            };
            let store = crate::saved_charts::SavedChartStore::default_store();
            let view = cx.new(|_cx| crate::saved_charts_panel::SaveChartDialog::new(store, draft));
            cx.subscribe(&view, Self::handle_save_chart_dialog_event)
                .detach();
            self.overlays.save_chart_dialog = Some(view);
            cx.notify();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_chart_dashboard(&mut self, cx: &mut Context<Self>) {
        if let Some(existing) = &self.overlays.chart_dashboard {
            existing.update(cx, |dashboard, cx| dashboard.reload(cx));
            cx.notify();
            return;
        }
        let store = crate::saved_charts::SavedChartStore::default_store();
        let router = self.service_router.clone();
        let view = cx.new(|cx| crate::chart_dashboard::ChartDashboard::new(store, router, cx));
        cx.subscribe(&view, Self::handle_chart_dashboard_event)
            .detach();
        self.overlays.chart_dashboard = Some(view);
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_chart_dashboard_event(
        &mut self,
        _view: Entity<crate::chart_dashboard::ChartDashboard>,
        event: &crate::tool_dialog::ToolDialogEvent,
        cx: &mut Context<Self>,
    ) {
        if self.apply_tool_dialog_event(event, cx) {
            self.overlays.chart_dashboard = None;
            self.restore_editor_focus(cx);
            cx.notify();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_save_chart_dialog_event(
        &mut self,
        _view: Entity<crate::saved_charts_panel::SaveChartDialog>,
        event: &crate::saved_charts_panel::SaveChartDialogEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::saved_charts_panel::SaveChartDialogEvent::Saved(chart) => {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::format(
                        "shell.charts.saved_status",
                        &[("name", &chart.name)],
                    ),
                );
                // Surfaces already showing the library pick up the new entry.
                if let Some(panel) = &self.overlays.saved_charts_panel {
                    panel.update(cx, |panel, cx| panel.refresh(cx));
                }
                if let Some(dashboard) = &self.overlays.chart_dashboard {
                    dashboard.update(cx, |dashboard, cx| dashboard.reload(cx));
                }
                cx.notify();
            }
            crate::saved_charts_panel::SaveChartDialogEvent::Closed => {
                self.overlays.save_chart_dialog = None;
                self.restore_editor_focus(cx);
                cx.notify();
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_saved_charts_panel_event(
        &mut self,
        _view: Entity<crate::saved_charts_panel::SavedChartsPanel>,
        event: &crate::saved_charts_panel::SavedChartsPanelEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::saved_charts_panel::SavedChartsPanelEvent::Open(chart) => {
                self.open_saved_chart(chart.as_ref().clone(), cx);
            }
            crate::saved_charts_panel::SavedChartsPanelEvent::OpenDashboard => {
                self.open_chart_dashboard(cx);
            }
            crate::saved_charts_panel::SavedChartsPanelEvent::Status { level, message } => {
                self.set_status(*level, message.clone());
                cx.notify();
            }
            crate::saved_charts_panel::SavedChartsPanelEvent::Closed => {
                self.overlays.saved_charts_panel = None;
                self.restore_editor_focus(cx);
                cx.notify();
            }
        }
    }

    /// Run a saved chart: retarget to its connection and database (connecting
    /// if needed), open its SQL — led by `-- sqlly view: chart` and its chart
    /// directive — in a new tab, and execute through the normal path so every
    /// gate (params, safety review, read-only locks) still applies. The
    /// results view then opens the Chart tab with the saved layout itself.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn open_saved_chart(
        &mut self,
        chart: crate::saved_charts::SavedChart,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = crate::chart_dashboard::resolve_profile(&chart) else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::format(
                    "shell.charts.connection_missing",
                    &[
                        ("connection", &chart.connection_name),
                        ("name", &chart.name),
                    ],
                ),
            );
            cx.notify();
            return;
        };
        let sql = chart.runnable_sql();
        let id = chart.id.clone();
        self.with_explorer_target_active(Some(profile), cx, move |app, cx| {
            app.new_tab(cx, Some(sql));
            let tab_idx = app.active_tab;
            app.execute_query(tab_idx, cx);
            let _ = crate::saved_charts::SavedChartStore::default_store()
                .touch_last_run(&id, chrono::Utc::now().timestamp_millis());
            cx.notify();
        });
    }
}
