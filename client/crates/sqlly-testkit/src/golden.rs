//! Golden-file compare helpers for deterministic SQL/formatter tests.

use std::fs;
use std::path::Path;

/// Compare `actual` text against a golden file on disk.
pub struct GoldenTest;

impl GoldenTest {
    /// Trim-compare against `golden_path`. When `SQLLY_UPDATE_GOLDEN=1`, rewrite
    /// the golden file and return `Ok(true)`. Otherwise return `Ok(false)` on mismatch.
    pub fn compare(actual: &str, golden_path: impl AsRef<Path>) -> std::io::Result<bool> {
        let golden_path = golden_path.as_ref();
        if std::env::var("SQLLY_UPDATE_GOLDEN").as_deref() == Ok("1") {
            Self::update(golden_path, actual)?;
            return Ok(true);
        }
        let expected = fs::read_to_string(golden_path)?;
        // Windows checkouts can materialize goldens with CRLF endings;
        // compare on normalized `\n` so the result is checkout-independent.
        Ok(normalize(actual) == normalize(&expected))
    }

    /// Write golden content (overwrite).
    pub fn update(golden_path: impl AsRef<Path>, content: &str) -> std::io::Result<()> {
        let golden_path = golden_path.as_ref();
        if let Some(parent) = golden_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(golden_path, content)
    }
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_ignores_crlf_in_the_golden_file() {
        let dir = std::env::temp_dir().join(format!("sqlly-golden-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("crlf.golden.sql");
        fs::write(&path, "SELECT\n  1\r\nFROM t\r\n").unwrap();
        assert!(GoldenTest::compare("SELECT\n  1\nFROM t\n", &path).unwrap());
        assert!(!GoldenTest::compare("SELECT 2", &path).unwrap());
        fs::remove_dir_all(&dir).ok();
    }
}
