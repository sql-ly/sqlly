//! Rail and panel sizes for notebook tabs, persisted with the workspace
//! layout so every notebook opens the way the user last arranged one.

use serde::{Deserialize, Serialize};

pub const DEFAULT_LEFT_WIDTH: f32 = 240.0;
pub const DEFAULT_RIGHT_WIDTH: f32 = 280.0;
pub const DEFAULT_ACTIVITY_HEIGHT: f32 = 160.0;
pub const MIN_RAIL_WIDTH: f32 = 160.0;
pub const MAX_RAIL_WIDTH: f32 = 640.0;
pub const MIN_ACTIVITY_HEIGHT: f32 = 80.0;
pub const MAX_ACTIVITY_HEIGHT: f32 = 600.0;

/// Which page the left rail shows. Persisted by its serde name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeftRailPage {
    #[default]
    Outline,
    Parameters,
    Variables,
    Lineage,
    Search,
    LocalData,
    History,
    Bookmarks,
}

impl LeftRailPage {
    pub const ALL: [Self; 8] = [
        Self::Outline,
        Self::Parameters,
        Self::Variables,
        Self::Lineage,
        Self::Search,
        Self::LocalData,
        Self::History,
        Self::Bookmarks,
    ];

    pub fn label_key(self) -> &'static str {
        match self {
            Self::Outline => "notebook.shell.rail.outline_tab",
            Self::Parameters => "notebook.shell.rail.parameters_tab",
            Self::Variables => "notebook.shell.rail.variables_tab",
            Self::Lineage => "notebook.shell.rail.lineage_tab",
            Self::Search => "notebook.shell.rail.search_tab",
            Self::LocalData => "notebook.local.data.tab",
            Self::History => "notebook.analysis.history.tab",
            Self::Bookmarks => "notebook.editing.bookmarks.tab",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotebookLayout {
    pub left_visible: bool,
    pub left_width: f32,
    pub left_page: LeftRailPage,
    pub right_visible: bool,
    pub right_width: f32,
    pub activity_visible: bool,
    pub activity_height: f32,
}

impl Default for NotebookLayout {
    fn default() -> Self {
        Self {
            left_visible: true,
            left_width: DEFAULT_LEFT_WIDTH,
            left_page: LeftRailPage::Outline,
            right_visible: true,
            right_width: DEFAULT_RIGHT_WIDTH,
            activity_visible: false,
            activity_height: DEFAULT_ACTIVITY_HEIGHT,
        }
    }
}

impl NotebookLayout {
    /// Clamp sizes a hand-edited or damaged session could carry.
    pub fn sanitized(mut self) -> Self {
        let rail = |w: f32, default: f32| {
            if w.is_finite() && w > 0.0 {
                w.clamp(MIN_RAIL_WIDTH, MAX_RAIL_WIDTH)
            } else {
                default
            }
        };
        self.left_width = rail(self.left_width, DEFAULT_LEFT_WIDTH);
        self.right_width = rail(self.right_width, DEFAULT_RIGHT_WIDTH);
        self.activity_height = if self.activity_height.is_finite() && self.activity_height > 0.0 {
            self.activity_height
                .clamp(MIN_ACTIVITY_HEIGHT, MAX_ACTIVITY_HEIGHT)
        } else {
            DEFAULT_ACTIVITY_HEIGHT
        };
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitized_clamps_and_repairs_sizes() {
        let layout = NotebookLayout {
            left_width: 5.0,
            right_width: f32::NAN,
            activity_height: 10_000.0,
            ..NotebookLayout::default()
        }
        .sanitized();
        assert_eq!(layout.left_width, MIN_RAIL_WIDTH);
        assert_eq!(layout.right_width, DEFAULT_RIGHT_WIDTH);
        assert_eq!(layout.activity_height, MAX_ACTIVITY_HEIGHT);
    }

    #[test]
    fn layout_round_trips_and_tolerates_missing_fields() {
        let layout = NotebookLayout {
            left_page: LeftRailPage::Lineage,
            activity_visible: true,
            ..NotebookLayout::default()
        };
        let json = serde_json::to_string(&layout).unwrap();
        assert_eq!(
            serde_json::from_str::<NotebookLayout>(&json).unwrap(),
            layout
        );
        let partial: NotebookLayout = serde_json::from_str(r#"{"left_visible":false}"#).unwrap();
        assert!(!partial.left_visible);
        assert_eq!(partial.right_width, DEFAULT_RIGHT_WIDTH);
    }
}
