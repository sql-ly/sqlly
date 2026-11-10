//! Signature help: while the cursor sits inside `name( … )`, report which
//! callable is being invoked, which argument the cursor is in, and what that
//! argument expects. Built-in functions come from the dialect's vocabulary
//! (`builtins`). Pure; runs on the completion debounce, never on the raw
//! keystroke path.

use sqlly_lang::sqlmodel::Dialect;

use crate::intellisense::builtins::builtins;
use crate::intellisense::complete::scan::{self, ScanTokenKind};
use crate::intellisense::goto::RoutineKind;
use crate::intellisense::hover::{HoverParameter, RoutineSignature};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallContext {
    /// Name as written (dotted chain joined with `.`), delimiters stripped.
    pub callee: String,
    /// Byte offset of the opening paren.
    pub open_paren: usize,
    /// Zero-based index of the argument the cursor is in.
    pub active_parameter: usize,
}

/// Find the innermost unclosed call the cursor is inside. Uses `scan_tokens`
/// so parens inside strings/comments are ignored. Returns `None` when the
/// cursor is not inside a call or the token before `(` is not a Word
/// (e.g. `(SELECT …)` subquery, `IN (…)` list, `VALUES (…)`).
pub fn call_context_at(sql: &str, cursor: usize) -> Option<CallContext> {
    let tokens = scan::scan_tokens(sql);
    let mut last_before: Option<usize> = None;
    for (i, tok) in tokens.iter().enumerate() {
        if tok.byte_offset < cursor {
            last_before = Some(i);
        } else {
            break;
        }
    }
    let mut i = last_before?;
    let mut depth: i32 = 0;
    let mut commas = 0usize;
    loop {
        match tokens[i].kind {
            ScanTokenKind::RightParen => depth += 1,
            ScanTokenKind::LeftParen => {
                if depth == 0 {
                    return call_at_open_paren(&tokens, i, commas);
                }
                depth -= 1;
            }
            ScanTokenKind::Comma => {
                if depth == 0 {
                    commas += 1;
                }
            }
            ScanTokenKind::Semicolon if depth == 0 => {
                return None;
            }
            _ => {}
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

const NON_CALL_KEYWORDS: &[&str] = &[
    "IN", "VALUES", "EXISTS", "AND", "OR", "NOT", "SELECT", "WHERE", "ON", "AS", "OVER", "FROM",
    "JOIN", "SET", "WITH", "INSERT", "INTO", "UPDATE", "DELETE", "THEN", "ELSE", "WHEN", "CASE",
];

fn call_at_open_paren(
    tokens: &[scan::ScanToken<'_>],
    open_idx: usize,
    commas: usize,
) -> Option<CallContext> {
    if open_idx == 0 {
        return None;
    }
    let prev = &tokens[open_idx - 1];
    if prev.kind != ScanTokenKind::Word {
        return None;
    }
    let word = prev.text.as_str();
    if NON_CALL_KEYWORDS
        .iter()
        .any(|k| word.eq_ignore_ascii_case(k))
    {
        return None;
    }
    let callee = dotted_callee(tokens, open_idx - 1);
    Some(CallContext {
        callee,
        open_paren: tokens[open_idx].byte_offset,
        active_parameter: commas,
    })
}

/// Walk back over `Dot`/`Word` pairs to assemble `schema.name` (delimiters
/// already stripped by the scanner).
fn dotted_callee(tokens: &[scan::ScanToken<'_>], word_idx: usize) -> String {
    let mut parts = vec![tokens[word_idx].text.as_str().to_string()];
    let mut i = word_idx;
    while i >= 2
        && tokens[i - 1].kind == ScanTokenKind::Dot
        && tokens[i - 2].kind == ScanTokenKind::Word
    {
        parts.push(tokens[i - 2].text.as_str().to_string());
        i -= 2;
    }
    parts.reverse();
    parts.join(".")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureHelp {
    pub label: String,
    pub parameters: Vec<String>,
    /// Byte range of each parameter's text inside `label`, so the active one
    /// can be emphasized without searching the label for its text.
    pub parameter_ranges: Vec<(usize, usize)>,
    /// One line per parameter: its expected type and what it means (empty
    /// when unknown, as for live routines without documentation).
    pub parameter_docs: Vec<String>,
    pub active_parameter: Option<usize>,
    pub kind: RoutineKind,
    /// Opening paren of the call this help describes (for Esc dismissal).
    pub open_paren: usize,
    /// What the function does, for built-ins.
    pub summary: Option<String>,
    /// Which overload `label` shows (zero-based) and how many exist.
    pub overload_index: usize,
    pub overload_count: usize,
    /// The words the active argument accepts (date parts and the like).
    pub active_values: Vec<String>,
}

/// Resolve a call context to a signature in SQL Server's vocabulary. See
/// [`signature_help_for`].
pub fn signature_help_at(
    sql: &str,
    cursor: usize,
    routines: &[RoutineSignature],
) -> Option<SignatureHelp> {
    signature_help_for(Dialect::SqlServer, sql, cursor, routines)
}

/// Resolve a call context to a signature: live routine signatures first
/// (procedures and user functions), then `dialect`'s built-in functions,
/// choosing the overload that accepts the argument the cursor is in.
pub fn signature_help_for(
    dialect: Dialect,
    sql: &str,
    cursor: usize,
    routines: &[RoutineSignature],
) -> Option<SignatureHelp> {
    let ctx = call_context_at(sql, cursor)?;
    if let Some(sig) = match_live_routine(routines, &ctx.callee) {
        let parameters = sig
            .parameters
            .iter()
            .map(format_live_param)
            .collect::<Vec<_>>();
        let (label, parameter_ranges) = format_routine_label(sig, &parameters);
        let active_parameter = map_active(ctx.active_parameter, &parameters);
        return Some(SignatureHelp {
            label,
            parameter_docs: vec![String::new(); parameters.len()],
            parameters,
            parameter_ranges,
            active_parameter,
            kind: RoutineKind::Procedure,
            open_paren: ctx.open_paren,
            summary: None,
            overload_index: 0,
            overload_count: 1,
            active_values: Vec::new(),
        });
    }
    let last = ctx.callee.rsplit('.').next().unwrap_or(&ctx.callee);
    // A qualified name (`dbo.LEN`) is a user routine, never a built-in.
    if ctx.callee.contains('.') {
        return None;
    }
    let function = builtins(dialect).function(last)?;
    let overload_index = function.best_signature(ctx.active_parameter);
    let signature = &function.signatures[overload_index];
    let (label, parameter_ranges) = function.label(overload_index);
    let parameters = signature.params.iter().map(|p| p.name.clone()).collect();
    let parameter_docs = signature
        .params
        .iter()
        .map(|p| {
            if p.ty.is_empty() {
                p.doc.clone()
            } else {
                format!("{} — {}", p.ty, p.doc)
            }
        })
        .collect();
    let active_parameter = signature.param_for_arg(ctx.active_parameter);
    let active_values = active_parameter
        .map(|i| signature.params[i].values.clone())
        .unwrap_or_default();
    Some(SignatureHelp {
        label,
        parameters,
        parameter_ranges,
        parameter_docs,
        active_parameter,
        kind: RoutineKind::Function,
        open_paren: ctx.open_paren,
        summary: Some(function.summary.clone()),
        overload_index,
        overload_count: function.signatures.len(),
        active_values,
    })
}

fn match_live_routine<'a>(
    routines: &'a [RoutineSignature],
    callee: &str,
) -> Option<&'a RoutineSignature> {
    let last = callee.rsplit('.').next().unwrap_or(callee);
    routines.iter().find(|r| {
        let dotted = format!("{}.{}", r.schema, r.name);
        dotted.eq_ignore_ascii_case(callee) || r.name.eq_ignore_ascii_case(last)
    })
}

fn format_live_param(p: &HoverParameter) -> String {
    let mut s = p.name.clone();
    if !p.data_type.is_empty() {
        s.push(' ');
        s.push_str(&p.data_type);
    }
    if p.is_output {
        s.push_str(" OUTPUT");
    }
    s
}

fn format_routine_label(
    sig: &RoutineSignature,
    parameters: &[String],
) -> (String, Vec<(usize, usize)>) {
    let mut label = format!("{}.{}(", sig.schema, sig.name);
    let mut ranges = Vec::with_capacity(parameters.len());
    for (i, p) in parameters.iter().enumerate() {
        if i > 0 {
            label.push_str(", ");
        }
        ranges.push((label.len(), label.len() + p.len()));
        label.push_str(p);
    }
    label.push(')');
    (label, ranges)
}

fn map_active(index: usize, parameters: &[String]) -> Option<usize> {
    (index < parameters.len()).then_some(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intellisense::hover::HoverParameter;

    fn pipe(sql: &str) -> (String, usize) {
        let cursor = sql.find('|').expect("cursor mark");
        let mut out = String::with_capacity(sql.len() - 1);
        out.push_str(&sql[..cursor]);
        out.push_str(&sql[cursor + 1..]);
        (out, cursor)
    }

    fn ctx(sql: &str) -> CallContext {
        let (text, cursor) = pipe(sql);
        call_context_at(&text, cursor).expect("call context")
    }

    fn help(sql: &str, routines: &[RoutineSignature]) -> SignatureHelp {
        let (text, cursor) = pipe(sql);
        signature_help_at(&text, cursor, routines).expect("signature help")
    }

    #[test]
    fn counts_top_level_commas_only() {
        let c = ctx("DATEADD(day, COALESCE(a, b, c), |)");
        assert_eq!(c.callee, "DATEADD");
        assert_eq!(c.active_parameter, 2);
    }

    #[test]
    fn ignores_commas_and_parens_inside_strings() {
        let c = ctx("CONCAT('a,(b', |)");
        assert_eq!(c.callee, "CONCAT");
        assert_eq!(c.active_parameter, 1);
    }

    #[test]
    fn ignores_commas_inside_comments() {
        let c = ctx("CONCAT(a /* , , */, |)");
        assert_eq!(c.active_parameter, 1);
    }

    #[test]
    fn returns_none_outside_any_call() {
        let (text, cursor) = pipe("SELECT a, b|");
        assert!(call_context_at(&text, cursor).is_none());
    }

    #[test]
    fn returns_none_for_subquery_and_in_list() {
        let (in_sql, in_cur) = pipe("WHERE x IN (1, |)");
        assert!(call_context_at(&in_sql, in_cur).is_none());
        let (sub_sql, sub_cur) = pipe("FROM (SELECT |");
        assert!(call_context_at(&sub_sql, sub_cur).is_none());
    }

    #[test]
    fn resolves_dotted_callee() {
        let c = ctx("SELECT dbo.fn_total(|)");
        assert_eq!(c.callee, "dbo.fn_total");
    }

    #[test]
    fn nested_call_reports_innermost() {
        let c = ctx("ROUND(SUM(|), 2)");
        assert_eq!(c.callee, "SUM");
        assert_eq!(c.active_parameter, 0);
    }

    #[test]
    fn closed_call_before_cursor_is_not_active() {
        let (text, cursor) = pipe("GETDATE() + |");
        assert!(call_context_at(&text, cursor).is_none());
    }

    #[test]
    fn variadic_binds_extra_arguments_to_the_repeating_param() {
        let h = help("COALESCE(a, b, c, |)", &[]);
        assert!(h.label.contains('…'), "{}", h.label);
        assert_eq!(h.active_parameter, Some(h.parameters.len() - 1));
    }

    #[test]
    fn too_many_args_for_fixed_arity_yields_none_active() {
        let h = help("COUNT(a, |)", &[]);
        assert_eq!(h.active_parameter, None);
        assert_eq!(h.kind, RoutineKind::Function);
    }

    #[test]
    fn builtin_help_carries_summary_and_param_docs() {
        let h = help("SELECT COUNT(|", &[]);
        assert!(h.summary.as_deref().is_some_and(|s| !s.is_empty()));
        assert_eq!(h.parameters.len(), h.parameter_docs.len());
        assert!(h.parameter_docs.iter().all(|d| !d.is_empty()));
        let (start, end) = h.parameter_ranges[0];
        assert_eq!(&h.label[start..end], h.parameters[0]);
    }

    #[test]
    fn qualified_calls_are_never_builtins() {
        let (text, cursor) = pipe("SELECT dbo.COUNT(|");
        assert!(signature_help_at(&text, cursor, &[]).is_none());
    }

    #[test]
    fn live_routine_signature_wins_over_builtin() {
        let routines = [RoutineSignature {
            schema: "dbo".into(),
            name: "FORMAT".into(),
            parameters: vec![HoverParameter {
                name: "@Value".into(),
                data_type: "int".into(),
                is_output: false,
            }],
        }];
        let h = help("SELECT dbo.FORMAT(|)", &routines);
        assert!(h.label.contains("@Value int"), "{}", h.label);
        assert_eq!(h.kind, RoutineKind::Procedure);
        assert_eq!(h.parameters, vec!["@Value int".to_string()]);
    }

    #[test]
    fn slot_words_ride_along_with_the_active_argument() {
        let h = help("SELECT DATEADD(|", &[]);
        assert_eq!(h.active_parameter, Some(0));
        assert!(
            h.active_values
                .iter()
                .any(|v| v.eq_ignore_ascii_case("day"))
        );
        let h = help("SELECT DATEADD(day, |", &[]);
        assert!(h.active_values.is_empty());
    }

    #[test]
    fn the_overload_that_fits_the_argument_is_chosen() {
        // A function whose forms differ in arity: the chosen form must accept
        // the argument the cursor is in.
        let catalog = builtins(Dialect::Sqlite);
        let log = catalog.function("log").expect("sqlite log");
        assert!(log.signatures.len() >= 2);
        let (text, cursor) = pipe("SELECT log(2, |");
        let h = signature_help_for(Dialect::Sqlite, &text, cursor, &[]).unwrap();
        assert_eq!(h.overload_count, log.signatures.len());
        assert!(log.signatures[h.overload_index].accepts_arg(1));
        assert_eq!(h.active_parameter, Some(1));
    }

    #[test]
    fn dialects_resolve_their_own_builtins() {
        let (text, cursor) = pipe("SELECT NVL(a, |");
        assert!(signature_help_for(Dialect::Oracle, &text, cursor, &[]).is_some());
        let (text, cursor) = pipe("SELECT date_trunc(|");
        assert!(signature_help_for(Dialect::Postgres, &text, cursor, &[]).is_some());
    }
}
