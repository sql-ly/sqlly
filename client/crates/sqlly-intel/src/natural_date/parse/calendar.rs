//! Calendar arithmetic used by the grammar. Everything is checked: a phrase
//! like "99999999 days ago" must come back as `None`, never a panic.

use chrono::{Datelike, Days, Months, NaiveDate, NaiveDateTime, TimeDelta, Weekday};

/// A calendar or clock unit a phrase can count in ("3 weeks ago").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Unit {
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl Unit {
    pub(super) fn parse(word: &str) -> Option<Unit> {
        Some(match word {
            "second" | "seconds" | "sec" | "secs" => Unit::Second,
            "minute" | "minutes" | "min" | "mins" => Unit::Minute,
            "hour" | "hours" | "hr" | "hrs" => Unit::Hour,
            "day" | "days" => Unit::Day,
            "week" | "weeks" | "wk" | "wks" => Unit::Week,
            "month" | "months" | "mo" | "mos" => Unit::Month,
            "quarter" | "quarters" | "qtr" | "qtrs" => Unit::Quarter,
            "year" | "years" | "yr" | "yrs" => Unit::Year,
            _ => return None,
        })
    }

    /// Clock units shift instants; calendar units shift whole days.
    pub(super) fn is_clock(self) -> bool {
        matches!(self, Unit::Second | Unit::Minute | Unit::Hour)
    }
}

pub(super) fn midnight(d: NaiveDate) -> NaiveDateTime {
    d.and_hms_opt(0, 0, 0).expect("midnight is always valid")
}

pub(super) fn add_days(d: NaiveDate, n: i64) -> Option<NaiveDate> {
    if n >= 0 {
        d.checked_add_days(Days::new(n.unsigned_abs()))
    } else {
        d.checked_sub_days(Days::new(n.unsigned_abs()))
    }
}

pub(super) fn add_months(d: NaiveDate, n: i64) -> Option<NaiveDate> {
    let months = Months::new(u32::try_from(n.unsigned_abs()).ok()?);
    if n >= 0 {
        d.checked_add_months(months)
    } else {
        d.checked_sub_months(months)
    }
}

/// Shift a date by `n` calendar units. Month arithmetic clamps to the end of
/// the month (Mar 31 − 1 month = Feb 28), matching chrono and most SQL
/// engines' DATEADD.
pub(super) fn shift_date(d: NaiveDate, unit: Unit, n: i64) -> Option<NaiveDate> {
    match unit {
        Unit::Day => add_days(d, n),
        Unit::Week => add_days(d, n.checked_mul(7)?),
        Unit::Month => add_months(d, n),
        Unit::Quarter => add_months(d, n.checked_mul(3)?),
        Unit::Year => add_months(d, n.checked_mul(12)?),
        Unit::Second | Unit::Minute | Unit::Hour => None,
    }
}

/// Shift a timestamp by `n` units of any kind.
pub(super) fn shift_dt(dt: NaiveDateTime, unit: Unit, n: i64) -> Option<NaiveDateTime> {
    let delta = match unit {
        Unit::Second => TimeDelta::try_seconds(n)?,
        Unit::Minute => TimeDelta::try_minutes(n)?,
        Unit::Hour => TimeDelta::try_hours(n)?,
        _ => return Some(shift_date(dt.date(), unit, n)?.and_time(dt.time())),
    };
    dt.checked_add_signed(delta)
}

/// First day of the week containing `d`.
pub(super) fn week_start(d: NaiveDate, first: Weekday) -> Option<NaiveDate> {
    let back = (d.weekday().num_days_from_monday() + 7 - first.num_days_from_monday()) % 7;
    add_days(d, -i64::from(back))
}

pub(super) fn ymd(y: i32, m: u32, d: u32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(y, m, d)
}

/// `[start, end)` dates of the calendar period of `unit` containing `d`.
/// `fiscal_start` only matters for [`Period::FiscalYear`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Period {
    Day,
    Week,
    Month,
    Quarter,
    Year,
    FiscalYear,
}

impl Period {
    pub(super) fn parse(word: &str) -> Option<Period> {
        Some(match Unit::parse(word)? {
            Unit::Day => Period::Day,
            Unit::Week => Period::Week,
            Unit::Month => Period::Month,
            Unit::Quarter => Period::Quarter,
            Unit::Year => Period::Year,
            _ => return None,
        })
    }

    /// The unit one step of this period moves by.
    pub(super) fn unit(self) -> Unit {
        match self {
            Period::Day => Unit::Day,
            Period::Week => Unit::Week,
            Period::Month => Unit::Month,
            Period::Quarter => Unit::Quarter,
            Period::Year | Period::FiscalYear => Unit::Year,
        }
    }
}

pub(super) fn period_start(
    p: Period,
    d: NaiveDate,
    week_first: Weekday,
    fiscal_start: u32,
) -> Option<NaiveDate> {
    match p {
        Period::Day => Some(d),
        Period::Week => week_start(d, week_first),
        Period::Month => ymd(d.year(), d.month(), 1),
        Period::Quarter => ymd(d.year(), (d.month() - 1) / 3 * 3 + 1, 1),
        Period::Year => ymd(d.year(), 1, 1),
        Period::FiscalYear => {
            let m = fiscal_start.clamp(1, 12);
            let y = if d.month() >= m {
                d.year()
            } else {
                d.year() - 1
            };
            ymd(y, m, 1)
        }
    }
}

/// Start of fiscal year "FY`label`". A fiscal year that doesn't start in
/// January is named after the calendar year it ends in (FY2025 with a July
/// start runs Jul 2024 – Jun 2025), the common US corporate convention.
pub(super) fn fiscal_year_start(label: i32, fiscal_start: u32) -> Option<NaiveDate> {
    let m = fiscal_start.clamp(1, 12);
    if m == 1 {
        ymd(label, 1, 1)
    } else {
        ymd(label.checked_sub(1)?, m, 1)
    }
}

/// The `n`th (1-based) `wd` on or after `from`.
pub(super) fn nth_weekday_from(from: NaiveDate, wd: Weekday, n: u32) -> Option<NaiveDate> {
    let ahead = (wd.num_days_from_monday() + 7 - from.weekday().num_days_from_monday()) % 7;
    add_days(from, i64::from(ahead) + 7 * (i64::from(n) - 1))
}

/// The last `wd` strictly before `end`.
pub(super) fn last_weekday_before(end: NaiveDate, wd: Weekday) -> Option<NaiveDate> {
    let last = add_days(end, -1)?;
    let back = (last.weekday().num_days_from_monday() + 7 - wd.num_days_from_monday()) % 7;
    add_days(last, -i64::from(back))
}

pub(super) fn parse_weekday(word: &str) -> Option<Weekday> {
    Some(match word {
        "sunday" | "sun" | "suns" | "sundays" => Weekday::Sun,
        "monday" | "mon" | "mondays" => Weekday::Mon,
        "tuesday" | "tue" | "tues" | "tuesdays" => Weekday::Tue,
        "wednesday" | "wed" | "weds" | "wednesdays" => Weekday::Wed,
        "thursday" | "thu" | "thur" | "thurs" | "thursdays" => Weekday::Thu,
        "friday" | "fri" | "fridays" => Weekday::Fri,
        "saturday" | "sat" | "saturdays" => Weekday::Sat,
        _ => return None,
    })
}

pub(super) fn parse_month(word: &str) -> Option<u32> {
    Some(match word {
        "january" | "jan" => 1,
        "february" | "feb" => 2,
        "march" | "mar" => 3,
        "april" | "apr" => 4,
        "may" => 5,
        "june" | "jun" => 6,
        "july" | "jul" => 7,
        "august" | "aug" => 8,
        "september" | "sep" | "sept" => 9,
        "october" | "oct" => 10,
        "november" | "nov" => 11,
        "december" | "dec" => 12,
        _ => return None,
    })
}

/// "one".."twenty" — enough for "last twelve months" style phrases.
pub(super) fn number_word(word: &str) -> Option<u64> {
    const WORDS: [&str; 20] = [
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
        "twenty",
    ];
    WORDS.iter().position(|w| *w == word).map(|i| i as u64 + 1)
}

/// "first".."fifth" as written-out ordinals.
pub(super) fn ordinal_word(word: &str) -> Option<u32> {
    Some(match word {
        "first" => 1,
        "second" => 2,
        "third" => 3,
        "fourth" => 4,
        "fifth" => 5,
        _ => return None,
    })
}

/// A two-digit year: 00–69 → 2000s, 70–99 → 1900s.
pub(super) fn expand_short_year(y: u32) -> i32 {
    let y = y as i32;
    if y < 70 { 2000 + y } else { 1900 + y }
}

/// The handful of (US-centric) holidays people actually filter by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Holiday {
    NewYearsDay,
    NewYearsEve,
    ValentinesDay,
    MemorialDay,
    IndependenceDay,
    LaborDay,
    Halloween,
    Thanksgiving,
    ChristmasEve,
    Christmas,
}

/// Holiday spellings, longest first so "new years eve" wins over "new years".
pub(super) const HOLIDAYS: &[(&[&str], Holiday)] = &[
    (&["new", "years", "eve"], Holiday::NewYearsEve),
    (&["new", "year", "eve"], Holiday::NewYearsEve),
    (&["new", "years", "day"], Holiday::NewYearsDay),
    (&["new", "year", "day"], Holiday::NewYearsDay),
    (&["new", "years"], Holiday::NewYearsDay),
    (&["new", "year"], Holiday::NewYearsDay),
    (&["valentines", "day"], Holiday::ValentinesDay),
    (&["valentine", "day"], Holiday::ValentinesDay),
    (&["valentines"], Holiday::ValentinesDay),
    (&["memorial", "day"], Holiday::MemorialDay),
    (&["independence", "day"], Holiday::IndependenceDay),
    (&["fourth", "of", "july"], Holiday::IndependenceDay),
    (&["labor", "day"], Holiday::LaborDay),
    (&["labour", "day"], Holiday::LaborDay),
    (&["halloween"], Holiday::Halloween),
    (&["thanksgiving", "day"], Holiday::Thanksgiving),
    (&["thanksgiving"], Holiday::Thanksgiving),
    (&["christmas", "eve"], Holiday::ChristmasEve),
    (&["xmas", "eve"], Holiday::ChristmasEve),
    (&["christmas", "day"], Holiday::Christmas),
    (&["christmas"], Holiday::Christmas),
    (&["xmas"], Holiday::Christmas),
];

impl Holiday {
    pub(super) fn date(self, year: i32) -> Option<NaiveDate> {
        match self {
            Holiday::NewYearsDay => ymd(year, 1, 1),
            Holiday::NewYearsEve => ymd(year, 12, 31),
            Holiday::ValentinesDay => ymd(year, 2, 14),
            Holiday::MemorialDay => last_weekday_before(ymd(year, 6, 1)?, Weekday::Mon),
            Holiday::IndependenceDay => ymd(year, 7, 4),
            Holiday::LaborDay => nth_weekday_from(ymd(year, 9, 1)?, Weekday::Mon, 1),
            Holiday::Halloween => ymd(year, 10, 31),
            Holiday::Thanksgiving => nth_weekday_from(ymd(year, 11, 1)?, Weekday::Thu, 4),
            Holiday::ChristmasEve => ymd(year, 12, 24),
            Holiday::Christmas => ymd(year, 12, 25),
        }
    }
}
