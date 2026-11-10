//! Gesture preferences — port of `GesturePreferences.swift`.
//!
//! Pure, serde-serializable with fault-tolerant decode. Controls how trackpad
//! gestures interact with the result container and query swapping.

use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};

/// Modifier keys that can activate the result-container scroll-to-snap gesture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResultContainerScrollModifier {
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub command: bool,
    #[serde(default)]
    pub function: bool,
    #[serde(default)]
    pub control: bool,
    #[serde(default)]
    pub option: bool,
}

impl ResultContainerScrollModifier {
    pub fn display_title(&self) -> String {
        let parts: Vec<&str> = [
            self.shift.then_some("Shift"),
            self.command.then_some("Cmd"),
            self.function.then_some("Fn"),
            self.control.then_some("Ctrl"),
            self.option.then_some("Option"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if parts.is_empty() {
            crate::localization::text("settings.gestures.modifier.none")
        } else {
            parts.join("+")
        }
    }

    pub fn matches(
        self,
        shift: bool,
        command: bool,
        function: bool,
        control: bool,
        option: bool,
    ) -> bool {
        self.shift == shift
            && self.command == command
            && self.function == function
            && self.control == control
            && self.option == option
    }
}

/// Direction of the swipe gesture that swaps between query tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QuerySwapGestureDirection {
    #[default]
    Disabled,
    Vertical,
    Horizontal,
}

impl QuerySwapGestureDirection {
    #[allow(dead_code)]
    pub fn from_stored_value(value: Option<&str>) -> Self {
        match value.unwrap_or("").to_lowercase().as_str() {
            "vertical" => Self::Vertical,
            "horizontal" => Self::Horizontal,
            _ => Self::Disabled,
        }
    }
}

/// Complete gesture preference set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct GesturePreferences {
    #[serde(default)]
    pub result_container_scroll_modifier: ResultContainerScrollModifier,
    #[serde(default)]
    pub results_two_finger_scroll_enabled: bool,
    #[serde(default)]
    pub query_swap_direction: QuerySwapGestureDirection,
}

impl GesturePreferences {
    pub fn defaults() -> Self {
        Self {
            result_container_scroll_modifier: ResultContainerScrollModifier {
                shift: false,
                command: false,
                function: false,
                control: false,
                option: false,
            },
            results_two_finger_scroll_enabled: true,
            query_swap_direction: QuerySwapGestureDirection::Disabled,
        }
    }
}

/// Fault-tolerant decode: falls back to `defaults()` on any error.
pub fn decode_preferences(json: &str) -> GesturePreferences {
    serde_json::from_str(json).unwrap_or_else(|_| GesturePreferences::defaults())
}

pub fn encode_preferences(prefs: &GesturePreferences) -> String {
    crate::config_version::to_string_pretty(prefs).unwrap_or_else(|_| "{}".to_string())
}

/// Load from a file path (fault-tolerant → defaults).
/// Browser build: reads localStorage instead of the filesystem.
pub fn load_preferences(path: &std::path::Path) -> GesturePreferences {
    #[cfg(target_arch = "wasm32")]
    return {
        let _ = path;
        match crate::host_storage::load_text(
            crate::host_storage::StorageArea::Data,
            "gesture-preferences.json",
        ) {
            Some(content) => decode_preferences(&content),
            None => GesturePreferences::defaults(),
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    match std::fs::read_to_string(path) {
        Ok(content) => decode_preferences(&content),
        Err(_) => GesturePreferences::defaults(),
    }
}

/// Load from the default app config path (`$DATA_DIR/SQLLY/gesture-preferences.json`).
/// Falls back to `defaults()` if the file is missing or unreadable.
pub fn load_default_preferences() -> GesturePreferences {
    if let Ok(guard) = default_gesture_prefs_cache().read()
        && let Some(cached) = guard.as_ref()
    {
        return cached.clone();
    }
    let prefs = load_preferences(&default_gesture_prefs_path());
    if let Ok(mut guard) = default_gesture_prefs_cache().write() {
        *guard = Some(prefs.clone());
    }
    prefs
}

fn default_gesture_prefs_path() -> std::path::PathBuf {
    #[cfg(not(target_arch = "wasm32"))]
    let mut p = dirs::data_dir().unwrap_or_else(std::env::temp_dir);
    // Browser build: nominal path only — load_preferences uses localStorage.
    #[cfg(target_arch = "wasm32")]
    let mut p = std::path::PathBuf::new();
    p.push("SQLLY");
    p.push("gesture-preferences.json");
    p
}

static DEFAULT_GESTURE_PREFS_CACHE: OnceLock<RwLock<Option<GesturePreferences>>> = OnceLock::new();

fn default_gesture_prefs_cache() -> &'static RwLock<Option<GesturePreferences>> {
    DEFAULT_GESTURE_PREFS_CACHE.get_or_init(|| RwLock::new(None))
}

/// Save to a file path (atomic via `.tmp` rename).
/// Browser build: writes localStorage instead of the filesystem.
pub fn save_preferences(prefs: &GesturePreferences, path: &std::path::Path) -> std::io::Result<()> {
    let json = encode_preferences(prefs);
    #[cfg(target_arch = "wasm32")]
    return {
        let _ = path;
        let result = crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            "gesture-preferences.json",
            &json,
        )
        .map_err(std::io::Error::other);
        if result.is_ok() {
            if let Ok(mut guard) = default_gesture_prefs_cache().write() {
                *guard = Some(prefs.clone());
            }
        }
        result
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        if path == default_gesture_prefs_path()
            && let Ok(mut guard) = default_gesture_prefs_cache().write()
        {
            *guard = Some(prefs.clone());
        }
        Ok(())
    }
}

/// Clears the in-memory cache so the next [`load_default_preferences`] re-reads.
#[cfg(test)]
pub(crate) fn invalidate_default_cache() {
    if let Ok(mut guard) = default_gesture_prefs_cache().write() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_display_none() {
        let m = ResultContainerScrollModifier::default();
        assert_eq!(m.display_title(), "None");
    }

    #[test]
    fn modifier_display_shift_cmd() {
        let m = ResultContainerScrollModifier {
            shift: true,
            command: true,
            ..Default::default()
        };
        assert_eq!(m.display_title(), "Shift+Cmd");
    }

    #[test]
    fn modifier_matches() {
        let m = ResultContainerScrollModifier {
            shift: true,
            command: false,
            function: false,
            control: true,
            option: false,
        };
        assert!(m.matches(true, false, false, true, false));
        assert!(!m.matches(false, false, false, true, false));
    }

    #[test]
    fn swap_direction_from_stored() {
        assert_eq!(
            QuerySwapGestureDirection::from_stored_value(Some("vertical")),
            QuerySwapGestureDirection::Vertical
        );
        assert_eq!(
            QuerySwapGestureDirection::from_stored_value(Some("HORIZONTAL")),
            QuerySwapGestureDirection::Horizontal
        );
        assert_eq!(
            QuerySwapGestureDirection::from_stored_value(Some("")),
            QuerySwapGestureDirection::Disabled
        );
        assert_eq!(
            QuerySwapGestureDirection::from_stored_value(None),
            QuerySwapGestureDirection::Disabled
        );
    }

    #[test]
    fn defaults() {
        let d = GesturePreferences::defaults();
        assert!(d.results_two_finger_scroll_enabled);
        assert_eq!(d.query_swap_direction, QuerySwapGestureDirection::Disabled);
    }

    #[test]
    fn round_trip() {
        let prefs = GesturePreferences {
            result_container_scroll_modifier: ResultContainerScrollModifier {
                shift: true,
                ..Default::default()
            },
            results_two_finger_scroll_enabled: false,
            query_swap_direction: QuerySwapGestureDirection::Vertical,
        };
        let json = encode_preferences(&prefs);
        let decoded = decode_preferences(&json);
        assert_eq!(decoded, prefs);
    }

    #[test]
    fn load_default_returns_cached_prefs_without_disk_reread() {
        invalidate_default_cache();
        let prefs = GesturePreferences {
            results_two_finger_scroll_enabled: false,
            ..GesturePreferences::defaults()
        };
        if let Ok(mut guard) = default_gesture_prefs_cache().write() {
            *guard = Some(prefs);
        }
        assert!(!load_default_preferences().results_two_finger_scroll_enabled);
        invalidate_default_cache();
    }

    #[test]
    fn decode_corrupt_falls_back() {
        let prefs = decode_preferences("garbage");
        assert!(prefs.results_two_finger_scroll_enabled);
    }

    #[test]
    fn decode_missing_fields_uses_defaults() {
        let prefs = decode_preferences("{}");
        assert!(!prefs.results_two_finger_scroll_enabled);
        assert_eq!(
            prefs.query_swap_direction,
            QuerySwapGestureDirection::Disabled
        );
    }
}
