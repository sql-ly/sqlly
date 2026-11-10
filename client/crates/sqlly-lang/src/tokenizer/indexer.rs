//! Filesystem indexing pipeline + a poll-based debounced watcher.
//!
//! The pipeline ties together [`crate::fsmodel`]'s profile/index/delta types and
//! this crate's [`SimpleSqlTokenizer`]: it discovers files under a
//! [`FilesystemDatabaseProfile`], reads + hashes each one, tokenizes the changed
//! ones into per-file snapshots, merges them, and computes a
//! [`SqlModelDelta`](crate::fsmodel::SqlModelDelta) versus a prior index.
//!
//! The watcher is intentionally poll-based (no OS-specific crate) so it stays
//! portable and unit-testable: [`FilesystemWatcher::poll`] re-scans and returns a
//! delta only when something actually changed, and debouncing is expressed as a
//! minimum interval between effective polls.

use crate::fsmodel::{
    FileIndex, FileSourceRef, FilesystemDatabaseProfile, SqlModelDelta, SqlModelSnapshot,
    compute_delta, content_hash, merge_snapshots, normalize_path_separators,
};
use crate::tokenizer::{SimpleSqlTokenizer, SqlTokenizer};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

/// Per-file tokenization result that can be reused when the file hash is
/// unchanged on a later scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    pub hash: String,
    pub snapshot: SqlModelSnapshot,
}

pub type FileSnapshotCache = BTreeMap<String, FileSnapshot>;

/// The result of a (re)index pass: the merged model, the updated file index, and
/// the delta versus the previous snapshot.
#[derive(Debug, Clone)]
pub struct IndexOutcome {
    pub snapshot: SqlModelSnapshot,
    pub file_index: FileIndex,
    pub file_snapshots: FileSnapshotCache,
    pub delta: SqlModelDelta,
    pub tokenized_files: usize,
    pub reused_file_snapshots: usize,
}

/// Recursively collects files under `root` that the profile accepts. Symlinks
/// are not followed (cycle safety). I/O errors on a subtree are skipped rather
/// than aborting the whole scan.
pub fn discover_files(profile: &FilesystemDatabaseProfile, root: &Path) -> Vec<PathBuf> {
    let exclude_lower = lowered_exclude_patterns(profile);
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if let Some(s) = path.to_str() {
                    // Let the profile prune excluded directories early.
                    if profile.accepts_path(&format!("{s}/")) || !is_excluded_dir(&exclude_lower, s)
                    {
                        stack.push(path);
                    }
                } else {
                    stack.push(path);
                }
            } else if let Some(s) = path.to_str()
                && profile.accepts_path(s)
            {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Whether `path` (a directory) falls under one of the profile's excluded
/// directory patterns. Matches [`FilesystemDatabaseProfile::accepts_path`]
/// semantics: patterns keep their slash delimiters, so `/bin/` matches the
/// path segment `bin` but not `robin`. The path gets a trailing slash so a
/// pattern can match the final segment.
fn is_excluded_dir(exclude_lower: &[String], path: &str) -> bool {
    let lower = format!("{}/", normalize_path_separators(path).to_ascii_lowercase());
    exclude_lower.iter().any(|x| lower.contains(x))
}

fn lowered_exclude_patterns(profile: &FilesystemDatabaseProfile) -> Vec<String> {
    profile
        .exclude_contains
        .iter()
        .map(|x| x.to_ascii_lowercase())
        .collect()
}

/// Tokenizes a single source string into a per-file snapshot.
pub fn tokenize_file(path: &Path, contents: &str, hash: &str) -> SqlModelSnapshot {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let source = FileSourceRef {
        source_file_name: name,
        source_file_path: path.to_string_lossy().to_string(),
        file_hash: hash.to_string(),
    };
    SimpleSqlTokenizer
        .tokenize_source(contents, source)
        .snapshot
}

/// Runs a full (re)index of every root in the profile, computing a delta versus
/// `previous`. Files are still read to compute their content hash, but unchanged
/// files reuse their prior per-file tokenization result instead of retokenizing.
pub fn index_profile(
    profile: &FilesystemDatabaseProfile,
    previous: &SqlModelSnapshot,
    prior_index: &FileIndex,
    prior_file_snapshots: &FileSnapshotCache,
) -> IndexOutcome {
    let mut file_index = FileIndex::new();
    let mut file_snapshots = FileSnapshotCache::new();
    let mut per_file: Vec<SqlModelSnapshot> = Vec::new();
    let mut tokenized_files = 0;
    let mut reused_file_snapshots = 0;

    for root in &profile.root_paths {
        for path in discover_files(profile, Path::new(root)) {
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let hash = content_hash(&bytes);
            let key = path.to_string_lossy().to_string();
            file_index.record(&key, &hash);
            if !prior_index.is_changed(&key, &hash)
                && let Some(cached) = prior_file_snapshots.get(&key)
                && cached.hash == hash
            {
                per_file.push(cached.snapshot.clone());
                file_snapshots.insert(key, cached.clone());
                reused_file_snapshots += 1;
                continue;
            }
            let contents = String::from_utf8_lossy(&bytes);
            let snapshot = tokenize_file(&path, &contents, &hash);
            per_file.push(snapshot.clone());
            file_snapshots.insert(key, FileSnapshot { hash, snapshot });
            tokenized_files += 1;
        }
    }

    let snapshot = merge_snapshots(per_file);
    let delta = compute_delta(previous, &snapshot);
    IndexOutcome {
        snapshot,
        file_index,
        file_snapshots,
        delta,
        tokenized_files,
        reused_file_snapshots,
    }
}

/// A portable, poll-based debounced filesystem watcher. The host calls
/// [`poll`](Self::poll) on a timer (or on an OS event); the watcher honors a
/// debounce interval and only returns an [`IndexOutcome`] when the on-disk file
/// set/hashes actually changed since the last effective scan.
pub struct FilesystemWatcher {
    profile: FilesystemDatabaseProfile,
    snapshot: SqlModelSnapshot,
    file_index: FileIndex,
    file_snapshots: FileSnapshotCache,
    debounce: Duration,
    last_scan: Option<Instant>,
}

impl FilesystemWatcher {
    pub fn new(profile: FilesystemDatabaseProfile, debounce: Duration) -> Self {
        Self {
            profile,
            snapshot: SqlModelSnapshot::default(),
            file_index: FileIndex::new(),
            file_snapshots: FileSnapshotCache::new(),
            debounce,
            last_scan: None,
        }
    }

    pub fn snapshot(&self) -> &SqlModelSnapshot {
        &self.snapshot
    }

    pub fn file_index(&self) -> &FileIndex {
        &self.file_index
    }

    pub fn file_snapshots(&self) -> &FileSnapshotCache {
        &self.file_snapshots
    }

    /// Performs the initial full scan, always returning the outcome (delta is
    /// everything as "added").
    pub fn initial_scan(&mut self) -> IndexOutcome {
        let empty_snapshots = FileSnapshotCache::new();
        let outcome = index_profile(
            &self.profile,
            &SqlModelSnapshot::default(),
            &FileIndex::new(),
            &empty_snapshots,
        );
        self.snapshot = outcome.snapshot.clone();
        self.file_index = outcome.file_index.clone();
        self.file_snapshots = outcome.file_snapshots.clone();
        self.last_scan = Some(Instant::now());
        outcome
    }

    /// Re-scans if the debounce window has elapsed. Returns `Some(outcome)` only
    /// when the file index actually changed (new/changed/removed files);
    /// otherwise `None` (debounced or no-op).
    pub fn poll(&mut self) -> Option<IndexOutcome> {
        if let Some(last) = self.last_scan
            && last.elapsed() < self.debounce
        {
            return None;
        }
        let outcome = index_profile(
            &self.profile,
            &self.snapshot,
            &self.file_index,
            &self.file_snapshots,
        );
        self.last_scan = Some(Instant::now());
        let changed = outcome.file_index != self.file_index;
        self.snapshot = outcome.snapshot.clone();
        self.file_index = outcome.file_index.clone();
        self.file_snapshots = outcome.file_snapshots.clone();
        if changed { Some(outcome) } else { None }
    }

    /// Forces an immediate re-scan ignoring the debounce window (e.g. on an
    /// explicit OS change event), returning the outcome if anything changed.
    pub fn force_poll(&mut self) -> Option<IndexOutcome> {
        self.last_scan = None;
        self.poll()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::SystemTime;
    #[cfg(target_arch = "wasm32")]
    use web_time::SystemTime;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "fsidx_{}_{}_{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn write(dir: &Path, name: &str, sql: &str) {
        std::fs::write(dir.join(name), sql).unwrap();
    }

    struct TempExtractDir {
        path: PathBuf,
    }

    impl TempExtractDir {
        fn new(tag: &str) -> Self {
            Self {
                path: temp_dir(tag),
            }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempExtractDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.path).ok();
        }
    }

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .expect("crate lives under client/crates/sqlly-tokenizer")
            .to_path_buf()
    }

    fn unzip_fixture(zip_path: &Path, destination: &Path) {
        let status = std::process::Command::new("unzip")
            .arg("-q")
            .arg(zip_path)
            .arg("-d")
            .arg(destination)
            .status()
            .expect("run unzip command for test-db.zip fixture");
        assert!(status.success(), "unzip failed for {}", zip_path.display());
    }

    #[test]
    fn initial_scan_builds_model_from_files() {
        let dir = temp_dir("initial");
        write(
            &dir,
            "Orders.sql",
            "CREATE TABLE dbo.Orders (Id INT NOT NULL, Total DECIMAL(10,2) NULL);",
        );
        write(
            &dir,
            "GetOrders.sql",
            "CREATE PROCEDURE dbo.GetOrders @Id INT AS SELECT Id FROM dbo.Orders;",
        );
        let profile = FilesystemDatabaseProfile::new("db", vec![dir.to_string_lossy().into()]);
        let mut watcher = FilesystemWatcher::new(profile, Duration::from_millis(0));
        let outcome = watcher.initial_scan();

        assert_eq!(outcome.file_index.files.len(), 2);
        // dbo schema with one table + one procedure.
        let schema = outcome
            .snapshot
            .schemas
            .iter()
            .find(|s| s.node.name.eq_ignore_ascii_case("dbo"))
            .expect("dbo schema present");
        assert!(
            schema
                .tables
                .iter()
                .any(|t| t.source_node.node.name == "Orders")
        );
        assert!(
            schema
                .procedures
                .iter()
                .any(|p| p.operation.source_node.node.name == "GetOrders")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn poll_detects_create_modify_delete() {
        let dir = temp_dir("cmd");
        write(&dir, "A.sql", "CREATE TABLE dbo.A (Id INT);");
        let profile = FilesystemDatabaseProfile::new("db", vec![dir.to_string_lossy().into()]);
        let mut watcher = FilesystemWatcher::new(profile, Duration::from_millis(0));
        watcher.initial_scan();

        // CREATE a new file.
        write(&dir, "B.sql", "CREATE TABLE dbo.B (Id INT);");
        let created = watcher.force_poll().expect("change detected");
        assert!(created.delta.added.iter().any(|n| n.name == "B"));

        // MODIFY a file (add a column).
        write(
            &dir,
            "B.sql",
            "CREATE TABLE dbo.B (Id INT, Name NVARCHAR(50));",
        );
        let modified = watcher.force_poll().expect("modify detected");
        assert!(
            modified.delta.added.iter().any(|n| n.name == "Name")
                || modified.delta.updated.iter().any(|n| n.name == "B")
        );

        // DELETE a file.
        std::fs::remove_file(dir.join("A.sql")).unwrap();
        let deleted = watcher.force_poll().expect("delete detected");
        assert!(
            deleted
                .delta
                .removed
                .iter()
                .any(|id| id.0.to_lowercase().contains("a"))
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn poll_debounces_within_interval() {
        let dir = temp_dir("debounce");
        write(&dir, "A.sql", "CREATE TABLE dbo.A (Id INT);");
        let profile = FilesystemDatabaseProfile::new("db", vec![dir.to_string_lossy().into()]);
        let mut watcher = FilesystemWatcher::new(profile, Duration::from_secs(60));
        watcher.initial_scan();
        write(&dir, "B.sql", "CREATE TABLE dbo.B (Id INT);");
        // Within the 60s debounce window, a normal poll is suppressed.
        assert!(watcher.poll().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unchanged_tree_yields_no_delta() {
        let dir = temp_dir("noop");
        write(&dir, "A.sql", "CREATE TABLE dbo.A (Id INT);");
        let profile = FilesystemDatabaseProfile::new("db", vec![dir.to_string_lossy().into()]);
        let mut watcher = FilesystemWatcher::new(profile, Duration::from_millis(0));
        watcher.initial_scan();
        // No filesystem change -> force_poll returns None.
        assert!(watcher.force_poll().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unchanged_files_reuse_cached_snapshots() {
        let dir = temp_dir("reuse");
        write(&dir, "A.sql", "CREATE TABLE dbo.A (Id INT);");
        write(&dir, "B.sql", "CREATE TABLE dbo.B (Id INT);");
        let profile = FilesystemDatabaseProfile::new("db", vec![dir.to_string_lossy().into()]);
        let empty_snapshots = FileSnapshotCache::new();

        let first = index_profile(
            &profile,
            &SqlModelSnapshot::default(),
            &FileIndex::new(),
            &empty_snapshots,
        );
        assert_eq!(first.tokenized_files, 2);
        assert_eq!(first.reused_file_snapshots, 0);

        let second = index_profile(
            &profile,
            &first.snapshot,
            &first.file_index,
            &first.file_snapshots,
        );
        assert_eq!(second.file_index, first.file_index);
        assert_eq!(second.tokenized_files, 0);
        assert_eq!(second.reused_file_snapshots, 2);
        assert!(second.delta.added.is_empty());
        assert!(second.delta.updated.is_empty());
        assert!(second.delta.removed.is_empty());

        write(
            &dir,
            "B.sql",
            "CREATE TABLE dbo.B (Id INT, Name NVARCHAR(50));",
        );
        let third = index_profile(
            &profile,
            &second.snapshot,
            &second.file_index,
            &second.file_snapshots,
        );
        assert_eq!(third.tokenized_files, 1);
        assert_eq!(third.reused_file_snapshots, 1);
        assert!(third.delta.added.iter().any(|n| n.name == "Name"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn excluded_dir_requires_delimited_segment() {
        let profile = FilesystemDatabaseProfile::default();
        let exclude_lower = lowered_exclude_patterns(&profile);
        assert!(!is_excluded_dir(&exclude_lower, "/users/robin/projects"));
        assert!(!is_excluded_dir(&exclude_lower, "/users/robin/objections"));
        assert!(!is_excluded_dir(
            &exclude_lower,
            "/home/dev/target-practice"
        ));
    }

    #[test]
    fn excluded_dir_matches_real_segments() {
        let profile = FilesystemDatabaseProfile::default();
        let exclude_lower = lowered_exclude_patterns(&profile);
        assert!(is_excluded_dir(&exclude_lower, "/repo/bin"));
        assert!(is_excluded_dir(&exclude_lower, "/repo/bin/debug"));
        assert!(is_excluded_dir(&exclude_lower, "/repo/node_modules"));
        assert!(is_excluded_dir(&exclude_lower, "/repo/OBJ"));
    }

    #[test]
    fn excluded_dir_git_segment() {
        let profile = FilesystemDatabaseProfile::default();
        let exclude_lower = lowered_exclude_patterns(&profile);
        assert!(is_excluded_dir(&exclude_lower, "/repo/.git"));
        assert!(!is_excluded_dir(&exclude_lower, "/repo/.github"));
    }

    #[test]
    fn excluded_dir_windows_paths_use_forward_slash_patterns() {
        let profile = FilesystemDatabaseProfile::default();
        let exclude_lower = lowered_exclude_patterns(&profile);
        assert!(!is_excluded_dir(&exclude_lower, r"c:\users\robin\src"));
        assert!(is_excluded_dir(&exclude_lower, r"c:\repo\bin\foo"));
        assert!(!is_excluded_dir(&exclude_lower, "/Users/robin/project"));
    }

    #[test]
    fn discover_files_does_not_skip_robin_path() {
        let dir = temp_dir("robin");
        write(&dir, "Orders.sql", "CREATE TABLE dbo.Orders (Id INT);");
        let profile = FilesystemDatabaseProfile::new("db", vec![dir.to_string_lossy().into()]);
        let files = discover_files(&profile, &dir);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("Orders.sql"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn discover_files_prunes_real_bin_directory() {
        let dir = temp_dir("bin_prune");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        write(
            &bin_dir,
            "Ignored.sql",
            "CREATE TABLE dbo.Ignored (Id INT);",
        );
        write(&dir, "Orders.sql", "CREATE TABLE dbo.Orders (Id INT);");
        let profile = FilesystemDatabaseProfile::new("db", vec![dir.to_string_lossy().into()]);
        let files = discover_files(&profile, &dir);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("Orders.sql"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn large_test_db_zip_indexes_and_cleans_up() {
        let zip_path = repo_root().join("test-db.zip");
        assert!(
            zip_path.exists(),
            "large fixture missing at {}",
            zip_path.display()
        );

        let extraction_path;
        {
            let temp = TempExtractDir::new("testdb_zip");
            extraction_path = temp.path().to_path_buf();
            unzip_fixture(&zip_path, temp.path());

            let root = temp.path().join("test-db");
            assert!(root.is_dir(), "expected extracted test-db directory");

            let profile =
                FilesystemDatabaseProfile::new("test-db", vec![root.to_string_lossy().into()]);
            let files = discover_files(&profile, &root);
            assert!(
                files.len() >= 2_000,
                "expected large SQL fixture, discovered {} files",
                files.len()
            );

            let empty_snapshots = FileSnapshotCache::new();
            let first = index_profile(
                &profile,
                &SqlModelSnapshot::default(),
                &FileIndex::new(),
                &empty_snapshots,
            );
            assert_eq!(first.file_index.files.len(), files.len());
            assert_eq!(first.tokenized_files, files.len());
            assert_eq!(first.reused_file_snapshots, 0);
            assert!(
                !first.snapshot.schemas.is_empty(),
                "large fixture should produce schema nodes"
            );

            let second = index_profile(
                &profile,
                &first.snapshot,
                &first.file_index,
                &first.file_snapshots,
            );
            assert_eq!(second.file_index, first.file_index);
            assert_eq!(second.tokenized_files, 0);
            assert_eq!(second.reused_file_snapshots, files.len());
            assert!(
                second.delta.added.is_empty()
                    && second.delta.updated.is_empty()
                    && second.delta.removed.is_empty(),
                "unchanged large fixture should produce an empty delta"
            );
        }

        assert!(
            !extraction_path.exists(),
            "temporary extraction directory should be removed after the test"
        );
    }
}
