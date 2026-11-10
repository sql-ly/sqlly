//! Workspace ▸ iCloud Sync pane (macOS): the on/off switch (off by
//! default), what syncs and what never does, how conflicts resolve, and
//! the latest status with a Sync Now button.

use super::*;

#[cfg(not(target_arch = "wasm32"))]
thread_local! {
    /// Why the last on/off change could not be saved, shown under the box.
    static SAVE_ERROR: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

impl PreferencesWindow {
    pub(super) fn render_icloud_sync(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = crate::localization::text;
        if !cfg!(target_os = "macos") {
            return self.pane_container(vec![self.hint(&t("icloud_sync.settings.unsupported"))]);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let offered = crate::icloud_sync::is_offered();
            let enabled = offered && crate::icloud_sync::is_enabled();
            let mut rows = vec![
                self.heading(&t("settings.navigation.workspace.icloud_sync")),
                self.intro(&t("icloud_sync.settings.intro")),
            ];
            if offered {
                rows.push(self.checkbox(
                    "icloud_sync_enabled",
                    &t("icloud_sync.settings.enable_label"),
                    enabled,
                    cx,
                    |this, cx| this.toggle_icloud_sync(cx),
                ));
            } else {
                rows.push(self.checkbox_disabled(
                    "icloud_sync_enabled",
                    &t("icloud_sync.settings.enable_label"),
                    false,
                    true,
                    cx,
                    |_, _| {},
                ));
                rows.push(self.warning(&t("icloud_sync.settings.app_store_unavailable")));
            }
            if let Some(error) = SAVE_ERROR.with(|e| e.borrow().clone()) {
                rows.push(self.warning(&error));
            }
            rows.push(self.hint(&t("icloud_sync.settings.off_hint")));

            if enabled {
                rows.push(self.section_header(&t("icloud_sync.settings.section_status")));
                let status = crate::icloud_sync::runtime::status();
                let summary = status
                    .summary
                    .as_ref()
                    .map(crate::localization::message)
                    .unwrap_or_else(|| t("icloud_sync.status.waiting"));
                rows.push(if status.is_problem {
                    self.warning(&summary)
                } else {
                    self.hint(&summary)
                });
                for note in &status.notes {
                    rows.push(self.hint(&crate::localization::message(note)));
                }
                rows.push(
                    div()
                        .flex()
                        .flex_row()
                        .child(self.quiet_button(
                            "icloud_sync_now",
                            &t("icloud_sync.settings.sync_now"),
                            cx,
                            |this, _window, cx| this.icloud_sync_now(cx),
                        ))
                        .into_any_element(),
                );
            }

            rows.push(self.section_header(&t("icloud_sync.settings.section_syncs")));
            for key in [
                "icloud_sync.settings.syncs_connections",
                "icloud_sync.settings.syncs_snippets",
                "icloud_sync.settings.syncs_library",
                "icloud_sync.settings.syncs_settings",
            ] {
                rows.push(self.hint(&t(key)));
            }
            rows.push(self.section_header(&t("icloud_sync.settings.section_never")));
            rows.push(self.hint(&t("icloud_sync.settings.never_secrets")));
            rows.push(self.hint(&t("icloud_sync.settings.never_local")));
            rows.push(self.section_header(&t("icloud_sync.settings.section_conflicts")));
            rows.push(self.hint(&t("icloud_sync.settings.conflicts_hint")));
            rows.push(self.hint(&t("icloud_sync.settings.restart_hint")));
            self.pane_container(rows)
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = cx;
            self.pane_container(vec![self.hint(&t("icloud_sync.settings.unsupported"))])
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn toggle_icloud_sync(&mut self, cx: &mut Context<Self>) {
        let enable = !crate::icloud_sync::is_enabled();
        let result = crate::icloud_sync::set_enabled(enable);
        SAVE_ERROR.with(|slot| {
            *slot.borrow_mut() = result.err().map(|error| {
                crate::localization::format(
                    "icloud_sync.settings.save_failed",
                    &[("error", &error.to_string())],
                )
            });
        });
        if enable {
            crate::icloud_sync::runtime::start();
        }
        self.refresh_icloud_status_soon(cx);
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn icloud_sync_now(&mut self, cx: &mut Context<Self>) {
        crate::icloud_sync::runtime::sync_now();
        self.refresh_icloud_status_soon(cx);
        cx.notify();
    }

    /// The pass runs on its own thread; repaint a few times so its result
    /// shows up without the user having to move the mouse.
    #[cfg(not(target_arch = "wasm32"))]
    fn refresh_icloud_status_soon(&mut self, cx: &mut Context<Self>) {
        let entity = cx.entity();
        cx.spawn(async move |_this, cx| {
            for delay in [1u64, 3, 8] {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(delay))
                    .await;
                entity.update(cx, |_this, cx| cx.notify());
            }
        })
        .detach();
    }
}
