use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use gpui::{Hsla, px};
use parking_lot::RwLock;
use sqlly_core::ConnectionKind;

/// Visual theme variant. `System` follows the live OS appearance (dark/light),
/// updated at startup and whenever the system appearance changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeKind {
    System,
    Light,
    Dark,
    HighContrast,
}

impl ThemeKind {
    const SYSTEM: u8 = 0;
    const LIGHT: u8 = 1;
    const DARK: u8 = 2;
    const HIGH_CONTRAST: u8 = 3;

    fn to_u8(self) -> u8 {
        match self {
            Self::System => Self::SYSTEM,
            Self::Light => Self::LIGHT,
            Self::Dark => Self::DARK,
            Self::HighContrast => Self::HIGH_CONTRAST,
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            Self::LIGHT => Self::Light,
            Self::DARK => Self::Dark,
            Self::HIGH_CONTRAST => Self::HighContrast,
            _ => Self::System,
        }
    }
}

static THEME_KIND: AtomicU8 = AtomicU8::new(ThemeKind::SYSTEM);

/// Named palette used while the resolved mode is light.
static LIGHT_THEME: AtomicU8 = AtomicU8::new(0); // ColorTheme::Light
/// Named palette used while the resolved mode is dark.
static DARK_THEME: AtomicU8 = AtomicU8::new(12); // ColorTheme::Dark

/// The last-observed OS appearance. `true` = dark. Defaults to dark so the very
/// first paint (before the appearance observer fires) is dark rather than a
/// white flash; it is corrected to the real OS value at window creation.
static SYSTEM_IS_DARK: AtomicBool = AtomicBool::new(true);

/// Set the active theme variant. Thread-safe.
pub fn set_kind(kind: ThemeKind) {
    THEME_KIND.store(kind.to_u8(), Ordering::Relaxed);
}

/// Get the active theme variant.
pub fn kind() -> ThemeKind {
    ThemeKind::from_u8(THEME_KIND.load(Ordering::Relaxed))
}

/// Record the current OS appearance (dark/light). Called once at window
/// creation and again whenever the system appearance changes. Only affects the
/// resolved colors while the active kind is `System`. Thread-safe.
pub fn set_system_dark(is_dark: bool) {
    SYSTEM_IS_DARK.store(is_dark, Ordering::Relaxed);
}

/// Whether the OS is currently in dark mode (as last observed).
pub fn system_is_dark() -> bool {
    SYSTEM_IS_DARK.load(Ordering::Relaxed)
}

/// Resolve `System` to a concrete kind by following the live OS appearance.
/// Light/HighContrast/Dark pass through unchanged.
pub(crate) fn resolved_kind() -> ThemeKind {
    match kind() {
        ThemeKind::System => {
            if system_is_dark() {
                ThemeKind::Dark
            } else {
                ThemeKind::Light
            }
        }
        other => other,
    }
}

/// Syntax-highlighting + completion-kind colors for one appearance mode.
///
/// `keyword`, `string`, and `comment` are authored per theme (they carry the
/// theme's identity and the Visual Studio string-red / comment-green
/// convention). `number`, `function`, `constant`, `type_name`, and `variable`
/// are fitted around them by [`with_editor_inks`] so every token kind the
/// editor paints reads as its own color in every theme.
#[derive(Clone, Copy)]
pub(crate) struct SyntaxColors {
    keyword: Hsla,
    string: Hsla,
    comment: Hsla,
    number: Hsla,
    /// Built-in and user function calls in the editor.
    function: Hsla,
    /// NULL / TRUE / FALSE and fixed argument words (`day` in DATEADD).
    constant: Hsla,
    /// Data type names.
    type_name: Hsla,
    /// `@variables`.
    variable: Hsla,
    kind_keyword: Hsla,
    #[allow(dead_code)]
    kind_table: Hsla,
    kind_column: Hsla,
    #[allow(dead_code)]
    kind_function: Hsla,
    #[allow(dead_code)]
    kind_procedure: Hsla,
    kind_database: Hsla,
    kind_schema: Hsla,
    kind_snippet: Hsla,
    kind_default: Hsla,
    #[allow(dead_code)]
    kind_view: Hsla,
    #[allow(dead_code)]
    kind_index: Hsla,
    #[allow(dead_code)]
    kind_trigger: Hsla,
    #[allow(dead_code)]
    kind_constraint: Hsla,
    kind_string: Hsla,
}

/// All theme-dependent colors for one variant.
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    bg: Hsla,
    panel_bg: Hsla,
    sidebar_bg: Hsla,
    /// Query tab strip only — a 4–6% step above the canvas so active tabs can
    /// open into the editor. Sidebar stays equal to `bg`.
    tab_strip_bg: Hsla,
    border: Hsla,
    /// Resting fill of a hovered, unselected row.
    ///
    /// A role of its own rather than a reuse of `border`, which is what every
    /// row hover used to paint. That worked by luck in the families where the
    /// border happens to be a mid gray, and failed completely in High Contrast,
    /// where `border` *is* `text` (both pure white) — so hovering any row in the
    /// tree, sidebar, or grouping picker painted white text on a white fill and
    /// erased the label under the pointer. A hover fill has to be a surface, and
    /// surfaces must clear 4.5:1 against the text they carry; `border` is a line
    /// color and was never asked to.
    row_hover: Hsla,
    input_bg: Hsla,
    popover_bg: Hsla,
    text: Hsla,
    text_dim: Hsla,
    text_disabled: Hsla,
    accent: Hsla,
    underline_accent: Hsla,
    /// Status-bar heart + "I love SQLLY" tagline. Always a rose, shifted to
    /// the palette's temperature so it stands out without reading as error red
    /// or stealing the accent.
    love: Hsla,
    badge_table: Hsla,
    badge_view: Hsla,
    badge_proc: Hsla,
    badge_fn: Hsla,
    selection: Hsla,
    cursor: Hsla,
    row_alt: Hsla,
    error: Hsla,
    warning: Hsla,
    info: Hsla,
    success: Hsla,
    popup_border: Hsla,
    completion_selected: Hsla,
    /// Wash painted over the page behind a modal. A palette role rather than a
    /// literal so the four scrims in the app can't drift apart again.
    scrim: Hsla,
    syntax: SyntaxColors,
}

pub(crate) const fn h(h: f32, s: f32, l: f32, a: f32) -> Hsla {
    Hsla { h, s, l, a }
}

/// Rose ink for the status-bar heart and tagline.
///
/// Always a pink–red so it reads as "love", but shifted to the palette's
/// temperature so it belongs on the canvas: cooler families get raspberry,
/// warm families (amber, sepia) get rose, purple accents pull magenta,
/// muted graphite desaturates, and high contrast goes full hot pink.
pub(crate) fn love_for(p: &Palette, dark: bool) -> Hsla {
    let bg = p.bg;
    let accent = p.accent;
    let high_contrast = accent.s > 0.95 && (bg.l < 0.02 || bg.l > 0.98);
    let warm_hue = |hue: f32| !(0.12..=0.95).contains(&hue);
    let purple_hue = |hue: f32| (0.72..0.88).contains(&hue);
    let muted = bg.s < 0.10 && accent.s < 0.40;

    if high_contrast {
        return if dark {
            h(0.90, 1.0, 0.68, 1.0)
        } else {
            h(0.90, 1.0, 0.36, 1.0)
        };
    }

    let hue = if purple_hue(accent.h) {
        0.88
    } else if warm_hue(accent.h) || (bg.s > 0.12 && warm_hue(bg.h)) {
        0.94
    } else {
        0.91
    };
    let sat = if muted {
        if dark { 0.40 } else { 0.55 }
    } else if dark {
        0.52
    } else {
        0.72
    };
    let light = if dark { 0.74 } else { 0.36 };
    h(hue, sat, light, 1.0)
}

/// The `*_light` named colors the kit uses for colored ink on dark fills —
/// the base hue with its lightness lifted, clamped at 1.0.
fn lighten(c: Hsla, amount: f32) -> Hsla {
    gpui::hsla(c.h, c.s, (c.l + amount).min(1.0), c.a)
}

pub(crate) const AMBER_DARK: Palette = Palette {
    bg: h(0.0, 0.0, 0.118, 1.0),                    // #1e1e1e (30/255)
    panel_bg: h(0.0, 0.0, 0.173, 1.0),              // #2c2c2d (44/255)
    sidebar_bg: h(0.0, 0.0, 0.118, 1.0),            // #1e1e1e — same canvas as bg
    tab_strip_bg: h(0.0, 0.0, 0.118, 1.0),          // same as canvas — tabs sit on bg
    border: h(0.0, 0.0, 0.227, 1.0),                // #3a3a3c
    row_hover: h(0.0, 0.0, 0.165, 1.0),             // #2a2a2b — lift off the unified canvas
    input_bg: h(0.0, 0.0, 0.165, 1.0),              // #2a2a2b (42/255)
    popover_bg: h(0.0, 0.0, 0.173, 1.0),            // #2c2c2d (44/255)
    text: h(0.0, 0.0, 0.831, 1.0),                  // #d4d4d4
    text_dim: h(0.0, 0.0, 0.580, 1.0),              // #8a8a8e — WCAG-tuned on panel/hover
    text_disabled: h(0.0, 0.0, 0.361, 1.0),         // #5c5c60
    accent: h(0.1047, 0.922, 0.502, 1.0),           // #f59e0b
    underline_accent: h(0.1047, 0.922, 0.502, 1.0), // #f59e0b
    love: h(0.96, 0.52, 0.74, 1.0),                 // warm rose — tagline/heart
    badge_table: h(0.350, 0.65, 0.55, 1.0),         // green
    badge_view: h(0.1047, 0.90, 0.55, 1.0),         // amber
    badge_proc: h(0.750, 0.55, 0.65, 1.0),          // purple
    badge_fn: h(0.580, 0.70, 0.60, 1.0),            // blue
    selection: h(0.1047, 0.922, 0.502, 0.12),
    cursor: h(0.0, 0.0, 1.0, 1.0),    // #ffffff
    row_alt: h(0.0, 0.0, 0.133, 1.0), // slightly lifted bg
    error: h(0.0, 0.78, 0.67, 1.0),
    warning: h(0.1047, 0.90, 0.55, 1.0),
    info: h(0.580, 0.70, 0.60, 1.0),
    success: h(0.350, 0.65, 0.55, 1.0),
    popup_border: h(0.0, 0.0, 0.227, 1.0), // #3a3a3c
    completion_selected: h(0.1047, 0.922, 0.502, 0.20),
    scrim: h(0.0, 0.0, 0.0, 0.5),
    syntax: SyntaxColors {
        keyword: h(0.1047, 0.85, 0.65, 1.0), // amber keywords
        // Strings are warm red, comments green — the pairing Visual Studio,
        // SSMS, and VS Code all use. The two used to be neighbouring greens.
        string: h(0.0139, 0.60, 0.68, 1.0), // salmon red
        // Comments are green — the convention every SQL tool shares. Muted
        // and a hue off the mint strings sit at, so the two read apart.
        comment: h(0.2819, 0.2857, 0.4667, 1.0), // #6A9955
        number: h(0.08, 0.70, 0.65, 1.0),
        // Fitted by `with_editor_inks`; these seeds are never painted.
        function: h(0.0, 0.0, 0.5, 1.0),
        constant: h(0.0, 0.0, 0.5, 1.0),
        type_name: h(0.0, 0.0, 0.5, 1.0),
        variable: h(0.0, 0.0, 0.5, 1.0),
        kind_keyword: h(0.1047, 0.85, 0.65, 1.0),
        kind_table: h(0.350, 0.65, 0.55, 1.0),
        kind_column: h(0.167, 0.50, 0.60, 1.0),
        kind_function: h(0.580, 0.70, 0.60, 1.0),
        kind_procedure: h(0.750, 0.55, 0.68, 1.0),
        kind_database: h(0.580, 0.60, 0.62, 1.0),
        kind_schema: h(0.750, 0.50, 0.70, 1.0),
        kind_snippet: h(0.12, 0.50, 0.60, 1.0),
        kind_default: h(0.0, 0.0, 0.60, 1.0),
        kind_view: h(0.1047, 0.90, 0.55, 1.0),
        kind_index: h(0.45, 0.40, 0.60, 1.0),
        kind_trigger: h(0.95, 0.50, 0.66, 1.0),
        kind_constraint: h(0.08, 0.40, 0.60, 1.0),
        kind_string: h(0.350, 0.60, 0.60, 1.0),
    },
};

pub(crate) const AMBER_LIGHT: Palette = Palette {
    bg: h(0.0, 0.0, 1.0, 1.0),              // #ffffff
    panel_bg: h(0.0, 0.0, 1.0, 1.0),        // #ffffff
    sidebar_bg: h(0.0, 0.0, 1.0, 1.0),      // #ffffff — same canvas as bg
    tab_strip_bg: h(0.0, 0.0, 1.0, 1.0),    // same as canvas
    border: h(0.0, 0.0, 0.898, 1.0),        // #e5e5e5
    row_hover: h(0.0, 0.0, 0.953, 1.0),     // #f3f3f3 — lift off the unified canvas
    input_bg: h(0.0, 0.0, 1.0, 1.0),        // #ffffff
    popover_bg: h(0.0, 0.0, 1.0, 1.0),      // #ffffff
    text: h(0.0, 0.0, 0.118, 1.0),          // #1e1e1e
    text_dim: h(0.0, 0.0, 0.420, 1.0),      // #6b6b70
    text_disabled: h(0.0, 0.0, 0.604, 1.0), // #9a9aa0
    accent: h(0.1047, 0.95, 0.32, 1.0),     // darkened amber for AA on light canvas
    underline_accent: h(0.1047, 0.95, 0.32, 1.0),
    love: h(0.94, 0.72, 0.36, 1.0), // fuchsia-rose on white
    badge_table: h(0.350, 0.65, 0.38, 1.0),
    badge_view: h(0.1047, 0.90, 0.40, 1.0),
    badge_proc: h(0.750, 0.55, 0.42, 1.0),
    badge_fn: h(0.580, 0.70, 0.42, 1.0),
    selection: h(0.1047, 0.90, 0.45, 0.12),
    cursor: h(0.0, 0.0, 0.0, 1.0),
    row_alt: h(0.0, 0.0, 0.973, 1.0), // #f8f8f8
    error: h(0.0, 0.78, 0.38, 1.0),
    warning: h(0.1047, 0.95, 0.32, 1.0),
    info: h(0.580, 0.75, 0.36, 1.0),
    success: h(0.350, 0.70, 0.31, 1.0),
    popup_border: h(0.0, 0.0, 0.898, 1.0),
    completion_selected: h(0.1047, 0.90, 0.45, 0.15),
    scrim: h(0.0, 0.0, 0.0, 0.4),
    syntax: SyntaxColors {
        keyword: h(0.1047, 0.90, 0.33, 1.0),
        string: h(0.0, 0.77, 0.36, 1.0), // #A31515 — the VS/SSMS string red
        comment: h(0.3056, 1.0, 0.245, 1.0), // deep green, AA on white
        number: h(0.08, 0.75, 0.34, 1.0),
        // Fitted by `with_editor_inks`; these seeds are never painted.
        function: h(0.0, 0.0, 0.5, 1.0),
        constant: h(0.0, 0.0, 0.5, 1.0),
        type_name: h(0.0, 0.0, 0.5, 1.0),
        variable: h(0.0, 0.0, 0.5, 1.0),
        kind_keyword: h(0.1047, 0.90, 0.33, 1.0),
        kind_table: h(0.350, 0.65, 0.32, 1.0),
        kind_column: h(0.167, 0.55, 0.30, 1.0),
        kind_function: h(0.580, 0.75, 0.36, 1.0),
        kind_procedure: h(0.750, 0.55, 0.36, 1.0),
        kind_database: h(0.580, 0.65, 0.34, 1.0),
        kind_schema: h(0.750, 0.50, 0.36, 1.0),
        kind_snippet: h(0.12, 0.55, 0.34, 1.0),
        kind_default: h(0.0, 0.0, 0.33, 1.0),
        kind_view: h(0.1047, 0.95, 0.32, 1.0),
        kind_index: h(0.45, 0.45, 0.34, 1.0),
        kind_trigger: h(0.95, 0.50, 0.34, 1.0),
        kind_constraint: h(0.08, 0.45, 0.34, 1.0),
        kind_string: h(0.350, 0.65, 0.32, 1.0),
    },
};

pub(crate) const HIGH_CONTRAST: Palette = Palette {
    bg: h(0.0, 0.0, 0.0, 1.0),
    panel_bg: h(0.0, 0.0, 0.0, 1.0),
    sidebar_bg: h(0.0, 0.0, 0.0, 1.0),
    tab_strip_bg: h(0.0, 0.0, 0.0, 1.0),
    border: h(0.0, 0.0, 1.0, 1.0),
    row_hover: h(0.0, 0.0, 0.30, 1.0),
    input_bg: h(0.0, 0.0, 0.0, 1.0),
    popover_bg: h(0.0, 0.0, 0.0, 1.0),
    text: h(0.0, 0.0, 1.0, 1.0),
    text_dim: h(0.0, 0.0, 0.85, 1.0),
    text_disabled: h(0.0, 0.0, 0.60, 1.0),
    accent: h(0.1047, 1.0, 0.50, 1.0),
    underline_accent: h(0.1047, 1.0, 0.50, 1.0),
    love: h(0.90, 1.0, 0.68, 1.0), // hot pink, not error red
    badge_table: h(0.33, 1.0, 0.50, 1.0),
    badge_view: h(0.1047, 1.0, 0.50, 1.0),
    badge_proc: h(0.75, 1.0, 0.55, 1.0),
    badge_fn: h(0.55, 1.0, 0.50, 1.0),
    selection: h(0.1047, 1.0, 0.50, 0.5),
    cursor: h(0.0, 0.0, 1.0, 1.0),
    row_alt: h(0.0, 0.0, 0.1, 1.0),
    error: h(0.0, 1.0, 0.5, 1.0),
    warning: h(0.1047, 1.0, 0.50, 1.0),
    info: h(0.55, 1.0, 0.5, 1.0),
    success: h(0.33, 1.0, 0.5, 1.0),
    popup_border: h(0.0, 0.0, 1.0, 1.0),
    completion_selected: h(0.1047, 1.0, 0.50, 0.5),
    scrim: h(0.0, 0.0, 0.0, 0.6),
    syntax: SyntaxColors {
        keyword: h(0.1047, 1.0, 0.65, 1.0),
        string: h(0.0139, 1.0, 0.68, 1.0),
        comment: h(0.2861, 1.0, 0.55, 1.0),
        number: h(0.10, 1.0, 0.60, 1.0),
        // Fitted by `with_editor_inks`; these seeds are never painted.
        function: h(0.0, 0.0, 0.5, 1.0),
        constant: h(0.0, 0.0, 0.5, 1.0),
        type_name: h(0.0, 0.0, 0.5, 1.0),
        variable: h(0.0, 0.0, 0.5, 1.0),
        kind_keyword: h(0.1047, 1.0, 0.65, 1.0),
        kind_table: h(0.33, 1.0, 0.60, 1.0),
        kind_column: h(0.0, 1.0, 0.60, 1.0),
        kind_function: h(0.55, 1.0, 0.60, 1.0),
        kind_procedure: h(0.75, 1.0, 0.63, 1.0),
        kind_database: h(0.55, 1.0, 0.62, 1.0),
        kind_schema: h(0.75, 1.0, 0.63, 1.0),
        kind_snippet: h(0.12, 1.0, 0.60, 1.0),
        kind_default: h(0.0, 0.0, 0.9, 1.0),
        kind_view: h(0.1047, 1.0, 0.60, 1.0),
        kind_index: h(0.45, 1.0, 0.60, 1.0),
        kind_trigger: h(0.95, 1.0, 0.60, 1.0),
        kind_constraint: h(0.08, 1.0, 0.60, 1.0),
        kind_string: h(0.33, 1.0, 0.60, 1.0),
    },
};

/// High Contrast Light — white canvas, black ink, black hairlines, maximum
/// saturation signals darkened to AA on white. `row_hover` is a gray surface,
/// never the (black) border.
pub(crate) const HIGH_CONTRAST_LIGHT: Palette = Palette {
    bg: h(0.0, 0.0, 1.0, 1.0),
    panel_bg: h(0.0, 0.0, 1.0, 1.0),
    sidebar_bg: h(0.0, 0.0, 1.0, 1.0),
    tab_strip_bg: h(0.0, 0.0, 1.0, 1.0),
    border: h(0.0, 0.0, 0.0, 1.0),
    row_hover: h(0.0, 0.0, 0.82, 1.0),
    input_bg: h(0.0, 0.0, 1.0, 1.0),
    popover_bg: h(0.0, 0.0, 1.0, 1.0),
    text: h(0.0, 0.0, 0.0, 1.0),
    text_dim: h(0.0, 0.0, 0.25, 1.0),
    text_disabled: h(0.0, 0.0, 0.45, 1.0),
    accent: h(0.1047, 1.0, 0.32, 1.0),
    underline_accent: h(0.1047, 1.0, 0.32, 1.0),
    love: h(0.90, 1.0, 0.36, 1.0), // deep magenta on white
    badge_table: h(0.33, 1.0, 0.22, 1.0),
    badge_view: h(0.1047, 1.0, 0.32, 1.0),
    badge_proc: h(0.75, 1.0, 0.32, 1.0),
    badge_fn: h(0.58, 1.0, 0.32, 1.0),
    selection: h(0.1047, 1.0, 0.32, 0.22),
    cursor: h(0.0, 0.0, 0.0, 1.0),
    row_alt: h(0.0, 0.0, 0.94, 1.0),
    error: h(0.0, 1.0, 0.32, 1.0),
    warning: h(0.1047, 1.0, 0.32, 1.0),
    info: h(0.58, 1.0, 0.32, 1.0),
    success: h(0.33, 1.0, 0.22, 1.0),
    popup_border: h(0.0, 0.0, 0.0, 1.0),
    completion_selected: h(0.1047, 1.0, 0.32, 0.28),
    scrim: h(0.0, 0.0, 0.0, 0.45),
    syntax: SyntaxColors {
        keyword: h(0.1047, 1.0, 0.32, 1.0),
        string: h(0.0, 1.0, 0.33, 1.0),
        comment: h(0.2861, 1.0, 0.20, 1.0),
        number: h(0.08, 1.0, 0.32, 1.0),
        // Fitted by `with_editor_inks`; these seeds are never painted.
        function: h(0.0, 0.0, 0.5, 1.0),
        constant: h(0.0, 0.0, 0.5, 1.0),
        type_name: h(0.0, 0.0, 0.5, 1.0),
        variable: h(0.0, 0.0, 0.5, 1.0),
        kind_keyword: h(0.1047, 1.0, 0.32, 1.0),
        kind_table: h(0.33, 1.0, 0.22, 1.0),
        kind_column: h(0.0, 0.0, 0.20, 1.0),
        kind_function: h(0.58, 1.0, 0.32, 1.0),
        kind_procedure: h(0.75, 1.0, 0.32, 1.0),
        kind_database: h(0.58, 1.0, 0.30, 1.0),
        kind_schema: h(0.75, 1.0, 0.32, 1.0),
        kind_snippet: h(0.12, 1.0, 0.30, 1.0),
        kind_default: h(0.0, 0.0, 0.20, 1.0),
        kind_view: h(0.1047, 1.0, 0.32, 1.0),
        kind_index: h(0.45, 1.0, 0.22, 1.0),
        kind_trigger: h(0.95, 1.0, 0.32, 1.0),
        kind_constraint: h(0.08, 1.0, 0.30, 1.0),
        kind_string: h(0.33, 1.0, 0.22, 1.0),
    },
};

/// The default `Dark` theme and the base every other dark palette builds off —
/// the SQLLY brand: deep navy canvas, lighter blue rails, ice-blue accent from
/// the logo gradient `#2AF0FF → #2077FF → #172A83`. Composed from the flat
/// `AMBER_DARK` base with brand surfaces and accent.
pub(crate) const DARK: Palette = Palette {
    bg: h(0.6087, 0.6970, 0.1294, 1.0),         // #0A1A38
    sidebar_bg: h(0.6071, 0.6364, 0.1725, 1.0), // #102448
    tab_strip_bg: h(0.6087, 0.6970, 0.1294, 1.0),
    panel_bg: h(0.6071, 0.6364, 0.1725, 1.0),
    input_bg: h(0.6071, 0.6364, 0.1725, 1.0),
    popover_bg: h(0.6071, 0.6364, 0.1725, 1.0),
    border: h(0.6000, 0.4878, 0.3216, 1.0), // #2A4A7A
    popup_border: h(0.6000, 0.4878, 0.3216, 1.0),
    row_hover: h(0.5990, 0.5926, 0.2118, 1.0), // #163056
    row_alt: h(0.6090, 0.6842, 0.1490, 1.0),   // #0C1E40
    text: h(0.5864, 1.0, 0.9471, 1.0),         // #E4F1FF
    text_dim: h(0.5853, 0.5811, 0.7098, 1.0),  // #8AB4E0
    text_disabled: h(0.5946, 0.3333, 0.4353, 1.0), // #4A6A94
    accent: h(0.5811, 1.0, 0.7784, 1.0),       // #8EC8FF
    underline_accent: h(0.5811, 1.0, 0.7784, 1.0),
    love: h(0.91, 0.62, 0.74, 1.0), // icy raspberry on navy
    selection: h(0.5811, 1.0, 0.7784, 0.14),
    completion_selected: h(0.5811, 1.0, 0.7784, 0.22),
    cursor: h(0.5365, 1.0, 0.9373, 1.0), // #DFF8FF
    info: h(0.5811, 1.0, 0.7784, 1.0),
    badge_fn: h(0.5811, 1.0, 0.7784, 1.0),
    syntax: SyntaxColors {
        keyword: h(0.5811, 1.0, 0.7784, 1.0),
        kind_keyword: h(0.5811, 1.0, 0.7784, 1.0),
        kind_function: h(0.5811, 1.0, 0.7784, 1.0),
        ..AMBER_DARK.syntax
    },
    ..AMBER_DARK
};

/// The default `Light` theme and the base every other light palette builds off
/// — the SQLLY brand on an icy blue-tinted canvas with deep navy ink and the
/// mark's azure accent (darkened for AA). Composed from the flat `AMBER_LIGHT`
/// base, with the green/amber semantic roles deepened for the off-white canvas.
pub(crate) const LIGHT: Palette = Palette {
    bg: h(0.6061, 1.0, 0.9784, 1.0), // #F4F8FF
    tab_strip_bg: h(0.6061, 1.0, 0.9784, 1.0),
    panel_bg: h(0.6061, 1.0, 0.9784, 1.0),
    input_bg: h(0.5952, 0.7778, 0.9471, 1.0), // #E7F0FC
    popover_bg: h(0.6061, 1.0, 0.9784, 1.0),
    sidebar_bg: h(0.5965, 0.7600, 0.9510, 1.0), // #E9F1FC
    border: h(0.5894, 0.5616, 0.8569, 1.0),     // #C6D9EF
    popup_border: h(0.5894, 0.5616, 0.8569, 1.0),
    row_hover: h(0.5909, 0.6471, 0.9333, 1.0), // #E3EDF9
    row_alt: h(0.5938, 0.7273, 0.9569, 1.0),   // #ECF3FC
    text: h(0.5975, 0.6386, 0.1627, 1.0),      // #0F2544
    text_dim: h(0.5982, 0.3782, 0.3784, 1.0),  // #3C5A85
    cursor: h(0.5975, 0.6386, 0.1627, 1.0),
    accent: h(0.5920, 0.7476, 0.4039, 1.0), // #1A5FB4
    underline_accent: h(0.5920, 0.7476, 0.4039, 1.0),
    love: h(0.91, 0.75, 0.38, 1.0), // raspberry on icy canvas
    selection: h(0.5920, 0.7476, 0.4039, 0.12),
    completion_selected: h(0.5920, 0.7476, 0.4039, 0.16),
    info: h(0.5920, 0.7476, 0.4039, 1.0),
    badge_fn: h(0.5920, 0.7476, 0.4039, 1.0),
    success: h(0.3414, 0.4133, 0.2941, 1.0), // #2C6A2F
    warning: h(0.1087, 1.0, 0.2706, 1.0),    // #8A5A00
    badge_table: h(0.3414, 0.4133, 0.2941, 1.0),
    badge_view: h(0.1087, 1.0, 0.2706, 1.0),
    syntax: SyntaxColors {
        keyword: h(0.5920, 0.7476, 0.4039, 1.0),
        kind_keyword: h(0.5920, 0.7476, 0.4039, 1.0),
        kind_function: h(0.5920, 0.7476, 0.4039, 1.0),
        string: h(0.0, 0.77, 0.36, 1.0), // string red, not the table green
        kind_string: h(0.3414, 0.4133, 0.2941, 1.0),
        kind_table: h(0.3414, 0.4133, 0.2941, 1.0),
        kind_view: h(0.1087, 1.0, 0.2706, 1.0),
        kind_column: h(0.1667, 1.0, 0.1333, 1.0), // #444400
        kind_index: h(0.4275, 0.3433, 0.2627, 1.0), // #2C5A46
        kind_snippet: h(0.1011, 1.0, 0.2392, 1.0), // #7A4A00
        ..AMBER_LIGHT.syntax
    },
    ..AMBER_LIGHT
};

#[path = "theme_catalog.rs"]
mod theme_catalog;
pub use theme_catalog::ColorTheme;

/// Set the named palette used in light mode. Thread-safe.
pub fn set_light_theme(theme: ColorTheme) {
    LIGHT_THEME.store(theme.sanitize_light().to_u8(), Ordering::Relaxed);
}

/// The named palette used while the resolved mode is light.
pub fn light_theme() -> ColorTheme {
    ColorTheme::from_u8(LIGHT_THEME.load(Ordering::Relaxed)).sanitize_light()
}

/// Set the named palette used in dark mode. Thread-safe.
pub fn set_dark_theme(theme: ColorTheme) {
    DARK_THEME.store(theme.sanitize_dark().to_u8(), Ordering::Relaxed);
}

/// The named palette used while the resolved mode is dark.
pub fn dark_theme() -> ColorTheme {
    ColorTheme::from_u8(DARK_THEME.load(Ordering::Relaxed)).sanitize_dark()
}

fn palette() -> &'static Palette {
    match resolved_kind() {
        ThemeKind::Light => light_theme().palette(),
        // High Contrast is a named palette, not a painted mode. A leftover
        // HighContrast kind follows the dark named theme (legacy tests / menu).
        ThemeKind::Dark | ThemeKind::System | ThemeKind::HighContrast => dark_theme().palette(),
    }
}

pub struct Theme;

impl Theme {
    /// Global UI scale factor. All fonts, widths, and heights are multiplied by
    /// this value to make everything 15% bigger.
    pub const SCALE: f32 = 1.15;

    /// Returns `v` scaled by `SCALE`.
    ///
    /// Note what this does *not* do: `SCALE` is a compile-time constant, so a
    /// size passed through here is fixed forever. It buys consistency with the
    /// rest of the chrome, not responsiveness to the user's font. Anything that
    /// has to hold text — a bar, a row, a control — must be sized from
    /// [`Theme::font_size`] or [`Theme::line_height`] instead, or it will clip
    /// the moment someone raises their interface font for legibility.
    pub fn scaled(v: f32) -> f32 {
        v * Self::SCALE
    }

    /// Height for a chrome bar holding a single line of interface text, plus
    /// `padding` above and below. Derived from the user's font, so raising the
    /// interface font size grows the bar with it rather than clipping inside it.
    pub fn bar_height(padding: f32) -> f32 {
        Self::line_height() + Self::scaled(padding) * 2.0
    }

    /// Height for a compact single-line row (tree nodes, list items).
    pub fn row_height() -> f32 {
        Self::line_height() + Self::scaled(3.0)
    }

    /// Shared height for Explorer header, query tab strip, command bar, and AI drawer header.
    pub fn chrome_bar_height() -> f32 {
        Self::bar_height(6.0)
    }

    /// Shared left/right inset for every chrome bar: the query tab strip, the
    /// command bar, and the sidebar / AI panel header rows.
    ///
    /// The active tab, the server picker and the panel tabs all line up on
    /// one column.
    pub fn chrome_inline_px() -> f32 {
        Self::scaled(4.0)
    }

    /// Tab strip chrome height. Matches [`chrome_bar_height`].
    #[allow(dead_code)]
    pub fn tab_strip_height() -> f32 {
        Self::chrome_bar_height()
    }

    /// Status bar height (~26px at default scale). Phase 1+ call sites.
    #[allow(dead_code)]
    pub fn status_bar_height() -> f32 {
        Self::row_height()
    }

    /// Height of a standard control (button, input, select).
    pub fn control_height() -> f32 {
        (Self::line_height() + Self::scaled(6.0)).max(Self::scaled(26.0))
    }

    /// Glyph size for a toolbar icon button (~16px chrome).
    pub fn toolbar_icon_size() -> f32 {
        Self::scaled(14.0)
    }

    /// Glyph size for an icon beside a label inside a wider button (~14px).
    pub fn inline_icon_size() -> f32 {
        Self::scaled(12.0)
    }

    /// The interface font as a `gpui::Font` — the family the user actually
    /// chose, ready to hand to the text system.
    pub fn ui_font() -> gpui::Font {
        gpui::font(Self::font_family())
    }

    /// The advance width, in pixels, of one interface-font character at `size`.
    ///
    /// **Measured, not assumed.** This used to be a hardcoded `0.62` (later
    /// `0.6`) — Fira Code's advance ratio — multiplied by the font size. That is
    /// only correct for the default font, and `Theme::font_family()` is a user
    /// preference; DESIGN.md's rule is that nothing may hardcode a family. Pick
    /// any other face and every metadata column was mis-sized, which is exactly
    /// the failure the type cells already had for a different reason.
    ///
    /// Measured from `0`, the digit the metadata columns are sized around, and
    /// exact for any monospace face. A proportional face has no single advance,
    /// so `chars × advance` remains an estimate there — but one taken from the
    /// real font rather than from a guess about which font it is.
    ///
    /// Shaping is not free, so callers measure once per render and pass the
    /// result to [`Theme::cell_width`], rather than measuring per row.
    pub fn char_advance(window: &gpui::Window, size: f32) -> f32 {
        let shaped = window.text_system().shape_line(
            gpui::SharedString::from("0"),
            px(size),
            &[gpui::TextRun {
                len: 1,
                font: Self::ui_font(),
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        f32::from(shaped.width)
    }

    /// Width of a cell holding `chars` characters whose advance is `advance`
    /// (from [`Theme::char_advance`]), plus the standard cell padding.
    ///
    /// The one place this arithmetic lives. It was previously inlined three
    /// times in the schema tree with three different rulers — one using the
    /// user's font size, two frozen at a literal `11.0` that only matched the
    /// default — so raising the interface font made data-type cells overrun the
    /// column beside them.
    pub fn cell_width(chars: usize, advance: f32) -> f32 {
        chars as f32 * advance + Self::scaled(6.0)
    }

    /// A text size expressed as a fraction of the user's interface font, for
    /// the places that want to read smaller than body text (badges, shortcut
    /// hints). Keeps the relationship instead of freezing a pixel value.
    pub fn font_size_ratio(ratio: f32) -> f32 {
        Self::font_size() * ratio
    }

    /// A group-header background carrying a client/project/environment's
    /// identity color.
    ///
    /// Entity colors are typed in by the user, so they are the one class of
    /// color in this app that cannot be pre-verified against the palette. This
    /// keeps the Tested Palette Rule anyway: the entity color is composited over
    /// the sidebar at the strongest alpha whose result still clears 4.5:1
    /// against body text, stepping down until it does and falling back to the
    /// untinted sidebar if none qualifies. So the identity is as loud as it can
    /// be while the label stays readable — for *every* color, not just the ones
    /// someone happened to try. `theme.rs::wcag` sweeps the hue/lightness space
    /// and asserts exactly that.
    ///
    /// A tint (never a side stripe, which is banned) is also what lets this stay
    /// inside the Calibrated Signal Rule: the color means "this is production",
    /// which is signal, not decoration.
    pub fn entity_tint(entity: Hsla) -> Hsla {
        Self::entity_tint_over(entity, Self::sidebar_bg())
    }

    /// [`Theme::entity_tint`] over an arbitrary surface — the hover fill for a
    /// tinted row, so hovering a group header lifts it without washing the
    /// identity out. Clamped exactly the same way.
    pub fn entity_tint_over(entity: Hsla, base: Hsla) -> Hsla {
        entity_tint_on(entity, base, Self::text())
    }

    /// The ink to paint on a user-chosen fill used at full strength — an
    /// entity's identity color as a status-bar chip background, where the color
    /// *is* the point and tinting it away ([`Theme::entity_tint`]) would defeat
    /// it.
    ///
    /// Black or white, whichever contrasts better. That choice always clears
    /// AA: the two ratios cross at 4.58:1, so the better of them can never dip
    /// under 4.5 whatever color the user typed in — `ink_on_any_fill_clears_aa`
    /// sweeps the space and asserts it.
    pub fn ink_on(fill: Hsla) -> Hsla {
        let black = gpui::hsla(0.0, 0.0, 0.0, 1.0);
        let white = gpui::hsla(0.0, 0.0, 1.0, 1.0);
        if contrast_ratio(black, fill) >= contrast_ratio(white, fill) {
            black
        } else {
            white
        }
    }

    /// The color a foreground takes on a row, given whether that row is
    /// selected. `resting` is what it would otherwise be — muted ink, a kind
    /// glyph, a status color.
    ///
    /// On a selected row this is always [`Theme::text`], because the accent
    /// selection fill carries no other foreground at AA — see [`themed_row`].
    /// Routing every foreground in a row through here is what keeps that true:
    /// the alternative is remembering it at each of a dozen call sites, which is
    /// how the pairing went unasserted in the first place.
    pub fn selection_fg(selected: bool, resting: Hsla) -> Hsla {
        if selected { Self::text() } else { resting }
    }

    /// Accent tint for a selected row at rest — matches [`Palette::selection`] alpha.
    pub fn selection_tint() -> f32 {
        palette().selection.a
    }

    /// Accent tint for a selected row while the pointer is on it — a small lift over
    /// [`Self::selection_tint`], not a separate heavy wash.
    pub const SELECTION_HOVER_TINT: f32 = 0.18;

    /// Fill for a selected row background (accent at [`Self::selection_tint`]).
    pub fn selection_fill() -> Hsla {
        Self::accent().opacity(Self::selection_tint())
    }

    /// Fill for a selected row while hovered.
    pub fn selection_hover_fill() -> Hsla {
        Self::accent().opacity(Self::SELECTION_HOVER_TINT)
    }

    pub fn bg() -> Hsla {
        palette().bg
    }
    pub fn panel_bg() -> Hsla {
        palette().panel_bg
    }
    pub fn sidebar_bg() -> Hsla {
        palette().sidebar_bg
    }
    pub fn tab_strip_bg() -> Hsla {
        palette().tab_strip_bg
    }
    pub fn border() -> Hsla {
        palette().border
    }
    /// Brighter (higher-contrast) hairline used on pane-divider hover. Mixes
    /// ink onto the resting border so dark chrome lightens and light chrome
    /// darkens — never an accent fill.
    pub fn border_hover() -> Hsla {
        mix(palette().text, palette().border, 0.45)
    }
    /// Fill for a hovered, unselected row. See [`Palette::row_hover`] — this is
    /// deliberately not `border()`.
    pub fn row_hover() -> Hsla {
        palette().row_hover
    }
    pub fn text() -> Hsla {
        palette().text
    }
    pub fn text_dim() -> Hsla {
        palette().text_dim
    }
    /// Highlighter-pen fill behind the characters a completion filter
    /// matched. Deliberately the same fluorescent yellow in every theme so
    /// matches stand out whatever the popup's palette.
    pub fn match_highlight() -> Hsla {
        gpui::rgb(0xF5FF1F).into()
    }
    /// Ink drawn on [`Self::match_highlight`]: near-black for contrast.
    pub fn match_highlight_text() -> Hsla {
        gpui::rgb(0x111111).into()
    }
    /// Muted ink for disabled controls.
    pub fn text_disabled() -> Hsla {
        palette().text_disabled
    }
    pub fn accent() -> Hsla {
        palette().accent
    }
    pub fn underline_accent() -> Hsla {
        palette().underline_accent
    }
    /// Rose ink for the status-bar heart and "I love SQLLY" tagline.
    /// A pink–red shifted to the active palette's temperature — not the
    /// accent, and not error red.
    pub fn love() -> Hsla {
        palette().love
    }
    pub fn input_bg() -> Hsla {
        palette().input_bg
    }
    #[allow(dead_code)]
    pub fn popover_bg() -> Hsla {
        palette().popover_bg
    }
    #[allow(dead_code)]
    pub fn badge_table() -> Hsla {
        palette().badge_table
    }
    #[allow(dead_code)]
    pub fn badge_view() -> Hsla {
        palette().badge_view
    }
    #[allow(dead_code)]
    pub fn badge_proc() -> Hsla {
        palette().badge_proc
    }
    #[allow(dead_code)]
    pub fn badge_fn() -> Hsla {
        palette().badge_fn
    }
    /// Ink for a database engine's glyph (used by the tab switcher). Keyed on
    /// the wire-protocol family so all ~30 `DatabaseType`s collapse to the
    /// handful of hues below; saturation/lightness shift with the mode so each
    /// stays legible on both the light and dark popup surface.
    pub fn engine_color(kind: ConnectionKind) -> Hsla {
        let dark = matches!(
            resolved_kind(),
            ThemeKind::Dark | ThemeKind::System | ThemeKind::HighContrast
        );
        // Base hue per family (0.0–1.0 around the wheel), chosen to evoke each
        // engine's brand while staying distinguishable side by side.
        let hue = match kind {
            ConnectionKind::Tds => 0.99,            // SQL Server red
            ConnectionKind::PostgresWire => 0.58,   // Postgres blue
            ConnectionKind::MySqlWire => 0.09,      // MySQL amber
            ConnectionKind::SqliteFile => 0.66,     // SQLite indigo
            ConnectionKind::HranaHttp => 0.47,      // libSQL / Turso teal
            ConnectionKind::DuckDbFile => 0.13,     // DuckDB gold
            ConnectionKind::ClickHouseHttp => 0.16, // ClickHouse yellow
            ConnectionKind::Resp => 0.97,           // Redis crimson
            ConnectionKind::OracleNet => 0.03,      // Oracle red-orange
            ConnectionKind::Drda => 0.62,           // Db2 deep blue
            ConnectionKind::Odbc => 0.83,           // Access magenta
            ConnectionKind::HttpApi => 0.52,        // cloud cyan
            ConnectionKind::Proprietary => 0.45,    // teal-green
        };
        let sat = if dark { 0.62 } else { 0.66 };
        let light = if dark { 0.66 } else { 0.44 };
        h(hue, sat, light, 1.0)
    }
    pub fn keyword() -> Hsla {
        palette().syntax.keyword
    }
    pub fn string() -> Hsla {
        palette().syntax.string
    }
    /// Ink for SQL function names (COUNT, GETDATE, …) — its own color, never
    /// the keyword's.
    pub fn function() -> Hsla {
        palette().syntax.function
    }
    /// Ink for NULL / TRUE / FALSE and fixed argument words.
    pub fn constant() -> Hsla {
        palette().syntax.constant
    }
    /// Ink for data type names.
    pub fn type_name() -> Hsla {
        palette().syntax.type_name
    }
    /// Ink for `@variables`.
    pub fn variable() -> Hsla {
        palette().syntax.variable
    }
    /// Map a classified SQL token onto the syntax palette. Shared by the
    /// editor, DDL viewer, and any other surface that paints SQL.
    pub fn syntax_color(kind: crate::sql_syntax::SyntaxTokenKind) -> Hsla {
        use crate::sql_syntax::SyntaxTokenKind;
        match kind {
            SyntaxTokenKind::Keyword => Self::keyword(),
            SyntaxTokenKind::Type => Self::type_name(),
            SyntaxTokenKind::Function => Self::function(),
            SyntaxTokenKind::Constant => Self::constant(),
            SyntaxTokenKind::Variable => Self::variable(),
            SyntaxTokenKind::Number => Self::number(),
            SyntaxTokenKind::String => Self::string(),
            SyntaxTokenKind::Comment => Self::comment(),
        }
    }
    pub fn comment() -> Hsla {
        palette().syntax.comment
    }
    pub fn number() -> Hsla {
        palette().syntax.number
    }
    pub fn selection() -> Hsla {
        palette().selection
    }
    pub fn cursor() -> Hsla {
        palette().cursor
    }
    pub fn row_alt() -> Hsla {
        palette().row_alt
    }
    pub fn error() -> Hsla {
        palette().error
    }
    pub fn warning() -> Hsla {
        palette().warning
    }
    pub fn info() -> Hsla {
        palette().info
    }
    pub fn success() -> Hsla {
        palette().success
    }
    pub fn popup_bg() -> Hsla {
        palette().popover_bg
    }
    pub fn popup_border() -> Hsla {
        palette().popup_border
    }
    pub fn completion_selected() -> Hsla {
        palette().completion_selected
    }
    /// The wash painted over the page behind a modal. Always use this rather
    /// than a black literal, so every modal in the app dims by the same amount.
    pub fn scrim() -> Hsla {
        palette().scrim
    }
    pub fn kind_keyword() -> Hsla {
        palette().syntax.kind_keyword
    }
    pub fn kind_table() -> Hsla {
        palette().badge_table
    }
    pub fn kind_column() -> Hsla {
        palette().syntax.kind_column
    }
    pub fn kind_function() -> Hsla {
        palette().badge_fn
    }
    pub fn kind_procedure() -> Hsla {
        palette().badge_proc
    }
    pub fn kind_database() -> Hsla {
        palette().syntax.kind_database
    }
    pub fn kind_schema() -> Hsla {
        palette().syntax.kind_schema
    }
    pub fn kind_snippet() -> Hsla {
        palette().syntax.kind_snippet
    }
    pub fn kind_default() -> Hsla {
        palette().syntax.kind_default
    }
    pub fn kind_view() -> Hsla {
        palette().badge_view
    }
    #[allow(dead_code)]
    pub fn kind_index() -> Hsla {
        palette().syntax.kind_index
    }
    #[allow(dead_code)]
    pub fn kind_trigger() -> Hsla {
        palette().syntax.kind_trigger
    }
    #[allow(dead_code)]
    pub fn kind_constraint() -> Hsla {
        palette().syntax.kind_constraint
    }
    pub fn kind_string() -> Hsla {
        palette().syntax.kind_string
    }

    pub fn font_size() -> f32 {
        Self::scaled(font_registry().interface_font_size)
    }
    pub fn line_height() -> f32 {
        Self::scaled(font_registry().interface_font_size * 1.5)
    }
    pub fn font_family() -> String {
        resolved_interface_font_family()
    }
    pub fn editor_font_family() -> String {
        // Browser: only the embedded monospace face exists (see
        // `resolved_interface_font_family`).
        #[cfg(target_arch = "wasm32")]
        {
            return crate::DEFAULT_EDITOR_FONT.to_string();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            font_registry().editor_font.clone()
        }
    }
    pub fn editor_font_size() -> f32 {
        Self::scaled(font_registry().editor_font_size)
    }

    pub fn set_font_prefs(
        interface_font: &str,
        interface_font_size: f32,
        editor_font: &str,
        editor_font_size: f32,
    ) {
        let mut reg = font_registry_inner().write();
        reg.interface_font = interface_font.to_string();
        reg.interface_font_size = interface_font_size;
        reg.editor_font = editor_font.to_string();
        reg.editor_font_size = editor_font_size;
    }

    /// Result text size set in Preferences → Display, or `None` to follow the
    /// grid density preset. Unscaled: the grid sizes its own canvas text.
    pub fn results_font_size() -> Option<f32> {
        font_registry().results_font_size
    }

    /// Store the results font preference and hand the family to the
    /// datatable crate, which paints every grid, pivot, and chart with it.
    /// Live grids still need their metrics re-applied (see
    /// `ResultsView::apply_density_to_live_grids`).
    pub fn set_results_font_prefs(results_font: &str, results_font_size: Option<f32>) {
        font_registry_inner().write().results_font_size = results_font_size;
        // The browser only has the faces the wasm shell embeds, so it keeps
        // the grid's own default there (see `editor_font_family`).
        let family = (!cfg!(target_arch = "wasm32")
            && !results_font.eq_ignore_ascii_case(crate::preferences::RESULTS_FONT_DEFAULT))
        .then(|| gpui::SharedString::from(results_font.to_string()));
        sqlly_datatable::set_grid_font_family(family);
    }
}

/// Resolve the persisted interface font preference to a concrete family name.
fn resolved_interface_font_family() -> String {
    let stored = font_registry().interface_font.clone();
    // The browser has no system fonts at all: every family must be one of the
    // faces the wasm shell embeds and registers (see apps/browser). Resolve
    // both "System" and any other stored preference to the embedded UI face.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = stored;
        return "Noto Sans SC".to_string();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if stored.eq_ignore_ascii_case("System") {
            #[cfg(target_os = "macos")]
            {
                return ".SystemUIFont".to_string();
            }
            #[cfg(target_os = "windows")]
            {
                return "Segoe UI".to_string();
            }
            #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
            {
                return "Inter".to_string();
            }
        }
        stored
    }
}

/// Parse a `#RRGGBB` hex color into an `Hsla`, tolerant of a missing leading
/// `#`. `None` on malformed input, so callers decide their own fallback rather
/// than inheriting a silent black.
///
/// The one hex→paint-color parser. Two hand-rolled copies of it existed — the
/// status bar badge and the entity picker — disagreeing about what a bad digit
/// meant (one fell back to black, one to the border color); both now call this.
///
/// Not to be confused with `entity_editing::parse_hex_color`, which is a
/// different job: it validates what the user is typing into the color field,
/// accepts 8-digit `#RRGGBBAA`, and returns raw bytes rather than a paint color.
/// That one stays where it is.
pub fn parse_hex_color(input: &str) -> Option<Hsla> {
    let hex = input.trim().trim_start_matches('#');
    if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |range: std::ops::Range<usize>| -> f32 {
        u8::from_str_radix(&hex[range], 16).unwrap_or(0) as f32 / 255.0
    };
    Some(
        gpui::Rgba {
            r: channel(0..2),
            g: channel(2..4),
            b: channel(4..6),
            a: 1.0,
        }
        .into(),
    )
}

/// The clamp behind [`Theme::entity_tint`], taking its surfaces explicitly so
/// it can be swept against every palette without touching the process-global
/// theme state.
pub fn entity_tint_on(entity: Hsla, base: Hsla, text: Hsla) -> Hsla {
    // 0.30 → 0.06 in six steps: as much identity as the label can carry.
    for step in 0..=6 {
        let alpha = 0.30 - step as f32 * 0.04;
        let candidate = mix(entity, base, alpha);
        if contrast_ratio(text, candidate) >= 4.5 {
            return candidate;
        }
    }
    // Every tint would bury the label — show none. `text` on `base` is already
    // asserted at AA by `every_palette_meets_wcag_contrast`.
    base
}

/// Composite `fg` over `bg` at `alpha`, in sRGB.
fn mix(fg: Hsla, bg: Hsla, alpha: f32) -> Hsla {
    let f: gpui::Rgba = fg.into();
    let b: gpui::Rgba = bg.into();
    let lerp = |x: f32, y: f32| x * alpha + y * (1.0 - alpha);
    gpui::Rgba {
        r: lerp(f.r, b.r),
        g: lerp(f.g, b.g),
        b: lerp(f.b, b.b),
        a: 1.0,
    }
    .into()
}

/// sRGB relative luminance, per WCAG 2.x. Alpha is ignored — every color this
/// is asked about has already been composited to opacity.
pub fn relative_luminance(c: Hsla) -> f32 {
    let rgba: gpui::Rgba = c.into();
    let lin = |u: f32| -> f32 {
        if u <= 0.04045 {
            u / 12.92
        } else {
            ((u + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(rgba.r) + 0.7152 * lin(rgba.g) + 0.0722 * lin(rgba.b)
}

/// WCAG contrast ratio between two opaque colors, from 1.0 to 21.0.
pub fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// OKLab coordinates of an opaque color — a perceptual space where straight
/// distance tracks how different two colors look.
pub(crate) fn oklab(c: Hsla) -> [f32; 3] {
    let rgba: gpui::Rgba = c.into();
    let lin = |u: f32| -> f32 {
        if u <= 0.04045 {
            u / 12.92
        } else {
            ((u + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(rgba.r), lin(rgba.g), lin(rgba.b));
    let l = (0.412_221_46 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// Perceptual distance between two opaque colors (OKLab ΔE). About 0.02 is
/// a just-noticeable difference; syntax inks aim for [`INK_SEPARATION`].
pub(crate) fn color_distance(a: Hsla, b: Hsla) -> f32 {
    let (x, y) = (oklab(a), oklab(b));
    ((x[0] - y[0]).powi(2) + (x[1] - y[1]).powi(2) + (x[2] - y[2]).powi(2)).sqrt()
}

/// How far apart (OKLab ΔE) every pair of editor token inks should sit.
pub(crate) const INK_SEPARATION: f32 = 0.10;
/// The floor the fitted inks are tested against — a pair can land between
/// this and [`INK_SEPARATION`] only when a theme's authored colors leave no
/// room for better.
#[cfg(test)]
pub(crate) const MIN_INK_SEPARATION: f32 = 0.08;

/// A syntax ink of `hue` for a canvas: vivid but readable, stepping the
/// lightness away from the canvas until it clears `min_contrast` on every
/// surface in `surfaces`.
fn fitted_ink(
    hue: f32,
    dark: bool,
    high_contrast: bool,
    surfaces: &[Hsla],
    min_contrast: f32,
) -> Hsla {
    let sat = if high_contrast {
        1.0
    } else if dark {
        0.70
    } else {
        0.78
    };
    let mut light: f32 = if dark { 0.70 } else { 0.38 };
    let readable = |c: Hsla| {
        surfaces
            .iter()
            .all(|s| contrast_ratio(c, *s) >= min_contrast)
    };
    for _ in 0..40 {
        let c = h(hue.rem_euclid(1.0), sat, light, 1.0);
        if readable(c) {
            return c;
        }
        light = if dark {
            (light + 0.02).min(0.95)
        } else {
            (light - 0.02).max(0.10)
        };
    }
    h(hue.rem_euclid(1.0), sat, light, 1.0)
}

/// Fit the editor's derived syntax inks — number, function, constant, type,
/// variable — around a palette's authored keyword, string, comment, and text
/// colors, so every token kind reads as its own color. Each role tries its
/// conventional hues first (functions gold, constants violet, types teal,
/// variables sky, numbers the theme's authored number ink) and falls back to
/// the hue farthest from everything already on the page.
pub(crate) fn with_editor_inks(mut p: Palette) -> Palette {
    let dark = relative_luminance(p.bg) < 0.2;
    let high_contrast = contrast_ratio(p.text, p.bg) >= 15.0 && p.accent.s > 0.95;
    let surfaces = [p.bg, p.panel_bg];
    let min_contrast = if high_contrast { 7.0 } else { 4.5 };
    let readable = |c: Hsla| {
        surfaces
            .iter()
            .all(|s| contrast_ratio(c, *s) >= min_contrast)
    };

    let mut taken = vec![p.syntax.keyword, p.syntax.string, p.syntax.comment, p.text];
    let nearest = |c: Hsla, taken: &[Hsla]| {
        taken
            .iter()
            .map(|t| color_distance(c, *t))
            .fold(f32::MAX, f32::min)
    };
    let pick = |authored: Option<Hsla>, preferred: &[f32], taken: &mut Vec<Hsla>| -> Hsla {
        if let Some(c) = authored
            && readable(c)
            && nearest(c, taken) >= INK_SEPARATION
        {
            taken.push(c);
            return c;
        }
        let ink = |hue: f32| fitted_ink(hue, dark, high_contrast, &surfaces, min_contrast);
        let chosen = preferred
            .iter()
            .map(|&hue| ink(hue))
            .find(|c| nearest(*c, taken) >= INK_SEPARATION)
            .unwrap_or_else(|| {
                (0..72)
                    .map(|i| ink(i as f32 / 72.0))
                    .max_by(|a, b| nearest(*a, taken).total_cmp(&nearest(*b, taken)))
                    .unwrap_or_else(|| ink(preferred[0]))
            });
        taken.push(chosen);
        chosen
    };
    p.syntax.number = pick(Some(p.syntax.number), &[0.08, 0.36, 0.12, 0.30], &mut taken);
    p.syntax.function = pick(None, &[0.14, 0.12, 0.47, 0.90, 0.58], &mut taken);
    p.syntax.constant = pick(None, &[0.76, 0.70, 0.83, 0.55, 0.95], &mut taken);
    p.syntax.type_name = pick(None, &[0.47, 0.50, 0.42, 0.62, 0.30], &mut taken);
    p.syntax.variable = pick(None, &[0.55, 0.60, 0.95, 0.30, 0.66], &mut taken);
    p
}

/// Paint a selectable row's resting and hover background — source-list rows,
/// segmented-control buttons, anything where "selected" and "hovered" can be
/// true at once.
///
/// **Hover must not erase selection.** An unconditional `hover(bg(border()))`
/// repaints the *selected* row gray exactly while the pointer is on it, so the
/// row the user is about to click is the one row that reads as unselected. The
/// fix is that hover on a selected row deepens the accent rather than replacing
/// it.
///
/// This lives here because the bug was found and fixed once inline in the
/// sidebar's grouping picker, and then shipped again in the schema tree 2,100
/// lines away — there was no shared helper for the knowledge to travel through.
/// Now there is: route every selectable row through this and the bug cannot
/// come back a third time.
///
/// `hover()` panics in debug if applied twice to one element, so this must be
/// the only `hover` on the row's background.
///
/// **A selected row must paint its content in [`Theme::text`] — all of it.**
/// The selection fill is the accent at [`Theme::selection_tint`] (~12% alpha,
/// matching [`Palette::selection`]); hovered-selected lifts to
/// [`Theme::SELECTION_HOVER_TINT`] (~18%). That surface still carries full ink
/// at AA — muted ink and kind glyphs do not. [`Theme::selection_fg`] is that
/// decision in one place, and `theme.rs::wcag` asserts the pairings.
pub fn themed_row<E>(el: E, selected: bool) -> E
where
    E: gpui::Styled + gpui::InteractiveElement,
{
    use gpui::Styled as _;
    if selected {
        el.bg(Theme::selection_fill())
            .hover(|s| s.bg(Theme::selection_hover_fill()))
    } else {
        el.hover(|s| s.bg(Theme::row_hover()))
    }
}

/// Explorer/sidebar rows: selected is a quiet lift (`row_hover`), not an amber
/// block. Hover on an unselected row uses the same fill.
pub fn themed_sidebar_row<E>(el: E, selected: bool) -> E
where
    E: gpui::Styled + gpui::InteractiveElement,
{
    use gpui::Styled as _;
    if selected {
        el.bg(Theme::row_hover())
            .hover(|s| s.bg(Theme::row_hover()))
    } else {
        el.hover(|s| s.bg(Theme::row_hover()))
    }
}

/// The house accent-pill tint: accent background at 15% over accent ink. This
/// is both the fill of the small pill buttons ([`accent_pill_button`]) and the
/// "on" state of the toolbar icon toggles, so the two families stay in step.
/// Apply it in exactly one place per element; conditional sites wrap it in a
/// `.when(on, ...)`.
pub fn accent_pill_style<E>(el: E) -> E
where
    E: gpui::Styled,
{
    el.bg(Theme::accent().opacity(0.15))
        .text_color(Theme::accent())
}

/// The house accent-pill button: a small labelled pill in the accent tint
/// (same treatment as the DDL header buttons). Geometry, radius, text size,
/// and font are fixed here so the sites cannot drift; callers add `on_click`
/// (and a tooltip if they want one).
pub fn accent_pill_button(
    id: &'static str,
    label: impl Into<gpui::SharedString>,
) -> gpui::Stateful<gpui::Div> {
    use gpui::{InteractiveElement as _, ParentElement as _, Styled as _, div, px};
    let s = Theme::scaled;
    accent_pill_style(
        div()
            .id(id)
            .cursor_pointer()
            .px(px(s(10.0)))
            .py(px(s(3.0)))
            .rounded(px(s(4.0)))
            .text_size(px(Theme::font_size_ratio(0.92)))
            .font(Theme::ui_font()),
    )
    .child(label.into())
}

/// Format an `Hsla` paint color as a `#rrggbb` hex string. The inverse of
/// [`parse_hex_color`], used by the color-picker surfaces that keep a
/// hex-string field canonical.
pub fn hsla_to_hex_string(color: Hsla) -> String {
    let rgba: gpui::Rgba = color.into();
    let to_byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        to_byte(rgba.r),
        to_byte(rgba.g),
        to_byte(rgba.b)
    )
}

/// Fingerprint of every process-global input that feeds
/// [`sync_component_theme`]: (kind, system-dark, interface font, quantized font
/// size). When it changes, the gpui-component global theme must be re-synced.
type ComponentThemeFingerprint = (u8, u8, u8, bool, String, u32);

fn component_theme_fingerprint() -> ComponentThemeFingerprint {
    (
        THEME_KIND.load(Ordering::Relaxed),
        LIGHT_THEME.load(Ordering::Relaxed),
        DARK_THEME.load(Ordering::Relaxed),
        system_is_dark(),
        Theme::font_family(),
        (Theme::font_size() * 16.0) as u32,
    )
}

/// Push the app palette into the gpui-component global [`gpui_component::Theme`]
/// so every stock widget (inputs, checkboxes, selects, menus, sliders, tabs,
/// color pickers, notifications) paints with SQLLY's colors instead of the
/// library defaults. Idempotent and cheap: re-syncs only when the theme
/// fingerprint (kind/family/system appearance/fonts) actually changed, so it is
/// safe to call at the top of every frame as well as at startup.
///
/// This replaces the old per-widget `themed_input` / `themed_checkbox` /
/// `themed_select` / `themed_*_theme` shims from the gpui-ui-kit era: theming
/// now happens once, globally, exactly like the library intends.
pub fn sync_component_theme(cx: &mut gpui::App) {
    use std::sync::Mutex;
    static LAST: Mutex<Option<ComponentThemeFingerprint>> = Mutex::new(None);
    let fingerprint = component_theme_fingerprint();
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.as_ref() == Some(&fingerprint) {
            return;
        }
        *last = Some(fingerprint);
    }
    force_sync_component_theme(cx);
}

/// Unconditionally rebuild the gpui-component global theme from the app
/// palette. Prefer [`sync_component_theme`] which fingerprint-gates this.
pub fn force_sync_component_theme(cx: &mut gpui::App) {
    let is_dark = match resolved_kind() {
        ThemeKind::Light => false,
        ThemeKind::Dark | ThemeKind::System | ThemeKind::HighContrast => true,
    };

    let theme = gpui_component::Theme::global_mut(cx);
    theme.mode = if is_dark {
        gpui_component::ThemeMode::Dark
    } else {
        gpui_component::ThemeMode::Light
    };

    theme.font_family = gpui::SharedString::from(Theme::font_family());
    theme.font_size = px(Theme::font_size());
    theme.radius = px(Theme::scaled(4.0));
    theme.radius_lg = px(Theme::scaled(6.0));
    theme.shadow = false;

    let colors = &mut theme.colors;
    colors.background = Theme::bg();
    colors.foreground = Theme::text();
    colors.border = Theme::border();
    // gpui-component uses `colors.input` as the form-control **border** (Select,
    // Input, Checkbox, Radio), not the fill. Map it to the same hairline as
    // outline buttons so dropdowns read as controls. Input **fill** is
    // `colors.background` / `Theme::bg()` inside the widget.
    colors.input = Theme::border();
    colors.ring = Theme::accent();
    colors.caret = Theme::cursor();
    colors.selection = Theme::selection();

    let primary_ink = Theme::ink_on(Theme::accent());
    colors.primary = Theme::accent();
    colors.primary_foreground = primary_ink;
    colors.primary_hover = Theme::accent().opacity(0.9);
    colors.primary_active = Theme::accent().opacity(0.8);

    colors.secondary = Theme::panel_bg();
    colors.secondary_foreground = Theme::text();
    colors.secondary_hover = Theme::row_hover();
    colors.secondary_active = Theme::row_alt();

    colors.danger = Theme::error();
    colors.danger_foreground = Theme::ink_on(Theme::error());
    colors.danger_hover = Theme::error().opacity(0.9);
    colors.danger_active = Theme::error().opacity(0.8);

    colors.warning = Theme::warning();
    colors.warning_foreground = Theme::ink_on(Theme::warning());
    colors.warning_hover = Theme::warning().opacity(0.9);
    colors.warning_active = Theme::warning().opacity(0.8);

    colors.info = Theme::info();
    colors.info_foreground = Theme::ink_on(Theme::info());
    colors.info_hover = Theme::info().opacity(0.9);
    colors.info_active = Theme::info().opacity(0.8);

    colors.success = Theme::success();
    colors.success_foreground = Theme::ink_on(Theme::success());
    colors.success_hover = Theme::success().opacity(0.9);
    colors.success_active = Theme::success().opacity(0.8);

    colors.muted = Theme::panel_bg();
    colors.muted_foreground = Theme::text_dim();

    // gpui-component "accent" is the subtle hover surface (shadcn accent), not
    // the brand color — map it to the row-hover wash so menu/list hover reads
    // like the rest of the app.
    colors.accent = Theme::row_hover();
    colors.accent_foreground = Theme::text();

    colors.popover = Theme::popup_bg();
    colors.popover_foreground = Theme::text();
    colors.overlay = Theme::scrim();

    colors.list = Theme::bg();
    colors.list_active = Theme::selection_fill();
    colors.list_active_border = Theme::accent();
    colors.list_even = Theme::row_alt();
    colors.list_head = Theme::panel_bg();
    colors.list_hover = Theme::row_hover();

    colors.sidebar = Theme::sidebar_bg();
    colors.sidebar_foreground = Theme::text();
    colors.sidebar_border = Theme::border();
    colors.sidebar_accent = Theme::row_hover();
    colors.sidebar_accent_foreground = Theme::text();
    colors.sidebar_primary = Theme::accent();
    colors.sidebar_primary_foreground = primary_ink;

    colors.tab_bar = Theme::panel_bg();
    colors.tab = gpui::transparent_black();
    colors.tab_active = Theme::bg();
    colors.tab_foreground = Theme::text_dim();
    colors.tab_active_foreground = Theme::text();
    colors.tab_bar_segmented = Theme::panel_bg();

    colors.table = Theme::bg();
    colors.table_active = Theme::selection_fill();
    colors.table_active_border = Theme::accent();
    colors.table_even = Theme::row_alt();
    colors.table_head = Theme::panel_bg();
    colors.table_head_foreground = Theme::text_dim();
    colors.table_hover = Theme::row_hover();
    colors.table_row_border = Theme::border();

    colors.title_bar = Theme::panel_bg();
    colors.title_bar_border = Theme::border();

    colors.scrollbar = gpui::transparent_black();
    colors.scrollbar_thumb = Theme::border();
    colors.scrollbar_thumb_hover = Theme::text_dim();

    colors.slider_bar = Theme::accent();
    colors.slider_thumb = Theme::text();

    colors.switch = Theme::border();
    colors.switch_thumb = Theme::bg();

    colors.skeleton = Theme::row_alt();
    colors.link = Theme::underline_accent();
    colors.link_hover = Theme::accent().opacity(0.9);
    colors.link_active = Theme::accent().opacity(0.8);
    colors.drag_border = Theme::accent();
    colors.drop_target = Theme::accent().opacity(0.15);
    colors.window_border = Theme::border();
    colors.progress_bar = Theme::accent();

    // Buttons. The crate's Button paints filled variants from the button_*
    // family and the Default/outline label from button_foreground; leaving
    // these at the crate defaults renders light-theme ink on the app palette
    // (the invisible "Sign out / switch account" button on the dark theme).
    colors.button = Theme::panel_bg();
    colors.button_foreground = Theme::text();
    colors.button_hover = Theme::row_hover();
    colors.button_active = Theme::row_alt();

    colors.button_primary = Theme::accent();
    colors.button_primary_foreground = primary_ink;
    colors.button_primary_hover = Theme::accent().opacity(0.9);
    colors.button_primary_active = Theme::accent().opacity(0.8);

    colors.button_secondary = Theme::panel_bg();
    colors.button_secondary_foreground = Theme::text();
    colors.button_secondary_hover = Theme::row_hover();
    colors.button_secondary_active = Theme::row_alt();

    colors.button_danger = Theme::error();
    colors.button_danger_foreground = Theme::ink_on(Theme::error());
    colors.button_danger_hover = Theme::error().opacity(0.9);
    colors.button_danger_active = Theme::error().opacity(0.8);

    colors.button_warning = Theme::warning();
    colors.button_warning_foreground = Theme::ink_on(Theme::warning());
    colors.button_warning_hover = Theme::warning().opacity(0.9);
    colors.button_warning_active = Theme::warning().opacity(0.8);

    colors.button_info = Theme::info();
    colors.button_info_foreground = Theme::ink_on(Theme::info());
    colors.button_info_hover = Theme::info().opacity(0.9);
    colors.button_info_active = Theme::info().opacity(0.8);

    colors.button_success = Theme::success();
    colors.button_success_foreground = Theme::ink_on(Theme::success());
    colors.button_success_hover = Theme::success().opacity(0.9);
    colors.button_success_active = Theme::success().opacity(0.8);

    colors.group_box = Theme::panel_bg();
    colors.group_box_foreground = Theme::text();
    // `accordion_hover` was removed upstream (the accordion no longer paints a
    // hover background); only the base background remains themable.
    colors.accordion = Theme::bg();
    colors.status_bar = Theme::panel_bg();
    colors.status_bar_border = Theme::border();
    colors.table_foot = Theme::panel_bg();
    colors.table_foot_foreground = Theme::text_dim();
    colors.description_list_label = Theme::panel_bg();
    colors.description_list_label_foreground = Theme::text_dim();
    // `tiles` was removed upstream in gpui-component 0.6 along with the Tiles
    // dock surface; nothing left to theme.

    colors.chart_1 = Theme::accent();
    colors.chart_2 = Theme::info();
    colors.chart_3 = Theme::success();
    colors.chart_4 = Theme::warning();
    colors.chart_5 = Theme::error();
    colors.chart_bullish = Theme::success();
    colors.chart_bearish = Theme::error();

    colors.red = Theme::error();
    colors.red_light = lighten(Theme::error(), 0.25);
    colors.green = Theme::success();
    colors.green_light = lighten(Theme::success(), 0.25);
    colors.blue = Theme::info();
    colors.blue_light = lighten(Theme::info(), 0.25);
    colors.yellow = Theme::warning();
    colors.yellow_light = lighten(Theme::warning(), 0.25);
    colors.magenta = h(0.83, 0.6, 0.65, 1.0);
    colors.magenta_light = h(0.83, 0.6, 0.78, 1.0);
    colors.cyan = h(0.52, 0.6, 0.6, 1.0);
    colors.cyan_light = h(0.52, 0.6, 0.75, 1.0);

    // Rebuild the resolved token set from the freshly-mapped colors. The
    // library derives `tokens` from `colors` only at Theme construction; menus,
    // popovers, dropdowns, and context menus render from `tokens.*` (not
    // `colors.*`), so mutating `colors` alone leaves every kit menu painted from
    // the DEFAULT palette — the "dropdowns & context menus don't match" bug.
    // Re-deriving here re-syncs every kit floating surface at once.
    theme.tokens = gpui_component::ThemeTokens::from(&theme.colors);

    // Rebuild the Base-layer theme and the TextView (markdown) defaults from
    // the freshly-mapped colors. TextView paints from defaults installed at
    // `gpui_component::init` — without this, every `TextView::markdown` (AI
    // summary bar, notebook cells) keeps the library's light-palette
    // foreground: near-black text on the dark theme.
    gpui_component::Theme::sync_base(cx);

    cx.refresh_windows();
}

struct FontRegistry {
    interface_font: String,
    interface_font_size: f32,
    editor_font: String,
    editor_font_size: f32,
    results_font_size: Option<f32>,
}

static FONT_REGISTRY: OnceLock<RwLock<FontRegistry>> = OnceLock::new();

fn font_registry_inner() -> &'static RwLock<FontRegistry> {
    // Pre-preferences fallback only — `AppearancePreferences` (whose Default
    // is the real source of truth, 10pt on wasm / 13pt native) overwrites
    // this via `set_font_prefs` during startup.
    let default_size = if cfg!(target_arch = "wasm32") {
        10.0
    } else {
        13.0
    };
    FONT_REGISTRY.get_or_init(move || {
        RwLock::new(FontRegistry {
            interface_font: "System".to_string(),
            interface_font_size: default_size,
            editor_font: crate::DEFAULT_EDITOR_FONT.to_string(),
            editor_font_size: default_size,
            results_font_size: None,
        })
    })
}

fn font_registry() -> parking_lot::RwLockReadGuard<'static, FontRegistry> {
    font_registry_inner().read()
}

/// The theme `kind`/`family` are process-global mutable state. cargo runs
/// tests concurrently, so any test that mutates them (or asserts on resolved
/// colors) must serialize against the others. Crate-visible so tests in
/// other modules (e.g. `grid_bridge`) can take the same lock.
#[cfg(test)]
pub(crate) static THEME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::THEME_LOCK;
    use super::*;

    /// The component theme mapping must paint the crate's button family from
    /// the app palette — the sign-in window's outline button was invisible
    /// when `button_foreground` sat at the crate's light-theme default.
    #[gpui::test]
    fn component_theme_maps_the_button_family_to_the_app_palette(cx: &mut gpui::TestAppContext) {
        let _guard = THEME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }

            set_kind(ThemeKind::Dark);
            force_sync_component_theme(cx);
            let colors = &gpui_component::Theme::global(cx).colors;
            assert_eq!(colors.button, Theme::panel_bg());
            assert_eq!(colors.button_foreground, Theme::text());
            assert_eq!(colors.button_hover, Theme::row_hover());
            assert_eq!(colors.button_active, Theme::row_alt());
            assert_eq!(colors.button_primary, Theme::accent());
            assert_eq!(
                colors.button_primary_foreground,
                Theme::ink_on(Theme::accent())
            );
            assert_eq!(colors.button_secondary, Theme::panel_bg());
            assert_eq!(colors.button_secondary_foreground, Theme::text());
            assert_eq!(colors.button_danger, Theme::error());
            assert_eq!(
                colors.button_danger_foreground,
                Theme::ink_on(Theme::error())
            );
            assert_eq!(colors.button_warning, Theme::warning());
            assert_eq!(colors.button_info, Theme::info());
            assert_eq!(colors.button_success, Theme::success());
            assert_eq!(colors.input, Theme::border());
            assert_eq!(colors.border, Theme::border());

            set_kind(ThemeKind::Light);
            force_sync_component_theme(cx);
            let colors = &gpui_component::Theme::global(cx).colors;
            assert_eq!(colors.button_foreground, Theme::text());
            assert_eq!(colors.button_primary, Theme::accent());
            assert_eq!(colors.input, Theme::border());
            assert_eq!(colors.border, Theme::border());

            set_kind(ThemeKind::Dark);
        });
    }

    #[test]
    fn set_and_get_kind() {
        let _guard = THEME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_light_theme(ColorTheme::Light);
        set_dark_theme(ColorTheme::Dark);
        set_kind(ThemeKind::Light);
        assert_eq!(kind(), ThemeKind::Light);
        set_kind(ThemeKind::Dark);
        assert_eq!(kind(), ThemeKind::Dark);
        set_kind(ThemeKind::HighContrast);
        assert_eq!(kind(), ThemeKind::HighContrast);
        // System now follows the live OS appearance.
        set_kind(ThemeKind::System);
        assert_eq!(kind(), ThemeKind::System);
        set_system_dark(true);
        assert_eq!(resolved_kind(), ThemeKind::Dark);
        set_system_dark(false);
        assert_eq!(resolved_kind(), ThemeKind::Light);
    }

    #[test]
    fn system_follows_os_appearance_colors() {
        let _guard = THEME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_light_theme(ColorTheme::Light);
        set_dark_theme(ColorTheme::Dark);
        set_kind(ThemeKind::System);
        set_system_dark(true);
        let sys_dark_bg = Theme::bg();
        set_system_dark(false);
        let sys_light_bg = Theme::bg();
        assert_ne!(
            sys_dark_bg, sys_light_bg,
            "System theme must produce different colors for OS dark vs light"
        );
        // And they must match the explicit Dark/Light palettes.
        set_kind(ThemeKind::Dark);
        assert_eq!(sys_dark_bg, Theme::bg());
        set_kind(ThemeKind::Light);
        assert_eq!(sys_light_bg, Theme::bg());
    }

    #[test]
    fn light_differs_from_dark() {
        let _guard = THEME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_light_theme(ColorTheme::Light);
        set_dark_theme(ColorTheme::Dark);
        set_kind(ThemeKind::Dark);
        let dark_bg = Theme::bg();
        set_kind(ThemeKind::Light);
        let light_bg = Theme::bg();
        assert_ne!(dark_bg, light_bg);
    }

    #[test]
    fn high_contrast_has_max_contrast() {
        let _guard = THEME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_dark_theme(ColorTheme::HighContrastDark);
        set_kind(ThemeKind::Dark);
        let bg = Theme::bg();
        let text = Theme::text();
        assert_eq!(bg.l, 0.0);
        assert_eq!(text.l, 1.0);
        set_light_theme(ColorTheme::HighContrastLight);
        set_kind(ThemeKind::Light);
        assert_eq!(Theme::bg().l, 1.0);
        assert_eq!(Theme::text().l, 0.0);
        set_light_theme(ColorTheme::Light);
        set_dark_theme(ColorTheme::Dark);
        set_kind(ThemeKind::Dark);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn dark_shade_ladder_increases_monotonically() {
        assert_eq!(
            AMBER_DARK.bg.l, AMBER_DARK.sidebar_bg.l,
            "sidebar shares the canvas; regions separate with hairlines"
        );
        assert_eq!(
            AMBER_DARK.tab_strip_bg.l, AMBER_DARK.bg.l,
            "tab strip shares the canvas; active tabs use panel_bg fill"
        );
        assert!(
            AMBER_DARK.bg.l < AMBER_DARK.panel_bg.l,
            "raised panel must sit above the unified canvas"
        );
        assert_eq!(
            AMBER_DARK.panel_bg.l, AMBER_DARK.popover_bg.l,
            "popover must match raised panel"
        );
        assert!(
            AMBER_DARK.row_hover.l > AMBER_DARK.sidebar_bg.l,
            "hover must lift off the unified canvas"
        );
        assert_eq!(
            AMBER_LIGHT.bg.l, AMBER_LIGHT.sidebar_bg.l,
            "light sidebar shares the canvas too"
        );
        assert_eq!(
            AMBER_LIGHT.tab_strip_bg.l, AMBER_LIGHT.bg.l,
            "light tab strip shares the canvas too"
        );
        assert!(
            AMBER_LIGHT.row_hover.l < AMBER_LIGHT.bg.l,
            "light hover is a gray lift off white canvas"
        );
    }

    #[test]
    fn round_trip_u8() {
        for k in [
            ThemeKind::System,
            ThemeKind::Light,
            ThemeKind::Dark,
            ThemeKind::HighContrast,
        ] {
            assert_eq!(ThemeKind::from_u8(k.to_u8()), k);
        }
    }

    #[test]
    fn system_resolves_to_dark_palette_when_os_dark() {
        let _guard = THEME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_dark_theme(ColorTheme::Dark);
        set_kind(ThemeKind::System);
        set_system_dark(true);
        assert_eq!(Theme::bg(), ColorTheme::Dark.palette().bg);
        set_kind(ThemeKind::Dark);
    }

    #[test]
    fn named_dark_theme_applies_in_dark_mode() {
        let _guard = THEME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_light_theme(ColorTheme::AmberLight);
        set_dark_theme(ColorTheme::Dark);
        set_kind(ThemeKind::Dark);
        assert_eq!(Theme::bg(), ColorTheme::Dark.palette().bg);
        assert_ne!(Theme::bg(), AMBER_DARK.bg);
        set_kind(ThemeKind::Light);
        assert_eq!(Theme::bg(), AMBER_LIGHT.bg);
        set_kind(ThemeKind::System);
        set_system_dark(true);
        assert_eq!(Theme::bg(), ColorTheme::Dark.palette().bg);
        set_dark_theme(ColorTheme::Dark);
        set_kind(ThemeKind::Dark);
    }

    #[test]
    fn font_family_resolves_system_on_macos() {
        Theme::set_font_prefs("System", 13.0, "Fira Code", 13.0);
        #[cfg(target_os = "macos")]
        assert_eq!(Theme::font_family(), ".SystemUIFont");
        #[cfg(target_os = "windows")]
        assert_eq!(Theme::font_family(), "Segoe UI");
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        assert_eq!(Theme::font_family(), "Inter");
        Theme::set_font_prefs("Fira Code", 13.0, "Fira Code", 13.0);
    }

    #[test]
    fn function_accessor_matches_kind_function_and_syntax_color() {
        let _guard = THEME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_light_theme(ColorTheme::Light);
        set_dark_theme(ColorTheme::Dark);
        set_kind(ThemeKind::Dark);
        use crate::sql_syntax::SyntaxTokenKind;
        assert_ne!(Theme::function(), Theme::keyword());
        assert_eq!(
            Theme::syntax_color(SyntaxTokenKind::Constant),
            Theme::constant()
        );
        assert_eq!(
            Theme::syntax_color(SyntaxTokenKind::Type),
            Theme::type_name()
        );
        assert_eq!(
            Theme::syntax_color(SyntaxTokenKind::Function),
            Theme::function()
        );
        assert_eq!(
            Theme::syntax_color(SyntaxTokenKind::Keyword),
            Theme::keyword()
        );
        assert_eq!(
            Theme::syntax_color(SyntaxTokenKind::String),
            Theme::string()
        );
    }
}

/// WCAG contrast verification for the Neutral and Signature palettes, ported
/// from the sqlly-datatable crate's `wcag` test module: every text role is
/// checked against every surface it is painted on, at AA thresholds (4.5:1
/// for text, 3:1 for UI indicators; 7:1 AAA for body text on the canvas).
#[cfg(test)]
mod wcag {
    use super::*;

    /// The contrast math lives at module scope so runtime clamps
    /// ([`Theme::entity_tint`]) and these assertions can never disagree about
    /// what "4.5:1" means.
    use super::contrast_ratio as contrast;

    /// Composite `fg` at `alpha` over the opaque `bg`, returning the color a
    /// user's eye actually receives. Needed because the matrix reasons about
    /// opaque palette entries, while `themed_row` fills with a translucent
    /// accent — so without this, the fill under a selected row's label is a
    /// color no assertion has ever seen.
    fn blend(fg: Hsla, alpha: f32, bg: Hsla) -> Hsla {
        let f: gpui::Rgba = fg.into();
        let b: gpui::Rgba = bg.into();
        gpui::Rgba {
            r: f.r * alpha + b.r * (1.0 - alpha),
            g: f.g * alpha + b.g * (1.0 - alpha),
            b: f.b * alpha + b.b * (1.0 - alpha),
            a: 1.0,
        }
        .into()
    }

    fn requirements(t: &Palette) -> Vec<(Hsla, Hsla, f32, &'static str)> {
        let sel_tint = t.selection.a;
        let sel_hover = super::Theme::SELECTION_HOVER_TINT;
        let mut reqs = vec![
            (t.text, t.bg, 7.0, "body text on bg"),
            (t.text, t.panel_bg, 4.5, "body text on panel"),
            (t.text, t.sidebar_bg, 4.5, "body text on sidebar"),
            (t.text, t.tab_strip_bg, 4.5, "body text on tab strip"),
            (t.text, t.row_alt, 7.0, "body text on zebra row"),
            (
                t.text,
                super::mix(t.success, t.bg, 0.06),
                7.0,
                "body text on result-source line",
            ),
            (
                t.text_dim,
                super::mix(t.success, t.bg, 0.06),
                4.5,
                "muted text on result-source line",
            ),
            (t.accent, t.sidebar_bg, 3.0, "gutter run glyph on sidebar"),
            (
                t.success,
                t.sidebar_bg,
                3.0,
                "gutter run outcome on sidebar",
            ),
            (t.error, t.sidebar_bg, 3.0, "gutter run outcome on sidebar"),
            // A hovered row still has to be readable — this is the pairing that
            // `border`-as-hover-fill was silently failing in High Contrast.
            (t.text, t.row_hover, 4.5, "body text on hovered row"),
            (t.text_dim, t.row_hover, 4.5, "muted text on hovered row"),
            // The formatting-directives reference paints a focused sample row
            // in row_hover with an accent focus ring.
            (t.accent, t.row_hover, 3.0, "focus ring on hovered row"),
            (t.text, t.popover_bg, 7.0, "menu labels"),
            (t.text_dim, t.bg, 4.5, "muted text on bg"),
            (t.text_dim, t.panel_bg, 4.5, "muted text on panel"),
            (t.text_dim, t.sidebar_bg, 4.5, "muted text on sidebar"),
            (t.text_dim, t.tab_strip_bg, 4.5, "muted text on tab strip"),
            (t.text_dim, t.popover_bg, 4.5, "muted text on popup"),
            (t.accent, t.bg, 3.0, "accent indicator on bg"),
            (t.accent, t.panel_bg, 3.0, "accent indicator on panel"),
            (t.error, t.bg, 4.5, "error text on bg"),
            (t.love, t.bg, 4.5, "tagline love on bg"),
            (t.love, t.panel_bg, 4.5, "tagline love on panel"),
            (t.warning, t.bg, 4.5, "warning text on bg"),
            (t.info, t.bg, 4.5, "info text on bg"),
            (t.success, t.bg, 4.5, "success text on bg"),
            (
                t.text,
                blend(t.success, 0.18, t.bg),
                4.5,
                "staged insert marker on canvas",
            ),
            (
                t.text,
                blend(t.warning, 0.18, t.bg),
                4.5,
                "staged edit marker on canvas",
            ),
            (
                t.text,
                blend(t.error, 0.18, t.bg),
                4.5,
                "staged delete marker on canvas",
            ),
            // The accent and error are used as *fills* under a label — hovered
            // and danger menu rows, the primary Run button. The matrix used to
            // check only text-on-surface, which is why Ink-on-accent (1.83:1 in
            // Neutral Dark) shipped. The label flips to the canvas color, so
            // that is the pair to assert.
            // label on accent fill uses ink_on(accent), not canvas.
            (
                Theme::ink_on(t.accent),
                t.accent,
                4.5,
                "label on accent fill",
            ),
            (t.bg, t.error, 4.5, "label on error fill"),
            (
                Theme::ink_on(t.warning),
                t.warning,
                4.5,
                "label on production chip",
            ),
            // Semantic colors are painted on chrome, not just the canvas: the
            // status bar sits on panel, the sidebar tree reports errors, and
            // popups carry validation text.
            (t.error, t.panel_bg, 4.5, "error text on panel"),
            (t.warning, t.panel_bg, 4.5, "warning text on panel"),
            (t.info, t.panel_bg, 4.5, "info text on panel"),
            (t.success, t.panel_bg, 4.5, "success text on panel"),
            (t.error, t.sidebar_bg, 4.5, "error text on sidebar"),
            // The query-history banner's marker dot is an info-colored
            // indicator on the results chrome (sidebar surface).
            (
                t.info,
                t.sidebar_bg,
                3.0,
                "history banner marker on sidebar",
            ),
            // The schema tree marks the connected server by turning its kind
            // glyph green. That is an indicator, not prose — 3:1 is the bar
            // WCAG sets for it, and it is stricter than what the other `kind_*`
            // glyph colors are held to today, which is nothing.
            (
                t.success,
                t.sidebar_bg,
                3.0,
                "connected server icon on sidebar",
            ),
            (
                t.success,
                t.row_hover,
                3.0,
                "connected server icon on hovered row",
            ),
            // The selection fill, on the two surfaces it is painted over, at
            // rest and hovered. Only full ink is asserted because only full ink
            // is allowed on it — `Theme::selection_fg` enforces that, and every
            // other foreground measured 1.7–4.9 here.
            (
                t.text,
                super::mix(t.accent, t.sidebar_bg, sel_tint),
                4.5,
                "body text on selected row",
            ),
            (
                t.text,
                super::mix(t.accent, t.sidebar_bg, sel_hover),
                4.5,
                "body text on selected + hovered row",
            ),
            // (There is deliberately no "selected over the hover fill" entry:
            // `themed_row` replaces the background when selected, so the accent
            // composites over the sidebar, never over `row_hover`.)
            // Failed load is a status *dot*, not the word "failed" — indicator
            // contrast (3:1), not the text bar.
            (t.error, t.sidebar_bg, 3.0, "failed status dot at rest"),
            (t.error, t.popover_bg, 4.5, "error text on popup"),
            // A dialog's status line reports success and failure on the popup
            // surface, not the canvas. `error` was asserted here; `success` was
            // not, so the green half of that pairing went unchecked on the one
            // surface a dialog can actually paint it.
            (t.success, t.popover_bg, 4.5, "success text on popup"),
            (t.text, t.input_bg, 4.5, "body text on input"),
            (t.badge_table, t.sidebar_bg, 3.0, "table badge on sidebar"),
            (t.badge_view, t.sidebar_bg, 3.0, "view badge on sidebar"),
            (t.badge_proc, t.sidebar_bg, 3.0, "proc badge on sidebar"),
            (t.badge_fn, t.sidebar_bg, 3.0, "fn badge on sidebar"),
        ];
        // `themed_row` paints selection as accent at [`Theme::selection_tint`]
        // (hovered-selected at [`Theme::SELECTION_HOVER_TINT`]) over whatever
        // surface the row sits on. Composite the fill and assert the ink painted
        // on top.
        for (surface, what) in [
            (t.bg, "body text on selected row (canvas)"),
            (t.sidebar_bg, "body text on selected row (sidebar)"),
            (t.panel_bg, "body text on selected row (panel)"),
            (t.popover_bg, "body text on selected row (popup)"),
        ] {
            reqs.push((t.text, blend(t.accent, sel_tint, surface), 4.5, what));
        }
        for (surface, what) in [
            (t.bg, "body text on selected+hovered row (canvas)"),
            (t.sidebar_bg, "body text on selected+hovered row (sidebar)"),
            (t.panel_bg, "body text on selected+hovered row (panel)"),
            (t.popover_bg, "body text on selected+hovered row (popup)"),
        ] {
            reqs.push((t.text, blend(t.accent, sel_hover, surface), 4.5, what));
        }
        // The editor's own transient surfaces, previously untested:
        // • text over the editor selection highlight (alpha fill on canvas),
        // • text over the completion popup's selected row and the format
        //   dialog's keyboard-highlighted choice (both `completion_selected`
        //   over the popup surface),
        // • warning/info gutter dots (indicators on the sidebar-colored
        //   gutter) and diagnostic tooltip borders (indicators on popup),
        // • the caret against the canvas,
        // • DDL link-hover text, which paints `accent` as *prose* and so
        //   answers to the 4.5:1 text bar, not the 3:1 indicator bar.
        reqs.push((
            t.text,
            blend(t.selection, t.selection.a, t.bg),
            4.5,
            "body text over editor selection",
        ));
        reqs.push((
            t.text,
            blend(t.completion_selected, t.completion_selected.a, t.popover_bg),
            4.5,
            "body text over selected completion row",
        ));
        reqs.push((t.warning, t.sidebar_bg, 3.0, "warning gutter dot"));
        reqs.push((t.info, t.sidebar_bg, 3.0, "info gutter dot"));
        reqs.push((t.warning, t.popover_bg, 3.0, "warning tooltip border"));
        reqs.push((t.info, t.popover_bg, 3.0, "info tooltip border"));
        reqs.push((t.cursor, t.bg, 3.0, "caret on canvas"));
        reqs.push((t.accent, t.bg, 4.5, "DDL link-hover text on canvas"));
        let s = &t.syntax;
        // Completion-kind colors are rendered on the completion popup, which is
        // a different surface from the editor canvas — and in dark mode a
        // lighter one, i.e. the harder case the matrix was not testing.
        for (fg, what) in [
            (s.kind_keyword, "kind_keyword on popup"),
            (s.kind_table, "kind_table on popup"),
            (s.kind_column, "kind_column on popup"),
            (s.kind_function, "kind_function on popup"),
            (s.kind_procedure, "kind_procedure on popup"),
            (s.kind_database, "kind_database on popup"),
            (s.kind_schema, "kind_schema on popup"),
            (s.kind_snippet, "kind_snippet on popup"),
            (s.kind_default, "kind_default on popup"),
            (s.kind_view, "kind_view on popup"),
            (s.kind_index, "kind_index on popup"),
            (s.kind_trigger, "kind_trigger on popup"),
            (s.kind_constraint, "kind_constraint on popup"),
            (s.kind_string, "kind_string on popup"),
        ] {
            reqs.push((fg, t.popover_bg, 4.5, what));
        }
        for (fg, what) in [
            (s.keyword, "keyword"),
            (s.string, "string"),
            (s.comment, "comment"),
            (s.number, "number"),
            (s.kind_keyword, "kind_keyword"),
            (s.kind_table, "kind_table"),
            (s.kind_column, "kind_column"),
            (s.kind_function, "kind_function"),
            (s.kind_procedure, "kind_procedure"),
            (s.kind_database, "kind_database"),
            (s.kind_schema, "kind_schema"),
            (s.kind_snippet, "kind_snippet"),
            (s.kind_default, "kind_default"),
            (s.kind_view, "kind_view"),
            (s.kind_index, "kind_index"),
            (s.kind_trigger, "kind_trigger"),
            (s.kind_constraint, "kind_constraint"),
            (s.kind_string, "kind_string"),
        ] {
            reqs.push((fg, t.bg, 4.5, what));
        }
        reqs
    }

    fn all_palettes() -> Vec<(String, &'static Palette)> {
        ColorTheme::ALL
            .iter()
            .map(|t| (t.label(), t.palette()))
            .collect()
    }

    #[test]
    fn every_palette_meets_wcag_contrast() {
        // Collect every failure rather than tripping on the first, so a palette
        // change reports its whole blast radius in one run.
        let mut failures = Vec::new();
        for (name, palette) in all_palettes() {
            for (fg, bg, min, what) in requirements(palette) {
                let ratio = contrast(fg, bg);
                if ratio < min {
                    failures.push(format!("{name}: {what} — {ratio:.2} < {min}"));
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} WCAG contrast failure(s):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// Strings and comments sit next to each other constantly (`WHERE name =
    /// 'x' -- why`), and for a while every palette painted both green, so a
    /// commented-out literal was indistinguishable from a live one. Guard the
    /// separation rather than the specific hues: any future palette is free to
    /// pick its own two colors as long as they read apart.
    #[test]
    fn string_and_comment_inks_stay_distinct() {
        /// Distance around the hue circle, in turns (max 0.5).
        fn hue_gap(a: Hsla, b: Hsla) -> f32 {
            let raw = (a.h - b.h).abs();
            raw.min(1.0 - raw)
        }

        let mut failures = Vec::new();
        for (name, p) in all_palettes() {
            let (string, comment) = (p.syntax.string, p.syntax.comment);
            // ~29° apart, or far enough apart in lightness to read as two
            // inks even at the same hue.
            let separated =
                hue_gap(string, comment) >= 0.08 || (string.l - comment.l).abs() >= 0.25;
            if !separated {
                failures.push(format!(
                    "{name}: string {string:?} and comment {comment:?} are too close"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} palette(s) paint strings and comments alike:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// Every kind of word the editor paints — keyword, function, constant,
    /// type, variable, number, string, comment, and plain text — must read
    /// as its own color in every theme, and every syntax ink must stay
    /// readable on the editor canvas. For a long time keywords and function
    /// names shared the accent in every theme, and constants shared the
    /// number ink.
    #[test]
    fn every_editor_token_kind_has_its_own_readable_color() {
        let mut failures = Vec::new();
        for (name, p) in all_palettes() {
            let s = &p.syntax;
            let inks = [
                ("keyword", s.keyword),
                ("function", s.function),
                ("constant", s.constant),
                ("type", s.type_name),
                ("variable", s.variable),
                ("number", s.number),
                ("string", s.string),
                ("comment", s.comment),
                ("text", p.text),
            ];
            for (i, (a_name, a)) in inks.iter().enumerate() {
                for (b_name, b) in &inks[i + 1..] {
                    let d = super::color_distance(*a, *b);
                    if d < super::MIN_INK_SEPARATION {
                        failures.push(format!("{name}: {a_name} vs {b_name} ΔE {d:.3}"));
                    }
                }
            }
            let high_contrast = name.contains("High Contrast");
            let min = if high_contrast { 7.0 } else { 4.5 };
            for (ink_name, ink) in &inks[1..6] {
                for (surface_name, surface) in [("bg", p.bg), ("panel", p.panel_bg)] {
                    let ratio = contrast(*ink, surface);
                    if ratio < min {
                        failures.push(format!(
                            "{name}: {ink_name} on {surface_name} — {ratio:.2} < {min}"
                        ));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} syntax color problem(s):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// Entity colors are user-supplied, so the pairing that ships is not a
    /// color but the *clamp*: whatever hex someone types, the group header it
    /// tints must still carry body text at AA. Sweep the space rather than
    /// spot-check the colors we happened to think of.
    #[test]
    fn entity_tint_keeps_body_text_readable_for_any_user_color() {
        // 24 hues × 9 lightnesses × 3 saturations, plus pure black/white.
        let mut colors: Vec<Hsla> = Vec::new();
        for h in 0..24 {
            for l in 1..10 {
                for s in [0.0_f32, 0.5, 1.0] {
                    colors.push(gpui::hsla(h as f32 / 24.0, s, l as f32 / 10.0, 1.0));
                }
            }
        }
        colors.push(gpui::hsla(0.0, 0.0, 0.0, 1.0));
        colors.push(gpui::hsla(0.0, 0.0, 1.0, 1.0));

        let mut failures = Vec::new();
        for (name, palette) in all_palettes() {
            // Both surfaces a tinted row is painted on: resting (sidebar) and
            // hovered (border).
            for (surface, base) in [("rest", palette.sidebar_bg), ("hover", palette.row_hover)] {
                for c in &colors {
                    let tinted = super::entity_tint_on(*c, base, palette.text);
                    let ratio = contrast(palette.text, tinted);
                    if ratio < 4.5 {
                        failures.push(format!(
                            "{name}/{surface}: hsla({:.2},{:.2},{:.2}) → {ratio:.2} < 4.5",
                            c.h, c.s, c.l
                        ));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} entity-tint contrast failure(s):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// A status-bar chip paints the entity color at full strength, so the clamp
    /// that saves the group header ([`entity_tint_on`]) does not apply — the ink
    /// moves instead. Sweep the same space and assert the label always clears
    /// AA on the chip.
    #[test]
    fn ink_on_any_fill_clears_aa() {
        let mut failures = Vec::new();
        for h in 0..24 {
            for l in 0..=10 {
                for s in [0.0_f32, 0.5, 1.0] {
                    let fill = gpui::hsla(h as f32 / 24.0, s, l as f32 / 10.0, 1.0);
                    let ratio = contrast(Theme::ink_on(fill), fill);
                    if ratio < 4.5 {
                        failures.push(format!(
                            "hsla({:.2},{:.2},{:.2}) → {ratio:.2} < 4.5",
                            fill.h, fill.s, fill.l
                        ));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} chip-ink contrast failure(s):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    #[test]
    fn parse_hex_color_round_trips_and_rejects_junk() {
        let red = super::parse_hex_color("#FF0000").expect("valid hex");
        assert!(contrast(red, gpui::hsla(0.0, 1.0, 0.5, 1.0)) < 1.05);
        assert!(super::parse_hex_color("00ff00").is_some(), "bare hex is ok");
        assert!(
            super::parse_hex_color("#GGGGGG").is_none(),
            "non-hex digits"
        );
        assert!(super::parse_hex_color("#FFF").is_none(), "short form");
        assert!(super::parse_hex_color("").is_none());
    }

    #[test]
    fn chrome_bars_share_one_height() {
        assert_eq!(Theme::chrome_bar_height(), Theme::tab_strip_height());
        assert_eq!(Theme::chrome_bar_height(), Theme::bar_height(6.0));
    }
}
