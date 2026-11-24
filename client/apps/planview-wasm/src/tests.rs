//! The JSON contract the page renders, checked against every bundled real
//! plan, plus the paste clean-up, format recognition, the schema and query
//! inputs, diagram re-layout and plan comparison.

use super::*;
use sqlly_intel::plans::warning::PlanWarning;
use std::collections::HashSet;

/// Keep in sync with `QueryPlanViewerModel.Formats` (the page's picker).
const PAGE_FORMATS: [&str; 11] = [
    "sqlserver",
    "postgresql",
    "mysql",
    "mariadb",
    "sqlite",
    "duckdb",
    "clickhouse",
    "oracle",
    "cockroachdb",
    "trino",
    "spark",
];

fn sample(id: &str) -> &'static Sample {
    SAMPLES.iter().find(|s| s.id == id).expect("sample")
}

fn auto() -> Options {
    Options::format("auto")
}

/// The sample's plan alone: no table definitions, no query text.
fn analyzed(id: &str) -> Value {
    analyze(sample(id).text, &auto()).unwrap_or_else(|e| panic!("{id}: {e}"))
}

/// The sample as the page loads it, with its schema and query.
fn analyzed_with_context(id: &str) -> Value {
    let s = sample(id);
    let options = Options {
        schema: s.schema.to_string(),
        query: s.query.to_string(),
        ..auto()
    };
    analyze(s.text, &options).unwrap_or_else(|e| panic!("{id}: {e}"))
}

fn kinds(statement: &Value) -> Vec<String> {
    statement["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["kind"].as_str().unwrap().to_string())
        .collect()
}

fn finding<'a>(statement: &'a Value, kind: &str) -> &'a Value {
    statement["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} finding in {:?}", kinds(statement)))
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

#[test]
fn every_format_the_page_offers_exists() {
    for id in PAGE_FORMATS {
        assert!(Format::from_id(id).is_some(), "{id}");
    }
    assert_eq!(Format::ALL.len(), PAGE_FORMATS.len());
    assert!(Format::from_id("db2").is_none());
    for format in Format::ALL {
        assert_eq!(format.id(), format.plan_engine().id());
    }
    // The engines the page says it does not read stay unread.
    for id in ["snowflake", "bigquery", "databricks"] {
        assert!(Format::from_id(id).is_none(), "{id}");
    }
}

#[test]
fn every_sample_is_recognized_as_its_own_format() {
    for s in SAMPLES {
        let doc = analyze(s.text, &auto()).unwrap_or_else(|e| panic!("{}: {e}", s.id));
        assert_eq!(doc["format"], s.format.id(), "{}", s.id);
        assert_eq!(doc["detected"], true);
        assert!(
            !doc["statements"].as_array().unwrap().is_empty(),
            "{}",
            s.id
        );
        // Picking the format explicitly reads the same plan.
        let explicit = analyze(s.text, &Options::format(s.format.id())).unwrap();
        assert_eq!(explicit["statements"], doc["statements"], "{}", s.id);
        assert_eq!(explicit["detected"], false, "{}", s.id);
        if !s.after.is_empty() {
            let after = analyze(s.after, &auto()).unwrap_or_else(|e| panic!("{}: {e}", s.id));
            assert_eq!(after["format"], s.format.id(), "{} after", s.id);
        }
    }
}

/// The structural contract the page's renderer relies on, for every sample,
/// with and without its schema and query.
#[test]
fn every_statement_honours_the_render_contract() {
    for s in SAMPLES {
        for doc in [analyzed(s.id), analyzed_with_context(s.id)] {
            // Valid, finite JSON all the way down (serde_json refuses NaN).
            assert!(serde_json::from_str::<Value>(&doc.to_string()).is_ok());
            for statement in doc["statements"].as_array().unwrap() {
                check_statement(s.id, statement);
            }
        }
    }
}

fn check_statement(id: &str, statement: &Value) {
    let nodes = statement["nodes"].as_array().unwrap();
    let ids: HashSet<i64> = nodes.iter().map(|n| n["id"].as_i64().unwrap()).collect();
    assert_eq!(ids.len(), nodes.len(), "{id}: node ids are unique");
    assert_eq!(
        statement["facts"]["operatorCount"].as_u64().unwrap() as usize,
        ids.len(),
        "{id}"
    );
    assert!(matches!(
        statement["kind"].as_str(),
        Some("actual" | "estimated")
    ));
    for node in nodes {
        assert!(!text(&node["title"]).is_empty(), "{id}");
        assert!(!text(&node["category"]).is_empty(), "{id}");
        assert!(
            node["step"].as_u64().unwrap() >= 1,
            "{id}: every node is a step"
        );
        if let Some(parent) = node["parentId"].as_i64() {
            assert!(ids.contains(&parent), "{id}");
        }
        for metric in ["time", "cost", "rows", "estimate", "io"] {
            let heat = node["heat"][metric].as_f64().unwrap();
            assert!((0.0..=1.0).contains(&heat), "{id}: heat {metric} {heat}");
        }
    }

    // The story names every operator once, in execution order.
    let story = &statement["story"];
    assert!(!text(&story["headline"]).is_empty(), "{id}");
    let steps = story["steps"].as_array().unwrap();
    assert_eq!(steps.len(), ids.len(), "{id}");
    for (position, step) in steps.iter().enumerate() {
        assert_eq!(step["number"].as_u64().unwrap() as usize, position + 1);
        assert!(ids.contains(&step["nodeId"].as_i64().unwrap()), "{id}");
        assert!(!text(&step["sentence"]).is_empty(), "{id}");
    }
    if let Some(node) = story["bottleneck"]["nodeId"].as_i64() {
        assert!(ids.contains(&node), "{id}");
    }

    for finding in statement["findings"].as_array().unwrap() {
        assert!(matches!(
            finding["severity"].as_str(),
            Some("critical" | "warning" | "info")
        ));
        assert!(matches!(
            finding["confidence"].as_str(),
            Some("high" | "medium" | "low")
        ));
        for key in ["summary", "why", "action", "detail", "kind", "category"] {
            assert!(!text(&finding[key]).is_empty(), "{id} {key}");
        }
        if let Some(node) = finding["nodeId"].as_i64() {
            assert!(ids.contains(&node), "{id}: finding on unknown node {node}");
            assert!(!text(&finding["nodeTitle"]).is_empty());
            assert!(finding["step"].as_u64().is_some(), "{id}");
        }
        for link in finding["learn"].as_array().unwrap() {
            assert!(text(&link["path"]).starts_with('/'), "{id}");
            assert!(!text(&link["title"]).is_empty(), "{id}");
        }
    }
    let counts = &statement["findingCounts"];
    let total = counts["critical"].as_u64().unwrap()
        + counts["warning"].as_u64().unwrap()
        + counts["info"].as_u64().unwrap();
    assert_eq!(
        total as usize,
        statement["findings"].as_array().unwrap().len()
    );

    // The diagram: every operator a box inside the canvas, every edge
    // between two boxes with a routed line.
    let diagram = &statement["diagram"];
    let boxes = diagram["nodes"].as_array().unwrap();
    assert_eq!(boxes.len(), ids.len(), "{id}");
    let (w, h) = (
        diagram["width"].as_f64().unwrap(),
        diagram["height"].as_f64().unwrap(),
    );
    for b in boxes {
        assert!(ids.contains(&b["id"].as_i64().unwrap()), "{id}");
        assert!(b["x"].as_f64().unwrap() >= 0.0 && b["y"].as_f64().unwrap() >= 0.0);
        assert!(
            b["x"].as_f64().unwrap() + b["w"].as_f64().unwrap() <= w + 0.5,
            "{id}"
        );
        assert!(
            b["y"].as_f64().unwrap() + b["h"].as_f64().unwrap() <= h + 0.5,
            "{id}"
        );
    }
    let edges = diagram["edges"].as_array().unwrap();
    assert_eq!(
        edges.len(),
        nodes.iter().filter(|n| !n["parentId"].is_null()).count(),
        "{id}"
    );
    for edge in edges {
        assert!(ids.contains(&edge["from"].as_i64().unwrap()), "{id}");
        assert!(ids.contains(&edge["to"].as_i64().unwrap()), "{id}");
        assert!(edge["points"].as_array().unwrap().len() >= 2, "{id}");
        assert!(edge["thickness"].as_f64().unwrap() > 0.0, "{id}");
    }

    // The operator table: one row per operator, a cell per column.
    let table = &statement["table"];
    let columns = table["columns"].as_array().unwrap();
    assert!(columns.len() >= 2, "{id}");
    let rows = table["rows"].as_array().unwrap();
    assert_eq!(rows.len(), ids.len(), "{id}");
    for row in rows {
        for column in columns {
            let cell = &row["cells"][text(&column["key"])];
            assert!(
                cell.is_null() || cell.as_array().is_some_and(|c| c.len() == 2),
                "{id}: {cell}"
            );
        }
    }

    // Metrics the page offers include the one it opens on.
    let metrics: Vec<&str> = statement["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| text(&m["key"]))
        .collect();
    assert!(
        metrics.contains(&text(&statement["facts"]["primaryMetric"])),
        "{id}: {metrics:?}"
    );
    for metric in ["time", "cost"] {
        let flame = &statement["flame"][metric];
        if flame.is_null() {
            continue;
        }
        for cell in flame["cells"].as_array().unwrap() {
            let (x, width) = (cell["x"].as_f64().unwrap(), cell["width"].as_f64().unwrap());
            assert!(x >= -1e-9 && x + width <= 1.0 + 1e-6, "{id}: {x}+{width}");
            assert!(ids.contains(&cell["nodeId"].as_i64().unwrap()), "{id}");
        }
    }
}

#[test]
fn sql_server_sample_reports_what_the_plan_shows() {
    let doc = analyzed("sqlserver");
    let statements = doc["statements"].as_array().unwrap();
    assert_eq!(statements.len(), 3);
    assert!(statements.iter().all(|s| s["kind"] == "actual"));
    assert!(text(&statements[0]["statementText"]).starts_with("SELECT c.CustomerID"));
    // Each statement's share of the batch, for the switcher.
    let shares: f64 = statements
        .iter()
        .map(|s| s["share"].as_f64().unwrap())
        .sum();
    assert!((shares - 1.0).abs() < 0.01, "{shares}");

    // The scan of Orders and the optimizer's own missing-index request are
    // one finding, not two, with the index written out. It is a warning,
    // not critical: the statement ran in 26 ms.
    let first = kinds(&statements[0]);
    assert_eq!(
        first,
        ["missing_index", "implicit_conversion", "memory_grant"]
    );
    let missing = finding(&statements[0], "missing_index");
    assert_eq!(missing["severity"], "warning");
    assert_eq!(
        missing["actionSql"],
        "CREATE INDEX [IX_Orders_CustomerID] ON [dbo].[Orders] ([CustomerID]) INCLUDE ([OrderDate], [Status], [Total]);"
    );
    assert!(text(&missing["nodeTitle"]).starts_with("Clustered Index Scan on Orders"));
    assert_eq!(missing["impact"]["optimizerEstimate"], true);
    assert!(
        text(&finding(&statements[0], "implicit_conversion")["summary"]).contains("c.Email"),
        "the conversion names the column"
    );

    // The local-variable predicate: estimated 180K rows, read 4,931, and
    // the advice is about the variable, not about statistics.
    assert_eq!(
        kinds(&statements[1]),
        ["missing_index", "estimate_mismatch"]
    );
    let estimate = finding(&statements[1], "estimate_mismatch");
    assert!(text(&estimate["summary"]).contains("@since"));
    assert_eq!(estimate["actionSql"], "OPTION (RECOMPILE)");

    // TOP (20) over an index: 20 key lookups in under a millisecond is a
    // healthy plan, so nothing is flagged.
    assert_eq!(kinds(&statements[2]), Vec::<String>::new());
    assert!(text(&statements[2]["story"]["text"]).contains("Key Lookup"));
}

#[test]
fn the_default_sample_shows_three_problems_and_the_schema_adds_a_fourth() {
    let plain = analyzed("postgresql");
    let statement = &plain["statements"][0];
    assert_eq!(statement["kind"], "actual");
    assert_eq!(
        kinds(statement),
        ["estimate_mismatch", "nested_loop_volume", "spill"]
    );
    assert!(plain["schema"].is_null());
    assert!(
        statement["findings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["usedSchema"] == false)
    );
    let spill = finding(statement, "spill");
    assert_eq!(spill["actionSql"], "SET work_mem = '16MB';");
    assert!(
        spill["learn"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l["path"] == "/sql-performance/sort-and-hash-spills")
    );
    // The story's bottleneck is the inner side of the loop the finding is
    // about: the products lookup that ran 810,000 times.
    assert!(text(&statement["story"]["bottleneck"]["message"]).contains("products_pkey"));
    assert!(text(&finding(statement, "nested_loop_volume")["summary"]).contains("810K times"));

    let with = analyzed_with_context("postgresql");
    assert_eq!(with["schema"]["tables"], 4);
    assert_eq!(with["schema"]["indexes"], 7);
    assert_eq!(with["schema"]["skipped"], 0);
    let statement = &with["statements"][0];
    assert_eq!(
        kinds(statement),
        [
            "estimate_mismatch",
            "nested_loop_volume",
            "spill",
            "schema_fk_unindexed"
        ]
    );
    let fk = finding(statement, "schema_fk_unindexed");
    assert_eq!(fk["usedSchema"], true);
    assert_eq!(
        fk["actionSql"],
        "CREATE INDEX \"ix_orders_customer_id\" ON \"orders\" (\"customer_id\");"
    );
    assert_eq!(finding(statement, "spill")["usedSchema"], false);
}

#[test]
fn the_schema_replaces_a_guess_with_what_the_tables_say() {
    // Without definitions the scan of orders is a generic "index the join
    // column" suggestion.
    let plain = analyzed("postgresql-lookup");
    assert_eq!(
        kinds(&plain["statements"][0]),
        ["non_sargable", "table_scan"]
    );
    // With them, the same operator is explained by the foreign key that has
    // no index, and the generic suggestion is listed as replaced.
    let with = analyzed_with_context("postgresql-lookup");
    let statement = &with["statements"][0];
    assert_eq!(kinds(statement), ["non_sargable", "schema_fk_unindexed"]);
    let expression = finding(statement, "non_sargable");
    assert_eq!(
        expression["actionSql"],
        "CREATE INDEX \"ix_customers_expr\" ON \"customers\" (lower(email));"
    );
    assert_eq!(expression["usedSchema"], false);
    assert_eq!(
        finding(statement, "schema_fk_unindexed")["usedSchema"],
        true
    );
    let ruled_out = statement["ruledOut"].as_array().unwrap();
    assert_eq!(ruled_out.len(), 1);
    assert_eq!(ruled_out[0]["kind"], "table_scan");

    // Definitions that are not DDL are counted as skipped, not guessed at.
    let options = Options {
        schema: "this is not a table definition".to_string(),
        ..auto()
    };
    let doc = analyze(sample("postgresql-lookup").text, &options).unwrap();
    assert_eq!(doc["schema"]["tables"], 0);
    assert_eq!(kinds(&doc["statements"][0]), kinds(&plain["statements"][0]));
}

#[test]
fn query_text_puts_table_names_back_where_the_plan_printed_aliases() {
    let s = sample("mysql-subquery");
    let plain = analyzed("mysql-subquery");
    assert_eq!(plain["printsAliases"], true);
    assert_eq!(plain["aliasesResolved"], 0);
    assert!(text(&plain["statements"][0]["story"]["text"]).contains("Table scan on c"));

    let options = Options {
        query: s.query.to_string(),
        ..auto()
    };
    let doc = analyze(s.text, &options).unwrap();
    assert!(doc["aliasesResolved"].as_u64().unwrap() >= 2, "{doc:#}");
    let statement = &doc["statements"][0];
    let aliases = statement["aliases"].as_array().unwrap();
    assert!(
        aliases
            .iter()
            .any(|a| a["printed"] == "c" && a["table"] == "customers"),
        "{aliases:?}"
    );
    assert!(text(&statement["story"]["headline"]).contains("customers"));
    assert_eq!(statement["statementText"], s.query.trim());
    // PostgreSQL prints table names next to aliases already.
    assert_eq!(analyzed("postgresql")["printsAliases"], false);
}

#[test]
fn other_samples_report_their_scans_and_misses() {
    for id in ["mysql-analyze", "mysql-subquery", "mariadb"] {
        let doc = analyzed(id);
        let scan = finding(&doc["statements"][0], "table_scan");
        assert_eq!(scan["severity"], "warning", "{id}");
        assert!(text(&scan["actionSql"]).starts_with("CREATE INDEX"), "{id}");
    }
    // An estimated plan can only suggest: low confidence, informational.
    let mysql = analyzed("mysql");
    assert_eq!(mysql["statements"][0]["kind"], "estimated");
    let scan = finding(&mysql["statements"][0], "table_scan");
    assert_eq!(scan["severity"], "info");
    assert_eq!(scan["confidence"], "low");

    let oracle = analyzed("oracle");
    assert_eq!(oracle["statements"][0]["kind"], "actual");
    // The top-N filter over the window sort is not a statistics problem.
    assert_eq!(kinds(&oracle["statements"][0]), ["spill"]);

    assert_eq!(analyzed("mariadb")["statements"][0]["kind"], "actual");
    assert_eq!(analyzed("clickhouse")["format"], "clickhouse");

    // Plans with nothing wrong say nothing: the page shows its "no problems
    // found" state for these.
    for id in [
        "postgresql-text",
        "duckdb",
        "cockroachdb",
        "trino",
        "sqlite",
    ] {
        assert_eq!(
            kinds(&analyzed(id)["statements"][0]),
            Vec::<String>::new(),
            "{id}"
        );
    }
}

#[test]
fn psql_aligned_output_is_unwrapped() {
    let psql = "                 QUERY PLAN                  \n---------------------------------------------\n [                                          +\n   {                                        +\n     \"Plan\": {                             +\n       \"Node Type\": \"Seq Scan\",           +\n       \"Parallel Aware\": false,            +\n       \"Relation Name\": \"orders\",         +\n       \"Total Cost\": 10.5,                 +\n       \"Plan Rows\": 600                    +\n     }                                      +\n   }                                        +\n ]\n(1 row)\n";
    let cleaned = clean_input(psql);
    assert!(cleaned.starts_with('['), "{cleaned}");
    assert!(cleaned.ends_with(']'), "{cleaned}");
    let doc = analyze(psql, &auto()).unwrap();
    assert_eq!(doc["format"], "postgresql");
    let statement = &doc["statements"][0];
    assert_eq!(statement["facts"]["operatorCount"], 1);
    assert_eq!(statement["nodes"][0]["object"], "orders");
    // An unfiltered 600-row estimated scan is not a problem.
    assert_eq!(kinds(statement), Vec::<String>::new());
}

#[test]
fn mysql_client_borders_and_row_banners_are_removed() {
    let tree = sample("mysql-analyze").text.trim_end();
    let table = format!(
        "+---------+\n| EXPLAIN |\n+---------+\n| {tree} |\n+---------+\n1 row in set (0.09 sec)\n"
    );
    let doc = analyze(&table, &auto()).unwrap();
    assert_eq!(doc["format"], "mysql");
    let expected = analyzed("mysql-analyze");
    assert_eq!(doc["statements"], expected["statements"]);

    let json = sample("mysql").text.trim_end();
    let banner = format!(
        "*************************** 1. row ***************************\nEXPLAIN: {json}\n1 row in set, 1 warning (0.00 sec)\n"
    );
    let doc = analyze(&banner, &auto()).unwrap();
    assert_eq!(doc["statements"], analyzed("mysql")["statements"]);
}

#[test]
fn windows_line_ends_bom_and_code_fences_are_tolerated() {
    let sqlite = sample("sqlite").text;
    let messy = format!("\u{feff}```text\r\n{}```\r\n", sqlite.replace('\n', "\r\n"));
    let doc = analyze(&messy, &auto()).unwrap();
    assert_eq!(doc["format"], "sqlite");
    assert_eq!(doc["statements"], analyzed("sqlite")["statements"]);
}

#[test]
fn utf16_plan_files_decode() {
    let xml = sample("sqlserver").text;
    let mut bytes = vec![0xFF, 0xFE];
    for unit in xml.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let decoded = decode_bytes(&bytes);
    let doc = analyze(&decoded, &auto()).unwrap();
    assert_eq!(doc["statements"], analyzed("sqlserver")["statements"]);
    assert_eq!(decode_bytes(xml.as_bytes()), xml);
}

#[test]
fn every_output_shape_is_read_as_what_it_is() {
    let pg_text =
        "Seq Scan on orders  (cost=0.00..10.50 rows=600 width=36)\n  Filter: (total > 1200)";
    let doc = analyze(pg_text, &auto()).unwrap();
    assert_eq!(doc["format"], "postgresql");
    assert_eq!(doc["sourceFormat"], "postgresql-text");
    assert_eq!(doc["statements"][0]["facts"]["operatorCount"], 1);

    for (id, format, source) in [
        ("postgresql-text", "postgresql", "postgresql-text"),
        ("sqlserver-text", "sqlserver", "sqlserver-text"),
        ("mysql-table", "mysql", "mysql-table"),
        ("duckdb-text", "duckdb", "duckdb-text"),
        ("clickhouse-text", "clickhouse", "clickhouse-text"),
        ("cockroachdb", "cockroachdb", "cockroachdb-text"),
        ("trino", "trino", "trino-text"),
        ("spark", "spark", "spark-text"),
    ] {
        let doc = analyzed(id);
        assert_eq!(doc["format"], format, "{id}");
        assert_eq!(doc["sourceFormat"], source, "{id}");
        assert!(!text(&doc["sourceFormatLabel"]).is_empty());
    }
    for id in [
        "postgresql-text",
        "sqlserver-text",
        "duckdb-text",
        "cockroachdb",
        "trino",
    ] {
        assert_eq!(analyzed(id)["statements"][0]["kind"], "actual", "{id}");
    }
    for id in ["mysql-table", "clickhouse-text", "spark"] {
        assert_eq!(analyzed(id)["statements"][0]["kind"], "estimated", "{id}");
    }
}

#[test]
fn shapes_it_cannot_read_say_what_they_are_and_what_to_run() {
    let bytecode = "addr  opcode         p1    p2    p3    p4             p5  comment\n----  -------------  ----  ----  ----  -------------  --  -------------\n0     Init           0     8     0                    0   Start at 8\n1     OpenRead       0     2     0     3              0   root=2 iDb=0; orders\n2     Rewind         0     7     0                    0\n";
    let err = analyze(bytecode, &auto()).unwrap_err();
    assert!(err.message.contains("SQLite bytecode"), "{err}");
    assert!(err.message.contains("EXPLAIN QUERY PLAN"), "{err}");
    assert_eq!(
        err.label.as_deref(),
        Some("SQLite bytecode from plain EXPLAIN")
    );
    assert!(err.hint.as_deref().unwrap().contains("EXPLAIN QUERY PLAN"));
    assert_eq!(
        detect(&clean_input(bytecode)),
        Detected::Unsupported(err.message.clone())
    );

    let err = analyze("hello world", &auto()).unwrap_err();
    assert!(
        err.message.contains("does not look like an execution plan"),
        "{err}"
    );
    assert_eq!(detect("hello world"), Detected::Unknown);
    assert!(analyze("   \n", &auto()).is_err());
}

#[test]
fn a_picked_format_that_does_not_fit() {
    let pg = sample("postgresql").text;
    // Certainly another engine's plan: read as what it is, and say so.
    let doc = analyze(pg, &Options::format("sqlserver")).unwrap();
    assert_eq!(doc["format"], "postgresql");
    assert_eq!(doc["detected"], true);
    // The picked engine's own plan is not reported as detected.
    assert_eq!(
        analyze(pg, &Options::format("postgresql")).unwrap()["detected"],
        false
    );
    // Nothing recognizable: the error names the picked engine and how to
    // capture its plan.
    let err = analyze("hello world", &Options::format("sqlserver")).unwrap_err();
    assert!(err.message.contains("SQL Server"), "{err}");
    assert!(err.message.contains("SET STATISTICS XML"), "{err}");
    assert!(analyze(pg, &Options::format("nope")).is_err());
}

#[test]
fn a_broken_plan_says_where_reading_stopped() {
    let xml = sample("sqlserver").text;
    let err = analyze(&xml[..xml.len() / 3], &auto()).unwrap_err();
    assert!(err.message.contains("SQL Server"), "{err}");
    assert_eq!(err.format, Some(Format::SqlServer));

    let json = sample("postgresql").text;
    let cut = &json[..json.len() / 2];
    let err = analyze(cut, &auto()).unwrap_err();
    assert_eq!(err.format, Some(Format::Postgres), "{err:?}");
    let value = err.to_json();
    assert_eq!(value["format"], "postgresql");
    assert_eq!(value["engine"], "PostgreSQL");
    assert!(!text(&value["message"]).is_empty());
    assert!(!text(&value["hint"]).is_empty(), "{value}");
    if let Some(line) = err.line {
        assert!(line >= 1 && line <= cut.lines().count() + 1, "{line}");
    }
}

#[test]
fn the_page_envelope_always_parses() {
    let pg = sample("postgresql");
    let options = json!({ "format": "auto", "schema": pg.schema, "query": pg.query,
        "orientation": "root_left", "metric": "rows" })
    .to_string();
    let ok: Value = serde_json::from_str(&analyze_json(pg.text, &options)).unwrap();
    assert_eq!(ok["ok"], true);
    assert_eq!(ok["schema"]["tables"], 4);
    assert_eq!(ok["statements"][0]["diagram"]["orientation"], "root_left");
    assert_eq!(ok["statements"][0]["diagram"]["metric"], "rows");

    // A bare format id is accepted where the options object goes.
    let bare: Value = serde_json::from_str(&analyze_json(pg.text, "postgresql")).unwrap();
    assert_eq!(bare["ok"], true);
    assert_eq!(bare["detected"], false);

    let bad: Value = serde_json::from_str(&analyze_json("hello world", "{}")).unwrap();
    assert_eq!(bad["ok"], false);
    assert!(text(&bad["error"]["message"]).contains("does not look like"));
}

#[test]
fn the_diagram_is_laid_out_again_on_request() {
    let doc = analyzed("postgresql");
    let statement = &doc["statements"][0];
    let first = &statement["diagram"];
    assert_eq!(first["orientation"], "root_top");
    let total = statement["nodes"].as_array().unwrap().len();

    let request = |value: Value| DiagramRequest::from_json(&value);
    let turned = layout_diagram(0, &request(json!({ "orientation": "root_left" }))).unwrap();
    assert_eq!(turned["orientation"], "root_left");
    assert_eq!(turned["nodes"].as_array().unwrap().len(), total);
    assert_ne!(turned["width"], first["width"]);

    // Collapsing the operator with the largest subtree below the root
    // hides its inputs and says how many.
    let target = statement["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|n| !n["parentId"].is_null())
        .max_by_key(|n| n["subtreeSize"].as_u64().unwrap())
        .unwrap();
    let id = target["id"].as_i64().unwrap();
    let hidden = target["subtreeSize"].as_u64().unwrap() as usize - 1;
    assert!(hidden >= 2);
    let collapsed = layout_diagram(0, &request(json!({ "collapsed": [id] }))).unwrap();
    let boxes = collapsed["nodes"].as_array().unwrap();
    assert_eq!(boxes.len(), total - hidden);
    let folded = boxes.iter().find(|b| b["id"] == id).unwrap();
    assert_eq!(folded["collapsed"], true);
    assert_eq!(folded["hiddenCount"].as_u64().unwrap() as usize, hidden);

    for metric in ["time", "cost", "rows", "estimate", "io"] {
        let laid = layout_diagram(0, &request(json!({ "metric": metric }))).unwrap();
        assert_eq!(laid["metric"], metric);
    }
    assert!(layout_diagram(7, &DiagramRequest::default()).is_none());
}

#[test]
fn two_captures_of_one_statement_compare() {
    let s = sample("postgresql-lookup");
    let changes = compare(s.text, s.after, "auto", s.query).unwrap();
    assert!(!text(&changes["headline"]).is_empty());
    let statement = &changes["statements"][0];
    assert_eq!(statement["effect"], "better", "{changes:#}");
    assert_eq!(statement["sameShape"], false);
    let list = statement["changes"].as_array().unwrap();
    assert!(!list.is_empty());
    assert!(list.iter().any(|c| c["structural"] == true));
    let all: String = list.iter().map(|c| text(&c["message"])).collect();
    assert!(all.contains("customers_email_lower_idx"), "{all}");

    // The same plan twice has nothing to report as a regression.
    let same = compare(s.text, s.text, "auto", "").unwrap();
    assert_eq!(same["statements"][0]["sameShape"], true);

    let err = compare(s.text, sample("mysql").text, "auto", "").unwrap_err();
    assert!(err.message.contains("same engine"), "{err}");
    assert!(compare("", s.text, "auto", "").is_err());

    let envelope: Value = serde_json::from_str(&compare_json(s.text, s.after, "auto", "")).unwrap();
    assert_eq!(envelope["ok"], true);
    assert!(envelope["changes"]["statements"].is_array());
}

#[test]
fn samples_json_carries_everything_the_picker_shows() {
    let samples = samples_json();
    let list = samples.as_array().unwrap();
    assert_eq!(list.len(), SAMPLES.len());
    let ids: HashSet<&str> = list.iter().map(|s| text(&s["id"])).collect();
    assert_eq!(ids.len(), list.len(), "sample ids are unique");
    for s in list {
        for key in [
            "id", "format", "engine", "title", "capture", "query", "text",
        ] {
            assert!(!text(&s[key]).is_empty(), "{key}");
        }
        for key in ["about", "schema", "after"] {
            assert!(s[key].is_string(), "{key}");
        }
    }
    // One sample per engine at least.
    let formats: HashSet<&str> = list.iter().map(|s| text(&s["format"])).collect();
    assert_eq!(formats.len(), PAGE_FORMATS.len());
    // The sample the page opens on.
    assert!(ids.contains("postgresql"));
    // A title names a version only where the capture shows it.
    for s in SAMPLES {
        let engine = s.title.split(',').next().unwrap_or_default();
        let versioned = engine.chars().any(|c| c.is_ascii_digit());
        let shown = match s.id {
            "sqlserver" => s.text.contains("Build=\"17."),
            "postgresql" => s.text.contains("Index Searches"),
            "postgresql-lookup" => s.after.contains("Index Searches"),
            "trino" => s.text.contains("Trino version: 484"),
            _ => false,
        };
        assert!(!versioned || shown, "{}: {}", s.id, s.title);
    }
}

#[test]
fn every_engine_has_capture_help() {
    let help = capture_help_json();
    let list = help.as_array().unwrap();
    assert_eq!(list.len(), Format::ALL.len());
    for entry in list {
        assert!(text(&entry["hint"]).len() > 20, "{entry}");
    }
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
