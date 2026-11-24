//! Lexical helpers shared by references, expansion and parameter binding:
//! masks that blank everything that is not live code, so a scan of the mask
//! never matches inside a string, comment or quoted name. Masks keep the
//! input's byte length (and newlines), so their offsets index the original.

use sqlly_lang::sqllex::{StrictLexer, TokenKind};
use sqlly_lang::sqlmodel::DatabaseType;

fn blank(bytes: &mut [u8]) {
    for b in bytes {
        if *b != b'\n' {
            *b = b' ';
        }
    }
}

fn into_string(bytes: Vec<u8>) -> String {
    // Every blanked range is a whole token or line, so it starts and ends on
    // character boundaries and the result stays UTF-8.
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `sql` with comments, string literals (every quoting form, dollar quotes
/// included) and quoted identifiers blanked, read the way `db`'s editor
/// lexer reads it.
pub(crate) fn mask_sql(db: DatabaseType, sql: &str) -> String {
    thread_local! {
        static MASKED: std::cell::RefCell<super::memo::SourceMemo> = Default::default();
    }
    MASKED.with(|memo| super::memo::SourceMemo::get(memo, db, sql, || mask_sql_now(db, sql)))
}

fn mask_sql_now(db: DatabaseType, sql: &str) -> String {
    let mut out = sql.as_bytes().to_vec();
    for token in StrictLexer::tokenize(sql, db.family()) {
        let hidden = matches!(token.kind, TokenKind::Comment(_) | TokenKind::StringLit(_))
            || token.ident_quoted();
        if hidden {
            blank(&mut out[token.span.start..token.span.end]);
        }
    }
    into_string(out)
}

/// `text` with fenced code blocks, inline code spans and HTML comments
/// blanked: a `{{name}}` shown as code in Markdown is not a reference.
pub(crate) fn mask_markdown(text: &str) -> String {
    let mut out = text.as_bytes().to_vec();
    let mut fence: Option<(u8, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let run = |c: u8| trimmed.bytes().take_while(|&b| b == c).count();
        if let Some((c, len)) = fence {
            blank(&mut out[start..offset]);
            if indent <= 3 && run(c) >= len && trimmed[run(c)..].trim().is_empty() {
                fence = None;
            }
            continue;
        }
        if indent <= 3 {
            for c in *b"`~" {
                if run(c) >= 3 {
                    fence = Some((c, run(c)));
                    blank(&mut out[start..offset]);
                }
            }
            if fence.is_some() {
                continue;
            }
        }
    }
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if out[i] == b' ' && bytes[i] != b' ' {
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"<!--") {
            let end = text[i + 4..]
                .find("-->")
                .map_or(bytes.len(), |at| i + 4 + at + 3);
            blank(&mut out[i..end]);
            i = end;
            continue;
        }
        if bytes[i] == b'`' {
            let n = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            let mut j = i + n;
            let mut close = None;
            while j < bytes.len() {
                if bytes[j] == b'`' {
                    let m = bytes[j..].iter().take_while(|&&b| b == b'`').count();
                    if m == n {
                        close = Some(j + m);
                        break;
                    }
                    j += m;
                } else {
                    j += 1;
                }
            }
            match close {
                Some(end) => {
                    blank(&mut out[i..end]);
                    i = end;
                }
                None => i += n,
            }
            continue;
        }
        i += 1;
    }
    into_string(out)
}

/// A bare SQL identifier: an ASCII letter or `_`, then letters, digits or
/// `_`, at most 128 bytes.
pub(crate) fn is_identifier(s: &str) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

pub(crate) fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_mask_blanks_strings_comments_and_quoted_names_only() {
        let sql = "SELECT '{{a}}', [{{b}}] -- {{c}}\n/* {{d}} */ {{e}}";
        let masked = mask_sql(DatabaseType::SqlServer, sql);
        assert_eq!(masked.len(), sql.len());
        assert!(!masked.contains("{{a}}"));
        assert!(!masked.contains("{{b}}"));
        assert!(!masked.contains("{{c}}"));
        assert!(!masked.contains("{{d}}"));
        assert!(masked.contains("{{e}}"));
        assert_eq!(masked.matches('\n').count(), 1);
    }

    #[test]
    fn sql_mask_follows_the_dialect() {
        let pg = "SELECT $$ {{a}} $$, \"{{b}}\", `{{c}}`";
        let masked = mask_sql(DatabaseType::Postgres, pg);
        assert!(!masked.contains("{{a}}"));
        assert!(!masked.contains("{{b}}"));
        let mysql = "SELECT `{{c}}`, \"{{d}}\" # {{e}}\n, {{f}}";
        let masked = mask_sql(DatabaseType::MySql, mysql);
        assert!(!masked.contains("{{c}}"));
        assert!(!masked.contains("{{d}}"));
        assert!(!masked.contains("{{e}}"));
        assert!(masked.contains("{{f}}"));
    }

    #[test]
    fn sql_mask_keeps_multibyte_text_valid() {
        let sql = "SELECT 'héllo {{x}}' AS 名前, {{y}}";
        let masked = mask_sql(DatabaseType::Postgres, sql);
        assert_eq!(masked.len(), sql.len());
        assert!(masked.contains("{{y}}"));
    }

    #[test]
    fn markdown_mask_blanks_code_and_comments() {
        let md = "Total {{a}}\n```sql\nSELECT {{b}}\n```\nInline `{{c}}` and ``x {{d}} ``\n<!-- {{e}} -->\n~~~~\n{{f}}\n~~~~\nend {{g}}";
        let masked = mask_markdown(md);
        assert_eq!(masked.len(), md.len());
        for hidden in ["{{b}}", "{{c}}", "{{d}}", "{{e}}", "{{f}}"] {
            assert!(!masked.contains(hidden), "{hidden} should be masked");
        }
        assert!(masked.contains("{{a}}"));
        assert!(masked.contains("{{g}}"));
    }

    #[test]
    fn markdown_unclosed_backtick_is_literal() {
        let masked = mask_markdown("a ` {{x}}");
        assert!(masked.contains("{{x}}"));
    }

    #[test]
    fn identifiers() {
        assert!(is_identifier("orders"));
        assert!(is_identifier("_x1"));
        assert!(!is_identifier("1x"));
        assert!(!is_identifier("a-b"));
        assert!(!is_identifier(""));
        assert!(!is_identifier(&"a".repeat(129)));
    }
}
