//! Little-endian builder for binary request/response bodies.
//!
//! This is the encode-side counterpart of [`crate::engine_protocol::reader::BinaryReader`] and
//! the single source of truth for the `.u16_le(..).string(..).done()` builder
//! pattern that was previously copy-pasted into both `apps/cli/src/engine.rs`
//! tests (`BinaryTestWriter`) and `apps/cli/tests/engine_io.rs`
//! (`BinaryFrameWriter`). Both now alias this type.

/// Accumulates little-endian encoded fields into a body buffer.
#[derive(Debug, Default, Clone)]
pub struct BinaryWriter {
    buf: Vec<u8>,
}

impl BinaryWriter {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn u8(mut self, v: u8) -> Self {
        self.buf.push(v);
        self
    }

    pub fn u16_le(mut self, v: u16) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn u32_le(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn i64_le(mut self, v: i64) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn f64_le(mut self, v: f64) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub fn bool(self, v: bool) -> Self {
        self.u8(if v { 1 } else { 0 })
    }

    /// `[u32 len][utf8 bytes]`.
    pub fn string(mut self, s: &str) -> Self {
        let bytes = s.as_bytes();
        self.buf
            .extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(bytes);
        self
    }

    /// `[u8 present]` then, when `Some`, a length-prefixed string.
    pub fn opt_string(self, s: Option<&str>) -> Self {
        match s {
            Some(v) => self.u8(1).string(v),
            None => self.u8(0),
        }
    }

    /// `[u8 present]` then, when `Some`, an `i64` little-endian.
    pub fn opt_i64(self, v: Option<i64>) -> Self {
        match v {
            Some(n) => self.u8(1).i64_le(n),
            None => self.u8(0),
        }
    }

    /// `[u8 present]` then, when `Some`, a `u32` little-endian.
    pub fn opt_u32(self, v: Option<u32>) -> Self {
        match v {
            Some(n) => self.u8(1).u32_le(n),
            None => self.u8(0),
        }
    }

    /// Optional string list: `[u8 present]` then, when `Some`, `[u32 count]`
    /// followed by length-prefixed UTF-8 strings.
    pub fn opt_string_list(self, items: Option<&[String]>) -> Self {
        match items {
            None => self.u8(0),
            Some(list) => {
                let mut w = self.u8(1).u32_le(list.len() as u32);
                for item in list {
                    w = w.string(item);
                }
                w
            }
        }
    }

    /// Append raw bytes verbatim.
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self.buf.extend_from_slice(b);
        self
    }

    /// Consume the builder and return the accumulated body.
    pub fn done(self) -> Vec<u8> {
        self.buf
    }
}
