//! Gesture interpreter — port of `GestureInterpreter.swift`.
//!
//! Pure, UI-free. Translates scroll deltas + modifier state into
//! `GestureScrollAction`s for the result container, based on the
//! `GesturePreferences`.

use crate::gesture_prefs::{GesturePreferences, QuerySwapGestureDirection};
use crate::result_layout::QueryResultLayoutSpec;

// std::time::Instant panics on wasm32; web-time is the drop-in replacement.
#[cfg(not(target_arch = "wasm32"))]
pub use std::time::{Duration, Instant};
#[cfg(target_arch = "wasm32")]
pub use web_time::{Duration, Instant};

pub const ACTIVATION_THRESHOLD: f32 = 24.0;
#[allow(dead_code)]
pub const QUERY_SWAP_ACTIVATION_THRESHOLD: f32 = 60.0;

/// Per-event |Δy| below this reads as the decaying tail of a momentum
/// stream rather than an active gesture.
pub const EDGE_SWITCH_IDLE_DELTA: f32 = 6.0;

/// A pause in above-idle events at least this long separates two gestures.
/// Momentum events arrive at frame rate (well under this) until the flick
/// truly dies, so a single flick — fingers plus momentum — stays one stream.
pub const EDGE_SWITCH_QUIET: Duration = Duration::from_millis(160);

#[allow(dead_code)]
/// The action the result container should take in response to a scroll event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureScrollAction {
    /// No special action — let the default scroll happen.
    AllowDefault,
    /// Scroll the entire stacked result grid container.
    ScrollStackContainer,
    /// Snap to the next/prev result card in the stack.
    SnapStackCard { direction: ScrollDirection },
    /// Switch to the next/prev result set.
    SwitchResultSet { direction: ScrollDirection },
    /// Advance the query switcher (swipe between tabs).
    AdvanceQuerySwitcher { direction: ScrollDirection },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

impl ScrollDirection {
    pub fn from_delta(delta_x: f32, delta_y: f32) -> Self {
        if delta_y.abs() >= delta_x.abs() {
            if delta_y > 0.0 { Self::Down } else { Self::Up }
        } else if delta_x > 0.0 {
            Self::Right
        } else {
            Self::Left
        }
    }
}

#[allow(dead_code)]
/// Edge state of the scroll container — which edges are "pinned."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EdgeState {
    pub at_top: bool,
    pub at_bottom: bool,
    pub at_left: bool,
    pub at_right: bool,
}

/// Accumulates scroll deltas across events so small gestures build up.
#[derive(Debug, Clone, Default)]
pub struct ScrollDeltaAccumulator {
    pub x: f32,
    pub y: f32,
}

impl ScrollDeltaAccumulator {
    pub fn add(&mut self, delta_x: f32, delta_y: f32) {
        self.x += delta_x;
        self.y += delta_y;
    }

    pub fn reset(&mut self) {
        self.x = 0.0;
        self.y = 0.0;
    }

    pub fn magnitude(&self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
}

/// Rate-limits edge-triggered result-set switches to one per gesture.
///
/// A trackpad flick delivers dozens of precise wheel events over the
/// activation threshold — including macOS momentum events that keep
/// arriving after the fingers lift. Without a latch, every one of them
/// switches a tab and the view cycles through all result sets. The gate
/// admits the first qualifying event, then swallows the rest of the
/// stream until it goes quiet ([`EDGE_SWITCH_QUIET`] with no above-idle
/// event) or the user deliberately reverses direction — momentum never
/// reverses, so a reversal is always a new intent.
#[derive(Debug, Clone, Default)]
pub struct EdgeSwitchGate {
    latched: Option<(ScrollDirection, Instant)>,
}

impl EdgeSwitchGate {
    /// Feed one precise wheel event; returns `true` when a switch toward
    /// `dir` should happen now. `at_edge` says the grid is pinned at the
    /// edge that `dir` scrolls past; `delta_y` is this event's Δy.
    pub fn admit(
        &mut self,
        dir: ScrollDirection,
        at_edge: bool,
        delta_y: f32,
        now: Instant,
    ) -> bool {
        let active = delta_y.abs() >= EDGE_SWITCH_IDLE_DELTA;
        if let Some((latched_dir, last_active)) = self.latched {
            let quiet = now.duration_since(last_active) > EDGE_SWITCH_QUIET;
            if dir == latched_dir && active && !quiet {
                // Same stream still running: refresh and swallow.
                self.latched = Some((latched_dir, now));
                return false;
            }
            // Reversal, quiet gap, or momentum tail: the gesture that
            // caused the switch is over.
            self.latched = None;
        }
        if at_edge && delta_y.abs() > ACTIVATION_THRESHOLD {
            self.latched = Some((dir, now));
            return true;
        }
        false
    }

    /// Forget any in-flight gesture (e.g. when the result sets change).
    pub fn reset(&mut self) {
        self.latched = None;
    }
}

#[allow(dead_code)]
/// Computes the gesture action for the result container.
///
/// - `layout`: the current result layout (for multi-result-set detection).
/// - `delta_x`/`delta_y`: scroll deltas for this event.
/// - `is_precise`: whether the input is a precise trackpad (vs. imprecise mouse
///   wheel).
/// - `modifier_held`: whether the configured modifier is held.
/// - `prefs`: the gesture preferences.
/// - `edge_state`: which edges the content is pinned to.
/// - `threshold`: activation threshold override (defaults to
///   `ACTIVATION_THRESHOLD`).
#[allow(clippy::too_many_arguments)]
pub fn result_action(
    layout: &QueryResultLayoutSpec,
    delta_x: f32,
    delta_y: f32,
    is_precise: bool,
    modifier_held: bool,
    prefs: &GesturePreferences,
    edge_state: EdgeState,
    threshold: Option<f32>,
) -> GestureScrollAction {
    let thr = threshold.unwrap_or(ACTIVATION_THRESHOLD);
    let abs_y = delta_y.abs();
    let _abs_x = delta_x.abs();

    // Multi-result-set snapping: when a modifier is held and scrolling, snap
    // between result cards.
    if modifier_held && abs_y > thr {
        let dir = ScrollDirection::from_delta(delta_x, delta_y);
        return GestureScrollAction::SnapStackCard { direction: dir };
    }

    // Two-finger scroll with stacked results: switch result sets at edges.
    if prefs.results_two_finger_scroll_enabled && is_precise && layout.has_layout() && abs_y > thr {
        let dir = ScrollDirection::from_delta(delta_x, delta_y);
        // Only switch when at the edge of the current result.
        let can_advance_down = dir == ScrollDirection::Down && edge_state.at_bottom;
        let can_advance_up = dir == ScrollDirection::Up && edge_state.at_top;
        if can_advance_down || can_advance_up {
            return GestureScrollAction::SwitchResultSet { direction: dir };
        }
    }

    GestureScrollAction::AllowDefault
}

#[allow(dead_code)]
/// Computes the gesture action for the query tab swapper.
///
/// Returns `None` when query-swapping is disabled or the threshold isn't met.
pub fn query_action(
    delta_x: f32,
    delta_y: f32,
    prefs: &GesturePreferences,
    threshold: Option<f32>,
) -> Option<GestureScrollAction> {
    if prefs.query_swap_direction == QuerySwapGestureDirection::Disabled {
        return None;
    }
    let thr = threshold.unwrap_or(QUERY_SWAP_ACTIVATION_THRESHOLD);
    match prefs.query_swap_direction {
        QuerySwapGestureDirection::Vertical => {
            if delta_y.abs() > thr {
                let dir = if delta_y > 0.0 {
                    ScrollDirection::Down
                } else {
                    ScrollDirection::Up
                };
                Some(GestureScrollAction::AdvanceQuerySwitcher { direction: dir })
            } else {
                None
            }
        }
        QuerySwapGestureDirection::Horizontal => {
            if delta_x.abs() > thr {
                let dir = if delta_x > 0.0 {
                    ScrollDirection::Right
                } else {
                    ScrollDirection::Left
                };
                Some(GestureScrollAction::AdvanceQuerySwitcher { direction: dir })
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_direction_from_delta() {
        assert_eq!(ScrollDirection::from_delta(0.0, -10.0), ScrollDirection::Up);
        assert_eq!(
            ScrollDirection::from_delta(0.0, 10.0),
            ScrollDirection::Down
        );
        assert_eq!(
            ScrollDirection::from_delta(10.0, 0.0),
            ScrollDirection::Right
        );
        assert_eq!(
            ScrollDirection::from_delta(-10.0, 0.0),
            ScrollDirection::Left
        );
        // Y dominates when both present.
        assert_eq!(ScrollDirection::from_delta(5.0, -10.0), ScrollDirection::Up);
    }

    #[test]
    fn accumulator_adds_and_resets() {
        let mut acc = ScrollDeltaAccumulator::default();
        acc.add(10.0, -5.0);
        assert_eq!(acc.x, 10.0);
        assert_eq!(acc.y, -5.0);
        acc.add(2.0, 3.0);
        assert_eq!(acc.x, 12.0);
        assert_eq!(acc.y, -2.0);
        acc.reset();
        assert_eq!(acc.x, 0.0);
        assert_eq!(acc.y, 0.0);
    }

    #[test]
    fn accumulator_magnitude() {
        let mut acc = ScrollDeltaAccumulator::default();
        acc.add(3.0, 4.0);
        assert_eq!(acc.magnitude(), 5.0);
    }

    #[test]
    fn default_action_allow_default() {
        let layout = QueryResultLayoutSpec::none();
        let prefs = GesturePreferences::defaults();
        let action = result_action(
            &layout,
            0.0,
            5.0,
            true,
            false,
            &prefs,
            EdgeState::default(),
            None,
        );
        assert_eq!(action, GestureScrollAction::AllowDefault);
    }

    #[test]
    fn modifier_held_snaps_stack() {
        let layout = QueryResultLayoutSpec::none();
        let prefs = GesturePreferences::defaults();
        let action = result_action(
            &layout,
            0.0,
            30.0,
            true,
            true,
            &prefs,
            EdgeState::default(),
            None,
        );
        assert_eq!(
            action,
            GestureScrollAction::SnapStackCard {
                direction: ScrollDirection::Down
            }
        );
    }

    #[test]
    fn query_action_disabled() {
        let prefs = GesturePreferences::defaults();
        assert_eq!(query_action(0.0, 100.0, &prefs, None), None);
    }

    #[test]
    fn query_action_vertical() {
        let prefs = GesturePreferences {
            query_swap_direction: QuerySwapGestureDirection::Vertical,
            ..GesturePreferences::defaults()
        };
        let action = query_action(0.0, 100.0, &prefs, None);
        assert_eq!(
            action,
            Some(GestureScrollAction::AdvanceQuerySwitcher {
                direction: ScrollDirection::Down
            })
        );
    }

    #[test]
    fn query_action_horizontal() {
        let prefs = GesturePreferences {
            query_swap_direction: QuerySwapGestureDirection::Horizontal,
            ..GesturePreferences::defaults()
        };
        let action = query_action(100.0, 0.0, &prefs, None);
        assert_eq!(
            action,
            Some(GestureScrollAction::AdvanceQuerySwitcher {
                direction: ScrollDirection::Right
            })
        );
    }

    #[test]
    fn query_action_below_threshold() {
        let prefs = GesturePreferences {
            query_swap_direction: QuerySwapGestureDirection::Vertical,
            ..GesturePreferences::defaults()
        };
        assert_eq!(query_action(0.0, 10.0, &prefs, None), None);
    }

    #[test]
    fn edge_state_default() {
        let e = EdgeState::default();
        assert!(!e.at_top);
        assert!(!e.at_bottom);
        assert!(!e.at_left);
        assert!(!e.at_right);
    }

    const _: () = {
        assert!(ACTIVATION_THRESHOLD > 0.0);
        assert!(QUERY_SWAP_ACTIVATION_THRESHOLD > ACTIVATION_THRESHOLD);
        assert!(EDGE_SWITCH_IDLE_DELTA < ACTIVATION_THRESHOLD);
    };

    /// A flick's event stream: strong deltas decaying at frame-rate spacing,
    /// the shape a trackpad flick with momentum actually produces.
    fn flick(
        gate: &mut EdgeSwitchGate,
        dir: ScrollDirection,
        start: Instant,
        deltas: &[f32],
    ) -> usize {
        let mut admitted = 0;
        for (i, d) in deltas.iter().enumerate() {
            let now = start + Duration::from_millis(8 * i as u64);
            if gate.admit(dir, true, *d, now) {
                admitted += 1;
            }
        }
        admitted
    }

    #[test]
    fn gate_admits_one_switch_per_flick() {
        let mut gate = EdgeSwitchGate::default();
        let t0 = Instant::now();
        // Finger events plus a long momentum tail, all above threshold.
        let deltas = [40.0, 60.0, 80.0, 70.0, 55.0, 45.0, 38.0, 30.0, 26.0];
        assert_eq!(
            flick(&mut gate, ScrollDirection::Down, t0, &deltas),
            1,
            "a single flick must switch exactly once, not once per event"
        );
    }

    #[test]
    fn gate_rearms_after_quiet_gap() {
        let mut gate = EdgeSwitchGate::default();
        let t0 = Instant::now();
        assert_eq!(
            flick(&mut gate, ScrollDirection::Down, t0, &[50.0, 40.0]),
            1
        );
        let t1 = t0 + EDGE_SWITCH_QUIET + Duration::from_millis(50);
        assert_eq!(
            flick(&mut gate, ScrollDirection::Down, t1, &[50.0, 40.0]),
            1,
            "a new flick after a pause is a new gesture"
        );
    }

    #[test]
    fn gate_rearms_after_momentum_decays_to_idle() {
        let mut gate = EdgeSwitchGate::default();
        let t0 = Instant::now();
        // Flick whose momentum decays into the sub-idle tail.
        let deltas = [50.0, 35.0, 20.0, 9.0, 4.0, 2.0];
        assert_eq!(flick(&mut gate, ScrollDirection::Down, t0, &deltas), 1);
        // Immediate second flick (no quiet gap needed — the tail already
        // told the gate the previous gesture died).
        let t1 = t0 + Duration::from_millis(8 * deltas.len() as u64);
        assert!(gate.admit(ScrollDirection::Down, true, 50.0, t1));
    }

    #[test]
    fn gate_reversal_switches_immediately() {
        let mut gate = EdgeSwitchGate::default();
        let t0 = Instant::now();
        assert!(gate.admit(ScrollDirection::Down, true, 50.0, t0));
        // Deliberate reverse scroll milliseconds later: momentum never
        // reverses, so this is new intent and must not be swallowed.
        let t1 = t0 + Duration::from_millis(30);
        assert!(gate.admit(ScrollDirection::Up, true, -50.0, t1));
    }

    #[test]
    fn gate_requires_edge_and_threshold() {
        let mut gate = EdgeSwitchGate::default();
        let t0 = Instant::now();
        assert!(
            !gate.admit(ScrollDirection::Down, false, 50.0, t0),
            "off-edge scrolls never switch"
        );
        assert!(
            !gate.admit(ScrollDirection::Down, true, 10.0, t0),
            "gentle scrolls below the activation threshold never switch"
        );
    }

    #[test]
    fn gate_reset_forgets_latch() {
        let mut gate = EdgeSwitchGate::default();
        let t0 = Instant::now();
        assert!(gate.admit(ScrollDirection::Down, true, 50.0, t0));
        gate.reset();
        let t1 = t0 + Duration::from_millis(8);
        assert!(gate.admit(ScrollDirection::Down, true, 50.0, t1));
    }
}
