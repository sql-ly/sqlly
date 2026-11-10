//! SugarJS-style natural-language date parsing: "last wed at 3",
//! "november", "ytd", "between mon and fri".
//!
//! Pure and deterministic: every phrase resolves against a caller-supplied
//! local `now`, so the editor and the tests agree on what "today" means.
//! Results are half-open spans (`[start, end)`) so the SQL side can emit
//! `col >= start AND col < end` without off-by-one-day bugs.

use chrono::{Datelike, NaiveDateTime, NaiveTime, Weekday};

mod calendar;
mod grammar;
mod lexer;

#[cfg(test)]
mod tests;

/// How to resolve an ambiguous reference that doesn't say last/next
/// ("wed", "november", "nov 5", "Q2", "christmas").
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum DatePreference {
    /// The most recent one that has started (today counts) — what a
    /// `WHERE` filter over historical rows almost always means.
    #[default]
    Past,
    /// The next one that hasn't ended (today counts).
    Future,
    /// The one in the current week / year (SugarJS's default).
    Current,
}

/// How to read an hour written without am/pm ("at 3", "3:30").
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum BareHour {
    /// 1–7 mean afternoon (13–19); 8–12 as written (12 = noon); 0 and
    /// 13–23 as written.
    #[default]
    BusinessHours,
    /// Always as written ("3" = 03:00).
    TwentyFourHour,
}

/// Where "to date" periods and rolling windows ("ytd", "past 7 days") end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub enum ToDateEnd {
    /// Exclusive end at tomorrow 00:00, so rows stamped later today match.
    #[default]
    EndOfToday,
    /// Exactly `now`.
    Now,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateParseOptions {
    pub week_start: Weekday,
    pub prefer: DatePreference,
    pub bare_hour: BareHour,
    pub to_date_end: ToDateEnd,
    /// 1..=12; values outside are clamped. A fiscal year that doesn't start
    /// in January is named by the calendar year it ends in.
    pub fiscal_year_start_month: u32,
}

impl Default for DateParseOptions {
    fn default() -> Self {
        Self {
            week_start: Weekday::Sun,
            prefer: DatePreference::Past,
            bare_hour: BareHour::BusinessHours,
            to_date_end: ToDateEnd::EndOfToday,
            fiscal_year_start_month: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Granularity {
    /// A point in time: "now", "last wed at 3".
    Instant,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Quarter,
    Year,
    /// A span that isn't one calendar unit: "past 7 days", "ytd",
    /// "this weekend", "mon to fri", "tomorrow morning".
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDate {
    pub start: NaiveDateTime,
    /// Exclusive end. Equals `start` for an Instant.
    pub end: NaiveDateTime,
    pub granularity: Granularity,
}

impl ParsedDate {
    pub fn is_range(&self) -> bool {
        self.granularity != Granularity::Instant
    }

    /// A range of whole days (both ends at midnight), which SQL can express
    /// with date literals alone.
    pub fn date_only(&self) -> bool {
        self.is_range() && self.start.time() == NaiveTime::MIN && self.end.time() == NaiveTime::MIN
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DateParseError {
    Empty,
    /// Carries the trimmed input.
    NotUnderstood(String),
}

impl std::fmt::Display for DateParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DateParseError::Empty => f.write_str("no date given"),
            DateParseError::NotUnderstood(s) => write!(f, "not a date: {s}"),
        }
    }
}

impl std::error::Error for DateParseError {}

/// Inputs longer than this are not dates; refusing early keeps the
/// split-point search in the grammar trivially cheap.
const MAX_INPUT_CHARS: usize = 200;

/// Parse `input` relative to `now` (local wall-clock time; no time zones).
pub fn parse_natural_date(
    input: &str,
    now: NaiveDateTime,
    opts: &DateParseOptions,
) -> Result<ParsedDate, DateParseError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(DateParseError::Empty);
    }
    let not_understood = || DateParseError::NotUnderstood(trimmed.to_string());
    if trimmed.chars().count() > MAX_INPUT_CHARS {
        return Err(not_understood());
    }
    let normalized = lexer::normalize(trimmed);
    if normalized.is_empty() {
        // Only quotes/punctuation, e.g. `""` typed before the phrase.
        return Err(DateParseError::Empty);
    }
    let lexemes = lexer::lex(&normalized).ok_or_else(not_understood)?;
    let tokens: Vec<lexer::T> = lexemes.iter().map(lexer::Lexeme::view).collect();
    let ctx = grammar::Ctx {
        now,
        prefer: opts.prefer,
        opts,
    };
    ctx.parse(&tokens).ok_or_else(not_understood)
}

fn fmt_time(t: NaiveDateTime) -> String {
    if t.format("%S").to_string() == "00" {
        t.format("%-I:%M %p").to_string()
    } else {
        t.format("%-I:%M:%S %p").to_string()
    }
}

/// The last day a date-only span covers.
fn last_covered_day(p: &ParsedDate) -> NaiveDateTime {
    p.end
        .checked_sub_signed(chrono::TimeDelta::days(1))
        .unwrap_or(p.start)
}

pub fn describe_message(parsed: &ParsedDate) -> sqlly_core::DisplayMessage {
    let (start, end) = (parsed.start, parsed.end);
    let (key, end_value) = if !parsed.is_range() {
        ("services.natural_date.single", fmt_time(end))
    } else if parsed.date_only() {
        let last = last_covered_day(parsed);
        if last.date() == start.date() {
            ("services.natural_date.single_date", String::new())
        } else if last.year() == start.year() {
            (
                "services.natural_date.range_same_year",
                last.format("%Y-%m-%d").to_string(),
            )
        } else {
            (
                "services.natural_date.range_different_years",
                last.format("%Y-%m-%d").to_string(),
            )
        }
    } else {
        let inclusive = end
            .checked_sub_signed(chrono::TimeDelta::seconds(1))
            .unwrap_or(start);
        if inclusive.date() == start.date() {
            ("services.natural_date.datetime_same_day", fmt_time(end))
        } else {
            (
                "services.natural_date.datetime_range",
                format!("{} {}", end.format("%Y-%m-%d"), fmt_time(end)),
            )
        }
    };
    // Each template names only the parts it shows; an unused argument would
    // make the localized format fail and fall back to English.
    let message = sqlly_core::DisplayMessage::new(key, describe(parsed))
        .with_arg("start_date", start.format("%Y-%m-%d").to_string());
    match key {
        "services.natural_date.single_date" => message,
        "services.natural_date.single" => message.with_arg("start_time", fmt_time(start)),
        "services.natural_date.range_same_year" | "services.natural_date.range_different_years" => {
            message.with_arg("end_date_or_time", end_value)
        }
        _ => message
            .with_arg("start_time", fmt_time(start))
            .with_arg("end_date_or_time", end_value),
    }
}

pub fn describe_relative_message(
    parsed: &ParsedDate,
    now: NaiveDateTime,
) -> sqlly_core::DisplayMessage {
    let Some(tomorrow) = now
        .date()
        .succ_opt()
        .map(|date| date.and_time(NaiveTime::MIN))
    else {
        return describe_message(parsed);
    };
    let start_of_today = now.date().and_time(NaiveTime::MIN);
    if parsed.is_range() && parsed.end == tomorrow && parsed.start < start_of_today {
        return sqlly_core::DisplayMessage::new(
            "services.natural_date.relative_today",
            describe_relative(parsed, now),
        )
        .with_arg("start_date", parsed.start.format("%Y-%m-%d").to_string())
        .with_arg("start_time", fmt_time(parsed.start));
    }
    if parsed.is_range() && parsed.end == now && parsed.start < now {
        return sqlly_core::DisplayMessage::new(
            "services.natural_date.relative_now",
            describe_relative(parsed, now),
        )
        .with_arg("start_date", parsed.start.format("%Y-%m-%d").to_string())
        .with_arg("start_time", fmt_time(parsed.start));
    }
    describe_message(parsed)
}

/// A short human description of a result for UI previews, e.g.
/// "Wed, Oct 21 2026 3:00 PM", "Nov 1 – Nov 30 2025".
pub fn describe(parsed: &ParsedDate) -> String {
    let (s, e) = (parsed.start, parsed.end);
    if !parsed.is_range() {
        return format!("{} {}", s.format("%a, %b %-d %Y"), fmt_time(s));
    }
    if parsed.date_only() {
        let last = last_covered_day(parsed);
        if last.date() == s.date() {
            return s.format("%a, %b %-d %Y").to_string();
        }
        return if s.format("%Y").to_string() == last.format("%Y").to_string() {
            format!("{} – {}", s.format("%b %-d"), last.format("%b %-d %Y"))
        } else {
            format!("{} – {}", s.format("%b %-d %Y"), last.format("%b %-d %Y"))
        };
    }
    let e_inclusive = e
        .checked_sub_signed(chrono::TimeDelta::seconds(1))
        .unwrap_or(s);
    if e_inclusive.date() == s.date() {
        format!(
            "{} {} – {}",
            s.format("%a, %b %-d %Y"),
            fmt_time(s),
            fmt_time(e)
        )
    } else {
        format!(
            "{} {} – {} {}",
            s.format("%b %-d %Y"),
            fmt_time(s),
            e.format("%b %-d %Y"),
            fmt_time(e)
        )
    }
}

/// Like [`describe`], but a span that runs up to the present reads
/// "… – today" / "… – now", e.g. "Oct 1 2026 – today" for "mtd".
pub fn describe_relative(parsed: &ParsedDate, now: NaiveDateTime) -> String {
    let Some(tomorrow) = now.date().succ_opt().map(|d| d.and_time(NaiveTime::MIN)) else {
        return describe(parsed);
    };
    let start_of_today = now.date().and_time(NaiveTime::MIN);
    let start = || {
        if parsed.start.time() == NaiveTime::MIN {
            parsed.start.format("%b %-d %Y").to_string()
        } else {
            format!(
                "{} {}",
                parsed.start.format("%b %-d %Y"),
                fmt_time(parsed.start)
            )
        }
    };
    if parsed.is_range() && parsed.end == tomorrow && parsed.start < start_of_today {
        return format!("{} – today", start());
    }
    if parsed.is_range() && parsed.end == now && parsed.start < now {
        return format!("{} – now", start());
    }
    describe(parsed)
}

/// Example phrases for a help card.
pub fn example_phrases() -> &'static [&'static str] {
    &[
        "today",
        "yesterday at noon",
        "last wed at 3",
        "november",
        "nov 5 2024",
        "q3 2024",
        "ytd",
        "past 7 days",
        "3 days ago",
        "this week",
        "end of last month",
        "mon to fri",
        "tomorrow morning",
        "christmas",
    ]
}
