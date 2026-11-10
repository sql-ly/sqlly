use std::collections::HashMap;

use gpui::Entity;

#[derive(Clone)]
pub(crate) struct PendingMutationReview {
    pub(crate) sql: String,
    pub(crate) tab_id: String,
    pub(crate) bottom_line: String,
    pub(crate) has_unbounded: bool,
    /// ADR-008: whether this run's effective safety policy requires a
    /// generated AI review. While required-and-absent, Execute is disabled —
    /// the gate fails closed if the review cannot be produced.
    pub(crate) ai_review_required: bool,
    pub(crate) production: bool,
    /// The generated plain-language review, once the local model answers.
    pub(crate) ai_explanation: Option<String>,
    /// Monotonic token tying an in-flight review to this dialog instance; a
    /// review that completes after cancel/re-run is dropped.
    pub(crate) ai_generation: u64,
    /// When set, confirming this review re-enters execute for a notebook cell
    /// rather than the tab's hidden editor/results pair.
    pub(crate) notebook_cell: Option<String>,
    /// When set, confirming runs on these connection ids instead of the active profile.
    pub(crate) multi_targets: Option<Vec<uuid::Uuid>>,
}

/// Pending resume-connection confirmation, mirroring the Swift
/// `pendingResumeConnection` / `hasPendingResumeConnectionConfirmation`
/// flow. When a tab has a remembered connection that differs from the
/// active one, query execution arms this confirmation so the user can
/// switch to the remembered connection before running.
#[derive(Clone)]
pub(crate) struct PendingResumeConnectionConfirmation {
    pub(crate) tab_idx: usize,
    pub(crate) server: String,
    pub(crate) database: Option<String>,
}

/// An armed connection-profile deletion awaiting confirmation. The fields are
/// snapshotted at arm time so the dialog can name exactly what is about to be
/// destroyed rather than asking "are you sure?" about nothing in particular.
#[derive(Clone)]
pub(crate) struct PendingConnectionDeletion {
    pub(crate) id: uuid::Uuid,
    pub(crate) name: String,
    pub(crate) server: String,
    pub(crate) environment: String,
}

/// What SQLLY can offer to do about a firewall block it has just recognised.
#[derive(Clone)]
pub(crate) enum FirewallPromptPlan {
    /// Everything the ARM call needs is in place: the profile is bound to an
    /// Entra account, and that account is registered on this machine.
    Ready {
        account_id: sqlly_services::security::azure::EntraAccountId,
        /// UPN shown in the prompt so the user knows whose permissions decide
        /// whether the change is allowed.
        account_label: Option<String>,
        default_rule_name: String,
    },
    /// SQLLY cannot make the call at all — say why instead of offering a form
    /// whose only possible ending is a failure.
    Unavailable(String),
}

/// A recognised error-40615 block waiting to be shown as a dialog.
#[derive(Clone)]
pub(crate) struct PendingFirewallPrompt {
    pub(crate) server: String,
    /// The client IP the server itself named in the error, when it named one.
    pub(crate) blocked_ip: Option<String>,
    pub(crate) plan: FirewallPromptPlan,
}

#[derive(Clone)]
pub(crate) struct PendingStagedSave {
    pub(crate) batch: sqlly_services::row_editing::StagedBatch,
    pub(crate) rollback_sql: String,
    pub(crate) preview_sql: String,
    pub(crate) results: Entity<crate::results::ResultsView>,
    pub(crate) bottom_line: String,
    pub(crate) ai_review_required: bool,
    pub(crate) ai_explanation: Option<String>,
    pub(crate) ai_generation: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransactionCloseReason {
    CloseTab,
    /// Carries the profile being disconnected so the resume-after-confirm
    /// path disconnects the server the action was invoked on — not whichever
    /// connection happens to be focused by the time the user confirms.
    Disconnect(uuid::Uuid),
    SwitchToAuto,
}

#[derive(Clone)]
pub(crate) struct PendingTransactionClose {
    pub(crate) tab_id: String,
    pub(crate) tab_name: String,
    pub(crate) statements: u32,
    pub(crate) reason: TransactionCloseReason,
}

/// Armed Reload/Keep or Keep/Close prompt for a file-backed tab.
#[derive(Clone)]
pub(crate) struct PendingDiskChange {
    pub(crate) tab_id: String,
    pub(crate) state: crate::file_watch::DiskState,
    pub(crate) current: crate::file_watch::WatchSnapshot,
}

/// Armed macOS Open in Terminal notice. The launch is fully built (keyring
/// resolved) and waits for the user to acknowledge that the password rides in
/// a short-lived launcher file; cancelling drops it, secret and all.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone)]
pub(crate) struct PendingTerminalNotice {
    pub(crate) name: String,
    pub(crate) launch: crate::terminal_launch::CliLaunch,
    pub(crate) dont_show_again: bool,
}

/// Where an Open in Terminal request is, for the status bar. The status must
/// only claim a terminal is opening once one really is: while the macOS notice
/// waits for the user nothing is being launched, and a cancelled notice
/// launched nothing at all.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TerminalLaunchPhase {
    /// Resolving credentials, finding the CLI, opening any tunnel.
    Preparing,
    /// The launcher-file notice is on screen; nothing is launched yet.
    AwaitingNotice,
    /// Confirmed (or no notice needed): the terminal is being spawned.
    Launching,
    /// The terminal started; `notes` are the launch's caveats, if any.
    Launched { notes: Vec<String> },
    /// The user backed out of the notice.
    Cancelled,
}

/// The status bar line for `phase` of opening `name` in a terminal. The idle
/// phases (notice waiting, cancelled) clear the status back to "Ready".
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn terminal_launch_status(
    name: &str,
    phase: &TerminalLaunchPhase,
) -> (super::StatusLevel, String) {
    use super::StatusLevel;
    match phase {
        TerminalLaunchPhase::Preparing => (
            StatusLevel::Busy,
            crate::localization::format("shell.terminal_launch.preparing", &[("name", name)]),
        ),
        TerminalLaunchPhase::AwaitingNotice | TerminalLaunchPhase::Cancelled => (
            StatusLevel::Info,
            crate::localization::text("shell.status.terminal_launch_ready"),
        ),
        TerminalLaunchPhase::Launching => (
            StatusLevel::Busy,
            crate::localization::format("shell.terminal_launch.opening", &[("name", name)]),
        ),
        TerminalLaunchPhase::Launched { notes } if notes.is_empty() => (
            StatusLevel::Success,
            crate::localization::format("shell.terminal_launch.opened", &[("name", name)]),
        ),
        TerminalLaunchPhase::Launched { notes } => (
            StatusLevel::Info,
            crate::localization::format(
                "shell.terminal_launch.opened_with_notes",
                &[("name", name), ("notes", &notes.join(" "))],
            ),
        ),
    }
}

/// Body of the macOS Open in Terminal notice: the headline, why the launcher
/// file exists, the four facts about it, and where to turn the note back on.
/// Every line wraps at the card's width (the bullets through
/// [`crate::dialog::bullet_item`]).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn terminal_notice_body(name: &str, tool: &str) -> Vec<gpui::Div> {
    use crate::theme::Theme;
    use gpui::{div, prelude::*, px};
    let bullets = [
        crate::localization::text("shell.terminal_notice.private_script"),
        crate::localization::format(
            "shell.terminal_notice.cleanup_script",
            &[
                ("tool", tool),
                (
                    "minutes",
                    &crate::terminal_launch::LAUNCHER_SWEEP_MINUTES.to_string(),
                ),
            ],
        ),
        crate::localization::text("shell.terminal_notice.no_command_line_password"),
        crate::localization::format(
            "shell.terminal_notice.remove_saved_password",
            &[("tool", tool)],
        ),
    ];
    vec![
        div()
            .text_size(px(Theme::font_size_ratio(0.87)))
            .text_color(Theme::text())
            .child(crate::localization::format(
                "shell.terminal_notice.password_on_disk",
                &[("name", name)],
            )),
        div()
            .text_size(px(Theme::font_size_ratio(0.80)))
            .text_color(Theme::text_dim())
            .child(crate::localization::text(
                "shell.terminal_notice.launcher_explanation",
            )),
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(4.0)))
            .children(bullets.into_iter().enumerate().map(|(i, text)| {
                crate::dialog::bullet_item(text)
                    .debug_selector(move || format!("terminal-notice-bullet-{i}"))
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text())
            })),
        div()
            .debug_selector(|| "terminal-notice-footnote".to_string())
            .text_size(px(Theme::font_size_ratio(0.80)))
            .text_color(Theme::text_dim())
            .child(crate::localization::text(
                "shell.terminal_notice.re_enable_hint",
            )),
    ]
}

#[derive(Default)]
pub(crate) struct PendingDialogs {
    pub(crate) mutation_review: Option<PendingMutationReview>,
    pub(crate) staged_save: Option<PendingStagedSave>,
    pub(crate) resume_confirmation: Option<PendingResumeConnectionConfirmation>,
    pub(crate) connection_deletion: Option<PendingConnectionDeletion>,
    pub(crate) firewall_prompt: Option<PendingFirewallPrompt>,
    pub(crate) transaction_close: Option<PendingTransactionClose>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) terminal_notice: Option<PendingTerminalNotice>,
    /// One prompt at a time: set when a disk change should be shown from render.
    pub(crate) disk_change: Option<PendingDiskChange>,
    /// True while `window.prompt` for a disk change is awaiting a choice.
    pub(crate) disk_prompt_open: bool,
    /// One-shot: a gutter play glyph asked to run the statement at this byte
    /// offset; consumed by the next `execute_query`.
    pub(crate) run_statement_offset: Option<usize>,
    /// One-shot joined batch from the Run Statements picker; consumed by the next
    /// `execute_query`. Carries the batch text and the buffer offset of its
    /// first selected statement for result-source highlighting.
    pub(crate) run_statement_selection: Option<(String, usize)>,
    /// Parsed server version per connection id. `Some(None)` means probed but
    /// unparseable; a missing key means not yet probed this session.
    pub(crate) server_versions: HashMap<String, Option<sqlly_services::plans::ServerVersion>>,
    pub(crate) explain_prefs: crate::preferences::ExplainPreferences,
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::dialog::{Dialog, DialogFrame};
    use crate::theme::Theme;
    use gpui::{Context, IntoElement, Render, Window, div, prelude::*};

    /// The notice exactly as the workbench mounts it: the same chrome, the
    /// same auto-height 440px frame, the same body.
    struct NoticeHost {
        frame: DialogFrame,
    }

    impl Render for NoticeHost {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().relative().child(
                Dialog::new("terminal-secret-notice", |this: &mut Self| &mut this.frame)
                    .title("Open in Terminal")
                    .confirm("Open Terminal", "Cancel")
                    .on_confirm(|_, _, _| {})
                    .on_reject(|_, _, _| {})
                    .children_body(terminal_notice_body("Analytics Postgres", "psql"))
                    .build(self, window, cx),
            )
        }
    }

    /// The status bar used to say "Opening a terminal for …" the whole time
    /// the notice sat waiting, and kept saying it after a cancel. Only a
    /// confirmed launch may claim a terminal is opening.
    #[test]
    fn terminal_status_only_claims_opening_once_a_launch_is_under_way() {
        use crate::app::StatusLevel;
        let name = "Analytics Postgres";
        for idle in [
            TerminalLaunchPhase::Preparing,
            TerminalLaunchPhase::AwaitingNotice,
            TerminalLaunchPhase::Cancelled,
        ] {
            let (_, text) = terminal_launch_status(name, &idle);
            assert!(
                !text.contains("Opening") && !text.contains("Opened"),
                "{idle:?} must not claim a terminal is opening: {text}"
            );
        }
        assert_eq!(
            terminal_launch_status(name, &TerminalLaunchPhase::AwaitingNotice),
            (StatusLevel::Info, "Ready".to_string()),
            "the waiting notice clears the status"
        );
        assert_eq!(
            terminal_launch_status(name, &TerminalLaunchPhase::Cancelled),
            (StatusLevel::Info, "Ready".to_string()),
            "a cancel clears the status"
        );
        assert_eq!(
            terminal_launch_status(name, &TerminalLaunchPhase::Launching),
            (
                StatusLevel::Busy,
                "Opening a terminal for Analytics Postgres…".to_string()
            )
        );
        assert_eq!(
            terminal_launch_status(name, &TerminalLaunchPhase::Launched { notes: vec![] }),
            (
                StatusLevel::Success,
                "Opened a terminal for Analytics Postgres".to_string()
            )
        );
        let (level, text) = terminal_launch_status(
            name,
            &TerminalLaunchPhase::Launched {
                notes: vec!["psql connects to 127.0.0.1:50123 through the tunnel.".to_string()],
            },
        );
        assert_eq!(level, StatusLevel::Info);
        assert_eq!(
            text,
            "Opened a terminal for Analytics Postgres. psql connects to 127.0.0.1:50123 through the tunnel."
        );
    }

    /// Every bullet used to be one unwrapped line cut off at the card's right
    /// edge. Each must wrap onto several lines inside the card, and the
    /// auto-height card must grow to hold all of them.
    #[gpui::test]
    fn terminal_notice_bullets_wrap_inside_the_card(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let (_host, cx) = cx.add_window_view(|_window, _cx| NoticeHost {
            frame: DialogFrame::new_auto(Theme::scaled(440.0)),
        });
        cx.run_until_parked();

        let card = cx
            .debug_bounds("dialog-card-terminal-secret-notice")
            .expect("the notice card is laid out");
        let font = Theme::font_size_ratio(0.80);
        let selectors = [
            "terminal-notice-bullet-0",
            "terminal-notice-bullet-1",
            "terminal-notice-bullet-2",
            "terminal-notice-bullet-3",
            "terminal-notice-footnote",
        ];
        let mut previous_bottom = f32::MIN;
        for selector in selectors {
            let row = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} is laid out"));
            assert!(
                row.left() >= card.left() && row.right() <= card.right(),
                "{selector} {row:?} stays inside the card {card:?}"
            );
            assert!(
                f32::from(row.bottom()) <= f32::from(card.bottom()),
                "{selector} {row:?} is not cut off by the card's bottom {card:?}"
            );
            assert!(
                f32::from(row.top()) >= previous_bottom - 0.01,
                "{selector} overlaps the line above"
            );
            previous_bottom = f32::from(row.bottom());
            if selector.contains("bullet") {
                // One line is ~1.6x the font size; every bullet is longer than
                // the 440px card is wide, so a wrapped bullet is 2+ lines.
                assert!(
                    f32::from(row.size.height) > font * 2.5,
                    "{selector} wraps onto several lines, got {row:?} at {font}px text"
                );
            }
        }
    }
}
