//! Local data in the notebook chrome: cell header badges and menu, the
//! footer under a cell's results (row cap, inputs' provenance), decision
//! prompts, the Local data rail page, the inspector's cache and limit
//! settings, and copy jobs in the activity panel.

use gpui::{
    AnyElement, AppContext as _, Context, Focusable as _, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::switch::Switch;
use gpui_component::{Selectable as _, Sizable as _};
use sqlly_core::ConnectionEnvironment;
use sqlly_services::notebook::CachePolicy;
use sqlly_services::notebook::cells::local::{CacheSettings, CacheStorage};

use super::{LocalPrompt, ProvenanceView, SourceKindView, age_text, count_text, size_text};
use crate::icon_catalog::AppIcon;
use crate::notebook::cells::CellRenderCx;
use crate::notebook::connection::{environment_badge, environment_color, environment_label_key};
use crate::notebook::doc_adapter::{KIND_ATTACHMENT, KIND_SQL};
use crate::notebook::rails::{icon_button, text_button};
use crate::notebook::view::{CellStatus, NotebookView};
use crate::theme::Theme;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn small(text: String) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.8)))
        .text_color(Theme::text_dim())
        .child(text)
}

/// A rounded outlined chip.
fn chip(text: String, color: gpui::Hsla) -> gpui::Div {
    div()
        .flex_none()
        .px(px(Theme::scaled(5.0)))
        .rounded(px(Theme::scaled(3.0)))
        .border_1()
        .border_color(color)
        .text_color(color)
        .text_size(px(Theme::font_size_ratio(0.75)))
        .child(text)
}

fn environment_from(text: Option<&str>) -> ConnectionEnvironment {
    text.and_then(|e| serde_json::from_value(serde_json::Value::String(e.to_string())).ok())
        .unwrap_or(ConnectionEnvironment::Unset)
}

/// The badge naming the local target.
fn local_badge() -> impl IntoElement {
    chip(t("notebook.local.badge.local"), Theme::accent())
}

/// Everything the attachment renderer needs, snapshotted from the view.
pub struct AttachmentCx {
    pub spec: sqlly_services::notebook::cells::attachment::AttachmentSpec,
    pub import: Option<super::ImportSummary>,
    pub image: Option<super::AttachmentImage>,
    pub relation_input: Option<gpui::Entity<InputState>>,
    pub table_input: Option<gpui::Entity<InputState>>,
    pub outside_folder: bool,
}

impl NotebookView {
    fn input_for(&self, id: &str, which: &str) -> Option<gpui::Entity<InputState>> {
        self.local_inputs
            .get(id)
            .and_then(|list| list.iter().find(|(k, _)| *k == which))
            .map(|(_, e)| e.clone())
    }

    #[allow(clippy::too_many_arguments)]
    fn add_input(
        &mut self,
        id: &str,
        which: &'static str,
        placeholder: &'static str,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
        on_commit: impl Fn(&mut NotebookView, &str, String, &mut Context<NotebookView>) + 'static,
    ) {
        if self.input_for(id, which).is_some() {
            return;
        }
        let state = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                placeholder,
            ))
        });
        if !value.is_empty() {
            state.update(cx, |s, cx| s.set_value(value, window, cx));
        }
        let cell = id.to_string();
        cx.subscribe(&state, move |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                let value = input.read(cx).value().to_string();
                on_commit(this, &cell, value, cx);
            }
        })
        .detach();
        self.local_inputs
            .entry(id.to_string())
            .or_default()
            .push((which, state));
    }

    /// Create the inputs local data renders (they need a window).
    pub(in crate::notebook) fn ensure_local_inputs(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(LocalPrompt::Name { cell_id }) = self.local.prompt.clone()
            && self.local_name_input.is_none()
        {
            let value = self
                .doc
                .materialized_as(&cell_id)
                .or_else(|| self.doc.suggested_relation(&cell_id))
                .unwrap_or_default();
            let state = cx.new(|cx| InputState::new(window, cx));
            state.update(cx, |s, cx| s.set_value(value, window, cx));
            cx.subscribe(&state, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.confirm_local_prompt(cx);
                }
            })
            .detach();
            window.focus(&state.read(cx).focus_handle(cx), cx);
            self.local_name_input = Some(state);
        }
        if let Some(id) = self.active.clone()
            && self.doc.cell_by_id(&id).is_some_and(|c| c.kind == KIND_SQL)
        {
            let policy = self.doc.cell_by_id(&id).map(|c| c.policy.clone());
            let max_rows = policy
                .as_ref()
                .and_then(|p| p.max_rows)
                .map(|n| n.to_string())
                .unwrap_or_default();
            self.add_input(
                &id,
                "max_rows",
                "notebook.local.inspector.max_rows_placeholder",
                max_rows,
                window,
                cx,
                |this, id, value, cx| {
                    let value = value.trim().replace([',', '_'], "");
                    let parsed = if value.is_empty() {
                        Some(None)
                    } else {
                        value.parse::<u64>().ok().filter(|n| *n > 0).map(Some)
                    };
                    match parsed {
                        Some(n) => this.set_max_rows(id, n, cx),
                        None => this.status_message(
                            t("notebook.local.inspector.max_rows_invalid"),
                            true,
                            cx,
                        ),
                    }
                },
            );
            let ttl = match policy.map(|p| p.cache) {
                Some(CachePolicy::Ttl { seconds }) => seconds.to_string(),
                _ => String::new(),
            };
            self.add_input(
                &id,
                "ttl",
                "notebook.local.inspector.ttl_placeholder",
                ttl,
                window,
                cx,
                |this, id, value, cx| {
                    if let Ok(seconds) = value.trim().parse::<u64>()
                        && seconds > 0
                    {
                        this.set_cache_policy(id, CachePolicy::Ttl { seconds }, cx);
                    }
                },
            );
        }
        let attachments: Vec<(String, Option<String>, bool, Option<String>)> = self
            .doc
            .cells()
            .filter(|c| c.kind == KIND_ATTACHMENT)
            .filter_map(|c| {
                let spec = self.doc.attachment(c.id)?;
                let sqlite = spec.format
                    == Some(sqlly_services::notebook::cells::attachment::AttachmentFormat::Sqlite);
                Some((
                    c.id.to_string(),
                    spec.relation.clone(),
                    sqlite,
                    spec.table.clone(),
                ))
            })
            .collect();
        for (id, relation, sqlite, table) in attachments {
            self.add_input(
                &id,
                "relation",
                "notebook.local.attachment.relation_placeholder",
                relation.unwrap_or_default(),
                window,
                cx,
                |this, id, value, cx| {
                    #[cfg(not(target_arch = "wasm32"))]
                    this.set_attachment_relation(id, &value, cx);
                    #[cfg(target_arch = "wasm32")]
                    let _ = (this, id, value, cx);
                },
            );
            if sqlite {
                self.add_input(
                    &id,
                    "table",
                    "notebook.local.attachment.table_placeholder",
                    table.unwrap_or_default(),
                    window,
                    cx,
                    |this, id, value, cx| {
                        #[cfg(not(target_arch = "wasm32"))]
                        this.set_attachment_table(id, &value, cx);
                        #[cfg(target_arch = "wasm32")]
                        let _ = (this, id, value, cx);
                    },
                );
            }
        }
    }

    pub(crate) fn attachment_cx(&self, id: &str) -> Option<AttachmentCx> {
        let spec = self.doc.attachment(id)?;
        Some(AttachmentCx {
            outside_folder: spec.absolute_path.is_some(),
            import: self.local.imports.get(id).cloned(),
            image: self.local.images.get(id).cloned(),
            relation_input: self.input_for(id, "relation"),
            table_input: self.input_for(id, "table"),
            spec,
        })
    }

    // ── Cell header ───────────────────────────────────────────────────────

    /// Badges and the local data menu for a SQL cell's header.
    pub(in crate::notebook) fn local_header_items(
        &mut self,
        cell: &CellRenderCx,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if cell.kind != KIND_SQL {
            return out;
        }
        let id = cell.id.clone();
        let local = self.doc.targets_local(&id);
        if local {
            out.push(local_badge().into_any_element());
            for input_id in self.local_input_cells(&id) {
                let line = self.input_line(&input_id);
                let is_local = self.doc.targets_local(&input_id)
                    || self
                        .doc
                        .cell_by_id(&input_id)
                        .is_some_and(|c| c.kind == KIND_ATTACHMENT);
                let text = if is_local {
                    line.cell_name
                } else {
                    crate::localization::format(
                        "notebook.local.badge.input_source",
                        &[
                            ("cell_name", line.cell_name.as_str()),
                            ("connection_name", line.connection.as_str()),
                            ("environment", &t(environment_label_key(line.environment))),
                        ],
                    )
                };
                let color = if is_local {
                    Theme::text_dim()
                } else {
                    environment_color(line.environment)
                };
                out.push(chip(text, color).into_any_element());
            }
        }
        if let Some(row) = self.local.relation_from_cell(&id)
            && let Some(prov) = &row.provenance
            && prov.kind == SourceKindView::Query
        {
            let age = prov
                .extracted_at
                .map(|at| age_text(at, chrono::Utc::now()))
                .unwrap_or_default();
            let mut text = crate::localization::format(
                if row.rows == 1 {
                    "notebook.local.badge.materialized_one"
                } else {
                    "notebook.local.badge.materialized"
                },
                &[
                    ("relation", row.name.as_str()),
                    ("age", age.as_str()),
                    ("count", count_text(row.rows).as_str()),
                ],
            );
            if !prov.complete {
                text.push_str(" · ");
                text.push_str(&t("notebook.local.badge.partial"));
            }
            out.push(
                chip(
                    text,
                    if prov.complete {
                        Theme::success()
                    } else {
                        Theme::warning()
                    },
                )
                .into_any_element(),
            );
        }
        let index = cell.index;
        let this = cx.entity();
        let menu_id = id.clone();
        out.push(
            icon_button(
                format!("nb-local-menu-{index}"),
                AppIcon::MoreHorizontal,
                t("notebook.local.menu.title"),
            )
            .dropdown_menu(move |menu, _, _| {
                let materialize = this.clone();
                let target = this.clone();
                let mid = menu_id.clone();
                let tid = menu_id.clone();
                menu.item(
                    PopupMenuItem::new(t("notebook.local.menu.materialize")).on_click(
                        move |_, _, app| {
                            materialize.update(app, |v, cx| v.request_materialize(&mid, cx));
                        },
                    ),
                )
                .item(
                    PopupMenuItem::new(t(if local {
                        "notebook.local.menu.run_on_connection"
                    } else {
                        "notebook.local.menu.run_locally"
                    }))
                    .on_click(move |_, _, app| {
                        target.update(app, |v, cx| v.set_cell_local(&tid, !local, cx));
                    }),
                )
            })
            .into_any_element(),
        );
        out
    }

    // ── Under the results ─────────────────────────────────────────────────

    /// The row-cap note with Fetch All, and a local cell's inputs with
    /// where and when each came from.
    pub(in crate::notebook) fn render_local_footer(
        &mut self,
        cell: &CellRenderCx,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if cell.kind != KIND_SQL || cell.collapsed {
            return None;
        }
        let s = Theme::scaled;
        let mut rows: Vec<AnyElement> = Vec::new();
        if let Some(truncation) = self.truncation(&cell.id)
            && !cell.state.is_active()
        {
            let id = cell.id.clone();
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::warning())
                            .child(super::caps_message(truncation)),
                    )
                    .child(
                        text_button(
                            format!("nb-fetch-all-{}", cell.index),
                            t("notebook.local.caps.fetch_all"),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.fetch_all(&id, cx))),
                    )
                    .into_any_element(),
            );
        }
        if self.doc.targets_local(&cell.id) && cell.state == CellStatus::Fresh {
            let inputs = self.doc.local_inputs(&cell.id).unwrap_or_default();
            if !inputs.is_empty() {
                rows.push(small(t("notebook.local.provenance.inputs_title")).into_any_element());
            }
            let now = chrono::Utc::now();
            for input in inputs {
                let line = match self
                    .local
                    .relation(&input.relation)
                    .and_then(|r| r.provenance.as_ref())
                {
                    Some(prov) => provenance_line(&input.relation, prov, now),
                    None => crate::localization::format(
                        "notebook.local.provenance.missing",
                        &[("relation", input.relation.as_str())],
                    ),
                };
                let warn = self
                    .local
                    .relation(&input.relation)
                    .and_then(|r| r.provenance.as_ref())
                    .is_some_and(|p| !p.complete);
                rows.push(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .text_color(if warn {
                            Theme::warning()
                        } else {
                            Theme::text_dim()
                        })
                        .child(line)
                        .into_any_element(),
                );
            }
        }
        if rows.is_empty() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .px(px(s(10.0)))
                .py(px(s(4.0)))
                .border_t_1()
                .border_color(Theme::border())
                .children(rows)
                .into_any_element(),
        )
    }

    // ── Prompts ───────────────────────────────────────────────────────────

    pub(in crate::notebook) fn render_local_prompt(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let prompt = self.local.prompt.clone()?;
        let s = Theme::scaled;
        let mut production = false;
        let (title, body, confirm): (String, Vec<AnyElement>, String) = match &prompt {
            LocalPrompt::Consent {
                label, environment, ..
            } => {
                production = *environment == ConnectionEnvironment::Production;
                (
                    crate::localization::format(
                        "notebook.local.consent.title",
                        &[("connection_name", label.as_str())],
                    ),
                    vec![
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(s(6.0)))
                            .child(small(label.clone()))
                            .child(environment_badge(*environment))
                            .into_any_element(),
                        small(t("notebook.local.consent.detail")).into_any_element(),
                    ],
                    t("notebook.local.consent.allow"),
                )
            }
            LocalPrompt::Inputs { cell_id, inputs } => {
                let mut body: Vec<AnyElement> = inputs
                    .iter()
                    .map(|line| {
                        if line.environment == ConnectionEnvironment::Production {
                            production = true;
                        }
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(6.0)))
                            .child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.85)))
                                    .text_color(Theme::text())
                                    .child(line.cell_name.clone()),
                            )
                            .child(small(line.connection.clone()))
                            .child(environment_badge(line.environment))
                            .into_any_element()
                    })
                    .collect();
                body.push(small(t("notebook.local.inputs.detail")).into_any_element());
                let auto = self.doc.auto_materialize();
                let this = cx.entity();
                body.push(
                    Checkbox::new("nb-local-auto")
                        .checked(auto)
                        .label(t("notebook.local.inputs.always"))
                        .on_click(move |checked: &bool, _, app| {
                            let on = *checked;
                            this.update(app, |v, cx| v.set_auto_materialize(on, cx));
                        })
                        .into_any_element(),
                );
                (
                    crate::localization::format(
                        if inputs.len() == 1 {
                            "notebook.local.inputs.title_one"
                        } else {
                            "notebook.local.inputs.title"
                        },
                        &[
                            ("cell_name", self.cell_label(cell_id)),
                            ("count", inputs.len().to_string()),
                        ],
                    ),
                    body,
                    t("notebook.local.inputs.confirm"),
                )
            }
            LocalPrompt::Name { cell_id } => (
                crate::localization::format(
                    "notebook.local.name.title",
                    &[("cell_name", self.cell_label(cell_id))],
                ),
                vec![
                    self.local_name_input
                        .as_ref()
                        .map(|i| Input::new(i).small().into_any_element())
                        .unwrap_or_else(|| div().into_any_element()),
                    small(t("notebook.local.name.detail")).into_any_element(),
                ],
                t("notebook.local.name.confirm"),
            ),
            LocalPrompt::Refetch { cell_id, .. } => (
                crate::localization::format(
                    "notebook.local.refetch.title",
                    &[("cell_name", self.cell_label(cell_id))],
                ),
                vec![small(t("notebook.local.refetch.detail")).into_any_element()],
                t("notebook.local.refetch.confirm"),
            ),
            LocalPrompt::Clear => (
                t("notebook.local.clear.title"),
                vec![small(t("notebook.local.clear.detail")).into_any_element()],
                t("notebook.local.clear.confirm"),
            ),
        };
        Some(
            div()
                .id("nb-local-scrim")
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
                        .id("nb-local-prompt")
                        .w(px(s(460.0)))
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
                        .child(div().text_color(Theme::text()).child(title))
                        .children(body)
                        .when(production, |d| {
                            d.child(
                                div()
                                    .text_size(px(Theme::font_size_ratio(0.85)))
                                    .text_color(Theme::error())
                                    .child(t("notebook.local.prompt.production")),
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
                                        "nb-local-cancel",
                                        t("notebook.local.prompt.cancel"),
                                    )
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.cancel_local_prompt(cx)),
                                    ),
                                )
                                .child(text_button("nb-local-confirm", confirm).on_click(
                                    cx.listener(|this, _, _, cx| this.confirm_local_prompt(cx)),
                                )),
                        ),
                )
                .into_any_element(),
        )
    }

    // ── Left rail ─────────────────────────────────────────────────────────

    pub(in crate::notebook) fn local_data_page(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        #[cfg(not(target_arch = "wasm32"))]
        self.ensure_local_data_loaded(cx);
        let s = Theme::scaled;
        let mut out: Vec<AnyElement> = Vec::new();
        if !sqlly_services::local_analytics::is_available() {
            out.push(
                small(crate::localization::message(
                    sqlly_services::local_analytics::unavailable_message(),
                ))
                .px(px(s(10.0)))
                .py(px(s(8.0)))
                .into_any_element(),
            );
            return out;
        }
        let quota = self.local.quota_bytes();
        let used = self.local.disk_bytes;
        let fraction = if quota == 0 {
            0.0
        } else {
            (used as f32 / quota as f32).clamp(0.0, 1.0)
        };
        out.push(
            div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .px(px(s(10.0)))
                .py(px(s(6.0)))
                .child(small(crate::localization::format(
                    if self.local.relations.len() == 1 {
                        "notebook.local.data.usage_one"
                    } else {
                        "notebook.local.data.usage"
                    },
                    &[
                        ("used", size_text(used)),
                        ("quota", size_text(quota)),
                        ("count", self.local.relations.len().to_string()),
                    ],
                )))
                .child(
                    div()
                        .h(px(s(4.0)))
                        .w_full()
                        .rounded(px(s(2.0)))
                        .bg(Theme::input_bg())
                        .child(
                            div()
                                .h_full()
                                .w(gpui::relative(fraction))
                                .rounded(px(s(2.0)))
                                .bg(if fraction > 0.9 {
                                    Theme::warning()
                                } else {
                                    Theme::accent()
                                }),
                        ),
                )
                .into_any_element(),
        );
        out.push(self.encryption_switch(cx));
        let this = cx.entity();
        let auto = self.doc.auto_materialize();
        let per = self.local.prefs.per_connection;
        out.push(
            div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .px(px(s(10.0)))
                .pb(px(s(6.0)))
                .child(
                    Checkbox::new("nb-local-auto-rail")
                        .checked(auto)
                        .label(t("notebook.local.data.always_copy_inputs"))
                        .on_click({
                            let this = this.clone();
                            move |checked: &bool, _, app| {
                                let on = *checked;
                                this.update(app, |v, cx| v.set_auto_materialize(on, cx));
                            }
                        }),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(4.0)))
                        .child(small(t("notebook.local.data.per_connection")))
                        .children((1..=4).map(|n| {
                            let this = this.clone();
                            text_button(format!("nb-local-per-{n}"), n.to_string())
                                .when(n == per, |b| b.selected(true))
                                .on_click(move |_, _, app| {
                                    this.update(app, |v, cx| v.set_per_connection(n, cx));
                                })
                        })),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(s(4.0)))
                        .child(
                            text_button("nb-local-refresh", t("notebook.local.data.refresh_all"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    #[cfg(not(target_arch = "wasm32"))]
                                    this.refresh_local_data(cx);
                                    #[cfg(target_arch = "wasm32")]
                                    let _ = (this, cx);
                                })),
                        )
                        .child(
                            text_button("nb-local-clear", t("notebook.local.data.clear")).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.local.prompt = Some(LocalPrompt::Clear);
                                    cx.notify();
                                }),
                            ),
                        )
                        .child(
                            text_button(
                                "nb-local-clear-cache",
                                t("notebook.local.cache.clear_button"),
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.clear_cached_outputs(cx))),
                        ),
                )
                .into_any_element(),
        );
        if let Some(error) = self.local.error.clone() {
            out.push(
                div()
                    .px(px(s(10.0)))
                    .py(px(s(2.0)))
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::error())
                    .child(error)
                    .into_any_element(),
            );
        }
        if self.local.relations.is_empty() {
            out.push(
                small(t("notebook.local.data.empty"))
                    .px(px(s(10.0)))
                    .py(px(s(6.0)))
                    .into_any_element(),
            );
            return out;
        }
        let now = chrono::Utc::now();
        for (n, row) in self.local.relations.clone().into_iter().enumerate() {
            let open = self.local.provenance_open.as_deref() == Some(row.name.as_str());
            let name = row.name.clone();
            let source = row.provenance.as_ref().map(|p| source_text(p, now));
            let mut item = div()
                .id(("nb-local-rel", n))
                .flex()
                .flex_col()
                .px(px(s(10.0)))
                .py(px(s(4.0)))
                .border_t_1()
                .border_color(Theme::border())
                .cursor_pointer()
                .hover(|style| style.bg(Theme::row_hover()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.local.provenance_open =
                        if this.local.provenance_open.as_deref() == Some(name.as_str()) {
                            None
                        } else {
                            Some(name.clone())
                        };
                    cx.notify();
                }))
                .child(
                    div()
                        .truncate()
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text())
                        .child(row.name.clone()),
                )
                .child(small(crate::localization::format(
                    "notebook.local.data.row_detail",
                    &[
                        ("count", count_text(row.rows)),
                        ("columns", row.columns.to_string()),
                        ("size", size_text(row.bytes)),
                    ],
                )))
                .when_some(source, |d, text| d.child(small(text)));
            if open {
                if let Some(prov) = &row.provenance {
                    item = item.child(provenance_popover(prov, now));
                }
                let actions_name = row.name.clone();
                item = item.child(self.relation_actions(n, &actions_name, cx));
            }
            out.push(item.into_any_element());
        }
        out
    }

    /// The Local data page's encryption switch: its state, the switch in
    /// progress, and why it was refused while local tables exist.
    fn encryption_switch(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let this = cx.entity();
        let encrypted = self.local.encrypted;
        let status = if self.local.switching_encryption {
            t("notebook.fu_final.encryption.switching")
        } else if encrypted {
            t("notebook.local.data.encrypted")
        } else {
            t("notebook.local.data.not_encrypted")
        };
        div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .px(px(s(10.0)))
            .pb(px(s(6.0)))
            .child(
                Switch::new("nb-local-encrypt")
                    .checked(encrypted)
                    .label(t("notebook.fu_final.encryption.switch"))
                    .accessibility_label(t("notebook.fu_final.encryption.switch_accessibility"))
                    .on_click(move |on: &bool, _, app| {
                        let on = *on;
                        this.update(app, |v, cx| {
                            #[cfg(not(target_arch = "wasm32"))]
                            v.set_local_encryption(on, cx);
                            #[cfg(target_arch = "wasm32")]
                            let _ = (v, on, cx);
                        });
                    }),
            )
            .child(small(status))
            .when(self.local.encryption_needs_clear, |d| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .text_color(Theme::warning())
                        .child(t("notebook.fu_final.encryption.clear_first")),
                )
                .child(
                    div().flex().flex_row().child(
                        text_button(
                            "nb-local-encrypt-clear",
                            t("notebook.fu_final.encryption.clear_button"),
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.local.prompt = Some(LocalPrompt::Clear);
                            cx.notify();
                        })),
                    ),
                )
            })
            .into_any_element()
    }

    fn relation_actions(&mut self, n: usize, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let refresh = name.to_string();
        let parquet = name.to_string();
        let csv = name.to_string();
        let drop = name.to_string();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(s(4.0)))
            .pt(px(s(4.0)))
            .child(
                text_button(
                    format!("nb-local-rel-refresh-{n}"),
                    t("notebook.local.data.refresh"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    #[cfg(not(target_arch = "wasm32"))]
                    this.refresh_local_relation(&refresh, cx);
                    #[cfg(target_arch = "wasm32")]
                    let _ = (this, &refresh, cx);
                })),
            )
            .child(
                text_button(
                    format!("nb-local-rel-parquet-{n}"),
                    t("notebook.local.data.export_parquet"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    #[cfg(not(target_arch = "wasm32"))]
                    this.export_local_relation(
                        &parquet,
                        sqlly_services::local_analytics::ExportFormat::Parquet,
                        cx,
                    );
                    #[cfg(target_arch = "wasm32")]
                    let _ = (this, &parquet, cx);
                })),
            )
            .child(
                text_button(
                    format!("nb-local-rel-csv-{n}"),
                    t("notebook.local.data.export_csv"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    #[cfg(not(target_arch = "wasm32"))]
                    this.export_local_relation(
                        &csv,
                        sqlly_services::local_analytics::ExportFormat::Csv,
                        cx,
                    );
                    #[cfg(target_arch = "wasm32")]
                    let _ = (this, &csv, cx);
                })),
            )
            .child(
                text_button(
                    format!("nb-local-rel-drop-{n}"),
                    t("notebook.local.data.drop"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    #[cfg(not(target_arch = "wasm32"))]
                    this.drop_local_relation(&drop, cx);
                    #[cfg(target_arch = "wasm32")]
                    let _ = (this, &drop, cx);
                })),
            )
            .into_any_element()
    }

    // ── Inspector ─────────────────────────────────────────────────────────

    /// Row cap, cache policy and cache storage for the active SQL cell.
    pub(in crate::notebook) fn render_local_inspector(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let cell = self.doc.cell_by_id(id)?;
        if cell.kind != KIND_SQL {
            return None;
        }
        let s = Theme::scaled;
        let policy = cell.policy.cache;
        let settings = self.doc.cache_settings(id);
        let local = self.doc.targets_local(id);
        let this = cx.entity();
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .child(small(t("notebook.local.inspector.limits_section")).pt(px(s(10.0))));
        if let Some(input) = self.input_for(id, "max_rows") {
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .child(small(t("notebook.local.inspector.max_rows_label")))
                    .child(div().flex_1().child(Input::new(&input).small())),
            );
        }
        body = body.child(small(t("notebook.local.inspector.cache_section")).pt(px(s(8.0))));
        let modes: [(CachePolicy, &str); 4] = [
            (CachePolicy::Off, "notebook.local.inspector.cache_off"),
            (CachePolicy::Reuse, "notebook.local.inspector.cache_reuse"),
            (
                CachePolicy::Ttl {
                    seconds: match policy {
                        CachePolicy::Ttl { seconds } => seconds,
                        _ => 3600,
                    },
                },
                "notebook.local.inspector.cache_ttl",
            ),
            (CachePolicy::Pinned, "notebook.local.inspector.cache_pinned"),
        ];
        let mut row = div().flex().flex_row().flex_wrap().gap(px(s(4.0)));
        for (n, (mode, key)) in modes.into_iter().enumerate() {
            let selected = std::mem::discriminant(&mode) == std::mem::discriminant(&policy);
            let this = this.clone();
            let cid = id.to_string();
            row = row.child(
                text_button(format!("nb-local-cache-{n}"), t(key))
                    .when(selected, |b| b.selected(true))
                    .on_click(move |_, _, app| {
                        this.update(app, |v, cx| v.set_cache_policy(&cid, mode, cx));
                    }),
            );
        }
        body = body.child(row);
        if matches!(policy, CachePolicy::Ttl { .. })
            && let Some(input) = self.input_for(id, "ttl")
        {
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .child(small(t("notebook.local.inspector.ttl_label")))
                    .child(div().flex_1().child(Input::new(&input).small())),
            );
        }
        if !local {
            let mut storage = div().flex().flex_row().gap(px(s(4.0)));
            for (n, option) in CacheStorage::ALL.into_iter().enumerate() {
                let this = this.clone();
                let cid = id.to_string();
                let key = match option {
                    CacheStorage::Memory => "notebook.local.inspector.storage_memory",
                    CacheStorage::EncryptedDisk => "notebook.local.inspector.storage_encrypted",
                };
                storage = storage.child(
                    text_button(format!("nb-local-storage-{n}"), t(key))
                        .when(settings.storage == option, |b| b.selected(true))
                        .on_click(move |_, _, app| {
                            this.update(app, |v, cx| {
                                v.set_cache_settings(
                                    &cid,
                                    CacheSettings {
                                        storage: option,
                                        ..settings
                                    },
                                    cx,
                                )
                            });
                        }),
                );
            }
            let cid = id.to_string();
            body = body.child(storage).child(
                Checkbox::new("nb-local-clear-on-disconnect")
                    .checked(settings.clear_on_disconnect)
                    .label(t("notebook.local.inspector.clear_on_disconnect"))
                    .on_click(move |checked: &bool, _, app| {
                        let on = *checked;
                        this.update(app, |v, cx| {
                            v.set_cache_settings(
                                &cid,
                                CacheSettings {
                                    clear_on_disconnect: on,
                                    ..settings
                                },
                                cx,
                            )
                        });
                    }),
            );
        }
        body = body.child(small(t(match policy {
            CachePolicy::Ttl { .. } => "notebook.local.inspector.ttl_note",
            CachePolicy::Pinned => "notebook.local.inspector.pinned_note",
            _ => "notebook.local.inspector.cache_note",
        })));
        Some(body.into_any_element())
    }

    // ── Activity ──────────────────────────────────────────────────────────

    /// Copies in progress with Cancel, and the cells running side by side.
    pub(in crate::notebook) fn local_activity_rows(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let s = Theme::scaled;
        let mut out: Vec<AnyElement> = Vec::new();
        let running = self.running_cells();
        if running.len() > 1 {
            let names: Vec<String> = running.iter().map(|id| self.cell_label(id)).collect();
            out.push(
                small(crate::localization::format(
                    "notebook.local.activity.running_together",
                    &[
                        ("count", running.len().to_string()),
                        ("cells", names.join(", ")),
                    ],
                ))
                .px(px(s(10.0)))
                .py(px(s(2.0)))
                .into_any_element(),
            );
        }
        let jobs: Vec<(u64, Option<String>, String, u64)> = self
            .local
            .jobs
            .iter()
            .map(|j| (j.id, j.cell_id.clone(), j.relation.clone(), j.rows()))
            .collect();
        for (job, cell, relation, rows) in jobs {
            let source = cell.map(|c| self.cell_label(&c)).unwrap_or_default();
            out.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .px(px(s(10.0)))
                    .py(px(s(2.0)))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text())
                            .child(crate::localization::format(
                                if rows == 1 {
                                    "notebook.local.activity.copying_one"
                                } else {
                                    "notebook.local.activity.copying"
                                },
                                &[
                                    ("cell_name", source.as_str()),
                                    ("relation", relation.as_str()),
                                    ("count", count_text(rows).as_str()),
                                ],
                            )),
                    )
                    .child(
                        text_button(
                            format!("nb-local-job-{job}"),
                            t("notebook.local.activity.cancel"),
                        )
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.cancel_local_job(job, cx)),
                        ),
                    )
                    .into_any_element(),
            );
        }
        out
    }

    /// Repaint while copies run, so their row counts move.
    pub(crate) fn tick_local_jobs(&mut self, cx: &mut Context<Self>) {
        if self.local.ticker.is_some() {
            return;
        }
        self.local.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                let more = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        !this.local.jobs.is_empty()
                    })
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
            this.update(cx, |this, _| this.local.ticker = None).ok();
        }));
    }
}

/// "orders: Sales · production, extracted 2 min ago, 48,210 rows".
fn provenance_line(
    relation: &str,
    prov: &ProvenanceView,
    now: chrono::DateTime<chrono::Utc>,
) -> String {
    let mut line = crate::localization::format(
        if prov.row_count == 1 {
            "notebook.local.provenance.line_one"
        } else {
            "notebook.local.provenance.line"
        },
        &[
            ("relation", relation.to_string()),
            ("source", source_text(prov, now)),
            ("count", count_text(prov.row_count)),
        ],
    );
    if let Some(partial) = &prov.partial {
        line.push_str(" · ");
        line.push_str(partial);
    } else if !prov.complete {
        line.push_str(" · ");
        line.push_str(&t("notebook.local.badge.partial"));
    }
    line
}

/// Where a relation came from and how long ago, in one phrase.
fn source_text(prov: &ProvenanceView, now: chrono::DateTime<chrono::Utc>) -> String {
    let age = prov
        .extracted_at
        .map(|at| age_text(at, now))
        .unwrap_or_default();
    match prov.kind {
        SourceKindView::File => crate::localization::format(
            "notebook.local.provenance.file",
            &[("file", prov.label.as_str()), ("age", age.as_str())],
        ),
        SourceKindView::Local => crate::localization::format(
            "notebook.local.provenance.derived",
            &[("relations", prov.upstream.join(", ")), ("age", age)],
        ),
        SourceKindView::Query => crate::localization::format(
            "notebook.local.provenance.query",
            &[
                ("connection_name", prov.label.as_str()),
                (
                    "environment",
                    &t(environment_label_key(environment_from(
                        prov.environment.as_deref(),
                    ))),
                ),
                ("age", age.as_str()),
            ],
        ),
    }
}

fn provenance_popover(prov: &ProvenanceView, now: chrono::DateTime<chrono::Utc>) -> AnyElement {
    let s = Theme::scaled;
    let at = |time: Option<chrono::DateTime<chrono::Utc>>| {
        time.map(|t| {
            format!(
                "{} ({})",
                t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S"),
                age_text(t, now)
            )
        })
        .unwrap_or_default()
    };
    let field = |key: &str, value: String| {
        div()
            .flex()
            .flex_row()
            .gap(px(s(6.0)))
            .child(small(t(key)).flex_none())
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text())
                    .child(value),
            )
    };
    let kind = t(match prov.kind {
        SourceKindView::Query => "notebook.local.provenance.kind_query",
        SourceKindView::File => "notebook.local.provenance.kind_file",
        SourceKindView::Local => "notebook.local.provenance.kind_local",
    });
    let mut card = div()
        .id("nb-local-provenance")
        .flex()
        .flex_col()
        .gap(px(s(2.0)))
        .mt(px(s(4.0)))
        .p(px(s(6.0)))
        .rounded(px(s(4.0)))
        .border_1()
        .border_color(Theme::popup_border())
        .bg(Theme::popup_bg())
        .child(field("notebook.local.provenance.kind_label", kind))
        .child(field(
            "notebook.local.provenance.source_label",
            prov.label.clone(),
        ));
    if let Some(env) = &prov.environment {
        card = card.child(field(
            "notebook.local.provenance.environment_label",
            t(environment_label_key(environment_from(Some(env)))),
        ));
    }
    if let Some(db) = &prov.database {
        card = card.child(field(
            "notebook.local.provenance.database_label",
            db.clone(),
        ));
    }
    if let Some(cell) = &prov.cell_id {
        card = card.child(field("notebook.local.provenance.cell_label", cell.clone()));
    }
    if !prov.upstream.is_empty() {
        card = card.child(field(
            "notebook.local.provenance.upstream_label",
            prov.upstream.join(", "),
        ));
    }
    card = card
        .child(field(
            "notebook.local.provenance.extracted_label",
            at(prov.extracted_at),
        ))
        .child(field(
            "notebook.local.provenance.materialized_label",
            at(prov.materialized_at),
        ))
        .child(field(
            "notebook.local.provenance.rows_label",
            count_text(prov.row_count),
        ))
        .child(field(
            "notebook.local.provenance.complete_label",
            match &prov.partial {
                Some(reason) => reason.clone(),
                None if prov.complete => t("notebook.local.provenance.complete_yes"),
                None => t("notebook.local.badge.partial"),
            },
        ));
    if let Some(hash) = &prov.sql_hash {
        card = card.child(field(
            "notebook.local.provenance.sql_hash_label",
            hash.chars().take(19).collect(),
        ));
    }
    if let Some(sum) = &prov.checksum {
        card = card.child(field(
            "notebook.local.provenance.checksum_label",
            sum.chars().take(24).collect(),
        ));
    }
    let fallbacks: Vec<&str> = prov
        .columns
        .iter()
        .filter(|(_, _, fell)| *fell)
        .map(|(name, _, _)| name.as_str())
        .collect();
    if !fallbacks.is_empty() {
        card = card.child(
            div()
                .text_size(px(Theme::font_size_ratio(0.8)))
                .text_color(Theme::warning())
                .child(crate::localization::format(
                    "notebook.local.provenance.fell_back",
                    &[("columns", fallbacks.join(", "))],
                )),
        );
    }
    card.into_any_element()
}
