//! Pure SQL-aware scanner for the `-- sqlly name {n}: {text}` result-set name
//! comment grammar.
//!
//! Port of `SQLLYExports/ResultSetNameParser.swift`. A line comment of that
//! form, anywhere in the query (outside strings, bracket identifiers, and
//! block comments), names the 1-based result set `{n}` with `{text}`. Ranges
//! are absolute over the original SQL so the editor can select the name text
//! without re-normalizing line endings.
//!
//! Detection is deliberately conservative about SQL lexical state so a `--`
//! inside a string literal (e.g. `'a -- sqlly name 1: x'`) is never mistaken for a
//! name comment. None of these helpers throw or perform I/O.

/// One `-- sqlly name {n}: {text}` assignment parsed from a query, with absolute
/// ranges over the original (un-normalized) SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSetNameAssignment {
    /// 0-based result-set index targeted by the comment (`{n}` minus one).
    pub result_set_index: usize,
    /// The trimmed name text (`{text}`).
    pub name: String,
    /// Absolute byte range of the whole `-- sqlly name {n}: {text}` comment.
    pub comment_range: std::ops::Range<usize>,
    /// Absolute byte range of just the trimmed `{text}` portion.
    pub name_range: std::ops::Range<usize>,
}

/// Parses every `-- sqlly name {n}: {text}` assignment in document order. When the
/// same result-set index is targeted more than once, all assignments are
/// returned; `resolved_names` applies last-wins.
pub fn parse(sql: &str) -> Vec<ResultSetNameAssignment> {
    // Lock-block directives count; blanking keeps every offset valid.
    let unwrapped = crate::sqlly_directive::unwrap_lock_blocks(sql);
    let sql = unwrapped.as_ref();
    let bytes = sql.as_bytes();
    let count = bytes.len();
    let mut assignments: Vec<ResultSetNameAssignment> = Vec::new();
    let mut state = ScannerState::default();
    let mut index = 0usize;

    while index < count {
        let ch = bytes[index];
        let next = if index + 1 < count {
            Some(bytes[index + 1])
        } else {
            None
        };

        if state.in_block_comment {
            if ch == b'*' && next == Some(b'/') {
                state.in_block_comment = false;
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        if state.in_string {
            if ch == b'\'' && next == Some(b'\'') {
                index += 2;
            } else if ch == b'\'' {
                state.in_string = false;
                index += 1;
            } else {
                index += 1;
            }
            continue;
        }
        if state.in_bracket {
            if ch == b']' {
                state.in_bracket = false;
            }
            index += 1;
            continue;
        }
        if ch == b'\'' {
            state.in_string = true;
            index += 1;
            continue;
        }
        if ch == b'[' {
            state.in_bracket = true;
            index += 1;
            continue;
        }
        if ch == b'/' && next == Some(b'*') {
            state.in_block_comment = true;
            index += 2;
            continue;
        }
        if ch == b'-' && next == Some(b'-') {
            let comment_start = index;
            let body_start = index + 2;
            let line_end = line_comment_end(bytes, body_start);
            // SAFETY: body_start..line_end are within a valid UTF-8 boundary
            // because we scan byte-by-byte over ASCII line terminators and the
            // body can only start after `--` (both ASCII). The body extends to
            // the next `\n`/`\r` or end of input, and UTF-8 continuation bytes
            // never collide with these ASCII values.
            let body = &sql[body_start..line_end];
            if let Some(parsed) = parse_name_body(body) {
                assignments.push(ResultSetNameAssignment {
                    result_set_index: parsed.result_set_index,
                    name: parsed.name,
                    comment_range: comment_start..line_end,
                    name_range: (body_start + parsed.name_start)..(body_start + parsed.name_end),
                });
            }
            index = line_end;
            continue;
        }
        index += 1;
    }
    assignments
}

/// Reduces a list of assignments to a 0-based index → name map. When the same
/// index is named more than once, the last assignment in document order wins.
pub fn resolved_names(
    assignments: &[ResultSetNameAssignment],
) -> std::collections::BTreeMap<usize, String> {
    let mut map = std::collections::BTreeMap::new();
    for a in assignments {
        map.insert(a.result_set_index, a.name.clone());
    }
    map
}

/// Update or insert a `-- sqlly name {n}: {text}` comment for the given 0-based
/// result set index. If an existing assignment targets this index, the last
/// one in document order has its name text replaced in-place. If no
/// assignment exists, a new comment line is inserted at the top of the SQL.
/// Returns the modified SQL string.
// The app writes names through `result_header_actions::set_name_edit` (one
// undoable edit that respects the run region); kept for its tests' coverage
// of the name grammar round trip.
#[cfg_attr(not(test), allow(dead_code))]
pub fn update_or_insert_name_comment(sql: &str, result_set_index: usize, name: &str) -> String {
    let assignments = parse(sql);
    let target = result_set_index + 1;
    if let Some(last) = assignments
        .iter()
        .rev()
        .find(|a| a.result_set_index == result_set_index)
    {
        let mut result = String::with_capacity(sql.len() + name.len());
        result.push_str(&sql[..last.name_range.start]);
        result.push_str(name);
        result.push_str(&sql[last.name_range.end..]);
        result
    } else {
        let comment = format!("-- sqlly name {}: {}\n", target, name);
        let mut result = String::with_capacity(comment.len() + sql.len());
        result.push_str(&comment);
        result.push_str(sql);
        result
    }
}

#[allow(dead_code)]
/// Returns a human-readable error if two distinct result sets share the same
/// non-empty name, otherwise `None`. Deterministic (iterates result sets in
/// ascending index order) so the reported indices are stable.
pub fn duplicate_name_error(assignments: &[ResultSetNameAssignment]) -> Option<String> {
    let resolved = resolved_names(assignments);
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (index, name) in &resolved {
        if name.is_empty() {
            continue;
        }
        if let Some(first) = seen.get(name) {
            return Some(crate::localization::format(
                "results.result_names.duplicate_name_error",
                &[
                    ("result_name", name),
                    ("first_result_number", &(first + 1).to_string()),
                    ("second_result_number", &(index + 1).to_string()),
                ],
            ));
        }
        seen.insert(name.clone(), *index);
    }
    None
}

#[derive(Default)]
struct ScannerState {
    in_block_comment: bool,
    in_string: bool,
    in_bracket: bool,
}

/// Offset of the first line terminator at or after `start`, or `bytes.len()`.
/// Matches `\n`, `\r`, or `\r\n`.
fn line_comment_end(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\n' || c == b'\r' {
            break;
        }
        i += 1;
    }
    i
}

struct ParsedName {
    result_set_index: usize,
    name: String,
    name_start: usize,
    name_end: usize,
}

/// Matches `\s*sqlly\s+name\s+(\d+)\s*:\s*(.*?)\s*$` against `body` — the
/// shared lexer's name grammar. Offsets come back relative to `body`.
fn parse_name_body(body: &str) -> Option<ParsedName> {
    let crate::sqlly_directive::Directive::Name {
        number,
        name,
        name_start,
        name_end,
    } = crate::sqlly_directive::classify_body(body)?
    else {
        return None;
    };
    Some(ParsedName {
        result_set_index: number - 1,
        name: name.to_string(),
        name_start,
        name_end,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sql_has_no_assignments() {
        assert!(parse("").is_empty());
        assert!(parse("SELECT 1").is_empty());
    }

    #[test]
    fn parses_simple_name_comment() {
        let sql = "-- sqlly name 1: Orders\nSELECT * FROM Orders";
        let a = parse(sql);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].result_set_index, 0);
        assert_eq!(a[0].name, "Orders");
    }

    #[test]
    fn parses_second_result_set() {
        let sql = "SELECT 1\n-- sqlly name 2: Customers\nSELECT 2";
        let a = parse(sql);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].result_set_index, 1);
        assert_eq!(a[0].name, "Customers");
    }

    #[test]
    fn name_keyword_is_case_insensitive() {
        let sql = "-- SQLLY NAME 1: X\nSELECT 1";
        let a = parse(sql);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "X");
    }

    #[test]
    fn ignores_unnamespaced_name_comments() {
        assert!(parse("-- name 1: not a directive\nSELECT 1").is_empty());
    }

    #[test]
    fn ignores_dash_dash_inside_string() {
        let sql = "SELECT '-- sqlly name 1: fake'\n-- sqlly name 1: real\nSELECT 1";
        let a = parse(sql);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "real");
    }

    #[test]
    fn ignores_dash_dash_inside_bracket_identifier() {
        let sql = "SELECT [-- sqlly name 1: fake]\n-- sqlly name 1: real\nSELECT 1";
        let a = parse(sql);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "real");
    }

    #[test]
    fn ignores_dash_dash_inside_block_comment() {
        let sql = "/* -- sqlly name 1: fake */\n-- sqlly name 1: real\nSELECT 1";
        let a = parse(sql);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "real");
    }

    #[test]
    fn trims_surrounding_whitespace_in_name() {
        let sql = "-- sqlly name 1:   spaced name   \nSELECT 1";
        let a = parse(sql);
        assert_eq!(a[0].name, "spaced name");
    }

    #[test]
    fn comment_range_covers_whole_comment() {
        let sql = "-- sqlly name 1: Orders\nSELECT 1";
        let a = parse(sql);
        let range = a[0].comment_range.clone();
        assert_eq!(&sql[range], "-- sqlly name 1: Orders");
    }

    #[test]
    fn name_range_covers_only_name_text() {
        let sql = "-- sqlly name 1: Orders\nSELECT 1";
        let a = parse(sql);
        let range = a[0].name_range.clone();
        assert_eq!(&sql[range], "Orders");
    }

    #[test]
    fn resolved_names_last_wins() {
        let sql = "-- sqlly name 1: First\n-- sqlly name 1: Second\nSELECT 1";
        let a = parse(sql);
        let resolved = resolved_names(&a);
        assert_eq!(resolved.get(&0), Some(&"Second".to_string()));
    }

    #[test]
    fn resolved_names_multiple_sets() {
        let sql = "-- sqlly name 1: A\nSELECT 1\n-- sqlly name 2: B\nSELECT 2";
        let a = parse(sql);
        let resolved = resolved_names(&a);
        assert_eq!(resolved.get(&0), Some(&"A".to_string()));
        assert_eq!(resolved.get(&1), Some(&"B".to_string()));
    }

    #[test]
    fn duplicate_name_error_when_shared() {
        let sql = "-- sqlly name 1: X\nSELECT 1\n-- sqlly name 2: X\nSELECT 2";
        let a = parse(sql);
        let err = duplicate_name_error(&a).expect("should have error");
        assert!(err.contains("X"));
        assert!(err.contains("result set 1"));
        assert!(err.contains("result set 2"));
    }

    #[test]
    fn no_duplicate_error_when_unique() {
        let sql = "-- sqlly name 1: A\nSELECT 1\n-- sqlly name 2: B\nSELECT 2";
        let a = parse(sql);
        assert!(duplicate_name_error(&a).is_none());
    }

    #[test]
    fn no_duplicate_error_for_empty_names() {
        let sql = "-- sqlly name 1: \nSELECT 1";
        let a = parse(sql);
        assert!(a.is_empty());
    }

    #[test]
    fn rejects_zero_and_negative_indices() {
        assert!(parse("-- sqlly name 0: zero\nSELECT 1").is_empty());
    }

    #[test]
    fn rejects_missing_colon() {
        assert!(parse("-- sqlly name 1 Orders\nSELECT 1").is_empty());
    }

    #[test]
    fn rejects_missing_number() {
        assert!(parse("-- sqlly name: Orders\nSELECT 1").is_empty());
    }

    #[test]
    fn handles_crlf_line_endings() {
        let sql = "-- sqlly name 1: Orders\r\nSELECT 1";
        let a = parse(sql);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "Orders");
    }

    #[test]
    fn handles_cr_only_line_endings() {
        let sql = "-- sqlly name 1: Orders\rSELECT 1";
        let a = parse(sql);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "Orders");
    }

    #[test]
    fn multiple_assignments_preserve_document_order() {
        let sql = "-- sqlly name 1: First\nSELECT 1\n-- sqlly name 2: Second\nSELECT 2\n-- sqlly name 1: Override\nSELECT 3";
        let a = parse(sql);
        assert_eq!(a.len(), 3);
        assert_eq!(a[0].name, "First");
        assert_eq!(a[1].name, "Second");
        assert_eq!(a[2].name, "Override");
    }

    #[test]
    fn update_or_insert_inserts_new_comment_when_absent() {
        let sql = "SELECT 1\nSELECT 2";
        let result = update_or_insert_name_comment(sql, 0, "Orders");
        assert!(result.starts_with("-- sqlly name 1: Orders\n"));
        assert!(result.contains("SELECT 1\nSELECT 2"));
        let reparsed = parse(&result);
        assert_eq!(reparsed.len(), 1);
        assert_eq!(reparsed[0].result_set_index, 0);
        assert_eq!(reparsed[0].name, "Orders");
    }

    #[test]
    fn update_or_insert_replaces_existing_name() {
        let sql = "-- sqlly name 1: Old\nSELECT 1";
        let result = update_or_insert_name_comment(sql, 0, "New");
        assert!(result.contains("New"));
        assert!(!result.contains("Old"));
        let reparsed = parse(&result);
        assert_eq!(reparsed.len(), 1);
        assert_eq!(reparsed[0].name, "New");
    }

    #[test]
    fn update_or_insert_updates_last_assignment_for_index() {
        let sql = "-- sqlly name 1: First\n-- sqlly name 1: Second\nSELECT 1";
        let result = update_or_insert_name_comment(sql, 0, "Third");
        let reparsed = parse(&result);
        let last = reparsed
            .iter()
            .rev()
            .find(|a| a.result_set_index == 0)
            .unwrap();
        assert_eq!(last.name, "Third");
    }

    #[test]
    fn update_or_insert_preserves_other_comments() {
        let sql = "-- sqlly name 1: A\nSELECT 1\n-- sqlly name 2: B\nSELECT 2";
        let result = update_or_insert_name_comment(sql, 1, "Updated");
        assert!(result.contains("-- sqlly name 1: A"));
        let reparsed = parse(&result);
        assert_eq!(
            reparsed
                .iter()
                .find(|a| a.result_set_index == 0)
                .unwrap()
                .name,
            "A"
        );
        assert_eq!(
            reparsed
                .iter()
                .find(|a| a.result_set_index == 1)
                .unwrap()
                .name,
            "Updated"
        );
    }

    #[test]
    fn update_or_insert_second_result_set() {
        let sql = "SELECT 1\nSELECT 2";
        let result = update_or_insert_name_comment(sql, 1, "Customers");
        assert!(result.starts_with("-- sqlly name 2: Customers\n"));
        let reparsed = parse(&result);
        assert_eq!(reparsed[0].result_set_index, 1);
        assert_eq!(reparsed[0].name, "Customers");
    }
}
