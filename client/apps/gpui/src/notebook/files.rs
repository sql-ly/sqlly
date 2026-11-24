//! Notebook files: new documents, opening any supported format through the
//! core loader, and saving (canonical `.sqllynb`, or Jupyter for `.ipynb`).
//! Exports go through `export_dialog`.

use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::notebook::document::{EXTENSION, IPYNB_EXTENSION, LEGACY_EXTENSION};
use sqlly_services::notebook::{NotebookDoc, RandomIds};

pub use sqlly_services::notebook::Loaded;

/// A new notebook with one empty SQL cell.
pub fn new_document() -> NotebookDoc {
    NotebookDoc::new("", &mut RandomIds)
}

fn has_extension(path: &str, extension: &str) -> bool {
    path.to_ascii_lowercase()
        .ends_with(&format!(".{}", extension.to_ascii_lowercase()))
}

/// Whether `path` names a notebook SQLLY opens: `.sqllynb`, the older
/// `.sqlnb.json`, or Jupyter `.ipynb`.
pub fn is_notebook_path(path: &str) -> bool {
    [EXTENSION, LEGACY_EXTENSION, IPYNB_EXTENSION]
        .iter()
        .any(|ext| has_extension(path, ext))
}

pub fn is_ipynb_path(path: &str) -> bool {
    has_extension(path, IPYNB_EXTENSION)
}

/// A `.sqlnb.json` file: older SQLLY versions cannot read the current
/// format, so Save offers a `.sqllynb` file instead of overwriting it.
pub fn is_legacy_path(path: &str) -> bool {
    has_extension(path, LEGACY_EXTENSION)
}

/// The extension Save As suggests for a notebook last saved at `path`.
pub fn save_extension(path: Option<&str>) -> &'static str {
    if path.is_some_and(is_ipynb_path) {
        IPYNB_EXTENSION
    } else {
        EXTENSION
    }
}

/// Open a notebook file of any supported format; errors come back as
/// display text. `db` judges which cells of an older file change data.
/// Every open (file, recent file, session restore, recovery, browser) runs
/// the malicious-file scan first, as the command line does, and refuses a
/// file it blocks before parsing it.
pub fn load_file(path: &str, text: &str, db: Option<DatabaseType>) -> Result<Loaded, String> {
    let report = sqlly_services::notebook_security::scan_notebook_file(text);
    if report.rejected() {
        let reasons: Vec<String> = report
            .findings
            .iter()
            .filter(|f| f.severity == sqlly_services::notebook_security::scan::Severity::Reject)
            .map(|f| crate::localization::message(f.message()))
            .collect();
        return Err(crate::localization::format(
            "notebook.shell.open.scan_refused",
            &[("reasons", reasons.join(" "))],
        ));
    }
    sqlly_services::notebook::load(Some(path), text, &mut RandomIds, db)
        .map_err(|e| crate::localization::message(e.message()))
}

/// What the malicious-file scan flagged in `text` without blocking it, as
/// display text, each once. The app shows these when the notebook opens.
pub fn scan_warnings(text: &str) -> Vec<String> {
    let report = sqlly_services::notebook_security::scan_notebook_file(text);
    let mut out: Vec<String> = Vec::new();
    for finding in report.warnings() {
        let message = crate::localization::message(finding.message());
        if !out.contains(&message) {
            out.push(message);
        }
    }
    out
}

/// Read the notebook at `path`: a file, or (browser build) a notebook kept
/// in browser storage.
pub fn read_text(path: &str) -> std::io::Result<String> {
    if super::browser::is_browser_path(path) {
        return super::browser::read_in(super::browser::store(), path);
    }
    std::fs::read_to_string(path)
}

/// Write `text` to the notebook at `path`, atomically for a file.
pub fn write_text(path: &str, text: &str) -> std::io::Result<()> {
    if super::browser::is_browser_path(path) {
        return super::browser::write_in(super::browser::store(), path, text);
    }
    crate::fs_safe::write_document_atomic(std::path::Path::new(path), text.as_bytes())
}

/// Whether a notebook exists at `path`.
pub fn exists(path: &str) -> bool {
    if super::browser::is_browser_path(path) {
        return read_text(path).is_ok();
    }
    std::path::Path::new(path).exists()
}

/// What Save writes to `path`.
pub fn file_text(path: &str, doc: &NotebookDoc) -> String {
    sqlly_services::notebook::save(path, doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = r##"{"version":1,"cells":[
        {"id":"cell_1","kind":"markdown","source":"# Notes"},
        {"id":"cell_2","kind":"sql","source":"SELECT 1 AS n"},
        {"id":"cell_3","kind":"sql","source":"DELETE FROM t WHERE id = 1"}
    ],"params":[["region","EU"]],"next_cell_id":4}"##;

    #[test]
    fn older_notebooks_open_through_the_core_and_save_as_sqllynb() {
        let loaded =
            load_file("/tmp/report.sqlnb.json", V1, Some(DatabaseType::SqlServer)).unwrap();
        assert_eq!(loaded.migrated_from, Some(1));
        let doc = loaded.doc;
        assert_eq!(doc.cells[1].id, "cell_2");
        assert!(doc.cells[1].execution_policy.read_only);
        assert!(
            !doc.cells[2].execution_policy.read_only,
            "v1 write cells keep running"
        );
        assert_eq!(doc.parameters[0].name, "region");
        assert!(is_legacy_path("/tmp/report.sqlnb.json"));
        assert_eq!(save_extension(Some("/tmp/report.sqlnb.json")), "sqllynb");
        assert_eq!(save_extension(Some("C:\\n\\a.IPYNB")), "ipynb");
        let text = file_text("/tmp/report.sqllynb", &doc);
        let back = load_file("/tmp/report.sqllynb", &text, None).unwrap();
        assert_eq!(back.doc, doc);
        assert!(text.ends_with('\n'));
        let ipynb = file_text("/tmp/report.ipynb", &doc);
        assert!(ipynb.contains("\"nbformat\""));
    }

    #[test]
    fn warnings_the_scan_does_not_block_are_reported_once() {
        let flagged = r#"{"format":"sqlly.notebook","format_version":2,"notebook_id":"n","settings":{"run_mode":"reactive"},"cells":[{"id":"c1","kind":"markdown","source":"![x](https://example.com/a.png) ![y](https://example.com/b.png)"}]}"#;
        assert!(load_file("/tmp/flagged.sqllynb", flagged, None).is_ok());
        let warnings = scan_warnings(flagged);
        assert!(!warnings.is_empty(), "{warnings:?}");
        let mut unique = warnings.clone();
        unique.dedup();
        assert_eq!(unique, warnings);
        assert!(scan_warnings(V1).is_empty());
    }

    #[test]
    fn files_the_scan_blocks_are_never_opened() {
        let bad = r#"{"format":"sqlly.notebook","format_version":2,"notebook_id":"n","connection_refs":[{"id":"a","label":"x","connection_string":"Server=s;User Id=sa;Password=hunter2"}],"cells":[]}"#;
        let err = load_file("/tmp/bad.sqllynb", bad, None).unwrap_err();
        assert!(!err.contains("hunter2"), "{err}");
        assert!(
            err.starts_with(&crate::localization::format(
                "notebook.shell.open.scan_refused",
                &[("reasons", "")],
            )),
            "{err}"
        );
        let clean = file_text("/tmp/ok.sqllynb", &new_document());
        assert!(load_file("/tmp/ok.sqllynb", &clean, None).is_ok());
    }

    #[test]
    fn garbage_is_refused_with_a_message() {
        assert!(load_file("/tmp/x.sqllynb", "not json", None).is_err());
        assert!(is_notebook_path("/a/b.SQLLYNB"));
        assert!(!is_notebook_path("/a/b.sql"));
    }
}
