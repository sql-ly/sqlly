//! Mac App Store in-app purchase: the one non-consumable "SQLLY Lifetime
//! License" that unlocks the App Store edition.
//!
//! StoreKit 2 lives in Swift (native/macos/SqllyAppStore.swift, linked by
//! build.rs for the `app-store` feature on macOS). Every call here blocks
//! until the App Store answers, so call them off the main thread. Outside the
//! App Store edition the calls answer "unavailable" without touching StoreKit.
//!
//! Ownership is mirrored into the license marker
//! ([`crate::license::record_app_store_entitlement`]) at launch, after every
//! purchase/restore, and whenever StoreKit reports a transaction that happened
//! elsewhere (Ask to Buy approval, refund, purchase on another Mac), so
//! [`crate::license::is_paying`] stays a cheap, offline read.

use serde::Deserialize;

/// The App Store Connect product id of the lifetime unlock.
pub const LIFETIME_PRODUCT_ID: &str = "app.sqlly.lifetime";

/// The product as the App Store presents it, localized for the user's
/// storefront (price included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductInfo {
    pub display_name: String,
    pub display_price: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PurchaseOutcome {
    Purchased,
    /// Waiting on approval (Ask to Buy, a payment method update).
    Pending,
    Cancelled,
    Failed(StoreKitError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOutcome {
    Restored,
    NothingToRestore,
    Failed(StoreKitError),
}

/// A StoreKit failure. StoreKit's own (already OS-localized) text stays raw;
/// SQLLY's fallbacks are keyed, because the StoreKit calls block on a
/// background thread where the UI language is not visible. The UI thread
/// words them with [`crate::localization::message`].
pub type StoreKitError = sqlly_core::DisplayMessage;

/// The failure outside the Mac App Store edition (no StoreKit).
fn unavailable() -> StoreKitError {
    crate::localization::deferred_text("unlock.errors.app_store_edition_only")
}

#[derive(Deserialize)]
struct ProductReply {
    ok: bool,
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    display_price: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct PurchaseReply {
    outcome: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct RestoreReply {
    ok: bool,
    #[serde(default)]
    entitled: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Parse the Swift side's product JSON. Pure.
fn parse_product(json: &str) -> Result<ProductInfo, StoreKitError> {
    let reply: ProductReply =
        serde_json::from_str(json).map_err(|e| StoreKitError::raw(e.to_string()))?;
    if !reply.ok {
        return Err(reply.error.map(StoreKitError::raw).unwrap_or_else(|| {
            crate::localization::deferred_text("unlock.errors.product_load_failed")
        }));
    }
    Ok(ProductInfo {
        display_name: reply.display_name,
        display_price: reply.display_price,
        description: reply.description,
    })
}

/// Parse the Swift side's purchase JSON. Pure.
fn parse_purchase(json: &str) -> PurchaseOutcome {
    match serde_json::from_str::<PurchaseReply>(json) {
        Ok(reply) => {
            match reply.outcome.as_str() {
                "purchased" => PurchaseOutcome::Purchased,
                "pending" => PurchaseOutcome::Pending,
                "cancelled" => PurchaseOutcome::Cancelled,
                _ => PurchaseOutcome::Failed(reply.error.map(StoreKitError::raw).unwrap_or_else(
                    || crate::localization::deferred_text("unlock.errors.purchase_incomplete"),
                )),
            }
        }
        Err(error) => PurchaseOutcome::Failed(StoreKitError::raw(error.to_string())),
    }
}

/// Parse the Swift side's restore JSON. Pure.
fn parse_restore(json: &str) -> RestoreOutcome {
    match serde_json::from_str::<RestoreReply>(json) {
        Ok(reply) if reply.ok && reply.entitled => RestoreOutcome::Restored,
        Ok(reply) if reply.ok => RestoreOutcome::NothingToRestore,
        Ok(reply) => {
            RestoreOutcome::Failed(reply.error.map(StoreKitError::raw).unwrap_or_else(|| {
                crate::localization::deferred_text("unlock.errors.restore_failed")
            }))
        }
        Err(error) => RestoreOutcome::Failed(StoreKitError::raw(error.to_string())),
    }
}

/// Load the lifetime product's localized name and price. Blocking.
pub fn load_product() -> Result<ProductInfo, StoreKitError> {
    // Debug builds only: stand in for StoreKit, which answers only for a
    // signed, sandboxed build, so the Unlock window can be screenshotted
    // (the App Store Connect in-app purchase review screenshot).
    #[cfg(debug_assertions)]
    if let Ok(price) = std::env::var("SQLLY_DEBUG_APP_STORE_PRICE") {
        return Ok(ProductInfo {
            display_name: "SQLLY Lifetime License".to_string(),
            display_price: price,
            description: "Unlimited saved connections, forever. One purchase.".to_string(),
        });
    }
    match ffi::product(LIFETIME_PRODUCT_ID) {
        Some(json) => parse_product(&json),
        None => Err(unavailable()),
    }
}

/// Buy the lifetime unlock, recording ownership on success. Blocking: the
/// App Store shows its own confirmation sheet while this waits.
///
/// When signed in, the purchase carries the SQLLY account id (Apple's
/// `appAccountToken`) and is then recorded on the account (see
/// [`sync_with_account_soon`]).
pub fn purchase() -> PurchaseOutcome {
    // Without StoreKit, answer before reading the session: that read touches
    // the keychain, which can block on a locked keychain (it hung CI's tests).
    if !cfg!(all(feature = "app-store", target_os = "macos")) {
        return PurchaseOutcome::Failed(unavailable());
    }
    let token = signed_in_session().and_then(|s| app_account_token(&s.account_id));
    let Some(json) = ffi::purchase(LIFETIME_PRODUCT_ID, token.as_deref()) else {
        return PurchaseOutcome::Failed(unavailable());
    };
    let outcome = parse_purchase(&json);
    if outcome == PurchaseOutcome::Purchased {
        crate::license::record_app_store_entitlement(true);
        sync_with_account_soon();
    }
    outcome
}

// ---------------------------------------------------------------------------
// Recording App Store purchases on the SQLLY account
// ---------------------------------------------------------------------------
//
// An App Store purchase is Apple's record; the SQLLY store also needs it so
// the account shows the license (Devices, other Macs, support, refunds via
// App Store Server Notifications). Whenever the unlock is owned and the user
// is signed in — after a purchase or restore, at launch, after signing in,
// and when StoreKit reports a transaction — the Apple-signed transaction is
// posted to `POST /api/billing/apple/transactions`, which verifies Apple's
// signature and grants the account the App Store lifetime license. A marker
// remembers which (account, original transaction) pair is already recorded
// so this happens once per account.

/// Where the "already recorded" marker lives: `<account id>:<original
/// transaction id>`.
const RECORDED_ARTIFACT: &str = "app-store-recorded.txt";

/// The last problem recording the purchase on the account (e.g. it belongs
/// to another account), shown in the Unlock SQLLY window. `None` when fine.
static ACCOUNT_LINK_PROBLEM: std::sync::Mutex<Option<sqlly_core::DisplayMessage>> =
    std::sync::Mutex::new(None);

/// The problem from the last attempt to record the purchase, if any. Set on
/// the runtime; the UI words it with [`crate::localization::message`].
pub fn account_link_problem() -> Option<sqlly_core::DisplayMessage> {
    ACCOUNT_LINK_PROBLEM
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn set_account_link_problem(problem: Option<sqlly_core::DisplayMessage>) {
    *ACCOUNT_LINK_PROBLEM
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = problem;
}

/// Apple's `appAccountToken` must be a UUID. SQLLY account ids are
/// `acct_` + a GUID's 32 hex digits, so the GUID is sent (the server maps it
/// back to the account, see `AppStorePurchaseService.AccountIdForToken`);
/// anything else is simply not sent. Pure.
fn app_account_token(account_id: &str) -> Option<String> {
    let id = account_id.trim();
    uuid::Uuid::parse_str(id.strip_prefix("acct_").unwrap_or(id))
        .ok()
        .map(|uuid| uuid.hyphenated().to_string())
}

/// `originalTransactionId` from a StoreKit JWS payload, without verifying it
/// (the server verifies; this only keys the local marker). Pure.
fn original_transaction_id(jws: &str) -> Option<String> {
    use base64::Engine as _;
    let payload = jws.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    match value.get("originalTransactionId")? {
        serde_json::Value::String(id) => Some(id.clone()),
        serde_json::Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

/// The marker text for an (account, original transaction) pair. Pure.
fn recorded_marker(account_id: &str, original_transaction_id: &str) -> String {
    format!("{account_id}:{original_transaction_id}")
}

fn signed_in_session() -> Option<crate::store::StoreSession> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        crate::store::session::load().filter(|session| !session.is_expired())
    }
    #[cfg(target_arch = "wasm32")]
    {
        None
    }
}

/// Record the owned unlock on the signed-in account, in the background,
/// unless it is already recorded for this account. A no-op outside the App
/// Store edition, when signed out, or when the unlock isn't owned.
pub fn sync_with_account_soon() {
    if !crate::distribution::is_app_store() {
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    crate::tokio_handle().spawn(async {
        let Some(session) = signed_in_session() else {
            return;
        };
        let Ok(Some(jws)) =
            tokio::task::spawn_blocking(|| ffi::entitlement_jws(LIFETIME_PRODUCT_ID)).await
        else {
            return;
        };
        let Some(original) = original_transaction_id(&jws) else {
            tracing::warn!("App Store transaction has no originalTransactionId");
            return;
        };
        let marker = recorded_marker(&session.account_id, &original);
        let recorded = crate::host_storage::load_text(
            crate::host_storage::StorageArea::Data,
            RECORDED_ARTIFACT,
        );
        if recorded.as_deref().map(str::trim) == Some(marker.as_str()) {
            return;
        }
        match crate::store::record_app_store_transaction(&session.access_token, &jws).await {
            Ok(subscription) => {
                let _ = crate::host_storage::store_text(
                    crate::host_storage::StorageArea::Data,
                    RECORDED_ARTIFACT,
                    &marker,
                );
                crate::license::record_entitlement(crate::license::entitlement_from_subscription(
                    Some(&subscription),
                ));
                set_account_link_problem(None);
            }
            Err(crate::store::StoreError::Server { status: 409, .. }) => {
                set_account_link_problem(Some(crate::localization::deferred_text(
                    "unlock.errors.purchase_linked_to_other_account",
                )));
            }
            Err(error) => {
                // Offline or a server hiccup: the next launch, sign-in or
                // transaction retries, since nothing was marked recorded.
                tracing::info!(%error, "could not record the App Store purchase yet");
            }
        }
    });
}

/// Restore Purchases: resync with the App Store and record the answer.
/// Blocking; may prompt the user to sign in to their Apple account.
pub fn restore() -> RestoreOutcome {
    let Some(json) = ffi::restore(LIFETIME_PRODUCT_ID) else {
        return RestoreOutcome::Failed(unavailable());
    };
    let outcome = parse_restore(&json);
    match outcome {
        RestoreOutcome::Restored => {
            crate::license::record_app_store_entitlement(true);
            sync_with_account_soon();
        }
        RestoreOutcome::NothingToRestore => crate::license::record_app_store_entitlement(false),
        RestoreOutcome::Failed(_) => {}
    }
    outcome
}

/// Launch-time setup for the App Store edition: re-check ownership against
/// StoreKit's verified local transaction history (works offline) and start
/// listening for transactions that arrive later. Spawns its own thread; a
/// no-op in other editions.
pub fn start() {
    if !crate::distribution::is_app_store() {
        return;
    }
    ffi::start_listener(LIFETIME_PRODUCT_ID);
    let spawned = std::thread::Builder::new()
        .name("app-store-entitlement".to_string())
        .spawn(|| {
            if let Some(owned) = ffi::is_entitled(LIFETIME_PRODUCT_ID) {
                crate::license::record_app_store_entitlement(owned);
                if owned {
                    sync_with_account_soon();
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "could not start the App Store entitlement check");
    }
}

/// The Mac's IOPlatformUUID, read in-process (App Store edition; the sandbox
/// can't run `ioreg`).
#[cfg(all(feature = "app-store", target_os = "macos"))]
pub fn platform_uuid() -> Option<String> {
    ffi::platform_uuid().filter(|uuid| !uuid.is_empty())
}

/// Post a macOS user notification through the Notification Center (App Store
/// edition; the sandbox forbids the osascript route).
#[cfg(all(feature = "app-store", target_os = "macos"))]
pub fn notify(title: &str, body: &str) {
    ffi::notify(title, body);
}

#[cfg(all(feature = "app-store", target_os = "macos"))]
mod ffi {
    use std::ffi::{CStr, CString, c_char};

    unsafe extern "C" {
        fn sqlly_free_string(pointer: *mut c_char);
        fn sqlly_iap_product(product_id: *const c_char) -> *mut c_char;
        fn sqlly_iap_purchase(
            product_id: *const c_char,
            app_account_token: *const c_char,
        ) -> *mut c_char;
        fn sqlly_iap_entitlement_jws(product_id: *const c_char) -> *mut c_char;
        fn sqlly_iap_is_entitled(product_id: *const c_char) -> i32;
        fn sqlly_iap_restore(product_id: *const c_char) -> *mut c_char;
        fn sqlly_iap_start_listener(product_id: *const c_char, callback: extern "C" fn(i32));
        fn sqlly_notify(title: *const c_char, body: *const c_char);
        fn sqlly_platform_uuid() -> *mut c_char;
    }

    pub fn platform_uuid() -> Option<String> {
        // SAFETY: takes no input; the result is taken exactly once.
        take(unsafe { sqlly_platform_uuid() })
    }

    pub fn notify(title: &str, body: &str) {
        let (Ok(title), Ok(body)) = (CString::new(title), CString::new(body)) else {
            return;
        };
        // SAFETY: both strings outlive the call; the Swift side copies them.
        unsafe { sqlly_notify(title.as_ptr(), body.as_ptr()) };
    }

    /// Take ownership of a string the Swift side `strdup`ed.
    fn take(pointer: *mut c_char) -> Option<String> {
        if pointer.is_null() {
            return None;
        }
        // SAFETY: non-null pointers from the Swift glue are NUL-terminated
        // `strdup` results that we own until `sqlly_free_string`.
        let text = unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: freed exactly once, with the allocator that created it.
        unsafe { sqlly_free_string(pointer) };
        Some(text)
    }

    fn id(product_id: &str) -> CString {
        CString::new(product_id).expect("product ids contain no NUL")
    }

    pub fn product(product_id: &str) -> Option<String> {
        let id = id(product_id);
        // SAFETY: `id` outlives the call; the result is taken exactly once.
        take(unsafe { sqlly_iap_product(id.as_ptr()) })
    }

    pub fn purchase(product_id: &str, app_account_token: Option<&str>) -> Option<String> {
        let id = id(product_id);
        let token = app_account_token.and_then(|t| CString::new(t).ok());
        let token_ptr = token.as_ref().map_or(std::ptr::null(), |t| t.as_ptr());
        // SAFETY: `id` and `token` outlive the call (a null token means
        // none); the result is taken exactly once.
        take(unsafe { sqlly_iap_purchase(id.as_ptr(), token_ptr) })
    }

    pub fn entitlement_jws(product_id: &str) -> Option<String> {
        let id = id(product_id);
        // SAFETY: as in `product`.
        take(unsafe { sqlly_iap_entitlement_jws(id.as_ptr()) })
    }

    pub fn restore(product_id: &str) -> Option<String> {
        let id = id(product_id);
        // SAFETY: as in `product`.
        take(unsafe { sqlly_iap_restore(id.as_ptr()) })
    }

    pub fn is_entitled(product_id: &str) -> Option<bool> {
        let id = id(product_id);
        // SAFETY: `id` outlives the call.
        Some(unsafe { sqlly_iap_is_entitled(id.as_ptr()) } == 1)
    }

    extern "C" fn on_transaction_update(owned: i32) {
        crate::license::record_app_store_entitlement(owned == 1);
        if owned == 1 {
            super::sync_with_account_soon();
        }
    }

    pub fn start_listener(product_id: &str) {
        let id = id(product_id);
        // SAFETY: the Swift side copies the id before returning; the callback
        // is a plain `extern "C"` fn that lives for the whole process.
        unsafe { sqlly_iap_start_listener(id.as_ptr(), on_transaction_update) };
    }
}

#[cfg(not(all(feature = "app-store", target_os = "macos")))]
mod ffi {
    pub fn product(_product_id: &str) -> Option<String> {
        None
    }

    pub fn purchase(_product_id: &str, _app_account_token: Option<&str>) -> Option<String> {
        None
    }

    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn entitlement_jws(_product_id: &str) -> Option<String> {
        None
    }

    pub fn restore(_product_id: &str) -> Option<String> {
        None
    }

    pub fn is_entitled(_product_id: &str) -> Option<bool> {
        None
    }

    pub fn start_listener(_product_id: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_reply_carries_the_localized_price() {
        let info = parse_product(
            r#"{"ok":true,"display_name":"SQLLY Lifetime License","display_price":"$69.99","description":"Unlimited connections"}"#,
        )
        .unwrap();
        assert_eq!(info.display_name, "SQLLY Lifetime License");
        assert_eq!(info.display_price, "$69.99");
    }

    #[test]
    fn product_error_is_surfaced() {
        let err = parse_product(r#"{"ok":false,"error":"offline"}"#).unwrap_err();
        assert_eq!(err, "offline");
        assert!(parse_product("not json").is_err());
    }

    #[test]
    fn purchase_outcomes_map_one_to_one() {
        assert_eq!(
            parse_purchase(r#"{"outcome":"purchased"}"#),
            PurchaseOutcome::Purchased
        );
        assert_eq!(
            parse_purchase(r#"{"outcome":"pending"}"#),
            PurchaseOutcome::Pending
        );
        assert_eq!(
            parse_purchase(r#"{"outcome":"cancelled"}"#),
            PurchaseOutcome::Cancelled
        );
        assert_eq!(
            parse_purchase(r#"{"outcome":"failed","error":"card declined"}"#),
            PurchaseOutcome::Failed("card declined".into())
        );
        assert!(matches!(
            parse_purchase("garbage"),
            PurchaseOutcome::Failed(_)
        ));
    }

    #[test]
    fn app_account_token_is_the_account_ids_guid() {
        assert_eq!(
            app_account_token("6F9619FF-8B86-D011-B42D-00C04FC964FF").as_deref(),
            Some("6f9619ff-8b86-d011-b42d-00c04fc964ff")
        );
        assert_eq!(
            app_account_token("acct_af01841160e14449b75b1dbd4c4ba560").as_deref(),
            Some("af018411-60e1-4449-b75b-1dbd4c4ba560")
        );
        assert_eq!(app_account_token("acct_123"), None);
        assert_eq!(app_account_token(""), None);
    }

    #[test]
    fn original_transaction_id_is_read_from_the_jws_payload() {
        use base64::Engine as _;
        let encode =
            |json: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json.as_bytes());
        let jws = format!(
            "{}.{}.sig",
            encode(r#"{"alg":"ES256"}"#),
            encode(r#"{"originalTransactionId":"2000000123","productId":"app.sqlly.lifetime"}"#)
        );
        assert_eq!(original_transaction_id(&jws).as_deref(), Some("2000000123"));
        let numeric = format!("h.{}.s", encode(r#"{"originalTransactionId":42}"#));
        assert_eq!(original_transaction_id(&numeric).as_deref(), Some("42"));
        assert_eq!(original_transaction_id("not-a-jws"), None);
        assert_eq!(original_transaction_id("a.!!!.c"), None);
    }

    #[test]
    fn recorded_marker_pairs_account_and_transaction() {
        assert_eq!(recorded_marker("acct", "2000"), "acct:2000");
        assert_ne!(recorded_marker("a", "1"), recorded_marker("b", "1"));
    }

    #[test]
    fn restore_distinguishes_nothing_to_restore_from_failure() {
        assert_eq!(
            parse_restore(r#"{"ok":true,"entitled":true}"#),
            RestoreOutcome::Restored
        );
        assert_eq!(
            parse_restore(r#"{"ok":true,"entitled":false}"#),
            RestoreOutcome::NothingToRestore
        );
        assert_eq!(
            parse_restore(r#"{"ok":false,"error":"signed out"}"#),
            RestoreOutcome::Failed("signed out".into())
        );
    }

    #[cfg(not(feature = "app-store"))]
    #[test]
    fn outside_the_app_store_edition_purchases_are_unavailable() {
        assert_eq!(load_product().unwrap_err(), unavailable());
        assert_eq!(purchase(), PurchaseOutcome::Failed(unavailable()));
        assert_eq!(restore(), RestoreOutcome::Failed(unavailable()));
        assert!(unavailable().contains("Mac App Store edition"));
    }
}
