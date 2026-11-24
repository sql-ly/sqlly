//! Windowed tests for AI cells: rendering stored plans, findings and
//! proposals from the screenshot fixtures, run requests (and none on an
//! untrusted notebook), Run All passing over AI cells, Explain with AI on a
//! failed cell, accept/reject, Apply as one group and Undo AI changes.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{
    AppContext as _, Entity, ParentElement as _, Styled as _, TestAppContext, VisualTestContext,
    px, size,
};
use sqlly_services::notebook::cells::ai::{
    AgentMode, AiCellSpec, AiResult, BudgetUsage, ProposalStatus, RunStatus, generated_by,
};
use sqlly_services::notebook::{Cell, CellKind, ExecutionPolicy, NotebookDoc};

use crate::notebook::view::{
    CellStatus, DispatchOutcome, NotebookEvent, NotebookOpening, NotebookView,
};

fn init(cx: &mut TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
}

fn open(
    cx: &mut TestAppContext,
    model: NotebookDoc,
    opening: NotebookOpening,
) -> (Entity<NotebookView>, &mut VisualTestContext) {
    init(cx);
    let (root, vcx) = cx.add_window_view(|window, cx| {
        let nb = cx.new(|cx| NotebookView::with_choices(model, opening, Rc::new(Vec::new), cx));
        gpui_component::Root::new(nb, window, cx)
    });
    let view = vcx.update(|_, cx| {
        root.read(cx)
            .view()
            .clone()
            .downcast::<NotebookView>()
            .unwrap()
    });
    (view, vcx)
}

fn draw(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let view = view.clone();
    cx.draw(
        gpui::Point::default(),
        size(px(1400.0), px(900.0)),
        move |_, _| gpui::div().size_full().child(view.clone()),
    );
}

fn record(
    view: &Entity<NotebookView>,
    cx: &mut VisualTestContext,
) -> Rc<RefCell<Vec<NotebookEvent>>> {
    let events = Rc::new(RefCell::new(Vec::new()));
    let seen = events.clone();
    cx.update(|_, cx| {
        cx.subscribe(view, move |_, event: &NotebookEvent, _| {
            seen.borrow_mut().push(event.clone())
        })
        .detach();
    });
    events
}

fn ai_requests(events: &Rc<RefCell<Vec<NotebookEvent>>>) -> Vec<(String, bool)> {
    events
        .borrow()
        .iter()
        .filter_map(|e| match e {
            NotebookEvent::AiRunRequested { cell_id, consented } => {
                Some((cell_id.clone(), *consented))
            }
            _ => None,
        })
        .collect()
}

fn fixture(name: &str) -> NotebookDoc {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../scripts/docs-shots/fixtures/notebooks")
        .join(format!("{name}.sqllynb"));
    let text = std::fs::read_to_string(&path)
        .unwrap()
        .replace("{P_DEV}", "0c3d2b1a-5f4e-4d3c-8b2a-1f0e9d8c7b6a");
    crate::notebook::files::load_file(&path.to_string_lossy(), &text, None)
        .unwrap()
        .doc
}

fn ai_cell(doc: &NotebookDoc) -> String {
    doc.cells
        .iter()
        .find(|c| c.kind == CellKind::Ai)
        .map(|c| c.id.clone())
        .unwrap()
}

#[test]
fn every_fixture_ai_cell_holds_a_typed_result() {
    for (name, status) in [
        ("ai-plan", Some(RunStatus::AwaitingApproval)),
        ("ai-build", Some(RunStatus::Completed)),
        ("ai-investigate", Some(RunStatus::Completed)),
        ("ai-review", Some(RunStatus::Completed)),
        ("ai-explain-error", Some(RunStatus::Completed)),
        ("ai-consent", None),
    ] {
        let doc = fixture(name);
        let spec = AiCellSpec::of(doc.cell(&ai_cell(&doc)).unwrap());
        assert_eq!(spec.result.as_ref().map(|r| r.status), status, "{name}");
        if let Some(result) = &spec.result {
            assert!(!result.summary.is_empty(), "{name}");
            for finding in &result.findings {
                if let Some(message) = &finding.message {
                    assert!(
                        sqlly_i18n::contains_key(&message.key),
                        "{name}: {}",
                        message.key
                    );
                }
            }
        }
    }
    let review = fixture("ai-review");
    let spec = AiCellSpec::of(review.cell(&ai_cell(&review)).unwrap());
    let fanout = &spec.result.unwrap().findings[0];
    assert_eq!(
        crate::localization::stored_message(fanout.message.as_ref().unwrap(), ""),
        fanout.text,
        "the stored English text is the catalog template filled in"
    );
}

#[gpui::test]
fn stored_results_render_with_inputs_and_labels(cx: &mut TestAppContext) {
    for name in [
        "ai-plan",
        "ai-build",
        "ai-investigate",
        "ai-review",
        "ai-explain-error",
        "ai-consent",
    ] {
        let doc = fixture(name);
        let id = ai_cell(&doc);
        let (view, vcx) = open(cx, doc, NotebookOpening::New);
        view.update(vcx, |nb, cx| nb.reveal_cell(&id, cx));
        draw(&view, vcx);
        draw(&view, vcx);
        view.read_with(vcx, |nb, _| {
            assert!(
                nb.ai.cells.get(&id).is_some_and(|ui| ui.prompt.is_some()),
                "{name} prompt input"
            );
        });
    }
}

fn ai_doc() -> NotebookDoc {
    let mut doc = NotebookDoc::empty("nbai", "AI");
    let mut sql = Cell::new("cell_1", CellKind::Sql, ExecutionPolicy::default());
    sql.name = Some("orders".into());
    sql.source = "SELECT 1 AS n".into();
    doc.cells.push(sql);
    let mut ai = Cell::new("cell_2", CellKind::Ai, ExecutionPolicy::default());
    ai.source = "What does orders return?".into();
    AiCellSpec::new(AgentMode::Investigate).write_to(&mut ai);
    doc.cells.push(ai);
    doc
}

#[gpui::test]
fn run_requests_go_to_the_host_and_untrusted_notebooks_run_nothing(cx: &mut TestAppContext) {
    let (view, vcx) = open(cx, ai_doc(), NotebookOpening::New);
    let events = record(&view, vcx);
    draw(&view, vcx);
    view.update(vcx, |nb, cx| nb.ai_request_run("cell_2", false, cx));
    assert_eq!(ai_requests(&events), vec![("cell_2".to_string(), false)]);
    view.update(vcx, |nb, cx| nb.ai_request_run("cell_2", true, cx));
    assert_eq!(ai_requests(&events).len(), 2);

    let mut shared = ai_doc();
    shared.notebook_id = "nbai_from_elsewhere".into();
    let (untrusted, ucx) = open(cx, shared, NotebookOpening::File);
    let events = record(&untrusted, ucx);
    untrusted.update(ucx, |nb, cx| nb.ai_request_run("cell_2", false, cx));
    assert!(
        ai_requests(&events).is_empty(),
        "untrusted notebooks never start the agent"
    );
    assert!(!untrusted.read_with(ucx, |nb, _| nb.ai_trust_decision().authorizes("", None)));
}

#[gpui::test]
fn a_finished_run_is_stored_and_run_all_passes_over_ai_cells(cx: &mut TestAppContext) {
    let (view, vcx) = open(cx, ai_doc(), NotebookOpening::New);
    let events = record(&view, vcx);
    draw(&view, vcx);
    view.update(vcx, |nb, cx| {
        nb.ai_started(
            "cell_2",
            crate::notebook::ai::new_cancel(),
            sqlly_services::notebook::cells::ai::Budget::default(),
            cx,
        );
        nb.ai_progress(
            "cell_2",
            sqlly_services::notebook_agent::NotebookAgentEvent::Usage(BudgetUsage {
                queries: 1,
                ..Default::default()
            }),
            cx,
        );
    });
    assert!(view.read_with(vcx, |nb, _| nb.is_ai_running("cell_2")));
    draw(&view, vcx);
    view.update(vcx, |nb, cx| {
        nb.ai_finished(
            "cell_2",
            AiResult {
                run_id: "r1".into(),
                status: RunStatus::Completed,
                summary: "It returns one row.".into(),
                ..AiResult::default()
            },
            cx,
        )
    });
    let spec = view.read_with(vcx, |nb, _| {
        AiCellSpec::of(nb.model().cell("cell_2").unwrap())
    });
    assert_eq!(spec.result.unwrap().summary, "It returns one row.");
    assert!(!view.read_with(vcx, |nb, _| nb.is_ai_running("cell_2")));
    draw(&view, vcx);

    view.update(vcx, |nb, cx| nb.run_all(cx));
    let asked: Vec<String> = events
        .borrow()
        .iter()
        .filter_map(|e| match e {
            NotebookEvent::RunCell { cell_id } => Some(cell_id.clone()),
            _ => None,
        })
        .collect();
    assert!(asked.contains(&"cell_1".to_string()));
    assert!(
        asked.contains(&"cell_2".to_string()),
        "the queue reaches the AI cell"
    );
    view.update(vcx, |nb, cx| {
        nb.dispatch_result("cell_1", DispatchOutcome::Started, cx);
        nb.cell_finished("cell_1", true, cx);
    });
    view.update(vcx, |nb, cx| nb.ai_skip_queued("cell_2", cx));
    assert_eq!(
        view.read_with(vcx, |nb, _| nb.doc().status("cell_2")),
        CellStatus::Fresh
    );
    assert!(
        !view.read_with(vcx, |nb, _| nb.is_running()),
        "the queue finished"
    );
    assert!(
        ai_requests(&events).is_empty(),
        "Run All never starts a model"
    );
}

#[gpui::test]
fn explain_with_ai_adds_an_assist_cell_aimed_at_the_failed_cell(cx: &mut TestAppContext) {
    let mut doc = ai_doc();
    doc.cells.truncate(1);
    let (view, vcx) = open(cx, doc, NotebookOpening::New);
    let events = record(&view, vcx);
    view.update(vcx, |nb, cx| {
        nb.run_scope(
            crate::notebook::doc_adapter::RunScope::Cell,
            Some("cell_1".into()),
            cx,
        );
        nb.dispatch_result(
            "cell_1",
            DispatchOutcome::NotStarted("Invalid column name 'n2'.".into()),
            cx,
        );
    });
    assert_eq!(
        view.read_with(vcx, |nb, _| nb.doc().status("cell_1")),
        CellStatus::Failed
    );
    assert_eq!(
        view.read_with(vcx, |nb, _| nb.last_failed_cell())
            .as_deref(),
        Some("cell_1")
    );
    draw(&view, vcx);
    view.update(vcx, |nb, cx| nb.explain_with_ai("cell_1", cx));
    let (id, spec, source) = view.read_with(vcx, |nb, _| {
        let cell = &nb.model().cells[1];
        (cell.id.clone(), AiCellSpec::of(cell), cell.source.clone())
    });
    assert_eq!(spec.mode, AgentMode::Assist);
    assert_eq!(spec.target.as_deref(), Some("cell_1"));
    assert!(!source.is_empty());
    assert_eq!(ai_requests(&events), vec![(id, false)]);
}

#[gpui::test]
fn accepted_proposals_apply_as_attributed_cells_and_undo_in_one_step(cx: &mut TestAppContext) {
    let doc = fixture("ai-build");
    let ai = ai_cell(&doc);
    let before: Vec<String> = doc.cells.iter().map(|c| c.id.clone()).collect();
    let original = doc.cell("cell_2").unwrap().source.clone();
    let (view, vcx) = open(cx, doc, NotebookOpening::New);
    draw(&view, vcx);
    view.update(vcx, |nb, cx| {
        nb.ai_decide(&ai, "p2", false, cx);
        nb.ai_decide(&ai, "p3", true, cx);
        nb.ai_decide(&ai, "p4", true, cx);
    });
    let spec = view.read_with(vcx, |nb, _| AiCellSpec::of(nb.model().cell(&ai).unwrap()));
    let result = spec.result.unwrap();
    assert_eq!(
        result.proposal("p2").unwrap().status,
        ProposalStatus::Rejected
    );
    assert_eq!(
        result.proposal("p4").unwrap().status,
        ProposalStatus::Refused,
        "refused stays refused"
    );

    view.update(vcx, |nb, cx| nb.ai_apply(&ai, cx));
    let (cells, generated, edited, undo) = view.read_with(vcx, |nb, _| {
        let doc = nb.model();
        (
            doc.cells.len(),
            doc.cells
                .iter()
                .filter(|c| generated_by(c).is_some())
                .count(),
            doc.cell("cell_2").unwrap().source.clone(),
            nb.ai.applied.is_some(),
        )
    });
    assert_eq!(cells, before.len() + 3);
    assert_eq!(generated, 3);
    assert_ne!(edited, original);
    assert!(undo);
    draw(&view, vcx);

    view.update(vcx, |nb, cx| nb.ai_undo(cx));
    let (ids, source) = view.read_with(vcx, |nb, _| {
        (
            nb.model()
                .cells
                .iter()
                .map(|c| c.id.clone())
                .collect::<Vec<_>>(),
            nb.model().cell("cell_2").unwrap().source.clone(),
        )
    });
    assert_eq!(ids, before);
    assert_eq!(source, original);
    draw(&view, vcx);
}

#[gpui::test]
fn the_agent_reads_completed_outputs_from_results_views(cx: &mut TestAppContext) {
    use sqlly_core::{CellValue, ResultColumn};
    use sqlly_services::QueryEvent;
    let (view, vcx) = open(cx, ai_doc(), NotebookOpening::New);
    let results = vcx.update(|_, cx| cx.new(crate::results::ResultsView::new));
    view.update(vcx, |nb, cx| {
        nb.attach_results("cell_1", results.clone(), cx);
        nb.run_scope(
            crate::notebook::doc_adapter::RunScope::Cell,
            Some("cell_1".into()),
            cx,
        );
        nb.dispatch_result("cell_1", DispatchOutcome::Started, cx);
    });
    results.update(vcx, |rv, cx| {
        rv.handle_event(
            QueryEvent::ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "n".into(),
                    data_type: "int".into(),
                    nullable: false,
                }],
            },
            cx,
        );
        rv.handle_event(
            QueryEvent::Rows {
                index: 0,
                rows: vec![vec![CellValue::I64(1)], vec![CellValue::I64(2)]],
            },
            cx,
        );
        rv.handle_event(QueryEvent::Completed { elapsed_ms: 3 }, cx);
    });
    let (outputs, runtime) = view.read_with(vcx, |nb, cx| {
        (super::inputs::outputs(nb, cx), super::inputs::runtime(nb))
    });
    let output = outputs
        .get("cell_1")
        .expect("the completed cell has an output");
    assert_eq!(output.rows.len(), 2);
    assert_eq!(output.row_count, 2);
    assert!(output.completeness.is_complete());
    assert!(!output.run_id.is_empty());
    assert_eq!(runtime["cell_1"].status, CellStatus::Fresh);
    assert!(runtime["cell_1"].parts.is_some());
    assert!(!outputs.contains_key("cell_2"));
}

#[test]
fn the_budget_meter_shows_the_estimated_cost_when_the_provider_has_a_price() {
    use super::render::{CostLine, cost_line};
    use sqlly_services::notebook::cells::ai::Budget;
    use sqlly_services::notebook_agent::{BudgetMeter, Pricing};
    let capped = Budget {
        max_cost_usd: Some(0.5),
        ..Budget::for_mode(AgentMode::Assist)
    };
    // A fake provider run: the meter charges what the turns used.
    let meter = BudgetMeter::new(
        &capped,
        Pricing {
            usd_per_1k_input: Some(0.003),
            usd_per_1k_output: Some(0.015),
        },
    );
    meter.begin_turn(2_000).unwrap();
    meter.charge_reply(1_000);
    let usage = meter.usage();
    match cost_line(&capped, &usage) {
        Some(CostLine::Meter { label, used, cap }) => {
            assert!((used - 0.021).abs() < 1e-9);
            assert_eq!(cap, 0.5);
            assert!(label.contains("0.02") && label.contains("0.50"), "{label}");
        }
        other => panic!("{other:?}"),
    }
    let uncapped = Budget {
        max_cost_usd: None,
        ..capped.clone()
    };
    let Some(CostLine::Note(note)) = cost_line(&uncapped, &usage) else {
        panic!("an estimate without a cap");
    };
    assert!(note.contains("0.02"), "{note}");
    let free = BudgetMeter::new(&capped, Pricing::default());
    free.begin_turn(2_000).unwrap();
    let Some(CostLine::Note(unpriced)) = cost_line(&capped, &free.usage()) else {
        panic!("a cap without a price says so");
    };
    assert_eq!(
        unpriced,
        crate::localization::format("notebook.ai.budget.cost_unpriced", &[("max", "0.50")])
    );
    assert_eq!(cost_line(&uncapped, &free.usage()), None);
}

#[gpui::test]
fn an_applied_ai_group_is_one_structural_undo_step(cx: &mut TestAppContext) {
    let doc = fixture("ai-build");
    let ai = ai_cell(&doc);
    let before: Vec<String> = doc.cells.iter().map(|c| c.id.clone()).collect();
    let (view, vcx) = open(cx, doc, NotebookOpening::New);
    draw(&view, vcx);
    view.update(vcx, |nb, cx| {
        nb.ai_decide(&ai, "p3", true, cx);
        nb.ai_apply(&ai, cx);
    });
    assert!(view.read_with(vcx, |nb, _| nb.ai.applied.is_some()));
    assert_ne!(
        view.read_with(vcx, |nb, _| nb.model().cells.len()),
        before.len()
    );
    // Z in command mode reverses the whole group and Undo AI changes
    // no longer offers it, so the two can't both reverse it.
    view.update(vcx, |nb, cx| nb.undo_structure(cx));
    let ids: Vec<String> = view.read_with(vcx, |nb, _| {
        nb.model().cells.iter().map(|c| c.id.clone()).collect()
    });
    assert_eq!(ids, before);
    assert!(view.read_with(vcx, |nb, _| nb.ai.applied.is_none()));
    view.update(vcx, |nb, cx| nb.ai_undo(cx));
    assert_eq!(
        view.read_with(vcx, |nb, _| nb.model().cells.len()),
        before.len(),
        "a second undo through the AI button does nothing"
    );
}
