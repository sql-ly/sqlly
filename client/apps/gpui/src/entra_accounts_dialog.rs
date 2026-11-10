//! Entra account management dialog — the gpui equivalent of Swift's
//! `AzureAccountDialogController`.
//!
//! Lists the registered Entra accounts, reports what the dialog is doing in a
//! typed status line, and exposes "Add User" + "Remove" controls. Emits an
//! [`EntraAccountsEvent`] when the user adds or removes an account so the
//! parent view can refresh the connection-profile picker (Slice 11).

use gpui::{
    EventEmitter, FocusHandle, Hsla, IntoElement, ParentElement, Render, Styled, Window, div, img,
    prelude::*, px,
};
use sqlly_services::security::azure::{EntraAccountId, EntraAccountProfile};

use crate::azure_registry::EntraAccountRegistryStore;
use crate::azure_signin::SigninOutcome;
use crate::dialog::{Dialog, DialogFrame};
use crate::theme::{Theme, themed_row};

/// Maximum height of the status area so long OAuth error payloads scroll
/// instead of pushing the dialog past the screen. The account list is clamped
/// separately (`flex_1` + `min_h_0` + scroll): clamping only the status area is
/// what let a long enough list push the button bar — including Close — off a
/// modal that had no Escape key.
const STATUS_MAX_HEIGHT: f32 = 96.0;
/// Default status text shown when the dialog opens with no in-flight action.
fn default_status() -> String {
    crate::localization::text("connections.entra.tokens_in_keychain")
}

/// What the dialog is waiting on. `None` means idle.
///
/// This is an `Option<InFlight>` rather than a `bool` because the bool shipped
/// two bugs that the type now makes unrepresentable:
///
/// - `on_signin_success` never cleared it, so a *successful* sign-in latched
///   every control off for the dialog's lifetime — you could never add a second
///   account. The failure path recovered; the success path did not. Now the only
///   ways out of the flight are [`EntraAccountsDialog::on_signin_success`] and
///   [`EntraAccountsDialog::on_signin_error`], and both consume the state.
/// - Every row read the same global flag, so refreshing one account rendered
///   "Refreshing…" on all of them. A refresh now carries the id of the account it
///   actually belongs to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InFlight {
    /// An interactive browser sign-in registering a brand-new account.
    Add,
    /// Re-auth for an account already in the registry.
    Refresh(EntraAccountId),
}

/// The register the status line is speaking in.
///
/// Every status — idle prose, success, and OAuth failure — used to paint in
/// `text_dim`, so the one message that mattered whispered in the voice reserved
/// for boilerplate the user had already learned to ignore.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StatusKind {
    Idle,
    Busy,
    Success,
    Error,
}

impl StatusKind {
    /// Resolved at draw time so live theme/family/OS switches repaint — never
    /// cached in a field.
    fn color(self) -> Hsla {
        match self {
            Self::Idle | Self::Busy => Theme::text_dim(),
            Self::Success => Theme::success(),
            Self::Error => Theme::error(),
        }
    }
}

#[derive(Clone)]
pub enum EntraAccountsEvent {
    /// The user clicked "Add User" to begin an interactive browser sign-in.
    /// The host (`AppView`) starts the OAuth orchestration and threads the
    /// result back via [`EntraAccountsDialog::on_signin_success`] /
    /// [`EntraAccountsDialog::on_signin_error`].
    AddUserRequested,
    /// Re-run interactive sign-in for a listed account so its keychain token
    /// bundle is replaced with fresh credentials.
    RefreshCredentialsRequested(Box<EntraAccountProfile>),
    Changed,
    /// Dialog dismissed (user pressed Close). The host clears the
    /// `AppView.entra_accounts_dialog` reference so the next invocation
    /// builds a fresh dialog instead of resurrecting this one.
    Dismissed,
}

impl EventEmitter<EntraAccountsEvent> for EntraAccountsDialog {}

pub struct EntraAccountsDialog {
    registry: EntraAccountRegistryStore,
    accounts: Vec<EntraAccountProfile>,
    selected: Option<usize>,
    status: String,
    status_kind: StatusKind,
    in_flight: Option<InFlight>,
    /// The account armed for removal, awaiting a second click to commit.
    /// Removing a keychain credential is irreversible, so it takes two.
    pending_removal: Option<EntraAccountId>,
    /// What to re-attempt when the user takes the "Try again" affordance —
    /// captured from `in_flight` as it is consumed by a failure.
    last_failed: Option<InFlight>,
    /// Arrow keys move the selection, Enter adds, Delete removes, Escape backs
    /// out. Without a focus handle this dialog was mouse-only, and — since the
    /// backdrop was the sole dismissal — a keyboard-only user could not even
    /// leave it.
    focus_handle: FocusHandle,
    /// One-shot: take the keyboard on first paint so Escape works on open.
    focus_requested: bool,
    frame: DialogFrame,
}

impl gpui::Focusable for EntraAccountsDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntraAccountsDialog {
    pub fn new(registry: EntraAccountRegistryStore, cx: &mut gpui::Context<Self>) -> Self {
        let accounts = registry.list();
        let mut this = Self {
            registry,
            accounts,
            selected: None,
            status: default_status(),
            status_kind: StatusKind::Idle,
            in_flight: None,
            pending_removal: None,
            last_failed: None,
            focus_handle: cx.focus_handle(),
            focus_requested: false,
            frame: DialogFrame::new_auto(Theme::scaled(560.0)),
        };
        // Mirror the Swift dialog: pre-select the most-recently-used account if
        // there is exactly one so Remove is immediately actionable. Safe now
        // that Remove arms a confirm rather than destroying on the first click.
        if this.accounts.len() == 1 {
            this.selected = Some(0);
        }
        // Trigger initial render.
        cx.notify();
        this
    }

    /// Re-read the registry from disk. Called by the parent after add/remove
    /// events so the table reflects the up-to-date identity list.
    pub fn reload(&mut self, cx: &mut gpui::Context<Self>) {
        self.accounts = self.registry.list();
        if let Some(idx) = self.selected
            && idx >= self.accounts.len()
        {
            self.selected = None;
        }
        // An account armed for removal that is no longer in the registry must
        // not stay armed — the next Remove would act on a stale id.
        if let Some(id) = &self.pending_removal
            && !self.accounts.iter().any(|a| &a.id == id)
        {
            self.pending_removal = None;
        }
        cx.notify();
    }

    fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status_kind = kind;
        self.status = text.into();
    }

    /// Marks the dialog as busy so the "Add User" button is disabled while
    /// the browser is open.
    pub fn begin_signin(&mut self, cx: &mut gpui::Context<Self>) {
        self.in_flight = Some(InFlight::Add);
        self.pending_removal = None;
        self.set_status(
            StatusKind::Busy,
            crate::localization::text("connections.entra.waiting_for_signin"),
        );
        cx.notify();
    }

    pub fn begin_credential_refresh(
        &mut self,
        account: &EntraAccountProfile,
        cx: &mut gpui::Context<Self>,
    ) {
        self.in_flight = Some(InFlight::Refresh(account.id.clone()));
        self.pending_removal = None;
        self.set_status(
            StatusKind::Busy,
            crate::localization::format(
                "connections.entra.waiting_for_refresh",
                &[("account_label", &display_label(account))],
            ),
        );
        cx.notify();
    }

    /// Called when the orchestration succeeded — re-read the registry and
    /// emit [`EntraAccountsEvent::Changed`] so the picker updates.
    pub fn on_signin_success(&mut self, outcome: &SigninOutcome, cx: &mut gpui::Context<Self>) {
        // Consume the flight. Forgetting this line is what bricked the dialog:
        // every control stayed disabled after the first successful sign-in.
        self.in_flight = None;
        self.last_failed = None;
        self.reload(cx);
        let label = display_label(&outcome.profile);
        self.set_status(
            StatusKind::Success,
            crate::localization::format(
                "connections.entra.signin_succeeded",
                &[("account_label", &label)],
            ),
        );
        cx.emit(EntraAccountsEvent::Changed);
        cx.notify();
    }

    /// Called when the orchestration failed — keep the table as-is and show
    /// the error so the user can retry. `take()` both consumes the flight and
    /// captures what to re-attempt.
    pub fn on_signin_error(&mut self, message: impl Into<String>, cx: &mut gpui::Context<Self>) {
        self.last_failed = self.in_flight.take();
        self.set_status(
            StatusKind::Error,
            crate::localization::format(
                "connections.entra.signin_failed",
                &[("error", &message.into())],
            ),
        );
        cx.notify();
    }

    /// Re-emit whichever request just failed.
    fn retry_last_failed(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(failed) = self.last_failed.clone() else {
            return;
        };
        match failed {
            InFlight::Add => cx.emit(EntraAccountsEvent::AddUserRequested),
            InFlight::Refresh(id) => {
                let Some(account) = self.accounts.iter().find(|a| a.id == id).cloned() else {
                    // The account was removed while the failure was on screen.
                    self.last_failed = None;
                    self.set_status(StatusKind::Idle, default_status());
                    cx.notify();
                    return;
                };
                cx.emit(EntraAccountsEvent::RefreshCredentialsRequested(Box::new(
                    account,
                )));
            }
        }
        cx.notify();
    }

    /// Ask the host to re-authenticate the selected account. The per-row button
    /// emits the same event; this is the keyboard's way in. Without it, an
    /// expired credential could only be refreshed with a mouse.
    fn refresh_selected(&mut self, cx: &mut gpui::Context<Self>) {
        if self.in_flight.is_some() {
            return;
        }
        let Some(account) = self.selected.and_then(|i| self.accounts.get(i)).cloned() else {
            return;
        };
        cx.emit(EntraAccountsEvent::RefreshCredentialsRequested(Box::new(
            account,
        )));
        cx.notify();
    }

    fn select(&mut self, idx: Option<usize>, cx: &mut gpui::Context<Self>) {
        if self.selected != idx {
            // Moving off an armed row disarms it — the confirm belongs to the
            // account it was armed for, not to whatever is selected next.
            self.pending_removal = None;
        }
        self.selected = idx;
        cx.notify();
    }

    fn move_selection(&mut self, delta: isize, cx: &mut gpui::Context<Self>) {
        if self.accounts.is_empty() {
            return;
        }
        let last = self.accounts.len() - 1;
        let next = match self.selected {
            None if delta > 0 => 0,
            None => last,
            Some(i) => (i as isize + delta).clamp(0, last as isize) as usize,
        };
        self.select(Some(next), cx);
    }

    /// First click arms the confirm, second commits. A keychain credential is
    /// unrecoverable once gone, and until this existed a single stray click on
    /// a row the user could not reliably see destroyed one.
    fn remove_selected(&mut self, cx: &mut gpui::Context<Self>) {
        // Never destroy an account whose own re-auth is still running: the
        // orchestration would land a fresh token bundle for an id that is no
        // longer in the registry.
        if self.in_flight.is_some() {
            return;
        }
        let Some(idx) = self.selected else { return };
        let Some(account) = self.accounts.get(idx) else {
            return;
        };
        let id = account.id.clone();
        let label = display_label(account);
        if self.pending_removal.as_ref() == Some(&id) {
            self.registry.remove(&id);
            self.pending_removal = None;
            self.reload(cx);
            self.set_status(
                StatusKind::Success,
                crate::localization::format(
                    "connections.entra.account_removed",
                    &[("account_label", &label)],
                ),
            );
            cx.emit(EntraAccountsEvent::Changed);
        } else {
            self.pending_removal = Some(id);
            self.set_status(
                StatusKind::Idle,
                crate::localization::format(
                    "connections.entra.remove_confirmation",
                    &[("account_label", &label)],
                ),
            );
        }
        cx.notify();
    }

    /// Emit [`EntraAccountsEvent::Dismissed`] so the host (`AppView`) can
    /// clear its `entra_accounts_dialog` reference. Lighter than a
    /// `window.close()` because we are rendering the dialog as an overlay
    /// inside the AppView window, not a separate window.
    pub fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(EntraAccountsEvent::Dismissed);
        cx.notify();
    }

    pub fn is_in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        // Let modified chords through to the app's own bindings.
        let m = &event.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        match event.keystroke.key.as_str() {
            "up" => self.move_selection(-1, cx),
            "down" => self.move_selection(1, cx),
            "enter" => {
                // Enter takes the most specific action on offer: commit an armed
                // removal, else retry a failure the user is staring at, else add.
                if self.pending_removal.is_some() {
                    self.remove_selected(cx);
                } else if self.status_kind == StatusKind::Error && self.last_failed.is_some() {
                    self.retry_last_failed(cx);
                } else if self.in_flight.is_none() {
                    cx.emit(EntraAccountsEvent::AddUserRequested);
                    cx.notify();
                }
            }
            "delete" | "backspace" => self.remove_selected(cx),
            // Re-auth the selected account. The row's Refresh button was
            // mouse-only, which stranded the keyboard user this handler exists
            // for on the one task an expiring credential forces.
            "r" => self.refresh_selected(cx),
            "escape" => {
                // Escape backs out of the confirm before it backs out of the
                // dialog — one Escape should never do two things at once. The
                // dialog chrome's own key handler sits on an ancestor and
                // closes on Escape, so consume the key here or a single press
                // would both disarm the confirm and dismiss the dialog.
                cx.stop_propagation();
                if self.pending_removal.take().is_some() {
                    self.set_status(StatusKind::Idle, default_status());
                    cx.notify();
                } else {
                    self.close(cx);
                }
            }
            _ => {}
        }
    }
}

fn display_label(account: &EntraAccountProfile) -> String {
    let friendly = account.display_name.as_deref().unwrap_or("").trim();
    if !friendly.is_empty() {
        return friendly.to_string();
    }
    let username = account.username.trim();
    if !username.is_empty() {
        return username.to_string();
    }
    crate::localization::text("connections.entra.untitled_user")
}

fn detail_label(account: &EntraAccountProfile) -> String {
    let username = account.username.trim();
    let tenant = account.tenant_id.trim();
    match (username.is_empty(), tenant.is_empty()) {
        (true, true) => crate::localization::text("connections.entra.fallback_account_detail"),
        (true, false) => crate::localization::format(
            "connections.entra.tenant_only_detail",
            &[("tenant_id", tenant)],
        ),
        (false, true) => username.to_string(),
        (false, false) => crate::localization::format(
            "connections.entra.username_tenant_detail",
            &[("username", username), ("tenant_id", tenant)],
        ),
    }
}

impl Render for EntraAccountsDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        // Take the keyboard on first paint so Escape works the moment the
        // dialog opens rather than only after a click lands somewhere in it.
        if !self.focus_requested {
            self.focus_requested = true;
            window.focus(&self.focus_handle, cx);
        }

        let selected = self.selected;
        let in_flight = self.in_flight.clone();
        let pending_removal = self.pending_removal.clone();
        let status = self.status.clone();
        let status_kind = self.status_kind;
        let can_retry = status_kind == StatusKind::Error && self.last_failed.is_some();
        let busy = in_flight.is_some();
        let gap = Theme::scaled(8.0);
        let radius = Theme::scaled(4.0);

        let list = if self.accounts.is_empty() {
            // An empty state that teaches the interface. "No accounts yet." over
            // a dead Remove button told a first-run user nothing about the one
            // thing they were here to do.
            div()
                .flex()
                .flex_1()
                .min_h_0()
                .items_center()
                .justify_center()
                .p(px(Theme::scaled(16.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "connections.entra.empty_accounts",
                ))
                .into_any_element()
        } else {
            div()
                .id("entra-account-rows")
                .flex()
                .flex_col()
                // The list — not the status area — is what grows without bound
                // here. flex_1 + min_h_0 + scroll keeps the button bar on screen
                // no matter how many accounts are registered.
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .gap(px(Theme::scaled(2.0)))
                .children(self.accounts.iter().enumerate().map(|(idx, account)| {
                    let is_selected = Some(idx) == selected;
                    let is_refreshing = in_flight == Some(InFlight::Refresh(account.id.clone()));
                    let refresh_account = account.clone();
                    // themed_row owns the row's bg and hover — it is the helper
                    // that exists because "hover repaints the selected row" has
                    // shipped twice already. It must be the only hover here.
                    themed_row(
                        div()
                            .id(("entra-account-row", idx))
                            .cursor_pointer()
                            .min_h(px(Theme::row_height()))
                            .px(px(Theme::scaled(6.0)))
                            .py(px(Theme::scaled(4.0)))
                            .rounded(px(radius))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                // Clicking a row gives the dialog the keyboard so
                                // arrows continue from what was just clicked.
                                window.focus(&this.focus_handle, cx);
                                this.select(Some(idx), cx);
                            })),
                        is_selected,
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .justify_between()
                            .gap(px(gap))
                            .child(
                                // display_name is server-controlled and can be
                                // arbitrarily long; without min_w_0 it shoves the
                                // tenant and Refresh out of the dialog, and
                                // text_ellipsis cuts it at a glyph with a "…"
                                // rather than mid-letter.
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_color(Theme::text())
                                    .child(display_label(account)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    // A selected row's fill is accent at 30% —
                                    // muted ink on it measures 3.13:1 in Classic
                                    // Dark, so the secondary text promotes to body
                                    // ink rather than dimming into the fill. Same
                                    // move the schema tree's failed marker makes on
                                    // a hovered row (theme.rs, "failed marker").
                                    .text_color(if is_selected {
                                        Theme::text()
                                    } else {
                                        Theme::text_dim()
                                    })
                                    .child(detail_label(account)),
                            )
                            .child(
                                div()
                                    .id(("entra-refresh-credentials", idx))
                                    .flex_none()
                                    .px(px(Theme::scaled(8.0)))
                                    .py(px(Theme::scaled(2.0)))
                                    .rounded(px(radius))
                                    // A control that reads as a control: this was
                                    // bare clickable text whose only affordance was
                                    // the cursor, which names nothing. The fill is a
                                    // surface step — the house depth cue — and not a
                                    // hairline, because `border` on `popup_bg` only
                                    // reaches 1.3:1 in most palettes. It is opaque so
                                    // the label answers to `text`-on-`panel_bg`
                                    // whatever the row underneath is doing.
                                    .bg(Theme::panel_bg())
                                    .text_color(if busy {
                                        Theme::text_dim()
                                    } else {
                                        Theme::text()
                                    })
                                    .when(!busy, |button| {
                                        button
                                            .cursor_pointer()
                                            // row_hover, not border(): border as a
                                            // hover fill is text-on-text (1.00:1)
                                            // in High Contrast — an invisible label.
                                            .hover(|style| style.bg(Theme::row_hover()))
                                            .on_click(cx.listener(move |_this, _, _, cx| {
                                                cx.emit(
                                                    EntraAccountsEvent::RefreshCredentialsRequested(
                                                        Box::new(refresh_account.clone()),
                                                    ),
                                                );
                                                cx.stop_propagation();
                                            }))
                                    })
                                    // Only the row actually being refreshed says so.
                                    .child(crate::localization::text(if is_refreshing {
                                        "connections.entra.refreshing_account"
                                    } else {
                                        "connections.entra.refresh_account"
                                    })),
                            ),
                    )
                }))
                .into_any_element()
        };

        let add_user = div()
            .id("entra-add-user")
            .flex()
            .items_center()
            .justify_center()
            .h(px(Theme::control_height()))
            .px(px(Theme::scaled(12.0)))
            .rounded(px(radius))
            .when(busy, |d| {
                // A real disabled treatment. The old one dimmed an accent fill
                // to 50%, which faked "disabled" by making already-failing text
                // fail harder and conveyed the state through contrast alone.
                d.bg(Theme::panel_bg()).text_color(Theme::text_dim())
            })
            .when(!busy, |d| {
                d.bg(Theme::accent())
                    // Dark ink on an accent fill — the pairing `ink_on` asserts.
                    .text_color(Theme::ink_on(Theme::accent()))
                    .cursor_pointer()
                    // Rest and hover were the same accent: a placebo hover on
                    // the primary action. opacity(0.9) matches the app's other
                    // accent-filled buttons.
                    .hover(|s| s.opacity(0.9))
                    .on_click(cx.listener(|_this, _, _, cx| {
                        // Ask the host to start the browser sign-in. The dialog
                        // itself has no access to the HTTP transport / token
                        // cache, so the orchestration is driven by `AppView`,
                        // which flips `in_flight` via `begin_signin`.
                        cx.emit(EntraAccountsEvent::AddUserRequested);
                        cx.notify();
                    }))
            })
            .child(crate::localization::text(if busy {
                "connections.entra.signing_in"
            } else {
                "connections.entra.add_user"
            }));

        let armed = pending_removal.is_some();
        let can_remove = selected.is_some() && !busy;
        let remove_btn = div()
            .id("entra-remove-user")
            .flex()
            .items_center()
            .justify_center()
            .h(px(Theme::control_height()))
            .px(px(Theme::scaled(12.0)))
            .rounded(px(radius))
            .when(!can_remove, |d| {
                d.bg(Theme::panel_bg()).text_color(Theme::text_dim())
            })
            .when(can_remove && !armed, |d| {
                d.bg(Theme::panel_bg())
                    .text_color(Theme::text())
                    .cursor_pointer()
                    .hover(|s| s.bg(Theme::row_hover()))
            })
            .when(can_remove && armed, |d| {
                // Danger fill with canvas ink — the ("label on error fill", 4.5)
                // pair the matrix already asserts.
                d.bg(Theme::error())
                    .text_color(Theme::ink_on(Theme::error()))
                    .cursor_pointer()
                    .hover(|s| s.opacity(0.9))
            })
            .when(can_remove, |d| {
                d.on_click(cx.listener(|this, _, _, cx| {
                    this.remove_selected(cx);
                }))
            })
            .child(crate::localization::text(if armed {
                "connections.entra.confirm_removal"
            } else {
                "connections.entra.remove_account"
            }));

        let status_area = div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(gap))
            .child(
                div()
                    .id("entra-status")
                    .flex_1()
                    .min_w_0()
                    .min_h(px(Theme::row_height()))
                    .max_h(px(Theme::scaled(STATUS_MAX_HEIGHT)))
                    .overflow_y_scroll()
                    .py(px(Theme::scaled(4.0)))
                    // Failure no longer wears the same gray as idle prose.
                    .text_color(status_kind.color())
                    .child(status),
            )
            .when(can_retry, |d| {
                d.child(
                    div()
                        .id("entra-retry")
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .h(px(Theme::control_height()))
                        .px(px(Theme::scaled(10.0)))
                        .rounded(px(radius))
                        // A fill, not a hairline: `border` on `popup_bg` measures
                        // 1.3:1 in six of seven palettes, so an outlined-only
                        // button is a button you cannot see — and this one appears
                        // exactly when the user is already stuck.
                        .bg(Theme::panel_bg())
                        .text_color(Theme::text())
                        .cursor_pointer()
                        .hover(|s| s.bg(Theme::row_hover()))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.retry_last_failed(cx);
                        }))
                        .child(crate::localization::text("connections.entra.try_again")),
                )
            });

        // The title text lives in the chrome's title bar now; the Entra icon
        // and the subtitle stay at the top of the body.
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::scaled(6.0)))
            .children(crate::icons::entra_id_icon().map(|icon| {
                img(icon)
                    .w(px(Theme::scaled(16.0)))
                    .h(px(Theme::scaled(16.0)))
            }))
            .child(
                div()
                    .text_color(Theme::text_dim())
                    // The subtitle and the status line used to say the same three
                    // things twice, in ~60 words, above a list of one row.
                    .child(crate::localization::text(
                        "connections.entra.dialog_subtitle",
                    )),
            );

        // The rich key handling (arrows, Enter, Delete, `r`, two-stage Escape)
        // stays on the body root, which takes focus on first paint — the
        // chrome's own Escape handler is an ancestor and only sees keys this
        // handler lets through.
        let body = div()
            .id("entra-dialog-body")
            .key_context("EntraAccountsDialog")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap(px(gap))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, window, cx| {
                    window.focus(&this.focus_handle, cx);
                }),
            )
            .child(header)
            .children(crate::browser_preview::preview_banner(
                crate::localization::text(if crate::browser_preview::served() {
                    "connections.entra.served_browser_banner"
                } else {
                    "connections.entra.preview_browser_banner"
                }),
            ))
            .child(list)
            .child(status_area);

        Dialog::new("entra-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("connections.entra.dialog_title"))
            .dismiss_on_scrim_click(false)
            .on_close(|this, _window, cx| this.close(cx))
            .child(body)
            .leading_button(add_user)
            .leading_button(remove_btn)
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(user: &str, tenant: &str) -> EntraAccountProfile {
        EntraAccountProfile::new(user, tenant, "home", None, chrono::Utc::now())
    }

    #[test]
    fn display_label_prefers_friendly_name_then_username() {
        let mut profile = profile("alex@contoso.com", "tenant-a");
        profile.display_name = Some("  Alex K  ".into());
        assert_eq!(display_label(&profile), "Alex K");
        profile.display_name = Some("".into());
        assert_eq!(display_label(&profile), "alex@contoso.com");
        let mut anonym = profile.clone();
        anonym.username = String::new();
        assert_eq!(display_label(&anonym), "Untitled Entra User");
    }

    #[test]
    fn detail_label_combines_username_and_tenant() {
        assert_eq!(
            detail_label(&profile("alex@contoso.com", "tenant-a")),
            "alex@contoso.com • Tenant: tenant-a"
        );
        assert_eq!(detail_label(&profile("", "tenant-a")), "Tenant: tenant-a");
        assert_eq!(detail_label(&profile("alex@x.com", "")), "alex@x.com");
        assert_eq!(
            detail_label(&profile("", "")),
            crate::localization::text("connections.entra.fallback_account_detail")
        );
    }

    /// A registry backed by a temp file, plus the dialog under test. The
    /// `TempDir` is returned because dropping it deletes the file the registry
    /// writes through.
    fn dialog<'a>(
        cx: &'a mut gpui::TestAppContext,
        seed: &[EntraAccountProfile],
    ) -> (
        gpui::Entity<EntraAccountsDialog>,
        &'a mut gpui::VisualTestContext,
        tempfile::TempDir,
    ) {
        // The unified dialog chrome renders gpui-component Buttons, which
        // read the component theme global — install it first.
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let temp = tempfile::tempdir().unwrap();
        let registry = EntraAccountRegistryStore::from_path(temp.path().join("accounts.json"))
            .expect("registry opens on a temp path");
        for p in seed {
            registry.upsert(p.clone());
        }
        let (view, cx) = cx.add_window_view(|_window, cx| EntraAccountsDialog::new(registry, cx));
        (view, cx, temp)
    }

    fn outcome_for(profile: &EntraAccountProfile) -> SigninOutcome {
        SigninOutcome {
            profile_id: profile.id.clone(),
            profile: profile.clone(),
            access_token_expiry: None,
        }
    }

    /// **The bug this dialog shipped.** A *successful* sign-in left `in_flight`
    /// latched, so every control stayed disabled and no second account could
    /// ever be added — the failure path recovered, the success path did not.
    ///
    /// This drives the real dialog: delete `self.in_flight = None` from
    /// `on_signin_success` and this test fails. An earlier version asserted the
    /// transition on a local `Option` and passed happily with the bug restored.
    #[gpui::test]
    fn a_successful_signin_releases_the_dialog(cx: &mut gpui::TestAppContext) {
        let alex = profile("alex@contoso.com", "tenant-a");
        let (view, cx, _temp) = dialog(cx, &[]);

        view.update(cx, |dlg, cx| {
            dlg.begin_signin(cx);
            assert!(dlg.is_in_flight(), "begin_signin puts the dialog in flight");

            // `run_signin` writes the profile to the registry (azure_signin.rs:98)
            // and only then hands the outcome back, so the dialog's reload is what
            // surfaces it. Mirror that order here rather than pretending the
            // dialog stores anything itself.
            dlg.registry.upsert(alex.clone());
            dlg.on_signin_success(&outcome_for(&alex), cx);
            assert!(
                !dlg.is_in_flight(),
                "a successful sign-in must release the dialog — latching here is \
                 what made a second account impossible to add"
            );
            assert_eq!(dlg.status_kind, StatusKind::Success);
            assert_eq!(dlg.accounts.len(), 1, "success re-reads the registry");

            // The point of releasing: a second sign-in can actually start.
            dlg.begin_signin(cx);
            assert!(dlg.is_in_flight(), "a second sign-in can begin");
        });
    }

    /// The failure path must also release, and must capture what to retry.
    #[gpui::test]
    fn a_failed_signin_releases_and_offers_a_retry(cx: &mut gpui::TestAppContext) {
        let (view, cx, _temp) = dialog(cx, &[]);

        view.update(cx, |dlg, cx| {
            dlg.begin_signin(cx);
            dlg.on_signin_error("the browser closed", cx);

            assert!(!dlg.is_in_flight(), "failure leaves the dialog idle");
            assert_eq!(dlg.status_kind, StatusKind::Error, "failure is not a hint");
            assert!(dlg.status.contains("the browser closed"));
            assert_eq!(
                dlg.last_failed,
                Some(InFlight::Add),
                "the retry affordance knows what to re-attempt"
            );
        });
    }

    /// Every row read one global `bool`, so refreshing one account rendered
    /// "Refreshing…" on all of them. Identity now lives in the flight itself.
    #[gpui::test]
    fn a_refresh_in_flight_names_only_its_own_account(cx: &mut gpui::TestAppContext) {
        let a = profile("a@x.com", "t1");
        let b = profile("b@x.com", "t2");
        assert_ne!(a.id, b.id);
        let (view, cx, _temp) = dialog(cx, &[a.clone(), b.clone()]);

        view.update(cx, |dlg, cx| {
            dlg.begin_credential_refresh(&a, cx);
            // This is exactly what the row renderer compares against.
            assert_eq!(dlg.in_flight, Some(InFlight::Refresh(a.id.clone())));
            assert_ne!(
                dlg.in_flight,
                Some(InFlight::Refresh(b.id.clone())),
                "b's row must not claim to be refreshing"
            );
            assert!(
                dlg.status.contains("a@x.com"),
                "the status names the account"
            );
        });
    }

    /// Removing a keychain credential is irreversible, so it takes two calls.
    #[gpui::test]
    fn removal_requires_a_confirm(cx: &mut gpui::TestAppContext) {
        let alex = profile("alex@contoso.com", "tenant-a");
        let (view, cx, _temp) = dialog(cx, std::slice::from_ref(&alex));

        view.update(cx, |dlg, cx| {
            // A lone account is pre-selected so Remove is actionable.
            assert_eq!(dlg.selected, Some(0));

            dlg.remove_selected(cx);
            assert_eq!(
                dlg.accounts.len(),
                1,
                "the first call must only arm the confirm, never destroy"
            );
            assert_eq!(dlg.pending_removal, Some(alex.id.clone()));
            assert!(
                dlg.status.contains("not affected"),
                "the confirm says the Microsoft account survives"
            );

            dlg.remove_selected(cx);
            assert!(dlg.accounts.is_empty(), "the second call commits");
            assert_eq!(dlg.pending_removal, None);
        });
    }

    /// Moving off an armed row disarms it: the confirm belongs to the account it
    /// was armed for, not to whatever is selected next.
    #[gpui::test]
    fn changing_selection_disarms_the_confirm(cx: &mut gpui::TestAppContext) {
        let a = profile("a@x.com", "t1");
        let b = profile("b@x.com", "t2");
        let (view, cx, _temp) = dialog(cx, &[a, b]);

        view.update(cx, |dlg, cx| {
            dlg.select(Some(0), cx);
            dlg.remove_selected(cx);
            assert!(dlg.pending_removal.is_some(), "armed");

            dlg.move_selection(1, cx);
            assert_eq!(dlg.pending_removal, None, "moving off the row disarms it");

            // So the next Remove arms afresh rather than committing.
            dlg.remove_selected(cx);
            assert_eq!(dlg.accounts.len(), 2, "nothing was destroyed");
        });
    }

    /// Deleting an account whose own re-auth is in flight would land a fresh
    /// token bundle for an id no longer in the registry.
    #[gpui::test]
    fn removal_is_refused_while_a_signin_is_in_flight(cx: &mut gpui::TestAppContext) {
        let alex = profile("alex@contoso.com", "tenant-a");
        let (view, cx, _temp) = dialog(cx, std::slice::from_ref(&alex));

        view.update(cx, |dlg, cx| {
            dlg.select(Some(0), cx);
            dlg.remove_selected(cx);
            assert!(dlg.pending_removal.is_some(), "armed before the flight");

            dlg.begin_credential_refresh(&alex, cx);
            assert_eq!(
                dlg.pending_removal, None,
                "starting a flight disarms a pending confirm"
            );

            dlg.remove_selected(cx);
            dlg.remove_selected(cx);
            assert_eq!(
                dlg.accounts.len(),
                1,
                "no number of clicks removes an account mid-flight"
            );
            assert_eq!(dlg.pending_removal, None, "and nothing gets armed either");
        });
    }

    /// Refresh was reachable only with a mouse, on the one task an expiring
    /// credential forces. `r` is the keyboard's way to the same event.
    #[gpui::test]
    fn refresh_selected_is_reachable_and_guarded(cx: &mut gpui::TestAppContext) {
        let a = profile("a@x.com", "t1");
        let (view, cx, _temp) = dialog(cx, std::slice::from_ref(&a));

        view.update(cx, |dlg, cx| {
            assert_eq!(dlg.selected, Some(0));
            // Nothing selected → nothing to refresh, and no panic.
            dlg.select(None, cx);
            dlg.refresh_selected(cx);

            dlg.select(Some(0), cx);
            dlg.refresh_selected(cx);

            // Guarded like every other mutating path: a second request cannot
            // start while one is already in the air.
            dlg.begin_credential_refresh(&a, cx);
            dlg.refresh_selected(cx);
            assert_eq!(
                dlg.in_flight,
                Some(InFlight::Refresh(a.id.clone())),
                "the in-flight refresh is not disturbed by a second request"
            );
        });
    }

    /// The retry affordance appears exactly when the user is stuck, so it has to
    /// be reachable without a mouse too.
    #[gpui::test]
    fn a_failed_refresh_retries_the_same_account(cx: &mut gpui::TestAppContext) {
        let a = profile("a@x.com", "t1");
        let b = profile("b@x.com", "t2");
        let (view, cx, _temp) = dialog(cx, &[a.clone(), b]);

        view.update(cx, |dlg, cx| {
            dlg.begin_credential_refresh(&a, cx);
            dlg.on_signin_error("token endpoint said no", cx);
            assert_eq!(dlg.last_failed, Some(InFlight::Refresh(a.id.clone())));

            // Retry re-emits for `a`, not for whatever happens to be selected.
            dlg.select(Some(1), cx);
            dlg.retry_last_failed(cx);
            assert_eq!(
                dlg.status_kind,
                StatusKind::Error,
                "still showing the error"
            );
        });
    }

    /// A retry whose account was removed in the meantime must not resurrect it.
    #[gpui::test]
    fn retrying_a_removed_account_stands_down(cx: &mut gpui::TestAppContext) {
        let a = profile("a@x.com", "t1");
        let (view, cx, _temp) = dialog(cx, std::slice::from_ref(&a));

        view.update(cx, |dlg, cx| {
            dlg.begin_credential_refresh(&a, cx);
            dlg.on_signin_error("network down", cx);

            // The account goes away while the failure is still on screen.
            dlg.registry.remove(&a.id);
            dlg.reload(cx);

            dlg.retry_last_failed(cx);
            assert_eq!(dlg.last_failed, None, "the retry stands down");
            assert_eq!(dlg.status_kind, StatusKind::Idle);
        });
    }

    #[test]
    fn status_kinds_map_to_distinct_registers() {
        // Idle and Busy share the muted register; Success and Error must not.
        assert_eq!(StatusKind::Idle.color(), StatusKind::Busy.color());
        assert_ne!(StatusKind::Error.color(), StatusKind::Idle.color());
        assert_ne!(StatusKind::Success.color(), StatusKind::Idle.color());
        assert_ne!(StatusKind::Error.color(), StatusKind::Success.color());
    }
}
