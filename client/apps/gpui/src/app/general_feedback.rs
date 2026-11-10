//! Send Feedback wiring on `AppView`: capture the window, open the detached
//! annotation dialog, and run the background upload queue.
//!
//! Other code opens the dialog through [`AppView::open_send_feedback`] (or by
//! dispatching the [`SendFeedback`] action — the Help menu item, the
//! `help.sendFeedback` palette command, and its shortcut all do).

use gpui::{Context, Window};

use super::{AppView, SendFeedback, StatusLevel};

/// Guards against a second capture starting while one is in flight (a
/// double-pressed shortcut would otherwise open two dialogs).
#[cfg(not(target_arch = "wasm32"))]
static CAPTURE_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

impl AppView {
    pub(super) fn handle_send_feedback(
        &mut self,
        _action: &SendFeedback,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_send_feedback(window, cx);
    }

    /// Open Send Feedback for this window: screenshot the window as it looks
    /// right now (at native resolution), then open the annotation dialog in
    /// its own window. The capture waits one short beat first so whatever
    /// triggered it — the command palette, a menu — has closed and the
    /// workbench has repainted without it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn open_send_feedback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use std::sync::atomic::Ordering;

        if let Some(message) = crate::intellisense_feedback::feedback_upload().disabled_message() {
            self.notify_feature_disabled(message, cx);
            return;
        }
        if CAPTURE_IN_FLIGHT.swap(true, Ordering::SeqCst) {
            return;
        }
        let context = self.feedback_context();
        self.set_status(
            StatusLevel::Busy,
            crate::localization::text("shell.feedback.capturing_screenshot"),
        );
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(200))
                .await;
            let target = this
                .update_in(cx, |_, window, _| crate::screenshot::capture_target(window))
                .unwrap_or_else(|_| {
                    crate::screenshot::CaptureTarget::Unavailable("the window closed".into())
                });
            let captured = cx
                .background_executor()
                .spawn(async move {
                    crate::screenshot::capture(&target)
                        .map(crate::general_feedback_dialog::CapturedScreenshot::new)
                })
                .await;
            let _ = this.update(cx, |app, cx| {
                CAPTURE_IN_FLIGHT.store(false, Ordering::SeqCst);
                if let Err(reason) = &captured {
                    tracing::info!("send feedback: no screenshot ({reason})");
                }
                app.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.feedback.window_opened"),
                );
                cx.notify();
                // Opening a window renders it immediately — never inside
                // this entity's own update.
                let weak_app = cx.weak_entity();
                cx.defer(move |cx| {
                    let Some(dialog) = crate::general_feedback_dialog::GeneralFeedbackDialog::open(
                        captured, context, cx,
                    ) else {
                        let _ = weak_app.update(cx, |app, cx| {
                            app.set_status(
                                StatusLevel::Error,
                                crate::localization::text("shell.feedback.window_open_failed"),
                            );
                            cx.notify();
                        });
                        return;
                    };
                    let _ = weak_app.update(cx, |_, cx| {
                        cx.subscribe(&dialog, Self::handle_general_feedback_event)
                            .detach();
                    });
                });
            });
            // Never leave the guard stuck if the app view went away.
            CAPTURE_IN_FLIGHT.store(false, Ordering::SeqCst);
        })
        .detach();
    }

    /// Browser build: no OS windows to capture and no upload path.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn open_send_feedback(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_status(
            StatusLevel::Info,
            crate::browser_preview::unavailable(&crate::localization::text(
                "shell.feedback.browser_unavailable",
            )),
        );
        cx.notify();
    }

    /// Where the user is: the active connection's engine and the kind of
    /// view in front of them.
    #[cfg(not(target_arch = "wasm32"))]
    fn feedback_context(&self) -> crate::general_feedback::FeedbackContext {
        let active_view = self.active_tab().map(|tab| {
            if tab.notebook.is_some() {
                "notebook"
            } else if tab.ddl_view.is_some() {
                "ddl-viewer"
            } else if tab.snapshot_only {
                "result-snapshot"
            } else {
                "query-editor"
            }
            .to_string()
        });
        crate::general_feedback::FeedbackContext {
            active_connection_engine: self
                .active_profile
                .as_ref()
                .map(|p| p.database_type.display_name().to_string()),
            active_view: active_view.or_else(|| Some("welcome".to_string())),
            // Filled in at send time, off the UI thread.
            settings: None,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn handle_general_feedback_event(
        &mut self,
        _dialog: gpui::Entity<crate::general_feedback_dialog::GeneralFeedbackDialog>,
        event: &crate::general_feedback_dialog::GeneralFeedbackEvent,
        cx: &mut Context<Self>,
    ) {
        let crate::general_feedback_dialog::GeneralFeedbackEvent::Submitted(submitted) = event;
        // The event hands out a reference; move the (Arc-backed) capture out
        // by cloning the cheap parts.
        let submitted = crate::general_feedback::SubmittedFeedback {
            message: submitted.message.clone(),
            screenshot: submitted.screenshot.clone(),
            annotations: submitted.annotations.clone(),
            context: submitted.context.clone(),
        };
        self.queue_general_feedback(submitted, cx);
    }

    /// A sent dialog (already closed): snapshot the (scrubbed) settings,
    /// flatten the annotations into the capture and encode the PNG off the UI
    /// thread, write the report to its pending file stamped with a fresh
    /// GUID, then try the upload at once.
    /// A failed upload leaves the file for the background worker
    /// (`feedback_uploader::sync`), which retries every five minutes until
    /// the server accepts it.
    #[cfg(not(target_arch = "wasm32"))]
    fn queue_general_feedback(
        &mut self,
        submitted: crate::general_feedback::SubmittedFeedback,
        cx: &mut Context<Self>,
    ) {
        self.set_status(
            StatusLevel::Busy,
            crate::localization::text("shell.feedback.sending"),
        );
        cx.notify();

        let device_id = crate::store::pairing::load().map(|p| p.device_id);
        let weak_self = cx.weak_entity();
        let mut async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let outcome = crate::tokio_handle()
                .spawn(async move {
                    let record = tokio::task::spawn_blocking(move || {
                        let mut submitted = submitted;
                        let mut settings = crate::settings_snapshot::capture();
                        settings.fit_within(crate::general_feedback::MAX_SETTINGS_JSON_BYTES);
                        submitted.context.settings = Some(settings);
                        let draft = submitted.finish()?;
                        let request = crate::general_feedback::build_request(
                            draft,
                            uuid::Uuid::new_v4().to_string(),
                            device_id,
                            crate::general_feedback::os_version(),
                        );
                        let record = crate::general_feedback::PendingGeneralFeedback {
                            created_at: chrono::Utc::now(),
                            attempts: 0,
                            next_attempt_at: None,
                            request,
                        };
                        crate::general_feedback::save(&record);
                        Ok::<_, String>(record)
                    })
                    .await
                    .map_err(|e| e.to_string())??;
                    Ok::<_, String>(crate::feedback_uploader::sync::upload_general(record).await)
                })
                .await
                .map_err(|e| e.to_string())
                .and_then(|inner| inner);
            let _ = weak_self.update(&mut async_cx, |this, cx| {
                let (level, text) = feedback_status(outcome, "Feedback");
                this.set_status(level, text);
                cx.notify();
            });
        })
        .detach();
    }
}

/// Status-bar line for the first upload attempt of a just-submitted report
/// (`what` is "Feedback" or "IntelliSense feedback").
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn feedback_status(
    outcome: Result<crate::feedback_uploader::sync::Outcome, String>,
    what: &str,
) -> (StatusLevel, String) {
    use crate::feedback_uploader::{retry_interval_label, sync::Outcome};
    let what = if what == "Feedback" {
        crate::localization::text("shell.feedback.kind_general")
    } else {
        crate::localization::text("shell.feedback.kind_intellisense")
    };
    match outcome {
        Ok(Outcome::Sent) => (
            StatusLevel::Success,
            crate::localization::format("shell.feedback.sent", &[("kind", &what)]),
        ),
        Ok(Outcome::Skipped) => (
            StatusLevel::Info,
            crate::localization::format("shell.feedback.saved_sending", &[("kind", &what)]),
        ),
        Ok(Outcome::Pending(reason)) => (
            StatusLevel::Warning,
            crate::localization::format(
                "shell.feedback.saved_retry",
                &[
                    ("kind", &what),
                    ("reason", &crate::localization::message(reason)),
                    ("interval", &retry_interval_label()),
                ],
            ),
        ),
        Ok(Outcome::Deferred { reason, until }) => (
            StatusLevel::Warning,
            crate::localization::format(
                "shell.feedback.saved_later",
                &[
                    ("kind", &what),
                    ("reason", &crate::localization::message(reason)),
                    (
                        "when",
                        &until
                            .with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M")
                            .to_string(),
                    ),
                ],
            ),
        ),
        Ok(Outcome::Rejected(reason)) => (
            StatusLevel::Error,
            crate::localization::format(
                "shell.feedback.rejected",
                &[
                    ("kind", &what),
                    ("reason", &crate::localization::message(reason)),
                ],
            ),
        ),
        Ok(Outcome::Dropped) => (
            StatusLevel::Error,
            crate::localization::format("shell.feedback.save_failed", &[("kind", &what)]),
        ),
        Err(reason) => (
            StatusLevel::Error,
            crate::localization::format(
                "shell.feedback.prepare_failed",
                &[("kind", &what), ("reason", &reason)],
            ),
        ),
    }
}
