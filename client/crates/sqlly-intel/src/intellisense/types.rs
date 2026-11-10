use sqlly_core::ObjectKind;
use sqlly_lang::fsmodel::SchemaSqlNode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionKind {
    Keyword,
    Database,
    Schema,
    Table,
    Column,
    Function,
    Procedure,
    Type,
    Parameter,
    ResultSetColumn,
    Snippet,
    Value,
    Join,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompletionSource {
    Keyword,
    Builtin,
    LiveMetadata,
    Filesystem,
    Inferred,
    Snippet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub label: String,
    pub insert_text: String,
    pub kind: CompletionKind,
    pub source: CompletionSource,
    pub score: i32,
    pub detail: String,
    pub replace_span: Option<(usize, usize)>,
}

/// A foreign-key relationship from a column on a `CompletionObject` to a
/// target table+column. Populated by the catalog builder when FK metadata is
/// available (from filesystem DDL or live metadata). The join-candidates
/// module consumes this to suggest `JOIN` completions.
///
/// One `FkRelation` is one column pair. A composite foreign key contributes
/// one relation per key column, in key-column order, all sharing the same
/// [`constraint`](Self::constraint) — consumers that build join conditions
/// must group them (see [`FkRelation::group`]) so the key is joined on every
/// column, not one suggestion per column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FkRelation {
    pub column: String,
    pub target_schema: String,
    pub target_table: String,
    pub target_column: String,
    /// Identity of the FK constraint this pair belongs to (its name, or a
    /// synthesized per-table key for unnamed constraints). `None` marks a
    /// standalone single-column relation.
    pub constraint: Option<String>,
}

impl FkRelation {
    /// Group an object's relations into whole foreign keys: pairs sharing a
    /// [`constraint`](Self::constraint) and target table form one key, kept
    /// in their (key-column) order; a relation without a constraint is a key
    /// of its own. Keys come back in order of first appearance.
    pub fn group(relations: &[FkRelation]) -> Vec<Vec<&FkRelation>> {
        let mut keys: Vec<Vec<&FkRelation>> = Vec::new();
        for rel in relations {
            let existing = rel.constraint.as_ref().and_then(|constraint| {
                keys.iter_mut().find(|key| {
                    let first = key[0];
                    first.constraint.as_ref() == Some(constraint)
                        && first.target_schema.eq_ignore_ascii_case(&rel.target_schema)
                        && first.target_table.eq_ignore_ascii_case(&rel.target_table)
                })
            });
            match existing {
                Some(key) => key.push(rel),
                None => keys.push(vec![rel]),
            }
        }
        keys
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionObject {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub name: String,
    pub kind: ObjectKind,
    pub columns: Vec<String>,
    pub parameters: Vec<String>,
    pub foreign_keys: Vec<FkRelation>,
}

impl CompletionObject {
    pub fn table(schema: impl Into<String>, name: impl Into<String>, columns: Vec<String>) -> Self {
        Self {
            database: None,
            schema: Some(schema.into()),
            name: name.into(),
            kind: ObjectKind::Table,
            columns,
            parameters: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CompletionCatalog {
    pub databases: Vec<String>,
    pub schemas: Vec<String>,
    pub objects: Vec<CompletionObject>,
    pub filesystem_schemas: Vec<SchemaSqlNode>,
    pub snippets: Vec<(String, String)>,
}
