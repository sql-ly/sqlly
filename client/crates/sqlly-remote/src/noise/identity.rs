//! Device static-key identity: a persistent X25519 keypair whose private half
//! lives in the OS keyring (or an owner-only file store on hosts without one),
//! abstracted behind [`SecretStore`] so tests never touch the real keychain.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use blake2::{Blake2s256, Digest};
use keyring::Entry;

use crate::noise::{KEY_LEN, KEYRING_SERVICE, NOISE_PARAMS, NoiseError, Result, hex};

/// Pluggable secret backend for the 32-byte static private key.
///
/// `load` MUST return `Ok(None)` (not an error) when there is simply no stored
/// value yet, so [`StaticKeypair::load_or_generate_with`] can generate on first
/// run. Real backends map their "no entry" case to `Ok(None)`.
pub trait SecretStore {
    /// Load the raw secret bytes for `account`, or `None` if not present.
    fn load(&self, account: &str) -> Result<Option<Vec<u8>>>;
    /// Persist `secret` under `account`, overwriting any existing value.
    fn store(&self, account: &str, secret: &[u8]) -> Result<()>;
    /// Remove any secret stored under `account`. Missing is not an error.
    fn delete(&self, account: &str) -> Result<()>;
}

/// OS-keyring-backed [`SecretStore`] using a stable service name
/// ([`KEYRING_SERVICE`]). The 32-byte key is stored hex-encoded via the
/// password API for portability across platform keyrings.
///
/// **Never use this in tests** — it writes to the user's real keychain. Tests
/// use [`InMemorySecretStore`].
#[derive(Debug, Default, Clone, Copy)]
pub struct OsKeyringStore;

impl OsKeyringStore {
    fn entry(account: &str) -> Result<Entry> {
        Ok(Entry::new(KEYRING_SERVICE, account)?)
    }
}

impl SecretStore for OsKeyringStore {
    fn load(&self, account: &str) -> Result<Option<Vec<u8>>> {
        match Self::entry(account)?.get_password() {
            Ok(s) => Ok(Some(hex::decode(&s)?)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(NoiseError::Keyring(e)),
        }
    }

    fn store(&self, account: &str, secret: &[u8]) -> Result<()> {
        Self::entry(account)?.set_password(&hex::encode(secret))?;
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<()> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(NoiseError::Keyring(e)),
        }
    }
}

/// File-backed [`SecretStore`] for hosts without a usable OS keyring — e.g.
/// headless Linux servers where no Secret Service is on the DBus session.
///
/// Each account's secret is stored hex-encoded (the same encoding
/// [`OsKeyringStore`] uses) in its own file under `dir`. On Unix the directory
/// is created `0700` and files `0600`, so only the owning user can read them.
/// This is strictly less secure than the OS keyring; callers should prefer the
/// keyring when it is available and surface which backend was chosen.
#[derive(Debug, Clone)]
pub struct FileSecretStore {
    dir: PathBuf,
}

impl FileSecretStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory secrets are stored under (for display; may not exist yet).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Account keys may contain arbitrary bytes (the scope joiner is NUL), so
    /// the file name is the lowercase-hex encoding of the account bytes.
    fn path(&self, account: &str) -> PathBuf {
        self.dir
            .join(format!("{}.secret", hex::encode(account.as_bytes())))
    }

    fn io_err(op: &str, e: std::io::Error) -> NoiseError {
        NoiseError::Store(format!("{op}: {e}"))
    }
}

impl SecretStore for FileSecretStore {
    fn load(&self, account: &str) -> Result<Option<Vec<u8>>> {
        match std::fs::read_to_string(self.path(account)) {
            Ok(text) => Ok(Some(hex::decode(&text)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Self::io_err("read secret file", e)),
        }
    }

    fn store(&self, account: &str, secret: &[u8]) -> Result<()> {
        create_private_dir(&self.dir).map_err(|e| Self::io_err("create secret dir", e))?;
        write_private_file(&self.path(account), hex::encode(secret).as_bytes())
            .map_err(|e| Self::io_err("write secret file", e))
    }

    fn delete(&self, account: &str) -> Result<()> {
        match std::fs::remove_file(self.path(account)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Self::io_err("delete secret file", e)),
        }
    }
}

/// Create `dir` (and parents), then tighten it to owner-only on Unix.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `contents` to `path`, created owner-read/write-only on Unix.
fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents)
}

/// In-memory [`SecretStore`] for tests. Holds secrets in a mutex-guarded map;
/// nothing is written to disk or the OS keychain.
#[derive(Default)]
pub struct InMemorySecretStore {
    inner: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl InMemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of stored entries (test introspection).
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SecretStore for InMemorySecretStore {
    fn load(&self, account: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.inner.lock().unwrap().get(account).cloned())
    }

    fn store(&self, account: &str, secret: &[u8]) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .insert(account.to_string(), secret.to_vec());
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<()> {
        self.inner.lock().unwrap().remove(account);
        Ok(())
    }
}

/// Compose the keyring account key from a `scope` (e.g. `"bridge"`, `"app"`)
/// and a stable `client_id`. A NUL joiner keeps the two fields unambiguous.
fn account_key(scope: &str, client_id: &str) -> String {
    format!("{scope}\u{0}{client_id}")
}

/// A device's persistent X25519 static keypair for Noise XX.
///
/// The private key is redacted from `Debug` and zeroized on drop. It is never
/// exposed by any public accessor except through the handshake builder inside
/// this crate (via [`StaticKeypair::private_bytes`], which is crate-private).
#[derive(Clone)]
pub struct StaticKeypair {
    private: [u8; KEY_LEN],
    public: [u8; KEY_LEN],
}

impl StaticKeypair {
    /// Load the device identity from the OS keyring, generating and persisting
    /// a fresh keypair on first run.
    ///
    /// The account key is scoped by `scope` + a stable `client_id`.
    pub fn load_or_generate(scope: &str, client_id: &str) -> Result<Self> {
        Self::load_or_generate_with(&OsKeyringStore, scope, client_id)
    }

    /// Like [`Self::load_or_generate`] but against an injected [`SecretStore`],
    /// so tests can supply [`InMemorySecretStore`].
    pub fn load_or_generate_with<S: SecretStore + ?Sized>(
        store: &S,
        scope: &str,
        client_id: &str,
    ) -> Result<Self> {
        let account = account_key(scope, client_id);
        match store.load(&account)? {
            Some(bytes) => Self::from_private_bytes(&bytes),
            None => {
                let kp = Self::generate()?;
                store.store(&account, &kp.private)?;
                Ok(kp)
            }
        }
    }

    /// Generate a fresh keypair (no persistence). Uses `snow`'s generator so
    /// the public key derivation matches exactly what the handshake will use.
    pub fn generate() -> Result<Self> {
        let params = NOISE_PARAMS.parse().map_err(NoiseError::from)?;
        let kp = snow::Builder::new(params).generate_keypair()?;
        let private = to_array(&kp.private)?;
        let public = to_array(&kp.public)?;
        Ok(Self { private, public })
    }

    /// Reconstruct a keypair from a stored 32-byte private key, deriving the
    /// public key via X25519. The derived public matches `snow`'s own
    /// computation (both follow RFC 7748 clamped scalar multiplication), which
    /// the handshake tests exercise end-to-end.
    pub fn from_private_bytes(bytes: &[u8]) -> Result<Self> {
        let private = to_array(bytes)?;
        let secret = x25519_dalek::StaticSecret::from(private);
        let public = x25519_dalek::PublicKey::from(&secret).to_bytes();
        Ok(Self { private, public })
    }

    /// The 32-byte static public key, safe to share/pin/register.
    pub fn public_key(&self) -> [u8; KEY_LEN] {
        self.public
    }

    /// A stable fingerprint of the public key: lowercase hex of
    /// `BLAKE2s-256(public_key)`. Suitable for display and for the API's
    /// public-key directory.
    pub fn fingerprint(&self) -> String {
        hex::encode(&Self::fingerprint_bytes(&self.public))
    }

    /// Raw 32-byte BLAKE2s fingerprint of an arbitrary public key.
    pub fn fingerprint_bytes(public_key: &[u8; KEY_LEN]) -> [u8; 32] {
        let mut h = Blake2s256::new();
        h.update(public_key);
        h.finalize().into()
    }

    /// Crate-internal access to the private key for building handshake state.
    /// Deliberately not `pub`.
    pub(crate) fn private_bytes(&self) -> &[u8; KEY_LEN] {
        &self.private
    }
}

/// Redacts the private key. Only the public key is shown.
impl std::fmt::Debug for StaticKeypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticKeypair")
            .field("public", &hex::encode(&self.public))
            .field("private", &"<redacted>")
            .finish()
    }
}

/// Best-effort zeroization of the private key on drop.
impl Drop for StaticKeypair {
    #[allow(unsafe_code)] // sole unsafe in the crate: volatile key zeroize
    fn drop(&mut self) {
        for b in self.private.iter_mut() {
            // volatile-ish write; sufficient to avoid trivially leaving the key
            // in freed memory. Not a hardened zeroize but adequate here.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

fn to_array(bytes: &[u8]) -> Result<[u8; KEY_LEN]> {
    if bytes.len() != KEY_LEN {
        return Err(NoiseError::InvalidKeyLength);
    }
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique, cleaned-on-drop temp dir per test (no tempfile dependency).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(test: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("sqlly-noise-{test}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn file_store_round_trips_and_deletes() {
        let tmp = TempDir::new("round-trip");
        let store = FileSecretStore::new(&tmp.0);
        // Account with a NUL joiner, like real scope\0client_id keys.
        let account = "bridge\u{0}machine-1";

        assert_eq!(store.load(account).unwrap(), None, "empty store loads None");

        let secret = [7u8; KEY_LEN];
        store.store(account, &secret).unwrap();
        assert_eq!(store.load(account).unwrap(), Some(secret.to_vec()));

        // Overwrite wins.
        let secret2 = [9u8; KEY_LEN];
        store.store(account, &secret2).unwrap();
        assert_eq!(store.load(account).unwrap(), Some(secret2.to_vec()));

        store.delete(account).unwrap();
        assert_eq!(store.load(account).unwrap(), None);
        // Deleting a missing entry is not an error.
        store.delete(account).unwrap();
    }

    #[test]
    fn file_store_load_or_generate_persists_identity() {
        let tmp = TempDir::new("load-or-generate");
        let store = FileSecretStore::new(&tmp.0);
        let first = StaticKeypair::load_or_generate_with(&store, "bridge", "m1").unwrap();
        let second = StaticKeypair::load_or_generate_with(&store, "bridge", "m1").unwrap();
        assert_eq!(
            first.public_key(),
            second.public_key(),
            "reload must return the same identity"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new("perms");
        let store = FileSecretStore::new(&tmp.0);
        store.store("bridge\u{0}m1", &[1u8; KEY_LEN]).unwrap();

        let dir_mode = std::fs::metadata(&tmp.0).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "secret dir must be owner-only");
        for entry in std::fs::read_dir(&tmp.0).unwrap() {
            let mode = entry.unwrap().metadata().unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "secret files must be owner-only");
        }
    }
}
