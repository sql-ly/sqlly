use std::io::Read;

pub const CHUNK_BYTES: usize = 256 * 1024;

pub struct ChunkReader<R: Read> {
    inner: R,
    carry: Vec<u8>,
    first: bool,
    pub bytes_read: u64,
}

impl<R: Read> ChunkReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            carry: Vec::new(),
            first: true,
            bytes_read: 0,
        }
    }

    pub fn next_chunk(&mut self) -> std::io::Result<Option<String>> {
        let mut buf = vec![0u8; CHUNK_BYTES];
        loop {
            if !self.carry.is_empty() {
                return self.carry_to_chunk();
            }
            let n = self.inner.read(&mut buf)?;
            if n == 0 {
                return Ok(None);
            }
            self.bytes_read += n as u64;
            let mut data = buf[..n].to_vec();
            if self.first {
                self.first = false;
                if data.starts_with(&[0xEF, 0xBB, 0xBF]) {
                    data.drain(..3);
                } else if data.starts_with(&[0xFF, 0xFE]) || data.starts_with(&[0xFE, 0xFF]) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "File is UTF-16 encoded (BOM found); save it as UTF-8 and try again",
                    ));
                }
            }
            if data.is_empty() {
                continue;
            }
            self.carry = data;
            return self.carry_to_chunk();
        }
    }

    fn carry_to_chunk(&mut self) -> std::io::Result<Option<String>> {
        match std::str::from_utf8(&self.carry) {
            Ok(all) => {
                let text = all.to_string();
                self.carry.clear();
                Ok(Some(text))
            }
            Err(err) => {
                let valid = err.valid_up_to();
                if err.error_len().is_some() {
                    let offset = self.bytes_read - self.carry.len() as u64 + valid as u64;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid UTF-8 at byte offset {offset}"),
                    ));
                }
                let (chunk, rest) = self.carry.split_at(valid);
                let text = std::str::from_utf8(chunk)
                    .map(|s| s.to_string())
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                self.carry = rest.to_vec();
                Ok(Some(text))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn utf8_bom_stripped() {
        let data = b"\xEF\xBB\xBFselect 1;";
        let mut reader = ChunkReader::new(Cursor::new(data.as_slice()));
        let chunk = reader.next_chunk().unwrap().unwrap();
        assert_eq!(chunk, "select 1;");
        assert!(reader.next_chunk().unwrap().is_none());
    }

    #[test]
    fn utf16_bom_rejected() {
        let data = b"\xFF\xFE\x00\x00";
        let mut reader = ChunkReader::new(Cursor::new(data.as_slice()));
        let err = reader.next_chunk().unwrap_err();
        assert!(err.to_string().contains("UTF-16"));
    }

    #[test]
    fn multibyte_char_straddling_boundary() {
        let text = "ä".repeat(CHUNK_BYTES / 2);
        let mut bytes = text.into_bytes();
        bytes.extend_from_slice("end".as_bytes());
        let mut reader = ChunkReader::new(Cursor::new(bytes));
        while reader.next_chunk().unwrap().is_some() {}
        assert!(reader.carry.is_empty());
    }

    #[test]
    fn invalid_utf8_reports_byte_offset() {
        let mut data = b"ok ".to_vec();
        data.push(0xFF);
        let mut reader = ChunkReader::new(Cursor::new(data));
        let err = reader.next_chunk().unwrap_err();
        assert!(err.to_string().contains("byte offset"));
    }

    #[test]
    fn empty_file_returns_none_immediately() {
        let mut reader = ChunkReader::new(Cursor::new(Vec::<u8>::new()));
        assert!(reader.next_chunk().unwrap().is_none());
    }
}
