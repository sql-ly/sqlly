//! The real captured plans behind the page's sample picker.

use serde_json::{Value, json};

use crate::Format;

/// A real plan, captured for the page's sample picker.
pub struct Sample {
    pub id: &'static str,
    pub format: Format,
    pub title: &'static str,
    /// The command that captured it.
    pub capture: &'static str,
    /// The SQL it explains.
    pub query: &'static str,
    pub text: &'static str,
    /// What to look at in this plan, one sentence.
    pub about: &'static str,
    /// The table definitions the plan ran against, when the sample has them.
    pub schema: &'static str,
    /// A later capture of the same statement, for the comparison view.
    pub after: &'static str,
}

/// Every sample was captured from the engine named, against a small shop
/// schema (`customers`, `orders`, and `order_items` in some) or, for Trino,
/// the TPC-H connector; none is hand-written. Engines with more than one
/// common output shape have a sample of each. A title names a version only
/// where the capture itself shows it: the SQL Server plan's `Build`
/// attribute, Trino's version line, and `Index Searches`, which PostgreSQL
/// prints from version 18.
pub const SAMPLES: &[Sample] = &[
    Sample {
        id: "sqlserver",
        format: Format::SqlServer,
        title: "SQL Server 2025, actual plan with three statements",
        capture: "SET STATISTICS XML ON;",
        query: "DECLARE @email nvarchar(200) = N'user4242@example.com';\nDECLARE @since date = '2026-06-25';\n\nSELECT c.CustomerID, c.Email, o.OrderID, o.OrderDate, o.Status, o.Total\nFROM dbo.Customers AS c\nJOIN dbo.Orders AS o ON o.CustomerID = c.CustomerID\nWHERE c.Email = @email\nORDER BY o.OrderDate DESC;\n\nSELECT o.Status, COUNT(*) AS Orders, SUM(o.Total) AS Revenue\nFROM dbo.Orders AS o\nWHERE o.OrderDate >= @since\nGROUP BY o.Status;\n\nSELECT TOP (20) OrderID, OrderDate, Status, Total\nFROM dbo.Orders\nWHERE OrderDate >= '2026-03-01'\nORDER BY OrderDate;",
        text: include_str!("../samples/sqlserver-actual.xml"),
        about: "A batch of three statements. The second takes 79% of the time: it scans 600,000 rows of Orders to return 4,931, because a variable hid the date from the optimizer.",
        schema: "",
        after: "",
    },
    Sample {
        id: "postgresql",
        format: Format::Postgres,
        title: "PostgreSQL 18, monthly revenue report",
        capture: "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)",
        query: include_str!("../samples/postgresql-report.sql"),
        text: include_str!("../samples/postgresql-report.json"),
        about: "A four-table report that takes 467 ms: a row estimate off by 10,000 times, a nested loop that runs 810,000 times, and a sort that spills to disk.",
        schema: include_str!("../samples/postgresql-schema.sql"),
        after: "",
    },
    Sample {
        id: "postgresql-lookup",
        format: Format::Postgres,
        title: "PostgreSQL 18, lookup before and after two indexes",
        capture: "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)",
        query: include_str!("../samples/postgresql-lookup.sql"),
        text: include_str!("../samples/postgresql-lookup.json"),
        about: "A lookup by lower(email) that scans both tables. The second capture is the same query after creating the two suggested indexes: 22 ms down to 0.1 ms.",
        schema: include_str!("../samples/postgresql-schema.sql"),
        after: include_str!("../samples/postgresql-lookup-indexed.json"),
    },
    Sample {
        id: "mysql",
        format: Format::MySql,
        title: "MySQL, EXPLAIN FORMAT=JSON",
        capture: "EXPLAIN FORMAT=JSON",
        query: "SELECT c.country, COUNT(*) AS orders, SUM(o.total) AS revenue\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded' AND o.total > 1200\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/mysql-explain.json"),
        about: "An estimated plan: costs and row guesses, no times. The scan of orders carries 90% of the estimated cost.",
        schema: "",
        after: "",
    },
    Sample {
        id: "mysql-analyze",
        format: Format::MySql,
        title: "MySQL, EXPLAIN ANALYZE",
        capture: "EXPLAIN ANALYZE",
        query: "SELECT c.country, COUNT(*) AS orders, SUM(o.total) AS revenue\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded' AND o.total > 1200\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/mysql-analyze.txt"),
        about: "A scan of 600,000 orders rows that keeps 5,409, with the real time and row count of every step.",
        schema: "",
        after: "",
    },
    Sample {
        id: "mysql-subquery",
        format: Format::MySql,
        title: "MySQL, EXPLAIN ANALYZE with subqueries",
        capture: "EXPLAIN ANALYZE",
        query: include_str!("../samples/mysql-subquery.sql"),
        text: include_str!("../samples/mysql-subquery-analyze.txt"),
        about: "MySQL prints aliases (c, o) instead of table names. With the query text supplied, the viewer puts the table names back.",
        schema: include_str!("../samples/mysql-schema.sql"),
        after: "",
    },
    Sample {
        id: "mariadb",
        format: Format::MariaDb,
        title: "MariaDB, ANALYZE FORMAT=JSON",
        capture: "ANALYZE FORMAT=JSON",
        query: "SELECT c.country, COUNT(*) AS orders, SUM(o.total) AS revenue\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded' AND o.total > 1200\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/mariadb-analyze.json"),
        about: "ANALYZE runs the query, so the JSON carries real rows and times: a scan of 600,000 orders rows that keeps 5,409.",
        schema: "",
        after: "",
    },
    Sample {
        id: "sqlite",
        format: Format::Sqlite,
        title: "SQLite, EXPLAIN QUERY PLAN",
        capture: "EXPLAIN QUERY PLAN",
        query: "SELECT c.country, count(*) AS orders, sum(o.total) AS revenue\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded' AND o.total > 1200\n  AND c.email NOT IN (SELECT email FROM customers WHERE country = 'US')\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/sqlite.txt"),
        about: "SQLite prints the access path for each table and nothing else: no costs, rows or times.",
        schema: "",
        after: "",
    },
    Sample {
        id: "duckdb",
        format: Format::DuckDb,
        title: "DuckDB, EXPLAIN ANALYZE as JSON",
        capture: "EXPLAIN (ANALYZE, FORMAT JSON)",
        query: "SELECT c.country, count(*) AS orders, sum(o.total) AS revenue\nFROM orders o\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'refunded' AND o.total > 1200\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/duckdb-analyze.json"),
        about: "Real time per operator. Nothing is flagged here; the scan of orders is the slowest step at 3.2 ms.",
        schema: "",
        after: "",
    },
    Sample {
        id: "clickhouse",
        format: Format::ClickHouse,
        title: "ClickHouse, EXPLAIN json = 1",
        capture: "EXPLAIN json = 1, indexes = 1",
        query: "SELECT status, count() AS orders\nFROM orders\nWHERE customer_id = 4242\nGROUP BY status;",
        text: include_str!("../samples/clickhouse.json"),
        about: "ClickHouse lists its processing steps without costs or times, so this shows the shape of the plan.",
        schema: "",
        after: "",
    },
    Sample {
        id: "oracle",
        format: Format::Oracle,
        title: "Oracle, DBMS_XPLAN ALLSTATS LAST",
        capture: "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY_CURSOR(NULL, NULL, 'ALLSTATS LAST'));",
        query: "SELECT /*+ GATHER_PLAN_STATISTICS */ COUNT(*)\nFROM (SELECT o.id, o.padding, o.total,\n             ROW_NUMBER() OVER (ORDER BY o.total DESC, o.padding) rn\n      FROM orders o)\nWHERE rn > 299990;",
        text: include_str!("../samples/oracle-window-allstats.txt"),
        about: "A window sort over 300,000 rows that runs out of work area and uses 33 MB of temporary space.",
        schema: "",
        after: "",
    },
    Sample {
        id: "postgresql-text",
        format: Format::Postgres,
        title: "PostgreSQL, EXPLAIN ANALYZE as psql prints it",
        capture: "EXPLAIN (ANALYZE, BUFFERS)",
        query: "SELECT c.country, count(*) AS orders, sum(oi.quantity * oi.unit_price) AS revenue\nFROM order_items oi\nJOIN orders o ON o.id = oi.order_id\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'paid'\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/postgresql-analyze.txt"),
        about: "The indented text psql prints, read into the same tree as the JSON form. Nothing is flagged in this one.",
        schema: "",
        after: "",
    },
    Sample {
        id: "sqlserver-text",
        format: Format::SqlServer,
        title: "SQL Server, STATISTICS PROFILE with IO and TIME messages",
        capture: "SET STATISTICS PROFILE ON; SET STATISTICS IO ON; SET STATISTICS TIME ON;",
        query: "SELECT o.order_id, o.total, o.status\nFROM dbo.orders o\nWHERE o.customer_id = 4242;",
        text: include_str!("../samples/sqlserver-profile.txt"),
        about: "The text plan, with the STATISTICS IO and TIME messages printed around it.",
        schema: "",
        after: "",
    },
    Sample {
        id: "mysql-table",
        format: Format::MySql,
        title: "MySQL, EXPLAIN table",
        capture: "EXPLAIN",
        query: "SELECT c.country, COUNT(*) AS orders, SUM(oi.quantity * oi.unit_price) AS revenue\nFROM order_items oi\nJOIN orders o ON o.id = oi.order_id\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'paid'\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/mysql-table.txt"),
        about: "The tabular EXPLAIN: one row per table, with row guesses only.",
        schema: "",
        after: "",
    },
    Sample {
        id: "duckdb-text",
        format: Format::DuckDb,
        title: "DuckDB, EXPLAIN ANALYZE box drawing",
        capture: "EXPLAIN ANALYZE",
        query: "SELECT c.country, count(*) AS orders, sum(oi.quantity * oi.unit_price) AS revenue\nFROM order_items oi\nJOIN orders o ON o.id = oi.order_id\nJOIN customers c ON c.id = o.customer_id\nWHERE o.status = 'paid'\nGROUP BY c.country\nORDER BY revenue DESC;",
        text: include_str!("../samples/duckdb-analyze.txt"),
        about: "The box drawing DuckDB prints in its shell, read into the same tree as the JSON form.",
        schema: "",
        after: "",
    },
    Sample {
        id: "clickhouse-text",
        format: Format::ClickHouse,
        title: "ClickHouse, EXPLAIN indexes = 1, actions = 1",
        capture: "EXPLAIN indexes = 1, actions = 1",
        query: "SELECT c.country, count(), sum(o.total)\nFROM customers c\nJOIN orders o ON c.id = o.customer_id\nWHERE o.placed_at >= '2023-06-01 00:00:00' AND o.status = 'paid'\nGROUP BY c.country\nORDER BY sum(o.total) DESC\nLIMIT 5;",
        text: include_str!("../samples/clickhouse-indexes.txt"),
        about: "The indented text form, with the index and expression lines under each step.",
        schema: "",
        after: "",
    },
    Sample {
        id: "cockroachdb",
        format: Format::CockroachDb,
        title: "CockroachDB, EXPLAIN ANALYZE",
        capture: "EXPLAIN ANALYZE",
        query: "SELECT c.region, count(*) AS n, sum(o.total) AS revenue\nFROM customers c\nJOIN orders o ON o.customer_id = c.id\nWHERE c.region IN ('east', 'west')\nGROUP BY c.region\nORDER BY revenue DESC;",
        text: include_str!("../samples/cockroachdb-analyze.txt"),
        about: "A two-table join with real row counts and times.",
        schema: "",
        after: "",
    },
    Sample {
        id: "trino",
        format: Format::Trino,
        title: "Trino 484, EXPLAIN ANALYZE (TPC-H tiny)",
        capture: "EXPLAIN ANALYZE",
        query: "SELECT n.name, count(*) AS n, sum(o.totalprice) AS revenue\nFROM customer c\nJOIN orders o ON o.custkey = c.custkey\nJOIN nation n ON n.nationkey = c.nationkey\nWHERE c.mktsegment = 'BUILDING'\nGROUP BY n.name\nORDER BY revenue DESC;",
        text: include_str!("../samples/trino-analyze.txt"),
        about: "A distributed plan in fragments, with rows and CPU time per operator.",
        schema: "",
        after: "",
    },
    Sample {
        id: "spark",
        format: Format::Spark,
        title: "Spark SQL, EXPLAIN FORMATTED",
        capture: "EXPLAIN FORMATTED",
        query: "WITH big AS (SELECT customer_id, sum(total) t FROM orders GROUP BY customer_id)\nSELECT c.name, big.t, row_number() OVER (ORDER BY big.t DESC) rn\nFROM big\nJOIN customers c ON c.id = big.customer_id\nWHERE big.t > (SELECT avg(total) FROM orders);",
        text: include_str!("../samples/spark-formatted.txt"),
        about: "A physical plan without statistics: the operator tree and what each step does.",
        schema: "",
        after: "",
    },
];

/// The sample list as the page's JSON.
pub fn samples_json() -> Value {
    Value::Array(
        SAMPLES
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "format": s.format.id(),
                    "engine": s.format.engine(),
                    "title": s.title,
                    "capture": s.capture,
                    "query": s.query,
                    "text": s.text,
                    "about": s.about,
                    "schema": s.schema,
                    "after": s.after,
                })
            })
            .collect(),
    )
}
