//! Transform cells: a no-code list of steps (filter, select, rename,
//! computed column, group, join, sort, limit, pivot, unpivot) compiled to
//! one DuckDB query over the inputs materialized in the notebook's local
//! workspace. The compiled SQL is shown read-only next to the steps.
//!
//! ```json
//! {"input": "c_orders", "inputs": ["c_customers"],
//!  "steps": [{"op": "filter", "column": "amount", "test": "gt", "value": 100},
//!            {"op": "join", "with": "c_customers", "kind": "left", "on": [{"left": "customer_id", "right": "id"}]},
//!            {"op": "group", "by": ["country"], "aggregates": [{"function": "sum", "column": "amount"}]},
//!            {"op": "sort", "by": [{"column": "sum_amount", "descending": true}]},
//!            {"op": "limit", "rows": 10}]}
//! ```
//!
//! Every name is quoted as an identifier and every value is written as a
//! literal; the only free text is a computed column's expression, which is
//! checked to be one expression (no statement separators or comments) and
//! runs in the workspace's read-only query path.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlly_core::DisplayMessage;

use super::spec::SpecExtra;

/// Longest computed-column expression accepted.
pub const MAX_EXPRESSION_CHARS: usize = 2_000;

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TransformCellSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// Other cells joined in (kept in step with the join steps).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
    #[serde(default)]
    pub steps: Vec<TransformStep>,
    #[serde(flatten)]
    pub extra: SpecExtra,
}

/// How a filter step compares. The text tests (`contains`, `starts_with`)
/// ignore case. Comparisons follow SQL: a row whose value is empty (NULL)
/// matches no comparison, so `ne` leaves empty values out unless the step
/// asks to keep them (`include_empty`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterTest {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Contains,
    StartsWith,
    IsNull,
    NotNull,
    In,
}

impl FilterTest {
    pub const ALL: [FilterTest; 11] = [
        FilterTest::Eq,
        FilterTest::Ne,
        FilterTest::Lt,
        FilterTest::Le,
        FilterTest::Gt,
        FilterTest::Ge,
        FilterTest::Contains,
        FilterTest::StartsWith,
        FilterTest::IsNull,
        FilterTest::NotNull,
        FilterTest::In,
    ];

    pub fn needs_value(self) -> bool {
        !matches!(self, FilterTest::IsNull | FilterTest::NotNull)
    }

    /// Whether the step can choose to keep rows whose value is empty.
    pub fn offers_include_empty(self) -> bool {
        self == FilterTest::Ne
    }

    pub fn name(self) -> &'static str {
        match self {
            FilterTest::Eq => "eq",
            FilterTest::Ne => "ne",
            FilterTest::Lt => "lt",
            FilterTest::Le => "le",
            FilterTest::Gt => "gt",
            FilterTest::Ge => "ge",
            FilterTest::Contains => "contains",
            FilterTest::StartsWith => "starts_with",
            FilterTest::IsNull => "is_null",
            FilterTest::NotNull => "not_null",
            FilterTest::In => "in",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggFunction {
    #[default]
    Sum,
    Avg,
    Min,
    Max,
    Count,
    CountDistinct,
    Median,
}

impl AggFunction {
    pub const ALL: [AggFunction; 7] = [
        AggFunction::Sum,
        AggFunction::Avg,
        AggFunction::Min,
        AggFunction::Max,
        AggFunction::Count,
        AggFunction::CountDistinct,
        AggFunction::Median,
    ];

    pub fn name(self) -> &'static str {
        match self {
            AggFunction::Sum => "sum",
            AggFunction::Avg => "avg",
            AggFunction::Min => "min",
            AggFunction::Max => "max",
            AggFunction::Count => "count",
            AggFunction::CountDistinct => "count_distinct",
            AggFunction::Median => "median",
        }
    }

    fn call(self, column: Option<&str>) -> String {
        match (self, column) {
            (AggFunction::Count, None) | (AggFunction::CountDistinct, None) => {
                "COUNT(*)".to_string()
            }
            (AggFunction::CountDistinct, Some(c)) => format!("COUNT(DISTINCT {})", ident(c)),
            (f, Some(c)) => format!("{}({})", f.sql(), ident(c)),
            (f, None) => format!("{}(*)", f.sql()),
        }
    }

    fn sql(self) -> &'static str {
        match self {
            AggFunction::Sum => "SUM",
            AggFunction::Avg => "AVG",
            AggFunction::Min => "MIN",
            AggFunction::Max => "MAX",
            AggFunction::Count | AggFunction::CountDistinct => "COUNT",
            AggFunction::Median => "MEDIAN",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AggregateColumn {
    #[serde(default)]
    pub function: AggFunction,
    /// `None` counts rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

impl AggregateColumn {
    /// The output column name: the alias, else `<function>_<column>`.
    pub fn output_name(&self) -> String {
        if let Some(alias) = self.alias.as_ref().filter(|a| !a.trim().is_empty()) {
            return alias.clone();
        }
        match &self.column {
            Some(column) => format!("{}_{column}", self.function.name()),
            None => "row_count".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinKind {
    #[default]
    Inner,
    Left,
    Right,
    Full,
}

impl JoinKind {
    pub const ALL: [JoinKind; 4] = [
        JoinKind::Inner,
        JoinKind::Left,
        JoinKind::Right,
        JoinKind::Full,
    ];

    pub fn name(self) -> &'static str {
        match self {
            JoinKind::Inner => "inner",
            JoinKind::Left => "left",
            JoinKind::Right => "right",
            JoinKind::Full => "full",
        }
    }

    fn sql(self) -> &'static str {
        match self {
            JoinKind::Inner => "INNER JOIN",
            JoinKind::Left => "LEFT JOIN",
            JoinKind::Right => "RIGHT JOIN",
            JoinKind::Full => "FULL OUTER JOIN",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct JoinOn {
    pub left: String,
    pub right: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SortKey {
    pub column: String,
    #[serde(default)]
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TransformStep {
    Filter {
        column: String,
        test: FilterTest,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        value: Value,
        /// Keep rows whose value is empty (NULL) too (`ne` only).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        include_empty: bool,
    },
    Select {
        columns: Vec<String>,
    },
    Rename {
        column: String,
        to: String,
    },
    Compute {
        name: String,
        expression: String,
    },
    Group {
        #[serde(default)]
        by: Vec<String>,
        #[serde(default)]
        aggregates: Vec<AggregateColumn>,
    },
    Join {
        with: String,
        #[serde(default)]
        kind: JoinKind,
        #[serde(default)]
        on: Vec<JoinOn>,
    },
    Sort {
        by: Vec<SortKey>,
    },
    Limit {
        rows: u64,
    },
    Pivot {
        on: String,
        value: String,
        #[serde(default)]
        function: AggFunction,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        group_by: Vec<String>,
    },
    Unpivot {
        columns: Vec<String>,
        name: String,
        value: String,
    },
}

impl TransformStep {
    /// The stable operation name (`filter`, `group`, …).
    pub fn op(&self) -> &'static str {
        match self {
            TransformStep::Filter { .. } => "filter",
            TransformStep::Select { .. } => "select",
            TransformStep::Rename { .. } => "rename",
            TransformStep::Compute { .. } => "compute",
            TransformStep::Group { .. } => "group",
            TransformStep::Join { .. } => "join",
            TransformStep::Sort { .. } => "sort",
            TransformStep::Limit { .. } => "limit",
            TransformStep::Pivot { .. } => "pivot",
            TransformStep::Unpivot { .. } => "unpivot",
        }
    }

    pub const OPS: [&'static str; 10] = [
        "filter", "select", "rename", "compute", "group", "join", "sort", "limit", "pivot",
        "unpivot",
    ];

    /// A new, empty step of kind `op`, with `column` filled in where a step
    /// needs one.
    pub fn new(op: &str, column: Option<&str>) -> Option<TransformStep> {
        let c = column.unwrap_or_default().to_string();
        Some(match op {
            "filter" => TransformStep::Filter {
                column: c,
                test: FilterTest::Eq,
                value: Value::Null,
                include_empty: false,
            },
            "select" => TransformStep::Select {
                columns: column.into_iter().map(str::to_string).collect(),
            },
            "rename" => TransformStep::Rename {
                column: c.clone(),
                to: format!("{c}_renamed"),
            },
            "compute" => TransformStep::Compute {
                name: "computed".into(),
                expression: String::new(),
            },
            "group" => TransformStep::Group {
                by: column.into_iter().map(str::to_string).collect(),
                aggregates: vec![AggregateColumn {
                    function: AggFunction::Count,
                    column: None,
                    alias: None,
                }],
            },
            "join" => TransformStep::Join {
                with: String::new(),
                kind: JoinKind::Inner,
                on: vec![JoinOn {
                    left: c.clone(),
                    right: c,
                }],
            },
            "sort" => TransformStep::Sort {
                by: vec![SortKey {
                    column: c,
                    descending: false,
                }],
            },
            "limit" => TransformStep::Limit { rows: 100 },
            "pivot" => TransformStep::Pivot {
                on: c.clone(),
                value: c,
                function: AggFunction::Sum,
                group_by: Vec::new(),
            },
            "unpivot" => TransformStep::Unpivot {
                columns: column.into_iter().map(str::to_string).collect(),
                name: "name".into(),
                value: "value".into(),
            },
            _ => return None,
        })
    }
}

impl TransformCellSpec {
    /// Keep `inputs` in step with the cells the join steps read.
    pub fn sync_inputs(&mut self) {
        let mut inputs: Vec<String> = Vec::new();
        for step in &self.steps {
            if let TransformStep::Join { with, .. } = step
                && !with.is_empty()
                && Some(with) != self.input.as_ref()
                && !inputs.contains(with)
            {
                inputs.push(with.clone());
            }
        }
        self.inputs = inputs;
    }

    /// Every cell the transform reads, main input first.
    pub fn all_inputs(&self) -> Vec<String> {
        let mut out: Vec<String> = self.input.iter().cloned().collect();
        for step in &self.steps {
            if let TransformStep::Join { with, .. } = step
                && !with.is_empty()
                && !out.contains(with)
            {
                out.push(with.clone());
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransformError {
    NoInput,
    /// A cell the transform reads has no local relation yet.
    UnknownInput {
        cell: String,
    },
    /// Step `index` (0-based) is missing a column or value it needs.
    Incomplete {
        index: usize,
        op: &'static str,
    },
    InvalidValue {
        index: usize,
    },
    InvalidExpression {
        index: usize,
    },
}

impl TransformError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            TransformError::NoInput => DisplayMessage::new(
                "notebook.visual.transform.errors.no_input",
                "Pick the result this transform starts from.",
            ),
            TransformError::UnknownInput { cell } => DisplayMessage::new(
                "notebook.visual.transform.errors.unknown_input",
                "Cell {cell_name} has no result to transform yet. Run it first.",
            )
            .with_arg("cell_name", cell.clone()),
            TransformError::Incomplete { index, op } => DisplayMessage::new(
                "notebook.visual.transform.errors.incomplete",
                "Step {step} ({operation}) is missing a column or value.",
            )
            .with_arg("step", (index + 1).to_string())
            .with_arg("operation", op.to_string()),
            TransformError::InvalidValue { index } => DisplayMessage::new(
                "notebook.visual.transform.errors.invalid_value",
                "Step {step} has a value that is not a number, text or true/false.",
            )
            .with_arg("step", (index + 1).to_string()),
            TransformError::InvalidExpression { index } => DisplayMessage::new(
                "notebook.visual.transform.errors.invalid_expression",
                "Step {step} needs a single expression, without semicolons or comments.",
            )
            .with_arg("step", (index + 1).to_string()),
        }
    }
}

/// The compiled query and the relations it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledTransform {
    pub sql: String,
    /// `(cell id, relation)` for every input, main input first.
    pub relations: Vec<(String, String)>,
}

/// A DuckDB identifier.
pub fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn string_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn literal(value: &Value) -> Option<String> {
    match value {
        Value::Null => Some("NULL".into()),
        Value::Bool(true) => Some("TRUE".into()),
        Value::Bool(false) => Some("FALSE".into()),
        Value::Number(n) => {
            let text = n.to_string();
            n.as_f64().filter(|f| f.is_finite()).map(|_| {
                if text.starts_with('-') {
                    format!("({text})")
                } else {
                    text
                }
            })
        }
        Value::String(s) => Some(string_literal(s)),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn values_of(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::String(s) if s.contains(',') => s
            .split(',')
            .map(|part| Value::String(part.trim().to_string()))
            .collect(),
        Value::Null => Vec::new(),
        other => vec![other.clone()],
    }
}

fn valid_expression(expression: &str) -> bool {
    let text = expression.trim();
    if text.is_empty()
        || text.chars().count() > MAX_EXPRESSION_CHARS
        || text.contains(';')
        || text.contains("--")
        || text.contains("/*")
    {
        return false;
    }
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    quote.is_none() && depth == 0
}

fn columns(names: &[String]) -> String {
    names
        .iter()
        .map(|n| ident(n))
        .collect::<Vec<_>>()
        .join(", ")
}

fn order_by(keys: &[SortKey]) -> String {
    keys.iter()
        .map(|k| {
            if k.descending {
                format!("{} DESC", ident(&k.column))
            } else {
                ident(&k.column)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Compile `spec` to DuckDB SQL. `relation` names the local relation each
/// input cell was materialized as, or `None` when it has not been.
pub fn compile(
    spec: &TransformCellSpec,
    relation: &dyn Fn(&str) -> Option<String>,
) -> Result<CompiledTransform, TransformError> {
    let input = spec.input.as_deref().ok_or(TransformError::NoInput)?;
    let mut relations: Vec<(String, String)> = Vec::new();
    let mut resolve = |cell: &str| -> Result<String, TransformError> {
        if let Some((_, rel)) = relations.iter().find(|(c, _)| c == cell) {
            return Ok(rel.clone());
        }
        let rel = relation(cell).ok_or_else(|| TransformError::UnknownInput {
            cell: cell.to_string(),
        })?;
        relations.push((cell.to_string(), rel.clone()));
        Ok(rel)
    };
    let base = ident(&resolve(input)?);
    let mut ctes: Vec<String> = Vec::new();
    let mut previous = base;
    // The latest sort, re-applied at the end while later steps keep it.
    let mut order: Option<String> = None;
    for (index, step) in spec.steps.iter().enumerate() {
        let incomplete = || TransformError::Incomplete {
            index,
            op: step.op(),
        };
        let from = previous.clone();
        let body = match step {
            TransformStep::Filter {
                column,
                test,
                value,
                include_empty,
            } => {
                if column.is_empty() {
                    return Err(incomplete());
                }
                let c = ident(column);
                let value_sql = || -> Result<String, TransformError> {
                    if value.is_null() {
                        return Err(incomplete());
                    }
                    literal(value).ok_or(TransformError::InvalidValue { index })
                };
                let text = || -> Result<String, TransformError> {
                    match value {
                        Value::Null => Err(incomplete()),
                        Value::String(s) => Ok(string_literal(s)),
                        Value::Number(n) => Ok(string_literal(&n.to_string())),
                        Value::Bool(b) => Ok(string_literal(&b.to_string())),
                        _ => Err(TransformError::InvalidValue { index }),
                    }
                };
                let condition = match test {
                    FilterTest::Eq => format!("{c} = {}", value_sql()?),
                    FilterTest::Ne if *include_empty => {
                        format!("({c} <> {} OR {c} IS NULL)", value_sql()?)
                    }
                    FilterTest::Ne => format!("{c} <> {}", value_sql()?),
                    FilterTest::Lt => format!("{c} < {}", value_sql()?),
                    FilterTest::Le => format!("{c} <= {}", value_sql()?),
                    FilterTest::Gt => format!("{c} > {}", value_sql()?),
                    FilterTest::Ge => format!("{c} >= {}", value_sql()?),
                    FilterTest::Contains => format!(
                        "strpos(lower(CAST({c} AS VARCHAR)), lower({})) > 0",
                        text()?
                    ),
                    FilterTest::StartsWith => format!(
                        "starts_with(lower(CAST({c} AS VARCHAR)), lower({}))",
                        text()?
                    ),
                    FilterTest::IsNull => format!("{c} IS NULL"),
                    FilterTest::NotNull => format!("{c} IS NOT NULL"),
                    FilterTest::In => {
                        let items = values_of(value)
                            .iter()
                            .map(|v| literal(v).ok_or(TransformError::InvalidValue { index }))
                            .collect::<Result<Vec<_>, _>>()?;
                        if items.is_empty() {
                            return Err(incomplete());
                        }
                        format!("{c} IN ({})", items.join(", "))
                    }
                };
                format!("SELECT * FROM {from} WHERE {condition}")
            }
            TransformStep::Select { columns: names } => {
                if names.is_empty() {
                    return Err(incomplete());
                }
                order = None;
                format!("SELECT {} FROM {from}", columns(names))
            }
            TransformStep::Rename { column, to } => {
                if column.is_empty() || to.trim().is_empty() {
                    return Err(incomplete());
                }
                order = None;
                format!(
                    "SELECT * RENAME ({} AS {}) FROM {from}",
                    ident(column),
                    ident(to.trim())
                )
            }
            TransformStep::Compute { name, expression } => {
                if name.trim().is_empty() {
                    return Err(incomplete());
                }
                if !valid_expression(expression) {
                    return Err(TransformError::InvalidExpression { index });
                }
                format!(
                    "SELECT *, ({}) AS {} FROM {from}",
                    expression.trim(),
                    ident(name.trim())
                )
            }
            TransformStep::Group { by, aggregates } => {
                if by.is_empty() && aggregates.is_empty() {
                    return Err(incomplete());
                }
                order = None;
                let mut select: Vec<String> = by.iter().map(|b| ident(b)).collect();
                // Only a count can run over "all rows"; SUM(*) is no total
                // and COUNT(DISTINCT *) is not a row count.
                if aggregates
                    .iter()
                    .any(|a| a.column.is_none() && a.function != AggFunction::Count)
                {
                    return Err(incomplete());
                }
                for a in aggregates {
                    select.push(format!(
                        "{} AS {}",
                        a.function.call(a.column.as_deref()),
                        ident(&a.output_name())
                    ));
                }
                if by.is_empty() {
                    format!("SELECT {} FROM {from}", select.join(", "))
                } else {
                    format!(
                        "SELECT {} FROM {from} GROUP BY {}",
                        select.join(", "),
                        columns(by)
                    )
                }
            }
            TransformStep::Join { with, kind, on } => {
                if with.is_empty() || on.is_empty() || on.iter().any(|k| k.left.is_empty()) {
                    return Err(incomplete());
                }
                order = None;
                let other = ident(&resolve(with)?);
                if on.iter().all(|k| k.left == k.right || k.right.is_empty()) {
                    let keys: Vec<String> = on.iter().map(|k| k.left.clone()).collect();
                    // Both sides aliased, so a cell joined with itself works.
                    format!(
                        "SELECT * FROM {from} AS l {} {other} AS r USING ({})",
                        kind.sql(),
                        columns(&keys)
                    )
                } else {
                    let condition = on
                        .iter()
                        .map(|k| {
                            let right = if k.right.is_empty() {
                                &k.left
                            } else {
                                &k.right
                            };
                            format!("l.{} = r.{}", ident(&k.left), ident(right))
                        })
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    format!(
                        "SELECT * FROM {from} AS l {} {other} AS r ON {condition}",
                        kind.sql()
                    )
                }
            }
            TransformStep::Sort { by } => {
                if by.is_empty() || by.iter().any(|k| k.column.is_empty()) {
                    return Err(incomplete());
                }
                let keys = order_by(by);
                order = Some(keys.clone());
                format!("SELECT * FROM {from} ORDER BY {keys}")
            }
            TransformStep::Limit { rows } => match &order {
                Some(keys) => format!("SELECT * FROM {from} ORDER BY {keys} LIMIT {rows}"),
                None => format!("SELECT * FROM {from} LIMIT {rows}"),
            },
            TransformStep::Pivot {
                on,
                value,
                function,
                group_by,
            } => {
                if on.is_empty() || value.is_empty() {
                    return Err(incomplete());
                }
                order = None;
                let mut sql = format!(
                    "PIVOT {from} ON {} USING {}",
                    ident(on),
                    function.call(Some(value))
                );
                if !group_by.is_empty() {
                    sql.push_str(&format!(" GROUP BY {}", columns(group_by)));
                }
                sql
            }
            TransformStep::Unpivot {
                columns: names,
                name,
                value,
            } => {
                if names.is_empty() || name.trim().is_empty() || value.trim().is_empty() {
                    return Err(incomplete());
                }
                order = None;
                format!(
                    "UNPIVOT {from} ON {} INTO NAME {} VALUE {}",
                    columns(names),
                    ident(name.trim()),
                    ident(value.trim())
                )
            }
        };
        let name = format!("step_{}", index + 1);
        ctes.push(format!("{name} AS (\n  {body}\n)"));
        previous = name;
    }
    let mut sql = String::new();
    if !ctes.is_empty() {
        sql.push_str("WITH ");
        sql.push_str(&ctes.join(",\n"));
        sql.push('\n');
    }
    sql.push_str(&format!("SELECT * FROM {previous}"));
    if let Some(keys) = order {
        sql.push_str(&format!(" ORDER BY {keys}"));
    }
    Ok(CompiledTransform { sql, relations })
}

/// One input handed to [`run_local`]: the relation to (re)create and the
/// output rows that fill it.
#[cfg(not(target_arch = "wasm32"))]
pub struct LocalInput {
    pub relation: String,
    pub columns: Vec<sqlly_core::ResultColumn>,
    pub rows: std::sync::Arc<Vec<Vec<sqlly_core::CellValue>>>,
    pub source: crate::local_analytics::SourceInfo,
}

/// Materialize `inputs` in `workspace` (replacing earlier copies) and run
/// the compiled transform there, read-only, capped at `max_rows`.
#[cfg(not(target_arch = "wasm32"))]
pub fn run_local(
    workspace: &sqlly_engines::duckdb::local_store::LocalWorkspace,
    compiled: &CompiledTransform,
    inputs: Vec<LocalInput>,
    max_rows: usize,
    cancel: &crate::local_analytics::CancelToken,
) -> Result<crate::local_analytics::LocalResultSet, crate::local_analytics::LocalStoreError> {
    use sqlly_engines::duckdb::local_store::MaterializeRequest;
    for input in inputs {
        workspace.materialize(
            &MaterializeRequest {
                relation: &input.relation,
                columns: &input.columns,
                source: &input.source,
                mode: crate::local_analytics::WriteMode::Replace,
            },
            crate::local_analytics::SnapshotRows::new(input.rows),
            cancel,
        )?;
    }
    let mut sets = workspace.query(&compiled.sql, Some(max_rows), cancel)?;
    Ok(sets
        .pop()
        .unwrap_or(crate::local_analytics::LocalResultSet {
            columns: Vec::new(),
            rows: Vec::new(),
            truncated: false,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sqlly_testkit::golden::GoldenTest;

    fn golden(name: &str, actual: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("notebook")
            .join("cells")
            .join("testdata")
            .join(name);
        assert!(
            GoldenTest::compare(actual, &path).unwrap_or(false),
            "{name} differs from its golden file; rerun with SQLLY_UPDATE_GOLDEN=1\n{actual}"
        );
    }

    fn relations(cell: &str) -> Option<String> {
        match cell {
            "c_orders" => Some("orders".into()),
            "c_customers" => Some("customers".into()),
            _ => None,
        }
    }

    fn spec(steps: serde_json::Value) -> TransformCellSpec {
        let mut spec: TransformCellSpec = super::super::spec::read(&json!({
            "input": "c_orders",
            "steps": steps,
        }));
        spec.sync_inputs();
        spec
    }

    #[test]
    fn every_step_kind_compiles_to_the_golden_sql() {
        let spec = spec(json!([
            {"op": "filter", "column": "amount", "test": "gt", "value": 100},
            {"op": "filter", "column": "status", "test": "in", "value": ["paid", "shipped"]},
            {"op": "filter", "column": "note", "test": "contains", "value": "it's"},
            {"op": "compute", "name": "net", "expression": "amount * (1 - discount)"},
            {"op": "join", "with": "c_customers", "kind": "left", "on": [{"left": "customer_id", "right": "id"}]},
            {"op": "group", "by": ["country"], "aggregates": [
                {"function": "sum", "column": "net"},
                {"function": "count"},
                {"function": "count_distinct", "column": "customer_id", "alias": "customers"}]},
            {"op": "rename", "column": "sum_net", "to": "revenue"},
            {"op": "sort", "by": [{"column": "revenue", "descending": true}, {"column": "country"}]},
            {"op": "limit", "rows": 10}
        ]));
        assert_eq!(spec.inputs, vec!["c_customers".to_string()]);
        let compiled = compile(&spec, &relations).unwrap();
        assert_eq!(
            compiled.relations,
            vec![
                ("c_orders".to_string(), "orders".to_string()),
                ("c_customers".to_string(), "customers".to_string())
            ]
        );
        golden("transform_all_steps.sql", &compiled.sql);
    }

    #[test]
    fn reshaping_steps_compile_to_the_golden_sql() {
        let spec = spec(json!([
            {"op": "join", "with": "c_customers", "on": [{"left": "customer_id", "right": "customer_id"}]},
            {"op": "select", "columns": ["country", "year", "amount"]},
            {"op": "pivot", "on": "year", "value": "amount", "function": "sum", "group_by": ["country"]},
            {"op": "unpivot", "columns": ["2025", "2026"], "name": "year", "value": "total"}
        ]));
        golden(
            "transform_reshape.sql",
            &compile(&spec, &relations).unwrap().sql,
        );
        let none = spec_with_no_steps();
        assert_eq!(
            compile(&none, &relations).unwrap().sql,
            "SELECT * FROM \"orders\""
        );
    }

    fn spec_with_no_steps() -> TransformCellSpec {
        TransformCellSpec {
            input: Some("c_orders".into()),
            ..TransformCellSpec::default()
        }
    }

    #[test]
    fn names_and_values_are_quoted_and_bad_steps_are_refused() {
        let hostile = spec(json!([
            {"op": "filter", "column": "a\"; DROP TABLE x; --", "test": "eq", "value": "x'; DROP TABLE y; --"}
        ]));
        let sql = compile(&hostile, &relations).unwrap().sql;
        assert!(
            sql.contains("\"a\"\"; DROP TABLE x; --\" = 'x''; DROP TABLE y; --'"),
            "{sql}"
        );

        let injected =
            spec(json!([{"op": "compute", "name": "x", "expression": "1); DROP TABLE t; --"}]));
        assert_eq!(
            compile(&injected, &relations),
            Err(TransformError::InvalidExpression { index: 0 })
        );
        let unbalanced = spec(json!([{"op": "compute", "name": "x", "expression": "(a + 'b"}]));
        assert!(compile(&unbalanced, &relations).is_err());
        let missing = spec(json!([{"op": "filter", "column": "a", "test": "gt"}]));
        assert_eq!(
            compile(&missing, &relations),
            Err(TransformError::Incomplete {
                index: 0,
                op: "filter"
            })
        );
        let null_test = spec(json!([{"op": "filter", "column": "a", "test": "is_null"}]));
        assert!(
            compile(&null_test, &relations)
                .unwrap()
                .sql
                .contains("\"a\" IS NULL")
        );
        let unknown = spec(
            json!([{"op": "join", "with": "c_nowhere", "on": [{"left": "id", "right": "id"}]}]),
        );
        assert_eq!(
            compile(&unknown, &relations),
            Err(TransformError::UnknownInput {
                cell: "c_nowhere".into()
            })
        );
        assert_eq!(
            compile(&TransformCellSpec::default(), &relations),
            Err(TransformError::NoInput)
        );
        let negative = spec(json!([{"op": "filter", "column": "a", "test": "lt", "value": -3}]));
        assert!(
            compile(&negative, &relations)
                .unwrap()
                .sql
                .contains("\"a\" < (-3)")
        );
        assert!(TransformError::NoInput.message().key().is_some());
    }

    #[test]
    fn steps_round_trip_and_new_steps_start_valid_where_they_can() {
        let spec = spec(json!([
            {"op": "limit", "rows": 5},
            {"op": "pivot", "on": "y", "value": "v"}
        ]));
        let json = super::super::spec::write(&spec);
        assert_eq!(super::super::spec::read::<TransformCellSpec>(&json), spec);
        for op in TransformStep::OPS {
            let step = TransformStep::new(op, Some("amount")).unwrap();
            assert_eq!(step.op(), op);
        }
        assert!(TransformStep::new("explode", None).is_none());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn compiled_transforms_run_in_a_local_workspace() {
        use sqlly_core::{CellValue, ResultColumn};
        use sqlly_engines::duckdb::local_store::{LocalWorkspace, WorkspaceOptions};
        use sqlly_lang::sqlmodel::DatabaseType;

        use crate::local_analytics::{CancelToken, SourceInfo};

        let root = tempfile::tempdir().unwrap();
        let workspace =
            LocalWorkspace::open("nbtransform", root.path(), WorkspaceOptions::default()).unwrap();
        let col = |name: &str, data_type: &str| ResultColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        };
        let orders = LocalInput {
            relation: "orders".into(),
            columns: vec![
                col("customer_id", "int"),
                col("amount", "float"),
                col("year", "int"),
            ],
            rows: std::sync::Arc::new(vec![
                vec![
                    CellValue::I64(1),
                    CellValue::F64(120.0),
                    CellValue::I64(2025),
                ],
                vec![
                    CellValue::I64(1),
                    CellValue::F64(80.0),
                    CellValue::I64(2026),
                ],
                vec![
                    CellValue::I64(2),
                    CellValue::F64(300.0),
                    CellValue::I64(2026),
                ],
                vec![
                    CellValue::I64(3),
                    CellValue::F64(50.0),
                    CellValue::I64(2026),
                ],
            ]),
            source: SourceInfo::query(DatabaseType::SqlServer, "Dev"),
        };
        let customers = LocalInput {
            relation: "customers".into(),
            columns: vec![col("id", "int"), col("country", "varchar")],
            rows: std::sync::Arc::new(vec![
                vec![CellValue::I64(1), CellValue::String("Germany".into())],
                vec![CellValue::I64(2), CellValue::String("France".into())],
                vec![CellValue::I64(3), CellValue::String("Germany".into())],
            ]),
            source: SourceInfo::query(DatabaseType::SqlServer, "Dev"),
        };
        let grouped = spec(json!([
            {"op": "filter", "column": "amount", "test": "ge", "value": 60},
            {"op": "join", "with": "c_customers", "kind": "left", "on": [{"left": "customer_id", "right": "id"}]},
            {"op": "compute", "name": "net", "expression": "amount * 0.5"},
            {"op": "group", "by": ["country"], "aggregates": [{"function": "sum", "column": "net"}]},
            {"op": "rename", "column": "sum_net", "to": "revenue"},
            {"op": "sort", "by": [{"column": "revenue", "descending": true}]},
            {"op": "limit", "rows": 5}
        ]));
        let compiled = compile(&grouped, &relations).unwrap();
        let out = run_local(
            &workspace,
            &compiled,
            vec![orders, customers],
            100,
            &CancelToken::new(),
        )
        .unwrap();
        let names: Vec<&str> = out.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["country", "revenue"]);
        assert_eq!(out.rows.len(), 2);
        assert_eq!(out.rows[0][0], CellValue::String("France".into()));

        let reshaped = spec(json!([
            {"op": "pivot", "on": "year", "value": "amount", "function": "sum", "group_by": ["customer_id"]},
            {"op": "unpivot", "columns": ["2025", "2026"], "name": "year", "value": "total"},
            {"op": "filter", "column": "total", "test": "not_null"},
            {"op": "sort", "by": [{"column": "customer_id"}, {"column": "year"}]}
        ]));
        let compiled = compile(&reshaped, &relations).unwrap();
        let out = run_local(&workspace, &compiled, Vec::new(), 100, &CancelToken::new()).unwrap();
        assert_eq!(out.rows.len(), 4, "{:?}", out.rows);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn typed_filter_text_matches_text_and_number_columns_and_self_joins_run() {
        use sqlly_core::{CellValue, ResultColumn};
        use sqlly_engines::duckdb::local_store::{LocalWorkspace, WorkspaceOptions};
        use sqlly_lang::sqlmodel::DatabaseType;

        use crate::local_analytics::{CancelToken, SourceInfo};

        let root = tempfile::tempdir().unwrap();
        let workspace =
            LocalWorkspace::open("nbtransformtyped", root.path(), WorkspaceOptions::default())
                .unwrap();
        let col = |name: &str, data_type: &str| ResultColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable: true,
        };
        let text = |t: &str| CellValue::String(t.into());
        let orders = LocalInput {
            relation: "orders".into(),
            columns: vec![
                col("zip", "varchar"),
                col("year", "varchar"),
                col("id", "bigint"),
            ],
            rows: std::sync::Arc::new(vec![
                vec![
                    text("007"),
                    text("2024"),
                    CellValue::I64(1234567890123456789),
                ],
                vec![text("7"), text("2025"), CellValue::I64(1234567890123456788)],
            ]),
            source: SourceInfo::query(DatabaseType::SqlServer, "Dev"),
        };
        let run = |steps: serde_json::Value, inputs: Vec<LocalInput>| {
            let compiled = compile(&spec(steps), &relations).unwrap();
            run_local(&workspace, &compiled, inputs, 100, &CancelToken::new())
                .unwrap_or_else(|e| panic!("{}: {e:?}", compiled.sql))
        };
        // Values as the filter box hands them over: the text typed.
        let out = run(
            json!([{"op": "filter", "column": "zip", "test": "eq", "value": "007"}]),
            vec![orders],
        );
        assert_eq!(out.rows.len(), 1, "007 is not 7 in a text column");
        let out = run(
            json!([{"op": "filter", "column": "year", "test": "in", "value": ["2024", "2026"]}]),
            Vec::new(),
        );
        assert_eq!(out.rows.len(), 1);
        let out = run(
            json!([{"op": "filter", "column": "id", "test": "eq", "value": "1234567890123456789"}]),
            Vec::new(),
        );
        assert_eq!(out.rows.len(), 1, "a 19-digit id keeps every digit");
        let out = run(
            json!([{"op": "filter", "column": "id", "test": "gt", "value": "1234567890123456788"}]),
            Vec::new(),
        );
        assert_eq!(
            out.rows.len(),
            1,
            "text compares as the column's number type"
        );
        let out = run(
            json!([{"op": "join", "with": "c_orders", "kind": "inner", "on": [{"left": "zip", "right": "zip"}]}]),
            Vec::new(),
        );
        assert_eq!(out.rows.len(), 2, "a cell joins with itself");

        let sum_all = spec(json!([
            {"op": "group", "by": ["zip"], "aggregates": [{"function": "sum"}]}
        ]));
        assert!(matches!(
            compile(&sum_all, &relations),
            Err(TransformError::Incomplete { .. })
        ));
        let distinct_all = spec(json!([
            {"op": "group", "by": ["zip"], "aggregates": [{"function": "count_distinct"}]}
        ]));
        assert!(compile(&distinct_all, &relations).is_err());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn text_tests_ignore_case_and_not_equal_keeps_empty_values_only_when_asked() {
        use sqlly_core::{CellValue, ResultColumn};
        use sqlly_engines::duckdb::local_store::{LocalWorkspace, WorkspaceOptions};
        use sqlly_lang::sqlmodel::DatabaseType;

        use crate::local_analytics::{CancelToken, SourceInfo};

        let root = tempfile::tempdir().unwrap();
        let workspace =
            LocalWorkspace::open("nbtransformnull", root.path(), WorkspaceOptions::default())
                .unwrap();
        let text = |t: &str| CellValue::String(t.into());
        let orders = LocalInput {
            relation: "orders".into(),
            columns: vec![ResultColumn {
                name: "city".into(),
                data_type: "varchar".into(),
                nullable: true,
            }],
            rows: std::sync::Arc::new(vec![
                vec![text("Berlin")],
                vec![text("bern")],
                vec![text("Paris")],
                vec![CellValue::Null],
            ]),
            source: SourceInfo::query(DatabaseType::SqlServer, "Dev"),
        };
        let mut inputs = vec![orders];
        let mut run = |steps: serde_json::Value| {
            let compiled = compile(&spec(steps), &relations).unwrap();
            run_local(
                &workspace,
                &compiled,
                std::mem::take(&mut inputs),
                100,
                &CancelToken::new(),
            )
            .unwrap_or_else(|e| panic!("{}: {e:?}", compiled.sql))
            .rows
            .len()
        };
        assert_eq!(
            run(json!([{"op": "filter", "column": "city", "test": "starts_with", "value": "BER"}])),
            2,
            "starts with ignores case, as contains does"
        );
        assert_eq!(
            run(json!([{"op": "filter", "column": "city", "test": "contains", "value": "ER"}])),
            2
        );
        assert_eq!(
            run(json!([{"op": "filter", "column": "city", "test": "ne", "value": "Paris"}])),
            2,
            "not equal leaves empty values out by default"
        );
        assert_eq!(
            run(
                json!([{"op": "filter", "column": "city", "test": "ne", "value": "Paris", "include_empty": true}])
            ),
            3,
            "and keeps them when asked"
        );
        let step: TransformStep = serde_json::from_value(
            json!({"op": "filter", "column": "city", "test": "ne", "value": "x"}),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&step).unwrap(),
            json!({"op": "filter", "column": "city", "test": "ne", "value": "x"}),
            "the default is not written"
        );
        assert!(FilterTest::Ne.offers_include_empty() && !FilterTest::Eq.offers_include_empty());
    }
}
