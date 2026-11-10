//! Key names from the explorer's last scan, per logical database, for the
//! key slots of a command. Bounded so a 10,000-key listing costs one sorted
//! vector per database and a prefix lookup is a binary search.

use std::collections::BTreeMap;

/// How many key names one database keeps; the explorer's own listing cap
/// is 10,000, so this holds every scan it can show.
pub const KEY_INDEX_CAP: usize = 20_000;

/// One scanned key: its name and Redis type (`hash`, `string`, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownKey {
    pub name: String,
    pub kind: String,
}

/// Key names by logical database (`"0"`, `"3"`), each sorted by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedisKeyIndex {
    by_database: BTreeMap<String, Vec<KnownKey>>,
}

impl RedisKeyIndex {
    pub const fn new() -> Self {
        Self {
            by_database: BTreeMap::new(),
        }
    }

    /// Merge a scan's keys into `database`'s list, replacing a key's type
    /// when it was already known and dropping the alphabetically last
    /// names beyond [`KEY_INDEX_CAP`].
    pub fn merge(&mut self, database: &str, keys: impl IntoIterator<Item = KnownKey>) {
        let list = self.by_database.entry(database.to_string()).or_default();
        for key in keys {
            match list.binary_search_by(|k| k.name.as_str().cmp(key.name.as_str())) {
                Ok(index) => list[index] = key,
                Err(index) => list.insert(index, key),
            }
        }
        list.truncate(KEY_INDEX_CAP);
    }

    /// Replace `database`'s list wholesale (a fresh full scan).
    pub fn replace(&mut self, database: &str, keys: impl IntoIterator<Item = KnownKey>) {
        let mut list: Vec<KnownKey> = keys.into_iter().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list.dedup_by(|a, b| a.name == b.name);
        list.truncate(KEY_INDEX_CAP);
        self.by_database.insert(database.to_string(), list);
    }

    pub fn clear(&mut self) {
        self.by_database.clear();
    }

    pub fn len(&self, database: &str) -> usize {
        self.by_database.get(database).map_or(0, Vec::len)
    }

    pub fn is_empty(&self) -> bool {
        self.by_database.values().all(Vec::is_empty)
    }

    /// Keys of `database` starting with `prefix` (byte-wise, case-sensitive
    /// — Redis keys are), at most `limit`, in name order. An empty prefix
    /// lists the first `limit` keys.
    pub fn matching<'a>(&'a self, database: &str, prefix: &str, limit: usize) -> Vec<&'a KnownKey> {
        let Some(list) = self.by_database.get(database) else {
            return Vec::new();
        };
        let start = list.partition_point(|k| k.name.as_str() < prefix);
        list[start..]
            .iter()
            .take_while(|k| k.name.starts_with(prefix))
            .take(limit)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, kind: &str) -> KnownKey {
        KnownKey {
            name: name.into(),
            kind: kind.into(),
        }
    }

    #[test]
    fn merge_keeps_names_sorted_and_prefix_lookup_is_bounded() {
        let mut index = RedisKeyIndex::default();
        index.merge("0", vec![key("user:2", "hash"), key("user:1", "hash")]);
        index.merge("0", vec![key("order:1", "string"), key("user:1", "string")]);
        let names: Vec<&str> = index
            .matching("0", "user:", 10)
            .iter()
            .map(|k| k.name.as_str())
            .collect();
        assert_eq!(names, vec!["user:1", "user:2"]);
        assert_eq!(index.matching("0", "user:", 10)[0].kind, "string");
        assert_eq!(index.matching("0", "", 1).len(), 1);
        assert!(index.matching("1", "user:", 10).is_empty());
        assert!(
            index.matching("0", "USER:", 10).is_empty(),
            "case-sensitive"
        );
    }

    #[test]
    fn replace_dedups_and_caps() {
        let mut index = RedisKeyIndex::default();
        index.replace(
            "3",
            (0..(KEY_INDEX_CAP + 10)).map(|i| key(&format!("k:{i:06}"), "string")),
        );
        assert_eq!(index.len("3"), KEY_INDEX_CAP);
        index.replace("3", vec![key("a", "set"), key("a", "set")]);
        assert_eq!(index.len("3"), 1);
    }
}
