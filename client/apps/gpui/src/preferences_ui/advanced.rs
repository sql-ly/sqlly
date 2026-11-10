//! Query Resume, Advanced, and Debug Logging panes.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    // ============================================================
    // Workspace > Query resume
    // ============================================================

    pub(super) fn render_query_resume(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self.store.query_resume().scope;
        let options: Vec<DropdownOption> = preferences::QueryResumeScope::all()
            .into_iter()
            .map(|scope| {
                let on_select: DropdownHandler = std::rc::Rc::new(
                    move |this: &mut Self, _window: &mut Window, cx: &mut Context<Self>| {
                        this.set_qr_scope(scope, cx);
                    },
                );
                (
                    scope.display_title().to_string(),
                    scope == current,
                    on_select,
                )
            })
            .collect();

        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.advanced.heading.query_resume",
            )),
            self.intro(&crate::localization::text(
                "settings.advanced.intro.query_resume",
            )),
            self.section_header(&crate::localization::text(
                "settings.advanced.section_header.default",
            )),
            self.select_dropdown("qr_scope", &current.display_title(), options, window, cx),
            self.hint(&current.explanation()),
            self.section_header(&crate::localization::text(
                "settings.advanced.section_header.per_query",
            )),
            self.hint(&crate::localization::text(
                "settings.advanced.hint.per_query_toggle",
            )),
            self.hint(&crate::localization::text(
                "settings.advanced.hint.per_query_tracked_state",
            )),
            self.section_header(&crate::localization::text(
                "settings.advanced.section_header.reconnect",
            )),
            self.hint(&crate::localization::text(
                "settings.advanced.hint.reconnect_behavior",
            )),
            self.warning(&preferences::QueryResumeScope::Manual.explanation()),
        ])
    }

    // ============================================================
    // Workspace > Advanced
    // ============================================================

    pub(super) fn render_advanced(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let _ = window;
        #[cfg(target_arch = "wasm32")]
        let _ = cx;
        let mut rows: Vec<AnyElement> = vec![
            self.heading(&crate::localization::text(
                "settings.advanced.heading.advanced",
            )),
            self.intro(&crate::localization::text(
                "settings.advanced.intro.advanced",
            )),
        ];
        #[cfg(not(target_arch = "wasm32"))]
        {
            let detected = match sqlly_engines::ssh::locate_kubectl() {
                sqlly_engines::ssh::KubectlLocation::Override(path)
                | sqlly_engines::ssh::KubectlLocation::OnPath(path) => crate::localization::format(
                    "settings.advanced.message.found",
                    &[("value", &(path.display()).to_string())],
                ),
                sqlly_engines::ssh::KubectlLocation::NotFound => {
                    crate::localization::text("settings.advanced.option.not_found_path")
                }
            };
            let entity = cx.entity();
            rows.push(self.section_header(&crate::localization::text(
                "settings.advanced.section_header.kubernetes",
            )));
            if crate::distribution::can_launch_user_programs() {
                rows.push(self.input_row(
                    &crate::localization::text("settings.advanced.input_row.kubectl_path"),
                    "kubectl_path",
                ));
                rows.push(self.hint(&detected));
                rows.push(
                    Button::new("prefs-kubectl-detect")
                        .label(crate::localization::text(
                            "settings.advanced.label.detect_again",
                        ))
                        .ghost()
                        .on_click(move |_, _window, app| {
                            entity.update(app, |_this, cx| cx.notify());
                        })
                        .into_any_element(),
                );
            } else {
                let _ = (detected, entity);
                rows.push(crate::app_store_edition_dialog::edition_notice(
                    &crate::localization::text(
                        "settings.advanced.computed.app_store_kubectl_notice",
                    ),
                ));
            }
            rows.push(self.section_header(&crate::localization::text(
                "settings.advanced.section_header.database_tools",
            )));
            if crate::distribution::is_app_store() {
                rows.push(self.intro(&crate::localization::text(
                    "settings.advanced.intro.tools_bundled_app_store",
                )));
            } else {
                rows.push(self.intro(&crate::localization::text(
                    "settings.advanced.intro.tool_path_overrides",
                )));
                rows.push(self.native_tool_rows(window, cx));
            }
            if cfg!(target_os = "macos") && crate::distribution::can_launch_user_programs() {
                let explain =
                    !preferences::NativeToolPreferences::load().terminal_secret_notice_dismissed;
                rows.push(self.checkbox(
                    "prefs-terminal-secret-notice",
                    &crate::localization::text(
                        "settings.advanced.checkbox.terminal_password_notice",
                    ),
                    explain,
                    cx,
                    |_this, cx| {
                        let mut prefs = preferences::NativeToolPreferences::load();
                        prefs.terminal_secret_notice_dismissed =
                            !prefs.terminal_secret_notice_dismissed;
                        prefs.save();
                        cx.notify();
                    },
                ));
                rows.push(self.hint(&crate::localization::text(
                    "settings.advanced.hint.terminal_password_script",
                )));
            }
        }
        rows.push(self.section_header(&crate::localization::text(
            "settings.advanced.section_header.reset",
        )));
        rows.push(
            div()
                .id("prefs-clear-all-data")
                .px(px(Theme::scaled(10.0)))
                .py(px(Theme::scaled(6.0)))
                .rounded(px(Theme::scaled(4.0)))
                .border_1()
                .border_color(Theme::error())
                .text_color(Theme::error())
                .cursor_pointer()
                .hover(|s| s.bg(Theme::row_hover()))
                .child(self.match_text(&crate::localization::text(
                    "settings.advanced.match_text.clear_all_data",
                )))
                .on_mouse_down(gpui::MouseButton::Left, |_, window, cx| {
                    window.dispatch_action(Box::new(crate::app::ClearAllData), cx);
                })
                .into_any_element(),
        );
        self.pane_container(rows)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn apply_kubectl_path(&mut self, value: String, cx: &mut Context<Self>) {
        let path = value.trim();
        let mut prefs = preferences::KubectlPreferences::load();
        prefs.kubectl_path = if path.is_empty() {
            None
        } else {
            Some(path.to_string())
        };
        prefs.save();
        prefs.apply_override();
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn apply_native_tool_path(
        &mut self,
        tool: sqlly_services::NativeTool,
        value: String,
        cx: &mut Context<Self>,
    ) {
        let path = value.trim();
        let mut prefs = preferences::NativeToolPreferences::load();
        if path.is_empty() {
            prefs.overrides.remove(tool.snake_case());
        } else {
            prefs
                .overrides
                .insert(tool.snake_case().to_string(), path.to_string());
        }
        prefs.save();
        let entity = cx.entity();
        cx.spawn(async move |_this, _cx| {
            preferences::refresh_native_tool_locations().await;
            entity.update(_cx, |_this, cx| cx.notify());
        })
        .detach();
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn native_tool_rows(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        use sqlly_services::{NativeTool, find_tool};
        let locations = preferences::native_tool_locations();
        let entity = cx.entity();
        let mut rows: Vec<AnyElement> = vec![];
        for tool in NativeTool::all() {
            let status = find_tool(&locations, tool)
                .map(|loc| {
                    if let Some(ver) = &loc.version {
                        crate::localization::format(
                            "settings.advanced.message.found_ver_at",
                            &[
                                ("ver", &(ver).to_string()),
                                ("value", &(loc.path.display()).to_string()),
                            ],
                        )
                    } else {
                        crate::localization::format(
                            "settings.advanced.message.found_at",
                            &[("value", &(loc.path.display()).to_string())],
                        )
                    }
                })
                .unwrap_or_else(|| {
                    crate::localization::text("settings.advanced.computed.not_found").to_string()
                });
            rows.push(self.input_row(
                tool.label(),
                match tool {
                    NativeTool::PgDump => "native_tool_pg_dump",
                    NativeTool::PgRestore => "native_tool_pg_restore",
                    NativeTool::Psql => "native_tool_psql",
                    NativeTool::MysqlDump => "native_tool_mysqldump",
                    NativeTool::Mysql => "native_tool_mysql",
                    NativeTool::Sqlcmd => "native_tool_sqlcmd",
                    NativeTool::RedisCli => "native_tool_redis_cli",
                    NativeTool::ClickHouseClient => "native_tool_clickhouse_client",
                    NativeTool::DuckDb => "native_tool_duckdb",
                    NativeTool::Sqlite3 => "native_tool_sqlite3",
                    NativeTool::Sqlplus => "native_tool_sqlplus",
                    NativeTool::Sqlcl => "native_tool_sqlcl",
                },
            ));
            rows.push(self.hint(&status));
        }
        rows.push(
            Button::new("prefs-native-tools-rescan")
                .label(crate::localization::text("settings.advanced.label.re_scan"))
                .ghost()
                .on_click({
                    let entity = entity.clone();
                    move |_, _window, app| {
                        let entity = entity.clone();
                        entity.update(app, |_this, cx| {
                            cx.spawn(async move |_this, _cx| {
                                preferences::refresh_native_tool_locations().await;
                            })
                            .detach();
                            cx.notify();
                        });
                    }
                })
                .into_any_element(),
        );
        let _ = window;
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(8.)))
            .children(rows)
            .into_any_element()
    }

    // ============================================================
    // Debug > Logging
    // ============================================================

    pub(super) fn render_debug_logging(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = self.store.diagnostic_logging().level;
        let path = crate::log_console::diagnostic_log_file();

        let options: Vec<DropdownOption> = preferences::DiagnosticLogLevelPref::all()
            .into_iter()
            .map(|level| {
                let on_select: DropdownHandler = std::rc::Rc::new(
                    move |this: &mut Self, _window: &mut Window, cx: &mut Context<Self>| {
                        this.set_log_level(level, cx);
                    },
                );
                (
                    level.display_title().to_string(),
                    level == current,
                    on_select,
                )
            })
            .collect();

        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.advanced.heading.logging",
            )),
            self.intro(&crate::localization::text(
                "settings.advanced.intro.logging",
            )),
            self.section_header(&crate::localization::text(
                "settings.advanced.section_header.log_level",
            )),
            self.labeled_row(
                &crate::localization::text("settings.advanced.labeled_row.log_level"),
                self.select_dropdown("diag_level", &current.display_title(), options, window, cx),
            ),
            self.hint(&crate::localization::text(
                "settings.advanced.hint.log_level_explained",
            )),
            self.section_header(&crate::localization::text(
                "settings.advanced.section_header.log_location",
            )),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(8.)))
                .child(
                    div()
                        .text_color(Theme::text_dim())
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .font_family(Theme::font_family())
                        .child(path.to_string_lossy().to_string()),
                )
                .into_any_element(),
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(8.)))
                .child(
                    div()
                        .px(px(Theme::scaled(12.)))
                        .py(px(Theme::scaled(4.)))
                        .bg(Theme::panel_bg())
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(Theme::scaled(4.)))
                        .cursor_pointer()
                        .text_color(Theme::text())
                        .text_size(px(Theme::font_size()))
                        .id("diag_reveal")
                        .child(self.match_text(&crate::localization::text(
                            "settings.advanced.match_text.reveal_finder",
                        )))
                        .on_mouse_down(gpui::MouseButton::Left, cx.listener(Self::reveal_diag_log)),
                )
                .child(
                    div()
                        .px(px(Theme::scaled(12.)))
                        .py(px(Theme::scaled(4.)))
                        .bg(Theme::panel_bg())
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(Theme::scaled(4.)))
                        .cursor_pointer()
                        .text_color(Theme::text())
                        .text_size(px(Theme::font_size()))
                        .id("diag_open")
                        .child(self.match_text(&crate::localization::text(
                            "settings.advanced.match_text.open_log_file",
                        )))
                        .on_mouse_down(gpui::MouseButton::Left, cx.listener(Self::open_diag_log)),
                )
                .into_any_element(),
        ])
    }

    // ---- Logging pane actions ----

    pub(super) fn set_log_level(
        &mut self,
        level: preferences::DiagnosticLogLevelPref,
        cx: &mut Context<Self>,
    ) {
        let mut prefs = self.store.diagnostic_logging();
        prefs.level = level;
        self.store.set_diagnostic_logging(prefs);
        cx.notify();
    }

    /// Ensures the diagnostics log directory (and an empty file) exist so that
    /// "Reveal in Finder" / "Open Log File" always have a target even before the
    /// first line is logged. Returns the file path.
    pub(super) fn ensure_diag_log_file() -> std::path::PathBuf {
        let dir = crate::log_console::diagnostic_log_dir();
        let _ = std::fs::create_dir_all(&dir);
        let file = crate::log_console::diagnostic_log_file();
        if !file.exists() {
            let _ = std::fs::write(&file, b"");
        }
        file
    }

    pub(super) fn reveal_diag_log(
        _this: &mut Self,
        _ev: &MouseDownEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let file = Self::ensure_diag_log_file();
        // macOS: select the file in Finder.
        let _ = std::process::Command::new("open")
            .arg("-R")
            .arg(&file)
            .spawn();
    }

    pub(super) fn open_diag_log(
        _this: &mut Self,
        _ev: &MouseDownEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        let file = Self::ensure_diag_log_file();
        let _ = std::process::Command::new("open").arg(&file).spawn();
    }

    // ---- Query resume pane actions ----

    pub(super) fn set_qr_scope(
        &mut self,
        scope: preferences::QueryResumeScope,
        cx: &mut Context<Self>,
    ) {
        let mut prefs = self.store.query_resume();
        prefs.scope = scope;
        self.store.set_query_resume(prefs);
        cx.notify();
    }
}
