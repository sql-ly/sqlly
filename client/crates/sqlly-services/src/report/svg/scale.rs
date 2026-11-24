//! Axis scales, "nice" tick values and text measurement for the charts.

use chrono::{DateTime, Datelike, Months, NaiveDate, NaiveDateTime};

/// Advance widths of ASCII 0x20..=0x7E in Noto Sans, per 1000 em (the
/// report's PDF font, so SVG and PDF lay out alike).
const ASCII_ADVANCE: [u16; 95] = [
    260, 269, 408, 646, 572, 831, 732, 225, 300, 300, 551, 572, 268, 322, 268, 372, 572, 572, 572,
    572, 572, 572, 572, 572, 572, 572, 268, 268, 572, 572, 572, 434, 899, 639, 650, 632, 730, 556,
    519, 728, 741, 339, 273, 619, 524, 907, 760, 781, 605, 781, 622, 549, 556, 731, 600, 930, 586,
    566, 572, 329, 372, 329, 572, 444, 281, 561, 615, 480, 615, 564, 344, 615, 618, 258, 258, 534,
    258, 935, 618, 605, 615, 615, 413, 479, 361, 618, 508, 786, 529, 510, 470, 380, 551, 380, 572,
];

fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F | 0x2E80..=0x303E | 0x3041..=0x33FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F | 0x20000..=0x2FFFD)
}

/// Estimated width of `text` at `size` px.
pub fn text_width(text: &str, size: f32) -> f32 {
    let units: u32 = text
        .chars()
        .map(|c| match c as u32 {
            0x20..=0x7E => u32::from(ASCII_ADVANCE[(c as usize) - 0x20]),
            _ if is_wide(c) => 1000,
            0x300..=0x36F | 0x200B..=0x200F => 0,
            _ => 600,
        })
        .sum();
    units as f32 / 1000.0 * size
}

/// `text` cut with an ellipsis to fit `max` px.
pub fn fit_text(text: &str, size: f32, max: f32) -> String {
    if text_width(text, size) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        let candidate = format!("{out}{c}…");
        if text_width(&candidate, size) > max {
            break;
        }
        out.push(c);
    }
    if out.is_empty() {
        return String::new();
    }
    format!("{out}…")
}

/// A tick step of 1, 2, 2.5 or 5 × 10ⁿ giving about `target` ticks.
pub fn nice_step(span: f64, target: usize) -> f64 {
    if !(span.is_finite() && span > 0.0) {
        return 1.0;
    }
    let raw = span / target.max(1) as f64;
    let magnitude = 10f64.powf(raw.log10().floor());
    let residual = raw / magnitude;
    let nice = if residual <= 1.0 {
        1.0
    } else if residual <= 2.0 {
        2.0
    } else if residual <= 2.5 {
        2.5
    } else if residual <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

/// Domain widened to whole steps, and the ticks across it.
pub fn nice_ticks(min: f64, max: f64, target: usize) -> (f64, f64, Vec<f64>) {
    let (mut lo, mut hi) = (min.min(max), min.max(max));
    if !lo.is_finite() || !hi.is_finite() {
        return (0.0, 1.0, vec![0.0, 1.0]);
    }
    if lo == hi {
        if lo == 0.0 {
            hi = 1.0;
        } else {
            let pad = lo.abs() * 0.1;
            lo -= pad;
            hi += pad;
        }
    }
    let step = nice_step(hi - lo, target);
    let start = (lo / step).floor() * step;
    let end = (hi / step).ceil() * step;
    let mut ticks = Vec::new();
    let count = ((end - start) / step).round() as i64;
    for i in 0..=count.min(100) {
        let v = start + step * i as f64;
        ticks.push(if v.abs() < step * 1e-9 {
            0.0
        } else {
            round_to_step(v, step)
        });
    }
    (start, end, ticks)
}

fn round_to_step(v: f64, step: f64) -> f64 {
    let decimals = (-step.log10().floor()).max(0.0) as i32 + 2;
    let p = 10f64.powi(decimals);
    (v * p).round() / p
}

#[derive(Debug, Clone, Copy)]
pub struct Linear {
    pub d0: f64,
    pub d1: f64,
    pub r0: f32,
    pub r1: f32,
}

impl Linear {
    pub fn new(domain: (f64, f64), range: (f32, f32)) -> Self {
        Self {
            d0: domain.0,
            d1: domain.1,
            r0: range.0,
            r1: range.1,
        }
    }

    pub fn map(&self, v: f64) -> f32 {
        let span = self.d1 - self.d0;
        if span == 0.0 {
            return (self.r0 + self.r1) / 2.0;
        }
        let t = (v - self.d0) / span;
        self.r0 + (self.r1 - self.r0) * t as f32
    }
}

/// Evenly spaced bands for categories.
#[derive(Debug, Clone, Copy)]
pub struct Band {
    pub start: f32,
    pub step: f32,
    pub width: f32,
}

impl Band {
    pub fn new(count: usize, range: (f32, f32), padding: f32) -> Self {
        let count = count.max(1) as f32;
        let step = (range.1 - range.0) / count;
        Self {
            start: range.0,
            step,
            width: step * (1.0 - padding.clamp(0.0, 0.9)),
        }
    }

    pub fn left(&self, i: usize) -> f32 {
        self.start + self.step * i as f32 + (self.step - self.width) / 2.0
    }

    pub fn center(&self, i: usize) -> f32 {
        self.start + self.step * (i as f32 + 0.5)
    }
}

/// Ticks for a time axis over `[t0, t1]` seconds: whole hours, days,
/// months or years, labelled in ISO form at the granularity of the step so
/// no month names need translating.
pub fn time_ticks(t0: f64, t1: f64, target: usize) -> Vec<(f64, String)> {
    let span = (t1 - t0).max(1.0);
    let day = 86_400.0;
    let hour = 3600.0;
    let to_dt = |t: f64| DateTime::from_timestamp(t.floor() as i64, 0).map(|d| d.naive_utc());
    let (Some(start), Some(_)) = (to_dt(t0), to_dt(t1)) else {
        return Vec::new();
    };
    let target = target.max(2) as f64;
    let mut out = Vec::new();
    let push = |out: &mut Vec<(f64, String)>, dt: NaiveDateTime, label: String| {
        let t = dt.and_utc().timestamp() as f64;
        if t >= t0 - 1e-6 && t <= t1 + 1e-6 {
            out.push((t, label));
        }
    };
    if span <= 2.0 * day {
        let step_h = [1, 2, 3, 6, 12]
            .into_iter()
            .find(|h| span / (*h as f64 * hour) <= target)
            .unwrap_or(24);
        let mut t = (t0 / (step_h as f64 * hour)).ceil() * step_h as f64 * hour;
        while t <= t1 && out.len() < 64 {
            if let Some(dt) = to_dt(t) {
                push(&mut out, dt, dt.format("%H:%M").to_string());
            }
            t += step_h as f64 * hour;
        }
        return out;
    }
    if span <= 120.0 * day {
        let step_d = [1, 2, 7, 14]
            .into_iter()
            .find(|d| span / (*d as f64 * day) <= target)
            .unwrap_or(30);
        let mut date = start.date();
        if step_d == 7 || step_d == 14 {
            while date.weekday() != chrono::Weekday::Mon {
                date = date.succ_opt().unwrap_or(date);
            }
        } else if start.time() != chrono::NaiveTime::MIN {
            date = date.succ_opt().unwrap_or(date);
        }
        while out.len() < 64 {
            let dt = date.and_hms_opt(0, 0, 0).unwrap_or_default();
            if dt.and_utc().timestamp() as f64 > t1 {
                break;
            }
            push(&mut out, dt, dt.format("%Y-%m-%d").to_string());
            date = date
                .checked_add_days(chrono::Days::new(step_d))
                .unwrap_or(date);
        }
        return out;
    }
    let months = span / (30.44 * day);
    let step_m = [1u32, 2, 3, 6, 12, 24, 60, 120]
        .into_iter()
        .find(|m| months / f64::from(*m) <= target)
        .unwrap_or(240);
    let mut date = NaiveDate::from_ymd_opt(start.year(), start.month(), 1).unwrap_or_default();
    if step_m >= 12 {
        date = NaiveDate::from_ymd_opt(start.year(), 1, 1).unwrap_or_default();
    }
    while out.len() < 64 {
        let dt = date.and_hms_opt(0, 0, 0).unwrap_or_default();
        if dt.and_utc().timestamp() as f64 > t1 {
            break;
        }
        let label = if step_m >= 12 {
            dt.format("%Y").to_string()
        } else {
            dt.format("%Y-%m").to_string()
        };
        push(&mut out, dt, label);
        date = match date.checked_add_months(Months::new(step_m)) {
            Some(d) => d,
            None => break,
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_ticks_land_on_round_numbers() {
        let (lo, hi, ticks) = nice_ticks(3.2, 97.0, 5);
        assert_eq!((lo, hi), (0.0, 100.0));
        assert_eq!(ticks, vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]);
        let (_, _, ticks) = nice_ticks(-0.013, 0.042, 5);
        assert_eq!(ticks.first(), Some(&-0.02));
        assert!(ticks.contains(&0.0));
        let (lo, hi, _) = nice_ticks(5.0, 5.0, 5);
        assert!(lo < 5.0 && hi > 5.0);
    }

    #[test]
    fn widths_scale_and_wide_glyphs_count_double() {
        assert!((text_width("Hi", 10.0) - (7.41 + 2.58)).abs() < 0.01);
        assert_eq!(text_width("日本", 10.0), 20.0);
        let cut = fit_text("A very long category label", 10.0, 40.0);
        assert!(cut.ends_with('…') && text_width(&cut, 10.0) <= 40.0);
    }

    #[test]
    fn time_ticks_pick_calendar_steps() {
        let t = |y, m, d| {
            NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .timestamp() as f64
        };
        let ticks = time_ticks(t(2024, 1, 15), t(2026, 6, 1), 6);
        assert!(
            ticks.iter().all(|(_, l)| l.len() == 4 || l.len() == 7),
            "{ticks:?}"
        );
        assert!(ticks.len() <= 7, "{ticks:?}");
        let days = time_ticks(t(2026, 11, 1), t(2026, 11, 8), 8);
        assert_eq!(days.first().map(|(_, l)| l.as_str()), Some("2026-11-01"));
    }
}
