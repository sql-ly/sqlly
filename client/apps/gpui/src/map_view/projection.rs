//! Web Mercator math for the map view: longitude/latitude ↔ world pixels ↔
//! tile coordinates ↔ screen pixels, plus the pan/zoom/fit viewport.
//!
//! "World" coordinates are zoom-0 pixels: the whole Mercator square is
//! `TILE_SIZE` × `TILE_SIZE`, x growing east from the antimeridian and y
//! growing south from the top edge (≈85.05° N). Zooming multiplies by
//! `2^zoom`, so a viewport only stores a world center and a zoom.

use sqlly_core::spatial::{BoundingBox, Coord};

/// Side of one map tile, and of the whole world at zoom 0, in pixels.
pub const TILE_SIZE: f64 = 256.0;
/// Latitude where the Mercator square ends (the tiles' top and bottom edge).
pub const MAX_LATITUDE: f64 = 85.051_128_779_806_59;
/// Lowest zoom: the whole world in one tile.
pub const MIN_ZOOM: f64 = 0.0;
/// Highest zoom the OpenStreetMap standard layer serves.
pub const MAX_TILE_ZOOM: f64 = 19.0;
/// Highest zoom for a plain (tile-less) sketch of projected coordinates.
pub const MAX_PLANAR_ZOOM: f64 = 24.0;
/// Most tiles a single frame asks for (a huge window at a fractional zoom
/// could otherwise request dozens at once).
pub const MAX_VISIBLE_TILES: usize = 48;

/// Clamp a zoom into `[MIN_ZOOM, max]`; non-finite input becomes `MIN_ZOOM`.
pub fn clamp_zoom(zoom: f64, max: f64) -> f64 {
    if zoom.is_finite() {
        zoom.clamp(MIN_ZOOM, max)
    } else {
        MIN_ZOOM
    }
}

/// Clamp a latitude to the Mercator square.
pub fn clamp_latitude(lat: f64) -> f64 {
    lat.clamp(-MAX_LATITUDE, MAX_LATITUDE)
}

/// Fold any longitude into `[-180, 180)`.
#[cfg_attr(not(test), allow(dead_code))] // Inverse/reference math, exercised by the tests.
pub fn wrap_longitude(lon: f64) -> f64 {
    let wrapped = (lon + 180.0).rem_euclid(360.0) - 180.0;
    if wrapped.is_finite() { wrapped } else { 0.0 }
}

/// Longitude/latitude → zoom-0 world pixels. Latitude is clamped to the
/// Mercator square; longitude is not wrapped, so 190° lands just past the
/// right edge (callers wrap or unwrap as they need).
pub fn lonlat_to_world(lon: f64, lat: f64) -> (f64, f64) {
    let x = (lon + 180.0) / 360.0 * TILE_SIZE;
    let lat = clamp_latitude(lat).to_radians();
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * TILE_SIZE;
    (x, y)
}

/// Zoom-0 world pixels → longitude/latitude (longitude wrapped).
pub fn world_to_lonlat(x: f64, y: f64) -> (f64, f64) {
    let lon = wrap_longitude(x / TILE_SIZE * 360.0 - 180.0);
    let n = std::f64::consts::PI * (1.0 - 2.0 * y / TILE_SIZE);
    let lat = n.sinh().atan().to_degrees();
    (lon, lat)
}

/// One OpenStreetMap tile address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileKey {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

/// The tile containing a longitude/latitude at an integer zoom.
#[cfg_attr(not(test), allow(dead_code))] // Inverse/reference math, exercised by the tests.
pub fn tile_containing(lon: f64, lat: f64, z: u8) -> TileKey {
    let (wx, wy) = lonlat_to_world(wrap_longitude(lon), lat);
    let n = 1u64 << z;
    let tile_world = TILE_SIZE / n as f64;
    let x = ((wx / tile_world).floor() as i64).clamp(0, n as i64 - 1);
    let y = ((wy / tile_world).floor() as i64).clamp(0, n as i64 - 1);
    TileKey {
        z,
        x: x as u32,
        y: y as u32,
    }
}

/// A tile column index that may run past either edge (the map repeats
/// sideways across the antimeridian) folded back into `[0, 2^z)`.
pub fn wrap_tile_x(x: i64, z: u8) -> u32 {
    x.rem_euclid(1i64 << z) as u32
}

/// How data coordinates become world coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Projection {
    /// Longitude/latitude on Web Mercator, under OpenStreetMap tiles.
    Mercator,
    /// Projected or unknown coordinates drawn as-is on a plain sketch:
    /// `world = ((x - origin_x) * scale, (origin_y - y) * scale)`.
    Planar {
        origin_x: f64,
        origin_y: f64,
        scale: f64,
    },
}

impl Projection {
    /// A planar projection that fits `bbox` into the zoom-0 world square.
    pub fn planar_for(bbox: Option<BoundingBox>) -> Self {
        let Some(b) = bbox else {
            return Self::Planar {
                origin_x: 0.0,
                origin_y: 0.0,
                scale: 1.0,
            };
        };
        let extent = (b.max_x - b.min_x).max(b.max_y - b.min_y);
        let scale = if extent.is_finite() && extent > 0.0 {
            TILE_SIZE / extent
        } else {
            1.0
        };
        Self::Planar {
            origin_x: b.min_x,
            origin_y: b.max_y,
            scale,
        }
    }

    pub fn project(&self, c: Coord) -> (f64, f64) {
        match *self {
            Self::Mercator => lonlat_to_world(c.x, c.y),
            Self::Planar {
                origin_x,
                origin_y,
                scale,
            } => ((c.x - origin_x) * scale, (origin_y - c.y) * scale),
        }
    }

    /// World coordinates back to data coordinates (longitude/latitude for
    /// Mercator), as a `Coord`.
    pub fn unproject(&self, (wx, wy): (f64, f64)) -> Coord {
        match *self {
            Self::Mercator => {
                let (x, y) = world_to_lonlat(wx, wy);
                Coord { x, y }
            }
            Self::Planar {
                origin_x,
                origin_y,
                scale,
            } => Coord {
                x: wx / scale + origin_x,
                y: origin_y - wy / scale,
            },
        }
    }

    /// Whether the world repeats sideways (Mercator across the antimeridian).
    pub fn wraps(&self) -> bool {
        matches!(self, Self::Mercator)
    }

    pub fn max_zoom(&self) -> f64 {
        match self {
            Self::Mercator => MAX_TILE_ZOOM,
            Self::Planar { .. } => MAX_PLANAR_ZOOM,
        }
    }
}

/// Re-anchor each x in a path so consecutive vertices never jump more than
/// half a world: a line from 179° E to 179° W crosses the antimeridian
/// instead of streaking back across the whole map.
pub fn unwrap_path(points: &mut [(f64, f64)]) {
    for i in 1..points.len() {
        let prev = points[i - 1].0;
        let x = &mut points[i].0;
        *x += TILE_SIZE * ((prev - *x) / TILE_SIZE).round();
    }
}

/// The narrowest horizontal span covering every world x on a repeating
/// world: points at 170° E and 170° W span 20°, not 340°. Returns
/// `(min, max)` where `max` may exceed `TILE_SIZE` when the span crosses
/// the antimeridian.
pub fn wrapped_x_extent(xs: &[f64]) -> Option<(f64, f64)> {
    let mut sorted: Vec<f64> = xs
        .iter()
        .copied()
        .filter(|x| x.is_finite())
        .map(|x| x.rem_euclid(TILE_SIZE))
        .collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(f64::total_cmp);
    let (first, last) = (sorted[0], sorted[sorted.len() - 1]);
    // The gap that wraps from the last value round to the first.
    let mut best_gap = first + TILE_SIZE - last;
    let mut span = (first, last);
    for pair in sorted.windows(2) {
        let gap = pair[1] - pair[0];
        if gap > best_gap {
            best_gap = gap;
            span = (pair[1], pair[0] + TILE_SIZE);
        }
    }
    Some(span)
}

/// One tile placed on screen: its address and its top-left corner and
/// side in viewport pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TilePlacement {
    pub key: TileKey,
    pub x: f64,
    pub y: f64,
    pub size: f64,
}

/// What the map window currently shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// World point at the viewport center (zoom-0 pixels).
    pub center: (f64, f64),
    pub zoom: f64,
    /// Viewport size in logical pixels.
    pub width: f64,
    pub height: f64,
}

impl Viewport {
    pub fn new(width: f64, height: f64) -> Self {
        Self {
            center: (TILE_SIZE / 2.0, TILE_SIZE / 2.0),
            zoom: 1.0,
            width,
            height,
        }
    }

    /// Pixels per world unit.
    pub fn scale(&self) -> f64 {
        2f64.powf(self.zoom)
    }

    pub fn world_to_screen(&self, wx: f64, wy: f64) -> (f64, f64) {
        let s = self.scale();
        (
            (wx - self.center.0) * s + self.width / 2.0,
            (wy - self.center.1) * s + self.height / 2.0,
        )
    }

    pub fn screen_to_world(&self, sx: f64, sy: f64) -> (f64, f64) {
        let s = self.scale();
        (
            (sx - self.width / 2.0) / s + self.center.0,
            (sy - self.height / 2.0) / s + self.center.1,
        )
    }

    /// The copy of a repeating world x closest to the view center, so a
    /// point at 179° W shows up next to a view centered on 179° E.
    pub fn nearest_copy(&self, wx: f64) -> f64 {
        wx + TILE_SIZE * ((self.center.0 - wx) / TILE_SIZE).round()
    }

    /// Drag the map by a screen delta.
    pub fn pan_by(&mut self, dx: f64, dy: f64, projection: &Projection) {
        let s = self.scale();
        self.center.0 -= dx / s;
        self.center.1 -= dy / s;
        self.normalize(projection);
    }

    /// Zoom to `zoom`, keeping the world point under `anchor` (screen
    /// pixels) fixed — the wheel, pinch, and +/- buttons all go through here.
    pub fn zoom_around(&mut self, zoom: f64, anchor: (f64, f64), projection: &Projection) {
        let before = self.screen_to_world(anchor.0, anchor.1);
        self.zoom = clamp_zoom(zoom, projection.max_zoom());
        let after = self.screen_to_world(anchor.0, anchor.1);
        self.center.0 += before.0 - after.0;
        self.center.1 += before.1 - after.1;
        self.normalize(projection);
    }

    /// Center and zoom so the world rectangle fills the viewport, leaving
    /// `padding` pixels at each edge. A single point (zero-size box) gets
    /// `point_zoom`.
    pub fn fit(
        &mut self,
        min: (f64, f64),
        max: (f64, f64),
        padding: f64,
        point_zoom: f64,
        projection: &Projection,
    ) {
        let w = (max.0 - min.0).max(0.0);
        let h = (max.1 - min.1).max(0.0);
        let avail_w = (self.width - padding * 2.0).max(1.0);
        let avail_h = (self.height - padding * 2.0).max(1.0);
        let zoom = if w <= f64::EPSILON && h <= f64::EPSILON {
            point_zoom
        } else {
            let sx = if w > 0.0 { avail_w / w } else { f64::INFINITY };
            let sy = if h > 0.0 { avail_h / h } else { f64::INFINITY };
            sx.min(sy).log2()
        };
        self.zoom = clamp_zoom(zoom, projection.max_zoom());
        self.center = ((min.0 + max.0) / 2.0, (min.1 + max.1) / 2.0);
        self.normalize(projection);
    }

    /// Keep the center inside the world: wrap x on a repeating map, clamp
    /// y to the Mercator square.
    fn normalize(&mut self, projection: &Projection) {
        if !self.center.0.is_finite() || !self.center.1.is_finite() {
            self.center = (TILE_SIZE / 2.0, TILE_SIZE / 2.0);
        }
        if projection.wraps() {
            self.center.0 = self.center.0.rem_euclid(TILE_SIZE);
            self.center.1 = self.center.1.clamp(0.0, TILE_SIZE);
        }
    }

    /// Integer tile zoom for the current (fractional) zoom.
    pub fn tile_zoom(&self) -> u8 {
        clamp_zoom(self.zoom.round(), MAX_TILE_ZOOM) as u8
    }

    /// Every tile that overlaps the viewport, left to right, top to bottom,
    /// with x wrapped across the antimeridian and rows past the poles
    /// dropped. Never more than [`MAX_VISIBLE_TILES`].
    pub fn visible_tiles(&self) -> Vec<TilePlacement> {
        let z = self.tile_zoom();
        let n = 1i64 << z;
        let tile_world = TILE_SIZE / n as f64;
        let (left, top) = self.screen_to_world(0.0, 0.0);
        let (right, bottom) = self.screen_to_world(self.width, self.height);
        let x0 = (left / tile_world).floor() as i64;
        let x1 = (right / tile_world).floor() as i64;
        let y0 = ((top / tile_world).floor() as i64).max(0);
        let y1 = ((bottom / tile_world).floor() as i64).min(n - 1);
        let size = tile_world * self.scale();
        let mut out = Vec::new();
        'rows: for ty in y0..=y1 {
            for tx in x0..=x1 {
                if out.len() >= MAX_VISIBLE_TILES {
                    break 'rows;
                }
                let (sx, sy) = self.world_to_screen(tx as f64 * tile_world, ty as f64 * tile_world);
                out.push(TilePlacement {
                    key: TileKey {
                        z,
                        x: wrap_tile_x(tx, z),
                        y: ty as u32,
                    },
                    x: sx,
                    y: sy,
                    size,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn lonlat_world_round_trips() {
        for &(lon, lat) in &[
            (0.0, 0.0),
            (-122.4194, 37.7749),
            (151.2093, -33.8688),
            (179.9, 85.0),
            (-179.9, -85.0),
        ] {
            let (x, y) = lonlat_to_world(lon, lat);
            let (lon2, lat2) = world_to_lonlat(x, y);
            assert!(close(lon, lon2, 1e-9), "{lon} vs {lon2}");
            assert!(close(lat, lat2, 1e-9), "{lat} vs {lat2}");
        }
    }

    #[test]
    fn world_corners_and_center() {
        assert_eq!(lonlat_to_world(0.0, 0.0), (128.0, 128.0));
        let (x, y) = lonlat_to_world(-180.0, MAX_LATITUDE);
        assert!(close(x, 0.0, 1e-9) && close(y, 0.0, 1e-6));
        let (x, y) = lonlat_to_world(180.0, -MAX_LATITUDE);
        assert!(close(x, 256.0, 1e-9) && close(y, 256.0, 1e-6));
        // Beyond the Mercator square latitude clamps to the edge.
        assert_eq!(
            lonlat_to_world(0.0, 90.0),
            lonlat_to_world(0.0, MAX_LATITUDE)
        );
    }

    #[test]
    fn tile_containing_matches_osm_reference() {
        // The OSM wiki's slippy-map example: Munich-ish at zoom 10.
        assert_eq!(
            tile_containing(11.5, 48.1, 10),
            TileKey {
                z: 10,
                x: 544,
                y: 355
            }
        );
        assert_eq!(tile_containing(0.0, 0.0, 0), TileKey { z: 0, x: 0, y: 0 });
        // Exactly 180° folds to the western edge instead of overflowing.
        assert_eq!(tile_containing(180.0, 0.0, 2).x, 0);
        // A pole clamps into the last row.
        assert_eq!(tile_containing(0.0, -90.0, 3).y, 7);
    }

    #[test]
    fn longitude_and_tile_x_wrap() {
        assert_eq!(wrap_longitude(190.0), -170.0);
        assert_eq!(wrap_longitude(-190.0), 170.0);
        assert_eq!(wrap_longitude(180.0), -180.0);
        assert_eq!(wrap_tile_x(-1, 2), 3);
        assert_eq!(wrap_tile_x(4, 2), 0);
        assert_eq!(wrap_tile_x(9, 3), 1);
    }

    #[test]
    fn zoom_clamps_to_projection_limits() {
        assert_eq!(clamp_zoom(-3.0, MAX_TILE_ZOOM), 0.0);
        assert_eq!(clamp_zoom(25.0, MAX_TILE_ZOOM), 19.0);
        assert_eq!(clamp_zoom(f64::NAN, MAX_TILE_ZOOM), 0.0);
        let mut v = Viewport::new(800.0, 600.0);
        v.zoom_around(40.0, (400.0, 300.0), &Projection::Mercator);
        assert_eq!(v.zoom, MAX_TILE_ZOOM);
        let planar = Projection::planar_for(None);
        v.zoom_around(40.0, (400.0, 300.0), &planar);
        assert_eq!(v.zoom, MAX_PLANAR_ZOOM);
    }

    #[test]
    fn screen_world_round_trip_and_anchor_zoom() {
        let mut v = Viewport::new(800.0, 600.0);
        v.zoom = 3.0;
        v.center = (100.0, 90.0);
        let (wx, wy) = v.screen_to_world(123.0, 456.0);
        let (sx, sy) = v.world_to_screen(wx, wy);
        assert!(close(sx, 123.0, 1e-9) && close(sy, 456.0, 1e-9));
        // Zooming around a point keeps that point under the cursor.
        v.zoom_around(5.5, (123.0, 456.0), &Projection::Mercator);
        let (sx, sy) = v.world_to_screen(wx, wy);
        assert!(close(sx, 123.0, 1e-6) && close(sy, 456.0, 1e-6));
    }

    #[test]
    fn pan_wraps_across_the_antimeridian_and_clamps_poles() {
        let mut v = Viewport::new(512.0, 512.0);
        v.zoom = 1.0;
        v.center = (10.0, 128.0);
        // Drag right by 40px at zoom 1 = 20 world units west.
        v.pan_by(40.0, 0.0, &Projection::Mercator);
        assert!(close(v.center.0, 246.0, 1e-9));
        v.pan_by(0.0, 10_000.0, &Projection::Mercator);
        assert_eq!(v.center.1, 0.0);
        // A planar sketch never wraps.
        let planar = Projection::planar_for(None);
        let mut p = Viewport::new(512.0, 512.0);
        p.center = (10.0, 10.0);
        p.pan_by(100.0, 0.0, &planar);
        assert!(p.center.0 < 0.0);
    }

    #[test]
    fn nearest_copy_brings_far_side_points_next_to_the_view() {
        let mut v = Viewport::new(800.0, 600.0);
        v.center = lonlat_to_world(179.0, 0.0);
        let (wx, _) = lonlat_to_world(-179.0, 0.0);
        let near = v.nearest_copy(wx);
        assert!(near > TILE_SIZE, "drawn one world to the right: {near}");
        assert!(close(near - v.center.0, 2.0 / 360.0 * TILE_SIZE, 1e-9));
    }

    #[test]
    fn unwrap_path_crosses_the_antimeridian_the_short_way() {
        let mut pts = vec![
            lonlat_to_world(170.0, 0.0),
            lonlat_to_world(-170.0, 0.0),
            lonlat_to_world(-160.0, 0.0),
        ];
        unwrap_path(&mut pts);
        assert!(pts[1].0 > pts[0].0 && pts[2].0 > pts[1].0);
        assert!(close(pts[1].0 - pts[0].0, 20.0 / 360.0 * TILE_SIZE, 1e-9));
    }

    #[test]
    fn wrapped_extent_picks_the_narrow_side() {
        let xs = [
            lonlat_to_world(170.0, 0.0).0,
            lonlat_to_world(-170.0, 0.0).0,
        ];
        let (min, max) = wrapped_x_extent(&xs).unwrap();
        assert!(
            close(max - min, 20.0 / 360.0 * TILE_SIZE, 1e-9),
            "{min}..{max}"
        );
        let plain = [10.0, 20.0, 30.0];
        assert_eq!(wrapped_x_extent(&plain), Some((10.0, 30.0)));
        assert_eq!(wrapped_x_extent(&[]), None);
    }

    #[test]
    fn fit_frames_data_and_single_points() {
        let mut v = Viewport::new(800.0, 600.0);
        v.fit(
            (100.0, 100.0),
            (110.0, 105.0),
            20.0,
            12.0,
            &Projection::Mercator,
        );
        // 760 px for 10 world units → 2^zoom = 76.
        assert!(close(v.scale(), 76.0, 1e-6));
        assert_eq!(v.center, (105.0, 102.5));
        v.fit(
            (50.0, 60.0),
            (50.0, 60.0),
            20.0,
            12.0,
            &Projection::Mercator,
        );
        assert_eq!(v.zoom, 12.0);
        assert_eq!(v.center, (50.0, 60.0));
    }

    #[test]
    fn visible_tiles_cover_the_viewport_and_wrap() {
        let mut v = Viewport::new(512.0, 256.0);
        v.zoom = 1.0;
        v.center = (10.0, 128.0);
        let tiles = v.visible_tiles();
        // At zoom 1 the world is 512 px: centered just east of the
        // antimeridian we see the end of x=1 (wrapped from x=-1), x=0, and
        // the start of x=1.
        assert!(
            tiles
                .iter()
                .all(|t| t.key.z == 1 && t.key.x < 2 && t.key.y < 2)
        );
        assert!(
            tiles.iter().any(|t| t.x < 0.0),
            "a tile hangs off the left edge"
        );
        assert!(tiles.iter().all(|t| close(t.size, 256.0, 1e-9)));
        // Rows past the poles are never requested.
        v.center = (128.0, 0.0);
        assert!(
            v.visible_tiles()
                .iter()
                .all(|t| t.y >= -256.0 && t.key.y < 2)
        );
        // A huge window is capped.
        let mut big = Viewport::new(20_000.0, 20_000.0);
        big.zoom = 10.0;
        assert_eq!(big.visible_tiles().len(), MAX_VISIBLE_TILES);
    }

    #[test]
    fn unproject_inverts_both_projections() {
        let c = Coord {
            x: -0.1278,
            y: 51.5074,
        };
        let back = Projection::Mercator.unproject(Projection::Mercator.project(c));
        assert!(close(back.x, c.x, 1e-9) && close(back.y, c.y, 1e-9));
        let p = Projection::Planar {
            origin_x: 10.0,
            origin_y: 20.0,
            scale: 0.5,
        };
        let back = p.unproject(p.project(c));
        assert!(close(back.x, c.x, 1e-9) && close(back.y, c.y, 1e-9));
    }

    #[test]
    fn planar_projection_fits_bbox_into_the_world_square() {
        let p = Projection::planar_for(Some(BoundingBox {
            min_x: 500_000.0,
            min_y: 100_000.0,
            max_x: 501_000.0,
            max_y: 100_500.0,
        }));
        assert_eq!(
            p.project(Coord {
                x: 500_000.0,
                y: 100_500.0
            }),
            (0.0, 0.0)
        );
        let (x, y) = p.project(Coord {
            x: 501_000.0,
            y: 100_000.0,
        });
        assert!(close(x, 256.0, 1e-9) && close(y, 128.0, 1e-9));
        assert!(!p.wraps());
    }
}
