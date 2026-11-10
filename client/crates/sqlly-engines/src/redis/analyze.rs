//! Memory analysis for one logical database: a bounded `SCAN` sample with
//! `TYPE` / `TTL` / `MEMORY USAGE … SAMPLES 5` probed per key, aggregated
//! into the memory analyzer's reports — memory by data type, top keys by
//! memory, and the TTL distribution — plus a one-row summary of the sample
//! itself (size, completeness, whether bytes could be measured).
//!
//! The sample is bounded by design (never `KEYS *`, never the whole
//! keyspace on a busy server, never more than [`MAX_ANALYSIS_SAMPLE`] keys),
//! so every report carries its sampled counts next to the estimates
//! extrapolated linearly to the database's `DBSIZE`. One script run takes
//! ONE sample per (database, size) and derives every report from it (see
//! [`AnalysisCache`]), so the reports agree with each other and the server
//! is walked once. The aggregation is pure and unit-tested.

use std::sync::Arc;

use redis::Value;
use sqlly_core::{StudioError, StudioResult};

use super::RedisClient;
use super::browse::KeyInfo;

/// Keys one analysis pass samples when no size is given.
pub const DEFAULT_ANALYSIS_SAMPLE: usize = 1000;
/// The largest sample a script may ask for; bigger requests are clamped.
pub const MAX_ANALYSIS_SAMPLE: usize = 50_000;
/// Keys the top-keys report lists when no count is given.
pub const DEFAULT_TOP_KEYS: usize = 25;
/// `MEMORY USAGE … SAMPLES n` depth for nested types (matches Disk Usage).
const USAGE_SAMPLES: usize = 5;
/// `MEMORY USAGE` probes per pipeline round trip.
const USAGE_CHUNK: usize = 200;

/// One sampled key: its type, TTL, and the bytes `MEMORY USAGE` reports
/// (`None` only when the server cannot measure memory at all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySample {
    pub key: Vec<u8>,
    pub kind: String,
    pub ttl_seconds: Option<i64>,
    pub bytes: Option<i64>,
}

/// What one pass over the current database saw.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnalysisSample {
    /// Keys that still existed when probed. Keys that vanished between the
    /// `SCAN` and the probes are dropped, not reported as a `none` type.
    pub keys: Vec<KeySample>,
    /// `DBSIZE` when the sample was taken (the estimates' base).
    pub total_keys: i64,
    /// The `SCAN` walk finished: the sample covers the whole keyspace and
    /// the estimates are exact.
    pub complete: bool,
    /// Keys the walk saw that were gone by the time they were probed.
    pub vanished: usize,
    /// Why `MEMORY USAGE` could not be used (unknown command, `NOPERM`):
    /// every `bytes` is `None`, and byte-based reports must say so rather
    /// than show zeros. `None` when bytes were measured.
    pub memory_unavailable: Option<sqlly_core::DisplayMessage>,
}

impl AnalysisSample {
    /// `MEMORY USAGE` could not measure this sample.
    pub fn memory_unsupported(&self) -> bool {
        self.memory_unavailable.is_some()
    }
}

/// Why a `MEMORY USAGE` failure means "this server or user cannot measure
/// memory" rather than a real error: an unknown command or subcommand (old
/// or restricted servers, renamed commands) or an ACL `NOPERM`. Any other
/// error reply is NOT swallowed — the caller propagates it.
pub fn memory_unavailable_message(error: &StudioError) -> Option<sqlly_core::DisplayMessage> {
    use sqlly_core::DisplayMessage;
    match error {
        StudioError::PermissionDenied(text) => Some(
            DisplayMessage::new(
                "services.redis.memory.permission_denied",
                format!(
                    "MEMORY USAGE is not permitted for this user ({})",
                    text.trim()
                ),
            )
            .with_arg("detail", text.trim()),
        ),
        StudioError::QueryExecution(text) => {
            let lower = text.to_ascii_lowercase();
            (lower.contains("unknown command") || lower.contains("unknown subcommand")).then(|| {
                DisplayMessage::new(
                    "services.redis.memory.unsupported",
                    format!(
                        "MEMORY USAGE is not supported by this server ({})",
                        text.trim()
                    ),
                )
                .with_arg("detail", text.trim())
            })
        }
        _ => None,
    }
}

pub fn memory_unavailable_reason(error: &StudioError) -> Option<String> {
    memory_unavailable_message(error).map(sqlly_core::DisplayMessage::into_fallback)
}

/// Merge the probed keys with their `MEMORY USAGE` replies (`None` when
/// memory is unavailable). Keys whose `TYPE` came back `none`, or whose
/// usage came back nil, vanished mid-sample and are dropped; the count of
/// dropped keys is returned alongside.
pub fn merge_usage(keys: Vec<KeyInfo>, usage: Option<Vec<Value>>) -> (Vec<KeySample>, usize) {
    let mut vanished = 0usize;
    let mut out = Vec::with_capacity(keys.len());
    let mut replies = usage.map(Vec::into_iter);
    for key in keys {
        let reply = replies.as_mut().map(|r| r.next());
        if key.kind == "none" {
            vanished += 1;
            continue;
        }
        let bytes = match reply {
            // Memory unavailable: keep the key, bytes unknown.
            None => None,
            Some(Some(Value::Int(n))) => Some(n),
            // Nil (or a missing reply): the key is gone.
            Some(_) => {
                vanished += 1;
                continue;
            }
        };
        out.push(KeySample {
            key: key.key,
            kind: key.kind,
            ttl_seconds: key.ttl_seconds,
            bytes,
        });
    }
    (out, vanished)
}

/// Sample the CURRENT database: a bounded `SCAN`, `TYPE`/`TTL` from the
/// key-listing probe, then `MEMORY USAGE key SAMPLES 5` pipelined in
/// chunks of [`USAGE_CHUNK`]. The size is clamped to
/// [`MAX_ANALYSIS_SAMPLE`].
pub async fn analyze_current(client: &RedisClient, sample: usize) -> StudioResult<AnalysisSample> {
    let sample = sample.clamp(1, MAX_ANALYSIS_SAMPLE);
    let (listing, total_keys) = if client.is_cluster() {
        // Each master's share of the sample, in proportion to its DBSIZE.
        super::cluster::sample_across(client, sample).await?
    } else {
        let listing = super::browse::scan_keys(client, "*", sample).await?;
        let total_keys = match client.run(&redis::cmd("DBSIZE")).await {
            Ok(Value::Int(n)) => n,
            _ => listing.keys.len() as i64,
        };
        (listing, total_keys)
    };
    let mut out = AnalysisSample {
        total_keys,
        complete: listing.complete,
        ..Default::default()
    };
    if listing.keys.is_empty() {
        return Ok(out);
    }
    let mut replies: Vec<Value> = Vec::with_capacity(listing.keys.len());
    for chunk in listing.keys.chunks(USAGE_CHUNK) {
        let mut pipe = redis::pipe();
        for key in chunk {
            pipe.cmd("MEMORY")
                .arg("USAGE")
                .arg(key.key.as_slice())
                .arg("SAMPLES")
                .arg(USAGE_SAMPLES);
        }
        match client.run_pipeline(&pipe).await {
            Ok(chunk_replies) => replies.extend(chunk_replies),
            Err(error) => match memory_unavailable_message(&error) {
                Some(reason) => {
                    out.memory_unavailable = Some(reason);
                    break;
                }
                None => return Err(error),
            },
        }
    }
    let usage = out.memory_unavailable.is_none().then_some(replies);
    let (keys, vanished) = merge_usage(listing.keys, usage);
    out.keys = keys;
    out.vanished = vanished;
    Ok(out)
}

/// One cached sample: the database and size it was taken for.
#[derive(Debug, Clone)]
struct CachedAnalysis {
    database: i64,
    sample: usize,
    analysis: Arc<AnalysisSample>,
}

/// Per-run memo of the analyzer sample. `SQLLY.MEMBYTYPE`, `SQLLY.TOPKEYS`,
/// `SQLLY.TTLBUCKETS` and `SQLLY.MEMSUMMARY` in one script share the sample
/// taken for their (database, size); the driver clears it when a script
/// starts and whenever a real command runs, so a later run (or a write
/// between reports) always resamples.
#[derive(Debug, Default)]
pub struct AnalysisCache {
    slot: parking_lot::Mutex<Option<CachedAnalysis>>,
}

impl AnalysisCache {
    pub fn get(&self, database: i64, sample: usize) -> Option<Arc<AnalysisSample>> {
        self.slot
            .lock()
            .as_ref()
            .filter(|cached| cached.database == database && cached.sample == sample)
            .map(|cached| cached.analysis.clone())
    }

    pub fn put(&self, database: i64, sample: usize, analysis: Arc<AnalysisSample>) {
        *self.slot.lock() = Some(CachedAnalysis {
            database,
            sample,
            analysis,
        });
    }

    pub fn clear(&self) {
        *self.slot.lock() = None;
    }
}

/// The run's sample of the current database at `sample` keys (clamped),
/// taken on first use and shared by every analyzer report after it.
pub async fn analysis_for_run(
    client: &RedisClient,
    sample: usize,
) -> StudioResult<Arc<AnalysisSample>> {
    let sample = sample.clamp(1, MAX_ANALYSIS_SAMPLE);
    let database = client.current_database();
    if let Some(hit) = client.analysis_cache.get(database, sample) {
        return Ok(hit);
    }
    let analysis = Arc::new(analyze_current(client, sample).await?);
    client
        .analysis_cache
        .put(database, sample, analysis.clone());
    Ok(analysis)
}

/// Scale a sampled figure to the keyspace: exact when the walk completed,
/// `sampled × total ÷ sampled_count` otherwise, never below the sampled
/// figure itself (a shrinking database mid-scan must not invert the
/// estimate). `None` when there is no sample to scale from.
fn extrapolate(sampled: i64, sampled_count: usize, total_keys: i64, complete: bool) -> i64 {
    if complete || sampled_count == 0 {
        return sampled;
    }
    let scaled = sampled as f64 * total_keys as f64 / sampled_count as f64;
    (scaled.round() as i64).max(sampled)
}

/// One row of the memory-by-type report.
#[derive(Debug, Clone, PartialEq)]
pub struct TypeAggregate {
    pub kind: String,
    pub sampled_keys: i64,
    /// `None` when the server cannot measure memory: counts stay honest,
    /// byte columns go NULL instead of pretending to be zero.
    pub sampled_bytes: Option<i64>,
    pub estimated_bytes: Option<i64>,
    /// Share of the sample's measured bytes, one decimal; 0 when unknown.
    pub pct_of_sampled_bytes: f64,
}

/// Aggregate the sample by data type, largest measured footprint first
/// (most-keys first when the server cannot measure).
pub fn by_type(sample: &AnalysisSample) -> Vec<TypeAggregate> {
    let mut kinds: Vec<(String, i64, i64)> = Vec::new();
    let mut total_bytes = 0i64;
    for key in &sample.keys {
        let bytes = key.bytes.unwrap_or(0);
        total_bytes += bytes;
        match kinds.iter_mut().find(|(kind, _, _)| *kind == key.kind) {
            Some((_, keys, sum)) => {
                *keys += 1;
                *sum += bytes;
            }
            None => kinds.push((key.kind.clone(), 1, bytes)),
        }
    }
    let mut out: Vec<TypeAggregate> = kinds
        .into_iter()
        .map(|(kind, keys, bytes)| TypeAggregate {
            kind,
            sampled_keys: keys,
            sampled_bytes: (!sample.memory_unsupported()).then_some(bytes),
            estimated_bytes: (!sample.memory_unsupported())
                .then(|| extrapolate(bytes, sample.keys.len(), sample.total_keys, sample.complete)),
            pct_of_sampled_bytes: if total_bytes > 0 {
                (bytes as f64 * 1000.0 / total_bytes as f64).round() / 10.0
            } else {
                0.0
            },
        })
        .collect();
    out.sort_by(|a, b| {
        b.sampled_bytes
            .unwrap_or(0)
            .cmp(&a.sampled_bytes.unwrap_or(0))
            .then(b.sampled_keys.cmp(&a.sampled_keys))
            .then(a.kind.cmp(&b.kind))
    });
    out
}

/// One row of the memory-by-prefix report: a namespace's sampled keys and
/// bytes, with both extrapolated to the keyspace.
#[derive(Debug, Clone, PartialEq)]
pub struct PrefixRow {
    /// `None` for keys whose names hold no delimiter.
    pub prefix: Option<String>,
    pub sampled_keys: i64,
    pub estimated_keys: i64,
    pub sampled_bytes: Option<i64>,
    pub estimated_bytes: Option<i64>,
    pub pct_of_sampled_bytes: f64,
}

/// Aggregate the sample by key namespace `depth` segments deep, splitting
/// names on every delimiter the setting names
/// (see [`sqlly_core::redis_keys::parse_delimiters`]). Byte columns stay
/// `None` when the server cannot measure memory.
pub fn by_prefix(sample: &AnalysisSample, depth: usize, delimiters: &str) -> Vec<PrefixRow> {
    let delimiters = sqlly_core::redis_keys::parse_delimiters(delimiters);
    let names: Vec<(String, Option<i64>)> = sample
        .keys
        .iter()
        .map(|k| (String::from_utf8_lossy(&k.key).into_owned(), k.bytes))
        .collect();
    let measured = !sample.memory_unsupported();
    let sampled = sample.keys.len();
    sqlly_core::redis_keys::aggregate_by_prefix(
        names.iter().map(|(k, b)| (k.as_str(), *b)),
        &delimiters,
        depth.clamp(1, MAX_PREFIX_DEPTH),
    )
    .into_iter()
    .map(|row| PrefixRow {
        prefix: row.prefix,
        sampled_keys: row.keys,
        estimated_keys: extrapolate(row.keys, sampled, sample.total_keys, sample.complete),
        sampled_bytes: row.bytes.filter(|_| measured),
        estimated_bytes: row
            .bytes
            .filter(|_| measured)
            .map(|b| extrapolate(b, sampled, sample.total_keys, sample.complete)),
        pct_of_sampled_bytes: row.pct_of_sampled_bytes,
    })
    .collect()
}

/// The deepest namespace level the by-prefix report splits to.
pub const MAX_PREFIX_DEPTH: usize = 3;

/// One row of the top-keys report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopKey {
    pub key: String,
    pub kind: String,
    pub ttl_seconds: Option<i64>,
    pub bytes: i64,
}

/// The `n` largest sampled keys, measured bytes descending.
pub fn top_keys(sample: &AnalysisSample, n: usize) -> Vec<TopKey> {
    let mut rows: Vec<TopKey> = sample
        .keys
        .iter()
        .filter_map(|k| {
            Some(TopKey {
                key: String::from_utf8_lossy(&k.key).into_owned(),
                kind: k.kind.clone(),
                ttl_seconds: k.ttl_seconds,
                bytes: k.bytes?,
            })
        })
        .collect();
    rows.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.key.cmp(&b.key)));
    rows.truncate(n.max(1));
    rows
}

/// TTL bucket labels, in report order.
pub const TTL_BUCKET_LABELS: [&str; 5] = [
    "no TTL",
    "< 1 hour",
    "1 hour – 1 day",
    "1 – 7 days",
    "> 7 days",
];

const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

/// Which bucket a TTL falls in: none, under an hour, under a day, under a
/// week, or longer. Boundary values land in the higher bucket (exactly one
/// hour is "1 hour – 1 day").
pub fn ttl_bucket_index(ttl_seconds: Option<i64>) -> usize {
    match ttl_seconds {
        None => 0,
        Some(ttl) if ttl < HOUR => 1,
        Some(ttl) if ttl < DAY => 2,
        Some(ttl) if ttl < 7 * DAY => 3,
        _ => 4,
    }
}

/// One row of the TTL-distribution report.
#[derive(Debug, Clone, PartialEq)]
pub struct TtlBucket {
    pub bucket: &'static str,
    pub sampled_keys: i64,
    pub estimated_keys: i64,
    /// Share of the sampled keys, one decimal.
    pub pct_of_sample: f64,
}

/// The sample's TTL distribution across the five buckets, in bucket order.
pub fn ttl_buckets(sample: &AnalysisSample) -> Vec<TtlBucket> {
    let mut counts = [0i64; 5];
    for key in &sample.keys {
        counts[ttl_bucket_index(key.ttl_seconds)] += 1;
    }
    let sampled = sample.keys.len();
    TTL_BUCKET_LABELS
        .into_iter()
        .enumerate()
        .map(|(i, bucket)| TtlBucket {
            bucket,
            sampled_keys: counts[i],
            estimated_keys: extrapolate(counts[i], sampled, sample.total_keys, sample.complete),
            pct_of_sample: if sampled > 0 {
                (counts[i] as f64 * 1000.0 / sampled as f64).round() / 10.0
            } else {
                0.0
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        keys: Vec<(&str, &str, Option<i64>, Option<i64>)>,
        total: i64,
        complete: bool,
    ) -> AnalysisSample {
        AnalysisSample {
            keys: keys
                .into_iter()
                .map(|(key, kind, ttl, bytes)| KeySample {
                    key: key.as_bytes().to_vec(),
                    kind: kind.to_string(),
                    ttl_seconds: ttl,
                    bytes,
                })
                .collect(),
            total_keys: total,
            complete,
            vanished: 0,
            memory_unavailable: None,
        }
    }

    #[test]
    fn ttl_bucket_boundaries_land_in_the_higher_bucket() {
        assert_eq!(ttl_bucket_index(None), 0);
        assert_eq!(ttl_bucket_index(Some(59)), 1);
        assert_eq!(ttl_bucket_index(Some(3_599)), 1);
        assert_eq!(
            ttl_bucket_index(Some(3_600)),
            2,
            "exactly one hour is 1h–1d"
        );
        assert_eq!(ttl_bucket_index(Some(86_399)), 2);
        assert_eq!(
            ttl_bucket_index(Some(86_400)),
            3,
            "exactly one day is 1d–7d"
        );
        assert_eq!(ttl_bucket_index(Some(7 * 86_400 - 1)), 3);
        assert_eq!(
            ttl_bucket_index(Some(7 * 86_400)),
            4,
            "exactly one week is > 7d"
        );
        assert_eq!(ttl_bucket_index(Some(30 * 86_400)), 4);
        // A TTL of 0 still means the key expires; only None is "no TTL".
        assert_eq!(ttl_bucket_index(Some(0)), 1);
    }

    #[test]
    fn by_type_sums_bytes_and_scales_estimates_to_dbsize() {
        // 4 of 400 keys sampled → estimates scale ×100.
        let s = sample(
            vec![
                ("a", "string", None, Some(100)),
                ("b", "string", None, Some(300)),
                ("h", "hash", Some(60), Some(800)),
                ("z", "zset", None, None),
            ],
            400,
            false,
        );
        let rows = by_type(&s);
        assert_eq!(rows[0].kind, "hash");
        assert_eq!(rows[0].sampled_keys, 1);
        assert_eq!(rows[0].sampled_bytes, Some(800));
        assert_eq!(rows[0].estimated_bytes, Some(80_000));
        assert_eq!(rows[1].kind, "string");
        assert_eq!(rows[1].sampled_bytes, Some(400));
        assert_eq!(rows[1].estimated_bytes, Some(40_000));
        assert_eq!(rows[1].pct_of_sampled_bytes, 33.3);
        assert_eq!(rows[2].kind, "zset");
        assert_eq!(rows[2].sampled_bytes, Some(0));
        // A complete walk reports exact figures.
        let full = sample(
            vec![
                ("a", "string", None, Some(100)),
                ("h", "hash", None, Some(50)),
            ],
            2,
            true,
        );
        let rows = by_type(&full);
        assert_eq!(rows[0].estimated_bytes, rows[0].sampled_bytes);
    }

    #[test]
    fn by_type_keeps_counts_but_nulls_bytes_when_memory_is_unsupported() {
        let mut s = sample(
            vec![("a", "string", None, None), ("h", "hash", None, None)],
            2,
            true,
        );
        s.memory_unavailable = Some("MEMORY USAGE is not supported by this server".into());
        let rows = by_type(&s);
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .all(|r| r.sampled_bytes.is_none() && r.estimated_bytes.is_none())
        );
        assert_eq!(rows.iter().map(|r| r.sampled_keys).sum::<i64>(), 2);
    }

    #[test]
    fn top_keys_orders_by_bytes_and_truncates_to_n() {
        let s = sample(
            vec![
                ("small", "string", None, Some(10)),
                ("big", "hash", Some(3600), Some(9_000)),
                ("mid", "list", None, Some(500)),
                ("gone", "string", None, None),
            ],
            4,
            true,
        );
        let rows = top_keys(&s, 2);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, "big");
        assert_eq!(rows[0].ttl_seconds, Some(3600));
        assert_eq!(rows[1].key, "mid");
        // Keys whose probe answered nil never pretend a size.
        assert!(!rows.iter().any(|r| r.key == "gone"));
        let all = top_keys(&s, 100);
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn ttl_buckets_distribute_and_estimate() {
        let s = sample(
            vec![
                ("a", "string", None, Some(1)),
                ("b", "string", Some(30), Some(1)),
                ("c", "string", Some(7_200), Some(1)),
                ("d", "string", Some(200_000), Some(1)),
                ("e", "string", Some(900_000), Some(1)),
            ],
            500,
            false,
        );
        let rows = ttl_buckets(&s);
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0].bucket, "no TTL");
        assert_eq!(rows[0].sampled_keys, 1);
        assert_eq!(rows[0].estimated_keys, 100, "1 of 5 sampled × 500 total");
        assert_eq!(rows[0].pct_of_sample, 20.0);
        assert_eq!(rows[1].sampled_keys, 1, "< 1 hour");
        assert_eq!(rows[2].sampled_keys, 1, "1 hour – 1 day");
        assert_eq!(rows[3].sampled_keys, 1, "1 – 7 days");
        assert_eq!(rows[4].sampled_keys, 1, "> 7 days");
        let empty = ttl_buckets(&AnalysisSample::default());
        assert!(
            empty
                .iter()
                .all(|b| b.sampled_keys == 0 && b.estimated_keys == 0)
        );
    }

    #[test]
    fn extrapolation_never_drops_below_the_sample() {
        // The database shrank mid-scan (DBSIZE < sampled count): the estimate
        // clamps to what was actually measured rather than inverting.
        assert_eq!(extrapolate(500, 100, 20, false), 500);
        assert_eq!(extrapolate(500, 0, 0, false), 500);
        assert_eq!(extrapolate(500, 100, 10_000, true), 500);
    }

    fn info(key: &str, kind: &str, ttl: Option<i64>) -> KeyInfo {
        KeyInfo {
            key: key.as_bytes().to_vec(),
            kind: kind.to_string(),
            ttl_seconds: ttl,
        }
    }

    #[test]
    fn merge_usage_drops_keys_that_vanished_mid_sample() {
        let keys = vec![
            info("a", "string", None),
            info("gone-before-type", "none", None),
            info("gone-before-usage", "hash", None),
            info("b", "list", Some(60)),
        ];
        let usage = vec![Value::Int(100), Value::Nil, Value::Nil, Value::Int(250)];
        let (kept, vanished) = merge_usage(keys, Some(usage));
        assert_eq!(vanished, 2);
        assert_eq!(
            kept.iter().map(|k| k.key.as_slice()).collect::<Vec<_>>(),
            vec![b"a".as_slice(), b"b".as_slice()]
        );
        assert_eq!(kept[1].bytes, Some(250));
        assert_eq!(kept[1].ttl_seconds, Some(60));
        // No `none` row can reach the by-type report.
        let sample = AnalysisSample {
            keys: kept,
            total_keys: 4,
            complete: true,
            ..Default::default()
        };
        assert!(by_type(&sample).iter().all(|t| t.kind != "none"));
    }

    #[test]
    fn merge_usage_keeps_counts_when_memory_is_unavailable() {
        let keys = vec![
            info("a", "string", None),
            info("gone", "none", None),
            info("h", "hash", Some(5)),
        ];
        let (kept, vanished) = merge_usage(keys, None);
        assert_eq!(vanished, 1);
        assert_eq!(kept.len(), 2);
        assert!(kept.iter().all(|k| k.bytes.is_none()));
    }

    #[test]
    fn memory_unavailable_matches_only_unknown_command_and_noperm() {
        // Unknown command / subcommand: the server cannot measure.
        let unknown = StudioError::QueryExecution(
            "ERR unknown command 'MEMORY', with args beginning with: 'USAGE'".into(),
        );
        assert!(
            memory_unavailable_reason(&unknown)
                .unwrap()
                .contains("not supported")
        );
        let subcommand = StudioError::QueryExecution(
            "ERR Unknown subcommand or wrong number of arguments for 'USAGE'".into(),
        );
        assert!(memory_unavailable_reason(&subcommand).is_some());
        // ACL denial arrives as PermissionDenied (NOPERM), not QueryExecution.
        let noperm = StudioError::PermissionDenied(
            "NOPERM User reader has no permissions to run the 'memory|usage' command".into(),
        );
        assert!(
            memory_unavailable_reason(&noperm)
                .unwrap()
                .contains("not permitted")
        );
        // Unrelated ERR replies are real errors, not "memory unsupported".
        for text in [
            "ERR max number of clients reached",
            "LOADING Redis is loading the dataset in memory",
            "ERR syntax error",
        ] {
            assert!(
                memory_unavailable_reason(&StudioError::QueryExecution(text.into())).is_none(),
                "{text}"
            );
        }
        assert!(memory_unavailable_reason(&StudioError::Connection("reset".into())).is_none());
    }

    #[test]
    fn by_prefix_sums_namespaces_and_extrapolates() {
        let s = sample(
            vec![
                ("user:1", "hash", None, Some(300)),
                ("user:2", "hash", None, Some(100)),
                ("cache|home", "string", None, Some(100)),
                ("config", "string", None, Some(0)),
            ],
            8,
            false,
        );
        let rows = by_prefix(&s, 1, ":|");
        assert_eq!(rows[0].prefix.as_deref(), Some("user:"));
        assert_eq!(rows[0].sampled_keys, 2);
        assert_eq!(rows[0].estimated_keys, 4);
        assert_eq!(rows[0].sampled_bytes, Some(400));
        assert_eq!(rows[0].estimated_bytes, Some(800));
        assert_eq!(rows[0].pct_of_sampled_bytes, 80.0);
        assert_eq!(rows[1].prefix.as_deref(), Some("cache|"));
        assert_eq!(rows[2].prefix, None);
        // Depth is clamped to 1..=3; a lone `:` setting leaves `|` alone.
        let colon = by_prefix(&s, 0, ":");
        assert!(colon.iter().all(|r| r.prefix.as_deref() != Some("cache|")));
        let mut unmeasured = s.clone();
        unmeasured.memory_unavailable = Some(sqlly_core::DisplayMessage::new("k", "no"));
        let rows = by_prefix(&unmeasured, 1, ":");
        assert!(
            rows.iter()
                .all(|r| r.sampled_bytes.is_none() && r.estimated_bytes.is_none())
        );
        assert_eq!(rows.iter().map(|r| r.sampled_keys).sum::<i64>(), 4);
    }

    #[test]
    fn analysis_cache_is_keyed_by_database_and_size() {
        let cache = AnalysisCache::default();
        assert!(cache.get(0, 1000).is_none());
        let analysis = Arc::new(AnalysisSample {
            total_keys: 7,
            ..Default::default()
        });
        cache.put(0, 1000, analysis.clone());
        assert!(Arc::ptr_eq(&cache.get(0, 1000).unwrap(), &analysis));
        // Another database or another size is a different sample.
        assert!(cache.get(1, 1000).is_none());
        assert!(cache.get(0, 500).is_none());
        cache.clear();
        assert!(cache.get(0, 1000).is_none());
    }

    /// Live: the analyzer sees seeded keys by type, with MEMORY USAGE bytes
    /// and TTL buckets. Opt-in through `SQLLY_REDIS_URL`; uses database 14
    /// so it never touches the default keyspace.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_analysis_reports_types_bytes_and_ttls() {
        let Some(mut profile) = super::super::live_profile_from_env("redis-analyze") else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        profile.database = Some("14".to_string());
        let client = super::super::open_client(&profile).await.expect("client");
        let prefix = format!("sqlly-analyze-{}", std::process::id());
        let mut seed = redis::pipe();
        seed.cmd("SET")
            .arg(format!("{prefix}:s"))
            .arg("x".repeat(200));
        seed.cmd("HSET")
            .arg(format!("{prefix}:h"))
            .arg("f")
            .arg("v");
        seed.cmd("SET")
            .arg(format!("{prefix}:t"))
            .arg("1")
            .arg("EX")
            .arg(120);
        client.run_pipeline(&seed).await.expect("seed");

        let sample = analyze_current(&client, 10_000).await.expect("analyze");
        let ours: Vec<&KeySample> = sample
            .keys
            .iter()
            .filter(|k| k.key.starts_with(prefix.as_bytes()))
            .collect();
        assert_eq!(ours.len(), 3, "all three seeded keys sampled");
        if !sample.memory_unsupported() {
            assert!(ours.iter().all(|k| k.bytes.is_some_and(|b| b > 0)));
        }
        let types = by_type(&sample);
        assert!(types.iter().any(|t| t.kind == "string"));
        assert!(types.iter().any(|t| t.kind == "hash"));
        let buckets = ttl_buckets(&sample);
        assert!(buckets.iter().map(|b| b.sampled_keys).sum::<i64>() >= 3);

        let mut cleanup = redis::cmd("DEL");
        cleanup
            .arg(format!("{prefix}:s"))
            .arg(format!("{prefix}:h"))
            .arg(format!("{prefix}:t"));
        client.run(&cleanup).await.expect("cleanup");
    }
}
