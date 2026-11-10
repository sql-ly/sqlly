//! Pinned database clients for per-tab manual commit mode.
//!
//! Separate from [`crate::tds_pool::WarmPool`]: open transactions must never be
//! reaped by the idle TTL, and a busy slot must refuse a second concurrent run.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use sqlly_core::{ConnectionKind, StudioError, StudioResult};

/// Engine client pinned to a tab for the life of a manual transaction.
#[allow(clippy::large_enum_variant)]
pub(crate) enum PinnedClient {
    Tds(sqlly_engines::tds::TdsClient),
    Pg(sqlly_engines::pg::PgClient),
    MySql(sqlly_engines::mysql::MySqlClient),
}

/// What the host sees about a pinned session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualTxState {
    /// BEGIN was issued and the client is idle in the slot.
    Open { statements: u32 },
    /// A statement is executing on the pinned client right now.
    Busy { statements: u32 },
    /// The client was lost (transport failure / cancel); the server rolled back.
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionEnd {
    Commit,
    Rollback,
}

struct Slot {
    client: Option<PinnedClient>,
    statements: u32,
    lost: bool,
}

/// Pinned sessions keyed by `WarmPool::pool_key(Some(lane), profile)`. Separate
/// from the warm pools on purpose: the reaper must never drop an open
/// user transaction, and a busy slot must refuse a second concurrent run.
#[derive(Clone, Default)]
pub(crate) struct PinnedSessions {
    inner: Arc<Mutex<HashMap<String, Slot>>>,
}

pub(crate) fn begin_sql(kind: ConnectionKind) -> &'static str {
    match kind {
        ConnectionKind::PostgresWire => "BEGIN",
        ConnectionKind::MySqlWire => "START TRANSACTION",
        ConnectionKind::Tds => "SET XACT_ABORT OFF; BEGIN TRANSACTION",
        _ => "BEGIN",
    }
}

pub(crate) fn end_sql(kind: ConnectionKind, end: TransactionEnd) -> &'static str {
    match (kind, end) {
        (ConnectionKind::Tds, TransactionEnd::Commit) => "IF @@TRANCOUNT > 0 COMMIT TRANSACTION",
        (ConnectionKind::Tds, TransactionEnd::Rollback) => {
            "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION"
        }
        (_, TransactionEnd::Commit) => "COMMIT",
        (_, TransactionEnd::Rollback) => "ROLLBACK",
    }
}

impl PinnedSessions {
    pub fn take(&self, key: &str) -> StudioResult<Option<PinnedClient>> {
        let mut inner = self.inner.lock().expect("pinned sessions poisoned");
        let Some(slot) = inner.get_mut(key) else {
            return Ok(None);
        };
        if slot.lost {
            return Ok(None);
        }
        if slot.client.is_none() {
            return Err(StudioError::InvalidInput(
                "A statement is already running in this tab's transaction".to_string(),
            ));
        }
        Ok(slot.client.take())
    }

    pub fn put_back(&self, key: &str, client: PinnedClient) {
        let mut inner = self.inner.lock().expect("pinned sessions poisoned");
        if let Some(slot) = inner.get_mut(key) {
            slot.statements = slot.statements.saturating_add(1);
            slot.client = Some(client);
            slot.lost = false;
        }
    }

    pub fn mark_lost(&self, key: &str) {
        let mut inner = self.inner.lock().expect("pinned sessions poisoned");
        if let Some(slot) = inner.get_mut(key) {
            slot.lost = true;
            slot.client = None;
        }
    }

    pub fn close(&self, key: &str) {
        let _ = self.remove(key);
    }

    fn remove(&self, key: &str) -> Option<Slot> {
        self.inner
            .lock()
            .expect("pinned sessions poisoned")
            .remove(key)
    }

    pub fn open(&self, key: String, client: PinnedClient) {
        self.inner.lock().expect("pinned sessions poisoned").insert(
            key,
            Slot {
                client: Some(client),
                statements: 0,
                lost: false,
            },
        );
    }

    pub fn state(&self, key: &str) -> Option<ManualTxState> {
        let inner = self.inner.lock().expect("pinned sessions poisoned");
        let slot = inner.get(key)?;
        if slot.lost {
            return Some(ManualTxState::Lost);
        }
        if slot.client.is_some() {
            Some(ManualTxState::Open {
                statements: slot.statements,
            })
        } else {
            Some(ManualTxState::Busy {
                statements: slot.statements,
            })
        }
    }

    pub fn keys_for_identity(&self, identity: &str) -> Vec<String> {
        let suffix = format!("\u{1f}{identity}");
        self.inner
            .lock()
            .expect("pinned sessions poisoned")
            .keys()
            .filter(|k| k.ends_with(&suffix) || **k == identity)
            .cloned()
            .collect()
    }

    /// Remove every pinned slot for `identity` and return lane keys with any
    /// held client (for best-effort rollback on profile eviction).
    pub(crate) fn drain_identity_clients(
        &self,
        identity: &str,
    ) -> Vec<(String, Option<PinnedClient>)> {
        self.keys_for_identity(identity)
            .into_iter()
            .filter_map(|key| {
                let slot = self.remove(&key)?;
                Some((key, slot.client))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(lane: &str, identity: &str) -> String {
        format!("{lane}\u{1f}{identity}")
    }

    #[test]
    fn take_on_busy_slot_is_an_error() {
        let pinned = PinnedSessions::default();
        let k = key("tab-1", "identity-a");
        pinned.inner.lock().unwrap().insert(
            k.clone(),
            Slot {
                client: None,
                statements: 1,
                lost: false,
            },
        );
        let err = match pinned.take(&k) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("expected busy error"),
        };
        assert!(err.contains("already running"));
    }

    #[test]
    fn put_back_increments_statements_and_reports_open() {
        let pinned = PinnedSessions::default();
        let k = key("tab-1", "identity-a");
        pinned.inner.lock().unwrap().insert(
            k.clone(),
            Slot {
                client: None,
                statements: 0,
                lost: false,
            },
        );
        assert_eq!(
            pinned.state(&k),
            Some(ManualTxState::Busy { statements: 0 })
        );
        {
            let mut inner = pinned.inner.lock().unwrap();
            let slot = inner.get_mut(&k).unwrap();
            slot.statements = slot.statements.saturating_add(1);
        }
        assert_eq!(
            pinned.state(&k),
            Some(ManualTxState::Busy { statements: 1 })
        );
    }

    #[test]
    fn mark_lost_reports_lost_and_take_returns_none() {
        let pinned = PinnedSessions::default();
        let k = key("tab-1", "identity-a");
        pinned.inner.lock().unwrap().insert(
            k.clone(),
            Slot {
                client: None,
                statements: 2,
                lost: false,
            },
        );
        pinned.mark_lost(&k);
        assert_eq!(pinned.state(&k), Some(ManualTxState::Lost));
        assert!(pinned.take(&k).unwrap().is_none());
    }

    #[test]
    fn keys_for_identity_matches_only_that_identity() {
        let pinned = PinnedSessions::default();
        let a1 = key("tab-1", "identity-a");
        let a2 = key("tab-2", "identity-a");
        let b1 = key("tab-1", "identity-b");
        pinned.inner.lock().unwrap().insert(
            a1.clone(),
            Slot {
                client: None,
                statements: 0,
                lost: false,
            },
        );
        pinned.inner.lock().unwrap().insert(
            a2.clone(),
            Slot {
                client: None,
                statements: 0,
                lost: false,
            },
        );
        pinned.inner.lock().unwrap().insert(
            b1.clone(),
            Slot {
                client: None,
                statements: 0,
                lost: false,
            },
        );
        let mut keys = pinned.keys_for_identity("identity-a");
        keys.sort();
        assert_eq!(keys, vec![a1, a2]);
    }

    #[test]
    fn begin_and_end_sql_per_engine() {
        assert_eq!(begin_sql(ConnectionKind::PostgresWire), "BEGIN");
        assert_eq!(begin_sql(ConnectionKind::MySqlWire), "START TRANSACTION");
        assert!(
            begin_sql(ConnectionKind::Tds).starts_with("SET XACT_ABORT OFF"),
            "TDS begin must disable XACT_ABORT for manual mode"
        );
        assert_eq!(
            end_sql(ConnectionKind::Tds, TransactionEnd::Commit),
            "IF @@TRANCOUNT > 0 COMMIT TRANSACTION"
        );
        assert_eq!(
            end_sql(ConnectionKind::PostgresWire, TransactionEnd::Rollback),
            "ROLLBACK"
        );
    }
}
