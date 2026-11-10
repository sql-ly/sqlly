//! Key-prefix grouping for the Redis key browser.
//!
//! Redis has no schema: structure lives in key names by convention
//! (`user:42:profile`). The explorer folds a flat key listing into folders
//! one delimiter segment at a time — `user:*` holds every key that starts
//! with `user:`, and expanding it groups the remainder again — so a keyspace
//! of any size reads like a tree. The delimiter is per connection
//! ([`effective_delimiter`]); `:` is the convention almost everyone uses.
//! A setting may name several delimiters ([`parse_delimiters`]) for
//! keyspaces that mix conventions (`user:42` beside `cache|en|home`), and
//! the memory analyzer's by-prefix report splits key names the same way
//! ([`namespace_prefix`], [`aggregate_by_prefix`]).

use crate::ConnectionProfile;

/// The delimiter used when a connection sets none.
pub const DEFAULT_KEY_DELIMITER: &str = ":";

/// The key delimiter a profile groups by: its own setting when it names
/// one, `:` otherwise. Whitespace-only settings count as unset.
pub fn effective_delimiter(profile: &ConnectionProfile) -> String {
    profile
        .redis_key_delimiter
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .unwrap_or(DEFAULT_KEY_DELIMITER)
        .to_string()
}

/// The delimiters a delimiter setting names, in the order written.
///
/// - Blank → `:`.
/// - Whitespace-separated tokens → each token is one delimiter, so a
///   multi-character delimiter can sit beside others (`:: |`).
/// - One character, or the same character repeated (`::`) → that one
///   delimiter, exactly as stored settings have always meant it.
/// - Otherwise → every distinct character is a delimiter (`:|` splits on
///   both `:` and `|`).
pub fn parse_delimiters(spec: &str) -> Vec<String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return vec![DEFAULT_KEY_DELIMITER.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut push = |d: String| {
        if !d.is_empty() && !out.contains(&d) {
            out.push(d);
        }
    };
    if spec.chars().any(char::is_whitespace) {
        for token in spec.split_whitespace() {
            push(token.to_string());
        }
    } else {
        let mut chars = spec.chars();
        let first = chars.next().expect("non-empty");
        if chars.all(|c| c == first) {
            push(spec.to_string());
        } else {
            for c in spec.chars() {
                push(c.to_string());
            }
        }
    }
    out
}

/// The first delimiter occurrence in `text`: its byte position and the
/// delimiter matched there (the longest one when several start at the same
/// position, so `::` wins over `:`).
pub fn find_delimiter<'d>(text: &str, delimiters: &'d [String]) -> Option<(usize, &'d str)> {
    let mut best: Option<(usize, &'d str)> = None;
    for delimiter in delimiters {
        if delimiter.is_empty() {
            continue;
        }
        if let Some(pos) = text.find(delimiter.as_str()) {
            let better = match best {
                None => true,
                Some((at, current)) => pos < at || (pos == at && delimiter.len() > current.len()),
            };
            if better {
                best = Some((pos, delimiter.as_str()));
            }
        }
    }
    best
}

/// The namespace a key belongs to at `depth` segments, delimiter included
/// (`user:42:profile` at depth 1 → `user:`, at depth 2 → `user:42:`). A key
/// with fewer namespace segments than `depth` reports the deepest it has
/// (`user:42` at depth 2 → `user:`, since `42` is the key's own name), and a
/// key with none (`config`, or a leading delimiter) reports `None`. An empty
/// segment ends the walk, the way the key browser treats it.
pub fn namespace_prefix(key: &str, delimiters: &[String], depth: usize) -> Option<String> {
    let mut end = 0usize;
    for _ in 0..depth.max(1) {
        let rest = &key[end..];
        match find_delimiter(rest, delimiters) {
            Some((pos, delimiter)) if pos > 0 => end += pos + delimiter.len(),
            _ => break,
        }
    }
    (end > 0).then(|| key[..end].to_string())
}

/// One row of the memory analyzer's by-prefix report.
#[derive(Debug, Clone, PartialEq)]
pub struct PrefixAggregate {
    /// The namespace (`user:`); `None` for keys with no delimiter.
    pub prefix: Option<String>,
    pub keys: i64,
    /// Measured bytes; `None` when the sample could not measure memory.
    pub bytes: Option<i64>,
    /// Share of the sample's measured bytes, one decimal; 0 when unknown.
    pub pct_of_sampled_bytes: f64,
}

/// Sum a sample's keys and measured bytes per namespace prefix at `depth`.
/// `bytes` is `None` for every key when memory could not be measured; the
/// key counts stay honest and the byte columns stay unknown. Largest
/// measured footprint first, then most keys, then by name.
pub fn aggregate_by_prefix<'k>(
    keys: impl IntoIterator<Item = (&'k str, Option<i64>)>,
    delimiters: &[String],
    depth: usize,
) -> Vec<PrefixAggregate> {
    let mut index: std::collections::HashMap<Option<String>, usize> = Default::default();
    let mut rows: Vec<(Option<String>, i64, i64)> = Vec::new();
    let mut measured = false;
    let mut total = 0i64;
    for (key, bytes) in keys {
        let prefix = namespace_prefix(key, delimiters, depth);
        measured |= bytes.is_some();
        let bytes = bytes.unwrap_or(0);
        total += bytes;
        match index.get(&prefix) {
            Some(&i) => {
                rows[i].1 += 1;
                rows[i].2 += bytes;
            }
            None => {
                index.insert(prefix.clone(), rows.len());
                rows.push((prefix, 1, bytes));
            }
        }
    }
    let mut out: Vec<PrefixAggregate> = rows
        .into_iter()
        .map(|(prefix, keys, bytes)| PrefixAggregate {
            prefix,
            keys,
            bytes: measured.then_some(bytes),
            pct_of_sampled_bytes: if total > 0 {
                (bytes as f64 * 1000.0 / total as f64).round() / 10.0
            } else {
                0.0
            },
        })
        .collect();
    out.sort_by(|a, b| {
        b.bytes
            .unwrap_or(0)
            .cmp(&a.bytes.unwrap_or(0))
            .then(b.keys.cmp(&a.keys))
            .then(a.prefix.cmp(&b.prefix))
    });
    out
}

/// One folder of the grouping: the keys sharing the next segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyGroup {
    /// The segment shown as the folder's name (`user`).
    pub label: String,
    /// The full prefix the folder covers, delimiter included (`user:`), so
    /// expanding it scans `MATCH user:*`.
    pub prefix: String,
    /// Keys under the prefix in the listing that was grouped.
    pub count: usize,
}

/// A grouping of one listing level: folders first (busiest first), then the
/// keys that end at this level, as indexes into the grouped slice.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyGrouping {
    pub folders: Vec<KeyGroup>,
    pub leaves: Vec<usize>,
}

/// Group `keys` one level below `parent_prefix`. Keys outside the prefix are
/// ignored; a key whose remainder holds a delimiter becomes part of the
/// folder named by its next segment, and every other key is a leaf. A key
/// exactly equal to the prefix (or with an empty next segment) is a leaf.
/// `delimiter` is a delimiter setting ([`parse_delimiters`]): with several
/// delimiters each key splits at the first one it holds, and two folders
/// that share a segment but not a delimiter (`a:` and `a|`) are labelled
/// with their delimiter so they stay told apart.
pub fn group_keys(keys: &[String], parent_prefix: &str, delimiter: &str) -> KeyGrouping {
    let delimiters = parse_delimiters(delimiter);
    let mut folders: Vec<KeyGroup> = Vec::new();
    let mut index_by_prefix: std::collections::HashMap<String, usize> = Default::default();
    let mut leaves: Vec<usize> = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        let Some(rest) = key.strip_prefix(parent_prefix) else {
            continue;
        };
        match find_delimiter(rest, &delimiters) {
            Some((pos, matched)) if pos > 0 => {
                let label = &rest[..pos];
                let prefix = format!("{parent_prefix}{label}{matched}");
                match index_by_prefix.get(&prefix) {
                    Some(&index) => folders[index].count += 1,
                    None => {
                        index_by_prefix.insert(prefix.clone(), folders.len());
                        folders.push(KeyGroup {
                            label: label.to_string(),
                            prefix,
                            count: 1,
                        });
                    }
                }
            }
            _ => leaves.push(i),
        }
    }
    let mut label_uses: std::collections::HashMap<String, usize> = Default::default();
    for folder in &folders {
        *label_uses.entry(folder.label.clone()).or_default() += 1;
    }
    for folder in &mut folders {
        if label_uses.get(&folder.label).copied().unwrap_or(0) > 1 {
            let delimiter = &folder.prefix[parent_prefix.len() + folder.label.len()..];
            folder.label = format!("{}{delimiter}", folder.label);
        }
    }
    folders.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.label.cmp(&b.label)));
    leaves.sort_by(|a, b| keys[*a].cmp(&keys[*b]));
    KeyGrouping { folders, leaves }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SecretProvider, SecretRef};

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn groups_by_first_segment_and_orders_busiest_first() {
        let listing = keys(&[
            "user:1:profile",
            "user:2:profile",
            "user:1:session",
            "order:7",
            "config",
            "user:",
            "cache/x",
        ]);
        let grouping = group_keys(&listing, "", ":");
        assert_eq!(grouping.folders.len(), 2);
        // `user:` itself belongs to the folder (it is the empty-remainder leaf
        // one level down), so the folder counts four.
        assert_eq!(
            grouping.folders[0],
            KeyGroup {
                label: "user".into(),
                prefix: "user:".into(),
                count: 4
            }
        );
        assert_eq!(grouping.folders[1].label, "order");
        let leaves: Vec<&str> = grouping
            .leaves
            .iter()
            .map(|i| listing[*i].as_str())
            .collect();
        assert_eq!(leaves, vec!["cache/x", "config"]);
        let inside = group_keys(&listing, "user:", ":");
        assert_eq!(inside.leaves, vec![5]);
    }

    #[test]
    fn groups_deeper_levels_relative_to_the_parent_prefix() {
        let listing = keys(&[
            "user:1:profile",
            "user:1:session",
            "user:2:profile",
            "user:x",
            "order:1",
        ]);
        let grouping = group_keys(&listing, "user:", ":");
        assert_eq!(grouping.folders.len(), 2);
        assert_eq!(grouping.folders[0].prefix, "user:1:");
        assert_eq!(grouping.folders[0].count, 2);
        assert_eq!(grouping.folders[1].prefix, "user:2:");
        assert_eq!(grouping.leaves, vec![3]);
        let deeper = group_keys(&listing, "user:1:", ":");
        assert!(deeper.folders.is_empty());
        assert_eq!(deeper.leaves, vec![0, 1]);
    }

    #[test]
    fn honours_other_delimiters_and_defaults_blank_ones() {
        let listing = keys(&["a/b/c", "a/d", "e"]);
        let grouping = group_keys(&listing, "", "/");
        assert_eq!(grouping.folders[0].prefix, "a/");
        assert_eq!(grouping.folders[0].count, 2);
        assert_eq!(grouping.leaves, vec![2]);
        let colon = group_keys(&keys(&["x:y", "z"]), "", "");
        assert_eq!(colon.folders[0].prefix, "x:");
        let dotted = group_keys(&keys(&["a.b", "a..c", ".d"]), "", ".");
        assert_eq!(dotted.folders.len(), 1);
        assert_eq!(dotted.folders[0].count, 2);
        assert_eq!(dotted.leaves, vec![2]);
    }

    #[test]
    fn delimiter_settings_parse_as_a_set() {
        let set = |spec: &str| parse_delimiters(spec);
        assert_eq!(set(""), vec![":"]);
        assert_eq!(set("   "), vec![":"]);
        // Stored single-character settings keep meaning exactly that.
        assert_eq!(set(":"), vec![":"]);
        assert_eq!(set("/"), vec!["/"]);
        // A repeated character stays one multi-character delimiter.
        assert_eq!(set("::"), vec!["::"]);
        // Several distinct characters: each one splits.
        assert_eq!(set(":|"), vec![":", "|"]);
        assert_eq!(set(":|:"), vec![":", "|"]);
        // Whitespace separates tokens, so a long delimiter can sit beside others.
        assert_eq!(set(":: |"), vec!["::", "|"]);
        assert_eq!(set(" : | : "), vec![":", "|"]);
    }

    #[test]
    fn groups_by_the_first_of_several_delimiters() {
        let listing = keys(&["user:1", "user:2", "cache|en|home", "cache|fr", "plain"]);
        let grouping = group_keys(&listing, "", ":|");
        let prefixes: Vec<&str> = grouping.folders.iter().map(|f| f.prefix.as_str()).collect();
        assert_eq!(prefixes, vec!["cache|", "user:"]);
        assert_eq!(grouping.folders[0].label, "cache");
        assert_eq!(grouping.leaves, vec![4]);
        // One level down the remainder splits on either delimiter too.
        let inside = group_keys(&listing, "cache|", ":|");
        assert_eq!(inside.folders.len(), 1);
        assert_eq!(inside.folders[0].prefix, "cache|en|");
        assert_eq!(inside.leaves, vec![3]);
        // `a:b|c` splits at the earlier delimiter.
        let mixed = group_keys(&keys(&["a:b|c"]), "", ":|");
        assert_eq!(mixed.folders[0].prefix, "a:");
    }

    #[test]
    fn same_segment_under_two_delimiters_stays_two_labelled_folders() {
        let listing = keys(&["a:1", "a|1", "a|2"]);
        let grouping = group_keys(&listing, "", ":|");
        let labels: Vec<(&str, &str)> = grouping
            .folders
            .iter()
            .map(|f| (f.label.as_str(), f.prefix.as_str()))
            .collect();
        assert_eq!(labels, vec![("a|", "a|"), ("a:", "a:")]);
    }

    #[test]
    fn longest_delimiter_wins_at_the_same_position() {
        let delimiters = parse_delimiters(":: :");
        assert_eq!(find_delimiter("a::b", &delimiters), Some((1, "::")));
        assert_eq!(find_delimiter("a:b", &delimiters), Some((1, ":")));
        assert_eq!(find_delimiter("ab", &delimiters), None);
    }

    #[test]
    fn namespace_prefix_follows_depth_and_every_delimiter() {
        let d = parse_delimiters(":|");
        assert_eq!(
            namespace_prefix("user:42:profile", &d, 1).as_deref(),
            Some("user:")
        );
        assert_eq!(
            namespace_prefix("user:42:profile", &d, 2).as_deref(),
            Some("user:42:")
        );
        // Fewer namespace segments than asked: the deepest the key has.
        assert_eq!(
            namespace_prefix("user:42:profile", &d, 3).as_deref(),
            Some("user:42:")
        );
        assert_eq!(namespace_prefix("user:42", &d, 2).as_deref(), Some("user:"));
        assert_eq!(
            namespace_prefix("cache|en|home", &d, 2).as_deref(),
            Some("cache|en|")
        );
        assert_eq!(namespace_prefix("a:b|c", &d, 2).as_deref(), Some("a:b|"));
        assert_eq!(namespace_prefix("config", &d, 1), None);
        assert_eq!(namespace_prefix(":leading", &d, 1), None);
        // Depth 0 is treated as 1.
        assert_eq!(namespace_prefix("user:1", &d, 0).as_deref(), Some("user:"));
        // An empty segment ends the walk.
        assert_eq!(
            namespace_prefix("a::b:c", &parse_delimiters(":"), 3).as_deref(),
            Some("a:")
        );
    }

    #[test]
    fn aggregates_keys_and_bytes_per_prefix_with_shares() {
        let sample = [
            ("user:1:profile", Some(300)),
            ("user:2:profile", Some(100)),
            ("user:2:session", Some(100)),
            ("cache|en|home", Some(400)),
            ("config", Some(100)),
        ];
        let d = parse_delimiters(":|");
        let rows = aggregate_by_prefix(sample.iter().map(|(k, b)| (*k, *b)), &d, 1);
        let view: Vec<(Option<&str>, i64, Option<i64>, f64)> = rows
            .iter()
            .map(|r| (r.prefix.as_deref(), r.keys, r.bytes, r.pct_of_sampled_bytes))
            .collect();
        assert_eq!(
            view,
            vec![
                (Some("user:"), 3, Some(500), 50.0),
                (Some("cache|"), 1, Some(400), 40.0),
                (None, 1, Some(100), 10.0),
            ]
        );
        let deeper = aggregate_by_prefix(sample.iter().map(|(k, b)| (*k, *b)), &d, 2);
        let prefixes: Vec<Option<&str>> = deeper.iter().map(|r| r.prefix.as_deref()).collect();
        assert_eq!(
            prefixes,
            vec![Some("cache|en|"), Some("user:1:"), Some("user:2:"), None]
        );
        assert_eq!(deeper[2].keys, 2);
    }

    #[test]
    fn unmeasured_samples_keep_counts_and_leave_bytes_unknown() {
        let d = parse_delimiters(":");
        let rows = aggregate_by_prefix([("a:1", None), ("a:2", None), ("b:1", None)], &d, 1);
        assert_eq!(rows[0].prefix.as_deref(), Some("a:"));
        assert_eq!(rows[0].keys, 2);
        assert_eq!(rows[0].bytes, None);
        assert_eq!(rows[0].pct_of_sampled_bytes, 0.0);
        assert!(aggregate_by_prefix(std::iter::empty(), &d, 1).is_empty());
    }

    #[test]
    fn profile_delimiter_defaults_to_colon() {
        let mut p = ConnectionProfile::sql_login(
            "r",
            "h",
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        assert_eq!(effective_delimiter(&p), ":");
        p.redis_key_delimiter = Some("  ".into());
        assert_eq!(effective_delimiter(&p), ":");
        p.redis_key_delimiter = Some(" / ".into());
        assert_eq!(effective_delimiter(&p), "/");
    }
}
