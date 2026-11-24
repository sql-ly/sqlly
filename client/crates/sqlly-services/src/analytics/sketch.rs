//! Bounded-memory sketches for profiling results too large to hold:
//! HyperLogLog distinct counts, a KLL quantile sketch, and Space-Saving
//! heavy hitters. All three are deterministic: the same values in the same
//! order always give the same state, whatever the batch boundaries.
//!
//! Documented accuracy (tested in this module):
//! - [`HyperLogLog`] at precision 12 (4 KiB): relative standard error
//!   1.04/√4096 ≈ 1.6%; estimates stay within 5% (≈3σ).
//! - [`KllSketch`] with k = 256: normalized rank error ≈ 1.7/k ≈ 0.7%;
//!   tested within 2% of the true rank.
//! - [`SpaceSaving`] with m counters: each reported count overestimates the
//!   true count by at most its recorded error, which is at most n/m.

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

use super::value::splitmix64;

/// HyperLogLog distinct-count estimator over pre-hashed 64-bit values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HyperLogLog {
    precision: u8,
    registers: Vec<u8>,
}

impl HyperLogLog {
    /// `precision` is clamped to 4..=16 (16 bytes to 64 KiB of registers).
    pub fn new(precision: u8) -> Self {
        let precision = precision.clamp(4, 16);
        Self {
            precision,
            registers: vec![0; 1 << precision],
        }
    }

    pub fn insert_hash(&mut self, hash: u64) {
        let p = u32::from(self.precision);
        let index = (hash >> (64 - p)) as usize;
        let rest = hash << p;
        let rank = if rest == 0 {
            (64 - p + 1) as u8
        } else {
            (rest.leading_zeros() + 1) as u8
        };
        if rank > self.registers[index] {
            self.registers[index] = rank;
        }
    }

    pub fn estimate(&self) -> f64 {
        let m = self.registers.len() as f64;
        let alpha = match self.registers.len() {
            16 => 0.673,
            32 => 0.697,
            64 => 0.709,
            _ => 0.7213 / (1.0 + 1.079 / m),
        };
        let sum: f64 = self
            .registers
            .iter()
            .map(|r| 2f64.powi(-i32::from(*r)))
            .sum();
        let raw = alpha * m * m / sum;
        let zeros = self.registers.iter().filter(|r| **r == 0).count();
        if raw <= 2.5 * m && zeros > 0 {
            // Linear counting is far more accurate in the small range.
            m * (m / zeros as f64).ln()
        } else {
            raw
        }
    }

    /// Fold another sketch of the same precision into this one.
    pub fn merge(&mut self, other: &HyperLogLog) {
        if other.precision != self.precision {
            return;
        }
        for (mine, theirs) in self.registers.iter_mut().zip(&other.registers) {
            *mine = (*mine).max(*theirs);
        }
    }
}

/// KLL quantile sketch (Karnin, Lang, Liberty 2016) over `f64`. Compaction
/// coin flips come from a fixed-seed generator, so the sketch is a pure
/// function of its input sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KllSketch {
    k: usize,
    levels: Vec<Vec<f64>>,
    count: u64,
    rng: u64,
}

impl KllSketch {
    pub fn new(k: usize) -> Self {
        Self {
            k: k.max(8),
            levels: vec![Vec::new()],
            count: 0,
            rng: 0x0005_EED0_F5A1,
        }
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    /// Insert a value; NaN is ignored (callers count it separately).
    pub fn insert(&mut self, value: f64) {
        if value.is_nan() {
            return;
        }
        self.levels[0].push(value);
        self.count += 1;
        if self.levels[0].len() >= self.capacity(0) {
            self.compress();
        }
    }

    fn capacity(&self, level: usize) -> usize {
        let depth = self.levels.len() - level - 1;
        let scaled = self.k as f64 * (2.0f64 / 3.0).powi(depth as i32);
        (scaled.ceil() as usize).max(2)
    }

    fn compress(&mut self) {
        let mut level = 0;
        while level < self.levels.len() {
            if self.levels[level].len() >= self.capacity(level) {
                if level + 1 == self.levels.len() {
                    self.levels.push(Vec::new());
                }
                let mut items = std::mem::take(&mut self.levels[level]);
                items.sort_by(f64::total_cmp);
                // An odd item out stays behind at this level.
                if items.len() % 2 == 1 {
                    let kept = items.pop().unwrap_or_default();
                    self.levels[level].push(kept);
                }
                self.rng = splitmix64(self.rng);
                let offset = (self.rng & 1) as usize;
                let promoted = items.iter().skip(offset).step_by(2).copied();
                self.levels[level + 1].extend(promoted);
            }
            level += 1;
        }
    }

    /// Weighted, sorted items: (value, weight).
    fn weighted(&self) -> Vec<(f64, u64)> {
        let mut items: Vec<(f64, u64)> = self
            .levels
            .iter()
            .enumerate()
            .flat_map(|(level, values)| values.iter().map(move |v| (*v, 1u64 << level)))
            .collect();
        items.sort_by(|a, b| a.0.total_cmp(&b.0));
        items
    }

    /// The value at quantile `q` in `0..=1` (nearest rank), `None` when
    /// empty.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        self.quantiles(&[q]).into_iter().next().flatten()
    }

    pub fn quantiles(&self, qs: &[f64]) -> Vec<Option<f64>> {
        let items = self.weighted();
        let total: u64 = items.iter().map(|(_, w)| *w).sum();
        if total == 0 {
            return vec![None; qs.len()];
        }
        qs.iter()
            .map(|q| {
                let target = (q.clamp(0.0, 1.0) * total as f64).ceil().max(1.0) as u64;
                let mut cumulative = 0u64;
                for (value, weight) in &items {
                    cumulative += weight;
                    if cumulative >= target {
                        return Some(*value);
                    }
                }
                items.last().map(|(v, _)| *v)
            })
            .collect()
    }

    /// Estimated fraction of values `<= x`.
    pub fn cdf(&self, x: f64) -> f64 {
        let items = self.weighted();
        let total: u64 = items.iter().map(|(_, w)| *w).sum();
        if total == 0 {
            return 0.0;
        }
        let below: u64 = items
            .iter()
            .take_while(|(v, _)| *v <= x)
            .map(|(_, w)| *w)
            .sum();
        below as f64 / total as f64
    }

    /// Estimated fraction of values `< x`.
    pub fn cdf_exclusive(&self, x: f64) -> f64 {
        let items = self.weighted();
        let total: u64 = items.iter().map(|(_, w)| *w).sum();
        if total == 0 {
            return 0.0;
        }
        let below: u64 = items
            .iter()
            .take_while(|(v, _)| *v < x)
            .map(|(_, w)| *w)
            .sum();
        below as f64 / total as f64
    }

    /// Retained item count (the sketch's memory footprint in values).
    pub fn retained(&self) -> usize {
        self.levels.iter().map(Vec::len).sum()
    }
}

/// One tracked heavy hitter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeavyHitter<K> {
    pub key: K,
    pub count: u64,
    /// Upper bound on how much `count` overstates the true count.
    pub error: u64,
}

/// Space-Saving top-k (Metwally, Agrawal, El Abbadi 2005) with `capacity`
/// counters. Ties for the minimum are broken by the oldest insertion, so the
/// outcome depends only on the input order.
#[derive(Debug, Clone)]
pub struct SpaceSaving<K: std::hash::Hash + Eq + Clone> {
    capacity: usize,
    slots: Vec<Slot<K>>,
    index: HashMap<K, usize>,
    by_count: BTreeSet<(u64, u64, usize)>,
    sequence: u64,
    total: u64,
}

#[derive(Debug, Clone)]
struct Slot<K> {
    key: K,
    count: u64,
    error: u64,
    sequence: u64,
}

impl<K: std::hash::Hash + Eq + Clone> SpaceSaving<K> {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            slots: Vec::new(),
            index: HashMap::new(),
            by_count: BTreeSet::new(),
            sequence: 0,
            total: 0,
        }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// Count one occurrence of `key`; returns the key evicted to make room,
    /// if any.
    pub fn insert(&mut self, key: K) -> Option<K> {
        self.insert_weighted(key, 1, 0)
    }

    /// Count `weight` occurrences of `key`, carrying a known prior error.
    pub fn insert_weighted(&mut self, key: K, weight: u64, error: u64) -> Option<K> {
        self.total += weight;
        self.sequence += 1;
        if let Some(&slot_index) = self.index.get(&key) {
            let slot = &mut self.slots[slot_index];
            self.by_count
                .remove(&(slot.count, slot.sequence, slot_index));
            slot.count += weight;
            slot.error += error;
            self.by_count
                .insert((slot.count, slot.sequence, slot_index));
            return None;
        }
        if self.slots.len() < self.capacity {
            let slot_index = self.slots.len();
            self.slots.push(Slot {
                key: key.clone(),
                count: weight,
                error,
                sequence: self.sequence,
            });
            self.index.insert(key, slot_index);
            self.by_count.insert((weight, self.sequence, slot_index));
            return None;
        }
        let (min_count, min_sequence, slot_index) = *self.by_count.iter().next()?;
        self.by_count.remove(&(min_count, min_sequence, slot_index));
        let slot = &mut self.slots[slot_index];
        let evicted = std::mem::replace(&mut slot.key, key.clone());
        self.index.remove(&evicted);
        slot.count = min_count + weight;
        slot.error = min_count + error;
        slot.sequence = self.sequence;
        self.index.insert(key, slot_index);
        self.by_count
            .insert((slot.count, slot.sequence, slot_index));
        Some(evicted)
    }

    /// The tracked keys, highest count first (ties: earliest tracked).
    pub fn top(&self, n: usize) -> Vec<HeavyHitter<K>> {
        let mut slots: Vec<&Slot<K>> = self.slots.iter().collect();
        slots.sort_by(|a, b| b.count.cmp(&a.count).then(a.sequence.cmp(&b.sequence)));
        slots
            .into_iter()
            .take(n)
            .map(|slot| HeavyHitter {
                key: slot.key.clone(),
                count: slot.count,
                error: slot.error,
            })
            .collect()
    }

    pub fn contains(&self, key: &K) -> bool {
        self.index.contains_key(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hll_counts_small_and_huge_cardinalities() {
        for (n, tolerance) in [(0u64, 0.0f64), (10, 0.01), (1_000, 0.03), (1_000_000, 0.05)] {
            let mut hll = HyperLogLog::new(12);
            for value in 0..n {
                hll.insert_hash(splitmix64(value));
            }
            let estimate = hll.estimate();
            let error = if n == 0 {
                estimate
            } else {
                (estimate - n as f64).abs() / n as f64
            };
            assert!(error <= tolerance.max(0.0001), "n={n} estimate={estimate}");
        }
    }

    #[test]
    fn hll_ignores_duplicates_and_merges() {
        let mut a = HyperLogLog::new(12);
        let mut b = HyperLogLog::new(12);
        for value in 0..5_000u64 {
            a.insert_hash(splitmix64(value % 100));
            b.insert_hash(splitmix64(value % 100 + 50));
        }
        assert!((a.estimate() - 100.0).abs() < 3.0);
        a.merge(&b);
        assert!((a.estimate() - 150.0).abs() < 5.0);
    }

    #[test]
    fn kll_ranks_stay_within_two_percent() {
        let n = 200_000u64;
        let mut sketch = KllSketch::new(256);
        // A scrambled order so the input is not already sorted.
        for i in 0..n {
            sketch.insert((splitmix64(i) % n) as f64);
        }
        assert_eq!(sketch.count(), n);
        assert!(sketch.retained() < 2_000, "retained {}", sketch.retained());
        for q in [0.01, 0.05, 0.25, 0.5, 0.75, 0.95, 0.99] {
            let value = sketch.quantile(q).unwrap();
            let true_rank = value / n as f64;
            assert!((true_rank - q).abs() < 0.02, "q={q} value={value}");
        }
        let cdf = sketch.cdf(n as f64 / 2.0);
        assert!((cdf - 0.5).abs() < 0.02, "cdf {cdf}");
    }

    #[test]
    fn kll_is_deterministic_and_skips_nan() {
        let build = || {
            let mut sketch = KllSketch::new(64);
            for i in 0..10_000u64 {
                sketch.insert((i % 977) as f64);
            }
            sketch.insert(f64::NAN);
            sketch
        };
        assert_eq!(build(), build());
        assert_eq!(build().count(), 10_000);
        assert_eq!(KllSketch::new(64).quantile(0.5), None);
        let mut small = KllSketch::new(64);
        for v in [3.0, 1.0, 2.0] {
            small.insert(v);
        }
        assert_eq!(
            small.quantiles(&[0.0, 0.5, 1.0]),
            vec![Some(1.0), Some(2.0), Some(3.0)]
        );
    }

    #[test]
    fn space_saving_finds_heavy_hitters_within_bound() {
        let mut ss = SpaceSaving::new(50);
        let mut n = 0u64;
        for i in 0..100_000u64 {
            // Keys 0..5 are heavy; the rest is a long uniform tail.
            let key = if i % 2 == 0 {
                i % 10 / 2
            } else {
                1_000 + splitmix64(i) % 50_000
            };
            ss.insert(key);
            n += 1;
        }
        let top = ss.top(5);
        let mut keys: Vec<u64> = top.iter().map(|h| h.key).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec![0, 1, 2, 3, 4]);
        for hitter in &top {
            assert!(hitter.count >= 10_000);
            assert!(hitter.count - hitter.error <= 10_000);
            assert!(hitter.error <= n / 50);
        }
        assert_eq!(ss.total(), n);
    }

    #[test]
    fn space_saving_ties_break_by_age() {
        let mut ss = SpaceSaving::new(2);
        ss.insert("a");
        ss.insert("b");
        assert_eq!(ss.insert("c"), Some("a"));
        let top = ss.top(2);
        assert_eq!(top[0].key, "c");
        assert_eq!(top[0].count, 2);
        assert_eq!(top[0].error, 1);
    }
}
