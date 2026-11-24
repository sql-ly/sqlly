//! What the agent reads from the notebook tab: each cell's current output,
//! from the notebook's output registry (`notebook/outputs.rs`), and its run
//! state.

#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::collections::BTreeMap;

use gpui::App;
use sqlly_services::notebook::CellOutput;
use sqlly_services::notebook_agent::CellRuntime;

use crate::notebook::view::{CellStatus, NotebookView};

/// Every cell's current output in the registry. A failed cell's last
/// output is left out: the agent reads the error instead.
pub fn outputs(view: &NotebookView, _cx: &App) -> BTreeMap<String, CellOutput> {
    view.doc
        .cells()
        .filter(|cell| view.doc.status(cell.id) != CellStatus::Failed)
        .filter_map(|cell| {
            let output = view.outputs.get(cell.id)?;
            Some((cell.id.to_string(), output.as_ref().clone()))
        })
        .collect()
}

/// Each cell's state for the agent: status, staleness, its last error and
/// its current fingerprint parts.
pub fn runtime(view: &NotebookView) -> BTreeMap<String, CellRuntime> {
    view.doc
        .cells()
        .map(|cell| {
            let status = view.doc.status(cell.id);
            let error = (status == CellStatus::Failed)
                .then(|| view.cells.get(cell.id).and_then(|rt| rt.message_text()))
                .flatten();
            (
                cell.id.to_string(),
                CellRuntime {
                    status,
                    stale: view.is_stale(cell.id),
                    error,
                    parts: view.doc.cell_fingerprint(cell.id).map(|f| f.parts),
                },
            )
        })
        .collect()
}
