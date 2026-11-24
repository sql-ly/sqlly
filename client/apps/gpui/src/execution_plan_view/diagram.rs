//! The Diagram view: the operator tree as cards on a canvas that pans and
//! zooms.
//!
//! The layout comes from `plan_diagram`. It is computed when the plan, the
//! orientation, the heat metric, the folded set or the interface language
//! changes, and kept as a [`Scene`] with every string already localised.
//! Painting walks the scene, skips what is outside the viewport, and draws
//! connectors as plain rectangles (they are right-angled), so a plan of
//! several hundred operators costs a few hundred rectangle and text calls per
//! frame and no layout work.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use gpui::{
    AnyElement, App, BorderStyle, Bounds, ContentMask, Context, Corners, Edges, Focusable as _,
    Font, FontWeight, Hsla, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    PaintQuad, Pixels, Point, SharedString, Styled, TextAlign, TextRun, Window, canvas, div, point,
    prelude::*, px, size,
};
use gpui_component::Selectable as _;
use gpui_component::Sizable as _;
use gpui_component::button::Button;
use gpui_component::input::{Input, InputEvent, InputState};
use sqlly_services::plans::diagram::DIAGRAM_SIDE_ANCHOR;
use sqlly_services::plans::{
    DiagramNode, DiagramOptions, DiagramOrientation, FindingSeverity, PlanDiagram, PlanMetric,
    plan_diagram,
};

use super::summary::bottleneck_tag;
use super::{ExecutionPlanView, StatementPlans, glyphs, heat_color, is_wide, mix};
use crate::theme::Theme;

/// Card width in layout units. One unit is one pixel at 100% zoom and the
/// default interface font size.
const CARD_WIDTH: f32 = 224.0;
const ZOOM_MIN: f32 = 0.02;
const ZOOM_MAX: f32 = 3.0;
/// The zoom a large plan opens at when fitting it whole would make the
/// cards unreadable.
const ZOOM_READABLE: f32 = 0.6;
/// Below this text size in pixels the cards drop their text and become
/// heat-coloured blocks.
const TEXT_MIN_PX: f32 = 6.5;
/// Below this text size the connector row counts are left out.
const EDGE_LABEL_MIN_PX: f32 = 8.0;
const MINIMAP_W: f32 = 160.0;
const MINIMAP_H: f32 = 104.0;
const FOLD_SIZE: f32 = 14.0;

/// A laid-out plan with everything the painter prints already resolved.
pub(super) struct Scene {
    pub(super) diagram: PlanDiagram,
    cards: Vec<Card>,
    edge_labels: Vec<Option<SharedString>>,
    group_labels: Vec<SharedString>,
    bottleneck_tag: SharedString,
    chrome_revision: u64,
    revision: u64,
    /// Card lines shortened with an ellipsis to fit the card, by card index
    /// and line (0 title, 1 subtitle), measured at the first paint. Text
    /// width grows with the zoom like the card does, so a fit holds at every
    /// zoom.
    fitted: RefCell<HashMap<(usize, u8), Fitted>>,
}

#[cfg(test)]
impl Scene {
    /// The mark painted on `node_id`'s card.
    pub(super) fn card_severity(&self, node_id: i32) -> Option<FindingSeverity> {
        let index = self
            .diagram
            .nodes
            .iter()
            .position(|node| node.node_id == node_id)?;
        self.cards[index].severity
    }
}

/// A card line as painted.
#[derive(Clone)]
struct Fitted {
    text: SharedString,
    /// The line was shortened; hovering the card shows it whole.
    clipped: bool,
}

struct Card {
    title: SharedString,
    subtitle: Option<SharedString>,
    stats: Option<SharedString>,
    summary: Option<SharedString>,
    share_label: Option<SharedString>,
    /// The worst finding on the operator. Only findings mark a card: the
    /// warnings parsers attach to scans and lookups are heuristics the
    /// findings already weigh, so a mark the Findings list does not explain
    /// would contradict it.
    severity: Option<FindingSeverity>,
}

/// Operators whose name, table or index contains the search text.
pub(super) struct SearchMatches {
    query: String,
    /// In execution order, so Enter walks the plan the way the story does.
    pub(super) ordered: Vec<i32>,
    set: HashSet<i32>,
}

#[derive(Clone, Copy)]
enum Drag {
    Pan {
        start: Point<Pixels>,
        pan: (f32, f32),
    },
    Minimap,
}

pub(super) struct DiagramState {
    pub(super) orientation: DiagramOrientation,
    /// `None` colours by the plan's primary metric.
    pub(super) metric: Option<PlanMetric>,
    pub(super) collapsed: HashSet<i32>,
    /// Bumped whenever the layout inputs change.
    revision: u64,
    /// User zoom; the painted scale also follows the interface font size.
    zoom: Cell<f32>,
    /// Canvas-relative position of the layout origin, in pixels.
    pan: Cell<(f32, f32)>,
    pub(super) bounds: Cell<Option<Bounds<Pixels>>>,
    pub(super) scene: Option<Rc<Scene>>,
    drag: Option<Drag>,
    pub(super) search: String,
    pub(super) matches: Option<Rc<SearchMatches>>,
    search_cursor: usize,
    /// Bring this operator into view at the next paint, once the canvas
    /// size is known.
    pub(super) reveal_pending: Cell<Option<i32>>,
    /// Fit the plan at the next paint: `Some(true)` fits it whole,
    /// `Some(false)` opens it at a readable zoom on the root.
    fit_pending: Cell<Option<bool>>,
    /// Keep this operator where it was on screen across a re-layout.
    anchor_pending: Cell<Option<(i32, f32, f32)>>,
    /// The card under the pointer and where the pointer is, canvas-relative,
    /// for the full-name label of a shortened card.
    hovered: Option<(i32, Point<Pixels>)>,
}

impl Default for DiagramState {
    fn default() -> Self {
        Self {
            orientation: DiagramOrientation::default(),
            metric: None,
            collapsed: HashSet::new(),
            revision: 0,
            zoom: Cell::new(1.0),
            pan: Cell::new((0.0, 0.0)),
            bounds: Cell::new(None),
            scene: None,
            drag: None,
            search: String::new(),
            matches: None,
            search_cursor: 0,
            hovered: None,
            reveal_pending: Cell::new(None),
            fit_pending: Cell::new(Some(false)),
            anchor_pending: Cell::new(None),
        }
    }
}

/// Pixels per layout unit at the current zoom and interface font size.
fn scale_for(zoom: f32) -> f32 {
    zoom * Theme::font_size() / 13.0
}

impl DiagramState {
    /// Forget everything tied to one statement. Orientation, the heat
    /// metric and the search text are the reader's choices and carry over.
    pub(super) fn reset(&mut self) {
        self.collapsed.clear();
        self.revision += 1;
        self.scene = None;
        self.drag = None;
        self.matches = None;
        self.search_cursor = 0;
        self.zoom.set(1.0);
        self.pan.set((0.0, 0.0));
        self.reveal_pending.set(None);
        self.fit_pending.set(Some(false));
        self.anchor_pending.set(None);
        self.hovered = None;
    }

    /// Unfold every ancestor of `node_id` so it is on the canvas.
    pub(super) fn expand_to(&mut self, node_id: i32, active: Option<&StatementPlans>) {
        let Some(active) = active else { return };
        let mut changed = false;
        let mut at = active
            .analysis
            .node(node_id)
            .and_then(|node| node.parent_id);
        while let Some(id) = at {
            changed |= self.collapsed.remove(&id);
            at = active.analysis.node(id).and_then(|node| node.parent_id);
        }
        if changed {
            self.revision += 1;
        }
    }

    pub(super) fn zoom(&self) -> f32 {
        self.zoom.get()
    }

    fn scale(&self) -> f32 {
        scale_for(self.zoom.get())
    }

    /// The scene for `active`, rebuilt only when something it depends on
    /// changed.
    pub(super) fn ensure_scene(&mut self, active: &StatementPlans) -> Rc<Scene> {
        let chrome = crate::localization::chrome_revision();
        if let Some(scene) = &self.scene
            && scene.revision == self.revision
            && scene.chrome_revision == chrome
        {
            return scene.clone();
        }
        // A metric the plan does not carry (left over from another
        // statement) falls back to the plan's own.
        let metric = self
            .metric
            .filter(|metric| available_metrics(active).contains(metric));
        let options = DiagramOptions {
            orientation: self.orientation,
            collapsed: self.collapsed.clone(),
            metric,
            node_width: CARD_WIDTH,
            ..DiagramOptions::default()
        };
        let diagram = plan_diagram(&active.plan, &active.analysis, &options);
        let say = |message| SharedString::from(crate::localization::message(message));
        let cards = diagram
            .nodes
            .iter()
            .map(|node| Card {
                // Whole: the painter shortens what does not fit the card,
                // and a hover shows it in full.
                title: node.title.clone().into(),
                subtitle: node.subtitle.clone().map(SharedString::from),
                stats: node.stats.as_ref().map(say),
                summary: node.summary.as_ref().map(say),
                share_label: node.share_label.clone().map(SharedString::from),
                severity: active.node_severity(node.node_id),
            })
            .collect();
        let edge_labels = diagram
            .edges
            .iter()
            .map(|edge| {
                let label = edge.rows_label.as_deref()?;
                Some(if edge.rows_estimated && active.analysis.has_actuals {
                    // An estimate on a measured plan (the input never ran).
                    SharedString::from(format!("≈{label}"))
                } else {
                    SharedString::from(label.to_string())
                })
            })
            .collect();
        let group_labels = diagram
            .groups
            .iter()
            .map(|group| say(&group.label))
            .collect();
        let scene = Rc::new(Scene {
            diagram,
            cards,
            edge_labels,
            group_labels,
            bottleneck_tag: bottleneck_tag().into(),
            chrome_revision: chrome,
            revision: self.revision,
            fitted: RefCell::new(HashMap::new()),
        });
        self.scene = Some(scene.clone());
        scene
    }

    /// Canvas-relative pixel position of a layout point.
    fn to_canvas(&self, x: f32, y: f32) -> (f32, f32) {
        let (px_, py) = self.pan.get();
        let scale = self.scale();
        (px_ + x * scale, py + y * scale)
    }

    /// Layout coordinates of a window position.
    fn to_layout(&self, position: Point<Pixels>) -> Option<(f32, f32)> {
        let bounds = self.bounds.get()?;
        let (px_, py) = self.pan.get();
        let scale = self.scale();
        Some((
            (f32::from(position.x - bounds.origin.x) - px_) / scale,
            (f32::from(position.y - bounds.origin.y) - py) / scale,
        ))
    }

    fn node_at(&self, position: Point<Pixels>) -> Option<&DiagramNode> {
        let (x, y) = self.to_layout(position)?;
        let scene = self.scene.as_ref()?;
        scene.diagram.nodes.iter().rev().find(|node| {
            x >= node.x && x <= node.x + node.width && y >= node.y && y <= node.y + node.height
        })
    }

    /// The fold toggle under a window position.
    fn fold_at(&self, position: Point<Pixels>) -> Option<i32> {
        let (x, y) = self.to_layout(position)?;
        let scene = self.scene.as_ref()?;
        scene
            .diagram
            .nodes
            .iter()
            .filter(|node| node.has_children)
            .find(|node| {
                let (fx, fy) = fold_origin(node, scene.diagram.orientation);
                x >= fx - 2.0
                    && x <= fx + FOLD_SIZE + 2.0
                    && y >= fy - 2.0
                    && y <= fy + FOLD_SIZE + 2.0
            })
            .map(|node| node.node_id)
    }

    /// Zoom to `zoom`, keeping the canvas-relative point `about` fixed.
    fn zoom_about(&self, zoom: f32, about: (f32, f32)) {
        let zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
        let old = self.scale();
        let new = scale_for(zoom);
        let (px_, py) = self.pan.get();
        self.pan.set((
            about.0 - (about.0 - px_) * new / old,
            about.1 - (about.1 - py) * new / old,
        ));
        self.zoom.set(zoom);
    }

    fn zoom_about_center(&self, zoom: f32) {
        let center = self.bounds.get().map_or((0.0, 0.0), |bounds| {
            (
                f32::from(bounds.size.width) / 2.0,
                f32::from(bounds.size.height) / 2.0,
            )
        });
        self.zoom_about(zoom, center);
    }

    /// Apply the pending fit, anchor and reveal now that the canvas size
    /// is known. Runs during prepaint; everything it touches is a `Cell`.
    fn settle(&self, scene: &Scene, bounds: Bounds<Pixels>) {
        let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        if bw <= 1.0 || bh <= 1.0 {
            return;
        }
        let diagram = &scene.diagram;
        if let Some(whole) = self.fit_pending.take()
            && diagram.width > 0.0
            && diagram.height > 0.0
        {
            let unit = scale_for(1.0);
            let fit = ((bw - 16.0) / (diagram.width * unit))
                .min((bh - 16.0) / (diagram.height * unit))
                .clamp(ZOOM_MIN, 1.0);
            let zoom = if whole { fit } else { fit.max(ZOOM_READABLE) };
            self.zoom.set(zoom);
            let scale = scale_for(zoom);
            let (dw, dh) = (diagram.width * scale, diagram.height * scale);
            // Centre what fits; otherwise start on the root, which is at
            // the top (or the left) of the layout.
            let root = diagram.nodes.first();
            let pan_x = if dw <= bw {
                (bw - dw) / 2.0
            } else {
                match (diagram.orientation, root) {
                    (DiagramOrientation::RootTop, Some(root)) => {
                        bw / 2.0 - (root.x + root.width / 2.0) * scale
                    }
                    _ => 8.0,
                }
            };
            let pan_y = if dh <= bh {
                (bh - dh) / 2.0
            } else {
                match (diagram.orientation, root) {
                    (DiagramOrientation::RootLeft, Some(root)) => {
                        bh / 2.0 - (root.y + root.height / 2.0) * scale
                    }
                    _ => 8.0,
                }
            };
            self.pan.set((pan_x, pan_y));
        }
        if let Some((node_id, sx, sy)) = self.anchor_pending.take()
            && let Some(node) = diagram.node(node_id)
        {
            let scale = self.scale();
            self.pan.set((sx - node.x * scale, sy - node.y * scale));
        }
        if let Some(node_id) = self.reveal_pending.take()
            && let Some(node) = diagram.node(node_id)
        {
            let scale = self.scale();
            let (mut pan_x, mut pan_y) = self.pan.get();
            let (x0, y0) = (pan_x + node.x * scale, pan_y + node.y * scale);
            let (x1, y1) = (x0 + node.width * scale, y0 + node.height * scale);
            let margin = 24.0;
            // Move only as far as needed, so stepping through neighbours
            // does not make the canvas jump.
            if x1 - x0 > bw - 2.0 * margin {
                pan_x += bw / 2.0 - (x0 + x1) / 2.0;
            } else if x0 < margin {
                pan_x += margin - x0;
            } else if x1 > bw - margin {
                pan_x -= x1 - (bw - margin);
            }
            if y1 - y0 > bh - 2.0 * margin {
                pan_y += bh / 2.0 - (y0 + y1) / 2.0;
            } else if y0 < margin {
                pan_y += margin - y0;
            } else if y1 > bh - margin {
                pan_y -= y1 - (bh - margin);
            }
            self.pan.set((pan_x, pan_y));
        }
    }

    /// The minimap's rectangle (canvas-relative) and its pixels per layout
    /// unit, when the plan is larger than the canvas.
    fn minimap(&self, scene: &Scene, bounds: Bounds<Pixels>) -> Option<(f32, f32, f32, f32, f32)> {
        let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let diagram = &scene.diagram;
        let scale = self.scale();
        if diagram.width <= 0.0 || diagram.height <= 0.0 {
            return None;
        }
        if diagram.width * scale <= bw && diagram.height * scale <= bh {
            return None;
        }
        let (mw, mh) = (Theme::scaled(MINIMAP_W), Theme::scaled(MINIMAP_H));
        if bw < mw * 2.5 || bh < mh * 2.0 {
            return None;
        }
        let unit = (mw / diagram.width).min(mh / diagram.height);
        let (w, h) = (diagram.width * unit, diagram.height * unit);
        Some((bw - w - 10.0, bh - h - 10.0, w, h, unit))
    }

    /// Centre the canvas on the layout point under a minimap position.
    fn minimap_jump(&self, position: Point<Pixels>) -> bool {
        let (Some(bounds), Some(scene)) = (self.bounds.get(), self.scene.as_ref()) else {
            return false;
        };
        let Some((mx, my, mw, mh, unit)) = self.minimap(scene, bounds) else {
            return false;
        };
        let x = f32::from(position.x - bounds.origin.x);
        let y = f32::from(position.y - bounds.origin.y);
        if x < mx || x > mx + mw || y < my || y > my + mh {
            return false;
        }
        let (lx, ly) = ((x - mx) / unit, (y - my) / unit);
        let scale = self.scale();
        self.pan.set((
            f32::from(bounds.size.width) / 2.0 - lx * scale,
            f32::from(bounds.size.height) / 2.0 - ly * scale,
        ));
        true
    }
}

/// Top-left corner of a card's fold toggle, in layout units: on the edge
/// its inputs attach to.
fn fold_origin(node: &DiagramNode, orientation: DiagramOrientation) -> (f32, f32) {
    match orientation {
        DiagramOrientation::RootTop => (
            node.x + node.width / 2.0 - FOLD_SIZE / 2.0,
            node.y + node.height - FOLD_SIZE / 2.0,
        ),
        DiagramOrientation::RootLeft => (
            node.x + node.width - FOLD_SIZE / 2.0,
            node.y + DIAGRAM_SIDE_ANCHOR - FOLD_SIZE / 2.0,
        ),
    }
}

/// The heat metrics this plan can be coloured by.
pub(super) fn available_metrics(active: &StatementPlans) -> Vec<PlanMetric> {
    let analysis = &active.analysis;
    let mut metrics = Vec::with_capacity(5);
    if analysis.has_timing {
        metrics.push(PlanMetric::Time);
    }
    if analysis.has_costs {
        metrics.push(PlanMetric::Cost);
    }
    metrics.push(PlanMetric::Rows);
    if analysis.has_actuals && analysis.has_estimates {
        metrics.push(PlanMetric::EstimateError);
    }
    if analysis.has_io {
        metrics.push(PlanMetric::Io);
    }
    metrics
}

/// Find the operators matching `query` (case-insensitive) by operator name,
/// table, index or alias.
pub(super) fn search_plan(active: &StatementPlans, query: &str) -> SearchMatches {
    let needle = query.trim().to_lowercase();
    let mut ordered = Vec::new();
    if !needle.is_empty() {
        let order: Vec<i32> = if active.analysis.execution_order.is_empty() {
            active.analysis.nodes.iter().map(|n| n.node_id).collect()
        } else {
            active.analysis.execution_order.clone()
        };
        for node_id in order {
            let Some(node) = active.analysis.node(node_id) else {
                continue;
            };
            let object = node.object.as_ref();
            let hit = node.operator.name.to_lowercase().contains(&needle)
                || crate::localization::message(node.title())
                    .to_lowercase()
                    .contains(&needle)
                || object.is_some_and(|object| {
                    object.label().to_lowercase().contains(&needle)
                        || object
                            .alias
                            .as_deref()
                            .is_some_and(|alias| alias.to_lowercase().contains(&needle))
                })
                || node
                    .index
                    .as_deref()
                    .is_some_and(|index| index.to_lowercase().contains(&needle));
            if hit {
                ordered.push(node_id);
            }
        }
    }
    SearchMatches {
        query: query.to_string(),
        set: ordered.iter().copied().collect(),
        ordered,
    }
}

/// Everything one frame needs, gathered before painting.
struct Frame {
    scene: Rc<Scene>,
    scale: f32,
    pan: (f32, f32),
    selected: Option<i32>,
    finding_nodes: Rc<HashSet<i32>>,
    matches: Option<Rc<SearchMatches>>,
    minimap: Option<(f32, f32, f32, f32, f32)>,
}

#[allow(clippy::too_many_arguments)]
fn quad(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    fill: Hsla,
    border: Hsla,
    width: f32,
    radius: f32,
) -> PaintQuad {
    PaintQuad {
        bounds: Bounds {
            origin: point(px(x), px(y)),
            size: size(px(w), px(h)),
        },
        background: fill.into(),
        border_color: border,
        border_widths: Edges::all(px(width)),
        corner_radii: Corners::all(px(radius)),
        border_style: BorderStyle::default(),
    }
}

fn fill(x: f32, y: f32, w: f32, h: f32, color: Hsla) -> PaintQuad {
    quad(x, y, w, h, color, gpui::transparent_black(), 0.0, 0.0)
}

/// Shape and paint one line of text; returns its width. The text system
/// caches shaped lines between frames, so repainting the same card text is a
/// lookup.
#[allow(clippy::too_many_arguments)]
fn paint_line(
    text: &SharedString,
    x: f32,
    y: f32,
    font_px: f32,
    color: Hsla,
    font: &Font,
    right_align_to: Option<f32>,
    window: &mut Window,
    cx: &mut App,
) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let run = TextRun {
        len: text.len(),
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let line = window
        .text_system()
        .clone()
        .shape_line(text.clone(), px(font_px), &[run], None);
    let width = f32::from(line.width);
    let x = right_align_to.map_or(x, |right| right - width);
    let _ = line.paint(
        point(px(x), px(y)),
        px(font_px * 1.4),
        TextAlign::Left,
        None,
        window,
        cx,
    );
    width
}

/// `text` as it fits in `max_width` at `font_px`: whole, or cut on a
/// character boundary and ended with an ellipsis. Measured with the same
/// shaping the painter uses, once per card line; the result is kept in the
/// scene.
fn fit_line(
    scene: &Scene,
    key: (usize, u8),
    text: &SharedString,
    max_width: f32,
    font_px: f32,
    font: &Font,
    window: &mut Window,
) -> SharedString {
    if let Some(fitted) = scene.fitted.borrow().get(&key) {
        return fitted.text.clone();
    }
    let measure = |candidate: &SharedString, window: &mut Window| -> f32 {
        let run = TextRun {
            len: candidate.len(),
            font: font.clone(),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        f32::from(
            window
                .text_system()
                .clone()
                .shape_line(candidate.clone(), px(font_px), &[run], None)
                .width,
        )
    };
    let fitted = if max_width <= 0.0 || measure(text, window) <= max_width {
        Fitted {
            text: text.clone(),
            clipped: false,
        }
    } else {
        // The longest prefix that still fits with the ellipsis.
        let boundaries: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
        let (mut low, mut high) = (0usize, boundaries.len());
        let cut = |count: usize| -> SharedString {
            let end = boundaries.get(count).copied().unwrap_or(text.len());
            SharedString::from(format!("{}\u{2026}", text[..end].trim_end()))
        };
        while low < high {
            let mid = (low + high).div_ceil(2);
            if measure(&cut(mid), window) <= max_width {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        Fitted {
            text: cut(low),
            clipped: true,
        }
    };
    let text = fitted.text.clone();
    scene.fitted.borrow_mut().insert(key, fitted);
    text
}

fn paint_diagram(frame: &Frame, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let scene = &frame.scene;
    let diagram = &scene.diagram;
    let scale = frame.scale;
    let (ox, oy) = (
        f32::from(bounds.origin.x) + frame.pan.0,
        f32::from(bounds.origin.y) + frame.pan.1,
    );
    let (bw, bh) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
    // The viewport in layout units, for culling.
    let (vx0, vy0) = (-frame.pan.0 / scale, -frame.pan.1 / scale);
    let (vx1, vy1) = (vx0 + bw / scale, vy0 + bh / scale);
    let visible = |x: f32, y: f32, w: f32, h: f32| {
        x + w >= vx0 - 12.0 && x <= vx1 + 12.0 && y + h >= vy0 - 20.0 && y <= vy1 + 12.0
    };
    let at = |x: f32, y: f32| (ox + x * scale, oy + y * scale);

    let text = Theme::text();
    let dim = Theme::text_dim();
    let faint = Theme::text_disabled();
    let card_bg = Theme::bg();
    let border = Theme::border();
    let line_color = Theme::border_hover();
    let accent = Theme::accent();
    let error = Theme::error();
    let panel = Theme::panel_bg();
    let regular = Theme::ui_font();
    let mut strong = Theme::ui_font();
    strong.weight = FontWeight::SEMIBOLD;
    let title_px = 12.0 * scale;
    let line_px = 11.0 * scale;
    let small_px = 10.0 * scale;
    let show_text = line_px >= TEXT_MIN_PX;
    let searching = frame
        .matches
        .as_ref()
        .is_some_and(|matches| !matches.query.trim().is_empty());

    window.with_content_mask(Some(ContentMask { bounds }), |window| {
        // Frames around subplans, CTEs and parallel zones, behind everything.
        for (group, label) in diagram.groups.iter().zip(&scene.group_labels) {
            if !visible(group.x, group.y, group.width, group.height) {
                continue;
            }
            let (x, y) = at(group.x, group.y);
            let mut frame_quad = quad(
                x,
                y,
                group.width * scale,
                group.height * scale,
                Theme::row_alt(),
                line_color,
                1.0,
                6.0 * scale,
            );
            frame_quad.border_style = BorderStyle::Dashed;
            window.paint_quad(frame_quad);
            if small_px >= TEXT_MIN_PX {
                paint_line(
                    label,
                    x + 8.0 * scale,
                    y + 4.0 * scale,
                    small_px,
                    dim,
                    &regular,
                    None,
                    window,
                    cx,
                );
            }
        }

        // Connectors. Each is a right-angled polyline, so every segment is
        // one rectangle; width carries the row count.
        for (edge, label) in diagram.edges.iter().zip(&scene.edge_labels) {
            let Some((first, rest)) = edge.points.split_first() else {
                continue;
            };
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (first.0, first.1, first.0, first.1);
            for p in rest {
                min_x = min_x.min(p.0);
                min_y = min_y.min(p.1);
                max_x = max_x.max(p.0);
                max_y = max_y.max(p.1);
            }
            if !visible(min_x, min_y, max_x - min_x, max_y - min_y) {
                continue;
            }
            let weight = (edge.thickness * scale).max(1.0);
            let color = if edge.never_executed {
                faint
            } else if edge.on_critical_path {
                mix(line_color, text, 0.55)
            } else {
                line_color
            };
            let mut from = *first;
            for to in rest {
                let (x0, y0) = at(from.0.min(to.0), from.1.min(to.1));
                let (x1, y1) = at(from.0.max(to.0), from.1.max(to.1));
                window.paint_quad(fill(
                    x0 - weight / 2.0,
                    y0 - weight / 2.0,
                    (x1 - x0) + weight,
                    (y1 - y0) + weight,
                    color,
                ));
                from = *to;
            }
            // The heaviest path is marked by a core line as well as by its
            // darker colour, so it reads without colour.
            if edge.on_critical_path && !edge.never_executed && weight >= 3.0 {
                let mut from = *first;
                for to in rest {
                    let (x0, y0) = at(from.0.min(to.0), from.1.min(to.1));
                    let (x1, y1) = at(from.0.max(to.0), from.1.max(to.1));
                    window.paint_quad(fill(
                        x0 - 0.5,
                        y0 - 0.5,
                        (x1 - x0) + 1.0,
                        (y1 - y0) + 1.0,
                        panel,
                    ));
                    from = *to;
                }
            }
            if let Some(label) = label
                && small_px >= EDGE_LABEL_MIN_PX
            {
                let (x, y) = at(edge.label_x, edge.label_y);
                // A plate behind the number keeps it legible on the line.
                let width = label.chars().count() as f32 * small_px * 0.62 + 6.0;
                window.paint_quad(quad(
                    x - width / 2.0,
                    y - small_px * 0.75,
                    width,
                    small_px * 1.5,
                    panel,
                    gpui::transparent_black(),
                    0.0,
                    3.0,
                ));
                paint_line(
                    label,
                    x - width / 2.0 + 3.0,
                    y - small_px * 0.7,
                    small_px,
                    dim,
                    &regular,
                    None,
                    window,
                    cx,
                );
            }
        }

        // Cards.
        for (card_index, (node, card)) in diagram.nodes.iter().zip(&scene.cards).enumerate() {
            if !visible(node.x, node.y, node.width, node.height) {
                continue;
            }
            let (x, y) = at(node.x, node.y);
            let (w, h) = (node.width * scale, node.height * scale);
            let heat = heat_color(node.heat);
            let selected = frame.selected == Some(node.node_id);
            let in_finding = frame.finding_nodes.contains(&node.node_id);
            let matched = frame
                .matches
                .as_ref()
                .is_some_and(|matches| matches.set.contains(&node.node_id));
            let idle = node.flags.never_executed;

            if !show_text {
                // Far out: a block whose fill is the heat, with the
                // bottleneck and the selection still outlined.
                let outline = if selected {
                    accent
                } else if node.flags.bottleneck {
                    error
                } else {
                    border
                };
                let block = if idle {
                    card_bg
                } else {
                    mix(card_bg, heat, 0.25 + 0.75 * node.heat)
                };
                window.paint_quad(quad(
                    x,
                    y,
                    w,
                    h,
                    block,
                    outline,
                    if selected || node.flags.bottleneck {
                        2.0
                    } else {
                        1.0
                    },
                    2.0,
                ));
                if searching && matched {
                    window.paint_quad(quad(
                        x - 3.0,
                        y - 3.0,
                        w + 6.0,
                        h + 6.0,
                        gpui::transparent_black(),
                        Theme::match_highlight(),
                        2.0,
                        3.0,
                    ));
                }
                continue;
            }

            let radius = 5.0 * scale;
            if in_finding || (searching && matched) {
                let ring = if searching && matched {
                    Theme::match_highlight()
                } else {
                    Theme::warning()
                };
                window.paint_quad(quad(
                    x - 4.0,
                    y - 4.0,
                    w + 8.0,
                    h + 8.0,
                    gpui::transparent_black(),
                    ring,
                    2.0,
                    radius + 4.0,
                ));
            }
            let (outline, outline_width) = if selected {
                (accent, 2.0)
            } else if node.flags.bottleneck {
                (error, 2.0)
            } else {
                (border, 1.0)
            };
            let body = if selected {
                mix(card_bg, accent, 0.10)
            } else {
                mix(card_bg, heat, 0.12 * node.heat)
            };
            window.paint_quad(quad(x, y, w, h, body, outline, outline_width, radius));

            // Heat stripe down the left edge.
            let stripe = 5.0 * scale;
            window.paint_quad(quad(
                x + outline_width,
                y + outline_width,
                stripe,
                h - 2.0 * outline_width,
                if idle { border } else { heat },
                gpui::transparent_black(),
                0.0,
                (radius - outline_width).max(0.0),
            ));

            let ink = if idle { faint } else { text };
            let soft = if idle { faint } else { dim };
            let left = x + 12.0 * scale;
            let right = x + w - 8.0 * scale;

            glyphs::category_glyph(node.category).paint(
                Bounds {
                    origin: point(px(left), px(y + 8.0 * scale)),
                    size: size(px(12.0 * scale), px(12.0 * scale)),
                },
                soft,
                window,
            );

            // Share of the plan, right-aligned on the title line.
            let mut title_right = right;
            if let Some(label) = &card.share_label {
                let width = paint_line(
                    label,
                    0.0,
                    y + 6.0 * scale,
                    line_px,
                    soft,
                    &regular,
                    Some(right),
                    window,
                    cx,
                );
                title_right = right - width - 6.0 * scale;
            }
            let title_left = left + 17.0 * scale;
            let clip = |x0: f32, x1: f32| ContentMask {
                bounds: Bounds {
                    origin: point(px(x0), px(y)),
                    size: size(px((x1 - x0).max(0.0)), px(h)),
                },
            };
            let title = fit_line(
                scene,
                (card_index, 0),
                &card.title,
                title_right - title_left,
                title_px,
                &strong,
                window,
            );
            let subtitle = card.subtitle.as_ref().map(|subtitle| {
                fit_line(
                    scene,
                    (card_index, 1),
                    subtitle,
                    right - left,
                    line_px,
                    &regular,
                    window,
                )
            });
            window.with_content_mask(Some(clip(title_left, title_right)), |window| {
                paint_line(
                    &title,
                    title_left,
                    y + 5.0 * scale,
                    title_px,
                    ink,
                    &strong,
                    None,
                    window,
                    cx,
                );
            });
            window.with_content_mask(Some(clip(left, right)), |window| {
                let mut line_y = y + 24.0 * scale;
                for line in [&subtitle, &card.stats, &card.summary]
                    .into_iter()
                    .flatten()
                {
                    paint_line(
                        line, left, line_y, line_px, soft, &regular, None, window, cx,
                    );
                    line_y += 16.0 * scale;
                }
            });

            // The bar repeats the heat as a length, so the metric reads
            // without colour.
            let bar_y = y + h - 7.0 * scale;
            let bar_h = (3.0 * scale).max(2.0);
            let bar_w = right - left;
            window.paint_quad(quad(
                left,
                bar_y,
                bar_w,
                bar_h,
                border,
                gpui::transparent_black(),
                0.0,
                bar_h / 2.0,
            ));
            if !idle && node.heat > 0.0 {
                window.paint_quad(quad(
                    left,
                    bar_y,
                    (bar_w * node.heat.clamp(0.0, 1.0)).max(bar_h),
                    bar_h,
                    heat,
                    gpui::transparent_black(),
                    0.0,
                    bar_h / 2.0,
                ));
            }

            if node.flags.bottleneck {
                paint_line(
                    &scene.bottleneck_tag,
                    x + 2.0 * scale,
                    y - 15.0 * scale,
                    small_px,
                    error,
                    &strong,
                    None,
                    window,
                    cx,
                );
            }

            // Finding badge on the top-right corner: a shape per severity.
            if let Some(severity) = card.severity {
                let badge = 15.0 * scale;
                let (bx, by) = (x + w - badge * 0.7, y - badge * 0.45);
                window.paint_quad(quad(
                    bx - 2.0,
                    by - 2.0,
                    badge + 4.0,
                    badge + 4.0,
                    panel,
                    gpui::transparent_black(),
                    0.0,
                    (badge + 4.0) / 2.0,
                ));
                glyphs::severity_glyph(severity).paint(
                    Bounds {
                        origin: point(px(bx), px(by)),
                        size: size(px(badge), px(badge)),
                    },
                    ExecutionPlanView::severity_color(severity),
                    window,
                );
            }

            if node.has_children {
                let (fx, fy) = fold_origin(node, diagram.orientation);
                let (fx, fy) = at(fx, fy);
                let side = FOLD_SIZE * scale;
                window.paint_quad(quad(
                    fx,
                    fy,
                    side,
                    side,
                    card_bg,
                    line_color,
                    1.0,
                    3.0 * scale,
                ));
                glyphs::fold_glyph(node.collapsed).paint(
                    Bounds {
                        origin: point(px(fx + side * 0.2), px(fy + side * 0.2)),
                        size: size(px(side * 0.6), px(side * 0.6)),
                    },
                    dim,
                    window,
                );
            }

            // While searching, everything that does not match steps back.
            if searching && !matched && !selected {
                window.paint_quad(quad(
                    x - 1.0,
                    y - 1.0,
                    w + 2.0,
                    h + 2.0,
                    panel.opacity(0.62),
                    gpui::transparent_black(),
                    0.0,
                    radius,
                ));
            }
        }

        // Minimap: the whole plan as heat-coloured specks, with the
        // viewport outlined.
        if let Some((mx, my, mw, mh, unit)) = frame.minimap {
            let (bx, by) = (
                f32::from(bounds.origin.x) + mx,
                f32::from(bounds.origin.y) + my,
            );
            window.paint_quad(quad(
                bx - 4.0,
                by - 4.0,
                mw + 8.0,
                mh + 8.0,
                Theme::popup_bg().opacity(0.94),
                Theme::popup_border(),
                1.0,
                4.0,
            ));
            for node in &diagram.nodes {
                let color = if frame.selected == Some(node.node_id) {
                    accent
                } else if node.flags.never_executed {
                    border
                } else {
                    heat_color(node.heat)
                };
                window.paint_quad(fill(
                    bx + node.x * unit,
                    by + node.y * unit,
                    (node.width * unit).max(1.5),
                    (node.height * unit).max(1.5),
                    color,
                ));
            }
            let clamp = |v: f32, lo: f32, hi: f32| v.max(lo).min(hi);
            let (x0, y0) = (clamp(vx0 * unit, 0.0, mw), clamp(vy0 * unit, 0.0, mh));
            let (x1, y1) = (clamp(vx1 * unit, 0.0, mw), clamp(vy1 * unit, 0.0, mh));
            window.paint_quad(quad(
                bx + x0,
                by + y0,
                (x1 - x0).max(4.0),
                (y1 - y0).max(4.0),
                accent.opacity(0.12),
                accent,
                1.0,
                1.0,
            ));
        }
    });
}

impl ExecutionPlanView {
    pub(super) fn toggle_fold(&mut self, node_id: i32, cx: &mut Context<Self>) {
        // Keep the card under the pointer where it is while its inputs
        // appear or disappear.
        if let Some(node) = self
            .diagram
            .scene
            .as_ref()
            .and_then(|scene| scene.diagram.node(node_id))
        {
            let (sx, sy) = self.diagram.to_canvas(node.x, node.y);
            self.diagram.anchor_pending.set(Some((node_id, sx, sy)));
        }
        if !self.diagram.collapsed.insert(node_id) {
            self.diagram.collapsed.remove(&node_id);
        }
        self.diagram.revision += 1;
        // A selection inside the folded subtree moves up to the fold.
        if let (Some(selected), Some(active)) = (
            self.selected_node,
            self.statements.get(self.active_statement),
        ) && self.diagram.collapsed.contains(&node_id)
        {
            let mut at = active.analysis.node(selected).and_then(|n| n.parent_id);
            while let Some(id) = at {
                if id == node_id {
                    self.selected_node = Some(node_id);
                    break;
                }
                at = active.analysis.node(id).and_then(|n| n.parent_id);
            }
        }
        cx.notify();
    }

    fn set_diagram_metric(&mut self, metric: PlanMetric, cx: &mut Context<Self>) {
        if self.diagram.metric != Some(metric) {
            self.diagram.metric = Some(metric);
            self.diagram.revision += 1;
            cx.notify();
        }
    }

    pub(super) fn flip_diagram(&mut self, cx: &mut Context<Self>) {
        self.diagram.orientation = match self.diagram.orientation {
            DiagramOrientation::RootTop => DiagramOrientation::RootLeft,
            DiagramOrientation::RootLeft => DiagramOrientation::RootTop,
        };
        self.diagram.revision += 1;
        // The two layouts share nothing; start the new one from its root,
        // or from the selection when there is one.
        self.diagram.fit_pending.set(Some(false));
        self.diagram.reveal_pending.set(self.selected_node);
        cx.notify();
    }

    fn fit_diagram(&mut self, cx: &mut Context<Self>) {
        self.diagram.fit_pending.set(Some(true));
        cx.notify();
    }

    fn zoom_diagram(&mut self, factor: f32, cx: &mut Context<Self>) {
        self.diagram.zoom_about_center(self.diagram.zoom() * factor);
        cx.notify();
    }

    pub(super) fn set_diagram_search(&mut self, query: String, cx: &mut Context<Self>) {
        if self.diagram.search != query {
            self.diagram.search = query;
            self.diagram.matches = None;
            self.diagram.search_cursor = 0;
            cx.notify();
        }
    }

    /// Select the next (or previous) search match and bring it into view.
    pub(super) fn step_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(matches) = self.diagram.matches.clone() else {
            return;
        };
        if matches.ordered.is_empty() {
            return;
        }
        let count = matches.ordered.len();
        let current = self
            .selected_node
            .and_then(|id| matches.ordered.iter().position(|m| *m == id));
        let next = match current {
            Some(at) if forward => (at + 1) % count,
            Some(at) => (at + count - 1) % count,
            None => self.diagram.search_cursor.min(count - 1),
        };
        self.diagram.search_cursor = next;
        let node_id = matches.ordered[next];
        self.diagram
            .expand_to(node_id, self.statements.get(self.active_statement));
        self.select_node(Some(node_id), cx);
        self.diagram.reveal_pending.set(Some(node_id));
    }

    /// Arrow keys walk the tree the way it is drawn; Enter and Space fold;
    /// plus, minus and zero zoom.
    pub(super) fn diagram_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(active) = self.statements.get(self.active_statement) else {
            return false;
        };
        match key {
            "escape" => {
                if self.selected_node.is_none() {
                    return false;
                }
                self.select_node(None, cx);
                return true;
            }
            "=" | "+" => {
                self.zoom_diagram(1.2, cx);
                return true;
            }
            "-" => {
                self.zoom_diagram(1.0 / 1.2, cx);
                return true;
            }
            "0" => {
                self.fit_diagram(cx);
                return true;
            }
            _ => {}
        }
        let Some(selected) = self.selected_node else {
            if matches!(key, "up" | "down" | "left" | "right") {
                let root = active.analysis.roots.first().copied();
                if let Some(root) = root {
                    self.select_node(Some(root), cx);
                    self.diagram.reveal_pending.set(Some(root));
                    return true;
                }
            }
            return false;
        };
        let Some(node) = active.analysis.node(selected) else {
            return false;
        };
        if matches!(key, "enter" | "space") {
            if node.children.is_empty() {
                return false;
            }
            self.toggle_fold(selected, cx);
            return true;
        }
        let top = self.diagram.orientation == DiagramOrientation::RootTop;
        let (to_parent, to_child, to_previous, to_next) = if top {
            ("up", "down", "left", "right")
        } else {
            ("left", "right", "up", "down")
        };
        let sibling = |step: isize| -> Option<i32> {
            let siblings: &[i32] = match node.parent_id {
                Some(parent) => &active.analysis.node(parent)?.children,
                None => &active.analysis.roots,
            };
            let at = siblings.iter().position(|id| *id == selected)? as isize + step;
            usize::try_from(at)
                .ok()
                .and_then(|at| siblings.get(at))
                .copied()
        };
        let target = if key == to_parent {
            node.parent_id
        } else if key == to_child {
            if self.diagram.collapsed.contains(&selected) {
                None
            } else {
                node.children.first().copied()
            }
        } else if key == to_previous {
            sibling(-1)
        } else if key == to_next {
            sibling(1)
        } else {
            return false;
        };
        if let Some(target) = target {
            self.select_node(Some(target), cx);
            self.diagram.reveal_pending.set(Some(target));
        }
        // An arrow at the edge of the tree is still ours: it must not
        // scroll something else.
        true
    }

    fn on_diagram_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        if self.diagram.minimap_jump(event.position) {
            self.diagram.drag = Some(Drag::Minimap);
            cx.notify();
            return;
        }
        if let Some(node_id) = self.diagram.fold_at(event.position) {
            self.toggle_fold(node_id, cx);
            return;
        }
        let hit = self
            .diagram
            .node_at(event.position)
            .map(|node| (node.node_id, node.has_children));
        if let Some((node_id, has_children)) = hit {
            if event.click_count >= 2 && has_children {
                self.toggle_fold(node_id, cx);
            }
            self.select_node(Some(node_id), cx);
        }
        self.diagram.drag = Some(Drag::Pan {
            start: event.position,
            pan: self.diagram.pan.get(),
        });
        cx.notify();
    }

    fn on_diagram_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.pressed_button != Some(MouseButton::Left) {
            self.diagram.drag = None;
            let hovered = self
                .diagram
                .node_at(event.position)
                .map(|node| node.node_id);
            let at = self
                .diagram
                .bounds
                .get()
                .map(|bounds| event.position - bounds.origin);
            let next = hovered.zip(at);
            if next.map(|(id, _)| id) != self.diagram.hovered.map(|(id, _)| id)
                || (next.is_some() && self.hovered_card_is_clipped())
            {
                self.diagram.hovered = next;
                cx.notify();
            }
            return;
        }
        self.diagram.hovered = None;
        match self.diagram.drag {
            Some(Drag::Pan { start, pan }) => {
                self.diagram.pan.set((
                    pan.0 + f32::from(event.position.x - start.x),
                    pan.1 + f32::from(event.position.y - start.y),
                ));
                cx.notify();
            }
            Some(Drag::Minimap) if self.diagram.minimap_jump(event.position) => cx.notify(),
            Some(Drag::Minimap) | None => {}
        }
    }

    fn on_diagram_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.diagram.drag = None;
    }

    fn on_diagram_scroll(
        &mut self,
        event: &gpui::ScrollEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(px(24.0));
        let (dx, dy) = (f32::from(delta.x), f32::from(delta.y));
        if event.modifiers.platform || event.modifiers.control {
            if let Some(bounds) = self.diagram.bounds.get() {
                let about = (
                    f32::from(event.position.x - bounds.origin.x),
                    f32::from(event.position.y - bounds.origin.y),
                );
                self.diagram
                    .zoom_about(self.diagram.zoom() * (1.0 + dy / 200.0), about);
            }
        } else {
            let (pan_x, pan_y) = self.diagram.pan.get();
            self.diagram.pan.set((pan_x + dx, pan_y + dy));
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn on_diagram_pinch(
        &mut self,
        event: &gpui::PinchEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(bounds) = self.diagram.bounds.get() {
            let about = (
                f32::from(event.position.x - bounds.origin.x),
                f32::from(event.position.y - bounds.origin.y),
            );
            self.diagram
                .zoom_about(self.diagram.zoom() * (1.0 + event.delta), about);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn ensure_search_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search_input.is_some() {
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "query.execution_plan.search_placeholder",
            ))
        });
        self.search_subscription = Some(cx.subscribe_in(
            &input,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => {
                    let query = state.read(cx).value().to_string();
                    this.set_diagram_search(query, cx);
                }
                InputEvent::PressEnter { secondary, .. } => this.step_search(!*secondary, cx),
                _ => {}
            },
        ));
        self.search_input = Some(input);
    }

    /// True while the search box has the keyboard, so typing a digit or a
    /// minus sign there does not zoom the canvas.
    pub(super) fn search_has_focus(&self, window: &Window, cx: &App) -> bool {
        self.search_input
            .as_ref()
            .is_some_and(|input| input.focus_handle(cx).is_focused(window))
    }

    fn render_diagram_toolbar(
        &self,
        active: &StatementPlans,
        scene: &Scene,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        let shown_metric = scene.diagram.metric;
        let metrics = available_metrics(active);
        let match_count = self
            .diagram
            .matches
            .as_ref()
            .filter(|matches| !matches.query.trim().is_empty())
            .map(|matches| matches.ordered.len());
        div()
            .flex()
            .flex_row()
            .flex_shrink_0()
            .flex_wrap()
            .items_center()
            .min_h(px(s(30.0)))
            .px(px(s(8.0)))
            .py(px(s(3.0)))
            .gap(px(s(6.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("query.execution_plan.heat_label")),
            )
            .children(metrics.into_iter().enumerate().map(|(index, metric)| {
                Button::new(("plan-heat-metric", index))
                    .small()
                    .label(crate::localization::message(metric.label()))
                    .selected(shown_metric == Some(metric))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_diagram_metric(metric, cx);
                    }))
            }))
            .child(div().w(px(s(8.0))))
            .child(
                Button::new("plan-diagram-flip")
                    .small()
                    .label(match self.diagram.orientation {
                        DiagramOrientation::RootTop => crate::localization::text(
                            "query.execution_plan.layout_left_to_right_button",
                        ),
                        DiagramOrientation::RootLeft => {
                            crate::localization::text("query.execution_plan.layout_top_down_button")
                        }
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.flip_diagram(cx))),
            )
            .child(
                Button::new("plan-diagram-zoom-out")
                    .small()
                    .label("−")
                    .tooltip(crate::localization::text(
                        "query.execution_plan.zoom_out_tooltip",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_diagram(1.0 / 1.2, cx))),
            )
            .child(
                div()
                    .min_w(px(s(38.0)))
                    .text_center()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text_dim())
                    .child(format!("{:.0}%", self.diagram.zoom() * 100.0)),
            )
            .child(
                Button::new("plan-diagram-zoom-in")
                    .small()
                    .label("+")
                    .tooltip(crate::localization::text(
                        "query.execution_plan.zoom_in_tooltip",
                    ))
                    .on_click(cx.listener(|this, _, _, cx| this.zoom_diagram(1.2, cx))),
            )
            .child(
                Button::new("plan-diagram-fit")
                    .small()
                    .label(crate::localization::text("query.execution_plan.fit_button"))
                    .on_click(cx.listener(|this, _, _, cx| this.fit_diagram(cx))),
            )
            .child(div().flex_1())
            .when_some(match_count, |bar, count| {
                bar.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(if count == 0 {
                            Theme::warning()
                        } else {
                            Theme::text_dim()
                        })
                        .child(crate::localization::format(
                            "query.execution_plan.search_matches",
                            &[("count", &count.to_string())],
                        )),
                )
            })
            .when_some(self.search_input.clone(), |bar, input| {
                bar.child(
                    div()
                        .w(px(s(180.0)))
                        .child(Input::new(&input).small().cleanable(true)),
                )
            })
    }

    /// What the colours, bars and line widths mean, in words.
    fn render_diagram_legend(&self, scene: &Scene) -> impl IntoElement {
        let s = Theme::scaled;
        let small = Theme::font_size_ratio(0.82);
        let label = |text: String| {
            div()
                .text_size(px(small))
                .text_color(Theme::text_dim())
                .child(text)
        };
        div()
            .absolute()
            .left(px(s(10.0)))
            .bottom(px(s(10.0)))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .px(px(s(8.0)))
            .py(px(s(4.0)))
            .rounded(px(s(4.0)))
            .border_1()
            .border_color(Theme::popup_border())
            .bg(Theme::popup_bg().opacity(0.94))
            .children(
                scene
                    .diagram
                    .metric
                    .map(|metric| label(crate::localization::message(metric.label()))),
            )
            .child(label(crate::localization::text(
                "query.execution_plan.legend_low",
            )))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(s(1.0)))
                    // Steps grow as well as warm, so the scale reads
                    // without colour.
                    .children((0..5).map(|step| {
                        let heat = step as f32 / 4.0;
                        div()
                            .w(px(s(9.0)))
                            .h(px(s(4.0 + 2.0 * step as f32)))
                            .bg(heat_color(heat))
                    })),
            )
            .child(label(crate::localization::text(
                "query.execution_plan.legend_high",
            )))
            .child(div().w(px(s(6.0))))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(s(2.0)))
                    .child(div().w(px(s(16.0))).h(px(s(1.0))).bg(Theme::border_hover()))
                    .child(div().w(px(s(16.0))).h(px(s(4.0))).bg(Theme::border_hover())),
            )
            .child(label(crate::localization::text(
                "query.execution_plan.legend_rows",
            )))
            .child(div().w(px(s(6.0))))
            .child(div().w(px(s(16.0))).h(px(s(4.0))).bg(mix(
                Theme::border_hover(),
                Theme::text(),
                0.55,
            )))
            .child(label(crate::localization::text(
                "query.execution_plan.legend_heaviest_path",
            )))
    }

    pub(super) fn render_diagram(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.ensure_search_input(window, cx);
        let Some(active) = self.statements.get(self.active_statement) else {
            return div().into_any_element();
        };
        let scene = self.diagram.ensure_scene(active);
        if self
            .diagram
            .matches
            .as_ref()
            .is_none_or(|matches| matches.query != self.diagram.search)
        {
            self.diagram.matches = Some(Rc::new(search_plan(active, &self.diagram.search)));
        }
        let wide = self
            .body_bounds
            .get()
            .is_none_or(|bounds| is_wide(bounds.size.width));

        let entity = cx.entity();
        let paint_scene = scene.clone();
        let selected = self.selected_node;
        let finding_nodes = Rc::new(self.finding_nodes());
        let matches = self.diagram.matches.clone();

        let canvas_area = div()
            .id("plan-diagram-canvas")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .bg(Theme::panel_bg())
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_diagram_mouse_down))
            .on_mouse_move(cx.listener(Self::on_diagram_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_diagram_mouse_up))
            .on_scroll(cx.listener(Self::on_diagram_scroll))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered && this.diagram.hovered.take().is_some() {
                    cx.notify();
                }
            }))
            .on_pinch(cx.listener(Self::on_diagram_pinch))
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let state = &entity.read(cx).diagram;
                        state.bounds.set(Some(bounds));
                        let zoom_before = state.zoom();
                        state.settle(&paint_scene, bounds);
                        let zoom_changed = (state.zoom() - zoom_before).abs() > f32::EPSILON;
                        let frame = Frame {
                            minimap: state.minimap(&paint_scene, bounds),
                            scale: state.scale(),
                            pan: state.pan.get(),
                            scene: paint_scene,
                            selected,
                            finding_nodes,
                            matches,
                        };
                        // The toolbar's zoom figure was drawn before the
                        // fit ran; draw it again with the settled zoom.
                        // A notify during this draw is not acted on, so it
                        // waits for the next frame.
                        if zoom_changed {
                            let id = entity.entity_id();
                            window.on_next_frame(move |_, cx| cx.notify(id));
                        }
                        frame
                    },
                    move |bounds, frame, window, cx| paint_diagram(&frame, bounds, window, cx),
                )
                .size_full(),
            )
            .child(self.render_diagram_legend(&scene))
            .children(self.render_hover_label(&scene));

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(self.render_diagram_toolbar(&self.statements[self.active_statement], &scene, cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(canvas_area)
                    .when(wide, |row| {
                        row.child(self.render_details(&self.statements[self.active_statement], cx))
                    }),
            )
            .into_any_element()
    }

    /// True when the hovered card had a line shortened to fit.
    fn hovered_card_is_clipped(&self) -> bool {
        let (Some((node_id, _)), Some(scene)) = (self.diagram.hovered, &self.diagram.scene) else {
            return false;
        };
        let Some(index) = scene
            .diagram
            .nodes
            .iter()
            .position(|node| node.node_id == node_id)
        else {
            return false;
        };
        let fitted = scene.fitted.borrow();
        [0u8, 1u8]
            .iter()
            .any(|line| fitted.get(&(index, *line)).is_some_and(|f| f.clipped))
    }

    /// The full title and subtitle of a hovered card whose text was
    /// shortened, next to the pointer.
    fn render_hover_label(&self, scene: &Scene) -> Option<AnyElement> {
        if !self.hovered_card_is_clipped() {
            return None;
        }
        let (node_id, at) = self.diagram.hovered?;
        let index = scene
            .diagram
            .nodes
            .iter()
            .position(|node| node.node_id == node_id)?;
        let card = scene.cards.get(index)?;
        let s = Theme::scaled;
        Some(
            div()
                .absolute()
                .left(at.x + px(14.0))
                .top(at.y + px(18.0))
                .max_w(px(s(360.0)))
                .px(px(s(8.0)))
                .py(px(s(5.0)))
                .rounded(px(s(4.0)))
                .border_1()
                .border_color(Theme::border())
                .bg(Theme::bg())
                .shadow_md()
                .flex()
                .flex_col()
                .gap(px(s(2.0)))
                .child(
                    div()
                        .whitespace_normal()
                        .text_size(px(Theme::font_size()))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(Theme::text())
                        .child(card.title.clone()),
                )
                .children(card.subtitle.clone().map(|subtitle| {
                    div()
                        .whitespace_normal()
                        .text_size(px(Theme::font_size_ratio(0.9)))
                        .text_color(Theme::text_dim())
                        .child(subtitle)
                }))
                .into_any_element(),
        )
    }

    /// Window position of the middle of an operator's card, for tests.
    #[cfg(test)]
    pub(super) fn diagram_node_center(&self, node_id: i32) -> Option<Point<Pixels>> {
        let bounds = self.diagram.bounds.get()?;
        let node = self.diagram.scene.as_ref()?.diagram.node(node_id)?;
        let (x, y) = self
            .diagram
            .to_canvas(node.x + node.width / 2.0, node.y + node.height / 2.0);
        Some(point(bounds.origin.x + px(x), bounds.origin.y + px(y)))
    }

    /// Window position of an operator's fold toggle, for tests.
    #[cfg(test)]
    pub(super) fn diagram_fold_center(&self, node_id: i32) -> Option<Point<Pixels>> {
        let bounds = self.diagram.bounds.get()?;
        let scene = self.diagram.scene.as_ref()?;
        let node = scene.diagram.node(node_id)?;
        let (fx, fy) = fold_origin(node, scene.diagram.orientation);
        let (x, y) = self
            .diagram
            .to_canvas(fx + FOLD_SIZE / 2.0, fy + FOLD_SIZE / 2.0);
        Some(point(bounds.origin.x + px(x), bounds.origin.y + px(y)))
    }
}
