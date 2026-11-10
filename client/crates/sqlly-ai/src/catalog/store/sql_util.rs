//! Shared SQLite helpers for bounded `IN (…)` lists and chunked writes.

use rusqlite::{Connection, ToSql, params_from_iter};

use super::CatalogError;

/// Conservative cap for SQLite bind-variable lists (well below typical 999/32766 limits).
pub const SQLITE_IN_CHUNK_SIZE: usize = 500;

/// Yield successive slices of `items` no larger than `chunk_size`.
pub fn chunked_slices_with_size<T>(items: &[T], chunk_size: usize) -> impl Iterator<Item = &[T]> {
    let chunk_size = chunk_size.max(1);
    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= items.len() {
            return None;
        }
        let end = (start + chunk_size).min(items.len());
        let slice = &items[start..end];
        start = end;
        Some(slice)
    })
}

/// Yield successive slices of `items` no larger than [`SQLITE_IN_CHUNK_SIZE`].
pub fn chunked_slices<T>(items: &[T]) -> impl Iterator<Item = &[T]> {
    chunked_slices_with_size(items, SQLITE_IN_CHUNK_SIZE)
}

/// Build `?1,?2,…` placeholders starting at `start_index` (1-based).
pub fn in_placeholders(count: usize, start_index: usize) -> String {
    (start_index..start_index + count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Run `UPDATE … WHERE col IN (…)` in chunks, binding `keys` as the IN list
/// and `fixed` as the leading parameters (e.g. timestamp values).
pub fn batch_update_in(
    conn: &Connection,
    sql_prefix: &str,
    sql_suffix: &str,
    fixed: &[&dyn ToSql],
    keys: &[String],
) -> Result<usize, CatalogError> {
    if keys.is_empty() {
        return Ok(0);
    }
    let mut total = 0usize;
    for chunk in chunked_slices(keys) {
        let placeholders = in_placeholders(chunk.len(), fixed.len() + 1);
        let sql = format!("{sql_prefix} ({placeholders}){sql_suffix}");
        let mut params: Vec<&dyn ToSql> = fixed.to_vec();
        for key in chunk {
            params.push(key);
        }
        total += conn.execute(&sql, params_from_iter(params))?;
    }
    Ok(total)
}
