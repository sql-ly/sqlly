//! Rule-based chart suggestions from a result profile: which chart kinds
//! fit the columns' roles (time, measure, category), ranked, with
//! encodings by column name, a time grain, top-N for high cardinality, and
//! warnings when the data is incomplete.
//!
//! Kind names match the report `ChartSpec` kinds. Semantics shared with the
//! results pane's model-backed suggestion (`chart_suggest` in the app): a
//! label/category axis plus at most [`MAX_SERIES`] numeric measures, ID-like
//! numeric columns are never plotted as measures, and no values leave the
//! profile.

use serde::{Deserialize, Serialize};

use super::profile::{ColumnProfile, Monotonicity, ResultProfile, SemanticType};
use super::time::TimeGrain;

/// At most this many value columns are suggested, keeping a first chart
/// readable.
pub const MAX_SERIES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartKindName {
    Bar,
    Line,
    Area,
    Scatter,
    Histogram,
    Pie,
    Donut,
    Heatmap,
    CalendarHeatmap,
    Waterfall,
    Funnel,
    Treemap,
    Box,
    CohortRetention,
    CorrelationMatrix,
    Sparkline,
}

impl ChartKindName {
    pub const ALL: [ChartKindName; 16] = [
        ChartKindName::Bar,
        ChartKindName::Line,
        ChartKindName::Area,
        ChartKindName::Scatter,
        ChartKindName::Histogram,
        ChartKindName::Pie,
        ChartKindName::Donut,
        ChartKindName::Heatmap,
        ChartKindName::CalendarHeatmap,
        ChartKindName::Waterfall,
        ChartKindName::Funnel,
        ChartKindName::Treemap,
        ChartKindName::Box,
        ChartKindName::CohortRetention,
        ChartKindName::CorrelationMatrix,
        ChartKindName::Sparkline,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ChartKindName::Bar => "bar",
            ChartKindName::Line => "line",
            ChartKindName::Area => "area",
            ChartKindName::Scatter => "scatter",
            ChartKindName::Histogram => "histogram",
            ChartKindName::Pie => "pie",
            ChartKindName::Donut => "donut",
            ChartKindName::Heatmap => "heatmap",
            ChartKindName::CalendarHeatmap => "calendar_heatmap",
            ChartKindName::Waterfall => "waterfall",
            ChartKindName::Funnel => "funnel",
            ChartKindName::Treemap => "treemap",
            ChartKindName::Box => "box",
            ChartKindName::CohortRetention => "cohort_retention",
            ChartKindName::CorrelationMatrix => "correlation_matrix",
            ChartKindName::Sparkline => "sparkline",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

/// Column names per visual channel.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Encodings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub x: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub y: Vec<String>,
    /// Splits into series / colors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<String>,
    /// The single measure of pie, treemap, heatmap and funnel charts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// The category of pie, treemap and funnel charts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionReason {
    TimeSeries,
    CategoryMeasure,
    TwoMeasures,
    Distribution,
    Share,
    Matrix,
    OrderedStages,
    PositiveAndNegative,
    DailyDensity,
    Correlation,
    Cohort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SuggestionWarning {
    /// The profile covers incomplete data; the chart would too.
    IncompleteData,
    /// Too many categories to show; only the top N are suggested.
    TopN { distinct: u64, top_n: usize },
    /// Figures behind the suggestion are estimates.
    ApproximateProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortHint {
    /// By the measure, largest first.
    ValueDescending,
    /// By the x axis (time or stage order).
    XAscending,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChartSuggestion {
    pub kind: ChartKindName,
    /// 0..=1, higher first.
    pub score: f64,
    pub reason: SuggestionReason,
    pub encodings: Encodings,
    pub time_grain: Option<TimeGrain>,
    pub top_n: Option<usize>,
    pub sort: Option<SortHint>,
    pub warnings: Vec<SuggestionWarning>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SuggestOptions {
    pub max_suggestions: usize,
    pub max_series: usize,
    /// Above this many categories a bar chart is limited to the top N.
    pub max_bar_categories: usize,
    /// Pie and donut only up to this many slices.
    pub max_pie_slices: u64,
    /// A category column may split series up to this many values.
    pub max_series_categories: u64,
}

impl Default for SuggestOptions {
    fn default() -> Self {
        Self {
            max_suggestions: 6,
            max_series: MAX_SERIES,
            max_bar_categories: 20,
            max_pie_slices: 6,
            max_series_categories: 8,
        }
    }
}

#[derive(Clone, Copy)]
struct Roles<'a> {
    time: Option<&'a ColumnProfile>,
    measures: &'a [&'a ColumnProfile],
    categories: &'a [&'a ColumnProfile],
}

fn is_measure(column: &ColumnProfile) -> bool {
    column.semantic_type.is_numeric() && column.numeric.as_ref().is_some_and(|n| n.count > 0)
}

fn is_category(column: &ColumnProfile) -> bool {
    match column.semantic_type {
        SemanticType::Categorical | SemanticType::Boolean => true,
        SemanticType::Text | SemanticType::Email | SemanticType::Url => column.distinct.value >= 1,
        _ => false,
    }
}

fn has_word(name: &str, words: &[&str]) -> bool {
    let lower = name.to_ascii_lowercase();
    words.iter().any(|word| lower.contains(word))
}

/// Rank chart suggestions for a profiled result.
pub fn suggest_charts(profile: &ResultProfile, options: &SuggestOptions) -> Vec<ChartSuggestion> {
    if profile.row_count == 0 {
        return Vec::new();
    }
    let time = profile
        .columns
        .iter()
        .find(|c| c.semantic_type.is_temporal() && c.temporal.is_some());
    let measures: Vec<&ColumnProfile> = profile.columns.iter().filter(|c| is_measure(c)).collect();
    let mut categories: Vec<&ColumnProfile> =
        profile.columns.iter().filter(|c| is_category(c)).collect();
    // Categorical before free-form text, then fewer distinct values first.
    categories.sort_by_key(|c| {
        (
            c.semantic_type != SemanticType::Categorical,
            c.distinct.value,
        )
    });
    let roles = Roles {
        time,
        measures: &measures,
        categories: &categories,
    };

    let mut out: Vec<ChartSuggestion> = Vec::new();
    time_charts(roles, options, &mut out);
    category_charts(roles, options, &mut out);
    measure_charts(roles, options, profile, &mut out);
    cohort_chart(profile, &mut out);

    let incomplete = !profile.completeness.is_complete();
    let approximate = profile.columns.iter().any(|c| c.distinct.approximate);
    for suggestion in &mut out {
        if incomplete {
            suggestion.warnings.push(SuggestionWarning::IncompleteData);
        }
        if approximate && suggestion.top_n.is_some() {
            suggestion
                .warnings
                .push(SuggestionWarning::ApproximateProfile);
        }
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.kind.cmp(&b.kind)));
    let mut seen = Vec::new();
    out.retain(|s| {
        if seen.contains(&s.kind) {
            false
        } else {
            seen.push(s.kind);
            true
        }
    });
    out.truncate(options.max_suggestions);
    out
}

fn base(kind: ChartKindName, score: f64, reason: SuggestionReason) -> ChartSuggestion {
    ChartSuggestion {
        kind,
        score,
        reason,
        encodings: Encodings::default(),
        time_grain: None,
        top_n: None,
        sort: None,
        warnings: Vec::new(),
    }
}

fn names(columns: &[&ColumnProfile], max: usize) -> Vec<String> {
    columns.iter().take(max).map(|c| c.name.clone()).collect()
}

fn time_charts(roles: Roles<'_>, options: &SuggestOptions, out: &mut Vec<ChartSuggestion>) {
    let Some(time) = roles.time else {
        return;
    };
    let Some(temporal) = &time.temporal else {
        return;
    };
    let grain = Some(temporal.grain);
    let series = roles
        .categories
        .iter()
        .find(|c| c.distinct.value >= 2 && c.distinct.value <= options.max_series_categories)
        .map(|c| c.name.clone());
    if roles.measures.is_empty() {
        let mut line = base(ChartKindName::Line, 0.6, SuggestionReason::TimeSeries);
        line.encodings.x = Some(time.name.clone());
        line.time_grain = grain;
        line.sort = Some(SortHint::XAscending);
        out.push(line);
    } else {
        let y = names(roles.measures, options.max_series);
        let mut line = base(ChartKindName::Line, 0.95, SuggestionReason::TimeSeries);
        line.encodings.x = Some(time.name.clone());
        line.encodings.y = y.clone();
        line.encodings.series = series.clone().filter(|_| y.len() == 1);
        line.time_grain = grain;
        line.sort = Some(SortHint::XAscending);
        out.push(line);

        let non_negative = roles
            .measures
            .iter()
            .take(options.max_series)
            .all(|m| m.numeric.as_ref().is_some_and(|n| n.min >= 0.0));
        let mut area = base(
            ChartKindName::Area,
            if non_negative { 0.7 } else { 0.4 },
            SuggestionReason::TimeSeries,
        );
        area.encodings = out.last().map(|l| l.encodings.clone()).unwrap_or_default();
        area.time_grain = grain;
        area.sort = Some(SortHint::XAscending);
        out.push(area);

        let mut spark = base(ChartKindName::Sparkline, 0.35, SuggestionReason::TimeSeries);
        spark.encodings.x = Some(time.name.clone());
        spark.encodings.y = y.into_iter().take(1).collect();
        spark.time_grain = grain;
        out.push(spark);
    }
    let one_day = 86_400;
    if !temporal.has_time
        && temporal.span_seconds >= 60 * one_day
        && temporal.span_seconds <= 3 * 366 * one_day
    {
        let mut calendar = base(
            ChartKindName::CalendarHeatmap,
            0.6,
            SuggestionReason::DailyDensity,
        );
        calendar.encodings.x = Some(time.name.clone());
        calendar.encodings.value = roles.measures.first().map(|m| m.name.clone());
        calendar.time_grain = Some(TimeGrain::Day);
        out.push(calendar);
    }
    if let (Some(category), Some(measure)) = (roles.categories.first(), roles.measures.first()) {
        let mut heat = base(ChartKindName::Heatmap, 0.55, SuggestionReason::Matrix);
        heat.encodings.x = Some(time.name.clone());
        heat.encodings.y = vec![category.name.clone()];
        heat.encodings.value = Some(measure.name.clone());
        heat.time_grain = grain;
        heat.top_n = top_n(category, options);
        out.push(heat);
    }
}

fn top_n(category: &ColumnProfile, options: &SuggestOptions) -> Option<usize> {
    (category.distinct.value > options.max_bar_categories as u64)
        .then_some(options.max_bar_categories)
}

fn with_top_n(
    mut suggestion: ChartSuggestion,
    category: &ColumnProfile,
    options: &SuggestOptions,
) -> ChartSuggestion {
    suggestion.top_n = top_n(category, options);
    if let Some(n) = suggestion.top_n {
        suggestion.warnings.push(SuggestionWarning::TopN {
            distinct: category.distinct.value,
            top_n: n,
        });
    }
    suggestion
}

fn category_charts(roles: Roles<'_>, options: &SuggestOptions, out: &mut Vec<ChartSuggestion>) {
    let Some(category) = roles.categories.first().copied() else {
        return;
    };
    let measure = roles.measures.first().copied();
    let time_present = roles.time.is_some();
    let y = names(roles.measures, options.max_series);
    let mut bar = base(
        ChartKindName::Bar,
        if measure.is_some() {
            if time_present { 0.75 } else { 0.9 }
        } else {
            0.6
        },
        SuggestionReason::CategoryMeasure,
    );
    bar.encodings.x = Some(category.name.clone());
    bar.encodings.y = y;
    bar.sort = Some(SortHint::ValueDescending);
    out.push(with_top_n(bar, category, options));

    let Some(measure) = measure else {
        return;
    };
    let numeric = measure.numeric.as_ref();
    let non_negative = numeric.is_some_and(|n| n.min >= 0.0);
    let distinct = category.distinct.value;
    if non_negative && distinct >= 2 && distinct <= options.max_pie_slices && !time_present {
        for (kind, score) in [(ChartKindName::Pie, 0.62), (ChartKindName::Donut, 0.6)] {
            let mut share = base(kind, score, SuggestionReason::Share);
            share.encodings.category = Some(category.name.clone());
            share.encodings.value = Some(measure.name.clone());
            share.sort = Some(SortHint::ValueDescending);
            out.push(share);
        }
    }
    if non_negative && distinct > options.max_pie_slices && !time_present {
        let mut tree = base(ChartKindName::Treemap, 0.5, SuggestionReason::Share);
        tree.encodings.category = Some(category.name.clone());
        tree.encodings.series = roles.categories.get(1).map(|c| c.name.clone());
        tree.encodings.value = Some(measure.name.clone());
        out.push(with_top_n(tree, category, options));
    }
    let stage_named = has_word(&category.name, &["stage", "step", "funnel", "phase"]);
    let narrowing = matches!(
        measure.monotonicity,
        Monotonicity::StrictlyDecreasing | Monotonicity::Decreasing
    );
    if non_negative && (3..=12).contains(&distinct) && (stage_named || narrowing) && !time_present {
        let mut funnel = base(
            ChartKindName::Funnel,
            if stage_named && narrowing { 0.93 } else { 0.7 },
            SuggestionReason::OrderedStages,
        );
        funnel.encodings.category = Some(category.name.clone());
        funnel.encodings.value = Some(measure.name.clone());
        funnel.sort = Some(SortHint::XAscending);
        out.push(funnel);
    }
    if let Some(n) = numeric
        && n.negative_count > 0
        && n.max > 0.0
        && distinct <= 30
        && !time_present
    {
        let mut waterfall = base(
            ChartKindName::Waterfall,
            0.6,
            SuggestionReason::PositiveAndNegative,
        );
        waterfall.encodings.x = Some(category.name.clone());
        waterfall.encodings.value = Some(measure.name.clone());
        waterfall.sort = Some(SortHint::XAscending);
        out.push(waterfall);
    }
    if let Some(second) = roles.categories.get(1)
        && !time_present
    {
        let mut heat = base(ChartKindName::Heatmap, 0.72, SuggestionReason::Matrix);
        heat.encodings.x = Some(category.name.clone());
        heat.encodings.y = vec![second.name.clone()];
        heat.encodings.value = Some(measure.name.clone());
        out.push(with_top_n(heat, category, options));
    }
    if distinct >= 2 && distinct <= options.max_series_categories && profile_count(measure) >= 20 {
        let mut boxplot = base(ChartKindName::Box, 0.58, SuggestionReason::Distribution);
        boxplot.encodings.x = Some(category.name.clone());
        boxplot.encodings.y = vec![measure.name.clone()];
        out.push(boxplot);
    }
}

fn profile_count(column: &ColumnProfile) -> u64 {
    column.numeric.as_ref().map_or(0, |n| n.count)
}

fn measure_charts(
    roles: Roles<'_>,
    options: &SuggestOptions,
    profile: &ResultProfile,
    out: &mut Vec<ChartSuggestion>,
) {
    let measures = roles.measures;
    if let Some(first) = measures.first()
        && profile_count(first) >= 2
    {
        let mut histogram = base(
            ChartKindName::Histogram,
            if roles.categories.is_empty() && roles.time.is_none() {
                0.8
            } else {
                0.5
            },
            SuggestionReason::Distribution,
        );
        histogram.encodings.x = Some(first.name.clone());
        out.push(histogram);
        if roles.categories.is_empty() && profile_count(first) >= 20 {
            let mut boxplot = base(ChartKindName::Box, 0.45, SuggestionReason::Distribution);
            boxplot.encodings.y = vec![first.name.clone()];
            out.push(boxplot);
        }
    }
    if measures.len() >= 2 && roles.time.is_none() && profile.row_count >= 3 {
        let mut scatter = base(
            ChartKindName::Scatter,
            if roles.categories.is_empty() {
                0.85
            } else {
                0.7
            },
            SuggestionReason::TwoMeasures,
        );
        scatter.encodings.x = Some(measures[0].name.clone());
        scatter.encodings.y = vec![measures[1].name.clone()];
        scatter.encodings.size = measures.get(2).map(|m| m.name.clone());
        scatter.encodings.series = roles
            .categories
            .iter()
            .find(|c| c.distinct.value <= options.max_series_categories)
            .map(|c| c.name.clone());
        out.push(scatter);
    }
    if measures.len() >= 3 && profile.row_count >= 3 {
        let mut matrix = base(
            ChartKindName::CorrelationMatrix,
            0.6,
            SuggestionReason::Correlation,
        );
        matrix.encodings.y = measures.iter().map(|m| m.name.clone()).collect();
        out.push(matrix);
    }
}

fn cohort_chart(profile: &ResultProfile, out: &mut Vec<ChartSuggestion>) {
    let cohort = profile
        .columns
        .iter()
        .find(|c| has_word(&c.name, &["cohort"]));
    let period = profile.columns.iter().find(|c| {
        matches!(c.semantic_type, SemanticType::Integer)
            && has_word(
                &c.name,
                &[
                    "period",
                    "offset",
                    "month_number",
                    "week_number",
                    "age",
                    "months_since",
                    "weeks_since",
                ],
            )
    });
    let measure = profile.columns.iter().find(|c| {
        is_measure(c)
            && Some(c.name.as_str()) != period.map(|p| p.name.as_str())
            && has_word(
                &c.name,
                &[
                    "retention",
                    "retained",
                    "active",
                    "users",
                    "customers",
                    "rate",
                    "count",
                ],
            )
    });
    if let (Some(cohort), Some(period), Some(measure)) = (cohort, period, measure) {
        let mut chart = base(
            ChartKindName::CohortRetention,
            0.97,
            SuggestionReason::Cohort,
        );
        chart.encodings.x = Some(period.name.clone());
        chart.encodings.y = vec![cohort.name.clone()];
        chart.encodings.value = Some(measure.name.clone());
        out.push(chart);
    }
}

#[cfg(test)]
mod tests {
    use sqlly_core::{CellValue, ResultColumn};

    use super::*;
    use crate::analytics::Completeness;
    use crate::analytics::profile::{ProfileOptions, profile_rows};
    use crate::analytics::test_support::*;

    fn suggest(columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> Vec<ChartSuggestion> {
        let profile = profile_rows(
            columns,
            rows,
            Completeness::Complete,
            &ProfileOptions::default(),
        );
        suggest_charts(&profile, &SuggestOptions::default())
    }

    fn kinds(suggestions: &[ChartSuggestion]) -> Vec<ChartKindName> {
        suggestions.iter().map(|s| s.kind).collect()
    }

    fn date(day: i64) -> CellValue {
        let d = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(day);
        s(&d.format("%Y-%m-%d").to_string())
    }

    #[test]
    fn time_series_suggests_line_first_with_a_grain() {
        let columns = vec![
            col("day", "date"),
            col("revenue", "decimal"),
            col("id", "int"),
        ];
        let rows: Vec<Vec<CellValue>> = (0..120)
            .map(|d| vec![date(d), f(100.0 + d as f64), i(d)])
            .collect();
        let suggestions = suggest(&columns, &rows);
        assert_eq!(suggestions[0].kind, ChartKindName::Line);
        assert_eq!(suggestions[0].encodings.x.as_deref(), Some("day"));
        assert_eq!(
            suggestions[0].encodings.y,
            vec!["revenue"],
            "the ID column is not a measure"
        );
        assert_eq!(suggestions[0].time_grain, Some(TimeGrain::Week));
        assert!(kinds(&suggestions).contains(&ChartKindName::Area));
        assert!(kinds(&suggestions).contains(&ChartKindName::CalendarHeatmap));
        assert!(suggestions.iter().all(|s| s.warnings.is_empty()));
    }

    #[test]
    fn category_and_measure_suggest_bar_and_share_charts_for_few_categories() {
        let columns = vec![col("region", "text"), col("sales", "int")];
        let rows: Vec<Vec<CellValue>> = (0..40)
            .map(|n| vec![s(["n", "s", "e", "w"][n % 4]), i(n as i64)])
            .collect();
        let suggestions = suggest(&columns, &rows);
        assert_eq!(suggestions[0].kind, ChartKindName::Bar);
        assert_eq!(suggestions[0].encodings.x.as_deref(), Some("region"));
        assert_eq!(suggestions[0].sort, Some(SortHint::ValueDescending));
        let all = kinds(&suggestions);
        assert!(all.contains(&ChartKindName::Pie) && all.contains(&ChartKindName::Donut));
        assert!(!all.contains(&ChartKindName::Treemap));
    }

    #[test]
    fn high_cardinality_gets_top_n_and_no_pie() {
        let columns = vec![col("product", "text"), col("units", "int")];
        let rows: Vec<Vec<CellValue>> = (0..300)
            .map(|n| vec![s(&format!("p{}", n % 100)), i(n as i64)])
            .collect();
        let profile = profile_rows(
            &columns,
            &rows,
            Completeness::Complete,
            &ProfileOptions::default(),
        );
        // 100 distinct text values reads as text, not categorical; still a category axis.
        let suggestions = suggest_charts(&profile, &SuggestOptions::default());
        let bar = suggestions
            .iter()
            .find(|s| s.kind == ChartKindName::Bar)
            .expect("bar");
        assert_eq!(bar.top_n, Some(20));
        assert!(bar.warnings.contains(&SuggestionWarning::TopN {
            distinct: 100,
            top_n: 20
        }));
        assert!(!kinds(&suggestions).contains(&ChartKindName::Pie));
        assert!(kinds(&suggestions).contains(&ChartKindName::Treemap));
    }

    #[test]
    fn two_measures_scatter_and_many_measures_correlate() {
        let columns = vec![
            col("height", "float"),
            col("weight", "float"),
            col("age", "int"),
        ];
        let rows: Vec<Vec<CellValue>> = (0..50)
            .map(|n| {
                vec![
                    f(150.0 + n as f64),
                    f(50.0 + n as f64 / 2.0),
                    i(20 + n % 30),
                ]
            })
            .collect();
        let suggestions = suggest(&columns, &rows);
        assert_eq!(suggestions[0].kind, ChartKindName::Scatter);
        assert_eq!(suggestions[0].encodings.size.as_deref(), Some("age"));
        assert!(kinds(&suggestions).contains(&ChartKindName::CorrelationMatrix));
        assert!(kinds(&suggestions).contains(&ChartKindName::Histogram));
    }

    #[test]
    fn single_measure_is_a_distribution() {
        let columns = vec![col("latency_ms", "float")];
        let rows: Vec<Vec<CellValue>> = (0..100).map(|n| vec![f((n * 7 % 31) as f64)]).collect();
        let suggestions = suggest(&columns, &rows);
        assert_eq!(
            kinds(&suggestions),
            vec![ChartKindName::Histogram, ChartKindName::Box]
        );
    }

    #[test]
    fn ordered_stages_suggest_a_funnel_and_signed_values_a_waterfall() {
        let columns = vec![col("stage", "text"), col("users", "int")];
        let rows = vec![
            vec![s("visit"), i(1000)],
            vec![s("signup"), i(400)],
            vec![s("trial"), i(150)],
            vec![s("paid"), i(40)],
        ];
        let suggestions = suggest(&columns, &rows);
        assert_eq!(suggestions[0].kind, ChartKindName::Funnel);
        assert_eq!(suggestions[0].encodings.category.as_deref(), Some("stage"));

        let columns = vec![col("step", "text"), col("change", "int")];
        let rows = vec![
            vec![s("start"), i(100)],
            vec![s("refunds"), i(-20)],
            vec![s("fees"), i(-5)],
            vec![s("growth"), i(30)],
        ];
        assert!(kinds(&suggest(&columns, &rows)).contains(&ChartKindName::Waterfall));
    }

    #[test]
    fn two_categories_and_a_measure_suggest_a_heatmap() {
        let columns = vec![
            col("weekday", "text"),
            col("hour_band", "text"),
            col("orders", "int"),
        ];
        let rows: Vec<Vec<CellValue>> = (0..70)
            .map(|n| {
                vec![
                    s(["mon", "tue", "wed", "thu", "fri", "sat", "sun"][n % 7]),
                    s(["am", "pm"][n % 2]),
                    i(n as i64),
                ]
            })
            .collect();
        let heat = suggest(&columns, &rows)
            .into_iter()
            .find(|s| s.kind == ChartKindName::Heatmap)
            .expect("heatmap");
        assert_eq!(
            heat.encodings.x.as_deref(),
            Some("hour_band"),
            "fewest categories on x"
        );
        assert_eq!(heat.encodings.value.as_deref(), Some("orders"));
    }

    #[test]
    fn cohort_shaped_results_suggest_retention() {
        let columns = vec![
            col("cohort_month", "text"),
            col("period", "int"),
            col("retained_users", "int"),
        ];
        let rows: Vec<Vec<CellValue>> = (0..12)
            .map(|n| {
                vec![
                    s(["2024-01", "2024-02", "2024-03"][n % 3]),
                    i((n / 3) as i64),
                    i(100 - n as i64 * 5),
                ]
            })
            .collect();
        assert_eq!(
            suggest(&columns, &rows)[0].kind,
            ChartKindName::CohortRetention
        );
    }

    #[test]
    fn incomplete_profiles_warn_and_empty_results_suggest_nothing() {
        let columns = vec![col("region", "text"), col("sales", "int")];
        let rows: Vec<Vec<CellValue>> = (0..10)
            .map(|n| vec![s(["a", "b"][n % 2]), i(n as i64)])
            .collect();
        let profile = profile_rows(
            &columns,
            &rows,
            Completeness::Sampled { n: 10 },
            &ProfileOptions::default(),
        );
        let suggestions = suggest_charts(&profile, &SuggestOptions::default());
        assert!(!suggestions.is_empty());
        assert!(
            suggestions
                .iter()
                .all(|s| s.warnings.contains(&SuggestionWarning::IncompleteData))
        );
        assert!(suggest(&columns, &[]).is_empty());
        let text_only = vec![col("a", "text")];
        let rows = vec![vec![s("x")], vec![s("y")]];
        assert_eq!(kinds(&suggest(&text_only, &rows)), vec![ChartKindName::Bar]);
    }

    #[test]
    fn suggestions_are_deterministic_ranked_and_named_like_chart_specs() {
        let columns = vec![
            col("day", "date"),
            col("region", "text"),
            col("sales", "int"),
            col("cost", "int"),
        ];
        let rows: Vec<Vec<CellValue>> = (0..90)
            .map(|d| {
                vec![
                    date(d % 30),
                    s(["a", "b", "c"][(d % 3) as usize]),
                    i(d),
                    i(d / 2),
                ]
            })
            .collect();
        let first = suggest(&columns, &rows);
        assert_eq!(first, suggest(&columns, &rows));
        assert!(first.windows(2).all(|w| w[0].score >= w[1].score));
        assert!(first.len() <= SuggestOptions::default().max_suggestions);
        let mut unique = kinds(&first);
        unique.dedup();
        assert_eq!(unique.len(), first.len());
        for kind in ChartKindName::ALL {
            assert_eq!(ChartKindName::from_name(kind.as_str()), Some(kind));
            assert_eq!(
                serde_json::to_string(&kind).unwrap(),
                format!("\"{}\"", kind.as_str())
            );
        }
        assert_eq!(first[0].encodings.y, vec!["sales", "cost"]);
    }
}
