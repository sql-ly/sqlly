//! Minimal ZIP (STORE) writer: no compression, no external dependencies,
//! a fixed timestamp so archives are byte-for-byte reproducible.

const CRC32_TABLE: [u32; 256] = build_crc32_table();

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut value = i as u32;
        let mut j = 0;
        while j < 8 {
            if value & 1 == 1 {
                value = 0xEDB8_8320 ^ (value >> 1);
            } else {
                value >>= 1;
            }
            j += 1;
        }
        table[i] = value;
        i += 1;
    }
    table
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        let index = ((crc ^ byte as u32) & 0xFF) as usize;
        crc = CRC32_TABLE[index] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

struct ZipEntry {
    name_bytes: Vec<u8>,
    crc: u32,
    size: u32,
    offset: u32,
}

/// DOS date 1980-01-01, time 00:00:00 — a fixed, valid timestamp.
const DOS_DATE: u16 = 0x0021;
const DOS_TIME: u16 = 0x0000;

pub fn zip_archive(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut metas: Vec<ZipEntry> = entries
        .iter()
        .map(|(name, data)| ZipEntry {
            name_bytes: name.as_bytes().to_vec(),
            crc: crc32(data),
            size: data.len() as u32,
            offset: 0,
        })
        .collect();

    let mut local: Vec<u8> = Vec::new();
    for (meta, (_, data)) in metas.iter_mut().zip(entries) {
        meta.offset = local.len() as u32;
        let name_bytes = &meta.name_bytes;
        let crc = meta.crc;
        let size = meta.size;

        local.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        local.extend_from_slice(&20u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes()); // STORE
        local.extend_from_slice(&DOS_TIME.to_le_bytes());
        local.extend_from_slice(&DOS_DATE.to_le_bytes());
        local.extend_from_slice(&crc.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&size.to_le_bytes());
        local.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        local.extend_from_slice(&0u16.to_le_bytes());
        local.extend_from_slice(name_bytes);
        local.extend_from_slice(data);
    }

    let mut central: Vec<u8> = Vec::new();
    for meta in metas.iter() {
        let name_bytes = &meta.name_bytes;
        let crc = meta.crc;
        let size = meta.size;
        let offset = meta.offset;

        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // STORE
        central.extend_from_slice(&DOS_TIME.to_le_bytes());
        central.extend_from_slice(&DOS_DATE.to_le_bytes());
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name_bytes);
    }

    let mut archive: Vec<u8> = Vec::with_capacity(local.len() + central.len() + 22);
    let central_offset = local.len() as u32;
    archive.extend_from_slice(&local);
    archive.extend_from_slice(&central);
    archive.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&(metas.len() as u16).to_le_bytes());
    archive.extend_from_slice(&(metas.len() as u16).to_le_bytes());
    archive.extend_from_slice(&(central.len() as u32).to_le_bytes());
    archive.extend_from_slice(&central_offset.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_known_values() {
        // CRC32 of "123456789" is 0xCBF43926
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        // CRC32 of empty data
        assert_eq!(crc32(b""), 0x0000_0000);
    }

    #[test]
    fn zip_archive_basic() {
        let entries = vec![
            ("a.txt".to_string(), b"hello".to_vec()),
            ("b.txt".to_string(), b"world".to_vec()),
        ];
        let archive = zip_archive(&entries);
        // Starts with local file header
        assert!(archive.starts_with(&0x0403_4b50u32.to_le_bytes()));
        // Ends with end of central directory record
        let eocd = &archive[archive.len() - 22..];
        assert_eq!(&eocd[0..4], &0x0605_4b50u32.to_le_bytes());
        // Entry count in EOCD
        let count = u16::from_le_bytes([eocd[10], eocd[11]]);
        assert_eq!(count, 2);
    }

    #[test]
    fn zip_archive_contains_file_data() {
        let entries = vec![("test.txt".to_string(), b"hello world".to_vec())];
        let archive = zip_archive(&entries);
        let text = String::from_utf8_lossy(&archive);
        assert!(text.contains("hello world"));
        assert!(text.contains("test.txt"));
    }
}
