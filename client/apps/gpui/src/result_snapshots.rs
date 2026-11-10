//! Named result snapshots: durable, portable captures of a result set that
//! reopen as read-only snapshot tabs without rerunning the query.
//!
//! Storage is one `.sqllyexport` file per snapshot (the app's existing
//! portable result format) plus a small JSON index carrying the metadata the
//! browser lists. Pure store logic is separated from the GPUI browser view
//! so retention/rename/delete rules are unit tested directly.

use gpui::{
    Context, Entity, EventEmitter, IntoElement, MouseButton, ParentElement, Render, Styled, Window,
    div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};

use crate::dialog::{Dialog, DialogFrame};
use crate::intellisense_feedback::redact_sql;
use crate::result_export::EXPORT_MAX_ROWS;
use crate::theme::Theme;

/// Snapshots keep at most this many rows; larger results save a labeled
/// prefix rather than silently materializing a spill-backed set. Shared with
/// spill-backed copy/export ([`EXPORT_MAX_ROWS`]).
pub const SNAPSHOT_MAX_ROWS: usize = EXPORT_MAX_ROWS;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct SnapshotMeta {
    pub id: String,
    pub name: String,
    pub created_unix_ms: i64,
    /// Engine label of the connection the result came from.
    #[serde(default)]
    pub engine: String,
    /// The query text that produced the result (bounded).
    #[serde(default)]
    pub query: String,
    pub row_count: usize,
    #[serde(default)]
    pub truncated: bool,
    /// File name (not path) of the `.sqllyexport` payload inside the store.
    pub file: String,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct SnapshotIndex {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    snapshots: Vec<SnapshotMeta>,
}

/// Filesystem-backed snapshot store rooted at a directory.
pub struct SnapshotStore {
    dir: std::path::PathBuf,
    /// Serializes payload writes, index updates, and orphan cleanup so a
    /// concurrent `list` cannot delete a payload `save_new` has not indexed yet.
    mutation: std::sync::Mutex<()>,
}

impl SnapshotStore {
    pub fn new(dir: std::path::PathBuf) -> Self {
        Self {
            dir,
            mutation: std::sync::Mutex::new(()),
        }
    }

    /// The app's default store under the data directory.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn default_store() -> Self {
        let mut dir = crate::preferences::preferences_path();
        dir.push("Snapshots");
        Self::new(dir)
    }

    fn index_path(&self) -> std::path::PathBuf {
        self.dir.join("snapshots.json")
    }

    /// Resolve a payload basename under `dir`, rejecting path escape.
    fn payload_path(&self, file: &str) -> Result<std::path::PathBuf, String> {
        if file.is_empty() {
            return Err(crate::localization::text(
                "results.snapshots.invalid_file_name",
            ));
        }
        if file.contains('/') || file.contains('\\') || file.contains("..") {
            return Err(crate::localization::text(
                "results.snapshots.invalid_file_name",
            ));
        }
        if std::path::Path::new(file).components().count().ne(&1) {
            return Err(crate::localization::text(
                "results.snapshots.invalid_file_name",
            ));
        }
        let Some(stem) = file.strip_suffix(".sqllyexport") else {
            return Err(crate::localization::text(
                "results.snapshots.invalid_file_name",
            ));
        };
        if uuid::Uuid::parse_str(stem).is_err() {
            return Err(crate::localization::text(
                "results.snapshots.invalid_file_name",
            ));
        }
        Ok(self.dir.join(file))
    }

    /// Load the index without repair. Missing file is an empty catalog; a
    /// corrupt or unreadable index is an error so mutations cannot clobber it.
    fn try_list(&self) -> Result<Vec<SnapshotMeta>, String> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&path).map_err(|error| {
            crate::localization::format(
                "results.snapshots.index_read_failed",
                &[("error", &error.to_string())],
            )
        })?;
        serde_json::from_str::<SnapshotIndex>(&text)
            .map(|index| index.snapshots)
            .map_err(|error| {
                crate::localization::format(
                    "results.snapshots.index_parse_failed",
                    &[("error", &error.to_string())],
                )
            })
    }

    /// List snapshots, self-repairing on the way: index entries whose payload
    /// file is missing are dropped (and the pruned index persisted), and
    /// stray `.sqllyexport` payloads no index entry references are deleted —
    /// so a crash between payload and index writes can never accumulate
    /// orphans or list snapshots that cannot open. A corrupt index yields an
    /// empty list without rewriting the file.
    pub fn list(&self) -> Vec<SnapshotMeta> {
        let Ok(_guard) = self.mutation.lock() else {
            return Vec::new();
        };
        let Ok(mut snapshots) = self.try_list() else {
            return Vec::new();
        };
        let before = snapshots.len();
        snapshots.retain(|meta| {
            self.payload_path(&meta.file)
                .ok()
                .is_some_and(|path| path.exists())
        });
        if snapshots.len() != before {
            let _ = self.write_index(snapshots.clone());
        }
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".sqllyexport") && !snapshots.iter().any(|meta| meta.file == name)
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        snapshots
    }

    /// Atomic index write: unique temp in the same directory, fsync, rename.
    fn write_index(&self, snapshots: Vec<SnapshotMeta>) -> Result<(), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let index = SnapshotIndex {
            version: 1,
            snapshots,
        };
        let text = serde_json::to_string_pretty(&index).map_err(|e| e.to_string())?;
        crate::fs_safe::write_atomic(&self.index_path(), text.as_bytes()).map_err(|e| e.to_string())
    }

    /// Persist a new snapshot: the envelope payload to its own file, the
    /// metadata to the index (newest first).
    #[allow(clippy::too_many_arguments)]
    pub fn save_new(
        &self,
        name: &str,
        engine: &str,
        query: &str,
        row_count: usize,
        truncated: bool,
        created_unix_ms: i64,
        envelope_json: &str,
    ) -> Result<SnapshotMeta, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(crate::localization::text(
                "results.snapshots.save_name_required",
            ));
        }
        let guard = self
            .mutation
            .lock()
            .map_err(|_| crate::localization::text("results.snapshots.store_busy"))?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let id = uuid::Uuid::new_v4().to_string();
        let file = format!("{id}.sqllyexport");
        std::fs::write(self.dir.join(&file), envelope_json).map_err(|e| e.to_string())?;
        let meta = SnapshotMeta {
            id,
            name: name.to_string(),
            created_unix_ms,
            engine: engine.to_string(),
            query: redact_sql(query).chars().take(2000).collect(),
            row_count,
            truncated,
            file: file.clone(),
        };
        let mut snapshots = self.try_list()?;
        snapshots.insert(0, meta.clone());
        if let Err(error) = self.write_index(snapshots) {
            // Never leave a payload the index does not know about.
            let _ = std::fs::remove_file(self.dir.join(&file));
            return Err(error);
        }
        drop(guard);
        Ok(meta)
    }

    #[cfg(test)]
    fn lock_mutation_for_test(&self) -> std::sync::MutexGuard<'_, ()> {
        self.mutation.lock().unwrap()
    }

    pub fn rename(&self, id: &str, new_name: &str) -> Result<(), String> {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            return Err(crate::localization::text(
                "results.snapshots.rename_name_required",
            ));
        }
        let mut snapshots = self.try_list()?;
        let Some(meta) = snapshots.iter_mut().find(|meta| meta.id == id) else {
            return Err(crate::localization::text(
                "results.snapshots.rename_not_found",
            ));
        };
        meta.name = new_name.to_string();
        self.write_index(snapshots)
    }

    pub fn delete(&self, id: &str) -> Result<(), String> {
        let mut snapshots = self.try_list()?;
        let Some(position) = snapshots.iter().position(|meta| meta.id == id) else {
            return Err(crate::localization::text(
                "results.snapshots.delete_not_found",
            ));
        };
        let meta = snapshots.remove(position);
        let payload = self.payload_path(&meta.file)?;
        let _ = std::fs::remove_file(payload);
        self.write_index(snapshots)
    }

    /// Read a snapshot's envelope payload.
    pub fn read(&self, id: &str) -> Result<String, String> {
        let meta = self
            .try_list()?
            .into_iter()
            .find(|meta| meta.id == id)
            .ok_or_else(|| crate::localization::text("results.snapshots.read_not_found"))?;
        let payload = self.payload_path(&meta.file)?;
        std::fs::read_to_string(payload).map_err(|error| {
            crate::localization::format(
                "results.snapshots.payload_read_failed",
                &[("error", &error.to_string())],
            )
        })
    }
}

/// Relative age label ("just now", "5 m ago", "3 h ago", "2 d ago").
pub fn relative_age(created_unix_ms: i64, now_unix_ms: i64) -> String {
    let seconds = (now_unix_ms - created_unix_ms).max(0) / 1000;
    if seconds < 60 {
        return crate::localization::text("results.snapshots.age_just_now");
    }
    let (key, value) = match seconds {
        0..=3_599 => ("results.snapshots.age_minutes", seconds / 60),
        3_600..=86_399 => ("results.snapshots.age_hours", seconds / 3_600),
        _ => ("results.snapshots.age_days", seconds / 86_400),
    };
    crate::localization::format(key, &[("count", &value.to_string())])
}

// ── Browser dialog ─────────────────────────────────────────────────────────

pub enum SnapshotBrowserEvent {
    /// Open a snapshot as a read-only tab; `text` is the envelope payload.
    Open {
        name: String,
        text: String,
    },
    Closed,
}

pub struct SnapshotBrowser {
    store: SnapshotStore,
    snapshots: Vec<SnapshotMeta>,
    selected: Option<usize>,
    rename_input: Entity<InputState>,
    error: Option<String>,
    frame: DialogFrame,
    _subscription: gpui::Subscription,
}

impl EventEmitter<SnapshotBrowserEvent> for SnapshotBrowser {}

impl SnapshotBrowser {
    pub fn new(store: SnapshotStore, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let snapshots = store.list();
        let rename_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.snapshots.rename_placeholder",
            ))
        });
        let subscription = cx.subscribe(
            &rename_input,
            |this: &mut Self, state, event: &InputEvent, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let value = state.read(cx).value().to_string();
                    this.apply_rename(value, cx);
                }
            },
        );
        Self {
            store,
            snapshots,
            selected: None,
            rename_input,
            error: None,
            frame: DialogFrame::new(Theme::scaled(640.0), Theme::scaled(460.0)),
            _subscription: subscription,
        }
    }

    fn refresh(&mut self) {
        self.snapshots = self.store.list();
        if self
            .selected
            .is_some_and(|selected| selected >= self.snapshots.len())
        {
            self.selected = None;
        }
    }

    fn apply_rename(&mut self, value: String, cx: &mut Context<Self>) {
        let Some(selected) = self.selected else {
            return;
        };
        let Some(meta) = self.snapshots.get(selected) else {
            return;
        };
        match self.store.rename(&meta.id, &value) {
            Ok(()) => {
                self.error = None;
                self.refresh();
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn open_selected(&mut self, cx: &mut Context<Self>) {
        let Some(meta) = self.selected.and_then(|i| self.snapshots.get(i)) else {
            return;
        };
        match self.store.read(&meta.id) {
            Ok(text) => {
                cx.emit(SnapshotBrowserEvent::Open {
                    name: meta.name.clone(),
                    text,
                });
                cx.emit(SnapshotBrowserEvent::Closed);
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some(meta) = self.selected.and_then(|i| self.snapshots.get(i)) else {
            return;
        };
        match self.store.delete(&meta.id) {
            Ok(()) => {
                self.error = None;
                self.refresh();
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }
}

impl Render for SnapshotBrowser {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let has_selection = self.selected.is_some();
        let now_ms = chrono::Utc::now().timestamp_millis();

        let mut list = div()
            .id("snapshot-list")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col();
        if self.snapshots.is_empty() {
            list = list.child(div().p(px(s(12.0))).text_color(Theme::text_dim()).child(
                crate::localization::text("results.snapshots.empty_instructions"),
            ));
        }
        for (i, meta) in self.snapshots.iter().enumerate() {
            let selected = self.selected == Some(i);
            let truncated = if meta.truncated {
                crate::localization::text("results.snapshots.truncated_suffix")
            } else {
                String::new()
            };
            let subtitle = crate::localization::format(
                "results.snapshots.row_subtitle",
                &[
                    ("engine_name", &meta.engine),
                    ("row_count", &meta.row_count.to_string()),
                    ("truncated_suffix", &truncated),
                    ("relative_age", &relative_age(meta.created_unix_ms, now_ms)),
                ],
            );
            let preview: String = meta
                .query
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(90)
                .collect();
            list = list.child(
                div()
                    .id(("snapshot-row", i))
                    .px(px(s(12.0)))
                    .py(px(s(6.0)))
                    .border_b_1()
                    .border_color(Theme::border())
                    .cursor_pointer()
                    .when(selected, |el| el.bg(Theme::row_hover()))
                    .hover(|style| style.bg(Theme::row_hover()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _ev, window, cx| {
                            this.selected = Some(i);
                            if let Some(meta) = this.snapshots.get(i) {
                                let name = meta.name.clone();
                                this.rename_input
                                    .update(cx, |state, cx| state.set_value(name, window, cx));
                            }
                            cx.notify();
                        }),
                    )
                    .child(div().text_color(Theme::text()).child(meta.name.clone()))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.78)))
                            .text_color(Theme::text_dim())
                            .child(subtitle),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.78)))
                            .text_color(Theme::text_dim())
                            .truncate()
                            .font_family(Theme::editor_font_family())
                            .child(preview),
                    ),
            );
        }

        // The list manages its own scrolling, so the chrome body stays bare;
        // the rename input rides in the footer's leading slot with the action
        // buttons in the trailing cluster (default Close replaces the old
        // header Close button).
        Dialog::new("snapshot-browser", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "results.snapshots.browser_dialog_title",
            ))
            .easy_close(true)
            .bare_body()
            .on_close(|_this, _window, cx| cx.emit(SnapshotBrowserEvent::Closed))
            .child(list)
            .children_body(self.error.clone().map(|error| {
                div()
                    .px(px(s(12.0)))
                    .py(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::error())
                    .child(error)
            }))
            .leading_button(
                div()
                    .w(px(s(220.0)))
                    .child(Input::new(&self.rename_input).small()),
            )
            .leading_button(
                Button::new("snapshot-rename")
                    .label(crate::localization::text("results.snapshots.rename_button"))
                    .ghost()
                    .xsmall()
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _ev, _w, cx| {
                        let value = this.rename_input.read(cx).value().to_string();
                        this.apply_rename(value, cx);
                    })),
            )
            .button(
                Button::new("snapshot-delete")
                    .label(crate::localization::text("results.snapshots.delete_button"))
                    .ghost()
                    .xsmall()
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _ev, _w, cx| this.delete_selected(cx))),
            )
            .button(
                Button::new("snapshot-open")
                    .label(crate::localization::text("results.snapshots.open_button"))
                    .xsmall()
                    .disabled(!has_selection)
                    .on_click(cx.listener(|this, _ev, _w, cx| this.open_selected(cx))),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> SnapshotStore {
        let dir = std::env::temp_dir().join(format!(
            "sqlly-snapshots-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        SnapshotStore::new(dir)
    }

    #[test]
    fn save_new_redacts_query_literals_before_persisting() {
        let store = temp_store();
        let meta = store
            .save_new(
                "login audit",
                "SQL Server",
                "CREATE LOGIN x WITH PASSWORD = 'secret'",
                1,
                false,
                1,
                "{}",
            )
            .unwrap();
        assert!(!meta.query.contains("secret"));
        assert!(meta.query.contains("'<redacted>'"));
        assert!(meta.query.contains("CREATE LOGIN x WITH PASSWORD"));
        assert_eq!(store.list()[0].query, meta.query);
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn save_list_rename_read_delete_round_trip() {
        let store = temp_store();
        let meta = store
            .save_new(
                "Before migration",
                "PostgreSQL",
                "SELECT 1",
                3,
                false,
                1_000,
                "{}",
            )
            .unwrap();
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.read(&meta.id).unwrap(), "{}");

        store.rename(&meta.id, "  After review  ").unwrap();
        assert_eq!(store.list()[0].name, "After review");
        assert!(
            store.rename(&meta.id, "   ").is_err(),
            "blank rename rejected"
        );

        store.delete(&meta.id).unwrap();
        assert!(store.list().is_empty());
        assert!(store.read(&meta.id).is_err());
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn newest_snapshot_lists_first_and_blank_names_are_rejected() {
        let store = temp_store();
        assert!(
            store
                .save_new("", "SQLite", "SELECT 1", 1, false, 0, "{}")
                .is_err()
        );
        store
            .save_new("first", "SQLite", "SELECT 1", 1, false, 1, "{}")
            .unwrap();
        store
            .save_new("second", "SQLite", "SELECT 2", 1, false, 2, "{}")
            .unwrap();
        let names: Vec<String> = store.list().into_iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["second".to_string(), "first".to_string()]);
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn list_repairs_missing_payloads_and_orphaned_files() {
        let store = temp_store();
        let kept = store
            .save_new("kept", "SQLite", "SELECT 1", 1, false, 1, "{}")
            .unwrap();
        let broken = store
            .save_new("broken", "SQLite", "SELECT 2", 1, false, 2, "{}")
            .unwrap();
        // Simulate a crash aftermath: payload gone but still indexed…
        std::fs::remove_file(store.dir.join(&broken.file)).unwrap();
        // …and an orphaned payload no index entry references.
        std::fs::write(store.dir.join("orphan.sqllyexport"), "{}").unwrap();

        let listed = store.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, kept.id);
        assert!(
            !store.dir.join("orphan.sqllyexport").exists(),
            "orphaned payload is cleaned up"
        );
        // The pruned index persisted: a raw re-read agrees.
        assert_eq!(store.try_list().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn payload_path_rejects_escape_and_accepts_uuid_names() {
        let store = temp_store();
        assert!(store.payload_path("../../etc/passwd").is_err());
        assert!(store.payload_path("foo/bar.sqllyexport").is_err());
        assert!(store.payload_path("not-a-uuid.sqllyexport").is_err());
        let id = uuid::Uuid::new_v4();
        let name = format!("{id}.sqllyexport");
        assert_eq!(store.payload_path(&name).unwrap(), store.dir.join(&name));
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn save_new_refuses_corrupt_index_without_clobbering() {
        let store = temp_store();
        std::fs::create_dir_all(&store.dir).unwrap();
        let corrupt = r#"{"version":1,"snapshots":[{"id":"old","name":"Old""#;
        std::fs::write(store.index_path(), corrupt).unwrap();

        let err = store
            .save_new("new", "SQLite", "SELECT 1", 1, false, 1, "{}")
            .unwrap_err();
        assert!(
            !err.is_empty(),
            "corrupt index should surface a parse error"
        );
        assert_eq!(
            std::fs::read_to_string(store.index_path()).unwrap(),
            corrupt,
            "corrupt index must not be overwritten"
        );
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn index_writes_are_staged_then_renamed() {
        let store = temp_store();
        store
            .save_new("a", "SQLite", "SELECT 1", 1, false, 1, "{}")
            .unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(&store.dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away");
        assert!(store.index_path().exists());
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn list_does_not_delete_unindexed_payload_while_save_holds_lock() {
        let store = std::sync::Arc::new(temp_store());
        std::fs::create_dir_all(&store.dir).unwrap();
        let id = uuid::Uuid::new_v4();
        let file = format!("{id}.sqllyexport");
        let payload_path = store.dir.join(&file);
        let _guard = store.lock_mutation_for_test();
        std::fs::write(&payload_path, "{}").unwrap();

        let store_clone = std::sync::Arc::clone(&store);
        let list_handle = std::thread::spawn(move || store_clone.list());

        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            payload_path.exists(),
            "list must not delete a payload while save holds the mutation lock"
        );

        store
            .write_index(vec![SnapshotMeta {
                id: id.to_string(),
                name: "in-flight".into(),
                created_unix_ms: 1,
                engine: "SQLite".into(),
                query: String::new(),
                row_count: 0,
                truncated: false,
                file: file.clone(),
            }])
            .unwrap();

        drop(_guard);
        let listed = list_handle.join().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(
            payload_path.exists(),
            "indexed payload must survive list after save completes"
        );
        let _ = std::fs::remove_dir_all(&store.dir);
    }

    #[test]
    fn relative_age_buckets() {
        assert_eq!(relative_age(0, 30_000), "just now");
        assert_eq!(relative_age(0, 5 * 60_000), "5 m ago");
        assert_eq!(relative_age(0, 3 * 3_600_000), "3 h ago");
        assert_eq!(relative_age(0, 2 * 86_400_000), "2 d ago");
        assert_eq!(relative_age(10_000, 0), "just now", "clock skew clamps");
    }
}
