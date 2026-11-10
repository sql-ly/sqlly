//! Move a query tab to another workbench window (tab context menu
//! "Move to New Window" / "Move to Window ‹n›").
//!
//! A move is copy-then-close: the source tab is captured into a
//! [`TabTransfer`] (text, caret, name, file path, dirty flag, pinned
//! connection, auto-refresh interval, file-watch baseline, an untitled tab's
//! query-version history, the parameter strip's values, and a snapshot of its
//! result sets), adopted by the
//! target window as a new tab, and only then closed in the source window. If
//! the target cannot take it, the source tab stays exactly where it was.
//! Results past the spill threshold travel as a bounded prefix (the first
//! [`MOVED_SPILL_ROW_CAP`] rows) under a banner that says so.
//!
//! The workspace session file is shared by every window, so each save merges
//! in the tabs of the other open windows (see
//! [`merge_other_window_tabs`]): after a move, whichever window saves last
//! still records both the moved tab and the tabs left behind.
//!
//! Entities are deliberately NOT handed across windows: every tab's editor
//! and results view carry detached subscriptions to the `AppView` that made
//! them, so a moved entity would keep reporting to the old window. The
//! target builds fresh ones and the results arrive as a read-only snapshot
//! under a banner (re-run to refresh), the same render the result cache and
//! query-version history use.
//!
//! Native-only: the browser build has a single window.

use super::*;
use std::cell::RefCell;

/// Everything a query tab carries to another window.
#[derive(Debug, Clone)]
pub(crate) struct TabTransfer {
    pub name: String,
    pub file_path: Option<String>,
    pub text: String,
    pub caret: usize,
    pub dirty: bool,
    pub pinned: bool,
    pub user_named: bool,
    pub auto_named: bool,
    pub auto_number: Option<u32>,
    pub connection_target: Option<crate::tabs::TabConnectionTarget>,
    pub last_run_sql: Option<String>,
    pub result_sets: Vec<crate::query_version_history::SnapshotSet>,
    /// The engine those results ran on (the tab's connection may have
    /// changed since), so the target window reads their columns the same.
    pub results_engine: Option<sqlly_lang::sqlmodel::DatabaseType>,
    /// Some set arrived cut short (a spilled set's bounded prefix).
    pub results_truncated: bool,
    pub auto_refresh_secs: Option<u64>,
    /// File-watch baseline: what the file looked like when last opened or
    /// saved, and any change the user already acknowledged. Without it the
    /// target would take the CURRENT disk state as clean and a later save
    /// could silently overwrite an external edit.
    pub disk_state: crate::file_watch::DiskState,
    pub disk_snapshot: Option<crate::file_watch::WatchSnapshot>,
    pub disk_prompt_ack: Option<crate::file_watch::WatchSnapshot>,
    /// An untitled tab's query-version history directory, handed over so the
    /// target can adopt it under the new tab's key instead of the source
    /// purging it on close.
    pub history_dir: Option<std::path::PathBuf>,
    /// The `-- sqlly param:` strip's rows and the values typed into them,
    /// so a moved tab runs with the same parameters.
    pub param_values: Vec<(crate::query_params::ParamSpec, String)>,
}

/// Rows a spilled (past the spill threshold) result set carries to another
/// window: the export cap, read as a prefix straight from the spill file.
pub(crate) const MOVED_SPILL_ROW_CAP: usize = crate::result_export::EXPORT_MAX_ROWS;

/// Facts about a tab that decide whether it can move right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MoveBlockers {
    pub ddl_view: bool,
    pub snapshot_only: bool,
    pub notebook: bool,
    pub running: bool,
    pub open_transaction: bool,
    pub staged_edits: bool,
    /// A mutation-review (or staged-save review) dialog is waiting on this
    /// tab: confirming it after the move would run against a closed tab.
    pub pending_review: bool,
}

/// Why a tab cannot move, or `None` when it can. Every refusal protects
/// state that lives in the source window and cannot be carried: a running
/// query's session, an open manual transaction, uncommitted staged grid
/// edits, or a view that is not a plain query editor.
pub(crate) fn move_block_reason(b: MoveBlockers) -> Option<String> {
    let key = if b.ddl_view || b.snapshot_only {
        "tabs.move.query_only"
    } else if b.notebook {
        "tabs.move.notebook_unsupported"
    } else if b.running {
        "tabs.move.query_running"
    } else if b.open_transaction {
        "tabs.move.transaction_open"
    } else if b.staged_edits {
        "tabs.move.staged_edits"
    } else if b.pending_review {
        "tabs.move.review_pending"
    } else {
        return None;
    };
    Some(crate::localization::text(key))
}

/// Menu labels for the other workbench windows. Windows are numbered by
/// creation order (window id) across ALL workbench windows, the current one
/// included, so "Window 2" names the same window from every window's menu.
/// Each label carries that window's active tab name to tell them apart.
pub(crate) fn window_move_labels(current: u64, others: &[(u64, String)]) -> Vec<(u64, String)> {
    let mut ids: Vec<u64> = others.iter().map(|(id, _)| *id).collect();
    ids.push(current);
    ids.sort_unstable();
    ids.dedup();
    let mut sorted: Vec<&(u64, String)> = others.iter().filter(|(id, _)| *id != current).collect();
    sorted.sort_by_key(|(id, _)| *id);
    sorted
        .into_iter()
        .map(|(id, active_tab)| {
            let n = ids.iter().position(|x| x == id).unwrap_or(0) + 1;
            let label = if active_tab.is_empty() {
                crate::localization::format("tabs.move.window_label", &[("number", &n.to_string())])
            } else {
                crate::localization::format(
                    "tabs.move.window_with_tab_label",
                    &[("number", &n.to_string()), ("tab", active_tab)],
                )
            };
            (*id, label)
        })
        .collect()
}

/// Banner shown over results that arrived with a moved tab.
pub(crate) const MOVED_RESULTS_BANNER: &str = "tabs.move.results_banner";

/// Banner for moved results when a large (spilled) set arrived as a prefix.
pub(crate) fn moved_results_banner(truncated: bool) -> String {
    if truncated {
        crate::localization::format(
            "tabs.move.results_truncated_banner",
            &[("count", &MOVED_SPILL_ROW_CAP.to_string())],
        )
    } else {
        crate::localization::text(MOVED_RESULTS_BANNER)
    }
}

/// Merge the tabs other open windows hold into this window's session, so the
/// single shared session file never drops a live window's tabs (each window
/// used to overwrite the file with only its own tabs; after a move, the last
/// window to save won and the moved tab — or the ones left behind — vanished
/// on relaunch). This window's tabs keep their order and indices (the
/// selected tab and split refer to them); other windows' tabs follow. A tab
/// is skipped when one already listed is the same document: the same file or
/// notebook path, or an untitled tab with the same title and text (a New
/// Window restores a copy of the session, and an untouched copy is not a
/// second document).
pub(crate) fn merge_other_window_tabs(
    mut session: crate::workspace_session::WorkspaceSession,
    other_windows: Vec<Vec<crate::workspace_session::TabState>>,
) -> crate::workspace_session::WorkspaceSession {
    fn same_document(
        a: &crate::workspace_session::TabState,
        b: &crate::workspace_session::TabState,
    ) -> bool {
        if a.notebook_path.is_some() || b.notebook_path.is_some() {
            return a.notebook_path == b.notebook_path;
        }
        match (&a.file_path, &b.file_path) {
            (Some(x), Some(y)) => x == y,
            (None, None) => a.title == b.title && a.content == b.content,
            _ => false,
        }
    }
    for tabs in other_windows {
        for tab in tabs {
            if !session.tabs.iter().any(|kept| same_document(kept, &tab)) {
                session.tabs.push(tab);
            }
        }
    }
    session
}

thread_local! {
    /// The transfer a freshly opened "Move to New Window" window adopts in
    /// place of restoring the saved session. Set only for the duration of
    /// one `open_window` call; `AppView::new` takes it.
    static PENDING_SEED: RefCell<Option<TabTransfer>> = const { RefCell::new(None) };
}

/// Taken by `AppView::new`: the moved tab this window opens with, if any.
pub(crate) fn take_pending_seed() -> Option<TabTransfer> {
    PENDING_SEED.with(|seed| seed.borrow_mut().take())
}

fn set_pending_seed(transfer: Option<TabTransfer>) {
    PENDING_SEED.with(|seed| *seed.borrow_mut() = transfer);
}

impl AppView {
    /// Other workbench windows a tab could move to, as (handle, menu label).
    pub(super) fn move_tab_window_targets(
        &self,
        current: gpui::WindowId,
        cx: &mut Context<Self>,
    ) -> Vec<(gpui::AnyWindowHandle, String)> {
        let me = cx.entity_id();
        let mut others: Vec<(u64, String)> = Vec::new();
        let mut handles: Vec<(u64, gpui::AnyWindowHandle)> = Vec::new();
        for handle in cx.windows() {
            if handle.window_id() == current {
                continue;
            }
            let Some(root) = handle.downcast::<gpui_component::Root>() else {
                continue;
            };
            let Ok(root) = root.read(cx) else {
                continue;
            };
            let Ok(view) = root.view().clone().downcast::<AppView>() else {
                continue;
            };
            if view.entity_id() == me {
                continue;
            }
            let active_name = {
                let v = view.read(cx);
                v.tabs
                    .get(v.active_tab)
                    .map(|t| t.name.clone())
                    .unwrap_or_default()
            };
            let id = handle.window_id().as_u64();
            others.push((id, active_name));
            handles.push((id, handle));
        }
        window_move_labels(current.as_u64(), &others)
            .into_iter()
            .filter_map(|(id, label)| {
                handles
                    .iter()
                    .find(|(hid, _)| *hid == id)
                    .map(|(_, h)| (*h, label))
            })
            .collect()
    }

    fn capture_tab_transfer(
        &self,
        tab_idx: usize,
        cx: &Context<Self>,
    ) -> Result<TabTransfer, String> {
        let tab = self
            .tabs
            .get(tab_idx)
            .ok_or_else(|| crate::localization::text("tabs.move.tab_missing"))?;
        let results = tab.results.read(cx);
        let blockers = MoveBlockers {
            ddl_view: tab.ddl_view.is_some(),
            snapshot_only: tab.snapshot_only,
            notebook: tab.notebook.is_some(),
            running: tab.active_query_id.is_some()
                || tab.active_multi_run.is_some()
                || results.is_running(),
            open_transaction: tab.manual_tx.is_some(),
            staged_edits: results.has_staged_changes(),
            pending_review: self
                .pending
                .mutation_review
                .as_ref()
                .is_some_and(|review| review.tab_id == tab.id)
                || self
                    .pending
                    .staged_save
                    .as_ref()
                    .is_some_and(|save| save.results.entity_id() == tab.results.entity_id()),
        };
        if let Some(reason) = move_block_reason(blockers) {
            return Err(reason.to_string());
        }
        let editor = tab.editor.read(cx);
        let (result_sets, results_truncated) =
            results.snapshot_sets_for_move(MOVED_SPILL_ROW_CAP, cx);
        let results_engine = results.results_engine();
        Ok(TabTransfer {
            name: tab.name.clone(),
            file_path: tab.file_path.clone(),
            text: editor.text().to_string(),
            caret: editor.cursor(),
            dirty: tab.dirty,
            pinned: tab.pinned,
            user_named: tab.user_named,
            auto_named: tab.auto_named,
            auto_number: tab.auto_number,
            connection_target: tab.connection_target.clone(),
            last_run_sql: tab.last_run_sql.clone(),
            result_sets,
            results_engine,
            results_truncated,
            auto_refresh_secs: tab.auto_refresh_secs,
            disk_state: tab.disk_state,
            disk_snapshot: tab.disk_snapshot,
            disk_prompt_ack: tab.disk_prompt_ack,
            history_dir: None,
            param_values: tab.query_params.snapshot(cx),
        })
    }

    /// Hand an untitled tab's version-history store over to the move: the
    /// store leaves this window's open-store map (so closing the tab here no
    /// longer purges it) and its directory travels in the transfer. A
    /// file-backed tab's history is keyed by its path and is shared already.
    fn detach_untitled_history(&mut self, tab_idx: usize) -> Option<std::path::PathBuf> {
        let tab = self.tabs.get(tab_idx)?;
        if tab.is_file_backed() {
            return None;
        }
        let key = crate::query_version_history::VersionStore::store_key(
            &Self::history_tab_id(&self.history_session_id, tab),
            None,
        );
        self.history_stores.remove(&key);
        let dir = crate::query_version_history::VersionStore::base_dir().join(&key);
        dir.is_dir().then_some(dir)
    }

    /// Take over a moved untitled tab's history directory under this
    /// window's key for the new tab. Best effort: on failure the history is
    /// simply not carried (the startup sweep removes the orphan).
    fn adopt_untitled_history(&mut self, tab_idx: usize, dir: std::path::PathBuf) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        if tab.is_file_backed() {
            return;
        }
        let key = crate::query_version_history::VersionStore::store_key(
            &Self::history_tab_id(&self.history_session_id, tab),
            None,
        );
        let target = crate::query_version_history::VersionStore::base_dir().join(&key);
        if target.exists() {
            return;
        }
        self.history_stores.remove(&key);
        if let Err(error) = std::fs::rename(&dir, &target) {
            tracing::warn!(%error, "could not carry query history to the moved tab");
        }
    }

    /// This window's session plus the tabs every other open workbench window
    /// holds (see [`merge_other_window_tabs`]). A window that is mid-update
    /// (leased) cannot be read and is skipped; it saves its own state when
    /// its update ends.
    pub(super) fn session_with_other_windows(
        session: crate::workspace_session::WorkspaceSession,
        me: gpui::EntityId,
        cx: &gpui::App,
    ) -> crate::workspace_session::WorkspaceSession {
        let mut others = Vec::new();
        for handle in cx.windows() {
            let Some(root) = handle.downcast::<gpui_component::Root>() else {
                continue;
            };
            let Ok(root) = root.read(cx) else {
                continue;
            };
            let Ok(view) = root.view().clone().downcast::<AppView>() else {
                continue;
            };
            if view.entity_id() == me {
                continue;
            }
            others.push(view.read(cx).current_workspace_session(cx).tabs);
        }
        merge_other_window_tabs(session, others)
    }

    /// Tab context menu: move `tab_idx` to `target` (an existing workbench
    /// window) or, with `None`, to a brand-new window.
    pub(super) fn move_tab_to_window(
        &mut self,
        tab_idx: usize,
        target: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
        // Windowed result sets travel as their first rows; read them off the
        // UI thread first, then move the tab (found again by id) with the
        // rows in memory.
        if let Some(tab) = self.tabs.get(tab_idx) {
            let results = tab.results.clone();
            let tab_id = tab.id.clone();
            let wants = results
                .read(cx)
                .spill_prefixes_wanted_for_all(MOVED_SPILL_ROW_CAP);
            if !wants.is_empty() {
                let weak = cx.weak_entity();
                let deferred = results.update(cx, |view, cx| {
                    view.defer_until_spill_prefixes(
                        wants,
                        crate::localization::text("tabs.move.reading_results"),
                        cx,
                        move |cx| {
                            let _ = weak.update(cx, |this, cx| {
                                if let Some(idx) = this.tabs.iter().position(|t| t.id == tab_id) {
                                    this.move_tab_to_window(idx, target, cx);
                                }
                            });
                        },
                    )
                });
                if deferred {
                    return;
                }
            }
        }
        let transfer = match self.capture_tab_transfer(tab_idx, cx) {
            Ok(transfer) => transfer,
            Err(reason) => {
                self.set_status(StatusLevel::Warning, reason);
                cx.notify();
                return;
            }
        };
        let Some(tab_id) = self.tabs.get(tab_idx).map(|t| t.id.clone()) else {
            return;
        };
        let mut transfer = transfer;
        transfer.history_dir = self.detach_untitled_history(tab_idx);
        let name = transfer.name.clone();
        let services = self.services.clone();
        let source = cx.weak_entity();
        // The click arrives inside this window's update; opening or
        // updating another window must wait until it is released.
        cx.defer(move |cx| {
            let adopted = match target {
                Some(handle) => Self::adopt_into_window(handle, transfer, cx),
                None => Self::open_window_seeded(services, Some(transfer), cx),
            };
            let _ = source.update(cx, |this, cx| {
                if adopted {
                    this.finish_tab_move_out(&tab_id, cx);
                } else {
                    this.set_status(
                        StatusLevel::Error,
                        crate::localization::format("tabs.move.target_missing", &[("name", &name)]),
                    );
                    cx.notify();
                }
            });
        });
    }

    fn adopt_into_window(
        handle: gpui::AnyWindowHandle,
        transfer: TabTransfer,
        cx: &mut gpui::App,
    ) -> bool {
        handle
            .update(cx, |root, window, cx| {
                let root = root.downcast::<gpui_component::Root>().ok()?;
                let view = root.read(cx).view().clone().downcast::<AppView>().ok()?;
                view.update(cx, |view, cx| {
                    view.adopt_transferred_tab(transfer, cx);
                    view.activate_moved_tab_connection(cx);
                });
                window.activate_window();
                Some(())
            })
            .ok()
            .flatten()
            .is_some()
    }

    /// Open a new workbench window whose only tab is `seed` (instead of the
    /// restored session). Returns whether the window opened.
    pub(crate) fn open_window_seeded(
        services: Services,
        seed: Option<TabTransfer>,
        cx: &mut gpui::App,
    ) -> bool {
        let seeded = seed.is_some();
        set_pending_seed(seed);
        let handle = Self::open_window(services, cx);
        // Never let an unconsumed seed leak into a later window.
        set_pending_seed(None);
        let Some(handle) = handle else {
            return false;
        };
        if seeded {
            let _ = gpui::AnyWindowHandle::from(handle).update(cx, |root, _window, cx| {
                if let Ok(root) = root.downcast::<gpui_component::Root>()
                    && let Ok(view) = root.read(cx).view().clone().downcast::<AppView>()
                {
                    view.update(cx, |view, cx| view.activate_moved_tab_connection(cx));
                }
            });
        }
        true
    }

    /// Add the moved tab as a new, active tab of this window.
    pub(super) fn adopt_transferred_tab(&mut self, transfer: TabTransfer, cx: &mut Context<Self>) {
        let TabTransfer {
            name,
            file_path,
            text,
            caret,
            dirty,
            pinned,
            user_named,
            auto_named,
            auto_number,
            connection_target,
            last_run_sql,
            result_sets,
            results_engine,
            results_truncated,
            auto_refresh_secs,
            disk_state,
            disk_snapshot,
            disk_prompt_ack,
            history_dir,
            param_values,
        } = transfer;
        let caret = caret.min(text.len());
        self.new_tab(cx, Some(text));
        let Some(tab) = self.tabs.last_mut() else {
            return;
        };
        tab.name = name.clone();
        tab.file_path = file_path;
        tab.dirty = dirty;
        tab.pinned = pinned;
        tab.user_named = user_named;
        tab.auto_named = auto_named;
        tab.auto_number = auto_number;
        // Held inert until `activate_moved_tab_connection`, which runs once
        // the window (and, for a new window, its session connection restore)
        // is fully built.
        tab.connection_target = connection_target;
        tab.restored_connection_inert = tab.connection_target.is_some();
        tab.last_run_sql = last_run_sql;
        tab.disk_state = disk_state;
        tab.disk_snapshot = disk_snapshot;
        tab.disk_prompt_ack = disk_prompt_ack;
        // Inputs are built by the strip's next render (it has the `Window`).
        tab.query_params = crate::query_params_strip::ParamStripState::seeded(param_values);
        let is_file_backed = tab.is_file_backed();
        let editor = tab.editor.clone();
        let results = tab.results.clone();
        editor.update(cx, |e, cx| e.place_cursor(caret, cx));
        if !result_sets.is_empty() {
            results.update(cx, |r, cx| {
                r.set_results_engine(results_engine);
                r.set_historical_results(moved_results_banner(results_truncated), result_sets, cx);
            });
        }
        if let Some(dir) = history_dir {
            self.adopt_untitled_history(self.tabs.len() - 1, dir);
        }
        if is_file_backed {
            self.ensure_file_watch_ticker(cx);
        }
        if auto_refresh_secs.is_some() {
            // `new_tab` made the moved tab active; this also starts the ticker.
            self.set_active_auto_refresh(auto_refresh_secs, cx);
        }
        self.set_status(
            StatusLevel::Success,
            crate::localization::format("tabs.move.adopted", &[("name", &name)]),
        );
        self.save_workspace_session(cx);
        cx.notify();
    }

    /// Wake the moved tab's pinned connection so the window follows it.
    fn activate_moved_tab_connection(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
            tab.restored_connection_inert = false;
        }
        self.sync_connection_to_active_tab(cx);
        cx.notify();
    }

    /// The target took the tab: close it here without a save prompt (its
    /// text, dirty flag and file path live on in the target) and without
    /// listing it under Reopen Closed Tab (it was moved, not closed).
    fn finish_tab_move_out(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let Some(index) = self.tab_index_by_id(tab_id) else {
            return;
        };
        let closed_before = self.closed_tabs.len();
        self.close_tab_impl(index, cx);
        self.closed_tabs.truncate(closed_before);
        // Checkpoint now: the saved session must list the moved tab (via the
        // target window) and every tab left here.
        self.save_workspace_session(cx);
        self.set_status(
            StatusLevel::Info,
            crate::localization::text("tabs.move.completed"),
        );
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_idle_query_tab_can_move() {
        assert_eq!(move_block_reason(MoveBlockers::default()), None);
    }

    #[test]
    fn source_only_state_blocks_the_move() {
        let cases = [
            MoveBlockers {
                ddl_view: true,
                ..Default::default()
            },
            MoveBlockers {
                snapshot_only: true,
                ..Default::default()
            },
            MoveBlockers {
                notebook: true,
                ..Default::default()
            },
            MoveBlockers {
                running: true,
                ..Default::default()
            },
            MoveBlockers {
                open_transaction: true,
                ..Default::default()
            },
            MoveBlockers {
                staged_edits: true,
                ..Default::default()
            },
            MoveBlockers {
                pending_review: true,
                ..Default::default()
            },
        ];
        for case in cases {
            assert!(move_block_reason(case).is_some(), "{case:?} must block");
        }
    }

    #[test]
    fn running_query_reason_names_the_fix() {
        let reason = move_block_reason(MoveBlockers {
            running: true,
            open_transaction: true,
            ..Default::default()
        })
        .unwrap();
        assert!(reason.contains("finish"), "{reason}");
    }

    #[test]
    fn windows_are_numbered_globally_by_creation_order() {
        // Current window is id 5; others 2 and 9 → Window 1 (id 2), the
        // current is Window 2, id 9 is Window 3.
        let labels =
            window_move_labels(5, &[(9, "Query 4".to_string()), (2, "Orders".to_string())]);
        assert_eq!(
            labels,
            vec![
                (2, "Move to Window 1 (Orders)".to_string()),
                (9, "Move to Window 3 (Query 4)".to_string()),
            ]
        );
    }

    #[test]
    fn empty_window_label_omits_the_tab_name_and_current_is_excluded() {
        let labels = window_move_labels(1, &[(1, "self".to_string()), (3, String::new())]);
        assert_eq!(labels, vec![(3, "Move to Window 2".to_string())]);
    }

    #[test]
    fn seed_is_taken_once() {
        set_pending_seed(Some(TabTransfer {
            name: "Query 1".into(),
            file_path: None,
            text: "SELECT 1".into(),
            caret: 3,
            dirty: true,
            pinned: false,
            user_named: false,
            auto_named: true,
            auto_number: Some(1),
            connection_target: None,
            last_run_sql: None,
            result_sets: Vec::new(),
            results_engine: None,
            results_truncated: false,
            auto_refresh_secs: Some(30),
            disk_state: crate::file_watch::DiskState::default(),
            disk_snapshot: None,
            disk_prompt_ack: None,
            history_dir: None,
            param_values: Vec::new(),
        }));
        assert_eq!(take_pending_seed().map(|t| t.text), Some("SELECT 1".into()));
        assert!(take_pending_seed().is_none());
    }

    #[test]
    fn truncated_moves_say_so_in_the_banner() {
        assert_eq!(
            moved_results_banner(false),
            crate::localization::text(MOVED_RESULTS_BANNER)
        );
        let banner = moved_results_banner(true);
        assert!(
            banner.contains(&MOVED_SPILL_ROW_CAP.to_string()),
            "{banner}"
        );
        assert!(banner.contains("re-run"));
    }

    fn untitled(title: &str, content: &str) -> crate::workspace_session::TabState {
        crate::workspace_session::TabState {
            title: title.into(),
            content: Some(content.into()),
            ..Default::default()
        }
    }

    fn file_tab(path: &str) -> crate::workspace_session::TabState {
        crate::workspace_session::TabState {
            title: path.into(),
            file_path: Some(path.into()),
            ..Default::default()
        }
    }

    /// Regression: each window wrote only its own tabs to the one shared
    /// session file, so after a move whichever window saved last erased the
    /// other's tabs — including the moved tab's unsaved text.
    #[test]
    fn a_save_keeps_every_open_windows_tabs() {
        let source = crate::workspace_session::WorkspaceSession {
            tabs: vec![untitled("Query 1", "SELECT 1"), file_tab("/w/a.sql")],
            selected_tab: 1,
            ..crate::workspace_session::WorkspaceSession::new()
        };
        let target_tabs = vec![untitled("Moved", "SELECT unsaved_work")];
        let merged = merge_other_window_tabs(source, vec![target_tabs]);
        let titles: Vec<&str> = merged.tabs.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, vec!["Query 1", "/w/a.sql", "Moved"]);
        assert_eq!(merged.selected_tab, 1, "this window's indices are kept");
        assert_eq!(
            merged.tabs[2].content.as_deref(),
            Some("SELECT unsaved_work")
        );
    }

    #[test]
    fn merged_sessions_keep_each_windows_parameter_values() {
        let param = |value: &str| crate::workspace_session::SavedParamValue {
            name: "@id".into(),
            ty: "int".into(),
            value: value.into(),
        };
        let mut own_tab = untitled("Query 1", "SELECT @id");
        own_tab.query_params = vec![param("1")];
        let own = crate::workspace_session::WorkspaceSession {
            tabs: vec![own_tab],
            ..crate::workspace_session::WorkspaceSession::new()
        };
        let mut moved = untitled("Moved", "SELECT @id + 1");
        moved.query_params = vec![param("2")];
        // A duplicate of this window's document: this window's values win.
        let mut copy = untitled("Query 1", "SELECT @id");
        copy.query_params = vec![param("99")];
        let merged = merge_other_window_tabs(own, vec![vec![copy, moved]]);
        let values: Vec<&str> = merged
            .tabs
            .iter()
            .map(|t| t.query_params[0].value.as_str())
            .collect();
        assert_eq!(values, ["1", "2"]);
        // And the merged session round-trips them through the file format.
        let decoded = crate::workspace_session::decode_session(
            &crate::workspace_session::encode_session(&merged),
        );
        assert_eq!(decoded.tabs[1].query_params, vec![param("2")]);
    }

    #[test]
    fn merged_sessions_do_not_duplicate_the_same_document() {
        let own = crate::workspace_session::WorkspaceSession {
            tabs: vec![untitled("Query 1", "SELECT 1"), file_tab("/w/a.sql")],
            ..crate::workspace_session::WorkspaceSession::new()
        };
        let other = vec![
            // An untouched New Window copy of the same untitled tab.
            untitled("Query 1", "SELECT 1"),
            // The same file open in both windows.
            file_tab("/w/a.sql"),
            // A copy that has since been edited is a different document.
            untitled("Query 1", "SELECT 2"),
            file_tab("/w/b.sql"),
        ];
        let merged = merge_other_window_tabs(own, vec![other.clone(), other]);
        let summary: Vec<(String, Option<String>)> = merged
            .tabs
            .iter()
            .map(|t| (t.title.clone(), t.content.clone().or(t.file_path.clone())))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("Query 1".to_string(), Some("SELECT 1".to_string())),
                ("/w/a.sql".to_string(), Some("/w/a.sql".to_string())),
                ("Query 1".to_string(), Some("SELECT 2".to_string())),
                ("/w/b.sql".to_string(), Some("/w/b.sql".to_string())),
            ]
        );
    }
}
