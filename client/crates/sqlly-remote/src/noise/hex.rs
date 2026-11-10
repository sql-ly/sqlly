//! Minimal, dependency-free lowercase hex encode/decode. Kept internal so the
//! crate does not pull in a hex dependency just to serialize a 32-byte key.

use crate::noise::NoiseError;

pub(crate) fn encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn decode(s: &str) -> Result<Vec<u8>, NoiseError> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return Err(NoiseError::InvalidKeyEncoding);
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = nibble(bytes[i])?;
        let lo = nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn nibble(c: u8) -> Result<u8, NoiseError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(NoiseError::InvalidKeyEncoding),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let data = [0u8, 1, 15, 16, 255, 128, 42];
        let s = encode(&data);
        assert_eq!(s, "00010f10ff802a");
        assert_eq!(decode(&s).unwrap(), data);
    }

    #[test]
    fn rejects_odd_and_non_hex() {
        assert!(decode("abc").is_err());
        assert!(decode("zz").is_err());
    }
}
