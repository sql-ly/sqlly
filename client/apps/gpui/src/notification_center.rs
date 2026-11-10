//! In-app notification center: a bounded, newest-first record of events
//! worth seeing after looking away — long query completions, failures, and
//! available updates — behind a status-bar bell.
//!
//! Pure model: the ring buffer, unread accounting, and formatting carry no
//! GPUI types, so the retention and read/unread rules are directly testable.

/// Most notifications kept; older ones fall off the end.
pub const NOTIFICATION_CAP: usize = 100;

/// Completions faster than this stay out of the center — they were watched
/// live. Failures are always recorded.
pub const LONG_QUERY_NOTICE_SECS: u64 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationKind {
    QueryCompleted,
    QueryFailed,
    UpdateAvailable,
    /// A file/export deliverable finished writing — a result export, a table
    /// data export, or generated schema documentation.
    ExportSaved,
}

impl NotificationKind {
    pub fn label(self) -> String {
        let key = match self {
            Self::QueryCompleted => "notifications.kind.query_completed",
            Self::QueryFailed => "notifications.kind.query_failed",
            Self::UpdateAvailable => "notifications.kind.update_available",
            Self::ExportSaved => "notifications.kind.export_saved",
        };
        crate::localization::text(key)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppNotification {
    pub kind: NotificationKind,
    pub message: String,
    /// UTC time the event was recorded, as the logger's own format.
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub read: bool,
    pub reveal_path: Option<std::path::PathBuf>,
}

/// Newest-first bounded buffer of notifications.
#[derive(Default)]
pub struct NotificationCenter {
    items: Vec<AppNotification>,
}

impl NotificationCenter {
    pub fn push(
        &mut self,
        kind: NotificationKind,
        message: impl Into<String>,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        self.push_with_path(kind, message, now, None);
    }

    pub fn push_with_path(
        &mut self,
        kind: NotificationKind,
        message: impl Into<String>,
        now: chrono::DateTime<chrono::Utc>,
        reveal_path: Option<std::path::PathBuf>,
    ) {
        self.items.insert(
            0,
            AppNotification {
                kind,
                message: message.into(),
                timestamp: now,
                read: false,
                reveal_path,
            },
        );
        self.items.truncate(NOTIFICATION_CAP);
    }

    pub fn items(&self) -> &[AppNotification] {
        &self.items
    }

    pub fn unread_count(&self) -> usize {
        self.items.iter().filter(|item| !item.read).count()
    }

    pub fn mark_all_read(&mut self) {
        for item in &mut self.items {
            item.read = true;
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}

/// Whether a successful run belongs in the center: only ones long enough
/// that the user plausibly looked away.
pub fn completion_is_noteworthy(duration_ms: Option<u64>) -> bool {
    duration_ms.is_some_and(|ms| ms >= LONG_QUERY_NOTICE_SECS * 1000)
}

/// Whether long-query completions and failures also raise a system
/// notification while the window is in the background. Persisted beside the
/// other preference files.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct OsNotifyPreferences {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Default for OsNotifyPreferences {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl OsNotifyPreferences {
    #[cfg(not(target_arch = "wasm32"))]
    fn path() -> std::path::PathBuf {
        let mut p = crate::preferences::preferences_path();
        p.push("os-notifications.json");
        p
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn save(&self) {
        if let Ok(text) = crate::config_version::to_string_pretty(self) {
            let _ = std::fs::write(Self::path(), text);
        }
    }

    /// Browser build: no system notifications; the toggle is inert.
    #[cfg(target_arch = "wasm32")]
    pub fn load() -> Self {
        Self::default()
    }

    #[cfg(target_arch = "wasm32")]
    pub fn save(&self) {}
}

/// The AppleScript source for a notification, with quotes and backslashes
/// escaped so arbitrary titles/messages cannot break out of the string
/// literals. Pure, so the escaping contract is unit tested. Only the unbundled
/// dev-build fallback uses it.
#[cfg(any(test, target_os = "macos"))]
#[cfg_attr(feature = "app-store", allow(dead_code))]
fn osascript_notification(title: &str, message: &str) -> String {
    fn escape(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }
    format!(
        "display notification \"{}\" with title \"{}\"",
        escape(message),
        escape(title)
    )
}

/// Raise a system-level notification, best-effort and fire-and-forget.
///
/// macOS posts through `UNUserNotificationCenter` (via GPUI) so the banner
/// carries SQLLY's own icon and name; the old `osascript` route attributed
/// every banner to Script Editor. That API needs an app bundle, so a bare
/// `cargo run` binary falls back to `osascript`. Linux uses `notify-send`
/// when present; Windows has no dependency-free toast path, so it stays
/// in-app only there.
#[cfg(not(target_arch = "wasm32"))]
pub fn notify_system(title: &str, message: &str, cx: &gpui::App) {
    // Mac App Store edition: the Swift glue owns the Notification Center.
    #[cfg(all(feature = "app-store", target_os = "macos"))]
    {
        let _ = cx;
        crate::app_store::notify(title, message);
    }
    #[cfg(all(target_os = "macos", not(feature = "app-store")))]
    {
        if running_from_app_bundle() {
            cx.show_system_notification(gpui::SystemNotification {
                tag: format!("sqlly-{}", uuid::Uuid::new_v4()).into(),
                title: title.to_string().into(),
                body: message.to_string().into(),
                actions: Vec::new(),
            });
        } else {
            let script = osascript_notification(title, message);
            let _ = std::process::Command::new("osascript")
                .arg("-e")
                .arg(script)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
    }
    #[cfg(target_os = "linux")]
    {
        let _ = cx;
        let _ = std::process::Command::new("notify-send")
            .arg(title)
            .arg(message)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (title, message, cx);
    }
}

/// Whether this process runs from inside a `.app` bundle — the condition
/// `UNUserNotificationCenter` needs (GPUI silently drops notifications
/// otherwise).
#[cfg(all(target_os = "macos", not(feature = "app-store")))]
fn running_from_app_bundle() -> bool {
    std::env::current_exe().is_ok_and(|exe| {
        exe.ancestors()
            .any(|dir| dir.extension().is_some_and(|ext| ext == "app"))
    })
}

#[cfg(target_arch = "wasm32")]
pub fn notify_system(_title: &str, _message: &str, _cx: &gpui::App) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-08-25T10:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn newest_first_bounded_and_unread_counted() {
        let mut center = NotificationCenter::default();
        for index in 0..(NOTIFICATION_CAP + 5) {
            center.push(NotificationKind::QueryCompleted, format!("q{index}"), now());
        }
        assert_eq!(center.items().len(), NOTIFICATION_CAP);
        assert_eq!(
            center.items()[0].message,
            format!("q{}", NOTIFICATION_CAP + 4)
        );
        assert_eq!(center.unread_count(), NOTIFICATION_CAP);
        center.mark_all_read();
        assert_eq!(center.unread_count(), 0);
        center.push(NotificationKind::QueryFailed, "boom", now());
        assert_eq!(center.unread_count(), 1);
        center.clear();
        assert!(center.items().is_empty());
    }

    #[test]
    fn export_saved_is_recorded_with_its_own_label() {
        let mut center = NotificationCenter::default();
        center.push(
            NotificationKind::ExportSaved,
            "Saved CSV to /tmp/out.csv",
            now(),
        );
        assert_eq!(center.items().len(), 1);
        assert_eq!(center.items()[0].kind, NotificationKind::ExportSaved);
        assert_eq!(NotificationKind::ExportSaved.label(), "Export saved");
        assert_eq!(center.unread_count(), 1);
    }

    #[test]
    fn only_long_completions_are_noteworthy() {
        assert!(!completion_is_noteworthy(None));
        assert!(!completion_is_noteworthy(Some(9_999)));
        assert!(completion_is_noteworthy(Some(10_000)));
    }

    #[test]
    fn push_with_path_keeps_reveal_target() {
        let mut center = NotificationCenter::default();
        let path = std::path::PathBuf::from("/tmp/results.csv");
        center.push_with_path(
            NotificationKind::ExportSaved,
            "Saved CSV",
            now(),
            Some(path.clone()),
        );
        assert_eq!(center.items()[0].reveal_path, Some(path));
        assert_eq!(center.unread_count(), 1);
    }

    #[test]
    fn osascript_escaping_cannot_break_out_of_the_literals() {
        let script = osascript_notification("SQLLY", "plain message");
        assert_eq!(
            script,
            "display notification \"plain message\" with title \"SQLLY\""
        );
        // Quotes and backslashes in server-derived text stay inert.
        let hostile = osascript_notification("t\"itle", "say \"hi\" \\ bye");
        assert_eq!(
            hostile,
            "display notification \"say \\\"hi\\\" \\\\ bye\" with title \"t\\\"itle\""
        );
        assert_eq!(
            hostile.matches('"').count() - hostile.matches("\\\"").count(),
            4,
            "exactly the four delimiter quotes remain unescaped"
        );
    }

    #[test]
    fn os_notify_preferences_default_on_and_survive_serde() {
        let prefs = OsNotifyPreferences::default();
        assert!(prefs.enabled);
        let text = serde_json::to_string(&OsNotifyPreferences { enabled: false }).unwrap();
        let back: OsNotifyPreferences = serde_json::from_str(&text).unwrap();
        assert!(!back.enabled);
        // Missing/extra fields fall back safely.
        let legacy: OsNotifyPreferences = serde_json::from_str("{}").unwrap();
        assert!(legacy.enabled, "absent field defaults to on");
        assert!(serde_json::from_str::<OsNotifyPreferences>("garbage").is_err());
    }
}
