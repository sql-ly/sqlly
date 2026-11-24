//! Five-field cron expressions and their next run time in a time zone.
//!
//! Syntax: `minute hour day-of-month month day-of-week`, each a `*`, a
//! value, a range `a-b`, a step `*/n`, `a-b/n` or `a/n`, or a comma list;
//! month and weekday names (`jan`, `mon`) work; weekday 0 and 7 are
//! Sunday; `@hourly`, `@daily`, `@midnight`, `@weekly`, `@monthly`,
//! `@yearly` and `@annually` are shorthands. As in Vixie cron, when both
//! day fields are restricted a day matches if either does; when either
//! starts with `*` both must match.
//!
//! Daylight saving time, in the job's zone:
//! - a time that does not exist (the clock jumps forward over it) runs once
//!   at the moment the clock jumps, as cron does;
//! - a time that happens twice (the clock falls back) runs once, at the
//!   first occurrence.

use std::collections::BTreeSet;

use chrono::{
    DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    pub minutes: BTreeSet<u32>,
    pub hours: BTreeSet<u32>,
    pub days: BTreeSet<u32>,
    pub months: BTreeSet<u32>,
    /// 0 = Sunday.
    pub weekdays: BTreeSet<u32>,
    pub day_star: bool,
    pub weekday_star: bool,
    /// The five fields as written (shorthands expanded).
    pub fields: [String; 5],
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn value(text: &str, min: u32, max: u32, names: &[&str], name_base: u32) -> Result<u32, String> {
    let lower = text.to_ascii_lowercase();
    if let Some(i) = names.iter().position(|n| *n == lower) {
        return Ok(i as u32 + name_base);
    }
    let n: u32 = text
        .parse()
        .map_err(|_| format!("'{text}' is not a number"))?;
    if n < min || n > max {
        return Err(format!("{n} is outside {min}-{max}"));
    }
    Ok(n)
}

fn field(
    text: &str,
    min: u32,
    max: u32,
    names: &[&str],
    name_base: u32,
) -> Result<BTreeSet<u32>, String> {
    let mut out = BTreeSet::new();
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s.parse().map_err(|_| format!("bad step '{s}'"))?;
                if step == 0 {
                    return Err("a step cannot be 0".into());
                }
                (r, step)
            }
            None => (item, 1),
        };
        let (lo, hi) = if range == "*" || range == "?" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let (a, b) = (
                value(a, min, max, names, name_base)?,
                value(b, min, max, names, name_base)?,
            );
            if a > b {
                return Err(format!("range {a}-{b} runs backwards"));
            }
            (a, b)
        } else {
            let a = value(range, min, max, names, name_base)?;
            (a, if item.contains('/') { max } else { a })
        };
        out.extend((lo..=hi).step_by(step as usize));
    }
    if out.is_empty() {
        return Err(format!("'{text}' selects nothing"));
    }
    Ok(out)
}

impl Cron {
    pub fn parse(text: &str) -> Result<Cron, String> {
        let text = text.trim();
        let expanded = match text.to_ascii_lowercase().as_str() {
            "@yearly" | "@annually" => "0 0 1 1 *".to_string(),
            "@monthly" => "0 0 1 * *".to_string(),
            "@weekly" => "0 0 * * 0".to_string(),
            "@daily" | "@midnight" => "0 0 * * *".to_string(),
            "@hourly" => "0 * * * *".to_string(),
            other if other.starts_with('@') => {
                return Err(format!("unsupported schedule '{text}'"));
            }
            _ => text.to_string(),
        };
        let parts: Vec<&str> = expanded.split_whitespace().collect();
        let [minute, hour, day, month, weekday] = parts.as_slice() else {
            return Err(format!(
                "a cron expression has 5 fields (minute hour day month weekday), got {}",
                parts.len()
            ));
        };
        let wrap = |name: &str, r: Result<BTreeSet<u32>, String>| {
            r.map_err(|e| format!("{name} field: {e}"))
        };
        let mut weekdays = wrap("weekday", field(weekday, 0, 7, &WEEKDAYS, 0))?;
        if weekdays.remove(&7) {
            weekdays.insert(0);
        }
        Ok(Cron {
            minutes: wrap("minute", field(minute, 0, 59, &[], 0))?,
            hours: wrap("hour", field(hour, 0, 23, &[], 0))?,
            days: wrap("day", field(day, 1, 31, &[], 0))?,
            months: wrap("month", field(month, 1, 12, &MONTHS, 1))?,
            weekdays,
            day_star: day.starts_with('*') || day.starts_with('?'),
            weekday_star: weekday.starts_with('*') || weekday.starts_with('?'),
            fields: [
                minute.to_string(),
                hour.to_string(),
                day.to_string(),
                month.to_string(),
                weekday.to_string(),
            ],
        })
    }

    pub fn expression(&self) -> String {
        self.fields.join(" ")
    }

    /// Both day fields restricted: a day matches when either does.
    pub fn days_or(&self) -> bool {
        !self.day_star && !self.weekday_star
    }

    pub fn day_matches(&self, date: NaiveDate) -> bool {
        let dom = self.days.contains(&date.day());
        let dow = self
            .weekdays
            .contains(&date.weekday().num_days_from_sunday());
        if self.days_or() {
            dom || dow
        } else {
            dom && dow
        }
    }

    fn matches(&self, t: &NaiveDateTime) -> bool {
        self.months.contains(&t.month())
            && self.day_matches(t.date())
            && self.hours.contains(&t.hour())
            && self.minutes.contains(&t.minute())
    }

    /// The first run strictly after `after`, in `tz`.
    pub fn next_after<Z: TimeZone>(&self, tz: &Z, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let local = after.with_timezone(tz).naive_local();
        let mut t = local.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        let horizon = local.year() + 9;
        for _ in 0..600_000 {
            if t.year() > horizon {
                return None;
            }
            if !self.months.contains(&t.month()) {
                let (y, m) = if t.month() == 12 {
                    (t.year() + 1, 1)
                } else {
                    (t.year(), t.month() + 1)
                };
                t = NaiveDate::from_ymd_opt(y, m, 1)?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.day_matches(t.date()) {
                t = t.date().succ_opt()?.and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.hours.contains(&t.hour()) {
                t = t.with_minute(0)? + Duration::hours(1);
                continue;
            }
            if !self.matches(&t) {
                t += Duration::minutes(1);
                continue;
            }
            let instant = match tz.from_local_datetime(&t) {
                LocalResult::Single(x) => Some(x.with_timezone(&Utc)),
                LocalResult::Ambiguous(first, _) => Some(first.with_timezone(&Utc)),
                LocalResult::None => gap_end(tz, t),
            };
            if let Some(instant) = instant
                && instant > after
            {
                return Some(instant);
            }
            t += Duration::minutes(1);
        }
        None
    }

    /// The next `n` runs after `after`.
    pub fn upcoming<Z: TimeZone>(
        &self,
        tz: &Z,
        after: DateTime<Utc>,
        n: usize,
    ) -> Vec<DateTime<Utc>> {
        let mut out = Vec::with_capacity(n);
        let mut at = after;
        while out.len() < n {
            match self.next_after(tz, at) {
                Some(next) => {
                    out.push(next);
                    at = next;
                }
                None => break,
            }
        }
        out
    }
}

/// The instant the clock lands on after skipping over `t`.
fn gap_end<Z: TimeZone>(tz: &Z, t: NaiveDateTime) -> Option<DateTime<Utc>> {
    (1..=24 * 60).find_map(|m| {
        tz.from_local_datetime(&(t + Duration::minutes(m)))
            .earliest()
            .map(|x| x.with_timezone(&Utc))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Tz;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    fn zone(name: &str) -> Tz {
        name.parse().unwrap()
    }

    #[test]
    fn parses_fields_names_steps_and_shorthands() {
        let c = Cron::parse("*/15 9-17 * jan-mar mon-fri").unwrap();
        assert_eq!(c.minutes, BTreeSet::from([0, 15, 30, 45]));
        assert_eq!(c.hours.len(), 9);
        assert_eq!(c.months, BTreeSet::from([1, 2, 3]));
        assert_eq!(c.weekdays, BTreeSet::from([1, 2, 3, 4, 5]));
        assert_eq!(
            Cron::parse("0 0 * * 7").unwrap().weekdays,
            BTreeSet::from([0])
        );
        assert_eq!(
            Cron::parse("5/20 * * * *").unwrap().minutes,
            BTreeSet::from([5, 25, 45])
        );
        assert_eq!(Cron::parse("@daily").unwrap().expression(), "0 0 * * *");
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* * 0 * *",
            "*/0 * * * *",
            "5-1 * * * *",
            "@reboot",
            "x * * * *",
        ] {
            assert!(Cron::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn weekday_mornings_in_utc() {
        let c = Cron::parse("0 7 * * 1-5").unwrap();
        // Friday 2026-11-20 08:00 → Monday 07:00.
        assert_eq!(
            c.next_after(&Utc, utc(2026, 11, 20, 8, 0)),
            Some(utc(2026, 11, 23, 7, 0))
        );
        // Strictly after: exactly 07:00 Monday → Tuesday.
        assert_eq!(
            c.next_after(&Utc, utc(2026, 11, 23, 7, 0)),
            Some(utc(2026, 11, 24, 7, 0))
        );
    }

    #[test]
    fn time_zones_shift_the_instant() {
        let c = Cron::parse("0 9 * * *").unwrap();
        // 09:00 in New York in November is 14:00 UTC.
        assert_eq!(
            c.next_after(&zone("America/New_York"), utc(2026, 11, 18, 0, 0)),
            Some(utc(2026, 11, 18, 14, 0))
        );
        // India's half-hour offset.
        assert_eq!(
            c.next_after(&zone("Asia/Kolkata"), utc(2026, 11, 18, 0, 0)),
            Some(utc(2026, 11, 18, 3, 30))
        );
    }

    #[test]
    fn spring_forward_runs_the_skipped_time_once_at_the_jump() {
        let london = zone("Europe/London");
        let c = Cron::parse("30 1 * * *").unwrap();
        // 2026-03-29: clocks go 01:00 GMT → 02:00 BST; 01:30 never happens.
        let next = c.next_after(&london, utc(2026, 3, 28, 12, 0)).unwrap();
        assert_eq!(next, utc(2026, 3, 29, 1, 0)); // 02:00 BST, the jump
        // The next day is back to normal: 01:30 BST = 00:30 UTC.
        assert_eq!(c.next_after(&london, next), Some(utc(2026, 3, 30, 0, 30)));
        // Every-15-minutes jobs inside the gap collapse to one run.
        let q = Cron::parse("*/15 1 * * *").unwrap();
        let runs = q.upcoming(&london, utc(2026, 3, 29, 0, 0), 2);
        assert_eq!(runs, vec![utc(2026, 3, 29, 1, 0), utc(2026, 3, 30, 0, 0)]);
    }

    #[test]
    fn fall_back_runs_the_repeated_time_once() {
        let london = zone("Europe/London");
        let c = Cron::parse("30 1 * * *").unwrap();
        // 2026-10-25: 01:30 happens at 00:30 UTC (BST) and 01:30 UTC (GMT).
        let first = c.next_after(&london, utc(2026, 10, 24, 12, 0)).unwrap();
        assert_eq!(first, utc(2026, 10, 25, 0, 30));
        assert_eq!(c.next_after(&london, first), Some(utc(2026, 10, 26, 1, 30)));
        // Hourly jobs keep one run per wall-clock hour (01:00 once).
        let hourly = Cron::parse("0 * * * *").unwrap();
        let runs = hourly.upcoming(&london, utc(2026, 10, 24, 23, 30), 3);
        assert_eq!(
            runs,
            vec![
                utc(2026, 10, 25, 0, 0),
                utc(2026, 10, 25, 2, 0),
                utc(2026, 10, 25, 3, 0)
            ]
        );
    }

    #[test]
    fn day_fields_or_when_both_restricted() {
        // The 13th, or any Friday.
        let c = Cron::parse("0 0 13 * 5").unwrap();
        assert!(c.days_or());
        let runs = c.upcoming(&Utc, utc(2026, 11, 1, 0, 0), 3);
        assert_eq!(
            runs,
            vec![
                utc(2026, 11, 6, 0, 0),
                utc(2026, 11, 13, 0, 0),
                utc(2026, 11, 20, 0, 0)
            ]
        );
        // `*/2` counts as a star: odd days that are also Mondays.
        let and = Cron::parse("0 0 */2 * 1").unwrap();
        assert!(!and.days_or());
        assert_eq!(
            and.next_after(&Utc, utc(2026, 11, 1, 0, 0)),
            Some(utc(2026, 11, 9, 0, 0))
        );
    }

    #[test]
    fn leap_days_and_impossible_dates() {
        let c = Cron::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            c.next_after(&Utc, utc(2026, 11, 18, 0, 0)),
            Some(utc(2028, 2, 29, 0, 0))
        );
        assert_eq!(
            Cron::parse("0 0 31 2 *")
                .unwrap()
                .next_after(&Utc, utc(2026, 1, 1, 0, 0)),
            None
        );
    }
}
