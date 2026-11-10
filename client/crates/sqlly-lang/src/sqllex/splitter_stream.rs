//! Incremental, chunk-fed statement splitting for streaming script execution.

use std::collections::VecDeque;

use crate::sqlmodel::Dialect;

/// One executable unit from a script: a `;`-terminated statement, or for
/// SQL Server a whole `GO` batch (which may contain many `;` statements).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptUnit {
    pub text: String,
    pub start_line: u32,
    pub end_line: u32,
    pub ordinal: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Code,
    SingleQuote,
    DoubleQuote,
    Bracket,
    Backtick,
    LineComment,
    BlockComment { depth: u32 },
    DollarQuote { tag: String },
}

pub struct StreamSplitter {
    dialect: Dialect,
    state: State,
    delimiter: String,
    pending: String,
    unit_start_line: u32,
    current_line: u32,
    ordinal: usize,
    saw_significant_code: bool,
    finished: bool,
    pause: bool,
    /// Characters read from the stream but not yet handed to the state machine.
    hold: VecDeque<char>,
}

impl StreamSplitter {
    pub fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            state: State::Code,
            delimiter: ";".to_string(),
            pending: String::new(),
            unit_start_line: 1,
            current_line: 1,
            ordinal: 0,
            saw_significant_code: false,
            finished: false,
            pause: false,
            hold: VecDeque::new(),
        }
    }

    pub fn push(&mut self, chunk: &str, out: &mut Vec<ScriptUnit>) {
        if self.finished {
            return;
        }
        self.hold.extend(chunk.chars());
        self.pause = false;
        while !self.hold.is_empty() {
            if self.defer_for_boundary() {
                break;
            }
            let ch = self.hold.pop_front().unwrap();
            self.consume_char(ch, out);
            if self.pause {
                break;
            }
        }
    }

    /// Wait for the next chunk when a multi-byte token might be split across boundaries.
    fn defer_for_boundary(&self) -> bool {
        if self.finished {
            return false;
        }
        let front = self.hold.front().copied();
        match self.state {
            State::Code => {
                if front == Some('/') && self.hold.len() == 1 {
                    return true;
                }
                if front == Some('-') && self.hold.len() == 1 {
                    return true;
                }
                if front == Some('\r') && self.hold.len() == 1 {
                    return true;
                }
                if self.dialect == Dialect::Postgres
                    && front == Some('$')
                    && !dollar_open_tag_complete(&self.hold)
                {
                    return true;
                }
                false
            }
            State::BlockComment { .. } => front == Some('*') && self.hold.len() == 1,
            State::SingleQuote => {
                self.dialect == Dialect::MySql && front == Some('\\') && self.hold.len() == 1
            }
            _ => false,
        }
    }

    pub fn finish(&mut self, out: &mut Vec<ScriptUnit>) -> Result<(), String> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        match self.state {
            State::Code => {
                self.strip_final_go_line();
                if self.on_delimiter_directive_line() {
                    self.strip_trailing_delimiter_line();
                }
                self.emit_unit(out);
                Ok(())
            }
            State::SingleQuote => Err("unterminated single-quoted string".into()),
            State::DoubleQuote => Err("unterminated double-quoted string".into()),
            State::Bracket => Err("unterminated bracket identifier".into()),
            State::Backtick => Err("unterminated backtick identifier".into()),
            State::LineComment => {
                self.state = State::Code;
                self.strip_final_go_line();
                self.emit_unit(out);
                Ok(())
            }
            State::BlockComment { .. } => Err("unterminated block comment".into()),
            State::DollarQuote { .. } => Err("unterminated dollar-quoted string".into()),
        }
    }

    fn consume_char(&mut self, ch: char, out: &mut Vec<ScriptUnit>) {
        if ch == '\r' {
            if self.hold.front() == Some(&'\n') {
                self.hold.pop_front();
            } else if self.hold.is_empty() {
                self.hold.push_front('\r');
                self.pause = true;
                return;
            }
            self.on_newline(out);
            return;
        }
        if ch == '\n' {
            self.on_newline(out);
            return;
        }

        match self.state.clone() {
            State::Code => self.code_char(ch, out),
            State::SingleQuote => self.single_quote_char(ch),
            State::DoubleQuote => self.double_quote_char(ch),
            State::Bracket => self.bracket_char(ch),
            State::Backtick => self.backtick_char(ch),
            State::LineComment => self.pending.push(ch),
            State::BlockComment { depth } => self.block_comment_char(ch, depth),
            State::DollarQuote { tag } => self.dollar_quote_char(ch, &tag),
        }
    }

    fn on_newline(&mut self, out: &mut Vec<ScriptUnit>) {
        if self.dialect == Dialect::MySql
            && let Some(line) = self.current_line_text()
            && let Some(new_delim) = parse_delimiter_directive(&line)
        {
            self.strip_trailing_delimiter_line();
            self.emit_unit(out);
            self.delimiter = new_delim;
            self.current_line += 1;
            return;
        }
        if self.dialect == Dialect::SqlServer
            && let Some(line) = self.current_line_text()
            && is_go_separator_line(&line)
        {
            self.state = State::Code;
            self.strip_trailing_go_line();
            self.emit_unit(out);
            self.current_line += 1;
            return;
        }
        if matches!(self.state, State::LineComment) {
            self.state = State::Code;
        }
        self.note_pending_char('\n');
        self.current_line += 1;
    }

    /// MySQL: the current line is a `DELIMITER xx` directive (it may contain
    /// the active delimiter, as mysqldump's `DELIMITER ;;` does), which the
    /// newline handler applies instead of cutting a statement at it.
    fn on_delimiter_directive_line(&self) -> bool {
        self.dialect == Dialect::MySql
            && self
                .current_line_text()
                .is_some_and(|line| parse_delimiter_directive(&line).is_some())
    }

    fn current_line_text(&self) -> Option<String> {
        Some(self.pending.rsplit('\n').next()?.to_string())
    }

    /// A last `GO` with no newline after it still ends the batch; left in,
    /// the server would read it as a column alias of the last statement.
    fn strip_final_go_line(&mut self) {
        if self.dialect == Dialect::SqlServer
            && self
                .current_line_text()
                .is_some_and(|line| is_go_separator_line(&line))
        {
            self.strip_trailing_go_line();
        }
    }

    fn strip_trailing_delimiter_line(&mut self) {
        self.strip_trailing_go_line();
    }

    fn strip_trailing_go_line(&mut self) {
        if let Some(pos) = self.pending.rfind('\n') {
            self.pending.truncate(pos + 1);
        } else {
            self.pending.clear();
        }
        while self.pending.ends_with('\n') {
            self.pending.pop();
        }
    }

    fn code_char(&mut self, ch: char, out: &mut Vec<ScriptUnit>) {
        if ch == '-' && self.hold.front() == Some(&'-') {
            self.hold.pop_front();
            self.note_pending_str("--");
            self.state = State::LineComment;
            return;
        }
        if ch == '#' && self.dialect == Dialect::MySql {
            self.note_pending_char(ch);
            self.state = State::LineComment;
            return;
        }
        if ch == '/' && self.hold.front() == Some(&'*') {
            self.hold.pop_front();
            self.note_pending_str("/*");
            self.state = State::BlockComment { depth: 1 };
            return;
        }
        if ch == '\'' {
            self.note_pending_char(ch);
            self.state = State::SingleQuote;
            return;
        }
        if ch == '"' {
            self.note_pending_char(ch);
            self.state = State::DoubleQuote;
            return;
        }
        if ch == '[' && self.dialect == Dialect::SqlServer {
            self.note_pending_char(ch);
            self.state = State::Bracket;
            return;
        }
        if ch == '`' && matches!(self.dialect, Dialect::MySql | Dialect::Sqlite) {
            self.note_pending_char(ch);
            self.state = State::Backtick;
            return;
        }
        if ch == '$' && self.dialect == Dialect::Postgres {
            if let Some(tag) = self.read_dollar_tag_from_hold() {
                self.note_pending_str(&tag);
                self.state = State::DollarQuote { tag };
            }
            return;
        }

        if !ch.is_whitespace() {
            if !self.saw_significant_code
                && self.pending.chars().all(|c| c.is_whitespace() || c == '\n')
            {
                self.unit_start_line = self.current_line;
            }
            self.saw_significant_code = true;
        }
        self.note_pending_char(ch);
        if self.dialect != Dialect::SqlServer
            && self.pending.ends_with(&self.delimiter)
            && !self.on_delimiter_directive_line()
        {
            self.pending
                .truncate(self.pending.len() - self.delimiter.len());
            self.emit_unit(out);
        }
    }

    fn read_dollar_tag_from_hold(&mut self) -> Option<String> {
        let mut tag = String::from("$");
        if self.hold.front() == Some(&'$') {
            tag.push('$');
            self.hold.pop_front();
            return Some(tag);
        }
        while let Some(&ch) = self.hold.front() {
            if ch == '$' {
                tag.push('$');
                self.hold.pop_front();
                return Some(tag);
            }
            if ch.is_ascii_alphanumeric() || ch == '_' {
                tag.push(ch);
                self.hold.pop_front();
            } else {
                return None;
            }
        }
        None
    }

    fn single_quote_char(&mut self, ch: char) {
        self.note_pending_char(ch);
        if ch == '\'' {
            if self.hold.front() == Some(&'\'') {
                self.hold.pop_front();
                self.pending.push('\'');
            } else {
                self.state = State::Code;
            }
        } else if ch == '\\'
            && self.dialect == Dialect::MySql
            && let Some(next) = self.hold.pop_front()
        {
            self.pending.push(next);
        }
    }

    fn double_quote_char(&mut self, ch: char) {
        self.note_pending_char(ch);
        if ch == '"' && !self.pending.ends_with("\"\"") {
            self.state = State::Code;
        }
    }

    fn bracket_char(&mut self, ch: char) {
        self.note_pending_char(ch);
        if ch == ']' && !self.pending.ends_with("]]") {
            self.state = State::Code;
        }
    }

    fn backtick_char(&mut self, ch: char) {
        self.note_pending_char(ch);
        if ch == '`' {
            self.state = State::Code;
        }
    }

    fn block_comment_char(&mut self, ch: char, depth: u32) {
        self.note_pending_char(ch);
        if ch == '*' && self.hold.front() == Some(&'/') {
            self.hold.pop_front();
            self.pending.push('/');
            let new_depth = depth - 1;
            self.state = if new_depth == 0 {
                State::Code
            } else {
                State::BlockComment { depth: new_depth }
            };
        } else if ch == '/' && self.hold.front() == Some(&'*') && self.dialect == Dialect::Postgres
        {
            self.hold.pop_front();
            self.pending.push('*');
            self.state = State::BlockComment { depth: depth + 1 };
        }
    }

    fn dollar_quote_char(&mut self, ch: char, tag: &str) {
        self.note_pending_char(ch);
        if self.pending.ends_with(tag) {
            self.state = State::Code;
        }
    }

    /// The unit starts on the line of its first non-whitespace character,
    /// which may be a comment: comments above a statement travel with it.
    fn note_pending_char(&mut self, ch: char) {
        if !ch.is_whitespace() && self.pending.chars().all(char::is_whitespace) {
            self.unit_start_line = self.current_line;
        }
        self.pending.push(ch);
    }

    fn note_pending_str(&mut self, s: &str) {
        if self.pending.chars().all(char::is_whitespace) {
            self.unit_start_line = self.current_line;
        }
        self.pending.push_str(s);
    }

    /// Hand the pending text on as one unit. Comments above the statement
    /// stay in it (SQL Server stores a view's or procedure's source from the
    /// start of its batch, header comments included); a unit holding nothing
    /// but comments is dropped.
    fn emit_unit(&mut self, out: &mut Vec<ScriptUnit>) {
        let text = self.pending.trim().to_string();
        let start_line = self.unit_start_line;
        let end_line = self.current_line.max(1);
        self.pending.clear();
        self.unit_start_line = self.current_line;
        self.saw_significant_code = false;

        if text.is_empty() || strip_leading_comments_in(self.dialect, &text).is_empty() {
            return;
        }

        out.push(ScriptUnit {
            text,
            start_line,
            end_line,
            ordinal: self.ordinal,
        });
        self.ordinal += 1;
    }
}

fn dollar_open_tag_complete(hold: &VecDeque<char>) -> bool {
    let mut iter = hold.iter().skip(1);
    match iter.next() {
        None => false,
        Some(&'$') => true,
        Some(&c) if c.is_ascii_alphanumeric() || c == '_' => {
            for &ch in iter {
                if ch == '$' {
                    return true;
                }
                if !ch.is_ascii_alphanumeric() && ch != '_' {
                    return false;
                }
            }
            false
        }
        _ => false,
    }
}

fn parse_delimiter_directive(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if trimmed.len() >= 9
        && trimmed.is_char_boundary(9)
        && trimmed[..9].eq_ignore_ascii_case("delimiter")
        && trimmed[9..].chars().next().is_none_or(char::is_whitespace)
    {
        let new_delim = trimmed[9..].trim();
        Some(if new_delim.is_empty() {
            ";".to_string()
        } else {
            new_delim.to_string()
        })
    } else {
        None
    }
}

fn is_go_separator_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.eq_ignore_ascii_case("go") {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("go ") && lower[2..].trim_start().starts_with("--")
}

/// `text` without the whitespace, `--` line comments, and `/* */` block
/// comments in front of its first statement token. For callers that look at
/// how a statement begins (an `INSERT INTO … VALUES` prefix, a leading
/// keyword) now that units keep the comments written above them. Dialect
/// neutral: a MySQL `#` comment is code here; see
/// [`strip_leading_comments_in`].
pub fn strip_leading_comments(text: &str) -> &str {
    strip_leading_comments_in(Dialect::Postgres, text)
}

/// [`strip_leading_comments`] for one dialect: on MySQL/MariaDB a `#` line
/// comment is skipped too, matching what the splitter itself lexes as a
/// comment, so a trailing `# …` block is not sent to the server as an empty
/// statement and INSERTs below one still batch.
pub fn strip_leading_comments_in(dialect: Dialect, text: &str) -> &str {
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if dialect == Dialect::MySql && (rest.starts_with("/*!") || rest.starts_with("/*M!")) {
            // A versioned comment is code to MySQL / MariaDB (mysqldump
            // writes whole CREATE VIEW / TRIGGER statements inside them).
            return rest;
        }
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.find('\n').map_or("", |at| &after[at + 1..]);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.find("*/").map_or("", |at| &after[at + 2..]);
        } else if let Some(after) = rest.strip_prefix('#').filter(|_| dialect == Dialect::MySql) {
            rest = after.find('\n').map_or("", |at| &after[at + 1..]);
        } else {
            return rest;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_all(src: &str, dialect: Dialect, chunk: usize) -> Vec<ScriptUnit> {
        let mut splitter = StreamSplitter::new(dialect);
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < src.len() {
            let mut end = (pos + chunk).min(src.len());
            while end < src.len() && !src.is_char_boundary(end) {
                end += 1;
            }
            splitter.push(&src[pos..end], &mut out);
            pos = end;
        }
        splitter.finish(&mut out).expect("finish");
        out
    }

    fn assert_chunks_agree(src: &str, dialect: Dialect) {
        let a = split_all(src, dialect, 1);
        let b = split_all(src, dialect, 7);
        let c = split_all(src, dialect, 4096);
        assert_eq!(a, b, "chunk 1 vs 7");
        assert_eq!(b, c, "chunk 7 vs 4096");
    }

    #[test]
    fn semicolon_inside_string_does_not_split() {
        let src = "insert into t values ('a;b'); select 1";
        assert_chunks_agree(src, Dialect::Postgres);
        let units = split_all(src, Dialect::Postgres, 7);
        assert_eq!(units.len(), 2);
        assert!(units[0].text.contains("'a;b'"));
        assert_eq!(units[1].text, "select 1");
    }

    #[test]
    fn dollar_quoted_body_postgres() {
        let src = "create function f() returns int as $$ begin return 1; end $$ language plpgsql; select 2;";
        assert_chunks_agree(src, Dialect::Postgres);
        let units = split_all(src, Dialect::Postgres, 4096);
        assert_eq!(units.len(), 2);
        assert!(units[0].text.contains("return 1;"));
        assert_eq!(units[1].text.trim_end_matches(';'), "select 2");
    }

    #[test]
    fn tagged_dollar_quote() {
        let src = "$body$ foo ; bar $body$; select 1;";
        assert_chunks_agree(src, Dialect::Postgres);
        let units = split_all(src, Dialect::Postgres, 1);
        assert_eq!(units.len(), 2);
        assert!(units[0].text.contains(';'));
    }

    #[test]
    fn go_inside_block_comment_ignored() {
        let src = "/* GO */ select 1\nGO\nselect 2";
        assert_chunks_agree(src, Dialect::SqlServer);
        let units = split_all(src, Dialect::SqlServer, 4096);
        assert_eq!(units.len(), 2);
        assert!(units[0].text.contains("select 1"));
        assert_eq!(units[1].text, "select 2");
    }

    #[test]
    fn go_inside_string_ignored() {
        let src = "select 'GO'\nGO\nselect 2";
        assert_chunks_agree(src, Dialect::SqlServer);
        let units = split_all(src, Dialect::SqlServer, 7);
        assert_eq!(units.len(), 2);
    }

    #[test]
    fn go_case_insensitive_with_trailing_comment() {
        let src = "select 1\ngo -- next\nselect 2";
        assert_chunks_agree(src, Dialect::SqlServer);
        let units = split_all(src, Dialect::SqlServer, 4096);
        assert_eq!(units.len(), 2);
    }

    #[test]
    fn semicolon_never_splits_sqlserver() {
        let src = "select 1; select 2\nGO\nselect 3";
        assert_chunks_agree(src, Dialect::SqlServer);
        let units = split_all(src, Dialect::SqlServer, 4096);
        assert_eq!(units.len(), 2);
        assert!(units[0].text.contains("select 1;"));
        assert_eq!(units[1].text, "select 3");
    }

    #[test]
    fn mysql_delimiter_block() {
        let src =
            "DELIMITER //\ncreate procedure p() begin select 1; end//\nDELIMITER ;\nselect 2;";
        assert_chunks_agree(src, Dialect::MySql);
        let units = split_all(src, Dialect::MySql, 7);
        assert_eq!(units.len(), 2);
        assert!(units[0].text.contains("create procedure"));
        assert!(units[1].text.starts_with("select 2"));
    }

    #[test]
    fn mysql_delimiter_directive_that_contains_the_current_delimiter() {
        // mysqldump writes `DELIMITER ;;` around routines and triggers: the
        // directive line must not be cut at its own `;`.
        let src = "DELIMITER ;;\nCREATE PROCEDURE p() BEGIN SELECT 1; END ;;\nDELIMITER ;\nselect 2;\nDELIMITER ;";
        assert_chunks_agree(src, Dialect::MySql);
        let units = split_all(src, Dialect::MySql, 7);
        let texts: Vec<&str> = units.iter().map(|u| u.text.trim()).collect();
        assert_eq!(
            texts,
            vec!["CREATE PROCEDURE p() BEGIN SELECT 1; END", "select 2"]
        );
    }

    #[test]
    fn mysql_versioned_comment_statements_are_kept() {
        // mysqldump's view and trigger statements live entirely inside
        // `/*!NNNNN … */` comments, which MySQL executes.
        let src = "/*!50001 DROP VIEW IF EXISTS `v`*/;\n/*!50001 CREATE ALGORITHM=UNDEFINED */\n/*!50013 SQL SECURITY DEFINER */\n/*!50001 VIEW `v` AS select 1 AS `1` */;\n/* plain comment */;\nselect 2;";
        assert_chunks_agree(src, Dialect::MySql);
        let units = split_all(src, Dialect::MySql, 7);
        assert_eq!(units.len(), 3, "{units:#?}");
        assert!(units[0].text.starts_with("/*!50001 DROP VIEW"));
        assert!(units[1].text.contains("/*!50001 VIEW `v`"));
        assert_eq!(units[2].text.trim(), "select 2");
        // Other engines still treat them as comments.
        assert_eq!(
            strip_leading_comments_in(Dialect::Postgres, "/*!50001 x */"),
            ""
        );
    }

    #[test]
    fn mysql_hash_comment_and_backticks() {
        let src = "# note\nselect `a;b` from t;";
        assert_chunks_agree(src, Dialect::MySql);
        let units = split_all(src, Dialect::MySql, 1);
        assert_eq!(units.len(), 1);
        assert!(units[0].text.contains("`a;b`"));
    }

    #[test]
    fn postgres_nested_block_comment() {
        let src = "select 1 /* outer /* inner ; */ still */ ; select 2;";
        assert_chunks_agree(src, Dialect::Postgres);
        let units = split_all(src, Dialect::Postgres, 4096);
        assert_eq!(units.len(), 2);
    }

    #[test]
    fn bracket_identifier_with_semicolon_sqlserver() {
        let src = "select [a;b] from t\nGO\nselect 2";
        assert_chunks_agree(src, Dialect::SqlServer);
        let units = split_all(src, Dialect::SqlServer, 7);
        assert_eq!(units.len(), 2);
        assert!(units[0].text.contains("[a;b]"));
    }

    #[test]
    fn line_numbers_across_chunks() {
        let src = "select 1;\n\nselect 2;\nselect 3";
        let units = split_all(src, Dialect::Postgres, 3);
        assert_eq!(units[0].start_line, 1);
        assert_eq!(units[1].start_line, 3);
        assert_eq!(units[2].start_line, 4);
    }

    #[test]
    fn trailing_unit_without_terminator_flushed_on_finish() {
        let src = "select 1;\nselect 2";
        let units = split_all(src, Dialect::Postgres, 4096);
        assert_eq!(units.len(), 2);
    }

    #[test]
    fn comment_only_unit_dropped() {
        let src = "-- just a comment\nselect 1;\n-- nothing after this\n/* or this */\n";
        assert_chunks_agree(src, Dialect::Postgres);
        let units = split_all(src, Dialect::Postgres, 4096);
        assert_eq!(units.len(), 1);
        // The comment above a statement belongs to it; only units with
        // nothing but comments are dropped.
        assert_eq!(units[0].text, "-- just a comment\nselect 1");
    }

    #[test]
    fn mysql_hash_comment_only_unit_dropped() {
        // `#` is a line comment on MySQL/MariaDB (the splitter lexes it as
        // one); a trailing `# …` block used to become a unit of its own that
        // Run SQL File sent to the server, which answered ER_EMPTY_QUERY.
        let src = "# header\nselect 1;\n# nothing after this\n# or this\n";
        assert_chunks_agree(src, Dialect::MySql);
        let units = split_all(src, Dialect::MySql, 4096);
        assert_eq!(units.len(), 1, "{units:#?}");
        assert_eq!(units[0].text, "# header\nselect 1");
        // Elsewhere `#` is code and the unit stays.
        let units = split_all("select 1;\n# not a comment\n", Dialect::Postgres, 4096);
        assert_eq!(units.len(), 2, "{units:#?}");
    }

    #[test]
    fn tsql_header_comment_stays_with_batch() {
        // SQL Server stores a view's or procedure's text from the start of
        // its batch, so a header comment must reach the server with it.
        let src = "-- Orders view\n/* author: me\n   multi-line */\nCREATE VIEW dbo.vOrders AS SELECT 1 AS x\nGO\n-- only a comment\nGO\n\n-- second header\nCREATE PROCEDURE dbo.p AS SELECT 2\nGO";
        assert_chunks_agree(src, Dialect::SqlServer);
        let units = split_all(src, Dialect::SqlServer, 4096);
        assert_eq!(units.len(), 2, "{units:#?}");
        assert_eq!(
            units[0].text,
            "-- Orders view\n/* author: me\n   multi-line */\nCREATE VIEW dbo.vOrders AS SELECT 1 AS x"
        );
        assert_eq!(units[0].start_line, 1);
        assert_eq!(
            units[1].text,
            "-- second header\nCREATE PROCEDURE dbo.p AS SELECT 2"
        );
        assert_eq!(units[1].start_line, 9);
        // Same with a trailing comment on the last GO.
        let units = split_all("SELECT 1\nGO -- end", Dialect::SqlServer, 3);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].text, "SELECT 1");
    }

    #[test]
    fn strip_leading_comments_skips_line_and_block_comments() {
        assert_eq!(
            strip_leading_comments("  -- a\n/* b\n c */ -- d\n  INSERT INTO t VALUES (1)"),
            "INSERT INTO t VALUES (1)"
        );
        assert_eq!(strip_leading_comments("select 1 -- x"), "select 1 -- x");
        assert_eq!(strip_leading_comments("-- only"), "");
        // `#` only opens a comment on MySQL/MariaDB.
        assert_eq!(
            strip_leading_comments_in(Dialect::MySql, "# a\n  -- b\n# c\nINSERT INTO t VALUES (1)"),
            "INSERT INTO t VALUES (1)"
        );
        assert_eq!(strip_leading_comments_in(Dialect::MySql, "# only\n"), "");
        assert_eq!(strip_leading_comments("# only\n"), "# only\n");
        assert_eq!(
            strip_leading_comments_in(Dialect::Postgres, "# x\nselect 1"),
            "# x\nselect 1"
        );
    }

    #[test]
    fn unterminated_string_reports_error_on_finish() {
        let mut splitter = StreamSplitter::new(Dialect::Postgres);
        let mut out = Vec::new();
        splitter.push("select 'x", &mut out);
        assert!(splitter.finish(&mut out).is_err());
    }

    #[test]
    fn crlf_line_endings() {
        let src = "select 1;\r\nselect 2;";
        assert_chunks_agree(src, Dialect::Postgres);
        let units = split_all(src, Dialect::Postgres, 5);
        assert_eq!(units.len(), 2);
    }
}
