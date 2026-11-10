//! Host-side staged grid edits: undo/redo and batch assembly.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::{Hash, Hasher};

use sqlly_core::{CellValue, ConnectionKind, DatabaseType, ResultColumn};
use sqlly_services::row_editing::{
    ColumnExpression, DeleteRowRequest, InsertRowRequest, RowEditSession, StagedBatch, StagedOp,
    UpdateRowRequest,
};

/// A staged cell write: a typed value, or an engine-evaluated expression.
#[derive(Clone, Debug, PartialEq)]
pub enum CellEdit {
    Value(CellValue),
    Expression(ColumnExpression),
}

impl From<CellValue> for CellEdit {
    fn from(value: CellValue) -> Self {
        CellEdit::Value(value)
    }
}

/// Primary-key values in `session.key_columns` order. Wrapped so it can index maps.
#[derive(Clone, Debug)]
pub struct RowKey {
    values: Vec<CellValue>,
    id: String,
}

impl PartialEq for RowKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for RowKey {}

impl RowKey {
    pub fn new(values: Vec<CellValue>) -> Self {
        let id = serde_json::to_string(&values).unwrap_or_default();
        Self { values, id }
    }

    pub fn values(&self) -> &[CellValue] {
        &self.values
    }
}

impl PartialOrd for RowKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RowKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id.cmp(&other.id)
    }
}

impl Hash for RowKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowState {
    Edited,
    Deleted,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PendingChanges {
    pub session: Option<RowEditSession>,
    pub dirty: BTreeMap<RowKey, BTreeMap<String, CellEdit>>,
    pub originals: BTreeMap<RowKey, Vec<CellValue>>,
    pub new_rows: Vec<Vec<(String, CellEdit)>>,
    pub deleted: BTreeSet<RowKey>,
}

#[derive(Default)]
pub struct PendingHistory {
    current: PendingChanges,
    undo: VecDeque<PendingChanges>,
    redo: Vec<PendingChanges>,
}

pub const MAX_UNDO: usize = 100;

impl PendingHistory {
    pub fn current(&self) -> &PendingChanges {
        &self.current
    }

    pub fn is_empty(&self) -> bool {
        self.current.is_empty()
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        self.current.counts()
    }

    pub fn row_state(&self, key: &RowKey) -> Option<RowState> {
        self.current.row_state(key)
    }

    pub fn to_batch(&self, db: DatabaseType) -> StagedBatch {
        self.current.to_batch(db)
    }

    pub fn clear(&mut self) {
        self.snapshot();
        self.current.clear();
        self.redo.clear();
    }

    pub fn undo(&mut self) -> bool {
        if let Some(previous) = self.undo.pop_back() {
            self.redo.push(self.current.clone());
            self.current = previous;
            true
        } else {
            false
        }
    }

    pub fn redo(&mut self) -> bool {
        if let Some(next) = self.redo.pop() {
            self.undo.push_back(self.current.clone());
            self.current = next;
            true
        } else {
            false
        }
    }

    pub fn stage_update(
        &mut self,
        session: RowEditSession,
        key: RowKey,
        original_row: Vec<CellValue>,
        changes: BTreeMap<String, CellEdit>,
    ) {
        self.snapshot();
        self.current.session = Some(session);
        self.current.apply_update(key, original_row, changes);
    }

    /// Stage several row updates as ONE undo step (grid find & replace).
    /// Returns the number of cell values actually staged.
    pub fn stage_updates(
        &mut self,
        session: RowEditSession,
        updates: Vec<(RowKey, Vec<CellValue>, BTreeMap<String, CellEdit>)>,
    ) -> usize {
        if updates.is_empty() {
            return 0;
        }
        self.snapshot();
        self.current.session = Some(session);
        let mut staged = 0;
        for (key, original_row, changes) in updates {
            staged += self.current.apply_update(key, original_row, changes);
        }
        staged
    }

    /// Apply a transform macro to every staged string value (edits and new
    /// rows) as ONE undo step. Only plain-text, non-enum columns are touched
    /// (see [`sqlly_services::row_editing::is_plain_text_column`]): rewriting
    /// an ENUM/SET, JSON, or XML value can make it invalid, and MySQL in
    /// non-strict mode silently stores `''` for an invalid ENUM. Returns the
    /// number of values changed; NULLs, expressions, binary values, and
    /// values in other columns are left alone.
    pub fn transform_staged_strings(
        &mut self,
        transform: crate::cell_transform::CellTransform,
    ) -> usize {
        let Some(session) = self.current.session.clone() else {
            return 0;
        };
        let transformable = |column: &str| {
            session
                .columns
                .iter()
                .any(|c| c.name == column && sqlly_services::row_editing::is_plain_text_column(c))
        };
        let mut next = self.current.clone();
        let mut changed = 0;
        for edits in next.dirty.values_mut() {
            for (column, edit) in edits.iter_mut() {
                if transformable(column) {
                    changed += usize::from(transform_edit_value(transform, edit));
                }
            }
        }
        for row in &mut next.new_rows {
            for (column, edit) in row.iter_mut() {
                if transformable(column) {
                    changed += usize::from(transform_edit_value(transform, edit));
                }
            }
        }
        if changed == 0 {
            return 0;
        }
        self.snapshot();
        self.current = next;
        changed
    }

    pub fn stage_insert(&mut self, session: RowEditSession, values: Vec<(String, CellEdit)>) {
        self.snapshot();
        self.current.session = Some(session);
        self.current.new_rows.push(values);
    }

    /// Stage several inserts as ONE undo step.
    pub fn stage_inserts(&mut self, session: RowEditSession, rows: Vec<Vec<(String, CellEdit)>>) {
        if rows.is_empty() {
            return;
        }
        self.snapshot();
        self.current.session = Some(session);
        self.current.new_rows.extend(rows);
    }

    pub fn stage_delete(
        &mut self,
        session: RowEditSession,
        key: RowKey,
        original_row: Vec<CellValue>,
    ) {
        self.snapshot();
        self.current.session = Some(session);
        self.current.dirty.remove(&key);
        self.current.originals.insert(key.clone(), original_row);
        self.current.deleted.insert(key);
    }

    /// Stage several deletes as one undo step (multi-select Stage Delete).
    pub fn stage_deletes(&mut self, session: RowEditSession, rows: Vec<(RowKey, Vec<CellValue>)>) {
        if rows.is_empty() {
            return;
        }
        self.snapshot();
        self.current.session = Some(session);
        for (key, original_row) in rows {
            self.current.dirty.remove(&key);
            self.current.originals.insert(key.clone(), original_row);
            self.current.deleted.insert(key);
        }
    }

    #[allow(dead_code)] // Retained for future use.
    pub fn unstage_insert(&mut self, index: usize) {
        if index >= self.current.new_rows.len() {
            return;
        }
        self.snapshot();
        self.current.new_rows.remove(index);
        if self.current.is_empty() {
            self.current.session = None;
        }
    }

    pub fn unstage_key(&mut self, key: &RowKey) {
        self.snapshot();
        self.current.dirty.remove(key);
        self.current.deleted.remove(key);
        self.current.originals.remove(key);
        if self.current.is_empty() {
            self.current.session = None;
        }
    }

    fn snapshot(&mut self) {
        self.undo.push_back(self.current.clone());
        while self.undo.len() > MAX_UNDO {
            self.undo.pop_front();
        }
        self.redo.clear();
    }
}

impl PendingChanges {
    /// Merge one row's column edits into the staged set. Returns the number
    /// of cell values written (an edit equal to the original un-stages the
    /// column instead). Caller owns session/undo bookkeeping.
    pub fn apply_update(
        &mut self,
        key: RowKey,
        original_row: Vec<CellValue>,
        changes: BTreeMap<String, CellEdit>,
    ) -> usize {
        // Compare against THIS row's first-staged original (aligned with
        // `session.columns`), not the session's fetched row — a multi-row
        // stage (find & replace) shares one session across many rows.
        let row_original = self
            .originals
            .entry(key.clone())
            .or_insert(original_row)
            .clone();
        let mut written = 0;
        let entry = self.dirty.entry(key.clone()).or_default();
        for (column, edit) in changes {
            match &edit {
                CellEdit::Value(value) => {
                    let original = self
                        .session
                        .as_ref()
                        .and_then(|session| {
                            session
                                .columns
                                .iter()
                                .position(|col| col.name == column)
                                .and_then(|index| row_original.get(index).cloned())
                        })
                        .unwrap_or(CellValue::Null);
                    if value == &original {
                        entry.remove(&column);
                    } else {
                        entry.insert(column, edit);
                        written += 1;
                    }
                }
                CellEdit::Expression(_) => {
                    entry.insert(column, edit);
                    written += 1;
                }
            }
        }
        if entry.is_empty() {
            self.dirty.remove(&key);
        }
        written
    }

    pub fn is_empty(&self) -> bool {
        self.dirty.is_empty() && self.new_rows.is_empty() && self.deleted.is_empty()
    }

    pub fn counts(&self) -> (usize, usize, usize) {
        (self.dirty.len(), self.new_rows.len(), self.deleted.len())
    }

    pub fn row_state(&self, key: &RowKey) -> Option<RowState> {
        if self.deleted.contains(key) {
            return Some(RowState::Deleted);
        }
        if self.dirty.contains_key(key) {
            return Some(RowState::Edited);
        }
        None
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn to_batch(&self, db: DatabaseType) -> StagedBatch {
        let Some(session) = &self.session else {
            return StagedBatch::default();
        };
        let target = session.target.clone();
        let key_columns = session.key_columns.clone();
        let mut ops = Vec::new();

        for key in &self.deleted {
            if let Some(original_row) = self.originals.get(key) {
                let request = build_delete_request(session, key, original_row);
                let full_row = session
                    .columns
                    .iter()
                    .zip(original_row.iter())
                    .map(|(column, value)| (column.name.clone(), value.clone()))
                    .collect();
                ops.push(StagedOp::Delete {
                    request,
                    original_row: full_row,
                });
            }
        }

        for (key, changes) in &self.dirty {
            if let Some(original_row) = self.originals.get(key)
                && let Some(request) = build_update_request(session, key, original_row, changes)
            {
                ops.push(StagedOp::Update(request));
            }
        }

        for values in &self.new_rows {
            let omit_default = db.connection_kind() == ConnectionKind::SqliteFile;
            let mut value_pairs = Vec::new();
            let mut expressions = Vec::new();
            for (name, edit) in values {
                match edit {
                    CellEdit::Value(value) => value_pairs.push((name.clone(), value.clone())),
                    CellEdit::Expression(ColumnExpression::Default) if omit_default => {}
                    CellEdit::Expression(expr) => expressions.push((name.clone(), *expr)),
                }
            }
            ops.push(StagedOp::Insert(InsertRowRequest {
                target: target.clone(),
                values: value_pairs,
                expressions,
            }));
        }

        StagedBatch {
            target: Some(target),
            key_columns,
            ops,
        }
    }
}

fn build_update_request(
    session: &RowEditSession,
    key: &RowKey,
    original_row: &[CellValue],
    changes: &BTreeMap<String, CellEdit>,
) -> Option<UpdateRowRequest> {
    if changes.is_empty() || session.key_columns.is_empty() {
        return None;
    }
    let key_pairs = key_values(session, key);
    let mut change_pairs = Vec::new();
    let mut expressions = Vec::new();
    let mut original_values = Vec::new();
    for (name, edit) in changes {
        match edit {
            CellEdit::Value(value) => change_pairs.push((name.clone(), value.clone())),
            CellEdit::Expression(expr) => expressions.push((name.clone(), *expr)),
        }
        if let Some(index) = session
            .columns
            .iter()
            .position(|column| &column.name == name)
            && let Some(original) = original_row.get(index)
        {
            original_values.push((name.clone(), original.clone()));
        }
    }
    Some(UpdateRowRequest {
        target: session.target.clone(),
        key: key_pairs,
        rowversion: row_rowversion(session, original_row),
        changes: change_pairs,
        original_values,
        expressions,
    })
}

fn build_delete_request(
    session: &RowEditSession,
    key: &RowKey,
    original_row: &[CellValue],
) -> DeleteRowRequest {
    let original_values = session
        .columns
        .iter()
        .zip(original_row.iter())
        .filter(|(column, _)| {
            !session.key_columns.contains(&column.name)
                && !column.is_rowversion
                && concurrency_comparable(&column.data_type)
        })
        .map(|(column, value)| (column.name.clone(), value.clone()))
        .collect();
    DeleteRowRequest {
        target: session.target.clone(),
        key: key_values(session, key),
        rowversion: row_rowversion(session, original_row),
        original_values,
    }
}

/// Build a [`RowKey`] from a result-set row using the session's key columns,
/// each resolved through provenance (`result_ordinal`) rather than by result
/// column name: with `SELECT id AS item_id`, a name lookup finds nothing and
/// every row would collapse onto one `[NULL]` key. `None` when any key
/// column is not projected (or the row is short), so callers take their
/// "no key" path instead of staging against a bogus key. `_columns` (the
/// result's column list) is intentionally not consulted: result names are
/// not a safe key-column identity.
#[must_use]
pub fn row_key_from_result_row(
    session: &RowEditSession,
    _columns: &[ResultColumn],
    row: &[CellValue],
) -> Option<RowKey> {
    if session.key_columns.is_empty() {
        return None;
    }
    let values = session
        .key_columns
        .iter()
        .map(|name| {
            let ordinal = session
                .columns
                .iter()
                .find(|column| &column.name == name)?
                .result_ordinal?;
            row.get(ordinal).cloned()
        })
        .collect::<Option<Vec<CellValue>>>()?;
    Some(RowKey::new(values))
}

/// The result-set column index that projects table column `name`, through
/// provenance only (see [`row_key_from_result_row`]). `None` when the column
/// is not projected — e.g. the result shows an aliased expression that merely
/// shares the column's name.
#[must_use]
pub fn result_index_for_table_column(session: &RowEditSession, name: &str) -> Option<usize> {
    session
        .columns
        .iter()
        .find(|column| column.name == name)?
        .result_ordinal
}

fn key_values(session: &RowEditSession, key: &RowKey) -> Vec<(String, CellValue)> {
    session
        .key_columns
        .iter()
        .zip(key.values().iter())
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn concurrency_comparable(data_type: &str) -> bool {
    !matches!(
        data_type.to_ascii_lowercase().as_str(),
        "text"
            | "ntext"
            | "image"
            | "xml"
            | "geography"
            | "geometry"
            | "hierarchyid"
            | "sql_variant"
            | "json"
    )
}

/// The rowversion guard for one staged row, read from that row's own
/// original (aligned with `session.columns`) rather than the session's
/// fetched row, so multi-row stages guard each row with its own version. A
/// NULL value means the version was not projected for this row — fall back
/// to original-value concurrency instead of a guard that can never match.
fn row_rowversion(
    session: &RowEditSession,
    original_row: &[CellValue],
) -> Option<(String, CellValue)> {
    let name = session.rowversion_column.as_ref()?;
    let ordinal = session.columns.iter().position(|c| &c.name == name)?;
    match original_row.get(ordinal) {
        Some(CellValue::Null) | None => None,
        Some(value) => Some((name.clone(), value.clone())),
    }
}

/// Apply a transform macro to one staged edit when it holds string text.
fn transform_edit_value(
    transform: crate::cell_transform::CellTransform,
    edit: &mut CellEdit,
) -> bool {
    let CellEdit::Value(CellValue::String(text)) = edit else {
        return false;
    };
    let transformed = transform.apply(text);
    if transformed == *text {
        return false;
    }
    *text = transformed;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::row_editing::{RowEditColumn, TableTarget};

    fn sample_session() -> RowEditSession {
        RowEditSession {
            target: TableTarget {
                schema: "dbo".to_string(),
                table: "Items".to_string(),
            },
            key_columns: vec!["id".to_string()],
            rowversion_column: None,
            columns: vec![
                RowEditColumn {
                    name: "id".to_string(),
                    data_type: "int".to_string(),
                    char_max_length: None,
                    precision: None,
                    scale: None,
                    nullable: false,
                    is_primary_key: true,
                    is_identity: true,
                    is_computed: false,
                    is_rowversion: false,
                    default_definition: None,
                    result_ordinal: Some(0),

                    enum_values: Vec::new(),
                },
                RowEditColumn {
                    name: "name".to_string(),
                    data_type: "nvarchar".to_string(),
                    char_max_length: Some(100),
                    precision: None,
                    scale: None,
                    nullable: true,
                    is_primary_key: false,
                    is_identity: false,
                    is_computed: false,
                    is_rowversion: false,
                    default_definition: None,
                    result_ordinal: Some(1),

                    enum_values: Vec::new(),
                },
            ],
            original_row: vec![CellValue::I64(1), CellValue::String("old".to_string())],
            foreign_keys: Vec::new(),
            fk_display: Vec::new(),
            check_constraints: Vec::new(),
            incoming_foreign_keys: Vec::new(),
        }
    }

    fn key() -> RowKey {
        RowKey::new(vec![CellValue::I64(1)])
    }

    fn name_change(value: CellValue) -> BTreeMap<String, CellEdit> {
        let mut changes = BTreeMap::new();
        changes.insert("name".to_string(), CellEdit::Value(value));
        changes
    }

    #[test]
    fn stage_update_merges_columns_per_key() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            name_change(CellValue::String("a".to_string())),
        );
        history.stage_update(
            session,
            key(),
            vec![CellValue::I64(1), CellValue::Null],
            name_change(CellValue::String("b".to_string())),
        );
        assert_eq!(
            history.current().dirty[&key()]["name"],
            CellEdit::Value(CellValue::String("b".to_string()))
        );
    }

    #[test]
    fn reverting_to_original_removes_column_then_key() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            name_change(CellValue::String("new".to_string())),
        );
        history.stage_update(
            session,
            key(),
            vec![CellValue::I64(1), CellValue::Null],
            name_change(CellValue::String("old".to_string())),
        );
        assert!(!history.current().dirty.contains_key(&key()));
    }

    #[test]
    fn stage_delete_drops_dirty_for_same_key() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            name_change(CellValue::String("x".to_string())),
        );
        history.stage_delete(session, key(), vec![CellValue::I64(1), CellValue::Null]);
        assert!(!history.current().dirty.contains_key(&key()));
        assert!(history.current().deleted.contains(&key()));
    }

    #[test]
    fn stage_inserts_is_one_undo_step() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        history.stage_inserts(
            session,
            vec![
                vec![(
                    "name".to_string(),
                    CellEdit::Value(CellValue::String("a".into())),
                )],
                vec![(
                    "name".to_string(),
                    CellEdit::Value(CellValue::String("b".into())),
                )],
                vec![(
                    "name".to_string(),
                    CellEdit::Value(CellValue::String("c".into())),
                )],
            ],
        );
        assert_eq!(history.counts().1, 3);
        assert!(history.undo());
        assert_eq!(history.counts().1, 0);
    }

    #[test]
    fn stage_deletes_is_one_undo_step() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        let key_b = RowKey::new(vec![CellValue::I64(2)]);
        history.stage_deletes(
            session,
            vec![
                (key(), vec![CellValue::I64(1), CellValue::Null]),
                (key_b.clone(), vec![CellValue::I64(2), CellValue::Null]),
            ],
        );
        assert_eq!(history.current().deleted.len(), 2);
        assert!(history.undo());
        assert!(history.current().deleted.is_empty());
        assert!(history.redo());
        assert!(history.current().deleted.contains(&key()));
        assert!(history.current().deleted.contains(&key_b));
    }

    #[test]
    fn undo_redo_round_trip() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            name_change(CellValue::String("x".to_string())),
        );
        assert!(!history.is_empty());
        assert!(history.undo());
        assert!(history.is_empty());
        assert!(history.redo());
        assert!(!history.is_empty());
    }

    #[test]
    fn new_op_after_undo_clears_redo() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            name_change(CellValue::String("x".to_string())),
        );
        history.undo();
        history.stage_update(
            session,
            key(),
            vec![CellValue::I64(1), CellValue::Null],
            name_change(CellValue::String("y".to_string())),
        );
        assert!(history.redo.is_empty());
    }

    #[test]
    fn undo_stack_capped_at_100() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        for index in 0..=MAX_UNDO {
            history.stage_update(
                session.clone(),
                key(),
                session.original_row.clone(),
                name_change(CellValue::String(format!("v{index}"))),
            );
        }
        assert!(history.undo.len() <= MAX_UNDO);
    }

    #[test]
    fn to_batch_builds_requests_with_key_rowversion_and_originals() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            name_change(CellValue::String("new".to_string())),
        );
        let batch = history.to_batch(DatabaseType::SqlServer);
        assert_eq!(batch.ops.len(), 1);
        match &batch.ops[0] {
            StagedOp::Update(request) => {
                assert_eq!(request.key[0].1, CellValue::I64(1));
                assert_eq!(
                    request.original_values[0].1,
                    CellValue::String("old".to_string())
                );
            }
            _ => panic!("expected update"),
        }
    }

    #[test]
    fn row_state_reports_edited_and_deleted() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            name_change(CellValue::String("x".to_string())),
        );
        assert_eq!(history.row_state(&key()), Some(RowState::Edited));
        history.stage_delete(session, key(), vec![CellValue::I64(1), CellValue::Null]);
        assert_eq!(history.row_state(&key()), Some(RowState::Deleted));
    }

    #[test]
    fn expression_edit_is_always_dirty() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        let mut changes = BTreeMap::new();
        changes.insert(
            "name".to_string(),
            CellEdit::Expression(ColumnExpression::Default),
        );
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            changes,
        );
        assert_eq!(
            history.current().dirty[&key()]["name"],
            CellEdit::Expression(ColumnExpression::Default)
        );
        history.stage_update(
            session,
            key(),
            vec![CellValue::I64(1), CellValue::Null],
            name_change(CellValue::String("old".to_string())),
        );
        assert!(
            !history.current().dirty.contains_key(&key()),
            "a value equal to the original still clears the expression"
        );
    }

    #[test]
    fn to_batch_splits_values_and_expressions() {
        let mut history = PendingHistory::default();
        let mut session = sample_session();
        session.columns.push(RowEditColumn {
            name: "ts".to_string(),
            data_type: "datetime2".to_string(),
            char_max_length: None,
            precision: None,
            scale: None,
            nullable: true,
            is_primary_key: false,
            is_identity: false,
            is_computed: false,
            is_rowversion: false,
            default_definition: None,
            result_ordinal: Some(2),
            enum_values: Vec::new(),
        });
        session.original_row.push(CellValue::Null);
        let mut changes = BTreeMap::new();
        changes.insert(
            "name".to_string(),
            CellEdit::Value(CellValue::String("new".to_string())),
        );
        changes.insert(
            "ts".to_string(),
            CellEdit::Expression(ColumnExpression::ServerTime),
        );
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            changes,
        );
        let batch = history.to_batch(DatabaseType::SqlServer);
        match &batch.ops[0] {
            StagedOp::Update(request) => {
                assert_eq!(
                    request.changes,
                    vec![("name".to_string(), CellValue::String("new".to_string()))]
                );
                assert_eq!(
                    request.expressions,
                    vec![("ts".to_string(), ColumnExpression::ServerTime)]
                );
                assert!(
                    request
                        .original_values
                        .iter()
                        .any(|(name, _)| name == "name")
                );
                assert!(request.original_values.iter().any(|(name, _)| name == "ts"));
            }
            _ => panic!("expected update"),
        }
    }

    #[test]
    fn sqlite_insert_default_omits_column() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        history.stage_insert(
            session,
            vec![
                ("id".to_string(), CellEdit::Value(CellValue::I64(2))),
                (
                    "name".to_string(),
                    CellEdit::Expression(ColumnExpression::Default),
                ),
            ],
        );
        let sqlite = history.to_batch(DatabaseType::Sqlite);
        match &sqlite.ops[0] {
            StagedOp::Insert(request) => {
                assert_eq!(request.values, vec![("id".to_string(), CellValue::I64(2))]);
                assert!(request.expressions.is_empty());
            }
            _ => panic!("expected insert"),
        }
        let postgres = history.to_batch(DatabaseType::Postgres);
        match &postgres.ops[0] {
            StagedOp::Insert(request) => {
                assert_eq!(request.values, vec![("id".to_string(), CellValue::I64(2))]);
                assert_eq!(
                    request.expressions,
                    vec![("name".to_string(), ColumnExpression::Default)]
                );
            }
            _ => panic!("expected insert"),
        }
    }

    #[test]
    fn undo_redo_keeps_expressions() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        let mut changes = BTreeMap::new();
        changes.insert(
            "name".to_string(),
            CellEdit::Expression(ColumnExpression::ServerTime),
        );
        history.stage_update(
            session,
            key(),
            vec![CellValue::I64(1), CellValue::String("old".to_string())],
            changes,
        );
        assert!(history.undo());
        assert!(history.is_empty());
        assert!(history.redo());
        assert_eq!(
            history.current().dirty[&key()]["name"],
            CellEdit::Expression(ColumnExpression::ServerTime)
        );
    }

    #[test]
    fn stage_updates_is_one_undo_step() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        let key_b = RowKey::new(vec![CellValue::I64(2)]);
        let staged = history.stage_updates(
            session.clone(),
            vec![
                (
                    key(),
                    session.original_row.clone(),
                    name_change(CellValue::String("a".to_string())),
                ),
                (
                    key_b.clone(),
                    vec![CellValue::I64(2), CellValue::Null],
                    name_change(CellValue::String("b".to_string())),
                ),
            ],
        );
        assert_eq!(staged, 2);
        assert_eq!(history.current().dirty.len(), 2);
        assert!(history.undo(), "both rows revert together");
        assert!(history.current().dirty.is_empty());
        assert!(history.redo());
        assert_eq!(
            history.current().dirty[&key_b]["name"],
            CellEdit::Value(CellValue::String("b".to_string()))
        );
    }

    #[test]
    fn stage_updates_compares_each_row_to_its_own_original() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        // Row 2's original name is "two"; staging "old" (row 1's original)
        // for it is a real change, while staging "two" is a no-op.
        let key_b = RowKey::new(vec![CellValue::I64(2)]);
        let original_b = vec![CellValue::I64(2), CellValue::String("two".to_string())];
        let staged = history.stage_updates(
            session.clone(),
            vec![(
                key_b.clone(),
                original_b.clone(),
                name_change(CellValue::String("old".to_string())),
            )],
        );
        assert_eq!(staged, 1);
        let staged = history.stage_updates(
            session,
            vec![(
                key_b,
                original_b,
                name_change(CellValue::String("two".to_string())),
            )],
        );
        assert_eq!(staged, 0, "reverting to the row's own original un-stages");
        assert!(history.current().dirty.is_empty());
    }

    #[test]
    fn multi_row_updates_guard_each_row_with_its_own_rowversion() {
        let mut session = sample_session();
        session.rowversion_column = Some("rv".to_string());
        session.columns.push(RowEditColumn {
            name: "rv".to_string(),
            data_type: "rowversion".to_string(),
            char_max_length: None,
            precision: None,
            scale: None,
            nullable: false,
            is_primary_key: false,
            is_identity: false,
            is_computed: false,
            is_rowversion: true,
            default_definition: None,
            result_ordinal: None,
            enum_values: Vec::new(),
        });
        session.original_row.push(CellValue::Bytes(vec![1]));
        let mut history = PendingHistory::default();
        history.stage_updates(
            session.clone(),
            vec![
                (
                    key(),
                    session.original_row.clone(),
                    name_change(CellValue::String("a".to_string())),
                ),
                (
                    RowKey::new(vec![CellValue::I64(2)]),
                    // Rowversion not projected for this row: unknown.
                    vec![
                        CellValue::I64(2),
                        CellValue::String("two".to_string()),
                        CellValue::Null,
                    ],
                    name_change(CellValue::String("b".to_string())),
                ),
            ],
        );
        let batch = history.current().to_batch(DatabaseType::SqlServer);
        let updates: Vec<_> = batch
            .ops
            .iter()
            .filter_map(|op| match op {
                StagedOp::Update(request) => Some(request),
                _ => None,
            })
            .collect();
        assert_eq!(updates.len(), 2);
        let row1 = updates
            .iter()
            .find(|r| r.key == vec![("id".to_string(), CellValue::I64(1))])
            .expect("row 1");
        assert_eq!(
            row1.rowversion,
            Some(("rv".to_string(), CellValue::Bytes(vec![1])))
        );
        let row2 = updates
            .iter()
            .find(|r| r.key == vec![("id".to_string(), CellValue::I64(2))])
            .expect("row 2");
        assert_eq!(row2.rowversion, None, "unknown version falls back");
        assert_eq!(
            row2.original_values,
            vec![("name".to_string(), CellValue::String("two".to_string()))],
            "row 2 is guarded by its own original value"
        );
    }

    #[test]
    fn stage_updates_skips_noop_but_keeps_undo_step() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        // Value equal to the original un-stages the column; the key never
        // becomes dirty.
        let staged = history.stage_updates(
            session.clone(),
            vec![(
                key(),
                session.original_row.clone(),
                name_change(CellValue::String("old".to_string())),
            )],
        );
        assert_eq!(staged, 0);
        assert!(history.is_empty());
    }

    #[test]
    fn transform_staged_strings_rewrites_edits_and_inserts_as_one_step() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            name_change(CellValue::String("alice".to_string())),
        );
        history.stage_insert(
            session,
            vec![
                (
                    "name".to_string(),
                    CellEdit::Value(CellValue::String("bob".to_string())),
                ),
                (
                    "id".to_string(),
                    CellEdit::Expression(ColumnExpression::Default),
                ),
            ],
        );
        let changed = history.transform_staged_strings(crate::cell_transform::CellTransform::Upper);
        assert_eq!(changed, 2, "expression and id edits are untouched");
        assert_eq!(
            history.current().dirty[&key()]["name"],
            CellEdit::Value(CellValue::String("ALICE".to_string()))
        );
        assert_eq!(
            history.current().new_rows[0][0].1,
            CellEdit::Value(CellValue::String("BOB".to_string()))
        );
        assert!(history.undo(), "the whole transform is one undo step");
        assert_eq!(
            history.current().dirty[&key()]["name"],
            CellEdit::Value(CellValue::String("alice".to_string()))
        );
    }

    #[test]
    fn transform_staged_strings_noop_when_nothing_changes() {
        let mut history = PendingHistory::default();
        let session = sample_session();
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            name_change(CellValue::String("ALICE".to_string())),
        );
        let changed = history.transform_staged_strings(crate::cell_transform::CellTransform::Upper);
        assert_eq!(changed, 0);
        assert_eq!(
            history.undo.len(),
            1,
            "a no-op transform must not push an undo step"
        );
    }

    fn result_columns(names: &[&str]) -> Vec<ResultColumn> {
        names
            .iter()
            .map(|name| ResultColumn {
                name: (*name).to_string(),
                data_type: "x".to_string(),
                nullable: true,
            })
            .collect()
    }

    #[test]
    fn row_key_resolves_an_aliased_key_through_provenance() {
        // SELECT id AS item_id, name — the key column is aliased; a name
        // lookup would miss it and collapse every row onto one NULL key.
        let session = sample_session();
        let columns = result_columns(&["item_id", "name"]);
        let first = row_key_from_result_row(
            &session,
            &columns,
            &[CellValue::I64(7), CellValue::String("a".to_string())],
        );
        let second = row_key_from_result_row(
            &session,
            &columns,
            &[CellValue::I64(8), CellValue::String("b".to_string())],
        );
        assert_eq!(first, Some(RowKey::new(vec![CellValue::I64(7)])));
        assert_eq!(second, Some(RowKey::new(vec![CellValue::I64(8)])));
        assert_ne!(first, second, "distinct rows keep distinct keys");
    }

    #[test]
    fn row_key_is_none_when_a_key_column_is_not_projected() {
        let mut session = sample_session();
        session.columns[0].result_ordinal = None;
        // A result column that merely shares the key's name is not the key.
        let columns = result_columns(&["id", "name"]);
        assert_eq!(
            row_key_from_result_row(
                &session,
                &columns,
                &[CellValue::I64(1), CellValue::String("a".to_string())],
            ),
            None
        );
        // A short row cannot yield a key either.
        let session = sample_session();
        assert_eq!(row_key_from_result_row(&session, &columns, &[]), None);
    }

    #[test]
    fn result_index_for_table_column_uses_provenance() {
        let mut session = sample_session();
        assert_eq!(result_index_for_table_column(&session, "name"), Some(1));
        session.columns[1].result_ordinal = None;
        assert_eq!(result_index_for_table_column(&session, "name"), None);
        assert_eq!(result_index_for_table_column(&session, "missing"), None);
    }

    #[test]
    fn transform_staged_strings_skips_enum_and_structured_columns() {
        let mut session = sample_session();
        let mut status = session.columns[1].clone();
        status.name = "status".to_string();
        status.data_type = "enum".to_string();
        status.enum_values = vec!["open".to_string(), "closed".to_string()];
        status.result_ordinal = Some(2);
        let mut doc = session.columns[1].clone();
        doc.name = "doc".to_string();
        doc.data_type = "json".to_string();
        doc.result_ordinal = Some(3);
        session.columns.push(status);
        session.columns.push(doc);
        session
            .original_row
            .push(CellValue::String("open".to_string()));
        session
            .original_row
            .push(CellValue::String("{}".to_string()));
        let mut changes = name_change(CellValue::String("alice".to_string()));
        changes.insert(
            "status".to_string(),
            CellEdit::Value(CellValue::String("closed".to_string())),
        );
        changes.insert(
            "doc".to_string(),
            CellEdit::Value(CellValue::String("{\"a\":true}".to_string())),
        );
        let mut history = PendingHistory::default();
        history.stage_update(
            session.clone(),
            key(),
            session.original_row.clone(),
            changes,
        );
        let changed = history.transform_staged_strings(crate::cell_transform::CellTransform::Upper);
        assert_eq!(changed, 1, "only the plain-text column is rewritten");
        let dirty = &history.current().dirty[&key()];
        assert_eq!(
            dirty["name"],
            CellEdit::Value(CellValue::String("ALICE".to_string()))
        );
        assert_eq!(
            dirty["status"],
            CellEdit::Value(CellValue::String("closed".to_string())),
            "an ENUM value must not become an invalid 'CLOSED'"
        );
        assert_eq!(
            dirty["doc"],
            CellEdit::Value(CellValue::String("{\"a\":true}".to_string()))
        );
    }
}
