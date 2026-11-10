//! MessagePack payload decoding for the value viewer's MessagePack format.
//!
//! Any payload becomes a JSON-shaped value so the viewer's existing JSON
//! pretty-printer, highlighter, and tree render it. What JSON cannot
//! express becomes an explicit, visible object rather than disappearing:
//! binary payloads decode to `{"$binary": "<hex>"}`, the standard timestamp
//! extension (type -1) to `{"$timestamp": "<RFC 3339>"}`, any other
//! extension to `{"$ext": {"type": <n>, "data": "<hex>"}}`, and a map whose
//! keys would collide once rendered as JSON object keys (an integer `1` next
//! to the string `"1"`, or a duplicated key) to `{"$map": [[key, value], …]}`
//! so no entry is lost. Structural errors fail the whole decode and the
//! viewer falls back to its honest hex dump. Trailing bytes after the first
//! value are an error too — a Redis value is exactly one document.
//!
//! The payload is untrusted: every length in it is a claim, not a fact.
//! Container pre-allocation is capped by the bytes actually present (each
//! element needs at least one byte) and by [`MAX_PREALLOC`], string and
//! binary reads are bounded by the input slice, and nesting is capped at
//! [`MAX_DEPTH`].

use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::fmt;

/// Deepest container nesting decoded before the payload is refused.
pub const MAX_DEPTH: usize = 128;

/// Most elements pre-allocated for one array or map, whatever its header
/// claims; the vector still grows past this when the elements really exist.
const MAX_PREALLOC: usize = 1024;

/// Decode MessagePack bytes into a JSON value. Structural errors and
/// trailing garbage are an honest `Err` — the caller falls back to the hex
/// dump rather than showing a partial decode.
pub fn decode(bytes: &[u8]) -> Result<serde_json::Value, String> {
    let mut deserializer = rmp_serde::Deserializer::new(std::io::Cursor::new(bytes));
    deserializer.set_max_depth(MAX_DEPTH);
    let seed = AnySeed {
        input_len: bytes.len(),
    };
    let value = seed.deserialize(&mut deserializer).map_err(|error| {
        crate::localization::format(
            "results.structured_data.messagepack_decode_failed",
            &[("error", &error.to_string())],
        )
    })?;
    let consumed = deserializer.position() as usize;
    if consumed < bytes.len() {
        return Err(crate::localization::format(
            "results.structured_data.messagepack_trailing_bytes",
            &[("byte_count", &(bytes.len() - consumed).to_string())],
        ));
    }
    Ok(value)
}

/// Capacity to reserve for a container whose header claims `hint`
/// elements: never more than the input could hold, never more than
/// [`MAX_PREALLOC`]. A 5-byte `dd ff ff ff ff` must not reserve gigabytes.
fn bounded_capacity(hint: Option<usize>, input_len: usize) -> usize {
    hint.unwrap_or(0).min(input_len).min(MAX_PREALLOC)
}

/// Lowercase hex, no separators — compact inside the `$binary` object.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// "Any value" seed; carries the input length so nested containers can
/// bound their pre-allocation.
#[derive(Clone, Copy)]
struct AnySeed {
    input_len: usize,
}

impl<'de> DeserializeSeed<'de> for AnySeed {
    type Value = serde_json::Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(JsonVisitor {
            input_len: self.input_len,
        })
    }
}

/// A map key rendered as JSON object keys must be: strings verbatim,
/// anything else as its compact JSON text (`1` → `"1"`, `true` → `"true"`).
fn key_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Build the JSON value for a decoded map: a plain object when every key
/// renders to a distinct object key, otherwise the lossless `$map` pair
/// list (`{1: "a", "1": "b"}` must not silently drop one entry).
fn map_value(entries: Vec<(serde_json::Value, serde_json::Value)>) -> serde_json::Value {
    let mut seen = std::collections::HashSet::with_capacity(entries.len());
    let distinct = entries.iter().all(|(key, _)| seen.insert(key_text(key)));
    if distinct {
        let mut object = serde_json::Map::with_capacity(entries.len());
        for (key, value) in entries {
            object.insert(key_text(&key), value);
        }
        return serde_json::Value::Object(object);
    }
    let pairs = entries
        .into_iter()
        .map(|(key, value)| serde_json::Value::Array(vec![key, value]))
        .collect();
    let mut object = serde_json::Map::new();
    object.insert("$map".to_string(), serde_json::Value::Array(pairs));
    serde_json::Value::Object(object)
}

/// A JSON number, or the float's text when JSON has no number for it
/// (NaN, ±infinity).
fn float_value(value: f64) -> serde_json::Value {
    serde_json::Number::from_f64(value)
        .map(serde_json::Value::Number)
        .unwrap_or_else(|| serde_json::Value::String(value.to_string()))
}

struct JsonVisitor {
    input_len: usize,
}

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str(&crate::localization::text(
            "results.structured_data.messagepack_expected_value",
        ))
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::from(value))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::from(value))
    }

    fn visit_f32<E>(self, value: f32) -> Result<Self::Value, E> {
        // Widen via the shortest decimal text so float32 1.1 shows as 1.1,
        // not 1.100000023841858.
        let widened = value.to_string().parse::<f64>().unwrap_or(value as f64);
        Ok(float_value(widened))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
        Ok(float_value(value))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(value))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    /// rmp-serde's `deserialize_any` only produces a newtype struct for an
    /// extension value, delivered as the `(type, data)` pair — reshape it
    /// into the visible `$timestamp` / `$ext` object.
    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let (kind, data) = deserializer.deserialize_any(ExtVisitor)?;
        Ok(ext_value(kind, &data))
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E> {
        Ok(binary_object(&hex(value)))
    }

    fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E> {
        Ok(binary_object(&hex(&value)))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let seed = AnySeed {
            input_len: self.input_len,
        };
        let mut items = Vec::with_capacity(bounded_capacity(seq.size_hint(), self.input_len));
        while let Some(item) = seq.next_element_seed(seed)? {
            items.push(item);
        }
        Ok(serde_json::Value::Array(items))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let seed = AnySeed {
            input_len: self.input_len,
        };
        let mut entries = Vec::with_capacity(bounded_capacity(map.size_hint(), self.input_len));
        while let Some(entry) = map.next_entry_seed(seed, seed)? {
            entries.push(entry);
        }
        Ok(map_value(entries))
    }
}

/// Reads an extension's `(type, data)` pair as rmp-serde presents it.
struct ExtVisitor;

impl<'de> Visitor<'de> for ExtVisitor {
    type Value = (i8, Vec<u8>);

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str(&crate::localization::text(
            "results.structured_data.messagepack_expected_extension",
        ))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        use serde::de::Error as _;
        let kind: i8 = seq.next_element()?.ok_or_else(|| {
            A::Error::custom(crate::localization::text(
                "results.structured_data.messagepack_extension_missing_type",
            ))
        })?;
        let data: RawBytes = seq.next_element()?.ok_or_else(|| {
            A::Error::custom(crate::localization::text(
                "results.structured_data.messagepack_extension_missing_data",
            ))
        })?;
        Ok((kind, data.0))
    }
}

/// Extension data bytes. Their length was checked against the input by
/// rmp-serde's slice reader before they reach us.
struct RawBytes(Vec<u8>);

impl<'de> serde::Deserialize<'de> for RawBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct BytesVisitor;
        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = RawBytes;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str(&crate::localization::text(
                    "results.structured_data.messagepack_expected_extension_data",
                ))
            }
            fn visit_bytes<E>(self, value: &[u8]) -> Result<RawBytes, E> {
                Ok(RawBytes(value.to_vec()))
            }
            fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<RawBytes, E> {
                Ok(RawBytes(value))
            }
        }
        deserializer.deserialize_bytes(BytesVisitor)
    }
}

/// MessagePack's reserved timestamp extension type.
const TIMESTAMP_EXT: i8 = -1;

/// `(type, data)` → `{"$timestamp": iso}` for a well-formed timestamp,
/// else `{"$ext": {"type": type, "data": hex}}`.
fn ext_value(kind: i8, data: &[u8]) -> serde_json::Value {
    if kind == TIMESTAMP_EXT
        && let Some(text) = timestamp_text(data)
    {
        return serde_json::json!({"$timestamp": text});
    }
    serde_json::json!({"$ext": {"type": kind, "data": hex(data)}})
}

/// Decode the timestamp extension's three layouts (32-bit seconds; 64-bit
/// 30-bit nanos + 34-bit seconds; 96-bit u32 nanos + i64 seconds) to an
/// RFC 3339 UTC string. Malformed or out-of-range data is `None`, so the
/// caller shows the raw `$ext` instead of guessing.
fn timestamp_text(data: &[u8]) -> Option<String> {
    let (seconds, nanos): (i64, u32) = match data.len() {
        4 => (u32::from_be_bytes(data.try_into().ok()?) as i64, 0),
        8 => {
            let packed = u64::from_be_bytes(data.try_into().ok()?);
            ((packed & 0x3_ffff_ffff) as i64, (packed >> 34) as u32)
        }
        12 => (
            i64::from_be_bytes(data[4..12].try_into().ok()?),
            u32::from_be_bytes(data[0..4].try_into().ok()?),
        ),
        _ => return None,
    };
    if nanos >= 1_000_000_000 {
        return None;
    }
    let when = chrono::DateTime::from_timestamp(seconds, nanos)?;
    Some(when.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true))
}

fn binary_object(hex_text: &str) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    object.insert(
        "$binary".to_string(),
        serde_json::Value::String(hex_text.to_string()),
    );
    serde_json::Value::Object(object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_the_canonical_scalars() {
        // fixstr "hello", positive fixint, negative fixint, uint16, bool, nil.
        assert_eq!(decode(b"\xa5hello").unwrap(), json!("hello"));
        assert_eq!(decode(&[0x2a]).unwrap(), json!(42));
        assert_eq!(decode(&[0xfb]).unwrap(), json!(-5));
        assert_eq!(decode(&[0xcd, 0x12, 0x34]).unwrap(), json!(4660));
        assert_eq!(decode(&[0xc3]).unwrap(), json!(true));
        assert_eq!(decode(&[0xc0]).unwrap(), serde_json::Value::Null);
        // float64 3.5
        assert_eq!(
            decode(&[0xcb, 0x40, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]).unwrap(),
            json!(3.5)
        );
    }

    #[test]
    fn decodes_every_integer_width() {
        assert_eq!(decode(&[0xcc, 0xff]).unwrap(), json!(255)); // uint8
        assert_eq!(decode(&[0xcd, 0xff, 0xff]).unwrap(), json!(65535)); // uint16
        assert_eq!(
            decode(&[0xce, 0xff, 0xff, 0xff, 0xff]).unwrap(),
            json!(4_294_967_295u64)
        ); // uint32
        assert_eq!(
            decode(&[0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).unwrap(),
            json!(u64::MAX)
        ); // uint64
        assert_eq!(decode(&[0xd0, 0x80]).unwrap(), json!(-128)); // int8
        assert_eq!(decode(&[0xd1, 0x80, 0x00]).unwrap(), json!(-32768)); // int16
        assert_eq!(
            decode(&[0xd2, 0x80, 0x00, 0x00, 0x00]).unwrap(),
            json!(i32::MIN)
        ); // int32
        assert_eq!(
            decode(&[0xd3, 0x80, 0, 0, 0, 0, 0, 0, 0]).unwrap(),
            json!(i64::MIN)
        ); // int64
    }

    #[test]
    fn decodes_float32_without_widening_noise() {
        // float32 1.1 (0x3f8ccccd) shows as 1.1, not 1.100000023841858.
        assert_eq!(decode(&[0xca, 0x3f, 0x8c, 0xcc, 0xcd]).unwrap(), json!(1.1));
        // float64 NaN has no JSON number; it stays visible as text.
        assert_eq!(
            decode(&[0xcb, 0x7f, 0xf8, 0, 0, 0, 0, 0, 0]).unwrap(),
            json!("NaN")
        );
    }

    #[test]
    fn decodes_str_and_bin_of_every_width() {
        assert_eq!(decode(&[0xd9, 0x02, b'h', b'i']).unwrap(), json!("hi")); // str8
        assert_eq!(decode(&[0xda, 0x00, 0x01, b'x']).unwrap(), json!("x")); // str16
        assert_eq!(
            decode(&[0xdb, 0x00, 0x00, 0x00, 0x01, b'y']).unwrap(),
            json!("y")
        ); // str32
        assert_eq!(
            decode(&[0xc5, 0x00, 0x01, 0xab]).unwrap(),
            json!({"$binary": "ab"})
        ); // bin16
        assert_eq!(
            decode(&[0xc6, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01]).unwrap(),
            json!({"$binary": "0001"})
        ); // bin32
    }

    #[test]
    fn decodes_array_and_map_16_and_32() {
        assert_eq!(
            decode(&[0xdc, 0x00, 0x02, 0x01, 0x02]).unwrap(),
            json!([1, 2])
        ); // array16
        assert_eq!(
            decode(&[0xdd, 0x00, 0x00, 0x00, 0x01, 0xc3]).unwrap(),
            json!([true])
        ); // array32
        assert_eq!(
            decode(&[0xde, 0x00, 0x01, 0xa1, b'k', 0x07]).unwrap(),
            json!({"k": 7})
        ); // map16
        assert_eq!(
            decode(&[0xdf, 0x00, 0x00, 0x00, 0x01, 0xa1, b'k', 0xc2]).unwrap(),
            json!({"k": false})
        ); // map32
    }

    /// A header claiming 2^32-1 elements over a 5-byte payload must fail as
    /// truncated input, not reserve gigabytes and abort the app.
    #[test]
    fn length_bomb_headers_fail_without_allocating() {
        assert!(decode(&[0xdd, 0xff, 0xff, 0xff, 0xff]).is_err()); // array32
        assert!(decode(&[0xdf, 0xff, 0xff, 0xff, 0xff]).is_err()); // map32
        assert!(decode(&[0xdc, 0xff, 0xff]).is_err()); // array16
        assert!(decode(&[0xde, 0xff, 0xff]).is_err()); // map16
        assert!(decode(&[0xdb, 0xff, 0xff, 0xff, 0xff, b'a']).is_err()); // str32
        assert!(decode(&[0xc6, 0xff, 0xff, 0xff, 0xff, 0x00]).is_err()); // bin32
        assert!(decode(&[0xc9, 0xff, 0xff, 0xff, 0xff, 0x01]).is_err()); // ext32
        // Nested bombs: every level claims 2^32-1 elements.
        let mut nested = Vec::new();
        for _ in 0..64 {
            nested.extend_from_slice(&[0xdd, 0xff, 0xff, 0xff, 0xff]);
        }
        assert!(decode(&nested).is_err());
        let mut nested_maps = Vec::new();
        for _ in 0..64 {
            nested_maps.extend_from_slice(&[0xdf, 0xff, 0xff, 0xff, 0xff, 0xa1, b'k']);
        }
        assert!(decode(&nested_maps).is_err());
    }

    #[test]
    fn bounded_capacity_never_exceeds_the_input_or_the_cap() {
        assert_eq!(bounded_capacity(Some(u32::MAX as usize), 5), 5);
        assert_eq!(bounded_capacity(Some(u32::MAX as usize), 1 << 20), 1024);
        assert_eq!(bounded_capacity(Some(3), 100), 3);
        assert_eq!(bounded_capacity(None, 100), 0);
    }

    #[test]
    fn a_real_array_larger_than_the_prealloc_cap_still_decodes() {
        let mut bytes = vec![0xdc, 0x07, 0xd0]; // array16 of 2000
        bytes.extend(std::iter::repeat_n(0x01, 2000));
        let value = decode(&bytes).unwrap();
        assert_eq!(value.as_array().unwrap().len(), 2000);
    }

    #[test]
    fn truncated_input_is_an_error() {
        assert!(decode(&[]).is_err());
        assert!(decode(&[0xa5, 0x68, 0x65]).is_err()); // fixstr 5, 2 bytes
        assert!(decode(&[0x92, 0x01]).is_err()); // fixarray 2, 1 element
        assert!(decode(&[0x81, 0xa1, b'k']).is_err()); // map value missing
        assert!(decode(&[0xcd, 0x12]).is_err()); // uint16 missing a byte
        assert!(decode(&[0xcb, 0x40, 0x0c]).is_err()); // float64 short
        assert!(decode(&[0xd6, 0xff, 0x00]).is_err()); // fixext4 short
    }

    #[test]
    fn nesting_is_capped() {
        let within: Vec<u8> = std::iter::repeat_n(0x91, MAX_DEPTH - 1)
            .chain([0xc0])
            .collect();
        assert!(decode(&within).is_ok());
        let beyond: Vec<u8> = std::iter::repeat_n(0x91, MAX_DEPTH + 10)
            .chain([0xc0])
            .collect();
        assert!(decode(&beyond).is_err());
    }

    #[test]
    fn decodes_maps_arrays_and_nesting() {
        // {"a": 1, "b": [true, nil]} — the canonical msgpack map fixture.
        let bytes = [0x82, 0xa1, 0x61, 0x01, 0xa1, 0x62, 0x92, 0xc3, 0xc0];
        assert_eq!(decode(&bytes).unwrap(), json!({"a": 1, "b": [true, null]}));
        // fixarray of three ints.
        assert_eq!(decode(&[0x93, 0x01, 0x02, 0x03]).unwrap(), json!([1, 2, 3]));
        // A map with a non-string key stringifies the key.
        let int_key = [0x81, 0x01, 0xa1, 0x78];
        assert_eq!(decode(&int_key).unwrap(), json!({"1": "x"}));
    }

    #[test]
    fn colliding_keys_keep_every_entry() {
        // {1: "a", "1": "b"} would collapse to one "1" key as a JSON object.
        let bytes = [0x82, 0x01, 0xa1, b'a', 0xa1, b'1', 0xa1, b'b'];
        assert_eq!(
            decode(&bytes).unwrap(),
            json!({"$map": [[1, "a"], ["1", "b"]]})
        );
        // A duplicated string key keeps both values too.
        let dup = [0x82, 0xa1, b'k', 0x01, 0xa1, b'k', 0x02];
        assert_eq!(decode(&dup).unwrap(), json!({"$map": [["k", 1], ["k", 2]]}));
    }

    #[test]
    fn binary_payloads_become_visible_objects() {
        // bin8 of three bytes.
        let bytes = [0xc4, 0x03, 0x01, 0x02, 0xff];
        assert_eq!(decode(&bytes).unwrap(), json!({"$binary": "0102ff"}));
    }

    #[test]
    fn extensions_are_visible_and_garbage_fails_the_decode() {
        // ext8 type 5, one byte, and fixext1 type 1.
        assert_eq!(
            decode(&[0xc7, 0x01, 0x05, 0xff]).unwrap(),
            json!({"$ext": {"type": 5, "data": "ff"}})
        );
        assert_eq!(
            decode(&[0x91, 0xd4, 0x01, 0x2a]).unwrap(),
            json!([{"$ext": {"type": 1, "data": "2a"}}])
        );
        // fixext2/8/16 and ext16/ext32.
        assert_eq!(
            decode(&[0xd5, 0x02, 0x01, 0x02]).unwrap(),
            json!({"$ext": {"type": 2, "data": "0102"}})
        );
        assert_eq!(
            decode(&[0xd7, 0x03, 0, 0, 0, 0, 0, 0, 0, 1]).unwrap(),
            json!({"$ext": {"type": 3, "data": "0000000000000001"}})
        );
        let mut fixext16 = vec![0xd8, 0x04];
        fixext16.extend([0xaa; 16]);
        assert_eq!(
            decode(&fixext16).unwrap(),
            json!({"$ext": {"type": 4, "data": "aa".repeat(16)}})
        );
        assert_eq!(
            decode(&[0xc8, 0x00, 0x01, 0x06, 0x0f]).unwrap(),
            json!({"$ext": {"type": 6, "data": "0f"}})
        );
        assert_eq!(
            decode(&[0xc9, 0x00, 0x00, 0x00, 0x01, 0x07, 0x0e]).unwrap(),
            json!({"$ext": {"type": 7, "data": "0e"}})
        );
        // Negative (reserved) types other than the timestamp stay raw.
        assert_eq!(
            decode(&[0xd4, 0xfe, 0x00]).unwrap(),
            json!({"$ext": {"type": -2, "data": "00"}})
        );
        // 0xc1 is never a valid MessagePack marker.
        assert!(decode(&[0xc1]).is_err());
        // Trailing bytes after a complete value are an error, not silently ok.
        assert!(decode(&[0xa1, 0x61, 0x01]).is_err());
    }

    #[test]
    fn timestamp_extension_decodes_to_rfc3339() {
        // timestamp32: 1_700_000_000 seconds.
        let ts32 = [0xd6, 0xff, 0x65, 0x53, 0xf1, 0x00];
        assert_eq!(
            decode(&ts32).unwrap(),
            json!({"$timestamp": "2023-11-14T22:13:20Z"})
        );
        // timestamp64: 500_000_000 ns + 1_700_000_000 s.
        let packed: u64 = (500_000_000u64 << 34) | 1_700_000_000u64;
        let mut ts64 = vec![0xd7, 0xff];
        ts64.extend(packed.to_be_bytes());
        assert_eq!(
            decode(&ts64).unwrap(),
            json!({"$timestamp": "2023-11-14T22:13:20.500Z"})
        );
        // timestamp96: 1 ns before the epoch.
        let mut ts96 = vec![0xc7, 0x0c, 0xff];
        ts96.extend(999_999_999u32.to_be_bytes());
        ts96.extend((-1i64).to_be_bytes());
        assert_eq!(
            decode(&ts96).unwrap(),
            json!({"$timestamp": "1969-12-31T23:59:59.999999999Z"})
        );
        // Nanoseconds out of range: shown raw, not guessed.
        let bad: u64 = (1_000_000_000u64 << 34) | 1;
        let mut bad_ts = vec![0xd7, 0xff];
        bad_ts.extend(bad.to_be_bytes());
        assert_eq!(
            decode(&bad_ts).unwrap(),
            json!({"$ext": {"type": -1, "data": hex(&bad.to_be_bytes())}})
        );
    }

    #[test]
    fn key_text_keeps_strings_and_compacts_the_rest() {
        assert_eq!(key_text(&json!("k")), "k");
        assert_eq!(key_text(&json!(7)), "7");
        assert_eq!(key_text(&json!(true)), "true");
        assert_eq!(key_text(&json!([1])), "[1]");
    }
}
