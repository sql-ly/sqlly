//! Reusable git surface for a folder of scripts: status list, diff preview,
//! and commit with a message. Hosted by the SQL library sidebar (library
//! root) and by the Project Git window (the folder opened with File › Open
//! Folder…). All git work goes through [`crate::library_git`], off the UI
//! thread; mutating verbs (init, stage + commit) only ever run from buttons.
//!
//! Desktop-only: shells out to the `git` CLI.

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};

use gpui::{
    AnyElement, Context, Entity, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};

use crate::theme::Theme;

fn localized_format<T: std::fmt::Display>(key: &str, arguments: &[(&str, T)]) -> String {
    crate::localization::format(key, arguments)
}

/// One background status read, as the panel folds it in.
/// Messages are keyed: the read runs on a background executor and the panel
/// resolves them on the UI thread.
enum RefreshOutcome {
    GitUnavailable(sqlly_core::DisplayMessage),
    Read {
        is_repo: bool,
        changes: Vec<crate::library_git::GitChange>,
        error: Option<sqlly_core::DisplayMessage>,
    },
}

/// The read-only diff preview's prepared content.
pub(crate) struct GitDiffPreview {
    title: String,
    /// Classified lines, capped at [`GIT_DIFF_LINE_CAP`].
    lines: Vec<(crate::library_git::DiffLineKind, String)>,
    /// Honest stand-in when there is no diff (untracked file, empty diff).
    note: Option<String>,
}

/// Diff preview size bound: the dialog renders every line, so a giant diff is
/// truncated with a note rather than freezing the sidebar.
const GIT_DIFF_LINE_CAP: usize = 4_000;

impl GitDiffPreview {
    fn from_text(path: &str, text: &str) -> Self {
        if text.trim().is_empty() {
            return Self::note(
                path,
                &crate::localization::text("explorer.git_panel.diff.no_differences"),
            );
        }
        let mut lines: Vec<(crate::library_git::DiffLineKind, String)> = text
            .lines()
            .take(GIT_DIFF_LINE_CAP)
            .map(|line| (crate::library_git::diff_line_kind(line), line.to_string()))
            .collect();
        let total = text.lines().count();
        if total > GIT_DIFF_LINE_CAP {
            let shown = GIT_DIFF_LINE_CAP.to_string();
            let total = total.to_string();
            lines.push((
                crate::library_git::DiffLineKind::Meta,
                localized_format(
                    "explorer.git_panel.diff.truncated",
                    &[("shown", &shown), ("total", &total)],
                ),
            ));
        }
        Self {
            title: localized_format("explorer.git_panel.diff.title", &[("path", path)]),
            lines,
            note: None,
        }
    }

    fn note(path: &str, note: &str) -> Self {
        Self {
            title: localized_format("explorer.git_panel.diff.title", &[("path", path)]),
            lines: Vec::new(),
            note: Some(note.to_string()),
        }
    }
}

/// Git status / diff / commit for one folder.
pub struct GitPanel {
    root: PathBuf,
    /// Element-id namespace, so two hosts never collide ("library-git",
    /// "project-git").
    id_prefix: &'static str,
    /// Is the folder inside a work tree, and what changed.
    git_repo: bool,
    /// `git` itself could not be run (not installed / not on PATH): the
    /// panel says so instead of offering an init that would fail.
    git_unavailable: Option<String>,
    git_changes: Vec<crate::library_git::GitChange>,
    git_selected: Option<String>,
    /// Error from the last user action (init, commit, diff). Kept until the
    /// next action or an explicit Refresh.
    git_error: Option<String>,
    /// Error from the last status read. Replaced by every refresh, so a
    /// transient failure does not outlive a later successful read.
    status_error: Option<String>,
    /// Bumped on every refresh; a status result from an older refresh
    /// (e.g. of the previous folder after `set_root`) is dropped.
    refresh_generation: u64,
    /// Bumped whenever the panel is pointed at another folder; action results
    /// (commit, diff, init) for the old folder are dropped.
    root_generation: u64,
    git_notice: Option<String>,
    git_busy: bool,
    git_message: Entity<InputState>,
    git_message_text: String,
    /// A background commit finished; the message field clears at the next
    /// render (the completion path has no `&mut Window` for `set_value`).
    git_message_clear_pending: bool,
    git_diff: Option<GitDiffPreview>,
    diff_frame: crate::dialog::DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl GitPanel {
    pub fn new(
        root: PathBuf,
        id_prefix: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_message = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.git_panel.commit.placeholder",
            ))
        });
        let subscriptions = vec![cx.subscribe_in(
            &git_message,
            window,
            |this, input, event: &InputEvent, _window, cx| {
                if let InputEvent::Change = event {
                    this.git_message_text = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )];
        let mut this = Self {
            root,
            id_prefix,
            git_repo: false,
            git_unavailable: None,
            git_changes: Vec::new(),
            git_selected: None,
            git_error: None,
            status_error: None,
            refresh_generation: 0,
            root_generation: 0,
            git_notice: None,
            git_busy: false,
            git_message,
            git_message_text: String::new(),
            git_message_clear_pending: false,
            git_diff: None,
            diff_frame: crate::dialog::DialogFrame::new(Theme::scaled(720.0), Theme::scaled(520.0)),
            _subscriptions: subscriptions,
        };
        this.refresh(cx);
        this
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Point the panel at another folder: selection, notices, and any open
    /// diff belong to the old folder and are dropped.
    pub fn set_root(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if root == self.root {
            self.refresh(cx);
            return;
        }
        self.root = root;
        self.root_generation += 1;
        // Any in-flight action for the old folder no longer owns the busy
        // flag; its result is dropped by the generation check.
        self.git_busy = false;
        self.git_repo = false;
        self.git_selected = None;
        self.git_error = None;
        self.status_error = None;
        self.git_notice = None;
        self.git_diff = None;
        self.git_changes.clear();
        self.refresh(cx);
    }

    /// Whether the diff preview dialog is open. The host mounts
    /// [`GitPanel::render_diff_dialog`] beside the panel; the dialog lifts
    /// itself to a window-level layer, so a clipping host (the sidebar) never
    /// cuts it off.
    pub fn diff_open(&self) -> bool {
        self.git_diff.is_some()
    }

    /// Open the diff dialog on prepared text, as a finished `git diff` would.
    #[cfg(test)]
    pub(crate) fn show_diff_for_test(&mut self, path: &str, text: &str, cx: &mut Context<Self>) {
        self.git_diff = Some(GitDiffPreview::from_text(path, text));
        cx.notify();
    }

    fn id(&self, suffix: &str) -> SharedString {
        element_id(self.id_prefix, suffix)
    }

    /// Re-read the git status for the panel's folder (read-only, safe to run
    /// any time). Keeps any selection whose file is still changed.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        let root = self.root.clone();
        let task = cx.background_spawn(async move {
            let git = crate::library_git::LibraryGit::new(crate::library_git::CliGitRunner, root);
            match git.probe() {
                crate::library_git::RepoProbe::Repo => match git.status() {
                    Ok(changes) => RefreshOutcome::Read {
                        is_repo: true,
                        changes,
                        error: None,
                    },
                    Err(error) => RefreshOutcome::Read {
                        is_repo: true,
                        changes: Vec::new(),
                        error: Some(error),
                    },
                },
                crate::library_git::RepoProbe::NotRepo => RefreshOutcome::Read {
                    is_repo: false,
                    changes: Vec::new(),
                    error: None,
                },
                crate::library_git::RepoProbe::GitUnavailable(why) => {
                    RefreshOutcome::GitUnavailable(why)
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |this, cx| {
                if this.accept_refresh(generation, outcome) {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Fold the status read started as refresh `generation` into the panel,
    /// unless a newer refresh (or a re-pointed folder) superseded it — then
    /// it is dropped so an old folder's slower read cannot overwrite the new
    /// list. Returns whether it was applied.
    fn accept_refresh(&mut self, generation: u64, outcome: RefreshOutcome) -> bool {
        if self.refresh_generation != generation {
            return false;
        }
        self.apply_refresh(outcome);
        true
    }

    /// Fold one status read into the panel. A successful read replaces any
    /// earlier status error; action errors (commit, init, diff) are separate
    /// and survive it.
    fn apply_refresh(&mut self, outcome: RefreshOutcome) {
        match outcome {
            RefreshOutcome::GitUnavailable(why) => {
                self.git_repo = false;
                self.git_unavailable = Some(crate::localization::message(why));
                self.git_changes.clear();
                self.git_selected = None;
                self.status_error = None;
            }
            RefreshOutcome::Read {
                is_repo,
                changes,
                error,
            } => {
                self.git_repo = is_repo;
                self.git_unavailable = None;
                if let Some(selected) = &self.git_selected
                    && !changes.iter().any(|c| &c.path == selected)
                {
                    self.git_selected = None;
                }
                self.git_changes = changes;
                self.status_error = error.map(crate::localization::message);
            }
        }
    }

    /// `git init` the folder — the one-click answer to "not a repo".
    /// Button-driven only.
    fn initialize_git(&mut self, cx: &mut Context<Self>) {
        if self.git_busy {
            return;
        }
        self.git_busy = true;
        self.git_error = None;
        self.git_notice = None;
        cx.notify();
        let root = self.root.clone();
        let generation = self.root_generation;
        let task = cx.background_spawn(async move {
            crate::library_git::LibraryGit::new(crate::library_git::CliGitRunner, root).init()
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.root_generation != generation {
                    return;
                }
                this.git_busy = false;
                match result {
                    Ok(()) => {
                        this.git_notice = Some(crate::localization::text(
                            "explorer.git_panel.initialize.success",
                        ));
                    }
                    Err(error) => this.git_error = Some(crate::localization::message(error)),
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Stage + commit — all changes (`scope_all`) or just the selected file.
    /// Button-driven only; the argv builders refuse a blank message before
    /// git ever runs.
    fn commit_git(&mut self, scope_all: bool, cx: &mut Context<Self>) {
        if self.git_busy {
            return;
        }
        let message = self.git_message_text.trim().to_string();
        if message.is_empty() {
            self.git_error = Some(crate::localization::text(
                "explorer.git_panel.commit.message_required",
            ));
            cx.notify();
            return;
        }
        let paths: Option<Vec<String>> = if scope_all {
            None
        } else {
            match &self.git_selected {
                Some(path) => {
                    // A rename stages both sides so git records it as one.
                    let mut list = vec![path.clone()];
                    if let Some(orig) = self
                        .git_changes
                        .iter()
                        .find(|c| &c.path == path)
                        .and_then(|c| c.orig_path.clone())
                    {
                        list.push(orig);
                    }
                    Some(list)
                }
                None => {
                    self.git_error = Some(crate::localization::text(
                        "explorer.git_panel.commit.selection_required",
                    ));
                    cx.notify();
                    return;
                }
            }
        };
        self.git_busy = true;
        self.git_error = None;
        self.git_notice = None;
        cx.notify();
        let root = self.root.clone();
        let generation = self.root_generation;
        let task = cx.background_spawn(async move {
            let git = crate::library_git::LibraryGit::new(crate::library_git::CliGitRunner, root);
            git.stage(paths.as_deref())
                .and_then(|()| git.commit(&message, paths.as_deref()))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.root_generation != generation {
                    return;
                }
                this.git_busy = false;
                match result {
                    Ok(summary) => {
                        let summary = summary.lines().next().unwrap_or("");
                        this.git_notice = Some(localized_format(
                            "explorer.git_panel.commit.success",
                            &[("summary", summary)],
                        ));
                        this.git_message_text.clear();
                        this.git_message_clear_pending = true;
                    }
                    Err(error) => this.git_error = Some(crate::localization::message(error)),
                }
                this.refresh(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the read-only diff preview for the selected change.
    fn open_git_diff(&mut self, cx: &mut Context<Self>) {
        if self.git_busy {
            return;
        }
        let Some(path) = self.git_selected.clone() else {
            return;
        };
        if let Some(change) = self.git_changes.iter().find(|c| c.path == path)
            && change.is_untracked()
        {
            self.git_diff = Some(GitDiffPreview::note(
                &change.display_path(),
                &crate::localization::text("explorer.git_panel.diff.untracked"),
            ));
            cx.notify();
            return;
        }
        self.git_busy = true;
        cx.notify();
        let root = self.root.clone();
        let generation = self.root_generation;
        let task = cx.background_spawn(async move {
            crate::library_git::LibraryGit::new(crate::library_git::CliGitRunner, root)
                .diff(&path)
                .map(|text| (path, text))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.root_generation != generation {
                    return;
                }
                this.git_busy = false;
                match result {
                    Ok((path, text)) => {
                        this.git_error = None;
                        this.git_diff = Some(GitDiffPreview::from_text(&path, &text));
                    }
                    Err(error) => this.git_error = Some(crate::localization::message(error)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn render_diff_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        let Some(diff) = &self.git_diff else {
            return div().into_any_element();
        };
        let body: AnyElement = if let Some(note) = &diff.note {
            div()
                .p(px(s(14.0)))
                .text_size(px(Theme::font_size_ratio(0.88)))
                .text_color(Theme::text_dim())
                .whitespace_normal()
                .child(note.clone())
                .into_any_element()
        } else {
            div()
                .id(self.id("diff-body"))
                .flex_1()
                .min_h(px(0.0))
                .overflow_y_scroll()
                .px(px(s(12.0)))
                .py(px(s(8.0)))
                .flex()
                .flex_col()
                .children(diff.lines.iter().map(|(kind, line)| {
                    let color = match kind {
                        crate::library_git::DiffLineKind::Add => Theme::success(),
                        crate::library_git::DiffLineKind::Remove => Theme::error(),
                        crate::library_git::DiffLineKind::Hunk => Theme::accent(),
                        crate::library_git::DiffLineKind::Meta => Theme::text_dim(),
                        crate::library_git::DiffLineKind::Context => Theme::text(),
                    };
                    div()
                        .font_family(Theme::editor_font_family())
                        .text_size(px(Theme::font_size_ratio(0.78)))
                        .text_color(color)
                        .child(line.clone())
                }))
                .into_any_element()
        };
        let dialog = crate::dialog::Dialog::new(self.id("diff-dialog"), |this: &mut Self| {
            &mut this.diff_frame
        })
        .title(diff.title.clone())
        .easy_close(true)
        .key_context("LibraryGitDiff")
        .bare_body()
        .on_close(|this, _window, cx| {
            this.git_diff = None;
            cx.notify();
        })
        .child(body)
        .build(self, window, cx);
        // The library host is a narrow sidebar that clips its children: lift
        // the scrim and card to the window so the diff is centered on (and
        // sized against) the whole window, as in the Project Git window.
        crate::dialog::window_layer(dialog, window)
    }

    /// The Git section: repo state, the
    /// changed-files list, the diff verb, and commit. Mutating verbs are
    /// buttons here and nowhere else.
    fn render_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        let mut section = div()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .px(px(s(8.0)))
                    .py(px(s(5.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child("GIT"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .truncate()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(if self.git_unavailable.is_some() {
                                crate::localization::text("explorer.git_panel.status.unavailable")
                            } else if !self.git_repo {
                                crate::localization::text(
                                    "explorer.git_panel.status.not_repository",
                                )
                            } else if self.git_changes.is_empty() {
                                crate::localization::text("explorer.git_panel.status.no_changes")
                            } else {
                                let count = self.git_changes.len().to_string();
                                localized_format(
                                    "explorer.git_panel.status.changed_count",
                                    &[("count", &count)],
                                )
                            }),
                    )
                    .child(
                        Button::new(self.id("refresh"))
                            .ghost()
                            .xsmall()
                            .label(crate::localization::text(
                                "explorer.git_panel.actions.refresh",
                            ))
                            .disabled(self.git_busy)
                            .on_click(cx.listener(|this, _ev, _w, cx| {
                                // An explicit Refresh also dismisses the last
                                // action's error and notice.
                                this.git_error = None;
                                this.git_notice = None;
                                this.refresh(cx);
                            })),
                    ),
            );

        if let Some(why) = &self.git_unavailable {
            section = section.child(
                div()
                    .px(px(s(8.0)))
                    .pb(px(s(8.0)))
                    .text_size(px(Theme::font_size_ratio(0.84)))
                    .text_color(Theme::text_dim())
                    .whitespace_normal()
                    .child(why.clone()),
            );
            return self.render_git_notices(section).into_any_element();
        }

        if !self.git_repo {
            section = section.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(6.0)))
                    .px(px(s(8.0)))
                    .pb(px(s(8.0)))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.84)))
                            .text_color(Theme::text_dim())
                            .whitespace_normal()
                            .child(crate::localization::text(
                                "explorer.git_panel.initialize.guidance",
                            )),
                    )
                    .child(
                        Button::new(self.id("init"))
                            .xsmall()
                            .label(crate::localization::text(
                                "explorer.git_panel.initialize.button",
                            ))
                            .disabled(self.git_busy)
                            .on_click(cx.listener(|this, _ev, _w, cx| this.initialize_git(cx))),
                    ),
            );
            return self.render_git_notices(section).into_any_element();
        }

        if !self.git_changes.is_empty() {
            let mut list = div()
                .id(self.id("changes"))
                .max_h(px(s(150.0)))
                .overflow_y_scroll()
                .flex()
                .flex_col();
            for change in &self.git_changes {
                let path = change.path.clone();
                let selected = self.git_selected.as_ref() == Some(&change.path);
                list = list.child(
                    div()
                        .id(SharedString::from(format!(
                            "{}-row:{}",
                            self.id_prefix, change.path
                        )))
                        .h(px(Theme::row_height()))
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(6.0)))
                        .px(px(s(8.0)))
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
                        .on_click(cx.listener(move |this, _ev, _w, cx| {
                            this.git_selected = Some(path.clone());
                            cx.notify();
                        }))
                        .child(
                            div()
                                .w(px(s(24.0)))
                                .flex_shrink_0()
                                .font_family(Theme::editor_font_family())
                                .text_size(px(Theme::font_size_ratio(0.74)))
                                .text_color(if change.is_untracked() {
                                    Theme::text_dim()
                                } else if change.is_staged() {
                                    Theme::success()
                                } else {
                                    Theme::warning()
                                })
                                .child(change.badge()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(Theme::font_size_ratio(0.84)))
                                .text_color(Theme::text())
                                .child(change.display_path()),
                        ),
                );
            }
            section = section.child(list);

            let has_selection = self.git_selected.is_some();
            let has_message = !self.git_message_text.trim().is_empty();
            section = section.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(6.0)))
                    .px(px(s(8.0)))
                    .py(px(s(6.0)))
                    .child(Input::new(&self.git_message).small())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap(px(s(4.0)))
                            .child(
                                Button::new(self.id("diff"))
                                    .ghost()
                                    .xsmall()
                                    .label(crate::localization::text(
                                        "explorer.git_panel.actions.diff",
                                    ))
                                    .disabled(!has_selection || self.git_busy)
                                    .on_click(cx.listener(|this, _ev, _w, cx| {
                                        this.open_git_diff(cx);
                                    })),
                            )
                            .child(
                                Button::new(self.id("commit-selected"))
                                    .xsmall()
                                    .label(crate::localization::text(
                                        "explorer.git_panel.actions.commit_selected",
                                    ))
                                    .disabled(!has_selection || !has_message || self.git_busy)
                                    .on_click(cx.listener(|this, _ev, _w, cx| {
                                        this.commit_git(false, cx);
                                    })),
                            )
                            .child(
                                Button::new(self.id("commit-all"))
                                    .xsmall()
                                    .label(crate::localization::text(
                                        "explorer.git_panel.actions.commit_all",
                                    ))
                                    .disabled(!has_message || self.git_busy)
                                    .on_click(cx.listener(|this, _ev, _w, cx| {
                                        this.commit_git(true, cx);
                                    })),
                            ),
                    ),
            );
        }
        self.render_git_notices(section).into_any_element()
    }

    /// Error / notice text under the git controls, if any.
    fn render_git_notices(&self, section: gpui::Div) -> gpui::Div {
        let s = Theme::scaled;
        let mut section = section;
        for error in [&self.git_error, &self.status_error].into_iter().flatten() {
            section = section.child(
                div()
                    .px(px(s(8.0)))
                    .pb(px(s(6.0)))
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::error())
                    .whitespace_normal()
                    .child(error.clone()),
            );
        }
        if let Some(notice) = &self.git_notice {
            section = section.child(
                div()
                    .px(px(s(8.0)))
                    .pb(px(s(6.0)))
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .whitespace_normal()
                    .child(notice.clone()),
            );
        }
        section
    }
}

/// `<prefix>-<suffix>` element id.
fn element_id(prefix: &str, suffix: &str) -> SharedString {
    SharedString::from(format!("{prefix}-{suffix}"))
}

impl Render for GitPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A background commit cleared the message; the field follows here,
        // where a `&mut Window` exists (`set_value` needs one).
        if self.git_message_clear_pending {
            self.git_message_clear_pending = false;
            self.git_message
                .update(cx, |state, cx| state.set_value("", window, cx));
        }
        self.render_section(cx)
    }
}

/// A standalone window showing the git surface for the open project folder
/// (File › Open Folder…). Opened from the project file picker's Git… button
/// or the palette's "Project Folder: Git Changes…".
pub struct ProjectGitWindow {
    git: Entity<GitPanel>,
}

impl ProjectGitWindow {
    pub fn new(root: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let git = cx.new(|cx| GitPanel::new(root, "project-git", window, cx));
        cx.observe(&git, |_, _, cx| cx.notify()).detach();
        Self { git }
    }

    /// Open (or re-point) the Project Git window for `root`: an existing
    /// Project Git window is reused and brought to front rather than
    /// stacking copies.
    pub fn open(root: PathBuf, cx: &mut gpui::App) {
        for handle in cx.windows() {
            let Some(root_handle) = handle.downcast::<gpui_component::Root>() else {
                continue;
            };
            let Ok(view) = root_handle
                .read(cx)
                .map(|r| r.view().clone().downcast::<ProjectGitWindow>())
            else {
                continue;
            };
            let Ok(view) = view else {
                continue;
            };
            let root = root.clone();
            let _ = handle.update(cx, |_, window, cx| {
                // Re-pointing at another folder retitles the window too.
                window.set_window_title(&project_git_window_title(&root));
                view.update(cx, |this, cx| {
                    this.git.update(cx, |git, cx| git.set_root(root, cx));
                });
                window.activate_window();
            });
            return;
        }
        let title = SharedString::from(project_git_window_title(&root));
        let bounds = gpui::Bounds::centered(None, gpui::size(px(560.), px(640.)), cx);
        let options = gpui::WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some(title),
                ..Default::default()
            }),
            app_id: Some(crate::about::APP_ID.into()),
            ..Default::default()
        };
        let _ = cx.open_window(options, move |window, cx| {
            let view = cx.new(|cx| ProjectGitWindow::new(root, window, cx));
            cx.new(|cx| gpui_component::Root::new(view, window, cx))
        });
    }
}

/// Window title for the Project Git window: the folder's name, falling back
/// to the full path for a root like `/`.
pub fn project_git_window_title(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| root.display().to_string());
    localized_format(
        "explorer.git_panel.project.window_title",
        &[("name", &name)],
    )
}

impl Render for ProjectGitWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let root = self.git.read(cx).root().display().to_string();
        let mut chrome = div()
            .size_full()
            .flex()
            .flex_col()
            .bg(Theme::panel_bg())
            .text_color(Theme::text())
            .child(
                div()
                    .px(px(s(10.0)))
                    .py(px(s(8.0)))
                    .text_size(px(Theme::font_size_ratio(0.84)))
                    .text_color(Theme::text_dim())
                    .truncate()
                    .child(localized_format(
                        "explorer.git_panel.project.folder_label",
                        &[("root", &root)],
                    )),
            )
            .child(
                div()
                    .id("project-git-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .child(self.git.clone()),
            );
        if self.git.read(cx).diff_open() {
            let dialog = self
                .git
                .update(cx, |git, cx| git.render_diff_dialog(window, cx));
            chrome = chrome.child(dialog);
        }
        chrome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    fn read(is_repo: bool, paths: &[&str], error: Option<&str>) -> RefreshOutcome {
        RefreshOutcome::Read {
            is_repo,
            changes: paths
                .iter()
                .map(|p| crate::library_git::GitChange {
                    path: p.to_string(),
                    orig_path: None,
                    x: '?',
                    y: '?',
                })
                .collect(),
            error: error.map(sqlly_core::DisplayMessage::raw),
        }
    }

    /// A status read started before `set_root` must not land on the new
    /// folder, and a later successful read clears an earlier status error
    /// while an action error stays until the user moves on.
    #[gpui::test]
    fn stale_refreshes_are_dropped_and_status_errors_do_not_stick(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let (view, cx) =
            cx.add_window_view(|window, cx| GitPanel::new(first.clone(), "test-git", window, cx));
        cx.run_until_parked();
        view.update(cx, |panel, cx| {
            let old_generation = panel.refresh_generation;
            panel.set_root(second.clone(), cx);
            assert!(
                !panel.accept_refresh(old_generation, read(true, &["first/old.sql"], None)),
                "the old folder's read is dropped"
            );
            assert!(panel.git_changes.is_empty());

            let current = panel.refresh_generation;
            assert!(panel.accept_refresh(current, read(true, &[], Some("index locked"))));
            assert_eq!(panel.status_error.as_deref(), Some("index locked"));
            panel.git_error = Some("git commit failed: hook".to_string());
            assert!(panel.accept_refresh(current, read(true, &["a.sql"], None)));
            assert_eq!(panel.status_error, None, "a good read clears it");
            assert_eq!(
                panel.git_error.as_deref(),
                Some("git commit failed: hook"),
                "the action error is not wiped by the refresh that follows it"
            );
            assert_eq!(panel.git_changes.len(), 1);

            panel.apply_refresh(RefreshOutcome::GitUnavailable("Install git".into()));
            assert!(!panel.git_repo);
            assert!(panel.git_unavailable.is_some());
            assert!(panel.git_changes.is_empty());
        });
    }

    #[test]
    fn diff_preview_classifies_and_titles() {
        let preview = GitDiffPreview::from_text("a.sql", "@@ -1 +1 @@\n-old\n+new\n ctx\n");
        assert_eq!(preview.title, "Diff — a.sql");
        assert!(preview.note.is_none());
        let kinds: Vec<_> = preview.lines.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            vec![
                crate::library_git::DiffLineKind::Hunk,
                crate::library_git::DiffLineKind::Remove,
                crate::library_git::DiffLineKind::Add,
                crate::library_git::DiffLineKind::Context,
            ]
        );
    }

    #[test]
    fn empty_diff_becomes_an_honest_note() {
        let preview = GitDiffPreview::from_text("a.sql", "  \n");
        assert!(preview.lines.is_empty());
        assert!(preview.note.unwrap().contains("No differences"));
    }

    #[test]
    fn giant_diff_is_truncated_with_a_marker() {
        let text: String = (0..GIT_DIFF_LINE_CAP + 10)
            .map(|i| format!("+line {i}\n"))
            .collect();
        let preview = GitDiffPreview::from_text("big.sql", &text);
        assert_eq!(preview.lines.len(), GIT_DIFF_LINE_CAP + 1);
        assert!(preview.lines.last().unwrap().1.contains("truncated"));
    }

    #[test]
    fn element_ids_are_namespaced_per_host() {
        assert_eq!(
            element_id("library-git", "refresh").as_ref(),
            "library-git-refresh"
        );
        assert_ne!(
            element_id("library-git", "commit-all"),
            element_id("project-git", "commit-all")
        );
    }

    #[test]
    fn window_title_uses_the_folder_name() {
        assert_eq!(
            project_git_window_title(Path::new("/work/reports")),
            "Git — reports"
        );
        assert_eq!(project_git_window_title(Path::new("/")), "Git — /");
    }
}
