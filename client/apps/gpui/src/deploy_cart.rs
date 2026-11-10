//! Deploy Cart — named collections of objects gathered from any connection,
//! deployed as one dependency-ordered script per source database (into a
//! reviewed tab) or as per-object files through the dump pipeline, numbered in
//! that same dependency order.
//!
//! The cart state and its JSON document live in `sqlly_services::deploy_cart`;
//! this module is the window and the storage seam. The window opens detached
//! ([`crate::dialog::open_detached`]); its scripts and statuses reach the main
//! window as [`ToolDialogEvent`]s, so every generated script lands in a
//! reviewed tab — the cart itself never executes anything.
//!
//! Script structure always comes from the script-set pipeline (the cart's
//! order within dependency constraints); tables flagged "data" add their rows
//! from a data-only dump that is staged in a private temp file, capped while
//! it streams, and deleted as soon as it is read. Every run can be cancelled.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::{Disableable as _, IndexPath, Sizable as _};
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::{
    DeployCart, DeployCartGroup, DeployCartsFile, DeployObjectKind, DumpLayout, DumpOptions,
};

use crate::app::StatusLevel;
use crate::connection::ConnectionStore;
use crate::dialog::{Dialog, DialogFrame};
use crate::fs_safe::{self, StoreLoad};
use crate::host_storage::{self, StorageArea};
use crate::theme::Theme;
use crate::tool_dialog::ToolDialogEvent;

/// File name of the cart collection inside the Data storage area.
pub const DEPLOY_CARTS_FILE: &str = "deploy-carts.json";

/// Scripts larger than this never open in a tab (mirrors the dump dialog's
/// open-in-tab cap). The data half of a script is cut off *while* it
/// streams once the whole script would pass this size.
const OPEN_IN_TAB_MAX_BYTES: u64 = 8 * 1024 * 1024;

impl EventEmitter<ToolDialogEvent> for DeployCartDialog {}

/// The open Deploy Cart window, if any: its view and OS window, so a second
/// "Open Deploy Cart" raises it and an explorer add refreshes it live.
#[derive(Default)]
struct OpenDeployCart(Option<(gpui::WeakEntity<DeployCartDialog>, gpui::AnyWindowHandle)>);

impl gpui::Global for OpenDeployCart {}

/// Open (or raise) the Deploy Cart window, routing its events into the app.
/// Must run outside any window update (callers `cx.defer`).
pub(crate) fn open_window(
    router: crate::service_router::ServiceRouter,
    app: gpui::WeakEntity<crate::app::AppView>,
    cx: &mut gpui::App,
) {
    if let Some((view, handle)) = cx.try_global::<OpenDeployCart>().and_then(|g| g.0.clone())
        && view.upgrade().is_some()
    {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
        return;
    }
    let slot: std::rc::Rc<std::cell::Cell<Option<gpui::AnyWindowHandle>>> = Default::default();
    let slot_in = slot.clone();
    let opened = crate::dialog::open_detached(
        crate::dialog::DetachedOptions::localized(
            "explorer.deploy_cart.title",
            Theme::scaled(700.0),
            Theme::scaled(560.0),
        ),
        None,
        cx,
        move |window, cx| {
            slot_in.set(Some(window.window_handle()));
            cx.new(|cx| DeployCartDialog::new(router, window, cx))
        },
    );
    let (Some(view), Some(handle)) = (opened, slot.get()) else {
        return;
    };
    cx.subscribe(&view, move |_, event: &ToolDialogEvent, cx| {
        app.update(cx, |app, cx| app.handle_deploy_cart_event(event, cx))
            .ok();
    })
    .detach();
    cx.set_global(OpenDeployCart(Some((view.downgrade(), handle))));
}

/// Append one object to the active cart, persist, and refresh an open cart
/// window. Returns the cart's name and what the add did. A cart file that
/// exists but cannot be read is reported and left untouched.
pub(crate) fn add_item_to_active_cart(
    item: sqlly_services::DeployCartItem,
    cx: &mut gpui::App,
) -> Result<(String, sqlly_services::AddOutcome), String> {
    let mut file = load_deploy_carts()?;
    let cart = file.active_mut();
    let outcome = cart.add(item);
    let name = cart.name.clone();
    save_deploy_carts(&file)?;
    if let Some(view) = cx
        .try_global::<OpenDeployCart>()
        .and_then(|g| g.0.as_ref())
        .and_then(|(view, _)| view.upgrade())
    {
        view.update(cx, |dialog, cx| dialog.reload(cx));
    }
    Ok((name, outcome))
}

fn deploy_carts_path() -> Result<PathBuf, String> {
    host_storage::artifact_path(StorageArea::Data, DEPLOY_CARTS_FILE)
        .ok_or_else(|| crate::localization::text("explorer.deploy_cart.no_storage_location"))
}

/// Loads the cart collection. A missing file is an empty collection; a file
/// that exists but does not parse is an error (never silently "empty", or the
/// next save would overwrite the user's carts).
pub(crate) fn load_deploy_carts() -> Result<DeployCartsFile, String> {
    load_deploy_carts_from(&deploy_carts_path()?)
}

fn load_deploy_carts_from(path: &Path) -> Result<DeployCartsFile, String> {
    match fs_safe::load_json_store::<DeployCartsFile>(path) {
        StoreLoad::Missing => Ok(DeployCartsFile::default()),
        StoreLoad::Loaded(file) => Ok(file),
        StoreLoad::Corrupt { error } => Err(crate::localization::message(error)),
    }
}

/// Persists the cart collection atomically. Refuses to overwrite an on-disk
/// file that exists but cannot be parsed.
pub(crate) fn save_deploy_carts(file: &DeployCartsFile) -> Result<(), String> {
    save_deploy_carts_to(&deploy_carts_path()?, file)
}

fn save_deploy_carts_to(path: &Path, file: &DeployCartsFile) -> Result<(), String> {
    if let StoreLoad::Corrupt { error } = fs_safe::load_json_store::<DeployCartsFile>(path) {
        return Err(crate::localization::message(error));
    }
    fs_safe::write_atomic(path, file.to_json().as_bytes()).map_err(|e| {
        crate::localization::format(
            "explorer.deploy_cart.save_failed",
            &[("error", &e.to_string())],
        )
    })
}

/// The Deploy Cart kind a schema-tree node maps to, if any.
pub(crate) fn deploy_object_kind(kind: &crate::schema_tree::NodeKind) -> Option<DeployObjectKind> {
    use crate::schema_tree::NodeKind as N;
    Some(match kind {
        N::Table { .. } => DeployObjectKind::Table,
        // Materialized views are not collectable: their DDL and DROP differ
        // from a plain view's, and the script pipeline only knows views.
        N::View { .. } => DeployObjectKind::View,
        N::Procedure { .. } => DeployObjectKind::Procedure,
        N::Function { .. } => DeployObjectKind::Function,
        _ => return None,
    })
}

/// "Open scripts on" picker labels: the per-group source first, then every
/// saved connection by name.
fn target_labels(profiles: &[sqlly_core::ConnectionProfile]) -> Vec<String> {
    std::iter::once(crate::localization::text(
        "explorer.deploy_cart.target_each_source",
    ))
    .chain(profiles.iter().map(|p| p.name.clone()))
    .collect()
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Why a group scripted from `source` must not open on a `target` of another
/// engine family, or `None` when the two speak the same SQL. The cart never
/// translates between dialects, so a T-SQL script is refused on PostgreSQL.
fn dialect_mismatch(source: DatabaseType, target: DatabaseType) -> Option<String> {
    (source.connection_kind() != target.connection_kind()).then(|| {
        crate::localization::format(
            "explorer.deploy_cart.dialect_mismatch",
            &[
                ("source", &source.display_name()),
                ("target", &target.display_name()),
            ],
        )
    })
}

/// One export folder name per group: `<connection>-<database>`, sanitized
/// to portable characters and made unique within the run (`-2`, `-3`, …) so
/// same-named databases on different connections — or names that sanitize
/// alike — never share a folder.
fn export_folder_names(groups: &[(&str, &str)]) -> Vec<String> {
    let mut used: Vec<String> = Vec::with_capacity(groups.len());
    for (server, database) in groups {
        let base = format!(
            "{}-{}",
            sanitize_filename(if server.is_empty() {
                "connection"
            } else {
                server
            }),
            sanitize_filename(if database.is_empty() {
                "default"
            } else {
                database
            })
        );
        let taken = |candidate: &str| used.iter().any(|u| u.eq_ignore_ascii_case(candidate));
        let name = if taken(&base) {
            (2..)
                .map(|n| format!("{base}-{n}"))
                .find(|candidate| !taken(candidate))
                .expect("an unused suffix always exists")
        } else {
            base
        };
        used.push(name);
    }
    used
}

/// Create and return a folder under `parent` that did not exist before:
/// `name`, else `name-2`, `name-3`, … `create_dir` is the atomic existence
/// check, so a previous export (or a concurrent one) is never written into.
fn claim_fresh_dir(parent: &Path, name: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(parent)?;
    for n in 1..10_000 {
        let candidate = if n == 1 {
            parent.join(name)
        } else {
            parent.join(format!("{name}-{n}"))
        };
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::other(crate::localization::format(
        "explorer.deploy_cart.no_free_folder",
        &[("name", name), ("parent", &parent.display().to_string())],
    )))
}

/// A resolved source group ready to generate from: its live profile (with
/// the group's database applied) and what to generate.
struct ResolvedGroup {
    profile: sqlly_core::ConnectionProfile,
    server_name: String,
    database: String,
    items_label: String,
    /// Per-object file export scope (structure, plus data when flagged).
    options: DumpOptions,
    /// The flagged tables' rows, appended after the generated structure.
    data_options: Option<DumpOptions>,
    script_objects: Vec<sqlly_services::ScriptSetObject>,
}

/// One line of the items list: a group header or an item row (by flat index).
enum DisplayRow {
    Header(String),
    Item(usize),
}

pub struct DeployCartDialog {
    file: DeployCartsFile,
    router: crate::service_router::ServiceRouter,
    cart_select: Entity<SelectState<Vec<String>>>,
    name_input: Entity<InputState>,
    /// "Open scripts on": index 0 is each group's own source connection, the
    /// rest index `target_profiles` + 1 — the "run elsewhere" half of the cart.
    target_select: Entity<SelectState<Vec<String>>>,
    target_profiles: Vec<sqlly_core::ConnectionProfile>,
    /// The name field must follow a cart switch that happened without a
    /// window at hand (a reload from an explorer add).
    name_dirty: bool,
    persist_error: Option<String>,
    /// In-flight generation/export description, else `None`.
    running: Option<String>,
    /// Group jobs still in flight for the current run; `running` clears when
    /// the last one lands.
    pending_jobs: usize,
    /// Bumped by every run and by Cancel; a job's result lands only while its
    /// run is still the current one.
    run_generation: u64,
    /// The current run's cancel flag, shared with its jobs (and through them
    /// with the dump pipeline).
    cancel: Arc<AtomicBool>,
    selects_dirty: bool,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl DeployCartDialog {
    pub fn new(
        router: crate::service_router::ServiceRouter,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let (mut file, load_error) = match load_deploy_carts() {
            Ok(file) => (file, None),
            // Show an empty cart but keep the error up: every save refuses
            // until the damaged file is dealt with.
            Err(error) => (DeployCartsFile::default(), Some(error)),
        };
        // Always show one cart: the first open of a fresh collection starts
        // with an empty Default cart (persisted on the first mutation).
        if file.carts.is_empty() {
            file.active_mut();
        }
        let names: Vec<String> = file.carts.iter().map(|c| c.name.clone()).collect();
        let active_index = Self::active_index_of(&file);
        let cart_select =
            cx.new(|cx| SelectState::new(names, Some(IndexPath::new(active_index)), window, cx));
        let name_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.deploy_cart.name_placeholder",
                ))
                .default_value(
                    file.carts
                        .get(active_index)
                        .map(|c| c.name.clone())
                        .unwrap_or_default(),
                )
        });
        let target_profiles = ConnectionStore::global().list();
        let target_select = cx.new(|cx| {
            SelectState::new(
                target_labels(&target_profiles),
                Some(IndexPath::new(0)),
                window,
                cx,
            )
        });
        let subscriptions = vec![
            cx.subscribe_in(
                &cart_select,
                window,
                |this: &mut Self, state, event: &SelectEvent<Vec<String>>, window, cx| {
                    let SelectEvent::Confirm(_) = event;
                    // By position, not by name: the position is the cart.
                    if let Some(index) = state.read(cx).selected_index(cx) {
                        this.switch_cart_at(index.row, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &target_select,
                window,
                |_, _, _: &SelectEvent<Vec<String>>, _, cx| {
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &name_input,
                window,
                |this: &mut Self, state, event: &InputEvent, window, cx| {
                    // Commit on Enter or leaving the field — not per keystroke.
                    if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        let name = state.read(cx).value().to_string();
                        this.rename_active(name, window, cx);
                    }
                },
            ),
        ];
        Self {
            file,
            router,
            cart_select,
            name_input,
            target_select,
            target_profiles,
            name_dirty: false,
            persist_error: load_error,
            running: None,
            pending_jobs: 0,
            run_generation: 0,
            cancel: Arc::new(AtomicBool::new(false)),
            selects_dirty: false,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(680.0), Theme::scaled(540.0)),
            _subscriptions: subscriptions,
        }
    }

    fn active_index_of(file: &DeployCartsFile) -> usize {
        file.active_cart
            .and_then(|id| file.carts.iter().position(|c| c.id == id))
            .unwrap_or(0)
    }

    fn active_index(&self) -> usize {
        Self::active_index_of(&self.file)
    }

    fn active_cart(&self) -> &DeployCart {
        let index = self.active_index();
        &self.file.carts[index]
    }

    /// Re-read the collection from disk (an explorer add changed it) and
    /// refresh the pickers on the next render.
    pub(crate) fn reload(&mut self, cx: &mut gpui::Context<Self>) {
        let mut file = match load_deploy_carts() {
            Ok(file) => file,
            Err(error) => {
                self.persist_error = Some(error);
                cx.notify();
                return;
            }
        };
        if file.carts.is_empty() {
            file.active_mut();
        }
        let active_changed = file.active_cart != self.file.active_cart;
        self.file = file;
        self.selects_dirty = true;
        self.name_dirty |= active_changed;
        cx.notify();
    }

    /// The chosen "Open scripts on" connection, or `None` for each group's
    /// own source connection.
    fn target_profile(&self, cx: &gpui::App) -> Option<sqlly_core::ConnectionProfile> {
        let index = self.target_select.read(cx).selected_index(cx)?.row;
        index
            .checked_sub(1)
            .and_then(|i| self.target_profiles.get(i))
            .cloned()
    }

    fn persist(&mut self, cx: &mut gpui::Context<Self>) {
        match save_deploy_carts(&self.file) {
            Ok(()) => self.persist_error = None,
            Err(error) => self.persist_error = Some(error),
        }
        cx.notify();
    }

    fn switch_cart_at(&mut self, index: usize, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if index >= self.file.carts.len() || index == self.active_index() {
            return;
        }
        self.file.active_cart = Some(self.file.carts[index].id);
        let name = self.file.carts[index].name.clone();
        self.name_input.update(cx, |state, cx| {
            state.set_value(name, window, cx);
        });
        self.selects_dirty = true;
        self.persist(cx);
    }

    fn rename_active(&mut self, name: String, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let index = self.active_index();
        let id = self.file.carts[index].id;
        let trimmed = name.trim();
        if trimmed.is_empty() || self.file.carts[index].name == trimmed {
            // Snap an emptied field back to the cart's real name.
            self.name_dirty |= trimmed.is_empty();
            cx.notify();
            return;
        }
        if self.file.name_taken(trimmed, Some(id)) {
            self.persist_error = Some(crate::localization::format(
                "explorer.deploy_cart.duplicate_name",
                &[("name", trimmed)],
            ));
            let current = self.file.carts[index].name.clone();
            self.name_input
                .update(cx, |state, cx| state.set_value(current, window, cx));
            cx.notify();
            return;
        }
        self.file.carts[index].name = trimmed.to_string();
        self.selects_dirty = true;
        self.persist(cx);
    }

    fn new_cart(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let name = self.file.unique_cart_name(&crate::localization::format(
            "explorer.deploy_cart.default_name",
            &[("count", &(self.file.carts.len() + 1).to_string())],
        ));
        let cart = DeployCart::new(name.clone());
        self.file.active_cart = Some(cart.id);
        self.file.carts.push(cart);
        self.name_input.update(cx, |state, cx| {
            state.set_value(name, window, cx);
        });
        self.selects_dirty = true;
        self.persist(cx);
    }

    /// Ask before a destructive cart change; `apply` runs only on "yes", and
    /// only if the same cart is still the active one.
    fn confirm_then(
        &mut self,
        message: String,
        detail: String,
        confirm_label: String,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
        apply: impl FnOnce(&mut Self, &mut Window, &mut gpui::Context<Self>) + 'static,
    ) {
        let cart_id = self.active_cart().id;
        let answer = window.prompt(
            gpui::PromptLevel::Warning,
            &message,
            Some(&detail),
            &[
                gpui::PromptButton::cancel(crate::localization::text(
                    "explorer.deploy_cart.confirm_cancel",
                )),
                gpui::PromptButton::new(confirm_label),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(1) {
                return;
            }
            let _ = this.update_in(cx, |this, window, cx| {
                if this.active_cart().id == cart_id {
                    apply(this, window, cx);
                }
            });
        })
        .detach();
    }

    fn request_delete_cart(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let cart = self.active_cart();
        let message = crate::localization::format(
            "explorer.deploy_cart.delete_message",
            &[("name", &cart.name)],
        );
        let detail = crate::localization::format(
            "explorer.deploy_cart.delete_detail",
            &[("count", &cart.items.len().to_string())],
        );
        self.confirm_then(
            message,
            detail,
            crate::localization::text("explorer.deploy_cart.delete_confirm"),
            window,
            cx,
            |this, window, cx| this.delete_active_cart(window, cx),
        );
    }

    fn request_clear_cart(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let cart = self.active_cart();
        let message = crate::localization::format(
            "explorer.deploy_cart.clear_message",
            &[("name", &cart.name)],
        );
        let detail = crate::localization::format(
            "explorer.deploy_cart.clear_detail",
            &[("count", &cart.items.len().to_string())],
        );
        self.confirm_then(
            message,
            detail,
            crate::localization::text("explorer.deploy_cart.clear_confirm"),
            window,
            cx,
            |this, _window, cx| this.mutate_active(cx, |cart| cart.clear()),
        );
    }

    fn delete_active_cart(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let index = self.active_index();
        self.file.carts.remove(index);
        self.file.active_cart = self.file.carts.first().map(|c| c.id);
        if self.file.carts.is_empty() {
            // Never leave the window without a cart to show.
            self.file.active_mut();
        }
        let active = self.active_index();
        let name = self.file.carts[active].name.clone();
        self.name_input.update(cx, |state, cx| {
            state.set_value(name, window, cx);
        });
        self.selects_dirty = true;
        self.persist(cx);
    }

    fn mutate_active(
        &mut self,
        cx: &mut gpui::Context<Self>,
        mutate: impl FnOnce(&mut DeployCart),
    ) {
        let index = self.active_index();
        mutate(&mut self.file.carts[index]);
        self.persist(cx);
    }

    /// The item list as display rows: a header per source group, then its
    /// items by flat cart index (groups follow first-appearance order).
    fn display_rows(&self) -> Vec<DisplayRow> {
        let cart = self.active_cart();
        let mut used = vec![false; cart.items.len()];
        let mut rows = Vec::new();
        for group in cart.groups() {
            rows.push(DisplayRow::Header(format!(
                "{} — {} ({})",
                group.server_name,
                group.database,
                group.items.len()
            )));
            for item in &group.items {
                let index = cart
                    .items
                    .iter()
                    .enumerate()
                    .find(|(i, candidate)| !used[*i] && *candidate == item)
                    .map(|(i, _)| i);
                if let Some(index) = index {
                    used[index] = true;
                    rows.push(DisplayRow::Item(index));
                }
            }
        }
        rows
    }

    /// Resolve each source group to its live profile and services route.
    /// Groups whose connection no longer exists come back as an error string.
    fn resolve_groups(&self) -> (Vec<(ResolvedGroup, sqlly_services::Services)>, Vec<String>) {
        let mut resolved = Vec::new();
        let mut failed = Vec::new();
        for group in self.active_cart().groups() {
            match self.resolve_group(&group) {
                Ok(pair) => resolved.push(pair),
                Err(reason) => failed.push(reason),
            }
        }
        (resolved, failed)
    }

    fn resolve_group(
        &self,
        group: &DeployCartGroup,
    ) -> Result<(ResolvedGroup, sqlly_services::Services), String> {
        let Some(mut profile) = ConnectionStore::global().get(group.server_id) else {
            return Err(crate::localization::format(
                "explorer.deploy_cart.connection_missing",
                &[
                    ("server", &group.server_name),
                    ("database", &group.database),
                ],
            ));
        };
        profile.database = Some(group.database.clone());
        let services = self.router.services_for(&profile);
        let server_name = if group.server_name.is_empty() {
            profile.name.clone()
        } else {
            group.server_name.clone()
        };
        Ok((
            ResolvedGroup {
                profile,
                server_name,
                database: group.database.clone(),
                items_label: format!("{} — {}", group.server_name, group.database),
                options: group.dump_options(DumpLayout::FilePerObject),
                data_options: group.data_only_dump_options(),
                script_objects: group.script_objects(),
            },
            services,
        ))
    }

    /// Start a new run: fresh cancel flag and generation, busy text shown.
    fn begin_run(&mut self, text: String, jobs: usize) -> (u64, Arc<AtomicBool>) {
        self.run_generation = self.run_generation.wrapping_add(1);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.running = Some(text);
        self.pending_jobs = jobs;
        (self.run_generation, self.cancel.clone())
    }

    /// Cancel: stop the dump pipeline at its next check, and drop whatever
    /// the run's jobs still deliver.
    fn cancel_run(&mut self, cx: &mut gpui::Context<Self>) {
        if self.running.is_none() {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        self.run_generation = self.run_generation.wrapping_add(1);
        self.running = None;
        self.pending_jobs = 0;
        cx.emit(ToolDialogEvent::Status {
            level: StatusLevel::Info,
            message: crate::localization::text("explorer.deploy_cart.run_cancelled"),
        });
        cx.notify();
    }

    /// Whether a job of run `generation` may still deliver; also counts the
    /// job off and clears the busy state after the last one.
    fn finish_job(&mut self, generation: u64) -> bool {
        if generation != self.run_generation {
            return false;
        }
        self.pending_jobs = self.pending_jobs.saturating_sub(1);
        if self.pending_jobs == 0 {
            self.running = None;
        }
        true
    }

    /// Groups whose source engine differs from the chosen "Open scripts on"
    /// target, with the reason — shown under the picker and skipped by
    /// Generate.
    fn dialect_conflicts(&self, cx: &gpui::App) -> Vec<String> {
        let Some(target) = self.target_profile(cx) else {
            return Vec::new();
        };
        self.active_cart()
            .groups()
            .iter()
            .filter_map(|group| {
                let source = ConnectionStore::global().get(group.server_id)?;
                dialect_mismatch(source.database_type, target.database_type)
                    .map(|reason| format!("{} — {}: {reason}", group.server_name, group.database))
            })
            .collect()
    }

    /// Generate one dependency-ordered script per source group into a reviewed
    /// tab. Structure renders from live definitions; groups with data-flagged
    /// tables append those rows from a capped, cancellable data-only dump.
    fn generate_script(&mut self, cx: &mut gpui::Context<Self>) {
        if self.running.is_some() {
            return;
        }
        let (groups, mut failed) = self.resolve_groups();
        let target = self.target_profile(cx);
        let groups: Vec<_> = groups
            .into_iter()
            .filter(|(group, _)| {
                let Some(target) = &target else {
                    return true;
                };
                match dialect_mismatch(group.profile.database_type, target.database_type) {
                    Some(reason) => {
                        failed.push(format!("{}: {reason}", group.items_label));
                        false
                    }
                    None => true,
                }
            })
            .collect();
        for failure in &failed {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Warning,
                message: crate::localization::format(
                    "explorer.deploy_cart.skipped",
                    &[("reason", failure)],
                ),
            });
        }
        if groups.is_empty() {
            self.persist_error = failed.first().cloned();
            cx.notify();
            return;
        }
        let (generation, cancel) = self.begin_run(
            crate::localization::format(
                "explorer.deploy_cart.generating_scripts",
                &[("count", &groups.len().to_string())],
            ),
            groups.len(),
        );
        cx.notify();
        for (group, services) in groups {
            self.generate_for_group(
                group,
                services,
                target.clone(),
                generation,
                cancel.clone(),
                cx,
            );
        }
    }

    fn generate_for_group(
        &mut self,
        group: ResolvedGroup,
        services: sqlly_services::Services,
        target: Option<sqlly_core::ConnectionProfile>,
        generation: u64,
        cancel: Arc<AtomicBool>,
        cx: &mut gpui::Context<Self>,
    ) {
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        let label = group.items_label.clone();
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, String>>();
        let profile = target.unwrap_or_else(|| group.profile.clone());
        // The dump pipeline's progress callback is not `Send`, so the job runs
        // on its own thread inside the shared runtime rather than as a task.
        std::thread::spawn(move || {
            crate::tokio_handle().block_on(async move {
                let _ = tx.send(generate_group_sql(&services, &group, &cancel).await);
            });
        });

        cx.spawn(async move |_this, _cx| {
            let Ok(result) = rx.await else { return };
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                if !this.finish_job(generation) {
                    // Cancelled or superseded: drop the late result.
                    return;
                }
                match result {
                    Ok(sql) => {
                        let profile_name = profile.name.clone();
                        cx.emit(ToolDialogEvent::ScriptToEditorForProfile {
                            sql,
                            profile: Box::new(profile),
                        });
                        cx.emit(ToolDialogEvent::Status {
                            level: StatusLevel::Success,
                            message: crate::localization::format(
                                "explorer.deploy_cart.script_opened",
                                &[("group", &label), ("profile", &profile_name)],
                            ),
                        });
                    }
                    Err(error) => {
                        cx.emit(ToolDialogEvent::Status {
                            level: StatusLevel::Error,
                            message: crate::localization::format(
                                "explorer.deploy_cart.script_failed",
                                &[("group", &label), ("error", &error)],
                            ),
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Export per-object files: one dump-pipeline run per source group, each
    /// into its own fresh `<cart>/<connection>-<database>` folder under the
    /// chosen directory.
    fn export_files(&mut self, cx: &mut gpui::Context<Self>) {
        if self.running.is_some() {
            return;
        }
        let (groups, failed) = self.resolve_groups();
        if groups.is_empty() {
            self.persist_error = failed.first().cloned();
            cx.notify();
            return;
        }
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        let cart_name = sanitize_filename(&self.active_cart().name);
        cx.spawn(async move |_this, _cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(base) = paths.into_iter().next() else {
                return;
            };
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                if this.running.is_some() {
                    return;
                }
                for failure in &failed {
                    cx.emit(ToolDialogEvent::Status {
                        level: StatusLevel::Warning,
                        message: crate::localization::format(
                            "explorer.deploy_cart.skipped",
                            &[("reason", failure)],
                        ),
                    });
                }
                let (generation, cancel) = this.begin_run(
                    crate::localization::text("explorer.deploy_cart.exporting_files"),
                    groups.len(),
                );
                cx.notify();
                let keys: Vec<(&str, &str)> = groups
                    .iter()
                    .map(|(g, _)| (g.server_name.as_str(), g.database.as_str()))
                    .collect();
                let folders = export_folder_names(&keys);
                let parent = base.join(if cart_name.is_empty() {
                    "cart".to_string()
                } else {
                    cart_name.clone()
                });
                for ((group, services), folder) in groups.into_iter().zip(folders) {
                    this.export_group_files(
                        group,
                        services,
                        parent.clone(),
                        folder,
                        generation,
                        cancel.clone(),
                        cx,
                    );
                }
            })
            .ok();
        })
        .detach();
    }

    #[allow(clippy::too_many_arguments)]
    fn export_group_files(
        &mut self,
        group: ResolvedGroup,
        services: sqlly_services::Services,
        parent: PathBuf,
        folder: String,
        generation: u64,
        cancel: Arc<AtomicBool>,
        cx: &mut gpui::Context<Self>,
    ) {
        let label = group.items_label.clone();
        let mut opts = group.options.clone();
        let profile = group.profile.clone();
        let database = group.database.clone();
        let script_objects = group.script_objects.clone();
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        let (tx, rx) = tokio::sync::oneshot::channel::<
            Result<DeferredExportSummary, sqlly_core::DisplayMessage>,
        >();
        // The dump pipeline's progress callback is not `Send`, so the job runs
        // on its own thread inside the shared runtime rather than as a task.
        std::thread::spawn(move || {
            crate::tokio_handle().block_on(async move {
                let result = async {
                    // Number the per-object files in the same dependency order
                    // the single script uses (tables by foreign key, views and
                    // routines by what their definitions reference, cart order
                    // otherwise) instead of the dump's alphabetical views and
                    // routines.
                    let (ordered, order_warnings) = sqlly_services::order_for_create_live(
                        &services,
                        &profile,
                        &database,
                        &script_objects,
                    )
                    .await
                    .map_err(|e| e.message())?;
                    // This thread cannot see the UI language (translation
                    // state is per thread): text stays keyed until the
                    // result is back on the UI thread.
                    let mut warnings = order_warnings;
                    opts.structure_order = Some(ordered.iter().map(|o| o.label()).collect());
                    let output = claim_fresh_dir(&parent, &folder).map_err(|e| {
                        crate::localization::deferred(
                            "explorer.deploy_cart.create_folder_failed",
                            &[("error", &e.to_string())],
                        )
                    })?;
                    let report = sqlly_services::run_database_dump(
                        &services,
                        &profile,
                        &database,
                        &output,
                        &opts,
                        cancel,
                        &mut |_| {},
                    )
                    .await
                    .map_err(|e| e.message())?;
                    if report.cancelled {
                        return Err(crate::localization::deferred(
                            "explorer.deploy_cart.export_cancelled_partial",
                            &[("path", &output.display().to_string())],
                        ));
                    }
                    warnings.extend(report.warnings);
                    Ok(DeferredExportSummary {
                        text: crate::localization::deferred(
                            "explorer.deploy_cart.export_file_summary",
                            &[
                                ("count", &report.files.len().to_string()),
                                ("path", &output.display().to_string()),
                            ],
                        ),
                        warnings,
                    })
                }
                .await;
                let _ = tx.send(result);
            });
        });
        cx.spawn(async move |_this, _cx| {
            let Ok(result) = rx.await else { return };
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                if !this.finish_job(generation) {
                    return;
                }
                let result = result
                    .map(DeferredExportSummary::resolve)
                    .map_err(crate::localization::message);
                let (level, message) = match result {
                    // Skipped definitions and ordering caveats must not
                    // vanish behind a success line.
                    Ok(summary) if !summary.warnings.is_empty() => (
                        StatusLevel::Warning,
                        crate::localization::format(
                            "explorer.deploy_cart.export_succeeded_warnings",
                            &[
                                ("group", &label),
                                ("summary", &summary.text),
                                ("warnings", &export_warnings_suffix(&summary.warnings)),
                            ],
                        ),
                    ),
                    Ok(summary) => (
                        StatusLevel::Success,
                        crate::localization::format(
                            "explorer.deploy_cart.export_succeeded",
                            &[("group", &label), ("summary", &summary.text)],
                        ),
                    ),
                    Err(error) => (
                        StatusLevel::Error,
                        crate::localization::format(
                            "explorer.deploy_cart.export_failed",
                            &[("group", &label), ("error", &error)],
                        ),
                    ),
                };
                cx.emit(ToolDialogEvent::Status { level, message });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The cart always lives in its own OS window: closing removes it (and
    /// cancels any run in flight). Close is deferred past this update so
    /// nothing emitted in it is dropped.
    fn close(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        window.defer(cx, |window, _| window.remove_window());
    }
}

/// What a finished per-group file export reports: the summary line and any
/// non-fatal warnings (unreadable definitions, ordering caveats).
struct ExportSummary {
    text: String,
    warnings: Vec<String>,
}

/// [`ExportSummary`] as the export thread builds it: keyed messages that the
/// UI thread resolves in its language.
struct DeferredExportSummary {
    text: sqlly_core::DisplayMessage,
    warnings: Vec<sqlly_core::DisplayMessage>,
}

impl DeferredExportSummary {
    fn resolve(self) -> ExportSummary {
        ExportSummary {
            text: crate::localization::message(self.text),
            warnings: self
                .warnings
                .into_iter()
                .map(crate::localization::message)
                .collect(),
        }
    }
}

/// The status-line tail for an export's warnings: the one warning itself, or
/// the count and the first.
fn export_warnings_suffix(warnings: &[String]) -> String {
    match warnings {
        [] => String::new(),
        [one] => crate::localization::format(
            "explorer.deploy_cart.warning_one_suffix",
            &[("warning", one)],
        ),
        many => crate::localization::format(
            "explorer.deploy_cart.warning_many_suffix",
            &[("count", &many.len().to_string()), ("warning", &many[0])],
        ),
    }
}

/// One group's script: the structure from live definitions (cart order within
/// dependency constraints), then — when tables are flagged — their rows.
async fn generate_group_sql(
    services: &sqlly_services::Services,
    group: &ResolvedGroup,
    cancel: &Arc<AtomicBool>,
) -> Result<String, String> {
    let report = sqlly_services::render_create_script(
        services,
        &group.profile,
        &group.database,
        &group.script_objects,
        false,
    )
    .await
    .map_err(|e| crate::localization::message(e.message()))?;
    if cancel.load(Ordering::Relaxed) {
        return Err(crate::localization::text("explorer.deploy_cart.cancelled"));
    }
    let mut sql = report.sql;
    if let Some(opts) = &group.data_options {
        let budget = OPEN_IN_TAB_MAX_BYTES.saturating_sub(sql.len() as u64);
        let data = dump_data_capped(services, group, opts, cancel, budget).await?;
        sql.push_str("\n-- Table data\n");
        sql.push_str(&data);
    }
    Ok(sql)
}

/// Deletes the staged data file however the job ends.
struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Run the flagged tables' data-only dump into a private temp file (0600 in
/// a 0700 directory on Unix, exclusively created) and read it back. A
/// watcher stops the dump as soon as the file passes `cap` bytes or the user
/// cancels; the file is deleted when this returns, whatever the outcome.
async fn dump_data_capped(
    services: &sqlly_services::Services,
    group: &ResolvedGroup,
    opts: &DumpOptions,
    cancel: &Arc<AtomicBool>,
    cap: u64,
) -> Result<String, String> {
    let (file, path) =
        fs_safe::private_temp_file("deploy-cart", "deploy-data", ".sql").map_err(|e| {
            crate::localization::format(
                "explorer.deploy_cart.temp_file_failed",
                &[("error", &e.to_string())],
            )
        })?;
    drop(file);
    let _cleanup = TempFileGuard(path.clone());

    let dump_cancel = Arc::new(AtomicBool::new(false));
    let over_cap = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let watcher = {
        let (path, cancel, dump_cancel, over_cap, done) = (
            path.clone(),
            cancel.clone(),
            dump_cancel.clone(),
            over_cap.clone(),
            done.clone(),
        );
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                if cancel.load(Ordering::Relaxed) {
                    dump_cancel.store(true, Ordering::Relaxed);
                    return;
                }
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if size > cap {
                    over_cap.store(true, Ordering::Relaxed);
                    dump_cancel.store(true, Ordering::Relaxed);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        })
    };
    let result = sqlly_services::run_database_dump(
        services,
        &group.profile,
        &group.database,
        &path,
        opts,
        dump_cancel,
        &mut |_| {},
    )
    .await;
    done.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    let report = result.map_err(|e| crate::localization::message(e.message()))?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if over_cap.load(Ordering::Relaxed) || size > cap {
        return Err(crate::localization::format(
            "explorer.deploy_cart.review_limit_exceeded",
            &[(
                "limit",
                &(OPEN_IN_TAB_MAX_BYTES / (1024 * 1024)).to_string(),
            )],
        ));
    }
    if report.cancelled || cancel.load(Ordering::Relaxed) {
        return Err(crate::localization::text("explorer.deploy_cart.cancelled"));
    }
    std::fs::read_to_string(&path).map_err(|e| e.to_string())
}

impl gpui::Focusable for DeployCartDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DeployCartDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        if self.selects_dirty {
            self.selects_dirty = false;
            let names: Vec<String> = self.file.carts.iter().map(|c| c.name.clone()).collect();
            let active = self.active_index();
            self.cart_select.update(cx, |state, cx| {
                state.set_items(names, window, cx);
                state.set_selected_index(Some(IndexPath::new(active)), window, cx);
            });
        }
        if self.name_dirty {
            self.name_dirty = false;
            let name = self.active_cart().name.clone();
            self.name_input
                .update(cx, |state, cx| state.set_value(name, window, cx));
        }

        let running = self.running.clone();
        let conflicts = self.dialect_conflicts(cx);
        let cart_empty = self.active_cart().items.is_empty();
        let rows = self.display_rows();
        let item_count = self.active_cart().items.len();

        // Toolbar: cart picker, new/delete, name.
        let toolbar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(8.0)))
            .px(px(s(14.0)))
            .py(px(s(10.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(div().w(px(s(200.0))).child(Select::new(&self.cart_select)))
            .child(
                Button::new("dc-new-cart")
                    .label(crate::localization::text("explorer.deploy_cart.new_button"))
                    .on_click(cx.listener(|this, _, window, cx| this.new_cart(window, cx))),
            )
            .child(
                Button::new("dc-delete-cart")
                    .label(crate::localization::text(
                        "explorer.deploy_cart.delete_button",
                    ))
                    .disabled(self.running.is_some())
                    .on_click(
                        cx.listener(|this, _, window, cx| this.request_delete_cart(window, cx)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .child(Input::new(&self.name_input)),
            );

        // Items list, grouped by source connection.
        let mut list = div()
            .id("deploy-cart-items")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .px(px(s(14.0)))
            .py(px(s(8.0)))
            .gap(px(s(2.0)));
        if rows.iter().all(|r| matches!(r, DisplayRow::Header(_)))
            || self.active_cart().items.is_empty()
        {
            list = list.child(
                div()
                    .py(px(s(24.0)))
                    .text_align(gpui::TextAlign::Center)
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("explorer.deploy_cart.empty")),
            );
        }
        for row in rows {
            match row {
                DisplayRow::Header(text) => {
                    list = list.child(
                        div()
                            .pt(px(s(8.0)))
                            .pb(px(s(2.0)))
                            .text_size(px(Theme::font_size_ratio(0.67)))
                            .text_color(Theme::text_dim())
                            .child(text.to_uppercase()),
                    );
                }
                DisplayRow::Item(index) => {
                    let item = self.active_cart().items[index].clone();
                    let last = item_count.saturating_sub(1);
                    let up = Button::new(gpui::SharedString::from(format!("dc-up-{index}")))
                        .label("↑")
                        .xsmall()
                        .ghost()
                        .disabled(index == 0)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.mutate_active(cx, |cart| {
                                cart.move_item(index, index.saturating_sub(1));
                            });
                        }));
                    let down = Button::new(gpui::SharedString::from(format!("dc-down-{index}")))
                        .label("↓")
                        .xsmall()
                        .ghost()
                        .disabled(index >= last)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.mutate_active(cx, |cart| {
                                cart.move_item(index, (index + 1).min(last));
                            });
                        }));
                    let remove = Button::new(gpui::SharedString::from(format!("dc-rm-{index}")))
                        .label("✕")
                        .xsmall()
                        .ghost()
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.mutate_active(cx, |cart| {
                                cart.remove_at(index);
                            });
                        }));
                    let mut row = div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(6.0)))
                        .child(up)
                        .child(down)
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .text_size(px(Theme::font_size_ratio(0.87)))
                                .text_color(Theme::text())
                                .child(item.label()),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .text_size(px(Theme::font_size_ratio(0.74)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::message(item.kind.message())),
                        );
                    if item.kind == DeployObjectKind::Table {
                        row = row.child(
                            Checkbox::new(gpui::SharedString::from(format!("dc-data-{index}")))
                                .label(crate::localization::text(
                                    "explorer.deploy_cart.include_data",
                                ))
                                .checked(item.include_data)
                                .on_click(cx.listener(move |this, checked: &bool, _window, cx| {
                                    let checked = *checked;
                                    this.mutate_active(cx, |cart| {
                                        if let Some(item) = cart.items.get_mut(index) {
                                            item.include_data = checked;
                                        }
                                    });
                                })),
                        );
                    }
                    list = list.child(row.child(remove));
                }
            }
        }

        let body = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .child(toolbar)
            .child(list)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .px(px(s(14.0)))
                    .py(px(s(8.0)))
                    .border_t_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.82)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text(
                                "explorer.deploy_cart.open_scripts_on",
                            )),
                    )
                    .child(
                        div()
                            .w(px(s(260.0)))
                            .child(Select::new(&self.target_select)),
                    ),
            )
            .children(conflicts.into_iter().map(|conflict| {
                div()
                    .px(px(s(14.0)))
                    .pb(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::warning())
                    .child(crate::localization::format(
                        "explorer.deploy_cart.will_skip",
                        &[("reason", &conflict)],
                    ))
            }))
            .children(self.persist_error.clone().map(|error| {
                div()
                    .px(px(s(14.0)))
                    .pb(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::error())
                    .child(error)
            }))
            .children(running.map(|text| {
                div()
                    .px(px(s(14.0)))
                    .pb(px(s(4.0)))
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .child(text)
            }));

        let busy = self.running.is_some();
        let mut dialog = Dialog::new("deploy-cart-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("explorer.deploy_cart.title"))
            .windowed(true)
            .easy_close(true)
            .on_close(|this, window, cx| this.close(window, cx))
            .leading_button(
                Button::new("dc-clear")
                    .label(crate::localization::text(
                        "explorer.deploy_cart.clear_button",
                    ))
                    .disabled(busy || cart_empty)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.request_clear_cart(window, cx)),
                    ),
            );
        if busy {
            dialog = dialog.button(
                Button::new("dc-cancel")
                    .label(crate::localization::text(
                        "explorer.deploy_cart.cancel_run_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| this.cancel_run(cx))),
            );
        }
        dialog
            .button(
                Button::new("dc-files")
                    .label(crate::localization::text(
                        "explorer.deploy_cart.files_button",
                    ))
                    .disabled(busy || cart_empty)
                    .on_click(cx.listener(|this, _, _window, cx| this.export_files(cx))),
            )
            .button(
                Button::new("dc-generate")
                    .primary()
                    .label(crate::localization::text(
                        "explorer.deploy_cart.generate_script_button",
                    ))
                    .disabled(busy || cart_empty)
                    .on_click(cx.listener(|this, _, _window, cx| this.generate_script(cx))),
            )
            .child(body)
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_tree::NodeKind;

    #[test]
    fn export_warnings_are_summarised_not_dropped() {
        assert_eq!(export_warnings_suffix(&[]), "");
        assert_eq!(
            export_warnings_suffix(&["dbo.v: definition unavailable".into()]),
            " (1 warning: dbo.v: definition unavailable)"
        );
        let many = export_warnings_suffix(&["a".into(), "b".into(), "c".into()]);
        assert!(many.starts_with(" (3 warnings"), "{many}");
        assert!(many.contains("a"), "{many}");
    }

    #[test]
    fn filenames_are_sanitized_to_portable_characters() {
        assert_eq!(sanitize_filename("Staging cart/2"), "Staging_cart_2");
        assert_eq!(sanitize_filename("db.v1-a_b"), "db.v1-a_b");
        assert_eq!(sanitize_filename("naïve"), "na_ve");
    }

    #[test]
    fn node_kinds_map_to_cart_kinds() {
        let t = |schema: &str, name: &str| (schema.to_string(), name.to_string());
        let (s, n) = t("dbo", "orders");
        assert_eq!(
            deploy_object_kind(&NodeKind::Table {
                schema: s.clone(),
                name: n.clone()
            }),
            Some(DeployObjectKind::Table)
        );
        assert_eq!(
            deploy_object_kind(&NodeKind::MaterializedView {
                schema: s.clone(),
                name: n.clone()
            }),
            None,
            "materialized views are not collectable"
        );
        assert_eq!(
            deploy_object_kind(&NodeKind::Procedure {
                schema: s.clone(),
                name: n.clone()
            }),
            Some(DeployObjectKind::Procedure)
        );
        assert_eq!(
            deploy_object_kind(&NodeKind::Function { schema: s, name: n }),
            Some(DeployObjectKind::Function)
        );
        assert_eq!(deploy_object_kind(&NodeKind::TablesFolder), None);
    }

    #[test]
    fn target_labels_lead_with_the_source_option() {
        let labels = target_labels(&[]);
        assert_eq!(labels.len(), 1);
        assert!(labels[0].contains("source"));
    }

    #[test]
    fn scripts_are_refused_on_a_target_of_another_engine_family() {
        assert!(dialect_mismatch(DatabaseType::SqlServer, DatabaseType::Postgres).is_some());
        assert!(dialect_mismatch(DatabaseType::Postgres, DatabaseType::MySql).is_some());
        // Same wire family: same SQL.
        assert!(
            dialect_mismatch(DatabaseType::SqlServer, DatabaseType::AzureSqlDatabase).is_none()
        );
        assert!(dialect_mismatch(DatabaseType::Postgres, DatabaseType::Supabase).is_none());
        let reason = dialect_mismatch(DatabaseType::SqlServer, DatabaseType::Postgres).unwrap();
        assert!(reason.contains("does not translate"), "{reason}");
    }

    #[test]
    fn export_folders_carry_the_connection_and_never_collide() {
        let names = export_folder_names(&[
            ("prod", "shop"),
            ("staging", "shop"),
            ("prod/eu", "shop"),
            ("prod_eu", "shop"),
            ("", ""),
        ]);
        assert_eq!(
            names,
            vec![
                "prod-shop",
                "staging-shop",
                "prod_eu-shop",
                "prod_eu-shop-2",
                "connection-default"
            ]
        );
    }

    #[test]
    fn each_export_claims_a_fresh_folder() {
        let dir = tempfile::tempdir().unwrap();
        let first = claim_fresh_dir(dir.path(), "prod-shop").unwrap();
        std::fs::write(first.join("manifest.sql"), "old").unwrap();
        let second = claim_fresh_dir(dir.path(), "prod-shop").unwrap();
        assert_ne!(first, second);
        assert_eq!(second.file_name().unwrap(), "prod-shop-2");
        assert!(std::fs::read_dir(&second).unwrap().next().is_none());
        assert_eq!(
            std::fs::read_to_string(first.join("manifest.sql")).unwrap(),
            "old",
            "an earlier export is never written into"
        );
    }

    #[test]
    fn corrupt_cart_store_is_reported_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEPLOY_CARTS_FILE);
        // Missing → empty collection; saving works and round-trips.
        let mut file = load_deploy_carts_from(&path).expect("missing is fine");
        assert!(file.carts.is_empty());
        file.active_mut();
        save_deploy_carts_to(&path, &file).unwrap();
        assert_eq!(load_deploy_carts_from(&path).unwrap(), file);
        // Corrupt → an error on load, and a save refuses to clobber it.
        std::fs::write(&path, "{ not json").unwrap();
        assert!(load_deploy_carts_from(&path).is_err());
        assert!(save_deploy_carts_to(&path, &DeployCartsFile::default()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }
}
