//! Deciding whether a newer desktop build is available, from the store's
//! changelog feed. Pure logic (no I/O); the fetch lives in [`crate::store`] and
//! the UI in `app.rs`.

use crate::store::{ChangelogFeedVersion, ChangelogSince, LatestVersion};

/// A detected update: the newest published version plus every version newer than
/// what's installed, so the dialog can show everything the user would gain.
#[derive(Debug, Clone)]
pub struct UpdateAvailable {
    pub latest: String,
    /// Versions strictly newer than the installed build, newest first.
    pub versions: Vec<ChangelogFeedVersion>,
}

/// Parse a dotted version into a comparable `(major, minor, patch)`, tolerating
/// a leading `v` and fewer than three segments (missing tail segments are 0).
///
/// Returns `None` for a genuinely malformed version — an empty string or any
/// segment that is not a plain number (e.g. `"garbage"`, `"0.40.x"`,
/// `"0.40.0-rc1"`). A malformed *server* version must be treated as
/// non-actionable, not silently coerced to `0.0.0` (which would compare as
/// "older than anything" and could hide, or in edge cases fake, an update).
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let trimmed = v.trim().trim_start_matches('v').trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut out = [0u64; 3];
    for (i, seg) in trimmed.split('.').enumerate() {
        let n: u64 = seg.trim().parse().ok()?;
        if i < 3 {
            out[i] = n;
        }
        // Segments beyond patch must still be numeric (parsed above) but are
        // ignored for comparison.
    }
    Some((out[0], out[1], out[2]))
}

/// Whether `candidate` is a strictly newer version than `installed`. An
/// unparseable `candidate` is never newer (non-actionable) and is logged so a
/// broken server feed is diagnosable rather than silently ignored.
pub fn is_newer(candidate: &str, installed: &str) -> bool {
    let Some(candidate_v) = parse_version(candidate) else {
        tracing::warn!(version = %candidate, "ignoring unparseable update version from server");
        return false;
    };
    let Some(installed_v) = parse_version(installed) else {
        // The installed build's own version should always parse; if it somehow
        // does not, refuse to advertise an update rather than guess.
        tracing::warn!(version = %installed, "installed version is unparseable");
        return false;
    };
    candidate_v > installed_v
}

/// How long "Remind me later" hides an update.
pub const SNOOZE_HOURS: i64 = 24;

/// Whether `update` should be surfaced (chip + dialog) given the user's
/// persisted choices. `skipped` is a version the user asked to skip: only a
/// version strictly newer than it re-prompts. `snoozed_until` hides every
/// update until that instant passes.
pub fn should_surface(
    update: &UpdateAvailable,
    skipped: Option<&str>,
    snoozed_until: Option<chrono::DateTime<chrono::Utc>>,
    now: chrono::DateTime<chrono::Utc>,
) -> bool {
    if let Some(until) = snoozed_until
        && now < until
    {
        return false;
    }
    match skipped.filter(|s| parse_version(s).is_some()) {
        Some(skipped) => is_newer(&update.latest, skipped),
        None => true,
    }
}

/// Whether a deferred install may proceed: nothing is in flight.
pub fn idle_install_ready(in_flight: &InFlightWork) -> bool {
    in_flight.is_idle()
}

/// Minimal count of user-visible work in progress. `exports` is the
/// background task center (plan 188); overlay dump/transfer flags remain as
/// a fallback for jobs that have not registered yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InFlightWork {
    pub queries: usize,
    pub dumps: usize,
    pub transfers: usize,
    pub ai_runs: usize,
    pub exports: usize,
    pub manual_tx: usize,
}

impl InFlightWork {
    pub fn is_idle(&self) -> bool {
        self.queries + self.dumps + self.transfers + self.ai_runs + self.exports + self.manual_tx
            == 0
    }

    /// One-line status text, e.g. "2 queries, 1 dump" — used by the dialog.
    pub fn summary(&self) -> String {
        fn part(count: usize, singular: &str, plural: &str) -> Option<String> {
            (count > 0).then(|| {
                let key = if count == 1 { singular } else { plural };
                crate::localization::format(key, &[("count", &count.to_string())])
            })
        }
        [
            part(
                self.queries,
                "updates.work.query_singular",
                "updates.work.query_plural",
            ),
            part(
                self.dumps,
                "updates.work.dump_singular",
                "updates.work.dump_plural",
            ),
            part(
                self.transfers,
                "updates.work.transfer_singular",
                "updates.work.transfer_plural",
            ),
            part(
                self.ai_runs,
                "updates.work.ai_run_singular",
                "updates.work.ai_run_plural",
            ),
            part(
                self.exports,
                "updates.work.task_singular",
                "updates.work.task_plural",
            ),
            part(
                self.manual_tx,
                "updates.work.transaction_singular",
                "updates.work.transaction_plural",
            ),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ")
    }
}

/// Trims a version to its display form: at most the leading `major.minor.patch`
/// segments, dropping a leading `v` and any 4th "build" segment the download
/// route carries (e.g. `"0.44.1.0"` → `"0.44.1"`). Keeps the update UI's version
/// consistent with the app's own `major.minor.patch` build string.
pub fn display_version(version: &str) -> String {
    version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .take(3)
        .collect::<Vec<_>>()
        .join(".")
}

/// The store's human download page (every platform and package), shown as a
/// visible link in the update dialog alongside the per-platform download
/// button. `base` is the store root, e.g. [`crate::store::base_url`].
pub fn download_page_url(base: &str) -> String {
    format!("{}/download", base.trim().trim_end_matches('/'))
}

/// The link text for a URL: the address without its scheme, so the dialog
/// reads `sqlly.app/download` rather than `https://sqlly.app/download`.
pub fn link_label(url: &str) -> String {
    let url = url.trim();
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url)
        .to_string()
}

/// Combines the two update endpoints into what the UI shows: the latest build
/// downloadable for THIS machine's OS + architecture (already confirmed newer
/// than the installed one) plus, when available, the changelog diff for the
/// "what's new" list.
///
/// The announced version is always the platform's downloadable build, never the
/// changelog's newest release: platforms publish independently (a macOS build
/// can lag Windows/Linux), and announcing a release this machine can't download
/// would send the user to a build older than promised. The changelog diff is
/// trimmed to match, dropping releases newer than that build. A missing or
/// empty changelog still surfaces the update, just without a change list.
pub fn build_update(download: &LatestVersion, since: Option<ChangelogSince>) -> UpdateAvailable {
    let latest = display_version(&download.version);
    let versions = since
        .map(|since| {
            since
                .versions
                .into_iter()
                .filter(|v| !is_newer(&v.version, &latest))
                .collect()
        })
        .unwrap_or_default();
    UpdateAvailable { latest, versions }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::store::{ChangelogFeedSection, ChangelogFeedVersion};

    fn latest(version: &str) -> LatestVersion {
        LatestVersion {
            version: version.to_string(),
            ..Default::default()
        }
    }

    fn since(versions: &[&str]) -> ChangelogSince {
        ChangelogSince {
            versions: versions
                .iter()
                .map(|v| ChangelogFeedVersion {
                    version: v.to_string(),
                    date: String::new(),
                    sections: vec![ChangelogFeedSection {
                        title: "s".to_string(),
                        details: vec![],
                    }],
                })
                .collect(),
        }
    }

    #[test]
    fn newer_version_is_detected() {
        assert!(is_newer("0.38.0", "0.37.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.37.1", "0.37.0"));
        assert!(!is_newer("0.37.0", "0.37.0"));
        assert!(!is_newer("0.36.0", "0.37.0"));
        // Tolerates a leading v and differing segment counts.
        assert!(is_newer("v0.38", "0.37.0"));
    }

    #[test]
    fn build_update_prefers_changelog_diff() {
        // The download route reports a padded 4-segment build; the changelog
        // supplies the diff. The announced version is the trimmed download build.
        let download = latest("0.39.0.0");
        let feed = since(&["0.39.0", "0.38.0"]);
        let update = build_update(&download, Some(feed));
        assert_eq!(update.latest, "0.39.0");
        assert_eq!(
            update
                .versions
                .iter()
                .map(|v| v.version.as_str())
                .collect::<Vec<_>>(),
            vec!["0.39.0", "0.38.0"]
        );
    }

    #[test]
    fn build_update_announces_only_this_platforms_build() {
        // The changelog has 0.97.3, but this platform's newest downloadable
        // build is 0.97.1: announce 0.97.1 and hide the releases it lacks.
        let download = latest("0.97.1.0");
        let feed = since(&["0.97.3", "0.97.2", "0.97.1", "0.97.0"]);
        let update = build_update(&download, Some(feed));
        assert_eq!(update.latest, "0.97.1");
        assert_eq!(
            update
                .versions
                .iter()
                .map(|v| v.version.as_str())
                .collect::<Vec<_>>(),
            vec!["0.97.1", "0.97.0"]
        );
    }

    #[test]
    fn build_update_falls_back_to_download_version() {
        // Changelog fetch failed (None) or returned nothing new: still surface the
        // update, using the download build's version trimmed to display form.
        let download = latest("0.39.0.0");
        let update = build_update(&download, None);
        assert_eq!(update.latest, "0.39.0");
        assert!(update.versions.is_empty());

        let empty = since(&[]);
        let update = build_update(&download, Some(empty));
        assert_eq!(update.latest, "0.39.0");
        assert!(update.versions.is_empty());
    }

    #[test]
    fn download_page_link_points_at_the_store_download_page() {
        assert_eq!(
            download_page_url("https://sqlly.app"),
            "https://sqlly.app/download"
        );
        assert_eq!(
            download_page_url("http://localhost:5000/"),
            "http://localhost:5000/download"
        );
        assert_eq!(
            link_label("https://sqlly.app/download"),
            "sqlly.app/download"
        );
        assert_eq!(
            link_label("http://localhost:5000/download"),
            "localhost:5000/download"
        );
        assert_eq!(link_label("sqlly.app/download"), "sqlly.app/download");
    }

    #[test]
    fn display_version_trims_prefix_and_build_segment() {
        assert_eq!(display_version("0.44.1.0"), "0.44.1");
        assert_eq!(display_version("v0.44.1"), "0.44.1");
        assert_eq!(display_version("1"), "1");
        assert_eq!(display_version(" 0.44.1 "), "0.44.1");
    }

    #[test]
    fn malformed_server_version_is_non_actionable() {
        // Garbage, empty, and pre-release-tagged versions must NOT be coerced to
        // 0.0.0 and must never advertise an update.
        assert!(!is_newer("garbage", "0.37.0"));
        assert!(!is_newer("", "0.37.0"));
        assert!(!is_newer("0.40.x", "0.37.0"));
        assert!(!is_newer("0.40.0-rc1", "0.37.0"));
    }

    #[test]
    fn parse_version_tolerates_short_forms_but_rejects_junk() {
        assert_eq!(parse_version("v0.41"), Some((0, 41, 0)));
        assert_eq!(parse_version("1"), Some((1, 0, 0)));
        assert_eq!(parse_version("0.41.0.2"), Some((0, 41, 0)));
        assert_eq!(parse_version("nope"), None);
        assert_eq!(parse_version(""), None);
    }

    fn update_at(version: &str) -> UpdateAvailable {
        UpdateAvailable {
            latest: version.to_string(),
            versions: Vec::new(),
        }
    }

    #[test]
    fn skipped_version_hides_same_and_older_but_not_newer() {
        let now = chrono::Utc::now();
        let skipped = Some("0.40.0");
        assert!(!should_surface(&update_at("0.40.0"), skipped, None, now));
        assert!(!should_surface(&update_at("0.39.9"), skipped, None, now));
        assert!(should_surface(&update_at("0.40.1"), skipped, None, now));
    }

    #[test]
    fn snooze_hides_until_expiry() {
        let now = chrono::Utc::now();
        let snoozed_until = Some(now + chrono::Duration::hours(1));
        assert!(!should_surface(
            &update_at("0.50.0"),
            None,
            snoozed_until,
            now
        ));
        assert!(should_surface(
            &update_at("0.50.0"),
            None,
            Some(now - chrono::Duration::seconds(1)),
            now
        ));
        assert!(!should_surface(
            &update_at("0.50.0"),
            Some("0.40.0"),
            snoozed_until,
            now
        ));
    }

    #[test]
    fn unparseable_skipped_version_does_not_hide() {
        let now = chrono::Utc::now();
        assert!(should_surface(
            &update_at("0.40.0"),
            Some("garbage"),
            None,
            now
        ));
    }

    #[test]
    fn in_flight_work_idle_and_summary() {
        let idle = InFlightWork::default();
        assert!(idle.is_idle());
        assert!(idle_install_ready(&idle));
        assert_eq!(idle.summary(), "");

        let busy = InFlightWork {
            queries: 2,
            dumps: 1,
            ..Default::default()
        };
        assert!(!busy.is_idle());
        assert_eq!(busy.summary(), "2 queries, 1 dump");

        let exporting = InFlightWork {
            exports: 1,
            ..Default::default()
        };
        assert!(!idle_install_ready(&exporting));
        assert_eq!(exporting.summary(), "1 background task");

        let open_tx = InFlightWork {
            manual_tx: 1,
            ..Default::default()
        };
        assert!(!idle_install_ready(&open_tx));
        assert_eq!(open_tx.summary(), "1 open transaction");
    }
}
