//! A small unified line diff for cell sources (LCS; sources are short).

/// Above this many line pairs the diff shows a whole replacement instead.
const MAX_CELLS: usize = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

fn script(a: &[&str], b: &[&str]) -> Vec<Op> {
    let (n, m) = (a.len(), b.len());
    if n.saturating_mul(m) > MAX_CELLS {
        let mut ops: Vec<Op> = (0..n).map(Op::Delete).collect();
        ops.extend((0..m).map(Op::Insert));
        return ops;
    }
    // lcs[i][j]: LCS length of a[i..] and b[j..].
    let width = m + 1;
    let mut lcs = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * width + j] = if a[i] == b[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut ops = Vec::with_capacity(n + m);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push(Op::Equal(i, j));
            i += 1;
            j += 1;
        } else if i < n && (j == m || lcs[(i + 1) * width + j] >= lcs[i * width + j + 1]) {
            ops.push(Op::Delete(i));
            i += 1;
        } else {
            ops.push(Op::Insert(j));
            j += 1;
        }
    }
    ops
}

/// Unified diff hunks (no file header) with `context` lines around each
/// change; empty when the texts are equal.
pub fn unified(old: &str, new: &str, context: usize) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = script(&a, &b);
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Equal(..)))
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return String::new();
    }
    // Group changes whose context windows touch.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &at in &changed {
        let start = at.saturating_sub(context);
        let end = (at + context + 1).min(ops.len());
        match groups.last_mut() {
            Some(last) if start <= last.1 => last.1 = end,
            _ => groups.push((start, end)),
        }
    }
    let mut out = String::new();
    for (start, end) in groups {
        let slice = &ops[start..end];
        let old_start = position(&ops[..start], true);
        let new_start = position(&ops[..start], false);
        let old_len = slice
            .iter()
            .filter(|op| !matches!(op, Op::Insert(_)))
            .count();
        let new_len = slice
            .iter()
            .filter(|op| !matches!(op, Op::Delete(_)))
            .count();
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(old_start, old_len),
            range(new_start, new_len)
        ));
        for op in slice {
            match *op {
                Op::Equal(i, _) => out.push_str(&format!(" {}\n", a[i])),
                Op::Delete(i) => out.push_str(&format!("-{}\n", a[i])),
                Op::Insert(j) => out.push_str(&format!("+{}\n", b[j])),
            }
        }
    }
    out
}

/// Lines of the old (or new) side consumed by `ops`.
fn position(ops: &[Op], old: bool) -> usize {
    ops.iter()
        .filter(|op| match op {
            Op::Equal(..) => true,
            Op::Delete(_) => old,
            Op::Insert(_) => !old,
        })
        .count()
}

fn range(start: usize, len: usize) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_texts_have_no_hunks() {
        assert_eq!(unified("a\nb\n", "a\nb\n", 3), "");
    }

    #[test]
    fn a_changed_line_shows_context() {
        let old = "SELECT id,\n  total\nFROM orders\nWHERE 1 = 1\n";
        let new = "SELECT id,\n  total\nFROM orders\nWHERE region = :region\n";
        assert_eq!(
            unified(old, new, 2),
            "@@ -2,3 +2,3 @@\n   total\n FROM orders\n-WHERE 1 = 1\n+WHERE region = :region\n"
        );
    }

    #[test]
    fn insertions_into_empty_and_far_apart_changes() {
        assert_eq!(unified("", "x\ny\n", 3), "@@ -0,0 +1,2 @@\n+x\n+y\n");
        let old: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let new = old
            .replacen("2\n", "two\n", 1)
            .replacen("19\n", "nineteen\n", 1);
        let diff = unified(&old, &new, 1);
        assert_eq!(diff.matches("@@ -").count(), 2, "{diff}");
    }
}
