//! The render-neutral chart description (ADR-003). A notebook chart cell
//! stores a [`ChartSpec`]; the app draws the seven interactive kinds live
//! (their names match sqlly-datatable's `ChartKind`) and every kind, those
//! included, renders deterministically through [`super::svg`] for exports
//! and in-app previews.

use serde::{Deserialize, Serialize};
use sqlly_core::DisplayMessage;

use super::doc::{Status, TableColumn};
use super::format::{ColumnKind, ValueFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    #[default]
    Bar,
    Line,
    Area,
    Scatter,
    /// Scatter with a third measure as the point area.
    Bubble,
    Histogram,
    Pie,
    Donut,
    Heatmap,
    CalendarHeatmap,
    Waterfall,
    Funnel,
    Treemap,
    #[serde(rename = "box", alias = "box_plot")]
    BoxPlot,
    CohortRetention,
    CorrelationMatrix,
    Sparkline,
    Kpi,
}

impl ChartKind {
    pub const ALL: [ChartKind; 18] = [
        ChartKind::Bar,
        ChartKind::Line,
        ChartKind::Area,
        ChartKind::Scatter,
        ChartKind::Bubble,
        ChartKind::Histogram,
        ChartKind::Pie,
        ChartKind::Donut,
        ChartKind::Heatmap,
        ChartKind::CalendarHeatmap,
        ChartKind::Waterfall,
        ChartKind::Funnel,
        ChartKind::Treemap,
        ChartKind::BoxPlot,
        ChartKind::CohortRetention,
        ChartKind::CorrelationMatrix,
        ChartKind::Sparkline,
        ChartKind::Kpi,
    ];

    /// The stable name, shared with sqlly-datatable for the kinds both know.
    pub fn name(self) -> &'static str {
        match self {
            ChartKind::Bar => "bar",
            ChartKind::Line => "line",
            ChartKind::Area => "area",
            ChartKind::Scatter => "scatter",
            ChartKind::Bubble => "bubble",
            ChartKind::Histogram => "histogram",
            ChartKind::Pie => "pie",
            ChartKind::Donut => "donut",
            ChartKind::Heatmap => "heatmap",
            ChartKind::CalendarHeatmap => "calendar_heatmap",
            ChartKind::Waterfall => "waterfall",
            ChartKind::Funnel => "funnel",
            ChartKind::Treemap => "treemap",
            ChartKind::BoxPlot => "box",
            ChartKind::CohortRetention => "cohort_retention",
            ChartKind::CorrelationMatrix => "correlation_matrix",
            ChartKind::Sparkline => "sparkline",
            ChartKind::Kpi => "kpi",
        }
    }

    pub fn from_name(name: &str) -> Option<ChartKind> {
        let name = name.trim();
        if name.eq_ignore_ascii_case("box_plot") {
            return Some(ChartKind::BoxPlot);
        }
        ChartKind::ALL
            .into_iter()
            .find(|k| k.name().eq_ignore_ascii_case(name))
    }

    /// The kinds the results grid can also draw interactively.
    pub fn is_interactive_in_grid(self) -> bool {
        matches!(
            self,
            ChartKind::Bar
                | ChartKind::Line
                | ChartKind::Area
                | ChartKind::Scatter
                | ChartKind::Histogram
                | ChartKind::Pie
                | ChartKind::Donut
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    /// Rows are plotted as they are; rows sharing a category are summed.
    #[default]
    None,
    Sum,
    Avg,
    Count,
    CountDistinct,
    Min,
    Max,
    Median,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortBy {
    /// Dates and numbers ascending, text in first-seen order.
    #[default]
    Natural,
    /// By category label.
    Category,
    /// By the category's total across series.
    Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Sort {
    #[serde(default)]
    pub by: SortBy,
    #[serde(default)]
    pub descending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopN {
    pub n: usize,
    /// Fold the remaining categories into one "Other" category.
    #[serde(default)]
    pub other: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DateGrain {
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NullTreatment {
    /// A missing value breaks the line / leaves the bar out.
    #[default]
    Gap,
    Zero,
    /// Rows with a missing measure are dropped before aggregating.
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BarMode {
    #[default]
    Grouped,
    Stacked,
    /// Stacked to 100%.
    Normalized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Orientation {
    #[default]
    Vertical,
    Horizontal,
}

/// A column bound to a channel.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Encoding {
    pub column: String,
    /// Legend / axis label; the column name when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Overrides the spec-wide aggregate for this measure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<Aggregate>,
}

impl Encoding {
    pub fn new(column: impl Into<String>) -> Self {
        Self {
            column: column.into(),
            ..Default::default()
        }
    }

    pub fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.column)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AxisSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub format: ValueFormat,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceLine {
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Colors the line by status; neutral when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

/// A note pinned to a category (or date label) on the x axis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    pub x: String,
    pub text: String,
}

/// An expected range drawn behind a line, from two columns of the input
/// (the analytics module computes them, e.g. rolling mean ± k·sd). Points
/// outside the band are marked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnomalyBand {
    pub lower: String,
    pub upper: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaletteKind {
    /// The colour-blind-safe categorical ramp.
    #[default]
    Categorical,
    /// One hue, light to dark, for ordered categories.
    Sequential,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartSpec {
    pub kind: ChartKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Category, date or numeric x; the label for pie, funnel, treemap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<Encoding>,
    /// Measures. Several measures make several series.
    #[serde(default)]
    pub y: Vec<Encoding>,
    /// Splits one measure into a series per distinct value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<Encoding>,
    /// Bubble area.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Encoding>,
    /// Colors scatter points by category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Encoding>,
    #[serde(default)]
    pub aggregate: Aggregate,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n: Option<TopN>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_grain: Option<DateGrain>,
    #[serde(default)]
    pub nulls: NullTreatment,
    #[serde(default)]
    pub bar_mode: BarMode,
    #[serde(default)]
    pub orientation: Orientation,
    #[serde(default)]
    pub x_axis: AxisSpec,
    #[serde(default)]
    pub y_axis: AxisSpec,
    #[serde(default)]
    pub reference_lines: Vec<ReferenceLine>,
    #[serde(default)]
    pub annotations: Vec<Annotation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anomaly_band: Option<AnomalyBand>,
    #[serde(default)]
    pub palette: PaletteKind,
    /// Histogram bin count; chosen from the data when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bins: Option<usize>,
    /// Print values on bars, slices and cells.
    #[serde(default)]
    pub show_values: bool,
    #[serde(default = "yes")]
    pub show_legend: bool,
}

fn yes() -> bool {
    true
}

impl Default for ChartSpec {
    fn default() -> Self {
        Self {
            kind: ChartKind::Bar,
            title: None,
            subtitle: None,
            x: None,
            y: Vec::new(),
            series: None,
            size: None,
            color: None,
            aggregate: Aggregate::None,
            sort: Sort::default(),
            top_n: None,
            date_grain: None,
            nulls: NullTreatment::Gap,
            bar_mode: BarMode::Grouped,
            orientation: Orientation::Vertical,
            x_axis: AxisSpec::default(),
            y_axis: AxisSpec::default(),
            reference_lines: Vec::new(),
            annotations: Vec::new(),
            anomaly_band: None,
            palette: PaletteKind::Categorical,
            bins: None,
            show_values: false,
            show_legend: true,
        }
    }
}

impl ChartSpec {
    pub fn new(kind: ChartKind) -> Self {
        Self {
            kind,
            ..Default::default()
        }
    }

    pub fn x(mut self, column: impl Into<String>) -> Self {
        self.x = Some(Encoding::new(column));
        self
    }

    pub fn y(mut self, column: impl Into<String>) -> Self {
        self.y.push(Encoding::new(column));
        self
    }

    pub fn series(mut self, column: impl Into<String>) -> Self {
        self.series = Some(Encoding::new(column));
        self
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Every column the spec reads.
    pub fn columns(&self) -> Vec<&str> {
        let encodings = self
            .x
            .iter()
            .chain(self.y.iter())
            .chain(self.series.iter())
            .chain(self.size.iter())
            .chain(self.color.iter())
            .map(|e| e.column.as_str());
        let band = self
            .anomaly_band
            .iter()
            .flat_map(|b| [b.lower.as_str(), b.upper.as_str()]);
        let mut out: Vec<&str> = Vec::new();
        for c in encodings.chain(band) {
            if !out.contains(&c) {
                out.push(c);
            }
        }
        out
    }

    /// Checks the spec against the input columns, returning every problem.
    pub fn validate(&self, columns: &[TableColumn]) -> Result<(), Vec<ChartSpecError>> {
        let mut errors = Vec::new();
        let find = |name: &str| {
            columns
                .iter()
                .find(|c| c.name == name)
                .or_else(|| columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
        };
        for name in self.columns() {
            if find(name).is_none() {
                errors.push(ChartSpecError::UnknownColumn(name.to_string()));
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        let kind_of = |e: &Encoding| find(&e.column).map(TableColumn::kind);
        let numeric = |e: &Encoding, errors: &mut Vec<ChartSpecError>| {
            let counting = matches!(
                e.aggregate.unwrap_or(self.aggregate),
                Aggregate::Count | Aggregate::CountDistinct
            );
            if !counting && kind_of(e).is_some_and(|k| !k.is_numeric()) {
                errors.push(ChartSpecError::NotNumeric {
                    column: e.column.clone(),
                });
            }
        };
        let need_x = |errors: &mut Vec<ChartSpecError>| {
            if self.x.is_none() {
                errors.push(ChartSpecError::MissingEncoding {
                    channel: Channel::X,
                });
            }
        };
        let need_y = |count: usize, errors: &mut Vec<ChartSpecError>| {
            if self.y.len() < count {
                errors.push(ChartSpecError::MissingEncoding {
                    channel: Channel::Y,
                });
            }
        };
        let single_measure = |errors: &mut Vec<ChartSpecError>| {
            if self.y.len() > 1 {
                errors.push(ChartSpecError::TooManyMeasures {
                    kind: self.kind,
                    max: 1,
                });
            }
        };
        match self.kind {
            ChartKind::Bar | ChartKind::Line | ChartKind::Area => {
                need_x(&mut errors);
                need_y(1, &mut errors);
                for e in &self.y {
                    numeric(e, &mut errors);
                }
            }
            ChartKind::Scatter | ChartKind::Bubble => {
                need_x(&mut errors);
                need_y(1, &mut errors);
                if let Some(x) = &self.x
                    && kind_of(x).is_some_and(|k| !k.is_numeric() && !k.is_temporal())
                {
                    errors.push(ChartSpecError::NotNumeric {
                        column: x.column.clone(),
                    });
                }
                for e in &self.y {
                    numeric(e, &mut errors);
                }
                if self.kind == ChartKind::Bubble {
                    match &self.size {
                        Some(size) => numeric(size, &mut errors),
                        None => errors.push(ChartSpecError::MissingEncoding {
                            channel: Channel::Size,
                        }),
                    }
                }
            }
            ChartKind::Histogram | ChartKind::BoxPlot | ChartKind::Sparkline | ChartKind::Kpi => {
                need_y(1, &mut errors);
                single_measure(&mut errors);
                for e in &self.y {
                    numeric(e, &mut errors);
                }
            }
            ChartKind::Pie | ChartKind::Donut | ChartKind::Funnel | ChartKind::Waterfall => {
                need_x(&mut errors);
                need_y(1, &mut errors);
                single_measure(&mut errors);
                for e in &self.y {
                    numeric(e, &mut errors);
                }
                if self.series.is_some() {
                    errors.push(ChartSpecError::SeriesNotSupported { kind: self.kind });
                }
            }
            ChartKind::Treemap => {
                need_x(&mut errors);
                need_y(1, &mut errors);
                single_measure(&mut errors);
                for e in &self.y {
                    numeric(e, &mut errors);
                }
            }
            ChartKind::Heatmap | ChartKind::CohortRetention => {
                need_x(&mut errors);
                need_y(1, &mut errors);
                single_measure(&mut errors);
                for e in &self.y {
                    numeric(e, &mut errors);
                }
                if self.series.is_none() {
                    errors.push(ChartSpecError::MissingEncoding {
                        channel: Channel::Series,
                    });
                }
                if self.kind == ChartKind::CohortRetention
                    && let Some(period) = &self.series
                    && kind_of(period).is_some_and(|k| !k.is_numeric())
                {
                    errors.push(ChartSpecError::NotNumeric {
                        column: period.column.clone(),
                    });
                }
            }
            ChartKind::CalendarHeatmap => {
                need_x(&mut errors);
                need_y(1, &mut errors);
                single_measure(&mut errors);
                for e in &self.y {
                    numeric(e, &mut errors);
                }
                if let Some(x) = &self.x
                    && kind_of(x)
                        .is_some_and(|k| !matches!(k, ColumnKind::Date | ColumnKind::DateTime))
                {
                    errors.push(ChartSpecError::NotTemporal {
                        column: x.column.clone(),
                    });
                }
            }
            ChartKind::CorrelationMatrix => {
                need_y(2, &mut errors);
                for e in &self.y {
                    if kind_of(e).is_some_and(|k| !k.is_numeric()) {
                        errors.push(ChartSpecError::NotNumeric {
                            column: e.column.clone(),
                        });
                    }
                }
            }
        }
        if self.date_grain.is_some()
            && let Some(x) = &self.x
            && kind_of(x).is_some_and(|k| !k.is_temporal())
        {
            errors.push(ChartSpecError::NotTemporal {
                column: x.column.clone(),
            });
        }
        if let Some(top) = self.top_n
            && top.n == 0
        {
            errors.push(ChartSpecError::InvalidTopN);
        }
        if let Some(bins) = self.bins
            && !(1..=200).contains(&bins)
        {
            errors.push(ChartSpecError::InvalidBins);
        }
        if let (Some(min), Some(max)) = (self.y_axis.min, self.y_axis.max)
            && min.partial_cmp(&max) != Some(std::cmp::Ordering::Less)
        {
            errors.push(ChartSpecError::InvalidAxisRange);
        }
        if let Some(band) = &self.anomaly_band {
            for name in [&band.lower, &band.upper] {
                if find(name).is_some_and(|c| !c.kind().is_numeric()) {
                    errors.push(ChartSpecError::NotNumeric {
                        column: name.clone(),
                    });
                }
            }
            if !matches!(self.kind, ChartKind::Line | ChartKind::Area) {
                errors.push(ChartSpecError::BandNeedsLine);
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    X,
    Y,
    Series,
    Size,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChartSpecError {
    UnknownColumn(String),
    MissingEncoding { channel: Channel },
    NotNumeric { column: String },
    NotTemporal { column: String },
    TooManyMeasures { kind: ChartKind, max: usize },
    SeriesNotSupported { kind: ChartKind },
    InvalidTopN,
    InvalidBins,
    InvalidAxisRange,
    BandNeedsLine,
}

impl ChartSpecError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            ChartSpecError::UnknownColumn(column) => DisplayMessage::new(
                "notebook.report.chart_errors.unknown_column",
                format!("The chart uses a column the result does not have: {column}."),
            )
            .with_arg("column", column.clone()),
            ChartSpecError::MissingEncoding { channel } => match channel {
                Channel::X => DisplayMessage::new(
                    "notebook.report.chart_errors.missing_x",
                    "Choose a column for the x axis.",
                ),
                Channel::Y => DisplayMessage::new(
                    "notebook.report.chart_errors.missing_y",
                    "Choose at least one column to measure.",
                ),
                Channel::Series => DisplayMessage::new(
                    "notebook.report.chart_errors.missing_series",
                    "Choose a column to split the chart into series.",
                ),
                Channel::Size => DisplayMessage::new(
                    "notebook.report.chart_errors.missing_size",
                    "Choose a column for the bubble size.",
                ),
            },
            ChartSpecError::NotNumeric { column } => DisplayMessage::new(
                "notebook.report.chart_errors.not_numeric",
                format!("{column} is not a numeric column, so it cannot be measured."),
            )
            .with_arg("column", column.clone()),
            ChartSpecError::NotTemporal { column } => DisplayMessage::new(
                "notebook.report.chart_errors.not_temporal",
                format!("{column} is not a date or time column."),
            )
            .with_arg("column", column.clone()),
            ChartSpecError::TooManyMeasures { .. } => DisplayMessage::new(
                "notebook.report.chart_errors.too_many_measures",
                "This chart type shows a single measure; remove the extra measure columns.",
            ),
            ChartSpecError::SeriesNotSupported { .. } => DisplayMessage::new(
                "notebook.report.chart_errors.series_not_supported",
                "This chart type can't be split into series.",
            ),
            ChartSpecError::InvalidTopN => DisplayMessage::new(
                "notebook.report.chart_errors.invalid_top_n",
                "Top N must keep at least one category.",
            ),
            ChartSpecError::InvalidBins => DisplayMessage::new(
                "notebook.report.chart_errors.invalid_bins",
                "A histogram needs between 1 and 200 bins.",
            ),
            ChartSpecError::InvalidAxisRange => DisplayMessage::new(
                "notebook.report.chart_errors.invalid_axis_range",
                "The axis minimum must be below its maximum.",
            ),
            ChartSpecError::BandNeedsLine => DisplayMessage::new(
                "notebook.report.chart_errors.band_needs_line",
                "An expected-range band can only be drawn on a line or area chart.",
            ),
        }
    }
}

impl std::fmt::Display for ChartSpecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message().fallback())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<TableColumn> {
        vec![
            TableColumn::new("day", "date"),
            TableColumn::new("region", "varchar(20)"),
            TableColumn::new("revenue", "decimal(18,2)"),
            TableColumn::new("orders", "int"),
        ]
    }

    #[test]
    fn grid_kind_names_match_the_datatable() {
        for (kind, name) in [
            (ChartKind::Bar, "bar"),
            (ChartKind::Line, "line"),
            (ChartKind::Area, "area"),
            (ChartKind::Scatter, "scatter"),
            (ChartKind::Histogram, "histogram"),
            (ChartKind::Pie, "pie"),
            (ChartKind::Donut, "donut"),
        ] {
            assert_eq!(kind.name(), name);
            assert!(kind.is_interactive_in_grid());
            assert_eq!(ChartKind::from_name(name), Some(kind));
        }
        let json = serde_json::to_string(&ChartKind::CalendarHeatmap).unwrap();
        assert_eq!(json, "\"calendar_heatmap\"");
    }

    #[test]
    fn every_suggested_kind_is_a_chart_kind() {
        for name in crate::analytics::suggest::ChartKindName::ALL {
            let kind = ChartKind::from_name(name.as_str());
            assert_eq!(
                kind.map(ChartKind::name),
                Some(name.as_str()),
                "{}",
                name.as_str()
            );
        }
        assert_eq!(ChartKind::from_name("box_plot"), Some(ChartKind::BoxPlot));
        let old: ChartKind = serde_json::from_str("\"box_plot\"").unwrap();
        assert_eq!(old, ChartKind::BoxPlot);
        assert_eq!(
            serde_json::to_string(&ChartKind::BoxPlot).unwrap(),
            "\"box\""
        );
    }

    #[test]
    fn a_sound_spec_validates() {
        let spec = ChartSpec::new(ChartKind::Line)
            .x("day")
            .y("revenue")
            .series("region");
        assert_eq!(spec.validate(&cols()), Ok(()));
    }

    #[test]
    fn unknown_and_mistyped_columns_are_reported() {
        let spec = ChartSpec::new(ChartKind::Bar).x("day").y("nope");
        assert_eq!(
            spec.validate(&cols()),
            Err(vec![ChartSpecError::UnknownColumn("nope".into())])
        );
        let spec = ChartSpec::new(ChartKind::Bar).x("day").y("region");
        assert_eq!(
            spec.validate(&cols()),
            Err(vec![ChartSpecError::NotNumeric {
                column: "region".into()
            }])
        );
        let mut counted = ChartSpec::new(ChartKind::Bar).x("day").y("region");
        counted.aggregate = Aggregate::Count;
        assert_eq!(counted.validate(&cols()), Ok(()));
    }

    #[test]
    fn kind_rules_are_enforced() {
        let pie = ChartSpec::new(ChartKind::Pie)
            .x("region")
            .y("revenue")
            .y("orders");
        let errors = pie.validate(&cols()).unwrap_err();
        assert!(errors.contains(&ChartSpecError::TooManyMeasures {
            kind: ChartKind::Pie,
            max: 1
        }));
        let calendar = ChartSpec::new(ChartKind::CalendarHeatmap)
            .x("region")
            .y("orders");
        assert_eq!(
            calendar.validate(&cols()),
            Err(vec![ChartSpecError::NotTemporal {
                column: "region".into()
            }])
        );
        let mut grain = ChartSpec::new(ChartKind::Bar).x("region").y("orders");
        grain.date_grain = Some(DateGrain::Month);
        assert!(grain.validate(&cols()).is_err());
        let corr = ChartSpec::new(ChartKind::CorrelationMatrix).y("revenue");
        assert!(corr.validate(&cols()).is_err());
    }

    #[test]
    fn errors_carry_catalog_keys() {
        let error = ChartSpecError::UnknownColumn("x".into());
        assert_eq!(
            error.message().key(),
            Some("notebook.report.chart_errors.unknown_column")
        );
        assert!(error.to_string().contains('x'));
    }

    #[test]
    fn specs_round_trip_through_json() {
        let mut spec = ChartSpec::new(ChartKind::Waterfall)
            .x("region")
            .y("revenue");
        spec.reference_lines.push(ReferenceLine {
            value: 10.0,
            label: Some("target".into()),
            status: Some(Status::Good),
        });
        let json = serde_json::to_string(&spec).unwrap();
        let back: ChartSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
        let minimal: ChartSpec = serde_json::from_str("{\"kind\":\"line\"}").unwrap();
        assert!(minimal.show_legend);
    }
}
