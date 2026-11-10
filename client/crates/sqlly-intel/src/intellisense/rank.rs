//! Post-ranking helpers applied by the services pipeline after core
//! completion and before truncation. Pure and allocation-light: they never
//! rescan the SQL text.

use std::collections::HashMap;

use crate::intellisense::types::CompletionItem;

/// Added when the typed prefix equals the item label (or its last dotted
/// segment) case-insensitively. Above the fuzzy CAMEL tier (25) so an exact
/// name always outranks a fuzzy sibling of the same kind.
pub const EXACT_LABEL_BOOST: i32 = 30;
pub const ACCEPT_BOOST_PER_HIT: i32 = 80;
pub const ACCEPT_BOOST_MAX: i32 = 500;
pub const ACCEPT_HISTORY_CAP: usize = 512;

pub fn exact_label_boost(prefix: &str, label: &str) -> i32 {
    if prefix.is_empty() {
        return 0;
    }
    if label.eq_ignore_ascii_case(prefix) {
        return EXACT_LABEL_BOOST;
    }
    if let Some(last) = label.rsplit('.').next()
        && last.eq_ignore_ascii_case(prefix)
    {
        return EXACT_LABEL_BOOST;
    }
    0
}

/// Per-session LRU of accepted completion labels → accept count.
#[derive(Debug, Default, Clone)]
pub struct AcceptHistory {
    counts: HashMap<String, u32>,
    order: Vec<String>,
}

impl AcceptHistory {
    pub fn record(&mut self, label: &str) {
        let key = label.to_ascii_lowercase();
        Self::lru_touch(&mut self.order, &key);
        *self.counts.entry(key).or_insert(0) += 1;
        Self::lru_evict(&mut self.counts, &mut self.order, ACCEPT_HISTORY_CAP);
    }

    pub fn boost(&self, label: &str) -> i32 {
        let key = label.to_ascii_lowercase();
        self.counts
            .get(&key)
            .map(|count| {
                (*count as i32)
                    .saturating_mul(ACCEPT_BOOST_PER_HIT)
                    .min(ACCEPT_BOOST_MAX)
            })
            .unwrap_or(0)
    }

    pub fn len(&self) -> usize {
        self.counts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
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

    fn lru_evict(counts: &mut HashMap<String, u32>, order: &mut Vec<String>, limit: usize) {
        while counts.len() > limit {
            let Some(oldest) = order.first().cloned() else {
                break;
            };
            order.remove(0);
            counts.remove(&oldest);
        }
    }
}

/// Apply both boosts in place. `prefix` is the typed prefix at the cursor.
pub fn apply_rank_boosts(
    items: &mut [CompletionItem],
    prefix: &str,
    history: Option<&AcceptHistory>,
) {
    if prefix.is_empty() && history.is_none_or(|h| h.is_empty()) {
        return;
    }
    for item in items {
        item.score += exact_label_boost(prefix, &item.label);
        if let Some(h) = history {
            item.score += h.boost(&item.label);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intellisense::types::{CompletionKind, CompletionSource};

    fn item(label: &str, score: i32) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            insert_text: label.to_string(),
            kind: CompletionKind::Table,
            source: CompletionSource::LiveMetadata,
            score,
            detail: String::new(),
            replace_span: None,
        }
    }

    #[test]
    fn exact_boost_on_full_label_and_last_segment() {
        assert_eq!(exact_label_boost("ord", "sales.Orders"), 0);
        assert_eq!(
            exact_label_boost("orders", "sales.Orders"),
            EXACT_LABEL_BOOST
        );
        assert_eq!(
            exact_label_boost("sales.orders", "sales.Orders"),
            EXACT_LABEL_BOOST
        );
    }

    #[test]
    fn exact_boost_is_case_insensitive() {
        assert_eq!(
            exact_label_boost("ORDERS", "sales.Orders"),
            EXACT_LABEL_BOOST
        );
    }

    #[test]
    fn exact_boost_empty_prefix_is_zero() {
        assert_eq!(exact_label_boost("", "Orders"), 0);
    }

    #[test]
    fn accept_history_boost_math() {
        let mut history = AcceptHistory::default();
        history.record("dbo.Orders");
        assert_eq!(history.boost("dbo.Orders"), 80);
        for _ in 0..5 {
            history.record("dbo.Orders");
        }
        assert_eq!(history.boost("dbo.Orders"), 480);
        history.record("dbo.Orders");
        assert_eq!(history.boost("dbo.Orders"), 500);
        assert_eq!(history.boost("unknown"), 0);
    }

    #[test]
    fn accept_history_lru_evicts_oldest_at_cap() {
        let mut history = AcceptHistory::default();
        for i in 0..513 {
            history.record(&format!("label{i:04}"));
        }
        assert_eq!(history.len(), ACCEPT_HISTORY_CAP);
        assert_eq!(history.boost("label0000"), 0);
    }

    #[test]
    fn accept_history_rerecord_moves_key_to_back() {
        let mut history = AcceptHistory::default();
        history.record("a");
        history.record("b");
        history.record("a");
        for i in 0..511 {
            history.record(&format!("label{i:04}"));
        }
        assert_eq!(history.len(), ACCEPT_HISTORY_CAP);
        assert_eq!(history.boost("b"), 0);
        assert!(history.boost("a") > 0);
    }

    #[test]
    fn apply_rank_boosts_fast_path_leaves_scores_unchanged() {
        let mut items = vec![item("dbo.Orders", 100)];
        apply_rank_boosts(&mut items, "", None);
        assert_eq!(items[0].score, 100);
    }
}
