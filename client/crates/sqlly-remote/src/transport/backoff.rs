//! Reconnect backoff: exponential growth with jitter.
//!
//! The schedule is split into a pure function ([`backoff_delay`]) that takes the
//! jitter fraction as an explicit argument — so it is deterministic and unit
//! testable — and a thin runtime wrapper ([`BackoffPolicy::sample`]) that draws
//! the fraction from an RNG.

use std::time::Duration;

/// Parameters for the reconnect backoff schedule.
#[derive(Debug, Clone, Copy)]
pub struct BackoffPolicy {
    /// Delay for attempt 0, before any exponential growth.
    pub base: Duration,
    /// Upper bound on the pre-jitter delay.
    pub max: Duration,
    /// Multiplier applied per attempt (e.g. `2.0` doubles each time).
    pub factor: f64,
    /// Jitter fraction in `[0.0, 1.0]`. `0.5` spreads the delay across
    /// `[raw * 0.5, raw]`; `0.0` disables jitter.
    pub jitter: f64,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(500),
            max: Duration::from_secs(30),
            factor: 2.0,
            jitter: 0.5,
        }
    }
}

impl BackoffPolicy {
    /// The pre-jitter (raw) delay for `attempt`: `base * factor^attempt`, capped
    /// at `max`.
    #[must_use]
    pub fn raw_delay(&self, attempt: u32) -> Duration {
        let base = self.base.as_secs_f64();
        let raw = base * self.factor.powi(attempt as i32);
        let capped = raw.min(self.max.as_secs_f64());
        Duration::from_secs_f64(capped)
    }

    /// Draw the next delay, sampling the jitter fraction from `rng`.
    pub fn sample<R: rand::Rng>(&self, attempt: u32, rng: &mut R) -> Duration {
        use rand::RngExt;
        backoff_delay(self, attempt, rng.random::<f64>())
    }

    /// Draw the next delay from the thread-local RNG.
    pub fn next_delay(&self, attempt: u32) -> Duration {
        self.sample(attempt, &mut rand::rng())
    }
}

/// Compute the backoff delay for `attempt` given an explicit `jitter_frac` in
/// `[0.0, 1.0)`.
///
/// The result is `raw * (1 - jitter) + raw * jitter * jitter_frac`, where `raw`
/// is [`BackoffPolicy::raw_delay`]. Because `jitter_frac < 1`, the delay never
/// exceeds `raw` (and therefore never exceeds `policy.max`), and it grows
/// monotonically with `jitter_frac`.
#[must_use]
pub fn backoff_delay(policy: &BackoffPolicy, attempt: u32, jitter_frac: f64) -> Duration {
    let raw = policy.raw_delay(attempt).as_secs_f64();
    let jitter = policy.jitter.clamp(0.0, 1.0);
    let frac = jitter_frac.clamp(0.0, 1.0);
    let floor = raw * (1.0 - jitter);
    let delay = floor + raw * jitter * frac;
    Duration::from_secs_f64(delay)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> BackoffPolicy {
        BackoffPolicy {
            base: Duration::from_millis(100),
            max: Duration::from_secs(10),
            factor: 2.0,
            jitter: 0.5,
        }
    }

    #[test]
    fn raw_delay_doubles_then_caps() {
        let p = policy();
        assert_eq!(p.raw_delay(0), Duration::from_millis(100));
        assert_eq!(p.raw_delay(1), Duration::from_millis(200));
        assert_eq!(p.raw_delay(2), Duration::from_millis(400));
        // 100ms * 2^7 = 12.8s, capped at 10s.
        assert_eq!(p.raw_delay(7), Duration::from_secs(10));
        assert_eq!(p.raw_delay(30), Duration::from_secs(10));
    }

    #[test]
    fn jitter_spans_floor_to_raw() {
        let p = policy();
        // attempt 0: raw = 100ms, jitter 0.5 -> floor 50ms.
        let lo = backoff_delay(&p, 0, 0.0);
        let hi = backoff_delay(&p, 0, 1.0);
        assert_eq!(lo, Duration::from_millis(50));
        assert_eq!(hi, Duration::from_millis(100));
    }

    #[test]
    fn jitter_is_monotonic_and_bounded_by_max() {
        let p = policy();
        let mut prev = Duration::ZERO;
        for i in 0..=10 {
            let frac = i as f64 / 10.0;
            let d = backoff_delay(&p, 5, frac);
            assert!(d >= prev, "delay must be monotonic in jitter_frac");
            assert!(d <= p.max, "delay must never exceed max");
            prev = d;
        }
    }

    #[test]
    fn sampled_delay_stays_in_range() {
        let p = policy();
        let mut rng = rand::rng();
        for attempt in 0..12 {
            let raw = p.raw_delay(attempt);
            let d = p.sample(attempt, &mut rng);
            let floor = Duration::from_secs_f64(raw.as_secs_f64() * 0.5);
            assert!(d >= floor && d <= raw, "attempt {attempt}: {d:?}");
        }
    }
}
