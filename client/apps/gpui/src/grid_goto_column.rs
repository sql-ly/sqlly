//! Fuzzy column-name matching for the result-grid "Go to Column" popover.

/// Live query, column names in source order, and the highlighted match.
pub struct GotoColumnModel {
    pub query: String,
    pub names: Vec<String>,
    pub selected: usize,
}

impl GotoColumnModel {
    pub fn new(names: Vec<String>) -> Self {
        Self {
            query: String::new(),
            names,
            selected: 0,
        }
    }

    /// Case-insensitive matches: prefix, then contains, then subsequence, each
    /// stable by source column index. Empty query lists every column.
    pub fn matches(&self) -> Vec<(usize, String)> {
        let q = self.query.trim().to_lowercase();
        if q.is_empty() {
            return self
                .names
                .iter()
                .enumerate()
                .map(|(i, name)| (i, name.clone()))
                .collect();
        }
        let mut prefix = Vec::new();
        let mut contains = Vec::new();
        let mut subsequence = Vec::new();
        for (index, name) in self.names.iter().enumerate() {
            let lower = name.to_lowercase();
            if lower.starts_with(&q) {
                prefix.push((index, name.clone()));
            } else if lower.contains(&q) {
                contains.push((index, name.clone()));
            } else if is_subsequence(&lower, &q) {
                subsequence.push((index, name.clone()));
            }
        }
        prefix.extend(contains);
        prefix.extend(subsequence);
        prefix
    }

    pub fn move_selection(&mut self, delta: isize) {
        let n = self.matches().len();
        if n == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected.min(n - 1) as isize;
        self.selected = (cur + delta).rem_euclid(n as isize) as usize;
    }

    pub fn clamp_selection(&mut self) {
        let n = self.matches().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    /// Source column index of the highlighted match.
    pub fn chosen(&self) -> Option<usize> {
        let matches = self.matches();
        if matches.is_empty() {
            return None;
        }
        matches
            .get(self.selected.min(matches.len() - 1))
            .map(|(index, _)| *index)
    }
}

fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|want| chars.any(|got| got == want))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_ranks_before_contains_before_subsequence() {
        let model = GotoColumnModel {
            query: "id".into(),
            names: vec!["aibd".into(), "order_id".into(), "id".into()],
            selected: 0,
        };
        let matches: Vec<usize> = model.matches().into_iter().map(|(i, _)| i).collect();
        assert_eq!(matches, vec![2, 1, 0]);
    }

    #[test]
    fn matching_is_case_insensitive() {
        let model = GotoColumnModel {
            query: "NAME".into(),
            names: vec!["id".into(), "Name".into(), "username".into()],
            selected: 0,
        };
        let names: Vec<String> = model.matches().into_iter().map(|(_, n)| n).collect();
        assert_eq!(names, vec!["Name".to_string(), "username".to_string()]);
    }

    #[test]
    fn selection_wraps_within_matches() {
        let mut model = GotoColumnModel {
            query: "a".into(),
            names: vec!["a".into(), "b".into(), "aa".into(), "ab".into()],
            selected: 0,
        };
        assert_eq!(model.matches().len(), 3);
        model.move_selection(-1);
        assert_eq!(model.selected, 2);
        model.move_selection(1);
        assert_eq!(model.selected, 0);
        model.move_selection(1);
        assert_eq!(model.selected, 1);
    }
}
