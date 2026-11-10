//! A deliberately small evaluator for SQL Server `CHECK` constraint
//! expressions, used for client-side prevalidation in the row editor.
//!
//! This is **not** a SQL engine. It parses the common shapes that column and
//! table checks take — comparisons, `IN`, `BETWEEN`, `IS [NOT] NULL`, and
//! `AND`/`OR`/parenthesised combinations over bracketed columns and numeric or
//! string literals — and evaluates them with SQL's three-valued logic. Anything
//! it does not fully understand (functions, `LIKE`, subqueries, bare
//! identifiers, type mismatches) yields [`CheckOutcome::Unknown`], meaning
//! "cannot decide locally — defer to the server", so it never reports a false
//! violation. The database remains the sole authority; this only catches
//! obvious mistakes before a round trip.

use sqlly_core::CellValue;

/// The result of evaluating a check against the current draft values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The predicate is definitely satisfied (TRUE or three-valued UNKNOWN,
    /// both of which a `CHECK` allows).
    Satisfied,
    /// The predicate is definitely FALSE — the row would be rejected.
    Violated,
    /// Not locally decidable; the server must judge it.
    Unknown,
}

/// Extracts the bracketed column identifiers a definition references, in order
/// of first appearance and de-duplicated. Used to know which fields to resolve
/// and to attach a violation message to.
pub fn referenced_columns(definition: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = definition.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'['
            && let Some(end) = definition[i + 1..].find(']')
        {
            let name = &definition[i + 1..i + 1 + end];
            if !out.iter().any(|n| n == name) {
                out.push(name.to_string());
            }
            i = i + 1 + end + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// Evaluates a check definition against a value resolver. `resolve` returns the
/// current draft value for a referenced column, or `None` when it cannot be
/// supplied (in which case the outcome is [`CheckOutcome::Unknown`]).
pub fn evaluate_check(
    definition: &str,
    resolve: &dyn Fn(&str) -> Option<CellValue>,
) -> CheckOutcome {
    let tokens = match tokenize(definition) {
        Some(tokens) if !tokens.is_empty() => tokens,
        _ => return CheckOutcome::Unknown,
    };
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
        resolve,
    };
    match parser.parse_or() {
        Some(tri) if parser.pos == tokens.len() => match tri {
            Tri::False => CheckOutcome::Violated,
            Tri::True | Tri::Unknown => CheckOutcome::Satisfied,
        },
        // Trailing tokens or a construct we could not parse: don't guess.
        _ => CheckOutcome::Unknown,
    }
}

/// SQL three-valued logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tri {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    LParen,
    RParen,
    Comma,
    Op(Cmp),
    And,
    Or,
    In,
    Between,
    Is,
    Not,
    Null,
    Column(String),
    Number(f64),
    Text(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn tokenize(input: &str) -> Option<Vec<Token>> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    // Bound work so a pathological definition can't stall the UI thread.
    if chars.len() > 4000 {
        return None;
    }
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '(' => {
                tokens.push(Token::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                i += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                i += 1;
            }
            '=' => {
                tokens.push(Token::Op(Cmp::Eq));
                i += 1;
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Op(Cmp::Le));
                    i += 2;
                } else if chars.get(i + 1) == Some(&'>') {
                    tokens.push(Token::Op(Cmp::Ne));
                    i += 2;
                } else {
                    tokens.push(Token::Op(Cmp::Lt));
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Op(Cmp::Ge));
                    i += 2;
                } else {
                    tokens.push(Token::Op(Cmp::Gt));
                    i += 1;
                }
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Token::Op(Cmp::Ne));
                    i += 2;
                } else {
                    return None;
                }
            }
            '[' => {
                let mut name = String::new();
                i += 1;
                while i < chars.len() && chars[i] != ']' {
                    name.push(chars[i]);
                    i += 1;
                }
                if i >= chars.len() {
                    return None; // unterminated identifier
                }
                i += 1; // consume ']'
                tokens.push(Token::Column(name));
            }
            'N' if chars.get(i + 1) == Some(&'\'') => {
                // Unicode string literal N'...'
                i += 1;
                let (text, next) = read_string(&chars, i)?;
                tokens.push(Token::Text(text));
                i = next;
            }
            '\'' => {
                let (text, next) = read_string(&chars, i)?;
                tokens.push(Token::Text(text));
                i = next;
            }
            '0'..='9' | '.' | '+' | '-' => {
                let start = i;
                i += 1;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                    i += 1;
                }
                let literal: String = chars[start..i].iter().collect();
                match literal.parse::<f64>() {
                    Ok(n) => tokens.push(Token::Number(n)),
                    Err(_) => return None,
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                match word.to_ascii_uppercase().as_str() {
                    "AND" => tokens.push(Token::And),
                    "OR" => tokens.push(Token::Or),
                    "IN" => tokens.push(Token::In),
                    "BETWEEN" => tokens.push(Token::Between),
                    "IS" => tokens.push(Token::Is),
                    "NOT" => tokens.push(Token::Not),
                    "NULL" => tokens.push(Token::Null),
                    // A bare identifier (function, unbracketed column) means we
                    // cannot safely evaluate.
                    _ => return None,
                }
            }
            _ => return None,
        }
    }
    Some(tokens)
}

fn read_string(chars: &[char], start: usize) -> Option<(String, usize)> {
    // `chars[start]` is the opening quote.
    let mut i = start + 1;
    let mut out = String::new();
    while i < chars.len() {
        if chars[i] == '\'' {
            if chars.get(i + 1) == Some(&'\'') {
                out.push('\'');
                i += 2;
            } else {
                return Some((out, i + 1));
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    None // unterminated
}

/// An evaluated operand: a concrete value, or NULL/unresolvable.
enum Operand {
    Number(f64),
    Text(String),
    Null,
}

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    resolve: &'a dyn Fn(&str) -> Option<CellValue>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<&Token> {
        let token = self.tokens.get(self.pos);
        if token.is_some() {
            self.pos += 1;
        }
        token
    }

    fn parse_or(&mut self) -> Option<Tri> {
        let mut acc = self.parse_and()?;
        while matches!(self.peek(), Some(Token::Or)) {
            self.pos += 1;
            let rhs = self.parse_and()?;
            acc = tri_or(acc, rhs);
        }
        Some(acc)
    }

    fn parse_and(&mut self) -> Option<Tri> {
        let mut acc = self.parse_term()?;
        while matches!(self.peek(), Some(Token::And)) {
            self.pos += 1;
            let rhs = self.parse_term()?;
            acc = tri_and(acc, rhs);
        }
        Some(acc)
    }

    fn parse_term(&mut self) -> Option<Tri> {
        // A parenthesised sub-expression, but only when the parentheses wrap a
        // boolean expression rather than a literal operand like `(0)`.
        if matches!(self.peek(), Some(Token::LParen)) && self.paren_wraps_expr() {
            self.pos += 1; // (
            let inner = self.parse_or()?;
            if !matches!(self.advance(), Some(Token::RParen)) {
                return None;
            }
            return Some(inner);
        }
        self.parse_predicate()
    }

    /// Looks ahead to decide whether the `(` at `pos` opens a boolean group
    /// (contains a comparison/IN/BETWEEN/IS at its own paren depth) versus a
    /// parenthesised operand such as `(0)`.
    fn paren_wraps_expr(&self) -> bool {
        let mut depth = 0;
        let mut i = self.pos;
        while i < self.tokens.len() {
            match &self.tokens[i] {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        return false;
                    }
                }
                Token::Op(_) | Token::In | Token::Between | Token::Is if depth == 1 => {
                    return true;
                }
                Token::And | Token::Or if depth == 1 => return true,
                _ => {}
            }
            i += 1;
        }
        false
    }

    fn parse_predicate(&mut self) -> Option<Tri> {
        let left = self.parse_operand()?;
        match self.peek() {
            Some(Token::Op(cmp)) => {
                let cmp = *cmp;
                self.pos += 1;
                let right = self.parse_operand()?;
                Some(compare(&left, cmp, &right))
            }
            Some(Token::Is) => {
                self.pos += 1;
                let negated = if matches!(self.peek(), Some(Token::Not)) {
                    self.pos += 1;
                    true
                } else {
                    false
                };
                if !matches!(self.advance(), Some(Token::Null)) {
                    return None;
                }
                let is_null = matches!(left, Operand::Null);
                let result = if negated { !is_null } else { is_null };
                Some(if result { Tri::True } else { Tri::False })
            }
            Some(Token::In) => {
                self.pos += 1;
                if !matches!(self.advance(), Some(Token::LParen)) {
                    return None;
                }
                let mut any_true = Tri::False;
                loop {
                    let item = self.parse_operand()?;
                    let eq = compare(&left, Cmp::Eq, &item);
                    any_true = tri_or(any_true, eq);
                    match self.advance() {
                        Some(Token::Comma) => continue,
                        Some(Token::RParen) => break,
                        _ => return None,
                    }
                }
                Some(any_true)
            }
            Some(Token::Between) => {
                self.pos += 1;
                let low = self.parse_operand()?;
                if !matches!(self.advance(), Some(Token::And)) {
                    return None;
                }
                let high = self.parse_operand()?;
                let ge = compare(&left, Cmp::Ge, &low);
                let le = compare(&left, Cmp::Le, &high);
                Some(tri_and(ge, le))
            }
            _ => None,
        }
    }

    fn parse_operand(&mut self) -> Option<Operand> {
        let token = self.tokens.get(self.pos)?.clone();
        self.pos += 1;
        match token {
            Token::LParen => {
                let inner = self.parse_operand()?;
                if !matches!(self.advance(), Some(Token::RParen)) {
                    return None;
                }
                Some(inner)
            }
            Token::Number(n) => Some(Operand::Number(n)),
            Token::Text(s) => Some(Operand::Text(s)),
            Token::Column(name) => (self.resolve)(&name).map(|value| cell_to_operand(&value)),
            _ => None,
        }
    }
}

fn cell_to_operand(value: &CellValue) -> Operand {
    match value {
        CellValue::Null => Operand::Null,
        CellValue::Bool(b) => Operand::Number(if *b { 1.0 } else { 0.0 }),
        CellValue::I64(n) => Operand::Number(*n as f64),
        CellValue::F64(f) => Operand::Number(*f),
        CellValue::String(s) => match s.trim().parse::<f64>() {
            Ok(n) => Operand::Number(n),
            Err(_) => Operand::Text(s.clone()),
        },
        CellValue::Bytes(_) => Operand::Text(String::new()),
    }
}

fn compare(left: &Operand, cmp: Cmp, right: &Operand) -> Tri {
    use Operand::*;
    let ordering = match (left, right) {
        (Null, _) | (_, Null) => return Tri::Unknown,
        (Number(a), Number(b)) => a.partial_cmp(b),
        (Text(a), Text(b)) => Some(a.cmp(b)),
        // A number vs a numeric-looking string: compare numerically.
        (Number(a), Text(b)) | (Text(b), Number(a)) => match b.trim().parse::<f64>() {
            Ok(bn) => {
                // Preserve operand order for the comparison.
                if matches!(left, Number(_)) {
                    a.partial_cmp(&bn)
                } else {
                    bn.partial_cmp(a)
                }
            }
            Err(_) => return Tri::Unknown,
        },
    };
    let Some(ordering) = ordering else {
        return Tri::Unknown;
    };
    let result = match cmp {
        Cmp::Eq => ordering.is_eq(),
        Cmp::Ne => ordering.is_ne(),
        Cmp::Lt => ordering.is_lt(),
        Cmp::Le => ordering.is_le(),
        Cmp::Gt => ordering.is_gt(),
        Cmp::Ge => ordering.is_ge(),
    };
    if result { Tri::True } else { Tri::False }
}

fn tri_and(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::False, _) | (_, Tri::False) => Tri::False,
        (Tri::True, Tri::True) => Tri::True,
        _ => Tri::Unknown,
    }
}

fn tri_or(a: Tri, b: Tri) -> Tri {
    match (a, b) {
        (Tri::True, _) | (_, Tri::True) => Tri::True,
        (Tri::False, Tri::False) => Tri::False,
        _ => Tri::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn resolver(values: &[(&str, CellValue)]) -> impl Fn(&str) -> Option<CellValue> {
        let map: HashMap<String, CellValue> = values
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn referenced_columns_are_extracted_in_order() {
        assert_eq!(
            referenced_columns("([Age]>=(0) AND [Age]<=[Max])"),
            vec!["Age".to_string(), "Max".to_string()]
        );
    }

    #[test]
    fn numeric_comparison_pass_and_fail() {
        let def = "([Age]>=(0))";
        assert_eq!(
            evaluate_check(def, &resolver(&[("Age", CellValue::I64(5))])),
            CheckOutcome::Satisfied
        );
        assert_eq!(
            evaluate_check(def, &resolver(&[("Age", CellValue::I64(-1))])),
            CheckOutcome::Violated
        );
    }

    #[test]
    fn range_with_and() {
        let def = "([Age]>=(0) AND [Age]<=(120))";
        let r = |n: i64| evaluate_check(def, &resolver(&[("Age", CellValue::I64(n))]));
        assert_eq!(r(0), CheckOutcome::Satisfied);
        assert_eq!(r(120), CheckOutcome::Satisfied);
        assert_eq!(r(121), CheckOutcome::Violated);
    }

    #[test]
    fn between_is_inclusive() {
        let def = "([Score] BETWEEN (1) AND (10))";
        let r = |n: i64| evaluate_check(def, &resolver(&[("Score", CellValue::I64(n))]));
        assert_eq!(r(1), CheckOutcome::Satisfied);
        assert_eq!(r(10), CheckOutcome::Satisfied);
        assert_eq!(r(11), CheckOutcome::Violated);
    }

    #[test]
    fn string_in_list_and_equality() {
        let def = "([Status]='A' OR [Status]='B')";
        assert_eq!(
            evaluate_check(def, &resolver(&[("Status", CellValue::String("A".into()))])),
            CheckOutcome::Satisfied
        );
        assert_eq!(
            evaluate_check(def, &resolver(&[("Status", CellValue::String("Z".into()))])),
            CheckOutcome::Violated
        );

        let in_def = "([Status] IN ('A','B','C'))";
        assert_eq!(
            evaluate_check(
                in_def,
                &resolver(&[("Status", CellValue::String("C".into()))])
            ),
            CheckOutcome::Satisfied
        );
        assert_eq!(
            evaluate_check(
                in_def,
                &resolver(&[("Status", CellValue::String("D".into()))])
            ),
            CheckOutcome::Violated
        );
    }

    #[test]
    fn null_value_makes_check_pass() {
        // NULL comparisons are UNKNOWN, which a CHECK permits.
        let def = "([Age]>=(0))";
        assert_eq!(
            evaluate_check(def, &resolver(&[("Age", CellValue::Null)])),
            CheckOutcome::Satisfied
        );
    }

    #[test]
    fn unicode_and_escaped_string_literals() {
        let def = "([Name]=N'O''Brien')";
        assert_eq!(
            evaluate_check(
                def,
                &resolver(&[("Name", CellValue::String("O'Brien".into()))])
            ),
            CheckOutcome::Satisfied
        );
    }

    #[test]
    fn is_not_null_predicate() {
        let def = "([Email] IS NOT NULL)";
        assert_eq!(
            evaluate_check(
                def,
                &resolver(&[("Email", CellValue::String("x@y".into()))])
            ),
            CheckOutcome::Satisfied
        );
        assert_eq!(
            evaluate_check(def, &resolver(&[("Email", CellValue::Null)])),
            CheckOutcome::Violated
        );
    }

    #[test]
    fn unparseable_defs_are_unknown_not_violated() {
        // Function call — we must not guess.
        assert_eq!(
            evaluate_check(
                "(len([Code])>(3))",
                &resolver(&[("Code", CellValue::String("ab".into()))])
            ),
            CheckOutcome::Unknown
        );
        // Unresolvable column.
        assert_eq!(
            evaluate_check("([Age]>=(0))", &resolver(&[])),
            CheckOutcome::Unknown
        );
    }

    #[test]
    fn or_of_ranges() {
        let def = "([Q]<(0) OR [Q]>(100))";
        let r = |n: i64| evaluate_check(def, &resolver(&[("Q", CellValue::I64(n))]));
        assert_eq!(r(-5), CheckOutcome::Satisfied);
        assert_eq!(r(50), CheckOutcome::Violated);
        assert_eq!(r(150), CheckOutcome::Satisfied);
    }
}
