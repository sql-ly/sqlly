//! The encrypted notebook snapshot store (spec 5.2 "encrypted disk and
//! pinned snapshot policies, quotas, TTLs, LRU eviction,
//! clear-on-disconnect, secure removal; keys via macOS Keychain",
//! ADR-005, 12.3 "a policy-restricted user cannot retrieve another user's
//! cached result by changing IDs").
//!
//! Layout: `<root>/<scope id>/<entry name>.snap`, where the scope id and
//! the entry name are HMACs under a key derived from the cache master key,
//! so neither can be computed from notebook, cell or profile ids. Each file
//! is a plaintext header (magic, version, flags, stored/expiry times,
//! random 24-byte nonce) followed by the XChaCha20-Poly1305 encryption of
//! the payload, with the header, scope id and entry name as associated
//! data: a file copied into another scope or renamed fails to decrypt. The
//! payload is versioned (`SQNP`): JSON metadata naming its scope and entry
//! (checked again after decryption), the rows in the spill v2 row codec,
//! and a SHA-256 checksum. Anything that fails to decrypt or verify is
//! deleted and reported as [`GetOutcome::Discarded`].
//!
//! The master key lives in the OS credential store ([`KeyringCacheKey`],
//! service `SQLLY.notebook-cache`, account `cache-key`). Losing it makes
//! the cache unreadable, which only costs a re-run: the cache is never the
//! only copy of anything, so recovery is "discard and refetch" and
//! rotation is [`SnapshotStore::rotate_key`] (clear, then a new key).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlly_core::DisplayMessage;

use super::messages::{message, private_fs};
use super::scope::{CacheScope, ScopeKey, hmac_sha256, owner_tag};
use crate::notebook::{CellOutput, ConnectionIdentity};

pub const KEYRING_SERVICE: &str = "SQLLY.notebook-cache";
pub const KEYRING_ACCOUNT: &str = "cache-key";

const FILE_MAGIC: &[u8; 4] = b"SQNC";
const PAYLOAD_MAGIC: &[u8; 4] = b"SQNP";
const FILE_VERSION: u16 = 1;
const PAYLOAD_VERSION: u16 = 1;
const NONCE_LEN: usize = 24;
const HEADER_LEN: usize = 4 + 2 + 2 + 8 + 8 + NONCE_LEN;
const FLAG_PINNED: u16 = 1;
const EXTENSION: &str = "snap";
const OWNER_PREFIX: &str = "owner-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheError {
    /// No usable key (no credential store on this machine, or it refused).
    /// The host keeps outputs in memory only.
    KeyUnavailable {
        detail: String,
    },
    Io {
        detail: String,
    },
    TooLarge {
        bytes: u64,
        limit: u64,
    },
}

impl CacheError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            CacheError::KeyUnavailable { .. } => message(
                "notebook.security.cache.key_unavailable",
                "Saved notebook results can't be encrypted on this computer, so results are kept only until you close the notebook.",
                &[],
            ),
            CacheError::Io { detail } => message(
                "notebook.security.cache.io",
                "Could not read or save notebook results: {detail}",
                &[("detail", detail.clone())],
            ),
            CacheError::TooLarge { bytes, limit } => message(
                "notebook.security.cache.too_large",
                "This result is {bytes} bytes, more than the {limit}-byte limit for saved results, so it isn't saved.",
                &[("bytes", bytes.to_string()), ("limit", limit.to_string())],
            ),
        }
    }
}

impl From<std::io::Error> for CacheError {
    fn from(error: std::io::Error) -> Self {
        CacheError::Io {
            detail: error.to_string(),
        }
    }
}

/// 32 bytes of key material, wiped on drop.
#[derive(Clone)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    pub fn generate() -> MasterKey {
        use rand::Rng;
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        MasterKey(bytes)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> MasterKey {
        MasterKey(bytes)
    }

    fn encode(&self) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(self.0)
    }

    fn decode(text: &str) -> Option<MasterKey> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text.trim())
            .ok()?;
        Some(MasterKey(bytes.try_into().ok()?))
    }

    fn derive(&self, label: &[u8]) -> [u8; 32] {
        hmac_sha256(&self.0, &[label])
    }
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.0.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(..)")
    }
}

/// Where the cache master key is kept.
pub trait CacheKeyProvider: Send + Sync {
    /// The stored key, creating and storing a new one when there is none.
    fn load_or_create(&self) -> Result<MasterKey, CacheError>;
    /// Replace the stored key (rotation).
    fn store(&self, key: &MasterKey) -> Result<(), CacheError>;
}

/// The OS credential store (Keychain, Credential Manager, Secret Service).
pub struct KeyringCacheKey {
    service: String,
    account: String,
}

impl Default for KeyringCacheKey {
    fn default() -> Self {
        KeyringCacheKey {
            service: KEYRING_SERVICE.into(),
            account: KEYRING_ACCOUNT.into(),
        }
    }
}

impl KeyringCacheKey {
    fn entry(&self) -> Result<keyring::Entry, CacheError> {
        keyring::Entry::new(&self.service, &self.account).map_err(|e| CacheError::KeyUnavailable {
            detail: e.to_string(),
        })
    }
}

impl CacheKeyProvider for KeyringCacheKey {
    fn load_or_create(&self) -> Result<MasterKey, CacheError> {
        let entry = self.entry()?;
        match entry.get_password() {
            Ok(text) => match MasterKey::decode(&text) {
                Some(key) => Ok(key),
                // A damaged entry: start over (the old cache becomes
                // unreadable and is discarded on read).
                None => {
                    let key = MasterKey::generate();
                    self.store(&key)?;
                    Ok(key)
                }
            },
            Err(keyring::Error::NoEntry) => {
                let key = MasterKey::generate();
                self.store(&key)?;
                Ok(key)
            }
            Err(e) => Err(CacheError::KeyUnavailable {
                detail: e.to_string(),
            }),
        }
    }

    fn store(&self, key: &MasterKey) -> Result<(), CacheError> {
        self.entry()?
            .set_password(&key.encode())
            .map_err(|e| CacheError::KeyUnavailable {
                detail: e.to_string(),
            })
    }
}

/// An in-memory key, for tests and for hosts that keep the cache only for
/// the life of the process.
#[derive(Default)]
pub struct MemoryCacheKey(Mutex<Option<MasterKey>>);

impl MemoryCacheKey {
    pub fn with_key(key: MasterKey) -> MemoryCacheKey {
        MemoryCacheKey(Mutex::new(Some(key)))
    }
}

impl CacheKeyProvider for MemoryCacheKey {
    fn load_or_create(&self) -> Result<MasterKey, CacheError> {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Ok(slot.get_or_insert_with(MasterKey::generate).clone())
    }

    fn store(&self, key: &MasterKey) -> Result<(), CacheError> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(key.clone());
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotStoreConfig {
    pub root: PathBuf,
    /// Total size at which least recently used, unpinned entries go.
    pub max_total_bytes: u64,
    /// Larger outputs are not cached.
    pub max_entry_bytes: u64,
    /// Expiry for entries stored without their own TTL; `None` keeps them
    /// until evicted.
    pub default_ttl: Option<Duration>,
}

impl SnapshotStoreConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        SnapshotStoreConfig {
            root: root.into(),
            max_total_bytes: 512 * 1024 * 1024,
            max_entry_bytes: 64 * 1024 * 1024,
            default_ttl: Some(Duration::from_secs(7 * 24 * 3600)),
        }
    }

    /// `<data dir>/SQLLY/notebooks/cache`.
    pub fn default_root() -> PathBuf {
        crate::notebook::data_dir().join("cache")
    }
}

/// What an entry holds.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKey {
    /// A cell's output snapshot, keyed by its fingerprint.
    Cell { fingerprint: String },
    /// One cell's output from one saved run.
    Run { run_id: String, cell_id: String },
}

impl EntryKey {
    fn canonical(&self) -> String {
        match self {
            EntryKey::Cell { fingerprint } => format!("cell\u{1f}{fingerprint}"),
            EntryKey::Run { run_id, cell_id } => format!("run\u{1f}{run_id}\u{1f}{cell_id}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PutOptions {
    /// Overrides the store's default TTL.
    pub ttl: Option<Duration>,
    /// Kept through TTL expiry and LRU eviction (still cleared by
    /// clear/disconnect).
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CachedOutput {
    pub output: CellOutput,
    pub stored_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GetOutcome {
    Hit(Box<CachedOutput>),
    Miss,
    /// Past its TTL; removed.
    Expired,
    /// Failed to decrypt or verify (tampered, corrupt, from another scope
    /// or an older key); removed.
    Discarded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EvictionReport {
    pub expired: usize,
    pub evicted: usize,
    pub bytes_after: u64,
}

#[derive(Serialize, Deserialize)]
struct PayloadMeta {
    scope: String,
    entry: String,
    output: CellOutput,
}

struct Header {
    flags: u16,
    stored_at_ms: i64,
    expires_at_ms: i64,
    nonce: [u8; NONCE_LEN],
}

impl Header {
    fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[..4].copy_from_slice(FILE_MAGIC);
        out[4..6].copy_from_slice(&FILE_VERSION.to_le_bytes());
        out[6..8].copy_from_slice(&self.flags.to_le_bytes());
        out[8..16].copy_from_slice(&self.stored_at_ms.to_le_bytes());
        out[16..24].copy_from_slice(&self.expires_at_ms.to_le_bytes());
        out[24..].copy_from_slice(&self.nonce);
        out
    }

    fn decode(bytes: &[u8]) -> Option<Header> {
        let head = bytes.get(..HEADER_LEN)?;
        if &head[..4] != FILE_MAGIC || u16::from_le_bytes([head[4], head[5]]) != FILE_VERSION {
            return None;
        }
        Some(Header {
            flags: u16::from_le_bytes([head[6], head[7]]),
            stored_at_ms: i64::from_le_bytes(head[8..16].try_into().ok()?),
            expires_at_ms: i64::from_le_bytes(head[16..24].try_into().ok()?),
            nonce: head[24..].try_into().ok()?,
        })
    }

    fn expired(&self, now_ms: i64) -> bool {
        self.flags & FLAG_PINNED == 0 && self.expires_at_ms != 0 && now_ms >= self.expires_at_ms
    }
}

fn encode_payload(meta: &PayloadMeta, output: &CellOutput) -> Result<Vec<u8>, CacheError> {
    let json = serde_json::to_vec(meta).map_err(|e| CacheError::Io {
        detail: e.to_string(),
    })?;
    let mut out = Vec::with_capacity(json.len() + 64);
    out.extend_from_slice(PAYLOAD_MAGIC);
    out.extend_from_slice(&PAYLOAD_VERSION.to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&json);
    sqlly_core::spill::encode_rows(&output.rows, &mut out).map_err(|e| CacheError::Io {
        detail: e.to_string(),
    })?;
    let checksum = Sha256::digest(&out);
    out.extend_from_slice(&checksum);
    Ok(out)
}

fn decode_payload(bytes: &[u8]) -> Option<(PayloadMeta, CellOutput)> {
    let (body, checksum) = bytes.split_at_checked(bytes.len().checked_sub(32)?)?;
    if Sha256::digest(body).as_slice() != checksum {
        return None;
    }
    if body.get(..4)? != PAYLOAD_MAGIC
        || u16::from_le_bytes([*body.get(4)?, *body.get(5)?]) != PAYLOAD_VERSION
    {
        return None;
    }
    let json_len = u32::from_le_bytes(body.get(6..10)?.try_into().ok()?) as usize;
    let json = body.get(10..10usize.checked_add(json_len)?)?;
    let meta: PayloadMeta = serde_json::from_slice(json).ok()?;
    let rows = sqlly_core::spill::decode_rows(&body[10 + json_len..]).ok()?;
    let mut output = meta.output.clone();
    output.rows = Arc::new(rows);
    Some((meta, output))
}

fn mtime(now_ms: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(now_ms.max(0) as u64)
}

/// The encrypted, scoped snapshot store.
pub struct SnapshotStore {
    config: SnapshotStoreConfig,
    scope_key: ScopeKey,
    cipher: XChaCha20Poly1305,
    lock: Mutex<()>,
}

impl SnapshotStore {
    pub fn open(
        config: SnapshotStoreConfig,
        keys: &dyn CacheKeyProvider,
    ) -> Result<SnapshotStore, CacheError> {
        let master = keys.load_or_create()?;
        private_fs::create_private_dir(&config.root)?;
        let mut encrypt = master.derive(b"sqlly.notebook.cache.encrypt.v1");
        let cipher = XChaCha20Poly1305::new((&encrypt).into());
        encrypt.fill(0);
        Ok(SnapshotStore {
            scope_key: ScopeKey::from_bytes(master.derive(b"sqlly.notebook.cache.scope.v1")),
            cipher,
            config,
            lock: Mutex::new(()),
        })
    }

    pub fn config(&self) -> &SnapshotStoreConfig {
        &self.config
    }

    /// The scope for results fetched as `identity`.
    pub fn scope_for(&self, identity: &ConnectionIdentity) -> CacheScope {
        CacheScope::derive(identity, &self.scope_key)
    }

    /// The scope for in-process outputs of one notebook with no connection
    /// upstream.
    pub fn local_scope(&self, notebook_id: &str) -> CacheScope {
        CacheScope::local(notebook_id, &self.scope_key)
    }

    /// The scope for an output derived from several scopes.
    pub fn combined_scope(&self, scopes: &[CacheScope]) -> Option<CacheScope> {
        CacheScope::combine(scopes, &self.scope_key)
    }

    fn scope_dir(&self, scope: &CacheScope) -> PathBuf {
        self.config.root.join(scope.id())
    }

    fn entry_path(&self, scope: &CacheScope, entry: &EntryKey) -> PathBuf {
        self.scope_dir(scope).join(format!(
            "{}.{EXTENSION}",
            scope.entry_name(&entry.canonical(), &self.scope_key)
        ))
    }

    fn aad(header: &[u8], scope: &CacheScope, entry: &EntryKey) -> Vec<u8> {
        let entry = entry.canonical();
        let mut aad = Vec::with_capacity(header.len() + 64 + entry.len());
        aad.extend_from_slice(header);
        aad.extend_from_slice(scope.id().as_bytes());
        aad.push(0);
        aad.extend_from_slice(entry.as_bytes());
        aad
    }

    /// Encrypt and store `output` under `scope`. Returns the bytes written.
    pub fn put(
        &self,
        scope: &CacheScope,
        entry: &EntryKey,
        output: &CellOutput,
        options: PutOptions,
        now_ms: i64,
    ) -> Result<u64, CacheError> {
        let mut meta_output = output.clone();
        meta_output.rows = Arc::new(Vec::new());
        let meta = PayloadMeta {
            scope: scope.id().to_string(),
            entry: entry.canonical(),
            output: meta_output,
        };
        let payload = encode_payload(&meta, output)?;
        let size = (payload.len() + HEADER_LEN + 16) as u64;
        if size > self.config.max_entry_bytes {
            return Err(CacheError::TooLarge {
                bytes: size,
                limit: self.config.max_entry_bytes,
            });
        }
        let ttl = options.ttl.or(self.config.default_ttl);
        let header = Header {
            flags: if options.pinned { FLAG_PINNED } else { 0 },
            stored_at_ms: now_ms,
            expires_at_ms: match ttl {
                Some(ttl) if !options.pinned => now_ms.saturating_add(ttl.as_millis() as i64),
                _ => 0,
            },
            nonce: {
                use rand::Rng;
                let mut nonce = [0u8; NONCE_LEN];
                rand::rng().fill_bytes(&mut nonce);
                nonce
            },
        };
        let head = header.encode();
        let sealed = self
            .cipher
            .encrypt(
                XNonce::from_slice(&header.nonce),
                Payload {
                    msg: &payload,
                    aad: &Self::aad(&head, scope, entry),
                },
            )
            .map_err(|_| CacheError::Io {
                detail: "encryption failed".into(),
            })?;
        let mut file = Vec::with_capacity(HEADER_LEN + sealed.len());
        file.extend_from_slice(&head);
        file.extend_from_slice(&sealed);
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let dir = self.scope_dir(scope);
        private_fs::create_private_dir(&dir)?;
        for owner in scope.owners() {
            let marker = dir.join(format!("{OWNER_PREFIX}{owner}"));
            if !marker.exists() {
                private_fs::write_atomic(&marker, b"")?;
            }
        }
        let path = self.entry_path(scope, entry);
        private_fs::write_atomic(&path, &file)?;
        let _ = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(mtime(now_ms)));
        Ok(file.len() as u64)
    }

    /// The output stored under `scope` for `entry`, verified.
    pub fn get(
        &self,
        scope: &CacheScope,
        entry: &EntryKey,
        now_ms: i64,
    ) -> Result<GetOutcome, CacheError> {
        let path = self.entry_path(scope, entry);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(GetOutcome::Miss),
            Err(e) => return Err(e.into()),
        };
        let Some(header) = Header::decode(&bytes) else {
            private_fs::secure_remove(&path)?;
            return Ok(GetOutcome::Discarded);
        };
        if header.expired(now_ms) {
            private_fs::secure_remove(&path)?;
            return Ok(GetOutcome::Expired);
        }
        let opened = self.cipher.decrypt(
            XNonce::from_slice(&header.nonce),
            Payload {
                msg: &bytes[HEADER_LEN..],
                aad: &Self::aad(&bytes[..HEADER_LEN], scope, entry),
            },
        );
        let verified = opened
            .ok()
            .and_then(|plain| decode_payload(&plain))
            .filter(|(meta, _)| meta.scope == scope.id() && meta.entry == entry.canonical());
        let Some((_, output)) = verified else {
            private_fs::secure_remove(&path)?;
            return Ok(GetOutcome::Discarded);
        };
        let _ = std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|f| f.set_modified(mtime(now_ms)));
        Ok(GetOutcome::Hit(Box::new(CachedOutput {
            output,
            stored_at_ms: header.stored_at_ms,
            expires_at_ms: (header.expires_at_ms != 0).then_some(header.expires_at_ms),
            pinned: header.flags & FLAG_PINNED != 0,
        })))
    }

    pub fn remove(&self, scope: &CacheScope, entry: &EntryKey) -> Result<bool, CacheError> {
        let path = self.entry_path(scope, entry);
        let existed = path.exists();
        private_fs::secure_remove(&path)?;
        Ok(existed)
    }

    /// Scope directories under the root (well-formed names only; anything
    /// else in the root is left alone).
    fn scope_dirs(&self) -> Vec<PathBuf> {
        let Ok(read) = std::fs::read_dir(&self.config.root) else {
            return Vec::new();
        };
        read.filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .and_then(CacheScope::from_id)
                    .is_some()
            })
            .map(|e| e.path())
            .collect()
    }

    fn remove_scope_dir(dir: &Path) -> Result<usize, CacheError> {
        let mut removed = 0;
        if let Ok(read) = std::fs::read_dir(dir) {
            for entry in read.filter_map(Result::ok) {
                let path = entry.path();
                if entry.file_type().is_ok_and(|t| t.is_file()) {
                    if path.extension().and_then(|e| e.to_str()) == Some(EXTENSION) {
                        removed += 1;
                    }
                    private_fs::secure_remove(&path)?;
                }
            }
        }
        let _ = std::fs::remove_dir(dir);
        Ok(removed)
    }

    /// Remove every entry in `scope`. Returns entries removed.
    pub fn clear_scope(&self, scope: &CacheScope) -> Result<usize, CacheError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        Self::remove_scope_dir(&self.scope_dir(scope))
    }

    /// Remove every scope holding results from `profile_id` (clear on
    /// disconnect), whatever login, database or policy they were fetched
    /// under, including combined scopes that used it.
    pub fn clear_profile(&self, profile_id: &str) -> Result<usize, CacheError> {
        let marker = format!("{OWNER_PREFIX}{}", owner_tag(profile_id, &self.scope_key));
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut removed = 0;
        for dir in self.scope_dirs() {
            if dir.join(&marker).exists() {
                removed += Self::remove_scope_dir(&dir)?;
            }
        }
        Ok(removed)
    }

    /// Remove everything.
    pub fn clear_all(&self) -> Result<usize, CacheError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut removed = 0;
        for dir in self.scope_dirs() {
            removed += Self::remove_scope_dir(&dir)?;
        }
        Ok(removed)
    }

    /// Discard the cache and switch to a new master key.
    pub fn rotate_key(self, keys: &dyn CacheKeyProvider) -> Result<SnapshotStore, CacheError> {
        self.clear_all()?;
        keys.store(&MasterKey::generate())?;
        SnapshotStore::open(self.config.clone(), keys)
    }

    fn entries(&self) -> Vec<(PathBuf, u64, SystemTime, Option<Header>)> {
        let mut out = Vec::new();
        for dir in self.scope_dirs() {
            let Ok(read) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in read.filter_map(Result::ok) {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                let header = std::fs::File::open(&path).ok().and_then(|mut f| {
                    use std::io::Read;
                    let mut head = [0u8; HEADER_LEN];
                    f.read_exact(&mut head).ok()?;
                    Header::decode(&head)
                });
                out.push((
                    path,
                    meta.len(),
                    meta.modified().unwrap_or(UNIX_EPOCH),
                    header,
                ));
            }
        }
        out
    }

    /// Bytes used by cache entries.
    pub fn usage(&self) -> u64 {
        self.entries().iter().map(|e| e.1).sum()
    }

    /// Drop expired entries, then least recently used unpinned entries until
    /// the store fits `max_total_bytes`. Empty scope directories go too.
    pub fn enforce_limits(&self, now_ms: i64) -> Result<EvictionReport, CacheError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut report = EvictionReport::default();
        let mut live = Vec::new();
        for (path, size, used, header) in self.entries() {
            match &header {
                None => private_fs::secure_remove(&path)?,
                Some(h) if h.expired(now_ms) => {
                    private_fs::secure_remove(&path)?;
                    report.expired += 1;
                }
                Some(h) => live.push((path, size, used, h.flags & FLAG_PINNED != 0)),
            }
        }
        let mut total: u64 = live.iter().map(|e| e.1).sum();
        live.sort_by_key(|e| e.2);
        for (path, size, _, pinned) in &live {
            if total <= self.config.max_total_bytes {
                break;
            }
            if *pinned {
                continue;
            }
            private_fs::secure_remove(path)?;
            total -= size;
            report.evicted += 1;
        }
        for dir in self.scope_dirs() {
            let has_entries = std::fs::read_dir(&dir).is_ok_and(|mut r| {
                r.any(|e| {
                    e.is_ok_and(|e| {
                        e.path().extension().and_then(|x| x.to_str()) == Some(EXTENSION)
                    })
                })
            });
            if !has_entries {
                Self::remove_scope_dir(&dir)?;
            }
        }
        report.bytes_after = total;
        Ok(report)
    }

    /// Ids of the scopes on disk (for diagnostics and tests).
    pub fn scope_ids(&self) -> BTreeSet<String> {
        self.scope_dirs()
            .iter()
            .filter_map(|d| d.file_name()?.to_str().map(str::to_string))
            .collect()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::notebook::{Completeness, Fingerprint, Provenance};
    use crate::notebook_security::scope::tests::identity;
    use sqlly_core::{CellValue, ResultColumn};

    pub(crate) const MARKER: &str = "PLAINTEXT-MARKER-4d1f";

    pub(crate) fn output(rows: usize) -> CellOutput {
        CellOutput {
            columns: vec![
                ResultColumn {
                    name: "id".into(),
                    data_type: "int".into(),
                    nullable: false,
                },
                ResultColumn {
                    name: "note".into(),
                    data_type: "nvarchar".into(),
                    nullable: true,
                },
            ],
            rows: Arc::new(
                (0..rows)
                    .map(|i| {
                        vec![
                            CellValue::I64(i as i64),
                            if i % 3 == 0 {
                                CellValue::Null
                            } else {
                                CellValue::String(format!("{MARKER} {i} ✓"))
                            },
                        ]
                    })
                    .collect(),
            ),
            completeness: Completeness::Complete,
            row_count: rows as u64,
            elapsed_ms: 12,
            run_id: "r_1".into(),
            fingerprint: Fingerprint("fp".into()),
            provenance: Provenance {
                connection_label: "Orders".into(),
                database: Some("sales".into()),
                ..Provenance::default()
            },
            freshness: Default::default(),
        }
    }

    pub(crate) fn store(dir: &Path) -> SnapshotStore {
        SnapshotStore::open(
            SnapshotStoreConfig::new(dir.join("cache")),
            &MemoryCacheKey::with_key(MasterKey::from_bytes([9; 32])),
        )
        .unwrap()
    }

    pub(crate) fn cell(fp: &str) -> EntryKey {
        EntryKey::Cell {
            fingerprint: fp.into(),
        }
    }

    fn hit(outcome: GetOutcome) -> CachedOutput {
        match outcome {
            GetOutcome::Hit(hit) => *hit,
            other => panic!("expected a hit, got {other:?}"),
        }
    }

    fn all_files(dir: &Path) -> Vec<PathBuf> {
        let mut out = vec![];
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    out.push(e.path());
                }
            }
        }
        out
    }

    #[test]
    fn outputs_round_trip_encrypted_with_rows_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let scope = store.scope_for(&identity("p-1", "alice"));
        let original = output(250);
        store
            .put(&scope, &cell("fp"), &original, PutOptions::default(), 1_000)
            .unwrap();
        let got = hit(store.get(&scope, &cell("fp"), 2_000).unwrap());
        assert_eq!(got.output, original);
        assert_eq!(got.stored_at_ms, 1_000);
        assert!(got.expires_at_ms.is_some());
        for file in all_files(dir.path()) {
            let bytes = std::fs::read(&file).unwrap();
            assert!(
                !String::from_utf8_lossy(&bytes).contains(MARKER),
                "{file:?}"
            );
            assert!(
                !String::from_utf8_lossy(&bytes).contains("sales"),
                "{file:?}"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        let names: Vec<String> = all_files(dir.path())
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| !n.contains("p-1") && !n.contains("alice") && !n.contains("fp."))
        );
    }

    #[test]
    fn each_put_uses_a_fresh_nonce() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let scope = store.scope_for(&identity("p-1", "alice"));
        store
            .put(&scope, &cell("a"), &output(3), PutOptions::default(), 0)
            .unwrap();
        let first = std::fs::read(store.entry_path(&scope, &cell("a"))).unwrap();
        store
            .put(&scope, &cell("a"), &output(3), PutOptions::default(), 0)
            .unwrap();
        let second = std::fs::read(store.entry_path(&scope, &cell("a"))).unwrap();
        assert_ne!(first[24..HEADER_LEN], second[24..HEADER_LEN]);
        assert_ne!(first[HEADER_LEN..], second[HEADER_LEN..]);
    }

    #[test]
    fn other_principals_never_see_a_scope_and_copied_files_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let alice = store.scope_for(&identity("p-1", "alice"));
        let bob = store.scope_for(&identity("p-1", "bob"));
        store
            .put(&alice, &cell("fp"), &output(5), PutOptions::default(), 0)
            .unwrap();
        assert_eq!(store.get(&bob, &cell("fp"), 0).unwrap(), GetOutcome::Miss);
        // Plant alice's file where bob's entry would be.
        std::fs::create_dir_all(store.scope_dir(&bob)).unwrap();
        std::fs::copy(
            store.entry_path(&alice, &cell("fp")),
            store.entry_path(&bob, &cell("fp")),
        )
        .unwrap();
        assert_eq!(
            store.get(&bob, &cell("fp"), 0).unwrap(),
            GetOutcome::Discarded
        );
        assert!(!store.entry_path(&bob, &cell("fp")).exists());
        // Renamed within the scope (another entry's name) fails too.
        std::fs::copy(
            store.entry_path(&alice, &cell("fp")),
            store.entry_path(&alice, &cell("other")),
        )
        .unwrap();
        assert_eq!(
            store.get(&alice, &cell("other"), 0).unwrap(),
            GetOutcome::Discarded
        );
        assert!(matches!(
            store.get(&alice, &cell("fp"), 0).unwrap(),
            GetOutcome::Hit(_)
        ));
    }

    #[test]
    fn a_different_key_reads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let a = store(dir.path());
        let scope = a.scope_for(&identity("p-1", "alice"));
        a.put(&scope, &cell("fp"), &output(5), PutOptions::default(), 0)
            .unwrap();
        let b = SnapshotStore::open(
            SnapshotStoreConfig::new(dir.path().join("cache")),
            &MemoryCacheKey::with_key(MasterKey::from_bytes([1; 32])),
        )
        .unwrap();
        let scope_b = b.scope_for(&identity("p-1", "alice"));
        assert_ne!(scope_b, scope);
        assert_eq!(b.get(&scope_b, &cell("fp"), 0).unwrap(), GetOutcome::Miss);
        // Even pointed at the old scope directory, the old file fails AEAD.
        assert_eq!(
            b.get(&scope, &cell("fp"), 0).unwrap(),
            GetOutcome::Miss,
            "entry names are keyed too"
        );
        std::fs::copy(
            a.entry_path(&scope, &cell("fp")),
            b.entry_path(&scope, &cell("fp")),
        )
        .unwrap();
        assert_eq!(
            b.get(&scope, &cell("fp"), 0).unwrap(),
            GetOutcome::Discarded
        );
    }

    #[test]
    fn tampering_anywhere_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let scope = store.scope_for(&identity("p-1", "alice"));
        store
            .put(&scope, &cell("fp"), &output(20), PutOptions::default(), 0)
            .unwrap();
        let path = store.entry_path(&scope, &cell("fp"));
        let pristine = std::fs::read(&path).unwrap();
        let positions = [
            0,
            5,
            7,
            9,
            17,
            30,
            HEADER_LEN,
            HEADER_LEN + 1,
            pristine.len() / 2,
            pristine.len() - 1,
        ];
        for at in positions {
            let mut bytes = pristine.clone();
            bytes[at] ^= 0x01;
            std::fs::write(&path, &bytes).unwrap();
            let outcome = store.get(&scope, &cell("fp"), 0).unwrap();
            assert!(
                matches!(outcome, GetOutcome::Discarded | GetOutcome::Expired),
                "byte {at}: {outcome:?}"
            );
            assert!(!path.exists(), "byte {at}");
        }
        std::fs::write(&path, &pristine[..pristine.len() - 5]).unwrap();
        assert_eq!(
            store.get(&scope, &cell("fp"), 0).unwrap(),
            GetOutcome::Discarded
        );
        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            store.get(&scope, &cell("fp"), 0).unwrap(),
            GetOutcome::Discarded
        );
    }

    #[test]
    fn ttl_pinning_and_lru_eviction() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = SnapshotStoreConfig::new(dir.path().join("cache"));
        config.default_ttl = Some(Duration::from_secs(60));
        let store = SnapshotStore::open(config, &MemoryCacheKey::default()).unwrap();
        let scope = store.scope_for(&identity("p-1", "alice"));
        store
            .put(&scope, &cell("ttl"), &output(2), PutOptions::default(), 0)
            .unwrap();
        store
            .put(
                &scope,
                &cell("pin"),
                &output(2),
                PutOptions {
                    pinned: true,
                    ..Default::default()
                },
                0,
            )
            .unwrap();
        assert!(matches!(
            store.get(&scope, &cell("ttl"), 59_000).unwrap(),
            GetOutcome::Hit(_)
        ));
        assert_eq!(
            store.get(&scope, &cell("ttl"), 60_000).unwrap(),
            GetOutcome::Expired
        );
        assert!(matches!(
            store.get(&scope, &cell("pin"), 10_000_000).unwrap(),
            GetOutcome::Hit(_)
        ));

        let dir = tempfile::tempdir().unwrap();
        let mut config = SnapshotStoreConfig::new(dir.path().join("cache"));
        config.default_ttl = None;
        let probe = SnapshotStore::open(config.clone(), &MemoryCacheKey::default()).unwrap();
        let s = probe.scope_for(&identity("p", "u"));
        let size = probe
            .put(&s, &cell("probe"), &output(10), PutOptions::default(), 0)
            .unwrap();
        probe.clear_all().unwrap();
        config.max_total_bytes = size * 3;
        let store = SnapshotStore::open(config, &MemoryCacheKey::default()).unwrap();
        let scope = store.scope_for(&identity("p-1", "alice"));
        store
            .put(
                &scope,
                &cell("pinned-old"),
                &output(10),
                PutOptions {
                    pinned: true,
                    ..Default::default()
                },
                1,
            )
            .unwrap();
        for (i, name) in ["a", "b", "c", "d"].iter().enumerate() {
            store
                .put(
                    &scope,
                    &cell(name),
                    &output(10),
                    PutOptions::default(),
                    10 + i as i64,
                )
                .unwrap();
        }
        // Touch "a" so "b" is the least recently used unpinned entry.
        assert!(matches!(
            store.get(&scope, &cell("a"), 100).unwrap(),
            GetOutcome::Hit(_)
        ));
        let report = store.enforce_limits(200).unwrap();
        assert_eq!(report.evicted, 2);
        assert!(report.bytes_after <= size * 3);
        assert_eq!(
            store.get(&scope, &cell("b"), 300).unwrap(),
            GetOutcome::Miss
        );
        assert_eq!(
            store.get(&scope, &cell("c"), 300).unwrap(),
            GetOutcome::Miss
        );
        for kept in ["pinned-old", "a", "d"] {
            assert!(
                matches!(
                    store.get(&scope, &cell(kept), 300).unwrap(),
                    GetOutcome::Hit(_)
                ),
                "{kept}"
            );
        }
    }

    #[test]
    fn oversized_outputs_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = SnapshotStoreConfig::new(dir.path().join("cache"));
        config.max_entry_bytes = 1_000;
        let store = SnapshotStore::open(config, &MemoryCacheKey::default()).unwrap();
        let scope = store.scope_for(&identity("p-1", "alice"));
        assert!(matches!(
            store.put(&scope, &cell("big"), &output(500), PutOptions::default(), 0),
            Err(CacheError::TooLarge { .. })
        ));
        assert_eq!(store.usage(), 0);
    }

    #[test]
    fn clear_on_disconnect_clear_scope_and_clear_all() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let a1 = store.scope_for(&identity("p-1", "alice"));
        let a2 = store.scope_for(&identity("p-1", "admin"));
        let other = store.scope_for(&identity("p-2", "alice"));
        let joined = store.combined_scope(&[a1.clone(), other.clone()]).unwrap();
        for scope in [&a1, &a2, &other, &joined] {
            store
                .put(scope, &cell("fp"), &output(2), PutOptions::default(), 0)
                .unwrap();
        }
        assert_eq!(store.scope_ids().len(), 4);
        assert_eq!(store.clear_profile("p-1").unwrap(), 3);
        assert_eq!(store.scope_ids(), [other.id().to_string()].into());
        assert!(matches!(
            store.get(&other, &cell("fp"), 0).unwrap(),
            GetOutcome::Hit(_)
        ));
        store
            .put(&a1, &cell("fp"), &output(2), PutOptions::default(), 0)
            .unwrap();
        assert_eq!(store.clear_scope(&a1).unwrap(), 1);
        std::fs::write(dir.path().join("cache").join("unrelated.txt"), "keep").unwrap();
        assert_eq!(store.clear_all().unwrap(), 1);
        assert!(store.scope_ids().is_empty());
        assert!(dir.path().join("cache").join("unrelated.txt").exists());
    }

    #[test]
    fn rotating_the_key_discards_everything() {
        let dir = tempfile::tempdir().unwrap();
        let keys = MemoryCacheKey::default();
        let store =
            SnapshotStore::open(SnapshotStoreConfig::new(dir.path().join("cache")), &keys).unwrap();
        let scope = store.scope_for(&identity("p-1", "alice"));
        store
            .put(&scope, &cell("fp"), &output(2), PutOptions::default(), 0)
            .unwrap();
        let store = store.rotate_key(&keys).unwrap();
        assert!(store.scope_ids().is_empty());
        let scope2 = store.scope_for(&identity("p-1", "alice"));
        assert_ne!(scope2, scope);
    }

    #[test]
    fn keyring_provider_creates_once_and_reuses() {
        keyring::mock::install();
        let provider = KeyringCacheKey {
            service: "SQLLY.notebook-cache.test".into(),
            account: "cache-key-create".into(),
        };
        let first = provider.load_or_create().unwrap();
        let second = provider.load_or_create().unwrap();
        assert_eq!(first.0, second.0);
        keyring::Entry::new("SQLLY.notebook-cache.test", "cache-key-create")
            .unwrap()
            .set_password("not base64 !!")
            .unwrap();
        let replaced = provider.load_or_create().unwrap();
        assert_ne!(replaced.0, first.0);
        assert_eq!(KeyringCacheKey::default().service, KEYRING_SERVICE);
        assert_eq!(KeyringCacheKey::default().account, KEYRING_ACCOUNT);
    }
}
