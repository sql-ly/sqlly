//! Little-endian cursor over a binary request/response body.
//!
//! Every read validates against remaining bytes and reports the field name
//! on failure, so decode errors are self-describing. The error *message
//! strings* are part of the contract — `sqlly-cli`'s tests assert on
//! substrings like `"truncated"`, `"invalid bool"`, `"declared N bytes"`, and
//! `"trailing"`.

use crate::engine_protocol::error::{ProtocolError, ProtocolResult};

/// Safe little-endian cursor over a binary body.
pub struct BinaryReader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> BinaryReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.offset
    }

    /// Byte offset of the cursor into the underlying buffer.
    pub fn offset(&self) -> usize {
        self.offset
    }

    pub fn read_u8(&mut self, field: &str) -> ProtocolResult<u8> {
        if self.remaining() < 1 {
            return Err(ProtocolError::decode(format!(
                "truncated: field '{field}' at offset {}",
                self.offset
            )));
        }
        let v = self.data[self.offset];
        self.offset += 1;
        Ok(v)
    }

    pub fn read_u16_le(&mut self, field: &str) -> ProtocolResult<u16> {
        if self.remaining() < 2 {
            return Err(ProtocolError::decode(format!(
                "truncated: field '{field}' at offset {}",
                self.offset
            )));
        }
        let v = u16::from_le_bytes([self.data[self.offset], self.data[self.offset + 1]]);
        self.offset += 2;
        Ok(v)
    }

    pub fn read_u32_le(&mut self, field: &str) -> ProtocolResult<u32> {
        if self.remaining() < 4 {
            return Err(ProtocolError::decode(format!(
                "truncated: field '{field}' at offset {}",
                self.offset
            )));
        }
        let v = u32::from_le_bytes([
            self.data[self.offset],
            self.data[self.offset + 1],
            self.data[self.offset + 2],
            self.data[self.offset + 3],
        ]);
        self.offset += 4;
        Ok(v)
    }

    pub fn read_i64_le(&mut self, field: &str) -> ProtocolResult<i64> {
        if self.remaining() < 8 {
            return Err(ProtocolError::decode(format!(
                "truncated: field '{field}' at offset {}",
                self.offset
            )));
        }
        let bytes: [u8; 8] = self.data[self.offset..self.offset + 8].try_into().unwrap();
        self.offset += 8;
        Ok(i64::from_le_bytes(bytes))
    }

    pub fn read_f64_le(&mut self, field: &str) -> ProtocolResult<f64> {
        if self.remaining() < 8 {
            return Err(ProtocolError::decode(format!(
                "truncated: field '{field}' at offset {}",
                self.offset
            )));
        }
        let bytes: [u8; 8] = self.data[self.offset..self.offset + 8].try_into().unwrap();
        self.offset += 8;
        Ok(f64::from_le_bytes(bytes))
    }

    pub fn read_optional_u32(&mut self, field: &str) -> ProtocolResult<Option<u32>> {
        let present = self.read_u8(&format!("{field}.present"))?;
        match present {
            0 => Ok(None),
            1 => Ok(Some(self.read_u32_le(field)?)),
            v => Err(ProtocolError::decode(format!(
                "invalid presence byte 0x{v:02x} in field '{field}' (expected 0x00 or 0x01)"
            ))),
        }
    }

    pub fn read_bool(&mut self, field: &str) -> ProtocolResult<bool> {
        match self.read_u8(field)? {
            0 => Ok(false),
            1 => Ok(true),
            v => Err(ProtocolError::decode(format!(
                "invalid bool byte 0x{v:02x} in field '{field}' (expected 0x00 or 0x01)"
            ))),
        }
    }

    pub fn read_string(&mut self, field: &str) -> ProtocolResult<String> {
        let len = self.read_u32_le(&format!("{field}.length"))? as usize;
        if len > self.remaining() {
            return Err(ProtocolError::decode(format!(
                "field '{field}' declared {len} bytes but only {} remain",
                self.remaining()
            )));
        }
        let bytes = &self.data[self.offset..self.offset + len];
        let s = std::str::from_utf8(bytes)
            .map_err(|_| ProtocolError::decode(format!("invalid UTF-8 in field '{field}'")))?;
        self.offset += len;
        Ok(s.to_string())
    }

    pub fn read_optional_string(&mut self, field: &str) -> ProtocolResult<Option<String>> {
        let present = self.read_u8(&format!("{field}.present"))?;
        match present {
            0 => Ok(None),
            1 => Ok(Some(self.read_string(field)?)),
            v => Err(ProtocolError::decode(format!(
                "invalid presence byte 0x{v:02x} in field '{field}' (expected 0x00 or 0x01)"
            ))),
        }
    }

    pub fn read_optional_i64(&mut self, field: &str) -> ProtocolResult<Option<i64>> {
        let present = self.read_u8(&format!("{field}.present"))?;
        match present {
            0 => Ok(None),
            1 => Ok(Some(self.read_i64_le(field)?)),
            v => Err(ProtocolError::decode(format!(
                "invalid presence byte 0x{v:02x} in field '{field}' (expected 0x00 or 0x01)"
            ))),
        }
    }

    /// Optional string list: `[u8 present]` then, when `Some`, `[u32 count]`
    /// followed by length-prefixed UTF-8 strings.
    pub fn read_opt_string_list(&mut self, field: &str) -> ProtocolResult<Option<Vec<String>>> {
        let present = self.read_u8(&format!("{field}.present"))?;
        match present {
            0 => Ok(None),
            1 => {
                let count = self.read_count_with_min(field, 4)?;
                let mut out = Vec::with_capacity(count);
                for i in 0..count {
                    out.push(self.read_string(&format!("{field}[{i}]"))?);
                }
                Ok(Some(out))
            }
            v => Err(ProtocolError::decode(format!(
                "invalid presence byte 0x{v:02x} in field '{field}' (expected 0x00 or 0x01)"
            ))),
        }
    }

    /// Read a `u32` count and validate it does not exceed remaining bytes.
    /// Each element needs at least one byte, so a count larger than `remaining`
    /// proves the frame is corrupt or truncated.
    pub fn read_count(&mut self, field: &str) -> ProtocolResult<usize> {
        let count = self.read_u32_le(field)? as usize;
        if count > self.remaining() {
            return Err(ProtocolError::decode(format!(
                "field '{field}' declared {count} elements but only {} bytes remain",
                self.remaining()
            )));
        }
        Ok(count)
    }

    /// Read a `u32` count and validate that `count * min_bytes_per_element`
    /// does not exceed remaining bytes. This prevents absurd allocations from
    /// corrupt frames where the count is large but each element needs multiple
    /// bytes — [`read_count`](Self::read_count) alone only checks
    /// `count > remaining`.
    pub fn read_count_with_min(
        &mut self,
        field: &str,
        min_bytes_per_element: usize,
    ) -> ProtocolResult<usize> {
        let count = self.read_u32_le(field)? as usize;
        let remaining = self.remaining();
        if min_bytes_per_element > 0 {
            let min_required = count.checked_mul(min_bytes_per_element).ok_or_else(|| {
                ProtocolError::decode(format!(
                    "field '{field}' declared {count} elements which overflows size calculation"
                ))
            })?;
            if min_required > remaining {
                return Err(ProtocolError::decode(format!(
                    "field '{field}' declared {count} elements requiring at least {min_required} bytes, but only {remaining} bytes remain"
                )));
            }
        } else if count > remaining {
            return Err(ProtocolError::decode(format!(
                "field '{field}' declared {count} elements but only {remaining} bytes remain"
            )));
        }
        Ok(count)
    }

    pub fn ensure_at_end(&self) -> ProtocolResult<()> {
        if self.remaining() > 0 {
            return Err(ProtocolError::decode(format!(
                "{} trailing byte(s) after decoded payload",
                self.remaining()
            )));
        }
        Ok(())
    }
}
