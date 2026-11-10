//! Dependency-free WKT/EWKT and WKB/EWKB geometry parsing for cell
//! previews. Produces a flat, render-friendly model (rings of 2D points)
//! plus a summary (type, SRID, bbox, counts). Not a GIS library.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coord {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Point(Coord),
    LineString(Vec<Coord>),
    /// First ring is the exterior; the rest are holes.
    Polygon(Vec<Vec<Coord>>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryKind {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
    GeometryCollection,
}

impl GeometryKind {
    fn name(self) -> &'static str {
        match self {
            Self::Point => "POINT",
            Self::LineString => "LINESTRING",
            Self::Polygon => "POLYGON",
            Self::MultiPoint => "MULTIPOINT",
            Self::MultiLineString => "MULTILINESTRING",
            Self::MultiPolygon => "MULTIPOLYGON",
            Self::GeometryCollection => "GEOMETRYCOLLECTION",
        }
    }

    fn from_wkb_code(code: u32) -> Option<Self> {
        match code {
            1 => Some(Self::Point),
            2 => Some(Self::LineString),
            3 => Some(Self::Polygon),
            4 => Some(Self::MultiPoint),
            5 => Some(Self::MultiLineString),
            6 => Some(Self::MultiPolygon),
            7 => Some(Self::GeometryCollection),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Geometry {
    pub kind: GeometryKind,
    pub srid: Option<u32>,
    pub has_z: bool,
    pub has_m: bool,
    pub shapes: Vec<Shape>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundingBox {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpatialError {
    #[error("empty geometry text")]
    Empty,
    #[error("unknown geometry type {0}")]
    UnknownType(String),
    #[error("{0}")]
    Syntax(String),
    #[error("truncated WKB")]
    Truncated,
    #[error("bad WKB byte order {0}")]
    BadByteOrder(u8),
    #[error("unsupported WKB type {0}")]
    UnsupportedWkbType(u32),
    #[error("geometry nesting exceeds 64 levels")]
    TooDeep,
}

impl SpatialError {
    pub fn message(&self) -> crate::DisplayMessage {
        use crate::DisplayMessage;
        match self {
            Self::Empty => DisplayMessage::new("services.spatial.errors.empty", self.to_string()),
            Self::UnknownType(kind) => {
                DisplayMessage::new("services.spatial.errors.unknown_type", self.to_string())
                    .with_arg("geometry_type", kind)
            }
            Self::Syntax(detail) => DisplayMessage::raw(detail),
            Self::Truncated => {
                DisplayMessage::new("services.spatial.errors.truncated", self.to_string())
            }
            Self::BadByteOrder(order) => {
                DisplayMessage::new("services.spatial.errors.bad_byte_order", self.to_string())
                    .with_arg("byte_order", order.to_string())
            }
            Self::UnsupportedWkbType(kind) => DisplayMessage::new(
                "services.spatial.errors.unsupported_wkb_type",
                self.to_string(),
            )
            .with_arg("geometry_type", kind.to_string()),
            Self::TooDeep => {
                DisplayMessage::new("services.spatial.errors.too_deep", self.to_string())
            }
        }
    }
}

/// Maximum nesting depth of `GEOMETRYCOLLECTION` children in either the WKB
/// or WKT path; deeper inputs are rejected instead of overflowing the stack.
const MAX_NESTING: usize = 64;

/// Smallest possible encoded coordinate: two little- or big-endian `f64`s.
/// Used to cap `Vec` pre-allocation by the bytes actually remaining.
const MIN_COORD_BYTES: usize = 16;

impl Geometry {
    /// Bounding box of every vertex. `None` when the geometry is EMPTY or
    /// every coordinate is non-finite.
    pub fn bbox(&self) -> Option<BoundingBox> {
        let mut bbox: Option<BoundingBox> = None;
        self.for_each_coord(|coord| {
            if !coord.x.is_finite() || !coord.y.is_finite() {
                return;
            }
            match &mut bbox {
                None => {
                    bbox = Some(BoundingBox {
                        min_x: coord.x,
                        min_y: coord.y,
                        max_x: coord.x,
                        max_y: coord.y,
                    });
                }
                Some(b) => {
                    b.min_x = b.min_x.min(coord.x);
                    b.min_y = b.min_y.min(coord.y);
                    b.max_x = b.max_x.max(coord.x);
                    b.max_y = b.max_y.max(coord.y);
                }
            }
        });
        bbox
    }

    pub fn point_count(&self) -> usize {
        let mut n = 0usize;
        self.for_each_coord(|_| n += 1);
        n
    }

    pub fn summary_message(&self) -> crate::DisplayMessage {
        let fallback = self.summary();
        let srid = self
            .srid
            .map(|value| value.to_string())
            .unwrap_or_else(|| "—".into());
        if self.shapes.is_empty() {
            return crate::DisplayMessage::new("services.spatial.summary_empty", fallback)
                .with_arg("geometry_type", self.kind.name())
                .with_arg("srid", srid);
        }
        let rings = self.ring_count();
        let points = self.point_count();
        let (min_x, min_y, max_x, max_y) = match self.bbox() {
            Some(bbox) => (
                fmt_num(bbox.min_x),
                fmt_num(bbox.min_y),
                fmt_num(bbox.max_x),
                fmt_num(bbox.max_y),
            ),
            None => ("—".into(), "—".into(), "—".into(), "—".into()),
        };
        let key = match (rings, points) {
            (1, _) => "services.spatial.summary_one_ring",
            (2.., _) => "services.spatial.summary_many_rings",
            (_, 1) => "services.spatial.summary_one_point",
            _ => "services.spatial.summary_many_points",
        };
        let mut message = crate::DisplayMessage::new(key, fallback)
            .with_arg("geometry_type", self.kind.name())
            .with_arg("srid", srid)
            .with_arg("point_count", points.to_string())
            .with_arg("min_x", min_x)
            .with_arg("min_y", min_y)
            .with_arg("max_x", max_x)
            .with_arg("max_y", max_y);
        // Only the ring summaries name the ring count.
        if rings > 0 {
            message = message.with_arg("ring_count", rings.to_string());
        }
        message
    }

    /// English compatibility fallback; localized callers use [`Self::summary_message`].
    pub fn summary(&self) -> String {
        let mut parts = vec![self.kind.name().to_string()];
        if let Some(srid) = self.srid {
            parts.push(format!("SRID {srid}"));
        }
        if self.shapes.is_empty() {
            parts.push("EMPTY".to_string());
            return parts.join(" · ");
        }
        let rings = self.ring_count();
        if rings > 0 {
            parts.push(format!("{rings} ring{}", if rings == 1 { "" } else { "s" }));
        }
        let n = self.point_count();
        parts.push(format!("{n} point{}", if n == 1 { "" } else { "s" }));
        if let Some(b) = self.bbox() {
            parts.push(format!(
                "bbox ({}, {})…({}, {})",
                fmt_num(b.min_x),
                fmt_num(b.min_y),
                fmt_num(b.max_x),
                fmt_num(b.max_y)
            ));
        }
        parts.join(" · ")
    }

    fn ring_count(&self) -> usize {
        self.shapes
            .iter()
            .map(|s| match s {
                Shape::Polygon(rings) => rings.len(),
                _ => 0,
            })
            .sum()
    }

    fn for_each_coord(&self, mut f: impl FnMut(Coord)) {
        for shape in &self.shapes {
            match shape {
                Shape::Point(c) => f(*c),
                Shape::LineString(pts) => {
                    for c in pts {
                        f(*c);
                    }
                }
                Shape::Polygon(rings) => {
                    for ring in rings {
                        for c in ring {
                            f(*c);
                        }
                    }
                }
            }
        }
    }
}

fn fmt_num(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < (i64::MAX as f64) {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Cheap sniff: optional `SRID=n;` then a known type keyword followed by `(`
/// or ` EMPTY`.
pub fn looks_like_wkt(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    let rest = skip_srid_prefix(trimmed).0;
    let rest = rest.trim_start();
    let (word, after) = take_ident(rest);
    if word.is_empty() {
        return false;
    }
    let (kind, _, _) = match split_type_name(word) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let _ = kind;
    let after = after.trim_start();
    let after = skip_dim_token(after);
    let after = after.trim_start();
    after.starts_with('(')
        || after
            .get(..5)
            .is_some_and(|w| w.eq_ignore_ascii_case("EMPTY"))
}

pub fn parse_wkt(text: &str) -> Result<Geometry, SpatialError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(SpatialError::Empty);
    }
    let mut p = WktParser {
        src: trimmed,
        pos: 0,
        has_z: false,
        has_m: false,
        depth: 0,
    };
    let geom = p.parse_geometry(true)?;
    p.skip_ws();
    if p.pos != p.src.len() {
        return Err(SpatialError::Syntax(
            "trailing characters after geometry".into(),
        ));
    }
    Ok(geom)
}

/// ≥ 10 hex chars, first byte `00` or `01`, next 4 bytes a known type code
/// with EWKB flag bits masked.
pub fn looks_like_wkb_hex(text: &str) -> bool {
    let hex = strip_hex_prefix(text.trim());
    if hex.len() < 10 || !hex.len().is_multiple_of(2) {
        return false;
    }
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let mut bytes = [0u8; 5];
    let hex_bytes = hex.as_bytes();
    for (i, slot) in bytes.iter_mut().enumerate() {
        let pair = &hex_bytes[i * 2..i * 2 + 2];
        let Ok(text) = std::str::from_utf8(pair) else {
            return false;
        };
        let Ok(v) = u8::from_str_radix(text, 16) else {
            return false;
        };
        *slot = v;
    }
    let order = bytes[0];
    if order > 1 {
        return false;
    }
    let raw = if order == 1 {
        u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]])
    } else {
        u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]])
    };
    split_wkb_type(raw).is_ok()
}

pub fn parse_wkb_hex(text: &str) -> Result<Geometry, SpatialError> {
    let hex = strip_hex_prefix(text.trim());
    if hex.is_empty() {
        return Err(SpatialError::Empty);
    }
    if !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SpatialError::Syntax("invalid hex WKB".into()));
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let raw = hex.as_bytes();
    for i in (0..raw.len()).step_by(2) {
        let slice = std::str::from_utf8(&raw[i..i + 2])
            .map_err(|_| SpatialError::Syntax("invalid hex WKB".into()))?;
        let b = u8::from_str_radix(slice, 16)
            .map_err(|_| SpatialError::Syntax("invalid hex WKB".into()))?;
        bytes.push(b);
    }
    parse_wkb(&bytes)
}

pub fn parse_wkb(bytes: &[u8]) -> Result<Geometry, SpatialError> {
    if bytes.is_empty() {
        return Err(SpatialError::Truncated);
    }
    let mut cur = ByteCur { bytes, pos: 0 };
    parse_wkb_geom(&mut cur, 0)
}

fn strip_hex_prefix(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        rest
    } else if let Some(rest) = text
        .strip_prefix("\\x")
        .or_else(|| text.strip_prefix("\\X"))
    {
        rest
    } else {
        text
    }
}

fn skip_srid_prefix(text: &str) -> (&str, Option<u32>) {
    let t = text.trim_start();
    // `t.get(..4)` is boundary-safe: a text like `dat€…` whose 4th byte lands
    // inside a multi-byte scalar yields `None` rather than panicking the way
    // `t[..4]` would. Reachable from the results-grid right-click menu, which
    // classifies every cell value through `looks_like_wkt`.
    if t.len() < 5 || !t.get(..4).is_some_and(|w| w.eq_ignore_ascii_case("SRID")) {
        return (t, None);
    }
    let after = t[4..].trim_start();
    if !after.starts_with('=') {
        return (t, None);
    }
    let after = after[1..].trim_start();
    let mut i = 0usize;
    for (idx, c) in after.char_indices() {
        if c.is_ascii_digit() {
            i = idx + 1;
        } else {
            break;
        }
    }
    if i == 0 {
        return (t, None);
    }
    let Ok(srid) = after[..i].parse::<u32>() else {
        return (t, None);
    };
    let rest = after[i..].trim_start();
    if let Some(rest) = rest.strip_prefix(';') {
        (rest, Some(srid))
    } else {
        (t, None)
    }
}

fn take_ident(text: &str) -> (&str, &str) {
    let bytes = text.as_bytes();
    if bytes.is_empty() || !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return ("", text);
    }
    let mut i = 1;
    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
        i += 1;
    }
    (&text[..i], &text[i..])
}

fn skip_dim_token(text: &str) -> &str {
    let t = text.trim_start();
    let (word, rest) = take_ident(t);
    if word.eq_ignore_ascii_case("Z")
        || word.eq_ignore_ascii_case("M")
        || word.eq_ignore_ascii_case("ZM")
    {
        rest
    } else {
        t
    }
}

fn split_type_name(name: &str) -> Result<(GeometryKind, bool, bool), SpatialError> {
    let upper = name.to_ascii_uppercase();
    let (base, has_z, has_m) = if let Some(stem) = upper.strip_suffix("ZM") {
        (stem, true, true)
    } else if let Some(stem) = upper.strip_suffix('Z') {
        (stem, true, false)
    } else if let Some(stem) = upper.strip_suffix('M') {
        (stem, false, true)
    } else {
        (upper.as_str(), false, false)
    };
    let kind = match base {
        "POINT" => GeometryKind::Point,
        "LINESTRING" => GeometryKind::LineString,
        "POLYGON" => GeometryKind::Polygon,
        "MULTIPOINT" => GeometryKind::MultiPoint,
        "MULTILINESTRING" => GeometryKind::MultiLineString,
        "MULTIPOLYGON" => GeometryKind::MultiPolygon,
        "GEOMETRYCOLLECTION" => GeometryKind::GeometryCollection,
        _ => return Err(SpatialError::UnknownType(name.to_string())),
    };
    Ok((kind, has_z, has_m))
}

struct WktParser<'a> {
    src: &'a str,
    pos: usize,
    has_z: bool,
    has_m: bool,
    depth: usize,
}

impl<'a> WktParser<'a> {
    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn eat(&mut self, expected: char) -> bool {
        self.skip_ws();
        if self.peek() == Some(expected) {
            self.pos += expected.len_utf8();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, expected: char) -> Result<(), SpatialError> {
        if self.eat(expected) {
            Ok(())
        } else {
            Err(SpatialError::Syntax(format!(
                "expected '{expected}' at position {}",
                self.pos
            )))
        }
    }

    fn ident(&mut self) -> Option<String> {
        self.skip_ws();
        let (word, rest) = take_ident(self.rest());
        if word.is_empty() {
            return None;
        }
        self.pos += word.len();
        let _ = rest;
        Some(word.to_string())
    }

    fn starts_number(&self) -> bool {
        match self.peek() {
            Some('+') | Some('-') | Some('.') => true,
            Some(c) if c.is_ascii_digit() => true,
            _ => false,
        }
    }

    fn number(&mut self) -> Result<f64, SpatialError> {
        self.skip_ws();
        let start = self.pos;
        let bytes = self.src.as_bytes();
        if self.pos >= bytes.len() {
            return Err(SpatialError::Syntax("expected number".into()));
        }
        if bytes[self.pos] == b'+' || bytes[self.pos] == b'-' {
            self.pos += 1;
        }
        let after_sign = self.pos;
        while self.pos < bytes.len() && bytes[self.pos].is_ascii_digit() {
            self.pos += 1;
        }
        if self.pos < bytes.len() && bytes[self.pos] == b'.' {
            self.pos += 1;
            while self.pos < bytes.len() && bytes[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
        }
        if self.pos < bytes.len() && (bytes[self.pos] == b'e' || bytes[self.pos] == b'E') {
            let exp_at = self.pos;
            self.pos += 1;
            if self.pos < bytes.len() && (bytes[self.pos] == b'+' || bytes[self.pos] == b'-') {
                self.pos += 1;
            }
            let exp_digits = self.pos;
            while self.pos < bytes.len() && bytes[self.pos].is_ascii_digit() {
                self.pos += 1;
            }
            if self.pos == exp_digits {
                self.pos = exp_at;
            }
        }
        if self.pos == after_sign || self.pos == start {
            return Err(SpatialError::Syntax("expected number".into()));
        }
        self.src[start..self.pos].parse::<f64>().map_err(|_| {
            SpatialError::Syntax(format!("invalid number '{}'", &self.src[start..self.pos]))
        })
    }

    fn parse_geometry(&mut self, allow_srid: bool) -> Result<Geometry, SpatialError> {
        self.skip_ws();
        let mut srid = None;
        if allow_srid {
            let (rest, parsed_srid) = skip_srid_prefix(self.rest());
            if parsed_srid.is_some() {
                let consumed = self.src.len() - self.pos - rest.len();
                self.pos += consumed;
                srid = parsed_srid;
            }
        }
        self.skip_ws();
        let name = self
            .ident()
            .ok_or_else(|| SpatialError::Syntax("expected geometry type".into()))?;
        let (kind, mut has_z, mut has_m) = split_type_name(&name)?;
        self.skip_ws();
        if let Some(word) = {
            let (w, _) = take_ident(self.rest());
            if w.eq_ignore_ascii_case("Z")
                || w.eq_ignore_ascii_case("M")
                || w.eq_ignore_ascii_case("ZM")
            {
                Some(w.to_string())
            } else {
                None
            }
        } {
            let _ = self.ident();
            let u = word.to_ascii_uppercase();
            if u == "ZM" {
                has_z = true;
                has_m = true;
            } else if u == "Z" {
                has_z = true;
            } else if u == "M" {
                has_m = true;
            }
        }
        self.has_z |= has_z;
        self.has_m |= has_m;
        self.skip_ws();
        if self
            .rest()
            .get(..5)
            .is_some_and(|w| w.eq_ignore_ascii_case("EMPTY"))
        {
            self.pos += 5;
            return Ok(Geometry {
                kind,
                srid,
                has_z: self.has_z,
                has_m: self.has_m,
                shapes: Vec::new(),
            });
        }
        self.expect('(')?;
        let mut shapes = Vec::new();
        match kind {
            GeometryKind::Point => {
                if let Some(c) = self.parse_coord()? {
                    shapes.push(Shape::Point(c));
                }
            }
            GeometryKind::LineString => {
                let pts = self.parse_coord_list()?;
                if !pts.is_empty() {
                    shapes.push(Shape::LineString(pts));
                }
            }
            GeometryKind::Polygon => {
                let rings = self.parse_polygon_rings()?;
                if rings.iter().any(|r| !r.is_empty()) {
                    shapes.push(Shape::Polygon(rings));
                }
            }
            GeometryKind::MultiPoint => {
                shapes.extend(self.parse_multipoint()?);
            }
            GeometryKind::MultiLineString => loop {
                self.skip_ws();
                if self.peek() == Some(')') {
                    break;
                }
                self.expect('(')?;
                let pts = self.parse_coord_list()?;
                self.expect(')')?;
                if !pts.is_empty() {
                    shapes.push(Shape::LineString(pts));
                }
                self.skip_ws();
                if self.eat(',') {
                    continue;
                }
                break;
            },
            GeometryKind::MultiPolygon => loop {
                self.skip_ws();
                if self.peek() == Some(')') {
                    break;
                }
                self.expect('(')?;
                let rings = self.parse_polygon_rings()?;
                self.expect(')')?;
                if rings.iter().any(|r| !r.is_empty()) {
                    shapes.push(Shape::Polygon(rings));
                }
                self.skip_ws();
                if self.eat(',') {
                    continue;
                }
                break;
            },
            GeometryKind::GeometryCollection => loop {
                self.skip_ws();
                if self.peek() == Some(')') {
                    break;
                }
                self.depth += 1;
                if self.depth > MAX_NESTING {
                    return Err(SpatialError::TooDeep);
                }
                let child = self.parse_geometry(false)?;
                self.depth -= 1;
                self.has_z |= child.has_z;
                self.has_m |= child.has_m;
                shapes.extend(child.shapes);
                self.skip_ws();
                if self.eat(',') {
                    continue;
                }
                break;
            },
        }
        self.expect(')')?;
        Ok(Geometry {
            kind,
            srid,
            has_z: self.has_z,
            has_m: self.has_m,
            shapes,
        })
    }

    fn ords_per_coord(&self) -> usize {
        2 + usize::from(self.has_z) + usize::from(self.has_m)
    }

    fn parse_ord_group(&mut self) -> Result<Vec<f64>, SpatialError> {
        let mut ords = Vec::new();
        loop {
            self.skip_ws();
            if !self.starts_number() {
                break;
            }
            ords.push(self.number()?);
            self.skip_ws();
            if self.peek() == Some(',') || self.peek() == Some(')') {
                break;
            }
        }
        if ords.is_empty() {
            return Err(SpatialError::Syntax("expected coordinates".into()));
        }
        Ok(ords)
    }

    fn infer_or_check_dims(&mut self, n: usize) -> Result<usize, SpatialError> {
        if self.has_z || self.has_m {
            let expected = self.ords_per_coord();
            if n != expected {
                return Err(SpatialError::Syntax(format!(
                    "expected {expected} ordinates per coordinate, got {n}"
                )));
            }
            return Ok(expected);
        }
        match n {
            2 => Ok(2),
            3 => {
                self.has_z = true;
                Ok(3)
            }
            4 => {
                self.has_z = true;
                self.has_m = true;
                Ok(4)
            }
            _ => Err(SpatialError::Syntax(format!(
                "expected 2, 3 or 4 ordinates per coordinate, got {n}"
            ))),
        }
    }

    fn coord_from_ords(ords: &[f64]) -> Option<Coord> {
        if ords.len() < 2 {
            return None;
        }
        let x = ords[0];
        let y = ords[1];
        if x.is_nan() || y.is_nan() {
            return None;
        }
        Some(Coord { x, y })
    }

    fn parse_coord(&mut self) -> Result<Option<Coord>, SpatialError> {
        let ords = self.parse_ord_group()?;
        self.infer_or_check_dims(ords.len())?;
        Ok(Self::coord_from_ords(&ords))
    }

    fn parse_coord_list(&mut self) -> Result<Vec<Coord>, SpatialError> {
        let mut pts = Vec::new();
        let mut dim: Option<usize> = None;
        loop {
            self.skip_ws();
            if self.peek() == Some(')') {
                break;
            }
            let ords = self.parse_ord_group()?;
            let n = match dim {
                None => {
                    let n = self.infer_or_check_dims(ords.len())?;
                    dim = Some(n);
                    n
                }
                Some(n) => {
                    if ords.len() != n {
                        return Err(SpatialError::Syntax(format!(
                            "inconsistent coordinate dimension (expected {n}, got {})",
                            ords.len()
                        )));
                    }
                    n
                }
            };
            let _ = n;
            if let Some(c) = Self::coord_from_ords(&ords) {
                pts.push(c);
            }
            self.skip_ws();
            if self.eat(',') {
                continue;
            }
            break;
        }
        Ok(pts)
    }

    fn parse_polygon_rings(&mut self) -> Result<Vec<Vec<Coord>>, SpatialError> {
        let mut rings = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(')') {
                break;
            }
            self.expect('(')?;
            let pts = self.parse_coord_list()?;
            self.expect(')')?;
            rings.push(pts);
            self.skip_ws();
            if self.eat(',') {
                continue;
            }
            break;
        }
        Ok(rings)
    }

    fn parse_multipoint(&mut self) -> Result<Vec<Shape>, SpatialError> {
        let mut shapes = Vec::new();
        self.skip_ws();
        let nested = self.peek() == Some('(');
        if nested {
            loop {
                self.skip_ws();
                if self.peek() == Some(')') {
                    break;
                }
                self.expect('(')?;
                if let Some(c) = self.parse_coord()? {
                    shapes.push(Shape::Point(c));
                }
                self.expect(')')?;
                self.skip_ws();
                if self.eat(',') {
                    continue;
                }
                break;
            }
        } else {
            for c in self.parse_coord_list()? {
                shapes.push(Shape::Point(c));
            }
        }
        Ok(shapes)
    }
}

struct ByteCur<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl ByteCur<'_> {
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    fn u8(&mut self) -> Result<u8, SpatialError> {
        let b = *self.bytes.get(self.pos).ok_or(SpatialError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], SpatialError> {
        if self.pos + N > self.bytes.len() {
            return Err(SpatialError::Truncated);
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.bytes[self.pos..self.pos + N]);
        self.pos += N;
        Ok(out)
    }

    fn u32(&mut self, le: bool) -> Result<u32, SpatialError> {
        let b = self.read_exact::<4>()?;
        Ok(if le {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn f64(&mut self, le: bool) -> Result<f64, SpatialError> {
        let b = self.read_exact::<8>()?;
        Ok(if le {
            f64::from_le_bytes(b)
        } else {
            f64::from_be_bytes(b)
        })
    }
}

fn split_wkb_type(raw: u32) -> Result<(u32, bool, bool, bool), SpatialError> {
    let has_z_flag = raw & 0x8000_0000 != 0;
    let has_m_flag = raw & 0x4000_0000 != 0;
    let has_srid = raw & 0x2000_0000 != 0;
    let mut code = raw & !0xE000_0000;
    let mut has_z = has_z_flag;
    let mut has_m = has_m_flag;
    if (3000..4000).contains(&code) {
        has_z = true;
        has_m = true;
        code -= 3000;
    } else if (2000..3000).contains(&code) {
        has_m = true;
        code -= 2000;
    } else if (1000..2000).contains(&code) {
        has_z = true;
        code -= 1000;
    }
    let base = code & 0xFF;
    if GeometryKind::from_wkb_code(base).is_none() {
        return Err(SpatialError::UnsupportedWkbType(raw));
    }
    Ok((base, has_z, has_m, has_srid))
}

fn parse_wkb_geom(cur: &mut ByteCur<'_>, depth: usize) -> Result<Geometry, SpatialError> {
    if depth > MAX_NESTING {
        return Err(SpatialError::TooDeep);
    }
    let order = cur.u8()?;
    let le = match order {
        0 => false,
        1 => true,
        b => return Err(SpatialError::BadByteOrder(b)),
    };
    let raw_type = cur.u32(le)?;
    let (base, has_z, has_m, has_srid) = split_wkb_type(raw_type)?;
    let kind = GeometryKind::from_wkb_code(base).expect("split_wkb_type checked");
    let srid = if has_srid { Some(cur.u32(le)?) } else { None };
    let mut geom = Geometry {
        kind,
        srid,
        has_z,
        has_m,
        shapes: Vec::new(),
    };
    match kind {
        GeometryKind::Point => {
            if let Some(c) = read_wkb_coord(cur, le, has_z, has_m)? {
                geom.shapes.push(Shape::Point(c));
            }
        }
        GeometryKind::LineString => {
            let pts = read_wkb_line(cur, le, has_z, has_m)?;
            if !pts.is_empty() {
                geom.shapes.push(Shape::LineString(pts));
            }
        }
        GeometryKind::Polygon => {
            let rings = read_wkb_polygon(cur, le, has_z, has_m)?;
            if rings.iter().any(|r| !r.is_empty()) {
                geom.shapes.push(Shape::Polygon(rings));
            }
        }
        GeometryKind::MultiPoint
        | GeometryKind::MultiLineString
        | GeometryKind::MultiPolygon
        | GeometryKind::GeometryCollection => {
            let n = cur.u32(le)? as usize;
            for _ in 0..n {
                let child = parse_wkb_geom(cur, depth + 1)?;
                geom.has_z |= child.has_z;
                geom.has_m |= child.has_m;
                if geom.srid.is_none() {
                    geom.srid = child.srid;
                }
                geom.shapes.extend(child.shapes);
            }
        }
    }
    Ok(geom)
}

fn read_wkb_coord(
    cur: &mut ByteCur<'_>,
    le: bool,
    has_z: bool,
    has_m: bool,
) -> Result<Option<Coord>, SpatialError> {
    let x = cur.f64(le)?;
    let y = cur.f64(le)?;
    if has_z {
        let _z = cur.f64(le)?;
    }
    if has_m {
        let _m = cur.f64(le)?;
    }
    if x.is_nan() || y.is_nan() {
        return Ok(None);
    }
    Ok(Some(Coord { x, y }))
}

fn read_wkb_line(
    cur: &mut ByteCur<'_>,
    le: bool,
    has_z: bool,
    has_m: bool,
) -> Result<Vec<Coord>, SpatialError> {
    let n = cur.u32(le)? as usize;
    // Each point needs at least MIN_COORD_BYTES more bytes; a count larger
    // than that can never parse, so reject it before allocating.
    if n > cur.remaining() / MIN_COORD_BYTES {
        return Err(SpatialError::Truncated);
    }
    let mut pts = Vec::with_capacity(n.min(cur.remaining() / MIN_COORD_BYTES));
    for _ in 0..n {
        if let Some(c) = read_wkb_coord(cur, le, has_z, has_m)? {
            pts.push(c);
        }
    }
    Ok(pts)
}

fn read_wkb_polygon(
    cur: &mut ByteCur<'_>,
    le: bool,
    has_z: bool,
    has_m: bool,
) -> Result<Vec<Vec<Coord>>, SpatialError> {
    let n = cur.u32(le)? as usize;
    // Each ring needs at least a u32 point count; a ring count larger than
    // that can never parse, so reject it before allocating.
    if n > cur.remaining() / 4 {
        return Err(SpatialError::Truncated);
    }
    let mut rings = Vec::with_capacity(n.min(cur.remaining() / MIN_COORD_BYTES));
    for _ in 0..n {
        rings.push(read_wkb_line(cur, le, has_z, has_m)?);
    }
    Ok(rings)
}

/// The SRID a longitude/latitude basemap expects (WGS 84).
pub const WGS84_SRID: u32 = 4326;

impl Geometry {
    /// True when the geometry can sit on a longitude/latitude basemap: its
    /// SRID is WGS 84 or unstated, and every vertex lies inside the
    /// longitude/latitude ranges. Projected data (a UTM grid, a local
    /// survey) answers false and is drawn without tiles.
    pub fn is_lon_lat(&self) -> bool {
        if self
            .srid
            .is_some_and(|srid| srid != WGS84_SRID && srid != 0)
        {
            return false;
        }
        self.bbox().is_none_or(|b| {
            b.min_x >= -180.0 && b.max_x <= 180.0 && b.min_y >= -90.0 && b.max_y <= 90.0
        })
    }
}

/// Parse a text cell as geometry: well-known text (with an optional
/// `SRID=n;` prefix) or hex-encoded well-known binary. `None` when the text
/// is neither.
pub fn parse_geometry_text(text: &str) -> Option<Geometry> {
    if looks_like_wkt(text) {
        return parse_wkt(text).ok();
    }
    if looks_like_wkb_hex(text) {
        return parse_wkb_hex(text).ok();
    }
    None
}

/// Parse a binary cell as geometry: plain or extended well-known binary, or
/// MySQL's internal layout (a 4-byte little-endian SRID ahead of the WKB).
pub fn parse_geometry_bytes(bytes: &[u8]) -> Option<Geometry> {
    if let Ok(geometry) = parse_wkb(bytes) {
        return Some(geometry);
    }
    let (srid, rest) = bytes.split_first_chunk::<4>()?;
    let mut geometry = parse_wkb(rest).ok()?;
    let srid = u32::from_le_bytes(*srid);
    if geometry.srid.is_none() && srid != 0 {
        geometry.srid = Some(srid);
    }
    Some(geometry)
}

impl fmt::Display for GeometryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lon_lat_detection_follows_srid_and_ranges() {
        assert!(parse_wkt("POINT(-122.4 37.8)").unwrap().is_lon_lat());
        assert!(parse_wkt("SRID=4326;POINT(10 20)").unwrap().is_lon_lat());
        assert!(!parse_wkt("SRID=27700;POINT(10 20)").unwrap().is_lon_lat());
        assert!(!parse_wkt("POINT(530000 180000)").unwrap().is_lon_lat());
        assert!(parse_wkt("POINT EMPTY").unwrap().is_lon_lat());
    }

    #[test]
    fn geometry_text_accepts_wkt_and_hex_wkb_only() {
        assert!(parse_geometry_text("LINESTRING(0 0, 1 1)").is_some());
        assert!(parse_geometry_text("0101000000000000000000F03F0000000000000040").is_some());
        assert!(parse_geometry_text("hello").is_none());
        assert!(parse_geometry_text("POINT(1").is_none());
    }

    #[test]
    fn geometry_bytes_accept_mysql_srid_prefix() {
        let mut wkb = vec![1u8, 1, 0, 0, 0];
        wkb.extend_from_slice(&1.5f64.to_le_bytes());
        wkb.extend_from_slice(&2.5f64.to_le_bytes());
        let plain = parse_geometry_bytes(&wkb).unwrap();
        assert_eq!(plain.shapes, vec![Shape::Point(pt(1.5, 2.5))]);
        let mut mysql = 4326u32.to_le_bytes().to_vec();
        mysql.extend_from_slice(&wkb);
        let prefixed = parse_geometry_bytes(&mysql).unwrap();
        assert_eq!(prefixed.srid, Some(4326));
        assert_eq!(prefixed.shapes, plain.shapes);
        assert!(parse_geometry_bytes(&[9, 9]).is_none());
    }

    fn pt(x: f64, y: f64) -> Coord {
        Coord { x, y }
    }

    #[test]
    fn parses_point_and_ewkt_srid() {
        let g = parse_wkt("SRID=4326;POINT(1 2)").unwrap();
        assert_eq!(g.srid, Some(4326));
        assert_eq!(g.kind, GeometryKind::Point);
        assert_eq!(g.shapes, vec![Shape::Point(pt(1.0, 2.0))]);
        assert!(!g.has_z && !g.has_m);
    }

    #[test]
    fn parses_polygon_with_hole_and_bbox() {
        let g = parse_wkt(
            "POLYGON((-1 -1, 1 -1, 1 1, -1 1, -1 -1),(-0.5 -0.5, 0.5 -0.5, 0.5 0.5, -0.5 0.5, -0.5 -0.5))",
        )
        .unwrap();
        assert_eq!(g.kind, GeometryKind::Polygon);
        match &g.shapes[..] {
            [Shape::Polygon(rings)] => {
                assert_eq!(rings.len(), 2);
                assert_eq!(rings[0].len(), 5);
                assert_eq!(rings[1].len(), 5);
            }
            other => panic!("expected one polygon, got {other:?}"),
        }
        let b = g.bbox().expect("bbox");
        assert_eq!(b.min_x, -1.0);
        assert_eq!(b.min_y, -1.0);
        assert_eq!(b.max_x, 1.0);
        assert_eq!(b.max_y, 1.0);
    }

    #[test]
    fn parses_multipolygon_and_collection() {
        let multi =
            parse_wkt("MULTIPOLYGON(((0 0, 1 0, 1 1, 0 1, 0 0)),((2 2, 3 2, 3 3, 2 3, 2 2)))")
                .unwrap();
        assert_eq!(multi.kind, GeometryKind::MultiPolygon);
        assert_eq!(multi.shapes.len(), 2);

        let col = parse_wkt("GEOMETRYCOLLECTION(POINT(1 2), LINESTRING(0 0, 1 1))").unwrap();
        assert_eq!(col.kind, GeometryKind::GeometryCollection);
        assert_eq!(col.shapes.len(), 2);
        assert!(matches!(col.shapes[0], Shape::Point(_)));
        assert!(matches!(col.shapes[1], Shape::LineString(_)));
    }

    #[test]
    fn parses_z_and_m_suffixes_and_inferred_dims() {
        let z = parse_wkt("POINT Z (1 2 3)").unwrap();
        assert!(z.has_z && !z.has_m);
        assert_eq!(z.shapes, vec![Shape::Point(pt(1.0, 2.0))]);

        let zm = parse_wkt("POINT(1 2 3 4)").unwrap();
        assert!(zm.has_z && zm.has_m);
        assert_eq!(zm.shapes, vec![Shape::Point(pt(1.0, 2.0))]);
    }

    #[test]
    fn empty_geometry_has_no_bbox() {
        let g = parse_wkt("POINT EMPTY").unwrap();
        assert!(g.shapes.is_empty());
        assert!(g.bbox().is_none());
        assert_eq!(g.point_count(), 0);
    }

    #[test]
    fn rejects_unknown_type_and_unbalanced_parens() {
        match parse_wkt("FOOBAR(1 2)") {
            Err(SpatialError::UnknownType(name)) => assert_eq!(name.to_ascii_uppercase(), "FOOBAR"),
            other => panic!("expected UnknownType, got {other:?}"),
        }
        assert!(matches!(
            parse_wkt("POINT(1 2"),
            Err(SpatialError::Syntax(_))
        ));
        assert!(matches!(
            parse_wkt("POLYGON((0 0, 1 0, 1 1, 0 1, 0 0)"),
            Err(SpatialError::Syntax(_))
        ));
    }

    #[test]
    fn wkb_little_endian_point() {
        let mut bytes = vec![1u8, 1, 0, 0, 0];
        bytes.extend_from_slice(&1.0f64.to_le_bytes());
        bytes.extend_from_slice(&2.0f64.to_le_bytes());
        let g = parse_wkb(&bytes).unwrap();
        assert_eq!(g.kind, GeometryKind::Point);
        assert_eq!(g.shapes, vec![Shape::Point(pt(1.0, 2.0))]);
        assert!(g.srid.is_none());
    }

    #[test]
    fn wkb_big_endian_linestring() {
        let mut bytes = vec![0u8, 0, 0, 0, 2, 0, 0, 0, 2];
        bytes.extend_from_slice(&0.0f64.to_be_bytes());
        bytes.extend_from_slice(&0.0f64.to_be_bytes());
        bytes.extend_from_slice(&1.0f64.to_be_bytes());
        bytes.extend_from_slice(&1.0f64.to_be_bytes());
        let g = parse_wkb(&bytes).unwrap();
        assert_eq!(g.kind, GeometryKind::LineString);
        assert_eq!(
            g.shapes,
            vec![Shape::LineString(vec![pt(0.0, 0.0), pt(1.0, 1.0)])]
        );
    }

    #[test]
    fn ewkb_srid_flag_point_4326() {
        let g = parse_wkb_hex("0101000020E6100000000000000000F03F0000000000000040").unwrap();
        assert_eq!(g.srid, Some(4326));
        assert_eq!(g.kind, GeometryKind::Point);
        assert_eq!(g.shapes, vec![Shape::Point(pt(1.0, 2.0))]);
    }

    #[test]
    fn iso_wkb_z_type_code_1001() {
        let mut bytes = vec![1u8, 0xE9, 0x03, 0, 0];
        bytes.extend_from_slice(&1.0f64.to_le_bytes());
        bytes.extend_from_slice(&2.0f64.to_le_bytes());
        bytes.extend_from_slice(&3.0f64.to_le_bytes());
        let g = parse_wkb(&bytes).unwrap();
        assert_eq!(g.kind, GeometryKind::Point);
        assert!(g.has_z && !g.has_m);
        assert_eq!(g.shapes, vec![Shape::Point(pt(1.0, 2.0))]);
    }

    #[test]
    fn wkb_truncated_is_error() {
        assert_eq!(parse_wkb(&[1, 1, 0, 0]), Err(SpatialError::Truncated));
        assert_eq!(parse_wkb(&[]), Err(SpatialError::Truncated));
    }

    #[test]
    fn wkb_absurd_point_count_is_error() {
        // Little-endian LINESTRING header, point count u32::MAX, no payload.
        let mut bytes = vec![0x01, 0x02, 0x00, 0x00, 0x00];
        bytes.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(parse_wkb(&bytes), Err(SpatialError::Truncated));
    }

    #[test]
    fn wkb_absurd_ring_count_is_error() {
        // Little-endian POLYGON header, ring count u32::MAX, no payload.
        let mut bytes = vec![0x01, 0x03, 0x00, 0x00, 0x00];
        bytes.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(parse_wkb(&bytes), Err(SpatialError::Truncated));
    }

    #[test]
    fn wkb_deep_collection_is_too_deep() {
        // 100 nested little-endian GEOMETRYCOLLECTION headers, each with one
        // child.
        let mut bytes = Vec::new();
        for _ in 0..100 {
            bytes.extend_from_slice(&[0x01, 0x07, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00]);
        }
        assert_eq!(parse_wkb(&bytes), Err(SpatialError::TooDeep));
    }

    #[test]
    fn wkt_deep_collection_is_too_deep() {
        let text = "GEOMETRYCOLLECTION(".repeat(100);
        assert_eq!(parse_wkt(&text), Err(SpatialError::TooDeep));
    }

    #[test]
    fn wkb_hex_accepts_0x_and_backslash_x_prefixes() {
        let hex = "0101000000000000000000F03F0000000000000040";
        let a = parse_wkb_hex(&format!("0x{hex}")).unwrap();
        let b = parse_wkb_hex(&format!("\\x{hex}")).unwrap();
        assert_eq!(a.shapes, b.shapes);
        assert_eq!(a.shapes, vec![Shape::Point(pt(1.0, 2.0))]);
    }

    #[test]
    fn looks_like_sniffers_reject_plain_text_and_numbers() {
        assert!(!looks_like_wkt("hello"));
        assert!(!looks_like_wkt("12345"));
        assert!(!looks_like_wkt("{\"a\":1}"));
        assert!(looks_like_wkt("POINT(1 2)"));
        assert!(looks_like_wkt("SRID=4326;LINESTRING EMPTY"));
        assert!(!looks_like_wkb_hex("hello"));
        assert!(!looks_like_wkb_hex("1234567890"));
        assert!(looks_like_wkb_hex(
            "0101000020E6100000000000000000F03F0000000000000040"
        ));
    }

    #[test]
    fn summary_mentions_type_srid_and_counts() {
        let g = parse_wkt("SRID=4326;POLYGON((-1 -1, 1 -1, 1 1, -1 1, -1 -1))").unwrap();
        let s = g.summary();
        assert!(s.contains("POLYGON"), "{s}");
        assert!(s.contains("SRID 4326"), "{s}");
        assert!(s.contains("1 ring"), "{s}");
        assert!(s.contains("5 point"), "{s}");
        assert!(s.contains("bbox"), "{s}");
    }

    #[test]
    fn large_geometry_point_count_is_exact() {
        let mut wkt = String::from("LINESTRING(");
        for i in 0..1000 {
            if i > 0 {
                wkt.push_str(", ");
            }
            wkt.push_str(&format!("{i} {i}"));
        }
        wkt.push(')');
        let g = parse_wkt(&wkt).unwrap();
        assert_eq!(g.point_count(), 1000);
    }
}
