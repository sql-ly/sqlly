//! App-side coordination for DDL navigation (cmd/ctrl-click → go to
//! definition): monitor detection and popup geometry. Symbol resolution
//! lives in `sqlly_intel::intellisense::goto`; DDL fetching in
//! `sqlly_services::ddl`; the viewer in `crate::ddl_viewer`.

use gpui::{App, Bounds, DisplayId, Pixels, Window, point, px, size};

use crate::preferences::{
    DDL_POPUP_MIN_HEIGHT, DDL_POPUP_MIN_WIDTH, DdlPopupMonitorPreference, DdlPopupPreferences,
};

/// A detected monitor, described independently of GPUI's display handles so
/// the preferences preview and popup placement share one model.
#[derive(Debug, Clone)]
pub struct DdlMonitorInfo {
    pub id: Option<DisplayId>,
    /// Stable identifier persisted in preferences (survives restarts);
    /// `None` when the platform can't provide one.
    pub uuid: Option<String>,
    /// Human-readable dropdown label, includes the resolution.
    pub label: String,
    pub bounds: Bounds<Pixels>,
}

impl DdlMonitorInfo {
    pub fn resolution_label(bounds: &Bounds<Pixels>) -> String {
        format!(
            "{}×{}",
            f32::from(bounds.size.width).round() as i64,
            f32::from(bounds.size.height).round() as i64
        )
    }
}

/// Enumerate monitors via GPUI's display API. Returns an empty vec when the
/// platform reports none (navigation then falls back to the current window).
pub fn detect_monitors(cx: &App) -> Vec<DdlMonitorInfo> {
    cx.displays()
        .into_iter()
        .enumerate()
        .map(|(index, display)| {
            let bounds = display.bounds();
            DdlMonitorInfo {
                id: Some(display.id()),
                uuid: display.uuid().ok().map(|u| u.to_string()),
                label: crate::localization::format(
                    "editor.ddl_navigation.monitor_label",
                    &[
                        ("number", &(index + 1).to_string()),
                        ("resolution", &DdlMonitorInfo::resolution_label(&bounds)),
                    ],
                ),
                bounds,
            }
        })
        .collect()
}

/// The monitor of the window the click happened in, when the platform
/// reports it.
pub fn current_window_monitor(window: &Window, cx: &App) -> Option<DdlMonitorInfo> {
    let display = window.display(cx)?;
    let bounds = display.bounds();
    Some(DdlMonitorInfo {
        id: Some(display.id()),
        uuid: display.uuid().ok().map(|u| u.to_string()),
        label: crate::localization::format(
            "editor.ddl_navigation.current_monitor_label",
            &[("resolution", &DdlMonitorInfo::resolution_label(&bounds))],
        ),
        bounds,
    })
}

/// Pick the monitor the popup should open on: the persisted monitor when it
/// still exists, else the current window's monitor, else the first detected
/// one. `None` means no display information is available at all — center on
/// the current window instead.
pub fn select_monitor<'a>(
    preference: &DdlPopupMonitorPreference,
    monitors: &'a [DdlMonitorInfo],
    current: Option<&'a DdlMonitorInfo>,
) -> Option<&'a DdlMonitorInfo> {
    if let DdlPopupMonitorPreference::Monitor { id } = preference
        && let Some(found) = monitors
            .iter()
            .find(|m| m.uuid.as_deref() == Some(id.as_str()))
    {
        return Some(found);
    }
    current.or_else(|| monitors.first())
}

/// Compute the popup's window bounds on `monitor_bounds` from the persisted
/// geometry: size clamped between the minimums and the monitor, position as
/// a fraction of the leftover space so the popup can never land offscreen.
/// GPUI 0.2 exposes only raw display bounds (no dock/taskbar work area), so
/// clamping is against the full monitor rect.
pub fn popup_bounds(popup: &DdlPopupPreferences, monitor_bounds: Bounds<Pixels>) -> Bounds<Pixels> {
    let popup = popup.clamped();
    let monitor_width = f32::from(monitor_bounds.size.width).max(1.0);
    let monitor_height = f32::from(monitor_bounds.size.height).max(1.0);

    let width = (popup.width as f32)
        .clamp(DDL_POPUP_MIN_WIDTH as f32, monitor_width)
        .min(monitor_width);
    let height = (popup.height as f32)
        .clamp(DDL_POPUP_MIN_HEIGHT as f32, monitor_height)
        .min(monitor_height);

    let x = f32::from(monitor_bounds.origin.x) + (monitor_width - width) * popup.x_percent;
    let y = f32::from(monitor_bounds.origin.y) + (monitor_height - height) * popup.y_percent;

    Bounds::new(point(px(x), px(y)), size(px(width), px(height)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    fn prefs(width: u32, height: u32, x_percent: f32, y_percent: f32) -> DdlPopupPreferences {
        DdlPopupPreferences {
            width,
            height,
            x_percent,
            y_percent,
            monitor: DdlPopupMonitorPreference::CurrentWindow,
        }
    }

    #[test]
    fn popup_centers_on_monitor() {
        let bounds = popup_bounds(
            &prefs(900, 700, 0.5, 0.5),
            monitor(0.0, 0.0, 2560.0, 1440.0),
        );
        assert_eq!(f32::from(bounds.origin.x), (2560.0 - 900.0) / 2.0);
        assert_eq!(f32::from(bounds.origin.y), (1440.0 - 700.0) / 2.0);
        assert_eq!(f32::from(bounds.size.width), 900.0);
        assert_eq!(f32::from(bounds.size.height), 700.0);
    }

    #[test]
    fn popup_respects_monitor_origin_offset() {
        // Secondary monitor to the right of the primary.
        let bounds = popup_bounds(
            &prefs(800, 600, 0.0, 0.0),
            monitor(2560.0, 0.0, 1920.0, 1080.0),
        );
        assert_eq!(f32::from(bounds.origin.x), 2560.0);
        assert_eq!(f32::from(bounds.origin.y), 0.0);
    }

    #[test]
    fn oversized_popup_clamps_to_monitor() {
        let bounds = popup_bounds(
            &prefs(5000, 4000, 1.0, 1.0),
            monitor(0.0, 0.0, 1920.0, 1080.0),
        );
        assert_eq!(f32::from(bounds.size.width), 1920.0);
        assert_eq!(f32::from(bounds.size.height), 1080.0);
        // Fully clamped popup sits exactly on the monitor, never past it.
        assert_eq!(f32::from(bounds.origin.x), 0.0);
        assert_eq!(f32::from(bounds.origin.y), 0.0);
    }

    #[test]
    fn tiny_popup_clamps_to_minimums() {
        let bounds = popup_bounds(&prefs(10, 10, 0.5, 0.5), monitor(0.0, 0.0, 1920.0, 1080.0));
        assert_eq!(f32::from(bounds.size.width), DDL_POPUP_MIN_WIDTH as f32);
        assert_eq!(f32::from(bounds.size.height), DDL_POPUP_MIN_HEIGHT as f32);
    }

    #[test]
    fn out_of_range_percent_stays_on_screen() {
        let bounds = popup_bounds(
            &prefs(900, 700, 9.0, -3.0),
            monitor(0.0, 0.0, 1920.0, 1080.0),
        );
        let right = f32::from(bounds.origin.x) + f32::from(bounds.size.width);
        let bottom = f32::from(bounds.origin.y) + f32::from(bounds.size.height);
        assert!(right <= 1920.0);
        assert!(f32::from(bounds.origin.y) >= 0.0);
        assert!(bottom <= 1080.0);
    }

    #[test]
    fn select_monitor_prefers_persisted_uuid() {
        let monitors = vec![
            DdlMonitorInfo {
                id: None,
                uuid: Some("aaa".into()),
                label: "Monitor 1".into(),
                bounds: monitor(0.0, 0.0, 1920.0, 1080.0),
            },
            DdlMonitorInfo {
                id: None,
                uuid: Some("bbb".into()),
                label: "Monitor 2".into(),
                bounds: monitor(1920.0, 0.0, 2560.0, 1440.0),
            },
        ];
        let selected = select_monitor(
            &DdlPopupMonitorPreference::Monitor { id: "bbb".into() },
            &monitors,
            Some(&monitors[0]),
        )
        .unwrap();
        assert_eq!(selected.uuid.as_deref(), Some("bbb"));
    }

    #[test]
    fn select_monitor_falls_back_when_uuid_gone() {
        let monitors = vec![DdlMonitorInfo {
            id: None,
            uuid: Some("aaa".into()),
            label: "Monitor 1".into(),
            bounds: monitor(0.0, 0.0, 1920.0, 1080.0),
        }];
        let selected = select_monitor(
            &DdlPopupMonitorPreference::Monitor {
                id: "unplugged".into(),
            },
            &monitors,
            None,
        )
        .unwrap();
        assert_eq!(selected.uuid.as_deref(), Some("aaa"));
    }

    #[test]
    fn select_monitor_none_when_no_displays() {
        assert!(select_monitor(&DdlPopupMonitorPreference::CurrentWindow, &[], None).is_none());
    }
}
