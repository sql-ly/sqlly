//! Custom "Open Project File" dialog.
//!
//! GPUI port of `ProjectFilePickerController.swift`: a modal overlay with a
//! Folder / Project SQL mode toggle, a metadata-aware file list (schema /
//! object-kind / confidence chips), prefix filtering (`v:`, `p:`, `schema:`,
//! …), and a syntax-highlighted SQL preview pane. The heavy lifting (scans,
//! filtering, metadata extraction) lives in `project_file_picker_model` and
//! runs on the background executor; this file is only view state + rendering.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::{
    ClipboardItem, Context, Entity, EventEmitter, IntoElement, KeyDownEvent, PathPromptOptions,
    Render, ScrollHandle, Styled, Subscription, Window, div, prelude::*, px,
};

use crate::dialog::{Dialog, DialogFrame};
use crate::project_file_picker_model::{
    self as model, FilterOutput, KnownSchemaObject, MetadataCache, PickerMode, PickerRow,
    PickerRowKind, PickerSnapshot, PreviewState, RowSource, ScanOptions, SqlObjectKind, SqlPreview,
};
use crate::sql_syntax::{SyntaxToken, classify};
use crate::theme::Theme;
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};

pub enum ProjectFilePickerEvent {
    /// The user chose a SQL file to open in a query tab.
    Open(PathBuf),
    /// The user chose a data-model object; the host fetches its definition
    /// from the server (`sp_helptext`-style) and shows it like a file.
    OpenDdl {
        schema: Option<String>,
        name: String,
        kind: SqlObjectKind,
    },
    /// The user picked a new project root; the host remembers it for the
    /// next time the picker opens.
    RootChanged(PathBuf),
    /// Show git status / diff / commit for the project root (desktop only).
    ShowGit(PathBuf),
    Cancelled,
}

impl EventEmitter<ProjectFilePickerEvent> for ProjectFilePicker {}

/// A loaded preview plus its pre-classified syntax spans.
struct RenderedPreview {
    preview: SqlPreview,
    spans: Vec<SyntaxToken>,
}

pub struct ProjectFilePicker {
    root: PathBuf,
    current_directory: PathBuf,
    mode: PickerMode,
    include_hidden: bool,
    /// Treat loaded data-model objects as openable "files".
    include_ddl: bool,

    filter: String,
    filter_input: Entity<InputState>,
    all_rows: Vec<PickerRow>,
    visible_rows: Vec<PickerRow>,
    selected: usize,
    /// Center-of-list status ("Scanning…", "No matches", …).
    state_line: Option<String>,
    state_details: Vec<String>,
    inline_help: Option<String>,
    scanning: bool,

    preview: Option<RenderedPreview>,

    known_objects: Vec<KnownSchemaObject>,
    /// Virtual rows for the data model, merged in when `include_ddl` is on.
    model_rows: Vec<PickerRow>,
    metadata_cache: Arc<MetadataCache>,
    /// Bumped per scan/preview request; stale background results are dropped.
    scan_generation: Arc<AtomicU64>,
    preview_generation: Arc<AtomicU64>,

    list_scroll: ScrollHandle,
    preview_scroll: ScrollHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl ProjectFilePicker {
    /// `root` is the project root used by Project SQL mode and as the Folder
    /// mode boundary; `known_objects` upgrades scanned metadata to "indexed"
    /// when the object exists in the live schema catalog.
    pub fn new(
        root: PathBuf,
        known_objects: Vec<KnownSchemaObject>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "project_picker.filter_placeholder",
            ))
        });
        let filter_subscription = cx.subscribe_in(
            &filter_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => {
                    let text = input.read(cx).value().to_string();
                    if this.filter != text {
                        this.filter = text;
                        this.apply_filter(None, Vec::new(), cx);
                    }
                }
                InputEvent::PressEnter { .. } => this.activate_selection(cx),
                _ => {}
            },
        );
        let mut this = Self {
            current_directory: root.clone(),
            root,
            mode: PickerMode::Project,
            include_hidden: false,
            include_ddl: false,
            filter: String::new(),
            filter_input,
            all_rows: Vec::new(),
            visible_rows: Vec::new(),
            selected: 0,
            state_line: None,
            state_details: Vec::new(),
            inline_help: None,
            scanning: false,
            preview: None,
            model_rows: model::model_object_rows(&known_objects),
            known_objects,
            metadata_cache: Arc::new(MetadataCache::default()),
            scan_generation: Arc::new(AtomicU64::new(0)),
            preview_generation: Arc::new(AtomicU64::new(0)),
            list_scroll: ScrollHandle::new(),
            preview_scroll: ScrollHandle::new(),
            frame: DialogFrame::new(Theme::scaled(960.0), Theme::scaled(620.0)),
            _subscriptions: vec![filter_subscription],
        };
        this.rescan("project_picker.scanning_project", cx);
        this
    }

    /// Focus the filter field — called by the host right after opening the
    /// picker so typing filters immediately.
    pub fn focus_filter(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_input.update(cx, |state, cx| {
            state.focus(window, cx);
        });
    }

    // -- scanning -----------------------------------------------------------

    fn rescan(&mut self, reason: &'static str, cx: &mut Context<Self>) {
        let generation = self.scan_generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.scanning = true;
        self.state_line = Some(crate::localization::text(reason));
        self.state_details.clear();
        cx.notify();

        let root = self.root.clone();
        let current = self.current_directory.clone();
        let mode = self.mode;
        let options = ScanOptions {
            include_hidden: self.include_hidden,
            ..ScanOptions::default()
        };
        let known_objects = self.known_objects.clone();
        let cache = self.metadata_cache.clone();
        let cancel_flag = self.scan_generation.clone();

        let task = cx.background_spawn(async move {
            let is_stale = || cancel_flag.load(Ordering::SeqCst) != generation;
            match mode {
                PickerMode::Folder => {
                    model::folder_snapshot(&root, &current, options, &known_objects, &cache)
                }
                PickerMode::Project => {
                    model::project_snapshot(&root, options, &known_objects, &cache, &is_stale)
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let snapshot: PickerSnapshot = task.await;
            this.update(cx, |this, cx| {
                if this.scan_generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                this.scanning = false;
                this.all_rows = snapshot.rows;
                this.apply_filter(snapshot.state_title, snapshot.state_detail, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Re-filters `all_rows`, keeping the current selection when the selected
    /// path survives the filter.
    fn apply_filter(
        &mut self,
        empty_title: Option<model::PickerMessage>,
        empty_detail: Vec<model::PickerMessage>,
        cx: &mut Context<Self>,
    ) {
        let selected_name = self
            .selected_row()
            .map(|row| (row.path.clone(), row.name.clone()));
        let combined: Vec<PickerRow> = if self.include_ddl {
            self.all_rows
                .iter()
                .chain(self.model_rows.iter())
                .cloned()
                .collect()
        } else {
            self.all_rows.clone()
        };
        let FilterOutput {
            rows,
            inline_help,
            state_title,
            state_detail,
        } = model::filter_rows(&combined, &self.filter, self.mode);
        self.visible_rows = rows;
        self.inline_help = inline_help.map(|message| message.resolve());

        let title = state_title.or_else(|| {
            self.visible_rows
                .is_empty()
                .then_some(empty_title)
                .flatten()
        });
        let detail = if state_detail.is_empty() && self.visible_rows.is_empty() {
            empty_detail
        } else {
            state_detail
        };
        self.state_line = title.map(|message| message.resolve());
        self.state_details = detail
            .into_iter()
            .map(|message| message.resolve())
            .collect();

        self.selected = selected_name
            .and_then(|(path, name)| {
                self.visible_rows
                    .iter()
                    .position(|row| row.path == path && row.name == name)
            })
            .or_else(|| self.visible_rows.iter().position(|row| row.is_openable()))
            .unwrap_or(0);
        self.load_selected_preview(false, cx);
        cx.notify();
    }

    // -- selection / activation ----------------------------------------------

    fn selected_row(&self) -> Option<&PickerRow> {
        self.visible_rows.get(self.selected)
    }

    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        if index < self.visible_rows.len() && index != self.selected {
            self.selected = index;
            self.load_selected_preview(false, cx);
            cx.notify();
        }
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.visible_rows.is_empty() {
            return;
        }
        let last = self.visible_rows.len() - 1;
        let next = self.selected.saturating_add_signed(delta).min(last);
        if next != self.selected {
            self.selected = next;
            self.list_scroll.scroll_to_item(next);
            self.load_selected_preview(false, cx);
            cx.notify();
        }
    }

    /// Enter / double-click: descend into directories (Folder mode) or open
    /// the selected SQL file.
    fn activate_selection(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        match row.kind {
            PickerRowKind::ParentDirectory | PickerRowKind::Directory => {
                if self.mode == PickerMode::Folder {
                    self.current_directory = row.path.clone();
                    self.rescan("project_picker.loading_files", cx);
                }
            }
            PickerRowKind::File => {
                if row.source == RowSource::Model {
                    cx.emit(ProjectFilePickerEvent::OpenDdl {
                        schema: row.metadata.schema.clone(),
                        name: row
                            .metadata
                            .object_name
                            .clone()
                            .unwrap_or_else(|| row.name.clone()),
                        kind: row.metadata.kind,
                    });
                } else if row.is_sql_file() {
                    if row.path.exists() {
                        cx.emit(ProjectFilePickerEvent::Open(row.path.clone()));
                    } else {
                        self.load_selected_preview(false, cx);
                    }
                }
            }
        }
    }

    fn escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter.is_empty() {
            cx.emit(ProjectFilePickerEvent::Cancelled);
        } else {
            self.filter.clear();
            self.filter_input
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.apply_filter(None, Vec::new(), cx);
        }
    }

    pub fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "up" => self.move_selection(-1, cx),
            "down" => self.move_selection(1, cx),
            "enter" => self.activate_selection(cx),
            "escape" => {
                // Escape is two-stage (clear the filter first, then cancel);
                // stop it here so the dialog chrome's escape-to-close never
                // bypasses the filter-clearing stage.
                cx.stop_propagation();
                self.escape(window, cx);
            }
            _ => {}
        }
    }

    // -- header actions --------------------------------------------------------

    fn set_mode(&mut self, mode: PickerMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        if mode == PickerMode::Project {
            self.current_directory = self.root.clone();
        }
        self.rescan("project_picker.scanning_files", cx);
    }

    fn navigate_up(&mut self, cx: &mut Context<Self>) {
        if self.mode != PickerMode::Folder || self.current_directory == self.root {
            return;
        }
        if let Some(parent) = self.current_directory.parent() {
            self.current_directory = parent.to_path_buf();
            self.rescan("project_picker.loading_files", cx);
        }
    }

    fn choose_root(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            self.state_line = Some(
                crate::browser_preview::unavailable(&crate::localization::text(
                    "project_picker.choose_folder_unavailable",
                ))
                .to_string(),
            );
            self.state_details.clear();
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let rx = cx.prompt_for_paths(PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some(crate::localization::text("project_picker.choose_root_prompt").into()),
            });
            cx.spawn(async move |this, cx| {
                if let Ok(Ok(Some(paths))) = rx.await
                    && let Some(path) = paths.into_iter().next()
                {
                    crate::sandbox_access::remember(&path);
                    this.update(cx, |this, cx| {
                        this.root = path.clone();
                        this.current_directory = path.clone();
                        this.mode = PickerMode::Project;
                        cx.emit(ProjectFilePickerEvent::RootChanged(path));
                        this.rescan("project_picker.scanning_project", cx);
                    })
                    .ok();
                }
            })
            .detach();
        }
    }

    fn create_new_sql_file(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            self.state_line = Some(
                crate::browser_preview::unavailable(&crate::localization::text(
                    "project_picker.create_file_unavailable",
                ))
                .to_string(),
            );
            self.state_details.clear();
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let directory = match self.mode {
                PickerMode::Folder => self.current_directory.clone(),
                PickerMode::Project => self.root.clone(),
            };
            let rx = cx.prompt_for_new_path(&directory, Some("query.sql"));
            cx.spawn(async move |this, cx| {
                if let Ok(Ok(Some(path))) = rx.await {
                    if !path.exists() && std::fs::write(&path, "").is_err() {
                        this.update(cx, |this, cx| {
                            this.state_line = Some(crate::localization::format(
                                "project_picker.create_file_failed",
                                &[("path", &path.display().to_string())],
                            ));
                            this.state_details.clear();
                            cx.notify();
                        })
                        .ok();
                        return;
                    }
                    this.update(cx, |_, cx| {
                        cx.emit(ProjectFilePickerEvent::Open(path));
                    })
                    .ok();
                }
            })
            .detach();
        }
    }

    fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        self.include_hidden = !self.include_hidden;
        self.rescan("project_picker.refreshing_files", cx);
    }

    /// Toggles data-model objects in the list; no rescan needed since the
    /// model rows are built up front.
    fn toggle_ddl(&mut self, cx: &mut Context<Self>) {
        self.include_ddl = !self.include_ddl;
        self.apply_filter(None, Vec::new(), cx);
    }

    // -- preview ---------------------------------------------------------------

    fn load_selected_preview(&mut self, full: bool, cx: &mut Context<Self>) {
        let generation = self.preview_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(row) = self.selected_row().filter(|row| row.is_sql_file()).cloned() else {
            self.preview = None;
            cx.notify();
            return;
        };

        let flag = self.preview_generation.clone();
        let task = cx.background_spawn(async move {
            let (max_bytes, max_lines) = if full {
                (usize::MAX, usize::MAX)
            } else {
                (model::PREVIEW_BYTE_LIMIT, model::PREVIEW_LINE_LIMIT)
            };
            let preview = model::load_preview(
                &row.path,
                &row.metadata,
                Some(&row.relative_path),
                max_bytes,
                max_lines,
            );
            let spans = if preview.state == PreviewState::Loaded {
                classify(&preview.text)
            } else {
                Vec::new()
            };
            RenderedPreview { preview, spans }
        });
        cx.spawn(async move |this, cx| {
            let rendered = task.await;
            this.update(cx, |this, cx| {
                if flag.load(Ordering::SeqCst) == generation {
                    this.preview = Some(rendered);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn reveal_selection(&self) {
        if let Some(row) = self.selected_row() {
            crate::reveal_path::reveal_file(&row.path);
        }
    }

    fn copy_selection_path(&self, cx: &mut Context<Self>) {
        if let Some(row) = self.selected_row() {
            cx.write_to_clipboard(ClipboardItem::new_string(row.relative_path.clone()));
        }
    }

    // -- rendering ----------------------------------------------------------

    fn button(
        &self,
        id: &'static str,
        label: impl Into<gpui::SharedString>,
        enabled: bool,
        prominent: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        let mut button = div()
            .id(id)
            .px(px(s(10.0)))
            .py(px(s(4.0)))
            .rounded(px(s(4.0)))
            .text_size(px(Theme::font_size_ratio(0.92)))
            .whitespace_nowrap();
        button = if !enabled {
            button
                .bg(Theme::panel_bg())
                .text_color(Theme::text_dim().opacity(0.5))
        } else if prominent {
            button
                .cursor_pointer()
                .bg(Theme::accent().opacity(0.2))
                .text_color(Theme::accent())
                .on_click(cx.listener(move |this, _ev, _window, cx| on_click(this, cx)))
        } else {
            button
                .cursor_pointer()
                .bg(Theme::panel_bg())
                .border_1()
                .border_color(Theme::border())
                .text_color(Theme::text())
                .on_click(cx.listener(move |this, _ev, _window, cx| on_click(this, cx)))
        };
        button.child(label.into())
    }

    fn mode_segment(
        &self,
        id: &'static str,
        label: &'static str,
        mode: PickerMode,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = Theme::scaled;
        let active = self.mode == mode;
        div()
            .id(id)
            .px(px(s(10.0)))
            .py(px(s(4.0)))
            .cursor_pointer()
            .text_size(px(Theme::font_size_ratio(0.92)))
            .bg(if active {
                Theme::selection()
            } else {
                Theme::panel_bg()
            })
            .text_color(if active {
                Theme::text()
            } else {
                Theme::text_dim()
            })
            .on_click(cx.listener(move |this, _ev, _window, cx| this.set_mode(mode, cx)))
            .child(crate::localization::text(label))
    }

    fn render_header(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let at_root = self.current_directory == self.root;
        let path_label = match self.mode {
            PickerMode::Project => crate::localization::format(
                "project_picker.project_path",
                &[("path", &self.root.display().to_string())],
            ),
            PickerMode::Folder => crate::localization::format(
                "project_picker.folder_path",
                &[("path", &self.current_directory.display().to_string())],
            ),
        };

        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .px(px(s(14.0)))
            .py(px(s(10.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .overflow_hidden()
                    .child(self.mode_segment(
                        "picker-mode-folder",
                        "project_picker.folder_mode",
                        PickerMode::Folder,
                        cx,
                    ))
                    .child(self.mode_segment(
                        "picker-mode-project",
                        "project_picker.project_mode",
                        PickerMode::Project,
                        cx,
                    )),
            )
            .child(self.button(
                "picker-up",
                crate::localization::text("project_picker.up_button"),
                self.mode == PickerMode::Folder && !at_root,
                false,
                cx,
                |this, cx| this.navigate_up(cx),
            ))
            .child(self.button(
                "picker-choose-root",
                crate::localization::text("project_picker.choose_root_button"),
                true,
                false,
                cx,
                |this, cx| this.choose_root(cx),
            ))
            .child(self.button(
                "picker-new-file",
                crate::localization::text("project_picker.new_file_button"),
                true,
                false,
                cx,
                |this, cx| this.create_new_sql_file(cx),
            ))
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .whitespace_nowrap()
                    .child(path_label),
            )
            .children(self.scanning.then(|| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "project_picker.scanning_indicator",
                    ))
            }))
            .child(
                div()
                    .id("picker-hidden-toggle")
                    .cursor_pointer()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .rounded(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .bg(if self.include_hidden {
                        Theme::selection()
                    } else {
                        Theme::panel_bg()
                    })
                    .border_1()
                    .border_color(Theme::border())
                    .text_color(if self.include_hidden {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .on_click(cx.listener(|this, _ev, _window, cx| this.toggle_hidden(cx)))
                    .child(crate::localization::text("project_picker.hidden_toggle")),
            )
            .child(
                div()
                    .id("picker-ddl-toggle")
                    .cursor_pointer()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .rounded(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .bg(if self.include_ddl {
                        Theme::selection()
                    } else {
                        Theme::panel_bg()
                    })
                    .border_1()
                    .border_color(Theme::border())
                    .text_color(if self.include_ddl {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .on_click(cx.listener(|this, _ev, _window, cx| this.toggle_ddl(cx)))
                    .child(crate::localization::text("project_picker.ddl_toggle")),
            )
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let row = &self.visible_rows[index];
        let is_selected = index == self.selected;
        let is_directory = matches!(
            row.kind,
            PickerRowKind::Directory | PickerRowKind::ParentDirectory
        );

        let name = match row.kind {
            PickerRowKind::ParentDirectory => "↑ ..".to_string(),
            PickerRowKind::Directory => row.name.clone(),
            PickerRowKind::File => row.name.clone(),
        };

        let mut chips: Vec<String> = Vec::new();
        if row.is_openable() {
            chips.push(row.metadata.schema_chip().unwrap_or_else(|| {
                crate::localization::text("project_picker.unknown_schema_chip")
            }));
            chips.push(crate::localization::text(row.metadata.type_chip()));
            if row.source == RowSource::Model {
                chips.push(crate::localization::text("project_picker.data_model_chip"));
            } else {
                chips.push(crate::localization::text(row.metadata.source_chip()));
            }
            if row.metadata.multiple_objects {
                chips.push(crate::localization::text(
                    "project_picker.multiple_objects_chip",
                ));
            }
        }
        if let Some(reason) = &row.match_reason {
            chips.push(crate::localization::format(
                "project_picker.match_chip",
                &[("reason", &reason.resolve())],
            ));
        }

        let mut meta: Vec<String> = Vec::new();
        if !is_directory {
            if let Some(bytes) = row.byte_count {
                meta.push(model::format_bytes(bytes));
            }
            if let Some(modified) = row.modified {
                let local: chrono::DateTime<chrono::Local> = modified.into();
                meta.push(local.format("%b %e, %Y %H:%M").to_string());
            }
        }
        let show_relative = row.relative_path != row.name && !is_directory;

        div()
            .id(("picker-row", index))
            .w_full()
            .px(px(s(12.0)))
            .py(px(s(5.0)))
            .cursor_pointer()
            .bg(if is_selected {
                Theme::selection()
            } else if index % 2 == 1 {
                Theme::row_alt()
            } else {
                Theme::panel_bg()
            })
            .on_click(
                cx.listener(move |this, ev: &gpui::ClickEvent, _window, cx| {
                    this.select(index, cx);
                    if ev.click_count() >= 2 {
                        this.activate_selection(cx);
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(1.0)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(4.0)))
                            .when(matches!(row.kind, PickerRowKind::Directory), |d| {
                                d.child(crate::icons::disclosure_chevron(false))
                            })
                            .child(
                                div()
                                    .text_size(px(Theme::font_size()))
                                    .text_color(Theme::text())
                                    .whitespace_nowrap()
                                    .child(name),
                            ),
                    )
                    .children((!chips.is_empty()).then(|| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim())
                            .whitespace_nowrap()
                            .child(chips.join("   "))
                    }))
                    .children(show_relative.then(|| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text_dim().opacity(0.7))
                            .whitespace_nowrap()
                            .child(row.relative_path.clone())
                    }))
                    .children((!meta.is_empty()).then(|| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.77)))
                            .text_color(Theme::text_dim().opacity(0.7))
                            .whitespace_nowrap()
                            .child(meta.join("   "))
                    })),
            )
    }

    /// Hand-rolled list, deliberately not gpui-component 0.6.4's
    /// `list::List`/`ListDelegate` (same verdict as `command_palette`):
    ///
    /// - **Variable row heights.** The kit's virtualized list measures one row
    ///   and reuses that height for every entry, but picker rows stack one to
    ///   four lines (name, then optional chips / relative-path / size-date
    ///   lines), so directories and files disagree on height and scrolling
    ///   would misplace rows.
    /// - **Click semantics.** The kit confirms on every row click (its
    ///   `on_click` runs `Confirm`); this picker needs single-click to only
    ///   select (so the preview pane follows) and double-click to open.
    /// - **Selection retention.** `apply_filter` keeps the selected row across
    ///   refilters by `(path, name)` and otherwise falls back to the first
    ///   *openable* row; the kit's `start_search` always resets selection to
    ///   the first row.
    /// - **Filter field placement + keys.** The filter input lives in the
    ///   footer with prefix-syntax inline help, not at the list top where the
    ///   kit hard-wires its query input, and Escape is two-stage
    ///   (clear-filter, then cancel) which the kit's `Cancel` action would
    ///   bypass.
    ///
    /// The preview-follows-selection coupling alone would have fit (via
    /// `set_selected_index`), but the points above are structural.
    fn render_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let rows: Vec<_> = (0..self.visible_rows.len())
            .map(|index| self.render_row(index, cx).into_any_element())
            .collect();

        div()
            .id("picker-list")
            .flex_1()
            .min_w(px(s(340.0)))
            .h_full()
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll)
            .border_r_1()
            .border_color(Theme::border())
            .children(rows)
            .children(self.state_line.clone().map(|line| {
                div()
                    .p(px(s(16.0)))
                    .pb(px(0.0))
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .text_color(Theme::text_dim())
                    .child(line)
            }))
            .children(self.state_details.iter().map(|detail| {
                div()
                    .p(px(s(16.0)))
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .text_color(Theme::text_dim())
                    .child(detail.clone())
            }))
    }

    fn render_preview(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let selected = self.selected_row().cloned();
        let is_model = selected
            .as_ref()
            .is_some_and(|row| row.source == RowSource::Model);
        let selection_exists = selected
            .as_ref()
            .is_some_and(|row| !is_model && row.path.exists());
        let is_sql = selected.as_ref().is_some_and(|row| row.is_sql_file());

        let (title, meta_line) = match (&self.preview, &selected) {
            (Some(rendered), _) => {
                let metadata = selected
                    .as_ref()
                    .map(|row| &row.metadata)
                    .filter(|_| is_sql);
                let chips = metadata
                    .map(|metadata| {
                        [
                            metadata.schema_chip().unwrap_or_else(|| {
                                crate::localization::text("project_picker.unknown_schema_chip")
                            }),
                            crate::localization::text(metadata.type_chip()),
                            crate::localization::text(metadata.source_chip()),
                        ]
                        .join("   ")
                    })
                    .unwrap_or_default();
                let detail = rendered
                    .preview
                    .detail
                    .iter()
                    .map(model::PreviewText::resolve)
                    .collect::<Vec<_>>()
                    .join(" • ");
                let line = [chips, detail]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join("   ");
                let title = if rendered.preview.title.is_empty() {
                    crate::localization::text("project_picker.preview_missing_title")
                } else {
                    rendered.preview.title.clone()
                };
                (title, line)
            }
            (None, Some(row)) if is_model => (
                row.name.clone(),
                crate::localization::text("project_picker.preview_model_detail"),
            ),
            (None, Some(row)) if !is_sql => (
                row.name.clone(),
                if matches!(
                    row.kind,
                    PickerRowKind::Directory | PickerRowKind::ParentDirectory
                ) {
                    crate::localization::text("project_picker.preview_folder_detail")
                } else {
                    crate::localization::text("project_picker.preview_sql_only")
                },
            ),
            _ => (
                crate::localization::text("project_picker.preview_no_selection"),
                crate::localization::text("project_picker.preview_select_file"),
            ),
        };

        let show_truncated_action = self
            .preview
            .as_ref()
            .is_some_and(|r| r.preview.is_truncated);

        let body: gpui::AnyElement = match &self.preview {
            Some(rendered) if rendered.preview.state == PreviewState::Loaded => {
                render_highlighted_sql(&rendered.preview.text, &rendered.spans).into_any_element()
            }
            Some(rendered) => div()
                .p(px(s(12.0)))
                .text_size(px(Theme::font_size_ratio(0.92)))
                .text_color(Theme::text_dim())
                .child(
                    rendered
                        .preview
                        .detail
                        .iter()
                        .map(model::PreviewText::resolve)
                        .collect::<Vec<_>>()
                        .join(" • "),
                )
                .into_any_element(),
            None => div().into_any_element(),
        };

        div()
            .flex()
            .flex_col()
            .w(px(s(430.0)))
            .h_full()
            .gap(px(s(6.0)))
            .p(px(s(12.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(Theme::text())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::text_dim())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(meta_line),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(6.0)))
                    .child(self.button(
                        "picker-reveal",
                        crate::localization::text("project_picker.reveal_button"),
                        selection_exists,
                        false,
                        cx,
                        |this, _cx| this.reveal_selection(),
                    ))
                    .child(self.button(
                        "picker-copy-path",
                        crate::localization::text("project_picker.copy_path_button"),
                        selection_exists || is_model,
                        false,
                        cx,
                        |this, cx| this.copy_selection_path(cx),
                    ))
                    .child(self.button(
                        "picker-full-preview",
                        crate::localization::text("project_picker.load_full_button"),
                        selection_exists && is_sql && show_truncated_action,
                        false,
                        cx,
                        |this, cx| this.load_selected_preview(true, cx),
                    )),
            )
            .child(
                div()
                    .id("picker-preview")
                    .flex_1()
                    .overflow_scroll()
                    .track_scroll(&self.preview_scroll)
                    .rounded(px(s(4.0)))
                    .bg(Theme::bg())
                    .border_1()
                    .border_color(Theme::border())
                    .child(body),
            )
    }

    fn render_footer(&mut self) -> impl IntoElement {
        // Text changes and Enter commits arrive via the `InputState`
        // subscription set up in `new` (filter -> apply_filter, Enter ->
        // activate_selection). Cancel/Open moved into the dialog chrome's
        // footer; this row keeps the search field and inline filter help.
        let s = Theme::scaled;
        let search = Input::new(&self.filter_input).cleanable(true);

        div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .px(px(s(14.0)))
            .py(px(s(10.0)))
            .border_t_1()
            .border_color(Theme::border())
            .children(self.inline_help.clone().map(|help| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .text_color(Theme::warning())
                    .child(help)
            }))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(div().flex_1().child(search)),
            )
    }
}

impl Render for ProjectFilePicker {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let open_enabled = self.selected_row().is_some_and(|row| {
            row.source == RowSource::Model || (row.is_sql_file() && row.path.exists())
        });

        // Key events bubble here from the focused filter input, exactly as
        // they reached the old card's `on_key_down`.
        let content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .on_key_down(cx.listener(Self::handle_key_down))
            .child(self.render_header(cx))
            .children(crate::browser_preview::preview_banner(
                if crate::browser_preview::served() {
                    crate::localization::text("project_picker.browser_served_banner")
                } else {
                    crate::localization::text("project_picker.browser_demo_banner")
                },
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_list(cx))
                    .child(self.render_preview(cx)),
            )
            .child(self.render_footer());

        let dialog = Dialog::new("project-file-picker", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("project_picker.dialog_title"))
            .easy_close(true)
            .on_close(|_this, _window, cx| {
                cx.emit(ProjectFilePickerEvent::Cancelled);
            })
            .bare_body()
            .child(content);
        #[cfg(not(target_arch = "wasm32"))]
        let dialog = dialog.button(
            Button::new("picker-git")
                .ghost()
                .label(crate::localization::text("project_picker.git_button"))
                .tooltip(crate::localization::text("project_picker.git_tooltip"))
                .on_click(cx.listener(|this, _, _window, cx| {
                    cx.emit(ProjectFilePickerEvent::ShowGit(this.root.clone()));
                })),
        );
        dialog
            .button(
                Button::new("picker-open")
                    .primary()
                    .label(crate::localization::text("project_picker.open_button"))
                    .disabled(!open_enabled)
                    .on_click(cx.listener(|this, _, _window, cx| this.activate_selection(cx))),
            )
            .build(self, window, cx)
    }
}

/// Line-numbered, syntax-highlighted SQL (same segmentation approach as
/// `ddl_viewer::render_sql_line`).
fn render_highlighted_sql(text: &str, spans: &[SyntaxToken]) -> gpui::Div {
    let s = Theme::scaled;
    let lines: Vec<&str> = text.split('\n').collect();
    let number_width = lines.len().to_string().len();
    let mut line_start = 0usize;

    div()
        .flex()
        .flex_col()
        .px(px(s(10.0)))
        .py(px(s(8.0)))
        .font_family(Theme::editor_font_family())
        .children(lines.into_iter().enumerate().map(|(index, line)| {
            let number = format!("{:>width$}  ", index + 1, width = number_width);
            let element = render_sql_line(line, line_start, spans, number);
            line_start += line.len() + 1;
            element
        }))
}

fn render_sql_line(
    line: &str,
    line_start: usize,
    spans: &[SyntaxToken],
    number: String,
) -> gpui::Div {
    let line_end = line_start + line.len();
    let mut segments: Vec<gpui::Div> = Vec::new();
    segments.push(
        div()
            .text_color(Theme::text_dim().opacity(0.6))
            .child(number),
    );
    let mut cursor = line_start;

    for span in spans {
        if span.end <= line_start || span.start >= line_end || span.end <= cursor {
            continue;
        }
        if span.start > cursor {
            let gap = &line[cursor - line_start..span.start.min(line_end) - line_start];
            segments.push(div().text_color(Theme::text()).child(gap.to_string()));
        }
        let seg_start = span.start.max(cursor) - line_start;
        let seg_end = span.end.min(line_end) - line_start;
        segments.push(
            div()
                .text_color(crate::ddl_viewer::token_color(span.kind))
                .child(line[seg_start..seg_end].to_string()),
        );
        cursor = span.end.min(line_end).max(cursor);
    }
    if cursor < line_end {
        segments.push(
            div()
                .text_color(Theme::text())
                .child(line[cursor - line_start..].to_string()),
        );
    }

    div()
        .flex()
        .flex_row()
        .min_h(px(Theme::scaled(17.0)))
        .whitespace_nowrap()
        .text_size(px(Theme::font_size_ratio(0.92)))
        .children(segments)
}

/// Convenience used by `app.rs` when constructing the picker: translate the
/// live completion catalog into the picker's schema-object list.
pub fn known_objects_from_catalog(
    catalog: &sqlly_intel::intellisense::CompletionCatalog,
) -> Vec<KnownSchemaObject> {
    use crate::project_file_picker_model::SqlObjectKind;
    catalog
        .objects
        .iter()
        .filter_map(|object| {
            let kind = match object.kind {
                sqlly_core::ObjectKind::Table => SqlObjectKind::Table,
                sqlly_core::ObjectKind::View => SqlObjectKind::View,
                sqlly_core::ObjectKind::StoredProcedure => SqlObjectKind::Sproc,
                sqlly_core::ObjectKind::ScalarFunction
                | sqlly_core::ObjectKind::TableFunction
                | sqlly_core::ObjectKind::AggregateFunction => SqlObjectKind::Function,
                _ => return None,
            };
            Some(KnownSchemaObject {
                schema: object.schema.clone(),
                name: object.name.clone(),
                kind,
            })
        })
        .collect()
}
