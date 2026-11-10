//! Statement bounding for completion.
//!
//! An editor buffer holds many statements, most of them unterminated while
//! they are being written. Every context decision — which clause the caret
//! is in, which table sources are in scope, whether a word after a FROM
//! source is its alias — must look only at the statement the caret is in; a
//! previous statement's tables, aliases and clauses must not leak into a new
//! one (feedback ifb_3798: `SELECT * FROM t AS x⏎⏎update ▏` offered the
//! SELECT's post-FROM keywords instead of UPDATE targets).
//!
//! The boundaries come from [`split_statements`], the same splitter the
//! parser and the execution path use, so the rules live in one place: `;`
//! terminators, `GO` batch separators, and a statement keyword that opens a
//! new statement inside an unterminated one (with its false-positive guards
//! for literals, quoted names, member access, `FOR UPDATE`, trigger event
//! lists, MERGE actions, join hints, …).

use std::ops::Range;

use sqlly_lang::sqllex::split_statements;
use sqlly_lang::sqlmodel::Dialect;

use super::complete::scan::{ScanTokenKind, scan_tokens};

/// The byte range of the statement that contains `cursor`: from the start of
/// the last statement beginning at or before the cursor (or the start of the
/// text) to the start of the next statement (or the end of the text). The
/// range therefore includes the whitespace and any terminator after the
/// statement's last token, so a caret resting after `update ` is inside the
/// `update` statement.
///
/// Inside a routine body, where the splitter deliberately keeps the whole
/// definition together, a `;` before the caret still starts the caret's
/// statement and one after it ends it.
pub fn statement_range(sql: &str, cursor: usize, dialect: Dialect) -> Range<usize> {
    let cursor = cursor.min(sql.len());
    let region = statement_region(sql, cursor, dialect);
    let (mut start, mut end) = (region.start, region.end);
    // Terminators the splitter left inside the region (routine bodies).
    let base = start;
    for token in scan_tokens(&sql[base..end]) {
        if token.kind != ScanTokenKind::Semicolon {
            continue;
        }
        let at = base + token.byte_offset;
        if at < cursor {
            start = at + 1;
        } else {
            end = at;
            break;
        }
    }
    start..end
}

/// The splitter's own region containing `cursor`: like [`statement_range`]
/// but without the inner-`;` refinement, so a routine definition (which the
/// splitter keeps whole, body and all) is one region. This is the unit the
/// parser produces one statement node for.
pub fn statement_region(sql: &str, cursor: usize, dialect: Dialect) -> Range<usize> {
    let cursor = cursor.min(sql.len());
    let mut start = 0usize;
    let mut end = sql.len();
    for slice in split_statements(sql, dialect) {
        if slice.span.start <= cursor {
            start = slice.span.start;
        } else {
            end = slice.span.start;
            break;
        }
    }
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(sql_with_marker: &str) -> Range<usize> {
        let cursor = sql_with_marker.find('▏').expect("marker");
        let sql = sql_with_marker.replacen('▏', "", 1);
        statement_range(&sql, cursor, Dialect::SqlServer)
    }

    fn text(sql_with_marker: &str) -> String {
        let r = range(sql_with_marker);
        sql_with_marker.replacen('▏', "", 1)[r].to_string()
    }

    #[test]
    fn a_single_statement_is_the_whole_text() {
        assert_eq!(text("SELECT * FROM t WHERE ▏"), "SELECT * FROM t WHERE ");
        assert_eq!(text("▏"), "");
    }

    #[test]
    fn the_reported_update_after_a_finished_select_is_its_own_statement() {
        let sql = "SELECT\n\t*\nFROM\n\tAccounting.ReportingPeriod AS arp\n\t\nupdate ▏";
        assert_eq!(text(sql), "update ");
    }

    #[test]
    fn semicolons_and_go_bound_statements() {
        assert_eq!(text("SELECT 1; SELECT ▏c"), "SELECT c");
        assert_eq!(text("SELECT ▏1; SELECT c"), "SELECT 1");
        assert_eq!(text("SELECT 1\nGO\nUPDATE ▏"), "UPDATE ");
    }

    #[test]
    fn the_caret_before_the_first_statement_sees_nothing_after_it() {
        assert_eq!(text("  ▏\nSELECT 1"), "  \n");
    }

    #[test]
    fn a_subquery_select_does_not_start_a_statement() {
        let sql = "SELECT * FROM t WHERE id IN (SELECT id FROM u WHERE ▏)";
        assert_eq!(text(sql), sql.replacen('▏', "", 1));
    }

    #[test]
    fn routine_bodies_still_bound_on_inner_semicolons() {
        let sql = "CREATE PROCEDURE p AS BEGIN SELECT * FROM a; UPDATE ▏; SELECT 2 END";
        assert_eq!(text(sql), " UPDATE ");
        // The region, though, is the whole routine.
        let cursor = sql.find('▏').unwrap();
        let plain = sql.replacen('▏', "", 1);
        assert_eq!(
            statement_region(&plain, cursor, Dialect::SqlServer),
            0..plain.len()
        );
    }

    #[test]
    fn an_unclosed_paren_in_the_previous_statement_does_not_swallow_the_next() {
        assert_eq!(text("SELECT COUNT( FROM t\n\nupdate ▏"), "update ");
    }

    #[test]
    fn keyword_lookalikes_stay_in_the_statement() {
        let sql = "SELECT 'update', [delete], t.insert, UpdatedBy FROM Audit.UpdateLog t WHERE ▏";
        assert_eq!(text(sql), sql.replacen('▏', "", 1));
    }
}
