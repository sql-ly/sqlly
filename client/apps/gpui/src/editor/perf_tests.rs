//! Typing-latency harness for large scripts. Ignored by default; run with
//!
//! ```text
//! cargo test -p sqlly-gpui --profile release-ci --lib editor::perf_tests -- --ignored --nocapture
//! ```
//!
//! `SQLLY_EDITOR_PERF_SQL=/path/to/script.sql` swaps the synthetic fixture
//! for a local file (never commit real data). `SQLLY_EDITOR_PERF_DUMP=path`
//! also writes the synthetic fixture there, for trying it in the app.

use super::*;
use std::time::{Duration, Instant};

fn script() -> String {
    if let Ok(path) = std::env::var("SQLLY_EDITOR_PERF_DUMP") {
        std::fs::write(path, super::perf_fixture::import_script()).expect("write fixture");
    }
    match std::env::var("SQLLY_EDITOR_PERF_SQL") {
        Ok(path) => std::fs::read_to_string(path).expect("read SQLLY_EDITOR_PERF_SQL"),
        Err(_) => super::perf_fixture::import_script(),
    }
}

fn summarize(label: &str, samples: &mut [Duration]) {
    samples.sort();
    let total: Duration = samples.iter().sum();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    eprintln!(
        "{label:<22} n={:<3} median {:>8.2} ms  p90 {:>8.2} ms  max {:>8.2} ms  total {:>9.1} ms",
        samples.len(),
        ms(samples[samples.len() / 2]),
        ms(samples[samples.len() * 9 / 10]),
        ms(*samples.last().unwrap()),
        ms(total),
    );
}

#[gpui::test]
#[ignore = "perf harness: run explicitly with --ignored --nocapture"]
fn typing_in_a_large_import_script(cx: &mut gpui::TestAppContext) {
    let sql = script();
    let (view, cx) = cx.add_window_view(|window, cx| {
        let editor = SqlEditor::new(cx);
        editor.focus_handle.focus(window, cx);
        editor
    });

    let t = Instant::now();
    view.update(cx, |editor, cx| editor.set_text(sql.clone(), cx));
    cx.run_until_parked();
    let load = t.elapsed();
    let t = Instant::now();
    cx.update(|window, cx| {
        window.refresh();
        let arena = window.draw(cx);
        arena.clear(cx);
    });
    eprintln!(
        "load {:.1} ms, first draw {:.1} ms ({} bytes, {} lines)",
        load.as_secs_f64() * 1000.0,
        t.elapsed().as_secs_f64() * 1000.0,
        sql.len(),
        sql.lines().count()
    );

    // Park the caret mid-script, at the end of a line, and scroll it in.
    view.update(cx, |editor, cx| {
        let mid = sql.len() / 2;
        let eol = sql[mid..].find('\n').map_or(sql.len(), |i| mid + i);
        editor.cursor = eol;
        editor.selection_anchor = None;
        editor.scroll_cursor_into_view();
        cx.notify();
    });
    cx.run_until_parked();

    let (mut keys, mut parks, mut draws, mut whole) = (vec![], vec![], vec![], vec![]);
    for ch in "SELECT 1 -- typing a short comment".chars() {
        let start = Instant::now();
        cx.simulate_input(&ch.to_string());
        keys.push(start.elapsed());
        let t = Instant::now();
        cx.run_until_parked();
        parks.push(t.elapsed());
        let t = Instant::now();
        cx.update(|window, cx| {
            window.refresh();
            let arena = window.draw(cx);
            arena.clear(cx);
        });
        draws.push(t.elapsed());
        whole.push(start.elapsed());
    }
    let typed = view.read_with(cx, |e, _| {
        e.content.contains("SELECT 1 -- typing a short comment")
    });
    assert!(typed, "keystrokes reached the buffer");
    summarize("keystroke handling", &mut keys);
    summarize("background work", &mut parks);
    summarize("redraw", &mut draws);
    summarize("keystroke→frame", &mut whole);
}

#[test]
#[ignore = "perf harness: run explicitly with --ignored --nocapture"]
fn statement_spans_per_dialect() {
    let sql = script();
    for dialect in sqlly_lang::sqlmodel::Dialect::ALL {
        let start = Instant::now();
        let spans = statements::statement_spans_for(&sql, dialect);
        eprintln!(
            "statement_spans_for {dialect:<10?} {:>4} spans in {:>8.2} ms",
            spans.len(),
            start.elapsed().as_secs_f64() * 1000.0
        );
    }
}
