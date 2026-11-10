// Mac App Store glue for the `app-store` build of SQLLY: StoreKit 2 in-app
// purchase and security-scoped bookmarks (so the sandboxed app can reopen
// files and folders the user picked after a relaunch).
//
// Compiled by build.rs into a static library and called from Rust through a
// small C ABI (src/app_store/ffi.rs). Every exported function except the
// listener blocks the calling thread until StoreKit answers, so Rust must call
// them from a background thread, never the main thread. Strings returned to
// Rust are `strdup`ed JSON and must be released with `sqlly_free_string`.

import Foundation
import IOKit
import StoreKit
import UserNotifications

// MARK: - Helpers

private final class Box<T>: @unchecked Sendable {
    var value: T?
}

/// Run `body` on the Swift concurrency pool and block until it finishes.
private func blocking<T>(_ body: @escaping @Sendable () async -> T) -> T {
    let semaphore = DispatchSemaphore(value: 0)
    let box = Box<T>()
    Task.detached {
        box.value = await body()
        semaphore.signal()
    }
    semaphore.wait()
    return box.value!
}

private func json(_ object: [String: Any]) -> UnsafeMutablePointer<CChar> {
    let data = (try? JSONSerialization.data(withJSONObject: object)) ?? Data("{}".utf8)
    return strdup(String(decoding: data, as: UTF8.self))!
}

private func string(_ pointer: UnsafePointer<CChar>) -> String {
    String(cString: pointer)
}

/// Whether the user currently owns `productID` (verified, not revoked).
private func ownsProduct(_ productID: String) async -> Bool {
    for await result in Transaction.currentEntitlements {
        guard case .verified(let transaction) = result else { continue }
        if transaction.productID == productID && transaction.revocationDate == nil {
            return true
        }
    }
    return false
}

@_cdecl("sqlly_free_string")
public func sqllyFreeString(_ pointer: UnsafeMutablePointer<CChar>?) {
    free(pointer)
}

// MARK: - In-app purchase

/// `{"ok":true,"display_name":…,"display_price":…,"description":…}` or
/// `{"ok":false,"error":…}`.
@_cdecl("sqlly_iap_product")
public func sqllyIapProduct(_ productID: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar> {
    let id = string(productID)
    return blocking {
        do {
            guard let product = try await Product.products(for: [id]).first else {
                return json(["ok": false, "error": "The App Store has no product \(id) for this build. Purchases work in TestFlight and App Store builds once the in-app purchase is available there."])
            }
            return json([
                "ok": true,
                "display_name": product.displayName,
                "display_price": product.displayPrice,
                "description": product.description,
            ])
        } catch {
            return json(["ok": false, "error": error.localizedDescription])
        }
    }
}

/// `{"outcome":"purchased"|"pending"|"cancelled"|"failed","jws"?:…,"error"?:…}`.
/// `appAccountToken` (nullable) is the signed-in SQLLY account's UUID; Apple
/// stamps it on the transaction so the store server can match refunds to the
/// account. `jws` is the Apple-signed transaction the server verifies.
@_cdecl("sqlly_iap_purchase")
public func sqllyIapPurchase(
    _ productID: UnsafePointer<CChar>,
    _ appAccountToken: UnsafePointer<CChar>?
) -> UnsafeMutablePointer<CChar> {
    let id = string(productID)
    let token = appAccountToken.flatMap { UUID(uuidString: string($0)) }
    return blocking {
        do {
            guard let product = try await Product.products(for: [id]).first else {
                return json(["outcome": "failed", "error": "The App Store has no product \(id) for this build. Purchases work in TestFlight and App Store builds once the in-app purchase is available there."])
            }
            var options: Set<Product.PurchaseOption> = []
            if let token {
                options.insert(.appAccountToken(token))
            }
            switch try await product.purchase(options: options) {
            case .success(let verification):
                switch verification {
                case .verified(let transaction):
                    await transaction.finish()
                    return json(["outcome": "purchased", "jws": verification.jwsRepresentation])
                case .unverified(_, let error):
                    return json(["outcome": "failed", "error": "The purchase could not be verified: \(error.localizedDescription)"])
                }
            case .pending:
                return json(["outcome": "pending"])
            case .userCancelled:
                return json(["outcome": "cancelled"])
            @unknown default:
                return json(["outcome": "failed", "error": "The App Store returned an unknown purchase result."])
            }
        } catch {
            return json(["outcome": "failed", "error": error.localizedDescription])
        }
    }
}

/// The Apple-signed JWS of the current, verified entitlement to `productID`,
/// or NULL when it isn't owned. Sent to the store server to record the
/// purchase on the signed-in account.
@_cdecl("sqlly_iap_entitlement_jws")
public func sqllyIapEntitlementJws(_ productID: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? {
    let id = string(productID)
    let jws: String? = blocking {
        for await result in Transaction.currentEntitlements {
            guard case .verified(let transaction) = result else { continue }
            if transaction.productID == id && transaction.revocationDate == nil {
                return result.jwsRepresentation
            }
        }
        return nil
    }
    return jws.flatMap { strdup($0) }
}

/// 1 when the product is owned, 0 when not.
@_cdecl("sqlly_iap_is_entitled")
public func sqllyIapIsEntitled(_ productID: UnsafePointer<CChar>) -> Int32 {
    let id = string(productID)
    return blocking { await ownsProduct(id) ? 1 : 0 }
}

/// Ask the App Store to resync this Apple account's purchases (Restore
/// Purchases). `{"ok":true,"entitled":Bool}` or `{"ok":false,"error":…}`.
@_cdecl("sqlly_iap_restore")
public func sqllyIapRestore(_ productID: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar> {
    let id = string(productID)
    return blocking {
        do {
            try await AppStore.sync()
            return json(["ok": true, "entitled": await ownsProduct(id)])
        } catch {
            return json(["ok": false, "error": error.localizedDescription])
        }
    }
}

/// Start listening for transactions that arrive outside a purchase call
/// (Ask to Buy approvals, refunds, purchases on another Mac). Calls
/// `callback(1|0)` with the product's ownership after each one. Returns
/// immediately; call once at launch.
@_cdecl("sqlly_iap_start_listener")
public func sqllyIapStartListener(
    _ productID: UnsafePointer<CChar>,
    _ callback: @escaping @convention(c) (Int32) -> Void
) {
    let id = string(productID)
    Task.detached {
        for await result in Transaction.updates {
            if case .verified(let transaction) = result {
                await transaction.finish()
            }
            callback(await ownsProduct(id) ? 1 : 0)
        }
    }
}

// MARK: - Security-scoped bookmarks

/// Base64 security-scoped bookmark for `path`, or NULL when the sandbox does
/// not currently grant access to it.
@_cdecl("sqlly_bookmark_create")
public func sqllyBookmarkCreate(_ path: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? {
    let url = URL(fileURLWithPath: string(path))
    guard let data = try? url.bookmarkData(
        options: [.withSecurityScope],
        includingResourceValuesForKeys: nil,
        relativeTo: nil
    ) else {
        return nil
    }
    return strdup(data.base64EncodedString())
}

/// Resolve a bookmark from `sqlly_bookmark_create` and start accessing it for
/// the rest of the process's life. `{"ok":true,"path":…,"stale":Bool}` or
/// `{"ok":false,"error":…}`. A stale bookmark still grants access; the caller
/// should re-create it from the returned path.
@_cdecl("sqlly_bookmark_resolve")
public func sqllyBookmarkResolve(_ base64: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar> {
    guard let data = Data(base64Encoded: string(base64)) else {
        return json(["ok": false, "error": "The bookmark is not valid base64."])
    }
    var stale = false
    do {
        let url = try URL(
            resolvingBookmarkData: data,
            options: [.withSecurityScope],
            relativeTo: nil,
            bookmarkDataIsStale: &stale
        )
        guard url.startAccessingSecurityScopedResource() else {
            return json(["ok": false, "error": "macOS refused access to \(url.path)."])
        }
        return json(["ok": true, "path": url.path, "stale": stale])
    } catch {
        return json(["ok": false, "error": error.localizedDescription])
    }
}

// MARK: - Notifications

/// Post a user notification through the Notification Center (asking for
/// permission the first time). The sandbox forbids the osascript route the
/// direct-download build uses. Returns immediately.
@_cdecl("sqlly_notify")
public func sqllyNotify(_ title: UnsafePointer<CChar>, _ body: UnsafePointer<CChar>) {
    let content = UNMutableNotificationContent()
    content.title = string(title)
    content.body = string(body)
    let center = UNUserNotificationCenter.current()
    center.requestAuthorization(options: [.alert, .sound]) { granted, _ in
        guard granted else { return }
        let request = UNNotificationRequest(
            identifier: UUID().uuidString,
            content: content,
            trigger: nil
        )
        center.add(request)
    }
}

// MARK: - Machine identity

/// The platform expert's IOPlatformUUID (what `ioreg -c IOPlatformExpertDevice`
/// prints), read in-process because the sandbox can't run `ioreg`. NULL when
/// unavailable. Only ever leaves the app hashed.
@_cdecl("sqlly_platform_uuid")
public func sqllyPlatformUuid() -> UnsafeMutablePointer<CChar>? {
    let service = IOServiceGetMatchingService(kIOMainPortDefault, IOServiceMatching("IOPlatformExpertDevice"))
    guard service != 0 else { return nil }
    defer { IOObjectRelease(service) }
    guard
        let property = IORegistryEntryCreateCFProperty(
            service, "IOPlatformUUID" as CFString, kCFAllocatorDefault, 0
        )?.takeRetainedValue(),
        let uuid = property as? String
    else {
        return nil
    }
    return strdup(uuid)
}
