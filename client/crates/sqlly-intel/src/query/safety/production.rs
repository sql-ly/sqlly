//! Production-environment policy layer over the SELECT-only gate.
//!
//! Production is never a parallel SQL classifier: callers resolve whether a
//! connection is production, whether writes are temporarily unlocked, and
//! whether the existing `block_all_but_select` path must be forced for this run.
//! Unlock state is in-memory only and never persisted.

#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, Instant};
#[cfg(target_arch = "wasm32")]
use web_time::{Duration, Instant};

/// How long a production connection stays writable after the user unlocks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockWindow {
    OneMinute,
    FiveMinutes,
    FifteenMinutes,
}

impl UnlockWindow {
    pub fn duration(self) -> Duration {
        match self {
            Self::OneMinute => Duration::from_secs(60),
            Self::FiveMinutes => Duration::from_secs(300),
            Self::FifteenMinutes => Duration::from_secs(900),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::OneMinute => "1 min",
            Self::FiveMinutes => "5 min",
            Self::FifteenMinutes => "15 min",
        }
    }

    pub fn message(self) -> sqlly_core::DisplayMessage {
        let (key, fallback) = match self {
            Self::OneMinute => ("services.safety.unlock.one_minute", "1 min"),
            Self::FiveMinutes => ("services.safety.unlock.five_minutes", "5 min"),
            Self::FifteenMinutes => ("services.safety.unlock.fifteen_minutes", "15 min"),
        };
        sqlly_core::DisplayMessage::new(key, fallback)
    }
}

/// In-memory, per-app-session write unlock. Never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteUnlock {
    pub expires_at: Instant,
    pub window: UnlockWindow,
}

impl WriteUnlock {
    pub fn start(window: UnlockWindow, now: Instant) -> Self {
        Self {
            expires_at: now + window.duration(),
            window,
        }
    }

    pub fn is_active(&self, now: Instant) -> bool {
        now < self.expires_at
    }

    pub fn remaining(&self, now: Instant) -> Duration {
        self.expires_at.saturating_duration_since(now)
    }

    /// "4:59" style countdown for the banner.
    pub fn remaining_label(&self, now: Instant) -> String {
        let remaining = self.remaining(now);
        if remaining.is_zero() {
            return "0:00".to_string();
        }
        let total_secs = remaining.as_secs();
        format!("{}:{:02}", total_secs / 60, total_secs % 60)
    }
}

/// What the production layer decides for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionDecision {
    /// Not a production target: nothing changes.
    NotProduction,
    /// Production and locked: force the SELECT-only gate; a non-SELECT batch is refused at the driver.
    ReadOnly,
    /// Production and unlocked: allow writes, but every data-changing batch must be confirmed with the exact SQL shown.
    WritesUnlocked,
}

/// Inputs already resolved by the caller (entity fold + profile override).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProductionGuard {
    /// Effective flag: environment entity `is_production` OR profile override `Some(true)`; `Some(false)` override clears the environment's flag for that one profile.
    pub is_production: bool,
}

impl ProductionGuard {
    pub fn resolve(env_is_production: bool, profile_override: Option<bool>) -> Self {
        Self {
            is_production: profile_override.unwrap_or(env_is_production),
        }
    }

    pub fn decide(&self, unlock: Option<&WriteUnlock>, now: Instant) -> ProductionDecision {
        if !self.is_production {
            return ProductionDecision::NotProduction;
        }
        if unlock.is_some_and(|u| u.is_active(now)) {
            ProductionDecision::WritesUnlocked
        } else {
            ProductionDecision::ReadOnly
        }
    }

    /// True when the per-run profile must carry `block_all_but_select = true`.
    pub fn forces_select_only(&self, unlock: Option<&WriteUnlock>, now: Instant) -> bool {
        matches!(self.decide(unlock, now), ProductionDecision::ReadOnly)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_production_never_locks() {
        let guard = ProductionGuard::resolve(false, None);
        assert_eq!(
            guard.decide(None, Instant::now()),
            ProductionDecision::NotProduction
        );
        assert!(!guard.forces_select_only(None, Instant::now()));
    }

    #[test]
    fn production_locked_by_default() {
        let guard = ProductionGuard::resolve(true, None);
        let now = Instant::now();
        assert_eq!(guard.decide(None, now), ProductionDecision::ReadOnly);
        assert!(guard.forces_select_only(None, now));
    }

    #[test]
    fn override_true_makes_non_production_env_production() {
        let guard = ProductionGuard::resolve(false, Some(true));
        let now = Instant::now();
        assert_eq!(guard.decide(None, now), ProductionDecision::ReadOnly);
    }

    #[test]
    fn override_false_opts_out_of_production_env() {
        let guard = ProductionGuard::resolve(true, Some(false));
        assert_eq!(
            guard.decide(None, Instant::now()),
            ProductionDecision::NotProduction
        );
    }

    #[test]
    fn unlock_allows_writes_until_expiry() {
        let guard = ProductionGuard::resolve(true, None);
        let t0 = Instant::now();
        let unlock = WriteUnlock::start(UnlockWindow::OneMinute, t0);
        assert_eq!(
            guard.decide(Some(&unlock), t0 + Duration::from_secs(59)),
            ProductionDecision::WritesUnlocked
        );
        assert_eq!(
            guard.decide(Some(&unlock), t0 + Duration::from_secs(60)),
            ProductionDecision::ReadOnly
        );
    }

    #[test]
    fn unlock_windows_have_expected_durations() {
        assert_eq!(UnlockWindow::OneMinute.duration(), Duration::from_secs(60));
        assert_eq!(
            UnlockWindow::FiveMinutes.duration(),
            Duration::from_secs(300)
        );
        assert_eq!(
            UnlockWindow::FifteenMinutes.duration(),
            Duration::from_secs(900)
        );
    }

    #[test]
    fn remaining_label_formats_mmss() {
        let t0 = Instant::now();
        let unlock = WriteUnlock::start(UnlockWindow::FiveMinutes, t0);
        assert_eq!(unlock.remaining_label(t0 + Duration::from_secs(1)), "4:59");
        assert_eq!(
            unlock.remaining_label(t0 + Duration::from_secs(300)),
            "0:00"
        );
    }

    #[test]
    fn expired_unlock_on_non_production_is_still_not_production() {
        let guard = ProductionGuard::resolve(false, None);
        let t0 = Instant::now();
        let unlock = WriteUnlock::start(UnlockWindow::OneMinute, t0);
        assert_eq!(
            guard.decide(Some(&unlock), t0 + Duration::from_secs(120)),
            ProductionDecision::NotProduction
        );
    }
}
