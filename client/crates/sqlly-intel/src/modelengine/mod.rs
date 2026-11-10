//! # sqlly-modelengine
//!
//! The ADR-011 model engine runtime: parallel collectors (database via
//! connection pools, filesystem via a bounded worker pool), generation-based
//! staging with a fan-in barrier, atomic snapshot swap, two-scope validation
//! with cached results, fixed/edit modes, ad-hoc query validation, and
//! cursor-aware IntelliSense (symbol + value modes).

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

pub mod collect;
pub mod intellisense;
pub mod pool;
pub mod sql_collector;
pub mod sqlite_driver;
pub mod validate;

pub use collect::{CatalogCollector, CollectionError};
pub use intellisense::{
    CompletionItem, CompletionMode, CompletionRequest, Completions, MatchType, SortOrder,
    ValueCompletionParams, complete_symbols, complete_values,
};
pub use pool::{ConnectionPool, PooledConnection, PooledDriver};
pub use validate::{
    Diagnostic, EngineMode, Exemption, ExemptionSet, IssueClass, QueryValidationReport,
    QueryValidationRequest, Severity, ValidationScope,
};

use sqlly_lang::sqlmodel::{Dialect, ModelDelta, ModelSnapshot, SqlName};
use sqlly_lang::sqlparse::{content_hash, parse};

// ---------------------------------------------------------------------------
// Configuration (ADR-011 §4.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EngineConcurrency {
    /// Max in-flight catalog/value queries per connection profile. The pool's
    /// max size per profile equals this value, so the limit is structural.
    pub db_concurrency: NonZeroUsize,
    /// Worker threads for file reading + parsing.
    pub fs_read_threads: NonZeroUsize,
}

impl Default for EngineConcurrency {
    fn default() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self {
            db_concurrency: NonZeroUsize::new(4).expect("nonzero"),
            fs_read_threads: NonZeroUsize::new(cores.min(8)).expect("nonzero"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub concurrency: EngineConcurrency,
    pub default_catalog: Option<SqlName>,
    pub default_namespace: Option<SqlName>,
    /// Maximum number of distinct value-menu slots held in the value cache.
    /// When exceeded, the least-recently-used entry is evicted. The cache is
    /// keyed by `(profile, canonical empty-prefix SQL)`, so this bounds memory
    /// across many distinct columns/tables a long-lived session touches.
    pub value_cache_capacity: usize,
}

/// Default cap for the value cache (see [`EngineConfig::value_cache_capacity`]).
pub const VALUE_CACHE_CAPACITY_DEFAULT: usize = 64;

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            concurrency: EngineConcurrency::default(),
            default_catalog: None,
            default_namespace: None,
            value_cache_capacity: VALUE_CACHE_CAPACITY_DEFAULT,
        }
    }
}

/// A source the engine collects from.
pub enum SourceConfig {
    Database {
        profile: String,
        dialect: Dialect,
        driver: Arc<dyn PooledDriver>,
        collector: Arc<dyn CatalogCollector>,
    },
    Filesystem {
        root: PathBuf,
        dialect: Dialect,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notify {
    FileChanged { path: PathBuf },
    FileRemoved { path: PathBuf },
    DatabaseChanged { profile: String },
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

struct DatabaseSource {
    profile: String,
    pool: Arc<ConnectionPool>,
    collector: Arc<dyn CatalogCollector>,
}

struct FilesystemSource {
    root: PathBuf,
    dialect: Dialect,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ValidationKey {
    content_hash: u64,
    generation: u64,
    fixed_mode: bool,
    model_scope: bool,
}

/// Raw candidate rows captured by an explicit value-menu trigger, plus the
/// plan they were fetched with. Keystrokes filter these rows locally; the
/// (live-slot-dependent) replace span is never cached — items are re-shaped per
/// request. Invalidated by a re-trigger (overwrite) or a snapshot rebuild.
struct CachedValueSet {
    rows: Vec<Vec<Option<String>>>,
    plan: intellisense::ValueQueryPlan,
    truncated: bool,
    note: String,
    dialect: Dialect,
    /// Monotonic access tick for LRU eviction; bumped on insert and on each
    /// cache hit so the least-recently-used slot is evicted first.
    last_used: u64,
}

pub struct ModelEngine {
    config: RwLock<EngineConfig>,
    snapshot: RwLock<Arc<ModelSnapshot>>,
    db_sources: RwLock<Vec<Arc<DatabaseSource>>>,
    fs_sources: RwLock<Vec<Arc<FilesystemSource>>>,
    mode: RwLock<EngineMode>,
    generation: Mutex<u64>,
    validation_cache: Mutex<HashMap<ValidationKey, Arc<QueryValidationReport>>>,
    value_cache: Mutex<HashMap<String, CachedValueSet>>,
    /// Monotonic counter for value-cache LRU bookkeeping.
    value_cache_tick: Mutex<u64>,
    errors: Mutex<Vec<CollectionError>>,
}

impl ModelEngine {
    pub fn new(config: EngineConfig) -> Arc<Self> {
        Arc::new(Self {
            config: RwLock::new(config),
            snapshot: RwLock::new(Arc::new(ModelSnapshot::empty())),
            db_sources: RwLock::new(Vec::new()),
            fs_sources: RwLock::new(Vec::new()),
            mode: RwLock::new(EngineMode::Fixed),
            generation: Mutex::new(0),
            validation_cache: Mutex::new(HashMap::new()),
            value_cache: Mutex::new(HashMap::new()),
            value_cache_tick: Mutex::new(0),
            errors: Mutex::new(Vec::new()),
        })
    }

    /// Reconfigure concurrency at runtime. Pool caps apply to new pools;
    /// existing pools drain naturally (no task is killed).
    pub fn configure(&self, concurrency: EngineConcurrency) {
        self.config.write().concurrency = concurrency;
    }

    pub fn add_source(&self, source: SourceConfig) {
        match source {
            SourceConfig::Database {
                profile,
                dialect: _,
                driver,
                collector,
            } => {
                let cap = self.config.read().concurrency.db_concurrency.get();
                let pool = ConnectionPool::new(driver, cap);
                self.db_sources.write().push(Arc::new(DatabaseSource {
                    profile,
                    pool,
                    collector,
                }));
            }
            SourceConfig::Filesystem { root, dialect } => {
                self.fs_sources
                    .write()
                    .push(Arc::new(FilesystemSource { root, dialect }));
            }
        }
    }

    /// Lock-free-ish read of the current snapshot (read lock held only for
    /// the Arc clone; readers never block on collection).
    pub fn snapshot(&self) -> Arc<ModelSnapshot> {
        Arc::clone(&self.snapshot.read())
    }

    pub fn mode(&self) -> EngineMode {
        self.mode.read().clone()
    }

    /// Switching Edit → Fixed clears the validation cache so everything the
    /// exemption set suppressed revalidates.
    pub fn set_mode(&self, mode: EngineMode) {
        let was_edit = matches!(*self.mode.read(), EngineMode::Edit { .. });
        let to_fixed = matches!(mode, EngineMode::Fixed);
        *self.mode.write() = mode;
        if was_edit && to_fixed {
            self.validation_cache.lock().clear();
        }
    }

    pub fn collection_errors(&self) -> Vec<CollectionError> {
        self.errors.lock().clone()
    }

    fn next_generation(&self) -> u64 {
        let mut g = self.generation.lock();
        *g += 1;
        *g
    }

    /// Full rebuild (ADR-011 §4.5): run every collector at full configured
    /// parallelism, barrier, merge, swap.
    pub async fn rebuild(&self) -> ModelDelta {
        let generation = self.next_generation();
        let fs_threads = self.config.read().concurrency.fs_read_threads.get();
        let db_sources: Vec<Arc<DatabaseSource>> = self.db_sources.read().clone();
        let fs_sources: Vec<Arc<FilesystemSource>> = self.fs_sources.read().clone();

        // Database collectors fan out on the async runtime…
        let mut db_handles = Vec::new();
        for src in db_sources {
            let collector = Arc::clone(&src.collector);
            let pool = Arc::clone(&src.pool);
            let profile = src.profile.clone();
            db_handles.push(tokio::spawn(collect::collect_database(
                collector, pool, profile,
            )));
        }
        // …while filesystem collection runs its own bounded thread pool.
        let fs_staging = tokio::task::spawn_blocking(move || {
            let mut all = Vec::new();
            for src in fs_sources {
                let files = collect::discover_sql_files(&src.root);
                all.push(collect::collect_filesystem(&files, src.dialect, fs_threads));
            }
            all
        });

        let mut stagings = Vec::new();
        for h in db_handles {
            if let Ok(staging) = h.await {
                stagings.push(staging);
            }
        }
        if let Ok(fs) = fs_staging.await {
            stagings.extend(fs);
        }

        // Rebuild replaces the world: start from an empty snapshot so removed
        // objects disappear, but preserve the generation lineage.
        let empty = ModelSnapshot::empty();
        let (next, delta, errors) = collect::merge(&empty, generation, stagings);
        *self.errors.lock() = errors;
        self.swap_snapshot(next, &delta);
        delta
    }

    /// Incremental change notification (ADR-011 §7): cost proportional to the
    /// change, not the model.
    pub async fn notify(&self, notification: Notify) -> ModelDelta {
        let generation = self.next_generation();
        match notification {
            Notify::FileChanged { path } => {
                let dialect = self
                    .fs_sources
                    .read()
                    .iter()
                    .find(|s| path.starts_with(&s.root))
                    .map(|s| s.dialect)
                    .unwrap_or(Dialect::SqlServer);
                let staging = tokio::task::spawn_blocking(move || {
                    let mut staging = collect::Staging::default();
                    // Re-collection replaces all prior fragments of this file.
                    staging.removed_files.insert(path.display().to_string());
                    match collect::collect_file(&path, dialect) {
                        Ok(fragments) => staging.fragments.extend(fragments),
                        Err(e) => staging.errors.push(e),
                    }
                    staging
                })
                .await
                .unwrap_or_default();
                let previous = self.snapshot();
                let (next, delta, errors) = collect::merge(&previous, generation, vec![staging]);
                self.errors.lock().extend(errors);
                self.swap_snapshot(next, &delta);
                delta
            }
            Notify::FileRemoved { path } => {
                let mut staging = collect::Staging::default();
                staging.removed_files.insert(path.display().to_string());
                let previous = self.snapshot();
                let (next, delta, _) = collect::merge(&previous, generation, vec![staging]);
                self.swap_snapshot(next, &delta);
                delta
            }
            Notify::DatabaseChanged { profile } => {
                let source = self
                    .db_sources
                    .read()
                    .iter()
                    .find(|s| s.profile == profile)
                    .cloned();
                match source {
                    Some(src) => {
                        let staging = collect::collect_database(
                            Arc::clone(&src.collector),
                            Arc::clone(&src.pool),
                            src.profile.clone(),
                        )
                        .await;
                        let previous = self.snapshot();
                        let (next, delta, errors) =
                            collect::merge(&previous, generation, vec![staging]);
                        self.errors.lock().extend(errors);
                        self.swap_snapshot(next, &delta);
                        delta
                    }
                    None => ModelDelta {
                        generation,
                        ..ModelDelta::default()
                    },
                }
            }
        }
    }

    fn swap_snapshot(&self, next: ModelSnapshot, delta: &ModelDelta) {
        let previous = self.snapshot();
        let next = Arc::new(next);
        *self.snapshot.write() = Arc::clone(&next);
        self.invalidate_validation_cache(&previous, delta);
        // Cached value-menu candidates may be stale after the model changes;
        // drop them so the next explicit trigger refetches (§8.2, cache
        // lifetime = until re-trigger or snapshot rebuild).
        self.value_cache.lock().clear();
    }

    /// Targeted invalidation (ADR-011 §6.3): drop only model-scope entries
    /// whose reports referenced symbols touched by the delta (directly or via
    /// the dependency index). Tree-scope entries survive snapshot changes.
    fn invalidate_validation_cache(&self, previous: &ModelSnapshot, delta: &ModelDelta) {
        if delta.is_empty() {
            return;
        }
        let mut doomed_keys: BTreeSet<String> = BTreeSet::new();
        for touched in delta.touched() {
            doomed_keys.insert(touched.fold_key());
            doomed_keys.insert(touched.name.fold_key().to_string());
            for dep in previous.dependents_of(touched) {
                if let Some(entry) = previous.get(dep) {
                    doomed_keys.insert(entry.reference.fold_key());
                }
            }
        }
        let mut cache = self.validation_cache.lock();
        cache.retain(|key, report| {
            if !key.model_scope {
                return true;
            }
            report.resolved.iter().all(|r| {
                !doomed_keys.contains(&r.reference.fold_key())
                    && !doomed_keys.contains(r.reference.name.fold_key())
                    && r.resolved_to
                        .as_ref()
                        .map(|t| !doomed_keys.contains(&t.fold_key()))
                        .unwrap_or(true)
            })
        });
    }

    /// Ad-hoc query validation (ADR-011 §5.4), cache-first.
    pub fn validate_query(&self, req: QueryValidationRequest) -> Arc<QueryValidationReport> {
        let snapshot = self.snapshot();
        let mode = self.mode();
        let key = ValidationKey {
            content_hash: content_hash(&req.text),
            generation: if req.scope == ValidationScope::Model {
                snapshot.generation
            } else {
                0
            },
            fixed_mode: matches!(mode, EngineMode::Fixed),
            model_scope: req.scope == ValidationScope::Model,
        };
        if let Some(hit) = self.validation_cache.lock().get(&key) {
            return Arc::clone(hit);
        }
        let report = Arc::new(validate::validate_query(&req, &snapshot, &mode));
        self.validation_cache
            .lock()
            .insert(key, Arc::clone(&report));
        report
    }

    pub fn cache_len(&self) -> usize {
        self.validation_cache.lock().len()
    }

    /// Number of slots currently held in the value-completion cache. Primarily
    /// for tests/diagnostics.
    pub fn value_cache_len(&self) -> usize {
        self.value_cache.lock().len()
    }

    /// Next monotonic tick for value-cache LRU bookkeeping.
    fn next_value_tick(&self) -> u64 {
        let mut tick = self.value_cache_tick.lock();
        *tick += 1;
        *tick
    }

    /// IntelliSense entry point (ADR-011 §8). Symbol mode is a synchronous
    /// snapshot lookup; value mode awaits a pooled query.
    pub async fn complete(&self, req: CompletionRequest) -> Completions {
        let snapshot = self.snapshot();
        match &req.mode {
            CompletionMode::Symbol => complete_symbols(&req, &snapshot),
            CompletionMode::Value(params) => {
                let source = self.db_sources.read().first().map(Arc::clone);
                match source {
                    Some(source) => self
                        .complete_values_cached(&req, params, &snapshot, &source)
                        .await
                        .unwrap_or_else(|e| Completions {
                            items: vec![],
                            truncated: false,
                            snapshot_generation: snapshot.generation,
                            slot_note: Some(format!("value query failed: {e}")),
                        }),
                    None => Completions {
                        items: vec![],
                        truncated: false,
                        snapshot_generation: snapshot.generation,
                        slot_note: Some("no database source configured".into()),
                    },
                }
            }
        }
    }

    /// Value-mode completion with per-trigger caching (§8.2).
    ///
    /// * `trigger == true` (explicit invoke): always run a fresh query, store
    ///   the full candidate set in the value cache (overwriting any prior set
    ///   for the slot — i.e. invalidate-on-retrigger), and shape the items.
    /// * `trigger == false` (keystroke):
    ///   - cache hit → filter the cached rows by the typed prefix locally and
    ///     re-shape, with no database round-trip;
    ///   - cache miss → run a live query but do **not** populate the cache
    ///     (only explicit triggers seed it).
    ///
    /// The cache key is `"{profile}\u{1}{plan.sql}"`. The planned SQL binds the
    /// typed prefix as a parameter, so it is prefix-independent and identifies
    /// the slot exactly.
    async fn complete_values_cached(
        &self,
        req: &CompletionRequest,
        params: &ValueCompletionParams,
        snapshot: &ModelSnapshot,
        source: &Arc<DatabaseSource>,
    ) -> Result<Completions, pool::PoolError> {
        let resolution = match intellisense::resolve_value_slot(req, params, snapshot) {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Ok(Completions {
                    items: vec![],
                    truncated: false,
                    snapshot_generation: snapshot.generation,
                    slot_note: Some("no value slot at the cursor".into()),
                });
            }
            Err(note) => {
                return Ok(Completions {
                    items: vec![],
                    truncated: false,
                    snapshot_generation: snapshot.generation,
                    slot_note: Some(note),
                });
            }
        };
        let intellisense::ValueSlotResolution {
            plan,
            cache_plan,
            prefix,
            replace,
        } = resolution;
        let cache_key = format!("{}\u{1}{}", source.profile, cache_plan.sql);

        // Keystroke against a primed slot: filter the cached candidate set
        // locally and re-shape with the current replace span. No DB query.
        // Only `LIKE` plans permit local filtering; other match types compare
        // against the prefix, so a changed prefix must hit the database again.
        if !params.trigger {
            let tick = self.next_value_tick();
            let cached_filtered = {
                let mut cache = self.value_cache.lock();
                cache.get_mut(&cache_key).and_then(|cached| {
                    cached.plan.supports_cached_filter().then(|| {
                        cached.last_used = tick;
                        let filtered =
                            intellisense::filter_cached_rows(&cached.rows, &cached.plan, &prefix);
                        let items = intellisense::shape_value_items(
                            &filtered,
                            &cached.plan,
                            cached.dialect,
                            replace,
                        );
                        Completions {
                            items,
                            truncated: cached.truncated,
                            snapshot_generation: snapshot.generation,
                            slot_note: Some(cached.note.clone()),
                        }
                    })
                })
            };
            if let Some(completions) = cached_filtered {
                return Ok(completions);
            }
        }

        // Live query. An explicit trigger fetches the FULL candidate set
        // (empty-prefix `cache_plan`) so keystrokes can filter it locally; a
        // keystroke cache-miss runs the prefix-respecting `plan` directly.
        let query_plan = if params.trigger { &cache_plan } else { &plan };
        let mut conn = source.pool.acquire().await?;
        let rows = conn.query(&query_plan.sql, &query_plan.params).await?;
        let limit = params
            .limit
            .unwrap_or(intellisense::VALUE_LIMIT_DEFAULT)
            .min(intellisense::VALUE_LIMIT_CAP) as usize;
        let truncated = rows.len() >= limit;

        if params.trigger {
            // Seed/overwrite the cache for this slot (invalidate-on-retrigger).
            let tick = self.next_value_tick();
            let capacity = self.config.read().value_cache_capacity.max(1);
            let mut cache = self.value_cache.lock();
            cache.insert(
                cache_key.clone(),
                CachedValueSet {
                    rows: rows.clone(),
                    plan: cache_plan.clone(),
                    truncated,
                    note: cache_plan.note.clone(),
                    dialect: req.dialect,
                    last_used: tick,
                },
            );
            // Bounded LRU: drop the least-recently-used entries past capacity.
            while cache.len() > capacity {
                let Some(victim) = cache
                    .iter()
                    .filter(|(key, _)| *key != &cache_key)
                    .min_by_key(|(_, entry)| entry.last_used)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                cache.remove(&victim);
            }
        }

        // On an explicit trigger the user may already have typed a prefix; the
        // visible list is the cached full set filtered by it.
        let (display_rows, shape_plan): (std::borrow::Cow<'_, [Vec<Option<String>>]>, &_) =
            if params.trigger && !prefix.is_empty() {
                (
                    std::borrow::Cow::Owned(intellisense::filter_cached_rows(
                        &rows,
                        &cache_plan,
                        &prefix,
                    )),
                    &cache_plan,
                )
            } else if params.trigger {
                (std::borrow::Cow::Borrowed(rows.as_slice()), &cache_plan)
            } else {
                (std::borrow::Cow::Borrowed(rows.as_slice()), &plan)
            };
        let items =
            intellisense::shape_value_items(&display_rows, shape_plan, req.dialect, replace);
        Ok(Completions {
            items,
            truncated,
            snapshot_generation: snapshot.generation,
            slot_note: Some(query_plan.note.clone()),
        })
    }

    /// Current file-vs-database conflicts (drives pull/push UX).
    pub fn drift(&self) -> Vec<sqlly_lang::sqlmodel::SourceConflict> {
        self.snapshot().drift()
    }

    /// Validate a standalone tree (file/buffer) at tree scope without
    /// touching the snapshot — "is this file valid in itself".
    pub fn validate_tree_text(&self, text: &str, dialect: Dialect) -> Vec<Diagnostic> {
        let tree = parse(text, dialect);
        let mut diagnostics = validate::validate_tree(&tree, &self.mode());
        validate::position_diagnostics(text, &mut diagnostics);
        diagnostics
    }
}
