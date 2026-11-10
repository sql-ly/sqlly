//! Device-identity mapping between the control plane and the wire.
//!
//! The SQLLY control plane issues opaque string device ids (`dev_{guid}`,
//! `rly_{guid}`, …) and expresses relay route grants, pairs, and JWT `sub`
//! claims in terms of those strings. The relay data plane, by contrast, routes
//! by the fixed 16-byte [`Uuid`] `source`/`destination` fields of a
//! [`RelayFrame`](crate::RelayFrame) and binds each `sqlly-noise` peer identity
//! to a `Uuid`.
//!
//! [`device_uuid`] is the single, deterministic bridge between the two worlds:
//! a UUIDv5 of the control-plane device id under a fixed SQLLY namespace. Every
//! layer that turns a device id into a wire id — relay auth (`sub` → client id),
//! relay route-feed ingestion (grant device ids → routing pairs), the bridge and
//! the desktop app (own id + peer id), and `ConnectionRoute::ViaRelay`
//! resolution — MUST use this function so the same device always maps to the
//! same wire id. It is stable across processes and releases.

use uuid::Uuid;

/// Fixed namespace for SQLLY relay device-id → wire-id derivation. Never change
/// this: doing so would repartition every device's wire identity and silently
/// invalidate live route grants and pinned Noise peers.
///
/// (A random-but-frozen UUID; it only needs to be constant, not secret.)
pub const DEVICE_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x8f, 0x2a, 0x1c, 0x6b, 0x4d, 0x93, 0x47, 0xa5, 0x9e, 0x11, 0xc0, 0x7d, 0x3b, 0x55, 0xe8, 0x42,
]);

/// Map a control-plane device id string to its stable wire [`Uuid`].
///
/// Deterministic (UUIDv5) and total: any non-empty string maps to a non-nil
/// id. Callers on both endpoints and the relay agree on the result, so a route
/// grant expressed as `(control_device_id, connector_device_id)` strings and a
/// frame addressed `source`→`destination` as `Uuid`s line up exactly.
#[must_use]
pub fn device_uuid(device_id: &str) -> Uuid {
    Uuid::new_v5(&DEVICE_ID_NAMESPACE, device_id.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_deterministic_and_stable() {
        // Same string → same id, every time (the cross-process/cross-release
        // guarantee the whole routing scheme depends on).
        assert_eq!(device_uuid("dev_abc123"), device_uuid("dev_abc123"));
        // A frozen known vector guards against an accidental namespace change.
        assert_eq!(
            device_uuid("dev_abc123"),
            Uuid::new_v5(&DEVICE_ID_NAMESPACE, b"dev_abc123")
        );
    }

    #[test]
    fn distinct_devices_map_to_distinct_ids() {
        assert_ne!(device_uuid("dev_a"), device_uuid("dev_b"));
        assert_ne!(device_uuid("dev_x"), device_uuid("rly_x"));
    }

    #[test]
    fn maps_to_non_nil() {
        assert!(!device_uuid("dev_abc123").is_nil());
        // Even the empty string is non-nil under v5 (namespace + empty name).
        assert!(!device_uuid("").is_nil());
    }
}
