//! Interactive ER relationship graph for one database.
//!
//! A dialog on the unified chrome (`crate::dialog`, like `object_dependencies`)
//! that fetches every foreign-key column mapping via
//! `MetadataService::list_foreign_keys`,
//! folds the rows into a pure `ErGraph` (tables as nodes, one edge per
//! constraint — compound keys collapse into a single labeled edge), lays the
//! nodes out on a grid or layered canvas, and paints the result with the
//! `canvas` + `paint_quad` + `PathBuilder` + `shape_line` mechanism the chart
//! pane uses. Zoom, pan, node drag, focus-on-neighbours, and SVG/Mermaid/DBML
//! export live here; clicking a node without dragging still emits
//! [`ErDiagramEvent::RevealObject`] for the host to route to the schema tree.

// Browser build: swap tokio's channels for the reactor-free shim so the
// `tokio::` paths below compile unchanged (same trick as `tool_dialog`).
#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use gpui::{
    App, Bounds, ClipboardItem, Corners, Edges, Entity, EventEmitter, FocusHandle, Hsla,
    IntoElement, MouseButton, PaintQuad, ParentElement, Pixels, Point, Render, SharedString,
    Styled, Task, TextAlign, TextRun, TitlebarOptions, WeakEntity, Window, WindowBounds,
    WindowOptions, canvas, div, point, prelude::*, px, size,
};
use gpui_component::Disableable as _;
use gpui_component::IndexPath;
use gpui_component::Selectable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::list::{List, ListDelegate, ListState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_services::{BulkColumnRow, ForeignKeyRow};

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::er_export::{ErSvgStyle, er_dbml, er_mermaid, er_svg};
use crate::er_layout::{
    ZOOM_STEP, clamp_zoom, fit_to_view, layout_layered, layout_subset, neighbourhood, subgraph,
    zoom_about,
};
use crate::theme::Theme;
use crate::tool_dialog::ToolDialogContext;

fn localized_format<T: std::fmt::Display>(key: &str, arguments: &[(&str, T)]) -> String {
    crate::localization::format(key, arguments)
}

// ---------------------------------------------------------------------------
// Pure model + layout (no GPUI types; unit tested)
// ---------------------------------------------------------------------------

/// Most tables the diagram will draw. Beyond this the least-connected tables
/// are dropped (with an honest truncation notice) — a 600-table canvas is
/// noise, not a diagram.
pub const NODE_CAP: usize = 60;

/// A table identity: `(schema, table)`. `schema` may be empty for engines
/// (e.g. SQLite) that have no schema namespace.
pub type TableKey = (String, String);

/// How the diagram surface is hosted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErPresentation {
    /// Centered modal overlay inside the main window (backdrop + fixed pane).
    Modal,
    /// A standalone, OS-resizable/movable window that the pane fills.
    Window,
}

/// Default node box size in abstract canvas units (the paint pass treats them
/// as px). The width is a view setting the user can widen or narrow —
/// `NODE_W` is only where it starts.
pub const NODE_W: f32 = 210.0;
pub const NODE_H: f32 = 58.0;
/// Narrowest and widest the card can be dragged/stepped to.
pub(crate) const MIN_NODE_W: f32 = 140.0;
pub(crate) const MAX_NODE_W: f32 = 520.0;
/// One step of the width control.
pub(crate) const NODE_W_STEP: f32 = 30.0;
/// Grid cell pitch; the gap between boxes is the pitch minus the box size.
pub(crate) const CELL_W: f32 = 274.0;
pub(crate) const CELL_H: f32 = 128.0;
pub(crate) const MARGIN: f32 = 28.0;
/// Vertical gap between grid rows — the pitch is the node height plus this
/// (58 + 70 = the legacy `CELL_H` of 128, so key-only boxes lay out as before).
pub(crate) const CELL_V_GAP: f32 = CELL_H - NODE_H;
/// Horizontal gap between grid columns — the pitch is the node width plus
/// this, so widening the cards spreads the grid instead of overlapping it.
pub(crate) const CELL_H_GAP: f32 = CELL_W - NODE_W;
/// Title band height and per-column row height when column rows are shown,
/// canvas units.
pub(crate) const NODE_HEADER_H: f32 = 26.0;
pub(crate) const NODE_COL_ROW_H: f32 = 15.0;
/// Bottom padding under the last column row.
pub(crate) const NODE_COL_PAD: f32 = 8.0;
/// Safety ceiling on painted column rows. Cards are sized to show every
/// column of the widest table, so this exists only to stop one pathological
/// table (hundreds of columns) from making every card unusably tall.
pub(crate) const MAX_NODE_COLUMNS: usize = 200;
/// Shift+scroll travel (px-equivalent units) per 5% zoom step.
const ZOOM_SCROLL_STEP: f32 = 10.0;

/// One column of a diagrammed table (full catalog list, ordinal order).
#[derive(Clone, Debug, PartialEq)]
pub struct ErColumn {
    pub name: String,
    /// Engine-reported type name; empty when the engine didn't provide one.
    pub data_type: String,
    pub nullable: Option<bool>,
    /// Whether this column participates in any FK (either side).
    pub in_key: bool,
}

/// One table that participates in at least one foreign key.
#[derive(Clone, Debug, PartialEq)]
pub struct ErNode {
    pub schema: String,
    pub table: String,
    /// Columns of this table that appear in any FK (either side), first-seen
    /// order, deduplicated.
    pub columns_in_keys: Vec<String>,
    /// Full column list from the bulk catalog fetch (ordinal order); empty
    /// when the engine has no bulk listing. Attached by [`attach_columns`].
    pub columns: Vec<ErColumn>,
}

impl ErNode {
    pub fn qualified(&self) -> String {
        if self.schema.is_empty() {
            self.table.clone()
        } else {
            format!("{}.{}", self.schema, self.table)
        }
    }
}

/// One relationship (one FK constraint), pointing at the referenced table.
#[derive(Clone, Debug, PartialEq)]
pub struct ErEdge {
    /// Index into `ErGraph::nodes` of the referencing (child) table.
    pub from: usize,
    /// Index into `ErGraph::nodes` of the referenced (parent) table.
    pub to: usize,
    /// `constraint (from_cols → to_cols)`; compound keys list every pair.
    pub label: String,
    /// Recoverable column pairs (`from_col`, `to_col`) for exporters.
    pub pairs: Vec<(String, String)>,
}

/// The folded FK graph. Node order is the deterministic layout order:
/// connectivity-seeded BFS so related tables land near each other on the grid.
#[derive(Clone, Debug, Default)]
pub struct ErGraph {
    pub nodes: Vec<ErNode>,
    pub edges: Vec<ErEdge>,
    /// True when the node cap dropped tables (and their edges).
    pub truncated: bool,
    /// Distinct tables that appear in the FK rows, before the cap.
    pub total_tables: usize,
    /// Distinct FK constraints in the rows, before the cap.
    pub total_relationships: usize,
}

/// Positions (top-left corner of each node box, parallel to `nodes`) plus the
/// abstract canvas size that contains them all.
#[derive(Clone, Debug, PartialEq)]
pub struct ErLayout {
    pub positions: Vec<(f32, f32)>,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LayoutKind {
    Grid,
    Layered,
}

struct DragState {
    node: usize,
    grab_offset: (f32, f32),
    moved: bool,
    start: Point<Pixels>,
}

/// Fold raw FK column rows into the graph: group rows sharing a constraint
/// (and endpoints) into one edge, dedupe tables, cap to the [`NODE_CAP`]
/// most-connected tables, and order nodes hub-first via BFS.
pub fn build_graph(rows: &[ForeignKeyRow]) -> ErGraph {
    fold_graph(rows, true, &[])
}

/// Build a graph restricted to exactly the tables in `selected`. FK edges
/// whose *both* endpoints are selected are kept; a selected table that touches
/// no kept edge still appears as an isolated node. The [`NODE_CAP`] does not
/// apply — the user chose this set explicitly, so it is honored in full.
pub fn build_graph_selected(rows: &[ForeignKeyRow], selected: &HashSet<TableKey>) -> ErGraph {
    // Keep only FK rows fully inside the selection.
    let kept_rows: Vec<ForeignKeyRow> = rows
        .iter()
        .filter(|r| {
            selected.contains(&(r.from_schema.clone(), r.from_table.clone()))
                && selected.contains(&(r.to_schema.clone(), r.to_table.clone()))
        })
        .cloned()
        .collect();
    // Isolated selected tables: in the selection but touching no kept FK row.
    let mut connected: HashSet<TableKey> = HashSet::new();
    for r in &kept_rows {
        connected.insert((r.from_schema.clone(), r.from_table.clone()));
        connected.insert((r.to_schema.clone(), r.to_table.clone()));
    }
    let mut isolated: Vec<TableKey> = selected.difference(&connected).cloned().collect();
    isolated.sort();
    fold_graph(&kept_rows, false, &isolated)
}

/// The tables the canvas currently shows: the focus neighbourhood when one is
/// active, otherwise every node in the display graph. "Script as CREATE"
/// scripts exactly this set, sorted for deterministic output.
pub fn tables_in_view(graph: &ErGraph, visible: &Option<Vec<usize>>) -> Vec<TableKey> {
    let mut keys: Vec<TableKey> = match visible {
        Some(indices) => indices
            .iter()
            .filter_map(|i| graph.nodes.get(*i))
            .map(|n| (n.schema.clone(), n.table.clone()))
            .collect(),
        None => graph
            .nodes
            .iter()
            .map(|n| (n.schema.clone(), n.table.clone()))
            .collect(),
    };
    keys.sort();
    keys.dedup();
    keys
}

/// Shared graph fold. `apply_cap` drops all but the [`NODE_CAP`] most-connected
/// tables (default FK view); `extra_isolated` are tables to append as edgeless
/// nodes after the connected ones (used by the explicit-selection path).
fn fold_graph(rows: &[ForeignKeyRow], apply_cap: bool, extra_isolated: &[TableKey]) -> ErGraph {
    use std::collections::HashMap;

    struct Rel {
        constraint: String,
        from: usize,
        to: usize,
        pairs: Vec<(String, String)>,
    }

    // Tables, first-seen order (child endpoint first, per row).
    let mut table_index: HashMap<(String, String), usize> = HashMap::new();
    let mut tables: Vec<ErNode> = Vec::new();
    let mut intern = |schema: &str, table: &str, tables: &mut Vec<ErNode>| -> usize {
        *table_index
            .entry((schema.to_string(), table.to_string()))
            .or_insert_with(|| {
                tables.push(ErNode {
                    schema: schema.to_string(),
                    table: table.to_string(),
                    columns_in_keys: Vec::new(),
                    columns: Vec::new(),
                });
                tables.len() - 1
            })
    };

    // Relationships, first-seen order, keyed so compound keys fold together.
    let mut rel_index: HashMap<(String, usize, usize), usize> = HashMap::new();
    let mut rels: Vec<Rel> = Vec::new();
    for row in rows {
        let from = intern(&row.from_schema, &row.from_table, &mut tables);
        let to = intern(&row.to_schema, &row.to_table, &mut tables);
        let key = (row.constraint_name.clone(), from, to);
        let idx = *rel_index.entry(key).or_insert_with(|| {
            rels.push(Rel {
                constraint: row.constraint_name.clone(),
                from,
                to,
                pairs: Vec::new(),
            });
            rels.len() - 1
        });
        let pair = (row.from_column.clone(), row.to_column.clone());
        if !rels[idx].pairs.contains(&pair) {
            rels[idx].pairs.push(pair);
        }
    }

    // Key-column annotations per table, deduplicated, first-seen order.
    for rel in &rels {
        for (from_col, to_col) in &rel.pairs {
            let node = &mut tables[rel.from];
            if !node.columns_in_keys.contains(from_col) {
                node.columns_in_keys.push(from_col.clone());
            }
            let node = &mut tables[rel.to];
            if !node.columns_in_keys.contains(to_col) {
                node.columns_in_keys.push(to_col.clone());
            }
        }
    }

    // Connectivity: relationships touching each table (self-refs count once).
    let mut degree = vec![0usize; tables.len()];
    for rel in &rels {
        degree[rel.from] += 1;
        if rel.to != rel.from {
            degree[rel.to] += 1;
        }
    }

    let total_tables = tables.len() + extra_isolated.len();
    let total_relationships = rels.len();

    // Cap: keep the most-connected tables; name breaks ties deterministically.
    let mut order: Vec<usize> = (0..tables.len()).collect();
    order.sort_by(|&a, &b| {
        degree[b]
            .cmp(&degree[a])
            .then_with(|| tables[a].qualified().cmp(&tables[b].qualified()))
    });
    let truncated = apply_cap && order.len() > NODE_CAP;
    if apply_cap {
        order.truncate(NODE_CAP);
    }
    let kept: std::collections::HashSet<usize> = order.iter().copied().collect();

    // BFS from the highest-degree kept table so neighbors sit adjacently in
    // the row-major grid. Neighbor visit order is (degree desc, name asc) —
    // fully deterministic, no randomness anywhere.
    let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); tables.len()];
    for rel in &rels {
        if kept.contains(&rel.from) && kept.contains(&rel.to) && rel.from != rel.to {
            adjacency[rel.from].push(rel.to);
            adjacency[rel.to].push(rel.from);
        }
    }
    for neighbors in &mut adjacency {
        neighbors.sort_by(|&a, &b| {
            degree[b]
                .cmp(&degree[a])
                .then_with(|| tables[a].qualified().cmp(&tables[b].qualified()))
        });
        neighbors.dedup();
    }
    let mut visited = vec![false; tables.len()];
    let mut bfs_order: Vec<usize> = Vec::with_capacity(order.len());
    for &seed in &order {
        if visited[seed] {
            continue;
        }
        visited[seed] = true;
        let mut queue = std::collections::VecDeque::from([seed]);
        while let Some(current) = queue.pop_front() {
            bfs_order.push(current);
            for &next in &adjacency[current] {
                if !visited[next] {
                    visited[next] = true;
                    queue.push_back(next);
                }
            }
        }
    }

    // Remap into the final graph.
    let mut remap: HashMap<usize, usize> = HashMap::new();
    let mut nodes = Vec::with_capacity(bfs_order.len());
    for &old in &bfs_order {
        remap.insert(old, nodes.len());
        nodes.push(tables[old].clone());
    }
    let mut edges = Vec::new();
    for rel in &rels {
        let (Some(&from), Some(&to)) = (remap.get(&rel.from), remap.get(&rel.to)) else {
            continue;
        };
        let from_cols: Vec<&str> = rel.pairs.iter().map(|(f, _)| f.as_str()).collect();
        let to_cols: Vec<&str> = rel.pairs.iter().map(|(_, t)| t.as_str()).collect();
        edges.push(ErEdge {
            from,
            to,
            label: format!(
                "{} ({} → {})",
                rel.constraint,
                from_cols.join(", "),
                to_cols.join(", ")
            ),
            pairs: rel.pairs.clone(),
        });
    }

    // Explicitly-selected tables with no kept FK edge trail the connected nodes
    // as edgeless boxes (deduped against tables already interned from rows).
    for (schema, table) in extra_isolated {
        let already = nodes
            .iter()
            .any(|n| &n.schema == schema && &n.table == table);
        if already {
            continue;
        }
        nodes.push(ErNode {
            schema: schema.clone(),
            table: table.clone(),
            columns_in_keys: Vec::new(),
            columns: Vec::new(),
        });
    }

    ErGraph {
        nodes,
        edges,
        truncated,
        total_tables,
        total_relationships,
    }
}

/// Attach the bulk catalog column lists (name, type, nullability, ordinal
/// order) to the graph's nodes; FK-participating columns are flagged so the
/// painter can emphasize them and anchor edges. Nodes without a matching
/// table in `rows` keep an empty list and fall back to the key-columns line.
pub fn attach_columns(graph: &mut ErGraph, rows: &[BulkColumnRow]) {
    use std::collections::HashMap;
    let mut by_table: HashMap<(&str, &str), Vec<&BulkColumnRow>> = HashMap::new();
    for row in rows {
        by_table
            .entry((row.schema.as_str(), row.object.as_str()))
            .or_default()
            .push(row);
    }
    for node in &mut graph.nodes {
        let Some(cols) = by_table.get(&(node.schema.as_str(), node.table.as_str())) else {
            continue;
        };
        node.columns = cols
            .iter()
            .map(|c| ErColumn {
                name: c.column.clone(),
                data_type: c.type_name.clone().unwrap_or_default(),
                nullable: c.nullable,
                in_key: node.columns_in_keys.iter().any(|k| k == &c.column),
            })
            .collect();
    }
}

/// Column rows a node paints: every column, plus a "+N more" line in the
/// pathological case where the safety ceiling clips the list. Zero when
/// column rows are hidden or unavailable.
pub(crate) fn node_row_count(node: &ErNode, show_columns: bool) -> usize {
    if !show_columns || node.columns.is_empty() {
        return 0;
    }
    let n = node.columns.len();
    if n > MAX_NODE_COLUMNS {
        MAX_NODE_COLUMNS + 1
    } else {
        n
    }
}

/// Node box height for a graph under the current column visibility: the
/// legacy fixed height when no column rows are shown, else tall enough for
/// the largest node's rows (uniform across nodes so the grid stays regular).
pub(crate) fn node_height_for(graph: &ErGraph, show_columns: bool) -> f32 {
    let rows = graph
        .nodes
        .iter()
        .map(|n| node_row_count(n, show_columns))
        .max()
        .unwrap_or(0);
    if rows == 0 {
        NODE_H
    } else {
        (NODE_HEADER_H + rows as f32 * NODE_COL_ROW_H + NODE_COL_PAD).max(NODE_H)
    }
}

/// The right-hand detail for a column row: its type, its nullability, or
/// both, according to what the view has switched on. Painted right-aligned in
/// the card so it never runs into the column name, however long that name is.
pub(crate) fn column_detail(column: &ErColumn, show_types: bool, show_nulls: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if show_types && !column.data_type.is_empty() {
        parts.push(&column.data_type);
    }
    if show_nulls {
        // Unknown nullability says nothing rather than guessing.
        match column.nullable {
            Some(true) => parts.push("NULL"),
            Some(false) => parts.push("NOT NULL"),
            None => {}
        }
    }
    parts.join(" ")
}

/// The painted row index of `column` within a node, when it is visible under
/// the [`MAX_NODE_COLUMNS`] cap — the anchor row for FK edges.
pub(crate) fn visible_column_row(node: &ErNode, column: &str) -> Option<usize> {
    let ix = node.columns.iter().position(|c| c.name == column)?;
    (ix < MAX_NODE_COLUMNS).then_some(ix)
}

/// Deterministic row-major grid: `ceil(sqrt(n))` columns, fixed cell pitch.
/// Position `i` is the top-left of node `i`'s box; the canvas size bounds
/// every box plus the margin.
/// Layout kinds in the order of the layout select's labels.
const LAYOUT_CHOICES: [LayoutKind; 2] = [LayoutKind::Grid, LayoutKind::Layered];

fn layout_kind_from_row(row: Option<usize>) -> LayoutKind {
    row.and_then(|row| LAYOUT_CHOICES.get(row).copied())
        .unwrap_or(LayoutKind::Grid)
}

pub fn layout_grid(node_count: usize, node_w: f32, node_h: f32) -> ErLayout {
    if node_count == 0 {
        return ErLayout {
            positions: Vec::new(),
            width: 0.0,
            height: 0.0,
        };
    }
    let cell_h = node_h + CELL_V_GAP;
    let cell_w = node_w + CELL_H_GAP;
    let cols = ((node_count as f32).sqrt().ceil() as usize).max(1);
    let rows = node_count.div_ceil(cols);
    let positions = (0..node_count)
        .map(|i| {
            let col = i % cols;
            let row = i / cols;
            (MARGIN + col as f32 * cell_w, MARGIN + row as f32 * cell_h)
        })
        .collect();
    ErLayout {
        positions,
        width: 2.0 * MARGIN + (cols - 1) as f32 * cell_w + node_w,
        height: 2.0 * MARGIN + (rows - 1) as f32 * cell_h + node_h,
    }
}

/// Case-insensitive substring match of the filter against `schema.table`.
/// An empty (or whitespace) filter matches everything.
pub fn node_matches_filter(node: &ErNode, filter: &str) -> bool {
    let needle = filter.trim().to_lowercase();
    needle.is_empty() || node.qualified().to_lowercase().contains(&needle)
}

/// Point where the segment from `center` toward `toward` crosses the border
/// of the `half`-extent rectangle centered at `center`. Clamped so a `toward`
/// inside the rectangle never overshoots it.
pub fn rect_border_point(center: (f32, f32), half: (f32, f32), toward: (f32, f32)) -> (f32, f32) {
    let dx = toward.0 - center.0;
    let dy = toward.1 - center.1;
    if dx == 0.0 && dy == 0.0 {
        return center;
    }
    let sx = if dx != 0.0 {
        half.0 / dx.abs()
    } else {
        f32::INFINITY
    };
    let sy = if dy != 0.0 {
        half.1 / dy.abs()
    } else {
        f32::INFINITY
    };
    let s = sx.min(sy).min(1.0);
    (center.0 + dx * s, center.1 + dy * s)
}

/// Truncate to `max_chars` characters (not bytes) with a trailing ellipsis —
/// safe on multi-byte input.
pub fn truncate_label(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// What the diagram asks of the host. Mirrors `ToolDialogEvent`'s style: the
/// dialog stays passive; `AppView` routes reveals and status text.
// The allow drops out once the host's open action subscribes to the dialog.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub enum ErDiagramEvent {
    /// The user clicked a table node — reveal it in the schema tree.
    RevealObject { schema: String, name: String },
    /// Surface a status-bar message.
    Status { level: StatusLevel, message: String },
    /// Dismiss the dialog.
    Close,
    /// Open the generated CREATE script in a new editor tab for review,
    /// bound to the connection and database the diagram was drawn from (not
    /// whatever the workbench has active by the time the script lands).
    ScriptToEditor {
        sql: String,
        profile: Box<sqlly_core::ConnectionProfile>,
    },
    /// Re-open the diagram as a standalone window carrying the current state.
    /// Routed through the host (not opened by the view itself) so `AppView`
    /// can subscribe to the new window's view — otherwise clicking a table in
    /// the popped-out window could never reveal it in the schema tree.
    PopOut {
        selection: Option<HashSet<TableKey>>,
        focus: Option<(String, String)>,
    },
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

pub struct ErDiagramView {
    context: ToolDialogContext,
    database: String,
    /// Modal overlay vs standalone resizable window.
    presentation: ErPresentation,
    /// Chrome geometry for the modal presentation (unused for `Window`).
    frame: DialogFrame,
    filter_input: Entity<InputState>,
    filter: String,
    loading: bool,
    error: Option<String>,
    /// Raw FK rows from the last fetch, retained so the display graph can be
    /// rebuilt against a changed table selection without a re-fetch.
    fk_rows: Vec<ForeignKeyRow>,
    /// Every table in the database (from `list_tables`), sorted, for the picker.
    all_tables: Vec<TableKey>,
    tables_loading: bool,
    /// Which tables to draw. `None` = the default FK-connected, capped set;
    /// `Some` = an explicit user pick (isolated tables allowed, no cap).
    selection: Option<HashSet<TableKey>>,
    /// Whether the table-picker panel is open.
    picker_open: bool,
    /// The kit list backing the picker: query input + virtualized checkbox
    /// rows ([`TablePickerDelegate`]).
    picker_list: Entity<ListState<TablePickerDelegate>>,
    graph: Option<ErGraph>,
    layout: ErLayout,
    layout_kind: LayoutKind,
    layout_select: Entity<SelectState<Vec<String>>>,
    /// Pan offset applied to the whole canvas, clamped to keep some content
    /// visible. Adjusted by scroll-wheel events and zoom-about.
    pan: (f32, f32),
    zoom: f32,
    drag: Option<DragState>,
    overrides: HashMap<usize, (f32, f32)>,
    focus: Option<usize>,
    visible: Option<Vec<usize>>,
    pending_focus: Option<(String, String)>,
    /// Last painted canvas bounds, recorded in the canvas prepaint so the
    /// mouse-down hit test and pan clamp can map window coordinates back to
    /// layout space (same trick as the editor's completion popup bounds).
    canvas_bounds: std::cell::Cell<Option<Bounds<Pixels>>>,
    /// Show column rows inside the table boxes (default on).
    show_columns: bool,
    /// Show each column's data type on its row.
    show_types: bool,
    /// Show each column's nullability (`NULL` / `NOT NULL`) on its row.
    show_nulls: bool,
    /// Draw FK edges (default on). Off leaves just the table boxes.
    show_fks: bool,
    /// Card width in canvas units. User-adjustable between [`MIN_NODE_W`] and
    /// [`MAX_NODE_W`]; the layout pitch follows it so widening spreads the
    /// grid rather than overlapping it.
    node_w: f32,
    /// Bulk catalog column rows from the last fetch, retained so graph
    /// rebuilds (picker changes) can re-attach without a re-fetch.
    column_rows: Vec<BulkColumnRow>,
    /// Accumulated shift+scroll travel toward the next 5% zoom step —
    /// high-resolution wheels must cross a full threshold per step.
    zoom_scroll_accum: f32,
    refresh_generation: u64,
    /// A "Script as CREATE" run is in flight; further clicks are ignored
    /// until it lands so one click opens one tab.
    scripting: bool,
    focus_handle: FocusHandle,
    _subscriptions: Vec<gpui::Subscription>,
}

struct ExportSnapshot {
    graph: ErGraph,
    layout: ErLayout,
    positions: Vec<(f32, f32)>,
}

/// One refresh's two fetches. `tables` is best-effort — a failure leaves the
/// picker's table list untouched rather than blanking the diagram.
struct FetchOutcome {
    fks: Result<Vec<ForeignKeyRow>, String>,
    tables: Result<Vec<TableKey>, String>,
    /// Best-effort: engines without a bulk column listing report an error and
    /// the boxes simply fall back to the key-columns line.
    columns: Result<Vec<BulkColumnRow>, String>,
}

impl EventEmitter<ErDiagramEvent> for ErDiagramView {}

impl ErDiagramView {
    /// The allow drops out once the host's open action constructs the dialog.
    #[allow(dead_code)]
    pub fn new(
        context: ToolDialogContext,
        database: String,
        pending_focus: Option<(String, String)>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        Self::new_with(
            context,
            database,
            pending_focus,
            ErPresentation::Modal,
            None,
            window,
            cx,
        )
    }

    /// Full constructor: choose the presentation and seed an initial table
    /// selection (used when popping the diagram out into its own window so the
    /// user's chosen table set carries over).
    #[allow(clippy::too_many_arguments)]
    pub fn new_with(
        context: ToolDialogContext,
        database: String,
        pending_focus: Option<(String, String)>,
        presentation: ErPresentation,
        initial_selection: Option<HashSet<TableKey>>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.er_diagram.filter.placeholder",
            ))
        });
        // List selection stays off: multi-select lives on the per-row
        // checkboxes, so there is no row highlight or arrow-key selection.
        let picker_view = cx.weak_entity();
        let picker_list = cx.new(|cx| {
            ListState::new(
                TablePickerDelegate {
                    view: picker_view,
                    entries: Vec::new(),
                    checked: HashSet::new(),
                    tables_loading: false,
                    filtered: Vec::new(),
                    query: String::new(),
                },
                window,
                cx,
            )
            .searchable(true)
            .selectable(false)
        });
        let layout_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    vec![
                        crate::localization::text("explorer.er_diagram.layout.grid"),
                        crate::localization::text("explorer.er_diagram.layout.layered"),
                    ]
                }),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let filter_subscription = cx.subscribe_in(
            &filter_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    let text = input.read(cx).value().to_string();
                    if this.filter != text {
                        this.filter = text;
                        cx.notify();
                    }
                }
            },
        );
        let layout_subscription = cx.subscribe_in(
            &layout_select,
            window,
            |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                if value.is_none() {
                    return;
                }
                let kind =
                    layout_kind_from_row(select.read(cx).selected_index(cx).map(|ix| ix.row));
                if this.layout_kind != kind {
                    this.layout_kind = kind;
                    this.relayout();
                    cx.notify();
                }
            },
        );
        let mut this = Self {
            context,
            database,
            presentation,
            frame: DialogFrame::new(Theme::scaled(960.0), Theme::scaled(640.0)),
            filter_input,
            filter: String::new(),
            loading: false,
            error: None,
            fk_rows: Vec::new(),
            all_tables: Vec::new(),
            tables_loading: false,
            selection: initial_selection,
            picker_open: false,
            picker_list,
            graph: None,
            layout: layout_grid(0, NODE_W, NODE_H),
            layout_kind: LayoutKind::Grid,
            layout_select,
            pan: (0.0, 0.0),
            zoom: 1.0,
            drag: None,
            overrides: HashMap::new(),
            focus: None,
            visible: None,
            pending_focus,
            canvas_bounds: std::cell::Cell::new(None),
            show_columns: true,
            show_types: false,
            show_nulls: false,
            show_fks: true,
            node_w: NODE_W,
            column_rows: Vec::new(),
            zoom_scroll_accum: 0.0,
            refresh_generation: 0,
            scripting: false,
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![filter_subscription, layout_subscription],
        };
        this.refresh(cx);
        this
    }

    /// Re-fetch the FK rows *and* the full table list on the dialog's captured
    /// route. A superseded refresh's outcome is dropped by the generation check
    /// in `apply_fetch`.
    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        self.loading = true;
        self.tables_loading = true;
        self.error = None;
        let services = self.context.services.clone();
        let profile = self.context.profile.clone();
        let database = self.database.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<FetchOutcome>(1);
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let fks = services
                        .metadata
                        .list_foreign_keys(&profile, &database)
                        .await
                        .map_err(|e| e.to_string());
                    // The table list is a convenience for the picker: a failure
                    // here must not blank the diagram, so it is folded into an
                    // Option and reported only via the FK error path if needed.
                    let tables = services
                        .metadata
                        .list_tables(&profile, &database, None)
                        .await
                        .map(|nodes| {
                            let mut keys: Vec<TableKey> = nodes
                                .into_iter()
                                .map(|n| (n.schema.unwrap_or_default(), n.name))
                                .collect();
                            keys.sort();
                            keys.dedup();
                            keys
                        })
                        .map_err(|e| e.to_string());
                    let columns = services
                        .metadata
                        .list_all_columns(&profile, &database)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx
                        .send(FetchOutcome {
                            fks,
                            tables,
                            columns,
                        })
                        .await;
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            if let Some(outcome) = rx.recv().await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        this.apply_fetch(generation, outcome, cx)
                    })
                    .ok();
            }
        })
        .detach();
        cx.notify();
    }

    fn apply_fetch(
        &mut self,
        generation: u64,
        outcome: FetchOutcome,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.refresh_generation != generation {
            return;
        }
        self.loading = false;
        self.tables_loading = false;
        if let Ok(tables) = outcome.tables {
            self.all_tables = tables;
        }
        self.column_rows = outcome.columns.unwrap_or_default();
        match outcome.fks {
            Ok(rows) => {
                self.fk_rows = rows;
                let graph = self.build_display_graph();
                self.apply_graph(graph, cx);
            }
            Err(message) => {
                self.error = Some(message.clone());
                self.graph = None;
                cx.emit(ErDiagramEvent::Status {
                    level: StatusLevel::Info,
                    message: localized_format(
                        "explorer.er_diagram.status.load_failed",
                        &[("error", &message)],
                    ),
                });
            }
        }
        cx.notify();
    }

    /// Build the graph to display from the retained FK rows, honoring the
    /// current table selection (or the default FK-connected set when `None`).
    fn build_display_graph(&self) -> ErGraph {
        let mut graph = match &self.selection {
            Some(sel) => build_graph_selected(&self.fk_rows, sel),
            None => build_graph(&self.fk_rows),
        };
        attach_columns(&mut graph, &self.column_rows);
        graph
    }

    fn apply_graph(&mut self, graph: ErGraph, cx: &mut gpui::Context<Self>) {
        self.overrides.clear();
        self.focus = None;
        self.visible = None;
        self.zoom = 1.0;
        self.pan = (0.0, 0.0);
        self.drag = None;
        self.error = None;
        self.layout = self.compute_layout(&graph);
        self.graph = Some(graph);
        self.apply_pending_focus(cx);
    }

    fn apply_pending_focus(&mut self, cx: &mut gpui::Context<Self>) {
        let Some((schema, table)) = self.pending_focus.clone() else {
            return;
        };
        let Some(graph) = self.graph.as_ref() else {
            return;
        };
        let exact = graph
            .nodes
            .iter()
            .position(|n| n.schema == schema && n.table == table);
        let found = exact.or_else(|| {
            graph.nodes.iter().position(|n| {
                n.schema.eq_ignore_ascii_case(&schema) && n.table.eq_ignore_ascii_case(&table)
            })
        });
        if let Some(i) = found {
            self.focus = Some(i);
            self.visible = Some(neighbourhood(graph, i));
            let graph = graph.clone();
            self.layout = self.compute_layout(&graph);
            self.fit_now();
        } else {
            cx.emit(ErDiagramEvent::Status {
                level: StatusLevel::Info,
                message: localized_format(
                    "explorer.er_diagram.status.focus_without_foreign_keys",
                    &[("table", &table)],
                ),
            });
        }
    }

    fn compute_layout(&self, graph: &ErGraph) -> ErLayout {
        let node_h = node_height_for(graph, self.show_columns);
        let node_w = self.node_w;
        if let Some(visible) = &self.visible {
            let (sub, map) = subgraph(graph, visible);
            let packed = match self.layout_kind {
                LayoutKind::Grid => layout_grid(sub.nodes.len(), node_w, node_h),
                LayoutKind::Layered => layout_layered(&sub, node_w, node_h),
            };
            let full = match self.layout_kind {
                LayoutKind::Grid => layout_grid(graph.nodes.len(), node_w, node_h),
                LayoutKind::Layered => layout_layered(graph, node_w, node_h),
            };
            layout_subset(&full, &map, &packed)
        } else {
            match self.layout_kind {
                LayoutKind::Grid => layout_grid(graph.nodes.len(), node_w, node_h),
                LayoutKind::Layered => layout_layered(graph, node_w, node_h),
            }
        }
    }

    fn relayout(&mut self) {
        self.overrides.clear();
        let Some(graph) = self.graph.clone() else {
            return;
        };
        self.layout = self.compute_layout(&graph);
    }

    fn show_all(&mut self) {
        self.visible = None;
        self.focus = None;
        self.pending_focus = None;
        self.relayout();
        self.fit_now();
    }

    fn reset_layout(&mut self) {
        self.overrides.clear();
        self.relayout();
        self.fit_now();
    }

    fn fit_now(&mut self) {
        let view = match self.canvas_bounds.get() {
            Some(b) => (f32::from(b.size.width), f32::from(b.size.height)),
            None => (800.0, 480.0),
        };
        let (zoom, pan) = fit_to_view(&self.content_layout(), view);
        self.zoom = zoom;
        self.pan = pan;
        self.clamp_pan();
    }

    /// Node box height under the current graph and column visibility.
    fn current_node_h(&self) -> f32 {
        self.graph
            .as_ref()
            .map(|g| node_height_for(g, self.show_columns))
            .unwrap_or(NODE_H)
    }

    fn content_layout(&self) -> ErLayout {
        let node_h = self.current_node_h();
        let mut width = self.layout.width;
        let mut height = self.layout.height;
        for &(x, y) in self.overrides.values() {
            width = width.max(x + self.node_w);
            height = height.max(y + node_h);
        }
        ErLayout {
            positions: self.layout.positions.clone(),
            width,
            height,
        }
    }

    fn position_of(&self, i: usize) -> (f32, f32) {
        self.overrides.get(&i).copied().unwrap_or_else(|| {
            self.layout
                .positions
                .get(i)
                .copied()
                .unwrap_or((MARGIN, MARGIN))
        })
    }

    fn node_visible(&self, i: usize) -> bool {
        self.visible
            .as_ref()
            .is_none_or(|visible| visible.contains(&i))
    }

    fn canvas_local(&self, position: Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.canvas_bounds.get()?;
        let zoom = self.zoom.max(0.001);
        let lx = (f32::from(position.x - bounds.origin.x) - self.pan.0) / zoom;
        let ly = (f32::from(position.y - bounds.origin.y) - self.pan.1) / zoom;
        Some((lx, ly))
    }

    fn canvas_centre(&self) -> (f32, f32) {
        match self.canvas_bounds.get() {
            Some(b) => (
                f32::from(b.size.width) / 2.0,
                f32::from(b.size.height) / 2.0,
            ),
            None => (400.0, 240.0),
        }
    }

    fn set_zoom_about(&mut self, new: f32, anchor: (f32, f32)) {
        let new = clamp_zoom(new);
        self.pan = zoom_about(anchor, self.pan, self.zoom, new);
        self.zoom = new;
        self.clamp_pan();
    }

    /// Dismiss the surface. A modal asks the host to drop the overlay; a
    /// standalone window closes itself (there is no host subscription).
    fn close(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        match self.presentation {
            ErPresentation::Window => window.remove_window(),
            ErPresentation::Modal => cx.emit(ErDiagramEvent::Close),
        }
    }

    // -- Table picker -------------------------------------------------------

    /// Currently-displayed set of tables (the graph's nodes).
    fn displayed_tables(&self) -> HashSet<TableKey> {
        self.graph
            .as_ref()
            .map(|g| {
                g.nodes
                    .iter()
                    .map(|n| (n.schema.clone(), n.table.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn is_table_shown(&self, key: &TableKey) -> bool {
        match &self.selection {
            Some(sel) => sel.contains(key),
            None => self.graph.as_ref().is_some_and(|g| {
                g.nodes
                    .iter()
                    .any(|n| (&n.schema, &n.table) == (&key.0, &key.1))
            }),
        }
    }

    /// Rebuild the display graph from the current `selection` and reset the
    /// view onto it, then fit. Clears any focus-subset — the picker is the
    /// authoritative table filter once the user drives it.
    fn apply_selection_change(&mut self, cx: &mut gpui::Context<Self>) {
        self.pending_focus = None;
        let graph = self.build_display_graph();
        self.apply_graph(graph, cx);
        self.fit_now();
        cx.notify();
    }

    /// Toggle one table in/out of the shown set. The first toggle promotes the
    /// implicit default set to an explicit selection so subsequent edits are
    /// relative to what was on screen.
    fn toggle_table(&mut self, key: TableKey, cx: &mut gpui::Context<Self>) {
        let mut sel = self
            .selection
            .clone()
            .unwrap_or_else(|| self.displayed_tables());
        if !sel.remove(&key) {
            sel.insert(key);
        }
        self.selection = Some(sel);
        self.apply_selection_change(cx);
    }

    fn select_all_tables(&mut self, cx: &mut gpui::Context<Self>) {
        // Use the picker's entry set (all DB tables unioned with what's shown),
        // so "All" still works if `list_tables` failed and only the FK tables
        // are known.
        self.selection = Some(self.picker_entries().into_iter().collect());
        self.apply_selection_change(cx);
    }

    fn select_no_tables(&mut self, cx: &mut gpui::Context<Self>) {
        self.selection = Some(HashSet::new());
        self.apply_selection_change(cx);
    }

    /// Reset to the default FK-connected view (drops any explicit selection).
    fn select_default_tables(&mut self, cx: &mut gpui::Context<Self>) {
        self.selection = None;
        self.apply_selection_change(cx);
    }

    /// Toggle column rows. Node heights change with them, so the layout is
    /// recomputed — pan, zoom, and user-dragged positions stay put.
    fn toggle_columns(&mut self, cx: &mut gpui::Context<Self>) {
        self.show_columns = !self.show_columns;
        if let Some(graph) = self.graph.clone() {
            self.layout = self.compute_layout(&graph);
            self.clamp_pan();
        }
        cx.notify();
    }

    /// Resize every table card. The layout pitch follows the width, so the
    /// grid spreads instead of overlapping — which means the positions have to
    /// be recomputed. Cards the user has dragged keep their own positions.
    fn set_node_width(&mut self, width: f32, cx: &mut gpui::Context<Self>) {
        let clamped = width.clamp(MIN_NODE_W, MAX_NODE_W);
        if (clamped - self.node_w).abs() < f32::EPSILON {
            return;
        }
        self.node_w = clamped;
        if let Some(graph) = self.graph.clone() {
            self.layout = self.compute_layout(&graph);
        }
        self.clamp_pan();
        cx.notify();
    }

    fn toggle_picker(&mut self, cx: &mut gpui::Context<Self>) {
        self.picker_open = !self.picker_open;
        cx.notify();
    }

    /// Ask the host to re-open the diagram as its own resizable, movable OS
    /// window carrying the current state. The host opens the window (so it can
    /// subscribe to the new view's reveal/status events) and drops this modal.
    /// An explicit selection carries over verbatim; the default FK-connected
    /// view stays `None` so the window keeps its cap-and-truncation semantics.
    fn pop_out(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ErDiagramEvent::PopOut {
            selection: self.selection.clone(),
            focus: self.pending_focus.clone(),
        });
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            if self.picker_open {
                self.picker_open = false;
                cx.stop_propagation();
                cx.notify();
                return;
            }
            self.close(window, cx);
            // The dialog chrome's own escape handler sits on an ancestor of
            // this wrapper; stop here so a modal close doesn't fire twice.
            cx.stop_propagation();
            return;
        }
        if !event.keystroke.modifiers.platform {
            return;
        }
        match event.keystroke.key.as_str() {
            "=" | "+" => {
                self.set_zoom_about(self.zoom + ZOOM_STEP, self.canvas_centre());
                cx.stop_propagation();
                cx.notify();
            }
            "-" => {
                self.set_zoom_about(self.zoom - ZOOM_STEP, self.canvas_centre());
                cx.stop_propagation();
                cx.notify();
            }
            "0" => {
                self.set_zoom_about(1.0, self.canvas_centre());
                cx.stop_propagation();
                cx.notify();
            }
            _ => {}
        }
    }

    /// Map a window-space point into layout space via the recorded canvas
    /// bounds, pan, and zoom, then hit-test node boxes (topmost drawn last →
    /// scan in reverse).
    fn node_at(&self, position: Point<Pixels>) -> Option<usize> {
        let graph = self.graph.as_ref()?;
        let (lx, ly) = self.canvas_local(position)?;
        let node_h = self.current_node_h();
        (0..graph.nodes.len().min(self.layout.positions.len()))
            .rev()
            .find(|&i| {
                if !self.node_visible(i) {
                    return false;
                }
                let (x, y) = self.position_of(i);
                lx >= x && lx <= x + self.node_w && ly >= y && ly <= y + node_h
            })
    }

    fn on_canvas_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(index) = self.node_at(event.position) else {
            return;
        };
        let Some((lx, ly)) = self.canvas_local(event.position) else {
            return;
        };
        let (x, y) = self.position_of(index);
        self.drag = Some(DragState {
            node: index,
            grab_offset: (lx - x, ly - y),
            moved: false,
            start: event.position,
        });
        cx.notify();
    }

    fn on_canvas_mouse_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if event.pressed_button != Some(MouseButton::Left) {
            return;
        }
        let Some(drag) = self.drag.as_ref() else {
            return;
        };
        let node = drag.node;
        let grab = drag.grab_offset;
        let start = drag.start;
        let Some((mx, my)) = self.canvas_local(event.position) else {
            return;
        };
        let dx = f32::from(event.position.x - start.x);
        let dy = f32::from(event.position.y - start.y);
        let moved = drag.moved || dx * dx + dy * dy >= 9.0;
        if !moved {
            return;
        }
        let x = (mx - grab.0).max(0.0);
        let y = (my - grab.1).max(0.0);
        self.overrides.insert(node, (x, y));
        if let Some(drag) = self.drag.as_mut() {
            drag.moved = true;
        }
        self.clamp_pan();
        cx.stop_propagation();
        cx.notify();
    }

    fn on_canvas_mouse_up(
        &mut self,
        _event: &gpui::MouseUpEvent,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        if drag.moved {
            return;
        }
        if let Some(graph) = &self.graph
            && let Some(node) = graph.nodes.get(drag.node)
        {
            cx.emit(ErDiagramEvent::RevealObject {
                schema: node.schema.clone(),
                name: node.table.clone(),
            });
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &gpui::ScrollEvent,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if event.modifiers.shift {
            // Shift+scroll zooms in deliberate 5% steps: scroll travel
            // accumulates and a full threshold advances one step, so a
            // high-resolution wheel (Magic Mouse) can't rocket the zoom.
            let delta = event.delta.pixel_delta(px(24.0));
            // macOS reports shift+vertical-scroll on the horizontal axis;
            // take whichever axis actually moved.
            let dy = f32::from(delta.y);
            let dx = f32::from(delta.x);
            self.zoom_scroll_accum += if dy.abs() >= dx.abs() { dy } else { dx };
            let steps = (self.zoom_scroll_accum / ZOOM_SCROLL_STEP).trunc();
            if steps != 0.0 {
                self.zoom_scroll_accum -= steps * ZOOM_SCROLL_STEP;
                // Same direction convention as ⌘+scroll: scrolling up zooms in.
                let new = clamp_zoom(self.zoom * 1.05f32.powi(-steps as i32));
                let origin = self
                    .canvas_bounds
                    .get()
                    .map(|b| b.origin)
                    .unwrap_or_else(|| point(px(0.0), px(0.0)));
                let anchor = (
                    f32::from(event.position.x - origin.x),
                    f32::from(event.position.y - origin.y),
                );
                self.set_zoom_about(new, anchor);
            }
        } else if event.modifiers.platform {
            let delta = event.delta.pixel_delta(px(24.0));
            let new = clamp_zoom(self.zoom * (1.0 - f32::from(delta.y) / 200.0));
            let origin = self
                .canvas_bounds
                .get()
                .map(|b| b.origin)
                .unwrap_or_else(|| point(px(0.0), px(0.0)));
            let anchor = (
                f32::from(event.position.x - origin.x),
                f32::from(event.position.y - origin.y),
            );
            self.set_zoom_about(new, anchor);
        } else {
            let delta = event.delta.pixel_delta(px(24.0));
            self.pan.0 += f32::from(delta.x);
            self.pan.1 += f32::from(delta.y);
            self.clamp_pan();
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn on_pinch(
        &mut self,
        event: &gpui::PinchEvent,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let new = clamp_zoom(self.zoom * (1.0 + event.delta));
        let origin = self
            .canvas_bounds
            .get()
            .map(|b| b.origin)
            .unwrap_or_else(|| point(px(0.0), px(0.0)));
        let anchor = (
            f32::from(event.position.x - origin.x),
            f32::from(event.position.y - origin.y),
        );
        self.set_zoom_about(new, anchor);
        cx.stop_propagation();
        cx.notify();
    }

    /// Keep the canvas content reachable: pan never exposes space left/above
    /// the content, and never scrolls the content fully out of view. When the
    /// scaled content is smaller than the view, pan may be positive so Fit
    /// can centre it.
    fn clamp_pan(&mut self) {
        let (view_w, view_h) = match self.canvas_bounds.get() {
            Some(b) => (f32::from(b.size.width), f32::from(b.size.height)),
            None => (800.0, 480.0),
        };
        let content = self.content_layout();
        let content_w = content.width * self.zoom;
        let content_h = content.height * self.zoom;
        self.pan.0 = if content_w <= view_w {
            self.pan.0.clamp(0.0, view_w - content_w)
        } else {
            self.pan.0.clamp(view_w - content_w, 0.0)
        };
        self.pan.1 = if content_h <= view_h {
            self.pan.1.clamp(0.0, view_h - content_h)
        } else {
            self.pan.1.clamp(view_h - content_h, 0.0)
        };
    }

    fn build_paint(&self) -> ErPaint {
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let filter_active = !self.filter.trim().is_empty();
        let zoom = self.zoom;
        if let Some(graph) = &self.graph {
            let matches: Vec<bool> = graph
                .nodes
                .iter()
                .map(|n| node_matches_filter(n, &self.filter))
                .collect();
            let mut paint_index: HashMap<usize, usize> = HashMap::new();
            for (i, node) in graph.nodes.iter().enumerate() {
                if !self.node_visible(i) {
                    continue;
                }
                let (x, y) = self.position_of(i);
                paint_index.insert(i, nodes.len());
                let column_rows = self.show_columns && !node.columns.is_empty();
                let rows = if column_rows {
                    let mut rows: Vec<NodeRowPaint> = node
                        .columns
                        .iter()
                        .take(MAX_NODE_COLUMNS)
                        .map(|c| NodeRowPaint {
                            text: truncate_label(&c.name, 32),
                            detail: column_detail(c, self.show_types, self.show_nulls),
                            emphasized: c.in_key,
                        })
                        .collect();
                    if node.columns.len() > MAX_NODE_COLUMNS {
                        let count = (node.columns.len() - MAX_NODE_COLUMNS).to_string();
                        rows.push(NodeRowPaint {
                            text: localized_format(
                                "explorer.er_diagram.table.more_columns",
                                &[("count", &count)],
                            ),
                            detail: String::new(),
                            emphasized: false,
                        });
                    }
                    rows
                } else {
                    let joined = truncate_label(&node.columns_in_keys.join(", "), 34);
                    if joined.is_empty() {
                        Vec::new()
                    } else {
                        vec![NodeRowPaint {
                            text: joined,
                            detail: String::new(),
                            emphasized: false,
                        }]
                    }
                };
                nodes.push(NodePaint {
                    x: x * zoom,
                    y: y * zoom,
                    title: truncate_label(&node.qualified(), 26),
                    rows,
                    column_rows,
                    dimmed: filter_active && !matches[i],
                    matched: self.focus == Some(i) || (filter_active && matches[i]),
                });
            }
            if self.show_fks {
                for edge in &graph.edges {
                    let (Some(&from), Some(&to)) =
                        (paint_index.get(&edge.from), paint_index.get(&edge.to))
                    else {
                        continue;
                    };
                    // Point the edge at the exact column pair when both the
                    // column display and the columns themselves are visible.
                    let (from_row, to_row) = if self.show_columns {
                        let pair = edge.pairs.first();
                        let from_row = pair.and_then(|(from_col, _)| {
                            visible_column_row(&graph.nodes[edge.from], from_col)
                        });
                        let to_row = pair.and_then(|(_, to_col)| {
                            visible_column_row(&graph.nodes[edge.to], to_col)
                        });
                        (from_row, to_row)
                    } else {
                        (None, None)
                    };
                    edges.push(EdgePaint {
                        from,
                        to,
                        dimmed: filter_active && !(matches[edge.from] && matches[edge.to]),
                        from_row,
                        to_row,
                    });
                }
            }
        }
        ErPaint {
            nodes,
            edges,
            pan: self.pan,
            zoom,
            node_w: self.node_w * zoom,
            node_h: self.current_node_h() * zoom,
            header_h: NODE_HEADER_H * zoom,
            col_row_h: NODE_COL_ROW_H * zoom,
            background: Theme::panel_bg(),
            node_bg: Theme::popup_bg(),
            node_border: Theme::border(),
            accent: Theme::accent(),
            text: Theme::text(),
            text_dim: Theme::text_dim(),
            font_size: Theme::font_size() * zoom,
        }
    }

    fn svg_style(&self) -> ErSvgStyle {
        ErSvgStyle {
            background: Theme::panel_bg(),
            node_bg: Theme::popup_bg(),
            node_border: Theme::border(),
            accent: Theme::accent(),
            text: Theme::text(),
            text_dim: Theme::text_dim(),
            font_size: Theme::font_size(),
            node_h: self.current_node_h(),
            node_w: self.node_w,
        }
    }

    fn export_graph(&self) -> Option<ExportSnapshot> {
        let graph = self.graph.as_ref()?;
        if let Some(visible) = &self.visible {
            let (sub, map) = subgraph(graph, visible);
            let positions: Vec<(f32, f32)> = map.iter().map(|&i| self.position_of(i)).collect();
            let node_h = self.current_node_h();
            let mut width = 0.0f32;
            let mut height = 0.0f32;
            for &(x, y) in &positions {
                width = width.max(x + self.node_w);
                height = height.max(y + node_h);
            }
            let layout = ErLayout {
                positions: positions.clone(),
                width: width.max(1.0),
                height: height.max(1.0),
            };
            Some(ExportSnapshot {
                graph: sub,
                layout,
                positions,
            })
        } else {
            let positions: Vec<(f32, f32)> = (0..graph.nodes.len())
                .map(|i| self.position_of(i))
                .collect();
            Some(ExportSnapshot {
                graph: graph.clone(),
                layout: self.content_layout(),
                positions,
            })
        }
    }

    fn export_mermaid(&self) -> Option<String> {
        Some(er_mermaid(&self.export_graph()?.graph))
    }

    fn export_dbml(&self) -> Option<String> {
        Some(er_dbml(&self.export_graph()?.graph))
    }

    fn export_svg(&self) -> Option<String> {
        let snapshot = self.export_graph()?;
        Some(er_svg(
            &snapshot.graph,
            &snapshot.layout,
            &|i| {
                snapshot
                    .positions
                    .get(i)
                    .copied()
                    .unwrap_or((MARGIN, MARGIN))
            },
            &self.svg_style(),
        ))
    }

    /// "Script as CREATE": dependency-ordered CREATE TABLE DDL for exactly the
    /// tables on canvas (the picker selection, or the focus neighbourhood while
    /// one is active), opened in a new editor tab for review. Built from live
    /// catalog metadata by the dump pipeline's DDL fetcher — this is
    /// script-from-metadata, not a visual modeler: the canvas stays read-only.
    fn script_as_create(&mut self, cx: &mut gpui::Context<Self>) {
        if self.scripting {
            return;
        }
        let Some(graph) = &self.graph else {
            cx.emit(ErDiagramEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.er_diagram.script.not_loaded"),
            });
            return;
        };
        let tables = tables_in_view(graph, &self.visible);
        if tables.is_empty() {
            cx.emit(ErDiagramEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("explorer.er_diagram.script.no_tables"),
            });
            return;
        }
        let count = tables.len().to_string();
        cx.emit(ErDiagramEvent::Status {
            level: StatusLevel::Info,
            message: localized_format("explorer.er_diagram.script.started", &[("count", &count)]),
        });
        self.scripting = true;
        cx.notify();
        let services = self.context.services.clone();
        let profile = self.context.profile.clone();
        let database = self.database.clone();
        let mut tab_profile = profile.clone();
        if !database.is_empty() {
            tab_profile.database = Some(database.clone());
        }
        let fks = self.fk_rows.clone();
        // Same off-executor pattern as refresh(): build the script on the
        // tokio runtime, land the result back on the view.
        let (tx, mut rx) =
            tokio::sync::mpsc::channel::<Result<sqlly_services::alter::CreateScript, String>>(1);
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let result = sqlly_services::alter::script_tables_as_create(
                        services.queries.as_ref(),
                        &profile,
                        &database,
                        &tables,
                        &fks,
                    )
                    .await
                    .map_err(|e| e.to_string());
                    let _ = tx.send(result).await;
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            // A dropped sender (the task died) still clears the in-flight
            // flag, so the button never stays stuck on "Scripting…".
            let result = rx.recv().await.unwrap_or_else(|| {
                Err(crate::localization::text(
                    "explorer.er_diagram.script.task_stopped",
                ))
            });
            {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        this.scripting = false;
                        cx.notify();
                        match result {
                            Ok(script) => {
                                let count = script.tables_scripted.to_string();
                                let mut notes = Vec::new();
                                if script.deferred_foreign_keys > 0 {
                                    let deferred = script.deferred_foreign_keys.to_string();
                                    notes.push(localized_format(
                                        "explorer.er_diagram.script.deferred_foreign_keys",
                                        &[("count", &deferred)],
                                    ));
                                }
                                if !script.warnings.is_empty() {
                                    let warnings = script.warnings.len().to_string();
                                    notes.push(localized_format(
                                        "explorer.er_diagram.script.warnings",
                                        &[("count", &warnings)],
                                    ));
                                }
                                let notes = if notes.is_empty() {
                                    String::new()
                                } else {
                                    format!(" ({})", notes.join("; "))
                                };
                                let level = if script.warnings.is_empty() {
                                    StatusLevel::Success
                                } else {
                                    StatusLevel::Warning
                                };
                                cx.emit(ErDiagramEvent::ScriptToEditor {
                                    sql: script.sql,
                                    profile: Box::new(tab_profile),
                                });
                                cx.emit(ErDiagramEvent::Status {
                                    level,
                                    message: localized_format(
                                        "explorer.er_diagram.script.completed",
                                        &[("count", &count), ("notes", &notes)],
                                    ),
                                });
                            }
                            Err(message) => cx.emit(ErDiagramEvent::Status {
                                level: StatusLevel::Error,
                                message: localized_format(
                                    "explorer.er_diagram.script.failed",
                                    &[("error", &message)],
                                ),
                            }),
                        }
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn copy_export(&mut self, kind: &str, cx: &mut gpui::Context<Self>) {
        let (text, label) = match kind {
            "svg" => (
                self.export_svg(),
                crate::localization::text("explorer.er_diagram.export.copied_svg"),
            ),
            "mermaid" => (
                self.export_mermaid(),
                crate::localization::text("explorer.er_diagram.export.copied_mermaid"),
            ),
            "dbml" => (
                self.export_dbml(),
                crate::localization::text("explorer.er_diagram.export.copied_dbml"),
            ),
            _ => return,
        };
        let Some(text) = text else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        cx.emit(ErDiagramEvent::Status {
            level: StatusLevel::Info,
            message: label.to_string(),
        });
    }

    fn save_export(&mut self, kind: &str, cx: &mut gpui::Context<Self>) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (text, filename) = match kind {
                "svg" => (self.export_svg(), "relationships.svg"),
                "mermaid" => (self.export_mermaid(), "relationships.mmd"),
                "dbml" => (self.export_dbml(), "relationships.dbml"),
                _ => return,
            };
            // No graph to write: still loading, or the last refresh failed.
            // The Export menu renders either way, so the click has to say
            // something rather than look like a dead button.
            let Some(text) = text else {
                cx.emit(ErDiagramEvent::Status {
                    level: StatusLevel::Info,
                    message: crate::localization::text("explorer.er_diagram.export.not_loaded"),
                });
                return;
            };
            let rx = cx.prompt_for_new_path(Path::new("."), Some(filename));
            let async_cx = cx.to_async();
            let weak_self = cx.weak_entity();
            cx.spawn(async move |_this, _cx| {
                // A cancelled save dialog stays silent; the user already knows.
                let Ok(Ok(Some(path))) = rx.await else {
                    return;
                };
                let status = match std::fs::write(&path, text.as_bytes()) {
                    Ok(()) => {
                        let path = path.display().to_string();
                        ErDiagramEvent::Status {
                            level: StatusLevel::Success,
                            message: localized_format(
                                "explorer.er_diagram.export.saved",
                                &[("path", &path)],
                            ),
                        }
                    }
                    Err(error) => {
                        tracing::warn!("relationships export save failed: {error}");
                        let error = error.to_string();
                        ErDiagramEvent::Status {
                            level: StatusLevel::Error,
                            message: localized_format(
                                "explorer.er_diagram.export.save_failed",
                                &[("error", &error)],
                            ),
                        }
                    }
                };
                let mut cx = async_cx.clone();
                weak_self.update(&mut cx, |_this, cx| cx.emit(status)).ok();
            })
            .detach();
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (kind, cx);
        }
    }

    fn title_text(&self) -> String {
        if let (Some(graph), Some(i)) = (&self.graph, self.focus)
            && let Some(node) = graph.nodes.get(i)
        {
            let table = node.qualified();
            return localized_format(
                "explorer.er_diagram.window.focus_title",
                &[("database", &self.database), ("table", &table)],
            );
        }
        localized_format(
            "explorer.er_diagram.window.database_title",
            &[("database", &self.database)],
        )
    }

    fn render_toolbar(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let percent = (self.zoom * 100.0).round() as u32;
        let export = cx.entity();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(4.0)))
            .child(
                Button::new("er-zoom-out")
                    .small()
                    .ghost()
                    .label("−")
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.set_zoom_about(this.zoom - ZOOM_STEP, this.canvas_centre());
                        cx.notify();
                    })),
            )
            .child(
                Button::new("er-zoom-in")
                    .small()
                    .ghost()
                    .label("+")
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.set_zoom_about(this.zoom + ZOOM_STEP, this.canvas_centre());
                        cx.notify();
                    })),
            )
            .child(
                Button::new("er-zoom-100")
                    .small()
                    .ghost()
                    .label(format!("{percent}%"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.set_zoom_about(1.0, this.canvas_centre());
                        cx.notify();
                    })),
            )
            .child(
                Button::new("er-fit")
                    .small()
                    .ghost()
                    .label(crate::localization::text("explorer.er_diagram.toolbar.fit"))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.fit_now();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "explorer.er_diagram.toolbar.layout",
                            )),
                    )
                    .child(
                        div()
                            .w(px(s(128.0)))
                            .child(Select::new(&self.layout_select)),
                    ),
            )
            .child(
                Button::new("er-reset-layout")
                    .small()
                    .ghost()
                    .label(crate::localization::text(
                        "explorer.er_diagram.toolbar.reset_layout",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.reset_layout();
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(2.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .mr(px(s(2.0)))
                            .child(crate::localization::text(
                                "explorer.er_diagram.toolbar.width",
                            )),
                    )
                    .child(
                        Button::new("er-narrower")
                            .small()
                            .ghost()
                            .label("−")
                            .tooltip(crate::localization::text(
                                "explorer.er_diagram.toolbar.narrower_tooltip",
                            ))
                            .disabled(self.node_w <= MIN_NODE_W)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.set_node_width(this.node_w - NODE_W_STEP, cx);
                            })),
                    )
                    .child(
                        Button::new("er-wider")
                            .small()
                            .ghost()
                            .label("+")
                            .tooltip(crate::localization::text(
                                "explorer.er_diagram.toolbar.wider_tooltip",
                            ))
                            .disabled(self.node_w >= MAX_NODE_W)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.set_node_width(this.node_w + NODE_W_STEP, cx);
                            })),
                    ),
            )
            .child(
                Button::new("er-script-create")
                    .small()
                    .ghost()
                    .label(if self.scripting {
                        crate::localization::text("explorer.er_diagram.script.in_progress")
                    } else {
                        crate::localization::text("explorer.er_diagram.script.button")
                    })
                    .disabled(self.scripting)
                    .tooltip(crate::localization::text(
                        "explorer.er_diagram.script.tooltip",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.script_as_create(cx);
                    })),
            )
            .child(
                Button::new("er-export")
                    .small()
                    .ghost()
                    .label(crate::localization::text(
                        "explorer.er_diagram.export.button",
                    ))
                    .dropdown_menu(move |menu, _, _| {
                        let copy_svg = export.clone();
                        let save_svg = export.clone();
                        let copy_mmd = export.clone();
                        let save_mmd = export.clone();
                        let copy_dbml = export.clone();
                        let save_dbml = export.clone();
                        let menu = menu
                            .item(
                                PopupMenuItem::new(crate::localization::text(
                                    "explorer.er_diagram.export.copy_svg",
                                ))
                                .on_click(
                                    move |_ev, _window, app| {
                                        copy_svg
                                            .update(app, |this, cx| this.copy_export("svg", cx));
                                    },
                                ),
                            )
                            .item(
                                PopupMenuItem::new(crate::localization::text(
                                    "explorer.er_diagram.export.copy_mermaid",
                                ))
                                .on_click(
                                    move |_ev, _window, app| {
                                        copy_mmd.update(app, |this, cx| {
                                            this.copy_export("mermaid", cx)
                                        });
                                    },
                                ),
                            )
                            .item(
                                PopupMenuItem::new(crate::localization::text(
                                    "explorer.er_diagram.export.copy_dbml",
                                ))
                                .on_click(
                                    move |_ev, _window, app| {
                                        copy_dbml
                                            .update(app, |this, cx| this.copy_export("dbml", cx));
                                    },
                                ),
                            );
                        #[cfg(not(target_arch = "wasm32"))]
                        {
                            menu.separator()
                                .item(
                                    PopupMenuItem::new(crate::localization::text(
                                        "explorer.er_diagram.export.save_svg",
                                    ))
                                    .on_click(
                                        move |_ev, _window, app| {
                                            save_svg.update(app, |this, cx| {
                                                this.save_export("svg", cx)
                                            });
                                        },
                                    ),
                                )
                                .item(
                                    PopupMenuItem::new(crate::localization::text(
                                        "explorer.er_diagram.export.save_mermaid",
                                    ))
                                    .on_click(
                                        move |_ev, _window, app| {
                                            save_mmd.update(app, |this, cx| {
                                                this.save_export("mermaid", cx)
                                            });
                                        },
                                    ),
                                )
                                .item(
                                    PopupMenuItem::new(crate::localization::text(
                                        "explorer.er_diagram.export.save_dbml",
                                    ))
                                    .on_click(
                                        move |_ev, _window, app| {
                                            save_dbml.update(app, |this, cx| {
                                                this.save_export("dbml", cx)
                                            });
                                        },
                                    ),
                                )
                        }
                        #[cfg(target_arch = "wasm32")]
                        {
                            let _ = (save_svg, save_mmd, save_dbml);
                            menu
                        }
                    }),
            )
            .into_any_element()
    }

    #[cfg(test)]
    pub(crate) fn set_zoom_for_test(&mut self, zoom: f32) {
        self.zoom = clamp_zoom(zoom);
    }

    #[cfg(test)]
    pub(crate) fn apply_graph_for_test(&mut self, graph: ErGraph, cx: &mut gpui::Context<Self>) {
        self.apply_graph(graph, cx);
    }

    #[cfg(test)]
    pub(crate) fn export_text_for_test(&self, kind: &str) -> String {
        match kind {
            "mermaid" => self.export_mermaid().unwrap_or_default(),
            "dbml" => self.export_dbml().unwrap_or_default(),
            "svg" => self.export_svg().unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn render_canvas(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let paint = self.build_paint();
        let entity = cx.entity();
        div()
            .id("er-diagram-canvas")
            .size_full()
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_canvas_mouse_down))
            .on_mouse_move(cx.listener(Self::on_canvas_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_canvas_mouse_up))
            .on_scroll(cx.listener(Self::on_scroll_wheel))
            .on_pinch(cx.listener(Self::on_pinch))
            .child(
                canvas(
                    move |bounds, _window, cx| {
                        entity.read(cx).canvas_bounds.set(Some(bounds));
                        paint
                    },
                    move |bounds, paint, window, cx| paint_er(&paint, bounds, window, cx),
                )
                .size_full(),
            )
            .into_any_element()
    }

    fn render_body(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let message = |text: String, color: Hsla| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p(px(s(16.0)))
                .text_color(color)
                .text_size(px(Theme::font_size_ratio(0.93)))
                .whitespace_normal()
                .child(text)
                .into_any_element()
        };
        if self.loading {
            message(
                crate::localization::text("explorer.er_diagram.canvas.loading"),
                Theme::text_dim(),
            )
        } else if let Some(error) = &self.error {
            message(
                localized_format(
                    "explorer.er_diagram.canvas.metadata_error",
                    &[("error", error)],
                ),
                Theme::error(),
            )
        } else if self.graph.as_ref().is_none_or(|g| g.nodes.is_empty()) {
            // Distinguish "the user unchecked everything" from "the database
            // has no FK metadata" — the fix for one is not the fix for the other.
            let text = if self.selection.as_ref().is_some_and(|s| s.is_empty()) {
                crate::localization::text("explorer.er_diagram.canvas.no_tables_selected")
            } else {
                crate::localization::text("explorer.er_diagram.canvas.no_foreign_keys")
            };
            message(text, Theme::text_dim())
        } else {
            self.render_canvas(cx)
        }
    }
}

impl gpui::Focusable for ErDiagramView {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ErDiagramView {
    fn render_footer(&self) -> gpui::AnyElement {
        let s = Theme::scaled;
        let mut footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(12.0)))
            // Same size as the header row's text: this is the diagram's help
            // line, and at 0.74 it was too small to read.
            .text_size(px(Theme::font_size()))
            .text_color(Theme::text_dim())
            .child(crate::localization::text(
                "explorer.er_diagram.footer.arrow_legend",
            ));
        if let Some(graph) = &self.graph {
            if graph.truncated {
                let shown_tables = graph.nodes.len().to_string();
                let total_tables = graph.total_tables.to_string();
                let shown_relationships = graph.edges.len().to_string();
                let total_relationships = graph.total_relationships.to_string();
                footer = footer.child(localized_format(
                    "explorer.er_diagram.footer.truncated",
                    &[
                        ("shown_tables", &shown_tables),
                        ("total_tables", &total_tables),
                        ("shown_relationships", &shown_relationships),
                        ("total_relationships", &total_relationships),
                    ],
                ));
            }
            if !graph.nodes.is_empty() {
                let tables = graph.nodes.len().to_string();
                let relationships = graph.edges.len().to_string();
                let zoom_shortcut = if cfg!(target_os = "macos") {
                    "⌘-scroll".to_string()
                } else {
                    "Ctrl-scroll".to_string()
                };
                footer = footer.child(div().flex_1()).child(localized_format(
                    "explorer.er_diagram.footer.navigation_help",
                    &[
                        ("tables", &tables),
                        ("relationships", &relationships),
                        ("zoom_shortcut", &zoom_shortcut),
                    ],
                ));
            }
        }
        footer.into_any_element()
    }

    /// The header row (table picker, filter, refresh; plus the title and a
    /// close button in the window presentation — the modal's dialog chrome
    /// renders the title, pop-out, and close itself).
    fn render_header(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let is_modal = self.presentation == ErPresentation::Modal;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(px(Theme::font_size_ratio(1.07)))
                    .text_color(Theme::text())
                    .when(!is_modal, |title| title.child(self.title_text())),
            )
            .when(self.visible.is_some(), |row| {
                row.child(
                    Button::new("er-show-all")
                        .small()
                        .ghost()
                        .label(crate::localization::text(
                            "explorer.er_diagram.header.show_all",
                        ))
                        .on_click(cx.listener(|this, _, _window, cx| {
                            this.show_all();
                            cx.notify();
                        })),
                )
            })
            .child(self.render_view_toggles(cx))
            .child(
                Button::new("er-tables")
                    .small()
                    .ghost()
                    .label(crate::localization::text(
                        "explorer.er_diagram.header.tables",
                    ))
                    .selected(self.picker_open)
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_picker(cx))),
            )
            .child(
                div()
                    .w(px(s(200.0)))
                    .child(Input::new(&self.filter_input).cleanable(true)),
            )
            .child(
                Button::new("er-refresh")
                    .small()
                    .label(crate::localization::text(
                        "explorer.er_diagram.header.refresh",
                    ))
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
            )
            // No Close button in the window presentation: the OS title bar
            // already has one, and two of them side by side reads as though
            // they did different things.
            .into_any_element()
    }

    /// The four view switches. Checkboxes, not toggle buttons: these are
    /// independent "show this" options, and a checkbox says whether something
    /// is on without the reader having to decode a highlight.
    ///
    /// Types and Nulls both describe a column row, so they are only meaningful
    /// while Columns is on and are disabled with it.
    fn render_view_toggles(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let columns_on = self.show_columns;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(10.0)))
            .child(
                Checkbox::new("er-columns")
                    .label(crate::localization::text(
                        "explorer.er_diagram.view.columns",
                    ))
                    .checked(self.show_columns)
                    .on_click(
                        cx.listener(|this, _new: &bool, _window, cx| this.toggle_columns(cx)),
                    ),
            )
            .child(
                Checkbox::new("er-types")
                    .label(crate::localization::text("explorer.er_diagram.view.types"))
                    .checked(self.show_types)
                    .disabled(!columns_on)
                    .on_click(cx.listener(|this, _new: &bool, _window, cx| {
                        this.show_types = !this.show_types;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("er-nulls")
                    .label(crate::localization::text("explorer.er_diagram.view.nulls"))
                    .checked(self.show_nulls)
                    .disabled(!columns_on)
                    .on_click(cx.listener(|this, _new: &bool, _window, cx| {
                        this.show_nulls = !this.show_nulls;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("er-fks")
                    .label(crate::localization::text(
                        "explorer.er_diagram.view.foreign_keys",
                    ))
                    .checked(self.show_fks)
                    .on_click(cx.listener(|this, _new: &bool, _window, cx| {
                        this.show_fks = !this.show_fks;
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// The tables the picker lists: every DB table (from `list_tables`) unioned
    /// with whatever is currently displayed, sorted, so the list is usable even
    /// if `list_tables` failed and shows tables already on the canvas.
    fn picker_entries(&self) -> Vec<TableKey> {
        let mut entries: Vec<TableKey> = self.all_tables.clone();
        entries.extend(self.displayed_tables());
        entries.sort();
        entries.dedup();
        entries
    }

    fn render_picker(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let entries = self.picker_entries();
        let shown: HashSet<TableKey> = entries
            .iter()
            .filter(|k| self.is_table_shown(k))
            .cloned()
            .collect();
        let shown_count = shown.len();
        // Push the current picker state into the list delegate; `sync` skips
        // the re-filter and notify when nothing changed (this runs on every
        // view frame while the picker is open, e.g. during canvas pans).
        let tables_loading = self.tables_loading;
        self.picker_list.update(cx, |list, cx| {
            list.delegate_mut().sync(entries, shown, tables_loading, cx);
        });

        div()
            .id("er-picker")
            .occlude()
            .absolute()
            .top(px(s(34.0)))
            .right(px(0.0))
            .w(px(s(320.0)))
            .h(px(s(420.0)))
            .bg(Theme::popup_bg())
            .border_1()
            .border_color(Theme::popup_border())
            .rounded(px(s(6.0)))
            .shadow_lg()
            .flex()
            .flex_col()
            .gap(px(s(6.0)))
            .p(px(s(8.0)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _ev, _window, cx| cx.stop_propagation()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(Theme::font_size_ratio(0.9)))
                            .text_color(Theme::text_dim())
                            .child({
                                let count = shown_count.to_string();
                                localized_format(
                                    "explorer.er_diagram.picker.selection_count",
                                    &[("count", &count)],
                                )
                            }),
                    )
                    .child(
                        Button::new("er-pick-close")
                            .xsmall()
                            .ghost()
                            .label("✕")
                            .on_click(cx.listener(|this, _, _window, cx| this.toggle_picker(cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .child(
                        Button::new("er-pick-all")
                            .xsmall()
                            .ghost()
                            .label(crate::localization::text("explorer.er_diagram.picker.all"))
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.select_all_tables(cx)),
                            ),
                    )
                    .child(
                        Button::new("er-pick-none")
                            .xsmall()
                            .ghost()
                            .label(crate::localization::text("explorer.er_diagram.picker.none"))
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.select_no_tables(cx)),
                            ),
                    )
                    .child(
                        Button::new("er-pick-default")
                            .xsmall()
                            .ghost()
                            .label(crate::localization::text(
                                "explorer.er_diagram.picker.fk_connected",
                            ))
                            .on_click(
                                cx.listener(|this, _, _window, cx| this.select_default_tables(cx)),
                            ),
                    ),
            )
            .child(div().flex_1().min_h(px(0.0)).child(
                List::new(&self.picker_list).search_placeholder(crate::localization::text(
                    "explorer.er_diagram.picker.search_placeholder",
                )),
            ))
            .into_any_element()
    }

    /// The pane content shared by both presentations: header + toolbar, the
    /// canvas body, and the footer, plus the picker overlay when open.
    fn render_pane_inner(&mut self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let footer = self.render_footer();
        let header = self.render_header(cx);
        let toolbar = self.render_toolbar(cx);
        let body = self.render_body(cx);
        div()
            .relative()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .size_full()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(6.0)))
                    .child(header)
                    .child(toolbar),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .overflow_hidden()
                    .child(body),
            )
            .child(footer)
            .when(self.picker_open, |pane| pane.child(self.render_picker(cx)))
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Table-picker list delegate
// ---------------------------------------------------------------------------

/// `schema.table` (or just `table` for engines without a schema namespace),
/// as shown on picker rows and matched by the picker filter.
fn qualified_table(key: &TableKey) -> String {
    if key.0.is_empty() {
        key.1.clone()
    } else {
        format!("{}.{}", key.0, key.1)
    }
}

/// [`ListDelegate`] backing the table picker: the kit list supplies the query
/// input and virtualized same-height rows, while multi-select stays on the
/// per-row [`Checkbox`] — list selection is disabled (`selectable(false)`),
/// so there is no row highlight, arrow-key selection, or click-to-confirm to
/// conflict with the checkbox model. `render_picker` syncs
/// entries/checked/loading from the view each frame the picker is open;
/// checkbox clicks route back through the weak view handle.
struct TablePickerDelegate {
    view: WeakEntity<ErDiagramView>,
    /// Unfiltered picker entries (all DB tables ∪ displayed, sorted) —
    /// see [`ErDiagramView::picker_entries`].
    entries: Vec<TableKey>,
    /// Tables currently shown on the canvas — the checked set.
    checked: HashSet<TableKey>,
    tables_loading: bool,
    /// `entries` narrowed by the query input.
    filtered: Vec<TableKey>,
    query: String,
}

impl TablePickerDelegate {
    fn refilter(&mut self) {
        let needle = self.query.trim().to_lowercase();
        self.filtered = self
            .entries
            .iter()
            .filter(|key| {
                needle.is_empty() || qualified_table(key).to_lowercase().contains(&needle)
            })
            .cloned()
            .collect();
    }

    /// Adopt the view's current picker state, re-filtering and re-rendering
    /// only when something actually changed.
    fn sync(
        &mut self,
        entries: Vec<TableKey>,
        checked: HashSet<TableKey>,
        tables_loading: bool,
        cx: &mut gpui::Context<ListState<Self>>,
    ) {
        if self.entries == entries
            && self.checked == checked
            && self.tables_loading == tables_loading
        {
            return;
        }
        self.entries = entries;
        self.checked = checked;
        self.tables_loading = tables_loading;
        self.refilter();
        cx.notify();
    }
}

impl ListDelegate for TablePickerDelegate {
    type Item = Checkbox;

    fn perform_search(
        &mut self,
        query: &str,
        _window: &mut Window,
        cx: &mut gpui::Context<ListState<Self>>,
    ) -> Task<()> {
        // Same matching as the old hand-rolled list: trimmed, lowercased,
        // substring on the qualified name.
        self.query = query.to_string();
        self.refilter();
        cx.notify();
        Task::ready(())
    }

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.filtered.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut gpui::Context<ListState<Self>>,
    ) -> Option<Checkbox> {
        let key = self.filtered.get(ix.row)?.clone();
        let label = qualified_table(&key);
        let view = self.view.clone();
        // Id by name, not list position: the filter shifts positions between
        // frames, and positional ids would re-target rows.
        Some(
            Checkbox::new(SharedString::from(format!("er-pick-{label}")))
                .checked(self.checked.contains(&key))
                .label(label)
                .on_click(move |_new: &bool, _window, app| {
                    if let Some(this) = view.upgrade() {
                        this.update(app, |this, cx| {
                            this.toggle_table(key.clone(), cx);
                        });
                    }
                }),
        )
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        _cx: &mut gpui::Context<ListState<Self>>,
    ) -> impl IntoElement {
        let msg = if self.tables_loading {
            crate::localization::text("explorer.er_diagram.picker.loading")
        } else if self.entries.is_empty() {
            crate::localization::text("explorer.er_diagram.picker.no_tables")
        } else {
            crate::localization::text("explorer.er_diagram.picker.no_matches")
        };
        div()
            .p(px(Theme::scaled(8.0)))
            .text_color(Theme::text_dim())
            .child(msg)
    }

    fn set_selected_index(
        &mut self,
        _ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut gpui::Context<ListState<Self>>,
    ) {
    }
}

/// Open the diagram as a standalone resizable window and return the view so
/// the caller can subscribe to its events (reveal-in-tree, status). Returns
/// `None` only if the platform failed to create the window.
pub fn open_er_diagram_window(
    context: ToolDialogContext,
    database: String,
    selection: Option<HashSet<TableKey>>,
    focus: Option<(String, String)>,
    cx: &mut App,
) -> Option<Entity<ErDiagramView>> {
    let title = if database.is_empty() {
        crate::localization::text("explorer.er_diagram.window.title")
    } else {
        localized_format(
            "explorer.er_diagram.window.database_title",
            &[("database", &database)],
        )
    };
    let bounds = Bounds::centered(None, size(px(1040.0), px(720.0)), cx);
    // `open_window` builds the root synchronously; the slot smuggles the inner
    // view entity back out past the `Root` wrapper.
    let slot: std::rc::Rc<std::cell::RefCell<Option<Entity<ErDiagramView>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let slot_in = slot.clone();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some(SharedString::from(title)),
                ..Default::default()
            }),
            app_id: Some(crate::about::APP_ID.into()),
            is_resizable: true,
            ..Default::default()
        },
        move |window, cx| {
            let view = cx.new(|cx| {
                ErDiagramView::new_with(
                    context.clone(),
                    database.clone(),
                    focus.clone(),
                    ErPresentation::Window,
                    selection.clone(),
                    window,
                    cx,
                )
            });
            *slot_in.borrow_mut() = Some(view.clone());
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        },
    );
    if opened.is_err() {
        return None;
    }
    slot.borrow_mut().take()
}

impl Render for ErDiagramView {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let pane = self.render_pane_inner(cx);
        match self.presentation {
            ErPresentation::Window => div()
                .id("er-diagram-window")
                .key_context("ErDiagramView")
                .track_focus(&self.focus_handle)
                .on_key_down(cx.listener(Self::on_key_down))
                .size_full()
                .bg(Theme::popup_bg())
                .font(Theme::ui_font())
                .text_size(px(Theme::font_size()))
                .p(px(s(14.0)))
                .child(pane)
                .into_any_element(),
            // bare_body + fixed frame: the canvas painter, toolbar, and legend
            // manage their own layout — a scrolling chrome body would break
            // the canvas. Key handling stays on this inner wrapper (escape
            // must close the Tables ▾ picker before the dialog); the chrome
            // owns the scrim, title bar, pop-out, and footer Close.
            ErPresentation::Modal => {
                Dialog::new("er-diagram-pane", |this: &mut Self| &mut this.frame)
                    .title(self.title_text())
                    .easy_close(true)
                    .bare_body()
                    .detachable(|this: &mut Self, _window, cx| this.pop_out(cx))
                    .on_close(|this, window, cx| this.close(window, cx))
                    .child(
                        div()
                            .id("er-diagram-modal-body")
                            .key_context("ErDiagramView")
                            .track_focus(&self.focus_handle)
                            .on_key_down(cx.listener(Self::on_key_down))
                            .flex_1()
                            .min_h(px(0.0))
                            .font(Theme::ui_font())
                            .p(px(s(14.0)))
                            .child(pane),
                    )
                    .build(self, window, cx)
                    .into_any_element()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Canvas painting
// ---------------------------------------------------------------------------

/// One text row inside a node box: the label plus whether it is emphasized
/// (an FK-participating column paints in the primary text color).
struct NodeRowPaint {
    /// The column name, painted left-aligned.
    text: String,
    /// Type / nullability, painted right-aligned against the card's inner
    /// edge. Empty when both are switched off.
    detail: String,
    emphasized: bool,
}

struct NodePaint {
    x: f32,
    y: f32,
    title: String,
    /// Rows under the title: full column rows when column display is on,
    /// else the single joined key-columns line.
    rows: Vec<NodeRowPaint>,
    /// Whether `rows` are a real column list (paints the header separator
    /// and anchors edges at rows) as opposed to the legacy key-columns line.
    column_rows: bool,
    dimmed: bool,
    matched: bool,
}

struct EdgePaint {
    from: usize,
    to: usize,
    dimmed: bool,
    /// Painted row index of the FK's first column pair inside each endpoint,
    /// when that column is visible — edges then point at the exact column.
    from_row: Option<usize>,
    to_row: Option<usize>,
}

/// Everything the paint closure needs, resolved on the render pass.
struct ErPaint {
    nodes: Vec<NodePaint>,
    edges: Vec<EdgePaint>,
    pan: (f32, f32),
    zoom: f32,
    node_w: f32,
    node_h: f32,
    /// Title band height / column row pitch, already zoom-scaled like
    /// `node_h`.
    header_h: f32,
    col_row_h: f32,
    background: Hsla,
    node_bg: Hsla,
    node_border: Hsla,
    accent: Hsla,
    text: Hsla,
    text_dim: Hsla,
    font_size: f32,
}

const DIM_OPACITY: f32 = 0.2;

/// Paint edges (with arrowheads toward the referenced table and mid-point
/// labels) under the node boxes, all offset by the pan.
fn paint_er(p: &ErPaint, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    // Canvas background across the full viewport (not panned).
    window.paint_quad(PaintQuad {
        bounds,
        background: p.background.into(),
        border_color: gpui::transparent_black(),
        border_widths: Default::default(),
        corner_radii: Default::default(),
        border_style: Default::default(),
    });
    if p.nodes.is_empty() {
        return;
    }

    let ox = f32::from(bounds.origin.x) + p.pan.0;
    let oy = f32::from(bounds.origin.y) + p.pan.1;
    let text_system = window.text_system().clone();
    let font = Theme::ui_font();
    let line_height = |fs: f32| px(fs * 1.4);
    let shape = |text: &str, color: Hsla, fs: f32| {
        let run = TextRun {
            len: text.len(),
            color,
            font: font.clone(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        text_system.shape_line(text.to_owned().into(), px(fs), &[run], None)
    };
    let half = (p.node_w / 2.0, p.node_h / 2.0);
    let center = |n: &NodePaint| (ox + n.x + half.0, oy + n.y + half.1);
    let stroke_w = (1.5 * p.zoom).max(0.75);
    let arrow_len = (9.0 * p.zoom).max(4.0);
    let arrow_w = (4.5 * p.zoom).max(2.0);
    let pad_x = (9.0 * p.zoom).max(3.0);
    let pad_y = (8.0 * p.zoom).max(2.0);
    let radius = (6.0 * p.zoom).max(2.0);

    for edge in &p.edges {
        // Self-references have no meaningful line on a box diagram; the edge
        // stays in the model but is not painted.
        if edge.from == edge.to {
            continue;
        }
        let (Some(a), Some(b)) = (p.nodes.get(edge.from), p.nodes.get(edge.to)) else {
            continue;
        };
        let ca = center(a);
        let cb = center(b);
        // Anchor at the exact column row when it is visible: exit the side of
        // the box facing the other table, at the row's vertical center.
        let column_anchor = |n: &NodePaint, row: Option<usize>, other_cx: f32| {
            let row = row?;
            if !n.column_rows {
                return None;
            }
            let x = if other_cx >= ox + n.x + half.0 {
                ox + n.x + p.node_w
            } else {
                ox + n.x
            };
            let y = oy + n.y + p.header_h + (row as f32 + 0.5) * p.col_row_h;
            Some((x, y))
        };
        let start = column_anchor(a, edge.from_row, cb.0)
            .unwrap_or_else(|| rect_border_point(ca, half, cb));
        let end =
            column_anchor(b, edge.to_row, ca.0).unwrap_or_else(|| rect_border_point(cb, half, ca));
        let color = if edge.dimmed {
            p.text_dim.opacity(DIM_OPACITY)
        } else {
            p.text_dim
        };
        let mut stroke = gpui::PathBuilder::stroke(px(stroke_w));
        stroke.move_to(point(px(start.0), px(start.1)));
        stroke.line_to(point(px(end.0), px(end.1)));
        if let Ok(path) = stroke.build() {
            window.paint_path(path, color);
        }
        // Arrowhead toward the referenced table.
        let dx = end.0 - start.0;
        let dy = end.1 - start.1;
        let len = (dx * dx + dy * dy).sqrt();
        if len > 1.0 {
            let (ux, uy) = (dx / len, dy / len);
            let base = (end.0 - ux * arrow_len, end.1 - uy * arrow_len);
            let (px_, py_) = (-uy, ux);
            let mut head = gpui::PathBuilder::fill();
            head.move_to(point(px(end.0), px(end.1)));
            head.line_to(point(
                px(base.0 + px_ * arrow_w),
                px(base.1 + py_ * arrow_w),
            ));
            head.line_to(point(
                px(base.0 - px_ * arrow_w),
                px(base.1 - py_ * arrow_w),
            ));
            head.close();
            if let Ok(path) = head.build() {
                window.paint_path(path, color);
            }
        }
        // No constraint label: the arrow says which table is referenced, and
        // a name floating over the canvas was noise on any diagram with more
        // than a handful of relationships.
    }

    for node in &p.nodes {
        let x = ox + node.x;
        let y = oy + node.y;
        let (bg, border, title_color, cols_color) = if node.dimmed {
            (
                p.node_bg.opacity(DIM_OPACITY),
                p.node_border.opacity(DIM_OPACITY),
                p.text.opacity(DIM_OPACITY),
                p.text_dim.opacity(DIM_OPACITY),
            )
        } else if node.matched {
            (p.node_bg, p.accent, p.text, p.text_dim)
        } else {
            (p.node_bg, p.node_border, p.text, p.text_dim)
        };
        window.paint_quad(PaintQuad {
            bounds: Bounds {
                origin: point(px(x), px(y)),
                size: size(px(p.node_w), px(p.node_h)),
            },
            background: bg.into(),
            border_color: border,
            border_widths: Edges::all(px((1.0 * p.zoom).max(1.0))),
            corner_radii: Corners::all(px(radius)),
            border_style: Default::default(),
        });
        let title_fs = p.font_size * 0.9;
        let title = shape(&node.title, title_color, title_fs);
        let _ = title.paint(
            Point {
                x: px(x + pad_x),
                y: px(y + pad_y),
            },
            line_height(title_fs),
            TextAlign::Left,
            None,
            window,
            cx,
        );
        if node.rows.is_empty() {
            continue;
        }
        let cols_fs = p.font_size * 0.72;
        if node.column_rows {
            // Hairline under the title band, separating it from the rows.
            window.paint_quad(PaintQuad {
                bounds: Bounds {
                    origin: point(px(x), px(y + p.header_h)),
                    size: size(px(p.node_w), px((1.0 * p.zoom).max(0.5))),
                },
                background: border.into(),
                border_color: gpui::transparent_black(),
                border_widths: Default::default(),
                corner_radii: Default::default(),
                border_style: Default::default(),
            });
            for (row_ix, row) in node.rows.iter().enumerate() {
                let color = if node.dimmed {
                    cols_color
                } else if row.emphasized {
                    title_color
                } else {
                    cols_color
                };
                let row_y = y + p.header_h + row_ix as f32 * p.col_row_h;
                // Center the text in its row pitch.
                let text_y = px(row_y + (p.col_row_h - cols_fs * 1.2) / 2.0);

                // The type/nullability detail is painted right-aligned against
                // the card's inner edge, so there is always a clean gutter
                // between it and the column name however long the name is.
                // Paint it first and let it claim its width; the name is then
                // clipped to whatever is left.
                let mut name_room = p.node_w - pad_x * 2.0;
                if !row.detail.is_empty() {
                    let detail = shape(&row.detail, cols_color, cols_fs);
                    let detail_w = f32::from(detail.width);
                    let _ = detail.paint(
                        Point {
                            x: px(x + p.node_w - pad_x - detail_w),
                            y: text_y,
                        },
                        line_height(cols_fs),
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                    name_room -= detail_w + pad_x;
                }
                let line = shape(&row.text, color, cols_fs);
                let _ = line.paint(
                    Point {
                        x: px(x + pad_x),
                        y: text_y,
                    },
                    line_height(cols_fs),
                    TextAlign::Left,
                    Some(px(name_room.max(0.0))),
                    window,
                    cx,
                );
            }
        } else if let Some(row) = node.rows.first() {
            let cols = shape(&row.text, cols_color, cols_fs);
            let _ = cols.paint(
                Point {
                    x: px(x + pad_x),
                    y: px(y + pad_y + title_fs * 1.5),
                },
                line_height(cols_fs),
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef};
    use sqlly_services::Services;

    #[test]
    fn layout_kind_dispatches_on_row() {
        assert_eq!(layout_kind_from_row(Some(0)), LayoutKind::Grid);
        assert_eq!(layout_kind_from_row(Some(1)), LayoutKind::Layered);
        assert_eq!(layout_kind_from_row(None), LayoutKind::Grid);
    }

    fn fk(
        constraint: &str,
        from_schema: &str,
        from_table: &str,
        from_column: &str,
        to_schema: &str,
        to_table: &str,
        to_column: &str,
    ) -> ForeignKeyRow {
        ForeignKeyRow {
            constraint_name: constraint.to_string(),
            from_schema: from_schema.to_string(),
            from_table: from_table.to_string(),
            from_column: from_column.to_string(),
            to_schema: to_schema.to_string(),
            to_table: to_table.to_string(),
            to_column: to_column.to_string(),
        }
    }

    /// A `FetchOutcome` carrying only FK rows (table list empty) for tests that
    /// exercise the FK/graph path.
    fn fk_fetch(fks: Vec<ForeignKeyRow>) -> FetchOutcome {
        FetchOutcome {
            fks: Ok(fks),
            tables: Ok(Vec::new()),
            columns: Ok(Vec::new()),
        }
    }

    // -- build_graph ----------------------------------------------------------

    #[test]
    fn empty_rows_build_an_empty_graph() {
        let graph = build_graph(&[]);
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
        assert!(!graph.truncated);
        assert_eq!(graph.total_tables, 0);
        assert_eq!(graph.total_relationships, 0);
    }

    #[test]
    fn tables_in_view_covers_all_nodes_without_a_focus() {
        let rows = vec![
            fk(
                "fk_a",
                "dbo",
                "orders",
                "customer_id",
                "dbo",
                "customers",
                "id",
            ),
            fk(
                "fk_b",
                "dbo",
                "order_lines",
                "order_id",
                "dbo",
                "orders",
                "id",
            ),
        ];
        let graph = build_graph(&rows);
        let keys = tables_in_view(&graph, &None);
        assert_eq!(
            keys,
            vec![
                ("dbo".to_string(), "customers".to_string()),
                ("dbo".to_string(), "order_lines".to_string()),
                ("dbo".to_string(), "orders".to_string()),
            ]
        );
    }

    #[test]
    fn tables_in_view_restricts_to_the_focus_neighbourhood() {
        let rows = vec![
            fk(
                "fk_a",
                "dbo",
                "orders",
                "customer_id",
                "dbo",
                "customers",
                "id",
            ),
            fk(
                "fk_b",
                "dbo",
                "order_lines",
                "order_id",
                "dbo",
                "orders",
                "id",
            ),
        ];
        let graph = build_graph(&rows);
        let orders = graph
            .nodes
            .iter()
            .position(|n| n.table == "orders")
            .expect("orders node");
        let visible = Some(neighbourhood(&graph, orders));
        let keys = tables_in_view(&graph, &visible);
        assert!(
            keys.iter().any(|(_, table)| table == "orders")
                && keys.iter().any(|(_, table)| table == "customers")
                && keys.iter().any(|(_, table)| table == "order_lines"),
            "the neighbourhood keeps the focused table and both neighbours: {keys:?}"
        );
        // An out-of-range index is ignored rather than panicking.
        let keys = tables_in_view(&graph, &Some(vec![usize::MAX]));
        assert!(keys.is_empty());
    }

    #[test]
    fn tables_dedupe_across_rows_and_collect_key_columns() {
        let rows = vec![
            fk(
                "fk_a",
                "dbo",
                "orders",
                "customer_id",
                "dbo",
                "customers",
                "id",
            ),
            fk(
                "fk_b",
                "dbo",
                "order_lines",
                "order_id",
                "dbo",
                "orders",
                "id",
            ),
        ];
        let graph = build_graph(&rows);
        assert_eq!(graph.nodes.len(), 3);
        assert_eq!(graph.total_tables, 3);
        assert_eq!(graph.total_relationships, 2);
        let orders = graph
            .nodes
            .iter()
            .find(|n| n.table == "orders")
            .expect("orders node");
        // Child side contributes customer_id; parent side contributes id.
        assert_eq!(orders.columns_in_keys, vec!["customer_id", "id"]);
    }

    #[test]
    fn selected_graph_keeps_only_edges_inside_the_selection() {
        let rows = vec![
            fk(
                "fk_a",
                "dbo",
                "orders",
                "customer_id",
                "dbo",
                "customers",
                "id",
            ),
            fk(
                "fk_b",
                "dbo",
                "order_lines",
                "order_id",
                "dbo",
                "orders",
                "id",
            ),
        ];
        // Drop `order_lines`: its edge to `orders` must vanish, but the
        // orders→customers edge survives.
        let selected: HashSet<TableKey> = [
            ("dbo".to_string(), "orders".to_string()),
            ("dbo".to_string(), "customers".to_string()),
        ]
        .into_iter()
        .collect();
        let graph = build_graph_selected(&rows, &selected);
        let names: Vec<&str> = graph.nodes.iter().map(|n| n.table.as_str()).collect();
        assert!(names.contains(&"orders") && names.contains(&"customers"));
        assert!(!names.contains(&"order_lines"));
        assert_eq!(graph.edges.len(), 1);
        assert!(!graph.truncated, "explicit selection is never capped");
    }

    #[test]
    fn selected_graph_includes_isolated_tables_as_edgeless_nodes() {
        let rows = vec![fk(
            "fk_a",
            "dbo",
            "orders",
            "customer_id",
            "dbo",
            "customers",
            "id",
        )];
        // `audit_log` participates in no FK but is explicitly selected.
        let selected: HashSet<TableKey> = [
            ("dbo".to_string(), "orders".to_string()),
            ("dbo".to_string(), "customers".to_string()),
            ("dbo".to_string(), "audit_log".to_string()),
        ]
        .into_iter()
        .collect();
        let graph = build_graph_selected(&rows, &selected);
        assert_eq!(graph.nodes.len(), 3);
        let audit = graph
            .nodes
            .iter()
            .find(|n| n.table == "audit_log")
            .expect("isolated node present");
        assert!(audit.columns_in_keys.is_empty());
        assert_eq!(graph.edges.len(), 1);
    }

    #[test]
    fn empty_selection_yields_no_nodes() {
        let rows = vec![fk(
            "fk_a",
            "dbo",
            "orders",
            "customer_id",
            "dbo",
            "customers",
            "id",
        )];
        let graph = build_graph_selected(&rows, &HashSet::new());
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
    }

    #[test]
    fn compound_fk_folds_into_one_edge_with_both_column_pairs_in_the_label() {
        let rows = vec![
            fk("fk_ol", "dbo", "lines", "order_id", "dbo", "orders", "id"),
            fk("fk_ol", "dbo", "lines", "order_rev", "dbo", "orders", "rev"),
        ];
        let graph = build_graph(&rows);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(
            graph.edges[0].label,
            "fk_ol (order_id, order_rev → id, rev)"
        );
        let from = &graph.nodes[graph.edges[0].from];
        let to = &graph.nodes[graph.edges[0].to];
        assert_eq!(from.table, "lines");
        assert_eq!(to.table, "orders");
        assert_eq!(from.columns_in_keys, vec!["order_id", "order_rev"]);
        assert_eq!(to.columns_in_keys, vec!["id", "rev"]);
    }

    #[test]
    fn distinct_constraints_between_the_same_tables_stay_distinct_edges() {
        let rows = vec![
            fk(
                "fk_bill",
                "dbo",
                "orders",
                "bill_addr",
                "dbo",
                "addresses",
                "id",
            ),
            fk(
                "fk_ship",
                "dbo",
                "orders",
                "ship_addr",
                "dbo",
                "addresses",
                "id",
            ),
        ];
        let graph = build_graph(&rows);
        assert_eq!(graph.edges.len(), 2);
        assert_eq!(graph.total_relationships, 2);
    }

    #[test]
    fn self_referencing_fk_keeps_one_node_and_one_edge() {
        let rows = vec![fk(
            "fk_parent",
            "dbo",
            "employees",
            "manager_id",
            "dbo",
            "employees",
            "id",
        )];
        let graph = build_graph(&rows);
        assert_eq!(graph.nodes.len(), 1);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].from, graph.edges[0].to);
    }

    #[test]
    fn node_cap_keeps_the_most_connected_tables_and_flags_truncation() {
        // One hub referenced by 64 children: 65 tables total.
        let rows: Vec<ForeignKeyRow> = (0..64)
            .map(|i| {
                fk(
                    &format!("fk_{i:02}"),
                    "dbo",
                    &format!("child_{i:02}"),
                    "hub_id",
                    "dbo",
                    "hub",
                    "id",
                )
            })
            .collect();
        let graph = build_graph(&rows);
        assert!(graph.truncated);
        assert_eq!(graph.nodes.len(), NODE_CAP);
        assert_eq!(graph.total_tables, 65);
        assert_eq!(graph.total_relationships, 64);
        // The hub is the most connected and BFS-first.
        assert_eq!(graph.nodes[0].table, "hub");
        // Degree-1 children tie-break by name: the lowest names survive.
        assert!(graph.nodes.iter().any(|n| n.table == "child_00"));
        assert!(!graph.nodes.iter().any(|n| n.table == "child_63"));
        // Every surviving edge references surviving nodes.
        assert_eq!(graph.edges.len(), NODE_CAP - 1);
        for edge in &graph.edges {
            assert!(edge.from < graph.nodes.len());
            assert!(edge.to < graph.nodes.len());
        }
    }

    #[test]
    fn build_graph_is_deterministic() {
        let rows = vec![
            fk("fk_a", "s", "b", "x", "s", "a", "id"),
            fk("fk_b", "s", "c", "x", "s", "a", "id"),
            fk("fk_c", "s", "c", "y", "s", "b", "id"),
        ];
        let g1 = build_graph(&rows);
        let g2 = build_graph(&rows);
        assert_eq!(g1.nodes, g2.nodes);
        assert_eq!(g1.edges, g2.edges);
        // The hub (a, degree 2) leads the BFS order.
        assert_eq!(g1.nodes[0].table, "a");
    }

    // -- layout_grid ----------------------------------------------------------

    #[test]
    fn layout_is_deterministic_finite_and_in_bounds() {
        for n in [0usize, 1, 2, 7, NODE_CAP] {
            let a = layout_grid(n, NODE_W, NODE_H);
            let b = layout_grid(n, NODE_W, NODE_H);
            assert_eq!(a, b, "layout must be deterministic for n={n}");
            assert_eq!(a.positions.len(), n);
            for &(x, y) in &a.positions {
                assert!(x.is_finite() && y.is_finite(), "NaN/inf position");
                assert!(x >= 0.0 && y >= 0.0);
                assert!(x + NODE_W <= a.width, "x={x} overflows width={}", a.width);
                assert!(
                    y + NODE_H <= a.height,
                    "y={y} overflows height={}",
                    a.height
                );
            }
            // No two nodes share a cell.
            let mut seen: Vec<(i64, i64)> = a
                .positions
                .iter()
                .map(|&(x, y)| (x as i64, y as i64))
                .collect();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), n, "positions must be distinct");
        }
    }

    #[test]
    fn layout_of_zero_nodes_is_an_empty_canvas() {
        let layout = layout_grid(0, NODE_W, NODE_H);
        assert!(layout.positions.is_empty());
        assert_eq!(layout.width, 0.0);
        assert_eq!(layout.height, 0.0);
    }

    // -- helpers --------------------------------------------------------------

    #[test]
    fn filter_matches_qualified_name_case_insensitively() {
        let node = ErNode {
            schema: "Sales".to_string(),
            table: "OrderLines".to_string(),
            columns_in_keys: vec![],
            columns: Vec::new(),
        };
        assert!(node_matches_filter(&node, ""));
        assert!(node_matches_filter(&node, "   "));
        assert!(node_matches_filter(&node, "orderl"));
        assert!(node_matches_filter(&node, "sales.order"));
        assert!(!node_matches_filter(&node, "customers"));
    }

    #[test]
    fn rect_border_point_lands_on_the_rectangle_edge() {
        let center = (100.0, 100.0);
        let half = (50.0, 20.0);
        // Straight right: crosses the vertical edge.
        assert_eq!(
            rect_border_point(center, half, (300.0, 100.0)),
            (150.0, 100.0)
        );
        // Straight down: crosses the horizontal edge.
        assert_eq!(
            rect_border_point(center, half, (100.0, 300.0)),
            (100.0, 120.0)
        );
        // Degenerate: toward == center stays put.
        assert_eq!(rect_border_point(center, half, center), center);
        // A diagonal point stays on the border (one axis at its extent).
        let (x, y) = rect_border_point(center, half, (400.0, 400.0));
        let on_x = (x - 150.0).abs() < 1e-3 && (80.0..=120.0).contains(&y);
        let on_y = (y - 120.0).abs() < 1e-3 && (50.0..=150.0).contains(&x);
        assert!(on_x || on_y, "({x}, {y}) is not on the border");
    }

    #[test]
    fn truncate_label_respects_char_boundaries() {
        assert_eq!(truncate_label("orders", 10), "orders");
        assert_eq!(truncate_label("abcdef", 4), "abc…");
        assert_eq!(truncate_label("日本語のテーブル", 4), "日本語…");
    }

    // -- windowed view tests --------------------------------------------------

    /// gpui-component widgets read the library's global theme during render;
    /// windowed tests must install it (idempotently) before opening windows.
    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn test_context() -> ToolDialogContext {
        let secret = SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".to_string(),
        };
        let profile = ConnectionProfile::sql_login("test", "host", "sa", secret);
        ToolDialogContext::new(profile, Services::new())
    }

    /// The fetch delivers its outcome from a real tokio worker thread, which
    /// can wake the parked gpui task after the test scheduler has finished —
    /// `allow_parking()` is the scheduler's sanctioned escape hatch that makes
    /// the late cross-thread wake harmless (same rationale as `ai_panel`).
    fn open_view(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<ErDiagramView>, &mut gpui::VisualTestContext) {
        crate::ensure_test_tokio_runtime();
        ensure_components_initialized(cx);
        cx.dispatcher.allow_parking();
        cx.add_window_view(|window, cx| {
            ErDiagramView::new(test_context(), "sales".to_string(), None, window, cx)
        })
    }

    #[gpui::test]
    fn stub_metadata_error_lands_as_a_friendly_error(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        // The fetch hops through the real tokio runtime thread, so parking
        // once is not enough — poll until the outcome lands. The bound is
        // wall-clock, not iterations: a loaded CI agent can starve the tokio
        // worker for seconds while the whole suite runs in parallel.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let mut settled = false;
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            if view.read_with(cx, |view, _cx| !view.loading) {
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(settled, "fetch outcome never landed");
        view.read_with(cx, |view, _cx| {
            // The stub metadata service reports NotImplemented — the dialog
            // must surface it as text, not hang in loading or show a graph.
            assert!(view.error.is_some());
            assert!(view.graph.is_none());
        });
    }

    #[gpui::test]
    fn stale_generation_fetch_is_ignored(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            let stale = view.refresh_generation;
            view.refresh_generation += 1;
            view.loading = true;
            view.apply_fetch(stale, fk_fetch(vec![]), cx);
            assert!(view.loading, "a stale outcome must not land");
            let current = view.refresh_generation;
            view.apply_fetch(
                current,
                fk_fetch(vec![fk("fk", "s", "a", "x", "s", "b", "id")]),
                cx,
            );
            assert!(!view.loading);
            let graph = view.graph.as_ref().expect("graph after fresh outcome");
            assert_eq!(graph.nodes.len(), 2);
            assert_eq!(graph.edges.len(), 1);
            assert_eq!(view.layout.positions.len(), 2);
        });
    }

    #[gpui::test]
    fn node_hit_test_maps_window_coordinates_through_pan(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            let generation = view.refresh_generation;
            view.apply_fetch(
                generation,
                fk_fetch(vec![fk("fk", "s", "a", "x", "s", "b", "id")]),
                cx,
            );
            view.canvas_bounds.set(Some(Bounds {
                origin: point(px(10.0), px(20.0)),
                size: size(px(800.0), px(480.0)),
            }));
            view.pan = (0.0, 0.0);
            let (x0, y0) = view.layout.positions[0];
            let inside = point(px(10.0 + x0 + 5.0), px(20.0 + y0 + 5.0));
            assert_eq!(view.node_at(inside), Some(0));
            // Panning left by a node width moves the box out from under the
            // same window point.
            view.pan = (-(NODE_W + 1.0), 0.0);
            assert_eq!(view.node_at(inside), None);
            // A point in the pane but outside every box hits nothing.
            view.pan = (0.0, 0.0);
            let outside = point(px(10.0 + 1.0), px(20.0 + 1.0));
            assert_eq!(view.node_at(outside), None);
        });
    }

    fn mouse_down(position: Point<Pixels>) -> gpui::MouseDownEvent {
        gpui::MouseDownEvent {
            button: MouseButton::Left,
            position,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
            first_mouse: false,
        }
    }

    fn mouse_move(position: Point<Pixels>, pressed: bool) -> gpui::MouseMoveEvent {
        gpui::MouseMoveEvent {
            position,
            pressed_button: pressed.then_some(MouseButton::Left),
            modifiers: gpui::Modifiers::default(),
        }
    }

    fn mouse_up(position: Point<Pixels>) -> gpui::MouseUpEvent {
        gpui::MouseUpEvent {
            button: MouseButton::Left,
            position,
            modifiers: gpui::Modifiers::default(),
            click_count: 1,
        }
    }

    fn seed_two_node_graph(view: &mut ErDiagramView, cx: &mut gpui::Context<ErDiagramView>) {
        let generation = view.refresh_generation;
        view.apply_fetch(
            generation,
            fk_fetch(vec![fk("fk", "s", "a", "x", "s", "b", "id")]),
            cx,
        );
        view.canvas_bounds.set(Some(Bounds {
            origin: point(px(10.0), px(20.0)),
            size: size(px(800.0), px(480.0)),
        }));
        view.zoom = 1.0;
        view.pan = (0.0, 0.0);
    }

    #[gpui::test]
    fn script_as_create_runs_once_and_binds_the_diagram_profile(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        cx.run_until_parked();
        let events: std::rc::Rc<std::cell::RefCell<Vec<ErDiagramEvent>>> = Default::default();
        let sink = events.clone();
        cx.update(|_window, app| {
            app.subscribe(&view, move |_, event: &ErDiagramEvent, _| {
                sink.borrow_mut().push(event.clone());
            })
            .detach();
        });
        view.update(cx, |view, cx| {
            seed_two_node_graph(view, cx);
            view.script_as_create(cx);
            assert!(view.scripting, "the first click starts a run");
            // Repeated clicks while it runs are ignored.
            view.script_as_create(cx);
            view.script_as_create(cx);
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while std::time::Instant::now() < deadline {
            cx.run_until_parked();
            if view.read_with(cx, |view, _cx| !view.scripting) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            view.read_with(cx, |view, _cx| !view.scripting),
            "run never landed"
        );
        let events = events.borrow();
        let scripts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ErDiagramEvent::ScriptToEditor { profile, .. } => Some(profile.clone()),
                _ => None,
            })
            .collect();
        // The stub services cannot read any table, so every table is
        // SKIPPED — but the run still yields exactly one script, bound to the
        // diagram's database rather than the workbench's active one.
        assert_eq!(scripts.len(), 1, "{events:?}");
        assert_eq!(scripts[0].database.as_deref(), Some("sales"));
    }

    #[test]
    fn position_of_prefers_override() {
        let layout = layout_grid(2, NODE_W, NODE_H);
        let mut overrides = std::collections::HashMap::new();
        overrides.insert(0usize, (40.0, 50.0));
        let position_of = |i: usize| overrides.get(&i).copied().unwrap_or(layout.positions[i]);
        assert_eq!(position_of(0), (40.0, 50.0));
        assert_eq!(position_of(1), layout.positions[1]);
    }

    #[gpui::test]
    fn zoom_scales_hit_test(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            seed_two_node_graph(view, cx);
            let (x0, y0) = view.position_of(0);
            let just_past = point(px(10.0 + x0 + NODE_W + 10.0), px(20.0 + y0 * 2.0 + 5.0));
            assert_eq!(view.node_at(just_past), None);
            view.set_zoom_for_test(2.0);
            assert_eq!(view.node_at(just_past), Some(0));
        });
    }

    #[gpui::test]
    fn drag_moves_node_and_suppresses_reveal(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        cx.run_until_parked();
        let events: std::rc::Rc<std::cell::RefCell<Vec<ErDiagramEvent>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &ErDiagramEvent, _cx| {
                seen.borrow_mut().push(event.clone());
            })
            .detach();
        });
        cx.update(|window, app| {
            view.update(app, |view, cx| {
                seed_two_node_graph(view, cx);
                let (x0, y0) = view.position_of(0);
                let start = point(px(10.0 + x0 + 5.0), px(20.0 + y0 + 5.0));
                let moved = point(px(10.0 + x0 + 25.0), px(20.0 + y0 + 5.0));
                view.on_canvas_mouse_down(&mouse_down(start), window, cx);
                view.on_canvas_mouse_move(&mouse_move(moved, true), window, cx);
                view.on_canvas_mouse_up(&mouse_up(moved), window, cx);
                assert!(
                    view.overrides.contains_key(&0),
                    "drag should store an override"
                );
            });
        });
        assert!(
            !events
                .borrow()
                .iter()
                .any(|e| matches!(e, ErDiagramEvent::RevealObject { .. })),
            "drag must not reveal"
        );
    }

    /// Saving used to report only to the log: a failed write and a successful
    /// one were both silent, and with no graph loaded the menu item did
    /// nothing at all. The no-graph guard is the branch reachable without a
    /// file dialog, so it is the one asserted here.
    #[cfg(not(target_arch = "wasm32"))]
    #[gpui::test]
    fn saving_without_a_graph_reports_instead_of_doing_nothing(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        cx.run_until_parked();
        let events: std::rc::Rc<std::cell::RefCell<Vec<ErDiagramEvent>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &ErDiagramEvent, _cx| {
                seen.borrow_mut().push(event.clone());
            })
            .detach();
        });
        cx.update(|_window, app| {
            view.update(app, |view, cx| {
                view.graph = None;
                view.save_export("svg", cx);
            });
        });
        let reported = events.borrow().iter().any(|e| {
            matches!(e, ErDiagramEvent::Status { message, .. } if message.contains("Nothing to export"))
        });
        assert!(reported, "a save with no graph must say so, not no-op");
    }

    #[gpui::test]
    fn click_without_move_still_reveals(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        cx.run_until_parked();
        let events: std::rc::Rc<std::cell::RefCell<Vec<ErDiagramEvent>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &ErDiagramEvent, _cx| {
                seen.borrow_mut().push(event.clone());
            })
            .detach();
        });
        cx.update(|window, app| {
            view.update(app, |view, cx| {
                seed_two_node_graph(view, cx);
                let (x0, y0) = view.position_of(0);
                let start = point(px(10.0 + x0 + 5.0), px(20.0 + y0 + 5.0));
                view.on_canvas_mouse_down(&mouse_down(start), window, cx);
                view.on_canvas_mouse_up(&mouse_up(start), window, cx);
            });
        });
        assert!(
            events
                .borrow()
                .iter()
                .any(|e| matches!(e, ErDiagramEvent::RevealObject { .. })),
            "click without move should reveal"
        );
    }

    #[gpui::test]
    fn pop_out_emits_event_carrying_the_selection(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        cx.run_until_parked();
        let events: std::rc::Rc<std::cell::RefCell<Vec<ErDiagramEvent>>> =
            std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = events.clone();
        cx.update(|_window, cx| {
            cx.subscribe(&view, move |_view, event: &ErDiagramEvent, _cx| {
                seen.borrow_mut().push(event.clone());
            })
            .detach();
        });
        view.update(cx, |view, cx| {
            seed_two_node_graph(view, cx);
            // Default view pops out with `None` so the window keeps the
            // FK-connected cap semantics.
            view.pop_out(cx);
            // An explicit pick carries over verbatim.
            view.toggle_table(("s".into(), "b".into()), cx);
            view.pop_out(cx);
        });
        let events = events.borrow();
        let popouts: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ErDiagramEvent::PopOut { selection, .. } => Some(selection.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(popouts.len(), 2);
        assert_eq!(
            popouts[0], None,
            "default view must not bake in a selection"
        );
        let picked = popouts[1].as_ref().expect("explicit selection carries");
        assert!(picked.contains(&("s".to_string(), "a".to_string())));
        assert!(!picked.contains(&("s".to_string(), "b".to_string())));
    }

    #[gpui::test]
    fn toggling_a_table_rebuilds_the_graph_from_retained_fk_rows(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            seed_two_node_graph(view, cx);
            assert_eq!(view.graph.as_ref().unwrap().nodes.len(), 2);
            // Uncheck `b`: first toggle promotes the displayed set to an
            // explicit selection, then removes the table and its edge.
            view.toggle_table(("s".into(), "b".into()), cx);
            let graph = view.graph.as_ref().unwrap();
            assert_eq!(graph.nodes.len(), 1);
            assert_eq!(graph.nodes[0].table, "a");
            assert!(graph.edges.is_empty());
            // Check it back on: the edge comes back from the retained rows.
            view.toggle_table(("s".into(), "b".into()), cx);
            let graph = view.graph.as_ref().unwrap();
            assert_eq!(graph.nodes.len(), 2);
            assert_eq!(graph.edges.len(), 1);
            // Empty selection empties the graph (the body explains why).
            view.select_no_tables(cx);
            assert!(view.graph.as_ref().unwrap().nodes.is_empty());
            // FK-connected reset drops the explicit selection entirely.
            view.select_default_tables(cx);
            assert!(view.selection.is_none());
            assert_eq!(view.graph.as_ref().unwrap().nodes.len(), 2);
        });
    }

    #[gpui::test]
    fn focus_hides_unrelated_nodes(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.pending_focus = Some(("s".into(), "a".into()));
            view.apply_graph_for_test(
                build_graph(&[
                    fk("ab", "s", "b", "a_id", "s", "a", "id"),
                    fk("bc", "s", "c", "b_id", "s", "b", "id"),
                ]),
                cx,
            );
            let visible = view.visible.as_ref().expect("focus should set visible");
            assert_eq!(visible.len(), 2, "end node neighbourhood is self + 1 hop");
            assert!(view.focus.is_some());
        });
    }

    #[gpui::test]
    fn export_mermaid_uses_current_graph(cx: &mut gpui::TestAppContext) {
        let (view, cx) = open_view(cx);
        cx.run_until_parked();
        view.update(cx, |view, cx| {
            view.apply_graph_for_test(
                build_graph(&[fk(
                    "fk", "public", "orders", "user_id", "public", "users", "id",
                )]),
                cx,
            );
            let text = view.export_text_for_test("mermaid");
            assert!(text.contains("erDiagram"), "{text}");
        });
    }

    // -- column display -------------------------------------------------------

    fn bulk_col(
        schema: &str,
        object: &str,
        column: &str,
        ty: &str,
        nullable: bool,
    ) -> BulkColumnRow {
        BulkColumnRow {
            schema: schema.to_string(),
            object: object.to_string(),
            column: column.to_string(),
            type_name: Some(ty.to_string()),
            nullable: Some(nullable),
            is_identity: false,
        }
    }

    #[test]
    fn attach_columns_maps_types_nullability_and_key_flags() {
        let mut graph = build_graph(&[fk(
            "fk", "public", "orders", "user_id", "public", "users", "id",
        )]);
        attach_columns(
            &mut graph,
            &[
                bulk_col("public", "orders", "id", "int", false),
                bulk_col("public", "orders", "user_id", "int", true),
                bulk_col("public", "users", "id", "int", false),
                bulk_col("public", "elsewhere", "x", "text", true),
            ],
        );
        let orders = graph
            .nodes
            .iter()
            .find(|n| n.table == "orders")
            .expect("orders node");
        assert_eq!(orders.columns.len(), 2);
        assert_eq!(orders.columns[0].name, "id");
        assert_eq!(orders.columns[0].data_type, "int");
        assert_eq!(orders.columns[0].nullable, Some(false));
        assert!(
            !orders.columns[0].in_key,
            "id is not an FK column on orders"
        );
        assert!(orders.columns[1].in_key, "user_id participates in the FK");
        assert_eq!(orders.columns[1].nullable, Some(true));
    }

    #[test]
    fn node_height_grows_with_columns_and_caps_the_row_list() {
        let mut graph = build_graph(&[fk(
            "fk", "public", "orders", "user_id", "public", "users", "id",
        )]);
        // No columns attached: legacy fixed height.
        assert_eq!(node_height_for(&graph, true), NODE_H);
        // Column rows hidden: legacy height regardless of data.
        let cols: Vec<BulkColumnRow> = (0..20)
            .map(|i| bulk_col("public", "orders", &format!("c{i}"), "int", false))
            .collect();
        attach_columns(&mut graph, &cols);
        assert_eq!(node_height_for(&graph, false), NODE_H);
        // Every column gets a row — the card is sized to show the whole
        // table, not a truncated preview of it.
        let rows = node_row_count(
            graph.nodes.iter().find(|n| n.table == "orders").unwrap(),
            true,
        );
        assert_eq!(rows, 20);
        let expected = NODE_HEADER_H + rows as f32 * NODE_COL_ROW_H + NODE_COL_PAD;
        assert_eq!(node_height_for(&graph, true), expected);
    }

    /// The ceiling only exists to stop one pathological table from making
    /// every card unusably tall; it is far above any real schema.
    #[test]
    fn column_rows_stop_at_the_safety_ceiling() {
        let mut graph = build_graph(&[fk(
            "fk", "public", "orders", "user_id", "public", "users", "id",
        )]);
        let cols: Vec<BulkColumnRow> = (0..MAX_NODE_COLUMNS + 5)
            .map(|i| bulk_col("public", "orders", &format!("c{i}"), "int", false))
            .collect();
        attach_columns(&mut graph, &cols);
        let orders = graph.nodes.iter().find(|n| n.table == "orders").unwrap();
        assert_eq!(node_row_count(orders, true), MAX_NODE_COLUMNS + 1);
    }

    /// The right-hand detail column carries whatever the view asked for, and
    /// says nothing at all when both switches are off.
    #[test]
    fn column_detail_follows_the_view_switches() {
        let nullable = ErColumn {
            name: "email".into(),
            data_type: "nvarchar(200)".into(),
            nullable: Some(true),
            in_key: false,
        };
        let required = ErColumn {
            nullable: Some(false),
            ..nullable.clone()
        };
        let unknown = ErColumn {
            nullable: None,
            ..nullable.clone()
        };
        assert_eq!(column_detail(&nullable, false, false), "");
        assert_eq!(column_detail(&nullable, true, false), "nvarchar(200)");
        assert_eq!(column_detail(&nullable, false, true), "NULL");
        assert_eq!(column_detail(&required, false, true), "NOT NULL");
        assert_eq!(
            column_detail(&required, true, true),
            "nvarchar(200) NOT NULL"
        );
        // Unknown nullability says nothing rather than guessing.
        assert_eq!(column_detail(&unknown, true, true), "nvarchar(200)");
        // A column with no reported type contributes nothing either.
        let typeless = ErColumn {
            data_type: String::new(),
            ..nullable.clone()
        };
        assert_eq!(column_detail(&typeless, true, false), "");
    }

    #[test]
    fn visible_column_row_finds_every_painted_row() {
        let mut graph = build_graph(&[fk(
            "fk", "public", "orders", "user_id", "public", "users", "id",
        )]);
        let cols: Vec<BulkColumnRow> = (0..30)
            .map(|i| bulk_col("public", "orders", &format!("c{i}"), "int", false))
            .collect();
        attach_columns(&mut graph, &cols);
        let orders = graph.nodes.iter().find(|n| n.table == "orders").unwrap();
        // Every column is painted now, so every column can anchor an edge.
        assert_eq!(visible_column_row(orders, "c0"), Some(0));
        assert_eq!(visible_column_row(orders, "c29"), Some(29));
        assert_eq!(visible_column_row(orders, "missing"), None);
    }
}
