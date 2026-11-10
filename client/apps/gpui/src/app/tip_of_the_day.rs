//! Tip of the Day wiring on the workbench: the Help ▸ Tip of the Day… action
//! and the once-per-launch startup tip. The tip list, persistence, and the
//! dialog itself live in `crate::tip_of_the_day`.

use super::*;
use crate::tip_of_the_day::{TipOfTheDayDialog, TipOfTheDayEvent};

/// How long after the workbench is constructed the startup tip opens — long
/// enough that session restore, reconnects, and the first paint are not
/// competing with it.
#[cfg(not(any(test, target_arch = "wasm32")))]
const STARTUP_TIP_DELAY: std::time::Duration = std::time::Duration::from_millis(1200);

impl AppView {
    /// Help ▸ Tip of the Day… / command palette: open the dialog at the tip
    /// the next startup would show (or bring nothing new if it is open).
    pub(super) fn handle_tip_of_the_day(
        &mut self,
        _action: &TipOfTheDay,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.tip_of_the_day.is_some() {
            return;
        }
        self.open_tip_of_the_day(TipOfTheDayDialog::for_menu(), cx);
    }

    fn open_tip_of_the_day(&mut self, dialog: TipOfTheDayDialog, cx: &mut Context<Self>) {
        let view = cx.new(|_cx| dialog);
        cx.subscribe(
            &view,
            |this: &mut Self, _view, event: &TipOfTheDayEvent, cx| match event {
                TipOfTheDayEvent::Closed => {
                    this.overlays.tip_of_the_day = None;
                    cx.notify();
                }
            },
        )
        .detach();
        self.overlays.tip_of_the_day = Some(view);
        cx.notify();
    }

    /// Queue the startup tip. Desktop only (the browser preview is a demo
    /// surface, not a daily driver), never under `cargo test`, and at most
    /// once per process — see `tip_of_the_day::claim_startup_tip` for the
    /// preference and `SQLLY_SUPPRESS_TIP_OF_THE_DAY` gates.
    pub(super) fn schedule_startup_tip(&self, cx: &mut Context<Self>) {
        #[cfg(any(test, target_arch = "wasm32"))]
        let _ = cx;
        #[cfg(not(any(test, target_arch = "wasm32")))]
        {
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                async_cx
                    .background_executor()
                    .timer(STARTUP_TIP_DELAY)
                    .await;
                let mut cx = async_cx.clone();
                let _ = weak.update(&mut cx, |this: &mut Self, cx| {
                    if this.overlays.tip_of_the_day.is_some() {
                        return;
                    }
                    if let Some(index) = crate::tip_of_the_day::claim_startup_tip() {
                        this.open_tip_of_the_day(TipOfTheDayDialog::new(index), cx);
                    }
                });
            })
            .detach();
        }
    }
}
