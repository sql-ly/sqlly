const REDACTED_STRING_LITERAL: &str = "'<redacted>'";
const REDACTED_COMMENT: &str = "<redacted comment>";
const REDACTED_TOKEN: &str = "<redacted>";

const SENSITIVE_KEYWORDS: &[&str] = &["IDENTITY_VALUE", "KEY_SOURCE", "PASSWORD", "SECRET"];

/// Masks secret-bearing content in a SQL buffer before it leaves the machine:
/// - single-quoted string literals ('...' with '' escapes) -> `'<redacted>'`
/// - binary literals (0x[0-9A-Fa-f]+) -> `0x0`
/// - the value after PASSWORD/SECRET/IDENTITY-style keywords even when unquoted
///
/// Identifiers, keywords, numbers, and structure are preserved so reviewers
/// can still reproduce the completion context. Comment delimiters stay; bodies
/// become `<redacted comment>`.
pub fn redact_sql(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            out.push_str("/*");
            out.push_str(REDACTED_COMMENT);
            i += 2;
            while i + 1 < bytes.len() {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    out.push_str("*/");
                    i += 2;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            out.push_str("--");
            out.push_str(REDACTED_COMMENT);
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'\'' {
            out.push_str(REDACTED_STRING_LITERAL);
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if bytes[i] == b'[' {
            out.push('[');
            i += 1;
            while i < bytes.len() && bytes[i] != b']' {
                out.push(bytes[i] as char);
                i += 1;
            }
            if i < bytes.len() {
                out.push(']');
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'"' {
            out.push('"');
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                out.push(bytes[i] as char);
                i += 1;
            }
            if i < bytes.len() {
                out.push('"');
                i += 1;
            }
            continue;
        }
        if bytes[i] == b'0' && i + 1 < bytes.len() && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X')
        {
            out.push_str("0x0");
            i += 2;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
            continue;
        }
        if let Some(keyword_len) = sensitive_keyword_len(sql, i) {
            out.push_str(&sql[i..i + keyword_len]);
            i += keyword_len;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                out.push(bytes[i] as char);
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'=' {
                out.push('=');
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    out.push(bytes[i] as char);
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'\'' {
                    out.push_str(REDACTED_STRING_LITERAL);
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == b'\'' {
                            if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                                i += 2;
                            } else {
                                i += 1;
                                break;
                            }
                        } else {
                            i += 1;
                        }
                    }
                } else {
                    out.push_str(REDACTED_TOKEN);
                    while i < bytes.len() && is_unquoted_secret_char(bytes[i]) {
                        i += 1;
                    }
                }
            }
            continue;
        }

        let ch = sql[i..].chars().next().expect("byte index on valid utf-8");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn is_unquoted_secret_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'$' | b'/')
}

fn sensitive_keyword_len(sql: &str, start: usize) -> Option<usize> {
    if start > 0 {
        let prev = sql.as_bytes()[start - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return None;
        }
    }
    let tail = &sql[start..];
    for keyword in SENSITIVE_KEYWORDS {
        if tail.len() >= keyword.len()
            && tail.as_bytes()[..keyword.len()].eq_ignore_ascii_case(keyword.as_bytes())
            && !keyword_continues(tail, keyword.len())
        {
            return Some(keyword.len());
        }
    }
    None
}

fn keyword_continues(tail: &str, len: usize) -> bool {
    tail.as_bytes()
        .get(len)
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_sql_masks_password_literals_and_preserves_structure() {
        let input = "CREATE LOGIN x WITH PASSWORD = 'hunter2'";
        let redacted = redact_sql(input);
        assert!(!redacted.contains("hunter2"));
        assert!(redacted.contains(REDACTED_STRING_LITERAL));
        assert!(redacted.contains("CREATE LOGIN x WITH PASSWORD"));
    }
}
