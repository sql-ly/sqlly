//! The `-- sqlly map [<n>]:` directive: how a result is drawn on the map
//! (which columns hold the locations, labels, colors, sizes, the heatmap,
//! clustering, the basemap, the starting view, and click behavior).
//!
//! Pure text in, [`MapDirective`] out (and back via [`directive_line`]).
//! The map window's options panel edits the same struct, and its "Write to
//! query" button writes it back with the result lock. Columns are named,
//! matched case-insensitively; a name the result lacks is skipped and noted
//! in the map window.
//!
//! Grammar of the body — `;`-separated `key=value` settings (see
//! [`KEYS`]). A name containing whitespace or any of `,;:="|` is written in
//! double quotes, with `""` for a literal quote, as in `grid:`.
//!
//! Parsing is lenient: every valid setting is kept and every problem is
//! returned as a message, so one typo never switches the rest off (the
//! editor squiggles the line with the messages).

use crate::grid_directive::quote_name;

use super::features::MAX_FEATURES;

/// Every key, in the order [`directive_line`] writes them.
#[cfg_attr(not(test), allow(dead_code))]
pub const KEYS: &[&str] = &[
    "lat",
    "lon",
    "geom",
    "srid",
    "label",
    "tooltip",
    "mode",
    "color",
    "size",
    "weight",
    "radius",
    "blur",
    "intensity",
    "opacity",
    "palette",
    "cluster",
    "basemap",
    "center",
    "zoom",
    "fit",
    "limit",
    "click",
    "open",
];

/// Points, heatmap, or both layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MapMode {
    #[default]
    Points,
    Heatmap,
    Both,
}

impl MapMode {
    pub const ALL: [MapMode; 3] = [MapMode::Points, MapMode::Heatmap, MapMode::Both];
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Points => "points",
            Self::Heatmap => "heatmap",
            Self::Both => "both",
        }
    }
    pub fn shows_points(self) -> bool {
        self != Self::Heatmap
    }
    pub fn shows_heat(self) -> bool {
        self != Self::Points
    }
}

/// Heatmap color ramps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeatPalette {
    /// Blue → green → yellow → red.
    #[default]
    Heat,
    /// Dark red → orange → pale yellow.
    Fire,
    /// Purple → teal → yellow (color-blind friendly).
    Viridis,
    /// Pale cyan → deep blue.
    Ocean,
    /// The theme's accent color, faint to strong.
    Accent,
}

impl HeatPalette {
    pub const ALL: [HeatPalette; 5] = [
        HeatPalette::Heat,
        HeatPalette::Fire,
        HeatPalette::Viridis,
        HeatPalette::Ocean,
        HeatPalette::Accent,
    ];
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Heat => "heat",
            Self::Fire => "fire",
            Self::Viridis => "viridis",
            Self::Ocean => "ocean",
            Self::Accent => "accent",
        }
    }
}

/// One parsed `-- sqlly map` line. `None` means "not set": auto-detect or
/// the default.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapDirective {
    /// 1-based result set; `None` applies to every result set.
    pub index: Option<usize>,
    pub lat: Option<String>,
    pub lon: Option<String>,
    pub geom: Option<String>,
    pub srid: Option<u32>,
    pub label: Option<String>,
    pub tooltip: Vec<String>,
    pub mode: Option<MapMode>,
    pub color: Option<String>,
    pub size: Option<String>,
    pub weight: Option<String>,
    pub radius: Option<f64>,
    pub blur: Option<f64>,
    pub intensity: Option<f64>,
    pub opacity: Option<f64>,
    pub palette: Option<HeatPalette>,
    pub cluster: Option<bool>,
    /// `Some(false)` is `basemap=none`. Never overrides tile consent.
    pub basemap: Option<bool>,
    /// `(latitude, longitude)`.
    pub center: Option<(f64, f64)>,
    pub zoom: Option<f64>,
    /// `fit=data`: frame every feature (ignores center/zoom).
    pub fit: bool,
    pub limit: Option<usize>,
    /// `Some(false)` is `click=none`.
    pub click: Option<bool>,
    /// `open=auto`: open the map when the query finishes.
    pub open: Option<bool>,
}

/// Numeric bounds per key: `(min, max)`.
pub const RADIUS_RANGE: (f64, f64) = (6.0, 120.0);
pub const BLUR_RANGE: (f64, f64) = (0.0, 200.0);
pub const INTENSITY_RANGE: (f64, f64) = (0.0, 10.0);
pub const OPACITY_RANGE: (f64, f64) = (0.0, 1.0);
pub const ZOOM_RANGE: (f64, f64) = (0.0, 19.0);

impl MapDirective {
    pub fn applies_to(&self, slot: usize) -> bool {
        self.index.is_none_or(|index| index == slot + 1)
    }

    /// The effective feature cap: `limit`, clamped to the global maximum.
    pub fn feature_limit(&self) -> usize {
        self.limit.unwrap_or(MAX_FEATURES).clamp(1, MAX_FEATURES)
    }

    pub fn mode(&self) -> MapMode {
        self.mode.unwrap_or_default()
    }

    /// Fold a later line in: every setting it names replaces this one's.
    fn merge(&mut self, later: MapDirective) {
        let geom_only = later.geom.is_some() && later.lat.is_none() && later.lon.is_none();
        let lat_lon_only = later.lat.is_some() && later.geom.is_none();
        macro_rules! take {
            ($($f:ident),*) => { $( if later.$f.is_some() { self.$f = later.$f; } )* };
        }
        take!(
            lat, lon, geom, srid, label, mode, color, size, weight, radius, blur, intensity,
            opacity, palette, cluster, basemap, center, zoom, limit, click, open
        );
        if !later.tooltip.is_empty() {
            self.tooltip = later.tooltip;
        }
        if later.fit {
            self.fit = true;
            self.center = None;
            self.zoom = None;
        }
        // An explicit source in the later line replaces the other kind.
        if geom_only {
            self.lat = None;
            self.lon = None;
        }
        if lat_lon_only {
            self.geom = None;
        }
    }

    /// The settings for result `slot`: every applicable line, merged in
    /// document order. `None` when no line applies.
    pub fn for_slot(directives: &[MapDirective], slot: usize) -> Option<MapDirective> {
        let mut out: Option<MapDirective> = None;
        for d in directives.iter().filter(|d| d.applies_to(slot)) {
            match out.as_mut() {
                Some(acc) => acc.merge(d.clone()),
                None => out = Some(d.clone()),
            }
        }
        out
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        MapDirective {
            index: self.index,
            ..MapDirective::default()
        } == *self
    }
}

/// Every `-- sqlly map` line in `sql` (lock blocks included), in document
/// order, with its valid settings (problems are the editor's to show).
pub fn parse(sql: &str) -> Vec<MapDirective> {
    let sql = crate::sqlly_directive::unwrap_lock_blocks(sql);
    crate::result_layout::standalone_comments(&sql)
        .into_iter()
        .filter_map(
            |comment| match crate::sqlly_directive::classify_body(&comment.body)? {
                crate::sqlly_directive::Directive::Map { index, body } => {
                    let (mut directive, _) = parse_body(body);
                    directive.index = index;
                    Some(directive)
                }
                _ => None,
            },
        )
        .collect()
}

/// Whether the first non-empty line of `sql` is `-- sqlly view: map`.
pub fn view_is_map(sql: &str) -> bool {
    let sql = crate::sqlly_directive::unwrap_lock_blocks(sql);
    sql.lines()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| crate::sqlly_directive::classify(l.trim()))
        .is_some_and(|d| {
            matches!(d, crate::sqlly_directive::Directive::View { body }
                if body.trim().eq_ignore_ascii_case("map"))
        })
}

/// The `-- sqlly map` lines of `sql`, verbatim (trimmed), so a grid lock
/// can carry them into its rewritten block.
pub fn carry_over_lines(sql: Option<&str>) -> Vec<String> {
    let Some(sql) = sql else {
        return Vec::new();
    };
    let sql = crate::sqlly_directive::unwrap_lock_blocks(sql);
    crate::result_layout::standalone_comments(&sql)
        .into_iter()
        .filter(|c| {
            matches!(
                crate::sqlly_directive::classify_body(&c.body),
                Some(crate::sqlly_directive::Directive::Map { .. })
            )
        })
        .map(|c| format!("--{}", c.body).trim().to_string())
        .collect()
}

/// Whether `line` is a map directive for result `slot` (or for every
/// result).
pub fn is_map_line_for(line: &str, slot: usize) -> bool {
    match crate::sqlly_directive::classify(line.trim()) {
        Some(crate::sqlly_directive::Directive::Map { index, .. }) => {
            index.is_none_or(|i| i == slot + 1)
        }
        _ => false,
    }
}

/// Parse a `map:` body (everything after the colon): the valid settings,
/// and a message for each setting that was skipped.
pub fn parse_body(body: &str) -> (MapDirective, Vec<String>) {
    let mut out = MapDirective::default();
    let mut problems = Vec::new();
    let mut any = false;
    for segment in split_quoted(body, ';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some((key, value)) = segment.split_once('=') else {
            problems.push(crate::localization::format(
                "map_view.directive.not_key_value",
                &[("setting", segment)],
            ));
            continue;
        };
        any = true;
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        if let Err(problem) = apply_setting(&mut out, &key, value) {
            problems.push(problem);
        }
    }
    if !any && problems.is_empty() {
        problems.push(crate::localization::text("map_view.directive.no_settings"));
    }
    if out.lat.is_some() != out.lon.is_some() {
        problems.push(crate::localization::text(
            "map_view.directive.incomplete_pair",
        ));
    }
    (out, problems)
}

fn apply_setting(out: &mut MapDirective, key: &str, value: &str) -> Result<(), String> {
    let column = || {
        single_name(value).ok_or_else(|| {
            crate::localization::format("map_view.directive.missing_column", &[("key", key)])
        })
    };
    let number = |(min, max): (f64, f64)| -> Result<f64, String> {
        let v: f64 = value
            .parse()
            .ok()
            .filter(|v: &f64| v.is_finite())
            .ok_or_else(|| {
                crate::localization::format(
                    "map_view.directive.bad_number",
                    &[("key", key), ("value", value)],
                )
            })?;
        if v < min || v > max {
            return Err(crate::localization::format(
                "map_view.directive.out_of_range",
                &[
                    ("key", key),
                    ("value", value),
                    ("min", &min.to_string()),
                    ("max", &max.to_string()),
                ],
            ));
        }
        Ok(v)
    };
    let choice = |choices: &[&str]| -> Result<usize, String> {
        choices
            .iter()
            .position(|c| c.eq_ignore_ascii_case(value))
            .ok_or_else(|| {
                crate::localization::format(
                    "map_view.directive.bad_choice",
                    &[
                        ("key", key),
                        ("value", value),
                        ("choices", &choices.join(", ")),
                    ],
                )
            })
    };
    match key {
        "lat" => out.lat = Some(column()?),
        "lon" => out.lon = Some(column()?),
        "geom" => out.geom = Some(column()?),
        "srid" => {
            out.srid = Some(value.parse().map_err(|_| {
                crate::localization::format(
                    "map_view.directive.bad_number",
                    &[("key", key), ("value", value)],
                )
            })?)
        }
        "label" => out.label = Some(column()?),
        "tooltip" => {
            let names: Option<Vec<String>> = split_quoted(value, ',')
                .into_iter()
                .map(single_name)
                .collect();
            out.tooltip = names.ok_or_else(|| {
                crate::localization::format("map_view.directive.missing_column", &[("key", key)])
            })?;
        }
        "mode" => {
            out.mode = Some(MapMode::ALL[choice(&MapMode::ALL.map(MapMode::keyword))?]);
        }
        "color" => out.color = Some(column()?),
        "size" => out.size = Some(column()?),
        "weight" => out.weight = Some(column()?),
        "radius" => out.radius = Some(number(RADIUS_RANGE)?),
        "blur" => out.blur = Some(number(BLUR_RANGE)?),
        "intensity" => out.intensity = Some(number(INTENSITY_RANGE)?),
        "opacity" => out.opacity = Some(number(OPACITY_RANGE)?),
        "palette" => {
            out.palette =
                Some(HeatPalette::ALL[choice(&HeatPalette::ALL.map(HeatPalette::keyword))?]);
        }
        "cluster" => out.cluster = Some(choice(&["on", "off"])? == 0),
        "basemap" => out.basemap = Some(choice(&["osm", "none"])? == 0),
        "center" => {
            let parsed = value.split_once(',').and_then(|(a, b)| {
                let lat: f64 = a.trim().parse().ok()?;
                let lon: f64 = b.trim().parse().ok()?;
                ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon))
                    .then_some((lat, lon))
            });
            out.center = Some(parsed.ok_or_else(|| {
                crate::localization::format("map_view.directive.bad_center", &[("value", value)])
            })?);
        }
        "zoom" => out.zoom = Some(number(ZOOM_RANGE)?),
        "fit" => {
            choice(&["data"])?;
            out.fit = true;
        }
        "limit" => {
            let n: usize = value.parse().ok().filter(|n| *n >= 1).ok_or_else(|| {
                crate::localization::format(
                    "map_view.directive.bad_number",
                    &[("key", key), ("value", value)],
                )
            })?;
            out.limit = Some(n);
        }
        "click" => out.click = Some(choice(&["select", "none"])? == 0),
        "open" => out.open = Some(choice(&["auto", "manual"])? == 0),
        other => {
            return Err(crate::localization::format(
                "map_view.directive.unknown_key",
                &[("key", other)],
            ));
        }
    }
    Ok(())
}

/// Problems with a map body, for the editor's squiggle. Empty when clean.
pub fn diagnostics(body: &str) -> Vec<String> {
    parse_body(body).1
}

fn num(v: f64) -> String {
    // Short and exact enough for the UI's steps.
    let rounded = (v * 1000.0).round() / 1000.0;
    format!("{rounded}")
}

/// The `-- sqlly map` line that reproduces `directive` (`-- sqlly map:
/// fit=data` when it has nothing else to say, so the line still means
/// "map this result"). The index is written only when `Some`.
pub fn directive_line(d: &MapDirective) -> String {
    let q = |s: &str| quote_name(s);
    let mut parts: Vec<String> = Vec::new();
    if let Some(v) = &d.lat {
        parts.push(format!("lat={}", q(v)));
    }
    if let Some(v) = &d.lon {
        parts.push(format!("lon={}", q(v)));
    }
    if let Some(v) = &d.geom {
        parts.push(format!("geom={}", q(v)));
    }
    if let Some(v) = d.srid {
        parts.push(format!("srid={v}"));
    }
    if let Some(v) = &d.label {
        parts.push(format!("label={}", q(v)));
    }
    if !d.tooltip.is_empty() {
        let names: Vec<String> = d.tooltip.iter().map(|n| q(n)).collect();
        parts.push(format!("tooltip={}", names.join(", ")));
    }
    if let Some(v) = d.mode {
        parts.push(format!("mode={}", v.keyword()));
    }
    if let Some(v) = &d.color {
        parts.push(format!("color={}", q(v)));
    }
    if let Some(v) = &d.size {
        parts.push(format!("size={}", q(v)));
    }
    if let Some(v) = &d.weight {
        parts.push(format!("weight={}", q(v)));
    }
    for (key, v) in [
        ("radius", d.radius),
        ("blur", d.blur),
        ("intensity", d.intensity),
        ("opacity", d.opacity),
    ] {
        if let Some(v) = v {
            parts.push(format!("{key}={}", num(v)));
        }
    }
    if let Some(v) = d.palette {
        parts.push(format!("palette={}", v.keyword()));
    }
    if let Some(v) = d.cluster {
        parts.push(format!("cluster={}", if v { "on" } else { "off" }));
    }
    if let Some(v) = d.basemap {
        parts.push(format!("basemap={}", if v { "osm" } else { "none" }));
    }
    if d.fit {
        parts.push("fit=data".into());
    } else {
        if let Some((lat, lon)) = d.center {
            parts.push(format!("center={},{}", num(lat), num(lon)));
        }
        if let Some(v) = d.zoom {
            parts.push(format!("zoom={}", num(v)));
        }
    }
    if let Some(v) = d.limit {
        parts.push(format!("limit={v}"));
    }
    if let Some(v) = d.click {
        parts.push(format!("click={}", if v { "select" } else { "none" }));
    }
    if let Some(v) = d.open {
        parts.push(format!("open={}", if v { "auto" } else { "manual" }));
    }
    if parts.is_empty() {
        parts.push("fit=data".into());
    }
    let head = match d.index {
        Some(index) => format!("-- sqlly map {index}:"),
        None => "-- sqlly map:".to_string(),
    };
    format!("{head} {}", parts.join("; "))
}

/// Exactly one (possibly quoted) name; `None` when empty, unclosed, or
/// followed by more text.
fn single_name(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(body) = value.strip_prefix('"') {
        let mut name = String::new();
        let mut chars = body.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '"' {
                if chars.peek().is_some_and(|(_, n)| *n == '"') {
                    name.push('"');
                    chars.next();
                    continue;
                }
                return body[i + 1..].trim().is_empty().then_some(name);
            }
            name.push(c);
        }
        None
    } else if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn split_quoted(text: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_quotes = false;
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c == sep && !in_quotes {
            out.push(&text[start..i]);
            start = i + c.len_utf8();
        }
    }
    out.push(&text[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> MapDirective {
        MapDirective {
            index: Some(2),
            lat: Some("Pickup Lat".into()),
            lon: Some("pickup_lon".into()),
            geom: None,
            srid: Some(4326),
            label: Some("name".into()),
            tooltip: vec!["city".into(), "Total; Net".into()],
            mode: Some(MapMode::Both),
            color: Some("region".into()),
            size: Some("revenue".into()),
            weight: Some("orders".into()),
            radius: Some(30.0),
            blur: Some(12.5),
            intensity: Some(1.5),
            opacity: Some(0.6),
            palette: Some(HeatPalette::Viridis),
            cluster: Some(true),
            basemap: Some(false),
            center: Some((51.5, -0.12)),
            zoom: Some(9.0),
            fit: false,
            limit: Some(1000),
            click: Some(false),
            open: Some(true),
        }
    }

    #[test]
    fn every_key_round_trips() {
        let d = full();
        let line = directive_line(&d);
        assert!(
            line.starts_with("-- sqlly map 2: lat=\"Pickup Lat\"; lon=pickup_lon;"),
            "{line}"
        );
        let parsed = parse(&line);
        assert_eq!(parsed, vec![d.clone()]);
        // geom and fit, which exclude lat/lon and center/zoom.
        let g = MapDirective {
            geom: Some("shape".into()),
            fit: true,
            ..MapDirective::default()
        };
        let line = directive_line(&g);
        assert_eq!(line, "-- sqlly map: geom=shape; fit=data");
        assert_eq!(parse(&line), vec![g]);
        // Every documented key is written by a full directive.
        let all = format!(
            "{}\n{}",
            directive_line(&full()),
            directive_line(&MapDirective {
                geom: Some("g".into()),
                fit: true,
                ..MapDirective::default()
            })
        );
        for key in KEYS {
            assert!(all.contains(&format!("{key}=")), "{key}");
        }
    }

    #[test]
    fn index_targets_one_result_and_lines_merge_in_order() {
        let sql =
            "-- sqlly map: mode=heatmap; radius=20\n-- sqlly map 2: radius=40; color=x\nSELECT 1;";
        let all = parse(sql);
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].index, Some(2));
        let first = MapDirective::for_slot(&all, 0).unwrap();
        assert_eq!((first.radius, first.color.as_deref()), (Some(20.0), None));
        let second = MapDirective::for_slot(&all, 1).unwrap();
        assert_eq!(second.mode, Some(MapMode::Heatmap));
        assert_eq!(
            (second.radius, second.color.as_deref()),
            (Some(40.0), Some("x"))
        );
        let only_second = parse("-- sqlly map 2: mode=points");
        assert!(MapDirective::for_slot(&only_second, 0).is_none());
    }

    #[test]
    fn bad_values_are_reported_and_the_rest_kept() {
        let (d, problems) = parse_body(
            " mode=heat; radius=-3; colour=x; opacity=2; label=city; cluster=maybe; center=95,0; nonsense",
        );
        assert_eq!(d.label.as_deref(), Some("city"));
        assert_eq!(d.mode, None);
        assert_eq!(problems.len(), 7, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("colour")));
        let (_, empty) = parse_body("   ");
        assert_eq!(empty.len(), 1);
        let (_, half) = parse_body("lat=a");
        assert_eq!(half.len(), 1, "lat without lon");
        assert!(diagnostics("lat=a; lon=b; mode=points").is_empty());
    }

    #[test]
    fn limit_is_clamped_to_the_global_cap() {
        let (d, p) = parse_body("limit=9999999");
        assert!(p.is_empty());
        assert_eq!(d.feature_limit(), MAX_FEATURES);
        assert_eq!(parse_body("limit=10").0.feature_limit(), 10);
        assert_eq!(parse_body("limit=0").1.len(), 1);
    }

    #[test]
    fn view_map_and_carry_over() {
        assert!(view_is_map("-- sqlly view: map\nSELECT 1"));
        assert!(!view_is_map("SELECT 1\n-- sqlly view: map"));
        let sql = "/* sqlly lock\n-- sqlly map 2: mode=both\n*/\n-- sqlly grid: hide=x\nSELECT 1";
        assert_eq!(
            carry_over_lines(Some(sql)),
            vec!["-- sqlly map 2: mode=both"]
        );
        assert!(is_map_line_for("-- sqlly map 2: mode=both", 1));
        assert!(!is_map_line_for("-- sqlly map 2: mode=both", 0));
        assert!(is_map_line_for("-- sqlly map: fit=data", 5));
    }

    #[test]
    fn empty_directive_still_writes_a_line() {
        assert_eq!(
            directive_line(&MapDirective::default()),
            "-- sqlly map: fit=data"
        );
        assert!(MapDirective::default().is_empty());
        assert!(!full().is_empty());
    }
}
