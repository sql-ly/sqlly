//! Hand-built plans for the tests of the analysis, story, diagram,
//! projection and comparison modules. Every plan here is small enough to
//! check by hand, so the tests can assert exact numbers.

use super::dialects::PlanDialect;
use super::model::{ExecutionPlan, PlanEdge, PlanNode};

/// A node with only an id and an operator name.
pub(crate) fn op(id: i32, name: &str) -> PlanNode {
    PlanNode {
        id,
        logical_op: name.to_string(),
        physical_op: name.to_string(),
        ..Default::default()
    }
}

/// A node that reads `table`.
pub(crate) fn scan(id: i32, name: &str, table: &str) -> PlanNode {
    PlanNode {
        relation: Some(table.to_string()),
        ..op(id, name)
    }
}

/// Estimated rows and cumulative cost.
pub(crate) fn est(mut node: PlanNode, rows: f64, cost: f64) -> PlanNode {
    node.estimated_rows = rows;
    node.estimated_cost = cost;
    node
}

/// Measured rows per loop, loops, and inclusive time summed over all loops
/// (the model's convention; the PostgreSQL parser multiplies it out).
pub(crate) fn ran(mut node: PlanNode, rows: f64, loops: f64, time_ms: f64) -> PlanNode {
    node.actual_rows = Some(rows);
    node.loops = Some(loops);
    node.actual_time_ms = Some(time_ms);
    node
}

/// A plan from `nodes` and `(parent, child)` edges. The first node is the
/// root.
pub(crate) fn plan(
    dialect: PlanDialect,
    nodes: Vec<PlanNode>,
    edges: &[(i32, i32)],
) -> ExecutionPlan {
    let has_actuals = nodes.iter().any(|node| node.actual_rows.is_some());
    let total = nodes.first().map(|node| node.estimated_cost).unwrap_or(0.0);
    let rows = nodes.first().map(|node| node.estimated_rows).unwrap_or(0.0);
    ExecutionPlan {
        statement_type: "SELECT".into(),
        nodes,
        edges: edges
            .iter()
            .map(|&(from_node, to_node)| PlanEdge { from_node, to_node })
            .collect(),
        total_subtree_cost: total,
        estimated_rows: rows,
        dialect: Some(dialect),
        has_actuals,
        ..Default::default()
    }
}

/// PostgreSQL `EXPLAIN ANALYZE` shape: a hash join of a sequential scan of
/// `orders` (the slow part) with an index scan of `customers`, under a sort.
///
/// Inclusive times: sort 100, join 90, orders scan 70, hash 12, customers 10.
/// Self times: sort 10, join 8, orders scan 70, hash 2, customers 10.
pub(crate) fn pg_join() -> ExecutionPlan {
    let mut sort = ran(est(op(1, "Sort"), 1000.0, 500.0), 1000.0, 1.0, 100.0);
    sort.sort_keys = vec!["o.created_at DESC".into()];
    let mut join = ran(est(op(2, "Hash Join"), 1000.0, 400.0), 1000.0, 1.0, 90.0);
    join.join_type = Some("Inner".into());
    join.join_predicate = Some("(o.customer_id = c.id)".into());
    let mut orders = ran(
        est(scan(3, "Seq Scan", "orders"), 50.0, 300.0),
        1000.0,
        1.0,
        70.0,
    );
    orders.alias = Some("o".into());
    orders.filter = Some("(o.status = 'open'::text)".into());
    orders.rows_removed_by_filter = Some(99000.0);
    let hash = ran(est(op(4, "Hash"), 200.0, 40.0), 200.0, 1.0, 12.0);
    let mut customers = ran(
        est(scan(5, "Index Scan", "customers"), 200.0, 30.0),
        200.0,
        1.0,
        10.0,
    );
    customers.alias = Some("c".into());
    customers.index_name = Some("customers_pkey".into());
    let mut plan = plan(
        PlanDialect::Postgres,
        vec![sort, join, orders, hash, customers],
        &[(1, 2), (2, 3), (2, 4), (4, 5)],
    );
    plan.execution_time_ms = Some(100.5);
    plan.planning_time_ms = Some(0.4);
    plan
}

/// The same query as [`pg_join`] without measurements (plain `EXPLAIN`).
pub(crate) fn pg_join_estimated() -> ExecutionPlan {
    let mut plan = pg_join();
    for node in &mut plan.nodes {
        node.actual_rows = None;
        node.loops = None;
        node.actual_time_ms = None;
        node.rows_removed_by_filter = None;
    }
    plan.has_actuals = false;
    plan.execution_time_ms = None;
    plan.planning_time_ms = None;
    plan
}

/// PostgreSQL nested loop whose inner index scan runs once per outer row:
/// 100 loops of 2 rows each, 50 ms in all.
///
/// Self times: join 5, customers 5, orders 50. PostgreSQL prints the inner
/// side's cost per loop, so its 8.5 stands for 850 and the join's own cost
/// is 900 - 20 - 850 = 30.
pub(crate) fn pg_nested_loop() -> ExecutionPlan {
    let mut join = ran(est(op(1, "Nested Loop"), 200.0, 900.0), 200.0, 1.0, 60.0);
    join.join_type = Some("Inner".into());
    let mut outer = ran(
        est(scan(2, "Seq Scan", "customers"), 100.0, 20.0),
        100.0,
        1.0,
        5.0,
    );
    outer.alias = Some("c".into());
    let mut inner = ran(
        est(scan(3, "Index Scan", "orders"), 2.0, 8.5),
        2.0,
        100.0,
        50.0,
    );
    inner.alias = Some("o".into());
    inner.index_name = Some("orders_customer_id_idx".into());
    inner.index_cond = Some("(o.customer_id = c.id)".into());
    let mut plan = plan(
        PlanDialect::Postgres,
        vec![join, outer, inner],
        &[(1, 2), (1, 3)],
    );
    plan.execution_time_ms = Some(60.2);
    plan
}

/// A chain of `count` operators over one table scan, each a parent of the
/// next. Used for depth and performance tests.
pub(crate) fn chain(count: usize) -> ExecutionPlan {
    let mut nodes = Vec::with_capacity(count);
    let mut edges = Vec::with_capacity(count);
    for index in 0..count {
        let id = index as i32 + 1;
        let remaining = (count - index) as f64;
        let node = if index + 1 == count {
            scan(id, "Seq Scan", "events")
        } else {
            op(id, "Result")
        };
        nodes.push(ran(est(node, 10.0, remaining), 10.0, 1.0, remaining));
        if index + 1 < count {
            edges.push((id, id + 1));
        }
    }
    plan(PlanDialect::Postgres, nodes, &edges)
}

/// A bushy synthetic plan of about `count` nodes: a left-deep spine of hash
/// joins, each with a scan-under-hash on the build side, sorts and
/// aggregates mixed in. Deterministic, so timing runs are comparable.
pub(crate) fn large(count: usize) -> ExecutionPlan {
    let mut nodes: Vec<PlanNode> = Vec::with_capacity(count + 4);
    let mut edges: Vec<(i32, i32)> = Vec::with_capacity(count + 4);
    let mut next_id = 1;
    let mut take = || {
        let id = next_id;
        next_id += 1;
        id
    };
    let joins = count / 4;
    let mut parent: Option<i32> = None;
    for level in 0..joins {
        let remaining = (joins - level) as f64;
        let wrap = take();
        let mut wrapper = if level % 7 == 0 {
            let mut sort = op(wrap, "Sort");
            sort.sort_keys = vec![format!("t{level}.created_at")];
            sort
        } else {
            let mut aggregate = op(wrap, "HashAggregate");
            aggregate.group_keys = vec![format!("t{level}.kind")];
            aggregate
        };
        wrapper = ran(
            est(wrapper, 900.0, remaining * 40.0 + 3.0),
            1000.0,
            1.0,
            remaining * 4.0 + 0.3,
        );
        nodes.push(wrapper);
        if let Some(parent) = parent {
            edges.push((parent, wrap));
        }
        let join_id = take();
        let mut join = ran(
            est(op(join_id, "Hash Join"), 900.0, remaining * 40.0),
            1000.0,
            1.0,
            remaining * 4.0,
        );
        join.join_type = Some("Inner".into());
        join.join_predicate = Some(format!("(t{level}.id = t{}.parent_id)", level + 1));
        nodes.push(join);
        edges.push((wrap, join_id));
        let hash_id = take();
        nodes.push(ran(est(op(hash_id, "Hash"), 500.0, 12.0), 500.0, 1.0, 1.5));
        let scan_id = take();
        let mut table = ran(
            est(
                scan(scan_id, "Seq Scan", &format!("table_{level}")),
                500.0,
                10.0,
            ),
            500.0 + (level % 13) as f64 * 400.0,
            1.0,
            1.2,
        );
        table.alias = Some(format!("t{level}"));
        table.filter = Some(format!("(t{level}.active AND (t{level}.kind = 'a'::text))"));
        nodes.push(table);
        edges.push((join_id, hash_id));
        edges.push((hash_id, scan_id));
        parent = Some(join_id);
    }
    let last = take();
    let mut tail = ran(
        est(scan(last, "Index Scan", "table_tail"), 100.0, 5.0),
        100.0,
        1.0,
        0.8,
    );
    tail.index_name = Some("table_tail_pkey".into());
    nodes.push(tail);
    if let Some(parent) = parent {
        edges.push((parent, last));
    }
    let mut plan = plan(PlanDialect::Postgres, nodes, &edges);
    plan.execution_time_ms = Some(joins as f64 * 4.0 + 1.0);
    plan
}
