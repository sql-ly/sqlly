//! Turning result sets and geometry values into plottable map features:
//! longitude/latitude column detection, geometry column detection, row
//! labels, and the feature cap.

use sqlly_core::spatial::{self, BoundingBox, Coord, Geometry, Shape};
use sqlly_core::{CellValue, ResultColumn};

/// Most features one map draws; the rest are counted and reported as
/// "showing N of M".
pub const MAX_FEATURES: usize = 50_000;
/// Non-null cells sampled when deciding whether a column holds geometry.
const GEOMETRY_SAMPLE: usize = 20;
/// Longest hover label, in characters.
const MAX_LABEL_CHARS: usize = 80;

/// Which coordinate a column name says it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordRole {
    Longitude,
    Latitude,
}

const LON_WORDS: &[&str] = &["lon", "lng", "long", "longitude"];
const LAT_WORDS: &[&str] = &["lat", "latitude"];

/// Split a column name into lower-case words at `_`, spaces, dots,
/// hyphens, and camelCase humps: `pickupLongitude` → `pickup`, `longitude`.
fn name_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for ch in name.chars() {
        if !ch.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        prev_lower = ch.is_lowercase() || ch.is_ascii_digit();
        cur.extend(ch.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

/// The coordinate a column name names, by its words (case-insensitive):
/// `lon`, `lng`, `long`, `longitude` / `lat`, `latitude`, alone or as part
/// of a longer name such as `pickup_lat` or `StartLongitude`.
pub fn coord_role(name: &str) -> Option<CoordRole> {
    let words = name_words(name);
    if words.iter().any(|w| LON_WORDS.contains(&w.as_str())) {
        Some(CoordRole::Longitude)
    } else if words.iter().any(|w| LAT_WORDS.contains(&w.as_str())) {
        Some(CoordRole::Latitude)
    } else {
        None
    }
}

/// The name with its coordinate word removed, so `pickup_lon` and
/// `pickup_lat` pair up (both → `pickup`).
fn coord_stem(name: &str) -> String {
    name_words(name)
        .into_iter()
        .filter(|w| !LON_WORDS.contains(&w.as_str()) && !LAT_WORDS.contains(&w.as_str()))
        .collect::<Vec<_>>()
        .join("_")
}

/// One way to read locations out of a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapSource {
    LonLat { lon: usize, lat: usize },
    Geometry { column: usize },
}

impl MapSource {
    /// Short label for the source picker: `lon / lat` or the column name.
    pub fn label(&self, columns: &[ResultColumn]) -> String {
        let name = |i: usize| columns.get(i).map(|c| c.name.clone()).unwrap_or_default();
        match *self {
            Self::LonLat { lon, lat } => format!("{} / {}", name(lon), name(lat)),
            Self::Geometry { column } => name(column),
        }
    }
}

/// Every plottable reading of a result, best first: longitude/latitude
/// pairs (matched by shared name stem when there are several), then
/// geometry columns. More than one entry means the user picks.
pub fn map_sources(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> Vec<MapSource> {
    let mut lons = Vec::new();
    let mut lats = Vec::new();
    for (i, c) in columns.iter().enumerate() {
        match coord_role(&c.name) {
            Some(CoordRole::Longitude) => lons.push(i),
            Some(CoordRole::Latitude) => lats.push(i),
            None => {}
        }
    }
    let mut out = Vec::new();
    if lons.len() == 1 && lats.len() == 1 {
        out.push(MapSource::LonLat {
            lon: lons[0],
            lat: lats[0],
        });
    } else {
        // Pair columns that share a stem (`pickup_lon` + `pickup_lat`);
        // when nothing pairs that way, offer every combination.
        for &lon in &lons {
            let stem = coord_stem(&columns[lon].name);
            for &lat in &lats {
                if coord_stem(&columns[lat].name) == stem {
                    out.push(MapSource::LonLat { lon, lat });
                }
            }
        }
        if out.is_empty() {
            for &lon in &lons {
                for &lat in &lats {
                    out.push(MapSource::LonLat { lon, lat });
                }
            }
        }
    }
    out.extend(
        geometry_columns(columns, rows)
            .into_iter()
            .map(|column| MapSource::Geometry { column }),
    );
    out
}

/// Columns whose declared type is spatial, or whose first non-null cells
/// all parse as well-known text or binary geometry.
pub fn geometry_columns(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> Vec<usize> {
    let mut out = Vec::new();
    for (i, c) in columns.iter().enumerate() {
        if coord_role(&c.name).is_some() {
            continue;
        }
        let ty = c.data_type.to_ascii_lowercase();
        let typed = [
            "geometry",
            "geography",
            "point",
            "polygon",
            "linestring",
            "geoshape",
        ]
        .iter()
        .any(|t| ty.contains(t));
        let mut seen = 0usize;
        let mut parsed = 0usize;
        for row in rows {
            let Some(cell) = row.get(i) else { continue };
            if matches!(cell, CellValue::Null) {
                continue;
            }
            seen += 1;
            if parse_geometry_cell(cell).is_some() {
                parsed += 1;
            }
            if seen >= GEOMETRY_SAMPLE {
                break;
            }
        }
        if (typed && parsed > 0) || (seen > 0 && parsed == seen) {
            out.push(i);
        }
    }
    out
}

/// Geometry in one cell: WKT, hex WKB, or binary WKB (MySQL layout too).
pub fn parse_geometry_cell(cell: &CellValue) -> Option<Geometry> {
    match cell {
        CellValue::String(text) => spatial::parse_geometry_text(text),
        CellValue::Bytes(bytes) => spatial::parse_geometry_bytes(bytes),
        _ => None,
    }
}

/// A numeric cell as `f64`; text cells that hold a number count too.
fn cell_number(cell: &CellValue) -> Option<f64> {
    let v = match cell {
        CellValue::F64(v) => *v,
        CellValue::I64(v) => *v as f64,
        CellValue::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    v.is_finite().then_some(v)
}

/// The column whose value labels a feature on hover: a Redis `member`, a
/// `key`, `name`, `label`, `title`, or `id`, else the first text column
/// that isn't itself a coordinate or geometry.
pub fn label_column(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    source: MapSource,
) -> Option<usize> {
    const PREFERRED: &[&str] = &["member", "key", "name", "label", "title", "id"];
    let used = |i: usize| match source {
        MapSource::LonLat { lon, lat } => i == lon || i == lat,
        MapSource::Geometry { column } => i == column,
    };
    for want in PREFERRED {
        if let Some(i) = columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(want))
            .filter(|&i| !used(i))
        {
            return Some(i);
        }
    }
    let first_row = rows.first();
    (0..columns.len()).find(|&i| {
        !used(i)
            && coord_role(&columns[i].name).is_none()
            && first_row.is_some_and(|r| matches!(r.get(i), Some(CellValue::String(_))))
    })
}

fn cell_label(cell: Option<&CellValue>) -> String {
    let text = match cell {
        Some(CellValue::String(s)) => s.clone(),
        Some(CellValue::I64(v)) => v.to_string(),
        Some(CellValue::F64(v)) => v.to_string(),
        Some(CellValue::Bool(v)) => v.to_string(),
        _ => return String::new(),
    };
    if text.chars().count() > MAX_LABEL_CHARS {
        let cut: String = text.chars().take(MAX_LABEL_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        text
    }
}

/// How a feature's color was chosen by a `color=` column.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum ColorKey {
    /// The theme accent (no color column, or a NULL value).
    #[default]
    Default,
    /// A text category: its index in [`MapData::categories`].
    Category(usize),
    /// A number, normalized to `0..=1` between the column's min and max.
    Scale(f32),
}

/// One drawable shape and the result row it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct MapFeature {
    pub shape: Shape,
    /// Result row (`None` for a single value opened from the viewer).
    pub row: Option<usize>,
    pub label: String,
    pub color: ColorKey,
    /// Marker size from a `size=` column, normalized to `0..=1`.
    pub size: Option<f32>,
    /// Heatmap weight from a `weight=` column (1 without one).
    pub weight: f32,
}

/// Distinct text values a `color=` column can name before the rest share
/// one "other" color.
pub const MAX_CATEGORIES: usize = 12;

/// Everything the map draws, plus the counts behind its notes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapData {
    pub features: Vec<MapFeature>,
    /// Features found before the cap (the "M" in "showing N of M").
    pub total_features: usize,
    /// Rows with no usable location (NULL, unparsable, out of range).
    pub skipped_rows: usize,
    /// A non-WGS 84 SRID that kept the basemap off, when one was seen.
    pub foreign_srid: Option<u32>,
    /// False when any geometry isn't longitude/latitude: the map is then
    /// drawn as a plain sketch without tiles.
    pub geographic: bool,
    /// Text categories of the `color=` column, in first-seen order (the
    /// last one is "other" when there were more than [`MAX_CATEGORIES`]).
    pub categories: Vec<String>,
    /// Min and max of a numeric `color=` column.
    pub color_range: Option<(f64, f64)>,
}

/// Which label column to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LabelChoice {
    /// `member`, `key`, `name`, … or the first text column.
    #[default]
    Auto,
    Column(usize),
}

/// How rows become features beyond the location itself.
#[derive(Debug, Clone, PartialEq)]
pub struct RowOptions {
    pub label: LabelChoice,
    pub color: Option<usize>,
    pub size: Option<usize>,
    pub weight: Option<usize>,
    /// Declared coordinate system (overrides an SRID in the geometry).
    pub srid: Option<u32>,
    /// Feature cap (already clamped to [`MAX_FEATURES`]).
    pub limit: usize,
}

impl Default for RowOptions {
    fn default() -> Self {
        Self {
            label: LabelChoice::Auto,
            color: None,
            size: None,
            weight: None,
            srid: None,
            limit: MAX_FEATURES,
        }
    }
}

struct Style {
    color: ColorKey,
    size: Option<f32>,
    weight: f32,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            color: ColorKey::Default,
            size: None,
            weight: 1.0,
        }
    }
}

fn numeric_range(rows: &[Vec<CellValue>], col: usize) -> Option<(f64, f64)> {
    let mut range: Option<(f64, f64)> = None;
    for v in rows.iter().filter_map(|r| r.get(col).and_then(cell_number)) {
        let r = range.get_or_insert((v, v));
        r.0 = r.0.min(v);
        r.1 = r.1.max(v);
    }
    range
}

fn normalize(v: f64, (min, max): (f64, f64)) -> f32 {
    if max > min {
        ((v - min) / (max - min)).clamp(0.0, 1.0) as f32
    } else {
        0.5
    }
}

/// Whether a column holds numbers (its first non-NULL cell is numeric).
fn is_numeric_column(rows: &[Vec<CellValue>], col: usize) -> bool {
    rows.iter()
        .filter_map(|r| r.get(col))
        .find(|c| !matches!(c, CellValue::Null))
        .is_some_and(|c| matches!(c, CellValue::I64(_) | CellValue::F64(_)))
}

impl MapData {
    fn new() -> Self {
        Self {
            geographic: true,
            ..Self::default()
        }
    }

    /// Whether the cap dropped features.
    pub fn is_capped(&self) -> bool {
        self.total_features > self.features.len()
    }

    fn push(&mut self, shape: Shape, row: Option<usize>, label: &str, style: &Style, limit: usize) {
        self.total_features += 1;
        if self.features.len() < limit {
            self.features.push(MapFeature {
                shape,
                row,
                label: label.to_string(),
                color: style.color,
                size: style.size,
                weight: style.weight,
            });
        }
    }

    fn push_geometry(
        &mut self,
        geometry: Geometry,
        row: Option<usize>,
        label: &str,
        style: &Style,
        limit: usize,
    ) {
        if !geometry.is_lon_lat() {
            self.geographic = false;
            if let Some(srid) = geometry
                .srid
                .filter(|&s| s != spatial::WGS84_SRID && s != 0)
            {
                self.foreign_srid.get_or_insert(srid);
            }
        }
        for shape in geometry.shapes {
            self.push(shape, row, label, style, limit);
        }
    }

    /// A single geometry value (the value viewer's Map mode).
    pub fn from_geometry(geometry: &Geometry, label: &str) -> Self {
        let mut data = Self::new();
        data.push_geometry(
            geometry.clone(),
            None,
            label,
            &Style::default(),
            MAX_FEATURES,
        );
        data
    }

    /// Read every row through `source` with default options.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn from_rows(columns: &[ResultColumn], rows: &[Vec<CellValue>], source: MapSource) -> Self {
        Self::from_rows_with(columns, rows, source, &RowOptions::default())
    }

    /// Read every row through `source`, styled by `options`.
    pub fn from_rows_with(
        columns: &[ResultColumn],
        rows: &[Vec<CellValue>],
        source: MapSource,
        options: &RowOptions,
    ) -> Self {
        let label_col = match options.label {
            LabelChoice::Auto => label_column(columns, rows, source),
            LabelChoice::Column(i) => Some(i),
        };
        let limit = options.limit.clamp(1, MAX_FEATURES);
        let mut data = Self::new();
        let color_numeric = options.color.filter(|&c| is_numeric_column(rows, c));
        data.color_range = color_numeric.and_then(|c| numeric_range(rows, c));
        let size_range = options.size.and_then(|c| numeric_range(rows, c));
        let projected = options
            .srid
            .is_some_and(|s| s != spatial::WGS84_SRID && s != 0);
        if projected && matches!(source, MapSource::LonLat { .. }) {
            data.geographic = false;
            data.foreign_srid = options.srid;
        }
        for (r, row) in rows.iter().enumerate() {
            let label = cell_label(label_col.and_then(|i| row.get(i)));
            let mut style = Style::default();
            if let Some(c) = options.color {
                style.color = match (row.get(c), data.color_range) {
                    (Some(cell), Some(range)) if color_numeric.is_some() => cell_number(cell)
                        .map(|v| ColorKey::Scale(normalize(v, range)))
                        .unwrap_or_default(),
                    (Some(CellValue::Null) | None, _) => ColorKey::Default,
                    (Some(cell), _) => {
                        ColorKey::Category(data.category_of(&cell_label(Some(cell))))
                    }
                };
            }
            if let (Some(c), Some(range)) = (options.size, size_range) {
                style.size = row
                    .get(c)
                    .and_then(cell_number)
                    .map(|v| normalize(v, range));
            }
            if let Some(c) = options.weight {
                style.weight = row
                    .get(c)
                    .and_then(cell_number)
                    .map(|v| v.max(0.0) as f32)
                    .unwrap_or(0.0);
            }
            match source {
                MapSource::LonLat { lon, lat } => {
                    let coord = row
                        .get(lon)
                        .and_then(cell_number)
                        .zip(row.get(lat).and_then(cell_number))
                        .filter(|(x, y)| {
                            projected
                                || ((-180.0..=180.0).contains(x) && (-90.0..=90.0).contains(y))
                        });
                    match coord {
                        Some((x, y)) => {
                            data.push(Shape::Point(Coord { x, y }), Some(r), &label, &style, limit)
                        }
                        None => data.skipped_rows += 1,
                    }
                }
                MapSource::Geometry { column } => {
                    match row.get(column).and_then(parse_geometry_cell) {
                        Some(mut g) if !g.shapes.is_empty() => {
                            if options.srid.is_some() {
                                g.srid = options.srid;
                            }
                            data.push_geometry(g, Some(r), &label, &style, limit)
                        }
                        _ => data.skipped_rows += 1,
                    }
                }
            }
        }
        data
    }

    fn category_of(&mut self, value: &str) -> usize {
        if let Some(i) = self.categories.iter().position(|c| c == value) {
            return i;
        }
        if self.categories.len() < MAX_CATEGORIES {
            self.categories.push(value.to_string());
            return self.categories.len() - 1;
        }
        // Everything past the cap shares the last ("other") slot.
        MAX_CATEGORIES
    }

    /// Bounding box of every plotted vertex.
    pub fn bbox(&self) -> Option<BoundingBox> {
        let mut b: Option<BoundingBox> = None;
        self.for_each_coord(|c| {
            if !c.x.is_finite() || !c.y.is_finite() {
                return;
            }
            let bb = b.get_or_insert(BoundingBox {
                min_x: c.x,
                min_y: c.y,
                max_x: c.x,
                max_y: c.y,
            });
            bb.min_x = bb.min_x.min(c.x);
            bb.min_y = bb.min_y.min(c.y);
            bb.max_x = bb.max_x.max(c.x);
            bb.max_y = bb.max_y.max(c.y);
        });
        b
    }

    pub fn for_each_coord(&self, mut f: impl FnMut(Coord)) {
        for feature in &self.features {
            for_each_shape_coord(&feature.shape, &mut f);
        }
    }
}

/// A column by name, ignoring case.
pub fn find_column(columns: &[ResultColumn], name: &str) -> Option<usize> {
    columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(name))
}

/// A map directive resolved against one result's columns.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Resolved {
    /// The directive's own source (`lat`/`lon` or `geom`), when it names
    /// one the result has; `None` falls back to auto-detection.
    pub source: Option<MapSource>,
    pub options: RowOptions,
    /// Extra tooltip columns that exist.
    pub tooltip: Vec<usize>,
    /// Column names the directive mentions that the result lacks.
    pub missing: Vec<String>,
}

/// Resolve `directive`'s column names against `columns` (case-insensitive).
/// A missing name is skipped and reported, never fatal.
pub fn resolve(columns: &[ResultColumn], directive: &super::directive::MapDirective) -> Resolved {
    let mut missing = Vec::new();
    let mut find = |name: &Option<String>| -> Option<usize> {
        let name = name.as_deref()?;
        let found = find_column(columns, name);
        if found.is_none()
            && !missing
                .iter()
                .any(|m: &String| m.eq_ignore_ascii_case(name))
        {
            missing.push(name.to_string());
        }
        found
    };
    let source = if directive.geom.is_some() {
        find(&directive.geom).map(|column| MapSource::Geometry { column })
    } else {
        match (find(&directive.lon), find(&directive.lat)) {
            (Some(lon), Some(lat)) => Some(MapSource::LonLat { lon, lat }),
            _ => None,
        }
    };
    let label = find(&directive.label)
        .map(LabelChoice::Column)
        .unwrap_or_default();
    let color = find(&directive.color);
    let size = find(&directive.size);
    let weight = find(&directive.weight);
    let tooltip = directive
        .tooltip
        .iter()
        .filter_map(|n| find(&Some(n.clone())))
        .collect();
    Resolved {
        source,
        options: RowOptions {
            label,
            color,
            size,
            weight,
            srid: directive.srid,
            limit: directive.feature_limit(),
        },
        tooltip,
        missing,
    }
}

pub fn for_each_shape_coord(shape: &Shape, f: &mut impl FnMut(Coord)) {
    match shape {
        Shape::Point(c) => f(*c),
        Shape::LineString(pts) => pts.iter().copied().for_each(&mut *f),
        Shape::Polygon(rings) => rings.iter().flatten().copied().for_each(&mut *f),
    }
}

/// The point a heatmap counts for a shape: the point itself, or the
/// average of a line's or polygon's (exterior) vertices.
pub fn shape_anchor(shape: &Shape) -> Option<Coord> {
    let pts: &[Coord] = match shape {
        Shape::Point(c) => return Some(*c),
        Shape::LineString(pts) => pts,
        Shape::Polygon(rings) => rings.first().map(Vec::as_slice).unwrap_or(&[]),
    };
    if pts.is_empty() {
        return None;
    }
    let n = pts.len() as f64;
    let (sx, sy) = pts.iter().fold((0.0, 0.0), |(x, y), c| (x + c.x, y + c.y));
    Some(Coord {
        x: sx / n,
        y: sy / n,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: ty.into(),
            nullable: true,
        }
    }

    fn s(v: &str) -> CellValue {
        CellValue::String(v.into())
    }

    #[test]
    fn directive_overrides_auto_detection_and_reports_missing_columns() {
        use super::super::directive::parse_body;
        let cols = vec![
            col("lat", "f"),
            col("lon", "f"),
            col("home_y", "f"),
            col("home_x", "f"),
            col("City", "text"),
            col("sales", "int"),
        ];
        let (d, problems) =
            parse_body("lat=HOME_Y; lon=home_x; label=city; tooltip=sales, nope; color=ghost");
        assert!(problems.is_empty());
        let r = resolve(&cols, &d);
        assert_eq!(r.source, Some(MapSource::LonLat { lon: 3, lat: 2 }));
        assert_eq!(r.options.label, LabelChoice::Column(4));
        assert_eq!(r.tooltip, vec![5]);
        // Reported in setting order (label, color, …, tooltip).
        assert_eq!(r.missing, vec!["ghost".to_string(), "nope".to_string()]);
        // A missing source column falls back to auto-detection.
        let (d, _) = parse_body("geom=shape");
        let r = resolve(&cols, &d);
        assert_eq!(r.source, None);
        assert_eq!(r.missing, vec!["shape".to_string()]);
    }

    #[test]
    fn color_size_weight_and_srid_style_features() {
        let cols = vec![
            col("lon", "f"),
            col("lat", "f"),
            col("kind", "text"),
            col("n", "int"),
        ];
        let rows = vec![
            vec![
                CellValue::F64(1.0),
                CellValue::F64(1.0),
                s("cafe"),
                CellValue::I64(10),
            ],
            vec![
                CellValue::F64(2.0),
                CellValue::F64(2.0),
                s("bar"),
                CellValue::I64(30),
            ],
            vec![
                CellValue::F64(3.0),
                CellValue::F64(3.0),
                s("cafe"),
                CellValue::Null,
            ],
        ];
        let source = MapSource::LonLat { lon: 0, lat: 1 };
        let opts = RowOptions {
            color: Some(2),
            size: Some(3),
            weight: Some(3),
            limit: 2,
            ..RowOptions::default()
        };
        let data = MapData::from_rows_with(&cols, &rows, source, &opts);
        assert_eq!(data.features.len(), 2);
        assert_eq!(data.total_features, 3);
        assert_eq!(data.categories, vec!["cafe".to_string(), "bar".to_string()]);
        assert_eq!(data.features[1].color, ColorKey::Category(1));
        assert_eq!(data.features[0].size, Some(0.0));
        assert_eq!(data.features[1].size, Some(1.0));
        assert_eq!(data.features[1].weight, 30.0);
        let numeric = RowOptions {
            color: Some(3),
            ..RowOptions::default()
        };
        let data = MapData::from_rows_with(&cols, &rows, source, &numeric);
        assert_eq!(data.color_range, Some((10.0, 30.0)));
        assert_eq!(data.features[1].color, ColorKey::Scale(1.0));
        assert_eq!(data.features[2].color, ColorKey::Default);
        // A projected SRID keeps out-of-range coordinates and drops the basemap.
        let rows = vec![vec![
            CellValue::F64(530000.0),
            CellValue::F64(180000.0),
            s("x"),
            CellValue::I64(1),
        ]];
        let projected = RowOptions {
            srid: Some(27700),
            ..RowOptions::default()
        };
        let data = MapData::from_rows_with(&cols, &rows, source, &projected);
        assert_eq!(data.features.len(), 1);
        assert!(!data.geographic);
        assert_eq!(data.foreign_srid, Some(27700));
    }

    #[test]
    fn coord_roles_by_name_case_insensitively() {
        for name in [
            "lon",
            "LNG",
            "Long",
            "longitude",
            "pickup_lon",
            "StartLongitude",
            "geo.lng",
        ] {
            assert_eq!(coord_role(name), Some(CoordRole::Longitude), "{name}");
        }
        for name in ["lat", "LAT", "Latitude", "dropoff_lat", "endLatitude"] {
            assert_eq!(coord_role(name), Some(CoordRole::Latitude), "{name}");
        }
        for name in [
            "latency", "longest", "plate", "lone", "flat", "member", "dist",
        ] {
            assert_eq!(coord_role(name), None, "{name}");
        }
    }

    #[test]
    fn redis_geosearch_columns_map_as_one_lonlat_pair() {
        let cols = vec![
            col("member", "string"),
            col("dist", "double"),
            col("lon", "double"),
            col("lat", "double"),
        ];
        let rows = vec![vec![
            s("Palermo"),
            CellValue::F64(190.4),
            CellValue::F64(13.361389),
            CellValue::F64(38.115556),
        ]];
        let sources = map_sources(&cols, &rows);
        assert_eq!(sources, vec![MapSource::LonLat { lon: 2, lat: 3 }]);
        assert_eq!(label_column(&cols, &rows, sources[0]), Some(0));
        let data = MapData::from_rows(&cols, &rows, sources[0]);
        assert_eq!(data.features.len(), 1);
        assert_eq!(data.features[0].label, "Palermo");
        assert_eq!(data.features[0].row, Some(0));
        assert!(data.geographic);
    }

    #[test]
    fn several_coordinate_pairs_pair_by_stem_and_are_ambiguous() {
        let cols = vec![
            col("pickup_lat", "float"),
            col("pickup_lon", "float"),
            col("dropoff_lat", "float"),
            col("dropoff_lon", "float"),
        ];
        let sources = map_sources(&cols, &[]);
        assert_eq!(
            sources,
            vec![
                MapSource::LonLat { lon: 1, lat: 0 },
                MapSource::LonLat { lon: 3, lat: 2 },
            ]
        );
        assert_eq!(sources[1].label(&cols), "dropoff_lon / dropoff_lat");
    }

    #[test]
    fn unpaired_coordinate_columns_offer_every_combination() {
        // `lat` and `lng` share the empty stem, so they pair on their own.
        let cols = vec![col("lat", "f"), col("lng", "f"), col("home_longitude", "f")];
        assert_eq!(
            map_sources(&cols, &[]),
            vec![MapSource::LonLat { lon: 1, lat: 0 }]
        );
        // Nothing shares a stem: every combination is offered.
        let cols = vec![col("a_lat", "f"), col("b_lon", "f"), col("c_lon", "f")];
        assert_eq!(map_sources(&cols, &[]).len(), 2);
        // Only a longitude: nothing to plot.
        assert!(map_sources(&[col("lon", "f")], &[]).is_empty());
    }

    #[test]
    fn geometry_columns_by_content_and_by_type() {
        let cols = vec![
            col("id", "int"),
            col("shape", "nvarchar"),
            col("note", "text"),
        ];
        let rows = vec![
            vec![CellValue::I64(1), s("POINT(1 2)"), s("a")],
            vec![CellValue::I64(2), CellValue::Null, s("POLYGON")],
            vec![CellValue::I64(3), s("LINESTRING(0 0, 1 1)"), s("b")],
        ];
        assert_eq!(geometry_columns(&cols, &rows), vec![1]);
        // A spatial type with one bad cell still counts.
        let typed = vec![col("geom", "geometry")];
        let rows = vec![vec![s("POINT(1 2)")], vec![s("garbage")]];
        assert_eq!(geometry_columns(&typed, &rows), vec![0]);
        // Mixed text without a spatial type does not.
        let untyped = vec![col("g", "text")];
        assert!(geometry_columns(&untyped, &rows).is_empty());
    }

    #[test]
    fn wkt_rows_become_features_with_multi_parts_split() {
        let cols = vec![col("name", "text"), col("g", "geometry")];
        let rows = vec![
            vec![s("park"), s("POLYGON((0 0, 1 0, 1 1, 0 0))")],
            vec![s("stops"), s("MULTIPOINT((1 1), (2 2), (3 3))")],
            vec![s("route"), s("MULTILINESTRING((0 0, 1 1), (2 2, 3 3))")],
            vec![s("none"), CellValue::Null],
        ];
        let data = MapData::from_rows(&cols, &rows, MapSource::Geometry { column: 1 });
        assert_eq!(data.features.len(), 1 + 3 + 2);
        assert_eq!(data.skipped_rows, 1);
        assert!(matches!(data.features[0].shape, Shape::Polygon(_)));
        assert!(
            data.features[1..4]
                .iter()
                .all(|f| f.label == "stops" && f.row == Some(1))
        );
        assert!(matches!(data.features[5].shape, Shape::LineString(_)));
        assert!(data.geographic);
    }

    #[test]
    fn foreign_srid_turns_the_basemap_off() {
        let g = spatial::parse_wkt("SRID=27700;POINT(530000 180000)").unwrap();
        let data = MapData::from_geometry(&g, "x");
        assert!(!data.geographic);
        assert_eq!(data.foreign_srid, Some(27700));
        let wgs = spatial::parse_wkt("SRID=4326;POINT(10 20)").unwrap();
        assert!(MapData::from_geometry(&wgs, "x").geographic);
    }

    #[test]
    fn out_of_range_and_text_numbers() {
        let cols = vec![col("lon", "text"), col("lat", "text")];
        let rows = vec![
            vec![s(" -0.1278 "), s("51.5074")],
            vec![s("200"), s("10")],
            vec![CellValue::Null, s("10")],
        ];
        let data = MapData::from_rows(&cols, &rows, MapSource::LonLat { lon: 0, lat: 1 });
        assert_eq!(data.features.len(), 1);
        assert_eq!(data.skipped_rows, 2);
    }

    #[test]
    fn cap_keeps_the_first_features_and_counts_the_rest() {
        let cols = vec![col("lon", "f"), col("lat", "f")];
        let rows: Vec<Vec<CellValue>> = (0..MAX_FEATURES + 7)
            .map(|i| {
                vec![
                    CellValue::F64((i % 360) as f64 - 180.0),
                    CellValue::F64(0.0),
                ]
            })
            .collect();
        let data = MapData::from_rows(&cols, &rows, MapSource::LonLat { lon: 0, lat: 1 });
        assert_eq!(data.features.len(), MAX_FEATURES);
        assert_eq!(data.total_features, MAX_FEATURES + 7);
        assert!(data.is_capped());
    }

    #[test]
    fn label_falls_back_to_first_text_column_and_truncates() {
        let cols = vec![col("lat", "f"), col("lon", "f"), col("city", "text")];
        let long = "x".repeat(200);
        let rows = vec![vec![CellValue::F64(1.0), CellValue::F64(2.0), s(&long)]];
        let source = MapSource::LonLat { lon: 1, lat: 0 };
        assert_eq!(label_column(&cols, &rows, source), Some(2));
        let data = MapData::from_rows(&cols, &rows, source);
        assert_eq!(data.features[0].label.chars().count(), MAX_LABEL_CHARS);
    }

    #[test]
    fn anchors_and_bbox() {
        let line = Shape::LineString(vec![Coord { x: 0.0, y: 0.0 }, Coord { x: 2.0, y: 4.0 }]);
        assert_eq!(shape_anchor(&line), Some(Coord { x: 1.0, y: 2.0 }));
        let g = spatial::parse_wkt("MULTIPOINT((1 5), (-3 2))").unwrap();
        let b = MapData::from_geometry(&g, "").bbox().unwrap();
        assert_eq!((b.min_x, b.min_y, b.max_x, b.max_y), (-3.0, 2.0, 1.0, 5.0));
    }
}
