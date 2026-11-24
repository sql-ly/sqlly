//! Date and time handling for the analytics: reading engine date/time text,
//! calendar buckets (time grains), and choosing a grain for a span.

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use sqlly_core::CellValue;

/// A calendar bucket size. Weeks start on Monday (ISO 8601).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeGrain {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl TimeGrain {
    pub const ALL: [TimeGrain; 8] = [
        TimeGrain::Second,
        TimeGrain::Minute,
        TimeGrain::Hour,
        TimeGrain::Day,
        TimeGrain::Week,
        TimeGrain::Month,
        TimeGrain::Quarter,
        TimeGrain::Year,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            TimeGrain::Second => "second",
            TimeGrain::Minute => "minute",
            TimeGrain::Hour => "hour",
            TimeGrain::Day => "day",
            TimeGrain::Week => "week",
            TimeGrain::Month => "month",
            TimeGrain::Quarter => "quarter",
            TimeGrain::Year => "year",
        }
    }

    /// Typical bucket length in seconds (months and years averaged), for
    /// estimating bucket counts.
    pub fn approx_seconds(self) -> f64 {
        match self {
            TimeGrain::Second => 1.0,
            TimeGrain::Minute => 60.0,
            TimeGrain::Hour => 3_600.0,
            TimeGrain::Day => 86_400.0,
            TimeGrain::Week => 604_800.0,
            TimeGrain::Month => 2_629_746.0,
            TimeGrain::Quarter => 7_889_238.0,
            TimeGrain::Year => 31_556_952.0,
        }
    }
}

/// A date or date-time as written, with the UTC offset it carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZonedTimestamp {
    /// The wall-clock value as written (offset not applied).
    pub local: NaiveDateTime,
    /// Seconds east of UTC, when the text had a zone (`Z` is 0).
    pub offset_seconds: Option<i64>,
    /// The text was a date with no time part.
    pub date_only: bool,
}

impl ZonedTimestamp {
    /// The instant in UTC (the written value when it had no zone).
    pub fn utc(&self) -> NaiveDateTime {
        match self.offset_seconds {
            Some(offset) => self
                .local
                .checked_sub_signed(Duration::seconds(offset))
                .unwrap_or(self.local),
            None => self.local,
        }
    }
}

/// Read a date or date-time text the way engines print them: ISO dates,
/// date-times with `T` or a space, fractional seconds, and a zone suffix
/// (`Z`, `+05:30`, `+0530`, `+05`, ` UTC`), kept apart from the wall-clock
/// value. Slashed dates (`2024/01/31`) are accepted; ambiguous day/month
/// orders are not. The one parser behind analytics and report timestamps,
/// so a chart and a profile put the same row in the same bucket.
pub fn parse_zoned(text: &str) -> Option<ZonedTimestamp> {
    let t = text.trim();
    let bytes = t.as_bytes();
    if bytes.len() < 10
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !matches!(bytes[4], b'-' | b'/')
    {
        return None;
    }
    let (core, offset_seconds) = split_zone(t);
    let normalized;
    let core = if core.as_bytes()[4] == b'/' {
        normalized = core.replacen('/', "-", 2);
        normalized.as_str()
    } else {
        core
    };
    let timed = [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
    ]
    .iter()
    .find_map(|fmt| NaiveDateTime::parse_from_str(core, fmt).ok());
    let (local, date_only) = match timed {
        Some(local) => (local, false),
        None => (
            NaiveDate::parse_from_str(core, "%Y-%m-%d")
                .ok()?
                .and_time(NaiveTime::MIN),
            true,
        ),
    };
    Some(ZonedTimestamp {
        local,
        offset_seconds,
        date_only,
    })
}

/// [`parse_zoned`] as a UTC instant: a zone suffix is applied so every
/// value lands in UTC.
pub fn parse_timestamp(text: &str) -> Option<NaiveDateTime> {
    let zoned = parse_zoned(text)?;
    match zoned.offset_seconds {
        Some(offset) => zoned.local.checked_sub_signed(Duration::seconds(offset)),
        None => Some(zoned.local),
    }
}

/// Split a trailing zone designator off a date-time text, returning the
/// rest and the offset east of UTC in seconds.
fn split_zone(t: &str) -> (&str, Option<i64>) {
    if let Some(rest) = t.strip_suffix('Z').or_else(|| t.strip_suffix('z')) {
        return (rest.trim_end(), Some(0));
    }
    if let Some(rest) = t.strip_suffix(" UTC") {
        return (rest.trim_end(), Some(0));
    }
    // A zone can only follow the time part, which starts after the date.
    if t.len() <= 10 {
        return (t, None);
    }
    let tail = &t[10..];
    let Some(sign_at) = tail.rfind(['+', '-']) else {
        return (t, None);
    };
    let zone = &tail[sign_at + 1..];
    let sign = if tail.as_bytes()[sign_at] == b'-' {
        -1
    } else {
        1
    };
    let digits: String = zone.chars().filter(|c| *c != ':').collect();
    if !(digits.len() == 2 || digits.len() == 4) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return (t, None);
    }
    let hours: i64 = digits[..2].parse().unwrap_or(0);
    let minutes: i64 = if digits.len() == 4 {
        digits[2..].parse().unwrap_or(0)
    } else {
        0
    };
    if hours > 18 || minutes > 59 {
        return (t, None);
    }
    let core = t[..10 + sign_at].trim_end();
    (core, Some(sign * (hours * 3_600 + minutes * 60)))
}

/// The timestamp a cell holds: date/time text. Integers are not taken as
/// epochs, since nothing in a result says which epoch or unit they use.
pub fn cell_timestamp(cell: &CellValue) -> Option<NaiveDateTime> {
    match cell {
        CellValue::String(text) => parse_timestamp(text),
        _ => None,
    }
}

/// Whether a parsed timestamp has a time-of-day part other than midnight.
pub fn has_time_of_day(value: NaiveDateTime) -> bool {
    value.time() != NaiveTime::MIN
}

pub fn epoch_seconds(value: NaiveDateTime) -> i64 {
    value.and_utc().timestamp()
}

pub fn from_epoch_seconds(seconds: i64) -> Option<NaiveDateTime> {
    chrono::DateTime::from_timestamp(seconds, 0).map(|dt| dt.naive_utc())
}

/// The start of the bucket holding `value`.
pub fn floor_to_grain(value: NaiveDateTime, grain: TimeGrain) -> NaiveDateTime {
    let date = value.date();
    let midnight = |d: NaiveDate| d.and_time(NaiveTime::MIN);
    match grain {
        TimeGrain::Second => value.with_nanosecond(0).unwrap_or(value),
        TimeGrain::Minute => date.and_time(
            NaiveTime::from_hms_opt(value.hour(), value.minute(), 0).unwrap_or(NaiveTime::MIN),
        ),
        TimeGrain::Hour => {
            date.and_time(NaiveTime::from_hms_opt(value.hour(), 0, 0).unwrap_or(NaiveTime::MIN))
        }
        TimeGrain::Day => midnight(date),
        TimeGrain::Week => {
            midnight(date - Duration::days(i64::from(date.weekday().num_days_from_monday())))
        }
        TimeGrain::Month => midnight(date.with_day(1).unwrap_or(date)),
        TimeGrain::Quarter => {
            let month = (date.month() - 1) / 3 * 3 + 1;
            midnight(NaiveDate::from_ymd_opt(date.year(), month, 1).unwrap_or(date))
        }
        TimeGrain::Year => midnight(NaiveDate::from_ymd_opt(date.year(), 1, 1).unwrap_or(date)),
    }
}

/// The start of the bucket after the one starting at `start`.
pub fn next_bucket(start: NaiveDateTime, grain: TimeGrain) -> NaiveDateTime {
    let add_months = |months: u32| {
        start
            .date()
            .checked_add_months(chrono::Months::new(months))
            .map(|date| date.and_time(start.time()))
            .unwrap_or(start)
    };
    match grain {
        TimeGrain::Second => start + Duration::seconds(1),
        TimeGrain::Minute => start + Duration::minutes(1),
        TimeGrain::Hour => start + Duration::hours(1),
        TimeGrain::Day => start + Duration::days(1),
        TimeGrain::Week => start + Duration::days(7),
        TimeGrain::Month => add_months(1),
        TimeGrain::Quarter => add_months(3),
        TimeGrain::Year => add_months(12),
    }
}

/// The start of the bucket before the one starting at `start`.
pub fn previous_bucket(start: NaiveDateTime, grain: TimeGrain) -> NaiveDateTime {
    let sub_months = |months: u32| {
        start
            .date()
            .checked_sub_months(chrono::Months::new(months))
            .map(|date| date.and_time(start.time()))
            .unwrap_or(start)
    };
    match grain {
        TimeGrain::Second => start - Duration::seconds(1),
        TimeGrain::Minute => start - Duration::minutes(1),
        TimeGrain::Hour => start - Duration::hours(1),
        TimeGrain::Day => start - Duration::days(1),
        TimeGrain::Week => start - Duration::days(7),
        TimeGrain::Month => sub_months(1),
        TimeGrain::Quarter => sub_months(3),
        TimeGrain::Year => sub_months(12),
    }
}

/// A stable, sortable label for the bucket starting at `start`:
/// `2024-01-05`, `2024-W01` style weeks are avoided in favour of the
/// Monday's date so labels sort as text.
pub fn bucket_label(start: NaiveDateTime, grain: TimeGrain) -> String {
    match grain {
        TimeGrain::Second => start.format("%Y-%m-%d %H:%M:%S").to_string(),
        TimeGrain::Minute | TimeGrain::Hour => start.format("%Y-%m-%d %H:%M").to_string(),
        TimeGrain::Day | TimeGrain::Week => start.format("%Y-%m-%d").to_string(),
        TimeGrain::Month => start.format("%Y-%m").to_string(),
        TimeGrain::Quarter => format!("{}-Q{}", start.year(), (start.month() - 1) / 3 + 1),
        TimeGrain::Year => start.format("%Y").to_string(),
    }
}

/// The finest grain that puts the span `min..=max` into at most
/// `max_buckets` buckets. Date-only data never goes below a day.
pub fn choose_grain(
    min: NaiveDateTime,
    max: NaiveDateTime,
    max_buckets: usize,
    date_only: bool,
) -> TimeGrain {
    let span = (max - min).num_seconds().max(0) as f64;
    let max_buckets = max_buckets.max(1) as f64;
    TimeGrain::ALL
        .iter()
        .copied()
        .filter(|grain| !date_only || *grain >= TimeGrain::Day)
        .find(|grain| span / grain.approx_seconds() + 1.0 <= max_buckets)
        .unwrap_or(TimeGrain::Year)
}

/// Every bucket start from the bucket holding `min` to the one holding
/// `max`, capped at `cap` buckets.
pub fn bucket_starts(
    min: NaiveDateTime,
    max: NaiveDateTime,
    grain: TimeGrain,
    cap: usize,
) -> Vec<NaiveDateTime> {
    let mut starts = Vec::new();
    let mut current = floor_to_grain(min, grain);
    let last = floor_to_grain(max, grain);
    while current <= last && starts.len() < cap {
        starts.push(current);
        let next = next_bucket(current, grain);
        if next <= current {
            break;
        }
        current = next;
    }
    starts
}

/// Calendar distance from `a` to `b` in whole `unit`s (b − a, truncated
/// toward zero; months and years count calendar boundaries crossed by
/// complete months).
pub fn diff_in(unit: TimeGrain, a: NaiveDateTime, b: NaiveDateTime) -> i64 {
    let seconds = (b - a).num_seconds();
    match unit {
        TimeGrain::Second => seconds,
        TimeGrain::Minute => seconds / 60,
        TimeGrain::Hour => seconds / 3_600,
        TimeGrain::Day => seconds / 86_400,
        TimeGrain::Week => seconds / 604_800,
        TimeGrain::Month | TimeGrain::Quarter | TimeGrain::Year => {
            let mut months =
                i64::from(b.year() - a.year()) * 12 + i64::from(b.month()) - i64::from(a.month());
            let a_rest = (a.day(), a.time());
            let b_rest = (b.day(), b.time());
            if months > 0 && b_rest < a_rest {
                months -= 1;
            } else if months < 0 && b_rest > a_rest {
                months += 1;
            }
            match unit {
                TimeGrain::Month => months,
                TimeGrain::Quarter => months / 3,
                _ => months / 12,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(text: &str) -> NaiveDateTime {
        parse_timestamp(text).unwrap_or_else(|| panic!("parse {text}"))
    }

    #[test]
    fn engine_date_texts_parse_to_utc() {
        let cases = [
            ("2024-01-05", "2024-01-05 00:00:00"),
            ("2024/01/05", "2024-01-05 00:00:00"),
            ("2024-01-05 13:04:05", "2024-01-05 13:04:05"),
            ("2024-01-05T13:04:05.123", "2024-01-05 13:04:05"),
            ("2024-01-05T13:04:05Z", "2024-01-05 13:04:05"),
            ("2024-01-05 13:04:05+00", "2024-01-05 13:04:05"),
            ("2024-01-05 13:04:05 +05:30", "2024-01-05 07:34:05"),
            ("2024-01-05T13:04:05-0800", "2024-01-05 21:04:05"),
            ("2024-01-05 13:04", "2024-01-05 13:04:00"),
            ("2024-01-05 13:04:05 UTC", "2024-01-05 13:04:05"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                dt(input).format("%Y-%m-%d %H:%M:%S").to_string(),
                expected,
                "{input}"
            );
        }
        for bad in [
            "",
            "2024",
            "01/05/2024",
            "2024-13-01",
            "hello world!",
            "2024-01-05x",
        ] {
            assert_eq!(parse_timestamp(bad), None, "{bad}");
        }
        assert_eq!(cell_timestamp(&CellValue::I64(1_700_000_000)), None);
    }

    #[test]
    fn grains_floor_label_and_step() {
        let value = dt("2024-05-15 13:47:12");
        let cases = [
            (TimeGrain::Minute, "2024-05-15 13:47", "2024-05-15 13:48"),
            (TimeGrain::Hour, "2024-05-15 13:00", "2024-05-15 14:00"),
            (TimeGrain::Day, "2024-05-15", "2024-05-16"),
            (TimeGrain::Week, "2024-05-13", "2024-05-20"),
            (TimeGrain::Month, "2024-05", "2024-06"),
            (TimeGrain::Quarter, "2024-Q2", "2024-Q3"),
            (TimeGrain::Year, "2024", "2025"),
        ];
        for (grain, label, next) in cases {
            let start = floor_to_grain(value, grain);
            assert_eq!(bucket_label(start, grain), label, "{grain:?}");
            assert_eq!(
                bucket_label(next_bucket(start, grain), grain),
                next,
                "{grain:?}"
            );
            assert_eq!(previous_bucket(next_bucket(start, grain), grain), start);
        }
        let jan31 = dt("2024-01-31");
        assert_eq!(
            bucket_label(
                next_bucket(floor_to_grain(jan31, TimeGrain::Month), TimeGrain::Month),
                TimeGrain::Month
            ),
            "2024-02"
        );
    }

    #[test]
    fn grain_choice_fits_the_bucket_budget() {
        let start = dt("2024-01-01");
        assert_eq!(
            choose_grain(start, dt("2024-01-20"), 60, true),
            TimeGrain::Day
        );
        assert_eq!(
            choose_grain(start, dt("2024-06-01"), 60, true),
            TimeGrain::Week
        );
        assert_eq!(
            choose_grain(start, dt("2027-01-01"), 60, true),
            TimeGrain::Month
        );
        assert_eq!(
            choose_grain(start, dt("2034-01-01"), 60, true),
            TimeGrain::Quarter
        );
        assert_eq!(
            choose_grain(start, dt("2200-01-01"), 60, true),
            TimeGrain::Year
        );
        assert_eq!(
            choose_grain(start, dt("2024-01-01 00:45"), 60, false),
            TimeGrain::Minute
        );
        assert_eq!(
            choose_grain(start, dt("2024-01-01 05:00"), 60, false),
            TimeGrain::Hour
        );
        assert_eq!(choose_grain(start, start, 60, false), TimeGrain::Second);
        assert_eq!(
            bucket_starts(start, dt("2024-03-15"), TimeGrain::Month, 10).len(),
            3
        );
        assert_eq!(
            bucket_starts(start, dt("2030-03-15"), TimeGrain::Day, 10).len(),
            10
        );
    }

    #[test]
    fn calendar_differences() {
        let a = dt("2024-01-31");
        assert_eq!(diff_in(TimeGrain::Day, a, dt("2024-03-01")), 30);
        assert_eq!(diff_in(TimeGrain::Month, a, dt("2024-02-29")), 0);
        assert_eq!(diff_in(TimeGrain::Month, a, dt("2024-03-31")), 2);
        assert_eq!(diff_in(TimeGrain::Month, dt("2024-03-31"), a), -2);
        assert_eq!(
            diff_in(TimeGrain::Year, dt("2020-02-29"), dt("2024-02-28")),
            3
        );
        assert_eq!(diff_in(TimeGrain::Hour, a, dt("2024-01-31 05:59")), 5);
        assert_eq!(diff_in(TimeGrain::Week, a, dt("2024-02-14")), 2);
    }
}
