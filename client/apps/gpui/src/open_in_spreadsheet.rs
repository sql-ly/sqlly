//! "Open in spreadsheet": export the current result set to a temp file and
//! open it in the OS default spreadsheet app (Excel, Numbers, LibreOffice
//! Calc, …). Reuses the exact export writers the Export dialog uses — the
//! only new pieces are format selection, the private temp file, and
//! best-effort cleanup.
//!
//! The workbook build, the file write, and the launch run on the background
//! executor ([`spawn_open`]); callers gather the rows on the UI thread and
//! report failures when the task resolves. The file is created with
//! [`crate::fs_safe::private_temp_file`] — exclusively, owner-only, inside a
//! `sqlly-spreadsheet` directory — so other local users cannot read query
//! results out of a shared temp folder, and a planted symlink cannot
//! redirect the write. The launcher is the shared platform opener
//! ([`crate::reveal_path::platform_command`]: macOS `open`, Linux
//! `xdg-open`, Windows `explorer`), which passes the path as one argument
//! with no shell parsing in between. Temp files are tracked per session and
//! deleted on app quit; a sweep on creation removes leftovers older than a
//! day from sessions that ended without a clean quit.

#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;

use crate::preferences::ExportPreferences;
use crate::results::ExportFormat;

/// Which container the temp export uses: a real `.xlsx` workbook when that
/// format is enabled (its writer is built in, with banded rows and an
/// auto-filter table), CSV otherwise — every spreadsheet app opens both.
pub fn spreadsheet_format(prefs: &ExportPreferences) -> ExportFormat {
    if prefs.standard_enabled(ExportFormat::Xlsx) {
        ExportFormat::Xlsx
    } else {
        ExportFormat::Csv
    }
}

/// UTF-8 byte-order mark. Excel reads a BOM-less CSV in the system ANSI
/// code page, which garbles every non-ASCII value.
const UTF8_BOM: &str = "\u{FEFF}";

/// The bytes an open-in-spreadsheet export writes for `columns`/`rows`:
/// an XLSX workbook or CSV text (per [`spreadsheet_format`]), with the
/// user's formula-guard preference applied to CSV and a UTF-8 BOM so Excel
/// decodes it as UTF-8. `None` only when the writer has nothing to produce
/// for the format.
pub fn spreadsheet_bytes(
    prefs: &ExportPreferences,
    db: sqlly_lang::sqlmodel::DatabaseType,
    columns: &[sqlly_core::ResultColumn],
    rows: &[Vec<sqlly_core::CellValue>],
) -> Option<(Vec<u8>, ExportFormat)> {
    let format = spreadsheet_format(prefs);
    let bytes = if format.is_binary() {
        crate::results::export_bytes_for_format(format, columns, rows)?
    } else {
        let opts = crate::results::adhoc_export_options(format, prefs);
        let text = crate::results::export_text_for_format_with(format, db, columns, rows, &opts)?;
        with_utf8_bom(format, text).into_bytes()
    };
    Some((bytes, format))
}

/// Browser build: a page cannot launch a desktop app, so the spreadsheet
/// export downloads instead. Browsers set to open that file type
/// automatically (Chrome's "Always open files of this type", Safari's "Open
/// safe files") hand it straight to the spreadsheet app. Returns the file
/// name the browser was given.
#[cfg(target_arch = "wasm32")]
pub fn download(bytes: &[u8], format: ExportFormat) -> Result<String, String> {
    let filename = format!("results.{}", format.extension());
    crate::browser_download::download_bytes(&filename, bytes)?;
    Ok(filename)
}

/// Prefix CSV text with a UTF-8 BOM (once). Other formats pass through.
pub fn with_utf8_bom(format: ExportFormat, text: String) -> String {
    if format == ExportFormat::Csv && !text.starts_with(UTF8_BOM) {
        format!("{UTF8_BOM}{text}")
    } else {
        text
    }
}

/// Temp-file purpose (the `sqlly-<purpose>` directory under the OS temp
/// dir) and name stem.
#[cfg(not(target_arch = "wasm32"))]
const TEMP_PURPOSE: &str = "spreadsheet";
#[cfg(not(target_arch = "wasm32"))]
const TEMP_STEM: &str = "sqlly-results";

/// Temp files are considered stale after this long and swept on the next
/// open (they belong to a session that never got to purge them).
#[cfg(not(target_arch = "wasm32"))]
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Per-session registry of temp files to purge on app quit.
#[cfg(not(target_arch = "wasm32"))]
mod registry {
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    fn files() -> &'static Mutex<Vec<PathBuf>> {
        static FILES: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
        FILES.get_or_init(|| Mutex::new(Vec::new()))
    }

    pub fn track(path: PathBuf) {
        if let Ok(mut guard) = files().lock() {
            guard.push(path);
        }
    }

    pub fn purge() {
        if let Ok(mut guard) = files().lock() {
            for path in guard.drain(..) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Delete stale open-in-spreadsheet temp files (from crashed or killed
/// sessions) in `dir`. Best-effort: every error is ignored.
#[cfg(not(target_arch = "wasm32"))]
fn sweep_stale_temp_files(dir: &std::path::Path) {
    let cutoff = std::time::SystemTime::now() - STALE_AFTER;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(TEMP_STEM) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| modified < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Register the quit hook that purges this session's temp files. Registered
/// on the first open of the session only.
#[cfg(not(target_arch = "wasm32"))]
fn ensure_quit_purge(cx: &mut gpui::App) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static REGISTERED: AtomicBool = AtomicBool::new(false);
    if REGISTERED.swap(true, Ordering::Relaxed) {
        return;
    }
    cx.on_app_quit(|_cx| {
        registry::purge();
        async {}
    })
    .detach();
}

/// Write `bytes` to a fresh private temp file for `format`. On failure no
/// file is left behind.
#[cfg(not(target_arch = "wasm32"))]
fn write_private_temp(
    bytes: &[u8],
    format: ExportFormat,
) -> Result<PathBuf, sqlly_core::DisplayMessage> {
    use std::io::Write as _;
    if let Ok(dir) = crate::fs_safe::private_temp_dir(TEMP_PURPOSE) {
        sweep_stale_temp_files(&dir);
    }
    let suffix = format!(".{}", format.extension());
    let (mut file, path) = crate::fs_safe::private_temp_file(TEMP_PURPOSE, TEMP_STEM, &suffix)
        .map_err(|err| {
            crate::localization::deferred(
                "results.open_in_spreadsheet.create_temp_error",
                &[("error", &err.to_string())],
            )
        })?;
    if let Err(err) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(crate::localization::deferred(
            "results.open_in_spreadsheet.write_temp_error",
            &[
                ("file_path", &path.display().to_string()),
                ("error", &err.to_string()),
            ],
        ));
    }
    Ok(path)
}

/// Write `bytes` to a private temp file and open it in the OS default app.
/// Blocking — run it off the UI thread (see [`spawn_open`]). The file is
/// tracked for cleanup on app quit; on any failure it is removed before
/// returning the error, which the UI thread resolves to display text.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_temp_and_open(
    bytes: &[u8],
    format: ExportFormat,
) -> Result<PathBuf, sqlly_core::DisplayMessage> {
    let path = write_private_temp(bytes, format)?;
    let Some(mut cmd) = crate::reveal_path::platform_command(&path) else {
        let _ = std::fs::remove_file(&path);
        return Err(crate::localization::deferred_text(
            "results.open_in_spreadsheet.no_opener_error",
        ));
    };
    if let Err(err) = cmd.spawn() {
        let _ = std::fs::remove_file(&path);
        return Err(crate::localization::deferred(
            "results.open_in_spreadsheet.open_error",
            &[
                ("file_path", &path.display().to_string()),
                ("error", &err.to_string()),
            ],
        ));
    }
    registry::track(path.clone());
    Ok(path)
}

/// Build the file with `build` (an XLSX workbook or CSV text for rows the
/// caller already gathered on the UI thread), write it, and open it — all on
/// the background executor, so a large workbook never stalls the UI. The
/// task resolves to the opened path, or a message for Messages / a toast.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_open(
    cx: &mut gpui::App,
    build: impl FnOnce() -> Option<(Vec<u8>, ExportFormat)> + Send + 'static,
) -> gpui::Task<Result<PathBuf, String>> {
    use gpui::AppContext as _;
    ensure_quit_purge(cx);
    // The work runs on the background executor, where the UI language is not
    // visible (translation state is per thread): errors come back as keyed
    // messages and are resolved here, on the foreground executor.
    let work = cx.background_spawn(async move {
        let (bytes, format) = build().ok_or_else(|| {
            crate::localization::deferred_text("results.open_in_spreadsheet.empty_error")
        })?;
        write_temp_and_open(&bytes, format)
    });
    cx.spawn(async move |_cx| work.await.map_err(crate::localization::message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs(xlsx: bool) -> ExportPreferences {
        ExportPreferences {
            xlsx,
            ..Default::default()
        }
    }

    #[test]
    fn format_prefers_xlsx_when_enabled() {
        assert_eq!(spreadsheet_format(&prefs(true)), ExportFormat::Xlsx);
    }

    #[test]
    fn format_falls_back_to_csv_when_xlsx_disabled() {
        assert_eq!(spreadsheet_format(&prefs(false)), ExportFormat::Csv);
    }

    fn sample() -> (
        Vec<sqlly_core::ResultColumn>,
        Vec<Vec<sqlly_core::CellValue>>,
    ) {
        let columns = vec![
            sqlly_core::ResultColumn {
                name: "id".into(),
                data_type: "int".into(),
                nullable: false,
            },
            sqlly_core::ResultColumn {
                name: "formula".into(),
                data_type: "nvarchar".into(),
                nullable: true,
            },
        ];
        let rows = vec![vec![
            sqlly_core::CellValue::I64(1),
            sqlly_core::CellValue::String("=1+1".into()),
        ]];
        (columns, rows)
    }

    #[test]
    fn bytes_are_an_xlsx_zip_when_xlsx_is_enabled() {
        let (columns, rows) = sample();
        let (bytes, format) = spreadsheet_bytes(
            &prefs(true),
            sqlly_lang::sqlmodel::DatabaseType::SqlServer,
            &columns,
            &rows,
        )
        .unwrap();
        assert_eq!(format, ExportFormat::Xlsx);
        assert!(bytes.starts_with(b"PK"), "xlsx is a zip container");
    }

    #[test]
    fn bytes_are_bom_prefixed_csv_with_formula_guard_when_xlsx_is_disabled() {
        let (columns, rows) = sample();
        let mut p = prefs(false);
        p.formula_safe = true;
        let (bytes, format) = spreadsheet_bytes(
            &p,
            sqlly_lang::sqlmodel::DatabaseType::SqlServer,
            &columns,
            &rows,
        )
        .unwrap();
        assert_eq!(format, ExportFormat::Csv);
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.starts_with("\u{FEFF}id,formula"),
            "UTF-8 BOM so Excel decodes non-ASCII text: {text}"
        );
        assert!(!text.contains("\n=1+1"), "formula guard applied: {text}");
    }

    #[test]
    fn bom_is_added_once_and_only_to_csv() {
        let once = with_utf8_bom(ExportFormat::Csv, "a".into());
        assert_eq!(with_utf8_bom(ExportFormat::Csv, once.clone()), once);
        assert_eq!(with_utf8_bom(ExportFormat::Json, "a".into()), "a");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn temp_file_is_private_and_named_for_the_format() {
        let path = write_private_temp(b"PK", ExportFormat::Xlsx).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("sqlly-results-"), "{name}");
        assert!(name.ends_with(".xlsx"), "{name}");
        assert_eq!(
            path.parent()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().to_string()),
            Some("sqlly-spreadsheet".to_string()),
            "lives in the private per-purpose directory, not the shared temp root"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"PK");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "owner-only, not world-readable");
        }
        let second = write_private_temp(b"PK", ExportFormat::Xlsx).unwrap();
        assert_ne!(path, second, "rapid opens never overwrite each other");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(second);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn opener_takes_the_path_as_one_argument_without_a_shell() {
        // Building the command without spawning it, so no spreadsheet app
        // window opens during `cargo test`. A path with shell metacharacters
        // must reach the opener verbatim as a single argument.
        let path = std::env::temp_dir().join("a&b^c results.xlsx");
        let cmd = crate::reveal_path::platform_command(&path).expect("wired for this OS");
        let program = cmd.get_program().to_string_lossy().to_lowercase();
        assert!(!program.contains("cmd"), "no cmd.exe parsing: {program}");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, vec![path.as_os_str()]);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn registry_purge_removes_tracked_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sqlly-results-test-purge.xlsx");
        std::fs::write(&path, b"pk").unwrap();
        registry::track(path.clone());
        registry::purge();
        assert!(!path.exists(), "purge should remove tracked files");
    }
}
