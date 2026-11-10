//! MySQL / MariaDB `DEFINER = user` clause rewriting for dumps and scripts.
//!
//! A dump taken on one server names the accounts that own its views,
//! routines, triggers and events (`CREATE DEFINER=`root`@`localhost` …`,
//! `/*!50017 DEFINER=… */`). Importing it where those accounts do not exist
//! fails, so the dump dialog can strip the clauses from mysqldump's output and
//! Run SQL File can strip them or point them at `CURRENT_USER` before each
//! statement runs.
//!
//! The rewriter is a byte-level lexer, not a regex: string literals, quoted
//! identifiers and ordinary comments pass through untouched, `/*! … */`
//! versioned comments are scanned as code (that is where mysqldump puts the
//! clauses), and a clause is only recognized where MySQL accepts one — after
//! `CREATE`, `ALTER`, `OR REPLACE`, an `ALGORITHM=` value, or at the start of
//! a versioned comment. It streams: input arrives in arbitrary chunks (dumps
//! can be many gigabytes, and BLOB data need not be valid UTF-8), and any
//! token that could still change meaning with the next chunk is held back.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefinerMode {
    /// Remove the clause; the object is then owned by whoever runs the script.
    Strip,
    /// Replace the account with `CURRENT_USER`.
    CurrentUser,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Lex {
    Code,
    Single,
    Double,
    Backtick,
    LineComment,
    BlockComment,
}

/// What the last code token was, for deciding whether `DEFINER` here starts
/// a definer clause.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Prev {
    /// A token after which a definer clause is valid.
    ClauseSlot,
    Other,
}

/// A definer clause found at the cursor.
struct Clause {
    /// End of the account (`` `u`@`h` ``, `CURRENT_USER()`, …).
    user_end: usize,
    /// End of the whitespace run after the account.
    ws_end: usize,
    current_user: bool,
}

enum Scan<T> {
    Found(T),
    /// The answer depends on bytes that have not arrived yet.
    NeedMore,
    NoMatch,
}

fn need<T>(fin: bool) -> Scan<T> {
    if fin { Scan::NoMatch } else { Scan::NeedMore }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// Bytes of an unquoted account or host name (`root`, `10.0.%`, `db-1.lan`).
fn is_bare_name_byte(b: u8) -> bool {
    is_word_byte(b) || matches!(b, b'.' | b'%' | b'-')
}

fn skip_space(buf: &[u8], mut i: usize) -> usize {
    while i < buf.len() && is_space(buf[i]) {
        i += 1;
    }
    i
}

fn word_end(buf: &[u8], mut i: usize) -> usize {
    while i < buf.len() && is_word_byte(buf[i]) {
        i += 1;
    }
    i
}

/// Words after which MySQL accepts a `DEFINER = …` clause.
fn opens_clause_slot(word: &[u8]) -> bool {
    [
        &b"CREATE"[..],
        b"ALTER",
        b"REPLACE",
        b"UNDEFINED",
        b"MERGE",
        b"TEMPTABLE",
    ]
    .iter()
    .any(|kw| word.eq_ignore_ascii_case(kw))
}

/// End of the quoted name starting at `i` (`buf[i]` is the quote).
fn quoted_end(buf: &[u8], i: usize, fin: bool) -> Scan<usize> {
    let quote = buf[i];
    let mut k = i + 1;
    while k < buf.len() {
        let b = buf[k];
        if b == b'\\' && quote != b'`' {
            if k + 1 >= buf.len() {
                return need(fin);
            }
            k += 2;
            continue;
        }
        if b == quote {
            if k + 1 >= buf.len() && !fin {
                return Scan::NeedMore;
            }
            if buf.get(k + 1) == Some(&quote) {
                k += 2;
                continue;
            }
            return Scan::Found(k + 1);
        }
        k += 1;
    }
    need(fin)
}

/// End of one account part (user or host) starting at `i`.
fn name_part_end(buf: &[u8], i: usize, fin: bool) -> Scan<usize> {
    let Some(&b) = buf.get(i) else {
        return need(fin);
    };
    match b {
        b'\'' | b'"' | b'`' => quoted_end(buf, i, fin),
        b if is_bare_name_byte(b) => {
            let mut k = i;
            while k < buf.len() && is_bare_name_byte(buf[k]) {
                k += 1;
            }
            if k == buf.len() && !fin {
                Scan::NeedMore
            } else {
                Scan::Found(k)
            }
        }
        _ => Scan::NoMatch,
    }
}

/// Parse `= account` after a `DEFINER` keyword that ends at `after_word`.
fn parse_clause(buf: &[u8], after_word: usize, fin: bool) -> Scan<Clause> {
    let len = buf.len();
    let mut j = skip_space(buf, after_word);
    if j >= len {
        return need(fin);
    }
    if buf[j] != b'=' {
        return Scan::NoMatch;
    }
    j = skip_space(buf, j + 1);
    if j >= len {
        return need(fin);
    }
    let mut current_user = false;
    let user_end;
    let word = word_end(buf, j);
    if word == len && !fin && is_word_byte(buf[j]) {
        return Scan::NeedMore;
    }
    if buf[j..word].eq_ignore_ascii_case(b"CURRENT_USER") {
        current_user = true;
        let k = skip_space(buf, word);
        if k >= len {
            if !fin {
                return Scan::NeedMore;
            }
            user_end = word;
        } else if buf[k] == b'(' {
            let close = skip_space(buf, k + 1);
            if close >= len {
                return need(fin);
            }
            if buf[close] != b')' {
                return Scan::NoMatch;
            }
            user_end = close + 1;
        } else {
            user_end = word;
        }
    } else {
        let user = match name_part_end(buf, j, fin) {
            Scan::Found(end) => end,
            Scan::NeedMore => return Scan::NeedMore,
            Scan::NoMatch => return Scan::NoMatch,
        };
        if user >= len && !fin {
            return Scan::NeedMore;
        }
        user_end = if buf.get(user) == Some(&b'@') {
            match name_part_end(buf, user + 1, fin) {
                Scan::Found(end) => end,
                Scan::NeedMore => return Scan::NeedMore,
                Scan::NoMatch => return Scan::NoMatch,
            }
        } else {
            user
        };
    }
    let ws_end = skip_space(buf, user_end);
    if ws_end >= len && !fin {
        return Scan::NeedMore;
    }
    Scan::Found(Clause {
        user_end,
        ws_end,
        current_user,
    })
}

/// Streaming DEFINER rewriter; feed chunks with [`push`](Self::push), then
/// call [`finish`](Self::finish) once for the held-back tail.
pub struct DefinerRewriter {
    mode: DefinerMode,
    lex: Lex,
    prev: Prev,
    /// Last code byte consumed, so `@definer` / `t.definer` never count.
    last_code_byte: u8,
    /// Inside a `/*! … */` versioned comment.
    in_versioned: bool,
    /// The versioned comment's opening marker plus whitespace, held until
    /// real content shows up: a comment whose only content was a stripped
    /// clause is dropped entirely.
    versioned_pending: Option<Vec<u8>>,
    versioned_stripped: bool,
    /// Drop one space or tab after a dropped versioned comment.
    skip_one_space: bool,
    /// Unconsumed input (a token that may continue in the next chunk).
    carry: Vec<u8>,
    rewrites: usize,
}

impl DefinerRewriter {
    pub fn new(mode: DefinerMode) -> Self {
        Self {
            mode,
            lex: Lex::Code,
            prev: Prev::Other,
            last_code_byte: b' ',
            in_versioned: false,
            versioned_pending: None,
            versioned_stripped: false,
            skip_one_space: false,
            carry: Vec::new(),
            rewrites: 0,
        }
    }

    /// Rewrite the next chunk; returns the bytes that are final so far.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let mut out = Vec::with_capacity(buf.len());
        let consumed = self.scan(&buf, false, &mut out);
        self.carry = buf.split_off(consumed);
        out
    }

    /// Flush everything still held back. Call once, after the last `push`.
    pub fn finish(&mut self) -> Vec<u8> {
        let buf = std::mem::take(&mut self.carry);
        let mut out = Vec::with_capacity(buf.len() + 16);
        let consumed = self.scan(&buf, true, &mut out);
        debug_assert_eq!(consumed, buf.len());
        if let Some(pending) = self.versioned_pending.take() {
            out.extend_from_slice(&pending);
        }
        out
    }

    /// DEFINER clauses removed or remapped so far.
    pub fn rewrites(&self) -> usize {
        self.rewrites
    }

    /// Emit `buf[from..to]` and stop there: the byte at `to` needs lookahead.
    /// `None` (consume nothing) when there is nothing before it.
    fn emit_held(
        &mut self,
        out: &mut Vec<u8>,
        buf: &[u8],
        from: usize,
        to: usize,
    ) -> Option<usize> {
        if to == from {
            return None;
        }
        self.emit(out, &buf[from..to]);
        Some(to)
    }

    fn emit(&mut self, out: &mut Vec<u8>, bytes: &[u8]) {
        if let Some(pending) = self.versioned_pending.as_mut() {
            if bytes.iter().all(|&b| is_space(b)) {
                pending.extend_from_slice(bytes);
                return;
            }
            out.append(pending);
            self.versioned_pending = None;
        }
        out.extend_from_slice(bytes);
    }

    /// Consume as much of `buf` as can be decided; returns the bytes consumed.
    fn scan(&mut self, buf: &[u8], fin: bool, out: &mut Vec<u8>) -> usize {
        let mut i = 0;
        while i < buf.len() {
            if self.skip_one_space {
                self.skip_one_space = false;
                if matches!(buf[i], b' ' | b'\t') {
                    i += 1;
                    continue;
                }
            }
            let next = match self.lex {
                Lex::Code => self.code_step(buf, i, fin, out),
                Lex::Single | Lex::Double | Lex::Backtick => self.quoted_step(buf, i, fin, out),
                Lex::LineComment => {
                    let end = buf[i..]
                        .iter()
                        .position(|&b| b == b'\n')
                        .map(|p| {
                            self.lex = Lex::Code;
                            i + p + 1
                        })
                        .unwrap_or(buf.len());
                    self.emit(out, &buf[i..end]);
                    Some(end)
                }
                Lex::BlockComment => self.block_comment_step(buf, i, fin, out),
            };
            match next {
                Some(next) => i = next,
                None => return i,
            }
        }
        buf.len()
    }

    fn quoted_step(&mut self, buf: &[u8], i: usize, fin: bool, out: &mut Vec<u8>) -> Option<usize> {
        let quote = match self.lex {
            Lex::Single => b'\'',
            Lex::Double => b'"',
            _ => b'`',
        };
        let backslash = quote != b'`';
        let mut k = i;
        while k < buf.len() {
            let b = buf[k];
            if backslash && b == b'\\' {
                if k + 1 >= buf.len() {
                    if fin {
                        k += 1;
                        continue;
                    }
                    return self.emit_held(out, buf, i, k);
                }
                k += 2;
                continue;
            }
            if b == quote {
                if k + 1 >= buf.len() && !fin {
                    return self.emit_held(out, buf, i, k);
                }
                if buf.get(k + 1) == Some(&quote) {
                    k += 2;
                    continue;
                }
                self.emit(out, &buf[i..=k]);
                self.lex = Lex::Code;
                self.prev = Prev::Other;
                self.last_code_byte = quote;
                return Some(k + 1);
            }
            k += 1;
        }
        let end = k.min(buf.len());
        self.emit(out, &buf[i..end]);
        Some(end)
    }

    fn block_comment_step(
        &mut self,
        buf: &[u8],
        i: usize,
        fin: bool,
        out: &mut Vec<u8>,
    ) -> Option<usize> {
        let mut k = i;
        while k < buf.len() {
            if buf[k] == b'*' {
                if k + 1 >= buf.len() && !fin {
                    return self.emit_held(out, buf, i, k);
                }
                if buf.get(k + 1) == Some(&b'/') {
                    self.emit(out, &buf[i..k + 2]);
                    self.lex = Lex::Code;
                    return Some(k + 2);
                }
            }
            k += 1;
        }
        self.emit(out, &buf[i..]);
        Some(buf.len())
    }

    fn code_step(&mut self, buf: &[u8], i: usize, fin: bool, out: &mut Vec<u8>) -> Option<usize> {
        let len = buf.len();
        let b = buf[i];
        match b {
            b'\'' | b'"' | b'`' => {
                self.lex = match b {
                    b'\'' => Lex::Single,
                    b'"' => Lex::Double,
                    _ => Lex::Backtick,
                };
                self.emit(out, &buf[i..=i]);
                Some(i + 1)
            }
            b'#' => {
                self.lex = Lex::LineComment;
                self.emit(out, &buf[i..=i]);
                Some(i + 1)
            }
            b'-' => {
                if i + 2 >= len && !fin {
                    return None;
                }
                if buf.get(i + 1) == Some(&b'-') && buf.get(i + 2).is_none_or(|&c| c <= b' ') {
                    self.lex = Lex::LineComment;
                    self.emit(out, &buf[i..i + 2]);
                    return Some(i + 2);
                }
                self.other_byte(out, b);
                Some(i + 1)
            }
            b'/' => {
                if i + 1 >= len && !fin {
                    return None;
                }
                if buf.get(i + 1) != Some(&b'*') {
                    self.other_byte(out, b);
                    return Some(i + 1);
                }
                // `/*!` (MySQL) or `/*M!` (MariaDB) opens a versioned comment.
                if i + 3 >= len && !fin {
                    return None;
                }
                let bang = match (buf.get(i + 2), buf.get(i + 3)) {
                    (Some(b'!'), _) => Some(i + 3),
                    (Some(b'M'), Some(b'!')) => Some(i + 4),
                    _ => None,
                };
                match bang {
                    Some(mut end) if !self.in_versioned => {
                        while end < len && buf[end].is_ascii_digit() {
                            end += 1;
                        }
                        if end == len && !fin {
                            return None;
                        }
                        if let Some(pending) = self.versioned_pending.take() {
                            out.extend_from_slice(&pending);
                        }
                        self.versioned_pending = Some(buf[i..end].to_vec());
                        self.versioned_stripped = false;
                        self.in_versioned = true;
                        self.prev = Prev::ClauseSlot;
                        self.last_code_byte = b' ';
                        Some(end)
                    }
                    _ => {
                        self.lex = Lex::BlockComment;
                        self.emit(out, &buf[i..i + 2]);
                        Some(i + 2)
                    }
                }
            }
            b'*' if self.in_versioned => {
                if i + 1 >= len && !fin {
                    return None;
                }
                if buf.get(i + 1) != Some(&b'/') {
                    self.other_byte(out, b);
                    return Some(i + 1);
                }
                self.in_versioned = false;
                match self.versioned_pending.take() {
                    Some(_) if self.versioned_stripped => self.skip_one_space = true,
                    Some(pending) => {
                        out.extend_from_slice(&pending);
                        out.extend_from_slice(b"*/");
                    }
                    None => out.extend_from_slice(b"*/"),
                }
                self.versioned_stripped = false;
                Some(i + 2)
            }
            b if is_space(b) => {
                let end = skip_space(buf, i);
                self.emit(out, &buf[i..end]);
                Some(end)
            }
            b if is_word_byte(b) => {
                let end = word_end(buf, i);
                if end == len && !fin {
                    return None;
                }
                let word = &buf[i..end];
                let attached = matches!(self.last_code_byte, b'@' | b'.');
                if !attached
                    && self.prev == Prev::ClauseSlot
                    && word.eq_ignore_ascii_case(b"DEFINER")
                {
                    match parse_clause(buf, end, fin) {
                        Scan::NeedMore => return None,
                        Scan::Found(clause) => {
                            return Some(self.rewrite(&buf[i..clause.ws_end], clause, out));
                        }
                        Scan::NoMatch => {}
                    }
                }
                self.prev = if !attached && opens_clause_slot(word) {
                    Prev::ClauseSlot
                } else {
                    Prev::Other
                };
                self.last_code_byte = buf[end - 1];
                self.emit(out, word);
                Some(end)
            }
            _ => {
                self.other_byte(out, b);
                Some(i + 1)
            }
        }
    }

    fn other_byte(&mut self, out: &mut Vec<u8>, b: u8) {
        self.prev = Prev::Other;
        self.last_code_byte = b;
        self.emit(out, &[b]);
    }

    /// Apply the mode to the clause at the cursor (`text` runs from the
    /// `DEFINER` keyword through the whitespace after the account); returns
    /// where scanning resumes.
    fn rewrite(&mut self, text: &[u8], clause: Clause, out: &mut Vec<u8>) -> usize {
        self.last_code_byte = b' ';
        match self.mode {
            DefinerMode::Strip => {
                self.rewrites += 1;
                if self.in_versioned {
                    self.versioned_stripped = true;
                }
                // Keep the line breaks the clause spanned so later
                // statements keep their line numbers in error reports.
                let breaks = text.iter().filter(|&&b| b == b'\n').count();
                if breaks > 0 {
                    self.emit(out, &vec![b'\n'; breaks]);
                }
                clause.ws_end
            }
            DefinerMode::CurrentUser => {
                if !clause.current_user {
                    self.rewrites += 1;
                }
                self.emit(out, b"DEFINER=CURRENT_USER");
                clause.user_end
            }
        }
    }
}

/// Rewrite a whole script in one call; returns the text and the number of
/// clauses changed.
pub fn rewrite_definers(text: &str, mode: DefinerMode) -> (String, usize) {
    let mut rewriter = DefinerRewriter::new(mode);
    let mut out = rewriter.push(text.as_bytes());
    out.extend(rewriter.finish());
    (
        String::from_utf8_lossy(&out).into_owned(),
        rewriter.rewrites(),
    )
}

/// Rewrite a dump file in place, streaming through a temporary copy so a
/// multi-gigabyte dump never sits in memory. The copy lives in the system
/// temp directory and is written back over the original, so only the file
/// itself needs to be writable (a sandboxed app may be granted the file but
/// not its folder). Returns the number of clauses changed; the file is left
/// untouched when there were none.
#[cfg(not(target_arch = "wasm32"))]
pub fn rewrite_definers_in_file(
    path: &std::path::Path,
    mode: DefinerMode,
) -> std::io::Result<usize> {
    use std::io::{Read, Seek, Write};

    let mut input = std::fs::File::open(path)?;
    let mut temp = tempfile::tempfile()?;
    let mut rewriter = DefinerRewriter::new(mode);
    let mut buf = vec![0u8; 256 * 1024];
    {
        let mut writer = std::io::BufWriter::new(&mut temp);
        loop {
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            writer.write_all(&rewriter.push(&buf[..n]))?;
        }
        writer.write_all(&rewriter.finish())?;
        writer.flush()?;
    }
    drop(input);
    let rewrites = rewriter.rewrites();
    if rewrites == 0 {
        return Ok(0);
    }
    temp.seek(std::io::SeekFrom::Start(0))?;
    let mut target = std::fs::File::create(path)?;
    std::io::copy(&mut temp, &mut target)?;
    target.flush()?;
    Ok(rewrites)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(text: &str) -> String {
        rewrite_definers(text, DefinerMode::Strip).0
    }

    fn remap(text: &str) -> String {
        rewrite_definers(text, DefinerMode::CurrentUser).0
    }

    #[test]
    fn strips_create_definer_procedure_function_and_view() {
        assert_eq!(
            strip("CREATE DEFINER=`root`@`localhost` PROCEDURE `p`() BEGIN END"),
            "CREATE PROCEDURE `p`() BEGIN END"
        );
        assert_eq!(
            strip("CREATE DEFINER=`app`@`%` FUNCTION f() RETURNS INT RETURN 1"),
            "CREATE FUNCTION f() RETURNS INT RETURN 1"
        );
        assert_eq!(
            strip(
                "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`localhost` SQL SECURITY DEFINER VIEW v AS SELECT 1"
            ),
            "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW v AS SELECT 1"
        );
        assert_eq!(
            strip("create or replace definer = 'u'@'h' view v as select 1"),
            "create or replace view v as select 1"
        );
        assert_eq!(
            strip("ALTER DEFINER=`a`@`b` EVENT e ON COMPLETION PRESERVE"),
            "ALTER EVENT e ON COMPLETION PRESERVE"
        );
    }

    #[test]
    fn strips_trigger_and_event_definers_inside_versioned_comments() {
        assert_eq!(
            strip(
                "/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET @a = 1 */;;"
            ),
            "/*!50003 CREATE*/ /*!50003 TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET @a = 1 */;;"
        );
        assert_eq!(
            strip(
                "/*!50106 CREATE*/ /*!50117 DEFINER=`root`@`localhost`*/ /*!50106 EVENT `e` ON SCHEDULE EVERY 1 DAY DO SELECT 1 */ ;;"
            ),
            "/*!50106 CREATE*/ /*!50106 EVENT `e` ON SCHEDULE EVERY 1 DAY DO SELECT 1 */ ;;"
        );
    }

    #[test]
    fn strips_view_definer_but_keeps_sql_security() {
        let input = "/*!50001 CREATE ALGORITHM=UNDEFINED */\n/*!50013 DEFINER=`root`@`localhost` SQL SECURITY DEFINER */\n/*!50001 VIEW `v` AS select 1 AS `1` */;";
        assert_eq!(
            strip(input),
            "/*!50001 CREATE ALGORITHM=UNDEFINED */\n/*!50013 SQL SECURITY DEFINER */\n/*!50001 VIEW `v` AS select 1 AS `1` */;"
        );
    }

    #[test]
    fn remaps_to_current_user_everywhere() {
        assert_eq!(
            remap("CREATE DEFINER=`root`@`localhost` PROCEDURE p() BEGIN END"),
            "CREATE DEFINER=CURRENT_USER PROCEDURE p() BEGIN END"
        );
        assert_eq!(
            remap("/*!50013 DEFINER=`root`@`localhost` SQL SECURITY DEFINER */"),
            "/*!50013 DEFINER=CURRENT_USER SQL SECURITY DEFINER */"
        );
        assert_eq!(
            remap("/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`%`*/ /*!50003 TRIGGER t"),
            "/*!50003 CREATE*/ /*!50017 DEFINER=CURRENT_USER*/ /*!50003 TRIGGER t"
        );
    }

    #[test]
    fn handles_every_quoting_style_and_bare_names() {
        for user in [
            "`ro``ot`@`local``host`",
            "'it''s'@'%'",
            "'it\\'s'@'%'",
            "\"svc\"@\"10.0.0.%\"",
            "root@localhost",
            "'app'@'192.168.%'",
            "`app-user`@`db.example.com`",
            "CURRENT_USER",
            "CURRENT_USER()",
            "current_user ( )",
            "`only_user`",
        ] {
            let input = format!("CREATE DEFINER={user} PROCEDURE p() SELECT 1");
            assert_eq!(strip(&input), "CREATE PROCEDURE p() SELECT 1", "{user}");
            assert_eq!(
                remap(&input),
                "CREATE DEFINER=CURRENT_USER PROCEDURE p() SELECT 1",
                "{user}"
            );
        }
    }

    #[test]
    fn leaves_string_literals_and_comments_alone() {
        let input = "INSERT INTO t VALUES ('CREATE DEFINER=`root`@`localhost` PROCEDURE p', \"/*!50017 DEFINER=`x`@`y`*/\");\n-- CREATE DEFINER=`a`@`b` VIEW v\n# CREATE DEFINER=`a`@`b` VIEW v\n/* CREATE DEFINER=`a`@`b` VIEW v */\nSELECT `CREATE DEFINER=x@y VIEW`;";
        assert_eq!(strip(input), input);
        assert_eq!(remap(input), input);
        // Escaped and doubled quotes do not end a literal early.
        let escaped =
            "SELECT 'a\\' CREATE DEFINER=`r`@`h` PROCEDURE', 'b'' CREATE DEFINER=`r`@`h` VIEW';";
        assert_eq!(strip(escaped), escaped);
        // A multi-line literal stays a literal on every line.
        let multiline = "SELECT 'first line\nCREATE DEFINER=`r`@`h` VIEW v';";
        assert_eq!(strip(multiline), multiline);
    }

    #[test]
    fn leaves_unrelated_definer_words_alone() {
        for text in [
            "SQL SECURITY DEFINER",
            "UPDATE accounts SET definer = 'bob' WHERE id = 1",
            "SET @definer = 'x'@'y'",
            "SELECT t.definer FROM information_schema.views t",
            "SELECT definer FROM mysql.proc WHERE definer='root@localhost'",
            "CREATE TABLE t (definer VARCHAR(10))",
            "CREATE TABLE definer (id INT)",
        ] {
            assert_eq!(strip(text), text);
            assert_eq!(remap(text), text);
        }
    }

    #[test]
    fn counts_rewrites() {
        let (_, count) = rewrite_definers(
            "CREATE DEFINER=a@b VIEW v AS SELECT 1; CREATE DEFINER=`c`@`d` PROCEDURE p() BEGIN END; SELECT 'DEFINER=x';",
            DefinerMode::Strip,
        );
        assert_eq!(count, 2);
        // Already CURRENT_USER is not a change when remapping.
        let (_, count) = rewrite_definers(
            "CREATE DEFINER=CURRENT_USER VIEW v AS SELECT 1",
            DefinerMode::CurrentUser,
        );
        assert_eq!(count, 0);
    }

    #[test]
    fn strips_a_clause_that_spans_lines_but_keeps_the_line_count() {
        let input = "CREATE\n  DEFINER = `root`@`localhost`\n  PROCEDURE p() BEGIN END";
        let output = strip(input);
        assert_eq!(output, "CREATE\n  \nPROCEDURE p() BEGIN END");
        assert_eq!(output.lines().count(), input.lines().count());
    }

    #[test]
    fn streaming_matches_one_shot_at_every_split_point() {
        let input = "/*!50003 CREATE*/ /*!50017 DEFINER=`ro``ot`@`localhost`*/ /*!50003 TRIGGER t */;;\nINSERT INTO t VALUES ('it''s DEFINER=`x`@`y`', \"q\\\"\");\n-- DEFINER=`a`@`b`\nCREATE DEFINER='u'@'%' PROCEDURE p() SELECT 'é';\n/*!50013 DEFINER=root@localhost SQL SECURITY DEFINER */ x-1 a/b c*d";
        for mode in [DefinerMode::Strip, DefinerMode::CurrentUser] {
            let (expected, expected_count) = rewrite_definers(input, mode);
            assert!(expected_count >= 3);
            let bytes = input.as_bytes();
            for split in 0..=bytes.len() {
                let mut rw = DefinerRewriter::new(mode);
                let mut out = rw.push(&bytes[..split]);
                out.extend(rw.push(&bytes[split..]));
                out.extend(rw.finish());
                assert_eq!(String::from_utf8(out).unwrap(), expected, "split {split}");
                assert_eq!(rw.rewrites(), expected_count, "split {split}");
            }
            // Byte-at-a-time is the worst case for the hold-back logic.
            let mut rw = DefinerRewriter::new(mode);
            let mut out = Vec::new();
            for b in bytes {
                out.extend(rw.push(std::slice::from_ref(b)));
            }
            out.extend(rw.finish());
            assert_eq!(String::from_utf8(out).unwrap(), expected);
        }
    }

    #[test]
    fn binary_bytes_pass_through_untouched() {
        let mut input = b"INSERT INTO b VALUES ('".to_vec();
        input.extend([0xff, 0xfe, 0x00, b'\'', b'\'', 0x80]);
        input.extend(b"');\nCREATE DEFINER=`r`@`h` VIEW v AS SELECT 1;");
        let mut rw = DefinerRewriter::new(DefinerMode::Strip);
        let mut out = rw.push(&input);
        out.extend(rw.finish());
        let mut expected = b"INSERT INTO b VALUES ('".to_vec();
        expected.extend([0xff, 0xfe, 0x00, b'\'', b'\'', 0x80]);
        expected.extend(b"');\nCREATE VIEW v AS SELECT 1;");
        assert_eq!(out, expected);
    }

    #[test]
    fn incomplete_clause_at_end_of_input_is_left_as_written() {
        for text in [
            "CREATE DEFINER=",
            "CREATE DEFINER",
            "CREATE DEFINER=`unterminated",
            "CREATE DEFINER=@",
            "/*!50017 DEFINER=`root`@`localhost`",
        ] {
            let expected = if text.starts_with("/*!") {
                // An unterminated versioned comment keeps its text; only the
                // clause itself is removed.
                "/*!50017 ".to_string()
            } else {
                text.to_string()
            };
            assert_eq!(strip(text), expected, "{text}");
        }
    }

    #[test]
    fn mariadb_versioned_comments_are_code() {
        assert_eq!(
            strip(
                "/*M!100100 CREATE*/ /*M!100100 DEFINER=`root`@`localhost`*/ /*M!100100 TRIGGER t */"
            ),
            "/*M!100100 CREATE*/ /*M!100100 TRIGGER t */"
        );
    }

    #[test]
    fn rewrite_file_rewrites_in_place_without_leaving_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dump.sql");
        std::fs::write(
            &path,
            "CREATE DEFINER=`root`@`localhost` PROCEDURE p() BEGIN END;\nSELECT 1;\n",
        )
        .unwrap();
        let count = rewrite_definers_in_file(&path, DefinerMode::Strip).unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            std::fs::read_to_string(&path)
                .unwrap()
                .replace("\r\n", "\n"),
            "CREATE PROCEDURE p() BEGIN END;\nSELECT 1;\n"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
