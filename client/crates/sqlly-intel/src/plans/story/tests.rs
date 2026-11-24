use super::*;
use crate::plans::import::import_plan_document;

fn samples_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/planview-wasm/samples")
}

fn sample_names() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(samples_dir())
        .expect("samples directory")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.') && !name.ends_with(".md"))
        .collect();
    names.sort();
    names
}

fn story_text(text: &str) -> String {
    let imported = import_plan_document(text).expect("plan parses");
    let batch = batch_story(&imported.plans);
    let mut out = String::new();
    if let Some(headline) = &batch.headline {
        out.push_str(headline.fallback());
        out.push_str("\n\n");
    }
    for statement in &batch.statements {
        if batch.statements.len() > 1 {
            out.push_str(&format!("--- statement {} ---\n", statement.index + 1));
        }
        out.push_str(&statement.story.to_text());
        out.push('\n');
    }
    out
}

/// Prints the story for every sample plan and, with `PLAN_STORY_DIR` set,
/// for every file below that directory. Run with
/// `cargo test -p sqlly-intel --lib plans::story::tests::print_stories -- --ignored --nocapture`.
#[test]
#[ignore = "prints the stories for reading; not an assertion"]
fn print_stories() {
    for name in sample_names() {
        let text = std::fs::read_to_string(samples_dir().join(&name)).expect("sample reads");
        println!("===== {name} =====\n{}", story_text(&text));
    }
    if let Ok(dir) = std::env::var("PLAN_STORY_DIR") {
        let mut stack = vec![std::path::PathBuf::from(dir)];
        let mut files = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    files.push(path);
                }
            }
        }
        files.sort();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            match std::panic::catch_unwind(|| {
                import_plan_document(&text).map(|_| story_text(&text))
            }) {
                Ok(Ok(story)) => println!("===== {} =====\n{story}", path.display()),
                Ok(Err(error)) => {
                    println!("===== {} =====\n(not parsed: {error:?})\n", path.display())
                }
                Err(_) => println!("===== {} =====\nPANIC\n", path.display()),
            }
        }
    }
}

/// Prints the parsed fields the analysis reads, for checking a story
/// against the plan it came from.
#[test]
#[ignore = "prints parsed plan fields for reading; not an assertion"]
fn print_sample_fields() {
    for name in sample_names() {
        let text = std::fs::read_to_string(samples_dir().join(&name)).expect("sample reads");
        let imported = import_plan_document(&text).expect("plan parses");
        println!("===== {name} =====");
        for plan in &imported.plans {
            println!(
                "PLAN dialect={:?} actuals={} exec={:?} planning={:?} cost={} dop={:?}",
                plan.dialect,
                plan.has_actuals,
                plan.execution_time_ms,
                plan.planning_time_ms,
                plan.total_subtree_cost,
                plan.degree_of_parallelism
            );
            let analysis = analyze_plan(plan);
            for (node, facts) in plan.nodes.iter().zip(&analysis.nodes) {
                println!(
                    "  #{} d{} [{}] est_rows={} est_exec={:?} cost={} act={:?} total={:?} read={:?} loops={:?} t={:?} self={:?} rm={:?} mode={:?} par={:?} rel={:?} sub={:?} | self_ms={:?} share={:?} cost_share={:?}",
                    node.id,
                    facts.depth,
                    node.physical_op,
                    node.estimated_rows,
                    node.estimated_executions,
                    node.estimated_cost,
                    node.actual_rows,
                    node.actual_rows_total,
                    node.rows_read,
                    node.loops,
                    node.actual_time_ms,
                    node.actual_self_time_ms,
                    node.rows_removed_by_filter,
                    node.execution_mode,
                    node.parallel,
                    node.parent_relationship,
                    node.subplan_name,
                    facts.metrics.self_time_ms,
                    facts.metrics.time_share,
                    facts.metrics.cost_share,
                );
                for property in &node.properties {
                    println!("      {property:?}");
                }
            }
        }
    }
}

use crate::plans::diagram::{DiagramOptions, plan_diagram};
use crate::plans::plan_fixtures::*;
use crate::plans::projections::{plan_breakdown, plan_flame_layout, plan_operator_table};

fn story_of(plan: &ExecutionPlan) -> PlanStory {
    plan_story(plan, &analyze_plan(plan))
}

#[test]
fn a_measured_plan_reads_as_a_story() {
    let story = story_of(&pg_join());
    assert_eq!(
        story.to_text(),
        "This query read from customers and orders and returned 1,000 rows in 100 ms.\n\
         Step 3 (Seq Scan on orders o) is the bottleneck: 70 ms of its own work, 70% of the total. It read 100K rows to return 1,000 rows.\n\
         The largest estimate miss is at step 3 (Seq Scan on orders o): the optimizer expected 50 rows and got 1,000 rows, 20 times more.\n\
         \n\
         1. Index Scan on customers c using customers_pkey: Uses the index customers_pkey to find rows in customers and returns 200 rows. Took 10 ms of its own, 10% of the total.\n\
         2. Hash: Builds a hash table from 200 rows for the join above it.\n\
         3. Seq Scan on orders o: Scans orders and returns 1,000 rows. Keeps only rows where o.status = 'open', which discards 99K rows (99% of the rows it read). The optimizer expected 50 rows here and got 1,000 rows, 20 times more. Took 70 ms of its own, 70% of the total.\n\
         4. Hash Join: Builds a hash table from step 2, probes it with the rows from step 3 and returns 1,000 rows. Rows are matched on o.customer_id = c.id. Took 8 ms of its own, 8% of the total.\n\
         5. Sort: Sorts 1,000 rows by o.created_at DESC. Took 10 ms of its own, 10% of the total.\n"
    );
}

#[test]
fn an_estimated_plan_only_says_what_is_expected() {
    let story = story_of(&pg_join_estimated());
    assert_eq!(
        story.to_text(),
        "The optimizer expects this query to read from customers and orders and return about 1,000 rows, at an estimated cost of 500.\n\
         Step 3 (Seq Scan on orders o) carries 60% of the estimated cost, more than any other step.\n\
         This is an estimated plan: the query was not run, so every number is the optimizer's estimate.\n\
         \n\
         1. Index Scan on customers c using customers_pkey: Expects to use the index customers_pkey to find about 200 rows in customers. Accounts for 6% of the estimated cost.\n\
         2. Hash: Expects to build a hash table from about 200 rows for the join above it.\n\
         3. Seq Scan on orders o: Expects to scan orders and return about 50 rows. Keeps only rows where o.status = 'open'. Accounts for 60% of the estimated cost.\n\
         4. Hash Join: Expects to build a hash table from step 2, probe it with the rows from step 3 and return about 1,000 rows. Rows are matched on o.customer_id = c.id. Accounts for 12% of the estimated cost.\n\
         5. Sort: Expects to sort about 1,000 rows by o.created_at DESC. Accounts for 20% of the estimated cost.\n"
    );
    // Nothing in an estimated story claims something happened.
    for step in &story.steps {
        let sentence = step.sentence.fallback();
        assert!(sentence.starts_with("Expects"), "{sentence}");
        assert!(
            step.notes
                .iter()
                .all(|note| note.kind != StoryNoteKind::Time)
        );
    }
}

#[test]
fn steps_are_tied_to_nodes_and_to_each_other() {
    let plan = pg_join();
    let analysis = analyze_plan(&plan);
    let story = plan_story(&plan, &analysis);
    let nodes: Vec<i32> = story.steps.iter().map(|step| step.node_id).collect();
    assert_eq!(nodes, analysis.execution_order);
    for (index, step) in story.steps.iter().enumerate() {
        assert_eq!(step.number, index + 1);
        assert_eq!(
            story.step_for_node(step.node_id).map(|found| found.number),
            Some(step.number)
        );
        for input in &step.inputs {
            assert!(*input < step.number, "inputs come first");
        }
    }
    let join = story.step_for_node(2).expect("join step");
    // In plan order: the probe side (step 3) is the first input, the hash
    // (step 2) the second.
    assert_eq!(join.inputs, vec![3, 2]);
    assert_eq!(join.depth, 1);
    let orders = story.step_for_node(3).expect("orders step");
    assert!(orders.is_bottleneck);
    assert_eq!(orders.share, Some(0.7));
    assert_eq!(orders.share_metric, PlanMetric::Time);
    assert_eq!(
        story.steps.iter().filter(|step| step.is_bottleneck).count(),
        1
    );
    let kinds: Vec<StoryNoteKind> = orders.notes.iter().map(|note| note.kind).collect();
    assert_eq!(
        kinds,
        vec![
            StoryNoteKind::RowsRemoved,
            StoryNoteKind::Estimate,
            StoryNoteKind::Time
        ]
    );
    // Only bookkeeping operators (computing expressions, returning the
    // result) are foldable; a hash build is part of the join however cheap.
    assert!(!story.step_for_node(4).expect("hash").minor);
    let bottleneck = story.bottleneck.as_ref().expect("bottleneck");
    assert_eq!((bottleneck.node_id, bottleneck.step), (3, 3));
    assert_eq!(bottleneck.share, 0.7);
    assert_eq!(
        bottleneck
            .detail
            .as_ref()
            .map(|detail| detail.fallback().to_string())
            .as_deref(),
        Some("It read 100K rows to return 1,000 rows.")
    );
    assert_eq!(story.step_for_node(999), None);
}

#[test]
fn loops_are_spelled_out() {
    let story = story_of(&pg_nested_loop());
    assert_eq!(
        story.to_text(),
        "This query read from customers and orders and returned 200 rows in 60.2 ms.\n\
         Step 2 (Index Scan on orders o using orders_customer_id_idx) is the bottleneck: 50 ms of its own work, 83% of the total. It ran 100 times.\n\
         \n\
         1. Seq Scan on customers c: Scans customers and returns 100 rows. Took 5 ms of its own, 8.3% of the total.\n\
         2. Index Scan on orders o using orders_customer_id_idx: Uses the index orders_customer_id_idx to find rows in orders and returns 200 rows. The index is searched for o.customer_id = c.id. Ran 100 times, returning about 2 rows each time. Took 50 ms of its own, 83% of the total.\n\
         3. Nested Loop: For each row from step 1, looks for matches in step 2, and returns 200 rows. Took 5 ms of its own, 8.3% of the total.\n"
    );
}

#[test]
fn a_batch_names_the_statement_that_took_the_time() {
    let batch = batch_story(&[pg_join(), pg_nested_loop()]);
    assert_eq!(batch.statements.len(), 2);
    assert_eq!(
        batch
            .headline
            .as_ref()
            .map(|headline| headline.fallback().to_string())
            .as_deref(),
        Some("This batch has 2 statements. Statement 1 took 63% of the time.")
    );
    assert_eq!(batch.statements[1].index, 1);
    let shares: f64 = batch
        .statements
        .iter()
        .filter_map(|statement| statement.share)
        .sum();
    assert!((shares - 1.0).abs() < 1e-9);
    assert!(
        batch_story(std::slice::from_ref(&pg_join()))
            .headline
            .is_none()
    );
    assert!(batch_story(&[]).statements.is_empty());
}

#[test]
fn plans_without_numbers_still_tell_what_happens() {
    let plan = plan(
        crate::plans::PlanDialect::Sqlite,
        vec![op(1, "SCAN orders"), op(2, "USE TEMP B-TREE FOR ORDER BY")],
        &[],
    );
    let story = story_of(&plan);
    assert!(story.bottleneck.is_none());
    assert_eq!(story.steps.len(), 2);
    for step in &story.steps {
        assert!(step.share.is_none());
        assert!(!step.sentence.fallback().contains("Expects"));
    }
    let empty = story_of(&ExecutionPlan::default());
    assert!(empty.steps.is_empty());
    assert!(!empty.headline.fallback().is_empty());
}

#[test]
fn every_sample_tells_a_complete_story() {
    let mut told = 0;
    for name in sample_names() {
        let text = std::fs::read_to_string(samples_dir().join(&name)).expect("sample reads");
        let Ok(imported) = import_plan_document(&text) else {
            continue;
        };
        let batch = batch_story(&imported.plans);
        for (plan, statement) in imported.plans.iter().zip(&batch.statements) {
            let story = &statement.story;
            assert_eq!(
                story.steps.len(),
                plan.nodes.len(),
                "{name}: one step per operator"
            );
            assert!(!story.headline.fallback().trim().is_empty(), "{name}");
            for step in &story.steps {
                let sentence = step.sentence.fallback();
                assert!(!sentence.trim().is_empty(), "{name}: empty sentence");
                assert!(sentence.ends_with('.'), "{name}: {sentence}");
                assert!(
                    !sentence.contains('{'),
                    "{name}: unfilled placeholder in {sentence}"
                );
                assert!(
                    !sentence.contains("  "),
                    "{name}: double space in {sentence}"
                );
                for note in &step.notes {
                    let message = note.message.fallback();
                    assert!(!message.contains('{'), "{name}: {message}");
                    assert!(
                        !message.contains("NaN") && !message.contains("inf"),
                        "{name}: {message}"
                    );
                }
                if !statement.analysis.has_actuals {
                    assert!(
                        !sentence.contains(" returns ") && !sentence.contains("Took "),
                        "{name}: an estimated plan claims a measurement: {sentence}"
                    );
                }
            }
            told += 1;
        }
    }
    assert!(told >= 10, "only {told} sample plans parsed");
}

/// Everything a viewer computes when a plan opens, on a 2,000-operator
/// plan. The bound is generous for a debug build on a loaded machine; the
/// point is to fail loudly if any pass turns quadratic (2,000 squared is
/// four million steps per pass, which blows well past it).
#[test]
fn a_two_thousand_node_plan_is_analysed_narrated_and_laid_out_quickly() {
    let plan = large(2000);
    assert!(plan.nodes.len() >= 2000);
    // Warm up once so lazy statics are not timed.
    let _ = plan_story(&plan, &analyze_plan(&plan));
    let mut best = std::time::Duration::MAX;
    let mut parts = [std::time::Duration::ZERO; 4];
    for _ in 0..5 {
        let start = std::time::Instant::now();
        let analysis = analyze_plan(&plan);
        let analysed = start.elapsed();
        let story = plan_story(&plan, &analysis);
        let narrated = start.elapsed();
        let diagram = plan_diagram(&plan, &analysis, &DiagramOptions::default());
        let laid_out = start.elapsed();
        let table = plan_operator_table(&analysis);
        let flame = plan_flame_layout(&analysis, None).expect("flame");
        let breakdown = plan_breakdown(&analysis, 10).expect("breakdown");
        let total = start.elapsed();
        assert_eq!(story.steps.len(), plan.nodes.len());
        assert_eq!(diagram.nodes.len(), plan.nodes.len());
        assert_eq!(table.len(), plan.nodes.len());
        assert_eq!(flame.cells.len(), plan.nodes.len());
        assert!(!breakdown.by_operator.is_empty());
        if total < best {
            best = total;
            parts = [
                analysed,
                narrated - analysed,
                laid_out - narrated,
                total - laid_out,
            ];
        }
    }
    println!(
        "{} nodes: analysis {:?}, story {:?}, diagram {:?}, table+flame+breakdown {:?}, total {:?}",
        plan.nodes.len(),
        parts[0],
        parts[1],
        parts[2],
        parts[3],
        best
    );
    let limit = if cfg!(debug_assertions) { 400 } else { 60 };
    assert!(
        best.as_millis() < limit,
        "2,000-node plan took {best:?}, over the {limit} ms guard"
    );

    // Doubling the plan must not quadruple the time.
    let double = large(4000);
    let mut best_double = std::time::Duration::MAX;
    for _ in 0..5 {
        let start = std::time::Instant::now();
        let analysis = analyze_plan(&double);
        let _ = plan_story(&double, &analysis);
        let _ = plan_diagram(&double, &analysis, &DiagramOptions::default());
        best_double = best_double.min(start.elapsed());
    }
    println!("{} nodes: {:?}", double.nodes.len(), best_double);
    assert!(
        best_double.as_secs_f64() < best.as_secs_f64() * 3.2 + 0.005,
        "4,000 nodes took {best_double:?} against {best:?} for 2,000: not linear"
    );
}

/// An Append over five scans that each take a fifth of the time: the
/// largest is still marked as the slowest, but nothing dominates.
fn even_append() -> ExecutionPlan {
    let mut nodes = vec![ran(est(op(1, "Append"), 500.0, 50.0), 500.0, 1.0, 51.0)];
    let mut edges = Vec::new();
    for id in 2..=6 {
        let time = if id == 2 { 10.5 } else { 10.0 };
        nodes.push(ran(
            est(scan(id, "Seq Scan", &format!("events_p{id}")), 100.0, 10.0),
            100.0,
            1.0,
            time,
        ));
        edges.push((1, id));
    }
    plan(PlanDialect::Postgres, nodes, &edges)
}

#[test]
fn the_story_names_a_bottleneck_only_when_one_step_dominates() {
    let story = story_of(&pg_join());
    assert_eq!(story.named_bottleneck(), Some(3));
    assert!(story.bottleneck.as_ref().is_some_and(|b| b.dominant));

    let plan = even_append();
    let analysis = crate::plans::analysis::analyze_plan(&plan);
    let story = plan_story(&plan, &analysis);
    let bottleneck = story
        .bottleneck
        .as_ref()
        .expect("the slowest step is named");
    assert!(!bottleneck.dominant);
    assert!(
        bottleneck
            .message
            .fallback()
            .starts_with("No single step dominates"),
        "{}",
        bottleneck.message.fallback()
    );
    assert_eq!(story.named_bottleneck(), None);
    // The analysis still marks the largest step; a viewer that calls it a
    // bottleneck must check the story first.
    assert!(analysis.node(2).is_some_and(|node| node.flags.bottleneck));
}
