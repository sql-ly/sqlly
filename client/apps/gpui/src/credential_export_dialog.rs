//! The picker behind both "Export Connections…" and "Export Conn w/
//! Creds…": a server → database tree of saved connections with checkboxes at
//! both levels, so any or all servers and any or all databases under a
//! server can be exported. Both write the same connection file; the
//! credentials mode opens with a warning that the file holds passwords in
//! clear text. "Export for Other Tools" reuses the same picker, listing only
//! the connections the chosen tool can open, and writes that tool's format.

use std::collections::{BTreeSet, HashSet};

use gpui::{
    Context, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render, Styled,
    Window, div, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::scroll::ScrollableElement;
use sqlly_core::ConnectionProfile;
use sqlly_core::tool_export::ToolExportFormat;
use uuid::Uuid;

use crate::credential_export::{ExportSecrets, ExportServerNode, export_tree};
use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialExportDialogEvent {
    Export {
        ids: Vec<Uuid>,
        secrets: ExportSecrets,
        /// Set for Export for Other Tools: the format to write instead of a
        /// SQLLY connection file. Such a file never holds secrets.
        tool: Option<ToolExportFormat>,
    },
    Cancel,
}

impl EventEmitter<CredentialExportDialogEvent> for CredentialExportDialog {}

pub struct CredentialExportDialog {
    focus_handle: FocusHandle,
    secrets: ExportSecrets,
    tool: Option<ToolExportFormat>,
    servers: Vec<ExportServerNode>,
    selected: HashSet<Uuid>,
    /// Indices of servers whose databases are hidden.
    collapsed: BTreeSet<usize>,
    focus_requested: bool,
    frame: DialogFrame,
}

impl Focusable for CredentialExportDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Selection rules, kept free of UI so they can be tested directly.
pub(crate) fn server_state(server: &ExportServerNode, selected: &HashSet<Uuid>) -> (usize, usize) {
    let picked = server
        .databases
        .iter()
        .filter(|d| selected.contains(&d.id))
        .count();
    (picked, server.databases.len())
}

/// Clicking a server's checkbox selects every database under it, or clears
/// them all when every one is already selected.
pub(crate) fn toggle_server(server: &ExportServerNode, selected: &mut HashSet<Uuid>) {
    let (picked, total) = server_state(server, selected);
    let select = picked < total;
    for database in &server.databases {
        if select {
            selected.insert(database.id);
        } else {
            selected.remove(&database.id);
        }
    }
}

impl CredentialExportDialog {
    pub fn new(
        profiles: &[ConnectionProfile],
        secrets: ExportSecrets,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_tool(profiles, secrets, None, cx)
    }

    /// The picker for Export for Other Tools: `profiles` should already be
    /// narrowed to the engines `tool` can open.
    pub fn for_tool(
        profiles: &[ConnectionProfile],
        tool: ToolExportFormat,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::with_tool(profiles, ExportSecrets::Omit, Some(tool), cx)
    }

    fn with_tool(
        profiles: &[ConnectionProfile],
        secrets: ExportSecrets,
        tool: Option<ToolExportFormat>,
        cx: &mut Context<Self>,
    ) -> Self {
        let servers = export_tree(profiles);
        let selected = servers
            .iter()
            .flat_map(|s| s.databases.iter().map(|d| d.id))
            .collect();
        Self {
            focus_handle: cx.focus_handle(),
            secrets,
            tool,
            servers,
            selected,
            collapsed: BTreeSet::new(),
            focus_requested: false,
            frame: DialogFrame::new_auto(Theme::scaled(560.0)),
        }
    }

    fn total(&self) -> usize {
        self.servers.iter().map(|s| s.databases.len()).sum()
    }

    fn set_all(&mut self, select: bool, cx: &mut Context<Self>) {
        if select {
            self.selected = self
                .servers
                .iter()
                .flat_map(|s| s.databases.iter().map(|d| d.id))
                .collect();
        } else {
            self.selected.clear();
        }
        cx.notify();
    }

    fn toggle_server(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(server) = self.servers.get(index) {
            toggle_server(server, &mut self.selected);
        }
        cx.notify();
    }

    fn toggle_database(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if !self.selected.remove(&id) {
            self.selected.insert(id);
        }
        cx.notify();
    }

    fn toggle_collapsed(&mut self, index: usize, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&index) {
            self.collapsed.insert(index);
        }
        cx.notify();
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        if self.selected.is_empty() {
            return;
        }
        // Keep tree order so the file lists connections as the picker did.
        let ids = self
            .servers
            .iter()
            .flat_map(|s| s.databases.iter().map(|d| d.id))
            .filter(|id| self.selected.contains(id))
            .collect();
        cx.emit(CredentialExportDialogEvent::Export {
            ids,
            secrets: self.secrets,
            tool: self.tool,
        });
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        cx.emit(CredentialExportDialogEvent::Cancel);
    }
}

impl Render for CredentialExportDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.focus_requested {
            self.focus_requested = true;
            window.focus(&self.focus_handle, cx);
        }

        let s = Theme::scaled;
        let n = self.selected.len();
        let total = self.total();
        let all_selected = n == total && total > 0;

        let (heading, body_text) = match self.tool {
            Some(tool) => (None, tool_intro(tool)),
            None => intro(self.secrets),
        };
        let intro = match heading {
            Some(heading) => div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .p(px(s(10.0)))
                .rounded(px(s(4.0)))
                .border_2()
                .border_color(Theme::warning())
                .bg(Theme::warning().opacity(0.12))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(1.05)))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Theme::warning())
                        .child(heading),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.88)))
                        .text_color(Theme::text())
                        .child(body_text),
                ),
            None => div()
                .text_color(Theme::text_dim())
                .text_size(px(Theme::font_size_ratio(0.80)))
                .child(body_text),
        };

        let selected_count = |key: &str, selected: usize, total: usize| {
            crate::localization::format(
                key,
                &[
                    ("selected_count", &selected.to_string()),
                    ("total_count", &total.to_string()),
                ],
            )
        };
        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(selected_count(
                        "connections.credentials.export_total_selected",
                        n,
                        total,
                    )),
            )
            .child(
                Button::new("cx-all")
                    .label(crate::localization::text(if all_selected {
                        "connections.credentials.export_clear_selection"
                    } else {
                        "connections.credentials.export_select_all"
                    }))
                    .ghost()
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.set_all(!all_selected, cx);
                    })),
            );

        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        for (server_ix, server) in self.servers.iter().enumerate() {
            let (picked, count) = server_state(server, &self.selected);
            let collapsed = self.collapsed.contains(&server_ix);
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(Theme::row_height()))
                    .gap(px(s(6.0)))
                    .child(
                        Button::new(("cx-fold", server_ix))
                            .label(if collapsed { "▸" } else { "▾" })
                            .ghost()
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.toggle_collapsed(server_ix, cx);
                            })),
                    )
                    .child(
                        Checkbox::new(("cx-server", server_ix))
                            .checked(picked == count && count > 0)
                            .label(server.label.clone())
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.toggle_server(server_ix, cx);
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.87)))
                            .child(if picked > 0 && picked < count {
                                selected_count(
                                    "connections.credentials.export_server_partial_selected",
                                    picked,
                                    count,
                                )
                            } else if count == 1 {
                                crate::localization::text(
                                    "connections.credentials.export_one_database",
                                )
                            } else {
                                crate::localization::format(
                                    "connections.credentials.export_database_count",
                                    &[("count", &count.to_string())],
                                )
                            }),
                    )
                    .into_any_element(),
            );
            if collapsed {
                continue;
            }
            for (db_ix, database) in server.databases.iter().enumerate() {
                let id = database.id;
                rows.push(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .h(px(Theme::row_height()))
                        .pl(px(s(40.0)))
                        .gap(px(s(8.0)))
                        .child(
                            Checkbox::new(("cx-db", server_ix * 10_000 + db_ix))
                                .checked(self.selected.contains(&id))
                                .label(database.label.clone())
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.toggle_database(id, cx);
                                })),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_color(Theme::text_dim())
                                .text_size(px(Theme::font_size_ratio(0.87)))
                                .child(database.detail.clone()),
                        )
                        .into_any_element(),
                );
            }
        }

        let body = div()
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .max_h(px(s(380.0)))
            .overflow_y_scrollbar()
            .children(rows);

        let export_key = if n == 1 {
            "connections.credentials.export_one_connection_button"
        } else {
            "connections.credentials.export_many_connections_button"
        };
        let export_label = crate::localization::format(export_key, &[("count", &n.to_string())]);
        let focus_handle = self.focus_handle.clone();
        Dialog::new("credential-export-dialog", |this: &mut Self| {
            &mut this.frame
        })
        .title(match self.tool {
            Some(tool) => crate::localization::format(
                "connections.credentials.export_for_tool_title",
                &[("tool", tool.tool_name())],
            ),
            None => crate::localization::text(match self.secrets {
                ExportSecrets::Include => "connections.credentials.export_with_secrets_title",
                ExportSecrets::Omit => "connections.credentials.export_without_secrets_title",
            }),
        })
        .key_context("CredentialExportDialog")
        .track_focus(&focus_handle)
        .dismiss_on_scrim_click(false)
        .on_close(|this, _window, cx| this.cancel(cx))
        .child(intro)
        .child(toolbar)
        .child(body)
        .button(
            Button::new("cx-export")
                .label(export_label)
                .primary()
                .disabled(n == 0)
                .on_click(cx.listener(|this, _, _window, cx| this.export(cx))),
        )
        .build(self, window, cx)
    }
}

/// The text above the tree: a warning heading for the credentials export
/// (shown as a banner at the top of the dialog), a plain note otherwise.
pub(crate) fn intro(secrets: ExportSecrets) -> (Option<String>, String) {
    match secrets {
        ExportSecrets::Include => (
            Some(crate::localization::text(
                "connections.credentials.export_warning_title",
            )),
            crate::localization::text("connections.credentials.export_warning_body"),
        ),
        ExportSecrets::Omit => (
            None,
            crate::localization::text("connections.credentials.export_without_secrets_body"),
        ),
    }
}

/// The note above the tree in Export for Other Tools.
pub(crate) fn tool_intro(tool: ToolExportFormat) -> String {
    crate::localization::format(
        "connections.credentials.export_for_tool_body",
        &[("tool", tool.tool_name()), ("file_name", tool.file_name())],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential_export::ExportDatabaseNode;

    #[test]
    fn tool_export_intro_names_the_tool_and_file_and_promises_no_passwords() {
        let body = tool_intro(ToolExportFormat::PgAdminServersJson);
        assert!(body.contains("pgAdmin"), "{body}");
        assert!(body.contains("servers.json"), "{body}");
        assert!(body.contains("no passwords"), "{body}");
    }

    #[test]
    fn only_the_credentials_export_warns_about_clear_text() {
        let (heading, body) = intro(ExportSecrets::Include);
        assert!(heading.unwrap().contains("passwords in clear text"));
        assert!(body.contains("Anyone who gets the file"));
        let (heading, body) = intro(ExportSecrets::Omit);
        assert!(heading.is_none());
        assert!(body.contains("no passwords"));
    }

    fn server(ids: &[Uuid]) -> ExportServerNode {
        ExportServerNode {
            label: "h:5432 · PostgreSQL".into(),
            databases: ids
                .iter()
                .map(|id| ExportDatabaseNode {
                    id: *id,
                    label: id.to_string(),
                    detail: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn server_checkbox_selects_all_then_clears() {
        let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let node = server(&ids);
        let mut selected: HashSet<Uuid> = [ids[0]].into_iter().collect();
        assert_eq!(server_state(&node, &selected), (1, 3));
        toggle_server(&node, &mut selected);
        assert_eq!(server_state(&node, &selected), (3, 3), "partial → all");
        toggle_server(&node, &mut selected);
        assert_eq!(server_state(&node, &selected), (0, 3), "all → none");
    }

    #[test]
    fn server_toggle_leaves_other_servers_alone() {
        let a = server(&[Uuid::new_v4()]);
        let other = Uuid::new_v4();
        let mut selected: HashSet<Uuid> = [other].into_iter().collect();
        toggle_server(&a, &mut selected);
        toggle_server(&a, &mut selected);
        assert!(selected.contains(&other));
    }
}
