//! Table-driven tests. `now` is Thursday 2026-10-22 10:30 unless a test
//! says otherwise; with a Sunday week start the current week is
//! Sun Oct 18 – Sat Oct 24.

use super::*;
use Granularity::*;
use chrono::NaiveDate;

fn at(s: &str) -> NaiveDateTime {
    for f in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(v) = NaiveDateTime::parse_from_str(s, f) {
            return v;
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .unwrap_or_else(|_| panic!("bad test timestamp {s}"))
        .and_time(NaiveTime::MIN)
}

fn now() -> NaiveDateTime {
    at("2026-10-22 10:30")
}

fn with(f: impl FnOnce(&mut DateParseOptions)) -> DateParseOptions {
    let mut o = DateParseOptions::default();
    f(&mut o);
    o
}

/// `(input, start, end, granularity)`; an empty `end` means "same as
/// start" (instants).
type Case = (&'static str, &'static str, &'static str, Granularity);

/// Runs every case and reports all mismatches at once.
fn check_at(opts: &DateParseOptions, now: NaiveDateTime, cases: &[Case]) {
    let mut failures = Vec::new();
    for &(input, start, end, g) in cases {
        let want_start = at(start);
        let want_end = if end.is_empty() { want_start } else { at(end) };
        match parse_natural_date(input, now, opts) {
            Ok(p) if p.start == want_start && p.end == want_end && p.granularity == g => {}
            other => failures.push(format!(
                "{input:?}: want {want_start}..{want_end} {g:?}, got {other:?}"
            )),
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

fn check(opts: &DateParseOptions, cases: &[Case]) {
    check_at(opts, now(), cases);
}

fn defaults(cases: &[Case]) {
    check(&DateParseOptions::default(), cases);
}

#[test]
fn keywords_and_input_tolerance() {
    defaults(&[
        ("now", "2026-10-22 10:30", "", Instant),
        ("right now", "2026-10-22 10:30", "", Instant),
        ("today", "2026-10-22", "2026-10-23", Day),
        ("yesterday", "2026-10-21", "2026-10-22", Day),
        ("tomorrow", "2026-10-23", "2026-10-24", Day),
        ("the day before yesterday", "2026-10-20", "2026-10-21", Day),
        ("day after tomorrow", "2026-10-24", "2026-10-25", Day),
        ("  TODAY.  ", "2026-10-22", "2026-10-23", Day),
        ("\"yesterday\"", "2026-10-21", "2026-10-22", Day),
        ("'tomorrow'", "2026-10-23", "2026-10-24", Day),
        ("\u{201c}today\u{201d}", "2026-10-22", "2026-10-23", Day),
        ("Today!", "2026-10-22", "2026-10-23", Day),
        ("on   tuesday", "2026-10-20", "2026-10-21", Day),
        ("in november", "2025-11-01", "2025-12-01", Month),
    ]);
}

#[test]
fn calendar_periods() {
    defaults(&[
        ("this week", "2026-10-18", "2026-10-25", Week),
        ("last week", "2026-10-11", "2026-10-18", Week),
        ("previous week", "2026-10-11", "2026-10-18", Week),
        ("next week", "2026-10-25", "2026-11-01", Week),
        ("coming week", "2026-10-25", "2026-11-01", Week),
        ("this day", "2026-10-22", "2026-10-23", Day),
        ("previous day", "2026-10-21", "2026-10-22", Day),
        ("next day", "2026-10-23", "2026-10-24", Day),
        ("this month", "2026-10-01", "2026-11-01", Month),
        ("current month", "2026-10-01", "2026-11-01", Month),
        ("last month", "2026-09-01", "2026-10-01", Month),
        ("next month", "2026-11-01", "2026-12-01", Month),
        ("this quarter", "2026-10-01", "2027-01-01", Quarter),
        ("last quarter", "2026-07-01", "2026-10-01", Quarter),
        ("next quarter", "2027-01-01", "2027-04-01", Quarter),
        ("this year", "2026-01-01", "2027-01-01", Year),
        ("last year", "2025-01-01", "2026-01-01", Year),
        ("next year", "2027-01-01", "2028-01-01", Year),
        ("this weekend", "2026-10-24", "2026-10-26", Custom),
        ("last weekend", "2026-10-17", "2026-10-19", Custom),
        ("next weekend", "2026-10-31", "2026-11-02", Custom),
        ("weekend", "2026-10-17", "2026-10-19", Custom),
    ]);
}

#[test]
fn week_start_monday_shifts_weeks() {
    let o = with(|o| o.week_start = Weekday::Mon);
    check(
        &o,
        &[
            ("this week", "2026-10-19", "2026-10-26", Week),
            ("last week", "2026-10-12", "2026-10-19", Week),
            ("next week", "2026-10-26", "2026-11-02", Week),
            ("wtd", "2026-10-19", "2026-10-23", Custom),
            ("this sunday", "2026-10-25", "2026-10-26", Day),
            ("sunday last week", "2026-10-18", "2026-10-19", Day),
            ("start of this week", "2026-10-19", "2026-10-20", Day),
            ("end of week", "2026-10-25", "2026-10-26", Day),
            // The weekend never depends on the week start.
            ("this weekend", "2026-10-24", "2026-10-26", Custom),
        ],
    );
    // Sunday start for contrast.
    defaults(&[
        ("this sunday", "2026-10-18", "2026-10-19", Day),
        ("end of week", "2026-10-24", "2026-10-25", Day),
    ]);
}

#[test]
fn weekend_on_a_weekend_day() {
    let saturday = at("2026-10-24 09:00");
    let sunday = at("2026-10-25 09:00");
    let o = DateParseOptions::default();
    check_at(
        &o,
        saturday,
        &[
            ("this weekend", "2026-10-24", "2026-10-26", Custom),
            ("last weekend", "2026-10-17", "2026-10-19", Custom),
        ],
    );
    check_at(
        &o,
        sunday,
        &[
            ("this weekend", "2026-10-24", "2026-10-26", Custom),
            ("weekend", "2026-10-24", "2026-10-26", Custom),
        ],
    );
}

#[test]
fn to_date_periods() {
    defaults(&[
        ("ytd", "2026-01-01", "2026-10-23", Custom),
        ("YTD", "2026-01-01", "2026-10-23", Custom),
        ("year to date", "2026-01-01", "2026-10-23", Custom),
        ("this year so far", "2026-01-01", "2026-10-23", Custom),
        ("mtd", "2026-10-01", "2026-10-23", Custom),
        ("month to date", "2026-10-01", "2026-10-23", Custom),
        ("qtd", "2026-10-01", "2026-10-23", Custom),
        ("quarter to date", "2026-10-01", "2026-10-23", Custom),
        ("wtd", "2026-10-18", "2026-10-23", Custom),
        ("week to date", "2026-10-18", "2026-10-23", Custom),
        ("fytd", "2026-01-01", "2026-10-23", Custom),
    ]);
    let o = with(|o| o.to_date_end = ToDateEnd::Now);
    check(
        &o,
        &[
            ("ytd", "2026-01-01", "2026-10-22 10:30", Custom),
            ("mtd", "2026-10-01", "2026-10-22 10:30", Custom),
            (
                "this month so far",
                "2026-10-01",
                "2026-10-22 10:30",
                Custom,
            ),
            ("last 7 days", "2026-10-16", "2026-10-22 10:30", Custom),
            ("past 2 weeks", "2026-10-08", "2026-10-22 10:30", Custom),
            ("since monday", "2026-10-19", "2026-10-22 10:30", Custom),
        ],
    );
}

#[test]
fn fiscal_years() {
    defaults(&[
        ("this fiscal year", "2026-01-01", "2027-01-01", Year),
        ("fy2025", "2025-01-01", "2026-01-01", Year),
    ]);
    let july = with(|o| o.fiscal_year_start_month = 7);
    check(
        &july,
        &[
            ("this fiscal year", "2026-07-01", "2027-07-01", Year),
            ("fiscal year", "2026-07-01", "2027-07-01", Year),
            ("last fiscal year", "2025-07-01", "2026-07-01", Year),
            ("next fy", "2027-07-01", "2028-07-01", Year),
            ("fy2025", "2024-07-01", "2025-07-01", Year),
            ("FY 2025", "2024-07-01", "2025-07-01", Year),
            ("fy25", "2024-07-01", "2025-07-01", Year),
            ("fiscal year 2027", "2026-07-01", "2027-07-01", Year),
            ("fytd", "2026-07-01", "2026-10-23", Custom),
            ("fiscal year to date", "2026-07-01", "2026-10-23", Custom),
            ("start of fiscal year", "2026-07-01", "2026-07-02", Day),
            ("end of last fiscal year", "2026-06-30", "2026-07-01", Day),
        ],
    );
    let october = with(|o| o.fiscal_year_start_month = 10);
    check(
        &october,
        &[
            ("this fiscal year", "2026-10-01", "2027-10-01", Year),
            ("last fiscal year", "2025-10-01", "2026-10-01", Year),
            ("fy2026", "2025-10-01", "2026-10-01", Year),
            ("fytd", "2026-10-01", "2026-10-23", Custom),
        ],
    );
    // In September the October-start fiscal year is still last year's.
    check_at(
        &october,
        at("2026-09-15 08:00"),
        &[("this fiscal year", "2025-10-01", "2026-10-01", Year)],
    );
}

#[test]
fn rolling_windows() {
    defaults(&[
        ("last 7 days", "2026-10-16", "2026-10-23", Custom),
        ("past 7 days", "2026-10-16", "2026-10-23", Custom),
        ("past seven days", "2026-10-16", "2026-10-23", Custom),
        ("trailing 30 days", "2026-09-23", "2026-10-23", Custom),
        ("last 1 day", "2026-10-22", "2026-10-23", Custom),
        ("past 2 weeks", "2026-10-08", "2026-10-23", Custom),
        ("last 3 months", "2026-07-22", "2026-10-23", Custom),
        ("last twelve months", "2025-10-22", "2026-10-23", Custom),
        ("past year", "2025-10-22", "2026-10-23", Custom),
        ("last 2 years", "2024-10-22", "2026-10-23", Custom),
        ("last 2 quarters", "2026-04-22", "2026-10-23", Custom),
        (
            "past 3 hours",
            "2026-10-22 07:30",
            "2026-10-22 10:30",
            Custom,
        ),
        ("past hour", "2026-10-22 09:30", "2026-10-22 10:30", Custom),
        (
            "last 15 minutes",
            "2026-10-22 10:15",
            "2026-10-22 10:30",
            Custom,
        ),
        (
            "last 90 mins",
            "2026-10-22 09:00",
            "2026-10-22 10:30",
            Custom,
        ),
        ("the last few days", "2026-10-20", "2026-10-23", Custom),
        ("last couple of days", "2026-10-21", "2026-10-23", Custom),
        ("next 7 days", "2026-10-22", "2026-10-30", Custom),
        ("next 2 weeks", "2026-10-22", "2026-11-06", Custom),
        (
            "next 3 hours",
            "2026-10-22 10:30",
            "2026-10-22 13:30",
            Custom,
        ),
    ]);
}

#[test]
fn relative_offsets() {
    defaults(&[
        ("3 days ago", "2026-10-19", "2026-10-20", Day),
        ("five days ago", "2026-10-17", "2026-10-18", Day),
        ("a few days ago", "2026-10-19", "2026-10-20", Day),
        ("a week ago", "2026-10-15", "2026-10-16", Day),
        ("2 weeks ago", "2026-10-08", "2026-10-09", Day),
        ("a month ago", "2026-09-22", "2026-09-23", Day),
        ("1 year ago", "2025-10-22", "2025-10-23", Day),
        ("an hour ago", "2026-10-22 09:30", "", Instant),
        ("30 minutes ago", "2026-10-22 10:00", "", Instant),
        ("in 3 days", "2026-10-25", "2026-10-26", Day),
        ("in a week", "2026-10-29", "2026-10-30", Day),
        ("in 2 hours", "2026-10-22 12:30", "", Instant),
        ("3 days from now", "2026-10-25", "2026-10-26", Day),
        ("2 weeks from today", "2026-11-05", "2026-11-06", Day),
        ("an hour from now", "2026-10-22 11:30", "", Instant),
        ("2 days later", "2026-10-24", "2026-10-25", Day),
        ("3 days before christmas", "2025-12-22", "2025-12-23", Day),
        ("2 days after tomorrow", "2026-10-25", "2026-10-26", Day),
        (
            "1 month after last month",
            "2026-10-01",
            "2026-11-01",
            Month,
        ),
    ]);
}

#[test]
fn weekdays_by_preference() {
    defaults(&[
        ("wed", "2026-10-21", "2026-10-22", Day),
        ("wednesday", "2026-10-21", "2026-10-22", Day),
        ("weds", "2026-10-21", "2026-10-22", Day),
        ("tues", "2026-10-20", "2026-10-21", Day),
        ("thurs", "2026-10-22", "2026-10-23", Day), // today counts
        ("fri", "2026-10-16", "2026-10-17", Day),
        ("sat", "2026-10-17", "2026-10-18", Day),
        ("sun", "2026-10-18", "2026-10-19", Day),
        ("Monday", "2026-10-19", "2026-10-20", Day),
    ]);
    let future = with(|o| o.prefer = DatePreference::Future);
    check(
        &future,
        &[
            ("wed", "2026-10-28", "2026-10-29", Day),
            ("thu", "2026-10-22", "2026-10-23", Day), // today counts
            ("fri", "2026-10-23", "2026-10-24", Day),
            ("sun", "2026-10-25", "2026-10-26", Day),
        ],
    );
    let current = with(|o| o.prefer = DatePreference::Current);
    check(
        &current,
        &[
            ("wed", "2026-10-21", "2026-10-22", Day),
            ("fri", "2026-10-23", "2026-10-24", Day),
            ("sat", "2026-10-24", "2026-10-25", Day),
            ("sun", "2026-10-18", "2026-10-19", Day),
        ],
    );
    let current_mon = with(|o| {
        o.prefer = DatePreference::Current;
        o.week_start = Weekday::Mon;
    });
    check(&current_mon, &[("sun", "2026-10-25", "2026-10-26", Day)]);
}

#[test]
fn qualified_weekdays() {
    // Qualifiers ignore `prefer`.
    for prefer in [
        DatePreference::Past,
        DatePreference::Future,
        DatePreference::Current,
    ] {
        check(
            &with(|o| o.prefer = prefer),
            &[
                ("last wed", "2026-10-21", "2026-10-22", Day),
                ("last thu", "2026-10-15", "2026-10-16", Day),
                ("last friday", "2026-10-16", "2026-10-17", Day),
                ("previous saturday", "2026-10-17", "2026-10-18", Day),
                ("next friday", "2026-10-23", "2026-10-24", Day),
                ("next wed", "2026-10-28", "2026-10-29", Day),
                ("next thursday", "2026-10-29", "2026-10-30", Day),
                ("coming monday", "2026-10-26", "2026-10-27", Day),
                ("this monday", "2026-10-19", "2026-10-20", Day),
                ("this saturday", "2026-10-24", "2026-10-25", Day),
                ("monday last week", "2026-10-12", "2026-10-13", Day),
                ("friday next week", "2026-10-30", "2026-10-31", Day),
                ("wed of next week", "2026-10-28", "2026-10-29", Day),
                ("last week monday", "2026-10-12", "2026-10-13", Day),
                ("tuesday this week", "2026-10-20", "2026-10-21", Day),
            ],
        );
    }
}

#[test]
fn clock_times_alone_mean_today() {
    defaults(&[
        ("at 3", "2026-10-22 15:00", "", Instant),
        ("at 3pm", "2026-10-22 15:00", "", Instant),
        ("3pm", "2026-10-22 15:00", "", Instant),
        ("3 PM", "2026-10-22 15:00", "", Instant),
        ("3 p.m.", "2026-10-22 15:00", "", Instant),
        ("3:30", "2026-10-22 15:30", "", Instant),
        ("15:30", "2026-10-22 15:30", "", Instant),
        ("3:30 pm", "2026-10-22 15:30", "", Instant),
        ("3:30pm", "2026-10-22 15:30", "", Instant),
        ("3:30:15 am", "2026-10-22 03:30:15", "", Instant),
        ("noon", "2026-10-22 12:00", "", Instant),
        ("at noon", "2026-10-22 12:00", "", Instant),
        ("midnight", "2026-10-22 00:00", "", Instant),
        ("at 9am", "2026-10-22 09:00", "", Instant),
        ("@ 9am", "2026-10-22 09:00", "", Instant),
        ("at 9", "2026-10-22 09:00", "", Instant),
        ("at 12", "2026-10-22 12:00", "", Instant),
        ("at 7", "2026-10-22 19:00", "", Instant),
        ("at 8", "2026-10-22 08:00", "", Instant),
        ("at 0", "2026-10-22 00:00", "", Instant),
        ("at 15", "2026-10-22 15:00", "", Instant),
        ("at 03:00", "2026-10-22 03:00", "", Instant),
        ("12am", "2026-10-22 00:00", "", Instant),
        ("12pm", "2026-10-22 12:00", "", Instant),
        ("9 o'clock", "2026-10-22 09:00", "", Instant),
        ("morning", "2026-10-22 06:00", "2026-10-22 12:00", Custom),
        (
            "this morning",
            "2026-10-22 06:00",
            "2026-10-22 12:00",
            Custom,
        ),
        ("afternoon", "2026-10-22 12:00", "2026-10-22 17:00", Custom),
        (
            "this evening",
            "2026-10-22 17:00",
            "2026-10-22 21:00",
            Custom,
        ),
        (
            "in the morning",
            "2026-10-22 06:00",
            "2026-10-22 12:00",
            Custom,
        ),
        ("tonight", "2026-10-22 18:00", "2026-10-23 00:00", Custom),
        ("last night", "2026-10-21 18:00", "2026-10-22 00:00", Custom),
    ]);
}

#[test]
fn bare_hour_twenty_four() {
    let o = with(|o| o.bare_hour = BareHour::TwentyFourHour);
    check(
        &o,
        &[
            ("at 3", "2026-10-22 03:00", "", Instant),
            ("3:30", "2026-10-22 03:30", "", Instant),
            ("at 7", "2026-10-22 07:00", "", Instant),
            ("at 15", "2026-10-22 15:00", "", Instant),
            ("3pm", "2026-10-22 15:00", "", Instant),
            ("last wed at 3", "2026-10-21 03:00", "", Instant),
        ],
    );
}

#[test]
fn days_with_times() {
    defaults(&[
        ("last wed at 3", "2026-10-21 15:00", "", Instant),
        ("yesterday at noon", "2026-10-21 12:00", "", Instant),
        ("today at 3", "2026-10-22 15:00", "", Instant),
        (
            "tomorrow morning",
            "2026-10-23 06:00",
            "2026-10-23 12:00",
            Custom,
        ),
        (
            "tomorrow in the morning",
            "2026-10-23 06:00",
            "2026-10-23 12:00",
            Custom,
        ),
        (
            "yesterday afternoon",
            "2026-10-21 12:00",
            "2026-10-21 17:00",
            Custom,
        ),
        (
            "monday night",
            "2026-10-19 18:00",
            "2026-10-20 00:00",
            Custom,
        ),
        ("nov 5 at 3:15pm", "2025-11-05 15:15", "", Instant),
        ("3pm tomorrow", "2026-10-23 15:00", "", Instant),
        ("noon on friday", "2026-10-16 12:00", "", Instant),
        ("at 9 on monday", "2026-10-19 09:00", "", Instant),
        ("friday at 9am", "2026-10-16 09:00", "", Instant),
        ("November 5 2024 3pm", "2024-11-05 15:00", "", Instant),
        ("2024-11-05 15:30", "2024-11-05 15:30", "", Instant),
        ("2024-11-05T15:30:00", "2024-11-05 15:30", "", Instant),
        ("2024-11-05 03:00", "2024-11-05 03:00", "", Instant),
        (
            "2024-11-05 15:30:45.123",
            "2024-11-05 15:30:45",
            "",
            Instant,
        ),
        ("3 days ago at 5pm", "2026-10-19 17:00", "", Instant),
    ]);
}

#[test]
fn months_by_preference() {
    defaults(&[
        ("november", "2025-11-01", "2025-12-01", Month),
        ("nov", "2025-11-01", "2025-12-01", Month),
        ("sept", "2026-09-01", "2026-10-01", Month),
        ("october", "2026-10-01", "2026-11-01", Month),
        ("november 2024", "2024-11-01", "2024-12-01", Month),
        ("nov '24", "2024-11-01", "2024-12-01", Month),
        ("nov 5", "2025-11-05", "2025-11-06", Day),
        ("november 5th", "2025-11-05", "2025-11-06", Day),
        ("5 nov", "2025-11-05", "2025-11-06", Day),
        ("5th of november", "2025-11-05", "2025-11-06", Day),
        ("nov 5 2024", "2024-11-05", "2024-11-06", Day),
        ("Nov 5, 2024", "2024-11-05", "2024-11-06", Day),
        ("5 nov 2024", "2024-11-05", "2024-11-06", Day),
        ("oct 22", "2026-10-22", "2026-10-23", Day),
        ("oct 23", "2025-10-23", "2025-10-24", Day),
        ("wednesday, oct 21", "2026-10-21", "2026-10-22", Day),
        ("feb 29", "2024-02-29", "2024-03-01", Day),
    ]);
    let future = with(|o| o.prefer = DatePreference::Future);
    check(
        &future,
        &[
            ("november", "2026-11-01", "2026-12-01", Month),
            ("october", "2026-10-01", "2026-11-01", Month),
            ("september", "2027-09-01", "2027-10-01", Month),
            ("nov 5", "2026-11-05", "2026-11-06", Day),
            ("oct 21", "2027-10-21", "2027-10-22", Day),
            ("oct 22", "2026-10-22", "2026-10-23", Day),
        ],
    );
    let current = with(|o| o.prefer = DatePreference::Current);
    check(
        &current,
        &[
            ("november", "2026-11-01", "2026-12-01", Month),
            ("jan", "2026-01-01", "2026-02-01", Month),
            ("nov 5", "2026-11-05", "2026-11-06", Day),
        ],
    );
    // Once November has started, Past means this one.
    check_at(
        &DateParseOptions::default(),
        at("2026-11-10 09:00"),
        &[("november", "2026-11-01", "2026-12-01", Month)],
    );
}

#[test]
fn qualified_months() {
    defaults(&[
        ("last november", "2025-11-01", "2025-12-01", Month),
        ("next november", "2026-11-01", "2026-12-01", Month),
        ("last october", "2025-10-01", "2025-11-01", Month),
        ("next october", "2027-10-01", "2027-11-01", Month),
        ("last september", "2026-09-01", "2026-10-01", Month),
        ("this november", "2026-11-01", "2026-12-01", Month),
        ("november last year", "2025-11-01", "2025-12-01", Month),
        ("march next year", "2027-03-01", "2027-04-01", Month),
        ("march of next year", "2027-03-01", "2027-04-01", Month),
    ]);
}

#[test]
fn day_of_month_ordinals() {
    defaults(&[
        ("the 5th", "2026-10-05", "2026-10-06", Day),
        ("on the 22nd", "2026-10-22", "2026-10-23", Day),
        ("the 25th", "2026-09-25", "2026-09-26", Day),
        // September has no 31st, so Past walks back to August.
        ("the 31st", "2026-08-31", "2026-09-01", Day),
        ("5th of next month", "2026-11-05", "2026-11-06", Day),
        ("1st of last month", "2026-09-01", "2026-09-02", Day),
    ]);
    check(
        &with(|o| o.prefer = DatePreference::Future),
        &[("the 5th", "2026-11-05", "2026-11-06", Day)],
    );
    check(
        &with(|o| o.prefer = DatePreference::Current),
        &[
            ("the 5th", "2026-10-05", "2026-10-06", Day),
            ("the 31st", "2026-10-31", "2026-11-01", Day),
        ],
    );
}

#[test]
fn period_boundaries() {
    defaults(&[
        ("beginning of november", "2025-11-01", "2025-11-02", Day),
        ("start of november", "2025-11-01", "2025-11-02", Day),
        ("end of november", "2025-11-30", "2025-12-01", Day),
        ("first day of next month", "2026-11-01", "2026-11-02", Day),
        ("1st day of next month", "2026-11-01", "2026-11-02", Day),
        ("last day of this month", "2026-10-31", "2026-11-01", Day),
        ("last day of february", "2026-02-28", "2026-03-01", Day),
        ("last day of february 2024", "2024-02-29", "2024-03-01", Day),
        ("start of this week", "2026-10-18", "2026-10-19", Day),
        ("end of last month", "2026-09-30", "2026-10-01", Day),
        ("beginning of the year", "2026-01-01", "2026-01-02", Day),
        ("end of year", "2026-12-31", "2027-01-01", Day),
        ("start of next quarter", "2027-01-01", "2027-01-02", Day),
        ("start of today", "2026-10-22", "2026-10-23", Day),
        ("end of day", "2026-10-22 23:59:59", "", Instant),
        ("end of the day", "2026-10-22 23:59:59", "", Instant),
        ("eod", "2026-10-22 23:59:59", "", Instant),
        ("end of tomorrow", "2026-10-23 23:59:59", "", Instant),
        ("first monday of november", "2025-11-03", "2025-11-04", Day),
        ("last friday of this month", "2026-10-30", "2026-10-31", Day),
        (
            "second tuesday of next month",
            "2026-11-10",
            "2026-11-11",
            Day,
        ),
    ]);
}

#[test]
fn quarters() {
    defaults(&[
        ("q1", "2026-01-01", "2026-04-01", Quarter),
        ("Q4", "2026-10-01", "2027-01-01", Quarter), // started Oct 1
        ("q3 2024", "2024-07-01", "2024-10-01", Quarter),
        ("2024 q3", "2024-07-01", "2024-10-01", Quarter),
        ("2024q3", "2024-07-01", "2024-10-01", Quarter),
        ("q3 '24", "2024-07-01", "2024-10-01", Quarter),
        ("q3 of 2024", "2024-07-01", "2024-10-01", Quarter),
        ("first quarter", "2026-01-01", "2026-04-01", Quarter),
        ("2nd quarter", "2026-04-01", "2026-07-01", Quarter),
        ("fourth quarter 2025", "2025-10-01", "2026-01-01", Quarter),
        ("q3 of last year", "2025-07-01", "2025-10-01", Quarter),
        ("last q4", "2025-10-01", "2026-01-01", Quarter),
        ("next q1", "2027-01-01", "2027-04-01", Quarter),
    ]);
    check(
        &with(|o| o.prefer = DatePreference::Future),
        &[
            ("q1", "2027-01-01", "2027-04-01", Quarter),
            ("q4", "2026-10-01", "2027-01-01", Quarter),
        ],
    );
    check(
        &with(|o| o.prefer = DatePreference::Current),
        &[("q1", "2026-01-01", "2026-04-01", Quarter)],
    );
}

#[test]
fn years_and_absolute_dates() {
    defaults(&[
        ("2024", "2024-01-01", "2025-01-01", Year),
        ("the year 2023", "2023-01-01", "2024-01-01", Year),
        ("in 2024", "2024-01-01", "2025-01-01", Year),
        ("2024-11-05", "2024-11-05", "2024-11-06", Day),
        ("2024/11/05", "2024-11-05", "2024-11-06", Day),
        ("2024-11", "2024-11-01", "2024-12-01", Month),
        ("11/5/2024", "2024-11-05", "2024-11-06", Day),
        ("11/5/24", "2024-11-05", "2024-11-06", Day),
        ("1/2/99", "1999-01-02", "1999-01-03", Day),
        ("11/5", "2025-11-05", "2025-11-06", Day),
        ("10/1", "2026-10-01", "2026-10-02", Day),
    ]);
}

#[test]
fn ranges() {
    defaults(&[
        // Past resolves "fri" to last week; the range rule re-resolves it
        // forward from Monday.
        ("from monday to friday", "2026-10-19", "2026-10-24", Custom),
        ("mon to fri", "2026-10-19", "2026-10-24", Custom),
        ("mon-fri", "2026-10-19", "2026-10-24", Custom),
        (
            "between monday and friday",
            "2026-10-19",
            "2026-10-24",
            Custom,
        ),
        ("jan through mar", "2026-01-01", "2026-04-01", Custom),
        ("jan-mar 2024", "2024-01-01", "2024-04-01", Custom),
        ("nov to feb", "2025-11-01", "2026-03-01", Custom),
        (
            "from yesterday to today",
            "2026-10-21",
            "2026-10-23",
            Custom,
        ),
        (
            "last monday until today",
            "2026-10-19",
            "2026-10-23",
            Custom,
        ),
        (
            "2024-01-01 - 2024-03-31",
            "2024-01-01",
            "2024-04-01",
            Custom,
        ),
        (
            "2024-01-01 – 2024-03-31",
            "2024-01-01",
            "2024-04-01",
            Custom,
        ),
        ("nov 5 to nov 10 2024", "2024-11-05", "2024-11-11", Custom),
        ("9am to 5pm", "2026-10-22 09:00", "2026-10-22 17:00", Custom),
        (
            "between 9am and 5pm",
            "2026-10-22 09:00",
            "2026-10-22 17:00",
            Custom,
        ),
        ("2023-2024", "2023-01-01", "2025-01-01", Custom),
        ("q1 thru q2 2025", "2025-01-01", "2025-07-01", Custom),
        ("since monday", "2026-10-19", "2026-10-23", Custom),
        ("since 2024", "2024-01-01", "2026-10-23", Custom),
    ]);
    // Future already orders "nov" before "feb".
    check(
        &with(|o| o.prefer = DatePreference::Future),
        &[("nov to feb", "2026-11-01", "2027-03-01", Custom)],
    );
}

#[test]
fn holidays_by_preference() {
    defaults(&[
        ("christmas", "2025-12-25", "2025-12-26", Day),
        ("Christmas Day", "2025-12-25", "2025-12-26", Day),
        ("christmas 2024", "2024-12-25", "2024-12-26", Day),
        ("last christmas", "2025-12-25", "2025-12-26", Day),
        ("next christmas", "2026-12-25", "2026-12-26", Day),
        ("christmas last year", "2025-12-25", "2025-12-26", Day),
        ("xmas eve", "2025-12-24", "2025-12-25", Day),
        ("new year's day", "2026-01-01", "2026-01-02", Day),
        ("new years", "2026-01-01", "2026-01-02", Day),
        ("new year\u{2019}s eve", "2025-12-31", "2026-01-01", Day),
        ("thanksgiving", "2025-11-27", "2025-11-28", Day),
        ("independence day", "2026-07-04", "2026-07-05", Day),
        ("july 4th", "2026-07-04", "2026-07-05", Day),
        ("4th of july", "2026-07-04", "2026-07-05", Day),
        ("fourth of july", "2026-07-04", "2026-07-05", Day),
        ("halloween", "2025-10-31", "2025-11-01", Day),
        ("valentine's day", "2026-02-14", "2026-02-15", Day),
        ("valentines", "2026-02-14", "2026-02-15", Day),
        ("memorial day", "2026-05-25", "2026-05-26", Day),
        ("labor day", "2026-09-07", "2026-09-08", Day),
        ("christmas at 9am", "2025-12-25 09:00", "", Instant),
    ]);
    check(
        &with(|o| o.prefer = DatePreference::Future),
        &[
            ("christmas", "2026-12-25", "2026-12-26", Day),
            ("thanksgiving", "2026-11-26", "2026-11-27", Day),
            ("halloween", "2026-10-31", "2026-11-01", Day),
            ("labor day", "2027-09-06", "2027-09-07", Day),
        ],
    );
    check(
        &with(|o| o.prefer = DatePreference::Current),
        &[
            ("christmas", "2026-12-25", "2026-12-26", Day),
            ("valentines day", "2026-02-14", "2026-02-15", Day),
        ],
    );
}

#[test]
fn junk_is_rejected_without_panicking() {
    let junk = [
        "customer",
        "abc",
        "nov 5 abc",
        "today please",
        "last",
        "next",
        "at",
        "3",
        "15",
        "in",
        "from",
        "to",
        "-",
        "on",
        "since",
        "the",
        "q5",
        "q0",
        "13/45/2024",
        "2024-13-01",
        "2024-02-30",
        "feb 30",
        "nov 32",
        "\u{1f600}",
        "\u{65e5}\u{672c}",
        "caf\u{e9}",
        "99999999 days ago",
        "in 99999999999999999999 years",
        "last 999999999999 months",
        "next 18446744073709551615 days",
        "9223372036854775807 hours ago",
        "99999999999999999999999999",
        "at 25",
        "at 3:75",
        "25:00",
        "13pm",
        "0am",
        "fy99999",
        "the 99999999999th",
        "the 0th",
        "4294967296th",
        "nov 5 to",
        "between and",
        "from to",
        "today today",
        "yesterday tomorrow",
        "christmas christmas",
        "@@@",
        "1/1/1/1",
        "q3 q4",
        "- - -",
        "3'",
        "'2024",
        "first monday of 3 days ago",
        "end of now",
        "start of 3pm",
        "this week at 3",
        "last 0 days",
        "since tomorrow",
        "november 2024 at 3",
        "a",
        "few",
        "mon 5",
        "may be",
    ];
    for s in junk {
        let r = parse_natural_date(s, now(), &DateParseOptions::default());
        assert!(
            matches!(r, Err(DateParseError::NotUnderstood(_))),
            "{s:?} should not parse, got {r:?}"
        );
    }
    let long = "a".repeat(10_000);
    assert!(parse_natural_date(&long, now(), &DateParseOptions::default()).is_err());
    let many = "last ".repeat(200) + "week";
    assert!(parse_natural_date(&many, now(), &DateParseOptions::default()).is_err());
    // Extreme `now` values must not panic either.
    for n in [NaiveDateTime::MIN, NaiveDateTime::MAX] {
        for s in [
            "tomorrow",
            "next year",
            "last 7 days",
            "ytd",
            "christmas",
            "in 3 hours",
        ] {
            let _ = parse_natural_date(s, n, &DateParseOptions::default());
        }
    }
}

#[test]
fn errors_carry_trimmed_input() {
    let o = DateParseOptions::default();
    assert_eq!(
        parse_natural_date("", now(), &o),
        Err(DateParseError::Empty)
    );
    assert_eq!(
        parse_natural_date("  \t ", now(), &o),
        Err(DateParseError::Empty)
    );
    assert_eq!(
        parse_natural_date("\"\"", now(), &o),
        Err(DateParseError::Empty)
    );
    assert_eq!(
        parse_natural_date("  customer  ", now(), &o),
        Err(DateParseError::NotUnderstood("customer".into()))
    );
}

#[test]
fn parsed_date_flags() {
    let o = DateParseOptions::default();
    let p = |s| parse_natural_date(s, now(), &o).unwrap();
    assert!(!p("now").is_range());
    assert!(!p("now").date_only());
    assert!(p("today").is_range() && p("today").date_only());
    assert!(p("november").date_only());
    assert!(p("tonight").is_range() && !p("tonight").date_only());
    assert!(!p("past 3 hours").date_only());
}

#[test]
fn describe_previews() {
    let o = DateParseOptions::default();
    let p = |s| parse_natural_date(s, now(), &o).unwrap();
    assert_eq!(describe(&p("last wed at 3")), "Wed, Oct 21 2026 3:00 PM");
    assert_eq!(describe(&p("november")), "Nov 1 – Nov 30 2025");
    assert_eq!(describe(&p("today")), "Thu, Oct 22 2026");
    assert_eq!(describe(&p("nov to feb")), "Nov 1 2025 – Feb 28 2026");
    assert_eq!(
        describe(&p("tomorrow morning")),
        "Fri, Oct 23 2026 6:00 AM – 12:00 PM"
    );
    assert_eq!(
        describe(&p("tonight")),
        "Thu, Oct 22 2026 6:00 PM – 12:00 AM"
    );
    assert_eq!(describe(&p("eod")), "Thu, Oct 22 2026 11:59:59 PM");
    assert_eq!(describe(&p("mtd")), "Oct 1 – Oct 22 2026");
    assert_eq!(describe_relative(&p("mtd"), now()), "Oct 1 2026 – today");
    assert_eq!(describe_relative(&p("today"), now()), "Thu, Oct 22 2026");
    assert_eq!(
        describe_relative(&p("past 3 hours"), now()),
        "Oct 22 2026 7:30 AM – now"
    );
}

#[test]
fn example_phrases_all_parse() {
    let examples = example_phrases();
    assert!((8..=14).contains(&examples.len()));
    for s in examples {
        assert!(
            parse_natural_date(s, now(), &DateParseOptions::default()).is_ok(),
            "{s:?}"
        );
    }
}
