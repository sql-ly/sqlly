//! Pure model for the custom "Open Project File" picker.
//!
//! Port of `ProjectFilePickerModel.swift`: row/metadata types, the filter
//! query parser (`v:`, `p:`, `f:`, `schema:` …), match scoring, the
//! lightweight SQL metadata scanner (first `CREATE`/`ALTER` object in a
//! file), a minimal gitignore matcher, folder / recursive-project scans, and
//! the bounded preview loader. Everything here is synchronous and
//! filesystem-only so it can run on a background thread and be unit tested
//! without GPUI.

use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerMessage {
    pub key: &'static str,
    pub args: Vec<(&'static str, String)>,
}

impl PickerMessage {
    pub fn new(key: &'static str) -> Self {
        Self {
            key,
            args: Vec::new(),
        }
    }

    pub fn with_arg(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.args.push((name, value.into()));
        self
    }

    pub fn resolve(&self) -> String {
        let args = self
            .args
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect::<Vec<_>>();
        crate::localization::format(self.key, &args)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewText {
    Message(PickerMessage),
    Value(String),
}

impl PreviewText {
    pub fn resolve(&self) -> String {
        match self {
            Self::Message(message) => message.resolve(),
            Self::Value(value) => value.clone(),
        }
    }
}

/// Directory names never shown or scanned, regardless of gitignore.
const BUILT_IN_IGNORED_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "dist",
    "build",
    ".next",
    "target",
    "bin",
    "obj",
];

/// Read at most this much of a SQL file when scanning metadata or previewing.
pub const PREVIEW_BYTE_LIMIT: usize = 256 * 1024;
/// Preview at most this many lines unless a full preview is requested.
pub const PREVIEW_LINE_LIMIT: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    Folder,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SqlObjectKind {
    Table,
    View,
    Trigger,
    Index,
    Sproc,
    Function,
    Type,
    Unknown,
}

impl SqlObjectKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::Trigger => "trigger",
            Self::Index => "index",
            Self::Sproc => "sproc",
            Self::Function => "function",
            Self::Type => "type",
            Self::Unknown => "unknown",
        }
    }

    pub fn localization_key(self) -> &'static str {
        match self {
            Self::Table => "project_picker.kind_table",
            Self::View => "project_picker.kind_view",
            Self::Trigger => "project_picker.kind_trigger",
            Self::Index => "project_picker.kind_index",
            Self::Sproc => "project_picker.kind_sproc",
            Self::Function => "project_picker.kind_function",
            Self::Type => "project_picker.kind_type",
            Self::Unknown => "project_picker.kind_unknown",
        }
    }

    /// Triggers and indexes are only ever scanned from file text in the MVP,
    /// never verified against the live schema model.
    pub fn is_scan_only(self) -> bool {
        matches!(self, Self::Trigger | Self::Index)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataConfidence {
    /// The scanned object also exists in the live completion catalog.
    Indexed,
    /// Parsed from the file text only.
    Scanned,
    Unknown,
}

impl MetadataConfidence {
    pub fn localization_key(self) -> &'static str {
        match self {
            Self::Indexed => "project_picker.source_indexed",
            Self::Scanned => "project_picker.source_scanned",
            Self::Unknown => "project_picker.source_unknown",
        }
    }
}

/// What a `.sql` file appears to define, per a bounded scan of its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlFileMetadata {
    pub schema: Option<String>,
    pub object_name: Option<String>,
    pub kind: SqlObjectKind,
    pub confidence: MetadataConfidence,
    /// More than one top-level CREATE/ALTER was found; metadata describes the
    /// first.
    pub multiple_objects: bool,
}

impl SqlFileMetadata {
    pub const UNKNOWN: Self = Self {
        schema: None,
        object_name: None,
        kind: SqlObjectKind::Unknown,
        confidence: MetadataConfidence::Unknown,
        multiple_objects: false,
    };

    pub fn schema_chip(&self) -> Option<String> {
        self.schema
            .as_deref()
            .filter(|schema| !schema.is_empty())
            .map(str::to_string)
    }

    pub fn type_chip(&self) -> &'static str {
        self.kind.localization_key()
    }

    pub fn source_chip(&self) -> &'static str {
        if self.kind.is_scan_only() {
            "project_picker.source_scan_only"
        } else {
            self.confidence.localization_key()
        }
    }
}

/// An object known to the live schema/completion catalog, used to upgrade
/// scanned metadata to `Indexed` confidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownSchemaObject {
    pub schema: Option<String>,
    pub name: String,
    pub kind: SqlObjectKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerRowKind {
    ParentDirectory,
    Directory,
    File,
}

/// Where a picker row comes from: a file on disk, or an object in the loaded
/// data model whose definition is fetched from the server when opened
/// (`sp_helptext`-style DDL generation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSource {
    Filesystem,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRow {
    /// Empty for `RowSource::Model` rows.
    pub path: PathBuf,
    pub source: RowSource,
    pub kind: PickerRowKind,
    pub name: String,
    pub relative_path: String,
    pub metadata: SqlFileMetadata,
    pub byte_count: Option<u64>,
    pub modified: Option<SystemTime>,
    /// Which field matched the active filter (shown as a chip).
    pub match_reason: Option<PickerMessage>,
}

impl PickerRow {
    pub fn is_sql_file(&self) -> bool {
        self.source == RowSource::Filesystem
            && self.kind == PickerRowKind::File
            && self
                .path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
    }

    /// Whether activating the row opens content (a SQL file or a data-model
    /// object) as opposed to navigating a directory.
    pub fn is_openable(&self) -> bool {
        self.source == RowSource::Model || self.is_sql_file()
    }
}

/// Builds virtual rows for the loaded data model so schema objects can be
/// browsed, filtered, and opened like files. Sorted by qualified name.
pub fn model_object_rows(known_objects: &[KnownSchemaObject]) -> Vec<PickerRow> {
    let mut rows: Vec<PickerRow> = known_objects
        .iter()
        .map(|object| {
            let qualified = match object.schema.as_deref().filter(|s| !s.is_empty()) {
                Some(schema) => format!("{schema}.{}", object.name),
                None => object.name.clone(),
            };
            PickerRow {
                path: PathBuf::new(),
                source: RowSource::Model,
                kind: PickerRowKind::File,
                name: qualified.clone(),
                relative_path: qualified,
                metadata: SqlFileMetadata {
                    schema: object.schema.clone(),
                    object_name: Some(object.name.clone()),
                    kind: object.kind,
                    confidence: MetadataConfidence::Indexed,
                    multiple_objects: false,
                },
                byte_count: None,
                modified: None,
                match_reason: None,
            }
        })
        .collect();
    rows.sort_by(|a, b| compare_ci(&a.name, &b.name));
    rows
}

/// The result of scanning a directory (Folder mode) or a whole tree (Project
/// mode).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PickerSnapshot {
    pub rows: Vec<PickerRow>,
    pub state_title: Option<PickerMessage>,
    pub state_detail: Vec<PickerMessage>,
}

#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    pub include_hidden: bool,
    pub max_project_files: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            include_hidden: false,
            max_project_files: 10_000,
        }
    }
}

/// A metadata cache entry: the file size and mtime the metadata was scanned
/// at, plus the scanned result.
struct CachedMetadata {
    byte_count: Option<u64>,
    modified: Option<SystemTime>,
    metadata: SqlFileMetadata,
}

/// Caches scanned metadata keyed by (path, size, mtime) so rescans don't
/// re-read unchanged files. Shared with the background scan thread.
#[derive(Default)]
pub struct MetadataCache {
    entries: Mutex<HashMap<PathBuf, CachedMetadata>>,
}

impl MetadataCache {
    fn get(
        &self,
        path: &Path,
        byte_count: Option<u64>,
        modified: Option<SystemTime>,
    ) -> Option<SqlFileMetadata> {
        let entries = self.entries.lock().ok()?;
        let entry = entries.get(path)?;
        (entry.byte_count == byte_count && entry.modified == modified)
            .then(|| entry.metadata.clone())
    }

    fn store(
        &self,
        path: &Path,
        byte_count: Option<u64>,
        modified: Option<SystemTime>,
        metadata: SqlFileMetadata,
    ) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.insert(
                path.to_path_buf(),
                CachedMetadata {
                    byte_count,
                    modified,
                    metadata,
                },
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Filter parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterPrefix {
    Object(SqlObjectKind),
    Schema,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFilter {
    pub prefix: Option<FilterPrefix>,
    pub text: String,
    pub inline_help: Option<PickerMessage>,
}

/// Parses a picker filter query. Recognized prefixes: `table:`/`t:`,
/// `view:`/`v:`, `trigger:`/`tr:`, `index:`/`i:`, `sproc:`/`proc:`/`p:`,
/// `function:`/`f:`, `type:`/`ty:`, `schema:`/`sch:`. A `"quoted"` query is
/// searched literally; unknown prefixes fall back to plain text. `s:` is
/// ambiguous and produces inline help instead of a filter.
pub fn parse_filter(raw: &str) -> ParsedFilter {
    let trimmed = raw.trim();
    let plain = |text: &str| ParsedFilter {
        prefix: None,
        text: text.to_string(),
        inline_help: None,
    };

    if trimmed.is_empty() {
        return plain("");
    }
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        return plain(&trimmed[1..trimmed.len() - 1]);
    }
    let Some((raw_prefix, remainder)) = trimmed.split_once(':') else {
        return plain(trimmed);
    };
    let raw_prefix = raw_prefix.trim().to_lowercase();
    if raw_prefix.is_empty() || raw_prefix.contains(char::is_whitespace) {
        return plain(trimmed);
    }
    if raw_prefix == "s" {
        return ParsedFilter {
            inline_help: Some(PickerMessage::new("project_picker.ambiguous_filter_help")),
            ..plain(trimmed)
        };
    }

    use SqlObjectKind::*;
    let prefix = match raw_prefix.as_str() {
        "table" | "t" => FilterPrefix::Object(Table),
        "view" | "v" => FilterPrefix::Object(View),
        "trigger" | "tr" => FilterPrefix::Object(Trigger),
        "index" | "i" => FilterPrefix::Object(Index),
        "sproc" | "proc" | "p" => FilterPrefix::Object(Sproc),
        "function" | "f" => FilterPrefix::Object(Function),
        "type" | "ty" => FilterPrefix::Object(Type),
        "schema" | "sch" => FilterPrefix::Schema,
        _ => return plain(trimmed),
    };
    ParsedFilter {
        prefix: Some(prefix),
        text: remainder.trim().to_string(),
        inline_help: None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FilterOutput {
    pub rows: Vec<PickerRow>,
    pub inline_help: Option<PickerMessage>,
    pub state_title: Option<PickerMessage>,
    pub state_detail: Vec<PickerMessage>,
}

/// Applies a filter query to scanned rows: prefix gating, per-term scoring
/// across name / object / schema / path, and score-descending ordering.
pub fn filter_rows(rows: &[PickerRow], query: &str, mode: PickerMode) -> FilterOutput {
    let parsed = parse_filter(query);
    if rows.is_empty() {
        return FilterOutput {
            rows: Vec::new(),
            inline_help: parsed.inline_help,
            ..Default::default()
        };
    }

    let text = parsed.text.trim();
    let mut scored: Vec<(PickerRow, i64)> = Vec::new();
    for row in rows {
        if !prefix_matches(parsed.prefix, row, text) {
            continue;
        }
        let (score, reason) = if text.is_empty() {
            let base = match mode {
                PickerMode::Folder => folder_base_score(row),
                PickerMode::Project => 0,
            };
            (Some(base), prefix_reason(parsed.prefix))
        } else if parsed.prefix == Some(FilterPrefix::Schema) {
            (
                schema_score(row, text),
                Some(PickerMessage::new("project_picker.match_schema")),
            )
        } else {
            (score_row(row, text), match_reason(row, text))
        };
        if let Some(score) = score {
            let mut row = row.clone();
            row.match_reason = reason;
            scored.push((row, score));
        }
    }

    scored.sort_by(|(a, sa), (b, sb)| {
        sb.cmp(sa).then_with(|| match mode {
            PickerMode::Folder => folder_order(a, b),
            PickerMode::Project => compare_ci(&a.relative_path, &b.relative_path),
        })
    });
    let rows: Vec<PickerRow> = scored.into_iter().map(|(row, _)| row).collect();
    let empty = rows.is_empty();
    FilterOutput {
        rows,
        inline_help: parsed.inline_help,
        state_title: empty.then(|| PickerMessage::new("project_picker.no_matches")),
        state_detail: if empty {
            vec![PickerMessage::new("project_picker.no_matches_detail")]
        } else {
            Vec::new()
        },
    }
}

fn prefix_matches(prefix: Option<FilterPrefix>, row: &PickerRow, text: &str) -> bool {
    match prefix {
        None => true,
        Some(FilterPrefix::Object(kind)) => row.metadata.kind == kind,
        Some(FilterPrefix::Schema) => {
            let Some(schema) = row.metadata.schema.as_deref().filter(|s| !s.is_empty()) else {
                return false;
            };
            match text.split_whitespace().next() {
                Some(term) => schema.to_lowercase().contains(&term.to_lowercase()),
                None => true,
            }
        }
    }
}

fn prefix_reason(prefix: Option<FilterPrefix>) -> Option<PickerMessage> {
    match prefix {
        None => None,
        Some(FilterPrefix::Object(kind)) => Some(PickerMessage::new(kind.localization_key())),
        Some(FilterPrefix::Schema) => Some(PickerMessage::new("project_picker.match_schema")),
    }
}

fn schema_score(row: &PickerRow, text: &str) -> Option<i64> {
    let schema = row.metadata.schema.as_deref()?.to_lowercase();
    if schema.is_empty() {
        return None;
    }
    let mut terms = text.split_whitespace().map(str::to_lowercase);
    let Some(first) = terms.next() else {
        return Some(folder_base_score(row));
    };
    let mut total = if schema == first {
        150
    } else if schema.starts_with(&first) {
        125
    } else if schema.contains(&first) {
        90
    } else {
        return None;
    };
    let remaining = terms.collect::<Vec<_>>().join(" ");
    if !remaining.is_empty() {
        total += score_row(row, &remaining)?;
    }
    Some(total + folder_base_score(row))
}

fn score_row(row: &PickerRow, text: &str) -> Option<i64> {
    // Keep the original case of each term: the IntelliSense matcher reads
    // CamelCase intent from it (so "PuOr" targets "PurchaseOrder"), which a
    // lowercased term would erase.
    let terms: Vec<&str> = text.split_whitespace().collect();
    if terms.is_empty() {
        return Some(folder_base_score(row));
    }
    let name = row.name.as_str();
    let object = row.metadata.object_name.as_deref().unwrap_or("");
    let schema_lc = row.metadata.schema.as_deref().unwrap_or("").to_lowercase();
    let path_lc = row.relative_path.to_lowercase();
    let mut total = 0;
    for term in &terms {
        // Fuzzy subsequence + CamelCase-initials match on the name fields —
        // the same scorer IntelliSense completion uses — taking the better of
        // the file/object name and the catalog object name. Falls back to a
        // plain contains-match on schema and path.
        let fuzzy = fuzzy_name_score(term, name, object);
        let term_lc = term.to_lowercase();
        total += if let Some(score) = fuzzy {
            // Lift name matches above the schema/path fallbacks below.
            100 + score
        } else if !schema_lc.is_empty() && schema_lc.starts_with(&term_lc) {
            75
        } else if !schema_lc.is_empty() && schema_lc.contains(&term_lc) {
            65
        } else if path_lc.contains(&term_lc) {
            35
        } else {
            return None;
        };
    }
    Some(total + folder_base_score(row))
}

/// Best IntelliSense fuzzy score of `term` against a row's name fields, or
/// `None` if it matches neither. Shared by scoring and the match-reason chip.
fn fuzzy_name_score(term: &str, name: &str, object: &str) -> Option<i64> {
    let name_score = sqlly_intel::intellisense::qualified_object_score(term, name);
    let object_score = (!object.is_empty())
        .then(|| sqlly_intel::intellisense::qualified_object_score(term, object))
        .flatten();
    name_score
        .into_iter()
        .chain(object_score)
        .max()
        .map(i64::from)
}

fn match_reason(row: &PickerRow, text: &str) -> Option<PickerMessage> {
    let term = text.split_whitespace().next().unwrap_or(text);
    if term.is_empty() {
        return None;
    }
    let object = row.metadata.object_name.as_deref().unwrap_or("");
    // Mirror score_row: a fuzzy name hit ("PuOr" → "PurchaseOrder") counts,
    // not just a substring.
    if sqlly_intel::intellisense::qualified_object_score(term, &row.name).is_some() {
        return Some(PickerMessage::new("project_picker.match_name"));
    }
    if !object.is_empty()
        && sqlly_intel::intellisense::qualified_object_score(term, object).is_some()
    {
        return Some(PickerMessage::new("project_picker.match_object"));
    }
    let term_lc = term.to_lowercase();
    if row
        .metadata
        .schema
        .as_deref()
        .is_some_and(|s| s.to_lowercase().contains(&term_lc))
    {
        return Some(PickerMessage::new("project_picker.match_schema"));
    }
    if row.metadata.kind.label().to_lowercase().contains(&term_lc) {
        return Some(PickerMessage::new(row.metadata.kind.localization_key()));
    }
    if row.relative_path.to_lowercase().contains(&term_lc) {
        return Some(PickerMessage::new("project_picker.match_path"));
    }
    None
}

fn folder_base_score(row: &PickerRow) -> i64 {
    match row.kind {
        PickerRowKind::ParentDirectory => 1_000,
        PickerRowKind::Directory => 100,
        PickerRowKind::File => {
            if row.is_sql_file() {
                20
            } else if row.source == RowSource::Model {
                // Data-model objects rank just below real SQL files.
                15
            } else {
                0
            }
        }
    }
}

fn folder_order(a: &PickerRow, b: &PickerRow) -> std::cmp::Ordering {
    fn rank(row: &PickerRow) -> u8 {
        match row.kind {
            PickerRowKind::ParentDirectory => 0,
            PickerRowKind::Directory => 1,
            PickerRowKind::File => 2,
        }
    }
    rank(a)
        .cmp(&rank(b))
        .then_with(|| compare_ci(&a.name, &b.name))
}

fn compare_ci(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase())
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Lists the direct children of `current` (Folder mode), with a `..` row when
/// above `root`.
pub fn folder_snapshot(
    root: &Path,
    current: &Path,
    options: ScanOptions,
    known_objects: &[KnownSchemaObject],
    cache: &MetadataCache,
) -> PickerSnapshot {
    let Ok(entries) = fs::read_dir(current) else {
        return PickerSnapshot {
            rows: Vec::new(),
            state_title: Some(PickerMessage::new("project_picker.folder_unavailable")),
            state_detail: vec![
                PickerMessage::new("project_picker.folder_unavailable_detail")
                    .with_arg("path", current.display().to_string()),
            ],
        };
    };

    let mut rows = Vec::new();
    if current != root
        && let Some(parent) = current.parent()
    {
        rows.push(PickerRow {
            path: parent.to_path_buf(),
            source: RowSource::Filesystem,
            kind: PickerRowKind::ParentDirectory,
            name: "..".to_string(),
            relative_path: "..".to_string(),
            metadata: SqlFileMetadata::UNKNOWN,
            byte_count: None,
            modified: None,
            match_reason: None,
        });
    }

    for entry in entries.flatten() {
        let path = entry.path();
        if !should_include(&path, options.include_hidden) {
            continue;
        }
        rows.push(make_row(&path, root, known_objects, cache));
    }

    rows.sort_by(folder_order);
    let empty = rows.is_empty();
    PickerSnapshot {
        rows,
        state_title: empty.then(|| PickerMessage::new("project_picker.folder_empty")),
        state_detail: if empty {
            vec![PickerMessage::new("project_picker.folder_empty_detail")]
        } else {
            Vec::new()
        },
    }
}

/// Recursively finds every `.sql` file under `root` (Project mode), honoring
/// `.gitignore`, skipping symlinks that escape the root, and stopping at
/// `max_project_files`. `should_cancel` is polled so a superseded scan can
/// abort early.
pub fn project_snapshot(
    root: &Path,
    options: ScanOptions,
    known_objects: &[KnownSchemaObject],
    cache: &MetadataCache,
    should_cancel: &dyn Fn() -> bool,
) -> PickerSnapshot {
    if !root.exists() {
        return PickerSnapshot {
            rows: Vec::new(),
            state_title: Some(PickerMessage::new(
                "project_picker.project_root_unavailable",
            )),
            state_detail: vec![PickerMessage::new(
                "project_picker.project_root_unavailable_detail",
            )],
        };
    }

    let ignores = GitignoreMatcher::load(root);
    let root_resolved = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut rows = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut skipped_unreadable = 0usize;
    let mut skipped_symlinks = 0usize;
    let mut reached_limit = false;

    'outer: while let Some(dir) = stack.pop() {
        if should_cancel() {
            break;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                skipped_unreadable += 1;
                continue;
            }
        };
        for entry in entries.flatten() {
            if should_cancel() {
                break 'outer;
            }
            let path = entry.path();
            if !should_include(&path, options.include_hidden) {
                continue;
            }
            if is_symlink_outside_root(&path, &root_resolved) {
                skipped_symlinks += 1;
                continue;
            }
            let is_directory = path.is_dir();
            if ignores.ignores(&path, is_directory)
                && !(is_directory && ignores.may_contain_negated_child(&path))
            {
                continue;
            }
            if is_directory {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
                && !ignores.ignores(&path, false)
            {
                rows.push(make_row(&path, root, known_objects, cache));
                if rows.len() >= options.max_project_files {
                    reached_limit = true;
                    break 'outer;
                }
            }
        }
    }

    rows.sort_by(|a, b| compare_ci(&a.relative_path, &b.relative_path));

    let mut details = Vec::new();
    if rows.is_empty() {
        details.push(PickerMessage::new("project_picker.project_empty_detail"));
    }
    if skipped_unreadable > 0 {
        let key = if skipped_unreadable == 1 {
            "project_picker.skipped_folder_singular"
        } else {
            "project_picker.skipped_folder_plural"
        };
        details.push(PickerMessage::new(key).with_arg("count", skipped_unreadable.to_string()));
    }
    if skipped_symlinks > 0 {
        let key = if skipped_symlinks == 1 {
            "project_picker.skipped_symlink_singular"
        } else {
            "project_picker.skipped_symlink_plural"
        };
        details.push(PickerMessage::new(key).with_arg("count", skipped_symlinks.to_string()));
    }
    if reached_limit {
        details.push(PickerMessage::new("project_picker.scan_limit"));
    }

    PickerSnapshot {
        state_title: rows.is_empty().then(|| {
            let root_name = root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| root.display().to_string());
            PickerMessage::new("project_picker.project_empty_title")
                .with_arg("root_name", root_name)
        }),
        state_detail: details,
        rows,
    }
}

fn make_row(
    path: &Path,
    root: &Path,
    known_objects: &[KnownSchemaObject],
    cache: &MetadataCache,
) -> PickerRow {
    let file_meta = fs::metadata(path).ok();
    let is_directory = file_meta.as_ref().is_some_and(|m| m.is_dir());
    let byte_count = file_meta
        .as_ref()
        .and_then(|m| (!m.is_dir()).then_some(m.len()));
    let modified = file_meta.as_ref().and_then(|m| m.modified().ok());

    let is_sql = !is_directory
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("sql"));
    let metadata = if is_sql {
        let scanned = cache.get(path, byte_count, modified).unwrap_or_else(|| {
            let scanned = read_bounded(path, PREVIEW_BYTE_LIMIT)
                .and_then(|(data, _)| String::from_utf8(data).ok())
                .map(|sql| scan_sql_metadata(&sql))
                .unwrap_or(SqlFileMetadata::UNKNOWN);
            cache.store(path, byte_count, modified, scanned.clone());
            scanned
        });
        apply_catalog_confidence(scanned, known_objects)
    } else {
        SqlFileMetadata::UNKNOWN
    };

    PickerRow {
        path: path.to_path_buf(),
        source: RowSource::Filesystem,
        kind: if is_directory {
            PickerRowKind::Directory
        } else {
            PickerRowKind::File
        },
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        relative_path: relative_path(path, root),
        metadata,
        byte_count,
        modified,
        match_reason: None,
    }
}

/// Upgrades scanned metadata to `Indexed` when the object also exists in the
/// live schema catalog (name + optional schema, matching kind).
fn apply_catalog_confidence(
    mut metadata: SqlFileMetadata,
    known_objects: &[KnownSchemaObject],
) -> SqlFileMetadata {
    let model_backed = metadata.object_name.as_deref().is_some_and(|object_name| {
        known_objects.iter().any(|object| {
            object.kind == metadata.kind
                && !metadata.kind.is_scan_only()
                && object.name.eq_ignore_ascii_case(object_name)
                && match (&metadata.schema, &object.schema) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    _ => true,
                }
        })
    });
    if model_backed {
        metadata.confidence = MetadataConfidence::Indexed;
    } else if metadata.kind != SqlObjectKind::Unknown {
        metadata.confidence = MetadataConfidence::Scanned;
    }
    metadata
}

fn should_include(path: &Path, include_hidden: bool) -> bool {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy()) else {
        return false;
    };
    if !include_hidden && name.starts_with('.') {
        return false;
    }
    !BUILT_IN_IGNORED_NAMES.contains(&name.as_ref())
}

fn is_symlink_outside_root(path: &Path, root_resolved: &Path) -> bool {
    let Ok(link_meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if !link_meta.file_type().is_symlink() {
        return false;
    }
    match path.canonicalize() {
        Ok(resolved) => !resolved.starts_with(root_resolved),
        Err(_) => true,
    }
}

fn relative_path(path: &Path, root: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel
            .components()
            .map(|component| component.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        _ => path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Preview loading
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewState {
    Empty,
    Loaded,
    Unavailable,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlPreview {
    pub state: PreviewState,
    pub title: String,
    pub detail: Vec<PreviewText>,
    pub text: String,
    pub is_truncated: bool,
}

/// Loads a bounded, line-limited preview of a `.sql` file.
pub fn load_preview(
    path: &Path,
    metadata: &SqlFileMetadata,
    relative_path: Option<&str>,
    max_bytes: usize,
    max_lines: usize,
) -> SqlPreview {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
    {
        return SqlPreview {
            state: PreviewState::Empty,
            title: name,
            detail: vec![PreviewText::Message(PickerMessage::new(
                "project_picker.preview_sql_only",
            ))],
            text: String::new(),
            is_truncated: false,
        };
    }
    if !path.exists() {
        return SqlPreview {
            state: PreviewState::Missing,
            title: String::new(),
            detail: vec![PreviewText::Value(path.display().to_string())],
            text: String::new(),
            is_truncated: false,
        };
    }

    let (data, byte_truncated) = match read_bounded(path, max_bytes) {
        Some(read) => read,
        None => {
            return SqlPreview {
                state: PreviewState::Unavailable,
                title: name,
                detail: vec![PreviewText::Message(PickerMessage::new(
                    "project_picker.preview_unreadable",
                ))],
                text: String::new(),
                is_truncated: false,
            };
        }
    };
    let Ok(raw_text) = String::from_utf8(data) else {
        return SqlPreview {
            state: PreviewState::Unavailable,
            title: name,
            detail: vec![PreviewText::Message(PickerMessage::new(
                "project_picker.preview_invalid_utf8",
            ))],
            text: String::new(),
            is_truncated: byte_truncated,
        };
    };

    let line_count = raw_text.split('\n').count();
    let line_truncated = line_count > max_lines;
    let text = if line_truncated {
        raw_text
            .split('\n')
            .take(max_lines)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        raw_text
    };
    let truncated = byte_truncated || line_truncated;

    let byte_count = fs::metadata(path).ok().map(|m| m.len());
    let mut details = Vec::new();
    if let Some(rel) = relative_path.filter(|r| !r.is_empty()) {
        details.push(PreviewText::Value(rel.to_string()));
    }
    if let Some(bytes) = byte_count {
        details.push(PreviewText::Value(format_bytes(bytes)));
    }
    if metadata.multiple_objects {
        details.push(PreviewText::Message(PickerMessage::new(
            "project_picker.preview_multiple_objects",
        )));
    }
    if truncated {
        details.push(PreviewText::Message(PickerMessage::new(
            "project_picker.preview_truncated",
        )));
    }

    SqlPreview {
        state: PreviewState::Loaded,
        title: name,
        detail: details,
        text,
        is_truncated: truncated,
    }
}

/// Reads at most `max_bytes` from a file; the flag reports whether more data
/// remained.
fn read_bounded(path: &Path, max_bytes: usize) -> Option<(Vec<u8>, bool)> {
    let file = fs::File::open(path).ok()?;
    if max_bytes == usize::MAX {
        let mut data = Vec::new();
        file.take(u64::MAX).read_to_end(&mut data).ok()?;
        return Some((data, false));
    }
    let mut data = Vec::new();
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut data)
        .ok()?;
    let truncated = data.len() > max_bytes;
    if truncated {
        data.truncate(max_bytes);
        // Avoid splitting a UTF-8 code point at the cut.
        while !data.is_empty() && data.last().is_some_and(|b| b & 0b1100_0000 == 0b1000_0000) {
            data.pop();
        }
    }
    Some((data, truncated))
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// ---------------------------------------------------------------------------
// SQL metadata scanner
// ---------------------------------------------------------------------------

/// Finds the first (and counts subsequent) top-level `CREATE`/`ALTER` object
/// in a SQL script. Handles `CREATE OR ALTER`, `UNIQUE`/`CLUSTERED`/
/// `NONCLUSTERED` index modifiers, bracketed / quoted identifiers, and infers
/// index/trigger schema from the `ON <target>` clause.
pub fn scan_sql_metadata(sql: &str) -> SqlFileMetadata {
    let tokens = tokenize_sql(sql);
    let mut matches: Vec<SqlFileMetadata> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let verb = tokens[i].clean_upper();
        if verb != "CREATE" && verb != "ALTER" {
            i += 1;
            continue;
        }

        let mut kind_index = i + 1;
        if verb == "CREATE"
            && kind_index + 1 < tokens.len()
            && tokens[kind_index].clean_upper() == "OR"
            && tokens[kind_index + 1].clean_upper() == "ALTER"
        {
            kind_index += 2;
        }
        while kind_index < tokens.len()
            && matches!(
                tokens[kind_index].clean_upper().as_str(),
                "UNIQUE" | "CLUSTERED" | "NONCLUSTERED"
            )
        {
            kind_index += 1;
        }

        let Some(kind) = tokens
            .get(kind_index)
            .and_then(|t| object_kind_for_token(&t.clean_upper()))
        else {
            i += 1;
            continue;
        };

        let identifier = parse_multipart_identifier(&tokens, kind_index + 1);
        let metadata = match identifier {
            Some((parts, _)) => {
                let mut schema = schema_part(&parts);
                let object_name = parts.last().cloned();
                if matches!(kind, SqlObjectKind::Index | SqlObjectKind::Trigger)
                    && schema.is_none()
                    && let Some(on_index) = (kind_index + 1..tokens.len())
                        .find(|&idx| tokens[idx].clean_upper() == "ON")
                    && let Some((target_parts, _)) =
                        parse_multipart_identifier(&tokens, on_index + 1)
                {
                    schema = schema_part(&target_parts);
                }
                SqlFileMetadata {
                    schema,
                    object_name,
                    kind,
                    confidence: MetadataConfidence::Scanned,
                    multiple_objects: false,
                }
            }
            None => SqlFileMetadata {
                schema: None,
                object_name: None,
                kind,
                confidence: MetadataConfidence::Scanned,
                multiple_objects: false,
            },
        };
        matches.push(metadata);
        i = kind_index + 1;
    }

    let count = matches.len();
    match matches.into_iter().next() {
        Some(mut first) => {
            first.multiple_objects = count > 1;
            first
        }
        None => SqlFileMetadata::UNKNOWN,
    }
}

fn object_kind_for_token(token: &str) -> Option<SqlObjectKind> {
    match token {
        "TABLE" => Some(SqlObjectKind::Table),
        "VIEW" => Some(SqlObjectKind::View),
        "TRIGGER" => Some(SqlObjectKind::Trigger),
        "INDEX" => Some(SqlObjectKind::Index),
        "PROCEDURE" | "PROC" => Some(SqlObjectKind::Sproc),
        "FUNCTION" => Some(SqlObjectKind::Function),
        "TYPE" => Some(SqlObjectKind::Type),
        _ => None,
    }
}

fn parse_multipart_identifier(tokens: &[SqlToken], start: usize) -> Option<(Vec<String>, usize)> {
    let first = tokens.get(start)?;
    if !first.is_identifier_like() {
        return None;
    }
    let mut parts = vec![first.clean()];
    let mut idx = start + 1;
    while idx + 1 < tokens.len() && tokens[idx].text == "." && tokens[idx + 1].is_identifier_like()
    {
        parts.push(tokens[idx + 1].clean());
        idx += 2;
    }
    parts.retain(|p| !p.is_empty());
    Some((parts, idx - 1))
}

fn schema_part(parts: &[String]) -> Option<String> {
    (parts.len() >= 2).then(|| parts[parts.len() - 2].clone())
}

struct SqlToken {
    text: String,
}

impl SqlToken {
    /// Strips `[bracket]` / `"quote"` delimiters and unescapes doubles.
    fn clean(&self) -> String {
        let t = &self.text;
        if t.starts_with('[') && t.ends_with(']') && t.len() >= 2 {
            t[1..t.len() - 1].replace("]]", "]")
        } else if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
            t[1..t.len() - 1].replace("\"\"", "\"")
        } else {
            t.clone()
        }
    }

    fn clean_upper(&self) -> String {
        self.clean().to_uppercase()
    }

    fn is_identifier_like(&self) -> bool {
        self.text
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_' || c == '#' || c == '[' || c == '"')
    }
}

/// Tokenizes SQL just enough for object-header scanning: skips whitespace,
/// comments, and string literals; keeps identifiers (including bracketed and
/// quoted) and `.`/`(`/`)`/`,` punctuation.
fn tokenize_sql(sql: &str) -> Vec<SqlToken> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;

    let is_ident_start = |c: char| c.is_alphabetic() || c == '_' || c == '#' || c == '@';
    let is_ident_continue =
        |c: char| c.is_alphanumeric() || c == '_' || c == '#' || c == '@' || c == '$';

    while i < chars.len() {
        let start = i;
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
        } else if c == '\'' {
            i += 1;
            while i < chars.len() {
                if chars[i] == '\'' {
                    i += 1;
                    if chars.get(i) != Some(&'\'') {
                        break;
                    }
                }
                i += 1;
            }
        } else if c == '[' || c == '"' {
            let close = if c == '[' { ']' } else { '"' };
            i += 1;
            while i < chars.len() {
                if chars[i] == close {
                    i += 1;
                    if chars.get(i) != Some(&close) {
                        break;
                    }
                }
                i += 1;
            }
            tokens.push(SqlToken {
                text: chars[start..i].iter().collect(),
            });
        } else if is_ident_start(c) {
            i += 1;
            while i < chars.len() && is_ident_continue(chars[i]) {
                i += 1;
            }
            tokens.push(SqlToken {
                text: chars[start..i].iter().collect(),
            });
        } else if c == '.' || c == '(' || c == ')' || c == ',' {
            i += 1;
            tokens.push(SqlToken {
                text: c.to_string(),
            });
        } else {
            i += 1;
        }
    }
    tokens
}

// ---------------------------------------------------------------------------
// Gitignore matching
// ---------------------------------------------------------------------------

/// Minimal `.gitignore` matcher: root-level file only, `*` wildcards,
/// trailing-slash directory rules, and `!` negations.
pub struct GitignoreMatcher {
    root: PathBuf,
    rules: Vec<GitignoreRule>,
}

struct GitignoreRule {
    pattern: String,
    is_negated: bool,
}

impl GitignoreMatcher {
    pub fn load(root: &Path) -> Self {
        let text = fs::read_to_string(root.join(".gitignore")).unwrap_or_default();
        Self::from_patterns(root, text.lines())
    }

    pub fn from_patterns<'a>(root: &Path, patterns: impl IntoIterator<Item = &'a str>) -> Self {
        let rules = patterns
            .into_iter()
            .filter_map(|raw| {
                let trimmed = raw.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    return None;
                }
                if let Some(rest) = trimmed.strip_prefix('!') {
                    let pattern = rest.trim().to_string();
                    return (!pattern.is_empty()).then_some(GitignoreRule {
                        pattern,
                        is_negated: true,
                    });
                }
                Some(GitignoreRule {
                    pattern: trimmed.to_string(),
                    is_negated: false,
                })
            })
            .collect();
        Self {
            root: root.to_path_buf(),
            rules,
        }
    }

    pub fn ignores(&self, path: &Path, is_directory: bool) -> bool {
        let relative = relative_path(path, &self.root);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut ignored = false;
        for rule in &self.rules {
            if Self::rule_matches(rule, &relative, &name, is_directory) {
                ignored = !rule.is_negated;
            }
        }
        ignored
    }

    /// Whether an ignored directory could still contain a negated
    /// (re-included) child, so the scan must descend into it anyway.
    pub fn may_contain_negated_child(&self, path: &Path) -> bool {
        let relative = relative_path(path, &self.root);
        if relative.is_empty() {
            return true;
        }
        self.rules.iter().any(|rule| {
            rule.is_negated && {
                let normalized = rule.pattern.trim_matches('/');
                !normalized.contains('/') || normalized.starts_with(&format!("{relative}/"))
            }
        })
    }

    fn rule_matches(rule: &GitignoreRule, relative: &str, name: &str, is_directory: bool) -> bool {
        let normalized = rule.pattern.trim_matches('/');
        if normalized.is_empty() {
            return false;
        }
        if rule.pattern.ends_with('/') {
            if is_directory && wildcard_matches(normalized, name) {
                return true;
            }
            return relative == normalized || relative.starts_with(&format!("{normalized}/"));
        }
        if rule.pattern.contains('/') {
            if wildcard_matches(normalized, relative) {
                return true;
            }
            return is_directory
                && (relative == normalized || relative.starts_with(&format!("{normalized}/")));
        }
        wildcard_matches(normalized, name)
            || relative.split('/').any(|segment| segment == normalized)
    }
}

/// Glob match supporting `*` only (as the Swift matcher does).
fn wildcard_matches(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    let segments: Vec<&str> = pattern.split('*').collect();
    let mut rest = value;
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            continue;
        }
        if index == 0 {
            let Some(after) = rest.strip_prefix(segment) else {
                return false;
            };
            rest = after;
        } else if index == segments.len() - 1 {
            return rest.ends_with(segment);
        } else {
            let Some(found) = rest.find(segment) else {
                return false;
            };
            rest = &rest[found + segment.len()..];
        }
    }
    // A trailing `*` (or all-segment consumption) matches any remainder.
    segments.last().is_some_and(|s| s.is_empty()) || rest.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_messages_keep_keys_and_arguments_typed() {
        let message = PickerMessage::new("project_picker.folder_unavailable_detail")
            .with_arg("path", "/tmp/project");
        assert_eq!(message.key, "project_picker.folder_unavailable_detail");
        assert_eq!(message.args, vec![("path", "/tmp/project".to_string())]);
    }

    fn sql_row(name: &str, relative: &str, metadata: SqlFileMetadata) -> PickerRow {
        PickerRow {
            path: PathBuf::from(format!("/proj/{relative}")),
            source: RowSource::Filesystem,
            kind: PickerRowKind::File,
            name: name.to_string(),
            relative_path: relative.to_string(),
            metadata,
            byte_count: Some(100),
            modified: None,
            match_reason: None,
        }
    }

    fn metadata(schema: &str, object: &str, kind: SqlObjectKind) -> SqlFileMetadata {
        SqlFileMetadata {
            schema: Some(schema.to_string()),
            object_name: Some(object.to_string()),
            kind,
            confidence: MetadataConfidence::Scanned,
            multiple_objects: false,
        }
    }

    // ---- filter parsing -------------------------------------------------

    #[test]
    fn parse_filter_empty_and_plain() {
        assert_eq!(parse_filter("").text, "");
        let plain = parse_filter("orders");
        assert_eq!(plain.prefix, None);
        assert_eq!(plain.text, "orders");
    }

    #[test]
    fn parse_filter_object_prefixes() {
        for (query, kind) in [
            ("t:x", SqlObjectKind::Table),
            ("view:x", SqlObjectKind::View),
            ("tr:x", SqlObjectKind::Trigger),
            ("i:x", SqlObjectKind::Index),
            ("p:x", SqlObjectKind::Sproc),
            ("proc:x", SqlObjectKind::Sproc),
            ("f:x", SqlObjectKind::Function),
            ("ty:x", SqlObjectKind::Type),
        ] {
            assert_eq!(
                parse_filter(query).prefix,
                Some(FilterPrefix::Object(kind)),
                "query {query}"
            );
        }
        assert_eq!(
            parse_filter("schema:dbo").prefix,
            Some(FilterPrefix::Schema)
        );
        assert_eq!(parse_filter("sch:dbo").prefix, Some(FilterPrefix::Schema));
    }

    #[test]
    fn parse_filter_ambiguous_s_gets_help() {
        let parsed = parse_filter("s:foo");
        assert_eq!(parsed.prefix, None);
        assert!(parsed.inline_help.is_some());
        assert_eq!(parsed.text, "s:foo");
    }

    #[test]
    fn parse_filter_quoted_is_literal() {
        let parsed = parse_filter("\"v: literal\"");
        assert_eq!(parsed.prefix, None);
        assert_eq!(parsed.text, "v: literal");
    }

    #[test]
    fn parse_filter_unknown_prefix_is_plain_text() {
        let parsed = parse_filter("foo:bar");
        assert_eq!(parsed.prefix, None);
        assert_eq!(parsed.text, "foo:bar");
    }

    // ---- filtering / scoring --------------------------------------------

    #[test]
    fn filter_by_object_kind_prefix() {
        let rows = vec![
            sql_row(
                "a.sql",
                "a.sql",
                metadata("dbo", "OrdersView", SqlObjectKind::View),
            ),
            sql_row(
                "b.sql",
                "b.sql",
                metadata("dbo", "Orders", SqlObjectKind::Table),
            ),
        ];
        let output = filter_rows(&rows, "v:", PickerMode::Project);
        assert_eq!(output.rows.len(), 1);
        assert_eq!(output.rows[0].name, "a.sql");
        assert_eq!(
            output.rows[0]
                .match_reason
                .as_ref()
                .map(|reason| reason.key),
            Some("project_picker.kind_view")
        );
    }

    #[test]
    fn filter_schema_prefix_matches_and_excludes() {
        let rows = vec![
            sql_row(
                "a.sql",
                "a.sql",
                metadata("HR", "Employee", SqlObjectKind::Table),
            ),
            sql_row(
                "b.sql",
                "b.sql",
                metadata("dbo", "Orders", SqlObjectKind::Table),
            ),
        ];
        let output = filter_rows(&rows, "schema:hr", PickerMode::Project);
        assert_eq!(output.rows.len(), 1);
        assert_eq!(output.rows[0].name, "a.sql");
    }

    #[test]
    fn filter_name_match_outranks_path_match() {
        let rows = vec![
            sql_row(
                "orders.sql",
                "sub/orders.sql",
                metadata("dbo", "Orders", SqlObjectKind::Table),
            ),
            sql_row(
                "misc.sql",
                "orders/misc.sql",
                metadata("dbo", "Misc", SqlObjectKind::Table),
            ),
        ];
        let output = filter_rows(&rows, "orders", PickerMode::Project);
        assert_eq!(output.rows.len(), 2);
        assert_eq!(output.rows[0].name, "orders.sql");
        assert_eq!(
            output.rows[0]
                .match_reason
                .as_ref()
                .map(|reason| reason.key),
            Some("project_picker.match_name")
        );
        assert_eq!(
            output.rows[1]
                .match_reason
                .as_ref()
                .map(|reason| reason.key),
            Some("project_picker.match_path")
        );
    }

    #[test]
    fn filter_all_terms_must_match() {
        let rows = vec![sql_row(
            "orders.sql",
            "orders.sql",
            metadata("dbo", "Orders", SqlObjectKind::Table),
        )];
        assert!(
            filter_rows(&rows, "orders zzz", PickerMode::Project)
                .rows
                .is_empty()
        );
    }

    #[test]
    fn filter_camelcase_initials_match_like_intellisense() {
        // "PuOr" should find "PurchaseOrder" the way IntelliSense completion
        // does — CamelCase initials, not a plain substring.
        let rows = vec![
            sql_row(
                "purchase_order.sql",
                "purchase_order.sql",
                metadata("dbo", "PurchaseOrder", SqlObjectKind::Table),
            ),
            sql_row(
                "customer.sql",
                "customer.sql",
                metadata("dbo", "Customer", SqlObjectKind::Table),
            ),
        ];
        let output = filter_rows(&rows, "PuOr", PickerMode::Project);
        assert_eq!(output.rows.len(), 1);
        assert_eq!(
            output.rows[0].metadata.object_name.as_deref(),
            Some("PurchaseOrder")
        );
        // Lowercase subsequence intent also works.
        assert_eq!(
            filter_rows(&rows, "puror", PickerMode::Project).rows.len(),
            1
        );
    }

    #[test]
    fn filter_empty_returns_all() {
        let rows = vec![
            sql_row("a.sql", "a.sql", SqlFileMetadata::UNKNOWN),
            sql_row("b.sql", "b.sql", SqlFileMetadata::UNKNOWN),
        ];
        assert_eq!(filter_rows(&rows, "", PickerMode::Project).rows.len(), 2);
    }

    // ---- SQL metadata scanner -------------------------------------------

    #[test]
    fn scan_create_procedure_with_schema() {
        let m = scan_sql_metadata("CREATE PROCEDURE [HR].[pEmployee_Fetch] AS BEGIN SELECT 1 END");
        assert_eq!(m.kind, SqlObjectKind::Sproc);
        assert_eq!(m.schema.as_deref(), Some("HR"));
        assert_eq!(m.object_name.as_deref(), Some("pEmployee_Fetch"));
        assert!(!m.multiple_objects);
    }

    #[test]
    fn scan_create_or_alter_view() {
        let m = scan_sql_metadata("CREATE OR ALTER VIEW dbo.vOrders AS SELECT 1 AS x");
        assert_eq!(m.kind, SqlObjectKind::View);
        assert_eq!(m.schema.as_deref(), Some("dbo"));
        assert_eq!(m.object_name.as_deref(), Some("vOrders"));
    }

    #[test]
    fn scan_index_takes_schema_from_on_clause() {
        let m = scan_sql_metadata("CREATE NONCLUSTERED INDEX IX_Orders ON Sales.Orders (Id)");
        assert_eq!(m.kind, SqlObjectKind::Index);
        assert_eq!(m.schema.as_deref(), Some("Sales"));
        assert_eq!(m.object_name.as_deref(), Some("IX_Orders"));
    }

    #[test]
    fn scan_skips_comments_and_strings() {
        let m = scan_sql_metadata(
            "-- CREATE TABLE dbo.NotThis\n/* CREATE VIEW x */\nSELECT 'CREATE PROC y'\nCREATE TABLE dbo.Real (Id INT)",
        );
        assert_eq!(m.kind, SqlObjectKind::Table);
        assert_eq!(m.object_name.as_deref(), Some("Real"));
    }

    #[test]
    fn scan_flags_multiple_objects() {
        let m = scan_sql_metadata(
            "CREATE TABLE dbo.A (Id INT)\nGO\nCREATE VIEW dbo.B AS SELECT 1 AS x",
        );
        assert_eq!(m.kind, SqlObjectKind::Table);
        assert!(m.multiple_objects);
    }

    #[test]
    fn scan_plain_select_is_unknown() {
        assert_eq!(
            scan_sql_metadata("SELECT * FROM dbo.Orders"),
            SqlFileMetadata::UNKNOWN
        );
    }

    // ---- data-model rows ---------------------------------------------------

    #[test]
    fn model_object_rows_are_openable_and_sorted() {
        let rows = model_object_rows(&[
            KnownSchemaObject {
                schema: Some("Sales".to_string()),
                name: "Orders".to_string(),
                kind: SqlObjectKind::Table,
            },
            KnownSchemaObject {
                schema: Some("dbo".to_string()),
                name: "pFetch".to_string(),
                kind: SqlObjectKind::Sproc,
            },
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "dbo.pFetch");
        assert_eq!(rows[1].name, "Sales.Orders");
        assert!(rows.iter().all(|r| r.is_openable() && !r.is_sql_file()));
        assert!(
            rows.iter()
                .all(|r| r.metadata.confidence == MetadataConfidence::Indexed)
        );
    }

    #[test]
    fn model_rows_respect_kind_prefix_filter() {
        let rows = model_object_rows(&[
            KnownSchemaObject {
                schema: Some("dbo".to_string()),
                name: "Orders".to_string(),
                kind: SqlObjectKind::Table,
            },
            KnownSchemaObject {
                schema: Some("dbo".to_string()),
                name: "pFetch".to_string(),
                kind: SqlObjectKind::Sproc,
            },
        ]);
        let output = filter_rows(&rows, "p:fetch", PickerMode::Project);
        assert_eq!(output.rows.len(), 1);
        assert_eq!(
            output.rows[0].metadata.object_name.as_deref(),
            Some("pFetch")
        );
    }

    // ---- catalog confidence ----------------------------------------------

    #[test]
    fn catalog_match_upgrades_to_indexed() {
        let known = vec![KnownSchemaObject {
            schema: Some("dbo".to_string()),
            name: "Orders".to_string(),
            kind: SqlObjectKind::Table,
        }];
        let upgraded =
            apply_catalog_confidence(metadata("DBO", "orders", SqlObjectKind::Table), &known);
        assert_eq!(upgraded.confidence, MetadataConfidence::Indexed);
        let unrelated =
            apply_catalog_confidence(metadata("dbo", "Other", SqlObjectKind::Table), &known);
        assert_eq!(unrelated.confidence, MetadataConfidence::Scanned);
    }

    // ---- gitignore --------------------------------------------------------

    #[test]
    fn gitignore_basic_rules() {
        let root = Path::new("/proj");
        let m = GitignoreMatcher::from_patterns(root, ["*.bak", "logs/", "docs/tmp"]);
        assert!(m.ignores(Path::new("/proj/query.bak"), false));
        assert!(!m.ignores(Path::new("/proj/query.sql"), false));
        assert!(m.ignores(Path::new("/proj/logs"), true));
        assert!(m.ignores(Path::new("/proj/logs/inner.sql"), false));
        assert!(m.ignores(Path::new("/proj/docs/tmp"), true));
    }

    #[test]
    fn gitignore_negation_reincludes() {
        let root = Path::new("/proj");
        let m = GitignoreMatcher::from_patterns(root, ["*.sql", "!keep.sql"]);
        assert!(m.ignores(Path::new("/proj/drop.sql"), false));
        assert!(!m.ignores(Path::new("/proj/keep.sql"), false));
    }

    #[test]
    fn wildcard_edges() {
        assert!(wildcard_matches("*.sql", "a.sql"));
        assert!(!wildcard_matches("*.sql", "a.sqlx"));
        assert!(wildcard_matches("temp*", "temp123"));
        assert!(wildcard_matches("a*b*c", "aXbYc"));
        assert!(!wildcard_matches("a*b*c", "aXbY"));
    }

    // ---- filesystem scans --------------------------------------------------

    fn make_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(root.join("top.sql"), "CREATE TABLE dbo.Top (Id INT)").unwrap();
        fs::write(root.join("readme.md"), "# hi").unwrap();
        fs::write(
            root.join("sub/nested.sql"),
            "CREATE VIEW hr.vNested AS SELECT 1 AS x",
        )
        .unwrap();
        fs::write(
            root.join("node_modules/skip.sql"),
            "CREATE TABLE dbo.Skip (Id INT)",
        )
        .unwrap();
        dir
    }

    #[test]
    fn folder_snapshot_lists_children_sorted() {
        let dir = make_tree();
        let cache = MetadataCache::default();
        let snapshot = folder_snapshot(dir.path(), dir.path(), ScanOptions::default(), &[], &cache);
        let names: Vec<&str> = snapshot.rows.iter().map(|r| r.name.as_str()).collect();
        // Directories first, then files, case-insensitive; node_modules excluded.
        assert_eq!(names, vec!["sub", "readme.md", "top.sql"]);
        assert!(
            snapshot
                .rows
                .iter()
                .all(|r| r.kind != PickerRowKind::ParentDirectory)
        );
    }

    #[test]
    fn folder_snapshot_adds_parent_row_below_root() {
        let dir = make_tree();
        let cache = MetadataCache::default();
        let snapshot = folder_snapshot(
            dir.path(),
            &dir.path().join("sub"),
            ScanOptions::default(),
            &[],
            &cache,
        );
        assert_eq!(snapshot.rows[0].kind, PickerRowKind::ParentDirectory);
        assert_eq!(snapshot.rows[0].name, "..");
    }

    #[test]
    fn project_snapshot_finds_sql_recursively() {
        let dir = make_tree();
        let cache = MetadataCache::default();
        let snapshot = project_snapshot(dir.path(), ScanOptions::default(), &[], &cache, &|| false);
        let paths: Vec<&str> = snapshot
            .rows
            .iter()
            .map(|r| r.relative_path.as_str())
            .collect();
        assert_eq!(paths, vec!["sub/nested.sql", "top.sql"]);
        let nested = &snapshot.rows[0];
        assert_eq!(nested.metadata.kind, SqlObjectKind::View);
        assert_eq!(nested.metadata.schema.as_deref(), Some("hr"));
    }

    #[test]
    fn project_snapshot_respects_gitignore() {
        let dir = make_tree();
        fs::write(dir.path().join(".gitignore"), "sub/\n").unwrap();
        let cache = MetadataCache::default();
        let snapshot = project_snapshot(dir.path(), ScanOptions::default(), &[], &cache, &|| false);
        let paths: Vec<&str> = snapshot
            .rows
            .iter()
            .map(|r| r.relative_path.as_str())
            .collect();
        assert_eq!(paths, vec!["top.sql"]);
    }

    #[test]
    fn project_snapshot_cancellation_stops_early() {
        let dir = make_tree();
        let cache = MetadataCache::default();
        let snapshot = project_snapshot(dir.path(), ScanOptions::default(), &[], &cache, &|| true);
        assert!(snapshot.rows.is_empty());
    }

    // ---- preview -----------------------------------------------------------

    #[test]
    fn preview_loads_and_reports_detail() {
        let dir = make_tree();
        let path = dir.path().join("top.sql");
        let preview = load_preview(
            &path,
            &SqlFileMetadata::UNKNOWN,
            Some("top.sql"),
            PREVIEW_BYTE_LIMIT,
            PREVIEW_LINE_LIMIT,
        );
        assert_eq!(preview.state, PreviewState::Loaded);
        assert!(preview.text.contains("CREATE TABLE"));
        assert!(
            preview.detail.iter().any(|detail| {
                matches!(detail, PreviewText::Value(value) if value == "top.sql")
            })
        );
        assert!(!preview.is_truncated);
    }

    #[test]
    fn preview_line_truncation() {
        let dir = make_tree();
        let path = dir.path().join("many.sql");
        fs::write(&path, "SELECT 1\n".repeat(50)).unwrap();
        let preview = load_preview(
            &path,
            &SqlFileMetadata::UNKNOWN,
            None,
            PREVIEW_BYTE_LIMIT,
            10,
        );
        assert!(preview.is_truncated);
        assert_eq!(preview.text.split('\n').count(), 10);
    }

    #[test]
    fn preview_missing_file() {
        let preview = load_preview(
            Path::new("/nonexistent/x.sql"),
            &SqlFileMetadata::UNKNOWN,
            None,
            PREVIEW_BYTE_LIMIT,
            PREVIEW_LINE_LIMIT,
        );
        assert_eq!(preview.state, PreviewState::Missing);
    }

    #[test]
    fn preview_non_sql_file() {
        let dir = make_tree();
        let preview = load_preview(
            &dir.path().join("readme.md"),
            &SqlFileMetadata::UNKNOWN,
            None,
            PREVIEW_BYTE_LIMIT,
            PREVIEW_LINE_LIMIT,
        );
        assert_eq!(preview.state, PreviewState::Empty);
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
