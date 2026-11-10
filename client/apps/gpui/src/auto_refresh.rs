//! Per-tab result auto-refresh scheduling.
//!
//! Pure decision logic the app ticker calls each second. Pause rules are
//! testable without a window: an interval stays set while skipped, and a
//! refresh is never issued while a run is in flight.

/// Interval choices shown in the menu; `None` = off.
pub const INTERVAL_CHOICES: [Option<u64>; 5] = [None, Some(5), Some(10), Some(30), Some(60)];

/// Everything the ticker needs to know at one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickInput {
    pub interval_secs: Option<u64>,
    pub tab_is_active: bool,
    pub run_in_flight: bool,
    pub mutation_review_pending: bool,
    pub tab_snapshot_only: bool,
    pub last_sql_is_select_only: bool,
    pub secs_since_last_completion: Option<u64>, // None = never completed
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickDecision {
    Off,
    Skip,
    Refresh,
}

pub fn interval_label(secs: Option<u64>) -> String {
    match secs {
        None => crate::localization::text("query.auto_refresh.interval_off"),
        Some(60) => crate::localization::text("query.auto_refresh.interval_one_minute"),
        Some(secs) => crate::localization::format(
            "query.auto_refresh.interval_seconds",
            &[("seconds", &secs.to_string())],
        ),
    }
}

/// Decide whether this tick should re-run the tab's last SELECT.
///
/// `interval_secs == None` → [`TickDecision::Off`]. Any pause condition or an
/// interval that has not elapsed since the previous run's **completion** →
/// [`TickDecision::Skip`]. Otherwise [`TickDecision::Refresh`].
pub fn tick_decision(input: TickInput) -> TickDecision {
    if input.interval_secs.is_none() {
        return TickDecision::Off;
    }
    if !input.tab_is_active
        || input.run_in_flight
        || input.mutation_review_pending
        || input.tab_snapshot_only
        || !input.last_sql_is_select_only
    {
        return TickDecision::Skip;
    }
    match input.secs_since_last_completion {
        Some(elapsed) if elapsed >= input.interval_secs.unwrap_or(0) => TickDecision::Refresh,
        _ => TickDecision::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> TickInput {
        TickInput {
            interval_secs: Some(5),
            tab_is_active: true,
            run_in_flight: false,
            mutation_review_pending: false,
            tab_snapshot_only: false,
            last_sql_is_select_only: true,
            secs_since_last_completion: Some(5),
        }
    }

    #[test]
    fn off_when_interval_none() {
        let mut input = ready();
        input.interval_secs = None;
        assert_eq!(tick_decision(input), TickDecision::Off);
    }

    #[test]
    fn skips_inactive_tab() {
        let mut input = ready();
        input.tab_is_active = false;
        assert_eq!(tick_decision(input), TickDecision::Skip);
    }

    #[test]
    fn skips_while_run_in_flight() {
        let mut input = ready();
        input.run_in_flight = true;
        assert_eq!(tick_decision(input), TickDecision::Skip);
    }

    #[test]
    fn skips_pending_mutation_review() {
        let mut input = ready();
        input.mutation_review_pending = true;
        assert_eq!(tick_decision(input), TickDecision::Skip);
    }

    #[test]
    fn skips_snapshot_only_tab() {
        let mut input = ready();
        input.tab_snapshot_only = true;
        assert_eq!(tick_decision(input), TickDecision::Skip);
    }

    #[test]
    fn skips_non_select_sql() {
        let mut input = ready();
        input.last_sql_is_select_only = false;
        assert_eq!(tick_decision(input), TickDecision::Skip);
    }

    #[test]
    fn skips_until_interval_elapsed_since_completion() {
        let mut input = ready();
        input.secs_since_last_completion = None;
        assert_eq!(tick_decision(input), TickDecision::Skip);
        input.secs_since_last_completion = Some(4);
        assert_eq!(tick_decision(input), TickDecision::Skip);
    }

    #[test]
    fn refreshes_when_all_clear() {
        assert_eq!(tick_decision(ready()), TickDecision::Refresh);
    }

    #[test]
    fn labels_render_off_seconds_and_minute() {
        assert_eq!(interval_label(None), "Auto-refresh: off");
        assert_eq!(interval_label(Some(5)), "Auto-refresh: 5s");
        assert_eq!(interval_label(Some(60)), "Auto-refresh: 1m");
    }

    #[test]
    fn labels_use_complete_spanish_messages() {
        let _guard = crate::localization::test_language_with(
            crate::localization::Language::Spanish,
            Some(
                r#"{"query":{"auto_refresh":{"interval_seconds":{"value":"Actualización automática: {seconds} s","location":"Query toolbar → Auto-refresh: interval menu showing seconds between result refreshes."}}}}"#,
            ),
        );
        assert_eq!(interval_label(Some(7)), "Actualización automática: 7 s");
    }
}
