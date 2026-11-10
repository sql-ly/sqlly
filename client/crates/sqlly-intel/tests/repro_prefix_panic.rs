//! Regression: completion prefix helpers must not panic when the caret sits
//! adjacent to multi-byte UTF-8 characters (e.g. Spanish `ó` in a comment).
//! `rfind` with a `char` predicate returns the separator's *start* byte;
//! naively adding 1 lands inside the multi-byte scalar and slicing there
//! panics with "byte index is not a char boundary". See the app crash report
//! for a batch UPDATE with Spanish comments.

#[test]
fn current_prefix_after_multibyte_char_does_not_panic() {
    // Caret immediately after `ó` (2 bytes) — the reported crash site.
    let sql = "-- Marcó";
    let prefix = sqlly_intel::intellisense::current_prefix(sql, sql.len());
    assert_eq!(prefix, "");
}

#[test]
fn current_prefix_word_after_multibyte_separator() {
    // A real word preceded by a multi-byte separator: the prefix must be the
    // ASCII word, not a mid-char slice.
    let sql = "año_col"; // `ñ` is the separator; `o_col` is the word after it
    // Caret at end; the last non-word char before it is the multi-byte `ñ`.
    let prefix = sqlly_intel::intellisense::current_prefix(sql, sql.len());
    assert_eq!(prefix, "o_col");
}

#[test]
fn current_prefix_across_full_batch_cursor_positions() {
    // Sweep every char-boundary cursor position over a batch with accented
    // comments and datetimeoffset literals — none may panic.
    let sql = "UPDATE t SET c = '2026-08-03T08:25:00-06:00' -- Marcó salida\nWHERE id = 1;";
    for (i, _) in sql.char_indices() {
        let _ = sqlly_intel::intellisense::current_prefix(sql, i);
    }
    let _ = sqlly_intel::intellisense::current_prefix(sql, sql.len());
}
