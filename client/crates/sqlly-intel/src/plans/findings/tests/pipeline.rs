//! What happens after the rules run: overlapping findings, one index per
//! table, ranking, and the string catalog.

use super::*;
use crate::plans::schema::SchemaContext;
use std::collections::{BTreeMap, BTreeSet};

/// Two sequential scans of `orders` under an Append, each discarding
/// 600,000 rows; the second one filters on `second_filter`.
fn two_scans(second_filter: &str) -> ExecutionPlan {
    let append = node(1, "Append", 40.0, 1.0, 900.0);
    let mut first = read(2, "Seq Scan", "orders", 20.0, 1.0, 500.0);
    first.filter = Some("(status = 'refunded'::text)".into());
    first.rows_removed_by_filter = Some(600_000.0);
    let mut second = read(3, "Seq Scan", "orders", 20.0, 1.0, 390.0);
    second.filter = Some(second_filter.into());
    second.rows_removed_by_filter = Some(600_000.0);
    timed(
        plan(
            PlanDialect::Postgres,
            vec![append, first, second],
            &[(1, 2), (1, 3)],
        ),
        900.0,
    )
}

fn statements(findings: &[PlanFinding]) -> Vec<&str> {
    findings
        .iter()
        .filter_map(|finding| finding.action_sql.as_deref())
        .collect()
}

#[test]
fn the_same_index_is_written_once_for_two_findings() {
    let findings = findings_for_plan(&two_scans("(status = 'lost'::text)"));
    assert_eq!(findings.len(), 2, "{}", render(&findings));
    assert_eq!(
        statements(&findings),
        vec!["CREATE INDEX \"ix_orders_status\" ON \"orders\" (\"status\");"]
    );
    // The more expensive scan carries the statement.
    assert_eq!(findings[0].node_ids, vec![2]);
    assert!(findings[0].action_sql.is_some());
    assert!(has_note(&findings[1], "common.note.same_index"));
    assert!(!has_note(&findings[1], "common.note.write_cost"));
}

#[test]
fn a_wider_index_absorbs_the_suggestion_it_has_as_a_prefix() {
    let findings = findings_for_plan(&two_scans(
        "((status = 'lost'::text) AND (total > '5'::numeric))",
    ));
    assert_eq!(
        statements(&findings),
        vec!["CREATE INDEX \"ix_orders_status_total\" ON \"orders\" (\"status\", \"total\");"]
    );
    let narrow = findings
        .iter()
        .find(|finding| finding.node_ids == vec![2])
        .expect("first scan");
    assert_eq!(narrow.action_sql, None);
    assert!(
        narrow
            .notes
            .iter()
            .any(|note| note.fallback().contains("orders (status, total)")),
        "{}",
        render(&findings)
    );
}

#[test]
fn two_different_indexes_on_one_table_are_both_written_with_a_warning() {
    let findings = findings_for_plan(&two_scans("(customer_id = 7)"));
    assert_eq!(statements(&findings).len(), 2, "{}", render(&findings));
    assert!(!has_note(&findings[0], "common.note.second_index"));
    assert!(has_note(&findings[1], "common.note.second_index"));
    assert!(has_note(&findings[1], "common.note.write_cost"));
}

#[test]
fn an_index_in_the_schema_is_never_suggested_again() {
    let schema = SchemaContext::from_ddl(
        "CREATE TABLE orders (id int PRIMARY KEY, status text, total numeric);\n\
         CREATE INDEX ix_orders_status ON orders (status);",
        PlanDialect::Postgres,
    );
    let findings =
        findings_for_plan_with_schema(&two_scans("(status = 'lost'::text)"), Some(&schema));
    assert!(statements(&findings).is_empty(), "{}", render(&findings));
    assert_none(&findings, K::TableScan);
    assert_eq!(
        findings
            .iter()
            .filter(|finding| finding.kind == K::SchemaIndexNotUsed)
            .count(),
        2
    );
}

#[test]
fn the_schema_note_is_only_shown_when_no_schema_was_supplied() {
    let plan = two_scans("(customer_id = 7)");
    let without = findings_for_plan(&plan);
    assert!(
        without
            .iter()
            .all(|finding| has_note(finding, "common.note.needs_schema"))
    );
    let schema = SchemaContext::from_ddl(
        "CREATE TABLE orders (id int PRIMARY KEY, status text, customer_id int);",
        PlanDialect::Postgres,
    );
    let with = findings_for_plan_with_schema(&plan, Some(&schema));
    assert_eq!(statements(&with).len(), 2, "{}", render(&with));
    assert!(
        with.iter()
            .all(|finding| !has_note(finding, "common.note.needs_schema"))
    );
}

#[test]
fn a_column_the_schema_does_not_have_gets_no_statement() {
    // The plan filters on `status`, the supplied table has no such column:
    // the advice must not write DDL against a column it cannot confirm.
    let schema = SchemaContext::from_ddl(
        "CREATE TABLE orders (id int PRIMARY KEY, state text);",
        PlanDialect::Postgres,
    );
    let findings =
        findings_for_plan_with_schema(&two_scans("(status = 'lost'::text)"), Some(&schema));
    assert!(statements(&findings).is_empty(), "{}", render(&findings));
}

#[test]
fn findings_are_ordered_by_rank_and_the_rank_follows_the_time_at_stake() {
    let findings = findings_for_plan(&two_scans("(customer_id = 7)"));
    assert!(findings.windows(2).all(|pair| pair[0].rank >= pair[1].rank));
    assert_eq!(findings[0].node_ids, vec![2]);
    let first = findings[0].impact.expect("timed");
    let second = findings[1].impact.expect("timed");
    assert!(first.share > second.share);
    assert_eq!(first.time_ms, Some(500.0));
}

#[test]
fn sample_plans_rank_the_biggest_saving_first() {
    let plans = sample("sqlserver-actual.xml");
    let findings = findings_for_plan(&plans[0]);
    assert_eq!(
        findings
            .iter()
            .map(|finding| finding.kind)
            .collect::<Vec<_>>(),
        vec![K::MissingIndex, K::ImplicitConversion, K::MemoryGrant],
        "{}",
        render(&findings)
    );
    // The optimizer's own index request replaces the generic scan finding
    // on the same operator.
    assert_none(&findings, K::TableScan);
    assert_eq!(
        sql(&findings[0]),
        "CREATE INDEX [IX_Orders_CustomerID] ON [dbo].[Orders] ([CustomerID]) INCLUDE ([OrderDate], [Status], [Total]);"
    );

    let plans = sample("postgresql-analyze.json");
    let findings = findings_for_plan(&plans[0]);
    assert_eq!(
        findings
            .iter()
            .map(|finding| finding.kind)
            .collect::<Vec<_>>(),
        vec![K::TableScan, K::NonSargable],
        "{}",
        render(&findings)
    );
    assert_eq!(
        sql(&findings[1]),
        "CREATE INDEX \"ix_customers_expr\" ON \"customers\" (lower(email));"
    );
}

#[test]
fn healthy_sample_plans_have_no_findings() {
    for name in [
        "clickhouse.json",
        "duckdb-analyze.json",
        "oracle-allstats.txt",
        "postgresql-analyze.txt",
        "sqlite.txt",
        "mysql-table.txt",
    ] {
        for plan in sample(name) {
            let findings = findings_for_plan(&plan);
            assert!(findings.is_empty(), "{name}:\n{}", render(&findings));
        }
    }
}

#[test]
fn every_finding_names_its_kind_category_and_operators() {
    for name in [
        "sqlserver-actual.xml",
        "postgresql-analyze.json",
        "mysql-analyze.txt",
    ] {
        for plan in sample(name) {
            for finding in findings_for_plan(&plan) {
                assert_eq!(finding.category, finding.kind.category());
                assert_eq!(K::from_id(finding.kind.as_str()), Some(finding.kind));
                assert_eq!(finding.node_id, finding.node_ids.first().copied());
                for id in &finding.node_ids {
                    assert!(plan.nodes.iter().any(|node| node.id == *id));
                }
                for text in [&finding.summary, &finding.why, &finding.action] {
                    assert!(text.key().is_some(), "{:?}", text);
                    assert!(!text.fallback().contains('{'), "{}", text.fallback());
                }
            }
        }
    }
}

#[test]
fn kind_ids_are_unique_and_round_trip() {
    let mut seen = BTreeSet::new();
    for kind in K::ALL {
        assert!(seen.insert(kind.as_str()), "duplicate id {}", kind.as_str());
        assert_eq!(K::from_id(kind.as_str()), Some(*kind));
    }
    assert_eq!(K::from_id("no_such_kind"), None);
}

fn placeholders(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                found.insert(after[..close].to_string());
                rest = &after[close + 1..];
            }
            None => break,
        }
    }
    found
}

fn flatten(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
    let Some(object) = value.as_object() else {
        return;
    };
    if let Some(text) = object.get("value").and_then(|value| value.as_str()) {
        out.insert(prefix.to_string(), text.to_string());
        return;
    }
    for (key, child) in object {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        flatten(&path, child, out);
    }
}

fn english_catalog() -> BTreeMap<String, String> {
    let text = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../sqlly-i18n/translations/en.json"
    ));
    let root: serde_json::Value = serde_json::from_str(text).expect("en.json");
    let mut out = BTreeMap::new();
    flatten("", &root["services"]["plans"]["findings"], &mut out);
    out
}

#[test]
fn templates_have_unique_keys_and_valid_placeholders() {
    let mut seen = BTreeSet::new();
    for (short, english, what) in strings::all_templates() {
        assert!(seen.insert(*short), "duplicate template {short}");
        assert!(!what.is_empty(), "{short} has no translator note");
        for segment in short.split('.') {
            assert!(
                segment.starts_with(|c: char| c.is_ascii_lowercase())
                    && segment
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                    && !segment.ends_with('_')
                    && !segment.contains("__"),
                "bad key segment in {short}"
            );
        }
        for name in placeholders(english) {
            assert!(
                !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "bad placeholder {{{name}}} in {short}"
            );
        }
    }
}

/// The English catalog carries every finding string with the same text, so
/// a translated catalog renders what the fallback says.
#[test]
fn the_english_catalog_matches_the_templates() {
    let catalog = english_catalog();
    for (short, english, _) in strings::all_templates() {
        match catalog.get(*short) {
            Some(value) => assert_eq!(value, english, "en.json differs for {short}"),
            None => panic!("en.json has no services.plans.findings.{short}"),
        }
    }
}

/// Prints the catalog entries for every template as JSON lines
/// (`key`, `value`, `location`), for adding them to the translation files.
/// `cargo test -p sqlly-intel --lib findings::tests::pipeline::dump_templates -- --ignored --nocapture`
#[test]
#[ignore = "prints the string catalog entries"]
fn dump_templates() {
    for (short, english, what) in strings::all_templates() {
        let entry = serde_json::json!({
            "key": format!("services.plans.findings.{short}"),
            "value": english,
            "location": strings::location(short, what),
        });
        println!("TEMPLATE {entry}");
    }
}
