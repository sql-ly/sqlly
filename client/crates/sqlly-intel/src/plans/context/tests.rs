use super::*;
use crate::plans::findings::{findings_for_plan, findings_for_plan_with_context};
use crate::plans::import::parse_plan_document;
use crate::plans::story::batch_story_with_context;

const Q1: &str = "SELECT c.name, SUM(oi.quantity * oi.unit_price) AS revenue \
     FROM orders o \
     JOIN customers c ON c.id = o.customer_id \
     JOIN order_items AS oi ON oi.order_id = o.id \
     WHERE o.status = 'paid' GROUP BY c.name ORDER BY revenue DESC";

fn fixture(engine: &str, file: &str, dialect: PlanDialect) -> ExecutionPlan {
    let path = format!(
        "{}/tests/golden/plans/{engine}/{file}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    parse_plan_document(dialect, &text)
        .unwrap_or_else(|error| panic!("{path}: {error:?}"))
        .remove(0)
}

fn names(plan: &ExecutionPlan) -> Vec<(String, Option<String>)> {
    analyze_plan(plan)
        .nodes
        .iter()
        .filter_map(|node| node.object.as_ref())
        .filter(|object| object.kind == PlanObjectKind::Table)
        .map(|object| (object.name.clone(), object.alias.clone()))
        .collect()
}

#[test]
fn statement_sources_read_aliases_and_plain_tables() {
    let sources = StatementSources::read(Q1, PlanDialect::MySql);
    assert_eq!(
        sources.resolve("o").map(|t| t.name).as_deref(),
        Some("orders")
    );
    assert_eq!(
        sources.resolve("C").map(|t| t.name).as_deref(),
        Some("customers")
    );
    assert_eq!(
        sources.resolve("oi").map(|t| t.name).as_deref(),
        Some("order_items")
    );
    assert_eq!(sources.resolve("x"), None);

    let plain = StatementSources::read(
        "select * from shop.orders where total > 5",
        PlanDialect::MySql,
    );
    let orders = plain.resolve("orders").expect("plain table");
    assert_eq!(orders.name, "orders");
    assert_eq!(orders.schema.as_deref(), Some("shop"));
}

#[test]
fn an_alias_used_for_two_tables_is_not_resolved() {
    let sources = StatementSources::read(
        "SELECT * FROM orders t WHERE EXISTS (SELECT 1 FROM customers t WHERE t.id = 1)",
        PlanDialect::MySql,
    );
    assert_eq!(sources.resolve("t"), None);
}

#[test]
fn a_cte_name_is_not_a_table() {
    let sources = StatementSources::read(
        "WITH recent AS (SELECT * FROM orders o) SELECT * FROM recent r JOIN customers c ON c.id = r.customer_id",
        PlanDialect::Sqlite,
    );
    assert_eq!(sources.resolve("r"), None);
    assert_eq!(sources.resolve("recent"), None);
    assert_eq!(
        sources.resolve("o").map(|t| t.name).as_deref(),
        Some("orders")
    );
    assert_eq!(
        sources.resolve("c").map(|t| t.name).as_deref(),
        Some("customers")
    );
}

#[test]
fn the_capture_command_is_stripped() {
    assert_eq!(strip_explain_prefix("EXPLAIN ANALYZE SELECT 1"), "SELECT 1");
    assert_eq!(
        strip_explain_prefix("explain format=json select 1"),
        "select 1"
    );
    assert_eq!(
        strip_explain_prefix("EXPLAIN FORMAT = TREE SELECT 1"),
        "SELECT 1"
    );
    assert_eq!(
        strip_explain_prefix("EXPLAIN QUERY PLAN SELECT 1"),
        "SELECT 1"
    );
    assert_eq!(strip_explain_prefix("SELECT 1"), "SELECT 1");
    assert_eq!(strip_explain_prefix("explained"), "explained");
    let sources = StatementSources::read(
        "EXPLAIN ANALYZE SELECT * FROM orders o WHERE o.id = 1",
        PlanDialect::MySql,
    );
    assert_eq!(
        sources.resolve("o").map(|t| t.name).as_deref(),
        Some("orders")
    );
}

#[test]
fn mysql_tree_plan_gets_real_table_names_from_the_query() {
    let mut plan = fixture("mysql", "mysql84-q1-join-tree.txt", PlanDialect::MySql);
    assert_eq!(
        names(&plan),
        vec![
            ("o".to_string(), None),
            ("c".to_string(), None),
            ("oi".to_string(), None)
        ]
    );
    let before = findings_for_plan(&plan);
    let context = PlanContext::new().with_query_text(Some(Q1));
    let resolved = resolve_plan_aliases(&mut plan, &context);
    assert_eq!(resolved.len(), 3);
    assert!(resolved.iter().all(|r| r.source == AliasSource::QueryText));
    assert_eq!(
        names(&plan),
        vec![
            ("orders".to_string(), Some("o".to_string())),
            ("customers".to_string(), Some("c".to_string())),
            ("order_items".to_string(), Some("oi".to_string()))
        ]
    );
    // Resolving twice changes nothing.
    assert!(resolve_plan_aliases(&mut plan, &context).is_empty());

    let after = findings_for_plan(&plan);
    let sql = |findings: &[crate::plans::PlanFinding]| -> String {
        findings
            .iter()
            .filter_map(|finding| finding.action_sql.clone())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let notes = |findings: &[crate::plans::PlanFinding]| -> String {
        findings
            .iter()
            .flat_map(|finding| finding.notes.iter())
            .map(|note| note.fallback().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(sql(&before).contains("ON `o`"), "{}", sql(&before));
    assert!(notes(&before).contains("may be an alias"));
    assert!(sql(&after).contains("ON `orders`"), "{}", sql(&after));
    assert!(!sql(&after).contains("ON `o`"));
    assert!(!notes(&after).contains("may be an alias"));
}

#[test]
fn findings_with_context_match_a_pre_resolved_plan() {
    let plan = fixture("mysql", "mysql84-q1-join-tree.txt", PlanDialect::MySql);
    let context = PlanContext::new().with_query_text(Some(Q1));
    let direct = findings_for_plan_with_context(&plan, &context);
    let resolved = plan_with_context(&plan, &context);
    assert_eq!(direct, findings_for_plan(&resolved));
    assert!(
        direct
            .iter()
            .any(|finding| finding.summary.fallback().contains("orders")),
        "{:?}",
        direct
            .iter()
            .map(|finding| finding.summary.fallback().to_string())
            .collect::<Vec<_>>()
    );
}

#[test]
fn story_names_the_real_tables() {
    let plan = fixture("mysql", "mysql84-q1-join-table.txt", PlanDialect::MySql);
    let context = PlanContext::new().with_query_text(Some(Q1));
    let story = batch_story_with_context(std::slice::from_ref(&plan), &context);
    let text = story.statements[0].story.to_text();
    assert!(text.contains("orders"), "{text}");
    assert!(text.contains("customers"), "{text}");
    assert!(text.contains("order_items"), "{text}");
}

#[test]
fn formats_that_print_the_real_name_are_left_alone() {
    // MySQL JSON format 2 carries table_name and alias.
    let mut plan = fixture("mysql", "mysql84-q1-join-v2.json", PlanDialect::MySql);
    let before = names(&plan);
    assert!(before.contains(&("orders".to_string(), Some("o".to_string()))));
    // A wrong query must not rename anything.
    let context =
        PlanContext::new().with_query_text(Some("SELECT * FROM wrong o, bad c, worse oi"));
    assert!(resolve_plan_aliases(&mut plan, &context).is_empty());
    assert_eq!(names(&plan), before);

    // PostgreSQL is never touched.
    let mut pg = ExecutionPlan {
        dialect: Some(PlanDialect::Postgres),
        nodes: vec![crate::plans::PlanNode {
            physical_op: "Seq Scan".to_string(),
            relation: Some("o".to_string()),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(resolve_plan_aliases(&mut pg, &context).is_empty());
    assert_eq!(pg.nodes[0].relation.as_deref(), Some("o"));
}

#[test]
fn sqlite_alias_is_resolved_and_unaliased_table_is_confirmed() {
    let mut plan = ExecutionPlan {
        dialect: Some(PlanDialect::Sqlite),
        ..Default::default()
    };
    for (id, line) in [
        "SCAN o",
        "SEARCH c USING INTEGER PRIMARY KEY (rowid=?)",
        "SCAN products",
    ]
    .into_iter()
    .enumerate()
    {
        plan.nodes.push(crate::plans::PlanNode {
            id: id as i32,
            physical_op: line.to_string(),
            logical_op: line.to_string(),
            ..Default::default()
        });
    }
    let context = PlanContext::new().with_query_text(Some(
        "SELECT * FROM orders AS o JOIN customers c ON c.id = o.customer_id, products",
    ));
    let resolved = resolve_plan_aliases(&mut plan, &context);
    assert_eq!(resolved.len(), 3, "{resolved:?}");
    assert_eq!(plan.nodes[0].relation.as_deref(), Some("orders"));
    assert_eq!(plan.nodes[0].alias.as_deref(), Some("o"));
    assert_eq!(plan.nodes[1].relation.as_deref(), Some("customers"));
    assert_eq!(plan.nodes[2].relation.as_deref(), Some("products"));
    assert_eq!(plan.nodes[2].alias, None);
    assert!(plan.nodes.iter().all(|node| node.relation_resolved));
}

#[test]
fn schema_alone_resolves_only_an_unambiguous_alias() {
    let schema = SchemaContext::from_ddl(
        "CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT, status VARCHAR(20), total DECIMAL(10,2));
         CREATE TABLE customers (id INT PRIMARY KEY, name VARCHAR(100), status VARCHAR(20));
         CREATE TABLE order_items (id INT PRIMARY KEY, order_id INT, quantity INT);
         CREATE INDEX order_items_order_idx ON order_items (order_id);",
        PlanDialect::MySql,
    );
    let mut plan = fixture(
        "mysql",
        "mysql84-q1-join-analyze-tree.txt",
        PlanDialect::MySql,
    );
    let context = PlanContext::new().with_schema(Some(&schema));
    let resolved = resolve_plan_aliases(&mut plan, &context);
    // `oi` is read through an index only order_items has. `o` is filtered
    // on `status`, which two tables have, and `c` is read by PRIMARY, which
    // every table has: both stay as printed.
    assert_eq!(
        resolved
            .iter()
            .map(|r| (r.printed.as_str(), r.table.as_str(), r.source))
            .collect::<Vec<_>>(),
        vec![("oi", "order_items", AliasSource::Schema)]
    );
    assert_eq!(
        names(&plan),
        vec![
            ("o".to_string(), None),
            ("c".to_string(), None),
            ("order_items".to_string(), Some("oi".to_string()))
        ]
    );
}

#[test]
fn qualified_columns_are_found_with_and_without_quotes() {
    assert_eq!(
        qualified_columns("(o.`status` = 'paid') and (`o`.`total` > 5)", "o"),
        vec!["status".to_string(), "total".to_string()]
    );
    assert_eq!(
        qualified_columns("(foo.status = 1)", "o"),
        Vec::<String>::new()
    );
    assert_eq!(
        qualified_columns("id=o.customer_id", "o"),
        vec!["customer_id"]
    );
}

#[test]
fn batch_resolves_each_plan_against_its_own_statement() {
    let scan = |name: &str| ExecutionPlan {
        dialect: Some(PlanDialect::Sqlite),
        nodes: vec![crate::plans::PlanNode {
            physical_op: format!("SCAN {name}"),
            logical_op: format!("SCAN {name}"),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut plans = vec![scan("t"), scan("t")];
    let context = PlanContext::new()
        .with_query_text(Some("SELECT * FROM orders t; SELECT * FROM customers t;"));
    let resolved = resolve_batch_aliases(&mut plans, &context);
    assert_eq!(resolved.len(), 2);
    assert_eq!(plans[0].nodes[0].relation.as_deref(), Some("orders"));
    assert_eq!(plans[1].nodes[0].relation.as_deref(), Some("customers"));

    // One plan, two statements that disagree: nothing is resolved.
    let mut one = scan("t");
    assert!(resolve_plan_aliases(&mut one, &context).is_empty());
    assert_eq!(one.nodes[0].relation, None);
}

#[test]
fn sources_inside_subqueries_and_comma_lists_are_read() {
    let read = |sql: &str| StatementSources::read(sql, PlanDialect::MySql);
    let table = |sources: &StatementSources, name: &str| sources.resolve(name).map(|t| t.name);

    let s = read("SELECT * FROM a x, b y WHERE x.id = y.id AND x.k IN (SELECT k FROM c z)");
    assert_eq!(table(&s, "x").as_deref(), Some("a"));
    assert_eq!(table(&s, "y").as_deref(), Some("b"));
    assert_eq!(table(&s, "z").as_deref(), Some("c"));

    // The alias of a derived table names no table; the table inside does.
    let s =
        read("SELECT * FROM (SELECT * FROM orders o2) d JOIN customers c ON c.id = d.customer_id");
    assert_eq!(table(&s, "d"), None);
    assert_eq!(table(&s, "o2").as_deref(), Some("orders"));
    assert_eq!(table(&s, "c").as_deref(), Some("customers"));

    let s = read("DELETE FROM orders WHERE id IN (SELECT order_id FROM order_items oi)");
    assert_eq!(table(&s, "orders").as_deref(), Some("orders"));
    assert_eq!(table(&s, "oi").as_deref(), Some("order_items"));

    let s = read("UPDATE orders o SET total = 1 WHERE o.id = 2");
    assert_eq!(table(&s, "o").as_deref(), Some("orders"));

    let s = read("INSERT INTO archive SELECT * FROM `orders` AS `o` WHERE o.id < 5");
    assert_eq!(table(&s, "o").as_deref(), Some("orders"));
    assert_eq!(table(&s, "archive").as_deref(), Some("archive"));

    // FROM inside a function call is not a table list, and a table function
    // is not a table.
    let s = read(
        "SELECT EXTRACT(YEAR FROM o.created_at), TRIM(BOTH ' ' FROM o.note) \
         FROM orders o USE INDEX (ix_a, ix_b) LEFT JOIN json_table(o.doc, '$') jt ON 1 = 1 \
         WHERE o.id > 1 ORDER BY 1",
    );
    assert_eq!(table(&s, "o").as_deref(), Some("orders"));
    assert_eq!(table(&s, "created_at"), None);
    assert_eq!(table(&s, "jt"), None);
    assert!(s.aliases.len() == 1 && s.plain.is_empty(), "{s:?}");

    // Unbalanced and truncated text must not panic.
    for sql in [
        "SELECT * FROM",
        "SELECT * FROM (",
        ")))) FROM a b",
        "WITH x (",
        "FROM , ,",
    ] {
        let _ = read(sql);
    }
}
