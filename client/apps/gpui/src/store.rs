//! SQLLY Store account client: browser device-pairing sign-in.
//!
//! The store (store.sqlly.app) is the customer portal the pricing page
//! sells through. Rather than collect a password, the app pairs itself like a
//! streaming-TV device so the browser can offer every sign-in option
//! (Google/Microsoft/GitHub/Apple/passkey/password):
//!
//! 1. `POST /api/devices/pairings` with the machine's platform, hostname, app
//!    version, and a stable machine fingerprint → an unclaimed device plus a
//!    short pairing `code` (`XXXX-XXXX-XXXX`) and an opaque `secret`. This is
//!    persisted (see [`pairing`]) and reused across launches.
//! 2. The user opens `/devices/register?code=…` in the browser, signs in, and
//!    the page claims the device to their account.
//! 3. The app polls `POST /api/devices/pairings/token` with its `secret` until
//!    the claim lands, receiving a JWT it stores as a [`StoreSession`]. The
//!    secret keeps working while the device is active, so it also re-issues
//!    tokens silently after the JWT expires.
//!
//! The fingerprint is SHA-256 of the OS's stable machine identifier
//! (IOPlatformUUID / MachineGuid / machine-id) — never the raw value. The
//! session and pairing both persist in the OS keychain (see [`session`],
//! [`pairing`]).
//!
//! Everything here is native-only; the browser build gets `Unsupported`.

use serde::{Deserialize, Serialize};

/// Production store base URL. Overridable with `SQLLY_STORE_URL` so dev
/// builds can point at `http://localhost:5207`.
pub const DEFAULT_BASE_URL: &str = "https://sqlly.app";

/// The base URL store requests are sent to.
pub fn base_url() -> String {
    resolve_base_url(std::env::var("SQLLY_STORE_URL").ok())
}

/// Pure override-resolution behind [`base_url`]: a non-empty `SQLLY_STORE_URL`
/// (trailing slash trimmed) wins, else the production apex. Split out so the
/// override logic is unit-testable without mutating the process-global env.
fn resolve_base_url(override_var: Option<String>) -> String {
    override_var
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
}

/// Standalone builds never contact sqlly.app: every store request fails fast
/// with [`StoreError::Standalone`] before any network I/O.
#[cfg(not(target_arch = "wasm32"))]
fn ensure_online() -> Result<(), StoreError> {
    if crate::distribution::is_standalone() {
        Err(StoreError::Standalone)
    } else {
        Ok(())
    }
}

/// Errors the store client can surface. [`StoreError::display_message`] is the
/// user-presentable, translatable description.
#[derive(Debug)]
pub enum StoreError {
    /// The browser (wasm) build has no native HTTP / keychain / machine-id.
    /// Only constructed on wasm targets — native always takes the real path.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    BrowserUnsupported,
    /// The HTTP request never completed (offline, DNS, TLS, timeout).
    Network,
    /// The server answered with a non-success status. `message` carries the
    /// server's `error` field when it sent one (e.g. bad credentials).
    Server { status: u16, message: String },
    /// A success response whose body did not match the expected schema.
    UnexpectedResponse,
    /// Reading the machine fingerprint failed (all OS sources exhausted).
    MachineIdUnavailable,
    /// A standalone build (the `standalone` feature) never contacts
    /// sqlly.app, so every store request stops here.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Standalone,
    /// This build was made without the feedback signing key, so sqlly.app
    /// would not accept its feedback.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    FeedbackUnsigned,
    /// A feedback upload hit a rate limit (any `429`): retryable, but not
    /// before `retry_after_secs` (when the limit resets). `scope` says which
    /// limit, so the user is told what happened.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    FeedbackThrottled {
        retry_after_secs: u64,
        scope: ThrottleScope,
    },
}

/// Which rate limit a feedback upload ran into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThrottleScope {
    /// This device's daily feedback quota (`scope: "device"`).
    Device,
    /// The daily quota for the network's public IP (`scope: "ip"`).
    Network,
    /// sqlly.app's overall daily feedback quota (`scope: "global"`).
    Global,
    /// The per-minute request limiter (a `429` with no feedback code).
    RequestRate,
}

impl StoreError {
    /// Keyed, thread-safe description; the UI resolves it with
    /// `localization::message`. A server-supplied `error` field is shown as is.
    pub fn display_message(&self) -> sqlly_core::DisplayMessage {
        match self {
            Self::BrowserUnsupported => {
                crate::localization::deferred_text("account.store_errors.browser_unsupported")
            }
            Self::Network => crate::localization::deferred_text("account.store_errors.network"),
            Self::Server { status, message } if message.is_empty() => {
                crate::localization::deferred(
                    "account.store_errors.http_status",
                    &[("status", status)],
                )
            }
            Self::Server { message, .. } => sqlly_core::DisplayMessage::raw(message),
            Self::UnexpectedResponse => {
                crate::localization::deferred_text("account.store_errors.unexpected_response")
            }
            Self::MachineIdUnavailable => {
                crate::localization::deferred_text("account.store_errors.machine_id_unavailable")
            }
            Self::Standalone => {
                crate::localization::deferred_text("account.store_errors.standalone_build")
            }
            Self::FeedbackUnsigned => {
                crate::localization::deferred_text("account.store_errors.feedback_unsigned")
            }
            Self::FeedbackThrottled { scope, .. } => {
                crate::localization::deferred_text(match scope {
                    ThrottleScope::Device => "account.store_errors.feedback_throttled_device",
                    ThrottleScope::Network => "account.store_errors.feedback_throttled_network",
                    ThrottleScope::Global => "account.store_errors.feedback_throttled_global",
                    ThrottleScope::RequestRate => {
                        "account.store_errors.feedback_throttled_request_rate"
                    }
                })
            }
        }
    }
}

/// English text, for logs. User-visible surfaces resolve
/// [`StoreError::display_message`] on the UI thread instead.
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_message().fallback())
    }
}

/// Upload and status code keep the keyed description of a store failure.
impl From<StoreError> for sqlly_core::DisplayMessage {
    fn from(error: StoreError) -> Self {
        error.display_message()
    }
}

impl std::error::Error for StoreError {}

/// `AccountProfileResponse` as the server serializes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountProfile {
    pub id: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub has_password: bool,
    #[serde(default)]
    pub linked_providers: Vec<String>,
}

/// `AccessTokenResponse` as the server serializes it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessTokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub account: AccountProfile,
}

/// `POST /api/devices/pairings` request body. `machine_id` is the SHA-256
/// fingerprint hex from [`machine_fingerprint`], never the raw OS identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatePairingRequest {
    pub name: String,
    pub platform: String,
    pub app_version: Option<String>,
    pub machine_id: String,
}

/// A device pairing: the server-issued `code` to show the user, the opaque
/// `secret` the app polls with, and the device id. Persisted in the keychain
/// (see [`pairing`]) and reused across launches. Matches `CreatePairingResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingRegistration {
    pub device_id: String,
    pub code: String,
    pub secret: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// Outcome of polling the pairing-token endpoint.
#[derive(Debug)]
pub enum PairingPoll {
    /// The browser hasn't claimed the device yet — keep polling.
    Pending,
    /// The device is claimed; here's the signed-in session.
    Ready(StoreSession),
}

/// The platform token the store expects (`DeviceService.ParsePlatform`).
pub fn platform_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

/// Everything the app needs to register this machine, gathered in one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineIdentity {
    /// Hostname, used as the device's display name.
    pub name: String,
    /// `macos` / `windows` / `linux`.
    pub platform: &'static str,
    /// SHA-256 of the OS machine identifier, lowercase hex.
    pub machine_id: String,
}

/// The app's machine identity for device registration.
#[cfg(not(target_arch = "wasm32"))]
pub fn machine_identity() -> Result<MachineIdentity, StoreError> {
    Ok(MachineIdentity {
        name: hostname(),
        platform: platform_name(),
        machine_id: machine_fingerprint()?,
    })
}

#[cfg(target_arch = "wasm32")]
pub fn machine_identity() -> Result<MachineIdentity, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// The machine hostname, trimmed; `"unknown-host"` when the OS will not say.
/// Read with `gethostname(3)` on Unix — the same value the `hostname` command
/// prints, and it works inside the App Store sandbox, which can't spawn it.
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is writable for its full length, and one byte is
    // held back so the result is always NUL-terminated.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len() - 1) };
    if rc != 0 {
        return "unknown-host".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    if name.is_empty() {
        "unknown-host".to_string()
    } else {
        name
    }
}

/// The machine hostname, trimmed; `"unknown-host"` when the OS will not say.
/// On Windows this is the DNS host name `hostname.exe` prints, read straight
/// from the TCP/IP parameters key: spawning `hostname.exe` costs a process
/// start (slow under real-time antivirus) on the thread that asked.
#[cfg(target_os = "windows")]
pub fn hostname() -> String {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters")
        .and_then(|key| key.get_value::<String, _>("Hostname"))
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown-host".to_string())
}

/// The machine hostname, trimmed; `"unknown-host"` when the OS will not say.
#[cfg(all(not(unix), not(windows), not(target_arch = "wasm32")))]
pub fn hostname() -> String {
    "unknown-host".to_string()
}

/// SHA-256 of the OS's stable machine identifier, as lowercase hex.
#[cfg(not(target_arch = "wasm32"))]
pub fn machine_fingerprint() -> Result<String, StoreError> {
    let raw = raw_machine_id()?;
    Ok(sha256_hex(raw.as_bytes()))
}

/// The OS-specific stable machine identifier, unhashed. Only ever leaves the
/// process as a SHA-256 digest.
#[cfg(not(target_arch = "wasm32"))]
fn raw_machine_id() -> Result<String, StoreError> {
    #[cfg(target_os = "macos")]
    {
        raw_machine_id_macos()
    }
    #[cfg(target_os = "windows")]
    {
        raw_machine_id_windows()
    }
    #[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
    {
        raw_machine_id_linux()
    }
}

/// macOS: `IOPlatformUUID` from the platform expert — stable across reboots,
/// reinstalls of the app, and user accounts.
#[cfg(target_os = "macos")]
fn raw_machine_id_macos() -> Result<String, StoreError> {
    // The App Store sandbox can't run `ioreg`; read the same value via IOKit.
    #[cfg(feature = "app-store")]
    {
        crate::app_store::platform_uuid().ok_or(StoreError::MachineIdUnavailable)
    }
    #[cfg(not(feature = "app-store"))]
    raw_machine_id_macos_ioreg()
}

#[cfg(all(target_os = "macos", not(feature = "app-store")))]
fn raw_machine_id_macos_ioreg() -> Result<String, StoreError> {
    let out = std::process::Command::new("ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|_| StoreError::MachineIdUnavailable)?;
    if !out.status.success() {
        return Err(StoreError::MachineIdUnavailable);
    }
    parse_ioreg_platform_uuid(&String::from_utf8_lossy(&out.stdout))
        .ok_or(StoreError::MachineIdUnavailable)
}

/// Windows: `MachineGuid` from the Cryptography registry key — written by
/// Windows setup, stable for the life of the OS install. Read through the
/// registry API (64-bit view, as `reg query` from a 64-bit shell sees it), so
/// the value — and the fingerprint derived from it — is the one earlier
/// versions read by running `reg.exe`, without the process start.
#[cfg(target_os = "windows")]
fn raw_machine_id_windows() -> Result<String, StoreError> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY};
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            r"SOFTWARE\Microsoft\Cryptography",
            KEY_READ | KEY_WOW64_64KEY,
        )
        .and_then(|key| key.get_value::<String, _>("MachineGuid"))
        .ok()
        .map(|guid| guid.trim().to_string())
        .filter(|guid| !guid.is_empty())
        .ok_or(StoreError::MachineIdUnavailable)
}

/// Linux: `/etc/machine-id` (systemd), falling back to the dbus copy.
#[cfg(all(target_os = "linux", not(target_arch = "wasm32")))]
fn raw_machine_id_linux() -> Result<String, StoreError> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(contents) = std::fs::read_to_string(path) {
            let id = contents.trim();
            if !id.is_empty() {
                return Ok(id.to_string());
            }
        }
    }
    Err(StoreError::MachineIdUnavailable)
}

/// Pulls the UUID out of `ioreg -rd1 -c IOPlatformExpertDevice` output.
/// Pure for testing.
#[cfg(target_os = "macos")]
#[cfg_attr(feature = "app-store", allow(dead_code))]
fn parse_ioreg_platform_uuid(output: &str) -> Option<String> {
    let start = output.find("\"IOPlatformUUID\" = \"")? + "\"IOPlatformUUID\" = \"".len();
    let end = output[start..].find('"')? + start;
    let uuid = output[start..end].trim();
    (!uuid.is_empty()).then(|| uuid.to_string())
}

/// Lowercase-hex SHA-256 of `data`. Pure for testing.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(data);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Requests a fresh device pairing (code + secret) from the store. Async so it
/// can run inside a `tokio_handle().spawn`.
#[cfg(not(target_arch = "wasm32"))]
pub async fn create_pairing(identity: &MachineIdentity) -> Result<PairingRegistration, StoreError> {
    ensure_online()?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| StoreError::Network)?;

    let resp = client
        .post(format!("{}/api/devices/pairings", base_url()))
        .json(&CreatePairingRequest {
            name: identity.name.clone(),
            platform: identity.platform.to_string(),
            app_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            machine_id: identity.machine_id.clone(),
        })
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    resp.json::<PairingRegistration>()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)
}

#[cfg(target_arch = "wasm32")]
pub async fn create_pairing(
    _identity: &MachineIdentity,
) -> Result<PairingRegistration, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// Polls the pairing-token endpoint with a stored pairing. Returns
/// [`PairingPoll::Pending`] until the browser claim lands, then
/// [`PairingPoll::Ready`] with a session whose `device_id` is filled in.
#[cfg(not(target_arch = "wasm32"))]
pub async fn poll_pairing_token(pairing: &PairingRegistration) -> Result<PairingPoll, StoreError> {
    ensure_online()?;
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct TokenRequest<'a> {
        secret: &'a str,
    }

    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| StoreError::Network)?;

    let resp = client
        .post(format!("{}/api/devices/pairings/token", base_url()))
        .json(&TokenRequest {
            secret: &pairing.secret,
        })
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    // The endpoint returns either `{ "status": "pending" }` or a full
    // `AccessTokenResponse` (which has an `accessToken`). Parse the raw JSON
    // and branch on which shape arrived.
    let value: serde_json::Value = resp
        .json()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)?;
    if value.get("accessToken").is_some() {
        let token: AccessTokenResponse =
            serde_json::from_value(value).map_err(|_| StoreError::UnexpectedResponse)?;
        let mut session = StoreSession::from_token(token);
        session.device_id = Some(pairing.device_id.clone());
        Ok(PairingPoll::Ready(session))
    } else {
        Ok(PairingPoll::Pending)
    }
}

#[cfg(target_arch = "wasm32")]
pub async fn poll_pairing_token(_pairing: &PairingRegistration) -> Result<PairingPoll, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// `POST /api/intellisense-feedback` response body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitFeedbackResponse {
    pub id: String,
    pub status: String,
}

/// Submits an "improve IntelliSense" capture to the store. The request is
/// signed with the baked-in app key (`X-Sqlly-App-*` headers). Ownership is by
/// paired device: the paired `device_id` (if the app is paired) travels in the
/// body so the server can attribute the submission.
#[cfg(not(target_arch = "wasm32"))]
pub async fn submit_intellisense_feedback(
    submission_id: &str,
    device_id: Option<&str>,
    expected_text: &str,
    context_brotli: &[u8],
    bearer: Option<String>,
) -> Result<SubmitFeedbackResponse, StoreError> {
    ensure_online()?;
    use crate::intellisense_feedback;

    let body = intellisense_feedback::build_request_body(
        submission_id,
        expected_text,
        context_brotli,
        device_id,
    );
    let timestamp = chrono::Utc::now().timestamp();
    let Some(signature) =
        intellisense_feedback::sign_request(intellisense_feedback::APP_VERSION, timestamp, &body)
    else {
        return Err(StoreError::FeedbackUnsigned);
    };

    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|_| StoreError::Network)?;

    let mut request = client
        .post(format!("{}/api/intellisense-feedback", base_url()))
        .header("Content-Type", "application/json")
        .header("X-Sqlly-App-Version", intellisense_feedback::APP_VERSION)
        .header("X-Sqlly-App-Timestamp", timestamp.to_string())
        .header("X-Sqlly-App-Signature", signature)
        .body(body);
    // Signed in → exempt from the anonymous feedback quotas.
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }

    let resp = request.send().await.map_err(|_| StoreError::Network)?;

    if !resp.status().is_success() {
        return Err(feedback_error(resp).await);
    }

    resp.json::<SubmitFeedbackResponse>()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)
}

#[cfg(target_arch = "wasm32")]
pub async fn submit_intellisense_feedback(
    _submission_id: &str,
    _device_id: Option<&str>,
    _expected_text: &str,
    _context_brotli: &[u8],
    _bearer: Option<String>,
) -> Result<SubmitFeedbackResponse, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// The store's answer to a Send Feedback submission.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GeneralFeedbackResponse {
    /// `gfb_<guid>` — the stored submission's id.
    pub id: String,
    /// True when the server already had this `clientSubmissionId` (an earlier
    /// attempt landed) — still a success for the queue.
    #[serde(default)]
    pub duplicate: bool,
}

/// Submits a general "Send Feedback" report (`POST /api/feedback/general`).
/// `body` is the finished JSON (`general_feedback::GeneralFeedbackRequest`);
/// it is signed with the baked-in app key exactly like IntelliSense
/// feedback (`X-Sqlly-App-*` headers). When the user is signed in, their
/// `bearer` token rides along so the server links the report to the account.
#[cfg(not(target_arch = "wasm32"))]
pub async fn submit_general_feedback(
    body: Vec<u8>,
    bearer: Option<String>,
) -> Result<GeneralFeedbackResponse, StoreError> {
    ensure_online()?;
    use crate::intellisense_feedback;

    let timestamp = chrono::Utc::now().timestamp();
    let Some(signature) =
        intellisense_feedback::sign_request(intellisense_feedback::APP_VERSION, timestamp, &body)
    else {
        return Err(StoreError::FeedbackUnsigned);
    };

    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        // Screenshots make this body several MB; allow slow uplinks.
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|_| StoreError::Network)?;

    let mut request = client
        .post(format!("{}/api/feedback/general", base_url()))
        .header("Content-Type", "application/json")
        .header("X-Sqlly-App-Version", intellisense_feedback::APP_VERSION)
        .header("X-Sqlly-App-Timestamp", timestamp.to_string())
        .header("X-Sqlly-App-Signature", signature);
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let resp = request
        .body(body)
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    if !resp.status().is_success() {
        return Err(feedback_error(resp).await);
    }

    resp.json::<GeneralFeedbackResponse>()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)
}

/// A billing subscription for the signed-in account, as returned by
/// `GET /api/billing/subscription` (`SubscriptionResponse` on the server). The
/// endpoint answers `404` when the account has no subscription (free tier),
/// which the client surfaces as `Ok(None)` from [`fetch_subscription`].
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub id: String,
    pub plan_id: String,
    pub plan_name: String,
    /// `"Active"`, `"Cancelled"`, or `"PastDue"` — the server's
    /// `SubscriptionStatus` enum, string-encoded (`JsonStringEnumConverter`).
    pub status: String,
    pub current_period_start: chrono::DateTime<chrono::Utc>,
    /// End of the current paid period. A far-future value (year 9999) means
    /// "no expiry" — used for complimentary/never-renewing grants.
    pub current_period_end: chrono::DateTime<chrono::Utc>,
    /// Whether the plan currently grants paid access.
    pub has_access: bool,
    /// Whether it will auto-renew at period end (false for comped or cancelled).
    pub will_renew: bool,
    /// Amount charged at the next renewal, if any.
    pub next_renewal_amount: Option<f64>,
    /// An admin-granted, no-charge subscription.
    #[serde(default)]
    pub is_comped: bool,
    /// Platforms this plan licenses (`"macos"`, …); `None` means every
    /// platform. The Mac App Store lifetime license is `["macos"]`.
    #[serde(default)]
    pub licensed_platforms: Option<Vec<String>>,
    #[serde(default)]
    pub assigned_by_email: Option<String>,
    #[serde(default)]
    pub comp_reason: Option<String>,
}

/// Fetches the signed-in account's subscription for this platform. Returns
/// `Ok(None)` when the account has no subscription that licenses this
/// platform (the store answers `404` — free tier), and the full
/// [`Subscription`] otherwise. Authenticates with the bearer `access_token`
/// from the stored [`StoreSession`].
///
/// The platform matters: a Mac App Store purchase licenses macOS only, so a
/// Windows or Linux install of the same account must not count it.
#[cfg(not(target_arch = "wasm32"))]
pub async fn fetch_subscription(access_token: &str) -> Result<Option<Subscription>, StoreError> {
    ensure_online()?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| StoreError::Network)?;

    let resp = client
        .get(subscription_url(&base_url(), platform_name()))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    // A missing subscription is a normal state, not an error.
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    resp.json::<Subscription>()
        .await
        .map(Some)
        .map_err(|_| StoreError::UnexpectedResponse)
}

#[cfg(target_arch = "wasm32")]
pub async fn fetch_subscription(_access_token: &str) -> Result<Option<Subscription>, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// `GET` URL for this platform's view of the account's subscription. Pure.
fn subscription_url(base: &str, platform: &str) -> String {
    format!("{base}/api/billing/subscription?platform={platform}")
}

/// Records a Mac App Store purchase on the signed-in account
/// (`POST /api/billing/apple/transactions`). `signed_transaction` is
/// StoreKit's Apple-signed JWS; the server verifies it and answers with the
/// account's resulting subscription. `409` means the purchase is linked to
/// another account; `410` that Apple revoked or refunded it.
#[cfg(not(target_arch = "wasm32"))]
pub async fn record_app_store_transaction(
    access_token: &str,
    signed_transaction: &str,
) -> Result<Subscription, StoreError> {
    ensure_online()?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| StoreError::Network)?;
    let resp = client
        .post(format!("{}/api/billing/apple/transactions", base_url()))
        .bearer_auth(access_token)
        .json(&serde_json::json!({ "signedTransaction": signed_transaction }))
        .send()
        .await
        .map_err(|_| StoreError::Network)?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }
    resp.json::<Subscription>()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)
}

// ---------------------------------------------------------------------------
// Relay control plane (remote-engine transport, "Direct or Via Relay")
//
// These talk to the relay endpoints on the same store host: the public relay
// directory (`GET /remote-endpoint-list`, no account) plus the account-scoped
// connector/pair management under `/api/relay/*` (bearer-authenticated with the
// signed-in [`StoreSession::access_token`]). Together they yield everything a
// `ViaRelay` profile needs to open a `sqlly_remote::transport::PeerSession` initiator:
// which relay hostname to dial, the connector's pinned Noise public key, and the
// authorizing route pair. NOTHING here is secret — public keys and ids only, so
// results are safe to cache in secret-free connection profiles.
// ---------------------------------------------------------------------------

/// A public relay-forwarding endpoint from `GET /remote-endpoint-list`
/// (`RelayEndpointInfo`). The server emits snake_case field names explicitly
/// (except `relayId`), so this struct deliberately does NOT use
/// `rename_all = "camelCase"`. Carries no credential, id, or capacity secret —
/// only what a client needs to choose an endpoint. Bridges/clients dial only
/// `status == "Online"` endpoints that are not `"Full"`.
///
/// Field names/optionality mirror the bridge's decoder
/// (the bridge's control-plane client) — the two read one contract.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RelayEndpoint {
    /// The relay server's wire identity — the frame `destination` the app
    /// authenticates to. `None` when the control plane has not yet learned it
    /// from the relay's heartbeat; such endpoints are unusable for auth and
    /// are skipped by endpoint selection.
    #[serde(rename = "relayId", default)]
    pub relay_id: Option<uuid::Uuid>,
    pub hostname: String,
    pub location: String,
    pub status: String,
    pub capacity_pct: i32,
}

/// An account relay connector from `GET /api/relay/devices` (`RelayConnectorSummary`).
/// The pinned Noise *public* key is the only key material — never anything
/// secret. `device_id` doubles as the peer's relay routing id and the
/// `connector_device_id` of a route pair.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayConnector {
    pub device_id: String,
    /// Optional display-name override. Null since v0.30 when the owner never
    /// set one — fall back to `hostname` via [`Self::display_name`].
    #[serde(default)]
    pub name: Option<String>,
    pub platform: String,
    /// Canonical base64 of the connector's 32-byte X25519 static public key.
    pub noise_public_key: String,
    /// Colon-separated hex of the SHA-256 key fingerprint (human-comparable).
    pub noise_public_key_fingerprint: String,
    /// The machine the bridge runs on — its default display name (v0.30+).
    #[serde(default)]
    pub hostname: Option<String>,
    pub registered_at: chrono::DateTime<chrono::Utc>,
    pub last_seen_at: chrono::DateTime<chrono::Utc>,
}

impl RelayConnector {
    /// The label shown for this bridge: the name override, else the hostname,
    /// else the device id — the same fallback the account site uses.
    pub fn display_name(&self) -> String {
        self.name
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .or(self.hostname.as_deref().filter(|h| !h.trim().is_empty()))
            .unwrap_or(&self.device_id)
            .to_string()
    }
    /// The pinned Noise X25519 static public key, base64-decoded to its 32 raw
    /// bytes — exactly the `expected_peer` a `sqlly_remote::transport::PeerSession::establish`
    /// initiator pins the handshake against. `UnexpectedResponse` if the server
    /// ever hands back a value that is not 32 bytes of base64.
    ///
    /// Production establishes via the pair's `connector_noise_public_key_bytes`;
    /// this connector-level accessor is kept (tests pin its validation) for
    /// callers that start from the connector list instead of a pair.
    #[allow(dead_code)]
    pub fn noise_public_key_bytes(&self) -> Result<[u8; 32], StoreError> {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(self.noise_public_key.as_bytes())
            .map_err(|_| StoreError::UnexpectedResponse)?;
        <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| StoreError::UnexpectedResponse)
    }
}

/// An account route pair from `GET /api/relay/pairs` (`RelayPairSummary`): the
/// user-authorized link between an app/control device and a relay connector that
/// a `ViaRelay` profile persists as `pair_id` + `relay_device_id`.
///
/// `control_noise_public_key` / `connector_noise_public_key` are each side's
/// pinned Noise *public* key (base64), `None` until that device has registered
/// one. The app pins the connector before the Noise handshake; the two keys are
/// the only key material here and are never secret.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayPair {
    pub id: String,
    pub control_device_id: String,
    pub connector_device_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub active: bool,
    /// The control (app) device's pinned Noise public key, base64. `None` until
    /// the app has registered one via [`register_app_noise_key`].
    #[serde(default)]
    pub control_noise_public_key: Option<String>,
    /// The connector (bridge) device's pinned Noise public key, base64. `None`
    /// until the bridge has registered one. This is the key the app pins.
    #[serde(default)]
    pub connector_noise_public_key: Option<String>,
}

impl RelayPair {
    /// The connector's pinned Noise X25519 static public key, base64-decoded to
    /// its 32 raw bytes — the `expected_peer` a `PeerSession::establish`
    /// initiator pins the handshake against. `None` when the connector has not
    /// registered a key yet; `UnexpectedResponse` if the stored value is not
    /// 32 bytes of base64.
    pub fn connector_noise_public_key_bytes(&self) -> Result<Option<[u8; 32]>, StoreError> {
        let Some(encoded) = self.connector_noise_public_key.as_deref() else {
            return Ok(None);
        };
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .map_err(|_| StoreError::UnexpectedResponse)?;
        <[u8; 32]>::try_from(bytes.as_slice())
            .map(Some)
            .map_err(|_| StoreError::UnexpectedResponse)
    }
}

/// Response from `POST /api/relay/token` (`RelayTokenResponse`): a short-lived
/// sqlly-relay-audience bearer that the caller's OWN device presents as the
/// relay Auth-frame token. The [`access_token`](Self::access_token) is
/// **secret** — never logged or persisted (its `Debug` is redacted).
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayTokenResponse {
    /// The sqlly-relay bearer token. **Secret.**
    pub access_token: String,
    pub token_type: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub token_id: String,
    pub account_id: String,
    pub device_id: String,
    pub device_kind: String,
}

impl std::fmt::Debug for RelayTokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayTokenResponse")
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("expires_at", &self.expires_at)
            .field("token_id", &self.token_id)
            .field("account_id", &self.account_id)
            .field("device_id", &self.device_id)
            .field("device_kind", &self.device_kind)
            .finish()
    }
}

/// `POST /api/relay/token` request body (`IssueRelayTokenRequest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueRelayTokenRequest {
    pub device_id: String,
}

/// `PUT /api/relay/devices/self/noise-key` request body (`SetAppNoiseKeyRequest`).
/// Carries only the app device's *public* Noise key (base64) — zero-knowledge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAppNoiseKeyRequest {
    pub device_id: String,
    pub noise_public_key: String,
}

/// `POST /api/relay/pairs` request body (`CreateRelayPairRequest`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRelayPairRequest {
    pub control_device_id: String,
    pub connector_device_id: String,
}

/// One reqwest client for the relay calls, matching the 30s budget the pairing
/// calls use.
#[cfg(not(target_arch = "wasm32"))]
fn relay_http_client() -> Result<reqwest::Client, StoreError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| StoreError::Network)
}

/// Fetches the public relay endpoint directory. No account needed — the endpoint
/// lives at the host root without the `/api` prefix.
#[cfg(not(target_arch = "wasm32"))]
pub async fn list_relay_endpoints() -> Result<Vec<RelayEndpoint>, StoreError> {
    ensure_online()?;
    let client = relay_http_client()?;
    let resp = client
        .get(format!("{}/remote-endpoint-list", base_url()))
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    resp.json::<Vec<RelayEndpoint>>()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)
}

#[cfg(target_arch = "wasm32")]
pub async fn list_relay_endpoints() -> Result<Vec<RelayEndpoint>, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// The CPU architecture token the download route expects, matching the server's
/// `ReleaseArch.Normalize` (`arm64` / `amd64`). Unknown arches pass through.
pub fn arch_name() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" | "arm64" => "arm64",
        "x86_64" | "x86" => "amd64",
        other => other,
    }
}

/// The store URL that redirects to the newest desktop-app build for THIS
/// machine's OS + architecture (`GET /download/latest?os=…&arch=…`). The app's
/// update notice opens this so "Download" lands on the right asset.
pub fn download_latest_url() -> String {
    format!(
        "{}/download/latest?os={}&arch={}",
        base_url(),
        platform_name(),
        arch_name()
    )
}

/// The latest downloadable build for this machine's platform
/// (`GET /download/latest/version?os=…&arch=…`). A tiny payload — the cheap,
/// authoritative answer to "is a newer build available for me?" without pulling
/// any changelog prose. `version` drives the update check; `file_name` and
/// `url` (the download-counted `/downloads/{version}/{file}` path) let the
/// in-app installer fetch that exact artifact, and `sha256` (empty for
/// releases published before checksums were recorded) is what it must hash to.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LatestVersion {
    pub version: String,
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub url: String,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub sha256: String,
}

/// Reads a JSON `null` (a release without a recorded checksum) as "".
fn null_as_empty<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    use serde::Deserialize as _;
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// The changes a build is missing (`GET /api/changelog/since/{version}`): every
/// release strictly newer than the one the client reported (newest first, with
/// their changes). Server-filtered, so the client never downloads the whole
/// changelog just to show the diff. The response's `current`/`latest`/`upToDate`
/// fields are ignored — the announced version comes from this platform's
/// download route, since the changelog's newest release may not have shipped
/// for this OS + architecture yet.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangelogSince {
    #[serde(default)]
    pub versions: Vec<ChangelogFeedVersion>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangelogFeedVersion {
    pub version: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub sections: Vec<ChangelogFeedSection>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangelogFeedSection {
    pub title: String,
    #[serde(default)]
    pub details: Vec<ChangelogFeedDetail>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangelogFeedDetail {
    pub text: String,
    pub is_bullet: bool,
}

/// Resolves the latest downloadable build for THIS machine's OS + architecture
/// (`GET /download/latest/version`). `Ok(None)` when the platform is supported
/// but no build is published for it yet (404) — nothing to update to, not an
/// error. Unauthenticated.
#[cfg(not(target_arch = "wasm32"))]
pub async fn fetch_latest_version() -> Result<Option<LatestVersion>, StoreError> {
    ensure_online()?;
    let client = relay_http_client()?;
    let resp = client
        .get(format!(
            "{}/download/latest/version?os={}&arch={}",
            base_url(),
            platform_name(),
            arch_name()
        ))
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    // No published build for this platform yet: a definitive "nothing newer",
    // distinct from a transport/server failure.
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    resp.json::<LatestVersion>()
        .await
        .map(Some)
        .map_err(|_| StoreError::UnexpectedResponse)
}

#[cfg(target_arch = "wasm32")]
pub async fn fetch_latest_version() -> Result<Option<LatestVersion>, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// Fetches only the releases newer than `version`, with their changes
/// (`GET /api/changelog/since/{version}`). The efficient source for the
/// "what's new" dialog — the server returns just the diff, not the full history.
/// Unauthenticated.
#[cfg(not(target_arch = "wasm32"))]
pub async fn fetch_changelog_since(version: &str) -> Result<ChangelogSince, StoreError> {
    ensure_online()?;
    let client = relay_http_client()?;
    let resp = client
        .get(format!("{}/api/changelog/since/{}", base_url(), version))
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    resp.json::<ChangelogSince>()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)
}

#[cfg(target_arch = "wasm32")]
pub async fn fetch_changelog_since(_version: &str) -> Result<ChangelogSince, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// Lists this account's relay connectors (`GET /api/relay/devices`), bearer-
/// authenticated with the signed-in session's access token. Each entry carries
/// the connector's pinned Noise public key needed to pin a `PeerSession`.
#[cfg(not(target_arch = "wasm32"))]
pub async fn list_relay_devices(access_token: &str) -> Result<Vec<RelayConnector>, StoreError> {
    ensure_online()?;
    let client = relay_http_client()?;
    let resp = client
        .get(format!("{}/api/relay/devices", base_url()))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    resp.json::<Vec<RelayConnector>>()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)
}

#[cfg(target_arch = "wasm32")]
pub async fn list_relay_devices(_access_token: &str) -> Result<Vec<RelayConnector>, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// A bridge SHARED with this account (`GET /api/relay/devices/shared`,
/// `SharedConnectorSummary`): the deliberately-minimal member projection —
/// display label, key fingerprint, device id. The owner's host inventory is
/// never present. Pairing needs only the device id; the Noise key the session
/// pins comes from the created pair.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedRelayConnector {
    pub device_id: String,
    pub name: String,
    #[serde(default)]
    pub noise_public_key_fingerprint: String,
}

/// Lists bridges other accounts shared with this one
/// (`GET /api/relay/devices/shared`), bearer-authenticated. Members pair their
/// own devices with these exactly like with owned connectors.
#[cfg(not(target_arch = "wasm32"))]
pub async fn list_shared_relay_devices(
    access_token: &str,
) -> Result<Vec<SharedRelayConnector>, StoreError> {
    ensure_online()?;
    let client = relay_http_client()?;
    let resp = client
        .get(format!("{}/api/relay/devices/shared", base_url()))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    resp.json::<Vec<SharedRelayConnector>>()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)
}

#[cfg(target_arch = "wasm32")]
pub async fn list_shared_relay_devices(
    _access_token: &str,
) -> Result<Vec<SharedRelayConnector>, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// Lists this account's route pairs (`GET /api/relay/pairs`), bearer-authenticated.
#[cfg(not(target_arch = "wasm32"))]
pub async fn list_relay_pairs(access_token: &str) -> Result<Vec<RelayPair>, StoreError> {
    ensure_online()?;
    let client = relay_http_client()?;
    let resp = client
        .get(format!("{}/api/relay/pairs", base_url()))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    resp.json::<Vec<RelayPair>>()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)
}

#[cfg(target_arch = "wasm32")]
pub async fn list_relay_pairs(_access_token: &str) -> Result<Vec<RelayPair>, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// Authorizes a new route pair between a control device and a relay connector
/// (`POST /api/relay/pairs`), returning the created pair. Bearer-authenticated.
#[cfg(not(target_arch = "wasm32"))]
pub async fn create_relay_pair(
    access_token: &str,
    control_device_id: &str,
    connector_device_id: &str,
) -> Result<RelayPair, StoreError> {
    ensure_online()?;
    let client = relay_http_client()?;
    let resp = client
        .post(format!("{}/api/relay/pairs", base_url()))
        .bearer_auth(access_token)
        .json(&CreateRelayPairRequest {
            control_device_id: control_device_id.to_string(),
            connector_device_id: connector_device_id.to_string(),
        })
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    resp.json::<RelayPair>()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)
}

#[cfg(target_arch = "wasm32")]
pub async fn create_relay_pair(
    _access_token: &str,
    _control_device_id: &str,
    _connector_device_id: &str,
) -> Result<RelayPair, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// Revokes a route pair (`DELETE /api/relay/pairs/{id}`), bearer-authenticated.
/// The server answers `204 No Content` on success.
#[cfg(not(target_arch = "wasm32"))]
pub async fn delete_relay_pair(access_token: &str, pair_id: &str) -> Result<(), StoreError> {
    ensure_online()?;
    let client = relay_http_client()?;
    let resp = client
        .delete(format!("{}/api/relay/pairs/{}", base_url(), pair_id))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    Ok(())
}

#[cfg(target_arch = "wasm32")]
pub async fn delete_relay_pair(_access_token: &str, _pair_id: &str) -> Result<(), StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// Issues a short-lived sqlly-relay bearer token for one of the account's OWN
/// devices (`POST /api/relay/token`), bearer-authenticated with the signed-in
/// session's access token. The controlling app is the session initiator and
/// presents this token as the relay Auth-frame credential. The returned
/// `access_token` is **secret** — never logged or persisted.
#[cfg(not(target_arch = "wasm32"))]
pub async fn get_relay_token(
    access_token: &str,
    device_id: &str,
) -> Result<RelayTokenResponse, StoreError> {
    ensure_online()?;
    let client = relay_http_client()?;
    let resp = client
        .post(format!("{}/api/relay/token", base_url()))
        .bearer_auth(access_token)
        .json(&IssueRelayTokenRequest {
            device_id: device_id.to_string(),
        })
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    resp.json::<RelayTokenResponse>()
        .await
        .map_err(|_| StoreError::UnexpectedResponse)
}

#[cfg(target_arch = "wasm32")]
pub async fn get_relay_token(
    _access_token: &str,
    _device_id: &str,
) -> Result<RelayTokenResponse, StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// Registers/rotates the controlling app device's pinned Noise *public* key so
/// the on-prem bridge can pin it (`PUT /api/relay/devices/self/noise-key`),
/// bearer-authenticated. `noise_public_key` is the canonical base64 of the
/// app's 32-byte X25519 static public key — zero-knowledge, never a private key.
#[cfg(not(target_arch = "wasm32"))]
pub async fn register_app_noise_key(
    access_token: &str,
    device_id: &str,
    noise_public_key: &str,
) -> Result<(), StoreError> {
    ensure_online()?;
    let client = relay_http_client()?;
    let resp = client
        .put(format!("{}/api/relay/devices/self/noise-key", base_url()))
        .bearer_auth(access_token)
        .json(&SetAppNoiseKeyRequest {
            device_id: device_id.to_string(),
            noise_public_key: noise_public_key.to_string(),
        })
        .send()
        .await
        .map_err(|_| StoreError::Network)?;

    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        let message = server_error_message(resp).await;
        return Err(StoreError::Server { status, message });
    }

    Ok(())
}

#[cfg(target_arch = "wasm32")]
pub async fn register_app_noise_key(
    _access_token: &str,
    _device_id: &str,
    _noise_public_key: &str,
) -> Result<(), StoreError> {
    Err(StoreError::BrowserUnsupported)
}

/// Reads the `{ "error": "..." }` body the server sends on failures.
#[cfg(not(target_arch = "wasm32"))]
/// A failed feedback upload: every `429` becomes
/// [`StoreError::FeedbackThrottled`] (with the server's Retry-After and which
/// limit it was), every other status a plain [`StoreError::Server`].
#[cfg(not(target_arch = "wasm32"))]
async fn feedback_error(resp: reqwest::Response) -> StoreError {
    let status = resp.status().as_u16();
    let header_retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let body = resp.bytes().await.unwrap_or_default();
    classify_feedback_error(status, header_retry_after, &body)
}

/// Pure part of [`feedback_error`].
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn classify_feedback_error(
    status: u16,
    header_retry_after: Option<u64>,
    body: &[u8],
) -> StoreError {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    struct FeedbackErrorBody {
        error: Option<String>,
        code: Option<String>,
        scope: Option<String>,
        retry_after_seconds: Option<u64>,
    }
    let parsed: FeedbackErrorBody = serde_json::from_slice(body).unwrap_or_default();
    if status == 429 {
        let scope = if parsed.code.as_deref() == Some("feedback_quota_exceeded") {
            match parsed.scope.as_deref() {
                Some("device") => ThrottleScope::Device,
                Some("ip") => ThrottleScope::Network,
                _ => ThrottleScope::Global,
            }
        } else {
            ThrottleScope::RequestRate
        };
        return StoreError::FeedbackThrottled {
            retry_after_secs: parsed
                .retry_after_seconds
                .or(header_retry_after)
                .unwrap_or(60),
            scope,
        };
    }
    StoreError::Server {
        status,
        message: parsed.error.unwrap_or_default(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn server_error_message(resp: reqwest::Response) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: Option<String>,
    }
    resp.json::<ErrorBody>()
        .await
        .ok()
        .and_then(|body| body.error)
        .unwrap_or_default()
}

/// The signed-in store session, persisted in the OS keychain so the app stays
/// registered across launches without the password ever touching disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreSession {
    pub access_token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    pub account_id: String,
    pub email: Option<String>,
    /// Set once [`register_device`] has succeeded for this machine.
    pub device_id: Option<String>,
}

impl StoreSession {
    pub fn from_token(token: AccessTokenResponse) -> Self {
        Self {
            access_token: token.access_token,
            expires_at: token.expires_at,
            account_id: token.account.id,
            email: token.account.email,
            device_id: None,
        }
    }

    /// Whether the token's expiry has passed. No clock skew needed — a
    /// 30-day token re-checked at launch is not a race anyone loses.
    pub fn is_expired(&self) -> bool {
        chrono::Utc::now() >= self.expires_at
    }
}

/// Keychain persistence for the [`StoreSession`]. One item for the whole app:
/// service `app.sqlly.store`, account `session`.
#[cfg(not(target_arch = "wasm32"))]
pub mod session {
    use super::StoreSession;

    const SERVICE: &str = crate::distribution::keychain_service!("app.sqlly.store");
    const ACCOUNT: &str = "session";

    fn entry() -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new(SERVICE, ACCOUNT)
    }

    /// Persists the session as JSON in the OS keychain.
    pub fn save(session: &StoreSession) -> Result<(), String> {
        let json = serde_json::to_string(session).map_err(|e| e.to_string())?;
        entry()
            .and_then(|e| e.set_password(&json))
            .map_err(|e| e.to_string())
    }

    /// Loads the persisted session, if any. A corrupt blob counts as none —
    /// the user signs in again rather than the app crashing on launch.
    pub fn load() -> Option<StoreSession> {
        let json = entry().ok()?.get_password().ok()?;
        serde_json::from_str(&json).ok()
    }

    /// Drops the persisted session (sign out).
    pub fn clear() {
        if let Ok(entry) = entry() {
            let _ = entry.delete_credential();
        }
    }
}

/// Browser build: no OS keychain. The session lives only for the page's
/// lifetime (in the panel struct itself), which matches how the browser build
/// treats every other credential.
#[cfg(target_arch = "wasm32")]
pub mod session {
    use super::StoreSession;

    pub fn save(_session: &StoreSession) -> Result<(), String> {
        Ok(())
    }

    pub fn load() -> Option<StoreSession> {
        None
    }

    pub fn clear() {}
}

/// Keychain persistence for the [`PairingRegistration`]. A second item beside
/// the session: service `app.sqlly.store`, account `pairing`. Reused across
/// launches so the app keeps the same device id / polling secret.
#[cfg(not(target_arch = "wasm32"))]
pub mod pairing {
    use super::PairingRegistration;

    const SERVICE: &str = crate::distribution::keychain_service!("app.sqlly.store");
    const ACCOUNT: &str = "pairing";

    fn entry() -> Result<keyring::Entry, keyring::Error> {
        keyring::Entry::new(SERVICE, ACCOUNT)
    }

    pub fn save(pairing: &PairingRegistration) -> Result<(), String> {
        let json = serde_json::to_string(pairing).map_err(|e| e.to_string())?;
        entry()
            .and_then(|e| e.set_password(&json))
            .map_err(|e| e.to_string())
    }

    pub fn load() -> Option<PairingRegistration> {
        let json = entry().ok()?.get_password().ok()?;
        serde_json::from_str(&json).ok()
    }

    pub fn clear() {
        if let Ok(entry) = entry() {
            let _ = entry.delete_credential();
        }
    }
}

/// Browser build: the pairing lives only in the panel struct.
#[cfg(target_arch = "wasm32")]
pub mod pairing {
    use super::PairingRegistration;

    pub fn save(_pairing: &PairingRegistration) -> Result<(), String> {
        Ok(())
    }

    pub fn load() -> Option<PairingRegistration> {
        None
    }

    pub fn clear() {}
}

#[cfg(test)]
impl AccessTokenResponse {
    /// Test-only expiry check on the token response itself (the real expiry
    /// check lives on `StoreSession`).
    fn is_expired_for_test(&self) -> bool {
        chrono::Utc::now() >= self.expires_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feedback_quota_rejections_carry_the_wait() {
        let body = br#"{"code":"feedback_quota_exceeded","scope":"global","limit":2000,"retryAfterSeconds":900,"error":"x"}"#;
        assert!(matches!(
            classify_feedback_error(429, Some(30), body),
            StoreError::FeedbackThrottled {
                retry_after_secs: 900,
                scope: ThrottleScope::Global
            }
        ));
        let device = br#"{"code":"feedback_quota_exceeded","scope":"device"}"#;
        assert!(matches!(
            classify_feedback_error(429, Some(120), device),
            StoreError::FeedbackThrottled {
                retry_after_secs: 120,
                scope: ThrottleScope::Device
            }
        ));
        let ip = br#"{"code":"feedback_quota_exceeded","scope":"ip","retryAfterSeconds":4000}"#;
        assert!(matches!(
            classify_feedback_error(429, None, ip),
            StoreError::FeedbackThrottled {
                retry_after_secs: 4000,
                scope: ThrottleScope::Network
            }
        ));
        // The per-minute request limiter's 429 has no code: still a rate
        // limit, named as such.
        assert!(matches!(
            classify_feedback_error(429, Some(45), b""),
            StoreError::FeedbackThrottled {
                retry_after_secs: 45,
                scope: ThrottleScope::RequestRate
            }
        ));
        // Each limit has its own message.
        let keys: std::collections::HashSet<String> = [
            ThrottleScope::Device,
            ThrottleScope::Network,
            ThrottleScope::Global,
            ThrottleScope::RequestRate,
        ]
        .into_iter()
        .map(|scope| {
            StoreError::FeedbackThrottled {
                retry_after_secs: 60,
                scope,
            }
            .display_message()
            .fallback()
            .to_string()
        })
        .collect();
        assert_eq!(keys.len(), 4);
        let too_large =
            br#"{"code":"feedback_too_large","error":"The feedback is too large to accept."}"#;
        match classify_feedback_error(413, None, too_large) {
            StoreError::Server { status, message } => {
                assert_eq!(status, 413);
                assert_eq!(message, "The feedback is too large to accept.");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn standalone_builds_refuse_every_store_request() {
        let result = ensure_online();
        if crate::distribution::is_standalone() {
            assert!(matches!(result, Err(StoreError::Standalone)));
        } else {
            assert!(result.is_ok());
        }
    }

    /// Every native request to sqlly.app must start with `ensure_online()?`,
    /// so a standalone build can never reach the network. This fails when a
    /// new request function forgets the guard.
    #[test]
    fn every_native_store_request_is_guarded() {
        let source = include_str!("store.rs");
        let lines: Vec<&str> = source.lines().collect();
        let mut checked = 0;
        for (i, line) in lines.iter().enumerate() {
            if !line.starts_with("pub async fn ") {
                continue;
            }
            let native = lines[..i]
                .iter()
                .rev()
                .take_while(|l| l.starts_with("#[") || l.starts_with("///"))
                .any(|l| l.contains("not(target_arch = \"wasm32\")"));
            if !native {
                continue;
            }
            let body_start = lines[i..]
                .iter()
                .position(|l| l.trim_end().ends_with('{'))
                .map(|p| i + p + 1)
                .expect("function body");
            assert_eq!(
                lines[body_start].trim(),
                "ensure_online()?;",
                "{line} must start with ensure_online()?"
            );
            checked += 1;
        }
        assert!(checked >= 16, "only {checked} guarded requests found");
    }

    #[test]
    fn base_url_defaults_to_the_prod_store() {
        // Env overrides are process-global, so only assert the default shape
        // (a dev machine with SQLLY_STORE_URL set still gets a valid URL).
        let url = base_url();
        assert!(url.starts_with("http"));
        assert!(!url.ends_with('/'));
    }

    #[test]
    fn default_base_url_is_the_apex_domain() {
        // The app talks to the apex (sqlly.app), not the old store. subdomain.
        assert_eq!(DEFAULT_BASE_URL, "https://sqlly.app");
        assert!(!DEFAULT_BASE_URL.contains("store.sqlly.app"));
    }

    #[test]
    fn resolve_base_url_honors_the_override_and_trims_the_slash() {
        // A non-empty SQLLY_STORE_URL wins over the prod apex, trailing slash
        // trimmed. Tested on the pure resolver so no process-global env is
        // mutated (which would race parallel tests).
        assert_eq!(
            resolve_base_url(Some("http://localhost:5207/".to_string())),
            "http://localhost:5207"
        );
        assert_eq!(
            resolve_base_url(Some("  http://localhost:5207  ".to_string())),
            "http://localhost:5207"
        );
    }

    #[test]
    fn resolve_base_url_falls_back_when_override_absent_or_blank() {
        assert_eq!(resolve_base_url(None), DEFAULT_BASE_URL);
        assert_eq!(resolve_base_url(Some(String::new())), DEFAULT_BASE_URL);
        assert_eq!(resolve_base_url(Some("   ".to_string())), DEFAULT_BASE_URL);
    }

    #[test]
    fn subscription_is_requested_for_this_platform() {
        assert_eq!(
            subscription_url("https://store.sqlly.app", "windows"),
            "https://store.sqlly.app/api/billing/subscription?platform=windows"
        );
    }

    #[test]
    fn licensed_platforms_is_optional_in_the_subscription_json() {
        let base = r#"{"id":"s","planId":"lifetime-app-store","planName":"Lifetime (Mac App Store)",
            "status":"Active","currentPeriodStart":"2026-10-05T00:00:00Z",
            "currentPeriodEnd":"9999-12-31T23:59:59Z","hasAccess":true,"willRenew":false,
            "nextRenewalAmount":null"#;
        let with: Subscription =
            serde_json::from_str(&format!(r#"{base},"licensedPlatforms":["macos"]}}"#)).unwrap();
        assert_eq!(with.licensed_platforms, Some(vec!["macos".to_string()]));
        let without: Subscription = serde_json::from_str(&format!("{base}}}")).unwrap();
        assert_eq!(without.licensed_platforms, None);
    }

    #[test]
    fn platform_name_matches_the_server_parser() {
        // Server accepts "macos" | "windows" | "linux" (DeviceService.ParsePlatform).
        assert!(["macos", "windows", "linux"].contains(&platform_name()));
    }

    #[test]
    fn arch_name_matches_the_server_normalizer() {
        // Server's ReleaseArch.Normalize yields "arm64" | "amd64" for the arches
        // it ships builds for; the download route must send one of those so it
        // never drifts from a real artifact name. (Runs on the host arch.)
        let arch = arch_name();
        assert!(
            ["arm64", "amd64"].contains(&arch),
            "unexpected arch token: {arch}"
        );
    }

    #[test]
    fn latest_version_reads_the_checksum_or_its_absence() {
        let with: LatestVersion = serde_json::from_str(
            r#"{"version":"1.9.0","fileName":"SQLLY-macos-arm64-1.9.0.dmg","url":"/downloads/1.9.0/SQLLY-macos-arm64-1.9.0.dmg","os":"macos","arch":"arm64","sha256":"ab12"}"#,
        )
        .unwrap();
        assert_eq!(with.sha256, "ab12");
        assert_eq!(with.file_name, "SQLLY-macos-arm64-1.9.0.dmg");
        let null: LatestVersion =
            serde_json::from_str(r#"{"version":"1.9.0","sha256":null}"#).unwrap();
        assert_eq!(null.sha256, "");
        let absent: LatestVersion = serde_json::from_str(r#"{"version":"1.9.0"}"#).unwrap();
        assert_eq!(absent.sha256, "");
    }

    #[test]
    fn download_latest_url_carries_os_and_arch() {
        let url = download_latest_url();
        assert!(url.contains("/download/latest?os="), "{url}");
        assert!(url.contains(&format!("os={}", platform_name())), "{url}");
        assert!(url.contains(&format!("arch={}", arch_name())), "{url}");
    }

    #[test]
    fn create_pairing_request_serializes_camel_case() {
        let json = serde_json::to_value(CreatePairingRequest {
            name: "studio-mac".to_string(),
            platform: "macos".to_string(),
            app_version: Some("0.21.0".to_string()),
            machine_id: "deadbeef".to_string(),
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "name": "studio-mac",
                "platform": "macos",
                "appVersion": "0.21.0",
                "machineId": "deadbeef"
            })
        );
    }

    #[test]
    fn pairing_registration_round_trips_and_reads_server_shape() {
        let pairing: PairingRegistration = serde_json::from_value(serde_json::json!({
            "deviceId": "dev_1",
            "code": "ABCD-EFGH-JKLM",
            "secret": "s3cr3t",
            "expiresAt": "2026-09-01T12:00:00Z"
        }))
        .unwrap();
        assert_eq!(pairing.device_id, "dev_1");
        assert_eq!(pairing.code, "ABCD-EFGH-JKLM");
        let json = serde_json::to_string(&pairing).unwrap();
        assert_eq!(
            serde_json::from_str::<PairingRegistration>(&json).unwrap(),
            pairing
        );
    }

    #[test]
    fn access_token_response_deserializes_camel_case() {
        let response: AccessTokenResponse = serde_json::from_value(serde_json::json!({
            "accessToken": "jwt",
            "tokenType": "Bearer",
            "expiresAt": "2999-01-01T00:00:00Z",
            "account": {
                "id": "acct_1",
                "email": "support@sqlly.app",
                "displayName": "Dev",
                "hasPassword": true,
                "linkedProviders": []
            }
        }))
        .unwrap();
        assert_eq!(response.access_token, "jwt");
        assert_eq!(response.account.id, "acct_1");
        assert!(!response.is_expired_for_test());
    }

    #[test]
    fn submit_feedback_response_deserializes_camel_case() {
        let response: SubmitFeedbackResponse = serde_json::from_value(serde_json::json!({
            "id": "ifb_123",
            "status": "Submitted"
        }))
        .unwrap();
        assert_eq!(response.id, "ifb_123");
        assert_eq!(response.status, "Submitted");
    }

    #[test]
    fn relay_endpoint_deserializes_the_public_snake_case_shape() {
        // `GET /remote-endpoint-list` emits explicit snake_case field names
        // (RelayEndpointInfo's JsonPropertyName), NOT camelCase — except
        // `relayId`, which is camelCase per the bridge's decoder.
        let endpoints: Vec<RelayEndpoint> = serde_json::from_value(serde_json::json!([
            {
                "relayId": "6f9619ff-8b86-d011-b42d-00cf4fc964ff",
                "hostname": "relay-use2.sqlly.app",
                "location": "US East",
                "status": "Online",
                "capacity_pct": 42
            }
        ]))
        .unwrap();
        assert_eq!(endpoints.len(), 1);
        assert_eq!(
            endpoints[0].relay_id,
            Some("6f9619ff-8b86-d011-b42d-00cf4fc964ff".parse().unwrap())
        );
        assert_eq!(endpoints[0].hostname, "relay-use2.sqlly.app");
        assert_eq!(endpoints[0].location, "US East");
        assert_eq!(endpoints[0].status, "Online");
        assert_eq!(endpoints[0].capacity_pct, 42);
    }

    /// The relay id is nullable/absent until the relay's heartbeat reports it
    /// (matching the bridge's parsing of the same contract) — both shapes must
    /// deserialize with `relay_id: None`.
    #[test]
    fn relay_endpoint_parses_null_or_absent_relay_id_as_none() {
        let endpoints: Vec<RelayEndpoint> = serde_json::from_value(serde_json::json!([
            {
                "relayId": null,
                "hostname": "relay-a.sqlly.app",
                "location": "US East",
                "status": "Online",
                "capacity_pct": 1
            },
            {
                "hostname": "relay-b.sqlly.app",
                "location": "EU",
                "status": "Online",
                "capacity_pct": 2
            }
        ]))
        .unwrap();
        assert_eq!(endpoints[0].relay_id, None);
        assert_eq!(endpoints[1].relay_id, None);
    }

    #[test]
    fn relay_connector_deserializes_camel_case_and_decodes_the_noise_key() {
        use base64::Engine as _;
        // 32 raw bytes -> canonical base64, exactly what the server stores/returns.
        let key_bytes: [u8; 32] = std::array::from_fn(|i| i as u8);
        let key_b64 = base64::engine::general_purpose::STANDARD.encode(key_bytes);

        let connectors: Vec<RelayConnector> = serde_json::from_value(serde_json::json!([
            {
                "deviceId": "rly_abc123",
                "name": "prod-bridge",
                "platform": "linux",
                "noisePublicKey": key_b64,
                "noisePublicKeyFingerprint": "00:01:02:03",
                "registeredAt": "2026-08-01T12:00:00Z",
                "lastSeenAt": "2026-08-04T09:30:00Z"
            }
        ]))
        .unwrap();
        assert_eq!(connectors.len(), 1);
        let connector = &connectors[0];
        assert_eq!(connector.device_id, "rly_abc123");
        assert_eq!(connector.name.as_deref(), Some("prod-bridge"));
        assert_eq!(connector.display_name(), "prod-bridge");
        assert_eq!(connector.noise_public_key_fingerprint, "00:01:02:03");
        // The pinned key round-trips to its 32 raw bytes for PeerSession pinning.
        assert_eq!(connector.noise_public_key_bytes().unwrap(), key_bytes);
    }

    /// v0.30 servers return `name: null` when no override is set, plus a
    /// `hostname`; the display label falls back name → hostname → device id.
    #[test]
    fn relay_connector_tolerates_null_name_and_falls_back_to_hostname() {
        let connectors: Vec<RelayConnector> = serde_json::from_value(serde_json::json!([
            {
                "deviceId": "rly_nameless",
                "name": null,
                "platform": "linux",
                "noisePublicKey": "",
                "noisePublicKeyFingerprint": "",
                "hostname": "db-host-01",
                "registeredAt": "2026-08-01T12:00:00Z",
                "lastSeenAt": "2026-08-04T09:30:00Z"
            }
        ]))
        .unwrap();
        assert_eq!(connectors[0].display_name(), "db-host-01");

        let bare: Vec<RelayConnector> = serde_json::from_value(serde_json::json!([
            {
                "deviceId": "rly_bare",
                "platform": "linux",
                "noisePublicKey": "",
                "noisePublicKeyFingerprint": "",
                "registeredAt": "2026-08-01T12:00:00Z",
                "lastSeenAt": "2026-08-04T09:30:00Z"
            }
        ]))
        .unwrap();
        assert_eq!(bare[0].display_name(), "rly_bare");
    }

    /// The shared-bridge listing is the minimal member projection.
    #[test]
    fn shared_relay_connector_deserializes_camel_case() {
        let shared: Vec<SharedRelayConnector> = serde_json::from_value(serde_json::json!([
            {
                "deviceId": "rly_team",
                "name": "Team bridge",
                "noisePublicKeyFingerprint": "aa:bb:cc"
            }
        ]))
        .unwrap();
        assert_eq!(shared[0].device_id, "rly_team");
        assert_eq!(shared[0].name, "Team bridge");
        assert_eq!(shared[0].noise_public_key_fingerprint, "aa:bb:cc");
    }

    #[test]
    fn noise_public_key_bytes_rejects_wrong_length_and_bad_base64() {
        use base64::Engine as _;
        let short = base64::engine::general_purpose::STANDARD.encode([0u8; 16]);
        let make = |key: &str| RelayConnector {
            device_id: "rly_x".to_string(),
            name: Some("x".to_string()),
            platform: "linux".to_string(),
            noise_public_key: key.to_string(),
            noise_public_key_fingerprint: String::new(),
            hostname: None,
            registered_at: chrono::Utc::now(),
            last_seen_at: chrono::Utc::now(),
        };
        assert!(matches!(
            make(&short).noise_public_key_bytes(),
            Err(StoreError::UnexpectedResponse)
        ));
        assert!(matches!(
            make("not base64!!!").noise_public_key_bytes(),
            Err(StoreError::UnexpectedResponse)
        ));
    }

    #[test]
    fn relay_pair_deserializes_camel_case() {
        let pairs: Vec<RelayPair> = serde_json::from_value(serde_json::json!([
            {
                "id": "pair_1",
                "controlDeviceId": "dev_app",
                "connectorDeviceId": "rly_abc123",
                "createdAt": "2026-08-04T09:30:00Z",
                "active": true
            }
        ]))
        .unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].id, "pair_1");
        assert_eq!(pairs[0].control_device_id, "dev_app");
        assert_eq!(pairs[0].connector_device_id, "rly_abc123");
        assert!(pairs[0].active);
    }

    #[test]
    fn create_relay_pair_request_serializes_camel_case() {
        let json = serde_json::to_value(CreateRelayPairRequest {
            control_device_id: "dev_app".to_string(),
            connector_device_id: "rly_abc123".to_string(),
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "controlDeviceId": "dev_app",
                "connectorDeviceId": "rly_abc123"
            })
        );
    }

    #[test]
    fn subscription_deserializes_the_server_shape() {
        // Mirrors `SubscriptionResponse` (camelCase properties, string-encoded
        // `SubscriptionStatus` via JsonStringEnumConverter).
        let subscription: Subscription = serde_json::from_value(serde_json::json!({
            "id": "sub_1",
            "planId": "pro-monthly",
            "planName": "Pro (Monthly)",
            "status": "Active",
            "currentPeriodStart": "2026-08-01T00:00:00Z",
            "currentPeriodEnd": "2026-09-01T00:00:00Z",
            "hasAccess": true,
            "willRenew": true,
            "nextRenewalAmount": 12.0,
            "isComped": false,
            "assignedByEmail": null,
            "compReason": null
        }))
        .unwrap();
        assert_eq!(subscription.plan_id, "pro-monthly");
        assert_eq!(subscription.plan_name, "Pro (Monthly)");
        assert_eq!(subscription.status, "Active");
        assert!(subscription.has_access);
        assert!(subscription.will_renew);
        assert_eq!(subscription.next_renewal_amount, Some(12.0));
        assert!(!subscription.is_comped);
    }

    #[test]
    fn comped_subscription_parses_far_future_period_end() {
        // Comp grants carry `DateTimeOffset.MaxValue` (year 9999) as their
        // period end — the client must parse it, and the UI treats it as
        // "no expiry".
        let subscription: Subscription = serde_json::from_value(serde_json::json!({
            "id": "sub_comp",
            "planId": "team",
            "planName": "Team",
            "status": "Active",
            "currentPeriodStart": "2026-08-01T00:00:00Z",
            "currentPeriodEnd": "9999-12-31T23:59:59.9999999+00:00",
            "hasAccess": true,
            "willRenew": false,
            "nextRenewalAmount": null,
            "isComped": true,
            "assignedByEmail": "admin@sqlly.app",
            "compReason": "Community contributor"
        }))
        .unwrap();
        assert!(subscription.is_comped);
        assert!(!subscription.will_renew);
        assert_eq!(subscription.next_renewal_amount, None);
        assert_eq!(
            subscription.comp_reason.as_deref(),
            Some("Community contributor")
        );
        use chrono::Datelike;
        assert_eq!(subscription.current_period_end.year(), 9999);
    }

    #[test]
    fn session_roundtrips_through_json() {
        let session = StoreSession {
            access_token: "jwt".to_string(),
            expires_at: chrono::DateTime::parse_from_rfc3339("2026-09-01T12:00:00Z")
                .unwrap()
                .to_utc(),
            account_id: "acct_1".to_string(),
            email: Some("support@sqlly.app".to_string()),
            device_id: Some("dev_1".to_string()),
        };
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(
            serde_json::from_str::<StoreSession>(&json).unwrap(),
            session
        );
    }

    #[test]
    fn expired_sessions_are_detected() {
        let session = StoreSession {
            access_token: "jwt".to_string(),
            expires_at: chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
                .unwrap()
                .to_utc(),
            account_id: "acct_1".to_string(),
            email: None,
            device_id: None,
        };
        assert!(session.is_expired());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn ioreg_parser_extracts_the_platform_uuid() {
        let output = r#"    +-o IOPlatformExpertDevice  <class IOPlatformExpertDevice>
        {
          "IOPlatformUUID" = "12345678-1234-1234-1234-1234567890AB"
        }"#;
        assert_eq!(
            parse_ioreg_platform_uuid(output).as_deref(),
            Some("12345678-1234-1234-1234-1234567890AB")
        );
        assert_eq!(parse_ioreg_platform_uuid("no uuid here"), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_identity_reads_the_registry() {
        let guid = raw_machine_id_windows().expect("MachineGuid is readable");
        assert_eq!(guid.len(), 36, "{guid}");
        assert_eq!(guid.matches('-').count(), 4, "{guid}");
        assert_ne!(hostname(), "unknown-host");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn sha256_hex_is_lowercase_and_deterministic() {
        // NIST test vector for "abc".
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(sha256_hex(b"abc").len(), 64);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn fingerprint_never_returns_the_raw_id() {
        // The fingerprint must be a 64-char hex digest, not the OS value.
        if let Ok(identity) = machine_identity() {
            assert_eq!(identity.machine_id.len(), 64);
            assert!(identity.machine_id.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn relay_pair_deserializes_both_noise_keys() {
        // The Finish-1 server enriches each pair with both peers' pinned keys.
        let pair: RelayPair = serde_json::from_str(
            r#"{
                "id": "pair-1",
                "controlDeviceId": "app-dev",
                "connectorDeviceId": "bridge-dev",
                "createdAt": "2026-08-04T00:00:00Z",
                "active": true,
                "controlNoisePublicKey": "Y29udHJvbA==",
                "connectorNoisePublicKey": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
            }"#,
        )
        .unwrap();
        assert_eq!(pair.control_device_id, "app-dev");
        assert_eq!(pair.connector_device_id, "bridge-dev");
        assert_eq!(
            pair.connector_noise_public_key.as_deref(),
            Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
        );
    }

    #[test]
    fn relay_pair_missing_noise_keys_default_to_none() {
        // A pre-Finish-1 shape (or a side that hasn't registered a key) must
        // still deserialize, with the keys defaulting to None.
        let pair: RelayPair = serde_json::from_str(
            r#"{
                "id": "pair-1",
                "controlDeviceId": "app-dev",
                "connectorDeviceId": "bridge-dev",
                "createdAt": "2026-08-04T00:00:00Z",
                "active": true
            }"#,
        )
        .unwrap();
        assert_eq!(pair.control_noise_public_key, None);
        assert_eq!(pair.connector_noise_public_key, None);
        assert_eq!(pair.connector_noise_public_key_bytes().unwrap(), None);
    }

    #[test]
    fn relay_pair_decodes_connector_key_to_32_bytes() {
        use base64::Engine as _;
        let raw = [7u8; 32];
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
        let pair = RelayPair {
            id: "p".into(),
            control_device_id: "c".into(),
            connector_device_id: "d".into(),
            created_at: chrono::Utc::now(),
            active: true,
            control_noise_public_key: None,
            connector_noise_public_key: Some(encoded),
        };
        assert_eq!(pair.connector_noise_public_key_bytes().unwrap(), Some(raw));
    }

    #[test]
    fn relay_pair_rejects_wrong_length_connector_key() {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        let pair = RelayPair {
            id: "p".into(),
            control_device_id: "c".into(),
            connector_device_id: "d".into(),
            created_at: chrono::Utc::now(),
            active: true,
            control_noise_public_key: None,
            connector_noise_public_key: Some(encoded),
        };
        assert!(matches!(
            pair.connector_noise_public_key_bytes(),
            Err(StoreError::UnexpectedResponse)
        ));
    }

    #[test]
    fn relay_token_response_deserializes_camel_case() {
        let token: RelayTokenResponse = serde_json::from_str(
            r#"{
                "accessToken": "secret.jwt.value",
                "tokenType": "Bearer",
                "expiresAt": "2026-08-04T01:00:00Z",
                "tokenId": "tok-1",
                "accountId": "acct-1",
                "deviceId": "app-dev",
                "deviceKind": "App"
            }"#,
        )
        .unwrap();
        assert_eq!(token.access_token, "secret.jwt.value");
        assert_eq!(token.device_id, "app-dev");
        assert_eq!(token.device_kind, "App");
    }

    #[test]
    fn relay_token_debug_redacts_the_secret() {
        let token = RelayTokenResponse {
            access_token: "super-secret-jwt".to_string(),
            token_type: "Bearer".to_string(),
            expires_at: chrono::Utc::now(),
            token_id: "tok".to_string(),
            account_id: "acct".to_string(),
            device_id: "dev".to_string(),
            device_kind: "App".to_string(),
        };
        let rendered = format!("{token:?}");
        assert!(!rendered.contains("super-secret-jwt"));
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn issue_relay_token_request_serializes_camel_case() {
        let json = serde_json::to_value(IssueRelayTokenRequest {
            device_id: "app-dev".to_string(),
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({ "deviceId": "app-dev" }));
    }

    #[test]
    fn set_app_noise_key_request_serializes_camel_case() {
        let json = serde_json::to_value(SetAppNoiseKeyRequest {
            device_id: "app-dev".to_string(),
            noise_public_key: "Y29udHJvbA==".to_string(),
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "deviceId": "app-dev",
                "noisePublicKey": "Y29udHJvbA=="
            })
        );
    }
}
