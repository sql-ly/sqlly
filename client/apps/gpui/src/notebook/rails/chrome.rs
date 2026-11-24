//! Top bar, banners, connection remap prompt, the Allow writes
//! confirmation, the delete undo toast, cell headers and the add-cell row.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _,
    Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::Disableable as _;
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::input::Input;
use gpui_component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};

use super::{icon_button, quiet_text, text_button};
use crate::icon_catalog::AppIcon;
use crate::notebook::cells::{self, CellRenderCx, NotebookCell};
use crate::notebook::connection::{ConnectionChoice, environment_badge, environment_label_key};
use crate::notebook::doc_adapter::{KIND_SQL, RunScope};
use crate::notebook::view::{
    CellStatus, NotebookEvent, NotebookView, RunMode, SaveState, status_label_key,
};
use crate::theme::Theme;

/// Called with the connection picked in a connection menu (`None`: inherit).
pub(super) type OnPick = std::rc::Rc<dyn Fn(Option<ConnectionChoice>, &mut gpui::App)>;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn small_text(text: String) -> gpui::Div {
    div()
        .flex_none()
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(Theme::text_dim())
        .child(text)
}

fn state_color(state: CellStatus, stale: bool) -> gpui::Hsla {
    match state {
        _ if stale => Theme::warning(),
        CellStatus::Stale | CellStatus::Partial => Theme::warning(),
        CellStatus::Fresh | CellStatus::Cached => Theme::success(),
        CellStatus::Running
        | CellStatus::Streaming
        | CellStatus::Validating
        | CellStatus::AwaitingApproval => Theme::accent(),
        CellStatus::Failed | CellStatus::Blocked | CellStatus::Unavailable => Theme::error(),
        _ => Theme::text_dim(),
    }
}

/// Menu items for picking a connection: `none_label` (the inherited
/// connection) first, then every local profile with its environment.
pub(super) fn connection_menu(
    mut menu: PopupMenu,
    none_label: String,
    choices: Vec<ConnectionChoice>,
    current: Option<String>,
    on_pick: OnPick,
) -> PopupMenu {
    let pick = on_pick.clone();
    menu = menu.item(
        PopupMenuItem::new(none_label)
            .checked(current.is_none())
            .on_click(move |_, _, app| pick(None, app)),
    );
    menu = menu.separator();
    for choice in choices {
        let pick = on_pick.clone();
        let label = crate::localization::format(
            "notebook.shell.connection.menu_item",
            &[
                ("connection_name", choice.label.as_str()),
                ("environment", &t(environment_label_key(choice.environment))),
            ],
        );
        let checked = current.as_deref() == Some(choice.profile_id.as_str());
        menu = menu.item(
            PopupMenuItem::new(label)
                .checked(checked)
                .on_click(move |_, _, app| pick(Some(choice.clone()), app)),
        );
    }
    menu
}

impl NotebookView {
    pub(in crate::notebook) fn render_top_bar(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let this = cx.entity();
        self.restore_test_summary();
        let running = self.is_running();
        let save_key = match self.save_state {
            SaveState::Saved => "notebook.shell.top_bar.saved",
            SaveState::Dirty => "notebook.shell.top_bar.unsaved",
            SaveState::Autosaved => "notebook.shell.top_bar.autosaved",
            SaveState::Recovered => "notebook.shell.top_bar.recovered",
        };

        let connection = {
            let notebook_conn = self.doc.notebook_connection().cloned();
            let shown = match &notebook_conn {
                Some(r) => Some(self.choice_for_ref(r)),
                None => self.tab_connection.clone(),
            };
            let label = shown
                .as_ref()
                .map(|c| c.label.clone())
                .unwrap_or_else(|| t("notebook.shell.connection.none"));
            let choices = self.choices_source();
            let current = notebook_conn.and_then(|r| r.profile_id);
            let pick_this = this.clone();
            let pick: OnPick = std::rc::Rc::new(move |choice, app| {
                pick_this.update(app, |v, cx| v.set_notebook_connection(choice, cx));
            });
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(4.0)))
                .child(
                    text_button(
                        "nb-notebook-connection",
                        crate::localization::format(
                            "notebook.shell.top_bar.connection_label",
                            &[("connection_name", label.as_str())],
                        ),
                    )
                    .tooltip(t("notebook.shell.top_bar.connection_tooltip"))
                    .dropdown_menu(move |menu, _, _| {
                        connection_menu(
                            menu,
                            t("notebook.shell.connection.use_tab"),
                            choices(),
                            current.clone(),
                            pick.clone(),
                        )
                    }),
                )
                .when_some(shown.map(|c| c.environment), |d, env| {
                    d.child(environment_badge(env))
                })
        };

        let run_menu_this = this.clone();
        let export_this = this.clone();
        let more_this = this.clone();
        let autosave_files = self.prefs.autosave_files;
        let zen = self.present.zen;
        let dense = self.present.dense;
        let embed_all = self.embeds_all();
        let run_mode = self.run_mode;

        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(6.0)))
            .px(px(s(8.0)))
            .py(px(s(4.0)))
            .border_b_1()
            .border_color(Theme::border())
            .bg(Theme::tab_strip_bg())
            .when_some(self.title_input.clone(), |d, input| {
                d.child(div().w(px(s(220.0))).child(Input::new(&input).small()))
            })
            .child(small_text(t(save_key)))
            .children(self.render_test_summary(cx))
            .when(self.conflict, |d| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(4.0)))
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::warning())
                                .child(t("notebook.shell.conflict.indicator")),
                        )
                        .child(
                            text_button("nb-conflict-reload", t("notebook.shell.conflict.reload"))
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(NotebookEvent::ReloadRequested)
                                })),
                        )
                        .child(
                            text_button("nb-conflict-keep", t("notebook.shell.conflict.keep_mine"))
                                .on_click(cx.listener(|this, _, _, cx| this.keep_mine(cx))),
                        ),
                )
            })
            .when(self.recovery_offer.is_some(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(4.0)))
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::warning())
                                .child(t("notebook.fix_runtime.recovery.offer")),
                        )
                        .child(
                            text_button(
                                "nb-recovery-restore",
                                t("notebook.fix_runtime.recovery.restore"),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.restore_offered(cx))),
                        )
                        .child(
                            text_button(
                                "nb-recovery-discard",
                                t("notebook.fix_runtime.recovery.discard"),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.discard_offered(cx))),
                        ),
                )
            })
            .child(div().w(px(1.0)).h(px(s(16.0))).bg(Theme::border()))
            .child(connection)
            .child(div().w(px(1.0)).h(px(s(16.0))).bg(Theme::border()))
            .child(
                text_button("nb-run-mode", t(run_mode.label_key()))
                    .tooltip(t("notebook.shell.top_bar.run_mode_tooltip"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.run_mode = match this.run_mode {
                            RunMode::CellOnly => RunMode::WithUpstream,
                            RunMode::WithUpstream => RunMode::CellOnly,
                        };
                        cx.notify();
                    })),
            )
            .child(
                icon_button(
                    "nb-run-active",
                    AppIcon::Play,
                    t("notebook.shell.top_bar.run_cell"),
                )
                .on_click(cx.listener(|this, _, _, cx| this.run_active(cx))),
            )
            .child(
                text_button("nb-run-all", t("notebook.shell.top_bar.run_all"))
                    .icon(Icon::empty().path(AppIcon::Play.path()))
                    .on_click(cx.listener(|this, _, _, cx| this.run_all(cx))),
            )
            .child(
                text_button("nb-stop", t("notebook.shell.top_bar.stop"))
                    .disabled(!running)
                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
            )
            .child(
                text_button("nb-run-menu", t("notebook.shell.top_bar.run_menu"))
                    .icon(Icon::empty().path(AppIcon::ChevronDown.path()))
                    .dropdown_menu(move |mut menu, _, _| {
                        for scope in RunScope::MENU {
                            let this = run_menu_this.clone();
                            if scope == RunScope::ValidateOnly {
                                menu = menu.separator();
                            }
                            menu = menu.item(PopupMenuItem::new(t(scope.label_key())).on_click(
                                move |_, _, app| {
                                    this.update(app, |v, cx| {
                                        let anchor = v.active.clone();
                                        v.run_scope(scope, anchor, cx)
                                    });
                                },
                            ));
                        }
                        menu
                    }),
            )
            .child(div().w(px(1.0)).h(px(s(16.0))).bg(Theme::border()))
            .child(
                text_button("nb-save", t("notebook.shell.top_bar.save"))
                    .icon(Icon::empty().path(AppIcon::Save.path()))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(NotebookEvent::SaveRequested))),
            )
            .child(
                text_button("nb-export", t("notebook.shell.top_bar.export")).dropdown_menu(
                    move |menu, _, _| {
                        let dialog = export_this.clone();
                        let md = export_this.clone();
                        let ipynb = export_this.clone();
                        let script = export_this.clone();
                        #[cfg(target_arch = "wasm32")]
                        let menu = browser_export_items(menu, &export_this);
                        menu.item(
                            PopupMenuItem::new(t("notebook.dashboard.export.menu_item")).on_click(
                                move |_, window, app| {
                                    dialog.update(app, |v, cx| {
                                        v.open_export_dialog(None, None, window, cx)
                                    })
                                },
                            ),
                        )
                        .separator()
                        .item(
                            PopupMenuItem::new(t("notebook.shell.top_bar.export_markdown"))
                                .on_click(move |_, _, app| {
                                    md.update(app, |_, cx| {
                                        cx.emit(NotebookEvent::ExportMarkdownRequested)
                                    })
                                }),
                        )
                        .item(
                            PopupMenuItem::new(t("notebook.shell.top_bar.export_jupyter"))
                                .on_click(move |_, _, app| {
                                    ipynb.update(app, |_, cx| {
                                        cx.emit(NotebookEvent::ExportJupyterRequested)
                                    })
                                }),
                        )
                        .item(
                            PopupMenuItem::new(t("notebook.shell.top_bar.convert_to_script"))
                                .on_click(move |_, _, app| {
                                    script.update(app, |_, cx| {
                                        cx.emit(NotebookEvent::ConvertToScriptRequested)
                                    })
                                }),
                        )
                    },
                ),
            )
            .child(
                text_button("nb-dashboard", t("notebook.dashboard.top_bar.dashboard"))
                    .tooltip(t("notebook.dashboard.top_bar.dashboard_tooltip"))
                    .on_click(cx.listener(|this, _, _, cx| this.open_dashboard_editor(None, cx))),
            )
            .child(
                text_button("nb-view", t("notebook.dashboard.top_bar.view"))
                    .tooltip(t("notebook.dashboard.top_bar.view_tooltip"))
                    .on_click(cx.listener(|this, _, _, cx| this.open_dashboard_viewer(None, cx))),
            )
            .child(
                text_button("nb-presentation", t("notebook.shell.top_bar.presentation"))
                    .tooltip(t("notebook.shell.top_bar.presentation_tooltip"))
                    .on_click(
                        cx.listener(|this, _, window, cx| {
                            this.start_presentation(None, window, cx)
                        }),
                    ),
            )
            .child(
                icon_button(
                    "nb-more",
                    AppIcon::MoreHorizontal,
                    t("notebook.shell.top_bar.more"),
                )
                .dropdown_menu(move |menu, _, _| {
                    let save_as = more_this.clone();
                    let autosave = more_this.clone();
                    let zen_this = more_this.clone();
                    let dense_this = more_this.clone();
                    let embed_this = more_this.clone();
                    menu.item(
                        PopupMenuItem::new(t("notebook.shell.top_bar.save_as")).on_click(
                            move |_, _, app| {
                                save_as.update(app, |_, cx| cx.emit(NotebookEvent::SaveAsRequested))
                            },
                        ),
                    )
                    .item(
                        PopupMenuItem::new(t("notebook.shell.top_bar.autosave_files"))
                            .checked(autosave_files)
                            .on_click(move |_, _, app| {
                                autosave
                                    .update(app, |v, cx| v.set_autosave_files(!autosave_files, cx))
                            }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new(t("notebook.dashboard.menu.zen"))
                            .checked(zen)
                            .on_click(move |_, _, app| {
                                zen_this.update(app, |v, cx| v.toggle_zen(cx))
                            }),
                    )
                    .item(
                        PopupMenuItem::new(t("notebook.dashboard.menu.dense"))
                            .checked(dense)
                            .on_click(move |_, _, app| {
                                dense_this.update(app, |v, cx| v.toggle_dense(cx))
                            }),
                    )
                    .item(
                        PopupMenuItem::new(t("notebook.embedded.menu.embed_all"))
                            .checked(embed_all)
                            .on_click(move |_, _, app| {
                                embed_this.update(app, |v, cx| v.toggle_embed_all(cx))
                            }),
                    )
                }),
            )
            .child(div().flex_1())
            .child(
                icon_button(
                    "nb-toggle-left",
                    AppIcon::SidebarLeft,
                    t("notebook.shell.top_bar.toggle_left_rail"),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_layout(|l| l.left_visible = !l.left_visible, cx)
                })),
            )
            .child(
                icon_button(
                    "nb-toggle-activity",
                    AppIcon::LayoutBottom,
                    t("notebook.shell.top_bar.toggle_activity"),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_layout(|l| l.activity_visible = !l.activity_visible, cx)
                })),
            )
            .child(
                icon_button(
                    "nb-toggle-right",
                    AppIcon::SidebarRight,
                    t("notebook.shell.top_bar.toggle_inspector"),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.update_layout(|l| l.right_visible = !l.right_visible, cx)
                })),
            )
            .into_any_element()
    }

    pub(in crate::notebook) fn render_banners(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let s = Theme::scaled;
        let mut out = Vec::new();
        out.extend(self.render_trust_banner(cx));
        out.extend(self.render_scan_banner(cx));
        if !self.unresolved.is_empty() {
            let this = cx.entity();
            let mut rows = div().flex().flex_col().gap(px(s(4.0)));
            for (n, missing) in self.unresolved.clone().into_iter().enumerate() {
                let label = match &missing.database {
                    Some(db) => crate::localization::format(
                        "notebook.shell.remap.reference_with_database",
                        &[
                            ("connection_name", missing.label.as_str()),
                            ("database", db.as_str()),
                        ],
                    ),
                    None => missing.label.clone(),
                };
                let choices = self.choices_source();
                let this = this.clone();
                let from = missing.id.clone();
                rows = rows.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(8.0)))
                        .child(
                            div()
                                .min_w(px(s(180.0)))
                                .text_color(Theme::text())
                                .child(label),
                        )
                        .child(
                            text_button(format!("nb-remap-{n}"), t("notebook.shell.remap.choose"))
                                .icon(Icon::empty().path(AppIcon::ChevronDown.path()))
                                .dropdown_menu(move |mut menu, _, _| {
                                    for choice in choices() {
                                        let this = this.clone();
                                        let from = from.clone();
                                        let label = crate::localization::format(
                                            "notebook.shell.connection.menu_item",
                                            &[
                                                ("connection_name", choice.label.as_str()),
                                                (
                                                    "environment",
                                                    &t(environment_label_key(choice.environment)),
                                                ),
                                            ],
                                        );
                                        menu = menu.item(PopupMenuItem::new(label).on_click(
                                            move |_, _, app| {
                                                this.update(app, |v, cx| {
                                                    v.remap_connection(&from, choice.clone(), cx)
                                                })
                                            },
                                        ));
                                    }
                                    menu
                                }),
                        ),
                );
            }
            out.push(
                div()
                    .id("nb-remap")
                    .flex()
                    .flex_col()
                    .gap(px(s(6.0)))
                    .px(px(s(12.0)))
                    .py(px(s(8.0)))
                    .border_b_1()
                    .border_color(Theme::warning())
                    .bg(Theme::panel_bg())
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.9)))
                            .text_color(Theme::text())
                            .child(t("notebook.shell.remap.title")),
                    )
                    .child(rows)
                    .child(
                        div().child(
                            text_button("nb-remap-skip", t("notebook.shell.remap.skip"))
                                .on_click(cx.listener(|this, _, _, cx| this.dismiss_remap(cx))),
                        ),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    pub(in crate::notebook) fn render_elevation_prompt(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let id = self.elevation_prompt.clone()?;
        let s = Theme::scaled;
        let name = self
            .doc
            .cell_by_id(&id)
            .map(|c| c.display_name().to_string())
            .unwrap_or_else(|| id.clone());
        let (choice, _) = self.cell_connection(&id);
        let connection = choice
            .as_ref()
            .map(|c| c.label.clone())
            .unwrap_or_else(|| t("notebook.shell.connection.none"));
        let env = choice
            .as_ref()
            .map(|c| c.environment)
            .unwrap_or(sqlly_core::ConnectionEnvironment::Unset);
        let production = env == sqlly_core::ConnectionEnvironment::Production;
        Some(
            div()
                .id("nb-elevation-scrim")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(Theme::scrim())
                .child(
                    div()
                        .id("nb-elevation")
                        .w(px(s(440.0)))
                        .flex()
                        .flex_col()
                        .gap(px(s(10.0)))
                        .p(px(s(16.0)))
                        .rounded(px(s(6.0)))
                        .border_1()
                        .border_color(if production {
                            Theme::error()
                        } else {
                            Theme::popup_border()
                        })
                        .bg(Theme::popup_bg())
                        .child(
                            div()
                                .text_color(Theme::text())
                                .child(crate::localization::format(
                                    "notebook.shell.safety.allow_writes_title",
                                    &[("cell_name", name.as_str())],
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(s(6.0)))
                                .child(small_text(crate::localization::format(
                                    "notebook.shell.safety.allow_writes_target",
                                    &[("connection_name", connection.as_str())],
                                )))
                                .child(environment_badge(env)),
                        )
                        .child(small_text(t("notebook.shell.safety.allow_writes_detail")))
                        .when(production, |d| {
                            d.child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.9)))
                                    .text_color(Theme::error())
                                    .child(t("notebook.shell.safety.allow_writes_production")),
                            )
                        })
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .justify_end()
                                .gap(px(s(8.0)))
                                .child(
                                    text_button(
                                        "nb-elevation-cancel",
                                        t("notebook.shell.safety.allow_writes_cancel"),
                                    )
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.cancel_allow_writes(cx)),
                                    ),
                                )
                                .child(
                                    text_button(
                                        "nb-elevation-confirm",
                                        t("notebook.shell.safety.allow_writes_confirm"),
                                    )
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.confirm_allow_writes(cx)),
                                    ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    pub(in crate::notebook) fn undo_toast(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.render_editing_toast(cx)
    }

    pub(in crate::notebook) fn render_add_row(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let mut row = div()
            .flex()
            .flex_row()
            .gap(px(s(8.0)))
            .px(px(s(12.0)))
            .py(px(s(10.0)));
        for renderer in cells::insertable_kinds() {
            let kind = renderer.kind();
            row = row.child(
                text_button(
                    format!("nb-add-{kind}"),
                    crate::localization::format(
                        "notebook.shell.cell.add_kind",
                        &[("kind", t(renderer.label_key()))],
                    ),
                )
                .icon(Icon::empty().path(AppIcon::Add.path()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    let end = this.doc.len();
                    let id = this.insert_cell(kind, end, cx);
                    this.edit_cell(&id, cx);
                })),
            );
        }
        row.into_any_element()
    }

    /// Whether `source` changes data, re-read only when it changed.
    pub(in crate::notebook) fn source_writes(&mut self, id: &str, source: &str) -> bool {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        source.hash(&mut hasher);
        let hash = hasher.finish();
        if let Some((seen, writes)) = self.writes_seen.get(id)
            && *seen == hash
        {
            return *writes;
        }
        let writes = crate::mutation_review::contains_non_select_statement(source);
        self.writes_seen.insert(id.to_string(), (hash, writes));
        writes
    }

    pub(in crate::notebook) fn render_cell_header(
        &mut self,
        cell: &CellRenderCx,
        renderer: &'static (dyn NotebookCell + Sync),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let id = cell.id.clone();
        let index = cell.index;
        let view = self.doc.cell_by_id(&id);
        let display = view
            .map(|c| c.display_name().to_string())
            .unwrap_or_else(|| id.clone());
        let read_only = view.is_some_and(|c| c.read_only());
        let is_sql = cell.kind == KIND_SQL;
        let writes = is_sql && read_only && self.source_writes(&id, &cell.source);
        let state_label = crate::localization::text(status_label_key(cell.state, cell.stale));
        let this = cx.entity();
        let selector = format!("nb-cell-header-{id}");
        // Only the active cell and the one under the pointer get real
        // buttons; the others draw the same labels as plain text, which
        // keeps the header from moving when the pointer arrives.
        let full = self.shows_controls(&id);

        let mut header = div()
            .debug_selector(move || selector.clone())
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(6.0)))
            .px(px(s(8.0)))
            .min_h(px(Theme::row_height()))
            .border_b_1()
            .border_color(Theme::border())
            .child(if full {
                self.render_drag_handle(cell, cx)
            } else {
                super::drag_handle_space()
            })
            .child(
                div()
                    .size(px(s(8.0)))
                    .rounded_full()
                    .bg(state_color(cell.state, cell.stale)),
            )
            .child(small_text(t(renderer.label_key())))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text())
                    .child(display.clone()),
            )
            .children(crate::notebook::ai::render::attribution_badge(self, &id));

        if renderer.runnable() {
            header = header.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(state_color(cell.state, cell.stale))
                    .child(state_label),
            );
            if let Some(ms) = cell.elapsed_ms {
                header = header.child(small_text(crate::localization::format(
                    "notebook.shell.cell.elapsed",
                    &[("milliseconds", ms)],
                )));
            }
        }

        if is_sql {
            let local = self.doc.targets_local(&id);
            let (choice, own) = self.cell_connection(&id);
            let resolved = choice
                .as_ref()
                .is_none_or(|c| self.is_resolved(&c.profile_id) || !own);
            let label = match (&choice, own) {
                _ if local => t(crate::notebook::local::LOCAL_LABEL_KEY),
                (Some(c), true) => c.label.clone(),
                (Some(c), false) => crate::localization::format(
                    "notebook.shell.connection.inherited",
                    &[("connection_name", c.label.as_str())],
                ),
                (None, _) => t("notebook.shell.connection.none"),
            };
            let choices = self.choices_source();
            let current = own
                .then(|| choice.as_ref().map(|c| c.profile_id.clone()))
                .flatten();
            let pick_this = this.clone();
            let pick_id = id.clone();
            let pick: OnPick = std::rc::Rc::new(move |choice, app| {
                pick_this.update(app, |v, cx| {
                    v.set_cell_local(&pick_id, false, cx);
                    v.set_cell_connection(&pick_id, choice, cx);
                });
            });
            let local_this = this.clone();
            let local_id = id.clone();
            let connection = if !full {
                quiet_text(label, cx).into_any_element()
            } else {
                text_button(format!("nb-cell-connection-{index}"), label)
                    .tooltip(t("notebook.shell.connection.cell_tooltip"))
                    .dropdown_menu(move |menu, _, _| {
                        let local_this = local_this.clone();
                        let local_id = local_id.clone();
                        connection_menu(
                            menu,
                            t("notebook.shell.connection.use_notebook"),
                            choices(),
                            current.clone().filter(|_| !local),
                            pick.clone(),
                        )
                        .separator()
                        .item(
                            PopupMenuItem::new(t(crate::notebook::local::LOCAL_LABEL_KEY))
                                .checked(local)
                                .on_click(move |_, _, app| {
                                    local_this
                                        .update(app, |v, cx| v.set_cell_local(&local_id, true, cx));
                                }),
                        )
                    })
                    .into_any_element()
            };
            header = header
                .child(connection)
                .when_some(
                    choice.as_ref().filter(|_| !local).map(|c| c.environment),
                    |d, env| d.child(environment_badge(env)),
                )
                .when(!resolved, |d| {
                    d.child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::error())
                            .child(t("notebook.shell.connection.missing_badge")),
                    )
                })
                .child(
                    div()
                        .px(px(s(5.0)))
                        .rounded(px(s(3.0)))
                        .bg(Theme::input_bg())
                        .text_size(px(Theme::font_size_ratio(0.75)))
                        .text_color(if read_only {
                            Theme::text_dim()
                        } else {
                            Theme::warning()
                        })
                        .child(t(if read_only {
                            "notebook.shell.safety.read_only_badge"
                        } else {
                            "notebook.shell.safety.writes_allowed_badge"
                        })),
                );
            if !full {
                let quiet = if read_only && writes {
                    Some("notebook.shell.safety.allow_writes_button")
                } else {
                    (!read_only).then_some("notebook.shell.safety.make_read_only_button")
                };
                header = header.children(quiet.map(|key| quiet_text(t(key), cx)));
            } else if read_only && writes {
                let id = id.clone();
                header = header.child(
                    text_button(
                        format!("nb-allow-writes-{index}"),
                        t("notebook.shell.safety.allow_writes_button"),
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.request_allow_writes(&id, cx)),
                    ),
                );
            } else if !read_only {
                let id = id.clone();
                header = header.child(
                    text_button(
                        format!("nb-make-read-only-{index}"),
                        t("notebook.shell.safety.make_read_only_button"),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| this.make_read_only(&id, cx))),
                );
            }
        }

        header = header.children(self.local_header_items(cell, cx));
        header = header.children(self.editing_header_badges(cell, cx));
        header = header.child(div().flex_1());
        if !full {
            return header.into_any_element();
        }
        header = header.child(self.editing_cell_menu(cell, cx));
        if renderer.runnable() {
            let id = id.clone();
            header = header.child(
                icon_button(
                    format!("nb-run-{index}"),
                    AppIcon::Play,
                    crate::localization::format(
                        "notebook.shell.cell.run_accessibility",
                        &[("cell_name", display.as_str())],
                    ),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.activate_cell(&id, cx);
                    let scope = this.run_mode.scope();
                    this.run_scope(scope, Some(id.clone()), cx);
                })),
            );
        }
        let up = id.clone();
        let down = id.clone();
        let fold = id.clone();
        let del = id.clone();
        header
            .child(
                icon_button(
                    format!("nb-up-{index}"),
                    AppIcon::ChevronUp,
                    t("notebook.shell.cell.move_up"),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.move_cell(&up, -1, cx))),
            )
            .child(
                icon_button(
                    format!("nb-down-{index}"),
                    AppIcon::ChevronDown,
                    t("notebook.shell.cell.move_down"),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.move_cell(&down, 1, cx))),
            )
            .child(
                icon_button(
                    format!("nb-collapse-{index}"),
                    if cell.collapsed {
                        AppIcon::ChevronRight
                    } else {
                        AppIcon::ChevronDown
                    },
                    t(if cell.collapsed {
                        "notebook.shell.cell.expand"
                    } else {
                        "notebook.shell.cell.collapse"
                    }),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_collapsed(&fold, cx))),
            )
            .child(
                icon_button(
                    format!("nb-delete-{index}"),
                    AppIcon::Close,
                    t("notebook.shell.cell.delete"),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.delete_cell(&del, cx))),
            )
            .into_any_element()
    }
}

/// The browser has no Export dialog window: each format downloads straight
/// from the menu (PDF and PNG say they need the desktop app).
#[cfg(target_arch = "wasm32")]
fn browser_export_items(mut menu: PopupMenu, this: &gpui::Entity<NotebookView>) -> PopupMenu {
    use crate::notebook::export_dialog::ExportFormat;
    for format in ExportFormat::ALL {
        let this = this.clone();
        menu = menu.item(PopupMenuItem::new(t(format.label_key())).on_click(
            move |_, window, app| {
                this.update(app, |v, cx| {
                    v.open_export_dialog(Some(format), None, window, cx)
                })
            },
        ));
    }
    menu.separator()
}
