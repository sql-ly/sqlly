//! The map view: an OpenStreetMap basemap (when allowed) under points,
//! lines, polygons, or a heatmap, with pan, zoom, fit, hover, click,
//! clustering, and the options panel (see `panel.rs`) that edits the same
//! settings the `-- sqlly map:` directive carries.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, Bounds, ContentMask, Context, Corners, Hsla, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Render, RenderImage, ScrollEvent, SharedString, Window, canvas, div,
    fill, hsla, point, prelude::*, px, size,
};
use sqlly_core::spatial::{Coord, Shape};
use sqlly_core::{CellValue, ResultColumn};

use super::directive::{HeatPalette, MapDirective, MapMode};
use super::features::{ColorKey, MAX_CATEGORIES, MapData, MapSource, resolve, shape_anchor};
use super::heatmap::{self, HeatGrid};
use super::projection::{
    Projection, TILE_SIZE, TilePlacement, Viewport, lonlat_to_world, unwrap_path, world_to_lonlat,
    wrapped_x_extent,
};
use super::tiles::{FetchError, TileStore};
use crate::localization::{format as tr_format, text as tr};
use crate::preferences::{MapPreferences, MapTileChoice};
use crate::theme::Theme;

/// Called with a result row index when the user clicks a feature.
pub type RowSelectHook = Rc<dyn Fn(usize, &mut App)>;

/// What a map window plots.
pub enum MapContent {
    /// One geometry value (the value viewer's Map mode).
    Value(MapData),
    /// A whole result; `sources` lists every plottable reading of it.
    Rows {
        columns: Vec<ResultColumn>,
        rows: Arc<Vec<Vec<CellValue>>>,
        sources: Vec<MapSource>,
    },
}

/// A feature in zoom-0 world coordinates, paths already unwrapped across
/// the antimeridian.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WorldShape {
    Point(f64, f64),
    Line(Vec<(f64, f64)>),
    Polygon(Vec<Vec<(f64, f64)>>),
}

fn to_world(shape: &Shape, projection: &Projection) -> WorldShape {
    let path = |pts: &[sqlly_core::spatial::Coord]| {
        let mut out: Vec<(f64, f64)> = pts.iter().map(|c| projection.project(*c)).collect();
        if projection.wraps() {
            unwrap_path(&mut out);
        }
        out
    };
    match shape {
        Shape::Point(c) => {
            let (x, y) = projection.project(*c);
            WorldShape::Point(x, y)
        }
        Shape::LineString(pts) => WorldShape::Line(path(pts)),
        Shape::Polygon(rings) => WorldShape::Polygon(rings.iter().map(|r| path(r)).collect()),
    }
}

/// Screen-space hit radius for points, and stroke tolerance for lines.
const POINT_HIT_PX: f64 = 10.0;
const LINE_HIT_PX: f64 = 5.0;
const POINT_PX: f32 = 12.0;
const FIT_PADDING_PX: f64 = 32.0;
/// Zoom used when the data is a single point.
const POINT_ZOOM: f64 = 13.0;

fn seg_distance(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

fn point_in_ring(p: (f64, f64), ring: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let n = ring.len();
    if n < 3 {
        return false;
    }
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        if (yi > p.1) != (yj > p.1) && p.0 < (xj - xi) * (p.1 - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// The feature under a screen point: the nearest point marker within
/// reach, else the topmost line within reach, else the topmost polygon
/// containing it.
pub(crate) fn hit_test(
    shapes: &[WorldShape],
    vp: &Viewport,
    wraps: bool,
    at: (f64, f64),
) -> Option<usize> {
    let screen = |(x, y): (f64, f64), shift: f64| vp.world_to_screen(x + shift, y);
    // Shift a shape by whole worlds so its first vertex is nearest the view.
    let shift_for = |x: f64| if wraps { vp.nearest_copy(x) - x } else { 0.0 };
    let mut best: Option<(usize, f64)> = None;
    for (i, s) in shapes.iter().enumerate() {
        if let WorldShape::Point(x, y) = s {
            let (sx, sy) = screen((*x, *y), shift_for(*x));
            let d = ((sx - at.0).powi(2) + (sy - at.1).powi(2)).sqrt();
            if d <= POINT_HIT_PX && best.is_none_or(|(_, bd)| d < bd) {
                best = Some((i, d));
            }
        }
    }
    if let Some((i, _)) = best {
        return Some(i);
    }
    for (i, s) in shapes.iter().enumerate().rev() {
        if let WorldShape::Line(pts) = s {
            let Some(first) = pts.first() else { continue };
            let shift = shift_for(first.0);
            if pts
                .windows(2)
                .any(|w| seg_distance(at, screen(w[0], shift), screen(w[1], shift)) <= LINE_HIT_PX)
            {
                return Some(i);
            }
        }
    }
    for (i, s) in shapes.iter().enumerate().rev() {
        if let WorldShape::Polygon(rings) = s {
            let Some(first) = rings.first().and_then(|r| r.first()) else {
                continue;
            };
            let shift = shift_for(first.0);
            let screen_rings: Vec<Vec<(f64, f64)>> = rings
                .iter()
                .map(|r| r.iter().map(|p| screen(*p, shift)).collect())
                .collect();
            let in_outer = screen_rings.first().is_some_and(|r| point_in_ring(at, r));
            let in_hole = screen_rings.iter().skip(1).any(|r| point_in_ring(at, r));
            if in_outer && !in_hole {
                return Some(i);
            }
        }
    }
    None
}

/// The world rectangle to fit: every vertex, with a repeating world's x
/// span taken the short way round the antimeridian.
pub(crate) fn world_extent(shapes: &[WorldShape], wraps: bool) -> Option<((f64, f64), (f64, f64))> {
    let mut xs = Vec::new();
    let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);
    let mut push = |(x, y): (f64, f64)| {
        if x.is_finite() && y.is_finite() {
            xs.push(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
    };
    for s in shapes {
        match s {
            WorldShape::Point(x, y) => push((*x, *y)),
            WorldShape::Line(pts) => pts.iter().copied().for_each(&mut push),
            WorldShape::Polygon(rings) => rings.iter().flatten().copied().for_each(&mut push),
        }
    }
    if xs.is_empty() {
        return None;
    }
    let (min_x, max_x) = if wraps {
        wrapped_x_extent(&xs)?
    } else {
        let min = xs.iter().copied().fold(f64::INFINITY, f64::min);
        let max = xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        (min, max)
    };
    Some(((min_x, min_y), (max_x, max_y)))
}

type TileImage = Arc<RenderImage>;

fn decode_tile(bytes: &[u8]) -> Result<TileImage, FetchError> {
    let dynamic = image::load_from_memory(bytes).map_err(|_| FetchError::BadImage)?;
    // GPUI samples RenderImage buffers as BGRA.
    let mut rgba = dynamic.to_rgba8();
    for px in rgba.pixels_mut() {
        px.0.swap(0, 2);
    }
    Ok(Arc::new(RenderImage::new([image::Frame::new(rgba)])))
}

/// A drawable tile: its placement and the image (or the scaled ancestor
/// sub-rectangle `(fx, fy, frac)`) standing in for it.
struct PaintTile {
    place: TilePlacement,
    image: TileImage,
    sub: Option<(f64, f64, f64)>,
}

/// Heat-cell styling for one frame.
#[derive(Clone, Copy)]
struct HeatStyle {
    palette: HeatPalette,
    intensity: f32,
    opacity: f32,
    accent: (f32, f32, f32),
}

/// A multi-member marker cluster on screen.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScreenCluster {
    pub x: f64,
    pub y: f64,
    pub members: Vec<usize>,
}

impl ScreenCluster {
    fn radius(&self) -> f64 {
        10.0 + 4.0 * (self.members.len() as f64).log2()
    }
}

/// Everything one frame paints, captured in prepaint.
struct Frame {
    vp: Viewport,
    shapes: Arc<Vec<WorldShape>>,
    data: Arc<MapData>,
    wraps: bool,
    tiles: Vec<PaintTile>,
    graticule: bool,
    heat: Option<(HeatGrid, HeatStyle)>,
    points: bool,
    clusters: Vec<ScreenCluster>,
    clustered: Arc<Vec<bool>>,
    hover: Option<Hit>,
    selected: Option<usize>,
    evicted: Vec<TileImage>,
}

/// What the pointer is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hit {
    Feature(usize),
    Cluster(usize),
}

/// Called with the map settings when the user writes them into the query.
pub type WriteHook = Rc<dyn Fn(MapDirective, &mut App)>;

/// What the map can do to the grid and query it came from.
#[derive(Clone, Default)]
pub struct MapHooks {
    pub select: Option<RowSelectHook>,
    pub write: Option<WriteHook>,
}

/// Default marker fill: a saturated blue that reads on OSM tiles and the sketch.
const DEFAULT_MARKER: Hsla = Hsla {
    h: 0.6,
    s: 0.85,
    l: 0.45,
    a: 1.0,
};

/// Marker color for a feature.
pub(crate) fn feature_color(color: ColorKey) -> Hsla {
    match color {
        // Fixed rather than the theme accent: markers sit on light street-map
        // tiles as often as on the dark sketch, and a pale accent vanishes there.
        ColorKey::Default => DEFAULT_MARKER,
        ColorKey::Category(i) if i >= MAX_CATEGORIES => hsla(0.0, 0.0, 0.55, 1.0),
        ColorKey::Category(i) => hsla((i as f32 * 0.618_034 + 0.58).fract(), 0.65, 0.5, 1.0),
        ColorKey::Scale(t) => {
            let (h, s, l, _) = heatmap::shade(HeatPalette::Viridis, t.max(0.03), 1.0, 1.0, None)
                .unwrap_or((0.7, 0.6, 0.3, 1.0));
            hsla(h, s, l, 1.0)
        }
    }
}

/// Marker diameter in pixels for a `size=` share (`None`: the default).
pub(crate) fn marker_diameter(size: Option<f32>) -> f64 {
    match size {
        Some(t) => 8.0 + 20.0 * f64::from(t.clamp(0.0, 1.0)),
        None => f64::from(POINT_PX),
    }
}

pub struct MapView {
    pub(super) title: String,
    pub(super) content: MapContent,
    /// The auto-detected source in use when the settings name none.
    pub(super) source_index: usize,
    /// The live settings: what the options panel shows and writes.
    pub(super) options: MapDirective,
    pub(super) tooltip_columns: Vec<usize>,
    /// Columns the settings name that the result lacks.
    pub(super) missing: Vec<String>,
    pub(super) data: Arc<MapData>,
    pub(super) source: Option<MapSource>,
    projection: Projection,
    shapes: Arc<Vec<WorldShape>>,
    pub(super) viewport: Viewport,
    needs_fit: bool,
    canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    drag: Option<(gpui::Point<Pixels>, bool)>,
    hover: Option<Hit>,
    hover_pos: (f64, f64),
    selected: Option<usize>,
    clusters: Vec<ScreenCluster>,
    pub(super) tiles: TileStore<TileImage>,
    pub(super) hooks: MapHooks,
    pub(super) show_panel: bool,
}

impl MapView {
    pub fn new(
        title: impl Into<String>,
        content: MapContent,
        directive: Option<MapDirective>,
        hooks: MapHooks,
    ) -> Self {
        let prefs = MapPreferences::load();
        #[cfg(not(target_arch = "wasm32"))]
        let tiles = TileStore::new(prefs.tiles, super::tiles::osm_fetcher());
        // The browser build never fetches tiles: the sketch is the map.
        #[cfg(target_arch = "wasm32")]
        let tiles = {
            let _ = prefs;
            TileStore::new(MapTileChoice::Declined, super::tiles::no_fetcher())
        };
        Self::with_tiles(title, content, directive, hooks, tiles)
    }

    /// [`Self::new`] with an explicit tile store (tests inject a fetcher).
    pub(crate) fn with_tiles(
        title: impl Into<String>,
        content: MapContent,
        directive: Option<MapDirective>,
        hooks: MapHooks,
        tiles: TileStore<TileImage>,
    ) -> Self {
        let mut view = Self {
            title: title.into(),
            content,
            source_index: 0,
            options: directive.unwrap_or_default(),
            tooltip_columns: Vec::new(),
            missing: Vec::new(),
            data: Arc::new(MapData::default()),
            source: None,
            projection: Projection::Mercator,
            shapes: Arc::new(Vec::new()),
            viewport: Viewport::new(640.0, 480.0),
            needs_fit: true,
            canvas_bounds: Rc::new(Cell::new(None)),
            drag: None,
            hover: None,
            hover_pos: (0.0, 0.0),
            selected: None,
            clusters: Vec::new(),
            tiles,
            hooks,
            show_panel: false,
        };
        view.rebuild();
        // Several plausible location columns and no choice in the
        // settings: open the panel so the Locations picker is in view.
        view.show_panel =
            view.sources().len() > 1 && view.options.lat.is_none() && view.options.geom.is_none();
        view
    }

    /// Swap in a fresh run of the same result (an auto-opened map that is
    /// already showing), keeping the window and its view.
    pub fn replace_content(&mut self, content: MapContent, directive: Option<MapDirective>) {
        self.content = content;
        if let Some(d) = directive {
            self.options = d;
        }
        self.rebuild();
    }

    /// Apply new settings (the options panel, or a test) and redraw.
    pub(crate) fn apply_options(&mut self, options: MapDirective) {
        let refit = options.fit != self.options.fit
            || options.center != self.options.center
            || options.zoom != self.options.zoom;
        self.options = options;
        self.rebuild();
        if refit {
            self.needs_fit = true;
        }
    }

    /// Re-read the data through the current settings and re-project it.
    /// Re-fits only when the location source changed.
    fn rebuild(&mut self) {
        let before = self.source;
        let (data, source) = match &self.content {
            MapContent::Value(data) => (data.clone(), None),
            MapContent::Rows {
                columns,
                rows,
                sources,
            } => {
                let resolved = resolve(columns, &self.options);
                self.tooltip_columns = resolved.tooltip.clone();
                self.missing = resolved.missing.clone();
                let source = resolved
                    .source
                    .or_else(|| sources.get(self.source_index).copied());
                let data = source
                    .map(|s| MapData::from_rows_with(columns, rows, s, &resolved.options))
                    .unwrap_or_default();
                (data, source)
            }
        };
        self.source = source;
        self.projection = if data.geographic {
            Projection::Mercator
        } else {
            Projection::planar_for(data.bbox())
        };
        self.shapes = Arc::new(
            data.features
                .iter()
                .map(|f| to_world(&f.shape, &self.projection))
                .collect(),
        );
        self.data = Arc::new(data);
        self.hover = None;
        self.selected = None;
        if before != self.source || before.is_none() {
            self.needs_fit = true;
        }
    }

    pub(super) fn sources(&self) -> &[MapSource] {
        match &self.content {
            MapContent::Rows { sources, .. } => sources,
            MapContent::Value(_) => &[],
        }
    }

    pub(super) fn columns(&self) -> &[ResultColumn] {
        match &self.content {
            MapContent::Rows { columns, .. } => columns,
            MapContent::Value(_) => &[],
        }
    }

    /// The view center as `(latitude, longitude)` (or `(y, x)` for a
    /// planar sketch), rounded for a directive line.
    pub(super) fn current_center(&self) -> (f64, f64) {
        let c = self.projection.unproject(self.viewport.center);
        let r = |v: f64| (v * 10_000.0).round() / 10_000.0;
        (r(c.y), r(c.x))
    }

    pub(super) fn fit(&mut self) {
        if let Some((min, max)) = world_extent(&self.shapes, self.projection.wraps()) {
            self.viewport
                .fit(min, max, FIT_PADDING_PX, POINT_ZOOM, &self.projection);
        }
    }

    /// The starting view: the settings' center/zoom, else fit to the data.
    fn initial_view(&mut self) {
        self.fit();
        if self.options.fit {
            return;
        }
        if let Some((lat, lon)) = self.options.center {
            self.viewport.center = self.projection.project(Coord { x: lon, y: lat });
        }
        if let Some(zoom) = self.options.zoom {
            let c = (self.viewport.width / 2.0, self.viewport.height / 2.0);
            self.viewport.zoom_around(zoom, c, &self.projection);
        }
    }

    fn sync_size(&mut self, bounds: Bounds<Pixels>) {
        let (w, h) = (
            f64::from(f32::from(bounds.size.width)),
            f64::from(f32::from(bounds.size.height)),
        );
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        self.viewport.width = w;
        self.viewport.height = h;
        if self.needs_fit {
            self.needs_fit = false;
            self.initial_view();
        }
    }

    fn local(&self, position: gpui::Point<Pixels>) -> Option<(f64, f64)> {
        let b = self.canvas_bounds.get()?;
        Some((
            f64::from(f32::from(position.x - b.origin.x)),
            f64::from(f32::from(position.y - b.origin.y)),
        ))
    }

    /// Whether this map wants a street map at all: geographic data, and
    /// not `basemap=none`. Consent is checked separately.
    pub(super) fn tiles_wanted(&self) -> bool {
        self.data.geographic && self.options.basemap != Some(false)
    }

    fn show_tiles(&self) -> bool {
        self.tiles_wanted() && self.tiles.active()
    }

    /// Start downloads for the visible tiles this map needs (none unless
    /// tiles are wanted and allowed).
    pub(crate) fn request_visible_tiles(
        &mut self,
    ) -> Vec<(super::projection::TileKey, super::tiles::FetchFuture)> {
        if !self.tiles_wanted() {
            return Vec::new();
        }
        let keys: Vec<_> = self
            .viewport
            .visible_tiles()
            .iter()
            .map(|p| p.key)
            .collect();
        self.tiles.start_fetches(&keys)
    }

    pub(super) fn set_choice(&mut self, choice: MapTileChoice, cx: &mut Context<Self>) {
        let prefs = MapPreferences { tiles: choice };
        prefs.save();
        self.tiles.set_choice(choice);
        cx.notify();
    }

    fn zoom_by(&mut self, delta: f64, anchor: Option<(f64, f64)>, cx: &mut Context<Self>) {
        let anchor = anchor.unwrap_or((self.viewport.width / 2.0, self.viewport.height / 2.0));
        let zoom = self.viewport.zoom + delta;
        self.viewport.zoom_around(zoom, anchor, &self.projection);
        cx.notify();
    }

    fn heat_style(&self) -> HeatStyle {
        let accent: gpui::Hsla = Theme::accent();
        HeatStyle {
            palette: self.options.palette.unwrap_or_default(),
            intensity: self.options.intensity.unwrap_or(1.0) as f32,
            opacity: self.options.opacity.unwrap_or(0.8) as f32,
            accent: (accent.h, accent.s, accent.l),
        }
    }

    /// Screen positions of the point features (`None` for other shapes).
    fn point_screens(&self, vp: &Viewport) -> Vec<Option<(f64, f64)>> {
        let wraps = self.projection.wraps();
        self.shapes
            .iter()
            .map(|s| match s {
                WorldShape::Point(x, y) => {
                    let x = if wraps { vp.nearest_copy(*x) } else { *x };
                    Some(vp.world_to_screen(x, *y))
                }
                _ => None,
            })
            .collect()
    }

    /// Build this frame's paint list and kick off any tile downloads it
    /// needs. Runs in the canvas prepaint, once the size is known.
    fn prepare_frame(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) -> Frame {
        self.sync_size(bounds);
        let vp = self.viewport;
        let mut tiles = Vec::new();
        let show_tiles = self.show_tiles();
        if show_tiles {
            let started = self.request_visible_tiles();
            self.spawn_tile_fetches(started, cx);
            for place in vp.visible_tiles() {
                if let Some(image) = self.tiles.get(&place.key) {
                    tiles.push(PaintTile {
                        place,
                        image: image.clone(),
                        sub: None,
                    });
                } else if let Some((image, fx, fy, frac)) = self.tiles.ancestor(place.key) {
                    tiles.push(PaintTile {
                        place,
                        image: image.clone(),
                        sub: Some((fx, fy, frac)),
                    });
                }
            }
        }
        let mode = self.options.mode();
        let heat = mode.shows_heat().then(|| {
            let wraps = self.projection.wraps();
            let pts = self.data.features.iter().filter_map(|f| {
                let c = shape_anchor(&f.shape)?;
                let (x, y) = self.projection.project(c);
                let x = if wraps { vp.nearest_copy(x) } else { x };
                let (sx, sy) = vp.world_to_screen(x, y);
                Some((sx, sy, f.weight))
            });
            let grid = heatmap::density_weighted(
                pts,
                vp.width,
                vp.height,
                self.options.radius.unwrap_or(heatmap::DEFAULT_RADIUS_PX),
                self.options.blur.unwrap_or(0.0),
            );
            (grid, self.heat_style())
        });
        let mut clustered = vec![false; self.shapes.len()];
        self.clusters.clear();
        if mode.shows_points() && self.options.cluster == Some(true) {
            let screens = self.point_screens(&vp);
            let (idx, pts): (Vec<usize>, Vec<(f64, f64)>) = screens
                .iter()
                .enumerate()
                .filter_map(|(i, p)| p.map(|p| (i, p)))
                .unzip();
            for c in heatmap::cluster_points(&pts, CLUSTER_CELL_PX) {
                if c.members.len() > 1 {
                    let members: Vec<usize> = c.members.iter().map(|&m| idx[m]).collect();
                    for &m in &members {
                        clustered[m] = true;
                    }
                    self.clusters.push(ScreenCluster {
                        x: c.x,
                        y: c.y,
                        members,
                    });
                }
            }
        }
        Frame {
            vp,
            shapes: self.shapes.clone(),
            data: self.data.clone(),
            wraps: self.projection.wraps(),
            tiles,
            graticule: !show_tiles && self.data.geographic,
            heat,
            points: mode.shows_points(),
            clusters: self.clusters.clone(),
            clustered: Arc::new(clustered),
            hover: self.hover,
            selected: self.selected,
            evicted: self.tiles.take_evicted(),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn spawn_tile_fetches(
        &mut self,
        started: Vec<(super::projection::TileKey, super::tiles::FetchFuture)>,
        cx: &mut Context<Self>,
    ) {
        for (key, fut) in started {
            let handle = crate::tokio_handle().spawn(fut);
            cx.spawn(async move |this, cx| {
                let fetched = handle.await.unwrap_or(Err(FetchError::Offline));
                let decoded = match fetched {
                    Ok(bytes) => {
                        cx.background_executor()
                            .spawn(async move { decode_tile(&bytes) })
                            .await
                    }
                    Err(e) => Err(e),
                };
                let _ = this.update(cx, |this, cx| {
                    this.tiles.finish(key, decoded);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn spawn_tile_fetches(
        &mut self,
        _started: Vec<(super::projection::TileKey, super::tiles::FetchFuture)>,
        _cx: &mut Context<Self>,
    ) {
    }

    /// What is under a canvas point: a cluster first, then a feature.
    fn hit(&self, at: (f64, f64)) -> Option<Hit> {
        if let Some(i) = self
            .clusters
            .iter()
            .position(|c| ((c.x - at.0).powi(2) + (c.y - at.1).powi(2)).sqrt() <= c.radius())
        {
            return Some(Hit::Cluster(i));
        }
        hit_test(&self.shapes, &self.viewport, self.projection.wraps(), at).map(Hit::Feature)
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _w: &mut Window, _cx: &mut Context<Self>) {
        self.drag = Some((event.position, false));
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _w: &mut Window, cx: &mut Context<Self>) {
        if let Some((last, moved)) = self.drag
            && event.dragging()
        {
            let dx = f64::from(f32::from(event.position.x - last.x));
            let dy = f64::from(f32::from(event.position.y - last.y));
            let moved = moved || dx.abs() + dy.abs() > 2.0;
            if moved {
                self.viewport.pan_by(dx, dy, &self.projection);
                self.drag = Some((event.position, true));
                self.hover = None;
                cx.notify();
            }
            return;
        }
        let Some(at) = self.local(event.position) else {
            return;
        };
        let hover = self.hit(at);
        if hover != self.hover || (hover.is_some() && at != self.hover_pos) {
            self.hover = hover;
            self.hover_pos = at;
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let Some((_, moved)) = self.drag.take() else {
            return;
        };
        if moved {
            return;
        }
        let Some(at) = self.local(event.position) else {
            return;
        };
        match self.hit(at) {
            Some(Hit::Cluster(i)) => {
                // Zoom into a cluster until it splits.
                let c = &self.clusters[i];
                let anchor = (c.x, c.y);
                self.selected = None;
                self.zoom_by(2.0, Some(anchor), cx);
            }
            hit => {
                self.selected = match hit {
                    Some(Hit::Feature(i)) => Some(i),
                    _ => None,
                };
                if self.options.click != Some(false)
                    && let Some(row) = self
                        .selected
                        .and_then(|i| self.data.features.get(i))
                        .and_then(|f| f.row)
                    && let Some(hook) = self.hooks.select.clone()
                {
                    hook(row, cx);
                }
            }
        }
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(40.0));
        let dy = f64::from(f32::from(delta.y));
        if dy == 0.0 {
            return;
        }
        let anchor = self.local(event.position);
        self.zoom_by(dy / 160.0, anchor, cx);
    }

    fn on_pinch(&mut self, event: &gpui::PinchEvent, _w: &mut Window, cx: &mut Context<Self>) {
        let factor = 1.0 + f64::from(event.delta);
        if factor <= 0.0 {
            return;
        }
        let anchor = self.local(event.position);
        self.zoom_by(factor.log2(), anchor, cx);
    }

    fn status_text(&self) -> String {
        let shown = self.data.features.len();
        let mut parts = vec![if self.data.is_capped() {
            tr_format(
                "map_view.status.capped",
                &[
                    ("shown", shown.to_string()),
                    ("total", self.data.total_features.to_string()),
                ],
            )
        } else {
            tr_format("map_view.status.features", &[("count", shown.to_string())])
        }];
        if self.data.skipped_rows > 0 {
            parts.push(tr_format(
                "map_view.status.skipped",
                &[("count", self.data.skipped_rows.to_string())],
            ));
        }
        parts.join(" · ")
    }

    fn note_row(text: String) -> gpui::Div {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .px(px(s(12.0)))
            .py(px(s(5.0)))
            .text_size(px(Theme::font_size_ratio(0.87)))
            .text_color(Theme::text_dim())
            .border_b_1()
            .border_color(Theme::border())
            .child(text)
    }

    /// The notes under the toolbar: missing columns named by the settings,
    /// and why there is no street map.
    fn notes(&self, cx: &mut Context<Self>) -> Vec<gpui::AnyElement> {
        let mut out = Vec::new();
        if !self.missing.is_empty() {
            out.push(
                Self::note_row(tr_format(
                    "map_view.note.missing_columns",
                    &[("columns", self.missing.join(", "))],
                ))
                .into_any_element(),
            );
        }
        if let Some(note) = self.basemap_note(cx) {
            out.push(note);
        }
        out
    }

    fn basemap_note(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.data.geographic {
            let text = match self.data.foreign_srid {
                Some(srid) => tr_format("map_view.note.foreign_srid", &[("srid", srid)]),
                None => tr("map_view.note.not_lonlat"),
            };
            return Some(Self::note_row(text).into_any_element());
        }
        if self.options.basemap == Some(false) {
            return Some(Self::note_row(tr("map_view.note.basemap_none")).into_any_element());
        }
        if cfg!(target_arch = "wasm32") {
            return Some(Self::note_row(tr("map_view.note.browser")).into_any_element());
        }
        if self.tiles.is_offline() {
            return Some(
                Self::note_row(tr("map_view.note.offline"))
                    .child(
                        crate::theme::accent_pill_button("map-retry", tr("map_view.note.retry"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.tiles.retry();
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        if self.tiles.choice() == MapTileChoice::Declined {
            return Some(
                Self::note_row(tr("map_view.note.tiles_off"))
                    .child(
                        crate::theme::accent_pill_button(
                            "map-tiles-on",
                            tr("map_view.note.turn_on"),
                        )
                        .on_click(
                            cx.listener(|this, _, _, cx| {
                                this.set_choice(MapTileChoice::Allowed, cx)
                            }),
                        ),
                    )
                    .into_any_element(),
            );
        }
        None
    }

    pub(super) fn segment(
        &self,
        id: impl Into<gpui::ElementId>,
        label: String,
        active: bool,
        cx: &mut Context<Self>,
        on: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> gpui::Stateful<gpui::Div> {
        let s = Theme::scaled;
        div()
            .id(id)
            .cursor_pointer()
            .px(px(s(10.0)))
            .py(px(s(3.0)))
            .rounded(px(s(4.0)))
            .text_size(px(Theme::font_size_ratio(0.92)))
            .when(active, |d| {
                d.bg(Theme::accent().opacity(0.2))
                    .text_color(Theme::accent())
            })
            .when(!active, |d| {
                d.text_color(Theme::text_dim())
                    .hover(|st| st.bg(Theme::row_hover()))
            })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                on(this, cx);
                cx.notify();
            }))
    }

    pub(super) fn group() -> gpui::Div {
        let s = Theme::scaled;
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(2.0)))
            .p(px(s(2.0)))
            .rounded(px(s(5.0)))
            .border_1()
            .border_color(Theme::border())
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> gpui::Div {
        let s = Theme::scaled;
        let mode = self.options.mode();
        let left = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(10.0)))
            .child(
                Self::group()
                    .child(self.segment(
                        "map-mode-points",
                        tr("map_view.toolbar.points"),
                        mode == MapMode::Points,
                        cx,
                        |this, _| {
                            let mut d = this.options.clone();
                            d.mode = Some(MapMode::Points);
                            this.apply_options(d);
                        },
                    ))
                    .child(self.segment(
                        "map-mode-heat",
                        tr("map_view.toolbar.heatmap"),
                        mode == MapMode::Heatmap,
                        cx,
                        |this, _| {
                            let mut d = this.options.clone();
                            d.mode = Some(MapMode::Heatmap);
                            this.apply_options(d);
                        },
                    ))
                    .child(self.segment(
                        "map-mode-both",
                        tr("map_view.toolbar.both"),
                        mode == MapMode::Both,
                        cx,
                        |this, _| {
                            let mut d = this.options.clone();
                            d.mode = Some(MapMode::Both);
                            this.apply_options(d);
                        },
                    )),
            );
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(10.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(self.status_text()),
            )
            .child(
                Self::group()
                    .child(
                        self.segment("map-zoom-out", "−".into(), false, cx, |this, _| {
                            let z = this.viewport.zoom - 1.0;
                            let c = (this.viewport.width / 2.0, this.viewport.height / 2.0);
                            this.viewport.zoom_around(z, c, &this.projection);
                        }),
                    )
                    .child(
                        self.segment("map-zoom-in", "+".into(), false, cx, |this, _| {
                            let z = this.viewport.zoom + 1.0;
                            let c = (this.viewport.width / 2.0, this.viewport.height / 2.0);
                            this.viewport.zoom_around(z, c, &this.projection);
                        }),
                    )
                    .child(self.segment(
                        "map-fit",
                        tr("map_view.toolbar.fit"),
                        false,
                        cx,
                        |this, _| this.fit(),
                    )),
            )
            .child(self.segment(
                "map-options",
                tr("map_view.toolbar.options"),
                self.show_panel,
                cx,
                |this, _| this.show_panel = !this.show_panel,
            ));
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(s(12.0)))
            .w_full()
            .px(px(s(12.0)))
            .py(px(s(6.0)))
            .bg(Theme::panel_bg())
            .border_b_1()
            .border_color(Theme::border())
            .child(left)
            .child(right)
    }

    fn consent_card(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        div()
            .absolute()
            .top(px(s(16.0)))
            .left(px(s(16.0)))
            .right(px(s(16.0)))
            .flex()
            .justify_center()
            .child(
                div()
                    .id("map-consent")
                    .occlude()
                    .max_w(px(s(520.0)))
                    .flex()
                    .flex_col()
                    .gap(px(s(8.0)))
                    .p(px(s(14.0)))
                    .rounded(px(s(8.0)))
                    .bg(Theme::popover_bg())
                    .border_1()
                    .border_color(Theme::border())
                    .shadow_md()
                    .child(
                        div()
                            .text_color(Theme::text())
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(tr("map_view.consent.title")),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .text_color(Theme::text_dim())
                            .child(tr("map_view.consent.body")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(s(8.0)))
                            .justify_end()
                            .child(self.segment(
                                "map-consent-decline",
                                tr("map_view.consent.decline"),
                                false,
                                cx,
                                |this, cx| this.set_choice(MapTileChoice::Declined, cx),
                            ))
                            .child(
                                crate::theme::accent_pill_button(
                                    "map-consent-allow",
                                    tr("map_view.consent.allow"),
                                )
                                .on_click(cx.listener(
                                    |this, _, _, cx| this.set_choice(MapTileChoice::Allowed, cx),
                                )),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Tooltip lines for the hovered feature or cluster: the title, then
    /// `column: value` for each `tooltip=` column.
    pub(crate) fn tooltip_lines(&self, hit: Hit) -> Vec<String> {
        match hit {
            Hit::Cluster(i) => self
                .clusters
                .get(i)
                .map(|c| {
                    vec![tr_format(
                        "map_view.tooltip.cluster",
                        &[("count", c.members.len())],
                    )]
                })
                .unwrap_or_default(),
            Hit::Feature(i) => {
                let Some(feature) = self.data.features.get(i) else {
                    return Vec::new();
                };
                let mut lines = Vec::new();
                match (feature.label.is_empty(), feature.row) {
                    (false, _) => lines.push(feature.label.clone()),
                    (true, Some(row)) => {
                        lines.push(tr_format("map_view.tooltip.row", &[("row", row + 1)]))
                    }
                    (true, None) => {}
                }
                if let (MapContent::Rows { columns, rows, .. }, Some(row)) =
                    (&self.content, feature.row)
                {
                    for &c in &self.tooltip_columns {
                        let value = rows
                            .get(row)
                            .and_then(|r| r.get(c))
                            .map(display_cell)
                            .unwrap_or_default();
                        if let Some(col) = columns.get(c) {
                            lines.push(format!("{}: {value}", col.name));
                        }
                    }
                }
                lines
            }
        }
    }

    fn tooltip(&self) -> Option<gpui::AnyElement> {
        let lines = self.tooltip_lines(self.hover?);
        if lines.is_empty() {
            return None;
        }
        let s = Theme::scaled;
        let (x, y) = self.hover_pos;
        let mut el = div()
            .absolute()
            .left(px(x as f32 + s(12.0)))
            .top(px(y as f32 + s(12.0)))
            .flex()
            .flex_col()
            .px(px(s(8.0)))
            .py(px(s(4.0)))
            .rounded(px(s(4.0)))
            .bg(Theme::popover_bg())
            .border_1()
            .border_color(Theme::border())
            .text_size(px(Theme::font_size_ratio(0.9)))
            .text_color(Theme::text());
        for (i, line) in lines.into_iter().enumerate() {
            el = el.child(
                div()
                    .when(i > 0, |d| d.text_color(Theme::text_dim()))
                    .child(line),
            );
        }
        Some(el.into_any_element())
    }

    fn attribution(&self) -> gpui::AnyElement {
        let s = Theme::scaled;
        div()
            .id("map-attribution")
            .absolute()
            .right(px(0.0))
            .bottom(px(0.0))
            .px(px(s(6.0)))
            .py(px(s(2.0)))
            .bg(hsla(0.0, 0.0, 1.0, 0.8))
            .text_color(hsla(0.0, 0.0, 0.15, 1.0))
            .text_size(px(Theme::font_size_ratio(0.8)))
            .cursor_pointer()
            .child(tr("map_view.attribution"))
            .on_click(|_, _, _| {
                let _ = crate::browser_open::open_in_browser(super::tiles::ATTRIBUTION_URL);
            })
            .into_any_element()
    }
}

/// A cell as tooltip text.
fn display_cell(cell: &CellValue) -> String {
    match cell {
        CellValue::Null => "NULL".into(),
        CellValue::Bool(v) => v.to_string(),
        CellValue::I64(v) => v.to_string(),
        CellValue::F64(v) => v.to_string(),
        CellValue::String(s) => s.chars().take(80).collect(),
        CellValue::Bytes(b) => format!("<{} bytes>", b.len()),
    }
}

/// Grid spacing in degrees for a view `span` degrees wide: the largest
/// "round" step that still draws at least four lines across.
fn graticule_step(span: f64) -> f64 {
    const STEPS: [f64; 10] = [30.0, 10.0, 5.0, 2.0, 1.0, 0.5, 0.2, 0.1, 0.05, 0.01];
    STEPS
        .into_iter()
        .find(|step| span / step >= 4.0)
        .unwrap_or(0.01)
}

/// Markers within this many pixels of each other cluster together.
const CLUSTER_CELL_PX: f64 = 48.0;

fn paint_frame(frame: &Frame, bounds: Bounds<Pixels>, window: &mut Window) {
    window.paint_quad(fill(bounds, Theme::panel_bg()));
    for image in &frame.evicted {
        let _ = window.drop_image(image.clone());
    }
    let ox = f64::from(f32::from(bounds.origin.x));
    let oy = f64::from(f32::from(bounds.origin.y));
    let at = |(x, y): (f64, f64)| point(px((x + ox) as f32), px((y + oy) as f32));
    let rect = |x: f64, y: f64, w: f64, h: f64| Bounds {
        origin: at((x, y)),
        size: size(px(w as f32), px(h as f32)),
    };
    let vp = &frame.vp;

    for tile in &frame.tiles {
        let p = tile.place;
        let tile_rect = rect(p.x, p.y, p.size, p.size);
        let image_rect = match tile.sub {
            None => tile_rect,
            Some((fx, fy, frac)) => {
                let full = p.size / frac;
                rect(p.x - fx * full, p.y - fy * full, full, full)
            }
        };
        let clip = tile_rect.intersect(&bounds);
        let _ = window.paint_image(
            clip,
            image_rect,
            Corners::default(),
            tile.image.clone(),
            0,
            false,
        );
    }

    if frame.graticule {
        // A light latitude/longitude grid so a tile-less map still has
        // reference lines; its spacing follows the zoom (see `graticule_step`).
        let line = Theme::border().opacity(0.6);
        let (wx0, wy0) = vp.screen_to_world(0.0, 0.0);
        let (wx1, wy1) = vp.screen_to_world(vp.width, vp.height);
        let (lon0, lat_top) = world_to_lonlat(wx0, wy0.clamp(0.0, TILE_SIZE));
        let (lon1, lat_bottom) = world_to_lonlat(wx1, wy1.clamp(0.0, TILE_SIZE));
        let step = graticule_step((lon1 - lon0).abs().min(360.0));
        let mut lon = (lon0 / step).floor() * step;
        while lon <= lon1 {
            let (wx, _) = lonlat_to_world(lon, 0.0);
            let (sx, _) = vp.world_to_screen(wx, 0.0);
            if (0.0..=vp.width).contains(&sx) {
                window.paint_quad(fill(rect(sx, 0.0, 1.0, vp.height), line));
            }
            lon += step;
        }
        let mut lat = (lat_bottom.max(-85.0) / step).floor() * step;
        while lat <= lat_top.min(85.0) {
            let (_, wy) = lonlat_to_world(0.0, lat);
            let (_, sy) = vp.world_to_screen(0.0, wy);
            if (0.0..=vp.height).contains(&sy) {
                window.paint_quad(fill(rect(0.0, sy, vp.width, 1.0), line));
            }
            lat += step;
        }
    }

    if let Some((heat, style)) = &frame.heat {
        for r in 0..heat.rows {
            for c in 0..heat.cols {
                let Some((h, s, l, a)) = heatmap::shade(
                    style.palette,
                    heat.normalized(c, r),
                    style.intensity,
                    style.opacity,
                    Some(style.accent),
                ) else {
                    continue;
                };
                window.paint_quad(fill(
                    rect(
                        c as f64 * heat.cell,
                        r as f64 * heat.cell,
                        heat.cell,
                        heat.cell,
                    ),
                    hsla(h, s, l, a),
                ));
            }
        }
    }

    let highlight = Theme::love();
    let shift_for = |x: f64| {
        if frame.wraps {
            vp.nearest_copy(x) - x
        } else {
            0.0
        }
    };
    let screen = |(x, y): (f64, f64), shift: f64| at(vp.world_to_screen(x + shift, y));
    let margin = 64.0;
    let on_screen = |(x, y): (f64, f64)| {
        x >= -margin && y >= -margin && x <= vp.width + margin && y <= vp.height + margin
    };
    let hovered = |i: usize| frame.hover == Some(Hit::Feature(i));
    // Lines and polygons stay visible over a heatmap.
    for (i, shape) in frame.shapes.iter().enumerate() {
        let emphasized = frame.selected == Some(i) || hovered(i);
        let base = frame
            .data
            .features
            .get(i)
            .map(|f| feature_color(f.color))
            .unwrap_or(DEFAULT_MARKER);
        let color = if emphasized { highlight } else { base };
        match shape {
            WorldShape::Point(..) => {}
            WorldShape::Line(pts) => {
                let Some(first) = pts.first() else { continue };
                let shift = shift_for(first.0);
                let mut path = gpui::PathBuilder::stroke(px(if emphasized { 3.0 } else { 2.0 }));
                path.move_to(screen(*first, shift));
                for p in &pts[1..] {
                    path.line_to(screen(*p, shift));
                }
                if pts.len() > 1
                    && let Ok(path) = path.build()
                {
                    window.paint_path(path, color);
                }
            }
            WorldShape::Polygon(rings) => {
                let Some(first) = rings.first().and_then(|r| r.first()) else {
                    continue;
                };
                let shift = shift_for(first.0);
                if let Some(outer) = rings.first()
                    && outer.len() > 2
                {
                    let mut area = gpui::PathBuilder::fill();
                    area.move_to(screen(outer[0], shift));
                    for p in &outer[1..] {
                        area.line_to(screen(*p, shift));
                    }
                    area.close();
                    if let Ok(path) = area.build() {
                        window.paint_path(path, color.opacity(if emphasized { 0.35 } else { 0.2 }));
                    }
                }
                for ring in rings {
                    if ring.len() < 2 {
                        continue;
                    }
                    let mut path = gpui::PathBuilder::stroke(px(1.5));
                    path.move_to(screen(ring[0], shift));
                    for p in &ring[1..] {
                        path.line_to(screen(*p, shift));
                    }
                    if let Ok(path) = path.build() {
                        window.paint_path(path, color);
                    }
                }
            }
        }
    }
    if !frame.points {
        return;
    }
    // A white ring separates markers from both light tiles and the dark sketch.
    let ring = hsla(0.0, 0.0, 1.0, 0.95);
    for (i, shape) in frame.shapes.iter().enumerate() {
        let WorldShape::Point(x, y) = shape else {
            continue;
        };
        if frame.clustered.get(i).copied().unwrap_or(false) {
            continue;
        }
        let (sx, sy) = vp.world_to_screen(x + shift_for(*x), *y);
        if !on_screen((sx, sy)) {
            continue;
        }
        let feature = frame.data.features.get(i);
        let emphasized = frame.selected == Some(i) || hovered(i);
        let base = marker_diameter(feature.and_then(|f| f.size));
        let d = if emphasized { base * 1.5 } else { base };
        let color = if emphasized {
            highlight
        } else {
            feature
                .map(|f| feature_color(f.color))
                .unwrap_or(DEFAULT_MARKER)
        };
        window.paint_quad(
            fill(rect(sx - d / 2.0, sy - d / 2.0, d, d), color)
                .corner_radii(px(d as f32 / 2.0))
                .border_widths(px(2.0))
                .border_color(ring),
        );
    }
    for (i, c) in frame.clusters.iter().enumerate() {
        let r = c.radius()
            * if frame.hover == Some(Hit::Cluster(i)) {
                1.15
            } else {
                1.0
            };
        window.paint_quad(
            fill(
                rect(c.x - r, c.y - r, r * 2.0, r * 2.0),
                DEFAULT_MARKER.opacity(0.8),
            )
            .corner_radii(px(r as f32))
            .border_widths(px(2.0))
            .border_color(ring),
        );
    }
}

impl Render for MapView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = self.toolbar(cx);
        let notes = self.notes(cx);
        let empty = if self.sources().is_empty()
            && self.source.is_none()
            && matches!(self.content, MapContent::Rows { .. })
        {
            Some(tr("map_view.empty.no_sources"))
        } else if self.data.features.is_empty() {
            Some(tr("map_view.empty.no_features"))
        } else {
            None
        };
        let entity = cx.entity();
        let bounds_cell = self.canvas_bounds.clone();
        let canvas_el = canvas(
            move |bounds, _window, cx| {
                bounds_cell.set(Some(bounds));
                entity.update(cx, |this, cx| this.prepare_frame(bounds, cx))
            },
            move |bounds, frame, window, _cx| {
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    paint_frame(&frame, bounds, window)
                })
            },
        )
        .size_full();

        let ask = self.tiles_wanted()
            && !cfg!(target_arch = "wasm32")
            && self.tiles.choice() == MapTileChoice::Ask
            && empty.is_none();
        let mut body = div()
            .id("map-canvas")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .cursor(if self.drag.is_some_and(|(_, moved)| moved) {
                gpui::CursorStyle::ClosedHand
            } else if self.hover.is_some() {
                gpui::CursorStyle::PointingHand
            } else {
                gpui::CursorStyle::OpenHand
            })
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.drag = None),
            )
            .on_scroll(cx.listener(Self::on_scroll))
            .on_pinch(cx.listener(Self::on_pinch))
            .child(canvas_el);
        if let Some(message) = empty {
            body = body.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p(px(Theme::scaled(24.0)))
                    .text_color(Theme::text_dim())
                    .child(message),
            );
        }
        if let Some(tip) = self.tooltip() {
            body = body.child(tip);
        }
        if self.show_tiles() {
            body = body.child(self.attribution());
        }
        if ask {
            body = body.child(self.consent_card(cx));
        }
        let panel = self.show_panel.then(|| self.options_panel(window, cx));

        div()
            .id(SharedString::from(format!("map-view-{}", self.title)))
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::bg())
            .text_color(Theme::text())
            .child(toolbar)
            .children(notes)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(body)
                    .children(panel),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map_view::directive::{directive_line, parse_body};
    use crate::map_view::tiles::{TileFetcher, TileStore};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn rows_content() -> MapContent {
        let col = |n: &str, t: &str| ResultColumn {
            name: n.into(),
            data_type: t.into(),
            nullable: true,
        };
        let columns = vec![
            col("name", "text"),
            col("lon", "float"),
            col("lat", "float"),
            col("alt_lon", "float"),
            col("alt_lat", "float"),
            col("n", "int"),
        ];
        let rows: Vec<Vec<CellValue>> = (0..10)
            .map(|i| {
                vec![
                    CellValue::String(format!("p{i}")),
                    CellValue::F64(i as f64),
                    CellValue::F64(i as f64 / 2.0),
                    CellValue::F64(-(i as f64)),
                    CellValue::F64(-(i as f64) / 2.0),
                    CellValue::I64(i),
                ]
            })
            .collect();
        let sources = crate::map_view::features::map_sources(&columns, &rows);
        MapContent::Rows {
            columns,
            rows: Arc::new(rows),
            sources,
        }
    }

    fn counting_store(choice: MapTileChoice) -> (TileStore<TileImage>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let fetcher: TileFetcher = Arc::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(Vec::new()) })
        });
        (TileStore::new(choice, fetcher), calls)
    }

    fn view_with(directive: &str, choice: MapTileChoice) -> (MapView, Arc<AtomicUsize>) {
        let (store, calls) = counting_store(choice);
        let d = (!directive.is_empty()).then(|| parse_body(directive).0);
        let mut v = MapView::with_tiles("t", rows_content(), d, MapHooks::default(), store);
        v.viewport.width = 800.0;
        v.viewport.height = 600.0;
        v.viewport.zoom = 3.0;
        (v, calls)
    }

    #[test]
    fn basemap_none_never_requests_tiles() {
        let (mut v, calls) = view_with("basemap=none", MapTileChoice::Allowed);
        assert!(v.request_visible_tiles().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // The same map with the street map on does fetch (consent given).
        let (mut v, calls) = view_with("", MapTileChoice::Allowed);
        assert!(!v.request_visible_tiles().is_empty());
        assert!(calls.load(Ordering::SeqCst) > 0);
        // basemap=osm never bypasses consent.
        let (mut v, calls) = view_with("basemap=osm", MapTileChoice::Ask);
        assert!(v.request_visible_tiles().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn directive_picks_source_and_options_round_trip_through_the_panel_model() {
        let (mut v, _) = view_with("lat=alt_lat; lon=alt_lon; limit=4", MapTileChoice::Declined);
        assert_eq!(v.source, Some(MapSource::LonLat { lon: 3, lat: 4 }));
        assert_eq!(v.data.features.len(), 4);
        assert!(v.data.is_capped());
        // What the panel does: edit the live settings, then apply them.
        let mut d = v.options.clone();
        d.mode = Some(MapMode::Both);
        d.weight = Some("n".into());
        d.radius = Some(36.0);
        d.palette = Some(HeatPalette::Ocean);
        d.tooltip = vec!["n".into()];
        d.limit = None;
        v.apply_options(d.clone());
        assert_eq!(v.data.features.len(), 10);
        assert_eq!(v.data.features[3].weight, 3.0);
        assert_eq!(
            v.tooltip_lines(Hit::Feature(3)),
            vec!["p3".to_string(), "n: 3".to_string()]
        );
        // The panel's settings and the directive line it writes agree.
        let line = directive_line(&v.options);
        let reparsed = crate::map_view::directive::parse(&line);
        assert_eq!(reparsed, vec![d]);
        // A missing column is skipped and noted, and auto-detection is used.
        let mut bad = v.options.clone();
        bad.lat = Some("nope".into());
        bad.lon = Some("lon".into());
        v.apply_options(bad);
        assert_eq!(v.missing, vec!["nope".to_string()]);
        assert_eq!(v.source, Some(MapSource::LonLat { lon: 1, lat: 2 }));
    }

    #[test]
    fn graticule_spacing_follows_the_zoom() {
        assert_eq!(graticule_step(360.0), 30.0);
        assert_eq!(graticule_step(25.0), 5.0);
        assert_eq!(graticule_step(3.0), 0.5);
        assert_eq!(graticule_step(0.001), 0.01);
    }

    #[test]
    fn styling_helpers() {
        assert_eq!(marker_diameter(None), f64::from(POINT_PX));
        assert_eq!(marker_diameter(Some(1.0)), 28.0);
        assert_ne!(
            feature_color(ColorKey::Category(0)),
            feature_color(ColorKey::Category(1))
        );
        assert_eq!(
            feature_color(ColorKey::Category(MAX_CATEGORIES)),
            feature_color(ColorKey::Category(MAX_CATEGORIES + 3))
        );
    }

    fn world_point(lon: f64, lat: f64) -> WorldShape {
        let (x, y) = lonlat_to_world(lon, lat);
        WorldShape::Point(x, y)
    }

    fn centered(lon: f64, lat: f64, zoom: f64) -> Viewport {
        let mut vp = Viewport::new(400.0, 300.0);
        vp.center = lonlat_to_world(lon, lat);
        vp.zoom = zoom;
        vp
    }

    #[test]
    fn hit_test_finds_nearest_point_within_reach() {
        let shapes = vec![world_point(0.0, 0.0), world_point(0.0001, 0.0)];
        let vp = centered(0.0, 0.0, 10.0);
        // The view center is exactly on the first point.
        assert_eq!(hit_test(&shapes, &vp, true, (200.0, 150.0)), Some(0));
        assert_eq!(hit_test(&shapes, &vp, true, (260.0, 150.0)), None);
    }

    #[test]
    fn hit_test_sees_points_across_the_antimeridian() {
        let shapes = vec![world_point(-179.99, 0.0)];
        let vp = centered(179.99, 0.0, 12.0);
        let (wx, wy) = lonlat_to_world(-179.99, 0.0);
        let (sx, sy) = vp.world_to_screen(vp.nearest_copy(wx), wy);
        assert!((0.0..400.0).contains(&sx), "drawn on screen: {sx}");
        assert_eq!(hit_test(&shapes, &vp, true, (sx, sy)), Some(0));
    }

    #[test]
    fn hit_test_lines_and_polygons_with_holes() {
        let proj = Projection::planar_for(None);
        let square = |x0: f64, x1: f64| {
            vec![
                Coord { x: x0, y: x0 },
                Coord { x: x1, y: x0 },
                Coord { x: x1, y: x1 },
                Coord { x: x0, y: x1 },
                Coord { x: x0, y: x0 },
            ]
        };
        let poly = to_world(
            &Shape::Polygon(vec![square(0.0, 100.0), square(40.0, 60.0)]),
            &proj,
        );
        let line = to_world(
            &Shape::LineString(vec![
                Coord { x: 0.0, y: 150.0 },
                Coord { x: 100.0, y: 150.0 },
            ]),
            &proj,
        );
        let shapes = vec![poly, line];
        let mut vp = Viewport::new(400.0, 400.0);
        vp.zoom = 0.0;
        vp.center = (50.0, -25.0);
        let at = |x: f64, y: f64| vp.world_to_screen(x, -y);
        assert_eq!(hit_test(&shapes, &vp, false, at(20.0, 20.0)), Some(0));
        assert_eq!(
            hit_test(&shapes, &vp, false, at(50.0, 50.0)),
            None,
            "inside the hole"
        );
        assert_eq!(hit_test(&shapes, &vp, false, at(30.0, 151.0)), Some(1));
        assert_eq!(hit_test(&shapes, &vp, false, at(30.0, 170.0)), None);
    }

    #[test]
    fn world_extent_wraps_short_way_and_projects_lines() {
        let shapes = vec![world_point(170.0, 10.0), world_point(-170.0, -10.0)];
        let ((min_x, _), (max_x, _)) = world_extent(&shapes, true).unwrap();
        assert!((max_x - min_x - 20.0 / 360.0 * TILE_SIZE).abs() < 1e-9);
        let line = to_world(
            &Shape::LineString(vec![
                Coord { x: 175.0, y: 0.0 },
                Coord { x: -175.0, y: 0.0 },
            ]),
            &Projection::Mercator,
        );
        let WorldShape::Line(pts) = line else {
            panic!()
        };
        assert!(pts[1].0 > pts[0].0, "unwrapped eastward across 180°");
        assert!(world_extent(&[], true).is_none());
    }

    #[test]
    fn decode_rejects_non_images() {
        assert_eq!(decode_tile(b"not a png").err(), Some(FetchError::BadImage));
    }
}
