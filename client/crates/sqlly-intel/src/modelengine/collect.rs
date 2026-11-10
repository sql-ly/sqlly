//! Collection pipeline (ADR-011 §4): fan-out collectors emit provenance-
//! tagged fragments into a generation's staging area; a barrier waits for all
//! tasks; the merge runs single-threaded and the snapshot pointer swaps
//! atomically. Readers never see a half-merged model.

use crate::modelengine::pool::{ConnectionPool, PoolError};
use sqlly_lang::sqlmodel::{
    Dialect, IncompleteRegions, ModelDelta, ModelSnapshot, ObjectFragment, Provenance,
};
use sqlly_lang::sqlparse::{lower, parse};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct CollectionError {
    pub source: String,
    pub detail: String,
}

/// Staged results of one generation, pre-merge.
#[derive(Default)]
pub struct Staging {
    pub fragments: Vec<ObjectFragment>,
    pub removed_files: BTreeSet<String>,
    pub errors: Vec<CollectionError>,
    pub incomplete: IncompleteRegions,
}

// ---------------------------------------------------------------------------
// Filesystem collector
// ---------------------------------------------------------------------------

/// Discover `.sql` files under `root`, skipping dot-directories.
pub fn discover_sql_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("sql"))
                .unwrap_or(false)
            {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Parse one file into fragments (the per-worker unit of the fs collector).
pub fn collect_file(path: &Path, dialect: Dialect) -> Result<Vec<ObjectFragment>, CollectionError> {
    let text = std::fs::read_to_string(path).map_err(|e| CollectionError {
        source: path.display().to_string(),
        detail: e.to_string(),
    })?;
    let tree = parse(&text, dialect);
    let provenance = Provenance::File {
        path: path.display().to_string(),
        content_hash: tree.content_hash,
        span: sqlly_lang::sqlmodel::Span::new(0, text.len()),
    };
    Ok(lower(&tree, provenance))
}

/// Run the filesystem collector over `files` on a bounded worker pool of
/// exactly `fs_read_threads` OS threads (parsing is CPU-bound; this is
/// intentionally not the async runtime).
pub fn collect_filesystem(files: &[PathBuf], dialect: Dialect, fs_read_threads: usize) -> Staging {
    let threads = fs_read_threads.max(1).min(files.len().max(1));
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: std::sync::Mutex<Staging> = std::sync::Mutex::new(Staging::default());

    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if i >= files.len() {
                        break;
                    }
                    match collect_file(&files[i], dialect) {
                        Ok(fragments) => {
                            results
                                .lock()
                                .expect("staging lock")
                                .fragments
                                .extend(fragments);
                        }
                        Err(e) => {
                            let mut staging = results.lock().expect("staging lock");
                            staging
                                .incomplete
                                .prefixes
                                .insert(format!("file:{}", files[i].display()));
                            staging.errors.push(e);
                        }
                    }
                }
            });
        }
    });

    results.into_inner().expect("staging lock")
}

// ---------------------------------------------------------------------------
// Database collector
// ---------------------------------------------------------------------------

/// Catalog queries for one dialect. Implementations translate pooled-query
/// rows into fragments. The SQLite implementation ships in
/// [`crate::modelengine::sqlite_driver`]; other dialects implement the same trait in their
/// driver crates.
#[async_trait::async_trait]
pub trait CatalogCollector: Send + Sync {
    fn dialect(&self) -> Dialect;

    /// Enumerate (namespace, kind-batch) collection units.
    async fn plan(&self, pool: &Arc<ConnectionPool>) -> Result<Vec<CollectUnit>, PoolError>;

    /// Collect one unit through a pooled connection.
    async fn collect(
        &self,
        pool: &Arc<ConnectionPool>,
        unit: &CollectUnit,
        profile: &str,
    ) -> Result<Vec<ObjectFragment>, PoolError>;
}

#[derive(Debug, Clone)]
pub struct CollectUnit {
    pub label: String,
    /// Fold-key prefix this unit covers, used to mark Incomplete on failure.
    pub region_prefix: String,
}

/// Fan out all units of a database collection through the pool. The pool cap
/// (== db_concurrency) bounds parallelism structurally; we just spawn.
pub async fn collect_database(
    collector: Arc<dyn CatalogCollector>,
    pool: Arc<ConnectionPool>,
    profile: String,
) -> Staging {
    let mut staging = Staging::default();
    let units = match collector.plan(&pool).await {
        Ok(u) => u,
        Err(e) => {
            staging.errors.push(CollectionError {
                source: profile.clone(),
                detail: e,
            });
            // Whole source unknown.
            staging.incomplete.prefixes.insert(String::new());
            return staging;
        }
    };

    let mut handles = Vec::new();
    for unit in units {
        let collector = Arc::clone(&collector);
        let pool = Arc::clone(&pool);
        let profile = profile.clone();
        handles.push(tokio::spawn(async move {
            let result = collector.collect(&pool, &unit, &profile).await;
            (unit, result)
        }));
    }
    // Barrier: every task completes (or fails) before merge.
    for h in handles {
        match h.await {
            Ok((_, Ok(fragments))) => staging.fragments.extend(fragments),
            Ok((unit, Err(e))) => {
                staging
                    .incomplete
                    .prefixes
                    .insert(unit.region_prefix.clone());
                staging.errors.push(CollectionError {
                    source: unit.label,
                    detail: e,
                });
            }
            Err(join_err) => {
                staging.errors.push(CollectionError {
                    source: profile.clone(),
                    detail: join_err.to_string(),
                });
            }
        }
    }
    staging
}

// ---------------------------------------------------------------------------
// Merge (fan-in)
// ---------------------------------------------------------------------------

/// Merge staged fragments into the previous snapshot, producing the next
/// snapshot + delta. Single-threaded by design (ADR-011 §4.4).
pub fn merge(
    previous: &ModelSnapshot,
    generation: u64,
    mut stagings: Vec<Staging>,
) -> (ModelSnapshot, ModelDelta, Vec<CollectionError>) {
    let mut fragments = Vec::new();
    let mut removed = BTreeSet::new();
    let mut errors = Vec::new();
    let mut incomplete = IncompleteRegions::default();
    for staging in stagings.drain(..) {
        // Database fragments merge before file fragments so the file-wins
        // rule applies deterministically regardless of collector finish order.
        fragments.extend(staging.fragments);
        removed.extend(staging.removed_files);
        errors.extend(staging.errors);
        incomplete.prefixes.extend(staging.incomplete.prefixes);
    }
    fragments.sort_by_key(|f| f.provenance.is_file()); // db first, files last
    let (next, delta) = previous.apply(generation, fragments, &removed, incomplete);
    (next, delta, errors)
}
