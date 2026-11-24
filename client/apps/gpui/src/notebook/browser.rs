//! Notebooks in the browser build.
//!
//! A browser tab has no file system, so every notebook store the desktop app
//! keeps under `<data dir>/SQLLY/notebooks` lives in the page's notebooks
//! folder instead (`sqlly_services::browser::notebook_files`): recovery
//! copies, trust, run history, the notebook preferences, and a copy of every
//! notebook opened or saved in this browser (its library). A library
//! notebook's path is `browser:library/<notebook>/<file name>`, which the
//! tab, the session and the recent list carry like any file path; the
//! notebook file on the user's computer, when the browser gave the page a
//! handle to it, is written alongside on every save.
//!
//! Cells that need something the browser does not have (the notebook's
//! local DuckDB data, files on disk) show why instead of failing when run.

use std::io;

use sqlly_services::notebook::text_store::valid_name;
use sqlly_services::notebook::{Cell, CellKind, TextStore};

/// Prefix of a notebook path in browser storage.
pub const PATH_PREFIX: &str = "browser:";
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const LIBRARY: &str = "library";
const RECENT: &str = "recent.json";
/// Longest file name kept for a library notebook, in characters.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
const MAX_FILE_NAME: usize = 120;

/// The page's notebooks folder. Native builds never have browser paths; a
/// per-thread memory store stands in so the same code runs in tests.
pub fn store() -> &'static dyn TextStore {
    #[cfg(target_arch = "wasm32")]
    {
        &sqlly_services::browser::notebook_files::BrowserTextStore
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        thread_local! {
            static STORE: &'static sqlly_services::notebook::MemoryTextStore =
                Box::leak(Box::default());
        }
        STORE.with(|store| *store as &'static dyn TextStore)
    }
}

pub fn is_browser_path(path: &str) -> bool {
    path.starts_with(PATH_PREFIX)
}

/// The storage name behind a browser path.
pub fn store_name(path: &str) -> Option<&str> {
    path.strip_prefix(PATH_PREFIX)
        .filter(|name| valid_name(name))
}

/// `name` made safe as one storage name segment: no separators or control
/// characters, no leading dots, at most [`MAX_FILE_NAME`] characters.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    let cleaned = if cleaned.chars().count() > MAX_FILE_NAME {
        let (stem, ext) = cleaned.rsplit_once('.').unwrap_or((cleaned, ""));
        let keep = MAX_FILE_NAME.saturating_sub(ext.chars().count() + 1);
        let stem: String = stem.chars().take(keep).collect();
        if ext.is_empty() {
            stem
        } else {
            format!("{stem}.{ext}")
        }
    } else {
        cleaned.to_string()
    };
    if cleaned.is_empty() {
        format!("notebook.{}", sqlly_services::notebook::document::EXTENSION)
    } else {
        cleaned
    }
}

/// Where this browser keeps its copy of notebook `notebook_id` saved as
/// `file_name`.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn library_path(notebook_id: &str, file_name: &str) -> String {
    format!(
        "{PATH_PREFIX}{LIBRARY}/{}/{}",
        super::history::store::notebook_stem(notebook_id),
        safe_file_name(file_name)
    )
}

fn not_found(path: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, path.to_string())
}

/// Read a browser path from `store`.
pub fn read_in(store: &dyn TextStore, path: &str) -> io::Result<String> {
    let name = store_name(path).ok_or_else(|| not_found(path))?;
    store
        .read(name)
        .map_err(io::Error::other)?
        .ok_or_else(|| not_found(path))
}

/// Write a browser path in `store`.
pub fn write_in(store: &dyn TextStore, path: &str, text: &str) -> io::Result<()> {
    let name = store_name(path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, path.to_string()))?;
    store.write(name, text).map_err(io::Error::other)
}

/// The recent list kept in `store`, without browser notebooks that are gone.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn recent_in(store: &dyn TextStore) -> Vec<String> {
    let list: Vec<String> = store
        .read(RECENT)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    list.into_iter()
        .filter(|path| {
            !is_browser_path(path)
                || store_name(path).is_some_and(|name| matches!(store.read(name), Ok(Some(_))))
        })
        .collect()
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn save_recent_in(store: &dyn TextStore, paths: &[String]) {
    if let Ok(text) = serde_json::to_string_pretty(paths) {
        let _ = store.write(RECENT, &text);
    }
}

/// Whether what the page stores survives a reload (storage may be refused,
/// in a private window for instance).
#[cfg(target_arch = "wasm32")]
pub fn persists() -> bool {
    sqlly_services::browser::notebook_files::backend().persists()
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    /// File handles the page holds, by the browser path of the notebook
    /// they were opened or saved with.
    static HANDLES: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

#[cfg(target_arch = "wasm32")]
pub fn attach_handle(path: &str, handle: Option<String>) {
    HANDLES.with(|handles| {
        let mut handles = handles.borrow_mut();
        match handle {
            Some(handle) => handles.insert(path.to_string(), handle),
            None => handles.remove(path),
        };
    });
}

#[cfg(target_arch = "wasm32")]
pub fn handle_for(path: &str) -> Option<String> {
    HANDLES.with(|handles| handles.borrow().get(path).cloned())
}

/// Also write `text` to the computer's file behind `path` when the page
/// holds its handle and the browser already lets it write there; autosave
/// never asks for permission, and a refusal leaves the browser copy saved.
pub fn write_through(path: &str, text: &str) {
    #[cfg(target_arch = "wasm32")]
    if let Some(handle) = handle_for(path) {
        let write =
            sqlly_services::browser::notebook_files::save_file(handle, text.to_string(), false);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = write.await {
                tracing::warn!(?error, "notebook autosave could not write the file");
            }
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    let _ = (path, text);
}

/// What a cell needs that the browser build does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blocker {
    /// A SQL cell on the notebook's local data (DuckDB).
    LocalData,
    /// A transform, which runs in the notebook's local data.
    Transform,
    /// A data file or image attachment read from disk.
    Attachment,
    /// An export cell, which writes files on the computer.
    ExportCell,
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
impl Blocker {
    fn blocker_key(self) -> &'static str {
        match self {
            Blocker::LocalData => "notebook.browser.cell.local_data",
            Blocker::Transform => "notebook.browser.cell.transform",
            Blocker::Attachment => "notebook.browser.cell.attachment",
            Blocker::ExportCell => "notebook.browser.cell.export",
        }
    }

    /// The finished "not available" sentence for this page.
    pub fn message(self) -> String {
        crate::browser_preview::unavailable(&crate::localization::text(self.blocker_key()))
    }
}

/// Whether a SQL cell runs on the notebook's local data: its spec targets
/// `local`, or it names the reserved `local` connection slot.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn targets_local(cell: &Cell) -> bool {
    cell.kind == CellKind::Sql
        && (cell.spec.get("target").and_then(|v| v.as_str()) == Some("local")
            || cell.connection_ref.as_deref() == Some("local"))
}

/// What keeps `cell` from running in a browser, on any build.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub fn blocker(cell: &Cell) -> Option<Blocker> {
    match cell.kind {
        CellKind::Sql if targets_local(cell) => Some(Blocker::LocalData),
        CellKind::Transform => Some(Blocker::Transform),
        CellKind::Attachment => Some(Blocker::Attachment),
        CellKind::Export => Some(Blocker::ExportCell),
        _ => None,
    }
}

/// Why `cell` cannot run in this build: always `None` on desktop.
pub fn unavailable_reason(cell: &Cell) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        blocker(cell).map(Blocker::message)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = cell;
        None
    }
}

impl super::view::NotebookView {
    /// Show every cell this build cannot run as unavailable, with why.
    pub(super) fn mark_browser_unavailable(&mut self) {
        let ids: Vec<String> = self.doc.cells().map(|c| c.id.to_string()).collect();
        for id in ids {
            self.mark_unavailable_if_blocked(&id);
        }
    }

    /// Mark `id` unavailable when this build cannot run it, blocking the
    /// queued cells that read from it. Returns whether it was.
    pub(super) fn mark_unavailable_if_blocked(&mut self, id: &str) -> bool {
        let Some(reason) = self.doc.model().cell(id).and_then(unavailable_reason) else {
            return false;
        };
        self.doc.apply(
            id,
            sqlly_services::notebook::CellEvent::Unavailable {
                reason: sqlly_core::DisplayMessage::raw(reason.clone()),
            },
        );
        for dependent in self.doc.dependents(id) {
            if self.doc.status(&dependent) == sqlly_services::notebook::CellStatus::Queued {
                self.doc.apply(
                    &dependent,
                    sqlly_services::notebook::CellEvent::Blocked { by: id.to_string() },
                );
            }
        }
        self.cells.entry(id.to_string()).or_default().message =
            Some(sqlly_core::DisplayMessage::raw(reason));
        self.remeasure_cell(id);
        true
    }
}

/// The finished "<format> export is not available" sentence.
#[cfg(target_arch = "wasm32")]
pub fn format_unavailable(format: sqlly_services::report::ReportFormat) -> String {
    crate::browser_preview::unavailable(&crate::localization::format(
        "notebook.browser.export.format_unavailable",
        &[("format", format.extension().to_uppercase())],
    ))
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    /// The export the user was warned about (notebook id and format):
    /// choosing it again goes ahead with the sensitive values left out.
    static EXPORT_WARNED: std::cell::RefCell<Option<(String, super::export_dialog::ExportFormat)>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(target_arch = "wasm32")]
impl super::view::NotebookView {
    /// The browser's Export: the Export dialog's pipeline with `options`,
    /// each file handed to the browser as a download. When redaction finds
    /// sensitive data the first choice explains what is left out and the
    /// same choice again downloads.
    pub fn export_in_browser(
        &mut self,
        options: super::export_dialog::ExportOptions,
        cx: &mut gpui::Context<Self>,
    ) {
        use super::export_dialog::{ExportDecision, build_export};
        if let Some(format) = options.format.report_format().filter(|f| !f.is_available()) {
            self.status(format_unavailable(format), true, cx);
            return;
        }
        let snapshot = self.export_snapshot(cx);
        let key = (snapshot.doc.notebook_id.clone(), options.format);
        let warned = EXPORT_WARNED.with(|w| w.borrow().as_ref() == Some(&key));
        let preview = match build_export(&snapshot, &options, warned) {
            Ok(preview) => preview,
            Err(message) => {
                self.status(message, true, cx);
                return;
            }
        };
        match &preview.decision {
            ExportDecision::Allowed => {}
            ExportDecision::NeedsConfirmation(reason) => {
                EXPORT_WARNED.with(|w| *w.borrow_mut() = Some(key));
                let message = crate::localization::format(
                    "notebook.browser.export.confirm",
                    &[("reason", reason.as_str())],
                );
                self.status(message, true, cx);
                return;
            }
            ExportDecision::Refused(reason) => {
                self.status(reason.clone(), true, cx);
                return;
            }
        }
        EXPORT_WARNED.with(|w| *w.borrow_mut() = None);
        let mut names = Vec::new();
        for file in &preview.files {
            let name = file.path.replace('/', "-");
            if let Err(message) = crate::browser_download::download_bytes(&name, &file.bytes) {
                self.status(message, true, cx);
                return;
            }
            names.push(name);
        }
        let mut message = crate::localization::format(
            "notebook.browser.export.done",
            &[("file_name", names.join(", "))],
        );
        if !preview.notes.is_empty() {
            message.push(' ');
            message.push_str(&crate::localization::format(
                "notebook.browser.export.fidelity",
                &[("notes", preview.notes.join("; "))],
            ));
        }
        if !preview.removed.is_empty() {
            message.push(' ');
            message.push_str(&crate::localization::format(
                "notebook.browser.export.removed",
                &[("items", preview.removed.join("; "))],
            ));
        }
        self.status(message, false, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::notebook::{MemoryTextStore, NotebookDoc, SeededIds};

    #[test]
    fn library_paths_stay_inside_the_library() {
        let path = library_path("nb_123", "Q3 revenue.sqllynb");
        assert_eq!(path, "browser:library/nb_123/Q3 revenue.sqllynb");
        assert!(is_browser_path(&path));
        assert_eq!(store_name(&path), Some("library/nb_123/Q3 revenue.sqllynb"));
        for hostile in [
            "../../trust.json",
            "a/b\\c.ipynb",
            "..",
            "",
            "\u{0}.sqllynb",
        ] {
            let path = library_path("../escape", hostile);
            let name = store_name(&path).expect(hostile);
            assert!(name.starts_with("library/"), "{name}");
            assert_eq!(name.split('/').count(), 3, "{name}");
            assert!(name.split('/').all(|s| s != ".." && s != "."), "{name}");
        }
        let long = format!("{}.ipynb", "x".repeat(500));
        let name = library_path("nb", &long);
        assert!(name.ends_with(".ipynb"));
        assert!(name.rsplit('/').next().unwrap().chars().count() <= MAX_FILE_NAME);
        assert_eq!(store_name("/Users/me/n.sqllynb"), None);
        assert_eq!(store_name("browser:../x"), None);
    }

    #[test]
    fn browser_paths_read_write_and_keep_a_recent_list() {
        let store = MemoryTextStore::default();
        let path = library_path("nb_1", "report.sqllynb");
        assert_eq!(
            read_in(&store, &path).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        write_in(&store, &path, "{}").unwrap();
        assert_eq!(read_in(&store, &path).unwrap(), "{}");
        assert!(write_in(&store, "browser:../x", "{}").is_err());
        let gone = library_path("nb_2", "deleted.sqllynb");
        save_recent_in(
            &store,
            &[path.clone(), gone, "/Users/me/desktop.sqllynb".into()],
        );
        assert_eq!(
            recent_in(&store),
            vec![path, "/Users/me/desktop.sqllynb".to_string()]
        );
    }

    /// The browser walkthrough notebook (the coordinator's VM recipe opens
    /// it): its cells run on the PGlite Chinook sample and its local-data
    /// cell is the one the browser refuses.
    #[test]
    fn the_browser_demo_notebook_runs_on_the_chinook_sample() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../scripts/docs-shots/fixtures/notebooks/browser-chinook.sqllynb");
        let text = std::fs::read_to_string(&path).unwrap();
        let doc = super::super::files::load_file(&path.to_string_lossy(), &text, None)
            .unwrap()
            .doc;
        assert_eq!(doc.connection_refs.len(), 1);
        assert_eq!(
            doc.connection_refs[0].profile_id.as_deref(),
            Some("5baa11e5-0000-0000-0000-000000000002"),
            "the browser build's fixed Chinook sample profile"
        );
        let blocked: Vec<&str> = doc
            .cells
            .iter()
            .filter(|c| blocker(c).is_some())
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(blocked, vec!["cell_6"]);
        let library = library_path(&doc.notebook_id, "Chinook genres.sqllynb");
        let store = MemoryTextStore::default();
        write_in(&store, &library, &text).unwrap();
        let back =
            super::super::files::load_file(&library, &read_in(&store, &library).unwrap(), None)
                .unwrap()
                .doc;
        assert_eq!(back, doc);
    }

    #[test]
    fn local_data_transforms_attachments_and_export_cells_are_browser_blockers() {
        let mut ids = SeededIds(3);
        let mut doc = NotebookDoc::new("t", &mut ids);
        assert_eq!(blocker(&doc.cells[0]), None);
        doc.cells[0].spec = serde_json::json!({ "target": "local" });
        assert_eq!(blocker(&doc.cells[0]), Some(Blocker::LocalData));
        doc.cells[0].spec = serde_json::Value::Null;
        doc.cells[0].connection_ref = Some("local".into());
        assert_eq!(blocker(&doc.cells[0]), Some(Blocker::LocalData));
        for (kind, expected) in [
            (CellKind::Transform, Some(Blocker::Transform)),
            (CellKind::Attachment, Some(Blocker::Attachment)),
            (CellKind::Export, Some(Blocker::ExportCell)),
            (CellKind::Markdown, None),
            (CellKind::Chart, None),
            (CellKind::Profile, None),
            (CellKind::Ai, None),
        ] {
            let i = doc.add_cell(kind.clone(), None, &mut ids);
            assert_eq!(blocker(&doc.cells[i]), expected, "{kind:?}");
            assert_eq!(unavailable_reason(&doc.cells[i]), None, "desktop runs it");
        }
        for blocker in [
            Blocker::LocalData,
            Blocker::Transform,
            Blocker::Attachment,
            Blocker::ExportCell,
        ] {
            assert!(!blocker.message().is_empty());
        }
    }
}
