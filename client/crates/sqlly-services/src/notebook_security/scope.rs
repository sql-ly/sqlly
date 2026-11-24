//! Cache scopes (spec 5.2, ADR-005, NB-03 "role/tenant changed => no
//! unsafe cache reuse").
//!
//! A scope is a keyed hash (HMAC-SHA256 under a secret scope key) of the
//! connection profile id, database, effective login identity and policy
//! flags the results were fetched under. Cached results live only under
//! their scope, so another principal, database or policy never finds them,
//! and because the key is secret, scope ids and the file names under them
//! cannot be computed from ids someone can see or guess.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};

use crate::notebook::ConnectionIdentity;

const BLOCK: usize = 64;

/// Plain HMAC-SHA256 (RFC 2104) of the concatenated `message` pieces, for
/// keys up to one block.
fn hmac_raw(key: &[u8], message: &[&[u8]]) -> [u8; 32] {
    debug_assert!(key.len() <= BLOCK);
    let mut padded = [0u8; BLOCK];
    padded[..key.len()].copy_from_slice(key);
    let mut inner = Sha256::new();
    inner.update(padded.map(|b| b ^ 0x36));
    for piece in message {
        inner.update(piece);
    }
    let inner: [u8; 32] = inner.finalize().into();
    let mut outer = Sha256::new();
    outer.update(padded.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// HMAC-SHA256 over the length-prefixed concatenation of `parts`, so part
/// boundaries are unambiguous.
pub(crate) fn hmac_sha256(key: &[u8; 32], parts: &[&[u8]]) -> [u8; 32] {
    let lengths: Vec<[u8; 8]> = parts
        .iter()
        .map(|p| (p.len() as u64).to_le_bytes())
        .collect();
    let mut message: Vec<&[u8]> = Vec::with_capacity(parts.len() * 2);
    for (length, part) in lengths.iter().zip(parts) {
        message.push(length);
        message.push(part);
    }
    hmac_raw(key, &message)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The secret that keys scope ids and cache file names. Derived from the
/// cache master key; never written anywhere.
#[derive(Clone)]
pub struct ScopeKey([u8; 32]);

impl ScopeKey {
    pub fn from_bytes(bytes: [u8; 32]) -> ScopeKey {
        ScopeKey(bytes)
    }

    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Drop for ScopeKey {
    fn drop(&mut self) {
        self.0.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl std::fmt::Debug for ScopeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ScopeKey(..)")
    }
}

/// One security principal's slice of the cache.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CacheScope {
    id: String,
    /// Keyed tags of the connection profiles whose results this scope
    /// holds, so a disconnect can clear them without knowing the login.
    owners: BTreeSet<String>,
}

/// The keyed tag of a connection profile ([`CacheScope::owners`]).
pub fn owner_tag(profile_id: &str, key: &ScopeKey) -> String {
    hex(&hmac_sha256(
        key.bytes(),
        &[b"sqlly.notebook.owner.v1", profile_id.as_bytes()],
    ))
}

impl CacheScope {
    /// The scope for results fetched as `identity`.
    pub fn derive(identity: &ConnectionIdentity, key: &ScopeKey) -> CacheScope {
        let flags: Vec<&str> = identity.policy_flags.iter().map(String::as_str).collect();
        let flags = flags.join("\u{1f}");
        let database = identity.database.as_deref().unwrap_or("\u{0}none");
        CacheScope {
            id: hex(&hmac_sha256(
                key.bytes(),
                &[
                    b"sqlly.notebook.scope.v1",
                    identity.profile_id.as_bytes(),
                    database.as_bytes(),
                    identity.login_identity.as_bytes(),
                    flags.as_bytes(),
                ],
            )),
            owners: [owner_tag(&identity.profile_id, key)].into(),
        }
    }

    /// The scope for outputs computed in-process from no connection at all
    /// (imported files), private to one notebook.
    pub fn local(notebook_id: &str, key: &ScopeKey) -> CacheScope {
        CacheScope {
            id: hex(&hmac_sha256(
                key.bytes(),
                &[b"sqlly.notebook.scope.local.v1", notebook_id.as_bytes()],
            )),
            owners: BTreeSet::new(),
        }
    }

    /// The scope for an output derived from several upstream scopes (a
    /// chart over two connections): readable only where all of them meet,
    /// so it never lands in a scope one of its sources could not see.
    pub fn combine(scopes: &[CacheScope], key: &ScopeKey) -> Option<CacheScope> {
        let mut ids: Vec<&str> = scopes.iter().map(|s| s.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        let owners: BTreeSet<String> = scopes
            .iter()
            .flat_map(|s| s.owners.iter().cloned())
            .collect();
        match ids.as_slice() {
            [] => None,
            [only] => Some(CacheScope {
                id: (*only).to_string(),
                owners,
            }),
            many => {
                let mut parts: Vec<&[u8]> = vec![b"sqlly.notebook.scope.combined.v1"];
                parts.extend(many.iter().map(|id| id.as_bytes()));
                Some(CacheScope {
                    id: hex(&hmac_sha256(key.bytes(), &parts)),
                    owners,
                })
            }
        }
    }

    /// 64 lowercase hex digits; safe as a directory name.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Accepts only well-formed ids (as read back from a directory name).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn from_id(id: &str) -> Option<CacheScope> {
        (id.len() == 64
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .then(|| CacheScope {
            id: id.to_string(),
            owners: BTreeSet::new(),
        })
    }

    pub fn owners(&self) -> &BTreeSet<String> {
        &self.owners
    }

    /// The unguessable file stem for `entry` in this scope.
    pub fn entry_name(&self, entry: &str, key: &ScopeKey) -> String {
        hex(&hmac_sha256(
            key.bytes(),
            &[
                b"sqlly.notebook.entry.v1",
                self.id.as_bytes(),
                entry.as_bytes(),
            ],
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn identity(profile: &str, login: &str) -> ConnectionIdentity {
        ConnectionIdentity {
            profile_id: profile.into(),
            database: Some("sales".into()),
            login_identity: login.into(),
            engine: "SQL Server 16".into(),
            policy_flags: ["read_only".to_string()].into(),
            session_settings: Default::default(),
        }
    }

    fn key(n: u8) -> ScopeKey {
        ScopeKey::from_bytes([n; 32])
    }

    #[test]
    fn hmac_matches_rfc_4231_test_case_2() {
        let tag = hmac_raw(b"Jefe", &[b"what do ya want ", b"for nothing?"]);
        assert_eq!(
            hex(&tag),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn framed_hmac_keeps_part_boundaries() {
        let a = hmac_sha256(&[7; 32], &[b"ab", b"c"]);
        let b = hmac_sha256(&[7; 32], &[b"a", b"bc"]);
        assert_ne!(a, b, "length prefixes keep part boundaries");
        assert_eq!(a, hmac_sha256(&[7; 32], &[b"ab", b"c"]));
        assert_ne!(a, hmac_sha256(&[8; 32], &[b"ab", b"c"]));
    }

    #[test]
    fn every_identity_component_changes_the_scope() {
        let k = key(1);
        let base = identity("p-1", "alice");
        let scope = CacheScope::derive(&base, &k);
        let mut variants = vec![];
        let mut v = base.clone();
        v.profile_id = "p-2".into();
        variants.push(v);
        let mut v = base.clone();
        v.database = Some("hr".into());
        variants.push(v);
        let mut v = base.clone();
        v.database = None;
        variants.push(v);
        let mut v = base.clone();
        v.login_identity = "bob".into();
        variants.push(v);
        let mut v = base.clone();
        v.login_identity = "alice;role=admin".into();
        variants.push(v);
        let mut v = base.clone();
        v.policy_flags.clear();
        variants.push(v);
        for variant in variants {
            assert_ne!(CacheScope::derive(&variant, &k), scope, "{variant:?}");
        }
        // Engine version and session settings are the fingerprint's job.
        let mut same = base.clone();
        same.engine = "SQL Server 17".into();
        assert_eq!(CacheScope::derive(&same, &k), scope);
    }

    #[test]
    fn scopes_and_entries_depend_on_the_secret_key() {
        let id = identity("p-1", "alice");
        let a = CacheScope::derive(&id, &key(1));
        let b = CacheScope::derive(&id, &key(2));
        assert_ne!(a, b);
        assert_ne!(a.entry_name("fp", &key(1)), a.entry_name("fp", &key(2)));
        assert_ne!(a.entry_name("fp", &key(1)), b.entry_name("fp", &key(1)));
        assert!(CacheScope::from_id(a.id()).is_some());
        assert!(CacheScope::from_id("../etc").is_none());
        assert!(CacheScope::from_id(&"A".repeat(64)).is_none());
    }

    #[test]
    fn combined_scopes_are_order_free_and_distinct_from_their_parts() {
        let k = key(3);
        let a = CacheScope::derive(&identity("p-1", "alice"), &k);
        let b = CacheScope::derive(&identity("p-2", "alice"), &k);
        let ab = CacheScope::combine(&[a.clone(), b.clone()], &k).unwrap();
        assert_eq!(
            ab,
            CacheScope::combine(&[b.clone(), a.clone(), b.clone()], &k).unwrap()
        );
        assert_ne!(ab, a);
        assert_ne!(ab, b);
        assert_eq!(CacheScope::combine(&[a.clone(), a.clone()], &k), Some(a));
        assert_eq!(CacheScope::combine(&[], &k), None);
        assert_ne!(CacheScope::local("nb-1", &k), CacheScope::local("nb-2", &k));
    }
}
