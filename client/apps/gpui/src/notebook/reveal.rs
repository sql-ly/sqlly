//! Scrolling a cell into the place it is most usable.
//!
//! Selecting, jumping to, inserting or editing a cell asks for a reveal; the
//! scroll itself happens when the notebook next draws, because only then are
//! the cell's real height and the viewport known. A new cell or one whose
//! height just changed is measured by one frame and placed on the next.

use gpui::{Context, ListOffset, Window, px};

use super::view::NotebookView;

/// Why a cell is being brought into view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Reveal {
    /// The cell became the selected one: show all of it, and leave the page
    /// alone while the viewport is already inside a cell taller than itself.
    Select,
    /// The cell is about to be typed in: its header and editor come to the
    /// top even when the viewport was further down the same cell.
    Edit,
}

/// A reveal waiting for the next frames.
#[derive(Clone, Debug)]
pub(super) struct PendingReveal {
    id: String,
    mode: Reveal,
    /// Frames this reveal may still scroll on, so a cell that keeps changing
    /// height (or sits at the end of the list) cannot scroll forever.
    passes: u8,
}

const PASSES: u8 = 6;
/// Distances under this are already in place.
const SETTLED: f32 = 0.5;

/// How far the list scrolls (positive moves the content up) for a cell whose
/// top edge is `top` below the viewport's top, `height` tall, in a viewport
/// `viewport` tall.
///
/// A cell that fits moves the least that shows all of it. A taller cell is
/// put with its top at the top of the viewport.
pub(crate) fn reveal_delta(mode: Reveal, top: f32, height: f32, viewport: f32) -> f32 {
    let bottom = top + height;
    if height <= viewport {
        if top < 0.0 {
            top
        } else if bottom > viewport {
            bottom - viewport
        } else {
            0.0
        }
    } else if mode == Reveal::Select && top <= 0.0 && bottom >= viewport {
        0.0
    } else {
        top
    }
}

impl NotebookView {
    /// Bring cell `id` into view when the notebook next draws.
    pub(crate) fn reveal(&mut self, id: &str, mode: Reveal, cx: &mut Context<Self>) {
        if self.doc.index_of(id).is_none() {
            return;
        }
        let mode = match &self.pending_reveal {
            Some(pending) if pending.id == id => pending.mode.max(mode),
            _ => mode,
        };
        self.pending_reveal = Some(PendingReveal {
            id: id.to_string(),
            mode,
            passes: PASSES,
        });
        cx.notify();
    }

    /// The user scrolled by hand: a reveal still settling gives way.
    pub(super) fn cancel_reveal(&mut self) {
        self.pending_reveal = None;
    }

    /// Scroll toward the pending reveal with what the last frame measured.
    /// Called while drawing, before the list is laid out.
    pub(super) fn settle_reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(mut pending) = self.pending_reveal.take() else {
            return;
        };
        let Some(index) = self.doc.index_of(&pending.id) else {
            return;
        };
        let viewport = self.list_state.viewport_bounds();
        let height = f32::from(viewport.size.height);
        let scroll_top = self.list_state.logical_scroll_top();
        let offset = |edge: gpui::Pixels| f32::from(edge - viewport.top());
        let top_align = ListOffset {
            item_ix: index,
            offset_in_item: px(0.0),
        };
        // Whether this frame scrolled or is waiting to measure the cell.
        let settling = if height <= 0.0 {
            // Nothing is laid out yet.
            true
        } else if index < scroll_top.item_ix {
            // Above the viewport: its top goes to the top, whatever its size.
            self.list_state.scroll_to(top_align);
            true
        } else if let Some(bounds) = self.list_state.bounds_for_item(index) {
            let delta = reveal_delta(
                pending.mode,
                offset(bounds.top()),
                f32::from(bounds.size.height),
                height,
            );
            if delta.abs() >= SETTLED {
                self.list_state.scroll_by(px(delta));
            }
            delta.abs() >= SETTLED
        } else {
            // Not measured yet: new, its height just changed, or never drawn.
            // It is measured by the frame that has it in or near the viewport
            // and placed on the next one.
            let above = index
                .checked_sub(1)
                .and_then(|prev| self.list_state.bounds_for_item(prev));
            match above {
                // The cell starts right under a measured one: at most bring
                // that edge to the bottom of the viewport.
                Some(prev) if offset(prev.bottom()) > height => {
                    self.list_state
                        .scroll_by(px(offset(prev.bottom()) - height));
                }
                Some(_) => {}
                // The first cell on screen is drawn where it is.
                None if index == scroll_top.item_ix => {}
                // Nothing measured in between (a jump across cells that were
                // never drawn): it goes to the top.
                None => self.list_state.scroll_to(top_align),
            }
            true
        };
        // An editor that is still on its way changes the cell's height when
        // it arrives, so the reveal waits for it.
        let waiting = self.awaits_editor(&pending.id);
        if settling {
            pending.passes -= 1;
            if pending.passes > 0 {
                self.pending_reveal = Some(pending);
                cx.on_next_frame(window, |_, _, cx| cx.notify());
            }
        } else if waiting {
            self.pending_reveal = Some(pending);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Reveal, reveal_delta};

    #[test]
    fn a_fully_visible_cell_does_not_move() {
        for mode in [Reveal::Select, Reveal::Edit] {
            assert_eq!(reveal_delta(mode, 0.0, 200.0, 800.0), 0.0);
            assert_eq!(reveal_delta(mode, 600.0, 200.0, 800.0), 0.0);
        }
    }

    #[test]
    fn a_cell_that_fits_moves_the_least_that_shows_all_of_it() {
        // Clipped at the top: its top comes to the top.
        assert_eq!(reveal_delta(Reveal::Select, -120.0, 200.0, 800.0), -120.0);
        // Clipped at the bottom: its bottom comes to the bottom.
        assert_eq!(reveal_delta(Reveal::Select, 700.0, 200.0, 800.0), 100.0);
        // Wholly below.
        assert_eq!(reveal_delta(Reveal::Edit, 1500.0, 200.0, 800.0), 900.0);
    }

    #[test]
    fn a_cell_taller_than_the_viewport_shows_its_top() {
        // Coming from above, the plain "show the bottom" reveal would hide
        // the header and the SQL.
        assert_eq!(reveal_delta(Reveal::Select, 500.0, 2000.0, 800.0), 500.0);
        // Coming from below.
        assert_eq!(
            reveal_delta(Reveal::Select, -1700.0, 2000.0, 800.0),
            -1700.0
        );
    }

    #[test]
    fn reading_down_a_tall_cell_is_kept_until_it_is_edited() {
        assert_eq!(reveal_delta(Reveal::Select, -600.0, 2000.0, 800.0), 0.0);
        assert_eq!(reveal_delta(Reveal::Edit, -600.0, 2000.0, 800.0), -600.0);
    }
}
