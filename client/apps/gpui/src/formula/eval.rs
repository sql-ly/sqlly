//! Evaluation with Excel's coercion and comparison rules.

use super::parser::Expr;
use super::{CellRef, ErrorKind, Value, format_general, parse_datetime_text};

/// Where a formula reads its cells and variables from.
pub struct EvalEnv<'a> {
    /// The 0-based result row being computed.
    pub row: i64,
    /// A cell: `None` for a column the result does not have; a row outside
    /// the result is `Some(Value::Blank)`.
    pub cell: &'a dyn Fn(&str, i64) -> Option<Value>,
    /// The column names from `from` to `to` inclusive, in result order —
    /// for ranges spanning columns. `None` when either is unknown.
    pub columns_between: &'a dyn Fn(&str, &str) -> Option<Vec<String>>,
    /// A declared variable (`@name`).
    pub variable: &'a dyn Fn(&str) -> Option<Value>,
    /// TODAY()/NOW(), as a serial number.
    pub now: f64,
}

/// Evaluate `expr` for `env.row`. A bare range is a `#VALUE!` here, as it
/// is in a single Excel cell; functions take ranges through [`eval_arg`].
pub fn evaluate(expr: &Expr, env: &EvalEnv) -> Value {
    match eval_arg(expr, env) {
        Value::Array(_) => Value::Error(ErrorKind::Value),
        // A formula that reads only an empty cell shows 0, as in Excel.
        Value::Blank if matches!(expr, Expr::Ref(_)) => Value::Number(0.0),
        v => v,
    }
}

/// Evaluate allowing a range result (function arguments).
pub(crate) fn eval_arg(expr: &Expr, env: &EvalEnv) -> Value {
    match expr {
        Expr::Number(n) => Value::Number(*n),
        Expr::Text(t) => Value::Text(t.clone()),
        Expr::Bool(b) => Value::Bool(*b),
        Expr::Error(e) => Value::Error(*e),
        Expr::Missing => Value::Blank,
        Expr::Ref(r) => read_cell(r, env),
        Expr::Range(from, to) => read_range(from, to, env),
        Expr::Var(name) => (env.variable)(name).unwrap_or(Value::Error(ErrorKind::Name)),
        Expr::Neg(e) => match to_number(&scalar(eval_arg(e, env))) {
            Ok(n) => Value::Number(-n),
            Err(e) => Value::Error(e),
        },
        Expr::Percent(e) => match to_number(&scalar(eval_arg(e, env))) {
            Ok(n) => Value::Number(n / 100.0),
            Err(e) => Value::Error(e),
        },
        Expr::Binary(op, a, b) => {
            let a = scalar(eval_arg(a, env));
            let b = scalar(eval_arg(b, env));
            binary(op, &a, &b)
        }
        Expr::Call(name, args) => super::functions::call(name, args, env),
    }
}

/// A range used where one value is expected: `#VALUE!`.
pub(crate) fn scalar(v: Value) -> Value {
    match v {
        Value::Array(_) => Value::Error(ErrorKind::Value),
        v => v,
    }
}

fn read_cell(r: &CellRef, env: &EvalEnv) -> Value {
    (env.cell)(&r.column, r.resolve_row(env.row)).unwrap_or(Value::Error(ErrorKind::Ref))
}

fn read_range(from: &CellRef, to: &CellRef, env: &EvalEnv) -> Value {
    let Some(columns) = (env.columns_between)(&from.column, &to.column) else {
        return Value::Error(ErrorKind::Ref);
    };
    let (a, b) = (from.resolve_row(env.row), to.resolve_row(env.row));
    let (lo, hi) = (a.min(b), a.max(b));
    let mut out = Vec::new();
    for row in lo..=hi {
        for column in &columns {
            out.push((env.cell)(column, row).unwrap_or(Value::Error(ErrorKind::Ref)));
        }
    }
    Value::Array(out)
}

/// Excel's number coercion: blank is 0, booleans are 1/0, numeric or date
/// text converts, other text is `#VALUE!`.
pub(crate) fn to_number(v: &Value) -> Result<f64, ErrorKind> {
    match v {
        Value::Blank => Ok(0.0),
        Value::Number(n) => Ok(*n),
        Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
        Value::Error(e) => Err(*e),
        Value::Text(t) => text_to_number(t).ok_or(ErrorKind::Value),
        Value::Array(items) => items.first().map_or(Ok(0.0), to_number),
    }
}

pub(crate) fn text_to_number(t: &str) -> Option<f64> {
    let t = t.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(p) = t.strip_suffix('%') {
        return p
            .trim()
            .replace(',', "")
            .parse::<f64>()
            .ok()
            .map(|n| n / 100.0);
    }
    let cleaned = t.replace(',', "");
    let cleaned = cleaned.strip_prefix('$').unwrap_or(&cleaned);
    if let Ok(n) = cleaned.parse::<f64>()
        && n.is_finite()
    {
        return Some(n);
    }
    parse_datetime_text(t)
}

pub(crate) fn to_text(v: &Value) -> Result<String, ErrorKind> {
    match v {
        Value::Blank => Ok(String::new()),
        Value::Number(n) => Ok(format_general(*n)),
        Value::Text(t) => Ok(t.clone()),
        Value::Bool(b) => Ok(if *b { "TRUE" } else { "FALSE" }.into()),
        Value::Error(e) => Err(*e),
        Value::Array(items) => items.first().map_or(Ok(String::new()), to_text),
    }
}

pub(crate) fn to_bool(v: &Value) -> Result<bool, ErrorKind> {
    match v {
        Value::Blank => Ok(false),
        Value::Number(n) => Ok(*n != 0.0),
        Value::Bool(b) => Ok(*b),
        Value::Error(e) => Err(*e),
        Value::Text(t) if t.eq_ignore_ascii_case("TRUE") => Ok(true),
        Value::Text(t) if t.eq_ignore_ascii_case("FALSE") => Ok(false),
        Value::Text(_) => Err(ErrorKind::Value),
        Value::Array(items) => items.first().map_or(Ok(false), to_bool),
    }
}

fn binary(op: &str, a: &Value, b: &Value) -> Value {
    if let Value::Error(e) = a {
        return Value::Error(*e);
    }
    if let Value::Error(e) = b {
        return Value::Error(*e);
    }
    match op {
        "&" => match (to_text(a), to_text(b)) {
            (Ok(x), Ok(y)) => Value::Text(x + &y),
            (Err(e), _) | (_, Err(e)) => Value::Error(e),
        },
        "=" | "<>" | "<" | ">" | "<=" | ">=" => {
            let ord = compare(a, b);
            Value::Bool(match op {
                "=" => ord == std::cmp::Ordering::Equal,
                "<>" => ord != std::cmp::Ordering::Equal,
                "<" => ord == std::cmp::Ordering::Less,
                ">" => ord == std::cmp::Ordering::Greater,
                "<=" => ord != std::cmp::Ordering::Greater,
                _ => ord != std::cmp::Ordering::Less,
            })
        }
        _ => {
            let (x, y) = match (to_number(a), to_number(b)) {
                (Ok(x), Ok(y)) => (x, y),
                (Err(e), _) | (_, Err(e)) => return Value::Error(e),
            };
            let n = match op {
                "+" => x + y,
                "-" => x - y,
                "*" => x * y,
                "/" => {
                    if y == 0.0 {
                        return Value::Error(ErrorKind::Div0);
                    }
                    x / y
                }
                "^" => {
                    if x == 0.0 && y == 0.0 {
                        return Value::Error(ErrorKind::Num);
                    }
                    x.powf(y)
                }
                _ => return Value::Error(ErrorKind::Value),
            };
            number(n)
        }
    }
}

/// A finite number, or `#NUM!`.
pub(crate) fn number(n: f64) -> Value {
    if n.is_finite() {
        Value::Number(n)
    } else {
        Value::Error(ErrorKind::Num)
    }
}

/// Excel's comparison: a blank takes the other side's type (0, "", or
/// FALSE), numbers sort before text before booleans, and text compares
/// without regard to case.
pub(crate) fn compare(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Number(_) | Value::Blank => 0,
            Value::Text(_) => 1,
            Value::Bool(_) => 2,
            _ => 3,
        }
    }
    let a = blank_as(a, b);
    let b = blank_as(&b.clone(), &a);
    match (&a, &b) {
        (Value::Number(x), Value::Number(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Value::Text(x), Value::Text(y)) => x.to_lowercase().cmp(&y.to_lowercase()),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => rank(&a).cmp(&rank(&b)),
    }
}

fn blank_as(v: &Value, other: &Value) -> Value {
    match (v, other) {
        (Value::Blank, Value::Text(_)) => Value::Text(String::new()),
        (Value::Blank, Value::Bool(_)) => Value::Bool(false),
        (Value::Blank, _) => Value::Number(0.0),
        (v, _) => v.clone(),
    }
}
