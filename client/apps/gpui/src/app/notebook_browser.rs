//! Notebook files in the browser build (see `crate::notebook::browser`):
//! Open Notebook picks a file on the computer and keeps a copy in this
//! browser; Save writes that copy and, when the browser handed the page the
//! file's handle, the file too; Save As chooses a file where the browser can
//! (File System Access) and downloads one where it cannot.

use super::*;
use crate::notebook::browser;
use sqlly_services::browser::notebook_files::{self, FileError, PickedFile};

fn file_error_text(error: &FileError) -> String {
    match error {
        FileError::TooLarge { limit_mb } => crate::localization::format(
            "notebook.browser.open.too_large",
            &[("limit_mb", limit_mb.to_string())],
        ),
        FileError::Permission => crate::localization::text("notebook.browser.save.permission"),
        FileError::NoHandle => crate::localization::text("notebook.browser.save.no_handle"),
        FileError::Failed(detail) => crate::localization::format(
            "notebook.browser.file_failed",
            &[("error", detail.as_str())],
        ),
    }
}

fn file_type() -> String {
    crate::localization::text("notebook.browser.file_type")
}

/// The status after keeping a notebook in this browser, noting when the
/// browser will not keep it past this tab.
fn kept(message: String) -> String {
    if browser::persists() {
        message
    } else {
        format!(
            "{message} {}",
            crate::localization::text("notebook.browser.storage.session_only")
        )
    }
}

impl AppView {
    pub(super) fn open_notebook_in_browser(&mut self, cx: &mut Context<Self>) {
        let pick = notebook_files::pick_file(file_type());
        cx.spawn(async move |this, cx| {
            let picked = pick.await;
            this.update(cx, |this, cx| this.open_picked_notebook(picked, cx))
                .ok();
        })
        .detach();
    }

    fn open_picked_notebook(
        &mut self,
        picked: Result<Option<PickedFile>, FileError>,
        cx: &mut Context<Self>,
    ) {
        let file = match picked {
            Ok(Some(file)) => file,
            Ok(None) => return,
            Err(error) => {
                self.set_status(StatusLevel::Error, file_error_text(&error));
                cx.notify();
                return;
            }
        };
        let db = self.active_profile.as_ref().map(|p| p.database_type);
        let loaded = match crate::notebook::files::load_file(&file.name, &file.text, db) {
            Ok(loaded) => loaded,
            Err(message) => {
                self.set_status(StatusLevel::Error, message);
                cx.notify();
                return;
            }
        };
        let path = browser::library_path(&loaded.doc.notebook_id, &file.name);
        let stored = crate::notebook::files::write_text(&path, &file.text);
        browser::attach_handle(&path, file.handle);
        self.note_recent_file(&path);
        let index = self.new_notebook_tab(loaded.doc, Some(path), cx);
        self.note_scan_warnings(index, &file.text, cx);
        let message = match stored {
            Ok(()) => kept(crate::localization::format(
                "notebook.browser.open.opened",
                &[("file_name", file.name.as_str())],
            )),
            Err(error) => crate::localization::format(
                "notebook.browser.storage.write_failed",
                &[("error", error.to_string())],
            ),
        };
        self.set_status(StatusLevel::Info, message);
        cx.notify();
    }

    pub(super) fn save_notebook_in_browser(
        &mut self,
        tab_idx: usize,
        save_as: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.tabs.get(tab_idx) else {
            return;
        };
        let Some(notebook) = tab.notebook.clone() else {
            return;
        };
        let notebook_path = tab.notebook_path.clone();
        let tab_name = tab.name.clone();
        let save_as = save_as
            || notebook_path.as_deref().is_some_and(|path| {
                crate::notebook::files::is_legacy_path(path) || !browser::is_browser_path(path)
            });
        if let Some(path) = notebook_path.as_deref().filter(|_| !save_as)
            && !notebook.update(cx, |nb, cx| nb.may_overwrite_file(cx))
        {
            self.set_status(
                StatusLevel::Warning,
                crate::localization::format(
                    "notebook.fu_core.save.changed_in_another_tab",
                    &[("path", path)],
                ),
            );
            cx.notify();
            return;
        }
        let model = notebook.update(cx, |nb, cx| {
            nb.take_own_identity(cx);
            nb.model_for_save()
        });
        if let Some(path) = notebook_path.clone().filter(|_| !save_as) {
            let text = crate::notebook::files::file_text(&path, &model);
            if let Err(error) = crate::notebook::files::write_text(&path, &text) {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.files.notebook_browser_save_failed",
                        &[("error", &error.to_string())],
                    ),
                );
                cx.notify();
                return;
            }
            self.finish_browser_save(tab_idx, path, None, text, cx);
            return;
        }
        let extension = crate::notebook::files::save_extension(notebook_path.as_deref());
        let file_name = format!("{}.{extension}", file_stem(&tab_name));
        if notebook_files::can_save_files() {
            let choose = notebook_files::choose_save_file(file_name, file_type());
            cx.spawn(async move |this, cx| {
                let chosen = choose.await;
                this.update(cx, |this, cx| match chosen {
                    Ok(Some(saved)) => {
                        let path = browser::library_path(&model.notebook_id, &saved.name);
                        browser::attach_handle(&path, Some(saved.handle));
                        this.keep_saved_notebook(tab_idx, path, &model, cx);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        this.set_status(StatusLevel::Error, file_error_text(&error));
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
            return;
        }
        let path = browser::library_path(&model.notebook_id, &file_name);
        let text = crate::notebook::files::file_text(&path, &model);
        if let Err(message) = crate::browser_download::download_bytes(&file_name, text.as_bytes()) {
            self.set_status(StatusLevel::Error, message);
            cx.notify();
            return;
        }
        self.keep_saved_notebook(tab_idx, path, &model, cx);
    }

    /// Save As landed: keep the browser copy at `path` and point the tab at it.
    fn keep_saved_notebook(
        &mut self,
        tab_idx: usize,
        path: String,
        model: &sqlly_services::notebook::NotebookDoc,
        cx: &mut Context<Self>,
    ) {
        let text = crate::notebook::files::file_text(&path, model);
        if let Err(error) = crate::notebook::files::write_text(&path, &text) {
            self.set_status(
                StatusLevel::Error,
                crate::localization::format(
                    "notebook.browser.storage.write_failed",
                    &[("error", error.to_string())],
                ),
            );
            cx.notify();
            return;
        }
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.notebook_path = Some(path.clone());
            tab.auto_named = false;
            tab.auto_number = None;
            if !tab.user_named {
                tab.name = std::path::Path::new(&path)
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| tab.name.clone());
            }
        }
        self.note_recent_file(&path);
        let downloaded = browser::handle_for(&path).is_none();
        self.finish_browser_save(tab_idx, path.clone(), Some(downloaded), text, cx);
    }

    /// The browser copy at `path` holds the notebook: mark the tab saved and
    /// write the computer's file through its handle, if the page has one.
    /// `downloaded` is `Some(true)` when Save As downloaded the file.
    fn finish_browser_save(
        &mut self,
        tab_idx: usize,
        path: String,
        downloaded: Option<bool>,
        text: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.get_mut(tab_idx) {
            tab.dirty = false;
        }
        if let Some(nb) = self.tabs.get(tab_idx).and_then(|t| t.notebook.clone()) {
            let saved = path.clone();
            nb.update(cx, |nb, cx| nb.saved_to(saved, cx));
        }
        self.save_workspace_session(cx);
        let file_name = std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let Some(handle) = browser::handle_for(&path) else {
            let key = if downloaded == Some(true) {
                "notebook.browser.save.downloaded"
            } else {
                "notebook.browser.save.saved_in_browser"
            };
            self.set_status(
                StatusLevel::Success,
                kept(crate::localization::format(
                    key,
                    &[("file_name", file_name.as_str())],
                )),
            );
            cx.notify();
            return;
        };
        let write = notebook_files::save_file(handle, text, true);
        cx.spawn(async move |this, cx| {
            let written = write.await;
            this.update(cx, |this, cx| {
                match written {
                    Ok(()) => this.set_status(
                        StatusLevel::Success,
                        crate::localization::format(
                            "notebook.browser.save.saved_to_file",
                            &[("file_name", file_name.as_str())],
                        ),
                    ),
                    Err(error) => this.set_status(
                        StatusLevel::Warning,
                        crate::localization::format(
                            "notebook.browser.save.file_not_written",
                            &[
                                ("file_name", file_name.clone()),
                                ("error", file_error_text(&error)),
                            ],
                        ),
                    ),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

/// A file name stem from a tab title.
fn file_stem(title: &str) -> String {
    let stem: String = title
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let stem = stem.trim().trim_start_matches('.').trim();
    if stem.is_empty() {
        "notebook".to_string()
    } else {
        stem.to_string()
    }
}
