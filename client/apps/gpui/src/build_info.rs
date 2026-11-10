//! Compile-time build stamp.
//!
//! `build.rs` bakes the UTC build date into the binary as the
//! `SQLLY_BUILD_DATE` environment variable (`YYYY-MM-DD`). Releases do not
//! expire — the prerelease expiry policy (lockout window, status-bar
//! countdown) was removed along with the rest of the prerelease messaging.
//! The stamp remains the anchor for "days in use" style calculations.

use chrono::{NaiveDate, Utc};

/// The UTC date this binary was built, `YYYY-MM-DD`, stamped by `build.rs`.
pub const BUILD_DATE_STAMP: &str = env!("SQLLY_BUILD_DATE");

/// Today's date in UTC. Date-granular (not timestamp-granular) so anything
/// derived from it flips at the same moment in every time zone.
pub fn today() -> NaiveDate {
    Utc::now().date_naive()
}

/// Parses a `YYYY-MM-DD` stamp. Returns `None` for anything else.
fn parse_stamp(stamp: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(stamp.trim(), "%Y-%m-%d").ok()
}

/// The date this binary was built, from the baked stamp. Panics only if the
/// build script emitted a malformed stamp, which the unit tests catch.
pub fn build_date() -> NaiveDate {
    parse_stamp(BUILD_DATE_STAMP).expect("SQLLY_BUILD_DATE must be YYYY-MM-DD")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn baked_stamp_parses_as_yyyy_mm_dd() {
        let parsed = parse_stamp(BUILD_DATE_STAMP);
        assert!(
            parsed.is_some(),
            "SQLLY_BUILD_DATE is not YYYY-MM-DD: {BUILD_DATE_STAMP:?}"
        );
    }

    #[test]
    fn parse_stamp_rejects_garbage() {
        assert!(parse_stamp("").is_none());
        assert!(parse_stamp("2026/07/28").is_none());
        assert!(parse_stamp("July 28, 2026").is_none());
        assert!(parse_stamp("2026-13-01").is_none());
        assert!(parse_stamp("not-a-date").is_none());
    }

    #[test]
    fn parse_stamp_trims_whitespace() {
        assert_eq!(parse_stamp(" 2026-07-28 \n"), Some(date(2026, 7, 28)));
    }
}
