//! Result chart — configures the datatable Chart tab from a `-- sqlly chart:`
//! SQL comment hint, the sibling of [`crate::result_pivot`].
//!
//! Like the pivot spec, every field is a result-column **name** so the
//! directive stays stable when the SELECT list is reordered; the name→index
//! resolution to a [`ChartConfig`] (whose fields are source column indices)
//! happens against the live result columns. Pure, UI-free, no I/O.

use sqlly_core::ResultColumn;
use sqlly_datatable::{ChartAggregate, ChartConfig, ChartKind, ChartOrder};

use crate::result_pivot::ResultPivotDiagnostic;

// ---------------------------------------------------------------------------
// Enum <-> directive-token helpers
// ---------------------------------------------------------------------------
//
// The datatable's chart enums carry sidebar labels, not directive tokens, and
// no `from_str`. The directive grammar owns its own stable, lowercase spelling
// so a saved directive never shifts if a sidebar label is reworded.

/// The lowercase directive token for a chart kind.
fn chart_kind_as_str(kind: ChartKind) -> &'static str {
    match kind {
        ChartKind::Bar => "bar",
        ChartKind::Line => "line",
        ChartKind::Area => "area",
        ChartKind::Scatter => "scatter",
        ChartKind::Histogram => "histogram",
        ChartKind::Pie => "pie",
        ChartKind::Donut => "donut",
    }
}

/// Parse a chart kind from its directive token (case-insensitive).
fn chart_kind_from_str(raw: &str) -> Option<ChartKind> {
    match raw.to_ascii_lowercase().as_str() {
        "bar" => Some(ChartKind::Bar),
        "line" => Some(ChartKind::Line),
        "area" => Some(ChartKind::Area),
        "scatter" => Some(ChartKind::Scatter),
        "histogram" => Some(ChartKind::Histogram),
        "pie" => Some(ChartKind::Pie),
        "donut" => Some(ChartKind::Donut),
        _ => None,
    }
}

/// The lowercase directive token for an aggregate.
fn chart_aggregate_as_str(aggregate: ChartAggregate) -> &'static str {
    match aggregate {
        ChartAggregate::None => "none",
        ChartAggregate::Sum => "sum",
        ChartAggregate::Average => "average",
        ChartAggregate::Minimum => "minimum",
        ChartAggregate::Maximum => "maximum",
        ChartAggregate::Count => "count",
    }
}

/// Parse an aggregate from its directive token (case-insensitive). The terse
/// `avg` / `min` / `max` spellings are accepted as aliases of the canonical
/// tokens the serializer emits.
fn chart_aggregate_from_str(raw: &str) -> Option<ChartAggregate> {
    match raw.to_ascii_lowercase().as_str() {
        "none" => Some(ChartAggregate::None),
        "sum" => Some(ChartAggregate::Sum),
        "average" | "avg" => Some(ChartAggregate::Average),
        "minimum" | "min" => Some(ChartAggregate::Minimum),
        "maximum" | "max" => Some(ChartAggregate::Maximum),
        "count" => Some(ChartAggregate::Count),
        _ => None,
    }
}

/// The lowercase directive token for a category order.
fn chart_order_as_str(order: ChartOrder) -> &'static str {
    match order {
        ChartOrder::Source => "source",
        ChartOrder::CategoryAscending => "category",
        ChartOrder::ValueDescending => "value",
    }
}

/// Parse a category order from its directive token (case-insensitive).
fn chart_order_from_str(raw: &str) -> Option<ChartOrder> {
    match raw.to_ascii_lowercase().as_str() {
        "source" => Some(ChartOrder::Source),
        "category" => Some(ChartOrder::CategoryAscending),
        "value" => Some(ChartOrder::ValueDescending),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Spec
// ---------------------------------------------------------------------------

/// A fully-parsed chart request. Fields are result-column names (for `label`
/// and `values`) so the directive survives a reordered SELECT list; the other
/// fields mirror [`ChartConfig`] exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultChartSpec {
    /// The chart presentation (required — the one field that makes a chart
    /// directive meaningful).
    pub kind: ChartKind,
    /// Column whose values label the categories. `None` lets the chart
    /// auto-detect the first string-ish column.
    pub label_name: Option<String>,
    /// Numeric columns to plot as series, in draw order. Empty auto-selects
    /// every numeric column (capped by the datatable).
    pub value_names: Vec<String>,
    /// How multiple source rows collapse into one category value.
    pub aggregate: ChartAggregate,
    /// How categories are ordered along the axis.
    pub order: ChartOrder,
    /// Most categories drawn, after ordering.
    pub top_n: usize,
    /// Whether clicking a mark navigates to the source rows on the Grid tab.
    pub navigate_on_click: bool,
    /// Which result set (0-based) the hint targets. Defaults to the first set.
    pub result_set_index: usize,
}

impl ResultChartSpec {
    /// A minimal spec: just the kind, everything else at [`ChartConfig`]
    /// defaults.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(kind: ChartKind) -> Self {
        let defaults = ChartConfig::default();
        Self {
            kind,
            label_name: None,
            value_names: Vec::new(),
            aggregate: defaults.aggregate,
            order: defaults.order,
            top_n: defaults.top_n,
            navigate_on_click: defaults.navigate_on_click,
            result_set_index: 0,
        }
    }

    /// Resolve a column name to its index in `columns`, or an error diagnostic
    /// naming the missing column — the same validation the pivot spec does.
    fn resolve(
        role: &str,
        name: &str,
        columns: &[ResultColumn],
    ) -> Result<usize, ResultPivotDiagnostic> {
        columns
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                crate::result_pivot::localized_diagnostic(
                    "results.chart.missing_column",
                    &[("field_role", role), ("column_name", name)],
                )
            })
    }

    /// Convert this name-based hint into the datatable's index-based
    /// [`ChartConfig`]. Returns `None` when a named column is absent so a
    /// stale directive falls back to the chart's own auto-detection rather
    /// than plotting the wrong columns.
    pub fn chart_config(&self, columns: &[ResultColumn]) -> Option<ChartConfig> {
        let label_column = match &self.label_name {
            Some(name) => Some(Self::resolve("label", name, columns).ok()?),
            None => None,
        };
        let value_columns = self
            .value_names
            .iter()
            .map(|name| Self::resolve("value", name, columns))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        Some(ChartConfig {
            kind: self.kind,
            label_column,
            value_columns,
            aggregate: self.aggregate,
            order: self.order,
            top_n: self.top_n,
            navigate_on_click: self.navigate_on_click,
        })
    }

    /// Capture the native chart's current index-based state as a stable,
    /// name-based query directive. Returns `None` if any referenced index is
    /// out of range.
    pub fn from_config(
        config: &ChartConfig,
        columns: &[ResultColumn],
        result_set_index: usize,
    ) -> Option<Self> {
        let label_name = match config.label_column {
            Some(index) => Some(columns.get(index)?.name.clone()),
            None => None,
        };
        let value_names = config
            .value_columns
            .iter()
            .map(|&index| columns.get(index).map(|column| column.name.clone()))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            kind: config.kind,
            label_name,
            value_names,
            aggregate: config.aggregate,
            order: config.order,
            top_n: config.top_n,
            navigate_on_click: config.navigate_on_click,
            result_set_index,
        })
    }

    /// Serialize this spec back into a `-- sqlly chart:` comment. `kind` is
    /// always written; every other field is emitted only when it differs from
    /// the [`ChartConfig`] default, so the common directive stays terse and
    /// the round trip is exact.
    pub fn directive(&self) -> String {
        let defaults = ChartConfig::default();
        let mut directive = format!("-- sqlly chart: kind:{}", chart_kind_as_str(self.kind));
        if let Some(name) = &self.label_name {
            directive.push_str(&format!("; label:{name}"));
        }
        if !self.value_names.is_empty() {
            directive.push_str(&format!("; values:{}", self.value_names.join(",")));
        }
        if self.aggregate != defaults.aggregate {
            directive.push_str(&format!(
                "; aggregate:{}",
                chart_aggregate_as_str(self.aggregate)
            ));
        }
        if self.order != defaults.order {
            directive.push_str(&format!("; order:{}", chart_order_as_str(self.order)));
        }
        if self.top_n != defaults.top_n {
            directive.push_str(&format!("; top:{}", self.top_n));
        }
        if self.navigate_on_click != defaults.navigate_on_click {
            directive.push_str(&format!("; navigate:{}", self.navigate_on_click));
        }
        if self.result_set_index > 0 {
            directive.push_str(&format!("; result:{}", self.result_set_index + 1));
        }
        directive
    }
}

/// Replace the last chart directive or insert one with the other leading
/// SQLLY directives — the chart sidebar's save button persisted back into the
/// query editor. Mirrors [`crate::result_pivot::update_or_insert_pivot_comment`].
pub fn update_or_insert_chart_comment(sql: &str, spec: &ResultChartSpec) -> String {
    let replacement = spec.directive();
    let mut lines: Vec<String> = sql.lines().map(str::to_string).collect();
    if let Some(index) = lines
        .iter()
        .rposition(|line| chart_comment_body(line).is_some())
    {
        lines[index] = replacement;
    } else {
        let insert_at = lines
            .iter()
            .take_while(|line| {
                let line = line.trim();
                line.is_empty()
                    || (line.starts_with("--") && crate::sqlly_directive::names_namespace(line))
            })
            .count();
        lines.insert(insert_at, replacement);
    }
    let mut updated = lines.join("\n");
    if sql.ends_with('\n') {
        updated.push('\n');
    }
    updated
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// The `-- sqlly chart:` body of a line that is entirely such a directive,
/// via the shared lexer.
pub fn chart_comment_body(line: &str) -> Option<&str> {
    match crate::sqlly_directive::classify(line.trim())? {
        crate::sqlly_directive::Directive::Chart { body } => Some(body),
        _ => None,
    }
}

/// Parse a `-- sqlly chart:` body into a [`ResultChartSpec`]. Only `kind:` is
/// required; every other field defaults to [`ChartConfig`]'s value.
pub fn parse_chart_body(body: &str) -> Result<ResultChartSpec, ResultPivotDiagnostic> {
    let defaults = ChartConfig::default();
    let mut kind: Option<ChartKind> = None;
    let mut label_name: Option<String> = None;
    let mut value_names: Vec<String> = Vec::new();
    let mut aggregate = defaults.aggregate;
    let mut order = defaults.order;
    let mut top_n = defaults.top_n;
    let mut navigate_on_click = defaults.navigate_on_click;
    let mut result_set_index = 0usize;

    for segment in body.split(';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let Some((key, rest)) = segment.split_once(':') else {
            return Err(crate::result_pivot::localized_diagnostic(
                "results.chart.malformed_field",
                &[("field", segment)],
            ));
        };
        let key = key.trim().to_ascii_lowercase();
        let rest = rest.trim();
        match key.as_str() {
            "kind" => {
                kind = Some(chart_kind_from_str(rest).ok_or_else(|| {
                    crate::result_pivot::localized_diagnostic(
                        "results.chart.invalid_kind",
                        &[("kind", rest)],
                    )
                })?);
            }
            "label" => {
                label_name = Some(parse_column_name("label", rest)?);
            }
            "values" | "value" => {
                value_names = rest
                    .split(',')
                    .map(|name| parse_column_name("values", name))
                    .collect::<Result<Vec<_>, _>>()?;
            }
            "aggregate" => {
                aggregate = chart_aggregate_from_str(rest).ok_or_else(|| {
                    crate::result_pivot::localized_diagnostic(
                        "results.chart.invalid_aggregate",
                        &[("aggregate", rest)],
                    )
                })?;
            }
            "order" => {
                order = chart_order_from_str(rest).ok_or_else(|| {
                    crate::result_pivot::localized_diagnostic(
                        "results.chart.invalid_order",
                        &[("order", rest)],
                    )
                })?;
            }
            "top" => {
                top_n = rest.parse().ok().filter(|n| *n >= 1).ok_or_else(|| {
                    crate::result_pivot::localized_diagnostic(
                        "results.chart.invalid_top_count",
                        &[("value", rest)],
                    )
                })?;
            }
            "navigate" => {
                navigate_on_click = match rest.to_ascii_lowercase().as_str() {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(crate::result_pivot::localized_diagnostic(
                            "results.chart.invalid_navigate",
                            &[("value", rest)],
                        ));
                    }
                };
            }
            "result" => {
                result_set_index =
                    crate::result_pivot::parse_result_number(rest).ok_or_else(|| {
                        crate::result_pivot::localized_diagnostic(
                            "results.chart.invalid_result_number",
                            &[("value", rest)],
                        )
                    })?;
            }
            _ => {
                return Err(crate::result_pivot::localized_diagnostic(
                    "results.chart.unknown_field",
                    &[("field_name", &key)],
                ));
            }
        }
    }

    let Some(kind) = kind else {
        return Err(crate::result_pivot::localized_diagnostic(
            "results.chart.kind_required",
            &[],
        ));
    };

    Ok(ResultChartSpec {
        kind,
        label_name,
        value_names,
        aggregate,
        order,
        top_n,
        navigate_on_click,
        result_set_index,
    })
}

/// A single column name, with SQL identifier delimiters trimmed and column
/// indices rejected — the same rule the pivot parser applies.
fn parse_column_name(field: &str, raw: &str) -> Result<String, ResultPivotDiagnostic> {
    let name = raw.trim().trim_matches(['[', ']', '`', '"']);
    if name.is_empty() || name.parse::<usize>().is_ok() {
        return Err(crate::result_pivot::localized_diagnostic(
            "results.chart.column_index_not_allowed",
            &[("field_role", field)],
        ));
    }
    Ok(name.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result_pivot::parse_view_hints;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: "int".to_string(),
            nullable: false,
        }
    }

    #[test]
    fn parses_minimal_kind_only_directive() {
        let spec = parse_chart_body(" kind:bar").unwrap();
        assert_eq!(spec, ResultChartSpec::new(ChartKind::Bar));
    }

    #[test]
    fn parses_every_kind() {
        for (token, kind) in [
            ("bar", ChartKind::Bar),
            ("line", ChartKind::Line),
            ("area", ChartKind::Area),
            ("scatter", ChartKind::Scatter),
            ("histogram", ChartKind::Histogram),
            ("pie", ChartKind::Pie),
            ("donut", ChartKind::Donut),
        ] {
            let spec = parse_chart_body(&format!(" kind:{token}")).unwrap();
            assert_eq!(spec.kind, kind, "token {token}");
        }
    }

    #[test]
    fn parses_full_directive() {
        let spec = parse_chart_body(
            " kind:line; label:Month; values:Sales,Profit; aggregate:sum; order:value; \
             top:25; navigate:false",
        )
        .unwrap();
        assert_eq!(spec.kind, ChartKind::Line);
        assert_eq!(spec.label_name.as_deref(), Some("Month"));
        assert_eq!(spec.value_names, ["Sales", "Profit"]);
        assert_eq!(spec.aggregate, ChartAggregate::Sum);
        assert_eq!(spec.order, ChartOrder::ValueDescending);
        assert_eq!(spec.top_n, 25);
        assert!(!spec.navigate_on_click);
    }

    #[test]
    fn aggregate_aliases_are_accepted() {
        for (token, aggregate) in [
            ("avg", ChartAggregate::Average),
            ("min", ChartAggregate::Minimum),
            ("max", ChartAggregate::Maximum),
        ] {
            let spec = parse_chart_body(&format!(" kind:bar; aggregate:{token}")).unwrap();
            assert_eq!(spec.aggregate, aggregate, "alias {token}");
        }
    }

    #[test]
    fn missing_kind_is_an_error() {
        let err = parse_chart_body(" label:Month; values:Sales").unwrap_err();
        assert!(err.message.contains("kind"), "{}", err.message);
    }

    #[test]
    fn unknown_kind_is_an_error() {
        let err = parse_chart_body(" kind:bubble").unwrap_err();
        assert!(err.message.contains("bubble"), "{}", err.message);
    }

    #[test]
    fn unknown_field_is_an_error() {
        let err = parse_chart_body(" kind:bar; wat:1").unwrap_err();
        assert!(err.message.contains("unknown field"), "{}", err.message);
    }

    #[test]
    fn column_indices_are_rejected() {
        let err = parse_chart_body(" kind:bar; label:0").unwrap_err();
        assert!(
            err.message.contains("not use a column index"),
            "{}",
            err.message
        );
    }

    #[test]
    fn bad_top_is_an_error() {
        for bad in ["0", "-1", "lots"] {
            let err = parse_chart_body(&format!(" kind:bar; top:{bad}")).unwrap_err();
            assert!(err.message.contains("positive integer"), "top:{bad}");
        }
    }

    #[test]
    fn chart_config_resolves_names_to_indices() {
        let columns = vec![col("Month"), col("Sales"), col("Profit")];
        let spec = parse_chart_body(" kind:bar; label:Month; values:Sales,Profit").unwrap();
        let config = spec.chart_config(&columns).unwrap();
        assert_eq!(config.kind, ChartKind::Bar);
        assert_eq!(config.label_column, Some(0));
        assert_eq!(config.value_columns, vec![1, 2]);
    }

    #[test]
    fn chart_config_is_none_when_a_column_is_missing() {
        let columns = vec![col("Month"), col("Sales")];
        let spec = parse_chart_body(" kind:bar; values:Missing").unwrap();
        assert!(spec.chart_config(&columns).is_none());
    }

    #[test]
    fn config_directive_round_trips() {
        let columns = vec![col("Month"), col("Sales"), col("Profit")];
        let config = ChartConfig {
            kind: ChartKind::Line,
            label_column: Some(0),
            value_columns: vec![1, 2],
            aggregate: ChartAggregate::Average,
            order: ChartOrder::CategoryAscending,
            top_n: 50,
            navigate_on_click: false,
        };
        let spec = ResultChartSpec::from_config(&config, &columns, 0).unwrap();
        let directive = spec.directive();
        assert_eq!(
            directive,
            "-- sqlly chart: kind:line; label:Month; values:Sales,Profit; \
             aggregate:average; order:category; top:50; navigate:false"
        );
        let reparsed = parse_view_hints(&format!("{directive}\nSELECT 1;"))
            .chart
            .unwrap();
        assert_eq!(reparsed, spec);
        // And the reparsed spec rebuilds the same native config.
        assert_eq!(reparsed.chart_config(&columns), Some(config));
    }

    #[test]
    fn minimal_config_serializes_to_kind_only() {
        let spec = ResultChartSpec::new(ChartKind::Pie);
        assert_eq!(spec.directive(), "-- sqlly chart: kind:pie");
    }

    #[test]
    fn update_or_insert_replaces_existing_directive() {
        let sql = "-- sqlly view: chart\n-- sqlly chart: kind:bar\nSELECT 1;";
        let spec = parse_chart_body(" kind:line; label:Month").unwrap();
        let updated = update_or_insert_chart_comment(sql, &spec);
        assert!(updated.contains("-- sqlly chart: kind:line; label:Month"));
        assert!(!updated.contains("kind:bar"));
    }

    #[test]
    fn update_or_insert_adds_after_leading_directives() {
        let sql = "-- sqlly view: chart\nSELECT 1;";
        let spec = ResultChartSpec::new(ChartKind::Donut);
        let updated = update_or_insert_chart_comment(sql, &spec);
        assert_eq!(
            updated,
            "-- sqlly view: chart\n-- sqlly chart: kind:donut\nSELECT 1;"
        );
    }
}
