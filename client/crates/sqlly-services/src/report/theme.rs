//! Report colors. Surfaces, ink, the accent and the status colors are the
//! app's own (`apps/gpui/src/theme.rs`, the default Light and Dark
//! palettes, with the light canvas whitened for paper). The categorical
//! series ramp starts at the app accent and continues with the Okabe–Ito
//! colour-blind-safe hues, tuned per theme for contrast on the canvas.

use serde::{Deserialize, Serialize};

use super::doc::{AssertionStatus, CalloutKind, Status};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const fn hex(v: u32) -> Rgb {
        Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    pub fn css(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    pub fn unit(self) -> (f32, f32, f32) {
        (
            f32::from(self.0) / 255.0,
            f32::from(self.1) / 255.0,
            f32::from(self.2) / 255.0,
        )
    }

    /// `self` mixed toward `other` by `t` (0 = self, 1 = other).
    pub fn mix(self, other: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let m = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Rgb(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f32 {
        let lin = |c: u8| {
            let c = f32::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.0) + 0.7152 * lin(self.1) + 0.0722 * lin(self.2)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    #[default]
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportTheme {
    pub mode: ThemeMode,
    pub background: Rgb,
    pub surface: Rgb,
    pub text: Rgb,
    pub text_dim: Rgb,
    pub border: Rgb,
    pub grid: Rgb,
    pub accent: Rgb,
    pub good: Rgb,
    pub warning: Rgb,
    pub critical: Rgb,
    pub series: [Rgb; 8],
    /// Low and high ends of the sequential ramp (heatmaps, calendars).
    pub ramp: (Rgb, Rgb),
    /// Negative, middle and positive ends of the diverging ramp.
    pub diverging: (Rgb, Rgb, Rgb),
}

pub const LIGHT: ReportTheme = ReportTheme {
    mode: ThemeMode::Light,
    background: Rgb::hex(0xFFFFFF),
    surface: Rgb::hex(0xF4F8FF),
    text: Rgb::hex(0x0F2544),
    text_dim: Rgb::hex(0x3C5A85),
    border: Rgb::hex(0xC6D9EF),
    grid: Rgb::hex(0xE3EDF9),
    accent: Rgb::hex(0x1A5FB4),
    good: Rgb::hex(0x2C6A2F),
    warning: Rgb::hex(0x8A5A00),
    critical: Rgb::hex(0xAC1515),
    series: [
        Rgb::hex(0x1A5FB4),
        Rgb::hex(0xC9500A),
        Rgb::hex(0x00876A),
        Rgb::hex(0xB5608F),
        Rgb::hex(0xA86F00),
        Rgb::hex(0x3A8FC7),
        Rgb::hex(0x7A6A00),
        Rgb::hex(0x5F6B78),
    ],
    ramp: (Rgb::hex(0xE7F0FC), Rgb::hex(0x0F3F80)),
    diverging: (Rgb::hex(0xC9500A), Rgb::hex(0xF4F8FF), Rgb::hex(0x1A5FB4)),
};

pub const DARK: ReportTheme = ReportTheme {
    mode: ThemeMode::Dark,
    background: Rgb::hex(0x0A1A38),
    surface: Rgb::hex(0x102448),
    text: Rgb::hex(0xE4F1FF),
    text_dim: Rgb::hex(0x8AB4E0),
    border: Rgb::hex(0x2A4A7A),
    grid: Rgb::hex(0x163056),
    accent: Rgb::hex(0x8EC8FF),
    good: Rgb::hex(0x42D751),
    warning: Rgb::hex(0xF4A725),
    critical: Rgb::hex(0xEC6969),
    series: [
        Rgb::hex(0x8EC8FF),
        Rgb::hex(0xFF8A5C),
        Rgb::hex(0x3CC9A0),
        Rgb::hex(0xE38FC0),
        Rgb::hex(0xF0B040),
        Rgb::hex(0x5FD3F3),
        Rgb::hex(0xE8DC5A),
        Rgb::hex(0xA9B4C2),
    ],
    ramp: (Rgb::hex(0x13294F), Rgb::hex(0x8EC8FF)),
    diverging: (Rgb::hex(0xFF8A5C), Rgb::hex(0x13294F), Rgb::hex(0x8EC8FF)),
};

impl ReportTheme {
    pub fn for_mode(mode: ThemeMode) -> &'static ReportTheme {
        match mode {
            ThemeMode::Light => &LIGHT,
            ThemeMode::Dark => &DARK,
        }
    }

    pub fn series_color(&self, index: usize) -> Rgb {
        self.series[index % self.series.len()]
    }

    /// One hue stepped from light to dark, for `count` ordered categories.
    pub fn sequential_color(&self, index: usize, count: usize) -> Rgb {
        let t = if count <= 1 {
            1.0
        } else {
            0.25 + 0.75 * index as f32 / (count - 1) as f32
        };
        self.ramp.0.mix(self.ramp.1, t)
    }

    /// A value 0..=1 on the sequential ramp.
    pub fn ramp_color(&self, t: f32) -> Rgb {
        self.ramp.0.mix(self.ramp.1, t)
    }

    /// A value -1..=1 on the diverging ramp.
    pub fn diverging_color(&self, t: f32) -> Rgb {
        let (neg, mid, pos) = self.diverging;
        if t < 0.0 {
            mid.mix(neg, -t)
        } else {
            mid.mix(pos, t)
        }
    }

    pub fn status_color(&self, status: Status) -> Rgb {
        match status {
            Status::Good => self.good,
            Status::Warning => self.warning,
            Status::Critical => self.critical,
            Status::Neutral => self.text_dim,
        }
    }

    pub fn assertion_color(&self, status: AssertionStatus) -> Rgb {
        match status {
            AssertionStatus::Passed => self.good,
            AssertionStatus::Failed | AssertionStatus::Error => self.critical,
            AssertionStatus::Warning => self.warning,
            AssertionStatus::Skipped | AssertionStatus::Inconclusive => self.text_dim,
        }
    }

    pub fn callout_color(&self, kind: CalloutKind) -> Rgb {
        match kind {
            CalloutKind::Fact => self.accent,
            CalloutKind::Derived => self.series_color(5),
            CalloutKind::Hypothesis => self.series_color(3),
            CalloutKind::Recommendation => self.good,
            CalloutKind::Warning => self.warning,
            CalloutKind::Note => self.text_dim,
        }
    }

    /// Readable ink on a fill of `color`.
    pub fn ink_on(&self, color: Rgb) -> Rgb {
        if color.luminance() > 0.35 {
            LIGHT.text
        } else {
            Rgb::hex(0xFFFFFF)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contrast(a: Rgb, b: Rgb) -> f32 {
        let (l1, l2) = (a.luminance(), b.luminance());
        (l1.max(l2) + 0.05) / (l1.min(l2) + 0.05)
    }

    #[test]
    fn series_colors_are_distinct_and_visible_on_the_canvas() {
        for theme in [&LIGHT, &DARK] {
            for (i, a) in theme.series.iter().enumerate() {
                assert!(
                    contrast(*a, theme.background) >= 2.5,
                    "{:?} series {i} contrast",
                    theme.mode
                );
                for b in &theme.series[i + 1..] {
                    assert_ne!(a, b);
                }
            }
            assert!(contrast(theme.text, theme.background) >= 7.0);
            assert!(contrast(theme.text_dim, theme.background) >= 4.5);
        }
    }

    #[test]
    fn the_first_series_is_the_app_accent() {
        assert_eq!(LIGHT.series[0], LIGHT.accent);
        assert_eq!(DARK.series[0], DARK.accent);
        assert_eq!(LIGHT.accent.css(), "#1a5fb4");
    }
}
