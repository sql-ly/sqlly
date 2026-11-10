//! Redis Keys actions on a key listing in the results grid: delete the
//! selected keys, set their TTL, or remove their expiry.
//!
//! Which results qualify: a Redis-family result whose columns hold both a
//! `key` and a `type` column — the shape of `SQLLY.KEYS`, `SQLLY.KEYINFO`
//! and `SQLLY.TOPKEYS`, whose `key` cells are real key names, byte-exact.
//! A bare `key` column is not enough: `INFO` answers `section`/`key`/`value`,
//! where `key` names a server field, and offering to delete "used_memory"
//! would be a trap.
//!
//! The menu only collects the selected key names; the writes run through
//! the Redis key dialog's selected-keys mode, which shows the keys first,
//! asks for the typed count before a delete, and refuses to write on a
//! read-only or locked production connection — the same gates the
//! explorer's bulk actions use.

use sqlly_core::{CellValue, ResultColumn};
use sqlly_datatable::{ContextMenuItem, ContextMenuRequest, GridState, Selection};

use super::ResultGridMenuProvider;

/// Delete Selected Keys….
pub(crate) const DELETE: &str = "redis_delete_selected_keys";
/// Set TTL on Selected Keys….
pub(crate) const SET_TTL: &str = "redis_set_ttl_selected_keys";
/// Remove Expiry from Selected Keys….
pub(crate) const PERSIST: &str = "redis_persist_selected_keys";

/// What the user asked to do with the selected keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectedKeysAction {
    Delete,
    SetTtl,
    Persist,
}

impl SelectedKeysAction {
    pub(crate) fn from_action_id(id: &str) -> Option<Self> {
        match id {
            DELETE => Some(Self::Delete),
            SET_TTL => Some(Self::SetTtl),
            PERSIST => Some(Self::Persist),
            _ => None,
        }
    }
}

/// The index of the `key` column when `columns` are a Redis key listing
/// (see the module docs), `None` otherwise.
pub(crate) fn key_column(
    db: Option<sqlly_lang::sqlmodel::DatabaseType>,
    columns: &[ResultColumn],
) -> Option<usize> {
    if !db.is_some_and(|db| db.is_command_engine()) {
        return None;
    }
    let position = |name: &str| columns.iter().position(|c| c.name == name);
    position("type")?;
    position("key")
}

/// Whether a key listing came from a Redis Cluster: `SQLLY.KEYS` adds a
/// `masters` column on a cluster and never on a single server, so its
/// shape answers exactly. Other listings (`SQLLY.TOPKEYS`) cannot say.
pub(crate) fn listing_cluster(columns: &[ResultColumn]) -> Option<bool> {
    let has = |name: &str| columns.iter().any(|c| c.name == name);
    if has("masters") {
        Some(true)
    } else if has("cursor") && has("complete") {
        Some(false)
    } else {
        None
    }
}

/// The display rows a selection covers: whole rows, the rows of a cell
/// range or scattered cells, every row for a whole-column selection, and
/// the clicked row when there is no selection to speak of.
pub(crate) fn selected_rows(
    selection: &Selection,
    row_count: usize,
    clicked_row: Option<usize>,
) -> Vec<usize> {
    let mut rows: Vec<usize> = match selection {
        Selection::Row(r) => vec![*r],
        Selection::RowRange(a, b) | Selection::CellRange(a, _, b, _) => {
            (*a.min(b)..=*a.max(b)).collect()
        }
        Selection::Rows(rows) => rows.clone(),
        Selection::Cells(cells) => cells.iter().map(|(r, _)| *r).collect(),
        Selection::Column(_) | Selection::Columns(_) => (0..row_count).collect(),
        Selection::Cell(r, _) => vec![*r],
        Selection::None => clicked_row.into_iter().collect(),
    };
    rows.retain(|r| *r < row_count);
    rows.sort_unstable();
    rows.dedup();
    rows
}

/// The key names in `rows` at `key_col`, byte-exact (a binary key is a
/// `Bytes` cell), in row order without repeats; NULL and non-text cells
/// are skipped.
pub(crate) fn keys_in_rows(
    rows: &[Vec<CellValue>],
    key_col: usize,
    selected: &[usize],
) -> Vec<Vec<u8>> {
    let mut seen = std::collections::HashSet::new();
    selected
        .iter()
        .filter_map(|&r| match rows.get(r)?.get(key_col)? {
            CellValue::String(s) => Some(s.clone().into_bytes()),
            CellValue::Bytes(b) => Some(b.clone()),
            _ => None,
        })
        .filter(|key| seen.insert(key.clone()))
        .collect()
}

/// The Redis Keys submenu (after a divider, for the grid's Tools menu), or
/// nothing when the result is not a key listing.
pub(crate) fn menu_items(
    db: Option<sqlly_lang::sqlmodel::DatabaseType>,
    columns: &[ResultColumn],
) -> Vec<ContextMenuItem> {
    if key_column(db, columns).is_none() {
        return Vec::new();
    }
    let act = ContextMenuItem::action;
    vec![
        ContextMenuItem::separator(),
        ContextMenuItem::submenu(
            crate::localization::text("results.grid_menu.redis_keys.submenu"),
            vec![
                act(
                    SET_TTL,
                    crate::localization::text("results.grid_menu.redis_keys.set_ttl"),
                ),
                act(
                    PERSIST,
                    crate::localization::text("results.grid_menu.redis_keys.persist"),
                ),
                ContextMenuItem::separator(),
                act(
                    DELETE,
                    crate::localization::text("results.grid_menu.redis_keys.delete"),
                ),
            ],
        ),
    ]
}

impl ResultGridMenuProvider {
    /// Collect the selected key names and hand them to the key dialog.
    /// A windowed result reads its rows first, like the export actions.
    pub(super) fn redis_selected_keys_action(
        &self,
        action_id: &str,
        request: &ContextMenuRequest,
        state: &GridState,
        cx: &mut gpui::App,
    ) {
        let Some(action) = SelectedKeysAction::from_action_id(action_id) else {
            return;
        };
        if self.defer_until_export_rows(action_id, request, cx) {
            return;
        }
        let Some(rv) = self.results.upgrade() else {
            return;
        };
        let Some((columns, rows, _)) =
            rv.read(cx)
                .resolve_visible_rows(self.slot_index, state, self.pivot, cx)
        else {
            return;
        };
        let Some(key_col) = key_column(self.source_db(cx), &columns) else {
            return;
        };
        let selection = self.result_selection(state, cx);
        let clicked = request.target.display_row_index();
        // A windowed (spilled) grid numbers its rows from the window, while
        // the rows read above start at the result's first row.
        let offset = state.window.map(|w| w.offset).unwrap_or(0);
        let selected: Vec<usize> =
            selected_rows(&selection, rows.len().saturating_sub(offset), clicked)
                .into_iter()
                .map(|r| {
                    crate::results::backing_row_for_grid_row(state.window.map(|w| w.offset), r)
                })
                .collect();
        let keys = keys_in_rows(&rows, key_col, &selected);
        let cluster = listing_cluster(&columns);
        let results = self.results.clone();
        #[cfg(not(target_arch = "wasm32"))]
        cx.defer(move |cx| {
            crate::app::AppView::update_active_app_view(cx, move |app, window, cx| {
                app.open_redis_selected_keys_dialog(&results, keys, cluster, action, window, cx);
            });
        });
        #[cfg(target_arch = "wasm32")]
        let _ = (keys, cluster, results, action);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_lang::sqlmodel::DatabaseType;

    fn cols(names: &[&str]) -> Vec<ResultColumn> {
        names
            .iter()
            .map(|n| ResultColumn {
                name: n.to_string(),
                data_type: "string".into(),
                nullable: true,
            })
            .collect()
    }

    #[test]
    fn only_redis_key_listings_offer_the_actions() {
        let keys = cols(&["key", "type", "ttl_seconds", "complete", "cursor"]);
        assert_eq!(key_column(Some(DatabaseType::Redis), &keys), Some(0));
        assert_eq!(key_column(Some(DatabaseType::Valkey), &keys), Some(0));
        let top = cols(&["rank", "key", "type", "ttl_seconds", "bytes"]);
        assert_eq!(key_column(Some(DatabaseType::Redis), &top), Some(1));
        // INFO's `key` names a server field, not a key.
        let info = cols(&["section", "key", "value"]);
        assert_eq!(key_column(Some(DatabaseType::Redis), &info), None);
        // A SQL result with key + type columns is not a Redis keyspace.
        assert_eq!(key_column(Some(DatabaseType::Postgres), &keys), None);
        assert_eq!(key_column(None, &keys), None);
        assert!(menu_items(Some(DatabaseType::Postgres), &keys).is_empty());
        assert_eq!(menu_items(Some(DatabaseType::Redis), &keys).len(), 2);
    }

    #[test]
    fn cluster_shape_is_read_from_the_listing() {
        let single = cols(&["key", "type", "ttl_seconds", "complete", "cursor"]);
        assert_eq!(listing_cluster(&single), Some(false));
        let cluster = cols(&[
            "key",
            "type",
            "ttl_seconds",
            "complete",
            "cursor",
            "masters",
        ]);
        assert_eq!(listing_cluster(&cluster), Some(true));
        assert_eq!(listing_cluster(&cols(&["rank", "key", "type"])), None);
    }

    #[test]
    fn selections_resolve_to_rows() {
        assert_eq!(selected_rows(&Selection::Row(2), 5, None), vec![2]);
        assert_eq!(
            selected_rows(&Selection::RowRange(3, 1), 5, None),
            vec![1, 2, 3]
        );
        assert_eq!(
            selected_rows(&Selection::Rows(vec![4, 0, 4]), 5, None),
            vec![0, 4]
        );
        assert_eq!(
            selected_rows(&Selection::CellRange(1, 0, 2, 3), 5, None),
            vec![1, 2]
        );
        assert_eq!(
            selected_rows(&Selection::Cells(vec![(3, 1), (0, 2), (3, 0)]), 5, None),
            vec![0, 3]
        );
        assert_eq!(selected_rows(&Selection::Column(0), 3, None), vec![0, 1, 2]);
        assert_eq!(selected_rows(&Selection::Cell(1, 1), 3, Some(2)), vec![1]);
        assert_eq!(selected_rows(&Selection::None, 3, Some(2)), vec![2]);
        assert!(selected_rows(&Selection::None, 3, None).is_empty());
        // Rows past the end (a stale selection) are dropped.
        assert_eq!(
            selected_rows(&Selection::RowRange(1, 9), 3, None),
            vec![1, 2]
        );
    }

    #[test]
    fn keys_are_byte_exact_deduplicated_and_skip_nulls() {
        let rows = vec![
            vec![
                CellValue::String("user:1".into()),
                CellValue::String("hash".into()),
            ],
            vec![
                CellValue::Bytes(vec![0xff, b':', 0x00]),
                CellValue::String("string".into()),
            ],
            vec![CellValue::Null, CellValue::String("string".into())],
            vec![
                CellValue::String("user:1".into()),
                CellValue::String("hash".into()),
            ],
        ];
        assert_eq!(
            keys_in_rows(&rows, 0, &[0, 1, 2, 3, 7]),
            vec![b"user:1".to_vec(), vec![0xff, b':', 0x00]]
        );
        assert!(keys_in_rows(&rows, 5, &[0]).is_empty());
    }
}
