//! Reading a notebook file in any supported format.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use sqlly_services::notebook::{Loaded, SeededIds, load};
use sqlly_services::notebook_security::scan::{ScanReport, scan_notebook_file};

use crate::agent::CliError;

/// Notebooks larger than this are refused before parsing.
pub const MAX_NOTEBOOK_BYTES: u64 = 64 * 1024 * 1024;

/// A file after the malicious-file scan (lane 9), which runs before the
/// file is parsed.
pub enum Scanned {
    /// The scan found something that blocks loading; nothing was parsed.
    Rejected(ScanReport),
    Loaded(Box<Loaded>, ScanReport),
}

/// Scan, then load `.sqllynb`, `.sqlnb.json` or `.ipynb` as a v2 document.
///
/// Formats without a notebook id (v1 files, foreign `.ipynb`) get one
/// derived from the file's path, so their run history stays together
/// across runs instead of starting over with a fresh random id.
pub fn load_scanned(path: &Path) -> Result<Scanned, CliError> {
    let metadata = std::fs::metadata(path)
        .map_err(|e| CliError::io(format!("cannot read {}: {e}", path.display())))?;
    if !metadata.is_file() {
        return Err(CliError::usage(format!("{} is not a file", path.display())));
    }
    if metadata.len() > MAX_NOTEBOOK_BYTES {
        return Err(CliError::usage(format!(
            "{} is larger than {} MiB",
            path.display(),
            MAX_NOTEBOOK_BYTES / 1024 / 1024
        )));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::io(format!("cannot read {}: {e}", path.display())))?;
    let report = scan_notebook_file(&text);
    if report.rejected() {
        return Ok(Scanned::Rejected(report));
    }
    Ok(Scanned::Loaded(Box::new(load_text(path, &text)?), report))
}

/// The findings as one line each.
pub fn findings_text(report: &ScanReport) -> Vec<String> {
    report
        .findings
        .iter()
        .map(|f| {
            let at = if f.location.is_empty() {
                String::new()
            } else {
                format!(" (at {})", f.location)
            };
            format!("{}{at}", f.message().fallback())
        })
        .collect()
}

/// Load a notebook, refusing files the scan blocks.
pub fn load_file(path: &Path) -> Result<Loaded, CliError> {
    match load_scanned(path)? {
        Scanned::Loaded(loaded, _) => Ok(*loaded),
        Scanned::Rejected(report) => Err(CliError::policy(
            format!(
                "refused to open {}: {}",
                path.display(),
                findings_text(&report).join("; ")
            ),
            None,
        )),
    }
}

pub fn load_text(path: &Path, text: &str) -> Result<Loaded, CliError> {
    let mut ids = SeededIds(path_seed(path));
    let display = path.to_string_lossy();
    load(Some(&display), text, &mut ids, None)
        .map_err(|e| CliError::usage(format!("{}: {}", path.display(), e.message().fallback())))
}

fn path_seed(path: &Path) -> u64 {
    let absolute: PathBuf = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let digest = Sha256::digest(absolute.to_string_lossy().as_bytes());
    u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: &str = r#"{"version":1,"cells":[{"id":"cell_1","kind":"sql","source":"SELECT 1"}]}"#;

    #[test]
    fn local_cells_spelled_with_the_slot_are_read_as_local_targets() {
        let text = r#"{"format":"sqlly.notebook","format_version":2,"notebook_id":"n",
            "cells":[{"id":"c_aaaaaaaaaa","kind":"sql","connection_ref":"local","source":"SELECT 1"}]}"#;
        let loaded = load_text(Path::new("n.sqllynb"), text).unwrap();
        let cell = &loaded.doc.cells[0];
        assert_eq!(cell.connection_ref, None);
        assert_eq!(cell.spec["target"], "local");
        assert_eq!(
            super::super::remap::Slots::key_for(&loaded.doc, cell),
            super::super::remap::LOCAL_SLOT
        );
    }

    #[test]
    fn id_less_formats_get_a_stable_id_per_path() {
        let a = load_text(Path::new("/nb/a.sqlnb.json"), V1).unwrap();
        let again = load_text(Path::new("/nb/a.sqlnb.json"), V1).unwrap();
        let b = load_text(Path::new("/nb/b.sqlnb.json"), V1).unwrap();
        assert_eq!(a.doc.notebook_id, again.doc.notebook_id);
        assert_ne!(a.doc.notebook_id, b.doc.notebook_id);
        assert_eq!(a.migrated_from, Some(1));
    }

    #[test]
    fn credential_bearing_files_are_refused_before_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.sqllynb");
        std::fs::write(
            &path,
            r#"{"format":"sqlly.notebook","format_version":2,"notebook_id":"n","connection_refs":[{"id":"a","label":"x","connection_string":"Server=s;User Id=sa;Password=hunter2"}],"cells":[]}"#,
        )
        .unwrap();
        let err = load_file(&path).unwrap_err();
        assert_eq!(err.exit, crate::agent::ExitCode::PolicyRefused);
        assert!(!err.body.message.contains("hunter2"));
    }

    #[test]
    fn not_a_notebook_is_a_usage_error() {
        let err = load_text(Path::new("x.sqllynb"), "[1, 2]").unwrap_err();
        assert_eq!(err.exit, crate::agent::ExitCode::Usage);
        let err = load_text(Path::new("x.sqllynb"), "{nope").unwrap_err();
        assert_eq!(err.exit, crate::agent::ExitCode::Usage);
    }
}
