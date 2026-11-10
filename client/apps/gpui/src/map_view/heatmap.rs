//! Point density for the heatmap: bin screen-space points into a coarse
//! grid, then smooth the counts with a separable kernel so each point
//! spreads over `radius` pixels. Pure math; the view paints the grid.

use super::directive::HeatPalette;

/// Side of one heatmap cell in screen pixels.
pub const CELL_PX: f64 = 4.0;
/// Radius plus blur, in cells, above which cells grow instead.
const MAX_SPREAD_CELLS: f64 = 12.0;
/// Smoothing radius bounds and default, in screen pixels.
pub const MIN_RADIUS_PX: f64 = 6.0;
pub const DEFAULT_RADIUS_PX: f64 = 24.0;

/// Smoothed density over the viewport, row-major.
#[derive(Debug, Clone, PartialEq)]
pub struct HeatGrid {
    pub cols: usize,
    pub rows: usize,
    pub cell: f64,
    pub values: Vec<f32>,
    pub max: f32,
}

impl HeatGrid {
    pub fn at(&self, col: usize, row: usize) -> f32 {
        self.values[row * self.cols + col]
    }

    /// Value as a share of the grid maximum (0 when the grid is empty).
    pub fn normalized(&self, col: usize, row: usize) -> f32 {
        if self.max > 0.0 {
            self.at(col, row) / self.max
        } else {
            0.0
        }
    }
}

/// Count points per `cell`-sized square of a `width` × `height` viewport.
/// Points outside the viewport are dropped.
#[cfg_attr(not(test), allow(dead_code))]
pub fn bin_points(
    points: impl IntoIterator<Item = (f64, f64)>,
    width: f64,
    height: f64,
    cell: f64,
) -> HeatGrid {
    bin_weighted(
        points.into_iter().map(|(x, y)| (x, y, 1.0)),
        width,
        height,
        cell,
    )
}

/// [`bin_points`] with a weight per point (`weight=` column).
pub fn bin_weighted(
    points: impl IntoIterator<Item = (f64, f64, f32)>,
    width: f64,
    height: f64,
    cell: f64,
) -> HeatGrid {
    let cell = cell.max(1.0);
    let cols = (width / cell).ceil().max(0.0) as usize;
    let rows = (height / cell).ceil().max(0.0) as usize;
    let mut values = vec![0f32; cols * rows];
    for (x, y, w) in points {
        let inside = x >= 0.0 && y >= 0.0 && x < width && y < height;
        if !inside || w.is_nan() || w <= 0.0 {
            continue;
        }
        let c = ((x / cell) as usize).min(cols.saturating_sub(1));
        let r = ((y / cell) as usize).min(rows.saturating_sub(1));
        values[r * cols + c] += w;
    }
    let max = values.iter().copied().fold(0.0, f32::max);
    HeatGrid {
        cols,
        rows,
        cell,
        values,
        max,
    }
}

/// Normalized 1-D Gaussian weights for a radius of `r` cells: σ = r / 2,
/// reaching out to 3σ so the tail is effectively zero where it stops (a
/// shorter cut shows as a faint square edge). Sums to 1 so smoothing keeps
/// the total. A Gaussian, unlike most kernels, stays round when applied
/// separably (rows then columns): its 2-D product depends only on distance,
/// so heat spreads in circles rather than squares.
pub fn kernel(r: usize) -> Vec<f32> {
    let sigma = (r as f32 / 2.0).max(0.5);
    let half = (3.0 * sigma).ceil() as usize;
    let w: Vec<f32> = (0..=2 * half)
        .map(|i| {
            let d = i as f32 - half as f32;
            (-(d * d) / (2.0 * sigma * sigma)).exp()
        })
        .collect();
    let sum: f32 = w.iter().sum();
    w.into_iter().map(|v| v / sum).collect()
}

/// Smooth a binned grid in place with the separable kernel (rows, then
/// columns). Mass that would spill past an edge is lost, as on screen.
pub fn smooth(grid: &mut HeatGrid, radius_cells: usize) {
    if radius_cells == 0 || grid.cols == 0 || grid.rows == 0 {
        return;
    }
    let k = kernel(radius_cells);
    let r = (k.len() / 2) as isize;
    let (cols, rows) = (grid.cols as isize, grid.rows as isize);
    let mut tmp = vec![0f32; grid.values.len()];
    for y in 0..rows {
        for x in 0..cols {
            let v = grid.values[(y * cols + x) as usize];
            if v == 0.0 {
                continue;
            }
            for (i, w) in k.iter().enumerate() {
                let nx = x + i as isize - r;
                if (0..cols).contains(&nx) {
                    tmp[(y * cols + nx) as usize] += v * w;
                }
            }
        }
    }
    let mut out = vec![0f32; grid.values.len()];
    for y in 0..rows {
        for x in 0..cols {
            let v = tmp[(y * cols + x) as usize];
            if v == 0.0 {
                continue;
            }
            for (i, w) in k.iter().enumerate() {
                let ny = y + i as isize - r;
                if (0..rows).contains(&ny) {
                    out[(ny * cols + x) as usize] += v * w;
                }
            }
        }
    }
    grid.max = out.iter().copied().fold(0.0, f32::max);
    grid.values = out;
}

/// Bin and smooth: the whole heatmap for one frame.
#[cfg_attr(not(test), allow(dead_code))]
pub fn density(
    points: impl IntoIterator<Item = (f64, f64)>,
    width: f64,
    height: f64,
    radius_px: f64,
) -> HeatGrid {
    density_weighted(
        points.into_iter().map(|(x, y)| (x, y, 1.0)),
        width,
        height,
        radius_px,
        0.0,
    )
}

/// Weighted density with an extra `blur_px` softening pass.
pub fn density_weighted(
    points: impl IntoIterator<Item = (f64, f64, f32)>,
    width: f64,
    height: f64,
    radius_px: f64,
    blur_px: f64,
) -> HeatGrid {
    let radius = radius_px.clamp(MIN_RADIUS_PX, super::directive::RADIUS_RANGE.1);
    let blur = blur_px.clamp(0.0, super::directive::BLUR_RANGE.1);
    // This runs every frame: wide spreads use coarser cells (invisible in a
    // field that smooth) so the kernels stay a dozen or so cells long.
    let cell = CELL_PX.max((radius + blur) / MAX_SPREAD_CELLS);
    let mut grid = bin_weighted(points, width, height, cell);
    smooth(&mut grid, (radius / cell).round() as usize);
    smooth(&mut grid, (blur / cell).round() as usize);
    grid
}

/// Color for a normalized density `t` in `0..=1` on the default ramp:
/// transparent blue at the faint end through green and yellow to opaque
/// red at the peak, as `(hue 0..1, saturation, lightness, alpha)`.
#[cfg_attr(not(test), allow(dead_code))]
pub fn ramp(t: f32) -> (f32, f32, f32, f32) {
    shade(HeatPalette::Heat, t, 1.0, 0.8, None).unwrap_or((0.0, 0.0, 0.0, 0.0))
}

/// Linear interpolation between HSL stops (hue in degrees).
fn stops(points: &[(f32, f32, f32)], t: f32) -> (f32, f32, f32) {
    let n = points.len() - 1;
    let x = t.clamp(0.0, 1.0) * n as f32;
    let i = (x.floor() as usize).min(n - 1);
    let f = x - i as f32;
    let (a, b) = (points[i], points[i + 1]);
    (
        (a.0 + (b.0 - a.0) * f) / 360.0,
        a.1 + (b.1 - a.1) * f,
        a.2 + (b.2 - a.2) * f,
    )
}

/// A heat cell's color: `t` is the share of the peak, boosted by
/// `intensity` (1 = as is) and faded to at most `opacity`. `None` for cells
/// too faint to draw (so the basemap stays readable). `accent` is the
/// theme accent as HSL for [`HeatPalette::Accent`].
pub fn shade(
    palette: HeatPalette,
    t: f32,
    intensity: f32,
    opacity: f32,
    accent: Option<(f32, f32, f32)>,
) -> Option<(f32, f32, f32, f32)> {
    let t = (t * intensity.max(0.0)).clamp(0.0, 1.0);
    if t < 0.02 {
        return None;
    }
    let (h, s, l) = match palette {
        HeatPalette::Heat => stops(
            &[
                (240.0, 0.9, 0.5),
                (120.0, 0.9, 0.45),
                (60.0, 0.95, 0.5),
                (0.0, 0.9, 0.5),
            ],
            t,
        ),
        HeatPalette::Fire => stops(
            &[
                (0.0, 0.85, 0.25),
                (20.0, 0.95, 0.45),
                (40.0, 1.0, 0.55),
                (55.0, 1.0, 0.75),
            ],
            t,
        ),
        HeatPalette::Viridis => stops(
            &[
                (275.0, 0.6, 0.3),
                (220.0, 0.45, 0.4),
                (170.0, 0.55, 0.42),
                (90.0, 0.6, 0.48),
                (55.0, 0.9, 0.55),
            ],
            t,
        ),
        HeatPalette::Ocean => stops(
            &[(190.0, 0.7, 0.8), (205.0, 0.75, 0.55), (220.0, 0.8, 0.32)],
            t,
        ),
        HeatPalette::Accent => {
            let (h, s, l) = accent.unwrap_or((210.0 / 360.0, 0.8, 0.5));
            (h, s, l + (1.0 - t) * (0.85 - l).max(0.0) * 0.5)
        }
    };
    // Fade in from fully transparent over the faint quarter so the heat has
    // soft edges instead of a visible boundary where cells start drawing.
    let fade = (t / 0.25).min(1.0);
    let alpha = fade * (0.45 + 0.55 * t.sqrt()) * opacity.clamp(0.0, 1.0);
    Some((h, s, l, alpha))
}

/// A group of nearby markers drawn as one counted circle.
#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    /// Mean screen position of the members.
    pub x: f64,
    pub y: f64,
    /// Indices into the input, in input order.
    pub members: Vec<usize>,
}

/// Group screen points into `cell`-sized squares. Squares with a single
/// point come back as one-member clusters (drawn as plain markers).
pub fn cluster_points(points: &[(f64, f64)], cell: f64) -> Vec<Cluster> {
    let cell = cell.max(1.0);
    let mut by_cell: std::collections::HashMap<(i64, i64), usize> = Default::default();
    let mut out: Vec<Cluster> = Vec::new();
    for (i, &(x, y)) in points.iter().enumerate() {
        if !x.is_finite() || !y.is_finite() {
            continue;
        }
        let key = ((x / cell).floor() as i64, (y / cell).floor() as i64);
        let slot = *by_cell.entry(key).or_insert_with(|| {
            out.push(Cluster {
                x: 0.0,
                y: 0.0,
                members: Vec::new(),
            });
            out.len() - 1
        });
        let c = &mut out[slot];
        let n = c.members.len() as f64;
        c.x = (c.x * n + x) / (n + 1.0);
        c.y = (c.y * n + y) / (n + 1.0);
        c.members.push(i);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binning_counts_points_per_cell_and_drops_outside() {
        let pts = [
            (1.0, 1.0),
            (2.0, 5.0),
            (7.0, 1.0),
            (-1.0, 3.0),
            (30.0, 3.0),
            (11.9, 11.9),
        ];
        let g = bin_points(pts, 12.0, 12.0, 6.0);
        assert_eq!((g.cols, g.rows), (2, 2));
        assert_eq!(g.at(0, 0), 2.0);
        assert_eq!(g.at(1, 0), 1.0);
        assert_eq!(g.at(1, 1), 1.0);
        assert_eq!(g.at(0, 1), 0.0);
        assert_eq!(g.max, 2.0);
    }

    #[test]
    fn kernel_is_symmetric_normalized_and_peaked() {
        // r = 4 cells: σ = 2, reaching 3σ = 6 cells each side.
        let k = kernel(4);
        assert_eq!(k.len(), 13);
        assert!((k.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        assert_eq!(k[0], k[12]);
        assert!(k[6] > k[5] && k[5] > k[4] && k[1] > k[0] && k[0] > 0.0);
        assert!(k[0] / k[6] < 0.02, "the tail fades out before it stops");
    }

    #[test]
    fn smoothing_spreads_a_point_and_keeps_its_mass() {
        let mut g = bin_points([(30.0, 30.0)], 60.0, 60.0, 6.0);
        smooth(&mut g, 2);
        let total: f32 = g.values.iter().sum();
        assert!((total - 1.0).abs() < 1e-5, "{total}");
        assert!(g.at(5, 5) > g.at(6, 5) && g.at(6, 5) > g.at(7, 5));
        // r = 2: σ = 1, reaching 3 cells each side.
        assert!(g.at(8, 5) > 0.0 && g.at(9, 5) == 0.0);
        assert_eq!(g.at(4, 5), g.at(6, 5));
        assert_eq!(g.normalized(5, 5), 1.0);
    }

    #[test]
    fn smoothed_heat_is_round_not_square() {
        // One point at the centre cell; cells at offset (5, 0) and (3, 4)
        // are both 5 cells away, so a round spread gives them equal heat,
        // while the square corner (5, 5) gets clearly less.
        let mut g = bin_points([(63.0, 63.0)], 126.0, 126.0, 6.0);
        smooth(&mut g, 6);
        let axis = g.at(15, 10);
        let diagonal = g.at(13, 14);
        let corner = g.at(15, 15);
        assert!(
            (axis - diagonal).abs() / axis < 0.05,
            "{axis} vs {diagonal}"
        );
        assert!(corner < axis * 0.6, "{corner} vs {axis}");
    }

    #[test]
    fn dense_clusters_outrank_lone_points() {
        let mut pts = vec![(10.0, 10.0); 20];
        pts.push((100.0, 100.0));
        let g = density(pts, 120.0, 120.0, 12.0);
        let cell = |v: f64| (v / CELL_PX) as usize;
        let hot = g.normalized(cell(10.0), cell(10.0));
        let lone = g.normalized(cell(100.0), cell(100.0));
        assert_eq!(hot, 1.0);
        assert!(lone < 0.1, "{lone}");
    }

    #[test]
    fn wide_spreads_use_coarser_cells() {
        let fine = density_weighted([(60.0, 60.0, 1.0)], 400.0, 400.0, 24.0, 0.0);
        assert_eq!(fine.cell, CELL_PX);
        let wide = density_weighted([(200.0, 200.0, 1.0)], 400.0, 400.0, 120.0, 120.0);
        assert_eq!(wide.cell, 240.0 / MAX_SPREAD_CELLS);
        let total: f32 = wide.values.iter().sum();
        assert!(total > 0.9, "{total}");
    }

    #[test]
    fn empty_and_degenerate_grids() {
        let g = density(std::iter::empty(), 50.0, 50.0, 24.0);
        assert_eq!(g.max, 0.0);
        assert_eq!(g.normalized(0, 0), 0.0);
        let z = density([(1.0, 1.0)], 0.0, 0.0, 24.0);
        assert!(z.values.is_empty());
    }

    #[test]
    fn weights_scale_bins_and_skip_non_positive() {
        let g = bin_weighted(
            [
                (1.0, 1.0, 3.0),
                (2.0, 2.0, 0.5),
                (3.0, 3.0, 0.0),
                (4.0, 4.0, -2.0),
            ],
            12.0,
            12.0,
            6.0,
        );
        assert_eq!(g.at(0, 0), 3.5);
        let heavy = density_weighted(
            [(10.0, 10.0, 9.0), (100.0, 100.0, 1.0)],
            120.0,
            120.0,
            12.0,
            0.0,
        );
        let lone = (100.0 / CELL_PX) as usize;
        assert!(heavy.normalized(lone, lone) < 0.2);
    }

    #[test]
    fn blur_softens_without_losing_mass() {
        let sharp = density_weighted([(60.0, 60.0, 1.0)], 120.0, 120.0, 12.0, 0.0);
        let soft = density_weighted([(60.0, 60.0, 1.0)], 120.0, 120.0, 12.0, 24.0);
        assert!(soft.max < sharp.max);
        let total: f32 = soft.values.iter().sum();
        assert!((total - 1.0).abs() < 1e-4, "{total}");
    }

    #[test]
    fn palettes_intensity_and_opacity() {
        for p in HeatPalette::ALL {
            let (_, _, _, lo) = shade(p, 0.1, 1.0, 1.0, None).unwrap();
            let (_, _, _, hi) = shade(p, 1.0, 1.0, 1.0, None).unwrap();
            assert!(hi > lo, "{p:?}");
            assert!(shade(p, 0.01, 1.0, 1.0, None).is_none());
        }
        // Intensity lifts faint cells over the threshold; opacity caps alpha.
        assert!(shade(HeatPalette::Heat, 0.01, 4.0, 1.0, None).is_some());
        assert_eq!(
            shade(HeatPalette::Fire, 1.0, 1.0, 0.5, None).unwrap().3,
            0.5
        );
        let (h, _, _, _) =
            shade(HeatPalette::Accent, 1.0, 1.0, 1.0, Some((0.3, 0.5, 0.5))).unwrap();
        assert_eq!(h, 0.3);
    }

    #[test]
    fn clusters_group_by_cell_and_average_positions() {
        let pts = [
            (1.0, 1.0),
            (3.0, 5.0),
            (50.0, 50.0),
            (f64::NAN, 0.0),
            (2.0, 3.0),
        ];
        let c = cluster_points(&pts, 40.0);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].members, vec![0, 1, 4]);
        assert_eq!((c[0].x, c[0].y), (2.0, 3.0));
        assert_eq!(c[1].members, vec![2]);
    }

    #[test]
    fn ramp_runs_blue_to_red_and_fades_in() {
        let (h0, _, _, a0) = ramp(0.05);
        let (h1, _, _, a1) = ramp(1.0);
        assert!(h0 > 0.5, "{h0}");
        assert_eq!(h1, 0.0);
        assert!(a1 > a0);
    }
}
