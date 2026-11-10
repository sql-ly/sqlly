//! Licensing / entitlement flag and the free-tier "supporter" messaging.
//!
//! [`is_paying`] answers from a PERSISTED entitlement marker: when a
//! signed-in account's subscription is fetched from the store and grants
//! access, [`record_entitlement`] caches its paid-until time (in memory and
//! on disk), so licensed users stay recognized offline and across launches.
//! The app refreshes the marker at startup and whenever the account dialog
//! loads the subscription.
//!
//! Two surfaces exist, and BOTH stay quiet for a licensed user unless they
//! opt in:
//!
//! * The **supporter message** — a small, upbeat status-bar chip
//!   ("I love SQLLY, you will too!"). It shows for as long as the app is
//!   unlicensed. Once the user pays it turns off by default; a Preferences →
//!   Appearance toggle ([`keep_supporter_message`]) lets a paying fan keep
//!   flying the flag.
//! * The **pay dialog** — a modal, shown to unlicensed users at most once per
//!   [`NAG_DIALOG_INTERVAL_HOURS`]-hour window. The last-shown time is
//!   persisted so the cadence survives relaunches. Licensed users never see
//!   it (and there is deliberately no opt-in for a nag dialog).
//!
//! In the Mac App Store edition the in-app purchase is a second source of
//! truth: [`record_app_store_entitlement`] mirrors StoreKit's answer into its
//! own marker, and either marker makes [`is_paying`] true (a license bought on
//! the web store also unlocks the App Store edition once signed in). That
//! edition also enforces the free tier's [`FREE_SAVED_CONNECTIONS`] limit.
//!
//! The cadence logic is pure so it can be unit-tested (per AGENTS.md); only
//! the `*_now` wrappers and the persistence helpers read the clock, the baked
//! build date, or durable storage.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Duration, NaiveDate, Utc};

/// Pricing page the free-tier messaging links to.
pub const PRICING_URL: &str = "https://sqlly.app/pricing";

/// Public page listing every open-source project SQLLY is built on, with each
/// one's license. Where the About panel's "full license breakdown" link points.
pub const LICENSES_URL: &str = "https://sqlly.app/licenses";

/// My-account page a *licensed* user is taken to instead of pricing — there is
/// nothing left to sell them, so the supporter chip becomes a shortcut to
/// manage their subscription.
pub const ACCOUNT_URL: &str = "https://sqlly.app/account";

/// The site's privacy policy, linked from the account dialog, sign-in window
/// and Help menu (App Store guideline 5.1.1 wants it reachable in-app).
pub const PRIVACY_URL: &str = "https://sqlly.app/privacy";

/// The site's terms of use, linked beside the privacy policy.
pub const TERMS_URL: &str = "https://sqlly.app/terms";

/// Self-serve account deletion (App Store guideline 5.1.1(v)), linked from
/// the account dialog.
pub const DELETE_ACCOUNT_URL: &str = "https://sqlly.app/account/delete";

/// `&app=mas` in the App Store edition, else empty. Web pages the app opens
/// (sign-in, account deletion) read it to hide pricing and purchase links,
/// which the App Store edition may not lead users to (guideline 3.1.1).
pub fn web_edition_query_suffix() -> &'static str {
    if crate::distribution::sells_through_web_store() {
        ""
    } else {
        "&app=mas"
    }
}

/// [`DELETE_ACCOUNT_URL`] tagged with the edition (see
/// [`web_edition_query_suffix`]).
pub fn delete_account_url() -> String {
    match web_edition_query_suffix().strip_prefix('&') {
        Some(param) => format!("{DELETE_ACCOUNT_URL}?{param}"),
        None => DELETE_ACCOUNT_URL.to_string(),
    }
}

/// Where the supporter / upgrade surfaces should send the user: a licensed
/// supporter has nothing left to buy, so they go to manage their account;
/// everyone else goes to pricing.
pub fn supporter_link_url() -> &'static str {
    if is_paying() {
        ACCOUNT_URL
    } else {
        PRICING_URL
    }
}

/// Minimum gap between two showings of the pay dialog.
pub const NAG_DIALOG_INTERVAL_HOURS: i64 = 5;

/// Where the persisted "licensed until" marker lives. Carries only a
/// timestamp — no token, key, or secret — so plain data storage is fine.
const PAID_UNTIL_ARTIFACT: &str = "license-paid-until.txt";

/// In-memory copy of the persisted marker, loaded once and updated by
/// [`record_entitlement`]. Keeps [`is_paying`] cheap enough for render paths.
static PAID_UNTIL: OnceLock<Mutex<Option<DateTime<Utc>>>> = OnceLock::new();

fn paid_until_cell() -> &'static Mutex<Option<DateTime<Utc>>> {
    PAID_UNTIL.get_or_init(|| Mutex::new(load_paid_until()))
}

fn load_paid_until() -> Option<DateTime<Utc>> {
    let raw = crate::host_storage::load_text(
        crate::host_storage::StorageArea::Data,
        PAID_UNTIL_ARTIFACT,
    )?;
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Whether a license paid through `paid_until` still grants access at `now`.
///
/// Pure. `None` = no known entitlement. A cancelled-but-not-expired plan keeps
/// access until its period end (mirroring the store's `has_access` semantics);
/// lifetime/comped grants carry a far-future period end.
pub fn is_paying_at(paid_until: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    paid_until.is_some_and(|until| now < until)
}

/// Whether the current user has paid for the app, per the cached entitlement
/// markers (see module docs for how they are refreshed).
pub fn is_paying() -> bool {
    let cached = *paid_until_cell()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    is_paying_at(cached, Utc::now()) || app_store_unlocked()
}

/// Where the persisted "owns the App Store lifetime unlock" marker lives.
/// Re-derived from StoreKit at every launch, so a stale or hand-edited file
/// never outlives the next verified check.
const APP_STORE_UNLOCK_ARTIFACT: &str = "license-app-store.txt";

static APP_STORE_UNLOCKED: OnceLock<AtomicBool> = OnceLock::new();

fn app_store_unlocked_cell() -> &'static AtomicBool {
    APP_STORE_UNLOCKED.get_or_init(|| {
        let owned = crate::distribution::is_app_store()
            && crate::host_storage::load_text(
                crate::host_storage::StorageArea::Data,
                APP_STORE_UNLOCK_ARTIFACT,
            )
            .is_some_and(|raw| raw.trim() == "owned");
        AtomicBool::new(owned)
    })
}

/// Whether the App Store lifetime unlock is owned. Always false outside the
/// App Store edition.
fn app_store_unlocked() -> bool {
    crate::distribution::is_app_store() && app_store_unlocked_cell().load(Ordering::Relaxed)
}

/// Record StoreKit's answer to "does this Apple account own the lifetime
/// unlock?" (in memory and on disk). `false` clears it — a refunded purchase
/// starts the free-tier limits again.
pub fn record_app_store_entitlement(owned: bool) {
    if owned {
        let _ = crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            APP_STORE_UNLOCK_ARTIFACT,
            "owned",
        );
    } else {
        crate::host_storage::remove(
            crate::host_storage::StorageArea::Data,
            APP_STORE_UNLOCK_ARTIFACT,
        );
    }
    app_store_unlocked_cell().store(owned, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Free-tier saved-connection limit (Mac App Store edition)
// ---------------------------------------------------------------------------

/// How many saved connections the free tier keeps.
pub const FREE_SAVED_CONNECTIONS: usize = 2;

/// The saved-connection cap that applies, or `None` for unlimited. Pure.
pub fn saved_connection_limit(enforced: bool, paying: bool) -> Option<usize> {
    (enforced && !paying).then_some(FREE_SAVED_CONNECTIONS)
}

/// [`saved_connection_limit`] for this edition and the live license.
///
/// A relay-proxied connection list (the browser build behind a
/// `sqlly-relay --proxy-*` or a SQLly Embedded bridge) is never capped: the
/// bridge's license covers its users, the list is read-only, and a profile
/// may hold any number of servers and databases.
pub fn saved_connection_limit_now() -> Option<usize> {
    saved_connection_limit_for(
        crate::distribution::enforces_free_connection_limit(),
        is_paying(),
        crate::connection::connections_are_proxied(),
    )
}

/// [`saved_connection_limit`] with the proxied-list exemption. Pure.
pub fn saved_connection_limit_for(enforced: bool, paying: bool, proxied: bool) -> Option<usize> {
    if proxied {
        return None;
    }
    saved_connection_limit(enforced, paying)
}

/// Whether `adding` more saved connections fit next to `existing` ones. Pure.
pub fn fits_saved_connection_limit(limit: Option<usize>, existing: usize, adding: usize) -> bool {
    limit.is_none_or(|limit| existing.saturating_add(adding) <= limit)
}

/// Whether the saved connection at `position` (store order, 0-based) may be
/// opened. Connections beyond the cap — imported, or left over from a
/// refunded purchase — stay saved but locked. Pure.
pub fn saved_connection_usable(limit: Option<usize>, position: usize) -> bool {
    limit.is_none_or(|limit| position < limit)
}

/// Why the unlock window opened, shown as its lead line.
pub fn connection_limit_reason(limit: usize) -> String {
    crate::localization::format("license.connection_limit", &[("limit", &limit.to_string())])
}

/// Store the freshest entitlement answer from the store: the subscription's
/// period end when it grants access, or `None` for free tier / no access —
/// which CLEARS the marker so a lapsed license starts messaging again.
pub fn record_entitlement(paid_until: Option<DateTime<Utc>>) {
    match paid_until {
        Some(until) => {
            let _ = crate::host_storage::store_text(
                crate::host_storage::StorageArea::Data,
                PAID_UNTIL_ARTIFACT,
                &until.to_rfc3339(),
            );
        }
        None => {
            crate::host_storage::remove(
                crate::host_storage::StorageArea::Data,
                PAID_UNTIL_ARTIFACT,
            );
        }
    }
    *paid_until_cell()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = paid_until;
}

/// The entitlement marker a fetched subscription yields:
/// its period end when it currently grants access, else `None`.
pub fn entitlement_from_subscription(
    subscription: Option<&crate::store::Subscription>,
) -> Option<DateTime<Utc>> {
    subscription
        .filter(|s| s.has_access)
        .map(|s| s.current_period_end)
}

/// Whole days a build has been in use: `today - build_date`. Negative if the
/// clock is set before the build date.
pub fn days_in_use(build: NaiveDate, today: NaiveDate) -> i64 {
    (today - build).num_days()
}

/// Whether the free-tier nag is active: true whenever the app is unlicensed,
/// and never in a standalone build (license checks are off there).
pub fn should_nag_now() -> bool {
    crate::distribution::license_checks_enabled() && !is_paying()
}

// ---------------------------------------------------------------------------
// Supporter message (status-bar chip + banner)
// ---------------------------------------------------------------------------

/// Whether the supporter message should be shown.
///
/// Pure. Unpaid users see it whenever the nag is active; paying users only see
/// it if they opted to keep it (off by default once you pay).
pub fn should_show_supporter_message(paying: bool, nag_active: bool, keep_pref: bool) -> bool {
    if paying { keep_pref } else { nag_active }
}

/// [`should_show_supporter_message`] against the live entitlement, day gate,
/// and persisted preference. Reads the preference only when paying (the
/// unpaid path runs on every status-bar render and must stay I/O-free).
pub fn should_show_supporter_message_now() -> bool {
    let paying = is_paying();
    // Read the preference only when it matters (paying); the unpaid path stays
    // I/O-free because `keep` is ignored there.
    let keep = paying && keep_supporter_message();
    should_show_supporter_message(paying, should_nag_now(), keep)
}

/// The Preferences → Appearance toggle: keep the supporter message even after
/// paying. Stored on [`crate::preferences::AppearancePreferences`]; defaults
/// to `false`.
pub fn keep_supporter_message() -> bool {
    crate::preferences::AppearancePreferences::load().keep_supporter_message
}

pub fn status_bar_message() -> String {
    crate::localization::text("license.supporter_status")
}

// ---------------------------------------------------------------------------
// Pay dialog (modal)
// ---------------------------------------------------------------------------

/// Body copy of the pay dialog. The re-nag cadence is interpolated from
/// [`NAG_DIALOG_INTERVAL_HOURS`] so the copy can never drift from the gate.
pub fn nag_dialog_body() -> String {
    crate::localization::format(
        "license.reminder_body",
        &[("hours", &NAG_DIALOG_INTERVAL_HOURS.to_string())],
    )
}

pub fn nag_dialog_removal_note() -> String {
    crate::localization::text("license.reminder_removal_note")
}

pub fn nag_dialog_pay_label() -> String {
    crate::localization::text("license.reminder_pay_button")
}

/// Secondary (dismiss) button. Names the real re-nag cadence.
pub fn nag_dialog_later_label() -> String {
    crate::localization::format(
        "license.reminder_later_button",
        &[("hours", &NAG_DIALOG_INTERVAL_HOURS.to_string())],
    )
}

/// The dialog's query-counter line, e.g. `"Queries run so far: 12,345"`.
pub fn nag_dialog_query_count_line(query_count: u64) -> String {
    crate::localization::format(
        "license.reminder_query_count",
        &[("count", &format_thousands(query_count))],
    )
}

/// The dialog's heading, e.g. `"Day 91 with SQLLY. Thanks for being here!"`.
pub fn nag_dialog_heading(days_in_use: i64) -> String {
    crate::localization::format(
        "license.reminder_heading",
        &[("days", &days_in_use.to_string())],
    )
}

/// [`nag_dialog_heading`] against the baked build date and today.
pub fn nag_dialog_heading_now() -> String {
    nag_dialog_heading(days_in_use(
        crate::build_info::build_date(),
        crate::build_info::today(),
    ))
}

/// Where the persisted "last time the pay dialog was shown" timestamp lives.
const NAG_LAST_SHOWN_ARTIFACT: &str = "nag-last-shown.txt";

/// The last time the pay dialog was shown, if ever.
pub fn load_last_nag_shown() -> Option<DateTime<Utc>> {
    let raw = crate::host_storage::load_text(
        crate::host_storage::StorageArea::Data,
        NAG_LAST_SHOWN_ARTIFACT,
    )?;
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Persist that the pay dialog was shown at `now`.
pub fn record_nag_shown(now: DateTime<Utc>) {
    let _ = crate::host_storage::store_text(
        crate::host_storage::StorageArea::Data,
        NAG_LAST_SHOWN_ARTIFACT,
        &now.to_rfc3339(),
    );
}

// ---------------------------------------------------------------------------
// Query counter (persisted, all-time)
// ---------------------------------------------------------------------------

/// Where the persisted "queries run, all time" count lives.
const QUERY_COUNT_ARTIFACT: &str = "query-count.txt";

/// The number of queries run across all sessions (0 if never persisted or the
/// artifact is unreadable/corrupt — a lost count must never break a run).
pub fn load_query_count() -> u64 {
    crate::host_storage::load_text(crate::host_storage::StorageArea::Data, QUERY_COUNT_ARTIFACT)
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(0)
}

/// Increment the persisted all-time query counter and return the new total.
/// Called once per user-initiated query run.
pub fn increment_query_count() -> u64 {
    let next = load_query_count().saturating_add(1);
    let _ = crate::host_storage::store_text(
        crate::host_storage::StorageArea::Data,
        QUERY_COUNT_ARTIFACT,
        &next.to_string(),
    );
    next
}

/// Format a count with comma thousands separators: `1234567` → `"1,234,567"`.
///
/// Pure so it can be unit-tested (per AGENTS.md).
pub fn format_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Whether the pay dialog should be shown now.
///
/// Pure. Shown only when the nag is active AND it has either never been shown
/// or at least [`NAG_DIALOG_INTERVAL_HOURS`] hours have passed since last time.
pub fn should_show_nag_dialog(
    nag_active: bool,
    last_shown: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    if !nag_active {
        return false;
    }
    match last_shown {
        None => true,
        Some(prev) => now.signed_duration_since(prev) >= Duration::hours(NAG_DIALOG_INTERVAL_HOURS),
    }
}

/// [`should_show_nag_dialog`] against the live nag gate, the persisted
/// last-shown time, and the current clock.
///
/// Never in the Mac App Store edition: App Review guideline 2.4.5(vii) bars
/// a license or purchase screen at launch. There the unlock lives behind
/// SQLLY ▸ Unlock SQLLY…, the status-bar supporter link, and the free-tier
/// limit prompts.
#[cfg(not(target_arch = "wasm32"))]
pub fn should_show_nag_dialog_now() -> bool {
    !crate::distribution::is_app_store()
        && should_show_nag_dialog(should_nag_now(), load_last_nag_shown(), Utc::now())
}

/// The in-tab browser preview is a labeled demo with nothing to license — it
/// can't reach your servers and isn't the product — so the pay dialog never
/// fires there. (The pure [`should_show_nag_dialog`] gate is still exercised by
/// tests; only this host-facing entry point is short-circuited.)
#[cfg(target_arch = "wasm32")]
pub fn should_show_nag_dialog_now() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        date(y, m, d).and_hms_opt(h, 0, 0).unwrap().and_utc()
    }

    #[test]
    fn entitlement_gate_is_time_bounded() {
        let now = at(2026, 8, 11, 12);
        // No marker: free tier.
        assert!(!is_paying_at(None, now));
        // A live license grants access…
        assert!(is_paying_at(Some(now + Duration::days(30)), now));
        // …a lapsed one does not (cancelled plans keep access to period end,
        // then the messaging returns).
        assert!(!is_paying_at(Some(now - Duration::seconds(1)), now));
        // Lifetime/comped grants use a far-future period end.
        assert!(is_paying_at(Some(at(9999, 1, 1, 0)), now));
    }

    #[test]
    fn free_tier_connection_limit_applies_only_when_enforced_and_unpaid() {
        assert_eq!(
            saved_connection_limit(true, false),
            Some(FREE_SAVED_CONNECTIONS)
        );
        assert_eq!(saved_connection_limit(true, true), None);
        assert_eq!(saved_connection_limit(false, false), None);
        assert_eq!(saved_connection_limit(false, true), None);
    }

    /// A relay-proxied list is never capped, whatever the edition or the
    /// paid state: the bridge's license covers its users.
    #[test]
    fn proxied_connection_lists_are_never_capped() {
        assert_eq!(
            saved_connection_limit_for(true, false, false),
            Some(FREE_SAVED_CONNECTIONS)
        );
        assert_eq!(saved_connection_limit_for(true, false, true), None);
        assert_eq!(saved_connection_limit_for(true, true, true), None);
        assert_eq!(saved_connection_limit_for(false, false, true), None);
    }

    #[test]
    fn saved_connection_limit_counts_existing_plus_new() {
        let limit = Some(2);
        assert!(fits_saved_connection_limit(limit, 0, 1));
        assert!(fits_saved_connection_limit(limit, 1, 1));
        assert!(!fits_saved_connection_limit(limit, 2, 1));
        assert!(fits_saved_connection_limit(limit, 0, 2));
        assert!(!fits_saved_connection_limit(limit, 1, 2));
        assert!(fits_saved_connection_limit(None, 500, 500));
        assert!(!fits_saved_connection_limit(limit, usize::MAX, 0));
    }

    #[test]
    fn connections_beyond_the_cap_are_locked() {
        assert!(saved_connection_usable(Some(2), 0));
        assert!(saved_connection_usable(Some(2), 1));
        assert!(!saved_connection_usable(Some(2), 2));
        assert!(saved_connection_usable(None, 99));
    }

    #[test]
    fn connection_limit_reason_names_the_cap() {
        let reason = connection_limit_reason(2);
        assert!(reason.contains("up to 2 connections"));
        assert!(reason.contains("Unlock SQLLY"));
    }

    #[cfg(not(feature = "app-store"))]
    #[test]
    fn app_store_marker_is_ignored_outside_the_app_store_edition() {
        assert!(!app_store_unlocked());
        assert_eq!(saved_connection_limit_now(), None);
    }

    #[test]
    fn policy_and_deletion_links_are_https_sqlly_pages() {
        for url in [PRIVACY_URL, TERMS_URL, DELETE_ACCOUNT_URL] {
            assert!(
                url.starts_with("https://") && url.contains("sqlly.app"),
                "{url}"
            );
        }
        assert!(delete_account_url().starts_with(DELETE_ACCOUNT_URL));
        if crate::distribution::is_app_store() {
            assert_eq!(
                delete_account_url(),
                format!("{DELETE_ACCOUNT_URL}?app=mas")
            );
        } else {
            assert_eq!(delete_account_url(), DELETE_ACCOUNT_URL);
        }
    }

    #[test]
    fn pricing_url_is_the_sqlly_pricing_page() {
        assert!(PRICING_URL.starts_with("https://"));
        assert!(PRICING_URL.contains("sqlly.app"));
        assert!(PRICING_URL.contains("pricing"));
    }

    #[test]
    fn licenses_url_is_the_sqlly_licenses_page() {
        assert!(LICENSES_URL.starts_with("https://"));
        assert!(LICENSES_URL.contains("sqlly.app"));
        assert!(LICENSES_URL.contains("licenses"));
    }

    #[test]
    fn supporter_message_is_upbeat_and_non_empty() {
        assert!(!status_bar_message().trim().is_empty());
        assert!(status_bar_message().to_lowercase().contains("love"));
    }

    #[test]
    fn days_in_use_counts_from_the_build_date() {
        assert_eq!(days_in_use(date(2026, 1, 1), date(2026, 1, 1)), 0);
        assert_eq!(days_in_use(date(2026, 1, 1), date(2026, 4, 1)), 90);
        assert_eq!(days_in_use(date(2026, 1, 2), date(2026, 1, 1)), -1);
    }

    #[test]
    fn nag_is_active_whenever_unlicensed() {
        // No day gate: the nag mirrors the license state exactly — an
        // unlicensed app nags from day one, a licensed one never does.
        assert_eq!(should_nag_now(), !is_paying());
    }

    #[test]
    fn supporter_message_follows_pay_state_and_preference() {
        // Unpaid: driven purely by the day gate, regardless of the keep pref.
        assert!(should_show_supporter_message(false, true, false));
        assert!(should_show_supporter_message(false, true, true));
        assert!(!should_show_supporter_message(false, false, true));
        // Paid: off by default, on only when kept.
        assert!(!should_show_supporter_message(true, true, false));
        assert!(should_show_supporter_message(true, false, true));
    }

    #[test]
    fn pay_dialog_hidden_when_nag_inactive() {
        assert!(!should_show_nag_dialog(false, None, at(2026, 6, 1, 12)));
        assert!(!should_show_nag_dialog(
            false,
            Some(at(2026, 6, 1, 0)),
            at(2027, 1, 1, 0)
        ));
    }

    #[test]
    fn pay_dialog_shows_first_time_then_respects_the_interval() {
        let now = at(2026, 6, 1, 12);
        // Never shown before: show.
        assert!(should_show_nag_dialog(true, None, now));
        // Shown one hour short of the interval: still within the window, quiet.
        let recent = now - Duration::hours(NAG_DIALOG_INTERVAL_HOURS - 1);
        assert!(!should_show_nag_dialog(true, Some(recent), now));
        // Shown exactly the interval ago: due again.
        let due = now - Duration::hours(NAG_DIALOG_INTERVAL_HOURS);
        assert!(should_show_nag_dialog(true, Some(due), now));
    }

    #[test]
    fn nag_dialog_heading_thanks_the_user() {
        assert_eq!(
            nag_dialog_heading(91),
            "Day 91 with SQLLY. Thanks for being here!"
        );
    }

    #[test]
    fn format_thousands_groups_by_three() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(7), "7");
        assert_eq!(format_thousands(42), "42");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1000), "1,000");
        assert_eq!(format_thousands(12345), "12,345");
        assert_eq!(format_thousands(1234567), "1,234,567");
        assert_eq!(format_thousands(u64::MAX), "18,446,744,073,709,551,615");
    }

    #[test]
    fn query_count_line_uses_thousands_separators() {
        assert_eq!(nag_dialog_query_count_line(0), "Queries run so far: 0");
        assert_eq!(
            nag_dialog_query_count_line(1234567),
            "Queries run so far: 1,234,567"
        );
    }

    #[test]
    fn dialog_copy_names_the_real_renag_cadence() {
        // The dialog promises a re-nag in NAG_DIALOG_INTERVAL_HOURS hours; the
        // copy must interpolate the constant so text and gate never drift.
        let hours = format!("{NAG_DIALOG_INTERVAL_HOURS} hours");
        assert!(nag_dialog_body().contains(&hours));
        assert!(nag_dialog_later_label().contains(&hours));
    }

    #[test]
    fn dialog_copy_is_friendly_and_keeps_the_purchase_cta() {
        assert_eq!(nag_dialog_pay_label(), "Okay, fine — take my money");
        assert!(nag_dialog_body().contains("No rush"));
        assert!(nag_dialog_removal_note().contains("turns off these reminders"));
        assert!(nag_dialog_later_label().starts_with("Maybe later"));
    }
}
