//! Per-thread memos for the pure functions of a cell's source that
//! tokenize it (masking, normalizing), so rebuilding a notebook's graph and
//! fingerprints after one cell changed re-reads only that cell.

use std::cell::RefCell;
use std::collections::HashMap;

use sqlly_lang::sqlmodel::DatabaseType;

/// Source bytes a memo keeps before it starts over.
const CAPACITY_BYTES: usize = 8 << 20;

#[derive(Default)]
pub(crate) struct SourceMemo {
    bytes: usize,
    entries: HashMap<DatabaseType, HashMap<String, String>>,
}

impl SourceMemo {
    /// `compute()` for `source` read as `db`, from the memo when it is
    /// there.
    pub(crate) fn get(
        memo: &RefCell<SourceMemo>,
        db: DatabaseType,
        source: &str,
        compute: impl FnOnce() -> String,
    ) -> String {
        if let Some(found) = memo.borrow().entries.get(&db).and_then(|m| m.get(source)) {
            return found.clone();
        }
        let value = compute();
        let mut memo = memo.borrow_mut();
        let size = source.len() + value.len();
        if memo.bytes + size > CAPACITY_BYTES {
            memo.entries.clear();
            memo.bytes = 0;
        }
        memo.bytes += size;
        memo.entries
            .entry(db)
            .or_default()
            .insert(source.to_string(), value.clone());
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_memo_computes_once_per_source_and_dialect() {
        let memo = RefCell::new(SourceMemo::default());
        let mut calls = 0;
        let mut get = |db, source: &str| {
            SourceMemo::get(&memo, db, source, || {
                calls += 1;
                source.to_uppercase()
            })
        };
        assert_eq!(get(DatabaseType::SqlServer, "select 1"), "SELECT 1");
        assert_eq!(get(DatabaseType::SqlServer, "select 1"), "SELECT 1");
        assert_eq!(get(DatabaseType::Postgres, "select 1"), "SELECT 1");
        assert_eq!(get(DatabaseType::SqlServer, "select 2"), "SELECT 2");
        assert_eq!(calls, 3);
    }
}
