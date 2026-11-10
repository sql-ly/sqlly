//! OpenStreetMap tiles: the consent gate, the on-disk cache layout, and a
//! small request scheduler that only ever asks for visible tiles, at most
//! two at a time, as the OSM tile usage policy asks.
//!
//! The scheduler ([`TileStore`]) is generic over the decoded image type and
//! takes its fetcher as a value, so tests drive it without a network or a
//! GPU. The real fetcher (native only) checks the disk cache first and
//! sends an identifying `User-Agent`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use super::projection::TileKey;
use crate::preferences::MapTileChoice;

/// The OpenStreetMap standard tile layer.
pub const TILE_URL_TEMPLATE: &str = "https://tile.openstreetmap.org/{z}/{x}/{y}.png";
/// Where the attribution links (the license and copyright page).
pub const ATTRIBUTION_URL: &str = "https://www.openstreetmap.org/copyright";
/// Concurrent tile downloads (the usage policy asks for few).
pub const MAX_CONCURRENT_FETCHES: usize = 2;
/// How long a cached tile is used before it is downloaded again. The OSM
/// policy asks clients to cache for at least a week.
pub const CACHE_MAX_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// Decoded tiles kept in memory per map window.
pub const MEMORY_TILES: usize = 160;

/// The download URL for one tile.
pub fn tile_url(key: TileKey) -> String {
    TILE_URL_TEMPLATE
        .replace("{z}", &key.z.to_string())
        .replace("{x}", &key.x.to_string())
        .replace("{y}", &key.y.to_string())
}

/// The identifying `User-Agent` the tile server sees.
pub fn user_agent() -> String {
    format!(
        "SQLly/{} (+https://sqlly.app; map view)",
        crate::about::APP_VERSION
    )
}

/// A tile's path inside the cache folder: `z/x/y.png`.
pub fn cache_relative_path(key: TileKey) -> PathBuf {
    PathBuf::from(key.z.to_string())
        .join(key.x.to_string())
        .join(format!("{}.png", key.y))
}

/// The tile cache folder: `map-tiles` under the app data folder.
#[cfg(not(target_arch = "wasm32"))]
pub fn cache_root() -> PathBuf {
    crate::preferences::preferences_path().join("map-tiles")
}

/// Whether a cached tile of age `age` can be used without downloading.
pub fn cache_is_fresh(age: Duration) -> bool {
    age <= CACHE_MAX_AGE
}

/// Why a tile did not arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    /// No connection to the tile server (and nothing cached).
    Offline,
    /// The server answered with this status.
    Http(u16),
    /// The bytes were not a usable image.
    BadImage,
}

pub type FetchFuture = Pin<Box<dyn Future<Output = Result<Vec<u8>, FetchError>> + Send>>;
/// Fetches one tile's PNG bytes. Injected so tests can count requests.
pub type TileFetcher = Arc<dyn Fn(TileKey) -> FetchFuture + Send + Sync>;

/// Visible-tile scheduler and in-memory tile cache for one map.
pub struct TileStore<T> {
    choice: MapTileChoice,
    fetcher: TileFetcher,
    in_flight: HashSet<TileKey>,
    ready: HashMap<TileKey, (T, u64)>,
    failed: HashSet<TileKey>,
    evicted: Vec<T>,
    offline: bool,
    clock: u64,
}

impl<T: Clone> TileStore<T> {
    pub fn new(choice: MapTileChoice, fetcher: TileFetcher) -> Self {
        Self {
            choice,
            fetcher,
            in_flight: HashSet::new(),
            ready: HashMap::new(),
            failed: HashSet::new(),
            evicted: Vec::new(),
            offline: false,
            clock: 0,
        }
    }

    pub fn choice(&self) -> MapTileChoice {
        self.choice
    }

    pub fn set_choice(&mut self, choice: MapTileChoice) {
        self.choice = choice;
        if choice == MapTileChoice::Allowed {
            self.offline = false;
            self.failed.clear();
        }
    }

    /// True once a download failed for lack of a connection; the map then
    /// shows the plain sketch until [`Self::retry`].
    pub fn is_offline(&self) -> bool {
        self.offline
    }

    pub fn retry(&mut self) {
        self.offline = false;
        self.failed.clear();
    }

    /// Whether tiles are drawn at all (allowed and not offline).
    pub fn active(&self) -> bool {
        self.choice == MapTileChoice::Allowed && !self.offline
    }

    /// Start downloads for the visible tiles that aren't loaded, loading,
    /// or known-bad, up to the concurrency limit. Nothing is ever requested
    /// before the user allows tiles, and nothing outside `visible` is ever
    /// requested (no prefetching).
    pub fn start_fetches(&mut self, visible: &[TileKey]) -> Vec<(TileKey, FetchFuture)> {
        self.clock += 1;
        for key in visible {
            if let Some(entry) = self.ready.get_mut(key) {
                entry.1 = self.clock;
            }
        }
        if !self.active() {
            return Vec::new();
        }
        let mut started = Vec::new();
        for &key in visible {
            if self.in_flight.len() >= MAX_CONCURRENT_FETCHES {
                break;
            }
            if self.ready.contains_key(&key)
                || self.in_flight.contains(&key)
                || self.failed.contains(&key)
            {
                continue;
            }
            self.in_flight.insert(key);
            started.push((key, (self.fetcher)(key)));
        }
        started
    }

    /// Record a finished download (decoded, or the reason it failed).
    pub fn finish(&mut self, key: TileKey, result: Result<T, FetchError>) {
        self.in_flight.remove(&key);
        match result {
            Ok(image) => {
                self.clock += 1;
                self.ready.insert(key, (image, self.clock));
                while self.ready.len() > MEMORY_TILES {
                    let Some(oldest) = self
                        .ready
                        .iter()
                        .min_by_key(|(_, (_, used))| *used)
                        .map(|(k, _)| *k)
                    else {
                        break;
                    };
                    if let Some((image, _)) = self.ready.remove(&oldest) {
                        self.evicted.push(image);
                    }
                }
            }
            Err(FetchError::Offline) => self.offline = true,
            Err(_) => {
                self.failed.insert(key);
            }
        }
    }

    pub fn get(&self, key: &TileKey) -> Option<&T> {
        self.ready.get(key).map(|(image, _)| image)
    }

    /// The nearest loaded ancestor of `key` (up to 4 levels), with the
    /// sub-rectangle of it that `key` covers as `(x, y, size)` fractions —
    /// drawn scaled while the sharper tile loads.
    pub fn ancestor(&self, key: TileKey) -> Option<(&T, f64, f64, f64)> {
        for up in 1..=key.z.min(4) {
            let parent = TileKey {
                z: key.z - up,
                x: key.x >> up,
                y: key.y >> up,
            };
            if let Some(image) = self.get(&parent) {
                let span = (1u32 << up) as f64;
                let fx = (key.x - (parent.x << up)) as f64 / span;
                let fy = (key.y - (parent.y << up)) as f64 / span;
                return Some((image, fx, fy, 1.0 / span));
            }
        }
        None
    }

    /// Images dropped from memory since the last call (so the window can
    /// release their GPU atlas entries).
    pub fn take_evicted(&mut self) -> Vec<T> {
        std::mem::take(&mut self.evicted)
    }
}

/// A fetcher that never fetches (the browser build, and tests).
#[cfg(any(test, target_arch = "wasm32"))]
pub fn no_fetcher() -> TileFetcher {
    Arc::new(|_| Box::pin(async { Err(FetchError::Offline) }))
}

/// The real fetcher: fresh disk cache first, then the tile server with an
/// identifying User-Agent; a successful download is written back to the
/// cache, and a stale cached tile is still used when the server can't be
/// reached. Runs on the app's tokio runtime.
#[cfg(not(target_arch = "wasm32"))]
pub fn osm_fetcher() -> TileFetcher {
    Arc::new(|key| Box::pin(fetch_tile(cache_root(), key)))
}

#[cfg(not(target_arch = "wasm32"))]
fn http_client() -> Option<reqwest::Client> {
    static CLIENT: std::sync::OnceLock<Option<reqwest::Client>> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
            reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .user_agent(user_agent())
                .build()
                .ok()
        })
        .clone()
}

#[cfg(not(target_arch = "wasm32"))]
async fn fetch_tile(root: PathBuf, key: TileKey) -> Result<Vec<u8>, FetchError> {
    let path = root.join(cache_relative_path(key));
    let cached = std::fs::metadata(&path).ok().and_then(|meta| {
        let age = meta.modified().ok()?.elapsed().unwrap_or_default();
        Some(cache_is_fresh(age))
    });
    if cached == Some(true)
        && let Ok(bytes) = std::fs::read(&path)
    {
        return Ok(bytes);
    }
    let stale = || std::fs::read(&path).map_err(|_| FetchError::Offline);
    let Some(client) = http_client() else {
        return stale();
    };
    let response = match client.get(tile_url(key)).send().await {
        Ok(r) => r,
        Err(_) => return stale(),
    };
    let status = response.status();
    if !status.is_success() {
        return stale().map_err(|_| FetchError::Http(status.as_u16()));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| FetchError::Offline)?
        .to_vec();
    if let Some(dir) = path.parent()
        && std::fs::create_dir_all(dir).is_ok()
    {
        let _ = crate::fs_safe::write_atomic(&path, &bytes);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counting() -> (TileFetcher, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let fetcher: TileFetcher = Arc::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(vec![1, 2, 3]) })
        });
        (fetcher, calls)
    }

    fn keys(n: u32) -> Vec<TileKey> {
        (0..n).map(|x| TileKey { z: 3, x, y: 1 }).collect()
    }

    #[test]
    fn nothing_is_requested_before_consent_or_after_declining() {
        let (fetcher, calls) = counting();
        let mut store: TileStore<u8> = TileStore::new(MapTileChoice::Ask, fetcher);
        assert!(store.start_fetches(&keys(4)).is_empty());
        store.set_choice(MapTileChoice::Declined);
        assert!(store.start_fetches(&keys(4)).is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        store.set_choice(MapTileChoice::Allowed);
        assert_eq!(store.start_fetches(&keys(4)).len(), MAX_CONCURRENT_FETCHES);
        assert_eq!(calls.load(Ordering::SeqCst), MAX_CONCURRENT_FETCHES);
    }

    #[test]
    fn concurrency_is_capped_and_finished_tiles_are_not_refetched() {
        let (fetcher, calls) = counting();
        let mut store: TileStore<u8> = TileStore::new(MapTileChoice::Allowed, fetcher);
        let visible = keys(5);
        let first: Vec<_> = store
            .start_fetches(&visible)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(first, visible[..2]);
        // Still two in flight: nothing more starts.
        assert!(store.start_fetches(&visible).is_empty());
        store.finish(first[0], Ok(7));
        store.finish(first[1], Err(FetchError::Http(404)));
        let next: Vec<_> = store
            .start_fetches(&visible)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(next, visible[2..4]);
        assert_eq!(store.get(&first[0]), Some(&7));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn only_visible_tiles_are_requested() {
        let (fetcher, _) = counting();
        let mut store: TileStore<u8> = TileStore::new(MapTileChoice::Allowed, fetcher);
        let visible = [TileKey { z: 5, x: 9, y: 9 }];
        let started: Vec<_> = store
            .start_fetches(&visible)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        assert_eq!(started, visible);
    }

    #[test]
    fn offline_stops_requests_until_retry() {
        let (fetcher, calls) = counting();
        let mut store: TileStore<u8> = TileStore::new(MapTileChoice::Allowed, fetcher);
        let visible = keys(3);
        let (k, _) = store.start_fetches(&visible).remove(0);
        store.finish(k, Err(FetchError::Offline));
        assert!(store.is_offline() && !store.active());
        let before = calls.load(Ordering::SeqCst);
        assert!(store.start_fetches(&visible).is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), before);
        store.retry();
        assert!(!store.start_fetches(&visible).is_empty());
    }

    #[test]
    fn memory_cache_evicts_least_recently_used() {
        let mut store: TileStore<u32> = TileStore::new(MapTileChoice::Allowed, no_fetcher());
        for x in 0..MEMORY_TILES as u32 {
            store.finish(TileKey { z: 8, x, y: 0 }, Ok(x));
        }
        // Touch tile 0 so tile 1 becomes the oldest.
        store.start_fetches(&[TileKey { z: 8, x: 0, y: 0 }]);
        store.finish(TileKey { z: 8, x: 999, y: 0 }, Ok(999));
        assert_eq!(store.take_evicted(), vec![1]);
        assert!(store.get(&TileKey { z: 8, x: 0, y: 0 }).is_some());
        assert!(store.take_evicted().is_empty());
    }

    #[test]
    fn ancestor_tile_stands_in_while_loading() {
        let mut store: TileStore<u8> = TileStore::new(MapTileChoice::Allowed, no_fetcher());
        store.finish(TileKey { z: 2, x: 1, y: 2 }, Ok(5));
        let (img, fx, fy, size) = store.ancestor(TileKey { z: 4, x: 7, y: 9 }).unwrap();
        assert_eq!(*img, 5);
        assert_eq!((fx, fy, size), (0.75, 0.25, 0.25));
        assert!(store.ancestor(TileKey { z: 4, x: 0, y: 0 }).is_none());
    }

    #[test]
    fn urls_cache_paths_and_freshness() {
        let key = TileKey {
            z: 12,
            x: 2047,
            y: 1361,
        };
        assert_eq!(
            tile_url(key),
            "https://tile.openstreetmap.org/12/2047/1361.png"
        );
        assert_eq!(
            cache_relative_path(key),
            PathBuf::from("12").join("2047").join("1361.png")
        );
        assert!(cache_is_fresh(Duration::from_secs(3600)));
        assert!(!cache_is_fresh(CACHE_MAX_AGE + Duration::from_secs(1)));
        let ua = user_agent();
        assert!(ua.starts_with("SQLly/") && ua.contains(crate::about::APP_VERSION));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn fresh_disk_cache_answers_without_the_network() {
        let dir = tempfile::tempdir().unwrap();
        let key = TileKey { z: 1, x: 0, y: 1 };
        let path = dir.path().join(cache_relative_path(key));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"png").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let bytes = rt
            .block_on(fetch_tile(dir.path().to_path_buf(), key))
            .unwrap();
        assert_eq!(bytes, b"png");
    }
}
