//! Cursor/prefix text helpers: extracting the identifier prefix being typed
//! and the word before it, with UTF-8-boundary-safe slicing.

pub fn current_prefix(sql: &str, cursor: usize) -> &str {
    let cursor = cursor.min(sql.len());
    let before = &sql[..cursor];
    let start = before
        .rfind(|ch: char| {
            // `#` opens a SQL Server temp-table name (`#temp`), which is
            // typed and matched like any other object name (feedback
            // ifb_416b).
            !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' || ch == '@' || ch == '#')
        })
        .map_or(0, |idx| word_start_after(before, idx));
    &before[start..]
}

/// The word before the one under the cursor: with the caret on a word
/// (`FROM Ord▏`) the word ahead of it, and once a separator follows the word
/// (`UPDATE ▏`) that word itself — it is complete, so it is the keyword that
/// decides what comes next (feedback ifb_3798).
pub(crate) fn previous_word(sql: &str, cursor: usize) -> &str {
    let cursor = cursor.min(sql.len());
    let upto = &sql[..cursor];
    let is_word_char = |ch: char| ch.is_ascii_alphanumeric() || ch == '_';
    let before = upto.trim_end();
    let before_previous = if upto.ends_with(is_word_char) {
        let end = before.rfind(|ch: char| !is_word_char(ch)).unwrap_or(0);
        before[..end].trim_end()
    } else {
        before
    };
    let start = before_previous
        .rfind(|ch: char| !is_word_char(ch))
        .map_or(0, |idx| word_start_after(before_previous, idx));
    &before_previous[start..]
}

/// Byte offset of the character that follows the separator whose first byte is
/// `sep_idx`. `rfind` with a `char` predicate returns the separator's *start*
/// byte; naively adding 1 lands inside a multi-byte character (e.g. a Spanish
/// `ó` in a comment) and slicing there panics with "byte index is not a char
/// boundary". Advancing by the separator's full UTF-8 width keeps the slice on
/// a boundary.
fn word_start_after(s: &str, sep_idx: usize) -> usize {
    let sep_len = s[sep_idx..].chars().next().map_or(1, char::len_utf8);
    sep_idx + sep_len
}
