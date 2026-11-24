use super::*;
use crate::plans::PlanError;

macro_rules! fx {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/plans/",
            $name
        ))
    };
}

const PG_TEXT: &str = "\
Sort  (cost=17.43..17.56 rows=50 width=12) (actual time=0.210..0.214 rows=50 loops=1)
  Sort Key: total DESC
  Sort Method: quicksort  Memory: 27kB
  ->  Seq Scan on orders  (cost=0.00..16.02 rows=50 width=12) (actual time=0.011..0.180 rows=50 loops=1)
        Filter: (total > '100'::numeric)
        Rows Removed by Filter: 950
Planning Time: 0.080 ms
Execution Time: 0.240 ms
";

const PG_JSON: &str = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "orders", "Alias": "o", "Startup Cost": 0.0, "Total Cost": 16.02, "Plan Rows": 50, "Plan Width": 12}}]"#;

const MYSQL_TREE: &str = "\
-> Nested loop inner join  (cost=1.2 rows=3) (actual time=0.1..0.2 rows=3 loops=1)
    -> Table scan on a  (cost=0.5 rows=3) (actual time=0.05..0.06 rows=3 loops=1)
    -> Single-row index lookup on b using PRIMARY (id=a.id)  (cost=0.25 rows=1) (actual time=0.01..0.01 rows=1 loops=3)
";

const SHOWPLAN_XML: &str = r#"<?xml version="1.0" encoding="utf-16"?>
<ShowPlanXML xmlns="http://schemas.microsoft.com/sqlserver/2004/07/showplan" Version="1.5" Build="15.0.2000.5">
  <BatchSequence><Batch><Statements>
    <StmtSimple StatementText="SELECT * FROM dbo.Orders" StatementId="1" StatementSubTreeCost="0.0032831" StatementEstRows="5" StatementType="SELECT">
      <QueryPlan>
        <RelOp NodeId="0" PhysicalOp="Clustered Index Scan" LogicalOp="Clustered Index Scan" EstimateRows="5" EstimateIO="0.003125" EstimateCPU="0.0001581" AvgRowSize="23" EstimatedTotalSubtreeCost="0.0032831" Parallel="0" EstimateRebinds="0" EstimateRewinds="0">
          <OutputList />
          <IndexScan Ordered="0"><Object Database="[Shop]" Schema="[dbo]" Table="[Orders]" Index="[PK_Orders]" /></IndexScan>
        </RelOp>
      </QueryPlan>
    </StmtSimple>
  </Statements></Batch></BatchSequence>
</ShowPlanXML>"#;

fn format_of(doc: &str) -> Option<PlanFormat> {
    detect_plan_format(&clean_plan_text(doc)).map(|found| found.format)
}

fn import(doc: &str) -> ImportedPlan {
    import_plan_document(doc).unwrap_or_else(|error| panic!("{error}\n---\n{doc}"))
}

fn import_error(doc: &str) -> crate::plans::PlanImportError {
    match import_plan_document(doc) {
        Err(PlanError::Import(error)) => *error,
        Ok(imported) => panic!("read as {}: {doc}", imported.format.id()),
        Err(other) => panic!("untyped error {other}: {doc}"),
    }
}

// ---- the older entry points keep their behaviour ----

#[test]
fn sniffs_every_engine() {
    let cases: [(&str, PlanDialect); 12] = [
        (
            "<ShowPlanXML xmlns=\"http://schemas.microsoft.com/sqlserver/2004/07/showplan\">",
            PlanDialect::SqlServer,
        ),
        (
            "[{\"Plan\": {\"Node Type\": \"Result\"}}]",
            PlanDialect::Postgres,
        ),
        (
            "[{\"Plan\": {\"Node Type\": \"Expression\", \"Node Id\": \"Expression_2\"}}]",
            PlanDialect::ClickHouse,
        ),
        (
            "{\"query_block\": {\"select_id\": 1, \"cost_info\": {\"query_cost\": \"1.0\"}}}",
            PlanDialect::MySql,
        ),
        (
            "{\"query_optimization\": {}, \"query_block\": {\"select_id\": 1, \"cost\": 0.5}}",
            PlanDialect::MariaDb,
        ),
        ("-> Nested loop\n    -> Table scan on t", PlanDialect::MySql),
        ("QUERY PLAN\n`--SCAN t", PlanDialect::Sqlite),
        ("2|0|SCAN TABLE t", PlanDialect::Sqlite),
        (
            "{\"operator\": [], \"query\": \"select 1\"}",
            PlanDialect::DuckDb,
        ),
        (
            "| Id  | Operation         | Name |\n|   0 | SELECT STATEMENT  |      |",
            PlanDialect::Oracle,
        ),
        (
            "Seq Scan on t  (cost=0.00..1.50 rows=50 width=4)",
            PlanDialect::Postgres,
        ),
        (
            "  |--Table Scan(OBJECT:([db].[dbo].[t]))",
            PlanDialect::SqlServer,
        ),
    ];
    for (doc, dialect) in cases {
        assert_eq!(
            sniff_plan_document(doc),
            PlanSniff::Dialect(dialect),
            "{doc}"
        );
    }
}

#[test]
fn unreadable_shapes_say_why_and_garbage_is_unknown() {
    assert!(matches!(
        sniff_plan_document(fx!("sqlite/sqlite351-q3-bytecode.txt")),
        PlanSniff::Unsupported(why) if why.contains("EXPLAIN QUERY PLAN")
    ));
    assert!(matches!(
        sniff_plan_document("digraph G {\n  a -> b;\n}"),
        PlanSniff::Unsupported(_)
    ));
    for doc in ["hello world", "{}", "[{}]", "", "   \n\n", "null", "42"] {
        assert_eq!(sniff_plan_document(doc), PlanSniff::Unknown, "{doc}");
        assert!(import_plan_document(doc).is_err(), "{doc}");
    }
}

#[test]
fn clean_unwraps_psql_and_mysql_output() {
    let psql = " QUERY PLAN \n------------\n [         +\n   {\"Plan\": {\"Node Type\": \"Result\"}}+\n ]\n(1 row)\n";
    assert_eq!(
        clean_plan_text(psql),
        "[\n  {\"Plan\": {\"Node Type\": \"Result\"}}\n]"
    );
    let mysql = "+---------+\n| EXPLAIN |\n+---------+\n| -> Table scan on t  (cost=1 rows=2)\n |\n+---------+\n1 row in set (0.00 sec)";
    assert_eq!(
        clean_plan_text(mysql),
        "-> Table scan on t  (cost=1 rows=2)"
    );
    let banner = "*************************** 1. row ***************************\nEXPLAIN: {\"query_block\": {}}";
    assert_eq!(clean_plan_text(banner), "{\"query_block\": {}}");
}

#[test]
fn a_lone_postgres_plan_object_parses() {
    let plans = parse_plan_document(
        PlanDialect::Postgres,
        "{\"Plan\": {\"Node Type\": \"Seq Scan\", \"Relation Name\": \"t\", \"Total Cost\": 1.0, \"Plan Rows\": 5}}",
    )
    .unwrap();
    assert_eq!(plans[0].nodes[0].physical_op, "Seq Scan");
    assert_eq!(plans[0].dialect, Some(PlanDialect::Postgres));
    assert_eq!(plans[0].source_format.as_deref(), Some("postgresql-json"));
    assert!(!plans[0].has_actuals);
}

#[test]
fn every_plan_carries_its_dialect_format_and_whether_it_ran() {
    let estimated = parse_plan_document(PlanDialect::Postgres, PG_JSON).unwrap();
    assert!(!estimated[0].has_actuals);
    let ran = parse_plan_document(PlanDialect::Postgres, PG_TEXT).unwrap();
    assert!(ran[0].has_actuals);
    assert_eq!(ran[0].dialect, Some(PlanDialect::Postgres));
    assert_eq!(ran[0].source_format.as_deref(), Some("postgresql-text"));
    // MySQL and MariaDB are one family: asking for either reads both.
    let tree = parse_plan_document(PlanDialect::MariaDb, MYSQL_TREE).unwrap();
    assert!(tree[0].has_actuals);
    assert!(tree[0].dialect.is_some());
    let xml = parse_plan_document(PlanDialect::SqlServer, SHOWPLAN_XML).unwrap();
    assert_eq!(xml[0].dialect, Some(PlanDialect::SqlServer));
    assert_eq!(xml[0].source_format.as_deref(), Some("sqlserver-xml"));
    // A plan that is certainly another engine's is read as what it is.
    let other = parse_plan_document(PlanDialect::Oracle, PG_TEXT).unwrap();
    assert_eq!(other[0].dialect, Some(PlanDialect::Postgres));
    assert_eq!(other[0].source_format.as_deref(), Some("postgresql-text"));
}

// ---- ids ----

#[test]
fn format_and_engine_ids_round_trip() {
    let mut seen = std::collections::HashSet::new();
    for format in PlanFormat::ALL {
        assert!(seen.insert(format.id()), "duplicate id {}", format.id());
        assert_eq!(PlanFormat::from_id(format.id()), Some(format));
        assert!(!format.label().is_empty());
        assert_eq!(format.dialect(), format.engine().dialect());
    }
    for engine in PlanEngine::ALL {
        assert_eq!(PlanEngine::from_id(engine.id()), Some(engine));
        assert!(!engine.name().is_empty());
        assert!(engine.capture_hint().len() > 20, "{}", engine.id());
        if let Some(dialect) = engine.dialect() {
            assert_eq!(PlanEngine::from_dialect(dialect), engine);
        }
    }
    assert_eq!(PlanFormat::from_id("nope"), None);
    assert_eq!(PlanEngine::from_id(""), None);
}

// ---- detection ----

#[test]
fn each_format_is_detected_from_a_real_capture() {
    let cases: &[(&str, PlanFormat)] = &[
        (SHOWPLAN_XML, PlanFormat::SqlServerXml),
        (
            fx!("sqlserver/q7-showplan-text.txt"),
            PlanFormat::SqlServerText,
        ),
        (
            fx!("sqlserver/q7-showplan-all.txt"),
            PlanFormat::SqlServerText,
        ),
        (
            fx!("sqlserver/q4-profile-parallel.tsv"),
            PlanFormat::SqlServerText,
        ),
        (fx!("postgresql/join-analyze.txt"), PlanFormat::PostgresText),
        (
            fx!("postgresql/join-analyze.json"),
            PlanFormat::PostgresJson,
        ),
        (
            fx!("postgresql/join-analyze.yaml"),
            PlanFormat::PostgresYaml,
        ),
        (fx!("postgresql/join-analyze.xml"), PlanFormat::PostgresXml),
        (
            fx!("mysql/mysql84-q1-join-table.txt"),
            PlanFormat::MySqlTable,
        ),
        (
            fx!("mysql/mysql84-q1-join-vertical.txt"),
            PlanFormat::MySqlTable,
        ),
        (fx!("mysql/mysql84-q1-join-tsv.txt"), PlanFormat::MySqlTable),
        (fx!("mysql/mysql84-q1-join-tree.txt"), PlanFormat::MySqlTree),
        (
            fx!("mysql/mysql84-q1-join-analyze-tree.txt"),
            PlanFormat::MySqlTree,
        ),
        (fx!("mysql/mysql84-q1-join-v1.json"), PlanFormat::MySqlJson),
        (
            fx!("mysql/mysql84-q1-join-v2.json"),
            PlanFormat::MySqlJsonV2,
        ),
        (
            fx!("mariadb/mariadb-q1-join-analyze-table.txt"),
            PlanFormat::MariaDbTable,
        ),
        (
            fx!("mariadb/mariadb-q1-join-analyze.json"),
            PlanFormat::MariaDbJson,
        ),
        (
            fx!("mariadb/mariadb-q1-join-explain.json"),
            PlanFormat::MariaDbJson,
        ),
        (
            fx!("sqlite/sqlite351-q1-tree.txt"),
            PlanFormat::SqliteQueryPlan,
        ),
        (
            fx!("sqlite/sqlite351-q2-pipe.txt"),
            PlanFormat::SqliteQueryPlan,
        ),
        (
            fx!("sqlite/sqlite351-q2-rows-table.txt"),
            PlanFormat::SqliteQueryPlan,
        ),
        (
            fx!("duckdb/duckdb142-join-explain.txt"),
            PlanFormat::DuckDbText,
        ),
        (
            fx!("duckdb/duckdb142-join-analyze.txt"),
            PlanFormat::DuckDbText,
        ),
        (
            fx!("duckdb/duckdb142-join-analyze.json"),
            PlanFormat::DuckDbJson,
        ),
        (
            fx!("duckdb/duckdb142-join-explain.json"),
            PlanFormat::DuckDbJson,
        ),
        (
            fx!("clickhouse/clickhouse269-join-plan.txt"),
            PlanFormat::ClickHouseText,
        ),
        (
            fx!("clickhouse/clickhouse269-range-plan-pretty.txt"),
            PlanFormat::ClickHouseText,
        ),
        (
            fx!("clickhouse/clickhouse269-join-pipeline.txt"),
            PlanFormat::ClickHousePipeline,
        ),
        (
            fx!("clickhouse/clickhouse269-join-indexes.json"),
            PlanFormat::ClickHouseJson,
        ),
        (
            fx!("clickhouse/clickhouse269-range-json-escaped.tsv"),
            PlanFormat::ClickHouseJson,
        ),
        (
            fx!("cockroachdb/crdb243-q2-explain-table.txt"),
            PlanFormat::CockroachText,
        ),
        (
            fx!("cockroachdb/crdb243-q2-analyze-tsv.txt"),
            PlanFormat::CockroachText,
        ),
        (
            fx!("trino/trino484-q2-explain-aligned.txt"),
            PlanFormat::TrinoText,
        ),
        (
            fx!("trino/trino484-q2-analyze-aligned.txt"),
            PlanFormat::TrinoText,
        ),
        (fx!("spark/spark43-q2-simple.txt"), PlanFormat::SparkText),
        (fx!("spark/spark43-q5-formatted.txt"), PlanFormat::SparkText),
    ];
    for (doc, expected) in cases {
        let found = detect_plan_format(&clean_plan_text(doc))
            .unwrap_or_else(|| panic!("nothing detected for {}", expected.id()));
        assert_eq!(found.format, *expected, "{}", found.evidence);
        assert_eq!(
            found.confidence,
            PlanConfidence::Certain,
            "{}: {}",
            expected.id(),
            found.evidence
        );
        let imported = import(doc);
        assert_eq!(imported.format, *expected);
        assert!(!imported.plans.is_empty());
        assert!(imported.plans.iter().any(|plan| !plan.nodes.is_empty()));
    }
}

#[test]
fn look_alikes_are_told_apart() {
    // PostgreSQL text and MySQL's tree both use `->` and `(cost=`.
    assert_eq!(format_of(PG_TEXT), Some(PlanFormat::PostgresText));
    assert_eq!(format_of(MYSQL_TREE), Some(PlanFormat::MySqlTree));
    // PostgreSQL and ClickHouse JSON both have a top-level "Plan".
    assert_eq!(format_of(PG_JSON), Some(PlanFormat::PostgresJson));
    assert_eq!(
        format_of(
            r#"[{"Plan": {"Node Type": "Expression", "Plans": [{"Node Type": "ReadFromMergeTree"}]}}]"#
        ),
        Some(PlanFormat::ClickHouseJson)
    );
    // MySQL and MariaDB JSON both have "query_block".
    assert_eq!(
        format_of(
            r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "1.0"}, "table": {"table_name": "t"}}}"#
        ),
        Some(PlanFormat::MySqlJson)
    );
    assert_eq!(
        format_of(
            r#"{"query_block": {"select_id": 1, "r_loops": 1, "table": {"table_name": "t", "r_rows": 3}}}"#
        ),
        Some(PlanFormat::MariaDbJson)
    );
    // MySQL's and MariaDB's tabular EXPLAIN differ by the r_ columns.
    assert_eq!(
        format_of(fx!("mysql/mysql84-q1-join-table.txt")),
        Some(PlanFormat::MySqlTable)
    );
    assert_eq!(
        format_of(fx!("mariadb/mariadb-q1-join-analyze-table.txt")),
        Some(PlanFormat::MariaDbTable)
    );
    // An Oracle plan table and a MySQL result box are both `|` tables.
    let oracle = "\
Plan hash value: 2137789089

---------------------------------------------------------------------------
| Id  | Operation         | Name   | Rows  | Bytes | Cost (%CPU)| Time     |
---------------------------------------------------------------------------
|   0 | SELECT STATEMENT  |        |    14 |   532 |     3   (0)| 00:00:01 |
|*  1 |  TABLE ACCESS FULL| EMP    |    14 |   532 |     3   (0)| 00:00:01 |
---------------------------------------------------------------------------

Predicate Information (identified by operation id):
---------------------------------------------------

   1 - filter(\"SAL\">1000)
";
    assert_eq!(format_of(oracle), Some(PlanFormat::OracleXplan));
    // SQLite's tree and SQL Server's SHOWPLAN_TEXT both draw `|--`.
    assert_eq!(
        format_of("QUERY PLAN\n|--SCAN o\n`--SEARCH c USING INTEGER PRIMARY KEY (rowid=?)"),
        Some(PlanFormat::SqliteQueryPlan)
    );
    assert_eq!(
        format_of(
            "  |--Nested Loops(Inner Join, OUTER REFERENCES:([o].[CustomerId]))\n       |--Clustered Index Scan(OBJECT:([Shop].[dbo].[Orders].[PK_Orders] AS [o]))\n       |--Clustered Index Seek(OBJECT:([Shop].[dbo].[Customers].[PK_Customers] AS [c]), SEEK:([c].[Id]=[o].[CustomerId]) ORDERED FORWARD)"
        ),
        Some(PlanFormat::SqlServerText)
    );
    // DuckDB and Trino/Cockroach text are trees of a different drawing.
    assert_eq!(
        format_of(fx!("duckdb/duckdb142-range-explain.txt")),
        Some(PlanFormat::DuckDbText)
    );
    assert_eq!(
        format_of(fx!("cockroachdb/crdb243-q2-explain-raw.txt")),
        Some(PlanFormat::CockroachText)
    );
}

#[test]
fn prose_sql_and_data_are_not_plans() {
    for doc in [
        "SELECT * FROM orders WHERE total > 5;",
        "The query was slow -> we added an index (cost was high).",
        "id | name\n---+-----\n 1 | a\n 2 | b\n(2 rows)",
        "{\"name\": \"plan\", \"rows\": 3}",
        "[1, 2, 3]",
        "<html><body><p>Seq Scan</p></body></html>",
        "<root><child/></root>",
        "| a | b |\n|---|---|\n| 1 | 2 |",
        "- item one\n- item two\n  - nested",
        "1\n2\n3\n4",
        "ERROR:  relation \"orders\" does not exist\nLINE 1: explain select * from orders",
        "key: value\nother: thing\n",
    ] {
        assert!(
            import_plan_document(doc).is_err(),
            "read as {:?}: {doc}",
            import_plan_document(doc).map(|imported| imported.format.id())
        );
    }
}

// ---- cleanup ----

#[test]
fn bytes_decode_from_the_encodings_plan_files_use() {
    let utf8 = SHOWPLAN_XML.as_bytes().to_vec();
    assert_eq!(decode_plan_bytes(&utf8), SHOWPLAN_XML);
    let mut with_bom = vec![0xEF, 0xBB, 0xBF];
    with_bom.extend(&utf8);
    assert_eq!(decode_plan_bytes(&with_bom), SHOWPLAN_XML);
    let le: Vec<u8> = SHOWPLAN_XML
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let be: Vec<u8> = SHOWPLAN_XML
        .encode_utf16()
        .flat_map(u16::to_be_bytes)
        .collect();
    // SSMS saves .sqlplan as UTF-16 LE with a BOM.
    let le_bom: Vec<u8> = [0xFF, 0xFE].into_iter().chain(le.iter().copied()).collect();
    let be_bom: Vec<u8> = [0xFE, 0xFF].into_iter().chain(be.iter().copied()).collect();
    for bytes in [&le_bom, &be_bom, &le, &be] {
        let text = decode_plan_bytes(bytes);
        assert_eq!(text, SHOWPLAN_XML);
        assert_eq!(import(&text).format, PlanFormat::SqlServerXml);
    }
    // Odd lengths and invalid bytes do not panic.
    for bytes in [
        &[][..],
        &[0xFF],
        &[0xFF, 0xFE],
        &[0xFF, 0xFE, 0x41],
        &[0xC3, 0x28, 0xFF],
    ] {
        let _ = decode_plan_bytes(bytes);
    }
}

fn same_plan(wrapped: &str, plain: &str, what: &str) {
    let expected = import(plain);
    let got = import_plan_document(wrapped)
        .unwrap_or_else(|error| panic!("{what}: {error}\n---\n{wrapped}"));
    assert_eq!(got.format, expected.format, "{what}");
    assert_eq!(got.plans.len(), expected.plans.len(), "{what}");
    for (a, b) in got.plans.iter().zip(&expected.plans) {
        assert_eq!(a.nodes.len(), b.nodes.len(), "{what}");
        for (left, right) in a.nodes.iter().zip(&b.nodes) {
            assert_eq!(left.physical_op, right.physical_op, "{what}");
            assert_eq!(left.estimated_rows, right.estimated_rows, "{what}");
            assert_eq!(left.actual_rows, right.actual_rows, "{what}");
            assert_eq!(left.filter, right.filter, "{what}");
        }
    }
}

#[test]
fn line_endings_boms_and_code_fences_are_removed() {
    same_plan(&PG_TEXT.replace('\n', "\r\n"), PG_TEXT, "CRLF");
    same_plan(&format!("\u{feff}{PG_TEXT}"), PG_TEXT, "BOM");
    same_plan(&format!("```\n{PG_TEXT}```\n"), PG_TEXT, "fence");
    same_plan(&format!("```sql\n{PG_TEXT}```"), PG_TEXT, "sql fence");
    same_plan(
        &format!("Here is the plan from prod:\n\n```text\n{PG_TEXT}```\n\nAny idea?"),
        PG_TEXT,
        "fence inside prose",
    );
    same_plan(&format!("~~~\n{PG_JSON}\n~~~"), PG_JSON, "tilde fence");
    same_plan(
        &PG_TEXT.replace(' ', "\u{a0}"),
        PG_TEXT,
        "non-breaking spaces",
    );
    same_plan(
        &format!("```xml\r\n{}\r\n```", SHOWPLAN_XML.replace('\n', "\r\n")),
        SHOWPLAN_XML,
        "xml fence with CRLF",
    );
}

#[test]
fn prompts_and_the_statement_itself_are_removed() {
    let psql = format!(
        "shop=# EXPLAIN (ANALYZE, BUFFERS)\nshop-# SELECT * FROM orders WHERE total > 100\nshop-# ORDER BY total DESC;\n                    QUERY PLAN\n--------------------------------------------\n{}(8 rows)\n\nshop=# ",
        PG_TEXT
            .lines()
            .map(|line| format!(" {line}\n"))
            .collect::<String>()
    );
    same_plan(&psql, PG_TEXT, "psql session");
    same_plan(
        &format!("postgres=> explain analyze select 1;\n{PG_TEXT}"),
        PG_TEXT,
        "psql non-superuser prompt",
    );
    same_plan(
        &format!(
            "EXPLAIN ANALYZE SELECT * FROM orders WHERE total > 100 ORDER BY total DESC;\n{PG_TEXT}"
        ),
        PG_TEXT,
        "statement above the plan",
    );
    same_plan(
        &format!("mysql> EXPLAIN FORMAT=TREE SELECT * FROM a JOIN b USING (id);\n{MYSQL_TREE}"),
        MYSQL_TREE,
        "mysql prompt",
    );
    same_plan(
        &format!("MariaDB [shop]> EXPLAIN FORMAT=TREE SELECT 1;\n{MYSQL_TREE}"),
        MYSQL_TREE,
        "mariadb prompt",
    );
    // A `-> Select #2` style line is plan content, not a prompt.
    let tree = fx!("mysql/mysql84-q3-subqueries-analyze-tree.txt");
    assert_eq!(import(tree).format, PlanFormat::MySqlTree);
    let sqlite = "sqlite> EXPLAIN QUERY PLAN SELECT * FROM o JOIN c ON c.id = o.cid;\nQUERY PLAN\n|--SCAN o\n`--SEARCH c USING INTEGER PRIMARY KEY (rowid=?)\nsqlite> ";
    let plan = import(sqlite);
    assert_eq!(plan.format, PlanFormat::SqliteQueryPlan);
    assert_eq!(
        plan.plans[0]
            .nodes
            .iter()
            .filter(|n| n.relation.is_some())
            .count(),
        2
    );
}

#[test]
fn line_numbers_log_prefixes_and_quotes_are_removed() {
    let numbered: String = PG_TEXT
        .lines()
        .enumerate()
        .map(|(index, line)| format!("{:>2}  {line}\n", index + 1))
        .collect();
    same_plan(&numbered, PG_TEXT, "line numbers");
    let piped: String = PG_TEXT
        .lines()
        .enumerate()
        .map(|(index, line)| format!("{} | {line}\n", index + 1))
        .collect();
    same_plan(&piped, PG_TEXT, "line numbers with a bar");

    // auto_explain in the server log, with a log_line_prefix.
    let logged = "2026-11-20 10:15:02.123 UTC [4242] app@shop LOG:  duration: 0.240 ms  plan:\n\tQuery Text: SELECT * FROM orders WHERE total > 100 ORDER BY total DESC;\n".to_string()
        + &PG_TEXT
            .lines()
            .map(|line| format!("\t{line}\n"))
            .collect::<String>();
    let imported = import(&logged);
    assert_eq!(imported.format, PlanFormat::PostgresText);
    assert_eq!(imported.plans[0].nodes.len(), 2);
    assert!(
        imported.plans[0]
            .statement_text
            .starts_with("SELECT * FROM orders")
    );

    // pgAdmin copies every row as a quoted cell.
    let quoted = "\"QUERY PLAN\"\n".to_string()
        + &PG_TEXT
            .lines()
            .map(|line| format!("\"{}\"\n", line.replace('"', "\"\"")))
            .collect::<String>();
    same_plan(&quoted, PG_TEXT, "pgAdmin quotes");
}

#[test]
fn escaped_and_embedded_documents_are_unwrapped() {
    // HTML-escaped showplan, as SSMS grid cells and web pages give it.
    let escaped = SHOWPLAN_XML
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    same_plan(&escaped, SHOWPLAN_XML, "HTML-escaped XML");
    // A text plan from a web page: `-&gt;` arrows.
    same_plan(
        &PG_TEXT.replace('>', "&gt;").replace('\'', "&#39;"),
        PG_TEXT,
        "HTML-escaped text",
    );
    // An entity inside real XML is content and stays.
    let with_entity = SHOWPLAN_XML.replace(
        "SELECT * FROM dbo.Orders",
        "SELECT * FROM dbo.Orders WHERE a &lt; 5",
    );
    let plan = import(&with_entity);
    assert!(plan.plans[0].statement_text.contains("a < 5"));

    // JSON inside a string literal, from a log line or an API response.
    let literal = serde_json::to_string(PG_JSON).unwrap();
    same_plan(&literal, PG_JSON, "JSON string literal");
    same_plan(
        &PG_JSON.replace('"', "\\\""),
        PG_JSON,
        "escaped quotes without the outer quotes",
    );
    same_plan(
        &serde_json::to_string(PG_TEXT).unwrap(),
        PG_TEXT,
        "text plan in a string literal",
    );
    // A driver's result set: one row, one column.
    same_plan(
        &format!("[{{\"QUERY PLAN\": {PG_JSON}}}]"),
        PG_JSON,
        "result set as JSON",
    );
    let mysql_json = r#"{"query_block": {"select_id": 1, "cost_info": {"query_cost": "1.0"}, "table": {"table_name": "t", "access_type": "ALL", "rows_examined_per_scan": 3}}}"#;
    same_plan(
        &format!(
            "[{{\"EXPLAIN\": {}}}]",
            serde_json::to_string(mysql_json).unwrap()
        ),
        mysql_json,
        "MySQL result set with the plan as a string",
    );
    // Text around a JSON document.
    same_plan(
        &format!("QUERY PLAN\n----------\n{PG_JSON}\n(1 row)"),
        PG_JSON,
        "psql frame around JSON",
    );
}

#[test]
fn client_table_frames_are_unwrapped() {
    for (framed, plain) in [
        (
            fx!("mysql/mysql84-q2-range-analyze-tree-boxed.txt"),
            fx!("mysql/mysql84-q2-range-analyze-tree-batch.txt"),
        ),
        (
            fx!("mysql/mysql84-q2-range-analyze-tree-vertical.txt"),
            fx!("mysql/mysql84-q2-range-analyze-tree-batch.txt"),
        ),
        (
            fx!("clickhouse/clickhouse269-range-plan-pretty.txt"),
            fx!("clickhouse/clickhouse269-range-plan-header.txt"),
        ),
        (
            fx!("clickhouse/clickhouse269-range-plan-vertical.txt"),
            fx!("clickhouse/clickhouse269-range-plan-header.txt"),
        ),
    ] {
        let (a, b) = (import(framed), import(plain));
        assert_eq!(a.format, b.format);
        assert_eq!(a.plans[0].nodes.len(), b.plans[0].nodes.len());
        for (left, right) in a.plans[0].nodes.iter().zip(&b.plans[0].nodes) {
            assert_eq!(left.physical_op, right.physical_op);
        }
    }
}

#[test]
fn cleaning_is_idempotent_and_leaves_clean_plans_alone() {
    for doc in [PG_TEXT, PG_JSON, MYSQL_TREE, SHOWPLAN_XML] {
        let once = clean_plan_text(doc);
        assert_eq!(once, doc.trim(), "clean input changed");
        assert_eq!(clean_plan_text(&once), once);
    }
    for doc in [
        fx!("postgresql/join-analyze.txt"),
        fx!("mysql/mysql84-q1-join-table.txt"),
        fx!("sqlite/sqlite351-q2-rows-table.txt"),
        fx!("duckdb/duckdb142-join-analyze.txt"),
        fx!("clickhouse/clickhouse269-range-plan-pretty.txt"),
        fx!("cockroachdb/crdb243-q2-explain-table.txt"),
        fx!("trino/trino484-q2-explain-aligned.txt"),
        fx!("spark/spark43-q5-formatted.txt"),
        fx!("sqlserver/q7-showplan-all.txt"),
    ] {
        let once = clean_plan_text(doc);
        assert_eq!(clean_plan_text(&once), once);
    }
}

// ---- errors ----

#[test]
fn errors_name_the_format_the_place_and_how_to_capture_a_plan() {
    // Truncated JSON: we know it is PostgreSQL, and where it stops.
    let cut = &fx!("postgresql/join-analyze.json")[..2000];
    let error = import_error(cut);
    assert_eq!(error.format_id, Some("postgresql-json"));
    assert!(error.line.is_some(), "{error}");
    let text = error.to_string();
    assert!(text.starts_with("This looks like PostgreSQL"), "{text}");
    assert!(text.contains("line "), "{text}");
    assert!(text.contains("EXPLAIN"), "{text}");

    // Broken XML.
    let broken = SHOWPLAN_XML.replace("</QueryPlan>", "");
    let error = import_error(&broken);
    assert_eq!(error.format_id, Some("sqlserver-xml"));
    assert!(error.line.is_some(), "{error}");
    assert!(
        error
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("SHOWPLAN_XML")
            || error
                .hint
                .as_deref()
                .unwrap_or_default()
                .contains(".sqlplan")
    );

    // Nothing recognisable: the message lists what can be read.
    let error = import_error("hello world");
    assert_eq!(error.format_id, None);
    let text = error.to_string();
    for engine in [
        "PostgreSQL",
        "SQL Server",
        "MySQL",
        "Oracle",
        "SQLite",
        "DuckDB",
        "ClickHouse",
    ] {
        assert!(text.contains(engine), "{engine} missing from: {text}");
    }
    assert!(matches!(
        import_plan_document(""),
        Err(PlanError::EmptyPlan)
    ));
    assert!(matches!(
        import_plan_document("```\n```"),
        Err(PlanError::EmptyPlan)
    ));

    // A bare statement: say that a plan is needed.
    let error = import_error("SELECT * FROM orders WHERE total > 5");
    assert!(error.to_string().contains("EXPLAIN"), "{error}");

    // Shapes of a known engine that are not plans.
    let error = import_error(fx!("sqlite/sqlite351-q3-bytecode.txt"));
    assert!(
        error
            .format_label
            .as_deref()
            .unwrap_or_default()
            .contains("SQLite")
    );
    assert!(error.to_string().contains("EXPLAIN QUERY PLAN"), "{error}");

    // Nothing readable under a picked engine: the hint is that engine's.
    let Err(PlanError::Import(error)) = import_plan_document_as(PlanEngine::Oracle, "hello world")
    else {
        panic!("prose read as an Oracle plan");
    };
    assert!(error.to_string().contains("DBMS_XPLAN"), "{error}");
    // A plan that is certainly another engine's is read as that engine.
    let other = import_plan_document_as(PlanEngine::Oracle, PG_TEXT).unwrap();
    assert_eq!(other.format, PlanFormat::PostgresText);
}

#[test]
fn picking_an_engine_reads_what_detection_alone_is_unsure_of() {
    // A COSTS OFF plan has nothing but operator names.
    let costs_off = "Limit\n  ->  Sort\n        Sort Key: total DESC\n        ->  Seq Scan on orders\n              Filter: (total > '100'::numeric)\n";
    let picked = import_plan_document_as(PlanEngine::Postgres, costs_off).unwrap();
    assert_eq!(picked.format, PlanFormat::PostgresText);
    assert_eq!(picked.plans[0].nodes.len(), 3);
    // Picking the same engine never changes a certain detection.
    for (engine, doc) in [
        (PlanEngine::Postgres, PG_TEXT),
        (PlanEngine::Postgres, PG_JSON),
        (PlanEngine::MySql, MYSQL_TREE),
        (PlanEngine::MariaDb, MYSQL_TREE),
        (PlanEngine::SqlServer, SHOWPLAN_XML),
    ] {
        let picked = import_plan_document_as(engine, doc).unwrap();
        assert_eq!(picked.format, import(doc).format);
    }
}

#[test]
fn sql_server_messages_beside_a_plan_become_plan_statistics() {
    let imported = import(fx!("sqlserver/q2-io-time-profile.txt"));
    assert_eq!(imported.format, PlanFormat::SqlServerText);
    let plan = imported
        .plans
        .iter()
        .rev()
        .find(|plan| !plan.nodes.is_empty())
        .unwrap();
    assert!(!plan.table_io.is_empty());
    assert!(plan.table_io.iter().any(|io| io.logical_reads.is_some()));
    assert!(plan.cpu_time_ms.is_some() || plan.execution_time_ms.is_some());

    // The same messages pasted under a graphical plan's XML.
    let with_messages = format!(
        "{SHOWPLAN_XML}\n\nTable 'Orders'. Scan count 1, logical reads 3, physical reads 0, page server reads 0, read-ahead reads 0, page server read-ahead reads 0, lob logical reads 0, lob physical reads 0, lob page server reads 0, lob read-ahead reads 0, lob page server read-ahead reads 0.\n\n SQL Server Execution Times:\n   CPU time = 15 ms,  elapsed time = 21 ms.\n"
    );
    let imported = import(&with_messages);
    assert_eq!(imported.format, PlanFormat::SqlServerXml);
    let plan = &imported.plans[0];
    assert_eq!(plan.table_io.len(), 1);
    assert_eq!(plan.table_io[0].table, "Orders");
    assert_eq!(plan.table_io[0].logical_reads, Some(3.0));
    assert_eq!(plan.cpu_time_ms, Some(15.0));
    assert_eq!(plan.execution_time_ms, Some(21.0));

    // Messages with no plan: say what is missing.
    let error = import_error(
        "Table 'Orders'. Scan count 1, logical reads 3, physical reads 0, read-ahead reads 0, lob logical reads 0, lob physical reads 0, lob read-ahead reads 0.\n",
    );
    assert!(error.to_string().contains("SQL Server"), "{error}");
}

// ---- robustness ----

#[test]
fn arbitrary_input_never_panics() {
    let seeds = [
        PG_TEXT,
        PG_JSON,
        MYSQL_TREE,
        SHOWPLAN_XML,
        "{\"query_block\": {\"select_id\": 1}}",
        "QUERY PLAN\n|--SCAN o\n`--SEARCH c USING INTEGER PRIMARY KEY (rowid=?)",
        "| Id  | Operation         | Name |\n|   0 | SELECT STATEMENT  |      |\n|*  1 |  TABLE ACCESS FULL| EMP  |",
        "```\n```",
        "\"\\\"\\\\\"",
        "&lt;&gt;&amp;&quot;&#39;&#x;&#99999999999;",
        "=# \n=> \n> \n",
        "1 \n2 \n3 \n",
        "┌─explain─┐\n│ │\n└─────────┘",
        "Row 1:\n──────\nexplain: ",
        "+--+\n|  |\n+--+",
        "*************************** 1. row ***************************",
        "\u{feff}\u{0}\u{0}",
        "((((((((((((((((((((((((((((((((",
        "[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[[",
        "{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{{",
        "<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<<",
        "->->->->->->->->->->->->->->->->",
        "|--|--|--|--|--|--|--|--|--|--",
    ];
    for seed in seeds {
        crate::plans::dialects::common::assert_survives_mangling(seed, |doc| {
            let cleaned = clean_plan_text(doc);
            let _ = detect_plan_format(&cleaned);
            let _ = import_plan_document(doc);
            let _ = sniff_plan_document(doc);
            for engine in PlanEngine::ALL {
                let _ = import_plan_document_as(engine, doc);
            }
        });
    }
    // Deep nesting does not overflow the stack.
    let deep_json = format!("{}{}", "[".repeat(50_000), "]".repeat(50_000));
    let _ = import_plan_document(&deep_json);
    let deep_plan = format!(
        "[{{\"Plan\": {}{{\"Node Type\": \"Result\"}}{}}}]",
        "{\"Node Type\": \"Append\", \"Plans\": [".repeat(3_000),
        "]}".repeat(3_000)
    );
    let _ = import_plan_document(&deep_plan);
    let deep_text: String = (0..5_000)
        .map(|depth| {
            format!(
                "{}->  Seq Scan on t{depth}  (cost=0.00..1.00 rows=1 width=4)\n",
                "      ".repeat(depth.min(400))
            )
        })
        .collect();
    let _ = import_plan_document(&deep_text);
    let deep_xml = format!("{}{}", "<a>".repeat(20_000), "</a>".repeat(20_000));
    let _ = import_plan_document(&deep_xml);
    // A megabyte of one character takes no noticeable time.
    for ch in ['a', ' ', '|', '-', '(', '\n', '"', '1'] {
        let started = std::time::Instant::now();
        let _ = import_plan_document(&ch.to_string().repeat(1_000_000));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(20),
            "a megabyte of {ch:?} took {:?}",
            started.elapsed()
        );
    }
}

#[test]
fn a_plan_nested_too_deeply_says_so() {
    let mut doc = String::from("QUERY PLAN\n");
    for depth in 0..520 {
        doc.push_str(&"   ".repeat(depth));
        doc.push_str("`--SCAN t\n");
    }
    let error = import_error(&doc);
    assert!(error.detail.contains("deep"), "{}", error.detail);
}
