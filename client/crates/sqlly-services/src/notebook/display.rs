//! How a cell is shown in the notebook: in full (header, editor, results
//! chrome) or embedded (its output alone, flush with its neighbours, so a
//! notebook reads like a report). Kept in the cell's `extra` under
//! [`DISPLAY_KEY`], with a notebook-wide default in the settings under
//! [`DEFAULT_KEY`]. Display settings never change what a cell runs, so the
//! trust hash leaves them out.
//!
//! ```json
//! "display": {"mode": "embedded", "config": "shown", "height": 320, "lock": "pivot"}
//! ```
//!
//! Every field is optional; values this build does not know are kept as
//! written.

use serde_json::{Map, Value, json};

use super::document::{Cell, CellKind, NotebookDoc, NotebookSettings};

/// Cell `extra` key of a cell's own display settings.
pub const DISPLAY_KEY: &str = "display";
/// `settings` key of the notebook's default display mode.
pub const DEFAULT_KEY: &str = "display_default";
/// Bounds of a stored output height, in logical pixels.
pub const MIN_HEIGHT: f32 = 60.0;
pub const MAX_HEIGHT: f32 = 2000.0;

/// Full shows the cell with its header and editors; embedded shows only
/// its output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayMode {
    #[default]
    Full,
    Embedded,
}

impl DisplayMode {
    pub fn as_str(self) -> &'static str {
        match self {
            DisplayMode::Full => "full",
            DisplayMode::Embedded => "embedded",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "full" => Some(DisplayMode::Full),
            "embedded" => Some(DisplayMode::Embedded),
            _ => None,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            DisplayMode::Full => DisplayMode::Embedded,
            DisplayMode::Embedded => DisplayMode::Full,
        }
    }
}

/// The result view a grid stays on when switching is locked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockedView {
    Rows,
    Pivot,
    Chart,
}

impl LockedView {
    pub const ALL: [LockedView; 3] = [LockedView::Rows, LockedView::Pivot, LockedView::Chart];

    pub fn as_str(self) -> &'static str {
        match self {
            LockedView::Rows => "rows",
            LockedView::Pivot => "pivot",
            LockedView::Chart => "chart",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "rows" => Some(LockedView::Rows),
            "pivot" => Some(LockedView::Pivot),
            "chart" => Some(LockedView::Chart),
            _ => None,
        }
    }
}

/// One cell's display settings (or the notebook default) as written.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CellDisplay {
    pub mode: Option<DisplayMode>,
    /// Whether an embedded pivot or chart shows its configuration.
    pub config_shown: Option<bool>,
    /// The output's height when the user resized it.
    pub height: Option<f32>,
    /// A results grid stays on this view and cannot switch.
    pub lock: Option<LockedView>,
    /// Fields (and values) this build does not know, kept as written.
    pub unknown: Map<String, Value>,
}

impl CellDisplay {
    pub fn from_value(value: Option<&Value>) -> Self {
        let Some(Value::Object(map)) = value else {
            return Self::default();
        };
        let mut out = Self::default();
        for (key, value) in map {
            let known = match key.as_str() {
                "mode" => {
                    out.mode = value.as_str().and_then(DisplayMode::parse);
                    out.mode.is_some()
                }
                "config" => {
                    out.config_shown = match value.as_str() {
                        Some("shown") => Some(true),
                        Some("hidden") => Some(false),
                        _ => None,
                    };
                    out.config_shown.is_some()
                }
                "height" => {
                    out.height = value
                        .as_f64()
                        .filter(|h| h.is_finite())
                        .map(|h| (h as f32).clamp(MIN_HEIGHT, MAX_HEIGHT));
                    out.height.is_some()
                }
                "lock" => {
                    out.lock = value.as_str().and_then(LockedView::parse);
                    out.lock.is_some()
                }
                _ => false,
            };
            if !known {
                out.unknown.insert(key.clone(), value.clone());
            }
        }
        out
    }

    pub fn to_value(&self) -> Option<Value> {
        let mut map = self.unknown.clone();
        if let Some(mode) = self.mode {
            map.insert("mode".into(), json!(mode.as_str()));
        }
        if let Some(shown) = self.config_shown {
            map.insert(
                "config".into(),
                json!(if shown { "shown" } else { "hidden" }),
            );
        }
        if let Some(height) = self.height {
            map.insert(
                "height".into(),
                json!(height.clamp(MIN_HEIGHT, MAX_HEIGHT).round() as i64),
            );
        }
        if let Some(lock) = self.lock {
            map.insert("lock".into(), json!(lock.as_str()));
        }
        (!map.is_empty()).then_some(Value::Object(map))
    }
}

/// What a cell's display settings come to once the notebook default is
/// applied.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResolvedDisplay {
    pub mode: DisplayMode,
    /// An embedded pivot or chart shows its configuration (hidden unless
    /// the cell says otherwise).
    pub config_shown: bool,
    pub height: Option<f32>,
    pub lock: Option<LockedView>,
}

impl ResolvedDisplay {
    pub fn embedded(&self) -> bool {
        self.mode == DisplayMode::Embedded
    }
}

/// Whether a kind can be shown embedded: everything with an output.
/// Parameters are controls and dashboards are layouts, so they stay full.
pub fn embeddable(kind: &CellKind) -> bool {
    !matches!(
        kind,
        CellKind::Parameter | CellKind::Layout | CellKind::Python | CellKind::Unknown(_)
    )
}

/// The cell's own display settings.
pub fn cell_display(cell: &Cell) -> CellDisplay {
    CellDisplay::from_value(cell.extra.get(DISPLAY_KEY))
}

/// Write the cell's display settings; empty settings remove the key.
pub fn set_cell_display(cell: &mut Cell, display: &CellDisplay) {
    match display.to_value() {
        Some(value) => {
            cell.extra.insert(DISPLAY_KEY.to_string(), value);
        }
        None => {
            cell.extra.remove(DISPLAY_KEY);
        }
    }
}

/// The notebook's default display settings.
pub fn notebook_default(settings: &NotebookSettings) -> CellDisplay {
    CellDisplay::from_value(settings.extra.get(DEFAULT_KEY))
}

/// The notebook's default mode (full unless set).
pub fn default_mode(settings: &NotebookSettings) -> DisplayMode {
    notebook_default(settings).mode.unwrap_or_default()
}

/// Set the notebook's default mode, keeping anything else written there.
pub fn set_default_mode(settings: &mut NotebookSettings, mode: DisplayMode) {
    let mut display = notebook_default(settings);
    display.mode = (mode != DisplayMode::Full).then_some(mode);
    match display.to_value() {
        Some(value) => {
            settings.extra.insert(DEFAULT_KEY.to_string(), value);
        }
        None => {
            settings.extra.remove(DEFAULT_KEY);
        }
    }
}

/// The cell's display with the notebook default applied.
pub fn resolve(settings: &NotebookSettings, cell: &Cell) -> ResolvedDisplay {
    let own = cell_display(cell);
    let fallback = notebook_default(settings);
    let mode = if embeddable(&cell.kind) {
        own.mode.or(fallback.mode).unwrap_or_default()
    } else {
        DisplayMode::Full
    };
    ResolvedDisplay {
        mode,
        config_shown: own.config_shown.or(fallback.config_shown).unwrap_or(false),
        height: own.height,
        lock: own.lock,
    }
}

/// Show `ids` in `mode`. A cell whose mode then matches the notebook
/// default drops its own mode, so changing the default later moves it too.
/// Returns whether anything changed.
pub fn set_mode(doc: &mut NotebookDoc, ids: &[&str], mode: DisplayMode) -> bool {
    let default = default_mode(&doc.settings);
    let mut changed = false;
    for cell in doc
        .cells
        .iter_mut()
        .filter(|c| ids.contains(&c.id.as_str()))
    {
        if !embeddable(&cell.kind) {
            continue;
        }
        let mut display = cell_display(cell);
        display.mode = (mode != default).then_some(mode);
        let before = cell.extra.get(DISPLAY_KEY).cloned();
        set_cell_display(cell, &display);
        changed |= before != cell.extra.get(DISPLAY_KEY).cloned();
    }
    changed
}

/// Toggle `ids` between full and embedded: all embedded when any of them
/// is full, else all full. Returns the mode they now have.
pub fn toggle_mode(doc: &mut NotebookDoc, ids: &[&str]) -> Option<DisplayMode> {
    let modes: Vec<DisplayMode> = doc
        .cells
        .iter()
        .filter(|c| ids.contains(&c.id.as_str()) && embeddable(&c.kind))
        .map(|c| resolve(&doc.settings, c).mode)
        .collect();
    if modes.is_empty() {
        return None;
    }
    let mode = if modes.contains(&DisplayMode::Full) {
        DisplayMode::Embedded
    } else {
        DisplayMode::Full
    };
    set_mode(doc, ids, mode);
    Some(mode)
}

/// "Embed all outputs" on or off: the notebook default becomes `mode` and
/// every cell's own mode is cleared, so the whole notebook follows it (new
/// cells included).
pub fn set_all(doc: &mut NotebookDoc, mode: DisplayMode) {
    set_default_mode(&mut doc.settings, mode);
    for cell in &mut doc.cells {
        let mut display = cell_display(cell);
        if display.mode.take().is_some() {
            set_cell_display(cell, &display);
        }
    }
}

/// Change one cell's settings in place (config, height, lock).
pub fn update(cell: &mut Cell, change: impl FnOnce(&mut CellDisplay)) {
    let mut display = cell_display(cell);
    change(&mut display);
    set_cell_display(cell, &display);
}

/// The settings without display state, for hashes that must not move
/// when only the presentation changes.
pub fn without_display(settings: &NotebookSettings) -> NotebookSettings {
    let mut settings = settings.clone();
    settings.extra.remove(DEFAULT_KEY);
    settings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::{ExecutionPolicy, NotebookDoc};

    fn doc() -> NotebookDoc {
        let mut doc = NotebookDoc::empty("nb", "Report");
        for (id, kind) in [
            ("c_sql", CellKind::Sql),
            ("c_pivot", CellKind::Pivot),
            ("c_param", CellKind::Parameter),
        ] {
            doc.cells
                .push(Cell::new(id, kind, ExecutionPolicy::default()));
        }
        doc
    }

    #[test]
    fn settings_round_trip_and_keep_unknown_values() {
        let value = json!({"mode": "embedded", "config": "shown", "height": 321.4,
                           "lock": "pivot", "mode_hint": 1});
        let display = CellDisplay::from_value(Some(&value));
        assert_eq!(display.mode, Some(DisplayMode::Embedded));
        assert_eq!(display.config_shown, Some(true));
        assert_eq!(display.lock, Some(LockedView::Pivot));
        assert_eq!(
            display.to_value(),
            Some(json!({"mode": "embedded", "config": "shown", "height": 321,
                        "lock": "pivot", "mode_hint": 1}))
        );
        let newer = json!({"mode": "floating", "lock": "map"});
        let display = CellDisplay::from_value(Some(&newer));
        assert_eq!(display.mode, None);
        assert_eq!(display.lock, None);
        assert_eq!(display.to_value(), Some(newer));
        assert_eq!(CellDisplay::default().to_value(), None);
    }

    #[test]
    fn the_notebook_default_applies_to_cells_without_their_own_mode() {
        let mut doc = doc();
        doc.cells.push(Cell::new(
            "c_new",
            CellKind::Chart,
            ExecutionPolicy::default(),
        ));
        set_all(&mut doc, DisplayMode::Embedded);
        let mode = |doc: &NotebookDoc, i: usize| resolve(&doc.settings, &doc.cells[i]).mode;
        assert_eq!(mode(&doc, 0), DisplayMode::Embedded);
        assert_eq!(mode(&doc, 3), DisplayMode::Embedded);
        assert_eq!(mode(&doc, 2), DisplayMode::Full, "parameters stay full");
        assert!(!resolve(&doc.settings, &doc.cells[1]).config_shown);
        assert_eq!(
            doc.settings.extra.get(DEFAULT_KEY),
            Some(&json!({"mode": "embedded"}))
        );
        set_mode(&mut doc, &["c_sql"], DisplayMode::Full);
        assert_eq!(cell_display(&doc.cells[0]).mode, Some(DisplayMode::Full));
        set_mode(&mut doc, &["c_sql"], DisplayMode::Embedded);
        assert_eq!(
            doc.cells[0].extra.get(DISPLAY_KEY),
            None,
            "matching the default drops the cell's own mode"
        );
        set_all(&mut doc, DisplayMode::Full);
        assert!(!doc.settings.extra.contains_key(DEFAULT_KEY));
        assert_eq!(mode(&doc, 0), DisplayMode::Full);
    }

    #[test]
    fn toggling_a_mixed_selection_embeds_all_of_it() {
        let mut doc = doc();
        set_mode(&mut doc, &["c_sql"], DisplayMode::Embedded);
        let ids = ["c_sql", "c_pivot", "c_param"];
        assert_eq!(toggle_mode(&mut doc, &ids), Some(DisplayMode::Embedded));
        assert!(resolve(&doc.settings, &doc.cells[1]).embedded());
        assert_eq!(toggle_mode(&mut doc, &ids), Some(DisplayMode::Full));
        assert!(doc.cells.iter().all(|c| !c.extra.contains_key(DISPLAY_KEY)));
        assert_eq!(toggle_mode(&mut doc, &["c_param"]), None);
    }

    #[test]
    fn display_settings_survive_sqllynb_and_ipynb() {
        use crate::notebook::document::SeededIds;
        let mut doc = doc();
        set_all(&mut doc, DisplayMode::Embedded);
        update(&mut doc.cells[1], |d| {
            d.config_shown = Some(true);
            d.lock = Some(LockedView::Pivot);
        });
        let text = doc.to_canonical_json();
        let back = crate::notebook::load(Some("r.sqllynb"), &text, &mut SeededIds(1), None)
            .unwrap()
            .doc;
        assert_eq!(
            resolve(&back.settings, &back.cells[1]),
            resolve(&doc.settings, &doc.cells[1])
        );
        assert_eq!(back.settings, doc.settings);
        let ipynb = crate::notebook::to_ipynb(&doc);
        let back = crate::notebook::migrate::from_ipynb(&ipynb, &mut SeededIds(1), None).unwrap();
        assert_eq!(
            back.cells[1].extra.get(DISPLAY_KEY),
            doc.cells[1].extra.get(DISPLAY_KEY)
        );
        assert_eq!(default_mode(&back.settings), DisplayMode::Embedded);
    }

    #[test]
    fn update_keeps_the_other_fields_and_clears_an_empty_display() {
        let mut doc = doc();
        update(&mut doc.cells[1], |d| {
            d.config_shown = Some(true);
            d.height = Some(10.0);
        });
        let display = cell_display(&doc.cells[1]);
        assert_eq!(display.config_shown, Some(true));
        assert_eq!(display.height, Some(MIN_HEIGHT));
        update(&mut doc.cells[1], |d| *d = CellDisplay::default());
        assert!(!doc.cells[1].extra.contains_key(DISPLAY_KEY));
    }
}
