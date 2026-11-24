//! SQL notebooks on the `sqlly_services::notebook` core: the GPUI shell (virtualized cell list,
//! rails, inspector, activity panel), the cell renderer registry, and the
//! host-facing pieces (autosave, trust, connection routing, promote).
//!
//! The shell talks to the core only through [`doc_adapter::ShellDoc`].

pub mod ai;
pub mod analysis;
pub mod autosave;
pub mod browser;
pub mod cells;
pub mod connection;
pub mod dashboard;
pub mod doc_adapter;
pub mod editing;
pub mod embedded;
pub mod export_dialog;
pub mod files;
pub mod history;
pub mod layout;
pub mod local;
pub mod outputs;
pub mod params;
pub mod present;
pub mod preview;
pub mod promote;
mod rails;
pub mod reveal;
pub mod trust;
pub mod view;
pub mod visual;

#[cfg(test)]
pub(crate) mod perf_counters;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod tests;

use gpui::actions;

/// Key context of the notebook body; every notebook binding is scoped to it.
pub const KEY_CONTEXT: &str = "Notebook";
/// Extra key context present only while the notebook is in command mode (no
/// cell editor focused), so single-letter commands never reach typing.
pub const COMMAND_KEY_CONTEXT: &str = "NotebookCommand";

actions!(
    notebook,
    [
        RunCellAndAdvance,
        RunCellAndInsertBelow,
        EnterCommandMode,
        SelectNextCell,
        SelectPreviousCell,
        InsertCellAbove,
        InsertCellBelow,
        DeleteCell,
        ConvertToMarkdown,
        ConvertToSql,
        EditCell,
    ]
);
