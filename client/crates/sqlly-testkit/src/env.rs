//! Serialized, self-restoring environment mutation for tests.
//!
//! Under Edition 2024, `std::env::set_var`/`remove_var` are `unsafe` because
//! concurrent reads from another thread race with the mutation. Tests that
//! need env vars must therefore (a) serialize all env mutation process-wide
//! and (b) restore the prior state so parallel-ordered tests never observe
//! each other's values. [`EnvGuard`] packages both: it holds a global lock
//! for its lifetime and restores every touched variable on drop. This is the
//! one sanctioned place for test env mutation — do not hand-roll per-crate
//! locks or bare `unsafe { set_var }` blocks in test modules.
//!
//! The lock only serializes within one test binary, which is exactly the
//! hazard boundary: separate test binaries are separate processes.

#![allow(unsafe_code)]

use std::ffi::OsString;
use std::sync::{Mutex, MutexGuard, PoisonError};

static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Holds the process-wide env lock and restores every variable it touched
/// (in reverse order) when dropped.
pub struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(String, Option<OsString>)>,
}

impl EnvGuard {
    /// Acquire the env lock. Blocks until any other guard in the process is
    /// dropped; a panicked holder does not poison the lock for later tests.
    pub fn acquire() -> Self {
        Self {
            _lock: ENV_MUTEX.lock().unwrap_or_else(PoisonError::into_inner),
            saved: Vec::new(),
        }
    }

    /// Set `key` to `value`, remembering the prior value for restoration.
    pub fn set(&mut self, key: &str, value: &str) {
        self.save(key);
        // SAFETY: the global lock serializes all mutation through this
        // module, and test code is expected to route every env change here.
        unsafe { std::env::set_var(key, value) };
    }

    /// Remove `key`, remembering the prior value for restoration.
    pub fn remove(&mut self, key: &str) {
        self.save(key);
        // SAFETY: as in `set`.
        unsafe { std::env::remove_var(key) };
    }

    fn save(&mut self, key: &str) {
        if !self.saved.iter().any(|(k, _)| k == key) {
            self.saved.push((key.to_string(), std::env::var_os(key)));
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, prior) in self.saved.drain(..).rev() {
            match prior {
                // SAFETY: still holding the global lock (dropped after us).
                Some(value) => unsafe { std::env::set_var(&key, value) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_prior_values_and_removals() {
        let key = "SQLLY_TESTKIT_ENV_GUARD_TEST";
        {
            let mut g = EnvGuard::acquire();
            g.set(key, "outer");
        }
        // Restored to unset.
        assert!(std::env::var(key).is_err());

        let mut g = EnvGuard::acquire();
        g.set(key, "first");
        g.set(key, "second");
        g.remove(key);
        g.set(key, "third");
        assert_eq!(std::env::var(key).unwrap(), "third");
        drop(g);
        assert!(
            std::env::var(key).is_err(),
            "original (unset) state restored"
        );
    }

    #[test]
    fn serializes_concurrent_mutators() {
        let key = "SQLLY_TESTKIT_ENV_GUARD_RACE";
        let handles: Vec<_> = (0..8)
            .map(|i| {
                std::thread::spawn(move || {
                    let mut g = EnvGuard::acquire();
                    let value = format!("v{i}");
                    g.set(key, &value);
                    // While the guard is held, nobody else can change it.
                    assert_eq!(std::env::var(key).unwrap(), value);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(std::env::var(key).is_err());
    }
}
