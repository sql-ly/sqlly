//! Helpers the plan parsers share: tolerant number reading and the
//! bookkeeping every parser does once its nodes and edges exist.

use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode};

/// Read a count the way plan text writes them: thousands separators
/// (`1,234`), a `K` / `M` / `G` / `T` multiplier of 1000 (Oracle's `10K`,
/// `2M`), scientific notation (`1.5e+06`, MySQL's `696e-6`), and a leading
/// `~` (DuckDB estimates). Returns `None` for anything else.
pub(crate) fn parse_count(text: &str) -> Option<f64> {
    let t = text.trim().trim_start_matches('~').trim();
    if t.is_empty() || !t.starts_with(|c: char| c.is_ascii_digit() || c == '.' || c == '-') {
        return None;
    }
    let cleaned: String = t.chars().filter(|c| *c != ',' && *c != '_').collect();
    if let Ok(value) = cleaned.parse::<f64>() {
        return value.is_finite().then_some(value);
    }
    let mut chars = cleaned.chars();
    let suffix = chars.next_back()?;
    let scale = match suffix {
        'K' | 'k' => 1e3,
        'M' => 1e6,
        'G' => 1e9,
        'T' => 1e12,
        _ => return None,
    };
    let value = chars.as_str().parse::<f64>().ok()? * scale;
    value.is_finite().then_some(value)
}

/// Read a memory or disk size with a unit into kB: `25kB`, `16128 kB`,
/// `1.5MB`, `2 GB`, `512 bytes`. A bare number is taken as kB.
pub(crate) fn parse_size_kb(text: &str) -> Option<f64> {
    let t = text.trim();
    let split = t
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
        .unwrap_or(t.len());
    let number: String = t[..split].chars().filter(|c| *c != ',').collect();
    let value = number.parse::<f64>().ok()?;
    let unit = t[split..].trim().to_ascii_lowercase();
    let kb = match unit.as_str() {
        "" | "kb" | "kib" | "k" => value,
        "b" | "bytes" | "byte" => value / 1024.0,
        "mb" | "mib" | "m" => value * 1024.0,
        "gb" | "gib" | "g" => value * 1024.0 * 1024.0,
        "tb" | "tib" | "t" => value * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    kb.is_finite().then_some(kb)
}

/// Ids of the direct inputs of `id`, in document order.
pub(crate) fn child_ids(edges: &[PlanEdge], id: i32) -> Vec<i32> {
    edges
        .iter()
        .filter(|edge| edge.from_node == id)
        .map(|edge| edge.to_node)
        .collect()
}

/// Fill `actual_self_time_ms` for every node that has an inclusive
/// `actual_time_ms` and no self time yet: inclusive time minus the inclusive
/// time of its direct inputs, never below zero.
pub(crate) fn fill_self_times(nodes: &mut [PlanNode], edges: &[PlanEdge]) {
    let inclusive: std::collections::HashMap<i32, f64> = nodes
        .iter()
        .filter_map(|node| node.actual_time_ms.map(|ms| (node.id, ms)))
        .collect();
    // Summed once per parent, so wide plans stay linear.
    let mut inputs: std::collections::HashMap<i32, f64> = std::collections::HashMap::new();
    for edge in edges {
        if let Some(ms) = inclusive.get(&edge.to_node) {
            *inputs.entry(edge.from_node).or_default() += ms;
        }
    }
    for node in nodes.iter_mut() {
        if node.actual_self_time_ms.is_some() {
            continue;
        }
        let Some(own) = node.actual_time_ms else {
            continue;
        };
        let below = inputs.get(&node.id).copied().unwrap_or(0.0);
        node.actual_self_time_ms = Some((own - below).max(0.0));
    }
}

/// Last step of every parser: plan totals from the root when the parser left
/// them at zero, `has_actuals` from the nodes, and self times.
pub(crate) fn finish_plan(plan: &mut ExecutionPlan) {
    if let Some(root) = plan.nodes.first() {
        if plan.total_subtree_cost == 0.0 {
            plan.total_subtree_cost = root.estimated_cost;
        }
        if plan.estimated_rows == 0.0 {
            plan.estimated_rows = root.estimated_rows;
        }
    }
    if !plan.has_actuals {
        plan.has_actuals = plan.execution_time_ms.is_some()
            || plan.nodes.iter().any(|node| {
                node.actual_rows.is_some() || node.actual_time_ms.is_some() || node.never_executed
            });
    }
    fill_self_times(&mut plan.nodes, &plan.edges);
}

/// Feed `parse` truncated and garbled variants of `sample`. The closure must
/// return normally for every one: a parser may fail, it may not panic or
/// hang. Shared by the per-format tests.
#[cfg(test)]
pub(crate) fn assert_survives_mangling(sample: &str, parse: impl Fn(&str)) {
    let boundaries: Vec<usize> = sample
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(sample.len()))
        .collect();
    let step = (boundaries.len() / 120).max(1);
    for cut in boundaries.iter().step_by(step) {
        parse(&sample[..*cut]);
        parse(&sample[*cut..]);
    }
    let lines: Vec<&str> = sample.lines().collect();
    let line_step = (lines.len() / 60).max(1);
    for skip in (0..lines.len()).step_by(line_step) {
        let without: Vec<&str> = lines
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != skip)
            .map(|(_, line)| *line)
            .collect();
        parse(&without.join("\n"));
    }
    let reversed: Vec<&str> = lines.iter().rev().copied().collect();
    parse(&reversed.join("\n"));
    parse(&format!("{sample}\n{sample}"));
    for (from, to) in [
        ("(", ""),
        (")", ""),
        ("|", ""),
        ("|", "||"),
        (":", ""),
        ("=", ""),
        ("\"", ""),
        ("{", "["),
        ("}", ""),
        ("<", ""),
        (">", "<"),
        ("-", ""),
        (" ", ""),
        (" ", "\t"),
        ("\n", " "),
        ("\n", "\n\n"),
        ("0", "99999999999999999999999999"),
        ("1", "-1"),
        ("1", "NaN"),
        ("1", "1e999"),
        ("a", "\u{00e9}\u{4e2d}"),
        ("..", "."),
        (",", ""),
    ] {
        parse(&sample.replace(from, to));
    }
    // Indentation is structure in most text plans.
    let dedented: Vec<&str> = lines.iter().map(|line| line.trim_start()).collect();
    parse(&dedented.join("\n"));
    let indented: Vec<String> = lines.iter().map(|line| format!("        {line}")).collect();
    parse(&indented.join("\n"));
    parse(&sample.to_uppercase());
    parse(&sample.to_lowercase());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_read_separators_suffixes_and_exponents() {
        assert_eq!(parse_count("1,234"), Some(1234.0));
        assert_eq!(parse_count("10K"), Some(10_000.0));
        assert_eq!(parse_count("2M"), Some(2_000_000.0));
        assert_eq!(parse_count("~42"), Some(42.0));
        assert_eq!(parse_count("696e-6"), Some(0.000696));
        assert_eq!(parse_count("1.5e+06"), Some(1_500_000.0));
        assert_eq!(parse_count(""), None);
        assert_eq!(parse_count("abc"), None);
        assert_eq!(parse_count("K"), None);
        assert_eq!(parse_count("inf"), None);
        assert_eq!(parse_count("NaN"), None);
    }

    #[test]
    fn sizes_convert_to_kilobytes() {
        assert_eq!(parse_size_kb("25kB"), Some(25.0));
        assert_eq!(parse_size_kb("16128 kB"), Some(16128.0));
        assert_eq!(parse_size_kb("2MB"), Some(2048.0));
        assert_eq!(parse_size_kb("1024 bytes"), Some(1.0));
        assert_eq!(parse_size_kb("x"), None);
    }
}
