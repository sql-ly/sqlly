//! Decode import file bytes to UTF-8 text with a visible encoding label.

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::Encoding;

/// At most this many bytes are fed to the charset detector (speed).
pub const DETECTOR_BYTE_CAP: usize = 1024 * 1024;

pub struct DecodedText {
    pub text: String,
    pub encoding_label: &'static str,
    pub had_bom: bool,
    pub lossy: bool,
}

/// BOM first (UTF-8, UTF-16 LE/BE via [`Encoding::for_bom`]), else chardetng
/// guess, else UTF-8. Always returns text; [`DecodedText::lossy`] is true when
/// replacement characters were emitted.
pub fn decode_import_bytes(bytes: &[u8]) -> DecodedText {
    if let Some((encoding, bom_len)) = Encoding::for_bom(bytes) {
        let (decoded, lossy) = encoding.decode_without_bom_handling(&bytes[bom_len..]);
        return DecodedText {
            text: decoded.into_owned(),
            encoding_label: encoding.name(),
            had_bom: true,
            lossy,
        };
    }

    let detect_len = bytes.len().min(DETECTOR_BYTE_CAP);
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    detector.feed(&bytes[..detect_len], true);
    let encoding = detector.guess(None, Utf8Detection::Allow);
    let (decoded, lossy) = encoding.decode_without_bom_handling(bytes);
    DecodedText {
        text: decoded.into_owned(),
        encoding_label: encoding.name(),
        had_bom: false,
        lossy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_utf8_is_stripped_and_flagged() {
        let bytes = b"\xEF\xBB\xBFid,name\n1,Ada";
        let decoded = decode_import_bytes(bytes);
        assert!(decoded.had_bom);
        assert_eq!(decoded.encoding_label, "UTF-8");
        assert_eq!(decoded.text, "id,name\n1,Ada");
        assert!(!decoded.lossy);
    }

    #[test]
    fn utf16le_bom_decodes_to_expected_text() {
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend_from_slice(&[0x48, 0x00, 0x69, 0x00]); // "Hi"
        let decoded = decode_import_bytes(&bytes);
        assert!(decoded.had_bom);
        assert_eq!(decoded.text, "Hi");
        assert!(!decoded.lossy);
    }

    #[test]
    fn gbk_bytes_decode_with_gbk_label() {
        // "名称" in GBK
        let bytes = [0xC3, 0xFB, 0xB3, 0xC6];
        let decoded = decode_import_bytes(&bytes);
        assert_eq!(decoded.encoding_label, "GBK");
        assert_eq!(decoded.text, "名称");
        assert!(!decoded.lossy);
    }

    #[test]
    fn plain_ascii_is_utf8_without_loss() {
        let decoded = decode_import_bytes(b"id\n1");
        assert!(!decoded.had_bom);
        assert_eq!(decoded.encoding_label, "UTF-8");
        assert_eq!(decoded.text, "id\n1");
        assert!(!decoded.lossy);
    }

    #[test]
    fn truncated_multibyte_tail_is_lossy() {
        let bytes = b"\xEF\xBB\xBF\xe4\xb8".to_vec();
        let decoded = decode_import_bytes(&bytes);
        assert!(decoded.lossy);
        assert!(decoded.text.contains('\u{FFFD}'));
    }
}
