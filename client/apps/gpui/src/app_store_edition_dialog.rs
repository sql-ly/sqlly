//! Help ▸ Mac App Store Edition: what the sandboxed App Store edition can't
//! do, and what to do instead. Shown once automatically on the edition's
//! first launch so nobody discovers a missing feature by surprise, then
//! always reachable from the Help menu and the command palette. The list
//! itself lives in [`crate::distribution::APP_STORE_LIMITATIONS`].

use gpui::{Context, EventEmitter, IntoElement, Render, Styled, Window, div, prelude::*, px};
use gpui_component::button::{Button, ButtonVariants as _};

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub enum AppStoreEditionEvent {
    Closed,
}

pub struct AppStoreEditionDialog {
    frame: DialogFrame,
}

impl EventEmitter<AppStoreEditionEvent> for AppStoreEditionDialog {}

pub fn intro() -> String {
    crate::localization::text("app_store_edition.intro")
}

fn limitation_keys(index: usize) -> (&'static str, &'static str) {
    const KEYS: [(&str, &str); 11] = [
        (
            "app_store_edition.limitations.updates.title",
            "app_store_edition.limitations.updates.detail",
        ),
        (
            "app_store_edition.limitations.terminal.title",
            "app_store_edition.limitations.terminal.detail",
        ),
        (
            "app_store_edition.limitations.ai_tools.title",
            "app_store_edition.limitations.ai_tools.detail",
        ),
        (
            "app_store_edition.limitations.mcp.title",
            "app_store_edition.limitations.mcp.detail",
        ),
        (
            "app_store_edition.limitations.kubernetes.title",
            "app_store_edition.limitations.kubernetes.detail",
        ),
        (
            "app_store_edition.limitations.editors.title",
            "app_store_edition.limitations.editors.detail",
        ),
        (
            "app_store_edition.limitations.git.title",
            "app_store_edition.limitations.git.detail",
        ),
        (
            "app_store_edition.limitations.duckdb.title",
            "app_store_edition.limitations.duckdb.detail",
        ),
        (
            "app_store_edition.limitations.files.title",
            "app_store_edition.limitations.files.detail",
        ),
        (
            "app_store_edition.limitations.data.title",
            "app_store_edition.limitations.data.detail",
        ),
        (
            "app_store_edition.limitations.icloud.title",
            "app_store_edition.limitations.icloud.detail",
        ),
    ];
    KEYS[index]
}

/// Where the first-launch showing is recorded.
const SHOWN_ARTIFACT: &str = "app-store-edition-notice-shown.txt";

/// Whether the first-launch showing is due: App Store edition only, once.
pub fn first_launch_notice_due() -> bool {
    crate::distribution::is_app_store()
        && crate::host_storage::load_text(crate::host_storage::StorageArea::Data, SHOWN_ARTIFACT)
            .is_none()
}

/// Record that the first-launch showing happened.
pub fn record_first_launch_notice() {
    let _ = crate::host_storage::store_text(
        crate::host_storage::StorageArea::Data,
        SHOWN_ARTIFACT,
        "shown",
    );
}

impl AppStoreEditionDialog {
    pub fn new() -> Self {
        Self {
            frame: DialogFrame::new_auto(Theme::scaled(560.0)),
        }
    }
}

impl Default for AppStoreEditionDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl Render for AppStoreEditionDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let mut list = div().flex().flex_col().gap(px(s(8.0)));
        for (index, _) in crate::distribution::APP_STORE_LIMITATIONS
            .iter()
            .enumerate()
        {
            let (title_key, detail_key) = limitation_keys(index);
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(1.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(Theme::text())
                            .child(crate::localization::format(
                                "app_store_edition.limitation_item",
                                &[("title", &crate::localization::text(title_key))],
                            )),
                    )
                    .child(
                        div()
                            .pl(px(s(12.0)))
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(detail_key)),
                    ),
            );
        }
        Dialog::new("app-store-edition", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("app_store_edition.dialog.title"))
            .easy_close(true)
            .on_close(|_this, _window, cx| cx.emit(AppStoreEditionEvent::Closed))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(12.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .line_height(gpui::relative(1.4))
                            .child(intro()),
                    )
                    .child(list),
            )
            .leading_button(
                Button::new("app-store-edition-guide")
                    .label(crate::localization::text(
                        "app_store_edition.actions.read_guide",
                    ))
                    .on_click(|_, _window, _cx| {
                        let _ = crate::browser_open::open_in_browser(
                            crate::distribution::MACOS_DOCS_URL,
                        );
                    }),
            )
            .button(
                Button::new("app-store-edition-ok")
                    .primary()
                    .label(crate::localization::text("app_store_edition.actions.ok"))
                    .on_click(
                        cx.listener(|_this, _, _window, cx| cx.emit(AppStoreEditionEvent::Closed)),
                    ),
            )
            .build(self, window, cx)
    }
}

/// An inline note for a settings pane or panel whose feature the App Store
/// edition can't offer: names it plainly and points at Help ▸ Mac App Store
/// Edition. Callers render it only when the feature is unavailable.
pub fn edition_notice(text: &str) -> gpui::AnyElement {
    let s = Theme::scaled;
    div()
        .p(px(s(8.0)))
        .rounded(px(s(4.0)))
        .border_1()
        .border_color(Theme::warning())
        .text_size(px(Theme::font_size_ratio(0.88)))
        .text_color(Theme::text())
        .child(crate::localization::format(
            "app_store_edition.inline_notice",
            &[("feature", text)],
        ))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intro_names_the_edition_and_the_reason() {
        let intro = intro();
        assert!(intro.contains("Mac App Store edition"));
        assert!(intro.contains("App Sandbox"));
    }

    #[cfg(not(feature = "app-store"))]
    #[test]
    fn first_launch_notice_never_fires_outside_the_app_store_edition() {
        assert!(!first_launch_notice_due());
    }
}
