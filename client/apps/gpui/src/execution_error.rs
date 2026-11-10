//! Turns an engine error message into a span inside the SQL the user ran.
//! Every engine reports positions relative to the text it received; the run
//! pump knows which slice of the buffer that was (`RunRegion`) and whether the
//! rollback policy prepended a preamble, so this module can map back to
//! buffer byte offsets. Pure: no gpui, no I/O.

use sqlly_core::{ConnectionKind, DatabaseType};

/// The slice of the editor buffer that a run submitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunRegion {
    /// Byte offset of the submitted text inside the editor buffer.
    pub start: usize,
    /// Editor `content_revision` at submit time; a later revision means the
    /// buffer changed and any location is stale.
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorLocation {
    /// Byte range inside the *run text* (not the buffer).
    pub start: usize,
    pub end: usize,
    /// 0-based line inside the run text.
    pub line: usize,
}

pub struct LocateInput<'a> {
    pub db_type: DatabaseType,
    /// `ConnectionProfile::auto_wrap_rollback` for the connection that ran.
    pub auto_wrap_rollback: bool,
    /// Exactly the text handed to `Services`/the driver (post param substitution).
    pub run_sql: &'a str,
    pub message: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RawPosition {
    Line(usize),
    CharIndex(usize),
    ByteOffset {
        tail: String,
        offset: usize,
    },
    NearToken {
        token: String,
        line_hint: Option<usize>,
    },
}

pub fn locate(input: &LocateInput<'_>) -> Option<ErrorLocation> {
    let raw = match input.db_type.connection_kind() {
        ConnectionKind::Tds => parse_tds(input.message)?,
        ConnectionKind::PostgresWire => parse_pg(input.message)?,
        ConnectionKind::MySqlWire => parse_mysql(input.message)?,
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => parse_sqlite(input.message)?,
        _ => return None,
    };
    let raw = adjust_preamble(raw, input);
    resolve(raw, input)
}

fn parse_tds(message: &str) -> Option<RawPosition> {
    let first = message.lines().next()?;
    if !first.starts_with("Msg ") {
        return None;
    }
    let at = first.rfind("Line ")?;
    let n: usize = first[at + "Line ".len()..].trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    let rest = message.split_once('\n').map(|(_, body)| body).unwrap_or("");
    if let Some(token) = invalid_column_name(rest).or_else(|| extract_near_token(rest)) {
        return Some(RawPosition::NearToken {
            token,
            line_hint: Some(n),
        });
    }
    Some(RawPosition::Line(n))
}

fn parse_pg(message: &str) -> Option<RawPosition> {
    for line in message.lines() {
        if let Some(rest) = line.strip_prefix("POSITION: ") {
            let n: usize = rest.trim().parse().ok()?;
            return Some(RawPosition::CharIndex(n));
        }
    }
    None
}

fn parse_mysql(message: &str) -> Option<RawPosition> {
    let marker = " at line ";
    let at = message.rfind(marker)?;
    let n: usize = message[at + marker.len()..]
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    if n == 0 {
        return None;
    }
    let before = &message[..at];
    if let Some(token) = extract_near_token(before) {
        Some(RawPosition::NearToken {
            token,
            line_hint: Some(n),
        })
    } else {
        Some(RawPosition::Line(n))
    }
}

fn parse_sqlite(message: &str) -> Option<RawPosition> {
    if let Some(at) = message.rfind(" at offset ") {
        let offset: usize = message[at + " at offset ".len()..].trim().parse().ok()?;
        if let Some(in_at) = message[..at].rfind(" in ") {
            let tail = message[in_at + " in ".len()..at].to_string();
            return Some(RawPosition::ByteOffset { tail, offset });
        }
    }
    if let Some(token) = extract_near_token(message) {
        return Some(RawPosition::NearToken {
            token,
            line_hint: None,
        });
    }
    if let Some(rest) = message.split("no such column:").nth(1) {
        let token = rest.trim().trim_end_matches('.').trim();
        if !token.is_empty() {
            return Some(RawPosition::NearToken {
                token: token.to_string(),
                line_hint: None,
            });
        }
    }
    None
}

fn adjust_preamble(raw: RawPosition, input: &LocateInput<'_>) -> RawPosition {
    if !input.auto_wrap_rollback {
        return raw;
    }
    match input.db_type.connection_kind() {
        ConnectionKind::Tds => raw,
        ConnectionKind::PostgresWire => match raw {
            RawPosition::CharIndex(n) => {
                let preamble = "BEGIN;\n".chars().count();
                let leading =
                    input.run_sql.chars().count() - input.run_sql.trim_start().chars().count();
                let idx_0 = (n as i64) - 1 - (preamble as i64) + (leading as i64);
                RawPosition::CharIndex((idx_0.max(0) as usize).saturating_add(1))
            }
            other => other,
        },
        ConnectionKind::MySqlWire => {
            let preamble_lines = 1i64;
            let leading_ws = input.run_sql.len() - input.run_sql.trim_start().len();
            let leading_blank = input.run_sql[..leading_ws].matches('\n').count() as i64;
            let adj = |n: usize| ((n as i64) - preamble_lines + leading_blank).max(1) as usize;
            match raw {
                RawPosition::Line(n) => RawPosition::Line(adj(n)),
                RawPosition::NearToken {
                    token,
                    line_hint: Some(n),
                } => RawPosition::NearToken {
                    token,
                    line_hint: Some(adj(n)),
                },
                other => other,
            }
        }
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => match raw {
            RawPosition::ByteOffset { tail, offset } => {
                let stripped = tail
                    .strip_suffix("\nROLLBACK;")
                    .unwrap_or(&tail)
                    .to_string();
                RawPosition::ByteOffset {
                    tail: stripped,
                    offset,
                }
            }
            other => other,
        },
        _ => raw,
    }
}

fn resolve(raw: RawPosition, input: &LocateInput<'_>) -> Option<ErrorLocation> {
    let sql = input.run_sql;
    let (start, end) = match raw {
        RawPosition::Line(n) => line_span(sql, n)?,
        RawPosition::CharIndex(n) => {
            let idx_0 = n.saturating_sub(1);
            let start = char_index_to_byte(sql, idx_0)?;
            let end = token_end(sql, start);
            (start, end)
        }
        RawPosition::ByteOffset { tail, offset } => {
            let tail_start = sql.rfind(&tail)?;
            let start = tail_start.saturating_add(offset);
            let end = token_end(sql, start);
            (start, end)
        }
        RawPosition::NearToken { token, line_hint } => find_identifier(sql, &token, line_hint)?,
    };
    let (start, end) = clamp_span(sql, start, end)?;
    let line = sql
        .get(..start)
        .map(|prefix| prefix.matches('\n').count())?;
    Some(ErrorLocation { start, end, line })
}

fn line_span(sql: &str, line_1based: usize) -> Option<(usize, usize)> {
    if line_1based == 0 {
        return None;
    }
    let mut idx = 0usize;
    let mut current = 1usize;
    while current < line_1based {
        let rel = sql[idx..].find('\n')?;
        idx += rel + 1;
        current += 1;
    }
    if idx > sql.len() {
        return None;
    }
    let line_end = sql[idx..].find('\n').map(|i| idx + i).unwrap_or(sql.len());
    let line = &sql[idx..line_end];
    if line.trim().is_empty() {
        if line_end >= sql.len() {
            return None;
        }
        return line_span(sql, line_1based + 1);
    }
    let lead = line.len() - line.trim_start().len();
    let trimmed_len = line.trim().len();
    Some((idx + lead, idx + lead + trimmed_len))
}

fn char_index_to_byte(sql: &str, char_idx_0: usize) -> Option<usize> {
    match sql.char_indices().nth(char_idx_0) {
        Some((byte, _)) => Some(byte),
        None if char_idx_0 == sql.chars().count() => Some(sql.len()),
        None => None,
    }
}

fn token_end(sql: &str, start: usize) -> usize {
    if start >= sql.len() {
        return sql.len();
    }
    let start = floor_char_boundary(sql, start);
    let bytes = sql.as_bytes();
    if start < bytes.len() && is_identifier_byte(bytes[start]) {
        let mut end = start + 1;
        while end < bytes.len() && is_identifier_byte(bytes[end]) {
            end += 1;
        }
        end
    } else {
        sql[start..]
            .chars()
            .next()
            .map(|c| start + c.len_utf8())
            .unwrap_or(start)
    }
}

fn find_identifier(sql: &str, token: &str, line_hint: Option<usize>) -> Option<(usize, usize)> {
    if token.is_empty() {
        return None;
    }
    if let Some(line_1based) = line_hint {
        let line_idx = line_1based.saturating_sub(1);
        let start = nth_line_start(sql, line_idx);
        let end = sql[start..]
            .find('\n')
            .map(|i| start + i)
            .unwrap_or(sql.len());
        if let Some(found) = scan_identifier(&sql[start..end], start, token) {
            return Some(found);
        }
    }
    scan_identifier(sql, 0, token)
}

fn nth_line_start(sql: &str, line_0based: usize) -> usize {
    let mut idx = 0usize;
    let mut current = 0usize;
    while current < line_0based {
        match sql[idx..].find('\n') {
            Some(rel) => {
                idx += rel + 1;
                current += 1;
            }
            None => return sql.len(),
        }
    }
    idx
}

fn scan_identifier(slice: &str, base: usize, token: &str) -> Option<(usize, usize)> {
    let bytes = slice.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_identifier_byte(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_identifier_byte(bytes[i]) {
                i += 1;
            }
            if slice[start..i].eq_ignore_ascii_case(token) {
                return Some((base + start, base + i));
            }
        } else {
            i += 1;
        }
    }
    None
}

fn clamp_span(sql: &str, start: usize, end: usize) -> Option<(usize, usize)> {
    if sql.is_empty() {
        return None;
    }
    let start = floor_char_boundary(sql, start.min(sql.len()));
    let end = ceil_char_boundary(sql, end.min(sql.len())).max(start);
    if start >= sql.len() && end >= sql.len() {
        return None;
    }
    Some((start, end))
}

fn floor_char_boundary(sql: &str, mut offset: usize) -> usize {
    offset = offset.min(sql.len());
    while offset > 0 && !sql.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn ceil_char_boundary(sql: &str, mut offset: usize) -> usize {
    offset = offset.min(sql.len());
    while offset < sql.len() && !sql.is_char_boundary(offset) {
        offset += 1;
    }
    offset
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$' | b'#')
}

fn invalid_column_name(s: &str) -> Option<String> {
    let prefix = "Invalid column name '";
    let rest = s.split_once(prefix)?.1;
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

fn extract_near_token(s: &str) -> Option<String> {
    for (marker, close) in [("near '", '\''), ("near \"", '"')] {
        if let Some(i) = s.rfind(marker) {
            let rest = &s[i + marker.len()..];
            if let Some(end) = rest.find(close) {
                let frag = &rest[..end];
                if let Some(ident) = first_identifier(frag) {
                    return Some(ident);
                }
                if !frag.is_empty() {
                    return Some(frag.to_string());
                }
            }
        }
    }
    None
}

fn first_identifier(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_identifier_byte(bytes[i]) {
            let start = i;
            i += 1;
            while i < bytes.len() && is_identifier_byte(bytes[i]) {
                i += 1;
            }
            return Some(s[start..i].to_string());
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locate_for(
        db_type: DatabaseType,
        run_sql: &str,
        message: &str,
        auto_wrap_rollback: bool,
    ) -> Option<ErrorLocation> {
        locate(&LocateInput {
            db_type,
            auto_wrap_rollback,
            run_sql,
            message,
        })
    }

    fn span<'a>(sql: &'a str, loc: &ErrorLocation) -> &'a str {
        &sql[loc.start..loc.end]
    }

    #[test]
    fn tds_invalid_column_underlines_token() {
        let run_sql = "SELECT Id,\n  Nmae\nFROM T";
        let loc = locate_for(
            DatabaseType::SqlServer,
            run_sql,
            "Msg 207, Level 16, State 1, Line 2\nInvalid column name 'Nmae'.",
            false,
        )
        .unwrap();
        assert_eq!(span(run_sql, &loc), "Nmae");
        assert_eq!(loc.line, 1);
    }

    #[test]
    fn tds_line_zero_and_unrelated_are_none() {
        assert!(
            locate_for(
                DatabaseType::SqlServer,
                "SELECT 1",
                "Msg 207, Level 16, State 1, Line 0\nInvalid column name 'X'.",
                false,
            )
            .is_none()
        );
        assert!(
            locate_for(
                DatabaseType::SqlServer,
                "SELECT 1",
                "Connection reset",
                false,
            )
            .is_none()
        );
        assert!(
            locate_for(
                DatabaseType::SqlServer,
                "SELECT 1",
                "something without Msg prefix\nLine 2",
                false,
            )
            .is_none()
        );
    }

    #[test]
    fn pg_position_underlines_token() {
        let run_sql = "SELECT 1\n  FORM t";
        let loc = locate_for(
            DatabaseType::Postgres,
            run_sql,
            "ERROR: syntax error at or near \"FORM\"\nPOSITION: 12",
            false,
        )
        .unwrap();
        assert_eq!(loc.start, 11);
        assert_eq!(loc.end, 15);
        assert_eq!(span(run_sql, &loc), "FORM");
    }

    #[test]
    fn pg_position_undoes_rollback_preamble() {
        let run_sql = "SELECT 1\n  FORM t";
        let loc = locate_for(
            DatabaseType::Postgres,
            run_sql,
            "ERROR: syntax error at or near \"FORM\"\nPOSITION: 19",
            true,
        )
        .unwrap();
        assert_eq!(span(run_sql, &loc), "FORM");
        assert_eq!(loc.start, 11);
        assert_eq!(loc.end, 15);
    }

    #[test]
    fn pg_without_position_is_none() {
        assert!(
            locate_for(
                DatabaseType::Postgres,
                "SELECT 1",
                "ERROR: syntax error at or near \"FORM\"",
                false,
            )
            .is_none()
        );
        assert!(
            locate_for(
                DatabaseType::Postgres,
                "SELECT 1",
                "Connection reset",
                false,
            )
            .is_none()
        );
    }

    #[test]
    fn pg_multibyte_char_index_maps_to_bytes() {
        let run_sql = "SELECT é\n  FORM t";
        let form_byte = run_sql.find("FORM").unwrap();
        let loc = locate_for(
            DatabaseType::Postgres,
            run_sql,
            "ERROR: syntax error at or near \"FORM\"\nPOSITION: 12",
            false,
        )
        .unwrap();
        assert_eq!(loc.start, form_byte);
        assert_eq!(span(run_sql, &loc), "FORM");
    }

    #[test]
    fn mysql_near_token_at_line() {
        let run_sql = "SELECT 1\n  FORM t";
        let loc = locate_for(
            DatabaseType::MySql,
            run_sql,
            "ERROR 1064 (42000): You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'FORM t' at line 2",
            false,
        )
        .unwrap();
        assert_eq!(span(run_sql, &loc), "FORM");
        assert_eq!(loc.line, 1);
    }

    #[test]
    fn mysql_wrap_shifts_line_back() {
        let run_sql = "SELECT 1\n  FORM t";
        let loc = locate_for(
            DatabaseType::MySql,
            run_sql,
            "ERROR 1064 (42000): You have an error in your SQL syntax; check the manual near 'FORM t' at line 3",
            true,
        )
        .unwrap();
        assert_eq!(span(run_sql, &loc), "FORM");
        assert_eq!(loc.line, 1);
    }

    #[test]
    fn mysql_unrelated_is_none() {
        assert!(locate_for(DatabaseType::MySql, "SELECT 1", "Connection reset", false,).is_none());
    }

    #[test]
    fn sqlite_offset_underlines_token() {
        let run_sql = "SELECT 1\n  FORM t";
        let loc = locate_for(
            DatabaseType::Sqlite,
            run_sql,
            "near \"FORM\": syntax error in SELECT 1\n  FORM t at offset 11",
            false,
        )
        .unwrap();
        assert_eq!(span(run_sql, &loc), "FORM");
    }

    #[test]
    fn sqlite_no_such_column_searches_token() {
        let run_sql = "SELECT Id,\n  Nmae\nFROM T";
        let loc = locate_for(DatabaseType::Sqlite, run_sql, "no such column: Nmae", false).unwrap();
        assert_eq!(span(run_sql, &loc), "Nmae");
    }

    #[test]
    fn sqlite_unrelated_is_none() {
        assert!(locate_for(DatabaseType::Sqlite, "SELECT 1", "Connection reset", false,).is_none());
    }
}
