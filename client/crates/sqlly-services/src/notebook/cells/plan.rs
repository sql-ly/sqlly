//! Query plan cell spec, session statistics and parameter sweeps.
//!
//! ```json
//! {"input": "orders", "mode": "estimated", "session_stats": true,
//!  "compare": {"cell": "orders_v2"},
//!  "sweep": {"parameter": "region", "values": ["EMEA", "APAC"], "repetitions": 3}}
//! ```
//!
//! `mode` is `estimated` (default) or `actual`; an actual plan executes the
//! query, so the app asks before every actual run and never stores that
//! consent. `compare` is `{"cell": id}` (a second query variant) or
//! `{"run": "previous" | "<run id>"}` (this cell's plan in an earlier run).
//! A sweep runs the input `repetitions` times per value; it is checked
//! with `notebook_security::authorize(Benchmark)`, which refuses a
//! production connection unless the user authorizes that run.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlly_core::{CellValue, ResultColumn};
use sqlly_lang::sqlmodel::DatabaseType;

use super::{input_of, unknown_fields};
use crate::plans::{ExecutionPlan, PlanNode};

const KNOWN: &[&str] = &["input", "mode", "session_stats", "compare", "sweep"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanMode {
    #[default]
    Estimated,
    Actual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanCompare {
    /// Another SQL cell's plan, captured in the same run.
    Cell(String),
    PreviousRun,
    Run(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepSpec {
    pub parameter: String,
    #[serde(default)]
    pub values: Vec<Value>,
    #[serde(default = "one")]
    pub repetitions: u32,
}

fn one() -> u32 {
    1
}

/// Most executions one sweep may make (values × repetitions).
pub const MAX_SWEEP_RUNS: usize = 200;
pub const MAX_REPETITIONS: u32 = 20;

impl SweepSpec {
    pub fn repetitions(&self) -> u32 {
        self.repetitions.clamp(1, MAX_REPETITIONS)
    }

    pub fn total_runs(&self) -> usize {
        self.values.len() * self.repetitions() as usize
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanCellSpec {
    pub input: String,
    pub mode: PlanMode,
    pub session_stats: bool,
    pub compare: Option<PlanCompare>,
    pub sweep: Option<SweepSpec>,
    pub extra: Map<String, Value>,
}

impl PlanCellSpec {
    pub fn from_spec(spec: &Value) -> Self {
        let compare = spec.get("compare").and_then(|c| {
            if let Some(cell) = c.get("cell").and_then(Value::as_str) {
                return Some(PlanCompare::Cell(cell.to_string()));
            }
            match c.get("run").and_then(Value::as_str)? {
                "previous" => Some(PlanCompare::PreviousRun),
                id => Some(PlanCompare::Run(id.to_string())),
            }
        });
        Self {
            input: input_of(spec),
            mode: spec
                .get("mode")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default(),
            session_stats: spec
                .get("session_stats")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            compare,
            sweep: spec
                .get("sweep")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            extra: unknown_fields(spec, KNOWN),
        }
    }

    pub fn to_spec(&self) -> Value {
        let mut map = self.extra.clone();
        map.insert("input".into(), Value::String(self.input.clone()));
        if self.mode == PlanMode::Actual {
            map.insert("mode".into(), "actual".into());
        }
        if self.session_stats {
            map.insert("session_stats".into(), true.into());
        }
        if let Some(c) = &self.compare {
            let v = match c {
                PlanCompare::Cell(id) => serde_json::json!({ "cell": id }),
                PlanCompare::PreviousRun => serde_json::json!({ "run": "previous" }),
                PlanCompare::Run(id) => serde_json::json!({ "run": id }),
            };
            map.insert("compare".into(), v);
        }
        if let Some(s) = &self.sweep {
            map.insert(
                "sweep".into(),
                serde_json::to_value(s).unwrap_or(Value::Null),
            );
        }
        Value::Object(map)
    }

    /// Cells the plan cell reads (its input and a compared variant).
    pub fn inputs(&self) -> Vec<String> {
        let mut out = vec![self.input.clone()];
        if let Some(PlanCompare::Cell(id)) = &self.compare {
            out.push(id.clone());
        }
        out.retain(|s| !s.is_empty());
        out
    }
}

/// Timing summary of repeated runs.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct TimingStats {
    pub runs: u32,
    pub mean_ms: f64,
    /// Sample standard deviation (0 for one run).
    pub stddev_ms: f64,
    pub min_ms: f64,
    pub max_ms: f64,
    pub median_ms: f64,
}

impl TimingStats {
    pub fn of(timings_ms: &[f64]) -> Self {
        let n = timings_ms.len();
        if n == 0 {
            return Self::default();
        }
        let mean = timings_ms.iter().sum::<f64>() / n as f64;
        let var = if n > 1 {
            timings_ms.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / (n - 1) as f64
        } else {
            0.0
        };
        let mut sorted = timings_ms.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = if n % 2 == 1 {
            sorted[n / 2]
        } else {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        };
        Self {
            runs: n as u32,
            mean_ms: mean,
            stddev_ms: var.sqrt(),
            min_ms: sorted[0],
            max_ms: sorted[n - 1],
            median_ms: median,
        }
    }

    /// Coefficient of variation (stddev ÷ mean).
    pub fn variation(&self) -> f64 {
        if self.mean_ms > 0.0 {
            self.stddev_ms / self.mean_ms
        } else {
            0.0
        }
    }
}

/// How two timing summaries compare once run-to-run noise is allowed for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingVerdict {
    Faster,
    Slower,
    /// The difference is within two standard errors.
    WithinNoise,
    /// Too few runs to tell.
    NotEnoughRuns,
}

pub fn compare_timings(baseline: &TimingStats, other: &TimingStats) -> TimingVerdict {
    if baseline.runs < 2 || other.runs < 2 {
        return TimingVerdict::NotEnoughRuns;
    }
    let se = (baseline.stddev_ms.powi(2) / baseline.runs as f64
        + other.stddev_ms.powi(2) / other.runs as f64)
        .sqrt();
    let delta = other.mean_ms - baseline.mean_ms;
    if delta.abs() <= 2.0 * se {
        TimingVerdict::WithinNoise
    } else if delta < 0.0 {
        TimingVerdict::Faster
    } else {
        TimingVerdict::Slower
    }
}

/// Where session counters come from on an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatsSource {
    /// `sys.dm_exec_sessions` for the current session (SQL Server).
    DmExecSessions,
    /// `SHOW SESSION STATUS` (MySQL, MariaDB).
    SessionStatus,
}

impl StatsSource {
    pub fn label(self) -> &'static str {
        match self {
            StatsSource::DmExecSessions => "sys.dm_exec_sessions",
            StatsSource::SessionStatus => "SHOW SESSION STATUS",
        }
    }
}

const MYSQL_COUNTERS: [&str; 8] = [
    "Handler_read_key",
    "Handler_read_next",
    "Handler_read_rnd_next",
    "Innodb_rows_read",
    "Select_scan",
    "Sort_rows",
    "Created_tmp_tables",
    "Bytes_sent",
];

/// The read-only counter query for `db`, run before and after the
/// measured query on the same session.
pub fn session_stats_query(db: DatabaseType) -> Option<(StatsSource, String)> {
    use sqlly_core::ConnectionKind;
    match db.connection_kind() {
        ConnectionKind::Tds => Some((
            StatsSource::DmExecSessions,
            "SELECT cpu_time, reads, writes, logical_reads, total_elapsed_time, row_count \
             FROM sys.dm_exec_sessions WHERE session_id = @@SPID"
                .to_string(),
        )),
        ConnectionKind::MySqlWire => Some((
            StatsSource::SessionStatus,
            format!(
                "SHOW SESSION STATUS WHERE Variable_name IN ({})",
                MYSQL_COUNTERS
                    .iter()
                    .map(|c| format!("'{c}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
        _ => None,
    }
}

/// One counter's change across the measured query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatDelta {
    pub name: String,
    pub before: f64,
    pub after: f64,
}

impl StatDelta {
    pub fn delta(&self) -> f64 {
        self.after - self.before
    }
}

fn number(value: &CellValue) -> Option<f64> {
    match value {
        CellValue::I64(n) => Some(*n as f64),
        CellValue::F64(f) => Some(*f),
        CellValue::String(s) => s.trim().parse().ok(),
        CellValue::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    }
}

fn counters(
    source: StatsSource,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
) -> Vec<(String, f64)> {
    match source {
        StatsSource::DmExecSessions => rows
            .first()
            .map(|row| {
                columns
                    .iter()
                    .zip(row)
                    .filter_map(|(c, v)| Some((c.name.clone(), number(v)?)))
                    .collect()
            })
            .unwrap_or_default(),
        StatsSource::SessionStatus => rows
            .iter()
            .filter_map(|row| {
                let name = match row.first()? {
                    CellValue::String(s) => s.clone(),
                    _ => return None,
                };
                Some((name, number(row.get(1)?)?))
            })
            .collect(),
    }
}

/// Counter deltas from the before and after snapshots.
pub fn stat_deltas(
    source: StatsSource,
    before: (&[ResultColumn], &[Vec<CellValue>]),
    after: (&[ResultColumn], &[Vec<CellValue>]),
) -> Vec<StatDelta> {
    let old = counters(source, before.0, before.1);
    counters(source, after.0, after.1)
        .into_iter()
        .filter_map(|(name, after)| {
            let before = old.iter().find(|(n, _)| n.eq_ignore_ascii_case(&name))?.1;
            Some(StatDelta {
                name,
                before,
                after,
            })
        })
        .collect()
}

/// The plan's operators as an output table (one row per node, depth-first
/// from the root) so downstream cells and run history can read it.
pub fn plan_table(plans: &[ExecutionPlan]) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let col = |name: &str, ty: &str| ResultColumn {
        name: name.to_string(),
        data_type: ty.to_string(),
        nullable: true,
    };
    let columns = vec![
        col("statement", "int"),
        col("node_id", "int"),
        col("operator", "nvarchar"),
        col("estimated_rows", "float"),
        col("estimated_cost", "float"),
        col("actual_rows", "float"),
        col("warnings", "int"),
    ];
    let mut rows = Vec::new();
    for (s, plan) in plans.iter().enumerate() {
        for node in &plan.nodes {
            rows.push(vec![
                CellValue::I64(s as i64 + 1),
                CellValue::I64(i64::from(node.id)),
                CellValue::String(operator_name(node)),
                CellValue::F64(node.estimated_rows),
                CellValue::F64(node.estimated_cost),
                node.actual_rows.map_or(CellValue::Null, CellValue::F64),
                CellValue::I64(node.warnings.len() as i64),
            ]);
        }
    }
    (columns, rows)
}

pub fn operator_name(node: &PlanNode) -> String {
    if node.logical_op.is_empty() || node.logical_op == node.physical_op {
        node.physical_op.clone()
    } else {
        format!("{} ({})", node.physical_op, node.logical_op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sweep(values: usize, reps: u32) -> SweepSpec {
        SweepSpec {
            parameter: "p".into(),
            values: (0..values).map(|v| json!(v)).collect(),
            repetitions: reps,
        }
    }

    #[test]
    fn spec_round_trips() {
        let raw = json!({"input": "q", "mode": "actual", "session_stats": true,
                         "compare": {"run": "previous"},
                         "sweep": {"parameter": "region", "values": ["a"], "repetitions": 3}});
        let spec = PlanCellSpec::from_spec(&raw);
        assert_eq!(spec.mode, PlanMode::Actual);
        assert_eq!(spec.compare, Some(PlanCompare::PreviousRun));
        assert_eq!(spec.to_spec(), raw);
        let variant = PlanCellSpec::from_spec(&json!({"input": "q", "compare": {"cell": "q2"}}));
        assert_eq!(variant.inputs(), vec!["q".to_string(), "q2".to_string()]);
        assert_eq!(variant.mode, PlanMode::Estimated);
    }

    #[test]
    fn sweep_runs_are_values_times_clamped_repetitions() {
        assert_eq!(sweep(3, 2).total_runs(), 6);
        assert_eq!(sweep(0, 4).total_runs(), 0);
        assert_eq!(sweep(1, 99).repetitions(), MAX_REPETITIONS);
    }

    #[test]
    fn timing_comparisons_allow_for_noise() {
        let a = TimingStats::of(&[100.0, 102.0, 98.0, 101.0]);
        assert_eq!(a.runs, 4);
        assert!((a.mean_ms - 100.25).abs() < 1e-9);
        assert_eq!(a.median_ms, 100.5);
        let noisy = TimingStats::of(&[95.0, 110.0, 99.0, 104.0]);
        assert_eq!(compare_timings(&a, &noisy), TimingVerdict::WithinNoise);
        let slow = TimingStats::of(&[200.0, 205.0, 198.0, 202.0]);
        assert_eq!(compare_timings(&a, &slow), TimingVerdict::Slower);
        assert_eq!(compare_timings(&slow, &a), TimingVerdict::Faster);
        let single = TimingStats::of(&[5.0]);
        assert_eq!(single.stddev_ms, 0.0);
        assert_eq!(compare_timings(&single, &a), TimingVerdict::NotEnoughRuns);
    }

    #[test]
    fn session_counters_are_per_engine_and_read_only() {
        let (source, sql) = session_stats_query(DatabaseType::SqlServer).unwrap();
        assert_eq!(source.label(), "sys.dm_exec_sessions");
        assert!(sql.starts_with("SELECT"));
        let (source, sql) = session_stats_query(DatabaseType::MySql).unwrap();
        assert_eq!(source, StatsSource::SessionStatus);
        assert!(sql.starts_with("SHOW SESSION STATUS"));
        assert!(session_stats_query(DatabaseType::Postgres).is_none());

        let c = |n: &str| ResultColumn {
            name: n.into(),
            data_type: "int".into(),
            nullable: false,
        };
        let cols = [c("cpu_time"), c("reads")];
        let before = vec![vec![CellValue::I64(10), CellValue::I64(100)]];
        let after = vec![vec![CellValue::I64(25), CellValue::I64(160)]];
        let d = stat_deltas(
            StatsSource::DmExecSessions,
            (&cols, &before),
            (&cols, &after),
        );
        assert_eq!(d.len(), 2);
        assert_eq!(d[1].delta(), 60.0);
        let kv = [c("Variable_name"), c("Value")];
        let s = |t: &str| CellValue::String(t.into());
        let d = stat_deltas(
            StatsSource::SessionStatus,
            (&kv, &[vec![s("Handler_read_key"), s("5")]]),
            (&kv, &[vec![s("Handler_read_key"), s("9")]]),
        );
        assert_eq!(d[0].delta(), 4.0);
    }
}
