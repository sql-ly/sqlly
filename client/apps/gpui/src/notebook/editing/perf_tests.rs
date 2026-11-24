//! Performance probes for large notebooks (spec 17), ignored by default:
//! open-to-editable time, typing and scrolling frame times (p50/p95),
//! list items and cell subtrees built per frame, whole-notebook passes per
//! keystroke, allocations, and retained bytes per cell. Run in an optimized
//! build with
//! `cargo test --profile release-ci -p sqlly-gpui --lib notebook::editing::perf_tests -- --ignored --nocapture --test-threads=1`.
//! Timings depend on machine load (each line prints the load average);
//! the counts do not, so the regression tests at the end pin them.

use std::time::Instant;

use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext, px, size};
use sqlly_services::notebook::{Cell, CellKind, ExecutionPolicy, NotebookDoc};

use super::tests::{AutosaveRoot, draw, open};
use crate::editor::SqlEditor;
use crate::notebook::perf_counters::{self as counters, Pass, Snapshot};
use crate::notebook::view::{Hovered, NotebookEvent, NotebookView};
use crate::results::ResultsView;

fn mixed(n: usize) -> NotebookDoc {
    let mut nb = NotebookDoc::empty("nbperf", "Perf");
    for i in 0..n {
        let (kind, source) = match i % 5 {
            0 => (
                CellKind::Markdown,
                format!("## Section {i}\n\nNotes for section {i}."),
            ),
            3 => (CellKind::Chart, String::new()),
            _ => (
                CellKind::Sql,
                format!(
                    "SELECT o.OrderID, o.CustomerID, o.Total\nFROM dbo.Orders AS o\nWHERE o.OrderID > {i}\nORDER BY o.OrderID;"
                ),
            ),
        };
        let mut cell = Cell::new(format!("cell_{}", i + 1), kind, ExecutionPolicy::default());
        cell.source = source;
        if i % 5 == 1 {
            cell.name = Some(format!("q{i}"));
        }
        if i % 5 == 3 {
            cell.spec = serde_json::json!({"input": format!("cell_{i}"), "chart": {"kind": "bar", "title": format!("Chart {i}")}});
        }
        nb.cells.push(cell);
    }
    nb
}

/// Heap bytes a document's cells hold (strings and spec text), per cell.
fn doc_bytes_per_cell(doc: &NotebookDoc) -> usize {
    let bytes: usize = doc
        .cells
        .iter()
        .map(|c| {
            std::mem::size_of::<Cell>()
                + c.id.len()
                + c.source.len()
                + c.name.as_ref().map_or(0, String::len)
                + c.spec.to_string().len()
        })
        .sum();
    bytes / doc.cells.len().max(1)
}

/// Wall times of a run of frames, in milliseconds.
#[derive(Default)]
struct Frames(Vec<f64>);

impl Frames {
    fn time(&mut self, f: impl FnOnce()) {
        let started = Instant::now();
        f();
        self.0.push(started.elapsed().as_secs_f64() * 1000.0);
    }

    fn quantile(&self, q: f64) -> f64 {
        let mut sorted = self.0.clone();
        sorted.sort_by(f64::total_cmp);
        sorted
            .get(((sorted.len().max(1) - 1) as f64 * q).round() as usize)
            .copied()
            .unwrap_or_default()
    }

    fn summary(&self) -> String {
        format!(
            "p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms",
            self.quantile(0.5),
            self.quantile(0.95),
            self.quantile(1.0)
        )
    }
}

/// Act as the host: answer every editor request with plain entities.
fn serve_entities(view: &Entity<NotebookView>, cx: &mut VisualTestContext) {
    let target = view.downgrade();
    cx.update(|_, cx| {
        cx.subscribe(view, move |_, event: &NotebookEvent, cx| {
            if let NotebookEvent::NeedCellEntities { cell_ids } = event {
                let ids = cell_ids.clone();
                let target = target.clone();
                cx.defer(move |cx| {
                    let pairs: Vec<_> = ids
                        .into_iter()
                        .map(|id| (id, cx.new(SqlEditor::new), cx.new(ResultsView::new)))
                        .collect();
                    target
                        .update(cx, |nb, cx| nb.attach_cell_entities(pairs, cx))
                        .ok();
                });
            }
        })
        .detach();
    });
}

fn whole_passes() -> usize {
    super::super::doc_adapter::FINGERPRINT_PASSES.with(|n| n.get())
        + super::super::doc_adapter::GRAPH_BUILDS.with(|n| n.get())
}

const KEYSTROKES: usize = 60;

/// Keystrokes to type: `NB_PERF_KEYS` overrides the default, for a run
/// long enough to sample with a profiler.
fn keystrokes() -> usize {
    std::env::var("NB_PERF_KEYS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(KEYSTROKES)
}

/// Open `doc` in a 1400×900 window and put the caret in cell `target`'s
/// editor. Returns the view, the window and the editor.
fn edit_in(
    doc: NotebookDoc,
    target: usize,
    inspector: bool,
    cx: &mut TestAppContext,
) -> (
    Entity<NotebookView>,
    &mut VisualTestContext,
    String,
    Entity<SqlEditor>,
) {
    let (view, cx) = open(cx, doc);
    cx.simulate_resize(size(px(1400.0), px(900.0)));
    serve_entities(&view, cx);
    let id = view.read_with(cx, |nb, _| nb.doc.cell(target).unwrap().id.to_string());
    view.update(cx, |nb, cx| {
        nb.update_layout(|l| l.right_visible = inspector, cx);
        nb.list_state.scroll_to(gpui::ListOffset {
            item_ix: target.saturating_sub(1),
            offset_in_item: px(0.0),
        });
        nb.edit_cell(&id, cx);
    });
    for _ in 0..3 {
        cx.run_until_parked();
        view.update(cx, |_, cx| cx.notify());
    }
    cx.run_until_parked();
    assert_eq!(
        cx.update(|window, cx| view.read(cx).focused_cell(window, cx)),
        Some(id.clone()),
        "the caret is in the cell's editor"
    );
    let editor = view
        .read_with(cx, |nb, _| nb.cell_entities(&id))
        .map(|(editor, _)| editor)
        .expect("the cell has a live editor");
    // The pointer rests on the cell above, which draws its buttons too.
    view.update(cx, |nb, cx| {
        if let Some(next) = nb
            .doc
            .cell(target.saturating_sub(1))
            .map(|c| c.id.to_string())
        {
            nb.hover(Hovered::Cell(next), cx);
        }
    });
    cx.run_until_parked();
    (view, cx, id, editor)
}

/// What the platform's text input does with typed characters.
fn type_text(editor: &Entity<SqlEditor>, text: &str, cx: &mut VisualTestContext) {
    cx.update(|window, cx| {
        editor.update(cx, |e, cx| {
            gpui::EntityInputHandler::replace_text_in_range(e, None, text, window, cx)
        })
    });
    cx.run_until_parked();
}

/// Open `n` mixed cells in a 1400×900 window, put the caret in the SQL
/// cell nearest the middle, and type into it through the editor. Every
/// keystroke is timed from dispatch through the window's redraw.
fn typing(n: usize, inspector: bool, cx: &mut TestAppContext) {
    typing_with(n, inspector, false, cx);
}

/// Give every SQL cell in `range` a finished 25-row result, as after Run
/// All.
fn show_results(
    view: &Entity<NotebookView>,
    range: std::ops::Range<usize>,
    cx: &mut VisualTestContext,
) {
    use sqlly_core::{CellValue, ResultColumn};
    use sqlly_services::QueryEvent;
    let ids: Vec<String> = view.read_with(cx, |nb, _| {
        range
            .clone()
            .filter_map(|i| nb.doc.cell(i))
            .filter(|c| c.kind == super::super::doc_adapter::KIND_SQL)
            .map(|c| c.id.to_string())
            .collect()
    });
    for id in ids {
        let results = cx.update(|_, cx| cx.new(ResultsView::new));
        view.update(cx, |nb, cx| nb.attach_results(&id, results.clone(), cx));
        results.update(cx, |r, cx| {
            r.start_query(cx);
            r.handle_event(QueryEvent::Started, cx);
            r.handle_event(
                QueryEvent::ResultSet {
                    index: 0,
                    columns: ["OrderID", "CustomerID", "Total"]
                        .map(|name| ResultColumn {
                            name: name.into(),
                            data_type: "int".into(),
                            nullable: false,
                        })
                        .to_vec(),
                },
                cx,
            );
            r.handle_event(
                QueryEvent::Rows {
                    index: 0,
                    rows: (0..25)
                        .map(|i| {
                            vec![
                                CellValue::I64(i),
                                CellValue::I64(i * 7),
                                CellValue::I64(i * 31),
                            ]
                        })
                        .collect(),
                },
                cx,
            );
            r.handle_event(QueryEvent::Completed { elapsed_ms: 3 }, cx);
        });
        view.update(cx, |nb, _| {
            if let Some(rt) = nb.cells.get_mut(&id) {
                rt.elapsed_ms = Some(3);
            }
        });
    }
    cx.run_until_parked();
}

fn typing_with(n: usize, inspector: bool, results: bool, cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let target = (n / 2..n).find(|i| i % 5 == 2).unwrap_or(1);
    let (view, cx, id, editor) = edit_in(mixed(n), target, inspector, cx);
    if results {
        show_results(&view, target.saturating_sub(20)..target + 20, cx);
    }
    let source_len = |cx: &mut VisualTestContext| {
        view.read_with(cx, |nb, _| nb.doc().cell_by_id(&id).unwrap().source.len())
    };
    let type_text = |text: &str, cx: &mut VisualTestContext| type_text(&editor, text, cx);
    type_text("  ", cx);
    let start_len = source_len(cx);
    super::take_item_renders();
    let before = Snapshot::now();
    let passes = whole_passes();
    let allocs = counters::allocations();
    let mut frames = Frames::default();
    let keys = keystrokes();
    for k in 0..keys {
        let ch = match k % 40 {
            39 => "\n",
            k if k % 8 == 7 => " ",
            _ => "x",
        };
        frames.time(|| type_text(ch, cx));
    }
    let counts = Snapshot::now().since(before);
    let allocs = (counters::allocations() - allocs) / keys;
    let passes = whole_passes() - passes;
    let items = super::take_item_renders() as f64 / keys as f64;
    assert_eq!(
        source_len(cx) - start_len,
        keys,
        "every keystroke reached the cell"
    );
    let settle_started = Instant::now();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(400));
    cx.run_until_parked();
    let settle_ms = settle_started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "TYPING {n} cells{}{} [load {:.1}]: per keystroke {}; list items {items:.1}, cells built {:.1}, buttons {:.1}, allocations {allocs} per keystroke; whole-notebook passes over {keys} keys: {} (fingerprint+graph {passes}; references {}, folds {}, outline {}, history {}, trust {}, find {}, staleness {}); settle after typing {settle_ms:.1} ms",
        if inspector { "" } else { ", inspector hidden" },
        if results { ", with results" } else { "" },
        counters::load_average(),
        frames.summary(),
        counts.of(Pass::CellBuilt) as f64 / keys as f64,
        counts.of(Pass::Button) as f64 / keys as f64,
        counts.whole_notebook(),
        counts.of(Pass::References),
        counts.of(Pass::Folds),
        counts.of(Pass::Outline),
        counts.of(Pass::History),
        counts.of(Pass::Trust),
        counts.of(Pass::Find),
        counts.of(Pass::Staleness),
    );
}

/// Type a character, then let the edit debounce run: what a pause in
/// typing costs (staleness, fingerprints, the graph, and the caches typing
/// put off), timed per pause.
fn settling(n: usize, cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let target = (n / 2..n).find(|i| i % 5 == 2).unwrap_or(1);
    let (_view, cx, _, editor) = edit_in(mixed(n), target, true, cx);
    let mut frames = Frames::default();
    let before = Snapshot::now();
    let passes = whole_passes();
    let pauses = keystrokes().min(200);
    for _ in 0..pauses {
        type_text(&editor, "x", cx);
        frames.time(|| {
            cx.executor()
                .advance_clock(std::time::Duration::from_millis(400));
            cx.run_until_parked();
        });
    }
    let counts = Snapshot::now().since(before);
    println!(
        "SETTLE {n} cells [load {:.1}]: per pause {}; per pause: fingerprint+graph {:.1}, references {:.1}, staleness {:.1}, cells built {:.1}",
        counters::load_average(),
        frames.summary(),
        (whole_passes() - passes) as f64 / pauses as f64,
        counts.of(Pass::References) as f64 / pauses as f64,
        counts.of(Pass::Staleness) as f64 / pauses as f64,
        counts.of(Pass::CellBuilt) as f64 / pauses as f64,
    );
}

#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_settle_1000_cells(cx: &mut TestAppContext) {
    settling(1000, cx);
}

/// Open `n` cells, then scroll down and back up three times in 300 px frames.
/// Reports open time, frame times, what each frame built, and whether
/// the third round trip kept more memory or runtimes than the second.
fn scrolling(n: usize, zen: bool, cx: &mut TestAppContext) {
    scrolling_with(n, zen, false, cx);
}

fn scrolling_with(n: usize, zen: bool, results: bool, cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let doc = mixed(n);
    let per_cell = doc_bytes_per_cell(&doc);
    let live_before = counters::live_bytes();
    let started = Instant::now();
    let (view, cx) = open(cx, doc);
    cx.simulate_resize(size(px(1400.0), px(900.0)));
    if zen {
        view.update(cx, |nb, cx| nb.toggle_zen(cx));
    }
    draw(&view, cx);
    let open_ms = started.elapsed().as_secs_f64() * 1000.0;
    let live_open = counters::live_bytes() - live_before;
    if results {
        show_results(&view, 0..160, cx);
    }
    super::take_item_renders();
    let mut frames = Frames::default();
    let mut items = Vec::new();
    let mut built = 0;
    let mut round_trip = |cx: &mut VisualTestContext, frames: &mut Frames| {
        for step in 0..80 {
            let delta = if step < 40 { 300.0 } else { -300.0 };
            let before = counters::get(Pass::CellBuilt);
            frames.time(|| {
                view.update(cx, |nb, cx| {
                    nb.list_state.scroll_by(px(delta));
                    cx.notify();
                });
            });
            built += counters::get(Pass::CellBuilt) - before;
            items.push(super::take_item_renders());
            // Markdown parses on the background executor; let it finish
            // (untimed and uncounted) so the heap reflects what the app keeps.
            cx.run_until_parked();
            super::take_item_renders();
        }
        let (runtimes, previews) = view.read_with(cx, |nb, _| (nb.cells.len(), nb.preview_count()));
        (counters::live_bytes(), runtimes, previews)
    };
    round_trip(cx, &mut frames);
    let first = round_trip(cx, &mut frames);
    let second = round_trip(cx, &mut frames);
    let frame_count = frames.0.len();
    let history_started = Instant::now();
    view.update(cx, |nb, cx| {
        let id = nb.doc.cell(n / 2).unwrap().id.to_string();
        nb.delete_cells(vec![id], cx);
        nb.undo_structure(cx);
    });
    let undo_ms = history_started.elapsed().as_secs_f64() * 1000.0;
    let max = items.iter().max().copied().unwrap_or(0);
    let avg = items.iter().sum::<usize>() as f64 / items.len() as f64;
    println!(
        "SCROLL {n} cells{}{} [load {:.1}]: open+first frame {open_ms:.1} ms; frame {}; list items/frame avg {avg:.1} max {max}, cells built/frame {:.1}; delete+undo {undo_ms:.1} ms; doc bytes/cell {per_cell}, heap after open {:.0} KB ({:.0} B/cell); round trip 3 vs 2: heap {:+.0} KB, runtimes {} -> {}, previews {} -> {}",
        if zen { " (zen)" } else { "" },
        if results { ", with results" } else { "" },
        counters::load_average(),
        frames.summary(),
        built as f64 / frame_count as f64,
        live_open as f64 / 1024.0,
        live_open as f64 / n as f64,
        (second.0 - first.0) as f64 / 1024.0,
        first.1,
        second.1,
        first.2,
        second.2,
    );
    assert!(max < 60, "virtualized: only cells near the viewport render");
}

/// Heap the notebook keeps per cell that was never drawn: the difference
/// between a 100- and a 1,000-cell notebook after both opened.
#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_bytes_per_offscreen_cell(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut heap = Vec::new();
    for n in [100, 100, 1000] {
        let before = counters::live_bytes();
        let (view, vcx) = open(cx, mixed(n));
        draw(&view, vcx);
        heap.push(counters::live_bytes() - before);
        vcx.update(|window, _| window.remove_window());
        vcx.run_until_parked();
    }
    println!(
        "MEMORY: {:.0} bytes per offscreen cell (document {} bytes/cell)",
        (heap[2] - heap[1]) as f64 / 900.0,
        doc_bytes_per_cell(&mixed(1000)),
    );
}

#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_100_cells(cx: &mut TestAppContext) {
    scrolling(100, false, cx);
}

#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_500_cells(cx: &mut TestAppContext) {
    scrolling(500, false, cx);
}

#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_1000_cells(cx: &mut TestAppContext) {
    scrolling(1000, false, cx);
}

#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_1000_cells_with_results(cx: &mut TestAppContext) {
    scrolling_with(1000, false, true, cx);
}

#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_1000_cells_zen(cx: &mut TestAppContext) {
    scrolling(1000, true, cx);
}

#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_typing_500_cells(cx: &mut TestAppContext) {
    typing(500, true, cx);
}

#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_typing_1000_cells(cx: &mut TestAppContext) {
    typing(1000, true, cx);
}

#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_typing_1000_cells_with_results(cx: &mut TestAppContext) {
    typing_with(1000, true, true, cx);
}

#[gpui::test]
#[ignore = "performance probe; run with --ignored --nocapture"]
fn perf_typing_1000_cells_inspector_hidden(cx: &mut TestAppContext) {
    typing(1000, false, cx);
}

#[gpui::test]
fn typing_never_walks_the_whole_notebook(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx, _, editor) = edit_in(mixed(60), 32, true, cx);
    let before = Snapshot::now();
    let passes = whole_passes();
    for _ in 0..10 {
        type_text(&editor, "x", cx);
    }
    let counts = Snapshot::now().since(before);
    assert_eq!(counts.whole_notebook(), 0, "{counts:?}");
    assert_eq!(whole_passes(), passes, "fingerprints and graph wait too");
    assert!(counts.of(Pass::CellBuilt) > 0, "the frames were drawn");
    view.update(cx, |nb, cx| nb.open_find(false, cx));
    cx.run_until_parked();
    let before = Snapshot::now();
    type_text(&editor, "x", cx);
    assert_eq!(
        Snapshot::now().since(before).of(Pass::Find),
        0,
        "an open find bar catches up when typing pauses"
    );
}

#[gpui::test]
fn what_typing_put_off_catches_up_when_typing_pauses(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let mut doc = mixed(10);
    doc.cells[2].name = Some("source_q".into());
    let (view, cx, id, editor) = edit_in(doc, 2, false, cx);
    type_text(&editor, " -- typed_marker", cx);
    view.update(cx, |nb, cx| {
        nb.open_find(false, cx);
        nb.set_find_query("typed_marker", cx);
    });
    let found = view.read_with(cx, |nb, _| {
        nb.editing
            .find
            .as_ref()
            .map(|f| f.matches.len())
            .unwrap_or(0)
    });
    assert_eq!(found, 1, "find reads what was typed before it opened");
    type_text(&editor, " typed_marker", cx);
    let before = Snapshot::now();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(400));
    cx.run_until_parked();
    let counts = Snapshot::now().since(before);
    assert!(counts.of(Pass::Find) >= 1, "the find bar searched again");
    let found = view.read_with(cx, |nb, _| {
        nb.editing
            .find
            .as_ref()
            .map(|f| f.matches.len())
            .unwrap_or(0)
    });
    assert_eq!(found, 2);
    assert!(view.read_with(cx, |nb, _| {
        nb.doc()
            .cell_by_id(&id)
            .unwrap()
            .source
            .contains("typed_marker")
    }));
}

#[gpui::test]
fn only_the_active_and_hovered_cells_draw_header_buttons(cx: &mut TestAppContext) {
    let _root = AutosaveRoot::new();
    let (view, cx) = open(cx, mixed(12));
    cx.simulate_resize(size(px(1400.0), px(900.0)));
    let buttons_per_frame = |cx: &mut VisualTestContext| {
        let before = counters::get(Pass::Button);
        view.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        counters::get(Pass::Button) - before
    };
    let first = view.read_with(cx, |nb, _| nb.active_cell().map(str::to_string));
    assert_eq!(first.as_deref(), Some("cell_1"));
    let resting = buttons_per_frame(cx);
    let header = |id: &str, cx: &mut VisualTestContext| {
        cx.debug_bounds(Box::leak(format!("nb-cell-header-{id}").into_boxed_str()))
            .expect("the cell is drawn")
    };
    let bounds = header("cell_3", cx);
    cx.simulate_mouse_move(bounds.center(), None, gpui::Modifiers::default());
    cx.run_until_parked();
    assert_eq!(
        view.read_with(cx, |nb, _| nb.hovered.clone()),
        Some(Hovered::Cell("cell_3".into()))
    );
    assert!(view.read_with(cx, |nb, _| nb.shows_controls("cell_3")));
    assert!(!view.read_with(cx, |nb, _| nb.shows_controls("cell_4")));
    let hovering = buttons_per_frame(cx);
    assert!(
        hovering >= resting + 5,
        "the hovered cell gains its buttons: {resting} -> {hovering}"
    );
    // Leaving a cell keeps its buttons, so a menu opened from one stays.
    cx.simulate_mouse_move(
        gpui::point(px(1399.0), px(899.0)),
        None,
        gpui::Modifiers::default(),
    );
    cx.run_until_parked();
    assert!(view.read_with(cx, |nb, _| nb.shows_controls("cell_3")));
    // The keyboard's cell always has them.
    view.update(cx, |nb, cx| nb.activate_cell("cell_5", cx));
    assert!(view.read_with(cx, |nb, _| nb.shows_controls("cell_5")));
    assert!(!view.read_with(cx, |nb, _| nb.shows_controls("cell_1")));
}
