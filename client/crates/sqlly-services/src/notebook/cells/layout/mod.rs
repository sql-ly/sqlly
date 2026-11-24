//! Dashboard (`layout`) cells (spec 8.3): a responsive 12-column canvas of
//! sections and tabs whose tiles show other cells' outputs by id.
//!
//! The whole dashboard lives in the layout cell's `spec`, so it round-trips
//! through `.sqllynb` like any other cell and a notebook can hold several.
//! Every object keeps the fields it does not know in `extra`, and a
//! visibility rule this build does not understand is kept as written, so a
//! newer SQLLY's settings survive a save here.
//!
//! ```json
//! {"title": "Revenue",
//!  "filters": {"params": ["region"]},
//!  "sections": [{"id": "s1", "title": "Overview", "tabs": [{"id": "tab1", "tiles": [
//!     {"id": "t1", "cell": "c_kpi", "span": {"wide": 4, "compact": 12}, "height": 2},
//!     {"id": "t2", "cell": "c_by_region", "title": "By region", "span": 8,
//!      "visible_when": {"kind": "param_has_value", "param": "region"},
//!      "drill": {"section": "s2", "param": "region", "column": "region"}}]}]}],
//!  "presentation": {"slides": "per_section"},
//!  "theme": "follow_app"}
//! ```
//!
//! Titles the user left empty stay empty in the file; the app shows a
//! localized "Section 1" style label instead, so no translated text is ever
//! saved.

pub mod report;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sqlly_core::DisplayMessage;

use crate::notebook::{Cell, CellKind, ExecClass, Graph, Node, NotebookDoc};
use crate::report::text::{MAX_IMAGE_BYTES, png_is_acceptable, svg_is_unsafe};

/// Grid columns a row holds.
pub const GRID_COLUMNS: u8 = 12;
pub const DEFAULT_WIDE_SPAN: u8 = 6;
pub const DEFAULT_COMPACT_SPAN: u8 = 12;
/// Tile heights are in rows of [`ROW_HEIGHT`] pixels.
pub const MIN_HEIGHT: u8 = 1;
pub const MAX_HEIGHT: u8 = 8;
pub const DEFAULT_HEIGHT: u8 = 3;
pub const ROW_HEIGHT: f32 = 110.0;
/// Canvases narrower than this use each tile's compact span.
pub const COMPACT_BELOW: f32 = 720.0;
/// Largest logo kept in a notebook.
pub const MAX_LOGO_BYTES: usize = 256 * 1024;

type Extra = BTreeMap<String, Value>;

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

fn default_height() -> u8 {
    DEFAULT_HEIGHT
}

/// The dashboard stored in a layout cell's `spec`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LayoutSpec {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default)]
    pub sections: Vec<Section>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub filters: FilterBar,
    #[serde(default, skip_serializing_if = "is_default")]
    pub theme: ThemeChoice,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo: Option<Logo>,
    #[serde(default, skip_serializing_if = "is_default")]
    pub presentation: Presentation,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Section {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default)]
    pub tabs: Vec<Tab>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Tab {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default)]
    pub tiles: Vec<Tile>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tile {
    pub id: String,
    /// The id of the cell whose output the tile shows.
    pub cell: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A note shown under the tile's output (Markdown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
    #[serde(default)]
    pub span: Spans,
    #[serde(default = "default_height")]
    pub height: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visible_when: Option<Condition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drill: Option<Drill>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl Tile {
    pub fn new(id: impl Into<String>, cell: impl Into<String>) -> Tile {
        Tile {
            id: id.into(),
            cell: cell.into(),
            title: None,
            annotation: None,
            span: Spans::default(),
            height: DEFAULT_HEIGHT,
            visible_when: None,
            drill: None,
            extra: Extra::new(),
        }
    }

    pub fn span(&self, breakpoint: Breakpoint) -> u8 {
        match breakpoint {
            Breakpoint::Wide => self.span.wide,
            Breakpoint::Compact => self.span.compact,
        }
        .clamp(1, GRID_COLUMNS)
    }

    pub fn height(&self) -> u8 {
        self.height.clamp(MIN_HEIGHT, MAX_HEIGHT)
    }

    /// Whether the tile shows, given the current parameter values (as
    /// text, empty or `None` meaning unset) and check outcomes. A condition
    /// this build does not understand, or a check that has not run, hides
    /// the tile.
    pub fn is_visible(&self, inputs: &VisibilityInputs<'_>) -> bool {
        match &self.visible_when {
            None => true,
            Some(Condition::Known(rule)) => rule.holds(inputs),
            Some(Condition::Other(_)) => false,
        }
    }
}

/// A tile's width in grid columns on wide and compact canvases. `"span": 4`
/// is read as `{"wide": 4, "compact": 12}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Spans {
    pub wide: u8,
    pub compact: u8,
}

impl Default for Spans {
    fn default() -> Self {
        Spans {
            wide: DEFAULT_WIDE_SPAN,
            compact: DEFAULT_COMPACT_SPAN,
        }
    }
}

impl<'de> Deserialize<'de> for Spans {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            One(u8),
            Both {
                #[serde(default = "wide")]
                wide: u8,
                #[serde(default = "compact")]
                compact: u8,
            },
        }
        fn wide() -> u8 {
            DEFAULT_WIDE_SPAN
        }
        fn compact() -> u8 {
            DEFAULT_COMPACT_SPAN
        }
        Ok(match Repr::deserialize(d)? {
            Repr::One(wide) => Spans {
                wide,
                compact: DEFAULT_COMPACT_SPAN,
            },
            Repr::Both { wide, compact } => Spans { wide, compact },
        })
    }
}

/// A visibility rule, or one written by a newer SQLLY (kept as is).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Condition {
    Known(Visibility),
    Other(Value),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Visibility {
    /// The parameter has a non-empty value.
    ParamHasValue { param: String },
    /// The parameter's value reads exactly as `value`.
    ParamEquals { param: String, value: String },
    /// The check (assertion) cell's last run passed.
    TestPasses { cell: String },
}

impl Visibility {
    fn holds(&self, inputs: &VisibilityInputs<'_>) -> bool {
        match self {
            Visibility::ParamHasValue { param } => {
                (inputs.param)(param).is_some_and(|v| !v.trim().is_empty())
            }
            Visibility::ParamEquals { param, value } => {
                (inputs.param)(param).is_some_and(|v| v == *value)
            }
            Visibility::TestPasses { cell } => (inputs.test_passed)(cell) == Some(true),
        }
    }

    pub fn param(&self) -> Option<&str> {
        match self {
            Visibility::ParamHasValue { param } | Visibility::ParamEquals { param, .. } => {
                Some(param)
            }
            Visibility::TestPasses { .. } => None,
        }
    }
}

/// What a visibility rule reads: parameter values as text and check
/// outcomes (`None` when the check has not run).
pub struct VisibilityInputs<'a> {
    pub param: &'a dyn Fn(&str) -> Option<String>,
    pub test_passed: &'a dyn Fn(&str) -> Option<bool>,
}

/// A drill-through link: open another section (and tab), optionally
/// setting a parameter to a fixed value or to a value picked from the
/// tile's `column`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Drill {
    pub section: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// The parameters the filter bar shows, in order.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FilterBar {
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    #[default]
    FollowApp,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlideMode {
    /// One slide per section tab.
    #[default]
    PerSection,
    /// The whole dashboard on one scrolling slide.
    Whole,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Presentation {
    #[serde(default)]
    pub slides: SlideMode,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogoFormat {
    Png,
    Svg,
}

/// A user-picked logo, checked when it is added and again before use: a
/// well-formed PNG, or SVG markup without scripts, frames, event handlers
/// or outside references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Logo {
    pub format: LogoFormat,
    /// Base64 for PNG, the markup for SVG.
    pub data: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub alt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogoError {
    TooLarge,
    Unsupported,
    Unsafe,
}

impl LogoError {
    pub fn message(self) -> DisplayMessage {
        match self {
            LogoError::TooLarge => DisplayMessage::new(
                "notebook.dashboard.logo.too_large",
                "The logo is larger than 256 KB.",
            ),
            LogoError::Unsupported => DisplayMessage::new(
                "notebook.dashboard.logo.unsupported",
                "Choose a PNG or SVG image for the logo.",
            ),
            LogoError::Unsafe => DisplayMessage::new(
                "notebook.dashboard.logo.unsafe",
                "The logo contains scripts or outside links and was not added.",
            ),
        }
    }
}

impl Logo {
    /// A logo from a picked file's bytes; `extension` decides the format.
    pub fn from_file(extension: &str, bytes: &[u8], alt: &str) -> Result<Logo, LogoError> {
        use base64::Engine as _;
        if bytes.len() > MAX_LOGO_BYTES {
            return Err(LogoError::TooLarge);
        }
        match extension.to_ascii_lowercase().as_str() {
            "png" => {
                if !png_is_acceptable(bytes) {
                    return Err(LogoError::Unsupported);
                }
                Ok(Logo {
                    format: LogoFormat::Png,
                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                    alt: alt.to_string(),
                })
            }
            "svg" => {
                let markup = std::str::from_utf8(bytes).map_err(|_| LogoError::Unsupported)?;
                if !markup.to_ascii_lowercase().contains("<svg") {
                    return Err(LogoError::Unsupported);
                }
                if svg_is_unsafe(markup) {
                    return Err(LogoError::Unsafe);
                }
                Ok(Logo {
                    format: LogoFormat::Svg,
                    data: markup.to_string(),
                    alt: alt.to_string(),
                })
            }
            _ => Err(LogoError::Unsupported),
        }
    }

    /// The image bytes, when the stored logo still passes the checks (a
    /// hand-edited file is checked again).
    pub fn bytes(&self) -> Option<Vec<u8>> {
        use base64::Engine as _;
        match self.format {
            LogoFormat::Png => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(self.data.trim())
                    .ok()?;
                (bytes.len() <= MAX_LOGO_BYTES && png_is_acceptable(&bytes)).then_some(bytes)
            }
            LogoFormat::Svg => (self.data.len() <= MAX_LOGO_BYTES.min(MAX_IMAGE_BYTES)
                && !svg_is_unsafe(&self.data))
            .then(|| self.data.as_bytes().to_vec()),
        }
    }
}

/// Which span a canvas uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breakpoint {
    Wide,
    Compact,
}

impl Breakpoint {
    pub fn for_width(width: f32) -> Breakpoint {
        if width < COMPACT_BELOW {
            Breakpoint::Compact
        } else {
            Breakpoint::Wide
        }
    }
}

/// Where a tile sits: section, tab and tile indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TilePos {
    pub section: usize,
    pub tab: usize,
    pub tile: usize,
}

/// A section tab shown as one slide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlideRef {
    pub section: usize,
    pub tab: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutReadError {
    /// The spec is not a dashboard this build can read; it is kept as is.
    Invalid { detail: String },
}

impl LayoutReadError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            LayoutReadError::Invalid { detail } => DisplayMessage::new(
                "notebook.dashboard.issue.unreadable",
                format!("This dashboard could not be read and is kept unchanged: {detail}"),
            )
            .with_arg("detail", detail.clone()),
        }
    }
}

/// A problem in a dashboard, shown in the editor. None of them stop the
/// dashboard from opening.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum LayoutIssue {
    UnknownCell { tile: String, cell: String },
    TileShowsDashboard { tile: String },
    SpanOutOfRange { tile: String, span: u8 },
    HeightOutOfRange { tile: String, height: u8 },
    DuplicateId { id: String },
    EmptyId,
    SectionWithoutTabs { section: String },
    UnknownParameter { param: String },
    UnknownDrillTarget { tile: String, section: String },
    UnknownCheck { tile: String, cell: String },
    UnsupportedCondition { tile: String },
}

impl LayoutIssue {
    pub fn issue_key(&self) -> &'static str {
        match self {
            LayoutIssue::UnknownCell { .. } => "notebook.dashboard.issue.unknown_cell",
            LayoutIssue::TileShowsDashboard { .. } => {
                "notebook.dashboard.issue.tile_shows_dashboard"
            }
            LayoutIssue::SpanOutOfRange { .. } => "notebook.dashboard.issue.span_out_of_range",
            LayoutIssue::HeightOutOfRange { .. } => "notebook.dashboard.issue.height_out_of_range",
            LayoutIssue::DuplicateId { .. } => "notebook.dashboard.issue.duplicate_id",
            LayoutIssue::EmptyId => "notebook.dashboard.issue.empty_id",
            LayoutIssue::SectionWithoutTabs { .. } => {
                "notebook.dashboard.issue.section_without_tabs"
            }
            LayoutIssue::UnknownParameter { .. } => "notebook.dashboard.issue.unknown_parameter",
            LayoutIssue::UnknownDrillTarget { .. } => {
                "notebook.dashboard.issue.unknown_drill_target"
            }
            LayoutIssue::UnknownCheck { .. } => "notebook.dashboard.issue.unknown_check",
            LayoutIssue::UnsupportedCondition { .. } => {
                "notebook.dashboard.issue.unsupported_condition"
            }
        }
    }

    pub fn message(&self) -> DisplayMessage {
        let key = self.issue_key();
        let (text, args): (String, Vec<(&str, String)>) = match self {
            LayoutIssue::UnknownCell { tile, cell } => (
                format!("Tile {tile} shows cell {cell}, which is not in this notebook."),
                vec![("tile", tile.clone()), ("cell", cell.clone())],
            ),
            LayoutIssue::TileShowsDashboard { tile } => (
                format!("Tile {tile} shows a dashboard; tiles can show any other cell."),
                vec![("tile", tile.clone())],
            ),
            LayoutIssue::SpanOutOfRange { tile, span } => (
                format!("Tile {tile} is {span} columns wide; widths go from 1 to 12."),
                vec![("tile", tile.clone()), ("span", span.to_string())],
            ),
            LayoutIssue::HeightOutOfRange { tile, height } => (
                format!("Tile {tile} is {height} rows tall; heights go from 1 to 8."),
                vec![("tile", tile.clone()), ("height", height.to_string())],
            ),
            LayoutIssue::DuplicateId { id } => (
                format!("The id {id} is used more than once in this dashboard."),
                vec![("id", id.clone())],
            ),
            LayoutIssue::EmptyId => (
                "A section, tab or tile in this dashboard has no id.".to_string(),
                vec![],
            ),
            LayoutIssue::SectionWithoutTabs { section } => (
                format!("Section {section} has no tabs, so nothing in it can show."),
                vec![("section", section.clone())],
            ),
            LayoutIssue::UnknownParameter { param } => (
                format!(
                    "The dashboard uses the parameter {param}, which this notebook does not declare."
                ),
                vec![("param", param.clone())],
            ),
            LayoutIssue::UnknownDrillTarget { tile, section } => (
                format!(
                    "Tile {tile} drills through to {section}, which is not a section or tab of this dashboard."
                ),
                vec![("tile", tile.clone()), ("section", section.clone())],
            ),
            LayoutIssue::UnknownCheck { tile, cell } => (
                format!(
                    "Tile {tile} shows only when check {cell} passes, but that cell is not a check in this notebook."
                ),
                vec![("tile", tile.clone()), ("cell", cell.clone())],
            ),
            LayoutIssue::UnsupportedCondition { tile } => (
                format!(
                    "Tile {tile} has a visibility rule this version cannot evaluate, so it stays hidden."
                ),
                vec![("tile", tile.clone())],
            ),
        };
        args.into_iter()
            .fold(DisplayMessage::new(key, text), |m, (name, value)| {
                m.with_arg(name, value)
            })
    }
}

/// Whether a cell of `kind` can be shown on a tile: anything that has an
/// output or text, except dashboards themselves and kinds that never show
/// anything.
pub fn can_tile(kind: &CellKind) -> bool {
    !matches!(
        kind,
        CellKind::Layout
            | CellKind::Parameter
            | CellKind::Export
            | CellKind::Python
            | CellKind::Unknown(_)
    )
}

/// The cells the Add tile picker offers, in document order.
pub fn tileable_cells(doc: &NotebookDoc) -> impl Iterator<Item = &Cell> {
    doc.cells.iter().filter(|c| can_tile(&c.kind))
}

/// The smallest `{prefix}{n}` not in `taken`.
fn next_id(prefix: &str, taken: &BTreeSet<String>) -> String {
    (1..)
        .map(|n| format!("{prefix}{n}"))
        .find(|id| !taken.contains(id))
        .expect("an unused id exists")
}

impl LayoutSpec {
    /// The spec in a layout cell's `spec`. `null` (a new cell) is an empty
    /// dashboard.
    pub fn from_value(value: &Value) -> Result<LayoutSpec, LayoutReadError> {
        if value.is_null() {
            return Ok(LayoutSpec::default());
        }
        serde_json::from_value(value.clone()).map_err(|e| LayoutReadError::Invalid {
            detail: e.to_string(),
        })
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// A first dashboard for `doc`: one section and tab with a tile for
    /// each cell that has an output (up to `max_tiles`), KPIs and scalars
    /// a third wide, everything else half.
    pub fn starter(doc: &NotebookDoc, max_tiles: usize) -> LayoutSpec {
        let mut spec = LayoutSpec::default();
        let (section, tab) = spec.add_section();
        for cell in tileable_cells(doc)
            .filter(|c| c.kind != CellKind::Markdown)
            .take(max_tiles)
        {
            let id = spec.add_tile(section, tab, &cell.id);
            let narrow = matches!(cell.kind, CellKind::Kpi | CellKind::Scalar);
            if let Some(tile) = spec.tile_mut(&id) {
                tile.span.wide = if narrow { 4 } else { 6 };
                tile.height = if narrow { 2 } else { DEFAULT_HEIGHT };
            }
        }
        spec
    }

    fn ids(&self) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        for section in &self.sections {
            ids.insert(section.id.clone());
            for tab in &section.tabs {
                ids.insert(tab.id.clone());
                for tile in &tab.tiles {
                    ids.insert(tile.id.clone());
                }
            }
        }
        ids
    }

    /// Every tile with its position, in reading order.
    pub fn tiles(&self) -> impl Iterator<Item = (TilePos, &Tile)> {
        self.sections.iter().enumerate().flat_map(|(s, section)| {
            section.tabs.iter().enumerate().flat_map(move |(t, tab)| {
                tab.tiles.iter().enumerate().map(move |(i, tile)| {
                    (
                        TilePos {
                            section: s,
                            tab: t,
                            tile: i,
                        },
                        tile,
                    )
                })
            })
        })
    }

    pub fn tile_pos(&self, id: &str) -> Option<TilePos> {
        self.tiles().find(|(_, t)| t.id == id).map(|(pos, _)| pos)
    }

    pub fn tile(&self, id: &str) -> Option<&Tile> {
        self.tiles().find(|(_, t)| t.id == id).map(|(_, t)| t)
    }

    pub fn tile_mut(&mut self, id: &str) -> Option<&mut Tile> {
        let pos = self.tile_pos(id)?;
        self.sections[pos.section].tabs[pos.tab]
            .tiles
            .get_mut(pos.tile)
    }

    /// The tiles of one section tab.
    pub fn tab_tiles(&self, slide: SlideRef) -> &[Tile] {
        self.sections
            .get(slide.section)
            .and_then(|s| s.tabs.get(slide.tab))
            .map(|t| t.tiles.as_slice())
            .unwrap_or(&[])
    }

    /// Add a section with one tab; returns their indexes.
    pub fn add_section(&mut self) -> (usize, usize) {
        let mut taken = self.ids();
        let id = next_id("s", &taken);
        taken.insert(id.clone());
        let tab = next_id("tab", &taken);
        self.sections.push(Section {
            id,
            tabs: vec![Tab {
                id: tab,
                ..Tab::default()
            }],
            ..Section::default()
        });
        (self.sections.len() - 1, 0)
    }

    /// Add a tab to `section`; returns its index.
    pub fn add_tab(&mut self, section: usize) -> Option<usize> {
        let id = next_id("tab", &self.ids());
        let section = self.sections.get_mut(section)?;
        section.tabs.push(Tab {
            id,
            ..Tab::default()
        });
        Some(section.tabs.len() - 1)
    }

    pub fn remove_section(&mut self, section: usize) -> bool {
        if section < self.sections.len() {
            self.sections.remove(section);
            true
        } else {
            false
        }
    }

    pub fn remove_tab(&mut self, section: usize, tab: usize) -> bool {
        match self.sections.get_mut(section) {
            Some(s) if tab < s.tabs.len() && s.tabs.len() > 1 => {
                s.tabs.remove(tab);
                true
            }
            _ => false,
        }
    }

    /// Add a tile showing `cell` at the end of a section tab (creating the
    /// first section when there is none); returns its id.
    pub fn add_tile(&mut self, section: usize, tab: usize, cell: &str) -> String {
        if self.sections.is_empty() {
            self.add_section();
        }
        let section = section.min(self.sections.len() - 1);
        if self.sections[section].tabs.is_empty() {
            self.add_tab(section);
        }
        let tab = tab.min(self.sections[section].tabs.len() - 1);
        let id = next_id("t", &self.ids());
        self.sections[section].tabs[tab]
            .tiles
            .push(Tile::new(id.clone(), cell));
        id
    }

    pub fn remove_tile(&mut self, id: &str) -> bool {
        match self.tile_pos(id) {
            Some(pos) => {
                self.sections[pos.section].tabs[pos.tab]
                    .tiles
                    .remove(pos.tile);
                true
            }
            None => false,
        }
    }

    /// Move a tile `delta` places within its tab.
    pub fn move_tile(&mut self, id: &str, delta: isize) -> bool {
        let Some(pos) = self.tile_pos(id) else {
            return false;
        };
        let tiles = &mut self.sections[pos.section].tabs[pos.tab].tiles;
        let to = pos.tile as isize + delta;
        if to < 0 || to as usize >= tiles.len() {
            return false;
        }
        let tile = tiles.remove(pos.tile);
        tiles.insert(to as usize, tile);
        true
    }

    /// Move a tile to the end of another section tab.
    pub fn move_tile_to(&mut self, id: &str, slide: SlideRef) -> bool {
        let Some(pos) = self.tile_pos(id) else {
            return false;
        };
        if slide.section >= self.sections.len()
            || slide.tab >= self.sections[slide.section].tabs.len()
            || (pos.section, pos.tab) == (slide.section, slide.tab)
        {
            return false;
        }
        let tile = self.sections[pos.section].tabs[pos.tab]
            .tiles
            .remove(pos.tile);
        self.sections[slide.section].tabs[slide.tab]
            .tiles
            .push(tile);
        true
    }

    /// Set a tile's span for one breakpoint, clamped to the grid.
    pub fn set_span(&mut self, id: &str, breakpoint: Breakpoint, span: u8) -> bool {
        let span = span.clamp(1, GRID_COLUMNS);
        let Some(tile) = self.tile_mut(id) else {
            return false;
        };
        let slot = match breakpoint {
            Breakpoint::Wide => &mut tile.span.wide,
            Breakpoint::Compact => &mut tile.span.compact,
        };
        let changed = *slot != span;
        *slot = span;
        changed
    }

    pub fn set_height(&mut self, id: &str, height: u8) -> bool {
        let height = height.clamp(MIN_HEIGHT, MAX_HEIGHT);
        match self.tile_mut(id) {
            Some(tile) if tile.height != height => {
                tile.height = height;
                true
            }
            _ => false,
        }
    }

    /// Clamp spans and heights into range. Returns whether anything moved.
    pub fn normalize(&mut self) -> bool {
        let mut changed = false;
        for section in &mut self.sections {
            for tab in &mut section.tabs {
                for tile in &mut tab.tiles {
                    let before = (tile.span, tile.height);
                    tile.span.wide = tile.span.wide.clamp(1, GRID_COLUMNS);
                    tile.span.compact = tile.span.compact.clamp(1, GRID_COLUMNS);
                    tile.height = tile.height();
                    changed |= before != (tile.span, tile.height);
                }
            }
        }
        changed
    }

    /// The slides a presentation walks through: one per section tab, or
    /// the first tab of the first section for [`SlideMode::Whole`].
    pub fn slides(&self) -> Vec<SlideRef> {
        match self.presentation.slides {
            SlideMode::PerSection => self
                .sections
                .iter()
                .enumerate()
                .flat_map(|(s, section)| {
                    (0..section.tabs.len()).map(move |t| SlideRef { section: s, tab: t })
                })
                .collect(),
            SlideMode::Whole => (!self.sections.is_empty())
                .then_some(SlideRef { section: 0, tab: 0 })
                .into_iter()
                .collect(),
        }
    }

    /// Where a drill-through lands: the target section (by id) and tab (by
    /// id, else the first).
    pub fn drill_target(&self, drill: &Drill) -> Option<SlideRef> {
        let section = self.sections.iter().position(|s| s.id == drill.section)?;
        let tab = match &drill.tab {
            Some(id) => self.sections[section]
                .tabs
                .iter()
                .position(|t| t.id == *id)?,
            None => 0,
        };
        (tab < self.sections[section].tabs.len()).then_some(SlideRef { section, tab })
    }

    /// Every parameter the dashboard reads (filter bar, visibility rules,
    /// drill-throughs), in first-use order.
    pub fn parameters(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        let used = self
            .filters
            .params
            .iter()
            .map(String::as_str)
            .chain(self.tiles().flat_map(|(_, tile)| {
                let rule = match &tile.visible_when {
                    Some(Condition::Known(rule)) => rule.param(),
                    _ => None,
                };
                rule.into_iter()
                    .chain(tile.drill.as_ref().and_then(|d| d.param.as_deref()))
            }));
        for name in used {
            if !out.contains(&name) {
                out.push(name);
            }
        }
        out
    }

    /// Problems the editor lists. `doc` is the notebook the layout cell
    /// lives in.
    pub fn validate(&self, doc: &NotebookDoc) -> Vec<LayoutIssue> {
        let mut issues = BTreeSet::new();
        let mut seen = BTreeSet::new();
        let mut note_id = |id: &str, issues: &mut BTreeSet<LayoutIssue>| {
            if id.is_empty() {
                issues.insert(LayoutIssue::EmptyId);
            } else if !seen.insert(id.to_string()) {
                issues.insert(LayoutIssue::DuplicateId { id: id.to_string() });
            }
        };
        for section in &self.sections {
            note_id(&section.id, &mut issues);
            if section.tabs.is_empty() {
                issues.insert(LayoutIssue::SectionWithoutTabs {
                    section: section.id.clone(),
                });
            }
            for tab in &section.tabs {
                note_id(&tab.id, &mut issues);
                for tile in &tab.tiles {
                    note_id(&tile.id, &mut issues);
                }
            }
        }
        let declared: BTreeSet<&str> = doc.parameters.iter().map(|p| p.name.as_str()).collect();
        for param in self.parameters() {
            if !declared.contains(param) {
                issues.insert(LayoutIssue::UnknownParameter {
                    param: param.to_string(),
                });
            }
        }
        for (_, tile) in self.tiles() {
            match doc.cell(&tile.cell) {
                None => {
                    issues.insert(LayoutIssue::UnknownCell {
                        tile: tile.id.clone(),
                        cell: tile.cell.clone(),
                    });
                }
                Some(cell) if cell.kind == CellKind::Layout => {
                    issues.insert(LayoutIssue::TileShowsDashboard {
                        tile: tile.id.clone(),
                    });
                }
                Some(_) => {}
            }
            for span in [tile.span.wide, tile.span.compact] {
                if !(1..=GRID_COLUMNS).contains(&span) {
                    issues.insert(LayoutIssue::SpanOutOfRange {
                        tile: tile.id.clone(),
                        span,
                    });
                }
            }
            if !(MIN_HEIGHT..=MAX_HEIGHT).contains(&tile.height) {
                issues.insert(LayoutIssue::HeightOutOfRange {
                    tile: tile.id.clone(),
                    height: tile.height,
                });
            }
            match &tile.visible_when {
                Some(Condition::Other(_)) => {
                    issues.insert(LayoutIssue::UnsupportedCondition {
                        tile: tile.id.clone(),
                    });
                }
                Some(Condition::Known(Visibility::TestPasses { cell }))
                    if doc.cell(cell).is_none_or(|c| c.kind != CellKind::Assertion) =>
                {
                    issues.insert(LayoutIssue::UnknownCheck {
                        tile: tile.id.clone(),
                        cell: cell.clone(),
                    });
                }
                _ => {}
            }
            if let Some(drill) = &tile.drill
                && self.drill_target(drill).is_none()
            {
                issues.insert(LayoutIssue::UnknownDrillTarget {
                    tile: tile.id.clone(),
                    section: drill.section.clone(),
                });
            }
        }
        issues.into_iter().collect()
    }
}

/// Tiles packed into grid rows, left to right: each row's spans add up to
/// at most 12. Returns indexes into `tiles`.
pub fn pack_rows(tiles: &[&Tile], breakpoint: Breakpoint) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut used = GRID_COLUMNS;
    for (i, tile) in tiles.iter().enumerate() {
        let span = tile.span(breakpoint);
        if used + span > GRID_COLUMNS {
            rows.push(Vec::new());
            used = 0;
        }
        used += span;
        rows.last_mut().expect("a row was started").push(i);
    }
    rows
}

/// The remote cells (SQL, plans, entity lookups) whose results `cells`
/// show, directly or through their inputs: what a refresh of those tiles
/// queries. In run order, without duplicates.
pub fn remote_cells_behind(doc: &NotebookDoc, graph: &Graph, cells: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for id in cells {
        let node = Node::cell(*id);
        for n in graph.upstream_closure(&node).iter().chain([&node]) {
            if let Some(cell_id) = n.as_cell()
                && doc
                    .cell(cell_id)
                    .is_some_and(|c| c.kind.exec_class() == ExecClass::Remote)
                && !out.iter().any(|o| o == cell_id)
            {
                out.push(cell_id.to_string());
            }
        }
    }
    out
}
