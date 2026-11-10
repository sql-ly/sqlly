//! SQL Library sidebar panel: a folder tree of `.sql` files on disk.
//!
//! Desktop-only. Scanning and front-matter live in `sql_library_model`; this
//! file renders the tree and emits open/save/root-change events.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    MouseButton, MouseDownEvent, PromptLevel, Render, ScrollHandle, Styled, Subscription, Window,
    anchored, deferred, div, point, prelude::*, px,
};
use gpui_component::Sizable;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::{PopupMenu, PopupMenuItem};

use crate::sql_library_model::{
    FrontMatter, LibraryEntry, LibraryTree, filter_tree, sanitize_stem, scan_library,
};
use crate::theme::Theme;

#[derive(Clone, Debug)]
pub enum SqlLibraryEvent {
    OpenFile(PathBuf, FrontMatter),
    SaveActiveTabRequested { folder: PathBuf },
    RootChangeRequested,
    Refreshed,
}

impl EventEmitter<SqlLibraryEvent> for SqlLibraryPanel {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibraryRow {
    Folder { relative: PathBuf, depth: usize },
    File { entry: LibraryEntry, depth: usize },
}

pub fn visible_rows(tree: &LibraryTree, collapsed: &HashSet<PathBuf>) -> Vec<LibraryRow> {
    let mut rows = Vec::new();
    emit_level(tree, Path::new(""), 0, collapsed, &mut rows);
    rows
}

fn emit_level(
    tree: &LibraryTree,
    parent: &Path,
    depth: usize,
    collapsed: &HashSet<PathBuf>,
    rows: &mut Vec<LibraryRow>,
) {
    let mut child_folders: Vec<&PathBuf> = tree
        .folders
        .iter()
        .filter(|f| f.parent().unwrap_or_else(|| Path::new("")) == parent)
        .collect();
    child_folders.sort();
    for folder in child_folders {
        rows.push(LibraryRow::Folder {
            relative: folder.clone(),
            depth,
        });
        if !collapsed.contains(folder) {
            emit_level(tree, folder, depth + 1, collapsed, rows);
        }
    }
    let mut files: Vec<&LibraryEntry> = tree
        .entries
        .iter()
        .filter(|e| e.folder.as_path() == parent)
        .collect();
    files.sort_by(|a, b| a.name.cmp(&b.name));
    for entry in files {
        rows.push(LibraryRow::File {
            entry: entry.clone(),
            depth,
        });
    }
}

pub fn renamed_path(path: &Path, new_stem: &str) -> Option<PathBuf> {
    if new_stem.contains('/') || new_stem.contains('\\') || new_stem.contains('\0') {
        return None;
    }
    let trimmed = new_stem
        .trim()
        .trim_end_matches(".sql")
        .trim_end_matches(".SQL")
        .trim_end_matches(".Sql");
    let stem = sanitize_stem(trimmed);
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty())
        .unwrap_or("sql");
    Some(parent.join(format!("{stem}.{ext}")))
}

enum InlineEdit {
    NewFolder,
    Rename(PathBuf),
}

pub struct SqlLibraryPanel {
    root: PathBuf,
    tree: LibraryTree,
    filter_input: Entity<InputState>,
    filter_open: bool,
    filter: String,
    collapsed: HashSet<PathBuf>,
    selected: Option<PathBuf>,
    error: Option<String>,
    context_menu: Option<Entity<PopupMenu>>,
    context_menu_dismiss: Option<Subscription>,
    context_position: Option<gpui::Point<gpui::Pixels>>,
    overflow_menu: Option<Entity<PopupMenu>>,
    overflow_dismiss: Option<Subscription>,
    overflow_position: Option<gpui::Point<gpui::Pixels>>,
    inline: Option<InlineEdit>,
    inline_input: Entity<InputState>,
    list_scroll: ScrollHandle,
    focus_handle: FocusHandle,
    /// Git section (status, diff, commit) for the library root.
    git: Entity<crate::git_panel::GitPanel>,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for SqlLibraryPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SqlLibraryPanel {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let _ = std::fs::create_dir_all(&root);
        let tree = scan_library(&root).unwrap_or_default();
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "editor.sql_library.filter_placeholder",
            ))
        });
        let inline_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "editor.sql_library.name_placeholder",
            ))
        });
        let git =
            cx.new(|cx| crate::git_panel::GitPanel::new(root.clone(), "library-git", window, cx));
        cx.observe(&git, |_, _, cx| cx.notify()).detach();
        let subscriptions = vec![
            cx.subscribe_in(
                &filter_input,
                window,
                |this, input, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.filter = input.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &inline_input,
                window,
                |this, input, event: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        let value = input.read(cx).value().to_string();
                        this.commit_inline(value, window, cx);
                    }
                },
            ),
        ];
        Self {
            root,
            tree,
            filter_input,
            filter_open: false,
            filter: String::new(),
            collapsed: HashSet::new(),
            selected: None,
            error: None,
            context_menu: None,
            context_menu_dismiss: None,
            context_position: None,
            overflow_menu: None,
            overflow_dismiss: None,
            overflow_position: None,
            inline: None,
            inline_input,
            list_scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            git,
            _subscriptions: subscriptions,
        }
    }

    /// Is the header filter field showing? The sidebar header owns the toggle.
    pub fn filter_open(&self) -> bool {
        self.filter_open
    }

    pub fn overflow_open(&self) -> bool {
        self.overflow_menu.is_some()
    }

    pub fn filter_input(&self) -> &Entity<InputState> {
        &self.filter_input
    }

    /// Flip the filter field. Opening focuses it; closing clears the filter.
    pub fn toggle_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.filter_open {
            self.filter_open = false;
            self.filter.clear();
            self.filter_input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        } else {
            self.filter_open = true;
            self.filter_input.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        }
        cx.notify();
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        let _ = std::fs::create_dir_all(&root);
        self.root = root;
        self.rescan(cx);
    }

    pub fn rescan(&mut self, cx: &mut Context<Self>) {
        let root = self.root.clone();
        let task = cx.background_spawn(async move { scan_library(&root) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(tree) => {
                        this.tree = tree;
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e.to_string()),
                }
                cx.emit(SqlLibraryEvent::Refreshed);
                cx.notify();
            })
            .ok();
        })
        .detach();
        let root = self.root.clone();
        self.git.update(cx, |git, cx| git.set_root(root, cx));
    }

    fn displayed_tree(&self) -> LibraryTree {
        filter_tree(&self.tree, &self.filter)
    }

    fn selected_folder(&self) -> PathBuf {
        match &self.selected {
            Some(path) if path.is_dir() => path.clone(),
            Some(path) => path
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| self.root.clone()),
            None => self.root.clone(),
        }
    }

    /// Ask the app to save the active editor tab into the selected folder.
    ///
    /// Drives the sidebar's "Save query" action; it used to live in the
    /// overflow menu as "Save current tab here…".
    pub fn request_save_active_tab(&mut self, cx: &mut Context<Self>) {
        let folder = self.selected_folder();
        cx.emit(SqlLibraryEvent::SaveActiveTabRequested { folder });
    }

    pub fn start_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.inline = Some(InlineEdit::NewFolder);
        self.inline_input.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn start_rename(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.inline = Some(InlineEdit::Rename(path));
        self.inline_input.update(cx, |state, cx| {
            state.set_value(&stem, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn commit_inline(&mut self, value: String, _window: &mut Window, cx: &mut Context<Self>) {
        match self.inline.take() {
            Some(InlineEdit::NewFolder) => {
                let name = sanitize_stem(&value);
                let dir = self.selected_folder().join(&name);
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    self.error = Some(e.to_string());
                }
                self.rescan(cx);
            }
            Some(InlineEdit::Rename(path)) => {
                if let Some(dest) = renamed_path(&path, &value) {
                    if let Err(e) = std::fs::rename(&path, &dest) {
                        self.error = Some(e.to_string());
                    }
                    self.selected = Some(dest);
                }
                self.rescan(cx);
            }
            None => {}
        }
        cx.notify();
    }

    fn confirm_delete(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let rx = window.prompt(
            PromptLevel::Warning,
            &crate::localization::format("editor.sql_library.delete_confirm", &[("name", &name)]),
            Some(&crate::localization::text(
                "editor.sql_library.delete_warning",
            )),
            &[
                gpui::PromptButton::new(crate::localization::text(
                    "editor.sql_library.delete_button",
                )),
                gpui::PromptButton::cancel(crate::localization::text(
                    "editor.sql_library.cancel_delete_button",
                )),
            ],
            cx,
        );
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            if let Ok(0) = rx.await {
                let _ = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                let mut cx = async_cx.clone();
                weak.update(&mut cx, |this, cx| {
                    this.rescan(cx);
                })
                .ok();
            }
        })
        .detach();
    }

    fn open_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let fm = self
            .tree
            .entries
            .iter()
            .find(|e| e.path == path)
            .map(|e| e.front_matter.clone())
            .unwrap_or_else(|| {
                std::fs::read_to_string(&path)
                    .map(|t| crate::sql_library_model::parse_front_matter(&t))
                    .unwrap_or_default()
            });
        cx.emit(SqlLibraryEvent::OpenFile(path, fm));
    }

    fn close_menus(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.is_some() || self.overflow_menu.is_some() {
            crate::tooltip::menu_closed(cx);
        }
        self.context_menu = None;
        self.context_menu_dismiss = None;
        self.context_position = None;
        self.overflow_menu = None;
        self.overflow_dismiss = None;
        self.overflow_position = None;
        cx.notify();
    }

    pub fn open_overflow(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_menus(cx);
        let this = cx.entity();
        let view = PopupMenu::build(window, cx, {
            move |menu, _window, _cx| {
                let reveal_this = this.clone();
                let change_this = this.clone();
                let refresh_this = this.clone();
                menu.item(
                    PopupMenuItem::new(crate::localization::text(
                        "editor.sql_library.refresh_menu",
                    ))
                    .on_click(move |_ev, _w, app| {
                        refresh_this.update(app, |panel, cx| panel.rescan(cx));
                    }),
                )
                .separator()
                .item(
                    PopupMenuItem::new(crate::localization::text(
                        "editor.sql_library.reveal_finder_menu",
                    ))
                    .on_click(move |_ev, _w, app| {
                        reveal_this.update(app, |panel, _cx| {
                            let _ = crate::reveal_path::reveal_in_file_manager(panel.root());
                        });
                    }),
                )
                .item(
                    PopupMenuItem::new(crate::localization::text(
                        "editor.sql_library.change_folder_menu",
                    ))
                    .on_click(move |_ev, _w, app| {
                        change_this.update(app, |_panel, cx| {
                            cx.emit(SqlLibraryEvent::RootChangeRequested);
                        });
                    }),
                )
            }
        });
        let dismiss = cx.subscribe(&view, |this, _menu, _: &DismissEvent, cx| {
            this.close_menus(cx);
        });
        // No tooltips while the menu is up — see `crate::tooltip`.
        crate::tooltip::menu_opened(cx);
        self.overflow_menu = Some(view);
        self.overflow_dismiss = Some(dismiss);
        self.overflow_position = Some(position);
        cx.notify();
    }

    fn open_row_menu(
        &mut self,
        path: PathBuf,
        is_folder: bool,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_menus(cx);
        self.selected = Some(path.clone());
        let this = cx.entity();
        let view = PopupMenu::build(window, cx, {
            move |mut menu, _window, _cx| {
                if !is_folder {
                    let open_this = this.clone();
                    let open_path = path.clone();
                    menu = menu.item(
                        PopupMenuItem::new(crate::localization::text(
                            "editor.sql_library.open_menu",
                        ))
                        .on_click(move |_ev, _w, app| {
                            open_this
                                .update(app, |panel, cx| panel.open_file(open_path.clone(), cx));
                        }),
                    );
                }
                let rename_this = this.clone();
                let rename_path = path.clone();
                menu = menu.item(
                    PopupMenuItem::new(crate::localization::text("editor.sql_library.rename_menu"))
                        .on_click(move |_ev, window, app| {
                            rename_this.update(app, |panel, cx| {
                                panel.start_rename(rename_path.clone(), window, cx);
                            });
                        }),
                );
                let delete_this = this.clone();
                let delete_path = path.clone();
                menu = menu.item(
                    PopupMenuItem::new(crate::localization::text("editor.sql_library.delete_menu"))
                        .on_click(move |_ev, window, app| {
                            delete_this.update(app, |panel, cx| {
                                panel.confirm_delete(delete_path.clone(), window, cx);
                            });
                        }),
                );
                let reveal_path = path.clone();
                menu.item(
                    PopupMenuItem::new(crate::localization::text("editor.sql_library.reveal_menu"))
                        .on_click(move |_ev, _w, _app| {
                            crate::reveal_path::reveal_file(&reveal_path);
                        }),
                )
            }
        });
        let dismiss = cx.subscribe(&view, |this, _menu, _: &DismissEvent, cx| {
            this.close_menus(cx);
        });
        // No tooltips while the menu is up — see `crate::tooltip`.
        crate::tooltip::menu_opened(cx);
        self.context_menu = Some(view);
        self.context_menu_dismiss = Some(dismiss);
        self.context_position = Some(position);
        cx.notify();
    }

    fn render_row(&self, row: &LibraryRow, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let show_hint = crate::preferences::SqlLibraryPreferences::load().show_front_matter_hint;
        let (path, label, dim, is_folder, depth) = match row {
            LibraryRow::Folder { relative, depth } => {
                let abs = self.root.join(relative);
                let name = relative
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| relative.display().to_string());
                (abs, name, None, true, *depth)
            }
            LibraryRow::File { entry, depth } => {
                let hint = if show_hint {
                    entry.front_matter.connection.clone()
                } else {
                    None
                };
                (entry.path.clone(), entry.name.clone(), hint, false, *depth)
            }
        };
        let selected = self.selected.as_ref() == Some(&path);
        let expanded = is_folder
            && !self
                .collapsed
                .contains(Path::new(path.strip_prefix(&self.root).unwrap_or(&path)));
        let relative = path
            .strip_prefix(&self.root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.clone());
        let row_h = Theme::row_height();
        let click_path = path.clone();
        let menu_path = path.clone();
        let toggle_rel = relative.clone();

        div()
            .id(format!("lib-row:{}", path.to_string_lossy()))
            .h(px(row_h))
            .w_full()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(4.0)))
            .pl(px(s(8.0 + depth as f32 * 12.0)))
            .pr(px(s(8.0)))
            .cursor_pointer()
            .bg(if selected {
                Theme::selection_fill()
            } else {
                gpui::transparent_black()
            })
            .hover(|d| {
                if selected {
                    d
                } else {
                    d.bg(Theme::row_hover())
                }
            })
            .on_click(
                cx.listener(move |this, ev: &gpui::ClickEvent, _window, cx| {
                    this.selected = Some(click_path.clone());
                    if is_folder {
                        if this.collapsed.contains(&toggle_rel) {
                            this.collapsed.remove(&toggle_rel);
                        } else {
                            this.collapsed.insert(toggle_rel.clone());
                        }
                    } else if ev.click_count() >= 2 {
                        this.open_file(click_path.clone(), cx);
                    }
                    cx.notify();
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    this.open_row_menu(menu_path.clone(), is_folder, ev.position, window, cx);
                }),
            )
            .when(is_folder, |d| {
                d.child(crate::icons::disclosure_chevron(expanded))
            })
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .whitespace_nowrap()
                    .overflow_hidden()
                    .child(label),
            )
            .when_some(dim, |d, hint| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .whitespace_nowrap()
                        .child(hint),
                )
            })
            .into_any_element()
    }
}

impl Render for SqlLibraryPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let tree = self.displayed_tree();
        let rows = visible_rows(&tree, &self.collapsed);
        let creating = matches!(self.inline, Some(InlineEdit::NewFolder));

        let mut list = div()
            .id("lib-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.list_scroll);

        if let Some(err) = &self.error {
            list = list.child(
                div()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .child(err.clone()),
            );
        }

        if creating {
            list = list.child(
                div()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .child(Input::new(&self.inline_input).small()),
            );
        }

        if matches!(self.inline, Some(InlineEdit::Rename(_))) {
            list = list.child(
                div()
                    .px(px(s(8.0)))
                    .py(px(s(4.0)))
                    .child(Input::new(&self.inline_input).small()),
            );
        }

        if rows.is_empty() && !creating {
            list = list.child(
                div()
                    .flex_1()
                    .p(px(s(16.0)))
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size_ratio(0.92)))
                    .child(crate::localization::format(
                        "editor.sql_library.empty",
                        &[("path", &self.root.display().to_string())],
                    )),
            );
        }

        for row in &rows {
            list = list.child(self.render_row(row, cx));
        }

        let mut chrome = div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::sidebar_bg())
            .track_focus(&self.focus_handle)
            .child(list)
            .child(self.git.clone());

        if self.git.read(cx).diff_open() {
            let dialog = self
                .git
                .update(cx, |git, cx| git.render_diff_dialog(window, cx));
            chrome = chrome.child(dialog);
        }

        let menu_slot = self
            .context_menu
            .clone()
            .zip(self.context_position)
            .or_else(|| self.overflow_menu.clone().zip(self.overflow_position));
        if let Some((menu, position)) = menu_slot {
            let dismiss_entity = cx.entity();
            let overlay = deferred(anchored().position(point(position.x, position.y)).child(
                div().occlude().child(menu).on_mouse_down_out(
                    move |_ev: &MouseDownEvent, _window, app| {
                        dismiss_entity.update(app, |this, cx| this.close_menus(cx));
                    },
                ),
            ))
            .with_priority(1);
            chrome = chrome.child(overlay);
        }
        chrome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql_library_model::LibraryEntry;
    use std::time::SystemTime;

    fn entry(folder: &str, name: &str) -> LibraryEntry {
        let relative = if folder.is_empty() {
            PathBuf::from(format!("{name}.sql"))
        } else {
            PathBuf::from(folder).join(format!("{name}.sql"))
        };
        LibraryEntry {
            path: PathBuf::from("/lib").join(&relative),
            relative,
            name: name.to_string(),
            folder: if folder.is_empty() {
                PathBuf::new()
            } else {
                PathBuf::from(folder)
            },
            modified: Some(SystemTime::UNIX_EPOCH),
            front_matter: FrontMatter::default(),
        }
    }

    #[test]
    fn collapsed_folders_hide_children() {
        let tree = LibraryTree {
            folders: vec![PathBuf::from("reports"), PathBuf::from("reports/daily")],
            entries: vec![
                entry("", "root"),
                entry("reports", "monthly"),
                entry("reports/daily", "today"),
            ],
        };
        let open = visible_rows(&tree, &HashSet::new());
        assert!(
            open.iter()
                .any(|r| matches!(r, LibraryRow::File { entry, .. } if entry.name == "today"))
        );
        let mut collapsed = HashSet::new();
        collapsed.insert(PathBuf::from("reports"));
        let hidden = visible_rows(&tree, &collapsed);
        assert!(hidden.iter().any(
            |r| matches!(r, LibraryRow::Folder { relative, .. } if relative == Path::new("reports"))
        ));
        assert!(
            !hidden
                .iter()
                .any(|r| matches!(r, LibraryRow::File { entry, .. } if entry.name == "monthly"))
        );
        assert!(
            !hidden
                .iter()
                .any(|r| matches!(r, LibraryRow::File { entry, .. } if entry.name == "today"))
        );
        assert!(
            hidden
                .iter()
                .any(|r| matches!(r, LibraryRow::File { entry, .. } if entry.name == "root"))
        );
    }

    /// The app's layout around the library: a narrow sidebar that clips its
    /// body, beside the editor area.
    struct NarrowSidebarHost {
        library: Entity<SqlLibraryPanel>,
    }

    const RAIL_W: f32 = 48.0;
    const SIDEBAR_W: f32 = 240.0;

    impl Render for NarrowSidebarHost {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_row()
                // The activity rail left of the sidebar.
                .child(div().w(px(RAIL_W)).h_full().flex_none())
                .child(
                    div()
                        .w(px(SIDEBAR_W))
                        .h_full()
                        .flex_none()
                        .overflow_hidden()
                        .child(self.library.clone()),
                )
                .child(div().flex_1())
        }
    }

    /// The Git section's Diff dialog used to be laid out inside the sidebar:
    /// offset by the sidebar's origin and clipped to its 240px box. It must be
    /// centered on the window with the whole card on screen, and its modal
    /// scrim must cover the window (a click far outside the sidebar dismisses
    /// it, which a sidebar-clipped scrim never receives).
    #[gpui::test]
    fn library_git_diff_is_laid_out_against_the_window(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("library");
        let (host, cx) = cx.add_window_view(|window, cx| {
            let library = cx.new(|cx| SqlLibraryPanel::new(root.clone(), window, cx));
            NarrowSidebarHost { library }
        });
        cx.run_until_parked();
        host.update(cx, |host, cx| {
            host.library.update(cx, |library, cx| {
                library.git.update(cx, |git, cx| {
                    git.show_diff_for_test("a.sql", "@@ -1 +1 @@\n-old\n+new\n ctx\n", cx)
                })
            })
        });
        cx.run_until_parked();

        let viewport = cx.update(|window, _| window.viewport_size());
        let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));
        let card = cx
            .debug_bounds("dialog-card-library-git-diff-dialog")
            .expect("the diff dialog is laid out");
        let (left, right) = (f32::from(card.left()), f32::from(card.right()));
        let (top, bottom) = (f32::from(card.top()), f32::from(card.bottom()));
        assert!(
            vw > f32::from(card.size.width) + SIDEBAR_W,
            "precondition: the test window ({vw}) is wide enough to hold the card"
        );
        assert!(
            left >= 0.0 && right <= vw && top >= 0.0 && bottom <= vh,
            "card {card:?} must sit fully inside the {vw}x{vh} window"
        );
        assert!(
            ((left + right) / 2.0 - vw / 2.0).abs() < 2.0,
            "card {card:?} is centered on the window, not on the {SIDEBAR_W}px sidebar"
        );

        // Scrim, top-right corner: outside the sidebar and outside the card.
        cx.simulate_click(
            gpui::point(px(vw - 4.0), px(4.0)),
            gpui::Modifiers::default(),
        );
        cx.run_until_parked();
        let still_open = host.update(cx, |host, cx| {
            host.library.read(cx).git.read(cx).diff_open()
        });
        assert!(
            !still_open,
            "the scrim spans the window, so clicking it outside the sidebar closes the diff"
        );
    }

    #[test]
    fn rename_keeps_extension() {
        let path = PathBuf::from("/lib/reports/daily.sql");
        assert_eq!(
            renamed_path(&path, "weekly").as_deref(),
            Some(Path::new("/lib/reports/weekly.sql"))
        );
        assert_eq!(
            renamed_path(&path, "weekly.sql").as_deref(),
            Some(Path::new("/lib/reports/weekly.sql"))
        );
        assert!(renamed_path(&path, "a/b").is_none());
    }
}
