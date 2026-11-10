//! Excel's worksheet functions, by name and argument order.

use chrono::{Datelike, Duration, NaiveDate};

use super::eval::{EvalEnv, compare, eval_arg, number, scalar, to_bool, to_number, to_text};
use super::parser::Expr;
use super::{
    ErrorKind, Value, datetime_from_serial, format_text, parse_datetime_text, serial_from_date, ymd,
};

/// Every function [`call`] knows, for the docs and the formula bar's
/// completions (and held to `call` by a test: each name must not come back
/// `#NAME?`).
pub const FUNCTIONS: &[&str] = &[
    "ABS",
    "AND",
    "AVERAGE",
    "CEILING",
    "CEILING.MATH",
    "CHAR",
    "CHOOSE",
    "CLEAN",
    "COALESCE",
    "CODE",
    "CONCAT",
    "CONCATENATE",
    "COS",
    "COUNT",
    "COUNTA",
    "COUNTBLANK",
    "DATE",
    "DATEDIF",
    "DATEVALUE",
    "DAY",
    "DAYS",
    "DEGREES",
    "EDATE",
    "EOMONTH",
    "EVEN",
    "EXACT",
    "EXP",
    "FALSE",
    "FIND",
    "FIXED",
    "FLOOR",
    "FLOOR.MATH",
    "HOUR",
    "IF",
    "IFERROR",
    "IFNA",
    "IFS",
    "INT",
    "ISBLANK",
    "ISERR",
    "ISERROR",
    "ISEVEN",
    "ISLOGICAL",
    "ISNA",
    "ISNUMBER",
    "ISODD",
    "ISOWEEKNUM",
    "ISTEXT",
    "LEFT",
    "LEN",
    "LN",
    "LOG",
    "LOG10",
    "LOWER",
    "MAX",
    "MEDIAN",
    "MID",
    "MIN",
    "MINUTE",
    "MOD",
    "MONTH",
    "MROUND",
    "N",
    "NA",
    "NETWORKDAYS",
    "NOT",
    "NOW",
    "ODD",
    "OR",
    "PI",
    "POWER",
    "PRODUCT",
    "PROPER",
    "QUOTIENT",
    "RADIANS",
    "REPLACE",
    "REPT",
    "RIGHT",
    "ROUND",
    "ROUNDDOWN",
    "ROUNDUP",
    "SEARCH",
    "SECOND",
    "SIGN",
    "SIN",
    "SQRT",
    "SUBSTITUTE",
    "SUM",
    "SWITCH",
    "T",
    "TAN",
    "TEXT",
    "TEXTAFTER",
    "TEXTBEFORE",
    "TEXTJOIN",
    "TIME",
    "TIMEVALUE",
    "TODAY",
    "TRIM",
    "TRUE",
    "TRUNC",
    "UPPER",
    "VALUE",
    "WEEKDAY",
    "WEEKNUM",
    "WORKDAY",
    "XOR",
    "YEAR",
];

type R = Result<Value, ErrorKind>;

fn v(r: R) -> Value {
    r.unwrap_or_else(Value::Error)
}

pub fn call(name: &str, args: &[Expr], env: &EvalEnv) -> Value {
    v(call_inner(name, args, env))
}

fn arity(args: &[Expr], min: usize, max: usize) -> Result<(), ErrorKind> {
    if args.len() < min || args.len() > max {
        Err(ErrorKind::Value)
    } else {
        Ok(())
    }
}

fn val(args: &[Expr], i: usize, env: &EvalEnv) -> Value {
    args.get(i)
        .map(|a| scalar(eval_arg(a, env)))
        .unwrap_or(Value::Blank)
}

fn num(args: &[Expr], i: usize, env: &EvalEnv) -> Result<f64, ErrorKind> {
    to_number(&val(args, i, env))
}

fn num_or(args: &[Expr], i: usize, default: f64, env: &EvalEnv) -> Result<f64, ErrorKind> {
    match args.get(i) {
        None | Some(Expr::Missing) => Ok(default),
        Some(_) => num(args, i, env),
    }
}

fn text(args: &[Expr], i: usize, env: &EvalEnv) -> Result<String, ErrorKind> {
    to_text(&val(args, i, env))
}

fn boolean(args: &[Expr], i: usize, env: &EvalEnv) -> Result<bool, ErrorKind> {
    to_bool(&val(args, i, env))
}

/// Numbers for the aggregate functions: a range contributes only its
/// numbers (text, booleans and blanks are skipped); a value typed straight
/// into the call is coerced (TRUE counts as 1, "5" as 5).
fn numbers(args: &[Expr], env: &EvalEnv) -> Result<Vec<f64>, ErrorKind> {
    let mut out = Vec::new();
    for a in args {
        match eval_arg(a, env) {
            Value::Array(items) => {
                for item in items {
                    match item {
                        Value::Number(n) => out.push(n),
                        Value::Error(e) => return Err(e),
                        _ => {}
                    }
                }
            }
            Value::Blank if matches!(a, Expr::Ref(_)) => {}
            Value::Text(_) if matches!(a, Expr::Ref(_)) => {}
            Value::Bool(_) if matches!(a, Expr::Ref(_)) => {}
            other => out.push(to_number(&other)?),
        }
    }
    Ok(out)
}

/// Every value of the arguments, ranges flattened.
fn values(args: &[Expr], env: &EvalEnv) -> Vec<Value> {
    let mut out = Vec::new();
    for a in args {
        match eval_arg(a, env) {
            Value::Array(items) => out.extend(items),
            other => out.push(other),
        }
    }
    out
}

/// Excel rounds half away from zero; snapping first keeps binary noise
/// (2.675 stored as 2.67499…) from rounding the wrong way.
fn round_to(x: f64, digits: f64, mode: fn(f64) -> f64) -> f64 {
    let d = digits.trunc() as i32;
    let factor = 10f64.powi(d.abs());
    let scaled = if d >= 0 { x * factor } else { x / factor };
    let snapped = (scaled * 1e9).round() / 1e9;
    let r = mode(snapped);
    if d >= 0 { r / factor } else { r * factor }
}

fn round_half_away(x: f64) -> f64 {
    x.round()
}

fn round_up(x: f64) -> f64 {
    if x >= 0.0 { x.ceil() } else { x.floor() }
}

fn date_of(serial: f64) -> Result<NaiveDate, ErrorKind> {
    datetime_from_serial(serial)
        .map(|dt| dt.date())
        .ok_or(ErrorKind::Num)
}

fn date_arg(args: &[Expr], i: usize, env: &EvalEnv) -> Result<f64, ErrorKind> {
    match val(args, i, env) {
        Value::Text(t) => parse_datetime_text(&t).ok_or(ErrorKind::Value),
        other => to_number(&other),
    }
}

/// DATE's overflow rules: months past 12 roll into later years, day 0 is
/// the last day of the previous month, and years below 1900 add 1900.
fn make_date(year: f64, month: f64, day: f64) -> Result<f64, ErrorKind> {
    let mut y = year.trunc() as i64;
    if (0..1900).contains(&y) {
        y += 1900;
    }
    let m0 = month.trunc() as i64 - 1;
    let y = y + m0.div_euclid(12);
    let m = m0.rem_euclid(12) + 1;
    let first = NaiveDate::from_ymd_opt(y as i32, m as u32, 1).ok_or(ErrorKind::Num)?;
    let date = first
        .checked_add_signed(Duration::days(day.trunc() as i64 - 1))
        .ok_or(ErrorKind::Num)?;
    let s = serial_from_date(date);
    if s < 1.0 { Err(ErrorKind::Num) } else { Ok(s) }
}

fn add_months(serial: f64, months: f64, end_of_month: bool) -> Result<f64, ErrorKind> {
    let d = date_of(serial)?;
    let total = d.year() as i64 * 12 + d.month0() as i64 + months.trunc() as i64;
    let (y, m) = (total.div_euclid(12) as i32, total.rem_euclid(12) as u32 + 1);
    let last = last_day(y, m)?;
    let day = if end_of_month {
        last
    } else {
        d.day().min(last)
    };
    Ok(serial_from_date(
        NaiveDate::from_ymd_opt(y, m, day).ok_or(ErrorKind::Num)?,
    ))
}

fn last_day(y: i32, m: u32) -> Result<u32, ErrorKind> {
    let next = if m == 12 {
        NaiveDate::from_ymd_opt(y + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(y, m + 1, 1)
    }
    .ok_or(ErrorKind::Num)?;
    Ok(next.pred_opt().ok_or(ErrorKind::Num)?.day())
}

fn is_weekend(d: NaiveDate) -> bool {
    matches!(d.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun)
}

/// `?` and `*` wildcards (`~` escapes) as a case-insensitive regex.
fn wildcard_regex(pattern: &str) -> Option<regex::Regex> {
    let mut re = String::from("(?i)");
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '~' => {
                if let Some(n) = chars.next() {
                    re.push_str(&regex::escape(&n.to_string()));
                }
            }
            '?' => re.push('.'),
            '*' => re.push_str(".*?"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    regex::Regex::new(&re).ok()
}

fn char_index_to_byte(s: &str, chars: usize) -> usize {
    s.char_indices()
        .nth(chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

fn call_inner(name: &str, args: &[Expr], env: &EvalEnv) -> R {
    let n = |i| num(args, i, env);
    let t = |i| text(args, i, env);
    Ok(match name {
        // ---- Logical ----
        "IF" => {
            arity(args, 1, 3)?;
            if boolean(args, 0, env)? {
                match args.get(1) {
                    Some(Expr::Missing) | None => Value::Number(0.0),
                    Some(_) => val(args, 1, env),
                }
            } else {
                match args.get(2) {
                    None => Value::Bool(false),
                    Some(Expr::Missing) => Value::Number(0.0),
                    Some(_) => val(args, 2, env),
                }
            }
        }
        "IFS" => {
            if args.is_empty() || !args.len().is_multiple_of(2) {
                return Err(ErrorKind::Value);
            }
            for pair in args.chunks(2) {
                if to_bool(&scalar(eval_arg(&pair[0], env)))? {
                    return Ok(scalar(eval_arg(&pair[1], env)));
                }
            }
            return Err(ErrorKind::NA);
        }
        "IFERROR" => {
            arity(args, 2, 2)?;
            match val(args, 0, env) {
                Value::Error(_) => val(args, 1, env),
                ok => ok,
            }
        }
        "IFNA" => {
            arity(args, 2, 2)?;
            match val(args, 0, env) {
                Value::Error(ErrorKind::NA) => val(args, 1, env),
                ok => ok,
            }
        }
        "AND" | "OR" | "XOR" => {
            let mut seen = false;
            let mut acc = name == "AND";
            for item in values(args, env) {
                let b = match item {
                    Value::Blank | Value::Text(_) => continue,
                    other => to_bool(&other)?,
                };
                seen = true;
                acc = match name {
                    "AND" => acc && b,
                    "OR" => acc || b,
                    _ => acc ^ b,
                };
            }
            if !seen {
                return Err(ErrorKind::Value);
            }
            Value::Bool(acc)
        }
        "NOT" => {
            arity(args, 1, 1)?;
            Value::Bool(!boolean(args, 0, env)?)
        }
        "TRUE" => Value::Bool(true),
        "FALSE" => Value::Bool(false),
        "SWITCH" => {
            if args.len() < 3 {
                return Err(ErrorKind::Value);
            }
            let subject = val(args, 0, env);
            if let Value::Error(e) = subject {
                return Err(e);
            }
            let rest = &args[1..];
            for pair in rest.chunks(2) {
                if pair.len() == 1 {
                    return Ok(scalar(eval_arg(&pair[0], env)));
                }
                let candidate = scalar(eval_arg(&pair[0], env));
                if compare(&subject, &candidate) == std::cmp::Ordering::Equal {
                    return Ok(scalar(eval_arg(&pair[1], env)));
                }
            }
            return Err(ErrorKind::NA);
        }
        "CHOOSE" => {
            if args.len() < 2 {
                return Err(ErrorKind::Value);
            }
            let i = n(0)?.trunc() as usize;
            if i < 1 || i >= args.len() {
                return Err(ErrorKind::Value);
            }
            val(args, i, env)
        }
        "COALESCE" => {
            for i in 0..args.len() {
                match val(args, i, env) {
                    Value::Blank => continue,
                    Value::Text(t) if t.is_empty() => continue,
                    other => return Ok(other),
                }
            }
            Value::Blank
        }
        // ---- Information ----
        "ISBLANK" => Value::Bool(matches!(val(args, 0, env), Value::Blank)),
        "ISNUMBER" => Value::Bool(matches!(val(args, 0, env), Value::Number(_))),
        "ISTEXT" => Value::Bool(matches!(val(args, 0, env), Value::Text(_))),
        "ISLOGICAL" => Value::Bool(matches!(val(args, 0, env), Value::Bool(_))),
        "ISERROR" => Value::Bool(matches!(val(args, 0, env), Value::Error(_))),
        "ISERR" => Value::Bool(matches!(
            val(args, 0, env),
            Value::Error(e) if e != ErrorKind::NA
        )),
        "ISNA" => Value::Bool(matches!(val(args, 0, env), Value::Error(ErrorKind::NA))),
        "ISEVEN" | "ISODD" => {
            let x = n(0)?.trunc() as i64;
            Value::Bool((x % 2 == 0) == (name == "ISEVEN"))
        }
        "N" => match val(args, 0, env) {
            Value::Number(x) => Value::Number(x),
            Value::Bool(b) => Value::Number(if b { 1.0 } else { 0.0 }),
            Value::Error(e) => return Err(e),
            _ => Value::Number(0.0),
        },
        "T" => match val(args, 0, env) {
            Value::Text(s) => Value::Text(s),
            Value::Error(e) => return Err(e),
            _ => Value::Text(String::new()),
        },
        "NA" => return Err(ErrorKind::NA),
        // ---- Math ----
        "ABS" => number(n(0)?.abs()),
        "SIGN" => number(n(0)?.signum() * if n(0)? == 0.0 { 0.0 } else { 1.0 }),
        "INT" => number(n(0)?.floor()),
        "TRUNC" => number(round_to(n(0)?, num_or(args, 1, 0.0, env)?, f64::trunc)),
        "ROUND" => number(round_to(n(0)?, n(1)?, round_half_away)),
        "ROUNDUP" => number(round_to(n(0)?, n(1)?, round_up)),
        "ROUNDDOWN" => number(round_to(n(0)?, n(1)?, f64::trunc)),
        "MROUND" => {
            let (x, m) = (n(0)?, n(1)?);
            if m == 0.0 {
                Value::Number(0.0)
            } else if x.signum() * m.signum() < 0.0 {
                return Err(ErrorKind::Num);
            } else {
                number(round_half_away(x / m) * m)
            }
        }
        "CEILING" | "CEILING.MATH" => {
            let x = n(0)?;
            let sig = num_or(
                args,
                1,
                if x < 0.0 && name == "CEILING" {
                    -1.0
                } else {
                    1.0
                },
                env,
            )?;
            if sig == 0.0 {
                Value::Number(0.0)
            } else {
                number((x / sig).ceil() * sig)
            }
        }
        "FLOOR" | "FLOOR.MATH" => {
            let x = n(0)?;
            let sig = num_or(args, 1, 1.0, env)?;
            if sig == 0.0 {
                return Err(ErrorKind::Div0);
            }
            number((x / sig).floor() * sig)
        }
        "EVEN" | "ODD" => {
            let x = n(0)?;
            let mag = x.abs().ceil();
            let want_even = name == "EVEN";
            let mag = if (mag as i64 % 2 == 0) == want_even {
                mag
            } else {
                mag + 1.0
            };
            let mag = if !want_even && mag == 0.0 { 1.0 } else { mag };
            number(mag * if x < 0.0 { -1.0 } else { 1.0 })
        }
        "MOD" => {
            let (x, d) = (n(0)?, n(1)?);
            if d == 0.0 {
                return Err(ErrorKind::Div0);
            }
            number(x - d * (x / d).floor())
        }
        "QUOTIENT" => {
            let (x, d) = (n(0)?, n(1)?);
            if d == 0.0 {
                return Err(ErrorKind::Div0);
            }
            number((x / d).trunc())
        }
        "POWER" => {
            let (x, y) = (n(0)?, n(1)?);
            if x == 0.0 && y == 0.0 {
                return Err(ErrorKind::Num);
            }
            number(x.powf(y))
        }
        "SQRT" => {
            let x = n(0)?;
            if x < 0.0 {
                return Err(ErrorKind::Num);
            }
            number(x.sqrt())
        }
        "EXP" => number(n(0)?.exp()),
        "LN" | "LOG10" | "LOG" => {
            let x = n(0)?;
            if x <= 0.0 {
                return Err(ErrorKind::Num);
            }
            match name {
                "LN" => number(x.ln()),
                "LOG10" => number(x.log10()),
                _ => {
                    let base = num_or(args, 1, 10.0, env)?;
                    if base <= 0.0 || base == 1.0 {
                        return Err(ErrorKind::Num);
                    }
                    number(x.log(base))
                }
            }
        }
        "PI" => Value::Number(std::f64::consts::PI),
        "SIN" => number(n(0)?.sin()),
        "COS" => number(n(0)?.cos()),
        "TAN" => number(n(0)?.tan()),
        "DEGREES" => number(n(0)?.to_degrees()),
        "RADIANS" => number(n(0)?.to_radians()),
        "SUM" => number(numbers(args, env)?.iter().sum()),
        "PRODUCT" => {
            let ns = numbers(args, env)?;
            number(if ns.is_empty() {
                0.0
            } else {
                ns.iter().product()
            })
        }
        "AVERAGE" => {
            let ns = numbers(args, env)?;
            if ns.is_empty() {
                return Err(ErrorKind::Div0);
            }
            number(ns.iter().sum::<f64>() / ns.len() as f64)
        }
        "MEDIAN" => {
            let mut ns = numbers(args, env)?;
            if ns.is_empty() {
                return Err(ErrorKind::Num);
            }
            ns.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let mid = ns.len() / 2;
            number(if ns.len() % 2 == 0 {
                (ns[mid - 1] + ns[mid]) / 2.0
            } else {
                ns[mid]
            })
        }
        "MIN" => number(
            numbers(args, env)?
                .into_iter()
                .reduce(f64::min)
                .unwrap_or(0.0),
        ),
        "MAX" => number(
            numbers(args, env)?
                .into_iter()
                .reduce(f64::max)
                .unwrap_or(0.0),
        ),
        "COUNT" => Value::Number(
            args.iter()
                .map(|a| match eval_arg(a, env) {
                    Value::Array(items) => items
                        .iter()
                        .filter(|i| matches!(i, Value::Number(_)))
                        .count(),
                    Value::Number(_) => 1,
                    Value::Bool(_) | Value::Text(_)
                        if !matches!(a, Expr::Ref(_))
                            && to_number(&scalar(eval_arg(a, env))).is_ok() =>
                    {
                        1
                    }
                    _ => 0,
                })
                .sum::<usize>() as f64,
        ),
        "COUNTA" => Value::Number(
            values(args, env)
                .iter()
                .filter(|v| !matches!(v, Value::Blank))
                .count() as f64,
        ),
        "COUNTBLANK" => Value::Number(
            values(args, env)
                .iter()
                .filter(|v| {
                    matches!(v, Value::Blank) || matches!(v, Value::Text(t) if t.is_empty())
                })
                .count() as f64,
        ),
        // ---- Text ----
        "LEN" => Value::Number(t(0)?.chars().count() as f64),
        "LEFT" | "RIGHT" => {
            let s = t(0)?;
            let k = num_or(args, 1, 1.0, env)?;
            if k < 0.0 {
                return Err(ErrorKind::Value);
            }
            let k = k as usize;
            let chars: Vec<char> = s.chars().collect();
            Value::Text(if name == "LEFT" {
                chars.iter().take(k).collect()
            } else {
                chars[chars.len().saturating_sub(k)..].iter().collect()
            })
        }
        "MID" => {
            let s = t(0)?;
            let (start, k) = (n(1)?, n(2)?);
            if start < 1.0 || k < 0.0 {
                return Err(ErrorKind::Value);
            }
            Value::Text(
                s.chars()
                    .skip(start as usize - 1)
                    .take(k as usize)
                    .collect(),
            )
        }
        "UPPER" => Value::Text(t(0)?.to_uppercase()),
        "LOWER" => Value::Text(t(0)?.to_lowercase()),
        "PROPER" => {
            let mut out = String::new();
            let mut prev_letter = false;
            for c in t(0)?.chars() {
                if c.is_alphabetic() {
                    if prev_letter {
                        out.extend(c.to_lowercase());
                    } else {
                        out.extend(c.to_uppercase());
                    }
                    prev_letter = true;
                } else {
                    out.push(c);
                    prev_letter = false;
                }
            }
            Value::Text(out)
        }
        "TRIM" => Value::Text(
            t(0)?
                .split(' ')
                .filter(|w| !w.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        ),
        "CLEAN" => Value::Text(t(0)?.chars().filter(|c| (*c as u32) >= 32).collect()),
        "CONCAT" | "CONCATENATE" => {
            let mut out = String::new();
            for item in values(args, env) {
                out.push_str(&to_text(&item)?);
            }
            Value::Text(out)
        }
        "TEXTJOIN" => {
            if args.len() < 3 {
                return Err(ErrorKind::Value);
            }
            let delim = t(0)?;
            let skip_empty = boolean(args, 1, env)?;
            let mut parts = Vec::new();
            for item in values(&args[2..], env) {
                let s = to_text(&item)?;
                if skip_empty && s.is_empty() {
                    continue;
                }
                parts.push(s);
            }
            Value::Text(parts.join(&delim))
        }
        "SUBSTITUTE" => {
            let (s, old, new) = (t(0)?, t(1)?, t(2)?);
            if old.is_empty() {
                return Ok(Value::Text(s));
            }
            match args.get(3) {
                None => Value::Text(s.replace(&old, &new)),
                Some(_) => {
                    let which = n(3)?.trunc() as usize;
                    if which < 1 {
                        return Err(ErrorKind::Value);
                    }
                    match s.match_indices(&old).nth(which - 1) {
                        Some((at, _)) => {
                            Value::Text(format!("{}{}{}", &s[..at], new, &s[at + old.len()..]))
                        }
                        None => Value::Text(s),
                    }
                }
            }
        }
        "REPLACE" => {
            let s = t(0)?;
            let (start, k) = (n(1)?, n(2)?);
            if start < 1.0 || k < 0.0 {
                return Err(ErrorKind::Value);
            }
            let a = char_index_to_byte(&s, start as usize - 1);
            let b = char_index_to_byte(&s, start as usize - 1 + k as usize);
            Value::Text(format!("{}{}{}", &s[..a], t(3)?, &s[b..]))
        }
        "REPT" => {
            let k = n(1)?;
            if k < 0.0 {
                return Err(ErrorKind::Value);
            }
            Value::Text(t(0)?.repeat(k as usize))
        }
        "FIND" | "SEARCH" => {
            let (needle, hay) = (t(0)?, t(1)?);
            let start = num_or(args, 2, 1.0, env)?;
            if start < 1.0 || start as usize > hay.chars().count() + 1 {
                return Err(ErrorKind::Value);
            }
            let from = char_index_to_byte(&hay, start as usize - 1);
            let found = if name == "FIND" {
                hay[from..].find(&needle).map(|i| from + i)
            } else {
                wildcard_regex(&needle)
                    .and_then(|re| re.find(&hay[from..]))
                    .map(|m| from + m.start())
            };
            let at = found.ok_or(ErrorKind::Value)?;
            Value::Number((hay[..at].chars().count() + 1) as f64)
        }
        "EXACT" => Value::Bool(t(0)? == t(1)?),
        "CHAR" => {
            let c = n(0)?.trunc() as u32;
            if !(1..=255).contains(&c) {
                return Err(ErrorKind::Value);
            }
            Value::Text(char::from_u32(c).map(String::from).unwrap_or_default())
        }
        "CODE" => {
            let s = t(0)?;
            let c = s.chars().next().ok_or(ErrorKind::Value)?;
            Value::Number(f64::from(c as u32))
        }
        "VALUE" => match val(args, 0, env) {
            Value::Number(x) => Value::Number(x),
            other => Value::Number(
                super::eval::text_to_number(&to_text(&other)?).ok_or(ErrorKind::Value)?,
            ),
        },
        "TEXT" => {
            let value = val(args, 0, env);
            let fmt = t(1)?;
            Value::Text(format_text(&value, &fmt)?)
        }
        "FIXED" => {
            let x = n(0)?;
            let d = num_or(args, 1, 2.0, env)?;
            let no_commas = match args.get(2) {
                None | Some(Expr::Missing) => false,
                Some(_) => boolean(args, 2, env)?,
            };
            let decimals = d.max(0.0) as usize;
            let mut fmt = if no_commas {
                "0".to_string()
            } else {
                "#,##0".to_string()
            };
            if decimals > 0 {
                fmt.push('.');
                fmt.push_str(&"0".repeat(decimals));
            }
            Value::Text(format_text(
                &Value::Number(round_to(x, d, round_half_away)),
                &fmt,
            )?)
        }
        "TEXTBEFORE" | "TEXTAFTER" => {
            let (s, delim) = (t(0)?, t(1)?);
            let which = num_or(args, 2, 1.0, env)?.trunc() as i64;
            if which == 0 || delim.is_empty() {
                return Err(ErrorKind::Value);
            }
            let hits: Vec<usize> = s.match_indices(&delim).map(|(i, _)| i).collect();
            let at = if which > 0 {
                hits.get(which as usize - 1)
            } else {
                hits.len()
                    .checked_sub(which.unsigned_abs() as usize)
                    .and_then(|i| hits.get(i))
            }
            .copied()
            .ok_or(ErrorKind::NA)?;
            Value::Text(if name == "TEXTBEFORE" {
                s[..at].to_string()
            } else {
                s[at + delim.len()..].to_string()
            })
        }
        // ---- Date and time ----
        "TODAY" => Value::Number(env.now.floor()),
        "NOW" => Value::Number(env.now),
        "DATE" => Value::Number(make_date(n(0)?, n(1)?, n(2)?)?),
        "TIME" => {
            let secs = n(0)? * 3600.0 + n(1)? * 60.0 + n(2)?;
            if secs < 0.0 {
                return Err(ErrorKind::Num);
            }
            Value::Number((secs / 86_400.0).fract())
        }
        "YEAR" | "MONTH" | "DAY" => {
            let (y, m, d) = ymd(date_arg(args, 0, env)?).ok_or(ErrorKind::Num)?;
            Value::Number(match name {
                "YEAR" => f64::from(y),
                "MONTH" => f64::from(m),
                _ => f64::from(d),
            })
        }
        "HOUR" | "MINUTE" | "SECOND" => {
            let s = date_arg(args, 0, env)?;
            if s < 0.0 {
                return Err(ErrorKind::Num);
            }
            let secs = ((s.fract()) * 86_400.0).round() as i64 % 86_400;
            Value::Number(match name {
                "HOUR" => (secs / 3600) as f64,
                "MINUTE" => ((secs % 3600) / 60) as f64,
                _ => (secs % 60) as f64,
            })
        }
        "EDATE" => Value::Number(add_months(date_arg(args, 0, env)?, n(1)?, false)?),
        "EOMONTH" => Value::Number(add_months(date_arg(args, 0, env)?, n(1)?, true)?),
        "DAYS" => Value::Number(date_arg(args, 0, env)?.floor() - date_arg(args, 1, env)?.floor()),
        "DATEVALUE" => Value::Number(parse_datetime_text(&t(0)?).ok_or(ErrorKind::Value)?.floor()),
        "TIMEVALUE" => Value::Number(parse_datetime_text(&t(0)?).ok_or(ErrorKind::Value)?.fract()),
        "WEEKDAY" => {
            let d = date_of(date_arg(args, 0, env)?)?;
            let kind = num_or(args, 1, 1.0, env)? as i64;
            let from_monday = d.weekday().num_days_from_monday() as f64;
            let from_sunday = d.weekday().num_days_from_sunday() as f64;
            Value::Number(match kind {
                1 | 17 => from_sunday + 1.0,
                2 | 11 => from_monday + 1.0,
                3 => from_monday,
                _ => return Err(ErrorKind::Num),
            })
        }
        "WEEKNUM" => {
            let d = date_of(date_arg(args, 0, env)?)?;
            let kind = num_or(args, 1, 1.0, env)? as i64;
            if kind == 21 {
                Value::Number(f64::from(d.iso_week().week()))
            } else {
                let jan1 = NaiveDate::from_ymd_opt(d.year(), 1, 1).ok_or(ErrorKind::Num)?;
                let offset = match kind {
                    1 | 17 => jan1.weekday().num_days_from_sunday(),
                    2 | 11 => jan1.weekday().num_days_from_monday(),
                    _ => return Err(ErrorKind::Num),
                };
                Value::Number(((d.ordinal0() + offset) / 7 + 1) as f64)
            }
        }
        "ISOWEEKNUM" => Value::Number(f64::from(
            date_of(date_arg(args, 0, env)?)?.iso_week().week(),
        )),
        "DATEDIF" => {
            let (a, b) = (
                date_of(date_arg(args, 0, env)?)?,
                date_of(date_arg(args, 1, env)?)?,
            );
            if a > b {
                return Err(ErrorKind::Num);
            }
            let unit = t(2)?.to_ascii_uppercase();
            let months = (b.year() - a.year()) * 12 + b.month() as i32
                - a.month() as i32
                - i32::from(b.day() < a.day());
            Value::Number(match unit.as_str() {
                "Y" => (months / 12) as f64,
                "M" => months as f64,
                "D" => (b - a).num_days() as f64,
                "YM" => (months % 12) as f64,
                "MD" => {
                    let anchor = add_months(serial_from_date(a), f64::from(months), false)?;
                    (serial_from_date(b) - anchor).max(0.0)
                }
                "YD" => {
                    let years = months / 12;
                    let anchor = add_months(serial_from_date(a), f64::from(years * 12), false)?;
                    serial_from_date(b) - anchor
                }
                _ => return Err(ErrorKind::Num),
            })
        }
        "NETWORKDAYS" => {
            let (a, b) = (
                date_of(date_arg(args, 0, env)?)?,
                date_of(date_arg(args, 1, env)?)?,
            );
            let (lo, hi, sign) = if a <= b { (a, b, 1.0) } else { (b, a, -1.0) };
            let mut count = 0;
            let mut d = lo;
            while d <= hi {
                if !is_weekend(d) {
                    count += 1;
                }
                d = d.succ_opt().ok_or(ErrorKind::Num)?;
            }
            Value::Number(sign * f64::from(count))
        }
        "WORKDAY" => {
            let mut d = date_of(date_arg(args, 0, env)?)?;
            let days = n(1)?.trunc() as i64;
            let step = if days < 0 { -1 } else { 1 };
            let mut left = days.abs();
            while left > 0 {
                d = d
                    .checked_add_signed(Duration::days(step))
                    .ok_or(ErrorKind::Num)?;
                if !is_weekend(d) {
                    left -= 1;
                }
            }
            Value::Number(serial_from_date(d))
        }
        _ => return Err(ErrorKind::Name),
    })
}
