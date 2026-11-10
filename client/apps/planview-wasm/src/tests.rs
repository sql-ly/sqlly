//! The JSON contract the page renders, checked against every bundled real
//! plan, plus the paste clean-up and format recognition.

use super::*;
use std::collections::HashSet;

/// Keep in sync with `QueryPlanViewerModel.Formats` (the page's picker).
const PAGE_FORMATS: [&str; 8] = [
    "sqlserver",
    "postgresql",
    "mysql",
    "mariadb",
    "sqlite",
    "duckdb",
    "clickhouse",
    "oracle",
];

fn sample(id: &str) -> &'static Sample {
    SAMPLES.iter().find(|s| s.id == id).expect("sample")
}

fn analyzed(id: &str) -> Value {
    analyze(sample(id).text, "auto").unwrap_or_else(|e| panic!("{id}: {e}"))
}

fn kinds(statement: &Value) -> Vec<String> {
    statement["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["kind"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn every_format_the_page_offers_exists() {
    for id in PAGE_FORMATS {
        assert!(Format::from_id(id).is_some(), "{id}");
    }
    assert_eq!(Format::ALL.len(), PAGE_FORMATS.len());
    assert!(Format::from_id("db2").is_none());
}

#[test]
fn every_sample_is_recognized_as_its_own_format() {
    for s in SAMPLES {
        let doc = analyze(s.text, "auto").unwrap_or_else(|e| panic!("{}: {e}", s.id));
        assert_eq!(doc["format"], s.format.id(), "{}", s.id);
        assert_eq!(doc["detected"], true);
        assert!(
            !doc["statements"].as_array().unwrap().is_empty(),
            "{}",
            s.id
        );
        // Picking the format explicitly reads the same plan.
        let explicit = analyze(s.text, s.format.id()).unwrap();
        assert_eq!(explicit["statements"], doc["statements"], "{}", s.id);
    }
}

/// The structural contract the page's renderer relies on, for every sample.
#[test]
fn every_statement_honours_the_render_contract() {
    for s in SAMPLES {
        let doc = analyzed(s.id);
        // Valid, finite JSON all the way down (serde_json refuses NaN).
        let text = doc.to_string();
        assert!(serde_json::from_str::<Value>(&text).is_ok());
        for statement in doc["statements"].as_array().unwrap() {
            let ids: HashSet<i64> = statement["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n["id"].as_i64().unwrap())
                .collect();
            assert_eq!(
                ids.len(),
                statement["operatorCount"].as_u64().unwrap() as usize,
                "{}",
                s.id
            );
            assert!(matches!(
                statement["kind"].as_str(),
                Some("actual" | "estimated")
            ));
            for finding in statement["findings"].as_array().unwrap() {
                assert!(matches!(
                    finding["severity"].as_str(),
                    Some("critical" | "warning" | "info")
                ));
                for key in ["summary", "why", "action", "detail", "kind"] {
                    assert!(
                        !finding[key].as_str().unwrap_or("").is_empty(),
                        "{} {key}",
                        s.id
                    );
                }
                if let Some(node) = finding["nodeId"].as_i64() {
                    assert!(
                        ids.contains(&node),
                        "{}: finding on unknown node {node}",
                        s.id
                    );
                    assert!(!finding["nodeTitle"].as_str().unwrap().is_empty());
                }
            }
            for edge in statement["edges"].as_array().unwrap() {
                assert!(ids.contains(&edge["from"].as_i64().unwrap()), "{}", s.id);
                assert!(ids.contains(&edge["to"].as_i64().unwrap()), "{}", s.id);
            }
            let tree: Vec<i64> = statement["tree"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["nodeId"].as_i64().unwrap())
                .collect();
            assert_eq!(
                tree.len(),
                ids.len(),
                "{}: every operator in the tree once",
                s.id
            );
            assert_eq!(tree.iter().collect::<HashSet<_>>().len(), tree.len());
            let diagram = &statement["diagram"];
            assert_eq!(
                diagram["nodes"].as_array().unwrap().len(),
                ids.len(),
                "{}",
                s.id
            );
            let (w, h) = (
                diagram["width"].as_f64().unwrap(),
                diagram["height"].as_f64().unwrap(),
            );
            let nw = diagram["nodeWidth"].as_f64().unwrap();
            for n in diagram["nodes"].as_array().unwrap() {
                assert!(n["x"].as_f64().unwrap() + nw <= w, "{}", s.id);
                assert!(n["y"].as_f64().unwrap() < h, "{}", s.id);
            }
            assert_eq!(statement["operators"].as_array().unwrap().len(), ids.len());
            let counts = &statement["findingCounts"];
            let total = counts["critical"].as_u64().unwrap()
                + counts["warning"].as_u64().unwrap()
                + counts["info"].as_u64().unwrap();
            assert_eq!(
                total as usize,
                statement["findings"].as_array().unwrap().len()
            );
        }
    }
}

#[test]
fn sql_server_sample_reports_what_ssms_would_flag() {
    let doc = analyzed("sqlserver");
    let statements = doc["statements"].as_array().unwrap();
    assert_eq!(statements.len(), 3);
    assert!(statements.iter().all(|s| s["kind"] == "actual"));
    assert!(
        statements[0]["statementText"]
            .as_str()
            .unwrap()
            .starts_with("SELECT c.CustomerID")
    );

    let first = kinds(&statements[0]);
    assert!(first.contains(&"missing_index".to_string()), "{first:?}");
    assert!(
        first.contains(&"implicit_conversion".to_string()),
        "{first:?}"
    );
    let missing = statements[0]["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == "missing_index")
        .unwrap();
    assert_eq!(missing["severity"], "critical");
    assert!(
        missing["actionSql"]
            .as_str()
            .unwrap()
            .contains("CREATE NONCLUSTERED INDEX")
    );
    assert_eq!(missing["nodeTitle"], "Clustered Index Scan");

    // The local-variable predicate: estimated 180K rows, read 4,931.
    let second = kinds(&statements[1]);
    assert!(
        second.contains(&"estimate_mismatch".to_string()),
        "{second:?}"
    );
    assert!(second.contains(&"table_scan".to_string()), "{second:?}");

    // TOP (20) over an index: one key lookup per row, a row goal, and no
    // false misestimate on the loop's inner side.
    let third = kinds(&statements[2]);
    assert!(third.contains(&"key_lookup".to_string()), "{third:?}");
    assert_eq!(third.iter().filter(|k| *k == "row_goal").count(), 1);
    assert!(
        !third.contains(&"estimate_mismatch".to_string()),
        "{third:?}"
    );
}

#[test]
fn other_samples_report_their_scans_and_misses() {
    let pg = analyzed("postgresql");
    let pg = &pg["statements"][0];
    assert_eq!(pg["kind"], "actual");
    let k = kinds(pg);
    assert!(
        k.contains(&"table_scan".to_string()) && k.contains(&"estimate_mismatch".to_string()),
        "{k:?}"
    );

    for id in [
        "mysql",
        "mysql-analyze",
        "mariadb",
        "sqlite",
        "duckdb",
        "oracle",
    ] {
        let doc = analyzed(id);
        let k = kinds(&doc["statements"][0]);
        assert!(k.contains(&"table_scan".to_string()), "{id}: {k:?}");
    }
    assert_eq!(analyzed("mariadb")["statements"][0]["kind"], "actual");
    assert_eq!(analyzed("mysql")["statements"][0]["kind"], "estimated");
    assert_eq!(analyzed("oracle")["statements"][0]["kind"], "actual");
    assert_eq!(analyzed("clickhouse")["format"], "clickhouse");
}

#[test]
fn psql_aligned_output_is_unwrapped() {
    let psql = "                 QUERY PLAN                  \n---------------------------------------------\n [                                          +\n   {                                        +\n     \"Plan\": {                             +\n       \"Node Type\": \"Seq Scan\",           +\n       \"Parallel Aware\": false,            +\n       \"Relation Name\": \"orders\",         +\n       \"Total Cost\": 10.5,                 +\n       \"Plan Rows\": 600                    +\n     }                                      +\n   }                                        +\n ]\n(1 row)\n";
    let cleaned = clean_input(psql);
    assert!(cleaned.starts_with('['), "{cleaned}");
    assert!(cleaned.ends_with(']'), "{cleaned}");
    let doc = analyze(psql, "auto").unwrap();
    assert_eq!(doc["format"], "postgresql");
    assert_eq!(kinds(&doc["statements"][0]), vec!["table_scan"]);
}

#[test]
fn mysql_client_borders_and_row_banners_are_removed() {
    let tree = sample("mysql-analyze").text.trim_end();
    let table = format!(
        "+---------+\n| EXPLAIN |\n+---------+\n| {tree} |\n+---------+\n1 row in set (0.09 sec)\n"
    );
    let doc = analyze(&table, "auto").unwrap();
    assert_eq!(doc["format"], "mysql");
    let expected = analyzed("mysql-analyze");
    assert_eq!(doc["statements"], expected["statements"]);

    let json = sample("mysql").text.trim_end();
    let banner = format!(
        "*************************** 1. row ***************************\nEXPLAIN: {json}\n1 row in set, 1 warning (0.00 sec)\n"
    );
    let doc = analyze(&banner, "auto").unwrap();
    assert_eq!(doc["statements"], analyzed("mysql")["statements"]);
}

#[test]
fn windows_line_ends_bom_and_code_fences_are_tolerated() {
    let sqlite = sample("sqlite").text;
    let messy = format!("\u{feff}```text\r\n{}```\r\n", sqlite.replace('\n', "\r\n"));
    let doc = analyze(&messy, "auto").unwrap();
    assert_eq!(doc["format"], "sqlite");
    assert_eq!(doc["statements"], analyzed("sqlite")["statements"]);
}

#[test]
fn shapes_it_cannot_read_say_how_to_capture_one_it_can() {
    let pg_text =
        "Seq Scan on orders  (cost=0.00..10.50 rows=600 width=36)\n  Filter: (total > 1200)";
    let err = analyze(pg_text, "auto").unwrap_err();
    assert!(err.contains("FORMAT JSON"), "{err}");

    let ss_text = "  |--Clustered Index Scan(OBJECT:([Shop].[dbo].[Orders].[PK_Orders]))";
    let err = analyze(ss_text, "auto").unwrap_err();
    assert!(err.contains("SET STATISTICS XML"), "{err}");

    let mysql_v2 = r#"{"query": "SELECT 1", "inputs": [], "operation": "Table scan"}"#;
    let err = analyze(mysql_v2, "auto").unwrap_err();
    assert!(err.contains("explain_json_format_version"), "{err}");

    let err = analyze("hello world", "auto").unwrap_err();
    assert!(err.contains("doesn't look like an execution plan"), "{err}");
    assert!(analyze("   \n", "auto").is_err());
}

#[test]
fn a_forced_format_that_does_not_fit_names_the_engine() {
    let err = analyze(sample("postgresql").text, "sqlserver").unwrap_err();
    assert!(
        err.starts_with("Couldn't read this as a SQL Server plan"),
        "{err}"
    );
    assert!(analyze(sample("postgresql").text, "nope").is_err());
}

#[test]
fn truncated_xml_is_reported_not_guessed() {
    let xml = sample("sqlserver").text;
    let err = analyze(&xml[..xml.len() / 3], "auto").unwrap_err();
    assert!(err.contains("SQL Server"), "{err}");
}

#[test]
fn samples_json_carries_everything_the_picker_shows() {
    let samples = samples_json();
    let list = samples.as_array().unwrap();
    assert_eq!(list.len(), SAMPLES.len());
    let ids: HashSet<&str> = list.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), list.len(), "sample ids are unique");
    for s in list {
        for key in [
            "id", "format", "engine", "title", "capture", "query", "text",
        ] {
            assert!(!s[key].as_str().unwrap_or("").is_empty(), "{key}");
        }
    }
    // One sample per engine at least.
    let formats: HashSet<&str> = list.iter().map(|s| s["format"].as_str().unwrap()).collect();
    assert_eq!(formats.len(), PAGE_FORMATS.len());
}

#[test]
fn warning_labels_read_as_sentences() {
    assert_eq!(
        warning_label(&PlanWarning::TableScan {
            table: "[Orders]".into(),
            schema: "[dbo]".into()
        }),
        "Reads every row of dbo.Orders"
    );
    assert_eq!(
        warning_label(&PlanWarning::KeyLookup {
            table: "Orders".into(),
            index: "PK".into()
        }),
        "Lookup per row into Orders"
    );
}
