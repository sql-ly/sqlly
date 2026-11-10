//! # sqlly-sqlparse
//!
//! Per-dialect recovering parsers over `sqlly-sqllex` token streams
//! (ADR-011 §3). Produces span-faithful [`ParsedTree`]s: structured DDL
//! statements, every symbol reference with its use-kind, value slots for
//! ADR-010 type validation, and recorded recovery actions. Lowering converts
//! statements into [`crate::sqlmodel::ObjectFragment`]s. Deterministic and
//! panic-free on arbitrary input. No I/O.
//!
//! Layout: the output model lives in [`tree`], the recovering
//! `StatementParser` in [`parser`] (split by concern), and slot resolution in
//! [`slots`]; everything public is re-exported from the crate root.

#![forbid(unsafe_code)]

mod parser;
pub mod slots;
mod tree;

use crate::sqllex::{TokenCursor, split_statement_tokens};
use crate::sqlmodel::{
    Dialect, ObjectBody, ObjectFragment, ObjectKind, ObjectRef, Provenance, ViewBody,
};

use parser::StatementParser;

pub use slots::{
    LiteralShape, Slot, SlotKind, SlotMember, SlotValue, resolve_slots, resolve_slots_in_tree,
};
pub use tree::{
    CreateRoutineNode, CreateTableNode, CreateViewNode, CteDef, ExecNode, GrabBagToken, InsertNode,
    ParsedTree, QueryNode, StatementNode, SymbolUse, TableSource, UpdateNode, UseKind,
};

pub fn content_hash(text: &str) -> u64 {
    // FNV-1a; stable across runs (std SipHash keys are randomized).
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in text.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn parse(text: &str, dialect: Dialect) -> ParsedTree {
    let mut statements = Vec::new();
    let mut references = Vec::new();
    let mut cte_definitions = Vec::new();
    let mut recovery = Vec::new();

    // Single-lex pipeline: the splitter tokenizes the document once and hands
    // each statement's token run (document-absolute spans) straight to the
    // cursor — statements are never re-lexed.
    for tokens in split_statement_tokens(text, dialect) {
        let refs_before = references.len();
        let ctes_before = cte_definitions.len();
        let mut cursor = TokenCursor::from_tokens(tokens);
        {
            let mut parser = StatementParser {
                cur: &mut cursor,
                dialect,
                references: &mut references,
                ctes: &mut cte_definitions,
            };
            if let Some(stmt) = parser.parse_statement() {
                statements.push(stmt);
            }
        }
        // Stamp everything this statement produced with its index so
        // consumers can scope resolution per statement.
        let statement_index = statements.len().saturating_sub(1);
        for reference in &mut references[refs_before..] {
            reference.statement_index = statement_index;
        }
        for cte in &mut cte_definitions[ctes_before..] {
            cte.statement_index = statement_index;
        }
        recovery.extend(cursor.recovery);
    }

    ParsedTree {
        statements,
        references,
        cte_definitions,
        recovery,
        dialect,
        content_hash: content_hash(text),
    }
}

// ---------------------------------------------------------------------------
// Lowering: ParsedTree → ObjectFragments
// ---------------------------------------------------------------------------

pub fn lower(tree: &ParsedTree, provenance: Provenance) -> Vec<ObjectFragment> {
    let mut out = Vec::new();

    for stmt in &tree.statements {
        match stmt {
            StatementNode::CreateTable(node) => {
                let refs: Vec<ObjectRef> = node
                    .body
                    .foreign_keys
                    .iter()
                    .map(|fk| fk.target.clone())
                    .collect();
                out.push(ObjectFragment {
                    reference: node.reference.clone(),
                    kind: ObjectKind::Table,
                    dialect: tree.dialect,
                    body: ObjectBody::Table(node.body.clone()),
                    provenance: provenance.clone(),
                    fidelity: tree.fidelity(),
                    references: refs,
                });
            }
            StatementNode::CreateView(node) => {
                let refs: Vec<ObjectRef> = tree
                    .references
                    .iter()
                    .filter(|u| u.use_kind == UseKind::TableSource)
                    .filter(|u| node.span.contains(u.span.start))
                    .map(|u| u.reference.clone())
                    .collect();
                out.push(ObjectFragment {
                    reference: node.reference.clone(),
                    kind: ObjectKind::View,
                    dialect: tree.dialect,
                    body: ObjectBody::View(ViewBody {
                        columns: Vec::new(),
                        definition: Some(node.definition.clone()),
                    }),
                    provenance: provenance.clone(),
                    fidelity: tree.fidelity(),
                    references: refs,
                });
            }
            StatementNode::CreateRoutine(node) => {
                out.push(ObjectFragment {
                    reference: node.reference.clone(),
                    kind: node.kind,
                    dialect: tree.dialect,
                    body: ObjectBody::Routine(node.body.clone()),
                    provenance: provenance.clone(),
                    fidelity: tree.fidelity(),
                    references: Vec::new(),
                });
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod lib_tests {
    use super::content_hash;

    #[test]
    fn content_hash_is_fnv1a() {
        // Empty input yields the FNV-1a 64-bit offset basis; the hash must be
        // stable across runs (it keys persisted caches).
        assert_eq!(content_hash(""), 0xcbf29ce484222325);
        assert_eq!(content_hash("select 1"), content_hash("select 1"));
        assert_ne!(content_hash("select 1"), content_hash("select 2"));
    }
}
