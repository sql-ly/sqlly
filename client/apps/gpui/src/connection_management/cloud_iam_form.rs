use super::*;

impl ConnectionManagement {
    pub(super) fn maybe_prefill_aws_region_from_server(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        let _ = (window, cx);
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.auth_mode != AuthMode::AwsRdsIam || !self.aws_region.trim().is_empty() {
                return;
            }
            let Some(region) = crate::cloud_iam_token::aws_region_from_host(&self.server) else {
                return;
            };
            self.aws_region = region.clone();
            self.aws_region_input
                .update(cx, |state, cx| state.set_value(region, window, cx));
            self.mark_dirty();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn ensure_aws_profiles_loaded(&mut self, cx: &mut Context<Self>) {
        if self.aws_profiles_loaded {
            return;
        }
        self.aws_profiles_loaded = true;
        let mut async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let profiles = crate::tokio_handle()
                .spawn(async { sqlly_services::cloud_auth::aws::list_profiles().await })
                .await
                .unwrap_or_default();
            let mut labels = vec![crate::cloud_iam_token::aws_profile_default_label()];
            labels.extend(profiles);
            let _ = weak.update(&mut async_cx, |this, cx| {
                this.aws_profile_options = labels;
                this.aws_profile_select_stale = true;
                cx.notify();
            });
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn sync_aws_profile_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.aws_profile_options.is_empty() {
            self.aws_profile_options = vec![crate::cloud_iam_token::aws_profile_default_label()];
        }
        let row = crate::cloud_iam_token::aws_profile_select_row(
            &self.aws_profile,
            &self.aws_profile_options,
        );
        let items = crate::connection_editor::unique_select_labels(
            self.aws_profile_options.iter().cloned(),
        );
        self.aws_profile_select.update(cx, |state, cx| {
            state.set_items(items, window, cx);
            state.set_selected_index(row.map(gpui_component::IndexPath::new), window, cx);
        });
        self.aws_profile_select_stale = false;
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn refresh_gcp_adc_status(&mut self, cx: &mut Context<Self>) {
        let mut async_cx = cx.to_async();
        let weak = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            // Only the probe runs on the runtime; the status is worded here,
            // where the UI thread's language is active.
            let status = match crate::tokio_handle()
                .spawn(async {
                    sqlly_services::cloud_auth::gcp::adc_status()
                        .await
                        .map(drop)
                        .map_err(|error| error.to_string())
                })
                .await
            {
                Ok(Ok(())) => crate::localization::text("connections.management.gcp_adc_found"),
                Ok(Err(detail)) => crate::localization::format(
                    "connections.management.gcp_adc_error",
                    &[("error", detail.as_str())],
                ),
                Err(_) => crate::localization::text("connections.management.gcp_adc_check_failed"),
            };
            let _ = weak.update(&mut async_cx, |this, cx| {
                this.gcp_adc_status = status;
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn apply_iam_auth_mode_side_effects(
        &mut self,
        mode: AuthMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(mode, AuthMode::AwsRdsIam | AuthMode::GcpCloudSqlIam) {
            self.encrypt = true;
        }
        match mode {
            AuthMode::AwsRdsIam => {
                self.username_input.update(cx, |state, cx| {
                    state.set_placeholder("jane_doe", window, cx);
                });
                #[cfg(not(target_arch = "wasm32"))]
                self.ensure_aws_profiles_loaded(cx);
                self.maybe_prefill_aws_region_from_server(window, cx);
            }
            AuthMode::GcpCloudSqlIam => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let placeholder =
                        crate::cloud_iam_token::gcp_iam_username_placeholder(self.database_type);
                    self.username_input.update(cx, |state, cx| {
                        state.set_placeholder(placeholder, window, cx);
                    });
                    self.refresh_gcp_adc_status(cx);
                }
            }
            AuthMode::SqlLogin => {
                self.username_input.update(cx, |state, cx| {
                    state.set_placeholder("sa", window, cx);
                });
            }
            _ => {}
        }
    }

    pub(super) fn render_aws_iam_hint(&self) -> AnyElement {
        div()
            .px(px(Theme::scaled(12.0)))
            .py(px(Theme::scaled(8.0)))
            .text_size(px(Theme::font_size()))
            .text_color(Theme::text_dim())
            .child(crate::localization::text(
                "connections.management.aws_iam_hint",
            ))
            .into_any_element()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn render_gcp_adc_status_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = self.gcp_adc_status.clone();
        div()
            .px(px(Theme::scaled(12.0)))
            .py(px(Theme::scaled(8.0)))
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(6.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(if status.is_empty() {
                        crate::localization::text("connections.management.gcp_adc_checking")
                    } else {
                        status
                    }),
            )
            .child(
                Button::new("cm-gcp-adc-check")
                    .ghost()
                    .small()
                    .label(crate::localization::text(
                        "connections.management.gcp_adc_check_again",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.refresh_gcp_adc_status(cx);
                    })),
            )
            .into_any_element()
    }
}
