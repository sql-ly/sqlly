//! Export formats and format-rendering free functions, split from the
//! flat `results` module.

use super::*;

use crate::export_options::{self, ExportOptions};

/// Export format options shown in the export menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Tsv,
    Json,
    JsonLines,
    Markdown,
    Html,
    Xml,
    Insert,
    TempTable,
    SqlValues,
    Xlsx,
    SqllyExport,
    CSharp,
    Python,
    Rust,
    JavaScript,
    TypeScript,
    Kotlin,
    Swift,
    Java,
    Go,
    Php,
    Cpp,
}

impl ExportFormat {
    /// The twelve standard formats. Both the toolbar export dropdown and the
    /// results-grid context menu offer these (and the `programming()`
    /// formats) filtered by the user's export preferences.
    pub fn all() -> &'static [ExportFormat] {
        &[
            ExportFormat::Csv,
            ExportFormat::Tsv,
            ExportFormat::Json,
            ExportFormat::JsonLines,
            ExportFormat::Markdown,
            ExportFormat::Html,
            ExportFormat::Xml,
            ExportFormat::Insert,
            ExportFormat::TempTable,
            ExportFormat::SqlValues,
            ExportFormat::Xlsx,
            ExportFormat::SqllyExport,
        ]
    }

    /// The eleven programming-language code-snippet formats.
    pub fn programming() -> &'static [ExportFormat] {
        &[
            ExportFormat::CSharp,
            ExportFormat::Python,
            ExportFormat::Rust,
            ExportFormat::JavaScript,
            ExportFormat::TypeScript,
            ExportFormat::Kotlin,
            ExportFormat::Swift,
            ExportFormat::Java,
            ExportFormat::Go,
            ExportFormat::Php,
            ExportFormat::Cpp,
        ]
    }

    /// The programming language this format generates, if it is one of the
    /// code-snippet formats.
    pub fn code_language(self) -> Option<CodeLanguage> {
        Some(match self {
            ExportFormat::CSharp => CodeLanguage::CSharp,
            ExportFormat::Python => CodeLanguage::Python,
            ExportFormat::Rust => CodeLanguage::Rust,
            ExportFormat::JavaScript => CodeLanguage::JavaScript,
            ExportFormat::TypeScript => CodeLanguage::TypeScript,
            ExportFormat::Kotlin => CodeLanguage::Kotlin,
            ExportFormat::Swift => CodeLanguage::Swift,
            ExportFormat::Java => CodeLanguage::Java,
            ExportFormat::Go => CodeLanguage::Go,
            ExportFormat::Php => CodeLanguage::Php,
            ExportFormat::Cpp => CodeLanguage::Cpp,
            _ => return None,
        })
    }

    /// Returns true if this format produces binary bytes (e.g. XLSX)
    /// rather than a text string.
    pub fn is_binary(self) -> bool {
        matches!(self, ExportFormat::Xlsx)
    }

    pub fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "CSV",
            ExportFormat::Tsv => "TSV",
            ExportFormat::Json => "JSON",
            ExportFormat::JsonLines => "JSON Lines",
            ExportFormat::Markdown => "Markdown",
            ExportFormat::Html => "HTML",
            ExportFormat::Xml => "XML",
            ExportFormat::Insert => "INSERT",
            ExportFormat::TempTable => "Temp Table",
            ExportFormat::SqlValues => "SQL Values",
            ExportFormat::Xlsx => "Excel (XLSX)",
            ExportFormat::SqllyExport => "SQLLY Export",
            other => match other.code_language() {
                Some(lang) => lang.label(),
                None => unreachable!(),
            },
        }
    }

    pub fn localized_label(self) -> String {
        match self.code_language() {
            Some(language) => language.label().to_string(),
            None => crate::localization::text(match self {
                ExportFormat::Csv => "results.export_formats.csv",
                ExportFormat::Tsv => "results.export_formats.tsv",
                ExportFormat::Json => "results.export_formats.json",
                ExportFormat::JsonLines => "results.export_formats.json_lines",
                ExportFormat::Markdown => "results.export_formats.markdown",
                ExportFormat::Html => "results.export_formats.html",
                ExportFormat::Xml => "results.export_formats.xml",
                ExportFormat::Insert => "results.export_formats.insert",
                ExportFormat::TempTable => "results.export_formats.temp_table",
                ExportFormat::SqlValues => "results.export_formats.sql_values",
                ExportFormat::Xlsx => "results.export_formats.xlsx",
                ExportFormat::SqllyExport => "results.export_formats.sqlly_export",
                _ => unreachable!(),
            }),
        }
    }

    /// Inverse of [`Self::extension`] for save-as: the format implied by a
    /// file extension the user typed into the save dialog. Ambiguous `.sql`
    /// resolves to INSERT statements (the most common save-as intent);
    /// unknown extensions return `None` so the caller can fall back to its
    /// default format.
    pub fn from_extension(ext: &str) -> Option<ExportFormat> {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        Some(match ext.as_str() {
            "csv" => ExportFormat::Csv,
            "tsv" => ExportFormat::Tsv,
            "json" => ExportFormat::Json,
            "jsonl" => ExportFormat::JsonLines,
            "md" | "markdown" => ExportFormat::Markdown,
            "html" | "htm" => ExportFormat::Html,
            "xml" => ExportFormat::Xml,
            "sql" => ExportFormat::Insert,
            "xlsx" => ExportFormat::Xlsx,
            "sqllyexport" => ExportFormat::SqllyExport,
            "cs" => ExportFormat::CSharp,
            "py" => ExportFormat::Python,
            "rs" => ExportFormat::Rust,
            "js" => ExportFormat::JavaScript,
            "ts" => ExportFormat::TypeScript,
            "kt" => ExportFormat::Kotlin,
            "swift" => ExportFormat::Swift,
            "java" => ExportFormat::Java,
            "go" => ExportFormat::Go,
            "php" => ExportFormat::Php,
            "cpp" | "cc" | "cxx" => ExportFormat::Cpp,
            _ => return None,
        })
    }

    /// File extension for save-as.
    pub fn extension(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Tsv => "tsv",
            ExportFormat::Json => "json",
            ExportFormat::JsonLines => "jsonl",
            ExportFormat::Markdown => "md",
            ExportFormat::Html => "html",
            ExportFormat::Xml => "xml",
            ExportFormat::Insert => "sql",
            ExportFormat::TempTable => "sql",
            ExportFormat::SqlValues => "sql",
            ExportFormat::Xlsx => "xlsx",
            ExportFormat::SqllyExport => "sqllyexport",
            other => match other.code_language() {
                Some(lang) => lang.extension(),
                None => unreachable!(),
            },
        }
    }
}

/// Render a text export format from arbitrary columns + rows with the
/// format's default options (formula guard on). The SQL-shaped formats
/// (INSERT, temp table, SQL values) render in `db`'s dialect — the engine
/// the results came from. Returns `None` for binary formats (XLSX) — use
/// `export_bytes_for_format`. Production paths go through
/// [`export_text_for_format_with`] + [`adhoc_export_options`] so the user's
/// formula-guard preference applies.
#[cfg_attr(not(test), allow(dead_code))]
pub fn export_text_for_format(
    format: ExportFormat,
    db: sqlly_lang::sqlmodel::DatabaseType,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
) -> Option<String> {
    export_text_for_format_with(
        format,
        db,
        columns,
        rows,
        &ExportOptions::default_for(format),
    )
}

/// Options for a one-shot export that has no options UI of its own — a
/// clipboard copy, the Export… dialog, an explorer "Save Data As", a diff or
/// MCP-activity CSV: the format's defaults with the user's spreadsheet
/// formula-guard preference applied. `prefs` is the loaded export
/// preferences so callers that already hold (or pin, in tests) a copy never
/// re-read the file.
pub fn adhoc_export_options(
    format: ExportFormat,
    prefs: &crate::preferences::ExportPreferences,
) -> ExportOptions {
    ExportOptions {
        formula_safe: prefs.formula_safe,
        ..ExportOptions::default_for(format)
    }
}

/// Text export with explicit [`ExportOptions`].
pub fn export_text_for_format_with(
    format: ExportFormat,
    db: sqlly_lang::sqlmodel::DatabaseType,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    opts: &ExportOptions,
) -> Option<String> {
    Some(match format {
        ExportFormat::Csv => result_export::csv_with(columns, rows, opts),
        ExportFormat::Tsv => result_export::tsv_with(columns, rows, opts),
        ExportFormat::Json => result_export::json(columns, rows),
        ExportFormat::JsonLines => result_export::json_lines(columns, rows),
        ExportFormat::Markdown => result_export::markdown_with(columns, rows, opts),
        ExportFormat::Html => result_export::html_with(columns, rows, opts),
        ExportFormat::Xml => result_export::xml(columns, rows),
        ExportFormat::Insert => result_export::insert_statements_for(db, columns, rows, "Results"),
        ExportFormat::TempTable => result_export::temp_table_sql_for(db, columns, rows, "Results"),
        ExportFormat::SqlValues => result_export::sql_values_for(db, columns, rows, "Results"),
        ExportFormat::SqllyExport => result_export::sqlly_export_envelope(db, columns, rows),
        ExportFormat::Xlsx => return None,
        other => result_export_code::generate(other.code_language()?, columns, rows),
    })
}

/// Render a binary export format (XLSX) from arbitrary columns + rows.
/// Returns `None` for text formats — use `export_text_for_format`.
pub fn export_bytes_for_format(
    format: ExportFormat,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
) -> Option<Vec<u8>> {
    match format {
        ExportFormat::Xlsx => Some(result_export_xlsx::excel_xlsx(
            columns, rows, "Results", "Results",
        )),
        _ => None,
    }
}

/// The toolbar export dropdown's contents under `prefs`: the enabled
/// standard formats and the enabled programming-language formats, in menu
/// order. Kept as a free function so the preference gating is unit-testable
/// without a view.
pub fn toolbar_export_formats(
    prefs: &crate::preferences::ExportPreferences,
) -> (Vec<ExportFormat>, Vec<ExportFormat>) {
    let standard: Vec<ExportFormat> = ExportFormat::all()
        .iter()
        .copied()
        .filter(|f| prefs.standard_enabled(*f))
        .collect();
    let programming: Vec<ExportFormat> = ExportFormat::programming()
        .iter()
        .copied()
        .filter(|f| {
            f.code_language()
                .is_some_and(|lang| prefs.language_enabled(lang))
        })
        .collect();
    (standard, programming)
}

/// The model/type-definition languages the export menus offer under
/// `prefs`: each rides the enable switch of its row-exporter language (Zod
/// and Prisma follow TypeScript, SQLAlchemy follows Python), so the one
/// preference pane keeps governing which languages appear.
pub fn toolbar_model_languages(
    prefs: &crate::preferences::ExportPreferences,
) -> Vec<crate::model_codegen::ModelLanguage> {
    crate::model_codegen::ModelLanguage::all()
        .iter()
        .copied()
        .filter(|lang| prefs.language_enabled(lang.export_language()))
        .collect()
}

/// Write export bytes to `path`. When `gzip` is true the payload is
/// compressed before writing and `.gz` is appended to the path unless it is
/// already present.
pub(crate) fn write_export_bytes(
    path: &std::path::Path,
    bytes: &[u8],
    gzip: bool,
) -> Result<(), String> {
    let final_path = export_options::gz_path(path, gzip);
    let payload = if gzip {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            std::io::Write::write_all(&mut enc, bytes).map_err(|err| err.to_string())?;
            enc.finish().map_err(|err| err.to_string())?
        }
        #[cfg(target_arch = "wasm32")]
        bytes.to_vec()
    } else {
        bytes.to_vec()
    };
    std::fs::write(&final_path, payload).map_err(|error| {
        crate::localization::format(
            "results.export.save_failed",
            &[
                ("file_path", &final_path.display().to_string()),
                ("error", &error.to_string()),
            ],
        )
    })
}
