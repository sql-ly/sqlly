//! Pure SQL Library model: front-matter, folder scan, pin export, unique names.
//!
//! Desktop-only (the library lives on the local filesystem). Copied skip-list
//! names from `project_file_picker_model` rather than changing that module's
//! visibility.

#![cfg(not(target_arch = "wasm32"))]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::project_file_picker_model::PREVIEW_BYTE_LIMIT;
use crate::query_history::HistoryEntry;

pub const LIBRARY_DIR_NAME: &str = "library";
pub const FRONT_MATTER_PREFIX: &str = "-- sqlly:";

/// Directory names never scanned, matching `project_file_picker_model`.
const SKIP_DIR_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "dist",
    "build",
    ".next",
    "target",
    "bin",
    "obj",
];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrontMatter {
    pub connection: Option<String>,
    pub database: Option<String>,
}

pub fn parse_front_matter(text: &str) -> FrontMatter {
    let first = text.lines().next().unwrap_or("").trim();
    let Some(rest) = first.strip_prefix(FRONT_MATTER_PREFIX) else {
        return FrontMatter::default();
    };
    parse_pairs(rest)
}

fn parse_pairs(input: &str) -> FrontMatter {
    let mut fm = FrontMatter::default();
    let mut rest = input.trim();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else {
            break;
        };
        let key = rest[..eq].trim();
        rest = rest[eq + 1..].trim_start();
        let value = if let Some(stripped) = rest.strip_prefix('"') {
            if let Some(end) = stripped.find('"') {
                let v = stripped[..end].to_string();
                rest = stripped[end + 1..].trim_start();
                v
            } else {
                let v = stripped.to_string();
                rest = "";
                v
            }
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let v = rest[..end].to_string();
            rest = rest[end..].trim_start();
            v
        };
        match key {
            "connection" if !value.is_empty() => fm.connection = Some(value),
            "database" if !value.is_empty() => fm.database = Some(value),
            _ => {}
        }
    }
    fm
}

pub fn render_front_matter(fm: &FrontMatter) -> Option<String> {
    if fm.connection.is_none() && fm.database.is_none() {
        return None;
    }
    let mut bits = Vec::new();
    if let Some(c) = &fm.connection {
        bits.push(format!("connection={}", quote_if_needed(c)));
    }
    if let Some(d) = &fm.database {
        bits.push(format!("database={}", quote_if_needed(d)));
    }
    Some(format!("{} {}", FRONT_MATTER_PREFIX, bits.join(" ")))
}

fn quote_if_needed(value: &str) -> String {
    if value.is_empty()
        || value.contains(char::is_whitespace)
        || value.contains('=')
        || value.contains('"')
    {
        format!("\"{value}\"")
    } else {
        value.to_string()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryEntry {
    pub path: PathBuf,
    pub relative: PathBuf,
    pub name: String,
    pub folder: PathBuf,
    pub modified: Option<SystemTime>,
    pub front_matter: FrontMatter,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryTree {
    pub folders: Vec<PathBuf>,
    pub entries: Vec<LibraryEntry>,
}

pub fn default_library_root() -> PathBuf {
    crate::query_history::data_file(LIBRARY_DIR_NAME)
}

pub fn scan_library(root: &Path) -> std::io::Result<LibraryTree> {
    let mut folders = Vec::new();
    let mut entries = Vec::new();
    if !root.exists() {
        return Ok(LibraryTree::default());
    }
    walk(root, root, &mut folders, &mut entries)?;
    folders.sort();
    folders.dedup();
    entries.sort_by(|a, b| a.folder.cmp(&b.folder).then(a.name.cmp(&b.name)));
    Ok(LibraryTree { folders, entries })
}

fn walk(
    root: &Path,
    dir: &Path,
    folders: &mut Vec<PathBuf>,
    entries: &mut Vec<LibraryEntry>,
) -> std::io::Result<()> {
    let reader = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for item in reader {
        let item = item?;
        let path = item.path();
        let name = item.file_name();
        let name_str = name.to_string_lossy();
        let file_type = item.file_type()?;
        if file_type.is_dir() {
            if should_skip_dir(&name_str) {
                continue;
            }
            let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            folders.push(relative);
            walk(root, &path, folders, entries)?;
            continue;
        }
        if !file_type.is_file() || !is_sql_file(&path) {
            continue;
        }
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let folder = relative
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "query".to_string());
        let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
        entries.push(LibraryEntry {
            path: path.clone(),
            relative,
            name: stem,
            folder,
            modified,
            front_matter: read_front_matter(&path),
        });
    }
    Ok(())
}

fn should_skip_dir(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIR_NAMES.iter().any(|s| s.eq_ignore_ascii_case(name))
}

fn is_sql_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
}

fn read_front_matter(path: &Path) -> FrontMatter {
    let mut file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return FrontMatter::default(),
    };
    let mut buf = vec![0u8; PREVIEW_BYTE_LIMIT];
    let n = file.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    parse_front_matter(&String::from_utf8_lossy(&buf))
}

pub fn filter_tree(tree: &LibraryTree, needle: &str) -> LibraryTree {
    let needle = needle.trim();
    if needle.is_empty() {
        return tree.clone();
    }
    let n = needle.to_lowercase();
    let entries: Vec<LibraryEntry> = tree
        .entries
        .iter()
        .filter(|e| e.relative.to_string_lossy().to_lowercase().contains(&n))
        .cloned()
        .collect();
    let mut folders: Vec<PathBuf> = tree
        .folders
        .iter()
        .filter(|f| {
            f.to_string_lossy().to_lowercase().contains(&n)
                || entries
                    .iter()
                    .any(|e| path_is_prefix(f, &e.folder) || e.folder == **f)
        })
        .cloned()
        .collect();
    for entry in &entries {
        let mut cursor = entry.folder.clone();
        while !cursor.as_os_str().is_empty() {
            if !folders.iter().any(|f| f == &cursor) {
                folders.push(cursor.clone());
            }
            cursor = cursor
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_default();
        }
    }
    folders.sort();
    folders.dedup();
    LibraryTree { folders, entries }
}

fn path_is_prefix(prefix: &Path, path: &Path) -> bool {
    path.starts_with(prefix)
}

pub fn unique_file_name(dir: &Path, stem: &str) -> PathBuf {
    let stem = sanitize_stem(stem);
    let candidate = dir.join(format!("{stem}.sql"));
    if !candidate.exists() {
        return candidate;
    }
    let mut n = 2u32;
    loop {
        let candidate = dir.join(format!("{stem} ({n}).sql"));
        if !candidate.exists() {
            return candidate;
        }
        n = n.saturating_add(1);
        if n == u32::MAX {
            return dir.join(format!("{stem} ({n}).sql"));
        }
    }
}

pub fn sanitize_stem(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| *c != '/' && *c != '\\' && *c != '\0')
        .collect();
    let truncated: String = cleaned.trim().chars().take(120).collect();
    let trimmed = truncated.trim();
    if trimmed.is_empty() {
        "query".to_string()
    } else {
        trimmed.to_string()
    }
}

pub fn pin_to_library_file(entry: &HistoryEntry) -> (String, String) {
    let stem = if !entry.name.trim().is_empty() {
        sanitize_stem(&entry.name)
    } else {
        let first: String = entry
            .sql
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(40)
            .collect();
        sanitize_stem(&first)
    };
    let fm = FrontMatter {
        connection: Some(entry.server.clone()).filter(|s| !s.is_empty()),
        database: entry.database.clone().filter(|s| !s.is_empty()),
    };
    let mut contents = String::new();
    if let Some(line) = render_front_matter(&fm) {
        contents.push_str(&line);
        contents.push('\n');
    }
    contents.push_str(&entry.sql);
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    (stem, contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_history::{HistoryOutcome, HistoryProvenance};

    fn pin(name: &str, sql: &str, server: &str, database: Option<&str>) -> HistoryEntry {
        HistoryEntry {
            id: 0,
            sql: sql.to_string(),
            server: server.to_string(),
            database: database.map(str::to_string),
            timestamp: String::new(),
            duration_ms: None,
            outcome: HistoryOutcome::Unknown,
            run_count: 1,
            provenance: HistoryProvenance::Manual,
            affected_rows: None,
            rollback_sql: None,
            tags: Vec::new(),
            name: name.to_string(),
            description: String::new(),
        }
    }

    #[test]
    fn front_matter_parses_quoted_and_bare_values() {
        let fm = parse_front_matter("-- sqlly: connection=\"Prod (RO)\" database=sales\nSELECT 1");
        assert_eq!(fm.connection.as_deref(), Some("Prod (RO)"));
        assert_eq!(fm.database.as_deref(), Some("sales"));
    }

    #[test]
    fn front_matter_ignores_unknown_keys_and_missing_line() {
        let fm = parse_front_matter("-- sqlly: connection=prod extra=ignored\nSELECT 1");
        assert_eq!(fm.connection.as_deref(), Some("prod"));
        assert_eq!(fm.database, None);
        assert_eq!(parse_front_matter("SELECT 1"), FrontMatter::default());
        assert_eq!(parse_front_matter(""), FrontMatter::default());
    }

    #[test]
    fn front_matter_round_trips() {
        let fm = FrontMatter {
            connection: Some("Prod (RO)".into()),
            database: Some("sales".into()),
        };
        let line = render_front_matter(&fm).expect("line");
        assert_eq!(parse_front_matter(&line), fm);
        assert!(render_front_matter(&FrontMatter::default()).is_none());
    }

    #[test]
    fn scan_finds_nested_sql_files_and_sorts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("reports/daily")).unwrap();
        fs::write(root.join("alpha.sql"), "SELECT 1\n").unwrap();
        fs::write(
            root.join("reports/daily/beta.sql"),
            "-- sqlly: connection=prod\nSELECT 2\n",
        )
        .unwrap();
        fs::write(root.join("reports/zeta.sql"), "SELECT 3\n").unwrap();
        let tree = scan_library(root).unwrap();
        let names: Vec<_> = tree.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta", "beta"]);
        assert!(tree.folders.iter().any(|f| f == Path::new("reports")));
        assert!(tree.folders.iter().any(|f| f == Path::new("reports/daily")));
        let beta = tree
            .entries
            .iter()
            .find(|e| e.name == "beta")
            .expect("beta");
        assert_eq!(beta.front_matter.connection.as_deref(), Some("prod"));
        assert_eq!(beta.folder, PathBuf::from("reports/daily"));
    }

    #[test]
    fn scan_skips_non_sql_and_hidden_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".secret")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join(".secret/hidden.sql"), "SELECT 1\n").unwrap();
        fs::write(root.join("target/built.sql"), "SELECT 1\n").unwrap();
        fs::write(root.join("notes.md"), "not sql").unwrap();
        fs::write(root.join("ok.sql"), "SELECT 1\n").unwrap();
        let tree = scan_library(root).unwrap();
        assert_eq!(tree.entries.len(), 1);
        assert_eq!(tree.entries[0].name, "ok");
        assert!(!tree.folders.iter().any(|f| f == Path::new(".secret")));
        assert!(!tree.folders.iter().any(|f| f == Path::new("target")));
    }

    #[test]
    fn filter_matches_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("reports")).unwrap();
        fs::write(root.join("reports/daily.sql"), "SELECT 1\n").unwrap();
        fs::write(root.join("other.sql"), "SELECT 2\n").unwrap();
        let tree = scan_library(root).unwrap();
        let filtered = filter_tree(&tree, "DAILY");
        assert_eq!(filtered.entries.len(), 1);
        assert_eq!(filtered.entries[0].name, "daily");
        assert!(filtered.folders.iter().any(|f| f == Path::new("reports")));
        assert_eq!(filter_tree(&tree, "").entries.len(), 2);
    }

    #[test]
    fn unique_file_name_appends_counter() {
        let dir = tempfile::tempdir().unwrap();
        let first = unique_file_name(dir.path(), "query");
        assert_eq!(first.file_name().unwrap(), "query.sql");
        fs::write(&first, "--\n").unwrap();
        let second = unique_file_name(dir.path(), "query");
        assert_eq!(second.file_name().unwrap(), "query (2).sql");
        fs::write(&second, "--\n").unwrap();
        let third = unique_file_name(dir.path(), "query");
        assert_eq!(third.file_name().unwrap(), "query (3).sql");
        fs::write(&third, "--\n").unwrap();
        let slashy = unique_file_name(dir.path(), "a/b\\c");
        assert_eq!(slashy.file_name().unwrap(), "abc.sql");
        let empty = unique_file_name(dir.path(), "   ");
        assert_eq!(empty.file_name().unwrap(), "query (4).sql");
    }

    #[test]
    fn pin_export_uses_name_then_first_line() {
        let named = pin("Monthly sales", "SELECT 1 FROM huge_table", "s", None);
        let (stem, _) = pin_to_library_file(&named);
        assert_eq!(stem, "Monthly sales");
        let unnamed = pin(
            "",
            "SELECT amount FROM invoices WHERE year = 2024",
            "s",
            None,
        );
        let (stem, _) = pin_to_library_file(&unnamed);
        assert_eq!(stem, "SELECT amount FROM invoices WHERE year =");
        assert_eq!(stem.chars().count(), 40);
    }

    #[test]
    fn pin_export_writes_front_matter_from_server_and_database() {
        let entry = pin("q", "SELECT 1", "Prod", Some("sales"));
        let (_stem, contents) = pin_to_library_file(&entry);
        assert!(contents.starts_with("-- sqlly:"));
        let fm = parse_front_matter(&contents);
        assert_eq!(fm.connection.as_deref(), Some("Prod"));
        assert_eq!(fm.database.as_deref(), Some("sales"));
        assert!(contents.contains("SELECT 1"));
    }
}
