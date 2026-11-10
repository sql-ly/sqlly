//! Output model: the [`ParsedTree`] a parse produces, its statement nodes,
//! and the symbol-use records consumers resolve against.

use crate::sqllex::RecoveryAction;
use crate::sqlmodel::{
    Dialect, Fidelity, ObjectKind, ObjectRef, RecoveryNote, RoutineBody, Span, SqlName, TableBody,
};
use serde::{Deserialize, Serialize};

use crate::sqlparse::slots::SlotValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UseKind {
    TableSource, // FROM/JOIN/UPDATE/INSERT INTO/DELETE FROM target
    Column,      // qualified column reference
    /// Unqualified column candidate recorded ONLY in WHERE / JOIN ON
    /// predicates, where select-list aliases can never legally appear —
    /// keywords, function names, parameters, casts, and subquery contents are
    /// excluded at record time so consumers can resolve without re-guessing.
    ColumnBare,
    RoutineCall, // EXEC / CALL / function invocation
    ForeignKeyTarget,
    TypeName,
    SynonymTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolUse {
    pub reference: ObjectRef,
    pub use_kind: UseKind,
    pub span: Span,
    /// Alias this use introduces (table sources) or was qualified with.
    pub via_alias: Option<String>,
    /// Index of the statement this use belongs to in [`ParsedTree::statements`].
    #[serde(default)]
    pub statement_index: usize,
    /// True when the use sits in a predicate region (WHERE / JOIN ON /
    /// HAVING), which is what tenant-scope checking cares about.
    #[serde(default)]
    pub in_predicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrabBagToken {
    pub text: String,
    pub span: Span,
    pub shape: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatementNode {
    CreateTable(CreateTableNode),
    CreateView(CreateViewNode),
    CreateRoutine(CreateRoutineNode),
    Select(QueryNode),
    Insert(InsertNode),
    Update(UpdateNode),
    Delete(QueryNode),
    Exec(ExecNode),
    /// Statements we recognize but don't structure; span retained.
    Opaque {
        span: Span,
        head: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateTableNode {
    pub reference: ObjectRef,
    pub body: TableBody,
    pub span: Span,
    /// Tokens that fit no production, preserved per ADR-011 §2.3.
    pub grab_bag: Vec<GrabBagToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateViewNode {
    pub reference: ObjectRef,
    pub span: Span,
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateRoutineNode {
    pub reference: ObjectRef,
    pub kind: ObjectKind, // Procedure | Function
    pub body: RoutineBody,
    pub span: Span,
}

/// Table source with optional alias, as written in FROM/JOIN/UPDATE/….
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableSource {
    pub reference: ObjectRef,
    pub alias: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryNode {
    pub tables: Vec<TableSource>,
    /// Aliases of sources whose columns the parser cannot see: derived
    /// tables (`FROM (…) d`), table-valued functions (`OPENJSON(…) AS j`,
    /// `CROSS APPLY …`), and `PIVOT` / `UNPIVOT`. An unaliased one records an
    /// empty string. Their presence tells validation to stay conservative
    /// about bare-column resolution in this statement.
    #[serde(default)]
    pub derived_aliases: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsertNode {
    pub target: TableSource,
    pub columns: Vec<(SqlName, Span)>,
    /// Number of expressions in each VALUES row.
    pub value_row_arities: Vec<usize>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateNode {
    pub target: TableSource,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecNode {
    pub target: ObjectRef,
    pub span: Span,
    /// (param name if named, value) — slot resolution consumes this.
    pub args: Vec<(Option<String>, SlotValue)>,
}

/// A common-table-expression definition harvested from a `WITH` clause. The
/// body's table sources and qualified columns are recorded as ordinary
/// references; the definition itself exists so consumers can tell a CTE name
/// from an unknown table and, when columns are declared, resolve against them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CteDef {
    pub name: String,
    /// Declared output columns, when the CTE spells them (`name (a, b) AS …`).
    pub columns: Vec<String>,
    pub span: Span,
    /// Statement (in [`ParsedTree::statements`]) the WITH clause precedes.
    #[serde(default)]
    pub statement_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedTree {
    pub statements: Vec<StatementNode>,
    pub references: Vec<SymbolUse>,
    /// CTE definitions per statement, in source order.
    #[serde(default)]
    pub cte_definitions: Vec<CteDef>,
    pub recovery: Vec<RecoveryAction>,
    pub dialect: Dialect,
    pub content_hash: u64,
}

impl ParsedTree {
    pub fn fidelity(&self) -> Fidelity {
        if self.recovery.is_empty() {
            Fidelity::Exact
        } else {
            Fidelity::Recovered {
                actions: self
                    .recovery
                    .iter()
                    .map(|r| RecoveryNote {
                        kind: format!("{:?}", r.kind),
                        span: r.span,
                        assumption: r.assumption.clone(),
                    })
                    .collect(),
            }
        }
    }
}
