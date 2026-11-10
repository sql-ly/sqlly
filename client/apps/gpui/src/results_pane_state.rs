//! Results pane layout state. Pure; the host stores one value and renders from it.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResultsPaneState {
    Hidden,
    #[default]
    Split,
    /// Editor collapsed to its band; the results take the whole pane.
    Maximized,
}

impl ResultsPaneState {
    pub fn is_visible(self) -> bool {
        !matches!(self, Self::Hidden)
    }

    /// Legacy toggle (`ToggleResultsPane`): Hidden ⇄ last visible state (Split when unknown); Maximized → Hidden.
    pub fn toggled(self) -> Self {
        match self {
            Self::Hidden => Self::Split,
            _ => Self::Hidden,
        }
    }

    /// Maximize/restore: Split → Maximized, Maximized → Split, Hidden → Maximized.
    pub fn toggled_maximized(self) -> Self {
        match self {
            Self::Maximized => Self::Split,
            _ => Self::Maximized,
        }
    }

    /// "Something needs the results on screen" (output-tab selection, explain, opened snapshot): Hidden → Split, otherwise unchanged.
    pub fn shown(self) -> Self {
        if self == Self::Hidden {
            Self::Split
        } else {
            self
        }
    }

    /// Backward-compatible session decode from the old bool.
    pub fn from_legacy_visible(visible: bool) -> Self {
        if visible { Self::Split } else { Self::Hidden }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggled_cycles_hidden_and_split() {
        assert_eq!(ResultsPaneState::Hidden.toggled(), ResultsPaneState::Split);
        assert_eq!(ResultsPaneState::Split.toggled(), ResultsPaneState::Hidden);
    }

    #[test]
    fn toggled_from_maximized_hides() {
        assert_eq!(
            ResultsPaneState::Maximized.toggled(),
            ResultsPaneState::Hidden
        );
    }

    #[test]
    fn toggled_maximized_round_trips() {
        assert_eq!(
            ResultsPaneState::Split.toggled_maximized(),
            ResultsPaneState::Maximized
        );
        assert_eq!(
            ResultsPaneState::Maximized.toggled_maximized(),
            ResultsPaneState::Split
        );
        assert_eq!(
            ResultsPaneState::Hidden.toggled_maximized(),
            ResultsPaneState::Maximized
        );
    }

    #[test]
    fn shown_only_lifts_hidden() {
        assert_eq!(ResultsPaneState::Hidden.shown(), ResultsPaneState::Split);
        assert_eq!(ResultsPaneState::Split.shown(), ResultsPaneState::Split);
        assert_eq!(
            ResultsPaneState::Maximized.shown(),
            ResultsPaneState::Maximized
        );
    }

    #[test]
    fn legacy_bool_maps() {
        assert_eq!(
            ResultsPaneState::from_legacy_visible(true),
            ResultsPaneState::Split
        );
        assert_eq!(
            ResultsPaneState::from_legacy_visible(false),
            ResultsPaneState::Hidden
        );
    }

    #[test]
    fn serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&ResultsPaneState::Maximized).unwrap(),
            "\"maximized\""
        );
        assert_eq!(
            serde_json::from_str::<ResultsPaneState>("\"split\"").unwrap(),
            ResultsPaneState::Split
        );
        assert_eq!(
            serde_json::from_str::<ResultsPaneState>("\"hidden\"").unwrap(),
            ResultsPaneState::Hidden
        );
    }
}
