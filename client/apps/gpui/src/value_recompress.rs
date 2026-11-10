//! Edit-and-re-compress for compressed values: find the exact packaging a
//! value was read in (gzip, zlib, raw deflate, zstd, LZ4 frame or block,
//! MySQL `COMPRESS()`), hand the decompressed text to an editor, and pack
//! the edited text back the same way.
//!
//! A value whose packaging cannot be pinned down exactly — the decode would
//! succeed but re-packing could not reproduce the same framing — is refused:
//! writing it back in a different framing would break whatever reads it.

use crate::structured_data::{ValueFormat, codecs};

/// The precise packaging of a compressed value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Framing {
    Gzip,
    /// RFC 1950: a zlib header and Adler-32 trailer around deflate.
    Zlib,
    /// A bare deflate stream (RFC 1951).
    RawDeflate,
    Zstd,
    /// An LZ4 frame (magic `04 22 4d 18`).
    Lz4Frame,
    /// An LZ4 block with its uncompressed size prepended (4 bytes LE).
    Lz4Block,
    /// MySQL `COMPRESS()`: a 4-byte LE uncompressed length, then zlib.
    MySqlCompress,
}

impl Framing {
    /// Short name for buttons and notes.
    pub fn label(self) -> &'static str {
        match self {
            Self::Gzip => "gzip",
            Self::Zlib => "zlib",
            Self::RawDeflate => "deflate",
            Self::Zstd => "zstd",
            Self::Lz4Frame => "LZ4",
            Self::Lz4Block => "LZ4 block",
            Self::MySqlCompress => "MySQL COMPRESS",
        }
    }

    /// Unpack `bytes` in this framing.
    pub fn decode(self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Self::Gzip => codecs::gunzip(bytes),
            Self::Zlib => {
                if !codecs::looks_like_zlib(bytes) {
                    return Err(unknown_framing());
                }
                codecs::deflate_decode(bytes)
            }
            Self::RawDeflate => {
                use std::io::Read as _;
                let mut out = Vec::new();
                flate2::read::DeflateDecoder::new(bytes)
                    .take(codecs::MAX_DECODED_BYTES as u64 + 1)
                    .read_to_end(&mut out)
                    .map_err(|error| failed(self, &error.to_string()))?;
                if out.len() > codecs::MAX_DECODED_BYTES {
                    return Err(failed(self, &codecs::MAX_DECODED_BYTES.to_string()));
                }
                Ok(out)
            }
            Self::Zstd => codecs::zstd_decode(bytes),
            Self::Lz4Frame => {
                if !codecs::looks_like_lz4(bytes) {
                    return Err(unknown_framing());
                }
                codecs::lz4_decode(bytes)
            }
            Self::Lz4Block => {
                if codecs::looks_like_lz4(bytes) {
                    return Err(unknown_framing());
                }
                codecs::lz4_decode(bytes)
            }
            Self::MySqlCompress => codecs::mysql_compress_decode(bytes),
        }
    }

    /// Pack `plain` in this framing.
    pub fn encode(self, plain: &[u8]) -> Result<Vec<u8>, String> {
        use std::io::Write as _;
        let io = |error: std::io::Error| failed(self, &error.to_string());
        match self {
            Self::Gzip => {
                let mut encoder =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(plain).map_err(io)?;
                encoder.finish().map_err(io)
            }
            Self::Zlib => zlib(plain).map_err(io),
            Self::RawDeflate => {
                let mut encoder =
                    flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(plain).map_err(io)?;
                encoder.finish().map_err(io)
            }
            #[cfg(not(target_arch = "wasm32"))]
            Self::Zstd => zstd::stream::encode_all(plain, 3).map_err(io),
            #[cfg(target_arch = "wasm32")]
            Self::Zstd => Err(crate::localization::text(
                "results.structured_data.zstd_browser_unavailable",
            )),
            Self::Lz4Frame => {
                let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
                encoder.write_all(plain).map_err(io)?;
                encoder
                    .finish()
                    .map_err(|error| failed(self, &error.to_string()))
            }
            Self::Lz4Block => Ok(lz4_flex::block::compress_prepend_size(plain)),
            Self::MySqlCompress => {
                // MySQL stores an empty string as empty, and appends a `.`
                // when the stream ends in a space so CHAR trimming cannot
                // eat it.
                if plain.is_empty() {
                    return Ok(Vec::new());
                }
                let length = u32::try_from(plain.len())
                    .map_err(|_| failed(self, &plain.len().to_string()))?;
                let mut out = length.to_le_bytes().to_vec();
                out.extend(zlib(plain).map_err(io)?);
                if out.last() == Some(&b' ') {
                    out.push(b'.');
                }
                Ok(out)
            }
        }
    }
}

fn zlib(plain: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::Write as _;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(plain)?;
    encoder.finish()
}

fn unknown_framing() -> String {
    crate::localization::text("results.structured_data.recompress_unknown_framing")
}

fn failed(framing: Framing, error: &str) -> String {
    crate::localization::format(
        "results.structured_data.recompress_failed",
        &[("format_name", framing.label()), ("error", error)],
    )
}

/// The exact framing of `payload` when it was decoded as `format` (a
/// compressed format the viewer picked or Auto found). `None` for formats
/// that are not compression, and for payloads that do not decode in the
/// framing their bytes announce.
pub fn framing_for(payload: &[u8], format: ValueFormat) -> Option<Framing> {
    let framing = match format {
        ValueFormat::Gzip => Framing::Gzip,
        ValueFormat::Zstd => Framing::Zstd,
        ValueFormat::MySqlCompress => Framing::MySqlCompress,
        ValueFormat::Lz4 if codecs::looks_like_lz4(payload) => Framing::Lz4Frame,
        ValueFormat::Lz4 => Framing::Lz4Block,
        ValueFormat::Deflate if codecs::looks_like_zlib(payload) => Framing::Zlib,
        ValueFormat::Deflate => Framing::RawDeflate,
        _ => return None,
    };
    if payload.is_empty() {
        return None;
    }
    framing.decode(payload).ok().map(|_| framing)
}

/// A compressed value opened for editing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableValue {
    pub framing: Framing,
    /// The decompressed text the editor starts from.
    pub text: String,
}

/// Open `payload` (decoded as `format`) for text editing: its exact framing
/// and the decompressed text. `Err` says why it cannot be edited — the
/// framing is unknown, or what is inside is not text.
pub fn editable(payload: &[u8], format: ValueFormat) -> Result<EditableValue, String> {
    let framing = framing_for(payload, format).ok_or_else(unknown_framing)?;
    let plain = framing.decode(payload)?;
    let text = String::from_utf8(plain)
        .map_err(|_| crate::localization::text("results.structured_data.recompress_not_text"))?;
    Ok(EditableValue { framing, text })
}

/// Pack edited text back in `framing`, and check the result decodes to the
/// same text before anything is written.
pub fn recompress(framing: Framing, text: &str) -> Result<Vec<u8>, String> {
    let packed = framing.encode(text.as_bytes())?;
    if !packed.is_empty() && framing.decode(&packed)? != text.as_bytes() {
        return Err(failed(
            framing,
            &crate::localization::text("results.structured_data.recompress_verify_failed"),
        ));
    }
    Ok(packed)
}

/// The compressed format a decoded view reports, when there is one: the
/// explicit pick, or what Auto found.
pub fn compressed_format(format: Option<ValueFormat>) -> Option<ValueFormat> {
    format.filter(|format| format.group() == crate::structured_data::FormatGroup::Compressed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = "{\"name\":\"api\",\"replicas\":3}";
    const EDITED: &str = "{\"name\":\"api\",\"replicas\":5,\"note\":\"ünïcode \"}";

    fn every_framing() -> Vec<(Framing, ValueFormat)> {
        let mut all = vec![
            (Framing::Gzip, ValueFormat::Gzip),
            (Framing::Zlib, ValueFormat::Deflate),
            (Framing::RawDeflate, ValueFormat::Deflate),
            (Framing::Lz4Frame, ValueFormat::Lz4),
            (Framing::Lz4Block, ValueFormat::Lz4),
            (Framing::MySqlCompress, ValueFormat::MySqlCompress),
        ];
        if cfg!(not(target_arch = "wasm32")) {
            all.push((Framing::Zstd, ValueFormat::Zstd));
        }
        all
    }

    #[test]
    fn every_framing_round_trips_decode_edit_encode_decode() {
        for (framing, format) in every_framing() {
            let stored = framing.encode(ORIGINAL.as_bytes()).unwrap();
            let opened = editable(&stored, format)
                .unwrap_or_else(|error| panic!("{framing:?} did not open: {error}"));
            assert_eq!(opened.framing, framing, "framing for {format:?}");
            assert_eq!(opened.text, ORIGINAL);

            let rewritten = recompress(opened.framing, EDITED).unwrap();
            assert_eq!(
                framing_for(&rewritten, format),
                Some(framing),
                "{framing:?} keeps its framing after the edit"
            );
            assert_eq!(framing.decode(&rewritten).unwrap(), EDITED.as_bytes());
            // And through the viewer's own decoder for that format.
            let rendered = crate::structured_data::render_payload(&rewritten, format);
            assert_eq!(rendered.decoded, Some(format), "{framing:?} renders");
        }
    }

    #[test]
    fn mysql_compress_keeps_the_length_prefix_and_empty_stays_empty() {
        let stored = Framing::MySqlCompress.encode(b"hello").unwrap();
        assert_eq!(&stored[..4], &5u32.to_le_bytes());
        assert!(codecs::looks_like_zlib(&stored[4..]));
        assert_eq!(
            Framing::MySqlCompress.encode(b"").unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(
            codecs::mysql_compress_decode(&recompress(Framing::MySqlCompress, "x y ").unwrap())
                .unwrap(),
            b"x y "
        );
    }

    #[test]
    fn unknown_framing_is_refused() {
        // Not compressed at all.
        assert!(editable(b"plain text", ValueFormat::Gzip).is_err());
        assert!(editable(b"plain text", ValueFormat::Deflate).is_err());
        // A format that is not compression.
        let gz = Framing::Gzip.encode(ORIGINAL.as_bytes()).unwrap();
        assert!(editable(&gz, ValueFormat::Base64).is_err());
        assert!(editable(&gz, ValueFormat::Auto).is_err());
        assert_eq!(framing_for(&gz, ValueFormat::Utf8), None);
        // gzip bytes read as zstd: the framing does not match.
        assert_eq!(framing_for(&gz, ValueFormat::Zstd), None);
        // Truncated gzip: decode fails, so no framing.
        assert_eq!(framing_for(&gz[..gz.len() - 3], ValueFormat::Gzip), None);
    }

    #[test]
    fn binary_contents_are_not_opened_as_text() {
        let stored = Framing::Gzip.encode(&[0xff, 0xfe, 0x00, 0x81]).unwrap();
        assert!(editable(&stored, ValueFormat::Gzip).is_err());
    }
}
