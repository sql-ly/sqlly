//! Dependency discovery + topological ordering for multi-object scripting.
//!
//! When scripting more than one object (a "Generate Scripts" run), the emitted
//! batches must be ordered so that every object appears after the objects it
//! depends on (e.g. a view after the tables it selects from, a foreign-key table
//! after the table it references). This module provides a small, dependency-free
//! topological sort over a declared dependency graph, with deterministic tie
//! breaking and cycle detection so the caller can fall back to a stable order
//! and warn rather than emit a broken script.

use std::collections::{BTreeMap, BTreeSet};

/// A single scriptable object keyed by its fully-qualified name (e.g.
/// `[dbo].[orders]`), with the set of object keys it depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptObject {
    pub key: String,
    pub depends_on: Vec<String>,
}

impl ScriptObject {
    pub fn new(key: impl Into<String>, depends_on: Vec<String>) -> Self {
        Self {
            key: key.into(),
            depends_on,
        }
    }
}

/// Result of ordering: the dependency-respecting order plus any warnings
/// (unknown dependencies, or cycles that forced a fallback order).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DependencyOrder {
    pub order: Vec<String>,
    pub warnings: Vec<sqlly_core::DisplayMessage>,
}

/// Orders objects so dependencies precede dependents (Kahn's algorithm). Ties
/// are broken by ascending key so output is deterministic. Unknown dependency
/// keys are ignored (with a warning). If a cycle is detected, the remaining
/// objects are appended in stable key order and a warning is emitted.
pub fn topological_order(objects: &[ScriptObject]) -> DependencyOrder {
    let keys: BTreeSet<&str> = objects.iter().map(|o| o.key.as_str()).collect();
    let mut warnings = Vec::new();

    // Build in-degree + adjacency only over known keys.
    let mut indegree: BTreeMap<&str, usize> = keys.iter().map(|k| (*k, 0usize)).collect();
    let mut dependents: BTreeMap<&str, BTreeSet<&str>> =
        keys.iter().map(|k| (*k, BTreeSet::new())).collect();

    for object in objects {
        for dep in &object.depends_on {
            if !keys.contains(dep.as_str()) {
                warnings.push(
                    sqlly_core::DisplayMessage::new(
                        "services.scripting.dependency.missing",
                        format!(
                            "{} depends on {} which is not in the script set",
                            object.key, dep
                        ),
                    )
                    .with_arg("object_name", object.key.clone())
                    .with_arg("dependency_name", dep.clone()),
                );
                continue;
            }
            if dep == &object.key {
                continue; // ignore self-dependency
            }
            // edge: dep -> object  (dep must come first)
            if dependents
                .get_mut(dep.as_str())
                .unwrap()
                .insert(object.key.as_str())
            {
                *indegree.get_mut(object.key.as_str()).unwrap() += 1;
            }
        }
    }

    // Kahn with a sorted ready-set for determinism.
    let mut ready: BTreeSet<&str> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut order = Vec::with_capacity(objects.len());

    while let Some(&next) = ready.iter().next() {
        ready.remove(next);
        order.push(next.to_string());
        for &dependent in &dependents[next] {
            let d = indegree.get_mut(dependent).unwrap();
            *d -= 1;
            if *d == 0 {
                ready.insert(dependent);
            }
        }
    }

    if order.len() != keys.len() {
        // Cycle: append the unresolved objects in stable order.
        let placed: BTreeSet<&str> = order.iter().map(|s| s.as_str()).collect();
        let mut leftover: Vec<&str> = keys
            .iter()
            .copied()
            .filter(|k| !placed.contains(k))
            .collect();
        leftover.sort_unstable();
        warnings.push(
            sqlly_core::DisplayMessage::new(
                "services.scripting.dependency.cycle",
                format!(
                    "dependency cycle detected among {} object(s); emitting them in stable order",
                    leftover.len()
                ),
            )
            .with_arg("object_count", leftover.len().to_string()),
        );
        for k in leftover {
            order.push(k.to_string());
        }
    }

    DependencyOrder { order, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_dependencies_before_dependents() {
        let objects = vec![
            ScriptObject::new("[dbo].[v_orders]", vec!["[dbo].[orders]".into()]),
            ScriptObject::new("[dbo].[orders]", vec!["[dbo].[customers]".into()]),
            ScriptObject::new("[dbo].[customers]", vec![]),
        ];
        let result = topological_order(&objects);
        assert!(result.warnings.is_empty());
        assert_eq!(
            result.order,
            vec![
                "[dbo].[customers]".to_string(),
                "[dbo].[orders]".to_string(),
                "[dbo].[v_orders]".to_string(),
            ]
        );
    }

    #[test]
    fn deterministic_tie_break_by_key() {
        let objects = vec![
            ScriptObject::new("[dbo].[b]", vec![]),
            ScriptObject::new("[dbo].[a]", vec![]),
            ScriptObject::new("[dbo].[c]", vec![]),
        ];
        let result = topological_order(&objects);
        assert_eq!(result.order, vec!["[dbo].[a]", "[dbo].[b]", "[dbo].[c]"]);
    }

    #[test]
    fn warns_on_unknown_dependency_but_still_orders() {
        let objects = vec![ScriptObject::new(
            "[dbo].[orders]",
            vec!["[dbo].[missing]".into()],
        )];
        let result = topological_order(&objects);
        assert_eq!(result.order, vec!["[dbo].[orders]"]);
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("missing"));
    }

    #[test]
    fn detects_cycle_and_falls_back_to_stable_order() {
        let objects = vec![
            ScriptObject::new("[dbo].[a]", vec!["[dbo].[b]".into()]),
            ScriptObject::new("[dbo].[b]", vec!["[dbo].[a]".into()]),
        ];
        let result = topological_order(&objects);
        assert_eq!(result.order.len(), 2);
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("cycle"));
    }

    #[test]
    fn self_reference_is_not_a_cycle() {
        let objects = vec![ScriptObject::new("dbo.t", vec!["dbo.t".into()])];
        let result = topological_order(&objects);
        assert_eq!(result.order, vec!["dbo.t"]);
        assert!(result.warnings.is_empty());
    }
}
