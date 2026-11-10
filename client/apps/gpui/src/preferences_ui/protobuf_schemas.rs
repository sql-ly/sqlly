//! Protobuf Schemas pane: register `.proto` files or compiled descriptor
//! sets so the value viewer can decode Protobuf values into named fields.
//! The schemas live in `protobuf-schemas.json` (see `protobuf_schema.rs`).

use super::*;

/// The pane's last outcome ("Added shop.proto", or why a file was refused).
/// Process-wide rather than a window field: it is one line of feedback for
/// the one pane that writes it.
#[cfg(not(target_arch = "wasm32"))]
static STATUS: std::sync::Mutex<Option<(bool, String)>> = std::sync::Mutex::new(None);

#[cfg(not(target_arch = "wasm32"))]
fn set_status(ok: bool, text: String) {
    if let Ok(mut status) = STATUS.lock() {
        *status = Some((ok, text));
    }
}

impl PreferencesWindow {
    pub(super) fn render_protobuf_schemas(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut children = vec![
            self.heading(&crate::localization::text(
                "settings.protobuf_schemas.heading",
            )),
            self.intro(&crate::localization::text(
                "settings.protobuf_schemas.intro",
            )),
        ];
        #[cfg(target_arch = "wasm32")]
        {
            let _ = cx;
            children.push(self.hint(&crate::localization::text(
                "settings.protobuf_schemas.browser_unavailable",
            )));
            self.pane_container(children)
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            children.push(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(Theme::scaled(8.)))
                    .child(self.quiet_button(
                        "protobuf_add_proto",
                        &crate::localization::text("settings.protobuf_schemas.add_proto_button"),
                        cx,
                        |this, _window, cx| {
                            this.add_protobuf_schema(crate::protobuf_schema::SchemaKind::Proto, cx)
                        },
                    ))
                    .child(self.quiet_button(
                        "protobuf_add_descriptor_set",
                        &crate::localization::text(
                            "settings.protobuf_schemas.add_descriptor_set_button",
                        ),
                        cx,
                        |this, _window, cx| {
                            this.add_protobuf_schema(
                                crate::protobuf_schema::SchemaKind::DescriptorSet,
                                cx,
                            )
                        },
                    ))
                    .child(self.quiet_button(
                        "protobuf_reload",
                        &crate::localization::text("settings.protobuf_schemas.reload_button"),
                        cx,
                        |_this, _window, cx| {
                            crate::protobuf_schema::reload_schemas();
                            cx.notify();
                        },
                    ))
                    .into_any_element(),
            );
            if let Some((ok, text)) = STATUS.lock().ok().and_then(|status| status.clone()) {
                children.push(if ok {
                    self.hint(&text)
                } else {
                    self.warning(&text)
                });
            }
            children.push(self.section_header(&crate::localization::text(
                "settings.protobuf_schemas.registered_header",
            )));
            let schemas = crate::protobuf_schema::schemas();
            if schemas.is_empty() {
                children.push(self.hint(&crate::localization::text(
                    "settings.protobuf_schemas.none_registered",
                )));
            }
            for (index, summary) in schemas.into_iter().enumerate() {
                children.push(self.protobuf_schema_row(index, summary, cx));
            }
            children.push(self.hint(&crate::localization::text(
                "settings.protobuf_schemas.usage_hint",
            )));
            self.pane_container(children)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn protobuf_schema_row(
        &self,
        index: usize,
        summary: crate::protobuf_schema::SchemaSummary,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        const LISTED: usize = 12;
        let detail = match &summary.messages {
            Ok(messages) => {
                let mut shown: Vec<&str> =
                    messages.iter().take(LISTED).map(String::as_str).collect();
                let more = messages.len().saturating_sub(LISTED);
                let more_text;
                if more > 0 {
                    more_text = crate::localization::format(
                        "settings.protobuf_schemas.more_messages",
                        &[("count", &more.to_string())],
                    );
                    shown.push(&more_text);
                }
                self.hint(&crate::localization::format(
                    "settings.protobuf_schemas.message_types",
                    &[
                        ("count", &messages.len().to_string()),
                        ("messages", &shown.join(", ")),
                    ],
                ))
            }
            Err(error) => self.warning(error),
        };
        let id = summary.entry.id.clone();
        div()
            .flex()
            .flex_row()
            .items_start()
            .justify_between()
            .gap(px(Theme::scaled(12.)))
            .py(px(Theme::scaled(4.)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(self.label_row(&summary.entry.display_name()))
                    .child(self.hint(&summary.entry.path))
                    .child(detail),
            )
            .child(self.quiet_button(
                SharedString::from(format!("protobuf_remove_{index}")),
                &crate::localization::text("settings.protobuf_schemas.remove_button"),
                cx,
                move |_this, _window, cx| {
                    match crate::protobuf_schema::remove_schema(&id) {
                        Ok(()) => {
                            if let Ok(mut status) = STATUS.lock() {
                                *status = None;
                            }
                        }
                        Err(error) => set_status(false, error),
                    }
                    cx.notify();
                },
            ))
            .into_any_element()
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn add_protobuf_schema(
        &mut self,
        kind: crate::protobuf_schema::SchemaKind,
        cx: &mut Context<Self>,
    ) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                let mut added = Vec::new();
                let mut errors = Vec::new();
                for path in paths {
                    match crate::protobuf_schema::add_schema(&path, kind) {
                        Ok(summary) => added.push(summary.entry.display_name()),
                        Err(error) => errors.push(error),
                    }
                }
                if !errors.is_empty() {
                    set_status(false, errors.join("\n"));
                } else if !added.is_empty() {
                    set_status(
                        true,
                        crate::localization::format(
                            "settings.protobuf_schemas.added",
                            &[("files", &added.join(", "))],
                        ),
                    );
                }
                this.update(cx, |_this, cx| cx.notify()).ok();
            }
        })
        .detach();
    }
}
