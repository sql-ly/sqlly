//! Cell outputs (contract #5): the only way non-SQL cells read upstream
//! data. Rows are an `Arc` snapshot capped at the output row cap, so
//! sharing an output between cells and threads never copies it.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sqlly_core::{CellValue, ResultColumn};

use super::fingerprint::Fingerprint;

/// Rows kept in an output snapshot when the cell's policy sets no
/// `max_rows`.
pub const DEFAULT_OUTPUT_ROW_CAP: usize = 10_000;

/// How much of the result the snapshot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Completeness {
    /// Every row the query produced.
    Complete,
    /// The first `cap` rows; more existed.
    Truncated { cap: u64 },
    /// `total` rows exist, the rest spilled to disk.
    Spilled { total: u64 },
    /// A sample of `n` rows.
    Sampled { n: u64 },
    /// The run stopped early (canceled or failed mid-stream).
    Partial,
    /// Every row came back, but the query bound a value (a scalar cell)
    /// that was itself computed from truncated, sampled or partial data.
    DerivedFromPartial,
}

impl Completeness {
    pub fn is_complete(self) -> bool {
        self == Completeness::Complete
    }

    /// Whether totals, averages or charts over the snapshot describe the
    /// whole result. Sampled, truncated and partial data must be labelled.
    pub fn supports_summaries(self) -> bool {
        matches!(self, Completeness::Complete | Completeness::Spilled { .. })
    }
}

/// On what grounds the output is believed current.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    /// Fetched by a run in this session.
    #[default]
    Live,
    /// Reused under a TTL: not proof the source is unchanged.
    TtlCached,
    /// Kept because the user pinned it.
    Pinned,
    /// Reused because the source reported an unchanged data token.
    SourceVerified,
    /// Loaded from a saved snapshot.
    Snapshot,
}

/// Where the data came from.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Provenance {
    pub connection_label: String,
    pub database: Option<String>,
    /// `production`, `staging`, `development` or `unknown`.
    pub environment: Option<String>,
    pub extracted_at: Option<DateTime<Utc>>,
    /// SHA-256 of the exact SQL sent (hex), when there was SQL.
    pub sql_hash: Option<String>,
    /// Upstream provenance for outputs derived from other cells.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Provenance>,
}

fn ser_rows<S: Serializer>(rows: &Arc<Vec<Vec<CellValue>>>, s: S) -> Result<S::Ok, S::Error> {
    rows.as_ref().serialize(s)
}

fn de_rows<'de, D: Deserializer<'de>>(d: D) -> Result<Arc<Vec<Vec<CellValue>>>, D::Error> {
    Vec::<Vec<CellValue>>::deserialize(d).map(Arc::new)
}

/// One cell's output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellOutput {
    pub columns: Vec<ResultColumn>,
    #[serde(serialize_with = "ser_rows", deserialize_with = "de_rows")]
    pub rows: Arc<Vec<Vec<CellValue>>>,
    pub completeness: Completeness,
    /// Rows the query produced (at least `rows.len()`).
    pub row_count: u64,
    pub elapsed_ms: u64,
    pub run_id: String,
    pub fingerprint: Fingerprint,
    pub provenance: Provenance,
    #[serde(default)]
    pub freshness: Freshness,
}

impl CellOutput {
    /// Build an output from all fetched rows, keeping the first `cap` and
    /// marking the snapshot truncated when there were more. `total` is the
    /// producer's row count when it saw more rows than it delivered.
    #[allow(clippy::too_many_arguments)]
    pub fn capped(
        columns: Vec<ResultColumn>,
        mut rows: Vec<Vec<CellValue>>,
        total: Option<u64>,
        cap: usize,
        elapsed_ms: u64,
        run_id: impl Into<String>,
        fingerprint: Fingerprint,
        provenance: Provenance,
    ) -> Self {
        let row_count = total.unwrap_or(rows.len() as u64).max(rows.len() as u64);
        let completeness = if row_count > cap as u64 {
            Completeness::Truncated { cap: cap as u64 }
        } else {
            Completeness::Complete
        };
        rows.truncate(cap);
        Self {
            columns,
            rows: Arc::new(rows),
            completeness,
            row_count,
            elapsed_ms,
            run_id: run_id.into(),
            fingerprint,
            provenance,
            freshness: Freshness::Live,
        }
    }

    /// The column index named `name` (case-insensitive).
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// A copy limited to `rows` rows, for embedding in a saved notebook.
    pub fn snapshot(&self, rows: usize) -> CellOutput {
        let mut out = self.clone();
        if self.rows.len() > rows {
            out.rows = Arc::new(self.rows[..rows].to_vec());
            out.completeness = Completeness::Truncated { cap: rows as u64 };
        }
        out.freshness = Freshness::Snapshot;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: "int".into(),
            nullable: false,
        }
    }

    fn rows(n: i64) -> Vec<Vec<CellValue>> {
        (0..n).map(|i| vec![CellValue::I64(i)]).collect()
    }

    fn out(n: i64, total: Option<u64>, cap: usize) -> CellOutput {
        CellOutput::capped(
            vec![col("N")],
            rows(n),
            total,
            cap,
            5,
            "r_1",
            Fingerprint("f".into()),
            Provenance::default(),
        )
    }

    #[test]
    fn capping_marks_truncation() {
        let small = out(3, None, 10);
        assert_eq!(small.completeness, Completeness::Complete);
        assert_eq!(small.row_count, 3);
        let big = out(25, None, 10);
        assert_eq!(big.rows.len(), 10);
        assert_eq!(big.row_count, 25);
        assert_eq!(big.completeness, Completeness::Truncated { cap: 10 });
        let partial_fetch = out(10, Some(1_000), 10);
        assert_eq!(
            partial_fetch.completeness,
            Completeness::Truncated { cap: 10 }
        );
        assert_eq!(partial_fetch.row_count, 1_000);
        assert!(!Completeness::Sampled { n: 5 }.supports_summaries());
        assert!(!Completeness::Partial.supports_summaries());
        assert!(Completeness::Spilled { total: 9 }.supports_summaries());
    }

    #[test]
    fn outputs_share_rows_and_round_trip() {
        let o = out(4, None, 10);
        let copy = o.clone();
        assert!(Arc::ptr_eq(&o.rows, &copy.rows));
        assert_eq!(o.column_index("n"), Some(0));
        let json = serde_json::to_string(&o).unwrap();
        assert!(json.contains("\"kind\":\"complete\""));
        let back: CellOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(back, o);
        let snap = o.snapshot(2);
        assert_eq!(snap.rows.len(), 2);
        assert_eq!(snap.completeness, Completeness::Truncated { cap: 2 });
        assert_eq!(snap.freshness, Freshness::Snapshot);
    }
}
