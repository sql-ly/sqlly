//! Attachment cells: a data file the user picked, imported into a named
//! local relation and previewed in a grid with its row count and detected
//! column types, or a PNG/JPEG image shown inline (checked to be a real
//! image under the size limit first).

use gpui::{
    AnyElement, Context, IntoElement, ObjectFit, ParentElement as _, Styled as _, StyledImage as _,
    Window, div, img, prelude::FluentBuilder as _, px,
};
use gpui_component::Sizable as _;
use gpui_component::input::Input;

use super::{CellRenderCx, NotebookCell};
use crate::notebook::local::{count_text, size_text};
use crate::notebook::rails::text_button;
use crate::notebook::view::{CellStatus, NotebookView};
use crate::theme::Theme;

pub struct AttachmentCell;

/// Tallest an inline image is drawn, in unscaled pixels.
const MAX_IMAGE_HEIGHT: f32 = 420.0;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

fn note(text: String, color: gpui::Hsla) -> gpui::Div {
    div()
        .text_size(px(Theme::font_size_ratio(0.85)))
        .text_color(color)
        .child(text)
}

impl NotebookCell for AttachmentCell {
    fn kind(&self) -> &'static str {
        "attachment"
    }

    fn label_key(&self) -> &'static str {
        "notebook.local.attachment.kind"
    }

    fn runnable(&self) -> bool {
        true
    }

    fn render_body(
        &self,
        cell: &CellRenderCx,
        _window: &mut Window,
        cx: &mut Context<NotebookView>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let Some(at) = cell.attachment.clone() else {
            return div().into_any_element();
        };
        if cell.collapsed {
            return div().into_any_element();
        }
        let spec = &at.spec;
        let file = spec
            .file_name
            .clone()
            .unwrap_or_else(|| t("notebook.local.attachment.no_file_chosen"));
        let detail = match (spec.format, spec.size_bytes) {
            (Some(format), Some(size)) => format!("{} · {}", format.label(), size_text(size)),
            (Some(format), None) => format.label().to_string(),
            _ => String::new(),
        };
        let id = cell.id.clone();
        let mut controls = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(8.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text())
                    .child(file),
            )
            .when(!detail.is_empty(), |d| {
                d.child(note(detail, Theme::text_dim()))
            })
            .child(
                text_button(
                    format!("nb-attach-choose-{}", cell.index),
                    t("notebook.local.attachment.choose"),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    #[cfg(not(target_arch = "wasm32"))]
                    this.choose_attachment_file(&id, cx);
                    #[cfg(target_arch = "wasm32")]
                    let _ = (this, &id, cx);
                })),
            );
        if !spec.is_image()
            && let Some(input) = &at.relation_input
        {
            controls = controls
                .child(note(
                    t("notebook.local.attachment.relation_label"),
                    Theme::text_dim(),
                ))
                .child(div().w(px(s(160.0))).child(Input::new(input).small()));
        }
        if let Some(input) = &at.table_input {
            controls = controls
                .child(note(
                    t("notebook.local.attachment.table_label"),
                    Theme::text_dim(),
                ))
                .child(div().w(px(s(140.0))).child(Input::new(input).small()));
        }
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .px(px(s(10.0)))
            .py(px(s(6.0)))
            .when(!cell.output_only, |d| d.child(controls));
        if at.outside_folder {
            body = body.child(note(
                t("notebook.local.attachment.outside_note"),
                Theme::text_dim(),
            ));
        }
        if let Some(import) = &at.import {
            if import.changed {
                body = body.child(note(
                    t("notebook.local.attachment.changed"),
                    Theme::warning(),
                ));
            }
            body = body.child(note(
                crate::localization::format(
                    "notebook.local.attachment.summary",
                    &[
                        ("relation", import.relation.clone()),
                        ("count", count_text(import.rows)),
                        ("columns", import.columns.len().to_string()),
                    ],
                ),
                Theme::text(),
            ));
            let schema: Vec<String> = import
                .columns
                .iter()
                .map(|(name, ty, fell)| {
                    if *fell {
                        crate::localization::format(
                            "notebook.local.attachment.column_text",
                            &[("column", name.as_str()), ("type", ty.as_str())],
                        )
                    } else {
                        format!("{name} {ty}")
                    }
                })
                .collect();
            body = body.child(note(schema.join(", "), Theme::text_dim()));
            if let Some(partial) = &import.partial {
                body = body.child(note(partial.clone(), Theme::warning()));
            }
        }
        if let Some(message) = &cell.message {
            body = body.child(note(
                message.clone(),
                if matches!(cell.state, CellStatus::Failed | CellStatus::Blocked) {
                    Theme::error()
                } else {
                    Theme::text_dim()
                },
            ));
        }
        if let Some(image) = &at.image {
            let scale = if image.height as f32 > MAX_IMAGE_HEIGHT {
                MAX_IMAGE_HEIGHT / image.height as f32
            } else {
                1.0
            };
            body = body.child(
                img(image.image.clone())
                    .w(px(s(image.width as f32 * scale)))
                    .h(px(s(image.height as f32 * scale)))
                    .object_fit(ObjectFit::Contain),
            );
            return body.into_any_element();
        }
        div()
            .flex()
            .flex_col()
            .w_full()
            .child(body)
            .children(super::sql::results_pane(cell, cx))
            .into_any_element()
    }
}
