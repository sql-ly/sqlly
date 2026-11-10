//! Fold-aware name index over the live catalog, built once per validation.

use std::collections::HashMap;

use sqlly_lang::sqlmodel::{Dialect, ObjectRef, fold_identifier};

use crate::catalog::prompt::GroundedObject;
use crate::catalog::store::{CatalogResult, CatalogStore};

/// Outcome of resolving a table reference against the catalog.
pub(super) enum TableResolution {
    Resolved(usize),
    Unresolved,
    Ambiguous(Vec<String>),
}

/// One catalog object, indexed for fold-aware name/column lookup.
pub(super) struct IndexEntry {
    pub object_key: String,
    pub namespace: Option<String>,
    pub name: String,
    pub object_type: String,
    /// Folded column names.
    pub columns: Vec<String>,
    /// Folded primary-key column names, in key order. Empty when unknown.
    pub primary_key: Vec<String>,
}

impl IndexEntry {
    pub(super) fn display_name(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{ns}.{}", self.name),
            None => self.name.clone(),
        }
    }

    pub(super) fn grounded(&self) -> GroundedObject {
        GroundedObject {
            object_key: self.object_key.clone(),
            namespace: self.namespace.clone(),
            name: self.name.clone(),
            object_type: self.object_type.clone(),
        }
    }
}

/// A fold-aware index over the live catalog, built once per validation.
pub(super) struct CatalogNameIndex {
    pub entries: Vec<IndexEntry>,
    by_name: HashMap<String, Vec<usize>>,
    by_qualified: HashMap<String, Vec<usize>>,
}

impl CatalogNameIndex {
    pub(super) fn load(store: &CatalogStore, dialect: Dialect) -> CatalogResult<Self> {
        let rows = store.live_validation_index_rows()?;
        let mut entries = Vec::with_capacity(rows.len());
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_qualified: HashMap<String, Vec<usize>> = HashMap::new();

        for row in rows {
            let columns: Vec<String> = row
                .columns
                .iter()
                .map(|c| fold_identifier(c, dialect, false))
                .collect();
            let primary_key: Vec<String> = row
                .primary_key
                .iter()
                .map(|c| fold_identifier(c, dialect, false))
                .collect();
            let idx = entries.len();
            let name_fold = fold_identifier(&row.name, dialect, false);
            by_name.entry(name_fold.clone()).or_default().push(idx);
            if let Some(ns) = &row.namespace {
                let key = format!("{}.{}", fold_identifier(ns, dialect, false), name_fold);
                by_qualified.entry(key).or_default().push(idx);
            }

            entries.push(IndexEntry {
                object_key: row.object_key,
                namespace: row.namespace,
                name: row.name,
                object_type: row.kind.as_str().to_string(),
                columns,
                primary_key,
            });
        }

        Ok(Self {
            entries,
            by_name,
            by_qualified,
        })
    }

    /// An empty catalog (never synced for this connection) can't tell a real
    /// table from a typo — callers skip name resolution entirely rather than
    /// flood the editor with false errors.
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn resolve_table(&self, reference: &ObjectRef) -> TableResolution {
        self.resolve_table_with_default(reference, None)
    }

    /// [`Self::resolve_table`] with a default schema: an unqualified name that
    /// matches objects in several schemas resolves to the default schema's
    /// object when exactly one candidate lives there.
    pub(super) fn resolve_table_with_default(
        &self,
        reference: &ObjectRef,
        default_schema_fold: Option<&str>,
    ) -> TableResolution {
        let name_fold = reference.name.fold_key();
        let candidates = if let Some(ns) = &reference.namespace {
            let key = format!("{}.{}", ns.fold_key(), name_fold);
            // Fall back to the bare name when the qualified key misses: the
            // store's namespace may be absent or spelled differently than the
            // live database (it is synced out-of-band), and completion resolves
            // by bare name in that case. Better to miss a wrong-schema typo
            // than to flag a real table as unknown.
            self.by_qualified
                .get(&key)
                .or_else(|| self.by_name.get(name_fold))
        } else {
            self.by_name.get(name_fold)
        };

        match candidates {
            None => TableResolution::Unresolved,
            Some(idxs) if idxs.len() == 1 => TableResolution::Resolved(idxs[0]),
            Some(idxs) => {
                if reference.namespace.is_none()
                    && let Some(default_schema) = default_schema_fold
                {
                    let in_default: Vec<usize> = idxs
                        .iter()
                        .copied()
                        .filter(|&i| {
                            self.entries[i]
                                .namespace
                                .as_deref()
                                .is_some_and(|ns| ns.eq_ignore_ascii_case(default_schema))
                        })
                        .collect();
                    if let [only] = in_default.as_slice() {
                        return TableResolution::Resolved(*only);
                    }
                }
                TableResolution::Ambiguous(
                    idxs.iter()
                        .map(|&i| self.entries[i].object_key.clone())
                        .collect(),
                )
            }
        }
    }
}
