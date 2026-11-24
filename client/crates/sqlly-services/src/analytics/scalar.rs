//! Scalar (formula) cells: small, safe, deterministic expressions over
//! earlier outputs and parameters.
//!
//! The app's Excel-style `formula` engine reads row-relative column
//! references inside one grid and lives in the app crate; a scalar cell
//! needs whole-output references instead, so this is a separate evaluator
//! with SQL-flavoured syntax:
//!
//! - literals: `12`, `3.5e2`, `'text'` (`''` escapes a quote), `true`,
//!   `false`, `null`
//! - references: `revenue` (a value, or a one-cell output), `orders.total`
//!   (a column: aggregated by functions, or the cell of a one-row output),
//!   `orders.total[0]` (0-based row; `[-1]` is the last row), `param.region`,
//!   and `"quoted name"` for names with spaces
//! - operators: `+ - * / % ^`, `||` (concatenate), `= == != <> < <= > >=`,
//!   `and or not`; NULL propagates as in SQL
//! - functions: `if coalesce nullif round floor ceil abs sqrt ln log10 exp
//!   pow sign min max sum avg count count_distinct median first last
//!   safe_div pct_change date date_diff date_add year month day len lower
//!   upper trim concat`
//!
//! Evaluation has no I/O and is bounded: source length, nesting depth, node
//! count and a step budget (every node visited and every row an aggregate
//! reads costs one step).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use chrono::{Datelike, NaiveDateTime};
use serde::{Deserialize, Serialize};
use sqlly_core::CellValue;

use super::decimal::Decimal;
use super::kpi::{Aggregate, AggregateAccumulator};
use super::time::{TimeGrain, diff_in, parse_timestamp};
use super::value::{cell_text, declared_class, format_float, numeric_literal};
use super::{Completeness, OutputView};

pub const MAX_SOURCE_CHARS: usize = 4_096;
pub const MAX_DEPTH: usize = 64;
/// Deepest expression tree (a long `a + b + c …` chain nests one level per
/// operator), which bounds the evaluator's recursion.
pub const MAX_TREE_DEPTH: usize = 256;
pub const MAX_NODES: usize = 2_000;
pub const MAX_STEPS: u64 = 5_000_000;
pub const MAX_TEXT_BYTES: usize = 1 << 20;

/// A scalar cell's value. Whole numbers and decimals from the data stay
/// exact ([`ScalarValue::Integer`], [`ScalarValue::Decimal`]) through
/// addition, subtraction, multiplication, remainders, rounding and
/// comparisons; division, roots, averages and the like give a float.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ScalarValue {
    Null,
    /// A float.
    Number(f64),
    /// An exact whole number.
    Integer(i64),
    /// An exact decimal, as its digits (`12345678901234567.89`).
    Decimal(String),
    Text(String),
    Bool(bool),
    /// UTC.
    Date(NaiveDateTime),
}

impl ScalarValue {
    /// An exact number: an integer when it is whole and fits `i64`, else
    /// its decimal digits.
    pub fn exact(value: Decimal) -> Self {
        match value.to_i64() {
            Some(n) if value.scale() == 0 => ScalarValue::Integer(n),
            _ => ScalarValue::Decimal(value.to_string()),
        }
    }

    pub fn kind(&self) -> ValueKind {
        match self {
            ScalarValue::Null => ValueKind::Null,
            ScalarValue::Number(_) | ScalarValue::Integer(_) | ScalarValue::Decimal(_) => {
                ValueKind::Number
            }
            ScalarValue::Text(_) => ValueKind::Text,
            ScalarValue::Bool(_) => ValueKind::Bool,
            ScalarValue::Date(_) => ValueKind::Date,
        }
    }

    /// The value as a result cell (whole floats become integers, decimals
    /// keep their digits as text, dates ISO text).
    pub fn to_cell(&self) -> CellValue {
        match self {
            ScalarValue::Null => CellValue::Null,
            ScalarValue::Number(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => {
                CellValue::I64(*n as i64)
            }
            ScalarValue::Number(n) => CellValue::F64(*n),
            ScalarValue::Integer(n) => CellValue::I64(*n),
            ScalarValue::Decimal(d) => CellValue::String(d.clone()),
            ScalarValue::Text(t) => CellValue::String(t.clone()),
            ScalarValue::Bool(b) => CellValue::Bool(*b),
            ScalarValue::Date(d) => CellValue::String(format_date(*d)),
        }
    }

    /// Display text (`NULL` for null).
    pub fn display(&self) -> String {
        match self {
            ScalarValue::Null => "NULL".to_string(),
            ScalarValue::Number(n) => format_float(*n),
            ScalarValue::Integer(n) => n.to_string(),
            ScalarValue::Decimal(d) | ScalarValue::Text(d) => d.clone(),
            ScalarValue::Bool(b) => b.to_string(),
            ScalarValue::Date(d) => format_date(*d),
        }
    }

    /// A cell of a column declared as `data_type`: integers stay exact, and
    /// text in a numeric column becomes an exact decimal.
    pub fn from_typed_cell(cell: &CellValue, data_type: &str) -> Self {
        match cell {
            CellValue::Null => ScalarValue::Null,
            CellValue::Bool(b) => ScalarValue::Bool(*b),
            CellValue::I64(n) => ScalarValue::Integer(*n),
            CellValue::F64(f) => ScalarValue::Number(*f),
            CellValue::String(s) => {
                if declared_class(data_type).is_numeric()
                    && let Some(value) = Decimal::parse(s)
                {
                    return ScalarValue::exact(value);
                }
                ScalarValue::Text(s.clone())
            }
            CellValue::Bytes(_) => {
                ScalarValue::Text(cell_text(cell).map(|t| t.into_owned()).unwrap_or_default())
            }
        }
    }
}

/// A parsed expression's value, and how far to trust it.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    pub value: ScalarValue,
    /// It read an output that was truncated, sampled or stopped early (or a
    /// value that was), so it may not describe the whole result.
    pub partial: bool,
    /// An exact number too wide for a float went through float arithmetic,
    /// so trailing digits may differ.
    pub approximate: bool,
}

fn format_date(value: NaiveDateTime) -> String {
    if super::time::has_time_of_day(value) {
        value.format("%Y-%m-%dT%H:%M:%S").to_string()
    } else {
        value.format("%Y-%m-%d").to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueKind {
    Null,
    Number,
    Text,
    Bool,
    Date,
    Column,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParseErrorKind {
    UnexpectedChar { ch: char },
    UnexpectedEnd,
    UnexpectedToken { token: String },
    UnterminatedString,
    UnterminatedIdentifier,
    InvalidNumber,
    InvalidIndex,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScalarError {
    /// `at` is a character offset into the source.
    Parse {
        at: usize,
        error: ParseErrorKind,
    },
    TooLong {
        max_chars: usize,
    },
    TooDeep {
        max_depth: usize,
    },
    TooComplex {
        max_nodes: usize,
    },
    BudgetExceeded {
        max_steps: u64,
    },
    UnknownReference {
        name: String,
    },
    UnknownColumn {
        output: String,
        column: String,
    },
    /// A column reference used as a single value on a multi-row output.
    NotScalar {
        reference: String,
        rows: u64,
    },
    RowOutOfRange {
        reference: String,
        row: i64,
    },
    UnknownFunction {
        name: String,
    },
    WrongArgumentCount {
        function: String,
        expected: String,
    },
    /// `expected` is what the function or operator needed.
    TypeMismatch {
        context: String,
        expected: ValueKind,
        found: ValueKind,
    },
    DivisionByZero,
    InvalidDate {
        text: String,
    },
    InvalidUnit {
        unit: String,
    },
    TextTooLong {
        max_bytes: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum Expr {
    Literal {
        value: ScalarValue,
    },
    Ref {
        path: Vec<String>,
        index: Option<i64>,
    },
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
    },
}

impl Expr {
    /// The names this expression reads (first path segment, `param.x` kept
    /// whole), sorted and de-duplicated, for dependency tracking.
    pub fn references(&self) -> Vec<String> {
        let mut out = BTreeSet::new();
        self.collect_references(&mut out);
        out.into_iter().collect()
    }

    fn collect_references(&self, out: &mut BTreeSet<String>) {
        match self {
            Expr::Literal { .. } => {}
            Expr::Ref { path, .. } => {
                if path.len() >= 2 && path[0].eq_ignore_ascii_case("param") {
                    out.insert(format!("param.{}", path[1]));
                } else if let Some(first) = path.first() {
                    out.insert(first.clone());
                }
            }
            Expr::Unary { expr, .. } => expr.collect_references(out),
            Expr::Binary { left, right, .. } => {
                left.collect_references(out);
                right.collect_references(out);
            }
            Expr::Call { args, .. } => args.iter().for_each(|arg| arg.collect_references(out)),
        }
    }
}

/// Values and outputs a scalar expression can reference.
pub trait ScalarEnv {
    /// A named value: a parameter (`param.region`) or another scalar cell.
    fn value(&self, name: &str) -> Option<ScalarValue>;
    fn output(&self, name: &str) -> Option<OutputView<'_>>;
    /// Whether the named value was itself computed from partial data.
    fn value_is_partial(&self, _name: &str) -> bool {
        false
    }
}

/// A [`ScalarEnv`] over maps; names match exactly, then case-insensitively.
#[derive(Default)]
pub struct MapEnv<'a> {
    pub values: BTreeMap<String, ScalarValue>,
    pub outputs: BTreeMap<String, OutputView<'a>>,
    /// Names in `values` that were computed from partial data.
    pub partial_values: BTreeSet<String>,
}

fn lookup<'m, T>(map: &'m BTreeMap<String, T>, name: &str) -> Option<&'m T> {
    map.get(name).or_else(|| {
        map.iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    })
}

impl ScalarEnv for MapEnv<'_> {
    fn value(&self, name: &str) -> Option<ScalarValue> {
        lookup(&self.values, name).cloned()
    }

    fn output(&self, name: &str) -> Option<OutputView<'_>> {
        lookup(&self.outputs, name).copied()
    }

    fn value_is_partial(&self, name: &str) -> bool {
        self.partial_values
            .iter()
            .any(|key| key == name || key.eq_ignore_ascii_case(name))
    }
}

/// Parse and evaluate in one go.
pub fn eval_str(source: &str, env: &dyn ScalarEnv) -> Result<ScalarValue, ScalarError> {
    evaluate(&parse(source)?, env)
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Number(ScalarValue),
    Str(String),
    Ident(String),
    Quoted(String),
    Sym(&'static str),
}

fn lex(source: &str) -> Result<Vec<(Tok, usize)>, ScalarError> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    let err = |at: usize, error: ParseErrorKind| ScalarError::Parse { at, error };
    while i < chars.len() {
        let c = chars[i];
        let start = i;
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c.is_ascii_digit() || (c == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit)) {
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                i += 1;
            }
            if i < chars.len() && matches!(chars[i], 'e' | 'E') {
                let mut j = i + 1;
                if j < chars.len() && matches!(chars[j], '+' | '-') {
                    j += 1;
                }
                if j < chars.len() && chars[j].is_ascii_digit() {
                    i = j;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text: String = chars[start..i].iter().collect();
            let value = match Decimal::parse(&text) {
                Some(exact) => ScalarValue::exact(exact),
                None => ScalarValue::Number(
                    numeric_literal(&text)
                        .ok_or_else(|| err(start, ParseErrorKind::InvalidNumber))?,
                ),
            };
            tokens.push((Tok::Number(value), start));
            continue;
        }
        if c == '\'' || c == '"' {
            let mut text = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => {
                        return Err(err(
                            start,
                            if c == '\'' {
                                ParseErrorKind::UnterminatedString
                            } else {
                                ParseErrorKind::UnterminatedIdentifier
                            },
                        ));
                    }
                    Some(&q) if q == c => {
                        if chars.get(i + 1) == Some(&c) {
                            text.push(c);
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    }
                    Some(&other) => {
                        text.push(other);
                        i += 1;
                    }
                }
            }
            tokens.push((
                if c == '\'' {
                    Tok::Str(text)
                } else {
                    Tok::Quoted(text)
                },
                start,
            ));
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push((Tok::Ident(chars[start..i].iter().collect()), start));
            continue;
        }
        let two: String = chars[i..(i + 2).min(chars.len())].iter().collect();
        let symbol = ["<=", ">=", "<>", "!=", "==", "||"]
            .into_iter()
            .find(|s| *s == two)
            .or_else(|| {
                [
                    "+", "-", "*", "/", "%", "^", "=", "<", ">", "(", ")", ",", ".", "[", "]",
                ]
                .into_iter()
                .find(|s| s.starts_with(c))
            });
        match symbol {
            Some(symbol) => {
                i += symbol.chars().count();
                tokens.push((Tok::Sym(symbol), start));
            }
            None => return Err(err(start, ParseErrorKind::UnexpectedChar { ch: c })),
        }
    }
    Ok(tokens)
}

/// Byte ranges of every reference's first path segment in `source` whose
/// name is `name` (ASCII case-insensitive): the text to replace when the
/// cell called `name` is renamed. Function names, `param.x` and later path
/// segments (columns) are never included.
pub fn reference_spans(source: &str, name: &str) -> Result<Vec<(usize, usize)>, ScalarError> {
    let tokens = lex(source)?;
    let chars: Vec<(usize, char)> = source.char_indices().collect();
    let byte_at = |i: usize| chars.get(i).map_or(source.len(), |(b, _)| *b);
    let mut out = Vec::new();
    for (n, (tok, start)) in tokens.iter().enumerate() {
        let (text, width) = match tok {
            Tok::Ident(text) => (text, text.chars().count()),
            Tok::Quoted(text) => {
                // The quoted form's own length, doubled quotes included.
                let mut i = start + 1;
                loop {
                    match chars.get(i).map(|(_, c)| *c) {
                        Some('"') if chars.get(i + 1).map(|(_, c)| *c) == Some('"') => i += 2,
                        Some('"') | None => break,
                        Some(_) => i += 1,
                    }
                }
                (text, i + 1 - start)
            }
            _ => continue,
        };
        let after_dot = n > 0 && tokens[n - 1].0 == Tok::Sym(".");
        let called = tokens.get(n + 1).is_some_and(|(t, _)| *t == Tok::Sym("("));
        let keyword = matches!(tok, Tok::Ident(_))
            && matches!(
                text.to_ascii_lowercase().as_str(),
                "true" | "false" | "null" | "and" | "or" | "not"
            );
        if after_dot || called || keyword || !text.eq_ignore_ascii_case(name) {
            continue;
        }
        out.push((byte_at(*start), byte_at(start + width)));
    }
    Ok(out)
}

/// How `name` is written as a reference: bare when it lexes as one
/// identifier, else double-quoted.
pub fn reference_text(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "and" | "or" | "not"
        );
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse an expression, enforcing the source, depth and node limits.
pub fn parse(source: &str) -> Result<Expr, ScalarError> {
    if source.chars().count() > MAX_SOURCE_CHARS {
        return Err(ScalarError::TooLong {
            max_chars: MAX_SOURCE_CHARS,
        });
    }
    let tokens = lex(source)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        depth: 0,
        nodes: 0,
        end: source.chars().count(),
    };
    let expr = parser.expr()?;
    if tree_depth(&expr) > MAX_TREE_DEPTH {
        return Err(ScalarError::TooDeep {
            max_depth: MAX_TREE_DEPTH,
        });
    }
    if let Some((token, at)) = parser.tokens.get(parser.pos) {
        return Err(ScalarError::Parse {
            at: *at,
            error: ParseErrorKind::UnexpectedToken {
                token: describe(token),
            },
        });
    }
    Ok(expr)
}

/// Tree depth without recursion, so a pathological tree cannot overflow
/// the stack while being measured.
fn tree_depth(expr: &Expr) -> usize {
    let mut deepest = 0;
    let mut stack = vec![(expr, 1usize)];
    while let Some((node, depth)) = stack.pop() {
        deepest = deepest.max(depth);
        match node {
            Expr::Literal { .. } | Expr::Ref { .. } => {}
            Expr::Unary { expr, .. } => stack.push((expr, depth + 1)),
            Expr::Binary { left, right, .. } => {
                stack.push((left, depth + 1));
                stack.push((right, depth + 1));
            }
            Expr::Call { args, .. } => stack.extend(args.iter().map(|arg| (arg, depth + 1))),
        }
    }
    deepest
}

fn describe(token: &Tok) -> String {
    match token {
        Tok::Number(n) => n.display(),
        Tok::Str(s) => format!("'{s}'"),
        Tok::Ident(s) => s.clone(),
        Tok::Quoted(s) => format!("\"{s}\""),
        Tok::Sym(s) => s.to_string(),
    }
}

struct Parser {
    tokens: Vec<(Tok, usize)>,
    pos: usize,
    depth: usize,
    nodes: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|(t, _)| t)
    }

    fn at(&self) -> usize {
        self.tokens.get(self.pos).map_or(self.end, |(_, at)| *at)
    }

    fn is_sym(&self, symbol: &str) -> bool {
        matches!(self.peek(), Some(Tok::Sym(s)) if *s == symbol)
    }

    fn is_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s.eq_ignore_ascii_case(keyword))
    }

    fn expect_sym(&mut self, symbol: &str) -> Result<(), ScalarError> {
        if self.is_sym(symbol) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn unexpected(&self) -> ScalarError {
        ScalarError::Parse {
            at: self.at(),
            error: match self.peek() {
                Some(token) => ParseErrorKind::UnexpectedToken {
                    token: describe(token),
                },
                None => ParseErrorKind::UnexpectedEnd,
            },
        }
    }

    fn node(&mut self, expr: Expr) -> Result<Expr, ScalarError> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(ScalarError::TooComplex {
                max_nodes: MAX_NODES,
            });
        }
        Ok(expr)
    }

    fn enter(&mut self) -> Result<(), ScalarError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ScalarError::TooDeep {
                max_depth: MAX_DEPTH,
            });
        }
        Ok(())
    }

    fn binary(&mut self, op: BinaryOp, left: Expr, right: Expr) -> Result<Expr, ScalarError> {
        self.node(Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        })
    }

    fn expr(&mut self) -> Result<Expr, ScalarError> {
        self.enter()?;
        let result = self.or();
        self.depth -= 1;
        result
    }

    fn or(&mut self) -> Result<Expr, ScalarError> {
        let mut left = self.and()?;
        while self.is_keyword("or") {
            self.pos += 1;
            let right = self.and()?;
            left = self.binary(BinaryOp::Or, left, right)?;
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, ScalarError> {
        let mut left = self.not()?;
        while self.is_keyword("and") {
            self.pos += 1;
            let right = self.not()?;
            left = self.binary(BinaryOp::And, left, right)?;
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, ScalarError> {
        if self.is_keyword("not") {
            self.pos += 1;
            self.enter()?;
            let inner = self.not();
            self.depth -= 1;
            let expr = Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(inner?),
            };
            return self.node(expr);
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr, ScalarError> {
        let left = self.concat()?;
        let op = match self.peek() {
            Some(Tok::Sym("=" | "==")) => BinaryOp::Eq,
            Some(Tok::Sym("!=" | "<>")) => BinaryOp::Ne,
            Some(Tok::Sym("<")) => BinaryOp::Lt,
            Some(Tok::Sym("<=")) => BinaryOp::Le,
            Some(Tok::Sym(">")) => BinaryOp::Gt,
            Some(Tok::Sym(">=")) => BinaryOp::Ge,
            _ => return Ok(left),
        };
        self.pos += 1;
        let right = self.concat()?;
        self.binary(op, left, right)
    }

    fn concat(&mut self) -> Result<Expr, ScalarError> {
        let mut left = self.additive()?;
        while self.is_sym("||") {
            self.pos += 1;
            let right = self.additive()?;
            left = self.binary(BinaryOp::Concat, left, right)?;
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr, ScalarError> {
        let mut left = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Sym("+")) => BinaryOp::Add,
                Some(Tok::Sym("-")) => BinaryOp::Sub,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.multiplicative()?;
            left = self.binary(op, left, right)?;
        }
    }

    fn multiplicative(&mut self) -> Result<Expr, ScalarError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Sym("*")) => BinaryOp::Mul,
                Some(Tok::Sym("/")) => BinaryOp::Div,
                Some(Tok::Sym("%")) => BinaryOp::Rem,
                _ => return Ok(left),
            };
            self.pos += 1;
            let right = self.unary()?;
            left = self.binary(op, left, right)?;
        }
    }

    fn unary(&mut self) -> Result<Expr, ScalarError> {
        if self.is_sym("-") || self.is_sym("+") {
            let negate = self.is_sym("-");
            self.pos += 1;
            self.enter()?;
            let inner = self.unary();
            self.depth -= 1;
            let inner = inner?;
            if !negate {
                return Ok(inner);
            }
            return self.node(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(inner),
            });
        }
        self.power()
    }

    fn power(&mut self) -> Result<Expr, ScalarError> {
        let base = self.primary()?;
        if self.is_sym("^") {
            self.pos += 1;
            self.enter()?;
            let exponent = self.unary();
            self.depth -= 1;
            return self.binary(BinaryOp::Pow, base, exponent?);
        }
        Ok(base)
    }

    fn primary(&mut self) -> Result<Expr, ScalarError> {
        let Some(token) = self.peek().cloned() else {
            return Err(self.unexpected());
        };
        match token {
            Tok::Number(value) => {
                self.pos += 1;
                self.node(Expr::Literal { value })
            }
            Tok::Str(text) => {
                self.pos += 1;
                self.node(Expr::Literal {
                    value: ScalarValue::Text(text),
                })
            }
            Tok::Sym("(") => {
                self.pos += 1;
                let inner = self.expr()?;
                self.expect_sym(")")?;
                Ok(inner)
            }
            Tok::Ident(name) => {
                let literal = match name.to_ascii_lowercase().as_str() {
                    "true" => Some(ScalarValue::Bool(true)),
                    "false" => Some(ScalarValue::Bool(false)),
                    "null" => Some(ScalarValue::Null),
                    "and" | "or" | "not" => return Err(self.unexpected()),
                    _ => None,
                };
                self.pos += 1;
                if let Some(value) = literal {
                    return self.node(Expr::Literal { value });
                }
                if self.is_sym("(") {
                    self.pos += 1;
                    let mut args = Vec::new();
                    if !self.is_sym(")") {
                        loop {
                            args.push(self.expr()?);
                            if self.is_sym(",") {
                                self.pos += 1;
                                continue;
                            }
                            break;
                        }
                    }
                    self.expect_sym(")")?;
                    return self.node(Expr::Call {
                        name: name.to_ascii_lowercase(),
                        args,
                    });
                }
                self.reference(name)
            }
            Tok::Quoted(name) => {
                self.pos += 1;
                self.reference(name)
            }
            Tok::Sym(_) => Err(self.unexpected()),
        }
    }

    fn reference(&mut self, first: String) -> Result<Expr, ScalarError> {
        let mut path = vec![first];
        while self.is_sym(".") {
            self.pos += 1;
            match self.peek().cloned() {
                Some(Tok::Ident(name) | Tok::Quoted(name)) => {
                    self.pos += 1;
                    path.push(name);
                }
                _ => return Err(self.unexpected()),
            }
        }
        let mut index = None;
        if self.is_sym("[") {
            self.pos += 1;
            let negative = self.is_sym("-");
            if negative {
                self.pos += 1;
            }
            let at = self.at();
            match self.peek() {
                Some(Tok::Number(ScalarValue::Integer(n))) if n.abs() < 1_000_000_000_000_000 => {
                    let n = *n;
                    index = Some(if negative { -n } else { n });
                    self.pos += 1;
                }
                _ => {
                    return Err(ScalarError::Parse {
                        at,
                        error: ParseErrorKind::InvalidIndex,
                    });
                }
            }
            self.expect_sym("]")?;
        }
        self.node(Expr::Ref { path, index })
    }
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

/// Evaluate a parsed expression.
pub fn evaluate(expr: &Expr, env: &dyn ScalarEnv) -> Result<ScalarValue, ScalarError> {
    evaluate_detailed(expr, env).map(|evaluation| evaluation.value)
}

/// Evaluate, and say whether the value read partial data or lost digits to
/// float arithmetic.
pub fn evaluate_detailed(expr: &Expr, env: &dyn ScalarEnv) -> Result<Evaluation, ScalarError> {
    let mut evaluator = Evaluator {
        env,
        steps: 0,
        partial: false,
        approximate: false,
    };
    let value = evaluator.eval(expr)?;
    Ok(Evaluation {
        value,
        partial: evaluator.partial,
        approximate: evaluator.approximate,
    })
}

struct Evaluator<'e> {
    env: &'e dyn ScalarEnv,
    steps: u64,
    partial: bool,
    approximate: bool,
}

fn type_error(context: &str, expected: ValueKind, found: &ScalarValue) -> ScalarError {
    ScalarError::TypeMismatch {
        context: context.to_string(),
        expected,
        found: found.kind(),
    }
}

/// A number as the evaluator computes with it.
#[derive(Debug, Clone, Copy)]
enum Num {
    Exact(Decimal),
    Float(f64),
}

impl Num {
    fn value(self) -> ScalarValue {
        match self {
            Num::Exact(d) => ScalarValue::exact(d),
            Num::Float(f) => ScalarValue::Number(f),
        }
    }

    fn is_zero(self) -> bool {
        match self {
            Num::Exact(d) => d.is_zero(),
            Num::Float(f) => f == 0.0,
        }
    }
}

fn cmp_num(a: Num, b: Num) -> Ordering {
    match (a, b) {
        (Num::Exact(x), Num::Exact(y)) => x.cmp(&y),
        (Num::Float(x), Num::Float(y)) => x.total_cmp(&y),
        (Num::Exact(x), Num::Float(y)) => match Decimal::from_f64(y) {
            Some(y) => x.cmp(&y),
            None => x.to_f64().total_cmp(&y),
        },
        (Num::Float(_), Num::Exact(_)) => cmp_num(b, a).reverse(),
    }
}

fn text_num(text: &str) -> Option<Num> {
    Decimal::parse(text)
        .map(Num::Exact)
        .or_else(|| numeric_literal(text).map(Num::Float))
}

/// The number a value holds (numeric text included), `None` otherwise.
fn num_of(value: &ScalarValue) -> Option<Num> {
    match value {
        ScalarValue::Number(n) => Some(Num::Float(*n)),
        ScalarValue::Integer(n) => Some(Num::Exact(Decimal::from_i64(*n))),
        ScalarValue::Decimal(t) | ScalarValue::Text(t) => text_num(t),
        _ => None,
    }
}

/// A cell's number for an aggregate (finite only), as `finite_number`
/// reads it but exact where it can be.
fn cell_num(cell: &CellValue) -> Option<Num> {
    match cell {
        CellValue::I64(n) => Some(Num::Exact(Decimal::from_i64(*n))),
        CellValue::F64(f) if f.is_finite() => Some(Num::Float(*f)),
        CellValue::String(text) => text_num(text).filter(|n| match n {
            Num::Float(f) => f.is_finite(),
            Num::Exact(_) => true,
        }),
        _ => None,
    }
}

fn as_date(value: &ScalarValue, context: &str) -> Result<Option<NaiveDateTime>, ScalarError> {
    match value {
        ScalarValue::Null => Ok(None),
        ScalarValue::Date(d) => Ok(Some(*d)),
        ScalarValue::Text(t) => parse_timestamp(t)
            .map(Some)
            .ok_or_else(|| ScalarError::InvalidDate { text: t.clone() }),
        other => Err(type_error(context, ValueKind::Date, other)),
    }
}

fn as_bool(value: &ScalarValue, context: &str) -> Result<Option<bool>, ScalarError> {
    match value {
        ScalarValue::Null => Ok(None),
        ScalarValue::Bool(b) => Ok(Some(*b)),
        ScalarValue::Number(_) | ScalarValue::Integer(_) | ScalarValue::Decimal(_) => {
            Ok(num_of(value).map(|n| !n.is_zero()))
        }
        other => Err(type_error(context, ValueKind::Bool, other)),
    }
}

fn text_of(value: &ScalarValue) -> Option<String> {
    match value {
        ScalarValue::Null => None,
        other => Some(other.display()),
    }
}

fn unit_of(value: &ScalarValue) -> Result<TimeGrain, ScalarError> {
    let text = match value {
        ScalarValue::Text(t) => t.to_ascii_lowercase(),
        other => return Err(type_error("date unit", ValueKind::Text, other)),
    };
    let unit = text.trim_end_matches('s');
    TimeGrain::ALL
        .into_iter()
        .find(|grain| grain.as_str() == unit)
        .ok_or(ScalarError::InvalidUnit { unit: text })
}

/// A column of an output, as an aggregate argument.
struct ColumnArg<'v> {
    view: OutputView<'v>,
    column: Option<usize>,
}

fn column_arg<'e>(env: &'e dyn ScalarEnv, expr: &Expr) -> Option<ColumnArg<'e>> {
    let Expr::Ref { path, index: None } = expr else {
        return None;
    };
    if env.value(&path.join(".")).is_some() {
        return None;
    }
    let view = env.output(&path[0])?;
    match path.len() {
        1 => Some(ColumnArg {
            column: (view.columns.len() == 1).then_some(0),
            view,
        }),
        2 => Some(ColumnArg {
            column: view.column_index(&path[1]),
            view,
        }),
        _ => None,
    }
}

/// Sum, average, extremes, median, first and last over numbers, exact while
/// every input is exact and the sum fits 38 digits.
struct NumAggregate {
    aggregate: Aggregate,
    count: u64,
    sum: Option<Num>,
    min: Option<Num>,
    max: Option<Num>,
    first: Option<Num>,
    last: Option<Num>,
    values: Vec<Num>,
    /// An exact input or sum did not fit a float.
    lossy: bool,
}

fn add_nums(a: Num, b: Num, lossy: &mut bool) -> Num {
    if let (Num::Exact(x), Num::Exact(y)) = (a, b)
        && let Some(sum) = x.checked_add(y)
    {
        return Num::Exact(sum);
    }
    let float = |n: Num, lossy: &mut bool| match n {
        Num::Exact(d) => {
            *lossy |= !d.fits_f64();
            d.to_f64()
        }
        Num::Float(f) => f,
    };
    if matches!((a, b), (Num::Exact(_), Num::Exact(_))) {
        *lossy = true;
    }
    Num::Float(float(a, lossy) + float(b, lossy))
}

impl NumAggregate {
    fn new(aggregate: Aggregate) -> Self {
        Self {
            aggregate,
            count: 0,
            sum: None,
            min: None,
            max: None,
            first: None,
            last: None,
            values: Vec::new(),
            lossy: false,
        }
    }

    fn push(&mut self, n: Num) {
        self.count += 1;
        self.sum = Some(match self.sum {
            None => n,
            Some(sum) => add_nums(sum, n, &mut self.lossy),
        });
        if self.min.is_none_or(|m| cmp_num(n, m).is_lt()) {
            self.min = Some(n);
        }
        if self.max.is_none_or(|m| cmp_num(n, m).is_gt()) {
            self.max = Some(n);
        }
        self.first.get_or_insert(n);
        self.last = Some(n);
        if self.aggregate == Aggregate::Median {
            self.values.push(n);
        }
    }

    /// The result, and whether exact digits were lost on the way.
    fn finish(mut self) -> (Option<Num>, bool) {
        let float_of = |n: Num, lossy: &mut bool| match n {
            Num::Exact(d) => {
                *lossy |= !d.fits_f64();
                d.to_f64()
            }
            Num::Float(f) => f,
        };
        let mut lossy = self.lossy;
        let result = match self.aggregate {
            Aggregate::Sum => Some(self.sum.unwrap_or(Num::Exact(Decimal::ZERO))),
            Aggregate::Avg => self
                .sum
                .filter(|_| self.count > 0)
                .map(|sum| Num::Float(float_of(sum, &mut lossy) / self.count as f64)),
            Aggregate::Min => self.min,
            Aggregate::Max => self.max,
            Aggregate::First => self.first,
            Aggregate::Last => self.last,
            Aggregate::Median => {
                self.values.sort_by(|a, b| cmp_num(*a, *b));
                let mid = self.values.len() / 2;
                match self.values.len() {
                    0 => None,
                    n if n % 2 == 1 => Some(self.values[mid]),
                    _ => {
                        let (a, b) = (self.values[mid - 1], self.values[mid]);
                        let half = Decimal::new(5, 1).unwrap_or(Decimal::ZERO);
                        Some(match add_nums(a, b, &mut lossy) {
                            Num::Exact(sum) => sum
                                .checked_mul(half)
                                .map(Num::Exact)
                                .unwrap_or_else(|| Num::Float(sum.to_f64() / 2.0)),
                            Num::Float(sum) => Num::Float(sum / 2.0),
                        })
                    }
                }
            }
            Aggregate::Count | Aggregate::CountNonNull | Aggregate::CountDistinct => {
                Some(Num::Exact(Decimal::from_i64(self.count as i64)))
            }
        };
        let approximate = lossy && matches!(result, Some(Num::Float(_)));
        (result, approximate)
    }
}

/// Whether reading `index` (or the single row) of a view with
/// `completeness` sees the same value the whole result would: a truncated
/// or stopped-early result keeps its leading rows exactly.
fn row_is_exact(completeness: Completeness, index: Option<i64>) -> bool {
    completeness.is_complete()
        || (matches!(
            completeness,
            Completeness::Truncated { .. } | Completeness::Partial
        ) && index.is_some_and(|i| i >= 0))
}

impl Evaluator<'_> {
    fn step(&mut self, cost: u64) -> Result<(), ScalarError> {
        self.steps += cost;
        if self.steps > MAX_STEPS {
            Err(ScalarError::BudgetExceeded {
                max_steps: MAX_STEPS,
            })
        } else {
            Ok(())
        }
    }

    fn num(&self, value: &ScalarValue, context: &str) -> Result<Option<Num>, ScalarError> {
        match value {
            ScalarValue::Null => Ok(None),
            ScalarValue::Bool(b) => Ok(Some(Num::Exact(Decimal::from_i64(i64::from(*b))))),
            ScalarValue::Date(_) => Err(type_error(context, ValueKind::Number, value)),
            other => num_of(other)
                .map(Some)
                .ok_or_else(|| type_error(context, ValueKind::Number, value)),
        }
    }

    fn float_value(&mut self, n: Num) -> f64 {
        match n {
            Num::Float(f) => f,
            Num::Exact(d) => {
                if !d.fits_f64() {
                    self.approximate = true;
                }
                d.to_f64()
            }
        }
    }

    fn float(&mut self, value: &ScalarValue, context: &str) -> Result<Option<f64>, ScalarError> {
        let n = self.num(value, context)?;
        Ok(n.map(|n| self.float_value(n)))
    }

    fn arith(&mut self, op: BinaryOp, a: Num, b: Num) -> Result<Num, ScalarError> {
        if matches!(op, BinaryOp::Div | BinaryOp::Rem) && b.is_zero() {
            return Err(ScalarError::DivisionByZero);
        }
        if let (Num::Exact(x), Num::Exact(y)) = (a, b) {
            let exact = match op {
                BinaryOp::Add => x.checked_add(y),
                BinaryOp::Sub => x.checked_sub(y),
                BinaryOp::Mul => x.checked_mul(y),
                BinaryOp::Rem => x.checked_rem(y),
                _ => None,
            };
            if let Some(value) = exact {
                return Ok(Num::Exact(value));
            }
            if matches!(
                op,
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Rem
            ) {
                self.approximate = true;
            }
        }
        let x = self.float_value(a);
        let y = self.float_value(b);
        Ok(Num::Float(match op {
            BinaryOp::Add => x + y,
            BinaryOp::Sub => x - y,
            BinaryOp::Mul => x * y,
            BinaryOp::Div => x / y,
            BinaryOp::Rem => x % y,
            _ => x.powf(y),
        }))
    }

    fn eval(&mut self, expr: &Expr) -> Result<ScalarValue, ScalarError> {
        self.step(1)?;
        match expr {
            Expr::Literal { value } => Ok(value.clone()),
            Expr::Ref { path, index } => self.reference(path, *index),
            Expr::Unary { op, expr } => {
                let value = self.eval(expr)?;
                match op {
                    UnaryOp::Neg => Ok(self.num(&value, "-")?.map_or(ScalarValue::Null, |n| {
                        match n {
                            Num::Exact(d) => Num::Exact(-d),
                            Num::Float(f) => Num::Float(-f),
                        }
                        .value()
                    })),
                    UnaryOp::Not => Ok(as_bool(&value, "not")?
                        .map_or(ScalarValue::Null, |b| ScalarValue::Bool(!b))),
                }
            }
            Expr::Binary { op, left, right } => self.binary(*op, left, right),
            Expr::Call { name, args } => self.call(name, args),
        }
    }

    fn reference(
        &mut self,
        path: &[String],
        index: Option<i64>,
    ) -> Result<ScalarValue, ScalarError> {
        let full = path.join(".");
        if index.is_none()
            && let Some(value) = self.env.value(&full)
        {
            if self.env.value_is_partial(&full) {
                self.partial = true;
            }
            return Ok(value);
        }
        let output_name = &path[0];
        let Some(view) = self.env.output(output_name) else {
            return Err(ScalarError::UnknownReference { name: full });
        };
        let column = match path.get(1) {
            Some(name) => {
                Some(
                    view.column_index(name)
                        .ok_or_else(|| ScalarError::UnknownColumn {
                            output: output_name.clone(),
                            column: name.clone(),
                        })?,
                )
            }
            None if view.columns.len() == 1 => Some(0),
            None => None,
        };
        if path.len() > 2 {
            return Err(ScalarError::UnknownReference { name: full });
        }
        let rows = view.row_count();
        let Some(column) = column else {
            return Err(ScalarError::NotScalar {
                reference: full,
                rows,
            });
        };
        let row = match index {
            Some(i) if i < 0 => rows as i64 + i,
            Some(i) => i,
            None if rows == 1 => 0,
            None => {
                return Err(ScalarError::NotScalar {
                    reference: full,
                    rows,
                });
            }
        };
        if row < 0 || row as u64 >= rows {
            return Err(ScalarError::RowOutOfRange {
                reference: full,
                row: index.unwrap_or(0),
            });
        }
        if !row_is_exact(view.completeness, index) {
            self.partial = true;
        }
        self.step(row as u64 + 1)?;
        let data_type = view
            .columns
            .get(column)
            .map(|c| c.data_type.as_str())
            .unwrap_or_default();
        let mut found = ScalarValue::Null;
        view.for_each_row(|at, cells| {
            if at as i64 == row {
                found = ScalarValue::from_typed_cell(
                    cells.get(column).unwrap_or(&CellValue::Null),
                    data_type,
                );
                return ControlFlow::Break(());
            }
            ControlFlow::Continue(())
        });
        Ok(found)
    }

    fn binary(
        &mut self,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
    ) -> Result<ScalarValue, ScalarError> {
        if matches!(op, BinaryOp::And | BinaryOp::Or) {
            let l = as_bool(&self.eval(left)?, "and/or")?;
            // Short-circuit like SQL: false AND x is false, true OR x is true.
            match (op, l) {
                (BinaryOp::And, Some(false)) => return Ok(ScalarValue::Bool(false)),
                (BinaryOp::Or, Some(true)) => return Ok(ScalarValue::Bool(true)),
                _ => {}
            }
            let r = as_bool(&self.eval(right)?, "and/or")?;
            return Ok(match (op, l, r) {
                (BinaryOp::And, _, Some(false)) => ScalarValue::Bool(false),
                (BinaryOp::And, Some(true), Some(true)) => ScalarValue::Bool(true),
                (BinaryOp::Or, _, Some(true)) => ScalarValue::Bool(true),
                (BinaryOp::Or, Some(false), Some(false)) => ScalarValue::Bool(false),
                _ => ScalarValue::Null,
            });
        }
        let l = self.eval(left)?;
        let r = self.eval(right)?;
        if matches!(l, ScalarValue::Null) || matches!(r, ScalarValue::Null) {
            return Ok(ScalarValue::Null);
        }
        match op {
            BinaryOp::Concat => {
                let text = format!("{}{}", l.display(), r.display());
                if text.len() > MAX_TEXT_BYTES {
                    return Err(ScalarError::TextTooLong {
                        max_bytes: MAX_TEXT_BYTES,
                    });
                }
                Ok(ScalarValue::Text(text))
            }
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Le
            | BinaryOp::Gt
            | BinaryOp::Ge => {
                let ordering = compare(&l, &r)?;
                use std::cmp::Ordering::*;
                Ok(ScalarValue::Bool(match op {
                    BinaryOp::Eq => ordering == Equal,
                    BinaryOp::Ne => ordering != Equal,
                    BinaryOp::Lt => ordering == Less,
                    BinaryOp::Le => ordering != Greater,
                    BinaryOp::Gt => ordering == Greater,
                    _ => ordering != Less,
                }))
            }
            _ => {
                let symbol = match op {
                    BinaryOp::Add => "+",
                    BinaryOp::Sub => "-",
                    BinaryOp::Mul => "*",
                    BinaryOp::Div => "/",
                    BinaryOp::Rem => "%",
                    _ => "^",
                };
                let (Some(a), Some(b)) = (self.num(&l, symbol)?, self.num(&r, symbol)?) else {
                    return Ok(ScalarValue::Null);
                };
                Ok(self.arith(op, a, b)?.value())
            }
        }
    }

    fn arity(&self, name: &str, args: &[Expr], min: usize, max: usize) -> Result<(), ScalarError> {
        if args.len() < min || args.len() > max {
            let expected = if min == max {
                min.to_string()
            } else if max == usize::MAX {
                format!("{min}+")
            } else {
                format!("{min}-{max}")
            };
            return Err(ScalarError::WrongArgumentCount {
                function: name.to_string(),
                expected,
            });
        }
        Ok(())
    }

    fn finish_aggregate(&mut self, aggregate: NumAggregate) -> ScalarValue {
        let (result, approximate) = aggregate.finish();
        self.approximate |= approximate;
        result.map_or(ScalarValue::Null, Num::value)
    }

    fn aggregate(
        &mut self,
        name: &str,
        aggregate: Aggregate,
        args: &[Expr],
    ) -> Result<ScalarValue, ScalarError> {
        let counts = matches!(aggregate, Aggregate::Count | Aggregate::CountDistinct);
        if args.len() == 1
            && let Some(arg) = column_arg(self.env, &args[0])
        {
            let Expr::Ref { path, .. } = &args[0] else {
                unreachable!("column_arg checked")
            };
            if !arg.view.completeness.is_complete() {
                self.partial = true;
            }
            if aggregate == Aggregate::Count && path.len() == 1 && arg.column.is_none() {
                let rows = arg.view.row_count();
                return Ok(ScalarValue::Integer(rows as i64));
            }
            let column = arg.column.ok_or_else(|| ScalarError::UnknownColumn {
                output: path[0].clone(),
                column: path.get(1).cloned().unwrap_or_default(),
            })?;
            let rows = arg.view.row_count();
            self.step(rows)?;
            if counts {
                let mut accumulator = AggregateAccumulator::new(match aggregate {
                    Aggregate::Count => Aggregate::CountNonNull,
                    other => other,
                });
                arg.view.for_each_row(|_, row| {
                    accumulator.push(row.get(column).unwrap_or(&CellValue::Null));
                    ControlFlow::Continue(())
                });
                return Ok(accumulator
                    .finish()
                    .map_or(ScalarValue::Null, |n| ScalarValue::Integer(n as i64)));
            }
            let mut numbers = NumAggregate::new(aggregate);
            arg.view.for_each_row(|_, row| {
                if let Some(n) = row.get(column).and_then(cell_num) {
                    numbers.push(n);
                }
                ControlFlow::Continue(())
            });
            return Ok(self.finish_aggregate(numbers));
        }
        // Scalar arguments: sum(a, b, …), count(a, b, …), and so on.
        self.arity(name, args, 1, usize::MAX)?;
        if counts {
            let mut accumulator = AggregateAccumulator::new(match aggregate {
                Aggregate::Count => Aggregate::CountNonNull,
                other => other,
            });
            for arg in args {
                let cell = match self.eval(arg)? {
                    ScalarValue::Null => CellValue::Null,
                    ScalarValue::Number(n) => CellValue::F64(n),
                    ScalarValue::Integer(n) => CellValue::I64(n),
                    other => CellValue::String(other.display()),
                };
                accumulator.push(&cell);
            }
            return Ok(accumulator
                .finish()
                .map_or(ScalarValue::Null, |n| ScalarValue::Integer(n as i64)));
        }
        let mut numbers = NumAggregate::new(aggregate);
        for arg in args {
            let value = self.eval(arg)?;
            if let Some(n) = self.num(&value, name)? {
                numbers.push(n);
            }
        }
        Ok(self.finish_aggregate(numbers))
    }

    fn number_fn(
        &mut self,
        name: &str,
        args: &[Expr],
        f: fn(f64) -> f64,
    ) -> Result<ScalarValue, ScalarError> {
        self.arity(name, args, 1, 1)?;
        let value = self.eval(&args[0])?;
        Ok(self
            .float(&value, name)?
            .map_or(ScalarValue::Null, |n| ScalarValue::Number(f(n))))
    }

    /// A function that stays exact on exact input (`exact` returning `None`
    /// falls back to the float `f`).
    fn exact_fn(
        &mut self,
        name: &str,
        args: &[Expr],
        exact: fn(Decimal) -> Option<Decimal>,
        f: fn(f64) -> f64,
    ) -> Result<ScalarValue, ScalarError> {
        self.arity(name, args, 1, 1)?;
        let value = self.eval(&args[0])?;
        Ok(match self.num(&value, name)? {
            None => ScalarValue::Null,
            Some(Num::Exact(d)) => match exact(d) {
                Some(d) => ScalarValue::exact(d),
                None => ScalarValue::Number(f(self.float_value(Num::Exact(d)))),
            },
            Some(Num::Float(n)) => ScalarValue::Number(f(n)),
        })
    }

    /// A whole-number argument (truncated toward zero).
    fn whole(&mut self, value: &ScalarValue, context: &str) -> Result<Option<i64>, ScalarError> {
        Ok(match self.num(value, context)? {
            None => None,
            Some(Num::Exact(d)) => Some(d.to_i64().unwrap_or_else(|| d.to_f64() as i64)),
            Some(Num::Float(f)) => Some(f as i64),
        })
    }

    fn call(&mut self, name: &str, args: &[Expr]) -> Result<ScalarValue, ScalarError> {
        match name {
            "if" | "iif" => {
                self.arity(name, args, 2, 3)?;
                let condition = as_bool(&self.eval(&args[0])?, name)?;
                if condition == Some(true) {
                    self.eval(&args[1])
                } else if let Some(otherwise) = args.get(2) {
                    self.eval(otherwise)
                } else {
                    Ok(ScalarValue::Null)
                }
            }
            "coalesce" => {
                self.arity(name, args, 1, usize::MAX)?;
                for arg in args {
                    let value = self.eval(arg)?;
                    if !matches!(value, ScalarValue::Null) {
                        return Ok(value);
                    }
                }
                Ok(ScalarValue::Null)
            }
            "nullif" => {
                self.arity(name, args, 2, 2)?;
                let a = self.eval(&args[0])?;
                let b = self.eval(&args[1])?;
                if !matches!(a, ScalarValue::Null)
                    && !matches!(b, ScalarValue::Null)
                    && compare(&a, &b)? == std::cmp::Ordering::Equal
                {
                    Ok(ScalarValue::Null)
                } else {
                    Ok(a)
                }
            }
            "round" => {
                self.arity(name, args, 1, 2)?;
                let value = self.eval(&args[0])?;
                let digits = match args.get(1) {
                    Some(arg) => {
                        let digits = self.eval(arg)?;
                        self.whole(&digits, name)?.unwrap_or(0)
                    }
                    None => 0,
                };
                let digits = digits.clamp(-15, 15) as i32;
                Ok(match self.num(&value, name)? {
                    None => ScalarValue::Null,
                    Some(Num::Exact(d)) => match d.round(digits) {
                        Some(d) => ScalarValue::exact(d),
                        None => ScalarValue::Number(round_half_away(
                            self.float_value(Num::Exact(d)),
                            digits,
                        )),
                    },
                    Some(Num::Float(v)) => ScalarValue::Number(round_half_away(v, digits)),
                })
            }
            "floor" => self.exact_fn(name, args, Decimal::floor, f64::floor),
            "ceil" | "ceiling" => self.exact_fn(name, args, Decimal::ceil, f64::ceil),
            "abs" => self.exact_fn(name, args, |d| Some(d.abs()), f64::abs),
            "sign" => self.exact_fn(
                name,
                args,
                |d| Some(Decimal::from_i64(d.signum())),
                |v| if v == 0.0 { 0.0 } else { v.signum() },
            ),
            "sqrt" => self.number_fn(name, args, f64::sqrt),
            "ln" => self.number_fn(name, args, f64::ln),
            "log10" => self.number_fn(name, args, f64::log10),
            "exp" => self.number_fn(name, args, f64::exp),
            "pow" | "power" => {
                self.arity(name, args, 2, 2)?;
                let a = self.eval(&args[0])?;
                let b = self.eval(&args[1])?;
                let a = self.float(&a, name)?;
                let b = self.float(&b, name)?;
                Ok(match (a, b) {
                    (Some(a), Some(b)) => ScalarValue::Number(a.powf(b)),
                    _ => ScalarValue::Null,
                })
            }
            "sum" => self.aggregate(name, Aggregate::Sum, args),
            "avg" | "mean" => self.aggregate(name, Aggregate::Avg, args),
            "min" => self.aggregate(name, Aggregate::Min, args),
            "max" => self.aggregate(name, Aggregate::Max, args),
            "count" => self.aggregate(name, Aggregate::Count, args),
            "count_distinct" => self.aggregate(name, Aggregate::CountDistinct, args),
            "median" => self.aggregate(name, Aggregate::Median, args),
            "first" => self.aggregate(name, Aggregate::First, args),
            "last" => self.aggregate(name, Aggregate::Last, args),
            "safe_div" => {
                self.arity(name, args, 2, 2)?;
                let a = self.eval(&args[0])?;
                let b = self.eval(&args[1])?;
                Ok(match (self.num(&a, name)?, self.num(&b, name)?) {
                    (Some(a), Some(b)) if !b.is_zero() => self.arith(BinaryOp::Div, a, b)?.value(),
                    _ => ScalarValue::Null,
                })
            }
            "pct_change" => {
                self.arity(name, args, 2, 2)?;
                let new = self.eval(&args[0])?;
                let old = self.eval(&args[1])?;
                let new = self.float(&new, name)?;
                let old = self.float(&old, name)?;
                Ok(match (new, old) {
                    (Some(new), Some(old)) if old != 0.0 => {
                        ScalarValue::Number((new - old) / old.abs() * 100.0)
                    }
                    _ => ScalarValue::Null,
                })
            }
            "date" => {
                self.arity(name, args, 1, 1)?;
                let value = self.eval(&args[0])?;
                Ok(as_date(&value, name)?.map_or(ScalarValue::Null, ScalarValue::Date))
            }
            "date_diff" | "datediff" => {
                self.arity(name, args, 3, 3)?;
                let unit = unit_of(&self.eval(&args[0])?)?;
                let a = as_date(&self.eval(&args[1])?, name)?;
                let b = as_date(&self.eval(&args[2])?, name)?;
                Ok(match (a, b) {
                    (Some(a), Some(b)) => ScalarValue::Integer(diff_in(unit, a, b)),
                    _ => ScalarValue::Null,
                })
            }
            "date_add" | "dateadd" => {
                self.arity(name, args, 3, 3)?;
                let unit = unit_of(&self.eval(&args[0])?)?;
                let amount = self.eval(&args[1])?;
                let amount = self.whole(&amount, name)?;
                let date = as_date(&self.eval(&args[2])?, name)?;
                Ok(match (amount, date) {
                    (Some(amount), Some(date)) => {
                        date_add(unit, amount, date).map_or(ScalarValue::Null, ScalarValue::Date)
                    }
                    _ => ScalarValue::Null,
                })
            }
            "year" | "month" | "day" => {
                self.arity(name, args, 1, 1)?;
                let date = as_date(&self.eval(&args[0])?, name)?;
                Ok(date.map_or(ScalarValue::Null, |d| {
                    ScalarValue::Integer(i64::from(match name {
                        "year" => d.year(),
                        "month" => d.month() as i32,
                        _ => d.day() as i32,
                    }))
                }))
            }
            "len" | "length" => {
                self.arity(name, args, 1, 1)?;
                let value = self.eval(&args[0])?;
                Ok(text_of(&value).map_or(ScalarValue::Null, |t| {
                    ScalarValue::Integer(t.chars().count() as i64)
                }))
            }
            "lower" | "upper" | "trim" => {
                self.arity(name, args, 1, 1)?;
                let value = self.eval(&args[0])?;
                Ok(text_of(&value).map_or(ScalarValue::Null, |t| {
                    ScalarValue::Text(match name {
                        "lower" => t.to_lowercase(),
                        "upper" => t.to_uppercase(),
                        _ => t.trim().to_string(),
                    })
                }))
            }
            "concat" => {
                let mut text = String::new();
                for arg in args {
                    if let Some(part) = text_of(&self.eval(arg)?) {
                        text.push_str(&part);
                        if text.len() > MAX_TEXT_BYTES {
                            return Err(ScalarError::TextTooLong {
                                max_bytes: MAX_TEXT_BYTES,
                            });
                        }
                    }
                }
                Ok(ScalarValue::Text(text))
            }
            _ => Err(ScalarError::UnknownFunction {
                name: name.to_string(),
            }),
        }
    }
}

fn date_add(unit: TimeGrain, amount: i64, date: NaiveDateTime) -> Option<NaiveDateTime> {
    let months = |n: i64| -> Option<NaiveDateTime> {
        let shifted = if n >= 0 {
            date.date()
                .checked_add_months(chrono::Months::new(u32::try_from(n).ok()?))
        } else {
            date.date()
                .checked_sub_months(chrono::Months::new(u32::try_from(-n).ok()?))
        }?;
        Some(shifted.and_time(date.time()))
    };
    match unit {
        TimeGrain::Second => date.checked_add_signed(chrono::Duration::try_seconds(amount)?),
        TimeGrain::Minute => date.checked_add_signed(chrono::Duration::try_minutes(amount)?),
        TimeGrain::Hour => date.checked_add_signed(chrono::Duration::try_hours(amount)?),
        TimeGrain::Day => date.checked_add_signed(chrono::Duration::try_days(amount)?),
        TimeGrain::Week => date.checked_add_signed(chrono::Duration::try_weeks(amount)?),
        TimeGrain::Month => months(amount),
        TimeGrain::Quarter => months(amount.checked_mul(3)?),
        TimeGrain::Year => months(amount.checked_mul(12)?),
    }
}

/// SQL-style rounding of a float: halves away from zero, `digits` may be
/// negative.
fn round_half_away(value: f64, digits: i32) -> f64 {
    let factor = 10f64.powi(digits);
    let scaled = value * factor;
    // Nudge by the representation error so 2.675 rounds to 2.68.
    let nudged = scaled + scaled.signum() * scaled.abs() * f64::EPSILON * 4.0;
    nudged.round() / factor
}

fn compare(a: &ScalarValue, b: &ScalarValue) -> Result<Ordering, ScalarError> {
    use ScalarValue as V;
    let mismatch = || ScalarError::TypeMismatch {
        context: "comparison".to_string(),
        expected: a.kind(),
        found: b.kind(),
    };
    match (a, b) {
        (V::Bool(x), V::Bool(y)) => Ok(x.cmp(y)),
        (V::Text(x), V::Text(y)) => Ok(x.cmp(y)),
        (V::Date(x), V::Date(y)) => Ok(x.cmp(y)),
        (V::Date(x), V::Text(t)) => parse_timestamp(t).map(|y| x.cmp(&y)).ok_or_else(mismatch),
        (V::Text(t), V::Date(y)) => parse_timestamp(t).map(|x| x.cmp(y)).ok_or_else(mismatch),
        _ => match (num_of(a), num_of(b)) {
            (Some(x), Some(y)) => Ok(cmp_num(x, y)),
            _ => Err(mismatch()),
        },
    }
}

#[cfg(test)]
#[path = "scalar_tests.rs"]
mod tests;
