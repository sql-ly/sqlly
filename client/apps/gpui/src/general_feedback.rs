//! Send Feedback: the request payload and its background upload queue.
//!
//! The dialog (`general_feedback_dialog`) flattens the annotations into the
//! screenshot and hands the finished PNG here; nothing unflattened is ever
//! stored or sent. The request is persisted as one JSON file per submission
//! under `<data>/pending-general-feedback/` and kept there until the server
//! accepts it — `feedback_uploader::sync` retries it every five minutes with
//! no give-up. The client-generated GUID (`clientSubmissionId`) is the
//! server's dedup key, so a retry after a half-landed POST cannot
//! double-post.
//!
//! Wire contract: `POST /api/feedback/general`, signed exactly like the
//! IntelliSense feedback endpoint (`X-Sqlly-App-*` headers), body:
//! `{ clientSubmissionId, message, appVersion, platform, osVersion,
//!    screenshotPng (base64 | null), screenshotWidth, screenshotHeight,
//!    scaleFactor, annotations: [{ kind: "arrow", number, x1, y1, x2, y2 }],
//!    context: { activeConnectionEngine, activeView, settings? }, deviceId? }`,
//! answered by `{ id: "gfb_…", duplicate }`.

use serde::{Deserialize, Serialize};

/// Directory (under the app data root) holding pending submissions.
pub const PENDING_DIR_NAME: &str = "pending-general-feedback";

/// Server-enforced limits (`GeneralFeedbackService`), mirrored here so a
/// report is shaped to fit before it is queued — the server answers 400/413
/// for anything over, and those are permanent failures.
pub const MAX_MESSAGE_CHARS: usize = 8000;
/// Decoded PNG size cap.
pub const MAX_SCREENSHOT_BYTES: usize = 14 * 1024 * 1024;
/// Longest allowed image edge, in pixels.
pub const MAX_SCREENSHOT_EDGE: u32 = 16384;
pub const MAX_ANNOTATIONS: usize = 500;
/// `scaleFactor` must lie in `(0, MAX_SCALE_FACTOR]`.
pub const MAX_SCALE_FACTOR: f32 = 8.0;
/// Serialized `context` cap (`MaxContextJsonLength`).
pub const MAX_CONTEXT_JSON_BYTES: usize = 32 * 1024;
/// Budget for the settings snapshot inside `context`, leaving headroom for
/// the other context fields under [`MAX_CONTEXT_JSON_BYTES`].
pub const MAX_SETTINGS_JSON_BYTES: usize = MAX_CONTEXT_JSON_BYTES - 2 * 1024;

/// Check the message before sending: a report needs a message unless it
/// carries a screenshot, and the message must fit the server's cap.
pub fn validate_message(message: &str, has_screenshot: bool) -> Result<(), String> {
    if message.trim().is_empty() && !has_screenshot {
        return Err(crate::localization::text(
            "feedback.general.message_required",
        ));
    }
    let chars = message.chars().count();
    if chars > MAX_MESSAGE_CHARS {
        return Err(crate::localization::format(
            "feedback.general.message_too_long",
            &[
                ("limit", &MAX_MESSAGE_CHARS.to_string()),
                ("actual", &chars.to_string()),
            ],
        ));
    }
    Ok(())
}

/// One annotation as metadata (screenshot pixel coordinates). Only arrows
/// are listed; blackouts are already burned into the image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnotationMeta {
    pub kind: String,
    /// The 1-based number drawn in the arrow's tail badge, so the message
    /// can say "see #2". Absent in records queued by older builds.
    #[serde(default)]
    pub number: u32,
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

/// Where the user was when they asked to send feedback, and how the app was
/// configured.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackContext {
    pub active_connection_engine: Option<String>,
    pub active_view: Option<String>,
    /// The user's configuration settings, scrubbed of credentials and API
    /// keys (see `settings_snapshot`). Captured at send time; absent in
    /// records queued by older builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<crate::settings_snapshot::SettingsSnapshot>,
}

/// The `POST /api/feedback/general` body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneralFeedbackRequest {
    pub client_submission_id: String,
    pub message: String,
    pub app_version: String,
    pub platform: String,
    pub os_version: String,
    /// Base64 of the already-flattened PNG; `None` when the user chose not
    /// to include a screenshot or none could be captured.
    pub screenshot_png: Option<String>,
    pub screenshot_width: u32,
    pub screenshot_height: u32,
    pub scale_factor: f32,
    pub annotations: Vec<AnnotationMeta>,
    pub context: FeedbackContext,
    /// Paired device at submit time, mirroring IntelliSense feedback's
    /// device attribution. Extra to the base contract; omitted when unpaired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

/// A finished, flattened screenshot ready to attach.
pub struct FlattenedScreenshot {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
}

/// Everything the dialog produces on submit.
pub struct FeedbackDraft {
    pub message: String,
    pub screenshot: Option<FlattenedScreenshot>,
    /// Arrow segments `(x1, y1, x2, y2)` in screenshot pixels, in document
    /// order — arrow `i` is badge number `i + 1`.
    pub arrows: Vec<(f32, f32, f32, f32)>,
    pub context: FeedbackContext,
}

/// What the dialog hands back on Send, before the (CPU-heavy) flatten and
/// PNG encode, which run off the UI thread via [`SubmittedFeedback::finish`].
pub struct SubmittedFeedback {
    pub message: String,
    /// The raw capture plus its scale factor — `None` when the user unticked
    /// "Include screenshot" or no capture was possible. Never stored or sent
    /// as-is: `finish` burns the annotations in first.
    pub screenshot: Option<(std::sync::Arc<image::RgbaImage>, f32)>,
    pub annotations: Vec<crate::feedback_annotate::Annotation>,
    pub context: FeedbackContext,
}

impl SubmittedFeedback {
    /// Flatten arrows and blackouts into the capture at native resolution and
    /// encode the PNG. After this, blacked-out pixels exist nowhere in the
    /// draft.
    pub fn finish(self) -> Result<FeedbackDraft, String> {
        use crate::feedback_annotate::{self as annotate, Annotation};

        let arrows = self
            .annotations
            .iter()
            .filter_map(|a| match a {
                Annotation::Arrow { from, to } => Some((from.x, from.y, to.x, to.y)),
                Annotation::Blackout(_) => None,
            })
            .collect();
        let Some((image, scale_factor)) = self.screenshot else {
            return Ok(FeedbackDraft {
                message: self.message,
                screenshot: None,
                arrows,
                context: self.context,
            });
        };
        // Burn in at full native resolution FIRST, so any downscale below
        // only ever sees already-blacked-out pixels.
        let flat = annotate::flatten(
            &image,
            &self.annotations,
            annotate::arrow_width_for_scale(scale_factor),
        );
        let (width0, _) = flat.dimensions();
        let (png, width, height) =
            encode_within_limits(flat, MAX_SCREENSHOT_BYTES, MAX_SCREENSHOT_EDGE)?;
        let ratio = width as f32 / width0.max(1) as f32;
        Ok(FeedbackDraft {
            message: self.message,
            screenshot: Some(FlattenedScreenshot {
                png,
                width,
                height,
                scale_factor: scale_factor * ratio,
            }),
            // Annotation metadata follows the image if it was downscaled.
            arrows: arrows
                .into_iter()
                .map(|(x1, y1, x2, y2)| (x1 * ratio, y1 * ratio, x2 * ratio, y2 * ratio))
                .collect(),
            context: self.context,
        })
    }
}

/// Encode `img` as PNG, downscaling until it fits `max_bytes` and every edge
/// is at most `max_edge`. Returns the PNG and its final width and height.
pub fn encode_within_limits(
    img: image::RgbaImage,
    max_bytes: usize,
    max_edge: u32,
) -> Result<(Vec<u8>, u32, u32), String> {
    let (w0, h0) = img.dimensions();
    let mut ratio = (max_edge as f32 / w0.max(h0).max(1) as f32).min(1.0);
    for _ in 0..8 {
        let candidate = if ratio < 1.0 {
            let w = ((w0 as f32 * ratio).floor() as u32).clamp(1, max_edge);
            let h = ((h0 as f32 * ratio).floor() as u32).clamp(1, max_edge);
            image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle)
        } else {
            img.clone()
        };
        let png = crate::feedback_annotate::encode_png(&candidate)?;
        if png.len() <= max_bytes {
            return Ok((png, candidate.width(), candidate.height()));
        }
        // Shrink by the area overshoot plus a safety margin and try again.
        ratio *= ((max_bytes as f32 / png.len() as f32).sqrt() * 0.9).min(0.9);
    }
    Err(crate::localization::text(
        "feedback.general.screenshot_too_large",
    ))
}

/// Build the request for a draft, stamping a fresh submission GUID.
pub fn build_request(
    draft: FeedbackDraft,
    submission_id: String,
    device_id: Option<String>,
    os_version: String,
) -> GeneralFeedbackRequest {
    let (screenshot_png, width, height, scale_factor) = match &draft.screenshot {
        Some(shot) => (
            Some(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &shot.png,
            )),
            shot.width,
            shot.height,
            shot.scale_factor,
        ),
        None => (None, 0, 0, 1.0),
    };
    GeneralFeedbackRequest {
        client_submission_id: submission_id,
        message: draft.message,
        app_version: crate::intellisense_feedback::APP_VERSION.to_string(),
        platform: crate::store::platform_name().to_string(),
        os_version,
        screenshot_png,
        screenshot_width: width,
        screenshot_height: height,
        scale_factor: if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor.min(MAX_SCALE_FACTOR)
        } else {
            1.0
        },
        // Arrows only make sense against the image they annotate.
        annotations: if draft.screenshot.is_some() {
            draft
                .arrows
                .iter()
                .take(MAX_ANNOTATIONS)
                .enumerate()
                .map(|(i, &(x1, y1, x2, y2))| AnnotationMeta {
                    kind: "arrow".to_string(),
                    // `arrows` is in document order, the badge numbering.
                    number: i as u32 + 1,
                    x1,
                    y1,
                    x2,
                    y2,
                })
                .collect()
        } else {
            Vec::new()
        },
        context: draft.context,
        device_id,
    }
}

/// A submission not yet accepted by the server, persisted so retries survive
/// a restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingGeneralFeedback {
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Failed upload attempts so far (informational).
    pub attempts: u32,
    /// Don't try again before this time (set when the server asked us to
    /// wait). `None` means the next regular pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<chrono::DateTime<chrono::Utc>>,
    pub request: GeneralFeedbackRequest,
}

impl PendingGeneralFeedback {
    pub fn submission_id(&self) -> &str {
        &self.request.client_submission_id
    }

    /// The exact JSON body to POST (and sign).
    pub fn body_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.request).expect("feedback request serialization is infallible")
    }
}

/// Best-effort human-readable OS version ("macOS 15.4", "Windows 11 Pro
/// 24H2 (build 26100)", "Ubuntu 24.04 LTS (6.8.0-45-generic)").
#[cfg(not(target_arch = "wasm32"))]
pub fn os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        let version = std::process::Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        match version {
            Some(v) => format!("macOS {v}"),
            None => "macOS".to_string(),
        }
    }
    #[cfg(target_os = "windows")]
    {
        use winreg::RegKey;
        use winreg::enums::HKEY_LOCAL_MACHINE;
        let key = RegKey::predef(HKEY_LOCAL_MACHINE)
            .open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion")
            .ok();
        let get = |name: &str| -> Option<String> {
            key.as_ref()
                .and_then(|k| k.get_value::<String, _>(name).ok())
                .filter(|s| !s.is_empty())
        };
        let build = get("CurrentBuild");
        // Windows 11 still reports "Windows 10" in ProductName; the build
        // number disambiguates (11 starts at 22000).
        let mut product = get("ProductName").unwrap_or_else(|| "Windows".to_string());
        if build
            .as_deref()
            .and_then(|b| b.parse::<u32>().ok())
            .is_some_and(|b| b >= 22000)
        {
            product = product.replacen("Windows 10", "Windows 11", 1);
        }
        let mut out = product;
        if let Some(display) = get("DisplayVersion") {
            out.push(' ');
            out.push_str(&display);
        }
        if let Some(build) = build {
            out.push_str(&format!(" (build {build})"));
        }
        out
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let pretty = std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|text| parse_os_release_pretty_name(&text));
        let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        match (pretty, kernel) {
            (Some(p), Some(k)) => format!("{p} ({k})"),
            (Some(p), None) => p,
            (None, Some(k)) => format!("Linux {k}"),
            (None, None) => "Linux".to_string(),
        }
    }
}

/// `PRETTY_NAME` from an `/etc/os-release` body.
#[allow(dead_code)] // Linux-only at runtime; unit-tested everywhere.
pub fn parse_os_release_pretty_name(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let value = line.strip_prefix("PRETTY_NAME=")?;
        let value = value.trim().trim_matches('"').trim_matches('\'').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(not(target_arch = "wasm32"))]
mod fs_store {
    use super::{PENDING_DIR_NAME, PendingGeneralFeedback};
    use crate::feedback_uploader::queue_store::{delete_in, exists_in, load_all_in, save_in};
    use std::path::PathBuf;

    pub fn pending_dir() -> PathBuf {
        crate::preferences::preferences_path().join(PENDING_DIR_NAME)
    }

    pub fn save(record: &PendingGeneralFeedback) {
        save_in(&pending_dir(), record.submission_id(), record);
    }

    pub fn delete(submission_id: &str) {
        delete_in(&pending_dir(), submission_id);
    }

    pub fn load_all() -> Vec<PendingGeneralFeedback> {
        load_all_in(&pending_dir())
    }

    pub fn exists(submission_id: &str) -> bool {
        exists_in(&pending_dir(), submission_id)
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use fs_store::{delete, exists, load_all, save};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feedback_uploader::queue_store::{delete_in, load_all_in, save_in};

    fn draft(with_screenshot: bool) -> FeedbackDraft {
        FeedbackDraft {
            message: "The grid froze".to_string(),
            screenshot: with_screenshot.then(|| FlattenedScreenshot {
                png: vec![0x89, b'P', b'N', b'G'],
                width: 3200,
                height: 2000,
                scale_factor: 2.0,
            }),
            arrows: vec![(10.0, 20.0, 300.0, 400.0)],
            context: FeedbackContext {
                active_connection_engine: Some("PostgreSQL".to_string()),
                active_view: Some("query-editor".to_string()),
                settings: Some(crate::settings_snapshot::capture_from(|name| {
                    (name == "editor-preferences.json").then(|| {
                        r#"{ "fontSize": 13, "apiKey": "sk-live-abcdefghijklmnop" }"#.into()
                    })
                })),
            },
        }
    }

    fn pending() -> PendingGeneralFeedback {
        PendingGeneralFeedback {
            created_at: chrono::Utc::now(),
            attempts: 0,
            next_attempt_at: None,
            request: build_request(
                draft(true),
                "8d0f7a3e-1111-4222-8333-944445555666".to_string(),
                Some("dev_1".to_string()),
                "macOS 15.4".to_string(),
            ),
        }
    }

    #[test]
    fn request_body_matches_the_wire_contract() {
        let body: serde_json::Value = serde_json::from_slice(&pending().body_bytes()).unwrap();
        assert_eq!(
            body["clientSubmissionId"],
            "8d0f7a3e-1111-4222-8333-944445555666"
        );
        assert_eq!(body["message"], "The grid froze");
        assert_eq!(
            body["appVersion"],
            crate::intellisense_feedback::APP_VERSION
        );
        assert_eq!(body["platform"], crate::store::platform_name());
        assert_eq!(body["osVersion"], "macOS 15.4");
        assert_eq!(body["screenshotPng"], "iVBORw==");
        assert_eq!(body["screenshotWidth"], 3200);
        assert_eq!(body["screenshotHeight"], 2000);
        assert_eq!(body["scaleFactor"], 2.0);
        assert_eq!(
            body["annotations"],
            serde_json::json!([{ "kind": "arrow", "number": 1, "x1": 10.0, "y1": 20.0, "x2": 300.0, "y2": 400.0 }])
        );
        assert_eq!(body["context"]["activeConnectionEngine"], "PostgreSQL");
        assert_eq!(body["context"]["activeView"], "query-editor");
        let settings = &body["context"]["settings"];
        assert_eq!(
            settings["files"]["editor-preferences.json"],
            serde_json::json!({ "fontSize": 13, "apiKey": crate::settings_snapshot::REDACTED })
        );
        assert!(settings["defaults"].is_array());
        assert_eq!(body["deviceId"], "dev_1");
    }

    #[test]
    fn message_only_request_sends_a_null_screenshot_and_no_annotations() {
        let request = build_request(draft(false), "id".into(), None, "Linux".into());
        let body = serde_json::to_value(&request).unwrap();
        assert!(body["screenshotPng"].is_null());
        assert_eq!(body["screenshotWidth"], 0);
        assert_eq!(body["annotations"], serde_json::json!([]));
        assert!(body.get("deviceId").is_none(), "unpaired omits deviceId");
        assert!(body["context"].is_object());
    }

    #[test]
    fn pending_records_persist_reload_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let pending_dir = dir.path().join(PENDING_DIR_NAME);
        let mut record = pending();
        save_in(&pending_dir, record.submission_id(), &record);
        // A retry rewrites the same file (same GUID), not a second record.
        record.attempts = 3;
        save_in(&pending_dir, record.submission_id(), &record);
        // A corrupt neighbour is dropped (and deleted) on load.
        std::fs::write(pending_dir.join("junk.json"), b"{ not json").unwrap();
        std::fs::write(pending_dir.join("notes.txt"), b"ignored").unwrap();

        let loaded: Vec<PendingGeneralFeedback> = load_all_in(&pending_dir);
        assert_eq!(loaded, vec![record.clone()]);
        assert!(!pending_dir.join("junk.json").exists());
        assert!(pending_dir.join("notes.txt").exists());

        delete_in(&pending_dir, record.submission_id());
        let loaded: Vec<PendingGeneralFeedback> = load_all_in(&pending_dir);
        assert!(loaded.is_empty());
    }

    #[test]
    fn record_ids_cannot_escape_the_pending_dir() {
        let dir = tempfile::tempdir().unwrap();
        let pending_dir = dir.path().join("q");
        save_in(&pending_dir, "../../evil", &serde_json::json!({}));
        assert!(pending_dir.join("evil.json").exists());
        assert!(!dir.path().join("evil.json").exists());
    }

    #[test]
    fn finished_draft_png_has_blackouts_burned_in() {
        use crate::feedback_annotate::{Annotation, Pt, Rect};

        let base = image::RgbaImage::from_pixel(120, 80, image::Rgba([250, 200, 150, 255]));
        let submitted = SubmittedFeedback {
            message: "secret customer name visible".to_string(),
            screenshot: Some((std::sync::Arc::new(base), 2.0)),
            annotations: vec![
                Annotation::Blackout(Rect::new(5.0, 5.0, 10.0, 10.0)),
                Annotation::Arrow {
                    from: Pt::new(40.0, 60.0),
                    to: Pt::new(115.0, 60.0),
                },
            ],
            context: FeedbackContext::default(),
        };
        let draft = submitted.finish().unwrap();
        assert_eq!(draft.arrows, vec![(40.0, 60.0, 115.0, 60.0)]);
        let shot = draft.screenshot.as_ref().unwrap();
        assert_eq!((shot.width, shot.height, shot.scale_factor), (120, 80, 2.0));

        // Decode what would be uploaded: the blackout is pure black, the rest
        // of the capture is intact.
        let request = build_request(draft, "id".into(), None, "macOS".into());
        let png = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            request.screenshot_png.unwrap(),
        )
        .unwrap();
        let decoded = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (120, 80));
        for y in 5..15 {
            for x in 5..15 {
                assert_eq!(decoded.get_pixel(x, y).0, [0, 0, 0, 255], "({x},{y})");
            }
        }
        assert_eq!(decoded.get_pixel(2, 2).0, [250, 200, 150, 255]);
        assert_eq!(
            decoded.get_pixel(90, 60).0,
            crate::feedback_annotate::ARROW_RGBA
        );
        assert_eq!(request.annotations.len(), 1);
        assert_eq!(request.annotations[0].number, 1);
    }

    #[test]
    fn message_rules_mirror_the_server() {
        assert!(validate_message("", false).is_err());
        assert!(validate_message("   ", false).is_err());
        assert!(
            validate_message("", true).is_ok(),
            "a screenshot alone is a report"
        );
        assert!(validate_message("hi", false).is_ok());
        let long = "é".repeat(MAX_MESSAGE_CHARS);
        assert!(validate_message(&long, false).is_ok());
        assert!(validate_message(&(long + "x"), false).is_err());
    }

    #[test]
    fn oversized_screenshots_are_downscaled_to_fit() {
        // Noise compresses badly, so a small byte cap forces a downscale.
        let mut img = image::RgbaImage::new(300, 200);
        let mut seed = 0x1234_5678u32;
        for px in img.pixels_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            px.0 = [seed as u8, (seed >> 8) as u8, (seed >> 16) as u8, 255];
        }
        let (png, w, h) = encode_within_limits(img.clone(), 60_000, 16384).unwrap();
        assert!(png.len() <= 60_000);
        assert!(w < 300 && h < 200 && w > 0 && h > 0);
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (w, h));
        // Edge cap alone.
        let (_, w, h) = encode_within_limits(img.clone(), usize::MAX, 150).unwrap();
        assert_eq!((w, h), (150, 100));
        // Already fits: untouched.
        let (_, w, h) = encode_within_limits(img, usize::MAX, 16384).unwrap();
        assert_eq!((w, h), (300, 200));
    }

    #[test]
    fn annotations_and_scale_are_clamped_to_server_limits() {
        let mut d = draft(true);
        d.arrows = vec![(0.0, 0.0, 1.0, 1.0); MAX_ANNOTATIONS + 20];
        if let Some(shot) = d.screenshot.as_mut() {
            shot.scale_factor = 12.0;
        }
        let request = build_request(d, "id".into(), None, "macOS".into());
        assert_eq!(request.annotations.len(), MAX_ANNOTATIONS);
        assert_eq!(request.scale_factor, MAX_SCALE_FACTOR);
    }

    #[test]
    fn os_release_pretty_name_is_parsed() {
        let text = "NAME=\"Ubuntu\"\nPRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\n";
        assert_eq!(
            parse_os_release_pretty_name(text).as_deref(),
            Some("Ubuntu 24.04.1 LTS")
        );
        assert_eq!(parse_os_release_pretty_name("ID=arch\n"), None);
    }
}
