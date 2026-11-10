//! Safety verification: token-by-token equivalence between input and output
//! under the transforms the active options permit. Any mismatch makes
//! `format_sql` return its input unchanged.

use crate::sqlformat::prefs::{IdentifierQuoting, SqlFormattingPreferences};
use crate::sqllex::{Token, TokenKind};

fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Token-content equivalence between input and output under the transforms
/// `prefs` permits.
pub(super) fn tokens_equivalent(
    orig: &[Token],
    new: &[Token],
    prefs: &SqlFormattingPreferences,
) -> bool {
    let a: Vec<&Token> = orig
        .iter()
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace))
        .collect();
    let b: Vec<&Token> = new
        .iter()
        .filter(|t| !matches!(t.kind, TokenKind::Whitespace))
        .collect();

    let paren_inserts_allowed = prefs.parenthesize_mixed_boolean_logic;

    let mut i = 0usize;
    let mut j = 0usize;
    loop {
        match (a.get(i), b.get(j)) {
            (Some(ta), Some(tb)) => {
                if token_matches(ta, tb, prefs) {
                    i += 1;
                    j += 1;
                    continue;
                }
                if paren_inserts_allowed && matches!(tb.kind, TokenKind::LParen | TokenKind::RParen)
                {
                    j += 1;
                    continue;
                }
                return false;
            }
            (None, Some(tb)) => {
                if paren_inserts_allowed && matches!(tb.kind, TokenKind::LParen | TokenKind::RParen)
                {
                    j += 1;
                    continue;
                }
                return false;
            }
            (Some(_), None) => return false,
            (None, None) => return true,
        }
    }
}

fn token_matches(a: &Token, b: &Token, prefs: &SqlFormattingPreferences) -> bool {
    use TokenKind::*;
    match (a.kind, b.kind) {
        (Comment(_), Comment(_)) => {
            if prefs.format_inside_block_comments {
                normalize_ws(a.text) == normalize_ws(b.text)
            } else {
                a.text.trim_end() == b.text.trim_end()
            }
        }
        (Keyword, Keyword) => a.text.eq_ignore_ascii_case(b.text),
        // A keyword-spelled name behind a qualifier (`o.type`) may gain
        // quotes (`"o"."type"`) when the quoting option asks for them.
        (Keyword, Ident(_)) | (Ident(_), Keyword) => {
            let quoting_may_change = !matches!(
                prefs.identifier_quoting,
                IdentifierQuoting::PreserveExisting
            );
            if quoting_may_change {
                a.ident_text().eq_ignore_ascii_case(&b.ident_text())
            } else {
                a.text.eq_ignore_ascii_case(b.text)
            }
        }
        (Ident(_), Ident(_)) => {
            let quoting_may_change = !matches!(
                prefs.identifier_quoting,
                IdentifierQuoting::PreserveExisting
            );
            if quoting_may_change {
                a.ident_text().eq_ignore_ascii_case(&b.ident_text())
            } else if a.ident_quoted() || b.ident_quoted() {
                a.text == b.text
            } else {
                a.text.eq_ignore_ascii_case(b.text)
            }
        }
        (Number(_), Number(_))
        | (StringLit(_), StringLit(_))
        | (Operator, Operator)
        | (Comma, Comma)
        | (Semicolon, Semicolon)
        | (LParen, LParen)
        | (RParen, RParen)
        | (Dot, Dot)
        | (ParamMarker(_), ParamMarker(_)) => a.text == b.text,
        _ => false,
    }
}
