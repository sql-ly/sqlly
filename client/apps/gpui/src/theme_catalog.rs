//! Named color themes shown in Settings → Appearance → Themes.
//!
//! Light / Dark / System is the *mode*; each mode resolves to one of these
//! palettes. The default `Dark` and `Light` are both the SQLLY brand palette.
//! Every other family ships as a Light/Dark pair.

use std::sync::OnceLock;

use gpui::Hsla;
use serde::{Deserialize, Serialize};

use super::{
    AMBER_DARK, AMBER_LIGHT, DARK, HIGH_CONTRAST, HIGH_CONTRAST_LIGHT, LIGHT, Palette,
    parse_hex_color,
};

fn hx(hex: &str) -> Hsla {
    parse_hex_color(hex).unwrap_or_else(|| super::h(0.0, 0.0, 0.5, 1.0))
}

fn with_alpha(mut c: Hsla, a: f32) -> Hsla {
    c.a = a;
    c
}

/// Colors a theme-gallery thumbnail needs, without exposing the full palette.
#[derive(Clone, Copy)]
pub struct ThemePreview {
    pub bg: Hsla,
    pub sidebar: Hsla,
    pub accent: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum ColorTheme {
    #[default]
    Light,
    AmberLight,
    CobaltLight,
    GraphiteLight,
    SignatureLight,
    NordicLight,
    LavenderLight,
    TokyoLight,
    SepiaLight,
    HarborLight,
    SolarizedLight,
    HighContrastLight,
    Dark,
    AmberDark,
    CobaltDark,
    GraphiteDark,
    SignatureDark,
    NordicDark,
    LavenderDark,
    TokyoDark,
    SepiaDark,
    HarborDark,
    SolarizedDark,
    HighContrastDark,
}

impl ColorTheme {
    pub const LIGHT: [Self; 12] = [
        Self::Light,
        Self::AmberLight,
        Self::CobaltLight,
        Self::GraphiteLight,
        Self::SignatureLight,
        Self::NordicLight,
        Self::LavenderLight,
        Self::TokyoLight,
        Self::SepiaLight,
        Self::HarborLight,
        Self::SolarizedLight,
        Self::HighContrastLight,
    ];

    pub const DARK: [Self; 12] = [
        Self::Dark,
        Self::AmberDark,
        Self::CobaltDark,
        Self::GraphiteDark,
        Self::SignatureDark,
        Self::NordicDark,
        Self::LavenderDark,
        Self::TokyoDark,
        Self::SepiaDark,
        Self::HarborDark,
        Self::SolarizedDark,
        Self::HighContrastDark,
    ];

    #[allow(dead_code)]
    pub const ALL: [Self; 24] = [
        Self::Light,
        Self::AmberLight,
        Self::CobaltLight,
        Self::GraphiteLight,
        Self::SignatureLight,
        Self::NordicLight,
        Self::LavenderLight,
        Self::TokyoLight,
        Self::SepiaLight,
        Self::HarborLight,
        Self::SolarizedLight,
        Self::HighContrastLight,
        Self::Dark,
        Self::AmberDark,
        Self::CobaltDark,
        Self::GraphiteDark,
        Self::SignatureDark,
        Self::NordicDark,
        Self::LavenderDark,
        Self::TokyoDark,
        Self::SepiaDark,
        Self::HarborDark,
        Self::SolarizedDark,
        Self::HighContrastDark,
    ];

    pub fn is_dark(self) -> bool {
        matches!(
            self,
            Self::Dark
                | Self::AmberDark
                | Self::CobaltDark
                | Self::GraphiteDark
                | Self::SignatureDark
                | Self::NordicDark
                | Self::LavenderDark
                | Self::TokyoDark
                | Self::SepiaDark
                | Self::HarborDark
                | Self::SolarizedDark
                | Self::HighContrastDark
        )
    }

    pub fn label(self) -> String {
        match self {
            Self::Light => crate::localization::text("settings.appearance.themes.palette.light"),
            Self::AmberLight => {
                crate::localization::text("settings.appearance.themes.palette.amber_light")
            }
            Self::CobaltLight => {
                crate::localization::text("settings.appearance.themes.palette.cobalt_light")
            }
            Self::GraphiteLight => {
                crate::localization::text("settings.appearance.themes.palette.graphite_light")
            }
            Self::SignatureLight => {
                crate::localization::text("settings.appearance.themes.palette.signature_light")
            }
            Self::NordicLight => {
                crate::localization::text("settings.appearance.themes.palette.nordic_light")
            }
            Self::LavenderLight => {
                crate::localization::text("settings.appearance.themes.palette.lavender_light")
            }
            Self::TokyoLight => {
                crate::localization::text("settings.appearance.themes.palette.tokyo_light")
            }
            Self::SepiaLight => {
                crate::localization::text("settings.appearance.themes.palette.sepia_light")
            }
            Self::HarborLight => {
                crate::localization::text("settings.appearance.themes.palette.harbor_light")
            }
            Self::SolarizedLight => {
                crate::localization::text("settings.appearance.themes.palette.solarized_light")
            }
            Self::HighContrastLight => {
                crate::localization::text("settings.appearance.themes.palette.high_contrast_light")
            }
            Self::Dark => crate::localization::text("settings.appearance.themes.palette.dark"),
            Self::AmberDark => {
                crate::localization::text("settings.appearance.themes.palette.amber_dark")
            }
            Self::CobaltDark => {
                crate::localization::text("settings.appearance.themes.palette.cobalt_dark")
            }
            Self::GraphiteDark => {
                crate::localization::text("settings.appearance.themes.palette.graphite_dark")
            }
            Self::SignatureDark => {
                crate::localization::text("settings.appearance.themes.palette.signature_dark")
            }
            Self::NordicDark => {
                crate::localization::text("settings.appearance.themes.palette.nordic_dark")
            }
            Self::LavenderDark => {
                crate::localization::text("settings.appearance.themes.palette.lavender_dark")
            }
            Self::TokyoDark => {
                crate::localization::text("settings.appearance.themes.palette.tokyo_dark")
            }
            Self::SepiaDark => {
                crate::localization::text("settings.appearance.themes.palette.sepia_dark")
            }
            Self::HarborDark => {
                crate::localization::text("settings.appearance.themes.palette.harbor_dark")
            }
            Self::SolarizedDark => {
                crate::localization::text("settings.appearance.themes.palette.solarized_dark")
            }
            Self::HighContrastDark => {
                crate::localization::text("settings.appearance.themes.palette.high_contrast_dark")
            }
        }
    }

    #[allow(dead_code)]
    pub fn counterpart(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Light,
            Self::AmberLight => Self::AmberDark,
            Self::AmberDark => Self::AmberLight,
            Self::CobaltLight => Self::CobaltDark,
            Self::CobaltDark => Self::CobaltLight,
            Self::GraphiteLight => Self::GraphiteDark,
            Self::GraphiteDark => Self::GraphiteLight,
            Self::SignatureLight => Self::SignatureDark,
            Self::SignatureDark => Self::SignatureLight,
            Self::NordicLight => Self::NordicDark,
            Self::NordicDark => Self::NordicLight,
            Self::LavenderLight => Self::LavenderDark,
            Self::LavenderDark => Self::LavenderLight,
            Self::TokyoLight => Self::TokyoDark,
            Self::TokyoDark => Self::TokyoLight,
            Self::SepiaLight => Self::SepiaDark,
            Self::SepiaDark => Self::SepiaLight,
            Self::HarborLight => Self::HarborDark,
            Self::HarborDark => Self::HarborLight,
            Self::SolarizedLight => Self::SolarizedDark,
            Self::SolarizedDark => Self::SolarizedLight,
            Self::HighContrastLight => Self::HighContrastDark,
            Self::HighContrastDark => Self::HighContrastLight,
        }
    }

    pub fn to_u8(self) -> u8 {
        match self {
            Self::Light => 0,
            Self::AmberLight => 1,
            Self::CobaltLight => 2,
            Self::GraphiteLight => 3,
            Self::SignatureLight => 4,
            Self::NordicLight => 5,
            Self::LavenderLight => 6,
            Self::TokyoLight => 7,
            Self::SepiaLight => 8,
            Self::HarborLight => 9,
            Self::SolarizedLight => 10,
            Self::HighContrastLight => 11,
            Self::Dark => 12,
            Self::AmberDark => 13,
            Self::CobaltDark => 14,
            Self::GraphiteDark => 15,
            Self::SignatureDark => 16,
            Self::NordicDark => 17,
            Self::LavenderDark => 18,
            Self::TokyoDark => 19,
            Self::SepiaDark => 20,
            Self::HarborDark => 21,
            Self::SolarizedDark => 22,
            Self::HighContrastDark => 23,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::AmberLight,
            2 => Self::CobaltLight,
            3 => Self::GraphiteLight,
            4 => Self::SignatureLight,
            5 => Self::NordicLight,
            6 => Self::LavenderLight,
            7 => Self::TokyoLight,
            8 => Self::SepiaLight,
            9 => Self::HarborLight,
            10 => Self::SolarizedLight,
            11 => Self::HighContrastLight,
            12 => Self::Dark,
            13 => Self::AmberDark,
            14 => Self::CobaltDark,
            15 => Self::GraphiteDark,
            16 => Self::SignatureDark,
            17 => Self::NordicDark,
            18 => Self::LavenderDark,
            19 => Self::TokyoDark,
            20 => Self::SepiaDark,
            21 => Self::HarborDark,
            22 => Self::SolarizedDark,
            23 => Self::HighContrastDark,
            _ => Self::Light,
        }
    }

    /// Clamp a stored id onto the gallery it belongs to.
    pub fn sanitize_light(self) -> Self {
        if self.is_dark() { Self::Light } else { self }
    }

    pub fn sanitize_dark(self) -> Self {
        if self.is_dark() { self } else { Self::Dark }
    }

    pub fn preview(self) -> ThemePreview {
        let p = self.palette();
        ThemePreview {
            bg: p.bg,
            sidebar: p.sidebar_bg,
            accent: p.accent,
            text: p.text,
            muted: p.text_dim,
        }
    }

    pub fn palette(self) -> &'static Palette {
        // Every palette gets its editor syntax inks fitted once, on first use
        // (`with_editor_inks`), so no two token kinds share a color.
        macro_rules! fitted {
            ($build:expr) => {{
                static P: OnceLock<Palette> = OnceLock::new();
                P.get_or_init(|| super::with_editor_inks($build))
            }};
        }
        match self {
            Self::Dark => fitted!(DARK),
            Self::Light => fitted!(LIGHT),
            Self::HighContrastDark => fitted!(HIGH_CONTRAST),
            Self::HighContrastLight => fitted!(HIGH_CONTRAST_LIGHT),
            Self::AmberDark => fitted!(amber_dark()),
            Self::AmberLight => fitted!(amber_light()),
            Self::NordicDark => fitted!(nordic_dark()),
            Self::NordicLight => fitted!(nordic_light()),
            Self::LavenderDark => fitted!(lavender_dark()),
            Self::TokyoDark => fitted!(tokyo_dark()),
            Self::TokyoLight => fitted!(tokyo_light()),
            Self::LavenderLight => fitted!(lavender_light()),
            Self::HarborLight => fitted!(harbor_light()),
            Self::HarborDark => fitted!(harbor_dark()),
            Self::SolarizedLight => fitted!(solarized_light()),
            Self::SolarizedDark => fitted!(solarized_dark()),
            Self::CobaltDark => fitted!(cobalt_dark()),
            Self::CobaltLight => fitted!(cobalt_light()),
            Self::GraphiteDark => fitted!(graphite_dark()),
            Self::GraphiteLight => fitted!(graphite_light()),
            Self::SignatureDark => fitted!(signature_dark()),
            Self::SignatureLight => fitted!(signature_light()),
            Self::SepiaLight => fitted!(sepia_light()),
            Self::SepiaDark => fitted!(sepia_dark()),
        }
    }
}

fn amber_dark() -> Palette {
    let mut p = AMBER_DARK;
    p.love = super::love_for(&p, true);
    p
}

fn amber_light() -> Palette {
    let mut p = AMBER_LIGHT;
    p.love = super::love_for(&p, false);
    p
}

#[allow(clippy::too_many_arguments)]
fn paint_surfaces(
    mut p: Palette,
    bg: Hsla,
    rail: Hsla,
    border: Hsla,
    hover: Hsla,
    alt: Hsla,
    text: Hsla,
    dim: Hsla,
    disabled: Hsla,
) -> Palette {
    p.bg = bg;
    p.sidebar_bg = rail;
    p.tab_strip_bg = bg;
    p.panel_bg = rail;
    p.input_bg = rail;
    p.popover_bg = rail;
    p.border = border;
    p.popup_border = border;
    p.row_hover = hover;
    p.row_alt = alt;
    p.text = text;
    p.text_dim = dim;
    p.text_disabled = disabled;
    p.cursor = text;
    p
}

fn select(mut p: Palette, accent: Hsla, dark: bool) -> Palette {
    p.selection = with_alpha(accent, if dark { 0.14 } else { 0.12 });
    p.completion_selected = with_alpha(accent, if dark { 0.22 } else { 0.16 });
    p.love = super::love_for(&p, dark);
    p
}

/// Shared helper: point every accent-derived role at one hue. Keeps the
/// neutral surfaces of the base palette (which already pass WCAG) and only
/// moves the accent family.
fn recolor_accent(mut p: Palette, accent: Hsla) -> Palette {
    p.accent = accent;
    p.underline_accent = accent;
    p.info = accent;
    p.badge_fn = accent;
    p.syntax.keyword = accent;
    p.syntax.kind_keyword = accent;
    p.syntax.kind_function = accent;
    p
}

/// Deepen the green/amber semantic and syntax roles that the base `LIGHT`
/// palette tuned for a pure-white canvas. On a tinted off-white canvas those
/// hues fall a hair under AA, so any light theme that shifts `bg` off #ffffff
/// runs its colored ink through this first.
fn deepen_light_semantics(mut p: Palette) -> Palette {
    let green = hx("#2C6A2F");
    let amber = hx("#8A5A00");
    p.success = green;
    p.warning = amber;
    p.badge_table = green;
    p.badge_view = amber;
    // The completion-kind green stays a green; the editor's string ink is
    // red, so it never collides with the green comments.
    p.syntax.string = hx("#A31515");
    p.syntax.kind_string = green;
    p.syntax.kind_table = green;
    p.syntax.kind_view = amber;
    p.syntax.kind_column = hx("#444400");
    p.syntax.kind_index = hx("#2C5A46");
    p.syntax.kind_snippet = hx("#7A4A00");
    p
}

fn nordic_dark() -> Palette {
    let accent = hx("#88C0D0");
    let mut p = recolor_accent(AMBER_DARK, accent);
    p = paint_surfaces(
        p,
        hx("#2E3440"),
        hx("#353B48"),
        hx("#4C566A"),
        hx("#3B4252"),
        hx("#323845"),
        hx("#ECEFF4"),
        hx("#D8DEE9"),
        hx("#7B88A1"),
    );
    p.error = hx("#F08A94");
    p.syntax.comment = hx("#A3BE8C"); // Nord green — comments stay comments
    p.syntax.kind_procedure = hx("#D8B4E2");
    p.syntax.kind_database = hx("#A3C4E8");
    p.syntax.kind_schema = hx("#D8B4E2");
    p.syntax.kind_default = hx("#D8DEE9");
    p.syntax.kind_trigger = hx("#F0B4C0");
    p.syntax.kind_constraint = hx("#E0C8A8");
    select(p, accent, true)
}

fn nordic_light() -> Palette {
    let accent = hx("#3E5F8A");
    let mut p = recolor_accent(AMBER_LIGHT, accent);
    p = paint_surfaces(
        p,
        hx("#ECEFF4"),
        hx("#E5E9F0"),
        hx("#D8DEE9"),
        hx("#E0E4EC"),
        hx("#E8ECF2"),
        hx("#2E3440"),
        hx("#4C566A"),
        hx("#7B88A1"),
    );
    select(deepen_light_semantics(p), accent, false)
}

/// Dark counterpart to `Lavender Light`: a soft purple-tinted charcoal canvas
/// with a lavender accent.
fn lavender_dark() -> Palette {
    let accent = hx("#C4A7E7");
    let mut p = recolor_accent(AMBER_DARK, accent);
    p = paint_surfaces(
        p,
        hx("#1E1E2E"),
        hx("#252536"),
        hx("#45475A"),
        hx("#2A2B3D"),
        hx("#222233"),
        hx("#D4D4D4"),
        hx("#A0A0A4"),
        hx("#5C5C60"),
    );
    select(p, accent, true)
}

fn tokyo_dark() -> Palette {
    let accent = hx("#7AA2F7");
    let mut p = recolor_accent(AMBER_DARK, accent);
    p = paint_surfaces(
        p,
        hx("#1A1B26"),
        hx("#1F2130"),
        hx("#3B4261"),
        hx("#24283B"),
        hx("#1D1E2A"),
        hx("#D4D4D4"),
        hx("#A0A0A4"),
        hx("#5C5C60"),
    );
    let mut p = select(p, accent, true);
    p.love = hx("#FF7A9C"); // Tokyo Night, shifted pink of the red
    p
}

fn tokyo_light() -> Palette {
    let accent = hx("#2E4EA8");
    let mut p = recolor_accent(AMBER_LIGHT, accent);
    p = paint_surfaces(
        p,
        hx("#EEEFF4"),
        hx("#E6E7EE"),
        hx("#D0D2DC"),
        hx("#E0E1E8"),
        hx("#E8E9F0"),
        hx("#1A1B26"),
        hx("#4A4D62"),
        hx("#7A7D90"),
    );
    let mut p = select(deepen_light_semantics(p), accent, false);
    p.love = hx("#B01E50");
    p
}

fn lavender_light() -> Palette {
    let accent = hx("#1A54C4");
    let mauve = hx("#6D28D9");
    let mut p = LIGHT;
    p.bg = hx("#EFF1F5");
    p.tab_strip_bg = hx("#EFF1F5");
    p.panel_bg = hx("#EFF1F5");
    p.input_bg = hx("#E6E9EF");
    p.popover_bg = hx("#EFF1F5");
    p.sidebar_bg = hx("#E6E9EF");
    p.text_dim = hx("#4C4F69");
    p.border = hx("#CCD0DA");
    p.popup_border = hx("#CCD0DA");
    p.row_hover = hx("#E0E4EC");
    p.row_alt = hx("#E9ECF1");
    p.accent = accent;
    p.underline_accent = accent;
    p.selection = with_alpha(accent, 0.12);
    p.completion_selected = with_alpha(accent, 0.16);
    p.info = accent;
    p.badge_fn = accent;
    p.syntax.keyword = mauve;
    p.syntax.kind_keyword = mauve;
    p.syntax.kind_function = accent;
    p.syntax.kind_procedure = mauve;
    p.love = super::love_for(&p, false);
    // Accent is azure; pull the tagline toward the mauve so lavender stays lilac, not generic raspberry.
    p.love.h = 0.90;
    deepen_light_semantics(p)
}

fn harbor_light() -> Palette {
    let accent = hx("#0969DA");
    let mut p = recolor_accent(AMBER_LIGHT, accent);
    p.sidebar_bg = hx("#F6F8FA");
    p.border = hx("#D0D7DE");
    p.popup_border = hx("#D0D7DE");
    p.row_hover = hx("#EEF2F6");
    p.row_alt = hx("#F6F8FA");
    let mut p = select(p, accent, false);
    p.love = hx("#BF3989"); // GitHub pretty pink
    p
}

fn harbor_dark() -> Palette {
    let accent = hx("#58A6FF");
    let mut p = recolor_accent(AMBER_DARK, accent);
    p = paint_surfaces(
        p,
        hx("#0D1117"),
        hx("#161B22"),
        hx("#30363D"),
        hx("#1C2128"),
        hx("#12171D"),
        hx("#E6EDF3"),
        hx("#8B949E"),
        hx("#6E7681"),
    );
    let mut p = select(p, accent, true);
    p.love = hx("#F778BA"); // GitHub dark pink
    p
}

fn solarized_light() -> Palette {
    let accent = hx("#155E90");
    let mut p = recolor_accent(AMBER_LIGHT, accent);
    p = paint_surfaces(
        p,
        hx("#FDF6E3"),
        hx("#FBF4DE"),
        hx("#E4DCC0"),
        hx("#F6EED6"),
        hx("#F8F1DA"),
        hx("#1E1E1E"),
        hx("#4A5F66"),
        hx("#9A9AA0"),
    );
    p.warning = hx("#8A5A00");
    p.success = hx("#3D5C00");
    p.badge_view = hx("#8A5A00");
    p.badge_table = hx("#3D5C00");
    p.syntax.keyword = hx("#8A5A00");
    p.syntax.kind_keyword = hx("#8A5A00");
    p.syntax.string = hx("#17635C"); // Solarized cyan, deepened for AA
    p.syntax.kind_string = hx("#3D5C00");
    p.syntax.kind_table = hx("#3D5C00");
    p.syntax.kind_view = hx("#8A5A00");
    p.syntax.kind_column = hx("#5A4A00");
    p.syntax.kind_function = accent;
    p.syntax.kind_index = hx("#1F4A3A");
    let mut p = select(p, accent, false);
    p.love = hx("#A51D62"); // Solarized magenta, deepened for AA
    p
}

fn solarized_dark() -> Palette {
    let accent = hx("#4BA3D9");
    let mut p = recolor_accent(AMBER_DARK, accent);
    p = paint_surfaces(
        p,
        hx("#002B36"),
        hx("#073642"),
        hx("#586E75"),
        hx("#0A3A46"),
        hx("#00303C"),
        hx("#FDF6E3"),
        hx("#A7B6B6"),
        hx("#657B83"),
    );
    p.success = hx("#C4D15A");
    p.warning = hx("#E0B84A");
    p.badge_table = hx("#C4D15A");
    p.badge_view = hx("#E0B84A");
    p.error = hx("#FF8A82");
    p.syntax.keyword = hx("#E0B84A");
    p.syntax.kind_keyword = hx("#E0B84A");
    p.syntax.string = hx("#6FCFC4"); // Solarized cyan, lifted for AA
    p.syntax.kind_string = hx("#C4D15A");
    p.syntax.kind_function = accent;
    p.syntax.comment = hx("#A3B84F"); // Solarized green — comments stay green
    p.syntax.kind_procedure = hx("#D8A0E0");
    p.syntax.kind_trigger = hx("#F0A0B0");
    let mut p = select(p, accent, true);
    p.love = hx("#F078A8"); // Solarized magenta, lifted for AA on the dark cyan canvas
    p
}

/// Cobalt Dark — charcoal-blue (not brand navy) with a saturated cobalt accent.
fn cobalt_dark() -> Palette {
    let accent = hx("#4C8DFF");
    let mut p = recolor_accent(AMBER_DARK, accent);
    p = paint_surfaces(
        p,
        hx("#121826"),
        hx("#181F30"),
        hx("#2A3A58"),
        hx("#1C2638"),
        hx("#151B28"),
        hx("#E8EEF8"),
        hx("#9AABC4"),
        hx("#5A6A84"),
    );
    select(p, accent, true)
}

/// Cobalt Light — cool gray-blue paper, not the icy brand canvas.
fn cobalt_light() -> Palette {
    let accent = hx("#1557C0");
    let mut p = recolor_accent(AMBER_LIGHT, accent);
    p = paint_surfaces(
        p,
        hx("#E8EEF6"),
        hx("#DDE5F0"),
        hx("#C5D0E0"),
        hx("#D5DEEB"),
        hx("#E0E7F1"),
        hx("#102038"),
        hx("#3A4F6A"),
        hx("#7A8BA0"),
    );
    select(deepen_light_semantics(p), accent, false)
}

/// Graphite Dark — chroma-free charcoal with a restrained steel accent.
fn graphite_dark() -> Palette {
    let accent = hx("#8AA0B4");
    let mut p = recolor_accent(AMBER_DARK, accent);
    p = paint_surfaces(
        p,
        hx("#1C1C1C"),
        hx("#252525"),
        hx("#3A3A3A"),
        hx("#2A2A2A"),
        hx("#202020"),
        hx("#D4D4D4"),
        hx("#A0A0A4"),
        hx("#5C5C60"),
    );
    select(p, accent, true)
}

/// Graphite Light — true white/neutral gray, no blue tint.
fn graphite_light() -> Palette {
    let accent = hx("#3D5570");
    let mut p = recolor_accent(AMBER_LIGHT, accent);
    p.sidebar_bg = hx("#F4F4F5");
    p.row_hover = hx("#ECECEE");
    p.row_alt = hx("#F4F4F5");
    p.border = hx("#E2E2E4");
    p.popup_border = hx("#E2E2E4");
    select(p, accent, false)
}

/// Signature Dark — teal-anchored neutrals with a faint teal cast.
fn signature_dark() -> Palette {
    let accent = hx("#3FC0CB");
    let mut p = recolor_accent(AMBER_DARK, accent);
    p = paint_surfaces(
        p,
        hx("#181D1D"),
        hx("#212828"),
        hx("#2E3838"),
        hx("#1F2626"),
        hx("#1B2121"),
        hx("#D4D4D4"),
        hx("#A0A0A4"),
        hx("#5C5C60"),
    );
    p.input_bg = hx("#1F2626");
    select(p, accent, true)
}

/// Signature Light — soft teal-tinted paper with a dark teal accent.
fn signature_light() -> Palette {
    let accent = hx("#0F6E7A");
    let mut p = recolor_accent(AMBER_LIGHT, accent);
    p = paint_surfaces(
        p,
        hx("#F7FAFA"),
        hx("#EDF3F3"),
        hx("#D3E0E0"),
        hx("#E7EFEF"),
        hx("#EFF5F5"),
        hx("#1E1E1E"),
        hx("#6B6B70"),
        hx("#9A9AA0"),
    );
    p.input_bg = hx("#ECF2F2");
    select(deepen_light_semantics(p), accent, false)
}

/// Sepia Light — pale ivory paper (Light Soft's lightness, slightly warmer),
/// walnut ink, burnt-sienna accent. Lighter than Solarized Light.
fn sepia_light() -> Palette {
    let accent = hx("#8A3F18");
    let mut p = recolor_accent(AMBER_LIGHT, accent);
    p = paint_surfaces(
        p,
        hx("#FCFAF4"),
        hx("#F5F2EA"),
        hx("#E4DFD2"),
        hx("#F1EDE3"),
        hx("#F7F4EC"),
        hx("#3A2414"),
        hx("#5C4E40"),
        hx("#9A8C7A"),
    );
    p.input_bg = hx("#F4F1E8");
    p.syntax.kind_procedure = accent;
    let mut p = deepen_light_semantics(p);
    // Berry, not the usual brick red: brick sits too close to the copper
    // keywords for the two to read apart.
    p.syntax.string = hx("#9C1F5A");
    select(p, accent, false)
}

/// Sepia Dark — darkroom umber, warm ivory ink, copper-sienna accent.
fn sepia_dark() -> Palette {
    let accent = hx("#D4925A");
    let mut p = recolor_accent(AMBER_DARK, accent);
    p = paint_surfaces(
        p,
        hx("#1A120C"),
        hx("#261A12"),
        hx("#4A3528"),
        hx("#2C1E14"),
        hx("#1E1610"),
        hx("#EDE0C8"),
        hx("#B89A78"),
        hx("#7A624C"),
    );
    p.syntax.comment = hx("#9DAE6A"); // sage — green, but at home on umber
    // Rose, not salmon: salmon sits too close to the copper keywords.
    p.syntax.string = hx("#F08CB4");
    select(p, accent, true)
}

#[cfg(test)]
mod tests {
    use super::AMBER_DARK;
    use super::ColorTheme;

    #[test]
    fn light_and_dark_galleries_partition_all_themes() {
        let mut seen = Vec::new();
        seen.extend(ColorTheme::LIGHT);
        seen.extend(ColorTheme::DARK);
        seen.sort_by_key(|t| t.to_u8());
        let mut all = ColorTheme::ALL;
        all.sort_by_key(|t| t.to_u8());
        assert_eq!(seen, all);
        assert_eq!(ColorTheme::LIGHT.len(), 12);
        assert_eq!(ColorTheme::DARK.len(), 12);
        assert!(ColorTheme::LIGHT.iter().all(|t| !t.is_dark()));
        assert!(ColorTheme::DARK.iter().all(|t| t.is_dark()));
    }

    #[test]
    fn brand_leads_each_gallery() {
        assert_eq!(ColorTheme::DARK[0], ColorTheme::Dark);
        assert!(ColorTheme::Dark.is_dark());
        assert_eq!(ColorTheme::Dark.label(), "Dark");
        assert_eq!(ColorTheme::LIGHT[0], ColorTheme::Light);
        assert!(!ColorTheme::Light.is_dark());
        assert_eq!(ColorTheme::Light.label(), "Light");
    }

    #[test]
    fn galleries_list_matching_families() {
        for (light, dark) in ColorTheme::LIGHT.iter().zip(ColorTheme::DARK.iter()) {
            assert_eq!(light.counterpart(), *dark, "{}", light.label());
            assert_eq!(dark.counterpart(), *light, "{}", dark.label());
        }
    }

    #[test]
    fn counterpart_is_a_bijection() {
        for t in ColorTheme::ALL {
            assert_eq!(t.counterpart().counterpart(), t, "{}", t.label());
            assert_ne!(t.counterpart().is_dark(), t.is_dark(), "{}", t.label());
        }
    }

    #[test]
    fn labels_follow_family_light_dark() {
        assert_eq!(ColorTheme::CobaltDark.label(), "Cobalt Dark");
        assert_eq!(ColorTheme::GraphiteLight.label(), "Graphite Light");
        assert_eq!(ColorTheme::NordicDark.label(), "Nordic Dark");
        assert_eq!(ColorTheme::TokyoDark.label(), "Tokyo Dark");
        assert_eq!(ColorTheme::TokyoLight.label(), "Tokyo Light");
        assert_eq!(ColorTheme::SepiaLight.label(), "Sepia Light");
        assert_eq!(ColorTheme::SepiaDark.label(), "Sepia Dark");
        assert_eq!(ColorTheme::HarborLight.label(), "Harbor Light");
        assert_eq!(ColorTheme::HarborDark.label(), "Harbor Dark");
        assert_eq!(ColorTheme::HighContrastDark.label(), "High Contrast Dark");
        assert_eq!(ColorTheme::HighContrastLight.label(), "High Contrast Light");
        assert_eq!(ColorTheme::SolarizedDark.label(), "Solarized Dark");
    }

    #[test]
    fn u8_round_trip() {
        for t in ColorTheme::ALL {
            assert_eq!(ColorTheme::from_u8(t.to_u8()), t);
        }
        assert_eq!(ColorTheme::from_u8(255), ColorTheme::Light);
    }

    #[test]
    fn sanitize_rejects_cross_gallery_ids() {
        assert_eq!(ColorTheme::Dark.sanitize_light(), ColorTheme::Light);
        assert_eq!(ColorTheme::Light.sanitize_dark(), ColorTheme::Dark);
        assert_eq!(
            ColorTheme::NordicDark.sanitize_dark(),
            ColorTheme::NordicDark
        );
        assert_eq!(
            ColorTheme::LavenderLight.sanitize_light(),
            ColorTheme::LavenderLight
        );
    }

    #[test]
    fn serde_round_trip_uses_camel_case() {
        let json = serde_json::to_string(&ColorTheme::LavenderDark).unwrap();
        assert_eq!(json, "\"lavenderDark\"");
        let back: ColorTheme = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ColorTheme::LavenderDark);
        let dark: ColorTheme = serde_json::from_str("\"dark\"").unwrap();
        assert_eq!(dark, ColorTheme::Dark);
        let hc: ColorTheme = serde_json::from_str("\"highContrastDark\"").unwrap();
        assert_eq!(hc, ColorTheme::HighContrastDark);
    }

    #[test]
    fn retired_names_do_not_deserialize() {
        for old in [
            "nord",
            "tokyoNight",
            "githubLight",
            "lightSoft",
            "classicDark",
            "neutralDark",
            "highContrast",
        ] {
            let json = format!("\"{old}\"");
            assert!(
                serde_json::from_str::<ColorTheme>(&json).is_err(),
                "{old} should not deserialize"
            );
        }
    }

    #[test]
    fn families_are_visually_distinct_from_brand_and_amber() {
        let brand_dark = ColorTheme::Dark.palette().bg;
        let brand_light = ColorTheme::Light.palette().bg;
        assert_ne!(ColorTheme::CobaltDark.palette().bg, brand_dark);
        assert_ne!(ColorTheme::GraphiteDark.palette().bg, brand_dark);
        assert_ne!(ColorTheme::GraphiteDark.palette().bg, AMBER_DARK.bg);
        assert_ne!(ColorTheme::HarborDark.palette().bg, brand_dark);
        assert_ne!(ColorTheme::NordicLight.palette().bg, brand_light);
        assert_ne!(ColorTheme::CobaltLight.palette().bg, brand_light);
        assert_ne!(ColorTheme::TokyoLight.palette().bg, brand_light);
    }

    #[test]
    fn love_ink_is_a_theme_toned_rose_not_accent_or_error() {
        for theme in ColorTheme::ALL {
            let p = theme.palette();
            let hue = p.love.h;
            assert!(
                !(0.08..=0.85).contains(&hue),
                "{} love must be pink-red (h={hue})",
                theme.label()
            );
            assert_ne!(
                (p.love.h, p.love.s, p.love.l),
                (p.accent.h, p.accent.s, p.accent.l),
                "{} love must not reuse the accent",
                theme.label()
            );
            let hue_delta = (p.love.h - p.error.h).abs();
            let hue_delta = hue_delta.min(1.0 - hue_delta);
            assert!(
                hue_delta > 0.04 || (p.love.s - p.error.s).abs() > 0.15,
                "{} love (h={:.3} s={:.2}) must not collapse into error (h={:.3} s={:.2})",
                theme.label(),
                p.love.h,
                p.love.s,
                p.error.h,
                p.error.s
            );
        }
    }

    #[test]
    fn sepia_reads_as_brown_not_gray_or_slate_blue() {
        let dark = ColorTheme::SepiaDark.palette();
        assert!(
            dark.bg.s > 0.15,
            "Sepia Dark canvas must be brown, not gray (s={})",
            dark.bg.s
        );
        assert!(dark.bg.l < 0.2, "Sepia Dark canvas must stay dark");
        let light = ColorTheme::SepiaLight.palette();
        assert!(
            light.accent.s > 0.4,
            "Sepia Light accent must be saturated sienna"
        );
        assert!(
            light.accent.h < 0.12 || light.accent.h > 0.95,
            "Sepia Light accent must be sienna/brown, not slate-blue (h={})",
            light.accent.h
        );
        let solarized = ColorTheme::SolarizedLight.palette();
        let sepia_lum = super::super::relative_luminance(light.bg);
        let solarized_lum = super::super::relative_luminance(solarized.bg);
        assert!(
            sepia_lum > solarized_lum,
            "Sepia Light canvas (Y={sepia_lum:.3}) must be lighter than Solarized Light (Y={solarized_lum:.3})"
        );
    }
}
