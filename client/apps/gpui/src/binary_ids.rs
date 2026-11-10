//! 16-byte binary identifiers shown as text (Sequel Ace #2148): UUIDs in
//! the canonical hyphenated form and ULIDs in Crockford base32, plus the
//! reverse parse so a value typed or copied in that form can filter the
//! binary column it came from.

/// Crockford base32 alphabet used by ULID (no I, L, O, U).
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (lowercase) for exactly 16 bytes,
/// in stored byte order.
pub fn uuid_text(bytes: &[u8]) -> Option<String> {
    let id = uuid::Uuid::from_slice(bytes).ok()?;
    Some(id.hyphenated().to_string())
}

/// The 26-character ULID for exactly 16 bytes (big-endian 128-bit value).
pub fn ulid_text(bytes: &[u8]) -> Option<String> {
    let array: [u8; 16] = bytes.try_into().ok()?;
    let mut value = u128::from_be_bytes(array);
    let mut out = [0u8; 26];
    for slot in out.iter_mut().rev() {
        *slot = CROCKFORD[(value & 0x1F) as usize];
        value >>= 5;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// Uppercase hex with no prefix: the grid's `binary=hex` rendering, used
/// when a value is not 16 bytes long.
pub fn hex_text(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

/// The bytes of a UUID written as 32 hex digits (hyphens and braces
/// optional), case-insensitive.
pub fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let trimmed = text.trim();
    let trimmed = trimmed
        .strip_prefix('{')
        .and_then(|t| t.strip_suffix('}'))
        .unwrap_or(trimmed);
    uuid::Uuid::try_parse(trimmed).ok().map(|id| *id.as_bytes())
}

/// The bytes of a 26-character ULID, case-insensitive. Values whose first
/// character exceeds `7` overflow 128 bits and are rejected.
pub fn parse_ulid(text: &str) -> Option<[u8; 16]> {
    let trimmed = text.trim();
    if trimmed.len() != 26 {
        return None;
    }
    let mut value: u128 = 0;
    for (i, ch) in trimmed.bytes().enumerate() {
        let upper = ch.to_ascii_uppercase();
        let digit = CROCKFORD.iter().position(|&c| c == upper)? as u128;
        if i == 0 && digit > 7 {
            return None;
        }
        value = (value << 5) | digit;
    }
    Some(value.to_be_bytes())
}

/// A UUID or ULID typed as text, as the bytes it stands for.
pub fn parse_binary_id(text: &str) -> Option<[u8; 16]> {
    parse_uuid(text).or_else(|| parse_ulid(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BYTES: [u8; 16] = [
        0x01, 0x7F, 0x22, 0xE2, 0x79, 0xB0, 0x7C, 0xC3, 0x98, 0xC4, 0xDC, 0x0C, 0x0C, 0x07, 0x39,
        0x8F,
    ];

    #[test]
    fn uuid_is_canonical_lowercase_hyphenated() {
        assert_eq!(
            uuid_text(&BYTES).unwrap(),
            "017f22e2-79b0-7cc3-98c4-dc0c0c07398f"
        );
        assert_eq!(
            uuid_text(&[0u8; 16]).unwrap(),
            "00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn ulid_matches_the_spec_example() {
        // The ULID spec's binary example: 01ARZ3NDEKTSV4RRFFQ69G5FAV.
        let bytes = parse_ulid("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        assert_eq!(ulid_text(&bytes).unwrap(), "01ARZ3NDEKTSV4RRFFQ69G5FAV");
        assert_eq!(ulid_text(&BYTES).unwrap(), "01FWHE4YDGFK1SHH6W1G60EECF");
        assert_eq!(ulid_text(&[0u8; 16]).unwrap(), "0".repeat(26));
        assert_eq!(
            ulid_text(&[0xFF; 16]).unwrap(),
            "7ZZZZZZZZZZZZZZZZZZZZZZZZZ"
        );
    }

    #[test]
    fn non_sixteen_byte_values_have_no_id_form() {
        assert_eq!(uuid_text(&[1, 2, 3]), None);
        assert_eq!(ulid_text(&[0u8; 17]), None);
        assert_eq!(uuid_text(&[]), None);
        assert_eq!(hex_text(&[0xDE, 0xAD, 0x01]), "DEAD01");
    }

    #[test]
    fn parsing_round_trips_both_forms() {
        let uuid = uuid_text(&BYTES).unwrap();
        assert_eq!(parse_uuid(&uuid), Some(BYTES));
        assert_eq!(parse_uuid(&uuid.to_uppercase()), Some(BYTES));
        assert_eq!(parse_uuid(&format!("{{{uuid}}}")), Some(BYTES));
        assert_eq!(parse_uuid(&uuid.replace('-', "")), Some(BYTES));
        let ulid = ulid_text(&BYTES).unwrap();
        assert_eq!(parse_ulid(&ulid), Some(BYTES));
        assert_eq!(parse_ulid(&ulid.to_lowercase()), Some(BYTES));
        assert_eq!(parse_binary_id(&ulid), Some(BYTES));
        assert_eq!(parse_binary_id(&uuid), Some(BYTES));
    }

    #[test]
    fn parsing_rejects_other_text() {
        assert_eq!(parse_binary_id("hello"), None);
        assert_eq!(parse_binary_id("017f22e2-79b0-7cc3-98c4"), None);
        // Overflows 128 bits.
        assert_eq!(parse_ulid("8ZZZZZZZZZZZZZZZZZZZZZZZZZ"), None);
        // `U` is not in the alphabet.
        assert_eq!(parse_ulid("01ARZ3NDEKTSV4RRFFQ69G5FAU"), None);
    }
}
