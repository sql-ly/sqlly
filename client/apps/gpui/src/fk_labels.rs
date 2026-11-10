//! Foreign-key display labels for the result grid: pure cache, chunking, and
//! request planning. Async fetch and GPUI wiring live in `results/mod.rs`.

use std::collections::{HashMap, HashSet};

use sqlly_core::{CellValue, ResultColumn};
use sqlly_services::row_editing::{FkLabelRequest, ForeignKeySpec, TableTarget};

pub const CHUNK: usize = 500;
pub const CACHE_LIMIT: usize = 20_000;

/// One resolvable FK on the displayed page: local grid column, referenced table, key + label columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelledFk {
    pub column_index: usize,
    pub referenced: TableTarget,
    pub key_column: String,
    pub display_column: String,
}

/// FKs on `session.foreign_keys` that are single-column, have a display column
/// (user override from `overrides` wins over `fk.display_column`), and whose local column is in `columns`.
pub fn labelled_fks(
    session_fks: &[ForeignKeySpec],
    columns: &[ResultColumn],
    overrides: &HashMap<String, String>,
) -> Vec<LabelledFk> {
    let mut out = Vec::new();
    for fk in session_fks {
        if fk.column_pairs.len() != 1 {
            continue;
        }
        let (local_col, ref_col) = &fk.column_pairs[0];
        let Some(column_index) = columns.iter().position(|c| &c.name == local_col) else {
            continue;
        };
        let display_column = overrides
            .get(local_col)
            .cloned()
            .or_else(|| fk.display_column.clone());
        let Some(display_column) = display_column else {
            continue;
        };
        out.push(LabelledFk {
            column_index,
            referenced: fk.referenced.clone(),
            key_column: ref_col.clone(),
            display_column,
        });
    }
    out
}

/// Distinct non-NULL keys in `rows[*][fk.column_index]` not already in `cache`, split into CHUNK-sized requests.
pub fn plan_requests(
    fk: &LabelledFk,
    rows: &[Vec<CellValue>],
    cache: &LabelCache,
) -> Vec<FkLabelRequest> {
    let mut distinct: Vec<CellValue> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for row in rows {
        let Some(value) = row.get(fk.column_index) else {
            continue;
        };
        if matches!(value, CellValue::Null) {
            continue;
        }
        let key = LabelCache::cache_key(fk, value);
        if cache.contains(&key) || !seen.insert(key) {
            continue;
        }
        distinct.push(value.clone());
    }
    if distinct.is_empty() {
        return Vec::new();
    }
    distinct
        .chunks(CHUNK)
        .map(|chunk| FkLabelRequest {
            referenced: fk.referenced.clone(),
            key_column: fk.key_column.clone(),
            display_column: fk.display_column.clone(),
            keys: chunk.to_vec(),
        })
        .collect()
}

/// LRU keyed by `(schema, table, key-column, key text)` → label.
#[derive(Debug, Default)]
pub struct LabelCache {
    map: HashMap<String, String>,
    order: Vec<String>,
}

impl LabelCache {
    pub fn get(&mut self, key: &str) -> Option<&str> {
        if self.map.contains_key(key) {
            Self::lru_touch(&mut self.order, key);
            self.map.get(key).map(String::as_str)
        } else {
            None
        }
    }

    fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    pub fn insert_all(&mut self, prefix: &str, labels: Vec<(CellValue, String)>) {
        for (key, label) in labels {
            let cache_key = format!("{prefix}|{}", crate::result_export::cell_display(&key));
            self.map.insert(cache_key.clone(), label);
            Self::lru_touch(&mut self.order, &cache_key);
        }
        Self::lru_evict(&mut self.map, &mut self.order, CACHE_LIMIT);
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }

    pub fn cache_key(fk: &LabelledFk, key: &CellValue) -> String {
        format!(
            "{}|{}",
            cache_prefix(fk),
            crate::result_export::cell_display(key)
        )
    }

    fn lru_touch(order: &mut Vec<String>, key: &str) {
        if order.last().is_some_and(|k| k == key) {
            return;
        }
        if let Some(i) = order.iter().position(|k| k == key) {
            let k = order.remove(i);
            order.push(k);
        } else {
            order.push(key.to_string());
        }
    }

    fn lru_evict(map: &mut HashMap<String, String>, order: &mut Vec<String>, limit: usize) {
        while map.len() > limit {
            let Some(oldest) = order.first().cloned() else {
                break;
            };
            order.remove(0);
            map.remove(&oldest);
        }
    }
}

fn cache_prefix(fk: &LabelledFk) -> String {
    format!(
        "{}|{}|{}",
        fk.referenced.schema, fk.referenced.table, fk.key_column
    )
}

/// The per-column value→label map handed to the grid: raw cell text → label.
pub fn value_labels_for(
    fk: &LabelledFk,
    rows: &[Vec<CellValue>],
    cache: &mut LabelCache,
) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for row in rows {
        let Some(value) = row.get(fk.column_index) else {
            continue;
        };
        if matches!(value, CellValue::Null) {
            continue;
        }
        let key = LabelCache::cache_key(fk, value);
        if let Some(label) = cache.get(&key) {
            out.insert(crate::result_export::cell_display(value), label.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::row_editing::TableTarget;

    fn customer_fk(display: Option<&str>) -> ForeignKeySpec {
        ForeignKeySpec {
            constraint_name: "fk_orders_customer".into(),
            referenced: TableTarget {
                schema: "public".into(),
                table: "customer".into(),
            },
            column_pairs: vec![("customer_id".into(), "id".into())],
            display_column: display.map(str::to_string),
        }
    }

    fn columns() -> Vec<ResultColumn> {
        vec![
            ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "customer_id".into(),
                data_type: "int".into(),
                nullable: true,
            },
        ]
    }

    fn labelled() -> LabelledFk {
        LabelledFk {
            column_index: 1,
            referenced: TableTarget {
                schema: "public".into(),
                table: "customer".into(),
            },
            key_column: "id".into(),
            display_column: "name".into(),
        }
    }

    #[test]
    fn labelled_fks_skips_composite_and_missing_display() {
        let composite = ForeignKeySpec {
            constraint_name: "fk".into(),
            referenced: TableTarget {
                schema: "s".into(),
                table: "t".into(),
            },
            column_pairs: vec![("a".into(), "x".into()), ("b".into(), "y".into())],
            display_column: Some("label".into()),
        };
        let no_display = customer_fk(None);
        let cols = columns();
        assert!(labelled_fks(&[composite, no_display], &cols, &HashMap::new()).is_empty());
    }

    #[test]
    fn labelled_fks_user_override_wins() {
        let fk = customer_fk(Some("legacy_name"));
        let mut overrides = HashMap::new();
        overrides.insert("customer_id".into(), "preferred_name".into());
        let labelled = labelled_fks(&[fk], &columns(), &overrides);
        assert_eq!(labelled.len(), 1);
        assert_eq!(labelled[0].display_column, "preferred_name");
    }

    #[test]
    fn plan_requests_dedupes_skips_null_and_chunks_at_500() {
        let fk = labelled();
        let mut rows = Vec::new();
        for id in 0..1203 {
            rows.push(vec![CellValue::I64(0), CellValue::I64(id)]);
        }
        rows.push(vec![CellValue::I64(0), CellValue::Null]);
        rows.push(vec![CellValue::I64(0), CellValue::I64(7)]);
        let cache = LabelCache::default();
        let requests = plan_requests(&fk, &rows, &cache);
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].keys.len(), 500);
        assert_eq!(requests[1].keys.len(), 500);
        assert_eq!(requests[2].keys.len(), 203);
    }

    #[test]
    fn plan_requests_skips_cached_keys() {
        let fk = labelled();
        let rows = vec![vec![CellValue::I64(1), CellValue::I64(7)]];
        let mut cache = LabelCache::default();
        cache.insert_all(
            &format!(
                "{}|{}|{}",
                fk.referenced.schema, fk.referenced.table, fk.key_column
            ),
            vec![(CellValue::I64(7), "Acme".into())],
        );
        assert!(plan_requests(&fk, &rows, &cache).is_empty());
    }

    #[test]
    fn cache_evicts_oldest_beyond_limit() {
        let mut cache = LabelCache::default();
        for i in 0..=CACHE_LIMIT {
            cache.insert_all("p", vec![(CellValue::I64(i as i64), format!("v{i}"))]);
        }
        assert_eq!(cache.map.len(), CACHE_LIMIT);
        assert!(!cache.map.contains_key("p|0"));
        assert!(cache.map.contains_key(&format!("p|{CACHE_LIMIT}")));
    }

    #[test]
    fn value_labels_use_display_text_of_key() {
        let fk = labelled();
        let rows = vec![vec![CellValue::I64(1), CellValue::I64(42)]];
        let mut cache = LabelCache::default();
        cache.insert_all(
            &format!(
                "{}|{}|{}",
                fk.referenced.schema, fk.referenced.table, fk.key_column
            ),
            vec![(CellValue::I64(42), "Acme Ltd".into())],
        );
        let labels = value_labels_for(&fk, &rows, &mut cache);
        assert_eq!(labels.get("42"), Some(&"Acme Ltd".to_string()));
    }
}
