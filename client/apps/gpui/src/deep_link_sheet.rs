//! Confirmation sheet for `sqlly://` deep links and SQLite file opens.

use gpui::{
    AppContext as _, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement,
    Render, Styled, Window, div, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::Button;
use gpui_component::input::{Input, InputState};
use sqlly_core::{DeepLink, deep_link::describe_message};

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;

const SQL_PREVIEW_LINES: usize = 12;

#[derive(Clone, Debug)]
pub struct DeepLinkSheetModel {
    pub link: DeepLink,
    pub description: String,
    pub connection_found: bool,
    pub sql_preview: Option<String>,
    pub error: Option<String>,
    /// Connection URL links: the saved connection the URL matched, if any.
    pub matched_connection: Option<String>,
    /// Connection URL links with no password and no saved match: the sheet
    /// shows a password field.
    pub asks_password: bool,
}

impl DeepLinkSheetModel {
    pub fn new(link: DeepLink, connection_found: bool) -> Self {
        let sql_preview = match &link {
            DeepLink::Query { sql, .. } => Some(truncate_sql_preview(sql)),
            _ => None,
        };
        Self {
            description: crate::localization::message(describe_message(&link)),
            link,
            connection_found,
            sql_preview,
            error: None,
            matched_connection: None,
            asks_password: false,
        }
    }

    /// A raw connection URL link (Gridex #18). `matched_connection` names
    /// the saved connection it points at; without one, and without a
    /// password in the URL, the sheet asks for the password.
    pub fn new_connect_url(link: DeepLink, matched_connection: Option<String>) -> Self {
        let asks_password = matches!(
            &link,
            DeepLink::ConnectUrl { parsed }
                if parsed.password.is_none()
                    && matched_connection.is_none()
                    && password_applies(parsed.engine)
        );
        Self {
            matched_connection,
            asks_password,
            ..Self::new(link, true)
        }
    }

    /// The link to carry out, with the password typed into the sheet (if
    /// any) placed into a connection URL that had none.
    pub fn confirmed_link(&self, typed_password: Option<String>) -> DeepLink {
        let mut link = self.link.clone();
        if self.asks_password
            && let DeepLink::ConnectUrl { parsed } = &mut link
            && let Some(password) = typed_password.filter(|p| !p.is_empty())
        {
            parsed.password = Some(password);
        }
        link
    }

    /// The note under the description of a connection URL link: which saved
    /// connection it uses, or that the connection is not saved.
    pub fn connect_url_note(&self) -> Option<String> {
        if !matches!(self.link, DeepLink::ConnectUrl { .. }) {
            return None;
        }
        Some(match &self.matched_connection {
            Some(name) => crate::localization::format(
                "connections.deep_link.connect_url_saved_note",
                &[("connection_name", name)],
            ),
            None => crate::localization::text("connections.deep_link.connect_url_unsaved_note"),
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        match &self.link {
            DeepLink::Open { connection, .. } | DeepLink::Query { connection, .. } => {
                if !self.connection_found {
                    return Err(crate::localization::format(
                        "connections.deep_link.missing_connection",
                        &[("connection_name", connection)],
                    ));
                }
            }
            DeepLink::OpenDatabaseFile { path, engine } => {
                if !path.exists() {
                    return Err(crate::localization::format(
                        "connections.deep_link.missing_database_file",
                        &[
                            ("engine_name", engine.display_name()),
                            ("path", &path.to_string_lossy()),
                        ],
                    ));
                }
            }
            DeepLink::NewConnection { .. } | DeepLink::ConnectUrl { .. } => {}
        }
        Ok(())
    }
}

/// Engines whose URL sign-in takes a password (file engines take none).
fn password_applies(engine: sqlly_core::DatabaseType) -> bool {
    !matches!(
        engine,
        sqlly_core::DatabaseType::Sqlite | sqlly_core::DatabaseType::DuckDb
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeepLinkSheetEvent {
    /// Boxed: `DeepLink` carries a full parsed connection URL, and the
    /// no-data `Cancelled` variant would otherwise pay its size.
    Confirmed(Box<DeepLink>),
    Cancelled,
}

pub struct DeepLinkSheet {
    model: DeepLinkSheetModel,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    /// Masked password field for a connection URL without one; made at
    /// first render (it needs the window). Its text is read on Open and
    /// never stored anywhere else.
    password_input: Option<Entity<InputState>>,
}

impl EventEmitter<DeepLinkSheetEvent> for DeepLinkSheet {}

impl Focusable for DeepLinkSheet {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl DeepLinkSheet {
    pub fn new(model: DeepLinkSheetModel, cx: &mut gpui::Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        Self {
            model,
            focus_handle,
            frame: DialogFrame::new_auto(Theme::scaled(480.0)),
            password_input: None,
        }
    }
}

impl Render for DeepLinkSheet {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let validation = self.model.validate().err();
        if self.model.asks_password && self.password_input.is_none() {
            self.password_input = Some(cx.new(|cx| InputState::new(window, cx).masked(true)));
        }
        let connect_url_note = self.model.connect_url_note().map(|note| {
            div()
                .text_size(px(Theme::font_size_ratio(0.80)))
                .text_color(Theme::text_dim())
                .whitespace_normal()
                .child(note)
        });
        let password_section = self.password_input.clone().map(|input| {
            div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "connections.deep_link.password_label",
                        )),
                )
                .child(Input::new(&input).mask_toggle())
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.75)))
                        .text_color(Theme::text_dim())
                        .whitespace_normal()
                        .child(crate::localization::text(
                            "connections.deep_link.password_hint",
                        )),
                )
        });

        let preview_section = self.model.sql_preview.clone().map(|preview| {
            div()
                .flex()
                .flex_col()
                .gap(px(s(6.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "connections.deep_link.sql_preview_note",
                        )),
                )
                .child(
                    div()
                        .p(px(s(8.0)))
                        .bg(Theme::sidebar_bg())
                        .border_1()
                        .border_color(Theme::border())
                        .rounded(px(s(4.0)))
                        .max_h(px(s(180.0)))
                        .overflow_hidden()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .font_family(Theme::font_family())
                        .text_color(Theme::kind_string())
                        .whitespace_normal()
                        .child(preview),
                )
        });
        let validation_note = validation.clone().map(|message| {
            div()
                .text_size(px(Theme::font_size_ratio(0.87)))
                .text_color(Theme::error())
                .whitespace_normal()
                .child(message)
        });

        let focus_handle = self.focus_handle.clone();
        Dialog::new("deep-link-sheet", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("connections.deep_link.title"))
            .track_focus(&focus_handle)
            .dismiss_on_scrim_click(false)
            .on_close(|_, _window, cx| {
                cx.emit(DeepLinkSheetEvent::Cancelled);
            })
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .whitespace_normal()
                    .child(self.model.description.clone()),
            )
            .children_body(connect_url_note)
            .children_body(password_section)
            .children_body(preview_section)
            .children_body(validation_note)
            .button(
                Button::new("deep-link-open")
                    .label(crate::localization::text(
                        "connections.deep_link.open_button",
                    ))
                    .disabled(validation.is_some())
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if this.model.validate().is_ok() {
                            let typed = this
                                .password_input
                                .as_ref()
                                .map(|input| input.read(cx).value().to_string());
                            cx.emit(DeepLinkSheetEvent::Confirmed(Box::new(
                                this.model.confirmed_link(typed),
                            )));
                        }
                    })),
            )
            .build(self, window, cx)
    }
}

fn truncate_sql_preview(sql: &str) -> String {
    let mut lines = Vec::new();
    for line in sql.lines().take(SQL_PREVIEW_LINES) {
        lines.push(line);
    }
    let mut preview = lines.join("\n");
    if sql.lines().count() > SQL_PREVIEW_LINES {
        preview.push_str("\n…");
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::DeepLink;
    use std::path::PathBuf;

    #[test]
    fn open_link_without_matching_connection_fails_validation() {
        let model = DeepLinkSheetModel::new(
            DeepLink::Open {
                connection: "missing".into(),
                database: None,
                table: None,
            },
            false,
        );
        assert_eq!(
            model.validate(),
            Err("No connection named \"missing\" — add it first".into())
        );
    }

    #[test]
    fn query_sheet_preview_is_truncated_to_twelve_lines() {
        let sql = (0..20)
            .map(|line| format!("SELECT {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let model = DeepLinkSheetModel::new(
            DeepLink::Query {
                connection: "prod".into(),
                database: None,
                sql,
            },
            true,
        );
        let preview = model.sql_preview.expect("preview");
        assert_eq!(preview.lines().count(), 13);
        assert!(preview.ends_with('…'));
    }

    /// Gridex #18: a connection URL without a password and without a saved
    /// match asks for one and carries what was typed; a URL with a password,
    /// or one that names a saved connection, does not ask.
    #[test]
    fn connect_url_sheet_asks_for_a_missing_password_only() {
        let link = sqlly_core::parse_deep_link("mysql://app@server/sales").unwrap();
        let model = DeepLinkSheetModel::new_connect_url(link.clone(), None);
        assert!(model.asks_password);
        assert!(model.validate().is_ok());
        let DeepLink::ConnectUrl { parsed } = model.confirmed_link(Some("typed".into())) else {
            panic!("connect link");
        };
        assert_eq!(parsed.password.as_deref(), Some("typed"));
        let DeepLink::ConnectUrl { parsed } = model.confirmed_link(Some(String::new())) else {
            panic!("connect link");
        };
        assert_eq!(parsed.password, None);
        assert_eq!(
            model.connect_url_note().as_deref(),
            Some("This connection isn't saved; it stays in the sidebar until you quit SQLLY.")
        );

        let model = DeepLinkSheetModel::new_connect_url(link, Some("Sales DB".into()));
        assert!(!model.asks_password);
        assert_eq!(
            model.connect_url_note().as_deref(),
            Some("Uses your saved connection \"Sales DB\".")
        );

        let with_password = sqlly_core::parse_deep_link("mysql://app:pw@server/sales").unwrap();
        let model = DeepLinkSheetModel::new_connect_url(with_password, None);
        assert!(!model.asks_password);
        assert!(!model.description.contains("pw"));
    }

    #[test]
    fn sqlite_link_to_missing_file_fails_validation() {
        let model = DeepLinkSheetModel::new(
            DeepLink::OpenDatabaseFile {
                path: PathBuf::from("/tmp/definitely-missing-plan-197.sqlite"),
                engine: sqlly_core::DatabaseType::Sqlite,
            },
            true,
        );
        assert!(model.validate().is_err());
    }
}
