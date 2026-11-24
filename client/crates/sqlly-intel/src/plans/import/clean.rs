//! Removing what gets wrapped around a plan between the database and the
//! clipboard.
//!
//! Two stages. The first undoes damage that can happen to any document: a
//! wrong encoding, Windows line ends, Markdown fences, an editor's line
//! numbers, a log's line prefixes, HTML escaping, a JSON string literal
//! around the whole thing, the statement and prompt above the output. The
//! second unwraps a plan that is one cell of a client's result table (psql,
//! the mysql client, clickhouse-client). Decoration that is the format
//! itself is left alone: MySQL's tabular EXPLAIN box, Oracle's DBMS_XPLAN
//! table, DuckDB's boxes and SQL Server's fixed-width columns are read by
//! their parsers.

use serde_json::Value;

/// Decode the bytes of a plan file. `.sqlplan` files saved by SSMS are
/// UTF-16 with a byte-order mark; everything else is UTF-8. A file with no
/// mark whose every other byte is zero is taken as UTF-16 too. Bytes that
/// are not valid in the encoding become U+FFFD rather than an error.
pub fn decode_plan_bytes(bytes: &[u8]) -> String {
    let utf16 = |body: &[u8], little_endian: bool| {
        let units: Vec<u16> = body
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                if little_endian {
                    u16::from_le_bytes(*pair)
                } else {
                    u16::from_be_bytes(*pair)
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(body) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(body).into_owned();
    }
    if let Some(body) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(body, true);
    }
    if let Some(body) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(body, false);
    }
    // No mark: ASCII text stored as UTF-16 has a zero in every other byte.
    let sample = &bytes[..bytes.len().min(256)];
    if sample.len() >= 8 {
        let zeros_at = |offset: usize| {
            sample
                .iter()
                .skip(offset)
                .step_by(2)
                .filter(|byte| **byte == 0)
                .count()
        };
        let half = sample.len() / 2;
        if zeros_at(1) * 10 >= half * 9 && zeros_at(0) == 0 {
            return utf16(bytes, true);
        }
        if zeros_at(0) * 10 >= half * 9 && zeros_at(1) == 0 {
            return utf16(bytes, false);
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Undo what clients, chat tools and logs wrap around a plan: a byte-order
/// mark, CRLF line ends, Markdown code fences, line numbers, log line
/// prefixes, HTML entities, a JSON string literal around the document, the
/// prompt and statement above the output, pgAdmin's quoted lines, psql's
/// aligned cell (`QUERY PLAN` header, dashes, ` +` continuations, `(1 row)`),
/// the mysql client's `\G` banner or table borders around a single
/// `EXPLAIN` cell, and clickhouse-client's `explain` column.
///
/// Cleaning already clean text returns it unchanged apart from surrounding
/// whitespace.
pub fn clean_plan_text(text: &str) -> String {
    clean_layers(text, 0)
}

/// One pass of [`clean_plan_text`]. A plan can sit under two wrappers (a
/// TSV-escaped cell inside a code fence, an escaped cell stored as a JSON
/// string), so a pass that uncovers a one-line escaped document runs again.
fn clean_layers(text: &str, depth: u8) -> String {
    let mut text: String = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace(['\0', '\u{feff}'], "")
        .replace('\u{a0}', " ");

    if let Some(inner) = unquote_json_string(&text) {
        if depth < 3 {
            return clean_layers(&inner, depth + 1);
        }
        text = inner;
    }
    if let Some(decoded) = decode_html_entities(&text) {
        text = decoded;
    }

    let mut lines: Vec<String> = strip_code_fences(&text);
    strip_log_prefixes(&mut lines);
    strip_line_numbers(&mut lines);
    strip_quoted_lines(&mut lines);
    strip_prompt_and_statement(&mut lines);
    if depth < 3 {
        let mut content = lines.iter().filter(|line| !line.trim().is_empty());
        if let (Some(only), None) = (content.next(), content.next())
            && let Some(inner) = unquote_json_string(only)
        {
            return clean_layers(&inner, depth + 1);
        }
    }
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();

    let cleaned = unwrap_client_cell(lines);
    match unwrap_json_result_set(&cleaned) {
        Some(inner) => inner,
        None => cleaned,
    }
}

/// A plan stored as a JSON string: `"[\n  {\n    \"Plan\": ..."`, or the
/// same without the outer quotes, or clickhouse-client's TSV escaping of a
/// JSON cell (`[\n  {\n    "Plan": ...` on one line).
fn unquote_json_string(text: &str) -> Option<String> {
    let t = text.trim();
    if t.len() < 4 || t.contains('\n') && !t.starts_with('"') {
        return None;
    }
    if t.starts_with('"') && t.ends_with('"') {
        let inner = serde_json::from_str::<String>(t).ok()?;
        return looks_structured(&inner).then_some(inner);
    }
    let escaped_json = (t.starts_with("[{\\\"") || t.starts_with("{\\\"") || t.starts_with("[\\n"))
        && t.contains("\\\"");
    if escaped_json {
        let inner = serde_json::from_str::<String>(&format!("\"{t}\"")).ok()?;
        return Some(inner);
    }
    // TSV escaping: newlines are `\n`, quotes are left alone.
    let tsv = (t.starts_with("[\\n") || t.starts_with("{\\n") || t.contains("\\n"))
        && !t.contains('\n')
        && (t.starts_with('[') || t.starts_with('{') || t.starts_with("->"))
        && serde_json::from_str::<Value>(t).is_err();
    if tsv {
        let mut out = String::with_capacity(t.len());
        let mut chars = t.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('\'') => out.push('\''),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        }
        return Some(out);
    }
    None
}

fn looks_structured(inner: &str) -> bool {
    let t = inner.trim_start();
    inner.contains('\n') || t.starts_with(['[', '{', '<']) || t.starts_with("->")
}

/// Decode HTML entities when the whole document was escaped (ShowPlan XML
/// copied out of a grid cell or a web page, `-&gt;` arrows). A real XML
/// document legitimately contains entities inside attribute values, so one
/// that already has its root tag is left alone.
fn decode_html_entities(text: &str) -> Option<String> {
    if !text.contains('&') {
        return None;
    }
    let raw_xml =
        text.contains("<ShowPlanXML") || text.contains("<explain") || text.contains("<?xml");
    let escaped_xml = text.contains("&lt;ShowPlanXML")
        || text.contains("&lt;?xml")
        || text.contains("&lt;explain ")
        || text.contains("&lt;StmtSimple");
    let escaped_text = !text.trim_start().starts_with('<')
        && (text.contains("-&gt;") || text.contains("&quot;") || text.contains("&#39;"));
    if raw_xml || !(escaped_xml || escaped_text) {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest
            .char_indices()
            .take(10)
            .find(|(_, c)| *c == ';')
            .map(|(i, _)| i);
        let decoded = end.and_then(|end| {
            let name = &rest[1..end];
            let c = match name {
                "lt" => '<',
                "gt" => '>',
                "amp" => '&',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                _ => {
                    let code = name.strip_prefix('#')?;
                    let value = match code.strip_prefix(['x', 'X']) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                        None => code.parse::<u32>().ok()?,
                    };
                    char::from_u32(value)?
                }
            };
            Some((c, end + 1))
        });
        match decoded {
            Some((c, used)) => {
                out.push(c);
                rest = &rest[used..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Some(out.replace("\r\n", "\n").replace('\r', "\n"))
}

/// Drop Markdown fences. With an opening and a closing fence, only what is
/// between them is kept, so prose around the block goes too.
fn strip_code_fences(text: &str) -> Vec<String> {
    let is_fence = |line: &str| {
        let t = line.trim_start();
        t.starts_with("```") || t.starts_with("~~~")
    };
    let lines: Vec<&str> = text.lines().collect();
    let fences: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| is_fence(line))
        .map(|(index, _)| index)
        .collect();
    let (from, to) = match fences.as_slice() {
        [first, .., last] => (*first, *last),
        _ => (0, lines.len()),
    };
    lines[from..to]
        .iter()
        .filter(|line| !is_fence(line))
        .map(|line| line.to_string())
        .collect()
}

/// After `YYYY-MM-DD HH:MM:SS`, the rest of a log line's prefix: fraction,
/// zone, `[pid]`, `user@db`, and a `LOG:` style level. Returns what follows.
fn after_log_prefix(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let digits = |range: std::ops::Range<usize>| {
        bytes
            .get(range)
            .is_some_and(|part| part.iter().all(u8::is_ascii_digit))
    };
    let at = |index: usize, wanted: &[u8]| bytes.get(index).is_some_and(|b| wanted.contains(b));
    if !(digits(0..4)
        && at(4, b"-")
        && digits(5..7)
        && at(7, b"-")
        && digits(8..10)
        && at(10, b" T")
        && digits(11..13)
        && at(13, b":")
        && digits(14..16)
        && at(16, b":")
        && digits(17..19))
    {
        return None;
    }
    // Fraction and numeric zone are glued to the time.
    let mut rest = &line[19..];
    let glued = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    rest = rest[glued..].trim_start();
    // Zone name, pid, session tokens: short tokens before the message.
    for _ in 0..6 {
        let token_end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
        let token = &rest[..token_end];
        let zone = (2..=5).contains(&token.len()) && token.chars().all(|c| c.is_ascii_uppercase());
        let bracketed = token.starts_with('[') && token.ends_with(']');
        let session = token.contains('@') && !token.contains('(');
        let level = token.ends_with(':')
            && token.len() > 2
            && token[..token.len() - 1]
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
        if token.is_empty() || !(zone || bracketed || session || level) {
            break;
        }
        rest = rest[token_end..].trim_start();
        if level {
            break;
        }
    }
    Some(rest)
}

/// Server and container logs put a timestamp (and, for PostgreSQL, a
/// process id and `LOG:`) before the first line of a logged plan, or before
/// every line. auto_explain's `duration: ... plan:` line goes as well.
fn strip_log_prefixes(lines: &mut Vec<String>) {
    let stamped = lines
        .iter()
        .filter(|line| after_log_prefix(line).is_some())
        .count();
    if stamped == 0 {
        return;
    }
    let first_is_stamped = lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .is_some_and(|line| after_log_prefix(line).is_some());
    if !first_is_stamped {
        return;
    }
    let every_line = stamped == lines.iter().filter(|line| !line.trim().is_empty()).count();
    let mut out = Vec::with_capacity(lines.len());
    for line in lines.iter() {
        let body = match after_log_prefix(line) {
            Some(rest) => rest.to_string(),
            // PostgreSQL indents a logged message's later lines with a tab.
            None if !every_line => line.strip_prefix('\t').unwrap_or(line).to_string(),
            None => line.clone(),
        };
        let t = body.trim();
        if t.starts_with("duration:") && t.ends_with("plan:") {
            continue;
        }
        out.push(body);
    }
    *lines = out;
}

/// An editor or a web page numbered the lines: `1 `, `2 `, `3 ` (or `1:`,
/// `1.`, `1|`) down the left edge, starting at 1 with no gaps.
fn strip_line_numbers(lines: &mut [String]) {
    let split = |line: &str| -> Option<(u64, usize)> {
        let t = line.trim_start();
        let digits = t.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 || digits > 6 {
            return None;
        }
        let number = t[..digits].parse::<u64>().ok()?;
        let after = &t[digits..];
        let used = match after.chars().next() {
            None => 0,
            Some(':' | '.' | '|' | ')') => {
                // `1. ` or `1: `, then the text.
                1 + usize::from(after[1..].starts_with(' '))
            }
            Some(' ') => 1,
            Some(_) => return None,
        };
        Some((number, line.len() - t.len() + digits + used))
    };
    if lines.len() < 3 {
        return;
    }
    let mut cuts = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        match split(line) {
            Some((number, cut)) if number == index as u64 + 1 => cuts.push(cut),
            _ => return,
        }
    }
    // Rows of numbers are data, not numbering (SQLite and Oracle plan rows
    // start with ids): the text after the number must not be numeric too.
    let numeric_rows = lines.iter().zip(&cuts).all(|(line, cut)| {
        line[*cut..]
            .trim_start()
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    });
    if numeric_rows {
        return;
    }
    for (line, cut) in lines.iter_mut().zip(cuts) {
        *line = line[cut..].to_string();
    }
    // A gutter wider than one space leaves the same padding on every line.
    // Formats whose first line must start at the margin need it gone.
    let padding = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches(' ').len())
        .min()
        .unwrap_or(0);
    if padding > 0 {
        for line in lines.iter_mut() {
            if line.len() >= padding && line.is_char_boundary(padding) {
                *line = line[padding..].to_string();
            }
        }
    }
}

/// pgAdmin and CSV exports quote every line: `"Seq Scan on t  (cost=...)"`,
/// with `""` for a quote inside.
fn strip_quoted_lines(lines: &mut [String]) {
    let quoted = |line: &str| {
        let t = line.trim();
        t.len() >= 2 && t.starts_with('"') && t.ends_with('"')
    };
    let filled: Vec<&String> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
    if filled.is_empty() || !filled.iter().all(|line| quoted(line)) {
        return;
    }
    // A JSON document of strings is not a quoted plan.
    if filled.iter().any(|line| line.trim().ends_with("\",")) {
        return;
    }
    for line in lines.iter_mut() {
        let t = line.trim();
        if t.len() >= 2 {
            *line = t[1..t.len() - 1].replace("\"\"", "\"");
        }
    }
}

const STATEMENT_WORDS: [&str; 16] = [
    "explain", "analyze", "select", "with", "set", "desc", "describe", "pragma", "insert",
    "update", "delete", "show", "use", "alter", "exec", "call",
];

fn starts_statement(text: &str) -> bool {
    let t = text.trim_start();
    if t.starts_with(['\\', '.']) {
        // psql and sqlite3 meta commands: `\x`, `.eqp on`.
        return t.len() > 1 && t[1..].starts_with(|c: char| c.is_ascii_alphabetic());
    }
    let word: String = t
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase();
    STATEMENT_WORDS.contains(&word.as_str())
        && t[word.len()..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '(' || c == ';')
}

/// The statement after a client prompt on `line`: `postgres=# EXPLAIN ...`,
/// `mysql> EXPLAIN ...`, `MariaDB [shop]> `, `sqlite> `, `D `, `:) `,
/// `root@localhost:26257/defaultdb> `, `trino:shop> `, `spark-sql> `,
/// `SQL> `.
fn after_prompt(line: &str) -> Option<&str> {
    let t = line.trim_start();
    for marker in ["=# ", "=> ", "=*# ", "=!# ", "> ", ":) "] {
        let Some(at) = t.find(marker) else { continue };
        let head = &t[..at];
        let prompt_like = head.chars().count() <= 48
            && head.chars().any(|c| c.is_ascii_alphabetic() || c == ':')
            && !head.contains(": ")
            && head.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || matches!(
                        c,
                        '_' | '-' | '@' | ':' | '.' | '/' | '[' | ']' | '(' | ')' | ' '
                    )
            })
            && head.split_whitespace().count() <= 2;
        let rest = &t[at + marker.len()..];
        if prompt_like && starts_statement(rest) {
            return Some(rest);
        }
    }
    // DuckDB's prompt is a bare `D`.
    let rest = t.strip_prefix("D ")?;
    starts_statement(rest).then_some(rest)
}

fn statement_ends(text: &str) -> bool {
    let t = text.trim_end();
    t.ends_with(';') || t.ends_with("\\G") || t.ends_with("\\g") || t.ends_with("\\gx")
}

/// A continuation prompt of a statement typed over several lines.
fn is_continuation(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("-> ") || t.starts_with("...> ") || t.starts_with("> ") {
        return true;
    }
    ["-# ", "(# ", "'# ", "-> ", ":-] "].iter().any(|marker| {
        t.find(marker)
            .is_some_and(|at| at <= 48 && !t[..at].contains(' '))
    })
}

/// Drop the prompt lines above (and below) the output, and a bare
/// `EXPLAIN ...;` statement pasted in front of its own result.
fn strip_prompt_and_statement(lines: &mut Vec<String>) {
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_statement = false;
    let mut budget = 200usize;
    for line in lines.iter() {
        if in_statement {
            if is_continuation(line) && budget > 0 {
                budget -= 1;
                in_statement = !statement_ends(line);
                continue;
            }
            in_statement = false;
        }
        if let Some(statement) = after_prompt(line) {
            in_statement = !statement_ends(statement);
            continue;
        }
        out.push(line.clone());
    }
    // An empty prompt left at the end (`postgres=# `).
    while out.last().is_some_and(|line| {
        let t = line.trim();
        // `shop=#`, `shop=>`, `mysql>`, `sqlite>`, DuckDB's `D`. A closing
        // XML tag also ends in `>`, so the name has to be a plain word.
        let name = t.trim_end_matches(['=', '#', '>', '*', '!']);
        t.is_empty()
            || t == "D"
            || (name.len() < t.len()
                && t.len() <= 48
                && !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '@')))
    }) {
        out.pop();
    }

    // `EXPLAIN ANALYZE SELECT ...;` typed above the plan, without a prompt.
    let first = out.iter().position(|line| !line.trim().is_empty());
    if let Some(first) = first {
        let head = out[first].trim_start().to_ascii_lowercase();
        let explain = ["explain ", "explain(", "analyze "]
            .iter()
            .any(|word| head.starts_with(word));
        if explain {
            // Through the `;` that ends it, else the one line.
            let end = out
                .iter()
                .enumerate()
                .skip(first)
                .take(200)
                .find(|(_, line)| line.trim_end().ends_with(';'))
                .map_or(first, |(index, _)| index);
            // Only when something is left to be the plan.
            if out[end + 1..].iter().any(|line| !line.trim().is_empty()) {
                out.drain(first..=end);
            }
        }
    }
    *lines = out;
}

fn is_rule(t: &str) -> bool {
    !t.is_empty() && t.chars().all(|c| c == '-' || c == '+')
}

fn is_row_count(t: &str) -> bool {
    // `(1 row)`, `(2 rows)`, `1 row in set (0.01 sec)`, `1 row in set, 1 warning (0.00 sec)`
    (t.starts_with('(') && (t.ends_with(" row)") || t.ends_with(" rows)")))
        || t.contains(" row in set")
        || t.contains(" rows in set")
}

/// Stage two: a plan that is one cell of a client's result table.
fn unwrap_client_cell(mut lines: Vec<&str>) -> String {
    if let Some(text) = unwrap_mysql_batch(&lines) {
        return text;
    }
    // psql aligned output: every line of the cell but the last ends in `+`.
    // (Border rows like `+-----+` end in `+` too; they are not continuations.)
    let continued = lines
        .iter()
        .filter(|l| {
            let t = l.trim();
            t.ends_with('+') && !is_rule(t)
        })
        .count();
    let boxed = lines.iter().any(|l| l.trim_start().starts_with("+--"));
    if continued >= 2 && !boxed {
        let mut out = Vec::new();
        for line in &lines {
            let t = line.trim();
            if t.eq_ignore_ascii_case("QUERY PLAN") || is_rule(t) || is_row_count(t) {
                continue;
            }
            let body = line.trim_end();
            let body = body.strip_suffix('+').unwrap_or(body).trim_end();
            out.push(body.strip_prefix(' ').unwrap_or(body).to_string());
        }
        return out.join("\n").trim().to_string();
    }

    if let Some(unwrapped) = unwrap_clickhouse_cell(&lines) {
        return unwrapped;
    }

    // mysql `\G` around one cell: `*** 1. row ***` then `EXPLAIN: <plan>`
    // (`ANALYZE: <plan>` for MariaDB's ANALYZE FORMAT=JSON). The banner of
    // a vertical tabular EXPLAIN (`id: 1`, `select_type: ...`) is part of
    // that format and stays.
    let is_banner = |l: &str| {
        let t = l.trim();
        t.starts_with("***") && t.ends_with("***") && t.contains("row")
    };
    fn cell_label(l: &str) -> Option<&str> {
        let t = l.trim_start();
        t.strip_prefix("EXPLAIN:")
            .or_else(|| t.strip_prefix("ANALYZE:"))
    }
    let single_cell_banner = lines.iter().any(|l| cell_label(l).is_some())
        && !lines
            .iter()
            .any(|l| l.trim_start().starts_with("select_type:"));
    if single_cell_banner {
        lines.retain(|l| !is_banner(l));
    }
    // mysql table mode: `+----+` borders, a `| EXPLAIN |` header, and the
    // cell's text with `| ` before its first line and ` |` after its last.
    let cell_header = |l: &str| {
        let t = l.trim();
        (t.starts_with("| EXPLAIN") || t.starts_with("| ANALYZE"))
            && t.ends_with('|')
            && t.matches('|').count() == 2
    };
    let mysql_table = lines.iter().any(|l| cell_header(l));
    let mut out: Vec<String> = Vec::new();
    for line in lines {
        let t = line.trim();
        if mysql_table && ((t.starts_with('+') && is_rule(t)) || cell_header(line)) {
            continue;
        }
        if is_row_count(t) {
            continue;
        }
        out.push(line.to_string());
    }
    if mysql_table {
        if let Some(first) = out.iter_mut().find(|l| !l.trim().is_empty())
            && let Some(rest) = first.trim_start().strip_prefix("| ")
        {
            *first = rest.to_string();
        }
        if let Some(last) = out.iter_mut().rev().find(|l| !l.trim().is_empty()) {
            let t = last.trim_end();
            *last = t.strip_suffix('|').unwrap_or(t).trim_end().to_string();
        }
        // The cell's lines are padded to the column width.
        for line in &mut out {
            let trimmed = line.trim_end().len();
            line.truncate(trimmed);
        }
    }
    let mut joined = out.join("\n").trim().to_string();
    if single_cell_banner
        && let Some(rest) = ["EXPLAIN:", "ANALYZE:"]
            .iter()
            .find_map(|label| joined.strip_prefix(label))
    {
        joined = rest.trim_start().to_string();
    }
    joined
}

/// `mysql --batch` (and piped output) prints the cell on one line with the
/// line breaks written as a backslash and an `n`, under an `EXPLAIN` header.
fn unwrap_mysql_batch(lines: &[&str]) -> Option<String> {
    let mut content = lines.iter().map(|l| l.trim()).filter(|l| !l.is_empty());
    let first = content.next()?;
    let body = match content.next() {
        Some(second) if first == "EXPLAIN" || first == "ANALYZE" => second,
        Some(_) => return None,
        None => first,
    };
    if content.next().is_some()
        || !body.contains("\\n")
        || !(body.starts_with("-> ") || body.starts_with('{'))
    {
        return None;
    }
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    Some(out.trim().to_string())
}

/// clickhouse-client prints EXPLAIN as a one-column result named `explain`:
/// a Pretty box with numbered rows, `Row N:` blocks in Vertical format, or a
/// bare `explain` header line with TSVWithNames.
fn unwrap_clickhouse_cell(lines: &[&str]) -> Option<String> {
    let first = lines.iter().position(|l| !l.trim().is_empty())?;
    let head = lines[first].trim();

    if head.starts_with('┌') && head.contains("─explain─") {
        let mut out = Vec::new();
        for line in &lines[first + 1..] {
            let t = line.trim();
            if t.starts_with('└') {
                break;
            }
            // `12. │ text │`, or `│ text │` on versions without row numbers.
            let Some(bar) = t.find('│') else { continue };
            if !t[..bar]
                .trim()
                .trim_end_matches('.')
                .chars()
                .all(|c| c.is_ascii_digit())
            {
                continue;
            }
            let body = &t[bar + '│'.len_utf8()..];
            let body = body.strip_suffix('│').unwrap_or(body);
            let body = body.strip_prefix(' ').unwrap_or(body);
            out.push(body.trim_end().to_string());
        }
        return Some(out.join("\n").trim().to_string());
    }

    if head.starts_with("Row 1:") {
        let mut out = Vec::new();
        let mut any = false;
        for line in &lines[first..] {
            let t = line.trim_end();
            if let Some(body) = t.strip_prefix("explain:") {
                any = true;
                out.push(body.strip_prefix(' ').unwrap_or(body).to_string());
            } else if !(t.is_empty()
                || t.starts_with("Row ")
                || t.chars().all(|c| c == '─')
                || is_row_count(t.trim()))
            {
                return None;
            }
        }
        return any.then(|| out.join("\n").trim().to_string());
    }

    if head == "explain" && lines.len() > first + 1 {
        return Some(lines[first + 1..].join("\n").trim().to_string());
    }
    None
}

/// A driver or export tool serialized the result set itself as JSON:
/// `[{"QUERY PLAN": [ ... ]}]`, `[{"QUERY PLAN": "Seq Scan ..."}, ...]`,
/// `[{"EXPLAIN": "-> ..."}]`, `[{"explain": "Expression"}, ...]`.
fn unwrap_json_result_set(text: &str) -> Option<String> {
    let t = text.trim();
    let head: String = t.chars().take(60).collect::<String>().to_ascii_lowercase();
    if !t.starts_with('[')
        || !(head.contains("query plan") || head.contains("explain") || head.contains("analyze"))
    {
        return None;
    }
    let rows: Vec<Value> = serde_json::from_str(t).ok()?;
    let mut parts = Vec::with_capacity(rows.len());
    for row in &rows {
        let object = row.as_object()?;
        if object.len() != 1 {
            return None;
        }
        let (key, value) = object.iter().next()?;
        if !(key.eq_ignore_ascii_case("QUERY PLAN")
            || key.eq_ignore_ascii_case("EXPLAIN")
            || key.eq_ignore_ascii_case("ANALYZE"))
        {
            return None;
        }
        parts.push(match value {
            Value::String(text) => text.clone(),
            other => serde_json::to_string_pretty(other).ok()?,
        });
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("\n").trim().to_string())
}
