//! Editor text-zoom logic: a pure, UI-agnostic model for the zoom control
//! rendered in the bottom-right corner of the query editor. The zoom is a
//! multiplier applied to the configured editor font size (100% == the
//! preference value). All math lives here so it can be unit-tested without a
//! GPUI window/context.

/// Minimum zoom multiplier (50%).
pub const MIN_ZOOM: f32 = 0.5;
/// Maximum zoom multiplier (300%).
pub const MAX_ZOOM: f32 = 3.0;
/// Default zoom multiplier (100%).
pub const DEFAULT_ZOOM: f32 = 1.0;
/// Step applied by the − / + buttons (10 percentage points per click).
pub const ZOOM_STEP: f32 = 0.1;

/// Clamp an arbitrary zoom multiplier into the supported range.
pub fn clamp_zoom(zoom: f32) -> f32 {
    // `f32::clamp` panics on NaN; guard so a bad input degrades to the default.
    if zoom.is_nan() {
        return DEFAULT_ZOOM;
    }
    zoom.clamp(MIN_ZOOM, MAX_ZOOM)
}

/// Zoom multiplier one step larger, clamped to the max.
pub fn zoom_in(zoom: f32) -> f32 {
    clamp_zoom(clamp_zoom(zoom) + ZOOM_STEP)
}

/// Zoom multiplier one step smaller, clamped to the min.
pub fn zoom_out(zoom: f32) -> f32 {
    clamp_zoom(clamp_zoom(zoom) - ZOOM_STEP)
}

/// Convert a zoom multiplier into a whole-number percentage (1.0 -> 100).
pub fn zoom_to_percent(zoom: f32) -> u32 {
    (clamp_zoom(zoom) * 100.0).round() as u32
}

/// Effective font size for a base (preference) size at the given zoom.
pub fn zoomed_font_size(base_size: f32, zoom: f32) -> f32 {
    base_size * clamp_zoom(zoom)
}

/// Accumulate a wheel delta (in line units) and return whole zoom steps.
/// Fractional leftovers stay in `acc` so a pixel-precise trackpad does not fire a step on every tiny event.
pub fn wheel_zoom_steps(acc: &mut f32, delta_lines: f32) -> i32 {
    if !delta_lines.is_finite() {
        return 0;
    }
    *acc += delta_lines;
    if !acc.is_finite() {
        *acc = 0.0;
        return 0;
    }
    let steps = acc.trunc() as i32;
    *acc -= steps as f32;
    steps
}

/// Accumulate a pinch magnification delta and return at most one zoom step.
/// Leftover fraction stays in `acc` until it crosses [`ZOOM_STEP`].
pub fn pinch_zoom_steps(acc: &mut f32, delta: f32) -> i32 {
    if !delta.is_finite() {
        return 0;
    }
    *acc += delta;
    if !acc.is_finite() {
        *acc = 0.0;
        return 0;
    }
    if acc.abs() < ZOOM_STEP {
        return 0;
    }
    let raw = (*acc / ZOOM_STEP).trunc();
    let steps = raw.clamp(-1.0, 1.0) as i32;
    *acc -= steps as f32 * ZOOM_STEP;
    steps
}

/// Slider handle position as a 0.0..=1.0 fraction of the track for a zoom.
#[allow(dead_code)]
pub fn zoom_to_fraction(zoom: f32) -> f32 {
    ((clamp_zoom(zoom) - MIN_ZOOM) / (MAX_ZOOM - MIN_ZOOM)).clamp(0.0, 1.0)
}

/// Zoom multiplier for a slider fraction (0.0 -> MIN_ZOOM, 1.0 -> MAX_ZOOM).
/// The fraction is clamped so out-of-track drag positions saturate at the ends.
#[allow(dead_code)]
pub fn fraction_to_zoom(fraction: f32) -> f32 {
    let f = if fraction.is_nan() {
        0.0
    } else {
        fraction.clamp(0.0, 1.0)
    };
    clamp_zoom(MIN_ZOOM + f * (MAX_ZOOM - MIN_ZOOM))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn clamp_bounds_and_nan() {
        assert!(approx(clamp_zoom(0.1), MIN_ZOOM));
        assert!(approx(clamp_zoom(9.0), MAX_ZOOM));
        assert!(approx(clamp_zoom(1.25), 1.25));
        assert!(approx(clamp_zoom(f32::NAN), DEFAULT_ZOOM));
    }

    #[test]
    fn step_in_out_clamps_at_edges() {
        assert!(approx(zoom_in(1.0), 1.1));
        assert!(approx(zoom_out(1.0), 0.9));
        // Saturates, never exceeds bounds.
        assert!(approx(zoom_in(MAX_ZOOM), MAX_ZOOM));
        assert!(approx(zoom_out(MIN_ZOOM), MIN_ZOOM));
    }

    #[test]
    fn percent_rounds_to_whole_number() {
        assert_eq!(zoom_to_percent(1.0), 100);
        assert_eq!(zoom_to_percent(2.2), 220);
        assert_eq!(zoom_to_percent(0.5), 50);
        assert_eq!(zoom_to_percent(3.0), 300);
    }

    #[test]
    fn zoomed_font_size_multiplies_base() {
        assert!(approx(zoomed_font_size(13.0, 1.0), 13.0));
        assert!(approx(zoomed_font_size(13.0, 2.0), 26.0));
        // Base is scaled by the clamped zoom, not the raw input.
        assert!(approx(zoomed_font_size(10.0, 100.0), 10.0 * MAX_ZOOM));
    }

    #[test]
    fn fraction_zoom_roundtrip() {
        assert!(approx(zoom_to_fraction(MIN_ZOOM), 0.0));
        assert!(approx(zoom_to_fraction(MAX_ZOOM), 1.0));
        // Midpoint of the range maps to fraction 0.5.
        let mid = MIN_ZOOM + (MAX_ZOOM - MIN_ZOOM) / 2.0;
        assert!(approx(zoom_to_fraction(mid), 0.5));

        assert!(approx(fraction_to_zoom(0.0), MIN_ZOOM));
        assert!(approx(fraction_to_zoom(1.0), MAX_ZOOM));
        assert!(approx(fraction_to_zoom(0.5), mid));
    }

    #[test]
    fn fraction_saturates_out_of_range() {
        assert!(approx(fraction_to_zoom(-1.0), MIN_ZOOM));
        assert!(approx(fraction_to_zoom(2.0), MAX_ZOOM));
        assert!(approx(fraction_to_zoom(f32::NAN), MIN_ZOOM));
    }

    #[test]
    fn wheel_zoom_steps_accumulate_and_keep_fraction() {
        let mut acc = 0.0;
        assert_eq!(wheel_zoom_steps(&mut acc, 0.4), 0);
        assert!(approx(acc, 0.4));
        assert_eq!(wheel_zoom_steps(&mut acc, 0.7), 1);
        assert!(approx(acc, 0.1));
        assert_eq!(wheel_zoom_steps(&mut acc, -2.3), -2);
        assert!(approx(acc, -0.2));
    }

    #[test]
    fn pinch_zoom_steps_one_step_per_burst() {
        let mut acc = 0.0;
        assert_eq!(pinch_zoom_steps(&mut acc, 0.05), 0);
        assert_eq!(pinch_zoom_steps(&mut acc, 0.06), 1);
        assert!(approx(acc, 0.01));
        // A large burst still yields a single step; leftover stays queued.
        assert_eq!(pinch_zoom_steps(&mut acc, 0.5), 1);
        assert!(approx(acc, 0.41));
    }
}
