//! Result-layout and result-pivot hint completions.
//!
//! Ports `ResultLayoutHintCompletion.swift` and `ResultPivotHintCompletion.swift`
//! into shared Rust so both GPUI and Swift get the same comment-hint IntelliSense.
//!
//! When the cursor sits inside a recognized `--` hint comment, [`complete_hints`]
//! returns `Some(items)` (possibly empty) so the SQL completion engine is
//! bypassed — comments are not SQL. When the cursor is anywhere else it returns
//! `None` and the normal pipeline proceeds unchanged.

use sqlly_intel::intellisense::{CompletionItem, CompletionKind, CompletionSource};

/// Base score so hint items sort above nothing in particular but below strong
/// prefix matches from the core completion engine.
const BASE_SCORE: i32 = 50;

/// Largest canvas dimension the layout parser accepts (matches Swift's
/// `QueryResultLayoutSpec.maxCanvasSize`).
const MAX_CANVAS_SIZE: usize = 8;

/// The aggregate names accepted by the pivot `value:<index>:<aggregate>` grammar.
const AGGREGATES: &[&str] = &["count", "sum", "first", "max", "min"];

/// Entry point for both layout and pivot hint completions.
///
/// Returns `None` when the cursor is not inside a recognized hint comment
/// (normal SQL completion should proceed). Returns `Some(vec)` when the cursor
/// IS inside a hint comment — the vec may be empty, but SQL completion is still
/// suppressed because comments are not SQL.
///
/// `result_columns` is the list of column names from the most recent result set
/// (used by pivot index candidates). Pass `&[]` when not available.
pub fn complete_hints(
    sql: &str,
    cursor: usize,
    result_columns: &[String],
) -> Option<Vec<CompletionItem>> {
    let caret = cursor.min(sql.len());

    // Find the line containing the caret.
    let mut line_start = caret;
    let bytes = sql.as_bytes();
    while line_start > 0 && bytes[line_start - 1] != b'\n' {
        line_start -= 1;
    }
    let text_before_caret = &sql[line_start..caret];

    // Must be a `--` comment line (leading whitespace allowed).
    let left_trimmed = trim_leading_whitespace(text_before_caret);
    if !left_trimmed.starts_with("--") {
        return None;
    }
    let after_dashes = &left_trimmed[2..];
    let body = trim_leading_whitespace(after_dashes);
    // The directives live in the `sqlly` namespace (`-- sqlly result: 1|1`);
    // the hint grammars read the body after it.
    let (body, in_namespace) = strip_namespace(body);
    let body_lower = body.to_ascii_lowercase();

    // Try layout hints first, then pivot hints. They are mutually exclusive
    // (pivot bodies start with `pivot:`/`view:`, layout bodies start with
    // digits or `result:`).
    if let Some(items) = layout_hints(
        &body_lower,
        body,
        text_before_caret,
        line_start,
        sql,
        in_namespace,
    ) {
        return Some(items);
    }
    if let Some(items) = pivot_hints(&body_lower, body, text_before_caret, result_columns) {
        return Some(items);
    }

    None
}

// ---------------------------------------------------------------------------
// Layout hints (canvas tag + placement comments)
// ---------------------------------------------------------------------------

fn layout_hints(
    body_lower: &str,
    body: &str,
    text_before_caret: &str,
    line_start: usize,
    sql: &str,
    in_namespace: bool,
) -> Option<Vec<CompletionItem>> {
    // The trailing token being typed (replaced on accept).
    let tail = trailing_hint_token(text_before_caret);

    // Placement body: `result:` already typed → offer coordinate guidance.
    if body_lower.starts_with("result:") {
        let spec = &body["result:".len()..];
        let coordinate_tail = trailing_hint_token(spec);
        let candidates = placement_candidates(spec);
        return Some(rank(candidates, &coordinate_tail));
    }

    // Partially-typed `result:` placement starter.
    if !body.is_empty() && "result:".starts_with(body_lower) {
        let candidates = vec![Candidate {
            label: "result:".to_string(),
            insert: "result: 1|1".to_string(),
            kind: CompletionKind::Keyword,
            detail: "place next result at row|column (1-based)".to_string(),
        }];
        return Some(rank(candidates, &tail));
    }

    // `layout:` already typed → the layouts.
    if body_lower.starts_with("layout:") {
        let value_tail = trailing_identifier(&body["layout:".len()..]);
        return Some(rank(layout_value_candidates(), &value_tail));
    }
    // `lines:` / `lines <n>:` already typed → row counts.
    if body_lower.starts_with("lines") && body_lower.contains(':') {
        let colon = body.find(':').unwrap_or(0);
        let value_tail = trailing_identifier(&body[colon + 1..]);
        return Some(rank(lines_value_candidates(), &value_tail));
    }

    // Partially-typed `layout:` / `lines:` starters.
    let mut starters = Vec::new();
    if !body.is_empty() && "layout:".starts_with(body_lower) {
        starters.push(Candidate {
            label: "layout:".to_string(),
            insert: "layout: stacked".to_string(),
            kind: CompletionKind::Keyword,
            detail: "result layout for this run: stacked, tabbed, or canvas".to_string(),
        });
    }
    if !body.is_empty() && "lines:".starts_with(body_lower) {
        starters.push(Candidate {
            label: "lines:".to_string(),
            insert: "lines: 10".to_string(),
            kind: CompletionKind::Keyword,
            detail: "rows allotted to each stacked result card (lines <n>: for one)".to_string(),
        });
    }
    if !starters.is_empty() {
        return Some(rank(starters, &tail));
    }

    // Canvas tag is only meaningful on the first non-empty line.
    if is_first_non_empty_line(sql, line_start)
        && (body.is_empty() || is_canvas_tag_in_progress(body_lower))
    {
        let mut candidates = canvas_candidates();
        if body.is_empty() && in_namespace {
            candidates.extend(directive_starter_candidates());
        }
        return Some(rank(candidates, &tail));
    }
    // `-- sqlly ▏` anywhere else: the directives that can open here.
    if body.is_empty() && in_namespace {
        return Some(rank(directive_starter_candidates(), &tail));
    }

    None
}

/// The directives a `-- sqlly ` comment can open with, offered on an empty
/// body.
fn directive_starter_candidates() -> Vec<Candidate> {
    vec![
        Candidate {
            label: "layout:".to_string(),
            insert: "layout: stacked".to_string(),
            kind: CompletionKind::Keyword,
            detail: "result layout for this run: stacked, tabbed, or canvas".to_string(),
        },
        Candidate {
            label: "lines:".to_string(),
            insert: "lines: 10".to_string(),
            kind: CompletionKind::Keyword,
            detail: "rows allotted to each stacked result card (lines <n>: for one)".to_string(),
        },
        Candidate {
            label: "result:".to_string(),
            insert: "result: 1|1".to_string(),
            kind: CompletionKind::Keyword,
            detail: "place next result at row|column (1-based)".to_string(),
        },
    ]
}

fn layout_value_candidates() -> Vec<Candidate> {
    [
        (
            "stacked",
            "every result set as a resizable card in one column",
        ),
        ("tabbed", "one tab per result set"),
        (
            "canvas",
            "cells placed by -- sqlly <rows>x<columns> and result:",
        ),
    ]
    .into_iter()
    .map(|(label, detail)| Candidate {
        label: label.to_string(),
        insert: label.to_string(),
        kind: CompletionKind::Keyword,
        detail: detail.to_string(),
    })
    .collect()
}

fn lines_value_candidates() -> Vec<Candidate> {
    [5usize, 10, 15, 20, 30, 50]
        .into_iter()
        .map(|rows| Candidate {
            label: rows.to_string(),
            insert: rows.to_string(),
            kind: CompletionKind::Keyword,
            detail: format!("{rows} data rows per stacked card"),
        })
        .collect()
}

/// Strip a leading `sqlly` namespace (any case) and the whitespace after it,
/// reporting whether it was there. `sqllything` is not the namespace.
fn strip_namespace(body: &str) -> (&str, bool) {
    let Some(head) = body.get(..5) else {
        return (body, false);
    };
    if !head.eq_ignore_ascii_case("sqlly") {
        return (body, false);
    }
    let rest = &body[5..];
    if rest.is_empty() {
        return (rest, true);
    }
    if !rest.starts_with(|c: char| c.is_ascii_whitespace()) {
        return (body, false);
    }
    (trim_leading_whitespace(rest), true)
}

/// Canvas-tag suggestions: square grids from 2×2 up to `MAX_CANVAS_SIZE`.
fn canvas_candidates() -> Vec<Candidate> {
    (2..=MAX_CANVAS_SIZE)
        .map(|size| Candidate {
            label: format!("{size}x{size}"),
            insert: format!("{size}x{size}"),
            kind: CompletionKind::Keyword,
            detail: format!("canvas: {size} rows x {size} columns (first line only)"),
        })
        .collect()
}

/// Placement-body suggestions for `-- result: <spec>`.
fn placement_candidates(spec_before_caret: &str) -> Vec<Candidate> {
    let trimmed = spec_before_caret.trim();

    if trimmed.is_empty() {
        return vec![
            Candidate {
                label: "1|1".to_string(),
                insert: "1|1".to_string(),
                kind: CompletionKind::Value,
                detail: "row 1, column 1 (top-left)".to_string(),
            },
            Candidate {
                label: "1|2".to_string(),
                insert: "1|2".to_string(),
                kind: CompletionKind::Value,
                detail: "row 1, column 2".to_string(),
            },
            Candidate {
                label: "2|1".to_string(),
                insert: "2|1".to_string(),
                kind: CompletionKind::Value,
                detail: "row 2, column 1".to_string(),
            },
            Candidate {
                label: "1-2|1".to_string(),
                insert: "1-2|1".to_string(),
                kind: CompletionKind::Value,
                detail: "span rows 1-2 in column 1".to_string(),
            },
            Candidate {
                label: "1|1-2".to_string(),
                insert: "1|1-2".to_string(),
                kind: CompletionKind::Value,
                detail: "span columns 1-2 in row 1".to_string(),
            },
        ];
    }

    // Row typed, awaiting the column separator.
    if !trimmed.contains('|') {
        return vec![Candidate {
            label: format!("{trimmed}|1"),
            insert: format!("{trimmed}|1"),
            kind: CompletionKind::Value,
            detail: "add |column (use 1-2 for a span)".to_string(),
        }];
    }

    // Past the separator — nothing deterministic to suggest.
    Vec::new()
}

// ---------------------------------------------------------------------------
// Pivot hints (view:pivot and pivot: field grammar)
// ---------------------------------------------------------------------------

fn pivot_hints(
    body_lower: &str,
    body: &str,
    text_before_caret: &str,
    result_columns: &[String],
) -> Option<Vec<CompletionItem>> {
    let tail = trailing_identifier(text_before_caret);

    let candidates: Vec<Candidate> = if body_lower.starts_with("pivot:") {
        let spec = &body["pivot:".len()..];
        pivot_candidates(spec, result_columns)
    } else if body_lower.starts_with("view:") {
        vec![Candidate {
            label: "pivot".to_string(),
            insert: "pivot".to_string(),
            kind: CompletionKind::Keyword,
            detail: "pivot view".to_string(),
        }]
    } else if !body.is_empty() && "pivot:".starts_with(body_lower) {
        vec![Candidate {
            label: "pivot:".to_string(),
            insert: "pivot:".to_string(),
            kind: CompletionKind::Keyword,
            detail: "pivot hint".to_string(),
        }]
    } else if !body.is_empty() && "view:pivot".starts_with(body_lower) {
        vec![Candidate {
            label: "view:pivot".to_string(),
            insert: "view:pivot".to_string(),
            kind: CompletionKind::Keyword,
            detail: "default view".to_string(),
        }]
    } else {
        return None;
    };

    Some(rank(candidates, &tail))
}

/// Candidates for a `-- pivot:` body given the text already typed.
fn pivot_candidates(spec_before_caret: &str, result_columns: &[String]) -> Vec<Candidate> {
    // The portion of the current field after the last comma.
    let last_segment = spec_before_caret
        .rsplit(',')
        .next()
        .unwrap_or("")
        .to_string();
    let colon_count = last_segment.matches(':').count();

    if colon_count == 0 {
        // Between fields: offer field names not yet used.
        return field_candidates(spec_before_caret);
    }

    let key_part = last_segment
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    if key_part == "value" {
        // `value:<index>:<aggregate>` — after the second colon offer aggregates,
        // after the first offer column indexes.
        if colon_count >= 2 {
            return aggregate_candidates();
        }
        return index_candidates(result_columns);
    }

    // `column:` / `row:` — offer column indexes.
    index_candidates(result_columns)
}

fn field_candidates(existing: &str) -> Vec<Candidate> {
    let used_lower = existing.to_ascii_lowercase();
    let fields = [
        ("column", "pivot column field"),
        ("row", "pivot row field"),
        ("value", "pivot value field"),
    ];
    fields
        .iter()
        .filter(|(name, _)| !used_lower.contains(&format!("{name}:")))
        .map(|(name, detail)| Candidate {
            label: name.to_string(),
            insert: format!("{name}:"),
            kind: CompletionKind::Keyword,
            detail: detail.to_string(),
        })
        .collect()
}

fn index_candidates(result_columns: &[String]) -> Vec<Candidate> {
    result_columns
        .iter()
        .enumerate()
        .map(|(idx, name)| Candidate {
            label: idx.to_string(),
            insert: idx.to_string(),
            kind: CompletionKind::Column,
            detail: name.clone(),
        })
        .collect()
}

fn aggregate_candidates() -> Vec<Candidate> {
    AGGREGATES
        .iter()
        .map(|agg| Candidate {
            label: agg.to_string(),
            insert: agg.to_string(),
            kind: CompletionKind::Keyword,
            detail: "aggregate".to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Shared candidate + ranking
// ---------------------------------------------------------------------------

struct Candidate {
    label: String,
    insert: String,
    kind: CompletionKind,
    detail: String,
}

/// Rank candidates by a simple prefix-match score against the tail token.
/// This mirrors the Swift `CompletionMatcher.match` behavior for the small
/// number of hint candidates — no need for sophisticated fuzzy matching.
fn rank(candidates: Vec<Candidate>, tail: &str) -> Vec<CompletionItem> {
    let tail_lower = tail.to_ascii_lowercase();
    let mut items: Vec<CompletionItem> = candidates
        .into_iter()
        .filter_map(|c| {
            let score = if tail.is_empty() {
                BASE_SCORE
            } else {
                let label_lower = c.label.to_ascii_lowercase();
                if label_lower.starts_with(&tail_lower) {
                    BASE_SCORE + 50
                } else if label_lower.contains(&tail_lower) {
                    BASE_SCORE + 20
                } else {
                    return None;
                }
            };
            Some(CompletionItem {
                label: c.label,
                insert_text: c.insert,
                kind: c.kind,
                source: CompletionSource::Snippet,
                score,
                detail: c.detail,
                replace_span: None,
            })
        })
        .collect();
    items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
    items
}

// ---------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------

fn trim_leading_whitespace(s: &str) -> &str {
    s.trim_start_matches([' ', '\t'])
}

/// True when the line starting at `line_start` is the first non-empty line
/// of the document — the only place a canvas tag is honored.
fn is_first_non_empty_line(sql: &str, line_start: usize) -> bool {
    let bytes = sql.as_bytes();
    for &byte in &bytes[..line_start] {
        if byte != b' ' && byte != b'\t' && byte != b'\n' && byte != b'\r' {
            return false;
        }
    }
    true
}

/// True when the comment body is a canvas-tag-in-progress: a digit, optionally
/// followed by `x` and more digits (`2`, `2x`, `2x2`).
fn is_canvas_tag_in_progress(body_lower: &str) -> bool {
    let bytes = body_lower.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() {
        return false;
    }
    bytes.iter().all(|&b| b.is_ascii_digit() || b == b'x')
}

/// The trailing token to replace on accept for layout hints: captures digits,
/// `x`, `|`, `-`, `:`, and letters for the `result:` keyword.
fn trailing_hint_token(s: &str) -> String {
    let mut result = String::new();
    for ch in s.chars().rev() {
        if ch.is_alphanumeric() || ch == 'x' || ch == '|' || ch == '-' || ch == ':' {
            result.push(ch);
        } else {
            break;
        }
    }
    result.chars().rev().collect()
}

/// The trailing token to replace on accept for pivot hints: trailing run of
/// `[A-Za-z0-9]`.
fn trailing_identifier(s: &str) -> String {
    let mut result = String::new();
    for ch in s.chars().rev() {
        if ch.is_alphanumeric() {
            result.push(ch);
        } else {
            break;
        }
    }
    result.chars().rev().collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Not-in-comment tests ----

    #[test]
    fn returns_none_outside_comment() {
        assert!(complete_hints("SELECT * FROM Orders", 10, &[]).is_none());
    }

    #[test]
    fn returns_none_in_plain_comment() {
        // A prose comment should not trigger hint completion.
        assert!(complete_hints("-- this is a note\nSELECT 1", 10, &[]).is_none());
    }

    // ---- Layout: canvas tag ----

    #[test]
    fn canvas_tag_on_first_line() {
        let items = complete_hints("-- ", 3, &[]).unwrap();
        assert!(!items.is_empty());
        assert!(items.iter().any(|i| i.label == "2x2"));
        assert!(items.iter().any(|i| i.label == "8x8"));
    }

    #[test]
    fn canvas_tag_in_progress_2x() {
        let items = complete_hints("-- 2x", 5, &[]).unwrap();
        assert!(items.iter().any(|i| i.label == "2x2"));
        // Should NOT include 3x3 etc since they don't match "2x" prefix
        assert!(!items.iter().any(|i| i.label == "3x3"));
    }

    #[test]
    fn layout_and_lines_directives_complete_in_the_namespace() {
        let sql = "SELECT 1\n-- sqlly ";
        let items = complete_hints(sql, sql.len(), &[]).unwrap();
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"layout:") && labels.contains(&"lines:"),
            "{labels:?}"
        );

        let sql = "SELECT 1\n-- sqlly lay";
        let items = complete_hints(sql, sql.len(), &[]).unwrap();
        assert_eq!(items[0].label, "layout:");
        assert_eq!(items[0].insert_text, "layout: stacked");

        let sql = "-- sqlly layout: st";
        let items = complete_hints(sql, sql.len(), &[]).unwrap();
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["stacked"]);

        let sql = "-- SQLLY lines 2: ";
        let items = complete_hints(sql, sql.len(), &[]).unwrap();
        assert!(items.iter().any(|i| i.label == "10"), "{items:?}");

        // The namespace form of the existing placement grammar works too.
        let sql = "-- sqlly 2x2\n-- sqlly result: ";
        assert!(complete_hints(sql, sql.len(), &[]).is_some());
    }

    #[test]
    fn canvas_tag_not_on_second_line() {
        // Canvas tag only on first non-empty line.
        let sql = "SELECT 1\n-- ";
        assert!(complete_hints(sql, sql.len(), &[]).is_none());
    }

    #[test]
    fn canvas_tag_with_leading_whitespace() {
        let items = complete_hints("  -- ", 5, &[]).unwrap();
        assert!(items.iter().any(|i| i.label == "2x2"));
    }

    // ---- Layout: placement comments ----

    #[test]
    fn placement_result_starter() {
        let items = complete_hints("-- resu", 7, &[]).unwrap();
        assert!(items.iter().any(|i| i.label == "result:"));
    }

    #[test]
    fn placement_result_colon_empty() {
        let items = complete_hints("-- result: ", 11, &[]).unwrap();
        assert!(items.iter().any(|i| i.label == "1|1"));
        assert!(items.iter().any(|i| i.label == "1|2"));
        assert!(items.iter().any(|i| i.label == "2|1"));
    }

    #[test]
    fn placement_result_row_typed_awaiting_separator() {
        let items = complete_hints("-- result: 2", 12, &[]).unwrap();
        assert!(items.iter().any(|i| i.label == "2|1"));
    }

    #[test]
    fn placement_result_past_separator_empty() {
        let items = complete_hints("-- result: 1|", 13, &[]).unwrap();
        assert!(items.is_empty());
    }

    // ---- Pivot: view:pivot ----

    #[test]
    fn pivot_view_pivot_starter() {
        // `-- view` (no colon yet) triggers the "view:pivot" starter candidate.
        let items = complete_hints("-- view", 7, &[]).unwrap();
        assert!(items.iter().any(|i| i.label == "view:pivot"));
    }

    #[test]
    fn pivot_view_colon_offers_pivot() {
        // `-- view:` (with colon) triggers viewValueCandidates offering "pivot".
        let items = complete_hints("-- view:", 8, &[]).unwrap();
        assert!(items.iter().any(|i| i.label == "pivot"));
    }

    // ---- Pivot: pivot: field grammar ----

    #[test]
    fn pivot_starter() {
        let items = complete_hints("-- piv", 6, &[]).unwrap();
        assert!(items.iter().any(|i| i.label == "pivot:"));
    }

    #[test]
    fn pivot_field_candidates_empty_spec() {
        let items = complete_hints("-- pivot: ", 10, &[]).unwrap();
        assert!(items.iter().any(|i| i.label == "column"));
        assert!(items.iter().any(|i| i.label == "row"));
        assert!(items.iter().any(|i| i.label == "value"));
    }

    #[test]
    fn pivot_field_candidates_exclude_used() {
        let items = complete_hints("-- pivot: column:0,", 20, &[]).unwrap();
        // column: is already used, should only see row and value
        assert!(!items.iter().any(|i| i.label == "column"));
        assert!(items.iter().any(|i| i.label == "row"));
        assert!(items.iter().any(|i| i.label == "value"));
    }

    #[test]
    fn pivot_index_candidates_for_column() {
        let cols = vec!["Id".to_string(), "Name".to_string(), "Age".to_string()];
        let items = complete_hints("-- pivot: column:", 17, &cols).unwrap();
        assert!(items.iter().any(|i| i.label == "0" && i.detail == "Id"));
        assert!(items.iter().any(|i| i.label == "1" && i.detail == "Name"));
        assert!(items.iter().any(|i| i.label == "2" && i.detail == "Age"));
    }

    #[test]
    fn pivot_index_candidates_for_row() {
        let cols = vec!["Id".to_string(), "Name".to_string()];
        let items = complete_hints("-- pivot: row:", 14, &cols).unwrap();
        assert!(items.iter().any(|i| i.label == "0"));
        assert!(items.iter().any(|i| i.label == "1"));
    }

    #[test]
    fn pivot_aggregate_candidates_after_value_index_colon() {
        let cols = vec!["Id".to_string()];
        // `value:0:` — colon_count >= 2 → aggregates
        let items = complete_hints("-- pivot: value:0:", 18, &cols).unwrap();
        assert!(items.iter().any(|i| i.label == "count"));
        assert!(items.iter().any(|i| i.label == "sum"));
        assert!(items.iter().any(|i| i.label == "first"));
        assert!(items.iter().any(|i| i.label == "max"));
        assert!(items.iter().any(|i| i.label == "min"));
    }

    #[test]
    fn pivot_index_candidates_for_value_first_colon() {
        let cols = vec!["Id".to_string(), "Name".to_string()];
        // `value:` — colon_count == 1 → index candidates
        let items = complete_hints("-- pivot: value:", 16, &cols).unwrap();
        assert!(items.iter().any(|i| i.label == "0" && i.detail == "Id"));
        assert!(items.iter().any(|i| i.label == "1" && i.detail == "Name"));
    }

    // ---- Suppression: hint context suppresses SQL completion ----

    #[test]
    fn empty_hint_context_still_returns_some() {
        // Past the separator in a placement comment — no candidates, but
        // still Some([]) so SQL completion is suppressed.
        let result = complete_hints("-- result: 1|", 13, &[]);
        assert!(matches!(result, Some(ref v) if v.is_empty()));
    }

    // ---- Text helpers ----

    #[test]
    fn test_trailing_hint_token() {
        assert_eq!(trailing_hint_token("-- 2x"), "2x");
        assert_eq!(trailing_hint_token("-- result: 1|"), "1|");
        assert_eq!(trailing_hint_token("-- resu"), "resu");
        assert_eq!(trailing_hint_token("-- "), "");
    }

    #[test]
    fn test_trailing_identifier() {
        assert_eq!(trailing_identifier("-- pivot: col"), "col");
        assert_eq!(trailing_identifier("-- pivot: column:0,"), "");
        assert_eq!(trailing_identifier("-- view:pi"), "pi");
    }

    #[test]
    fn test_is_canvas_tag_in_progress() {
        assert!(is_canvas_tag_in_progress("2"));
        assert!(is_canvas_tag_in_progress("2x"));
        assert!(is_canvas_tag_in_progress("2x2"));
        assert!(is_canvas_tag_in_progress("2x2x"));
        assert!(!is_canvas_tag_in_progress(""));
        assert!(!is_canvas_tag_in_progress("abc"));
    }

    #[test]
    fn test_is_first_non_empty_line() {
        assert!(is_first_non_empty_line("-- 2x2\nSELECT 1", 0));
        assert!(is_first_non_empty_line("  -- 2x2", 2));
        assert!(is_first_non_empty_line("\n\n-- 2x2", 2));
        assert!(!is_first_non_empty_line("SELECT 1\n-- 2x2", 9));
    }
}
