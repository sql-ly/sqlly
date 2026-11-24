//! Attachment cells: a user-picked data file imported into a named local
//! relation, or a PNG/JPEG image shown inline.
//!
//! `spec` keys ([`AttachmentSpec`]): `relation`, `path` (relative to the
//! notebook's folder, `/`-separated; the only path a shared notebook carries,
//! and the one the security scan checks), `absolute_path` (set instead when
//! the file is outside the notebook's folder or the notebook is unsaved; the
//! host imports it only after the user approved that path on this machine),
//! `content_hash` (`file-sha256:` of the bytes at approval time, so a
//! re-import notices a changed file), `size_bytes`, `format`, `file_name`,
//! `table` (the table to read from a SQLite file).

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::document::Cell;

/// Largest image an attachment cell renders.
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Largest image edge (pixels) an attachment cell renders.
pub const MAX_IMAGE_EDGE: u32 = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentFormat {
    Csv,
    Parquet,
    Json,
    Ndjson,
    Sqlite,
    Png,
    Jpeg,
}

impl AttachmentFormat {
    pub const DATA: [Self; 5] = [
        Self::Csv,
        Self::Parquet,
        Self::Json,
        Self::Ndjson,
        Self::Sqlite,
    ];

    /// The format a file name's extension names (`.csv.gz` and `.csv.zst`
    /// are CSV).
    pub fn from_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?.to_ascii_lowercase();
        let name = name
            .strip_suffix(".gz")
            .or_else(|| name.strip_suffix(".zst"))
            .unwrap_or(&name);
        let ext = name.rsplit_once('.')?.1;
        Some(match ext {
            "csv" | "tsv" | "txt" => Self::Csv,
            "parquet" | "pq" => Self::Parquet,
            "json" => Self::Json,
            "ndjson" | "jsonl" => Self::Ndjson,
            "sqlite" | "sqlite3" | "db" => Self::Sqlite,
            "png" => Self::Png,
            "jpg" | "jpeg" => Self::Jpeg,
            _ => return None,
        })
    }

    pub fn is_image(self) -> bool {
        matches!(self, Self::Png | Self::Jpeg)
    }

    /// Extensions the file picker offers.
    pub fn extensions() -> &'static [&'static str] {
        &[
            "csv", "tsv", "gz", "zst", "parquet", "json", "ndjson", "jsonl", "sqlite", "sqlite3",
            "db", "png", "jpg", "jpeg",
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Csv => "CSV",
            Self::Parquet => "Parquet",
            Self::Json => "JSON",
            Self::Ndjson => "NDJSON",
            Self::Sqlite => "SQLite",
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachmentSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absolute_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<AttachmentFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The table or view to read from a SQLite file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
}

impl AttachmentSpec {
    pub fn of(cell: &Cell) -> Self {
        serde_json::from_value(cell.spec.clone()).unwrap_or_default()
    }

    /// Write these keys into the cell's spec, keeping any other keys.
    pub fn write(&self, cell: &mut Cell) -> bool {
        if &Self::of(cell) == self {
            return false;
        }
        let mut spec = match std::mem::take(&mut cell.spec) {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        for key in [
            "relation",
            "path",
            "absolute_path",
            "content_hash",
            "size_bytes",
            "format",
            "file_name",
            "table",
        ] {
            spec.remove(key);
        }
        if let Ok(Value::Object(ours)) = serde_json::to_value(self) {
            spec.extend(ours);
        }
        cell.spec = if spec.is_empty() {
            Value::Null
        } else {
            Value::Object(spec)
        };
        true
    }

    pub fn has_file(&self) -> bool {
        self.path.is_some() || self.absolute_path.is_some()
    }

    pub fn is_image(&self) -> bool {
        self.format.is_some_and(AttachmentFormat::is_image)
    }

    /// Where the file is on this machine: `path` under `notebook_dir`, else
    /// `absolute_path`. `None` when a relative path has no folder to resolve
    /// against (an unsaved notebook) or climbs out of it.
    pub fn resolve(&self, notebook_dir: Option<&Path>) -> Option<PathBuf> {
        if let Some(rel) = &self.path {
            let dir = notebook_dir?;
            if !is_contained_relative(rel) {
                return None;
            }
            let mut out = dir.to_path_buf();
            for part in rel
                .split(['/', '\\'])
                .filter(|p| !p.is_empty() && *p != ".")
            {
                out.push(part);
            }
            return contained(dir, out);
        }
        self.absolute_path.as_ref().map(PathBuf::from)
    }

    /// Record the user's pick of `file`: relative to `notebook_dir` when it
    /// is inside it, else absolute.
    pub fn set_file(
        &mut self,
        file: &Path,
        notebook_dir: Option<&Path>,
        content_hash: String,
        size: u64,
    ) {
        self.format = AttachmentFormat::from_path(file);
        self.file_name = file
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string);
        self.content_hash = Some(content_hash);
        self.size_bytes = Some(size);
        match notebook_dir.and_then(|dir| relative_inside(dir, file)) {
            Some(rel) => {
                self.path = Some(rel);
                self.absolute_path = None;
            }
            None => {
                self.path = None;
                self.absolute_path = Some(file.to_string_lossy().into_owned());
            }
        }
    }
}

/// The link scheme a Markdown cell uses to show an attachment cell's
/// picture: `![alt](attachment:<cell name or id>)`. Nothing else in a
/// Markdown cell is ever loaded as an image.
pub const MARKDOWN_SCHEME: &str = "attachment:";

/// The attachment cell an image destination names, when it uses
/// [`MARKDOWN_SCHEME`].
pub fn markdown_attachment_name(destination: &str) -> Option<&str> {
    let text = destination
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>');
    let head = text.get(..MARKDOWN_SCHEME.len())?;
    if !head.eq_ignore_ascii_case(MARKDOWN_SCHEME) {
        return None;
    }
    let name = text[MARKDOWN_SCHEME.len()..].trim();
    (!name.is_empty()).then_some(name)
}

/// The attachment cells `markdown`'s images name, in order and without
/// repeats: `![alt](attachment:name)`, `![alt](attachment:name "title")`
/// and `![alt](<attachment:a name>)`.
pub fn markdown_attachment_refs(markdown: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = markdown;
    while let Some(at) = rest.find("](") {
        let after = &rest[at + 2..];
        rest = after;
        if !markdown_before_is_image(markdown, markdown.len() - after.len() - 2) {
            continue;
        }
        let trimmed = after.trim_start();
        let destination = match trimmed.strip_prefix('<') {
            Some(inner) => inner.split('>').next().unwrap_or(""),
            None => trimmed
                .split(|c: char| c == ')' || c.is_whitespace())
                .next()
                .unwrap_or(""),
        };
        if let Some(name) = markdown_attachment_name(destination)
            && !out.iter().any(|n| n == name)
        {
            out.push(name.to_string());
        }
    }
    out
}

/// Whether the `]` at byte `close` ends an image's alt text (`![…]`).
fn markdown_before_is_image(text: &str, close: usize) -> bool {
    let head = &text[..close];
    let mut depth = 0usize;
    for (i, c) in head.char_indices().rev() {
        match c {
            ']' => depth += 1,
            '[' if depth == 0 => return head[..i].ends_with('!'),
            '[' => depth -= 1,
            '\n' if head[..i].ends_with('\n') => return false,
            _ => {}
        }
    }
    false
}

/// `path` when, with every symlink followed, it is still inside `dir`: the
/// real path when the file exists, else `path` (its deepest existing folder
/// checked). A folder link pointing elsewhere can't lead out.
fn contained(dir: &Path, path: PathBuf) -> Option<PathBuf> {
    let root = dir.canonicalize().ok()?;
    if let Ok(real) = path.canonicalize() {
        return real.starts_with(&root).then_some(real);
    }
    let folder = path
        .ancestors()
        .skip(1)
        .find_map(|a| a.canonicalize().ok())?;
    folder.starts_with(&root).then_some(path)
}

/// A relative path that stays inside its folder (no root, drive, `..`).
fn is_contained_relative(path: &str) -> bool {
    let p = Path::new(path);
    !path.is_empty()
        && !path.starts_with(['/', '\\', '~'])
        && !path.contains(':')
        && p.components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        && !path.split(['/', '\\']).any(|part| part == "..")
}

/// `file` relative to `dir` with `/` separators, when `file` is inside it.
pub fn relative_inside(dir: &Path, file: &Path) -> Option<String> {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let rel = file.strip_prefix(&dir).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| match c {
            Component::Normal(part) => part.to_str().map(str::to_string),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The image type `bytes` start with, judged by magic number only (the
/// extension is never trusted).
pub fn sniff_image(bytes: &[u8]) -> Option<AttachmentFormat> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some(AttachmentFormat::Png)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(AttachmentFormat::Jpeg)
    } else {
        None
    }
}

/// Pixel size of a PNG (IHDR) or baseline/progressive JPEG (SOFn), read
/// from the header without decoding.
pub fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    match sniff_image(bytes)? {
        AttachmentFormat::Png => {
            if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
                return None;
            }
            let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
            let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
            Some((w, h))
        }
        _ => {
            let mut i = 2;
            while i + 9 < bytes.len() {
                if bytes[i] != 0xFF {
                    i += 1;
                    continue;
                }
                let marker = bytes[i + 1];
                if marker == 0xD8 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                    i += 2;
                    continue;
                }
                let len = usize::from(u16::from_be_bytes([bytes[i + 2], bytes[i + 3]]));
                let is_sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
                if is_sof {
                    let h = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]);
                    let w = u16::from_be_bytes([bytes[i + 7], bytes[i + 8]]);
                    return Some((u32::from(w), u32::from(h)));
                }
                i += 2 + len;
            }
            None
        }
    }
}

/// Why an image attachment is not shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageRejection {
    TooLarge { bytes: u64, limit: u64 },
    NotAnImage,
    TooManyPixels { width: u32, height: u32 },
}

/// Check image bytes before handing them to the renderer: real PNG/JPEG
/// magic, under the size cap, readable dimensions within the edge cap.
pub fn validate_image(bytes: &[u8]) -> Result<AttachmentFormat, ImageRejection> {
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(ImageRejection::TooLarge {
            bytes: bytes.len() as u64,
            limit: MAX_IMAGE_BYTES,
        });
    }
    let format = sniff_image(bytes).ok_or(ImageRejection::NotAnImage)?;
    let (width, height) = image_dimensions(bytes).ok_or(ImageRejection::NotAnImage)?;
    if width == 0 || height == 0 || width > MAX_IMAGE_EDGE || height > MAX_IMAGE_EDGE {
        return Err(ImageRejection::TooManyPixels { width, height });
    }
    Ok(format)
}

/// `file-sha256:` and the hex digest of a file's bytes, read in chunks.
#[cfg(not(target_arch = "wasm32"))]
pub fn file_hash(path: &Path) -> std::io::Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(&buf[..n]);
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok((format!("file-sha256:{hex}"), total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::{CellKind, ExecutionPolicy};

    #[test]
    fn formats_come_from_extensions() {
        let f = |p: &str| AttachmentFormat::from_path(Path::new(p));
        assert_eq!(f("data/People.CSV"), Some(AttachmentFormat::Csv));
        assert_eq!(f("x.csv.gz"), Some(AttachmentFormat::Csv));
        assert_eq!(f("x.parquet"), Some(AttachmentFormat::Parquet));
        assert_eq!(f("x.jsonl"), Some(AttachmentFormat::Ndjson));
        assert_eq!(f("x.sqlite3"), Some(AttachmentFormat::Sqlite));
        assert_eq!(f("chart.jpeg"), Some(AttachmentFormat::Jpeg));
        assert_eq!(f("x.exe"), None);
        assert_eq!(f("noext"), None);
        assert!(AttachmentFormat::Png.is_image());
        assert!(!AttachmentFormat::Csv.is_image());
    }

    #[test]
    fn files_inside_the_notebook_folder_are_stored_relative() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("data");
        std::fs::create_dir_all(&sub).unwrap();
        let inside = sub.join("people.csv");
        std::fs::write(&inside, "id\n1\n").unwrap();
        let mut spec = AttachmentSpec::default();
        spec.set_file(&inside, Some(dir.path()), "file-sha256:x".into(), 5);
        assert_eq!(spec.path.as_deref(), Some("data/people.csv"));
        assert_eq!(spec.absolute_path, None);
        assert_eq!(
            spec.resolve(Some(dir.path()))
                .unwrap()
                .canonicalize()
                .unwrap(),
            inside.canonicalize().unwrap()
        );
        assert!(crate::notebook_security::scan::is_safe_relative_path(
            spec.path.as_deref().unwrap()
        ));

        let other = tempfile::tempdir().unwrap();
        let outside = other.path().join("o.parquet");
        std::fs::write(&outside, "PAR1").unwrap();
        spec.set_file(&outside, Some(dir.path()), "file-sha256:y".into(), 4);
        assert_eq!(spec.path, None);
        assert!(spec.absolute_path.is_some());
        assert_eq!(spec.format, Some(AttachmentFormat::Parquet));
        assert_eq!(spec.resolve(None).unwrap(), outside);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_folder_cannot_lead_out_of_the_notebook_folder() {
        let root = tempfile::tempdir().unwrap();
        let nb = root.path().join("repo");
        let secrets = root.path().join("home/.ssh");
        std::fs::create_dir_all(&nb).unwrap();
        std::fs::create_dir_all(&secrets).unwrap();
        std::fs::write(secrets.join("config"), "Host *\n").unwrap();
        std::fs::write(nb.join("people.csv"), "id\n1\n").unwrap();
        std::os::unix::fs::symlink(root.path().join("home"), nb.join("d")).unwrap();
        std::os::unix::fs::symlink(secrets.join("config"), nb.join("link.csv")).unwrap();
        for bad in ["d/.ssh/config", "link.csv", "d/.ssh/missing.csv"] {
            let spec = AttachmentSpec {
                path: Some(bad.into()),
                ..Default::default()
            };
            assert_eq!(spec.resolve(Some(&nb)), None, "{bad}");
        }
        let good = AttachmentSpec {
            path: Some("people.csv".into()),
            ..Default::default()
        };
        assert_eq!(
            good.resolve(Some(&nb)).unwrap(),
            nb.join("people.csv").canonicalize().unwrap()
        );
    }

    #[test]
    fn hostile_relative_paths_never_resolve() {
        let dir = Path::new("nb");
        for bad in [
            "../x.csv",
            "a/../../x.csv",
            "/etc/passwd",
            "C:\\x.csv",
            "~/x",
        ] {
            let spec = AttachmentSpec {
                path: Some(bad.into()),
                ..Default::default()
            };
            assert_eq!(spec.resolve(Some(dir)), None, "{bad}");
        }
        let unsaved = AttachmentSpec {
            path: Some("x.csv".into()),
            ..Default::default()
        };
        assert_eq!(unsaved.resolve(None), None);
    }

    #[test]
    fn spec_round_trips_and_keeps_foreign_keys() {
        let mut cell = Cell::new("a", CellKind::Attachment, ExecutionPolicy::default());
        cell.spec = serde_json::json!({"caption": "Customers"});
        let spec = AttachmentSpec {
            relation: Some("people".into()),
            path: Some("people.csv".into()),
            content_hash: Some("file-sha256:abc".into()),
            size_bytes: Some(12),
            format: Some(AttachmentFormat::Csv),
            file_name: Some("people.csv".into()),
            ..Default::default()
        };
        assert!(spec.write(&mut cell));
        assert!(!spec.write(&mut cell));
        assert_eq!(cell.spec["caption"], "Customers");
        assert_eq!(cell.spec["format"], "csv");
        assert_eq!(AttachmentSpec::of(&cell), spec);
        assert!(cell.spec_inputs().is_empty(), "no key reads as an input");
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        b.extend_from_slice(b"IHDR");
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[8, 6, 0, 0, 0]);
        b
    }

    #[test]
    fn images_are_checked_by_content_size_and_dimensions() {
        assert_eq!(validate_image(&png(64, 32)), Ok(AttachmentFormat::Png));
        assert_eq!(image_dimensions(&png(64, 32)), Some((64, 32)));
        assert!(matches!(
            validate_image(&png(100_000, 10)),
            Err(ImageRejection::TooManyPixels { .. })
        ));
        assert_eq!(
            validate_image(b"<svg onload=alert(1)>"),
            Err(ImageRejection::NotAnImage)
        );
        let jpeg = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x11, 0x08, 0x00,
            0x20, 0x00, 0x40, 0x03, 0x00, 0x00, 0x00, 0x00,
        ];
        assert_eq!(image_dimensions(&jpeg), Some((64, 32)));
        assert_eq!(validate_image(&jpeg), Ok(AttachmentFormat::Jpeg));
        let mut big = png(10, 10);
        big.resize(MAX_IMAGE_BYTES as usize + 1, 0);
        assert!(matches!(
            validate_image(&big),
            Err(ImageRejection::TooLarge { .. })
        ));
    }

    #[test]
    fn file_hash_changes_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.csv");
        std::fs::write(&p, "id\n1\n").unwrap();
        let (a, size) = file_hash(&p).unwrap();
        assert_eq!(size, 5);
        assert!(a.starts_with("file-sha256:"));
        std::fs::write(&p, "id\n2\n").unwrap();
        assert_ne!(file_hash(&p).unwrap().0, a);
    }

    #[test]
    fn markdown_names_attachment_pictures_and_nothing_else() {
        let text = "# Notes\n\n![Chart](attachment:chart) and ![logo](<attachment: brand logo >)\n\
                    ![again](attachment:chart \"Title\")\n![remote](https://example.com/a.png)\n\
                    [a link](attachment:not_an_image) ![nested [x]](ATTACHMENT:c_42)\n![](attachment:)";
        assert_eq!(
            markdown_attachment_refs(text),
            vec!["chart", "brand logo", "c_42"]
        );
        assert_eq!(markdown_attachment_name("attachment:chart"), Some("chart"));
        assert_eq!(markdown_attachment_name("<Attachment:x>"), Some("x"));
        assert_eq!(markdown_attachment_name("https://example.com/a.png"), None);
        assert_eq!(markdown_attachment_name("attach"), None);
        assert_eq!(markdown_attachment_name("attachment:  "), None);
    }
}
