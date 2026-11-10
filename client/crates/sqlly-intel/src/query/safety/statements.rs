//! The one statement view every safety check shares.
//!
//! Safety never lexes raw SQL itself. Text is read through the security
//! lexer (`sqlly_lang::seclex`): split into the execution units the driver
//! will send, and each unit read under every lexical interpretation its
//! engine could apply (sql_mode, standard_conforming_strings, executable
//! comment versions, …). Each reading is a *shadow* — same byte length as
//! the text, only live code left — and every check runs over every shadow, so
//! no check can see a statement another one misses, and no reading the server
//! might use goes unjudged.

use crate::query::tokenizer::{Token, TokenKind};
use sqlly_core::DatabaseType;
use sqlly_lang::seclex::{self, ExecUnit, LexEngine, LexTarget, View, Views};

/// Where a text is going: a connection's engine, or anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Destination {
    Engine(DatabaseType),
    Anywhere,
}

impl Destination {
    pub(crate) fn target(self) -> LexTarget {
        match self {
            Destination::Engine(db) => LexTarget::for_database(db),
            Destination::Anywhere => LexTarget::AnyEngine,
        }
    }
}

/// One execution unit with every reading of it.
pub(crate) struct UnitViews {
    pub unit: ExecUnit,
    pub views: Views,
}

impl UnitViews {
    /// Map a span inside the unit's text back onto the caller's SQL.
    pub(crate) fn map_span(&self, start: usize, end: usize) -> (usize, usize) {
        if self.unit.verbatim {
            (self.unit.start + start, self.unit.start + end)
        } else {
            (self.unit.start, self.unit.end)
        }
    }
}

/// The units a destination receives for `sql`, each with all its readings.
pub(crate) fn unit_views(sql: &str, dest: Destination) -> Vec<UnitViews> {
    let units = match dest {
        Destination::Engine(db) => seclex::execution_units(sql, db),
        Destination::Anywhere => {
            if sql.trim().is_empty() {
                Vec::new()
            } else {
                vec![ExecUnit {
                    text: sql.to_string(),
                    start: 0,
                    end: sql.len(),
                    verbatim: true,
                }]
            }
        }
    };
    let target = dest.target();
    units
        .into_iter()
        .map(|unit| {
            let views = seclex::views(&unit.text, target);
            UnitViews { unit, views }
        })
        .collect()
}

pub(crate) struct StatementSequence {
    pub(crate) keywords: Vec<String>,
    /// Every bare word (keywords and identifiers), uppercased, in order —
    /// filled only by [`statement_words`].
    pub(crate) words: Vec<String>,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Statement keyword sequences of one reading. Statements end at `;`, and on
/// T-SQL also at a `GO` line (a client batch separator, which the whole-text
/// callers see before the driver splits on it).
pub(crate) fn keyword_sequences(view: &View) -> Vec<StatementSequence> {
    sequences(view, false)
}

/// [`keyword_sequences`] with each statement's bare words (keywords and
/// identifiers) collected too, in one pass.
pub(crate) fn statement_words(view: &View) -> Vec<StatementSequence> {
    sequences(view, true)
}

fn sequences(view: &View, with_words: bool) -> Vec<StatementSequence> {
    let go_lines = view.engine == LexEngine::TSql;
    let tokens = view.tokens();
    let mut statements = Vec::new();
    let mut keywords = Vec::new();
    let mut words = Vec::new();
    let mut start: Option<usize> = None;
    let mut end = 0usize;

    for (i, token) in tokens.iter().enumerate() {
        let separator = (token.kind == TokenKind::Symbol && token.text == ";")
            || (go_lines && is_go_line(&tokens, i));
        if separator {
            if let Some(s) = start.take() {
                statements.push(StatementSequence {
                    keywords: std::mem::take(&mut keywords),
                    words: std::mem::take(&mut words),
                    start: s,
                    end: token.end,
                });
            }
            continue;
        }
        match token.kind {
            TokenKind::Comment | TokenKind::Whitespace => {}
            kind => {
                if start.is_none() {
                    start = Some(token.start);
                }
                end = token.end;
                if kind == TokenKind::Keyword {
                    keywords.push(token.text.to_ascii_uppercase());
                }
                if with_words && matches!(kind, TokenKind::Keyword | TokenKind::Identifier) {
                    words.push(token.text.to_ascii_uppercase());
                }
            }
        }
    }

    if let Some(s) = start {
        statements.push(StatementSequence {
            keywords,
            words,
            start: s,
            end,
        });
    }
    statements
}

/// `GO` alone on its line (comments are already blank in a shadow).
fn is_go_line(tokens: &[Token<'_>], i: usize) -> bool {
    let t = &tokens[i];
    if !t.text.eq_ignore_ascii_case("go") {
        return false;
    }
    let begins = tokens[..i]
        .iter()
        .rev()
        .take_while(|p| p.kind == TokenKind::Whitespace)
        .any(|p| p.text.contains('\n'))
        || tokens[..i].iter().all(|p| p.kind == TokenKind::Whitespace);
    let ends = tokens[i + 1..]
        .iter()
        .take_while(|n| n.kind == TokenKind::Whitespace)
        .any(|n| n.text.contains('\n'))
        || tokens[i + 1..]
            .iter()
            .all(|n| n.kind == TokenKind::Whitespace);
    begins && ends
}

/// A text whose every statement, under every reading, is `USE <db>` — a
/// session context switch that mutates nothing and must actually take effect,
/// so it is never wrapped in a rolled-back transaction by the connection
/// policy.
pub(super) fn is_pure_use_batch(sql: &str, dest: Destination) -> bool {
    let units = unit_views(sql, dest);
    !units.is_empty()
        && units.iter().all(|u| {
            u.views.ambiguity().is_none()
                && u.views.views.iter().all(|view| {
                    let statements = keyword_sequences(view);
                    !statements.is_empty()
                        && statements
                            .iter()
                            .all(|s| s.keywords.len() == 1 && s.keywords[0] == "USE")
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_use_batch_is_not_pure_use() {
        let any = Destination::Anywhere;
        assert!(!is_pure_use_batch("USE db; someproc", any));
        assert!(is_pure_use_batch("USE db", any));
        // A second statement hidden from one reading is still seen by another.
        let mysql = Destination::Engine(DatabaseType::MySql);
        assert!(!is_pure_use_batch("USE `'`; DELETE FROM t; -- '", mysql));
    }

    #[test]
    fn go_lines_separate_statements_on_tsql_only() {
        let views = seclex::views("SELECT 1\nGO\nfoo 42", LexTarget::Engine(LexEngine::TSql));
        let statements = keyword_sequences(views.primary());
        assert_eq!(statements.len(), 2);
        assert!(statements[1].keywords.is_empty());
        let views = seclex::views("SELECT go\nFROM t", LexTarget::Engine(LexEngine::TSql));
        assert_eq!(keyword_sequences(views.primary()).len(), 1);
    }
}
