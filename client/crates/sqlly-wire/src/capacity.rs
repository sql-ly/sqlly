//! Relay capacity percentage math.
//!
//! The relay reports how full it is to the API control plane as an integer
//! percentage. The semantics are deliberate (see the wire spec):
//!
//! * `allowed == 0` means the relay has no available slots (unavailable / not
//!   accepting connections). It reports `100` and never divides by zero.
//! * `active >= allowed` means the relay is full. It reports `100`.
//! * Otherwise the fraction is rounded half-up and capped at `99`, so a relay
//!   only ever shows `100` when it is genuinely at (or over) its limit.

/// Compute the relay capacity as an integer percentage in `0..=100`.
///
/// See the module docs for the exact semantics. This is intentionally total and
/// panic-free for every `u64` input pair.
///
/// # Examples
///
/// ```
/// use sqlly_wire::capacity_pct;
/// assert_eq!(capacity_pct(0, 2000), 0);
/// assert_eq!(capacity_pct(1, 2000), 0);
/// assert_eq!(capacity_pct(10, 2000), 1);
/// assert_eq!(capacity_pct(1999, 2000), 99);
/// assert_eq!(capacity_pct(2000, 2000), 100);
/// assert_eq!(capacity_pct(2001, 2000), 100);
/// assert_eq!(capacity_pct(5, 0), 100);
/// ```
#[must_use]
pub fn capacity_pct(active: u64, allowed: u64) -> u8 {
    // No slots configured/available: treat as full, and never divide by zero.
    if allowed == 0 {
        return 100;
    }

    // At or over the limit reports full.
    if active >= allowed {
        return 100;
    }

    // Round half-up. `active < allowed` here, so `active * 100` cannot overflow
    // for any realistic slot count, but use `u128` to be provably total for the
    // full `u64` domain.
    let rounded = ((u128::from(active) * 100) + (u128::from(allowed) / 2)) / u128::from(allowed);

    // Below the limit is capped at 99 so 100 is reserved for "full".
    rounded.min(99) as u8
}

#[cfg(test)]
mod tests {
    use super::capacity_pct;

    #[test]
    fn required_vectors() {
        assert_eq!(capacity_pct(0, 2000), 0);
        assert_eq!(capacity_pct(1, 2000), 0);
        assert_eq!(capacity_pct(10, 2000), 1);
        assert_eq!(capacity_pct(1999, 2000), 99);
        assert_eq!(capacity_pct(2000, 2000), 100);
        assert_eq!(capacity_pct(2001, 2000), 100);
    }

    #[test]
    fn allowed_zero_is_full_not_div_by_zero() {
        assert_eq!(capacity_pct(0, 0), 100);
        assert_eq!(capacity_pct(1, 0), 100);
        assert_eq!(capacity_pct(u64::MAX, 0), 100);
    }

    #[test]
    fn rounds_half_up() {
        // 15/2000 = 0.75% -> rounds to 1.
        assert_eq!(capacity_pct(15, 2000), 1);
        // 5/2000 = 0.25% -> rounds to 0.
        assert_eq!(capacity_pct(5, 2000), 0);
        // Exactly half a percent (10/2000 = 0.5%) rounds up to 1.
        assert_eq!(capacity_pct(10, 2000), 1);
    }

    #[test]
    fn just_below_limit_never_reads_full() {
        // One slot free must never report 100.
        assert_eq!(capacity_pct(999_999, 1_000_000), 99);
        assert_eq!(capacity_pct(allowed_minus_one(1_000_000), 1_000_000), 99);
    }

    fn allowed_minus_one(allowed: u64) -> u64 {
        allowed - 1
    }

    #[test]
    fn small_limits() {
        assert_eq!(capacity_pct(0, 1), 0);
        assert_eq!(capacity_pct(1, 1), 100);
        assert_eq!(capacity_pct(0, 2), 0);
        assert_eq!(capacity_pct(1, 2), 50);
        assert_eq!(capacity_pct(2, 2), 100);
    }

    #[test]
    fn never_overflows_at_u64_extremes() {
        // active just below an enormous allowed.
        assert_eq!(capacity_pct(u64::MAX - 1, u64::MAX), 99);
        assert_eq!(capacity_pct(u64::MAX, u64::MAX), 100);
        assert_eq!(capacity_pct(u64::MAX / 2, u64::MAX), 50);
    }
}
