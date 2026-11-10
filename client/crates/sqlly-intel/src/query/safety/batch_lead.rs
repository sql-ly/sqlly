//! T-SQL "must lead its batch" rules: CREATE/ALTER PROCEDURE, FUNCTION,
//! TRIGGER, VIEW, SCHEMA, DEFAULT, RULE must be the first statement of their
//! batch or SQL Server refuses the whole batch with error 111. This module
//! recognizes both shapes: a batch that *is* such a definition (so wraps must
//! step aside) and a definition preceded by another statement (a violation to
//! explain client-side).

use super::statements::keyword_sequences;
use crate::query::tokenizer::TokenKind;
use sqlly_lang::seclex::{self, LexEngine, LexTarget};

/// T-SQL statements that must lead (and be alone in) their batch:
/// CREATE/ALTER PROCEDURE, FUNCTION, TRIGGER, VIEW, SCHEMA, DEFAULT, RULE.
/// Prefixing them with anything makes the server reject the batch.
pub(super) fn tsql_batch_must_lead(sql: &str) -> bool {
    let views = seclex::views(sql, LexTarget::Engine(LexEngine::TSql));
    let mut words = views
        .primary()
        .tokens()
        .into_iter()
        .filter(|t| matches!(t.kind, TokenKind::Keyword | TokenKind::Identifier))
        .take(4)
        .map(|t| t.text.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .into_iter();
    fn is_kind(w: &str) -> bool {
        matches!(
            w,
            "proc" | "procedure" | "function" | "trigger" | "view" | "schema" | "default" | "rule"
        )
    }
    match words.next().as_deref() {
        Some("alter") => words.next().as_deref().is_some_and(is_kind),
        Some("create") => match words.next().as_deref() {
            // CREATE OR ALTER
            Some("or") => {
                words.next(); // alter
                words.next().as_deref().is_some_and(is_kind)
            }
            Some(kind) => is_kind(kind),
            None => false,
        },
        _ => false,
    }
}

/// A T-SQL "must lead its batch" DDL statement (`CREATE/ALTER PROCEDURE`,
/// `FUNCTION`, `TRIGGER`, `VIEW`, `SCHEMA`, `DEFAULT`, `RULE`) found *after*
/// another statement in the same batch. SQL Server refuses this with error 111
/// ("'CREATE/ALTER PROCEDURE' must be the first statement in a query batch"),
/// so we recognize the shape client-side and explain the fix instead of
/// forwarding a cryptic server error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchLeadViolation {
    /// The offending verb + kind, e.g. `"CREATE PROCEDURE"` or `"ALTER VIEW"`.
    pub ddl: String,
    /// Leading keyword of the statement that runs before it (`"SET"`, `"USE"`,
    /// …), or `None` when it could not be named cleanly.
    pub preceding: Option<String>,
}

impl BatchLeadViolation {
    pub fn display_message(&self, wrapped_in_rollback: bool) -> sqlly_core::DisplayMessage {
        let key = match (self.preceding.is_some(), wrapped_in_rollback) {
            (true, false) => "services.safety.batch_lead.named",
            (false, false) => "services.safety.batch_lead.unnamed",
            (true, true) => "services.safety.batch_lead.named_rollback",
            (false, true) => "services.safety.batch_lead.unnamed_rollback",
        };
        let mut message = sqlly_core::DisplayMessage::new(key, self.message(wrapped_in_rollback))
            .with_arg("ddl", self.ddl.clone());
        if let Some(preceding) = &self.preceding {
            message = message.with_arg("preceding_statement", preceding);
        }
        message
    }

    /// English compatibility fallback; localized callers use [`Self::display_message`].
    pub fn message(&self, wrapped_in_rollback: bool) -> String {
        let preceding = match &self.preceding {
            Some(word) => format!("a {word} statement"),
            None => "another statement".to_string(),
        };
        let mut msg = format!(
            "{ddl} must be the first statement in its batch, but {preceding} runs before it in \
             the same batch — SQL Server rejects this with error 111. Put a GO on its own line \
             before {ddl} so it starts a fresh batch.",
            ddl = self.ddl,
        );
        if wrapped_in_rollback {
            msg.push_str(
                " This connection also wraps each run in a rolled-back transaction, which adds a \
                 leading BEGIN TRANSACTION of its own; run \u{201c}Execute Without Rollback \
                 Wrap\u{201d} to drop that wrap for this run.",
            );
        }
        msg
    }
}

/// Uppercased word naming a routine/object whose `CREATE`/`ALTER` must lead its
/// batch on SQL Server.
fn is_must_lead_kind(word: &str) -> bool {
    matches!(
        word,
        "PROC" | "PROCEDURE" | "FUNCTION" | "TRIGGER" | "VIEW" | "SCHEMA" | "DEFAULT" | "RULE"
    )
}

/// If the uppercased leading `keywords` form `CREATE [OR ALTER|OR REPLACE]
/// <kind>` or `ALTER <kind>` for a must-lead kind, return the `"<VERB> <KIND>"`
/// label; otherwise `None`.
fn statement_lead_ddl(keywords: &[String]) -> Option<String> {
    let mut it = keywords.iter().map(String::as_str);
    match it.next()? {
        "ALTER" => {
            let kind = it.next()?;
            is_must_lead_kind(kind).then(|| format!("ALTER {kind}"))
        }
        "CREATE" => match it.next()? {
            // CREATE OR ALTER / CREATE OR REPLACE
            "OR" => {
                it.next(); // ALTER | REPLACE
                let kind = it.next()?;
                is_must_lead_kind(kind).then(|| format!("CREATE {kind}"))
            }
            kind if is_must_lead_kind(kind) => Some(format!("CREATE {kind}")),
            _ => None,
        },
        _ => None,
    }
}

/// Detects a T-SQL must-lead DDL statement that is not first in its (single-`GO`)
/// batch — the exact shape SQL Server refuses with error 111, read the way
/// SQL Server reads the batch.
/// A batch that simply *is* a routine definition (leading `CREATE PROCEDURE …`)
/// is fine: only a definition preceded by another statement is flagged.
pub fn tsql_batch_lead_violation(sql: &str) -> Option<BatchLeadViolation> {
    let views = seclex::views(sql, LexTarget::Engine(LexEngine::TSql));
    let statements = keyword_sequences(views.primary());
    for (idx, statement) in statements.iter().enumerate().skip(1) {
        if let Some(ddl) = statement_lead_ddl(&statement.keywords) {
            let preceding = statements[idx - 1].keywords.first().cloned();
            return Some(BatchLeadViolation { ddl, preceding });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_lead_violation_flags_ddl_after_another_statement() {
        // A routine definition preceded by any statement in the same batch is
        // the Msg 111 shape: SET/USE/… before CREATE PROCEDURE.
        let v = tsql_batch_lead_violation(
            "SET QUOTED_IDENTIFIER ON;\nCREATE PROCEDURE dbo.P AS SELECT 1",
        )
        .expect("SET before CREATE PROCEDURE is a lead violation");
        assert_eq!(v.ddl, "CREATE PROCEDURE");
        assert_eq!(v.preceding.as_deref(), Some("SET"));

        assert_eq!(
            tsql_batch_lead_violation("USE Sales;\nCREATE OR ALTER VIEW v AS SELECT 1")
                .map(|v| v.ddl),
            Some("CREATE VIEW".to_string()),
        );
        assert_eq!(
            tsql_batch_lead_violation(
                "SELECT 1;\nALTER FUNCTION dbo.F() RETURNS int AS BEGIN RETURN 1 END"
            )
            .map(|v| v.ddl),
            Some("ALTER FUNCTION".to_string()),
        );
    }

    #[test]
    fn batch_lead_violation_ignores_a_leading_or_lone_definition() {
        // The definition itself leading the batch is fine — including bodies
        // whose internal `;` split the statement list — as is a batch with no
        // must-lead DDL at all, and non-lead DDL (DROP, CREATE TABLE) later.
        assert!(
            tsql_batch_lead_violation(
                "-- header\nCREATE PROCEDURE dbo.P AS\nBEGIN\n SET NOCOUNT ON;\n SELECT 1;\nEND"
            )
            .is_none()
        );
        assert!(tsql_batch_lead_violation("SELECT 1;\nSELECT 2").is_none());
        assert!(tsql_batch_lead_violation("SELECT 1;\nDROP PROCEDURE dbo.P").is_none());
        assert!(tsql_batch_lead_violation("SELECT 1;\nCREATE TABLE t (a int)").is_none());
    }
}
