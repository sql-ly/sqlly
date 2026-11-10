//! Spill-to-disk, windowed row store.
//!
//! The result grid only ever shows one page of rows at a time, but the previous
//! pipeline kept *every* row resident in memory on both the CLI boundary and
//! the macOS side, so peak memory was O(total rows). For a result set of tens
//! of millions of rows that is fatal.
//!
//! [`SpillWriter`] streams rows to an on-disk append-log as they arrive.
//! [`SpillReader`] pages rows back with two seeks and no parsing, so the
//! working set is O(window) — the page size — not the total. Export reads the
//! full backing store by paging through it, so the full result is still
//! available without ever materializing it all at once.
//!
//! # On-disk format (version 2)
//!
//! Typed binary — no JSON, no per-cell text parsing:
//!
//! ```text
//! data file (`<path>`):
//!   magic  b"SQSP", u32 LE version
//!   per row record:
//!     u32 LE      column count n
//!     n × u32 LE  byte length of each encoded cell
//!     n encoded cells, concatenated (no separators)
//!
//! encoded cell: 1 tag byte + body
//!   0 Null    (empty body)          3 F64    (8 bytes LE)
//!   1 Bool    (1 byte, 0/1)         4 String (UTF-8 bytes)
//!   2 I64     (8 bytes LE)          5 Bytes  (raw bytes)
//!
//! index file (`<path>.idx`):
//!   magic  b"SQSX", u32 LE version
//!   row i's data-file offset is the u64 LE at byte 8 + i*8
//! ```
//!
//! To read row `i`: seek the index to `8 + i*8`, read the u64 offset, seek the
//! data file there, read the header, then read exactly the cell bytes. Row
//! records are self-describing, so a reader can rebuild a missing sidecar by
//! walking headers once.
//!
//! The reader keeps **no** in-memory offset table — the index itself is paged
//! (a page read pulls its offset window in one I/O). That keeps reader memory
//! O(page) even for billions of rows, where an in-memory `Vec<u64>` index
//! would be gigabytes.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::event::CellValue;

const DATA_MAGIC: &[u8; 4] = b"SQSP";
const INDEX_MAGIC: &[u8; 4] = b"SQSX";
const FORMAT_VERSION: u32 = 2;
/// Byte length of the magic + version header at the start of both files.
const HEADER_LEN: u64 = 8;

/// Upper bound on columns per row, to fail fast on a corrupt/garbage header
/// instead of attempting a huge allocation.
const MAX_COLUMNS: u32 = 1 << 20;

const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_I64: u8 = 2;
const TAG_F64: u8 = 3;
const TAG_STRING: u8 = 4;
const TAG_BYTES: u8 = 5;

/// Error surface for the spill store.
#[derive(Debug)]
pub enum SpillError {
    Io(std::io::Error),
    Serialize(String),
    Corrupt(String),
}

impl std::fmt::Display for SpillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpillError::Io(e) => write!(f, "spill io error: {e}"),
            SpillError::Serialize(e) => write!(f, "spill serialize error: {e}"),
            SpillError::Corrupt(e) => write!(f, "spill store corrupt: {e}"),
        }
    }
}

impl std::error::Error for SpillError {}

impl From<std::io::Error> for SpillError {
    fn from(e: std::io::Error) -> Self {
        SpillError::Io(e)
    }
}

fn encoded_cell_len(cell: &CellValue) -> usize {
    1 + match cell {
        CellValue::Null => 0,
        CellValue::Bool(_) => 1,
        CellValue::I64(_) | CellValue::F64(_) => 8,
        CellValue::String(s) => s.len(),
        CellValue::Bytes(b) => b.len(),
    }
}

fn write_cell(out: &mut impl Write, cell: &CellValue) -> std::io::Result<()> {
    match cell {
        CellValue::Null => out.write_all(&[TAG_NULL]),
        CellValue::Bool(b) => out.write_all(&[TAG_BOOL, u8::from(*b)]),
        CellValue::I64(v) => {
            out.write_all(&[TAG_I64])?;
            out.write_all(&v.to_le_bytes())
        }
        CellValue::F64(v) => {
            out.write_all(&[TAG_F64])?;
            out.write_all(&v.to_le_bytes())
        }
        CellValue::String(s) => {
            out.write_all(&[TAG_STRING])?;
            out.write_all(s.as_bytes())
        }
        CellValue::Bytes(b) => {
            out.write_all(&[TAG_BYTES])?;
            out.write_all(b)
        }
    }
}

fn decode_cell(payload: &[u8]) -> Result<CellValue, SpillError> {
    let (tag, body) = payload
        .split_first()
        .ok_or_else(|| SpillError::Corrupt("empty cell payload".into()))?;
    let fixed = |want: usize| -> Result<&[u8], SpillError> {
        (body.len() == want)
            .then_some(body)
            .ok_or_else(|| SpillError::Corrupt(format!("cell body {} != {want}", body.len())))
    };
    match *tag {
        TAG_NULL => Ok(CellValue::Null),
        TAG_BOOL => Ok(CellValue::Bool(fixed(1)?[0] != 0)),
        TAG_I64 => Ok(CellValue::I64(i64::from_le_bytes(
            fixed(8)?.try_into().expect("checked length"),
        ))),
        TAG_F64 => Ok(CellValue::F64(f64::from_le_bytes(
            fixed(8)?.try_into().expect("checked length"),
        ))),
        TAG_STRING => Ok(CellValue::String(
            String::from_utf8(body.to_vec())
                .map_err(|e| SpillError::Corrupt(format!("cell not utf-8: {e}")))?,
        )),
        TAG_BYTES => Ok(CellValue::Bytes(body.to_vec())),
        other => Err(SpillError::Corrupt(format!("unknown cell tag {other}"))),
    }
}

fn write_file_header(out: &mut impl Write, magic: &[u8; 4]) -> std::io::Result<()> {
    out.write_all(magic)?;
    out.write_all(&FORMAT_VERSION.to_le_bytes())
}

fn check_file_header(file: &mut impl Read, magic: &[u8; 4], what: &str) -> Result<(), SpillError> {
    let mut header = [0u8; 8];
    file.read_exact(&mut header)
        .map_err(|_| SpillError::Corrupt(format!("{what} shorter than its header")))?;
    if &header[..4] != magic {
        return Err(SpillError::Corrupt(format!("{what} has wrong magic")));
    }
    let version = u32::from_le_bytes(header[4..8].try_into().expect("checked length"));
    if version != FORMAT_VERSION {
        return Err(SpillError::Corrupt(format!(
            "{what} version {version}, expected {FORMAT_VERSION}"
        )));
    }
    Ok(())
}

/// Reads one row header (u32 column count + per-column u32 lengths) from the
/// current position. Returns the cell lengths and the total record length
/// (header + payloads) — how far the cursor advances to skip the row.
// `as_chunks` (clippy 1.98's suggestion) needs Rust 1.88; the workspace MSRV
// is 1.85, so `chunks_exact` stays until the MSRV moves.
#[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
fn read_row_header(reader: &mut BufReader<File>) -> Result<(Vec<u32>, u64), SpillError> {
    let mut count_buf = [0u8; 4];
    reader.read_exact(&mut count_buf)?;
    let n = u32::from_le_bytes(count_buf);
    if n > MAX_COLUMNS {
        return Err(SpillError::Corrupt(format!(
            "row claims {n} columns (max {MAX_COLUMNS})"
        )));
    }
    let mut len_bytes = vec![0u8; 4 * n as usize];
    reader.read_exact(&mut len_bytes)?;
    let mut lengths = Vec::with_capacity(n as usize);
    let mut payload_total = 0u64;
    for chunk in len_bytes.chunks_exact(4) {
        let len = u32::from_le_bytes(chunk.try_into().expect("checked length"));
        payload_total += u64::from(len);
        lengths.push(len);
    }
    Ok((lengths, 4 + 4 * u64::from(n) + payload_total))
}

/// Append-only writer. Rows are flushed to disk as they arrive; only a row
/// counter and the write cursor live in memory. The offset index streams to
/// the sidecar `.idx` file in parallel.
pub struct SpillWriter {
    path: PathBuf,
    writer: BufWriter<File>,
    index_writer: BufWriter<File>,
    rows: usize,
    cursor: u64,
}

impl SpillWriter {
    /// Creates (or truncates) a spill file at `path` plus its `.idx` sidecar,
    /// writing the magic + version headers.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self, SpillError> {
        let path = path.as_ref().to_path_buf();
        let file = owner_only_options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        let index_file = owner_only_options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(index_path(&path))?;
        let mut writer = BufWriter::new(file);
        let mut index_writer = BufWriter::new(index_file);
        write_file_header(&mut writer, DATA_MAGIC)?;
        write_file_header(&mut index_writer, INDEX_MAGIC)?;
        Ok(Self {
            path,
            writer,
            index_writer,
            rows: 0,
            cursor: HEADER_LEN,
        })
    }

    /// Appends one row of typed cells, spilling it to disk immediately in the
    /// binary record format and streaming its offset to the `.idx` sidecar.
    pub fn append(&mut self, row: &[CellValue]) -> Result<(), SpillError> {
        let n = u32::try_from(row.len())
            .ok()
            .filter(|n| *n <= MAX_COLUMNS)
            .ok_or_else(|| SpillError::Serialize(format!("row has {} columns", row.len())))?;
        self.index_writer.write_all(&self.cursor.to_le_bytes())?;

        self.writer.write_all(&n.to_le_bytes())?;
        let mut payload_total = 0u64;
        for cell in row {
            let len = encoded_cell_len(cell);
            let len = u32::try_from(len)
                .map_err(|_| SpillError::Serialize(format!("cell of {len} bytes exceeds u32")))?;
            payload_total += u64::from(len);
            self.writer.write_all(&len.to_le_bytes())?;
        }
        for cell in row {
            write_cell(&mut self.writer, cell)?;
        }
        self.rows += 1;
        self.cursor += 4 + 4 * u64::from(n) + payload_total;
        Ok(())
    }

    /// Number of rows written so far.
    pub fn row_count(&self) -> usize {
        self.rows
    }

    /// Flushes both buffers so the on-disk prefix is readable by a concurrent
    /// [`SpillReader`] while appends continue. Data is flushed BEFORE the
    /// index: every index entry on disk then points at fully written row
    /// bytes. Between explicit flushes the independent `BufWriter`s can leave
    /// the on-disk index AHEAD of the data (append writes the index entry
    /// first), so a concurrent reader must clamp to a row count communicated
    /// at a flush point — never its own `row_count()`. No fsync: same-OS
    /// page-cache visibility is all a local reader needs.
    pub fn flush(&mut self) -> Result<(), SpillError> {
        self.writer.flush()?;
        self.index_writer.flush()?;
        Ok(())
    }

    /// Flushes buffered writes and converts into a random-access [`SpillReader`].
    pub fn finish(mut self) -> Result<SpillReader, SpillError> {
        self.writer.flush()?;
        self.index_writer.flush()?;
        SpillReader::open(&self.path)
    }
}

/// Random-access reader that pages rows from disk without loading them all.
///
/// Both the rows AND the offset index are paged from disk — no O(total rows)
/// structure lives in memory, so this scales to sets whose index alone would
/// not fit in RAM.
pub struct SpillReader {
    reader: BufReader<File>,
    index: File,
    rows: usize,
}

impl SpillReader {
    /// Opens an existing spill file for random-access reading, validating the
    /// magic + version headers. If the `.idx` sidecar is missing, it is
    /// rebuilt on disk by walking the self-describing row headers once.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, SpillError> {
        let path = path.as_ref();
        let mut file = File::open(path)?;
        check_file_header(&mut file, DATA_MAGIC, "spill file")?;

        let index_file = index_path(path);
        if !index_file.exists() {
            rebuild_index(path, &index_file)?;
        }
        let mut index = File::open(&index_file)?;
        check_file_header(&mut index, INDEX_MAGIC, "spill index")?;
        let index_len = index.metadata()?.len();
        let body = index_len - HEADER_LEN;
        if !body.is_multiple_of(8) {
            return Err(SpillError::Corrupt(format!(
                "index body length {body} is not divisible by 8"
            )));
        }
        Ok(Self {
            reader: BufReader::new(file),
            index,
            rows: (body / 8) as usize,
        })
    }

    pub fn row_count(&self) -> usize {
        self.rows
    }

    /// Reads the offset window for rows `[start, end)` in a single index read.
    // `as_chunks` (clippy 1.98's suggestion) needs Rust 1.88; the workspace
    // MSRV is 1.85, so `chunks_exact` stays until the MSRV moves.
    #[allow(unknown_lints, clippy::chunks_exact_to_as_chunks)]
    fn offsets(&mut self, start: usize, end: usize) -> Result<Vec<u64>, SpillError> {
        if start >= end {
            return Ok(Vec::new());
        }
        self.index
            .seek(SeekFrom::Start(HEADER_LEN + 8 * start as u64))?;
        let mut buf = vec![0u8; 8 * (end - start)];
        self.index.read_exact(&mut buf)?;
        Ok(buf
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().expect("checked length")))
            .collect())
    }

    fn row_at_offset(&mut self, offset: u64) -> Result<Vec<CellValue>, SpillError> {
        self.reader.seek(SeekFrom::Start(offset))?;
        Ok(self.read_row_here()?.0)
    }

    /// Reads one row record at the reader's current position, returning the
    /// cells and the record's total encoded length (so a sequential caller
    /// can compute where the next row starts without seeking).
    fn read_row_here(&mut self) -> Result<(Vec<CellValue>, u64), SpillError> {
        let (lengths, record_len) = read_row_header(&mut self.reader)?;
        let mut out = Vec::with_capacity(lengths.len());
        for len in lengths {
            let mut payload = vec![0u8; len as usize];
            self.reader.read_exact(&mut payload)?;
            out.push(decode_cell(&payload)?);
        }
        Ok((out, record_len))
    }

    /// Reads a single row by absolute index — one index seek, one data seek,
    /// no parsing beyond the fixed-size cell decode.
    pub fn row(&mut self, index: usize) -> Result<Vec<CellValue>, SpillError> {
        if index >= self.rows {
            return Err(SpillError::Corrupt(format!("row {index} out of range")));
        }
        let offset = self.offsets(index, index + 1)?[0];
        self.row_at_offset(offset)
    }

    /// Reads a contiguous page `[offset, offset+len)`, clamped to the available
    /// rows. This is what the grid calls per page — peak memory is O(len), the
    /// page size, not O(total rows). The index window is fetched in one read.
    pub fn page(&mut self, offset: usize, len: usize) -> Result<Vec<Vec<CellValue>>, SpillError> {
        let end = offset.saturating_add(len).min(self.rows);
        if offset >= end {
            return Ok(Vec::new());
        }
        let offsets = self.offsets(offset, end)?;
        let mut out = Vec::with_capacity(offsets.len());
        // Rows are written append-only, so a page's rows are contiguous on
        // disk: seek once to the first row, then read forward, tracking the
        // position via each record's encoded length. Seeking per row would
        // discard the `BufReader` buffer every time (one syscall-backed read
        // per row); any index entry that does not match the running cursor
        // (corrupt or hand-edited index) falls back to an explicit seek.
        let mut cursor: Option<u64> = None;
        for row_offset in offsets {
            if cursor != Some(row_offset) {
                self.reader.seek(SeekFrom::Start(row_offset))?;
            }
            let (cells, record_len) = self.read_row_here()?;
            cursor = Some(row_offset + record_len);
            out.push(cells);
        }
        Ok(out)
    }
}

fn index_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.idx", path.to_string_lossy()))
}

/// `OpenOptions` that create spill artifacts readable by the owner only.
/// Spilled rows are query results — potentially sensitive data — so they
/// must never be world-readable under a permissive umask. On non-Unix
/// platforms the per-user profile directory provides the isolation.
fn owner_only_options() -> OpenOptions {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

/// Rebuilds a missing `.idx` sidecar by walking the data file's row headers
/// once, streaming offsets straight back to disk (O(1) memory).
fn rebuild_index(data_path: &Path, index_file: &Path) -> Result<(), SpillError> {
    let mut file = File::open(data_path)?;
    check_file_header(&mut file, DATA_MAGIC, "spill file")?;
    let total = file.metadata()?.len();
    let mut reader = BufReader::new(file);
    let mut out = BufWriter::new(
        owner_only_options()
            .create(true)
            .write(true)
            .truncate(true)
            .open(index_file)?,
    );
    write_file_header(&mut out, INDEX_MAGIC)?;
    let mut cursor = HEADER_LEN;
    while cursor < total {
        reader.seek(SeekFrom::Start(cursor))?;
        let (_, record_len) = read_row_header(&mut reader)?;
        out.write_all(&cursor.to_le_bytes())?;
        cursor += record_len;
    }
    if cursor != total {
        return Err(SpillError::Corrupt(format!(
            "trailing bytes: scanned to {cursor} of {total}"
        )));
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "studio_spill_{}_{}_{}.bin",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        p
    }

    fn cleanup(path: &Path) {
        std::fs::remove_file(path).ok();
        std::fs::remove_file(index_path(path)).ok();
    }

    fn row(i: usize) -> Vec<CellValue> {
        vec![
            CellValue::I64(i as i64),
            CellValue::String(format!("name-{i}")),
            CellValue::String(format!("payload-{i}")),
        ]
    }

    #[test]
    fn round_trips_rows_via_disk() {
        let path = tmp_path("roundtrip");
        let mut w = SpillWriter::create(&path).unwrap();
        for i in 0..1000 {
            w.append(&row(i)).unwrap();
        }
        assert_eq!(w.row_count(), 1000);
        let mut r = w.finish().unwrap();
        assert_eq!(r.row_count(), 1000);
        assert_eq!(r.row(0).unwrap(), row(0));
        assert_eq!(r.row(999).unwrap(), row(999));
        assert_eq!(r.row(500).unwrap(), row(500));
        cleanup(&path);
    }

    #[test]
    fn page_reads_match_per_row_reads_and_interleave_with_seeks() {
        let path = tmp_path("page-seq");
        let mut w = SpillWriter::create(&path).unwrap();
        for i in 0..300 {
            w.append(&row(i)).unwrap();
        }
        let mut r = w.finish().unwrap();
        // Sequential fast path: a whole page mid-file.
        let page = r.page(100, 50).unwrap();
        assert_eq!(page.len(), 50);
        for (i, cells) in page.iter().enumerate() {
            assert_eq!(cells, &row(100 + i));
        }
        // Interleaved single-row reads (which reposition the reader) must not
        // disturb subsequent page reads, and vice versa.
        assert_eq!(r.row(5).unwrap(), row(5));
        let page = r.page(290, 20).unwrap();
        assert_eq!(page.len(), 10, "clamped to available rows");
        assert_eq!(page[0], row(290));
        assert_eq!(page[9], row(299));
        cleanup(&path);
    }

    #[cfg(unix)]
    #[test]
    fn spill_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_path("perms");
        let mut w = SpillWriter::create(&path).unwrap();
        w.append(&row(0)).unwrap();
        w.finish().unwrap();
        for p in [path.clone(), index_path(&path)] {
            let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", p.display());
        }
        // A rebuilt index (sidecar missing) is owner-only too.
        std::fs::remove_file(index_path(&path)).unwrap();
        let _ = SpillReader::open(&path).unwrap();
        let mode = std::fs::metadata(index_path(&path))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        cleanup(&path);
    }

    #[test]
    fn round_trips_every_cell_type() {
        let path = tmp_path("types");
        let all = vec![
            CellValue::Null,
            CellValue::Bool(true),
            CellValue::Bool(false),
            CellValue::I64(i64::MIN),
            CellValue::I64(i64::MAX),
            CellValue::F64(-0.5),
            CellValue::F64(f64::MAX),
            CellValue::String(String::new()),
            CellValue::String("héllo 🙂 \"quoted\"".into()),
            CellValue::Bytes(vec![]),
            CellValue::Bytes(vec![0x00, 0xFF, 0x7F]),
        ];
        let mut w = SpillWriter::create(&path).unwrap();
        w.append(&all).unwrap();
        // Empty row (zero columns) must round-trip too.
        w.append(&[]).unwrap();
        let mut r = w.finish().unwrap();
        assert_eq!(r.row(0).unwrap(), all);
        assert_eq!(r.row(1).unwrap(), Vec::<CellValue>::new());
        cleanup(&path);
    }

    #[test]
    fn nan_round_trips_as_nan() {
        let path = tmp_path("nan");
        let mut w = SpillWriter::create(&path).unwrap();
        w.append(&[CellValue::F64(f64::NAN)]).unwrap();
        let mut r = w.finish().unwrap();
        match &r.row(0).unwrap()[0] {
            CellValue::F64(v) => assert!(v.is_nan()),
            other => panic!("expected F64, got {other:?}"),
        }
        cleanup(&path);
    }

    #[test]
    fn pages_clamp_and_window_correctly() {
        let path = tmp_path("pages");
        let mut w = SpillWriter::create(&path).unwrap();
        for i in 0..250 {
            w.append(&row(i)).unwrap();
        }
        let mut r = w.finish().unwrap();
        let page = r.page(100, 50).unwrap();
        assert_eq!(page.len(), 50);
        assert_eq!(page[0], row(100));
        assert_eq!(page[49], row(149));
        // Last page clamps.
        let tail = r.page(240, 50).unwrap();
        assert_eq!(tail.len(), 10);
        assert_eq!(tail[9], row(249));
        // Past the end is empty, not an error.
        assert!(r.page(1000, 10).unwrap().is_empty());
        cleanup(&path);
    }

    /// A reader opened from a path pages identically to one created via
    /// `finish` — this is the path the CLI `result-page` command and the grid
    /// use.
    #[test]
    fn reopen_pages_from_disk() {
        let path = tmp_path("reopen");
        let mut w = SpillWriter::create(&path).unwrap();
        for i in 0..500 {
            w.append(&row(i)).unwrap();
        }
        w.finish().unwrap(); // drop the writer-derived reader.

        let mut r = SpillReader::open(&path).unwrap();
        assert_eq!(r.row_count(), 500);
        assert_eq!(r.row(0).unwrap(), row(0));
        assert_eq!(r.row(499).unwrap(), row(499));
        let page = r.page(250, 100).unwrap();
        assert_eq!(page.len(), 100);
        assert_eq!(page[0], row(250));
        assert_eq!(page[99], row(349));
        cleanup(&path);
    }

    /// A flushed prefix of an in-progress file is readable by a concurrent
    /// reader while the writer keeps appending: rows `0..N` communicated at
    /// a flush point are stable forever (both files are append-only), and
    /// `finish()` after interleaved flushes is still correct.
    #[test]
    fn flushed_prefix_is_readable_mid_write() {
        let path = tmp_path("prefix");
        let mut w = SpillWriter::create(&path).unwrap();
        for i in 0..100 {
            w.append(&row(i)).unwrap();
        }
        w.flush().unwrap();
        let flushed = w.row_count();

        // Open mid-write and keep appending small rows so the index
        // BufWriter can auto-flush ahead of the data BufWriter.
        let mut r = SpillReader::open(&path).unwrap();
        for _ in 0..5_000 {
            w.append(&[CellValue::I64(7)]).unwrap();
        }
        assert!(r.row_count() >= flushed);
        let page = r.page(0, flushed).unwrap();
        assert_eq!(page.len(), flushed);
        assert_eq!(page[0], row(0));
        assert_eq!(page[flushed - 1], row(flushed - 1));

        drop(r);
        let mut done = w.finish().unwrap();
        assert_eq!(done.row_count(), 5_100);
        assert_eq!(done.row(99).unwrap(), row(99));
        assert_eq!(done.row(5_099).unwrap(), vec![CellValue::I64(7)]);
        cleanup(&path);
    }

    /// A deleted sidecar is rebuilt on disk from the self-describing row
    /// headers, and paging still works — with no in-memory offset table.
    #[test]
    fn missing_index_is_rebuilt_from_row_headers() {
        let path = tmp_path("rebuild");
        let mut w = SpillWriter::create(&path).unwrap();
        for i in 0..300 {
            w.append(&row(i)).unwrap();
        }
        w.finish().unwrap();
        std::fs::remove_file(index_path(&path)).unwrap();

        let mut r = SpillReader::open(&path).unwrap();
        assert_eq!(r.row_count(), 300);
        assert_eq!(r.row(299).unwrap(), row(299));
        assert!(
            index_path(&path).exists(),
            "rebuild must persist the sidecar for the next open"
        );
        cleanup(&path);
    }

    #[test]
    fn wrong_magic_or_version_is_rejected() {
        let path = tmp_path("magic");
        std::fs::write(&path, b"NOPE\x02\x00\x00\x00rest").unwrap();
        assert!(matches!(
            SpillReader::open(&path),
            Err(SpillError::Corrupt(_))
        ));
        let mut bad_version = DATA_MAGIC.to_vec();
        bad_version.extend_from_slice(&99u32.to_le_bytes());
        std::fs::write(&path, bad_version).unwrap();
        assert!(matches!(
            SpillReader::open(&path),
            Err(SpillError::Corrupt(_))
        ));
        cleanup(&path);
    }

    /// Memory-bound proof: a large result set pages from disk while the
    /// reader holds NO O(total) structure — the offset index is paged from
    /// the sidecar per read. We stream 200k rows to disk and then read
    /// random pages across the whole set.
    #[test]
    fn large_result_set_is_not_fully_materialized() {
        let path = tmp_path("membound");
        let total = 200_000usize;
        let mut w = SpillWriter::create(&path).unwrap();
        for i in 0..total {
            w.append(&row(i)).unwrap();
        }
        let mut r = w.finish().unwrap();
        assert_eq!(r.row_count(), total);

        let page_size = 1_000;
        for &start in &[0usize, 50_000, 123_456, total - 500] {
            let page = r.page(start, page_size).unwrap();
            let expected_len = page_size.min(total - start);
            assert_eq!(page.len(), expected_len);
            assert_eq!(page[0], row(start));
        }

        let on_disk = std::fs::metadata(&path).unwrap().len();
        assert!(
            on_disk > (total as u64) * 10,
            "rows should be spilled to disk, got {on_disk} bytes"
        );
        cleanup(&path);
    }

    /// Throughput probe for the AGENTS.md "keep it performant" bar. Ignored in
    /// CI; run with `cargo test -p sqlly-core --release -- --ignored spill_bench`
    /// to print write + random-page timings.
    #[test]
    #[ignore = "manual benchmark; prints timings"]
    fn spill_bench() {
        let path = tmp_path("bench");
        let total = 1_000_000usize;
        let wide_row = |i: usize| -> Vec<CellValue> {
            (0..17)
                .map(|c| match c % 4 {
                    0 => CellValue::I64((i * 17 + c) as i64),
                    1 => CellValue::F64(i as f64 * 1.5),
                    2 => CellValue::String(format!("value-{i}-{c}")),
                    _ => CellValue::Null,
                })
                .collect()
        };
        let start = std::time::Instant::now();
        let mut w = SpillWriter::create(&path).unwrap();
        for i in 0..total {
            w.append(&wide_row(i)).unwrap();
        }
        let mut r = w.finish().unwrap();
        let write_elapsed = start.elapsed();

        let start = std::time::Instant::now();
        let mut read_rows = 0usize;
        for &page_start in &[0usize, 250_000, 500_000, 750_000, total - 1_000] {
            read_rows += r.page(page_start, 1_000).unwrap().len();
        }
        let read_elapsed = start.elapsed();
        let on_disk = std::fs::metadata(&path).unwrap().len();
        println!(
            "spill_bench: wrote {total} rows x17 cols in {write_elapsed:?} \
             ({:.0} rows/ms), paged {read_rows} rows in {read_elapsed:?}, \
             {on_disk} bytes on disk",
            total as f64 / write_elapsed.as_millis().max(1) as f64
        );
        cleanup(&path);
    }
}
