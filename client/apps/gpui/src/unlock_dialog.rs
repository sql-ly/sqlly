//! "Unlock SQLLY": the Mac App Store edition's purchase window.
//!
//! Shows the lifetime unlock with its localized App Store price, a Buy button
//! that runs Apple's purchase sheet, and Restore Purchases (required by App
//! Review for non-consumables). Opened from SQLLY ▸ Unlock SQLLY…, from every
//! free-tier surface that would otherwise link to the web store, and when a
//! free-tier limit stops an action (with `reason` explaining which).

use gpui::{Context, EventEmitter, IntoElement, Render, Styled, Window, div, prelude::*, px};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};

use crate::app_store::{ProductInfo, PurchaseOutcome, RestoreOutcome};
use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub enum UnlockDialogEvent {
    Closed,
    /// The unlock is now owned; the workbench refreshes license-gated UI.
    Unlocked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProductState {
    Loading,
    Ready(ProductInfo),
    Failed(crate::app_store::StoreKitError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Activity {
    Idle,
    Purchasing,
    Restoring,
}

pub struct UnlockDialog {
    frame: DialogFrame,
    reason: Option<String>,
    product: ProductState,
    activity: Activity,
    /// Outcome line under the buttons ("Purchase cancelled.", errors, …).
    message: Option<String>,
}

impl EventEmitter<UnlockDialogEvent> for UnlockDialog {}

/// The fallback price shown while (or if) the App Store can't be reached.
const LIST_PRICE: &str = "$69.99";

/// What the unlock includes, one line each.
pub const UNLOCK_BENEFITS: [&str; 3] = [
    "unlock.benefits.unlimited_connections",
    "unlock.benefits.no_reminders",
    "unlock.benefits.one_time_purchase",
];

/// Status line for a finished purchase. Pure.
pub fn purchase_message(outcome: &PurchaseOutcome) -> Option<String> {
    match outcome {
        PurchaseOutcome::Purchased => None,
        PurchaseOutcome::Pending => Some(crate::localization::text("unlock.purchase.pending")),
        PurchaseOutcome::Cancelled => Some(crate::localization::text("unlock.purchase.cancelled")),
        PurchaseOutcome::Failed(error) => Some(crate::localization::format(
            "unlock.purchase.failed",
            &[("error", crate::localization::message(error))],
        )),
    }
}

/// Status line for a finished restore. Pure.
pub fn restore_message(outcome: &RestoreOutcome) -> Option<String> {
    match outcome {
        RestoreOutcome::Restored => None,
        RestoreOutcome::NothingToRestore => {
            Some(crate::localization::text("unlock.restore.not_found"))
        }
        RestoreOutcome::Failed(error) => Some(crate::localization::format(
            "unlock.restore.failed",
            &[("error", crate::localization::message(error))],
        )),
    }
}

impl UnlockDialog {
    /// Open the window and start loading the product from the App Store.
    pub fn new(reason: Option<String>, cx: &mut Context<Self>) -> Self {
        let this = Self {
            frame: DialogFrame::new_auto(Theme::scaled(460.0)),
            reason,
            product: ProductState::Loading,
            activity: Activity::Idle,
            message: None,
        };
        cx.spawn(async move |weak, cx| {
            let loaded = cx
                .background_spawn(async move { crate::app_store::load_product() })
                .await;
            weak.update(cx, |this, cx| {
                this.product = match loaded {
                    Ok(info) => ProductState::Ready(info),
                    Err(error) => ProductState::Failed(error),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        this
    }

    fn buy(&mut self, cx: &mut Context<Self>) {
        if self.activity != Activity::Idle {
            return;
        }
        self.activity = Activity::Purchasing;
        self.message = None;
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let outcome = cx
                .background_spawn(async move { crate::app_store::purchase() })
                .await;
            weak.update(cx, |this, cx| {
                this.activity = Activity::Idle;
                this.message = purchase_message(&outcome);
                if outcome == PurchaseOutcome::Purchased {
                    cx.emit(UnlockDialogEvent::Unlocked);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn restore(&mut self, cx: &mut Context<Self>) {
        if self.activity != Activity::Idle {
            return;
        }
        self.activity = Activity::Restoring;
        self.message = None;
        cx.notify();
        cx.spawn(async move |weak, cx| {
            let outcome = cx
                .background_spawn(async move { crate::app_store::restore() })
                .await;
            weak.update(cx, |this, cx| {
                this.activity = Activity::Idle;
                this.message = restore_message(&outcome);
                if outcome == RestoreOutcome::Restored {
                    cx.emit(UnlockDialogEvent::Unlocked);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

impl Render for UnlockDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let owned = crate::license::is_paying();
        let (title, price, description) = match &self.product {
            ProductState::Ready(info) => (
                info.display_name.clone(),
                info.display_price.clone(),
                Some(info.description.clone()).filter(|d| !d.trim().is_empty()),
            ),
            _ => (
                crate::localization::text("unlock.product.fallback_title"),
                LIST_PRICE.to_string(),
                None,
            ),
        };

        let mut body = div().flex().flex_col().gap(px(s(10.0)));
        if let Some(reason) = self.reason.clone().filter(|_| !owned) {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(reason),
            );
        }
        body = body.child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .justify_between()
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(1.15)))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(Theme::text())
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(1.15)))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(Theme::accent())
                        .child(price),
                ),
        );
        if let Some(description) = description {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(description),
            );
        }
        for benefit in UNLOCK_BENEFITS {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(crate::localization::format(
                        "unlock.benefits.item",
                        &[("benefit", &crate::localization::text(benefit))],
                    )),
            );
        }
        let status = if owned {
            Some(crate::localization::text("unlock.status.unlocked"))
        } else {
            match (&self.activity, &self.product) {
                (Activity::Purchasing, _) => {
                    Some(crate::localization::text("unlock.status.purchasing"))
                }
                (Activity::Restoring, _) => {
                    Some(crate::localization::text("unlock.status.restoring"))
                }
                (_, ProductState::Loading) => {
                    Some(crate::localization::text("unlock.status.loading_price"))
                }
                (_, ProductState::Failed(error)) if self.message.is_none() => {
                    Some(crate::localization::format(
                        "unlock.status.price_failed",
                        &[("error", crate::localization::message(error))],
                    ))
                }
                _ => self.message.clone(),
            }
        };
        if let Some(status) = status {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text_dim())
                    .child(status),
            );
        }
        if let Some(problem) = crate::app_store::account_link_problem() {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::warning())
                    .child(crate::localization::message(problem)),
            );
        }

        let busy = self.activity != Activity::Idle;
        let can_buy = !owned && !busy && matches!(self.product, ProductState::Ready(_));
        let mut dialog = Dialog::new("unlock-sqlly", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("unlock.dialog.title"))
            .easy_close(true)
            .on_close(|_this, _window, cx| cx.emit(UnlockDialogEvent::Closed))
            .child(body);
        if !owned {
            dialog = dialog
                .leading_button(
                    Button::new("unlock-restore")
                        .label(crate::localization::text("unlock.actions.restore"))
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _window, cx| this.restore(cx))),
                )
                .button(
                    Button::new("unlock-buy")
                        .primary()
                        .label(crate::localization::text("unlock.actions.buy"))
                        .disabled(!can_buy)
                        .on_click(cx.listener(|this, _, _window, cx| this.buy(cx))),
                );
        }
        dialog.build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purchase_messages_cover_every_unfinished_outcome() {
        assert_eq!(purchase_message(&PurchaseOutcome::Purchased), None);
        assert!(
            purchase_message(&PurchaseOutcome::Pending)
                .unwrap()
                .contains("waiting for approval")
        );
        assert_eq!(
            purchase_message(&PurchaseOutcome::Cancelled).as_deref(),
            Some("Purchase cancelled.")
        );
        assert!(
            purchase_message(&PurchaseOutcome::Failed("declined".into()))
                .unwrap()
                .ends_with("declined")
        );
    }

    #[test]
    fn restore_messages_tell_nothing_found_apart_from_errors() {
        assert_eq!(restore_message(&RestoreOutcome::Restored), None);
        assert!(
            restore_message(&RestoreOutcome::NothingToRestore)
                .unwrap()
                .contains("No SQLLY purchase")
        );
        assert!(
            restore_message(&RestoreOutcome::Failed("offline".into()))
                .unwrap()
                .contains("offline")
        );
    }

    #[test]
    fn benefits_lead_with_unlimited_connections() {
        assert!(
            crate::localization::text(UNLOCK_BENEFITS[0]).contains("Unlimited saved connections")
        );
    }
}
