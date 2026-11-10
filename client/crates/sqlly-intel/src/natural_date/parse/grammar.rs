//! The phrase grammar. Every rule matches a *whole* token slice (no prefix
//! matching), so "customer" or "nov 5 abc" never half-parse into something.
//! Composite forms ("<day> at <time>", "<x> to <y>") try each split point
//! and ask the simpler rules about both halves; inputs are capped at
//! [`MAX_TOKENS`] so that stays cheap.

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Weekday};

use super::calendar::{
    HOLIDAYS, Holiday, Period, Unit, add_days, add_months, expand_short_year, fiscal_year_start,
    last_weekday_before, midnight, nth_weekday_from, number_word, ordinal_word, parse_month,
    parse_weekday, period_start, shift_date, shift_dt, ymd,
};
use super::lexer::T;
use super::{BareHour, DateParseOptions, DatePreference, Granularity, ParsedDate, ToDateEnd};

/// Longest phrase we bother with; anything longer is not a date.
pub(super) const MAX_TOKENS: usize = 24;

/// Half-open `[start, end)`.
type Span = (NaiveDateTime, NaiveDateTime);

/// An explicit "this/last/next" in front of a recurring thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Qual {
    This,
    Last,
    Next,
}

impl Qual {
    fn parse(word: &str) -> Option<Qual> {
        Some(match word {
            "this" | "current" => Qual::This,
            "last" | "previous" | "prev" | "past" => Qual::Last,
            "next" | "coming" | "upcoming" => Qual::Next,
            _ => return None,
        })
    }

    /// Period offset for "this/last/next <period>".
    fn offset(self) -> i64 {
        match self {
            Qual::This => 0,
            Qual::Last => -1,
            Qual::Next => 1,
        }
    }
}

/// The time-of-day half of "<day> at <time>".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimePart {
    Clock(NaiveTime),
    /// Minutes after midnight, `[start, end)`; `end` may be 24:00.
    Window(u32, u32),
}

fn window(word: &str) -> Option<TimePart> {
    let (s, e) = match word {
        "morning" => (6, 12),
        "afternoon" => (12, 17),
        "evening" => (17, 21),
        "night" => (18, 24),
        _ => return None,
    };
    Some(TimePart::Window(s * 60, e * 60))
}

/// Something that happens once a year; resolved against `prefer` or an
/// explicit qualifier/year.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Yearly {
    Month(u32),
    MonthDay(u32, u32),
    Quarter(u32),
    Holiday(Holiday),
}

impl Yearly {
    fn span(self, year: i32) -> Option<Span> {
        let (start, end) = match self {
            Yearly::Month(m) => {
                let s = ymd(year, m, 1)?;
                (s, add_months(s, 1)?)
            }
            Yearly::Quarter(q) => {
                let s = ymd(year, (q - 1) * 3 + 1, 1)?;
                (s, add_months(s, 3)?)
            }
            Yearly::MonthDay(m, d) => {
                let s = ymd(year, m, d)?;
                (s, add_days(s, 1)?)
            }
            Yearly::Holiday(h) => {
                let s = h.date(year)?;
                (s, add_days(s, 1)?)
            }
        };
        Some((midnight(start), midnight(end)))
    }

    fn granularity(self) -> Granularity {
        match self {
            Yearly::Month(_) => Granularity::Month,
            Yearly::Quarter(_) => Granularity::Quarter,
            Yearly::MonthDay(..) | Yearly::Holiday(_) => Granularity::Day,
        }
    }
}

fn is_year_tok(t: &T) -> bool {
    matches!(t, T::N(_, 4) | T::Yr2(_))
}

fn year_of(t: &T) -> Option<i32> {
    match *t {
        T::N(y, 4) => i32::try_from(y).ok(),
        T::Yr2(y) => Some(expand_short_year(y)),
        _ => None,
    }
}

/// A day-of-month token: `5`, `05`, `5th`.
fn day_of(t: &T) -> Option<u32> {
    let d = match *t {
        T::N(d, 1 | 2) => u32::try_from(d).ok()?,
        T::Ord(d) => d,
        _ => return None,
    };
    (1..=31).contains(&d).then_some(d)
}

fn small(n: u64, digits: usize, max: u32) -> Option<u32> {
    let v = u32::try_from(n).ok()?;
    (matches!(digits, 1 | 2) && (1..=max).contains(&v)).then_some(v)
}

fn ordinal_of(t: &T) -> Option<u32> {
    match *t {
        T::Ord(n) => Some(n),
        T::W(w) => ordinal_word(w),
        _ => None,
    }
}

/// A count: `3`, `three`, `a`/`an`, `a few` (3), `a couple of` (2).
fn count<'s, 'a>(t: &'s [T<'a>]) -> Option<(u64, &'s [T<'a>])> {
    match t {
        [T::N(n, _), rest @ ..] => Some((*n, rest)),
        [T::W("a"), T::W("few"), rest @ ..] | [T::W("few"), rest @ ..] => Some((3, rest)),
        [T::W("a"), T::W("couple"), T::W("of"), rest @ ..]
        | [T::W("a"), T::W("couple"), rest @ ..]
        | [T::W("couple"), T::W("of"), rest @ ..]
        | [T::W("couple"), rest @ ..] => Some((2, rest)),
        [T::W("a" | "an"), rest @ ..] => Some((1, rest)),
        [T::W(w), rest @ ..] => number_word(w).map(|n| (n, rest)),
        _ => None,
    }
}

/// `count` followed by exactly one unit word.
fn count_unit(t: &[T]) -> Option<(i64, Unit)> {
    match count(t)? {
        (n, [T::W(u)]) => Some((i64::try_from(n).ok()?, Unit::parse(u)?)),
        _ => None,
    }
}

fn day_span(d: NaiveDate) -> Option<Span> {
    Some((midnight(d), midnight(add_days(d, 1)?)))
}

fn mk((start, end): Span, granularity: Granularity) -> ParsedDate {
    ParsedDate {
        start,
        end,
        granularity,
    }
}

fn day(d: NaiveDate) -> Option<ParsedDate> {
    Some(mk(day_span(d)?, Granularity::Day))
}

fn instant(at: NaiveDateTime) -> ParsedDate {
    mk((at, at), Granularity::Instant)
}

/// The last calendar day a span touches.
fn last_day(p: &ParsedDate) -> Option<NaiveDate> {
    if p.end.time() == NaiveTime::MIN {
        add_days(p.end.date(), -1)
    } else {
        Some(p.end.date())
    }
}

/// Resolution context. `now`/`prefer` are separate from `opts` because the
/// range rule re-resolves its right-hand side from the left's start.
pub(super) struct Ctx<'o> {
    pub(super) now: NaiveDateTime,
    pub(super) prefer: DatePreference,
    pub(super) opts: &'o DateParseOptions,
}

impl Ctx<'_> {
    fn today(&self) -> NaiveDate {
        self.now.date()
    }

    fn to_date_end(&self) -> Option<NaiveDateTime> {
        match self.opts.to_date_end {
            ToDateEnd::EndOfToday => Some(midnight(add_days(self.today(), 1)?)),
            ToDateEnd::Now => Some(self.now),
        }
    }

    /// Entry point: a whole phrase.
    pub(super) fn parse(&self, t: &[T]) -> Option<ParsedDate> {
        if t.is_empty() || t.len() > MAX_TOKENS {
            return None;
        }
        if let [T::W("on"), rest @ ..] = t
            && let Some(p) = self.parse(rest)
        {
            return Some(p);
        }
        if let [T::W("since"), rest @ ..] = t {
            let from = self.single(rest)?;
            let end = self.to_date_end()?;
            return (from.start < end).then(|| mk((from.start, end), Granularity::Custom));
        }
        self.single(t).or_else(|| self.range(t))
    }

    /// Anything except a two-sided range.
    fn single(&self, t: &[T]) -> Option<ParsedDate> {
        if t.is_empty() {
            return None;
        }
        if let Some(p) = self.standalone(t).or_else(|| self.date_expr(t)) {
            return Some(p);
        }
        if let Some(tp) = self.time_part(t) {
            return self.at_time(self.today(), tp);
        }
        for i in 1..t.len() {
            let (a, b) = t.split_at(i);
            if let Some(tp) = self.time_part(b)
                && let Some(d) = self.day_for_time(a)
            {
                return self.at_time(d, tp);
            }
            if let Some(tp) = self.time_part(a) {
                let b = match b {
                    [T::W("on"), rest @ ..] => rest,
                    _ => b,
                };
                if let Some(d) = self.day_for_time(b) {
                    return self.at_time(d, tp);
                }
            }
        }
        if let [T::W("in"), rest @ ..] = t {
            return self.single(rest);
        }
        None
    }

    /// The day half of "<day> at <time>": any whole-day expression,
    /// including offsets like "3 days ago".
    fn day_for_time(&self, t: &[T]) -> Option<NaiveDate> {
        self.date_expr(t)
            .or_else(|| self.standalone(t))
            .filter(|d| d.granularity == Granularity::Day)
            .map(|d| d.start.date())
    }

    fn at_time(&self, d: NaiveDate, tp: TimePart) -> Option<ParsedDate> {
        let base = midnight(d);
        match tp {
            TimePart::Clock(t) => Some(instant(d.and_time(t))),
            TimePart::Window(s, e) => {
                let start = base.checked_add_signed(TimeDelta::minutes(i64::from(s)))?;
                let end = base.checked_add_signed(TimeDelta::minutes(i64::from(e)))?;
                Some(mk((start, end), Granularity::Custom))
            }
        }
    }

    // ---- ranges ---------------------------------------------------------

    fn range(&self, t: &[T]) -> Option<ParsedDate> {
        const TO: &[&str] = &["to", "through", "thru", "until", "till", "til"];
        const AND: &[&str] = &["and", "to", "through", "thru", "until", "till", "til"];
        let (body, connectors) = match t {
            [T::W("from"), rest @ ..] => (rest, TO),
            [T::W("between"), rest @ ..] => (rest, AND),
            _ => (t, TO),
        };
        for (i, tok) in body.iter().enumerate() {
            let is_connector = match tok {
                T::Dash => true,
                T::W(w) => connectors.contains(w),
                _ => false,
            };
            if !is_connector || i == 0 || i + 1 >= body.len() {
                continue;
            }
            if let Some(p) = self.build_range(&body[..i], &body[i + 1..]) {
                return Some(p);
            }
        }
        None
    }

    fn build_range(&self, l: &[T], r: &[T]) -> Option<ParsedDate> {
        let right = self.single(r)?;
        // "jan - mar 2024": the left side borrows the right side's year.
        let borrowed = match r.last() {
            Some(y) if r.len() > 1 && is_year_tok(y) && !l.iter().any(is_year_tok) => {
                let mut with_year = l.to_vec();
                with_year.push(*y);
                self.single(&with_year)
            }
            _ => None,
        };
        let left = borrowed.or_else(|| self.single(l))?;
        // "mon to fri" with prefer=Past resolves fri to *last* Friday; the
        // reader meant the Friday after that Monday.
        let right = if right.end <= left.start {
            let from_left = Ctx {
                now: left.start,
                prefer: DatePreference::Future,
                opts: self.opts,
            };
            from_left
                .single(r)
                .filter(|r2| r2.end > left.start)
                .unwrap_or(right)
        } else {
            right
        };
        let start = left.start.min(right.start);
        let end = left.end.max(right.end);
        Some(mk((start, end), Granularity::Custom))
    }

    // ---- times ----------------------------------------------------------

    fn time_part(&self, t: &[T]) -> Option<TimePart> {
        match t {
            [T::W("at"), rest @ ..] => self.clock(rest, true).map(TimePart::Clock),
            // ISO 8601 "2024-11-05T15:30".
            [T::W("t"), rest @ ..] if rest.contains(&T::Colon) => {
                self.clock(rest, true).map(TimePart::Clock)
            }
            [T::W("in"), T::W(w)] | [T::W(w)] if window(w).is_some() => window(w),
            _ => self.clock(t, false).map(TimePart::Clock),
        }
    }

    /// A clock reading. Without "at", a bare number needs a colon or am/pm
    /// to count as a time ("3" alone is not one).
    fn clock(&self, t: &[T], after_at: bool) -> Option<NaiveTime> {
        match t {
            [T::W("noon" | "midday")] => return NaiveTime::from_hms_opt(12, 0, 0),
            [T::W("midnight")] => return Some(NaiveTime::MIN),
            _ => {}
        }
        let (core, pm, oclock) = match t {
            [core @ .., T::W(w)] => match *w {
                "am" | "a" => (core, Some(false), false),
                "pm" | "p" => (core, Some(true), false),
                "oclock" => (core, None, true),
                _ => return None,
            },
            _ => (t, None, false),
        };
        let (h, digits, m, s) = match core {
            [T::N(h, d)] => (*h, *d, 0, 0),
            [T::N(h, d), T::Colon, T::N(m, 2)] => (*h, *d, *m, 0),
            [T::N(h, d), T::Colon, T::N(m, 2), T::Colon, T::N(s, 2)]
            | [
                T::N(h, d),
                T::Colon,
                T::N(m, 2),
                T::Colon,
                T::N(s, 2),
                T::Dot,
                T::N(..),
            ] => (*h, *d, *m, *s),
            _ => return None,
        };
        if digits > 2 || m > 59 || s > 59 {
            return None;
        }
        if !(after_at || pm.is_some() || oclock || core.len() > 1) {
            return None;
        }
        let h = u32::try_from(h).ok()?;
        let hour = match pm {
            Some(pm) => {
                if !(1..=12).contains(&h) {
                    return None;
                }
                h % 12 + if pm { 12 } else { 0 }
            }
            // "03:00" is deliberately 24-hour; only a bare "3" is ambiguous.
            None if digits == 2 && h < 10 => h,
            None => match self.opts.bare_hour {
                BareHour::BusinessHours if (1..=7).contains(&h) => h + 12,
                _ => h,
            },
        };
        NaiveTime::from_hms_opt(hour, m as u32, s as u32)
    }

    // ---- standalone forms (never combined with a time) --------------------

    fn standalone(&self, t: &[T]) -> Option<ParsedDate> {
        let today = self.today();
        match t {
            [T::W("now")] | [T::W("right"), T::W("now")] => return Some(instant(self.now)),
            [T::W("tonight")] => return self.at_time(today, window("night")?),
            [T::W("last"), T::W("night")] => {
                return self.at_time(add_days(today, -1)?, window("night")?);
            }
            [T::W("this"), T::W(w)] if window(w).is_some() => {
                return self.at_time(today, window(w)?);
            }
            [T::W("eod")] => return self.end_of_day(today),
            _ => {}
        }
        self.to_date(t)
            .or_else(|| self.rolling(t))
            .or_else(|| self.offset(t))
    }

    fn end_of_day(&self, d: NaiveDate) -> Option<ParsedDate> {
        Some(instant(d.and_hms_opt(23, 59, 59)?))
    }

    /// ytd / "month to date" / "this year so far".
    fn to_date(&self, t: &[T]) -> Option<ParsedDate> {
        let period = match t {
            [T::W("ytd")] => Period::Year,
            [T::W("qtd")] => Period::Quarter,
            [T::W("mtd")] => Period::Month,
            [T::W("wtd")] => Period::Week,
            [T::W("fytd")]
            | [T::W("fiscal"), T::W("year"), T::W("to"), T::W("date")]
            | [T::W("fy"), T::W("to"), T::W("date")]
            | [
                T::W("this"),
                T::W("fiscal"),
                T::W("year"),
                T::W("so"),
                T::W("far"),
            ] => Period::FiscalYear,
            [T::W("today"), T::W("so"), T::W("far")] => Period::Day,
            [T::W(u), T::W("to"), T::W("date")]
            | [T::W("this"), T::W(u), T::W("so"), T::W("far")]
            | [T::W(u), T::W("so"), T::W("far")]
            | [T::W("so"), T::W("far"), T::W("this"), T::W(u)] => {
                Period::parse(u).filter(|p| *p != Period::Day)?
            }
            _ => return None,
        };
        let o = self.opts;
        let start = period_start(
            period,
            self.today(),
            o.week_start,
            o.fiscal_year_start_month,
        )?;
        let end = self.to_date_end()?;
        Some(mk((midnight(start), end), Granularity::Custom))
    }

    /// "past 7 days", "last 3 months", "next 2 weeks", "past hour".
    fn rolling(&self, t: &[T]) -> Option<ParsedDate> {
        let (forward, implicit_one, rest) = match t {
            [T::W("past" | "trailing"), rest @ ..] => (false, true, rest),
            [T::W("last" | "previous" | "prev"), rest @ ..] => (false, false, rest),
            [T::W("next" | "coming"), rest @ ..] => (true, false, rest),
            _ => return None,
        };
        let (n, unit) = match rest {
            [T::W(u)] if implicit_one => (1, Unit::parse(u)?),
            _ => count_unit(rest)?,
        };
        if n < 1 {
            return None;
        }
        let today = self.today();
        let span = if unit.is_clock() {
            let other = shift_dt(self.now, unit, if forward { n } else { -n })?;
            if forward {
                (self.now, other)
            } else {
                (other, self.now)
            }
        } else if forward {
            let last = shift_date(today, unit, n)?;
            (midnight(today), midnight(add_days(last, 1)?))
        } else {
            let first = match unit {
                Unit::Day => add_days(today, 1 - n)?,
                _ => shift_date(today, unit, -n)?,
            };
            (midnight(first), self.to_date_end()?)
        };
        Some(mk(span, Granularity::Custom))
    }

    /// "3 days ago", "in 2 weeks", "an hour from now", "2 days after christmas".
    fn offset(&self, t: &[T]) -> Option<ParsedDate> {
        if let [T::W("in"), rest @ ..] = t {
            let (n, unit) = count_unit(rest)?;
            return self.offset_from_now(unit, n);
        }
        let (n, rest) = count(t)?;
        let n = i64::try_from(n).ok()?;
        let (unit, rest) = match rest {
            [T::W(u), rest @ ..] => (Unit::parse(u)?, rest),
            _ => return None,
        };
        match rest {
            [T::W("ago")] => self.offset_from_now(unit, n.checked_neg()?),
            [T::W("from" | "after"), T::W("now" | "today")] | [T::W("hence" | "later")] => {
                self.offset_from_now(unit, n)
            }
            [T::W(dir @ ("before" | "after" | "from")), base @ ..] => {
                let base = self.date_expr(base)?;
                let n = if *dir == "before" {
                    n.checked_neg()?
                } else {
                    n
                };
                let start = shift_dt(base.start, unit, n)?;
                if unit.is_clock() || base.granularity == Granularity::Instant {
                    return Some(instant(start));
                }
                if base.granularity == Granularity::Day {
                    return day(start.date());
                }
                Some(mk((start, shift_dt(base.end, unit, n)?), base.granularity))
            }
            _ => None,
        }
    }

    /// `n` units from now: an instant for clock units, the whole day otherwise.
    fn offset_from_now(&self, unit: Unit, n: i64) -> Option<ParsedDate> {
        if unit.is_clock() {
            Some(instant(shift_dt(self.now, unit, n)?))
        } else {
            day(shift_date(self.today(), unit, n)?)
        }
    }

    // ---- date expressions (may be combined with a time) ------------------

    fn date_expr(&self, t: &[T]) -> Option<ParsedDate> {
        if t.is_empty() {
            return None;
        }
        self.keyword_day(t)
            .or_else(|| self.period(t))
            .or_else(|| self.weekday(t))
            .or_else(|| self.yearly(t))
            .or_else(|| self.monthly_ordinal(t))
            .or_else(|| self.year(t))
            .or_else(|| self.numeric(t))
            .or_else(|| self.boundary(t))
            .or_else(|| self.weekday_prefixed(t))
    }

    fn keyword_day(&self, t: &[T]) -> Option<ParsedDate> {
        let off = match t {
            [T::W("today")] => 0,
            [T::W("yesterday")] => -1,
            [T::W("tomorrow" | "tmrw" | "tmr")] => 1,
            [T::W("day"), T::W("before"), T::W("yesterday")] => -2,
            [T::W("day"), T::W("after"), T::W("tomorrow")] => 2,
            _ => return None,
        };
        day(add_days(self.today(), off)?)
    }

    /// "this week", "last month", "next quarter", "previous fiscal year".
    fn period(&self, t: &[T]) -> Option<ParsedDate> {
        let [T::W(q), rest @ ..] = t else {
            return None;
        };
        if *q == "weekend" && rest.is_empty() {
            return self.weekend(None);
        }
        let q = Qual::parse(q)?;
        let p = match rest {
            [T::W("weekend")] => return self.weekend(Some(q)),
            [T::W("fiscal"), T::W("year")] | [T::W("fy")] => Period::FiscalYear,
            [T::W(u)] => Period::parse(u)?,
            _ => return None,
        };
        self.period_span(p, q.offset())
    }

    /// The `k`th period of kind `p` relative to the one containing today.
    fn period_span(&self, p: Period, k: i64) -> Option<ParsedDate> {
        let o = self.opts;
        let here = period_start(p, self.today(), o.week_start, o.fiscal_year_start_month)?;
        let start = shift_date(here, p.unit(), k)?;
        let end = shift_date(start, p.unit(), 1)?;
        let g = match p {
            Period::Day => Granularity::Day,
            Period::Week => Granularity::Week,
            Period::Month => Granularity::Month,
            Period::Quarter => Granularity::Quarter,
            Period::Year | Period::FiscalYear => Granularity::Year,
        };
        Some(mk((midnight(start), midnight(end)), g))
    }

    /// Saturday 00:00 – Monday 00:00. "This weekend" is the upcoming one on
    /// a weekday and the current one on Saturday/Sunday, independent of
    /// `week_start` (a Monday-start week would otherwise split it).
    fn weekend(&self, q: Option<Qual>) -> Option<ParsedDate> {
        let today = self.today();
        let sat = match today.weekday() {
            Weekday::Sun => add_days(today, -1)?,
            wd => add_days(today, i64::from(5 - wd.num_days_from_monday()))?,
        };
        let f = |k: i64| {
            let s = add_days(sat, k.checked_mul(7)?)?;
            Some((midnight(s), midnight(add_days(s, 2)?)))
        };
        let span = match q {
            Some(q) => f(q.offset())?,
            None => self.pick(None, 0, 2, f)?,
        };
        Some(mk(span, Granularity::Custom))
    }

    /// Choose one occurrence of a recurring thing. `f(k)` yields occurrence
    /// `k` (`current` is the one in the current week/year/month).
    ///
    /// * explicit "this", or `Current` preference: occurrence `current`;
    /// * `Past`: the latest that has started (today counts);
    /// * `Future`: the earliest that hasn't ended (today counts);
    /// * "last": the latest wholly before today;
    /// * "next": the earliest starting after today.
    fn pick(
        &self,
        q: Option<Qual>,
        current: i64,
        reach: i64,
        f: impl Fn(i64) -> Option<Span>,
    ) -> Option<Span> {
        let today0 = midnight(self.today());
        let tomorrow0 = midnight(add_days(self.today(), 1)?);
        let back = (current - reach..=current + 1).rev();
        let fwd = current - 1..=current + reach;
        match (q, self.prefer) {
            (Some(Qual::This), _) | (None, DatePreference::Current) => f(current),
            (None, DatePreference::Past) => back.filter_map(f).find(|s| s.0 <= self.now),
            (None, DatePreference::Future) => fwd.filter_map(f).find(|s| s.1 > self.now),
            (Some(Qual::Last), _) => back.filter_map(f).find(|s| s.1 <= today0),
            (Some(Qual::Next), _) => fwd.filter_map(f).find(|s| s.0 >= tomorrow0),
        }
    }

    fn weekday_span(&self, wd: Weekday, k: i64) -> Option<Span> {
        let first = self.opts.week_start;
        let ws = super::calendar::week_start(self.today(), first)?;
        let off = (wd.num_days_from_monday() + 7 - first.num_days_from_monday()) % 7;
        day_span(add_days(ws, k.checked_mul(7)? + i64::from(off))?)
    }

    /// "wed", "last friday", "this monday", "monday last week".
    fn weekday(&self, t: &[T]) -> Option<ParsedDate> {
        let span = match t {
            [T::W(w)] => {
                let wd = parse_weekday(w)?;
                self.pick(None, 0, 2, |k| self.weekday_span(wd, k))?
            }
            [T::W(q), T::W(w)] => {
                let (q, wd) = (Qual::parse(q)?, parse_weekday(w)?);
                self.pick(Some(q), 0, 2, |k| self.weekday_span(wd, k))?
            }
            [T::W(w), T::W(q), T::W("week")]
            | [T::W(w), T::W("of"), T::W(q), T::W("week")]
            | [T::W(q), T::W("week"), T::W(w)] => {
                let (q, wd) = (Qual::parse(q)?, parse_weekday(w)?);
                self.weekday_span(wd, q.offset())?
            }
            _ => return None,
        };
        Some(mk(span, Granularity::Day))
    }

    /// "wednesday, nov 5" — the weekday is decoration on a full date.
    fn weekday_prefixed(&self, t: &[T]) -> Option<ParsedDate> {
        match t {
            [T::W(w), rest @ ..] if parse_weekday(w).is_some() && !rest.is_empty() => self
                .date_expr(rest)
                .filter(|d| d.granularity == Granularity::Day),
            _ => None,
        }
    }

    /// Months, month-days, quarters and holidays, with optional
    /// qualifier ("last november"), year ("q3 2024") or relative year
    /// ("christmas last year").
    fn yearly(&self, t: &[T]) -> Option<ParsedDate> {
        let cy = i64::from(self.today().year());
        if let [T::W(q), rest @ ..] = t
            && let Some(q) = Qual::parse(q)
            && let Some(item) = yearly_item(rest)
        {
            let span = self.pick(Some(q), cy, 8, |k| item.span(i32::try_from(k).ok()?))?;
            return Some(mk(span, item.granularity()));
        }
        if let [head @ .., T::W(q), T::W("year")] = t
            && let Some(q) = Qual::parse(q)
        {
            let head = match head {
                [h @ .., T::W("of")] => h,
                _ => head,
            };
            let item = yearly_item(head)?;
            let y = i32::try_from(cy + q.offset()).ok()?;
            return Some(mk(item.span(y)?, item.granularity()));
        }
        let (core, year) = split_year(t);
        let item = yearly_item(core)?;
        let span = match year {
            Some(y) => item.span(y)?,
            None => self.pick(None, cy, 8, |k| item.span(i32::try_from(k).ok()?))?,
        };
        Some(mk(span, item.granularity()))
    }

    /// "the 5th", "5th of next month".
    fn monthly_ordinal(&self, t: &[T]) -> Option<ParsedDate> {
        let today = self.today();
        let first = ymd(today.year(), today.month(), 1)?;
        let f = |d: u32| {
            move |k: i64| {
                let m = add_months(first, k)?;
                day_span(ymd(m.year(), m.month(), d)?)
            }
        };
        let span = match t {
            [T::Ord(d)] => self.pick(None, 0, 13, f(*d))?,
            [T::Ord(d), T::W("of"), T::W(q), T::W("month")] => f(*d)(Qual::parse(q)?.offset())?,
            _ => return None,
        };
        Some(mk(span, Granularity::Day))
    }

    /// "2024", "year 2024", "fy2025", "fiscal year", "fy 25".
    fn year(&self, t: &[T]) -> Option<ParsedDate> {
        match t {
            [T::N(y, 4)] | [T::W("year"), T::N(y, 4)] => {
                let s = ymd(i32::try_from(*y).ok()?, 1, 1)?;
                Some(mk(
                    (midnight(s), midnight(add_months(s, 12)?)),
                    Granularity::Year,
                ))
            }
            [T::W("fy")] | [T::W("fiscal"), T::W("year")] => {
                self.period_span(Period::FiscalYear, 0)
            }
            [T::W("fy"), y] | [T::W("fiscal"), T::W("year"), y] => {
                let label = match *y {
                    T::N(v, 2) => expand_short_year(u32::try_from(v).ok()?),
                    other => year_of(&other)?,
                };
                let s = fiscal_year_start(label, self.opts.fiscal_year_start_month)?;
                Some(mk(
                    (midnight(s), midnight(add_months(s, 12)?)),
                    Granularity::Year,
                ))
            }
            _ => None,
        }
    }

    /// ISO and US numeric dates.
    fn numeric(&self, t: &[T]) -> Option<ParsedDate> {
        match t {
            [T::N(y, 4), s1, T::N(m, md), s2, T::N(d, dd)]
                if s1 == s2 && matches!(s1, T::Dash | T::Slash | T::Dot) =>
            {
                let y = i32::try_from(*y).ok()?;
                day(ymd(y, small(*m, *md, 12)?, small(*d, *dd, 31)?)?)
            }
            [T::N(y, 4), T::Dash, T::N(m, md)] => {
                let s = ymd(i32::try_from(*y).ok()?, small(*m, *md, 12)?, 1)?;
                Some(mk(
                    (midnight(s), midnight(add_months(s, 1)?)),
                    Granularity::Month,
                ))
            }
            [T::N(m, md), T::Slash, T::N(d, dd), T::Slash, T::N(y, yd)] => {
                let y = match yd {
                    4 => i32::try_from(*y).ok()?,
                    2 => expand_short_year(u32::try_from(*y).ok()?),
                    _ => return None,
                };
                day(ymd(y, small(*m, *md, 12)?, small(*d, *dd, 31)?)?)
            }
            [T::N(m, md), T::Slash, T::N(d, dd)] => {
                let item = Yearly::MonthDay(small(*m, *md, 12)?, small(*d, *dd, 31)?);
                let cy = i64::from(self.today().year());
                let span = self.pick(None, cy, 8, |k| item.span(i32::try_from(k).ok()?))?;
                Some(mk(span, Granularity::Day))
            }
            _ => None,
        }
    }

    /// The target of "start of …": a bare unit means the current one.
    fn of_target(&self, t: &[T]) -> Option<ParsedDate> {
        match t {
            [T::W(u)] if Period::parse(u).is_some() => self.period_span(Period::parse(u)?, 0),
            _ => self.date_expr(t),
        }
    }

    /// "start of this week", "end of november", "last day of february",
    /// "first monday of next month", "end of day".
    fn boundary(&self, t: &[T]) -> Option<ParsedDate> {
        match t {
            [T::W("beginning" | "start"), T::W("of"), rest @ ..] => {
                let x = self.of_target(rest)?;
                (x.granularity != Granularity::Instant).then_some(())?;
                day(x.start.date())
            }
            [T::W("end"), T::W("of"), rest @ ..] => {
                let x = self.of_target(rest)?;
                match x.granularity {
                    Granularity::Instant => None,
                    Granularity::Day => self.end_of_day(x.start.date()),
                    _ => day(last_day(&x)?),
                }
            }
            [first, T::W("day"), T::W("of"), rest @ ..] if ordinal_of(first) == Some(1) => {
                let x = self.of_target(rest)?;
                (x.granularity > Granularity::Day).then_some(())?;
                day(x.start.date())
            }
            [T::W("last"), T::W("day"), T::W("of"), rest @ ..] => {
                let x = self.of_target(rest)?;
                (x.granularity > Granularity::Day).then_some(())?;
                day(last_day(&x)?)
            }
            [nth, T::W(w), T::W("of"), rest @ ..] => {
                let wd = parse_weekday(w)?;
                let x = self.of_target(rest)?;
                (x.granularity > Granularity::Day).then_some(())?;
                let d = match nth {
                    T::W("last") => last_weekday_before(add_days(last_day(&x)?, 1)?, wd)?,
                    _ => nth_weekday_from(x.start.date(), wd, ordinal_of(nth)?)?,
                };
                (midnight(d) >= x.start && midnight(d) < x.end).then_some(())?;
                day(d)
            }
            _ => None,
        }
    }
}

/// Peel a trailing (or leading) year off a yearly phrase: "nov 5 2024",
/// "q3 of 2024", "2024 q3".
fn split_year<'s, 'a>(t: &'s [T<'a>]) -> (&'s [T<'a>], Option<i32>) {
    match t {
        [head @ .., T::W("of"), y] if !head.is_empty() && is_year_tok(y) => (head, year_of(y)),
        [head @ .., y] if !head.is_empty() && is_year_tok(y) => (head, year_of(y)),
        [y, tail @ ..] if !tail.is_empty() && is_year_tok(y) => (tail, year_of(y)),
        _ => (t, None),
    }
}

/// A yearly item with no year attached.
fn yearly_item(t: &[T]) -> Option<Yearly> {
    for (words, h) in HOLIDAYS {
        if t.len() == words.len() && t.iter().zip(words.iter()).all(|(a, b)| *a == T::W(b)) {
            return Some(Yearly::Holiday(*h));
        }
    }
    match t {
        [T::W(m)] => Some(Yearly::Month(parse_month(m)?)),
        [T::W("q"), T::N(q, 1)] if (1..=4).contains(q) => Some(Yearly::Quarter(*q as u32)),
        [o, T::W("quarter")] => {
            let q = ordinal_of(o)?;
            (1..=4).contains(&q).then_some(Yearly::Quarter(q))
        }
        [T::W(m), d] | [d, T::W(m)] | [d, T::W("of"), T::W(m)] => {
            Some(Yearly::MonthDay(parse_month(m)?, day_of(d)?))
        }
        _ => None,
    }
}
