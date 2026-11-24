//! Child partitions in table completion (DBeaver #42745). A PostgreSQL table
//! partitioned by day can have hundreds of partitions, each its own table in
//! the catalog; offered alongside ordinary tables they bury them. A partition
//! is offered only when no ordinary table or view matches what was typed, so
//! `FROM events_2024_` still reaches one. The schema tree lists them all.

use sqlly_intel::intellisense::{CompletionItem, CompletionKind};
use sqlly_lang::sqlmodel::{Dialect, ModelSnapshot, ObjectRef, SqlName};

/// Drop table items naming a child partition, unless no other table or view
/// item matches `prefix`.
pub(crate) fn hide_child_partitions(
    items: &mut Vec<CompletionItem>,
    model: &ModelSnapshot,
    dialect: Dialect,
    prefix: &str,
) {
    if model.is_empty() || !items.iter().any(|i| i.kind == CompletionKind::Table) {
        return;
    }
    let partition: Vec<bool> = items
        .iter()
        .map(|i| i.kind == CompletionKind::Table && names_partition(model, dialect, &i.label))
        .collect();
    if !partition.contains(&true) {
        return;
    }
    let ordinary_match = items
        .iter()
        .zip(&partition)
        .any(|(i, &p)| i.kind == CompletionKind::Table && !p && matches_prefix(&i.label, prefix));
    if !ordinary_match {
        return;
    }
    let mut flags = partition.into_iter();
    items.retain(|_| !flags.next().unwrap_or(false));
}

/// Whether the dotted `label` resolves only to tables that are partitions.
fn names_partition(model: &ModelSnapshot, dialect: Dialect, label: &str) -> bool {
    let mut parts = label.rsplit('.');
    let Some(name) = parts.next().filter(|n| !n.is_empty()) else {
        return false;
    };
    let reference = ObjectRef {
        catalog: None,
        namespace: parts.next().map(|s| SqlName::new(s, dialect, false)),
        name: SqlName::new(name, dialect, false),
    };
    let entries = model.resolve(&reference, None, None);
    !entries.is_empty()
        && entries
            .iter()
            .all(|e| e.table().is_some_and(|t| t.partition_of().is_some()))
}

/// A plain (non-fuzzy) prefix match on the whole label or its last part.
fn matches_prefix(label: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    let starts = |text: &str, head: &str| {
        text.len() >= head.len()
            && text.is_char_boundary(head.len())
            && text[..head.len()].eq_ignore_ascii_case(head)
    };
    let last = |s: &'_ str| s.rsplit('.').next().unwrap_or("").to_string();
    starts(label, prefix) || starts(&last(label), &last(prefix))
}

#[cfg(test)]
mod tests {
    use crate::intellisense::{
        CompletionRequest, DefaultIntellisense, IntellisenseService, default_config_text, parse,
    };
    use sqlly_core::DatabaseType;
    use sqlly_intel::intellisense::{CompletionCatalog, CompletionKind};
    use sqlly_lang::sqlmodel::{
        Dialect, Fidelity, IncompleteRegions, ModelSnapshot, ObjectBody, ObjectFragment,
        ObjectKind, ObjectRef, Provenance, SqlName, TableBody,
    };

    fn table(name: &str, partition_of: Option<&str>) -> ObjectFragment {
        let dialect = Dialect::Postgres;
        let mut body = TableBody::default();
        if let Some(parent) = partition_of {
            body.options
                .insert(TableBody::PARTITION_OF_OPTION.into(), parent.into());
        }
        ObjectFragment {
            reference: ObjectRef::new(
                None,
                Some(SqlName::new("public", dialect, false)),
                SqlName::new(name, dialect, false),
            ),
            kind: ObjectKind::Table,
            dialect,
            body: ObjectBody::Table(body),
            provenance: Provenance::Database {
                profile: "p1".into(),
            },
            fidelity: Fidelity::Exact,
            references: vec![],
        }
    }

    fn model() -> ModelSnapshot {
        let mut fragments = vec![table("events", None), table("customers", None)];
        for day in 1..=31 {
            fragments.push(table(
                &format!("events_2024_01_{day:02}"),
                Some("public.events"),
            ));
        }
        ModelSnapshot::empty()
            .apply(
                1,
                fragments,
                &std::collections::BTreeSet::new(),
                IncompleteRegions::default(),
            )
            .0
    }

    fn tables(sql: &str) -> Vec<String> {
        let model = model();
        let catalog = CompletionCatalog::from_model_snapshot(&model);
        let cfg = parse(&default_config_text()).unwrap();
        let mut req = CompletionRequest::manual(sql, sql.len(), &catalog, &[], &cfg);
        req.model = Some(&model);
        req.database_type = Some(DatabaseType::Postgres);
        DefaultIntellisense
            .complete_enriched_response(&req)
            .items
            .into_iter()
            .filter(|i| i.kind == CompletionKind::Table)
            .map(|i| i.label)
            .collect()
    }

    fn has(labels: &[String], name: &str) -> bool {
        labels
            .iter()
            .any(|l| l == name || l.ends_with(&format!(".{name}")))
    }

    #[test]
    fn partitions_stay_out_of_from_while_an_ordinary_table_matches() {
        for sql in [
            "SELECT * FROM ",
            "SELECT * FROM ev",
            "SELECT * FROM public.ev",
        ] {
            let labels = tables(sql);
            assert!(has(&labels, "events"), "{sql}: {labels:?}");
            assert!(
                !labels.iter().any(|l| l.contains("events_2024")),
                "{sql}: {labels:?}"
            );
        }
        assert!(has(&tables("SELECT * FROM "), "customers"));
    }

    #[test]
    fn partitions_are_offered_when_only_they_match() {
        for sql in [
            "SELECT * FROM events_2024",
            "SELECT * FROM events_2024_01_0",
            "SELECT * FROM public.events_",
        ] {
            let labels = tables(sql);
            assert!(has(&labels, "events_2024_01_05"), "{sql}: {labels:?}");
        }
    }
}
